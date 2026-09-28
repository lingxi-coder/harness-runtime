//! Parity: M5-06 hooks executor 4-arm matrix.
//!
//! Locks the behaviour of each executor arm (Builtin / Http / Command / Agent)
//! under `PreToolUse` + `PostToolUse` events. The Command arm is exercised with
//! its ProcessRunner + Sandbox seams so the serialized hook envelope and the
//! successful output path stay covered; a separate test keeps the structured
//! error for an executor that has no platform runner.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-14-release-v0.6.0.md` Task 5.

use async_trait::async_trait;
use hooks::{
    BuiltinHookHandler, HookContext, HookDefinition, HookEvent, HookEventType, HookExecutor,
    HookExecutorImpl, HookOutcome, HookRegistry, HookResult, HookSource,
};
use mobile_linux_api::{ProcessError, ProcessOutput, SandboxBackend};
use platform_api::{
    ProcessCommand, ProcessHandle, ProcessRunner, Sandbox, SandboxCapability, SandboxError,
    SandboxFeatures, SandboxPolicy, SandboxedCommand, SandboxedTag,
};
use protocol::{HookId, HttpResponse, SessionId, ToolUseId};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use test_harness::mocks::{MockHttpTransport, MockRuntimeSpawner, ScriptedResponse};
use tokio::sync::RwLock;

const FIXTURE: &str = include_str!("../src/parity/fixtures/parity_hooks_runtime.json");

// ============================================================================
// Fixture types
// ============================================================================

#[derive(Debug, Deserialize)]
struct Fixture {
    #[serde(rename = "_meta")]
    #[allow(dead_code)]
    meta: serde_json::Value,
    arms: Vec<Arm>,
    ssrf_guard_blocked_urls: Vec<String>,
    hook_telemetry_events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Arm {
    name: String,
    #[allow(dead_code)]
    executor_variant: String,
}

fn load() -> Fixture {
    serde_json::from_str(FIXTURE).expect("fixture parse")
}

// ============================================================================
// Helpers
// ============================================================================

fn pre_tool_use_event() -> HookEvent {
    HookEvent::PreToolUse {
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({"command": "echo hi"}),
        tool_use_id: ToolUseId::new(),
    }
}

fn post_tool_use_event() -> HookEvent {
    HookEvent::PostToolUse {
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({"command": "echo hi"}),
        tool_output: serde_json::json!({"output": "hi"}),
        tool_use_id: ToolUseId::new(),
        duration_ms: None,
    }
}

fn dummy_ctx() -> HookContext {
    HookContext {
        session_id: SessionId::nil(),
        agent_id: None,
        cwd: std::path::PathBuf::from("/tmp"),
        ..Default::default()
    }
}

#[derive(Default)]
struct StaticResolver(HashMap<(String, u16), Result<Vec<SocketAddr>, String>>);

impl StaticResolver {
    fn parity_http_hosts() -> Self {
        let mut answers = HashMap::new();
        answers.insert(
            ("mock-server.test".to_string(), 80),
            Ok(vec!["93.184.216.34:80".parse().expect("public example ip")]),
        );
        Self(answers)
    }
}

#[async_trait]
impl hooks::DnsResolver for StaticResolver {
    async fn lookup_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        self.0
            .get(&(host.to_string(), port))
            .cloned()
            .unwrap_or_else(|| Err(format!("missing resolver answer for {host}:{port}")))
    }
}

// ============================================================================
// T5 — fixture meta: arm names cover 4 variants
// ============================================================================

#[test]
fn fixture_declares_all_four_arms() {
    let f = load();
    let names: Vec<&str> = f.arms.iter().map(|a| a.name.as_str()).collect();
    for expected in &["Builtin", "Http", "Command", "Agent"] {
        assert!(
            names.contains(expected),
            "fixture must declare arm {expected:?}; found {names:?}"
        );
    }
    assert_eq!(f.arms.len(), 4, "exactly 4 arms");
}

// ============================================================================
// T5 — telemetry: hook events are registered
// ============================================================================

#[test]
fn hook_telemetry_events_are_registered() {
    let f = load();
    let registered: std::collections::HashSet<&&str> =
        telemetry::tengu::ALL_EVENT_NAMES.iter().collect();
    for name in &f.hook_telemetry_events {
        assert!(
            registered.contains(&name.as_str()),
            "hook event {name:?} not found in ALL_EVENT_NAMES"
        );
    }
}

// ============================================================================
// T5 — SSRF guard: blocks loopback + link-local URLs
// ============================================================================

#[test]
fn ssrf_guard_blocks_known_bad_urls() {
    let f = load();
    let guard = hooks::SsrfGuard::with_defaults();
    for url in &f.ssrf_guard_blocked_urls {
        assert!(
            guard.check_url(url).is_err(),
            "SSRF guard must block {url:?} but returned Ok"
        );
    }
}

// ============================================================================
// T5 — Builtin arm: in-process handler produces expected HookResult
// ============================================================================

struct NoOpBuiltin;

#[async_trait]
impl BuiltinHookHandler for NoOpBuiltin {
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: "builtin-ran".into(),
            stderr: String::new(),
            exit_code: Some(0),
            response: None,
        }
    }
    fn id(&self) -> &str {
        "noop-builtin"
    }
}

#[tokio::test]
async fn builtin_arm_pretooluse_returns_success() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(HookDefinition {
        id: HookId::new(),
        name: "builtin-hook".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: "noop-builtin".into(),
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let http = Arc::new(MockHttpTransport::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let mut exec = HookExecutorImpl::new(reg, http, runtime);
    exec.register_builtin(Arc::new(NoOpBuiltin));

    let agg = exec.execute(pre_tool_use_event(), dummy_ctx()).await;
    // aggregate contains one result; no decision means no block/approve
    assert!(agg.decision.is_none());
    // stdout of last result is propagated through
    let stdout: String = agg
        .all_results
        .iter()
        .map(|(_, r)| r.stdout.as_str())
        .collect();
    assert!(
        stdout.contains("builtin-ran"),
        "builtin stdout must be 'builtin-ran'; got {stdout:?}"
    );
}

// ============================================================================
// T5 — Http arm: MockHttpTransport 200 response → Success
// ============================================================================

#[tokio::test]
async fn http_arm_pretooluse_with_mock_transport_succeeds() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(HookDefinition {
        id: HookId::new(),
        name: "http-hook".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: HookExecutor::Http {
            url: "http://mock-server.test/hook".into(),
            method: "POST".into(),
            headers: HashMap::new(),
            allowed_env_vars: Vec::new(),
            timeout: std::time::Duration::from_secs(5),
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let mock_http = Arc::new(MockHttpTransport::new());
    // Script a 200 OK with empty body (no HookResponse JSON → parsed as None,
    // outcome = Success because status 200).
    mock_http.enqueue(ScriptedResponse::Sync(HttpResponse {
        status: 200,
        headers: vec![],
        body: String::new(),
        body_bytes: Vec::new(),
    }));

    let runtime = Arc::new(MockRuntimeSpawner::default());
    let exec = HookExecutorImpl::new(reg, mock_http, runtime).with_ssrf_guard(
        hooks::SsrfGuard::with_resolver(StaticResolver::parity_http_hosts()),
    );

    let agg = exec.execute(pre_tool_use_event(), dummy_ctx()).await;
    // The HTTP arm returns Success for status 200; no Block decision.
    assert!(
        agg.decision.is_none(),
        "200 OK with empty body must not produce a Block decision"
    );
    // No error stderr from the single result.
    let stderr: String = agg
        .all_results
        .iter()
        .map(|(_, r)| r.stderr.as_str())
        .collect();
    assert!(
        !stderr.contains("SSRF guard rejected") && !stderr.contains("http error"),
        "200 OK must not produce error stderr; got {stderr:?}"
    );
}

// ============================================================================
// T5 — Http arm: SSRF guard blocks loopback URL
// ============================================================================

#[tokio::test]
async fn http_arm_ssrf_guard_blocks_loopback_url() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(HookDefinition {
        id: HookId::new(),
        name: "http-hook-loopback".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: HookExecutor::Http {
            url: "http://127.0.0.1:8080/hook".into(),
            method: "POST".into(),
            headers: HashMap::new(),
            allowed_env_vars: Vec::new(),
            timeout: std::time::Duration::from_secs(5),
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let mock_http = Arc::new(MockHttpTransport::new());
    // No scripted response — SSRF guard must reject before hitting the transport.

    let runtime = Arc::new(MockRuntimeSpawner::default());
    let exec = HookExecutorImpl::new(reg, mock_http.clone(), runtime);

    let agg = exec.execute(pre_tool_use_event(), dummy_ctx()).await;
    // SSRF guard should have produced an Error result with the rejection message.
    let stderr: String = agg
        .all_results
        .iter()
        .map(|(_, r)| r.stderr.as_str())
        .collect();
    assert!(
        stderr.contains("SSRF guard rejected url"),
        "expected SSRF guard rejection in stderr, got: {stderr:?}"
    );
    // The mock transport should have received no requests.
    assert!(
        mock_http.received_requests().is_empty(),
        "SSRF guard must prevent the HTTP transport from being called"
    );
}

// ============================================================================
// T5 — Command arm: ProcessRunner + Sandbox path
// ============================================================================

/// Deterministic ProcessRunner for the command-hook parity test. The hook
/// executor receives only a SandboxedCommand; recording its inner stdin proves
/// the sandbox boundary was crossed before the process runner was called.
struct RecordingProcessRunner {
    output: Mutex<Option<ProcessOutput>>,
    stdin: Mutex<Option<String>>,
}

impl RecordingProcessRunner {
    fn new(output: ProcessOutput) -> Arc<Self> {
        Arc::new(Self {
            output: Mutex::new(Some(output)),
            stdin: Mutex::new(None),
        })
    }
}

#[async_trait]
impl ProcessRunner for RecordingProcessRunner {
    async fn run(&self, command: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        *self.stdin.lock().expect("stdin mutex") = command.inner().stdin.clone();
        self.output
            .lock()
            .expect("output mutex")
            .take()
            .ok_or_else(|| ProcessError::Io("recording runner called twice".into()))
    }

    async fn spawn_background(
        &self,
        _command: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// A no-isolation sandbox used only to exercise the public minting seam in the
/// harness. Production hooks receive a platform implementation here.
struct RecordingSandbox;

#[async_trait]
impl Sandbox for RecordingSandbox {
    fn is_available(&self) -> bool {
        true
    }

    fn backend(&self) -> SandboxBackend {
        SandboxBackend::None
    }

    fn prepare(
        &self,
        command: ProcessCommand,
        _policy: &SandboxPolicy,
    ) -> Result<SandboxedCommand, SandboxError> {
        Ok(SandboxedCommand::__new_sandboxed(
            command,
            SandboxedTag::BypassAuditedWithReason {
                reason: "test".into(),
            },
        ))
    }

    fn bypass_with_audit(&self, command: ProcessCommand, reason: &str) -> SandboxedCommand {
        SandboxedCommand::__new_sandboxed(
            command,
            SandboxedTag::BypassAuditedWithReason {
                reason: reason.into(),
            },
        )
    }

    async fn probe_capability(&self) -> SandboxCapability {
        SandboxCapability {
            available: true,
            reason: None,
            features: SandboxFeatures::default(),
        }
    }
}

fn command_hook_definition() -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: "command-hook".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: HookExecutor::Command {
            command: "hook.sh".into(),
            args: vec!["--check".into()],
            env: HashMap::new(),
            cwd: None,
            shell: None,
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    }
}

#[tokio::test]
async fn command_arm_with_runner_executes_and_receives_hook_envelope() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(command_hook_definition());

    let runner = RecordingProcessRunner::new(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    let exec = HookExecutorImpl::new(
        reg,
        Arc::new(MockHttpTransport::new()),
        Arc::new(MockRuntimeSpawner::default()),
    )
    .with_process_runner(runner.clone(), Arc::new(RecordingSandbox));

    let aggregate = exec.execute(pre_tool_use_event(), dummy_ctx()).await;
    assert!(aggregate.decision.is_none(), "exit 0 must not block");
    let result = &aggregate.all_results[0].1;
    assert!(matches!(result.outcome, HookOutcome::Success));
    assert_eq!(result.exit_code, Some(0));

    let stdin = runner
        .stdin
        .lock()
        .expect("stdin mutex")
        .clone()
        .expect("command hook stdin");
    assert!(
        stdin.contains(r#""hook_event_name":"PreToolUse"#),
        "hook envelope must identify PreToolUse: {stdin:?}"
    );
    assert!(stdin.contains(r#""tool_name":"Bash"#));
    assert!(
        stdin.ends_with('\n'),
        "command hook payload needs trailing newline"
    );
}

// ============================================================================
// T5 — Command arm: missing runner returns structured fallback error
// ============================================================================

#[tokio::test]
async fn command_arm_without_runner_returns_structured_error() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(HookDefinition {
        id: HookId::new(),
        name: "cmd-hook".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: HookExecutor::Command {
            command: "true".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            shell: None,
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let http = Arc::new(MockHttpTransport::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let exec = HookExecutorImpl::new(reg, http, runtime);

    let agg = exec.execute(pre_tool_use_event(), dummy_ctx()).await;
    // The production Command arm is wired when both platform seams are
    // attached. Without them it intentionally returns the structured fallback
    // error instead of attempting an unsandboxed process spawn.
    let stderr: String = agg
        .all_results
        .iter()
        .map(|(_, r)| r.stderr.as_str())
        .collect();
    assert!(
        stderr.contains("command executor not wired"),
        "Command arm must return the documented not-wired error; got: {stderr:?}"
    );
}

// ============================================================================
// T5 — Agent arm: without SubagentSpawner wired, returns Error
// ============================================================================

#[tokio::test]
async fn agent_arm_without_spawner_returns_error() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(HookDefinition {
        id: HookId::new(),
        name: "agent-hook".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: HookExecutor::Agent {
            agent_type: "general-purpose".into(),
            prompt: "check the user's action".into(),
            model: None,
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let http = Arc::new(MockHttpTransport::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    // No with_agent_spawner() call → agent_spawner = None.
    let exec = HookExecutorImpl::new(reg, http, runtime);

    let agg = exec.execute(pre_tool_use_event(), dummy_ctx()).await;
    // Without a spawner, the agent arm returns an error.
    let stderr: String = agg
        .all_results
        .iter()
        .map(|(_, r)| r.stderr.as_str())
        .collect();
    assert!(
        stderr.contains("agent executor not wired"),
        "Agent arm without spawner must return the documented error; got: {stderr:?}"
    );
}

// ============================================================================
// T5 — PostToolUse event: Builtin arm fires on PostToolUse too
// ============================================================================

#[tokio::test]
async fn builtin_arm_posttooluse_fires_correctly() {
    let reg = Arc::new(RwLock::new(HookRegistry::new()));
    reg.write().await.register(HookDefinition {
        id: HookId::new(),
        name: "builtin-post".into(),
        events: vec![HookEventType::PostToolUse],
        if_condition: None,
        executor: HookExecutor::Builtin {
            handler_id: "noop-builtin".into(),
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let http = Arc::new(MockHttpTransport::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let mut exec = HookExecutorImpl::new(reg, http, runtime);
    exec.register_builtin(Arc::new(NoOpBuiltin));

    let agg = exec.execute(post_tool_use_event(), dummy_ctx()).await;
    assert!(agg.decision.is_none(), "no decision for noop builtin");
    let stdout: String = agg
        .all_results
        .iter()
        .map(|(_, r)| r.stdout.as_str())
        .collect();
    assert!(
        stdout.contains("builtin-ran"),
        "builtin stdout must be 'builtin-ran'; got {stdout:?}"
    );
}
