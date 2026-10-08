//! Session-owned remote UI capabilities; the caller declares its own answers.
use crate::types::utf16_json::Utf16JsonProjection;

/// Native control requests a UI client can answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModRemoteUiAnswer {
    Copy,
    PromptRead,
    PromptFill,
    PromptSuggest,
    ReadSelection,
}

impl ModRemoteUiAnswer {
    #[must_use]
    pub const fn subtype(self) -> &'static str {
        match self {
            Self::Copy => "ui_copy",
            Self::PromptRead => "ui_prompt_read",
            Self::PromptFill => "ui_prompt_fill",
            Self::PromptSuggest => "ui_prompt_suggest",
            Self::ReadSelection => "ui_read_selection",
        }
    }
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ui_copy" => Self::Copy,
            "ui_prompt_read" => Self::PromptRead,
            "ui_prompt_fill" => Self::PromptFill,
            "ui_prompt_suggest" => Self::PromptSuggest,
            "ui_read_selection" => Self::ReadSelection,
            _ => return None,
        })
    }
}

/// One attached client's declared callback capabilities.
#[derive(Clone, Debug)]
pub struct ModRemoteUiClient {
    pub client_id: String,
    pub surface: String,
    pub answers: Vec<ModRemoteUiAnswer>,
}

/// The connection owner delivers requests and owns their cancellation.
#[async_trait::async_trait]
pub trait ModRemoteUiHost: Send + Sync {
    /// Repeated attach replaces a given answer list; absence retains it.
    fn attach(&self, client: &str, surface: &str, answers: Option<Vec<ModRemoteUiAnswer>>);
    fn detach(&self, client: &str);
    fn clients(&self, answer: ModRemoteUiAnswer) -> Vec<ModRemoteUiClient>;
    /// None is the native unanswered/invalid-response fallback.
    async fn request(&self, request: Utf16JsonProjection) -> Option<Utf16JsonProjection>;
}
