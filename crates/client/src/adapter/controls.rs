//! Shared conversation-control conversions for bridge and mobile transports.

use crate::protocol::controls::{
    ControlDisabledReasonDto, ConversationControlsDto, PermissionControlStateDto,
    PermissionModeOptionDto, ReasoningBudgetRangeDto, ReasoningControlSpecDto,
    ReasoningControlStateDto, ReasoningOptionDto, ReasoningSelectionDto,
};

fn lower_reasoning_selection(
    selection: &platform_api::ReasoningSelection,
) -> ReasoningSelectionDto {
    match selection {
        platform_api::ReasoningSelection::Automatic => ReasoningSelectionDto::Automatic,
        platform_api::ReasoningSelection::Disabled => ReasoningSelectionDto::Disabled,
        platform_api::ReasoningSelection::Enabled => ReasoningSelectionDto::Enabled,
        platform_api::ReasoningSelection::Level { id } => {
            ReasoningSelectionDto::Level { id: id.clone() }
        }
        platform_api::ReasoningSelection::TokenBudget { tokens } => {
            ReasoningSelectionDto::TokenBudget { tokens: *tokens }
        }
    }
}

/// Lower the exact route-level reasoning contract used by request validation.
#[must_use]
pub fn lower_reasoning_control_spec(
    spec: &platform_api::ReasoningControlSpec,
) -> ReasoningControlSpecDto {
    ReasoningControlSpecDto {
        options: spec
            .available
            .iter()
            .map(|selection| ReasoningOptionDto {
                selection: lower_reasoning_selection(selection),
                persistable: spec.selections_persistable,
            })
            .collect(),
        budget_range: spec
            .budget_range
            .as_ref()
            .map(|range| ReasoningBudgetRangeDto {
                min_tokens: u64::from(range.min_tokens),
                max_tokens: u64::from(range.max_tokens),
            }),
        provider_default: lower_reasoning_selection(&spec.provider_default),
        forced_reasoning: spec.forced,
        editable: spec.modifiable,
        disabled_reason: spec
            .disabled_reason
            .as_ref()
            .map(|code| ControlDisabledReasonDto {
                code: code.clone(),
                message: None,
            }),
    }
}

/// Decode a client selection.
#[must_use]
pub fn decode_reasoning_selection(
    selection: ReasoningSelectionDto,
) -> platform_api::ReasoningSelection {
    match selection {
        ReasoningSelectionDto::Automatic => platform_api::ReasoningSelection::Automatic,
        ReasoningSelectionDto::Disabled => platform_api::ReasoningSelection::Disabled,
        ReasoningSelectionDto::Enabled => platform_api::ReasoningSelection::Enabled,
        ReasoningSelectionDto::Level { id } => platform_api::ReasoningSelection::Level { id },
        ReasoningSelectionDto::TokenBudget { tokens } => {
            platform_api::ReasoningSelection::TokenBudget { tokens }
        }
    }
}

/// Lower the authoritative conversation controls for client transports.
#[must_use]
pub fn lower_conversation_controls(
    controls: platform_api::ConversationControls,
) -> ConversationControlsDto {
    ConversationControlsDto {
        qualified_model: controls.model_reference,
        permission: PermissionControlStateDto {
            requested: controls.permission.requested,
            effective: controls.permission.effective,
            options: controls
                .permission
                .modes
                .into_iter()
                .map(|mode| PermissionModeOptionDto {
                    mode: mode.mode,
                    available: mode.available,
                    disabled_reason: mode.disabled_reason.map(|code| ControlDisabledReasonDto {
                        code,
                        message: None,
                    }),
                })
                .collect(),
        },
        reasoning: ReasoningControlStateDto {
            requested: lower_reasoning_selection(&controls.requested_reasoning_selection),
            effective: lower_reasoning_selection(&controls.effective_reasoning_selection),
            spec: lower_reasoning_control_spec(&controls.reasoning_spec),
        },
    }
}
