//! Explicit inputs for a headless session. Hosts resolve argv and environment.
pub use super::session::SessionStart;
use crate::desktop::DesktopConfig;
pub use lingxi_core::types::utf16_json::Utf16JsonProjection;
pub use lingxi_core::types::ImageSource;
pub use permission::PermissionMode;

pub struct HeadlessConfig {
    pub desktop: DesktopConfig,
    pub options: HeadlessOptions,
    pub session_start: SessionStart,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Ndjson,
    StreamJson,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

#[derive(Clone, Debug)]
pub struct HeadlessOptions {
    pub prompt: Option<String>,
    pub prompt_images: Vec<ImageSource>,
    pub output_format: OutputFormat,
    pub input_format: InputFormat,
    pub verbose: bool,
    pub explicit_print: bool,
    pub replay_user_messages: bool,
    pub include_partial_messages: bool,
    pub include_hook_events: bool,
    pub forward_subagent_text: bool,
    pub thinking_display: Option<String>,
    pub settings: Option<String>,
    pub betas: Option<Vec<String>>,
    pub max_budget_usd: Option<f64>,
    pub json_schema: Option<Utf16JsonProjection>,
    pub prompt_suggestions: bool,
    pub max_structured_output_retries: i64,
    pub permission_prompts_none: bool,
    pub rewind_files: Option<String>,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            prompt: None,
            prompt_images: Vec::new(),
            output_format: OutputFormat::Text,
            input_format: InputFormat::Text,
            verbose: false,
            explicit_print: true,
            replay_user_messages: false,
            include_partial_messages: false,
            include_hook_events: false,
            forward_subagent_text: false,
            thinking_display: None,
            settings: None,
            betas: None,
            max_budget_usd: None,
            json_schema: None,
            prompt_suggestions: false,
            max_structured_output_retries:
                super::structured_output::DEFAULT_MAX_STRUCTURED_OUTPUT_RETRIES,
            permission_prompts_none: false,
            rewind_files: None,
        }
    }
}

impl HeadlessOptions {
    /// Protocol combination diagnostics from the frozen 2.1.293 print entry.
    pub fn validation_error(&self) -> Option<&'static str> {
        if self.rewind_files.is_some() {
            return None;
        }
        if self.input_format == InputFormat::StreamJson {
            if self.output_format != OutputFormat::StreamJson {
                return Some(
                    "Error: --input-format=stream-json requires output-format=stream-json.",
                );
            }
            if !self.explicit_print {
                return Some("Error: --input-format=stream-json requires --print.");
            }
        }
        if self.replay_user_messages
            && (self.input_format != InputFormat::StreamJson
                || self.output_format != OutputFormat::StreamJson)
        {
            return Some(
                "Error: --replay-user-messages requires both --input-format=stream-json and --output-format=stream-json.",
            );
        }
        if self.include_partial_messages
            && !(self.explicit_print && self.output_format == OutputFormat::StreamJson)
        {
            return Some(
                "Error: --include-partial-messages requires --print and --output-format=stream-json.",
            );
        }
        if self.output_format == OutputFormat::StreamJson && self.explicit_print && !self.verbose {
            return Some(
                "Error: When using --print, --output-format=stream-json requires --verbose",
            );
        }
        None
    }

    pub fn prompt_suggestions_enabled(&self) -> bool {
        self.prompt_suggestions
    }
}
