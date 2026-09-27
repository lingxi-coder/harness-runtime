//! `AskUserQuestionTool` — asks the user one or more multiple-choice questions.
//!
//! Ported to the real claude-code multi-question contract
//! (`AskUserQuestionTool/AskUserQuestionTool.tsx:14-79`):
//! - `questions: [{ question, header, options:[{label, description, preview?}],
//!   multiSelect }]` — 1-4 questions, each with 2-4 options.
//! - `header` is a short chip label (max `ASK_USER_QUESTION_TOOL_CHIP_WIDTH`).
//! - Uniqueness refine (`UNIQUENESS_REFINE`): question texts must be unique and
//!   option labels must be unique within each question.
//! - Output `{ questions, answers, annotations? }` where `answers` maps each
//!   question text to the chosen label (multi-select answers are comma-joined),
//!   matching the TS `call` return shape.
//!
//! Auto-continue policy (oracle 2.1.201): AskUserQuestion does NOT auto-continue
//! by default. The setting `askUserQuestionTimeout: enum["60s","5m","10m",
//! "never"]` (settings schema `askUserQuestionTimeout:E.enum([...]).catch(void 0)`,
//! getter `Yye()`/`uSn("askUserQuestionTimeout")`, /config "Input & controls"
//! row "Question auto-continue timeout") controls the *idle* window before an
//! unanswered prompt auto-continues with the answers selected so far. The
//! DEFAULT is `never` — auto-continue only runs when explicitly set to
//! `60s`/`5m`/`10m`; otherwise the tool BLOCKS on the user. See
//! [`AskUserQuestionTimeout`].
//!
//! Resolvers:
//! - [`FirstOptionResolver`] — a hermetic test helper
//!   ([`AskUserQuestionTool::new`]). It synthesizes each question's first option
//!   label so formatting tests stay deterministic.
//! - [`DefaultTimeoutResolver`] — the **production** resolver wired at
//!   `lib.rs` registration. Without a live prompt broker it returns
//!   [`ToolError::InteractionRequired`] rather than inventing an answer.
//! - [`TuiBridgeResolver`] — a session-scoped interactive resolver for the
//!   mounted Ratatui bottom-pane questionnaire view. It preserves the same
//!   timeout semantics but routes the question set through the live UI.
//! - Production hosts may still override via [`AskUserQuestionTool::with_resolver`].
//!
//! The live TUI renders the countdown, collects real option/custom-text
//! selections, and resolves the bridge. The remaining parity item is the
//! `tengu_ask_user_question_afk_auto_advance` / `_accepted` / `_rejected` /
//! `_skipped` telemetry.
//!
//! Fidelity notes / divergences (see Batch 5 spec):
//! - TS `checkPermissions` uses `behavior:'ask'` ("Answer questions?"), and
//!   upstream RENDERS that ask as the questionnaire itself
//!   (`AskUserQuestionPermissionDialog`, answers returned as
//!   `updatedInput.answers`). This port draws the questionnaire in `call` via
//!   [`AskUserQuestionResolver`], so the permission result is `Allow` — an ask
//!   here would stack a generic approval dialog in front of the question card.
//!   `requires_user_interaction()` stays true, and the tool is deliberately
//!   absent from the auto-mode safe allowlist, so nothing else that bit governs
//!   changes. See `check_permissions` for the full reasoning.
//! - The HTML-preview validation (`validateHtmlPreview`, gated on
//!   `getQuestionPreviewFormat()==='html'`) remains out of scope here:
//!   `preview` is still a passthrough string. The synthetic "Other" answer path
//!   round-trips the user's text verbatim through `answers`; it does not add a
//!   model-facing interpretation of that text.
//!
//! no-truncation: bounded structured output (`{questions, answers, annotations?}`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Map, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    ASK_USER_QUESTION_COMPLETED, ASK_USER_QUESTION_FAILED, ASK_USER_QUESTION_STARTED,
};
use telemetry::AnalyticsBus;
use tool_api::ask_user_question::{AskOption, AskQuestion, AskUserQuestionExchange};

use tokio::sync::mpsc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

// -- Wire identifier locks ---------------------------------------------------

/// Tool name byte-lock (`prompt.ts:3`).
pub const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";
/// Synthetic option label auto-provided by the interactive UI.
pub const ASK_USER_QUESTION_OTHER_LABEL: &str = "Other";
/// Maximum number of questions (`inputSchema` `.max(4)`).
pub const MAX_ASK_QUESTIONS: usize = 4;
/// Minimum number of questions (`inputSchema` `.min(1)`).
pub const MIN_ASK_QUESTIONS: usize = 1;
/// Maximum number of options per question (`questionSchema` `.max(4)`).
pub const MAX_ASK_OPTIONS: usize = 4;
/// Minimum number of options per question (`questionSchema` `.min(2)`).
pub const MIN_ASK_OPTIONS: usize = 2;
/// Header chip width — `ASK_USER_QUESTION_TOOL_CHIP_WIDTH` (`prompt.ts:5`).
pub const ASK_USER_QUESTION_TOOL_CHIP_WIDTH: usize = 12;
/// Header display guidance (chip width). Claude documents this in the field
/// description but does not hard-validate it.
pub const MAX_ASK_HEADER_LEN: usize = ASK_USER_QUESTION_TOOL_CHIP_WIDTH;

/// LingXi-only locks retained for the system-tools parity fixture. TS uses
/// free-form `z.string()` for both `label` and `question` (no length cap), so
/// these are NOT enforced in `validate_input`; they exist only as exported
/// constants matching `parity/fixtures/system_tools.json`.
pub const MAX_ASK_LABEL_LEN: usize = 60;
/// LingXi-only lock (see [`MAX_ASK_LABEL_LEN`]). Not enforced.
pub const MAX_ASK_QUESTION_LEN: usize = 200;

/// Uniqueness-refine rejection message — byte-faithful to
/// `UNIQUENESS_REFINE.message` (`AskUserQuestionTool.tsx:53`).
pub const UNIQUENESS_REFINE_MESSAGE: &str =
    "Question texts must be unique, option labels must be unique within each question";

/// `checkPermissions` ask prompt — TS `message: 'Answer questions?'`
/// (`AskUserQuestionTool.tsx`). Retained for documentation; the Rust headless
/// path returns `Allow`.
pub const ASK_USER_QUESTION_ASK_MESSAGE: &str = "Answer questions?";

/// `askUserQuestionTimeout` config key (settings schema + /config row).
/// Byte-locked to the oracle key.
pub const ASK_USER_QUESTION_TIMEOUT_KEY: &str = "askUserQuestionTimeout";

/// Allowed `askUserQuestionTimeout` values, byte-faithful to the settings
/// schema enum `askUserQuestionTimeout:E.enum(["60s","5m","10m","never"])`
/// (oracle 2.1.201). The default is `"never"`.
pub const ASK_USER_QUESTION_TIMEOUT_VALUES: &[&str] = &["60s", "5m", "10m", "never"];

/// `askUserQuestionTimeout` — the idle window before an unanswered
/// AskUserQuestion prompt auto-continues with the answers selected so far.
///
/// Oracle default is [`Never`](Self::Never): auto-continue only runs when
/// explicitly set to `60s`/`5m`/`10m` (settings schema describe: "Idle time
/// before questions auto-continue with any answers selected so far. Defaults to
/// never — auto-continue only runs when explicitly set to 60s/5m/10m"). The
/// zod field is `.catch(void 0)`, so any unparsable value falls back to the
/// default rather than failing the load — mirrored by
/// [`AskUserQuestionTimeout::parse_or_default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AskUserQuestionTimeout {
    /// Wait forever — never auto-continue (the default).
    #[default]
    Never,
    /// Auto-continue after 60 seconds of idle time.
    S60,
    /// Auto-continue after 5 minutes of idle time.
    M5,
    /// Auto-continue after 10 minutes of idle time.
    M10,
}

impl AskUserQuestionTimeout {
    /// Strict parse of a settings string. Returns `None` for any value outside
    /// [`ASK_USER_QUESTION_TIMEOUT_VALUES`] (the caller decides whether to fall
    /// back to the default; see [`parse_or_default`](Self::parse_or_default)).
    #[must_use]
    pub fn from_settings_str(s: &str) -> Option<Self> {
        match s {
            "never" => Some(Self::Never),
            "60s" => Some(Self::S60),
            "5m" => Some(Self::M5),
            "10m" => Some(Self::M10),
            _ => None,
        }
    }

    /// Lenient parse mirroring the zod `.catch(void 0)` + `?? "never"` chain:
    /// an absent or unparsable value resolves to the default ([`Never`]).
    ///
    /// [`Never`]: Self::Never
    #[must_use]
    pub fn parse_or_default(s: Option<&str>) -> Self {
        s.and_then(Self::from_settings_str).unwrap_or_default()
    }

    /// The on-the-wire settings string (`"never"`/`"60s"`/`"5m"`/`"10m"`).
    #[must_use]
    pub fn as_settings_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::S60 => "60s",
            Self::M5 => "5m",
            Self::M10 => "10m",
        }
    }

    /// The idle window before auto-continue fires. [`Never`](Self::Never) ⇒
    /// `None` (block on the user forever); a duration variant ⇒ `Some(window)`.
    #[must_use]
    pub fn idle_window(self) -> Option<Duration> {
        match self {
            Self::Never => None,
            Self::S60 => Some(Duration::from_secs(60)),
            Self::M5 => Some(Duration::from_secs(5 * 60)),
            Self::M10 => Some(Duration::from_secs(10 * 60)),
        }
    }
}

// -- Domain types ------------------------------------------------------------

/// One selectable option (`questionOptionSchema`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    /// Display text the user selects (concise, 1-5 words).
    pub label: String,
    /// Explanation of what choosing this option means.
    pub description: String,
    /// Optional preview content rendered when this option is focused
    /// (markdown/HTML/code/etc.). Passthrough in headless Rust.
    pub preview: Option<String>,
}

/// One question (`questionSchema`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The complete question text.
    pub question: String,
    /// Very short chip/tag label (≤ chip width).
    pub header: String,
    /// 2-4 mutually-exclusive (unless `multi_select`) options.
    pub options: Vec<QuestionOption>,
    /// Allow multiple selections. Defaults to `false`.
    pub multi_select: bool,
}

/// Synthesize the first-option answer map — each question answered with its
/// first option's label. Used only by [`FirstOptionResolver`] in hermetic tests.
fn first_option_answers(questions: &[Question]) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(questions.len());
    for q in questions {
        // `options` is guaranteed non-empty by validation; first label is
        // the synthesized single-select answer.
        if let Some(first) = q.options.first() {
            out.insert(q.question.clone(), first.label.clone());
        }
    }
    out
}

/// Model-facing explanation used when no live prompt transport exists.
const ASK_USER_QUESTION_BLOCKED_MESSAGE: &str = "AskUserQuestion requires an interactive user selection but no live prompt UI is available in this session";

fn to_bridge_questions(questions: &[Question]) -> Vec<AskQuestion> {
    questions
        .iter()
        .map(|question| AskQuestion {
            question: question.question.clone(),
            header: question.header.clone(),
            options: question
                .options
                .iter()
                .map(|option| AskOption {
                    label: option.label.clone(),
                    description: option.description.clone(),
                    preview: option.preview.clone(),
                })
                .collect(),
            multi_select: question.multi_select,
        })
        .collect()
}

/// Resolver trait — production wraps the terminal/UI permission component that
/// collects the user's answers; the headless default synthesizes them.
///
/// Returns a map from each question's `question` text to the chosen answer
/// string (a single label, or for multi-select a comma-joined list of labels).
///
/// `non_interactive` is the session's `is_non_interactive_session` flag
/// (`--print` / async / batch). Resolvers use it to keep headless runs from
/// blocking while still refusing to silently auto-continue interactive ones.
#[async_trait]
pub trait AskUserQuestionResolver: Send + Sync {
    /// Resolve the answer map for the given questions.
    ///
    /// # Errors
    /// Implementations may surface `ToolError` if the prompt fails or if an
    /// interactive host has no answering UI (see [`DefaultTimeoutResolver`]).
    async fn resolve(
        &self,
        questions: &[Question],
        non_interactive: bool,
    ) -> Result<HashMap<String, String>, ToolError>;
}

/// Hermetic resolver — answers each question with its first option's label.
///
/// This is the default for [`AskUserQuestionTool::new`] in hermetic tests. Live
/// composition roots use a broker resolver or omit the tool entirely.
pub struct FirstOptionResolver;

#[async_trait]
impl AskUserQuestionResolver for FirstOptionResolver {
    async fn resolve(
        &self,
        questions: &[Question],
        _non_interactive: bool,
    ) -> Result<HashMap<String, String>, ToolError> {
        Ok(first_option_answers(questions))
    }
}

/// Production resolver honoring `askUserQuestionTimeout` (oracle default:
/// `never` ⇒ do NOT auto-continue).
///
/// Behavior:
/// A resolver without a UI never has a set of user-confirmed answers, so every
/// call returns [`ToolError::InteractionRequired`]. Timed auto-submit belongs
/// to [`TuiBridgeResolver`], whose view owns the confirmed-answer state.
///
/// The [`AskUserQuestionTimeout`] is fixed at construction. Registration
/// (`tool_ui::register_with_options`, M-15) builds it from the live
/// `askUserQuestionTimeout` settings value carried on
/// `BuiltinToolContext::ask_user_question_timeout`, so a user's explicit
/// `60s`/`5m`/`10m` opt-in takes effect (an absent / unparsable value falls back
/// to the oracle default [`AskUserQuestionTimeout::Never`]).
pub struct DefaultTimeoutResolver {
    /// Fixed idle window; `None` ⇒ `never` (block).
    window: Option<Duration>,
}

impl DefaultTimeoutResolver {
    /// Construct from an [`AskUserQuestionTimeout`] setting.
    #[must_use]
    pub fn new(timeout: AskUserQuestionTimeout) -> Self {
        Self {
            window: timeout.idle_window(),
        }
    }

    /// Construct with an explicit idle window (`None` ⇒ block). Test seam so the
    /// afk auto-advance path can be exercised without waiting minutes.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_window(window: Option<Duration>) -> Self {
        Self { window }
    }
}

/// Production resolver that forwards interactive sessions into the mounted TUI
/// questionnaire view through a session-scoped channel.
pub struct TuiBridgeResolver {
    event_tx: mpsc::Sender<AskUserQuestionExchange>,
    timeout_secs: Option<u64>,
}

impl TuiBridgeResolver {
    /// Build a resolver that forwards interactive questions to the provided
    /// session-scoped TUI channel.
    #[must_use]
    pub fn new(
        timeout: AskUserQuestionTimeout,
        event_tx: mpsc::Sender<AskUserQuestionExchange>,
    ) -> Self {
        Self {
            event_tx,
            timeout_secs: timeout.idle_window().map(|window| window.as_secs()),
        }
    }
}

#[async_trait]
impl AskUserQuestionResolver for DefaultTimeoutResolver {
    async fn resolve(
        &self,
        _questions: &[Question],
        _non_interactive: bool,
    ) -> Result<HashMap<String, String>, ToolError> {
        let _ = self.window;
        Err(ToolError::InteractionRequired(
            ASK_USER_QUESTION_BLOCKED_MESSAGE.to_string(),
        ))
    }
}

#[async_trait]
impl AskUserQuestionResolver for TuiBridgeResolver {
    async fn resolve(
        &self,
        questions: &[Question],
        non_interactive: bool,
    ) -> Result<HashMap<String, String>, ToolError> {
        if non_interactive {
            return Err(ToolError::InteractionRequired(
                ASK_USER_QUESTION_BLOCKED_MESSAGE.to_string(),
            ));
        }

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let exchange = AskUserQuestionExchange {
            questions: to_bridge_questions(questions),
            timeout_secs: self.timeout_secs,
            resp_tx,
        };
        self.event_tx.send(exchange).await.map_err(|_| {
            ToolError::Internal("AskUserQuestion interactive prompt bridge closed".to_string())
        })?;
        resp_rx.await.map_err(|_| {
            ToolError::Internal("AskUserQuestion interactive prompt dropped".to_string())
        })
    }
}

/// `AskUserQuestionTool` — asks the user one or more multiple-choice questions.
pub struct AskUserQuestionTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
    pub(crate) resolver: Arc<dyn AskUserQuestionResolver>,
}

impl AskUserQuestionTool {
    /// Construct with the default [`FirstOptionResolver`].
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self {
            ctx,
            resolver: Arc::new(FirstOptionResolver),
        }
    }

    /// Construct with a caller-supplied resolver (production use).
    #[must_use]
    pub fn with_resolver(
        ctx: tool_api::BuiltinToolContext,
        resolver: Arc<dyn AskUserQuestionResolver>,
    ) -> Self {
        Self { ctx, resolver }
    }
}

// -- Parsing + validation ----------------------------------------------------

/// Parse one option object. Requires non-empty string `label` + `description`;
/// `preview` is an optional string.
fn parse_option(idx_q: usize, idx_o: usize, v: &Value) -> Result<QuestionOption, ToolError> {
    let obj = v.as_object().ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].options[{idx_o}] must be an object"
        ))
    })?;
    let label = obj.get("label").and_then(Value::as_str).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].options[{idx_o}].label must be a string"
        ))
    })?;
    if label.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].options[{idx_o}].label is empty"
        )));
    }
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "AskUserQuestion: questions[{idx_q}].options[{idx_o}].description must be a string"
            ))
        })?;
    // `preview` optional: if present it must be a string (passthrough).
    let preview = match obj.get("preview") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(ToolError::InvalidInput(format!(
                "AskUserQuestion: questions[{idx_q}].options[{idx_o}].preview must be a string"
            )))
        }
    };
    Ok(QuestionOption {
        label: label.to_string(),
        description: description.to_string(),
        preview,
    })
}

/// Parse one question object, enforcing the per-question option-count bounds.
fn parse_question(idx_q: usize, v: &Value) -> Result<Question, ToolError> {
    let obj = v.as_object().ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}] must be an object"
        ))
    })?;
    let question = obj.get("question").and_then(Value::as_str).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].question must be a string"
        ))
    })?;
    if question.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].question is empty"
        )));
    }
    let header = obj.get("header").and_then(Value::as_str).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].header must be a string"
        ))
    })?;
    let opts_v = obj
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "AskUserQuestion: questions[{idx_q}].options must be an array"
            ))
        })?;
    if opts_v.len() < MIN_ASK_OPTIONS {
        return Err(ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].options must have at least {MIN_ASK_OPTIONS} options"
        )));
    }
    if opts_v.len() > MAX_ASK_OPTIONS {
        return Err(ToolError::InvalidInput(format!(
            "AskUserQuestion: questions[{idx_q}].options count {} exceeds max {MAX_ASK_OPTIONS}",
            opts_v.len()
        )));
    }
    let mut options = Vec::with_capacity(opts_v.len());
    for (idx_o, ov) in opts_v.iter().enumerate() {
        options.push(parse_option(idx_q, idx_o, ov)?);
    }

    // `multiSelect` defaults to false (TS `z.boolean().default(false)`).
    let multi_select = match obj.get("multiSelect") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ToolError::InvalidInput(format!(
                "AskUserQuestion: questions[{idx_q}].multiSelect must be a boolean"
            )))
        }
    };

    Ok(Question {
        question: question.to_string(),
        header: header.to_string(),
        options,
        multi_select,
    })
}

/// Parse the top-level `questions` array, enforcing question-count bounds.
pub(crate) fn parse_questions(input: &Value) -> Result<Vec<Question>, ToolError> {
    let arr = input
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolError::InvalidInput("AskUserQuestion: missing or non-array questions".into())
        })?;
    if arr.len() < MIN_ASK_QUESTIONS {
        return Err(ToolError::InvalidInput(format!(
            "AskUserQuestion: at least {MIN_ASK_QUESTIONS} question is required"
        )));
    }
    if arr.len() > MAX_ASK_QUESTIONS {
        return Err(ToolError::InvalidInput(format!(
            "AskUserQuestion: question count {} exceeds max {MAX_ASK_QUESTIONS}",
            arr.len()
        )));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (idx_q, qv) in arr.iter().enumerate() {
        out.push(parse_question(idx_q, qv)?);
    }
    Ok(out)
}

/// Port of `UNIQUENESS_REFINE.check` (`AskUserQuestionTool.tsx:32-54`):
/// question texts must be unique, and option labels must be unique within each
/// question. On failure returns [`UNIQUENESS_REFINE_MESSAGE`].
pub(crate) fn check_uniqueness(questions: &[Question]) -> Result<(), ToolError> {
    let mut seen_q: HashSet<&str> = HashSet::with_capacity(questions.len());
    for q in questions {
        if !seen_q.insert(q.question.as_str()) {
            return Err(ToolError::InvalidInput(UNIQUENESS_REFINE_MESSAGE.into()));
        }
        let mut seen_l: HashSet<&str> = HashSet::with_capacity(q.options.len());
        for opt in &q.options {
            if !seen_l.insert(opt.label.as_str()) {
                return Err(ToolError::InvalidInput(UNIQUENESS_REFINE_MESSAGE.into()));
            }
        }
    }
    Ok(())
}

/// Full input validation: parse + uniqueness refine.
pub(crate) fn validate_input_internal(input: &Value) -> Result<Vec<Question>, ToolError> {
    let questions = parse_questions(input)?;
    check_uniqueness(&questions)?;
    Ok(questions)
}

/// Re-serialize the parsed questions back to the wire/output shape, applying
/// the `multiSelect` default and dropping absent `preview`. This is what the TS
/// `call` echoes back as `data.questions`.
fn questions_to_json(questions: &[Question]) -> Value {
    Value::Array(
        questions
            .iter()
            .map(|q| {
                let mut opts = Vec::with_capacity(q.options.len());
                for opt in &q.options {
                    let mut o = Map::new();
                    o.insert("label".into(), Value::String(opt.label.clone()));
                    o.insert("description".into(), Value::String(opt.description.clone()));
                    if let Some(p) = &opt.preview {
                        o.insert("preview".into(), Value::String(p.clone()));
                    }
                    opts.push(Value::Object(o));
                }
                let mut m = Map::new();
                m.insert("question".into(), Value::String(q.question.clone()));
                m.insert("header".into(), Value::String(q.header.clone()));
                m.insert("options".into(), Value::Array(opts));
                m.insert("multiSelect".into(), Value::Bool(q.multi_select));
                Value::Object(m)
            })
            .collect(),
    )
}

// -- Schema ------------------------------------------------------------------

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": MIN_ASK_QUESTIONS,
                "maxItems": MAX_ASK_QUESTIONS,
                "description": "Questions to ask the user (1-4 questions)",
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark. Example: \"Which library should we use for date formatting?\" If multiSelect is true, phrase it accordingly, e.g. \"Which features do you want to enable?\""
                        },
                        "header": {
                            "type": "string",
                            "description": "Very short label displayed as a chip/tag (max 12 chars). Examples: \"Auth method\", \"Library\", \"Approach\"."
                        },
                        "options": {
                            "type": "array",
                            "minItems": MIN_ASK_OPTIONS,
                            "maxItems": MAX_ASK_OPTIONS,
                            "description": "The available choices for this question. Must have 2-4 options. Each option should be a distinct, mutually exclusive choice (unless multiSelect is enabled). There should be no 'Other' option, that will be provided automatically.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "description": "The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice."
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications."
                                    },
                                    "preview": {
                                        "type": "string",
                                        "description": "Optional preview content rendered when this option is focused. Use for mockups, code snippets, or visual comparisons that help users compare options. See the tool description for the expected content format."
                                    }
                                },
                                "required": ["label", "description"]
                            }
                        },
                        "multiSelect": {
                            "type": "boolean",
                            "default": false,
                            "description": "Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive."
                        }
                    },
                    "required": ["question", "header", "options"]
                }
            }
        },
        "required": ["questions"]
    })
});

// -- Telemetry helpers -------------------------------------------------------

fn pii_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(ASK_USER_QUESTION_FAILED, md).await;
}

// -- Tool impl ---------------------------------------------------------------

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        ASK_USER_QUESTION_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("prompt the user with a multiple-choice question")
    }
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }
    fn input_validation_issues(&self, input: &Value) -> Vec<Value> {
        // 2.1.263 j$e runs after base types parse, including a dirty (too many
        // options/questions) parse. Do not reuse the length-rejecting call parser.
        let Some(questions) = input.get("questions").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut texts = std::collections::HashSet::new();
        let mut duplicate = false;
        for question in questions {
            let (Some(text), Some(_header), Some(options)) = (
                question.get("question").and_then(Value::as_str),
                question.get("header").and_then(Value::as_str),
                question.get("options").and_then(Value::as_array),
            ) else {
                return Vec::new();
            };
            if question.get("multiSelect").is_some_and(|v| !v.is_boolean()) {
                return Vec::new();
            }
            duplicate |= !texts.insert(text);
            let mut labels = std::collections::HashSet::new();
            for option in options {
                let (Some(label), Some(_description)) = (
                    option.get("label").and_then(Value::as_str),
                    option.get("description").and_then(Value::as_str),
                ) else {
                    return Vec::new();
                };
                if option.get("preview").is_some_and(|v| !v.is_string()) {
                    return Vec::new();
                }
                duplicate |= !labels.insert(label);
            }
        }
        if duplicate {
            vec![json!({"code":"custom","path":[],"message":UNIQUENESS_REFINE_MESSAGE})]
        } else {
            Vec::new()
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        // TS `maxResultSizeChars: 100_000`.
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        // TS `isConcurrencySafe() { return true }`.
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        // TS `isReadOnly() { return true }`.
        true
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn requires_user_interaction(&self) -> bool {
        // TS `requiresUserInteraction() { return true }`.
        true
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        // 🚨 This must NOT be an `Ask`, even though the oracle's own
        // `checkPermissions` is `{behavior:"ask", message:"Answer questions?"}`
        // (2.1.270 `As` tool object, src_169588164.js @3607405). Upstream, that
        // `ask` IS the questionnaire: the permission surface for this one tool
        // is `AskUserQuestionPermissionDialog`
        // (`{event:"ui.render",component:"AskUserQuestion",
        // drawnBy:"AskUserQuestionPermissionDialog"}`), which draws the
        // questions and returns the chosen labels as `updatedInput.answers` —
        // `call` then only formats what the dialog already collected.
        //
        // This port puts the questionnaire in [`Self::call`] instead, behind
        // [`AskUserQuestionResolver`] (the TUI bottom-pane view, the desktop
        // `AskUserQuestionPrompt`, the mobile sheet). So an `ask` here does not
        // BECOME the question card the way it does upstream — it adds a
        // generic "allow AskUserQuestion?" approval in front of it, and the user
        // answers two dialogs for one question. Returning `Allow` is what makes
        // the observable behavior match the oracle's: exactly one dialog, the
        // questionnaire, and the user's answer is still the only way the call
        // produces a result.
        //
        // `requires_user_interaction()` stays `true` (the oracle's
        // `requiresUserInteraction(){return!0}`) so the tool keeps every
        // property that bit governs: no one-tap always-allow rule
        // (`suppressesAlwaysAllowRule`), no hook rescue, and no auto-mode
        // classifier waving it through — `AskUserQuestion` is deliberately NOT
        // in 2.1.270's `ojo` safe allowlist
        // (`permission::mode_policy::is_auto_mode_safe_tool`).
        //
        // ⛔ Do not "restore" the ask without first moving the questionnaire
        // onto the permission surface; the ask alone is a double prompt. It was
        // switched to `Ask` in `9ed4d5598` ("Enforce authorization before
        // interactive and executable surfaces") on the reading that the oracle
        // asks here, without that half.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: ASK_USER_QUESTION_ASK_MESSAGE.into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        // `prompt.ts:7-8` DESCRIPTION.
        "Asks the user multiple choice questions to gather information, clarify ambiguity, understand preferences, make decisions or offer them choices.".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        // `prompt.ts:32-44` ASK_USER_QUESTION_TOOL_PROMPT. The preview-format
        // suffix (`PREVIEW_FEATURE_PROMPT[format]`) is gated on
        // `getQuestionPreviewFormat()` which has no Rust substrate — omitted.
        "Use this tool when you need to ask the user questions during execution. This allows you to:\n\
         1. Gather user preferences or requirements\n\
         2. Clarify ambiguous instructions\n\
         3. Get decisions on implementation choices as you work\n\
         4. Offer choices to the user about what direction to take."
            .into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        validate_input_internal(input).map_err(|e| ValidationError(format!("{e}")))?;
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        let questions = match validate_input_internal(&input) {
            Ok(q) => q,
            Err(e) => {
                emit_failed(&bus, "invalid_input", started.elapsed().as_millis() as u64).await;
                return Err(e);
            }
        };

        // Started event.
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "question_count".into(),
            AnalyticsValue::Int(questions.len() as i64),
        );
        // `toAutoClassifierInput`: questions joined with " | ".
        let joined = questions
            .iter()
            .map(|q| q.question.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        md.insert("_PROTO_questions".into(), pii_str(&joined));
        bus.log_event(ASK_USER_QUESTION_STARTED, md).await;

        // Resolve the per-question answers (UI substitute). The resolver keys
        // its auto-continue policy off the session's interactivity.
        let non_interactive = ctx.options.is_non_interactive_session;
        // Dropping the resolver future drops its response receiver. The
        // broker can then remove exactly this request by observing a closed
        // sender, while unrelated workflow questions remain parked.
        let resolve = self.resolver.resolve(&questions, non_interactive);
        let answers_map = match if let Some(cancel) = ctx.cancel {
            tokio::select! {
                result = resolve => result,
                // `interrupt_behavior()` above is `Block`, so `turn_loop`'s
                // dispatcher-level abort race (`turn_loop.rs:4688`) never runs
                // for this tool: THIS arm is the only path a user interrupt
                // takes. `turn_loop.rs:4788` gates the `"interrupted"`
                // `toolDenialKind` on `matches!(err, ToolError::Aborted)`, so
                // returning `Internal` made `is_abort` false and recorded a
                // plain interrupt as a genuine tool failure carrying fabricated
                // model-facing copy. Use the repo's abort variant.
                _ = cancel.cancelled() => Err(ToolError::Aborted),
            }
        } else {
            resolve.await
        } {
            Ok(m) => m,
            Err(e) => {
                emit_failed(&bus, "resolver_error", started.elapsed().as_millis() as u64).await;
                return Err(e);
            }
        };

        // Build the answers object keyed by question text (insertion-stable in
        // question order for deterministic output).
        let mut answers = Map::new();
        for q in &questions {
            if let Some(answer) = answers_map.get(&q.question) {
                answers.insert(q.question.clone(), Value::String(answer.clone()));
            }
        }

        let mut completed: LogEventMetadata = HashMap::new();
        completed.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        completed.insert(
            "answer_count".into(),
            AnalyticsValue::Int(answers.len() as i64),
        );
        bus.log_event(ASK_USER_QUESTION_COMPLETED, completed).await;

        // Output `{ questions, answers, ...(annotations && {annotations}) }`.
        // Rust headless has no UI to produce annotations, so they are omitted
        // unless the input already carried them (passthrough parity).
        let mut data = Map::new();
        data.insert("questions".into(), questions_to_json(&questions));
        data.insert("answers".into(), Value::Object(answers));
        if let Some(annotations) = input.get("annotations").filter(|value| !value.is_null()) {
            data.insert("annotations".into(), annotations.clone());
        }

        Ok(ToolCallResult {
            data: Value::Object(data),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::process::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_ctx_cancelled, fresh_tx, shell_test_ctx};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// 🚨 The permission layer must NOT raise a prompt for this tool.
    ///
    /// Upstream's `checkPermissions` IS an `ask`, but upstream RENDERS that ask
    /// as the questionnaire (`AskUserQuestionPermissionDialog`) and reads the
    /// chosen labels back out of `updatedInput.answers`. This port draws the
    /// questionnaire in `call`, behind `AskUserQuestionResolver`, so an `ask`
    /// here is a SECOND dialog — a generic "allow AskUserQuestion?" card in
    /// front of the questions — and the user answers twice for one question.
    ///
    /// `requires_user_interaction()` must stay true regardless: it is what
    /// suppresses the one-tap always-allow rule and blocks a hook rescue.
    #[tokio::test]
    async fn permission_layer_does_not_stack_a_prompt_in_front_of_the_questionnaire() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let input = json!({
            "questions": [{
                "question": "Which approach?",
                "header": "Approach",
                "options": [opt("A", "first"), opt("B", "second")],
                "multiSelect": false
            }]
        });
        let result = tool.check_permissions(&input, &fresh_ctx()).await;
        assert!(
            matches!(result, PermissionResult::Allow { .. }),
            "AskUserQuestion owns its own dialog; the permission layer must not add one: {result:?}"
        );
        assert!(
            tool.requires_user_interaction(),
            "the oracle's requiresUserInteraction(){{return!0}} still holds"
        );
    }

    fn opt(label: &str, desc: &str) -> Value {
        json!({ "label": label, "description": desc })
    }

    fn one_question() -> Value {
        json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Choice",
                "options": [opt("Alpha", "the a"), opt("Beta", "the b"), opt("Gamma", "the c")]
            }]
        })
    }

    #[test]
    fn constants_locked() {
        assert_eq!(ASK_USER_QUESTION_TOOL_NAME, "AskUserQuestion");
        assert_eq!(MAX_ASK_OPTIONS, 4);
        assert_eq!(MIN_ASK_OPTIONS, 2);
        assert_eq!(MAX_ASK_QUESTIONS, 4);
        assert_eq!(MIN_ASK_QUESTIONS, 1);
        assert_eq!(ASK_USER_QUESTION_TOOL_CHIP_WIDTH, 12);
        assert_eq!(MAX_ASK_HEADER_LEN, 12);
        // Retained fixture locks (not enforced).
        assert_eq!(MAX_ASK_LABEL_LEN, 60);
        assert_eq!(MAX_ASK_QUESTION_LEN, 200);
        assert_eq!(
            UNIQUENESS_REFINE_MESSAGE,
            "Question texts must be unique, option labels must be unique within each question"
        );
    }

    // --- question-count bounds ---

    #[test]
    fn one_to_four_questions_accepted() {
        for n in MIN_ASK_QUESTIONS..=MAX_ASK_QUESTIONS {
            let qs: Vec<Value> = (0..n)
                .map(|i| {
                    json!({
                        "question": format!("Q{i}?"),
                        "header": "H",
                        "options": [opt("A", "a"), opt("B", "b")]
                    })
                })
                .collect();
            let input = json!({ "questions": qs });
            let parsed = validate_input_internal(&input).expect("n questions ok");
            assert_eq!(parsed.len(), n);
        }
    }

    #[test]
    fn five_questions_rejected() {
        let qs: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "question": format!("Q{i}?"),
                    "header": "H",
                    "options": [opt("A", "a"), opt("B", "b")]
                })
            })
            .collect();
        let input = json!({ "questions": qs });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(format!("{err}").contains("exceeds max 4"));
    }

    #[test]
    fn zero_questions_rejected() {
        let input = json!({ "questions": [] });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(format!("{err}").contains("at least 1 question"));
    }

    // --- option-count bounds ---

    #[test]
    fn two_to_four_options_accepted() {
        for n in MIN_ASK_OPTIONS..=MAX_ASK_OPTIONS {
            let opts: Vec<Value> = (0..n).map(|i| opt(&format!("L{i}"), "d")).collect();
            let input = json!({
                "questions": [{ "question": "Q?", "header": "H", "options": opts }]
            });
            validate_input_internal(&input).expect("n options ok");
        }
    }

    #[test]
    fn one_option_rejected() {
        let input = json!({
            "questions": [{ "question": "Q?", "header": "H", "options": [opt("A", "a")] }]
        });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(format!("{err}").contains("at least 2 options"));
    }

    #[test]
    fn five_options_rejected() {
        let opts: Vec<Value> = (0..5).map(|i| opt(&format!("L{i}"), "d")).collect();
        let input = json!({
            "questions": [{ "question": "Q?", "header": "H", "options": opts }]
        });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(format!("{err}").contains("exceeds max 4"));
    }

    // --- uniqueness refine ---

    #[test]
    fn duplicate_question_text_rejected() {
        let input = json!({
            "questions": [
                { "question": "Same?", "header": "H", "options": [opt("A", "a"), opt("B", "b")] },
                { "question": "Same?", "header": "H", "options": [opt("C", "c"), opt("D", "d")] }
            ]
        });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(format!("{err}").ends_with(UNIQUENESS_REFINE_MESSAGE));
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        assert_eq!(
            tool.input_validation_issues(&input),
            vec![json!({"code":"custom","path":[],"message":UNIQUENESS_REFINE_MESSAGE})]
        );
    }

    #[test]
    fn uniqueness_refinement_runs_on_dirty_but_not_aborted_base_parse() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let q = json!({"question":"Same?","header":"H","options":[opt("A","a"),opt("B","b")]});
        let mut input = json!({"questions":vec![q; 5]});
        assert_eq!(tool.input_validation_issues(&input).len(), 1);
        input["questions"][0]["header"] = json!(12);
        assert!(tool.input_validation_issues(&input).is_empty());
    }

    #[test]
    fn duplicate_label_within_question_rejected() {
        let input = json!({
            "questions": [{
                "question": "Q?",
                "header": "H",
                "options": [opt("Dup", "a"), opt("Dup", "b")]
            }]
        });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(format!("{err}").ends_with(UNIQUENESS_REFINE_MESSAGE));
    }

    #[test]
    fn same_label_across_different_questions_ok() {
        // Labels only need to be unique *within* a question.
        let input = json!({
            "questions": [
                { "question": "Q1?", "header": "H", "options": [opt("Yes", "a"), opt("No", "b")] },
                { "question": "Q2?", "header": "H", "options": [opt("Yes", "a"), opt("No", "b")] }
            ]
        });
        validate_input_internal(&input).expect("cross-question dup labels ok");
    }

    // --- header chip width guidance ---

    #[test]
    fn header_at_chip_width_ok() {
        let header = "x".repeat(MAX_ASK_HEADER_LEN);
        let input = json!({
            "questions": [{ "question": "Q?", "header": header, "options": [opt("A", "a"), opt("B", "b")] }]
        });
        validate_input_internal(&input).expect("header at chip width ok");
    }

    #[test]
    fn header_over_chip_width_is_not_hard_rejected() {
        let header = "x".repeat(MAX_ASK_HEADER_LEN + 1);
        let input = json!({
            "questions": [{ "question": "Q?", "header": header, "options": [opt("A", "a"), opt("B", "b")] }]
        });
        validate_input_internal(&input)
            .expect("chip width is model guidance, not an input-schema constraint");
    }

    // --- multiSelect default + preview passthrough ---

    #[test]
    fn multiselect_defaults_false_and_preview_passthrough() {
        let input = json!({
            "questions": [{
                "question": "Q?",
                "header": "H",
                "options": [
                    json!({ "label": "A", "description": "a", "preview": "```rust\nfn a(){}\n```" }),
                    opt("B", "b")
                ]
            }]
        });
        let qs = validate_input_internal(&input).expect("ok");
        assert!(!qs[0].multi_select);
        assert_eq!(
            qs[0].options[0].preview.as_deref(),
            Some("```rust\nfn a(){}\n```")
        );
        assert_eq!(qs[0].options[1].preview, None);
    }

    #[test]
    fn multiselect_true_parsed() {
        let input = json!({
            "questions": [{
                "question": "Q?",
                "header": "H",
                "multiSelect": true,
                "options": [opt("A", "a"), opt("B", "b")]
            }]
        });
        let qs = validate_input_internal(&input).expect("ok");
        assert!(qs[0].multi_select);
    }

    // --- resolver / call output ---

    #[tokio::test]
    async fn first_option_resolver_fills_first_label_per_question() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let input = json!({
            "questions": [
                { "question": "Q1?", "header": "H1", "options": [opt("A1", "a"), opt("B1", "b")] },
                { "question": "Q2?", "header": "H2", "options": [opt("A2", "a"), opt("B2", "b")] }
            ]
        });
        let out = tool.call(input, fresh_ctx(), fresh_tx()).await.expect("ok");
        assert_eq!(out.data["answers"]["Q1?"], json!("A1"));
        assert_eq!(out.data["answers"]["Q2?"], json!("A2"));
        // Echoed questions present with normalized multiSelect default.
        assert_eq!(out.data["questions"][0]["multiSelect"], json!(false));
        assert_eq!(out.data["questions"][0]["question"], json!("Q1?"));
    }

    #[tokio::test]
    async fn output_omits_annotations_when_absent() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let out = tool
            .call(one_question(), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert!(out.data.get("annotations").is_none());
    }

    #[tokio::test]
    async fn output_passes_through_annotations_when_present() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let input = json!({
            "questions": [{ "question": "Q?", "header": "H", "options": [opt("A", "a"), opt("B", "b")] }],
            "annotations": { "Q?": { "notes": "looks good" } }
        });
        let out = tool.call(input, fresh_ctx(), fresh_tx()).await.expect("ok");
        assert_eq!(out.data["annotations"]["Q?"]["notes"], json!("looks good"));
    }

    struct CustomResolver {
        answers: HashMap<String, String>,
    }

    #[async_trait]
    impl AskUserQuestionResolver for CustomResolver {
        async fn resolve(
            &self,
            _questions: &[Question],
            _non_interactive: bool,
        ) -> Result<HashMap<String, String>, ToolError> {
            Ok(self.answers.clone())
        }
    }

    #[tokio::test]
    async fn custom_single_answer_is_preserved_without_generated_annotations() {
        let mut answers = HashMap::new();
        answers.insert("Q?".to_string(), "Use unicode 自由输入".to_string());
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(CustomResolver { answers }),
        );
        let input = json!({
            "questions": [{ "question": "Q?", "header": "H", "options": [opt("A", "a"), opt("B", "b")] }]
        });
        let out = tool.call(input, fresh_ctx(), fresh_tx()).await.expect("ok");
        assert_eq!(out.data["answers"]["Q?"], json!("Use unicode 自由输入"));
        assert!(out.data.get("annotations").is_none());
    }

    #[tokio::test]
    async fn custom_multi_answer_preserves_input_annotations_only() {
        let mut answers = HashMap::new();
        answers.insert("Which?".to_string(), "A, pasted value".to_string());
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(CustomResolver { answers }),
        );
        let input = json!({
            "questions": [{
                "question": "Which?",
                "header": "H",
                "multiSelect": true,
                "options": [opt("A", "a"), opt("B", "b")]
            }],
            "annotations": { "Which?": { "notes": "keep me" } }
        });
        let out = tool.call(input, fresh_ctx(), fresh_tx()).await.expect("ok");
        assert_eq!(out.data["answers"]["Which?"], json!("A, pasted value"));
        assert_eq!(out.data["annotations"]["Which?"]["notes"], json!("keep me"));
        assert!(out.data["annotations"]["Which?"]
            .get("customResponses")
            .is_none());
    }

    #[tokio::test]
    async fn partial_resolver_output_omits_unanswered_questions() {
        let mut answers = HashMap::new();
        answers.insert("First?".to_string(), "A".to_string());
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(CustomResolver { answers }),
        );
        let input = json!({
            "questions": [
                {
                    "question": "First?",
                    "header": "First",
                    "options": [opt("A", "a"), opt("B", "b")]
                },
                {
                    "question": "Second?",
                    "header": "Second",
                    "options": [opt("C", "c"), opt("D", "d")]
                }
            ]
        });
        let out = tool.call(input, fresh_ctx(), fresh_tx()).await.expect("ok");
        assert_eq!(out.data["answers"]["First?"], json!("A"));
        assert!(
            out.data["answers"].get("Second?").is_none(),
            "an unanswered question must not be fabricated as an empty string"
        );
    }

    struct MultiResolver;
    #[async_trait]
    impl AskUserQuestionResolver for MultiResolver {
        async fn resolve(
            &self,
            questions: &[Question],
            _non_interactive: bool,
        ) -> Result<HashMap<String, String>, ToolError> {
            let mut m = HashMap::new();
            for q in questions {
                // Multi-select answer: comma-join all option labels.
                let joined = q
                    .options
                    .iter()
                    .map(|o| o.label.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                m.insert(q.question.clone(), joined);
            }
            Ok(m)
        }
    }

    #[tokio::test]
    async fn multiselect_answer_comma_joins() {
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(MultiResolver),
        );
        let input = json!({
            "questions": [{
                "question": "Which features?",
                "header": "Features",
                "multiSelect": true,
                "options": [opt("A", "a"), opt("B", "b"), opt("C", "c")]
            }]
        });
        let out = tool.call(input, fresh_ctx(), fresh_tx()).await.expect("ok");
        assert_eq!(out.data["answers"]["Which features?"], json!("A, B, C"));
    }

    #[tokio::test]
    async fn rejects_missing_questions() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(json!({}), fresh_ctx(), fresh_tx())
            .await
            .expect_err("missing questions");
        assert!(format!("{err}").contains("missing or non-array questions"));
    }

    #[tokio::test]
    async fn rejects_option_missing_label() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let input = json!({
            "questions": [{
                "question": "Q?",
                "header": "H",
                "options": [json!({ "description": "no label" }), opt("B", "b")]
            }]
        });
        let err = tool
            .call(input, fresh_ctx(), fresh_tx())
            .await
            .expect_err("missing label");
        assert!(format!("{err}").contains("label must be a string"));
    }

    #[test]
    fn schema_shape_is_object_with_questions() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        let s = tool.input_schema();
        assert_eq!(s["type"], json!("object"));
        assert_eq!(s["required"], json!(["questions"]));
        assert_eq!(s["properties"]["questions"]["minItems"], json!(1));
        assert_eq!(s["properties"]["questions"]["maxItems"], json!(4));
        let item = &s["properties"]["questions"]["items"];
        assert_eq!(item["properties"]["options"]["minItems"], json!(2));
        assert_eq!(item["properties"]["options"]["maxItems"], json!(4));
        // Binary does NOT emit maxLength on header — the 12-char limit is
        // enforced at runtime in parse_question, not via JSON schema constraint.
        assert!(item["properties"]["header"].get("maxLength").is_none());
        assert_eq!(item["properties"]["multiSelect"]["default"], json!(false));
        assert_eq!(item["required"], json!(["question", "header", "options"]));
    }

    // --- askUserQuestionTimeout setting ---

    #[test]
    fn timeout_default_is_never_and_blocks() {
        // Oracle default: never (wait forever, no auto-continue).
        assert_eq!(
            AskUserQuestionTimeout::default(),
            AskUserQuestionTimeout::Never
        );
        assert_eq!(AskUserQuestionTimeout::Never.idle_window(), None);
        assert_eq!(AskUserQuestionTimeout::Never.as_settings_str(), "never");
    }

    #[test]
    fn timeout_enum_values_byte_locked() {
        // Byte-faithful to the settings schema enum ["60s","5m","10m","never"].
        assert_eq!(
            ASK_USER_QUESTION_TIMEOUT_VALUES,
            &["60s", "5m", "10m", "never"]
        );
        assert_eq!(ASK_USER_QUESTION_TIMEOUT_KEY, "askUserQuestionTimeout");
    }

    #[test]
    fn timeout_parse_strict_and_windows() {
        assert_eq!(
            AskUserQuestionTimeout::from_settings_str("never"),
            Some(AskUserQuestionTimeout::Never)
        );
        assert_eq!(
            AskUserQuestionTimeout::from_settings_str("60s"),
            Some(AskUserQuestionTimeout::S60)
        );
        assert_eq!(
            AskUserQuestionTimeout::from_settings_str("5m"),
            Some(AskUserQuestionTimeout::M5)
        );
        assert_eq!(
            AskUserQuestionTimeout::from_settings_str("10m"),
            Some(AskUserQuestionTimeout::M10)
        );
        // Unknown value → None (strict).
        assert_eq!(AskUserQuestionTimeout::from_settings_str("30s"), None);
        assert_eq!(AskUserQuestionTimeout::from_settings_str(""), None);

        // Idle windows.
        assert_eq!(
            AskUserQuestionTimeout::S60.idle_window(),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            AskUserQuestionTimeout::M5.idle_window(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            AskUserQuestionTimeout::M10.idle_window(),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn timeout_parse_or_default_matches_zod_catch() {
        // zod `.catch(void 0)` + `?? "never"`: absent or unparsable ⇒ never.
        assert_eq!(
            AskUserQuestionTimeout::parse_or_default(None),
            AskUserQuestionTimeout::Never
        );
        assert_eq!(
            AskUserQuestionTimeout::parse_or_default(Some("bogus")),
            AskUserQuestionTimeout::Never
        );
        assert_eq!(
            AskUserQuestionTimeout::parse_or_default(Some("5m")),
            AskUserQuestionTimeout::M5
        );
    }

    // --- DefaultTimeoutResolver behavior ---

    #[tokio::test]
    async fn default_resolver_never_blocks_in_interactive_session() {
        // Interactive + `never` ⇒ do NOT synthesize an answer; return an error
        // (blocks pending the live prompt UI). This is the point that stops the
        // old silent auto-continue.
        let resolver = DefaultTimeoutResolver::new(AskUserQuestionTimeout::Never);
        let qs = validate_input_internal(&one_question()).expect("ok");
        let err = resolver
            .resolve(&qs, /* non_interactive */ false)
            .await
            .expect_err("never + interactive must block, not auto-pick");
        assert!(matches!(err, ToolError::InteractionRequired(_)));
        assert!(format!("{err}").contains("no live prompt UI"));
    }

    #[tokio::test]
    async fn default_resolver_requires_interaction_when_headless() {
        // Non-interactive (`--print`) has no truthful way to answer on the
        // user's behalf. It must fail explicitly rather than choosing option 1.
        let resolver = DefaultTimeoutResolver::new(AskUserQuestionTimeout::Never);
        let qs = validate_input_internal(&one_question()).expect("ok");
        let err = resolver
            .resolve(&qs, /* non_interactive */ true)
            .await
            .expect_err("headless must not fabricate an answer");
        assert!(matches!(err, ToolError::InteractionRequired(_)));
    }

    #[tokio::test]
    async fn default_resolver_without_ui_never_fabricates_timeout_answers() {
        // The resolver has no selection state. Even with a timeout it can only
        // report that interaction is required; the TUI bridge owns confirmed
        // answers and may submit an empty/partial map at timeout.
        let resolver = DefaultTimeoutResolver::with_window(Some(Duration::ZERO));
        let qs = validate_input_internal(&one_question()).expect("ok");
        let err = resolver
            .resolve(&qs, /* non_interactive */ false)
            .await
            .expect_err("a resolver without a UI cannot invent timeout answers");
        assert!(matches!(err, ToolError::InteractionRequired(_)));
    }

    #[tokio::test]
    async fn tool_with_default_resolver_errors_interactive_never() {
        // End-to-end via the tool `call`: production-style construction with the
        // default (`never`) resolver blocks an interactive call.
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(DefaultTimeoutResolver::new(AskUserQuestionTimeout::Never)),
        );
        // fresh_ctx() is interactive (is_non_interactive_session == false).
        let err = tool
            .call(one_question(), fresh_ctx(), fresh_tx())
            .await
            .expect_err("interactive never must not auto-continue");
        assert!(matches!(err, ToolError::InteractionRequired(_)));
    }

    /// `interrupt_behavior()` is `Block`, so `turn_loop`'s dispatcher-level
    /// abort race never applies to this tool: the in-tool `ctx.cancel` arm is
    /// the ONLY path a user interrupt takes. `turn_loop.rs` gates the
    /// `"interrupted"` `toolDenialKind` on `matches!(err, ToolError::Aborted)`,
    /// so returning `ToolError::Internal` recorded a plain interrupt as a
    /// genuine tool failure.
    /// A resolver that parks forever, so the only ready arm of the tool's
    /// `select!` is the cancellation token — the shape a real parked prompt has.
    struct NeverResolves;

    #[async_trait]
    impl AskUserQuestionResolver for NeverResolves {
        async fn resolve(
            &self,
            _questions: &[Question],
            _non_interactive: bool,
        ) -> Result<HashMap<String, String>, ToolError> {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    #[tokio::test]
    async fn cancelled_prompt_reports_aborted_not_internal() {
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(NeverResolves),
        );
        let err = tool
            .call(one_question(), fresh_ctx_cancelled(), fresh_tx())
            .await
            .expect_err("a cancelled prompt must not succeed");
        assert!(
            matches!(err, ToolError::Aborted),
            "a user interrupt must take the abort path so turn_loop stamps \
             toolDenialKind=\"interrupted\", got {err:?}"
        );
        assert_eq!(
            err.model_facing_message(),
            "aborted",
            "the abort variant carries no fabricated failure copy"
        );
    }

    /// `Block` is the contract that keeps `turn_loop`'s dispatcher from racing
    /// this tool's future; the fix above depends on it staying `Block`.
    #[test]
    fn ask_user_question_blocks_on_interrupt() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        assert!(matches!(
            tool.interrupt_behavior(&one_question()),
            InterruptBehavior::Block
        ));
    }
}
