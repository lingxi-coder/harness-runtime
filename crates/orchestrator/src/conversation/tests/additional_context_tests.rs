use super::*;
use crate::prompt::MemoryFile;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::ContentBlock;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn orch_with(memory: Arc<StaticMemoryProvider>, email: Option<&str>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig {
            user_email: email.map(str::to_string),
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory,
        std::env::temp_dir(),
    )
}

fn text(msg: &ConversationMessage) -> String {
    match msg {
        ConversationMessage::User { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn runtime_message(body: &str) -> ConversationMessage {
    ConversationMessage::user_meta(MessageId::new(), body.to_string())
}

fn mobile_environment_with_runtime(
    tool_runtime: lingxi_core::host::MobileToolRuntime,
    cwd: Option<&str>,
) -> lingxi_core::host::MobileRuntimeEnvironment {
    lingxi_core::host::MobileRuntimeEnvironment::new(
        lingxi_core::host::MobileHostEnvironment::new(
            lingxi_core::host::MobileHostOs::Ios,
            Some("19.0".into()),
            lingxi_core::host::MobileDeviceClass::Phone,
            lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
            lingxi_core::host::MobileLaunchMode::Interactive,
        ),
        tool_runtime,
        cwd.map(str::to_string),
        Some("/bin/sh".into()),
        Some("Mobile Linux sh".into()),
        lingxi_core::host::MobileNetworkPolicy::PermissionMediated,
        lingxi_core::host::MobileLifecyclePolicy::IosFiniteBackgroundAssertion,
    )
}

fn mobile_environment(cwd: &str) -> lingxi_core::host::MobileRuntimeEnvironment {
    mobile_environment_with_runtime(
        lingxi_core::host::MobileToolRuntime::MobileLinuxGuest,
        Some(cwd),
    )
}

#[tokio::test]
async fn all_three_keys_byte_exact_order_and_wrapper() {
    // claudeMd + userEmail present; currentDate always present. Insertion
    // order (claude-code `pS`): claudeMd, userEmail, currentDate.
    let mem = Arc::new(StaticMemoryProvider::with_files(vec![MemoryFile {
        path: std::path::PathBuf::from("/proj/LINGXI.md"),
        body: "MD BODY".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "MD BODY".into(),
        content_differs_from_disk: false,
    }]));
    let orch = orch_with(mem, Some("u@example.com"));
    let msg = orch.additional_context_message().await.expect("present");
    // It is a META user message (claude-code `isMeta:!0`).
    assert!(msg.is_meta(), "additionalContext must be isMeta");
    let body = text(&msg);

    // Exact wrapper: opens with the header line, closes with the IMPORTANT
    // line indented by 6 spaces + the closing tag + trailing LF.
    assert!(body.starts_with(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n"
    ));
    assert!(body.ends_with(
        "\n\n      IMPORTANT: this context may or may not be relevant to your tasks. \
You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n"
    ));

    // Keys in order, each `# key\nvalue`, joined by `\n`.
    let i_md = body.find("# claudeMd\n").expect("claudeMd key");
    let i_email = body.find("# userEmail\n").expect("userEmail key");
    let i_date = body.find("# currentDate\n").expect("currentDate key");
    assert!(
        i_md < i_email && i_email < i_date,
        "key order claudeMd<userEmail<currentDate"
    );

    // claudeMd value = the assembled memory block (preamble + Contents).
    assert!(body.contains("# claudeMd\nCodebase and user instructions are shown below."));
    assert!(body.contains("Contents of /proj/LINGXI.md"));
    assert!(body.contains("MD BODY"));
    // userEmail value.
    assert!(body.contains("# userEmail\nThe user's email address is u@example.com."));
    // currentDate value (ISO local date).
    let today = crate::prompt::env_meta::current_date_string();
    assert!(body.contains(&format!("# currentDate\nToday's date is {today}.")));
}

#[tokio::test]
async fn omits_lingxi_md_and_email_when_absent_keeps_date() {
    // Empty memory + no email → only `# currentDate` remains.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let msg = orch
        .additional_context_message()
        .await
        .expect("date always present");
    let body = text(&msg);
    assert!(!body.contains("# claudeMd"));
    assert!(!body.contains("# userEmail"));
    assert!(body.contains("# currentDate\nToday's date is "));
    // The body between the header and the IMPORTANT line is exactly the one
    // currentDate entry (no stray blank lines from empty entries).
    let today = crate::prompt::env_meta::current_date_string();
    let expected = format!(
        "<system-reminder>\n\
As you answer the user's questions, you can use the following context:\n\
# currentDate\nToday's date is {today}.\n\
\n      IMPORTANT: this context may or may not be relevant to your tasks. \
You should not respond to this context unless it is highly relevant to your task.\n\
</system-reminder>\n"
    );
    assert_eq!(body, expected, "single-key wrapper byte-lock");
}

#[tokio::test]
async fn empty_email_string_is_treated_as_absent() {
    // `user_email: Some("")` (or whitespace) is filtered, matching the
    // `...email&&{userEmail:…}` spread + LingXi's non-empty guard.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), Some("   "));
    let body = text(&orch.additional_context_message().await.expect("date"));
    assert!(!body.contains("# userEmail"));
}

#[tokio::test]
async fn runtime_message_is_prepended_before_additional_context() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let runtime = "<system-reminder>\nMobile runtime environment (version 1)\n</system-reminder>";
    orch.mobile_runtime_environment_message = Some(runtime_message(runtime));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));
    let original = ConversationMessage::user(MessageId::new(), "hello".into());
    let mut messages = vec![original.clone()];

    orch.prepend_leading_context(&mut messages).await;

    assert_eq!(text(&messages[0]), runtime);
    assert!(text(&messages[1]).contains("Guest workspace: /workspace/a"));
    assert!(text(&messages[2]).contains("# currentDate\nToday's date is "));
    assert_eq!(messages[3], original);
    assert_eq!(
        orch.mobile_runtime_environment_preview().await.as_deref(),
        Some(runtime)
    );
}

#[tokio::test]
async fn unresolved_native_workspace_path_falls_back_to_guest_coordinate() {
    let host_cwd = std::path::PathBuf::from("/tmp/native-host-worktree");
    let session_cwd = tool_api::SessionCwd::new(host_cwd.clone(), vec![host_cwd]);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_session_cwd(session_cwd)
        .with_mobile_runtime_environment(mobile_environment("/workspace/a"))
        .with_mobile_workspace_cwd_resolver(Arc::new(|_| None));
    let mut messages = Vec::new();

    orch.prepend_leading_context(&mut messages).await;

    assert!(text(&messages[1]).contains("Guest workspace: /workspace/a"));
    assert!(!text(&messages[1]).contains("native-host-worktree"));
}

#[test]
fn scheduled_mobile_runtime_uses_headless_prompt_guidance() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    orch.config.interactive_session = true;
    assert!(orch.prompt_is_interactive());

    let mut environment = mobile_environment("/workspace/a");
    environment.host.launch_mode = lingxi_core::host::MobileLaunchMode::ScheduledHeadless;
    orch.mobile_runtime_environment = Some(environment);

    assert!(!orch.prompt_is_interactive());
}

#[tokio::test]
async fn runtime_message_stays_first_when_transient_context_is_reattached() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let runtime = "<system-reminder>runtime</system-reminder>";
    let deferred = runtime_message("<system-reminder>deferred</system-reminder>");
    let date = runtime_message("<system-reminder>date</system-reminder>");
    let tail = runtime_message("<system-reminder>tail</system-reminder>");
    orch.mobile_runtime_environment_message = Some(runtime_message(runtime));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));
    let original = ConversationMessage::user(MessageId::new(), "hello".into());
    let mut messages = vec![original.clone()];

    orch.reattach_outgoing_context(
        &mut messages,
        Some(&deferred),
        Some(&date),
        std::slice::from_ref(&tail),
    )
    .await;

    assert_eq!(text(&messages[0]), runtime);
    assert!(text(&messages[1]).contains("Guest workspace: /workspace/a"));
    assert_eq!(text(&messages[2]), text(&date));
    assert_eq!(text(&messages[3]), text(&deferred));
    assert!(text(&messages[4]).contains("# currentDate\nToday's date is "));
    assert_eq!(messages[5], original);
    assert_eq!(messages[6], tail);
}

#[tokio::test]
async fn runtime_message_keeps_dynamic_environment_separate() {
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.mobile_runtime_environment_message = Some(runtime_message(
        "<system-reminder>\nMobile runtime environment (version 1)\n</system-reminder>",
    ));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));

    let body = text(&orch.additional_context_message().await.expect("date"));
    assert!(body.contains("# Environment\n"));
    assert!(body.contains("# currentDate\nToday's date is "));
}

#[tokio::test]
async fn non_guest_mobile_runtime_keeps_environment_re_emission_when_excluded() {
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.mobile_runtime_environment = Some(mobile_environment_with_runtime(
        lingxi_core::host::MobileToolRuntime::AndroidLegacy,
        None,
    ));

    let body = text(&orch.additional_context_message().await.expect("date"));
    assert!(body.contains("# Environment\n"));
    assert!(body.contains("# currentDate\nToday's date is "));
}

#[tokio::test]
async fn system_prompt_override_stays_verbatim_while_runtime_message_is_sent() {
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "ok".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            system_prompt_override: Some("CUSTOM PROMPT — no assembler".into()),
            ..OrchestratorConfig::default()
        },
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let runtime = "<system-reminder>\nMobile runtime environment (version 1)\n</system-reminder>";
    orch.mobile_runtime_environment_message = Some(runtime_message(runtime));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));

    orch.run_turn("hi").await.expect("turn");

    assert_eq!(
        api.captured_systems().await,
        vec![Some("CUSTOM PROMPT — no assembler".into())]
    );
    let sent = api.captured_msgs().await;
    assert_eq!(text(&sent[0][0]), runtime);
    assert!(text(&sent[0][1]).contains("Guest workspace: /workspace/a"));
    assert!(text(&sent[0][2]).contains("# currentDate\nToday's date is "));
}

// ------------------------------------------------------------------------
// `date_change` (cc `Cop`): mid-session midnight crossing.
// ------------------------------------------------------------------------

/// Rewind the memoized session-start date so the live local date always
/// differs — the "session started yesterday" setup.
fn seed_stale_session_date(
    orch: &ConversationOrchestrator,
    session_id: lingxi_core::types::SessionId,
) {
    let mut state = orch
        .prompt_runtime
        .date_change
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.session_id = Some(session_id);
    state.session_date = "2000-01-01".to_string();
    state.delivered_date = None;
}

fn expected_date_change_body() -> String {
    let today = crate::prompt::env_meta::current_date_string();
    format!(
        "<system-reminder>\nThe date has changed. Today's date is now {today}. \
No need to announce the new date \u{2014} the user's own clock shows it.\n</system-reminder>"
    )
}

#[tokio::test]
async fn additional_context_keeps_the_session_start_date_after_rollover() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = orch.session.lock().await.session_id;
    seed_stale_session_date(&orch, sid);

    let body = text(
        &orch
            .additional_context_message()
            .await
            .expect("date context"),
    );
    assert!(body.contains("# currentDate\nToday's date is 2000-01-01."));
    assert!(
        orch.date_change_reminder_message(sid).is_some(),
        "rollover is announced only by date_change"
    );
}

#[test]
fn date_change_none_when_date_unchanged() {
    // First producer run seeds the session-start memo (`LGe = Vr(wcs)`), so
    // a same-day session NEVER emits — the locked fixtures stay identical.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = lingxi_core::types::SessionId::new();
    assert!(orch.date_change_reminder_message(sid).is_none());
    assert!(orch.date_change_reminder_message(sid).is_none());
}

#[test]
fn date_change_emits_once_after_midnight() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = lingxi_core::types::SessionId::new();
    seed_stale_session_date(&orch, sid);
    let msg = orch
        .date_change_reminder_message(sid)
        .expect("date differs from session start");
    // Byte-exact reminder (renderer @238108493) inside the `Ww` wrap.
    assert_eq!(text(&msg), expected_date_change_body());
    // Meta user message (`zr({…, isMeta:!0})`).
    assert!(matches!(
        msg,
        ConversationMessage::User { is_meta: true, .. }
    ));
    // The producer is PURE: without a commit the SAME reminder is still due,
    // so a step that never reaches the model cannot swallow it.
    assert!(orch.date_change_reminder_message(sid).is_some());
    orch.commit_date_change_reminder();
    // Dedupe: once delivered, the following turn (same date) emits nothing.
    assert!(orch.date_change_reminder_message(sid).is_none());
}

#[test]
fn date_change_stays_deduped_after_a_compact_boundary() {
    // Compaction must not reset the session-level reminder. The leading
    // `currentDate` remains the session-start memo and the changed date was
    // already delivered once.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = lingxi_core::types::SessionId::new();
    seed_stale_session_date(&orch, sid);
    assert!(orch.date_change_reminder_message(sid).is_some());
    orch.commit_date_change_reminder();
    assert!(orch.date_change_reminder_message(sid).is_none());
    assert!(orch.date_change_reminder_message(sid).is_none());
}

#[test]
fn date_change_re_seeds_the_session_start_date_on_a_new_session() {
    // `clearSessionCaches` clears BOTH `LGe`'s memo and the emitted date, so
    // a `/clear` (fresh `SessionId`) or in-place resume (adopted id) must
    // NOT fire a reminder into the brand-new conversation.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let old = lingxi_core::types::SessionId::new();
    seed_stale_session_date(&orch, old);
    assert!(orch.date_change_reminder_message(old).is_some());

    let fresh = lingxi_core::types::SessionId::new();
    assert!(
        orch.date_change_reminder_message(fresh).is_none(),
        "a new session re-seeds the start date to today"
    );
}

// ------------------------------------------------------------------
// PathAtlas S3 (mobile-linux): the `claudeMd` memory block must be
// probed at the HOST directory backing the guest session cwd.
//
// `additional_context_message` is the ONLY render path for the memory
// block (`prompt/mod.rs` no longer splices it into the system prompt),
// and on mobile `session_cwd` holds the GUEST path
// (`engine-mobile/src/host.rs`: `model_cwd` comes from
// `workspace_mount.guest_path`). Loading memory at that raw guest path
// means a workspace `LINGXI.md` never reaches the model at all.
// `build_prompt_context` already hops guest→host through
// `prompt_probe_cwd_resolver`; this message must use the same
// coordinate.
// ------------------------------------------------------------------

fn orch_with_provider(
    memory: Arc<dyn crate::prompt::MemoryHierarchyProvider>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory,
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn claude_md_is_probed_at_the_host_dir_behind_a_guest_session_cwd() {
    let host = tempfile::tempdir().expect("tempdir");
    let host_root = host.path().to_path_buf();
    std::fs::write(
        host_root.join("LINGXI.md"),
        "MARKER-host-probed-workspace-contract",
    )
    .expect("seed LINGXI.md");

    // The guest path the model sees. It does NOT exist on the host, which
    // is exactly why probing it directly yields nothing.
    let guest = std::path::PathBuf::from("/workspace/app-pathatlas-s3-probe");

    let resolver_root = host_root.clone();
    let orch = orch_with_provider(Arc::new(crate::prompt::RealMemoryHierarchyProvider))
        .with_session_cwd(tool_api::SessionCwd::new(guest.clone(), Vec::new()))
        .with_prompt_probe_cwd_resolver(Arc::new(move |_path: &std::path::Path| {
            resolver_root.clone()
        }));

    let body = text(&orch.additional_context_message().await.expect("present"));
    assert!(
        body.contains("MARKER-host-probed-workspace-contract"),
        "workspace LINGXI.md must reach the model; body was:\n{body}"
    );
    assert!(
        body.contains(&format!(
            "Contents of {}",
            host_root.join("LINGXI.md").display()
        )),
        "the memory block must name the HOST path; body was:\n{body}"
    );
}

#[tokio::test]
async fn claude_md_without_a_resolver_still_probes_the_session_cwd() {
    // Desktop INERT INVARIANT: no resolver installed ⇒ `probe_cwd == cwd`,
    // so the memory block is byte-identical to before the guest→host hop.
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("LINGXI.md"),
        "MARKER-desktop-session-cwd-probe",
    )
    .expect("seed LINGXI.md");

    let orch =
        orch_with_provider(Arc::new(crate::prompt::RealMemoryHierarchyProvider)).with_session_cwd(
            tool_api::SessionCwd::new(root.path().to_path_buf(), Vec::new()),
        );

    let body = text(&orch.additional_context_message().await.expect("present"));
    assert!(
        body.contains("MARKER-desktop-session-cwd-probe"),
        "no-resolver path must keep probing the session cwd; body was:\n{body}"
    );
}

#[tokio::test]
async fn git_status_uses_the_host_dir_behind_a_guest_session_cwd() {
    let host = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(host.path())
            .status()
            .expect("git is available")
            .success()
    };
    assert!(git(&["init", "-q"]));
    assert!(git(&["config", "user.name", "Prompt Probe"]));
    assert!(git(&[
        "config",
        "user.email",
        "prompt-probe@example.invalid"
    ]));
    assert!(git(&["config", "commit.gpgsign", "false"]));
    std::fs::write(host.path().join("tracked.txt"), "seed").expect("seed file");
    assert!(git(&["add", "tracked.txt"]));
    assert!(git(&["commit", "-q", "-m", "seed"]));

    let guest = std::path::PathBuf::from("/workspace/app-pathatlas-git-probe");
    let resolver_root = host.path().to_path_buf();
    let orch = orch_with_provider(Arc::new(StaticMemoryProvider::empty()))
        .with_session_cwd(tool_api::SessionCwd::new(guest, Vec::new()))
        .with_prompt_probe_cwd_resolver(Arc::new(move |_path: &std::path::Path| {
            resolver_root.clone()
        }));

    let prompt = orch.build_system_prompt().await;
    assert!(
        prompt.contains("gitStatus: This is the git status at the start of the conversation."),
        "the host-backed gitStatus snapshot must survive prompt assembly; prompt was:\n{prompt}"
    );
    assert!(prompt.contains("Git user: Prompt Probe"));
}
