//! Rollout filename / path parsing + a lightweight thread-metadata builder.
//!
//! Faithful port of codex's filename helpers (`parse_rollout_file_name`,
//! `parse_timestamp_uuid_from_filename`, `rollout_date_parts`,
//! `plain_rollout_path`) and `metadata::builder_from_items`. Codex's full
//! metadata path drives a SQLite `state_db`; LingXi has no such store, so the
//! builder produces a small in-memory [`ThreadMetadata`] derived from the
//! session-meta line or the filename — enough to inspect/resume a rollout.

use crate::rollout::record::{RolloutItem, SessionSource, ThreadId};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const COMPRESSED_SUFFIX: &str = ".zst";
/// Filename timestamp layout: `YYYY-MM-DDThh-mm-ss` (colons replaced with
/// dashes so the name is valid on case-insensitive / colon-hostile FS).
const FILENAME_TS_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

/// Returns the canonical `.jsonl` name for a plain or `.jsonl.zst` rollout
/// name, or `None` for non-rollout names.
pub(crate) fn parse_rollout_file_name(name: &str) -> Option<&str> {
    let name = name.strip_suffix(COMPRESSED_SUFFIX).unwrap_or(name);
    if name.starts_with("rollout-") && name.ends_with(".jsonl") {
        Some(name)
    } else {
        None
    }
}

/// Returns the plain `.jsonl` path for a plain or compressed rollout path.
#[must_use]
pub fn plain_rollout_path(path: &Path) -> PathBuf {
    match path.file_name().and_then(OsStr::to_str) {
        Some(name) if name.ends_with(COMPRESSED_SUFFIX) => {
            let plain = name.trim_end_matches(COMPRESSED_SUFFIX);
            path.with_file_name(plain)
        }
        _ => path.to_path_buf(),
    }
}

/// Parse a `rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl[.zst]` filename into its
/// creation timestamp and thread UUID. Scans from the right for the `-` that
/// makes the suffix a valid UUID (UUIDs themselves contain dashes).
#[must_use]
pub fn parse_timestamp_uuid_from_filename(name: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let name = parse_rollout_file_name(name)?;
    let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;

    let (sep_idx, uuid) = core
        .match_indices('-')
        .rev()
        .find_map(|(i, _)| Uuid::parse_str(&core[i + 1..]).ok().map(|u| (i, u)))?;

    let ts_str = &core[..sep_idx];
    let naive = NaiveDateTime::parse_from_str(ts_str, FILENAME_TS_FORMAT).ok()?;
    let ts = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
    Some((ts, uuid))
}

/// Extract `(year, month, day)` directory components from a rollout filename.
#[must_use]
pub fn rollout_date_parts(file_name: &OsStr) -> Option<(String, String, String)> {
    let name = file_name.to_string_lossy();
    let date = name.strip_prefix("rollout-")?.get(..10)?;
    let year = date.get(..4)?.to_string();
    let month = date.get(5..7)?.to_string();
    let day = date.get(8..10)?.to_string();
    Some((year, month, day))
}

/// Lightweight thread metadata derived from a rollout — the LingXi analog of
/// codex's `ThreadMetadataBuilder`/`ThreadMetadata` (which back a SQLite row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMetadata {
    pub id: ThreadId,
    pub rollout_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub source: SessionSource,
    pub model_provider: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub cwd: PathBuf,
    pub cli_version: Option<String>,
    pub git_sha: Option<String>,
    pub git_branch: Option<String>,
    pub git_origin_url: Option<String>,
}

/// Builder for [`ThreadMetadata`], mirroring codex's `ThreadMetadataBuilder`.
#[derive(Debug, Clone)]
pub struct ThreadMetadataBuilder {
    pub id: ThreadId,
    pub rollout_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub source: SessionSource,
    pub model_provider: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub agent_path: Option<String>,
    pub cwd: PathBuf,
    pub cli_version: Option<String>,
    pub git_sha: Option<String>,
    pub git_branch: Option<String>,
    pub git_origin_url: Option<String>,
}

impl ThreadMetadataBuilder {
    #[must_use]
    pub fn new(
        id: ThreadId,
        rollout_path: PathBuf,
        created_at: DateTime<Utc>,
        source: SessionSource,
    ) -> Self {
        Self {
            id,
            rollout_path,
            created_at,
            source,
            model_provider: None,
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            cwd: PathBuf::new(),
            cli_version: None,
            git_sha: None,
            git_branch: None,
            git_origin_url: None,
        }
    }

    /// Finalize, filling the model provider from a default when unset.
    #[must_use]
    pub fn build(self, default_provider: &str) -> ThreadMetadata {
        ThreadMetadata {
            id: self.id,
            rollout_path: self.rollout_path,
            created_at: self.created_at,
            source: self.source,
            model_provider: Some(
                self.model_provider
                    .unwrap_or_else(|| default_provider.to_string()),
            ),
            agent_nickname: self.agent_nickname,
            agent_role: self.agent_role,
            cwd: self.cwd,
            cli_version: self.cli_version,
            git_sha: self.git_sha,
            git_branch: self.git_branch,
            git_origin_url: self.git_origin_url,
        }
    }
}

/// Build a [`ThreadMetadataBuilder`] from a session-meta line in `items`,
/// falling back to filename-derived id/timestamp. Faithful port of codex's
/// `metadata::builder_from_items` + `builder_from_session_meta`.
#[must_use]
pub fn builder_from_items(
    items: &[RolloutItem],
    rollout_path: &Path,
) -> Option<ThreadMetadataBuilder> {
    if let Some(meta_line) = items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line),
        _ => None,
    }) {
        let created_at = parse_timestamp_to_utc(meta_line.meta.timestamp.as_str())?;
        let mut builder = ThreadMetadataBuilder::new(
            meta_line.meta.id,
            rollout_path.to_path_buf(),
            created_at,
            meta_line.meta.source.clone(),
        );
        builder.model_provider = meta_line.meta.model_provider.clone();
        builder.agent_nickname = meta_line.meta.agent_nickname.clone();
        builder.agent_role = meta_line.meta.agent_role.clone();
        builder.agent_path = meta_line.meta.agent_path.clone();
        builder.cwd = meta_line.meta.cwd.clone();
        builder.cli_version = Some(meta_line.meta.cli_version.clone());
        if let Some(git) = meta_line.git.as_ref() {
            builder.git_sha = git.commit_hash.clone();
            builder.git_branch = git.branch.clone();
            builder.git_origin_url = git.repository_url.clone();
        }
        return Some(builder);
    }

    let file_name = rollout_path.file_name()?.to_str()?;
    let (created_at, uuid) = parse_timestamp_uuid_from_filename(file_name)?;
    Some(ThreadMetadataBuilder::new(
        ThreadId::from_uuid(uuid),
        rollout_path.to_path_buf(),
        created_at,
        SessionSource::default(),
    ))
}

/// Parse a session-meta `timestamp` (filename format or RFC3339) to UTC.
fn parse_timestamp_to_utc(ts: &str) -> Option<DateTime<Utc>> {
    if let Ok(naive) = NaiveDateTime::parse_from_str(ts, FILENAME_TS_FORMAT) {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&Utc));
    }
    None
}
