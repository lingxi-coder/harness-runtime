//! Shared protocol types for `LingXi` Core engine.
//!
//! This crate owns the boundary types (`Effect`, `EffectResult`, `EffectError`,
//! IDs, message DTOs, transport DTOs, capability flags) consumed by both
//! `core` and `platform-api`. Both depend on this crate to avoid a
//! cyclic dependency.
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
// NOTE: re-exports for other modules are added by each module's task (4-7) once
// the underlying types exist.
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
