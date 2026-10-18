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

use crate::jsonl::exact_json::{
    message_source_key_order, set_message_source_key_order, PRIVATE_SOURCE_KEY_ORDER_KEY,
    PRIVATE_UTF16_KEY,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;
#[cfg(test)]
use serde_json::Value;

fn deserialize_attachment_projection<'de, D>(
    deserializer: D,
) -> Result<Option<Utf16JsonProjection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    Utf16JsonProjection::parse(raw.get())
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn deserialize_message_projection<'de, D>(deserializer: D) -> Result<Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom)
}

fn deserialize_present_projection<'de, D>(
    deserializer: D,
) -> Result<Option<Utf16JsonProjection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    Utf16JsonProjection::parse(raw.get())
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn null_json_projection() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

struct RawProjection<'a>(&'a Utf16JsonProjection);

impl Serialize for RawProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let source = self.0.to_json_string().map_err(serde::ser::Error::custom)?;
        let raw = RawValue::from_string(source).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

#[cfg(test)]
fn test_projection(value: Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).unwrap()
}

/// One line of the session JSONL file.
///
/// Outer field NAMES **and ORDER** match claude: the hand-written
/// [`impl Serialize for JsonlMessage`] emits claude's per-kind envelope order
/// (see the module docs) rather than this struct's declaration order — so the
/// on-disk byte layout is 1:1, not just the field set. (This corrects an
/// earlier note that claimed the order "does not yet" match; the hand-written
/// `Serialize` closed that gap.) The `message` field stays in the flat exact JSON carrier
/// because its inner schema depends on `type` (Anthropic
/// Messages API for `user`/`assistant`, claude-code internal shapes for
/// `system`/`attachment`). All un-named outer fields land in `extra` via
/// `#[serde(flatten)]` so read→write round-trips preserve every byte we read.
#[derive(Debug, Clone)]
pub struct JsonlMessage {
    /// `"user" | "assistant" | "system" | "attachment" | "summary" | ...`
    /// — see `claude-code/src/types/logs.ts:297` `Entry` union.
    pub message_type: String,

    /// Stable identifier for this entry. UUID v4 lowercase.
    pub uuid: String,

    /// Parent entry's `uuid`, or JSON `null` for the first turn.
    /// MUST serialize as `null` (NOT omitted) — `claude-code/src/types/logs.ts:222`.
    pub parent_uuid: Option<String>,

    /// Session UUID. Matches the filename without `.jsonl`.
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
    pub timestamp: String,

    /// Canonical absolute cwd at the time this entry was written.
    /// TOLERANT (`#[serde(default)]`): present on `user`/`assistant`, may be
    /// absent on `attachment`/`system`.
    pub cwd: String,

    /// Engine version string. claude-code: `MACRO.VERSION`; lingxi-core: `CARGO_PKG_VERSION`.
    /// TOLERANT (`#[serde(default)]`) for the same reason as `cwd`.
    pub version: String,

    /// Inner message object — Anthropic Messages API shape for `user`/`assistant`,
    /// other shapes for system entries. Kept in the flat exact JSON carrier.
    ///
    /// TOLERANT: `attachment` and `system` chain-participant lines frequently
    /// lack a `message` field entirely (their payload lives in sibling outer
    /// fields captured by [`Self::extra`]). `#[serde(default)]` yields
    /// a JSON null projection when absent so the line still parses and joins the chain,
    /// matching `claude-code`'s `isTranscriptMessage` (it gates on `type` only,
    /// never on the presence of `message`).
    pub message: Utf16JsonProjection,

    /// Process-only flat attachment payload. The raw JSON member is captured
    /// before typed `extra` values are deserialized, then emitted in Native's
    /// attachment position without converting the payload to `Value`.
    pub attachment_projection: Option<Utf16JsonProjection>,

    /// Exact process-only capture of Native's raw tool result payload. The
    /// outer JSONL field remains one ordinary JSON value; it is parsed before
    /// the flattened typed-metadata map can recursively materialize it.
    pub tool_use_result_projection: Option<Utf16JsonProjection>,

    /// Process-only flat capture of arbitrary MCP metadata. It shares Native's
    /// top-level `mcpMeta` JSON value shape and stays outside recursive `extra`.
    pub mcp_meta_projection: Option<Utf16JsonProjection>,

    /// Native outer user-row turn-companion marker, retaining absence versus false.
    pub turn_companion: Option<Option<bool>>,

    /// Native outer user-row note, retaining absence versus explicit null.
    pub usage_limit_note: Option<Option<lingxi_core::types::messages::UsageLimitNote>>,

    /// Exact Native origin object, such as session task notification provenance.
    pub origin_projection: Option<Utf16JsonProjection>,

    /// Exact Native turn-origin string tag.
    pub turn_origin_projection: Option<Utf16JsonProjection>,

    // The Anthropic `request-id` response header (`req_…`) is persisted as the
    // top-level `requestId` field on REAL assistant lines (the SDK's
    // `response._request_id`). It is carried through [`Self::extra`] (the same
    // flatten channel as `isMeta`), set by the writer only for real assistant
    // lines — so no struct-literal constructor churn, and read-side round-trips
    // it transparently. See `orchestrator::conversation::to_jsonl_message_with_inner_id`.
    /// `false` for the main agent loop; `true` for sub-agent transcripts.
    /// Optional in the schema but emitted by claude-code's writer; we default
    /// to `false` on write and accept missing on read.
    pub is_sidechain: bool,

    /// `process.env.USER_TYPE || "external"` — lingxi-core lock: `"external"`.
    pub user_type: Option<String>,

    /// Git branch at write time, when available.
    /// `getBranch()` once per chain (`sessionStorage.ts:1012-1019`); on a
    /// non-repo / git failure the writer leaves it `None` → omitted.
    pub git_branch: Option<String>,

    /// CLI entrypoint string — `getEntrypoint()` (`sessionStorage.ts:1058`),
    /// e.g. `"cli"`. Stamped on EVERY line by claude-code's writer. Optional on
    /// read (legacy rows omit it); `skip_serializing_if = "Option::is_none"`
    /// so an unset value is omitted, matching TS `undefined`.
    pub entrypoint: Option<String>,

    /// Per-session plan slug — `getPlanSlugCache().get(sessionId)`
    /// (`sessionStorage.ts:1023, 1063`). `undefined` when the session has no
    /// plan slug; omitted on write when `None`.
    pub slug: Option<String>,

    /// Stable per-prompt id — `getPromptId()` (`sessionStorage.ts:1045-1046`),
    /// set ONLY on `user` lines (TS: `type === 'user' ? getPromptId() : undefined`).
    /// `None` (omitted) on every non-`user` line and on `user` lines when no
    /// prompt id is available.
    pub prompt_id: Option<String>,

    /// Compact-boundary back-link — `logicalParentUuid`
    /// (`sessionStorage.ts:1041`). On a compaction boundary TS sets
    /// `parentUuid: null` and stashes the real parent here
    /// (`isCompactBoundary ? parentUuid : undefined`); on every other line it is
    /// `undefined`. Omitted on write when `None`.
    pub logical_parent_uuid: Option<String>,

    /// All other outer fields (`agentId`, `agentName`, `agentColor`,
    /// `teamName`, `isMeta`, `toolUseResult`, ...) preserved verbatim across
    /// read->write so round-trips are byte-equivalent.
    pub extra: Utf16JsonProjection,
}

impl JsonlMessage {
    /// Parse a complete raw JSONL row into its typed scalar fields and flat
    /// exact payload projections. No member is materialized as Value.
    pub fn from_projection(row: &Utf16JsonProjection) -> Result<Self, String> {
        let string_field = |key: &str| -> Result<String, String> {
            row.get_property(key)
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .ok_or_else(|| format!("JSONL field {key} must be a string"))
        };
        let optional_string = |key: &str| -> Result<Option<String>, String> {
            match row.get_property(key) {
                None => Ok(None),
                Some(value) if value.is_null() => Ok(None),
                Some(value) => value
                    .as_str()
                    .map(|value| Some(value.to_owned()))
                    .ok_or_else(|| format!("JSONL field {key} must be a string or null")),
            }
        };
        let nullable_bool = |key: &str| -> Result<Option<Option<bool>>, String> {
            match row.get_property(key) {
                None => Ok(None),
                Some(value) if value.is_null() => Ok(Some(None)),
                Some(value) => value
                    .as_bool()
                    .map(|value| Some(Some(value)))
                    .ok_or_else(|| format!("JSONL field {key} must be a boolean or null")),
            }
        };
        let optional_projection = |key: &str| -> Result<Option<Utf16JsonProjection>, String> {
            if row.get_property(key).is_none() {
                return Ok(None);
            }
            row.subprojection_property_units(&key.encode_utf16().collect::<Vec<_>>())
                .map(Some)
                .map_err(|error| format!("invalid JSONL field {key}: {error}"))
        };
        let nullable_string =
            |key: &str| -> Result<Option<String>, String> { optional_string(key) };
        let message = optional_projection("message")?.unwrap_or_else(null_json_projection);
        let turn_companion = nullable_bool("turnCompanion")?;
        let usage_limit_note = match row.get_property("usageLimitNote") {
            None => None,
            Some(value) if value.is_null() => Some(None),
            Some(value) => {
                let text = value.as_str().ok_or_else(|| {
                    "JSONL field usageLimitNote must be a string or null".to_owned()
                })?;
                let parsed = match text {
                    "wrap_up" => lingxi_core::types::messages::UsageLimitNote::WrapUp,
                    "release" => lingxi_core::types::messages::UsageLimitNote::Release,
                    "near_limit" => lingxi_core::types::messages::UsageLimitNote::NearLimit,
                    _ => return Err("unknown JSONL usageLimitNote".into()),
                };
                Some(Some(parsed))
            }
        };

        let mut extra_fields = Vec::new();
        let mut source_key_order = Vec::new();
        let source_order_key_units = PRIVATE_SOURCE_KEY_ORDER_KEY
            .encode_utf16()
            .collect::<Vec<_>>();
        let utf16_key_units = PRIVATE_UTF16_KEY.encode_utf16().collect::<Vec<_>>();
        let known = [
            "type",
            "uuid",
            "parentUuid",
            "sessionId",
            "timestamp",
            "cwd",
            "version",
            "message",
            "attachment",
            "toolUseResult",
            "mcpMeta",
            "isSidechain",
            "userType",
            "gitBranch",
            "entrypoint",
            "slug",
            "promptId",
            "logicalParentUuid",
            "turnCompanion",
            "usageLimitNote",
            "origin",
            "turnOrigin",
        ];
        let known_units = known
            .iter()
            .map(|key| key.encode_utf16().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        for entry in row
            .object_entries()
            .ok_or_else(|| "JSONL row must be an object".to_owned())?
        {
            if entry.code_units == source_order_key_units || entry.code_units == utf16_key_units {
                continue;
            }
            source_key_order.push(entry.code_units.to_vec());
            if known_units.iter().any(|key| key == entry.code_units) {
                continue;
            }
            let value = row
                .subprojection_property_units(entry.code_units)
                .map_err(|error| format!("invalid JSONL extra field: {error}"))?;
            extra_fields.push((entry.code_units.to_vec(), value));
        }

        let is_sidechain = match row.get_property("isSidechain") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "JSONL field isSidechain must be a boolean".to_owned())?,
        };
        let mut message = Self {
            message_type: string_field("type")?,
            uuid: string_field("uuid")?,
            parent_uuid: nullable_string("parentUuid")?,
            session_id: string_field("sessionId")?,
            timestamp: optional_string("timestamp")?.unwrap_or_default(),
            cwd: optional_string("cwd")?.unwrap_or_default(),
            version: optional_string("version")?.unwrap_or_default(),
            message,
            attachment_projection: optional_projection("attachment")?,
            tool_use_result_projection: optional_projection("toolUseResult")?,
            mcp_meta_projection: optional_projection("mcpMeta")?,
            turn_companion,
            usage_limit_note,
            origin_projection: optional_projection("origin")?,
            turn_origin_projection: optional_projection("turnOrigin")?,
            is_sidechain,
            user_type: optional_string("userType")?,
            git_branch: optional_string("gitBranch")?,
            entrypoint: optional_string("entrypoint")?,
            slug: optional_string("slug")?,
            prompt_id: optional_string("promptId")?,
            logical_parent_uuid: optional_string("logicalParentUuid")?,
            extra: Utf16JsonProjection::object_units(extra_fields)
                .map_err(|error| format!("invalid JSONL extra object: {error}"))?,
        };
        set_message_source_key_order(&mut message, source_key_order);
        Ok(message)
    }
}

impl<'de> Deserialize<'de> for JsonlMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let projection = Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom)?;
        Self::from_projection(&projection).map_err(serde::de::Error::custom)
    }
}

/// `extra` keys this serializer pulls into their claude-ordered positions.
/// Anything NOT in this set is appended verbatim after the common trailer
/// (forward-compat for unported claude fields like `agentId`/`toolUseResult`).
const RECOGNIZED_EXTRA: &[&str] = &[
    "attachment",
    "toolUseResult",
    "mcpMeta",
    "origin",
    "isMeta",
    "requestId",
    "error",
    "errorDetails",
    "truncatedAfterOutput",
    "isApiErrorMessage",
    "apiErrorStatus",
    "effort",
    "perTurnEffort",
    "promptSource",
    "interruptedMessageId",
    "interruptedByShutdown",
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
    "usageLimitNote",
];

/// `extra` keys emitted BETWEEN `timestamp` and the common trailer, in this
/// exact order — claude's tool-result/interruption head.
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

const MODEL_FALLBACK_HEAD: &[&str] = &[
    "subtype",
    "content",
    "level",
    "trigger",
    "originalModel",
    "fallbackModel",
    "isMeta",
];

const REFUSAL_FALLBACK_HEAD: &[&str] = &[
    "subtype",
    "direction",
    "scope",
    "content",
    "level",
    "trigger",
    "originalModel",
    "fallbackModel",
    "requestId",
    "apiRefusalCategory",
    "apiRefusalExplanation",
    "retractedMessageUuids",
    "refusedUserMessageUuid",
    "isMeta",
];

// Hand-written `Serialize` so the outer JSONL keys land in claude-code's EXACT
// per-kind order (see module docs). The source-aware `Deserialize` is order-
// independent. Values are byte-identical to the typed schema; only key
// POSITION changes. Skip predicates mirror the struct's `skip_serializing_if`
// so presence parity is preserved.
impl Serialize for JsonlMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fields = Vec::<(Vec<u16>, Utf16JsonProjection)>::new();
        let is_user = self.message_type == "user";
        let is_assistant = self.message_type == "assistant";
        let is_system = self.message_type == "system";
        let is_attachment = self.message_type == "attachment";
        let is_api_error = self.extra.contains_property("isApiErrorMessage");
        let subtype = self.extra.string_at("/subtype");
        let is_scheduled_fire = is_system && subtype == Some("scheduled_task_fire");
        let is_model_fallback = is_system && subtype == Some("model_fallback");
        let is_refusal_fallback = is_system && subtype == Some("model_refusal_fallback");
        let is_compact_boundary = is_system && subtype == Some("compact_boundary");

        push_projection(
            &mut fields,
            "parentUuid",
            option_string_projection(self.parent_uuid.as_deref()),
        );
        if self.logical_parent_uuid.is_some() {
            push_projection(
                &mut fields,
                "logicalParentUuid",
                option_string_projection(self.logical_parent_uuid.as_deref()),
            );
        }
        push_projection(
            &mut fields,
            "isSidechain",
            bool_projection(self.is_sidechain),
        );

        if is_user {
            if let Some(prompt_id) = &self.prompt_id {
                push_projection(&mut fields, "promptId", string_projection(prompt_id));
            }
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            push_projection(&mut fields, "message", self.message.clone());
            push_extra_projection(&mut fields, &self.extra, "isMeta");
            if let Some(present) = self.turn_companion {
                push_projection(
                    &mut fields,
                    "turnCompanion",
                    present.map(bool_projection).unwrap_or_else(null_projection),
                );
            }
            if let Some(present) = self.usage_limit_note {
                let note = match present {
                    None => null_projection(),
                    Some(lingxi_core::types::messages::UsageLimitNote::WrapUp) => {
                        string_projection("wrap_up")
                    }
                    Some(lingxi_core::types::messages::UsageLimitNote::Release) => {
                        string_projection("release")
                    }
                    Some(lingxi_core::types::messages::UsageLimitNote::NearLimit) => {
                        string_projection("near_limit")
                    }
                };
                push_projection(&mut fields, "usageLimitNote", note);
            }
            push_extra_projection(&mut fields, &self.extra, "isVisibleInTranscriptOnly");
            push_extra_projection(&mut fields, &self.extra, "isCompactSummary");
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
            // Native 2.1.295 writes the safety-refusal response id on its
            // retry nudge immediately after the UUID/timestamp pair.
            push_extra_projection(&mut fields, &self.extra, "refusedMessageId");
        } else if is_assistant && is_api_error {
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
            push_projection(&mut fields, "message", self.message.clone());
            for key in [
                "requestId",
                "error",
                "errorDetails",
                "truncatedAfterOutput",
                "isApiErrorMessage",
                "apiErrorStatus",
                "refusedMessageId",
            ] {
                push_extra_projection(&mut fields, &self.extra, key);
            }
        } else if is_assistant {
            push_projection(&mut fields, "message", self.message.clone());
            push_extra_projection(&mut fields, &self.extra, "requestId");
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
            push_extra_projection(&mut fields, &self.extra, "effort");
            push_extra_projection(&mut fields, &self.extra, "perTurnEffort");
            push_extra_projection(&mut fields, &self.extra, "supersedesUuids");
        } else if is_attachment {
            if let Some(projection) = &self.attachment_projection {
                push_projection(&mut fields, "attachment", projection.clone());
            }
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
            push_extra_projection(&mut fields, &self.extra, "rendered");
            push_extra_projection(&mut fields, &self.extra, "renderedRole");
        } else if is_refusal_fallback {
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            for key in REFUSAL_FALLBACK_HEAD {
                push_extra_projection(&mut fields, &self.extra, key);
            }
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
        } else if is_model_fallback {
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            for key in MODEL_FALLBACK_HEAD {
                push_extra_projection(&mut fields, &self.extra, key);
            }
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
        } else if is_scheduled_fire {
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            for key in ["subtype", "content", "isMeta"] {
                push_extra_projection(&mut fields, &self.extra, key);
            }
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            for key in &SCHEDULED_FIRE_HEAD[3..] {
                push_extra_projection(&mut fields, &self.extra, key);
            }
        } else if is_compact_boundary {
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            for key in ["subtype", "content", "isMeta", "level", "compactMetadata"] {
                push_extra_projection(&mut fields, &self.extra, key);
            }
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
        } else {
            push_projection(&mut fields, "type", string_projection(&self.message_type));
            push_projection(&mut fields, "message", self.message.clone());
            push_extra_projection(&mut fields, &self.extra, "isMeta");
            push_projection(&mut fields, "uuid", string_projection(&self.uuid));
            push_projection(&mut fields, "timestamp", string_projection(&self.timestamp));
        }

        if let Some(projection) = &self.tool_use_result_projection {
            push_projection(&mut fields, "toolUseResult", projection.clone());
        }
        for key in TOOL_RESULT_HEAD_EXTRA {
            if *key == "mcpMeta" {
                if let Some(projection) = &self.mcp_meta_projection {
                    push_projection(&mut fields, "mcpMeta", projection.clone());
                }
            } else {
                push_extra_projection(&mut fields, &self.extra, key);
            }
        }
        if let Some(origin) = &self.origin_projection {
            push_projection(&mut fields, "origin", origin.clone());
        }
        for key in [
            "promptSource",
            "interruptedMessageId",
            "interruptedByShutdown",
        ] {
            push_extra_projection(&mut fields, &self.extra, key);
        }

        if let Some(value) = extra_projection(&self.extra, SESSION_KIND_KEY) {
            push_projection(&mut fields, SESSION_KIND_KEY, value);
        }
        if let Some(value) = &self.user_type {
            push_projection(&mut fields, "userType", string_projection(value));
        }
        if let Some(value) = &self.entrypoint {
            push_projection(&mut fields, "entrypoint", string_projection(value));
        }
        push_projection(&mut fields, "cwd", string_projection(&self.cwd));
        push_projection(
            &mut fields,
            "sessionId",
            string_projection(&self.session_id),
        );
        push_projection(&mut fields, "version", string_projection(&self.version));
        if let Some(value) = &self.git_branch {
            push_projection(&mut fields, "gitBranch", string_projection(value));
        }
        if let Some(value) = &self.slug {
            push_projection(&mut fields, "slug", string_projection(value));
        }

        if let Some(entries) = self.extra.object_entries() {
            for entry in entries {
                let value = self
                    .extra
                    .subprojection_property_units(entry.code_units)
                    .map_err(serde::ser::Error::custom)?;
                let Ok(key) = String::from_utf16(entry.code_units) else {
                    fields.push((entry.code_units.to_vec(), value));
                    continue;
                };
                if key == crate::jsonl::exact_json::PRIVATE_UTF16_KEY
                    || key == crate::jsonl::exact_json::PRIVATE_SOURCE_KEY_ORDER_KEY
                    || RECOGNIZED_EXTRA.contains(&key.as_str())
                    || (is_user
                        && (USER_HEAD_EXTRA.contains(&key.as_str())
                            || key == "usageLimitNote"
                            || key == "refusedMessageId"))
                    || (is_assistant && is_api_error && key == "refusedMessageId")
                    || (is_scheduled_fire && SCHEDULED_FIRE_HEAD.contains(&key.as_str()))
                    || (is_model_fallback && MODEL_FALLBACK_HEAD.contains(&key.as_str()))
                    || (is_refusal_fallback && REFUSAL_FALLBACK_HEAD.contains(&key.as_str()))
                    || (is_compact_boundary && BOUNDARY_HEAD_EXTRA.contains(&key.as_str()))
                    || (is_attachment
                        && ["attachment", "rendered", "renderedRole"].contains(&key.as_str()))
                    || TOOL_RESULT_HEAD_EXTRA.contains(&key.as_str())
                    || key == "origin"
                    || (is_assistant && !is_api_error && key == "supersedesUuids")
                {
                    continue;
                }
                fields.push((entry.code_units.to_vec(), value));
            }
        }

        if let Some(projection) = &self.attachment_projection {
            if !is_attachment {
                push_projection(&mut fields, "attachment", projection.clone());
            }
        }
        if let Some(projection) = &self.turn_origin_projection {
            push_projection(&mut fields, "turnOrigin", projection.clone());
        }

        if let Some(source_order) = message_source_key_order(self) {
            fields.sort_by_cached_key(|(key, _)| {
                source_order
                    .iter()
                    .position(|source_key| source_key == key)
                    .unwrap_or(usize::MAX)
            });
        }

        let row = Utf16JsonProjection::object_units(fields).map_err(serde::ser::Error::custom)?;
        RawProjection(&row).serialize(serializer)
    }
}

fn push_projection(
    fields: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
    key: &str,
    value: Utf16JsonProjection,
) {
    fields.push((key.encode_utf16().collect(), value));
}

fn push_extra_projection(
    fields: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
    extra: &Utf16JsonProjection,
    key: &str,
) {
    if let Some(value) = extra_projection(extra, key) {
        push_projection(fields, key, value);
    }
}

fn extra_projection(extra: &Utf16JsonProjection, key: &str) -> Option<Utf16JsonProjection> {
    extra
        .subprojection_property_units(&key.encode_utf16().collect::<Vec<_>>())
        .ok()
}

fn string_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust strings are valid JSON strings")
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("boolean literal is valid JSON")
}

fn null_projection() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

fn option_string_projection(value: Option<&str>) -> Utf16JsonProjection {
    value.map(string_projection).unwrap_or_else(null_projection)
}

#[cfg(test)]
mod attachment_envelope_tests {
    use super::{JsonlMessage, Utf16JsonProjection};

    #[test]
    fn captured_queued_rendering_precedes_trailer_in_both_native_rows() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/headless-fixtures/native-2.1.293-queued-command-jsonl.json"
        ))
        .unwrap();
        let mut count = 0;
        for entry in fixture["rows"].as_array().unwrap() {
            let raw = entry["rawLine"].as_str().unwrap();
            if !raw.contains("\"type\":\"attachment\"") {
                continue;
            }
            let row: JsonlMessage = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&row).unwrap(), raw);
            count += 1;
        }
        assert_eq!(count, 2);
    }

    /// Real 2.1.220 `attachment` lines put the `attachment` payload BEFORE the
    /// `type` discriminator — outer key order
    /// `parentUuid, isSidechain, attachment, type, uuid, timestamp, userType,
    /// entrypoint, cwd, sessionId, version, gitBranch[, slug]`
    /// (26 048 attachment lines mined from `~/.claude/projects/**/*.jsonl`;
    /// the dominant envelope, 12 607 lines, is exactly this plus the unported
    /// `session_id` / `sessionKind` / `slug` siblings).
    #[test]
    fn attachment_line_emits_payload_before_type() {
        let msg = JsonlMessage {
            message_type: "attachment".to_string(),
            uuid: "84c51927-5550-4d0c-bdff-16c5699c7d4c".to_string(),
            parent_uuid: Some("9fccb389-ff4d-4502-b0c4-e9ecb4459013".to_string()),
            session_id: "931e2281-6acb-4c36-8352-a2378fd6c88d".to_string(),
            timestamp: "2026-07-27T18:34:21.476Z".to_string(),
            cwd: "/private/tmp".to_string(),
            version: "0.12.0".to_string(),
            message: Utf16JsonProjection::parse("null").unwrap(),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: Some("HEAD".to_string()),
            entrypoint: Some("cli".to_string()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                .expect("empty JSON object"),
            attachment_projection: Some(
                Utf16JsonProjection::parse(r#"{"type":"hook_success","hookName":"Stop"}"#).unwrap(),
            ),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
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
        let msg = JsonlMessage {
            message_type: "attachment".to_string(),
            uuid: "u".to_string(),
            parent_uuid: None,
            session_id: "s".to_string(),
            timestamp: "t".to_string(),
            cwd: "/c".to_string(),
            version: "v".to_string(),
            message: Utf16JsonProjection::parse("null").unwrap(),
            is_sidechain: false,
            user_type: None,
            git_branch: None,
            entrypoint: None,
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                .expect("empty JSON object"),
            attachment_projection: Some(Utf16JsonProjection::parse(r#"{"type":"x"}"#).unwrap()),
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
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
        let mut extra = Utf16JsonProjection::parse("{}").unwrap();
        extra
            .set_field(
                super::SESSION_KIND_KEY,
                Utf16JsonProjection::parse(r#""daemon-worker""#).unwrap(),
            )
            .unwrap();
        let msg = JsonlMessage {
            message_type: "user".to_string(),
            uuid: "u".to_string(),
            parent_uuid: None,
            session_id: "s".to_string(),
            timestamp: "t".to_string(),
            cwd: "/c".to_string(),
            version: "v".to_string(),
            message: super::test_projection(serde_json::json!({"role": "user", "content": "hi"})),
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: Some("main".to_string()),
            entrypoint: Some("cli".to_string()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,

            attachment_projection: None,
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
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
            message: super::test_projection(serde_json::json!({"role": "user"})),
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
    use super::{JsonlMessage, Utf16JsonProjection};
    use serde_json::json;

    fn base() -> JsonlMessage {
        JsonlMessage {
            parent_uuid: Some("p".into()),
            is_sidechain: false,
            message_type: "user".into(),
            message: super::test_projection(json!({"role":"user","content":[]})),
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

    #[test]
    fn denial_head_keys_precede_the_common_trailer() {
        let mut m = base();
        m.tool_use_result_projection = Some(
            Utf16JsonProjection::root_string(
                "Error: denied".into(),
                "Error: denied".encode_utf16().collect(),
            )
            .unwrap(),
        );
        m.extra
            .set_field(
                "toolDenialKind",
                Utf16JsonProjection::parse(r#""permission-rule""#).unwrap(),
            )
            .unwrap();
        m.extra
            .set_field(
                "sourceToolAssistantUUID",
                Utf16JsonProjection::parse(r#""a-uuid""#).unwrap(),
            )
            .unwrap();
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
            .set_field(
                "sourceToolAssistantUUID",
                Utf16JsonProjection::parse(r#""a-uuid""#).unwrap(),
            )
            .unwrap();
        m.extra
            .set_field(
                "toolDenialKind",
                Utf16JsonProjection::parse(r#""cancelled""#).unwrap(),
            )
            .unwrap();
        m.tool_use_result_projection =
            Some(Utf16JsonProjection::root_string("x".into(), vec![b'x' as u16]).unwrap());
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
            .set_field(
                "sourceToolAssistantUUID",
                Utf16JsonProjection::parse(r#""a-uuid""#).unwrap(),
            )
            .unwrap();
        m.extra
            .set_field("toolEndsTurn", Utf16JsonProjection::parse("true").unwrap())
            .unwrap();
        m.mcp_meta_projection = Some(super::test_projection(
            json!({"_meta":{"claude/endTurn":true}}),
        ));
        m.extra
            .set_field(
                "toolDenialKind",
                Utf16JsonProjection::parse(r#""permission-rule""#).unwrap(),
            )
            .unwrap();
        m.tool_use_result_projection =
            Some(Utf16JsonProjection::root_string("x".into(), vec![b'x' as u16]).unwrap());
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
            .set_field(
                "sourceToolAssistantUUID",
                Utf16JsonProjection::parse(r#""a-uuid""#).unwrap(),
            )
            .unwrap();
        m.extra
            .set_field(
                "permissionMode",
                Utf16JsonProjection::parse(r#""plan""#).unwrap(),
            )
            .unwrap();
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

    #[test]
    fn new_rows_without_source_order_keep_the_canonical_native_order() {
        let message = base();
        assert!(!message
            .extra
            .contains_property(super::PRIVATE_SOURCE_KEY_ORDER_KEY));
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"parentUuid":"p","isSidechain":false,"type":"user","message":{"role":"user","content":[]},"uuid":"u","timestamp":"T","userType":"external","entrypoint":"cli","cwd":"/w","sessionId":"s","version":"0.12.0"}"#
        );
    }

    #[test]
    fn recovered_source_order_survives_modify_delete_and_new_fields() {
        let source = Utf16JsonProjection::parse(
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":[]},"uuid":"u","timestamp":"T","userType":"external","cwd":"/old","sessionId":"s","version":"v","opaque":{"items":[1,2]},"origin":{"kind":"peer"}}"#,
        )
        .unwrap();
        let mut message = JsonlMessage::from_projection(&source).unwrap();
        message.cwd = "/updated".into();
        message.user_type = None;
        message.extra.remove_field("opaque").unwrap();
        message.entrypoint = Some("cli".into());

        let serialized =
            Utf16JsonProjection::parse(&serde_json::to_string(&message).unwrap()).unwrap();
        let keys = serialized
            .object_entries()
            .unwrap()
            .iter()
            .map(|entry| entry.display_key.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                "parentUuid",
                "isSidechain",
                "type",
                "message",
                "uuid",
                "timestamp",
                "cwd",
                "sessionId",
                "version",
                "origin",
                "entrypoint",
            ]
            .map(str::to_owned)
        );
        assert_eq!(serialized.string_at("/cwd"), Some("/updated"));
        assert_eq!(
            serialized
                .subprojection("/message")
                .unwrap()
                .to_json_string()
                .unwrap(),
            source
                .subprojection("/message")
                .unwrap()
                .to_json_string()
                .unwrap()
        );
        assert_eq!(
            serialized
                .subprojection("/origin")
                .unwrap()
                .to_json_string()
                .unwrap(),
            source
                .subprojection("/origin")
                .unwrap()
                .to_json_string()
                .unwrap()
        );
        assert!(!serialized.contains_property(super::PRIVATE_SOURCE_KEY_ORDER_KEY));
    }

    #[test]
    fn recovered_order_keeps_lone_utf16_extra_key_and_ignores_forged_metadata() {
        let source = Utf16JsonProjection::parse(
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":[]},"\ud800":"opaque","uuid":"u","timestamp":"T","userType":"external","cwd":"/c","sessionId":"s","version":"v","origin":{"kind":"peer"},"lingxi_jsonl_source_key_order":["origin","version"]}"#,
        )
        .unwrap();
        let message = JsonlMessage::from_projection(&source).unwrap();
        let json = serde_json::to_string(&message).unwrap();
        let serialized = Utf16JsonProjection::parse(&json).unwrap();
        let keys = serialized
            .object_entries()
            .unwrap()
            .iter()
            .map(|entry| entry.code_units.to_vec())
            .collect::<Vec<_>>();
        let utf16 = |key: &str| key.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                utf16("parentUuid"),
                utf16("isSidechain"),
                utf16("type"),
                utf16("message"),
                vec![0xd800],
                utf16("uuid"),
                utf16("timestamp"),
                utf16("userType"),
                utf16("cwd"),
                utf16("sessionId"),
                utf16("version"),
                utf16("origin"),
            ]
        );
        assert!(json.contains(r#""\ud800":"opaque""#));
        assert!(!json.contains(super::PRIVATE_SOURCE_KEY_ORDER_KEY));
        assert!(!serialized.contains_property(super::PRIVATE_SOURCE_KEY_ORDER_KEY));
        assert_eq!(
            serialized
                .subprojection("/message")
                .unwrap()
                .to_json_string()
                .unwrap(),
            source
                .subprojection("/message")
                .unwrap()
                .to_json_string()
                .unwrap()
        );
        assert_eq!(
            serialized
                .subprojection("/origin")
                .unwrap()
                .to_json_string()
                .unwrap(),
            source
                .subprojection("/origin")
                .unwrap()
                .to_json_string()
                .unwrap()
        );
    }
}

#[cfg(test)]
mod supersedes_head_tests {
    use super::{JsonlMessage, Utf16JsonProjection};
    use serde_json::json;

    #[test]
    fn server_stitch_supersedes_ids_precede_the_common_trailer() {
        let extra =
            Utf16JsonProjection::parse(r#"{"supersedesUuids":["old-row-1","old-row-2"]}"#).unwrap();
        let message = JsonlMessage {
            message_type: "assistant".into(),
            uuid: "new-row".into(),
            parent_uuid: Some("parent".into()),
            session_id: "session".into(),
            timestamp: "2026-10-03T12:00:00.000Z".into(),
            cwd: "/workspace".into(),
            version: "0.12.0".into(),
            message: super::test_projection(
                json!({"id":"provider-message","role":"assistant","content":[]}),
            ),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: Some("cli".into()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,

            attachment_projection: None,
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
        };

        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"parentUuid":"parent","isSidechain":false,"message":{"id":"provider-message","role":"assistant","content":[]},"type":"assistant","uuid":"new-row","timestamp":"2026-10-03T12:00:00.000Z","supersedesUuids":["old-row-1","old-row-2"],"userType":"external","entrypoint":"cli","cwd":"/workspace","sessionId":"session","version":"0.12.0"}"#
        );
    }
}

#[cfg(test)]
#[test]
fn scheduled_fire_matches_2_1_270_oracle_bytes() {
    for expected in include_str!("../../tests/fixtures/loop-2.1.270/scheduled_fire.jsonl").lines() {
        let row = crate::jsonl::reader::route_lines(expected)
            .messages_in_order
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(serde_json::to_string(&row).unwrap(), expected);
    }
}

#[cfg(test)]
#[test]
fn loop_turn_companion_matches_2_1_270_oracle_bytes() {
    let expected = include_str!("../../tests/fixtures/loop-2.1.270/turn_companion.jsonl").trim();
    let row = crate::jsonl::reader::route_lines(expected)
        .messages_in_order
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(serde_json::to_string(&row).unwrap(), expected);
}

#[cfg(test)]
mod shutdown_interruption_tests {
    use super::{JsonlMessage, Utf16JsonProjection};

    fn user_row(extra: Utf16JsonProjection) -> JsonlMessage {
        JsonlMessage {
            message_type: "user".into(),
            uuid: "u".into(),
            parent_uuid: None,
            session_id: "s".into(),
            timestamp: "t".into(),
            cwd: "/c".into(),
            version: "v".into(),
            message: Utf16JsonProjection::parse(r#"{"role":"user","content":"A\ud800B"}"#).unwrap(),
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch: None,
            entrypoint: Some("cli".into()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
            attachment_projection: None,
            tool_use_result_projection: None,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
        }
    }

    #[test]
    fn interruption_flag_uses_native_slot_and_preserves_utf16_payload() {
        let extra = Utf16JsonProjection::parse(
            r#"{"promptSource":"cli","interruptedMessageId":"message-id","interruptedByShutdown":true}"#,
        )
        .unwrap();
        let row = user_row(extra);
        let line = serde_json::to_string(&row).unwrap();
        let timestamp = line.find("\"timestamp\":\"t\"").unwrap();
        let prompt_source = line.find("\"promptSource\":\"cli\"").unwrap();
        let interrupted_id = line
            .find("\"interruptedMessageId\":\"message-id\"")
            .unwrap();
        let shutdown = line.find("\"interruptedByShutdown\":true").unwrap();
        let trailer = line.find("\"userType\":\"external\"").unwrap();
        assert!(timestamp < prompt_source);
        assert!(prompt_source < interrupted_id);
        assert!(interrupted_id < shutdown);
        assert!(shutdown < trailer);
        assert!(
            line.contains(r#"\ud800"#),
            "UTF-16 carrier remains exact: {line}"
        );

        let cold: JsonlMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            cold.extra
                .at_pointer("/interruptedByShutdown")
                .and_then(lingxi_core::types::utf16_json::JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            cold.message.string_units("/content"),
            Some(vec![65, 0xd800, 66])
        );
    }

    #[test]
    fn interruption_flag_omits_absence_and_retains_explicit_false() {
        let absent =
            serde_json::to_string(&user_row(Utf16JsonProjection::parse("{}").unwrap())).unwrap();
        assert!(!absent.contains("interruptedByShutdown"));

        let false_extra = Utf16JsonProjection::parse(r#"{"interruptedByShutdown":false}"#).unwrap();
        let explicit_false = serde_json::to_string(&user_row(false_extra)).unwrap();
        assert!(explicit_false.contains("\"interruptedByShutdown\":false"));
    }

    #[test]
    fn refusal_message_id_uses_native_user_slot_and_preserves_utf16_payload() {
        let extra = Utf16JsonProjection::parse(r#"{"refusedMessageId":"A\ud800B"}"#).unwrap();
        let row = user_row(extra);
        let line = serde_json::to_string(&row).unwrap();
        let timestamp = line.find("\"timestamp\":\"t\"").unwrap();
        let refused = line.find("\"refusedMessageId\":\"A\\ud800B\"").unwrap();
        let trailer = line.find("\"userType\":\"external\"").unwrap();
        assert!(
            timestamp < refused && refused < trailer,
            "Native slot: {line}"
        );

        let cold: JsonlMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            cold.extra.string_units("/refusedMessageId"),
            Some(vec![65, 0xd800, 66])
        );
        let absent =
            serde_json::to_string(&user_row(Utf16JsonProjection::parse("{}").unwrap())).unwrap();
        assert!(!absent.contains("refusedMessageId"));
    }

    #[test]
    fn refusal_message_id_on_assistant_api_error_roundtrips_in_its_native_head() {
        let mut row = user_row(
            Utf16JsonProjection::parse(
                r#"{"isApiErrorMessage":true,"apiErrorStatus":422,"refusedMessageId":"A\ud800B"}"#,
            )
            .unwrap(),
        );
        row.message_type = "assistant".into();
        row.message = Utf16JsonProjection::parse(r#"{"role":"assistant","content":[]}"#).unwrap();
        let line = serde_json::to_string(&row).unwrap();
        let timestamp = line.find("\"timestamp\":\"t\"").unwrap();
        let status = line.find("\"apiErrorStatus\":422").unwrap();
        let refused = line.find("\"refusedMessageId\":\"A\\ud800B\"").unwrap();
        let trailer = line.find("\"userType\":\"external\"").unwrap();
        assert!(
            timestamp < status && status < refused && refused < trailer,
            "{line}"
        );

        let cold: JsonlMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(
            cold.extra.string_units("/refusedMessageId"),
            Some(vec![65, 0xd800, 66])
        );
    }
}
