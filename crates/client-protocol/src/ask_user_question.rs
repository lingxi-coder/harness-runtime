//! Transport-neutral DTOs for an interactive `AskUserQuestion` round-trip.

use serde::{Deserialize, Serialize};

/// One selectable answer shown by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskOptionDto {
    /// Short display label.
    pub label: String,
    /// Explanation of the option's effect.
    pub description: String,
    /// Optional focused-option preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One question in an interactive questionnaire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskQuestionDto {
    /// Complete question text and answer-map key.
    pub question: String,
    /// Short section label.
    pub header: String,
    /// Declared answer options. Clients synthesize the free-text `Other` row.
    pub options: Vec<AskOptionDto>,
    /// Whether more than one option may be selected.
    pub multi_select: bool,
}

/// One outbound interactive `AskUserQuestion` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AskUserQuestionRequestDto {
    /// Connection-scoped correlator echoed by the answer/cancel command.
    pub request_id: u64,
    /// Questions in display order.
    pub questions: Vec<AskQuestionDto>,
    /// Idle auto-continue window in seconds; `None` means wait indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}
