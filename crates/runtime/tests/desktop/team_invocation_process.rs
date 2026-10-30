#![cfg(target_os = "macos")]

//! Actual dedicated worker + actual SDK HTTP requests, synthetic loopback only.
//! Environment fixtures require --test-threads=1 and always restore old values.

use harness_runtime::desktop::team_invocation::TeamInvocationProfile;
use harness_runtime::desktop::team_invocation::{TeamInvocationIdentity, TeamInvocationLimits};
use harness_runtime::desktop::team_process::{
    TeamProcessDeadline, TeamProcessError, TeamProcessModel, TeamProcessOutcome,
    TeamProcessRequest, TeamProcessStart,
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
async fn run(
    root: &Path,
    request: TeamProcessRequest,
) -> Result<harness_runtime::desktop::team_process::TeamProcessResult, TeamProcessError> {
    tokio::time::timeout(
        Duration::from_secs(25),
        TeamProcessStart::prepare(root, request)
            .unwrap()
            .run(&worker(), CancellationToken::new()),
    )
    .await
    .expect("worker must be bounded")
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
    MalformedToolUse(String),
    Bash(String),
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
            for reply in replies {
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
                    Reply::MalformedToolUse(text) => ("200 OK", "text/event-stream", sse(json!({"type":"text","text":text}), "tool_use")),
                    Reply::Bash(command) => ("200 OK", "text/event-stream", sse(json!({"type":"tool_use","id":"tool_fixture_bash","name":"Bash","input":{"command":command}}), "tool_use")),
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

#[test]
fn model_and_backend_deadlines_fail_closed_without_exposing_credentials() {
    for endpoint in [
        "http://example.com",
        "https://user:secret@example.com",
        "https://example.com?token=x",
        "https://example.com/#token",
    ] {
        assert!(
            TeamProcessModel::anthropic_api_key(MODEL.into(), endpoint.into(), KEY.into()).is_err()
        );
    }
    assert!(TeamProcessModel::anthropic_api_key(
        "other/model".into(),
        "https://example.com".into(),
        KEY.into()
    )
    .is_err());
    assert!(TeamProcessDeadline::from_backend_unix_millis(now_ms() - 1).is_err());
    assert!(TeamProcessDeadline::from_backend_unix_millis(now_ms() + 600_000).is_err());
    let model =
        TeamProcessModel::anthropic_api_key(MODEL.into(), "https://example.com".into(), KEY.into())
            .unwrap();
    assert!(!format!("{model:?}").contains(KEY));
}

#[tokio::test]
async fn actual_worker_dispatch_uses_only_selected_credentials_and_private_prompt() {
    let (_dir, root) = root();
    let owner = root.join("owner");
    std::fs::create_dir_all(owner.join(".lingxi")).unwrap();
    std::fs::write(
        owner.join(".lingxi/CLAUDE.md"),
        "SYNTHETIC_AMBIENT_OWNER_INSTRUCTIONS",
    )
    .unwrap();
    std::fs::write(
        owner.join(".lingxi/settings.json"),
        r#"{"model":"owner/other","sandbox":{"enabled":false}}"#,
    )
    .unwrap();
    let server = Server::new(vec![Reply::Text("private answer".into())]).await;
    let _env = Environment::set(vec![
        ("HOME", owner.clone().into_os_string()),
        ("LINGXI_CONFIG_DIR", owner.join(".lingxi").into_os_string()),
        ("CFFIXED_USER_HOME", owner.clone().into_os_string()),
        (
            "__CF_USER_TEXT_ENCODING",
            "SYNTHETIC_AMBIENT_ENCODING".into(),
        ),
        ("ANTHROPIC_API_KEY", "SYNTHETIC_AMBIENT_KEY".into()),
        ("ANTHROPIC_AUTH_TOKEN", "SYNTHETIC_AMBIENT_TOKEN".into()),
        ("CLAUDE_CODE_OAUTH_TOKEN", "SYNTHETIC_AMBIENT_OAUTH".into()),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "x-owner-secret: SYNTHETIC_AMBIENT_HEADER".into(),
        ),
        (
            "CLAUDE_CODE_EXTRA_METADATA",
            r#"{"owner_secret":"SYNTHETIC_AMBIENT_METADATA"}"#.into(),
        ),
        (
            "CLAUDE_CODE_EXTRA_BODY",
            r#"{"owner_secret":"SYNTHETIC_AMBIENT_BODY"}"#.into(),
        ),
        (
            "ANTHROPIC_BASE_URL",
            "http://127.0.0.1:1/SYNTHETIC_AMBIENT_ENDPOINT".into(),
        ),
        ("HTTP_PROXY", "http://127.0.0.1:1".into()),
        ("HTTPS_PROXY", "http://127.0.0.1:1".into()),
        ("ALL_PROXY", "http://127.0.0.1:1".into()),
        ("CLAUDE_CODE_USE_BEDROCK", "true".into()),
        ("LINGXI_ENFORCE_PERMISSIONS", "false".into()),
    ]);
    let result = run(
        &root,
        request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    assert_eq!(result.text, "private answer");
    assert_eq!(result.dispatched_requests, 1);
    assert_eq!(result.denied_tools, 0);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let actual = &requests[0];
    assert!(actual
        .headers
        .to_ascii_lowercase()
        .contains(&format!("x-api-key: {KEY}")));
    assert!(!actual
        .headers
        .to_ascii_lowercase()
        .contains("authorization:"));
    assert_eq!(actual.body["model"], MODEL);
    assert!(actual.body["max_tokens"]
        .as_u64()
        .is_some_and(|n| n <= 4096));
    let text = actual.body.to_string();
    assert!(text.contains(PERSONA));
    assert!(text.contains(root.to_str().unwrap()));
    assert!(!text.contains(owner.to_str().unwrap()));
    assert!(!format!("{}{}", actual.headers, text).contains("SYNTHETIC_AMBIENT_"));
    assert!(!owner.join(".lingxi/.credentials.json").exists());
}

#[tokio::test]
async fn model_bash_request_is_denied_until_owner_broker_exists() {
    let (_dir, root) = root();
    let marker = root.join("must-not-execute");
    let server = Server::new(vec![
        Reply::Bash(format!("printf executed > '{}'", marker.display())),
        Reply::Text("Tool needs the owner.".into()),
    ])
    .await;
    let result = run(
        &root,
        request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    assert_eq!(result.denied_tools, 1);
    assert_eq!(result.dispatched_requests, 2);
    assert!(!marker.exists());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.to_string().contains("tool_result"));
}

#[tokio::test]
async fn worker_result_does_not_forward_provider_error_bodies_or_overflow_text() {
    for reply in [Reply::Error, Reply::Text("界".repeat(3000))] {
        let (_dir, root) = root();
        let overflow = matches!(reply, Reply::Text(_));
        let server = Server::new(vec![reply]).await;
        let result = run(
            &root,
            request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
        )
        .await
        .unwrap();
        assert_eq!(
            result.outcome,
            if overflow {
                TeamProcessOutcome::OutputLimit
            } else {
                TeamProcessOutcome::ModelFailed
            }
        );
        assert!(result.text.is_empty());
        assert_eq!(result.dispatched_requests, 1);
    }
}

#[tokio::test]
async fn deadline_kills_a_live_sdk_request_and_does_not_relaunch_the_invocation() {
    let (_dir, root) = root();
    let server = Server::new(vec![Reply::Hang]).await;
    let id = uuid::Uuid::new_v4();
    let start =
        TeamProcessStart::prepare(&root, request(server.endpoint.clone(), id, 10_000)).unwrap();
    let task = tokio::spawn(async move { start.run(&worker(), CancellationToken::new()).await });
    server.wait_requests(1).await;
    assert!(matches!(
        task.await.unwrap(),
        Err(TeamProcessError::Deadline)
    ));
    server.wait_disconnect().await;
    let replay = run(&root, request(server.endpoint.clone(), id, 20_000)).await;
    assert!(matches!(replay, Err(TeamProcessError::AlreadyAttempted)));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_and_future_drop_close_admitted_provider_connections() {
    for drop_future in [false, true] {
        let (_dir, root) = root();
        let server = Server::new(vec![Reply::Hang]).await;
        let start = TeamProcessStart::prepare(
            &root,
            request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let task = tokio::spawn(async move { start.run(&worker(), c).await });
        server.wait_requests(1).await;
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
        server.wait_disconnect().await;
    }
}

#[tokio::test]
async fn worker_closes_even_host_descriptors_without_cloexec() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    // First prove this synthetic descriptor really survives ordinary spawning.
    let (mut reader, inherited) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    nix::fcntl::fcntl(
        inherited.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .unwrap();
    let mut control = std::process::Command::new("/bin/sleep")
        .arg("20")
        .spawn()
        .unwrap();
    let control_fd = inherited.as_raw_fd();
    drop(inherited);
    reader
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    assert!(
        reader.read(&mut [0]).is_err(),
        "control child must retain the descriptor"
    );
    control.kill().unwrap();
    control.wait().unwrap();
    assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    drop(reader);

    let (_dir, root) = root();
    let server = Server::new(vec![Reply::Hang]).await;
    let (mut reader, inherited) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    nix::fcntl::fcntl(
        inherited.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .unwrap();
    let start = TeamProcessStart::prepare(
        &root,
        request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let task = tokio::spawn(async move { start.run(&worker(), c).await });
    server.wait_requests(1).await;
    drop(inherited);
    assert_eq!(
        reader.read(&mut [0]).unwrap(),
        0,
        "worker must close inherited host descriptor {control_fd}"
    );
    cancel.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(TeamProcessError::Cancelled)
    ));
    server.wait_disconnect().await;
}

#[tokio::test]
async fn invalid_executable_and_pre_cancel_do_not_consume_launch_but_spawn_failure_does() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, root) = root();
    let id = uuid::Uuid::new_v4();
    let prepare = || {
        TeamProcessStart::prepare(&root, request("http://127.0.0.1:1".into(), id, 20_000)).unwrap()
    };
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        prepare().run(&worker(), cancel).await,
        Err(TeamProcessError::Cancelled)
    ));
    let alias = root.join("worker-link");
    std::os::unix::fs::symlink(worker(), &alias).unwrap();
    assert!(matches!(
        prepare().run(&alias, CancellationToken::new()).await,
        Err(TeamProcessError::Executable)
    ));
    let broken = root.join("broken-executable");
    std::fs::write(&broken, "#!/missing/team/interpreter\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        prepare().run(&broken, CancellationToken::new()).await,
        Err(TeamProcessError::Io(_))
    ));
    assert!(matches!(
        prepare().run(&worker(), CancellationToken::new()).await,
        Err(TeamProcessError::AlreadyAttempted)
    ));
}

fn raw_bootstrap(root: &Path, id: uuid::Uuid, endpoint: &str) -> Value {
    json!({
        "version":1,"storage_root":root,"owner_uid":"fixture-owner","bot_key":"fixture-bot",
        "team_id":"fixture-team","conversation_id":"fixture-conversation","origin_human_uid":"fixture-human",
        "membership_generation":1,"invocation_id":id,"instructions":PERSONA,"input":"fixture human",
        "max_turns":12,"max_input_bytes":32768,"output_tokens_per_request":4096,"physical_dispatches":24,
        "deadline_unix_ms":now_ms()+20_000,"model_id":MODEL,"endpoint":endpoint,"api_key":KEY,
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

#[tokio::test]
async fn worker_parent_eof_or_unexpected_control_bytes_cancel_live_model_work() {
    for bad_byte in [false, true] {
        let (_dir, root) = root();
        let server = Server::new(vec![Reply::Hang]).await;
        let id = uuid::Uuid::new_v4();
        let mut child = raw_command(&root, id).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        write_bootstrap(&mut input, &raw_bootstrap(&root, id, &server.endpoint)).await;
        server.wait_requests(1).await;
        if bad_byte {
            // There is no permission-approval, question-answer or second-start
            // command on the parent lease. Any byte fails the invocation.
            input.write_all(b"allow").await.unwrap();
        } else {
            drop(input);
        }
        let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(75));
        server.wait_disconnect().await;
    }
}

#[tokio::test]
async fn private_bootstrap_rejects_dirty_environment_oversize_and_unknown_fields() {
    for case in [
        "environment",
        "oversize",
        "unknown",
        "version",
        "zero_limit",
    ] {
        let (_dir, root) = root();
        let id = uuid::Uuid::new_v4();
        let mut command = raw_command(&root, id);
        if case == "environment" {
            command.env("ANTHROPIC_API_KEY", "SYNTHETIC_AMBIENT_MUST_REJECT");
        }
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        if case == "oversize" {
            input
                .write_all(&(256u32 * 1024 + 1).to_be_bytes())
                .await
                .unwrap();
        } else if case != "environment" {
            let mut value = raw_bootstrap(&root, id, "http://127.0.0.1:1");
            match case {
                "unknown" => value["owner_approval"] = json!(true),
                "version" => value["version"] = json!(3),
                "zero_limit" => value["physical_dispatches"] = json!(0),
                _ => unreachable!(),
            }
            write_bootstrap(&mut input, &value).await;
        }
        drop(input);
        let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if case == "environment" { 78 } else { 65 }),
            "{case}"
        );
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{case}"
        );
    }
}

#[tokio::test]
async fn captured_deadline_cannot_be_extended_by_waiting_before_launch() {
    let (_dir, root) = root();
    let start = TeamProcessStart::prepare(
        &root,
        request("http://127.0.0.1:1".into(), uuid::Uuid::new_v4(), 100),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(
        start.run(&worker(), CancellationToken::new()).await,
        Err(TeamProcessError::Deadline)
    ));
}

#[tokio::test]
async fn concurrent_launches_cannot_reset_quota_or_start_a_second_worker() {
    let (_dir, root) = root();
    let server = Server::new(vec![Reply::Hang]).await;
    let id = uuid::Uuid::new_v4();
    let first =
        TeamProcessStart::prepare(&root, request(server.endpoint.clone(), id, 10_000)).unwrap();
    let second =
        TeamProcessStart::prepare(&root, request(server.endpoint.clone(), id, 10_000)).unwrap();
    let task = tokio::spawn(async move {
        let executable = worker();
        tokio::join!(
            first.run(&executable, CancellationToken::new()),
            second.run(&executable, CancellationToken::new())
        )
    });
    server.wait_requests(1).await;
    let (a, b) = task.await.unwrap();
    assert!(
        matches!(
            (&a, &b),
            (
                Err(TeamProcessError::Deadline),
                Err(TeamProcessError::AlreadyAttempted)
            ) | (
                Err(TeamProcessError::AlreadyAttempted),
                Err(TeamProcessError::Deadline)
            )
        ),
        "{a:?} {b:?}"
    );
    server.wait_disconnect().await;
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn physical_dispatch_budget_survives_the_process_boundary_and_tool_retry() {
    let (_dir, root) = root();
    let server = Server::new(vec![
        Reply::Bash("pwd".into()),
        Reply::Text("must not be requested".into()),
    ])
    .await;
    let mut request = request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000);
    request.limits.physical_dispatches = std::num::NonZeroU32::new(1).unwrap();
    let result = run(&root, request).await.unwrap();
    assert_eq!(
        result.outcome,
        TeamProcessOutcome::ModelFailed,
        "{result:?}"
    );
    assert_eq!(result.dispatched_requests, 1);
    assert_eq!(result.denied_tools, 1);
    assert!(result.text.is_empty());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejected_retry_text_is_absent_and_terminal_synthetic_errors_are_not_success() {
    for (first, second, expected) in [
        (
            Reply::MalformedToolUse("rejected attempt".into()),
            Reply::Text("accepted answer".into()),
            TeamProcessOutcome::Completed,
        ),
        (
            Reply::MalformedToolUse("rejected attempt".into()),
            Reply::MalformedToolUse("second bad attempt".into()),
            TeamProcessOutcome::ModelFailed,
        ),
        (
            Reply::Text(String::new()),
            Reply::Text(String::new()),
            TeamProcessOutcome::ModelFailed,
        ),
    ] {
        let (_dir, root) = root();
        let server = Server::new(vec![first, second]).await;
        let result = run(
            &root,
            request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, expected, "{result:?}");
        assert_eq!(result.dispatched_requests, 2);
        assert_eq!(
            result.text,
            if expected == TeamProcessOutcome::Completed {
                "accepted answer"
            } else {
                ""
            }
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn accepted_provider_text_with_preserved_fields_still_becomes_the_reply() {
    let (_dir, root) = root();
    let server = Server::new(vec![Reply::TextWithFields("保留合法回复".into())]).await;
    let result = run(
        &root,
        request(server.endpoint.clone(), uuid::Uuid::new_v4(), 20_000),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, TeamProcessOutcome::Completed, "{result:?}");
    assert_eq!(result.text, "保留合法回复");
}
