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

use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
    pub label: Utf16JsonProjection,
    /// Explanation of what choosing this option means.
    pub description: Utf16JsonProjection,
    /// Optional preview content rendered when this option is focused.
    pub preview: Option<Utf16JsonProjection>,
}

impl AskOption {
    /// Convenience constructor (label + description, no preview).
    #[must_use]
    pub fn new(
        label: impl Into<Utf16JsonProjection>,
        description: impl Into<Utf16JsonProjection>,
    ) -> Self {
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
    pub question: Utf16JsonProjection,
    /// Very short chip/tag label (≤ 12 chars).
    pub header: Utf16JsonProjection,
    /// 2-4 options; mutually exclusive unless `multi_select`.
    pub options: Vec<AskOption>,
    /// Allow multiple selections.
    pub multi_select: bool,
}

/// Join `labels` into the single answer string for a `multiSelect` question
/// (`", "`-separated, in the given order). A single-select answer is just the
/// one label, so callers pass a one-element slice.
#[must_use]
pub fn join_answer_labels(labels: &[Utf16JsonProjection]) -> Utf16JsonProjection {
    let separator = ASK_MULTI_JOIN.encode_utf16().collect::<Vec<_>>();
    let mut units = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            units.extend_from_slice(&separator);
        }
        units.extend(
            label
                .string_units("")
                .expect("AskUserQuestion labels are root strings"),
        );
    }
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("joined AskUserQuestion labels form a valid root string")
}

/// One in-flight `AskUserQuestion` round-trip between the tool and the TUI.
///
/// Constructed by the host resolver and sent over the app channel. The TUI
/// fills `resp_tx` with ordered `(question, answer)` pairs when the user
/// submits, or drops it unsent on cancel/skip (which the resolver maps
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
    /// One-shot reply channel — the TUI sends ordered `(question, answer)` pairs;
    /// multi-select labels are `", "`-joined while retaining exact UTF-16 units.
    pub resp_tx: oneshot::Sender<Vec<(Utf16JsonProjection, Utf16JsonProjection)>>,
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
        assert_eq!(
            join_answer_labels(&[Utf16JsonProjection::from("Alpha")]).string_at(""),
            Some("Alpha")
        );
    }

    #[test]
    fn join_multi_labels_comma_space() {
        let labels = vec![
            Utf16JsonProjection::from("A"),
            Utf16JsonProjection::from("B"),
            Utf16JsonProjection::from("C"),
        ];
        assert_eq!(join_answer_labels(&labels).string_at(""), Some("A, B, C"));
    }

    #[test]
    fn join_empty_is_empty() {
        assert_eq!(join_answer_labels(&[]).string_at(""), Some(""));
    }

    #[test]
    fn option_new_has_no_preview() {
        let o = AskOption::new("L", "D");
        assert_eq!(o.label.string_at(""), Some("L"));
        assert_eq!(o.description.string_at(""), Some("D"));
        assert_eq!(o.preview, None);
    }

    #[test]
    fn answer_join_preserves_unpaired_utf16_units() {
        let units = vec![b'A' as u16, 0xd800, b'B' as u16];
        let label =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone())
                .unwrap();
        let joined = join_answer_labels(&[label, Utf16JsonProjection::from("Next")]);
        let mut expected = units;
        expected.extend(", Next".encode_utf16());
        assert_eq!(joined.string_units(""), Some(expected));
    }
}
