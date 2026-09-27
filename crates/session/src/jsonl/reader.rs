//! JSONL reader — full parse + lite head-only metadata.
//!
//! Lite read mirrors `claude-code/src/utils/sessionStoragePortable.ts:215-282`
//! (`readSessionLite` head path) — we only need the head because the fields
//! we extract (`sessionId`, `cwd`, `type`) live on line 1.

use crate::jsonl::schema::JsonlMessage;
use platform_api::{FileSystem, FsError};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// A line whose outer `type` admits it into the conversation chain — 1:1 with
/// `claude-code/src/utils/sessionStorage.ts:139` `isTranscriptMessage`. These
/// are the ONLY line types fully parsed into [`JsonlMessage`] and joined to the
/// parent-uuid graph; every other `type` is metadata (Tier-1 side-map) or
/// ignored (Tier-2 / unknown).
#[must_use]
pub fn is_transcript_message_type(ty: &str) -> bool {
    matches!(ty, "user" | "assistant" | "attachment" | "system")
}

/// Parse Claude Code's pull-request selector representation.
///
/// This mirrors JavaScript `parseInt(raw, 10)` for positive leading numeric
/// input, then accepts GitHub, Bitbucket and GitLab PR URL forms. Keeping the
/// parser in the session layer lets both CLI filtering and legacy transcript
/// recovery use exactly the same semantics.
#[must_use]
pub fn parse_pr_number(raw: &str) -> Option<u64> {
    let trimmed = raw.trim_start();
    let (negative, digits_part) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let digits: String = digits_part
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if !digits.is_empty() && !negative {
        if let Ok(number) = digits.parse::<u64>() {
            if number > 0 {
                return Some(number);
            }
        }
    }

    for marker in ["/pull/", "/pull-requests/", "/-/merge_requests/"] {
        let Some(index) = raw.find(marker) else {
            continue;
        };
        let prefix = &raw[..index];
        let prefix = prefix
            .strip_prefix("https://")
            .or_else(|| prefix.strip_prefix("http://"))
            .unwrap_or(prefix);
        if !prefix.contains('/') || prefix.contains(char::is_whitespace) {
            continue;
        }
        let digits: String = raw[index + marker.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(number) = digits.parse::<u64>() {
            if number > 0 {
                return Some(number);
            }
        }
    }
    None
}

/// Two-phase tolerant load of a session JSONL — the structural equivalent of
/// `claude-code`'s `loadTranscriptFile` (`sessionStorage.ts:3472`): chain
/// participants are parsed into [`JsonlMessage`] and indexed by uuid; Tier-1
/// metadata (summary / custom-title / ai-title / last-prompt + feature side-maps)
/// is stashed into side-maps keyed by the uuid the reader cares about; everything
/// else (Tier-2 + unknown + malformed) is skipped without error.
///
/// **Side-map inventory vs binary `Yle` function:**
///
/// | Map | Status | Binary type string |
/// |-----|--------|--------------------|
/// | `summaries` | ✅ implemented | `"summary"` |
/// | `custom_titles` | ✅ implemented | `"custom-title"` |
/// | `ai_titles` | ✅ implemented | `"ai-title"` |
/// | `last_prompt` | ✅ implemented (gap #1 fix) | `"last-prompt"` |
/// | `tags` | ✅ implemented (gap #5) | `"tag"` |
/// | `agent_names` | ✅ implemented (gap #5) | `"agent-name"` |
/// | `agent_settings` | ✅ implemented (gap #5) | `"agent-setting"` |
/// | `modes` | ✅ implemented (gap #5) | `"mode"` |
/// | `permission_modes` | ✅ implemented (gap #5) | `"permission-mode"` |
/// | `worktree_states` | ✅ implemented (gap #5) | `"worktree-state"` |
/// | `prNumbers/prUrls/prRepositories` | ✅ implemented | `"pr-link"` |
/// | `bridgeSessionIds/bridgeLastSeqs/bridgeDialogKindsBySession` | ⏭ deferred — bridge subsystem absent | `"bridge-session"` |
/// | `contextCollapseCommits/contextCollapseSnapshot` | ✅ cold-load/reset routing implemented; runtime producer/consumer is still feature-gated and tracked separately | `"marble-origami-*"` |
/// | `attributionSnapshots` | ⏭ deferred — attribution subsystem absent | `"attribution-snapshot"` |
/// | `forkContextRefs` | ⏭ deferred — fork-context subsystem absent | `"fork-context-ref"` |
/// | `contentReplacements/agentContentReplacements` | ✅ cold-load routing implemented; session replacements are carried into `/branch` | `"content-replacement"` |
/// | `isolationLatches` | ⏭ deferred — isolation/worktree out-of-process-scope | `"isolation-latch"` |
/// | `atisLatch` | ✅ cold-load / re-append / `/branch` carry implemented; live Anthropic response producer remains transport-owned | `"atis-latch"` |
/// | `fileHistorySnapshots` | ⏭ deferred — file-history-snapshot subsystem absent | `"file-history-snapshot"` |
/// | `agentColors` | ⏭ already handled by `agent_color.rs` (confirmed correct C6) | `"agent-color"` |
///
/// **`atis-latch` (SC-11, new in 2.1.238 — 15 hits there, 0 in 2.1.220).**
/// Upstream writes it mid-chain from `insertMessageChain` (@296794049) and
/// from `planReAppendSessionMetadata` (@296782335, between `isolation-latch`
/// and `worktree-state`), reading it back here and in the fork loader with a
/// `/^[\x21-\x7e]*$/` validator. Its VALUE has no derivation in the port: it
/// is `TCe()` (@281057624) = `conversationAtisLatch`, an opaque server-supplied
/// token captured off an Anthropic API response, latched per conversation and
/// echoed as a request header (@286976717). The session layer preserves a
/// valid foreign/current latch even though the live response producer belongs
/// to the Anthropic transport, preventing resume, re-append, and fork from
/// silently discarding it.
#[derive(Debug, Clone, Default)]
pub struct LoadedTranscript {
    /// Number of non-empty lines dropped because they were malformed JSON or
    /// claimed to be transcript messages but failed schema validation. The
    /// tolerant reader still returns every recoverable line; catalog callers
    /// use this only to distinguish an entirely corrupt file from a genuinely
    /// empty or metadata-only transcript.
    pub malformed_line_count: usize,
    /// Chain-participant lines (`user`/`assistant`/`attachment`/`system`) in
    /// FILE ORDER. This is what [`JsonlReader::read_all`] returns and what the
    /// golden round-trip / append-chain tests rely on.
    pub messages_in_order: Vec<JsonlMessage>,
    /// The same chain participants indexed by their `uuid` — the input to the
    /// branch-aware DAG walk ([`crate::jsonl::loader::build_conversation_chain`]).
    /// Last-write-wins on a duplicate uuid, mirroring TS `messages.set(uuid, …)`.
    pub by_uuid: HashMap<String, JsonlMessage>,
    /// `summary` entries keyed by their `leafUuid` (`sessionStorage.ts` routing
    /// loop: `summaries.set(entry.leafUuid, entry.summary)`). Lets the picker
    /// link a session's stored summary to its chain tip.
    pub summaries: HashMap<String, String>,
    /// `custom-title` entries keyed by `sessionId`
    /// (`customTitles.set(entry.sessionId, entry.customTitle)`).
    pub custom_titles: HashMap<String, String>,
    /// Session ids whose latest `custom-title` record is the versioned mobile
    /// empty-session anchor. The value is tracked separately from
    /// `custom_titles` because an unmarked later rename must revoke the
    /// anchor, matching last-write-wins metadata semantics.
    pub mobile_empty_sessions: std::collections::HashSet<String>,
    /// `ai-title` entries keyed by `sessionId` (`saveAiGeneratedTitle` writes
    /// `{type:"ai-title", sessionId, aiTitle}`; readers prefer `custom-title`
    /// over `ai-title`, `sessionStorage.ts:2644-2646`).
    pub ai_titles: HashMap<String, String>,
    /// Pull-request number keyed by `sessionId` from the latest `pr-link`
    /// metadata entry. Used by `--from-pr` resume filtering.
    pub pr_numbers: HashMap<String, u64>,
    /// Pull-request URL keyed by `sessionId` (preserved for picker consumers).
    pub pr_urls: HashMap<String, String>,
    /// Pull-request repository identifier keyed by `sessionId`.
    pub pr_repositories: HashMap<String, String>,

    // ── Gap #1 fix: last-prompt resume tip ──────────────────────────────────
    /// The last `last-prompt` entry whose `explicit===true` flag is set.
    /// Binary `Yle` routing: `else if(N.type==="last-prompt"){if(N.leafUuid)
    /// L=N.explicit===true||L&&N.leafUuid===O, O=N.leafUuid}` where `O` ends up
    /// as the forced resume tip uuid and `L` (explicit) gates whether we force.
    /// We store the raw last-seen `leafUuid` and the cumulative `explicit` flag
    /// so `find_tip` can replicate the TS logic precisely.
    ///
    /// `None` when no `last-prompt` entry with a `leafUuid` was encountered.
    pub last_prompt_leaf_uuid: Option<String>,
    /// Whether the last-prompt entry (or any prior one with the same leafUuid)
    /// had `explicit===true`. Mirrors the TS `L` accumulation variable.
    pub last_prompt_explicit: bool,

    // ── Gap #5 fix: feature side-maps present in LingXi ─────────────────────
    /// `tag` entries: keyed by `sessionId`, LAST-WRITE-WINS single value.
    /// Binary 2.1.215: `if(J.type==="tag"&&J.sessionId) a.set(J.sessionId, J.tag)`
    /// — the map stores the LATEST tag per session (an earlier build's `Yle`
    /// accumulated `[...(prev??[]), tag]`; CC has since switched to `.set` single
    /// value, so a re-tag overwrites rather than appends).
    pub tags: HashMap<String, String>,
    /// `agent-name` entries: keyed by `sessionId` → agent display name.
    /// Binary `Yle`: `agentNames.set(N.sessionId, N.agentName)`.
    pub agent_names: HashMap<String, String>,
    /// `agent-setting` entries: keyed by `sessionId` → the inner setting payload.
    /// Binary `Yle`: `agentSettings.set(N.sessionId, N.agentSetting)`.
    pub agent_settings: HashMap<String, Value>,
    /// LingXi compatibility extension: immutable, versioned resolved-agent
    /// snapshots keyed by session id. Older Claude-compatible records omit it.
    pub agent_snapshots: HashMap<String, Value>,
    /// `mode` entries: keyed by `sessionId` → mode string, last-write-wins.
    /// Binary `Yle`: `modes.set(N.sessionId, N.mode)`.
    pub modes: HashMap<String, String>,
    /// `permission-mode` entries: keyed by `sessionId` → permission-mode string,
    /// last-write-wins. Binary `Yle`: `permissionModes.set(N.sessionId, N.permissionMode)`.
    pub permission_modes: HashMap<String, String>,
    /// `session-mode` entries: keyed by `sessionId` → mobile session mode,
    /// last-write-wins. LingXi-only metadata used to separate chat/code
    /// capability profiles without colliding with Claude's generic `mode`.
    pub session_modes: HashMap<String, String>,
    /// `worktree-state` entries: keyed by `sessionId` → the inner
    /// `worktreeSession` JSON value (an object for an active worktree, or `null`
    /// after `ExitWorktree` clears it). Binary (2.1.212 `Yle`):
    /// `worktreeStates.set(N.sessionId, N.worktreeSession)`. Read back on
    /// `--continue`/`--resume` by [`crate::jsonl::loader::read_worktree_state`] to
    /// rehydrate the session's active worktree so `ExitWorktree` operates instead
    /// of no-oping ("No-op: there is no active EnterWorktree session to exit").
    pub worktree_states: HashMap<String, Value>,
    /// Flattened session-level `content-replacement` entries keyed by `sessionId`.
    /// Each new line appends its `replacements` array onto the accumulated tail.
    pub content_replacements: HashMap<String, Vec<Value>>,
    /// Flattened agent-level `content-replacement` entries keyed by `agentId`.
    /// Each new line appends its `replacements` array onto the accumulated tail.
    pub agent_content_replacements: HashMap<String, Vec<Value>>,
    /// Latest non-empty `relocatedCwd` per session, used by `/branch` and
    /// resume-time cwd recovery. Empty writes do not clear the previous value.
    pub relocated_cwds: HashMap<String, String>,
    /// Whether the transcript contains any `history-suppression` record.
    /// A transcript file belongs to one session, matching `createFork`'s
    /// source scan, which deliberately does not filter this record by id.
    pub session_history_suppressed: bool,
    /// Latest validated `atis-latch` per session. The oracle accepts only
    /// ASCII bytes in `[0x21, 0x7e]`; the `*` quantifier also accepts empty.
    pub atis_latches: HashMap<String, String>,
    /// `marble-origami-commit` entries in source order, cleared by a later
    /// `marble-origami-reset` or compact boundary.
    pub context_collapse_commits: Vec<Value>,
    /// Latest `marble-origami-snapshot`, cleared by a later
    /// `marble-origami-reset` or compact boundary.
    pub context_collapse_snapshot: Option<Value>,
}

/// Failure modes for [`JsonlReader`].
#[derive(Debug, Error)]
pub enum ReaderError {
    /// Underlying filesystem error.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// Line `n` (0-indexed) failed to parse as `JsonlMessage`.
    ///
    /// RETAINED for API/back-compat only. As of the tolerant-reader gap fix the
    /// load path NEVER produces this — malformed and non-message lines are
    /// skipped (see [`route_lines`] / `JsonlReader::read_all`), matching
    /// `claude-code`'s `parseJSONL` (`json.ts:155`). Kept so any external match
    /// on `ReaderError` stays exhaustive.
    #[error("parse failure at line {0}: {1}")]
    Parse(usize, String),
    /// First line didn't contain a required metadata field.
    #[error("lite read: missing field {0}")]
    LiteMissing(&'static str),
}

/// Lite metadata — populated from the first JSON line only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    /// `sessionId` from the first line's outer object.
    pub session_id: String,
    /// `cwd` from the first line's outer object.
    pub cwd: String,
    /// `type` from the first line's outer object (e.g. `"user"`).
    pub first_type: String,
}

/// Reader for one session's `<uuid>.jsonl`.
pub struct JsonlReader {
    path: PathBuf,
    fs: Arc<dyn FileSystem>,
}

impl JsonlReader {
    /// Construct a reader. No I/O until `read_all`/`read_lite` is called.
    #[must_use]
    pub fn new(path: PathBuf, fs: Arc<dyn FileSystem>) -> Self {
        Self { path, fs }
    }

    /// Path on disk.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read every line and return the chain-participant lines
    /// (`user`/`assistant`/`attachment`/`system`) in FILE ORDER.
    ///
    /// TOLERANT (BLOCKING gap fix): a real `claude-code` transcript interleaves
    /// non-message line `type`s (`summary`, `file-history-snapshot`, `mode`,
    /// `permission-mode`, `last-prompt`, `queue-operation`, `ai-title`, …) and
    /// can contain truncated / malformed lines from a crash mid-write. The old
    /// `read_all` hard-errored on the FIRST such line. We now mirror
    /// `claude-code`'s two-phase load (`parseJSONL` skips malformed lines,
    /// `json.ts:155`; `isTranscriptMessage` selects chain participants,
    /// `sessionStorage.ts:139`): parse each line as a `Value`, skip on parse
    /// error, and keep only lines whose outer `type` is a transcript message.
    /// Metadata + unknown + malformed lines are dropped here — use
    /// [`Self::read_routed`] when the side-maps (summaries / titles) are needed.
    ///
    /// The function is now infallible-on-content (no `ReaderError::Parse`); the
    /// only error path left is the underlying [`FsError`] from the read itself.
    pub async fn read_all(&self) -> Result<Vec<JsonlMessage>, ReaderError> {
        Ok(self.read_routed().await?.messages_in_order)
    }

    /// Full two-phase tolerant load — see [`LoadedTranscript`]. Returns the
    /// chain participants (file-order + by-uuid index) AND the Tier-1 metadata
    /// side-maps (`summary` → `leafUuid`, `custom-title`/`ai-title` →
    /// `sessionId`). Tier-2 / unknown line types and malformed lines are
    /// silently skipped, NEVER errored — faithful to `loadTranscriptFile`.
    pub async fn read_routed(&self) -> Result<LoadedTranscript, ReaderError> {
        let path_str = self.path.to_str().expect("UTF-8 path");
        let content = self.fs.read_file(path_str, None, None).await?.content;
        Ok(route_lines(&content))
    }

    /// Read up to `LITE_READ_BUF_SIZE` bytes from the file head and extract
    /// `sessionId` / `cwd` / `type` from line 1 using
    /// [`extract_json_string_field`] (no full parse — works even if line 1
    /// is the only complete line in the buffer).
    pub async fn read_lite(&self) -> Result<SessionMetadata, ReaderError> {
        let path_str = self.path.to_str().expect("UTF-8 path");
        // Prefix read: at most `LITE_READ_BUF_SIZE` bytes from the file head.
        // Line 1 is extracted from that window (a very long line-1 payload is
        // still capped at 64 KiB).
        let read = self
            .fs
            .read_file_prefix(path_str, super::LITE_READ_BUF_SIZE)
            .await?;
        // Strip a leading UTF-8 BOM (claude-code parseJSONLBuffer, live in
        // v2.1.193) so `extract_json_string_field` sees a clean line 1 — without
        // it a BOM-prefixed transcript's first line yields no sessionId/cwd.
        let content = read
            .content
            .strip_prefix('\u{FEFF}')
            .unwrap_or(&read.content);
        let head = if content.len() > super::LITE_READ_BUF_SIZE {
            &content[..super::LITE_READ_BUF_SIZE]
        } else {
            content
        };
        let line1 = head.split('\n').next().unwrap_or("");
        let session_id = extract_json_string_field(line1, "sessionId")
            .ok_or(ReaderError::LiteMissing("sessionId"))?;
        let cwd = extract_json_string_field(line1, "cwd").ok_or(ReaderError::LiteMissing("cwd"))?;
        let first_type =
            extract_json_string_field(line1, "type").ok_or(ReaderError::LiteMissing("type"))?;
        Ok(SessionMetadata {
            session_id,
            cwd,
            first_type,
        })
    }
}

/// Route every non-empty line of a JSONL transcript into a [`LoadedTranscript`]
/// — the pure core of [`JsonlReader::read_routed`] (kept free-standing so it can
/// be unit-tested without a `FileSystem`). Two-phase, byte-for-byte faithful to
/// `claude-code`'s `loadTranscriptFile` routing loop (`sessionStorage.ts:3472`):
///
/// 1. Parse the line as `serde_json::Value`. On parse error → `continue` (skip
///    malformed; mirrors `parseJSONL`'s `try/catch`, `json.ts:155`). NEVER error.
/// 2. Branch on `value["type"]`:
///    - transcript message (`user`/`assistant`/`attachment`/`system`,
///      [`is_transcript_message_type`]) → `from_value::<JsonlMessage>` into
///      `messages_in_order` + `by_uuid`. A `JsonlMessage` that fails to
///      deserialize (e.g. a non-string `uuid`) is skipped, not errored — pure
///      metadata lines are routed out FIRST by `type`, so a transcript-typed
///      line that still won't parse is genuinely corrupt and dropped.
///    - Tier-1 metadata (`summary`/`custom-title`/`ai-title`/`last-prompt` +
///      feature side-maps) → side-maps on [`LoadedTranscript`].
///    - Tier-2 + unknown (`file-history-snapshot`, `queue-operation`, …) → ignored.
///
/// ⚠️ SES-2, verified 2026-09-14. The backlog records `queue-operation` as
/// "never written" and asks for a producer. Writing one would produce
/// WRITE-ONLY data: this line is the reason — the type is explicitly routed to
/// the ignored tier, so nothing in this port would ever read a row back. The
/// queue mutations themselves ARE recorded, as telemetry
/// (`msgqueue::telemetry_recorder`), which is the half that has a consumer.
/// The feature the rows exist to serve upstream is queue ADOPT-ON-RESUME
/// (`WAn(e)` reads `e.adopt`); a producer here without that is a transcript
/// that is one line longer and behaves identically. ⇒ Build the adopt path
/// first, then the producer, then a reader arm — in that order, or the middle
/// step is the only one that lands.
#[must_use]
pub fn route_lines(content: &str) -> LoadedTranscript {
    let mut out = LoadedTranscript::default();
    // Strip a leading UTF-8 BOM before splitting (claude-code json.ts
    // parseJSONLBuffer: `if(buf[0]===0xef&&buf[1]===0xbb&&buf[2]===0xbf) start=3`,
    // confirmed live in v2.1.193). Rust `str::trim()` does NOT treat U+FEFF as
    // whitespace, so without this the BOM survives onto line 1 and
    // `serde_json::from_str` rejects it → the first message is silently dropped.
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Phase 1 — tolerant JSON parse; skip malformed lines (no error).
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            out.malformed_line_count += 1;
            continue;
        };
        let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
        if is_transcript_message_type(ty) {
            // Phase 2a — chain participant. Route by `type` first, so we only
            // attempt the strict `JsonlMessage` parse on lines that are SUPPOSED
            // to be messages; a failure here means a corrupt transcript line, so
            // skip it (still no hard error, matching the tolerant contract).
            let Ok(msg) = serde_json::from_value::<JsonlMessage>(value) else {
                out.malformed_line_count += 1;
                continue;
            };
            // A full compact boundary makes every earlier collapse span stale:
            // its archived UUIDs are outside the new active chain. Claude's
            // forward reader clears both side stores at this exact point; later
            // marble records in the file build the post-boundary state anew.
            let clears_context_collapse = msg.message_type == "system"
                && msg.extra.get("subtype").and_then(Value::as_str) == Some("compact_boundary");
            out.by_uuid.insert(msg.uuid.clone(), msg.clone());
            out.messages_in_order.push(msg);
            if clears_context_collapse {
                out.context_collapse_commits.clear();
                out.context_collapse_snapshot = None;
            }
        } else if ty == "summary" {
            // `summaries.set(entry.leafUuid, entry.summary)` — keyed by leafUuid.
            if let (Some(leaf), Some(summary)) = (
                value.get("leafUuid").and_then(Value::as_str),
                value.get("summary").and_then(Value::as_str),
            ) {
                out.summaries.insert(leaf.to_string(), summary.to_string());
            }
        } else if ty == "custom-title" {
            // `customTitles.set(entry.sessionId, entry.customTitle)`.
            if let (Some(sid), Some(title)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("customTitle").and_then(Value::as_str),
            ) {
                out.custom_titles.insert(sid.to_string(), title.to_string());
                if value.get("mobileEmptySession").and_then(Value::as_u64) == Some(1) {
                    out.mobile_empty_sessions.insert(sid.to_string());
                } else {
                    out.mobile_empty_sessions.remove(sid);
                }
            }
        } else if ty == "ai-title" {
            // `saveAiGeneratedTitle` writes `{sessionId, aiTitle}`.
            if let (Some(sid), Some(title)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("aiTitle").and_then(Value::as_str),
            ) {
                out.ai_titles.insert(sid.to_string(), title.to_string());
            }
        } else if ty == "pr-link" {
            // PR metadata is a last-write-wins side record keyed by sessionId.
            // Be tolerant of older writers that serialized the number as a
            // decimal string instead of a JSON number.
            if let Some(sid) = value.get("sessionId").and_then(Value::as_str) {
                let url = value.get("prUrl").and_then(Value::as_str);
                let number = value
                    .get("prNumber")
                    .and_then(|v| {
                        v.as_u64()
                            .filter(|number| *number > 0)
                            .or_else(|| v.as_str().and_then(parse_pr_number))
                    })
                    // Older transcripts sometimes persisted only `prUrl`.
                    // Recover the number so `--from-pr` sees those sessions.
                    .or_else(|| url.and_then(parse_pr_number));
                if let Some(number) = number {
                    out.pr_numbers.insert(sid.to_string(), number);
                }
                if let Some(url) = url {
                    out.pr_urls.insert(sid.to_string(), url.to_string());
                }
                if let Some(repository) = value.get("prRepository").and_then(Value::as_str) {
                    out.pr_repositories
                        .insert(sid.to_string(), repository.to_string());
                }
            }

        // ── Gap #1 fix: last-prompt → explicit resume tip ────────────────────
        } else if ty == "last-prompt" {
            // Binary `Yle` (@ 206473264):
            //   `else if(N.type==="last-prompt"){if(N.leafUuid)
            //      L=N.explicit===true||L&&N.leafUuid===O, O=N.leafUuid}`
            // L = explicit flag, O = forced tip uuid.
            // We replicate: on each last-prompt entry that has a leafUuid:
            //   - new_explicit = entry.explicit===true
            //                    || (prior_explicit && leafUuid == prior_tip)
            //   - update last_prompt_leaf_uuid to the new leafUuid
            //   - update last_prompt_explicit to new_explicit
            if let Some(leaf_uuid) = value.get("leafUuid").and_then(Value::as_str) {
                let entry_explicit = value
                    .get("explicit")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let prev_explicit = out.last_prompt_explicit;
                let prev_uuid = out.last_prompt_leaf_uuid.as_deref().unwrap_or("");
                // TS: L = N.explicit===true || L && N.leafUuid===O
                let new_explicit = entry_explicit || (prev_explicit && leaf_uuid == prev_uuid);
                out.last_prompt_leaf_uuid = Some(leaf_uuid.to_string());
                out.last_prompt_explicit = new_explicit;
            }

        // ── Gap #5 fix: feature side-maps present in LingXi ─────────────────
        } else if ty == "tag" {
            // Binary 2.1.215: `a.set(J.sessionId, J.tag)` — last-write-wins single
            // value (a re-tag OVERWRITES the session's tag, not appends).
            if let (Some(sid), Some(tag)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("tag").and_then(Value::as_str),
            ) {
                out.tags.insert(sid.to_string(), tag.to_string());
            }
        } else if ty == "agent-name" {
            // Binary v2.1.193 @211658974: `else if(j.type==="agent-name"&&j.sessionId)
            // a.set(j.sessionId,j.agentName)` — keyed by `sessionId` (NOT `agentId`;
            // the write side `{type:"agent-name",agentName,sessionId}` carries no
            // `agentId`, so the old `agentId` guard dropped every real entry).
            if let (Some(sid), Some(agent_name)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("agentName").and_then(Value::as_str),
            ) {
                out.agent_names
                    .insert(sid.to_string(), agent_name.to_string());
            }
        } else if ty == "agent-setting" {
            // Binary v2.1.193 @211659124: `else if(j.type==="agent-setting"&&j.sessionId)
            // c.set(j.sessionId,j.agentSetting)` — keyed by `sessionId` and stores the
            // inner `agentSetting` payload (write side
            // `{type:"agent-setting",agentSetting,sessionId}`), not `agentId`/whole-entry.
            if let (Some(sid), Some(agent_setting)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("agentSetting"),
            ) {
                out.agent_settings
                    .insert(sid.to_string(), agent_setting.clone());
            }
            if let (Some(sid), Some(snapshot)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("agentSnapshot"),
            ) {
                out.agent_snapshots
                    .insert(sid.to_string(), snapshot.clone());
            }
        } else if ty == "mode" {
            // Binary `Yle`: `modes.set(N.sessionId, N.mode)`.
            if let (Some(sid), Some(mode)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("mode").and_then(Value::as_str),
            ) {
                out.modes.insert(sid.to_string(), mode.to_string());
            }
        } else if ty == "permission-mode" {
            // Binary `Yle`: `permissionModes.set(N.sessionId, N.permissionMode)`.
            if let (Some(sid), Some(pm)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("permissionMode").and_then(Value::as_str),
            ) {
                out.permission_modes.insert(sid.to_string(), pm.to_string());
            }
        } else if ty == "session-mode" {
            if let (Some(sid), Some(mode)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("sessionMode").and_then(Value::as_str),
            ) {
                out.session_modes.insert(sid.to_string(), mode.to_string());
            }
        } else if ty == "worktree-state" {
            // Binary (2.1.212 `Yle`): `x.set(J.sessionId, J.worktreeSession)` —
            // keyed by `sessionId` and stores the inner `worktreeSession` payload
            // (write side `{type:"worktree-state",worktreeSession,sessionId}`),
            // NOT `agentId`/whole-entry. The stored value may be JSON `null` (the
            // ExitWorktree clear record) — last-write-wins, so a later `null`
            // supersedes an earlier active session for the same `sessionId`.
            if let (Some(sid), Some(ws)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("worktreeSession"),
            ) {
                out.worktree_states.insert(sid.to_string(), ws.clone());
            }
        } else if ty == "content-replacement" {
            if let Some(agent_id) = value.get("agentId").and_then(Value::as_str) {
                if let Some(replacements) = value.get("replacements").and_then(Value::as_array) {
                    out.agent_content_replacements
                        .entry(agent_id.to_string())
                        .or_default()
                        .extend(replacements.iter().cloned());
                }
            } else if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                if let Some(replacements) = value.get("replacements").and_then(Value::as_array) {
                    out.content_replacements
                        .entry(session_id.to_string())
                        .or_default()
                        .extend(replacements.iter().cloned());
                }
            }
        } else if ty == "relocated" {
            if let (Some(session_id), Some(relocated_cwd)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("relocatedCwd").and_then(Value::as_str),
            ) {
                if !relocated_cwd.is_empty() {
                    out.relocated_cwds
                        .insert(session_id.to_string(), relocated_cwd.to_string());
                }
            }
        } else if ty == "history-suppression" {
            out.session_history_suppressed = true;
        } else if ty == "atis-latch" {
            if let (Some(session_id), Some(atis)) = (
                value.get("sessionId").and_then(Value::as_str),
                value.get("atis").and_then(Value::as_str),
            ) {
                if is_valid_atis_latch(atis) {
                    out.atis_latches
                        .insert(session_id.to_string(), atis.to_string());
                }
            }
        } else if ty == "marble-origami-commit" {
            out.context_collapse_commits.push(value.clone());
        } else if ty == "marble-origami-snapshot" {
            out.context_collapse_snapshot = Some(value.clone());
        } else if ty == "marble-origami-reset" {
            out.context_collapse_commits.clear();
            out.context_collapse_snapshot = None;
        }
        // else: Tier-2 / unknown / deferred subsystem → ignored (no error).
    }
    out
}

/// Claude 2.1.245's `/^[\x21-\x7e]*$/` ATIS validator.
#[must_use]
pub(crate) fn is_valid_atis_latch(value: &str) -> bool {
    value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

/// 1:1 port of `claude-code/src/utils/sessionStoragePortable.ts:53-76`.
///
/// Looks for `"key":"value"` or `"key": "value"` (one optional space after
/// the colon). Returns the first match. `\` escapes the next char inside
/// the value. The closing `"` ends the value.
#[must_use]
pub fn extract_json_string_field(text: &str, key: &str) -> Option<String> {
    let patterns = [format!("\"{key}\":\""), format!("\"{key}\": \"")];
    let bytes = text.as_bytes();
    for pat in &patterns {
        let pat_bytes = pat.as_bytes();
        if let Some(idx) = find_subslice(bytes, pat_bytes) {
            let value_start = idx + pat_bytes.len();
            let mut i = value_start;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = i.saturating_add(2);
                    continue;
                }
                if bytes[i] == b'"' {
                    let raw = &text[value_start..i];
                    return Some(unescape_json_string(raw));
                }
                i += 1;
            }
        }
    }
    None
}

/// 1:1 port of `claude-code/src/utils/sessionStoragePortable.ts:39-46`.
#[must_use]
pub fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let wrapped = format!("\"{raw}\"");
    serde_json::from_str::<String>(&wrapped).unwrap_or_else(|_| raw.to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}
