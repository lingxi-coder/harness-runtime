//! `PowerShellTool` — runs a PowerShell command.
//!
//! Windows: spawns `powershell.exe -Command <cmd>` directly (no sandbox available).
//! Other:   resolves `pwsh` via PATH; sandbox dispatch is identical to BashTool.
//!
//! See spec §7 wire identifiers (PowerShell row).

use crate::shared::strip_ansi_count;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{POWERSHELL_COMPLETED, POWERSHELL_FAILED, POWERSHELL_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH;
use tool_api::BuiltinToolContext;

/// Windows PowerShell executable.
pub const POWERSHELL_BIN_WINDOWS: &str = "powershell.exe";
/// Cross-platform PowerShell executable.
pub const POWERSHELL_BIN_UNIX: &str = "pwsh";
/// 2-minute default timeout — matches BashTool.
pub const POWERSHELL_DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 10-minute maximum timeout.
pub const POWERSHELL_MAX_TIMEOUT_MS: u64 = 600_000;
/// Tool name byte-lock.
pub const TOOL_NAME: &str = "PowerShell";

/// Refusal surfaced on Windows when the sandbox is enabled but the policy
/// disallows unsandboxed commands. Windows has no sandbox backend (PowerShell
/// cannot be wrapped), so a sandbox-required policy means the command MUST NOT
/// run — we refuse rather than silently running it unsandboxed.
pub const WINDOWS_SANDBOX_POLICY_REFUSAL: &str =
    "Sandbox is required by policy but is not available on Windows; PowerShell commands \
     cannot be run. Use the `/sandbox` command to adjust restrictions, or allow \
     unsandboxed commands.";

/// Does the Windows sandbox policy forbid running this command at all? — the
/// testable core of the `call`/`validate_input` refusal guards. On Windows the
/// sandbox cannot wrap PowerShell, so when the sandbox is `enabled` AND the
/// policy does NOT allow unsandboxed commands, the command must be refused.
/// Factored out so the matrix is testable on non-Windows hosts.
#[must_use]
fn windows_sandbox_policy_refuses(enabled: bool, allow_unsandboxed: bool) -> bool {
    enabled && !allow_unsandboxed
}

/// Locate the PowerShell binary appropriate for the host.
///
/// - On Windows: returns `Some("powershell.exe")` unconditionally; the OS resolves it.
/// - On other OSes: scans `$PATH` for an entry containing `pwsh`. Returns `None` if absent.
#[must_use]
pub fn resolve_powershell_path() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        Some(PathBuf::from(POWERSHELL_BIN_WINDOWS))
    } else {
        let path_env = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(POWERSHELL_BIN_UNIX);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

/// Build the PowerShell tool-result `data` payload for a finished command.
///
/// `is_error` runs through the 2.1.196 exit-code reinterpretation
/// (claude-code `Wja`, `powershell_semantics.rs`): grep-family / `git diff` /
/// `git grep` exit 1 = "no matches" / "differences found", NOT an error;
/// robocopy grades 0-7 succeed. When the exit code carries a semantic
/// meaning, `returnCodeInterpretation` is added (the binary's result data
/// `returnCodeInterpretation: E.message`); it is omitted otherwise. A timeout
/// is always an error.
#[must_use]
fn powershell_result_data(
    cmd_str: &str,
    exit_code: i32,
    timed_out: bool,
    stdout: &str,
    stderr: &str,
    truncated: bool,
) -> Value {
    let interp =
        crate::powershell_semantics::interpret_powershell_command_result(cmd_str, exit_code);
    let mut data = json!({
        "exit_code": exit_code,
        "stdout":    stdout,
        "stderr":    stderr,
        "is_error":  interp.is_error || timed_out,
        "timed_out": timed_out,
        "truncated": truncated,
    });
    if let Some(msg) = interp.message {
        data["returnCodeInterpretation"] = json!(msg);
    }
    data
}

/// `PowerShellTool` — same shape as BashTool but routes through pwsh on Unix
/// and powershell.exe on Windows.
#[derive(Clone)]
pub struct PowerShellTool {
    ctx: BuiltinToolContext,
}

impl PowerShellTool {
    /// Construct a fresh tool bound to the given builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "command":           { "type": "string", "description": "The PowerShell command to execute" },
            "timeout":           { "type": "number", "description": "Optional timeout in milliseconds (max 600000)" },
            "run_in_background": { "type": "boolean", "description": "Set to true to run this command in the background." },
            "description":       { "type": "string", "description": "Clear, concise description of what this command does in active voice." },
            "dangerouslyDisableSandbox": { "type": "boolean", "description": "Set this to true to dangerously override sandbox mode and run commands without sandboxing." }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

#[async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }
    /// `maxResultSizeChars:30000` on the PowerShell tool descriptor (2.1.220
    /// BIN off **235536522**), folded through `M0u` → `min(30000, 50000)`.
    /// Same persistence contract as Bash — see
    /// `tool_api::ToolHandler::persistence_threshold`.
    fn persistence_threshold(&self) -> Option<usize> {
        Some(MAX_TOOL_OUTPUT_LENGTH)
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-02 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _: &DescriptionOptions) -> String {
        input
            .get("description")
            .and_then(Value::as_str)
            .map_or_else(|| "Running PowerShell command".to_string(), str::to_string)
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Run a PowerShell command.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let cmd = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("missing `command`".into()))?;
        if cmd.is_empty() {
            return Err(ValidationError("`command` must not be empty".into()));
        }
        if let Some(t) = input.get("timeout").and_then(Value::as_u64) {
            if t > POWERSHELL_MAX_TIMEOUT_MS {
                return Err(ValidationError(format!(
                    "timeout {t} exceeds limit {POWERSHELL_MAX_TIMEOUT_MS}"
                )));
            }
        }
        // Windows sandbox-policy refusal: PowerShell cannot be sandbox-wrapped on
        // Windows, so a sandbox-required policy means the command must not run.
        // (/sandbox) Use the effective config so a live toggle is respected.
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();
        if cfg!(target_os = "windows")
            && windows_sandbox_policy_refuses(
                sandbox_runtime.enabled,
                sandbox_runtime.are_unsandboxed_commands_allowed(),
            )
        {
            return Err(ValidationError(WINDOWS_SANDBOX_POLICY_REFUSAL.into()));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        use platform_api::sandbox::ProcessCommand as SbxCommand;
        use sandbox::decision::{should_use_sandbox, SandboxDecision};

        let cmd_str = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing command".into()))?
            .to_string();
        let timeout_ms = input
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(POWERSHELL_DEFAULT_TIMEOUT_MS);
        if timeout_ms > POWERSHELL_MAX_TIMEOUT_MS {
            return Err(ToolError::InvalidInput(format!(
                "timeout {timeout_ms} exceeds limit {POWERSHELL_MAX_TIMEOUT_MS}"
            )));
        }

        // (/sandbox) Effective sandbox config for this command: frozen config
        // with `enabled` overridden by the live `/sandbox` toggle when wired.
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();

        // Windows sandbox-policy refusal — checked BEFORE `resolve_powershell_path`
        // so a missing-pwsh diagnostic cannot mask the policy refusal. PowerShell
        // cannot be sandbox-wrapped on Windows, so a sandbox-required policy means
        // the command must not run.
        if cfg!(target_os = "windows")
            && windows_sandbox_policy_refuses(
                sandbox_runtime.enabled,
                sandbox_runtime.are_unsandboxed_commands_allowed(),
            )
        {
            return Err(ToolError::PermissionDenied(
                WINDOWS_SANDBOX_POLICY_REFUSAL.into(),
            ));
        }

        let bin = match resolve_powershell_path() {
            Some(p) => p,
            None => {
                return Err(ToolError::InvalidInput(
                    "pwsh not found in PATH; install PowerShell to use this tool".into(),
                ))
            }
        };

        let started_at = SystemTime::now();
        let request_id = format!(
            "pwsh-{:016x}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
                ^ u64::from(std::process::id())
        );

        let mut meta_start: LogEventMetadata = HashMap::new();
        meta_start.insert(
            "request_id".into(),
            AnalyticsValue::String(request_id.clone()),
        );
        meta_start.insert("timeout_ms".into(), AnalyticsValue::Int(timeout_ms as i64));
        self.ctx.bus.log_event(POWERSHELL_STARTED, meta_start).await;

        // Sandbox decision — disabled on Windows (where wrap_with_sandbox bails);
        // the Windows exclusion is folded into the `sandbox_available` arg so the
        // decision falls to `NoSandbox` (branch 1 of `should_use_sandbox`).
        let dangerously_disable_sandbox = input
            .get("dangerouslyDisableSandbox")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Resolve the session cwd ONCE for this command (worktree parity
        // plan, Task 2) — reused below for the sandbox wrap and the spawned
        // process's `cwd`, so both read the same swap generation.
        let workspace = self.ctx.cwd();
        let decision = should_use_sandbox(
            &cmd_str,
            self.ctx.sandbox_available && cfg!(not(target_os = "windows")),
            dangerously_disable_sandbox,
            sandbox_runtime.are_unsandboxed_commands_allowed(),
            &sandbox_runtime,
            workspace.clone(),
        );
        let final_cmd = match decision {
            // HP-6: `sandbox.credentials` holds on the unsandboxed path too —
            // see the same arm in `bash.rs` for why `excludedCommands` and
            // `allowUnsandboxedCommands` must not double as credential consent.
            SandboxDecision::NoSandbox => format!(
                "{}{}",
                sandbox::credentials::unset_prefix_for_shell(
                    &sandbox_runtime.credential_deny_env,
                    Some(bin.display().to_string().as_str()),
                ),
                cmd_str
            ),
            SandboxDecision::Sandbox { policy: _ } => {
                // Wrap through the injected async `SandboxRunner`. The default
                // `LegacyWrapRunner` forwards to the sync `wrap_with_sandbox`
                // (ignoring `bin_shell`/`cwd`), so this is byte-identical to the
                // previous direct call.
                let bin_shell = bin.display().to_string();
                match self
                    .ctx
                    .sandbox_runner
                    .wrap(
                        &cmd_str,
                        &sandbox_runtime,
                        self.ctx.platform,
                        Some(&bin_shell),
                        Some(workspace.as_path()),
                    )
                    .await
                {
                    Ok(w) => w,
                    Err(sandbox::wrap::SandboxWrapError::Unsupported(s)) => {
                        return Err(ToolError::InvalidInput(s));
                    }
                    Err(sandbox::wrap::SandboxWrapError::SbplWrite(s)) => {
                        return Err(ToolError::Io(s));
                    }
                }
            }
        };

        let pcmd = SbxCommand {
            command: bin.display().to_string(),
            args: vec!["-Command".into(), final_cmd],
            cwd: Some(workspace.clone()),
            env: HashMap::new(),
            timeout: Some(Duration::from_millis(timeout_ms)),
            stdin: None,
        };
        let sandboxed = self
            .ctx
            .sandbox
            .bypass_with_audit(pcmd, "powershell_tool_call");

        let run_result = self.ctx.process.run(&sandboxed).await;
        // The wrapped command has finished: tear down any per-command sandbox
        // state. No-op for the default `LegacyWrapRunner`.
        self.ctx.sandbox_runner.cleanup_after_command().await;
        match run_result {
            Ok(out) => {
                // OTEL `claude_code.commit.count` / `claude_code.pull_request.count`
                // — claude-code drives `mEo(command, code, output)` from BOTH tool
                // completion seams, not just BashTool. The PowerShell arm alone
                // carries a skip guard:
                //   g = m.code===0 && !m.stdout && m.stderr && !m.backgroundTaskId
                //   if (!g && mEo(e.command, m.code, _).prResolved && …)
                // (the `backgroundTaskId` conjunct is vacuous here — this tool has
                // no background path). Byte-noop when OTEL is off.
                let quiet_success_with_stderr =
                    out.exit_code == 0 && out.stdout.is_empty() && !out.stderr.is_empty();
                if !quiet_success_with_stderr {
                    telemetry::otel::record_git_operation_counters(&cmd_str, out.exit_code);
                }

                let (stdout_clean, _ansi_out) = strip_ansi_count(&out.stdout);
                let (stderr_clean, _ansi_err) = strip_ansi_count(&out.stderr);
                // no-truncation: A1/STEP-4 — same contract as Bash. 2.1.220
                // has no shell output truncator left; `maxResultSizeChars:30000`
                // (BIN off 235536522) is a PERSISTENCE threshold consumed by
                // `orchestrator::tool_result_persistence`. `truncated` keeps
                // reporting whether the limit was EXCEEDED (the oracle's
                // `D.length>Jst()` predicate), which is what the analytics
                // field and the TUI badge mean.
                let truncated = stdout_clean.len() > MAX_TOOL_OUTPUT_LENGTH;
                let stdout_final = stdout_clean;
                let data = powershell_result_data(
                    &cmd_str,
                    out.exit_code,
                    out.timed_out,
                    &stdout_final,
                    &stderr_clean,
                    truncated,
                );
                let elapsed = SystemTime::now()
                    .duration_since(started_at)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert("request_id".into(), AnalyticsValue::String(request_id));
                meta.insert(
                    "exit_code".into(),
                    AnalyticsValue::Int(i64::from(out.exit_code)),
                );
                meta.insert("duration_ms".into(), AnalyticsValue::Int(elapsed as i64));
                meta.insert("truncated".into(), AnalyticsValue::Bool(truncated));
                self.ctx.bus.log_event(POWERSHELL_COMPLETED, meta).await;

                Ok(ToolCallResult {
                    data,
                    model_content: None,
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            Err(e) => {
                let mut meta: LogEventMetadata = HashMap::new();
                meta.insert(
                    "error_kind".into(),
                    AnalyticsValue::String("spawn_failed".into()),
                );
                self.ctx.bus.log_event(POWERSHELL_FAILED, meta).await;
                Err(ToolError::Io(format!("{e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    // PATH is process-global, and several tests install different temporary
    // `pwsh` binaries. The override CARRIES the crate-wide writer guard, so it
    // is restored on every exit path (including panic) and cannot be taken
    // without excluding both the sibling overrides here AND the tests in other
    // modules that resolve a program by bare name — see `crate::test_path_env`.
    struct PathOverride(
        Option<OsString>,
        Option<std::sync::RwLockWriteGuard<'static, ()>>,
    );

    impl PathOverride {
        fn set(value: impl AsRef<std::ffi::OsStr>) -> Self {
            let guard = crate::test_path_env::write();
            let prior = std::env::var_os("PATH");
            std::env::set_var("PATH", value);
            Self(prior, Some(guard))
        }
    }

    impl Drop for PathOverride {
        fn drop(&mut self) {
            match self.0.take() {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            drop(self.1.take());
        }
    }

    /// A1/STEP-4 — PowerShell mirrors Bash: no model-facing truncation, a
    /// 30 000-byte PERSISTENCE threshold instead (2.1.220 BIN off 235536522).
    #[test]
    fn powershell_declares_a_30k_persistence_threshold_and_no_truncator() {
        use tool_api::tool_trait::Tool as _;
        let tool = PowerShellTool::new(shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }));
        assert_eq!(tool.persistence_threshold(), Some(30_000));
        // Source-level lock: the shared shell truncator must not reappear on
        // the model-facing path. Built at runtime so this assertion's own text
        // cannot satisfy it.
        let needle = format!("{}{}", "truncate_shell", "_output");
        assert!(
            !include_str!("powershell.rs").contains(&needle),
            "the model-facing stdout must reach the result mapper verbatim"
        );
    }

    /// The OTEL runtime and its counter registry are process-global.
    static OTEL_LOCK: Mutex<()> = Mutex::new(());

    /// Drive one PowerShell completion through the real `call()` and return the
    /// rendered Prometheus registry.
    async fn powershell_completion_metrics(command: &str, out: ProcessOutput) -> String {
        use std::os::unix::fs::PermissionsExt;
        let tool = PowerShellTool::new(shell_test_ctx(out));

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pwsh");
        std::fs::write(&fake, "#!/bin/sh\necho stub\n").unwrap();
        let mut p = std::fs::metadata(&fake).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake, p).unwrap();
        let _path = PathOverride::set(dir.path());

        tool.call(json!({"command": command}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        telemetry::otel::prometheus_text().expect("prometheus registry")
    }

    /// claude-code drives `mEo(command, code, output)` from BOTH tool
    /// completion seams — Bash @235699561 and PowerShell @235540668 — so a
    /// `gh pr create` routed through PowerShell must bump
    /// `claude_code.pull_request.count` exactly as the Bash seam does. The
    /// PowerShell arm is the one that carries a skip guard:
    ///   g = m.code===0 && !m.stdout && m.stderr && !m.backgroundTaskId
    ///   if (!g && mEo(e.command, m.code, _).prResolved && …)
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn powershell_completion_records_git_counters_with_the_oracle_skip_guard() {
        let _otel = OTEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = telemetry::otel::OtelConfig::from_lookup(|key| match key {
            telemetry::otel::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("prometheus".to_string()),
            "OTEL_LOGS_EXPORTER" | "OTEL_TRACES_EXPORTER" => Some("none".to_string()),
            _ => None,
        });
        let _guard = telemetry::otel::install_process_with_config("test-powershell", false, cfg);

        // Silent success that only wrote to stderr ⇒ the whole `mEo` call is
        // skipped, so the counter must not even be registered yet.
        let quiet = powershell_completion_metrics(
            "gh pr create --title x",
            ProcessOutput {
                stdout: String::new(),
                stderr: "warning: something\n".into(),
                exit_code: 0,
                timed_out: false,
            },
        )
        .await;
        assert!(
            !quiet.contains("claude_code_pull_request_count"),
            "the !g guard must skip mEo entirely, got: {quiet}"
        );

        // Normal completion ⇒ counted, same as the Bash seam.
        let counted = powershell_completion_metrics(
            "gh pr create --title x",
            ProcessOutput {
                stdout: "https://github.com/o/r/pull/1\n".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
        )
        .await;
        assert!(
            counted.contains("claude_code_pull_request_count"),
            "PowerShell seam must record the PR counter, got: {counted}"
        );
    }

    /// 2.1.196 regression lock: grep-family / git diff / git grep exit 1 is
    /// NOT reported as an error (and carries `returnCodeInterpretation`),
    /// while default commands keep any-nonzero-is-error.
    #[test]
    fn exit_one_search_commands_are_not_failures() {
        let d = powershell_result_data("grep \"foo|bar\" file.txt", 1, false, "", "", false);
        assert_eq!(d["is_error"], false);
        assert_eq!(d["returnCodeInterpretation"], "No matches found");

        let g = powershell_result_data("git diff", 1, false, "", "", false);
        assert_eq!(g["is_error"], false);
        assert_eq!(g["returnCodeInterpretation"], "Files differ");

        let e = powershell_result_data("egrep pat f.txt", 1, false, "", "", false);
        assert_eq!(e["is_error"], false);

        // Default semantics: exit 1 stays an error with the failure note.
        let x = powershell_result_data("Get-ChildItem", 1, false, "", "", false);
        assert_eq!(x["is_error"], true);
        assert_eq!(
            x["returnCodeInterpretation"],
            "Command failed with exit code 1"
        );

        // Exit 0 carries NO returnCodeInterpretation key at all.
        let ok = powershell_result_data("git diff", 0, false, "out", "", false);
        assert_eq!(ok["is_error"], false);
        assert!(ok.get("returnCodeInterpretation").is_none());

        // A timeout is an error even when the exit code is benign.
        let t = powershell_result_data("grep pat f", 1, true, "", "", false);
        assert_eq!(t["is_error"], true);
    }

    #[test]
    fn locked_constants_unchanged() {
        assert_eq!(POWERSHELL_BIN_WINDOWS, "powershell.exe");
        assert_eq!(POWERSHELL_BIN_UNIX, "pwsh");
        assert_eq!(POWERSHELL_DEFAULT_TIMEOUT_MS, 120_000);
        assert_eq!(POWERSHELL_MAX_TIMEOUT_MS, 600_000);
        assert_eq!(TOOL_NAME, "PowerShell");
    }

    #[test]
    fn resolve_path_smoke() {
        // On non-Windows hosts this may legitimately return None.
        let _ = resolve_powershell_path();
    }

    /// The Windows sandbox-policy refusal predicate: enabled + disallowed ⇒
    /// refuse; allowed OR disabled ⇒ no refusal. Tested via the factored
    /// `windows_sandbox_policy_refuses` so it runs on every host.
    #[test]
    fn windows_sandbox_policy_refusal_blocks_call() {
        // enabled && !allow_unsandboxed ⇒ refuse.
        assert!(windows_sandbox_policy_refuses(true, false));
        // allow_unsandboxed=true ⇒ no refusal.
        assert!(!windows_sandbox_policy_refuses(true, true));
        // enabled=false ⇒ no refusal.
        assert!(!windows_sandbox_policy_refuses(false, false));
        assert!(!windows_sandbox_policy_refuses(false, true));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn resolve_path_unix_scans_path_env() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pwsh");
        std::fs::write(&fake, "#!/bin/sh\necho stub\n").unwrap();
        let mut perm = std::fs::metadata(&fake).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&fake, perm).unwrap();

        let _path = PathOverride::set(dir.path());
        let resolved = resolve_powershell_path();
        let resolved = resolved.expect("should find pwsh");
        assert_eq!(resolved.file_name().unwrap(), "pwsh");
    }

    #[tokio::test]
    async fn validate_rejects_overlong_timeout() {
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let tool = PowerShellTool::new(ctx);
        let r = tool
            .validate_input(
                &json!({"command": "Get-Date", "timeout": 600_001}),
                &fresh_ctx(),
            )
            .await;
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("600000"), "got: {msg}");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pwsh_missing_returns_invalid_input_with_diagnostic() {
        let _path = PathOverride::set("");
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let tool = PowerShellTool::new(ctx);
        let r = tool
            .call(json!({"command": "Get-Date"}), fresh_ctx(), fresh_tx())
            .await;
        let err = r.expect_err("pwsh missing");
        let msg = err.to_string();
        assert!(msg.contains("pwsh not found in PATH"), "got: {msg}");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn foreground_zero_exit_returns_stdout() {
        use std::os::unix::fs::PermissionsExt;
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: "ok\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let tool = PowerShellTool::new(ctx);

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pwsh");
        std::fs::write(&fake, "#!/bin/sh\necho stub\n").unwrap();
        let mut p = std::fs::metadata(&fake).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake, p).unwrap();

        let _path = PathOverride::set(dir.path());
        let res = tool
            .call(json!({"command": "Get-Date"}), fresh_ctx(), fresh_tx())
            .await;
        let r = res.expect("ok");
        assert_eq!(r.data["exit_code"], 0);
        assert_eq!(r.data["stdout"], "ok\n");
    }

    /// Task 10 — assert the M2-04 locked sandbox-refusal literal is still in
    /// place at its lock site. If `wrap_with_sandbox` returns
    /// `Unsupported(s)`, this is the string that PowerShellTool surfaces
    /// verbatim through `ToolError::InvalidInput(s)`.
    #[test]
    fn sandbox_refusal_literal_byte_locked_at_m204_site() {
        // The literal lives at `SandboxError::Unsupported` in lingxi-traits:
        // `#[error("sandbox not supported on this platform")]`.
        let err = platform_api::sandbox::SandboxError::Unsupported;
        assert_eq!(err.to_string(), "sandbox not supported on this platform");
    }

    /// Records `wrap`/`cleanup_after_command` calls so a test can prove the
    /// PowerShell tool routes through `ctx.sandbox_runner`.
    struct WrapCall {
        command: String,
        bin_shell: Option<String>,
        cwd: Option<std::path::PathBuf>,
    }

    #[derive(Default)]
    struct RecordingSandboxRunner {
        wrap_calls: std::sync::Mutex<Vec<WrapCall>>,
        cleanups: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl tool_api::SandboxRunner for RecordingSandboxRunner {
        async fn wrap(
            &self,
            command: &str,
            _cfg: &sandbox::runtime_config::SandboxRuntimeConfig,
            _platform: sandbox::runtime_config::Platform,
            bin_shell: Option<&str>,
            cwd: Option<&std::path::Path>,
        ) -> Result<String, sandbox::wrap::SandboxWrapError> {
            self.wrap_calls.lock().unwrap().push(WrapCall {
                command: command.to_string(),
                bin_shell: bin_shell.map(ToString::to_string),
                cwd: cwd.map(std::path::Path::to_path_buf),
            });
            Ok(format!("WRAPPED::{command}"))
        }

        async fn cleanup_after_command(&self) {
            self.cleanups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sandbox_branch_routes_through_injected_runner_and_cleans_up() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;

        let runner = Arc::new(RecordingSandboxRunner::default());
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        // Force the Sandbox branch: available sandbox + no excluded commands.
        ctx.sandbox_available = true;
        ctx.sandbox_runtime.excluded_commands = vec![];
        ctx.session_cwd
            .swap(std::path::PathBuf::from("/tmp"), ctx.trusted_dirs());
        ctx.sandbox_runner = runner.clone();
        let tool = PowerShellTool::new(ctx);

        // Stub a `pwsh` on PATH so the tool gets past the binary probe.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pwsh");
        std::fs::write(&fake, "#!/bin/sh\necho stub\n").unwrap();
        let mut p = std::fs::metadata(&fake).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake, p).unwrap();

        let _path = PathOverride::set(dir.path());
        let res = tool
            .call(json!({"command": "Get-Date"}), fresh_ctx(), fresh_tx())
            .await;
        res.expect("ok");

        let calls = runner.wrap_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "wrap should be called once");
        let call = &calls[0];
        assert_eq!(call.command, "Get-Date");
        // bin_shell is the resolved pwsh path.
        assert_eq!(
            call.bin_shell.as_deref(),
            Some(fake.display().to_string().as_str())
        );
        assert_eq!(call.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(
            runner.cleanups.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cleanup_after_command must be invoked once"
        );
    }

    /// Binary-grounded schema assertions (offset 203601400):
    /// `A.strictObject({command, timeout:sB(A.number()), run_in_background, description, dangerouslyDisableSandbox})`
    #[test]
    fn powershell_input_schema_byte_parity() {
        let schema = &*INPUT_SCHEMA;
        let props = &schema["properties"];

        // Field named `timeout` (not `timeout_ms`), type `number` (not integer), no minimum/maximum.
        assert!(
            props.get("timeout").is_some(),
            "schema must expose `timeout`"
        );
        assert!(
            props.get("timeout_ms").is_none(),
            "schema must NOT expose `timeout_ms`"
        );
        assert_eq!(
            props["timeout"]["type"], "number",
            "timeout type must be number"
        );
        assert!(
            props["timeout"].get("minimum").is_none(),
            "timeout must have no minimum"
        );
        assert!(
            props["timeout"].get("maximum").is_none(),
            "timeout must have no maximum"
        );

        // dangerouslyDisableSandbox present with boolean type and byte-exact description.
        assert!(
            props.get("dangerouslyDisableSandbox").is_some(),
            "dangerouslyDisableSandbox must be present"
        );
        assert_eq!(props["dangerouslyDisableSandbox"]["type"], "boolean");
        assert_eq!(
            props["dangerouslyDisableSandbox"]["description"],
            "Set this to true to dangerously override sandbox mode and run commands without sandboxing."
        );

        // additionalProperties:false (strictObject).
        assert_eq!(
            schema["additionalProperties"], false,
            "must have additionalProperties:false"
        );

        // Per-field descriptions present.
        assert_eq!(
            props["command"]["description"],
            "The PowerShell command to execute"
        );
        assert_eq!(
            props["description"]["description"],
            "Clear, concise description of what this command does in active voice."
        );
        assert_eq!(
            props["run_in_background"]["description"],
            "Set to true to run this command in the background."
        );
    }

    /// Bash gap: timeout must be type:number (not integer) and have no minimum constraint.
    #[test]
    fn bash_timeout_schema_type_number_no_minimum() {
        // Read the Bash INPUT_SCHEMA via the lazy static exposed in bash.rs via #[cfg(test)] helper.
        // Instead, use the powershell schema as our target here and let bash.rs tests cover bash.
        // This test covers the PowerShell side only (bash.rs has its own test module).
        let schema = &*INPUT_SCHEMA;
        let timeout = &schema["properties"]["timeout"];
        assert_eq!(timeout["type"], "number");
        assert!(timeout.get("minimum").is_none());
    }
}
