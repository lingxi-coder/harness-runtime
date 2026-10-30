#![cfg(target_os = "macos")]

//! Real connector HTTP and real all-tools-denied worker, synthetic loopback.
use async_trait::async_trait;
use harness_runtime::desktop::team_connector::{
    Error, TeamBotSnapshot, TeamBotState, TeamBotTarget, TeamConnector, TeamConnectorAccount,
    TeamConnectorCredential, TeamConnectorStatus,
};
use harness_runtime::desktop::team_invocation::TeamInvocationLimits;
use harness_runtime::desktop::team_process::TeamProcessModel;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-owner-firebase-token";
const KEY: &str = "synthetic-selected-provider-key";
const MODEL: &str = "claude-sonnet-4-20250514";
const INPUT: &str = "Only the authenticated original human input.";
const OUTPUT: &str = "Isolated worker reply.";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn worker() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_harness-team-worker"))
        .canonicalize()
        .unwrap()
}
fn target() -> TeamBotTarget {
    let bot_key = format!("bot:fixture-owner:{}:{}", "h".repeat(22), "b".repeat(22));
    let trigger_id = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!(["human-message01", bot_key])).unwrap())
    );
    TeamBotTarget {
        owner_uid: "fixture-owner".into(),
        owner_generation: 1,
        host_id: "h".repeat(22),
        bot_id: "b".repeat(22),
        bot_revision: 1,
        team_id: "fixture-team".into(),
        conversation_id: "fixture-conversation".into(),
        bot_key,
        trigger_id,
    }
}
fn execution(t: &TeamBotTarget, attempt: &Value) -> Value {
    json!({"version":1,"teamId":t.team_id,"conversationId":t.conversation_id,"botKey":t.bot_key,
        "triggerId":t.trigger_id,"attemptId":attempt,"ownerUid":t.owner_uid,"ownerGeneration":t.owner_generation,
        "hostId":t.host_id,"botId":t.bot_id,"botRevision":t.bot_revision,
        "origin":{"uid":"fixture-human","generation":7},"messageId":"human-message01","seq":1,"text":INPUT})
}
fn reply_id(t: &TeamBotTarget) -> String {
    format!(
        "bot_{}",
        &format!("{:x}", Sha256::digest(format!("{}:reply", t.trigger_id)))[..60]
    )
}
fn snapshot(model: &str) -> TeamBotSnapshot {
    TeamBotSnapshot {
        instructions: "Frozen connector test persona.".into(),
        limits: TeamInvocationLimits::default(),
        model: TeamProcessModel::anthropic_api_key(MODEL.into(), model.into(), KEY.into()).unwrap(),
    }
}

struct Account {
    active: AtomicBool,
    credential_uid: String,
    credential_epoch: u64,
    token: String,
}
impl Account {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
            credential_uid: "fixture-owner".into(),
            credential_epoch: 3,
            token: TOKEN.into(),
        })
    }
}
#[async_trait]
impl TeamConnectorAccount for Account {
    fn is_current(&self, uid: &str, epoch: u64) -> bool {
        self.active.load(Ordering::Acquire) && uid == "fixture-owner" && epoch == 3
    }
    async fn credential(&self) -> Result<TeamConnectorCredential, Error> {
        Ok(TeamConnectorCredential {
            owner_uid: self.credential_uid.clone(),
            epoch: self.credential_epoch,
            id_token: self.token.clone(),
        })
    }
}
struct Roots {
    _temp: tempfile::TempDir,
    journal: PathBuf,
    runtime: PathBuf,
}
impl Roots {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let journal = root.join("journal");
        let runtime = root.join("runtime");
        for path in [&journal, &runtime] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            _temp: temp,
            journal,
            runtime,
        }
    }
    fn connector(
        &self,
        endpoint: &str,
        account: Arc<dyn TeamConnectorAccount>,
        session: CancellationToken,
    ) -> TeamConnector {
        TeamConnector::new(
            endpoint,
            account,
            "fixture-owner".into(),
            3,
            session,
            self.journal.clone(),
            self.runtime.clone(),
            worker(),
        )
        .unwrap()
    }
    fn entry(&self) -> PathBuf {
        std::fs::read_dir(&self.journal)
            .unwrap()
            .map(|x| x.unwrap().path())
            .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("run-"))
            .unwrap()
    }
}

#[derive(Clone)]
enum Mode {
    Good,
    LostStart,
    FalseStart,
    BadPermit(&'static str, Value),
    LostFinish,
    PollRevoked(Arc<AtomicBool>),
    PollUnauthorized(Arc<AtomicBool>),
    PollBadLease(Arc<AtomicBool>),
    StartOversized,
    StartRedirect,
    ExpiredClaim,
    StartLease(u64),
    StartWithoutSnapshot,
    StartOversizedChunked,
    PollHang(Arc<AtomicBool>),
}
struct Backend {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Backend {
    async fn new(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/teams/messages",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            let lease = now() + 300000;
            let mut finished = false;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                assert!(headers
                    .lines()
                    .any(|s| s.eq_ignore_ascii_case(&format!("authorization: Bearer {TOKEN}"))));
                assert!(!headers.contains(KEY));
                captured.lock().unwrap().push(body.clone());
                let t = target();
                let mut status = "200 OK";
                let mut extra = "";
                let reply = match body["action"].as_str().unwrap() {
                    "claimBot" => {
                        json!({"state":"claimed","origin":{"uid":"WRONG_CLAIM_BODY","generation":999},"text":"IGNORE_CLAIM_TEXT","messageId":"claim-msg01","seq":9})
                    }
                    "startBot" => {
                        if matches!(mode, Mode::LostStart) {
                            continue;
                        }
                        if matches!(mode, Mode::StartOversized) {
                            write_http(
                                &mut stream,
                                status,
                                "",
                                &"x".repeat(65537),
                                "application/json",
                            )
                            .await;
                            continue;
                        }
                        if matches!(mode, Mode::StartOversizedChunked) {
                            let body = "x".repeat(65537);
                            let response=format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",body.len());
                            let _ = stream.write_all(response.as_bytes()).await;
                            continue;
                        }
                        if matches!(mode, Mode::StartRedirect) {
                            status = "307 Temporary Redirect";
                            extra = "Location: http://127.0.0.1:1/credential-leak\r\n";
                        }
                        let mut e = execution(&t, &body["attemptId"]);
                        if let Mode::BadPermit(field, value) = &mode {
                            e[*field] = value.clone();
                        }
                        if matches!(mode, Mode::StartWithoutSnapshot) {
                            json!({"state":"running","execute":true,"leaseUntil":lease})
                        } else if let Mode::StartLease(value) = mode {
                            json!({"state":"running","execute":true,"leaseUntil":value,"execution":e})
                        } else if matches!(mode, Mode::FalseStart) {
                            json!({"state":"running","execute":false,"leaseUntil":lease})
                        } else {
                            json!({"state":"running","execute":true,"leaseUntil":lease,"execution":e})
                        }
                    }
                    "botPoll" => {
                        if matches!(mode, Mode::ExpiredClaim) {
                            status = "409 Conflict";
                        }
                        if matches!(&mode, Mode::PollHang(active) if active.load(Ordering::Acquire))
                        {
                            let mut byte = [0; 1];
                            let _ = stream.read(&mut byte).await;
                            continue;
                        }
                        if let Mode::PollUnauthorized(active) = &mode {
                            if active.load(Ordering::Acquire) {
                                status = "401 Unauthorized";
                            }
                        }
                        if matches!(&mode, Mode::PollRevoked(active) if active.load(Ordering::Acquire))
                        {
                            json!({"state":"revoked"})
                        } else {
                            json!({"state":"running","leaseUntil":lease + if matches!(&mode, Mode::PollBadLease(active) if active.load(Ordering::Acquire)) { 1 } else { 0 },"questionId":null,"answers":null})
                        }
                    }
                    "finishBot" => {
                        assert_eq!(body["text"], OUTPUT);
                        assert_eq!(body["outcome"], "completed");
                        if matches!(mode, Mode::LostFinish) && !finished {
                            finished = true;
                            continue;
                        }
                        json!({"state":"completed","replyId":reply_id(&t)})
                    }
                    _ => panic!("unexpected action"),
                };
                write_http(
                    &mut stream,
                    status,
                    extra,
                    &reply.to_string(),
                    "application/json",
                )
                .await;
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }
    fn count(&self, action: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["action"] == action)
            .count()
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Model {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    active: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Model {
    async fn new(hang: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let active = Arc::new(AtomicBool::new(false));
        let live = active.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let disconnected = closed.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                assert!(headers
                    .lines()
                    .any(|s| s.eq_ignore_ascii_case(&format!("x-api-key: {KEY}"))));
                assert!(!headers.contains(TOKEN));
                assert!(!headers.to_ascii_lowercase().contains("authorization:"));
                assert!(!body.to_string().contains(TOKEN));
                assert!(!body.to_string().contains("IGNORE_CLAIM_TEXT"));
                assert!(!body.to_string().contains("WRONG_CLAIM_BODY"));
                assert!(body.to_string().contains(INPUT));
                captured.lock().unwrap().push(body);
                live.store(true, Ordering::Release);
                if hang {
                    let mut byte = [0; 1];
                    let result = stream.read(&mut byte).await;
                    disconnected.store(matches!(result, Ok(0) | Err(_)), Ordering::Release);
                    return;
                }
                let frames = [
                    json!({"type":"message_start","message":{"id":"msg_connector","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":OUTPUT}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
                    json!({"type":"message_stop"}),
                ];
                let body: String = frames
                    .iter()
                    .map(|f| format!("event: {}\ndata: {f}\n\n", f["type"].as_str().unwrap()))
                    .collect();
                write_http(&mut stream, "200 OK", "", &body, "text/event-stream").await;
            }
        });
        Self {
            endpoint,
            requests,
            active,
            closed,
            task,
        }
    }
    async fn wait(&self, flag: &AtomicBool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !flag.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_http(stream: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buf = [0; 4096];
        let size = stream.read(&mut buf).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buf[..size]);
        assert!(bytes.len() < 1048576);
        if let Some(i) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let len: usize = headers
        .lines()
        .find_map(|s| {
            let (k, v) = s.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() - end < len {
        let mut buf = [0; 4096];
        let size = stream.read(&mut buf).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&buf[..size]);
    }
    (
        headers,
        serde_json::from_slice(&bytes[end..end + len]).unwrap(),
    )
}
async fn write_http(stream: &mut TcpStream, status: &str, extra: &str, body: &str, kind: &str) {
    let response = format!("HTTP/1.1 {status}\r\n{extra}Content-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    let _ = stream.write_all(response.as_bytes()).await;
}

#[tokio::test]
async fn authenticated_snapshot_runs_real_worker_and_exact_result_retry_never_reexecutes() {
    let roots = Roots::new();
    let account = Account::new();
    let backend = Backend::new(Mode::Good).await;
    let model = Model::new(false).await;
    let connector = roots.connector(&backend.endpoint, account.clone(), CancellationToken::new());
    let expected = TeamConnectorStatus::Settled {
        state: TeamBotState::Completed,
        reply_id: Some(reply_id(&target())),
    };
    assert_eq!(
        connector
            .run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        expected
    );
    drop(connector);
    let restarted = roots.connector(&backend.endpoint, account, CancellationToken::new());
    assert_eq!(
        restarted
            .retry_result(target(), CancellationToken::new())
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        restarted
            .run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        expected
    );
    assert_eq!(backend.count("claimBot"), 1);
    assert_eq!(backend.count("startBot"), 1);
    assert_eq!(backend.count("finishBot"), 3);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let finishes: Vec<_> = backend
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "finishBot")
        .cloned()
        .collect();
    assert!(finishes.iter().all(|v| v == &finishes[0]));
    for file in std::fs::read_dir(roots.entry()).unwrap() {
        let path = file.unwrap().path();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(!data.contains(KEY));
        assert!(!data.contains(TOKEN));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }
}

#[tokio::test]
async fn lost_finish_ack_retries_saved_result_without_new_model_credential_or_worker() {
    let roots = Roots::new();
    let backend = Backend::new(Mode::LostFinish).await;
    let model = Model::new(false).await;
    let account = Account::new();
    let connector = roots.connector(&backend.endpoint, account.clone(), CancellationToken::new());
    assert!(matches!(
        connector
            .run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new()
            )
            .await,
        Err(Error::Transport)
    ));
    assert!(roots.entry().join("result.json").is_file());
    drop(connector);
    let restarted = roots.connector(&backend.endpoint, account, CancellationToken::new());
    assert!(matches!(
        restarted
            .retry_result(target(), CancellationToken::new())
            .await
            .unwrap(),
        TeamConnectorStatus::Settled {
            state: TeamBotState::Completed,
            ..
        }
    ));
    assert_eq!(backend.count("startBot"), 1);
    assert_eq!(backend.count("finishBot"), 2);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lost_start_ack_and_execute_false_refuse_restart_and_quota_reset() {
    for mode in [Mode::LostStart, Mode::FalseStart] {
        let roots = Roots::new();
        let backend = Backend::new(mode).await;
        let account = Account::new();
        let connector =
            roots.connector(&backend.endpoint, account.clone(), CancellationToken::new());
        let first = connector
            .run(
                target(),
                snapshot("http://127.0.0.1:1"),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            first,
            Err(Error::Transport) | Ok(TeamConnectorStatus::RecoveryRequired)
        ));
        assert!(roots.entry().join("start-requested.json").is_file());
        drop(connector);
        let restarted = roots.connector(&backend.endpoint, account, CancellationToken::new());
        assert_eq!(
            restarted
                .run(
                    target(),
                    snapshot("http://127.0.0.1:1"),
                    CancellationToken::new()
                )
                .await
                .unwrap(),
            TeamConnectorStatus::RecoveryRequired
        );
        assert_eq!(backend.count("claimBot"), 1);
        assert_eq!(backend.count("startBot"), 1);
        assert_eq!(std::fs::read_dir(&roots.runtime).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn mismatched_or_unknown_start_identity_never_prepares_worker() {
    for (field, value) in [
        ("version", json!(2)),
        ("teamId", json!("other-team")),
        ("conversationId", json!("other-thread")),
        ("ownerUid", json!("other-owner")),
        ("ownerGeneration", json!(2)),
        ("hostId", json!("x".repeat(22))),
        ("botId", json!("x".repeat(22))),
        ("botRevision", json!(2)),
        ("botKey", json!("forged-bot")),
        ("triggerId", json!("other-trigger")),
        ("attemptId", json!("other-attempt")),
        ("origin", json!({"uid":"fixture-human","generation":0})),
        ("origin", json!({"uid":"bad/uid","generation":7})),
        ("origin", json!({"uid":"bad\u{1f}uid","generation":7})),
        ("origin", json!({"uid":"bad\u{7f}uid","generation":7})),
        ("origin", json!({"uid":"😀".repeat(65),"generation":7})),
        ("seq", json!(0)),
        ("messageId", json!("other-message")),
        ("text", json!("x".repeat(32769))),
        ("extraAuthority", json!(true)),
    ] {
        let roots = Roots::new();
        let backend = Backend::new(Mode::BadPermit(field, value)).await;
        let connector =
            roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
        assert!(
            matches!(
                connector
                    .run(
                        target(),
                        snapshot("http://127.0.0.1:1"),
                        CancellationToken::new()
                    )
                    .await,
                Err(Error::Protocol)
            ),
            "field {field}"
        );
        assert_eq!(
            std::fs::read_dir(&roots.runtime).unwrap().count(),
            0,
            "field {field}"
        );
        assert_eq!(backend.count("startBot"), 1);
    }
}

#[tokio::test]
async fn oversized_and_redirected_start_responses_cannot_grant_execution() {
    for mode in [
        Mode::StartOversized,
        Mode::StartOversizedChunked,
        Mode::StartRedirect,
        Mode::StartWithoutSnapshot,
        Mode::StartLease(now() - 1),
        Mode::StartLease(now() + 600000),
    ] {
        let roots = Roots::new();
        let backend = Backend::new(mode).await;
        let connector =
            roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
        assert!(matches!(
            connector
                .run(
                    target(),
                    snapshot("http://127.0.0.1:1"),
                    CancellationToken::new()
                )
                .await,
            Err(Error::Protocol) | Err(Error::Http(307))
        ));
        assert_eq!(std::fs::read_dir(&roots.runtime).unwrap().count(), 0);
        assert_eq!(backend.count("startBot"), 1);
    }
}

#[tokio::test]
async fn active_revocation_or_poll_failure_kills_real_worker_before_settlement() {
    for variant in 0..4 {
        let roots = Roots::new();
        let model = Model::new(true).await;
        let mode = match variant {
            0 => Mode::PollRevoked(model.active.clone()),
            1 => Mode::PollUnauthorized(model.active.clone()),
            2 => Mode::PollBadLease(model.active.clone()),
            _ => Mode::PollHang(model.active.clone()),
        };
        let backend = Backend::new(mode).await;
        let connector =
            roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            connector.run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
        match variant {
            0 => assert!(matches!(
                result,
                Ok(TeamConnectorStatus::Settled {
                    state: TeamBotState::Revoked,
                    ..
                })
            )),
            1 => assert!(matches!(result, Err(Error::Http(401)))),
            2 => assert!(matches!(result, Err(Error::Protocol))),
            _ => assert!(matches!(result, Err(Error::Transport))),
        };
        model.wait(&model.closed).await;
        assert_eq!(backend.count("finishBot"), 0);
        assert!(!roots.entry().join("result.json").exists());
    }
}

#[tokio::test]
async fn account_logout_external_cancel_and_future_drop_stop_live_worker() {
    for variant in 0..3 {
        let roots = Roots::new();
        let model = Model::new(true).await;
        let backend = Backend::new(Mode::Good).await;
        let account = Account::new();
        let session = CancellationToken::new();
        let cancel = CancellationToken::new();
        let connector = roots.connector(&backend.endpoint, account.clone(), session.clone());
        let mut run = Box::pin(connector.run(target(), snapshot(&model.endpoint), cancel.clone()));
        tokio::select! { _=model.wait(&model.active)=>(), result=&mut run=>panic!("early exit: {result:?}") }
        if variant == 0 {
            account.active.store(false, Ordering::Release);
            session.cancel();
            assert!(matches!(run.await, Err(Error::Account)));
        } else if variant == 1 {
            cancel.cancel();
            assert!(matches!(run.await, Err(Error::Cancelled)));
        } else {
            drop(run);
        }
        model.wait(&model.closed).await;
        assert_eq!(backend.count("finishBot"), 0);
    }
}

#[tokio::test]
async fn concurrent_local_drivers_share_one_durable_attempt() {
    let roots = Roots::new();
    let model = Model::new(false).await;
    let backend = Backend::new(Mode::Good).await;
    let first = roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
    let second = roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
    let (a, b) = tokio::join!(
        first.run(
            target(),
            snapshot(&model.endpoint),
            CancellationToken::new()
        ),
        second.run(
            target(),
            snapshot(&model.endpoint),
            CancellationToken::new()
        )
    );
    assert!(matches!(
        (&a, &b),
        (Ok(TeamConnectorStatus::Settled { .. }), Err(Error::Busy))
            | (Err(Error::Busy), Ok(TeamConnectorStatus::Settled { .. }))
    ));
    assert_eq!(backend.count("startBot"), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn changed_bindings_and_corrupt_result_never_mint_a_new_attempt() {
    let roots = Roots::new();
    let backend = Backend::new(Mode::LostFinish).await;
    let model = Model::new(false).await;
    let connector = roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
    assert!(connector
        .run(
            target(),
            snapshot(&model.endpoint),
            CancellationToken::new()
        )
        .await
        .is_err());
    for field in [0, 1] {
        let mut changed = target();
        if field == 0 {
            changed.bot_revision = 2;
        } else {
            changed.owner_generation = 2;
        }
        assert!(matches!(
            connector
                .run(changed, snapshot(&model.endpoint), CancellationToken::new())
                .await,
            Err(Error::Storage)
        ));
    }
    let path = roots.entry().join("result.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, b"{partial").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(matches!(
        connector
            .retry_result(target(), CancellationToken::new())
            .await,
        Err(Error::Storage)
    ));
    assert!(matches!(
        connector
            .run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new()
            )
            .await,
        Err(Error::Storage)
    ));
    assert_eq!(backend.count("startBot"), 1);
    assert_eq!(backend.count("finishBot"), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stale_credentials_precancel_and_unsafe_roots_refuse_before_http() {
    let roots = Roots::new();
    let backend = Backend::new(Mode::Good).await;
    for (uid, epoch, token) in [
        ("other-owner", 3, TOKEN),
        ("fixture-owner", 4, TOKEN),
        ("fixture-owner", 3, "bad token"),
    ] {
        let account = Arc::new(Account {
            active: AtomicBool::new(true),
            credential_uid: uid.into(),
            credential_epoch: epoch,
            token: token.into(),
        });
        let connector = roots.connector(&backend.endpoint, account, CancellationToken::new());
        assert!(matches!(
            connector
                .run(
                    target(),
                    snapshot("http://127.0.0.1:1"),
                    CancellationToken::new()
                )
                .await,
            Err(Error::Account)
        ));
    }
    let connector = roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        connector
            .run(target(), snapshot("http://127.0.0.1:1"), cancel)
            .await,
        Err(Error::Cancelled)
    ));
    assert_eq!(backend.requests.lock().unwrap().len(), 0);
    let link = roots._temp.path().join("linked-journal");
    std::os::unix::fs::symlink(&roots.journal, &link).unwrap();
    assert!(TeamConnector::new(
        &backend.endpoint,
        Account::new(),
        "fixture-owner".into(),
        3,
        CancellationToken::new(),
        link,
        roots.runtime.clone(),
        worker()
    )
    .is_err());
    std::fs::set_permissions(&roots.journal, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(TeamConnector::new(
        &backend.endpoint,
        Account::new(),
        "fixture-owner".into(),
        3,
        CancellationToken::new(),
        roots.journal.clone(),
        roots.runtime.clone(),
        worker()
    )
    .is_err());
}

/// Run only when explicitly invoked by the product's isolated emulator fixture.
/// Normal test runs have no account/network dependency beyond loopback fixtures.
#[tokio::test]
#[ignore = "requires explicit isolated Firebase emulator fixture"]
async fn actual_firebase_backend_and_worker() {
    let endpoint = std::env::var("TEAM_CONNECTOR_BACKEND").unwrap();
    let token = std::env::var("TEAM_CONNECTOR_OWNER_TOKEN").unwrap();
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let roots = Roots::new();
    let model = Model::new(false).await;
    let account = Arc::new(Account {
        active: AtomicBool::new(true),
        credential_uid: "fixture-owner".into(),
        credential_epoch: 3,
        token,
    });
    let connector = roots.connector(&endpoint, account, CancellationToken::new());
    let expected = TeamConnectorStatus::Settled {
        state: TeamBotState::Completed,
        reply_id: Some(reply_id(&target())),
    };
    assert_eq!(
        connector
            .run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        connector
            .retry_result(target(), CancellationToken::new())
            .await
            .unwrap(),
        expected
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unicode_and_punctuation_human_uid_matches_backend_and_private_profile() {
    let roots = Roots::new();
    // Includes a C1 character accepted by the backend's explicit C0/DEL rule;
    // neither ASCII-only nor Rust's broader is_control predicate is equivalent.
    let backend = Backend::new(Mode::BadPermit(
        "origin",
        json!({"uid":"用户@example.com\u{85}","generation":7}),
    ))
    .await;
    let model = Model::new(false).await;
    let connector = roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
    assert!(matches!(
        connector
            .run(
                target(),
                snapshot(&model.endpoint),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        TeamConnectorStatus::Settled {
            state: TeamBotState::Completed,
            ..
        }
    ));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

struct RefreshFailsBeforeStart(AtomicUsize);
#[async_trait]
impl TeamConnectorAccount for RefreshFailsBeforeStart {
    fn is_current(&self, uid: &str, epoch: u64) -> bool {
        uid == "fixture-owner" && epoch == 3
    }
    async fn credential(&self) -> Result<TeamConnectorCredential, Error> {
        if self.0.fetch_add(1, Ordering::AcqRel) == 1 {
            return Err(Error::Transport);
        }
        Ok(TeamConnectorCredential {
            owner_uid: "fixture-owner".into(),
            epoch: 3,
            id_token: TOKEN.into(),
        })
    }
}

#[tokio::test]
async fn durable_start_intent_before_failed_token_refresh_never_reclaims_expired_claim() {
    let roots = Roots::new();
    let backend = Backend::new(Mode::ExpiredClaim).await;
    let account = Arc::new(RefreshFailsBeforeStart(AtomicUsize::new(0)));
    let connector = roots.connector(&backend.endpoint, account, CancellationToken::new());
    assert!(matches!(
        connector
            .run(
                target(),
                snapshot("http://127.0.0.1:1"),
                CancellationToken::new()
            )
            .await,
        Err(Error::Transport)
    ));
    assert!(roots.entry().join("start-requested.json").is_file());
    assert!(!roots.entry().join("permit.json").exists());
    drop(connector);
    let restarted = roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
    assert_eq!(
        restarted
            .run(
                target(),
                snapshot("http://127.0.0.1:1"),
                CancellationToken::new()
            )
            .await
            .unwrap(),
        TeamConnectorStatus::RecoveryRequired
    );
    assert_eq!(backend.count("claimBot"), 1);
    assert_eq!(backend.count("startBot"), 0);
    assert_eq!(backend.count("botPoll"), 1);
    assert_eq!(std::fs::read_dir(&roots.runtime).unwrap().count(), 0);
}

#[tokio::test]
async fn partial_symlink_and_hardlinked_journal_records_fail_closed() {
    for variant in 0..3 {
        let roots = Roots::new();
        let backend = Backend::new(Mode::LostStart).await;
        let connector =
            roots.connector(&backend.endpoint, Account::new(), CancellationToken::new());
        assert!(matches!(
            connector
                .run(
                    target(),
                    snapshot("http://127.0.0.1:1"),
                    CancellationToken::new()
                )
                .await,
            Err(Error::Transport)
        ));
        let intent = roots.entry().join("intent.json");
        if variant == 0 {
            std::fs::set_permissions(&intent, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::write(&intent, b"{partial").unwrap();
            std::fs::set_permissions(&intent, std::fs::Permissions::from_mode(0o400)).unwrap();
        } else if variant == 1 {
            let outside = roots._temp.path().join("moved-intent.json");
            std::fs::rename(&intent, &outside).unwrap();
            std::os::unix::fs::symlink(outside, intent).unwrap();
        } else {
            std::fs::hard_link(&intent, roots._temp.path().join("linked-intent.json")).unwrap();
        }
        let calls = backend.requests.lock().unwrap().len();
        assert!(connector
            .run(
                target(),
                snapshot("http://127.0.0.1:1"),
                CancellationToken::new()
            )
            .await
            .is_err());
        assert_eq!(backend.requests.lock().unwrap().len(), calls);
        assert_eq!(std::fs::read_dir(&roots.runtime).unwrap().count(), 0);
    }
}
