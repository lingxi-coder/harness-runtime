//! `EnterPlanModeTool` + `ExitPlanModeTool` — flip `SessionState.plan_mode`
//! and emit the byte-locked markers `[PLAN MODE]` / `[EXIT PLAN MODE]`
//! (spec §7 line 487).
//!
//! ## Divergence from TS (parity Batch MISC.11 — close-parity, flag prominently)
//!
//! The TS reference (`EnterPlanModeTool.ts`, `ExitPlanModeV2Tool.ts`) implements
//! plan mode on top of a real permission-mode substrate: it sets
//! `toolPermissionContext.mode = 'plan'` via `applyPermissionUpdate`, runs the
//! classifier activation in `prepareContextForPlanMode`, reads the plan from disk
//! (`getPlan`/`getPlanFilePath`), and performs a plan-approval handoff
//! (`setAwaitingPlanApproval`, teammate mailbox). LingXi's `SessionState` has only
//! a `plan_mode: bool`, **not** a permission-mode enum — none of that machinery
//! has a Rust home. This batch lands only the guardable, headless-meaningful
//! pieces faithful to TS:
//!   * the agent-context guard (`EnterPlanMode` rejects when `ctx.agent_id` is set;
//!     TS throws at `EnterPlanModeTool.ts:78-80`),
//!   * the `Entered plan mode...` instruction block reaching the model
//!     (port of `mapToolResultToToolResultBlockParam` `:103-125`),
//!   * the `ExitPlanMode` `{allowedPrompts?:[{tool:"Bash", prompt}]}` input schema
//!     (`ExitPlanModeV2Tool.ts:64-89`) and the `{plan, isAgent, allowedPrompts}`
//!     output passthrough (`:110-120`). Rust has no on-disk plan store, so `plan`
//!     is whatever the model passed (or `null`).
//!
//! TS uses prose, not literal markers; LingXi keeps the byte-locked
//! `[PLAN MODE]` / `[EXIT PLAN MODE]` markers (LingXi fixture lock) **and** adds
//! the TS instruction prose alongside them.

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

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};
use tool_api::BuiltinToolContext;

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

/// Full model-facing tool prompt for `EnterPlanMode` — 2.1.266 `N8o()`
/// (`src_162329786.js` @3898812), with `${D8o()}` (@3898366) spliced in at
/// `## What Happens in Plan Mode`.
///
/// Interpolations resolved as LingXi renders them: `${As}` → `AskUserQuestion`,
/// `${iy}` → `ExitPlanMode`, `${fo}, ${eo}, and ${tt}` → `Glob, Grep, and Read`,
/// and `${e}` → `O8o()` = `" (use the Agent tool instead)"` (the default output
/// style). `D8o`'s `oS()&&ei()` bash-first fork takes the NON-bash-first branch,
/// the same choice `tools/shell/src/prompt.rs` already makes for the search
/// tools. Generated from the executable, not retyped:
/// `~/.claude/oracle-chunks/notes/plan-goal-2.1.266/resolve_enter_prompt.py`.
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
1. Thoroughly explore the codebase using Glob, Grep, and Read
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

/// `SSt(hasTaskTool)` (`src_162329786.js` @3509271) — appended to the approved
/// text when the Agent tool is available and the output style is `default`.
///
/// ## Divergence (reason)
///
/// Upstream computes `hasTaskTool = Kr() && tools.some(isAgentTool)`: the
/// teammates gate AND the Agent tool being in the advertised pool. In LingXi the
/// Agent tool's `is_enabled` is unconditionally `true` and implicit teammates
/// are always available, so both terms hold — the suffix is emitted
/// unconditionally. The `zx()==="default"` output-style term has no Rust home in
/// the tool layer; the default output style is assumed, the same assumption the
/// ported `EnterPlanMode` prompt already makes for `O8o()`.
const EXIT_PLAN_TEAMMATE_SUFFIX: &str = "\n\nIf this plan can be broken down into multiple independent tasks, consider spawning named teammates with the Agent tool (pass a `name`) to parallelize the work.";

/// Heading for the echoed plan — `## ${planWasEdited?"Approved Plan (edited by
/// user)":"Approved Plan"}:`.
const EXIT_PLAN_APPROVED_HEADING: &str = "Approved Plan";
/// The edited-by-user variant of [`EXIT_PLAN_APPROVED_HEADING`].
const EXIT_PLAN_APPROVED_HEADING_EDITED: &str = "Approved Plan (edited by user)";

/// Model-facing approval text emitted by `ExitPlanMode` in an agent context —
/// byte-faithful port of the `isAgent` branch of
/// `mapToolResultToToolResultBlockParam` (`ExitPlanModeV2Tool.ts:452-459`).
const EXIT_PLAN_APPROVED_AGENT_MSG: &str =
    "User has approved the plan. There is nothing else needed from you now. Please respond with \"ok\"";

/// Model-facing approval text when the plan is empty — byte-faithful port of the
/// empty-plan branch (`ExitPlanModeV2Tool.ts:461-468`).
const EXIT_PLAN_APPROVED_EMPTY_MSG: &str =
    "User has approved exiting plan mode. You can now proceed.";

/// Opening line of the model-facing approval text when a plan is present — port
/// of the approved branch (2.1.266 @3516828). The two file-path lines that
/// follow it are [`EXIT_PLAN_SAVED_TO_PREFIX`] / [`EXIT_PLAN_REFER_BACK_LINE`],
/// and the echoed plan section comes LAST, after the optional
/// [`EXIT_PLAN_TEAMMATE_SUFFIX`].
const EXIT_PLAN_APPROVED_PREFIX: &str =
    "User has approved your plan. You can now start coding. Start with updating your todo list if applicable";

/// Locked rejection string for calling `ExitPlanMode` outside plan mode —
/// byte-faithful to `validateInput` (`ExitPlanModeV2Tool.ts:212-216`).
const EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG: &str =
    "You are not in plan mode. To enter plan mode, call the EnterPlanMode tool first. If your plan was already approved, continue with implementation.";

/// A missing permission gate is never an approval. Hosts that cannot wire an
/// enforcing gate must fail closed instead of allowing `ExitPlanMode` to
/// mutate the session state without user approval.
const EXIT_PLAN_MODE_PERMISSION_GATE_UNAVAILABLE_MSG: &str =
    "ExitPlanMode permission gate is unavailable; refusing to exit plan mode.";

/// Canonical tool name in the registry for `EnterPlanModeTool`.
pub const ENTER_TOOL_NAME: &str = "EnterPlanMode";
/// Canonical tool name in the registry for `ExitPlanModeTool`.
pub const EXIT_TOOL_NAME: &str = "ExitPlanMode";

static EMPTY_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
});

/// `ExitPlanMode` input schema — port of 2.1.206 `Btd`
/// (`ExitPlanModeV2Tool.ts`): `E.strictObject({allowedPrompts:
/// E.array(Poy()).optional().describe("Deprecated: no longer used.")})
/// .passthrough()`. `allowedPrompts` is now a DEPRECATED field ("no longer
/// used." — was the old prompt-based-permission array); its `Poy()` item shape
/// `{tool:"Bash", prompt:string}` is retained byte-exact. The `.passthrough()`
/// (extra keys allowed so `normalizeToolInput` can inject `plan`/`planFilePath`)
/// maps to `additionalProperties: true` here.
// Mirrors the pre-existing `EMPTY_INPUT_SCHEMA` `once_cell::Lazy` style above;
// `allow` keeps this batch from adding a net-new pedantic warning while staying
// consistent with the surrounding code (a `LazyLock` migration is out of scope).
// `unknown_lints` keeps the pinned 1.82 toolchain (whose clippy predates the
// `non_std_lazy_statics` lint) from erroring on the allow below under `-D warnings`,
// while newer clippy still honors the allow.
#[allow(unknown_lints, clippy::non_std_lazy_statics)]
static EXIT_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "allowedPrompts": {
                "type": "array",
                "description": "Deprecated: no longer used.",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": {
                            "type": "string",
                            "enum": ["Bash"],
                            "description": "The tool this prompt applies to"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Semantic description of the action, e.g. \"run tests\", \"install dependencies\""
                        }
                    },
                    "required": ["tool", "prompt"],
                    "additionalProperties": false
                }
            }
        },
        "additionalProperties": true
    })
});

/// `F8o` (2.1.266 `src_162329786.js` @3902505) — `EnterPlanMode`'s output
/// schema, `c({message: s().describe(…)})`.
#[allow(unknown_lints, clippy::non_std_lazy_statics)]
static ENTER_OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Confirmation that plan mode was entered"
            }
        },
        "required": ["message"],
        "additionalProperties": true
    })
});

/// `a4o` (2.1.266 `src_162329786.js` @3511807) — `ExitPlanMode`'s output schema.
/// `plan` is `s().nullable()`; every other field but `isAgent` is `.optional()`.
/// Upstream's `c(...)` is a non-strict zod object, so unknown keys pass — which
/// is what lets LingXi's `model_content` transport key ride along.
#[allow(unknown_lints, clippy::non_std_lazy_statics)]
static EXIT_OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
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
    })
});

fn fresh_invocation_id() -> String {
    tool_api::util::ids::ulid_or_uuid()
}

fn verified(s: impl Into<String>) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.into()).into_inner())
}

/// `EnterPlanModeTool` — flips `session.plan_mode` from `false` → `true`.
/// Emits `[PLAN MODE]` in the result.
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
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("switch to plan mode to design an approach before coding")
    }
    fn input_schema(&self) -> &Value {
        &EMPTY_INPUT_SCHEMA
    }
    fn output_schema(&self) -> Option<&Value> {
        Some(&ENTER_OUTPUT_SCHEMA)
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
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
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Requests permission to enter plan mode for complex tasks requiring exploration and design"
            .into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        ENTER_PLAN_MODE_TOOL_PROMPT.into()
    }

    async fn call(
        &self,
        _input: Value,
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
        {
            let mut guard = session.lock().await;
            if guard.plan_mode {
                drop(guard);
                let dur = started_at.elapsed().as_millis() as u64;
                self.emit_failed(&invocation_id, "already_in_plan_mode", dur)
                    .await;
                return Err(ToolError::InvalidInput(
                    "EnterPlanMode: session is already in plan mode".into(),
                ));
            }
            guard.plan_mode = true;
            // Replay the FULL (206 `LU_`) plan-mode reminder on this fresh entry:
            // reset the full-vs-sparse tracker so the next turn injects `full`
            // before switching to `sparse`. Mirrors 206 `reminderType` being
            // "full" on the first plan-mode reminder after entry.
            guard.plan_reminder_shown = false;
        }
        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, duration_ms).await;
        Ok(ToolCallResult {
            // claude-code EnterPlanModeTool result: `{message}` (the exploration
            // guidance — `EnterPlanModeTool.ts:96-125`). The model reads it via the
            // `model_content` channel (the prior `{marker, plan_mode, instructions}`
            // shape JSON-dumped to the model, since `instructions` is not in the
            // `tool_result_to_model_text` fallback chain). Plan mode is entered via
            // the session lock above — the marker/plan_mode flags were vestigial.
            data: json!({ "message": ENTER_PLAN_MODE_INSTRUCTIONS }),
            model_content: Some(ENTER_PLAN_MODE_INSTRUCTIONS.to_string()),
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// `ExitPlanModeTool` — flips `session.plan_mode` from `true` → `false`.
/// Emits `[EXIT PLAN MODE]` in the result.
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
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("present plan for approval and start coding (plan mode only)")
    }
    fn input_schema(&self) -> &Value {
        &EXIT_INPUT_SCHEMA
    }
    fn output_schema(&self) -> Option<&Value> {
        Some(&EXIT_OUTPUT_SCHEMA)
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
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
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Prompts the user to exit plan mode and start coding".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        // Full multi-section tool prompt (TS `ExitPlanModeV2Tool.ts:154-156`
        // returning `EXIT_PLAN_MODE_V2_TOOL_PROMPT`), not a one-line stub.
        EXIT_PLAN_MODE_V2_TOOL_PROMPT.into()
    }

    async fn call(
        &self,
        mut input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = fresh_invocation_id();
        let started_at = Instant::now();
        self.emit_started(&invocation_id).await;

        if let Some(requester) = ctx
            .agent_id
            .as_ref()
            .and_then(platform_api::teammate_plan::requester)
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
                    return Err(ToolError::InvalidInput(message));
                }
            };
            let model_content = data
                .as_object_mut()
                .and_then(|object| object.remove("model_content"))
                .and_then(|value| value.as_str().map(str::to_owned));
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
        if !session.lock().await.plan_mode {
            let dur = started_at.elapsed().as_millis() as u64;
            self.emit_failed(&invocation_id, "not_in_plan_mode", dur)
                .await;
            return Err(ToolError::InvalidInput(
                EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG.into(),
            ));
        }

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
            .get("plan")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| plan_on_disk.clone());
        let plan = plan_for_approval.as_deref().unwrap_or_default();
        let permission_ctx = platform_api::permission_gate::PermissionCheckContext {
            tool_use_id: ctx.tool_use_id.as_ref().map(ToString::to_string),
            is_agent_context: ctx.agent_id.is_some(),
            is_non_interactive_session: ctx.options.is_non_interactive_session,
            suppress_always_allow_rule: ctx.agent_id.is_some(),
            ..Default::default()
        };
        let outcome = gate.check_exit_plan_mode(plan, &permission_ctx).await;
        match outcome {
            platform_api::permission_gate::PermissionOutcome::Allow { updated_input, .. }
            | platform_api::permission_gate::PermissionOutcome::AllowAuto { updated_input } => {
                if let Some(updated) = updated_input {
                    input = updated;
                }
            }
            platform_api::permission_gate::PermissionOutcome::Deny { reason } => {
                let dur = started_at.elapsed().as_millis() as u64;
                self.emit_failed(&invocation_id, "permission_denied", dur)
                    .await;
                return Err(ToolError::PermissionDenied(reason));
            }
        }

        // Re-read AFTER the gate: `updated_input` may carry an edited plan, and
        // `planWasEdited` is precisely "an inline plan was present at call time"
        // (`I !== void 0`). An inline plan is persisted to the plan file first
        // (`Ykn`), so the echoed plan and the file on disk cannot disagree.
        let injected_plan = input.get("plan").and_then(Value::as_str).map(str::to_owned);
        if let (Some(text), Some(path)) = (injected_plan.as_deref(), plan_file.as_ref()) {
            if let Some(parent) = path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    tracing::warn!(%error, path = %parent.display(), "failed to create plans directory");
                }
            }
            if let Err(error) = std::fs::write(path, text) {
                tracing::error!(%error, path = %path.display(), "failed to persist plan");
            }
        }
        let plan_text = injected_plan.clone().or(plan_on_disk);

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
        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, duration_ms).await;

        // 2.1.266 output shape: `{plan, isAgent, filePath, hasTaskTool,
        // planWasEdited}`. `planWasEdited` is `I !== void 0` — true exactly when
        // the caller supplied the plan inline instead of leaving it on disk.
        let is_agent = ctx.agent_id.is_some();
        let plan_was_edited = injected_plan.is_some();
        let file_path = plan_file.as_ref().map(|p| p.to_string_lossy().into_owned());

        // Model-facing prose, mirroring `mapToolResultToToolResultBlockParam`
        // (@3516500). Without this the orchestrator (`turn_loop.rs`
        // `tool_result_to_model_text`) would JSON-dump the whole `data` object to
        // the model. Branch order matches upstream: awaiting-leader (handled in
        // the teammate path above) → agent context → empty plan → approved plan.
        let model_content = if is_agent {
            EXIT_PLAN_APPROVED_AGENT_MSG.to_string()
        } else {
            let plan_body = plan_text.as_deref().unwrap_or("");
            if plan_body.trim().is_empty() {
                EXIT_PLAN_APPROVED_EMPTY_MSG.to_string()
            } else {
                let heading = if plan_was_edited {
                    EXIT_PLAN_APPROVED_HEADING_EDITED
                } else {
                    EXIT_PLAN_APPROVED_HEADING
                };
                // Upstream always has a file path; when no plan-file identity has
                // been published (tests, and hosts that never wired one) the two
                // saved-to lines are dropped rather than inventing a path.
                let saved_to = match file_path.as_deref() {
                    Some(path) => format!(
                        "\n\n{EXIT_PLAN_SAVED_TO_PREFIX}{path}\n{EXIT_PLAN_REFER_BACK_LINE}"
                    ),
                    None => String::new(),
                };
                format!(
                    "{EXIT_PLAN_APPROVED_PREFIX}{saved_to}{EXIT_PLAN_TEAMMATE_SUFFIX}\n\n## {heading}:\n{plan_body}"
                )
            }
        };

        Ok(ToolCallResult {
            data: json!({
                "plan": plan_text,
                "isAgent": is_agent,
                "filePath": file_path,
                "hasTaskTool": true,
                "planWasEdited": plan_was_edited,
                // Model-facing string preferred by the orchestrator over a JSON
                // dump of `data` (TS prose parity, EXITPLAN.1).
                "model_content": model_content,
            }),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::SessionState;
    use protocol::{AgentId, SessionId};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tokio::sync::Mutex;
    use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};

    struct ScriptedExitGate {
        seen: Arc<
            StdMutex<
                Vec<(
                    String,
                    platform_api::permission_gate::PermissionCheckContext,
                )>,
            >,
        >,
        outcome: platform_api::permission_gate::PermissionOutcome,
    }

    #[async_trait]
    impl platform_api::permission_gate::PermissionGate for ScriptedExitGate {
        async fn check(
            &self,
            _name: &str,
            _input: &Value,
        ) -> platform_api::permission_gate::PermissionDecision {
            platform_api::permission_gate::PermissionDecision::Allow
        }

        async fn check_exit_plan_mode(
            &self,
            plan: &str,
            ctx: &platform_api::permission_gate::PermissionCheckContext,
        ) -> platform_api::permission_gate::PermissionOutcome {
            self.seen
                .lock()
                .unwrap()
                .push((plan.to_string(), ctx.clone()));
            self.outcome.clone()
        }
    }

    fn allowing_exit_gate() -> Arc<dyn platform_api::permission_gate::PermissionGate> {
        Arc::new(ScriptedExitGate {
            seen: Arc::new(StdMutex::new(Vec::new())),
            outcome: platform_api::permission_gate::PermissionOutcome::AllowAuto {
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
        (bctx, sink, session, use_ctx)
    }

    #[tokio::test]
    async fn enter_flips_flag_and_returns_marker() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterPlanModeTool::new(bctx);
        assert!(!session.lock().await.plan_mode);
        let res = tool
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect("enter must succeed on fresh session");
        // binary EnterPlanModeTool result = {message}; the model reads it via the
        // model_content channel. The marker/plan_mode flags are gone (plan mode is
        // entered via the session lock, asserted below).
        assert_eq!(res.data["message"], ENTER_PLAN_MODE_INSTRUCTIONS);
        assert!(res.data.get("marker").is_none());
        assert!(res.data.get("plan_mode").is_none());
        assert_eq!(
            res.model_content.as_deref(),
            Some(ENTER_PLAN_MODE_INSTRUCTIONS)
        );
        let instructions = res.data["message"].as_str().unwrap();
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
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect_err("enter in agent context must fail");
        assert_eq!(
            format!("{err}"),
            "invalid input: EnterPlanMode tool cannot be used in agent contexts"
        );
        // The guard runs before any session mutation.
        assert!(!session.lock().await.plan_mode);
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_PLAN_MODE_STARTED.to_string()));
        assert!(names.contains(&ENTER_PLAN_MODE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn enter_twice_rejects_with_locked_string() {
        let (bctx, sink, _session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = EnterPlanModeTool::new(bctx);
        tool.call(json!({}), use_ctx.clone(), fresh_tx())
            .await
            .expect("first enter must succeed");
        let err = tool
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect_err("second enter must fail");
        assert_eq!(
            format!("{err}"),
            "invalid input: EnterPlanMode: session is already in plan mode"
        );
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&ENTER_PLAN_MODE_FAILED.to_string()));
    }

    #[tokio::test]
    async fn exit_flips_flag_and_returns_marker() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true; // pre-arm
        let tool = ExitPlanModeTool::new(bctx);
        let res = tool
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed when in plan mode");
        // 2.1.266 output shape: no marker, no `plan_mode` — `{plan, isAgent,
        // filePath, hasTaskTool, planWasEdited}`.
        assert!(res.data.get("marker").is_none());
        assert!(res.data.get("plan_mode").is_none());
        // No plan file and no inline plan → null plan, isAgent false.
        assert_eq!(res.data["plan"], Value::Null);
        assert_eq!(res.data["isAgent"], false);
        assert_eq!(res.data["planWasEdited"], false);
        // Empty-plan branch (TS `ExitPlanModeV2Tool.ts:461-468`).
        assert_eq!(res.data["model_content"], EXIT_PLAN_APPROVED_EMPTY_MSG);
        assert_eq!(
            res.data["model_content"],
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
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed");

        assert_eq!(res.data["plan"], "## Context\nShip the port.");
        assert_eq!(res.data["filePath"], plan_path.to_string_lossy().as_ref());
        assert_eq!(res.data["planWasEdited"], false);
        let expected = format!(
            "User has approved your plan. You can now start coding. Start with updating your todo list if applicable\n\nYour plan has been saved to: {}\nYou can refer back to it if needed during implementation.\n\nIf this plan can be broken down into multiple independent tasks, consider spawning named teammates with the Agent tool (pass a `name`) to parallelize the work.\n\n## Approved Plan:\n## Context\nShip the port.",
            plan_path.to_string_lossy()
        );
        assert_eq!(res.data["model_content"], expected);
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
            .call(json!({"plan": "fresh plan"}), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed");

        assert_eq!(res.data["plan"], "fresh plan");
        assert_eq!(res.data["planWasEdited"], true);
        assert_eq!(
            std::fs::read_to_string(&plan_path).expect("plan file"),
            "fresh plan",
            "an inline plan is persisted to the plan file"
        );
        assert!(res.data["model_content"]
            .as_str()
            .expect("model content")
            .ends_with("## Approved Plan (edited by user):\nfresh plan"));
        std::fs::remove_dir_all(&plans_dir).ok();
    }

    #[tokio::test]
    async fn exit_without_permission_gate_fails_closed_and_keeps_plan_mode() {
        let (mut bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        bctx.permission_gate = None;
        session.lock().await.plan_mode = true;
        let tool = ExitPlanModeTool::new(bctx);

        let error = tool
            .call(json!({ "plan": "1. Ship it" }), use_ctx, fresh_tx())
            .await
            .expect_err("ExitPlanMode must deny when no permission gate is wired");
        assert!(matches!(
            error,
            ToolError::PermissionDenied(message)
                if message == EXIT_PLAN_MODE_PERMISSION_GATE_UNAVAILABLE_MSG
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
            .call(json!({ "plan": "1. Ship it" }), use_ctx, fresh_tx())
            .await
            .expect_err("headless ExitPlanMode must deny without a prompt");
        assert!(matches!(
            error,
            ToolError::PermissionDenied(message)
                if message.starts_with("Permission to use ExitPlanMode has been denied.")
        ));
        assert!(session.lock().await.plan_mode);
    }

    #[test]
    fn exit_input_schema_accepts_allowed_prompts() {
        let schema = &*EXIT_INPUT_SCHEMA;
        let props = &schema["properties"]["allowedPrompts"];
        assert_eq!(props["type"], "array");
        let item = &props["items"];
        assert_eq!(item["properties"]["tool"]["enum"], json!(["Bash"]));
        assert_eq!(item["properties"]["prompt"]["type"], "string");
        assert_eq!(item["required"], json!(["tool", "prompt"]));
        // passthrough() in TS → extra keys (plan/planFilePath) allowed.
        assert_eq!(schema["additionalProperties"], true);
    }

    #[tokio::test]
    async fn exit_accepts_allowed_prompts_and_passes_through() {
        let (bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        session.lock().await.plan_mode = true; // pre-arm
        let tool = ExitPlanModeTool::new(bctx);
        let input = json!({
            "allowedPrompts": [{ "tool": "Bash", "prompt": "run tests" }],
            "plan": "Step 1. Do the thing.",
        });
        let res = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect("exit must succeed with allowedPrompts");
        // `allowedPrompts` is accepted (deprecated passthrough) but is NOT part
        // of the 2.1.266 OUTPUT shape, so it is not echoed back.
        assert!(res.data.get("allowedPrompts").is_none());
        // An inline plan still wins over the (absent) plan file.
        assert_eq!(res.data["plan"], "Step 1. Do the thing.");
        assert_eq!(res.data["isAgent"], false);
        assert_eq!(res.data["planWasEdited"], true);
        // No plan-file identity was published here, so the saved-to pair is
        // dropped rather than inventing a path; the rest is the 2.1.266 text.
        assert_eq!(
            res.data["model_content"],
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
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed in agent context");
        assert_eq!(res.data["isAgent"], true);
        assert_eq!(res.data["plan"], Value::Null);
        // Agent branch wins over plan state (TS `ExitPlanModeV2Tool.ts:452-459`).
        assert_eq!(res.data["model_content"], EXIT_PLAN_APPROVED_AGENT_MSG);
        assert_eq!(
            res.data["model_content"],
            "User has approved the plan. There is nothing else needed from you now. Please respond with \"ok\""
        );
    }

    #[tokio::test]
    async fn exit_without_enter_rejects_with_locked_string() {
        let (bctx, sink, _session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink.clone()).await;
        let tool = ExitPlanModeTool::new(bctx);
        let err = tool
            .call(json!({}), use_ctx, fresh_tx())
            .await
            .expect_err("exit on fresh session must fail");
        // Out-of-plan-mode rejection now matches TS `validateInput`
        // (`ExitPlanModeV2Tool.ts:212-216`).
        assert_eq!(
            format!("{err}"),
            format!("invalid input: {EXIT_PLAN_MODE_NOT_IN_PLAN_MODE_MSG}")
        );
        assert_eq!(
            format!("{err}"),
            "invalid input: You are not in plan mode. To enter plan mode, call the EnterPlanMode tool first. If your plan was already approved, continue with implementation."
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
                include_examples: true,
                model: None,
                model_profile: None,
            })
            .await;
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
            .call(json!({ "plan": "   \n\t " }), use_ctx, fresh_tx())
            .await
            .expect("exit must succeed with whitespace plan");
        assert_eq!(res.data["model_content"], EXIT_PLAN_APPROVED_EMPTY_MSG);
    }

    #[tokio::test]
    async fn exit_call_changes_plan_state_only_after_approved_auto_outcome() {
        let (mut bctx, sink, session, use_ctx) = make_ctx();
        bctx.bus.attach_sink(sink).await;
        session.lock().await.plan_mode = true;
        let seen = Arc::new(StdMutex::new(Vec::new()));
        bctx.permission_gate = Some(Arc::new(ScriptedExitGate {
            seen: seen.clone(),
            outcome: platform_api::permission_gate::PermissionOutcome::AllowAuto {
                updated_input: None,
            },
        }));
        let tool = ExitPlanModeTool::new(bctx);
        tool.call(json!({ "plan": "1. Ship it" }), use_ctx, fresh_tx())
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
            outcome: platform_api::permission_gate::PermissionOutcome::Deny {
                reason: "user denied".to_string(),
            },
        }));
        let tool = ExitPlanModeTool::new(bctx);
        let error = tool
            .call(json!({ "plan": "1. Ship it" }), use_ctx, fresh_tx())
            .await
            .expect_err("denied approval must not exit plan mode");
        assert!(matches!(error, ToolError::PermissionDenied(message) if message == "user denied"));
        assert!(session.lock().await.plan_mode);
    }
    struct RegisteredReviewRequester;
    #[async_trait]
    impl platform_api::teammate_plan::TeammatePlanRequester for RegisteredReviewRequester {
        async fn submit(&self, _: Value) -> Result<Value, String> {
            Ok(
                json!({"awaitingLeaderApproval":true,"requestId":"review-1","model_content":"Your plan has been submitted to the team lead for approval."}),
            )
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
        let requester: Arc<dyn platform_api::teammate_plan::TeammatePlanRequester> =
            Arc::new(RegisteredReviewRequester);
        platform_api::teammate_plan::register(id, &requester);
        let result = tool.call(json!({}), context, fresh_tx()).await.unwrap();
        assert_eq!(
            result.model_content.as_deref(),
            Some("Your plan has been submitted to the team lead for approval.")
        );
        assert_eq!(result.data["awaitingLeaderApproval"], true);
        assert!(result.data.get("model_content").is_none());
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
    impl platform_api::permission_gate::PermissionGate for UnattendedPlanGate {
        async fn check(
            &self,
            _: &str,
            _: &Value,
        ) -> platform_api::permission_gate::PermissionDecision {
            platform_api::permission_gate::PermissionDecision::Deny {
                reason: "No unattended permission approval".into(),
            }
        }
    }

    struct DiskPlanRequester(String);
    #[async_trait]
    impl platform_api::teammate_plan::TeammatePlanRequester for DiskPlanRequester {
        fn writable_plan_path(&self) -> Option<&str> {
            Some(&self.0)
        }
        async fn submit(&self, _: Value) -> Result<Value, String> {
            let plan = std::fs::read_to_string(&self.0).map_err(|error| error.to_string())?;
            Ok(
                json!({"plan":plan,"awaitingLeaderApproval":true,"requestId":"disk-review-1","model_content":"Your plan has been submitted to the team lead for approval.\n\nDo NOT proceed until you receive approval."}),
            )
        }
    }
    struct PlanDiskWrite;
    #[async_trait]
    impl Tool for PlanDiskWrite {
        fn name(&self) -> &str {
            "Write"
        }
        fn input_schema(&self) -> &Value {
            &EXIT_INPUT_SCHEMA
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1000
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            false
        }
        fn is_read_only(&self, _: &Value) -> bool {
            false
        }
        async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
            PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "test filesystem write".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            }
        }
        async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
            String::new()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            input: Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            std::fs::write(
                input["file_path"].as_str().unwrap(),
                input["content"].as_str().unwrap(),
            )
            .unwrap();
            Ok(ToolCallResult::from_data(json!({"written":true})))
        }
    }
    #[tokio::test]
    async fn registry_plan_file_write_then_exit_keeps_submission_text_and_data() {
        use platform_api::ToolInvoker;
        let id = AgentId::new();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("lingxi-plan-invoker-{}", id.as_uuid()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("plan.md");
        let owner: Arc<dyn platform_api::teammate_plan::TeammatePlanRequester> =
            Arc::new(DiskPlanRequester(path.to_string_lossy().into_owned()));
        platform_api::teammate_plan::register(id, &owner);
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
        let context = || platform_api::tool_invoker::SubagentInvocationContext {
            permission_pause_observer: None,
            tool_execution_policy: platform_api::tool_invoker::ToolExecutionPolicy::Ordinary,
            parent_agent_id: Some(id),
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
            mode_override: Some("plan".into()),
            request_source: None,
            frozen_command_denies: vec![],
        };
        invoker
            .invoke(
                "Write",
                json!({"file_path":path,"content":"Inspect, test, implement"}),
                context(),
            )
            .await
            .unwrap();
        assert!(invoker
            .invoke(
                "Write",
                json!({"file_path":root.join("other.md"),"content":"no"}),
                context()
            )
            .await
            .is_err());
        let result = invoker
            .invoke_detailed("ExitPlanMode", json!({}), context(), None)
            .await
            .unwrap();
        assert_eq!(result.data["plan"], "Inspect, test, implement");
        assert_eq!(result.data["awaitingLeaderApproval"], true);
        assert!(result.data.get("model_content").is_none());
        assert_eq!(result.model_content.as_deref(), Some("Your plan has been submitted to the team lead for approval.\n\nDo NOT proceed until you receive approval."));
        std::fs::remove_dir_all(root).unwrap();
    }
}
