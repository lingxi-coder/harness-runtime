use super::*;
use crate::OrchestratorConfig;
use crate::prompt::skill_listing::{SkillListingEntry, SkillListingProvider};
use crate::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider, noop_hook_executor,
};
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// Static skill-listing fixture.
struct FixtureSkills(Vec<SkillListingEntry>);
#[async_trait]
impl SkillListingProvider for FixtureSkills {
    async fn skill_entries(&self) -> Vec<SkillListingEntry> {
        self.0.clone()
    }
}

/// Minimal tool whose only meaningful behavior is its name — used to put a
/// `Skill`-named tool (or not) into the registry for the gate test.
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

fn orch_with(
    tools: ToolRegistry,
    provider: Option<Arc<dyn SkillListingProvider>>,
) -> ConversationOrchestrator {
    let api = Arc::new(MockApiClient::new(vec![]));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    if let Some(p) = provider {
        orch = orch.with_skill_listing(p);
    }
    orch
}

fn fixture() -> Arc<dyn SkillListingProvider> {
    Arc::new(FixtureSkills(vec![SkillListingEntry {
        name: "debug".into(),
        description: "Debug a failing test".into(),
        when_to_use: None,
        is_bundled: false,
    }]))
}

#[tokio::test]
async fn reminder_present_when_provider_and_skill_tool_wired() {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(NamedTool("Skill")));
    let orch = orch_with(reg, Some(fixture()));
    let msg = orch
        .skill_listing_reminder_message()
        .await
        .expect("reminder present");
    let text = msg.text_content();
    assert!(text.starts_with("<system-reminder>"), "got: {text}");
    assert!(text.contains("The following skills are available for use with the Skill tool:"));
    assert!(text.contains("- debug: Debug a failing test"));
}

#[tokio::test]
async fn no_reminder_when_skill_tool_absent() {
    // Provider wired, but the Skill tool is not in the registry this turn.
    let orch = orch_with(ToolRegistry::new(), Some(fixture()));
    assert!(orch.skill_listing_reminder_message().await.is_none());
}

#[tokio::test]
async fn no_reminder_when_provider_absent() {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(NamedTool("Skill")));
    let orch = orch_with(reg, None);
    assert!(orch.skill_listing_reminder_message().await.is_none());
}

// ── PLANMODE (plan_mode_turn_messages) ────────────────────────────────

#[tokio::test]
async fn mod_attachment_receives_native_plan_details_for_entry_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("plan-attachment.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('prompt.attachment', { type: 'plan_mode' }, ($, e) => ({
            text: `${e.detail.reminder}:${e.detail.planFilePath}:${e.detail.hasPlan}`,
          }));
          on('prompt.attachment', { type: 'plan_mode_exit' }, ($, e) => ({
            text: `exit:${e.detail.planFilePath}:${e.detail.hasPlan}`,
          }));
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "plan-attachment",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(ToolRegistry::new(), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let plan_path = {
        let session_ref = orch.session();
        let mut session = session_ref.lock().await;
        session.plan_mode = true;
        session.plan_reminder_shown = false;
        orch.session_plan_file_path(&session.session_id)
    };
    let has_plan = std::path::Path::new(&plan_path).exists();
    let entry = orch.collect_turn_reminders(true, true, None).await;
    assert!(entry.transient.iter().any(|message| {
        message.text_content()
            == format!("<system-reminder>\nfull:{plan_path}:{has_plan}\n</system-reminder>")
    }));

    {
        let session_ref = orch.session();
        let mut session = session_ref.lock().await;
        session.plan_mode = false;
        session.plan_mode_exit_pending = true;
    }
    let exit = orch.collect_turn_reminders(true, true, None).await;
    assert!(exit.transient.iter().any(|message| {
        message.text_content()
            == format!("<system-reminder>\nexit:{plan_path}:{has_plan}\n</system-reminder>")
    }));
}

#[tokio::test]
async fn plan_mode_reminder_none_when_plan_mode_off() {
    // Default session: plan mode OFF ⇒ no reminder (keeps the default build
    // byte-identical).
    let orch = orch_with(ToolRegistry::new(), None);
    assert!(!orch.session().lock().await.plan_mode);
    assert!(orch.plan_mode_turn_messages().await.is_empty());
}

#[tokio::test]
async fn plan_mode_reminder_full_then_sparse() {
    let orch = orch_with(ToolRegistry::new(), None);
    {
        let sess = orch.session();
        let mut s = sess.lock().await;
        s.plan_mode = true;
        // EnterPlanMode resets this; assert the reset default explicitly.
        s.plan_reminder_shown = false;
    }

    // First plan-mode turn ⇒ FULL (206 `LU_`): the aIp banner + the 5-phase
    // workflow scaffold.
    let m0 = orch
        .plan_mode_turn_messages()
        .await
        .pop()
        .expect("plan-mode full reminder");
    // 2.1.238 `Zy`/`NT` envelope + `isMeta:!0` (@296675470 / @296673554).
    assert!(m0.is_meta(), "plan_mode reminder must be isMeta");
    let t0 = m0.text_content();
    assert!(
        t0.starts_with("<system-reminder>\nPlan mode is active. The user indicated"),
        "turn-0 must be the FULL reminder inside the system-reminder envelope, got: {t0}"
    );
    assert!(
        t0.ends_with("\n</system-reminder>"),
        "envelope must close: {t0}"
    );
    assert!(
        t0.contains("## Plan Workflow"),
        "full reminder scaffold: {t0}"
    );
    assert!(
        t0.contains("### Phase 5: Call ExitPlanMode"),
        "full reminder phases: {t0}"
    );
    // No plan file on disk for a fresh temp session.
    assert!(
        t0.contains("No plan file exists yet."),
        "planExists=false: {t0}"
    );
    // Injection armed the sparse flag.
    assert!(orch.session().lock().await.plan_reminder_shown);

    // 2.1.238 `X4T` cadence: the NEXT model call in the same turn (and the
    // next four user turns) get NOTHING — `if(_ && y < 5) return []`.
    assert!(
        orch.plan_mode_turn_messages().await.is_empty(),
        "no second attachment before 5 real user turns have passed"
    );
    push_real_user_turns(&orch, 4).await;
    assert!(
        orch.plan_mode_turn_messages().await.is_empty(),
        "4 turns is still under TURNS_BETWEEN_ATTACHMENTS"
    );

    // The 5th real user turn releases attachment #2 ⇒ SPARSE (`L5T`).
    push_real_user_turns(&orch, 1).await;
    let t1 = orch
        .plan_mode_turn_messages()
        .await
        .pop()
        .expect("plan-mode sparse reminder")
        .text_content();
    assert!(
        t1.starts_with(
            "<system-reminder>\nPlan mode still active (see full instructions earlier in conversation)."
        ),
        "turn-1 must be the SPARSE reminder, got: {t1}"
    );
    assert!(t1.contains("Follow 5-phase workflow."), "sparse body: {t1}");
}

/// Append `n` non-meta, non-tool_result user messages — the only kind `ixl`
/// (2.1.238 @296524028) counts toward `TURNS_BETWEEN_ATTACHMENTS`.
async fn push_real_user_turns(orch: &ConversationOrchestrator, n: usize) {
    let sess = orch.session();
    let mut s = sess.lock().await;
    for i in 0..n {
        s.history.push(ConversationMessage::user(
            MessageId::new(),
            format!("turn {i}"),
        ));
    }
}

/// `c % FULL_REMINDER_EVERY_N_ATTACHMENTS === 1`: attachments #1 and #6 are
/// FULL, #2..#5 sparse. The pre-fix port emitted FULL exactly once and was
/// sparse forever after.
#[tokio::test]
async fn plan_mode_reminder_returns_to_full_every_fifth_attachment() {
    let orch = orch_with(ToolRegistry::new(), None);
    {
        let sess = orch.session();
        let mut s = sess.lock().await;
        s.plan_mode = true;
        s.plan_reminder_shown = false;
    }
    let full_prefix = "<system-reminder>\nPlan mode is active. The user indicated";
    let sparse_prefix = "<system-reminder>\nPlan mode still active (see full instructions";

    let mut forms = Vec::new();
    for _ in 0..6 {
        let t = orch
            .plan_mode_turn_messages()
            .await
            .pop()
            .expect("attachment")
            .text_content();
        forms.push(if t.starts_with(full_prefix) {
            "full"
        } else {
            assert!(t.starts_with(sparse_prefix), "unexpected body: {t}");
            "sparse"
        });
        push_real_user_turns(&orch, 5).await;
    }
    assert_eq!(
        forms,
        vec!["full", "sparse", "sparse", "sparse", "sparse", "full"]
    );
}

/// A tool-result continuation is NOT a turn (`sxl`/`y3T` @296541952), so it
/// never advances the cadence.
#[tokio::test]
async fn tool_result_continuations_do_not_advance_the_plan_cadence() {
    let orch = orch_with(ToolRegistry::new(), None);
    {
        let sess = orch.session();
        let mut s = sess.lock().await;
        s.plan_mode = true;
        s.plan_reminder_shown = false;
    }
    assert!(!orch.plan_mode_turn_messages().await.is_empty(), "first");
    {
        let sess = orch.session();
        let mut s = sess.lock().await;
        for _ in 0..10 {
            s.history.push(ConversationMessage::User { api_message_override: None,
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: lingxi_core::types::ToolUseId::new(),
                    content: "ok".into(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            });
        }
    }
    assert!(
        orch.plan_mode_turn_messages().await.is_empty(),
        "ten tool-result continuations are still zero real user turns"
    );
}

#[tokio::test]
async fn plan_mode_reminder_reset_replays_full() {
    // After a sparse turn, re-entering plan mode (reset flag) replays FULL.
    let orch = orch_with(ToolRegistry::new(), None);
    orch.session().lock().await.plan_mode = true;
    let _full = orch.plan_mode_turn_messages().await.pop().expect("full");
    push_real_user_turns(&orch, 5).await;
    let _sparse = orch.plan_mode_turn_messages().await.pop().expect("sparse");
    // Simulate EnterPlanMode / set_plan_mode(true) re-arming the tracker.
    // Re-entry also resets the `X4T` cadence (the `plan_mode_exit` boundary
    // `Y4T` stops counting at), so the very next call emits again.
    orch.session().lock().await.plan_reminder_shown = false;
    let again = orch
        .plan_mode_turn_messages()
        .await
        .pop()
        .expect("full again after reset")
        .text_content();
    assert!(
        again.starts_with("<system-reminder>\nPlan mode is active. The user indicated"),
        "got: {again}"
    );
}

/// ONE derivation: the plan path the reminder names, the path the permission
/// carve-out allows and the path `ExitPlanMode` reads back all come from the
/// same published identity. Three separate derivations is precisely how the
/// carve-out silently stops covering the file the model was told to write.
#[tokio::test]
async fn the_plan_path_comes_from_the_shared_identity() {
    let mut orch = orch_with(ToolRegistry::new(), None);
    let sid = orch.session().lock().await.session_id;

    // No identity published ⇒ the pre-slug, session-id derivation.
    assert!(
        orch.session_plan_file_path(&sid)
            .ends_with(&format!("{}.md", sid.as_uuid()))
    );

    let matcher = Arc::new(
        lingxi_core::host::plan_files::PlanFileMatcher::with_identity(
            lingxi_core::host::plan_files::PlanFileIdentity {
                plans_dir: std::path::PathBuf::from("/tmp/lingxi-plans"),
                slug: "brave-baking-otter".into(),
                workshop_enabled: false,
            },
        ),
    );
    orch.config.plan_files = Some(matcher.clone());

    let from_identity = matcher
        .plan_file(None)
        .expect("identity")
        .to_string_lossy()
        .into_owned();
    assert_eq!(orch.session_plan_file_path(&sid), from_identity);

    // …and the reminder the model actually receives names that same path.
    orch.session().lock().await.plan_mode = true;
    let body = orch
        .plan_mode_turn_messages()
        .await
        .pop()
        .expect("plan-mode reminder")
        .text_content();
    assert!(body.contains(&from_identity), "{body}");
}

#[tokio::test]
async fn plan_mode_reminder_uses_custom_instructions() {
    // C5: `--plan-mode-instructions` (config.plan_mode_instructions) replaces
    // the default 5-phase body with the custom "## Plan Workflow" branch.
    let mut orch = orch_with(ToolRegistry::new(), None);
    orch.config.plan_mode_instructions = Some("MY BODY".to_string());
    orch.session().lock().await.plan_mode = true;
    let full = orch
        .plan_mode_turn_messages()
        .await
        .pop()
        .expect("plan-mode custom reminder")
        .text_content();
    assert!(
        full.contains("## Plan Workflow\n\nMY BODY\n\n### Call ExitPlanMode"),
        "custom workflow body: {full}"
    );
    assert!(
        !full.contains("### Phase 1"),
        "default phases suppressed: {full}"
    );
}

// ── SKILLLIST.1 delta (sent-tracking) ──────────────────────────────────

/// A skill provider whose entry set can change between turns (shared
/// `Arc<Mutex<…>>`), to exercise the "new skill appears later" delta path.
struct MutableSkills(std::sync::Arc<std::sync::Mutex<Vec<SkillListingEntry>>>);
#[async_trait]
impl SkillListingProvider for MutableSkills {
    async fn skill_entries(&self) -> Vec<SkillListingEntry> {
        self.0.lock().unwrap().clone()
    }
}

fn skill(name: &str) -> SkillListingEntry {
    SkillListingEntry {
        name: name.into(),
        description: format!("desc for {name}"),
        when_to_use: None,
        is_bundled: false,
    }
}

#[tokio::test]
async fn skill_listing_delta_turn0_full_then_none_when_no_new() {
    // Turn 0 emits the FULL listing; a later turn with the SAME skills (no
    // new names) emits nothing (None).
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(NamedTool("Skill")));
    let orch = orch_with(
        reg,
        Some(Arc::new(FixtureSkills(vec![skill("alpha"), skill("beta")]))),
    );

    // Turn 0: both skills present.
    let t0 = orch
        .skill_listing_reminder_message()
        .await
        .expect("turn-0 full listing");
    let t0 = t0.text_content();
    assert!(t0.contains("- alpha:"), "turn-0 missing alpha: {t0}");
    assert!(t0.contains("- beta:"), "turn-0 missing beta: {t0}");

    // Turn 1: no NEW skills since both were already sent → None.
    assert!(
        orch.skill_listing_reminder_message().await.is_none(),
        "turn-1 must emit nothing when no new skill appeared"
    );
}

#[tokio::test]
async fn skill_listing_delta_emits_only_new_skill_on_later_turn() {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(NamedTool("Skill")));
    let shared = std::sync::Arc::new(std::sync::Mutex::new(vec![skill("alpha")]));
    let orch = orch_with(reg, Some(Arc::new(MutableSkills(shared.clone()))));

    // Turn 0: only `alpha`.
    let t0 = orch
        .skill_listing_reminder_message()
        .await
        .expect("turn-0")
        .text_content();
    assert!(t0.contains("- alpha:"));
    assert!(!t0.contains("- gamma:"));

    // A new skill `gamma` appears.
    shared.lock().unwrap().push(skill("gamma"));

    // Turn 1: ONLY the new `gamma` is emitted (alpha was already sent).
    let t1 = orch
        .skill_listing_reminder_message()
        .await
        .expect("turn-1 new-only")
        .text_content();
    assert!(
        t1.contains("- gamma:"),
        "turn-1 must contain the new skill: {t1}"
    );
    assert!(
        !t1.contains("- alpha:"),
        "turn-1 must NOT re-emit the already-sent skill: {t1}"
    );
}

struct OnceAsyncResponses(std::sync::Mutex<Vec<String>>);
#[async_trait::async_trait]
impl crate::prompt::async_hook_response::AsyncHookResponseProvider for OnceAsyncResponses {
    async fn take_pending_responses(&self) -> Vec<hooks::ExactHookText> {
        std::mem::take(&mut *self.0.lock().unwrap())
            .into_iter()
            .map(Into::into)
            .collect()
    }
}

struct OnceDetailedAsyncResponses(
    std::sync::Mutex<Vec<crate::prompt::async_hook_response::AsyncHookResponse>>,
);
#[async_trait::async_trait]
impl crate::prompt::async_hook_response::AsyncHookResponseProvider for OnceDetailedAsyncResponses {
    async fn take_pending_responses(&self) -> Vec<hooks::ExactHookText> {
        std::mem::take(&mut *self.0.lock().unwrap())
            .into_iter()
            .map(|response| response.text)
            .collect()
    }

    async fn take_pending_with_events(
        &self,
    ) -> Vec<crate::prompt::async_hook_response::AsyncHookResponse> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[tokio::test]
async fn mod_attachment_receives_each_async_hook_event_origin() {
    use crate::prompt::async_hook_response::AsyncHookResponse;

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("async-attachment.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('prompt.attachment', { type: 'async_hook_response' }, ($, e, next) => {
            if (e.origin.kind !== 'hook' || !e.origin.event) throw new Error('missing hook origin');
            return next({ ...e, text: `${e.origin.event}: ${e.text}` });
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "async-attachment",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let provider = Arc::new(OnceDetailedAsyncResponses(std::sync::Mutex::new(vec![
        AsyncHookResponse {
            text: "first".into(),
            hook_event: Some("PostToolUse".into()),
            publication_guard: None,
        },
        AsyncHookResponse {
            text: "second".into(),
            hook_event: Some("SessionStart".into()),
            publication_guard: None,
        },
    ])));
    let orch = orch_with(ToolRegistry::new(), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
        .with_async_hook_responses(provider);
    let reminders = orch.collect_turn_reminders(true, true, None).await;
    let texts: Vec<_> = reminders
        .transient
        .iter()
        .map(ConversationMessage::text_content)
        .filter(|text| text.contains(": first") || text.contains(": second"))
        .collect();
    assert_eq!(
        texts,
        vec![
            "<system-reminder>\nPostToolUse: first\n</system-reminder>",
            "<system-reminder>\nSessionStart: second\n</system-reminder>"
        ]
    );
}

#[tokio::test]
async fn async_hook_response_reminder_folds_in_then_drains_once() {
    let reg = ToolRegistry::new();
    let orch = orch_with(reg, None).with_async_hook_responses(Arc::new(OnceAsyncResponses(
        std::sync::Mutex::new(vec!["ran background lints: clean".to_string()]),
    )));
    // Turn 0: the completed background-hook response is folded in, wrapped.
    let t0 = orch
        .async_hook_response_mod_messages()
        .await
        .pop()
        .expect("turn-0 async hook response")
        .message
        .text_content();
    assert!(t0.contains("<system-reminder>"), "must be wrapped: {t0}");
    assert!(
        t0.contains("ran background lints: clean"),
        "must carry the hook's system_message: {t0}"
    );
    // Turn 1: consume-once — the delivered response must NOT re-appear.
    assert!(
        orch.async_hook_response_mod_messages().await.is_empty(),
        "a delivered async-hook response must be drained, not repeated"
    );
}

#[tokio::test]
async fn async_hook_response_reminder_none_without_provider() {
    let reg = ToolRegistry::new();
    let orch = orch_with(reg, None);
    assert!(
        orch.async_hook_response_mod_messages().await.is_empty(),
        "no provider wired ⇒ strict no-op"
    );
}

// ── hook-bg-fields: Stop / SubagentStop background_tasks + session_crons ──

/// A [`StopHookSnapshotProvider`] that returns fixed fixtures, so the
/// orchestrator's `populate_stop_hook_snapshot` wiring is testable without a
/// live registry / cron file.
struct FixtureStopSnapshot {
    tasks: Vec<hooks::HookBackgroundTask>,
    crons: Vec<hooks::HookSessionCron>,
}
#[async_trait::async_trait]
impl crate::stop_hook_snapshot::StopHookSnapshotProvider for FixtureStopSnapshot {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        self.tasks.clone()
    }
    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        self.crons.clone()
    }
}

#[tokio::test]
async fn populate_stop_hook_snapshot_stamps_both_arrays_when_wired() {
    use hooks::{HookBackgroundTask, HookSessionCron};
    let reg = ToolRegistry::new();
    let orch = orch_with(reg, None).with_stop_hook_snapshot(Arc::new(FixtureStopSnapshot {
        tasks: vec![HookBackgroundTask {
            is_idle: false,
            id: "b1".into(),
            r#type: "shell".into(),
            status: "running".into(),
            description: "build".into(),
            command: Some("cargo build".into()),
            agent_type: None,
            server: None,
            tool: None,
            name: None,
        }],
        crons: vec![HookSessionCron {
            id: "c1".into(),
            schedule: "* * * * *".into(),
            recurring: true,
            prompt: "hi".into(),
        }],
    }));
    // A Stop-firing context (the only path that populates the snapshot)
    // carries BOTH arrays, populated, after the snapshot helper runs.
    let mut ctx = orch.lifecycle_hook_ctx(false).await;
    // Before population the lifecycle ctx leaves both fields None (the
    // default — a non-Stop lifecycle hook omits the keys).
    assert!(ctx.background_tasks.is_none());
    assert!(ctx.session_crons.is_none());
    orch.populate_stop_hook_snapshot(&mut ctx).await;
    let bg = ctx.background_tasks.expect("background_tasks populated");
    assert_eq!(bg.len(), 1);
    assert_eq!(bg[0].id, "b1");
    assert_eq!(bg[0].r#type, "shell");
    let crons = ctx.session_crons.expect("session_crons populated");
    assert_eq!(crons.len(), 1);
    assert_eq!(crons[0].id, "c1");
    assert!(crons[0].recurring);
}

#[tokio::test]
async fn populate_stop_hook_snapshot_noop_without_provider() {
    let reg = ToolRegistry::new();
    let orch = orch_with(reg, None);
    let mut ctx = orch.lifecycle_hook_ctx(false).await;
    orch.populate_stop_hook_snapshot(&mut ctx).await;
    // No provider wired ⇒ both fields stay None ⇒ the executor omits the
    // keys (claude `m = undefined`), byte-identical to the pre-feature build.
    assert!(ctx.background_tasks.is_none());
    assert!(ctx.session_crons.is_none());
}

// ── T35: `task-notification` reminder folds in then drains once ──────────

/// A [`TaskNotificationProvider`] that hands back its fixture exactly once
/// (the second drain returns empty), mirroring the registry's
/// take-mark-evict semantics so the consume-once invariant is testable
/// without a real registry.
struct OnceTaskNotifications(
    std::sync::Mutex<Vec<lingxi_core::host::task_registry::TaskNotification>>,
);
#[async_trait::async_trait]
impl crate::prompt::task_notification::TaskNotificationProvider for OnceTaskNotifications {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[tokio::test]
async fn task_notification_reminder_folds_in_then_drains_once() {
    let reg = ToolRegistry::new();
    // One terminal `local_bash` task — the minimal faithful surface.
    let bash = lingxi_core::host::task_registry::TaskNotification {
        task_id: "b12345678".into(),
        task_type: "local_bash".into(),
        status: "completed".into(),
        description: "run tests".into(),
        tool_use_id: None,
        output_path: Some("/tmp/tasks/b12345678.output".into()),
        exit_code: Some(0),
        error: None,
        result: None,
        usage: None,
        killed_by: None,
        worktree_path: None,
        worktree_branch: None,
        workflow_failures: Vec::new(),
        workflow_agent_count: None,
        workflow_total_tokens: None,
        workflow_total_tool_calls: None,
        workflow_duration_ms: None,
        ..Default::default()
    };
    let orch = orch_with(reg, None).with_task_notifications(Arc::new(OnceTaskNotifications(
        std::sync::Mutex::new(vec![bash]),
    )));
    // Turn 0: the terminal task is folded in as a byte-faithful
    // `<task-notification>` inside one `<system-reminder>`.
    let mut t0_messages = orch.task_notification_reminder_messages().await;
    assert_eq!(t0_messages.len(), 1, "one completion ⇒ one message");
    let t0 = t0_messages
        .pop()
        .expect("turn-0 task notification")
        .text_content();
    // 2.1.238 `b_a` (@285068292): the provenance header sits INSIDE the
    // `<system-reminder>` envelope.
    let body = "<task-notification>\n\
<task-id>b12345678</task-id>\n\
<output-file>/tmp/tasks/b12345678.output</output-file>\n\
<status>completed</status>\n\
<summary>Background command \"run tests\" completed (exit code 0)</summary>\n\
</task-notification>";
    assert_eq!(
        t0,
        format!(
            "<system-reminder>\n{}{body}\n</system-reminder>",
            crate::prompt::task_notification::NON_USER_INPUT_HEADER
        )
    );
    // Turn 1: consume-once — the notified+evicted task must NOT re-appear.
    assert!(
        orch.task_notification_reminder_messages().await.is_empty(),
        "a delivered task notification must be drained, not repeated"
    );
}

/// TN-04 middle link: two completions drained in one turn must reach the
/// drivers as TWO messages. The renderer producing two enveloped blocks is
/// useless if the producer hands the drivers only the first.
#[tokio::test]
async fn two_completions_in_one_turn_are_two_messages() {
    let reg = ToolRegistry::new();
    let bash = lingxi_core::host::task_registry::TaskNotification {
        task_id: "b11111111".into(),
        task_type: "local_bash".into(),
        status: "completed".into(),
        description: "run tests".into(),
        output_path: Some("/tmp/tasks/b11111111.output".into()),
        exit_code: Some(0),
        ..Default::default()
    };
    let agent = lingxi_core::host::task_registry::TaskNotification {
        task_id: "a22222222".into(),
        task_type: "local_agent".into(),
        status: "completed".into(),
        description: "research".into(),
        output_path: Some("/tmp/tasks/a22222222.output".into()),
        ..Default::default()
    };
    let orch = orch_with(reg, None).with_task_notifications(Arc::new(OnceTaskNotifications(
        std::sync::Mutex::new(vec![bash, agent]),
    )));

    let messages = orch.task_notification_reminder_messages().await;
    assert_eq!(messages.len(), 2, "two completions ⇒ two messages");
    let texts: Vec<String> = messages.iter().map(|m| m.text_content()).collect();
    assert!(texts[0].contains("b11111111"), "got: {:?}", texts[0]);
    assert!(texts[1].contains("a22222222"), "got: {:?}", texts[1]);
    // Each message carries its own envelope and its own provenance header.
    for text in &texts {
        assert_eq!(text.matches("<system-reminder>").count(), 1, "got: {text}");
        assert_eq!(
            text.matches(crate::prompt::task_notification::NON_USER_INPUT_HEADER)
                .count(),
            1,
            "got: {text}"
        );
    }
}

/// A completion notification is a durable conversation event: the drivers push
/// it into history and the JSONL rather than returning it as a transient
/// reminder. Keeping it out of the reminder vector is what stops a retry -- which
/// rebuilds the request from history and re-appends the reminders -- from
/// sending the same completion twice.
#[tokio::test]
async fn task_notification_is_durable_and_not_a_transient_reminder() {
    let reg = ToolRegistry::new();
    let bash = lingxi_core::host::task_registry::TaskNotification {
        task_id: "b87654321".into(),
        task_type: "local_bash".into(),
        status: "completed".into(),
        description: "run tests".into(),
        tool_use_id: None,
        output_path: Some("/tmp/tasks/b87654321.output".into()),
        exit_code: Some(0),
        ..Default::default()
    };
    let orch = orch_with(reg, None).with_task_notifications(Arc::new(OnceTaskNotifications(
        std::sync::Mutex::new(vec![bash]),
    )));

    let before = orch.session.lock().await.history.len();
    let reminders = orch.collect_turn_reminders(true, false, None).await;
    assert_eq!(
        reminders.task_notifications.len(),
        1,
        "one terminal task ⇒ one durable message"
    );
    assert!(
        reminders
            .transient
            .iter()
            .all(|message| !message.text_content().contains("b87654321")),
        "the durable notification must stay out of the retryable reminder list"
    );

    let history = orch.session.lock().await.history.clone();
    assert_eq!(
        history.len(),
        before + reminders.task_notifications.len() + reminders.model_reminders.len(),
        "durable completions and model reminders must survive as history entries",
    );
    let notification = history
        .iter()
        .find(|message| message.id() == reminders.task_notifications[0].id())
        .expect("the appended notification");
    let rendered = format!("{notification:?}");
    assert!(
        rendered.contains("b87654321"),
        "the appended entry must be the completion, got: {rendered}",
    );

    // Consume-once still holds: a second drain has nothing left, so the
    // completion cannot be appended twice.
    assert!(
        orch.collect_turn_reminders(true, false, None)
            .await
            .task_notifications
            .is_empty(),
        "a drained completion must not surface again",
    );
}

#[tokio::test]
async fn task_notification_reminder_none_without_provider() {
    let reg = ToolRegistry::new();
    let orch = orch_with(reg, None);
    assert!(
        orch.task_notification_reminder_messages().await.is_empty(),
        "no provider wired ⇒ strict no-op"
    );
}

struct ReminderCoordinatorMode(std::sync::atomic::AtomicBool);

impl lingxi_core::host::coordinator_mode::CoordinatorModeHandle for ReminderCoordinatorMode {
    fn is_enabled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test]
async fn hidden_skill_does_not_emit_or_consume_listing() {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(NamedTool("Skill")));
    reg.set_session_tool_allowlist(&[]);
    let mode = Arc::new(ReminderCoordinatorMode(std::sync::atomic::AtomicBool::new(
        true,
    )));
    let mut orch = orch_with(reg, Some(fixture()))
        .with_coordinator_mode(mode)
        .with_coordinator_simple_mode_for_test(false);
    assert!(orch.tools.find_registered("Skill").is_some());
    assert!(orch.skill_listing_reminder_message().await.is_none());
    Arc::get_mut(&mut orch.tools)
        .expect("fixture exclusively owns registry")
        .set_session_tool_allowlist(&["Skill".to_string()]);
    assert!(orch.skill_listing_reminder_message().await.is_some());
}
