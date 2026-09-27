//! Ultracode session reminder state machine.
//!
//! Both streaming and batched turn paths should call [`UltracodeState::advance`]
//! at the single prompt-submit seam. Keeping the transition logic here avoids
//! subtly different cadence and keyword behavior between transports.

use serde::{Deserialize, Serialize};

/// Default number of non-meta user turns between sparse reminders.
pub const DEFAULT_ULTRACODE_CADENCE: u32 = 10;
/// Environment override used by Claude Code's `bop()` resolution.
pub const ULTRACODE_CADENCE_ENV: &str = "CLAUDE_CODE_JUNIPER_SUNDIAL";

const ENTER_TEXT: &str = "Ultracode is on. Use the Workflow tool on every substantive task and keep orchestration aligned with the user's request.";
const SPARSE_TEXT: &str = "Ultracode is still on. Continue using Workflow for substantive tasks.";
const EXIT_TEXT: &str =
    "Ultracode is off. Return to normal tool selection; Workflow remains manual-only.";
const KEYWORD_TEXT: &str = "The user explicitly requested ultracode. Treat this as manual opt-in to workflow orchestration for this request.";
const REMINDER_PREFIX: &str = "<system-reminder>\n";
const REMINDER_SUFFIX: &str = "\n</system-reminder>";

/// Runtime inputs that decide whether Ultracode is active.
#[derive(Debug, Clone, Copy)]
pub struct UltracodeGate<'a> {
    /// Resolved model id. Empty means the model has not resolved yet.
    pub model: &'a str,
    /// Resolved effort.
    pub effort: Option<&'a str>,
    /// Whether Workflow is available and not disabled by policy.
    pub workflows_enabled: bool,
}

impl UltracodeGate<'_> {
    /// Claude's `EK(model, effort, workflowsOn)` projection.
    #[must_use]
    pub fn active(self) -> bool {
        !self.model.trim().is_empty() && self.effort == Some("xhigh") && self.workflows_enabled
    }
}

/// Cadence/keyword configuration frozen for a session.
#[derive(Debug, Clone, Copy, Default)]
pub struct UltracodeConfig {
    /// Statsig/feature-flag cadence, if configured.
    pub feature_flag_cadence: Option<u32>,
    /// Product experiment/default gate cadence, if configured.
    pub product_default_cadence: Option<u32>,
    /// Whether a literal keyword emits the sibling request attachment.
    pub keyword_trigger_enabled: bool,
}

impl UltracodeConfig {
    /// Resolve cadence as env → feature flag → product default → 10.
    #[must_use]
    pub fn cadence(self) -> u32 {
        std::env::var(ULTRACODE_CADENCE_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .filter(|value| *value > 0)
            .or_else(|| self.feature_flag_cadence.filter(|value| *value > 0))
            .or_else(|| self.product_default_cadence.filter(|value| *value > 0))
            .unwrap_or(DEFAULT_ULTRACODE_CADENCE)
    }
}

/// Attachment identity emitted into the model input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UltracodeAttachmentKind {
    /// False→true transition.
    UltraEffortEnter,
    /// Periodic still-active reminder.
    UltraEffortSparse,
    /// True→false transition.
    UltraEffortExit,
    /// Literal `ultracode` keyword opt-in.
    WorkflowKeywordRequest,
}

/// One bounded meta-context attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UltracodeAttachment {
    /// Stable attachment kind.
    pub kind: UltracodeAttachmentKind,
    /// `"full"` on enter, `"sparse"` while active, absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_type: Option<String>,
    /// Model-facing reminder body.
    pub text: String,
}

/// Persisted per-session Ultracode state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UltracodeState {
    /// Whether the preceding turn was inside Ultracode.
    pub active: bool,
    /// Non-meta turns since the last enter/sparse reminder.
    pub non_meta_turns_since_reminder: u32,
}

impl UltracodeState {
    /// Reconstruct the runtime state from one persisted user transcript entry.
    ///
    /// Real prompts precede their generated reminder in the JSONL chain. A
    /// non-meta prompt therefore advances the active cadence first, while the
    /// following enter/sparse/exit reminder resets or flips it.
    pub fn observe_persisted_user_message(&mut self, text: &str, is_meta: bool) {
        if !is_meta {
            if self.active {
                self.non_meta_turns_since_reminder =
                    self.non_meta_turns_since_reminder.saturating_add(1);
            }
            return;
        }

        let body = text
            .strip_prefix(REMINDER_PREFIX)
            .and_then(|text| text.strip_suffix(REMINDER_SUFFIX))
            .unwrap_or(text);
        if body == ENTER_TEXT || body == SPARSE_TEXT {
            self.active = true;
            self.non_meta_turns_since_reminder = 0;
        } else if body == EXIT_TEXT {
            self.active = false;
            self.non_meta_turns_since_reminder = 0;
        }
    }

    /// Build all attachments for a submitted prompt and advance the state.
    ///
    /// This is intentionally transport-neutral: streaming and batched adapters
    /// pass the same inputs and serialize the returned attachments identically.
    pub fn advance(
        &mut self,
        gate: UltracodeGate<'_>,
        config: UltracodeConfig,
        user_prompt: &str,
        is_meta_turn: bool,
    ) -> Vec<UltracodeAttachment> {
        let next_active = gate.active();
        let mut attachments = Vec::with_capacity(2);

        match (self.active, next_active) {
            (false, true) => {
                attachments.push(UltracodeAttachment {
                    kind: UltracodeAttachmentKind::UltraEffortEnter,
                    reminder_type: Some("full".into()),
                    text: ENTER_TEXT.into(),
                });
                self.non_meta_turns_since_reminder = 0;
            }
            (true, false) => {
                attachments.push(UltracodeAttachment {
                    kind: UltracodeAttachmentKind::UltraEffortExit,
                    reminder_type: None,
                    text: EXIT_TEXT.into(),
                });
                self.non_meta_turns_since_reminder = 0;
            }
            (true, true) if !is_meta_turn => {
                self.non_meta_turns_since_reminder =
                    self.non_meta_turns_since_reminder.saturating_add(1);
                if self.non_meta_turns_since_reminder >= config.cadence() {
                    attachments.push(UltracodeAttachment {
                        kind: UltracodeAttachmentKind::UltraEffortSparse,
                        reminder_type: Some("sparse".into()),
                        text: SPARSE_TEXT.into(),
                    });
                    self.non_meta_turns_since_reminder = 0;
                }
            }
            _ => {}
        }
        self.active = next_active;

        if config.keyword_trigger_enabled && contains_literal_ultracode(user_prompt) {
            attachments.push(UltracodeAttachment {
                kind: UltracodeAttachmentKind::WorkflowKeywordRequest,
                reminder_type: None,
                text: KEYWORD_TEXT.into(),
            });
        }

        attachments
    }
}

fn contains_literal_ultracode(prompt: &str) -> bool {
    prompt
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| token.eq_ignore_ascii_case("ultracode"))
}

/// Shared Workflow availability predicate used by both the tool and EK gate.
#[must_use]
pub fn workflows_enabled(managed_disabled: bool) -> bool {
    !managed_disabled
        && !platform_api::env::is_env_truthy(
            std::env::var("LINGXI_DISABLE_WORKFLOWS").ok().as_deref(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn active_gate() -> UltracodeGate<'static> {
        UltracodeGate {
            model: "claude-opus-5",
            effort: Some("xhigh"),
            workflows_enabled: true,
        }
    }

    #[test]
    fn enter_sparse_exit_and_meta_turns_share_one_state_machine() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(ULTRACODE_CADENCE_ENV);
        let config = UltracodeConfig {
            feature_flag_cadence: Some(2),
            ..Default::default()
        };
        let mut state = UltracodeState::default();
        let enter = state.advance(active_gate(), config, "work", false);
        assert_eq!(enter[0].kind, UltracodeAttachmentKind::UltraEffortEnter);
        assert_eq!(enter[0].reminder_type.as_deref(), Some("full"));

        assert!(state
            .advance(active_gate(), config, "/status", true)
            .is_empty());
        assert!(state
            .advance(active_gate(), config, "one", false)
            .is_empty());
        let sparse = state.advance(active_gate(), config, "two", false);
        assert_eq!(sparse[0].kind, UltracodeAttachmentKind::UltraEffortSparse);

        let exit = state.advance(
            UltracodeGate {
                effort: Some("high"),
                ..active_gate()
            },
            config,
            "done",
            false,
        );
        assert_eq!(exit[0].kind, UltracodeAttachmentKind::UltraEffortExit);
    }

    #[test]
    fn transcript_replay_restores_active_cadence_without_extra_metadata_rows() {
        let mut state = UltracodeState::default();
        state.observe_persisted_user_message("turn zero", false);
        state.observe_persisted_user_message(
            &format!("{REMINDER_PREFIX}{ENTER_TEXT}{REMINDER_SUFFIX}"),
            true,
        );
        state.observe_persisted_user_message("one", false);
        state.observe_persisted_user_message("two", false);
        assert!(state.active);
        assert_eq!(state.non_meta_turns_since_reminder, 2);

        state.observe_persisted_user_message(
            &format!("{REMINDER_PREFIX}{SPARSE_TEXT}{REMINDER_SUFFIX}"),
            true,
        );
        assert_eq!(state.non_meta_turns_since_reminder, 0);
    }

    #[test]
    fn cadence_precedence_is_env_then_flag_then_product_then_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = UltracodeConfig {
            feature_flag_cadence: Some(7),
            product_default_cadence: Some(8),
            keyword_trigger_enabled: false,
        };
        std::env::set_var(ULTRACODE_CADENCE_ENV, "6");
        assert_eq!(config.cadence(), 6);
        std::env::remove_var(ULTRACODE_CADENCE_ENV);
        assert_eq!(config.cadence(), 7);
        assert_eq!(
            UltracodeConfig {
                feature_flag_cadence: None,
                ..config
            }
            .cadence(),
            8
        );
        assert_eq!(UltracodeConfig::default().cadence(), 10);
    }

    #[test]
    fn keyword_attachment_requires_literal_token_and_setting() {
        let gate = UltracodeGate {
            model: "claude-opus-5",
            effort: Some("high"),
            workflows_enabled: true,
        };
        let mut state = UltracodeState::default();
        let on = UltracodeConfig {
            keyword_trigger_enabled: true,
            ..Default::default()
        };
        assert_eq!(
            state.advance(gate, on, "use ULTRACODE please", false)[0].kind,
            UltracodeAttachmentKind::WorkflowKeywordRequest
        );
        assert!(state
            .advance(gate, on, "ultracoder is different", false)
            .is_empty());
        assert!(state
            .advance(gate, UltracodeConfig::default(), "ultracode", false)
            .is_empty());
    }
}
