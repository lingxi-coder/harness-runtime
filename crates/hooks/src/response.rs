//! Hook response + result types (spec §9.4).
//!
//! A hook executor returns a [`HookResult`] capturing the raw process /
//! transport outcome plus the JSON [`HookResponse`] parsed from the hook's
//! reply. The executor merges all matching hooks' responses into a single
//! [`AggregateHookResult`] consumed by the calling subsystem.

use crate::events::HookProgressEvent;
use lingxi_core::types::HookId;
use serde::{Deserialize, Serialize};

/// Exact arbitrary JSON value retained by hook responses and aggregates.
pub type HookJson = lingxi_core::types::utf16_json::Utf16JsonProjection;

mod raw_hook_json_array {
    use super::HookJson;
    use serde::de::Error as DeError;
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::value::RawValue;

    pub fn serialize<S>(values: &Vec<HookJson>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(values.len()))?;
        for value in values {
            let source = value.to_json_string().map_err(serde::ser::Error::custom)?;
            let raw = RawValue::from_string(source).map_err(serde::ser::Error::custom)?;
            sequence.serialize_element(&raw)?;
        }
        sequence.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<HookJson>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let projection = HookJson::parse(raw.get()).map_err(D::Error::custom)?;
        let length = projection
            .array_len("")
            .ok_or_else(|| D::Error::custom("expected JSON array"))?;
        (0..length)
            .map(|index| {
                projection
                    .subprojection(&format!("/{index}"))
                    .map_err(D::Error::custom)
            })
            .collect()
    }
}

mod raw_exact_hook_text {
    use super::{ExactHookText, HookJson};
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::value::RawValue;

    pub fn serialize<S>(value: &ExactHookText, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        lingxi_core::types::utf16_json::raw_json_serde::serialize(
            &value.json_projection(),
            serializer,
        )
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ExactHookText, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        exact_text::<D::Error>(raw.get())
    }

    pub fn serialize_option<S>(
        value: &Option<ExactHookText>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serialize(value, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize_option<'de, D>(deserializer: D) -> Result<Option<ExactHookText>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let projection = HookJson::parse(raw.get()).map_err(D::Error::custom)?;
        if projection
            .at_pointer("")
            .is_some_and(|value| value.is_null())
        {
            return Ok(None);
        }
        exact_text_from_projection::<D::Error>(projection).map(Some)
    }

    fn exact_text<E>(raw: &str) -> Result<ExactHookText, E>
    where
        E: serde::de::Error,
    {
        let projection = HookJson::parse(raw).map_err(E::custom)?;
        exact_text_from_projection::<E>(projection)
    }

    fn exact_text_from_projection<E>(projection: HookJson) -> Result<ExactHookText, E>
    where
        E: serde::de::Error,
    {
        let display = projection
            .string_at("")
            .ok_or_else(|| E::custom("expected JSON string"))?
            .to_owned();
        let utf16_code_units = projection
            .string_units("")
            .ok_or_else(|| E::custom("expected JSON string"))?;
        Ok(ExactHookText {
            display,
            utf16_code_units,
        })
    }
}

mod raw_exact_hook_text_array {
    use super::{ExactHookText, HookJson};
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::value::RawValue;

    pub fn serialize<S>(values: &Vec<ExactHookText>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let projection =
            HookJson::array(values.iter().map(ExactHookText::json_projection).collect())
                .map_err(serde::ser::Error::custom)?;
        lingxi_core::types::utf16_json::raw_json_serde::serialize(&projection, serializer)
    }

    pub fn serialize_option<S>(
        values: &Option<Vec<ExactHookText>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match values {
            Some(values) => serialize(values, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize_option<'de, D>(
        deserializer: D,
    ) -> Result<Option<Vec<ExactHookText>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        parse::<D::Error>(raw.get()).map(Some)
    }

    fn parse<E>(source: &str) -> Result<Vec<ExactHookText>, E>
    where
        E: serde::de::Error,
    {
        let projection = HookJson::parse(source).map_err(E::custom)?;
        let length = projection
            .array_len("")
            .ok_or_else(|| E::custom("expected JSON array"))?;
        (0..length)
            .map(|index| {
                ExactHookText::from_json_projection(&projection, &format!("/{index}"))
                    .ok_or_else(|| E::custom("expected string array item"))
            })
            .collect()
    }
}

/// A hook string whose display text and exact JavaScript UTF-16 code units are
/// retained together. `display` is safe Rust UTF-8 (lone surrogates display as
/// U+FFFD); `utf16_code_units` is authoritative for JS-compatible length,
/// slicing, and provider/transcript serialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ExactHookText {
    /// Valid UTF-8 view used by logs and UI.
    pub display: String,
    /// Exact JS string contents in UTF-16 code units.
    pub utf16_code_units: Vec<u16>,
}

impl ExactHookText {
    /// Construct from an ordinary Rust string.
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        let display = text.into();
        let utf16_code_units = display.encode_utf16().collect();
        Self {
            display,
            utf16_code_units,
        }
    }

    /// Construct from exact JavaScript UTF-16 units, retaining a replacement-
    /// character display for isolated surrogates.
    #[must_use]
    pub fn from_utf16(utf16_code_units: Vec<u16>) -> Self {
        let display = String::from_utf16_lossy(&utf16_code_units);
        Self {
            display,
            utf16_code_units,
        }
    }

    /// Convert to the source carrier without deriving units from display text.
    #[must_use]
    pub fn into_projection(self) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            self.display,
            self.utf16_code_units,
        )
        .expect("ExactHookText display is derived from its UTF-16 units")
    }

    /// Read a string leaf from a JSON projection, including escaped isolated
    /// surrogates in the source JSON.
    #[must_use]
    pub fn from_json_projection(
        projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        pointer: &str,
    ) -> Option<Self> {
        let display = projection.string_at(pointer)?.to_owned();
        let utf16_code_units = projection.string_units(pointer)?;
        Some(Self {
            display,
            utf16_code_units,
        })
    }

    /// JavaScript `String.length` for this value.
    #[must_use]
    pub fn len_utf16(&self) -> usize {
        self.utf16_code_units.len()
    }

    /// Whether this JS string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.utf16_code_units.is_empty()
    }

    /// ECMAScript `String.prototype.trim()` over the JS whitespace and line
    /// terminator set, including BOM/U+FEFF (Rust `str::trim` differs there).
    #[must_use]
    pub fn trim_js(&self) -> Self {
        let start = self
            .utf16_code_units
            .iter()
            .position(|unit| !is_ecmascript_trim_unit(*unit))
            .unwrap_or(self.utf16_code_units.len());
        let end = self
            .utf16_code_units
            .iter()
            .rposition(|unit| !is_ecmascript_trim_unit(*unit))
            .map_or(start, |index| index + 1);
        Self::from_utf16(self.utf16_code_units[start..end].to_vec())
    }

    /// Join exact strings with a plain separator.
    #[must_use]
    pub fn join(values: &[Self], separator: &str) -> Self {
        let mut result = Self::default();
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                result.push_text(separator);
            }
            result.push(value);
        }
        result
    }

    /// Build a plain prefix + exact body + plain suffix string.
    #[must_use]
    pub fn wrapped(prefix: &str, body: &Self, suffix: &str) -> Self {
        let mut result = Self::from_text(prefix);
        result.push(body);
        result.push_text(suffix);
        result
    }

    /// Append another exact string without normalizing its code units.
    pub fn push(&mut self, other: &Self) {
        self.display.push_str(&other.display);
        self.utf16_code_units
            .extend_from_slice(&other.utf16_code_units);
    }

    /// Append ordinary text.
    pub fn push_text(&mut self, text: &str) {
        self.display.push_str(text);
        self.utf16_code_units.extend(text.encode_utf16());
    }

    /// Native `re(value, cap)`: slice by JS code units and remove a trailing
    /// high surrogate so the result is well-formed at a truncation boundary.
    #[must_use]
    pub fn truncate_well_formed(&self, cap: usize) -> Self {
        if self.len_utf16() <= cap {
            return self.clone();
        }
        let mut end = cap;
        if end > 0
            && self
                .utf16_code_units
                .get(end - 1)
                .is_some_and(|unit| (0xD800..=0xDBFF).contains(unit))
        {
            end -= 1;
        }
        Self::from_utf16(self.utf16_code_units[..end].to_vec())
    }

    /// Native `ZJe(value, cap)`: use a JS-unit prefix, preferring the last
    /// newline only when it lies past the midpoint. The returned prefix may
    /// end in a high surrogate because native `slice` preserves that case.
    #[must_use]
    pub fn preview(&self, cap: usize) -> (Self, bool) {
        if self.len_utf16() <= cap {
            return (self.clone(), false);
        }
        let limit = cap.min(self.len_utf16());
        let newline = self.utf16_code_units[..limit]
            .iter()
            .rposition(|unit| *unit == 0x000A);
        let end = newline.filter(|index| *index > cap / 2).unwrap_or(limit);
        (
            Self::from_utf16(self.utf16_code_units[..end].to_vec()),
            true,
        )
    }

    /// Match `TextEncoder.encodeInto`'s scalar-safe UTF-8 byte cap. Lone
    /// surrogates are encoded as U+FFFD (three bytes), as in the browser API.
    #[must_use]
    pub fn truncate_utf8_bytes(&self, cap: usize) -> Self {
        let units = &self.utf16_code_units;
        let mut index = 0;
        let mut written: usize = 0;
        while index < units.len() {
            let first = units[index];
            let (unit_count, byte_count) = if (0xD800..=0xDBFF).contains(&first)
                && units
                    .get(index + 1)
                    .is_some_and(|second| (0xDC00..=0xDFFF).contains(second))
            {
                (2, 4)
            } else if (0xD800..=0xDFFF).contains(&first) {
                (1, 3)
            } else if first <= 0x7F {
                (1, 1)
            } else if first <= 0x7FF {
                (1, 2)
            } else {
                (1, 3)
            };
            if written.saturating_add(byte_count) > cap {
                break;
            }
            written += byte_count;
            index += unit_count;
        }
        if index == units.len() {
            self.clone()
        } else {
            Self::from_utf16(units[..index].to_vec())
        }
    }

    /// Number of bytes `TextEncoder` writes for this JS string.
    #[must_use]
    pub fn encoded_utf8_len(&self) -> usize {
        self.truncate_utf8_bytes(usize::MAX).display.len()
    }

    /// Convert to a typed root-string JSON projection without losing isolated
    /// surrogate units.
    #[must_use]
    pub fn json_projection(&self) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        HookJson::root_string(self.display.clone(), self.utf16_code_units.clone())
            .expect("ExactHookText maintains matching display and UTF-16 units")
    }

    /// Construct a conversation user message while retaining isolated
    /// JavaScript UTF-16 code units for provider serialization.
    #[must_use]
    pub fn to_conversation_message(
        &self,
        id: lingxi_core::types::MessageId,
        is_meta: bool,
    ) -> lingxi_core::types::ConversationMessage {
        let display_units: Vec<_> = self.display.encode_utf16().collect();
        if display_units == self.utf16_code_units {
            return if is_meta {
                lingxi_core::types::ConversationMessage::user_meta(id, self.display.clone())
            } else {
                lingxi_core::types::ConversationMessage::user(id, self.display.clone())
            };
        }
        lingxi_core::types::ConversationMessage::User {
            id,
            content: vec![lingxi_core::types::ContentBlock::TextJsUtf16 {
                text: self.display.clone(),
                utf16_code_units: self.utf16_code_units.clone(),
                citations: None,
            }],
            is_meta,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
        }
    }
}

impl From<String> for ExactHookText {
    fn from(value: String) -> Self {
        Self::from_text(value)
    }
}

impl From<&str> for ExactHookText {
    fn from(value: &str) -> Self {
        Self::from_text(value)
    }
}

fn is_ecmascript_trim_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000
            ..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

impl serde::Serialize for ExactHookText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.utf16_code_units != self.display.encode_utf16().collect::<Vec<_>>() {
            return Err(serde::ser::Error::custom(
                "exact hook text with isolated UTF-16 units requires Utf16JsonProjection",
            ));
        }
        serializer.serialize_str(&self.display)
    }
}

impl<'de> serde::Deserialize<'de> for ExactHookText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_text)
    }
}

/// SH-01 — `Pfr = 2000` (oracle 2.1.238 @ 292378095): the per-hook cap, in
/// UTF-16 code units, applied to `hookSpecificOutput.classifierContext`.
///
/// The schema's describe string calls it "a budget shared across all hooks that
/// contribute to one call"; the aggregation loop itself (@ 296974134) applies
/// `wo(z.classifierContext, Pfr)` per hook and ACCUMULATES the post-cap lengths
/// into `classifierContextChars`, which is the counter that tracks the shared
/// budget. Both halves are ported: the per-hook cap here, the running total on
/// [`AggregateHookResult::classifier_context_chars`].
pub const CLASSIFIER_CONTEXT_CAP_UTF16: usize = 2000;

/// SH-01 — `wo(e, t)` (oracle 2.1.238 @ 281366731):
///
/// ```js
/// function wo(e,t){ if(t<=0)return"";
///   if(e.length<=t)return e;
///   let r=e.slice(0,t), n=r.charCodeAt(t-1);
///   return cTu(n>=55296&&n<=56319 ? r.slice(0,-1) : r) }
/// ```
///
/// `e.length` is UTF-16 code units, not chars and not bytes — a cap counted in
/// Rust `char`s or bytes would disagree with the oracle for any non-BMP or
/// non-ASCII content. The high-surrogate check (`0xD800..=0xDBFF`) drops a
/// trailing lone surrogate so the cut never splits an astral character. `cTu`
/// is a V8 sliced-string detach, semantically the identity.
#[must_use]
pub fn truncate_utf16(value: &str, cap: usize) -> String {
    if cap == 0 {
        return String::new();
    }
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= cap {
        return value.to_string();
    }
    let mut kept = &units[..cap];
    if kept.last().is_some_and(|u| (0xD800..=0xDBFF).contains(u)) {
        kept = &kept[..kept.len() - 1];
    }
    String::from_utf16_lossy(kept)
}

/// SH-01 — one host-asserted context line a `PostToolUse` hook attached to a
/// completed tool call's result (oracle 2.1.238 @ 296974134,
/// `classifierContexts:[{value:U,hostPrincipal:…}]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierHostContext {
    /// The post-cap text (see [`CLASSIFIER_CONTEXT_CAP_UTF16`]).
    #[serde(with = "raw_exact_hook_text")]
    pub value: ExactHookText,
    /// `hostPrincipal` — upstream computes it as
    /// `z.hook.type==="callback" && pluginId===undefined && pluginRoot===undefined
    ///  && skillRoot===undefined`, i.e. TRUE only for an in-process host callback
    /// that belongs to no plugin and no skill. The port has no `callback` hook
    /// type at all (`loader.rs` builds `command`/`http`/`agent`/`prompt`/
    /// `mcp_tool` only), so every context the port can produce today is
    /// `false` — which is what the binary would also compute for those types.
    pub host_principal: bool,
}

/// SH-01 — `pairedRewrite` (oracle 2.1.238 @ 296974134): which output-rewrite,
/// if any, the SAME hook performed in the same result as its
/// `classifierContext`.
///
/// ```js
/// pairedRewrite: z.updatedToolOutput!==void 0 ? "direct"
///              : z.updatedMCPToolOutput!==void 0 ? "legacy_mcp"
///              : z.legacyMcpRewriteSuppressed ? "suppressed" : "none"
/// ```
/// The classifier needs it to know whether the context describes the output the
/// model will actually see or the one the hook replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairedRewrite {
    /// The same hook set `updatedToolOutput`.
    Direct,
    /// The same hook set the legacy `updatedMCPToolOutput`.
    LegacyMcp,
    /// The legacy MCP rewrite was suppressed. The port has no
    /// `legacyMcpRewriteSuppressed` signal, so this variant is never produced
    /// today — it exists so the wire vocabulary is complete.
    Suppressed,
    /// The hook rewrote nothing.
    None,
}

impl PairedRewrite {
    /// The wire spelling upstream yields.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::LegacyMcp => "legacy_mcp",
            Self::Suppressed => "suppressed",
            Self::None => "none",
        }
    }
}

/// Structured response a hook may return to influence the in-flight action.
///
/// All fields are optional. A hook may simply observe (returning the default
/// response) or actively intervene by setting `decision`, `updated_input`,
/// `system_message`, or `attachments`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookResponse {
    /// Internal prompt-evaluator result (`ppr.impossible`), never supplied by
    /// command/HTTP hook JSON. A matching session goal terminates as failed.
    #[serde(skip)]
    pub impossible: bool,
    /// Engine action the hook recommends (block / allow / approve / continue).
    pub decision: Option<HookDecision>,
    /// Exact reason text for the decision (rendered to the user on Block).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub reason: Option<ExactHookText>,
    /// O2: the `command` half of this hook's `blockingError` object, when the
    /// EXECUTOR synthesized the block rather than the hook's JSON declaring it.
    ///
    /// The plain-text **exit-2** arm renders `command` as `iSe(hook)` — the raw
    /// per-arm text that ignores `statusMessage` — whereas the JSON
    /// `decision:"block"` arm renders it as `qq(hook)`, which prefers
    /// `statusMessage` (runner locals `te=iSe(q)` / `ee=qq(q)`, BIN off
    /// ~237802650). The two differ ONLY for a hook that sets `statusMessage`,
    /// which is exactly why the arm has to record its own choice here instead
    /// of letting the merge step re-derive one.
    ///
    /// `None` — the common case — means "no arm-specific override", and the
    /// merge step falls back to `qq(hook)`.
    ///
    /// NOT part of the hook's JSON wire contract: this is an internal channel
    /// the executor fills in, so it is `#[serde(skip)]`.
    #[serde(skip)]
    pub block_command: Option<ExactHookText>,
    /// Replacement input for the in-flight action (e.g. mutated tool input).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub updated_input: Option<HookJson>,
    /// Raw `updatedPermissions` carried by a `PermissionRequest` allow.
    ///
    /// Permission updates intentionally remain untyped here. The permission
    /// crate owns their schema and applies the same raw wire values that the
    /// host supplied, including destination and rule metadata.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub updated_permissions: Option<HookJson>,
    /// `interrupt` carried by a `PermissionRequest` deny. This is kept
    /// separate from the generic hook `prevent_continuation` signal: the
    /// former aborts the active turn, while the latter ends a hook loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<bool>,
    /// The event-specific `PermissionRequest` decision, retained alongside
    /// the normalized [`Self::decision`] for callers that need the raw
    /// allow/deny payload fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_request_result: Option<PermissionRequestResult>,
    /// Free-form `systemMessage` the hook returned (claude-code
    /// `result.systemMessage`). This is **user/transcript-facing only** — it is
    /// NOT sent to the model: claude-code routes it to a `hook_system_message`
    /// attachment whose `normalizeAttachmentForAPI` returns `[]`
    /// (`utils/messages.ts:4258`). Kept DISTINCT from [`Self::additional_context`]
    /// (which IS model-facing); the two must never be merged.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub system_message: Option<ExactHookText>,
    /// `hookSpecificOutput.additionalContext` the hook returned (claude-code
    /// `result.additionalContext`). This IS model-facing: claude-code yields it
    /// as a `hook_additional_context` attachment whose `normalizeAttachmentForAPI`
    /// returns a `<system-reminder>` user message that reaches the model
    /// (`utils/messages.ts:4117`). DISTINCT from [`Self::system_message`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub additional_context: Option<ExactHookText>,
    /// Additional content blocks (images, files, etc.) to attach.
    #[serde(default, with = "raw_hook_json_array")]
    pub attachments: Vec<HookJson>,
    /// If `true` the engine should suppress the default user-visible output
    /// for the in-flight action even when not blocked.
    #[serde(default)]
    pub suppress_output: bool,
    /// `true` when the hook returned `continue: false` (claude-code
    /// `hooks.ts:404`). For lifecycle hooks (`Stop` / `SubagentStop` /
    /// `TaskCompleted`) this is the *preventContinuation* signal: the agent
    /// loop must terminate rather than keep working, regardless of any
    /// `decision: block` also present (B4 — `query.ts:1278`). Distinct from a
    /// bare `Block` decision (exit-2 / `decision:block` without `continue:false`),
    /// which for a Stop hook means "keep working" (`query.ts:1282`). Additive /
    /// `..Default::default()`-compatible; defaults to `false` (the prior
    /// behavior where `continue:false` only copied `stopReason` into `reason`).
    #[serde(default)]
    pub prevent_continuation: bool,
    /// Caller-defined structured payload — for hooks that need to return
    /// metadata not covered by the canonical fields above.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub structured_content: Option<HookJson>,
    /// Replacement tool output a `PostToolUse` hook returned via
    /// `hookSpecificOutput.updatedMCPToolOutput` (claude-code
    /// `parseHookJSONOutput`, `utils/hooks.ts:646-649`). `Some` only when a
    /// `PostToolUse` hook supplied a replacement output. The orchestrator
    /// substitutes it for the tool's result — but ONLY for MCP tools, mirroring
    /// TS's `isMcpTool(tool)` gate (`toolHooks.ts:146` / `toolExecution.ts:1494`).
    /// Additive default `None`, so non-`PostToolUse` hooks (and `PostToolUse`
    /// hooks that don't set it) leave the result untouched.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub updated_mcp_tool_output: Option<HookJson>,
    /// Replacement tool output a `PostToolUse` hook returned via
    /// `hookSpecificOutput.updatedToolOutput` (claude-code `parseHookJSONOutput`,
    /// BIN off 205724076: `if(e.hookSpecificOutput.updatedToolOutput!==void 0)
    /// u.updatedToolOutput=e.hookSpecificOutput.updatedToolOutput`). Unlike the
    /// legacy [`Self::updated_mcp_tool_output`] (truthiness-gated, MCP-only),
    /// this field uses `!== void 0` semantics — an explicit JSON `null` IS a
    /// replacement — and the orchestrator substitutes it for the result of ALL
    /// tools with NO `isMcpTool` gate (BIN off 202169384:
    /// `if("updatedToolOutput"in D&&e.outputSchema?.safeParse(D.updatedToolOutput)
    /// ?.success!==!1)x.data=D.updatedToolOutput`). The schema describe string is
    /// `Replaces the tool output before it is sent to the model`. To preserve the
    /// `!== void 0` semantics this is `Option<HookJson>`: `None` = key absent;
    /// a present projection whose root is JSON `null` is an explicit replacement.
    /// Additive default `None`, so the common case (no `PostToolUse`
    /// hook, or a hook that omits the field) leaves the result untouched.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub updated_tool_output: Option<HookJson>,
    /// SH-01 — `hookSpecificOutput.classifierContext` a `PostToolUse` hook
    /// returned (NEW in 2.1.238; the literal counts 0 in 2.1.220). Oracle
    /// @ 296466460:
    ///
    /// > "Host-asserted context shown to the auto-mode permission classifier
    /// > alongside this tool call's result. … Capped at 2000 UTF-16 code units,
    /// > a budget shared across all hooks that contribute to one call …
    /// > Honored on synchronous hook responses only …"
    ///
    /// Stored RAW here; the cap is applied at fold time (the aggregation loop is
    /// where upstream calls `wo(z.classifierContext, Pfr)`), so the cap and its
    /// running character budget stay in one place.
    ///
    /// "Honored on synchronous hook responses only" — a hook that backgrounds
    /// itself (`{"async":true}`) contributes no synchronous response, so its
    /// value never reaches the fold.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub classifier_context: Option<ExactHookText>,
    /// Elicitation answer a hook provided via
    /// `hookSpecificOutput.{action,content}` (claude-code
    /// `parseElicitationHookOutput`, `utils/hooks.ts:4434-4446` /
    /// `hooks.ts:674-688`). `Some` only for `Elicitation` / `ElicitationResult`
    /// hooks that returned an `action`. The MCP elicitation handler consumes
    /// this to PROVIDE the elicitation response; a `decline` action also
    /// drives a `Block` decision. Additive default `None`.
    pub elicitation_response: Option<ElicitationHookResponse>,
    /// `hookSpecificOutput.retry` a `PermissionDenied` hook returned (claude-code
    /// `parseHookJSONOutput`, `case 'PermissionDenied': result.retry =
    /// json.hookSpecificOutput.retry`, `utils/hooks.ts:654-655`). `Some(true)`
    /// signals the auto-mode classifier deny is now approved and the model may
    /// retry — the turn loop then pushes the verbatim `isMeta` retry message
    /// (`toolExecution.ts:1092-1099`). `None` for every other hook (and every
    /// `PermissionDenied` hook that omits it). DORMANT in the public build: the
    /// retry message is gated behind the `TRANSCRIPT_CLASSIFIER` feature
    /// (off externally) AND a classifier-source deny, so a hook setting this
    /// has no effect unless both hold — faithful to claude-code.
    #[serde(default)]
    pub retry: Option<bool>,
    /// Top-level `terminalSequence` a hook returned (#40, claude-code schema
    /// BIN off 200873127). A hook may ask LingXi to emit a terminal escape
    /// sequence (e.g. an OSC 9 / OSC 777 desktop notification). This is a
    /// TOP-LEVEL field (NOT under `hookSpecificOutput`) and applies for ALL hook
    /// result types (command/stdout, HTTP, mcp_tool, callback). The apply path
    /// (`szn`, BIN off 205755390) runs the [`crate::terminal_seq`] allowlist
    /// validator (`NEo`) — accepting only OSC ps in {0,1,2,9,99,777} plus BEL —
    /// and either writes the validated sequence to the active terminal or warns
    /// and drops it. Additive default `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub terminal_sequence: Option<ExactHookText>,
    /// `hookSpecificOutput.sessionTitle` returned by a `UserPromptSubmit` hook
    /// (binary-confirmed at BIN off 201754804:
    /// `{hookEventName:"UserPromptSubmit", additionalContext?:string,
    /// sessionTitle?:string, suppressOriginalPrompt?:boolean}`).
    /// Allows a hook to rename the session. Additive default `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub session_title: Option<ExactHookText>,
    /// `hookSpecificOutput.suppressOriginalPrompt` returned by a `UserPromptSubmit`
    /// hook (binary-confirmed at BIN off 201754804; description: "When decision is
    /// 'block', omit the original prompt from the block message"). Additive default
    /// `false`. Applied by the orchestrator's `fire_user_prompt_submit` block-message
    /// render (P2-04): on a `Block`, the warning collapses to bare `${reason}` when
    /// this is `true` instead of appending `\n\nOriginal prompt: ${prompt}`.
    #[serde(default)]
    pub suppress_original_prompt: bool,
    /// `hookSpecificOutput.displayContent` returned by a `MessageDisplay` hook
    /// (binary-confirmed at BIN off 201757586; description: "Text displayed in
    /// place of the delta. Omit (or return the delta unchanged) to display the
    /// original."). Additive default `None`. Applied by the orchestrator's
    /// completed-message `MessageDisplay` pass (P2-04): when `Some`, the joined
    /// assistant text is rendered ON SCREEN as this value while the stored
    /// message / JSONL keep the original (claude-code's `displayedMessageContent`,
    /// `Qff`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub display_content: Option<ExactHookText>,
    /// `hookSpecificOutput.watchPaths` returned by a `FileChanged` / `CwdChanged`
    /// hook (claude-code `parseHookJSONOutput`: `"watchPaths" in
    /// e.hookSpecificOutput && e.hookSpecificOutput.watchPaths`). A hook may add
    /// paths to the file-changed watch set; the desktop watcher restarts over
    /// the union when the folded set is non-empty (`fileChangedWatcher.ts`'s
    /// `if (v.length > 0) updateWatchPaths(v)`), and the `CwdChanged` flow
    /// re-resolves them against the new cwd. Present-key capture: an empty array
    /// is still `Some(vec![])` (JS arrays are always truthy), distinguishing it
    /// from an absent key (`None`); a non-array value is ignored. Additive
    /// default `None`, so hooks that omit it leave the watch set untouched.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text_array::serialize_option",
        deserialize_with = "raw_exact_hook_text_array::deserialize_option"
    )]
    pub watch_paths: Option<Vec<ExactHookText>>,
    /// `hookSpecificOutput.initialUserMessage` returned by a `SessionStart` hook
    /// (claude-code schema: `initialUserMessage:S.string().optional()`; consumed
    /// as `if(p.initialUserMessage)$os=p.initialUserMessage` — the pending initial
    /// user prompt). Scoped to `SessionStart`. Additive default `None`; the
    /// orchestrator injects it as a (non-meta) user message at session start.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "raw_exact_hook_text::serialize_option",
        deserialize_with = "raw_exact_hook_text::deserialize_option"
    )]
    pub initial_user_message: Option<ExactHookText>,
    /// `hookSpecificOutput.reloadSkills` returned by a `SessionStart` hook
    /// (claude-code schema: `reloadSkills:S.boolean().describe("Re-scan skill and
    /// command directories")`; consumed as `if(p.reloadSkills)u=!0`). Scoped to
    /// `SessionStart`. Additive default `None`. NOTE: the port loads skills/
    /// commands once at boot and has no hot-reload seam, so this is parsed +
    /// folded (no longer silently dropped) but its re-scan ACTION is a documented
    /// follow-up — honoring it requires a skill/command registry reload path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_skills: Option<bool>,
    /// Internal completion signal for `asyncRewake`. This never crosses the
    /// hook JSON wire; the desktop completion drain consumes it only after the
    /// result's model-facing context has been buffered.
    #[serde(skip)]
    pub async_rewake: bool,
    /// Internal marker that the command hook successfully transferred its
    /// remaining work to the runtime async registry after printing the
    /// `{"async":true}` first line. Callers use this to keep live progress
    /// open until the registry's completion callback fires. It is never part
    /// of the hook JSON contract.
    #[serde(skip)]
    pub async_backgrounded: bool,
}

/// The nested `hookSpecificOutput.decision` union for a `PermissionRequest`
/// hook. This is deliberately distinct from PreToolUse's legacy top-level
/// `decision` / `permissionDecision` strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequestResult {
    /// Approve the request, optionally rewriting input and permission rules.
    Allow {
        /// Raw `updatedInput`, if present.
        updated_input: Option<HookJson>,
        /// Raw `updatedPermissions`, if present.
        updated_permissions: Option<HookJson>,
    },
    /// Reject the request, optionally aborting the whole active turn.
    Deny {
        /// User-facing rejection message.
        message: Option<ExactHookText>,
        /// Whether the active turn should be interrupted.
        interrupt: Option<bool>,
    },
}

impl Serialize for PermissionRequestResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut fields = Vec::new();
        match self {
            Self::Allow {
                updated_input,
                updated_permissions,
            } => {
                fields.push(("behavior".to_string(), json_string("allow")));
                if let Some(value) = updated_input {
                    fields.push(("updatedInput".to_string(), value.clone()));
                }
                if let Some(value) = updated_permissions {
                    fields.push(("updatedPermissions".to_string(), value.clone()));
                }
            }
            Self::Deny { message, interrupt } => {
                fields.push(("behavior".to_string(), json_string("deny")));
                if let Some(value) = message {
                    fields.push(("message".to_string(), value.json_projection()));
                }
                if let Some(value) = interrupt {
                    fields.push((
                        "interrupt".to_string(),
                        HookJson::parse(if *value { "true" } else { "false" })
                            .expect("boolean JSON literal is valid"),
                    ));
                }
            }
        }
        let projection = HookJson::object(fields).map_err(serde::ser::Error::custom)?;
        lingxi_core::types::utf16_json::raw_json_serde::serialize(&projection, serializer)
    }
}

impl<'de> Deserialize<'de> for PermissionRequestResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;
        use serde_json::value::RawValue;

        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let projection = HookJson::parse(raw.get()).map_err(D::Error::custom)?;
        let object = projection
            .at_pointer("")
            .filter(|value| value.object_entries().is_some())
            .ok_or_else(|| D::Error::custom("expected permission decision object"))?;
        let behavior = object
            .get_property("behavior")
            .and_then(|value| value.as_str())
            .ok_or_else(|| D::Error::custom("missing string behavior"))?;
        match behavior {
            "allow" => {
                let updated_input = object
                    .get_property("updatedInput")
                    .map(|_| projection.subprojection("/updatedInput"))
                    .transpose()
                    .map_err(D::Error::custom)?;
                let updated_permissions = object
                    .get_property("updatedPermissions")
                    .map(|_| projection.subprojection("/updatedPermissions"))
                    .transpose()
                    .map_err(D::Error::custom)?;
                if updated_permissions
                    .as_ref()
                    .is_some_and(|value| value.array_len("").is_none())
                {
                    return Err(D::Error::custom("updatedPermissions must be an array"));
                }
                Ok(Self::Allow {
                    updated_input,
                    updated_permissions,
                })
            }
            "deny" => {
                let message = match object.get_property("message") {
                    None => None,
                    Some(value) if value.is_null() => None,
                    Some(value) => {
                        if value.as_str().is_none() {
                            return Err(D::Error::custom("message must be a string"));
                        }
                        ExactHookText::from_json_projection(&projection, "/message")
                            .ok_or_else(|| D::Error::custom("message must be a string"))
                            .map(Some)?
                    }
                };
                let interrupt = match object.get_property("interrupt") {
                    None => None,
                    Some(value) if value.is_null() => None,
                    Some(value) => Some(
                        value
                            .as_bool()
                            .ok_or_else(|| D::Error::custom("interrupt must be a boolean"))?,
                    ),
                };
                Ok(Self::Deny { message, interrupt })
            }
            _ => Err(D::Error::custom("unknown permission decision behavior")),
        }
    }
}

fn json_string(value: &str) -> HookJson {
    HookJson::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("valid Rust strings have matching UTF-16 display")
}

/// Structured elicitation answer a hook can return, mirroring claude-code's
/// `ElicitationResponse` (`{ action, content? }`). 1:1 with the
/// `hookSpecificOutput.action` / `hookSpecificOutput.content` fields parsed by
/// `parseElicitationHookOutput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitationHookResponse {
    /// One of `"accept"` / `"decline"` / `"cancel"` — forwarded verbatim so the
    /// MCP handler can return it as the elicitation `action`. Kept as a raw
    /// `String` to avoid coupling the hooks crate to the MCP action enum.
    pub action: String,
    /// Optional structured form content (only meaningful for `accept`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub content: Option<HookJson>,
}

/// The action a hook recommends after observing an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookDecision {
    /// Allow the action to proceed. Equivalent to no decision.
    Allow,
    /// Approve a permission-gated action without prompting the user.
    Approve,
    /// Block the in-flight action. The engine short-circuits remaining hooks
    /// and surfaces `reason` to the user.
    Block,
    /// Explicitly continue — useful as a tie-breaker when later hooks may
    /// otherwise block.
    Continue,
    /// Ask the user (`permissionDecision: "ask"`, claude-code BIN off
    /// ~205721920: `case"ask":u.permissionBehavior="ask"`; schema enum
    /// `["allow","deny","ask","defer"]`). A `PreToolUse` hook may force the
    /// tool call through the INTERACTIVE permission prompt regardless of any
    /// configured allow rule — `permissionBehavior="ask"` makes claude-code
    /// prompt the user even when a rule would otherwise auto-allow. Distinct
    /// from `Approve`/`Allow` (which SKIP the prompt) and `Block` (which
    /// denies outright). The turn-loop consumer must route this through the
    /// normal ask path (`PermissionGate::check`, which delegates to the prompt
    /// transport) rather than a hook-allow resolution path (`honour_hook_allow`
    /// or `check_after_hook_allow_rewritten`). Unconditionally
    /// reassigned by the `hookSpecificOutput.permissionDecision` switch, like
    /// every other case (matching `azn`'s second switch).
    Ask,
    /// Defer the tool call (#37, `permissionDecision: "defer"`, claude-code BIN
    /// off 205722868: `case"defer":u.permissionBehavior="defer"`; schema off
    /// 205719537: `'"allow" | "deny" | "ask" | "defer" (optional)'`). A
    /// `PreToolUse` hook may DEFER a solo tool call so it is re-attempted on a
    /// later (interactive) resume rather than run now. The orchestrator gates
    /// this on PRINT/non-interactive mode AND a single tool_use block in the
    /// batch (BIN off 202454844): in interactive mode or a multi-tool batch it
    /// warns and ignores (proceeds normally); on the gated path it emits the
    /// `tengu_pre_tool_hook_deferred` analytic, pushes a `hook_deferred_tool`
    /// meta message, and TERMINATES the turn with the `tool_deferred`
    /// stop-reason (the tool is NOT executed). DORMANT on the default
    /// interactive REPL path (the interactive-mode gate ignores it there).
    Defer,
}

/// Raw outcome of a single hook invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    /// Coarse status (`Success`, `Error`, `Cancelled`, `Timeout`).
    pub outcome: HookOutcome,
    /// Captured stdout (or transport body).
    #[serde(with = "raw_exact_hook_text")]
    pub stdout: ExactHookText,
    /// Captured stderr (or transport diagnostic).
    #[serde(with = "raw_exact_hook_text")]
    pub stderr: ExactHookText,
    /// Process exit code (for `Command` hooks); `None` for non-process
    /// executors.
    pub exit_code: Option<i32>,
    /// Parsed [`HookResponse`] if the hook returned valid JSON.
    pub response: Option<HookResponse>,
}

/// Coarse status of a single hook invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookOutcome {
    /// Hook executed successfully (status 0 / 2xx / `Ok`).
    Success,
    /// Hook executed but reported failure (non-zero exit, non-2xx, etc.).
    Error,
    /// Caller cancelled the hook before it finished.
    Cancelled,
    /// Hook exceeded its configured timeout.
    Timeout,
}

/// The two model-switch event shapes that pass through the Native classic
/// middleware result projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitchHookPhase {
    /// A model switch that has not yet been applied.
    Pre,
    /// A model switch that has already been applied.
    Post,
}

/// A generic classic Mod `decision.behavior` override kept separate from
/// explicit `permissionDecision`, which can independently create a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSwitchPermissionBehavior {
    /// Skip the confirmation prompt unless an explicit block already exists.
    Allow,
    /// Continue without an ask or skip-confirm signal unless blocked.
    Deny,
}

/// Source-distinct Native fields retained for a model-switch hook result.
/// These are kept separate from the normalized `decision` / `reason` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSwitchHookOutput {
    /// Native `block` text, when a hook returned an actual block field.
    pub block: Option<ExactHookText>,
    /// PreModelSwitch `permissionDecision` from the original hook JSON.
    pub permission_decision: Option<ExactHookText>,
    /// PreModelSwitch `permissionDecisionReason` from the original hook JSON.
    pub permission_decision_reason: Option<ExactHookText>,
    /// Top-level `stopReason` from the original hook JSON.
    pub stop_reason: Option<ExactHookText>,
    /// Internal Mod-only behavior override. Never part of Native's result
    /// projection; retained separately from explicit permission fields.
    pub mod_permission_behavior_override: Option<ModelSwitchPermissionBehavior>,
}

/// Invalid source shape while projecting a classic model-switch middleware
/// result into the current aggregate.
#[derive(Debug, thiserror::Error)]
pub enum ModelSwitchProjectionError {
    /// The middleware result must remain an object projection.
    #[error("model-switch middleware result must be a JSON object")]
    NotObject,
    /// A supported result member had a shape that Native does not accept.
    #[error("model-switch result field {field} must be {expected}")]
    InvalidField {
        /// Exact result field name.
        field: &'static str,
        /// Expected source shape.
        expected: &'static str,
    },
    /// Building or reading the exact flat projection failed.
    #[error(transparent)]
    Projection(#[from] lingxi_core::types::utf16_json::Utf16JsonProjectionError),
}

/// Merged result of all hooks fired for one event. The executor folds each
/// individual [`HookResult`] into this aggregate in priority order; the first
/// `Block` decision wins and stops further folding.
#[derive(Debug, Clone, Default)]
pub struct AggregateHookResult {
    /// Final aggregated decision (latest non-`None` wins until a `Block`).
    pub decision: Option<HookDecision>,
    /// Source of the hook whose decision currently owns [`Self::decision`].
    /// This follows the same sticky-first-block / otherwise-last-decision fold
    /// as the decision itself, so downstream deferred-tool records can retain
    /// provenance instead of guessing from the event name.
    pub hook_source: Option<crate::definition::HookSource>,
    /// Reason associated with the final decision.
    pub reason: Option<ExactHookText>,
    /// O2: the `command` half of the blocking hook's `blockingError` object,
    /// frozen at the FIRST blocker exactly like [`Self::reason`] (with which it
    /// forms one unit: `{blockingError: reason, command: block_command}`).
    ///
    /// `Some` only when [`Self::decision`] is [`HookDecision::Block`]. The
    /// PostToolUse / PostToolUseFailure consumers need it to build the
    /// `hook_blocking_error` attachment (BIN off 234726074 / 234728254), which
    /// the executor deliberately does NOT build itself — the oracle's blocking
    /// arm yields a bare signal with no attachment (BIN off 237805098).
    ///
    /// Which rendering lands here depends on the arm, per the runner's locals
    /// `ee=qq(q)` / `te=iSe(q)` (BIN off ~237802650): the plain-text exit-2 arm
    /// uses `iSe` (raw, ignores `statusMessage`), the JSON `decision:"block"`
    /// arm uses `qq` (prefers `statusMessage`). See
    /// [`HookResponse::block_command`].
    ///
    /// Additive default `None`, so callers that ignore it are unaffected.
    pub block_command: Option<ExactHookText>,
    /// Most recent `updated_input` if any hook mutated the action's input.
    pub modified_input: Option<HookJson>,
    /// The most recent event-specific `PermissionRequest` result. The generic
    /// [`Self::decision`] remains the precedence-bearing normalized verdict;
    /// this field preserves the allow/deny payload for downstream consumers.
    pub permission_request_result: Option<PermissionRequestResult>,
    /// Raw permission-rule updates from the winning/most-recent
    /// `PermissionRequest` allow. Entries are intentionally not normalized in
    /// the hooks crate so consumers can apply the original wire values.
    pub permission_updates: Vec<HookJson>,
    /// OR-folded `interrupt` signal from `PermissionRequest` deny results.
    pub interrupt: bool,
    /// All `systemMessage`s emitted by hooks, in execution order. These are
    /// **user/transcript-facing only** and must NOT reach the model (claude-code
    /// `hook_system_message` → `normalizeAttachmentForAPI` returns `[]`,
    /// `utils/messages.ts:4258`). Kept DISTINCT from [`Self::additional_contexts`].
    pub system_messages: Vec<ExactHookText>,
    /// All `additionalContext`s emitted by hooks, in execution order. These ARE
    /// model-facing: claude-code surfaces them via `hook_additional_context` as a
    /// `<system-reminder>` user message (`utils/messages.ts:4117`). The turn loop
    /// builds the PreToolUse model-facing context message from THIS field only —
    /// never from [`Self::system_messages`].
    pub additional_contexts: Vec<ExactHookText>,
    /// `true` when ANY folded hook requested *preventContinuation*
    /// (`continue: false`). For lifecycle (`Stop`) hooks this signals the
    /// turn loop to TERMINATE the agent rather than continue working — it
    /// takes precedence over a `Block` decision (B4 — `query.ts:1278`).
    /// OR-folded across hooks in [`crate::HookExecutorImpl::execute`]'s
    /// merge step; defaults to `false`, so a registry with no lifecycle
    /// hook leaves it untouched (behavior-neutral for existing callers).
    pub prevent_continuation: bool,
    /// All attachments produced by hooks, in execution order.
    pub attachments: Vec<HookJson>,
    /// Per-hook results, in execution order — for telemetry and debugging.
    pub all_results: Vec<(HookId, HookResult)>,
    /// Ordered Native history rows yielded by each UserPromptExpansion hook.
    /// These are deferred until the expanded body is appended, so hook output
    /// remains after the command's body in the same queued turn.
    pub prompt_expansion_history_rows: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// One `hook_progress` event per matching hook, in execution order,
    /// emitted *before* each hook runs (claude-code `utils/hooks.ts:2094-2116`).
    /// Carries the per-hook `status_message` so the spinner can substitute it
    /// for the generic running line. Additive / `..Default::default()`-compatible;
    /// defaults to empty, so existing callers that ignore it are unaffected.
    pub progress: Vec<HookProgressEvent>,
    /// The last elicitation answer any folded hook provided (claude-code
    /// `executeElicitationHooks`: the loop keeps the latest non-empty
    /// `elicitationResponse`). `Some` only for `Elicitation` /
    /// `ElicitationResult` dispatches where a hook set
    /// `hookSpecificOutput.action`. The MCP elicitation handler reads this to
    /// PROVIDE the response. Additive default `None`, so non-elicitation
    /// callers are unaffected.
    pub elicitation_response: Option<ElicitationHookResponse>,
    /// The last `updatedMCPToolOutput` any folded `PostToolUse` hook returned
    /// (claude-code keeps the most recent — `result.updatedMCPToolOutput =
    /// json.hookSpecificOutput.updatedMCPToolOutput`, `utils/hooks.ts:647`). The
    /// orchestrator substitutes it for the tool's result, but ONLY for MCP tools
    /// (`isMcpTool(tool)`, `toolHooks.ts:146`). Additive default `None`, so a
    /// dispatch with no mutating `PostToolUse` hook leaves the result unchanged
    /// (byte-identical).
    pub updated_mcp_tool_output: Option<HookJson>,
    /// The last `updatedToolOutput` any folded `PostToolUse` hook returned
    /// (claude-code keeps the most recent — BIN off 205724076). Unlike
    /// [`Self::updated_mcp_tool_output`] this applies to ALL tools (no
    /// `isMcpTool` gate) and uses `!== void 0` semantics (an explicit JSON
    /// `null` IS a replacement), modeled as `Option<HookJson>`: `None` = no hook
    /// set it, and a UJP null is an explicit `null` replacement. The orchestrator
    /// validates it against the tool's output schema and substitutes for the
    /// result (BIN off 202169384). Additive default `None` → byte-identical when
    /// no hook mutates the output.
    pub updated_tool_output: Option<HookJson>,
    /// SH-01 — every `classifierContext` a folded `PostToolUse` hook supplied,
    /// in execution order, already capped to
    /// [`CLASSIFIER_CONTEXT_CAP_UTF16`] (oracle 2.1.238 @ 296974134 yields one
    /// `classifierContexts:[{value,hostPrincipal}]` per contributing hook).
    ///
    /// These are host-asserted context lines shown to the auto-mode permission
    /// classifier alongside the tool call's result — they are NOT model-facing
    /// and must never be merged into [`Self::additional_contexts`].
    pub classifier_contexts: Vec<ClassifierHostContext>,
    /// SH-01 — `M.classifierContextChars` (oracle @ 296974134): the running sum
    /// of POST-cap UTF-16 lengths across every contributing hook. This is the
    /// "budget shared across all hooks that contribute to one call" the schema
    /// describes, and the value upstream also reports per plugin through `G`.
    pub classifier_context_chars: usize,
    /// SH-01 — `pairedRewrite` of the LAST hook that supplied a
    /// `classifierContext` (upstream yields one per contributing hook; the port
    /// keeps the most recent, matching every other last-wins channel here).
    /// `None` when no hook supplied a context.
    pub paired_rewrite: Option<PairedRewrite>,
    /// `true` when ANY folded `PermissionDenied` hook returned
    /// `hookSpecificOutput.retry: true` (claude-code `toolExecution.ts:1090`,
    /// `if (result.retry) hookSaysRetry = true`). The turn loop reads this — only
    /// on the gated classifier-deny path — to push the verbatim `isMeta` retry
    /// message. OR-folded across hooks; defaults to `false`, so a registry with
    /// no retrying `PermissionDenied` hook leaves it untouched (behavior-neutral
    /// for existing callers). DORMANT in the public build (see [`HookResponse::retry`]).
    pub retry: bool,
    /// The last `terminalSequence` any folded hook returned (#40, claude-code
    /// keeps the most recent — `szn` is invoked per hook result). `Some` only
    /// when a hook supplied a `terminalSequence`. A consumer (the TUI terminal
    /// writer) validates it via [`crate::terminal_seq::validate_terminal_sequence`]
    /// and either emits the accepted sequence to the active terminal or warns +
    /// drops it. Additive default `None` → byte-identical when no hook sets it.
    pub terminal_sequence: Option<ExactHookText>,
    /// The last `sessionTitle` any folded `UserPromptSubmit` hook returned
    /// (binary-confirmed at BIN off 201754804). Allows a hook to rename the
    /// session at prompt-submit time. `None` when no hook set it. The orchestrator
    /// applies this by triggering the session title update path.
    pub session_title: Option<ExactHookText>,
    /// `true` when ANY folded `UserPromptSubmit` hook returned
    /// `suppressOriginalPrompt: true` (binary-confirmed at BIN off 201754804;
    /// description: "When decision is 'block', omit the original prompt from the
    /// block message"). OR-folded: a single hook setting it flips the aggregate.
    /// Consumed by the orchestrator's `fire_user_prompt_submit` block-message
    /// render (P2-04) to omit the `Original prompt:` tail on a `Block`.
    pub suppress_original_prompt: bool,
    /// The last `displayContent` any folded `MessageDisplay` hook returned
    /// (binary-confirmed at BIN off 201757586). When `Some`, the orchestrator
    /// should substitute this text for the assistant delta on-screen (without
    /// affecting the stored message). `None` when no hook set it. Consumed by the
    /// orchestrator's completed-message `MessageDisplay` pass (P2-04,
    /// `fire_message_display_completed`): the joined assistant text is rendered
    /// ON SCREEN as this value while the stored message / JSONL keep the original.
    pub display_content: Option<ExactHookText>,
    /// Every `hookSpecificOutput.watchPaths` entry folded from the `FileChanged`
    /// / `CwdChanged` hook results, in execution order (claude-code `v3r` / `E3r`
    /// return `{ results, watchPaths, systemMessages }`, where `watchPaths` is
    /// the concatenation of each fired hook's `hookSpecificOutput.watchPaths`).
    /// The desktop file-changed watcher restarts over the union when non-empty
    /// (`if (v.length > 0) updateWatchPaths(v)`); an empty vec leaves the watch
    /// set unchanged. Additive default empty, so callers with no such hook are
    /// unaffected (byte-identical).
    pub watch_paths: Vec<ExactHookText>,
    /// The last `hookSpecificOutput.initialUserMessage` any folded `SessionStart`
    /// hook returned (claude-code keeps the latest — `if(p.initialUserMessage)
    /// $os=p.initialUserMessage`). `Some` seeds a pending initial user prompt that
    /// the orchestrator injects as a (non-meta) user message at session start.
    /// Additive default `None` → byte-identical when no hook sets it.
    pub initial_user_message: Option<ExactHookText>,
    /// One transcript `attachment` payload per synchronously completed hook
    /// run, in execution order (claude-code persists exactly one for every hook
    /// that runs — 26 048 records mined from real 2.1.220 transcripts). Built by
    /// [`crate::attachment`] from the run's outcome: `hook_success`,
    /// `hook_non_blocking_error`, or `hook_cancelled`. A run that BLOCKED
    /// contributes nothing here — claude yields a `hook_blocking_error`
    /// attachment on that arm instead, which the port does not model.
    /// Also pushed to the executor's
    /// [`crate::attachment::HookAttachmentSink`] when one is wired (the
    /// engine's transcript writer). Truly asynchronous completions are written
    /// through that sink after this aggregate has returned and therefore do not
    /// appear in this vector. Additive default empty.
    pub hook_attachments: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// `true` when ANY folded `SessionStart` hook returned `reloadSkills: true`
    /// (claude-code `if(p.reloadSkills)u=!0`, OR-folded). Signals the skill/command
    /// directories should be re-scanned. NOTE: the port has no hot-reload seam yet,
    /// so this is captured (no longer dropped at parse) but its re-scan action is a
    /// documented follow-up. Additive default `false`.
    pub reload_skills: bool,
    /// Source-distinct classic model-switch result fields after middleware
    /// projection. `None` means derive them from retained exact hook stdout.
    pub model_switch_output: Option<ModelSwitchHookOutput>,
}

impl AggregateHookResult {
    /// Build the narrow Native classic result object used between managed,
    /// Mod, and core model-switch middleware. Exact hook strings are projected
    /// directly from retained UTF-16 stdout; no serde `Value` conversion is
    /// used. `additionalContext` is omitted when the ordered aggregate is
    /// empty, and `preventContinuation` is emitted only when true.
    pub fn model_switch_classic_projection(
        &self,
        phase: ModelSwitchHookPhase,
    ) -> Result<HookJson, ModelSwitchProjectionError> {
        let native = self
            .model_switch_output
            .clone()
            .unwrap_or_else(|| self.model_switch_output_from_source(phase));
        model_switch_projection_from_parts(
            native,
            self.additional_contexts.clone(),
            self.prevent_continuation,
            phase,
        )
    }

    /// Apply Native's outer `gyt(managed, downstream)` wrapper to a final Mod
    /// return. `next(e)` receives only the core partition; managed fields are
    /// overlaid after the Mod chain returns.
    pub fn model_switch_classic_managed_overlay(
        &self,
        phase: ModelSwitchHookPhase,
        downstream: &HookJson,
    ) -> Result<(HookJson, Option<PermissionRequestResult>), ModelSwitchProjectionError> {
        if downstream
            .at_pointer("")
            .and_then(|value| value.object_entries())
            .is_none()
        {
            return Err(ModelSwitchProjectionError::NotObject);
        }
        let managed_output = self
            .model_switch_output
            .clone()
            .unwrap_or_else(|| self.model_switch_output_from_source(phase));
        let downstream_output = model_switch_output_from_projection(downstream, phase)?;
        if downstream.contains_property("permissionDecision")
            && !matches!(
                downstream_output
                    .permission_decision
                    .as_ref()
                    .map(|decision| decision.display.as_str()),
                Some("allow" | "ask" | "deny")
            )
        {
            return Err(ModelSwitchProjectionError::InvalidField {
                field: "permissionDecision",
                expected: "allow, ask, or deny",
            });
        }
        // Native model-switch M/C results do not produce a generic decision.
        // A Mod can, and k9n/_yt applies it after the managed/downstream
        // permission fold, so retain it separately until delta reconciliation.
        let mod_decision = permission_request_decision_from_mod_return(downstream, phase)?;
        let mut output = managed_output;
        merge_native_gyt_model_switch_outputs(&mut output, downstream_output, phase);

        let mut contexts = self.additional_contexts.clone();
        if downstream.contains_property("additionalContext") {
            contexts.extend(exact_string_array(downstream, "additionalContext")?);
        }
        let prevent_continuation =
            self.prevent_continuation || returned_prevent_continuation(downstream)?;
        Ok((
            model_switch_projection_from_parts(output, contexts, prevent_continuation, phase)?,
            mod_decision,
        ))
    }

    /// Native runs managed model-switch hooks before entering classic Mod
    /// middleware. Its managed-result predicate short-circuits on any block,
    /// preventContinuation, PreModelSwitch deny, or deny decision. Keep this
    /// gate at the partition boundary so later Mod/core results cannot erase it.
    pub fn model_switch_managed_blocks_mod_dispatch(&self, phase: ModelSwitchHookPhase) -> bool {
        let output = self
            .model_switch_output
            .clone()
            .unwrap_or_else(|| self.model_switch_output_from_source(phase));
        self.decision == Some(HookDecision::Block)
            || self
                .permission_request_result
                .as_ref()
                .is_some_and(|decision| matches!(decision, PermissionRequestResult::Deny { .. }))
            || self.prevent_continuation
            || output.block.is_some()
            || (phase == ModelSwitchHookPhase::Pre
                && output
                    .permission_decision
                    .as_ref()
                    .is_some_and(|decision| decision.display == "deny"))
    }

    /// Merge a direct classic Mod return that did not call `next(e)`.
    /// `managed` retains field provenance: Native overlays only ordinary
    /// properties that the managed result actually supplied, while contexts
    /// and permission decisions use their dedicated folds.
    pub fn apply_model_switch_classic_direct(
        &mut self,
        phase: ModelSwitchHookPhase,
        managed: &Self,
        baseline: &HookJson,
        returned: &HookJson,
    ) -> Result<(), ModelSwitchProjectionError> {
        self.apply_model_switch_classic_delta(phase, managed, baseline, returned)
    }

    /// Apply the PreModelSwitch-only `permissionBehavior` derived by Native
    /// from a validated classic Mod `decision` object. It remains separate
    /// from explicit `permissionDecision` / `block`: generic deny means
    /// proceed, while explicit deny still blocks.
    pub fn apply_model_switch_classic_mod_decision(
        &mut self,
        phase: ModelSwitchHookPhase,
        decision: Option<PermissionRequestResult>,
    ) -> Result<(), ModelSwitchProjectionError> {
        if phase != ModelSwitchHookPhase::Pre {
            return Ok(());
        }
        let Some(decision) = decision else {
            return Ok(());
        };
        let behavior = match decision {
            PermissionRequestResult::Allow { .. } => ModelSwitchPermissionBehavior::Allow,
            PermissionRequestResult::Deny { .. } => ModelSwitchPermissionBehavior::Deny,
        };
        let mut output = self
            .model_switch_output
            .clone()
            .unwrap_or_else(|| self.model_switch_output_from_source(phase));
        output.mod_permission_behavior_override = Some(behavior);
        let has_explicit_block = self.decision == Some(HookDecision::Block)
            || output.block.is_some()
            || output
                .permission_decision
                .as_ref()
                .is_some_and(|value| value.display == "deny");
        if !has_explicit_block {
            self.decision = match behavior {
                ModelSwitchPermissionBehavior::Allow => Some(HookDecision::Approve),
                ModelSwitchPermissionBehavior::Deny => None,
            };
        }
        self.model_switch_output = Some(output);
        Ok(())
    }

    /// Apply the final Mod projection against Native's managed-plus-last-core
    /// baseline, then fold the final downstream result with the original
    /// managed partition. Keeping `managed` separate is necessary because
    /// `gyt` overlays only fields actually present on that partition; core
    /// fields must remain replaceable by the Mod return.
    pub fn apply_model_switch_classic_delta(
        &mut self,
        phase: ModelSwitchHookPhase,
        managed: &Self,
        baseline: &HookJson,
        returned: &HookJson,
    ) -> Result<(), ModelSwitchProjectionError> {
        if baseline
            .at_pointer("")
            .and_then(|value| value.object_entries())
            .is_none()
            || returned
                .at_pointer("")
                .and_then(|value| value.object_entries())
                .is_none()
        {
            return Err(ModelSwitchProjectionError::NotObject);
        }

        let managed_output = managed
            .model_switch_output
            .clone()
            .unwrap_or_else(|| managed.model_switch_output_from_source(phase));
        let previous_decision = managed.decision;
        let previous_output = managed_output.clone();
        let mut downstream_output = model_switch_output_from_projection(baseline, phase)?;

        if projection_field_changed(baseline, returned, "block")? {
            downstream_output.block = exact_optional_string(returned, "block")?;
        }
        if phase == ModelSwitchHookPhase::Pre
            && projection_field_changed(baseline, returned, "permissionDecision")?
        {
            downstream_output.permission_decision =
                exact_optional_string(returned, "permissionDecision")?;
        }
        if phase == ModelSwitchHookPhase::Pre
            && projection_field_changed(baseline, returned, "permissionDecisionReason")?
        {
            downstream_output.permission_decision_reason =
                exact_optional_string(returned, "permissionDecisionReason")?;
        }
        if projection_field_changed(baseline, returned, "stopReason")? {
            downstream_output.stop_reason = exact_optional_string(returned, "stopReason")?;
        }
        if projection_field_changed(baseline, returned, "additionalContext")? {
            let baseline_contexts = if baseline.contains_property("additionalContext") {
                exact_string_array(baseline, "additionalContext")?
            } else {
                Vec::new()
            };
            let returned_contexts = if returned.contains_property("additionalContext") {
                exact_string_array(returned, "additionalContext")?
            } else {
                Vec::new()
            };
            append_returned_contexts(
                &mut self.additional_contexts,
                &baseline_contexts,
                returned_contexts,
            );
        }
        if projection_field_changed(baseline, returned, "preventContinuation")? {
            self.prevent_continuation = returned.contains_property("preventContinuation")
                && optional_bool(returned, "preventContinuation")?;
        }
        self.prevent_continuation |= managed.prevent_continuation;

        let mut output = managed_output;
        merge_native_gyt_model_switch_outputs(&mut output, downstream_output, phase);
        self.decision = managed.decision;
        self.reason = managed.reason.clone();
        if output.block.is_none() {
            self.block_command = None;
        }

        self.apply_model_switch_normalized_decision(
            phase,
            previous_decision,
            &previous_output,
            &output,
        )?;
        self.model_switch_output = Some(output);
        Ok(())
    }

    fn apply_model_switch_normalized_decision(
        &mut self,
        phase: ModelSwitchHookPhase,
        previous_decision: Option<HookDecision>,
        previous_output: &ModelSwitchHookOutput,
        output: &ModelSwitchHookOutput,
    ) -> Result<(), ModelSwitchProjectionError> {
        if phase == ModelSwitchHookPhase::Post {
            if previous_output.block.is_none() && output.block.is_some() {
                self.reason = output.block.clone();
            }
            return Ok(());
        }

        if previous_decision == Some(HookDecision::Block) || output.block.is_some() {
            self.decision = Some(HookDecision::Block);
            self.reason = output.block.clone().or_else(|| self.reason.clone());
            return Ok(());
        }

        let normalized = permission_decision(&output.permission_decision)?;
        if let Some(normalized) = normalized {
            self.decision = Some(normalized);
            self.reason = output.permission_decision_reason.clone();
        } else if previous_decision.is_none() {
            self.reason = output.permission_decision_reason.clone();
        }
        Ok(())
    }

    /// Fold a later source partition after the earlier managed partition.
    /// Side-effect vectors retain source order; model-switch decisions follow
    /// Native's severity fold and ordinary result fields keep the managed value.
    pub fn append_model_switch_partition(&mut self, phase: ModelSwitchHookPhase, mut later: Self) {
        let current_output = self
            .model_switch_output
            .clone()
            .unwrap_or_else(|| self.model_switch_output_from_source(phase));
        let later_output = later
            .model_switch_output
            .clone()
            .unwrap_or_else(|| later.model_switch_output_from_source(phase));
        let mut merged_output = current_output.clone();
        merge_native_yv_model_switch_outputs(&mut merged_output, later_output.clone(), phase);

        let already_blocked = self.decision == Some(HookDecision::Block)
            || self.prevent_continuation
            || current_output.block.is_some()
            || (phase == ModelSwitchHookPhase::Pre
                && current_output
                    .permission_decision
                    .as_ref()
                    .is_some_and(|decision| decision.display == "deny"));
        if !already_blocked {
            let mut later_owns_decision = false;
            if merged_output.block.is_some() || later.decision == Some(HookDecision::Block) {
                self.decision = Some(HookDecision::Block);
                self.reason = merged_output.block.clone().or_else(|| later.reason.clone());
                if later.decision == Some(HookDecision::Block)
                    || (current_output.block.is_none() && later_output.block.is_some())
                {
                    self.hook_source = later.hook_source;
                }
            } else if phase == ModelSwitchHookPhase::Pre {
                let current_rank = permission_rank(current_output.permission_decision.as_ref());
                let later_rank = permission_rank(later_output.permission_decision.as_ref());
                let later_owns_permission = later_rank > current_rank;
                if let Some(decision) = permission_decision(&merged_output.permission_decision)
                    .ok()
                    .flatten()
                {
                    self.decision = Some(decision);
                    if later_owns_permission {
                        self.hook_source = later.hook_source;
                    }
                    later_owns_decision = later_owns_permission;
                } else if decision_rank(later.decision) > decision_rank(self.decision) {
                    if let Some(decision) = later.decision {
                        self.decision = Some(decision);
                        self.hook_source = later.hook_source;
                        later_owns_decision = true;
                    }
                }
            } else if let Some(decision) = later.decision {
                self.decision = Some(decision);
                self.hook_source = later.hook_source;
                later_owns_decision = true;
            }
            if phase == ModelSwitchHookPhase::Pre {
                self.reason = if merged_output.block.is_some() {
                    merged_output.block.clone()
                } else if merged_output.permission_decision.is_some() {
                    merged_output
                        .permission_decision_reason
                        .clone()
                        .or_else(|| self.reason.clone())
                } else if later_owns_decision {
                    later.reason.clone().or_else(|| self.reason.clone())
                } else {
                    self.reason.clone()
                };
            } else if self.reason.is_none() {
                self.reason = later.reason.take();
            }
            if later.decision == Some(HookDecision::Block) {
                self.block_command = later.block_command.take();
            }
            if later.permission_request_result.is_some() {
                self.permission_request_result = later.permission_request_result.take();
                self.permission_updates = std::mem::take(&mut later.permission_updates);
            }
            if !later.permission_updates.is_empty() {
                self.permission_updates = std::mem::take(&mut later.permission_updates);
            }
        }
        self.modified_input = later.modified_input.take().or(self.modified_input.take());
        self.interrupt |= later.interrupt;
        self.system_messages.append(&mut later.system_messages);
        self.additional_contexts
            .append(&mut later.additional_contexts);
        self.prevent_continuation |= later.prevent_continuation;
        self.attachments.append(&mut later.attachments);
        self.all_results.append(&mut later.all_results);
        self.prompt_expansion_history_rows
            .append(&mut later.prompt_expansion_history_rows);
        self.progress.append(&mut later.progress);
        if later.elicitation_response.is_some() {
            self.elicitation_response = later.elicitation_response.take();
        }
        if later.updated_mcp_tool_output.is_some() {
            self.updated_mcp_tool_output = later.updated_mcp_tool_output.take();
        }
        if later.updated_tool_output.is_some() {
            self.updated_tool_output = later.updated_tool_output.take();
        }
        self.classifier_contexts
            .append(&mut later.classifier_contexts);
        self.classifier_context_chars = self
            .classifier_context_chars
            .saturating_add(later.classifier_context_chars);
        if later.paired_rewrite.is_some() {
            self.paired_rewrite = later.paired_rewrite;
        }
        self.retry |= later.retry;
        if later.terminal_sequence.is_some() {
            self.terminal_sequence = later.terminal_sequence.take();
        }
        if later.session_title.is_some() {
            self.session_title = later.session_title.take();
        }
        self.suppress_original_prompt |= later.suppress_original_prompt;
        if later.display_content.is_some() {
            self.display_content = later.display_content.take();
        }
        self.watch_paths.append(&mut later.watch_paths);
        if later.initial_user_message.is_some() {
            self.initial_user_message = later.initial_user_message.take();
        }
        self.hook_attachments.append(&mut later.hook_attachments);
        self.reload_skills |= later.reload_skills;
        self.model_switch_output = Some(merged_output);
    }

    fn model_switch_output_from_source(
        &self,
        phase: ModelSwitchHookPhase,
    ) -> ModelSwitchHookOutput {
        let mut output = ModelSwitchHookOutput::default();
        for (_, result) in &self.all_results {
            if result.response.is_none() {
                continue;
            }
            let source = HookJson::parse_utf16_json(&result.stdout.utf16_code_units);
            let Ok(source) = source else {
                // Non-JSON executor failures synthesize their decision in the
                // existing aggregate path; preserve that source instead of
                // inferring a permissionDecision from the normalized enum.
                if result
                    .response
                    .as_ref()
                    .is_some_and(|response| response.decision == Some(HookDecision::Block))
                {
                    output.block = result
                        .response
                        .as_ref()
                        .and_then(|response| response.reason.clone())
                        .or_else(|| self.reason.clone());
                }
                continue;
            };
            if source.string_at("/decision") == Some("block") {
                output.block = ExactHookText::from_json_projection(&source, "/reason");
            }
            if source.contains_property("stopReason") {
                output.stop_reason = ExactHookText::from_json_projection(&source, "/stopReason");
            }
            if phase == ModelSwitchHookPhase::Pre {
                if let Ok(specific) = source.subprojection("/hookSpecificOutput") {
                    if specific.contains_property("permissionDecision") {
                        let decision = ExactHookText::from_json_projection(
                            &source,
                            "/hookSpecificOutput/permissionDecision",
                        );
                        if decision.as_ref().is_some_and(|value| !value.is_empty()) {
                            output.permission_decision = decision;
                        }
                    }
                    if specific.contains_property("permissionDecisionReason") {
                        output.permission_decision_reason = ExactHookText::from_json_projection(
                            &source,
                            "/hookSpecificOutput/permissionDecisionReason",
                        );
                    }
                }
            }
        }
        if output.block.is_none()
            && self.decision == Some(HookDecision::Block)
            && output
                .permission_decision
                .as_ref()
                .map(|value| value.display.as_str())
                != Some("deny")
            && self.all_results.iter().any(|(_, result)| {
                result.response.as_ref().is_some_and(|response| {
                    response.decision == Some(HookDecision::Block)
                        && HookJson::parse_utf16_json(&result.stdout.utf16_code_units).is_err()
                })
            })
        {
            output.block = self.reason.clone();
        }
        output
    }
}

fn projection_field_changed(
    before: &HookJson,
    after: &HookJson,
    field: &'static str,
) -> Result<bool, ModelSwitchProjectionError> {
    let before_has = before.contains_property(field);
    let after_has = after.contains_property(field);
    if before_has != after_has {
        return Ok(true);
    }
    if !before_has {
        return Ok(false);
    }
    let before_value = before.subprojection(&format!("/{field}"))?;
    let after_value = after.subprojection(&format!("/{field}"))?;
    Ok(!before_value.semantically_eq(&after_value))
}

fn model_switch_projection_from_parts(
    output: ModelSwitchHookOutput,
    contexts: Vec<ExactHookText>,
    prevent_continuation: bool,
    phase: ModelSwitchHookPhase,
) -> Result<HookJson, ModelSwitchProjectionError> {
    let mut fields = Vec::new();
    if let Some(block) = output.block {
        fields.push(("block".to_owned(), block.json_projection()));
    }
    if !contexts.is_empty() {
        fields.push((
            "additionalContext".to_owned(),
            HookJson::array(
                contexts
                    .iter()
                    .map(ExactHookText::json_projection)
                    .collect(),
            )?,
        ));
    }
    if prevent_continuation {
        fields.push(("preventContinuation".to_owned(), HookJson::parse("true")?));
    }
    if let Some(stop_reason) = output.stop_reason {
        fields.push(("stopReason".to_owned(), stop_reason.json_projection()));
    }
    if phase == ModelSwitchHookPhase::Pre {
        if let Some(decision) = output.permission_decision {
            fields.push(("permissionDecision".to_owned(), decision.json_projection()));
        }
        if let Some(reason) = output.permission_decision_reason {
            fields.push((
                "permissionDecisionReason".to_owned(),
                reason.json_projection(),
            ));
        }
    }
    Ok(HookJson::object(fields)?)
}

fn model_switch_output_from_projection(
    projection: &HookJson,
    _phase: ModelSwitchHookPhase,
) -> Result<ModelSwitchHookOutput, ModelSwitchProjectionError> {
    let mut output = ModelSwitchHookOutput::default();
    if projection.contains_property("block") {
        output.block = exact_optional_string(projection, "block")?;
    }
    if projection.contains_property("stopReason") {
        output.stop_reason = exact_optional_string(projection, "stopReason")?;
    }
    if projection.contains_property("permissionDecision") {
        output.permission_decision = exact_optional_string(projection, "permissionDecision")?;
        if !matches!(
            output
                .permission_decision
                .as_ref()
                .map(|decision| decision.display.as_str()),
            Some("allow" | "ask" | "deny")
        ) {
            return Err(ModelSwitchProjectionError::InvalidField {
                field: "permissionDecision",
                expected: "allow, ask, or deny",
            });
        }
    }
    if projection.contains_property("permissionDecisionReason") {
        output.permission_decision_reason =
            exact_optional_string(projection, "permissionDecisionReason")?;
    }
    Ok(output)
}

fn permission_request_decision_from_mod_return(
    projection: &HookJson,
    phase: ModelSwitchHookPhase,
) -> Result<Option<PermissionRequestResult>, ModelSwitchProjectionError> {
    returned_prevent_continuation(projection)?;
    if !projection.contains_property("decision") {
        return Ok(None);
    }
    let root = projection
        .at_pointer("")
        .ok_or(ModelSwitchProjectionError::NotObject)?;
    crate::hook_payload::validate_permission_request_decision(root).map_err(|_| {
        ModelSwitchProjectionError::InvalidField {
            field: "decision",
            expected: "a valid allow or deny decision object",
        }
    })?;
    let decision = projection.subprojection("/decision")?;
    let serialized = decision.to_json_string()?;
    let decision = serde_json::from_str::<PermissionRequestResult>(&serialized).map_err(|_| {
        ModelSwitchProjectionError::InvalidField {
            field: "decision",
            expected: "a valid allow or deny decision object",
        }
    })?;
    Ok((phase == ModelSwitchHookPhase::Pre).then_some(decision))
}

fn returned_prevent_continuation(
    projection: &HookJson,
) -> Result<bool, ModelSwitchProjectionError> {
    if !projection.contains_property("preventContinuation") {
        return Ok(false);
    }
    if optional_bool(projection, "preventContinuation")? {
        Ok(true)
    } else {
        Err(ModelSwitchProjectionError::InvalidField {
            field: "preventContinuation",
            expected: "true",
        })
    }
}

/// Native's `pyt` permission fold is severity ordered (`allow < ask < deny`),
/// with the earlier result winning ties. A missing winning reason falls back
/// to the other reason only when both partitions carry the same decision.
fn merge_native_permission_fields(
    output: &mut ModelSwitchHookOutput,
    later_decision: Option<ExactHookText>,
    later_reason: Option<ExactHookText>,
) {
    let same_decision = output.permission_decision == later_decision;
    let current_rank = permission_rank(output.permission_decision.as_ref());
    let later_rank = permission_rank(later_decision.as_ref());
    match later_rank.cmp(&current_rank) {
        std::cmp::Ordering::Greater => {
            output.permission_decision = later_decision;
            output.permission_decision_reason = later_reason.or_else(|| {
                same_decision
                    .then(|| output.permission_decision_reason.clone())
                    .flatten()
            });
        }
        std::cmp::Ordering::Equal => {
            if output.permission_decision.is_none() {
                output.permission_decision = later_decision;
            }
            if output.permission_decision_reason.is_none() && same_decision {
                output.permission_decision_reason = later_reason;
            }
        }
        std::cmp::Ordering::Less => {
            if output.permission_decision_reason.is_none() && same_decision {
                output.permission_decision_reason = later_reason;
            }
        }
    }
}

/// Native `YV(managed, downstream)` spreads downstream ordinary fields over
/// managed fields, while folding permissions and contexts specially.
fn merge_native_yv_model_switch_outputs(
    output: &mut ModelSwitchHookOutput,
    later: ModelSwitchHookOutput,
    phase: ModelSwitchHookPhase,
) {
    if later.block.is_some() {
        output.block = later.block;
    }
    if phase == ModelSwitchHookPhase::Pre {
        merge_native_permission_fields(
            output,
            later.permission_decision,
            later.permission_decision_reason,
        );
    }
    if later.stop_reason.is_some() {
        output.stop_reason = later.stop_reason;
    }
}

/// Native `gyt` restores ordinary fields that were present on the managed
/// result over the final downstream value. The left side is the original
/// managed partition, not an already merged managed-plus-core aggregate.
fn merge_native_gyt_model_switch_outputs(
    output: &mut ModelSwitchHookOutput,
    later: ModelSwitchHookOutput,
    phase: ModelSwitchHookPhase,
) {
    if output.block.is_none() {
        output.block = later.block;
    }
    if phase == ModelSwitchHookPhase::Pre {
        merge_native_permission_fields(
            output,
            later.permission_decision,
            later.permission_decision_reason,
        );
    }
    if output.stop_reason.is_none() {
        output.stop_reason = later.stop_reason;
    }
}

fn permission_rank(decision: Option<&ExactHookText>) -> u8 {
    match decision.map(|value| value.display.as_str()) {
        Some("allow") => 1,
        Some("ask") => 2,
        Some("deny") => 3,
        _ => 0,
    }
}

fn decision_rank(decision: Option<HookDecision>) -> u8 {
    match decision {
        Some(HookDecision::Approve | HookDecision::Allow) => 1,
        Some(HookDecision::Ask) => 2,
        Some(HookDecision::Block) => 3,
        _ => 0,
    }
}

fn append_returned_contexts(
    aggregate: &mut Vec<ExactHookText>,
    baseline: &[ExactHookText],
    returned: Vec<ExactHookText>,
) {
    let shared_prefix = baseline
        .iter()
        .zip(returned.iter())
        .take_while(|(before, after)| before == after)
        .count();
    aggregate.extend(returned.into_iter().skip(shared_prefix));
}

fn exact_optional_string(
    projection: &HookJson,
    field: &'static str,
) -> Result<Option<ExactHookText>, ModelSwitchProjectionError> {
    if !projection.contains_property(field) {
        return Ok(None);
    }
    ExactHookText::from_json_projection(projection, &format!("/{field}"))
        .map(Some)
        .ok_or(ModelSwitchProjectionError::InvalidField {
            field,
            expected: "a string",
        })
}

fn exact_string_array(
    projection: &HookJson,
    field: &'static str,
) -> Result<Vec<ExactHookText>, ModelSwitchProjectionError> {
    if !projection.contains_property(field) {
        return Ok(Vec::new());
    }
    let length = projection.array_len(&format!("/{field}")).ok_or(
        ModelSwitchProjectionError::InvalidField {
            field,
            expected: "an ordered string array",
        },
    )?;
    (0..length)
        .map(|index| {
            ExactHookText::from_json_projection(projection, &format!("/{field}/{index}")).ok_or(
                ModelSwitchProjectionError::InvalidField {
                    field,
                    expected: "an ordered string array",
                },
            )
        })
        .collect()
}

fn optional_bool(
    projection: &HookJson,
    field: &'static str,
) -> Result<bool, ModelSwitchProjectionError> {
    if !projection.contains_property(field) {
        return Ok(false);
    }
    projection
        .at_pointer(&format!("/{field}"))
        .and_then(|value| value.as_bool())
        .ok_or(ModelSwitchProjectionError::InvalidField {
            field,
            expected: "a boolean",
        })
}

fn permission_decision(
    decision: &Option<ExactHookText>,
) -> Result<Option<HookDecision>, ModelSwitchProjectionError> {
    match decision.as_ref().map(|value| value.display.as_str()) {
        None => Ok(None),
        Some("") => Ok(None),
        Some("allow") => Ok(Some(HookDecision::Approve)),
        Some("deny") => Ok(Some(HookDecision::Block)),
        Some("ask") => Ok(Some(HookDecision::Ask)),
        Some(_) => Err(ModelSwitchProjectionError::InvalidField {
            field: "permissionDecision",
            expected: "allow, deny, or ask",
        }),
    }
}

#[cfg(test)]
mod exact_hook_text_tests {
    use super::{
        exact_string_array, model_switch_output_from_projection,
        permission_request_decision_from_mod_return, returned_prevent_continuation,
        ElicitationHookResponse, ExactHookText, HookJson, HookOutcome, HookResponse, HookResult,
        ModelSwitchHookPhase, ModelSwitchProjectionError, PermissionRequestResult,
    };

    #[test]
    fn classic_mod_null_decision_is_rejected_instead_of_becoming_deny_or_allow() {
        let returned = HookJson::parse(r#"{"decision":null}"#).unwrap();
        assert!(matches!(
            permission_request_decision_from_mod_return(&returned, ModelSwitchHookPhase::Pre),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "decision",
                ..
            })
        ));
    }

    #[test]
    fn post_model_switch_validates_ignored_permission_fields_and_decision_shape() {
        let empty_decision = HookJson::parse(r#"{"permissionDecision":""}"#).unwrap();
        assert!(matches!(
            model_switch_output_from_projection(&empty_decision, ModelSwitchHookPhase::Post),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "permissionDecision",
                ..
            })
        ));

        let null_reason = HookJson::parse(r#"{"permissionDecisionReason":null}"#).unwrap();
        assert!(matches!(
            model_switch_output_from_projection(&null_reason, ModelSwitchHookPhase::Post),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "permissionDecisionReason",
                ..
            })
        ));

        let null_decision = HookJson::parse(r#"{"stopReason":"ignored","decision":null}"#).unwrap();
        assert!(matches!(
            permission_request_decision_from_mod_return(&null_decision, ModelSwitchHookPhase::Post),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "decision",
                ..
            })
        ));

        let invalid_behavior = HookJson::parse(r#"{"decision":{"behavior":"ask"}}"#).unwrap();
        assert!(matches!(
            permission_request_decision_from_mod_return(
                &invalid_behavior,
                ModelSwitchHookPhase::Post
            ),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "decision",
                ..
            })
        ));

        let invalid_updated_input =
            HookJson::parse(r#"{"decision":{"behavior":"allow","updatedInput":null}}"#).unwrap();
        assert!(matches!(
            permission_request_decision_from_mod_return(
                &invalid_updated_input,
                ModelSwitchHookPhase::Post
            ),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "decision",
                ..
            })
        ));
    }

    #[test]
    fn classic_mod_return_rejects_false_prevent_continuation_but_accepts_empty_context_items() {
        let false_prevent = HookJson::parse(r#"{"preventContinuation":false}"#).unwrap();
        assert!(matches!(
            returned_prevent_continuation(&false_prevent),
            Err(ModelSwitchProjectionError::InvalidField {
                field: "preventContinuation",
                ..
            })
        ));

        let empty_context = HookJson::parse(r#"{"additionalContext":[""]}"#).unwrap();
        let contexts = exact_string_array(&empty_context, "additionalContext").unwrap();
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].utf16_code_units.is_empty());
    }

    #[test]
    fn exact_surrogate_text_cannot_be_silently_serialized_as_display() {
        let exact = ExactHookText::from_utf16(vec![u16::from(b'x'), 0xD800]);
        assert!(serde_json::to_string(&exact).is_err());

        let exact_json = exact.json_projection().to_json_string().unwrap();
        assert_eq!(exact_json, r#""x\ud800""#);
    }

    #[test]
    fn native_preview_can_end_in_a_high_surrogate_but_error_cap_cannot() {
        let mut units = vec![u16::from(b'a'); 1_999];
        units.extend([0xD83D, 0xDE00, u16::from(b'!')]);
        let source = ExactHookText::from_utf16(units);
        let (preview, has_more) = source.preview(2_000);
        assert!(has_more);
        assert_eq!(preview.len_utf16(), 2_000);
        assert_eq!(preview.utf16_code_units.last(), Some(&0xD83D));
        assert_eq!(preview.display.chars().last(), Some('�'));
        assert!(preview
            .json_projection()
            .to_json_string()
            .unwrap()
            .contains(r#"\ud83d"#));

        let capped = source.truncate_well_formed(2_000);
        assert_eq!(capped.len_utf16(), 1_999);
        assert_eq!(capped.utf16_code_units.last(), Some(&u16::from(b'a')));
    }

    #[test]
    fn zero_length_well_formed_cap_is_safe_at_a_surrogate_boundary() {
        let source = ExactHookText::from_utf16(vec![0xD800]);
        assert!(source.truncate_well_formed(0).is_empty());
    }

    #[test]
    fn preview_prefers_only_a_newline_past_the_midpoint() {
        let mut units = vec![u16::from(b'a'); 1_500];
        units.push(0x000A);
        units.extend(vec![u16::from(b'b'); 700]);
        let source = ExactHookText::from_utf16(units);
        let (preview, has_more) = source.preview(2_000);
        assert!(has_more);
        assert_eq!(preview.len_utf16(), 1_500);
        assert_eq!(preview.utf16_code_units.last(), Some(&u16::from(b'a')));
    }

    #[test]
    fn text_encoder_byte_cap_does_not_split_a_scalar() {
        let emoji = ExactHookText::from_text("😀x");
        assert!(emoji.truncate_utf8_bytes(3).utf16_code_units.is_empty());
        assert_eq!(emoji.truncate_utf8_bytes(4).display, "😀");
        let lone = ExactHookText::from_utf16(vec![0xD800, u16::from(b'x')]);
        assert!(lone.truncate_utf8_bytes(2).utf16_code_units.is_empty());
        assert_eq!(lone.truncate_utf8_bytes(3).display, "�");
    }

    #[test]
    fn trim_matches_ecmascript_bom_and_excludes_non_whitespace() {
        let text = ExactHookText::from_text("\u{FEFF}\u{00A0}x\u{FEFF}\u{200B}");
        assert_eq!(text.trim_js().display, "x\u{FEFF}\u{200B}");
    }

    #[test]
    fn hook_response_json_members_keep_source_shape_null_presence_and_surrogates() {
        let mut response = HookResponse::default();
        response.updated_input =
            Some(HookJson::parse(r#"{"text":"\ud800","extra":[null]}"#).unwrap());
        response.updated_permissions = Some(
            HookJson::parse(r#"[{"type":"setMode","mode":"auto","destination":"session"}]"#)
                .unwrap(),
        );
        response.structured_content =
            Some(HookJson::parse(r#"{"meta":{"value":"\udfff"}}"#).unwrap());
        response.updated_mcp_tool_output =
            Some(HookJson::parse(r#"{"content":[{"text":"\ud800"},null]}"#).unwrap());
        response.updated_tool_output = Some(HookJson::parse("null").unwrap());
        response.elicitation_response = Some(ElicitationHookResponse {
            action: "accept".into(),
            content: Some(HookJson::parse(r#"{"answer":"\ud800"}"#).unwrap()),
        });
        response.reason = Some(ExactHookText::from_utf16(vec![0xD800]));
        response.terminal_sequence = Some(ExactHookText::from_utf16(vec![0xDC00]));
        response.session_title = Some(ExactHookText::from_utf16(vec![0xD800]));
        response.display_content = Some(ExactHookText::from_utf16(vec![0xDC00]));
        response.watch_paths = Some(vec![ExactHookText::from_utf16(vec![0xD800])]);
        response.classifier_context = Some(ExactHookText::from_utf16(vec![0xD800]));
        response.system_message = Some(ExactHookText::from_utf16(vec![0xDC00]));
        response.additional_context = Some(ExactHookText::from_utf16(vec![0xD800]));
        response.initial_user_message = Some(ExactHookText::from_utf16(vec![0xDC00]));
        response
            .attachments
            .push(HookJson::parse(r#"{"kind":"tool","value":"\udfff"}"#).unwrap());
        response.permission_request_result = Some(PermissionRequestResult::Allow {
            updated_input: Some(HookJson::parse(r#"{"nested":"\ud800"}"#).unwrap()),
            updated_permissions: Some(
                HookJson::parse(r#"[{"type":"addRules","rules":[]}]"#).unwrap(),
            ),
        });

        let encoded = serde_json::to_string(&response).unwrap();
        assert!(encoded.contains(r#""updated_input":{"text":"\ud800","extra":[null]}"#));
        assert!(encoded.contains(
            r#""updated_permissions":[{"type":"setMode","mode":"auto","destination":"session"}]"#
        ));
        assert!(encoded.contains(r#""structured_content":{"meta":{"value":"\udfff"}}"#));
        assert!(
            encoded.contains(r#""updated_mcp_tool_output":{"content":[{"text":"\ud800"},null]}"#)
        );
        assert!(encoded.contains(r#""updated_tool_output":null"#));
        assert!(encoded.contains(
            r#""elicitation_response":{"action":"accept","content":{"answer":"\ud800"}}"#
        ));
        assert!(encoded.contains(r#""reason":"\ud800""#));
        assert!(encoded.contains(r#""terminal_sequence":"\udc00""#));
        assert!(encoded.contains(r#""session_title":"\ud800""#));
        assert!(encoded.contains(r#""display_content":"\udc00""#));
        assert!(encoded.contains(r#""watch_paths":["\ud800"]"#));
        assert!(encoded.contains(r#""classifier_context":"\ud800""#));
        assert!(encoded.contains(r#""system_message":"\udc00""#));
        assert!(encoded.contains(r#""additional_context":"\ud800""#));
        assert!(encoded.contains(r#""initial_user_message":"\udc00""#));
        assert!(encoded.contains(r#""attachments":[{"kind":"tool","value":"\udfff"}]"#));
        assert!(encoded.contains(r#""permission_request_result":{"behavior":"allow","updatedInput":{"nested":"\ud800"},"updatedPermissions":[{"type":"addRules","rules":[]}]}"#));
        assert!(!encoded.contains("\"nodes\""));

        let decoded: HookResponse = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.updated_input, response.updated_input);
        assert_eq!(decoded.updated_permissions, response.updated_permissions);
        assert_eq!(decoded.structured_content, response.structured_content);
        assert_eq!(
            decoded.updated_mcp_tool_output,
            response.updated_mcp_tool_output
        );
        assert_eq!(decoded.updated_tool_output, response.updated_tool_output);
        assert_eq!(decoded.elicitation_response, response.elicitation_response);
        assert_eq!(decoded.reason, response.reason);
        assert_eq!(decoded.terminal_sequence, response.terminal_sequence);
        assert_eq!(decoded.session_title, response.session_title);
        assert_eq!(decoded.display_content, response.display_content);
        assert_eq!(decoded.watch_paths, response.watch_paths);
        assert_eq!(decoded.classifier_context, response.classifier_context);
        assert_eq!(decoded.system_message, response.system_message);
        assert_eq!(decoded.additional_context, response.additional_context);
        assert_eq!(decoded.initial_user_message, response.initial_user_message);
        assert_eq!(decoded.attachments, response.attachments);
        assert_eq!(
            decoded.permission_request_result,
            response.permission_request_result
        );

        let omitted: HookResponse = serde_json::from_str("{}").unwrap();
        assert!(omitted.updated_tool_output.is_none());
    }

    #[test]
    fn hook_result_output_keeps_exact_utf16_text_in_its_json_fields() {
        let result = HookResult {
            outcome: HookOutcome::Success,
            stdout: ExactHookText::from_utf16(vec![0xD800]),
            stderr: ExactHookText::from_utf16(vec![0xDC00]),
            exit_code: Some(0),
            response: None,
        };
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(encoded.contains(r#""stdout":"\ud800""#));
        assert!(encoded.contains(r#""stderr":"\udc00""#));
        let decoded: HookResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.stdout, result.stdout);
        assert_eq!(decoded.stderr, result.stderr);
    }

    #[test]
    fn permission_request_deny_message_keeps_exact_utf16_through_serde() {
        let result = PermissionRequestResult::Deny {
            message: Some(ExactHookText::from_utf16(vec![0xD800, u16::from(b'!')])),
            interrupt: Some(true),
        };
        let encoded = serde_json::to_string(&result).unwrap();
        assert_eq!(
            encoded,
            r#"{"behavior":"deny","message":"\ud800!","interrupt":true}"#
        );
        let decoded: PermissionRequestResult = serde_json::from_str(&encoded).unwrap();
        let PermissionRequestResult::Deny {
            message: Some(message),
            interrupt: Some(true),
        } = decoded
        else {
            panic!("expected the exact deny response");
        };
        assert_eq!(message.utf16_code_units, [0xD800, u16::from(b'!')]);
    }
}
