//! `TodoWriteTool` — persists a `Vec<TodoItem>` onto the running
//! `SessionState`. Spec §7 line 486 locks the three lifecycle states:
//! `pending` / `in_progress` / `completed`. The aliases `done` and `todo`
//! are NOT accepted.

use async_trait::async_trait;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};
use lingxi_core::{TodoItem, TodoState};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_schema, source_text_projection, DescriptionOptions, InterruptBehavior,
    PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext,
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
pub const TODO_WRITE_PROMPT_SIMPLE: &str =
    include_str!("templates/claude-code-2.1.319/todo-write-lean.md");

/// claude-code v2.1.183 `UWd` (binary offset 199292278): the long
/// standard TodoWrite tool prompt selected by the `Dh` gate for classic
/// models and whenever `LINGXI_SIMPLE_SYSTEM_PROMPT` is
/// env-defined-falsy. The single `${Ua}` substitution renders as the
/// `Edit` tool name (`Ua="Edit"`). Byte-exact: the binary template literal
/// (`UWd=` at offset 199292273) ends with `...successfully.\n` — a trailing
/// newline IS present (rendered length 9114 bytes), so the raw string closes
/// (`"#`) on the following line.
pub const TODO_WRITE_PROMPT_FULL: &str =
    include_str!("templates/claude-code-2.1.319/todo-write-long.md");

/// Compatibility entry point for standalone callers without request options.
/// The builtin resolver selects Claude's native style or generic Lean.
#[must_use]
pub fn select_todo_write_prompt(model: Option<&str>) -> &'static str {
    todo_write_prompt(&PromptOptions {
        model: model.map(str::to_owned),
        ..PromptOptions::default()
    })
}

fn todo_write_prompt(options: &PromptOptions) -> &'static str {
    if tool_api::prompt_uses_lean(options) {
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
        if t.content
            .string_units("")
            .is_none_or(|units| units.is_empty())
        {
            return Err("Content cannot be empty".into());
        }
        if t.active_form
            .string_units("")
            .is_none_or(|units| units.is_empty())
        {
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

static SOURCE_INPUT_SCHEMA: Lazy<Utf16JsonProjection> =
    Lazy::new(|| generated_builtin_schema(&INPUT_SCHEMA));

fn parse_todos(input: &Utf16JsonProjection) -> Result<Vec<TodoItem>, ToolError> {
    let todos = input
        .get_property("todos")
        .filter(|todos| todos.array_len().is_some())
        .ok_or_else(|| invalid_tool_input("TodoWrite: missing or invalid 'todos' field"))?;
    let count = todos.array_len().expect("TodoWrite todos are an array");
    let mut parsed = Vec::with_capacity(count);
    for index in 0..count {
        let item = todos
            .get_index(index)
            .filter(|item| item.object_entries().is_some())
            .ok_or_else(|| {
                invalid_tool_input(format!("TodoWrite: todos[{index}] must be an object"))
            })?;
        let content = item
            .get_property("content")
            .and_then(JsonValueView::code_units)
            .ok_or_else(|| {
                invalid_tool_input(format!(
                    "TodoWrite: todos[{index}].content must be a string"
                ))
            })?;
        let active_form = item
            .get_property("activeForm")
            .and_then(JsonValueView::code_units)
            .ok_or_else(|| {
                invalid_tool_input(format!(
                    "TodoWrite: todos[{index}].activeForm must be a string"
                ))
            })?;
        let status = item
            .get_property("status")
            .and_then(JsonValueView::as_str)
            .ok_or_else(|| {
                invalid_tool_input(format!("TodoWrite: todos[{index}].status must be a string"))
            })?;
        let status = match status {
            TODO_STATE_PENDING => TodoState::Pending,
            TODO_STATE_IN_PROGRESS => TodoState::InProgress,
            TODO_STATE_COMPLETED => TodoState::Completed,
            _ => {
                return Err(invalid_tool_input(format!(
                    "TodoWrite: todos[{index}].status is invalid"
                )))
            }
        };
        parsed.push(TodoItem {
            id: String::new(),
            content: Utf16JsonProjection::root_string(
                String::from_utf16_lossy(content),
                content.to_vec(),
            )
            .expect("string content retains exact UTF-16"),
            status,
            active_form: Utf16JsonProjection::root_string(
                String::from_utf16_lossy(active_form),
                active_form.to_vec(),
            )
            .expect("string activeForm retains exact UTF-16"),
        });
    }
    Ok(parsed)
}

fn todos_result_projection(input: &Utf16JsonProjection) -> Utf16JsonProjection {
    let todos = input
        .get_property("todos")
        .expect("TodoWrite todos were validated");
    let count = todos.array_len().expect("TodoWrite todos are an array");
    let mut builder = Utf16JsonBuilder::new();
    let rows = (0..count)
        .map(|index| {
            let item = todos
                .get_index(index)
                .expect("TodoWrite todo was validated");
            let fields = ["content", "status", "activeForm"]
                .into_iter()
                .map(|field| {
                    let value = item
                        .get_property(field)
                        .expect("TodoWrite field was validated");
                    let id = builder
                        .import_view(value)
                        .expect("TodoWrite field is valid JSON");
                    (field.into(), id)
                })
                .collect();
            builder
                .push_object(fields)
                .expect("TodoWrite row is valid JSON")
        })
        .collect();
    let root = builder
        .push_array(rows)
        .expect("TodoWrite rows are valid JSON");
    builder
        .finish(root)
        .expect("TodoWrite todo list is valid JSON")
}

fn todo_items_projection(todos: &[TodoItem]) -> Utf16JsonProjection {
    let items = todos
        .iter()
        .map(|todo| {
            let mut fields = Vec::with_capacity(4);
            if !todo.id.is_empty() {
                fields.push(("id".into(), source_text_projection(&todo.id)));
            }
            fields.push(("content".into(), todo.content.clone()));
            fields.push((
                "status".into(),
                source_text_projection(match todo.status {
                    TodoState::Pending => TODO_STATE_PENDING,
                    TodoState::InProgress => TODO_STATE_IN_PROGRESS,
                    TodoState::Completed => TODO_STATE_COMPLETED,
                }),
            ));
            fields.push(("activeForm".into(), todo.active_form.clone()));
            Utf16JsonProjection::object(fields).expect("TodoWrite item is valid JSON")
        })
        .collect();
    Utf16JsonProjection::array(items).expect("TodoWrite list is valid JSON")
}

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
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("manage the session task checklist".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &SOURCE_INPUT_SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
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
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
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
                reason: "TodoWrite mutates session state only; no external side-effect".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        // claude-code v2.1.183 `description(){return Qla}`.
        "Update the todo list for the current session. To be used proactively and often to track progress and pending tasks. Make sure that at least one task is in_progress at all times. Always provide both content (imperative) and activeForm (present continuous) for each task.".into()
    }
    async fn prompt(&self, opts: &PromptOptions) -> Utf16JsonProjection {
        todo_write_prompt(opts).into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Parse `todos` from `input`. (claude's TodoWrite emits NO per-tool
        // telemetry — per-call success/error is recorded by the generic tool
        // dispatcher — so the port no longer fires tengu_tool_todo_write_*.)
        let todos = parse_todos(&input)?;

        if let Err(msg) = validate_todos(&todos) {
            return Err(invalid_tool_input(msg));
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
            data: Utf16JsonProjection::object(vec![
                ("oldTodos".into(), todo_items_projection(&old_todos)),
                ("newTodos".into(), todos_result_projection(&input)),
            ])
            .expect("TodoWrite result is valid JSON"),
            model_content: Some(source_text_projection(&content)),
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

#[cfg(test)]
struct TestTodoWriteResult {
    data: Value,
    model_content: Option<String>,
}

#[cfg(test)]
impl TodoWriteTool {
    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<TestTodoWriteResult, ToolError> {
        let result = Tool::call(
            self,
            tool_api::tool_trait::generated_builtin_result(&input),
            ctx,
            progress,
        )
        .await?;
        let data = serde_json::from_str(
            &result
                .data
                .to_json_string()
                .expect("test result projection serializes"),
        )
        .expect("test result JSON parses");
        let model_content = result
            .model_content
            .as_ref()
            .and_then(|value| value.string_at(""))
            .map(str::to_owned);
        Ok(TestTodoWriteResult {
            data,
            model_content,
        })
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

    use lingxi_core::types::SessionId;
    use lingxi_core::SessionState;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tokio::sync::Mutex;
    use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};
    use tool_api::tool_trait::generated_builtin_result;

    fn todo(id: &str, content: &str, status: TodoState, active_form: &str) -> TodoItem {
        TodoItem {
            id: id.into(),
            content: source_text_projection(content),
            status,
            active_form: source_text_projection(active_form),
        }
    }

    fn source_text_units(units: &[u16]) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
            .expect("source test text is valid JSON")
    }

    #[test]
    fn borrowed_todo_parser_preserves_validation_order_and_sparse_hole_errors() {
        for (source, expected) in [
            (r#"{}"#, "TodoWrite: missing or invalid 'todos' field"),
            (
                r#"{"todos":null}"#,
                "TodoWrite: missing or invalid 'todos' field",
            ),
            (
                r#"{"todos":[null]}"#,
                "TodoWrite: todos[0] must be an object",
            ),
            (
                r#"{"todos":[{"content":null,"activeForm":null,"status":null}]}"#,
                "TodoWrite: todos[0].content must be a string",
            ),
            (
                r#"{"todos":[{"content":"x","activeForm":null,"status":null}]}"#,
                "TodoWrite: todos[0].activeForm must be a string",
            ),
            (
                r#"{"todos":[{"content":"x","activeForm":"y","status":null}]}"#,
                "TodoWrite: todos[0].status must be a string",
            ),
            (
                r#"{"todos":[{"content":"x","activeForm":"y","status":"done"}]}"#,
                "TodoWrite: todos[0].status is invalid",
            ),
        ] {
            let input = Utf16JsonProjection::parse(source).unwrap();
            let ToolError::InvalidInput(message) = parse_todos(&input).unwrap_err() else {
                panic!("expected TodoWrite input error");
            };
            assert_eq!(message.string_at(""), Some(expected), "{source}");
        }
        let input = Utf16JsonProjection::object(vec![(
            "todos".into(),
            Utf16JsonProjection::sparse_array(1, Vec::new()).unwrap(),
        )])
        .unwrap();
        let ToolError::InvalidInput(message) = parse_todos(&input).unwrap_err() else {
            panic!("expected TodoWrite hole error");
        };
        assert_eq!(
            message.string_at(""),
            Some("TodoWrite: todos[0] must be an object")
        );
    }

    #[test]
    fn borrowed_todo_rows_strip_unknown_subtrees_and_keep_exact_result_order() {
        let input = Utf16JsonProjection::parse(&format!(
            r#"{{"todos":[{{"activeForm":"\udc00","ignored":{}0{},"status":"pending","content":"\ud800","id":"ignored"}}]}}"#,
            "[".repeat(2_000), "]".repeat(2_000),
        )).unwrap();
        let parsed = parse_todos(&input).unwrap();
        assert_eq!(parsed[0].content.string_units(""), Some(vec![0xd800]));
        assert_eq!(parsed[0].active_form.string_units(""), Some(vec![0xdc00]));
        assert!(parsed[0].id.is_empty());
        assert_eq!(
            todos_result_projection(&input).to_json_string().unwrap(),
            r#"[{"content":"\ud800","status":"pending","activeForm":"\udc00"}]"#
        );
    }

    #[test]
    fn borrowed_todo_rows_cover_large_flat_shapes_without_changing_result_bytes() {
        let row = r#"{"content":"\ud800","status":"pending","activeForm":"\udc00"}"#;
        for count in [0, 1, 256, 4_096] {
            let rows = std::iter::repeat_n(row, count)
                .collect::<Vec<_>>()
                .join(",");
            let input = Utf16JsonProjection::parse(&format!(r#"{{"todos":[{rows}]}}"#)).unwrap();
            let parsed = parse_todos(&input).unwrap();
            assert_eq!(parsed.len(), count);
            assert!(parsed
                .iter()
                .all(|item| item.content.string_units("") == Some(vec![0xd800])));
            assert_eq!(
                todos_result_projection(&input).to_json_string().unwrap(),
                format!("[{rows}]")
            );
        }
    }

    fn prompt_text(prompt: Utf16JsonProjection) -> String {
        prompt
            .string_at("")
            .expect("generated prompt is a root string")
            .to_owned()
    }

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
            Some(Utf16JsonProjection::from(
                "manage the session task checklist"
            ))
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
                &Utf16JsonProjection::from_serde_value(&json!({}))
                    .expect("empty todo input is valid source JSON"),
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
            todo("a", "x", TodoState::Pending, "active"),
            todo("b", "y", TodoState::InProgress, "active"),
            todo("c", "z", TodoState::Completed, "active"),
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_rejects_empty_content() {
        let todos = vec![todo("a", "", TodoState::Pending, "active")];
        let err = validate_todos(&todos).expect_err("empty content must reject");
        assert_eq!(err, "Content cannot be empty");
    }

    #[test]
    fn validate_accepts_long_content() {
        // claude-code zod `TodoItemSchema` (BWd) has no `.max()` on content —
        // long todos must be accepted (matches v2.1.183).
        let huge = "x".repeat(10_000);
        let todos = vec![todo("a", &huge, TodoState::Pending, "active")];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_accepts_duplicate_id() {
        // TS `TodoItemSchema` has no `id` concept, so there is no uniqueness
        // rule; id-less input deserializes to empty ids, which must not reject.
        let todos = vec![
            todo("dup", "x", TodoState::Pending, "active"),
            todo("dup", "y", TodoState::Pending, "active"),
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_accepts_two_in_progress() {
        // TS treats single-in-progress as advisory (prompt-only), never an
        // error — multiple `in_progress` items must be accepted.
        let todos = vec![
            todo("a", "x", TodoState::InProgress, "active"),
            todo("b", "y", TodoState::InProgress, "active"),
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn validate_accepts_zero_in_progress() {
        let todos = vec![
            todo("a", "x", TodoState::Pending, "active"),
            todo("b", "y", TodoState::Completed, "active"),
        ];
        assert!(validate_todos(&todos).is_ok());
    }

    #[test]
    fn summary_counts_each_state() {
        let todos = vec![
            todo("a", "x", TodoState::Pending, "active"),
            todo("b", "y", TodoState::Pending, "active"),
            todo("c", "z", TodoState::InProgress, "active"),
            todo("d", "w", TodoState::Completed, "active"),
        ];
        assert_eq!(summary(&todos), (2_u32, 1_u32, 1_u32));
    }

    #[tokio::test]
    async fn execute_persists_todos_to_session() {
        let (tool, sink, session, use_ctx) = make_tool_and_session();
        tool.ctx.bus.attach_sink(sink.clone()).await;
        let input = json!({
            "todos": [
                { "content": "first",  "status": "pending",     "activeForm": "Doing first"  },
                { "content": "second", "status": "in_progress", "activeForm": "Doing second" },
                { "content": "third",  "status": "completed",   "activeForm": "Doing third"  }
            ]
        });
        let res = tool
            .call(input, use_ctx, fresh_tx())
            .await
            .expect("happy path");
        let s = session.lock().await;
        assert_eq!(s.todos.len(), 3);
        assert_eq!(s.todos[0].id, "", "TodoWrite input has no per-item id");
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
                { "content": "x", "status": "done", "activeForm": "Doing x" }
            ]
        });
        let err = match tool.call(input, use_ctx, fresh_tx()).await {
            Ok(_) => panic!("unknown variant must fail"),
            Err(error) => error,
        };
        assert!(matches!(&err, ToolError::InvalidInput(_)));
        let msg = source_text(&err.model_facing_message());
        assert_eq!(msg, "TodoWrite: todos[0].status is invalid");
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
        let todos = vec![todo("a", "x", TodoState::Pending, "")];
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
        assert_eq!(s.todos[0].active_form.string_at(""), Some("Shipping it"));
    }

    #[tokio::test]
    async fn source_todo_text_survives_session_and_old_todos_result() {
        let (tool, _sink, session, use_ctx) = make_tool_and_session();
        let content_units = vec![b'a' as u16, 0xD800, b'b' as u16];
        let active_units = vec![0xDC00, b'x' as u16];
        let first = Utf16JsonProjection::object(vec![(
            "todos".into(),
            Utf16JsonProjection::array(vec![Utf16JsonProjection::object(vec![
                ("content".into(), source_text_units(&content_units)),
                (
                    "status".into(),
                    source_text_projection(TODO_STATE_IN_PROGRESS),
                ),
                ("activeForm".into(), source_text_units(&active_units)),
            ])
            .expect("todo input object is valid JSON")])
            .expect("todo input list is valid JSON"),
        )])
        .expect("TodoWrite input is valid JSON");
        Tool::call(&tool, first, use_ctx.clone(), fresh_tx())
            .await
            .expect("source todo text is accepted");
        {
            let stored = session.lock().await;
            assert_eq!(
                stored.todos[0].content.string_units(""),
                Some(content_units.clone())
            );
            assert_eq!(
                stored.todos[0].active_form.string_units(""),
                Some(active_units.clone())
            );
        }

        let next = generated_builtin_result(&json!({
            "todos": [{
                "content": "next",
                "status": "in_progress",
                "activeForm": "Doing next"
            }]
        }));
        let result = Tool::call(&tool, next, use_ctx, fresh_tx())
            .await
            .expect("second source update succeeds");

        let stored = session.lock().await;
        assert_eq!(stored.todos[0].content.string_at(""), Some("next"));
        assert_eq!(
            stored.todos[0].active_form.string_at(""),
            Some("Doing next")
        );
        assert_eq!(
            result.data.string_units("/oldTodos/0/content"),
            Some(content_units.clone())
        );
        assert_eq!(
            result.data.string_units("/oldTodos/0/activeForm"),
            Some(active_units.clone())
        );
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
    async fn prompt_selects_native_or_generic_lean_from_request_options() {
        let (tool, _sink, _session, _use_ctx) = make_tool_and_session();
        // Standalone callers without a model use the generic Lean bundle.
        let opts = PromptOptions {
            prompt_policy: None,
            is_scheduled_turn: false,
            session_id: None,
            include_examples: false,
            model: None,
            model_profile: None,
            ..Default::default()
        };
        let with_model = |m: &str| PromptOptions {
            prompt_policy: None,
            is_scheduled_turn: false,
            session_id: None,
            include_examples: false,
            model: Some(m.to_string()),
            model_profile: None,
            ..Default::default()
        };

        // An unknown route uses the mode's generic Lean default. Known
        // classic Claude models retain the native long template.
        {
            let _g = EnvGuard::clear("LINGXI_SIMPLE_SYSTEM_PROMPT");
            assert_eq!(
                prompt_text(tool.prompt(&opts).await),
                TODO_WRITE_PROMPT_SIMPLE
            );
            let p = prompt_text(tool.prompt(&with_model("claude-opus-4-7")).await);
            assert_eq!(p, TODO_WRITE_PROMPT_FULL);
            assert!(p.starts_with(
                "Use this tool to create and manage a structured task list for your current coding session."
            ));
            assert!(p.ends_with(
                "Being proactive with task management demonstrates attentiveness and ensures you complete all requirements successfully.\n"
            ));
            assert!(p.contains("Uses the Edit tool to add a comment"));
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
                    prompt_text(tool.prompt(&with_model(classic)).await),
                    TODO_WRITE_PROMPT_FULL,
                    "UWu classic {classic:?} ⇒ UWd"
                );
            }
            // UWu == false ⇒ short FWd. opus-4-8 is OUTSIDE UWu's 4-0..4-7 list.
            for new_model in ["claude-opus-4-8", "claude-fable-5-1", "claude-mythos-5-1"] {
                assert_eq!(
                    prompt_text(tool.prompt(&with_model(new_model)).await),
                    TODO_WRITE_PROMPT_SIMPLE,
                    "new model {new_model:?} ⇒ FWd"
                );
            }
            // 2.1.319 applies the classic gate to recognized early-access aliases.
            for eap in ["claude-opus-4-7-eap", "claude-sonnet-4-5-eap[x]"] {
                assert_eq!(
                    prompt_text(tool.prompt(&with_model(eap)).await),
                    TODO_WRITE_PROMPT_FULL,
                    "classic early-access {eap:?} retains the native long prompt"
                );
            }
        }

        // `LINGXI_SIMPLE_SYSTEM_PROMPT` truthy ⇒ the short FWd prompt
        // (overrides the model branch — fires even for a classic model).
        for truthy in ["1", "true", "yes", "on", " On "] {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", truthy);
            let p = prompt_text(tool.prompt(&with_model("claude-opus-4-7")).await);
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
            let p = prompt_text(tool.prompt(&with_model("claude-opus-4-8")).await);
            assert_eq!(p, TODO_WRITE_PROMPT_FULL, "defined-falsy {falsy:?} ⇒ UWd");
        }

        // Generic Lean is independent of Claude-specific simple-prompt overrides.
        {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", "1");
            assert_eq!(
                prompt_text(tool.prompt(&opts).await),
                TODO_WRITE_PROMPT_SIMPLE
            );
        }

        // Any other (non-truthy, non-defined-falsy) value falls through to the
        // model-class branch for Claude; unknown routes remain generic Lean.
        {
            let _g = EnvGuard::set("LINGXI_SIMPLE_SYSTEM_PROMPT", "garbage");
            assert_eq!(
                prompt_text(tool.prompt(&with_model("claude-opus-4-7")).await),
                TODO_WRITE_PROMPT_FULL
            );
            assert_eq!(
                prompt_text(tool.prompt(&with_model("claude-opus-4-8")).await),
                TODO_WRITE_PROMPT_SIMPLE
            );
            assert_eq!(
                prompt_text(tool.prompt(&opts).await),
                TODO_WRITE_PROMPT_SIMPLE
            );
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
            assert_eq!(select_todo_write_prompt(None), TODO_WRITE_PROMPT_SIMPLE);
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
