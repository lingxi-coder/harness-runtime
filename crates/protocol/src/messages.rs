//! Conversation message DTOs.
//!
//! Mirrors `Anthropic`'s `Message` content-block model but is API-neutral —
//! provider adapters in `lingxi-api-client` map their native shapes to these.

use crate::ids::{MessageId, ToolUseId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Whether a raw nested tool-result value represents provider-visible media.
///
/// This is shared by the delegation collector and the final provider media cap
/// so both paths retain the same set of image/document blocks.
#[must_use]
pub fn is_nested_media_value(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
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

/// One block of structured content within a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Provider-native replay data. Never executed as an application tool.
    ProviderContent {
        /// Communication protocol which owns this payload.
        protocol: String,
        /// Unmodified provider content, including signed/encrypted reasoning.
        value: Value,
    },
    /// Plain UTF-8 text.
    Text {
        /// The text body.
        text: String,
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
    },
    /// A tool invocation requested by the assistant.
    ToolUse {
        /// Identifier echoed back in the matching `ToolResult`.
        id: ToolUseId,
        /// Tool name (e.g. `"Read"`, `"Bash"`).
        name: String,
        /// Tool-specific structured input.
        input: Value,
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
        content: String,
        /// Whether the tool reported failure.
        is_error: bool,
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
        content_blocks: Option<Vec<serde_json::Value>>,
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
        #[serde(default)]
        input: Value,
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
        content: Value,
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
    pub condition: String,
    /// When the goal became active.
    #[serde(alias = "setAt")]
    pub set_at: std::time::SystemTime,
    /// Most recent stop-time evaluation reason, when available.
    #[serde(default, alias = "lastReason")]
    pub last_reason: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
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

/// A single message in a conversation, role-tagged for serde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    },
    /// Assistant-authored message — may include tool-use blocks.
    Assistant {
        /// Stable message identifier.
        id: MessageId,
        /// Ordered content blocks.
        content: Vec<ContentBlock>,
        /// Provider-reported stop reason, if any.
        stop_reason: Option<String>,
    },
    /// System prompt — flat string, no tool blocks.
    System {
        /// Stable message identifier.
        id: MessageId,
        /// The system prompt body.
        content: String,
        /// Structured system-message subtype, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtype: Option<String>,
        /// Typed compact-boundary metadata.
        #[serde(
            default,
            rename = "compactMetadata",
            skip_serializing_if = "Option::is_none"
        )]
        compact_metadata: Option<CompactBoundaryMetadata>,
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

/// The `model_refusal_fallback` system message's payload.
///
/// claude-code carries the refusal notice as a TYPED system message in the
/// conversation itself (`Dcr` / `Jbt`, `src_163219561.js`), not as loose banner
/// text: it persists into the transcript, comes back on resume, and — through
/// [`Self::retracted_message_uuids`] — lets a later hop's notice say which
/// earlier ones it superseded, which is what makes them filterable rather than
/// merely stale.
///
/// ## What this port does NOT carry, and why
///
/// Upstream's object also has `apiRefusalExplanation` and `sawCyberRefusal`.
/// Neither has a source here — the cascade records a category but no
/// explanation, and nothing classifies a cyber refusal — so they are omitted
/// rather than serialized as permanent `null`s that read like a wired field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Earlier notices this one supersedes. A consumer rebuilding the
    /// conversation drops the messages named here rather than showing a swap
    /// the session has already moved past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retracted_message_uuids: Vec<String>,
    /// The user message that was refused, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_user_message_uuid: Option<String>,
}

impl ConversationMessage {
    /// Construct a simple user message containing a single text block.
    #[must_use]
    pub fn user(id: MessageId, text: String) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::Text { text }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
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
            content: vec![ContentBlock::Text { text }],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
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
            }],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
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
        }
    }

    /// Construct a compact-summary user message.
    #[must_use]
    pub fn compact_summary(id: MessageId, text: String) -> Self {
        Self::User {
            id,
            content: vec![ContentBlock::Text { text }],
            is_meta: false,
            is_compact_summary: true,
            is_visible_in_transcript_only: true,
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
            content,
            subtype: Some("compact_boundary".to_string()),
            compact_metadata: Some(metadata),
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
            content.push(ContentBlock::Text { text });
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
            content.push(ContentBlock::Text { text });
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
                .filter_map(|b| match b {
                    ContentBlock::Text { text } | ContentBlock::TextJsUtf16 { text, .. } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            Self::System { content, .. } => content.clone(),
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

pub use crate::scope::MemoryEntryTier;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_constructs() {
        let id = MessageId::new();
        let m = ConversationMessage::user(id, "hello".to_string());
        assert!(matches!(m.role(), MessageRole::User));
        assert_eq!(m.text_content(), "hello");
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
                assert!(matches!(&content[0], ContentBlock::Text { text } if text == "look:"));
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
        let back: ContentBlock = serde_json::from_value(v).unwrap();
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
        let back: ContentBlock = serde_json::from_value(v).unwrap();
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
            content: "notice".to_string(),
            subtype: None,
            compact_metadata: None,
            refusal_fallback: None,
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"system","id":"22222222-2222-2222-2222-222222222222","content":"notice"}"#
        );
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
            serde_json::from_value::<ConversationMessage>(value).unwrap(),
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
        assert_eq!(goal.condition, "all checks pass");
        assert_eq!(goal.last_reason.as_deref(), Some("still working"));
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
                },
                ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "Read".into(),
                    input: serde_json::json!({"path": "/tmp/x"}),
                    provider_id: None,
                },
            ],
            stop_reason: None,
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
            input: serde_json::json!({"path": "/tmp/x"}),
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
            input: serde_json::json!({}),
            provider_id: Some("toolu_01ABC".into()),
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(
            v.get("provider_id").and_then(|x| x.as_str()),
            Some("toolu_01ABC")
        );
        // Round-trips back identically.
        let back: ContentBlock = serde_json::from_value(v).unwrap();
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
        let block: ContentBlock = serde_json::from_value(legacy).unwrap();
        match block {
            ContentBlock::ToolUse { provider_id, .. } => assert_eq!(provider_id, None),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_provider_tool_use_id_skipped_when_none_preserved_when_some() {
        let none_block = ContentBlock::ToolResult {
            tool_use_id: ToolUseId::from("toolu_01ABC"),
            content: "ok".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        };
        let v = serde_json::to_value(&none_block).unwrap();
        assert!(v.get("provider_tool_use_id").is_none());

        let some_block = ContentBlock::ToolResult {
            tool_use_id: ToolUseId::new(),
            content: "ok".into(),
            is_error: false,
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
            input: serde_json::json!({}),
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
            content: "ok".into(),
            is_error: false,
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
            input: serde_json::json!({"query": "rust"}),
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
            content: serde_json::json!([{"type": "text", "text": "ok"}]),
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
        let back: ContentBlock = serde_json::from_value(value).unwrap();
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
}
