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
            cache_control: None, citations: None,
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
    // Opus 5 joined the fast-mode list in 2.1.220 (@113736244: `toggled with
    // /fast and is available on Opus 5/4.8/4.7.`); 2.1.238 then dropped 4.7
    // from the copy — `is available on Opus 5/4.8.` counts 238=4 / 220=0
    // (fns K9T @297075805 and Y9T @297076453).
    assert!(s.contains("available on Opus 5/4.8."));
    assert!(s.contains("# Context management"));
    // The act-don't-re-derive section (`ACT_DONT_REDERIVE_SECTION`) now follows
    // context management, so THAT is the tail. Verified in the 2.1.220 binary
    // @237499709, which ends the clause with NO trailing period — the next
    // constant starts `# Delivering work`, a section this model does not get
    // (it gates on `opus_5_prompt_bundle`).
    assert!(s.ends_with("give a recommendation, not an exhaustive survey"));
}
