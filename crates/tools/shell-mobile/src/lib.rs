//! Mobile-only `Shell` tool crate (shared mobile shell surface, spec r3 §Shell
//! tool / D9, D10).
//!
//! `ShellMobileTool` is the model-facing shell on mobile. Its `call()` builds a
//! **deny-net** [`SandboxPolicy`] and runs the command through the platform's
//! injected mobile `ProcessRunner` / `Sandbox` pair — `ctx.sandbox.prepare()`
//! then `ctx.process.run()`. The tool is registered ONLY when the device +
//! config gate passes (`ctx.mobile_shell().is_some_and(|c| c.enabled)`); that
//! gate is computed in the mobile engine composition root so a selected but
//! blocked/unlinked mobile-linux runtime never silently falls back to the
//! legacy Android shell. Desktop `BashTool` remains untouched.
//!
//! Network-intent commands (`git clone`, `curl`, `apk add`, `npm install`, …)
//! are refused up-front on legacy deny-net shells with an advisory pointing at
//! the structured Git tool or the mobile-linux permission gate, rather than run
//! to a confusing seccomp `EPERM` — see [`net_intent`]. In mobile-linux/iSH the
//! same capability report remains advisory only; the actual control point is
//! the shell invocation permission gate because iSH cannot provide a true
//! per-command deny-net sandbox.

#![forbid(unsafe_code)]

mod android_host_intent;
pub mod net_intent;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::{PermissionMetadata, PermissionPrompt};
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
};
use tool_api::util::output_truncation::{truncate_shell_output, MAX_TOOL_OUTPUT_LENGTH};
use tool_api::BuiltinToolContext;

use lingxi_core::host::sandbox::{ProcessCommand, SandboxError, SandboxPolicy};
use mobile_linux_api::ProcessError;
use mobile_linux_api::{NetworkPolicy, ResourceLimits};

/// Tool name byte-lock — the model-facing name for the mobile shell.
pub const TOOL_NAME: &str = "Shell";

/// Maximum shell timeout (ms) — mirrors the desktop Bash 10-minute ceiling.
const SHELL_MAX_TIMEOUT_MS: u64 = 600_000;

/// Default shell timeout (ms) when the caller omits one — mirrors the desktop
/// Bash 2-minute default so an unspecified-timeout command is always bounded
/// by the runner watchdog, never left to run to the runner's own ceiling.
const SHELL_DEFAULT_TIMEOUT_MS: u64 = 120_000;
static NEXT_SHELL_PROCESS_OWNER: AtomicU64 = AtomicU64::new(1);

/// `ShellMobileTool` — run a deny-net shell command through the P2 runner.
#[derive(Clone)]
pub struct ShellMobileTool {
    ctx: BuiltinToolContext,
    precommit_latch: tool_api::bash_precommit::BashPrecommitLatch,
}

impl ShellMobileTool {
    /// Construct from the builtin tool context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            precommit_latch: Default::default(),
        }
    }
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    tool_api::tool_trait::generated_builtin_schema(&json!({
        "type": "object",
        "properties": {
            "command":     { "type": "string", "description": "The shell command to run (mksh dialect)." },
            "timeout":     { "type": "integer", "minimum": 1, "maximum": SHELL_MAX_TIMEOUT_MS, "description": "Optional timeout in milliseconds." },
            "description": { "type": "string", "description": "Optional short description of what the command does." }
        },
        "required": ["command"]
    }))
});

/// Map a `prepare()` [`SandboxError`] to a tool error that NAMES the guarantee
/// that could not be enforced (spec: fail-closed, never silent success).
fn map_sandbox_err(e: &SandboxError) -> ToolError {
    match e {
        SandboxError::Unavailable(s) => ToolError::Internal(format!(
            "shell sandbox unavailable (deny-net not enforceable): {s}"
        )),
        SandboxError::Unsupported => ToolError::Internal(
            "shell sandbox not supported on this platform (deny-net not enforceable)".into(),
        ),
        SandboxError::PathCanonicalize(s) => {
            ToolError::Internal(format!("shell sandbox path canonicalization failed: {s}"))
        }
        SandboxError::SymlinkEscape(s) => {
            ToolError::Internal(format!("shell sandbox symlink escape rejected: {s}"))
        }
        SandboxError::Io(s) => ToolError::Io(format!("shell sandbox io error: {s}").into()),
    }
}

/// Map a `run()` [`ProcessError`] to a tool error. Each structured variant
/// names the guarantee that failed (spec: errors must name the unenforceable
/// guarantee); `Timeout` becomes a timeout message.
fn map_process_err(e: &ProcessError, timeout_ms: u64) -> ToolError {
    match e {
        ProcessError::Timeout => {
            ToolError::Internal(format!("Shell command timed out after {timeout_ms}ms"))
        }
        ProcessError::Unsupported => ToolError::Internal("shell unavailable on this device".into()),
        ProcessError::PolicyUnsupported(s) => ToolError::Internal(format!(
            "shell policy unsupported (deny-net not enforceable): {s}"
        )),
        ProcessError::MalformedSandboxPlan(s) => {
            ToolError::Internal(format!("shell sandbox plan malformed: {s}"))
        }
        ProcessError::SandboxEnforcementFailed(s) => ToolError::Internal(format!(
            "shell sandbox enforcement failed (fail-closed): {s}"
        )),
        ProcessError::Io(s) => ToolError::Io(format!("shell io error: {s}").into()),
    }
}

#[async_trait]
impl Tool for ShellMobileTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // Defensive double-gate: registration (`register_all`) already filters
        // on this same flag, but keep the tool inert if it ever lands in a
        // registry without the gate set.
        self.ctx.mobile_shell().is_some_and(|a| a.enabled)
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        // Conservative: a shell mutates cwd / filesystem, so never run it
        // concurrently with other tools.
        false
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        // A shell command can mutate the workspace; treat as not read-only.
        false
    }

    fn is_destructive(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        // The guest shell (iSH/Alpine) really can reach the network: it ships
        // apk, npm, npx, pip, git and curl, and nothing below it enforces
        // deny-net. `call` deliberately does not refuse network intent there —
        // refusing outright would make the toolchain useless — so the gate has
        // to be the decision point instead. Returning Ask here is what makes an
        // agent-initiated network command require approval, and the host's
        // prompt handling supplies the once/session/always semantics.
        //
        // On the legacy deny-net shell this is unnecessary: `call` already
        // refuses network intent up-front, before anything runs.
        let guest_shell = self
            .ctx
            .mobile_shell()
            .is_some_and(|shell| shell.force_platform_sandbox);
        if guest_shell {
            if let Some(command) = input.string_at("/command") {
                if let Some(advice) = net_intent::network_intent(command) {
                    return PermissionResult::Ask {
                        reason: PermissionDecisionReason::Other {
                            reason: "guest shell command shows network intent".into(),
                        },
                        prompt: PermissionPrompt {
                            title: "Allow network access?".into(),
                            message: advice.into(),
                            options: vec![
                                "Allow once".into(),
                                "Allow for this session".into(),
                                "Always allow".into(),
                                "Deny".into(),
                            ],
                        },
                        pending_classifier_check: None,
                        metadata: PermissionMetadata::default(),
                    };
                }
            }
        }

        // Otherwise: the desktop `BashTool::check_permissions` is itself a stub
        // (`tools/shell/src/bash.rs` "allow-all-gate (M4-02 default)"), with real
        // allow/ask gating in the engine's `AdapterPermissionGate` (wired on
        // mobile via `PermissionRequestSink` -> client UI). Mirror that.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "mobile-shell deny-net (engine AdapterPermissionGate handles allow/ask)"
                    .into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Utf16JsonProjection, _opts: &DescriptionOptions) -> String {
        match input.string_at("/description") {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => "Run shell command".into(),
        }
    }

    async fn prompt(&self, opts: &PromptOptions) -> Utf16JsonProjection {
        let (applets, sh_version, bundled, runtime_label) = match self.ctx.mobile_shell() {
            Some(a) => (
                a.applets.clone(),
                a.sh_version.clone(),
                a.bundled,
                a.runtime_label.clone(),
            ),
            None => (Vec::new(), None, false, "system mksh".to_string()),
        };
        let applet_line = if applets.is_empty() {
            "system toybox".to_string()
        } else {
            applets.join(", ")
        };
        let mut prompt = String::new();
        if bundled {
            prompt.push_str(&format!(
                "Run a shell command on this mobile device. The shell is a \
                 **{runtime_label}** — NOT bash. \
                 Avoid bash-only syntax: no process substitution `<(...)`, no \
                 `${{var,,}}` case-folding, no `mapfile`/`readarray`.\n\n",
            ));
        } else {
            prompt.push_str(&format!(
                "Run a shell command on this mobile device. The shell is the \
                 current mobile backend's **{runtime_label}** — NOT bash. Avoid bash-only \
                 syntax: no process substitution `<(...)`, no `${{var,,}}` case-folding, no \
                 `mapfile`/`readarray`.\n\n",
            ));
        }
        if self
            .ctx
            .mobile_shell()
            .is_some_and(|shell| shell.force_platform_sandbox)
        {
            prompt.push_str(
                "This mobile guest shell provides a basic development environment. \
                 Agent-initiated network access is available only after the existing \
                 shell permission gate approves the invocation (one-time, session, \
                 or permanent approval). iSH/OpenMinis cannot provide a true \
                 per-command deny-net sandbox, so network-intent detection here is \
                 advisory and the permission gate is the actual control point. \
                 Interactive users may still install packages with `apk add`; agent \
                 calls remain permission-gated.\n\n",
            );
        } else {
            prompt.push_str(
                "This mobile shell provides a basic development environment with \
                 no network access. Network commands (curl/wget/ssh/git clone/fetch/pull/push/\
                 apk add/npm install/pip install) are refused up-front. Use the Git \
                 tool for remote git operations; local git works here.\n\n",
            );
        }
        prompt.push_str(
            "Commands run rooted at the workspace directory. Output is captured and \
             truncated if very large. ",
        );
        if self
            .ctx
            .mobile_shell()
            .is_some_and(|shell| shell.force_platform_sandbox)
        {
            prompt.push_str(
                "This is an app-sandboxed guest shell, not the host process \
                 environment. Use guest/workspace paths only, and do not pretend \
                 host-level automation or device management can run here.\n\n",
            );
        } else {
            prompt.push_str(
                "This is an app-sandboxed workspace shell, not a host-device \
                 management shell: do not use host-only commands such as `monkey`, \
                 `am`, `cmd`, `pm`, `input`, `settings`, `dumpsys`, `/sdcard`, or \
                 other platform host paths. Use relative workspace paths. ",
            );
            if self.ctx.computer_control.is_some() {
                prompt.push_str(
                    "For supported device/app automation, call `android_use.status` and then \
                     `android_use.open_app`/UI actions in a user-started authorized Computer \
                     Use session. If that session is inactive, ask the user to enable/start \
                     it; never fall back to host commands or hide permission errors with \
                     `|| true`/`2>/dev/null`.\n\n",
                );
            } else {
                prompt.push_str(
                    "Host-level device/app automation is unavailable in this build; ask the \
                     user to complete those operations manually. Never retry with host \
                     commands or hide permission errors with `|| true`/`2>/dev/null`.\n\n",
                );
            }
        }
        if bundled {
            prompt.push_str(&format!(
                "Available bundled toybox applets (locked inventory): {applet_line}.\n"
            ));
        } else {
            prompt.push_str(&format!("Available applets: {applet_line}.\n"));
        }
        if let Some(v) = sh_version {
            prompt.push_str(&format!("Shell version: {v}.\n"));
        }
        let precommit = self.precommit_latch.suggestion(
            opts.bash_precommit_skills,
            opts.bash_precommit_session_generation,
        );
        if !precommit.is_empty() {
            prompt.push('\n');
            prompt.push_str(&precommit);
            prompt.push('\n');
        }
        prompt.into()
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let cmd = input
            .string_at("/command")
            .ok_or_else(|| ValidationError("missing `command`".into()))?;
        if cmd.is_empty() {
            return Err(ValidationError("`command` must not be empty".into()));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // 1. Extract + validate the command.
        // The command is kept as UTF-16 source through validation and reaches
        // the process boundary only when converted to the host OS string.
        let command_units = input
            .string_units("/command")
            .ok_or_else(|| ToolError::InvalidInput("missing command".into()))?;
        let command = String::from_utf16_lossy(&command_units);
        if command.is_empty() {
            return Err(ToolError::InvalidInput("command must not be empty".into()));
        }
        let timeout_ms = input.at_pointer("/timeout").and_then(|value| {
            let number = value.as_number()?;
            (number.is_finite()
                && number >= 0.0
                && number.fract() == 0.0
                && number < (u64::MAX as f64))
                .then_some(number as u64)
        });
        if let Some(t) = timeout_ms {
            if t > SHELL_MAX_TIMEOUT_MS {
                return Err(ToolError::InvalidInput(
                    format!("timeout {t} exceeds limit {SHELL_MAX_TIMEOUT_MS}").into(),
                ));
            }
        }

        // 2. Refuse Android host commands/paths and network-intent commands
        // BEFORE building/running anything. The mobile shell runs as the app
        // UID, not Android's privileged shell UID; trying `monkey`/`cmd` only
        // produces misleading SELinux diagnostics.
        if let Some(advice) =
            android_host_intent::android_host_intent(&command, self.ctx.computer_control.is_some())
        {
            return Err(ToolError::InvalidInput(advice.into()));
        }

        // The legacy shell is deny-net, so this is a clean advisory instead of
        // a confusing seccomp EPERM.
        let mobile_linux_guest = self
            .ctx
            .mobile_shell()
            .is_some_and(|shell| shell.force_platform_sandbox);
        if !mobile_linux_guest {
            if let Some(advice) = net_intent::network_intent(&command) {
                return Err(ToolError::InvalidInput(advice.into()));
            }
        }

        let shell_path = self
            .ctx
            .mobile_shell()
            .map(|ctx| ctx.shell_path.clone())
            .ok_or_else(|| ToolError::InvalidInput("mobile_shell context is absent".into()))?;

        // 3. Build the raw command (system mksh, -c). An unspecified timeout
        //    falls back to the default so the runner watchdog always has a
        //    bound (never relies on the runner's own ceiling).
        let effective_timeout = timeout_ms.unwrap_or(SHELL_DEFAULT_TIMEOUT_MS);
        let pcmd = ProcessCommand {
            command: shell_path,
            args: vec!["-c".to_string(), command],
            cwd: Some(self.ctx.cwd()),
            env: HashMap::new(),
            timeout: Some(Duration::from_millis(effective_timeout)),
            stdin: None,
        };

        // 4. Build the deny-net default policy (D10: deny-net ALWAYS).
        let policy = SandboxPolicy {
            // The adapter permission gate has already authorized this tool call.
            // Legacy retains strict deny-net; MobileLinux maps approved calls to
            // the guest network so `apk add` and explicit network work function.
            network: if mobile_linux_guest {
                NetworkPolicy::Allowed
            } else {
                NetworkPolicy::Disabled
            },
            writable_paths: vec![],
            denied_paths: vec![],
            allow_subprocess: true,
            limits: ResourceLimits::default(),
        };

        // 5. prepare() — fail-closed; surface a tool error naming the guarantee.
        let sandboxed = self
            .ctx
            .sandbox
            .prepare(pcmd, &policy)
            .map_err(|e| map_sandbox_err(&e))?;

        // Give this command a unique child owner under its agent. The runner
        // can then reap exactly this process on turn cancellation while
        // agent-level cleanup can still match the stable prefix.
        let agent_owner = ctx
            .agent_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "main".to_string());
        let process_owner = format!(
            "{agent_owner}/mobile-shell-{}",
            NEXT_SHELL_PROCESS_OWNER.fetch_add(1, Ordering::Relaxed)
        );
        let sandboxed = sandboxed.with_process_owner(Some(process_owner.clone()));

        // 6. Keep the process future alive until the runtime confirms that its
        // raw stdio session is closed and the guest process tree is reaped.
        // Dropping this future would abandon the runtime-owned session.
        let output_limit_bytes = Some(self.max_result_size_chars().saturating_mul(4));
        let run = self
            .ctx
            .process
            .run_foreground_with_output_limit(&sandboxed, output_limit_bytes);
        tokio::pin!(run);
        let foreground = if let Some(cancel) = ctx.cancel.as_ref() {
            if cancel.is_cancelled() {
                return Err(ToolError::Aborted);
            }
            // Poll once so the runner registers the owner before cancellation
            // can race with raw-session opening. If it completes immediately,
            // preserve that completed result.
            let first_poll =
                std::future::poll_fn(|cx| match std::future::Future::poll(run.as_mut(), cx) {
                    std::task::Poll::Ready(result) => std::task::Poll::Ready(Some(result)),
                    std::task::Poll::Pending => std::task::Poll::Ready(None),
                })
                .await;
            if let Some(result) = first_poll {
                result
            } else {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        self.ctx.process.kill_owner_processes(&process_owner).await;
                        let _ = run.await;
                        return Err(ToolError::Aborted);
                    }
                    result = &mut run => result,
                }
            }
        } else {
            run.await
        }
        .map_err(|e| map_process_err(&e, effective_timeout))?;
        let out = match foreground.outcome {
            lingxi_core::host::ForegroundOutcome::Completed(output) => output,
            lingxi_core::host::ForegroundOutcome::MovedToBackground(_) => {
                return Err(ToolError::Internal(
                    "mobile shell runner unexpectedly detached a foreground command".into(),
                ));
            }
        };

        // 7. Build the result. Honor `timed_out` -> timeout error.
        if out.timed_out {
            return Err(ToolError::Internal(format!(
                "Shell command timed out after {effective_timeout}ms"
            )));
        }
        let limit = self.max_result_size_chars();
        let (stdout, truncated_out) = truncate_shell_output(out.stdout, limit);
        let (stderr, truncated_err) = truncate_shell_output(out.stderr, limit);

        let permission_denied = output_reports_permission_denial(&stdout, &stderr);
        let command_failed = out.exit_code != 0 || permission_denied;
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({
                "exit_code": out.exit_code,
                "stdout":    stdout,
                "stderr":    stderr,
                "is_error":  command_failed,
                "timed_out": false,
                "truncated": truncated_out || truncated_err,
            })),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            // A command that masks Android/SELinux denial with `|| true` still
            // reports failure to the model and Android timeline.
            is_error: permission_denied,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

fn output_reports_permission_denial(stdout: &str, stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    let stdout = stdout.to_ascii_lowercase();
    stderr.contains("permission denied")
        || stderr.contains("access denied")
        || stderr.contains("permission denial:")
        || stderr.contains("securityexception")
        || stdout.contains("access denied finding property")
        || stdout.contains("permission denial:")
        || stdout.contains("securityexception")
}

/// Register the mobile `Shell` tool against `reg` — ONLY when the gate passes.
///
/// The gate is `ctx.mobile_shell().is_some_and(|c| c.enabled)`. When the gate
/// is unmet the tool is simply not registered — **absent, not erroring** (spec
/// invariant).
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    if ctx.mobile_shell().is_some_and(|a| a.enabled) {
        reg.register_builtin(Arc::new(ShellMobileTool::new(ctx)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
    use lingxi_core::host::sandbox::{
        Sandbox, SandboxCapability, SandboxFeatures, SandboxedCommand, SandboxedTag,
    };
    use mobile_linux_api::ProcessOutput;
    use mobile_linux_api::SandboxBackend;
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};
    use tool_api::AndroidShellToolCtx;

    /// A `Sandbox` that records the policy handed to `prepare()` and admits the
    /// command (wrapped as `AndroidMinijail`) so the runner is reachable.
    #[derive(Default)]
    struct RecordingSandbox {
        last_network: Mutex<Option<NetworkPolicy>>,
    }

    #[async_trait]
    impl Sandbox for RecordingSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::AndroidMinijail
        }
        fn prepare(
            &self,
            cmd: ProcessCommand,
            policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, SandboxError> {
            *self.last_network.lock().unwrap() = Some(policy.network);
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::Wrapped {
                    backend: SandboxBackend::AndroidMinijail,
                },
            ))
        }
        fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: reason.to_string(),
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

    /// A `ProcessRunner` that records its call count and returns a canned
    /// output.
    struct RecordingRunner {
        out: ProcessOutput,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProcessRunner for RecordingRunner {
        async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.out.clone())
        }
        async fn spawn_background(
            &self,
            _: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            unreachable!()
        }
        async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    struct CancellationRunner {
        started: Arc<Notify>,
        release: Arc<Notify>,
        joined: Arc<AtomicBool>,
        killed_owner: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl ProcessRunner for CancellationRunner {
        async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            unreachable!("ShellMobileTool uses its owned foreground seam")
        }

        async fn run_foreground_with_output_limit(
            &self,
            command: &SandboxedCommand,
            _: Option<usize>,
        ) -> Result<lingxi_core::host::ForegroundRunResult, ProcessError> {
            assert!(command.process_owner().is_some());
            self.started.notify_one();
            self.release.notified().await;
            self.joined.store(true, Ordering::Release);
            Ok(lingxi_core::host::ForegroundRunResult {
                outcome: lingxi_core::host::ForegroundOutcome::Completed(ok_output("finished")),
                output_file: None,
            })
        }

        async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
            *self.killed_owner.lock().expect("killed owner mutex") = Some(owner.to_string());
            self.release.notify_one();
            Vec::new()
        }

        async fn spawn_background(
            &self,
            _: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            unreachable!()
        }

        async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    fn ok_output(stdout: &str) -> ProcessOutput {
        ProcessOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Build a ctx with `android_shell` enabled, swapping in the given mock
    /// sandbox + a runner that returns `out` and records its call count.
    fn enabled_ctx(
        out: ProcessOutput,
    ) -> (BuiltinToolContext, Arc<RecordingSandbox>, Arc<AtomicUsize>) {
        let sandbox = Arc::new(RecordingSandbox::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut ctx = shell_test_ctx(ok_output(""));
        ctx.sandbox = sandbox.clone();
        ctx.process = Arc::new(RecordingRunner {
            out,
            calls: calls.clone(),
        });
        ctx.android_shell = Some(AndroidShellToolCtx::android_legacy(
            true,
            vec!["grep".into(), "sed".into()],
            Some("@(#)MIRBSD KSH".into()),
            false,
        ));
        (ctx, sandbox, calls)
    }

    fn mobile_linux_ctx(
        out: ProcessOutput,
    ) -> (BuiltinToolContext, Arc<RecordingSandbox>, Arc<AtomicUsize>) {
        let sandbox = Arc::new(RecordingSandbox::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut ctx = shell_test_ctx(ok_output(""));
        ctx.sandbox = sandbox.clone();
        ctx.process = Arc::new(RecordingRunner {
            out,
            calls: calls.clone(),
        });
        ctx.android_shell = Some(AndroidShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into(), "apk".into(), "git".into()],
            Some("BusyBox v1.37.0".into()),
        ));
        (ctx, sandbox, calls)
    }

    fn source_input(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("test fixture is valid JSON")
    }

    #[test]
    fn name_is_shell_and_schema_has_command() {
        let (ctx, _, _) = enabled_ctx(ok_output(""));
        let tool = ShellMobileTool::new(ctx);
        assert_eq!(tool.name(), "Shell");
        let schema = tool.input_schema();
        assert_eq!(schema.string_at("/properties/command/type"), Some("string"));
        assert_eq!(schema.string_at("/required/0"), Some("command"));
    }

    #[tokio::test]
    async fn call_builds_deny_net_policy_and_runs() {
        let (ctx, sandbox, calls) = enabled_ctx(ok_output("hi\n"));
        let tool = ShellMobileTool::new(ctx);
        let res = tool
            .call(
                source_input(&json!({"command": "echo hi"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("call should succeed");
        // Mock sandbox recorded a deny-net policy.
        assert_eq!(
            *sandbox.last_network.lock().unwrap(),
            Some(NetworkPolicy::Disabled)
        );
        // The runner ran once and its stdout reached the model.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(res.data.string_at("/stdout"), Some("hi\n"));
        assert_eq!(
            res.data
                .at_pointer("/exit_code")
                .and_then(|value| value.as_number()),
            Some(0.0)
        );
        assert_eq!(
            res.data
                .at_pointer("/is_error")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[tokio::test]
    async fn cancellation_kills_and_joins_the_same_foreground_run() {
        let (mut ctx, _, _) = mobile_linux_ctx(ok_output(""));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let joined = Arc::new(AtomicBool::new(false));
        let killed_owner = Arc::new(Mutex::new(None));
        ctx.process = Arc::new(CancellationRunner {
            started: started.clone(),
            release: release.clone(),
            joined: joined.clone(),
            killed_owner: killed_owner.clone(),
        });
        let cancel = CancellationToken::new();
        let mut tool_ctx = fresh_ctx();
        tool_ctx.cancel = Some(cancel.clone());
        let tool = ShellMobileTool::new(ctx);
        let task = tokio::spawn(async move {
            tool.call(
                source_input(&json!({"command": "printf safe"})),
                tool_ctx,
                fresh_tx(),
            )
            .await
        });

        started.notified().await;
        cancel.cancel();
        let error = task
            .await
            .expect("shell call task")
            .expect_err("abort result");
        assert!(matches!(error, ToolError::Aborted));
        assert!(
            joined.load(Ordering::Acquire),
            "run future must be joined before return"
        );
        let owner = killed_owner
            .lock()
            .expect("killed owner mutex")
            .clone()
            .expect("foreground owner was killed");
        assert!(owner.starts_with("main/mobile-shell-"));
    }

    #[tokio::test]
    async fn mobile_linux_guest_uses_allowed_network_policy_and_runs() {
        let (ctx, sandbox, calls) = mobile_linux_ctx(ok_output("ok\n"));
        let tool = ShellMobileTool::new(ctx);
        let res = tool
            .call(
                source_input(&json!({"command": "apk add git"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("mobile linux command should succeed after outer permission gate");
        assert_eq!(
            *sandbox.last_network.lock().unwrap(),
            Some(NetworkPolicy::Allowed)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(res.data.string_at("/stdout"), Some("ok\n"));
    }

    #[tokio::test]
    async fn guest_shell_network_intent_asks_before_running() {
        // The guest can actually reach the network and `call` deliberately does
        // not refuse there, so the permission gate is the only control point.
        // Before this was wired, check_permissions returned a blanket Allow and
        // an agent-initiated `apk add` ran with no approval at all.
        let (ctx, _, _) = mobile_linux_ctx(ok_output("ok\n"));
        let tool = ShellMobileTool::new(ctx);
        for command in [
            "apk add git",
            "npm i left-pad",
            "git clone https://example.com/r.git",
        ] {
            let decision = tool
                .check_permissions(&source_input(&json!({ "command": command })), &fresh_ctx())
                .await;
            let PermissionResult::Ask { prompt, .. } = decision else {
                panic!("{command:?} must require approval on the guest shell");
            };
            // once / session / always are what plan item 12 asks for.
            assert!(
                prompt.options.len() >= 3,
                "{command:?} -> {:?}",
                prompt.options
            );
        }
    }

    #[tokio::test]
    async fn guest_shell_local_command_is_not_gated() {
        // Over-asking is its own failure: a gate that prompts for `ls` trains
        // the user to approve everything.
        let (ctx, _, _) = mobile_linux_ctx(ok_output("ok\n"));
        let tool = ShellMobileTool::new(ctx);
        for command in ["ls -la", "npm run build", "git status"] {
            let decision = tool
                .check_permissions(&source_input(&json!({ "command": command })), &fresh_ctx())
                .await;
            assert!(
                matches!(decision, PermissionResult::Allow { .. }),
                "{command:?} must not prompt"
            );
        }
    }

    #[tokio::test]
    async fn network_intent_command_is_refused_without_running() {
        let (ctx, _, calls) = enabled_ctx(ok_output(""));
        let tool = ShellMobileTool::new(ctx);
        let err = tool
            .call(
                source_input(&json!({"command": "git clone https://x"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("network-intent command should be refused");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        // The runner must NOT have been called.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn android_host_command_is_refused_without_running() {
        let (ctx, _, calls) = enabled_ctx(ok_output(""));
        let tool = ShellMobileTool::new(ctx);
        let err = tool
            .call(
                source_input(&json!({"command": "monkey -p com.android.chrome 1 2>&1 || true"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("Android host command should be refused");
        let message = match err {
            ToolError::InvalidInput(message) => message,
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(
            message
                .string_at("")
                .unwrap()
                .contains("unavailable in this build"),
            "{}",
            message.string_at("").unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn masked_permission_denial_is_reported_as_error() {
        let output = ProcessOutput {
            stdout: "libc: Access denied finding property \"ro.debuggable\"\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let (ctx, _, calls) = enabled_ctx(output);
        let tool = ShellMobileTool::new(ctx);
        let result = tool
            .call(
                source_input(&json!({"command": "echo safe"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("runner output still returns structured result");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            result
                .data
                .at_pointer("/is_error")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn mobile_prompt_uses_native_precommit_sentence_and_request_carrier() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../shell/tests/fixtures/bash_precommit_286.json"
        ))
        .unwrap();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == "all_three")
            .unwrap();
        let expected = case["outputs"][0]["suggestion"].as_str().unwrap();
        let (ctx, _, _) = enabled_ctx(ok_output(""));
        let tool = ShellMobileTool::new(ctx);
        assert!(!tool
            .prompt(&PromptOptions::default())
            .await
            .string_at("")
            .unwrap()
            .contains("Always run `/"));
        let prompt = tool
            .clone()
            .prompt(&PromptOptions {
                bash_precommit_skills: tool_api::tool_trait::BashPrecommitSkills {
                    custom_verify: true,
                    custom_simplify: true,
                    code_review: true,
                },
                ..Default::default()
            })
            .await;
        let prompt = prompt.string_at("").unwrap();
        assert!(prompt.contains(expected), "{prompt}");
    }

    #[tokio::test]
    async fn prompt_declares_mksh_dialect_and_lists_applets() {
        let (ctx, _, _) = enabled_ctx(ok_output(""));
        let tool = ShellMobileTool::new(ctx);
        let prompt = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
                ..Default::default()
            })
            .await;
        let prompt = prompt.string_at("").unwrap();
        assert!(
            prompt.contains("mksh"),
            "prompt should declare mksh: {prompt}"
        );
        assert!(
            prompt.contains("grep"),
            "prompt should list applets: {prompt}"
        );
        // Non-bundled prompt points at the device's system sh.
        assert!(
            prompt.contains("system"),
            "non-bundled prompt should say system mksh: {prompt}"
        );
        assert!(
            prompt.contains("Host-level device/app automation is unavailable")
                && prompt.contains("/sdcard")
                && !prompt.contains("Android Computer Use is unavailable"),
            "prompt should route host-device work away from Shell without Android-only wording: {prompt}"
        );
    }

    #[tokio::test]
    async fn prompt_reflects_bundled_locked_inventory_when_bundled() {
        let mut ctx = shell_test_ctx(ok_output(""));
        ctx.android_shell = Some(AndroidShellToolCtx::android_legacy(
            true,
            vec!["grep".into(), "sed".into(), "find".into()],
            Some("@(#)MIRBSD KSH R59".into()),
            true,
        ));
        let tool = ShellMobileTool::new(ctx);
        let prompt = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
                ..Default::default()
            })
            .await;
        let prompt = prompt.string_at("").unwrap();
        assert!(prompt.contains("mksh"), "still mksh dialect: {prompt}");
        assert!(prompt.contains("grep"), "lists a bundled applet: {prompt}");
        // Signals the inventory/interpreter is bundled+locked, not the device's system sh:
        assert!(
            prompt.to_lowercase().contains("bundled") || prompt.to_lowercase().contains("locked"),
            "prompt should signal bundled/locked when bundled: {prompt}"
        );
        assert!(
            !prompt.contains("/system/bin/sh"),
            "bundled prompt must not point at system sh: {prompt}"
        );
    }

    #[tokio::test]
    async fn mobile_linux_prompt_mentions_permission_gate_and_no_android_host_copy() {
        let (ctx, _, _) = mobile_linux_ctx(ok_output(""));
        let tool = ShellMobileTool::new(ctx);
        let prompt = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
                ..Default::default()
            })
            .await;
        let prompt_text = prompt
            .string_at("")
            .expect("generated mobile Linux prompt is valid UTF-16");
        assert!(
            prompt_text.contains("basic development environment"),
            "prompt should describe the default capability shape: {prompt:?}"
        );
        assert!(
            prompt_text.contains("one-time, session,")
                && prompt_text.contains("permanent approval"),
            "prompt should describe the approval gate: {prompt:?}"
        );
        assert!(
            prompt_text.contains("iSH/OpenMinis cannot provide a true")
                && prompt_text.contains("advisory"),
            "prompt should state iSH deny-net limitations: {prompt:?}"
        );
        assert!(
            !prompt_text.contains("Android Computer Use is unavailable")
                && !prompt_text.contains("adb/system")
                && !prompt_text.contains("iOS host"),
            "mobile-linux prompt should avoid platform-specific host wording: {prompt:?}"
        );
    }
}
