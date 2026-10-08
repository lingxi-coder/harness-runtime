//! End-to-end schema rejection for the production Bash tool.
//!
//! The command carries a control character that Native 2.1.291 rejects in the
//! Bash input schema. The real `BashTool` is registered in the real turn loop;
//! the mocked provider and process runner keep the probe local and command-free.

use async_trait::async_trait;
use lingxi_core::host::permission_gate::{PermissionDecision, PermissionGate};
use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
use lingxi_core::host::sandbox::SandboxedCommand;
use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use llm_runtime::ContentBlock as LlmContentBlock;
use mobile_linux_api::{ProcessError, ProcessOutput};
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tool_api::registry::ToolRegistry;
use tool_shell::BashTool;

struct CountingProcessRunner(Arc<AtomicUsize>);

#[async_trait]
impl ProcessRunner for CountingProcessRunner {
    async fn run(&self, _command: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }

    async fn spawn_background(
        &self,
        _command: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProcessError::Unsupported)
    }

    async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

struct CountingPermissionGate(Arc<AtomicUsize>);

#[async_trait]
impl PermissionGate for CountingPermissionGate {
    async fn check(&self, _name: &str, _input: &serde_json::Value) -> PermissionDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        PermissionDecision::Allow
    }
}

#[tokio::test]
async fn actual_bash_tool_control_refinement_reaches_dispatch_error_path() {
    let process_calls = Arc::new(AtomicUsize::new(0));
    let permission_checks = Arc::new(AtomicUsize::new(0));
    let mut context = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    context.process = Arc::new(CountingProcessRunner(process_calls.clone()));

    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(BashTool::new(context)));
    let tool_use_id = ToolUseId::new();
    let api = Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall { input_projection: None,
                id: tool_use_id.to_string(),
                name: "Bash".into(),
                input: serde_json::json!({"command":"printf '\u{1b}'"}),
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(registry),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(CountingPermissionGate(permission_checks.clone())),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    // The model supplies an actual ESC code point after JSON decoding. Input
    // schema validation runs before hooks, permission checks, and BashTool::call.
    orch.run_turn("run a command with a hidden control character")
        .await
        .expect("schema rejection is a recoverable tool result");

    let session = orch.session();
    let state = session.lock().await;
    let result = state.history.iter().find_map(|message| {
        let ConversationMessage::User { content, .. } = message else {
            return None;
        };
        content.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id: result_id,
                content,
                is_error,
                ..
            } if result_id == &tool_use_id
                && content.contains("command contains control characters") =>
            {
                Some((content.clone(), is_error.unwrap_or(false)))
            }
            _ => None,
        })
    });
    let (content, is_error) = result.expect("Bash schema error tool result");
    let raw = concat!(
        "[\n",
        "  {\n",
        "    \"code\": \"custom\",\n",
        "    \"path\": [\n",
        "      \"command\"\n",
        "    ],\n",
        "    \"message\": \"command contains control characters that would be hidden in the approval dialog\"\n",
        "  }\n",
        "]"
    );
    assert_eq!(
        content,
        format!("<tool_use_error>InputValidationError: {raw}</tool_use_error>")
    );
    assert!(is_error);
    assert_eq!(permission_checks.load(Ordering::SeqCst), 0);
    assert_eq!(process_calls.load(Ordering::SeqCst), 0);
}
