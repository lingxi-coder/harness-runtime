use super::*;
use crate::prompt::todo_reminder::{TaskReminderItem, TodoReminderTaskProvider};
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::{TodoItem, TodoState};
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// `std::env::set_var`/`remove_var` are not thread-safe; serialize the
/// env-mutating tests (selection + killswitch) behind this lock.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Minimal name-only tool for the tool-presence gates.
struct NamedTool(&'static str);
#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(
            || serde_json::json!({ "type": "object", "properties": {} }),
        );
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other { reason: "t".into() },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Static V2 task source.
struct StaticTasks(Vec<TaskReminderItem>);
#[async_trait]
impl TodoReminderTaskProvider for StaticTasks {
    async fn task_items(
        &self,
        _session_id: lingxi_core::types::SessionId,
    ) -> Vec<TaskReminderItem> {
        self.0.clone()
    }
}

/// A main-loop model BELOW the `OO()` gate's thresholds
/// (`[("opus",[4,8]),("sonnet",[5]),("fable",[5]),("mythos",[5])]`).
///
/// `OrchestratorConfig::default()` uses `claude-opus-4-8`, which sits exactly AT
/// the opus threshold, so on the default config 2.1.263 withdraws the five
/// todo/task tools and both reminder variants with them. That is correct
/// behaviour, and [`the_model_gate_suppresses_the_reminder`] pins it — but it is
/// not what the rest of this file is about, so every other test here runs on an
/// ungated model and keeps exercising the reminder's own counters and text.
const UNGATED_MODEL: &str = "claude-sonnet-4-5";

fn orch_with(tools: ToolRegistry) -> ConversationOrchestrator {
    let api = Arc::new(MockApiClient::new(vec![]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.tools
        .set_main_loop_model(Some(UNGATED_MODEL.to_string()));
    orch
}

/// claude-code 2.1.263 `case"todo_reminder":{if(X_()||!OO())return[]` and
/// `case"task_reminder":{if(!h3())return[]` — a reminder must never describe
/// tools the model was not offered. Both variants, at both counter thresholds.
#[tokio::test]
async fn the_model_gate_suppresses_the_reminder() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");

    // Only meaningful while no `OO()` escape hatch is live in this process.
    if tool_task::reminder::select_mode(Some(orchestrator_default_model())).is_some() {
        return;
    }

    // The tool-presence gate wants the variant's own "recent use" tool:
    // `TodoWrite` for V1, `TaskUpdate` for V2.
    for (tasks_env, tool) in [(Some("off"), "TodoWrite"), (None, "TaskUpdate")] {
        match tasks_env {
            Some(v) => std::env::set_var("LINGXI_ENABLE_TASKS", v),
            None => std::env::remove_var("LINGXI_ENABLE_TASKS"),
        }
        let orch = orch_with(reg_with(&[tool]));
        // The gated model is what `OrchestratorConfig::default()` already
        // carries; `orch_with` overrides it, so put it back for this test.
        orch.tools
            .set_main_loop_model(Some(orchestrator_default_model().to_string()));
        prime_session(&orch, 10, 10).await;
        assert!(
            orch.todo_reminder_message().await.is_none(),
            "{tool} reminder must be suppressed on {}",
            orchestrator_default_model()
        );
        // Same session, same counters, ungated model ⇒ it fires again, which is
        // what proves the suppression came from the gate and not from the
        // counters or an empty history.
        orch.tools
            .set_main_loop_model(Some(UNGATED_MODEL.to_string()));
        assert!(
            orch.todo_reminder_message().await.is_some(),
            "{tool} reminder must fire once the gate opens"
        );
    }
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

fn orchestrator_default_model() -> &'static str {
    crate::config::DEFAULT_MODEL
}

fn reg_with(names: &[&'static str]) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for n in names {
        reg.register_builtin(Arc::new(NamedTool(n)));
    }
    reg
}

/// Make the session non-empty (the binary `!e||e.length===0 ⇒ []` gate) and
/// optionally arm both counters at their thresholds.
async fn prime_session(orch: &ConversationOrchestrator, write_c: u32, reminder_c: u32) {
    let mut s = orch.session.lock().await;
    s.history.push(ConversationMessage::user(
        MessageId::new(),
        "hi".to_string(),
    ));
    s.turns_since_last_todo_write = write_c;
    s.turns_since_last_reminder = reminder_c;
}

// ── counters ────────────────────────────────────────────────────────────

#[tokio::test]
async fn bump_increments_both_counters_each_turn() {
    let orch = orch_with(reg_with(&["TodoWrite"]));
    orch.bump_reminder_turn_counters().await;
    orch.bump_reminder_turn_counters().await;
    let s = orch.session.lock().await;
    assert_eq!(s.turns_since_last_todo_write, 2);
    assert_eq!(s.turns_since_last_reminder, 2);
}

#[tokio::test]
async fn note_tool_call_resets_write_counter_on_todowrite_v1() {
    let _g = ENV_LOCK.lock().await;
    std::env::set_var("LINGXI_ENABLE_TASKS", "off"); // ⇒ V1 selected
    let orch = orch_with(reg_with(&["TodoWrite"]));
    {
        let mut s = orch.session.lock().await;
        s.turns_since_last_todo_write = 7;
        s.turns_since_last_reminder = 7;
    }
    orch.note_todo_reminder_tool_call(&["TodoWrite".to_string()])
        .await;
    {
        let s = orch.session.lock().await;
        assert_eq!(
            s.turns_since_last_todo_write, 0,
            "TodoWrite resets write ctr"
        );
        assert_eq!(s.turns_since_last_reminder, 7, "reminder ctr untouched");
    }
    // A non-qualifying tool (Read) does NOT reset.
    {
        let mut s = orch.session.lock().await;
        s.turns_since_last_todo_write = 5;
    }
    orch.note_todo_reminder_tool_call(&["Read".to_string()])
        .await;
    assert_eq!(orch.session.lock().await.turns_since_last_todo_write, 5);
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn note_tool_call_resets_on_taskupdate_v2_default() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_ENABLE_TASKS"); // default ⇒ V2
    let orch = orch_with(reg_with(&["TaskUpdate"]));
    {
        let mut s = orch.session.lock().await;
        s.turns_since_last_todo_write = 9;
    }
    // V2 resets on TaskCreate or TaskUpdate, NOT on TodoWrite.
    orch.note_todo_reminder_tool_call(&["TodoWrite".to_string()])
        .await;
    assert_eq!(
        orch.session.lock().await.turns_since_last_todo_write,
        9,
        "V2 mode does not reset on TodoWrite"
    );
    orch.note_todo_reminder_tool_call(&["TaskUpdate".to_string()])
        .await;
    assert_eq!(orch.session.lock().await.turns_since_last_todo_write, 0);
}

// ── firing gates ──────────────────────────────────────────────────────────

#[tokio::test]
async fn no_fire_below_threshold() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");
    std::env::set_var("LINGXI_ENABLE_TASKS", "off"); // V1
    let mode = Arc::new(ReminderCoordinatorMode(std::sync::atomic::AtomicBool::new(
        true,
    )));
    let orch = orch_with(reg_with(&["TodoWrite"]))
        .with_coordinator_mode(mode.clone())
        .with_coordinator_simple_mode_for_test(false);
    prime_session(&orch, 10, 10).await;
    assert!(orch.tools.find_by_name("TodoWrite").is_some());
    assert!(orch.todo_reminder_message().await.is_none());
    assert_eq!(orch.session.lock().await.turns_since_last_reminder, 10);
    mode.0.store(false, std::sync::atomic::Ordering::SeqCst);
    orch.session.lock().await.turns_since_last_todo_write = 9; // write ctr one short
    assert!(orch.todo_reminder_message().await.is_none());
    // Now both at threshold ⇒ fires.
    {
        let mut s = orch.session.lock().await;
        s.turns_since_last_todo_write = 10;
    }
    assert!(orch.todo_reminder_message().await.is_some());
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn no_fire_when_history_empty() {
    let _g = ENV_LOCK.lock().await;
    std::env::set_var("LINGXI_ENABLE_TASKS", "off");
    let orch = orch_with(reg_with(&["TodoWrite"]));
    // counters armed but NO history.
    {
        let mut s = orch.session.lock().await;
        s.turns_since_last_todo_write = 10;
        s.turns_since_last_reminder = 10;
    }
    assert!(
        orch.todo_reminder_message().await.is_none(),
        "empty history suppresses the reminder"
    );
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn no_fire_when_tool_absent() {
    let _g = ENV_LOCK.lock().await;
    std::env::set_var("LINGXI_ENABLE_TASKS", "off"); // V1 needs TodoWrite
    let orch = orch_with(reg_with(&["Read"])); // no TodoWrite
    prime_session(&orch, 10, 10).await;
    assert!(orch.todo_reminder_message().await.is_none());
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn no_fire_when_brief_present() {
    let _g = ENV_LOCK.lock().await;
    std::env::set_var("LINGXI_ENABLE_TASKS", "off");
    // TodoWrite present AND Brief (SendUserMessage) present ⇒ skip.
    let orch = orch_with(reg_with(&["TodoWrite", "SendUserMessage"]));
    prime_session(&orch, 10, 10).await;
    assert!(orch.todo_reminder_message().await.is_none());
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn killswitch_off_suppresses() {
    let _g = ENV_LOCK.lock().await;
    std::env::set_var("LINGXI_ENABLE_TASKS", "off");
    std::env::set_var("LINGXI_TODO_REMINDER_MODE", "off");
    let orch = orch_with(reg_with(&["TodoWrite"]));
    prime_session(&orch, 10, 10).await;
    assert!(
        orch.todo_reminder_message().await.is_none(),
        "killswitch \"off\" suppresses the reminder"
    );
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

// ── exact text + reminder-counter reset on fire ──────────────────────────

#[tokio::test]
async fn v1_fires_with_exact_text_no_items_and_resets_reminder_ctr() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");
    std::env::set_var("LINGXI_ENABLE_TASKS", "off"); // V1
    let orch = orch_with(reg_with(&["TodoWrite"]));
    prime_session(&orch, 10, 10).await;
    let msg = orch.todo_reminder_message().await.expect("fires");
    // `Zy`/`NT` envelope + `isMeta:!0` (2.1.238 @296690005). NOTE the body's
    // own trailing `\n` sits directly before the wrapper's, exactly as the
    // oracle's `` `<system-reminder>\n${o}\n</system-reminder>` `` produces.
    assert!(msg.is_meta(), "todo_reminder must be isMeta");
    assert_eq!(
        msg.text_content(),
        "<system-reminder>\nThe TodoWrite tool hasn't been used recently. If you're working on tasks that would benefit from tracking progress, consider using the TodoWrite tool to track progress. Also consider cleaning up the todo list if has become stale and no longer matches what you are working on. Only use it if it's relevant to the current work. This is just a gentle reminder - ignore if not applicable.\n\n</system-reminder>"
    );
    // The reminder counter reset to 0 on fire.
    assert_eq!(orch.session.lock().await.turns_since_last_reminder, 0);
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn v1_fires_with_items_byte_exact() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");
    std::env::set_var("LINGXI_ENABLE_TASKS", "off"); // V1
    let orch = orch_with(reg_with(&["TodoWrite"]));
    prime_session(&orch, 10, 10).await;
    {
        let mut s = orch.session.lock().await;
        s.todos.push(TodoItem {
            id: "t1".into(),
            content: "first".into(),
            status: TodoState::Pending,
            active_form: "Doing first".into(),
        });
        s.todos.push(TodoItem {
            id: "t2".into(),
            content: "second".into(),
            status: TodoState::InProgress,
            active_form: "Doing second".into(),
        });
    }
    let msg = orch.todo_reminder_message().await.expect("fires");
    assert!(msg.text_content().ends_with(
        "\n\nHere are the existing contents of your todo list:\n\n[1. [pending] first\n2. [in_progress] second]\n</system-reminder>"
    ), "got: {:?}", msg.text_content());
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn v2_fires_with_items_from_provider_byte_exact() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");
    std::env::remove_var("LINGXI_ENABLE_TASKS"); // default ⇒ V2
    let orch =
        orch_with(reg_with(&["TaskUpdate"])).with_todo_reminder_tasks(Arc::new(StaticTasks(vec![
            TaskReminderItem {
                id: "1".into(),
                status: TodoState::Completed,
                subject: "alpha".into(),
            },
            TaskReminderItem {
                id: "2".into(),
                status: TodoState::Pending,
                subject: "beta".into(),
            },
        ])));
    prime_session(&orch, 10, 10).await;
    let msg = orch.todo_reminder_message().await.expect("fires");
    let text = msg.text_content();
    assert!(msg.is_meta(), "task_reminder must be isMeta");
    assert!(
        text.starts_with("<system-reminder>\nThe task tools haven't been used recently."),
        "got: {text}"
    );
    assert!(
        text.ends_with(
            "\n\nHere are the existing tasks:\n\n#1. [completed] alpha\n#2. [pending] beta\n</system-reminder>"
        ),
        "got: {text:?}"
    );
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

#[tokio::test]
async fn v2_fires_base_only_without_provider() {
    let _g = ENV_LOCK.lock().await;
    std::env::remove_var("LINGXI_TODO_REMINDER_MODE");
    std::env::remove_var("LINGXI_ENABLE_TASKS"); // V2
    let orch = orch_with(reg_with(&["TaskUpdate"])); // no task provider
    prime_session(&orch, 10, 10).await;
    let msg = orch.todo_reminder_message().await.expect("fires");
    assert_eq!(
        msg.text_content(),
        "<system-reminder>\nThe task tools haven't been used recently. If you're working on tasks that would benefit from tracking progress, consider using TaskCreate to add new tasks and TaskUpdate to update task status (set to in_progress when starting, completed when done). Also consider cleaning up the task list if it has become stale. Only use these if relevant to the current work. This is just a gentle reminder - ignore if not applicable.\n\n</system-reminder>"
    );
    std::env::remove_var("LINGXI_ENABLE_TASKS");
}

struct ReminderCoordinatorMode(std::sync::atomic::AtomicBool);

impl lingxi_core::host::coordinator_mode::CoordinatorModeHandle for ReminderCoordinatorMode {
    fn is_enabled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
