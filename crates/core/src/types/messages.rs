//! Conversation message DTOs.
//!
//! Mirrors `Anthropic`'s `Message` content-block model but is API-neutral —
//! provider adapters in `lingxi-api-client` map their native shapes to these.

use crate::types::ids::{MessageId, ToolUseId};
use crate::types::utf16_json::Utf16JsonProjection;
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::fmt;

fn deserialize_present_nullable_string<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

fn serialize_json_projection<S>(
    projection: &Utf16JsonProjection,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let source = projection
        .to_json_string()
        .map_err(serde::ser::Error::custom)?;
    let raw = RawValue::from_string(source).map_err(serde::ser::Error::custom)?;
    raw.serialize(serializer)
}

fn serialize_optional_projection<S>(
    projection: &Option<Utf16JsonProjection>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match projection {
        Some(projection) => serialize_json_projection(projection, serializer),
        None => serializer.serialize_none(),
    }
}

fn serialize_present_nullable_projection<S>(
    projection: &Option<Option<Utf16JsonProjection>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match projection {
        None | Some(None) => serializer.serialize_none(),
        Some(Some(projection)) => serialize_json_projection(projection, serializer),
    }
}

fn serialize_root_string_projection<S>(
    projection: &Utf16JsonProjection,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if projection.string_units("").is_none() {
        return Err(serde::ser::Error::custom(
            "projection must contain a root JSON string",
        ));
    }
    serialize_json_projection(projection, serializer)
}

fn serialize_optional_root_string_projection<S>(
    projection: &Option<Utf16JsonProjection>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match projection {
        Some(projection) => serialize_root_string_projection(projection, serializer),
        None => serializer.serialize_none(),
    }
}

fn deserialize_optional_root_string_projection<'de, D>(
    deserializer: D,
) -> Result<Option<Utf16JsonProjection>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    if raw.get().trim() == "null" {
        return Ok(None);
    }
    let projection = Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom)?;
    if projection.string_units("").is_none() {
        return Err(serde::de::Error::custom(
            "projection must contain a root JSON string",
        ));
    }
    Ok(Some(projection))
}

fn null_json_projection() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

/// An object whose member values remain raw JSON source until their semantic
/// field is selected. This bypasses serde's internally-tagged-enum
/// `ContentVisitor`, which eagerly buffers nested values and cannot carry
/// `RawValue` fields without recursively building a JSON tree.
struct RawObject(HashMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawObjectVisitor;

        impl<'de> Visitor<'de> for RawObjectVisitor {
            type Value = RawObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object with raw member values")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    // Match JSON.parse's last-member-wins behavior. Importantly,
                    // the value is captured as source before any typed branch
                    // can recursively deserialize it.
                    fields.insert(key, map.next_value::<Box<RawValue>>()?);
                }
                Ok(RawObject(fields))
            }
        }

        deserializer.deserialize_map(RawObjectVisitor)
    }
}

fn raw_required<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    serde_json::from_str(raw.get()).map_err(E::custom)
}

fn raw_optional<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<T>, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    fields
        .remove(name)
        .map(|raw| serde_json::from_str(raw.get()).map_err(E::custom))
        .transpose()
}

fn raw_optional_string<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<String>, E>
where
    E: serde::de::Error,
{
    match fields.remove(name) {
        None => Ok(None),
        Some(raw) if raw.get().trim() == "null" => Ok(None),
        Some(raw) => serde_json::from_str(raw.get()).map(Some).map_err(E::custom),
    }
}

fn raw_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Utf16JsonProjection, E>
where
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    Utf16JsonProjection::parse(raw.get()).map_err(E::custom)
}

fn raw_optional_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<Utf16JsonProjection>, E>
where
    E: serde::de::Error,
{
    fields
        .remove(name)
        .map(|raw| Utf16JsonProjection::parse(raw.get()).map_err(E::custom))
        .transpose()
}

fn raw_optional_nullable<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<T>, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    match fields.remove(name) {
        None => Ok(None),
        Some(raw) if raw.get().trim() == "null" => Ok(None),
        Some(raw) => serde_json::from_str(raw.get()).map(Some).map_err(E::custom),
    }
}

fn raw_optional_root_string_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<Utf16JsonProjection>, E>
where
    E: serde::de::Error,
{
    match fields.remove(name) {
        None => Ok(None),
        Some(raw) if raw.get().trim() == "null" => Ok(None),
        Some(raw) => {
            let projection = Utf16JsonProjection::parse(raw.get()).map_err(E::custom)?;
            if projection.string_units("").is_none() {
                return Err(E::custom(format!("field {name} must be a JSON string")));
            }
            Ok(Some(projection))
        }
    }
}

fn raw_string_json_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Utf16JsonProjection, E>
where
    E: serde::de::Error,
{
    let projection = raw_projection(fields, name)?;
    if projection.string_units("").is_none() {
        return Err(E::custom(format!("field {name} must be a JSON string")));
    }
    Ok(projection)
}

fn raw_projection_or_null<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Utf16JsonProjection, E>
where
    E: serde::de::Error,
{
    match fields.remove(name) {
        Some(raw) => Utf16JsonProjection::parse(raw.get()).map_err(E::custom),
        None => Ok(null_json_projection()),
    }
}

fn raw_projection_array_optional<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<Utf16JsonProjection>, E>
where
    E: serde::de::Error,
{
    let Some(raw) = fields.remove(name) else {
        return Ok(None);
    };
    let projection = Utf16JsonProjection::parse(raw.get()).map_err(E::custom)?;
    if projection
        .at_pointer("")
        .and_then(|root| root.array_len())
        .is_none()
    {
        return Err(E::custom(format!("field {name} must be a JSON array")));
    }
    Ok(Some(projection))
}

fn raw_string_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<(String, Vec<u16>), E>
where
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    let projection = Utf16JsonProjection::parse(raw.get()).map_err(E::custom)?;
    let display = projection
        .string_at("")
        .ok_or_else(|| E::custom(format!("field {name} must be a JSON string")))?
        .to_owned();
    let units = projection
        .string_units("")
        .ok_or_else(|| E::custom(format!("field {name} must be a JSON string")))?;
    Ok((display, units))
}

fn raw_present_nullable_projection<E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<Option<Option<Utf16JsonProjection>>, E>
where
    E: serde::de::Error,
{
    match fields.remove(name) {
        None => Ok(None),
        Some(raw) if raw.get().trim() == "null" => Ok(Some(None)),
        Some(raw) => Utf16JsonProjection::parse(raw.get())
            .map(Some)
            .map(Some)
            .map_err(E::custom),
    }
}

fn raw_content_blocks<E>(raw: &RawValue) -> Result<Vec<ContentBlock>, E>
where
    E: serde::de::Error,
{
    struct RawArray(Vec<Box<RawValue>>);
    impl<'de> Deserialize<'de> for RawArray {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct RawArrayVisitor;
            impl<'de> Visitor<'de> for RawArrayVisitor {
                type Value = RawArray;
                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an array with raw JSON elements")
                }
                fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                where
                    A: serde::de::SeqAccess<'de>,
                {
                    let mut items = Vec::new();
                    while let Some(item) = seq.next_element::<Box<RawValue>>()? {
                        items.push(item);
                    }
                    Ok(RawArray(items))
                }
            }
            deserializer.deserialize_seq(RawArrayVisitor)
        }
    }

    let RawArray(items) = serde_json::from_str(raw.get()).map_err(E::custom)?;
    items
        .into_iter()
        .map(|item| serde_json::from_str(item.get()).map_err(E::custom))
        .collect()
}

/// Whether a raw nested tool-result value represents provider-visible media.
///
/// This is shared by the delegation collector and the final provider media cap
/// so both paths retain the same set of image/document blocks.
#[must_use]
pub fn is_nested_media_value(value: crate::types::utf16_json::JsonValueView<'_>) -> bool {
    matches!(
        value.get_property("type").and_then(|value| value.as_str()),
        Some("image") | Some("image_url") | Some("document")
    )
}

/// High-level role of a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    /// Message from the human user.
    User,
    /// Message produced by the assistant model.
    Assistant,
    /// Out-of-band system instructions.
    System,
}

/// Native outer-envelope annotation on a user message produced by the
/// turn-companion flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageLimitNote {
    /// The companion is wrapping up a turn near its usage limit.
    WrapUp,
    /// The companion is releasing control at the current turn boundary.
    Release,
    /// The companion is announcing that the usage limit is near.
    NearLimit,
}

/// One block of structured content within a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Provider-native replay data. Never executed as an application tool.
    ProviderContent {
        /// Communication protocol which owns this payload.
        protocol: String,
        /// Unmodified provider content, including signed/encrypted reasoning.
        #[serde(serialize_with = "serialize_json_projection")]
        value: Utf16JsonProjection,
    },
    /// Plain UTF-8 text.
    Text {
        /// The text body.
        text: String,
        /// Exact provider field presence. The outer option distinguishes an
        /// absent key from a present key; the inner option preserves JSON null.
        #[serde(
            default,
            serialize_with = "serialize_present_nullable_projection",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<Utf16JsonProjection>>,
    },
    /// Display-safe text carrying an exact JS UTF-16 request-wire image.
    ///
    /// This is used only for narrow byte-parity cases where the provider wire
    /// must preserve lone surrogates (for example a `slice(0, n)` boundary in
    /// JS). `text` remains valid UTF-8 for display/history helpers; the exact
    /// provider-visible string lives in `utf16_code_units`.
    TextJsUtf16 {
        /// Display-safe text body.
        text: String,
        /// Exact UTF-16 code units that must be emitted on the provider wire.
        utf16_code_units: Vec<u16>,
        /// Exact provider field presence. See [`ContentBlock::Text::citations`].
        #[serde(
            default,
            serialize_with = "serialize_present_nullable_projection",
            skip_serializing_if = "Option::is_none"
        )]
        citations: Option<Option<Utf16JsonProjection>>,
    },
    /// A tool invocation requested by the assistant.
    ToolUse {
        /// Identifier echoed back in the matching `ToolResult`.
        id: ToolUseId,
        /// Tool name (e.g. `"Read"`, `"Bash"`).
        name: String,
        /// Tool-specific exact JSON input.
        #[serde(serialize_with = "serialize_json_projection")]
        input: Utf16JsonProjection,
        /// Verbatim provider-issued tool-call id (e.g. Anthropic `"toolu_01…"`,
        /// `OpenAI` `"call_…"`). `ToolUseId` is a UUID newtype and cannot hold a
        /// provider string, so the original is preserved here and replayed
        /// verbatim on egress — Anthropic pairs `tool_result.tool_use_id` to the
        /// `tool_use.id` it issued, and claude-code never rewrites the id.
        /// `None` for blocks minted internally (no provider round-trip).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_id: Option<String>,
    },
    /// The result of a previously-requested tool call.
    ToolResult {
        /// ID of the `ToolUse` this result belongs to.
        tool_use_id: ToolUseId,
        /// Stringified output payload (the model-facing TEXT / display form, and
        /// the egress `tool_result.content` when [`content_blocks`] is `None`).
        #[serde(serialize_with = "serialize_json_projection")]
        content: Utf16JsonProjection,
        /// Whether the tool reported failure. `None` also preserves the native
        /// omission of this optional field; `Some(false)` remains explicit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        /// Verbatim provider id of the `ToolUse` this answers — copied from the
        /// paired [`ContentBlock::ToolUse::provider_id`] so the egress
        /// `tool_result.tool_use_id` matches the provider-issued `tool_use.id`.
        /// `None` when the paired call had no provider id (internally minted).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_use_id: Option<String>,
        /// Model-facing content-BLOCK array, when the tool result is a structured
        /// content array rather than plain text (e.g. an MCP result with image /
        /// resource blocks). When `Some`, the egress sends this array VERBATIM as
        /// the `tool_result.content` (claude-code `mapToolResultToToolResultBlockParam`
        /// passes the MCP `content` array directly — text blocks stay separate,
        /// images stay viewable); when `None`, the egress falls back to the
        /// stringified [`content`]. Every non-MCP tool leaves this `None`, so its
        /// wire form is unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(serialize_with = "serialize_optional_projection")]
        content_blocks: Option<Utf16JsonProjection>,
    },
    /// Extended-thinking reasoning trace.
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// Optional cryptographic signature attesting to the trace.
        signature: Option<String>,
    },
    /// An image input (vision). Serializes to the Anthropic image-block wire
    /// shape; the OpenAI/Gemini codecs translate it to their native forms.
    Image {
        /// Where the image bytes come from.
        source: ImageSource,
    },
    /// A document input (e.g. a PDF). Serializes to the Anthropic document-block
    /// wire shape; the OpenAI/Gemini codecs translate or drop it.
    Document {
        /// Where the document bytes come from.
        source: DocumentSource,
    },
    /// Opaque redacted-reasoning block that must round-trip unmodified.
    ///
    /// Wire tag `redacted_thinking` (Anthropic protected-thinking beta). The
    /// `data` payload is provider-opaque and is preserved verbatim through
    /// resume/replay so JSONL bytes stay intact.
    RedactedThinking {
        /// Provider-opaque payload (base64-ish encrypted thinking).
        data: String,
    },
    /// Anthropic server-side tool invocation (advisor / `web_search`).
    ///
    /// Wire tag `server_tool_use`. Mirrors `llm_runtime::ContentBlock::ServerToolUse`
    /// exactly so it round-trips back to the API on the next request.
    ServerToolUse {
        /// Server-issued tool-use identifier.
        id: String,
        /// Name of the server tool being invoked.
        name: String,
        /// Tool input arguments (provider-specific JSON shape).
        #[serde(
            default = "null_json_projection",
            serialize_with = "serialize_json_projection"
        )]
        input: Utf16JsonProjection,
    },
    /// Anthropic Connector-Text block.
    ///
    /// Wire tag `connector_text`. Field name `connector_text` mirrors
    /// `llm_runtime::ContentBlock::ConnectorText` exactly (NOT `text`).
    ConnectorText {
        /// Connector-emitted text payload.
        #[serde(default)]
        connector_text: String,
        /// Optional provider integrity signature.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Advisor tool result mirrored from the server.
    ///
    /// Wire tag `advisor_tool_result`. Mirrors
    /// `llm_runtime::ContentBlock::AdvisorToolResult` exactly.
    AdvisorToolResult {
        /// Identifier of the originating `server_tool_use` block.
        tool_use_id: String,
        /// Tool result content (provider-specific JSON shape).
        #[serde(default)]
        #[serde(serialize_with = "serialize_json_projection")]
        content: Utf16JsonProjection,
        /// Whether the tool reported an error.
        #[serde(default)]
        is_error: bool,
    },
    /// Internal sidecar describing delegated vision analysis for one question.
    ///
    /// Serialized into JSONL/history and preserved by compaction, but hidden by
    /// user-facing renderers.
    MediaAnalysis {
        /// Structured delegated-media analysis payload.
        analysis: MediaAnalysis,
    },
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawObject(mut fields) = RawObject::deserialize(deserializer)?;
        let tag = raw_required::<String, D::Error>(&mut fields, "type")?;
        match tag.as_str() {
            "provider_content" => Ok(Self::ProviderContent {
                protocol: raw_required(&mut fields, "protocol")?,
                value: raw_projection(&mut fields, "value")?,
            }),
            "text" => {
                let (text, units) = raw_string_projection(&mut fields, "text")?;
                let citations = raw_present_nullable_projection(&mut fields, "citations")?;
                if String::from_utf16(&units).is_ok() {
                    Ok(Self::Text { text, citations })
                } else {
                    Ok(Self::TextJsUtf16 {
                        text,
                        utf16_code_units: units,
                        citations,
                    })
                }
            }
            "text_js_utf16" => {
                let text = raw_required(&mut fields, "text")?;
                let utf16_code_units = raw_required(&mut fields, "utf16_code_units")?;
                let citations = raw_present_nullable_projection(&mut fields, "citations")?;
                Ok(Self::TextJsUtf16 {
                    text,
                    utf16_code_units,
                    citations,
                })
            }
            "tool_use" => Ok(Self::ToolUse {
                id: raw_required(&mut fields, "id")?,
                name: raw_required(&mut fields, "name")?,
                input: raw_projection(&mut fields, "input")?,
                provider_id: raw_optional_string(&mut fields, "provider_id")?,
            }),
            "tool_result" => Ok(Self::ToolResult {
                tool_use_id: raw_required(&mut fields, "tool_use_id")?,
                content: raw_string_json_projection(&mut fields, "content")?,
                is_error: raw_optional(&mut fields, "is_error")?,
                provider_tool_use_id: raw_optional_string(&mut fields, "provider_tool_use_id")?,
                content_blocks: raw_projection_array_optional(&mut fields, "content_blocks")?,
            }),
            "thinking" => Ok(Self::Thinking {
                thinking: raw_required(&mut fields, "thinking")?,
                signature: raw_optional_string(&mut fields, "signature")?,
            }),
            "image" => Ok(Self::Image {
                source: raw_required(&mut fields, "source")?,
            }),
            "document" => Ok(Self::Document {
                source: raw_required(&mut fields, "source")?,
            }),
            "redacted_thinking" => Ok(Self::RedactedThinking {
                data: raw_required(&mut fields, "data")?,
            }),
            "server_tool_use" => Ok(Self::ServerToolUse {
                id: raw_required(&mut fields, "id")?,
                name: raw_required(&mut fields, "name")?,
                input: raw_projection_or_null(&mut fields, "input")?,
            }),
            "connector_text" => Ok(Self::ConnectorText {
                connector_text: raw_optional(&mut fields, "connector_text")?.unwrap_or_default(),
                signature: raw_optional_string(&mut fields, "signature")?,
            }),
            "advisor_tool_result" => Ok(Self::AdvisorToolResult {
                tool_use_id: raw_required(&mut fields, "tool_use_id")?,
                content: raw_projection_or_null(&mut fields, "content")?,
                is_error: raw_optional(&mut fields, "is_error")?.unwrap_or(false),
            }),
            "media_analysis" => Ok(Self::MediaAnalysis {
                analysis: raw_required(&mut fields, "analysis")?,
            }),
            _ => Err(serde::de::Error::custom(format!(
                "unknown conversation content block type {tag:?}"
            ))),
        }
    }
}

/// Structured vision-delegation analysis persisted into conversation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaAnalysis {
    /// Stable key for the user question this analysis belongs to.
    pub question_key: String,
    /// Ordered media fingerprints covered by this analysis packet.
    #[serde(default)]
    pub media_fingerprints: Vec<String>,
    /// Delegate model that produced the analysis.
    pub model: String,
    /// Prompt schema version used for the analysis request.
    pub prompt_version: u32,
    /// Time the analysis was created.
    pub created_at: std::time::SystemTime,
    /// Task-level findings relevant to the current user request.
    #[serde(default)]
    pub task_findings: Vec<String>,
    /// Per-media observations in packet order.
    #[serde(default)]
    pub media: Vec<MediaObservation>,
    /// Findings that depend on multiple media items together.
    #[serde(default)]
    pub cross_media_findings: Vec<String>,
    /// Whether the delegate reported truncation or partial output.
    #[serde(default)]
    pub truncated: bool,
}

/// One media item's delegated observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaObservation {
    /// Fingerprint of the referenced media item.
    pub fingerprint: String,
    /// Human-readable label used in the delegate output.
    pub label: String,
    /// Plain-language description of what is visible.
    pub description: String,
    /// OCR text, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<String>,
    /// Facts the delegate marked as relevant to the task.
    #[serde(default)]
    pub relevant_facts: Vec<String>,
    /// Delegate uncertainty note for this media item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty: Option<String>,
}

/// Source of a [`ContentBlock::Image`]. Serializes to Anthropic's
/// `source` wire shape (`{"type":"base64",…}` / `{"type":"url",…}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Inline base64-encoded image bytes.
    Base64 {
        /// MIME type, e.g. `image/png`.
        media_type: String,
        /// Base64-encoded image bytes (no `data:` prefix).
        data: String,
    },
    /// A remote image URL the provider fetches.
    Url {
        /// The image URL.
        url: String,
    },
}

/// Source of a [`ContentBlock::Document`]. Serializes to Anthropic's `source`
/// wire shape (`{"type":"base64","media_type":"application/pdf","data":…}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSource {
    /// Inline base64-encoded document bytes.
    Base64 {
        /// MIME type, e.g. `application/pdf`.
        media_type: String,
        /// Base64-encoded document bytes (no `data:` prefix).
        data: String,
    },
}

/// Whether a compaction was user-initiated or triggered automatically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactTrigger {
    /// User ran `/compact`.
    Manual,
    /// The token threshold triggered compaction.
    #[default]
    Auto,
}

impl CompactTrigger {
    /// The compact-metadata wire value for this trigger.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

/// Relink metadata for a preserved compacted-history segment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreservedSegment {
    /// UUID of the first preserved message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_uuid: Option<String>,
    /// UUID of the message the preserved segment follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_uuid: Option<String>,
    /// UUID of the last preserved message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_uuid: Option<String>,
}

/// Re-parenting list used to splice a preserved tail back into history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreservedMessages {
    /// UUID of the message the preserved tail follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_uuid: Option<String>,
    /// UUIDs participating in the reconstructed chain.
    pub uuids: Vec<String>,
    /// UUIDs of all preserved messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_uuids: Vec<String>,
}

/// Protocol-owned snapshot of an active session goal carried through compact
/// metadata. It mirrors the engine state without introducing a dependency
/// from `protocol` back to `engine`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactActiveGoalState {
    /// User-supplied goal condition.
    #[serde(with = "crate::types::utf16_json::raw_json_serde")]
    pub condition: Utf16JsonProjection,
    /// When the goal became active.
    #[serde(alias = "setAt")]
    pub set_at: std::time::SystemTime,
    /// Most recent stop-time evaluation reason, when available.
    #[serde(
        default,
        alias = "lastReason",
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub last_reason: Option<Utf16JsonProjection>,
    /// Stop evaluations completed while the goal was active.
    #[serde(default)]
    pub iterations: u64,
    /// Cumulative tokens when the goal was activated.
    #[serde(default, alias = "tokensAtStart")]
    pub tokens_at_start: u64,
    /// How the goal became active. Mirrors the engine's `GoalOrigin` without a
    /// `protocol` → `engine` dependency; carried so a goal that survives a
    /// compact boundary keeps reporting the origin it actually had, the way
    /// upstream's compact metadata does by dumping the whole goal object.
    #[serde(default)]
    pub origin: CompactGoalOrigin,
}

/// The `protocol`-side mirror of the engine's `GoalOrigin`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactGoalOrigin {
    /// The user set this goal in this session.
    #[default]
    User,
    /// The goal came back with a resumed session.
    Restored,
}

/// Typed metadata carried by a compact-boundary system message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactBoundaryMetadata {
    /// `'manual' | 'auto'`.
    #[serde(default)]
    pub trigger: CompactTrigger,
    /// Token count immediately before compaction.
    #[serde(default)]
    pub pre_tokens: u64,
    /// Token count after the rebuilt history is assembled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_tokens: Option<u64>,
    /// Cumulative discarded tokens across compactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_dropped_tokens: Option<u64>,
    /// Time spent compacting, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// User-context snapshot taken at compaction time.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub user_context: Option<Utf16JsonProjection>,
    /// Number of messages replaced by the summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages_summarized: Option<u32>,
    /// Whether the summary was generated in the background before being applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precomputed: Option<bool>,
    /// Deferred tools discovered before compaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_compact_discovered_tools: Vec<String>,
    /// Relink metadata for a preserved tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegment>,
    /// Complete re-splice list for a preserved tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_messages: Option<PreservedMessages>,
    /// Active `/goal` snapshot at compaction time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_goal: Option<CompactActiveGoalState>,
    /// UUID of the last pre-compact message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_parent_uuid: Option<String>,
}

impl ContentBlock {
    /// Recover the exact source input belonging to a tool call.
    pub fn projected_tool_input(
        &self,
    ) -> Result<Option<Utf16JsonProjection>, crate::types::utf16_json::Utf16JsonProjectionError>
    {
        let Self::ToolUse { input, .. } = self else {
            return Ok(None);
        };
        input.validate()?;
        Ok(Some(input.clone()))
    }

    /// Recover the exact model-facing source content for a tool result.
    pub fn projected_tool_result(
        &self,
    ) -> Result<Option<Utf16JsonProjection>, crate::types::utf16_json::Utf16JsonProjectionError>
    {
        use crate::types::utf16_json::Utf16JsonProjectionError;
        let Self::ToolResult {
            content,
            content_blocks,
            ..
        } = self
        else {
            return Ok(None);
        };
        let projection = content_blocks.as_ref().unwrap_or(content);
        projection.validate()?;
        if content_blocks.is_some() && projection.array_len("").is_none() {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "tool result content blocks must be a JSON array",
            ));
        }
        if content_blocks.is_none() && projection.string_units("").is_none() {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "tool result content must be a JSON string",
            ));
        }
        Ok(Some(projection.clone()))
    }

    /// Replace a tool result's source content while keeping its string/array
    /// representation in the canonical field.
    pub fn rebase_tool_result_projection(
        &mut self,
        source: Utf16JsonProjection,
    ) -> Result<(), crate::types::utf16_json::Utf16JsonProjectionError> {
        use crate::types::utf16_json::Utf16JsonProjectionError;
        source.validate()?;
        let Self::ToolResult {
            content,
            content_blocks,
            ..
        } = self
        else {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "result source requires a tool result block",
            ));
        };
        if source.string_units("").is_some() {
            *content = source;
            *content_blocks = None;
        } else if source.array_len("").is_some() {
            *content_blocks = Some(source);
        } else {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "tool result content must be a string or array",
            ));
        }
        Ok(())
    }

    /// Text a user-facing renderer may display from this block.
    ///
    /// Anthropic text blocks with unknown native fields stay as one raw
    /// `ProviderContent` block for replay. Their `text` member remains visible
    /// through this projection without synthesizing a second transcript block.
    #[must_use]
    pub fn visible_text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } | Self::TextJsUtf16 { text, .. } => Some(text),
            Self::ProviderContent { protocol, value }
                if protocol == "anthropic_messages" && value.string_at("/type") == Some("text") =>
            {
                value.string_at("/text")
            }
            _ => None,
        }
    }
}

/// A model-facing API system row or projection. Original transcript rows keep
/// their identity while an admitted projection changes only model input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSystemMessage {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<ApiSystemOutputConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSystemOutputConfig {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_root_string_projection",
        deserialize_with = "deserialize_optional_root_string_projection"
    )]
    pub effort: Option<Utf16JsonProjection>,
}

/// A single message in a conversation, role-tagged for serde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
// Keep compact metadata inline: boxing this public enum field would introduce
// a source-level compatibility break for every constructor and pattern match.
#[allow(clippy::large_enum_variant)]
pub enum ConversationMessage {
    /// User-authored message — always structured content blocks.
    User {
        /// Stable message identifier.
        id: MessageId,
        /// Ordered content blocks (typically text and/or tool results).
        content: Vec<ContentBlock>,
        /// `true` for synthetic/meta user messages — content the engine injects
        /// into the conversation (e.g. Stop-hook feedback) that is hidden from
        /// the user-facing UI and skipped when locating the last *real* user
        /// prompt, mirroring claude-code's `isMeta:true` (`createUserMessage`).
        /// Still part of the API stream. Defaults to `false`; with
        /// `skip_serializing_if` the wire/JSONL shape is unchanged for normal
        /// user messages.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_meta: bool,
        /// `true` when this user message contains a compacted-history summary.
        #[serde(
            default,
            rename = "isCompactSummary",
            skip_serializing_if = "std::ops::Not::not"
        )]
        is_compact_summary: bool,
        /// `true` when the message belongs in transcript history but not normal
        /// user-facing rendering.
        #[serde(
            default,
            rename = "isVisibleInTranscriptOnly",
            skip_serializing_if = "std::ops::Not::not"
        )]
        is_visible_in_transcript_only: bool,
        /// Native outer-envelope marker identifying a turn-companion user
        /// message. This is host history metadata, not provider message text.
        #[serde(
            default,
            rename = "turnCompanion",
            skip_serializing_if = "Option::is_none"
        )]
        turn_companion: Option<Option<bool>>,
        /// Native outer-envelope usage-limit annotation. It stays outside the
        /// nested `{role,content}` provider-facing message.
        #[serde(
            default,
            rename = "usageLimitNote",
            skip_serializing_if = "Option::is_none"
        )]
        usage_limit_note: Option<Option<UsageLimitNote>>,
        /// Exact task-notification or other native origin metadata. This is
        /// an outer history field and never enters provider message content.
        #[serde(
            default,
            rename = "origin",
            serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option",
            skip_serializing_if = "Option::is_none"
        )]
        origin: Option<Utf16JsonProjection>,
        /// Exact Native turn-origin tag, stored outside nested provider text.
        #[serde(
            default,
            rename = "turnOrigin",
            serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option",
            skip_serializing_if = "Option::is_none"
        )]
        turn_origin: Option<Utf16JsonProjection>,
        /// Model-facing representation of an admitted user row, when it differs
        /// from the original transcript row.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_message_override: Option<ApiSystemMessage>,
    },
    /// Assistant-authored message — may include tool-use blocks.
    Assistant {
        /// Stable message identifier.
        id: MessageId,
        /// Ordered content blocks.
        content: Vec<ContentBlock>,
        /// Provider-reported stop reason, if any.
        stop_reason: Option<String>,
        /// Exact inbound Anthropic resumable metadata. This is response/history
        /// state and is never copied into the provider request message.
        #[serde(
            default,
            rename = "resumable",
            serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option",
            skip_serializing_if = "Option::is_none"
        )]
        resumable: Option<Utf16JsonProjection>,
        /// Native admitted effort used by this successful response. Provider
        /// output cannot populate this owner-only history field.
        #[serde(
            default,
            rename = "perTurnEffort",
            serialize_with = "serialize_optional_root_string_projection",
            deserialize_with = "deserialize_optional_root_string_projection",
            skip_serializing_if = "Option::is_none"
        )]
        per_turn_effort: Option<Utf16JsonProjection>,
    },
    /// System prompt — flat string, no tool blocks.
    System {
        /// Stable message identifier.
        id: MessageId,
        /// The system prompt body, retained as exact JavaScript UTF-16 source.
        #[serde(
            serialize_with = "crate::types::utf16_json::raw_json_serde::serialize",
            deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize"
        )]
        content: Utf16JsonProjection,
        /// Structured system-message subtype, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtype: Option<String>,
        /// Explicit model-facing API system row. Transcript-only notices have
        /// no payload and remain excluded from model requests.
        #[serde(default, rename = "apiSystem", skip_serializing_if = "Option::is_none")]
        api_system: Option<ApiSystemMessage>,
        /// Typed compact-boundary metadata.
        #[serde(
            default,
            rename = "compactMetadata",
            skip_serializing_if = "Option::is_none"
        )]
        compact_metadata: Option<CompactBoundaryMetadata>,
        /// Typed query-local model-fallback notice.
        #[serde(
            default,
            rename = "modelFallback",
            skip_serializing_if = "Option::is_none"
        )]
        model_fallback: Option<ModelFallbackMetadata>,
        /// Typed refusal-fallback metadata, on `subtype ==
        /// "model_refusal_fallback"` only.
        #[serde(
            default,
            rename = "refusalFallback",
            skip_serializing_if = "Option::is_none"
        )]
        refusal_fallback: Option<RefusalFallbackMetadata>,
    },
}

impl<'de> Deserialize<'de> for ConversationMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawObject(mut fields) = RawObject::deserialize(deserializer)?;
        let role = raw_required::<String, D::Error>(&mut fields, "role")?;
        match role.as_str() {
            "user" => {
                let raw_content = fields
                    .remove("content")
                    .ok_or_else(|| serde::de::Error::missing_field("content"))?;
                Ok(Self::User {
                    id: raw_required(&mut fields, "id")?,
                    content: raw_content_blocks::<D::Error>(raw_content.as_ref())?,
                    is_meta: raw_optional(&mut fields, "is_meta")?.unwrap_or(false),
                    is_compact_summary: raw_optional(&mut fields, "isCompactSummary")?
                        .unwrap_or(false),
                    is_visible_in_transcript_only: raw_optional(
                        &mut fields,
                        "isVisibleInTranscriptOnly",
                    )?
                    .unwrap_or(false),
                    turn_companion: raw_optional(&mut fields, "turnCompanion")?,
                    usage_limit_note: raw_optional(&mut fields, "usageLimitNote")?,
                    origin: raw_optional_projection(&mut fields, "origin")?,
                    turn_origin: raw_optional_projection(&mut fields, "turnOrigin")?,
                    api_message_override: raw_optional_nullable(
                        &mut fields,
                        "api_message_override",
                    )?,
                })
            }
            "assistant" => {
                let raw_content = fields
                    .remove("content")
                    .ok_or_else(|| serde::de::Error::missing_field("content"))?;
                Ok(Self::Assistant {
                    id: raw_required(&mut fields, "id")?,
                    content: raw_content_blocks::<D::Error>(raw_content.as_ref())?,
                    stop_reason: raw_optional_string(&mut fields, "stop_reason")?,
                    resumable: raw_optional_projection(&mut fields, "resumable")?,
                    per_turn_effort: raw_optional_root_string_projection(
                        &mut fields,
                        "perTurnEffort",
                    )?,
                })
            }
            "system" => Ok(Self::System {
                id: raw_required(&mut fields, "id")?,
                content: raw_string_json_projection(&mut fields, "content")?,
                subtype: raw_optional_string(&mut fields, "subtype")?,
                api_system: raw_optional_nullable(&mut fields, "apiSystem")?,
                compact_metadata: raw_optional(&mut fields, "compactMetadata")?,
                model_fallback: raw_optional(&mut fields, "modelFallback")?,
                refusal_fallback: raw_optional(&mut fields, "refusalFallback")?,
            }),
            _ => Err(serde::de::Error::custom(format!(
                "unknown conversation message role {role:?}"
            ))),
        }
    }
}

/// The current query's serving-model transition. User model settings are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFallbackMetadata {
    /// Native trigger, such as `server_error` or `overloaded`.
    pub trigger: String,
    /// Failed serving model.
    pub original_model: String,
    /// Model serving this query after the transition.
    pub fallback_model: String,
}

impl ModelFallbackMetadata {
    /// Current SDK system envelope, independent of optional provider partial events.
    #[must_use]
    pub fn sdk_frame(
        &self,
        id: MessageId,
        session_id: crate::types::SessionId,
        content: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": "system", "subtype": "model_fallback", "uuid": id.as_uuid().to_string(),
            "trigger": self.trigger, "original_model": self.original_model,
            "fallback_model": self.fallback_model, "content": content,
            "session_id": session_id.as_uuid().to_string(),
        })
    }
}

/// The `model_refusal_fallback` system message's payload.
///
/// claude-code carries the refusal notice as a TYPED system message in the
/// conversation itself (`Dcr` / `Jbt`, `src_163219561.js`), not as loose banner
/// text: it persists into the transcript, comes back on resume, and — through
/// [`Self::retracted_message_uuids`] — lets a later hop's notice say which
/// earlier ones it superseded, which is what makes them filterable rather than
/// merely stale.
///
/// Native optional fields survive persistence even when a local producer has
/// no corresponding observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusalFallbackMetadata {
    /// What provoked the swap. `"refusal"` for the refusal cascade.
    pub trigger: String,
    /// `"retry"` — the swap is followed by a retry of the same turn.
    pub direction: String,
    /// `"session"` when the swap sticks for the rest of the session,
    /// `"local"` when it applies to this turn alone. This port's cascade
    /// persistently swaps the session model, so it reports `"session"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The model the episode started on.
    pub original_model: String,
    /// The model now serving.
    pub fallback_model: String,
    /// The provider request id of the refused call, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The API's refusal category for this hop, when the provider gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_refusal_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_refusal_explanation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saw_cyber_refusal: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neutralized_by_fork: Option<bool>,
    /// Earlier notices this one supersedes. A consumer rebuilding the
    /// conversation drops the messages named here rather than showing a swap
    /// the session has already moved past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retracted_message_uuids: Vec<String>,
    /// The user message that was refused, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_user_message_uuid: Option<String>,
    /// Host-only provider-profile companion to the previous app-state route.
    /// Resume applies it only when a notice in the route chain identifies the
    /// same selected model. The outer option distinguishes absent metadata
    /// from an explicitly profile-less route (`null`).
    #[serde(
        default,
        deserialize_with = "deserialize_present_nullable_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub previous_profile: Option<Option<String>>,
    /// Host-only provider-profile companion to the fallback model. Resume
    /// applies it only when that fallback model was selected independently
    /// from an assistant row or live route. The same outer/inner option
    /// convention distinguishes absent metadata from an explicit null.
    #[serde(
        default,
        deserialize_with = "deserialize_present_nullable_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub serving_profile: Option<Option<String>>,
    /// Host-only enqueue timestamp for the native queued notice. The JSONL
    /// exporter maps this to the outer transcript `timestamp`; it is never a
    /// refusal-metadata wire field.
    #[serde(skip)]
    pub notice_timestamp: Option<String>,
}

impl ConversationMessage {
    /// Construct a simple user message containing a single text block.
    #[must_use]
    pub fn user(id: MessageId, text: String) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::Text {
                text,
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Construct a synthetic/meta user message with a single text block —
    /// engine-injected content hidden from the user-facing UI but still part of
    /// the API stream (claude-code `createUserMessage({ …, isMeta: true })`).
    /// Used for Stop-hook feedback (`getStopHookMessage`, `utils/hooks.ts:1895`).
    #[must_use]
    pub fn user_meta(id: MessageId, text: String) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::Text {
                text,
                citations: None,
            }],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Mark a user message as a Native turn-companion message and attach its
    /// exact current usage-limit reason.
    #[must_use]
    pub fn with_turn_companion(mut self, note: UsageLimitNote) -> Self {
        if let Self::User {
            turn_companion,
            usage_limit_note,
            ..
        } = &mut self
        {
            *turn_companion = Some(Some(true));
            *usage_limit_note = Some(Some(note));
        }
        self
    }

    /// Attach exact Native origin metadata to a user message.
    #[must_use]
    pub fn with_origin(mut self, origin: Utf16JsonProjection) -> Self {
        if let Self::User {
            origin: current, ..
        } = &mut self
        {
            *current = Some(origin);
        }
        self
    }

    /// Attach the exact Native turn-origin tag to a user message.
    #[must_use]
    pub fn with_turn_origin(mut self, turn_origin: Utf16JsonProjection) -> Self {
        if let Self::User {
            turn_origin: current,
            ..
        } = &mut self
        {
            *current = Some(turn_origin);
        }
        self
    }

    /// Construct a synthetic/meta user message whose text block preserves an
    /// exact JS UTF-16 wire image when the provider request is serialized.
    #[must_use]
    pub fn user_meta_js_utf16(id: MessageId, text: String, utf16_code_units: Vec<u16>) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations: None,
            }],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Construct a synthetic/meta user message carrying one media-analysis block.
    #[must_use]
    pub fn user_media_analysis(id: MessageId, analysis: MediaAnalysis) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::MediaAnalysis { analysis }],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Construct a compact-summary user message.
    #[must_use]
    pub fn compact_summary(id: MessageId, text: String) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::Text {
                text,
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: true,
            is_visible_in_transcript_only: true,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Construct a typed compact-boundary system message.
    #[must_use]
    pub fn compact_boundary(
        id: MessageId,
        content: String,
        metadata: CompactBoundaryMetadata,
    ) -> Self {
        Self::System {
            id,
            content: content.into(),
            subtype: Some("compact_boundary".to_string()),
            api_system: None,
            compact_metadata: Some(metadata),
            model_fallback: None,
            refusal_fallback: None,
        }
    }

    /// Replace the typed metadata on a compact-boundary system message.
    ///
    /// Returns `false` for non-boundary messages and leaves them unchanged.
    pub fn set_compact_metadata(&mut self, metadata: CompactBoundaryMetadata) -> bool {
        match self {
            Self::System {
                subtype: Some(subtype),
                compact_metadata,
                ..
            } if subtype == "compact_boundary" => {
                *compact_metadata = Some(metadata);
                true
            }
            _ => false,
        }
    }

    /// Construct a user message with a leading text block (when non-empty)
    /// followed by one [`ContentBlock::Image`] per source. Used by the TUI
    /// paste→image path; with no images this is equivalent to [`Self::user`]
    /// (modulo an empty-text message carrying no blocks).
    #[must_use]
    pub fn user_with_images(id: MessageId, text: String, images: Vec<ImageSource>) -> Self {
        let mut content = Vec::with_capacity(1 + images.len());
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text,
                citations: None,
            });
        }
        for source in images {
            content.push(ContentBlock::Image { source });
        }
        Self::User {
            id,
            content,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Like [`Self::user_with_images`] but for document sources (P4a).
    #[must_use]
    pub fn user_with_documents(
        id: MessageId,
        text: String,
        documents: Vec<DocumentSource>,
    ) -> Self {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text,
                citations: None,
            });
        }
        for source in documents {
            content.push(ContentBlock::Document { source });
        }
        Self::User {
            id,
            content,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        }
    }

    /// Return the role of this message.
    #[must_use]
    pub fn role(&self) -> MessageRole {
        match self {
            Self::User { .. } => MessageRole::User,
            Self::Assistant { .. } => MessageRole::Assistant,
            Self::System { .. } => MessageRole::System,
        }
    }

    /// Return `true` for a synthetic/meta user message (claude-code
    /// `isMeta:true`). Always `false` for assistant/system messages.
    #[must_use]
    pub fn is_meta(&self) -> bool {
        matches!(self, Self::User { is_meta: true, .. })
    }

    /// Return `true` when this is a compact-summary user message.
    #[must_use]
    pub fn is_compact_summary(&self) -> bool {
        matches!(
            self,
            Self::User {
                is_compact_summary: true,
                ..
            }
        )
    }

    /// Return `true` when this message is transcript-only.
    #[must_use]
    pub fn is_visible_in_transcript_only(&self) -> bool {
        matches!(
            self,
            Self::User {
                is_visible_in_transcript_only: true,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
                ..
            }
        )
    }

    /// Return the stable identifier of this message.
    #[must_use]
    pub fn id(&self) -> MessageId {
        match self {
            Self::User { id, .. } | Self::Assistant { id, .. } | Self::System { id, .. } => *id,
        }
    }

    /// Concatenate all `Text` blocks. Returns "" if none.
    #[must_use]
    pub fn text_content(&self) -> String {
        match self {
            Self::User { content, .. } | Self::Assistant { content, .. } => content
                .iter()
                .filter_map(ContentBlock::visible_text)
                .collect::<Vec<_>>()
                .join(""),
            // This convenience API returns display text. Exact source remains
            // available on the System variant's `content` projection.
            Self::System { content, .. } => content.string_at("").unwrap_or_default().to_owned(),
        }
    }

    /// Return `true` if this is an assistant message that requested at least one tool call.
    #[must_use]
    pub fn has_tool_use(&self) -> bool {
        match self {
            Self::Assistant { content, .. } => content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
            _ => false,
        }
    }

    /// Returns references to `ToolUse` blocks; empty if not Assistant.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        match self {
            Self::Assistant { content, .. } => content
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Construct a model-facing API system row without adding a transcript
    /// notice to the ordinary system content stream.
    #[must_use]
    pub fn api_system(id: MessageId, message: ApiSystemMessage) -> Self {
        Self::System {
            id,
            content: Utf16JsonProjection::root_string(String::new(), Vec::new())
                .expect("empty text is a valid source string"),
            subtype: Some("api_system".into()),
            api_system: Some(message),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        }
    }
}

/// Project per-turn effort from successful assistant responses onto API system
/// rows while leaving the stored transcript messages untouched.
pub fn project_per_turn_effort(
    messages: Vec<ConversationMessage>,
    current: &Utf16JsonProjection,
) -> Vec<ConversationMessage> {
    current
        .string_units("")
        .expect("per-turn effort must be a root JSON string");
    let mut effort = current.clone();
    let mut levels = vec![Vec::new(); messages.len()];
    for (index, message) in messages.iter().enumerate().rev() {
        if let ConversationMessage::Assistant {
            per_turn_effort: Some(saved),
            ..
        } = message
        {
            levels[index] = saved
                .string_units("")
                .expect("stored per-turn effort must be a root JSON string");
            effort = saved.clone();
        } else {
            levels[index] = effort
                .string_units("")
                .expect("per-turn effort must be a root JSON string");
        }
    }
    // The reverse pass assigns an assistant's saved effort to its own row;
    // user rows use the nearest later assistant, or the current request at tail.
    let mut projected = Vec::with_capacity(messages.len() + 1);
    let mut active: Option<Vec<u16>> = None;
    let mut pending: Option<Utf16JsonProjection> = None;
    for (index, mut message) in messages.into_iter().enumerate() {
        if !matches!(message, ConversationMessage::User { .. }) {
            if let Some(next_effort) = pending.take() {
                active = next_effort.string_units("");
                if let ConversationMessage::System {
                    api_system: Some(payload),
                    ..
                } = &mut message
                {
                    payload.output_config = Some(ApiSystemOutputConfig {
                        effort: Some(next_effort),
                    });
                } else {
                    projected.push(ConversationMessage::api_system(
                        MessageId::new(),
                        ApiSystemMessage {
                            content: Vec::new(),
                            output_config: Some(ApiSystemOutputConfig {
                                effort: Some(next_effort),
                            }),
                        },
                    ));
                }
            }
        }
        if matches!(message, ConversationMessage::User { .. }) {
            let next_units = levels[index].clone();
            pending = (active.as_deref() != Some(next_units.as_slice())).then(|| {
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&next_units), next_units)
                    .expect("saved effort is a root JSON string")
            });
        }
        projected.push(message);
    }
    if let Some(next_effort) = pending {
        projected.push(ConversationMessage::api_system(
            MessageId::new(),
            ApiSystemMessage {
                content: Vec::new(),
                output_config: Some(ApiSystemOutputConfig {
                    effort: Some(next_effort),
                }),
            },
        ));
    }
    projected
}

/// One in-memory unit surfaced from the loader to the agent loop.
///
/// Boundary type shared by `lingxi-memory` (producer) and `lingxi-agent`
/// (consumer). Tier ordering and scoring rules live in `lingxi-memory`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    /// Absolute path the entry was loaded from.
    pub path: std::path::PathBuf,
    /// Tier the entry belongs to (`Session`, `Project`, `Team`, `User`).
    pub tier: MemoryEntryTier,
    /// Raw markdown body (frontmatter stripped, secrets redacted).
    pub body: String,
    /// Age in whole days from the load `now`. `0` for a just-written file.
    pub age_days: u64,
    /// File size in bytes (post-redaction body length).
    pub size_bytes: u64,
}

pub use crate::types::scope::MemoryEntryTier;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn projection(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).unwrap()
    }

    fn string_projection(value: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect()).unwrap()
    }

    fn decode_value<T: serde::de::DeserializeOwned>(value: Value) -> T {
        let source = serde_json::to_string(&value).unwrap();
        serde_json::from_str(&source).unwrap()
    }

    #[test]
    fn user_message_constructs() {
        let id = MessageId::new();
        let m = ConversationMessage::user(id, "hello".to_string());
        assert!(matches!(m.role(), MessageRole::User));
        assert_eq!(m.text_content(), "hello");
    }

    #[test]
    fn text_content_includes_native_anthropic_text_once() {
        let message = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: projection(serde_json::json!({
                        "type":"text",
                        "text":"visible",
                        "future_annotation":{"keep":true}
                    })),
                },
                ContentBlock::ProviderContent {
                    protocol: "openai_chat_completions".into(),
                    value: projection(serde_json::json!({"type":"text","text":"hidden"})),
                },
            ],
            stop_reason: None,

            resumable: None,
            per_turn_effort: None,
        };

        assert_eq!(message.text_content(), "visible");
    }

    #[test]
    fn message_roundtrip_json() {
        let m = ConversationMessage::user(MessageId::new(), "hi".into());
        let s = serde_json::to_string(&m).unwrap();
        let m2: ConversationMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn memory_entry_roundtrip_json() {
        let e = MemoryEntry {
            path: std::path::PathBuf::from("/tmp/LINGXI.md"),
            tier: MemoryEntryTier::Project,
            body: "hello".into(),
            age_days: 3,
            size_bytes: 5,
        };
        let s = serde_json::to_string(&e).unwrap();
        let e2: MemoryEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(e, e2);
    }

    #[test]
    fn user_with_images_appends_image_blocks_after_text() {
        let img = ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: "AAAA".to_string(),
        };
        let m = ConversationMessage::user_with_images(
            MessageId::new(),
            "look:".to_string(),
            vec![img.clone()],
        );
        match &m {
            ConversationMessage::User { content, .. } => {
                assert_eq!(content.len(), 2);
                assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "look:"));
                assert!(matches!(&content[1], ContentBlock::Image { source } if *source == img));
            }
            _ => panic!("expected user message"),
        }
        // Empty text → only the image block (no empty text block).
        let m2 = ConversationMessage::user_with_images(MessageId::new(), String::new(), vec![img]);
        match m2 {
            ConversationMessage::User { content, .. } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(content[0], ContentBlock::Image { .. }));
            }
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn image_base64_block_matches_anthropic_wire() {
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            },
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}
            })
        );
        let back: ContentBlock = decode_value(v);
        assert_eq!(back, block);
    }

    #[test]
    fn document_base64_block_matches_anthropic_wire() {
        let block = ContentBlock::Document {
            source: DocumentSource::Base64 {
                media_type: "application/pdf".to_string(),
                data: "JVBERi0=".to_string(),
            },
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "type": "document",
                "source": { "type": "base64", "media_type": "application/pdf", "data": "JVBERi0=" }
            })
        );
        let back: ContentBlock = decode_value(v);
        assert_eq!(back, block);
    }

    #[test]
    fn image_url_block_matches_anthropic_wire() {
        let block = ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://x/y.png".to_string(),
            },
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type":"image","source":{"type":"url","url":"https://x/y.png"}})
        );
    }

    #[test]
    fn conversation_message_with_image_roundtrips_jsonl() {
        let m = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/jpeg".to_string(),
                    data: "Zm9v".to_string(),
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        };
        let line = serde_json::to_string(&m).unwrap();
        // A non-meta user message must NOT serialize an `is_meta` field
        // (`skip_serializing_if`), keeping the wire/JSONL shape byte-unchanged.
        assert!(
            !line.contains("is_meta"),
            "non-meta user must omit is_meta: {line}"
        );
        let back: ConversationMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn user_meta_constructor_sets_flag_and_roundtrips() {
        let m = ConversationMessage::user_meta(MessageId::new(), "feedback".to_string());
        assert!(m.is_meta(), "user_meta must set is_meta");
        assert_eq!(m.text_content(), "feedback");
        // A plain `user` is NOT meta.
        assert!(!ConversationMessage::user(MessageId::new(), "hi".to_string()).is_meta());
        // Meta messages serialize the flag and round-trip it back.
        let line = serde_json::to_string(&m).unwrap();
        assert!(
            line.contains("\"is_meta\":true"),
            "meta user must emit is_meta: {line}"
        );
        let back: ConversationMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(back, m);
        assert!(back.is_meta());
    }

    #[test]
    fn user_native_outer_metadata_roundtrips_exactly_outside_provider_content() {
        let origin = Utf16JsonProjection::parse(
            r#"{"kind":"task-notification","producer":"session-task","detail":"\ud800"}"#,
        )
        .unwrap();
        let turn_origin = Utf16JsonProjection::root_string(
            "task_notification".into(),
            "task_notification".encode_utf16().collect(),
        )
        .unwrap();
        let message = ConversationMessage::user_meta(MessageId::new(), "notification".into())
            .with_turn_companion(UsageLimitNote::NearLimit)
            .with_origin(origin.clone())
            .with_turn_origin(turn_origin.clone());
        let line = serde_json::to_string(&message).unwrap();
        let row = Utf16JsonProjection::parse(&line).unwrap();
        assert_eq!(
            row.at_pointer("/turnCompanion").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(row.string_at("/usageLimitNote"), Some("near_limit"));
        assert_eq!(row.string_units("/origin/detail"), Some(vec![0xd800]));
        assert_eq!(row.string_at("/turnOrigin"), Some("task_notification"));
        assert!(row.at_pointer("/content/0/origin").is_none());
        let decoded: ConversationMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn user_meta_js_utf16_preserves_display_text_and_sidecar() {
        let m = ConversationMessage::user_meta_js_utf16(
            MessageId::new(),
            "A".to_string(),
            vec![0x0041, 0xD83D],
        );
        assert!(m.is_meta());
        assert_eq!(m.text_content(), "A");
        let line = serde_json::to_string(&m).unwrap();
        assert!(line.contains("\"type\":\"text_js_utf16\""), "{line}");
        assert!(line.contains("\"utf16_code_units\":[65,55357]"), "{line}");
        let back: ConversationMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn additive_compact_fields_leave_plain_user_wire_unchanged() {
        let id = MessageId::from_uuid(
            uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
        );
        let message = ConversationMessage::user(id, "hello".to_string());
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","id":"11111111-1111-1111-1111-111111111111","content":[{"type":"text","text":"hello"}]}"#
        );
    }

    #[test]
    fn additive_compact_fields_leave_plain_system_wire_unchanged() {
        let id = MessageId::from_uuid(
            uuid::Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
        );
        let message = ConversationMessage::System {
            id,
            content: "notice".into(),
            subtype: None,
            api_system: None,
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"system","id":"22222222-2222-2222-2222-222222222222","content":"notice"}"#
        );
    }

    #[test]
    fn system_message_preserves_unpaired_utf16_in_raw_json_roundtrip() {
        let message: ConversationMessage = serde_json::from_str(
            r#"{"role":"system","id":"22222222-2222-2222-2222-222222222222","content":"before\ud800after"}"#,
        )
        .unwrap();
        let ConversationMessage::System { content, .. } = &message else {
            panic!("expected system message");
        };
        let mut expected_units = "before".encode_utf16().collect::<Vec<_>>();
        expected_units.push(0xd800);
        expected_units.extend("after".encode_utf16());
        assert_eq!(content.string_units(""), Some(expected_units.clone()));

        let wire = serde_json::to_string(&message).unwrap();
        assert!(wire.contains(r#""content":"before\ud800after""#), "{wire}");
        let roundtrip: ConversationMessage = serde_json::from_str(&wire).unwrap();
        let ConversationMessage::System { content, .. } = roundtrip else {
            panic!("expected system message");
        };
        assert_eq!(content.string_units(""), Some(expected_units));
    }

    #[test]
    fn compact_summary_constructor_sets_flags_and_roundtrips() {
        let message = ConversationMessage::compact_summary(MessageId::new(), "summary".to_string());
        assert!(message.is_compact_summary());
        assert!(message.is_visible_in_transcript_only());
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value.get("isCompactSummary"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            value.get("isVisibleInTranscriptOnly"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            serde_json::from_str::<ConversationMessage>(&value.to_string()).unwrap(),
            message
        );
    }

    #[test]
    fn compact_active_goal_accepts_sdk_camel_case_fields() {
        let metadata: CompactBoundaryMetadata = serde_json::from_value(serde_json::json!({
            "trigger": "manual",
            "activeGoal": {
                "condition": "all checks pass",
                "setAt": {
                    "secs_since_epoch": 1_700_000_000_u64,
                    "nanos_since_epoch": 0_u32
                },
                "lastReason": "still working"
            }
        }))
        .expect("SDK camelCase active goal must deserialize");

        let goal = metadata.active_goal.expect("active goal");
        assert_eq!(goal.condition.string_at(""), Some("all checks pass"));
        assert_eq!(
            goal.last_reason
                .as_ref()
                .and_then(|reason| reason.string_at("")),
            Some("still working")
        );
    }

    #[test]
    fn legacy_partial_compact_metadata_deserializes_with_safe_defaults() {
        let metadata: CompactBoundaryMetadata = serde_json::from_value(serde_json::json!({
            "cumulativeDroppedTokens": 4_321,
            "preCompactDiscoveredTools": ["DeferredTool"]
        }))
        .unwrap();
        assert_eq!(metadata.trigger, CompactTrigger::Auto);
        assert_eq!(metadata.pre_tokens, 0);
        assert_eq!(metadata.cumulative_dropped_tokens, Some(4_321));
        assert_eq!(metadata.pre_compact_discovered_tools, vec!["DeferredTool"]);
    }

    #[test]
    fn assistant_message_with_tool_use_extracts_tool_calls() {
        let m = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "I'll read the file.".into(),
                    citations: None,
                },
                ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "Read".into(),
                    input: projection(serde_json::json!({"path": "/tmp/x"})),
                    provider_id: None,
                },
            ],
            stop_reason: None,

            resumable: None,
            per_turn_effort: None,
        };
        assert!(m.has_tool_use());
        assert_eq!(m.tool_calls().len(), 1);
    }

    #[test]
    fn tool_use_provider_id_skipped_when_none() {
        // Backward compat: a `None` provider_id MUST NOT appear on the wire, so
        // existing locked JSONL fixtures stay byte-identical.
        let block = ContentBlock::ToolUse {
            id: ToolUseId::from("toolu_01ABC"),
            name: "Read".into(),
            input: projection(serde_json::json!({"path": "/tmp/x"})),
            provider_id: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert!(
            v.get("provider_id").is_none(),
            "provider_id must be skipped when None, got: {v}"
        );
    }

    #[test]
    fn tool_use_provider_id_preserved_when_some() {
        let block = ContentBlock::ToolUse {
            id: ToolUseId::new(),
            name: "Read".into(),
            input: projection(serde_json::json!({})),
            provider_id: Some("toolu_01ABC".into()),
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v.get("provider_id").and_then(|x| x.as_str()),
            Some("toolu_01ABC")
        );
        // Round-trips back identically.
        let back: ContentBlock = decode_value(v);
        assert_eq!(back, block);
    }

    #[test]
    fn tool_use_legacy_json_without_provider_id_deserializes_to_none() {
        // A historical JSONL line written before this field existed must still
        // load — `#[serde(default)]` fills `provider_id: None`.
        let legacy = serde_json::json!({
            "type": "tool_use",
            "id": "00000000-0000-0000-0000-000000000000",
            "name": "Read",
            "input": {"path": "/tmp/x"}
        });
        let block: ContentBlock = decode_value(legacy);
        match block {
            ContentBlock::ToolUse { provider_id, .. } => assert_eq!(provider_id, None),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn text_citations_preserve_absence_null_and_value() {
        for (input, expected) in [
            (serde_json::json!({"type":"text","text":"x"}), None),
            (
                serde_json::json!({"type":"text","text":"x","citations":null}),
                Some(serde_json::Value::Null),
            ),
            (
                serde_json::json!({"type":"text","text":"x","citations":[]}),
                Some(serde_json::json!([])),
            ),
        ] {
            let block: ContentBlock = decode_value(input);
            let output = serde_json::to_value(&block).unwrap();
            assert_eq!(output.get("citations"), expected.as_ref());
        }
    }

    #[test]
    fn conversation_message_json_retains_mod_presence_for_resume() {
        let message = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "accepted Mod text".into(),
                    citations: Some(None),
                },
                ContentBlock::ToolResult {
                    tool_use_id: ToolUseId::from("toolu_01ABC"),
                    content: string_projection("done"),
                    is_error: None,
                    provider_tool_use_id: None,
                    content_blocks: None,
                },
            ],
            stop_reason: None,

            resumable: None,
            per_turn_effort: None,
        };
        let stored = serde_json::to_value(&message).unwrap();
        assert!(stored["content"][0].get("citations").is_some());
        assert_eq!(stored["content"][0]["citations"], serde_json::Value::Null);
        assert!(stored["content"][1].get("is_error").is_none());

        let resumed: ConversationMessage = serde_json::from_str(&stored.to_string()).unwrap();
        assert_eq!(resumed, message);
    }

    #[test]
    fn tool_result_is_error_preserves_omission_and_explicit_false() {
        let mut input = serde_json::json!({
            "type":"tool_result",
            "tool_use_id":"toolu_01ABC",
            "content":"ok"
        });
        let absent: ContentBlock = decode_value(input.clone());
        assert_eq!(
            absent,
            ContentBlock::ToolResult {
                tool_use_id: ToolUseId::from("toolu_01ABC"),
                content: string_projection("ok"),
                is_error: None,
                provider_tool_use_id: None,
                content_blocks: None,
            }
        );
        assert!(serde_json::to_value(absent)
            .unwrap()
            .get("is_error")
            .is_none());

        input["is_error"] = serde_json::Value::Bool(false);
        let explicit_false: ContentBlock = decode_value(input.clone());
        assert!(matches!(
            explicit_false,
            ContentBlock::ToolResult {
                is_error: Some(false),
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(explicit_false).unwrap()["is_error"],
            false
        );

        input["is_error"] = serde_json::Value::Bool(true);
        let explicit_true: ContentBlock = decode_value(input.clone());
        assert!(matches!(
            explicit_true,
            ContentBlock::ToolResult {
                is_error: Some(true),
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(explicit_true).unwrap()["is_error"],
            true
        );

        input["is_error"] = serde_json::Value::Null;
        let source = serde_json::to_string(&input).unwrap();
        assert!(serde_json::from_str::<ContentBlock>(&source).is_err());
    }

    #[test]
    fn tool_result_provider_tool_use_id_skipped_when_none_preserved_when_some() {
        let none_block = ContentBlock::ToolResult {
            tool_use_id: ToolUseId::from("toolu_01ABC"),
            content: string_projection("ok"),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        };
        let v = serde_json::to_value(&none_block).unwrap();
        assert!(v.get("provider_tool_use_id").is_none());

        let some_block = ContentBlock::ToolResult {
            tool_use_id: ToolUseId::new(),
            content: string_projection("ok"),
            is_error: Some(false),
            provider_tool_use_id: Some("toolu_01ABC".into()),
            content_blocks: None,
        };
        let v = serde_json::to_value(&some_block).unwrap();
        assert_eq!(
            v.get("provider_tool_use_id").and_then(|x| x.as_str()),
            Some("toolu_01ABC")
        );
    }

    #[test]
    fn tool_use_canonical_id_serializes_as_bare_provider_string() {
        // Byte parity with claude-code: the tool_use `id` is the canonical
        // provider string and NO `provider_id` sidecar appears on the wire.
        let block = ContentBlock::ToolUse {
            id: ToolUseId::from("toolu_01ABC"),
            name: "Read".into(),
            input: projection(serde_json::json!({})),
            provider_id: None,
        };
        let s = serde_json::to_string(&block).unwrap();
        assert_eq!(
            s,
            r#"{"type":"tool_use","id":"toolu_01ABC","name":"Read","input":{}}"#
        );
    }

    #[test]
    fn tool_result_canonical_id_serializes_as_bare_provider_string() {
        let block = ContentBlock::ToolResult {
            tool_use_id: ToolUseId::from("toolu_01ABC"),
            content: string_projection("ok"),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v.get("tool_use_id").and_then(|x| x.as_str()),
            Some("toolu_01ABC")
        );
        assert!(v.get("provider_tool_use_id").is_none());
    }

    // ── Low-frequency server-side blocks (resume/replay byte parity) ──────────

    #[test]
    fn redacted_thinking_round_trips_with_wire_tag() {
        let block = ContentBlock::RedactedThinking {
            data: "enc==".into(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(json, r#"{"type":"redacted_thinking","data":"enc=="}"#);
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn server_tool_use_round_trips_with_wire_tag() {
        let block = ContentBlock::ServerToolUse {
            id: "srvtoolu_01".into(),
            name: "web_search".into(),
            input: projection(serde_json::json!({"query": "rust"})),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(
            json,
            r#"{"type":"server_tool_use","id":"srvtoolu_01","name":"web_search","input":{"query":"rust"}}"#
        );
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn connector_text_round_trips_with_wire_tag() {
        let block = ContentBlock::ConnectorText {
            connector_text: "hi".into(),
            signature: Some("sig".into()),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(
            json,
            r#"{"type":"connector_text","connector_text":"hi","signature":"sig"}"#
        );
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
        // signature omitted when None
        let no_sig = ContentBlock::ConnectorText {
            connector_text: "hi".into(),
            signature: None,
        };
        assert_eq!(
            serde_json::to_string(&no_sig).unwrap(),
            r#"{"type":"connector_text","connector_text":"hi"}"#
        );
    }

    #[test]
    fn advisor_tool_result_round_trips_with_wire_tag() {
        let block = ContentBlock::AdvisorToolResult {
            tool_use_id: "srvtoolu_01".into(),
            content: projection(serde_json::json!([{"type": "text", "text": "ok"}])),
            is_error: false,
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(
            json,
            r#"{"type":"advisor_tool_result","tool_use_id":"srvtoolu_01","content":[{"type":"text","text":"ok"}],"is_error":false}"#
        );
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn media_analysis_block_round_trips_with_wire_tag() {
        let analysis = MediaAnalysis {
            question_key: "msg-123".into(),
            media_fingerprints: vec!["fp-a".into(), "fp-b".into()],
            model: "deepseek-flash".into(),
            prompt_version: 1,
            created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            task_findings: vec!["figure shows a chart".into()],
            media: vec![MediaObservation {
                fingerprint: "fp-a".into(),
                label: "image_1".into(),
                description: "A line chart with two series.".into(),
                ocr: Some("Q3 revenue".into()),
                relevant_facts: vec!["trend rises after May".into()],
                uncertainty: Some("legend is partially obscured".into()),
            }],
            cross_media_findings: vec!["image_2 provides zoomed-in detail".into()],
            truncated: false,
        };
        let block = ContentBlock::MediaAnalysis {
            analysis: analysis.clone(),
        };
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(value["type"], "media_analysis");
        let back: ContentBlock = decode_value(value);
        assert_eq!(back, block);
    }

    #[test]
    fn media_analysis_message_round_trips_and_is_meta() {
        let message = ConversationMessage::user_media_analysis(
            MessageId::new(),
            MediaAnalysis {
                question_key: "msg-123".into(),
                media_fingerprints: vec!["fp-a".into()],
                model: "deepseek-flash".into(),
                prompt_version: 1,
                created_at: std::time::UNIX_EPOCH,
                task_findings: vec!["contains a receipt".into()],
                media: vec![MediaObservation {
                    fingerprint: "fp-a".into(),
                    label: "receipt".into(),
                    description: "Small printed receipt.".into(),
                    ocr: None,
                    relevant_facts: vec!["total is visible".into()],
                    uncertainty: None,
                }],
                cross_media_findings: Vec::new(),
                truncated: false,
            },
        );
        assert!(message.is_meta());
        let line = serde_json::to_string(&message).unwrap();
        let back: ConversationMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn raw_tagged_content_ingress_keeps_deep_tool_input_and_tag_order() {
        let mut deep = r#""\ud800""#.to_owned();
        for _ in 0..384 {
            deep = format!("[{deep}]");
        }
        let source = format!(
            r#"{{"input":{{"nested":{deep},"\ud800":"\udc00"}},"id":"11111111-1111-4111-8111-111111111111","name":"Bash","type":"tool_use"}}"#
        );

        let block: ContentBlock = serde_json::from_str(&source).unwrap();
        let ContentBlock::ToolUse { input, .. } = &block else {
            panic!("expected a tool-use block");
        };
        assert_eq!(input.at_pointer("/nested").unwrap().array_len(), Some(1));
        assert_eq!(
            input
                .at_pointer("")
                .unwrap()
                .get_property_units(&[0xd800])
                .unwrap()
                .code_units(),
            Some([0xdc00].as_slice())
        );

        let encoded = serde_json::to_string(&block).unwrap();
        assert!(encoded.contains(r#""input":{"nested":["#), "{encoded}");
        assert!(encoded.contains(r#""\ud800":"\udc00""#), "{encoded}");
        let decoded: ContentBlock = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, block);
    }

    #[test]
    fn raw_role_ingress_preserves_tool_result_lone_utf16_as_json_string() {
        let source = r#"{"content":[{"content":"error: \ud800","tool_use_id":"11111111-1111-4111-8111-111111111111","type":"tool_result"}],"id":"22222222-2222-4222-8222-222222222222","role":"user"}"#;
        let message: ConversationMessage = serde_json::from_str(source).unwrap();
        let ConversationMessage::User { content, .. } = &message else {
            panic!("expected a user message");
        };
        let ContentBlock::ToolResult { content, .. } = &content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(
            content.string_units(""),
            Some(vec![101, 114, 114, 111, 114, 58, 32, 0xd800])
        );

        let encoded = serde_json::to_string(&message).unwrap();
        assert!(
            encoded.contains(r#""content":"error: \ud800""#),
            "{encoded}"
        );
        let decoded: ConversationMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn assistant_resumable_preserves_absence_null_and_exact_json_source() {
        let base = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: Vec::new(),
            stop_reason: Some("max_tokens".into()),
            resumable: None,
            per_turn_effort: None,
        };
        let absent = serde_json::to_string(&base).expect("assistant serializes");
        assert!(!absent.contains("resumable"));
        let decoded: ConversationMessage =
            serde_json::from_str(&absent).expect("assistant deserializes");
        assert!(matches!(
            decoded,
            ConversationMessage::Assistant {
                resumable: None,
                per_turn_effort: None,
                ..
            }
        ));

        for source in ["null", "false", r#"{"text":"\ud800"}"#] {
            let projection = Utf16JsonProjection::parse(source).expect("valid exact JSON");
            let message = ConversationMessage::Assistant {
                id: MessageId::new(),
                content: Vec::new(),
                stop_reason: Some("max_tokens".into()),
                resumable: Some(projection.clone()),
                per_turn_effort: None,
            };
            let serialized = serde_json::to_string(&message).expect("assistant serializes");
            assert!(serialized.contains(&format!(r#""resumable":{source}"#)));
            let decoded: ConversationMessage =
                serde_json::from_str(&serialized).expect("assistant deserializes");
            let ConversationMessage::Assistant {
                resumable: Some(actual),
                ..
            } = decoded
            else {
                panic!("resumable field is present")
            };
            assert_eq!(actual, projection);
        }
    }

    #[test]
    fn per_turn_effort_keeps_exact_source_through_history_projection() {
        let effort = Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        let response = ConversationMessage::Assistant {
            id: MessageId::new(),
            content: Vec::new(),
            stop_reason: Some("end_turn".into()),
            resumable: None,
            per_turn_effort: Some(effort.clone()),
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(encoded.contains(r#""perTurnEffort":"\ud800""#), "{encoded}");
        let decoded: ConversationMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, response);

        let projected = project_per_turn_effort(
            vec![
                ConversationMessage::user(MessageId::new(), "prompt".into()),
                response,
            ],
            &Utf16JsonProjection::root_string("medium".into(), "medium".encode_utf16().collect())
                .unwrap(),
        );
        let api_row = projected
            .iter()
            .find_map(|message| match message {
                ConversationMessage::System {
                    api_system: Some(payload),
                    ..
                } => Some(payload),
                _ => None,
            })
            .expect("effort is projected into an API system row");
        let actual = api_row
            .output_config
            .as_ref()
            .and_then(|config| config.effort.as_ref())
            .expect("the exact effort source remains present");
        assert_eq!(actual.string_units(""), Some(vec![0xd800]));
        assert!(serde_json::to_string(api_row)
            .unwrap()
            .contains(r#""effort":"\ud800""#));
    }

    #[test]
    fn compact_goal_and_user_context_keep_exact_utf16_source() {
        let exact = |units: Vec<u16>| {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                .expect("source text is valid JSON")
        };
        let condition_units = vec![b'g' as u16, 0xd800];
        let reason_units = vec![b'r' as u16, 0xdfff];
        let context_units = vec![b'c' as u16, 0xd800];
        let metadata = CompactBoundaryMetadata {
            user_context: Some(exact(context_units.clone())),
            active_goal: Some(CompactActiveGoalState {
                condition: exact(condition_units.clone()),
                set_at: std::time::SystemTime::UNIX_EPOCH,
                last_reason: Some(exact(reason_units.clone())),
                iterations: 3,
                tokens_at_start: 11,
                origin: CompactGoalOrigin::User,
            }),
            ..Default::default()
        };

        let serialized = serde_json::to_string(&metadata).expect("serialize compact metadata");
        let restored: CompactBoundaryMetadata =
            serde_json::from_str(&serialized).expect("restore compact metadata");
        let goal = restored.active_goal.expect("active goal remains present");
        assert_eq!(goal.condition.string_units(""), Some(condition_units));
        assert_eq!(
            goal.last_reason.unwrap().string_units(""),
            Some(reason_units)
        );
        assert_eq!(
            restored.user_context.unwrap().string_units(""),
            Some(context_units)
        );
    }
}
