//! system prompt via `assemble_system_prompt` and passes it to the
//! API client.
use llm_runtime::ContentBlock as LlmContentBlock;

use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tempfile::TempDir;
use tool_api::registry::ToolRegistry;

#[tokio::test]
async fn run_turn_passes_assembled_system_prompt_to_api_client() {
    let tmp = TempDir::new().unwrap();
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "ok".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let tools = Arc::new(ToolRegistry::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output,
        memory,
        tmp.path().to_path_buf(),
    );
    orch.run_turn("hi").await.expect("turn");

    let systems = api.captured_systems().await;
    assert_eq!(systems.len(), 1, "exactly one API call");
    let s = systems[0].as_deref().expect("system prompt threaded");
    // Assembler always opens with HEADER and carries the `# Environment` block.
    // There is NO `Notes:` FOOTER on the MAIN prompt (R-P1b).
    // GAP-2: `# Context management` follows the env block; the
    // act-don't-re-derive section is appended after it.
    assert!(s.starts_with("You are LingXi"));
    assert!(s.contains("# Environment"));
    assert!(!s.contains("<env>"));
    assert!(!s.contains("Notes:"));
    // The env block is still present; context management follows it.
    // The 2.1.319 bundle (acf3c6ba) ends the fast-mode line at `/fast.` with
    // no per-model availability list.
    assert!(s.contains("It can be toggled with /fast."));
    assert!(!s.contains("is available on Opus"));
    assert!(s.contains("# Context management"));
    // The act-don't-re-derive section follows context management. Since the
    // 2.1.319 bundle (acf3c6ba) it is no longer guaranteed to be the tail, so
    // only its presence and order are checked (as the crate's own prompt test).
    let context = s.find("# Context management").expect("context management");
    let act = s
        .find("give a recommendation, not an exhaustive survey")
        .expect("act-don't-re-derive section");
    assert!(context < act);
}
