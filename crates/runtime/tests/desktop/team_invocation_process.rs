#![cfg(target_os = "macos")]

//! Actual dedicated worker + actual SDK HTTP requests, synthetic loopback only.
//! Environment fixtures require --test-threads=1 and always restore old values.

#[path = "team_account_fixture.rs"]
mod team_account_fixture;

use harness_runtime::desktop::team_invocation::TeamInvocationLimits;
use harness_runtime::desktop::team_process::{
    TeamProcessDeadline, TeamProcessError, TeamProcessOutcome, TeamProcessRequest, TeamProcessStart,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "claude-sonnet-4-20250514";
const KEY: &str = "synthetic-selected-team-key";
const PERSONA: &str = "IMMUTABLE_TEAM_PROCESS_PERSONA";

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
    serde_json::from_value(json!({"schemaVersion":1,"batchId":format!("batch_{from:04}_{to:04}"),"teamId":"team_0001","sharedBotId":"sharedbot_01","conversationId":"chat_0001","sessionEpoch":1,"expectedSessionRevision":revision,"fromSeqExclusive":from,"toSeqInclusive":to,"lastChangeSeq":to,"bootstrap":revision==0,"currentMessageId":format!("message_{to:04}"),"changes":[],"messages":((from+1)..=to).map(|seq|json!({"messageId":format!("message_{seq:04}"),"seq":seq,"changeSeq":seq,"changeId":format!("change_{seq:04}"),"author":{"kind":"user","uid":if seq%2==0{"alice"}else{"bob"},"displayName":"Same name"},"createdAt":1_700_000_000_000u64+seq,"text":format!("SOURCE_{seq:04} {}","word ".repeat(150))})).collect::<Vec<_>>()})).unwrap()
}
fn request(
    root: &Path,
    endpoint: String,
    id: uuid::Uuid,
    input: harness_runtime::desktop::session_state::ChatInputBatch,
) -> TeamProcessRequest {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let bytes = serde_json::to_vec(&input).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let path = root.join(format!("{digest}.json"));
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    TeamProcessRequest {
        scope: input.scope(),
        task_id: format!("task_{}", id.simple()),
        invocation_id: id,
        instructions: PERSONA.into(),
        input_batch_ref: harness_runtime::desktop::team_process::TeamDataRef {
            path,
            sha256: digest,
            bytes: bytes.len() as u64,
        },
        knowledge: vec![
            harness_runtime::desktop::team_process::TeamKnowledgeDocument {
                id: "shared_knowledge".into(),
                title: "Published guide".into(),
                text: "PUBLISHED_KNOWLEDGE_SENTINEL".into(),
            },
        ],
        limits: TeamInvocationLimits {
            context_window_tokens: std::num::NonZeroU32::new(200_000).unwrap(),
            ..Default::default()
        },
        model: team_account_fixture::model(MODEL.into(), endpoint, KEY.into()).unwrap(),
        deadline: TeamProcessDeadline::from_backend_unix_millis(now_ms() + 300_000).unwrap(),
    }
}
#[derive(Default)]
struct Broker {
    revoked: AtomicBool,
    authorizations: std::sync::atomic::AtomicUsize,
    max_authorizations: std::sync::atomic::AtomicUsize,
    reads: Mutex<Vec<harness_runtime::desktop::team_process::TeamChatQuery>>,
}
#[async_trait::async_trait]
impl harness_runtime::desktop::team_process::TeamProcessBroker for Broker {
    async fn authorize(&self, _: CancellationToken) -> Result<(), TeamProcessError> {
        let count = self.authorizations.fetch_add(1, Ordering::SeqCst) + 1;
        let max = self.max_authorizations.load(Ordering::Acquire);
        if self.revoked.load(Ordering::Acquire) || (max > 0 && count > max) {
            Err(TeamProcessError::Cancelled)
        } else {
            Ok(())
        }
    }
    async fn read_chat(
        &self,
        request: harness_runtime::desktop::team_process::TeamChatReadRequest,
        _: CancellationToken,
    ) -> Result<harness_runtime::desktop::team_process::TeamChatReadResult, TeamProcessError> {
        self.reads.lock().unwrap().push(request.query.clone());
        let messages = match request.query {
            harness_runtime::desktop::team_process::TeamChatQuery::Before { before_seq, limit } => {
                batch(
                    (before_seq - 1).saturating_sub(limit as u64),
                    before_seq - 1,
                    0,
                )
                .messages
            }
            _ => return Err(TeamProcessError::Protocol),
        };
        Ok(harness_runtime::desktop::team_process::TeamChatReadResult {
            messages,
            complete: true,
            next_before_seq: None,
        })
    }
}
async fn run(
    root: &Path,
    request: TeamProcessRequest,
) -> Result<harness_runtime::desktop::team_process::TeamProcessResult, TeamProcessError> {
    run_broker(root, request, Arc::new(Broker::default())).await
}
async fn run_broker(
    root: &Path,
    request: TeamProcessRequest,
    broker: Arc<Broker>,
) -> Result<harness_runtime::desktop::team_process::TeamProcessResult, TeamProcessError> {
    tokio::time::timeout(
        Duration::from_secs(305),
        TeamProcessStart::prepare(root, request)
            .unwrap()
            .run_with_broker(&worker(), CancellationToken::new(), broker),
    )
    .await
    .expect("worker bounded")
}

struct Environment(Vec<(&'static str, Option<OsString>)>);
impl Environment {
    fn set(values: Vec<(&'static str, OsString)>) -> Self {
        Self(
            values
                .into_iter()
                .map(|(key, value)| {
                    let old = std::env::var_os(key);
                    std::env::set_var(key, value);
                    (key, old)
                })
                .collect(),
        )
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[derive(Clone)]
struct Captured {
    headers: String,
    body: Value,
}
enum Reply {
    Text(String),
    TextWithFields(String),
    Bash(String),
    Tool(String, Value),
    Error,
    Hang,
}
struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    disconnected: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let disconnected = Arc::new(AtomicBool::new(false));
        let closed = disconnected.clone();
        let task = tokio::spawn(async move {
            for (reply_index, reply) in replies.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http(&mut stream).await;
                captured.lock().unwrap().push(request);
                if matches!(reply, Reply::Hang) {
                    let mut byte = [0; 1];
                    let read = stream.read(&mut byte).await;
                    closed.store(matches!(read, Ok(0) | Err(_)), Ordering::Release);
                    return;
                }
                let (status, kind, body) = match reply {
                    Reply::Text(text) => ("200 OK", "text/event-stream", sse(json!({"type":"text","text":text}), "end_turn")),
                    Reply::TextWithFields(text) => ("200 OK", "text/event-stream", sse(json!({"type":"text","text":text,"provider_extra":{"preserved":true}}), "end_turn")),
                    Reply::Bash(command) => ("200 OK", "text/event-stream", sse(json!({"type":"tool_use","id":"tool_fixture_bash","name":"Bash","input":{"command":command}}), "tool_use")),
                    Reply::Tool(name,input) => ("200 OK","text/event-stream",sse(json!({"type":"tool_use","id":format!("tool_read_shared_{reply_index}"),"name":name,"input":input}),"tool_use")),
                    Reply::Error => ("400 Bad Request", "application/json", json!({"error":{"type":"invalid_request_error","message":"SYNTHETIC_PROVIDER_ERROR_MUST_NOT_BE_RETURNED"}}).to_string()),
                    Reply::Hang => unreachable!(),
                };
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            endpoint,
            requests,
            disconnected,
            task,
        }
    }
    async fn wait_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.requests.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual SDK request must arrive");
    }
    async fn wait_disconnect(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !self.disconnected.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker provider connection must close");
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
        assert!(bytes.len() < 4 * 1024 * 1024);
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
    assert!(len < 4 * 1024 * 1024);
    while bytes.len() - end < len {
        let mut buf = [0; 4096];
        let size = stream.read(&mut buf).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buf[..size]);
    }
    Captured {
        headers,
        body: serde_json::from_slice(&bytes[end..end + len]).unwrap(),
    }
}
fn sse(mut block: Value, reason: &str) -> String {
    let delta = if block["type"] == "tool_use" {
        let input = block["input"].take();
        block["input"] = json!({});
        Some(json!({"type":"input_json_delta","partial_json": input.to_string()}))
    } else {
        None
    };
    let mut frames = vec![
        json!({"type":"message_start","message":{"id":format!("msg_fixture_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
    ];
    if let Some(delta) = delta {
        frames.push(json!({"type":"content_block_delta","index":0,"delta":delta}));
    }
    frames.extend([
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]);
    frames
        .iter()
        .map(|frame| {
            format!(
                "event: {}\ndata: {frame}\n\n",
                frame["type"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn complete_200_bootstrap_and_450_delta_survive_worker_restart_with_author_time() {
    let (_temp, root) = root();
    let server = Server::new(vec![
        Reply::Text("First completed result".into()),
        Reply::Text("Second completed result".into()),
    ])
    .await;
    let first = run(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(50, 250, 0),
        ),
    )
    .await
    .unwrap();
    assert_eq!(first.outcome, TeamProcessOutcome::Completed);
    assert_eq!(first.cursor.last_message_seq, 250);
    let next = batch(250, 700, 1);
    assert!(serde_json::to_vec(&next).unwrap().len() > 256 * 1024);
    let second = run(
        &root,
        request(&root, server.endpoint.clone(), uuid::Uuid::new_v4(), next),
    )
    .await
    .unwrap();
    assert_eq!(second.outcome, TeamProcessOutcome::Completed);
    assert_eq!(second.cursor.last_message_seq, 700);
    assert_eq!(second.cursor.session_revision, 2);
    let cursor = harness_runtime::desktop::team_process::inspect_scope(&root, &scope()).unwrap();
    assert_eq!(cursor, second.cursor);
    let records = server.requests.lock().unwrap();
    assert_eq!(records.len(), 2);
    let first_body = records[0].body.to_string();
    assert!(!first_body.contains("SOURCE_0050"));
    for seq in 51..=250 {
        assert!(first_body.contains(&format!("SOURCE_{seq:04}")));
    }
    let body = records[1].body.to_string();
    for seq in 251..=700 {
        assert!(body.contains(&format!("SOURCE_{seq:04}")), "missing {seq}");
    }
    assert!(body.contains("First completed result"));
    assert!(
        body.contains("alice")
            && body.contains("bob")
            && body.contains("Same name")
            && body.contains("1700000000700")
    );
    assert_eq!(
        second.document.as_ref().unwrap().markdown,
        "Second completed result"
    );
    assert!(second.finished_at <= now_ms());
}
#[tokio::test]
async fn edits_and_recalls_remove_stale_source_and_derived_output_before_next_dispatch() {
    let (_temp, root) = root();
    let server = Server::new(vec![
        Reply::Text("STALE_DERIVED_SECRET".into()),
        Reply::Text("Fresh result".into()),
        Reply::Text("Third result".into()),
    ])
    .await;
    run(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 2, 0),
        ),
    )
    .await
    .unwrap();
    let mut next = batch(2, 3, 1);
    let mut changed = batch(0, 2, 0).messages;
    changed[0].text = "CORRECTED_SOURCE".into();
    changed[0].change_seq = 4;
    changed[0].change_id = "change_edit_0001".into();
    changed[0].edited_at = Some(now_ms());
    changed[1].text.clear();
    changed[1].recalled = Some(true);
    changed[1].change_seq = 5;
    changed[1].change_id = "change_recall_02".into();
    next.changes = changed;
    next.last_change_seq = 5;
    let result = run(
        &root,
        request(&root, server.endpoint.clone(), uuid::Uuid::new_v4(), next),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    let mut third = batch(3, 4, 2);
    third.last_change_seq = 6;
    third.messages[0].change_seq = 6;
    run(
        &root,
        request(&root, server.endpoint.clone(), uuid::Uuid::new_v4(), third),
    )
    .await
    .unwrap();
    let records = server.requests.lock().unwrap();
    for i in [1, 2] {
        let body = records[i].body.to_string();
        assert!(body.contains("CORRECTED_SOURCE"));
        assert!(!body.contains("SOURCE_0001"));
        assert!(!body.contains("SOURCE_0002"));
        assert!(!body.contains("STALE_DERIVED_SECRET"));
    }
}
#[tokio::test]
async fn readonly_history_and_published_knowledge_reach_real_model_without_owner_tools() {
    let (_temp, root) = root();
    let server = Server::new(vec![
        Reply::Tool(
            "TeamChatHistory".into(),
            json!({"kind":"before","before_seq":51,"limit":200}),
        ),
        Reply::Tool(
            "TeamKnowledge".into(),
            json!({"documentId":"shared_knowledge"}),
        ),
        Reply::Text("Cited response".into()),
    ])
    .await;
    let broker = Arc::new(Broker::default());
    let result = run_broker(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(50, 250, 0),
        ),
        broker.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(broker.reads.lock().unwrap().len(), 1);
    assert!(broker.authorizations.load(Ordering::SeqCst) >= 5);
    let records = server.requests.lock().unwrap();
    let tools = records[0].body["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names,
        std::collections::BTreeSet::from(["TeamChatHistory", "TeamKnowledge"])
    );
    let final_input = records[2].body.to_string();
    assert!(final_input.contains("SOURCE_0001"));
    assert!(final_input.contains("PUBLISHED_KNOWLEDGE_SENTINEL"));
}
#[tokio::test]
async fn static_capacity_failure_keeps_cursor_zero_and_dispatches_nothing() {
    let (_temp, root) = root();
    let server = Server::new(vec![Reply::Text("must not run".into())]).await;
    let mut req = request(
        &root,
        server.endpoint.clone(),
        uuid::Uuid::new_v4(),
        batch(0, 200, 0),
    );
    req.limits.context_window_tokens = std::num::NonZeroU32::new(8192).unwrap();
    assert!(run(&root, req).await.is_err());
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope())
            .unwrap()
            .session_revision,
        0
    );
    assert!(server.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn authorization_refusal_dispatches_nothing_and_does_not_commit_input() {
    let (_temp, root) = root();
    let server = Server::new(vec![Reply::Text("must not run".into())]).await;
    let broker = Arc::new(Broker::default());
    broker.revoked.store(true, Ordering::Release);
    assert!(run_broker(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 1, 0)
        ),
        broker
    )
    .await
    .is_err());
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope())
            .unwrap()
            .session_revision,
        0
    );
    assert!(server.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn a_started_task_never_reexecutes_after_result_loss() {
    let (_temp, root) = root();
    let server = Server::new(vec![Reply::Text("One computation".into())]).await;
    let id = uuid::Uuid::new_v4();
    run(
        &root,
        request(&root, server.endpoint.clone(), id, batch(0, 1, 0)),
    )
    .await
    .unwrap();
    assert!(matches!(
        run(
            &root,
            request(&root, server.endpoint.clone(), id, batch(0, 1, 0))
        )
        .await,
        Err(TeamProcessError::AlreadyAttempted)
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn cancellation_terminates_live_provider_and_child_process() {
    let (_temp, root) = root();
    let server = Server::new(vec![Reply::Hang]).await;
    let cancel = CancellationToken::new();
    let start = TeamProcessStart::prepare(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 1, 0),
        ),
    )
    .unwrap();
    let token = cancel.clone();
    let running = tokio::spawn(async move {
        start
            .run_with_broker(&worker(), token, Arc::new(Broker::default()))
            .await
    });
    server.wait_requests(1).await;
    cancel.cancel();
    assert!(matches!(
        running.await.unwrap(),
        Err(TeamProcessError::Cancelled)
    ));
    server.wait_disconnect().await;
}
#[tokio::test]
async fn private_worker_does_not_inherit_environment_or_owner_registry() {
    let (_temp, root) = root();
    let owner = root.join("owner");
    std::fs::create_dir_all(owner.join(".lingxi")).unwrap();
    std::fs::write(
        owner.join(".lingxi/CLAUDE.md"),
        "SYNTHETIC_OWNER_PRIVATE_TEXT",
    )
    .unwrap();
    let _env = Environment::set(vec![
        ("HOME", owner.clone().into_os_string()),
        ("LINGXI_CONFIG_DIR", owner.join(".lingxi").into_os_string()),
        ("ANTHROPIC_API_KEY", "SYNTHETIC_OWNER_KEY".into()),
    ]);
    let server = Server::new(vec![Reply::Text("Private result".into())]).await;
    let result = run(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 1, 0),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    let requests = server.requests.lock().unwrap();
    let actual = format!("{}{}", requests[0].headers, requests[0].body);
    assert!(actual.contains(KEY));
    assert!(actual.contains(PERSONA));
    assert!(!actual.contains("SYNTHETIC_OWNER_"));
    assert!(!actual.contains(owner.to_str().unwrap()));
}
#[tokio::test]
async fn same_bot_result_echo_is_consumed_without_duplicate_model_content() {
    let (_temp, root) = root();
    let server = Server::new(vec![
        Reply::Text("UNIQUE_OWN_REPLY".into()),
        Reply::Text("Following answer".into()),
    ])
    .await;
    let req = request(
        &root,
        server.endpoint.clone(),
        uuid::Uuid::new_v4(),
        batch(0, 1, 0),
    );
    let task = req.task_id.clone();
    run(&root, req).await.unwrap();
    let mut next = batch(1, 3, 1);
    next.messages[0].author = harness_runtime::desktop::session_state::ChatAuthor::SharedBot {
        shared_bot_id: "sharedbot_01".into(),
        display_name: "Our Bot".into(),
    };
    next.messages[0].text = "UNIQUE_OWN_REPLY".into();
    next.messages[0].source_task_id = Some(task);
    let result = run(
        &root,
        request(&root, server.endpoint.clone(), uuid::Uuid::new_v4(), next),
    )
    .await
    .unwrap();
    assert_eq!(result.cursor.last_message_seq, 3);
    let records = server.requests.lock().unwrap();
    let body = records[1].body.to_string();
    assert_eq!(body.matches("UNIQUE_OWN_REPLY").count(), 1);
    assert_eq!(body.matches("SOURCE_0001").count(), 1);
}
#[tokio::test]
async fn journal_commit_before_transcript_is_repaired_once_by_existing_writer() {
    use harness_runtime::desktop::{
        session_state::SessionStateCoordinator, team_invocation::TeamInvocationProfile,
    };
    let (_temp, root) = root();
    let profile = TeamInvocationProfile::prepare(
        &root,
        scope(),
        uuid::Uuid::new_v4(),
        PERSONA.into(),
        Default::default(),
    )
    .unwrap();
    let id = scope().session_id().unwrap();
    let home = profile.scope_root().join("home");
    let lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"))
        .claim_session_id(&id.to_string(), std::process::id())
        .unwrap()
        .into_shared();
    let state = SessionStateCoordinator::open(&home, id, lease).unwrap();
    let worker = state.start().await.unwrap();
    state.append_chat_input(batch(0, 2, 0)).await.unwrap();
    state.close_and_drain().await.unwrap();
    worker.await.unwrap();
    drop(state);
    let server = Server::new(vec![Reply::Text("Recovered projection".into())]).await;
    let req = request(
        &root,
        server.endpoint.clone(),
        uuid::Uuid::new_v4(),
        batch(2, 3, 1),
    );
    let task = req.task_id.clone();
    let result = run(&root, req).await.unwrap();
    assert_eq!(result.cursor.last_message_seq, 3);
    let recovered = harness_runtime::desktop::team_process::recover_result(&root, &scope(), &task)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.text, result.text);
    assert_eq!(recovered.finished_at, result.finished_at);
    assert_eq!(
        recovered.document.unwrap().sha256,
        result.document.unwrap().sha256
    );
    let records = server.requests.lock().unwrap();
    let body = records[0].body.to_string();
    for seq in 1..=3 {
        assert_eq!(body.matches(&format!("SOURCE_{seq:04}")).count(), 1);
    }
}
#[tokio::test]
async fn model_cannot_execute_hallucinated_shell_calls() {
    let (_temp, root) = root();
    let marker = root.join("never-execute");
    let server = Server::new(vec![
        Reply::Bash(format!("touch {}", marker.display())),
        Reply::Text("Available read-only answer".into()),
    ])
    .await;
    let result = run(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 1, 0),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert!(!marker.exists());
    let records = server.requests.lock().unwrap();
    assert!(!records[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "Bash"));
}

#[tokio::test]
async fn authorization_is_rechecked_at_physical_dispatch_after_input_commit() {
    let (_temp, root) = root();
    let server = Server::new(vec![Reply::Text("must not dispatch".into())]).await;
    let broker = Arc::new(Broker::default());
    broker.max_authorizations.store(1, Ordering::Release);
    let outcome = run_broker(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 1, 0),
        ),
        broker.clone(),
    )
    .await;
    assert!(outcome.as_ref().map_or(true, |result| result.outcome
        != TeamProcessOutcome::Completed));
    assert!(broker.authorizations.load(Ordering::Acquire) >= 2);
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope())
            .unwrap()
            .last_message_seq,
        1
    );
}

#[tokio::test]
async fn provider_errors_and_oversized_output_never_become_shared_documents() {
    for reply in [Reply::Error, Reply::Text("x".repeat(65_537))] {
        let (_temp, root) = root();
        let server = Server::new(vec![reply]).await;
        let result = run(
            &root,
            request(
                &root,
                server.endpoint.clone(),
                uuid::Uuid::new_v4(),
                batch(0, 1, 0),
            ),
        )
        .await
        .unwrap();
        assert_ne!(result.outcome, TeamProcessOutcome::Completed);
        assert!(result.text.is_empty());
        assert!(result.document.is_none());
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("SYNTHETIC_PROVIDER_ERROR"));
        // The aborted/failed request's usage may be unrecorded, so such a task
        // can never claim complete accounting.
        assert_ne!(
            serde_json::to_value(&result.usage).unwrap()["status"],
            "complete"
        );
    }
}
#[tokio::test]
async fn accepted_provider_text_keeps_observed_task_usage_and_exact_terminal_receipt() {
    let (_temp, root) = root();
    let server = Server::new(vec![Reply::TextWithFields("Accepted result".into())]).await;
    let req = request(
        &root,
        server.endpoint.clone(),
        uuid::Uuid::new_v4(),
        batch(0, 1, 0),
    );
    let task = req.task_id.clone();
    let result = run(&root, req).await.unwrap();
    assert_eq!(result.text, "Accepted result");
    let usage = serde_json::to_value(&result.usage).unwrap();
    // MODEL is in the built-in price table and the task ended normally, so the
    // accounting is complete and cost is always present.
    assert_eq!(usage["status"], "complete");
    assert!(usage["inputTokens"].as_u64().unwrap() > 0);
    assert!(usage["outputTokens"].as_u64().unwrap() > 0);
    assert!(usage["costMicros"].as_u64().is_some());
    assert_eq!(usage["currency"], "USD");
    let recovered = harness_runtime::desktop::team_process::recover_result(&root, &scope(), &task)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(recovered).unwrap(),
        serde_json::to_value(result).unwrap()
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn preflight_compacts_only_consumed_history_and_counts_authorized_summary_dispatch() {
    let (_temp, root) = root();
    let server = Server::new(vec![
        Reply::Text("Completed original round".into()),
        Reply::Text("<summary>Retained earlier shared Chat facts.</summary>".into()),
        Reply::Text("Answer after compaction".into()),
        Reply::Text("Answer after compacted restart".into()),
    ])
    .await;
    run(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(0, 200, 0),
        ),
    )
    .await
    .unwrap();
    let mut denied_request = request(
        &root,
        server.endpoint.clone(),
        uuid::Uuid::new_v4(),
        batch(200, 201, 1),
    );
    denied_request.limits.context_window_tokens = std::num::NonZeroU32::new(32768).unwrap();
    let denied_broker = Arc::new(Broker::default());
    denied_broker.max_authorizations.store(1, Ordering::Release);
    assert!(run_broker(&root, denied_request, denied_broker.clone())
        .await
        .is_err());
    assert!(denied_broker.authorizations.load(Ordering::Acquire) >= 2);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(
        harness_runtime::desktop::team_process::inspect_scope(&root, &scope())
            .unwrap()
            .last_message_seq,
        200
    );
    let mut req = request(
        &root,
        server.endpoint.clone(),
        uuid::Uuid::new_v4(),
        batch(200, 201, 1),
    );
    req.limits.context_window_tokens = std::num::NonZeroU32::new(32768).unwrap();
    let broker = Arc::new(Broker::default());
    let attempt = run_broker(&root, req, broker.clone()).await;
    assert!(
        attempt.is_ok(),
        "preflight: {attempt:?}; requests: {:?}",
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| (
                &r.body["stream"],
                &r.body["model"],
                r.body["messages"].as_array().map(Vec::len)
            ))
            .collect::<Vec<_>>()
    );
    let result = attempt.unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.dispatched_requests, 2);
    // The preflight compaction summary is billed to the same session tracker.
    assert_eq!(
        serde_json::to_value(&result.usage).unwrap()["status"],
        "complete"
    );
    assert!(broker.authorizations.load(Ordering::Acquire) >= 3);
    assert_eq!(result.cursor.last_message_seq, 201);
    {
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 3);
        let final_request = records[2].body.to_string();
        assert!(final_request.contains("Retained earlier shared Chat facts."));
        assert_eq!(final_request.matches("SOURCE_0201").count(), 1);
    }
    let resumed = run(
        &root,
        request(
            &root,
            server.endpoint.clone(),
            uuid::Uuid::new_v4(),
            batch(201, 202, 2),
        ),
    )
    .await
    .unwrap();
    assert_eq!(resumed.outcome, TeamProcessOutcome::Completed);
    let records = server.requests.lock().unwrap();
    assert_eq!(records.len(), 4);
    let resumed_request = records[3].body.to_string();
    assert!(resumed_request.contains("Retained earlier shared Chat facts."));
    assert_eq!(resumed_request.matches("SOURCE_0201").count(), 1);
    assert_eq!(resumed_request.matches("SOURCE_0202").count(), 1);
    assert!(!resumed_request.contains("SOURCE_0001"));
}
