//! Prove a persisted Native per-turn effort marker survives a process restart,
//! the production runtime resume path, and the next first-party SDK request.
//!
//! The parent test writes two real JSONL transcript rows and re-executes this
//! integration-test binary. The child loads the transcript through
//! `ConversationOrchestrator::with_resume`, sends the next turn through
//! `ProviderApiAdapter` + `ApiService`, and inspects bytes at the SDK transport
//! boundary. No mock API client assigns the expected output fields.

use async_trait::async_trait;
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::jsonl::{session_path, JsonlMessage, JsonlReader, JsonlWriter};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tool_api::registry::ToolRegistry;
use uuid::Uuid;

const MODEL: &str = "claude-sonnet-5-5";
const CHILD_HOME_ENV: &str = "LINGXI_EFFORT_COLD_RESUME_295_HOME";
const CHILD_CWD_ENV: &str = "LINGXI_EFFORT_COLD_RESUME_295_CWD";
const CHILD_SESSION_ENV: &str = "LINGXI_EFFORT_COLD_RESUME_295_SESSION";
const HISTORICAL_EFFORT: &str = "low";
const NEXT_TURN_EFFORT: &str = "high";

#[derive(Clone, Debug)]
struct CapturedRequest {
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

struct FixedResponseTransport {
    requests: Mutex<Vec<CapturedRequest>>,
}

#[async_trait]
impl Transport for FixedResponseTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.requests
            .lock()
            .expect("request capture lock")
            .push(CapturedRequest {
                body: request.body.to_vec(),
                headers: request.headers,
            });
        let response = json!({
            "id": "msg_cold_resume_295",
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [{"type": "text", "text": "resumed request accepted"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 4}
        })
        .to_string();
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: response.into_bytes().into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<FixedResponseTransport>) -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": "https://api.anthropic.com",
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": false,
                    "structured_output": false
                }
            }]
        }]
    }))
    .expect("fixed first-party route config");

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "effort-cold-resume-295",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled),
    )
}

fn transcript_row(
    role: &str,
    row_id: Uuid,
    parent: Option<Uuid>,
    inner_id: Uuid,
    session_id: Uuid,
    cwd: &str,
    text: &str,
    per_turn_effort: Option<&str>,
) -> JsonlMessage {
    let mut row = json!({
        "type": role,
        "uuid": row_id.to_string(),
        "parentUuid": parent.map(|id| id.to_string()),
        "sessionId": session_id.to_string(),
        "timestamp": "2026-10-11T12:00:00.000Z",
        "cwd": cwd,
        "version": "2.1.295",
        "isSidechain": false,
        "userType": "external",
        "message": {
            "id": inner_id.to_string(),
            "role": role,
            "model": MODEL,
            "content": [{"type": "text", "text": text}],
            "stop_reason": if role == "assistant" { Some("end_turn") } else { None },
            "stop_sequence": null
        }
    });
    if let Some(effort) = per_turn_effort {
        row["perTurnEffort"] = Value::String(effort.to_owned());
    }
    serde_json::from_value(row).expect("valid native-shaped transcript row")
}

fn string_markers(body: &Value) -> Vec<(usize, String, String)> {
    body["messages"]
        .as_array()
        .expect("SDK body has messages")
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let effort = message.pointer("/output_config/effort")?.as_str()?;
            Some((
                index,
                message["role"].as_str()?.to_owned(),
                effort.to_owned(),
            ))
        })
        .collect()
}

async fn run_fresh_process_resume(home: PathBuf, cwd: PathBuf, session_id: Uuid) {
    let cwd_string = cwd.to_string_lossy().into_owned();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(
        cwd.parent()
            .expect("test project has a parent directory")
            .to_path_buf(),
    ));
    let path = session_path(&home, &cwd_string, &session_id.to_string());
    let rows = JsonlReader::new(path.clone(), fs.clone())
        .read_all()
        .await
        .expect("fresh child reads JSONL from disk");
    assert_eq!(
        rows.len(),
        2,
        "child starts with the persisted two-row history"
    );
    assert_eq!(
        rows[1].extra.string_at("/perTurnEffort"),
        Some(HISTORICAL_EFFORT),
        "the source-level Native perTurnEffort field survived JSONL write/read"
    );

    let transport = Arc::new(FixedResponseTransport {
        requests: Mutex::new(Vec::new()),
    });
    let api: Arc<dyn orchestrator::OrchestratorApiClient> = Arc::new(
        ProviderApiAdapter::new(api_service(transport.clone()))
            .with_initial_effort(Some(json!(NEXT_TURN_EFFORT))),
    );
    let writer = Arc::new(JsonlWriter::new(path, fs.clone()));
    let orchestrator = ConversationOrchestrator::with_resume(
        OrchestratorConfig {
            model: MODEL.to_owned(),
            bare: true,
            max_turns: 1,
            effort: Some(NEXT_TURN_EFFORT.to_owned()),
            ..OrchestratorConfig::default()
        },
        session_id,
        home,
        cwd_string,
        fs,
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
        Some(writer),
    )
    .await
    .expect("production runtime cold-resume path loads the on-disk transcript");
    let orchestrator = ConversationOrchestrator::into_shared(orchestrator);

    let resumed_history = orchestrator.conversation_transcript().await;
    assert!(
        resumed_history.iter().any(|message| matches!(
            message,
            lingxi_core::types::ConversationMessage::Assistant {
                per_turn_effort: Some(effort), ..
            } if effort.string_at("") == Some(HISTORICAL_EFFORT)
        )),
        "the fresh orchestrator reconstructed the assistant effort from disk"
    );

    let outcome = orchestrator
        .run_turn("continue after process restart")
        .await
        .expect("next turn uses the production ApiService request path");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let requests = transport
        .requests
        .lock()
        .expect("captured production SDK request")
        .clone();
    assert_eq!(requests.len(), 1, "one next-turn SDK request was sent");
    let body: Value =
        serde_json::from_slice(&requests[0].body).expect("captured SDK request body is valid JSON");
    let messages = body["messages"]
        .as_array()
        .expect("encoded request messages");
    assert!(
        messages.iter().any(|message| {
            message["role"] == "assistant"
                && message["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|block| block["text"] == "historical answer")
                })
        }),
        "the actual next request contains the restored historical assistant response"
    );
    assert!(
        messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|block| block["text"] == "continue after process restart")
                })
        }),
        "the request is the new turn following disk resume"
    );
    assert_eq!(
        string_markers(&body),
        vec![
            (1, "system".to_owned(), HISTORICAL_EFFORT.to_owned()),
            (4, "system".to_owned(), NEXT_TURN_EFFORT.to_owned()),
        ],
        "the SDK encoder carries the historical low marker before the old assistant and the current high marker after the resumed history, matching the Native 2.1.295 fixture"
    );
    let beta_header = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| value.as_str())
        .expect("first-party request carries SDK beta header");
    assert!(
        beta_header.contains("per-turn-control-2026-07-01"),
        "the actual SDK request enables the protocol needed for message-level effort"
    );
}

#[tokio::test]
async fn cold_resume_295_fresh_process_preserves_effort_into_next_sdk_request() {
    if std::env::var_os(CHILD_HOME_ENV).is_some() {
        let home = PathBuf::from(std::env::var_os(CHILD_HOME_ENV).expect("child home"));
        let cwd = PathBuf::from(std::env::var_os(CHILD_CWD_ENV).expect("child cwd"));
        let session_id = Uuid::parse_str(
            std::env::var(CHILD_SESSION_ENV)
                .expect("child session id")
                .as_str(),
        )
        .expect("valid child session id");
        run_fresh_process_resume(home, cwd, session_id).await;
        return;
    }

    let temp = TempDir::new().expect("test temp directory");
    let root = temp.path();
    let cwd = root.join("project");
    tokio::fs::create_dir_all(&cwd)
        .await
        .expect("create project cwd");
    let cwd_string = cwd.to_string_lossy().into_owned();
    let home = root.join("home");
    let session_id = Uuid::new_v4();
    let path = session_path(&home, &cwd_string, &session_id.to_string());
    tokio::fs::create_dir_all(path.parent().expect("session directory"))
        .await
        .expect("create transcript directory");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(root.to_path_buf()));
    let writer = JsonlWriter::new(path, fs);
    let user_id = Uuid::new_v4();
    let assistant_id = Uuid::new_v4();
    writer
        .append(&transcript_row(
            "user",
            user_id,
            None,
            user_id,
            session_id,
            &cwd_string,
            "the earlier question",
            None,
        ))
        .await
        .expect("persist prior user turn");
    writer
        .append(&transcript_row(
            "assistant",
            assistant_id,
            Some(user_id),
            assistant_id,
            session_id,
            &cwd_string,
            "historical answer",
            Some(HISTORICAL_EFFORT),
        ))
        .await
        .expect("persist assistant perTurnEffort through the real JSONL writer");

    let child = Command::new(std::env::current_exe().expect("current integration-test binary"))
        .arg("--exact")
        .arg("cold_resume_295_fresh_process_preserves_effort_into_next_sdk_request")
        .arg("--nocapture")
        .env(CHILD_HOME_ENV, &home)
        .env(CHILD_CWD_ENV, &cwd)
        .env(CHILD_SESSION_ENV, session_id.to_string())
        .output()
        .expect("re-execute this test in a fresh process");
    assert!(
        child.status.success(),
        "fresh-process cold-resume assertion failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}
