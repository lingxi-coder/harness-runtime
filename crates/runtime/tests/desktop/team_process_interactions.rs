#![cfg(target_os = "macos")]
//! Actual worker/SDK interactions against synthetic loopback only.
use harness_runtime::desktop::team_invocation::{
    TeamInvocationIdentity, TeamInvocationLimits, TeamInvocationProfile,
};
use harness_runtime::desktop::team_process::{
    TeamHumanQuestion, TeamModelRoute, TeamOwnerPermission, TeamProcessBroker, TeamProcessDeadline,
    TeamProcessError, TeamProcessModel, TeamProcessOutcome, TeamProcessRequest, TeamProcessStart,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
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
fn identity() -> TeamInvocationIdentity {
    TeamInvocationIdentity {
        owner_uid: "fixture-owner".into(),
        bot_key: "fixture-bot".into(),
        team_id: "fixture-team".into(),
        conversation_id: "fixture-conversation".into(),
        origin_human_uid: "fixture-human".into(),
        membership_generation: 1,
    }
}
fn root() -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, root)
}
fn request(endpoint: String, id: uuid::Uuid, duration_ms: u64) -> TeamProcessRequest {
    TeamProcessRequest {
        identity: identity(),
        invocation_id: id,
        instructions: PERSONA.into(),
        input: "Please reply to this isolated human request.".into(),
        limits: TeamInvocationLimits::default(),
        model: TeamProcessModel::anthropic_api_key(MODEL.into(), endpoint, KEY.into()).unwrap(),
        deadline: TeamProcessDeadline::from_backend_unix_millis(now_ms() + duration_ms).unwrap(),
    }
}
#[derive(Clone)]
struct Captured {
    headers: String,
    body: Value,
}
enum Reply {
    Text(String),
    Question,
    Bash(String),
    Responses(Value),
}
struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            for (index, reply) in replies.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http(&mut stream).await;
                captured.lock().unwrap().push(request);
                let (status, kind, body) = match reply {
                    Reply::Responses(output) => {
                        ("200 OK", "text/event-stream", responses_sse(output, index))
                    }
                    Reply::Text(text) => (
                        "200 OK",
                        "text/event-stream",
                        sse(json!({"type":"text","text":text}), "end_turn"),
                    ),
                    Reply::Question => (
                        "200 OK",
                        "text/event-stream",
                        sse(
                            json!({"type":"tool_use","id":format!("tool_fixture_{index}"),"name":"AskUserQuestion","input":{"questions":[{"question":"Which résumé should I use?","header":"Document","options":[{"label":"Current","description":"Use the current document"},{"label":"Archived","description":"Use the saved document"}],"multiSelect":false}]}}),
                            "tool_use",
                        ),
                    ),
                    Reply::Bash(command) => (
                        "200 OK",
                        "text/event-stream",
                        sse(
                            json!({"type":"tool_use","id":format!("tool_fixture_{index}"),"name":"Bash","input":{"command":command}}),
                            "tool_use",
                        ),
                    ),
                };
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
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
        json!({"type":"message_start","message":{"id":"msg_team_fixture","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
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

struct Broker {
    decisions: Mutex<VecDeque<bool>>,
    permissions: Mutex<Vec<TeamOwnerPermission>>,
    questions: Mutex<Vec<TeamHumanQuestion>>,
    wrong_answer_keys: bool,
}
impl Broker {
    fn new(decisions: Vec<bool>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions.into()),
            permissions: Mutex::new(Vec::new()),
            questions: Mutex::new(Vec::new()),
            wrong_answer_keys: false,
        })
    }
}
#[async_trait::async_trait]
impl TeamProcessBroker for Broker {
    async fn approve(
        &self,
        request: TeamOwnerPermission,
        cancel: CancellationToken,
    ) -> Result<bool, TeamProcessError> {
        assert!(!cancel.is_cancelled());
        assert_eq!(request.tool_name, "Bash");
        self.permissions.lock().unwrap().push(request);
        Ok(self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .expect("each tool needs explicit owner decision"))
    }
    async fn ask(
        &self,
        request: TeamHumanQuestion,
        cancel: CancellationToken,
    ) -> Result<Option<BTreeMap<String, String>>, TeamProcessError> {
        assert!(!cancel.is_cancelled());
        let answers = request
            .questions
            .iter()
            .map(|q| {
                (
                    if self.wrong_answer_keys {
                        "wrong key".into()
                    } else {
                        q.clone()
                    },
                    "Current — exact human response".into(),
                )
            })
            .collect();
        self.questions.lock().unwrap().push(request);
        Ok(Some(answers))
    }
}
async fn broker_run(
    root: &Path,
    request: TeamProcessRequest,
    broker: Arc<dyn TeamProcessBroker>,
) -> Result<harness_runtime::desktop::team_process::TeamProcessResult, TeamProcessError> {
    tokio::time::timeout(
        Duration::from_secs(25),
        TeamProcessStart::prepare(root, request)
            .unwrap()
            .run_with_broker(&worker(), CancellationToken::new(), broker),
    )
    .await
    .expect("worker interaction must remain bounded")
}
fn workspace(root: &Path, id: uuid::Uuid) -> PathBuf {
    TeamInvocationProfile::prepare(root, identity(), id, PERSONA.into(), Default::default())
        .unwrap()
        .scope_root()
        .join("workspace")
}

#[tokio::test]
async fn actual_worker_allows_only_one_explicit_owner_bash_call_and_denies_the_next() {
    let (_dir, root) = root();
    let id = uuid::Uuid::new_v4();
    let workspace = workspace(&root, id);
    let server = Server::new(vec![
        Reply::Bash("printf first > owner-once.txt".into()),
        Reply::Bash("printf second >> owner-once.txt".into()),
        Reply::Text("Settled answer".into()),
    ])
    .await;
    let broker = Broker::new(vec![true, false]);
    let result = broker_run(
        &root,
        request(server.endpoint.clone(), id, 20_000),
        broker.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    assert_eq!(result.text, "Settled answer");
    assert_eq!(result.denied_tools, 1);
    assert_eq!(
        std::fs::read_to_string(workspace.join("owner-once.txt")).unwrap(),
        "first"
    );
    let permissions = broker.permissions.lock().unwrap();
    assert_eq!(permissions.len(), 2);
    assert_ne!(permissions[0].id, permissions[1].id);
    assert_eq!(
        permissions[0].input["command"],
        "printf first > owner-once.txt"
    );
    assert!(broker.questions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn actual_worker_owner_denial_never_executes_shell() {
    let (_dir, root) = root();
    let id = uuid::Uuid::new_v4();
    let workspace = workspace(&root, id);
    let server = Server::new(vec![
        Reply::Bash("printf forbidden > denied.txt".into()),
        Reply::Text("Denied safely".into()),
    ])
    .await;
    let result = broker_run(
        &root,
        request(server.endpoint.clone(), id, 20_000),
        Broker::new(vec![false]),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed);
    assert_eq!(result.denied_tools, 1);
    assert!(!workspace.join("denied.txt").exists());
}

#[tokio::test]
async fn actual_worker_question_uses_original_text_keys_and_dedicated_human_answer() {
    let (_dir, root) = root();
    let server = Server::new(vec![Reply::Question, Reply::Text("Human answered".into())]).await;
    let broker = Broker::new(vec![]);
    let result = broker_run(
        &root,
        request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
        broker.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    let questions = broker.questions.lock().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].questions, vec!["Which résumé should I use?"]);
    assert!(questions[0].text.contains("Document"));
    assert!(questions[0]
        .text
        .contains("Current: Use the current document"));
    assert!(broker.permissions.lock().unwrap().is_empty());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let followup = requests[1].body.to_string();
    assert!(followup.contains("Which résumé should I use?"));
    assert!(followup.contains("Current — exact human response"));
}

#[tokio::test]
async fn broker_answer_with_changed_question_keys_fails_closed_before_reply() {
    let (_dir, root) = root();
    let server = Server::new(vec![Reply::Question]).await;
    let broker = Arc::new(Broker {
        decisions: Mutex::new(VecDeque::new()),
        permissions: Mutex::new(Vec::new()),
        questions: Mutex::new(Vec::new()),
        wrong_answer_keys: true,
    });
    let result = broker_run(
        &root,
        request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
        broker,
    )
    .await;
    assert!(
        matches!(result, Err(TeamProcessError::Protocol)),
        "{result:?}"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

struct WaitingBroker {
    entered: tokio::sync::Notify,
    token: Mutex<Option<CancellationToken>>,
    dropped: Arc<AtomicBool>,
}
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl WaitingBroker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            token: Mutex::new(None),
            dropped: Arc::new(AtomicBool::new(false)),
        })
    }
    async fn wait(&self, token: CancellationToken) -> Result<bool, TeamProcessError> {
        let _guard = Dropped(self.dropped.clone());
        *self.token.lock().unwrap() = Some(token);
        self.entered.notify_one();
        std::future::pending().await
    }
    fn assert_cancelled(&self) {
        assert!(
            self.dropped.load(Ordering::Acquire),
            "callback future must be dropped"
        );
        assert!(
            self.token.lock().unwrap().as_ref().unwrap().is_cancelled(),
            "callback lease must be revoked"
        );
    }
}
#[async_trait::async_trait]
impl TeamProcessBroker for WaitingBroker {
    async fn approve(
        &self,
        _: TeamOwnerPermission,
        cancel: CancellationToken,
    ) -> Result<bool, TeamProcessError> {
        self.wait(cancel).await
    }
    async fn ask(
        &self,
        _: TeamHumanQuestion,
        cancel: CancellationToken,
    ) -> Result<Option<BTreeMap<String, String>>, TeamProcessError> {
        self.wait(cancel).await.map(|_| None)
    }
}

#[tokio::test]
async fn owner_and_human_waits_share_deadline_and_do_not_detach_callbacks() {
    for question in [false, true] {
        let (_dir, root) = root();
        let server = Server::new(vec![if question {
            Reply::Question
        } else {
            Reply::Bash("printf forbidden > deadline.txt".into())
        }])
        .await;
        let broker = WaitingBroker::new();
        let result = broker_run(
            &root,
            request(server.endpoint.clone(), uuid::Uuid::new_v4(), 5000),
            broker.clone(),
        )
        .await;
        assert!(
            matches!(result, Err(TeamProcessError::Deadline)),
            "{result:?}"
        );
        broker.assert_cancelled();
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn cancellation_and_future_drop_cancel_the_inline_broker() {
    for drop_future in [false, true] {
        let (_dir, root) = root();
        let server = Server::new(vec![Reply::Question]).await;
        let broker = WaitingBroker::new();
        let start = TeamProcessStart::prepare(
            &root,
            request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_broker = broker.clone();
        let task = tokio::spawn(async move {
            start
                .run_with_broker(&worker(), task_cancel, task_broker)
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), broker.entered.notified())
            .await
            .unwrap();
        if drop_future {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            cancel.cancel();
            assert!(matches!(
                task.await.unwrap(),
                Err(TeamProcessError::Cancelled)
            ));
        }
        broker.assert_cancelled();
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

fn raw_bootstrap(root: &Path, id: uuid::Uuid, endpoint: &str) -> Value {
    let model =
        TeamProcessModel::anthropic_api_key(MODEL.into(), endpoint.into(), KEY.into()).unwrap();
    let route = model.route();
    json!({
        "version":2,"storage_root":root,"owner_uid":"fixture-owner","bot_key":"fixture-bot",
        "team_id":"fixture-team","conversation_id":"fixture-conversation","origin_human_uid":"fixture-human",
        "membership_generation":1,"invocation_id":id,"instructions":PERSONA,"input":"fixture human",
        "max_turns":12,"max_input_bytes":32768,"output_tokens_per_request":4096,"physical_dispatches":24,
        "deadline_unix_ms":now_ms()+20_000,"model_id":MODEL,"endpoint":endpoint,"api_key":KEY,
        "model_provider":route.model_provider,"model_protocol":route.model_protocol,"model_profile":route.model_profile,
    })
}
fn raw_command(root: &Path, id: uuid::Uuid) -> tokio::process::Command {
    use std::process::Stdio;
    let p =
        TeamInvocationProfile::prepare(root, identity(), id, PERSONA.into(), Default::default())
            .unwrap();
    let mut c = tokio::process::Command::new(worker());
    c.env_clear()
        .env("HOME", p.scope_root().join("home"))
        .env("LINGXI_CONFIG_DIR", p.scope_root().join("home"))
        .env("TMPDIR", p.scope_root().join("tmp"))
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("CFFIXED_USER_HOME", p.scope_root().join("home"))
        .env(
            "__CF_USER_TEXT_ENCODING",
            format!("0x{:X}:0x8000100:0x0", nix::unistd::geteuid().as_raw()),
        )
        .current_dir(p.scope_root().join("workspace"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    c
}
async fn write_bootstrap(input: &mut tokio::process::ChildStdin, value: &Value) {
    let body = serde_json::to_vec(value).unwrap();
    input
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    input.write_all(&body).await.unwrap();
    input.flush().await.unwrap();
}

async fn read_worker_frame(output: &mut tokio::process::ChildStdout) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        let length = output.read_u32().await.unwrap() as usize;
        assert!((1..=256 * 1024).contains(&length));
        let mut bytes = vec![0; length];
        output.read_exact(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    })
    .await
    .expect("actual worker interaction must arrive")
}
async fn failed_worker(
    child: &mut tokio::process::Child,
    output: &mut tokio::process::ChildStdout,
) {
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("invalid replies/EOF must cancel waiting worker")
        .unwrap();
    assert_eq!(status.code(), Some(75));
    let mut rest = Vec::new();
    output.read_to_end(&mut rest).await.unwrap();
    assert!(
        rest.is_empty(),
        "cancelled interaction cannot publish terminal result"
    );
}

#[tokio::test]
async fn actual_worker_owner_and_human_waits_cancel_on_parent_eof() {
    for question in [false, true] {
        let (_dir, root) = root();
        let server = Server::new(vec![if question {
            Reply::Question
        } else {
            Reply::Bash("printf forbidden > eof.txt".into())
        }])
        .await;
        let id = uuid::Uuid::new_v4();
        let mut child = raw_command(&root, id).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        write_bootstrap(&mut input, &raw_bootstrap(&root, id, &server.endpoint)).await;
        let frame = read_worker_frame(&mut output).await;
        assert_eq!(
            frame["type"],
            if question {
                "human_question"
            } else {
                "owner_permission"
            }
        );
        drop(input);
        failed_worker(&mut child, &mut output).await;
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn actual_worker_rejects_wrong_unknown_oversized_and_out_of_order_replies() {
    for case in [
        "wrong_id",
        "wrong_channel",
        "unknown_field",
        "oversized",
        "second_start",
        "wrong_answer_keys",
        "empty_answer",
        "large_answer",
    ] {
        let (_dir, root) = root();
        let question = matches!(case, "wrong_answer_keys" | "empty_answer" | "large_answer");
        let server = Server::new(vec![if question {
            Reply::Question
        } else {
            Reply::Bash("printf forbidden > malformed.txt".into())
        }])
        .await;
        let id = uuid::Uuid::new_v4();
        let mut child = raw_command(&root, id).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        write_bootstrap(&mut input, &raw_bootstrap(&root, id, &server.endpoint)).await;
        let frame = read_worker_frame(&mut output).await;
        let request_id = frame["request"]["id"].as_str().unwrap();
        if case == "oversized" {
            input
                .write_all(&(256u32 * 1024 + 1).to_be_bytes())
                .await
                .unwrap();
        } else {
            let response = match case {
                "wrong_id" => {
                    json!({"type":"owner_permission","id":"not-pending","allow_once":true})
                }
                "wrong_channel" => json!({"type":"human_question","id":request_id,"answers":null}),
                "unknown_field" => {
                    json!({"type":"owner_permission","id":request_id,"allow_once":true,"input":{"command":"edited"}})
                }
                "second_start" => raw_bootstrap(&root, id, &server.endpoint),
                "wrong_answer_keys" => {
                    json!({"type":"human_question","id":request_id,"answers":{"altered":"Current"}})
                }
                "empty_answer" => {
                    json!({"type":"human_question","id":request_id,"answers":{"Which résumé should I use?":""}})
                }
                "large_answer" => {
                    json!({"type":"human_question","id":request_id,"answers":{"Which résumé should I use?":"a".repeat(2001)}})
                }
                _ => unreachable!(),
            };
            write_bootstrap(&mut input, &response).await;
        }
        failed_worker(&mut child, &mut output).await;
        assert_eq!(server.requests.lock().unwrap().len(), 1, "{case}");
    }
}

#[tokio::test]
async fn actual_worker_rejects_duplicate_owner_reply_while_human_question_is_pending() {
    let (_dir, root) = root();
    let server = Server::new(vec![
        Reply::Bash("printf forbidden > duplicate.txt".into()),
        Reply::Question,
    ])
    .await;
    let id = uuid::Uuid::new_v4();
    let mut child = raw_command(&root, id).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    write_bootstrap(&mut input, &raw_bootstrap(&root, id, &server.endpoint)).await;
    let first = read_worker_frame(&mut output).await;
    let denial = json!({"type":"owner_permission","id":first["request"]["id"],"allow_once":false});
    write_bootstrap(&mut input, &denial).await;
    let second = read_worker_frame(&mut output).await;
    assert_eq!(second["type"], "human_question");
    assert_ne!(first["request"]["id"], second["request"]["id"]);
    write_bootstrap(&mut input, &denial).await;
    failed_worker(&mut child, &mut output).await;
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

/// Synthetic fixtures are finite and own only the PIDs recorded in their
/// private workspace. Keep a fallback kill for a failed assertion as well.
struct AdmittedProcesses(Vec<i32>);
impl Drop for AdmittedProcesses {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
fn process_alive(pid: i32) -> bool {
    // A dead orphan may briefly be a zombie awaiting launchd reaping. A zombie
    // is terminated and cannot run more model-selected work.
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    output.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z')
}
async fn admitted_processes(workspace: &Path) -> AdmittedProcesses {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let shell = std::fs::read_to_string(workspace.join("admitted-shell.pid"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            let descendant = std::fs::read_to_string(workspace.join("admitted-child.pid"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            if let (Some(shell), Some(descendant)) = (shell, descendant) {
                assert!(shell > 1 && descendant > 1 && shell != descendant);
                let owned = AdmittedProcesses(vec![shell, descendant]);
                assert!(
                    owned.0.iter().all(|pid| nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(*pid),
                        None
                    )
                    .is_ok()),
                    "positive PID liveness control"
                );
                assert!(
                    owned.0.iter().all(|pid| process_alive(*pid)),
                    "approved Bash and its descendant must actually be running before termination"
                );
                return owned;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approved private command must create running Bash and descendant")
}
async fn assert_admitted_processes_terminated(mut processes: AdmittedProcesses) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while processes.0.iter().any(|pid| process_alive(*pid)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("worker loss must terminate admitted Bash and descendant without waiting for their natural exit");
    // They have terminated; never signal an unrelated future reuse of a PID.
    processes.0.clear();
}
const LONG_PRIVATE_BASH: &str = "printf '%s\\n' \"$$\" > admitted-shell.pid; /bin/sleep 60 & child=$!; printf '%s\\n' \"$child\" > admitted-child.pid; wait \"$child\"";

#[tokio::test]
async fn admitted_bash_and_descendant_die_after_worker_sigkill_parent_eof_and_deadline() {
    for loss in ["worker_sigkill", "parent_eof", "deadline"] {
        let (_dir, root) = root();
        let id = uuid::Uuid::new_v4();
        let workspace = workspace(&root, id);
        let server = Server::new(vec![Reply::Bash(LONG_PRIVATE_BASH.into())]).await;
        let mut child = raw_command(&root, id).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let mut bootstrap = raw_bootstrap(&root, id, &server.endpoint);
        if loss == "deadline" {
            bootstrap["deadline_unix_ms"] = json!(now_ms() + 5000);
        }
        write_bootstrap(&mut input, &bootstrap).await;
        let permission = read_worker_frame(&mut output).await;
        assert_eq!(permission["type"], "owner_permission");
        write_bootstrap(
            &mut input,
            &json!({"type":"owner_permission","id":permission["request"]["id"],"allow_once":true}),
        )
        .await;
        let processes = admitted_processes(&workspace).await;
        match loss {
            "worker_sigkill" => child.start_kill().unwrap(),
            "parent_eof" => drop(input),
            "deadline" => (),
            _ => unreachable!(),
        }
        let status = tokio::time::timeout(Duration::from_secs(8), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success(), "{loss}");
        if loss != "worker_sigkill" {
            assert_eq!(status.code(), Some(75), "{loss}");
        }
        assert_admitted_processes_terminated(processes).await;
        assert_eq!(server.requests.lock().unwrap().len(), 1, "{loss}");
    }
}

#[tokio::test]
async fn admitted_bash_and_descendant_die_when_host_run_future_is_dropped() {
    let (_dir, root) = root();
    let id = uuid::Uuid::new_v4();
    let workspace = workspace(&root, id);
    let server = Server::new(vec![Reply::Bash(LONG_PRIVATE_BASH.into())]).await;
    let start =
        TeamProcessStart::prepare(&root, request(server.endpoint.clone(), id, 20_000)).unwrap();
    let task = tokio::spawn(async move {
        start
            .run_with_broker(&worker(), CancellationToken::new(), Broker::new(vec![true]))
            .await
    });
    let processes = admitted_processes(&workspace).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_admitted_processes_terminated(processes).await;
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn normal_bash_return_removes_residual_child_before_model_receives_tool_result() {
    let (_dir, root) = root();
    let id = uuid::Uuid::new_v4();
    let workspace = workspace(&root, id);
    // The background child's pipes are redirected: pipe EOF cannot accidentally
    // supply the lifetime guarantee this test expects from the guardian. Hold
    // the shell briefly until the test has positively observed both live PIDs.
    let command = "printf '%s\\n' \"$$\" > admitted-shell.pid; /bin/sleep 60 >/dev/null 2>&1 & child=$!; printf '%s\\n' \"$child\" > admitted-child.pid; for ((i=0;i<100;i++)); do [ -e release-shell ] && break; /bin/sleep 0.05; done; exit 0";
    let server = Server::new(vec![
        Reply::Bash(command.into()),
        Reply::Text("Finished after cleanup".into()),
    ])
    .await;
    let start =
        TeamProcessStart::prepare(&root, request(server.endpoint.clone(), id, 20_000)).unwrap();
    let task = tokio::spawn(async move {
        start
            .run_with_broker(&worker(), CancellationToken::new(), Broker::new(vec![true]))
            .await
    });
    let processes = admitted_processes(&workspace).await;
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    std::fs::write(workspace.join("release-shell"), b"release").unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        while server.requests.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("model must receive the completed tool result");
    assert!(
        processes.0.iter().all(|pid| !process_alive(*pid)),
        "completed tool result must follow verified removal of shell and residual child"
    );
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    assert_eq!(result.text, "Finished after cleanup");
    assert_eq!(result.denied_tools, 0);
    {
        let requests = server.requests.lock().unwrap();
        let results = requests[1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|content| content["type"] == "tool_result")
            .collect::<Vec<_>>();
        assert!(!results.is_empty());
        assert!(results.iter().all(|result| result["is_error"] != true));
    }
    assert_admitted_processes_terminated(processes).await;
}

fn responses_sse(item: Value, index: usize) -> String {
    let mut start = item.clone();
    if item["type"] == "function_call" {
        start["arguments"] = json!("");
    } else {
        start["content"] = json!([]);
    }
    let mut frames = vec![
        json!({"type":"response.created","response":{"id":format!("resp_{index}"),"model":"gpt-6-luna","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":start}),
    ];
    if item["type"] == "message" {
        frames.push(json!({"type":"response.output_text.delta","item_id":item["id"],"output_index":0,"content_index":0,"delta":item["content"][0]["text"]}));
    }
    frames.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
    frames.push(json!({"type":"response.completed","response":{"id":format!("resp_{index}"),"model":"gpt-6-luna","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":3,"total_tokens":4}}}));
    frames
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn selected_openai_responses_uses_existing_codec_for_owner_bash_and_human_question() {
    let (_dir, root) = root();
    let id = uuid::Uuid::new_v4();
    let workspace = workspace(&root, id);
    let server = Server::new(vec![
        Reply::Responses(json!({"type":"function_call","id":"fc_bash","call_id":"call_bash","name":"Bash","arguments":json!({"command":"printf responses-ok > responses.txt"}).to_string()})),
        Reply::Responses(json!({"type":"function_call","id":"fc_human","call_id":"call_human","name":"AskUserQuestion","arguments":json!({"questions":[{"question":"Which résumé should I use?","header":"Document","options":[{"label":"Current","description":"Current document"},{"label":"Archived","description":"Saved document"}],"multiSelect":false}]}).to_string()})),
        Reply::Responses(json!({"type":"message","id":"msg_final","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Responses result","annotations":[]}]})),
    ]).await;
    let endpoint = format!("{}/v1", server.endpoint);
    let route = TeamModelRoute {
        model_provider:"openai".into(), model_protocol:serde_json::from_value(json!("open_ai_responses")).unwrap(),
        model_id:"gpt-6-luna".into(), model_endpoint:endpoint.clone(), credential_id:"openai".into(),
        model_profile:serde_json::from_value(json!({"provider_id":"openai","profile_name":"openai","protocol":"open_ai_responses","base_url":endpoint,"auth":"api_key","credential":{"source":"host_managed","key":"team-selected-model"},"models":[{"display_model":"gpt-6-luna","request_model":"gpt-6-luna","billing_model":"gpt-6-luna","capability_support":{"streaming":"supported","tools":"supported"}}],"extra":{}})).unwrap(),
    };
    let mut request = request(server.endpoint.clone(), id, 20_000);
    request.model = TeamProcessModel::selected_api_key(route, KEY.into()).unwrap();
    let broker = Broker::new(vec![true]);
    let result = broker_run(&root, request, broker.clone()).await.unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    assert_eq!(result.text, "Responses result");
    assert_eq!(
        std::fs::read_to_string(workspace.join("responses.txt")).unwrap(),
        "responses-ok"
    );
    assert_eq!(broker.permissions.lock().unwrap().len(), 1);
    assert_eq!(broker.questions.lock().unwrap().len(), 1);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert!(request.headers.starts_with("POST /v1/responses "));
        assert!(request
            .headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("authorization: Bearer {KEY}"))));
        assert!(!request.headers.to_ascii_lowercase().contains("x-api-key:"));
        assert_eq!(request.body["model"], "gpt-6-luna");
        assert!(request.body["max_output_tokens"]
            .as_u64()
            .is_some_and(|n| n > 0 && n <= 4096));
        assert_eq!(request.body["store"], false);
        assert!(request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["type"] == "function" && tool.get("function").is_none()));
    }
    assert!(requests[1].body.to_string().contains("call_bash"));
    assert!(requests[2].body.to_string().contains("call_human"));
    assert!(requests[2]
        .body
        .to_string()
        .contains("Current — exact human response"));
}
