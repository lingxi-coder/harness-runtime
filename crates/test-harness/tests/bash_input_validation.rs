//! End-to-end schema rejection for the production Bash tool.
//!
//! The command carries a control character that Native 2.1.291 rejects in the
//! Bash input schema. The real `BashTool` is registered in the real turn loop;
//! the mocked provider and process runner keep the probe local and command-free.

use async_trait::async_trait;
use lingxi_core::host::permission_gate::{PermissionDecision, PermissionGate};
use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
use lingxi_core::host::sandbox::SandboxedCommand;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, ToolUseId,
};
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

struct RecordingPermissionGate(std::sync::Mutex<Vec<Utf16JsonProjection>>);

#[async_trait]
impl PermissionGate for RecordingPermissionGate {
    async fn check(&self, name: &str, input: &Utf16JsonProjection) -> PermissionDecision {
        if name == "Bash" {
            self.0.lock().unwrap().push(input.clone());
        }
        PermissionDecision::Allow
    }
}

#[tokio::test]
async fn native_295_bash_aliases_reach_schema_permission_and_execution() {
    const CASES: &[(&str, &str, &str)] = &[
        ("alias-string", r#"{"command":"printf BASH_295_ALIAS > probe.ok","command_description":"Fixed alias description"}"#,
         include_str!("../../../docs/parity/current-alignment/fixtures/bash-input-295/native-baseline/2.1.295/alias-string/tool-input.raw.json")),
        ("existing-description", r#"{"command":"printf BASH_295_ALIAS > probe.ok","command_description":"Ignored alias","description":"Existing description"}"#,
         include_str!("../../../docs/parity/current-alignment/fixtures/bash-input-295/native-baseline/2.1.295/existing-description/tool-input.raw.json")),
        ("nonstring-alias", r#"{"command":"printf BASH_295_ALIAS > probe.ok","command_description":null}"#,
         include_str!("../../../docs/parity/current-alignment/fixtures/bash-input-295/native-baseline/2.1.295/nonstring-alias/tool-input.raw.json")),
        ("lone-surrogate-alias", r#"{"command":"printf BASH_295_ALIAS > probe.ok","command_description":"source-\ud800-tail"}"#,
         include_str!("../../../docs/parity/current-alignment/fixtures/bash-input-295/native-baseline/2.1.295/lone-surrogate-alias/tool-input.raw.json")),
        ("combined-timeout-alias", r#"{"command":"printf BASH_295_ALIAS > probe.ok","timeout_ms":"5000","command_description":"Combined aliases"}"#,
         include_str!("../../../docs/parity/current-alignment/fixtures/bash-input-295/native-baseline/2.1.295/combined-timeout-alias/tool-input.raw.json")),
    ];
    for (name, input, expected) in CASES {
        let process_calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(RecordingPermissionGate(std::sync::Mutex::new(Vec::new())));
        let mut context = tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        context.process = Arc::new(CountingProcessRunner(process_calls.clone()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(BashTool::new(context)));
        let api = Arc::new(MockApiClient::new(vec![
            mock_message_response(
                vec![LlmContentBlock::ToolCall {
                    id: "toolu_bash_input_295".into(),
                    name: "Bash".into(),
                    input: lingxi_llm_client::protocol::JsonPayload::parse(input).unwrap(),
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
        let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(registry),
            orchestrator::test_support::noop_hook_executor(),
            gate.clone(),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        ));
        orch.run_turn("Fixed Bash fixture.").await.expect(name);
        let inputs = gate.0.lock().unwrap();
        assert_eq!(
            inputs.len(),
            1,
            "{name}: repaired input must pass schema validation"
        );
        assert_eq!(
            inputs[0].to_json_string().unwrap().as_bytes(),
            expected.as_bytes(),
            "{name}: permission boundary must receive the original Native hook-input bytes"
        );
        assert!(
            process_calls.load(Ordering::SeqCst) > 0,
            "{name}: Bash execution reached the process boundary"
        );
    }
}

#[async_trait]
impl PermissionGate for CountingPermissionGate {
    async fn check(
        &self,
        _name: &str,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> PermissionDecision {
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
            vec![LlmContentBlock::ToolCall {
                id: tool_use_id.to_string(),
                name: "Bash".into(),
                input: lingxi_llm_client::protocol::JsonPayload::parse(
                    &serde_json::json!({"command":"printf '\u{1b}'"}).to_string(),
                )
                .unwrap(),
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
                && content
                    .string_at("")
                    .is_some_and(|text| text.contains("command contains control characters")) =>
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
        content.string_at(""),
        Some(format!("<tool_use_error>InputValidationError: {raw}</tool_use_error>").as_str())
    );
    assert!(is_error);
    assert_eq!(permission_checks.load(Ordering::SeqCst), 0);
    assert_eq!(process_calls.load(Ordering::SeqCst), 0);
}
