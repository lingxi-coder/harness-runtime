//! Shared boundary values for the internal core.
//!
//! IDs, messages, effects, transport DTOs and capability flags are shared by
//! the conversation state and the host capability interfaces.
//!
//! See spec §3 (D16 Shared protocol boundary).

#![forbid(unsafe_code)]

pub mod capabilities;
pub mod effect_result;
pub mod effects;
pub mod ids;
pub mod iso8601;
pub mod js_utf16;
pub mod mcp_name;
pub mod message_size;
pub mod messages;
pub mod scope;
pub mod secret;
pub mod transport;

// Re-exports for ergonomics.
pub use capabilities::{FileSystemCapabilities, PlatformCapabilities};
pub use effect_result::{EffectError, EffectErrorKind, EffectResult};
pub use effects::Effect;
pub use ids::{
    AgentId, HookId, McpConnectionId, MessageId, PluginId, PrefetchId, RequestId, SessionId,
    SnapshotId, ToolUseId,
};
pub use mcp_name::normalize_name_for_mcp;
pub use message_size::text_byte_size;
pub use messages::{
    is_nested_media_value, CompactActiveGoalState, CompactBoundaryMetadata, CompactGoalOrigin,
    CompactTrigger, ContentBlock, ConversationMessage, DocumentSource, ImageSource, MediaAnalysis,
    MediaObservation, MemoryEntry, MessageRole, PreservedMessages, PreservedSegment,
    RefusalFallbackMetadata,
};
pub use scope::{MemoryEntryTier, Scope, SettingsScope, WritableScope};
pub use secret::{
    RedactableContent, Secret, SecretKindDto, SecureStorageData, SecureStorageMetadata,
};
pub use transport::{HttpMethod, HttpRequest, HttpResponse, SseEvent};
