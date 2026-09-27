//! `JsonlMessage` — outer JSONL line schema.
//!
//! Field NAMES **and ORDER** are parity-locked to claude-code's transcript
//! schema (verified against real 2.1.195 on-disk transcripts —
//! ~810k lines). `Serialize` is hand-written ([`impl Serialize for
//! JsonlMessage`]) to emit claude's EXACT per-kind outer-key order instead of
//! the struct-declaration order; `Deserialize` stays derived (the reader is
//! order-independent).
//!
//! Per-kind head order (everything before the common trailer), for the fields
//! this engine actually emits:
//! - **user**: `parentUuid, isSidechain, [promptId,] type, message, [isMeta,]
//!   [isVisibleInTranscriptOnly,] [isCompactSummary,] uuid, timestamp` (the
//!   two compact-summary flags ride between `message` and `uuid`, per real
//!   2.1.207 summary lines).
//! - **assistant (normal)**: `parentUuid, isSidechain, message, [requestId,]
//!   type, uuid, timestamp`
//! - **assistant (api-error)**: `parentUuid, isSidechain, type, uuid,
//!   timestamp, message, [requestId,] [error,] [errorDetails,]
//!   [truncatedAfterOutput,] isApiErrorMessage, [apiErrorStatus]`
//! - **system (compact boundary, `extra.subtype == "compact_boundary"`)**:
//!   `parentUuid, [logicalParentUuid,] isSidechain, type, subtype, content,
//!   [isMeta,] level, compactMetadata, uuid, timestamp` — claude's FLATTENED
//!   boundary envelope (real 2.1.207 transcripts); these lines carry NO inner
//!   `message`.
//! - **system (other)**: `parentUuid, [logicalParentUuid,] isSidechain, type,
//!   message, [isMeta,] uuid, timestamp` (the port emits an inner
//!   `{role,content}` `message`; claude's flattened `subtype`/`content`
//!   top-level system envelope is ported only for the compact-boundary
//!   subtype, so on other system lines those siblings — when present in
//!   `extra` — are tail-appended verbatim rather than synthesized into
//!   claude's positions).
//!
//! Common trailer (every kind): `userType, [entrypoint,] cwd, sessionId,
//! version, [gitBranch,] [slug]`. On a `user` line carrying exactly one
//! `tool_result`, claude's TOOL-RESULT HEAD (`toolUseResult, toolDenialKind,
//! mcpMeta, toolEndsTurn, sourceToolAssistantUUID` — see
//! [`TOOL_RESULT_HEAD_EXTRA`]) rides between `timestamp` and that trailer. Any
//! UNRECOGNIZED `extra` key (an unported claude field, e.g. `agentId`) is
//! appended after the trailer in `extra` iteration order, so read→write
//! round-trips of unported fields stay byte-faithful.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

/// One line of the session JSONL file.
///
/// Outer field NAMES **and ORDER** match claude: the hand-written
/// [`impl Serialize for JsonlMessage`] emits claude's per-kind envelope order
/// (see the module docs) rather than this struct's declaration order — so the
/// on-disk byte layout is 1:1, not just the field set. (This corrects an
/// earlier note that claimed the order "does not yet" match; the hand-written
/// `Serialize` closed that gap.) The `message` field is an opaque `Value`
/// because its inner schema depends on `type` (Anthropic
/// Messages API for `user`/`assistant`, claude-code internal shapes for
/// `system`/`attachment`). All un-named outer fields land in `extra` via
/// `#[serde(flatten)]` so read→write round-trips preserve every byte we read.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonlMessage {
    /// `"user" | "assistant" | "system" | "attachment" | "summary" | ...`
    /// — see `claude-code/src/types/logs.ts:297` `Entry` union.
    #[serde(rename = "type")]
    pub message_type: String,

    /// Stable identifier for this entry. UUID v4 lowercase.
    pub uuid: String,

    /// Parent entry's `uuid`, or JSON `null` for the first turn.
    /// MUST serialize as `null` (NOT omitted) — `claude-code/src/types/logs.ts:222`.
    #[serde(rename = "parentUuid")]
    pub parent_uuid: Option<String>,

    /// Session UUID. Matches the filename without `.jsonl`.
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// ISO-8601 UTC timestamp with millisecond precision
    /// (`new Date().toISOString()`), e.g. `"2026-05-25T14:30:00.000Z"`.
    ///
    /// TOLERANT: `user`/`assistant` lines always carry it, but `attachment` /
    /// `system` chain-participant lines (and some legacy rows) may omit it —
    /// `claude-code`'s `isTranscriptMessage` admits those into the chain without
    /// requiring a timestamp, so we default to the empty string on read rather
    /// than rejecting the line. The branch-aware DAG walk
    /// ([`crate::jsonl::loader::build_conversation_chain`]) parses this via
    /// `chrono` and treats an unparsable / empty value as epoch (oldest), so a
    /// missing timestamp simply can't win the newest-leaf race.
    #[serde(default)]
    pub timestamp: String,

    /// Canonical absolute cwd at the time this entry was written.
    /// TOLERANT (`#[serde(default)]`): present on `user`/`assistant`, may be
    /// absent on `attachment`/`system`.
    #[serde(default)]
    pub cwd: String,

    /// Engine version string. claude-code: `MACRO.VERSION`; lingxi-core: `CARGO_PKG_VERSION`.
    /// TOLERANT (`#[serde(default)]`) for the same reason as `cwd`.
    #[serde(default)]
    pub version: String,

    /// Inner message object — Anthropic Messages API shape for `user`/`assistant`,
    /// other shapes for system entries. Preserved verbatim.
    ///
    /// TOLERANT: `attachment` and `system` chain-participant lines frequently
    /// lack a `message` field entirely (their payload lives in sibling outer
    /// fields captured by [`Self::extra`]). `#[serde(default)]` yields
    /// `Value::Null` when absent so the line still parses and joins the chain,
    /// matching `claude-code`'s `isTranscriptMessage` (it gates on `type` only,
    /// never on the presence of `message`).
    #[serde(default)]
    pub message: Value,

    // The Anthropic `request-id` response header (`req_…`) is persisted as the
    // top-level `requestId` field on REAL assistant lines (the SDK's
    // `response._request_id`). It is carried through [`Self::extra`] (the same
    // flatten channel as `isMeta`), set by the writer only for real assistant
    // lines — so no struct-literal constructor churn, and read-side round-trips
    // it transparently. See `orchestrator::conversation::to_jsonl_message_with_inner_id`.
    /// `false` for the main agent loop; `true` for sub-agent transcripts.
    /// Optional in the schema but emitted by claude-code's writer; we default
    /// to `false` on write and accept missing on read.
    #[serde(rename = "isSidechain", default)]
    pub is_sidechain: bool,

    /// `process.env.USER_TYPE || "external"` — lingxi-core lock: `"external"`.
    #[serde(rename = "userType", skip_serializing_if = "Option::is_none")]
    pub user_type: Option<String>,

    /// Git branch at write time, when available.
    /// `getBranch()` once per chain (`sessionStorage.ts:1012-1019`); on a
    /// non-repo / git failure the writer leaves it `None` → omitted.
    #[serde(rename = "gitBranch", skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,

    /// CLI entrypoint string — `getEntrypoint()` (`sessionStorage.ts:1058`),
    /// e.g. `"cli"`. Stamped on EVERY line by claude-code's writer. Optional on
    /// read (legacy rows omit it); `skip_serializing_if = "Option::is_none"`
    /// so an unset value is omitted, matching TS `undefined`.
    #[serde(
        rename = "entrypoint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub entrypoint: Option<String>,

    /// Per-session plan slug — `getPlanSlugCache().get(sessionId)`
    /// (`sessionStorage.ts:1023, 1063`). `undefined` when the session has no
    /// plan slug; omitted on write when `None`.
    #[serde(rename = "slug", default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,

    /// Stable per-prompt id — `getPromptId()` (`sessionStorage.ts:1045-1046`),
    /// set ONLY on `user` lines (TS: `type === 'user' ? getPromptId() : undefined`).
    /// `None` (omitted) on every non-`user` line and on `user` lines when no
    /// prompt id is available.
    #[serde(rename = "promptId", default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,

    /// Compact-boundary back-link — `logicalParentUuid`
    /// (`sessionStorage.ts:1041`). On a compaction boundary TS sets
    /// `parentUuid: null` and stashes the real parent here
    /// (`isCompactBoundary ? parentUuid : undefined`); on every other line it is
    /// `undefined`. Omitted on write when `None`.
    #[serde(
        rename = "logicalParentUuid",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub logical_parent_uuid: Option<String>,

    /// All other outer fields (`agentId`, `agentName`, `agentColor`,
    /// `teamName`, `isMeta`, `toolUseResult`, ...) preserved verbatim across
    /// read->write so round-trips are byte-equivalent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `extra` keys this serializer pulls into their claude-ordered positions.
/// Anything NOT in this set is appended verbatim after the common trailer
/// (forward-compat for unported claude fields like `agentId`/`toolUseResult`).
const RECOGNIZED_EXTRA: &[&str] = &[
    "isMeta",
    "requestId",
    "error",
    "errorDetails",
    "truncatedAfterOutput",
    "isApiErrorMessage",
    "apiErrorStatus",
    "effort",
    // SC-07: emitted by the common trailer (arm h), immediately before
    // `userType`, so it must not also fall out of the unrecognized-key tail.
    SESSION_KIND_KEY,
];

/// The outer chain-entry key for [`session_kind`].
///
/// Deliberately a constant: the loader's `/resume` daemon filter
/// (`crate::jsonl::loader`) looks the same key up out of
/// [`JsonlMessage::extra`], and the two spellings used to be independent
/// literals with only the reader half present.
pub const SESSION_KIND_KEY: &str = "sessionKind";

/// Env var carrying the session kind — `CLAUDE_CODE_SESSION_KIND` upstream,
/// `LINGXI_SESSION_KIND` here (the spelling `tool-api::defer`,
/// `commands/core/src/stop.rs` and `apps/cli` already read).
pub const SESSION_KIND_ENV: &str = "LINGXI_SESSION_KIND";

/// `a3e()` (cc-238.js @283798463), verbatim:
///
/// ```js
/// function a3e(){ let e=V.CLAUDE_CODE_SESSION_KIND;
///   if(e==="bg"||e==="daemon"||e==="daemon-worker") return e; return }
/// ```
///
/// Note the WHITELIST: any other value (including the port's own
/// `LINGXI_SESSION_KIND=interactive`, which `commands/core/src/stop.rs` tests
/// set) yields `undefined`, i.e. the key is omitted — it is not passed through.
///
/// Upstream this is stamped on EVERY chain entry
/// (`insertMessageChain` @296794533: `…, sessionKind:a3e(), userType, …`).
/// The port's reader half — the `/resume` picker's `daemon` / `daemon-worker`
/// filter — has existed since the SESSION.2 gap fix with no writer behind it;
/// [`crate::jsonl::writer::JsonlWriter::append`] is the writer.
#[must_use]
pub fn session_kind() -> Option<String> {
    match std::env::var(SESSION_KIND_ENV).ok()?.as_str() {
        kind @ ("bg" | "daemon" | "daemon-worker") => Some(kind.to_string()),
        _ => None,
    }
}

/// `extra` keys the USER head consumes (between `message` and `uuid`): the
/// compact-summary envelope flags, emitted in claude's on-disk order
/// (`..., "message":…, "isVisibleInTranscriptOnly":true, "isCompactSummary":
/// true, "uuid":…` — real 2.1.207 transcripts). Skipped from the tail ONLY on
/// user lines, so a non-user line carrying them still round-trips verbatim.
const SCHEDULED_FIRE_HEAD: &[&str] = &[
    "subtype",
    "content",
    "isMeta",
    "taskId",
    "cron",
    "prompt",
    "taskKind",
    "cronKind",
    "noOpStreak",
    "streakStartedAt",
    "foldedUuids",
];

const USER_HEAD_EXTRA: &[&str] = &[
    "isVisibleInTranscriptOnly",
    "isCompactSummary",
    "turnCompanion",
];

/// `extra` keys emitted BETWEEN `timestamp` and the common trailer, in this
/// exact order — claude's tool-result head.
///
/// Census of every `toolDenialKind`-bearing line in real 2.1.220 transcripts
/// (14 records across 5 envelope variants, which differ only in the optional
/// `agentId` / `session_id` / `slug`) agrees on
/// `…, uuid, timestamp, toolUseResult, toolDenialKind,
/// sourceToolAssistantUUID, userType, …` — i.e. these precede the trailer,
/// whereas an unrecognized `extra` key is appended after it.
///
/// Order comes from THIS list, not from `extra` insertion order, so the bytes
/// do not depend on the order a producer happens to fill the map in.
///
/// The full order comes from the in-memory user-message factory `zr()`
/// (2.1.220 BIN off **238011637**):
///
/// ```text
/// {type, message, isMeta, isVisibleInTranscriptOnly, isVirtual,
///  isCompactSummary, summarizeMetadata, uuid, timestamp,
///  toolUseResult, classifierMetaLines, toolDenialKind, userFeedback,
///  mcpMeta, toolEndsTurn, imagePasteIds, sourceToolAssistantUUID,
///  permissionMode, origin, promptSource, interruptedMessageId,
///  interruptedByShutdown}
/// ```
///
/// `JSON.stringify` drops the `undefined` slots, so the on-disk order falls
/// straight out. `mcpMeta`/`toolEndsTurn` sit between `toolDenialKind` and
/// `sourceToolAssistantUUID` — confirmed on disk by 2 146 real 2.1.220 lines
/// carrying the adjacent tuple `[toolEndsTurn, sourceToolAssistantUUID,
/// userType]`.
///
/// RESIDUAL: `classifierMetaLines`, `userFeedback` and `imagePasteIds` are
/// omitted from this table — the port has no producer for any of them, so
/// listing them would encode an unverifiable slot. A foreign line carrying one
/// still round-trips (via the unrecognized-key tail), just not in claude's
/// head slot.
const TOOL_RESULT_HEAD_EXTRA: &[&str] = &[
    "toolUseResult",
    "toolDenialKind",
    "mcpMeta",
    "toolEndsTurn",
    "sourceToolAssistantUUID",
    "permissionMode",
];

/// `extra` keys the `subtype:"compact_boundary"` SYSTEM head consumes —
/// claude's flattened boundary envelope (`type, subtype, content, level,
/// compactMetadata, uuid, timestamp`, real 2.1.207 transcripts; boundary lines
/// carry NO inner `message`). Skipped from the tail only when the boundary
/// head emitted them.
const BOUNDARY_HEAD_EXTRA: &[&str] = &["subtype", "content", "level", "compactMetadata"];

// Hand-written `Serialize` so the outer JSONL keys land in claude-code's EXACT
// per-kind order (see module docs). `Deserialize` stays derived — the reader is
// order-independent. VALUES are byte-identical to the derived impl; only key
// POSITION changes. Skip predicates mirror the struct's `skip_serializing_if`
// so presence parity is preserved.
impl Serialize for JsonlMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // serde_json's map serializer preserves `serialize_entry` insertion
        // order, so emitting entries in sequence yields claude's on-disk order.
        let mut map = serializer.serialize_map(None)?;

        let is_user = self.message_type == "user";
        let is_assistant = self.message_type == "assistant";
        let is_system = self.message_type == "system";
        // Attachment line: claude emits the `attachment` PAYLOAD before the
        // `type` discriminator and writes NO inner `message`.
        let is_attachment = self.message_type == "attachment";
        let is_api_error = self.extra.contains_key("isApiErrorMessage");
        let is_scheduled_fire = is_system
            && self.extra.get("subtype").and_then(Value::as_str) == Some("scheduled_task_fire");
        // Compact-boundary system line: claude flattens the system envelope
        // (`subtype`/`content`/`level`/`compactMetadata` are top-level
        // siblings, no inner `message`). Only THIS system subtype gets the
        // flattened head; other system lines keep the generic arm below.
        let is_compact_boundary = is_system
            && self.extra.get("subtype").and_then(Value::as_str) == Some("compact_boundary");

        // (a) parentUuid — ALWAYS first, emitted even when null.
        map.serialize_entry("parentUuid", &self.parent_uuid)?;
        // (b) logicalParentUuid — system places it right after parentUuid. The
        //     port sets it on compact-boundary lines (which carry
        //     `parentUuid: null` + the real parent here, the claude chain
        //     reset); omitted when None to match the TS `undefined` skip.
        if self.logical_parent_uuid.is_some() {
            map.serialize_entry("logicalParentUuid", &self.logical_parent_uuid)?;
        }
        // (c) isSidechain — always.
        map.serialize_entry("isSidechain", &self.is_sidechain)?;

        if is_user {
            // (d) user head: promptId?, type, message, isMeta?,
            //     isVisibleInTranscriptOnly?, isCompactSummary?, uuid,
            //     timestamp. The two compact-summary flags sit between
            //     `message` and `uuid` on claude's persisted summary lines
            //     (real 2.1.207 transcripts).
            if let Some(pid) = &self.prompt_id {
                map.serialize_entry("promptId", pid)?;
            }
            map.serialize_entry("type", &self.message_type)?;
            map.serialize_entry("message", &self.message)?;
            if let Some(v) = self.extra.get("isMeta") {
                map.serialize_entry("isMeta", v)?;
            }
            if let Some(v) = self.extra.get("turnCompanion") {
                map.serialize_entry("turnCompanion", v)?;
            }
            if let Some(v) = self.extra.get("isVisibleInTranscriptOnly") {
                map.serialize_entry("isVisibleInTranscriptOnly", v)?;
            }
            if let Some(v) = self.extra.get("isCompactSummary") {
                map.serialize_entry("isCompactSummary", v)?;
            }
            map.serialize_entry("uuid", &self.uuid)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
        } else if is_assistant && is_api_error {
            // (e1) assistant api-error head: type, uuid, timestamp, message,
            //      requestId?, error?, errorDetails?, truncatedAfterOutput?,
            //      isApiErrorMessage, apiErrorStatus?.
            // `truncatedAfterOutput` sits after `errorDetails` and before
            // `isApiErrorMessage` (cc 2.1.263 `Ggr`).
            map.serialize_entry("type", &self.message_type)?;
            map.serialize_entry("uuid", &self.uuid)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
            map.serialize_entry("message", &self.message)?;
            if let Some(v) = self.extra.get("requestId") {
                map.serialize_entry("requestId", v)?;
            }
            if let Some(v) = self.extra.get("error") {
                map.serialize_entry("error", v)?;
            }
            if let Some(v) = self.extra.get("errorDetails") {
                map.serialize_entry("errorDetails", v)?;
            }
            if let Some(v) = self.extra.get("truncatedAfterOutput") {
                map.serialize_entry("truncatedAfterOutput", v)?;
            }
            // isApiErrorMessage is guaranteed present (gated `is_api_error`).
            if let Some(v) = self.extra.get("isApiErrorMessage") {
                map.serialize_entry("isApiErrorMessage", v)?;
            }
            if let Some(v) = self.extra.get("apiErrorStatus") {
                map.serialize_entry("apiErrorStatus", v)?;
            }
        } else if is_assistant {
            // (e2) assistant normal head: message, requestId?, type, uuid,
            //      timestamp, effort?. `effort` (2.1.212) is the last field of
            //      the in-memory assistant message object `d` (after
            //      `advisorModel`), spread into the transcript record before the
            //      `userType`/`cwd`/`version`/`gitBranch` trailer — the level
            //      string from `Y4n(effort).level`, emitted only when present
            //      (claude's `...effort!==void 0&&{effort}` guard).
            map.serialize_entry("message", &self.message)?;
            if let Some(v) = self.extra.get("requestId") {
                map.serialize_entry("requestId", v)?;
            }
            map.serialize_entry("type", &self.message_type)?;
            map.serialize_entry("uuid", &self.uuid)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
            if let Some(v) = self.extra.get("effort") {
                map.serialize_entry("effort", v)?;
            }
        } else if is_attachment {
            // (d2) attachment head: attachment, type, uuid, timestamp — the
            //      payload leads, BEFORE `type` (real 2.1.220 transcripts:
            //      every one of the 26 048 mined attachment lines orders them
            //      `parentUuid, isSidechain, attachment, type, uuid,
            //      timestamp, …trailer`). Attachment lines carry NO inner
            //      `message`, so `self.message` (Null) is skipped like the
            //      compact-boundary arm.
            if let Some(v) = self.extra.get("attachment") {
                map.serialize_entry("attachment", v)?;
            }
            map.serialize_entry("type", &self.message_type)?;
            map.serialize_entry("uuid", &self.uuid)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
        } else if is_scheduled_fire {
            map.serialize_entry("type", &self.message_type)?;
            for key in ["subtype", "content", "isMeta"] {
                if let Some(value) = self.extra.get(key) {
                    map.serialize_entry(key, value)?;
                }
            }
            map.serialize_entry("timestamp", &self.timestamp)?;
            map.serialize_entry("uuid", &self.uuid)?;
            for key in &SCHEDULED_FIRE_HEAD[3..] {
                if let Some(value) = self.extra.get(*key) {
                    map.serialize_entry(*key, value)?;
                }
            }
        } else if is_compact_boundary {
            // (f1) compact-boundary system head — claude's flattened envelope
            //      (real 2.1.207 transcripts): type, subtype, content,
            //      [isMeta?,] level, compactMetadata, uuid, timestamp. NO
            //      inner `message` — claude never writes one on boundary
            //      lines, so `self.message` (Null) is intentionally skipped.
            //      (`isMeta` only appears on pre-2.1.199 boundary lines; kept
            //      near its historical slot for tolerant round-trips.)
            map.serialize_entry("type", &self.message_type)?;
            if let Some(v) = self.extra.get("subtype") {
                map.serialize_entry("subtype", v)?;
            }
            if let Some(v) = self.extra.get("content") {
                map.serialize_entry("content", v)?;
            }
            if let Some(v) = self.extra.get("isMeta") {
                map.serialize_entry("isMeta", v)?;
            }
            if let Some(v) = self.extra.get("level") {
                map.serialize_entry("level", v)?;
            }
            if let Some(v) = self.extra.get("compactMetadata") {
                map.serialize_entry("compactMetadata", v)?;
            }
            map.serialize_entry("uuid", &self.uuid)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
        } else {
            // (f) system + any other kind: type, message, isMeta?, uuid,
            //     timestamp. The port emits an inner `message`; claude's
            //     flattened system envelope (subtype/content as top-level
            //     siblings) is ported ONLY for the compact-boundary subtype
            //     (arm f1); other system subtypes tail-append their `extra`
            //     keys as before.
            map.serialize_entry("type", &self.message_type)?;
            map.serialize_entry("message", &self.message)?;
            if let Some(v) = self.extra.get("isMeta") {
                map.serialize_entry("isMeta", v)?;
            }
            map.serialize_entry("uuid", &self.uuid)?;
            map.serialize_entry("timestamp", &self.timestamp)?;
        }

        // (g2) Tool-result head — claude emits these BETWEEN `timestamp` and the
        //      common trailer, in the fixed order of [`TOOL_RESULT_HEAD_EXTRA`]
        //      rather than in `extra` insertion order, so a caller that sets
        //      them in any order writes the same bytes. Each is skipped when
        //      absent; a line carrying none of them is byte-unchanged.
        for key in TOOL_RESULT_HEAD_EXTRA {
            if let Some(v) = self.extra.get(*key) {
                map.serialize_entry(*key, v)?;
            }
        }

        // (h) Common trailer — same emit/skip predicates as the derived impl,
        //     only the POSITION moves (it now follows the per-kind envelope).
        //
        //     `sessionKind` leads it: the oracle's chain-entry literal
        //     (@296794533) is `…, agentId, ...g, sessionKind:a3e(), userType,
        //     entrypoint, cwd, …`. Carried in `extra` (not a named field)
        //     because that is the channel the `/resume` daemon filter in
        //     `loader.rs` already reads it back through.
        if let Some(v) = self.extra.get(SESSION_KIND_KEY) {
            map.serialize_entry(SESSION_KIND_KEY, v)?;
        }
        if let Some(v) = &self.user_type {
            map.serialize_entry("userType", v)?;
        }
        if let Some(v) = &self.entrypoint {
            map.serialize_entry("entrypoint", v)?;
        }
        map.serialize_entry("cwd", &self.cwd)?;
        map.serialize_entry("sessionId", &self.session_id)?;
        map.serialize_entry("version", &self.version)?;
        if let Some(v) = &self.git_branch {
            map.serialize_entry("gitBranch", v)?;
        }
        if let Some(v) = &self.slug {
            map.serialize_entry("slug", v)?;
        }

        // (i) Tail — any UNRECOGNIZED extra key (unported claude field), in
        //     `extra` iteration order, so round-trips of those survive. Keys a
        //     per-kind head already emitted are skipped ONLY for that kind.
        for (k, v) in &self.extra {
            if RECOGNIZED_EXTRA.contains(&k.as_str()) {
                continue;
            }
            if is_user && USER_HEAD_EXTRA.contains(&k.as_str()) {
                continue;
            }
            if is_scheduled_fire && SCHEDULED_FIRE_HEAD.contains(&k.as_str()) {
                continue;
            }
            if is_compact_boundary && BOUNDARY_HEAD_EXTRA.contains(&k.as_str()) {
                continue;
            }
            if is_attachment && k == "attachment" {
                continue;
            }
            // Already emitted by the tool-result head above.
            if TOOL_RESULT_HEAD_EXTRA.contains(&k.as_str()) {
                continue;
            }
            map.serialize_entry(k, v)?;
        }

        map.end()
    }
}

#[cfg(test)]
mod attachment_envelope_tests {
    use super::JsonlMessage;

    /// Real 2.1.220 `attachment` lines put the `attachment` payload BEFORE the
    /// `type` discriminator — outer key order
    /// `parentUuid, isSidechain, attachment, type, uuid, timestamp, userType,
    /// entrypoint, cwd, sessionId, version, gitBranch[, slug]`
    /// (26 048 attachment lines mined from `~/.claude/projects/**/*.jsonl`;
    /// the dominant envelope, 12 607 lines, is exactly this plus the unported
    /// `session_id` / `sessionKind` / `slug` siblings).
    #[test]
    fn attachment_line_emits_payload_before_type() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "attachment".to_string(),
            serde_json::json!({"type": "hook_success", "hookName": "Stop"}),
        );
        let msg = JsonlMessage {
            message_type: "attachment".to_string(),
            uuid: "84c51927-5550-4d0c-bdff-16c5699c7d4c".to_string(),
            parent_uuid: Some("9fccb389-ff4d-4502-b0c4-e9ecb4459013".to_string()),
            session_id: "931e2281-6acb-4c36-8352-a2378fd6c88d".to_string(),
            timestamp: "2026-07-27T18:34:21.476Z".to_string(),
            cwd: "/private/tmp".to_string(),
            version: "0.12.0".to_string(),
            message: serde_json::Value::Null,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: Some("HEAD".to_string()),
            entrypoint: Some("cli".to_string()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"parentUuid":"9fccb389-ff4d-4502-b0c4-e9ecb4459013","isSidechain":false,"attachment":{"type":"hook_success","hookName":"Stop"},"type":"attachment","uuid":"84c51927-5550-4d0c-bdff-16c5699c7d4c","timestamp":"2026-07-27T18:34:21.476Z","userType":"external","entrypoint":"cli","cwd":"/private/tmp","sessionId":"931e2281-6acb-4c36-8352-a2378fd6c88d","version":"0.12.0","gitBranch":"HEAD"}"#
        );
    }

    /// An attachment line carries NO inner `message` — the generic arm's
    /// `"message":null` must not leak onto it.
    #[test]
    fn attachment_line_has_no_inner_message_key() {
        let mut extra = serde_json::Map::new();
        extra.insert("attachment".to_string(), serde_json::json!({"type": "x"}));
        let msg = JsonlMessage {
            message_type: "attachment".to_string(),
            uuid: "u".to_string(),
            parent_uuid: None,
            session_id: "s".to_string(),
            timestamp: "t".to_string(),
            cwd: "/c".to_string(),
            version: "v".to_string(),
            message: serde_json::Value::Null,
            is_sidechain: false,
            user_type: None,
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(!s.contains("\"message\""), "no inner message key: {s}");
        // (`"attachment"` alone also matches the `"type":"attachment"` value,
        // so key on the payload's opening brace.)
        assert_eq!(
            s.matches("\"attachment\":{").count(),
            1,
            "payload emitted once, not tail-appended a second time: {s}"
        );
    }

    /// SC-07. `sessionKind` sits between the per-kind envelope and `userType`
    /// — the oracle's chain-entry literal is
    /// `…, sessionKind:a3e(), userType, entrypoint, cwd, sessionId, …`.
    /// It must NOT fall out of the unrecognized-key tail after `gitBranch`.
    #[test]
    fn session_kind_leads_the_common_trailer() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            super::SESSION_KIND_KEY.to_string(),
            serde_json::json!("daemon-worker"),
        );
        let msg = JsonlMessage {
            message_type: "user".to_string(),
            uuid: "u".to_string(),
            parent_uuid: None,
            session_id: "s".to_string(),
            timestamp: "t".to_string(),
            cwd: "/c".to_string(),
            version: "v".to_string(),
            message: serde_json::json!({"role": "user", "content": "hi"}),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: Some("main".to_string()),
            entrypoint: Some("cli".to_string()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(
            s.contains(
                r#""sessionKind":"daemon-worker","userType":"external","entrypoint":"cli","cwd":"/c""#
            ),
            "sessionKind must lead the trailer: {s}"
        );
        assert_eq!(s.matches("\"sessionKind\"").count(), 1, "emitted once: {s}");
    }

    /// A line with no session kind is byte-unchanged — the key is dropped, not
    /// emitted as `null` (`a3e()` returns `undefined`, which `JSON.stringify`
    /// omits).
    #[test]
    fn absent_session_kind_emits_no_key() {
        let msg = JsonlMessage {
            message_type: "user".to_string(),
            uuid: "u".to_string(),
            parent_uuid: None,
            session_id: "s".to_string(),
            timestamp: "t".to_string(),
            cwd: "/c".to_string(),
            version: "v".to_string(),
            message: serde_json::json!({"role": "user"}),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: serde_json::Map::new(),
        };
        assert!(!serde_json::to_string(&msg).unwrap().contains("sessionKind"));
    }
}

// The tool-result head: claude places `toolUseResult`, `toolDenialKind` and
// `sourceToolAssistantUUID` BETWEEN `timestamp` and the common trailer, not
// after it. Census of every `toolDenialKind`-bearing line in real 2.1.220
// transcripts (14 records, 5 envelope variants differing only in the optional
// `agentId` / `session_id` / `slug`) agrees on:
//
//   …, uuid, timestamp, toolUseResult, toolDenialKind, mcpMeta, toolEndsTurn,
//   sourceToolAssistantUUID, userType, entrypoint, cwd, sessionId, version,
//   gitBranch[, slug]
#[cfg(test)]
mod tool_result_head_tests {
    use super::JsonlMessage;
    use serde_json::{json, Map};

    fn base() -> JsonlMessage {
        JsonlMessage {
            parent_uuid: Some("p".into()),
            is_sidechain: false,
            message_type: "user".into(),
            message: json!({"role":"user","content":[]}),
            uuid: "u".into(),
            timestamp: "T".into(),
            user_type: Some("external".into()),
            entrypoint: Some("cli".into()),
            cwd: "/w".into(),
            session_id: "s".into(),
            version: "0.12.0".into(),
            git_branch: None,
            slug: None,
            logical_parent_uuid: None,
            prompt_id: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn denial_head_keys_precede_the_common_trailer() {
        let mut m = base();
        m.extra
            .insert("toolUseResult".into(), json!("Error: denied"));
        m.extra
            .insert("toolDenialKind".into(), json!("permission-rule"));
        m.extra
            .insert("sourceToolAssistantUUID".into(), json!("a-uuid"));
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"parentUuid":"p","isSidechain":false,"type":"user","message":{"role":"user","content":[]},"uuid":"u","timestamp":"T","toolUseResult":"Error: denied","toolDenialKind":"permission-rule","sourceToolAssistantUUID":"a-uuid","userType":"external","entrypoint":"cli","cwd":"/w","sessionId":"s","version":"0.12.0"}"#
        );
    }

    /// Order among the head keys is claude's, NOT the insertion order of the
    /// `extra` map — a caller that sets them in a different order still writes
    /// the same bytes.
    #[test]
    fn head_key_order_is_fixed_regardless_of_insertion_order() {
        let mut m = base();
        m.extra
            .insert("sourceToolAssistantUUID".into(), json!("a-uuid"));
        m.extra.insert("toolDenialKind".into(), json!("cancelled"));
        m.extra.insert("toolUseResult".into(), json!("x"));
        let s = serde_json::to_string(&m).unwrap();
        let i_res = s.find("toolUseResult").unwrap();
        let i_kind = s.find("toolDenialKind").unwrap();
        let i_src = s.find("sourceToolAssistantUUID").unwrap();
        let i_trailer = s.find("userType").unwrap();
        assert!(
            i_res < i_kind && i_kind < i_src && i_src < i_trailer,
            "claude order is toolUseResult < toolDenialKind < sourceToolAssistantUUID < trailer, got {s}"
        );
    }

    /// `mcpMeta` and `toolEndsTurn` ride BETWEEN `toolDenialKind` and
    /// `sourceToolAssistantUUID`.
    ///
    /// Oracle: the in-memory user-message factory `zr()` (2.1.220 BIN off
    /// **238011637**) fixes the field order
    /// `… uuid, timestamp, toolUseResult, classifierMetaLines, toolDenialKind,
    /// userFeedback, mcpMeta, toolEndsTurn, imagePasteIds,
    /// sourceToolAssistantUUID, permissionMode, …`; `JSON.stringify` drops the
    /// `undefined` slots, so the on-disk order falls straight out. Confirmed
    /// on disk by 2 146 real 2.1.220 lines carrying the adjacent tuple
    /// `[toolEndsTurn, sourceToolAssistantUUID, userType]`.
    #[test]
    fn mcp_meta_and_ends_turn_ride_between_denial_kind_and_source() {
        let mut m = base();
        m.extra
            .insert("sourceToolAssistantUUID".into(), json!("a-uuid"));
        m.extra.insert("toolEndsTurn".into(), json!(true));
        m.extra
            .insert("mcpMeta".into(), json!({"_meta":{"claude/endTurn":true}}));
        m.extra
            .insert("toolDenialKind".into(), json!("permission-rule"));
        m.extra.insert("toolUseResult".into(), json!("x"));
        let s = serde_json::to_string(&m).unwrap();
        let i_res = s.find("toolUseResult").unwrap();
        let i_kind = s.find("toolDenialKind").unwrap();
        let i_mcp = s.find("mcpMeta").unwrap();
        let i_ends = s.find("toolEndsTurn").unwrap();
        let i_src = s.find("sourceToolAssistantUUID").unwrap();
        let i_trailer = s.find("userType").unwrap();
        assert!(
            i_res < i_kind
                && i_kind < i_mcp
                && i_mcp < i_ends
                && i_ends < i_src
                && i_src < i_trailer,
            "claude order is toolUseResult < toolDenialKind < mcpMeta < toolEndsTurn < sourceToolAssistantUUID < trailer, got {s}"
        );
    }

    #[test]
    fn permission_mode_follows_source_uuid_before_the_common_trailer() {
        let mut m = base();
        m.extra
            .insert("sourceToolAssistantUUID".into(), json!("a-uuid"));
        m.extra.insert("permissionMode".into(), json!("plan"));
        let s = serde_json::to_string(&m).unwrap();
        assert!(
            s.find("sourceToolAssistantUUID").unwrap() < s.find("permissionMode").unwrap()
                && s.find("permissionMode").unwrap() < s.find("userType").unwrap(),
            "Claude order is sourceToolAssistantUUID < permissionMode < trailer, got {s}"
        );
    }

    /// A line carrying none of them is unchanged.
    #[test]
    fn absent_head_keys_change_nothing() {
        let s = serde_json::to_string(&base()).unwrap();
        assert!(!s.contains("toolDenialKind"));
        assert!(s.contains(r#""timestamp":"T","userType":"external""#));
    }
}

#[cfg(test)]
#[test]
fn scheduled_fire_matches_2_1_270_oracle_bytes() {
    for expected in include_str!("../../tests/fixtures/loop-2.1.270/scheduled_fire.jsonl").lines() {
        let row: JsonlMessage = serde_json::from_str(expected).unwrap();
        assert_eq!(serde_json::to_string(&row).unwrap(), expected);
    }
}

#[cfg(test)]
#[test]
fn loop_turn_companion_matches_2_1_270_oracle_bytes() {
    let expected = include_str!("../../tests/fixtures/loop-2.1.270/turn_companion.jsonl").trim();
    let row: JsonlMessage = serde_json::from_str(expected).unwrap();
    assert_eq!(serde_json::to_string(&row).unwrap(), expected);
}
