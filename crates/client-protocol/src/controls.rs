//! Provider-neutral conversation control DTOs.
//!
//! These DTOs carry the engine-authored permission + reasoning controls
//! snapshot for native clients. Clients render the returned state and
//! capabilities directly rather than inferring provider/model rules locally.

use serde::{Deserialize, Serialize};

/// One disabled/unavailable reason emitted by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ControlDisabledReasonDto {
    /// Stable machine-readable reason code.
    pub code: String,
    /// Optional user-facing explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One provider-neutral reasoning selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasoningSelectionDto {
    /// Use the provider/model default by omitting an override.
    Automatic,
    /// Explicitly disable reasoning when the provider supports it.
    Disabled,
    /// Explicitly enable reasoning when the provider exposes a boolean toggle.
    Enabled,
    /// Select one discrete effort/tier id published by the provider.
    Level {
        /// Provider-defined level id (for example `"high"`).
        id: String,
    },
    /// Select a provider-defined reasoning token budget.
    TokenBudget {
        /// Requested reasoning token budget.
        tokens: u64,
    },
}

/// One selectable reasoning option surfaced by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ReasoningOptionDto {
    /// The provider-neutral selection the engine can encode.
    pub selection: ReasoningSelectionDto,
    /// Whether this option may be saved as the default for future sessions.
    pub persistable: bool,
}

/// Official budget bounds for models that expose token-budget reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ReasoningBudgetRangeDto {
    /// Minimum accepted token budget.
    pub min_tokens: u64,
    /// Maximum accepted token budget.
    pub max_tokens: u64,
}

/// Capability description for the active model's reasoning controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ReasoningControlSpecDto {
    /// Selectable reasoning options the engine can encode for this model.
    pub options: Vec<ReasoningOptionDto>,
    /// Optional token-budget bounds, when the model supports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_range: Option<ReasoningBudgetRangeDto>,
    /// Provider/model default used when the requested selection is automatic.
    pub provider_default: ReasoningSelectionDto,
    /// Whether the model requires reasoning to remain enabled.
    pub forced_reasoning: bool,
    /// Whether the control may be edited at all.
    pub editable: bool,
    /// Optional reason when the control is not editable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<ControlDisabledReasonDto>,
}

/// Authoritative state for the active conversation's reasoning controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ReasoningControlStateDto {
    /// The user-requested selection stored for the conversation.
    pub requested: ReasoningSelectionDto,
    /// The effective selection applied by the engine for the active model.
    pub effective: ReasoningSelectionDto,
    /// Capability description for the active model.
    pub spec: ReasoningControlSpecDto,
}

/// Availability metadata for one permission mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PermissionModeOptionDto {
    /// Permission-mode id (`default`, `acceptEdits`, ...).
    pub mode: String,
    /// Whether the engine will currently accept the mode.
    pub available: bool,
    /// Optional reason when the mode is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<ControlDisabledReasonDto>,
}

/// Authoritative state for the active conversation's permission controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PermissionControlStateDto {
    /// The mode the user requested for the conversation.
    pub requested: String,
    /// The effective mode after engine-side validation/auto resolution.
    pub effective: String,
    /// Engine-authored availability for every known permission mode.
    pub options: Vec<PermissionModeOptionDto>,
}

/// Full conversation-controls snapshot authored by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ConversationControlsDto {
    /// Provider-qualified active model reference for this snapshot.
    pub qualified_model: String,
    /// Authoritative permission state for the active conversation.
    pub permission: PermissionControlStateDto,
    /// Authoritative reasoning state + capabilities for the active model.
    pub reasoning: ReasoningControlStateDto,
}
