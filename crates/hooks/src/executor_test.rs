//! Trailing tests extracted from executor.rs.

use super::*;

#[cfg(test)]
mod constants_tests {
    use super::*;

    #[test]
    fn http_timeout_is_10_minutes() {
        assert_eq!(HOOK_HTTP_TIMEOUT_MS, 600_000);
    }
    #[test]
    fn command_timeout_is_10_minutes() {
        assert_eq!(HOOK_COMMAND_TIMEOUT_MS, 600_000);
    }
    #[test]
    fn agent_timeout_is_60_seconds() {
        assert_eq!(HOOK_AGENT_TIMEOUT_MS, 60_000);
    }

    #[test]
    fn session_end_floor_and_cap_match_binary() {
        // claude-code `nzn=1500`, `rym=60000` (BIN off 205765355 / 205765364).
        assert_eq!(SESSION_END_HOOK_TIMEOUT_FLOOR_MS, 1_500);
        assert_eq!(SESSION_END_HOOK_TIMEOUT_CAP_MS, 60_000);
        assert_eq!(
            SESSION_END_HOOKS_TIMEOUT_ENV,
            "LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS"
        );
    }
}

#[cfg(test)]
mod session_end_timeout_tests {
    use super::{max_per_hook_timeout_ms, session_end_batch_timeout_ms};
    use crate::definition::{HookDefinition, HookExecutor, HookSource};
    use crate::events::HookEventType;
    use lingxi_core::types::HookId;
    use std::time::Duration;

    fn hook_with_timeout(timeout: Option<Duration>) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "test-session-end".into(),
            events: vec![HookEventType::SessionEnd],
            if_condition: None,
            executor: HookExecutor::Builtin {
                handler_id: "noop".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    #[test]
    fn default_with_no_per_hook_timeouts_collapses_to_floor() {
        // No env, no per-hook timeout ⇒ max(1500, min(0, 60000)) = 1500.
        assert_eq!(session_end_batch_timeout_ms(None, 0), 1_500);
    }

    #[test]
    fn default_clamps_below_floor_up_to_1500() {
        // max per-hook = 800ms ⇒ max(1500, min(800, 60000)) = 1500.
        assert_eq!(session_end_batch_timeout_ms(None, 800), 1_500);
    }

    #[test]
    fn default_uses_max_per_hook_when_between_floor_and_cap() {
        // max per-hook = 30000ms ⇒ max(1500, min(30000, 60000)) = 30000.
        assert_eq!(session_end_batch_timeout_ms(None, 30_000), 30_000);
    }

    #[test]
    fn default_caps_above_60000_down_to_cap() {
        // max per-hook = 120000ms ⇒ max(1500, min(120000, 60000)) = 60000.
        assert_eq!(session_end_batch_timeout_ms(None, 120_000), 60_000);
    }

    #[test]
    fn env_override_is_used_verbatim_and_unclamped() {
        // A finite, positive env value is returned EXACTLY (bypasses floor/cap).
        assert_eq!(session_end_batch_timeout_ms(Some("3000"), 0), 3_000);
        // Below the floor — still returned verbatim (env override is unclamped).
        assert_eq!(session_end_batch_timeout_ms(Some("100"), 30_000), 100);
        // Above the cap — still returned verbatim.
        assert_eq!(session_end_batch_timeout_ms(Some("999999"), 0), 999_999);
    }

    #[test]
    fn env_parseint_style_trailing_suffix_is_ignored() {
        // JS `parseInt("3000abc", 10)` ⇒ 3000.
        assert_eq!(session_end_batch_timeout_ms(Some("3000abc"), 0), 3_000);
        assert_eq!(session_end_batch_timeout_ms(Some("  4200 "), 0), 4_200);
    }

    #[test]
    fn env_non_numeric_or_nonpositive_falls_through_to_clamp() {
        // `parseInt("abc")` ⇒ NaN ⇒ computed clamp (here floor).
        assert_eq!(session_end_batch_timeout_ms(Some("abc"), 0), 1_500);
        assert_eq!(session_end_batch_timeout_ms(Some(""), 0), 1_500);
        // Zero / negative are rejected by the `> 0` guard ⇒ computed clamp.
        assert_eq!(session_end_batch_timeout_ms(Some("0"), 30_000), 30_000);
        assert_eq!(session_end_batch_timeout_ms(Some("-5"), 45_000), 45_000);
    }

    #[test]
    fn max_per_hook_timeout_takes_the_largest_declared() {
        let hooks = vec![
            hook_with_timeout(Some(Duration::from_secs(5))),
            hook_with_timeout(Some(Duration::from_secs(42))),
            hook_with_timeout(None),
            hook_with_timeout(Some(Duration::from_secs(3))),
        ];
        // max = 42s = 42000ms.
        assert_eq!(max_per_hook_timeout_ms(&hooks), 42_000);
        // Feeding that into the clamp ⇒ within [1500, 60000] ⇒ 42000.
        assert_eq!(
            session_end_batch_timeout_ms(None, max_per_hook_timeout_ms(&hooks)),
            42_000
        );
    }

    #[test]
    fn max_per_hook_timeout_empty_or_all_none_is_zero() {
        assert_eq!(max_per_hook_timeout_ms(&[]), 0);
        let hooks = vec![hook_with_timeout(None), hook_with_timeout(None)];
        assert_eq!(max_per_hook_timeout_ms(&hooks), 0);
    }
}

/// Integration-style tests that exercise [`HookExecutorImpl::execute_session_end`]
/// end-to-end: a slow `SessionEnd` hook is cut off by the batch shutdown deadline,
/// and a fast one completes normally.
#[cfg(test)]
mod session_end_batch_deadline_tests {
    use super::*;
    use crate::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
    use crate::events::{HookEvent, HookEventType};
    use crate::registry::{HookContext, HookRegistry};
    use crate::response::{HookOutcome, HookResult};
    use lingxi_core::host::RuntimeError;
    use lingxi_core::types::HookId;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `HttpTransport` stub — the Builtin arm never touches HTTP.
    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    struct UnusedRuntime;
    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
            Err(RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    /// A builtin handler that sleeps `delay`, then flips `ran` and returns
    /// success. If the batch deadline cuts it off, the sleep is cancelled and
    /// `ran` stays `false`.
    struct SleepingHandler {
        id: String,
        delay: Duration,
        ran: Arc<AtomicBool>,
    }
    #[async_trait]
    impl BuiltinHookHandler for SleepingHandler {
        async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
            tokio::time::sleep(self.delay).await;
            self.ran.store(true, Ordering::SeqCst);
            HookResult {
                outcome: HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                response: None,
            }
        }
        fn id(&self) -> &str {
            &self.id
        }
    }

    fn session_end_builtin_hook(handler_id: &str) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "test-session-end".into(),
            events: vec![HookEventType::SessionEnd],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: handler_id.into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
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

    fn session_end_event() -> HookEvent {
        HookEvent::SessionEnd {
            session_id: lingxi_core::types::SessionId::nil(),
            reason: "logout".into(),
        }
    }

    /// Both env-driven deadline behaviors in ONE test (the two halves share the
    /// process-global `LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS`, so they must not
    /// race in parallel — merging them keeps the env mutation single-threaded):
    ///
    /// * a SessionEnd hook running LONGER than the batch deadline is cut off
    ///   (outcome `Timeout`, side-effect never lands, returns fast); and
    /// * a hook completing WITHIN the deadline runs to completion (the deadline
    ///   is a ceiling, not a forced wait).
    #[tokio::test]
    async fn batch_deadline_cuts_off_slow_hook_but_not_fast_hook() {
        // The deadline is set PER EXECUTOR rather than through
        // `SESSION_END_HOOKS_TIMEOUT_ENV`. That variable is a process global and
        // Rust runs a binary's tests on parallel threads in one process, so the
        // old `set_var` windows made every neighbouring SessionEnd dispatch run
        // under this test's deadline — a 50ms ceiling briefly imposed on the
        // whole binary.

        // --- Half 1: tiny deadline (50ms), hook sleeps 5s → cut off. ---
        let slow_ran = Arc::new(AtomicBool::new(false));
        let mut registry = HookRegistry::new();
        registry.register(session_end_builtin_hook("slow"));
        let reg = Arc::new(RwLock::new(registry));
        let mut exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
            .with_session_end_timeout_ms("50");
        exec.register_builtin(Arc::new(SleepingHandler {
            id: "slow".into(),
            delay: Duration::from_secs(5),
            ran: slow_ran.clone(),
        }));
        let start = std::time::Instant::now();
        let slow_agg = exec
            .execute_session_end(session_end_event(), HookContext::default())
            .await;
        let elapsed = start.elapsed();

        // --- Half 2: generous deadline (5000ms), hook sleeps 10ms → completes. ---
        let fast_ran = Arc::new(AtomicBool::new(false));
        let mut registry = HookRegistry::new();
        registry.register(session_end_builtin_hook("fast"));
        let reg = Arc::new(RwLock::new(registry));
        let mut exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
            .with_session_end_timeout_ms("5000");
        exec.register_builtin(Arc::new(SleepingHandler {
            id: "fast".into(),
            delay: Duration::from_millis(10),
            ran: fast_ran.clone(),
        }));
        let fast_agg = exec
            .execute_session_end(session_end_event(), HookContext::default())
            .await;

        // Half 1 assertions: cut off at the CONFIGURED 50ms, not merely somewhere
        // before the 5s sleep. The default deadline is the 1500ms floor
        // (`SESSION_END_HOOK_TIMEOUT_FLOOR_MS`, and this fixture declares no
        // per-hook timeout), so a bound of 2s would pass whether or not the 50ms
        // was honoured at all — it did, which is how the override could have
        // silently stopped being read.
        assert!(
            elapsed < Duration::from_millis(500),
            "the configured 50ms deadline must be the one that applies (the default \
             floor is {SESSION_END_HOOK_TIMEOUT_FLOOR_MS}ms), took {elapsed:?}"
        );
        assert!(
            !slow_ran.load(Ordering::SeqCst),
            "the slow hook's success side-effect must NOT land — it was aborted"
        );
        let (_, slow_r) = &slow_agg.all_results[0];
        assert!(
            matches!(slow_r.outcome, HookOutcome::Timeout),
            "cut-off hook records a Timeout outcome"
        );
        assert!(slow_r.stderr.contains("batch deadline"));

        // Half 2 assertions: ran to completion.
        assert!(
            fast_ran.load(Ordering::SeqCst),
            "a hook finishing within the deadline must run to completion"
        );
        let (_, fast_r) = &fast_agg.all_results[0];
        assert!(matches!(fast_r.outcome, HookOutcome::Success));
    }

    /// No registered SessionEnd hook ⇒ strict no-op (default aggregate), no
    /// deadline machinery observable.
    #[tokio::test]
    async fn no_session_end_hook_is_a_noop() {
        let reg = Arc::new(RwLock::new(HookRegistry::new()));
        let exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));

        let agg = exec
            .execute_session_end(session_end_event(), HookContext::default())
            .await;

        assert!(agg.all_results.is_empty());
        assert_eq!(agg.decision, None);
    }
}

#[cfg(test)]
mod command_arm_tests {
    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::{HookEvent, HookEventType};
    use crate::response::HookDecision;
    use lingxi_core::host::sandbox::{SandboxBackend, SandboxCapability, SandboxedTag};
    use lingxi_core::host::{
        ProcessHandle, ProcessOutput, RuntimeError, SandboxPolicy, SandboxedCommand,
    };
    use lingxi_core::types::{HookId, ToolUseId};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Mock `ProcessRunner` that returns a canned `ProcessOutput` (or
    /// `ProcessError`) and records the `SandboxedCommand` it was handed so the
    /// test can assert the stdin payload + trailing newline.
    struct MockRunner {
        result: Mutex<Option<Result<ProcessOutput, ProcessError>>>,
        recorded_stdin: Mutex<Option<String>>,
        /// B2: the child env the arm handed to the sandbox, captured so tests
        /// can assert `LINGXI_PROJECT_DIR` injection + precedence.
        recorded_env: Mutex<Option<HashMap<String, String>>>,
        /// #43: the resolved command + args (after `${LINGXI_PROJECT_DIR}`
        /// substitution), captured so tests can assert the token replacement.
        recorded_cwd: Mutex<Option<Option<PathBuf>>>,
        recorded_command: Mutex<Option<String>>,
        recorded_args: Mutex<Option<Vec<String>>>,
    }

    impl MockRunner {
        fn ok(output: ProcessOutput) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Some(Ok(output))),
                recorded_stdin: Mutex::new(None),
                recorded_env: Mutex::new(None),
                recorded_cwd: Mutex::new(None),
                recorded_command: Mutex::new(None),
                recorded_args: Mutex::new(None),
            })
        }
        fn err(e: ProcessError) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Some(Err(e))),
                recorded_stdin: Mutex::new(None),
                recorded_env: Mutex::new(None),
                recorded_cwd: Mutex::new(None),
                recorded_command: Mutex::new(None),
                recorded_args: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl ProcessRunner for MockRunner {
        async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            *self.recorded_stdin.lock().unwrap() = cmd.inner().stdin.clone();
            *self.recorded_env.lock().unwrap() = Some(cmd.inner().env.clone());
            *self.recorded_cwd.lock().unwrap() = Some(cmd.inner().cwd.clone());
            *self.recorded_command.lock().unwrap() = Some(cmd.inner().command.clone());
            *self.recorded_args.lock().unwrap() = Some(cmd.inner().args.clone());
            self.result
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Err(ProcessError::Io("no script".into())))
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
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

    /// Minimal `Sandbox` that mints a `SandboxedCommand` via the documented
    /// external-impl seam (`__new_sandboxed`) so the runner has something to
    /// accept. No real isolation — adequate for unit-testing the arm's
    /// output-mapping logic.
    struct StubSandbox;

    #[async_trait]
    impl Sandbox for StubSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::None
        }
        fn prepare(
            &self,
            cmd: ProcessCommand,
            _policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, lingxi_core::host::SandboxError> {
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: "test".into(),
                },
            ))
        }
        fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: reason.into(),
                },
            )
        }
        async fn probe_capability(&self) -> SandboxCapability {
            SandboxCapability {
                available: true,
                reason: None,
                features: lingxi_core::host::SandboxFeatures::default(),
            }
        }
    }

    /// `RuntimeSpawner` stub — the runtime arm is never exercised here.
    struct UnusedRuntime;
    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
            Err(RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    /// `HttpTransport` stub — the HTTP arm is never exercised here.
    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    fn output(stdout: &str, stderr: &str, exit_code: i32) -> ProcessOutput {
        ProcessOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            timed_out: false,
        }
    }

    fn command_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "test-command".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Command {
                command: "hook.sh".into(),
                args: vec!["--check".into()],
                env: HashMap::new(),
                cwd: None,
                shell: None,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
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

    fn pre_event() -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: ToolUseId::new(),
        }
    }

    /// Build an executor whose registry already holds a single Command hook
    /// matching `PreToolUse`, with the supplied runner + stub sandbox wired.
    fn executor_with(process: Arc<dyn ProcessRunner>) -> HookExecutorImpl {
        let mut registry = HookRegistry::new();
        registry.register(command_hook());
        let reg = Arc::new(RwLock::new(registry));
        HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
            .with_process_runner(process, Arc::new(StubSandbox))
    }

    #[tokio::test]
    async fn exit_zero_allows_no_decision() {
        let runner = MockRunner::ok(output("approved\n", "", 0));
        let exec = executor_with(runner.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None, "exit 0 plain text must not block");
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Success));
        assert_eq!(r.exit_code, Some(0));
        assert!(r.response.is_none());
        // stdin payload carries the serialized PreToolUse envelope + trailing
        // newline (claude-code parity).
        let stdin = runner.recorded_stdin.lock().unwrap().clone().unwrap();
        assert!(stdin.contains(r#""hook_event_name":"PreToolUse""#));
        assert!(stdin.contains(r#""tool_name":"Bash""#));
        assert!(stdin.ends_with('\n'), "trailing newline is load-bearing");
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_reason() {
        let runner = MockRunner::ok(output("", "policy violation", 2));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        // Binary: `[${getHookDisplayText(hook)}]: ${stderr||"No stderr output"}`.
        // The test hook is `hook.sh --check` ⇒ display = command + args joined.
        assert_eq!(
            agg.reason.as_deref(),
            Some("[hook.sh --check]: policy violation")
        );
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert_eq!(r.exit_code, Some(2));
    }

    #[tokio::test]
    async fn exit_two_empty_stderr_falls_back_to_placeholder() {
        // Only a TRULY empty stderr falls back (`||`, empty string is falsy).
        let runner = MockRunner::ok(output("", "", 2));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(
            agg.reason.as_deref(),
            Some("[hook.sh --check]: No stderr output")
        );
    }

    #[tokio::test]
    async fn exit_two_whitespace_stderr_is_preserved_verbatim() {
        // A whitespace-only stderr is TRUTHY in JS `||`, so the binary uses it
        // verbatim (NOT trimmed, NOT the placeholder).
        let runner = MockRunner::ok(output("", "   ", 2));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("[hook.sh --check]:    "));
    }

    #[tokio::test]
    async fn json_stdout_decision_drives_block() {
        let runner = MockRunner::ok(output(
            r#"{"decision":"block","stopReason":"json said no","continue":false}"#,
            "",
            0,
        ));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("json said no"));
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Success), "exit 0 + JSON");
    }

    #[tokio::test]
    async fn json_stdout_permission_allow_approves() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#,
            "",
            0,
        ));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Approve));
    }

    /// PARITY 2.1.263 `H_n` — under `CLAUDE_CODE_EVAL_CONFINED=true` a hook's
    /// ALLOW is dropped before it reaches the aggregate: a confined eval run
    /// takes permission grants only from its command line. Block and ask are
    /// untouched, which is the point of running hooks in a confined harness at
    /// all.
    /// The confined fold, exercised WITHOUT touching the process environment.
    ///
    /// This used to `set_var("CLAUDE_CODE_EVAL_CONFINED", …)` around each arm.
    /// Rust runs a binary's tests on parallel threads in ONE process, so during
    /// those windows every other test that built a hook decision was silently
    /// confined too — and several of them assert `Approve` / `Allow`. The flag
    /// is now per-executor, so the arms below are hermetic.
    #[tokio::test]
    async fn confined_session_drops_a_hook_permission_allow() {
        let allow_json =
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#;

        // Baseline: unconfined, the allow lands.
        let exec =
            executor_with(MockRunner::ok(output(allow_json, "", 0))).with_eval_confined(false);
        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(
            agg.decision,
            Some(HookDecision::Approve),
            "unconfined, a hook allow must still approve"
        );

        // Confined: the same output yields no decision at all.
        let exec =
            executor_with(MockRunner::ok(output(allow_json, "", 0))).with_eval_confined(true);
        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(
            agg.decision, None,
            "a confined session must not take an allow from a hook"
        );

        // …but a BLOCK still binds.
        let exec = executor_with(MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
            "",
            0,
        )))
        .with_eval_confined(true);
        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(
            agg.decision,
            Some(HookDecision::Block),
            "a confined session must still honour a hook block"
        );
    }

    /// A neighbour that must stay green while the confined test runs beside it.
    ///
    /// Before the flag was per-executor this could not exist honestly: an
    /// executor built during the other test's `set_var` window dropped its
    /// allow. Its value is as a canary — if the gate ever goes back to reading
    /// the environment, this is what starts flaking.
    #[tokio::test]
    async fn a_default_executor_is_not_confined_while_a_confined_one_runs() {
        let allow_json =
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#;
        let confined =
            executor_with(MockRunner::ok(output(allow_json, "", 0))).with_eval_confined(true);
        let plain = executor_with(MockRunner::ok(output(allow_json, "", 0)));

        let (confined_agg, plain_agg) = tokio::join!(
            confined.execute(pre_event(), HookContext::default()),
            plain.execute(pre_event(), HookContext::default()),
        );

        assert_eq!(
            confined_agg.decision, None,
            "the confined executor drops it"
        );
        assert_eq!(
            plain_agg.decision,
            Some(HookDecision::Approve),
            "a concurrent default executor keeps its allow"
        );
    }

    /// The binary compares against the literal `true`; the usual truthy
    /// spellings do NOT arm it.
    ///
    /// Pinned against the pure comparison rather than the process environment:
    /// a `set_var` here would be visible to every other test running in
    /// parallel in this same binary, several of which assert that a hook allow
    /// survives.
    #[test]
    fn eval_confined_matches_only_the_literal_true() {
        use lingxi_core::host::env::is_eval_confined_value;
        assert!(!is_eval_confined_value(None));
        for spelling in ["1", "yes", "on", "TRUE", ""] {
            assert!(
                !is_eval_confined_value(Some(spelling)),
                "{spelling:?} must not arm the confined gate"
            );
        }
        assert!(is_eval_confined_value(Some("true")));
    }

    #[tokio::test]
    async fn permission_request_json_allow_carries_rewrite_and_raw_updates() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"command":"printf safe"},"updatedPermissions":[{"type":"addRules","rules":[{"toolName":"Bash"}],"behavior":"allow","destination":"session"}]}}}"#,
            "",
            0,
        ));
        let exec = executor_for(HookEventType::PermissionRequest, runner);
        let event = HookEvent::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "printf unsafe"}),
            reason: "needs approval".into(),
        };

        let agg = exec.execute(event, HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Allow));
        assert_eq!(agg.modified_input, Some(json!({"command": "printf safe"})));
        assert_eq!(agg.permission_updates.len(), 1);
        assert_eq!(agg.permission_updates[0]["destination"], "session");
        assert!(!agg.interrupt);
    }

    #[tokio::test]
    async fn permission_request_json_allow_aggregates_set_mode_auto_update() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{},"updatedPermissions":[{"type":"setMode","mode":"auto","destination":"session"}]}}}"#,
            "",
            0,
        ));
        let exec = executor_for(HookEventType::PermissionRequest, runner);
        let event = HookEvent::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "printf unsafe"}),
            reason: "needs approval".into(),
        };

        let agg = exec.execute(event, HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Allow));
        assert_eq!(
            agg.permission_updates,
            vec![json!({
                "type": "setMode",
                "mode": "auto",
                "destination": "session"
            })]
        );
        assert!(matches!(
            agg.permission_request_result,
            Some(PermissionRequestResult::Allow {
                updated_permissions: Some(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn permission_request_json_deny_carries_message_and_interrupt() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"not safe","interrupt":true}}}"#,
            "ignored stderr",
            2,
        ));
        let exec = executor_for(HookEventType::PermissionRequest, runner);
        let event = HookEvent::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            reason: "needs approval".into(),
        };

        let agg = exec.execute(event, HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("not safe"));
        assert!(agg.interrupt);
    }

    #[tokio::test]
    async fn permission_request_plain_exit_two_is_not_a_block() {
        let runner = MockRunner::ok(output("", "ignored stderr", 2));
        let exec = executor_for(HookEventType::PermissionRequest, runner);
        let event = HookEvent::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            reason: "needs approval".into(),
        };

        let agg = exec.execute(event, HookContext::default()).await;

        assert_eq!(agg.decision, None);
        assert_eq!(agg.reason, None);
        let (_, result) = &agg.all_results[0];
        assert_eq!(result.stderr, "ignored stderr");
        assert_eq!(result.exit_code, Some(2));
        assert!(result.response.is_none());
    }

    #[tokio::test]
    async fn json_parse_failure_is_error_for_non_blocking_exits() {
        // Once stdout starts with `{`, a malformed payload is JSON output, not
        // ordinary hook text. Non-blocking exits surface the parser error and
        // never become a successful plain-text hook result.
        for exit_code in [0, 1] {
            let runner = MockRunner::ok(output("{bad", "child diagnostic", exit_code));
            let exec = executor_with(runner);

            let agg = exec.execute(pre_event(), HookContext::default()).await;

            assert_eq!(agg.decision, None, "JSON parse failure must not block");
            let (_, r) = &agg.all_results[0];
            assert!(matches!(r.outcome, HookOutcome::Error));
            assert_eq!(r.exit_code, Some(exit_code));
            assert_eq!(r.stdout, "{bad");
            assert_eq!(
                r.stderr,
                "hook response is not valid JSON: key must be a string at line 1 column 2"
            );
            assert!(r.response.is_none());
        }
    }

    #[tokio::test]
    async fn json_parse_failure_at_exit_two_keeps_block_fallback() {
        // Exit 2 remains the command-hook blocking signal even when the
        // payload that preceded it looked like JSON but could not be parsed.
        let runner = MockRunner::ok(output("{bad", "child diagnostic", 2));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert_eq!(r.exit_code, Some(2));
        assert_eq!(r.stderr, "child diagnostic");
        assert_eq!(
            r.response
                .as_ref()
                .and_then(|response| response.reason.as_deref()),
            Some("[hook.sh --check]: child diagnostic")
        );
    }

    #[tokio::test]
    async fn json_schema_failure_is_error_for_non_blocking_exits() {
        // A valid JSON object with a known field of the wrong type is a schema
        // failure. Non-blocking exits must not be reinterpreted as success.
        for exit_code in [0, 1] {
            let runner = MockRunner::ok(output(
                r#"{"continue":"no"}"#,
                "child diagnostic",
                exit_code,
            ));
            let exec = executor_with(runner);

            let agg = exec.execute(pre_event(), HookContext::default()).await;

            assert_eq!(agg.decision, None, "schema failure must not block");
            let (_, r) = &agg.all_results[0];
            assert!(matches!(r.outcome, HookOutcome::Error));
            assert_eq!(r.exit_code, Some(exit_code));
            assert_eq!(
                r.stderr,
                "Hook JSON output validation failed — continue: expected boolean, received string"
            );
            assert!(r.response.is_none());
        }
    }

    #[tokio::test]
    async fn json_schema_failure_at_exit_two_keeps_block_fallback() {
        // Exit 2 remains a block even when JSON schema validation fails.
        let runner = MockRunner::ok(output(r#"{"continue":"no"}"#, "child diagnostic", 2));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert_eq!(r.exit_code, Some(2));
        assert_eq!(r.stderr, "child diagnostic");
        assert_eq!(
            agg.reason.as_deref(),
            Some("[hook.sh --check]: child diagnostic")
        );
    }

    #[tokio::test]
    async fn pre_tool_use_invalid_permission_answer_is_schema_error() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":123}}"#,
            "",
            0,
        ));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None);
        let (_, result) = &agg.all_results[0];
        assert!(matches!(result.outcome, HookOutcome::Error));
        assert!(result.response.is_none());
        assert!(result.stderr.contains("Hook JSON output validation failed"));
        assert!(result.stderr.contains("permissionDecision"));
    }

    #[tokio::test]
    async fn permission_request_invalid_answer_is_schema_error() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":"allow"}}"#,
            "",
            0,
        ));
        let exec = executor_for(HookEventType::PermissionRequest, runner);
        let event = HookEvent::PermissionRequest {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            reason: "destructive".into(),
        };

        let agg = exec.execute(event, HookContext::default()).await;

        assert_eq!(agg.decision, None);
        let (_, result) = &agg.all_results[0];
        assert!(matches!(result.outcome, HookOutcome::Error));
        assert!(result.response.is_none());
        assert!(result.stderr.contains("Hook JSON output validation failed"));
        assert!(result.stderr.contains("hookSpecificOutput.decision"));
    }

    #[tokio::test]
    async fn json_event_mismatch_is_error_not_plain_text_fallback() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse"}}"#,
            "",
            0,
        ));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None, "event mismatch must not block");
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(
            r.stderr,
            "hook response hookEventName mismatch: expected 'PreToolUse', got 'PostToolUse'"
        );
        assert!(r.response.is_none());
    }

    #[tokio::test]
    async fn other_nonzero_is_non_blocking_error() {
        let runner = MockRunner::ok(output("", "transient", 1));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None, "non-0/non-2 must NOT block");
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert_eq!(r.exit_code, Some(1));
        assert!(r.response.is_none());
    }

    #[tokio::test]
    async fn process_io_error_surfaces_as_error() {
        let runner = MockRunner::err(ProcessError::Io("spawn failed".into()));
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None);
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert!(r.stderr.contains("process error"));
        assert!(r.stderr.contains("spawn failed"));
    }

    #[tokio::test]
    async fn timed_out_output_maps_to_timeout() {
        let runner = MockRunner::ok(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            timed_out: true,
        });
        let exec = executor_with(runner);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Timeout));
        assert_eq!(agg.decision, None);
    }

    #[tokio::test]
    async fn not_wired_returns_structured_error() {
        let mut registry = HookRegistry::new();
        registry.register(command_hook());
        let reg = Arc::new(RwLock::new(registry));
        // No `.with_process_runner(..)` — Command arm must report "not wired".
        let exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None, "not-wired must not block");
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert!(r.stderr.contains("command executor not wired"));
    }

    // ---- B2: LINGXI_PROJECT_DIR injection into the Command child env -----

    /// A Command hook whose declared `env` is seeded with `entries`.
    fn command_hook_with_env(entries: &[(&str, &str)]) -> HookDefinition {
        let mut h = command_hook();
        if let DefHookExecutor::Command { env, .. } = &mut h.executor {
            for (k, v) in entries {
                env.insert((*k).to_string(), (*v).to_string());
            }
        }
        h
    }

    /// Executor wired with a single Command hook (custom `env`) + the recording
    /// runner, so the test can assert the child env the sandbox received.
    fn executor_with_hook(
        hook: HookDefinition,
        process: Arc<dyn ProcessRunner>,
    ) -> HookExecutorImpl {
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let reg = Arc::new(RwLock::new(registry));
        HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
            .with_process_runner(process, Arc::new(StubSandbox))
    }

    #[tokio::test]
    async fn command_env_contains_project_dir_from_ctx() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let ctx = HookContext {
            project_dir: Some(PathBuf::from("/repo/root")),
            cwd: PathBuf::from("/repo/root/worktree"),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert_eq!(
            env.get("LINGXI_PROJECT_DIR").map(String::as_str),
            Some("/repo/root"),
            "engine project_dir is injected verbatim",
        );
    }

    #[tokio::test]
    async fn command_env_engine_project_dir_wins_over_user_env() {
        // The hook declares its own LINGXI_PROJECT_DIR; the engine value is set
        // AFTER the base spread in claude-code (`utils/hooks.ts:882-885`), so
        // the engine value wins. Match that precedence.
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with_hook(
            command_hook_with_env(&[
                ("LINGXI_PROJECT_DIR", "/user/override"),
                ("MY_VAR", "keep-me"),
            ]),
            runner.clone(),
        );
        let ctx = HookContext {
            project_dir: Some(PathBuf::from("/engine/root")),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert_eq!(
            env.get("LINGXI_PROJECT_DIR").map(String::as_str),
            Some("/engine/root"),
            "engine value overrides the user-supplied hook.env entry",
        );
        // Unrelated user env entries are preserved.
        assert_eq!(env.get("MY_VAR").map(String::as_str), Some("keep-me"));
    }

    #[tokio::test]
    async fn command_env_project_dir_falls_back_to_cwd() {
        // No project_dir wired → LINGXI_PROJECT_DIR falls back to ctx.cwd, the
        // faithful approximation until the orchestrator populates a project root.
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let ctx = HookContext {
            project_dir: None,
            cwd: PathBuf::from("/some/cwd"),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert_eq!(
            env.get("LINGXI_PROJECT_DIR").map(String::as_str),
            Some("/some/cwd"),
            "absent project_dir falls back to ctx.cwd",
        );
    }

    #[tokio::test]
    async fn command_cwd_invalid_explicit_path_falls_back_to_project_dir() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with_hook(
            command_hook_with_cwd("/definitely/not/a/dir"),
            runner.clone(),
        );
        let ctx = HookContext {
            cwd: PathBuf::from("/also/not/a/dir"),
            project_dir: Some(PathBuf::from("/tmp")),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        assert_eq!(
            runner.recorded_cwd.lock().unwrap().clone().unwrap(),
            Some(PathBuf::from("/tmp"))
        );
    }

    #[tokio::test]
    async fn command_cwd_invalid_session_cwd_falls_back_to_home() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let home_dir = super::hook_home_dir().expect("a platform home directory must be set");
        let ctx = HookContext {
            cwd: PathBuf::from("/definitely/not/a/dir"),
            project_dir: Some(PathBuf::from("/also/not/a/dir")),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        assert_eq!(
            runner.recorded_cwd.lock().unwrap().clone().unwrap(),
            Some(home_dir)
        );
    }

    #[tokio::test]
    async fn command_env_forwards_traceparent_from_ctx() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let ctx = HookContext {
            trace_context: Some(telemetry::otel::SerializedTraceContext {
                traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
                tracestate: Some("foo=bar".into()),
            }),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert_eq!(
            env.get("TRACEPARENT").map(String::as_str),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
        assert!(
            !env.contains_key("TRACESTATE"),
            "command env stays byte-faithful to TRACEPARENT-only upstream output"
        );
    }

    // ---- #43: COLUMNS/LINES env + ${LINGXI_PROJECT_DIR} substitution -------

    /// A Command hook with a custom `command` + `args`, so #43 substitution can
    /// be asserted against the recorded resolved values.
    fn command_hook_with_cmd_args(command: &str, args: &[&str]) -> HookDefinition {
        let mut h = command_hook();
        if let DefHookExecutor::Command {
            command: c,
            args: a,
            ..
        } = &mut h.executor
        {
            *c = command.to_string();
            *a = args.iter().map(|s| (*s).to_string()).collect();
        }
        h
    }

    fn command_hook_with_cwd(cwd: &str) -> HookDefinition {
        let mut h = command_hook();
        if let DefHookExecutor::Command { cwd: slot, .. } = &mut h.executor {
            *slot = Some(PathBuf::from(cwd));
        }
        h
    }

    #[test]
    fn substitute_project_dir_replaces_every_occurrence() {
        // `replaceAll` semantics — every `${LINGXI_PROJECT_DIR}` token is
        // replaced, not just the first.
        assert_eq!(
            substitute_project_dir("${LINGXI_PROJECT_DIR}/a:${LINGXI_PROJECT_DIR}/b", "/root"),
            "/root/a:/root/b",
        );
    }

    #[test]
    fn substitute_project_dir_fast_path_no_template() {
        // `if(!fe.includes("${"))return fe` — a string with no `${` is returned
        // untouched.
        assert_eq!(
            substitute_project_dir("./fmt.sh --check", "/root"),
            "./fmt.sh --check"
        );
    }

    #[test]
    fn substitute_project_dir_leaves_plugin_tokens_untouched() {
        // The plugin tokens are NOT substituted here (no plugin scope) — a
        // residual; a string carrying only those passes through unchanged.
        assert_eq!(
            substitute_project_dir("${LINGXI_PLUGIN_ROOT}/x", "/root"),
            "${LINGXI_PLUGIN_ROOT}/x",
        );
    }

    /// OBS-1 — a `command` hook with NO argv is a SHELL STRING upstream.
    ///
    /// claude-code 2.1.238 spawns it as `spawn(M, [], {shell: He, …})`
    /// (@296948400) with `He = true` on POSIX, i.e. `/bin/sh -c <M>`. The port
    /// used to bare-exec the whole string, so `./fmt.sh --all` — and every hook
    /// containing a pipe, a redirect or `&&` — died with ENOENT. Nothing caught
    /// it because the fixtures only ever PARSED such hooks.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn shell_form_command_hook_runs_through_sh() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with_hook(
            command_hook_with_cmd_args("./fmt.sh --all && echo done", &[]),
            runner.clone(),
        );

        let _ = exec.execute(pre_event(), HookContext::default()).await;

        let cmd = runner.recorded_command.lock().unwrap().clone().unwrap();
        let args = runner.recorded_args.lock().unwrap().clone().unwrap();
        assert_eq!(cmd, "/bin/sh", "shell-form hook must spawn a shell");
        assert_eq!(
            args,
            vec!["-c".to_string(), "./fmt.sh --all && echo done".to_string()],
            "the whole command string is handed to `sh -c`, unsplit"
        );
    }

    /// The exec form is a SEPARATE upstream branch (`if(I) spawn(I[0], I[1],…)`)
    /// and must NOT be wrapped — otherwise an argv hook would get its arguments
    /// re-parsed by a shell.
    #[tokio::test]
    async fn exec_form_command_hook_is_not_shell_wrapped() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with_hook(
            command_hook_with_cmd_args("./fmt.sh", &["--all", "a b"]),
            runner.clone(),
        );

        let _ = exec.execute(pre_event(), HookContext::default()).await;

        let cmd = runner.recorded_command.lock().unwrap().clone().unwrap();
        let args = runner.recorded_args.lock().unwrap().clone().unwrap();
        assert_eq!(cmd, "./fmt.sh", "argv form keeps its own binary");
        assert_eq!(
            args,
            vec!["--all".to_string(), "a b".to_string()],
            "an arg containing a space stays ONE arg — no shell re-splitting"
        );
    }

    #[tokio::test]
    async fn command_and_args_substitute_project_dir_token() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with_hook(
            command_hook_with_cmd_args(
                "${LINGXI_PROJECT_DIR}/.lingxi/fmt.sh",
                &["--root", "${LINGXI_PROJECT_DIR}", "--plain"],
            ),
            runner.clone(),
        );
        let ctx = HookContext {
            project_dir: Some(PathBuf::from("/repo/root")),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let cmd = runner.recorded_command.lock().unwrap().clone().unwrap();
        let args = runner.recorded_args.lock().unwrap().clone().unwrap();
        assert_eq!(
            cmd, "/repo/root/.lingxi/fmt.sh",
            "command token substituted"
        );
        assert_eq!(
            args,
            vec![
                "--root".to_string(),
                "/repo/root".to_string(),
                "--plain".to_string()
            ],
            "each arg token substituted; non-token args untouched",
        );
    }

    #[tokio::test]
    async fn command_env_sets_columns_and_lines_from_ctx() {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let ctx = HookContext {
            terminal_columns: Some(120),
            terminal_rows: Some(40),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert_eq!(env.get("COLUMNS").map(String::as_str), Some("120"));
        assert_eq!(env.get("LINES").map(String::as_str), Some("40"));
    }

    #[tokio::test]
    async fn command_env_omits_columns_lines_when_absent_or_zero() {
        // The binary's `if(L)`/`if(D)` falsy guards: `None` (non-TTY,
        // `process.stdout.columns === undefined`) and `0` set NEITHER env var.
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let ctx = HookContext {
            terminal_columns: None,
            terminal_rows: Some(0),
            ..Default::default()
        };

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert!(!env.contains_key("COLUMNS"), "None columns sets no COLUMNS");
        assert!(!env.contains_key("LINES"), "zero rows is falsy → no LINES");
    }

    #[tokio::test]
    async fn command_env_sets_uot_harness_vars() {
        // #43: the hook command env spreads `...Uot(o)` with `source:"harness"`
        // (BIN off 205727901 / 199137330): LINGXI=1, LINGXI_SESSION_ID,
        // LINGXI_CHILD_SESSION=1 are always present. `AI_AGENT` is gated on
        // `source==="agent"`, so a hook child must NOT carry it.
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let ctx = HookContext::default();
        let expected_session = ctx.session_id.to_string();

        let _ = exec.execute(pre_event(), ctx).await;

        let env = runner.recorded_env.lock().unwrap().clone().unwrap();
        assert_eq!(env.get("LINGXI").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("LINGXI_CHILD_SESSION").map(String::as_str),
            Some("1"),
        );
        assert_eq!(
            env.get("LINGXI_SESSION_ID").map(String::as_str),
            Some(expected_session.as_str()),
            "Uot threads the session id into the hook child",
        );
        assert!(
            !env.contains_key("AI_AGENT"),
            "hook source is harness, not agent — AI_AGENT must be absent",
        );
    }

    #[tokio::test]
    async fn command_env_sets_effort_only_when_present() {
        // `Uot` sets `LINGXI_EFFORT=effortLevel` only when `effortLevel` is set
        // (`o.effortLevel = hookInput.effort?.level`). No effort on the ctx ⇒
        // the env var is omitted; an effort level ⇒ it is set verbatim.
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_with(runner.clone());
        let _ = exec.execute(pre_event(), HookContext::default()).await;
        assert!(
            !runner
                .recorded_env
                .lock()
                .unwrap()
                .clone()
                .unwrap()
                .contains_key("LINGXI_EFFORT"),
            "no effort on ctx ⇒ no LINGXI_EFFORT",
        );

        let runner2 = MockRunner::ok(output("", "", 0));
        let exec2 = executor_with(runner2.clone());
        let ctx = HookContext {
            effort: Some(crate::hook_payload::EffortLevel::new("high")),
            ..Default::default()
        };
        let _ = exec2.execute(pre_event(), ctx).await;
        assert_eq!(
            runner2
                .recorded_env
                .lock()
                .unwrap()
                .clone()
                .unwrap()
                .get("LINGXI_EFFORT")
                .map(String::as_str),
            Some("high"),
            "effort level surfaces as LINGXI_EFFORT",
        );
    }

    // ---- B1: lifecycle events now serialize through the Command arm -----

    /// A Command hook subscribed to a single lifecycle `event` type.
    fn command_hook_for(event: HookEventType) -> HookDefinition {
        let mut h = command_hook();
        h.events = vec![event];
        h
    }

    /// Executor wired with a Command hook subscribed to `event`.
    fn executor_for(event: HookEventType, process: Arc<dyn ProcessRunner>) -> HookExecutorImpl {
        let mut registry = HookRegistry::new();
        registry.register(command_hook_for(event));
        let reg = Arc::new(RwLock::new(registry));
        HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime))
            .with_process_runner(process, Arc::new(StubSandbox))
    }

    /// Dispatch `event` through a Command hook and return the stdin the child
    /// would have received (the serialized envelope). Empty if no hook fired.
    async fn dispatch_and_capture(event_type: HookEventType, event: HookEvent) -> String {
        dispatch_and_capture_with_ctx(event_type, event, HookContext::default()).await
    }

    async fn dispatch_and_capture_with_ctx(
        event_type: HookEventType,
        event: HookEvent,
        ctx: HookContext,
    ) -> String {
        let runner = MockRunner::ok(output("", "", 0));
        let exec = executor_for(event_type, runner.clone());
        let agg = exec.execute(event, ctx).await;
        assert_eq!(agg.all_results.len(), 1, "exactly one hook must fire");
        let captured = runner.recorded_stdin.lock().unwrap().clone().unwrap();
        captured
    }

    #[tokio::test]
    async fn stop_event_serializes_through_command_arm() {
        let stdin = dispatch_and_capture(
            HookEventType::Stop,
            HookEvent::Stop {
                reason: "done".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"Stop""#));
        assert!(stdin.contains(r#""stop_hook_active":false"#));
        assert!(stdin.ends_with('\n'), "trailing newline is load-bearing");
    }

    #[tokio::test]
    async fn subagent_stop_event_serializes_agent_id() {
        let agent_id = lingxi_core::types::AgentId::new();
        let stdin = dispatch_and_capture(
            HookEventType::SubagentStop,
            HookEvent::SubagentStop {
                agent_id,
                status: "completed".into(),
                agent_type: String::new(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"SubagentStop""#));
        assert!(stdin.contains(&format!(r#""agent_id":"{agent_id}""#)));
        assert!(stdin.contains(r#""stop_hook_active":false"#));
    }

    #[tokio::test]
    async fn stop_event_serializes_live_reentry_and_last_message() {
        let stdin = dispatch_and_capture_with_ctx(
            HookEventType::Stop,
            HookEvent::Stop {
                reason: "done".into(),
            },
            HookContext {
                stop_hook_active: true,
                last_assistant_message: Some("finished work".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(stdin.contains(r#""stop_hook_active":true"#));
        assert!(stdin.contains(r#""last_assistant_message":"finished work""#));
    }

    /// End-to-end: a `HookContext::prompt_id` must reach the hook child's stdin
    /// on a lifecycle event, and be OMITTED when the context has none. Oracle
    /// `createBaseHookInput` (2.1.238 minified `c_`, BIN off 296935693) puts
    /// `prompt_id:Vut()??void 0` on the base shared by all 31 events.
    #[tokio::test]
    async fn prompt_id_reaches_the_hook_child_stdin() {
        let stdin = dispatch_and_capture_with_ctx(
            HookEventType::Stop,
            HookEvent::Stop {
                reason: "done".into(),
            },
            HookContext {
                prompt_id: Some("7f1f0e2a-0000-4000-8000-000000000001".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(
            stdin.contains(r#""prompt_id":"7f1f0e2a-0000-4000-8000-000000000001""#),
            "{stdin}"
        );

        let without = dispatch_and_capture_with_ctx(
            HookEventType::Stop,
            HookEvent::Stop {
                reason: "done".into(),
            },
            HookContext::default(),
        )
        .await;
        assert!(
            !without.contains("prompt_id"),
            "absent until the first user input — the key must be omitted: {without}"
        );
    }

    #[tokio::test]
    async fn subagent_stop_serializes_live_context_fields() {
        let agent_id = lingxi_core::types::AgentId::new();
        let stdin = dispatch_and_capture_with_ctx(
            HookEventType::SubagentStop,
            HookEvent::SubagentStop {
                agent_id,
                status: "completed".into(),
                agent_type: "general-purpose".into(),
            },
            HookContext {
                stop_hook_active: true,
                last_assistant_message: Some("child done".into()),
                agent_transcript_path: Some(std::path::PathBuf::from("/tmp/agent-7.jsonl")),
                ..Default::default()
            },
        )
        .await;
        assert!(stdin.contains(r#""stop_hook_active":true"#));
        assert!(stdin.contains(r#""agent_transcript_path":"/tmp/agent-7.jsonl""#));
        assert!(stdin.contains(r#""last_assistant_message":"child done""#));
    }

    #[tokio::test]
    async fn stop_failure_serializes_last_assistant_message() {
        let stdin = dispatch_and_capture_with_ctx(
            HookEventType::StopFailure,
            HookEvent::StopFailure {
                error: "invalid_request".into(),
            },
            HookContext {
                last_assistant_message: Some("API Error: prompt too long".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(stdin.contains(r#""last_assistant_message":"API Error: prompt too long""#));
    }

    #[tokio::test]
    async fn task_completed_event_serializes_task_id() {
        let stdin = dispatch_and_capture(
            HookEventType::TaskCompleted,
            HookEvent::TaskCompleted {
                task_id: "task-99".into(),
                status: "completed".into(),
                task_subject: "ship the thing".into(),
                task_description: Some("do the work".into()),
                teammate_name: Some("buddy".into()),
                team_name: Some("alpha".into()),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"TaskCompleted""#));
        assert!(stdin.contains(r#""task_id":"task-99""#));
        // Full wire payload (coreSchemas.ts:614-625) — sourced from the event,
        // not defaulted. `status` is routing-only and must NOT appear.
        assert!(stdin.contains(r#""task_subject":"ship the thing""#));
        assert!(stdin.contains(r#""task_description":"do the work""#));
        assert!(stdin.contains(r#""teammate_name":"buddy""#));
        assert!(stdin.contains(r#""team_name":"alpha""#));
        assert!(
            !stdin.contains(r#""status""#),
            "TaskCompleted wire payload has no `status` field: {stdin}"
        );
    }

    #[tokio::test]
    async fn user_prompt_submit_event_serializes_prompt() {
        let stdin = dispatch_and_capture(
            HookEventType::UserPromptSubmit,
            HookEvent::UserPromptSubmit {
                prompt: "do the thing".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"UserPromptSubmit""#));
        assert!(stdin.contains(r#""prompt":"do the thing""#));
    }

    #[tokio::test]
    async fn session_start_event_serializes_source() {
        let stdin = dispatch_and_capture(
            HookEventType::SessionStart,
            HookEvent::SessionStart {
                session_id: lingxi_core::types::SessionId::nil(),
                source: "startup".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"SessionStart""#));
        assert!(stdin.contains(r#""source":"startup""#));
    }

    #[tokio::test]
    async fn stop_failure_event_serializes_error() {
        let stdin = dispatch_and_capture(
            HookEventType::StopFailure,
            HookEvent::StopFailure {
                error: "rate_limit".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"StopFailure""#));
        assert!(stdin.contains(r#""error":"rate_limit""#));
    }

    // ---- B6: additional events now serialize through the Command arm -----

    #[tokio::test]
    async fn post_tool_use_failure_event_serializes_error_and_tool_input() {
        let stdin = dispatch_and_capture(
            HookEventType::PostToolUseFailure,
            HookEvent::PostToolUseFailure {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "ls"}),
                error: "boom".into(),
                tool_use_id: ToolUseId::new(),
                duration_ms: None,
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"PostToolUseFailure""#));
        assert!(stdin.contains(r#""tool_name":"Bash""#));
        assert!(stdin.contains(r#""error":"boom""#));
        // The dispatched `tool_input` is now carried verbatim, not `null`.
        assert!(stdin.contains(r#""tool_input":{"command":"ls"}"#));
    }

    #[tokio::test]
    async fn session_end_event_serializes_reason() {
        let stdin = dispatch_and_capture(
            HookEventType::SessionEnd,
            HookEvent::SessionEnd {
                session_id: lingxi_core::types::SessionId::nil(),
                reason: "logout".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"SessionEnd""#));
        assert!(stdin.contains(r#""reason":"logout""#));
    }

    #[tokio::test]
    async fn pre_compact_event_serializes_trigger_and_null_instructions() {
        let stdin = dispatch_and_capture(
            HookEventType::PreCompact,
            HookEvent::PreCompact {
                reason: "manual".into(),
                custom_instructions: None,
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"PreCompact""#));
        assert!(stdin.contains(r#""trigger":"manual""#));
        // `.nullable()` field is always present as `null` when absent.
        assert!(stdin.contains(r#""custom_instructions":null"#));
    }

    #[tokio::test]
    async fn pre_compact_event_serializes_custom_instructions() {
        let stdin = dispatch_and_capture(
            HookEventType::PreCompact,
            HookEvent::PreCompact {
                reason: "manual".into(),
                custom_instructions: Some("focus on tests".into()),
            },
        )
        .await;
        assert!(stdin.contains(r#""custom_instructions":"focus on tests""#));
    }

    #[tokio::test]
    async fn post_compact_event_serializes_summary() {
        let stdin = dispatch_and_capture(
            HookEventType::PostCompact,
            HookEvent::PostCompact {
                summary: "did the thing".into(),
                tokens_freed: 1234,
                trigger: "manual".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"PostCompact""#));
        assert!(stdin.contains(r#""compact_summary":"did the thing""#));
        assert!(stdin.contains(r#""trigger":"manual""#));
    }

    #[tokio::test]
    async fn model_switch_events_serialize_required_cache_metadata() {
        let pre = dispatch_and_capture(
            HookEventType::PreModelSwitch,
            HookEvent::PreModelSwitch {
                from_model: "claude-sonnet-4-6".into(),
                to_model: "claude-opus-4-6".into(),
                requested_model: None,
                source: "command".into(),
                context_tokens: 12_345,
                prompt_cache_warm: false,
                cache_ttl: "5m".into(),
                estimated_cache_write_usd: 0.123,
                pricing: "catalog".into(),
            },
        )
        .await;
        assert!(pre.contains(r#""hook_event_name":"PreModelSwitch""#));
        assert!(pre.contains(r#""from_model":"claude-sonnet-4-6""#));
        assert!(pre.contains(r#""to_model":"claude-opus-4-6""#));
        // `requested_model` is required-but-nullable, so null is retained.
        assert!(pre.contains(r#""requested_model":null"#));
        assert!(pre.contains(r#""source":"command""#));
        assert!(pre.contains(r#""context_tokens":12345"#));
        assert!(pre.contains(r#""prompt_cache_warm":false"#));
        assert!(pre.contains(r#""cache_ttl":"5m""#));
        assert!(pre.contains(r#""estimated_cache_write_usd":0.123"#));
        assert!(pre.contains(r#""pricing":"catalog""#));

        let post = dispatch_and_capture(
            HookEventType::PostModelSwitch,
            HookEvent::PostModelSwitch {
                from_model: "claude-sonnet-4-6".into(),
                to_model: "claude-opus-4-6".into(),
                requested_model: Some("opus".into()),
                source: "resume".into(),
                context_tokens: 12_345,
                prompt_cache_warm: true,
                cache_ttl: "1h".into(),
                estimated_cache_write_usd: 0.0,
                pricing: "default".into(),
            },
        )
        .await;
        assert!(post.contains(r#""hook_event_name":"PostModelSwitch""#));
        assert!(post.contains(r#""requested_model":"opus""#));
        assert!(post.contains(r#""source":"resume""#));
        assert!(post.contains(r#""cache_ttl":"1h""#));
    }

    #[tokio::test]
    async fn pre_model_switch_execution_failure_blocks_the_switch() {
        let runner = MockRunner::err(ProcessError::Timeout);
        let exec = executor_for(HookEventType::PreModelSwitch, runner);

        let agg = exec
            .execute(
                HookEvent::PreModelSwitch {
                    from_model: "sonnet".into(),
                    to_model: "opus".into(),
                    requested_model: Some("opus".into()),
                    source: "sdk".into(),
                    context_tokens: 0,
                    prompt_cache_warm: false,
                    cache_ttl: "5m".into(),
                    estimated_cache_write_usd: 0.0,
                    pricing: "default".into(),
                },
                HookContext::default(),
            )
            .await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert!(agg
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("timed out")));
    }

    #[tokio::test]
    async fn post_model_switch_failure_is_best_effort_and_additional_context_survives() {
        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PostModelSwitch","permissionDecision":"deny","additionalContext":"warm the new model"}}"#,
            "late failure",
            2,
        ));
        let exec = executor_for(HookEventType::PostModelSwitch, runner);
        let agg = exec
            .execute(
                HookEvent::PostModelSwitch {
                    from_model: "sonnet".into(),
                    to_model: "opus".into(),
                    requested_model: Some("opus".into()),
                    source: "picker".into(),
                    context_tokens: 42,
                    prompt_cache_warm: false,
                    cache_ttl: "5m".into(),
                    estimated_cache_write_usd: 0.0,
                    pricing: "default".into(),
                },
                HookContext::default(),
            )
            .await;

        // A post-switch process failure cannot gate or undo the mutation. The
        // valid response is still retained when the process exits non-zero.
        assert_eq!(agg.decision, None);
        assert!(agg.additional_contexts.is_empty());

        let runner = MockRunner::ok(output(
            r#"{"hookSpecificOutput":{"hookEventName":"PostModelSwitch","permissionDecision":"deny","additionalContext":"warm the new model"}}"#,
            "",
            0,
        ));
        let exec = executor_for(HookEventType::PostModelSwitch, runner);
        let agg = exec
            .execute(
                HookEvent::PostModelSwitch {
                    from_model: "sonnet".into(),
                    to_model: "opus".into(),
                    requested_model: Some("opus".into()),
                    source: "picker".into(),
                    context_tokens: 42,
                    prompt_cache_warm: false,
                    cache_ttl: "5m".into(),
                    estimated_cache_write_usd: 0.0,
                    pricing: "default".into(),
                },
                HookContext::default(),
            )
            .await;
        assert_eq!(agg.decision, None, "post decisions never gate");
        assert_eq!(agg.additional_contexts, vec!["warm the new model"]);
    }

    #[tokio::test]
    async fn notification_event_serializes_message_and_type() {
        let stdin = dispatch_and_capture(
            HookEventType::Notification,
            HookEvent::Notification {
                message: "build done".into(),
                kind: "info".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"Notification""#));
        assert!(stdin.contains(r#""message":"build done""#));
        assert!(stdin.contains(r#""notification_type":"info""#));
    }

    #[tokio::test]
    async fn permission_request_event_serializes_tool() {
        let stdin = dispatch_and_capture(
            HookEventType::PermissionRequest,
            HookEvent::PermissionRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "rm -rf /"}),
                reason: "destructive".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"PermissionRequest""#));
        assert!(stdin.contains(r#""tool_name":"Bash""#));
        assert!(stdin.contains(r#""tool_input":{"command":"rm -rf /"}"#));
        // The variant's `reason` has no wire counterpart and must NOT appear.
        assert!(!stdin.contains(r#""reason""#));
    }

    #[tokio::test]
    async fn setup_event_serializes_through_command_arm() {
        let stdin = dispatch_and_capture(
            HookEventType::Setup,
            HookEvent::Setup {
                trigger: "init".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"Setup""#));
        assert!(stdin.contains(r#""trigger":"init""#));
    }

    #[tokio::test]
    async fn subagent_start_event_serializes_agent() {
        let agent_id = lingxi_core::types::AgentId::new();
        let stdin = dispatch_and_capture(
            HookEventType::SubagentStart,
            HookEvent::SubagentStart {
                agent_id,
                agent_type: "general-purpose".into(),
                parent_agent_id: None,
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"SubagentStart""#));
        assert!(stdin.contains(&format!(r#""agent_id":"{agent_id}""#)));
        assert!(stdin.contains(r#""agent_type":"general-purpose""#));
    }

    #[tokio::test]
    async fn cwd_changed_event_serializes_paths() {
        let stdin = dispatch_and_capture(
            HookEventType::CwdChanged,
            HookEvent::CwdChanged {
                old: std::path::PathBuf::from("/old"),
                new: std::path::PathBuf::from("/new"),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"CwdChanged""#));
        assert!(stdin.contains(r#""old_cwd":"/old""#));
        assert!(stdin.contains(r#""new_cwd":"/new""#));
    }

    #[tokio::test]
    async fn file_changed_event_serializes_path_and_event() {
        let stdin = dispatch_and_capture(
            HookEventType::FileChanged,
            HookEvent::FileChanged {
                path: std::path::PathBuf::from("/work/src/main.rs"),
                kind: "change".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"FileChanged""#));
        assert!(stdin.contains(r#""file_path":"/work/src/main.rs""#));
        assert!(stdin.contains(r#""event":"change""#));
    }

    #[tokio::test]
    async fn worktree_remove_event_serializes_path() {
        let stdin = dispatch_and_capture(
            HookEventType::WorktreeRemove,
            HookEvent::WorktreeRemove {
                path: std::path::PathBuf::from("/work/.worktrees/feat"),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"WorktreeRemove""#));
        assert!(stdin.contains(r#""worktree_path":"/work/.worktrees/feat""#));
    }

    // ---- deferred-completion batch: the final four events now serialize -----

    #[tokio::test]
    async fn config_change_event_serializes_source_and_file_path() {
        let stdin = dispatch_and_capture(
            HookEventType::ConfigChange,
            HookEvent::ConfigChange {
                source: crate::events::ConfigChangeSource::LocalSettings,
                file_path: Some(std::path::PathBuf::from(
                    "/work/.lingxi/settings.local.json",
                )),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"ConfigChange""#));
        // `source` serializes to the snake_case wire literal.
        assert!(stdin.contains(r#""source":"local_settings""#));
        assert!(stdin.contains(r#""file_path":"/work/.lingxi/settings.local.json""#));
    }

    #[tokio::test]
    async fn config_change_event_omits_absent_file_path() {
        let stdin = dispatch_and_capture(
            HookEventType::ConfigChange,
            HookEvent::ConfigChange {
                source: crate::events::ConfigChangeSource::PolicySettings,
                file_path: None,
            },
        )
        .await;
        assert!(stdin.contains(r#""source":"policy_settings""#));
        // `.optional()` field is skipped (not `null`) when absent.
        assert!(!stdin.contains(r#""file_path""#));
        // `createBaseHookInput(undefined)` ⇒ no permission_mode on the wire.
        assert!(!stdin.contains(r#""permission_mode""#));
    }

    #[tokio::test]
    async fn instructions_loaded_event_serializes_required_fields() {
        let stdin = dispatch_and_capture(
            HookEventType::InstructionsLoaded,
            HookEvent::InstructionsLoaded {
                file_path: std::path::PathBuf::from("/work/LINGXI.md"),
                memory_type: crate::events::InstructionsMemoryType::Project,
                load_reason: crate::events::InstructionsLoadReason::SessionStart,
                globs: None,
                trigger_file_path: None,
                parent_file_path: None,
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"InstructionsLoaded""#));
        assert!(stdin.contains(r#""file_path":"/work/LINGXI.md""#));
        // `memory_type` serializes PascalCase (no rename) per the TS enum.
        assert!(stdin.contains(r#""memory_type":"Project""#));
        // `load_reason` serializes snake_case per the TS enum.
        assert!(stdin.contains(r#""load_reason":"session_start""#));
        // optional fields skipped when absent.
        assert!(!stdin.contains(r#""globs""#));
        assert!(!stdin.contains(r#""trigger_file_path""#));
        assert!(!stdin.contains(r#""parent_file_path""#));
    }

    #[tokio::test]
    async fn instructions_loaded_event_serializes_optionals() {
        let stdin = dispatch_and_capture(
            HookEventType::InstructionsLoaded,
            HookEvent::InstructionsLoaded {
                file_path: std::path::PathBuf::from("/work/rules/api.md"),
                memory_type: crate::events::InstructionsMemoryType::Local,
                load_reason: crate::events::InstructionsLoadReason::PathGlobMatch,
                globs: Some(vec!["src/**/*.rs".into()]),
                trigger_file_path: Some(std::path::PathBuf::from("/work/src/main.rs")),
                parent_file_path: Some(std::path::PathBuf::from("/work/LINGXI.md")),
            },
        )
        .await;
        assert!(stdin.contains(r#""memory_type":"Local""#));
        assert!(stdin.contains(r#""load_reason":"path_glob_match""#));
        assert!(stdin.contains(r#""globs":["src/**/*.rs"]"#));
        assert!(stdin.contains(r#""trigger_file_path":"/work/src/main.rs""#));
        assert!(stdin.contains(r#""parent_file_path":"/work/LINGXI.md""#));
    }

    #[tokio::test]
    async fn elicitation_event_serializes_server_and_message() {
        let stdin = dispatch_and_capture(
            HookEventType::Elicitation,
            HookEvent::Elicitation {
                server_name: "github".into(),
                message: "Authorize access?".into(),
                mode: None,
                url: None,
                elicitation_id: None,
                requested_schema: None,
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"Elicitation""#));
        assert!(stdin.contains(r#""mcp_server_name":"github""#));
        assert!(stdin.contains(r#""message":"Authorize access?""#));
        // optional fields skipped when absent.
        assert!(!stdin.contains(r#""mode""#));
        assert!(!stdin.contains(r#""url""#));
        assert!(!stdin.contains(r#""elicitation_id""#));
        assert!(!stdin.contains(r#""requested_schema""#));
    }

    #[tokio::test]
    async fn elicitation_event_serializes_optionals() {
        let stdin = dispatch_and_capture(
            HookEventType::Elicitation,
            HookEvent::Elicitation {
                server_name: "linear".into(),
                message: "Pick a project".into(),
                mode: Some(crate::events::ElicitationMode::Form),
                url: Some("https://example.test/auth".into()),
                elicitation_id: Some("elic-42".into()),
                requested_schema: Some(json!({"type": "object"})),
            },
        )
        .await;
        assert!(stdin.contains(r#""mode":"form""#));
        assert!(stdin.contains(r#""url":"https://example.test/auth""#));
        assert!(stdin.contains(r#""elicitation_id":"elic-42""#));
        assert!(stdin.contains(r#""requested_schema":{"type":"object"}"#));
    }

    #[tokio::test]
    async fn worktree_create_event_serializes_name() {
        let stdin = dispatch_and_capture(
            HookEventType::WorktreeCreate,
            HookEvent::WorktreeCreate {
                name: "feature-x".into(),
                path: std::path::PathBuf::from("/work/.worktrees/feature-x"),
                branch: "feature-x".into(),
            },
        )
        .await;
        assert!(stdin.contains(r#""hook_event_name":"WorktreeCreate""#));
        assert!(stdin.contains(r#""name":"feature-x""#));
        // Only `name` is on the wire — the engine-side path/branch must NOT leak.
        assert!(!stdin.contains(r#""path""#));
        assert!(!stdin.contains(r#""branch""#));
        assert!(!stdin.contains(r#"".worktrees""#));
    }

    #[test]
    fn still_unported_events_return_none() {
        // The deferred-completion batch ported the final four events that lacked
        // a field to source a *required* wire value; the hook-firing batch then
        // ported `PermissionDenied` (extending its `HookEvent` variant with
        // `tool_input` / `tool_use_id`), the lifecycle-firing batch ported
        // `TaskCreated` (fired through the `TaskCreatedFirer` seam, mirroring
        // `TaskCompleted`), and the teammate-idle batch ported `TeammateIdle`
        // (extending its variant with `teammate_name` / `team_name`, fired
        // through the `TeammateIdleFirer` seam). The parity-fix batch then ported
        // `ElicitationResult` (P0 gap). All 30 `HookEvent` variants are now
        // serializable — this test is a no-op guard for future new unported variants.
        let ctx = HookContext::default();
        let unported: Vec<HookEvent> = vec![];
        for ev in unported {
            assert!(
                build_envelope_body(&ev, &ctx).is_none(),
                "still-unported event must not serialize: {ev:?}"
            );
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive event-marker table — one entry per covered event"
    )]
    fn build_envelope_body_returns_expected_event_markers() {
        let ctx = HookContext::default();
        let cases: Vec<(HookEvent, &'static str)> = vec![
            (HookEvent::Stop { reason: "r".into() }, "Stop"),
            (
                HookEvent::SubagentStop {
                    agent_id: lingxi_core::types::AgentId::new(),
                    status: "completed".into(),
                    agent_type: String::new(),
                },
                "SubagentStop",
            ),
            (
                HookEvent::TaskCompleted {
                    task_id: "t".into(),
                    status: "completed".into(),
                    task_subject: "s".into(),
                    task_description: None,
                    teammate_name: None,
                    team_name: None,
                },
                "TaskCompleted",
            ),
            (
                HookEvent::TaskCreated {
                    task_id: "t".into(),
                    task_type: "LocalBash".into(),
                    description: "do the work".into(),
                    teammate_name: None,
                    team_name: None,
                },
                "TaskCreated",
            ),
            (
                HookEvent::TeammateIdle {
                    teammate_name: "buddy".into(),
                    team_name: "alpha".into(),
                },
                "TeammateIdle",
            ),
            (
                HookEvent::UserPromptSubmit { prompt: "p".into() },
                "UserPromptSubmit",
            ),
            (
                HookEvent::SessionStart {
                    session_id: lingxi_core::types::SessionId::nil(),
                    source: "startup".into(),
                },
                "SessionStart",
            ),
            (
                HookEvent::StopFailure {
                    error: "unknown".into(),
                },
                "StopFailure",
            ),
            // B6 additions.
            (
                HookEvent::PostToolUseFailure {
                    tool_name: "Bash".into(),
                    tool_input: json!({}),
                    error: "boom".into(),
                    tool_use_id: ToolUseId::new(),
                    duration_ms: None,
                },
                "PostToolUseFailure",
            ),
            (
                HookEvent::SessionEnd {
                    session_id: lingxi_core::types::SessionId::nil(),
                    reason: "logout".into(),
                },
                "SessionEnd",
            ),
            (
                HookEvent::PreCompact {
                    reason: "manual".into(),
                    custom_instructions: None,
                },
                "PreCompact",
            ),
            (
                HookEvent::PostCompact {
                    summary: "s".into(),
                    tokens_freed: 0,
                    trigger: "auto".into(),
                },
                "PostCompact",
            ),
            (
                HookEvent::Notification {
                    message: "m".into(),
                    kind: "info".into(),
                },
                "Notification",
            ),
            (
                HookEvent::PermissionRequest {
                    tool_name: "Bash".into(),
                    tool_input: json!({}),
                    reason: "r".into(),
                },
                "PermissionRequest",
            ),
            (
                HookEvent::PermissionDenied {
                    tool_name: "Bash".into(),
                    tool_input: json!({ "command": "git push" }),
                    tool_use_id: lingxi_core::types::ToolUseId::new(),
                    reason: "denied".into(),
                },
                "PermissionDenied",
            ),
            (
                HookEvent::Setup {
                    trigger: "init".into(),
                },
                "Setup",
            ),
            (
                HookEvent::SubagentStart {
                    agent_id: lingxi_core::types::AgentId::new(),
                    agent_type: "general-purpose".into(),
                    parent_agent_id: None,
                },
                "SubagentStart",
            ),
            (
                HookEvent::CwdChanged {
                    old: std::path::PathBuf::from("/o"),
                    new: std::path::PathBuf::from("/n"),
                },
                "CwdChanged",
            ),
            (
                HookEvent::FileChanged {
                    path: std::path::PathBuf::from("/f"),
                    kind: "change".into(),
                },
                "FileChanged",
            ),
            (
                HookEvent::WorktreeRemove {
                    path: std::path::PathBuf::from("/w"),
                },
                "WorktreeRemove",
            ),
            // Deferred-completion batch.
            (
                HookEvent::ConfigChange {
                    source: crate::events::ConfigChangeSource::UserSettings,
                    file_path: None,
                },
                "ConfigChange",
            ),
            (
                HookEvent::InstructionsLoaded {
                    file_path: std::path::PathBuf::from("/work/LINGXI.md"),
                    memory_type: crate::events::InstructionsMemoryType::User,
                    load_reason: crate::events::InstructionsLoadReason::Include,
                    globs: None,
                    trigger_file_path: None,
                    parent_file_path: None,
                },
                "InstructionsLoaded",
            ),
            (
                HookEvent::Elicitation {
                    server_name: "srv".into(),
                    message: "m".into(),
                    mode: None,
                    url: None,
                    elicitation_id: None,
                    requested_schema: None,
                },
                "Elicitation",
            ),
            (
                HookEvent::WorktreeCreate {
                    name: "feat".into(),
                    path: std::path::PathBuf::from("/w"),
                    branch: "feat".into(),
                },
                "WorktreeCreate",
            ),
            // [P0] parity-fix: ElicitationResult (binary-confirmed at BIN off
            // ~201751493; action extracted from result JSON, defaults to "cancel").
            (
                HookEvent::ElicitationResult {
                    server_name: "srv".into(),
                    elicitation_id: None,
                    mode: None,
                    result: json!({"action": "accept", "content": {"token": "xyz"}}),
                },
                "ElicitationResult",
            ),
        ];
        for (ev, expected) in cases {
            let (marker, body) = build_envelope_body(&ev, &ctx).expect("must serialize");
            assert_eq!(marker, expected);
            // The serialized body's hook_event_name must equal the marker, and
            // round-trips through parse_response without a mismatch error.
            assert!(body.contains(&format!(r#""hook_event_name":"{expected}""#)));
        }
    }

    /// hook-bg-fields: a populated `HookContext.background_tasks` /
    /// `session_crons` flows into BOTH the `Stop` and `SubagentStop` wire bodies
    /// (spread last, after `last_assistant_message`), and `Some(vec![])` emits
    /// `[]` — while a NON-Stop lifecycle event (e.g. `UserPromptSubmit`) NEVER
    /// carries the keys even when the context happens to hold them. The latter is
    /// the orchestrator's `s`-gate (only Stop/SubagentStop firings populate the
    /// snapshot); here we prove the executor's lifecycle payloads honor it
    /// structurally (only Stop/SubagentStop have the fields at all).
    #[test]
    fn stop_and_subagentstop_carry_bg_snapshot_others_omit() {
        use crate::hook_payload::{HookBackgroundTask, HookSessionCron};
        let ctx = HookContext {
            background_tasks: Some(vec![HookBackgroundTask {
                is_idle: false,
                id: "b1".into(),
                r#type: "shell".into(),
                status: "running".into(),
                description: "build".into(),
                command: Some("cargo build".into()),
                agent_type: None,
                server: None,
                tool: None,
                name: None,
            }]),
            session_crons: Some(vec![HookSessionCron {
                id: "c1".into(),
                schedule: "* * * * *".into(),
                recurring: true,
                prompt: "hi".into(),
            }]),
            ..HookContext::default()
        };

        // Stop: both arrays present, spread LAST (after last_assistant_message is
        // null / omitted) and elements in Lic/Mic key order.
        let (_, stop_body) = build_envelope_body(&HookEvent::Stop { reason: "r".into() }, &ctx)
            .expect("stop serializes");
        assert!(
            stop_body.contains(
                r#""background_tasks":[{"id":"b1","type":"shell","status":"running","description":"build","command":"cargo build"}]"#
            ),
            "Stop must carry background_tasks: {stop_body}"
        );
        assert!(
            stop_body.contains(
                r#""session_crons":[{"id":"c1","schedule":"* * * * *","recurring":true,"prompt":"hi"}]"#
            ),
            "Stop must carry session_crons: {stop_body}"
        );

        // SubagentStop: same two arrays.
        let (_, sa_body) = build_envelope_body(
            &HookEvent::SubagentStop {
                agent_id: lingxi_core::types::AgentId::new(),
                status: "completed".into(),
                agent_type: "general-purpose".into(),
            },
            &ctx,
        )
        .expect("subagentstop serializes");
        assert!(
            sa_body.contains(r#""background_tasks":[{"id":"b1""#),
            "SubagentStop bg: {sa_body}"
        );
        assert!(
            sa_body.contains(r#""session_crons":[{"id":"c1""#),
            "SubagentStop crons: {sa_body}"
        );

        // A non-Stop lifecycle event NEVER carries the keys even with a populated
        // context (the fields live only on the Stop / SubagentStop payloads).
        let (_, ups_body) =
            build_envelope_body(&HookEvent::UserPromptSubmit { prompt: "p".into() }, &ctx)
                .expect("ups serializes");
        assert!(
            !ups_body.contains("background_tasks"),
            "UserPromptSubmit must omit bg: {ups_body}"
        );
        assert!(
            !ups_body.contains("session_crons"),
            "UserPromptSubmit must omit crons: {ups_body}"
        );

        // Some(vec![]) emits `[]` (the tool-use-context-present-but-empty case).
        let empty_ctx = HookContext {
            background_tasks: Some(vec![]),
            session_crons: Some(vec![]),
            ..HookContext::default()
        };
        let (_, empty_body) =
            build_envelope_body(&HookEvent::Stop { reason: "r".into() }, &empty_ctx)
                .expect("empty stop serializes");
        assert!(
            empty_body.contains(r#""background_tasks":[]"#),
            "empty bg → []: {empty_body}"
        );
        assert!(
            empty_body.contains(r#""session_crons":[]"#),
            "empty crons → []: {empty_body}"
        );
    }

    #[test]
    fn elicitation_result_envelope_extracts_action_from_result() {
        // [P0] parity-fix: `ElicitationResult` wire payload extracts `action`
        // from the embedded `result` JSON blob (binary-confirmed schema).
        let ctx = HookContext::default();
        let ev = HookEvent::ElicitationResult {
            server_name: "my-server".into(),
            elicitation_id: Some("elicit-7".into()),
            mode: Some(crate::events::ElicitationMode::Form),
            result: json!({"action": "decline", "content": {"reason": "no"}}),
        };
        let (marker, body) = build_envelope_body(&ev, &ctx).expect("must serialize");
        assert_eq!(marker, "ElicitationResult");
        assert!(body.contains(r#""mcp_server_name":"my-server""#), "{body}");
        assert!(body.contains(r#""action":"decline""#), "{body}");
        assert!(body.contains(r#""content":{"reason":"no"}"#), "{body}");
        assert!(body.contains(r#""elicitation_id":"elicit-7""#), "{body}");
        assert!(body.contains(r#""mode":"form""#), "{body}");
    }

    #[test]
    fn elicitation_result_envelope_defaults_action_to_cancel_when_absent() {
        // When `result` JSON has no `action`, fall back to `"cancel"` (safe default).
        let ctx = HookContext::default();
        let ev = HookEvent::ElicitationResult {
            server_name: "srv".into(),
            elicitation_id: None,
            mode: None,
            result: json!({}),
        };
        let (_marker, body) = build_envelope_body(&ev, &ctx).expect("must serialize");
        assert!(body.contains(r#""action":"cancel""#), "{body}");
    }
}

// ============================================================================
// B5 — config-`async` (blocking == false) backgrounding through the executor.
// ============================================================================
#[cfg(test)]
mod async_path_tests {
    //! A non-blocking Command hook is routed to the [`AsyncHookRegistry`]
    //! instead of being awaited: `execute` returns immediately, the hook can
    //! NEVER contribute a `Block` to the aggregate, and a `blocking == true`
    //! hook still runs synchronously (the regression guard).
    use super::*;
    use crate::async_registry::AsyncHookRegistry;
    use crate::attachment::HookAttachmentSink;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::{HookEvent, HookEventType};
    use crate::response::HookDecision;
    use lingxi_core::host::sandbox::{SandboxBackend, SandboxCapability, SandboxedTag};
    use lingxi_core::host::{
        BackgroundTaskHandle, ProcessError, ProcessHandle, ProcessOutput, RuntimeError,
        RuntimeSpawner, SandboxPolicy, SandboxedCommand,
    };
    use lingxi_core::types::{HookId, ToolUseId};
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::sync::{mpsc, Notify};

    #[derive(Default)]
    struct AsyncRecordingSink {
        seen: StdMutex<Vec<serde_json::Value>>,
    }

    #[async_trait]
    impl HookAttachmentSink for AsyncRecordingSink {
        async fn record(&self, attachment: serde_json::Value) {
            self.seen.lock().unwrap().push(attachment);
        }
    }

    /// Tokio-backed runtime — the hooks crate already depends on tokio, so the
    /// background hook future can be spawned with `tokio::spawn` here.
    struct TestRuntime {
        next_id: AtomicU64,
        handles: StdMutex<HashMap<u64, tokio::task::JoinHandle<()>>>,
    }
    impl TestRuntime {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                next_id: AtomicU64::new(1),
                handles: StdMutex::new(HashMap::new()),
            })
        }
    }
    #[async_trait]
    impl RuntimeSpawner for TestRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let h = tokio::spawn(task);
            self.handles.lock().unwrap().insert(id, h);
            Ok(BackgroundTaskHandle {
                task_name: name.into(),
                task_id: id,
            })
        }
        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            if let Some(h) = self.handles.lock().unwrap().remove(&handle.task_id) {
                h.abort();
            }
            Ok(())
        }
    }

    /// `ProcessRunner` that parks on a [`Notify`] before producing its output,
    /// so a test can prove the hook is still in-flight when `execute` returns.
    struct GatedRunner {
        gate: Arc<Notify>,
        output: StdMutex<Option<ProcessOutput>>,
        ran: Arc<Notify>,
    }
    impl GatedRunner {
        fn new(gate: Arc<Notify>, ran: Arc<Notify>, output: ProcessOutput) -> Arc<Self> {
            Arc::new(Self {
                gate,
                output: StdMutex::new(Some(output)),
                ran,
            })
        }
    }
    #[async_trait]
    impl ProcessRunner for GatedRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.ran.notify_one();
            self.gate.notified().await;
            Ok(self.output.lock().unwrap().take().unwrap())
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
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

    /// Minimal sandbox that mints a `SandboxedCommand` via the external-impl seam.
    struct StubSandbox;
    #[async_trait]
    impl Sandbox for StubSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::None
        }
        fn prepare(
            &self,
            cmd: ProcessCommand,
            _policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, lingxi_core::host::SandboxError> {
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: "test".into(),
                },
            ))
        }
        fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: reason.into(),
                },
            )
        }
        async fn probe_capability(&self) -> SandboxCapability {
            SandboxCapability {
                available: true,
                reason: None,
                features: lingxi_core::host::SandboxFeatures::default(),
            }
        }
    }

    /// `HttpTransport` stub — never exercised by these Command-arm tests.
    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    fn out(stdout: &str, stderr: &str, exit_code: i32) -> ProcessOutput {
        ProcessOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            timed_out: false,
        }
    }

    /// A Command hook with the supplied `blocking` flag, subscribed to `PreToolUse`.
    fn command_hook(blocking: bool) -> HookDefinition {
        command_hook_with_priority(blocking, 0)
    }

    /// As [`command_hook`] but with an explicit `priority` so a test can pin the
    /// firing order of a mixed (async + blocking) set deterministically.
    fn command_hook_with_priority(blocking: bool, priority: i32) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "async-cmd".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Command {
                command: "hook.sh".into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                shell: None,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking,
            timeout: None,
            priority,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    /// A `ProcessRunner` that returns a pre-canned [`ProcessOutput`] immediately
    /// (no gate) and records how many times it was invoked. Used by the mixed
    /// test where both the async and the blocking hook share one runner: we only
    /// need to assert the aggregate, not park either run.
    struct CountingRunner {
        output: StdMutex<ProcessOutput>,
        runs: AtomicU64,
        recorded_env: StdMutex<Vec<HashMap<String, String>>>,
    }
    impl CountingRunner {
        fn new(output: ProcessOutput) -> Arc<Self> {
            Arc::new(Self {
                output: StdMutex::new(output),
                runs: AtomicU64::new(0),
                recorded_env: StdMutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl ProcessRunner for CountingRunner {
        async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.recorded_env
                .lock()
                .unwrap()
                .push(cmd.inner().env.clone());
            Ok(self.output.lock().unwrap().clone())
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
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

    fn pre_event() -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_use_id: ToolUseId::new(),
        }
    }

    /// (1) A `blocking == false` Command hook does NOT block the aggregate:
    /// even though the hook would exit 2 (a Block in the sync path), `execute`
    /// returns immediately with no decision while the hook is still parked.
    #[tokio::test]
    async fn non_blocking_hook_never_blocks_aggregate() {
        let runtime = TestRuntime::new();
        let (tx, _rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));

        let gate = Arc::new(Notify::new());
        let ran = Arc::new(Notify::new());
        // exit 2 ⇒ would BLOCK on the synchronous path.
        let runner = GatedRunner::new(gate.clone(), ran.clone(), out("", "denied", 2));

        let hook = command_hook(false);
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            // The executor's own `runtime` field is unused on the async path
            // (the registry owns spawning); a second TestRuntime satisfies the
            // constructor.
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox))
        .with_async_registry(async_reg.clone());

        // `execute` must return WITHOUT awaiting the (still-parked) hook.
        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(
            agg.decision, None,
            "a backgrounded hook can never contribute a Block decision"
        );
        assert!(
            agg.all_results.is_empty(),
            "backgrounded hooks are excluded from the aggregate entirely"
        );

        // The hook is genuinely backgrounded and in-flight (its run() is parked
        // on the gate). Wait until the runner has actually begun.
        ran.notified().await;
        assert!(
            async_reg.is_in_flight(hook_id).await,
            "the backgrounded hook must be tracked in-flight"
        );

        // Let it finish so the test runtime doesn't leak the task.
        gate.notify_one();
    }

    /// A config-non-blocking hook returns before completion, so the completed
    /// run is retained through the transcript sink rather than the already
    /// returned aggregate.
    #[tokio::test]
    async fn non_blocking_completion_persists_real_attachment() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));
        let sink = Arc::new(AsyncRecordingSink::default());
        let runner = CountingRunner::new(out("async complete\n", "", 0));

        let hook = command_hook(false);
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox))
        .with_async_registry(async_reg)
        .with_attachment_sink(sink.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert!(agg.hook_attachments.is_empty());

        let (got_id, got) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("completion must publish before timeout")
            .expect("completion channel stays open");
        assert_eq!(got_id, hook_id);
        assert_eq!(got.stdout, "async complete\n");
        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["type"], "hook_success");
        assert_eq!(seen[0]["content"], "async complete");
    }

    #[tokio::test]
    async fn non_blocking_hook_preserves_traceparent_for_deferred_command_run() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));
        let runner = CountingRunner::new(out("async complete\n", "", 0));

        let hook = command_hook(false);
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox))
        .with_async_registry(async_reg);

        let agg = exec
            .execute(
                pre_event(),
                HookContext {
                    trace_context: Some(telemetry::otel::SerializedTraceContext {
                        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                            .into(),
                        tracestate: Some("foo=bar".into()),
                    }),
                    ..Default::default()
                },
            )
            .await;
        assert!(agg.hook_attachments.is_empty());

        let (got_id, _got) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("completion must publish before timeout")
            .expect("completion channel stays open");
        assert_eq!(got_id, hook_id);
        let envs = runner.recorded_env.lock().unwrap().clone();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].get("TRACEPARENT").map(String::as_str),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
    }

    /// A registry timeout cancels the work future, so attachment persistence
    /// must run from the registry's terminal-result finalizer.
    #[tokio::test]
    async fn non_blocking_timeout_persists_cancelled_attachment() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));
        let sink = Arc::new(AsyncRecordingSink::default());
        let gate = Arc::new(Notify::new());
        let ran = Arc::new(Notify::new());
        let runner = GatedRunner::new(gate, ran.clone(), out("never", "", 0));

        let mut hook = command_hook(false);
        hook.async_timeout = Some(Duration::from_millis(10));
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox))
        .with_async_registry(async_reg)
        .with_attachment_sink(sink.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert!(agg.hook_attachments.is_empty());
        ran.notified().await;

        let (got_id, got) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout result must publish")
            .expect("completion channel stays open");
        assert_eq!(got_id, hook_id);
        assert!(matches!(got.outcome, HookOutcome::Timeout));
        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["type"], "hook_cancelled");
        assert_eq!(seen[0]["timedOut"], true);
        assert_eq!(seen[0]["timeoutMs"], 10);
    }

    /// (2) + (3) The registry records the in-flight handle and publishes the
    /// eventual result on `completion_tx`; here the hook completes normally.
    #[tokio::test]
    async fn non_blocking_hook_publishes_completion() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));

        let gate = Arc::new(Notify::new());
        let ran = Arc::new(Notify::new());
        let runner = GatedRunner::new(gate.clone(), ran.clone(), out("ok", "", 0));

        let hook = command_hook(false);
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox))
        .with_async_registry(async_reg.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert!(agg.all_results.is_empty());

        ran.notified().await;
        assert!(async_reg.is_in_flight(hook_id).await);

        // Release the hook; its result must land on completion_tx keyed by id.
        gate.notify_one();
        let (got_id, got) = rx.recv().await.expect("completion must publish");
        assert_eq!(got_id, hook_id);
        assert!(matches!(got.outcome, HookOutcome::Success));
        assert_eq!(got.exit_code, Some(0));
    }

    #[tokio::test]
    async fn successful_non_blocking_once_hook_is_removed_after_completion() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));
        let runner = CountingRunner::new(out("ok", "", 0));

        let mut hook = command_hook(false);
        hook.once = true;
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let registry = Arc::new(RwLock::new(registry));
        let exec =
            HookExecutorImpl::new(registry.clone(), Arc::new(UnusedHttp), TestRuntime::new())
                .with_process_runner(runner.clone(), Arc::new(StubSandbox))
                .with_async_registry(async_reg);

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert!(agg.all_results.is_empty());
        let (got_id, got) = rx.recv().await.expect("completion must publish");
        assert_eq!(got_id, hook_id);
        assert!(matches!(got.outcome, HookOutcome::Success));
        assert!(
            registry.read().await.all_hooks().is_empty(),
            "a successful background once hook must be removed",
        );

        let second = exec.execute(pre_event(), HookContext::default()).await;
        assert!(second.all_results.is_empty());
        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            1,
            "the removed background once hook must not fire again",
        );
    }

    #[tokio::test]
    async fn successful_non_blocking_once_hook_is_removed_without_async_registry() {
        let runner = CountingRunner::new(out("ok", "", 0));
        let mut hook = command_hook(false);
        hook.once = true;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let registry = Arc::new(RwLock::new(registry));
        let exec =
            HookExecutorImpl::new(registry.clone(), Arc::new(UnusedHttp), TestRuntime::new())
                .with_process_runner(runner.clone(), Arc::new(StubSandbox));

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert!(agg.all_results.is_empty());
        assert!(
            registry.read().await.all_hooks().is_empty(),
            "the inline fallback must preserve once semantics",
        );

        let second = exec.execute(pre_event(), HookContext::default()).await;
        assert!(second.all_results.is_empty());
        assert_eq!(runner.runs.load(Ordering::SeqCst), 1);
    }

    /// (4) Regression guard: a `blocking == true` Command hook still runs
    /// SYNCHRONOUSLY — `execute` awaits it and its exit-2 Block is reflected in
    /// the aggregate exactly as before B5. (No async registry is even wired.)
    #[tokio::test]
    async fn blocking_hook_runs_synchronously() {
        let gate = Arc::new(Notify::new());
        let ran = Arc::new(Notify::new());
        // Pre-open the gate so the synchronous run() does not park.
        gate.notify_one();
        let runner = GatedRunner::new(gate, ran, out("", "policy violation", 2));

        let mut registry = HookRegistry::new();
        registry.register(command_hook(true));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox))
        .with_async_registry(Arc::new(AsyncHookRegistry::new(
            TestRuntime::new(),
            mpsc::channel(1).0,
        )));

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        // Synchronous: exit 2 ⇒ Block surfaces in the aggregate, result recorded.
        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("[hook.sh]: policy violation"));
        assert_eq!(
            agg.all_results.len(),
            1,
            "blocking hook IS in the aggregate"
        );
    }

    /// (5) Mixed: one async (`blocking == false`) + one blocking hook fire for
    /// the same event. ONLY the blocking hook contributes to the aggregate.
    ///
    /// The async hook is given the HIGHER priority so it is evaluated FIRST
    /// (`match_event` sorts priority-descending). Even though it would exit 2 —
    /// a `Block` on the synchronous path — it is backgrounded and excluded, so
    /// the aggregate's eventual `Block` comes solely from the lower-priority
    /// blocking hook. Both share one immediate runner: it is invoked exactly
    /// twice (once inline for the blocking hook, once in the background for the
    /// async hook), proving the async hook still runs — just not in the
    /// aggregate.
    #[tokio::test]
    async fn mixed_async_and_blocking_only_blocking_contributes() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));

        // exit 2 on BOTH: if the async hook could contribute, the aggregate
        // would still Block — but with the WRONG reason. We assert the reason is
        // the blocking hook's, proving the async exit-2 was never folded in.
        let runner = CountingRunner::new(out("", "blocking-reason", 2));

        // Async hook: HIGHER priority ⇒ evaluated first ⇒ backgrounded, excluded.
        let async_hook = command_hook_with_priority(false, 100);
        let async_id = async_hook.id;
        // Blocking hook: LOWER priority ⇒ evaluated second ⇒ its Block is the
        // aggregate's only decision.
        let blocking_hook = command_hook_with_priority(true, 0);
        let blocking_id = blocking_hook.id;

        let mut registry = HookRegistry::new();
        registry.register(async_hook);
        registry.register(blocking_hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox))
        .with_async_registry(async_reg.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        // The aggregate Block comes ONLY from the blocking hook.
        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(
            agg.reason.as_deref(),
            Some("[hook.sh]: blocking-reason"),
            "the Block reason must be the blocking hook's, not the backgrounded one's",
        );
        assert_eq!(
            agg.all_results.len(),
            1,
            "only the blocking hook is recorded in the aggregate",
        );
        assert_eq!(
            agg.all_results[0].0, blocking_id,
            "the single aggregate result is the blocking hook's",
        );

        // The async hook still RAN (fire-and-forget) — its completion lands on
        // the channel keyed by its own id, and it was never in the aggregate.
        let (got_id, _got) = rx.recv().await.expect("async hook completion publishes");
        assert_eq!(
            got_id, async_id,
            "the backgrounded completion is the async hook's, separate from the aggregate",
        );

        // Both hooks executed exactly once (inline blocking + background async).
        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            2,
            "both the blocking (inline) and async (background) hooks ran",
        );
    }

    // ---- P2-09: runtime `{"async":true}` marker fold-back --------------------

    /// A `ProcessRunner` whose `run_hook_with_async_detection` BACKGROUNDS the
    /// hook (mirroring the posix runner when it sees the `{"async":true}` marker
    /// on the child's first stdout line): it hands back a
    /// [`lingxi_core::host::HookRunOutcome::Backgrounded`] with an eventual-output handle
    /// pre-loaded with `eventual`. Its plain `run` is never taken on this path.
    struct MarkerBackgroundingRunner {
        eventual: StdMutex<Option<ProcessOutput>>,
        async_timeout: Duration,
    }
    impl MarkerBackgroundingRunner {
        fn new(eventual: ProcessOutput, async_timeout: Duration) -> Arc<Self> {
            Arc::new(Self {
                eventual: StdMutex::new(Some(eventual)),
                async_timeout,
            })
        }
    }
    #[async_trait]
    impl ProcessRunner for MarkerBackgroundingRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn run_hook_with_async_detection(
            &self,
            _cmd: &SandboxedCommand,
            _default_async_timeout: Duration,
        ) -> Result<lingxi_core::host::HookRunOutcome, ProcessError> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            // Deliver the eventual (post-marker) output immediately, as a real
            // detached drain would once the child finished.
            let _ = tx.send(self.eventual.lock().unwrap().take().expect("one call"));
            Ok(lingxi_core::host::HookRunOutcome::Backgrounded {
                async_timeout: self.async_timeout,
                output: Some(rx),
            })
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
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

    /// Runtime-marker runner whose detached output is released explicitly,
    /// allowing the test to observe the interval after the originating hook
    /// dispatch returns but before the real child completes.
    struct DeferredMarkerRunner {
        sender: StdMutex<Option<tokio::sync::oneshot::Sender<ProcessOutput>>>,
        async_timeout: Duration,
    }

    impl DeferredMarkerRunner {
        fn new(async_timeout: Duration) -> Arc<Self> {
            Arc::new(Self {
                sender: StdMutex::new(None),
                async_timeout,
            })
        }

        fn release(&self, output: ProcessOutput) {
            self.sender
                .lock()
                .unwrap()
                .take()
                .expect("runtime marker registered its output channel")
                .send(output)
                .expect("detached output receiver remains live");
        }
    }

    #[async_trait]
    impl ProcessRunner for DeferredMarkerRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            Err(ProcessError::Unsupported)
        }

        async fn run_hook_with_async_detection(
            &self,
            _cmd: &SandboxedCommand,
            _default_async_timeout: Duration,
        ) -> Result<lingxi_core::host::HookRunOutcome, ProcessError> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *self.sender.lock().unwrap() = Some(tx);
            Ok(lingxi_core::host::HookRunOutcome::Backgrounded {
                async_timeout: self.async_timeout,
                output: Some(rx),
            })
        }

        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
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

    #[derive(Default)]
    struct AsyncProgressObserver {
        events: StdMutex<Vec<(bool, String)>>,
    }

    #[async_trait]
    impl lingxi_core::host::OutputStream for AsyncProgressObserver {
        async fn emit_text(&self, _text: &str) {}

        async fn emit_tool_call(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _input: &serde_json::Value,
        ) {
        }

        async fn emit_tool_result(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _model_text: &str,
            _result: &serde_json::Value,
        ) {
        }

        async fn emit_end_turn(&self, _stop_reason: &str, _cost: &lingxi_core::host::CostSnapshot) {
        }

        async fn emit_hook_progress_started(
            &self,
            progress_id: &str,
            _hook_name: &str,
            _hook_event: &str,
            _status_message: Option<&str>,
        ) {
            self.events
                .lock()
                .unwrap()
                .push((true, progress_id.to_string()));
        }

        async fn emit_hook_progress_finished(&self, progress_id: &str) {
            self.events
                .lock()
                .unwrap()
                .push((false, progress_id.to_string()));
        }
    }

    /// A Command hook that prints the runtime `{"async":true}` marker contributes
    /// NO synchronous decision (it never blocks the turn), and its eventual
    /// post-marker output folds back on the async registry's completion channel —
    /// mapped through the command-hook contract — so the orchestrator can re-inject
    /// it as an `async_hook_response` on a later turn (claude-code
    /// `registerPendingAsyncHook`). P2-09.
    #[tokio::test]
    async fn runtime_marker_folds_eventual_output_back() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));
        let sink = Arc::new(AsyncRecordingSink::default());

        // Eventual (post-marker) output: JSON `additionalContext` the fold-back
        // parses through `map_command_output`.
        let eventual = out(
            "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"async done\"}}",
            "",
            0,
        );
        let runner = MarkerBackgroundingRunner::new(eventual, Duration::from_secs(30));

        // A BLOCKING hook drives the SYNCHRONOUS Command arm (where the marker is
        // detected). Even so, an async-marker hook must not gate the turn.
        let hook = command_hook(true);
        let hook_id = hook.id;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox))
        .with_async_registry(async_reg.clone())
        .with_attachment_sink(sink.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        // No synchronous decision, no stdout leak — the marker path is a no-op turn.
        assert_eq!(
            agg.decision, None,
            "an async-marker hook contributes no synchronous decision"
        );
        assert!(
            agg.hook_attachments.is_empty(),
            "the marker placeholder is not a completed hook run"
        );

        // The eventual output folds back on completion_tx, keyed by hook id and
        // mapped through the command-hook contract.
        let (got_id, got) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("fold-back must publish before the timeout")
            .expect("completion channel stays open");
        assert_eq!(got_id, hook_id);
        assert!(matches!(got.outcome, HookOutcome::Success));
        assert_eq!(
            got.response
                .as_ref()
                .and_then(|r| r.additional_context.as_deref()),
            Some("async done"),
            "the eventual additionalContext must survive the fold-back mapping"
        );
        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "only the eventual completion is persisted");
        assert_eq!(seen[0]["type"], "hook_success");
        assert_eq!(seen[0]["content"], "");
        assert_eq!(seen[0]["stdout"], got.stdout);
        assert_eq!(seen[0]["exitCode"], 0);
    }

    #[tokio::test]
    async fn runtime_marker_keeps_live_progress_until_detached_child_completes() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let async_reg = Arc::new(AsyncHookRegistry::new(runtime, tx));
        let runner = DeferredMarkerRunner::new(Duration::from_secs(30));
        let observer = Arc::new(AsyncProgressObserver::default());

        let mut registry = HookRegistry::new();
        registry.register(command_hook(true));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox))
        .with_async_registry(async_reg)
        .with_hook_observer(observer.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(agg.decision, None);
        {
            let events = observer.events.lock().unwrap();
            assert_eq!(events.len(), 1, "only the progress start is visible");
            assert!(events[0].0);
        }

        runner.release(out("done", "", 0));
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("runtime completion must publish")
            .expect("completion channel remains open");

        let events = observer.events.lock().unwrap();
        assert_eq!(events.len(), 2, "completion closes the live progress row");
        assert!(!events[1].0);
        assert_eq!(events[0].1, events[1].1, "the same progress run is closed");
    }

    /// When no async registry is wired, the runtime-marker path degrades to a
    /// no-op synchronous decision (the hook still ran in the runner, but there is
    /// nowhere to fold its eventual output back) — never a Block, never a panic.
    #[tokio::test]
    async fn runtime_marker_without_registry_is_a_noop() {
        let runner = MarkerBackgroundingRunner::new(out("ignored", "", 0), Duration::from_secs(30));
        let mut registry = HookRegistry::new();
        registry.register(command_hook(true));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner, Arc::new(StubSandbox));
        // No `.with_async_registry(...)`.

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(agg.decision, None, "marker path never blocks the turn");
    }

    // ---- #45b: no first-Block short-circuit + sticky Block; #41 runner gate -

    /// A `ProcessRunner` that returns a different pre-canned [`ProcessOutput`]
    /// per invocation (FIFO), so a multi-hook `execute` can be observed firing
    /// EVERY matched hook (no first-`Block` short-circuit — #45b).
    struct SequenceRunner {
        outputs: StdMutex<std::collections::VecDeque<ProcessOutput>>,
        runs: AtomicU64,
    }
    impl SequenceRunner {
        fn new(outputs: Vec<ProcessOutput>) -> Arc<Self> {
            Arc::new(Self {
                outputs: StdMutex::new(outputs.into_iter().collect()),
                runs: AtomicU64::new(0),
            })
        }
    }
    #[async_trait]
    impl ProcessRunner for SequenceRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let next = self.outputs.lock().unwrap().pop_front();
            Ok(next.unwrap_or_else(|| out("", "", 0)))
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
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

    /// #45b: `execute` dispatches EVERY matched blocking hook even after one
    /// returns `Block`. The first hook blocks (exit 2); the second still runs and
    /// its `systemMessage` side effect is folded into the aggregate — which the
    /// old first-`Block` `break` would have silently dropped. The aggregate
    /// verdict stays `Block` (sticky), reason frozen at the first blocker.
    #[tokio::test]
    async fn execute_dispatches_all_hooks_after_block_and_keeps_side_effects() {
        let runner = SequenceRunner::new(vec![
            out("", "first blocker", 2),
            out(r#"{"systemMessage":"second ran"}"#, "", 0),
        ]);
        // Higher priority fires first (deterministic order).
        let mut registry = HookRegistry::new();
        registry.register(command_hook_with_priority(true, 10));
        registry.register(command_hook_with_priority(true, 0));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox));

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "both hooks must run");
        assert_eq!(agg.all_results.len(), 2, "both results folded");
        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("[hook.sh]: first blocker"));
        assert!(
            agg.system_messages.iter().any(|m| m == "second ran"),
            "the later hook's systemMessage must survive the earlier Block: {:?}",
            agg.system_messages,
        );
    }

    /// #45b: a LATER non-Block hook must NOT overwrite an earlier Block verdict
    /// (the OR-fold equivalent of `some(blocked)`).
    #[tokio::test]
    async fn block_is_sticky_against_a_later_allowing_hook() {
        let runner = SequenceRunner::new(vec![
            out("", "blocked", 2),
            out(r#"{"decision":"approve"}"#, "", 0),
        ]);
        let mut registry = HookRegistry::new();
        registry.register(command_hook_with_priority(true, 10));
        registry.register(command_hook_with_priority(true, 0));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox));

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(runner.runs.load(Ordering::SeqCst), 2, "both hooks must run");
        assert_eq!(
            agg.decision,
            Some(HookDecision::Block),
            "an earlier Block is sticky — a later approve cannot un-block",
        );
    }

    /// #41 runner-head gate (`h$`): when `policy_disable_all_hooks` is set, NO
    /// hook is dispatched and the default (empty) aggregate is returned.
    #[tokio::test]
    async fn policy_disable_all_hooks_skips_every_hook_at_runner_head() {
        let runner = SequenceRunner::new(vec![out("", "blocked", 2)]);
        let mut registry = HookRegistry::new();
        registry.register(command_hook_with_priority(true, 0));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox))
        .with_policy_disable_all_hooks(true);

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            0,
            "disableAllHooks must skip dispatch entirely",
        );
        assert_eq!(
            agg.decision, None,
            "skipped batch yields the default aggregate"
        );
        assert!(agg.all_results.is_empty());
    }

    /// #41: with the gate OFF (the default no-policy path) hooks fire normally,
    /// so the gate is behavior-neutral until a policy is wired.
    #[tokio::test]
    async fn policy_disable_all_hooks_default_off_fires_hooks() {
        let runner = SequenceRunner::new(vec![out("", "blocked", 2)]);
        let mut registry = HookRegistry::new();
        registry.register(command_hook_with_priority(true, 0));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            TestRuntime::new(),
        )
        .with_process_runner(runner.clone(), Arc::new(StubSandbox));

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(
            runner.runs.load(Ordering::SeqCst),
            1,
            "gate off ⇒ hook fires"
        );
        assert_eq!(agg.decision, Some(HookDecision::Block));
    }
}

#[cfg(test)]
mod once_and_status_message_tests {
    //! RUNTIME wiring for the additive `once` / `status_message` hook fields.
    //!
    //! * `once: true` — a hook is dropped from the registry after it runs with
    //!   a *success* outcome (claude-code `registerSkillHooks.ts:35-36` +
    //!   `utils/hooks.ts:2918-2919`): the second dispatch finds nothing to run.
    //!   A `once` hook whose first run ERRORS is left in place — TS guards
    //!   `onHookSuccess` behind `result.outcome === 'success'`.
    //! * `status_message` — threaded onto the per-hook `hook_progress` event the
    //!   executor emits before each hook (claude-code `utils/hooks.ts:2094-2116`).
    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::{HookEvent, HookEventType};
    use lingxi_core::types::{HookId, ToolUseId};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct ProgressObserver {
        events: Mutex<Vec<(bool, String, Option<String>)>>,
    }

    #[async_trait]
    impl lingxi_core::host::OutputStream for ProgressObserver {
        async fn emit_text(&self, _text: &str) {}

        async fn emit_tool_call(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _input: &serde_json::Value,
        ) {
        }

        async fn emit_tool_result(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _model_text: &str,
            _result: &serde_json::Value,
        ) {
        }

        async fn emit_end_turn(&self, _stop_reason: &str, _cost: &lingxi_core::host::CostSnapshot) {
        }

        async fn emit_hook_progress_started(
            &self,
            progress_id: &str,
            _hook_name: &str,
            _hook_event: &str,
            status_message: Option<&str>,
        ) {
            self.events.lock().unwrap().push((
                true,
                progress_id.to_string(),
                status_message.map(str::to_owned),
            ));
        }

        async fn emit_hook_progress_finished(&self, progress_id: &str) {
            self.events
                .lock()
                .unwrap()
                .push((false, progress_id.to_string(), None));
        }
    }

    /// `HttpTransport` stub — never exercised by these Builtin-arm tests.
    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    /// `RuntimeSpawner` stub — backgrounding is never exercised here.
    struct UnusedRuntime;
    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    /// A builtin handler that counts invocations and returns a fixed outcome,
    /// so a test can prove a `once` hook ran exactly N times.
    struct CountingBuiltin {
        id: String,
        runs: Arc<AtomicU32>,
        outcome: HookOutcome,
    }
    #[async_trait]
    impl BuiltinHookHandler for CountingBuiltin {
        async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let exit_code = i32::from(!matches!(self.outcome, HookOutcome::Success));
            HookResult {
                outcome: self.outcome,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(exit_code),
                response: None,
            }
        }
        fn id(&self) -> &str {
            &self.id
        }
    }

    fn pre_event() -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_use_id: ToolUseId::new(),
        }
    }

    /// A Builtin hook subscribed to `PreToolUse`, with the supplied `once` flag,
    /// `status_message`, and handler id.
    fn builtin_hook(handler_id: &str, once: bool, status_message: Option<&str>) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "once-hook".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: handler_id.into(),
            },
            source: HookSource::Skill,
            blocking: true,
            timeout: None,
            priority: 0,
            once,
            status_message: status_message.map(Into::into),
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    fn executor_with(
        hook: HookDefinition,
        handler: Arc<dyn BuiltinHookHandler>,
    ) -> (HookExecutorImpl, Arc<RwLock<HookRegistry>>) {
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let reg = Arc::new(RwLock::new(registry));
        let mut exec =
            HookExecutorImpl::new(reg.clone(), Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        exec.register_builtin(handler);
        (exec, reg)
    }

    /// A `once: true` hook fires on the first dispatch and is then removed from
    /// the registry, so a second dispatch runs nothing.
    #[tokio::test]
    async fn once_hook_fires_once_then_is_gone() {
        let runs = Arc::new(AtomicU32::new(0));
        let handler = Arc::new(CountingBuiltin {
            id: "once-success".into(),
            runs: runs.clone(),
            outcome: HookOutcome::Success,
        });
        let (exec, reg) = executor_with(builtin_hook("once-success", true, None), handler);

        // First dispatch: the hook runs and its result is in the aggregate.
        let agg1 = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(agg1.all_results.len(), 1, "first dispatch runs the hook");
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // It was removed from the registry after the successful run.
        assert!(
            reg.read().await.all_hooks().is_empty(),
            "a once hook is dropped from the registry after success",
        );

        // Second dispatch: nothing matches, nothing runs.
        let agg2 = exec.execute(pre_event(), HookContext::default()).await;
        assert!(
            agg2.all_results.is_empty(),
            "the removed once hook does not fire a second time",
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the handler ran exactly once across both dispatches",
        );
    }

    /// A `once: true` hook whose run ERRORS is NOT removed — it stays in the
    /// registry and runs again on the next dispatch (TS guards removal behind a
    /// `success` outcome).
    #[tokio::test]
    async fn once_hook_that_errors_is_not_removed() {
        let runs = Arc::new(AtomicU32::new(0));
        let handler = Arc::new(CountingBuiltin {
            id: "once-error".into(),
            runs: runs.clone(),
            outcome: HookOutcome::Error,
        });
        let (exec, reg) = executor_with(builtin_hook("once-error", true, None), handler);

        let _ = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            reg.read().await.all_hooks().len(),
            1,
            "an erroring once hook is left in the registry",
        );

        // Second dispatch still finds and runs the hook.
        let _ = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the un-removed once hook fires again after an error",
        );
    }

    /// #9: `execute_agent_scoped` fires ONLY the named agent's frontmatter
    /// SubagentStop hooks — a session-level SubagentStop hook is excluded (the
    /// orchestrator chokepoint owns those), so the in-child fire can't double-run.
    #[tokio::test]
    async fn execute_agent_scoped_fires_only_agent_frontmatter() {
        let agent = lingxi_core::types::AgentId::new();
        let agent_runs = Arc::new(AtomicU32::new(0));
        let session_runs = Arc::new(AtomicU32::new(0));

        let mut registry = HookRegistry::new();
        // Agent frontmatter Stop hook → retargeted to SubagentStop (isAgent=true).
        let mut fm = builtin_hook("agent-stop", false, None);
        fm.events = vec![HookEventType::Stop];
        fm.name = "agent-stop".into();
        registry.register_agent_hooks(agent, &[fm], true);
        // Session-level SubagentStop hook (must NOT fire on the scoped call).
        let mut sess = builtin_hook("session-stop", false, None);
        sess.events = vec![HookEventType::SubagentStop];
        sess.name = "session-stop".into();
        sess.source = HookSource::Settings(lingxi_core::types::SettingsScope::User);
        registry.register(sess);

        let reg = Arc::new(RwLock::new(registry));
        let mut exec =
            HookExecutorImpl::new(reg.clone(), Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        exec.register_builtin(Arc::new(CountingBuiltin {
            id: "agent-stop".into(),
            runs: agent_runs.clone(),
            outcome: HookOutcome::Success,
        }));
        exec.register_builtin(Arc::new(CountingBuiltin {
            id: "session-stop".into(),
            runs: session_runs.clone(),
            outcome: HookOutcome::Success,
        }));

        let ev = HookEvent::SubagentStop {
            agent_id: agent,
            status: "completed".into(),
            agent_type: String::new(),
        };
        exec.execute_agent_scoped(ev, HookContext::default(), agent)
            .await;

        assert_eq!(
            agent_runs.load(Ordering::SeqCst),
            1,
            "the agent's frontmatter SubagentStop fires"
        );
        assert_eq!(
            session_runs.load(Ordering::SeqCst),
            0,
            "the session-level SubagentStop is NOT fired by the scoped call"
        );
    }

    /// A `PostToolUse` Builtin hook returning `updated_mcp_tool_output` has it
    /// folded into the aggregate's `updated_mcp_tool_output` by `merge`.
    #[tokio::test]
    async fn post_hook_updated_mcp_tool_output_reaches_aggregate() {
        struct RewriteBuiltin;
        #[async_trait]
        impl BuiltinHookHandler for RewriteBuiltin {
            fn id(&self) -> &str {
                "rewrite"
            }
            async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
                HookResult {
                    outcome: HookOutcome::Success,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                    response: Some(HookResponse {
                        updated_mcp_tool_output: Some(
                            serde_json::json!({ "content": "rewritten" }),
                        ),
                        ..Default::default()
                    }),
                }
            }
        }
        let hook = HookDefinition {
            id: HookId::new(),
            name: "rewrite".into(),
            events: vec![HookEventType::PostToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "rewrite".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let (exec, _reg) = executor_with(hook, Arc::new(RewriteBuiltin));
        let post = HookEvent::PostToolUse {
            tool_name: "mcp__srv__tool".into(),
            tool_input: serde_json::json!({}),
            tool_output: serde_json::json!({ "content": "original" }),
            tool_use_id: ToolUseId::new(),
            duration_ms: None,
        };
        let agg = exec.execute(post, HookContext::default()).await;
        assert_eq!(
            agg.updated_mcp_tool_output,
            Some(serde_json::json!({ "content": "rewritten" })),
            "the hook's updatedMCPToolOutput must reach the aggregate",
        );
    }

    /// SH-01: a `PostToolUse` hook's `classifierContext` is capped at 2000
    /// UTF-16 units, counted into the shared budget, tagged with the hook's
    /// `pairedRewrite`, and folded onto the aggregate's OWN channel — never onto
    /// `additional_contexts` (model-facing) or `system_messages`.
    #[tokio::test]
    async fn post_hook_classifier_context_reaches_aggregate_capped() {
        struct ContextBuiltin;
        #[async_trait]
        impl BuiltinHookHandler for ContextBuiltin {
            fn id(&self) -> &str {
                "ctx"
            }
            async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
                HookResult {
                    outcome: HookOutcome::Success,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                    response: Some(HookResponse {
                        // 2500 units — 500 over the `Pfr` cap.
                        classifier_context: Some("x".repeat(2500)),
                        // Pairs with a direct output rewrite ⇒ `pairedRewrite:"direct"`.
                        updated_tool_output: Some(Some(serde_json::json!({ "x": 1 }))),
                        ..Default::default()
                    }),
                }
            }
        }
        let hook = HookDefinition {
            id: HookId::new(),
            name: "ctx".into(),
            events: vec![HookEventType::PostToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "ctx".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let (exec, _reg) = executor_with(hook, Arc::new(ContextBuiltin));
        let post = HookEvent::PostToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_output: serde_json::json!({ "content": "original" }),
            tool_use_id: ToolUseId::new(),
            duration_ms: None,
        };
        let agg = exec.execute(post, HookContext::default()).await;
        assert_eq!(agg.classifier_contexts.len(), 1);
        assert_eq!(
            agg.classifier_contexts[0].value.chars().count(),
            2000,
            "capped at Pfr = 2000 UTF-16 code units",
        );
        assert!(
            !agg.classifier_contexts[0].host_principal,
            "no port hook type is a host principal",
        );
        assert_eq!(
            agg.classifier_context_chars, 2000,
            "the shared budget counts POST-cap length",
        );
        assert_eq!(
            agg.paired_rewrite,
            Some(crate::response::PairedRewrite::Direct),
            "the same hook set updatedToolOutput ⇒ \"direct\"",
        );
        assert!(
            agg.additional_contexts.is_empty(),
            "classifierContext must NOT leak onto the model-facing channel",
        );
        assert!(agg.system_messages.is_empty());
    }

    /// #38: A `PostToolUse` Builtin hook returning `updated_tool_output` (the
    /// all-tools field) has it folded into the aggregate by `merge`. The outer
    /// `Some` is preserved (`!== void 0` semantics).
    #[tokio::test]
    async fn post_hook_updated_tool_output_reaches_aggregate() {
        struct RewriteAll;
        #[async_trait]
        impl BuiltinHookHandler for RewriteAll {
            fn id(&self) -> &str {
                "rewrite_all"
            }
            async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
                HookResult {
                    outcome: HookOutcome::Success,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                    response: Some(HookResponse {
                        updated_tool_output: Some(Some(serde_json::json!({ "x": "all" }))),
                        ..Default::default()
                    }),
                }
            }
        }
        let hook = HookDefinition {
            id: HookId::new(),
            name: "rewrite_all".into(),
            events: vec![HookEventType::PostToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "rewrite_all".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let (exec, _reg) = executor_with(hook, Arc::new(RewriteAll));
        let post = HookEvent::PostToolUse {
            // a NON-mcp tool: updated_tool_output applies for all tools
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_output: serde_json::json!("original"),
            tool_use_id: ToolUseId::new(),
            duration_ms: None,
        };
        let agg = exec.execute(post, HookContext::default()).await;
        assert_eq!(
            agg.updated_tool_output,
            Some(Some(serde_json::json!({ "x": "all" }))),
            "the hook's updatedToolOutput must reach the aggregate",
        );
    }

    /// #40: a hook returning a top-level `terminal_sequence` has it folded into
    /// the aggregate by `merge` (latest wins); the consumer validates + emits.
    #[tokio::test]
    async fn hook_terminal_sequence_reaches_aggregate() {
        struct TermHook;
        #[async_trait]
        impl BuiltinHookHandler for TermHook {
            fn id(&self) -> &str {
                "term"
            }
            async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
                HookResult {
                    outcome: HookOutcome::Success,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                    response: Some(HookResponse {
                        terminal_sequence: Some("\u{0007}".into()),
                        ..Default::default()
                    }),
                }
            }
        }
        let hook = HookDefinition {
            id: HookId::new(),
            name: "term".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "term".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let (exec, _reg) = executor_with(hook, Arc::new(TermHook));
        let pre = HookEvent::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: ToolUseId::new(),
        };
        let agg = exec.execute(pre, HookContext::default()).await;
        assert_eq!(agg.terminal_sequence.as_deref(), Some("\u{0007}"));
    }

    /// A `PostToolUse` hook that does NOT set `updated_mcp_tool_output` leaves
    /// the aggregate's field `None` (strict no-op).
    #[tokio::test]
    async fn post_hook_without_mutation_leaves_aggregate_none() {
        let runs = Arc::new(AtomicU32::new(0));
        let handler = Arc::new(CountingBuiltin {
            id: "observe".into(),
            runs: runs.clone(),
            outcome: HookOutcome::Success,
        });
        let hook = HookDefinition {
            id: HookId::new(),
            name: "observe".into(),
            events: vec![HookEventType::PostToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "observe".into(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let (exec, _reg) = executor_with(hook, handler);
        let post = HookEvent::PostToolUse {
            tool_name: "mcp__srv__tool".into(),
            tool_input: serde_json::json!({}),
            tool_output: serde_json::json!({ "content": "original" }),
            tool_use_id: ToolUseId::new(),
            duration_ms: None,
        };
        let agg = exec.execute(post, HookContext::default()).await;
        assert!(
            agg.updated_mcp_tool_output.is_none(),
            "no mutating hook → the aggregate field stays None",
        );
    }

    /// The per-hook `status_message` is carried onto the emitted `hook_progress`
    /// event; a hook with no `status_message` carries `None`.
    #[tokio::test]
    async fn status_message_is_carried_on_progress_event() {
        let runs = Arc::new(AtomicU32::new(0));
        let handler = Arc::new(CountingBuiltin {
            id: "with-status".into(),
            runs,
            outcome: HookOutcome::Success,
        });
        let (exec, _reg) = executor_with(
            builtin_hook("with-status", false, Some("Formatting\u{2026}")),
            handler,
        );

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(
            agg.progress.len(),
            1,
            "one progress event per matching hook"
        );
        let p = &agg.progress[0];
        assert_eq!(p.hook_event, "PreToolUse");
        assert_eq!(p.hook_name, "once-hook");
        assert_eq!(
            p.status_message.as_deref(),
            Some("Formatting\u{2026}"),
            "the per-hook status_message threads onto the progress event",
        );
    }

    #[tokio::test]
    async fn status_message_reaches_live_observer_and_is_cleared() {
        let runs = Arc::new(AtomicU32::new(0));
        let handler = Arc::new(CountingBuiltin {
            id: "with-live-status".into(),
            runs,
            outcome: HookOutcome::Success,
        });
        let (exec, _reg) = executor_with(
            builtin_hook("with-live-status", false, Some("Formatting\u{2026}")),
            handler,
        );
        let observer = Arc::new(ProgressObserver::default());
        let exec = exec.with_hook_observer(observer.clone());

        exec.execute(pre_event(), HookContext::default()).await;

        let events = observer.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].0);
        assert_eq!(events[0].2.as_deref(), Some("Formatting\u{2026}"));
        assert!(!events[1].0);
        assert_eq!(events[0].1, events[1].1, "finish clears the same run");
    }

    /// A hook with no `status_message` yields a progress event whose
    /// `status_message` is `None` (the spinner falls back to the generic line).
    #[tokio::test]
    async fn absent_status_message_is_none_on_progress_event() {
        let runs = Arc::new(AtomicU32::new(0));
        let handler = Arc::new(CountingBuiltin {
            id: "no-status".into(),
            runs,
            outcome: HookOutcome::Success,
        });
        let (exec, _reg) = executor_with(builtin_hook("no-status", false, None), handler);

        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(agg.progress.len(), 1);
        assert_eq!(agg.progress[0].status_message, None);
    }
}

// ============================================================================
// Http / Agent arm dispatch routing through the full `execute` path.
//
// The loader now emits `HookExecutor::Http` / `HookExecutor::Agent`
// definitions (loader.rs). These tests prove the executor's `dispatch` routes
// each variant to its dedicated runner — the HTTP arm reaches the injected
// `HttpTransport` and the Agent arm reaches the injected `SubagentSpawner` —
// so a settings-declared http/agent hook is actually executed end-to-end.
// ============================================================================
#[cfg(test)]
mod http_agent_dispatch_tests {
    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::{HookEvent, HookEventType};
    use crate::response::HookDecision;
    use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
    use lingxi_core::host::subagent_spawn::{
        SubagentInheritance, SubagentResult, SubagentSpawnError, SubagentSpawnRequest,
        SubagentUsage,
    };
    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolInvoker, ToolInvokerError,
    };
    use lingxi_core::types::{HookId, HttpResponse, ToolUseId};
    use serde_json::json;
    use std::sync::Mutex;

    /// `RuntimeSpawner` stub — never exercised by these tests.
    struct UnusedRuntime;
    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    /// `HttpTransport` mock that records each request and returns a canned body
    /// — so a test can prove the HTTP arm reached it with the right URL.
    struct RecordingHttp {
        recorded: Mutex<Vec<lingxi_core::types::HttpRequest>>,
        body: String,
        status: u16,
    }
    #[async_trait]
    impl HttpTransport for RecordingHttp {
        async fn request(
            &self,
            req: lingxi_core::types::HttpRequest,
        ) -> Result<HttpResponse, lingxi_core::host::HttpError> {
            self.recorded.lock().unwrap().push(req);
            Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: self.body.clone(),
                body_bytes: Vec::new(),
            })
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    fn pre_event() -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_use_id: ToolUseId::new(),
        }
    }

    /// An `Http` hook subscribed to `PreToolUse` pointing at `url`.
    fn http_hook(url: &str) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: url.into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Http {
                url: url.into(),
                method: "POST".into(),
                headers: HashMap::new(),
                allowed_env_vars: Vec::new(),
                timeout: Duration::from_secs(5),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
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
    async fn http_hook_definition_dispatches_to_http_executor() {
        let http = Arc::new(RecordingHttp {
            recorded: Mutex::new(Vec::new()),
            status: 200,
            body:
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#
                    .into(),
        });
        let mut registry = HookRegistry::new();
        // A public IP literal keeps this dispatch test independent from the
        // machine's DNS. Domain-resolution and connection pinning are covered
        // by the dedicated HTTP-executor/transport tests.
        registry.register(http_hook("https://93.184.216.34/pre"));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            http.clone(),
            Arc::new(UnusedRuntime),
        );

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        // The HTTP arm reached the transport with the hook's URL.
        let recorded = http.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1, "the Http arm must reach the transport");
        assert_eq!(recorded[0].url, "https://93.184.216.34/pre");
        drop(recorded);
        // The parsed allow response surfaces as an Approve decision.
        assert_eq!(agg.decision, Some(HookDecision::Approve));
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Success));
    }

    // ---- agent arm ---------------------------------------------------------

    struct InertInvoker;
    #[async_trait]
    impl ToolInvoker for InertInvoker {
        async fn invoke(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            Err(ToolInvokerError::Internal("inert".into()))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct InertBudget;
    #[async_trait]
    impl BudgetEnforcerHandle for InertBudget {
        async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    fn dummy_inherit() -> SubagentInheritance {
        SubagentInheritance {
            tool_invoker: Arc::new(InertInvoker),
            budget: Arc::new(InertBudget),
        }
    }

    /// `HttpTransport` stub — never exercised by the agent test.
    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    /// `SubagentSpawner` mock that records the request and returns a canned
    /// terminal result — so a test can prove the Agent arm reached it with the
    /// hook's `agent_type` + spliced prompt.
    struct RecordingSpawner {
        recorded: Mutex<Vec<SubagentSpawnRequest>>,
        result: Mutex<Option<Result<SubagentResult, SubagentSpawnError>>>,
    }
    #[async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            self.recorded.lock().unwrap().push(request);
            self.result
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(SubagentSpawnError::Internal("no script".into())))
        }
    }

    /// An `Agent` hook subscribed to `PreToolUse`.
    fn agent_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "agent".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Agent {
                agent_type: "general-purpose".into(),
                prompt: "vet this".into(),
                model: None,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
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
    async fn agent_hook_definition_dispatches_to_agent_executor() {
        let spawner = Arc::new(RecordingSpawner {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(Some(Ok(SubagentResult::Completed {
                agent_id: lingxi_core::types::AgentId::new(),
                content: json!(r#"{"decision":"approve"}"#),
                usage: SubagentUsage::default(),
                total_tool_use_count: 0,
                total_duration_ms: 0,
                total_tokens: 0,
                assistant_message_count: 0,
                response_char_count: 0,
                last_request_id: None,
                cumulative_usage: SubagentUsage::default(),
                usage_complete: true,
            }))),
        });
        let mut registry = HookRegistry::new();
        registry.register(agent_hook());
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_agent_spawner(spawner.clone());

        // The Agent arm needs the inheritance bundle on the context.
        let ctx = HookContext {
            inherit: Some(dummy_inherit()),
            ..Default::default()
        };
        let agg = exec.execute(pre_event(), ctx).await;

        // The Agent arm reached the spawner with the hook's agent_type and a
        // prompt that spliced the template + the serialized payload.
        let recorded = spawner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1, "the Agent arm must reach the spawner");
        assert_eq!(recorded[0].subagent_type, "general-purpose");
        assert!(
            recorded[0].prompt.starts_with("vet this"),
            "prompt template is spliced ahead of the payload",
        );
        assert!(
            recorded[0]
                .prompt
                .contains(r#""hook_event_name":"PreToolUse""#),
            "the serialized event payload is appended to the prompt",
        );
        drop(recorded);
        // The subagent's approve content surfaces as an Approve decision.
        assert_eq!(agg.decision, Some(HookDecision::Approve));
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Success));
    }
}

// ============================================================================
// PROMPT-ARM DISPATCH TESTS
//
// Prove the executor's `dispatch` routes a `HookExecutor::Prompt` definition
// to the injected `HookPromptRunner` with the `$ARGUMENTS`-substituted prompt,
// and that the runner's `{ok:false}` verdict surfaces as a Block decision on
// the aggregate. Also proves the no-runner path is a strict no-op.
// ============================================================================
#[cfg(test)]
mod prompt_dispatch_tests {
    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::{HookEvent, HookEventType};
    use crate::mcp_invoker::{HookMcpInvocation, HookMcpInvocationResult, HookMcpInvoker};
    use crate::prompt_executor::{HookPromptRunner, PromptHookError, PromptHookRequest};
    use crate::response::HookDecision;
    use lingxi_core::types::{HookId, SessionId, ToolUseId};
    use serde_json::json;
    use std::sync::Mutex;

    /// `RuntimeSpawner` stub — never exercised by these tests.
    struct UnusedRuntime;
    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    /// `HttpTransport` stub — never exercised by these tests.
    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }

    /// `HookPromptRunner` mock that records the request and returns a canned
    /// body — so a test can prove the Prompt arm reached it with the
    /// `$ARGUMENTS`-substituted prompt.
    struct RecordingRunner {
        recorded: Mutex<Vec<PromptHookRequest>>,
        result: Mutex<Option<Result<String, PromptHookError>>>,
    }
    #[async_trait]
    impl HookPromptRunner for RecordingRunner {
        async fn run(&self, req: PromptHookRequest) -> Result<String, PromptHookError> {
            self.recorded.lock().unwrap().push(req);
            self.result
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(PromptHookError::Query("no script".into())))
        }
    }

    struct RecordingMcpInvoker {
        recorded: Mutex<Vec<HookMcpInvocation>>,
        result: Mutex<Vec<HookMcpInvocationResult>>,
    }
    #[async_trait]
    impl HookMcpInvoker for RecordingMcpInvoker {
        async fn invoke(&self, request: HookMcpInvocation) -> HookMcpInvocationResult {
            self.recorded.lock().unwrap().push(request);
            self.result.lock().unwrap().remove(0)
        }
    }

    fn pre_event() -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf /"}),
            tool_use_id: ToolUseId::new(),
        }
    }

    fn stop_event() -> HookEvent {
        HookEvent::Stop {
            reason: "end_turn".into(),
        }
    }

    fn prompt_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "prompt".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Prompt {
                prompt: "Is this safe? $ARGUMENTS".into(),
                model: Some("claude-sonnet-4-6".into()),
                continue_on_block: false,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
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
    async fn parent_subagent_stop_consumes_only_matching_child_live_history() {
        let runner = Arc::new(RecordingRunner {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(Some(Ok(r#"{"ok":true,"reason":"child complete"}"#.into()))),
        });
        let mut registry = HookRegistry::new();
        let mut hook = prompt_hook();
        hook.events = vec![HookEventType::SubagentStop];
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_prompt_runner(runner.clone());
        let session = lingxi_core::types::SessionId::new();
        let child = lingxi_core::types::AgentId::new();
        let other = lingxi_core::types::AgentId::new();
        let snapshot = |text: &str| crate::PromptHookTranscript {
            messages: vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                text.into(),
            )],
            last_usage_tokens: 321,
            ..Default::default()
        };
        let child_snapshot = snapshot("child-only unpersisted evidence");
        exec.publish_agent_prompt_transcript(session, child, child_snapshot.clone());
        exec.publish_agent_prompt_transcript(session, other, snapshot("other child"));
        exec.execute_excluding_agent(
            HookEvent::SubagentStop {
                agent_id: child,
                status: "completed".into(),
                agent_type: "worker".into(),
            },
            HookContext {
                session_id: session,
                prompt_transcript: Some(snapshot("parent must not leak")),
                ..Default::default()
            },
            child,
        )
        .await;
        let calls = runner.recorded.lock().unwrap();
        assert_eq!(
            calls[0].transcript.as_ref().unwrap().messages,
            child_snapshot.messages
        );
        assert_eq!(calls[0].transcript.as_ref().unwrap().last_usage_tokens, 321);
        drop(calls);
        assert!(exec.take_agent_prompt_transcript(session, child).is_none());
        assert!(exec
            .take_agent_prompt_transcript(lingxi_core::types::SessionId::new(), other)
            .is_none());
        exec.clear_session_hooks(session).await;
        assert!(exec.take_agent_prompt_transcript(session, other).is_none());
    }

    #[tokio::test]
    async fn prompt_hook_definition_dispatches_to_prompt_runner() {
        let runner = Arc::new(RecordingRunner {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(Some(Ok(r#"{"ok": false, "reason": "destructive"}"#.into()))),
        });
        let mut registry = HookRegistry::new();
        registry.register(prompt_hook());
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_prompt_runner(runner.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        // The Prompt arm reached the runner with the substituted prompt + the
        // serialized event payload + the model override.
        let recorded = runner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1, "the Prompt arm must reach the runner");
        assert!(
            recorded[0].prompt.starts_with("Is this safe? "),
            "prompt template precedes the payload: {:?}",
            recorded[0].prompt,
        );
        assert!(
            recorded[0]
                .prompt
                .contains(r#""hook_event_name":"PreToolUse""#),
            "the serialized event payload is spliced into $ARGUMENTS",
        );
        assert_eq!(recorded[0].model.as_deref(), Some("claude-sonnet-4-6"));
        drop(recorded);
        // The runner's `{ok:false}` verdict surfaces as a Block decision.
        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(
            agg.reason.as_deref(),
            Some("[Is this safe? $ARGUMENTS]: destructive")
        );
        assert!(agg.prevent_continuation);
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Success));
    }

    #[tokio::test]
    async fn prompt_hook_ok_true_does_not_block() {
        let runner = Arc::new(RecordingRunner {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(Some(Ok(r#"{"ok": true}"#.into()))),
        });
        let mut registry = HookRegistry::new();
        registry.register(prompt_hook());
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_prompt_runner(runner.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None, "condition met must not block");
        assert!(!agg.prevent_continuation);
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Success));
    }

    fn function_hook(source: &str) -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "fn-hook".into(),
            events: vec![HookEventType::Stop],
            if_condition: None,
            executor: DefHookExecutor::Function {
                source: source.into(),
                budget_ms: Some(500),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
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

    async fn run_function_hook(source: &str) -> crate::AggregateHookResult {
        let mut registry = HookRegistry::new();
        registry.register(function_hook(source));
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        );
        exec.execute(
            stop_event(),
            HookContext {
                session_id: SessionId::new(),
                ..Default::default()
            },
        )
        .await
    }

    /// A function hook sees the event payload and its decision flows through
    /// the SAME aggregation every other executor arm uses — no private dialect.
    #[tokio::test]
    async fn a_function_hook_reads_the_payload_and_decides() {
        let agg = run_function_hook(
            "return {decision: input.hook_event_name === 'Stop' ? 'block' : 'approve', \
             reason: 'saw ' + input.hook_event_name};",
        )
        .await;
        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("saw Stop"));
    }

    /// 🚨 A hook that throws must produce NO decision. If a crash read as an
    /// allow, any plugin could wave a decision through by throwing — the
    /// cheapest possible bypass. Asserting the aggregate, because that is what
    /// the engine acts on.
    #[tokio::test]
    async fn a_throwing_function_hook_yields_no_decision() {
        let agg = run_function_hook("throw new Error('boom');").await;
        assert_eq!(agg.decision, None, "a throw must not decide anything");
        // ⚠️ `decision == None` ALONE is vacuous: it is equally what "the arm
        // never ran" and "errors were swallowed as null" produce. An audit
        // confirmed both breakages left the old version of this test green.
        // Pin that the hook RAN and that its outcome was an error.
        let (_, result) = agg
            .all_results
            .first()
            .expect("the function hook must actually have been dispatched");
        assert_eq!(result.outcome, HookOutcome::Error, "a throw is an error");
        assert!(result.stderr.contains("boom"), "{}", result.stderr);
    }

    /// Same for exhausting the budget: a hook cannot buy an outcome by hanging.
    #[tokio::test]
    async fn a_hanging_function_hook_yields_no_decision() {
        let started = std::time::Instant::now();
        let agg = run_function_hook("while (true) {}").await;
        assert_eq!(agg.decision, None);
        let (_, result) = agg
            .all_results
            .first()
            .expect("the function hook must actually have been dispatched");
        assert_eq!(result.outcome, HookOutcome::Error);
        assert!(
            result.stderr.contains("time budget"),
            "the failure must be the BUDGET, not some other error: {}",
            result.stderr
        );
        // It must have actually waited — a hook that returned instantly would
        // satisfy every assertion above while proving the budget never ran.
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(400),
            "the 500ms budget must have been spent; took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn prompt_hook_supports_stop_event() {
        let runner = Arc::new(RecordingRunner {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(Some(Ok(r#"{"ok": false, "reason": "not yet"}"#.into()))),
        });
        let mut hook = prompt_hook();
        hook.events = vec![HookEventType::Stop];
        hook.executor = DefHookExecutor::Prompt {
            prompt: "Is this safe? $ARGUMENTS".into(),
            model: Some("claude-sonnet-4-6".into()),
            continue_on_block: true,
        };
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_prompt_runner(runner.clone());

        let agg = exec
            .execute(
                stop_event(),
                HookContext {
                    session_id: SessionId::new(),
                    ..Default::default()
                },
            )
            .await;

        let recorded = runner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].prompt.contains(r#""hook_event_name":"Stop""#),
            "Stop payload must be serialized into the prompt"
        );
        drop(recorded);
        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert!(
            !agg.prevent_continuation,
            "continueOnBlock=true is caller-controlled"
        );
    }

    #[tokio::test]
    async fn prompt_hook_without_runner_is_strict_noop() {
        let mut registry = HookRegistry::new();
        registry.register(prompt_hook());
        // No `.with_prompt_runner(..)` — the Prompt arm must be a no-op.
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        );

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        // No decision: a no-runner prompt hook can NEVER block.
        assert_eq!(agg.decision, None);
        assert!(!agg.prevent_continuation);
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert!(r.stderr.contains("prompt executor not wired"));
    }

    /// SH-04: an `mcp_tool` settings entry now LOADS (the loader used to drop it
    /// silently) and reaches `dispatch`. With no MCP invoker wired into the
    /// hooks crate the arm must behave exactly like the Command/Prompt arms
    /// without their runner: a structured `Error`, never a `Block`, so a
    /// non-executable hook can never gate a turn.
    #[tokio::test]
    async fn mcp_tool_hook_without_invoker_is_strict_noop() {
        let mut registry = HookRegistry::new();
        registry.register(HookDefinition {
            id: HookId::new(),
            name: "linter/format_file".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::McpTool {
                server: "linter".into(),
                tool: "format_file".into(),
                input: std::collections::HashMap::new(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        );

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(
            agg.decision, None,
            "an mcp_tool hook with no invoker can never block"
        );
        assert!(!agg.prevent_continuation);
        let (_, r) = &agg.all_results[0];
        assert!(matches!(r.outcome, HookOutcome::Error));
        assert!(r.stderr.contains("mcp_tool executor not wired"));
    }

    #[tokio::test]
    async fn mcp_tool_hook_uses_text_content_as_stdout_and_interpolates_input() {
        let invoker = Arc::new(RecordingMcpInvoker {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(vec![HookMcpInvocationResult::Success {
                text_content: vec![r#"{"decision":"block","reason":"denied"}"#.into()],
            }]),
        });
        let mut registry = HookRegistry::new();
        registry.register(HookDefinition {
            id: HookId::new(),
            name: "audit/lint".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::McpTool {
                server: "audit".into(),
                tool: "lint".into(),
                input: std::collections::HashMap::from([
                    ("path".into(), json!("${tool_input.command}")),
                    ("event".into(), json!("hook:${hook_event_name}")),
                ]),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            blocking: true,
            timeout: Some(Duration::from_secs(12)),
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_mcp_invoker(invoker.clone());

        let agg = exec.execute(pre_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        let recorded = invoker.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].server, "audit");
        assert_eq!(recorded[0].tool, "lint");
        assert_eq!(recorded[0].timeout, Duration::from_secs(12));
        assert_eq!(recorded[0].input.get("path"), Some(&json!("rm -rf /")));
        assert_eq!(
            recorded[0].input.get("event"),
            Some(&json!("hook:PreToolUse"))
        );
    }

    #[tokio::test]
    async fn mcp_tool_hook_not_connected_and_is_error_stay_non_blocking() {
        let invoker = Arc::new(RecordingMcpInvoker {
            recorded: Mutex::new(Vec::new()),
            result: Mutex::new(vec![
                HookMcpInvocationResult::NotConnected {
                    message: "server audit is not connected".into(),
                },
                HookMcpInvocationResult::Error {
                    text_content: vec![r#"{"decision":"block","reason":"ignored"}"#.into()],
                    message: "tool returned isError".into(),
                },
            ]),
        });

        let hook = HookDefinition {
            id: HookId::new(),
            name: "audit/lint".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::McpTool {
                server: "audit".into(),
                tool: "lint".into(),
                input: std::collections::HashMap::new(),
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::Project),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };

        let mut registry = HookRegistry::new();
        registry.register(hook.clone());
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_mcp_invoker(invoker.clone());
        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(agg.decision, None);
        assert!(matches!(agg.all_results[0].1.outcome, HookOutcome::Error));
        assert!(agg.all_results[0].1.stderr.contains("not connected"));

        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_mcp_invoker(invoker);
        let agg = exec.execute(pre_event(), HookContext::default()).await;
        assert_eq!(agg.decision, None);
        assert!(matches!(agg.all_results[0].1.outcome, HookOutcome::Error));
        assert_eq!(
            agg.all_results[0].1.stdout,
            r#"{"decision":"block","reason":"ignored"}"#
        );
        assert!(agg.all_results[0].1.response.is_none());
    }
}

/// SH-06 — the `shell: "powershell"` spawn branch (oracle 2.1.238 @ 296948400).
#[cfg(test)]
mod sh06_shell_selector_tests {
    use super::*;

    /// `Bfa() = ["-NoProfile","-NonInteractive"]` plus `["-ExecutionPolicy",
    /// "Bypass"]` unless the respect-policy env var is set.
    #[test]
    fn powershell_base_args_match_bfa() {
        // The env var is unset in the test process, so the Bypass pair is on.
        assert_eq!(
            powershell_base_args(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
            ],
        );
    }

    /// `o9T` rewrites the three `${VAR}` host tokens into PowerShell's
    /// `${env:VAR}` form, and touches nothing else.
    #[test]
    fn powershell_env_token_rewrite_matches_o9t() {
        assert_eq!(
            powershell_env_token_rewrite("cd ${LINGXI_PROJECT_DIR}; ls"),
            "cd ${env:LINGXI_PROJECT_DIR}; ls",
        );
        assert_eq!(
            powershell_env_token_rewrite("${LINGXI_PLUGIN_ROOT}/${LINGXI_PLUGIN_DATA}"),
            "${env:LINGXI_PLUGIN_ROOT}/${env:LINGXI_PLUGIN_DATA}",
        );
        // Already-scoped and unrelated text pass through untouched.
        assert_eq!(
            powershell_env_token_rewrite("${env:LINGXI_PROJECT_DIR} $HOME"),
            "${env:LINGXI_PROJECT_DIR} $HOME",
        );
    }

    /// The `/\$CLAUDE_PROJECT_DIR\b/` warn probe: a BARE `$VAR` reference trips
    /// it, `$VARSOMETHING` does not (the `\b`), and the `${…}` form does not —
    /// that one is rewritten rather than warned about.
    #[test]
    fn bare_project_dir_probe_respects_the_word_boundary() {
        assert!(references_bare_project_dir_var("echo $LINGXI_PROJECT_DIR"));
        assert!(references_bare_project_dir_var("$LINGXI_PROJECT_DIR/x"));
        assert!(!references_bare_project_dir_var(
            "$LINGXI_PROJECT_DIRECTORY"
        ));
        assert!(!references_bare_project_dir_var("$LINGXI_PROJECT_DIR_2"));
        assert!(!references_bare_project_dir_var("echo hello"));
        // `${…}` is not a bare reference — `powershell_env_token_rewrite` fixes it.
        assert!(!references_bare_project_dir_var("${LINGXI_PROJECT_DIR}"));
    }

    /// The byte-locked resolution-failure message.
    #[test]
    fn powershell_missing_error_is_byte_faithful() {
        assert_eq!(
            powershell_missing_error("build.ps1"),
            "Hook \"build.ps1\" has shell: 'powershell' but no PowerShell executable \
             (pwsh or powershell) was found on PATH. Install PowerShell, or remove \
             \"shell\": \"powershell\" to use bash."
        );
    }

    /// `Otr() = Sh() ? "bash" : "powershell"`; `Sh()` is unconditionally true
    /// off Windows.
    #[test]
    #[cfg(not(windows))]
    fn default_shell_is_bash_on_posix() {
        assert_eq!(default_hook_shell(), crate::definition::HookShell::Bash);
    }
}

/// SH-01 — `wo(e, t)` / `Pfr` (oracle 2.1.238 @ 281366731 / 292378095).
#[cfg(test)]
mod sh01_classifier_context_tests {
    use crate::response::{truncate_utf16, PairedRewrite, CLASSIFIER_CONTEXT_CAP_UTF16};

    /// `Pfr = 2000`.
    #[test]
    fn cap_matches_the_oracle_constant() {
        assert_eq!(CLASSIFIER_CONTEXT_CAP_UTF16, 2000);
    }

    /// `if(e.length<=t)return e` — under the cap, the value is returned as-is.
    #[test]
    fn under_cap_is_identity() {
        assert_eq!(truncate_utf16("hello", 2000), "hello");
        assert_eq!(truncate_utf16("hello", 5), "hello");
    }

    /// `if(t<=0)return""`.
    #[test]
    fn zero_cap_is_empty() {
        assert_eq!(truncate_utf16("hello", 0), "");
    }

    /// The cap counts UTF-16 CODE UNITS, not chars and not bytes. `é` is one
    /// unit but two bytes, so a byte-counting cap would cut at 3 chars here.
    #[test]
    fn cap_counts_utf16_code_units_not_bytes() {
        assert_eq!(truncate_utf16("ééééé", 3), "ééé");
    }

    /// An astral char is TWO UTF-16 units, so cutting mid-pair must drop the
    /// leading high surrogate rather than emit a lone one
    /// (`n>=55296&&n<=56319 ? r.slice(0,-1) : r`).
    #[test]
    fn a_cut_never_splits_a_surrogate_pair() {
        // "a" + U+1F600 (2 units) = 3 units total.
        let s = "a\u{1F600}";
        assert_eq!(s.encode_utf16().count(), 3);
        // cap 2 lands on the high surrogate → it is dropped.
        assert_eq!(truncate_utf16(s, 2), "a");
        // cap 3 keeps the whole pair.
        assert_eq!(truncate_utf16(s, 3), s);
    }

    /// The four wire spellings upstream yields for `pairedRewrite`.
    #[test]
    fn paired_rewrite_wire_spellings() {
        assert_eq!(PairedRewrite::Direct.as_str(), "direct");
        assert_eq!(PairedRewrite::LegacyMcp.as_str(), "legacy_mcp");
        assert_eq!(PairedRewrite::Suppressed.as_str(), "suppressed");
        assert_eq!(PairedRewrite::None.as_str(), "none");
    }
}

#[test]
fn teammate_idle_uses_bare_session_uuid_and_omits_tool_context_only_fields() {
    let ctx = HookContext {
        session_id: lingxi_core::types::SessionId::nil(),
        transcript_path: "/workspace/session.jsonl".into(),
        cwd: "/workspace".into(),
        permission_mode: Some("plan".into()),
        agent_id: Some(lingxi_core::types::AgentId::new()),
        effort: Some(crate::hook_payload::EffortLevel {
            level: "high".into(),
        }),
        ..Default::default()
    };
    let (_, body) = build_envelope_body(
        &HookEvent::TeammateIdle {
            teammate_name: "scout".into(),
            team_name: "session-team".into(),
        },
        &ctx,
    )
    .unwrap();
    // Executed 2.1.263 Sa + E_n, as documented by the independent harness
    // fixture: E_n has no fourth Sa argument even when its caller has a ctx.
    assert_eq!(
        body,
        r#"{"session_id":"00000000-0000-0000-0000-000000000000","transcript_path":"/workspace/session.jsonl","cwd":"/workspace","permission_mode":"plan","hook_event_name":"TeammateIdle","teammate_name":"scout","team_name":"session-team"}"#
    );
}
