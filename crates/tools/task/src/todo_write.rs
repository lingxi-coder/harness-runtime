//! `TodoWriteTool` — persists a `Vec<TodoItem>` onto the running
//! `SessionState`. Spec §7 line 486 locks the three lifecycle states:
//! `pending` / `in_progress` / `completed`. The aliases `done` and `todo`
//! are NOT accepted.

use async_trait::async_trait;
use lingxi_core::{TodoItem, TodoState};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Wire literal for the `pending` lifecycle state. Spec §7 line 486.
pub const TODO_STATE_PENDING: &str = "pending";
/// Wire literal for the `in_progress` lifecycle state.
pub const TODO_STATE_IN_PROGRESS: &str = "in_progress";
/// Wire literal for the `completed` lifecycle state.
pub const TODO_STATE_COMPLETED: &str = "completed";
/// All three locked states in spec-declared order.
pub const TODO_STATES: &[&str] = &[
    TODO_STATE_PENDING,
    TODO_STATE_IN_PROGRESS,
    TODO_STATE_COMPLETED,
];
/// Canonical tool name in the registry.
pub const TOOL_NAME: &str = "TodoWrite";

/// claude-code v2.1.183 `FWd` (binary offset 199291572): the SHORT
/// TodoWrite tool prompt selected by the `Dh` gate for new models
/// (claude-opus-4-8 / claude-fable-5-1 / claude-mythos-5-1) and whenever
/// `LINGXI_SIMPLE_SYSTEM_PROMPT` is env-truthy. Byte-exact.
pub const TODO_WRITE_PROMPT_SIMPLE: &str = "Create and update a task list for the current session. The list is rendered to the user as your working plan.\n\n- Each todo has `content`, `status` (\"pending\" | \"in_progress\" | \"completed\"), and `activeForm` (present-tense label shown while in progress).\n- Send the full list each call; it replaces the previous one.\n- Keep one item `in_progress` at a time and mark it `completed` when done.";

/// claude-code v2.1.183 `UWd` (binary offset 199292278): the long
/// standard TodoWrite tool prompt selected by the `Dh` gate for classic
/// models and whenever `LINGXI_SIMPLE_SYSTEM_PROMPT` is
/// env-defined-falsy. The single `${Ua}` substitution renders as the
/// `Edit` tool name (`Ua="Edit"`). Byte-exact: the binary template literal
/// (`UWd=` at offset 199292273) ends with `...successfully.\n` — a trailing
/// newline IS present (rendered length 9114 bytes), so the raw string closes
/// (`"#`) on the following line.
pub const TODO_WRITE_PROMPT_FULL: &str = r#"Use this tool to create and manage a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.
It also helps the user understand the progress of the task and overall progress of their requests.

## When to Use This Tool
Use this tool proactively in these scenarios:

1. Complex multi-step tasks - When a task requires 3 or more distinct steps or actions
2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations
3. User explicitly requests todo list - When the user directly asks you to use the todo list
4. User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)
5. After receiving new instructions - Immediately capture user requirements as todos
6. When you start working on a task - Mark it as in_progress BEFORE beginning work. Ideally you should only have one todo as in_progress at a time
7. After completing a task - Mark it as completed and add any new follow-up tasks discovered during implementation

## When NOT to Use This Tool

Skip using this tool when:
1. There is only a single, straightforward task
2. The task is trivial and tracking it provides no organizational benefit
3. The task can be completed in less than 3 trivial steps
4. The task is purely conversational or informational

NOTE that you should not use this tool if there is only one trivial task to do. In this case you are better off just doing the task directly.

## Examples of When to Use the Todo List

<example>
User: I want to add a dark mode toggle to the application settings. Make sure you run the tests and build when you're done!
Assistant: *Creates todo list with the following items:*
1. Creating dark mode toggle component in Settings page
2. Adding dark mode state management (context/store)
3. Implementing CSS-in-JS styles for dark theme
4. Updating existing components to support theme switching
5. Running tests and build process, addressing any failures or errors that occur
*Begins working on the first task*

<reasoning>
The assistant used the todo list because:
1. Adding dark mode is a multi-step feature requiring UI, state management, and styling changes
2. The user explicitly requested tests and build be run afterward
3. The assistant inferred that tests and build need to pass by adding "Ensure tests and build succeed" as the final task
</reasoning>
</example>

<example>
User: Help me rename the function getCwd to getCurrentWorkingDirectory across my project
Assistant: *Uses grep or search tools to locate all instances of getCwd in the codebase*
I've found 15 instances of 'getCwd' across 8 different files.
*Creates todo list with specific items for each file that needs updating*

<reasoning>
The assistant used the todo list because:
1. First, the assistant searched to understand the scope of the task
2. Upon finding multiple occurrences across different files, it determined this was a complex task with multiple steps
3. The todo list helps ensure every instance is tracked and updated systematically
4. This approach prevents missing any occurrences and maintains code consistency
</reasoning>
</example>


<example>
User: I need to implement these features for my e-commerce site: user registration, product catalog, shopping cart, and checkout flow.
Assistant: *Creates a todo list breaking down each feature into specific tasks based on the project architecture*
Let's start with user registration. This will involve creating database models, API endpoints, and frontend forms that integrate with your existing authentication system.

<reasoning>
The assistant used the todo list because:
1. The user provided multiple complex features to implement in a comma separated list
2. The todo list helps organize these large features into manageable tasks
3. This approach allows for tracking progress across the entire implementation
</reasoning>
</example>

<example>
User: Can you help optimize my React application? It's rendering slowly and has performance issues.
Assistant: *Reviews component structure, render patterns, state management, and data fetching*
After analyzing your codebase, I've identified several performance issues.
*Creates todo list with items like: 1) Implementing memoization for expensive calculations in ProductList, 2) Adding virtualization for long lists in Dashboard, 3) Optimizing image loading in Gallery component, 4) Fixing state update loops in ShoppingCart, 5) Reviewing bundle size and implementing code splitting*
Let's start by implementing memoization for the expensive calculations in your ProductList component.

<reasoning>
The assistant used the todo list because:
1. First, the assistant examined the codebase to identify specific performance issues
2. Based on this analysis, it identified multiple optimization opportunities
3. Performance optimization is a non-trivial task requiring multiple steps
4. The todo list helps methodically track improvements across different components
5. This systematic approach ensures all performance bottlenecks are addressed
</reasoning>
</example>

## Examples of When NOT to Use the Todo List

<example>
User: How do I print 'Hello World' in Python?
Assistant: In Python, you can print "Hello World" with this simple code:

python
print("Hello World")

This will output the text "Hello World" to the console when executed.

<reasoning>
The assistant did not use the todo list because this is a single, trivial task that can be completed in one step. There's no need to track multiple tasks or steps for such a straightforward request.
</reasoning>
</example>

<example>
User: What does the git status command do?
Assistant: The git status command shows the current state of your working directory and staging area. It displays which changes have been staged, which haven't, and which files aren't being tracked by Git. This is a fundamental command for understanding the current state of your repository before making commits.

<reasoning>
The assistant did not use the todo list because this is an informational request with no actual coding task to complete. The user is simply asking for an explanation, not for the assistant to perform multiple steps or tasks.
</reasoning>
</example>

<example>
User: Can you add a comment to the calculateTotal function to explain what it does?
Assistant: Sure, let me add a comment to the calculateTotal function to explain what it does.
* Uses the Edit tool to add a comment to the calculateTotal function *

<reasoning>
The assistant did not use the todo list because this is a single, straightforward task confined to one location in the code. Adding a comment doesn't require tracking multiple steps or systematic organization.
</reasoning>
</example>

<example>
User: Run npm install for me and tell me what happens.
Assistant: I'll run the npm install command for you.

*Executes: npm install*

The command completed successfully. Here's the output:
[Output of npm install command]

All dependencies have been installed according to your package.json file.

<reasoning>
The assistant did not use the todo list because this is a single command execution with immediate results. There are no multiple steps to track or organize, making the todo list unnecessary for this straightforward task.
</reasoning>
</example>

## Task States and Management

1. **Task States**: Use these states to track progress:
   - pending: Task not yet started
   - in_progress: Currently working on (limit to ONE task at a time)
   - completed: Task finished successfully

   **IMPORTANT**: Task descriptions must have two forms:
   - content: The imperative form describing what needs to be done (e.g., "Run tests", "Build the project")
   - activeForm: The present continuous form shown during execution (e.g., "Running tests", "Building the project")

2. **Task Management**:
   - Update task status in real-time as you work
   - Mark tasks complete IMMEDIATELY after finishing (don't batch completions)
   - Exactly ONE task must be in_progress at any time (not less, not more)
   - Complete current tasks before starting new ones
   - Remove tasks that are no longer relevant from the list entirely

3. **Task Completion Requirements**:
   - ONLY mark a task as completed when you have FULLY accomplished it
   - If you encounter errors, blockers, or cannot finish, keep the task as in_progress
   - When blocked, create a new task describing what needs to be resolved
   - Never mark a task as completed if:
     - Tests are failing
     - Implementation is partial
     - You encountered unresolved errors
     - You couldn't find necessary files or dependencies

4. **Task Breakdown**:
   - Create specific, actionable items
   - Break complex tasks into smaller, manageable steps
   - Use clear, descriptive task names
   - Always provide both forms:
     - content: "Fix authentication bug"
     - activeForm: "Fixing authentication bug"

When in doubt, use this tool. Being proactive with task management demonstrates attentiveness and ensures you complete all requirements successfully.
"#;

// The `Dh(model)` / `UWu(model)` / `dfe(model)` predicate triple is shared
// verbatim with the file tools (Read/Write/Edit/Glob/Grep), so it lives in
// `tool-api` as the single source of truth. See
// `tool_api::model_prompt_gate::dh_simple_system_prompt` (port of binary offset
// ~195159752); TodoWrite consumes it below.
use tool_api::dh_simple_system_prompt;

/// Port of claude-code `Xla(model)` = `Dh(model) ? FWd : UWd`
/// (`prompt({model:e}){return Xla(e)}`). Selects the SHORT `FWd`
/// ([`TODO_WRITE_PROMPT_SIMPLE`]) for new models (opus-4-8 / fable-5 /
/// mythos-5) and whenever `LINGXI_SIMPLE_SYSTEM_PROMPT` is env-truthy, and
/// the long `UWd` ([`TODO_WRITE_PROMPT_FULL`]) for classic models and whenever
/// the env override is defined-falsy. The session/subagent model is threaded
/// via [`PromptOptions::model`]; `None` mirrors `Dh(undefined)` → `UWd`.
#[must_use]
pub fn select_todo_write_prompt(model: Option<&str>) -> &'static str {
    if dh_simple_system_prompt(model) {
        TODO_WRITE_PROMPT_SIMPLE
    } else {
        TODO_WRITE_PROMPT_FULL
    }
}

/// Validate a `Vec<TodoItem>` for the TodoWrite contract.
///
/// Mirrors claude-code `TodoItemSchema` (zod `BWd`, v2.1.183), which only
/// requires `content` and `activeForm` to be non-empty (`.min(1, ...)`); there
/// is NO `.max()` on `content`, so long todos are accepted. TS does NOT reject a
/// list with multiple `in_progress` items — the single-in-progress convention
/// is advisory (surfaced in the prompt), never enforced — and TS input items
/// carry no `id`, so neither an id-uniqueness nor an in-progress-count check
/// exists. The emptiness messages are the verbatim zod strings.
///
/// # Errors
/// Returns a human-readable error string on the first rule violation.
pub fn validate_todos(todos: &[TodoItem]) -> Result<(), String> {
    for t in todos {
        if t.content.is_empty() {
            return Err("Content cannot be empty".into());
        }
        if t.active_form.is_empty() {
            return Err("Active form cannot be empty".into());
        }
    }
    Ok(())
}

/// Compute `(pending, in_progress, completed)` counts over a todo list.
#[must_use]
pub fn summary(todos: &[TodoItem]) -> (u32, u32, u32) {
    let mut pending = 0u32;
    let mut in_progress = 0u32;
    let mut completed = 0u32;
    for t in todos {
        match t.status {
            TodoState::Pending => pending += 1,
            TodoState::InProgress => in_progress += 1,
            TodoState::Completed => completed += 1,
        }
    }
    (pending, in_progress, completed)
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "description": "The updated todo list",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "minLength": 1 },
                        "status":  {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed"]
                        },
                        "activeForm": { "type": "string", "minLength": 1 }
                    },
                    "required": ["content", "status", "activeForm"]
                }
            }
        },
        "required": ["todos"],
        "additionalProperties": false
    })
});

/// `TodoWriteTool` — replaces the running session's `todos` list with the
/// caller-supplied list. Wire-locked by spec §7 line 486.
pub struct TodoWriteTool {
    ctx: BuiltinToolContext,
}

impl TodoWriteTool {
    /// Construct a new tool wired to the shared builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("manage the session task checklist")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        // claude-code 2.1.263 `isEnabled(){return!X_()&&OO()}`: the V1/V2 mutex
        // (TodoWrite is advertised only when the V2 Task tools are NOT), AND the
        // model gate that withdraws the whole family on the current model
        // generation. Both polarities of the mutex carry `OO()`, so the gate
        // hides TodoWrite rather than revealing it.
        crate::task::todo_write_enabled(ctx)
    }
    /// claude-code `userFacingName(){return ""}` — an EMPTY display name, not
    /// an absent one. Upstream reads it with `??` (`qo(e){return
    /// e.userFacingName?.()??e.name}`), so `""` survives and the tool renders
    /// with no label; only `undefined` falls back to the wire name.
    ///
    /// Nothing in this engine reads `user_facing_name` yet, so this is metadata
    /// correctness rather than an observable change — but leaving it `None`
    /// means the first consumer to appear silently labels TodoWrite where
    /// upstream shows nothing.
    fn user_facing_name(&self) -> Option<&str> {
        Some("")
    }
    /// claude-code `shouldDefer:!0`. This one IS observable: it is what puts
    /// TodoWrite behind Tool Search's deferral (`ToolRegistry::available_tools`
    /// → `DeferralState::wants_defer`), so an undeferred TodoWrite was being
    /// advertised up front where upstream makes the model discover it.
    fn should_defer(&self) -> bool {
        true
    }
    /// claude-code `strict:!0`. No production reader in this engine today;
    /// recorded so the runtime parser matches when one appears.
    fn strict(&self) -> bool {
        true
    }
    /// claude-code `maxResultSizeChars:1e5`. Observable: this is the truncation
    /// budget for the tool result, and the shared 30k default was cutting
    /// TodoWrite's output at less than a third of upstream's limit.
    fn max_result_size_chars(&self) -> usize {
        100_000
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
                reason: "TodoWrite mutates session state only; no external side-effect".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        // claude-code v2.1.183 `description(){return Qla}`.
        "Update the todo list for the current session. To be used proactively and often to track progress and pending tasks. Make sure that at least one task is in_progress at all times. Always provide both content (imperative) and activeForm (present continuous) for each task.".into()
    }
    async fn prompt(&self, opts: &PromptOptions) -> String {
        // claude-code v2.1.183 `prompt({model:e}){return Xla(e)}` where
        // `function Xla(e){return Dh(e)?FWd:UWd}`. `Dh(model)` selects the SHORT
        // `FWd` prompt for new models (opus-4-8 / fable-5 / mythos-5) and the
        // long `UWd` for classic models, gated up front by the
        // `LINGXI_SIMPLE_SYSTEM_PROMPT` env override. The session model is
        // threaded via `opts.model`; absent ⇒ `Dh(undefined)` → `UWd`.
        select_todo_write_prompt(opts.model.as_deref()).into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Parse `todos` from `input`. (claude's TodoWrite emits NO per-tool
        // telemetry — per-call success/error is recorded by the generic tool
        // dispatcher — so the port no longer fires tengu_tool_todo_write_*.)
        let raw_todos = match input.get("todos") {
            Some(v) => v.clone(),
            None => {
                return Err(ToolError::InvalidInput(
                    "TodoWrite: missing required 'todos' field".into(),
                ));
            }
        };
        let todos: Vec<TodoItem> = match serde_json::from_value(raw_todos) {
            Ok(v) => v,
            Err(e) => {
                return Err(ToolError::InvalidInput(format!(
                    "TodoWrite: invalid todos shape: {e}"
                )));
            }
        };

        if let Err(msg) = validate_todos(&todos) {
            return Err(ToolError::InvalidInput(msg));
        }

        // Mutate session. `TodoWriteTool.ts:69-70` — `allDone = todos.every(completed)`
        // and `newTodos = allDone ? [] : todos`: an all-completed write CLEARS the
        // stored list (the loop has exited; nothing is left to track), while the
        // result still reports the full `todos` the model sent (`newTodos: todos`,
        // `:99`). An empty write (`todos = []`) is vacuously all-completed, so it
        // also clears — matching TS `[].every(...) === true`.
        let all_done = todos.iter().all(|t| t.status == TodoState::Completed);
        let session = ctx.session.as_ref().ok_or_else(|| {
            ToolError::Internal(
                "TodoWrite: session not wired into ToolUseContext (M4-04 contract)".into(),
            )
        })?;
        // claude-code `call`: `o=n.todos[r]??[]` — snapshot the PRIOR stored
        // todos (`oldTodos`) BEFORE the overwrite, then `i=allDone?[]:e` is
        // written. The result reports `{oldTodos:o, newTodos:e}` where `e` is
        // the full list the model sent (not the cleared `i`).
        let old_todos: Vec<TodoItem> = {
            let mut guard = session.lock().await;
            let old = guard.todos.clone();
            if all_done {
                guard.todos.clear();
            } else {
                guard.todos.clone_from(&todos);
            }
            old
        };

        // Model-facing result text (`TodoWriteTool.ts`
        // `mapToolResultToToolResultBlockParam`): in claude-code v2.1.183 the
        // tool_result `content` is UNCONDITIONALLY the fixed base string
        // (verified at binary offset 199302445 — `content:"Todos have been
        // modified successfully. …"`). There is NO verification-nudge suffix:
        // grep of v2.1.183 for `spawn the verification agent`, `You just closed
        // out`, `tengu_hive_evidence`, `VERIFICATION_AGENT`, and
        // `verificationNudge` all return 0. The whole nudge feature is absent,
        // so neither the suffix nor a `verificationNudgeNeeded` field is emitted.
        // This string is carried as `ToolCallResult.model_content` (the dispatch
        // emits it verbatim as the tool's model text); `data` is pure metadata
        // `{oldTodos, newTodos}` — the binary's `outputSchema` (`sfp`).
        let content = String::from(
            "Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable",
        );

        Ok(ToolCallResult {
            data: json!({
                "oldTodos": old_todos,
                "newTodos": todos,
            }),
            model_content: Some(content),
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
    use lingxi_core::types::SessionId;
    use lingxi_core::SessionState;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tokio::sync::Mutex;
    use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};

    fn make_tool_and_session() -> (
        TodoWriteTool,
        Arc<InMemorySink>,
        Arc<Mutex<SessionState>>,
        tool_api::context::ToolUseContext,
    ) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let bctx = ctx_for_file_tools(make_dummy_fs(), bus.clone(), vec![std::env::temp_dir()]);
        // Attach sink synchronously by spawning a quick task is overkill —
        // tests use a tokio runtime so we can attach via .await in each test.
        let session = Arc::new(Mutex::new(SessionState::empty(
            SessionId::nil(),
            "claude-opus-4-7".into(),
        )));
        let mut use_ctx = fresh_ctx();
        use_ctx.session = Some(session.clone());
        (TodoWriteTool::new(bctx), sink, session, use_ctx)
    }

    /// claude-code's TodoWrite tool definition, field for field
    /// (`PWn=Tt({name:XS,searchHint:…,maxResultSizeChars:1e5,strict:!0,
    /// userFacingName(){return""},shouldDefer:!0,…})`).
    ///
    /// Two of these are observable and two are not, which is why they are
    /// pinned together: `should_defer` decides whether Tool Search defers the
    /// tool, `max_result_size_chars` is the result truncation budget, and the
    /// other two are metadata no consumer reads yet. A regression in the silent
    /// pair would otherwise be invisible until the first consumer appears.
    #[test]
    fn tool_definition_metadata_matches_the_oracle() {
        let (tool, _sink, _session, _ctx) = make_tool_and_session();
        assert_eq!(tool.name(), "TodoWrite");
        assert_eq!(
            tool.search_hint(),
            Some("manage the session task checklist")
        );
        assert_eq!(tool.max_result_size_chars(), 100_000);
        assert!(tool.should_defer(), "shouldDefer:!0");
        assert!(tool.strict(), "strict:!0");
        // An EMPTY name, not an absent one: upstream reads this with `??`, so
        // `""` renders as no label while `None` would fall back to "TodoWrite".
        assert_eq!(tool.user_facing_name(), Some(""));
    }

    #[test]
    fn state_literals_match_spec() {
        assert_eq!(TODO_STATE_PENDING, "pending");
        assert_eq!(TODO_STATE_IN_PROGRESS, "in_progress");
        assert_eq!(TODO_STATE_COMPLETED, "completed");
    }

    #[test]
    fn todo_states_array_has_locked_order() {
        assert_eq!(TODO_STATES, &["pending", "in_progress", "completed"]);
    }

    #[tokio::test]
    async fn description_matches_binary_qla() {
        // claude-code v2.1.183 `description(){return Qla}`.
        let (tool, _sink, _session, _use_ctx) = make_tool_and_session();
        let desc = tool
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert_eq!(
            desc,
            "Update the todo list for the current session. To be used proactively and often to track progress and pending tasks. Make sure that at least one task is in_progress at all times. Always provide both content (imperative) and activeForm (present continuous) for each task."
        );
    }

    #[test]
    fn todo_state_serialize_matches_lock() {
        assert_eq!(
            serde_json::to_string(&TodoState::Pending).unwrap(),
            r#""pending""#
        );
        assert_eq!(
            serde_json::to_string(&TodoState::InProgress).unwrap(),
            r#""in_progress""#
        );
        assert_eq!(
            serde_json::to_string(&TodoState::Completed).unwrap(),
            r#""completed""#
        );
    }

    #[test]
    fn rejects_done_alias() {
        let err =
            serde_json::from_str::<TodoState>(r#""done""#).expect_err("'done' must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("unknown variant"), "msg: {msg}");
        assert!(msg.contains("done"), "msg: {msg}");
    }

    #[test]
    fn rejects_todo_alias() {
        let err =
            serde_json::from_str::<TodoState>(r#""todo""#).expect_err("'todo' must be rejected");
        assert!(format!("{err}").contains("unknown variant"));
    }

    #[test]
    fn validate_accepts_well_formed_list() {
        let todos = vec![
            TodoItem {
                id: "a".into(),
                content: "x".into(),
                status: TodoState::Pending,
                active_form: "active".into(),
            },
            TodoItem {
                id: "b".into(),
                content: "y".into(),
                status: TodoState::InProgress,
                active_form: "active".into(),
            },
            TodoItem {
                id: "c".into(),
                content: "z".into(),
                status: TodoState::Completed,
                active_form: "active".into(),
            },
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_rejects_empty_content() {
        let todos = vec![TodoItem {
            id: "a".into(),
            content: String::new(),
            status: TodoState::Pending,
            active_form: "active".into(),
        }];
        let err = validate_todos(&todos).expect_err("empty content must reject");
        assert_eq!(err, "Content cannot be empty");
    }

    #[test]
    fn validate_accepts_long_content() {
        // claude-code zod `TodoItemSchema` (BWd) has no `.max()` on content —
        // long todos must be accepted (matches v2.1.183).
        let huge = "x".repeat(10_000);
        let todos = vec![TodoItem {
            id: "a".into(),
            content: huge,
            status: TodoState::Pending,
            active_form: "active".into(),
        }];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_accepts_duplicate_id() {
        // TS `TodoItemSchema` has no `id` concept, so there is no uniqueness
        // rule; id-less input deserializes to empty ids, which must not reject.
        let todos = vec![
            TodoItem {
                id: "dup".into(),
                content: "x".into(),
                status: TodoState::Pending,
                active_form: "active".into(),
            },
            TodoItem {
                id: "dup".into(),
                content: "y".into(),
                status: TodoState::Pending,
                active_form: "active".into(),
            },
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_accepts_two_in_progress() {
        // TS treats single-in-progress as advisory (prompt-only), never an
        // error — multiple `in_progress` items must be accepted.
        let todos = vec![
            TodoItem {
                id: "a".into(),
                content: "x".into(),
                status: TodoState::InProgress,
                active_form: "active".into(),
            },
            TodoItem {
                id: "b".into(),
                content: "y".into(),
                status: TodoState::InProgress,
                active_form: "active".into(),
            },
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_accepts_zero_in_progress() {
        let todos = vec![
            TodoItem {
                id: "a".into(),
                content: "x".into(),
                status: TodoState::Pending,
                active_form: "active".into(),
            },
            TodoItem {
                id: "b".into(),
                content: "y".into(),
                status: TodoState::Completed,
                active_form: "active".into(),
            },
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn summary_counts_each_state() {
        let todos = vec![
            TodoItem {
                id: "a".into(),
                content: "x".into(),
                status: TodoState::Pending,
                active_form: "active".into(),
            },
            TodoItem {
                id: "b".into(),
                content: "y".into(),
                status: TodoState::Pending,
                active_form: "active".into(),
            },
            TodoItem {
                id: "c".into(),
                content: "z".into(),
                status: TodoState::InProgress,
                active_form: "active".into(),
            },
            TodoItem {
                id: "d".into(),
                content: "w".into(),
                status: TodoState::Completed,
                active_form: "active".into(),
            },
        ];
        assert_eq!(summary(&todos), (2_u32, 1_u32, 1_u32));
    }

    #[tokio::test]
    async fn execute_persists_todos_to_session() {
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "id": "t1", "content": "first",  "status": "pending",     "activeForm": "Doing first"  },
                { "id": "t2", "content": "second", "status": "in_progress", "activeForm": "Doing second" },
                { "id": "t3", "content": "third",  "status": "completed",   "activeForm": "Doing third"  }
            ]
        });
        let res = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect("happy path");
        let s = session.lock().await;
        assert_eq!(s.todos.len(), 3);
        assert_eq!(s.todos[0].id, "t1");
        assert_eq!(s.todos[1].status, TodoState::InProgress);
        // `data` is the binary `{oldTodos, newTodos}` schema: oldTodos is the
        // pre-write snapshot (empty here — a fresh session), newTodos is the
        // full 3-item list the model sent.
        assert_eq!(res.data["oldTodos"].as_array().unwrap().len(), 0);
        assert_eq!(res.data["newTodos"].as_array().unwrap().len(), 3);
        assert!(
            res.data.get("content").is_none(),
            "content moved to model_content"
        );
        assert!(
            res.data.get("summary").is_none(),
            "summary dropped (not in binary)"
        );
        // Model text is the fixed base string carried as model_content.
        assert_eq!(
            res.model_content.as_deref(),
            Some("Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable")
        );
    }

    #[tokio::test]
    async fn execute_rejects_unknown_status_with_serde_message() {
        let (tool, sink, _session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "id": "t1", "content": "x", "status": "done" }
            ]
        });
        let err = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect_err("unknown variant must fail");
        let msg = format!("{err}");
        assert!(msg.contains("TodoWrite: invalid todos shape"), "msg: {msg}");
        assert!(msg.contains("unknown variant"), "msg: {msg}");
    }

    #[tokio::test]
    async fn execute_accepts_two_in_progress() {
        // TS does not reject multiple `in_progress`; the write succeeds and
        // both items persist (id-less input — TS items have no `id`).
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "content": "x", "status": "in_progress", "activeForm": "Doing x" },
                { "content": "y", "status": "in_progress", "activeForm": "Doing y" }
            ]
        });
        let res = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect("two in_progress is accepted");
        assert_eq!(res.data["newTodos"].as_array().unwrap().len(), 2);
        {
            let s = session.lock().await;
            assert_eq!(s.todos.len(), 2);
        }
    }

    #[tokio::test]
    async fn execute_accepts_id_less_input_and_omits_id_in_output() {
        // claude-code TodoItem input is `{ content, status, activeForm }` with
        // no `id`. Such input must deserialize and persist, and the OUTPUT
        // `todos` must not carry an `id` (TS `newTodos` items omit it).
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "content": "first",  "status": "pending",     "activeForm": "Doing first"  },
                { "content": "second", "status": "in_progress",  "activeForm": "Doing second" }
            ]
        });
        let res = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect("id-less input is accepted");
        {
            let s = session.lock().await;
            assert_eq!(s.todos.len(), 2);
            assert_eq!(s.todos[0].id, "", "id defaults to empty when absent");
        }
        let out = &res.data["newTodos"];
        for item in out.as_array().expect("newTodos array") {
            assert!(
                item.get("id").is_none(),
                "OUTPUT must not leak an id: {item}"
            );
        }
    }

    #[test]
    fn validate_rejects_empty_active_form() {
        // claude-code TodoItemSchema requires `activeForm` non-empty (min(1)).
        let todos = vec![TodoItem {
            id: "a".into(),
            content: "x".into(),
            status: TodoState::Pending,
            active_form: String::new(),
        }];
        let err = validate_todos(&todos).expect_err("empty activeForm must reject");
        assert_eq!(err, "Active form cannot be empty");
    }

    #[tokio::test]
    async fn execute_round_trips_active_form() {
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "id": "t1", "content": "ship it", "status": "in_progress", "activeForm": "Shipping it" }
            ]
        });
        tool.call(input, use_ctx, fresh_tx())
            .await
            .expect("activeForm happy path");
        let s = session.lock().await;
        assert_eq!(s.todos.len(), 1);
        assert_eq!(s.todos[0].active_form, "Shipping it");
    }

    // ── all-completed clears the stored list (TodoWriteTool.ts:70) ───────

    #[tokio::test]
    async fn all_completed_write_clears_session_but_reports_full_list() {
        // `newTodos = allDone ? [] : todos`: every-completed write clears the
        // stored list, while the result still reports the full list the model
        // sent (`newTodos: todos`).
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "id": "a", "content": "build", "status": "completed", "activeForm": "Building" },
                { "id": "b", "content": "ship",  "status": "completed", "activeForm": "Shipping" }
            ]
        });
        let res = tool.call(input, use_ctx, fresh_tx()).await.expect("ok");
        {
            let s = session.lock().await;
            assert!(
                s.todos.is_empty(),
                "all-completed write clears the stored todos"
            );
        }
        // The result still reports the full 2-item list as `newTodos` (the
        // model-sent `e`, not the cleared `i`).
        assert_eq!(res.data["newTodos"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn partial_write_retains_session_todos() {
        // Not all completed ⇒ the stored list is the full write (no clear).
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "id": "a", "content": "build", "status": "completed",   "activeForm": "Building" },
                { "id": "b", "content": "ship",  "status": "in_progress", "activeForm": "Shipping" }
            ]
        });
        tool.call(input, use_ctx, fresh_tx()).await.expect("ok");
        let s = session.lock().await;
        assert_eq!(
            s.todos.len(),
            2,
            "a not-all-completed write is stored verbatim"
        );
    }

    // ── oldTodos captures the PRE-WRITE snapshot (binary `o=n.todos[r]??[]`) ──

    #[tokio::test]
    async fn old_todos_is_pre_write_snapshot_across_two_writes() {
        // Binary `call`: `o=n.todos[r]??[]` reads the stored list BEFORE the
        // overwrite. A first write seeds the session; the second write's result
        // must report that seeded list as `oldTodos`, and its own list as
        // `newTodos`.
        let (tool, sink, _session, _use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;

        // First write — fresh session ⇒ oldTodos empty.
        let first = json!({
            "todos": [
                { "id": "a", "content": "first",  "status": "in_progress", "activeForm": "Doing first" }
            ]
        });
        let res1 = {
            let mut c = fresh_ctx();
            c.session = Some(_session.clone());
            tool.call(first, c, fresh_tx()).await.expect("first write")
        };
        assert_eq!(res1.data["oldTodos"].as_array().unwrap().len(), 0);
        assert_eq!(res1.data["newTodos"].as_array().unwrap().len(), 1);

        // Second write — oldTodos must be the 1-item list the first write stored.
        let second = json!({
            "todos": [
                { "id": "b", "content": "second", "status": "pending", "activeForm": "Doing second" },
                { "id": "c", "content": "third",  "status": "pending", "activeForm": "Doing third"  }
            ]
        });
        let res2 = {
            let mut c = fresh_ctx();
            c.session = Some(_session.clone());
            tool.call(second, c, fresh_tx())
                .await
                .expect("second write")
        };
        let old = res2.data["oldTodos"].as_array().expect("oldTodos array");
        assert_eq!(old.len(), 1, "oldTodos is the pre-write stored list");
        assert_eq!(old[0]["content"], "first");
        assert_eq!(res2.data["newTodos"].as_array().unwrap().len(), 2);
    }

    // ── result text is the BARE base string (finding #74) ────────────────
    //
    // claude-code v2.1.183 has NO verification-nudge feature: the TodoWrite
    // tool_result `content` is unconditionally the base string (binary offset
    // 199302445), and there is no `verificationNudgeNeeded` result field.
    // These tests pin the always-bare result and the absence of the field.

    const TODO_BASE: &str = "Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable";

    /// Markers from the removed (fabricated) verification-nudge suffix — must
    /// NEVER appear in the result text on any path.
    const NUDGE_MARKER: &str = "spawn the verification agent (subagent_type=\"verification\")";

    fn completed(id: &str, content: &str) -> Value {
        json!({ "id": id, "content": content, "status": "completed", "activeForm": content })
    }

    #[tokio::test]
    async fn result_is_bare_base_when_all_completed_3plus() {
        // The exact scenario that previously fired the (fabricated) nudge —
        // 3+ all-completed items on the main-thread interactive path. The
        // result must now be the bare base string with no suffix and no
        // `verificationNudgeNeeded` field.
        let (tool, sink, _session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                completed("1", "Implement parser"),
                completed("2", "Wire it up"),
                completed("3", "Write docs"),
            ]
        });
        let res = tool.call(input, use_ctx, fresh_tx()).await.expect("ok");
        // The base string is now the model text (`ToolCallResult.model_content`,
        // the binary's `mapToolResultToToolResultBlockParam` content), NOT a
        // `data` field. `data` is the pure `{oldTodos, newTodos}` metadata.
        assert_eq!(
            res.model_content.as_deref(),
            Some(TODO_BASE),
            "bare base string only"
        );
        assert!(
            !res.model_content.as_deref().unwrap().contains(NUDGE_MARKER),
            "no verification-nudge suffix"
        );
        assert!(res.data.get("content").is_none(), "content not in data");
        assert!(
            res.data.get("verificationNudgeNeeded").is_none(),
            "no verificationNudgeNeeded field in result data"
        );
    }

    #[tokio::test]
    async fn result_is_bare_base_for_fewer_than_three() {
        let (tool, sink, _session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [completed("1", "Implement"), completed("2", "Document")]
        });
        let res = tool.call(input, use_ctx, fresh_tx()).await.expect("ok");
        assert_eq!(res.model_content.as_deref(), Some(TODO_BASE));
        assert!(res.data.get("content").is_none());
        assert!(res.data.get("verificationNudgeNeeded").is_none());
    }

    #[tokio::test]
    async fn result_is_bare_base_for_subagent() {
        let (tool, sink, _session, mut use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        use_ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        let input = json!({
            "todos": [
                completed("1", "Implement parser"),
                completed("2", "Wire it up"),
                completed("3", "Write docs"),
            ]
        });
        let res = tool.call(input, use_ctx, fresh_tx()).await.expect("ok");
        assert_eq!(res.model_content.as_deref(), Some(TODO_BASE));
        assert!(res.data.get("content").is_none());
        assert!(res.data.get("verificationNudgeNeeded").is_none());
    }

    #[tokio::test]
    async fn result_is_bare_base_when_not_all_completed() {
        let (tool, sink, _session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                completed("1", "Implement parser"),
                completed("2", "Wire it up"),
                { "id": "3", "content": "Write docs", "status": "pending", "activeForm": "Writing docs" },
            ]
        });
        let res = tool.call(input, use_ctx, fresh_tx()).await.expect("ok");
        assert_eq!(res.model_content.as_deref(), Some(TODO_BASE));
        assert!(res.data.get("content").is_none());
        assert!(res.data.get("verificationNudgeNeeded").is_none());
    }

    // ── model-facing prompt() = Xla(model) = Dh(model) ? FWd : UWd (#71) ──
    //
    // All cases live in ONE test (no `serial_test` dep) so the parallel test
    // runner never has two bodies mutating the shared
    // `LINGXI_SIMPLE_SYSTEM_PROMPT` env var at once. The `EnvGuard`
    // restores the prior value on drop. No other test module reads this var.

    #[tokio::test]
    async fn prompt_selects_uwd_or_fwd_via_dh_env_gate() {
        let (tool, _sink, _session, _use_ctx) = make_tool_and_session();
        // No model threaded ⇒ `Dh(undefined)` is false ⇒ UWd (env override aside).
        let opts = PromptOptions {
            include_examples: false,
            model: None,
            model_profile: None,
        };
        let with_model = |m: &str| PromptOptions {
            include_examples: false,
            model: Some(m.to_string()),
            model_profile: None,
        };

        // Default (var unset): no model threaded ⇒ `Dh(undefined)` is false ⇒
        // the long UWd prompt — NOT the old custom 3-sentence blurb.
        {
            let _g = EnvGuard::clear("LINGXI_SIMPLE_SYSTEM_PROMPT");
            let p = tool.prompt(&opts).await;
            assert_eq!(p, TODO_WRITE_PROMPT_FULL);
            // Binary-locked head + tail of UWd.
            assert!(p.starts_with(
                "Use this tool to create and manage a structured task list for your current coding session."
            ));
            // UWd ends with `...successfully.\n` — the binary template literal
            // (UWd= @199292273) carries a trailing newline (rendered len 9114).
            assert!(p.ends_with(
                "Being proactive with task management demonstrates attentiveness and ensures you complete all requirements successfully.\n"
            ));
            // The `${Ua}` substitution renders the Edit tool name verbatim.
            assert!(p.contains("Uses the Edit tool to add a comment"));
            // The stale blurb (and its false `id` claim) is gone.
            assert!(!p.contains("Use TodoWrite to track work-in-progress"));
        }

        // Model-class gate (var unset). `Dh(model) = !UWu(model)` (FWu=false):
        // classic UWu models → UWd; new (opus-4-8 / fable-5 / mythos-5) → FWd.
        {
            let _g = EnvGuard::clear("LINGXI_SIMPLE_SYSTEM_PROMPT");
            // UWu == true ⇒ long UWd.
            for classic in [
                "claude-opus-4-7",
                "claude-opus-4-0",
                "claude-opus-4-1",
                "claude-opus-4-5",
                "claude-opus-4-6",
                "claude-3-5-sonnet-20241022",
                "claude-3-5-haiku-20241022",
                "claude-sonnet-4-5",
                "claude-haiku-4-5",
                "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
            ] {
                assert_eq!(
                    tool.prompt(&with_model(classic)).await,
                    TODO_WRITE_PROMPT_FULL,
                    "UWu classic {classic:?} ⇒ UWd"
                );
            }
            // UWu == false ⇒ short FWd. opus-4-8 is OUTSIDE UWu's 4-0..4-7 list.
            for new_model in ["claude-opus-4-8", "claude-fable-5-1", "claude-mythos-5-1"] {
                assert_eq!(
                    tool.prompt(&with_model(new_model)).await,
                    TODO_WRITE_PROMPT_SIMPLE,
                    "new model {new_model:?} ⇒ FWd"
                );
            }
            // `dfe` early-access (`-eap`) is never the standard prompt ⇒ FWd.
            for eap in ["claude-opus-4-7-eap", "claude-sonnet-4-5-eap[x]"] {
                assert_eq!(
                    tool.prompt(&with_model(eap)).await,
                    TODO_WRITE_PROMPT_SIMPLE,
                    "early-access {eap:?} ⇒ FWd"
                );
            }
        }

        // `LINGXI_SIMPLE_SYSTEM_PROMPT` truthy ⇒ the short FWd prompt
        // (overrides the model branch — fires even for a classic model).
        for truthy in ["1", "true", "yes", "on", " On "] {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", truthy);
            let p = tool.prompt(&with_model("claude-opus-4-7")).await;
            assert_eq!(p, TODO_WRITE_PROMPT_SIMPLE, "truthy {truthy:?} ⇒ FWd");
            assert!(p.starts_with(
                "Create and update a task list for the current session. The list is rendered to the user as your working plan."
            ));
            // FWd no longer (falsely) claims todos carry an `id` field.
            assert!(!p.contains("has id"));
        }

        // `LINGXI_SIMPLE_SYSTEM_PROMPT` defined-falsy ⇒ the long UWd prompt
        // (overrides the model branch — fires even for opus-4-8).
        for falsy in ["0", "false", "no", "off"] {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", falsy);
            let p = tool.prompt(&with_model("claude-opus-4-8")).await;
            assert_eq!(p, TODO_WRITE_PROMPT_FULL, "defined-falsy {falsy:?} ⇒ UWd");
        }

        // `if(!e) return false`: even with the env truthy, a MISSING model still
        // short-circuits to UWd (the binary's `Dh(undefined)` guard runs first).
        {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", "1");
            assert_eq!(tool.prompt(&opts).await, TODO_WRITE_PROMPT_FULL);
        }

        // Any other (non-truthy, non-defined-falsy) value falls through to the
        // model-class branch (here: classic ⇒ UWd; with no model ⇒ UWd).
        {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", "garbage");
            assert_eq!(
                tool.prompt(&with_model("claude-opus-4-7")).await,
                TODO_WRITE_PROMPT_FULL
            );
            assert_eq!(
                tool.prompt(&with_model("claude-opus-4-8")).await,
                TODO_WRITE_PROMPT_SIMPLE
            );
            assert_eq!(tool.prompt(&opts).await, TODO_WRITE_PROMPT_FULL);
        }

        // Selector helper agrees with the tool method.
        {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", "yes");
            assert_eq!(
                select_todo_write_prompt(Some("claude-opus-4-7")),
                TODO_WRITE_PROMPT_SIMPLE
            );
        }
        {
            let _g = EnvGuard::clear("LINGXI_SIMPLE_SYSTEM_PROMPT");
            assert_eq!(select_todo_write_prompt(None), TODO_WRITE_PROMPT_FULL);
            assert_eq!(
                select_todo_write_prompt(Some("claude-opus-4-8")),
                TODO_WRITE_PROMPT_SIMPLE
            );
            assert_eq!(
                select_todo_write_prompt(Some("claude-opus-4-7")),
                TODO_WRITE_PROMPT_FULL
            );
        }
    }

    /// Minimal scoped env guard: sets/clears a var for the test body and
    /// restores the prior value on drop.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, val);
            Self { key, prev }
        }
        fn clear(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
