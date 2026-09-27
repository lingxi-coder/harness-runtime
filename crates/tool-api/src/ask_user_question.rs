//! Backend-neutral `AskUserQuestion` prompt bridge types.
//!
//! Mirrors the permission-request bridge: a pure-data description of the
//! multiple-choice prompt plus a one-shot reply channel the TUI fills when the
//! user resolves the widget. The host resolver (an
//! `AskUserQuestionResolver` living in `tool-ui`) constructs an
//! [`AskUserQuestionExchange`], sends it to the TUI app, and awaits the answer
//! map on `resp_tx` — exactly the round-trip
//! `permission_bridge::PermissionExchange` performs for a permission prompt.
//!
//! These are backend-neutral so both the `tool-ui` producer and the `tui`
//! renderer can share them without either depending on the other. The engine
//! wiring that converts a `tool_ui::Question` into an [`AskQuestion`] and
//! installs the bridging resolver is the remaining step (see the crate-level
//! residuals); this module only defines the shared shapes + the answer
//! join contract.

use std::collections::HashMap;

use tokio::sync::oneshot;

/// The middot separator (`·`, U+00B7) used between joined multi-select labels
/// and inside the auto-continue countdown line — byte-locked to the oracle
/// (`AskUserQuestion` renderer: `"auto-continue in ",i,"s · any key to stay"`).
pub const ASK_MIDDOT: &str = "·";

/// How multiple selected labels are joined into a single answer string for a
/// `multiSelect` question. Matches the `tool-ui` resolver contract
/// (`", "`-joined labels in option order).
pub const ASK_MULTI_JOIN: &str = ", ";

/// One selectable option in an [`AskQuestion`] (the render-facing subset of
/// `tool_ui::QuestionOption`: the `label` + `description` the widget shows; the
/// optional `preview` is carried for the focused-option preview pane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOption {
    /// Display text the user selects (concise, 1-5 words).
    pub label: String,
    /// Explanation of what choosing this option means.
    pub description: String,
    /// Optional preview content rendered when this option is focused.
    pub preview: Option<String>,
}

impl AskOption {
    /// Convenience constructor (label + description, no preview).
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            preview: None,
        }
    }
}

/// One question the widget walks the user through (the render-facing subset of
/// `tool_ui::Question`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskQuestion {
    /// The complete question text (also the answer-map key).
    pub question: String,
    /// Very short chip/tag label (≤ 12 chars).
    pub header: String,
    /// 2-4 options; mutually exclusive unless `multi_select`.
    pub options: Vec<AskOption>,
    /// Allow multiple selections.
    pub multi_select: bool,
}

/// Join `labels` into the single answer string for a `multiSelect` question
/// (`", "`-separated, in the given order). A single-select answer is just the
/// one label, so callers pass a one-element slice.
#[must_use]
pub fn join_answer_labels(labels: &[String]) -> String {
    labels.join(ASK_MULTI_JOIN)
}

/// One in-flight `AskUserQuestion` round-trip between the tool and the TUI.
///
/// Constructed by the host resolver and sent over the app channel. The TUI
/// fills `resp_tx` with the answer map (question text → chosen label(s)) when
/// the user submits, or drops it unsent on cancel/skip (which the resolver maps
/// to an error the way a dropped permission `resp_tx` maps to `Deny`).
#[derive(Debug)]
pub struct AskUserQuestionExchange {
    /// The 1-4 questions to walk the user through.
    pub questions: Vec<AskQuestion>,
    /// The idle window before auto-continue, in whole seconds; `None` ⇒
    /// `askUserQuestionTimeout=never` (no countdown, block on the user). A
    /// duration variant (`60s`/`5m`/`10m`) arms the "auto-continue in Ns"
    /// countdown.
    pub timeout_secs: Option<u64>,
    /// One-shot reply channel — the TUI sends the answer map (question →
    /// chosen label, multi-select labels `", "`-joined) when the user submits.
    pub resp_tx: oneshot::Sender<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middot_and_join_are_byte_locked() {
        assert_eq!(ASK_MIDDOT, "·");
        assert_eq!(ASK_MULTI_JOIN, ", ");
    }

    #[test]
    fn join_single_label_is_the_label() {
        assert_eq!(join_answer_labels(&["Alpha".to_string()]), "Alpha");
    }

    #[test]
    fn join_multi_labels_comma_space() {
        let labels = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert_eq!(join_answer_labels(&labels), "A, B, C");
    }

    #[test]
    fn join_empty_is_empty() {
        assert_eq!(join_answer_labels(&[]), "");
    }

    #[test]
    fn option_new_has_no_preview() {
        let o = AskOption::new("L", "D");
        assert_eq!(o.label, "L");
        assert_eq!(o.description, "D");
        assert_eq!(o.preview, None);
    }
}
