//! Source-native tool wire definition shared by Host and `llm-runtime`.

use crate::types::utf16_json::Utf16JsonProjection;

/// Host-owned tool declaration before conversion to a provider SDK payload.
///
/// The schema stays in the exact source projection until `llm-runtime` hands
/// it to the SDK's raw JSON payload type. This DTO carries no execution or
/// registry behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceToolWireDefinition {
    /// Canonical tool name.
    pub name: String,
    /// Model-facing tool description.
    pub description: Utf16JsonProjection,
    /// Exact JSON Schema advertised to the provider.
    pub input_schema: Utf16JsonProjection,
    /// Provider strict-schema flag.
    pub strict: bool,
    /// Native deferred-tool marker.
    pub defer_loading: bool,
}
