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
use serde_json::{json, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    ASK_USER_QUESTION_COMPLETED, ASK_USER_QUESTION_FAILED, ASK_USER_QUESTION_STARTED,
};
use telemetry::AnalyticsBus;
use tool_api::ask_user_question::{AskOption, AskQuestion, AskUserQuestionExchange};

use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use tokio::sync::mpsc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, source_text_projection_units,
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
    ValidationError,
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
    pub label: Utf16JsonProjection,
    /// Explanation of what choosing this option means.
    pub description: Utf16JsonProjection,
    /// Optional preview content rendered when this option is focused
    /// (markdown/HTML/code/etc.). Passthrough in headless Rust.
    pub preview: Option<Utf16JsonProjection>,
}

/// One question (`questionSchema`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The complete question text.
    pub question: Utf16JsonProjection,
    /// Very short chip/tag label (≤ chip width).
    pub header: Utf16JsonProjection,
    /// 2-4 mutually-exclusive (unless `multi_select`) options.
    pub options: Vec<QuestionOption>,
    /// Allow multiple selections. Defaults to `false`.
    pub multi_select: bool,
}

/// Synthesize the first-option answer map — each question answered with its
/// first option's label. Used only by [`FirstOptionResolver`] in hermetic tests.
fn first_option_answers(questions: &[Question]) -> Vec<(Utf16JsonProjection, Utf16JsonProjection)> {
    let mut out = Vec::with_capacity(questions.len());
    for q in questions {
        // `options` is guaranteed non-empty by validation; first label is
        // the synthesized single-select answer.
        if let Some(first) = q.options.first() {
            out.push((q.question.clone(), first.label.clone()));
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
    ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError>;
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
    ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError> {
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
    ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError> {
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
    ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError> {
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
fn parse_option(
    idx_q: usize,
    idx_o: usize,
    value: JsonValueView<'_>,
) -> Result<QuestionOption, ToolError> {
    if value.object_entries().is_none() {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: questions[{idx_q}].options[{idx_o}] must be an object"
        )));
    }
    let label_units = value
        .get_property("label")
        .and_then(JsonValueView::code_units)
        .ok_or_else(|| {
            invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].options[{idx_o}].label must be a string"
            ))
        })?;
    if label_units.is_empty() {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: questions[{idx_q}].options[{idx_o}].label is empty"
        )));
    }
    let label = source_text_projection_units(label_units);
    let description = value
        .get_property("description")
        .and_then(JsonValueView::code_units)
        .map(source_text_projection_units)
        .ok_or_else(|| {
            invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].options[{idx_o}].description must be a string"
            ))
        })?;
    // `preview` optional: if present it must be a string (passthrough).
    let preview = match value.get_property("preview") {
        None => None,
        Some(value) if value.is_null() => None,
        Some(value) if value.code_units().is_some() => Some(source_text_projection_units(
            value.code_units().expect("checked string"),
        )),
        Some(_) => {
            return Err(invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].options[{idx_o}].preview must be a string"
            )));
        }
    };
    Ok(QuestionOption {
        label,
        description,
        preview,
    })
}

/// Parse one question object, enforcing the per-question option-count bounds.
fn parse_question(idx_q: usize, value: JsonValueView<'_>) -> Result<Question, ToolError> {
    if value.object_entries().is_none() {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: questions[{idx_q}] must be an object"
        )));
    }
    let question_units = value
        .get_property("question")
        .and_then(JsonValueView::code_units)
        .ok_or_else(|| {
            invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].question must be a string"
            ))
        })?;
    if question_units.is_empty() {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: questions[{idx_q}].question is empty"
        )));
    }
    let question = source_text_projection_units(question_units);
    let header = value
        .get_property("header")
        .and_then(JsonValueView::code_units)
        .map(source_text_projection_units)
        .ok_or_else(|| {
            invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].header must be a string"
            ))
        })?;
    let opts_v = value
        .get_property("options")
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| {
            invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].options must be an array"
            ))
        })?;
    if opts_v < MIN_ASK_OPTIONS {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: questions[{idx_q}].options must have at least {MIN_ASK_OPTIONS} options"
        )));
    }
    if opts_v > MAX_ASK_OPTIONS {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: questions[{idx_q}].options count {} exceeds max {MAX_ASK_OPTIONS}",
            opts_v
        )));
    }
    let mut options = Vec::with_capacity(opts_v);
    let options_view = value
        .get_property("options")
        .expect("options array was checked");
    for idx_o in 0..opts_v {
        options.push(parse_option(
            idx_q,
            idx_o,
            options_view.get_index(idx_o).expect("array item exists"),
        )?);
    }

    // `multiSelect` defaults to false (TS `z.boolean().default(false)`).
    let multi_select = match value.get_property("multiSelect") {
        None => false,
        Some(value) if value.is_null() => false,
        Some(value) if value.as_bool().is_some() => value.as_bool().unwrap_or(false),
        Some(_) => {
            return Err(invalid_tool_input(format!(
                "AskUserQuestion: questions[{idx_q}].multiSelect must be a boolean"
            )));
        }
    };

    Ok(Question {
        question,
        header,
        options,
        multi_select,
    })
}

/// Parse the top-level `questions` array, enforcing question-count bounds.
pub(crate) fn parse_questions_source(
    input: &Utf16JsonProjection,
) -> Result<Vec<Question>, ToolError> {
    let arr = input
        .get_property("questions")
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| invalid_tool_input("AskUserQuestion: missing or non-array questions"))?;
    if arr < MIN_ASK_QUESTIONS {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: at least {MIN_ASK_QUESTIONS} question is required"
        )));
    }
    if arr > MAX_ASK_QUESTIONS {
        return Err(invalid_tool_input(format!(
            "AskUserQuestion: question count {} exceeds max {MAX_ASK_QUESTIONS}",
            arr
        )));
    }
    let mut out = Vec::with_capacity(arr);
    let questions = input
        .get_property("questions")
        .expect("questions array was checked");
    for idx_q in 0..arr {
        out.push(parse_question(
            idx_q,
            questions.get_index(idx_q).expect("question item exists"),
        )?);
    }
    Ok(out)
}

/// Port of `UNIQUENESS_REFINE.check` (`AskUserQuestionTool.tsx:32-54`):
/// question texts must be unique, and option labels must be unique within each
/// question. On failure returns [`UNIQUENESS_REFINE_MESSAGE`].
pub(crate) fn check_uniqueness(questions: &[Question]) -> Result<(), ToolError> {
    let mut seen_q: HashSet<Vec<u16>> = HashSet::with_capacity(questions.len());
    for q in questions {
        let question_units = q.question.string_units("").unwrap_or_default();
        if !seen_q.insert(question_units) {
            return Err(invalid_tool_input(UNIQUENESS_REFINE_MESSAGE));
        }
        let mut seen_l: HashSet<Vec<u16>> = HashSet::with_capacity(q.options.len());
        for opt in &q.options {
            let label_units = opt.label.string_units("").unwrap_or_default();
            if !seen_l.insert(label_units) {
                return Err(invalid_tool_input(UNIQUENESS_REFINE_MESSAGE));
            }
        }
    }
    Ok(())
}

/// Full input validation: parse + uniqueness refine.
pub(crate) fn validate_input_internal_source(
    input: &Utf16JsonProjection,
) -> Result<Vec<Question>, ToolError> {
    let questions = parse_questions_source(input)?;
    check_uniqueness(&questions)?;
    Ok(questions)
}

#[cfg(test)]
pub(crate) fn validate_input_internal(input: &Value) -> Result<Vec<Question>, ToolError> {
    validate_input_internal_source(&generated_builtin_result(input))
}

fn questions_to_projection(
    input: &Utf16JsonProjection,
    questions: &[Question],
) -> Utf16JsonProjection {
    let mut rendered = Vec::with_capacity(questions.len());
    for question_index in 0..questions.len() {
        let pointer = format!("/questions/{question_index}");
        let question_source = input
            .subprojection(&format!("{pointer}/question"))
            .expect("validated question text exists");
        let header_source = input
            .subprojection(&format!("{pointer}/header"))
            .expect("validated question header exists");
        let options_source = input
            .at_pointer(&format!("{pointer}/options"))
            .expect("validated options exist");
        let option_count = options_source.array_len().expect("options are an array");
        let mut options = Vec::with_capacity(option_count);
        for option_index in 0..option_count {
            let option_pointer = format!("{pointer}/options/{option_index}");
            let mut fields = vec![
                (
                    "label".into(),
                    input
                        .subprojection(&format!("{option_pointer}/label"))
                        .expect("validated option label exists"),
                ),
                (
                    "description".into(),
                    input
                        .subprojection(&format!("{option_pointer}/description"))
                        .expect("validated option description exists"),
                ),
            ];
            if input
                .at_pointer(&format!("{option_pointer}/preview"))
                .is_some_and(|value| !value.is_null())
            {
                fields.push((
                    "preview".into(),
                    input
                        .subprojection(&format!("{option_pointer}/preview"))
                        .expect("validated option preview exists"),
                ));
            }
            options.push(
                Utf16JsonProjection::object(fields).expect("AskUserQuestion option is valid JSON"),
            );
        }
        let multi_select = input
            .at_pointer(&format!("{pointer}/multiSelect"))
            .filter(|value| !value.is_null())
            .and_then(|_| input.subprojection(&format!("{pointer}/multiSelect")).ok())
            .unwrap_or_else(|| generated_builtin_result(&json!(false)));
        rendered.push(
            Utf16JsonProjection::object(vec![
                ("question".into(), question_source),
                ("header".into(), header_source),
                (
                    "options".into(),
                    Utf16JsonProjection::array(options)
                        .expect("AskUserQuestion options are valid JSON"),
                ),
                ("multiSelect".into(), multi_select),
            ])
            .expect("AskUserQuestion question is valid JSON"),
        );
    }
    Utf16JsonProjection::array(rendered).expect("AskUserQuestion question list is valid JSON")
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

static SOURCE_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| generated_builtin_schema(&SCHEMA));

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
    fn supports_persistent_chat(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        ASK_USER_QUESTION_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("prompt the user with a multiple-choice question".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &SOURCE_SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn input_validation_issues(&self, input: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
        // 2.1.263 j$e runs after base types parse, including a dirty (too many
        // options/questions) parse. Do not reuse the length-rejecting call parser.
        let Some(questions) = input
            .get_property("questions")
            .and_then(JsonValueView::array_len)
        else {
            return Vec::new();
        };
        let mut texts = std::collections::HashSet::<Vec<u16>>::new();
        let mut duplicate = false;
        for index in 0..questions {
            let Some(question) = input
                .get_property("questions")
                .and_then(|questions| questions.get_index(index))
            else {
                return Vec::new();
            };
            let (Some(text), Some(_header), Some(options)) = (
                question
                    .get_property("question")
                    .and_then(JsonValueView::code_units),
                question
                    .get_property("header")
                    .and_then(JsonValueView::code_units),
                question
                    .get_property("options")
                    .and_then(JsonValueView::array_len),
            ) else {
                return Vec::new();
            };
            if question
                .get_property("multiSelect")
                .is_some_and(|value| !value.is_null() && value.as_bool().is_none())
            {
                return Vec::new();
            }
            duplicate |= !texts.insert(text.to_vec());
            let mut labels = std::collections::HashSet::<Vec<u16>>::new();
            let options_view = question
                .get_property("options")
                .expect("options array was checked");
            for option_index in 0..options {
                let Some(option) = options_view.get_index(option_index) else {
                    return Vec::new();
                };
                let (Some(label), Some(_description)) = (
                    option
                        .get_property("label")
                        .and_then(JsonValueView::code_units),
                    option
                        .get_property("description")
                        .and_then(JsonValueView::code_units),
                ) else {
                    return Vec::new();
                };
                if option
                    .get_property("preview")
                    .is_some_and(|value| !value.is_null() && value.code_units().is_none())
                {
                    return Vec::new();
                }
                duplicate |= !labels.insert(label.to_vec());
            }
        }
        if duplicate {
            vec![generated_builtin_result(
                &json!({"code":"custom","path":[],"message":UNIQUENESS_REFINE_MESSAGE}),
            )]
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
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        // TS `isConcurrencySafe() { return true }`.
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        // TS `isReadOnly() { return true }`.
        true
    }
    fn is_destructive(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_open_world(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn requires_user_interaction(&self) -> bool {
        // TS `requiresUserInteraction() { return true }`.
        true
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
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

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        // `prompt.ts:7-8` DESCRIPTION.
        "Asks the user multiple choice questions to gather information, clarify ambiguity, understand preferences, make decisions or offer them choices.".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
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
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        validate_input_internal_source(input)
            .map_err(|error| ValidationError(error.model_facing_message()))?;
        Ok(())
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        let questions = match validate_input_internal_source(&input) {
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
            .map(|q| q.question.string_at("").map(str::to_owned))
            .collect::<Option<Vec<_>>>();
        if let Some(joined) = joined {
            md.insert("_PROTO_questions".into(), pii_str(&joined.join(" | ")));
        }
        bus.log_event(ASK_USER_QUESTION_STARTED, md).await;

        // Resolve the per-question answers (UI substitute). The resolver keys
        // its auto-continue policy off the session's interactivity.
        let non_interactive = ctx.options.is_non_interactive_session;
        // Dropping the resolver future drops its response receiver. The
        // broker can then remove exactly this request by observing a closed
        // sender, while unrelated workflow questions remain parked.
        let resolve = self.resolver.resolve(&questions, non_interactive);
        let answers = match if let Some(cancel) = ctx.cancel {
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
        let mut answer_fields = Vec::new();
        for question in &questions {
            let question_units = question
                .question
                .string_units("")
                .expect("validated question text is a source string");
            if let Some((_, answer)) = answers
                .iter()
                .find(|(key, _)| key.string_units("").as_deref() == Some(question_units.as_slice()))
            {
                answer_fields.push((question_units, answer.clone()));
            }
        }
        let answer_count = answer_fields.len();
        let answers = Utf16JsonProjection::object_units(answer_fields)
            .expect("AskUserQuestion answers object is valid JSON");

        let mut completed: LogEventMetadata = HashMap::new();
        completed.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        completed.insert(
            "answer_count".into(),
            AnalyticsValue::Int(answer_count as i64),
        );
        bus.log_event(ASK_USER_QUESTION_COMPLETED, completed).await;

        // Output `{ questions, answers, ...(annotations && {annotations}) }`.
        // Rust headless has no UI to produce annotations, so they are omitted
        // unless the input already carried them (passthrough parity).
        let mut data = vec![
            (
                "questions".into(),
                questions_to_projection(&input, &questions),
            ),
            ("answers".into(), answers),
        ];
        if input
            .get_property("annotations")
            .is_some_and(|value| !value.is_null())
        {
            data.push((
                "annotations".into(),
                input
                    .subprojection("/annotations")
                    .expect("annotations field exists"),
            ));
        }

        Ok(ToolCallResult {
            data: Utf16JsonProjection::object(data).expect("AskUserQuestion result is valid JSON"),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

#[cfg(test)]
#[derive(Debug)]
struct TestAskUserQuestionResult {
    data: Value,
}

#[cfg(test)]
impl AskUserQuestionTool {
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }

    fn input_validation_issues(&self, input: &Value) -> Vec<Value> {
        Tool::input_validation_issues(self, &generated_builtin_result(input))
            .into_iter()
            .map(|issue| {
                serde_json::from_str(
                    &issue
                        .to_json_string()
                        .expect("test issue projection serializes"),
                )
                .expect("test issue is JSON")
            })
            .collect()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<TestAskUserQuestionResult, ToolError> {
        let result = Tool::call(self, generated_builtin_result(&input), ctx, progress).await?;
        let data = serde_json::from_str(
            &result
                .data
                .to_json_string()
                .expect("test result projection serializes"),
        )
        .expect("test result JSON parses");
        Ok(TestAskUserQuestionResult { data })
    }
}

fn invalid_tool_input(message: impl Into<String>) -> ToolError {
    let message = message.into();
    ToolError::InvalidInput(tool_api::tool_trait::source_text_projection(&message))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checked_source(
        source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> (String, Vec<u16>) {
        let display = source
            .string_at("")
            .expect("error source is a root string")
            .to_owned();
        let units = source
            .string_units("")
            .expect("error source retains UTF-16 units");
        assert_eq!(String::from_utf16_lossy(&units), display);
        (display, units)
    }

    fn source_text(source: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> String {
        checked_source(source).0
    }

    use mobile_linux_api::ProcessOutput;
    use tool_api::ask_user_question::join_answer_labels;
    use tool_api::test_support::{fresh_ctx, fresh_ctx_cancelled, fresh_tx, shell_test_ctx};
    use tool_api::tool_trait::source_text_projection;

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
        let result = tool
            .check_permissions(&generated_builtin_result(&input), &fresh_ctx())
            .await;
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
        assert!(source_text(&err.model_facing_message()).contains("exceeds max 4"));
    }

    #[test]
    fn zero_questions_rejected() {
        let input = json!({ "questions": [] });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(source_text(&err.model_facing_message()).contains("at least 1 question"));
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
        assert!(source_text(&err.model_facing_message()).contains("at least 2 options"));
    }

    #[test]
    fn five_options_rejected() {
        let opts: Vec<Value> = (0..5).map(|i| opt(&format!("L{i}"), "d")).collect();
        let input = json!({
            "questions": [{ "question": "Q?", "header": "H", "options": opts }]
        });
        let err = validate_input_internal(&input).unwrap_err();
        assert!(source_text(&err.model_facing_message()).contains("exceeds max 4"));
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
        assert!(source_text(&err.model_facing_message()).ends_with(UNIQUENESS_REFINE_MESSAGE));
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
        assert!(source_text(&err.model_facing_message()).ends_with(UNIQUENESS_REFINE_MESSAGE));
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
            qs[0].options[0]
                .preview
                .as_ref()
                .and_then(|preview| preview.string_at("")),
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
        answers: Vec<(Utf16JsonProjection, Utf16JsonProjection)>,
    }

    #[async_trait]
    impl AskUserQuestionResolver for CustomResolver {
        async fn resolve(
            &self,
            _questions: &[Question],
            _non_interactive: bool,
        ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError> {
            Ok(self.answers.clone())
        }
    }

    #[tokio::test]
    async fn custom_single_answer_is_preserved_without_generated_annotations() {
        let answers = vec![(
            source_text_projection("Q?"),
            source_text_projection("Use unicode 自由输入"),
        )];
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
    async fn ask_question_and_answer_retain_unpaired_utf16_units() {
        let question_units = vec![b'Q' as u16, 0xD800];
        let selected_units = vec![b'A' as u16, 0xDC00];
        let question = source_text_projection_units(&question_units);
        let selected = source_text_projection_units(&selected_units);
        let input = Utf16JsonProjection::object(vec![(
            "questions".into(),
            Utf16JsonProjection::array(vec![Utf16JsonProjection::object(vec![
                ("question".into(), question.clone()),
                ("header".into(), source_text_projection("H")),
                (
                    "options".into(),
                    Utf16JsonProjection::array(vec![
                        Utf16JsonProjection::object(vec![
                            ("label".into(), selected.clone()),
                            ("description".into(), source_text_projection("first")),
                        ])
                        .unwrap(),
                        Utf16JsonProjection::object(vec![
                            ("label".into(), source_text_projection("B")),
                            ("description".into(), source_text_projection("second")),
                        ])
                        .unwrap(),
                    ])
                    .unwrap(),
                ),
            ])
            .unwrap()])
            .unwrap(),
        )])
        .unwrap();
        let tool = AskUserQuestionTool::with_resolver(
            shell_test_ctx(dummy_out()),
            Arc::new(CustomResolver {
                answers: vec![(question, selected)],
            }),
        );

        let result = Tool::call(&tool, input, fresh_ctx(), fresh_tx())
            .await
            .expect("source question resolves");
        assert_eq!(
            result.data.string_units("/questions/0/question"),
            Some(question_units.clone())
        );
        let answers = result.data.subprojection("/answers").unwrap();
        let answer_entries = answers.object_entries().unwrap();
        assert_eq!(answer_entries[0].code_units, question_units);
        assert_eq!(
            answer_entries[0].value.code_units(),
            Some(selected_units.as_slice())
        );
    }

    #[tokio::test]
    async fn custom_multi_answer_preserves_input_annotations_only() {
        let answers = vec![(
            source_text_projection("Which?"),
            source_text_projection("A, pasted value"),
        )];
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
        let answers = vec![(
            source_text_projection("First?"),
            source_text_projection("A"),
        )];
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
        ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError> {
            let mut answers = Vec::new();
            for q in questions {
                // Multi-select answer: comma-join all option labels.
                let labels = q
                    .options
                    .iter()
                    .map(|o| o.label.clone())
                    .collect::<Vec<_>>();
                answers.push((q.question.clone(), join_answer_labels(&labels)));
            }
            Ok(answers)
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
        assert!(source_text(&err.model_facing_message()).contains("missing or non-array questions"));
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
        assert!(source_text(&err.model_facing_message()).contains("label must be a string"));
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
        ) -> Result<Vec<(Utf16JsonProjection, Utf16JsonProjection)>, ToolError> {
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
            err.model_facing_message().string_at(""),
            Some("aborted"),
            "the abort variant carries no fabricated failure copy"
        );
    }

    /// `Block` is the contract that keeps `turn_loop`'s dispatcher from racing
    /// this tool's future; the fix above depends on it staying `Block`.
    #[test]
    fn ask_user_question_blocks_on_interrupt() {
        let tool = AskUserQuestionTool::new(shell_test_ctx(dummy_out()));
        assert!(matches!(
            tool.interrupt_behavior(&generated_builtin_result(&one_question())),
            InterruptBehavior::Block
        ));
    }
}
