//! Resume-time session enumeration + load + chain validation + interactive picker.
//!
//! 1:1 port of `claude-code/src/commands/resume/` + `sessionStorage.ts::loadSameRepoMessageLogs`,
//! with the Ink TUI replaced by a stdio line-based picker (OQ-6 fallback).
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-08-resume.md` Task 0 for byte-locks.
//!
//! TRANSCRIPT TOLERANCE: [`load_session`] no longer enforces a strict
//! file-order linear chain. It routes the file tolerantly
//! ([`crate::jsonl::reader::JsonlReader::read_routed`]) and reconstructs the
//! main thread with a branch-aware DAG walk ([`build_conversation_chain`]),
//! faithful to `claude-code`'s `loadMessagesFromJsonlPath`
//! (`conversationRecovery.ts:416`). This lets the loader ingest a REAL
//! `claude-code` transcript (leading `summary`, interleaved
//! `attachment`/`system`, forked roots, sidechain branches).
//!
//! PARALLEL-TOOL-CALL RECOVERY — `recoverOrphanedParallelToolResults`
//! (`sessionStorage.ts:2096`): the post-walk DAG recovery pass that re-attaches
//! sibling assistant blocks + orphaned `tool_result`s produced by PARALLEL tool
//! calls (N `tool_use`s → N one-block assistant messages sharing `message.id`)
//! now runs as an additive post-pass
//! ([`recover_parallel_results`]) at the tail of
//! [`build_conversation_chain`]. The single-parent walk still keeps one branch;
//! the post-pass merges recovered siblings, tool results, and unambiguous
//! metadata tails in transcript insertion order, preserving the main chain.
//! A stale checkpoint on another result in the same parallel batch follows
//! the durable continuation, including when the process exited before updating
//! `last-prompt` (Claude Code 2.1.286).

use crate::jsonl::aliases::{normalized_directory_path, read_session_aliases};
use crate::jsonl::path::{project_dir_name, sanitized_cwd, MAX_SANITIZED_LENGTH};
use crate::jsonl::re_append::find_last_typed_field;
use crate::jsonl::reader::{
    extract_json_string_field, lines_naming_types, route_line_seq, route_lines,
    strip_transcript_bom, JsonlReader, LoadedTranscript,
};
use crate::jsonl::recovery::{
    append_metadata_descendants, follows_checkpoint, ordered_messages, recover_parallel_results,
};
use crate::jsonl::schema::{JsonlMessage, SESSION_KIND_KEY};
use crate::jsonl::title::{
    extract_title, has_autonomous_tick_prompt, truncate_title, EMPTY_TITLE_FALLBACK,
};
use crate::jsonl::SessionMode;
use lingxi_core::host::{FileSystem, FileSystemCacheIdentity};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

/// Resume catalog scope, matching the native picker's current/repository/all
/// choices. Direct `/add-dir` links are included in the first two scopes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SessionDiscoveryScope {
    /// The current directory's bucket and its direct session aliases.
    #[default]
    Current,
    /// The current directory, its repository worktrees and direct aliases.
    Repository,
    /// Every project bucket, without recursively following session aliases.
    All,
}

/// Native resolveJobTranscript (Iue) inputs. Ordinary CLI resume and picker
/// row loading deliberately use separate entry points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTranscriptResolveOptions {
    pub link_scan_path: Option<PathBuf>,
    pub cross_worktree: bool,
}

impl Default for JobTranscriptResolveOptions {
    fn default() -> Self {
        Self {
            link_scan_path: None,
            cross_worktree: true,
        }
    }
}

/// Native Iue's observable result. A false probe retains its first scoped
/// candidate path; it does not select an arbitrary unrelated global copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedJobTranscriptPath {
    pub path: PathBuf,
    pub has_messages: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessagePresence {
    Has,
    None,
    Unknown,
}

/// Metadata for one resumable session row (uuid + title + activity time + visible-message count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    /// The session UUID parsed from the filename stem.
    pub uuid: Uuid,
    /// The session title shown in the resume picker. Resolved with claude-code's
    /// precedence (custom-title > ai-title > summary > first-user-message; see
    /// [`collect_dir`]) and normalized to [`crate::jsonl::title::TITLE_MAX_CHARS`]
    /// chars + ellipsis; the display surfaces re-truncate by terminal width.
    pub title: String,
    /// Resume-catalog last-activity time (UTC `SystemTime`).
    ///
    /// Claude clamps the last user/assistant transcript timestamp to the file's
    /// mtime (`Math.min(lastMessageAtMs, modified.getTime())`), so touching a
    /// transcript forward does not reorder it while an older mtime still wins.
    pub modified: SystemTime,
    /// Transcript file creation/birth time.
    ///
    /// Used as the equal-`modified` tie-break, matching claude-code
    /// `sortLogs`/`loadSameRepoMessageLogs`.
    pub created: SystemTime,
    /// Number of user/assistant messages visible in the restored conversation.
    pub message_count: usize,
    /// Capability profile this session was created or forked under.
    pub mode: SessionMode,
    /// Absolute path to the `.jsonl` file (kept so callers can re-load without re-resolving).
    pub path: PathBuf,
    /// Original workspace directory recovered from the relocation marker or a
    /// first message whose cwd agrees with this transcript's storage bucket.
    /// Hosts can use this launch hint without guessing from a sanitized name.
    pub workspace_path: Option<String>,
    /// Pull-request number linked to this session, when a `pr-link` metadata
    /// entry exists. This powers Claude-compatible `--from-pr` filtering.
    pub pr_number: Option<u64>,
    /// The title `searchSessionsByCustomTitle` (`OEe`) matches against:
    /// `customTitle ?? aiTitle`, stored RAW.
    ///
    /// Deliberately NOT [`Self::title`]. That one folds in `agentName`,
    /// `summary`, and the first user message for display, and it is truncated
    /// to [`crate::jsonl::title::TITLE_MAX_CHARS`] — searching it would resume
    /// a session whose *summary* happened to contain the query, which the
    /// oracle never does, and would miss a match past the truncation point.
    /// `None` when the session has neither title kind, which the oracle skips
    /// outright (`if (!p) return !1`).
    pub custom_or_ai_title: Option<String>,
    /// Last real assistant model recovered from the transcript. This stays
    /// private to launch/catalog consumers and is not part of the public
    /// session listing protocol.
    pub resume_model: Option<String>,
    /// Provider profile persisted beside [`Self::resume_model`].
    pub resume_model_profile: Option<String>,
}

/// A resumable-session catalog plus the number of UUID-named transcript files
/// that could not be read or contained no recoverable messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCatalog {
    /// Readable, resumable sessions sorted and limited like
    /// [`list_recent_sessions`].
    pub sessions: Vec<SessionMetadata>,
    /// Corrupt or unreadable candidate files skipped while preserving the
    /// readable portion of the catalog.
    pub skipped_files: usize,
}

impl Ord for SessionMetadata {
    fn cmp(&self, other: &Self) -> Ordering {
        // Newest-first by transcript activity, then creation time desc.
        other
            .modified
            .cmp(&self.modified)
            .then_with(|| other.created.cmp(&self.created))
    }
}

impl PartialOrd for SessionMetadata {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// All errors raised by the resume layer.
#[derive(Debug, Error)]
pub enum LoaderError {
    /// The given session id / arg did not resolve to a `.jsonl` file under the cwd's project dir.
    #[error("Session {arg} was not found.")]
    SessionNotFound {
        /// The arg (UUID string) that was looked up.
        arg: String,
    },
    /// A UUID exists in several unrelated project buckets and has no local or
    /// repository-scoped winner. Global UUID lookup must not choose arbitrarily.
    #[error("Session {arg} exists in multiple project directories.")]
    AmbiguousSession {
        /// The UUID requested by the caller.
        arg: String,
    },
    /// The `parentUuid` chain is broken at the named message.
    #[error("Session {arg} corrupted: parentUuid chain broken at message {at_uuid}.")]
    ChainBroken {
        /// The session arg.
        arg: String,
        /// UUID of the offending message.
        at_uuid: Uuid,
    },
    /// Two or more messages in the file claim different `sessionId` values.
    #[error("Session {arg} corrupted: sessionId mismatch (expected {expected}, got {got}).")]
    SessionIdMismatch {
        /// The session arg.
        arg: String,
        /// The session id expected (filename-derived).
        expected: Uuid,
        /// The session id observed in the offending row.
        got: Uuid,
    },
    /// The interactive picker received 3 invalid inputs in a row.
    #[error("Invalid selection (3 attempts). Aborting.")]
    InvalidSelection,
    /// I/O error during dir listing / file open / file read.
    #[error("Session {arg} I/O error: {source}")]
    Io {
        /// The path / arg the I/O was attempted against.
        arg: String,
        /// Wrapped I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The cwd's project dir has no `.jsonl` files at all.
    #[error("No conversations found to resume.")]
    EmptyDirectory,
    /// A `.jsonl` file failed to deserialize (delegates to the reader's error).
    #[error("Session {arg} parse error: {source}")]
    Parse {
        /// The arg / path.
        arg: String,
        /// Wrapped parse failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Resolve `<lingxi_home>/projects/<sanitize(cwd)>[-djb2]` for a given cwd.
fn project_dir_for_cwd(lingxi_home: &Path, cwd: &str) -> PathBuf {
    lingxi_home.join("projects").join(project_dir_name(cwd))
}

fn canonicalized_path_string(path: &str) -> String {
    normalized_directory_path(Path::new(path))
        .to_string_lossy()
        .into_owned()
}

fn transcript_cwd_from_path(path: &Path, loaded: &LoadedTranscript) -> Option<String> {
    let session_id = path.file_stem().and_then(|stem| stem.to_str());
    if let Some(relocated) = session_id.and_then(|id| loaded.relocated_cwds.get(id)) {
        if !relocated.is_empty() {
            return Some(relocated.clone());
        }
    }
    if let Some(cwd) = session_id.and_then(|id| loaded.mobile_empty_session_cwds.get(id)) {
        return Some(cwd.clone());
    }
    loaded
        .messages_in_order
        .first()
        .map(|message| message.cwd.clone())
        .filter(|cwd| !cwd.is_empty())
}

/// Recover a workspace launch hint from the selected transcript. A relocation
/// marker is authoritative; without one, the first message's cwd is accepted
/// only when its sanitized directory name agrees with the selected file.
#[must_use]
pub fn transcript_workspace_path(path: &Path, loaded: &LoadedTranscript) -> Option<String> {
    let session_id = path.file_stem().and_then(|stem| stem.to_str());
    if let Some(relocated) = session_id.and_then(|id| loaded.relocated_cwds.get(id)) {
        if !relocated.is_empty() && Path::new(&relocated).is_absolute() {
            return Some(relocated.clone());
        }
    }
    let cwd = session_id
        .and_then(|id| loaded.mobile_empty_session_cwds.get(id))
        .map(String::as_str)
        .or_else(|| {
            loaded
                .messages_in_order
                .first()
                .map(|message| message.cwd.as_str())
        })?;
    if cwd.is_empty() || !Path::new(cwd).is_absolute() {
        return None;
    }
    let bucket = path.parent()?.file_name()?.to_str()?;
    (project_dir_name(cwd) == bucket || project_dir_name(&canonicalized_path_string(cwd)) == bucket)
        .then(|| cwd.to_string())
}

fn transcript_matches_target_dirs(transcript_cwd: Option<&str>, target_cwds: &[String]) -> bool {
    let Some(transcript_cwd) = transcript_cwd else {
        return true;
    };
    // Native multi-worktree rows carry ownWorktrees. Any recorded directory
    // inside one of those roots is already owned by this repository (g3r).
    if target_cwds.len() > 1 && belongs_to_worktree(transcript_cwd, target_cwds) {
        return true;
    }
    !target_cwds
        .iter()
        .any(|target| foreign_colliding_directory(transcript_cwd, target))
}

fn belongs_to_worktree(path: &str, roots: &[String]) -> bool {
    let path = normalized_identity_path(path).to_lowercase();
    roots.iter().any(|root| {
        let root = normalized_identity_path(root).to_lowercase();
        path == root
            || path.starts_with(&if root.ends_with('/') {
                root
            } else {
                format!("{root}/")
            })
    })
}

fn colliding_directory_spellings(left: &str, right: &str) -> bool {
    let left = left.nfc().collect::<String>();
    let right = right.nfc().collect::<String>();
    sanitized_cwd(&left).eq_ignore_ascii_case(&sanitized_cwd(&right))
        && normalized_identity_path(&left).to_lowercase()
            != normalized_identity_path(&right).to_lowercase()
}

/// Native MG's namespace checks, before touching a possibly remote path.
fn guarded_directory_namespace(path: &str, devices: bool) -> bool {
    let separator = |character| matches!(character, '/' | '\\');
    let bytes = path.as_bytes();
    let unc =
        bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\');
    let nt = bytes.len() >= 4
        && matches!(bytes[0], b'/' | b'\\')
        && &bytes[1..3] == b"??"
        && matches!(bytes[3], b'/' | b'\\');
    if unc {
        let mut parts = path[2..].split(separator);
        let host = parts.next().unwrap_or("");
        let distribution = parts.next().unwrap_or("");
        let local_wsl = (host.eq_ignore_ascii_case("wsl$")
            || host.eq_ignore_ascii_case("wsl.localhost"))
            && distribution
                .chars()
                .any(|character| !matches!(character, '.' | ' '));
        if !local_wsl {
            return true;
        }
    }
    if devices {
        let mut windows_parts = Vec::new();
        if path.contains("??") {
            for part in path.split(separator) {
                match part {
                    "" | "." => {}
                    ".." => {
                        windows_parts.pop();
                    }
                    part => windows_parts.push(part),
                }
            }
        }
        let normalized_nt = path.starts_with(separator)
            && windows_parts.first().is_some_and(|part| *part == "??")
            && windows_parts.len() > 1;
        let first = path
            .trim_start_matches(separator)
            .split(separator)
            .next()
            .unwrap_or("");
        if nt
            || normalized_nt
            || (path.starts_with(separator)
                && ["global??", "globalroot", "dosdevices", "device"]
                    .iter()
                    .any(|prefix| first.eq_ignore_ascii_case(prefix))
                && path.trim_start_matches(separator).contains(separator))
        {
            return true;
        }
    }
    if !path.starts_with('/') {
        return false;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
        if parts.len() == 1
            && ["network", ".vol", ".file", ".nofollow", ".resolve"]
                .iter()
                .any(|root| parts[0].eq_ignore_ascii_case(root))
        {
            return true;
        }
        if parts.len() == 2 && parts[0].eq_ignore_ascii_case("net") {
            return true;
        }
    }
    parts.len() == 1 && parts[0].eq_ignore_ascii_case("net")
}

fn lexical_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// The local default branch of native mkt/Ce. Safe local links can resolve;
/// namespace landings and exhausted link chains are unverifiable. Ordinary
/// lstat/readlink errors return no route, matching Ce with default options.
fn guarded_directory_route(path: &str) -> bool {
    if path.split(['/', '\\']).any(|part| part == "..") || guarded_directory_namespace(path, false)
    {
        return true;
    }
    let absolute = lexical_absolute_path(Path::new(path));
    let mut current = PathBuf::from("/");
    let mut pending = absolute
        .to_string_lossy()
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect::<std::collections::VecDeque<_>>();
    let mut hops = 0;
    let mut seen = HashSet::new();
    while !pending.is_empty() && hops < 64 {
        let component = pending.front().expect("non-empty route");
        let next = lexical_absolute_path(&current.join(component));
        if guarded_directory_namespace(&next.to_string_lossy(), false) {
            return true;
        }
        let metadata = match std::fs::symlink_metadata(&next) {
            Ok(metadata) => metadata,
            Err(error) => {
                #[cfg(unix)]
                if !matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT | libc::ENOTDIR | libc::ENAMETOOLONG)
                ) {
                    use std::os::unix::fs::OpenOptionsExt;
                    if std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                        .open(&next)
                        .is_ok()
                    {
                        pending.pop_front();
                        current = next;
                        continue;
                    }
                }
                return false;
            }
        };
        if !metadata.file_type().is_symlink() {
            pending.pop_front();
            current = next;
            continue;
        }
        hops += 1;
        if !seen.insert((next.clone(), pending.iter().cloned().collect::<Vec<_>>())) {
            return false;
        }
        let Ok(target) = std::fs::read_link(&next) else {
            return false;
        };
        let target = target.to_string_lossy();
        pending.pop_front();
        if target.starts_with('/') {
            if guarded_directory_namespace(&target, false) {
                return true;
            }
            current = PathBuf::from("/");
        }
        for component in target.split('/').filter(|part| !part.is_empty()).rev() {
            pending.push_front(component.to_string());
        }
    }
    !pending.is_empty()
}

fn foreign_colliding_directory(recorded: &str, target: &str) -> bool {
    if !colliding_directory_spellings(recorded, target)
        || guarded_directory_namespace(recorded, true)
        || guarded_directory_namespace(target, true)
        || guarded_directory_route(recorded)
        || guarded_directory_route(target)
    {
        return false;
    }
    let Ok(recorded) = std::fs::canonicalize(recorded) else {
        return false;
    };
    colliding_directory_spellings(
        &recorded.to_string_lossy(),
        &canonicalized_path_string(target),
    )
}

fn transcript_logical_modified(
    loaded: &LoadedTranscript,
    file_modified: SystemTime,
) -> Option<SystemTime> {
    let last = loaded
        .messages_in_order
        .iter()
        .rev()
        .find(|message| matches!(message.message_type.as_str(), "user" | "assistant"))?;
    let modified_ms = timestamp_millis(&last.timestamp);
    if modified_ms == i64::MIN {
        return None;
    }
    let transcript_modified = SystemTime::UNIX_EPOCH.checked_add(
        std::time::Duration::from_millis(u64::try_from(modified_ms).ok()?),
    )?;
    Some(std::cmp::min(transcript_modified, file_modified))
}

/// Parse the `worktree ` lines of `git worktree list --porcelain` into absolute
/// path strings — 1:1 with claude-code `getWorktreePaths`'s porcelain parse
/// (`claude-code/src/utils/getWorktreePaths.ts:50-53`: keep lines starting with
/// `"worktree "`, strip that prefix).
///
/// Split only on LF and normalize retained paths to NFC, as the native parser
/// does. A carriage return is not silently removed from a path.
fn parse_worktree_list(stdout: &str) -> Vec<String> {
    stdout
        .split('\n')
        .filter_map(|line| {
            line.strip_prefix("worktree ")
                .map(|path| path.nfc().collect())
        })
        .collect()
}

/// Run `git worktree list --porcelain` in `cwd` and return the absolute worktree
/// paths, or an empty vec on failure (git missing, `cwd` unreadable, not a repo,
/// non-zero exit). A single root still identifies which aliases and retained
/// worktree buckets belong to a session launched in a repository subdirectory.
///
/// Uses [`std::process::Command`] (no new dependency; `tokio`'s `process` feature
/// is not enabled in this crate). `output()` blocks the calling task briefly,
/// which is acceptable for the one-shot, interactive `/resume` entry point.
fn git_worktree_paths(cwd: &str) -> Vec<String> {
    let output = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_worktree_list(&stdout)
}

async fn transcript_windows(path: &Path) -> Option<(String, String)> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let metadata = file.metadata().await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    let window = crate::jsonl::LITE_READ_BUF_SIZE;
    let mut buffer = vec![0; window];
    let read = file.read(&mut buffer).await.ok()?;
    if read == 0 {
        return None;
    }
    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
    let offset = metadata.len().saturating_sub(u64::try_from(window).ok()?);
    let tail = if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset)).await.ok()?;
        let read = file.read(&mut buffer).await.ok()?;
        String::from_utf8_lossy(&buffer[..read]).into_owned()
    } else {
        head.clone()
    };
    Some((head, tail))
}

fn strict_head_cwd(head: &str) -> Option<String> {
    head.split('\n').find_map(|line| {
        if !line.contains("\"cwd\":") {
            return None;
        }
        serde_json::from_str::<Value>(line)
            .ok()?
            .get("cwd")?
            .as_str()
            .map(str::to_string)
    })
}

fn scoped_cwd_from_source(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let window = crate::jsonl::LITE_READ_BUF_SIZE;
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(window)]);
    let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(window)..]);
    find_last_typed_field(&tail, "relocated", "relocatedCwd").or_else(|| strict_head_cwd(&head))
}

fn normalized_identity_path(path: &str) -> String {
    path.nfc().collect::<String>().replace('\\', "/")
}

fn ambiguous_project_path(path: &str) -> bool {
    path.chars().any(|character| {
        !character.is_ascii_alphanumeric() && !matches!(character, '/' | '\\' | ':' | '-')
    })
}

async fn bucket_transcripts_by_mtime(project_dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(mut entries) = tokio::fs::read_dir(project_dir).await else {
        return Vec::new();
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !entry.file_type().await.is_ok_and(|kind| kind.is_file())
            || path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
            || !path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(crate::jsonl::validate_uuid)
        {
            continue;
        }
        if let Ok(metadata) = entry.metadata().await {
            files.push((path, metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        }
    }
    files.sort_by(|left, right| right.1.cmp(&left.1));
    files
}

/// Native Ice/NAt only invoke collision recovery when every scoped row has a
/// verified foreign cwd. Unknown or malformed head/tail data prevents it.
async fn buckets_are_foreign(
    directories: &[PathBuf],
    target_cwds: &[String],
    limit: usize,
) -> bool {
    for directory in directories {
        let mut files = bucket_transcripts_by_mtime(directory).await;
        if limit > 0 {
            files.truncate(limit);
        }
        for (path, _) in files {
            let Some((head, tail)) = transcript_windows(&path).await else {
                return false;
            };
            let recorded = find_last_typed_field(&tail, "relocated", "relocatedCwd")
                .or_else(|| strict_head_cwd(&head));
            if recorded.is_none()
                || transcript_matches_target_dirs(recorded.as_deref(), target_cwds)
            {
                return false;
            }
        }
    }
    true
}

/// Native FAt/O0e: classify a storage directory by its newest UUID file's
/// strict head cwd or final tail relocation. Do not infer ownership from an
/// arbitrary sanitized directory name or inspect older files after that row.
async fn collision_recovery_directories(
    lingxi_home: &Path,
    cwd: &str,
    excluded: &Path,
    include_descendants: bool,
) -> Vec<PathBuf> {
    let Ok(mut entries) = tokio::fs::read_dir(lingxi_home.join("projects")).await else {
        return Vec::new();
    };
    let expected = normalized_identity_path(cwd);
    let mut directories = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let directory = entry.path();
        if directory == excluded || !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let files = bucket_transcripts_by_mtime(&directory).await;
        let Some((newest, _)) = files.first() else {
            continue;
        };
        let Some((head, tail)) = transcript_windows(newest).await else {
            continue;
        };
        let recorded = find_last_typed_field(&tail, "relocated", "relocatedCwd")
            .or_else(|| strict_head_cwd(&head));
        let Some(recorded) = recorded else { continue };
        let recorded = normalized_identity_path(&recorded);
        if recorded == expected
            || (include_descendants && recorded.starts_with(&format!("{expected}/")))
        {
            directories.push(directory);
        }
    }
    directories
}

/// Native Kbn/ne: a truncated bucket is associated with a registered root only
/// when one regular JSONL file's last relocation or strict head cwd sanitizes
/// to the same complete root. The head and tail are bounded 64-KiB windows.
async fn bucket_matches_worktree_root(project_dir: &Path, root: &str) -> bool {
    let Ok(mut entries) = tokio::fs::read_dir(project_dir).await else {
        return false;
    };
    let expected = sanitized_cwd(root);
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_type().await.is_ok_and(|kind| kind.is_file())
            || entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("jsonl")
        {
            continue;
        }
        let Some((head, tail)) = transcript_windows(&entry.path()).await else {
            continue;
        };
        let recorded = find_last_typed_field(&tail, "relocated", "relocatedCwd")
            .or_else(|| strict_head_cwd(&head));
        if recorded.is_some_and(|cwd| sanitized_cwd(&cwd.nfc().collect::<String>()) == expected) {
            return true;
        }
    }
    false
}

/// Native rRe for local storage: retain the normal directory first, then
/// verified directories with the same truncated prefix. This also recovers a
/// long cwd stored under an older suffix without guessing from its directory.
async fn project_directories_for_cwd(lingxi_home: &Path, cwd: &str) -> Vec<PathBuf> {
    let current = project_dir_for_cwd(lingxi_home, cwd);
    let mut directories = Vec::new();
    if tokio::fs::read_dir(&current).await.is_ok() {
        directories.push(current.clone());
    }
    let name = project_dir_name(cwd);
    if name.len() <= MAX_SANITIZED_LENGTH {
        return directories;
    }
    let prefix = format!("{}-", &name[..MAX_SANITIZED_LENGTH]);
    let Ok(mut entries) = tokio::fs::read_dir(lingxi_home.join("projects")).await else {
        return directories;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path() == current
            || !entry.file_type().await.is_ok_and(|kind| kind.is_dir())
            || !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
        {
            continue;
        }
        if bucket_matches_worktree_root(&entry.path(), cwd).await {
            directories.push(entry.path());
        }
    }
    directories
}

async fn append_repository_project_dirs(
    projects_root: &Path,
    directories: &mut Vec<PathBuf>,
    worktree_paths: &[String],
) -> Result<bool, LoaderError> {
    let mut roots: Vec<_> = worktree_paths
        .iter()
        .map(|root| (root, project_dir_name(root)))
        .collect();
    roots.sort_by(|left, right| right.1.len().cmp(&left.1.len()));
    let Ok(mut entries) = tokio::fs::read_dir(projects_root).await else {
        return Ok(false);
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| LoaderError::Io {
            arg: projects_root.display().to_string(),
            source,
        })?
    {
        if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        for (root, exact_name) in &roots {
            let matches = if name == exact_name.as_str() {
                true
            } else if exact_name.len() > MAX_SANITIZED_LENGTH {
                name.starts_with(&format!("{}-", &exact_name[..MAX_SANITIZED_LENGTH]))
                    && bucket_matches_worktree_root(&entry.path(), root).await
            } else {
                exact_name.len() < MAX_SANITIZED_LENGTH && worktree_dir_matches(name, exact_name)
            };
            if matches {
                push_project_dir(directories, entry.path());
                break;
            }
        }
    }
    Ok(true)
}

struct DiscoveryDirectories {
    project_dirs: Vec<PathBuf>,
    target_cwds: Vec<String>,
    unfiltered_dirs: Vec<PathBuf>,
    limited_dirs: Vec<PathBuf>,
    has_aliases: bool,
}

fn push_project_dir(directories: &mut Vec<PathBuf>, path: PathBuf) {
    if !directories.contains(&path) {
        directories.push(path);
    }
}

async fn append_project_dirs_matching(
    projects_root: &Path,
    directories: &mut Vec<PathBuf>,
    matches: impl Fn(&str) -> bool,
) -> Result<(), LoaderError> {
    // Native directory enumeration falls back to the current bucket when the
    // projects root cannot be read. It has already been included by the caller.
    let Ok(mut entries) = tokio::fs::read_dir(projects_root).await else {
        return Ok(());
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| LoaderError::Io {
            arg: projects_root.display().to_string(),
            source,
        })?
    {
        if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if entry.file_name().to_str().is_some_and(&matches) {
            push_project_dir(directories, entry.path());
        }
    }
    Ok(())
}

async fn discover_session_directories(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    worktree_paths: &[String],
    limit: usize,
) -> Result<DiscoveryDirectories, LoaderError> {
    let projects_root = lingxi_home.join("projects");
    let current_project = project_dir_for_cwd(lingxi_home, cwd);
    let current_storage = if scope == SessionDiscoveryScope::Current {
        project_directories_for_cwd(lingxi_home, cwd)
            .await
            .into_iter()
            .next()
            .unwrap_or_else(|| current_project.clone())
    } else {
        current_project.clone()
    };
    let mut project_dirs = vec![current_storage];
    let mut target_cwds = vec![cwd.to_string()];
    let mut unfiltered_dirs = Vec::new();
    let mut limited_dirs = Vec::new();
    let mut has_aliases = false;
    if scope == SessionDiscoveryScope::All {
        append_project_dirs_matching(&projects_root, &mut project_dirs, |_| true).await?;
        return Ok(DiscoveryDirectories {
            project_dirs,
            target_cwds: Vec::new(),
            unfiltered_dirs,
            limited_dirs,
            has_aliases,
        });
    }

    let canonical_cwd = normalized_directory_path(Path::new(cwd));
    let repository_root = worktree_paths
        .iter()
        .filter(|root| canonical_cwd.starts_with(Path::new(root)))
        .max_by_key(|root| Path::new(root).components().count());
    if scope == SessionDiscoveryScope::Current
        && ambiguous_project_path(cwd)
        && buckets_are_foreign(&project_dirs, &target_cwds, limit).await
    {
        project_dirs =
            collision_recovery_directories(lingxi_home, cwd, &project_dirs[0], false).await;
    }
    if scope == SessionDiscoveryScope::Repository {
        target_cwds.extend(
            worktree_paths
                .iter()
                .filter(|root| root.as_str() != cwd)
                .cloned(),
        );
        if worktree_paths.len() > 1 {
            if repository_root.is_some() {
                project_dirs.clear();
            }
            if !append_repository_project_dirs(&projects_root, &mut project_dirs, worktree_paths)
                .await?
            {
                limited_dirs.push(current_project.clone());
            }
            // A long/truncated bucket name must not make the current session
            // disappear, even when it does not match a registered root prefix.
            push_project_dir(&mut project_dirs, current_project.clone());
            for root in &target_cwds {
                if !ambiguous_project_path(root) {
                    continue;
                }
                let scoped = project_dirs
                    .iter()
                    .filter(|directory| {
                        directory
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| worktree_dir_matches(name, &project_dir_name(root)))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if buckets_are_foreign(&scoped, &target_cwds, 0).await {
                    for recovered in collision_recovery_directories(
                        lingxi_home,
                        &canonicalized_path_string(root),
                        &project_dir_for_cwd(lingxi_home, root),
                        true,
                    )
                    .await
                    {
                        push_project_dir(&mut limited_dirs, recovered.clone());
                        push_project_dir(&mut project_dirs, recovered);
                    }
                }
            }
        } else {
            if ambiguous_project_path(cwd)
                && buckets_are_foreign(&project_dirs, &[cwd.to_string()], 0).await
            {
                for recovered in
                    collision_recovery_directories(lingxi_home, cwd, &current_project, false).await
                {
                    push_project_dir(&mut limited_dirs, recovered.clone());
                    push_project_dir(&mut project_dirs, recovered);
                }
            }
            // Native LAt retains sessions from removed managed worktrees even
            // when git now reports only the primary checkout. The namespace
            // remains LingXi's deliberate branding adaptation.
            let prefix = format!(
                "{}-{}-worktrees-",
                project_dir_name(repository_root.map_or(cwd, String::as_str)),
                branding::DOT_DIR.replace('.', "-")
            );
            let before = project_dirs.len();
            append_project_dirs_matching(&projects_root, &mut project_dirs, |name| {
                name.starts_with(&prefix)
            })
            .await?;
            limited_dirs.extend(project_dirs[before..].iter().cloned());
            unfiltered_dirs.extend(project_dirs[before..].iter().cloned());
        }
    }

    // Read only the link files at the current directory and (for Repository)
    // its closest containing worktree root. A linked directory's own aliases
    // are never traversed, nor are aliases at unrelated sibling worktree roots.
    let mut alias_sources = vec![current_project];
    if scope == SessionDiscoveryScope::Repository {
        if let Some(root) = repository_root.filter(|root| root.as_str() != cwd) {
            push_project_dir(&mut alias_sources, project_dir_for_cwd(lingxi_home, root));
        }
    }
    for source in alias_sources {
        for alias in read_session_aliases(&source).await {
            has_aliases = true;
            if !project_dirs.contains(&alias) {
                push_project_dir(&mut unfiltered_dirs, alias.clone());
                push_project_dir(&mut limited_dirs, alias.clone());
            }
            push_project_dir(&mut project_dirs, alias);
        }
    }
    Ok(DiscoveryDirectories {
        project_dirs,
        target_cwds,
        unfiltered_dirs,
        limited_dirs,
        has_aliases,
    })
}

/// claude-code's worktree dir-name match
/// (`getStatOnlyLogsForWorktrees`, `sessionStorage.ts`):
/// `dirName === prefix || dirName.startsWith(prefix + '-')`.
///
/// The `startsWith(prefix + '-')` arm catches sessions launched in a
/// SUBDIRECTORY of the worktree, whose sanitized project-dir name is
/// `<prefix>-<sanitized-subpath>`. The trailing `-` is load-bearing: it stops a
/// prefix like `-x-repo` from matching an unrelated `-x-repository`.
fn worktree_dir_matches(dir_name: &str, prefix: &str) -> bool {
    dir_name == prefix
        || dir_name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Deduplicate session rows by session id (the filename UUID), keeping the row
/// with the newest `modified` time — 1:1 with claude-code
/// `deduplicateLogsBySessionId` (`sessionStorage.ts:4955`), whose
/// `log.modified.getTime() > existing.modified.getTime()` replaces and keeps the
/// first-seen entry on a tie. The same session can appear under multiple
/// worktree project dirs; this collapses it to one.
#[cfg(test)]
fn deduplicate_by_session_id(rows: Vec<SessionMetadata>) -> Vec<SessionMetadata> {
    let mut by_id: HashMap<Uuid, usize> = HashMap::with_capacity(rows.len());
    let mut deduplicated: Vec<SessionMetadata> = Vec::with_capacity(rows.len());
    for row in rows {
        match by_id.get(&row.uuid).copied() {
            // Keep the existing row unless the incoming one is STRICTLY newer.
            Some(index) if deduplicated[index].modified >= row.modified => {}
            Some(index) => deduplicated[index] = row,
            None => {
                by_id.insert(row.uuid, deduplicated.len());
                deduplicated.push(row);
            }
        }
    }
    deduplicated
}

/// `isLoopSession` — detect whether a transcript was started by `/loop`, by
/// finding its FIRST GENUINE user prompt and testing it for the `/loop`
/// command-name tag. 1:1 with the CC 2.1.212 detector (the 2.1.211 fix that
/// stopped `/loop` from hiding sessions from `/resume` after a single use):
/// the pre-fix code tested `messages[0]` blindly, so a compacted or
/// meta-first transcript checked the WRONG line and mis-hid (or mis-showed)
/// the session. The fixed detector iterates messages and:
///   - skips any line that is not `type === "user"`,
///   - skips `isMeta === true` and `isCompactSummary === true`,
///   - skips user lines whose `message.content` array carries a `tool_result`
///     (tool-result turns are user-typed only in shape, not intent),
/// then returns whether the FIRST surviving user message's text contains
/// `"<command-name>/loop</command-name>"` (@ binary offset ~339753,
/// tag confirmed @ 113388700). It returns on the first real prompt — a
/// non-`/loop` first prompt yields `false` without scanning further.
fn is_loop_session(messages: &[JsonlMessage]) -> bool {
    const LOOP_TAG: &str = "<command-name>/loop</command-name>";
    for m in messages {
        // `if(s.type!=="user") continue` (mirrors the raw `"type":"user"` prefilter).
        if m.message_type != "user" {
            continue;
        }
        // `if(s.isMeta===!0||s.isCompactSummary===!0) continue`.
        if m.extra
            .get_property("isMeta")
            .and_then(JsonValueView::as_bool)
            == Some(true)
            || m.extra
                .get_property("isCompactSummary")
                .and_then(JsonValueView::as_bool)
                == Some(true)
        {
            continue;
        }
        // `let a=s.message; if(!a) continue` — a null/absent message is skipped.
        if m.message.at_pointer("").is_some_and(JsonValueView::is_null) {
            continue;
        }
        // `let l=a.content` — string → single candidate; array → gather `text`
        // blocks but bail out of THIS message if it carries a `tool_result`.
        let content = m.message.at_pointer("/content");
        let mut texts: Vec<String> = Vec::new();
        if let Some(s) = content.and_then(JsonValueView::as_str) {
            texts.push(s.to_owned());
        } else if let Some(arr) = content.and_then(JsonValueView::array_len) {
            let mut has_tool_result = false;
            for index in 0..arr {
                let Some(block) = content.and_then(|value| value.get_index(index)) else {
                    continue;
                };
                match block.get_property("type").and_then(JsonValueView::as_str) {
                    Some("tool_result") => {
                        has_tool_result = true;
                        break;
                    }
                    Some("text") => {
                        if let Some(t) = block.get_property("text").and_then(JsonValueView::as_str)
                        {
                            texts.push(t.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            if has_tool_result {
                continue;
            }
        }
        // `return c.some((u)=>u.includes("<command-name>/loop</command-name>"))`
        // — decide on the FIRST genuine prompt and stop.
        return texts.iter().any(|t| t.contains(LOOP_TAG));
    }
    false
}

/// Scan a single project dir, appending one [`SessionMetadata`] row per resumable
/// `.jsonl` file to `rows`. Applies the SESSION.1 sidechain/`teamName` hide
/// filter (first parsed line only). Each row's `title` follows claude-code's
/// display precedence — `custom-title` > `ai-title` > `summary` (keyed by the
/// chain tip's `leafUuid`) > first-user-message — composed from
/// `readLiteMetadata`'s custom-over-ai rule (`sessionStorage.ts:4771-4775`) and
/// `getLogDisplayTitle` (`utils/log.ts:30`). Returns `Ok(false)` when the dir
/// does not exist (`NotFound`) and `Ok(true)` when it was read. Directory-level
/// I/O errors are returned; unreadable or wholly corrupt candidate files are
/// counted in `skipped_files` so readable siblings can still be listed.
#[derive(Clone)]
struct CatalogCandidate {
    uuid: Uuid,
    path: PathBuf,
    stem: String,
    file_len: u64,
    file_modified: SystemTime,
    file_created: SystemTime,
}

/// Native G9 runs on KBe's lite rows before jAt clamps logical timestamps.
/// Its identity is the original filename stem, including UUID case.
fn deduplicate_catalog_candidates(candidates: Vec<CatalogCandidate>) -> Vec<CatalogCandidate> {
    let mut by_id: HashMap<String, usize> = HashMap::with_capacity(candidates.len());
    let mut rows: Vec<CatalogCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match by_id.get(&candidate.stem).copied() {
            Some(index) if rows[index].file_modified >= candidate.file_modified => {}
            Some(index) => rows[index] = candidate,
            None => {
                by_id.insert(candidate.stem.clone(), rows.len());
                rows.push(candidate);
            }
        }
    }
    rows.sort_by(|left, right| {
        right
            .file_modified
            .cmp(&left.file_modified)
            .then_with(|| right.file_created.cmp(&left.file_created))
    });
    rows
}

#[derive(Clone)]
struct CatalogCacheEntry {
    fs_identity: FileSystemCacheIdentity,
    path: PathBuf,
    file_len: u64,
    file_modified: SystemTime,
    file_created: SystemTime,
    current_is_sdk_entrypoint: bool,
    target_cwds: Vec<String>,
    scoped_cwd: Option<String>,
    row: Option<SessionMetadata>,
    skipped: bool,
}

fn catalog_cache() -> &'static Mutex<Vec<CatalogCacheEntry>> {
    static CACHE: OnceLock<Mutex<Vec<CatalogCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

async fn collect_dir(
    dir: &Path,
    _target_cwds: &[String],
    fs: &Arc<dyn FileSystem>,
    candidates: &mut Vec<CatalogCandidate>,
    skipped_files: &mut usize,
) -> Result<bool, LoaderError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(LoaderError::Io {
                arg: dir.display().to_string(),
                source,
            });
        }
    };

    // Process-constant: SDK runtimes do not hide SDK/loop sessions from the picker.
    let current_is_sdk_entrypoint = matches!(
        std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Ok("sdk-cli" | "sdk-ts" | "sdk-py")
    );

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| LoaderError::Io {
            arg: dir.display().to_string(),
            source,
        })?
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let metadata = match entry.metadata().await {
            Ok(metadata) => metadata,
            Err(_) => {
                *skipped_files += 1;
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let file_modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => {
                *skipped_files += 1;
                continue;
            }
        };
        // `created()` is the parity analog of TS `st.birthtime`. Unlike
        // `modified()` it is NOT available on every platform/filesystem — it
        // returns `Err` where birthtime is unsupported — so we fall back to
        // `modified` there (the equal-mtime tie-break then degrades to a stable
        // no-op rather than failing the whole scan). No new dependency: this is
        // std-only `std::fs::Metadata::created`.
        let file_created = metadata.created().unwrap_or(file_modified);

        // Parse uuid from filename stem; silently skip non-UUID files.
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !crate::jsonl::validate_uuid(stem) {
            continue;
        }
        let Ok(uuid) = Uuid::parse_str(stem) else {
            continue;
        };
        let stem = stem.to_string();

        // Cheap first-line hide: sidechain / daemon / SDK sessions can be
        // skipped without parsing the rest of the JSONL. Survivors still take
        // the full `read_routed` path (custom titles live at file tail).
        let path_str = path.to_str().unwrap_or("");
        if !path_str.is_empty() {
            if let Ok(prefix) = fs
                .read_file_prefix(path_str, crate::jsonl::LITE_READ_BUF_SIZE)
                .await
            {
                let content = prefix
                    .content
                    .strip_prefix('\u{FEFF}')
                    .unwrap_or(&prefix.content);
                let line1 = content.split('\n').next().unwrap_or("");
                if first_line_hides_session(line1, current_is_sdk_entrypoint) {
                    continue;
                }
            }
        }
        candidates.push(CatalogCandidate {
            uuid,
            path,
            stem,
            file_len: metadata.len(),
            file_modified,
            file_created,
        });
    }
    Ok(true)
}

/// Enrich every candidate, several at a time.
///
/// Each row is an independent full parse of one transcript, so a project with
/// many large sessions lists in roughly the time of its largest files rather
/// than their sum. Rows and errors keep candidate order, exactly as a
/// sequential scan would report them.
async fn enrich_candidates(
    candidates: Vec<CatalogCandidate>,
    fs: &Arc<dyn FileSystem>,
    current_is_sdk_entrypoint: bool,
    target_cwds: &[String],
    unfiltered_dirs: &[PathBuf],
    skipped_files: &mut usize,
) -> Result<Vec<SessionMetadata>, LoaderError> {
    let parallelism = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(1, 8);
    let cache_identity = fs.cache_identity();
    let total = candidates.len();
    let mut pending = candidates.into_iter().enumerate();
    let mut jobs = tokio::task::JoinSet::new();
    let mut results = Vec::with_capacity(total);
    loop {
        while jobs.len() < parallelism {
            let Some((index, candidate)) = pending.next() else {
                break;
            };
            let fs = Arc::clone(fs);
            let cache_identity = cache_identity.clone();
            let target_cwds =
                if candidate.path.parent().is_some_and(|parent| {
                    unfiltered_dirs.iter().any(|directory| directory == parent)
                }) {
                    Vec::new()
                } else {
                    target_cwds.to_vec()
                };
            jobs.spawn(async move {
                let mut skipped = 0;
                let row = enrich_candidate(
                    candidate,
                    &fs,
                    cache_identity.as_ref(),
                    current_is_sdk_entrypoint,
                    &target_cwds,
                    &mut skipped,
                )
                .await;
                (index, row, skipped)
            });
        }
        let Some(joined) = jobs.join_next().await else {
            break;
        };
        match joined {
            Ok(result) => results.push(result),
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => {
                return Err(LoaderError::Io {
                    arg: "session catalog".to_string(),
                    source: std::io::Error::other(error),
                });
            }
        }
    }
    results.sort_by_key(|(index, _, _)| *index);
    let mut rows = Vec::with_capacity(results.len());
    for (_, row, skipped) in results {
        *skipped_files += skipped;
        if let Some(row) = row? {
            rows.push(row);
        }
    }
    Ok(rows)
}

async fn enrich_candidate(
    candidate: CatalogCandidate,
    fs: &Arc<dyn FileSystem>,
    cache_identity: Option<&FileSystemCacheIdentity>,
    current_is_sdk_entrypoint: bool,
    target_cwds: &[String],
    skipped_files: &mut usize,
) -> Result<Option<SessionMetadata>, LoaderError> {
    let Some(cache_identity) = cache_identity else {
        let mut scoped_cwd = None;
        return enrich_candidate_uncached(
            candidate,
            fs,
            current_is_sdk_entrypoint,
            target_cwds,
            &mut scoped_cwd,
            skipped_files,
        )
        .await;
    };
    let cache = catalog_cache();
    {
        let cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cache.iter().find(|entry| {
            entry.fs_identity == *cache_identity
                && entry.path == candidate.path
                && entry.file_len == candidate.file_len
                && entry.file_modified == candidate.file_modified
                && entry.file_created == candidate.file_created
                && entry.current_is_sdk_entrypoint == current_is_sdk_entrypoint
                && entry.target_cwds == target_cwds
        }) {
            // Directory existence or a local link target can change without
            // changing the transcript revision. Recheck native m3r for a
            // cached row; a filtered row must be reparsed if it may now belong.
            if !transcript_matches_target_dirs(entry.scoped_cwd.as_deref(), target_cwds) {
                return Ok(None);
            }
            if target_cwds.is_empty() || entry.row.is_some() || entry.scoped_cwd.is_none() {
                if entry.skipped {
                    *skipped_files += 1;
                }
                return Ok(entry.row.clone());
            }
        }
    }

    let skipped_before = *skipped_files;
    let mut scoped_cwd = None;
    let result = enrich_candidate_uncached(
        candidate.clone(),
        fs,
        current_is_sdk_entrypoint,
        target_cwds,
        &mut scoped_cwd,
        skipped_files,
    )
    .await?;
    let skipped = *skipped_files > skipped_before;
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= 128 {
        cache.remove(0);
    }
    cache.push(CatalogCacheEntry {
        fs_identity: cache_identity.clone(),
        path: candidate.path,
        file_len: candidate.file_len,
        file_modified: candidate.file_modified,
        file_created: candidate.file_created,
        current_is_sdk_entrypoint,
        target_cwds: target_cwds.to_vec(),
        scoped_cwd,
        row: result.clone(),
        skipped,
    });
    Ok(result)
}

async fn enrich_candidate_uncached(
    candidate: CatalogCandidate,
    fs: &Arc<dyn FileSystem>,
    current_is_sdk_entrypoint: bool,
    target_cwds: &[String],
    scoped_cwd: &mut Option<String>,
    skipped_files: &mut usize,
) -> Result<Option<SessionMetadata>, LoaderError> {
    let CatalogCandidate {
        uuid,
        path,
        stem,
        file_modified,
        file_created,
        ..
    } = candidate;
    // Route the file TOLERANTLY ([`read_routed`]) rather than `read_all`:
    // besides the chain participants (`messages_in_order`, identical to what
    // `read_all` returned) it yields the Tier-1 metadata side-maps the picker
    // needs to surface a session's stored title — `summaries` (keyed by
    // leafUuid), `custom_titles` + `ai_titles` (keyed by sessionId). See
    // `LoadedTranscript`.
    let content = match fs
        .read_file(path.to_str().expect("UTF-8 path"), None, None)
        .await
    {
        Ok(read) => read.content,
        Err(_) => {
            *skipped_files += 1;
            return Ok(None);
        }
    };
    *scoped_cwd = scoped_cwd_from_source(&content);
    // The parse is the whole cost of a row: keep it off the async workers so
    // concurrent rows (see `enrich_candidates`) parse on separate threads.
    let loaded = match tokio::task::spawn_blocking(move || route_lines(&content)).await {
        Ok(loaded) => loaded,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => {
            *skipped_files += 1;
            return Ok(None);
        }
    };

    // A metadata-only file is not a resumable session unless its latest
    // custom-title is the explicit versioned mobile empty-session anchor.
    // This semantic check belongs in the shared loader; Electron must not
    // blanket-filter zero-count rows because validated empty sessions are
    // real UUID-preserving conversation targets on mobile.
    if loaded.messages_in_order.is_empty() && !loaded.mobile_empty_sessions.contains(&stem) {
        if loaded.malformed_line_count > 0 {
            *skipped_files += 1;
        }
        return Ok(None);
    }

    // The prefix check is only an early reject. A missing/partial first
    // line must not turn into an implicit allow: re-run the authoritative
    // cwd extraction after the tolerant full parse.
    if !transcript_matches_target_dirs(scoped_cwd.as_deref(), target_cwds) {
        return Ok(None);
    }

    let created = file_created;
    let modified = transcript_logical_modified(&loaded, file_modified).unwrap_or(file_modified);

    // SESSION.1 — claude-code HIDES sub-agent / sidechain transcripts from
    // the /resume picker. The decision is made from the FIRST line only:
    //   - `parseSessionInfoFromLite` returns null when the file's first line
    //     contains `"isSidechain":true` (listSessionsImpl.ts:88-95);
    //   - `enrichLog` returns null when the first entry `isSidechain` OR
    //     carries a truthy `teamName` (sessionStorage.ts:5055-5067);
    //   - `filterResumableSessions` drops `l.isSidechain` (resume picker).
    // Mirror that: inspect only the first chain-participant line (the first
    // parsed transcript line — we do NOT scan the whole file for the decision)
    // and skip the session when it is a sidechain message or carries a truthy
    // `teamName`. `teamName` is an outer field captured in
    // `JsonlMessage::extra`; the truthiness test matches TS
    // `if (enriched.teamName)` (an empty-string teamName is falsy).
    if let Some(first) = loaded.messages_in_order.first() {
        let has_team_name = first.extra.get_property("teamName").is_some_and(|value| {
            value
                .as_str()
                .map_or(!value.is_null(), |team_name| !team_name.is_empty())
        });
        if first.is_sidechain || has_team_name {
            return Ok(None);
        }

        // Gap #2 fix — SESSION.2: filter `sessionKind` daemon sessions.
        // Binary `vkm` (@ 206492423):
        //   `if(i.sessionKind==="daemon"||i.sessionKind==="daemon-worker") return C(...),null`
        // Binary log: `"$ filtered from /resume: sessionKind="` @ 113414433.
        // `sessionKind` is carried in `extra` (outer field, not a named
        // struct field). SC-07: the writer half now stamps it
        // (`jsonl::writer::stamp_session_kind`), so this filter can
        // actually fire on a LingXi-written transcript; both halves share
        // `SESSION_KIND_KEY` so the spelling cannot drift apart again.
        let session_kind = first
            .extra
            .get_property(SESSION_KIND_KEY)
            .and_then(JsonValueView::as_str)
            .unwrap_or("");
        if session_kind == "daemon" || session_kind == "daemon-worker" {
            return Ok(None);
        }

        // Gap #3 fix — SESSION.3: filter SDK-entrypoint sessions.
        // Binary `vkm`: `qpn=new Set(["sdk-cli","sdk-ts","sdk-py"])`;
        //   `if(!a && qpn.has(n.entrypoint??"")) return C(...),null`
        // Binary log: `"# filtered from /resume: entrypoint="` @ 113414513.
        // `!a` = the CURRENT process is NOT an SDK runtime (see
        // `current_is_sdk_entrypoint` above): SDK sessions are hidden only from
        // a normal CLI picker. The entrypoint field IS a named struct field.
        let entrypoint = first.entrypoint.as_deref().unwrap_or("");
        if !current_is_sdk_entrypoint && matches!(entrypoint, "sdk-cli" | "sdk-ts" | "sdk-py") {
            return Ok(None);
        }
    }

    // Gap #4 fix — SESSION.4: filter `/loop` sessions.
    // Binary `vkm`: `m=r.includes("<command-name>/loop</command-name>")` (where
    // `r` is the first line raw string), then
    //   `if(!a&&n.isLoopSession) return C(...),null`
    // Binary log: `"% filtered from /resume: /loop session"` @ 113414577.
    // Confirmed string: `"<command-name>/loop</command-name>"` @ 113388700.
    // 2.1.212 fix (2.1.211 changelog): the detector no longer tests
    // `messages[0]` blindly — it walks to the FIRST GENUINE user prompt,
    // skipping non-user lines, `isMeta`/`isCompactSummary` lines, and
    // `tool_result` turns, so a compacted / meta-first transcript is no
    // longer mis-hidden (or mis-shown). See [`is_loop_session`]. We test
    // OUTSIDE the `if let Some(first)` block so we don't shadow the
    // first-check path; the session is only reachable here when it has at
    // least one message (the block above `continue`d otherwise).
    if !current_is_sdk_entrypoint && is_loop_session(&loaded.messages_in_order) {
        return Ok(None);
    }

    // Title precedence — 1:1 with claude-code's resolution, which composes
    // `readLiteMetadata` (custom-title field wins over ai-title field;
    // `sessionStorage.ts:4771-4775`) with `getLogDisplayTitle`
    // (`customTitle || summary || firstPrompt`; `utils/log.ts:30`). Folded:
    //   custom-title > ai-title > summary(@ tip leafUuid) > first-user-message.
    // Custom (user rename) ALWAYS wins over an AI title — `logs.ts:69`
    // "User renames (custom-title) always win over AI titles in read
    // preference". `custom_titles`/`ai_titles` are keyed by sessionId; for a
    // resume-picker row that key is the filename stem (`sid`) — equal to the
    // chain tip's sessionId for the normal, non-forked sessions the picker
    // lists. The `summary` is keyed by the chain TIP's uuid (its `leafUuid`),
    // matching TS `summaries.get(leafMessage.uuid)` (`sessionStorage.ts:3009`).
    // The first-three sources are stored verbatim (only normalized via
    // `truncate_title`); the first-message fallback runs the full
    // `extract_title` transforms. Its `'(session)'` sentinel identifies the
    // empty case; the picker replaces that sentinel with the UUID prefix below.
    let sid = stem.as_str();
    // claude `getLogDisplayTitle` (`gBe`) leads with the session's `agentName`
    // — for an agent-owned session the picker row shows the agent's name above
    // any custom/ai title (`agentName || customTitle || aiTitle || summary ||
    // …`). `agent_names` is keyed by `sessionId` (reader.rs), = the stem `sid`.
    // (The remaining `gBe` tail — `<tick>` → "Autonomous session", the
    // `sessionId.slice(0,8)` empty fallback, and the `dln` wrapped-tag-pair
    // strip — is deferred; it is niche to the picker and the firstPrompt
    // sub-logic is intricate. The port keeps `extract_title`'s first-message
    // path; an empty first-prompt fallback is replaced below with Claude's
    // `sessionId.slice(0, 8)` picker label.)
    let tip = find_tip(&loaded, sid);
    let mut title = loaded
        .agent_names
        .get(sid)
        .or_else(|| loaded.custom_titles.get(sid))
        .or_else(|| loaded.ai_titles.get(sid))
        .or_else(|| tip.and_then(|tip| loaded.summaries.get(&tip.uuid)))
        .map_or_else(
            || extract_title(&loaded.messages_in_order),
            |t| truncate_title(t),
        );
    if title == EMPTY_TITLE_FALLBACK {
        title = if has_autonomous_tick_prompt(&loaded.messages_in_order) {
            "Autonomous session".to_string()
        } else {
            sid.chars().take(8).collect()
        };
    }
    let pr_number = loaded.pr_numbers.get(sid).copied();
    // `OEe`'s match source: `d.customTitle ?? d.aiTitle`. Note this skips
    // `agent_names`, which the DISPLAY title above leads with — an
    // agent-owned session is searchable by the title the user (or the AI)
    // gave it, not by the agent's name.
    let custom_or_ai_title = loaded
        .custom_titles
        .get(sid)
        .or_else(|| loaded.ai_titles.get(sid))
        .cloned();
    let mode = loaded
        .session_modes
        .get(sid)
        .and_then(|value| SessionMode::from_str(value))
        .unwrap_or(SessionMode::Code);
    let (resume_model, resume_model_profile) = persisted_model_ref(&loaded, tip);
    let workspace_path = scoped_cwd
        .as_deref()
        .filter(|cwd| Path::new(cwd).is_absolute())
        .filter(|cwd| {
            path.parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .is_some_and(|bucket| {
                    project_dir_name(cwd) == bucket
                        || project_dir_name(&canonicalized_path_string(cwd)) == bucket
                })
                || target_cwds.iter().any(|target| {
                    canonicalized_path_string(target) == canonicalized_path_string(cwd)
                })
        })
        .map(str::to_string)
        .or_else(|| transcript_workspace_path(&path, &loaded));

    Ok(Some(SessionMetadata {
        uuid,
        title,
        custom_or_ai_title,
        modified,
        created,
        // claude-code `messageCount: countVisibleMessages(chain)`
        // (sessionStorage.ts:2509/4665): only user/assistant lines with
        // VISIBLE content count — NOT every chain-participant line. A raw
        // `.len()` over-counts tool_result-only user lines, tool_use-only
        // assistant lines, isMeta lines, and system/attachment lines.
        message_count: count_visible_messages(&loaded.messages_in_order),
        mode,
        path,
        workspace_path,
        pr_number,
        resume_model,
        resume_model_profile,
    }))
}

/// Recover the same last real assistant model/profile pair used by resume.
/// Synthetic assistant rows never replace the model; an explicit profile
/// field remains last-write-wins, including `null` clearing an older profile.
fn persisted_model_ref(
    loaded: &LoadedTranscript,
    tip: Option<&JsonlMessage>,
) -> (Option<String>, Option<String>) {
    let mut model = None;
    let mut profile = None;
    let mut profile_seen = false;
    let mut seen: HashSet<String> = HashSet::new();
    let preserved = preserved_tail_state(loaded);
    let reparent = &preserved.parents;
    let mut current = tip;
    while let Some(message) = current {
        if !seen.insert(message.uuid.clone()) {
            break;
        }
        if message.message_type == "assistant" {
            if model.is_none() {
                model = message
                    .message
                    .string_at("/model")
                    .filter(|value| {
                        !value.is_empty()
                            && value.len() <= 256
                            && !value.contains('\0')
                            && !(value.starts_with('<') && value.ends_with('>'))
                    })
                    .map(str::to_owned);
            }
            if !profile_seen {
                if let Some(value) = message
                    .extra
                    .get_property("modelProfile")
                    .and_then(JsonValueView::as_str)
                {
                    profile = valid_provider_profile(value).then(|| value.to_owned());
                    profile_seen = true;
                }
            }
            if model.is_some() && profile_seen {
                break;
            }
        }
        let parent = reparent
            .get(message.uuid.as_str())
            .map(String::as_str)
            .or(message.parent_uuid.as_deref());
        current = match parent {
            Some(parent_uuid) => loaded
                .by_uuid
                .get(parent_uuid)
                .filter(|parent| !seen.contains(&parent.uuid))
                .or_else(|| timestamp_parent_fallback(loaded, message, &seen)),
            None => None,
        };
    }
    (model, profile)
}

fn valid_provider_profile(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
    })
}

/// Cheap first-line hide for the resume picker. Matches the first-message
/// filters (sidechain / teamName / daemon / SDK entrypoint) when those flags
/// are already present on line 1. Loop-session detection still needs the
/// full parse (first genuine user prompt, not necessarily line 1).
fn first_line_hides_session(line1: &str, current_is_sdk_entrypoint: bool) -> bool {
    if line1.contains("\"isSidechain\":true") || line1.contains("\"isSidechain\": true") {
        return true;
    }
    if extract_json_string_field(line1, "teamName").is_some_and(|s| !s.is_empty()) {
        return true;
    }
    if let Some("daemon" | "daemon-worker") =
        extract_json_string_field(line1, SESSION_KIND_KEY).as_deref()
    {
        return true;
    }
    if !current_is_sdk_entrypoint
        && matches!(
            extract_json_string_field(line1, "entrypoint").as_deref(),
            Some("sdk-cli" | "sdk-ts" | "sdk-py")
        )
    {
        return true;
    }
    false
}

/// Count the VISIBLE messages in a chain — port of `countVisibleMessages`
/// (`sessionStorage.ts:2453-2477`). Only `user`/`assistant` lines with visible
/// content count; `system`/`attachment`/`summary` lines, `isMeta` user lines,
/// `tool_result`-only user lines, and `tool_use`/`thinking`-only assistant lines
/// are excluded.
fn count_visible_messages(messages: &[JsonlMessage]) -> usize {
    messages.iter().filter(|m| is_visible_message(m)).count()
}

fn is_visible_message(m: &JsonlMessage) -> bool {
    let content = m.message.at_pointer("/content");
    match m.message_type.as_str() {
        "user" => {
            // Skip isMeta user lines.
            if m.extra
                .get_property("isMeta")
                .and_then(JsonValueView::as_bool)
                .unwrap_or(false)
            {
                return false;
            }
            has_visible_content(content, /* assistant = */ false)
        }
        "assistant" => has_visible_content(content, /* assistant = */ true),
        // system / attachment / summary / progress → never counted.
        _ => false,
    }
}

/// Whether a message's `content` carries user-visible output. Ports the two
/// claude-code predicates exactly:
///
/// - USER (`JZm`): a non-empty string, OR an array containing any block whose
///   `type` is `text`/`image`/`document` — **by type alone** (a `text` block
///   counts even when its text is empty/whitespace).
/// - ASSISTANT (`XZm`): the content MUST be an array (a bare string is never
///   visible), and that array must contain a `text` block with non-empty
///   trimmed text. `image`/`document` blocks do NOT make an assistant message
///   visible.
///
/// A `tool_result`-only user message and a `tool_use`/`thinking`-only assistant
/// message are NOT visible under either predicate.
fn has_visible_content(content: Option<JsonValueView<'_>>, assistant: bool) -> bool {
    let Some(content) = content else { return false };
    if let Some(s) = content.as_str() {
        // `XZm` requires an array, so a string content is never visible for an
        // assistant message; `JZm` treats a non-empty string as visible.
        return !assistant && !s.trim().is_empty();
    }
    let Some(arr) = content.array_len() else {
        return false;
    };
    (0..arr).any(|index| {
        let Some(block) = content.get_index(index) else {
            return false;
        };
        match block.get_property("type").and_then(JsonValueView::as_str) {
            Some("text") => {
                if assistant {
                    // `XZm`: assistant text block is visible only when non-empty.
                    block
                        .get_property("text")
                        .and_then(JsonValueView::as_str)
                        .is_some_and(|t| !t.trim().is_empty())
                } else {
                    // `JZm`: user text block is visible by TYPE ALONE.
                    true
                }
            }
            // `JZm`: images/documents make a USER message visible; `XZm`
            // (assistant) counts only text blocks.
            Some("image" | "document") => !assistant,
            _ => false,
        }
    })
}

/// Resolve the project dir for `cwd` and return up to `limit` most-recently-modified
/// `.jsonl` files as [`SessionMetadata`] rows, sorted by clamped logical
/// activity time desc (created/birthtime desc on tie).
///
/// Errors:
/// - [`LoaderError::EmptyDirectory`] if the project dir doesn't exist OR contains no `.jsonl`.
/// - [`LoaderError::Io`] on any other I/O failure.
///
/// Each row's `title` is resolved by [`collect_dir`] with claude-code's display
/// precedence — `custom-title` > `ai-title` > `summary` (at the chain tip's
/// `leafUuid`) > first-user-message ([`crate::jsonl::title::extract_title`]) —
/// from the **full** routed JSONL content. A cold scan opens and parses every
/// surviving candidate before sorting (logical chain timestamps are not
/// recoverable from file mtime alone); the bounded revision cache below makes
/// subsequent picker refreshes parse only files whose length or timestamps
/// changed.
///
/// Sub-agent / sidechain transcripts are HIDDEN (SESSION.1): a session is dropped
/// when its first parsed line is an `isSidechain` message or carries a truthy
/// `teamName` field, matching claude-code's `parseSessionInfoFromLite`
/// (listSessionsImpl.ts:88-95), `enrichLog` (sessionStorage.ts:5055-5067), and
/// `filterResumableSessions` (resume picker).
///
/// Locked against `claude-code/src/utils/sessionStorage.ts::loadSameRepoMessageLogs` — except:
/// - claude-code uses a 16-KiB head-only `enrichLogs` scan for the first user message; we
///   open + fully-parse because our `JsonlReader::read_all` is already in hand from M5-07.
///   (The sidechain/teamName decision still reads ONLY the first line, per TS.)
///
/// The default scope is the current directory and its direct session links.
/// Hosts explicitly choose [`SessionDiscoveryScope::Repository`] when the user
/// requests sibling worktrees, or [`SessionDiscoveryScope::All`] for all projects.
pub async fn list_recent_sessions(
    lingxi_home: &Path,
    cwd: &str,
    limit: usize,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<SessionMetadata>, LoaderError> {
    list_recent_sessions_in_scope(lingxi_home, cwd, SessionDiscoveryScope::Current, limit, fs).await
}

/// Enumerate resumable sessions in an explicit picker scope. Repository's
/// count applies per alias/orphan bucket, without capping the repository union.
/// Current count zero is unbounded unless direct aliases trigger a merged
/// final slice, matching the native selector's count truthiness rules.
pub async fn list_recent_sessions_in_scope(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    limit: usize,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<SessionMetadata>, LoaderError> {
    Ok(
        list_recent_sessions_in_scope_with_diagnostics(lingxi_home, cwd, scope, limit, fs)
            .await?
            .sessions,
    )
}

/// Filter already-loaded rows by session title — port of
/// `searchSessionsByCustomTitle` (`OEe`, 2.1.220 @237903684):
///
/// ```js
/// let a = e.toLowerCase().trim(),
///     l = s.filter((d) => {
///       let p = (d.customTitle ?? d.aiTitle)?.toLowerCase().trim();
///       if (!p) return !1;
///       return n ? p === a : p.includes(a)
///     });
/// // …dedup by session id keeping the newest `modified`, sort modified DESC, slice(limit)
/// ```
///
/// Both sides are lowercased AND trimmed before comparing, so a query with
/// stray whitespace or different casing still matches. Rows with neither title
/// kind are skipped rather than treated as an empty string — otherwise an empty
/// query would "match" every untitled session.
///
/// `exact` selects equality over substring. `--resume` passes `true`; the
/// `/resume` argument-completer passes `false` with a limit of 10.
///
/// The oracle's dedup-by-session-id step is a no-op here and is intentionally
/// not reproduced: it exists because its `logs` can hold several entries per
/// session, whereas [`collect_dir`] emits exactly one row per transcript FILE,
/// keyed by the uuid filename stem. Sorting is still applied — callers depend
/// on newest-first for the disambiguation listing.
#[must_use]
pub fn search_sessions_by_custom_title(
    sessions: Vec<SessionMetadata>,
    query: &str,
    exact: bool,
    limit: Option<usize>,
) -> Vec<SessionMetadata> {
    let needle = query.to_lowercase();
    let needle = needle.trim();
    let mut matched: Vec<SessionMetadata> = sessions
        .into_iter()
        .filter(|row| {
            let Some(title) = row.custom_or_ai_title.as_deref() else {
                return false;
            };
            let title = title.to_lowercase();
            let title = title.trim();
            if title.is_empty() {
                return false;
            }
            if exact {
                title == needle
            } else {
                title.contains(needle)
            }
        })
        .collect();
    matched.sort_by(|a, b| b.modified.cmp(&a.modified));
    if let Some(limit) = limit {
        matched.truncate(limit);
    }
    matched
}

/// Enumerate resumable sessions while retaining a count of corrupt or
/// unreadable candidate files that were skipped. Readable sessions are never
/// discarded because a sibling transcript is damaged. If every candidate is
/// damaged, the function returns [`LoaderError::Io`] so callers can surface a
/// recoverable catalog error instead of a misleading empty state.
pub async fn list_recent_sessions_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    limit: usize,
    fs: Arc<dyn FileSystem>,
) -> Result<SessionCatalog, LoaderError> {
    list_recent_sessions_in_scope_with_diagnostics(
        lingxi_home,
        cwd,
        SessionDiscoveryScope::Current,
        limit,
        fs,
    )
    .await
}

/// [`list_recent_sessions_in_scope`] with unreadable-file diagnostics.
pub async fn list_recent_sessions_in_scope_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    limit: usize,
    fs: Arc<dyn FileSystem>,
) -> Result<SessionCatalog, LoaderError> {
    let worktree_paths = if scope == SessionDiscoveryScope::Repository {
        git_worktree_paths(cwd)
    } else {
        Vec::new()
    };
    list_recent_sessions_in_scope_with_worktrees(
        lingxi_home,
        cwd,
        scope,
        limit,
        fs,
        &worktree_paths,
    )
    .await
}

/// Enumerate using worktree roots resolved in the host's execution environment.
/// Mobile supplies Linux guest roots instead of running a native Git process.
pub async fn list_recent_sessions_in_scope_with_worktrees(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    limit: usize,
    fs: Arc<dyn FileSystem>,
    worktree_paths: &[String],
) -> Result<SessionCatalog, LoaderError> {
    list_recent_sessions_inner_with_diagnostics(
        lingxi_home,
        cwd,
        scope,
        limit,
        &fs,
        worktree_paths,
        None,
    )
    .await
}

/// [`list_recent_sessions_with_diagnostics`] narrowed to one session: the
/// full catalog's answer with every other row removed.
///
/// Candidates are collected exactly as for the current-directory catalog (same
/// direct aliases and filters), but only this session's
/// transcript is parsed. A host that needs one row before opening a session
/// therefore no longer waits for every transcript in the project, which took
/// most of a second once one of them reached tens of megabytes. Only when the
/// session has no row does the full catalog run: whether that is an empty
/// answer or an error depends on the other transcripts. `skipped_files`
/// counts the files this lookup itself skipped.
pub async fn find_session_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<SessionCatalog, LoaderError> {
    find_session_in_scope_with_diagnostics(
        lingxi_home,
        cwd,
        SessionDiscoveryScope::Current,
        session_id,
        fs,
    )
    .await
}

/// Find one catalog row using the same explicit directory scope as a picker.
pub async fn find_session_in_scope_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<SessionCatalog, LoaderError> {
    let worktree_paths = if scope == SessionDiscoveryScope::Repository {
        git_worktree_paths(cwd)
    } else {
        Vec::new()
    };
    find_session_inner(lingxi_home, cwd, scope, session_id, &fs, &worktree_paths).await
}

/// Resolve an exact UUID once, then enrich only that selected transcript. This
/// keeps a launch hint and its source path on the same discovery decision.
pub async fn find_session_exact_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<SessionCatalog, LoaderError> {
    find_session_exact_raw_with_diagnostics(lingxi_home, cwd, &session_id.to_string(), fs).await
}

/// Exact catalog lookup that preserves the raw UUID filename's case.
pub async fn find_session_exact_raw_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    raw_session_id: &str,
    fs: Arc<dyn FileSystem>,
) -> Result<SessionCatalog, LoaderError> {
    let path = resolve_session_path_raw(lingxi_home, cwd, raw_session_id).await?;
    let session_id = Uuid::parse_str(raw_session_id).map_err(|_| LoaderError::SessionNotFound {
        arg: raw_session_id.to_string(),
    })?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|source| LoaderError::Io {
            arg: path.display().to_string(),
            source,
        })?;
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let candidate = CatalogCandidate {
        uuid: session_id,
        stem: raw_session_id.to_string(),
        path: path.clone(),
        file_len: metadata.len(),
        file_modified: modified,
        file_created: metadata.created().unwrap_or(modified),
    };
    let current_is_sdk_entrypoint = matches!(
        std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Ok("sdk-cli" | "sdk-ts" | "sdk-py")
    );
    let mut skipped_files = 0;
    let sessions = enrich_candidates(
        vec![candidate],
        &fs,
        current_is_sdk_entrypoint,
        &[],
        &[],
        &mut skipped_files,
    )
    .await?;
    if sessions.is_empty() && skipped_files > 0 {
        return Err(LoaderError::Io {
            arg: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session transcript is unreadable",
            ),
        });
    }
    Ok(SessionCatalog {
        sessions,
        skipped_files,
    })
}

async fn find_session_inner(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    session_id: Uuid,
    fs: &Arc<dyn FileSystem>,
    worktree_paths: &[String],
) -> Result<SessionCatalog, LoaderError> {
    if let Ok(found) = list_recent_sessions_inner_with_diagnostics(
        lingxi_home,
        cwd,
        scope,
        1,
        fs,
        worktree_paths,
        Some(session_id),
    )
    .await
    {
        // Its row exists, so the full catalog succeeds and lists this row.
        return Ok(found);
    }
    let mut catalog = list_recent_sessions_inner_with_diagnostics(
        lingxi_home,
        cwd,
        scope,
        usize::MAX,
        fs,
        worktree_paths,
        None,
    )
    .await?;
    catalog.sessions.retain(|row| row.uuid == session_id);
    Ok(catalog)
}

/// Explicit-scope core with injectable worktree paths for deterministic tests.
#[cfg(test)]
async fn list_recent_sessions_inner(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    limit: usize,
    fs: &Arc<dyn FileSystem>,
    worktree_paths: &[String],
) -> Result<Vec<SessionMetadata>, LoaderError> {
    Ok(list_recent_sessions_inner_with_diagnostics(
        lingxi_home,
        cwd,
        scope,
        limit,
        fs,
        worktree_paths,
        None,
    )
    .await?
    .sessions)
}

async fn list_recent_sessions_inner_with_diagnostics(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    limit: usize,
    fs: &Arc<dyn FileSystem>,
    worktree_paths: &[String],
    only_session: Option<Uuid>,
) -> Result<SessionCatalog, LoaderError> {
    let mut candidates: Vec<CatalogCandidate> = Vec::new();
    let mut skipped_files = 0;
    let DiscoveryDirectories {
        project_dirs,
        target_cwds,
        unfiltered_dirs,
        limited_dirs,
        has_aliases,
    } = discover_session_directories(lingxi_home, cwd, scope, worktree_paths, limit).await?;
    for project_dir in project_dirs {
        let collected = collect_dir(
            &project_dir,
            &target_cwds,
            fs,
            &mut candidates,
            &mut skipped_files,
        )
        .await;
        if let Err(error) = collected {
            if project_dir == project_dir_for_cwd(lingxi_home, cwd) {
                return Err(error);
            }
            // Direct aliases are best-effort storage references. A removed,
            // inaccessible, or malformed linked path must not poison the
            // current directory's readable catalog (native RDt returns []).
            tracing::debug!(path = %project_dir.display(), %error, "linked session directory unavailable");
        }
    }

    if let Some(session_id) = only_session {
        candidates.retain(|candidate| candidate.uuid == session_id);
    }
    let candidates = deduplicate_catalog_candidates(candidates);
    let current_is_sdk_entrypoint = matches!(
        std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Ok("sdk-cli" | "sdk-ts" | "sdk-py")
    );
    let mut rows = enrich_candidates(
        candidates,
        fs,
        current_is_sdk_entrypoint,
        &target_cwds,
        &unfiltered_dirs,
        &mut skipped_files,
    )
    .await?;
    // Native Repository keeps the current/worktree buckets unbounded. Its
    // count applies separately to direct aliases and retained worktree
    // buckets; Current performs its final slice only after a linked merge.
    if scope == SessionDiscoveryScope::Repository && limit > 0 {
        let mut kept = Vec::with_capacity(rows.len());
        for directory in limited_dirs {
            let mut bucket = rows
                .iter()
                .filter(|row| row.path.parent() == Some(directory.as_path()))
                .cloned()
                .collect::<Vec<_>>();
            bucket.sort();
            kept.extend(bucket.into_iter().take(limit).map(|row| row.path));
            rows.retain(|row| {
                row.path.parent() != Some(directory.as_path()) || kept.contains(&row.path)
            });
        }
    }

    if rows.is_empty() {
        if skipped_files > 0 {
            return Err(LoaderError::Io {
                arg: project_dir_for_cwd(lingxi_home, cwd).display().to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session catalog contains no recoverable transcripts",
                ),
            });
        }
        return Err(LoaderError::EmptyDirectory);
    }

    rows.sort();
    if scope == SessionDiscoveryScope::All
        || (scope == SessionDiscoveryScope::Current && (limit > 0 || has_aliases))
    {
        rows.truncate(limit);
    }
    Ok(SessionCatalog {
        sessions: rows,
        skipped_files,
    })
}

/// Load a session by UUID and return the MAIN-THREAD conversation chain
/// (root → tip, in walk order) reconstructed by a tolerant, branch-aware DAG
/// walk over the transcript's `parentUuid` graph.
///
/// REAL-TRANSCRIPT TOLERANCE (replaces the old strict linear-chain check): a
/// genuine `claude-code` `.jsonl` is a DAG, not a file-ordered linked list —
/// it interleaves metadata lines, may carry a LEADING `summary` line, splices
/// `attachment`/`system` entries between turns, and can contain forked /
/// sidechain branches with their own leaves and (for forks) a different root
/// `sessionId`. The old `validate_chain` (msg[0].parent==None; strict
/// file-order parent links; all `session_id` equal) rejected all of these. We
/// now mirror `claude-code`'s `loadMessagesFromJsonlPath`
/// (`conversationRecovery.ts:416`): route the file
/// ([`JsonlReader::read_routed`]), pick the newest non-sidechain
/// user/assistant leaf as the tip, and walk `tip → root` via `parentUuid`
/// ([`build_conversation_chain`]). The leaf supplies the session id, so forked
/// sessions (whose root row keeps the SOURCE session's id) load cleanly.
///
/// Returns the main thread only; sidechain branches are ignored. The
/// `parentUuid` walk is cycle-guarded and uses Native's same-sidechain,
/// nearest-preceding-row fallback for a missing/repeated parent within 5 s;
/// it does not error or rewrite persisted links.
///
/// Errors:
/// - [`LoaderError::SessionNotFound`] if the file doesn't exist.
/// - [`LoaderError::EmptyDirectory`] if the file has NO chain-participant
///   lines at all (nothing resumable) — surfaced via the same "nothing to
///   resume" channel callers already handle.
/// - [`LoaderError::Io`] on disk read failure.
///
/// The strict `ChainBroken` / `SessionIdMismatch` variants are RETAINED on
/// [`LoaderError`] (other code matches their `Display`) but are no longer
/// produced from this load path; structural anomalies are downgraded to a
/// `tracing::warn` + a best-effort partial chain.
pub async fn load_session(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let (_, _, loaded) = load_routed_session_with_source(lingxi_home, cwd, session_id, fs).await?;
    let (chain, _) = build_conversation_chain(&loaded, &session_id.to_string());
    if chain.is_empty() {
        return Err(LoaderError::EmptyDirectory);
    }
    Ok(chain)
}

/// Load every transcript entry in file order, including attachment records
/// that are intentionally omitted from the resumable main-message chain.
/// Consumers that implement persistence protocols (for example deferred-hook
/// replay) need these records even when the current conversation tip is an
/// assistant line whose child attachment is not itself a leaf.
pub async fn load_session_entries(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let (_, _, loaded) = load_routed_session_with_source(lingxi_home, cwd, session_id, fs).await?;
    if loaded.messages_in_order.is_empty() {
        return Err(LoaderError::EmptyDirectory);
    }
    Ok(loaded.messages_in_order)
}

async fn load_session_entries_from_path(
    path: PathBuf,
    arg: String,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let loaded = read_routed_session(path, arg, fs).await?;
    if loaded.messages_in_order.is_empty() {
        return Err(LoaderError::EmptyDirectory);
    }
    Ok(loaded.messages_in_order)
}

/// Route one session transcript: [`LoaderError::SessionNotFound`] when the
/// file is missing, [`LoaderError::Io`] when it cannot be read.
async fn read_routed_session(
    path: PathBuf,
    arg: String,
    fs: Arc<dyn FileSystem>,
) -> Result<LoadedTranscript, LoaderError> {
    match tokio::fs::metadata(&path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LoaderError::SessionNotFound { arg });
        }
        Err(source) => {
            return Err(LoaderError::Io {
                arg: path.display().to_string(),
                source,
            });
        }
    }
    let reader = JsonlReader::new(path, fs);
    let loaded = reader.read_routed().await.map_err(|e| LoaderError::Io {
        arg: arg.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
    })?;
    Ok(loaded)
}

/// Load a UUID using the legacy repository union's logical-activity selection.
///
/// Duplicate transcripts are compared after clamping their message timestamps
/// to file mtime. Native picker rows instead select by physical mtime before
/// enrichment; open their exact source with
/// [`load_routed_session_from_path_with_source`] rather than resolving again.
/// When no scoped copy exists, only one usable global transcript can be selected.
pub async fn load_session_across_worktrees(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let path = resolve_session_path_across_worktrees(lingxi_home, cwd, session_id).await?;
    load_session_from_path(path, session_id.to_string(), fs).await
}

/// Load every routed transcript entry for a session selected from the same
/// worktree-aware corpus as [`load_session_across_worktrees`]. Unlike the
/// resumable chain, this includes attachment records used by resume protocols
/// such as deferred-hook replay.
pub async fn load_session_entries_across_worktrees(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let path = resolve_session_path_across_worktrees(lingxi_home, cwd, session_id).await?;
    load_session_entries_from_path(path, session_id.to_string(), fs).await
}

/// Route the transcript the legacy logical-activity union selects, once.
///
/// [`load_session_across_worktrees`], [`load_session_entries_across_worktrees`]
/// and [`read_agent_resume_state`] each select and read that file again; a
/// resume that needs all three derives them from this one read, which also
/// keeps them on one revision of the file. A missing file is
/// [`LoaderError::SessionNotFound`]; callers apply their own emptiness check.
pub async fn load_routed_session_across_worktrees(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<LoadedTranscript, LoaderError> {
    Ok(
        load_routed_session_across_worktrees_with_path(lingxi_home, cwd, session_id, fs)
            .await?
            .1,
    )
}

/// Resolve once and return both the selected source path and its routed
/// transcript, so a resume cannot lose the authoritative storage directory.
pub async fn load_routed_session_across_worktrees_with_path(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<(PathBuf, LoadedTranscript), LoaderError> {
    let path = resolve_session_path_across_worktrees(lingxi_home, cwd, session_id).await?;
    let loaded = read_routed_session(path.clone(), session_id.to_string(), fs).await?;
    Ok((path, loaded))
}

/// CLI flag UUID resume with its authoritative source path, preferring a usable
/// current-directory copy. Picker rows use their selected source path through
/// [`load_routed_session_from_path_with_source`].
pub async fn load_routed_session_with_path(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<(PathBuf, LoadedTranscript), LoaderError> {
    let (path, _, loaded) =
        load_routed_session_with_source(lingxi_home, cwd, session_id, fs).await?;
    Ok((path, loaded))
}

/// Capture raw source and routed state from one filesystem read. Resume callers
/// use the same snapshot for history, metadata hydration and source identity.
pub async fn load_routed_session_with_source(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<(PathBuf, String, LoadedTranscript), LoaderError> {
    let raw_id = session_id.to_string();
    let roots = git_worktree_paths(cwd);
    resolve_cli_transcript_source(lingxi_home, cwd, &raw_id, Some(&fs), &roots)
        .await?
        .ok_or(LoaderError::SessionNotFound { arg: raw_id })
}

/// Open the source selected by a picker without resolving its UUID again.
/// Filename identity is checked before the single source read; history and
/// metadata are then routed from the same captured revision. Fork ancestors
/// may retain a different sessionId, as in native full transcript recovery.
pub async fn load_routed_session_from_path_with_source(
    path: &Path,
    expected_id: Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<(PathBuf, String, LoadedTranscript), LoaderError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| crate::jsonl::validate_uuid(stem));
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
        || stem.and_then(|stem| Uuid::parse_str(stem).ok()) != Some(expected_id)
    {
        return Err(LoaderError::SessionNotFound {
            arg: expected_id.to_string(),
        });
    }
    let content = fs
        .read_file(path.to_str().expect("UTF-8 transcript path"), None, None)
        .await
        .map_err(|error| LoaderError::Io {
            arg: path.display().to_string(),
            source: std::io::Error::other(error.to_string()),
        })?
        .content;
    let loaded = route_lines(&content);
    if loaded.messages_in_order.is_empty()
        && !stem.is_some_and(|stem| loaded.mobile_empty_sessions.contains(stem))
    {
        return Err(LoaderError::EmptyDirectory);
    }
    Ok((path.to_path_buf(), content, loaded))
}

/// Resolve CLI `--resume UUID` through native FIe: current full transcript,
/// repository full transcripts, then the unique global raw-message candidate.
/// Picker aliases are selected by scoped catalog rows and source-path loading.
pub async fn resolve_session_path(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
) -> Result<PathBuf, LoaderError> {
    resolve_session_path_raw(lingxi_home, cwd, &session_id.to_string()).await
}

/// Raw CLI UUID lookup preserves filename case. This is FIe's ordinary resume
/// selector, not resolveJobTranscript's raw-message probe or a picker lookup.
pub async fn resolve_session_path_raw(
    lingxi_home: &Path,
    cwd: &str,
    session_id: &str,
) -> Result<PathBuf, LoaderError> {
    let roots = git_worktree_paths(cwd);
    resolve_cli_transcript_source(lingxi_home, cwd, session_id, None, &roots)
        .await?
        .map(|(path, _, _)| path)
        .ok_or_else(|| LoaderError::SessionNotFound {
            arg: session_id.to_string(),
        })
}

fn cli_worktree_order(cwd: &str, roots: &[String]) -> Vec<String> {
    let containing = roots
        .iter()
        .find(|root| cwd == root.as_str() || cwd.starts_with(&format!("{root}/")));
    let mut others = roots
        .iter()
        .filter(|root| Some(*root) != containing)
        .cloned()
        .collect::<Vec<_>>();
    let collator = cli_worktree_collator();
    others.sort_by(|left, right| collator.compare(left, right));
    if let Some(root) = containing {
        others.insert(0, root.clone());
    }
    others
}

fn cli_worktree_collator() -> icu_collator::CollatorBorrowed<'static> {
    use icu_collator::options::{AlternateHandling, CollatorOptions, Strength};
    use icu_collator::preferences::{CollationCaseFirst, CollationNumericOrdering};
    let locale = cli_default_locale();
    let locale = icu_locale::Locale::try_from_str(&locale).unwrap_or_else(|_| {
        icu_locale::Locale::try_from_str("en-US").expect("compiled ICU data includes en-US")
    });
    let mut preferences = icu_collator::CollatorPreferences::from(&locale);
    preferences.case_first = Some(CollationCaseFirst::False);
    preferences.numeric_ordering = Some(CollationNumericOrdering::False);
    let mut options = CollatorOptions::default();
    options.strength = Some(Strength::Tertiary);
    options.alternate_handling = Some(AlternateHandling::NonIgnorable);
    icu_collator::Collator::try_new(preferences, options)
        .expect("compiled ICU data provides the native default collation profile")
}

fn cli_default_locale() -> String {
    #[cfg(any(target_vendor = "apple", target_os = "windows", target_os = "android"))]
    let locale = sys_locale::get_locale();
    #[cfg(all(unix, not(any(target_vendor = "apple", target_os = "android"))))]
    let locale = lingxi_core::native_locale::current_lc_ctype();
    #[cfg(not(any(unix, target_os = "windows", target_os = "android")))]
    let locale: Option<String> = None;
    let locale = locale.as_deref().unwrap_or("en-US");
    let locale = locale
        .split('.')
        .next()
        .unwrap_or(locale)
        .split('@')
        .next()
        .unwrap_or(locale);
    if locale.is_empty() || locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX")
    {
        "en-US".to_string()
    } else {
        locale.replace('_', "-")
    }
}

type CapturedCliTranscript = (PathBuf, String, LoadedTranscript);

async fn read_cli_transcript_source(
    path: PathBuf,
    session_id: &str,
    fs: Option<&Arc<dyn FileSystem>>,
) -> Result<Option<CapturedCliTranscript>, LoaderError> {
    // Host adapters may wrap ENOENT in their generic I/O variant. Native's
    // local reader still treats an absent computed _co path as a null result,
    // allowing repository/global fallback instead of throwing that wrapper.
    if let Err(error) = tokio::fs::metadata(&path).await {
        if error.kind() == std::io::ErrorKind::NotFound
            || error.raw_os_error() == Some(libc::ENOTDIR)
        {
            return Ok(None);
        }
    }
    let content = if let Some(fs) = fs {
        match fs
            .read_file(path.to_str().expect("UTF-8 transcript path"), None, None)
            .await
        {
            Ok(read) => read.content,
            Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(None),
            Err(error) => {
                return Err(LoaderError::Io {
                    arg: path.display().to_string(),
                    source: std::io::Error::other(error.to_string()),
                });
            }
        }
    } else {
        match tokio::fs::read(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ENOTDIR) =>
            {
                return Ok(None);
            }
            Err(source) => {
                return Err(LoaderError::Io {
                    arg: path.display().to_string(),
                    source,
                });
            }
        }
    };
    let loaded = route_lines(&content);
    // Mui first requires at least one parsed message; a cleared transcript
    // can then represent an empty continuation. Local selection uses the full
    // parser, so spaces around a JSON type colon remain valid here.
    if loaded.by_uuid.is_empty()
        || (!loaded.cleared_to_empty && find_tip(&loaded, session_id).is_none())
    {
        return Ok(None);
    }
    Ok(Some((path, content, loaded)))
}

async fn resolve_cli_transcript_source(
    lingxi_home: &Path,
    cwd: &str,
    session_id: &str,
    fs: Option<&Arc<dyn FileSystem>>,
    worktree_paths: &[String],
) -> Result<Option<CapturedCliTranscript>, LoaderError> {
    if !crate::jsonl::validate_uuid(session_id) {
        return Ok(None);
    }
    let filename = format!("{session_id}.jsonl");
    // Native _co returns the first rRe path whose stat succeeds, before the
    // full reader determines whether that chosen transcript is resumable.
    let mut current = project_dir_for_cwd(lingxi_home, cwd).join(&filename);
    for directory in project_directories_for_cwd(lingxi_home, cwd).await {
        let candidate = directory.join(&filename);
        if tokio::fs::metadata(&candidate).await.is_ok() {
            current = candidate;
            break;
        }
    }
    if let Some(captured) = read_cli_transcript_source(current, session_id, fs).await? {
        return Ok(Some(captured));
    }
    // Gfe puts the first containing registered root first, with all remaining
    // roots sorted by the native localeCompare collation profile. iQo includes
    // the current root again and full-loads each rRe directory in that order.
    for root in cli_worktree_order(cwd, worktree_paths) {
        for directory in project_directories_for_cwd(lingxi_home, &root).await {
            if let Some(captured) =
                read_cli_transcript_source(directory.join(&filename), session_id, fs).await?
            {
                return Ok(Some(captured));
            }
        }
    }
    // fQo calls f3r without an exclusion set: even an unparseable local file
    // with a raw message marker participates in global ambiguity detection.
    let mut directories = Vec::new();
    if append_project_dirs_matching(&lingxi_home.join("projects"), &mut directories, |_| true)
        .await
        .is_err()
    {
        return Ok(None);
    }
    let mut selected = None;
    for directory in directories {
        let candidate = directory.join(&filename);
        if message_presence(&candidate).await != MessagePresence::Has {
            continue;
        }
        if selected.is_some() {
            return Ok(None);
        }
        selected = Some(candidate);
    }
    if let Some(path) = selected {
        return read_cli_transcript_source(path, session_id, fs).await;
    }
    Ok(None)
}

async fn message_presence(path: &Path) -> MessagePresence {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if !metadata.is_file() => return MessagePresence::None,
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return MessagePresence::None,
        Err(_) => return MessagePresence::Unknown,
    }
    let Ok(file) = tokio::fs::File::open(path).await else {
        return MessagePresence::Unknown;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => return MessagePresence::None,
            Err(_) => return MessagePresence::Unknown,
            Ok(_) => {}
        }
        if [
            b"\"type\":\"user\"".as_slice(),
            b"\"type\":\"assistant\"".as_slice(),
        ]
        .iter()
        .any(|marker| line.windows(marker.len()).any(|window| window == *marker))
        {
            return MessagePresence::Has;
        }
    }
}

/// Resolve a raw UUID without lowercasing its filename. The returned false
/// result preserves native Iue's first scoped candidate, including when global
/// copies are ambiguous or the only local transcript has metadata but no user
/// or assistant marker. `cross_worktree: false` also disables global fallback.
pub async fn resolve_job_transcript_raw(
    lingxi_home: &Path,
    cwd: &str,
    session_id: &str,
    options: &JobTranscriptResolveOptions,
) -> Result<ResolvedJobTranscriptPath, LoaderError> {
    let canonical_cwd = canonicalized_path_string(cwd);
    let worktrees = if options.cross_worktree {
        git_worktree_paths(&canonical_cwd)
    } else {
        Vec::new()
    };
    resolve_job_transcript_raw_inner(lingxi_home, &canonical_cwd, session_id, options, &worktrees)
        .await
}

async fn resolve_job_transcript_raw_inner(
    lingxi_home: &Path,
    cwd: &str,
    session_id: &str,
    options: &JobTranscriptResolveOptions,
    worktree_paths: &[String],
) -> Result<ResolvedJobTranscriptPath, LoaderError> {
    let project = project_dir_for_cwd(lingxi_home, cwd);
    if !crate::jsonl::validate_uuid(session_id) {
        return Ok(ResolvedJobTranscriptPath {
            path: project.join("invalid-resume-id.jsonl"),
            has_messages: false,
        });
    }
    let filename = format!("{session_id}.jsonl");
    let computed = project.join(&filename);
    let mut candidates = Vec::new();
    if let Some(path) = &options.link_scan_path {
        candidates.push(if path.to_string_lossy().ends_with(&filename) {
            path.clone()
        } else {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&filename)
        });
    }
    for directory in project_directories_for_cwd(lingxi_home, cwd).await {
        candidates.push(directory.join(&filename));
    }
    candidates.push(computed);
    let fallback = candidates[0].clone();
    let mut seen = HashSet::new();
    for candidate in candidates {
        if seen.insert(candidate.clone())
            && message_presence(&candidate).await != MessagePresence::None
        {
            return Ok(ResolvedJobTranscriptPath {
                path: candidate,
                has_messages: true,
            });
        }
    }
    if options.cross_worktree {
        for root in worktree_paths.iter().filter(|root| root.as_str() != cwd) {
            for directory in project_directories_for_cwd(lingxi_home, root).await {
                let candidate = directory.join(&filename);
                if seen.insert(candidate.clone())
                    && message_presence(&candidate).await != MessagePresence::None
                {
                    return Ok(ResolvedJobTranscriptPath {
                        path: candidate,
                        has_messages: true,
                    });
                }
            }
        }
        let mut directories = Vec::new();
        if append_project_dirs_matching(&lingxi_home.join("projects"), &mut directories, |_| true)
            .await
            .is_err()
        {
            return Ok(ResolvedJobTranscriptPath {
                path: fallback,
                has_messages: false,
            });
        }
        let mut global = None;
        for directory in directories {
            let candidate = directory.join(&filename);
            if seen.contains(&candidate)
                || message_presence(&candidate).await != MessagePresence::Has
            {
                continue;
            }
            if global.is_some() {
                return Ok(ResolvedJobTranscriptPath {
                    path: fallback,
                    has_messages: false,
                });
            }
            global = Some(candidate);
        }
        if let Some(path) = global {
            return Ok(ResolvedJobTranscriptPath {
                path,
                has_messages: true,
            });
        }
    }
    Ok(ResolvedJobTranscriptPath {
        path: fallback,
        has_messages: false,
    })
}

/// Resolve the source selected by the legacy logical-activity repository union.
/// Native picker rows retain their own physical-mtime selection and should be
/// opened directly with [`load_routed_session_from_path_with_source`].
pub async fn resolve_session_path_across_worktrees(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
) -> Result<PathBuf, LoaderError> {
    match resolve_session_path_in_scope(
        lingxi_home,
        cwd,
        SessionDiscoveryScope::Repository,
        session_id,
    )
    .await
    {
        Err(LoaderError::SessionNotFound { .. }) => {}
        result => return result,
    }
    resolve_session_path_in_scope(lingxi_home, cwd, SessionDiscoveryScope::All, session_id).await
}

/// Resolve a UUID within an explicit directory scope using legacy selection.
/// Global scope requires a unique transcript with messages; repository scope
/// compares logical activity. This does not re-resolve a native picker row.
pub async fn resolve_session_path_in_scope(
    lingxi_home: &Path,
    cwd: &str,
    scope: SessionDiscoveryScope,
    session_id: Uuid,
) -> Result<PathBuf, LoaderError> {
    let worktree_paths = if scope == SessionDiscoveryScope::Repository {
        git_worktree_paths(cwd)
    } else {
        Vec::new()
    };
    let directories =
        discover_session_directories(lingxi_home, cwd, scope, &worktree_paths, usize::MAX).await?;
    select_session_path_from_directories(
        directories,
        session_id,
        scope == SessionDiscoveryScope::All,
    )
    .await
}

#[cfg(test)]
async fn load_session_across_worktrees_inner(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
    worktree_paths: &[String],
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let path =
        select_session_path_across_worktrees(lingxi_home, cwd, session_id, worktree_paths).await?;
    load_session_from_path(path, session_id.to_string(), fs).await
}

#[cfg(test)]
async fn load_session_entries_across_worktrees_inner(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    fs: Arc<dyn FileSystem>,
    worktree_paths: &[String],
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let path =
        select_session_path_across_worktrees(lingxi_home, cwd, session_id, worktree_paths).await?;
    load_session_entries_from_path(path, session_id.to_string(), fs).await
}

#[cfg(test)]
async fn select_session_path_across_worktrees(
    lingxi_home: &Path,
    cwd: &str,
    session_id: Uuid,
    worktree_paths: &[String],
) -> Result<PathBuf, LoaderError> {
    let directories = discover_session_directories(
        lingxi_home,
        cwd,
        SessionDiscoveryScope::Repository,
        worktree_paths,
        usize::MAX,
    )
    .await?;
    select_session_path_from_directories(directories, session_id, false).await
}

async fn select_session_path_from_directories(
    directories: DiscoveryDirectories,
    session_id: Uuid,
    require_unique_messages: bool,
) -> Result<PathBuf, LoaderError> {
    let arg = session_id.to_string();
    let mut selected: Option<(PathBuf, SystemTime)> = None;
    for directory in directories.project_dirs {
        let candidate = directory.join(format!("{arg}.jsonl"));
        let metadata = match tokio::fs::metadata(&candidate).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) if require_unique_messages => continue,
            // An inaccessible local candidate has Native's "unknown" presence
            // and must not silently select an unrelated global transcript.
            Err(_) => return Ok(candidate),
        };
        if !metadata.is_file() {
            continue;
        }
        // Native M performs a byte-substring probe, without JSON parsing. An
        // unreadable local candidate is "unknown" and remains selectable;
        // global fallback accepts only a positive message marker.
        let content = tokio::fs::read(&candidate).await.ok();
        let has_messages = content.as_ref().is_some_and(|bytes| {
            [
                b"\"type\":\"user\"".as_slice(),
                b"\"type\":\"assistant\"".as_slice(),
            ]
            .iter()
            .any(|marker| bytes.windows(marker.len()).any(|window| window == *marker))
        });
        if content.is_some() && !has_messages {
            continue;
        }
        if require_unique_messages && !has_messages {
            continue;
        }
        let loaded = content
            .as_ref()
            .map(|bytes| route_lines(&String::from_utf8_lossy(bytes)));
        let transcript_cwd = loaded
            .as_ref()
            .and_then(|loaded| transcript_cwd_from_path(&candidate, loaded));
        if !directories.unfiltered_dirs.contains(&directory)
            && !transcript_matches_target_dirs(transcript_cwd.as_deref(), &directories.target_cwds)
        {
            continue;
        }
        if require_unique_messages {
            if selected.is_some() {
                return Err(LoaderError::AmbiguousSession { arg });
            }
        }
        let file_modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let modified = loaded
            .as_ref()
            .and_then(|loaded| transcript_logical_modified(loaded, file_modified))
            .unwrap_or(file_modified);
        let replace = selected
            .as_ref()
            .is_none_or(|(_, selected_modified)| modified > *selected_modified);
        if replace {
            selected = Some((candidate, modified));
        }
    }

    selected
        .map(|(path, _)| path)
        .ok_or(LoaderError::SessionNotFound { arg })
}

async fn load_session_from_path(
    path: PathBuf,
    arg: String,
    fs: Arc<dyn FileSystem>,
) -> Result<Vec<JsonlMessage>, LoaderError> {
    let loaded = read_routed_session(path, arg.clone(), fs).await?;

    let (chain, _tip_session_id) = build_conversation_chain(&loaded, &arg);
    if chain.is_empty() {
        // No chain-participant lines at all → nothing to resume. (A file that is
        // pure metadata, or whose only messages are sidechains.) Surface the
        // same "nothing to resume" signal callers already expect.
        return Err(LoaderError::EmptyDirectory);
    }
    Ok(chain)
}

/// (P2-02 cc2.1.207) Read the persisted `agent-setting` for `session_id` from its
/// transcript, if present — the resume read side of claude's
/// `agentSettings.get(sessionId)` lookup that feeds `rVe`. Returns the stored
/// `agentSetting` string (the applied agent's `agentType`), or `None` when the
/// transcript is absent/unreadable, carries no `agent-setting` record for this
/// session, or the record's `agentSetting` is not a string.
///
/// `transcript_path` is the session's `<uuid>.jsonl` file; `session_id` MUST be
/// the bare uuid stem the writer keyed the record by (see
/// [`crate::jsonl::writer::JsonlWriter::append_agent_setting`]).
pub async fn read_agent_setting(
    transcript_path: &Path,
    fs: Arc<dyn FileSystem>,
    session_id: &str,
) -> Option<String> {
    read_agent_resume_state(transcript_path, fs, session_id)
        .await
        .0
}

/// Read the persisted main-thread agent type and immutable definition from one
/// routed transcript snapshot. Keeping the pair on one read prevents a
/// concurrent append from producing a type from one revision and a definition
/// from another.
///
/// 🔒 AG-5, verified at the 2.1.270 oracle 2026-09-14 — the reason that item is
/// NOT a gap here. 2.1.268 fixed a teammate respawn that could pick up an
/// UNTRUSTED agent file sharing the resumed agent's name, because upstream
/// re-resolves the definition by name at respawn time. This port never does:
/// the definition is written at spawn as an immutable `agentSnapshot`
/// (`JsonlWriter::append_agent_setting_snapshot`) carrying a SHA-256 over its
/// canonical JSON, and the read below returns it ONLY when the hash still
/// matches. A catalog entry edited, replaced or shadowed after the spawn
/// therefore cannot reach a resumed agent at all — a stronger guarantee than
/// the upstream fix, and the reason the missing `respawn_teammate` path is not
/// a hole. ⛔ Do not "simplify" this into a name lookup.
pub async fn read_agent_resume_state(
    transcript_path: &Path,
    fs: Arc<dyn FileSystem>,
    session_id: &str,
) -> (Option<String>, Option<Value>) {
    if !tokio::fs::try_exists(transcript_path)
        .await
        .unwrap_or(false)
    {
        return (None, None);
    }
    let Some(content) = read_transcript_content(transcript_path, fs).await else {
        return (None, None);
    };
    agent_resume_state_from_content(&content, session_id)
}

/// [`agent_resume_state_from_loaded`] over raw transcript text, routing only
/// the `agent-setting` rows that feed it (see [`route_rows_of_types`]).
#[must_use]
pub fn agent_resume_state_from_content(
    content: &str,
    session_id: &str,
) -> (Option<String>, Option<Value>) {
    agent_resume_state_from_loaded(
        &route_rows_of_types(content, &["agent-setting"]),
        session_id,
    )
}

/// Route only the rows that can be one of `types`.
///
/// Restore readers that consult one side map (agent settings, worktree
/// state) use this instead of a full [`route_lines`]: each of those rows is
/// routed independently of every other row, so the map comes out identical
/// while a long transcript's message rows are never parsed (see
/// [`lines_naming_types`] for why no row of these types is skipped).
#[must_use]
pub fn route_rows_of_types(content: &str, types: &[&str]) -> LoadedTranscript {
    route_line_seq(lines_naming_types(strip_transcript_bom(content), types))
}

/// The whole transcript text, as [`JsonlReader::read_routed`] reads it.
async fn read_transcript_content(
    transcript_path: &Path,
    fs: Arc<dyn FileSystem>,
) -> Option<String> {
    let path = transcript_path.to_str().expect("UTF-8 path");
    fs.read_file(path, None, None)
        .await
        .ok()
        .map(|read| read.content)
}

/// Read the same verified agent metadata from an already captured transcript.
/// Cold-resume hosts use this to keep history and startup configuration on one
/// writer-leased snapshot rather than reopening the source during assembly.
#[must_use]
pub fn agent_resume_state_from_loaded(
    loaded: &LoadedTranscript,
    session_id: &str,
) -> (Option<String>, Option<Value>) {
    use sha2::{Digest, Sha256};

    let setting = loaded
        .agent_settings
        .get(session_id)
        .and_then(Value::as_str)
        .map(str::to_string);
    let definition = loaded.agent_snapshots.get(session_id).and_then(|snapshot| {
        if snapshot.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
            return None;
        }
        let definition = snapshot.get("definition")?;
        let expected = snapshot.get("sha256").and_then(Value::as_str)?;
        let canonical = serde_json::to_vec(definition).ok()?;
        let actual = format!("{:x}", Sha256::digest(&canonical));
        (actual == expected).then(|| definition.clone())
    });
    (setting, definition)
}

/// Read the last immutable resolved-agent snapshot for a session. Returns
/// `None` for legacy transcripts and for malformed/unsupported snapshot
/// versions. Integrity is checked before the definition is returned.
pub async fn read_agent_snapshot(
    transcript_path: &Path,
    fs: Arc<dyn FileSystem>,
    session_id: &str,
) -> Option<Value> {
    read_agent_resume_state(transcript_path, fs, session_id)
        .await
        .1
}

/// (parity 2.1.212) Read the persisted `worktree-state` for `session_id` from
/// its transcript, if present — the resume read side of claude's
/// `worktreeStates.get(sessionId)` lookup that feeds `restoreWorktreeSession`
/// on `--continue`/`--resume`. Returns the stored `worktreeSession` object (an
/// active `EnterWorktree` session), or `None` when the transcript is
/// absent/unreadable, carries no `worktree-state` record for this session, or
/// the last such record cleared it (`worktreeSession: null`, the
/// `ExitWorktree` clear).
///
/// Because the loader keeps only the LAST `worktree-state` per `sessionId`
/// (last-write-wins), a session that entered then exited a worktree resolves to
/// `None` here — so `ExitWorktree` correctly stays a no-op on resume. This is
/// what makes `ExitWorktree` operate (instead of hitting "No-op: there is no
/// active EnterWorktree session to exit") only when a worktree is still active.
///
/// `transcript_path` is the session's `<uuid>.jsonl` file; `session_id` MUST be
/// the bare uuid stem the writer keyed the record by (see
/// [`crate::jsonl::writer::JsonlWriter::append_worktree_state`]).
pub async fn read_worktree_state(
    transcript_path: &Path,
    fs: Arc<dyn FileSystem>,
    session_id: &str,
) -> Option<Value> {
    if !tokio::fs::try_exists(transcript_path)
        .await
        .unwrap_or(false)
    {
        return None;
    }
    let content = read_transcript_content(transcript_path, fs).await?;
    worktree_state_from_content(&content, session_id)
}

/// [`read_worktree_state`] over raw transcript text, routing only the
/// `worktree-state` rows (see [`route_rows_of_types`]).
#[must_use]
pub fn worktree_state_from_content(content: &str, session_id: &str) -> Option<Value> {
    let loaded = route_rows_of_types(content, &["worktree-state"]);
    match loaded.worktree_states.get(session_id) {
        // An active worktree session (a JSON object); a cleared record is `null`.
        Some(v) if !v.is_null() => Some(v.clone()),
        _ => None,
    }
}

/// (P2-10 cc2.1.208) Scan a loaded transcript for every compact boundary's
/// `compactMetadata.preCompactDiscoveredTools`, returning the deduped, sorted
/// union of tool names.
///
/// This is the READ side of the Tool-Search deferred-tool carry: claude's resume
/// loader re-seeds the deferred-tool loaded-set from prior compact boundaries so a
/// tool the model loaded via `ToolSearch` before a compaction stays non-deferred
/// after a cold `--resume`. Mirrors the boundary arm of `Age()`
/// (`for (let s of i) t.add(s)` where `i =
/// n.compactMetadata?.preCompactDiscoveredTools`): every `type:"system"` /
/// `subtype:"compact_boundary"` line contributes its list to the set, across ALL
/// boundaries in the given slice (later boundaries already include earlier ones,
/// so the union is idempotent). Returns an empty `Vec` for any transcript without
/// a Tool-Search compaction — the default-off common path.
///
/// The caller marks the result loaded on the session's `DeferralState`; see the
/// orchestrator resume path (`ConversationOrchestrator::with_resume`).
#[must_use]
pub fn pre_compact_discovered_tools(messages: &[JsonlMessage]) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    for m in messages {
        if m.message_type != "system" {
            continue;
        }
        if m.extra.string_at("/subtype") != Some("compact_boundary") {
            continue;
        }
        let Some(tools) = m
            .extra
            .get_property("compactMetadata")
            .and_then(|metadata| metadata.get_property("preCompactDiscoveredTools"))
            .and_then(JsonValueView::array_len)
        else {
            continue;
        };
        let values = m
            .extra
            .get_property("compactMetadata")
            .and_then(|metadata| metadata.get_property("preCompactDiscoveredTools"))
            .expect("array length came from the same projection member");
        for index in 0..tools {
            if let Some(name) = values.get_index(index).and_then(JsonValueView::as_str) {
                set.insert(name.to_owned());
            }
        }
    }
    let mut names: Vec<String> = set.into_iter().collect();
    names.sort();
    names
}

/// Reconstruct every deferred tool discovered by `ToolSearch` from a loaded
/// transcript. This combines compact-boundary carry metadata with live
/// `tool_reference` blocks that have not yet been summarized.
///
/// Both LingXi's persisted protocol shape (`content_blocks`) and Claude Code's
/// Anthropic-compatible shape (array-valued `content`) are accepted, so a cold
/// resume does not lose schemas merely because no compaction occurred.
#[must_use]
pub fn discovered_tool_names(messages: &[JsonlMessage]) -> Vec<String> {
    let mut set: HashSet<String> = pre_compact_discovered_tools(messages).into_iter().collect();
    for message in messages {
        if message.message_type != "user" {
            continue;
        }
        let Some(content_len) = message
            .message
            .at_pointer("/content")
            .and_then(JsonValueView::array_len)
        else {
            continue;
        };
        for index in 0..content_len {
            let Some(tool_result) = message
                .message
                .at_pointer("/content")
                .and_then(|content| content.get_index(index))
            else {
                continue;
            };
            if tool_result
                .get_property("type")
                .and_then(JsonValueView::as_str)
                != Some("tool_result")
            {
                continue;
            }
            let nested = tool_result
                .get_property("content_blocks")
                .or_else(|| tool_result.get_property("content"));
            let Some(nested_len) = nested.and_then(JsonValueView::array_len) else {
                continue;
            };
            let Some(nested) = nested else { continue };
            for nested_index in 0..nested_len {
                let Some(block) = nested.get_index(nested_index) else {
                    continue;
                };
                if block.get_property("type").and_then(JsonValueView::as_str)
                    == Some("tool_reference")
                {
                    if let Some(name) = block
                        .get_property("tool_name")
                        .and_then(JsonValueView::as_str)
                    {
                        set.insert(name.to_owned());
                    }
                }
            }
        }
    }
    let mut names: Vec<String> = set.into_iter().collect();
    names.sort();
    names
}

/// Interactive line-based session picker (OQ-6 stdio fallback for the Ink TUI).
///
/// Renders:
/// ```text
/// Resume which session?
///   1. {title} [{modified}]
///   2. {title} [{modified}]
///   ...
/// >
/// ```
///
/// Behavior:
/// - Empty input line → `Ok(None)` (cancel).
/// - `1..=sessions.len()` (1-indexed) → `Ok(Some(uuid))`.
/// - Non-numeric, out-of-range, or `> sessions.len()` → print retry feedback,
///   try again up to **3 total attempts** (initial + 2 retries).
/// - After 3 failed attempts → `Err(LoaderError::InvalidSelection)`.
/// - EOF (zero-byte read) → `Ok(None)` (cancel).
///
/// `stdin` is any `AsyncBufRead`, `stdout` is any `AsyncWrite` — both passed
/// in so tests can supply `tokio::io::duplex` pairs (mirrors M5-05 permission
/// UX pattern).
///
/// Errors:
/// - [`LoaderError::EmptyDirectory`] if `sessions` is empty.
/// - [`LoaderError::InvalidSelection`] after 3 invalid inputs.
/// - [`LoaderError::Io`] on stdio read/write/flush failure.
pub async fn select_session_interactive<R, W>(
    sessions: &[SessionMetadata],
    stdin: &mut BufReader<R>,
    stdout: &mut W,
) -> Result<Option<Uuid>, LoaderError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    if sessions.is_empty() {
        return Err(LoaderError::EmptyDirectory);
    }
    let limit = sessions.len();

    // Render header + rows once.
    let mut out_buf = String::from("Resume which session?\n");
    for (i, row) in sessions.iter().enumerate() {
        let modified_rfc3339 = format_rfc3339_seconds(row.modified);
        out_buf.push_str(&format!(
            "  {}. {} [{}]\n",
            i + 1,
            row.title,
            modified_rfc3339
        ));
    }
    stdout
        .write_all(out_buf.as_bytes())
        .await
        .map_err(|source| LoaderError::Io {
            arg: "stdout".into(),
            source,
        })?;

    for _attempt in 0..3 {
        stdout
            .write_all(b"> ")
            .await
            .map_err(|source| LoaderError::Io {
                arg: "stdout".into(),
                source,
            })?;
        stdout.flush().await.map_err(|source| LoaderError::Io {
            arg: "stdout".into(),
            source,
        })?;

        let mut line = String::new();
        let n = stdin
            .read_line(&mut line)
            .await
            .map_err(|source| LoaderError::Io {
                arg: "stdin".into(),
                source,
            })?;
        if n == 0 {
            // EOF — treat as cancel.
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        match trimmed.parse::<usize>() {
            Ok(n) if (1..=limit).contains(&n) => {
                return Ok(Some(sessions[n - 1].uuid));
            }
            _ => {
                let msg = format!("Please enter a number from 1 to {limit}, or empty to cancel.\n");
                stdout
                    .write_all(msg.as_bytes())
                    .await
                    .map_err(|source| LoaderError::Io {
                        arg: "stdout".into(),
                        source,
                    })?;
            }
        }
    }
    Err(LoaderError::InvalidSelection)
}

/// RFC 3339 with second precision and `Z` suffix — e.g. `2026-05-24T19:03:12Z`.
///
/// Shared formatter for the resume surfaces: the M5-08 stdio picker above and
/// the M7-12 iocraft Resume screen (`tui::screens::resume`) both call
/// this so the two surfaces render timestamps byte-for-byte identically. Pre-1970
/// inputs (never produced by file mtime on the platforms we target) fall back to
/// the Unix epoch literal.
#[must_use]
pub fn format_rfc3339_seconds(t: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Cast to i64; pre-1970 timestamps are not produced by the OS for files we
    // care about (mtime). The session picker only ever sees positive offsets.
    #[allow(clippy::cast_possible_wrap)]
    let secs_i64 = secs as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs_i64, 0).map_or_else(
        || "1970-01-01T00:00:00Z".to_string(),
        |dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// Parse an RFC 3339 transcript timestamp to a comparable millisecond epoch.
/// Unparsable / empty timestamps sort OLDEST (`i64::MIN`) so a metadata-poor or
/// malformed leaf can never win the newest-leaf race — mirrors TS
/// `new Date(m.timestamp).getTime()` where an invalid date yields `NaN` and the
/// `ts > tipTs` comparison is always false.
fn timestamp_millis(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(i64::MIN)
}

/// Find Native's nearest preceding resume candidate when a chain edge was
/// removed (for example, by a server-fallback tombstone) or points into an
/// already-visited cycle. This is a read-time recovery only: the JSONL row's
/// `parentUuid` is never repaired or rewritten.
fn timestamp_parent_fallback<'a>(
    loaded: &'a LoadedTranscript,
    current: &JsonlMessage,
    visited: &HashSet<String>,
) -> Option<&'a JsonlMessage> {
    const MAX_PARENT_GAP_MS: i64 = 5_000;

    let current_ms = chrono::DateTime::parse_from_rfc3339(&current.timestamp)
        .ok()?
        .timestamp_millis();
    let mut seen_ids = HashSet::new();
    let mut nearest: Option<(i64, &JsonlMessage)> = None;

    // `Map#set` updates a duplicate UUID in place without changing its
    // insertion position. `messages_in_order` carries that first-seen order;
    // `by_uuid` supplies the last value for each ID.
    for row in &loaded.messages_in_order {
        if !seen_ids.insert(row.uuid.as_str()) {
            continue;
        }
        let Some(candidate) = loaded.by_uuid.get(row.uuid.as_str()) else {
            continue;
        };
        if visited.contains(&candidate.uuid)
            || candidate.uuid == current.uuid
            || candidate.is_sidechain != current.is_sidechain
        {
            continue;
        }
        let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(&candidate.timestamp) else {
            continue;
        };
        let candidate_ms = timestamp.timestamp_millis();
        let Some(gap_ms) = current_ms.checked_sub(candidate_ms) else {
            continue;
        };
        if !(0..=MAX_PARENT_GAP_MS).contains(&gap_ms) {
            continue;
        }
        if nearest.is_none_or(|(nearest_gap, _)| gap_ms < nearest_gap) {
            nearest = Some((gap_ms, candidate));
        }
    }

    nearest.map(|(_, candidate)| candidate)
}

/// Select a resumable main-thread user/assistant tip. Claude Code 2.1.286
/// prefers the durable tail or a saved checkpoint, promoting a stale checkpoint
/// when that tail follows it directly or through a parallel tool batch. An
/// explicit branch checkpoint permits only direct continuation. Preserved-tail
/// compactions use the effective reparented graph, and transcripts constructed
/// without write-order metadata retain newest-leaf selection with stable ties.
#[must_use]
pub fn find_tip<'a>(loaded: &'a LoadedTranscript, arg: &str) -> Option<&'a JsonlMessage> {
    let by_uuid = &loaded.by_uuid;
    if by_uuid.is_empty() {
        return None;
    }

    let preserved = preserved_tail_state(loaded);

    if loaded.cleared_to_empty {
        return None;
    }
    let ordered = ordered_messages(loaded, by_uuid);
    // 2.1.286 jln/yan: an ordinary checkpoint can be a completed sibling
    // tool result. Prefer the durable continuation of that same batch even
    // when the process died before writing its next last-prompt record.
    // Preserved-tail compactions retain their existing reparent-aware walk.
    if preserved.parents.is_empty() {
        if let Some(last) = loaded
            .last_transcript_uuid
            .as_deref()
            .filter(|id| by_uuid.contains_key(*id))
        {
            let checkpoint = loaded
                .last_prompt_leaf_uuid
                .as_deref()
                .filter(|id| by_uuid.get(*id).is_some_and(|row| !row.is_sidechain));
            let mut selected = checkpoint.unwrap_or(last);
            if !loaded.last_prompt_explicit
                && selected != last
                && follows_checkpoint(
                    by_uuid,
                    last,
                    selected,
                    true,
                    !loaded.last_prompt_was_explicit,
                )
            {
                selected = last;
            }
            let newest_written = ordered.iter().filter(|row| !row.is_sidechain).fold(
                None::<&&JsonlMessage>,
                |newest, row| match newest {
                    Some(prior) if prior.timestamp >= row.timestamp => Some(prior),
                    _ => Some(row),
                },
            );
            if checkpoint.is_none()
                && !loaded.last_prompt_seen
                && newest_written.is_some_and(|row| row.uuid != last)
            {
                // Delayed writes can put an ancestor at the file's tail.
                // Index descendants once; repeated parent walks are quadratic
                // on long transcripts.
                let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
                for row in &ordered {
                    if let Some(parent) = row.parent_uuid.as_deref() {
                        children.entry(parent).or_default().push(&row.uuid);
                    }
                }
                let mut descendants = HashSet::new();
                let mut pending = vec![last];
                while let Some(uuid) = pending.pop() {
                    if descendants.insert(uuid) {
                        pending.extend(children.get(uuid).into_iter().flatten().copied());
                    }
                }
                let mut newest = "";
                for row in &ordered {
                    if !row.is_sidechain
                        && row.uuid != last
                        && row.timestamp.as_str() >= newest
                        && descendants.contains(row.uuid.as_str())
                    {
                        selected = &row.uuid;
                        newest = &row.timestamp;
                    }
                }
            }
            let mut cursor = by_uuid.get(selected);
            let mut seen = HashSet::new();
            while let Some(row) = cursor {
                if !seen.insert(row.uuid.as_str()) {
                    break;
                }
                if !row.is_sidechain && matches!(row.message_type.as_str(), "user" | "assistant") {
                    return Some(row);
                }
                cursor = row.parent_uuid.as_deref().and_then(|id| by_uuid.get(id));
            }
        }
    }

    // (0) Gap #1 fix: explicit last-prompt override.
    // Binary: `V = L&&O&&n.has(O)&&!n.get(O)?.isSidechain`
    // where L = explicit and O = leafUuid. Mirror: if explicit is set AND the
    // leafUuid is present as a non-sidechain user/assistant participant → force.
    if loaded.last_prompt_explicit {
        if let Some(lp_uuid) = &loaded.last_prompt_leaf_uuid {
            if let Some(lp_msg) = by_uuid.get(lp_uuid.as_str()) {
                if !preserved.pruned.contains(lp_uuid)
                    && !lp_msg.is_sidechain
                    && (lp_msg.message_type == "user" || lp_msg.message_type == "assistant")
                {
                    tracing::debug!(
                        session = arg,
                        tip = %lp_uuid,
                        "last-prompt explicit override: forcing resume tip",
                    );
                    return Some(lp_msg);
                }
            }
        }
    }

    // (0b) Reparent-aware graph (claude 2.1.207 `E$_`): a `compact_boundary`'s
    // `compactMetadata.preservedMessages` re-parents the verbatim kept tail onto
    // the summary anchor. claude mutates the loaded messages IN PLACE before the
    // leaf computation runs, so the summary line stops being a graph terminal
    // (its first preserved child now points at it) and the SOLE tip is the
    // post-compact leaf. We mirror that here by resolving each message's
    // EFFECTIVE parent through the same overlay when computing terminals/leaves
    // below. Without it the summary competes as an independent leaf and, on a
    // same-millisecond timestamp tie with the real post-compact tip, can win the
    // (randomized-`HashSet`-order) selection — truncating the resumed chain to
    // just the summary and dropping the preserved tail. Empty (zero-cost) on any
    // transcript without a preserved-tail compaction.
    let reparent = &preserved.parents;

    // (1) Every EFFECTIVE parentUuid that is actually referenced (overlay first,
    // else the on-disk parent).
    let mut parent_uuids: HashSet<&str> = HashSet::new();
    for m in ordered
        .iter()
        .copied()
        .filter(|m| !preserved.pruned.contains(&m.uuid))
    {
        if let Some(p) = reparent
            .get(m.uuid.as_str())
            .map(String::as_str)
            .or(m.parent_uuid.as_deref())
        {
            parent_uuids.insert(p);
        }
    }

    // (2) Terminals = messages no other message points at.
    // (3) From each terminal, walk up to the nearest user/assistant leaf.
    let mut leaf_uuids: HashSet<String> = HashSet::new();
    for m in ordered
        .iter()
        .copied()
        .filter(|m| !preserved.pruned.contains(&m.uuid))
    {
        if parent_uuids.contains(m.uuid.as_str()) {
            continue; // not a terminal
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current: Option<&JsonlMessage> = Some(m);
        while let Some(node) = current {
            if !seen.insert(node.uuid.as_str()) {
                // Cycle in the parentUuid graph — abandon this terminal's walk.
                tracing::warn!(
                    session = arg,
                    at = %node.uuid,
                    "cycle detected walking transcript leaves; skipping terminal",
                );
                break;
            }
            if node.message_type == "user" || node.message_type == "assistant" {
                leaf_uuids.insert(node.uuid.clone());
                break;
            }
            current = reparent
                .get(node.uuid.as_str())
                .map(String::as_str)
                .or(node.parent_uuid.as_deref())
                .and_then(|p| by_uuid.get(p))
                .filter(|m| !preserved.pruned.contains(&m.uuid));
        }
    }

    // (4) tip = newest non-sidechain user/assistant leaf.
    let mut tip: Option<&JsonlMessage> = None;
    let mut tip_ts: i64 = i64::MIN;
    for m in ordered
        .iter()
        .copied()
        .filter(|m| leaf_uuids.contains(&m.uuid))
    {
        if m.is_sidechain {
            continue;
        }
        if m.message_type != "user" && m.message_type != "assistant" {
            continue;
        }
        let ts = timestamp_millis(&m.timestamp);
        // `>` (not `>=`) so the FIRST-seen leaf wins an exact-timestamp tie,
        // matching TS `loadMessagesFromJsonlPath`'s `if (ts > tipTs)`.
        if tip.is_none() || ts > tip_ts {
            tip_ts = ts;
            tip = Some(m);
        }
    }
    tip
}

/// Tolerant, branch-aware reconstruction of a transcript's MAIN conversation
/// thread — the structural equivalent of `claude-code`'s leaf computation
/// (`sessionStorage.ts:3716`, original/non-pebble branch) + newest-non-sidechain
/// leaf selection + `buildConversationChain` (`sessionStorage.ts:2069`), as
/// composed by `loadMessagesFromJsonlPath` (`conversationRecovery.ts:416`).
///
/// Algorithm:
///  1.–4. Pick the chain TIP via [`find_tip`] (newest non-sidechain
///     `user`/`assistant` leaf). The tip — not the file's first row — supplies
///     the returned session id (forked sessions copy `chain[0]` from the source).
///  5. Walk `tip → root` via `parentUuid` + `by_uuid.get`; when an edge is
///     missing or cycles, use Native's nearest preceding same-sidechain row
///     within five seconds as a read-time fallback. Then reverse to root → tip
///     order. The persisted parent links are not mutated.
///
/// Returns `(main_thread, tip_session_id)`. When no non-sidechain
/// user/assistant leaf exists the chain is empty and the session id is the
/// requested `arg` (the caller maps the empty chain to "nothing to resume").
///
///  6. Run [`recover_parallel_results`]
///     (`recoverOrphanedParallelToolResults`, `sessionStorage.ts:2096`): the
///     single-parent walk keeps one branch, so PARALLEL tool calls (N
///     `tool_use`s → N one-block assistant siblings sharing `message.id`) leave
///     off-chain siblings + their `tool_result`s orphaned. The post-pass splices
///     each group's recovered records into its batch in transcript insertion
///     order, including unambiguous metadata tails and result identities whose
///     original parent was missing or pointed outside the batch.
#[must_use]
pub fn build_conversation_chain(
    loaded: &LoadedTranscript,
    arg: &str,
) -> (Vec<JsonlMessage>, String) {
    let by_uuid = &loaded.by_uuid;
    if by_uuid.is_empty() {
        return (Vec::new(), arg.to_string());
    }

    // Steps (1)–(4): newest non-sidechain user/assistant leaf (the chain tip).
    let Some(tip) = find_tip(loaded, arg) else {
        // No resumable leaf — caller surfaces "nothing to resume".
        return (Vec::new(), arg.to_string());
    };
    let tip_session_id = tip.session_id.clone();

    // (4b) Compact-boundary re-splice (`E$_`, 2.1.207): each
    // `subtype:"compact_boundary"` line with `compactMetadata.preservedMessages`
    // re-parents its preserved tail onto the anchor (the last summary line), so
    // the tip→root walk below rejoins the post-compact chain — [boundary ←
    // summary ← preserved tail ← …] — instead of following the tail's on-disk
    // parents back into the FULL pre-compact history. Empty on transcripts
    // without preserved-tail compactions (zero-cost common path).
    let preserved = preserved_tail_state(loaded);
    let reparent = &preserved.parents;

    // (5) Walk tip → root, cycle-guarded. A null parent is a real root and
    // stops the walk; a non-null missing/repeated parent gets Native's nearest
    // preceding timestamp fallback. Boundary lines carry `parentUuid: null`
    // (the claude chain reset), so the summarized prefix never re-enters.
    let mut chain: Vec<JsonlMessage> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current: Option<&JsonlMessage> = Some(tip);
    while let Some(node) = current {
        if !seen.insert(node.uuid.clone()) {
            tracing::warn!(
                session = arg,
                at = %node.uuid,
                "cycle detected in parentUuid chain; returning partial transcript",
            );
            break;
        }
        // Apply the (4b) re-parent overlay: patch the pushed clone so the
        // returned chain is self-consistent (each entry's parentUuid points at
        // its in-chain predecessor), mirroring `E$_`'s in-place mutation.
        let mut entry = node.clone();
        if let Some(p) = reparent.get(node.uuid.as_str()) {
            entry.parent_uuid = Some(p.clone());
        }
        let parent = entry.parent_uuid.clone();
        chain.push(entry);
        current = match parent.as_deref() {
            Some(parent_uuid) => {
                let parent = by_uuid
                    .get(parent_uuid)
                    .filter(|message| !preserved.pruned.contains(&message.uuid))
                    .filter(|message| !seen.contains(&message.uuid));
                parent.or_else(|| timestamp_parent_fallback(loaded, node, &seen))
            }
            None => None, // reached a root or an intentional compact boundary
        };
    }
    chain.reverse();

    // (6) Recover sibling assistant blocks + tool_results the single-parent walk
    // orphaned (parallel-tool-call DAG). Additive post-pass; never reorders the
    // main chain. `seen` is exactly the set of on-chain uuids (the cycle guard
    // breaks BEFORE pushing, so no extra entries).
    let filtered;
    let recovery_messages = if preserved.pruned.is_empty() {
        by_uuid
    } else {
        filtered = by_uuid
            .iter()
            .filter(|(uuid, _)| !preserved.pruned.contains(*uuid))
            .map(|(uuid, message)| (uuid.clone(), message.clone()))
            .collect();
        &filtered
    };
    let ordered = ordered_messages(loaded, recovery_messages);
    let mut chain = recover_parallel_results(recovery_messages, &ordered, chain, &mut seen);
    append_metadata_descendants(&ordered, &tip.uuid, &mut chain, &mut seen);

    // Preserved assistant records still carry pre-compaction usage on disk.
    // Claude's applyPreservedSegmentRelinks clears these counters on resume,
    // otherwise restoring the context meter can immediately trigger another
    // compaction. Apply after sibling recovery so every preserved block is
    // treated alike; leave fresh post-boundary usage and billing metadata intact.
    for entry in &mut chain {
        if entry.message_type == "assistant" && preserved.usage_to_clear.contains(&entry.uuid) {
            let mut usage = entry
                .message
                .subprojection("/usage")
                .unwrap_or_else(|_| Utf16JsonProjection::parse("{}").expect("empty object"));
            if usage
                .at_pointer("")
                .and_then(JsonValueView::object_entries)
                .is_none()
            {
                usage = Utf16JsonProjection::parse("{}").expect("empty object");
            }
            for field in [
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ] {
                usage
                    .set_field(
                        field,
                        Utf16JsonProjection::parse("0").expect("zero is JSON"),
                    )
                    .expect("usage is an object");
            }
            entry
                .message
                .set_field("usage", usage)
                .expect("assistant message is an object");
        }
    }

    // A recovery marker can be the final persisted row when the retry was
    // interrupted. find_tip deliberately chooses user/assistant messages, so
    // retain attached thinking markers even without a later assistant leaf.
    let positions: HashMap<&str, usize> = loaded
        .messages_in_order
        .iter()
        .enumerate()
        .map(|(index, message)| (message.uuid.as_str(), index))
        .collect();
    for (position, marker) in loaded.messages_in_order.iter().enumerate() {
        if preserved.pruned.contains(&marker.uuid)
            || marker.message_type != "attachment"
            || marker
                .attachment_projection
                .as_ref()
                .and_then(|value| value.string_at("/type"))
                != Some("thinking_stripped")
            || marker.is_sidechain
            || seen.contains(&marker.uuid)
            || !marker
                .parent_uuid
                .as_ref()
                .is_some_and(|parent| seen.contains(parent))
        {
            continue;
        }
        let at = chain
            .iter()
            .position(|message| {
                positions
                    .get(message.uuid.as_str())
                    .is_some_and(|index| *index > position)
            })
            .unwrap_or(chain.len());
        chain.insert(at, marker.clone());
        seen.insert(marker.uuid.clone());
    }

    (chain, tip_session_id)
}

/// Read a chain-participant line's `message.id` (the Anthropic message id shared
/// by all sibling blocks of one streamed assistant turn). `None` when absent or
/// non-string — mirrors TS's `m.message.id` truthiness gate.
fn message_id(m: &JsonlMessage) -> Option<&str> {
    m.message.string_at("/id")
}

/// Compact-boundary preserved-tail re-parent overlay — the `E$_` pass
/// (claude 2.1.207): for every `type:"system"`/`subtype:"compact_boundary"`
/// line carrying `compactMetadata.preservedMessages` (`{anchorUuid, uuids,
/// allUuids}`), re-parent each preserved uuid onto the rolling anchor in
/// sequence — `uuids[0].parentUuid = anchorUuid`, `uuids[1].parentUuid =
/// uuids[0]`, … — splicing the verbatim kept tail back in AFTER the summary.
/// Without this, the post-compact suffix (whose first new line parents off the
/// tail's last on-disk line) walks straight through the tail into the FULL
/// pre-compact history and never reaches the summary/boundary.
///
/// Returns a `line uuid → new parent uuid` overlay (empty when no boundary
/// carries preserved metadata). Port adaptation: claude's in-memory history is
/// already one-block-per-message, so its preserved uuids ARE on-disk line
/// uuids; this engine merges assistant blocks in memory and splits them
/// per-block on write, so a preserved id that matches no outer `uuid` is
/// resolved to every assistant line sharing that inner `message.id`, in file
/// order (the write-side split invariant). Unresolvable ids are skipped
/// (best-effort, like the tolerant walk).
#[derive(Default)]
struct PreservedTailState {
    parents: HashMap<String, String>,
    usage_to_clear: HashSet<String>,
    pruned: HashSet<String>,
}

/// Claude's applyPreservedSegmentRelinks: only the latest boundary can keep
/// a segment live. Validate the original tail-to-head chain before applying
/// any relinks or pruning, so malformed transcripts remain recoverable.
fn preserved_tail_state(loaded: &LoadedTranscript) -> PreservedTailState {
    let boundaries: Vec<_> = loaded
        .messages_in_order
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.message_type == "system"
                && line.extra.string_at("/subtype") == Some("compact_boundary")
        })
        .collect();
    let latest_segment = boundaries.iter().rev().find(|(_, line)| {
        line.extra
            .get_property("compactMetadata")
            .and_then(|metadata| metadata.get_property("preservedSegment"))
            .is_some()
    });
    let Some((segment_index, boundary)) = latest_segment else {
        let parents = legacy_preserved_tail_reparents(loaded);
        return PreservedTailState {
            usage_to_clear: parents.keys().cloned().collect(),
            parents,
            pruned: HashSet::new(),
        };
    };
    let mut state = PreservedTailState::default();
    let latest_index = boundaries.last().expect("segment is a boundary").0;
    if *segment_index == latest_index {
        let Some(segment) = boundary
            .extra
            .get_property("compactMetadata")
            .and_then(|metadata| metadata.get_property("preservedSegment"))
        else {
            return state;
        };
        let (Some(head), Some(tail), Some(anchor)) = (
            segment
                .get_property("headUuid")
                .and_then(JsonValueView::as_str),
            segment
                .get_property("tailUuid")
                .and_then(JsonValueView::as_str),
            segment
                .get_property("anchorUuid")
                .and_then(JsonValueView::as_str),
        ) else {
            return state;
        };
        // Compaction stores in-memory response IDs; persistence splits each
        // assistant into block rows. Head is the first block, tail/anchor last.
        let head = preserved_endpoint_uuid(loaded, head, false);
        let tail = preserved_endpoint_uuid(loaded, tail, true);
        let anchor = preserved_endpoint_uuid(loaded, anchor, true);
        let mut current = loaded.by_uuid.get(tail);
        let mut reached_head = false;
        while let Some(message) = current {
            if !state.usage_to_clear.insert(message.uuid.clone()) {
                break;
            }
            if message.uuid == head {
                reached_head = true;
                break;
            }
            current = message
                .parent_uuid
                .as_ref()
                .and_then(|p| loaded.by_uuid.get(p));
        }
        if !reached_head {
            return PreservedTailState::default();
        }
        // LingXi persists parallel results against their source assistant,
        // creating branches outside the validated tail-to-head parent path.
        // Keep the latest boundary's complete membership before pruning so
        // sibling recovery can still see those branches. Resolve in-memory
        // assistant IDs to every split row, as with preserved endpoints.
        if let Some(messages) = boundary
            .extra
            .get_property("compactMetadata")
            .and_then(|metadata| metadata.get_property("preservedMessages"))
        {
            let mut ids = HashSet::<&str>::new();
            for key in ["uuids", "allUuids"] {
                if let Some(values) = messages.get_property(key) {
                    if let Some(length) = values.array_len() {
                        for index in 0..length {
                            if let Some(id) =
                                values.get_index(index).and_then(JsonValueView::as_str)
                            {
                                ids.insert(id);
                            }
                        }
                    }
                }
            }
            for message in &loaded.messages_in_order[..latest_index] {
                if ids.contains(message.uuid.as_str())
                    || (message.message_type == "assistant"
                        && message_id(message)
                            .is_some_and(|id| ids.contains(id) && !loaded.by_uuid.contains_key(id)))
                {
                    state.usage_to_clear.insert(message.uuid.clone());
                }
            }
        }
        state.parents.insert(head.to_string(), anchor.to_string());
        // Children appended against the summary before the tail was re-linked
        // must follow the tail, while the original segment internals stay intact.
        for message in loaded.by_uuid.values() {
            if message.parent_uuid.as_deref() == Some(anchor) && message.uuid != head {
                state.parents.insert(message.uuid.clone(), tail.to_string());
            }
        }
    }
    state.pruned = loaded.messages_in_order[..latest_index]
        .iter()
        .filter(|message| !state.usage_to_clear.contains(&message.uuid))
        .map(|message| message.uuid.clone())
        .collect();
    state
}

fn preserved_endpoint_uuid<'a>(loaded: &'a LoadedTranscript, id: &'a str, last: bool) -> &'a str {
    if loaded.by_uuid.contains_key(id) {
        return id;
    }
    let mut rows = loaded
        .messages_in_order
        .iter()
        .filter(|message| message.message_type == "assistant" && message_id(message) == Some(id));
    if last { rows.next_back() } else { rows.next() }.map_or(id, |message| message.uuid.as_str())
}

fn legacy_preserved_tail_reparents(loaded: &LoadedTranscript) -> HashMap<String, String> {
    let mut reparent: HashMap<String, String> = HashMap::new();
    for line in &loaded.messages_in_order {
        if line.message_type != "system"
            || line.extra.string_at("/subtype") != Some("compact_boundary")
        {
            continue;
        }
        let Some(pm) = line
            .extra
            .get_property("compactMetadata")
            .and_then(|metadata| metadata.get_property("preservedMessages"))
        else {
            continue;
        };
        let Some(mut anchor) = pm
            .get_property("anchorUuid")
            .and_then(JsonValueView::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(uuids) = pm.get_property("uuids").and_then(JsonValueView::array_len) else {
            continue;
        };
        for index in 0..uuids {
            let Some(preserved) = pm
                .get_property("uuids")
                .and_then(|values| values.get_index(index))
                .and_then(JsonValueView::as_str)
            else {
                continue;
            };
            // Resolve the preserved id to its on-disk line(s): outer `uuid`
            // first; else the per-block assistant siblings sharing this inner
            // `message.id`, in file order.
            let mut resolved: Vec<&str> = Vec::new();
            if loaded.by_uuid.contains_key(preserved) {
                resolved.push(preserved);
            } else {
                for m in &loaded.messages_in_order {
                    if m.message_type == "assistant" && message_id(m) == Some(preserved) {
                        resolved.push(m.uuid.as_str());
                    }
                }
            }
            for line_uuid in resolved {
                reparent.insert(line_uuid.to_string(), anchor);
                anchor = line_uuid.to_string();
            }
        }
    }
    reparent
}

#[cfg(test)]
mod tests {
    //! SESSION.1 coverage — `list_recent_sessions` HIDES sub-agent / sidechain
    //! transcripts (first line `isSidechain` or carrying a truthy `teamName`),
    //! matching claude-code's `parseSessionInfoFromLite` / `enrichLog` /
    //! `filterResumableSessions`. Driven over a real `tempfile` fixture via the
    //! posix `FileSystem`, mirroring `session/tests/list_recent_test.rs`.

    use super::*;
    use platform_posix::fs::PosixFileSystem;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;

    fn projection(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).unwrap()
    }

    // `has_visible_content` ports the two claude-code predicates `JZm` (user) and
    // `XZm` (assistant) exactly. `JZm`: non-empty string, OR an array with any
    // text/image/document block by TYPE ALONE. `XZm`: content MUST be an array
    // (a string is never visible) containing a NON-EMPTY text block; image/
    // document blocks do not count for an assistant.
    #[test]
    fn has_visible_content_matches_jzm_user_predicate() {
        let v = |c| {
            let projection = projection(c);
            has_visible_content(projection.at_pointer(""), /* assistant = */ false)
        };
        // String: non-empty visible, empty/whitespace not.
        assert!(v(json!("hi")));
        assert!(!v(json!("   ")));
        assert!(!v(json!("")));
        // Array text block is visible BY TYPE ALONE — even when text is empty.
        assert!(v(json!([{ "type": "text", "text": "" }])));
        assert!(v(json!([{ "type": "text", "text": "  " }])));
        assert!(v(json!([{ "type": "text", "text": "hello" }])));
        // image / document make a USER message visible.
        assert!(v(json!([{ "type": "image" }])));
        assert!(v(json!([{ "type": "document" }])));
        // tool_result-only is NOT visible.
        assert!(!v(json!([{ "type": "tool_result", "content": "x" }])));
        // Non-string / non-array content is not visible.
        assert!(!v(json!(123)));
    }

    #[test]
    fn has_visible_content_matches_xzm_assistant_predicate() {
        let v = |c| {
            let projection = projection(c);
            has_visible_content(projection.at_pointer(""), /* assistant = */ true)
        };
        // `XZm` requires an array — a bare string is NEVER visible for assistant.
        assert!(!v(json!("hi")));
        assert!(!v(json!("")));
        // Array text block is visible only when its text is non-empty.
        assert!(v(json!([{ "type": "text", "text": "hello" }])));
        assert!(!v(json!([{ "type": "text", "text": "" }])));
        assert!(!v(json!([{ "type": "text", "text": "   " }])));
        // image / document do NOT make an assistant message visible.
        assert!(!v(json!([{ "type": "image" }])));
        assert!(!v(json!([{ "type": "document" }])));
        // tool_use-only / thinking-only assistant is NOT visible.
        assert!(!v(json!([{ "type": "tool_use", "name": "Read" }])));
        assert!(!v(json!([{ "type": "thinking", "thinking": "…" }])));
    }

    // --- P2-10 preCompactDiscoveredTools cold-resume scan ----------------- //

    /// A minimal `compact_boundary` system line carrying `compactMetadata` with
    /// the given `preCompactDiscoveredTools` list (`None` ⇒ field absent). The
    /// on-disk shape matches `persist_compact_boundary_to_jsonl`.
    fn boundary_line(uuid: &str, discovered: Option<&[&str]>) -> JsonlMessage {
        let mut compact_metadata = serde_json::Map::new();
        compact_metadata.insert("trigger".to_string(), json!("auto"));
        compact_metadata.insert("preTokens".to_string(), json!(1234));
        if let Some(tools) = discovered {
            compact_metadata.insert("preCompactDiscoveredTools".to_string(), json!(tools));
        }
        let mut extra = serde_json::Map::new();
        extra.insert("subtype".to_string(), json!("compact_boundary"));
        extra.insert("content".to_string(), json!("Conversation compacted"));
        extra.insert("level".to_string(), json!("info"));
        extra.insert(
            "compactMetadata".to_string(),
            Value::Object(compact_metadata),
        );
        JsonlMessage {
            message_type: "system".to_string(),
            uuid: uuid.to_string(),
            parent_uuid: None,
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            timestamp: "2026-07-13T10:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.6.0".to_string(),
            message: Utf16JsonProjection::parse("null").unwrap(),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: Utf16JsonProjection::from_serde_value(&Value::Object(extra)).unwrap(),

            attachment_projection: None,
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
        }
    }

    fn user_line(uuid: &str) -> JsonlMessage {
        JsonlMessage {
            message_type: "user".to_string(),
            uuid: uuid.to_string(),
            parent_uuid: None,
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            timestamp: "2026-07-13T10:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.6.0".to_string(),
            message: projection(json!({"role": "user", "content": "hi"})),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                .expect("empty JSON object"),

            attachment_projection: None,
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
        }
    }

    fn assistant_line(
        uuid: &str,
        parent_uuid: Option<&str>,
        timestamp: &str,
        is_sidechain: bool,
    ) -> JsonlMessage {
        let mut message = user_line(uuid);
        message.message_type = "assistant".to_string();
        message.parent_uuid = parent_uuid.map(str::to_owned);
        message.timestamp = timestamp.to_string();
        message.is_sidechain = is_sidechain;
        message.message = projection(json!({
            "id": format!("provider-{uuid}"),
            "role": "assistant",
            "content": [{"type":"text", "text":uuid}],
            "model": "test-model",
            "stop_reason": "end_turn"
        }));
        message
    }

    fn loaded_transcript(rows: Vec<JsonlMessage>) -> LoadedTranscript {
        let mut loaded = LoadedTranscript::default();
        for row in rows {
            loaded.by_uuid.insert(row.uuid.clone(), row.clone());
            loaded.messages_in_order.push(row);
        }
        loaded
    }

    fn user_with_content(uuid: &str, content: Value) -> JsonlMessage {
        let mut m = user_line(uuid);
        m.message = projection(json!({ "role": "user", "content": content }));
        m
    }

    const LOOP_TAG: &str = "<command-name>/loop</command-name>";

    #[test]
    fn is_loop_session_detects_plain_first_prompt() {
        let msgs = vec![user_with_content("u1", json!(LOOP_TAG))];
        assert!(is_loop_session(&msgs));
    }

    #[test]
    fn is_loop_session_false_when_first_prompt_not_loop() {
        let msgs = vec![
            user_with_content("u1", json!("just a normal question")),
            user_with_content("u2", json!(LOOP_TAG)),
        ];
        // Detector returns on the FIRST genuine prompt — a non-`/loop` first
        // prompt yields false even if a later message carries the tag.
        assert!(!is_loop_session(&msgs));
    }

    #[test]
    fn is_loop_session_skips_meta_and_compact_before_real_prompt() {
        // A compacted / meta-first transcript: the 2.1.211 fix. The old
        // `messages[0]` check would test the isMeta line (no tag) → false and
        // fail to hide the session. The fixed detector skips it.
        let mut meta = user_with_content("m1", json!("<system reminder>"));
        meta.extra
            .set_field(
                "isMeta",
                Utf16JsonProjection::parse("true").expect("true is JSON"),
            )
            .unwrap();
        let mut compact = user_with_content("c1", json!("compacted summary"));
        compact
            .extra
            .set_field("isCompactSummary", projection(json!(true)))
            .unwrap();
        let msgs = vec![
            meta,
            compact,
            user_with_content("u1", json!([{ "type": "text", "text": LOOP_TAG }])),
        ];
        assert!(is_loop_session(&msgs));
    }

    #[test]
    fn is_loop_session_skips_tool_result_turns() {
        // A tool_result comes back as a `type:"user"` line; it must be skipped
        // so the genuine `/loop` prompt behind it is the one that's tested.
        let msgs = vec![
            user_with_content(
                "t1",
                json!([{ "type": "tool_result", "tool_use_id": "x", "content": "ok" }]),
            ),
            user_with_content("u1", json!([{ "type": "text", "text": LOOP_TAG }])),
        ];
        assert!(is_loop_session(&msgs));
    }

    #[test]
    fn is_loop_session_ignores_non_user_lines() {
        let msgs = vec![
            boundary_line("b1", None),
            user_with_content("u1", json!(LOOP_TAG)),
        ];
        assert!(is_loop_session(&msgs));
    }

    #[test]
    fn is_loop_session_empty_transcript_is_false() {
        assert!(!is_loop_session(&[]));
    }

    #[test]
    fn resume_parent_fallback_rejoins_tombstoned_stream_ancestry_without_mutating_links() {
        let root = user_line("root");
        let mut root = root;
        root.timestamp = "2026-10-03T12:00:00.000Z".into();
        let survivor = assistant_line(
            "survivor",
            Some("physically-removed-row"),
            "2026-10-03T12:00:04.000Z",
            false,
        );
        let loaded = loaded_transcript(vec![root, survivor]);

        let (chain, _) = build_conversation_chain(&loaded, "session");
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            ["root", "survivor"]
        );
        assert_eq!(
            chain.last().and_then(|row| row.parent_uuid.as_deref()),
            Some("physically-removed-row"),
            "resume may recover the chain in memory but must preserve the source parent field"
        );
    }

    #[tokio::test]
    async fn physical_tombstone_keeps_dangling_parent_and_resume_recovers_preceding_row() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        let session_id = Uuid::new_v4().to_string();
        let root_id = Uuid::new_v4().to_string();
        let discarded_id = Uuid::new_v4().to_string();
        let survivor_id = Uuid::new_v4().to_string();
        let source_rows = [
            serde_json::json!({
                "type":"user", "uuid":root_id, "parentUuid":null,
                "sessionId":session_id, "timestamp":"2026-10-03T12:00:00.000Z",
                "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
                "message":{"role":"user","content":"prompt"}
            }),
            serde_json::json!({
                "type":"assistant", "uuid":discarded_id, "parentUuid":root_id,
                "sessionId":session_id, "timestamp":"2026-10-03T12:00:02.000Z",
                "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
                "message":{"id":"provider-1","role":"assistant","model":"model-a","stop_reason":"end_turn","content":[{"type":"text","text":"discard me"}]}
            }),
            serde_json::json!({
                "type":"assistant", "uuid":survivor_id, "parentUuid":discarded_id,
                "sessionId":session_id, "timestamp":"2026-10-03T12:00:04.000Z",
                "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
                "message":{"id":"provider-2","role":"assistant","model":"model-b","stop_reason":"end_turn","content":[{"type":"text","text":"survived"}]}
            }),
        ];
        let mut source = source_rows
            .iter()
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        source.push('\n');
        std::fs::write(&path, source).unwrap();

        let fs = make_fs(temp.path());
        let writer = crate::jsonl::writer::JsonlWriter::new(path.clone(), fs.clone());
        writer
            .bootstrap_session_message_identity_snapshot(&path)
            .await
            .expect("import the existing Native rows before Host tombstone mutation");
        assert!(writer.remove_message_by_uuid(&discarded_id).await.unwrap());

        let loaded = JsonlReader::new(path, fs).read_routed().await.unwrap();
        assert!(!loaded.by_uuid.contains_key(&discarded_id));
        assert_eq!(
            loaded.by_uuid[&survivor_id].parent_uuid.as_deref(),
            Some(discarded_id.as_str()),
            "physical deletion leaves the child's native parentUuid untouched"
        );
        let (chain, _) = build_conversation_chain(&loaded, &session_id);
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            [root_id.as_str(), survivor_id.as_str()]
        );
    }

    #[test]
    fn resume_parent_fallback_rejects_other_sidechains_and_rows_over_five_seconds_old() {
        let mut wrong_sidechain = user_line("sidechain");
        wrong_sidechain.timestamp = "2026-10-03T12:00:04.000Z".into();
        wrong_sidechain.is_sidechain = true;
        let too_old = assistant_line("too-old", None, "2026-10-03T11:59:54.999Z", false);
        let survivor = assistant_line(
            "survivor",
            Some("missing"),
            "2026-10-03T12:00:00.000Z",
            false,
        );
        let loaded = loaded_transcript(vec![wrong_sidechain, too_old, survivor]);

        let (chain, _) = build_conversation_chain(&loaded, "session");
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            ["survivor"],
            "candidate scope is exact sidechain identity and an inclusive 5000ms window"
        );
    }

    #[test]
    fn resume_parent_fallback_requires_valid_current_and_candidate_timestamps() {
        let mut invalid_candidate = user_line("invalid-candidate");
        invalid_candidate.timestamp = "not-a-date".into();
        let invalid_current =
            assistant_line("invalid-current", Some("missing"), "not-a-date", false);
        let tip = assistant_line(
            "tip",
            Some("invalid-current"),
            "2026-10-03T12:00:00.000Z",
            false,
        );
        let loaded = loaded_transcript(vec![user_line("valid-root"), invalid_current, tip]);

        let (chain, _) = build_conversation_chain(&loaded, "session");
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            ["invalid-current", "tip"],
            "an invalid current timestamp does not invoke the timestamp fallback"
        );

        let current = assistant_line(
            "current",
            Some("missing"),
            "2026-10-03T12:00:00.000Z",
            false,
        );
        let loaded = loaded_transcript(vec![invalid_candidate, current]);
        let (chain, _) = build_conversation_chain(&loaded, "session");
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            ["current"],
            "invalid candidate timestamps are ignored"
        );
    }

    #[test]
    fn resume_parent_fallback_uses_first_transcript_candidate_for_equal_timestamps() {
        let first = user_line("first");
        let mut first = first;
        first.timestamp = "2026-10-03T12:00:00.000Z".into();
        let second = assistant_line("second", None, "2026-10-03T12:00:00.000Z", false);
        let survivor = assistant_line(
            "survivor",
            Some("missing"),
            "2026-10-03T12:00:01.000Z",
            false,
        );
        let loaded = loaded_transcript(vec![first, second, survivor]);

        let (chain, _) = build_conversation_chain(&loaded, "session");
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            ["first", "survivor"],
            "the nearest timestamp tie keeps Map insertion order"
        );
    }

    #[test]
    fn resume_parent_fallback_breaks_a_cycle_with_the_nearest_preceding_row() {
        let mut root = user_line("root");
        root.timestamp = "2026-10-03T12:00:00.000Z".into();
        let left = assistant_line("left", Some("right"), "2026-10-03T12:00:01.000Z", false);
        let right = assistant_line("right", Some("left"), "2026-10-03T12:00:02.000Z", false);
        let tip = assistant_line("tip", Some("right"), "2026-10-03T12:00:03.000Z", false);
        let loaded = loaded_transcript(vec![root, left, right, tip]);

        let (chain, _) = build_conversation_chain(&loaded, "session");
        assert_eq!(
            chain
                .iter()
                .map(|row| row.uuid.as_str())
                .collect::<Vec<_>>(),
            ["root", "left", "right", "tip"]
        );
        assert_eq!(chain[1].parent_uuid.as_deref(), Some("right"));
        assert_eq!(chain[2].parent_uuid.as_deref(), Some("left"));
    }

    #[test]
    fn pre_compact_discovered_tools_scans_single_boundary_sorted() {
        let msgs = vec![
            user_line("u1"),
            boundary_line("b1", Some(&["WebFetch", "Task", "Agent"])),
            user_line("u2"),
        ];
        assert_eq!(
            pre_compact_discovered_tools(&msgs),
            vec![
                "Agent".to_string(),
                "Task".to_string(),
                "WebFetch".to_string()
            ],
        );
    }

    #[test]
    fn pre_compact_discovered_tools_unions_and_dedups_across_boundaries() {
        // Later boundaries include earlier ones (claude's `Age` accumulates); the
        // union is idempotent + deduped.
        let msgs = vec![
            boundary_line("b1", Some(&["Task"])),
            user_line("u1"),
            boundary_line("b2", Some(&["Task", "WebFetch"])),
        ];
        assert_eq!(
            pre_compact_discovered_tools(&msgs),
            vec!["Task".to_string(), "WebFetch".to_string()],
        );
    }

    #[test]
    fn pre_compact_discovered_tools_empty_without_toolsearch_compaction() {
        // No boundary, a boundary WITHOUT the field, and a non-system line all
        // contribute nothing — the default-off common path returns empty.
        assert!(pre_compact_discovered_tools(&[user_line("u1")]).is_empty());
        assert!(pre_compact_discovered_tools(&[boundary_line("b1", None)]).is_empty());
        assert!(pre_compact_discovered_tools(&[boundary_line("b1", Some(&[]))]).is_empty());
    }

    #[test]
    fn discovered_tool_names_reads_uncompacted_lingxi_and_claude_shapes() {
        let lingxi = user_with_content(
            "u1",
            json!([{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "",
                "is_error": false,
                "content_blocks": [
                    {"type": "tool_reference", "tool_name": "mcp__x__read"}
                ]
            }]),
        );
        let claude = user_with_content(
            "u2",
            json!([{
                "type": "tool_result",
                "tool_use_id": "toolu_2",
                "content": [
                    {"type": "tool_reference", "tool_name": "mcp__y__write"}
                ]
            }]),
        );
        assert_eq!(
            discovered_tool_names(&[lingxi, claude]),
            vec!["mcp__x__read".to_string(), "mcp__y__write".to_string()]
        );
    }

    #[test]
    fn discovered_tool_names_unions_history_with_compact_carry() {
        let history = user_with_content(
            "u1",
            json!([{
                "type": "tool_result",
                "content_blocks": [
                    {"type": "tool_reference", "tool_name": "Task"}
                ]
            }]),
        );
        assert_eq!(
            discovered_tool_names(&[boundary_line("b1", Some(&["WebFetch", "Task"])), history,]),
            vec!["Task".to_string(), "WebFetch".to_string()]
        );
    }

    fn make_fs(root: &Path) -> Arc<dyn FileSystem> {
        Arc::new(PosixFileSystem::new(root.to_path_buf()))
    }

    /// Build `<lingxi_home>/projects/<sanitize(cwd)>/` and return
    /// `(tempdir, lingxi_home, cwd, project_subdir)`.
    fn setup() -> (TempDir, PathBuf, String, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("workproj").to_string_lossy().into_owned();
        let lingxi_home = temp.path().join("home");
        let project_subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
        std::fs::create_dir_all(&project_subdir).expect("mkdir");
        (temp, lingxi_home, cwd, project_subdir)
    }

    /// Write one `<uuid>.jsonl` first-user-message session (the M5-07/M5-08
    /// on-disk shape), optionally tagging it `isSidechain` and/or `teamName`,
    /// then stamp its mtime. `prompt` becomes the row's extracted title.
    fn write_session(
        dir: &Path,
        cwd: &str,
        prompt: &str,
        mtime: SystemTime,
        is_sidechain: bool,
        team_name: Option<&str>,
    ) -> Uuid {
        let uuid = Uuid::new_v4();
        let mut line = serde_json::json!({
            "type": "user",
            "uuid": uuid.to_string(),
            "parentUuid": null,
            "sessionId": uuid.to_string(),
            "timestamp": chrono::DateTime::<chrono::Utc>::from(mtime)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": is_sidechain,
            "userType": "external",
            "message": {"role": "user", "content": prompt},
        });
        if let Some(team) = team_name {
            line["teamName"] = serde_json::Value::String(team.to_string());
        }
        let bytes = format!("{}\n", serde_json::to_string(&line).unwrap());
        let path = dir.join(format!("{uuid}.jsonl"));
        std::fs::write(&path, bytes).unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
        uuid
    }

    #[tokio::test]
    async fn excludes_sidechain_and_teamname_sessions() {
        let (temp, lingxi_home, cwd, dir) = setup();
        let base = SystemTime::now();
        // Newer mtimes for the hidden rows ensures they would have sorted FIRST
        // if not filtered — so a passing assertion proves the filter, not luck.
        let main = write_session(&dir, &cwd, "main prompt", base, false, None);
        let _sidechain = write_session(
            &dir,
            &cwd,
            "sub-agent transcript",
            base + Duration::from_secs(1),
            true,
            None,
        );
        let _team = write_session(
            &dir,
            &cwd,
            "team chat",
            base + Duration::from_secs(2),
            false,
            Some("squad"),
        );

        let fs = make_fs(temp.path());
        let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
            .await
            .expect("list");

        assert_eq!(rows.len(), 1, "sidechain + teamName sessions are hidden");
        assert_eq!(rows[0].uuid, main);
        assert_eq!(rows[0].title, "main prompt");
    }

    #[tokio::test]
    async fn empty_teamname_string_is_not_filtered() {
        // TS `if (enriched.teamName)` is a truthiness check — an empty-string
        // `teamName` is falsy and must NOT hide an otherwise-normal session.
        let (temp, lingxi_home, cwd, dir) = setup();
        let keep = write_session(&dir, &cwd, "kept", SystemTime::now(), false, Some(""));

        let fs = make_fs(temp.path());
        let rows = list_recent_sessions(&lingxi_home, &cwd, 5, fs)
            .await
            .expect("list");

        assert_eq!(rows.len(), 1, "empty teamName is falsy → not hidden");
        assert_eq!(rows[0].uuid, keep);
    }

    #[tokio::test]
    async fn all_sidechain_dir_is_empty_directory() {
        // If every candidate is a hidden sidechain, the picker has nothing to
        // show — same surface as a project dir with no `.jsonl` files.
        let (temp, lingxi_home, cwd, dir) = setup();
        let _ = write_session(&dir, &cwd, "sub a", SystemTime::now(), true, None);
        let _ = write_session(&dir, &cwd, "sub b", SystemTime::now(), true, None);

        let fs = make_fs(temp.path());
        match list_recent_sessions(&lingxi_home, &cwd, 5, fs).await {
            Err(LoaderError::EmptyDirectory) => {}
            other => panic!("expected EmptyDirectory, got {other:?}"),
        }
    }

    // ---- SESSION.5: cross-worktree resume --------------------------------

    /// Write one normal first-user-message session at `<dir>/<uuid>.jsonl` (the
    /// dir is created if missing), stamp its mtime, and return nothing — the
    /// caller supplies the `uuid` so the same session id can be planted in two
    /// worktree dirs to exercise dedupe.
    fn write_session_id(dir: &Path, uuid: Uuid, cwd: &str, prompt: &str, mtime: SystemTime) {
        std::fs::create_dir_all(dir).unwrap();
        let line = serde_json::json!({
            "type": "user",
            "uuid": uuid.to_string(),
            "parentUuid": null,
            "sessionId": uuid.to_string(),
            "timestamp": chrono::DateTime::<chrono::Utc>::from(mtime)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": prompt},
        });
        let bytes = format!("{}\n", serde_json::to_string(&line).unwrap());
        let path = dir.join(format!("{uuid}.jsonl"));
        std::fs::write(&path, bytes).unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
    }

    #[test]
    fn parses_worktree_list_porcelain() {
        // The porcelain example from getWorktreePaths.ts: keep `worktree ` lines,
        // strip the prefix; ignore HEAD/branch/blank lines.
        let stdout = "worktree /Users/foo/repo\n\
                      HEAD abc123\n\
                      branch refs/heads/main\n\
                      \n\
                      worktree /Users/foo/repo-wt1\n\
                      HEAD def456\n\
                      branch refs/heads/feature\n";
        assert_eq!(
            parse_worktree_list(stdout),
            vec![
                "/Users/foo/repo".to_string(),
                "/Users/foo/repo-wt1".to_string()
            ]
        );
        // No worktree lines → empty.
        assert!(parse_worktree_list("not a porcelain output\n").is_empty());
    }

    #[tokio::test]
    async fn current_scope_does_not_implicitly_include_sibling_worktrees() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/native/repo/sub";
        let sibling = "/native/sibling";
        let current_id = Uuid::new_v4();
        let sibling_id = Uuid::new_v4();
        let now = SystemTime::now();
        write_session_id(
            &project_dir_for_cwd(&home, current),
            current_id,
            current,
            "current",
            now,
        );
        write_session_id(
            &project_dir_for_cwd(&home, sibling),
            sibling_id,
            sibling,
            "sibling",
            now,
        );
        let worktrees = vec!["/native/repo".to_string(), sibling.to_string()];
        let fs = make_fs(temp.path());
        let current_rows = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Current,
            20,
            &fs,
            &worktrees,
        )
        .await
        .unwrap();
        assert_eq!(
            current_rows.iter().map(|row| row.uuid).collect::<Vec<_>>(),
            [current_id]
        );
        let repository_rows = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Repository,
            20,
            &fs,
            &worktrees,
        )
        .await
        .unwrap();
        assert_eq!(repository_rows.len(), 2);
    }

    #[tokio::test]
    async fn repository_scope_keeps_current_root_aliases_and_removed_managed_worktrees() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let root = "/native/repo";
        let current = "/native/repo/sub";
        let orphan = "/native/repo/.lingxi/worktrees/removed";
        let linked = "/native/linked";
        let now = SystemTime::now();
        let current_id = Uuid::new_v4();
        let root_id = Uuid::new_v4();
        let orphan_id = Uuid::new_v4();
        let linked_id = Uuid::new_v4();
        for (cwd, id) in [
            (current, current_id),
            (root, root_id),
            (orphan, orphan_id),
            (linked, linked_id),
        ] {
            write_session_id(&project_dir_for_cwd(&home, cwd), id, cwd, cwd, now);
        }
        std::fs::write(
            project_dir_for_cwd(&home, root).join(".session-aliases"),
            format!("{}\n", project_dir_for_cwd(&home, linked).display()),
        )
        .unwrap();
        let rows = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Repository,
            20,
            &make_fs(temp.path()),
            &[root.to_string()],
        )
        .await
        .unwrap();
        let ids: HashSet<_> = rows.iter().map(|row| row.uuid).collect();
        assert_eq!(ids, HashSet::from([current_id, orphan_id, linked_id]));
        assert!(
            !ids.contains(&root_id),
            "a single repository root bucket is not implicitly a subdirectory's current bucket"
        );
    }

    #[test]
    fn deduplication_preserves_native_input_order_for_equal_sort_keys() {
        let mut first = search_row(1, 10, "first", None);
        let mut second = search_row(2, 10, "second", None);
        first.created = SystemTime::UNIX_EPOCH;
        second.created = SystemTime::UNIX_EPOCH;
        let mut rows = deduplicate_by_session_id(vec![first.clone(), second, first]);
        rows.sort();
        assert_eq!(
            rows.iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    fn native_fixture_path(home: &Path, path: &str) -> PathBuf {
        if let Some(suffix) = path.strip_prefix("/oracle/.claude/") {
            home.join(suffix)
        } else if let Some(suffix) = path.strip_prefix("/linked/") {
            home.join("linked").join(suffix)
        } else {
            PathBuf::from(path)
        }
    }

    #[test]
    fn native_collision_probe_requires_a_real_recorded_directory() {
        let temp = TempDir::new().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let a = base.join("same_path");
        let b = base.join("same-path");
        let a = a.to_string_lossy();
        let b = b.to_string_lossy();
        assert!(
            !foreign_colliding_directory(&a, &b),
            "missing recorded cwd is unverifiable"
        );
        std::fs::create_dir(a.as_ref()).unwrap();
        assert!(
            foreign_colliding_directory(&a, &b),
            "a missing target retains its raw spelling"
        );
        std::fs::create_dir(b.as_ref()).unwrap();
        assert!(foreign_colliding_directory(&a, &b));
        assert!(belongs_to_worktree("/repo/child", &["/REPO".into()]));
        assert!(!belongs_to_worktree("/repository", &["/repo".into()]));
        for guarded in [
            "/net/host/a_b",
            "/Network/Servers/host/a_b",
            "/.vol/a_b",
            "\\\\host\\a_b",
        ] {
            assert!(guarded_directory_namespace(guarded, true), "{guarded}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_collision_route_allows_safe_local_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let target = base.join("same-path");
        let link = base.join("same_path");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(!guarded_directory_route(&link.to_string_lossy()));
        assert!(!foreign_colliding_directory(
            &link.to_string_lossy(),
            &target.to_string_lossy()
        ));
        let network_link = base.join("network-link");
        symlink("/net/host/workspace", &network_link).unwrap();
        assert!(guarded_directory_route(&network_link.to_string_lossy()));
    }

    #[tokio::test]
    async fn current_recovers_colliding_storage_by_each_buckets_newest_strict_cwd() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let cwd = base.join("project+name").to_string_lossy().into_owned();
        let other = base.join("project-name").to_string_lossy().into_owned();
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&other).unwrap();
        let current_bucket = project_dir_for_cwd(&home, &cwd);
        let recovered = home.join("projects/legacy-good");
        let rejected = home.join("projects/legacy-newest-is-other");
        let good_id = Uuid::new_v4();
        let now = SystemTime::now();
        write_session_id(&current_bucket, Uuid::new_v4(), &other, "foreign", now);
        write_session_id(&recovered, good_id, &cwd, "recovered", now);
        write_session_id(&rejected, Uuid::new_v4(), &cwd, "older own", now);
        write_session_id(
            &rejected,
            Uuid::new_v4(),
            &other,
            "newer foreign",
            now + Duration::from_secs(1),
        );
        let rows = list_recent_sessions_inner(
            &home,
            &cwd,
            SessionDiscoveryScope::Current,
            20,
            &make_fs(temp.path()),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.uuid).collect::<Vec<_>>(),
            [good_id]
        );
        assert_eq!(rows[0].workspace_path.as_deref(), Some(cwd.as_str()));
    }

    #[tokio::test]
    async fn native_current_zero_and_repository_bucket_limits_are_preserved() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/native/current";
        let current_bucket = project_dir_for_cwd(&home, current);
        let linked = project_dir_for_cwd(&home, "/native/linked");
        let orphan = project_dir_for_cwd(&home, "/native/current/.lingxi/worktrees/removed");
        let now = SystemTime::now();
        for directory in [&current_bucket, &linked, &orphan] {
            for index in 0..2 {
                write_session_id(
                    directory,
                    Uuid::new_v4(),
                    if directory == &current_bucket {
                        current
                    } else {
                        "/native/linked"
                    },
                    "session",
                    now + Duration::from_secs(index),
                );
            }
        }
        let fs = make_fs(temp.path());
        let without_alias =
            list_recent_sessions_inner(&home, current, SessionDiscoveryScope::Current, 0, &fs, &[])
                .await
                .unwrap();
        assert_eq!(without_alias.len(), 2);
        std::fs::write(
            current_bucket.join(".session-aliases"),
            format!("{}\n", linked.display()),
        )
        .unwrap();
        let with_alias =
            list_recent_sessions_inner(&home, current, SessionDiscoveryScope::Current, 0, &fs, &[])
                .await
                .unwrap();
        assert!(with_alias.is_empty());
        let repository = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Repository,
            1,
            &fs,
            &[current.to_string()],
        )
        .await
        .unwrap();
        assert_eq!(
            repository.len(),
            4,
            "current is unbounded, linked/orphan buckets each contribute one"
        );
    }

    #[tokio::test]
    async fn explicit_alias_is_not_filtered_as_a_current_directory_collision() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = temp.path().join("same-path").to_string_lossy().into_owned();
        let other = temp.path().join("same_path");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&other).unwrap();
        let alias = temp
            .path()
            .join("imported")
            .join(project_dir_name(&current));
        let current_bucket = project_dir_for_cwd(&home, &current);
        let own_id = Uuid::new_v4();
        let alias_id = Uuid::new_v4();
        write_session_id(&current_bucket, own_id, &current, "own", SystemTime::now());
        write_session_id(
            &alias,
            alias_id,
            &other.to_string_lossy(),
            "linked",
            SystemTime::now(),
        );
        std::fs::write(
            current_bucket.join(".session-aliases"),
            format!("{}\n", alias.display()),
        )
        .unwrap();
        let rows = list_recent_sessions_inner(
            &home,
            &current,
            SessionDiscoveryScope::Current,
            20,
            &make_fs(temp.path()),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.uuid).collect::<HashSet<_>>(),
            HashSet::from([own_id, alias_id])
        );
    }

    #[tokio::test]
    async fn picker_deduplicates_physical_mtime_before_logical_timestamp_enrichment() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/workspace/A";
        let sibling = "/workspace/B";
        let id = Uuid::new_v4();
        let current_bucket = project_dir_for_cwd(&home, current);
        let sibling_bucket = project_dir_for_cwd(&home, sibling);
        for (bucket, cwd, physical, logical) in [
            (&current_bucket, current, 100, 10),
            (&sibling_bucket, sibling, 90, 80),
        ] {
            let physical = SystemTime::UNIX_EPOCH + Duration::from_secs(physical);
            write_session_id(bucket, id, cwd, "copy", physical);
            let path = bucket.join(format!("{id}.jsonl"));
            let mut row: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            row["timestamp"] = json!(chrono::DateTime::<chrono::Utc>::from(
                SystemTime::UNIX_EPOCH + Duration::from_secs(logical),
            )
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            std::fs::write(&path, format!("{row}\n")).unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(physical))
                .unwrap();
        }
        std::fs::write(
            current_bucket.join(".session-aliases"),
            format!("{}\n", sibling_bucket.display()),
        )
        .unwrap();
        let roots = [current.to_string(), sibling.to_string()];
        for scope in [
            SessionDiscoveryScope::Current,
            SessionDiscoveryScope::Repository,
        ] {
            let rows = list_recent_sessions_inner(
                &home,
                current,
                scope,
                20,
                &make_fs(temp.path()),
                &roots,
            )
            .await
            .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].path, current_bucket.join(format!("{id}.jsonl")));
            assert_eq!(
                rows[0].modified,
                SystemTime::UNIX_EPOCH + Duration::from_secs(10)
            );
            let found =
                find_session_inner(&home, current, scope, id, &make_fs(temp.path()), &roots)
                    .await
                    .unwrap();
            assert_eq!(found.sessions[0].path, rows[0].path);
        }
        assert_eq!(
            select_session_path_across_worktrees(&home, current, id, &roots)
                .await
                .unwrap(),
            sibling_bucket.join(format!("{id}.jsonl")),
            "the explicit legacy union loader retains logical-activity selection",
        );
    }

    #[test]
    fn native_307_g9_fixture_uses_physical_mtime_and_original_identity() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap();
        for case in fixture["dedup_cases"].as_array().unwrap() {
            let candidates = case["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|row| {
                    let stem = row["session_id"]
                        .as_str()
                        .filter(|stem| crate::jsonl::validate_uuid(stem))?;
                    Some(CatalogCandidate {
                        uuid: Uuid::parse_str(stem).unwrap(),
                        path: PathBuf::from(row["tag"].as_str().unwrap_or(stem)),
                        stem: stem.to_string(),
                        file_len: 0,
                        file_modified: SystemTime::UNIX_EPOCH
                            + Duration::from_millis(row["modified_ms"].as_u64().unwrap()),
                        file_created: SystemTime::UNIX_EPOCH
                            + Duration::from_millis(row["created_ms"].as_u64().unwrap()),
                    })
                })
                .collect();
            let actual = deduplicate_catalog_candidates(candidates);
            let expected = case["expected"].as_array().unwrap();
            assert_eq!(actual.len(), expected.len(), "{}", case["name"]);
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(
                    actual.stem,
                    expected["session_id"].as_str().unwrap(),
                    "{}",
                    case["name"]
                );
                assert_eq!(
                    actual
                        .file_modified
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    u128::from(expected["modified_ms"].as_u64().unwrap())
                );
                assert_eq!(
                    actual
                        .file_created
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    u128::from(expected["created_ms"].as_u64().unwrap())
                );
                if let Some(tag) = expected["tag"].as_str() {
                    assert_eq!(actual.path, PathBuf::from(tag), "{}", case["name"]);
                }
            }
        }
    }

    #[tokio::test]
    async fn cli_flag_uses_current_full_parser_while_repository_picker_uses_newest_copy() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/workspace/A";
        let sibling = "/workspace/W";
        let id = Uuid::new_v4();
        let now = SystemTime::now();
        let current_bucket = project_dir_for_cwd(&home, current);
        write_session_id(&current_bucket, id, current, "current", now);
        write_session_id(
            &project_dir_for_cwd(&home, sibling),
            id,
            sibling,
            "newer sibling",
            now + Duration::from_secs(5),
        );
        let path = current_bucket.join(format!("{id}.jsonl"));
        let source = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"type\":\"user\"", "\"type\": \"user\"");
        std::fs::write(&path, &source).unwrap();
        let roots = [current.to_string(), sibling.to_string()];
        let selected = resolve_cli_transcript_source(&home, current, &id.to_string(), None, &roots)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.0, path);
        assert_eq!(selected.1, source);
        let rows = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Repository,
            20,
            &make_fs(temp.path()),
            &roots,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].path,
            project_dir_for_cwd(&home, sibling).join(format!("{id}.jsonl"))
        );
    }

    #[tokio::test]
    async fn cli_flag_does_not_prioritize_picker_aliases_over_global_ambiguity() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/workspace/A";
        let linked = project_dir_for_cwd(&home, "/workspace/B");
        let unrelated = project_dir_for_cwd(&home, "/workspace/C");
        let id = Uuid::new_v4();
        write_session_id(&linked, id, "/workspace/B", "linked", SystemTime::now());
        write_session_id(&unrelated, id, "/workspace/C", "other", SystemTime::now());
        let current_bucket = project_dir_for_cwd(&home, current);
        std::fs::create_dir_all(&current_bucket).unwrap();
        std::fs::write(
            current_bucket.join(".session-aliases"),
            format!("{}\n", linked.display()),
        )
        .unwrap();
        assert!(
            resolve_cli_transcript_source(&home, current, &id.to_string(), None, &[])
                .await
                .unwrap()
                .is_none()
        );
        let rows = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Current,
            20,
            &make_fs(temp.path()),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(rows[0].path, linked.join(format!("{id}.jsonl")));
    }

    #[tokio::test]
    async fn cli_repository_fallback_tries_the_containing_worktree_before_primary() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let primary = "/workspace/P";
        let linked = "/linked/W";
        let current = "/linked/W/subdirectory";
        let id = Uuid::new_v4();
        write_session_id(
            &project_dir_for_cwd(&home, primary),
            id,
            primary,
            "primary",
            SystemTime::now(),
        );
        let linked_bucket = project_dir_for_cwd(&home, linked);
        write_session_id(&linked_bucket, id, linked, "linked", SystemTime::now());
        let roots = [primary.to_string(), linked.to_string()];
        assert_eq!(cli_worktree_order(current, &roots), [linked, primary]);
        let selected = resolve_cli_transcript_source(&home, current, &id.to_string(), None, &roots)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.0, linked_bucket.join(format!("{id}.jsonl")));
    }

    #[tokio::test]
    async fn cli_reader_errors_propagate_without_repository_fallback() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/workspace/A";
        let sibling = "/workspace/W";
        let id = Uuid::new_v4();
        std::fs::create_dir_all(project_dir_for_cwd(&home, current).join(format!("{id}.jsonl")))
            .unwrap();
        write_session_id(
            &project_dir_for_cwd(&home, sibling),
            id,
            sibling,
            "usable",
            SystemTime::now(),
        );
        let result = resolve_cli_transcript_source(
            &home,
            current,
            &id.to_string(),
            None,
            &[current.to_string(), sibling.to_string()],
        )
        .await;
        assert!(matches!(result, Err(LoaderError::Io { .. })));
    }

    #[test]
    fn cli_worktree_sort_matches_native_locale_punctuation_and_case() {
        let roots = ["/p/foo-2", "/p/foo_2", "/p/A", "/p/a"].map(str::to_string);
        assert_eq!(
            cli_worktree_order("/unrelated", &roots),
            ["/p/a", "/p/A", "/p/foo_2", "/p/foo-2"]
        );
    }

    #[tokio::test]
    async fn native_307_job_uuid_fixture_preserves_priority_and_false_candidates() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap();
        let mut exercised = 0;
        for case in fixture["uuid_resolution_cases"].as_array().unwrap() {
            let input = &case["input"];
            let transcripts = input["transcripts"].as_array().cloned().unwrap_or_default();
            // Native's injected I/O and realpath results are covered by its VM
            // oracle. This replay exercises actual local files; uppercase
            // lookup is separate because macOS can use case-insensitive APFS.
            if input.get("realpath_result").is_some()
                || input.get("project_directory_error").is_some()
                || case["name"] == "uppercase_UUID_is_valid_but_filename_lookup_preserves_case"
                || transcripts.iter().any(|row| {
                    ["lstat_error", "open_error", "stream_error"]
                        .iter()
                        .any(|key| row.get(key).is_some())
                })
            {
                continue;
            }
            let temp = TempDir::new().unwrap();
            let home = temp.path().join("home");
            for directory in input["project_directories"].as_array().unwrap() {
                let path = home
                    .join("projects")
                    .join(directory["name"].as_str().unwrap());
                if directory["is_directory"].as_bool().unwrap() {
                    std::fs::create_dir_all(path).unwrap();
                } else {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "").unwrap();
                }
            }
            for row in transcripts {
                let path = native_fixture_path(&home, row["path"].as_str().unwrap());
                if row["kind"] == "directory" {
                    std::fs::create_dir_all(path).unwrap();
                } else {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, row["utf8"].as_str().unwrap()).unwrap();
                }
            }
            let roots = input["repository_paths"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|root| root.as_str().unwrap().to_string())
                .collect::<Vec<_>>();
            let options = JobTranscriptResolveOptions {
                link_scan_path: input["link_scan_path"]
                    .as_str()
                    .map(|path| native_fixture_path(&home, path)),
                cross_worktree: input["cross_worktree"].as_bool().unwrap_or(true),
            };
            let actual = resolve_job_transcript_raw_inner(
                &home,
                input["current_dir"].as_str().unwrap(),
                input["session_id"].as_str().unwrap(),
                &options,
                &roots,
            )
            .await
            .unwrap();
            let expected = &case["expected"]["resolution"];
            assert_eq!(
                actual.path,
                native_fixture_path(&home, expected["path"].as_str().unwrap()),
                "{}",
                case["name"]
            );
            assert_eq!(
                actual.has_messages,
                expected["hasMessages"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            exercised += 1;
        }
        assert!(exercised >= 18, "the native local-file UUID cases must run");
    }

    #[tokio::test]
    async fn native_307_kbn_fixture_verifies_regular_jsonl_head_and_tail() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap();
        let mut exercised = 0;
        for case in fixture["kbn_cases"].as_array().unwrap() {
            let input = &case["input"];
            let files = input["probe_files"].as_array().unwrap();
            if files
                .iter()
                .any(|file| file.get("open_error").is_some() || file.get("read_error").is_some())
            {
                continue;
            }
            assert_eq!(input["case_insensitive"], false);
            let temp = TempDir::new().unwrap();
            let home = temp.path().join("home");
            let directory = native_fixture_path(&home, input["project_dir"].as_str().unwrap());
            std::fs::create_dir_all(&directory).unwrap();
            for file in files {
                let path = native_fixture_path(&home, file["path"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                if file["kind"] == "directory" {
                    std::fs::create_dir_all(path).unwrap();
                } else if file["kind"] == "symlink" {
                    let target = temp.path().join("symlink-target");
                    std::fs::write(&target, file["utf8"].as_str().unwrap()).unwrap();
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(target, path).unwrap();
                    #[cfg(not(unix))]
                    continue;
                } else {
                    std::fs::write(path, file["utf8"].as_str().unwrap()).unwrap();
                }
            }
            assert_eq!(
                bucket_matches_worktree_root(&directory, input["target_root"].as_str().unwrap())
                    .await,
                case["expected"]["matches"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            exercised += 1;
        }
        assert_eq!(exercised, 15);
    }

    #[tokio::test]
    async fn truncated_repository_prefix_requires_native_cwd_verification() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let current = "/native/current";
        let root = format!("/native/{}", "a".repeat(220));
        let name = project_dir_name(&root);
        let prefix = &name[..MAX_SANITIZED_LENGTH];
        let good_bucket = home.join("projects").join(format!("{prefix}-old-good"));
        let bad_bucket = home.join("projects").join(format!("{prefix}-old-bad"));
        let current_id = Uuid::new_v4();
        let good_id = Uuid::new_v4();
        let bad_id = Uuid::new_v4();
        let now = SystemTime::now();
        write_session_id(
            &project_dir_for_cwd(&home, current),
            current_id,
            current,
            "current",
            now,
        );
        write_session_id(&good_bucket, good_id, &root, "verified", now);
        write_session_id(&bad_bucket, bad_id, &format!("{root}x"), "wrong root", now);
        let roots = [current.to_string(), root.clone()];
        let rows = list_recent_sessions_inner(
            &home,
            current,
            SessionDiscoveryScope::Repository,
            20,
            &make_fs(temp.path()),
            &roots,
        )
        .await
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.uuid).collect::<HashSet<_>>(),
            HashSet::from([current_id, good_id]),
        );
        // Kbn probes every regular .jsonl filename, not only UUID transcripts.
        std::fs::write(
            bad_bucket.join("not-a-uuid.jsonl"),
            format!("{}\n", json!({"type":"relocated", "relocatedCwd":root})),
        )
        .unwrap();
        assert!(bucket_matches_worktree_root(&bad_bucket, &root).await);
        let recovered = project_directories_for_cwd(&home, &root).await;
        assert!(recovered.contains(&good_bucket));
        assert!(recovered.contains(&bad_bucket));
        let current_rows = list_recent_sessions_inner(
            &home,
            &root,
            SessionDiscoveryScope::Current,
            20,
            &make_fs(temp.path()),
            &[],
        )
        .await
        .unwrap();
        assert!(
            !current_rows.is_empty(),
            "rRe recovers a verified legacy suffix for Current"
        );
    }

    #[tokio::test]
    async fn repository_root_at_200_units_has_no_generic_subdirectory_prefix() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let root = format!("/{}", "a".repeat(199));
        assert_eq!(project_dir_name(&root).len(), MAX_SANITIZED_LENGTH);
        let child = home
            .join("projects")
            .join(format!("{}-child", project_dir_name(&root)));
        std::fs::create_dir_all(&child).unwrap();
        let mut selected = Vec::new();
        append_repository_project_dirs(&home.join("projects"), &mut selected, &[root])
            .await
            .unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn worktree_dir_match_rule() {
        // `dirName === prefix` and `dirName.startsWith(prefix + '-')` match…
        assert!(worktree_dir_matches("-x-repo", "-x-repo")); // exact
        assert!(worktree_dir_matches("-x-repo-sub", "-x-repo")); // subdir (prefix + '-')
                                                                 // …but a bare prefix-extension (no `-` boundary) must NOT match.
        assert!(!worktree_dir_matches("-x-repository", "-x-repo"));
        assert!(!worktree_dir_matches("-y-other", "-x-repo"));
    }

    #[test]
    fn git_unavailable_returns_no_worktrees() {
        // A throwaway dir that is not a git repo → empty (git non-zero / errors
        // out / single worktree all collapse to the same "behave as before").
        let temp = TempDir::new().unwrap();
        assert!(git_worktree_paths(&temp.path().to_string_lossy()).is_empty());
    }

    #[tokio::test]
    async fn single_worktree_scans_only_cwd_dir() {
        // `list_recent_sessions_inner` with an EMPTY worktree slice must behave
        // exactly like the pre-SESSION.5 single-dir scan: only the cwd's project
        // dir is consulted, sibling dirs are ignored.
        let (temp, lingxi_home, cwd, dir) = setup();
        let base = SystemTime::now();
        let main = write_session(&dir, &cwd, "main", base, false, None);

        // A sibling worktree dir exists on disk but must be invisible here.
        let sibling = lingxi_home
            .join("projects")
            .join(project_dir_name("/other/wt"));
        let _hidden = {
            let u = Uuid::new_v4();
            write_session_id(&sibling, u, "/other/wt", "sibling", base);
            u
        };

        let fs = make_fs(temp.path());
        let rows = list_recent_sessions_inner(
            &lingxi_home,
            &cwd,
            SessionDiscoveryScope::Current,
            5,
            &fs,
            &[],
        )
        .await
        .expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uuid, main);
    }

    #[tokio::test]
    async fn find_session_returns_the_catalog_row_without_its_siblings() {
        let (temp, lingxi_home, cwd, dir) = setup();
        let base = SystemTime::now();
        let older = write_session(&dir, &cwd, "older", base, false, None);
        let newer = write_session(
            &dir,
            &cwd,
            "newer",
            base + std::time::Duration::from_secs(5),
            false,
            None,
        );
        let fs = make_fs(temp.path());
        let full = list_recent_sessions_inner(
            &lingxi_home,
            &cwd,
            SessionDiscoveryScope::Current,
            5,
            &fs,
            &[],
        )
        .await
        .expect("list");
        assert_eq!(full.len(), 2);

        for id in [older, newer] {
            let found = find_session_inner(
                &lingxi_home,
                &cwd,
                SessionDiscoveryScope::Current,
                id,
                &fs,
                &[],
            )
            .await
            .expect("find");
            let expected = full.iter().find(|row| row.uuid == id).expect("listed");
            assert_eq!(found.sessions.len(), 1);
            assert_eq!(found.sessions[0].uuid, expected.uuid);
            assert_eq!(found.sessions[0].title, expected.title);
            assert_eq!(found.sessions[0].message_count, expected.message_count);
            assert_eq!(found.sessions[0].path, expected.path);
        }

        // A session without a row answers as the full catalog does: an empty
        // listing while other sessions are readable, even when the session
        // itself is a skipped, unreadable transcript...
        let corrupt = Uuid::new_v4();
        std::fs::write(
            dir.join(format!("{corrupt}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"{corrupt}\",\"cwd\":\"{cwd}\",\"uuid\":\n"
            ),
        )
        .unwrap();
        for id in [corrupt, Uuid::new_v4()] {
            let found = find_session_inner(
                &lingxi_home,
                &cwd,
                SessionDiscoveryScope::Current,
                id,
                &fs,
                &[],
            )
            .await
            .expect("other sessions are readable");
            assert!(found.sessions.is_empty());
        }
        // ...and the catalog's own error once nothing is readable.
        std::fs::remove_file(dir.join(format!("{older}.jsonl"))).unwrap();
        std::fs::remove_file(dir.join(format!("{newer}.jsonl"))).unwrap();
        let full = list_recent_sessions_inner_with_diagnostics(
            &lingxi_home,
            &cwd,
            SessionDiscoveryScope::Current,
            5,
            &fs,
            &[],
            None,
        )
        .await;
        let found = find_session_inner(
            &lingxi_home,
            &cwd,
            SessionDiscoveryScope::Current,
            Uuid::new_v4(),
            &fs,
            &[],
        )
        .await;
        assert!(matches!(full, Err(LoaderError::Io { .. })));
        assert!(matches!(found, Err(LoaderError::Io { .. })));
    }

    #[tokio::test]
    async fn relocated_tail_overrides_colliding_first_line_cwd() {
        let (temp, lingxi_home, cwd, dir) = setup();
        let id = Uuid::new_v4();
        // Replacing one path separator with another non-alphanumeric byte
        // preserves Claude's sanitized project-dir key while changing cwd.
        let old_cwd = cwd.replacen(std::path::MAIN_SEPARATOR, "-", 1);
        assert_ne!(old_cwd, cwd);
        assert_eq!(project_dir_name(&old_cwd), project_dir_name(&cwd));
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let user = serde_json::json!({
            "type": "user",
            "uuid": id.to_string(),
            "parentUuid": null,
            "sessionId": id.to_string(),
            "timestamp": timestamp,
            "cwd": old_cwd,
            "message": {"role": "user", "content": "relocated session"}
        });
        let relocated = serde_json::json!({
            "type": "relocated",
            "sessionId": id.to_string(),
            "relocatedCwd": cwd
        });
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{}\n{}\n", user, relocated),
        )
        .unwrap();

        let rows = list_recent_sessions_inner(
            &lingxi_home,
            &cwd,
            SessionDiscoveryScope::Current,
            5,
            &make_fs(temp.path()),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uuid, id);
    }

    #[tokio::test]
    async fn resume_modified_time_clamps_last_message_to_file_mtime() {
        let (temp, lingxi_home, cwd, dir) = setup();
        let base = SystemTime::now();
        let clamped = write_session(
            &dir,
            &cwd,
            "future transcript",
            base + Duration::from_secs(100),
            false,
            None,
        );
        filetime::set_file_mtime(
            dir.join(format!("{clamped}.jsonl")),
            filetime::FileTime::from_system_time(base + Duration::from_secs(10)),
        )
        .unwrap();
        let newer = write_session(
            &dir,
            &cwd,
            "newer activity",
            base + Duration::from_secs(20),
            false,
            None,
        );

        let rows = list_recent_sessions(&lingxi_home, &cwd, 5, make_fs(temp.path()))
            .await
            .unwrap();
        assert_eq!(rows[0].uuid, newer);
        assert_eq!(rows[1].uuid, clamped);
    }

    #[tokio::test]
    async fn includes_sibling_worktree_sessions_deduped() {
        let temp = TempDir::new().unwrap();
        let lingxi_home = temp.path().join("home");
        let projects = lingxi_home.join("projects");
        std::fs::create_dir_all(&projects).unwrap();

        let wt_a = "/wt/alpha";
        let wt_b = "/wt/beta";
        let prefix_a = project_dir_name(wt_a); // "-wt-alpha"
        let prefix_b = project_dir_name(wt_b); // "-wt-beta"

        let base = SystemTime::now();

        // Exact-prefix match for worktree A.
        let dir_a = projects.join(&prefix_a);
        let a_root = Uuid::new_v4();
        write_session_id(&dir_a, a_root, wt_a, "alpha-root", base);

        // Subdir of A → matched via startsWith(prefix + '-').
        let dir_a_sub = projects.join(project_dir_name("/wt/alpha/sub"));
        let a_sub = Uuid::new_v4();
        write_session_id(
            &dir_a_sub,
            a_sub,
            "/wt/alpha/sub",
            "alpha-sub",
            base + Duration::from_secs(1),
        );

        // Exact-prefix match for worktree B.
        let dir_b = projects.join(&prefix_b);
        let b_root = Uuid::new_v4();
        write_session_id(
            &dir_b,
            b_root,
            wt_b,
            "beta-root",
            base + Duration::from_secs(2),
        );

        // Boundary guard: "-wt-alphax" starts with prefix_a but the next char is
        // not '-', so it must be EXCLUDED.
        let dir_boundary = projects.join(format!("{prefix_a}x"));
        let ghost_boundary = Uuid::new_v4();
        write_session_id(
            &dir_boundary,
            ghost_boundary,
            "/wt/alphax",
            "ghost-boundary",
            base + Duration::from_secs(3),
        );

        // Unrelated dir → EXCLUDED.
        let dir_other = projects.join(project_dir_name("/some/other"));
        let ghost_other = Uuid::new_v4();
        write_session_id(
            &dir_other,
            ghost_other,
            "/some/other",
            "ghost-other",
            base + Duration::from_secs(4),
        );

        // Same session id under BOTH worktree dirs, different mtimes → dedupe
        // keeps the newest ("dup-new").
        let dup = Uuid::new_v4();
        write_session_id(&dir_a, dup, wt_a, "dup-old", base);
        write_session_id(&dir_b, dup, wt_b, "dup-new", base + Duration::from_secs(10));

        let fs = make_fs(temp.path());
        let worktrees = vec![wt_a.to_string(), wt_b.to_string()];
        let rows = list_recent_sessions_inner(
            &lingxi_home,
            wt_a,
            SessionDiscoveryScope::Repository,
            50,
            &fs,
            &worktrees,
        )
        .await
        .expect("list");

        let ids: std::collections::HashSet<Uuid> = rows.iter().map(|r| r.uuid).collect();
        assert!(ids.contains(&a_root), "alpha-root included (exact prefix)");
        assert!(ids.contains(&a_sub), "alpha-sub included (prefix + '-')");
        assert!(
            ids.contains(&b_root),
            "beta-root included (sibling worktree)"
        );
        assert!(ids.contains(&dup), "dup session present");
        assert!(
            !ids.contains(&ghost_boundary),
            "boundary dir excluded (no '-' after prefix)"
        );
        assert!(!ids.contains(&ghost_other), "unrelated dir excluded");

        // Dedupe by id: exactly one dup row, and it is the newer one.
        let dup_rows: Vec<_> = rows.iter().filter(|r| r.uuid == dup).collect();
        assert_eq!(
            dup_rows.len(),
            1,
            "dedupe collapses the duplicate session id"
        );
        assert_eq!(
            dup_rows[0].title, "dup-new",
            "dedupe keeps the newest mtime"
        );

        let loaded =
            load_session_across_worktrees_inner(&lingxi_home, wt_a, dup, fs.clone(), &worktrees)
                .await
                .expect("the catalog-selected duplicate must also load");
        assert_eq!(
            loaded[0].message.string_at("/content"),
            Some("dup-new"),
            "discovery and loading must use the same duplicate rule"
        );

        // Distinct surviving sessions: a_root, a_sub, b_root, dup.
        assert_eq!(rows.len(), 4);
    }

    #[tokio::test]
    async fn loads_a_session_selected_from_a_sibling_worktree() {
        let temp = TempDir::new().unwrap();
        let lingxi_home = temp.path().join("home");
        let wt_a = "/wt/alpha";
        let wt_b = "/wt/beta";
        let sibling_dir = lingxi_home.join("projects").join(project_dir_name(wt_b));
        let id = Uuid::new_v4();
        write_session_id(
            &sibling_dir,
            id,
            wt_b,
            "sibling transcript",
            SystemTime::now(),
        );

        let fs = make_fs(temp.path());
        let worktrees = vec![wt_a.to_string(), wt_b.to_string()];
        let messages = load_session_across_worktrees_inner(&lingxi_home, wt_a, id, fs, &worktrees)
            .await
            .expect("a row discovered in a sibling worktree must also load");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, id.to_string());
        assert_eq!(
            messages[0].message.string_at("/content"),
            Some("sibling transcript")
        );

        let entries = load_session_entries_across_worktrees_inner(
            &lingxi_home,
            wt_a,
            id,
            make_fs(temp.path()),
            &worktrees,
        )
        .await
        .expect("raw sibling transcript must use the same selected path");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].message.string_at("/content"),
            messages[0].message.string_at("/content")
        );
    }

    #[tokio::test]
    async fn repository_picker_and_legacy_loader_choose_their_own_activity_winners() {
        let temp = TempDir::new().unwrap();
        let lingxi_home = temp.path().join("home");
        let wt_a = "/wt/alpha";
        let wt_b = "/wt/beta";
        let dir_a = lingxi_home.join("projects").join(project_dir_name(wt_a));
        let dir_b = lingxi_home.join("projects").join(project_dir_name(wt_b));
        let id = Uuid::new_v4();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(100);

        write_session_id(
            &dir_a,
            id,
            wt_a,
            "older logical",
            base + Duration::from_secs(10),
        );
        filetime::set_file_mtime(
            dir_a.join(format!("{id}.jsonl")),
            filetime::FileTime::from_system_time(base + Duration::from_secs(30)),
        )
        .unwrap();
        write_session_id(
            &dir_b,
            id,
            wt_b,
            "newer logical",
            base + Duration::from_secs(20),
        );

        let worktrees = vec![wt_a.to_string(), wt_b.to_string()];
        let rows = list_recent_sessions_inner(
            &lingxi_home,
            wt_a,
            SessionDiscoveryScope::Repository,
            10,
            &make_fs(temp.path()),
            &worktrees,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, dir_a.join(format!("{id}.jsonl")));
        assert_eq!(rows[0].title, "older logical");
        assert_eq!(rows[0].modified, base + Duration::from_secs(10));
        assert_eq!(
            select_session_path_across_worktrees(&lingxi_home, wt_a, id, &worktrees)
                .await
                .unwrap(),
            dir_b.join(format!("{id}.jsonl")),
            "the explicit legacy resolver compares logical activity after the picker has selected by physical mtime",
        );

        let loaded = load_session_across_worktrees_inner(
            &lingxi_home,
            wt_a,
            id,
            make_fs(temp.path()),
            &worktrees,
        )
        .await
        .unwrap();
        assert_eq!(
            loaded[0].message.string_at("/content"),
            Some("newer logical")
        );
        let entries = load_session_entries_across_worktrees_inner(
            &lingxi_home,
            wt_a,
            id,
            make_fs(temp.path()),
            &worktrees,
        )
        .await
        .unwrap();
        assert_eq!(
            entries[0].message.string_at("/content"),
            Some("newer logical")
        );
    }

    #[tokio::test]
    async fn multi_worktree_empty_match_is_empty_directory() {
        // > 1 worktrees but no projects-root subdir matches any prefix → the
        // picker has nothing to resume, surfaced as EmptyDirectory (same as the
        // single-dir empty case).
        let temp = TempDir::new().unwrap();
        let lingxi_home = temp.path().join("home");
        std::fs::create_dir_all(lingxi_home.join("projects")).unwrap();

        let fs = make_fs(temp.path());
        let worktrees = vec!["/wt/alpha".to_string(), "/wt/beta".to_string()];
        match list_recent_sessions_inner(
            &lingxi_home,
            "/wt/alpha",
            SessionDiscoveryScope::Repository,
            5,
            &fs,
            &worktrees,
        )
        .await
        {
            Err(LoaderError::EmptyDirectory) => {}
            other => panic!("expected EmptyDirectory, got {other:?}"),
        }
    }

    // ---- SESSION.6: equal-mtime tie-break by `created` (birthtime) DESC -----

    #[test]
    fn ord_tiebreak_prefers_newer_created() {
        // Equal `modified` → the row with the NEWER `created` (birthtime) sorts
        // first, mirroring claude-code `sortLogs`'s created-DESC tie-break
        // (`types/logs.ts:327-328`).
        let same_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let older = SessionMetadata {
            uuid: Uuid::from_u128(1),
            title: "older-created".into(),
            modified: same_mtime,
            created: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            message_count: 1,
            mode: SessionMode::Code,
            path: PathBuf::from("z.jsonl"),
            workspace_path: None,
            pr_number: None,
            custom_or_ai_title: None,
            resume_model: None,
            resume_model_profile: None,
        };
        let newer = SessionMetadata {
            uuid: Uuid::from_u128(2),
            title: "newer-created".into(),
            modified: same_mtime,
            created: SystemTime::UNIX_EPOCH + Duration::from_secs(200),
            message_count: 1,
            mode: SessionMode::Code,
            path: PathBuf::from("a.jsonl"),
            workspace_path: None,
            pr_number: None,
            custom_or_ai_title: None,
            resume_model: None,
            resume_model_profile: None,
        };
        // Insert oldest-created first to prove the sort (not insertion order)
        // drives the result.
        let mut v = [older, newer];
        v.sort();
        assert_eq!(v[0].title, "newer-created", "newer birthtime sorts first");
        assert_eq!(v[1].title, "older-created");
    }

    /// (parity 2.1.212) `read_worktree_state` returns the persisted active
    /// worktree session on resume, and `None` once an `ExitWorktree` clear
    /// record (`worktreeSession: null`) supersedes it — the read side that lets
    /// `ExitWorktree` operate after `--continue`/`--resume` instead of no-oping.
    #[tokio::test]
    async fn read_worktree_state_returns_active_then_none_after_clear() {
        let temp = TempDir::new().expect("tempdir");
        let session_id = "44444444-5555-6666-7777-888888888888";
        let path = temp.path().join(format!("{session_id}.jsonl"));
        let fs = make_fs(temp.path());
        let writer = crate::jsonl::writer::JsonlWriter::new(path.clone(), fs.clone());

        // Absent transcript → None.
        assert!(read_worktree_state(&path, fs.clone(), session_id)
            .await
            .is_none());

        // After EnterWorktree persists an active session → Some(payload).
        let payload = serde_json::json!({
            "worktreePath": "/repo/.lingxi/worktrees/feat",
            "originalCwd": "/repo",
            "worktreeBranch": "worktree-feat",
            "enteredExisting": false,
        });
        writer
            .append_worktree_state(session_id, Some(&payload))
            .await
            .expect("persist active");
        let got = read_worktree_state(&path, fs.clone(), session_id)
            .await
            .expect("active worktree restored");
        assert_eq!(got["worktreePath"], "/repo/.lingxi/worktrees/feat");

        // After ExitWorktree clears it (null) → None (last-write-wins).
        writer
            .append_worktree_state(session_id, None)
            .await
            .expect("persist clear");
        assert!(
            read_worktree_state(&path, fs, session_id).await.is_none(),
            "cleared worktree state resolves to None"
        );
    }

    fn search_row(id: u128, secs: u64, display: &str, searchable: Option<&str>) -> SessionMetadata {
        SessionMetadata {
            uuid: Uuid::from_u128(id),
            title: display.to_string(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            created: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            message_count: 1,
            mode: SessionMode::Code,
            path: PathBuf::from(format!("{id}.jsonl")),
            workspace_path: None,
            pr_number: None,
            custom_or_ai_title: searchable.map(str::to_string),
            resume_model: None,
            resume_model_profile: None,
        }
    }

    /// `OEe` lowercases AND trims both sides before comparing.
    #[test]
    fn title_search_is_case_and_whitespace_insensitive() {
        let rows = vec![search_row(1, 100, "d", Some("  Ship The Parser  "))];
        let hit = search_sessions_by_custom_title(rows, "\tship the parser ", true, None);
        assert_eq!(
            hit.len(),
            1,
            "case + surrounding whitespace must not matter"
        );
    }

    /// The search source is `customTitle ?? aiTitle` — NOT the merged display
    /// title, which also folds in agentName/summary/first-user-message. Matching
    /// the display title would resume a session because its SUMMARY contained
    /// the query, which the oracle never does.
    #[test]
    fn title_search_ignores_the_merged_display_title() {
        let rows = vec![search_row(1, 100, "refactor the loader", None)];
        assert!(
            search_sessions_by_custom_title(rows, "refactor the loader", true, None).is_empty(),
            "a row with no custom/ai title must not match on its display title"
        );
    }

    /// `if (!p) return !1` — an untitled row is skipped outright, so an empty
    /// query cannot sweep up every session.
    #[test]
    fn title_search_skips_untitled_rows_even_for_an_empty_query() {
        let rows = vec![
            search_row(1, 100, "d", None),
            search_row(2, 100, "d", Some("")),
        ];
        assert!(search_sessions_by_custom_title(rows, "", false, None).is_empty());
    }

    /// `exact` is what `--resume` passes; substring is the completer's mode.
    #[test]
    fn title_search_exact_rejects_a_substring_that_inexact_accepts() {
        let rows = vec![search_row(1, 100, "d", Some("ship the parser"))];
        assert!(search_sessions_by_custom_title(rows.clone(), "parser", true, None).is_empty());
        assert_eq!(
            search_sessions_by_custom_title(rows, "parser", false, None).len(),
            1
        );
    }

    /// Sorted `modified` DESC, then `slice(limit)` — the disambiguation listing
    /// and the completer both depend on newest-first.
    #[test]
    fn title_search_sorts_newest_first_then_limits() {
        let rows = vec![
            search_row(1, 100, "d", Some("dup")),
            search_row(3, 300, "d", Some("dup")),
            search_row(2, 200, "d", Some("dup")),
        ];
        let hits = search_sessions_by_custom_title(rows, "dup", true, Some(2));
        assert_eq!(
            hits.iter().map(|r| r.uuid.as_u128()).collect::<Vec<_>>(),
            vec![3, 2],
            "newest first, then truncated to the limit"
        );
    }

    /// The restore readers route only the rows that can carry their type, so
    /// they must agree with a full route, even when a writer escapes a type.
    #[test]
    fn typed_row_routing_matches_a_full_route_with_escaped_types() {
        let content = [
            "\u{FEFF}{\"type\":\"agent-setting\",\"agentSetting\":\"bom\",\"sessionId\":\"b\"}",
            r#"{"type":"user","uuid":"u1","sessionId":"s","message":{"content":"agent-setting"}}"#,
            r#"{"type":"agent-setting","agentSetting":"first","sessionId":"s"}"#,
            r#"{"type":"worktree-state","worktreeSession":{"worktreePath":"/a"},"sessionId":"s"}"#,
            r#"{"type":"user","uuid":"u2","sessionId":"s","message":{"content":"\u001b[31mred"}}"#,
            r#"{"type":"agent\u002Dsetting","agentSetting":"escaped","sessionId":"s"}"#,
            r#"{"t\u0079pe":"agent-setting","agentSetting":"key","sessionId":"k"}"#,
            "\u{FEFF}{\"type\":\"agent-setting\",\"agentSetting\":\"inner-bom\",\"sessionId\":\"i\"}",
            "{\"type\":\"agent-setting\",\"agentSetting\":\"crlf\",\"sessionId\":\"c\"}\r",
            "not json",
            "{\"type\":\"worktree\\u002dstate\",\"worktreeSession\":null,\"sessionId\":\"s\"}\r",
        ]
        .join("\n");
        let full = route_lines(&content);
        let agents = route_rows_of_types(&content, &["agent-setting"]);
        assert_eq!(agents.agent_settings, full.agent_settings);
        assert_eq!(agents.agent_snapshots, full.agent_snapshots);
        assert_eq!(agents.agent_settings["s"], json!("escaped"));
        assert_eq!(agents.agent_settings["b"], json!("bom"));
        assert_eq!(agents.agent_settings["c"], json!("crlf"));
        assert_eq!(
            agents.agent_settings.get("i"),
            Some(&json!("inner-bom")),
            "Native X5e trims a BOM at each JSONL row boundary"
        );
        let worktrees = route_rows_of_types(&content, &["worktree-state"]);
        assert_eq!(worktrees.worktree_states, full.worktree_states);
        assert_eq!(worktrees.worktree_states["s"], Value::Null);
    }
}
