#![cfg(target_os = "macos")]

//! Q01 in-flight continuation against the actual isolated worker and an
//! OpenAI-Chat-compatible loopback provider. A kill is a real process-group
//! termination while the provider stream or the Chat broker is held open.
//! Environment-sensitive: run with --test-threads=1.

#[path = "team_account_fixture.rs"]
mod team_account_fixture;

use harness_runtime::desktop::team_invocation::TeamInvocationLimits;
use harness_runtime::desktop::team_process::{
    inspect_task, TeamContinuation, TeamKnowledgeDocument, TeamProcessDeadline, TeamProcessError,
    TeamProcessOutcome, TeamProcessRequest, TeamProcessResult, TeamProcessStart, TeamTaskPhase,
    TeamWaitAnswer,
};
use lingxi_llm_client::protocol::ProtocolFamily;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "fixture-chat-model";
const KEY: &str = "synthetic-selected-team-key";
const PERSONA: &str = "IMMUTABLE_TEAM_CONTINUATION_PERSONA";
const CHAT: ProtocolFamily = ProtocolFamily::OpenAiChat;

fn worker() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_harness-team-worker"))
        .canonicalize()
        .unwrap()
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn scope() -> harness_runtime::desktop::session_state::BotChatScope {
    harness_runtime::desktop::session_state::BotChatScope {
        team_id: "team_0001".into(),
        shared_bot_id: "sharedbot_01".into(),
        conversation_id: "chat_0001".into(),
        session_epoch: 1,
    }
}
fn root() -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, path)
}
fn batch(
    from: u64,
    to: u64,
    revision: u64,
) -> harness_runtime::desktop::session_state::ChatInputBatch {
    serde_json::from_value(json!({"schemaVersion":1,"batchId":format!("batch_{from:04}_{to:04}"),"teamId":"team_0001","sharedBotId":"sharedbot_01","conversationId":"chat_0001","sessionEpoch":1,"expectedSessionRevision":revision,"fromSeqExclusive":from,"toSeqInclusive":to,"lastChangeSeq":to,"bootstrap":revision==0,"currentMessageId":format!("message_{to:04}"),"changes":[],"messages":((from+1)..=to).map(|seq|json!({"messageId":format!("message_{seq:04}"),"seq":seq,"changeSeq":seq,"changeId":format!("change_{seq:04}"),"author":{"kind":"user","uid":if seq%2==0{"alice"}else{"bob"},"displayName":"Same name"},"createdAt":1_700_000_000_000u64+seq,"text":format!("SOURCE_{seq:04} short text")})).collect::<Vec<_>>()})).unwrap()
}

struct Task {
    id: uuid::Uuid,
    task_id: String,
    input: harness_runtime::desktop::session_state::ChatInputBatch,
    /// Offer the ask-the-requester tool.
    ask: bool,
}
impl Task {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4();
        Self {
            id,
            task_id: format!("task_{}", id.simple()),
            input: batch(0, 2, 0),
            ask: false,
        }
    }
    /// Same task and batch; a fresh invocation identity, like a new host permit.
    fn request(
        &self,
        root: &Path,
        model: harness_runtime::desktop::team_process::TeamProcessModel,
    ) -> TeamProcessRequest {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;
        let bytes = serde_json::to_vec(&self.input).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let path = root.join(format!("{digest}.json"));
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        TeamProcessRequest {
            scope: self.input.scope(),
            task_id: self.task_id.clone(),
            invocation_id: uuid::Uuid::new_v4(),
            instructions: PERSONA.into(),
            input_batch_ref: harness_runtime::desktop::team_process::TeamDataRef {
                path,
                sha256: digest,
                bytes: bytes.len() as u64,
            },
            knowledge: vec![TeamKnowledgeDocument {
                id: "shared_knowledge".into(),
                title: "Published guide".into(),
                text: "PUBLISHED_KNOWLEDGE_SENTINEL".into(),
            }],
            limits: TeamInvocationLimits {
                context_window_tokens: std::num::NonZeroU32::new(200_000).unwrap(),
                ..Default::default()
            },
            model,
            deadline: TeamProcessDeadline::from_backend_unix_millis(now_ms() + 300_000).unwrap(),
            ask_requester: self.ask,
        }
    }
    fn chat_model(
        &self,
        endpoint: &str,
    ) -> harness_runtime::desktop::team_process::TeamProcessModel {
        team_account_fixture::openai_chat_model(MODEL.into(), endpoint.into(), KEY.into()).unwrap()
    }
}

#[derive(Default)]
struct Broker {
    hang_reads: AtomicBool,
    read_calls: AtomicUsize,
}
#[async_trait::async_trait]
impl harness_runtime::desktop::team_process::TeamProcessBroker for Broker {
    async fn authorize(&self, _: CancellationToken) -> Result<(), TeamProcessError> {
        Ok(())
    }
    async fn read_chat(
        &self,
        request: harness_runtime::desktop::team_process::TeamChatReadRequest,
        cancel: CancellationToken,
    ) -> Result<harness_runtime::desktop::team_process::TeamChatReadResult, TeamProcessError> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        if self.hang_reads.load(Ordering::Acquire) {
            cancel.cancelled().await;
            return Err(TeamProcessError::Cancelled);
        }
        match request.query {
            harness_runtime::desktop::team_process::TeamChatQuery::Before { before_seq, limit } => {
                Ok(harness_runtime::desktop::team_process::TeamChatReadResult {
                    messages: batch(
                        (before_seq - 1).saturating_sub(limit as u64),
                        before_seq - 1,
                        0,
                    )
                    .messages,
                    complete: true,
                    next_before_seq: None,
                })
            }
            _ => Err(TeamProcessError::Protocol),
        }
    }
}

#[derive(Clone)]
struct Captured {
    body: Value,
}
enum Reply {
    Text(String),
    Tool(&'static str, Value),
    /// Send a text delta and keep the response open until the worker dies.
    Partial(String),
    /// Accept the request and never answer.
    Hang,
}
struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    held: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}
fn usage() -> Value {
    json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":1,"model":MODEL,"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":3,"total_tokens":4}})
}
fn sse(chunks: Vec<Value>) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}
impl Server {
    async fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let held = Arc::new(AtomicUsize::new(0));
        let holding = held.clone();
        let task = tokio::spawn(async move {
            for (index, reply) in replies.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http(&mut stream).await;
                captured.lock().unwrap().push(request);
                let body = match reply {
                    Reply::Hang | Reply::Partial(_) => {
                        if let Reply::Partial(text) = reply {
                            let first = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {}\n\n",
                                chunk(json!({"role":"assistant","content":text}), None)
                            );
                            let _ = stream.write_all(first.as_bytes()).await;
                            let _ = stream.flush().await;
                        }
                        holding.fetch_add(1, Ordering::SeqCst);
                        tokio::spawn(async move {
                            let mut byte = [0; 1];
                            let _ = stream.read(&mut byte).await;
                        });
                        continue;
                    }
                    Reply::Text(text) => sse(vec![
                        chunk(json!({"role":"assistant","content":text}), None),
                        chunk(json!({}), Some("stop")),
                        usage(),
                    ]),
                    Reply::Tool(name, input) => sse(vec![
                        chunk(
                            json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_fixture_{index}"),"type":"function","function":{"name":name,"arguments":input.to_string()}}]}),
                            None,
                        ),
                        chunk(json!({}), Some("tool_calls")),
                        usage(),
                    ]),
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            endpoint,
            requests,
            held,
            task,
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    async fn wait_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.count() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual SDK request must arrive");
    }
    async fn wait_held(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.held.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("provider response must be held open");
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn read_http(stream: &mut TcpStream) -> Captured {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buf = [0; 4096];
        let size = stream.read(&mut buf).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buf[..size]);
        if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let len = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .expect("actual SDK request must have a byte body");
    while bytes.len() - end < len {
        let mut buf = [0; 4096];
        let size = stream.read(&mut buf).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buf[..size]);
    }
    Captured {
        body: serde_json::from_slice(&bytes[end..end + len]).unwrap(),
    }
}

/// Every assistant tool call is answered by exactly one tool result before any
/// other message, and no tool result is orphaned.
fn assert_legal_sequence(body: &Value) {
    let messages = body["messages"].as_array().expect("chat messages");
    let mut index = 0;
    let mut seen_calls = std::collections::HashSet::new();
    while index < messages.len() {
        let message = &messages[index];
        assert_ne!(message["role"], "tool", "orphaned tool result at {index}");
        index += 1;
        let Some(calls) = message["tool_calls"].as_array() else {
            continue;
        };
        let mut wanted: std::collections::BTreeSet<String> = calls
            .iter()
            .map(|call| call["id"].as_str().unwrap().to_owned())
            .collect();
        for id in &wanted {
            assert!(seen_calls.insert(id.clone()), "duplicate call id {id}");
        }
        while !wanted.is_empty() {
            let result = messages
                .get(index)
                .unwrap_or_else(|| panic!("tool call {wanted:?} has no result"));
            assert_eq!(result["role"], "tool", "call {wanted:?} unanswered");
            let id = result["tool_call_id"].as_str().unwrap();
            assert!(wanted.remove(id), "result {id} answers no pending call");
            index += 1;
        }
    }
}
fn tool_results(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .cloned()
        .collect()
}

fn transcript(root: &Path) -> String {
    fn walk(dir: &Path, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push_str(&std::fs::read_to_string(path).unwrap_or_default());
            }
        }
    }
    let mut out = String::new();
    walk(&root.join("scopes"), &mut out);
    out
}
async fn wait_transcript(root: &Path, needle: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !transcript(root).contains(needle) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("transcript never contained {needle}"));
}

type RunHandle = tokio::task::JoinHandle<Result<TeamProcessResult, TeamProcessError>>;
fn launch(start: TeamProcessStart, broker: Arc<Broker>) -> (RunHandle, CancellationToken) {
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    (
        tokio::spawn(async move { start.run_with_broker(&worker(), token, broker).await }),
        cancel,
    )
}
async fn kill(handle: RunHandle, cancel: CancellationToken) {
    cancel.cancel();
    assert!(matches!(
        handle.await.unwrap(),
        Err(TeamProcessError::Cancelled)
    ));
}
async fn run_to_end(
    start: TeamProcessStart,
    broker: Arc<Broker>,
) -> Result<TeamProcessResult, TeamProcessError> {
    tokio::time::timeout(
        Duration::from_secs(200),
        start.run_with_broker(&worker(), CancellationToken::new(), broker),
    )
    .await
    .expect("worker bounded")
}
fn first_run(
    task: &Task,
    root: &Path,
    endpoint: &str,
    broker: Arc<Broker>,
) -> (RunHandle, CancellationToken) {
    launch(
        TeamProcessStart::prepare(root, task.request(root, task.chat_model(endpoint))).unwrap(),
        broker,
    )
}
fn checkpoint(
    root: &Path,
    task: &Task,
) -> harness_runtime::desktop::team_process::TeamTaskCheckpoint {
    inspect_task(root, &scope(), &task.task_id, CHAT).unwrap()
}
fn continuation(
    root: &Path,
    task: &Task,
    endpoint: &str,
    epoch: u32,
    digest: &str,
) -> Result<TeamProcessStart, TeamProcessError> {
    TeamProcessStart::prepare_continuation(
        root,
        task.request(root, task.chat_model(endpoint)),
        TeamContinuation {
            epoch,
            expected_checkpoint_digest: digest.into(),
            answer: None,
        },
    )
}
fn answered(
    root: &Path,
    task: &Task,
    endpoint: &str,
    epoch: u32,
    digest: &str,
    wait_id: &str,
    text: &str,
) -> Result<TeamProcessStart, TeamProcessError> {
    TeamProcessStart::prepare_continuation(
        root,
        task.request(root, task.chat_model(endpoint)),
        TeamContinuation {
            epoch,
            expected_checkpoint_digest: digest.into(),
            answer: Some(TeamWaitAnswer {
                wait_id: wait_id.into(),
                message_id: "message_0004".into(),
                text: text.into(),
            }),
        },
    )
}
fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect()
}
fn history_call() -> Reply {
    Reply::Tool(
        "TeamChatHistory",
        json!({"kind":"before","before_seq":3,"limit":200}),
    )
}

#[tokio::test]
async fn kill_after_tool_call_persisted_before_result_continues_and_runs_tool_in_second_process() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![history_call(), Reply::Text("Continued answer".into())]).await;
    let first_broker = Arc::new(Broker::default());
    first_broker.hang_reads.store(true, Ordering::Release);
    let (handle, cancel) = first_run(&task, &root, &server.endpoint, first_broker.clone());
    server.wait_requests(1).await;
    wait_transcript(&root, "\"tool_use\"").await;
    while first_broker.read_calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    kill(handle, cancel).await;

    let inspected = checkpoint(&root, &task);
    assert_eq!(inspected.phase, TeamTaskPhase::InFlight);
    assert_eq!(inspected.attempts, 1);
    assert_eq!(inspected.dispatches_observed, 1);
    assert!(inspected.continuable, "{:?}", inspected.reason);
    assert_eq!(inspected.pending_tool_calls.len(), 1);
    let pending = &inspected.pending_tool_calls[0];
    assert_eq!(pending.tool_name, "TeamChatHistory");
    assert!(pending.read_only);
    assert!(!pending.logical_step_id.contains("call_fixture"));
    assert_eq!(pending.logical_step_id, format!("{}/t0/c0", task.task_id));
    assert_eq!(inspected.cursor.session_revision, 1);
    // Read-only and stable: a second inspection is identical.
    assert_eq!(checkpoint(&root, &task), inspected);

    let second_broker = Arc::new(Broker::default());
    let result = run_to_end(
        continuation(
            &root,
            &task,
            &server.endpoint,
            2,
            &inspected.checkpoint_digest,
        )
        .unwrap(),
        second_broker.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.text, "Continued answer");
    assert_eq!(second_broker.read_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.dispatched_requests, 2);
    assert_eq!(result.attempt_dispatched_requests, 1);
    assert_eq!(server.count(), 2);
    let second = server.requests.lock().unwrap()[1].body.clone();
    assert_legal_sequence(&second);
    // Without the host's opt-in the Bot cannot ask its requester anything.
    assert_eq!(tool_names(&second), ["TeamChatHistory", "TeamKnowledge"]);
    let results = tool_results(&second);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_fixture_0");
    assert!(results[0]["content"].to_string().contains("SOURCE_0001"));
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope())
            .unwrap()
            .session_revision,
        1
    );
    let after = checkpoint(&root, &task);
    assert_eq!(after.phase, TeamTaskPhase::ResultRecorded);
    assert_eq!(after.attempts, 2);
    assert_eq!(after.dispatches_observed, 2);
}

#[tokio::test]
async fn kill_after_tool_result_persisted_before_next_request_never_reruns_the_tool() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![
        history_call(),
        Reply::Hang,
        Reply::Text("After persisted result".into()),
    ])
    .await;
    let first_broker = Arc::new(Broker::default());
    let (handle, cancel) = first_run(&task, &root, &server.endpoint, first_broker.clone());
    server.wait_requests(2).await;
    server.wait_held(1).await;
    assert_eq!(first_broker.read_calls.load(Ordering::SeqCst), 1);
    kill(handle, cancel).await;

    let inspected = checkpoint(&root, &task);
    assert_eq!(inspected.phase, TeamTaskPhase::InFlight);
    assert!(inspected.pending_tool_calls.is_empty());
    assert!(inspected.continuable, "{:?}", inspected.reason);
    assert_eq!(inspected.dispatches_observed, 2);

    let second_broker = Arc::new(Broker::default());
    let result = run_to_end(
        continuation(
            &root,
            &task,
            &server.endpoint,
            2,
            &inspected.checkpoint_digest,
        )
        .unwrap(),
        second_broker.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.dispatched_requests, 3);
    assert_eq!(result.attempt_dispatched_requests, 1);
    assert_eq!(
        second_broker.read_calls.load(Ordering::SeqCst),
        0,
        "a persisted tool result must be reused, not executed again"
    );
    assert_eq!(first_broker.read_calls.load(Ordering::SeqCst), 1);
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let resumed = &requests[2].body;
    assert_legal_sequence(resumed);
    assert_eq!(tool_results(resumed).len(), 1);
    // The persisted result is exactly what the first process produced.
    assert_eq!(tool_results(resumed), tool_results(&requests[1].body));
}

#[tokio::test]
async fn kill_in_the_middle_of_a_model_response_reissues_that_request_once() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![
        Reply::Partial("PARTIAL_MUST_NOT_BE_KEPT".into()),
        Reply::Text("Complete answer".into()),
    ])
    .await;
    let (handle, cancel) = first_run(&task, &root, &server.endpoint, Arc::new(Broker::default()));
    server.wait_requests(1).await;
    server.wait_held(1).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    kill(handle, cancel).await;
    assert!(!transcript(&root).contains("PARTIAL_MUST_NOT_BE_KEPT"));

    let inspected = checkpoint(&root, &task);
    assert_eq!(inspected.phase, TeamTaskPhase::InFlight);
    assert!(inspected.pending_tool_calls.is_empty());
    assert!(inspected.continuable, "{:?}", inspected.reason);
    assert_eq!(inspected.dispatches_observed, 1);

    let result = run_to_end(
        continuation(
            &root,
            &task,
            &server.endpoint,
            2,
            &inspected.checkpoint_digest,
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.text, "Complete answer");
    // The one allowed repeat is counted.
    assert_eq!(result.dispatched_requests, 2);
    assert_eq!(result.attempt_dispatched_requests, 1);
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_legal_sequence(&request.body);
        assert_eq!(request.body.to_string().matches("SOURCE_0001").count(), 1);
        assert!(!request
            .body
            .to_string()
            .contains("PARTIAL_MUST_NOT_BE_KEPT"));
    }
    // No second prompt was added: both requests carry the same user turns.
    let users = |body: &Value| {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .count()
    };
    assert_eq!(users(&requests[0].body), users(&requests[1].body));
}

#[tokio::test]
async fn an_epoch_is_single_use_and_a_stale_digest_fails_before_any_dispatch() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![Reply::Hang, Reply::Text("Third epoch answer".into())]).await;
    let (handle, cancel) = first_run(&task, &root, &server.endpoint, Arc::new(Broker::default()));
    server.wait_held(1).await;
    kill(handle, cancel).await;

    // Stale digest: refused by the worker under the lease, zero dispatches.
    let stale = "0".repeat(64);
    assert!(matches!(
        run_to_end(
            continuation(&root, &task, &server.endpoint, 2, &stale).unwrap(),
            Arc::new(Broker::default())
        )
        .await,
        Err(TeamProcessError::CheckpointMismatch)
    ));
    assert_eq!(server.count(), 1);
    // The refused launch consumed epoch 2.
    let inspected = checkpoint(&root, &task);
    assert_eq!(inspected.attempts, 2);
    assert!(matches!(
        run_to_end(
            continuation(
                &root,
                &task,
                &server.endpoint,
                2,
                &inspected.checkpoint_digest
            )
            .unwrap(),
            Arc::new(Broker::default())
        )
        .await,
        Err(TeamProcessError::AlreadyAttempted)
    ));
    assert_eq!(server.count(), 1);
    // A lower epoch is stale as well; epoch 1 can never be reused.
    assert!(continuation(
        &root,
        &task,
        &server.endpoint,
        1,
        &inspected.checkpoint_digest
    )
    .is_err());
    assert!(matches!(
        TeamProcessStart::prepare(
            &root,
            task.request(&root, task.chat_model(&server.endpoint))
        )
        .unwrap()
        .run_with_broker(
            &worker(),
            CancellationToken::new(),
            Arc::new(Broker::default())
        )
        .await,
        Err(TeamProcessError::AlreadyAttempted)
    ));
    assert_eq!(server.count(), 1);

    // Epoch 3 with a fresh digest continues and completes.
    let fresh = checkpoint(&root, &task);
    assert_eq!(fresh.attempts, 2);
    let result = run_to_end(
        continuation(&root, &task, &server.endpoint, 3, &fresh.checkpoint_digest).unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.text, "Third epoch answer");
    // Only the lost first request and its single repeat reached the provider.
    assert_eq!(result.dispatched_requests, 2);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn continue_after_the_result_is_recorded_returns_it_with_zero_model_calls() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![Reply::Text("Recorded once".into())]).await;
    let first = run_to_end(
        TeamProcessStart::prepare(
            &root,
            task.request(&root, task.chat_model(&server.endpoint)),
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(first.text, "Recorded once");
    let inspected = checkpoint(&root, &task);
    assert_eq!(inspected.phase, TeamTaskPhase::ResultRecorded);
    assert!(inspected.continuable);
    let again = run_to_end(
        continuation(
            &root,
            &task,
            &server.endpoint,
            2,
            &inspected.checkpoint_digest,
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap(),
        serde_json::to_value(&first).unwrap()
    );
    assert_eq!(server.count(), 1);
    // No epoch was consumed for a recorded result.
    assert_eq!(checkpoint(&root, &task).attempts, 1);
}

#[tokio::test]
async fn unsupported_route_is_not_continuable_and_refused_before_dispatch() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![Reply::Hang]).await;
    let (handle, cancel) = first_run(&task, &root, &server.endpoint, Arc::new(Broker::default()));
    server.wait_held(1).await;
    kill(handle, cancel).await;
    // The same durable state on a route nobody has proved.
    let unsupported = inspect_task(
        &root,
        &scope(),
        &task.task_id,
        ProtocolFamily::AnthropicMessages,
    )
    .unwrap();
    assert!(!unsupported.continuable);
    assert_eq!(unsupported.reason, Some("route_unsupported"));
    let supported = checkpoint(&root, &task);
    assert!(supported.continuable);
    assert_eq!(supported.checkpoint_digest, unsupported.checkpoint_digest);
    let anthropic = team_account_fixture::model(
        "claude-sonnet-4-20250514".into(),
        server.endpoint.clone(),
        KEY.into(),
    )
    .unwrap();
    assert!(matches!(
        TeamProcessStart::prepare_continuation(
            &root,
            task.request(&root, anthropic),
            TeamContinuation {
                epoch: 2,
                expected_checkpoint_digest: supported.checkpoint_digest.clone(),
                answer: None,
            }
        ),
        Err(TeamProcessError::RouteUnsupported)
    ));
    assert_eq!(server.count(), 1);
    // Refusal consumed nothing.
    assert_eq!(checkpoint(&root, &task).attempts, 1);
}

#[tokio::test]
async fn continued_task_keeps_one_batch_exact_cursors_and_a_legal_provider_history() {
    let (_temp, root) = root();
    let task = Task::new();
    let server = Server::new(vec![
        history_call(),
        Reply::Hang,
        Reply::Text("Final".into()),
    ])
    .await;
    let (handle, cancel) = first_run(&task, &root, &server.endpoint, Arc::new(Broker::default()));
    server.wait_held(1).await;
    kill(handle, cancel).await;
    let inspected = checkpoint(&root, &task);
    let cursor_before = inspected.cursor;
    let result = run_to_end(
        continuation(
            &root,
            &task,
            &server.endpoint,
            2,
            &inspected.checkpoint_digest,
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(
        result.cursor.session_revision,
        cursor_before.session_revision
    );
    assert_eq!(result.cursor.last_message_seq, 2);
    assert_eq!(result.cursor.last_change_seq, 2);
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope()).unwrap(),
        result.cursor
    );

    // The batch is admitted exactly once in the canonical journal...
    {
        use harness_runtime::desktop::session_state::SessionStateCoordinator;
        let id = scope().session_id().unwrap();
        let home = root
            .join("scopes")
            .join(id.as_uuid().to_string())
            .join("home");
        let lease =
            lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
                .claim_session_id(&id.to_string(), std::process::id())
                .unwrap()
                .into_shared();
        let state = SessionStateCoordinator::open(&home, id, lease).unwrap();
        state.hydrate_blocking().unwrap();
        let batches = state.chat_input_batches().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].batch_id, task.input.batch_id);
    }
    // ...and once in the transcript projection and in every provider request.
    let projected = transcript(&root)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| {
            row["origin"]["kind"] == "shared_chat"
                && row["origin"]["batchId"] == task.input.batch_id
        })
        .count();
    assert_eq!(projected, 2);
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let resumed = &requests[2].body;
    assert_legal_sequence(resumed);
    assert_eq!(tool_results(resumed).len(), 1);
    // The persisted result is exactly what the first process produced.
    assert_eq!(tool_results(resumed), tool_results(&requests[1].body));
}

fn question() -> Reply {
    Reply::Tool(
        "TeamAskRequester",
        json!({"question":"Which city should I plan for?"}),
    )
}
/// The Bot asked: the worker ends as `waiting` with the ask call unfinished.
async fn ask_and_wait(task: &Task, root: &Path, server: &Server) -> TeamProcessResult {
    let result = run_to_end(
        TeamProcessStart::prepare(root, task.request(root, task.chat_model(&server.endpoint)))
            .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Waiting);
    assert_eq!(result.text, "");
    assert!(result.document.is_none());
    result
}

#[tokio::test]
async fn a_question_pauses_the_task_and_only_its_answer_continues_it() {
    let (_temp, root) = root();
    let mut task = Task::new();
    task.ask = true;
    let server = Server::new(vec![
        question(),
        Reply::Text("Planned for Lisbon".into()),
        Reply::Text("Next task answer".into()),
    ])
    .await;
    let paused = ask_and_wait(&task, &root, &server).await;
    assert_eq!(paused.attempt_dispatched_requests, 1);
    assert_eq!(server.count(), 1);
    let first = server.requests.lock().unwrap()[0].body.clone();
    assert_eq!(
        tool_names(&first),
        ["TeamAskRequester", "TeamChatHistory", "TeamKnowledge"]
    );
    assert!(!transcript(&root).contains("team_bot_result"));

    let inspected = checkpoint(&root, &task);
    assert_eq!(inspected.phase, TeamTaskPhase::InFlight);
    assert!(inspected.continuable, "{:?}", inspected.reason);
    assert_eq!(inspected.pending_tool_calls.len(), 1);
    assert_eq!(
        inspected.pending_tool_calls[0].tool_name,
        "TeamAskRequester"
    );
    assert!(!inspected.pending_tool_calls[0].read_only);
    let wait = inspected
        .wait
        .clone()
        .expect("the checkpoint reports the wait");
    assert_eq!(wait.question, "Which city should I plan for?");
    assert_eq!(wait.logical_step_id, format!("{}/t0/c0", task.task_id));
    assert!(wait.wait_id.starts_with("wait_") && wait.wait_id.len() == 45);
    assert_eq!(wait.request_digest.len(), 64);
    assert_eq!(checkpoint(&root, &task), inspected);

    // No answer, or the answer to another question, never reaches the model.
    let refused = run_to_end(
        continuation(
            &root,
            &task,
            &server.endpoint,
            2,
            &inspected.checkpoint_digest,
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await;
    assert!(
        matches!(refused, Err(TeamProcessError::NotContinuable)),
        "{refused:?}"
    );
    let again = checkpoint(&root, &task);
    let other = run_to_end(
        answered(
            &root,
            &task,
            &server.endpoint,
            3,
            &again.checkpoint_digest,
            "wait_0000000000000000000000000000000000000000",
            "Porto",
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await;
    assert!(
        matches!(other, Err(TeamProcessError::NotContinuable)),
        "{other:?}"
    );
    assert_eq!(server.count(), 1);

    let ready = checkpoint(&root, &task);
    assert_eq!(
        ready.wait.as_ref(),
        Some(&wait),
        "the wait survives refused attempts"
    );
    let result = run_to_end(
        answered(
            &root,
            &task,
            &server.endpoint,
            4,
            &ready.checkpoint_digest,
            &wait.wait_id,
            "ANSWER_SENTINEL Lisbon",
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.text, "Planned for Lisbon");
    assert_eq!(result.dispatched_requests, 2);
    assert_eq!(result.attempt_dispatched_requests, 1);
    assert_eq!(server.count(), 2);
    let second = server.requests.lock().unwrap()[1].body.clone();
    assert_legal_sequence(&second);
    let results = tool_results(&second);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_fixture_0");
    assert!(results[0]["content"]
        .to_string()
        .contains("ANSWER_SENTINEL Lisbon"));
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope())
            .unwrap()
            .last_message_seq,
        2,
        "the answer is the ask call's result, not an admitted batch"
    );
    let done = checkpoint(&root, &task);
    assert_eq!(done.phase, TeamTaskPhase::ResultRecorded);
    assert!(done.wait.is_none());

    // The next request's batch carries the Bot's question and the requester's
    // answer as Chat messages. The session already holds both, so only the new
    // message is projected.
    let mut next = Task::new();
    let mut value = serde_json::to_value(batch(2, 5, 1)).unwrap();
    value["messages"][0]["author"] =
        json!({"kind":"shared_bot","sharedBotId":"sharedbot_01","displayName":"Bot"});
    value["messages"][0]["text"] = json!("QUESTION_ECHO Which city should I plan for?");
    value["messages"][0]["sourceTaskId"] = json!(task.task_id);
    value["messages"][0]["waitId"] = json!(wait.wait_id);
    value["messages"][1]["text"] = json!("ANSWER_ECHO Lisbon");
    value["messages"][1]["sourceTaskId"] = json!(task.task_id);
    value["messages"][1]["waitId"] = json!(wait.wait_id);
    next.input = serde_json::from_value(value).unwrap();
    let after = run_to_end(
        TeamProcessStart::prepare(
            &root,
            next.request(&root, next.chat_model(&server.endpoint)),
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(after.outcome, TeamProcessOutcome::Completed);
    let third = server.requests.lock().unwrap()[2].body.to_string();
    assert!(third.contains("SOURCE_0005"));
    assert!(!third.contains("QUESTION_ECHO") && !third.contains("ANSWER_ECHO"));
    assert!(
        third.contains("ANSWER_SENTINEL Lisbon"),
        "the answer stays as the tool result"
    );
    assert_legal_sequence(&server.requests.lock().unwrap()[2].body);
}

#[tokio::test]
async fn an_abandoned_question_does_not_block_the_next_task() {
    let (_temp, root) = root();
    let mut task = Task::new();
    task.ask = true;
    let server = Server::new(vec![
        question(),
        Reply::Text("Answer to the next request".into()),
    ])
    .await;
    ask_and_wait(&task, &root, &server).await;
    // The wait expired or was cancelled: the task is never continued.
    let mut next = Task::new();
    next.input = batch(2, 3, 1);
    let result = run_to_end(
        TeamProcessStart::prepare(
            &root,
            next.request(&root, next.chat_model(&server.endpoint)),
        )
        .unwrap(),
        Arc::new(Broker::default()),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.text, "Answer to the next request");
    let second = server.requests.lock().unwrap()[1].body.clone();
    assert_legal_sequence(&second);
    assert!(second.to_string().contains("SOURCE_0003"));
}
