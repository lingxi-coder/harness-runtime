//! File-backed Plan Mode tools, checked against Claude Code 2.1.319.
//!
//! Model prompts, input schemas and result text have artifact-derived byte
//! locks. The host owns the shared plan-file identity, enforcing approval gate
//! and teammate review transport; model input cannot replace those capabilities.

use std::time::Instant;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use telemetry::pii::Verified;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    ENTER_PLAN_MODE_COMPLETED, ENTER_PLAN_MODE_FAILED, ENTER_PLAN_MODE_STARTED,
    EXIT_PLAN_MODE_COMPLETED, EXIT_PLAN_MODE_FAILED, EXIT_PLAN_MODE_STARTED,
};

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Structured confirmation returned by the native EnterPlanMode call.
pub const ENTER_PLAN_MODE_CONFIRMATION: &str = "Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.";

/// Instruction block surfaced to the model on entering plan mode. Byte-faithful
/// port of the non-interview-phase branch of `EnterPlanModeTool.ts`
/// `mapToolResultToToolResultBlockParam` (`:108-118`). LingXi has no
/// `isPlanModeInterviewPhaseEnabled` substrate, so the close-parity default
/// (the numbered exploration steps) is always emitted.
pub const ENTER_PLAN_MODE_INSTRUCTIONS: &str = "Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.

In plan mode, you should:
1. Thoroughly explore the codebase to understand existing patterns
2. Identify similar features and architectural approaches
3. Consider multiple approaches and their trade-offs
4. Use AskUserQuestion if you need to clarify the approach
5. Design a concrete implementation strategy
6. When ready, use ExitPlanMode to present your plan for approval

Remember: DO NOT write or edit any files yet. This is a read-only exploration and planning phase.";

/// Claude Code 2.1.319 `en()`/`Ze()` with default output style and hybrid search
/// guidance. The two session-dependent substitutions are rendered below.
pub const ENTER_PLAN_MODE_TOOL_PROMPT: &str = r#"Use this tool proactively when you're about to start a non-trivial implementation task. Getting user sign-off on your approach before writing code prevents wasted effort and ensures alignment. This tool transitions you into plan mode where you can explore the codebase and design an implementation approach for user approval.

## When to Use This Tool

**Prefer using EnterPlanMode** for implementation tasks unless they're simple. Use it when ANY of these conditions apply:

1. **New Feature Implementation**: Adding meaningful new functionality
   - Example: "Add a logout button" - where should it go? What should happen on click?
   - Example: "Add form validation" - what rules? What error messages?

2. **Multiple Valid Approaches**: The task can be solved in several different ways
   - Example: "Add caching to the API" - could use Redis, in-memory, file-based, etc.
   - Example: "Improve performance" - many optimization strategies possible

3. **Code Modifications**: Changes that affect existing behavior or structure
   - Example: "Update the login flow" - what exactly should change?
   - Example: "Refactor this component" - what's the target architecture?

4. **Architectural Decisions**: The task requires choosing between patterns or technologies
   - Example: "Add real-time updates" - WebSockets vs SSE vs polling
   - Example: "Implement state management" - Redux vs Context vs custom solution

5. **Multi-File Changes**: The task will likely touch more than 2-3 files
   - Example: "Refactor the authentication system"
   - Example: "Add a new API endpoint with tests"

6. **Unclear Requirements**: You need to explore before understanding the full scope
   - Example: "Make the app faster" - need to profile and identify bottlenecks
   - Example: "Fix the bug in checkout" - need to investigate root cause

7. **User Preferences Matter**: The implementation could reasonably go multiple ways
   - If you would use AskUserQuestion to clarify the approach, use EnterPlanMode instead
   - Plan mode lets you explore first, then present options with context

## When NOT to Use This Tool

Only skip EnterPlanMode for simple tasks:
- Single-line or few-line fixes (typos, obvious bugs, small tweaks)
- Adding a single function with clear requirements
- Tasks where the user has given very specific, detailed instructions
- Pure research/exploration tasks (use the Agent tool instead)

## What Happens in Plan Mode

In plan mode, you'll:
1. Thoroughly explore the codebase using `find`/Glob, `grep`/Grep, and Read
2. Understand existing patterns and architecture
3. Design an implementation approach
4. Present your plan to the user for approval
5. Use AskUserQuestion if you need to clarify approaches
6. Exit plan mode with ExitPlanMode when ready to implement

## Examples

### GOOD - Use EnterPlanMode:
User: "Add user authentication to the app"
- Requires architectural decisions (session vs JWT, where to store tokens, middleware structure)

User: "Optimize the database queries"
- Multiple approaches possible, need to profile first, significant impact

User: "Implement dark mode"
- Architectural decision on theme system, affects many components

User: "Add a delete button to the user profile"
- Seems simple but involves: where to place it, confirmation dialog, API call, error handling, state updates

User: "Update the error handling in the API"
- Affects multiple files, user should approve the approach

### BAD - Don't use EnterPlanMode:
User: "Fix the typo in the README"
- Straightforward, no planning needed

User: "Add a console.log to debug this function"
- Simple, obvious implementation

User: "What files handle routing?"
- Research task, not implementation planning

## Important Notes

- This tool REQUIRES user approval - they must consent to entering plan mode
- If unsure whether to use it, err on the side of planning - it's better to get alignment upfront than to redo work
- Users appreciate being consulted before significant changes are made to their codebase
"#;

/// Resolve the original prompt's output-style and hybrid search branches from
/// trusted per-request host facts, without changing any other prompt bytes.
#[must_use]
pub fn render_enter_plan_mode_prompt(output_style_is_default: bool, hybrid_search: bool) -> String {
    let mut prompt = ENTER_PLAN_MODE_TOOL_PROMPT.to_owned();
    if !output_style_is_default {
        prompt = prompt.replace(
            "- Pure research/exploration tasks (use the Agent tool instead)",
            "- Pure research/exploration tasks",
        );
    }
    if !hybrid_search {
        prompt = prompt.replace(
            "using `find`/Glob, `grep`/Grep, and Read",
            "using Glob, Grep, and Read",
        );
    }
    prompt
}

/// Locked rejection string for using `EnterPlanMode` inside an agent context.
/// Byte-faithful to the TS throw at `EnterPlanModeTool.ts:79`.
const ENTER_PLAN_MODE_AGENT_GUARD_MSG: &str = "EnterPlanMode tool cannot be used in agent contexts";

/// Full model-facing tool prompt for `ExitPlanMode` — byte-faithful port of
/// `EXIT_PLAN_MODE_V2_TOOL_PROMPT` (`ExitPlanModeTool/prompt.ts:6-29`), returned
/// by `ExitPlanModeV2Tool.ts:154-156`. The TS template interpolates
/// `${ASK_USER_QUESTION_TOOL_NAME}` (= `"AskUserQuestion"`, prompt.ts:4); that
/// substitution is inlined here so the text is verbatim what the model sees.
pub const EXIT_PLAN_MODE_V2_TOOL_PROMPT: &str = r#"Use this tool when you are in plan mode and have finished writing your plan to the plan file and are ready for user approval.

## How This Tool Works
- You should have already written your plan to the plan file specified in the plan mode system message
- This tool does NOT take the plan content as a parameter - it will read the plan from the file you wrote
- This tool simply signals that you're done planning and ready for the user to review and approve
- The user will see the contents of your plan file when they review it

## When to Use This Tool
IMPORTANT: Only use this tool when the task requires planning the implementation steps of a task that requires writing code. For research tasks where you're gathering information, searching files, reading files or in general trying to understand the codebase - do NOT use this tool.

## Before Using This Tool
Ensure your plan is complete and unambiguous:
- If you have unresolved questions about requirements or approach, use AskUserQuestion first (in earlier phases)
- Once your plan is finalized, use THIS tool to request approval

**Important:** Do NOT use AskUserQuestion to ask "Is this plan okay?" or "Should I proceed?" - that's exactly what THIS tool does. ExitPlanMode inherently requests user approval of your plan.

## Examples

1. Initial task: "Search for and understand the implementation of vim mode in the codebase" - Do not use the exit plan mode tool because you are not planning the implementation steps of a task.
2. Initial task: "Help me implement yank mode for vim" - Use the exit plan mode tool after you have finished planning the implementation steps of the task.
3. Initial task: "Add a new feature to handle user authentication" - If unsure about auth method (OAuth, JWT, etc.), use AskUserQuestion first, then use exit plan mode tool after clarifying the approach.
"#;

/// The two lines that follow the approved-plan opener — 2.1.266
/// `mapToolResultToToolResultBlockParam` (`src_162329786.js` @3516828). Upstream
/// always has a `filePath` (`C=ay(n.agentId)`), so these always render there;
/// here they render whenever a plan-file identity has been published.
const EXIT_PLAN_SAVED_TO_PREFIX: &str = "Your plan has been saved to: ";
/// Second half of the same pair.
const EXIT_PLAN_REFER_BACK_LINE: &str = "You can refer back to it if needed during implementation.";

/// 2.1.319 `uer(hasTaskTool)` — used only when Agent is available and the
/// request uses the default output style.
const EXIT_PLAN_TEAMMATE_SUFFIX: &str = "\n\nIf this plan can be broken down into multiple independent tasks, consider spawning named teammates with the Agent tool (pass a `name`) to parallelize the work.";

/// Heading for the echoed plan — `## ${planWasEdited?"Approved Plan (edited by
/// user)":"Approved Plan"}:`.
const EXIT_PLAN_APPROVED_HEADING: &str = "Approved Plan";
/// The edited-by-user variant of [`EXIT_PLAN_APPROVED_HEADING`].
const EXIT_PLAN_APPROVED_HEADING_EDITED: &str = "Approved Plan (edited by user)";

/// Model-facing approval text emitted by `ExitPlanMode` in an agent context —
/// byte-faithful port of the `isAgent` branch of
/// `mapToolResultToToolResultBlockParam` (`ExitPlanModeV2Tool.ts:452-459`).
const EXIT_PLAN_APPROVED_AGENT_MSG: &str = "User has approved the plan. There is nothing else needed from you now. Please respond with \"ok\"";

/// Model-facing approval text when the plan is empty — byte-faithful port of the
/// empty-plan branch (`ExitPlanModeV2Tool.ts:461-468`).
const EXIT_PLAN_APPROVED_EMPTY_MSG: &str =
    "User has approved exiting plan mode. You can now proceed.";

/// Opening line of the model-facing approval text when a plan is present — port
/// of the approved branch (2.1.266 @3516828). The two file-path lines that
/// follow it are [`EXIT_PLAN_SAVED_TO_PREFIX`] / [`EXIT_PLAN_REFER_BACK_LINE`],
/// and the echoed plan section comes LAST, after the optional
/// [`EXIT_PLAN_TEAMMATE_SUFFIX`].
const EXIT_PLAN_APPROVED_PREFIX: &str = "User has approved your plan. You can now start coding. Start with updating your todo list if applicable";

/// Locked rejection string for calling `ExitPlanMode` outside plan mode —
/// byte-faithful to `validateInput` (`ExitPlanModeV2Tool.ts:212-216`).
const EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG: &str = "You are not in plan mode. To enter plan mode, call the EnterPlanMode tool first. If your plan was already approved, continue with implementation.";

/// A missing permission gate is never an approval. Hosts that cannot wire an
/// enforcing gate must fail closed instead of allowing `ExitPlanMode` to
/// mutate the session state without user approval.
const EXIT_PLAN_MODE_PERMISSION_GATE_UNAVAILABLE_MSG: &str =
    "ExitPlanMode permission gate is unavailable; refusing to exit plan mode.";

/// Canonical tool name in the registry for `EnterPlanModeTool`.
pub const ENTER_TOOL_NAME: &str = "EnterPlanMode";
/// Canonical tool name in the registry for `ExitPlanModeTool`.
pub const EXIT_TOOL_NAME: &str = "ExitPlanMode";

const PLAN_MODE_MAX_RESULT_SIZE_CHARS: usize = 100_000;

/// Model-facing Exit result. Keep the plan's original UTF-16 units when
/// appending it; the display projection is used only for the JS trim guard.
#[must_use]
pub fn render_exit_plan_mode_result(
    plan: Option<&Utf16JsonProjection>,
    is_agent: bool,
    file_path: Option<&str>,
    has_task_tool: bool,
    output_style_is_default: bool,
    plan_was_edited: bool,
) -> Utf16JsonProjection {
    if is_agent {
        return Utf16JsonProjection::from(EXIT_PLAN_APPROVED_AGENT_MSG);
    }
    let plan_body = plan.and_then(|source| source.string_at("")).unwrap_or("");
    if lingxi_core::host::effort::trim_js_whitespace(plan_body).is_empty() {
        return Utf16JsonProjection::from(EXIT_PLAN_APPROVED_EMPTY_MSG);
    }
    let heading = if plan_was_edited {
        EXIT_PLAN_APPROVED_HEADING_EDITED
    } else {
        EXIT_PLAN_APPROVED_HEADING
    };
    let saved_to = file_path.map_or_else(String::new, |path| {
        format!("\n\n{EXIT_PLAN_SAVED_TO_PREFIX}{path}\n{EXIT_PLAN_REFER_BACK_LINE}")
    });
    let teammate_suffix = if has_task_tool && output_style_is_default {
        EXIT_PLAN_TEAMMATE_SUFFIX
    } else {
        ""
    };
    let prefix =
        format!("{EXIT_PLAN_APPROVED_PREFIX}{saved_to}{teammate_suffix}\n\n## {heading}:\n");
    let mut units: Vec<u16> = prefix.encode_utf16().collect();
    units.extend(
        plan.and_then(|source| source.string_units(""))
            .unwrap_or_default(),
    );
    tool_api::tool_trait::source_text_projection_units(&units)
}

fn agent_tool_available(ctx: &ToolUseContext) -> bool {
    ctx.available_tool_names.as_ref().is_some_and(|names| {
        names
            .iter()
            .any(|name| matches!(name.string_at(""), Some("Agent" | "Task")))
    })
}

static EMPTY_INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    tool_api::tool_trait::generated_builtin_schema(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
});

/// `ExitPlanMode` input schema — port of 2.1.206 `Btd`
/// (`ExitPlanModeV2Tool.ts`): `E.strictObject({allowedPrompts:
/// E.array(Poy()).optional().describe("Deprecated: no longer used.")})
/// .passthrough()`. `allowedPrompts` is now a DEPRECATED field ("no longer
/// used." — was the old prompt-based-permission array); its `Poy()` item shape
/// `{tool:"Bash", prompt:string}` is retained byte-exact. The `.passthrough()`
/// (extra keys allowed so `normalizeToolInput` can inject `plan`/`planFilePath`)
/// maps to the native 2.1.319 `additionalProperties: {}` here.
// Mirrors the pre-existing `EMPTY_INPUT_SCHEMA` `once_cell::Lazy` style above;
// `allow` keeps this batch from adding a net-new pedantic warning while staying
// consistent with the surrounding code (a `LazyLock` migration is out of scope).
// `unknown_lints` keeps the pinned 1.82 toolchain (whose clippy predates the
// `non_std_lazy_statics` lint) from erroring on the allow below under `-D warnings`,
// while newer clippy still honors the allow.
#[allow(unknown_lints, clippy::non_std_lazy_statics)]
static EXIT_INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    tool_api::tool_trait::generated_builtin_schema(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "allowedPrompts": {
                "description": "Deprecated: no longer used.",
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": {
                            "description": "The tool this prompt applies to",
                            "type": "string",
                            "enum": ["Bash"]
                        },
                        "prompt": {
                            "description": "Semantic description of the action, e.g. \"run tests\", \"install dependencies\"",
                            "type": "string"
                        }
                    },
                    "required": ["tool", "prompt"],
                    "additionalProperties": false
                }
            }
        },
        "additionalProperties": {}
    }))
});

/// `F8o` (2.1.266 `src_162329786.js` @3902505) — `EnterPlanMode`'s output
/// schema, `c({message: s().describe(…)})`.
#[allow(unknown_lints, clippy::non_std_lazy_statics)]
static ENTER_OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    tool_api::tool_trait::generated_builtin_schema(&json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Confirmation that plan mode was entered"
            }
        },
        "required": ["message"],
        "additionalProperties": true
    }))
});

/// `a4o` (2.1.266 `src_162329786.js` @3511807) — `ExitPlanMode`'s output schema.
/// `plan` is `s().nullable()`; every other field but `isAgent` is `.optional()`.
/// Upstream's `c(...)` is a non-strict zod object, so unknown keys pass — which
/// is what lets LingXi's `model_content` transport key ride along.
#[allow(unknown_lints, clippy::non_std_lazy_statics)]
static EXIT_OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    tool_api::tool_trait::generated_builtin_schema(&json!({
        "type": "object",
        "properties": {
            "plan": {
                "type": ["string", "null"],
                "description": "The plan that was presented to the user"
            },
            "isAgent": { "type": "boolean" },
            "filePath": {
                "type": "string",
                "description": "The file path where the plan was saved"
            },
            "hasTaskTool": {
                "type": "boolean",
                "description": "Whether the Agent tool is available in the current context"
            },
            "planWasEdited": {
                "type": "boolean",
                "description": "True when the user edited the plan (CCR web UI or Ctrl+G); determines whether the plan is echoed back in tool_result"
            },
            "awaitingLeaderApproval": {
                "type": "boolean",
                "description": "When true, the teammate has sent a plan approval request to the team leader"
            },
            "requestId": {
                "type": "string",
                "description": "Unique identifier for the plan approval request"
            }
        },
        "required": ["plan", "isAgent"],
        "additionalProperties": true
    }))
});

fn fresh_invocation_id() -> String {
    tool_api::util::ids::ulid_or_uuid()
}

fn verified(s: impl Into<String>) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.into()).into_inner())
}

/// `EnterPlanModeTool` — enters plan mode and returns native confirmation text.
pub struct EnterPlanModeTool {
    ctx: BuiltinToolContext,
}

impl EnterPlanModeTool {
    /// Construct a new tool wired to the shared builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn emit_started(&self, invocation_id: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(ENTER_TOOL_NAME));
        self.ctx.bus.log_event(ENTER_PLAN_MODE_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(ENTER_TOOL_NAME));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(ENTER_PLAN_MODE_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, error_kind: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(ENTER_TOOL_NAME));
        md.insert("error_kind".into(), verified(error_kind));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(ENTER_PLAN_MODE_FAILED, md).await;
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        ENTER_TOOL_NAME
    }
    fn should_defer(&self) -> bool {
        true
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("switch to plan mode to design an approach before coding".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &EMPTY_INPUT_SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&ENTER_OUTPUT_SCHEMA)
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        PLAN_MODE_MAX_RESULT_SIZE_CHARS
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_open_world(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "EnterPlanMode toggles a session flag only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    /// 2.1.266 `d3t.description()` (`src_162329786.js` @3902685).
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Requests permission to enter plan mode for complex tasks requiring exploration and design"
            .into()
    }
    async fn prompt(&self, options: &PromptOptions) -> Utf16JsonProjection {
        render_enter_plan_mode_prompt(
            options.output_style_is_default.unwrap_or(true),
            options.plan_mode_hybrid_search.unwrap_or(true),
        )
        .into()
    }

    async fn call(
        &self,
        _input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = fresh_invocation_id();
        let started_at = Instant::now();
        self.emit_started(&invocation_id).await;

        // Agent-context guard — TS throws here (`EnterPlanModeTool.ts:78-80`):
        // plan mode is a user-interactive control that has no meaning inside a
        // spawned agent. Reject before touching session state.
        if ctx.agent_id.is_some() {
            let dur = started_at.elapsed().as_millis() as u64;
            self.emit_failed(&invocation_id, "agent_context", dur).await;
            return Err(ToolError::InvalidInput(
                ENTER_PLAN_MODE_AGENT_GUARD_MSG.into(),
            ));
        }

        let session = ctx.session.as_ref().ok_or_else(|| {
            ToolError::Internal(
                "EnterPlanMode: session not wired into ToolUseContext (M4-04 contract)".into(),
            )
        })?;
        let transition = session.lock().await.plan_mode_transition.clone();
        let transition_guard = transition.lock().await;
        if let Some(gate) = self.ctx.permission_gate.as_ref() {
            gate.set_permission_mode("plan")
                .await
                .map_err(|error| ToolError::PermissionDenied(error.into()))?;
        }
        {
            let mut guard = session.lock().await;
            if !guard.plan_mode {
                guard.plan_mode = true;
                guard.plan_mode_generation = guard.plan_mode_generation.wrapping_add(1);
                // A repeated Enter is successful but does not reset reminder
                // cadence or replace the session's saved pre-plan mode.
                guard.plan_reminder_shown = false;
                guard.plan_mode_exit_pending = false;
            }
        }
        drop(transition_guard);
        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, duration_ms).await;
        Ok(ToolCallResult {
            // claude-code EnterPlanModeTool result: `{message}` (the exploration
            // guidance — `EnterPlanModeTool.ts:96-125`). The model reads it via the
            // `model_content` channel (the prior `{marker, plan_mode, instructions}`
            // shape JSON-dumped to the model, since `instructions` is not in the
            // `tool_result_to_model_text` fallback chain). Plan mode is entered via
            // the session lock above — the marker/plan_mode flags were vestigial.
            data: tool_api::tool_trait::generated_builtin_result(
                &json!({ "message": ENTER_PLAN_MODE_CONFIRMATION }),
            ),
            model_content: Some(ENTER_PLAN_MODE_INSTRUCTIONS.into()),
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

/// `ExitPlanModeTool` — leaves plan mode after the enforcing approval gate.
pub struct ExitPlanModeTool {
    ctx: BuiltinToolContext,
}

impl ExitPlanModeTool {
    /// Construct a new tool wired to the shared builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn emit_started(&self, invocation_id: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        self.ctx.bus.log_event(EXIT_PLAN_MODE_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(EXIT_PLAN_MODE_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, error_kind: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("invocation_id".into(), verified(invocation_id));
        md.insert("tool_name".into(), verified(EXIT_TOOL_NAME));
        md.insert("error_kind".into(), verified(error_kind));
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(EXIT_PLAN_MODE_FAILED, md).await;
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        EXIT_TOOL_NAME
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn requires_user_interaction(&self) -> bool {
        true
    }
    fn requires_user_interaction_for_context(&self, ctx: &ToolUseContext) -> bool {
        ctx.agent_id
            .as_ref()
            .and_then(lingxi_core::host::teammate_plan::requester)
            .is_none()
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("present plan for approval and start coding (plan mode only)".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &EXIT_INPUT_SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&EXIT_OUTPUT_SCHEMA)
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        PLAN_MODE_MAX_RESULT_SIZE_CHARS
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_open_world(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ExitPlanMode toggles a session flag only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    /// 2.1.266 `t6.description()` (`src_162329786.js` @3512544).
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Prompts the user to exit plan mode and start coding".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        // Full multi-section tool prompt (TS `ExitPlanModeV2Tool.ts:154-156`
        // returning `EXIT_PLAN_MODE_V2_TOOL_PROMPT`), not a one-line stub.
        EXIT_PLAN_MODE_V2_TOOL_PROMPT.into()
    }

    async fn call(
        &self,
        mut input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = fresh_invocation_id();
        let started_at = Instant::now();
        self.emit_started(&invocation_id).await;

        if let Some(requester) = ctx
            .agent_id
            .as_ref()
            .and_then(lingxi_core::host::teammate_plan::requester)
        {
            let mut data = match requester.submit(input).await {
                Ok(data) => data,
                Err(message) => {
                    self.emit_failed(
                        &invocation_id,
                        "leader_review_submission_failed",
                        started_at.elapsed().as_millis() as u64,
                    )
                    .await;
                    return Err(ToolError::InvalidInput(message.into()));
                }
            };
            let model_content = data.subprojection("/model_content").ok();
            let _ = data.remove_field("model_content");
            self.emit_completed(&invocation_id, started_at.elapsed().as_millis() as u64)
                .await;
            return Ok(ToolCallResult {
                model_content,
                ..ToolCallResult::from_data(data)
            });
        }

        let session = ctx.session.as_ref().ok_or_else(|| {
            ToolError::Internal(
                "ExitPlanMode: session not wired into ToolUseContext (M4-04 contract)".into(),
            )
        })?;

        // Validate the live plan-mode state before opening an approval dialog.
        // The second check below closes the small race where another operation
        // exits plan mode while this approval is waiting for the user.
        let plan_entry = {
            let guard = session.lock().await;
            guard
                .plan_mode
                .then_some((guard.session_id, guard.plan_mode_generation))
        };
        let Some(plan_entry) = plan_entry else {
            let dur = started_at.elapsed().as_millis() as u64;
            self.emit_failed(&invocation_id, "not_in_plan_mode", dur)
                .await;
            return Err(ToolError::InvalidInput(
                EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG.into(),
            ));
        };

        // ExitPlanMode owns a user-facing approval round-trip. The ordinary
        // dispatcher permission check cannot carry the plan body (and this
        // tool's policy result is AllowByDefault), so ask the live gate through
        // its dedicated plan-approval seam before mutating session state.
        let Some(gate) = self.ctx.permission_gate.as_ref() else {
            let dur = started_at.elapsed().as_millis() as u64;
            self.emit_failed(&invocation_id, "permission_unavailable", dur)
                .await;
            return Err(ToolError::PermissionDenied(
                EXIT_PLAN_MODE_PERMISSION_GATE_UNAVAILABLE_MSG.into(),
            ));
        };
        // 2.1.266 `t6.call`: the plan comes from the plan FILE
        // (`let D = I ?? await Kke(n.agentId, n.storageV5)`), because the tool
        // prompt tells the model to write it there and pass no argument. An
        // inline `plan` still wins — upstream gets one through
        // `normalizeToolInput`, and here it can also arrive as the gate's
        // `updated_input` when the user edits the plan in the approval dialog.
        let agent_id = ctx.agent_id.as_ref().map(ToString::to_string);
        let plan_file = self
            .ctx
            .permission_policy
            .plan_files
            .as_ref()
            .and_then(|files| files.plan_file(agent_id.as_deref()));
        let plan_on_disk = plan_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok());
        let plan_for_approval = input
            .string_at("/plan")
            .map(str::to_owned)
            .or_else(|| plan_on_disk.clone());
        let plan = plan_for_approval.as_deref().unwrap_or_default();
        let permission_ctx = lingxi_core::host::permission_gate::PermissionCheckContext {
            tool_use_id: ctx.tool_use_id.as_ref().map(ToString::to_string),
            is_agent_context: ctx.agent_id.is_some(),
            is_non_interactive_session: ctx.options.is_non_interactive_session,
            suppress_always_allow_rule: ctx.agent_id.is_some(),
            ..Default::default()
        };
        let outcome = gate.check_exit_plan_mode(plan, &permission_ctx).await;
        let mut approved_permission_updates = Vec::new();
        let mut auto_requested = false;
        match outcome {
            lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                updated_input,
                permission_updates,
                ..
            } => {
                approved_permission_updates = permission_updates;
                if let Some(updated) = updated_input {
                    input = updated;
                }
            }
            lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto { updated_input } => {
                auto_requested = true;
                if let Some(updated) = updated_input {
                    input = updated;
                }
            }
            lingxi_core::host::permission_gate::PermissionOutcome::Deny { reason } => {
                let dur = started_at.elapsed().as_millis() as u64;
                self.emit_failed(&invocation_id, "permission_denied", dur)
                    .await;
                return Err(ToolError::PermissionDenied(reason));
            }
        }

        let transition = session.lock().await.plan_mode_transition.clone();
        let transition_guard = transition.lock().await;
        {
            let guard = session.lock().await;
            if !guard.plan_mode {
                return Err(ToolError::InvalidInput(
                    EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG.into(),
                ));
            }
            if (guard.session_id, guard.plan_mode_generation) != plan_entry {
                return Err(ToolError::InvalidInput(
                    "This planning session changed while approval was pending. Review the current plan and call ExitPlanMode again.".into(),
                ));
            }
        }
        // Re-read AFTER the gate: `updated_input` may carry an edited plan, and
        // `planWasEdited` is precisely "an inline plan was present at call time"
        // (`I !== void 0`). An inline plan is persisted to the plan file first
        // (`Ykn`), so the echoed plan and the file on disk cannot disagree.
        let injected_plan_source = input
            .subprojection("/plan")
            .ok()
            .filter(|source| source.string_units("").is_some());
        let injected_plan = injected_plan_source
            .as_ref()
            .and_then(|source| source.string_at("").map(str::to_owned));
        if let (Some(text), Some(path)) = (injected_plan.as_deref(), plan_file.as_ref()) {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    ToolError::Internal(
                        format!(
                            "Could not save the approved plan to {}: {error}",
                            path.display()
                        )
                        .into(),
                    )
                })?;
            }
            tokio::fs::write(path, text).await.map_err(|error| {
                ToolError::Internal(
                    format!(
                        "Could not save the approved plan to {}: {error}",
                        path.display()
                    )
                    .into(),
                )
            })?;
        }
        let plan_text_source = injected_plan_source
            .clone()
            .or_else(|| plan_on_disk.as_deref().map(Utf16JsonProjection::from));
        if ctx.agent_id.is_none() {
            if !approved_permission_updates.is_empty() {
                gate.apply_permission_updates(&approved_permission_updates);
                gate.persist_permission_updates(&approved_permission_updates)
                    .await;
            }
            gate.exit_plan_mode_after_approval(auto_requested)
                .await
                .map_err(|error| ToolError::PermissionDenied(error.into()))?;
        }
        {
            let mut guard = session.lock().await;
            if !guard.plan_mode {
                drop(guard);
                let dur = started_at.elapsed().as_millis() as u64;
                self.emit_failed(&invocation_id, "not_in_plan_mode", dur)
                    .await;
                return Err(ToolError::InvalidInput(
                    EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG.into(),
                ));
            }
            guard.plan_mode = false;
            // 2.1.266 `NM(!0),Vz(!0)` on the `F.mode==="plan"` exit branch: one
            // `plan_mode_exit` reminder is owed, and the next plan-mode entry
            // that finds a plan file is a RE-entry.
            guard.plan_mode_exited = true;
            guard.plan_mode_exit_pending = true;
        }
        drop(transition_guard);
        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, duration_ms).await;

        // 2.1.266 output shape: `{plan, isAgent, filePath, hasTaskTool,
        // planWasEdited}`. `planWasEdited` is `I !== void 0` — true exactly when
        // the caller supplied the plan inline instead of leaving it on disk.
        let is_agent = ctx.agent_id.is_some();
        let plan_was_edited = injected_plan.is_some();
        let file_path = plan_file.as_ref().map(|p| p.to_string_lossy().into_owned());

        let has_task_tool = agent_tool_available(&ctx);
        let model_content = render_exit_plan_mode_result(
            plan_text_source.as_ref(),
            is_agent,
            file_path.as_deref(),
            has_task_tool,
            ctx.options.output_style_is_default.unwrap_or(true),
            plan_was_edited,
        );
        let mut data = vec![
            (
                "plan".to_string(),
                plan_text_source.unwrap_or_else(|| {
                    tool_api::tool_trait::generated_builtin_result(&Value::Null)
                }),
            ),
            (
                "isAgent".to_string(),
                tool_api::tool_trait::generated_builtin_result(&json!(is_agent)),
            ),
        ];
        if let Some(path) = file_path {
            data.push(("filePath".to_string(), Utf16JsonProjection::from(path)));
        }
        if has_task_tool {
            data.push((
                "hasTaskTool".to_string(),
                tool_api::tool_trait::generated_builtin_result(&json!(true)),
            ));
        }
        if plan_was_edited {
            data.push((
                "planWasEdited".to_string(),
                tool_api::tool_trait::generated_builtin_result(&json!(true)),
            ));
        }
        // Internal dispatch compatibility: source_tool_result_text extracts
        // this field before forwarding the result text to the provider.
        data.push(("model_content".to_string(), model_content));

        Ok(ToolCallResult {
            data: Utf16JsonProjection::object(data)
                .expect("ExitPlanMode result is a valid JSON object"),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_319_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/claude_2_1_319_plan.json"
        ))
        .expect("hash-pinned native Plan Mode oracle")
    }

    #[test]
    fn native_319_input_schema_wire_bytes() {
        let fixture = native_319_fixture();
        for (name, actual) in [
            ("EnterPlanMode", &*EMPTY_INPUT_SCHEMA),
            ("ExitPlanMode", &*EXIT_INPUT_SCHEMA),
        ] {
            assert_eq!(
                actual.to_json_string().unwrap(),
                serde_json::to_string(&fixture["inputSchemas"][name]).unwrap(),
                "{name}: preserve native schema field order and empty-object vocabulary"
            );
        }
    }

    #[test]
    fn native_319_prompt_branches_and_enter_result() {
        let fixture = native_319_fixture();
        for case in fixture["enterPrompts"].as_array().unwrap() {
            assert_eq!(
                render_enter_plan_mode_prompt(
                    case["outputStyleIsDefault"].as_bool().unwrap(),
                    case["hybridSearch"].as_bool().unwrap(),
                ),
                case["expected"].as_str().unwrap()
            );
        }
        assert_eq!(
            EXIT_PLAN_MODE_V2_TOOL_PROMPT,
            fixture["exitPrompt"].as_str().unwrap()
        );
        assert_eq!(
            ENTER_PLAN_MODE_INSTRUCTIONS,
            fixture["enterResult"].as_str().unwrap()
        );
    }

    #[test]
    fn native_319_exit_result_utf16_bytes() {
        let fixture = native_319_fixture();
        for case in fixture["exitResults"].as_array().unwrap() {
            let input = &case["input"];
            let plan = input["planUnits"].as_array().map(|units| {
                tool_api::tool_trait::source_text_projection_units(
                    &units
                        .iter()
                        .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
                        .collect::<Vec<_>>(),
                )
            });
            let actual = render_exit_plan_mode_result(
                plan.as_ref(),
                input["isAgent"].as_bool().unwrap(),
                input["filePath"].as_str(),
                input["hasTaskTool"].as_bool().unwrap_or(false),
                case["outputStyleIsDefault"].as_bool().unwrap(),
                input["planWasEdited"].as_bool().unwrap_or(false),
            );
            let expected = case["expectedUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                actual.string_units("").unwrap(),
                expected,
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn agent_guidance_uses_the_effective_tool_pool() {
        let mut ctx = fresh_ctx();
        assert!(!agent_tool_available(&ctx));
        ctx.available_tool_names = Some(Vec::new());
        assert!(!agent_tool_available(&ctx));
        ctx.available_tool_names = Some(vec!["mcp__Agent".into()]);
        assert!(!agent_tool_available(&ctx));
        for name in ["Agent", "Task"] {
            ctx.available_tool_names = Some(vec![name.into()]);
            assert!(agent_tool_available(&ctx));
        }
    }

    #[test]
    fn native_319_tool_metadata() {
        let (ctx, _, _, _) = make_ctx();
        let enter = EnterPlanModeTool::new(ctx.clone());
        let exit = ExitPlanModeTool::new(ctx);
        let input = source(json!({}));
        let fixture = native_319_fixture();
        for (name, tool) in [("enter", &enter as &dyn Tool), ("exit", &exit as &dyn Tool)] {
            assert_eq!(
                tool.is_read_only(&input),
                fixture["metadata"][name]["isReadOnly"].as_bool().unwrap()
            );
            assert_eq!(
                tool.is_concurrency_safe(&input),
                fixture["metadata"][name]["isConcurrencySafe"]
                    .as_bool()
                    .unwrap()
            );
            assert_eq!(
                tool.max_result_size_chars(),
                fixture["metadata"][name]["maxResultSizeChars"]
                    .as_u64()
                    .unwrap() as usize
            );
            assert_eq!(
                tool.should_defer(),
                fixture["metadata"][name]["shouldDefer"].as_bool().unwrap()
            );
            assert_eq!(
                tool.requires_user_interaction(),
                fixture["metadata"][name]["requiresUserInteraction"]
                    .as_bool()
                    .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn plan_tools_follow_standard_undiscovered_and_discovered_wire_states() {
        use tool_api::defer::{DeferralState, ToolSearchMode};
        use tool_api::wire::{apply_defer_loading, tools_to_wire};

        let (ctx, _, _, _) = make_ctx();
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(EnterPlanModeTool::new(ctx.clone())),
            Arc::new(ExitPlanModeTool::new(ctx)),
        ];
        let baseline = tools_to_wire(&tools, &PromptOptions::default()).await;
        assert_eq!(baseline.len(), 2);
        assert!(baseline.iter().all(|definition| !definition.defer_loading));

        let mut standard = baseline.clone();
        apply_defer_loading(&mut standard, &tools, &DeferralState::disabled());
        assert_eq!(standard, baseline);

        let enabled = DeferralState::new(ToolSearchMode::Enabled, false);
        let mut undiscovered = baseline.clone();
        apply_defer_loading(&mut undiscovered, &tools, &enabled);
        assert!(undiscovered.is_empty());

        enabled.mark_loaded([ENTER_TOOL_NAME, EXIT_TOOL_NAME]);
        let mut discovered = baseline.clone();
        apply_defer_loading(&mut discovered, &tools, &enabled);
        assert_eq!(discovered.len(), 2);
        for (definition, original) in discovered.iter().zip(&baseline) {
            assert!(definition.defer_loading);
            assert_eq!(definition.name, original.name);
            assert_eq!(definition.input_schema, original.input_schema);
            assert_eq!(definition.description, original.description);
        }
    }

    struct MetadataOnlyTeammatePlanRequester;

    #[async_trait]
    impl lingxi_core::host::teammate_plan::TeammatePlanRequester for MetadataOnlyTeammatePlanRequester {
        async fn submit(
            &self,
            _: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
            unreachable!("metadata checks never submit a plan")
        }
    }

    #[test]
    fn exit_interaction_metadata_tracks_the_invocation_teammate_requester() {
        let (ctx, _, _, mut use_ctx) = make_ctx();
        let tool = ExitPlanModeTool::new(ctx);
        let fixture = native_319_fixture();
        assert_eq!(
            tool.requires_user_interaction_for_context(&use_ctx),
            fixture["metadata"]["exit"]["requiresUserInteraction"]
                .as_bool()
                .unwrap()
        );
        use_ctx.agent_id = Some(AgentId::new());
        assert!(
            tool.requires_user_interaction_for_context(&use_ctx),
            "ordinary agents do not gain a teammate review transport"
        );

        let teammate_id = AgentId::new();
        let requester: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
            Arc::new(MetadataOnlyTeammatePlanRequester);
        lingxi_core::host::teammate_plan::register(teammate_id, &requester);
        use_ctx.agent_id = Some(teammate_id);
        assert_eq!(
            tool.requires_user_interaction_for_context(&use_ctx),
            fixture["metadata"]["exit"]["requiresUserInteractionAsTeammate"]
                .as_bool()
                .unwrap()
        );
        use_ctx.agent_id = None;
        assert!(
            tool.requires_user_interaction_for_context(&use_ctx),
            "the same tool instance still requires main-session approval"
        );
        drop(requester);
        use_ctx.agent_id = Some(teammate_id);
        assert!(
            tool.requires_user_interaction_for_context(&use_ctx),
            "expired transports are not treated as registered capabilities"
        );
    }
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

    use lingxi_core::host::permission_gate::{
        PermissionCheckContext, PermissionDecision, PermissionGate, PermissionOutcome,
    };
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{AgentId, SessionId};
    use lingxi_core::SessionState;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tokio::sync::Mutex;
    use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};

    fn source(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).expect("test value is valid JSON")
    }

    fn as_value(source: &Utf16JsonProjection) -> Value {
        serde_json::from_str(&source.to_json_string().expect("test source serializes"))
            .expect("test source is valid JSON")
    }

    fn model_content(content: Option<&Utf16JsonProjection>) -> Option<String> {
        content.and_then(|content| content.string_at("").map(str::to_owned))
    }

    struct ScriptedExitGate {
        seen: Arc<
            StdMutex<
                Vec<(
                    String,
                    lingxi_core::host::permission_gate::PermissionCheckContext,
                )>,
            >,
        >,
        outcome: lingxi_core::host::permission_gate::PermissionOutcome,
    }

    #[async_trait]
    impl lingxi_core::host::permission_gate::PermissionGate for ScriptedExitGate {
        async fn check(
            &self,
            _name: &str,
            _input: &Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            lingxi_core::host::permission_gate::PermissionDecision::Allow
        }

        async fn check_exit_plan_mode(
            &self,
            plan: &str,
            ctx: &lingxi_core::host::permission_gate::PermissionCheckContext,
        ) -> lingxi_core::host::permission_gate::PermissionOutcome {
            self.seen
                .lock()
                .unwrap()
                .push((plan.to_string(), ctx.clone()));
            self.outcome.clone()
        }
    }

    fn allowing_exit_gate() -> Arc<dyn lingxi_core::host::permission_gate::PermissionGate> {
        Arc::new(ScriptedExitGate {
            seen: Arc::new(StdMutex::new(Vec::new())),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                updated_input: None,
            },
        })
    }

    fn make_ctx() -> (
        BuiltinToolContext,
        Arc<InMemorySink>,
        Arc<Mutex<SessionState>>,
        tool_api::context::ToolUseContext,
    ) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut bctx = ctx_for_file_tools(make_dummy_fs(), bus.clone(), vec![std::env::temp_dir()]);
        // Normal success-path fixtures model a host with an approval-capable
        // gate; the explicit no-gate regression below removes it.
        bctx.permission_gate = Some(allowing_exit_gate());
        let session = Arc::new(Mutex::new(SessionState::empty(
            SessionId::nil(),
            "claude-opus-4-7".into(),
        )));
        let mut use_ctx = fresh_ctx();
        use_ctx.session = Some(session.clone());
        use_ctx.available_tool_names = Some(vec![Utf16JsonProjection::from("Agent")]);
        (bctx, sink, session, use_ctx)
    }

    /// A prompt transport with no policy shortcuts. The production policy gate
    /// still computes Auto eligibility, stages its return mode and restores it.
    struct PlanApprovalTransport {
        outcome: StdMutex<PermissionOutcome>,
    }

    impl PlanApprovalTransport {
        fn new(outcome: PermissionOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome: StdMutex::new(outcome),
            })
        }

        fn ordinary_allow() -> PermissionOutcome {
            PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            }
        }
    }

    #[async_trait]
    impl PermissionGate for PlanApprovalTransport {
        async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            PermissionDecision::Allow
        }

        async fn check_with_context(
            &self,
            name: &str,
            _: &Utf16JsonProjection,
            _: &PermissionCheckContext,
        ) -> PermissionOutcome {
            assert_eq!(name, EXIT_TOOL_NAME);
            self.outcome.lock().unwrap().clone()
        }
    }

    struct PauseAfterPlanExitGate {
        inner: Arc<permission::PolicyPermissionGate>,
        restored: tokio::sync::Notify,
        continue_exit: tokio::sync::Notify,
        mode_updates: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl PermissionGate for PauseAfterPlanExitGate {
        async fn check(&self, name: &str, input: &Utf16JsonProjection) -> PermissionDecision {
            self.inner.check(name, input).await
        }

        async fn check_exit_plan_mode(
            &self,
            plan: &str,
            ctx: &PermissionCheckContext,
        ) -> PermissionOutcome {
            self.inner.check_exit_plan_mode(plan, ctx).await
        }

        async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
            self.mode_updates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.set_permission_mode(mode).await
        }

        async fn exit_plan_mode_after_approval(&self, auto_requested: bool) -> Result<(), String> {
            self.inner
                .exit_plan_mode_after_approval(auto_requested)
                .await?;
            self.restored.notify_one();
            self.continue_exit.notified().await;
            Ok(())
        }

        fn permission_mode(&self) -> Option<String> {
            self.inner.permission_mode()
        }
    }

    #[tokio::test]
    async fn production_policy_gate_restores_pre_plan_mode_and_keeps_denial_in_plan() {
        let (mut bctx, _, session, mut use_ctx) = make_ctx();
        use_ctx.options.is_non_interactive_session = false;
        let transport = PlanApprovalTransport::new(PlanApprovalTransport::ordinary_allow());
        let policy = Arc::new(permission::PermissionPolicy::new(
            permission::PermissionMode::AcceptEdits,
        ));
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            policy.clone(),
            transport.clone(),
        ));
        bctx.permission_policy = policy;
        bctx.permission_gate = Some(gate.clone());
        let enter = EnterPlanModeTool::new(bctx.clone());
        let exit = ExitPlanModeTool::new(bctx);

        enter
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap();
        assert_eq!(gate.permission_mode().as_deref(), Some("plan"));
        assert!(session.lock().await.plan_mode);
        exit.call(
            source(json!({"plan":"Approved work"})),
            use_ctx.clone(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(gate.permission_mode().as_deref(), Some("acceptEdits"));
        assert!(!session.lock().await.plan_mode);

        *transport.outcome.lock().unwrap() = PermissionOutcome::Deny {
            reason: "Revise the plan first".into(),
        };
        enter
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap();
        let error = exit
            .call(
                source(json!({"plan":"Unapproved work"})),
                use_ctx,
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PermissionDenied(_)));
        assert_eq!(gate.permission_mode().as_deref(), Some("plan"));
        assert!(session.lock().await.plan_mode);
    }

    #[tokio::test]
    async fn failed_edited_plan_save_keeps_live_plan_until_auto_retry_succeeds() {
        let plans_dir = std::env::temp_dir().join(format!(
            "lingxi-plan-save-retry-{}",
            tool_api::util::ids::ulid_or_uuid()
        ));
        let slug = "blocked-save";
        let plan_path = plans_dir.join(format!("{slug}.md"));
        // A directory is a deterministic write failure on every supported host.
        std::fs::create_dir_all(&plan_path).unwrap();
        let (mut bctx, _, session, mut use_ctx) = make_ctx();
        use_ctx.options.is_non_interactive_session = false;
        let transport = PlanApprovalTransport::new(PermissionOutcome::AllowAuto {
            updated_input: Some(source(json!({"plan":"The user-edited approved plan"}))),
        });
        let policy = Arc::new(
            permission::PermissionPolicy::new(permission::PermissionMode::AcceptEdits)
                .with_plan_files(Arc::new(
                    permission::plan_files::PlanFileMatcher::with_identity(
                        permission::plan_files::PlanFileIdentity {
                            plans_dir: plans_dir.clone(),
                            slug: slug.to_owned(),
                            workshop_enabled: false,
                        },
                    ),
                )),
        );
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            policy.clone(),
            transport,
        ));
        assert!(gate.can_request_auto_mode());
        bctx.permission_policy = policy;
        bctx.permission_gate = Some(gate.clone());
        let enter = EnterPlanModeTool::new(bctx.clone());
        let exit = ExitPlanModeTool::new(bctx);
        enter
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap();

        let error = exit
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Internal(_)));
        assert_eq!(gate.permission_mode().as_deref(), Some("plan"));
        assert!(session.lock().await.plan_mode);
        assert!(plan_path.is_dir());

        std::fs::remove_dir(&plan_path).unwrap();
        exit.call(source(json!({})), use_ctx, fresh_tx())
            .await
            .unwrap();
        assert_eq!(gate.permission_mode().as_deref(), Some("auto"));
        assert!(!session.lock().await.plan_mode);
        assert_eq!(
            std::fs::read_to_string(&plan_path).unwrap(),
            "The user-edited approved plan"
        );
        std::fs::remove_dir_all(plans_dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_enter_waits_for_the_complete_exit_transition() {
        use std::future::Future;
        use std::sync::atomic::Ordering;
        use std::task::Poll;

        let (mut bctx, _, session, mut use_ctx) = make_ctx();
        use_ctx.options.is_non_interactive_session = false;
        let policy = Arc::new(permission::PermissionPolicy::new(
            permission::PermissionMode::AcceptEdits,
        ));
        let gate = Arc::new(PauseAfterPlanExitGate {
            inner: Arc::new(permission::PolicyPermissionGate::new(
                policy.clone(),
                PlanApprovalTransport::new(PlanApprovalTransport::ordinary_allow()),
            )),
            restored: tokio::sync::Notify::new(),
            continue_exit: tokio::sync::Notify::new(),
            mode_updates: std::sync::atomic::AtomicUsize::new(0),
        });
        bctx.permission_policy = policy;
        bctx.permission_gate = Some(gate.clone());
        let enter = EnterPlanModeTool::new(bctx.clone());
        let exit = ExitPlanModeTool::new(bctx);
        enter
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap();
        let exit_ctx = use_ctx.clone();
        let exiting = tokio::spawn(async move {
            exit.call(
                source(json!({"plan":"Approved work"})),
                exit_ctx,
                fresh_tx(),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.restored.notified())
            .await
            .unwrap();
        assert_eq!(gate.permission_mode().as_deref(), Some("acceptEdits"));
        assert!(session.lock().await.plan_mode);
        let transition = session.lock().await.plan_mode_transition.clone();
        assert!(
            transition.try_lock().is_err(),
            "the gate-to-session transition is still owned by Exit"
        );

        let entering = enter.call(source(json!({})), use_ctx, fresh_tx());
        tokio::pin!(entering);
        // Poll Enter into the known gate/bool window. With the lock it parks
        // before setMode; without it the native idempotent Enter completes here
        // and the resumed Exit would leave a Plan gate with plan_mode=false.
        std::future::poll_fn(|cx| {
            assert!(entering.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(gate.mode_updates.load(Ordering::SeqCst), 1);
        gate.continue_exit.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), exiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        entering.await.unwrap();
        assert_eq!(gate.mode_updates.load(Ordering::SeqCst), 2);
        assert_eq!(gate.permission_mode().as_deref(), Some("plan"));
        assert!(session.lock().await.plan_mode);
        assert!(!session.lock().await.plan_mode_exit_pending);
    }

    #[tokio::test]
    async fn ordinary_agent_exit_does_not_update_or_exit_the_root_permission_gate() {
        let (mut bctx, _, root_session, mut use_ctx) = make_ctx();
        use_ctx.options.is_non_interactive_session = false;
        let transport = PlanApprovalTransport::new(PermissionOutcome::Allow {
            updated_input: None,
            permission_updates: vec![source(json!({
                "type":"setMode", "mode":"acceptEdits", "destination":"session"
            }))],
            decision_classification: None,
        });
        let policy = Arc::new(permission::PermissionPolicy::new(
            permission::PermissionMode::AcceptEdits,
        ));
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            policy.clone(),
            transport,
        ));
        bctx.permission_policy = policy;
        bctx.permission_gate = Some(gate.clone());
        EnterPlanModeTool::new(bctx.clone())
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap();
        let child_session = Arc::new(Mutex::new(SessionState::empty(
            SessionId::new(),
            "claude-opus-4-7".to_owned(),
        )));
        child_session.lock().await.plan_mode = true;
        use_ctx.session = Some(child_session.clone());
        use_ctx.agent_id = Some(AgentId::new());
        let result = ExitPlanModeTool::new(bctx)
            .call(source(json!({"plan":"Child report"})), use_ctx, fresh_tx())
            .await
            .unwrap();
        assert_eq!(as_value(&result.data)["isAgent"], true);
        assert_eq!(gate.permission_mode().as_deref(), Some("plan"));
        assert!(root_session.lock().await.plan_mode);
        assert!(!child_session.lock().await.plan_mode);
    }

    struct PendingPlanApprovalTransport {
        requested: tokio::sync::Notify,
        continue_review: tokio::sync::Notify,
        pause_next: std::sync::atomic::AtomicBool,
        outcome: StdMutex<PermissionOutcome>,
    }

    #[async_trait]
    impl PermissionGate for PendingPlanApprovalTransport {
        async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            PermissionDecision::Allow
        }

        async fn check_with_context(
            &self,
            name: &str,
            _: &Utf16JsonProjection,
            _: &PermissionCheckContext,
        ) -> PermissionOutcome {
            assert_eq!(name, EXIT_TOOL_NAME);
            if self
                .pause_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.requested.notify_one();
                self.continue_review.notified().await;
            }
            self.outcome.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn stale_approval_cannot_save_or_leave_a_new_plan_generation() {
        let plans_dir = std::env::temp_dir().join(format!(
            "lingxi-stale-plan-approval-{}",
            tool_api::util::ids::ulid_or_uuid()
        ));
        std::fs::create_dir_all(&plans_dir).unwrap();
        let slug = "new-planning-cycle";
        let plan_path = plans_dir.join(format!("{slug}.md"));
        let (mut bctx, _, session, mut use_ctx) = make_ctx();
        use_ctx.options.is_non_interactive_session = false;
        let transport = Arc::new(PendingPlanApprovalTransport {
            requested: tokio::sync::Notify::new(),
            continue_review: tokio::sync::Notify::new(),
            pause_next: std::sync::atomic::AtomicBool::new(true),
            outcome: StdMutex::new(PermissionOutcome::AllowAuto {
                updated_input: Some(source(json!({"plan":"Old approved plan"}))),
            }),
        });
        let policy = Arc::new(
            permission::PermissionPolicy::new(permission::PermissionMode::AcceptEdits)
                .with_plan_files(Arc::new(
                    permission::plan_files::PlanFileMatcher::with_identity(
                        permission::plan_files::PlanFileIdentity {
                            plans_dir: plans_dir.clone(),
                            slug: slug.to_owned(),
                            workshop_enabled: false,
                        },
                    ),
                )),
        );
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            policy.clone(),
            transport.clone(),
        ));
        bctx.permission_policy = policy;
        bctx.permission_gate = Some(gate.clone());
        EnterPlanModeTool::new(bctx.clone())
            .call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .unwrap();
        let old_exit = ExitPlanModeTool::new(bctx.clone());
        let old_context = use_ctx.clone();
        let old_approval = tokio::spawn(async move {
            old_exit
                .call(source(json!({})), old_context, fresh_tx())
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transport.requested.notified(),
        )
        .await
        .unwrap();
        // Model the trusted host leaving and re-entering Plan while an older
        // approval card is open: the boolean is true in both generations.
        session.lock().await.plan_mode_generation += 1;
        transport.continue_review.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), old_approval)
            .await
            .unwrap()
            .unwrap();
        result.expect_err("an older approval cannot commit the new planning cycle");
        assert!(
            !plan_path.exists(),
            "reject the stale approval before saving its plan"
        );
        assert!(session.lock().await.plan_mode);
        assert_eq!(gate.permission_mode().as_deref(), Some("plan"));

        // Its old Auto choice must not alter the new cycle's return mode.
        *transport.outcome.lock().unwrap() = PlanApprovalTransport::ordinary_allow();
        ExitPlanModeTool::new(bctx)
            .call(
                source(json!({"plan":"Current approved plan"})),
                use_ctx,
                fresh_tx(),
            )
            .await
            .unwrap();
        assert!(!session.lock().await.plan_mode);
        assert_eq!(gate.permission_mode().as_deref(), Some("acceptEdits"));
        assert_eq!(
            std::fs::read_to_string(&plan_path).unwrap(),
            "Current approved plan"
        );
        std::fs::remove_dir_all(plans_dir).unwrap();
    }

    #[tokio::test]
    async fn enter_enters_mode_and_returns_native_result() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterPlanModeTool::new(bctx);
        assert!(!session.lock().await.plan_mode);
        let res = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect("enter must succeed on fresh session");
        // binary EnterPlanModeTool result = {message}; the model reads it via the
        // model_content channel. The marker/plan_mode flags are gone (plan mode is
        // entered via the session lock, asserted below).
        assert_eq!(as_value(&res.data)["message"], ENTER_PLAN_MODE_CONFIRMATION);
        assert!(as_value(&res.data).get("marker").is_none());
        assert!(as_value(&res.data).get("plan_mode").is_none());
        assert_eq!(
            model_content(res.model_content.as_ref()).as_deref(),
            Some(ENTER_PLAN_MODE_INSTRUCTIONS)
        );
        let instructions = res.model_content.as_ref().unwrap().string_at("").unwrap();
        assert!(instructions.starts_with("Entered plan mode."));
        assert!(instructions
            .contains("6. When ready, use ExitPlanMode to present your plan for approval"));
        assert!(instructions.contains("DO NOT write or edit any files yet"));
        assert!(session.lock().await.plan_mode);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_PLAN_MODE_STARTED.to_string()));
        assert!(names.contains(&ENTER_PLAN_MODE_COMPLETED.to_string()));
    }

    #[tokio::test]
    async fn enter_in_agent_context_rejects_with_locked_string() {
        let (bctx, sink, session, mut use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        use_ctx.agent_id = Some(AgentId::new());
        let tool = EnterPlanModeTool::new(bctx);
        let err = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect_err("enter in agent context must fail");
        assert_eq!(
            source_text(&err.model_facing_message()),
            "EnterPlanMode tool cannot be used in agent contexts"
        );
        // The guard runs before any session mutation.
        assert!(!session.lock().await.plan_mode);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_PLAN_MODE_STARTED.to_string()));
        assert!(names.contains(&ENTER_PLAN_MODE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn enter_twice_is_idempotent_and_keeps_reminder_cadence() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterPlanModeTool::new(bctx);
        tool.call(source(json!({})), use_ctx.clone(), fresh_tx())
            .await
            .expect("first enter must succeed");
        session.lock().await.plan_reminder_shown = true;
        let result = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect("second enter succeeds as in the native executable");
        assert_eq!(
            as_value(&result.data)["message"],
            ENTER_PLAN_MODE_CONFIRMATION
        );
        assert!(session.lock().await.plan_reminder_shown);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(!names.contains(&ENTER_PLAN_MODE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn exit_leaves_mode_and_returns_native_result() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true; // pre-arm
        let tool = ExitPlanModeTool::new(bctx);
        let res = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed when in plan mode");
        // 2.1.266 output shape: no marker, no `plan_mode` — `{plan, isAgent,
        // filePath, hasTaskTool, planWasEdited}`.
        assert!(as_value(&res.data).get("marker").is_none());
        assert!(as_value(&res.data).get("plan_mode").is_none());
        // No plan file and no inline plan → null plan, isAgent false.
        assert_eq!(as_value(&res.data)["plan"], Value::Null);
        assert_eq!(as_value(&res.data)["isAgent"], false);
        assert!(as_value(&res.data).get("planWasEdited").is_none());
        // Empty-plan branch (TS `ExitPlanModeV2Tool.ts:461-468`).
        assert_eq!(
            as_value(&res.data)["model_content"],
            EXIT_PLAN_APPROVED_EMPTY_MSG
        );
        assert_eq!(
            as_value(&res.data)["model_content"],
            "User has approved exiting plan mode. You can now proceed."
        );
        assert!(!session.lock().await.plan_mode);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_PLAN_MODE_COMPLETED.to_string()));
    }

    /// P0-2: the plan comes off DISK and the approved text is the 2.1.266
    /// template — opener, saved-to pair, teammate suffix, then the plan LAST.
    #[tokio::test]
    async fn exit_reads_the_plan_file_and_echoes_the_full_approved_text() {
        let plans_dir = std::env::temp_dir().join(format!(
            "lingxi-planfile-{}",
            tool_api::util::ids::ulid_or_uuid()
        ));
        std::fs::create_dir_all(&plans_dir).expect("plans dir");
        let slug = "brave-baking-otter";
        let plan_path = plans_dir.join(format!("{slug}.md"));
        std::fs::write(&plan_path, "## Context\nShip the port.").expect("seed plan");

        let (mut bctx, _sink, session, use_ctx) = make_ctx();
        let policy = permission::PermissionPolicy::new(permission::PermissionMode::Plan)
            .with_plan_files(std::sync::Arc::new(
                permission::plan_files::PlanFileMatcher::with_identity(
                    permission::plan_files::PlanFileIdentity {
                        plans_dir: plans_dir.clone(),
                        slug: slug.to_string(),
                        workshop_enabled: false,
                    },
                ),
            ));
        bctx.permission_policy = std::sync::Arc::new(policy);
        session.lock().await.plan_mode = true;

        let tool = ExitPlanModeTool::new(bctx);
        let res = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed");

        assert_eq!(as_value(&res.data)["plan"], "## Context\nShip the port.");
        assert_eq!(
            as_value(&res.data)["filePath"],
            plan_path.to_string_lossy().as_ref()
        );
        assert!(as_value(&res.data).get("planWasEdited").is_none());
        let expected = format!(
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable\n\nYour plan has been saved to: {}\nYou can refer back to it if needed during implementation.\n\nIf this plan can be broken down into multiple independent tasks, consider spawning named teammates with the Agent tool (pass a `name`) to parallelize the work.\n\n## Approved Plan:\n## Context\nShip the port.",
            plan_path.to_string_lossy()
        );
        assert_eq!(as_value(&res.data)["model_content"], expected);
        std::fs::remove_dir_all(&plans_dir).ok();
    }

    /// An inline plan wins over the file AND is written back to it, so the
    /// echoed plan and the file on disk cannot disagree (`Ykn`).
    #[tokio::test]
    async fn exit_persists_an_inline_plan_and_marks_it_edited() {
        let plans_dir = std::env::temp_dir().join(format!(
            "lingxi-planfile-{}",
            tool_api::util::ids::ulid_or_uuid()
        ));
        std::fs::create_dir_all(&plans_dir).expect("plans dir");
        let slug = "quiet-humming-otter";
        let plan_path = plans_dir.join(format!("{slug}.md"));
        std::fs::write(&plan_path, "stale").expect("seed plan");

        let (mut bctx, _sink, session, use_ctx) = make_ctx();
        bctx.permission_policy = std::sync::Arc::new(
            permission::PermissionPolicy::new(permission::PermissionMode::Plan).with_plan_files(
                std::sync::Arc::new(permission::plan_files::PlanFileMatcher::with_identity(
                    permission::plan_files::PlanFileIdentity {
                        plans_dir: plans_dir.clone(),
                        slug: slug.to_string(),
                        workshop_enabled: false,
                    },
                )),
            ),
        );
        session.lock().await.plan_mode = true;

        let tool = ExitPlanModeTool::new(bctx);
        let res = tool
            .call(source(json!({"plan": "fresh plan"})), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed");

        assert_eq!(as_value(&res.data)["plan"], "fresh plan");
        assert_eq!(as_value(&res.data)["planWasEdited"], true);
        assert_eq!(
            std::fs::read_to_string(&plan_path).expect("plan file"),
            "fresh plan",
            "an inline plan is persisted to the plan file"
        );
        assert!(as_value(&res.data)["model_content"]
            .as_str()
            .expect("model content")
            .ends_with("## Approved Plan (edited by user):\nfresh plan"));
        std::fs::remove_dir_all(&plans_dir).ok();
    }

    #[tokio::test]
    async fn exit_ignores_non_string_inline_plans_and_preserves_the_disk_plan() {
        let plans_dir = std::env::temp_dir().join(format!(
            "lingxi-plan-type-{}",
            tool_api::util::ids::ulid_or_uuid()
        ));
        std::fs::create_dir_all(&plans_dir).unwrap();
        let slug = "typed-inline-plan";
        let plan_path = plans_dir.join(format!("{slug}.md"));
        let disk_plan = "The existing disk plan survives unchanged";
        std::fs::write(&plan_path, disk_plan).unwrap();
        let (mut bctx, _, session, use_ctx) = make_ctx();
        bctx.permission_policy = Arc::new(
            permission::PermissionPolicy::new(permission::PermissionMode::Plan).with_plan_files(
                Arc::new(permission::plan_files::PlanFileMatcher::with_identity(
                    permission::plan_files::PlanFileIdentity {
                        plans_dir: plans_dir.clone(),
                        slug: slug.to_owned(),
                        workshop_enabled: false,
                    },
                )),
            ),
        );
        let exit = ExitPlanModeTool::new(bctx);
        for extra_plan in [
            json!(null),
            json!(false),
            json!(17),
            json!({"content":"ignored"}),
        ] {
            session.lock().await.plan_mode = true;
            let result = exit
                .call(
                    source(json!({"plan":extra_plan})),
                    use_ctx.clone(),
                    fresh_tx(),
                )
                .await
                .unwrap();
            assert_eq!(as_value(&result.data)["plan"], disk_plan);
            assert!(as_value(&result.data).get("planWasEdited").is_none());
            assert!(result
                .data
                .string_at("/model_content")
                .unwrap()
                .ends_with(&format!("## Approved Plan:\n{disk_plan}")));
            assert_eq!(std::fs::read_to_string(&plan_path).unwrap(), disk_plan);
        }

        // Empty text is still a real inline edit, unlike a null or non-string.
        session.lock().await.plan_mode = true;
        let result = exit
            .call(source(json!({"plan":""})), use_ctx, fresh_tx())
            .await
            .unwrap();
        assert_eq!(as_value(&result.data)["plan"], "");
        assert_eq!(as_value(&result.data)["planWasEdited"], true);
        assert_eq!(std::fs::read_to_string(&plan_path).unwrap(), "");
        std::fs::remove_dir_all(plans_dir).unwrap();
    }

    #[tokio::test]
    async fn exit_without_permission_gate_fails_closed_and_keeps_plan_mode() {
        let (mut bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        bctx.permission_gate = None;
        session.lock().await.plan_mode = true;
        let tool = ExitPlanModeTool::new(bctx);

        let error = tool
            .call(source(json!({ "plan": "1. Ship it" })), use_ctx, fresh_tx())
            .await
            .expect_err("ExitPlanMode must deny when no permission gate is wired");
        assert!(matches!(
            error,
            ToolError::PermissionDenied(message)
                if message.string_at("") == Some(EXIT_PLAN_MODE_PERMISSION_GATE_UNAVAILABLE_MSG)
        ));
        assert!(session.lock().await.plan_mode);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_PLAN_MODE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn exit_headless_permission_gate_denies_without_exiting() {
        let (mut bctx, sink, session, mut use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink).await;
        bctx.permission_gate = Some(Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(permission::PermissionPolicy::new(
                permission::PermissionMode::Default,
            )),
            Arc::new(permission::DenyOnAskGate),
        )));
        use_ctx.options.is_non_interactive_session = true;
        session.lock().await.plan_mode = true;
        let tool = ExitPlanModeTool::new(bctx);

        let error = tool
            .call(source(json!({ "plan": "1. Ship it" })), use_ctx, fresh_tx())
            .await
            .expect_err("headless ExitPlanMode must deny without a prompt");
        assert!(matches!(
            error,
            ToolError::PermissionDenied(message)
                if message
                    .string_at("")
                    .is_some_and(|reason| reason.starts_with("Permission to use ExitPlanMode has been denied."))
        ));
        assert!(session.lock().await.plan_mode);
    }

    #[test]
    fn exit_input_schema_accepts_allowed_prompts() {
        let schema = as_value(&EXIT_INPUT_SCHEMA);
        let props = &schema["properties"]["allowedPrompts"];
        assert_eq!(props["type"], "array");
        let item = &props["items"];
        assert_eq!(item["properties"]["tool"]["enum"], json!(["Bash"]));
        assert_eq!(item["properties"]["prompt"]["type"], "string");
        assert_eq!(item["required"], json!(["tool", "prompt"]));
        // passthrough() in TS → extra keys (plan/planFilePath) allowed.
        assert_eq!(schema["additionalProperties"], json!({}));
    }

    #[tokio::test]
    async fn exit_accepts_allowed_prompts_and_passes_through() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true; // pre-arm
        let tool = ExitPlanModeTool::new(bctx);
        let input = source(json!({
            "allowedPrompts": [{ "tool": "Bash", "prompt": "run tests" }],
            "plan": "Step 1. Do the thing.",
        }));
        let res = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect("exit must succeed with allowedPrompts");
        // `allowedPrompts` is accepted (deprecated passthrough) but is NOT part
        // of the 2.1.266 OUTPUT shape, so it is not echoed back.
        assert!(as_value(&res.data).get("allowedPrompts").is_none());
        // An inline plan still wins over the (absent) plan file.
        assert_eq!(as_value(&res.data)["plan"], "Step 1. Do the thing.");
        assert_eq!(as_value(&res.data)["isAgent"], false);
        assert_eq!(as_value(&res.data)["planWasEdited"], true);
        // No plan-file identity was published here, so the saved-to pair is
        // dropped rather than inventing a path; the rest is the 2.1.266 text.
        assert_eq!(
            as_value(&res.data)["model_content"],
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable\n\nIf this plan can be broken down into multiple independent tasks, consider spawning named teammates with the Agent tool (pass a `name`) to parallelize the work.\n\n## Approved Plan (edited by user):\nStep 1. Do the thing."
        );
        assert!(!session.lock().await.plan_mode);
    }

    #[tokio::test]
    async fn exit_in_agent_context_reports_is_agent() {
        let (bctx, sink, session, mut use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true; // pre-arm
        use_ctx.agent_id = Some(AgentId::new());
        let tool = ExitPlanModeTool::new(bctx);
        let res = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed in agent context");
        assert_eq!(as_value(&res.data)["isAgent"], true);
        assert_eq!(as_value(&res.data)["plan"], Value::Null);
        // Agent branch wins over plan state (TS `ExitPlanModeV2Tool.ts:452-459`).
        assert_eq!(
            as_value(&res.data)["model_content"],
            EXIT_PLAN_APPROVED_AGENT_MSG
        );
        assert_eq!(
            as_value(&res.data)["model_content"],
            "User has approved the plan. There is nothing else needed from you now. Please respond with \"ok\""
        );
    }

    #[tokio::test]
    async fn exit_without_enter_rejects_with_locked_string() {
        let (bctx, sink, _session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = ExitPlanModeTool::new(bctx);
        let err = tool
            .call(source(json!({})), use_ctx, fresh_tx())
            .await
            .expect_err("exit on fresh session must fail");
        // Out-of-plan-mode rejection now matches TS `validateInput`
        // (`ExitPlanModeV2Tool.ts:212-216`).
        assert_eq!(
            source_text(&err.model_facing_message()),
            format!("{EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG}")
        );
        assert_eq!(
            source_text(&err.model_facing_message()),
            "You are not in plan mode. To enter plan mode, call the EnterPlanMode tool first. If your plan was already approved, continue with implementation."
        );
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&EXIT_PLAN_MODE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn exit_prompt_returns_full_ts_tool_prompt() {
        let (bctx, _sink, _session, _use_ctx) = make_ctx();
        let tool = ExitPlanModeTool::new(bctx);
        let prompt = tool
            .prompt(&PromptOptions {
                prompt_policy: None,
                is_scheduled_turn: false,
                session_id: None,
                include_examples: true,
                model: None,
                model_profile: None,
                ..Default::default()
            })
            .await;
        let prompt = prompt.string_at("").expect("tool prompt is a root string");
        // Verbatim port of `EXIT_PLAN_MODE_V2_TOOL_PROMPT` (prompt.ts:6-29).
        assert_eq!(prompt, EXIT_PLAN_MODE_V2_TOOL_PROMPT);
        // Multi-section, not the old one-line stub.
        assert!(prompt.starts_with(
            "Use this tool when you are in plan mode and have finished writing your plan"
        ));
        assert!(prompt.contains("## How This Tool Works"));
        assert!(prompt.contains("## When to Use This Tool"));
        assert!(prompt.contains("## Before Using This Tool"));
        assert!(prompt.contains("## Examples"));
        // `${ASK_USER_QUESTION_TOOL_NAME}` was interpolated to "AskUserQuestion".
        assert!(prompt.contains("use AskUserQuestion first (in earlier phases)"));
        assert!(!prompt.contains("${ASK_USER_QUESTION_TOOL_NAME}"));
        // Trailing newline preserved from the TS template literal.
        assert!(prompt.ends_with("after clarifying the approach.\n"));
    }

    #[test]
    fn exit_model_content_branch_strings_match_ts() {
        // Byte-checks against the TS `mapToolResultToToolResultBlockParam` strings.
        assert_eq!(
            EXIT_PLAN_APPROVED_AGENT_MSG,
            "User has approved the plan. There is nothing else needed from you now. Please respond with \"ok\""
        );
        assert_eq!(
            EXIT_PLAN_APPROVED_EMPTY_MSG,
            "User has approved exiting plan mode. You can now proceed."
        );
        assert_eq!(
            EXIT_PLAN_APPROVED_PREFIX,
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable"
        );
    }

    #[tokio::test]
    async fn exit_whitespace_only_plan_uses_empty_branch() {
        // TS empty-plan guard is `!plan || plan.trim() === ''` — whitespace-only
        // plan text takes the empty branch, not the approved branch.
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true; // pre-arm
        let tool = ExitPlanModeTool::new(bctx);
        let res = tool
            .call(source(json!({ "plan": "   \n\t " })), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed with whitespace plan");
        assert_eq!(
            as_value(&res.data)["model_content"],
            EXIT_PLAN_APPROVED_EMPTY_MSG
        );
    }

    #[tokio::test]
    async fn exit_call_changes_plan_state_only_after_approved_auto_outcome() {
        let (mut bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink).await;
        session.lock().await.plan_mode = true;
        let seen = Arc::new(StdMutex::new(Vec::new()));
        bctx.permission_gate = Some(Arc::new(ScriptedExitGate {
            seen: seen.clone(),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::AllowAuto {
                updated_input: None,
            },
        }));
        let tool = ExitPlanModeTool::new(bctx);
        tool.call(source(json!({ "plan": "1. Ship it" })), use_ctx, fresh_tx())
            .await
            .expect("approved auto response exits plan mode");
        assert!(!session.lock().await.plan_mode);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "1. Ship it");
        assert!(seen[0].1.auto_mode_prompt.is_none());
    }

    #[tokio::test]
    async fn exit_call_keeps_plan_state_when_approval_is_denied() {
        let (mut bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink).await;
        session.lock().await.plan_mode = true;
        bctx.permission_gate = Some(Arc::new(ScriptedExitGate {
            seen: Arc::new(StdMutex::new(Vec::new())),
            outcome: lingxi_core::host::permission_gate::PermissionOutcome::Deny {
                reason: "user denied".into(),
            },
        }));
        let tool = ExitPlanModeTool::new(bctx);
        let error = tool
            .call(source(json!({ "plan": "1. Ship it" })), use_ctx, fresh_tx())
            .await
            .expect_err("denied approval must not exit plan mode");
        assert!(
            matches!(error, ToolError::PermissionDenied(message) if message.string_at("") == Some("user denied"))
        );
        assert!(session.lock().await.plan_mode);
    }
    struct RegisteredReviewRequester;
    #[async_trait]
    impl lingxi_core::host::teammate_plan::TeammatePlanRequester for RegisteredReviewRequester {
        async fn submit(
            &self,
            _: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
            Ok(source(json!({
                "awaitingLeaderApproval": true,
                "requestId": "review-1",
                "model_content": "Your plan has been submitted to the team lead for approval."
            })))
        }
    }
    #[tokio::test]
    async fn teammate_review_uses_registered_requester_and_keeps_model_text_out_of_data() {
        let (mut bctx, sink, session, mut context) = make_ctx();
        bctx.permission_gate = None;
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true;
        let tool = ExitPlanModeTool::new(bctx);
        let id = AgentId::new();
        context.agent_id = Some(id);
        let requester: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
            Arc::new(RegisteredReviewRequester);
        lingxi_core::host::teammate_plan::register(id, &requester);
        let result = tool
            .call(source(json!({})), context, fresh_tx())
            .await
            .unwrap();
        assert_eq!(
            model_content(result.model_content.as_ref()).as_deref(),
            Some("Your plan has been submitted to the team lead for approval.")
        );
        assert_eq!(as_value(&result.data)["awaitingLeaderApproval"], true);
        assert!(as_value(&result.data).get("model_content").is_none());
        assert!(
            session.lock().await.plan_mode,
            "teammate review must not change leader session mode"
        );
        assert!(sink
            .events()
            .await
            .iter()
            .any(|event| event.name == EXIT_PLAN_MODE_COMPLETED));
    }
    /// Unlike the interactive ExitPlanMode fixture, this transport never
    /// supplies a user approval for ordinary file writes.
    struct UnattendedPlanGate;
    #[async_trait]
    impl lingxi_core::host::permission_gate::PermissionGate for UnattendedPlanGate {
        async fn check(
            &self,
            _: &str,
            _: &Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            lingxi_core::host::permission_gate::PermissionDecision::Deny {
                reason: "No unattended permission approval".into(),
            }
        }
    }

    struct DiskPlanRequester(String);
    #[async_trait]
    impl lingxi_core::host::teammate_plan::TeammatePlanRequester for DiskPlanRequester {
        fn writable_plan_path(&self) -> Option<&str> {
            Some(&self.0)
        }
        async fn submit(
            &self,
            _: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
            let plan = std::fs::read_to_string(&self.0)
                .map_err(|error| Utf16JsonProjection::from(error.to_string()))?;
            Ok(source(json!({
                "plan": plan,
                "awaitingLeaderApproval": true,
                "requestId": "disk-review-1",
                "model_content": "Your plan has been submitted to the team lead for approval.\n\nDo NOT proceed until you receive approval."
            })))
        }
    }
    struct PlanDiskWrite;
    #[async_trait]
    impl Tool for PlanDiskWrite {
        fn name(&self) -> &str {
            "Write"
        }
        fn input_schema(&self) -> &Utf16JsonProjection {
            &EXIT_INPUT_SCHEMA
        }
        fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
            ToolInputRepairMetadata::Known {
                schema: ToolInputRepairSchema::Advertised,
                schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1000
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &ToolUseContext,
        ) -> PermissionResult {
            PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "test filesystem write".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            }
        }
        async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
            Utf16JsonProjection::from("")
        }
        async fn call(
            &self,
            input: Utf16JsonProjection,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            std::fs::write(
                input.string_at("/file_path").unwrap(),
                input.string_at("/content").unwrap(),
            )
            .unwrap();
            Ok(ToolCallResult::from_data(source(json!({"written":true}))))
        }
    }
    #[tokio::test]
    async fn registry_plan_file_write_then_exit_keeps_submission_text_and_data() {
        use lingxi_core::host::ToolInvoker;
        let id = AgentId::new();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("lingxi-plan-invoker-{}", id.as_uuid()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("plan.md");
        let owner: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
            Arc::new(DiskPlanRequester(path.to_string_lossy().into_owned()));
        lingxi_core::host::teammate_plan::register(id, &owner);
        let (bctx, _, _, _) = make_ctx();
        let mut registry = tool_api::registry::ToolRegistry::new();
        registry.register_builtin(Arc::new(PlanDiskWrite));
        registry.register_builtin(Arc::new(ExitPlanModeTool::new(bctx)));
        let policy = permission::PermissionPolicy::new(permission::PermissionMode::Plan);
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(policy),
            Arc::new(UnattendedPlanGate),
        ));
        let invoker = tool_api::tool_invoker_impl::RegistryToolInvoker::new(Arc::new(registry))
            .with_gate(gate);
        let context = || lingxi_core::host::tool_invoker::SubagentInvocationContext {
            prompt_policy: None,
            available_tool_names: None,
            cancellation_token: lingxi_core::host::CancellationToken::new(),
            permission_pause_observer: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            parent_agent_id: Some(id),
            native_query_depth: None,
            origin_session_id: None,
            agent_name: Some("planner".into()),
            team_name: Some("team".into()),
            is_async: true,
            is_non_interactive_session: true,
            can_show_permission_prompts: false,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 1,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            mode_override: Some("plan".into()),
            request_source: None,
            frozen_command_denies: vec![],
        };
        invoker
            .invoke(
                "Write",
                source(json!({"file_path":path,"content":"Inspect, test, implement"})),
                context(),
            )
            .await
            .unwrap();
        assert!(invoker
            .invoke(
                "Write",
                source(json!({"file_path":root.join("other.md"),"content":"no"})),
                context()
            )
            .await
            .is_err());
        let result = invoker
            .invoke_detailed("ExitPlanMode", source(json!({})), context())
            .await
            .unwrap();
        assert_eq!(as_value(&result.data)["plan"], "Inspect, test, implement");
        assert_eq!(as_value(&result.data)["awaitingLeaderApproval"], true);
        assert!(as_value(&result.data).get("model_content").is_none());
        assert_eq!(
            model_content(result.model_content.as_ref()).as_deref(),
            Some(
                "Your plan has been submitted to the team lead for approval.\n\nDo NOT proceed until you receive approval."
            )
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
