use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::host::{PromptSnapshot, PromptToolDescription};
use serde_json::json;
use session::jsonl::JsonlMessage;
use std::sync::{Arc, Mutex as StdMutex};
use tool_api::registry::ToolRegistry;

static ENV_LOCK: StdMutex<()> = StdMutex::new(());

fn attachment(payload: serde_json::Value) -> JsonlMessage {
    JsonlMessage {
        message_type: "attachment".to_string(),
        uuid: "11111111-2222-4333-8444-555555555555".to_string(),
        parent_uuid: None,
        session_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string(),
        timestamp: "2026-08-30T00:00:00.000Z".to_string(),
        cwd: "/tmp/project".to_string(),
        version: "0.12.0".to_string(),
        message: serde_json::Value::Null,
        is_sidechain: false,
        user_type: Some("external".to_string()),
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra: [("attachment".to_string(), payload)].into_iter().collect(),
    }
}

#[test]
fn prompt_snapshot_serializes_to_oracle_payload_shape() {
    let snapshot = PromptSnapshot {
        system_prompt: vec!["frozen static prompt".to_string()],
        tools: vec![PromptToolDescription {
            name: "Read".to_string(),
            description: "old description".to_string(),
        }],
    };
    let value = serde_json::to_value(snapshot).expect("snapshot serializes");
    assert_eq!(value["systemPrompt"], json!(["frozen static prompt"]));
    assert_eq!(value["tools"][0]["name"], "Read");
    assert_eq!(value["tools"][0]["description"], "old description");
    assert!(
        value.get("type").is_none(),
        "attachment type belongs to the envelope"
    );
}

#[test]
fn prompt_snapshot_resume_uses_last_valid_attachment() {
    let valid = attachment(json!({
        "type": "prompt_snapshot",
        "systemPrompt": ["first static"],
        "tools": [{"name": "Read", "description": "v1"}],
    }));
    let invalid = attachment(json!({
        "type": "prompt_snapshot",
        "systemPrompt": [7],
    }));
    let snapshot = crate::resume::prompt_snapshot_from_messages(&[valid, invalid])
        .expect("the last valid snapshot should be recovered");
    assert_eq!(snapshot.system_prompt, vec!["first static"]);
    assert_eq!(snapshot.tools[0].name, "Read");
    assert_eq!(snapshot.tools[0].description, "v1");
}

#[tokio::test]
async fn prompt_snapshot_appends_only_new_inline_tools_after_success() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("CLAUDE_CODE_SIMPLE");
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    *orch.prompt_runtime.prompt_snapshot.lock().await = Some(PromptSnapshot {
        system_prompt: vec!["frozen".to_string()],
        tools: vec![PromptToolDescription {
            name: "Read".to_string(),
            description: "frozen Read".to_string(),
        }],
    });
    orch.record_inline_prompt_tools_after_success(&[
        json!({"name": "Read", "description": "live Read"}),
        json!({"name": "Deferred", "description": "not inline", "defer_loading": true}),
        json!({"name": "Write", "description": "new Write"}),
    ])
    .await;
    let snapshot = orch
        .prompt_runtime
        .prompt_snapshot
        .lock()
        .await
        .clone()
        .unwrap();
    assert_eq!(
        snapshot
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.description.as_str()))
            .collect::<Vec<_>>(),
        vec![("Read", "frozen Read"), ("Write", "new Write")]
    );
}

#[tokio::test]
async fn resumed_session_without_snapshot_never_creates_one() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("CLAUDE_CODE_SIMPLE");
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.prompt_runtime
        .prompt_snapshot_resume
        .store(true, std::sync::atomic::Ordering::Release);
    orch.record_prompt_snapshot_if_needed(Some("live prompt"), &[])
        .await;
    assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
}

/// Oracle `lje(e)`'s truth table. `explicit` is the `--system-prompt-snapshot`
/// choice, which is THREE-valued: absent defers to the disjuncts, `off`
/// overrules all of them, `on` forces the feature regardless of session kind.
#[test]
fn the_snapshot_gate_matches_the_oracle_truth_table() {
    use ConversationOrchestrator as O;
    // `if (e.systemPromptSnapshot === false) return false` comes FIRST, so an
    // explicit `off` beats every disjunct that would otherwise enable it.
    assert!(!O::snapshot_gate(Some(false), true, false));
    assert!(!O::snapshot_gate(Some(false), false, false));
    assert!(!O::snapshot_gate(Some(false), true, true));

    // An explicit `on` forces it even in simple mode.
    assert!(O::snapshot_gate(Some(true), false, true));

    // Absent: `SESSION_KIND === "bg" || !CLAUDE_CODE_SIMPLE`.
    assert!(O::snapshot_gate(None, true, true), "bg wins over simple");
    assert!(
        !O::snapshot_gate(None, false, true),
        "simple alone disables"
    );
    assert!(
        O::snapshot_gate(None, false, false),
        "the DEFAULT is on: 2.1.270 has no rollout flag left to wait for"
    );
}

/// The regression this fixes: with no env set and no flag passed, the snapshot
/// must be ELIGIBLE. The port used to require `tengu_carved_slate` /
/// `CLAUDE_CODE_CARVED_SLATE`, a 2.1.252 rollout flag that no longer exists
/// anywhere in the 2.1.270 binary — so the whole recorded-prompt feature was
/// implemented, tested, restored on resume, and unreachable.
#[tokio::test]
async fn a_plain_session_records_its_prompt_without_any_env_opt_in() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("CLAUDE_CODE_SIMPLE");
    std::env::remove_var("CLAUDE_CODE_CARVED_SLATE");
    std::env::remove_var("LINGXI_SESSION_KIND");
    lingxi_core::host::session_flags::set_system_prompt_snapshot(None);

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    assert!(
        orch.prompt_snapshot_eligible(),
        "no env, no flag — 2.1.270 records by default"
    );
    orch.record_prompt_snapshot_if_needed(Some("static prompt"), &[])
        .await;
    assert!(
        orch.prompt_runtime.prompt_snapshot.lock().await.is_some(),
        "an eligible session must actually record, not merely qualify"
    );
}

/// `--system-prompt-snapshot off` is the escape hatch the flag exists for:
/// "never record; the prompt is rendered fresh every request (for iterating on
/// prompt text)". It must beat the default that the test above pins.
#[tokio::test]
async fn the_off_flag_stops_recording_a_session_that_would_otherwise_record() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("CLAUDE_CODE_SIMPLE");
    std::env::remove_var("LINGXI_SESSION_KIND");
    lingxi_core::host::session_flags::set_system_prompt_snapshot(Some(false));

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    assert!(!orch.prompt_snapshot_eligible());
    orch.record_prompt_snapshot_if_needed(Some("static prompt"), &[])
        .await;
    assert!(
        orch.prompt_runtime.prompt_snapshot.lock().await.is_none(),
        "`off` must mean no record at all"
    );
    lingxi_core::host::session_flags::set_system_prompt_snapshot(None);
}
