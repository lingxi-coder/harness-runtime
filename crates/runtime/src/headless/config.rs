//! Explicit inputs for a headless session. Hosts resolve argv and environment.
pub use super::session::SessionStart;
use crate::desktop::DesktopConfig;
pub use lingxi_core::types::utf16_json::Utf16JsonProjection;
pub use lingxi_core::types::ImageSource;
pub use permission::PermissionMode;

/// Complete runtime and invocation configuration for one headless session.
pub struct HeadlessConfig {
    /// Desktop runtime settings and dependencies used to assemble the session.
    pub desktop: DesktopConfig,
    /// Output, input, prompt, and execution options for this invocation.
    pub options: HeadlessOptions,
    /// Whether to start new, resume a selected session, or continue the latest one.
    pub session_start: SessionStart,
}

/// Wire format used to write the assistant's output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Print user-facing text.
    #[default]
    Text,
    /// Write one JSON response object.
    Json,
    /// Write newline-delimited JSON output records.
    Ndjson,
    /// Write the streaming JSON event protocol.
    StreamJson,
}

/// Format accepted from the headless input stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputFormat {
    /// Treat the supplied prompt as plain text.
    #[default]
    Text,
    /// Read newline-delimited JSON input frames.
    StreamJson,
}

/// Per-invocation options for headless input, output, and request construction.
#[derive(Clone, Debug)]
pub struct HeadlessOptions {
    /// Optional text prompt supplied to the session.
    pub prompt: Option<String>,
    /// Images attached to the initial prompt.
    pub prompt_images: Vec<ImageSource>,
    /// Format used for assistant output.
    pub output_format: OutputFormat,
    /// Format read from the input stream.
    pub input_format: InputFormat,
    /// Use the batched provider request path for a one-shot prompt.
    pub no_stream: bool,
    /// Include diagnostic details in output formats that support them.
    pub verbose: bool,
    /// Whether this invocation was explicitly requested in print mode.
    pub explicit_print: bool,
    /// Replay accepted user messages as stream-json output frames.
    pub replay_user_messages: bool,
    /// Include partial assistant messages in stream-json output.
    pub include_partial_messages: bool,
    /// Include hook event frames in stream-json output.
    pub include_hook_events: bool,
    /// Forward subagent text into the headless output stream.
    pub forward_subagent_text: bool,
    /// Optional Native thinking-display selection for the request.
    pub thinking_display: Option<String>,
    /// Optional settings string supplied by the headless caller.
    pub settings: Option<String>,
    /// Optional provider beta values requested for this invocation.
    pub betas: Option<Vec<String>>,
    /// Optional maximum spend in US dollars for this invocation.
    pub max_budget_usd: Option<f64>,
    /// Optional source-preserving JSON schema for structured output.
    pub json_schema: Option<Utf16JsonProjection>,
    /// Whether to include prompt suggestions in eligible output.
    pub prompt_suggestions: bool,
    /// Maximum number of retries for structured-output validation failures.
    pub max_structured_output_retries: i64,
    /// Disable interactive permission prompts for this invocation.
    pub permission_prompts_none: bool,
    /// Optional user-message ID at which to restore file state when resuming.
    pub rewind_files: Option<String>,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            prompt: None,
            prompt_images: Vec::new(),
            output_format: OutputFormat::Text,
            input_format: InputFormat::Text,
            no_stream: false,
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
        if self.prompt_suggestions
            && !(self.explicit_print && self.output_format == OutputFormat::StreamJson)
        {
            return Some(
                "Error: --prompt-suggestions requires --print and --output-format=stream-json (prompt_suggestion messages are only surfaced in stream-json output).",
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

    /// Returns whether this invocation requests prompt suggestions.
    pub fn prompt_suggestions_enabled(&self) -> bool {
        self.prompt_suggestions
    }
}
