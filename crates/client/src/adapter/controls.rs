//! Shared conversation-control conversions for bridge and mobile transports.

use crate::protocol::controls::{
    ControlDisabledReasonDto, ConversationControlsDto, NativeProactivityControlStateDto,
    PermissionControlStateDto, PermissionModeOptionDto, ReasoningBudgetRangeDto,
    ReasoningControlSpecDto, ReasoningControlStateDto, ReasoningOptionDto, ReasoningSelectionDto,
};

/// Lower the host-authored Native proactivity snapshot into the optional
/// conversation control. Hosts that do not expose the feature keep the field
/// absent; an available opt-in gate is enough to publish the control while
/// the session remains opted out.
#[must_use]
pub fn lower_native_proactivity_control_state(
    snapshot: lingxi_core::host::NativeProactivityStatusSnapshot,
) -> Option<NativeProactivityControlStateDto> {
    (snapshot.opted_in || snapshot.opt_in_available).then_some(NativeProactivityControlStateDto {
        opted_in: snapshot.opted_in,
        selector_gate: snapshot.selector_gate,
        selector_gate_ever_on: snapshot.selector_gate_ever_on,
        attachments_enabled: snapshot.attachments_enabled,
        opt_in_available: snapshot.opt_in_available,
        level: snapshot.level,
    })
}

fn lower_reasoning_selection(
    selection: &lingxi_core::host::ReasoningSelection,
) -> ReasoningSelectionDto {
    match selection {
        lingxi_core::host::ReasoningSelection::Automatic => ReasoningSelectionDto::Automatic,
        lingxi_core::host::ReasoningSelection::Disabled => ReasoningSelectionDto::Disabled,
        lingxi_core::host::ReasoningSelection::Enabled => ReasoningSelectionDto::Enabled,
        lingxi_core::host::ReasoningSelection::Level { id } => {
            ReasoningSelectionDto::Level { id: id.clone() }
        }
        lingxi_core::host::ReasoningSelection::TokenBudget { tokens } => {
            ReasoningSelectionDto::TokenBudget { tokens: *tokens }
        }
    }
}

/// Lower the exact route-level reasoning contract used by request validation.
#[must_use]
pub fn lower_reasoning_control_spec(
    spec: &lingxi_core::host::ReasoningControlSpec,
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
) -> lingxi_core::host::ReasoningSelection {
    match selection {
        ReasoningSelectionDto::Automatic => lingxi_core::host::ReasoningSelection::Automatic,
        ReasoningSelectionDto::Disabled => lingxi_core::host::ReasoningSelection::Disabled,
        ReasoningSelectionDto::Enabled => lingxi_core::host::ReasoningSelection::Enabled,
        ReasoningSelectionDto::Level { id } => lingxi_core::host::ReasoningSelection::Level { id },
        ReasoningSelectionDto::TokenBudget { tokens } => {
            lingxi_core::host::ReasoningSelection::TokenBudget { tokens }
        }
    }
}

/// Lower the authoritative conversation controls for client transports.
#[must_use]
pub fn lower_conversation_controls(
    controls: lingxi_core::host::ConversationControls,
    proactivity: lingxi_core::host::NativeProactivityStatusSnapshot,
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
        proactivity: lower_native_proactivity_control_state(proactivity),
    }
}

#[cfg(test)]
mod tests {
    use super::lower_native_proactivity_control_state;
    use crate::protocol::controls::NativeProactivityControlStateDto;

    #[test]
    fn native_proactivity_control_is_absent_when_host_gates_do_not_expose_it() {
        assert_eq!(
            lower_native_proactivity_control_state(
                lingxi_core::host::NativeProactivityStatusSnapshot::default()
            ),
            None
        );
    }

    #[test]
    fn native_proactivity_control_preserves_live_host_snapshot() {
        let snapshot = lingxi_core::host::NativeProactivityStatusSnapshot {
            opted_in: false,
            opt_in_available: true,
            selector_gate: false,
            selector_gate_ever_on: false,
            attachments_enabled: true,
            level: "default".to_owned(),
        };
        assert_eq!(
            lower_native_proactivity_control_state(snapshot),
            Some(NativeProactivityControlStateDto {
                opted_in: false,
                selector_gate: false,
                selector_gate_ever_on: false,
                attachments_enabled: true,
                opt_in_available: true,
                level: "default".to_owned(),
            })
        );
    }
}
