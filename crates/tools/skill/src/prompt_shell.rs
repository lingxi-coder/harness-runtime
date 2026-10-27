//! Shared host shell-runner + real permission gate + provider for embedded
//! `!`cmd`` expansion in prompt bodies.
//!
//! The three surfaces that expand embedded shell commands in a prompt — the
//! slash-command dispatcher (`/commit`, `/commit-push-pr`, `/security-review`),
//! the ratatui TUI, and the `Skill` tool — all inject the SAME
//! [`command_api::ShellExpansionCtx`] built here, so their behavior is uniform
//! and 1:1 with claude-code's `executeShellCommandsInPrompt`
//! (`utils/promptShellExecution.ts`).
//!
//! ## Pieces
//!
//! * [`PromptShellRunner`] — the real host runner, relocated VERBATIM from
//!   `tools/skill/src/skill.rs` (it was already general-purpose: every field is
//!   cloned straight off [`BuiltinToolContext`] with no skill state). A 1:1
//!   re-impl of `BashTool::call`: extglob guard + `should_use_sandbox` +
//!   `sandbox_runner.wrap` + `process.run` inside `sandbox.bypass_with_audit`.
//! * [`PolicyShellPermissionGate`] — the REAL gate. Faithful port of
//!   `hasPermissionsToUseTool(BashTool, {command})`: calls
//!   [`permission::PermissionPolicy::authorize_with_mode`] and folds the 3-valued
//!   [`permission::PermissionResult`] to the 2-valued
//!   [`command_api::ShellPermissionDecision`] — `Allow`→`Allow`,
//!   `Deny{explanation}`→`Deny{explanation}`, `Ask{prompt}`→`Deny{prompt.message}`
//!   (TS treats `behavior !== 'allow'` — BOTH `ask` and `deny` — as a thrown
//!   `MalformedCommandError`). Non-interactive: an `Ask` is never surfaced as a
//!   UI prompt.
//! * [`PromptShellExpansionProvider`] — the per-command factory. Each
//!   [`command_api::ShellExpansionProvider::build`] call bakes THAT command's
//!   declared `allowed_tools` into a FRESH effective policy (base rules + parsed
//!   allow rules in the `Command` source bucket), mirroring claude-code building
//!   a fresh `toolPermissionContext` per `getPromptForCommand` call — never
//!   mutating the shared boot policy.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

use command_api::{
    FrontmatterShell, GitWorktreeContext, ShellExpansionCtx, ShellExpansionProvider, ShellOut,
    ShellPermissionDecision, ShellPermissionGate, ShellRunError, ShellRunner,
};
use permission::{
    PermissionBehavior, PermissionMode, PermissionPolicy, PermissionResult, PermissionRule,
    PermissionRuleSource, PermissionRuleValue,
};
use tool_api::builtin_context::BuiltinToolContext;

/// The real host runner for an embedded prompt `!command`.
///
/// Relocated verbatim from `tools/skill/src/skill.rs` (the former
/// `SkillShellRunner`) so every prompt-expansion surface (dispatcher / TUI /
/// skill) shares ONE host runner. Runtime fields are cloned straight off a
/// [`BuiltinToolContext`] (mirrors what `BashTool::call` reads off `self.ctx`)
/// — there is no skill-specific input, so it is constructible from any
/// `&BuiltinToolContext`.
///
/// Divergence (documented): the adapter carries no `AnalyticsBus`, so the
/// `sandbox_refused` / `sandbox_wrap_failed` telemetry events bash emits on the
/// refuse/wrap-failure branches are skipped here; the failure is still surfaced
/// as a [`command_api::ShellRunError`] (the TS `errorMessage(e)` generic path)
/// so the engine formats `[Error]\n…` and the command does NOT run. The
/// Windows-CMD `2>nul` rewrite is omitted (bash refuses on Windows outright);
/// the BASH.4 persistent-cwd `pwd -P` readback is omitted (one-shot expansion
/// keeps no shell-cwd state).
pub struct PromptShellRunner {
    side_chat: bool,
    cleanup_failed: Option<Arc<std::sync::atomic::AtomicBool>>,
    owner: Option<String>,
    cancel: Option<lingxi_core::host::CancellationToken>,
    process: Arc<dyn lingxi_core::host::process::ProcessRunner>,
    sandbox: Arc<dyn lingxi_core::host::sandbox::Sandbox>,
    workspace: std::path::PathBuf,
    // ===== Sandbox-decision inputs, captured from the `BuiltinToolContext`
    // (mirrors what `BashTool::call` reads off `self.ctx`). 1:1 with claude-code
    // `shouldUseSandbox.ts`, which has NO permission-mode, project-trust, or
    // classifier inputs — only host availability, the `dangerouslyDisableSandbox`
    // override, and the `excludedCommands` config.
    /// Whether the host has a working sandbox backend (`bash.rs`).
    sandbox_available: bool,
    /// Sandbox policy runtime config — supplies `excludedCommands` to the
    /// decision and drives the wrap (`bash.rs`).
    sandbox_runtime: sandbox::runtime_config::SandboxRuntimeConfig,
    /// Detected platform — selects the wrap branch (`bash.rs:533`).
    platform: sandbox::runtime_config::Platform,
    /// Injected async sandbox seam, threaded from the `BuiltinToolContext` so
    /// embedded `!command` expansion wraps through the same runner the Bash tool
    /// uses (default `LegacyWrapRunner` = byte-identical to the previous direct
    /// `wrap_with_sandbox` call).
    sandbox_runner: Arc<dyn tool_api::SandboxRunner>,
    /// Mobile-only override: execute via the platform sandbox/process adapters
    /// instead of `bypass_with_audit`, so mobile-linux guest shells route
    /// through the injected runtime bridge.
    force_platform_sandbox: bool,
    /// Executable selected by the actual registered shell carrier. Desktop
    /// keeps the existing zsh/bash resolver; mobile uses its `/bin/sh` or
    /// `/system/bin/sh` carrier and never assumes Bash exists.
    shell_path: String,
    /// Mobile shells run one-shot `-c` commands and do not load desktop login
    /// profiles/snapshots.
    mobile_shell: bool,
    /// Whether the mobile carrier is actually registered for this engine. A
    /// disabled carrier must fail closed instead of falling back to host Bash.
    shell_enabled: bool,
    /// One lazily-created shell snapshot shared by every command expansion in
    /// this session/provider.
    snapshot: Arc<ShellSnapshot>,
}

/// Lazily materialized aliases/functions/options from the user's interactive
/// shell. The private directory is removed when the session provider drops.
struct ShellSnapshot {
    dir: PathBuf,
    path: PathBuf,
    initialized: tokio::sync::OnceCell<bool>,
}

impl ShellSnapshot {
    fn new() -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("lingxi-shell-snapshot-{}-{id}", std::process::id()));
        Self {
            path: dir.join("snapshot.sh"),
            dir,
            initialized: tokio::sync::OnceCell::new(),
        }
    }

    async fn ensure(
        &self,
        shell_path: &str,
        process: &dyn lingxi_core::host::process::ProcessRunner,
        sandbox: &dyn lingxi_core::host::sandbox::Sandbox,
        workspace: &Path,
    ) -> Option<&Path> {
        use lingxi_core::host::sandbox::ProcessCommand;

        let ready = self
            .initialized
            .get_or_init(|| async {
                if std::fs::create_dir(&self.dir).is_err() {
                    return false;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                        .is_err()
                    {
                        return false;
                    }
                }

                let rc_file = if shell_path.contains("zsh") {
                    "\"$HOME/.zshrc\""
                } else {
                    "\"$HOME/.bashrc\""
                };
                // Options + functions. The one-line-per-entry OPTION sections
                // are capped at 1000 lines (oracle `head -n 1000`); the
                // multi-line `typeset -f`/`declare -f` function dump is left
                // uncapped since a line cap would truncate a definition mid-body.
                // For bash, append `shopt -s expand_aliases` AFTER `shopt -p`
                // (which captures the non-interactive default `shopt -u
                // expand_aliases`), so alias expansion is ON when the snapshot is
                // sourced — otherwise the subsequent `eval` never expands any
                // alias (oracle: `echo "shopt -s expand_aliases" >> snapshot`).
                let functions = if shell_path.contains("zsh") {
                    "typeset -f; setopt | sed 's/^/setopt /' | head -n 1000"
                } else {
                    "declare -f; shopt -p | head -n 1000; set +o | head -n 1000; \
                     echo 'shopt -s expand_aliases'"
                };
                // Normalize the alias dump so BOTH shells emit real
                // `alias -- name=value` definitions (oracle `alias | sed
                // 's/^alias //g' | sed 's/^/alias -- /' | head -n 1000`). Without
                // this, zsh's prefix-less `name='value'` lines source as stray
                // VARIABLE assignments, not aliases.
                let script = format!(
                    "if [ -f {rc_file} ]; then . {rc_file} </dev/null; fi; \
                     {{ printf '%s\\n' '# LingXi shell snapshot' \
                        'unalias -a 2>/dev/null || true'; \
                        {functions}; \
                        alias | sed 's/^alias //g' | sed 's/^/alias -- /' | head -n 1000; \
                        printf 'export PATH=%q\\n' \"$PATH\"; \
                     }} > {}",
                    shell_quote(&self.path.to_string_lossy())
                );
                let mut env = HashMap::new();
                env.insert("SHELL".to_string(), shell_path.to_string());
                env.insert("GIT_EDITOR".to_string(), "true".to_string());
                env.insert("CLAUDECODE".to_string(), "1".to_string());
                let command = ProcessCommand {
                    command: shell_path.to_string(),
                    args: vec!["-c".to_string(), "-l".to_string(), script],
                    cwd: Some(workspace.to_path_buf()),
                    env,
                    timeout: Some(std::time::Duration::from_secs(10)),
                    stdin: None,
                };
                let command = sandbox.bypass_with_audit(command, "prompt_shell_snapshot");
                matches!(process.run(&command).await, Ok(out) if !out.timed_out && out.exit_code == 0)
                    && self.path.is_file()
            })
            .await;

        (*ready && self.path.is_file()).then_some(self.path.as_path())
    }
}

impl Drop for ShellSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command_with_snapshot(command: &str, shell_path: &str, snapshot: Option<&Path>) -> String {
    let guarded = match disable_extglob(shell_path) {
        Some(prefix) => format!("{prefix} && {command}"),
        None => command.to_string(),
    };
    match snapshot {
        Some(path) => format!(
            ". {} 2>/dev/null || true; eval {}",
            shell_quote(&path.to_string_lossy()),
            shell_quote(&guarded)
        ),
        None => guarded,
    }
}

/// Resolve the login shell exactly like `bash.rs::resolve_shell_path`
/// (`/bin/zsh` on macOS, `/bin/bash` elsewhere). Public so tests that assert on
/// the runner's spawned shell can reference the single source of truth.
#[must_use]
pub fn resolve_shell_path() -> &'static str {
    if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/bash"
    }
}

/// BASH.1 extglob-disable guard, 1:1 with `bash.rs::disable_extglob_command`.
fn disable_extglob(shell_path: &str) -> Option<String> {
    if std::env::var("LINGXI_SHELL_PREFIX").is_ok_and(|v| !v.is_empty()) {
        return Some(
            "{ shopt -u extglob || setopt NO_EXTENDED_GLOB; } >/dev/null 2>&1 || true".into(),
        );
    }
    if shell_path.contains("bash") {
        Some("shopt -u extglob 2>/dev/null || true".into())
    } else if shell_path.contains("zsh") {
        Some("setopt NO_EXTENDED_GLOB 2>/dev/null || true".into())
    } else {
        None
    }
}

async fn run_prompt_shell_process(
    process: Arc<dyn lingxi_core::host::process::ProcessRunner>,
    command: lingxi_core::host::sandbox::SandboxedCommand,
    cancellation: lingxi_core::host::CancellationToken,
    owner_prefix: Option<&str>,
) -> Result<lingxi_core::host::process::ForegroundRunResult, mobile_linux_api::ProcessError> {
    let child_owner = format!("prompt-shell-{}", tool_api::util::ids::ulid_or_uuid());
    let owner = owner_prefix.map_or(child_owner.clone(), |prefix| {
        format!("{prefix}/{child_owner}")
    });
    let command = command
        .with_process_owner(Some(owner.clone()))
        .with_auto_background_on_timeout(false);
    let mut run = Box::pin(process.run_foreground_with_output_limit(&command, None));
    let mut cancellation_seen = false;
    let mut kill_tick = tokio::time::interval(Duration::from_millis(100));
    kill_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            result = &mut run => return result,
            _ = cancellation.cancelled(), if !cancellation_seen => cancellation_seen = true,
            _ = kill_tick.tick(), if cancellation_seen => {
                let _ = process.kill_owner_processes(&owner).await;
            }
        }
    }
}

#[async_trait]
impl ShellRunner for PromptShellRunner {
    async fn run(
        &self,
        command: &Utf16JsonProjection,
        _shell: Option<FrontmatterShell>,
        cancellation: lingxi_core::host::CancellationToken,
    ) -> Result<ShellOut, ShellRunError> {
        if self.side_chat
            && self
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
        {
            return Err(ShellRunError {
                stdout: String::new(),
                stderr: String::new(),
                interrupted: true,
                generic_message: None,
            });
        }
        if self.side_chat && self.owner.is_none() {
            return Err(ShellRunError {
                stdout: String::new(),
                stderr: String::new(),
                interrupted: false,
                generic_message: Some("Side Chat shell owner is absent".into()),
            });
        }
        use lingxi_core::host::sandbox::ProcessCommand;
        use sandbox::decision::{should_use_sandbox, SandboxDecision};

        // This is the real host process boundary. TextEncoder replaces only
        // unpaired UTF-16 units with U+FFFD; valid pairs and all other source
        // units are preserved in the executable command.
        if cancellation.is_cancelled() {
            return Err(ShellRunError {
                stdout: String::new(),
                stderr: String::new(),
                interrupted: true,
                generic_message: None,
            });
        }

        let command_text = String::from_utf16_lossy(
            &command
                .string_units("")
                .expect("shell runner receives a root-string command"),
        );

        if self.mobile_shell && !self.shell_enabled {
            return Err(ShellRunError {
                stdout: String::new(),
                stderr: String::new(),
                interrupted: false,
                generic_message: Some(
                    "Mobile Shell is unavailable; prompt commands cannot run on the host shell"
                        .into(),
                ),
            });
        }

        let shell_path = self.shell_path.as_str();
        let snapshot = if self.mobile_shell || self.side_chat {
            None
        } else {
            self.snapshot
                .ensure(
                    shell_path,
                    self.process.as_ref(),
                    self.sandbox.as_ref(),
                    &self.workspace,
                )
                .await
        };
        // Snapshot mode sources the captured aliases/functions/options and uses
        // `eval` so aliases expand. If snapshot creation/access failed, retain
        // the login-shell fallback.
        let spawn_cmd = command_with_snapshot(&command_text, shell_path, snapshot);

        // ===== Sandbox decision (mirror of `BashTool::call`) =====
        // A prompt `!command` has NO per-command `dangerouslyDisableSandbox` flag
        // (that is a Bash-tool *input* field; prompt bodies have no such surface),
        // so we pass `false` for that override and otherwise feed the same inputs
        // bash does — 1:1 with claude-code `shouldUseSandbox.ts`. `unsandboxed_allowed`
        // is the canonical `are_unsandboxed_commands_allowed()` accessor.
        let decision = should_use_sandbox(
            &command_text,
            self.sandbox_available,
            /* dangerously_disable_sandbox */ false,
            self.sandbox_runtime.are_unsandboxed_commands_allowed(),
            &self.sandbox_runtime,
            self.workspace.clone(),
        );
        let inner = match decision {
            // No sandbox: run the (extglob-guarded) command unchanged.
            SandboxDecision::NoSandbox => spawn_cmd,
            // Wrap the command string for the sandbox; on failure surface a
            // `ShellRunError` (the TS `errorMessage(e)` generic path) so the
            // command does NOT run. Bash splits this into two arms purely to emit
            // distinct telemetry (`Unsupported` -> `sandbox_refused`,
            // `SbplWrite` -> `sandbox_wrap_failed`) and to pick `InvalidInput` vs
            // `Io`; this adapter has no `AnalyticsBus` and a single `ShellRunError`
            // surface, so both `SandboxWrapError` variants collapse to the same
            // generic failure (the inner string is preserved verbatim — the same
            // string bash surfaces). See the struct doc.
            SandboxDecision::Sandbox { policy: _ } => {
                // Wrap through the injected async `SandboxRunner` (same seam the
                // Bash tool uses), rooted at the session workspace the prompt
                // shell runs in.
                let rooted_runtime = sandbox::root::rooted_at(
                    &self.sandbox_runtime,
                    &self.workspace,
                    sandbox::root::SandboxRootScope::Session,
                );
                match self
                    .sandbox_runner
                    .wrap(
                        &spawn_cmd,
                        &rooted_runtime,
                        self.platform,
                        Some(shell_path),
                        Some(self.workspace.as_path()),
                    )
                    .await
                {
                    Ok(wrapped) => wrapped,
                    Err(
                        sandbox::wrap::SandboxWrapError::Unsupported(s)
                        | sandbox::wrap::SandboxWrapError::SbplWrite(s),
                    ) => {
                        return Err(ShellRunError {
                            stdout: String::new(),
                            stderr: String::new(),
                            interrupted: false,
                            generic_message: Some(s),
                        });
                    }
                }
            }
        };

        let pcmd = ProcessCommand {
            command: shell_path.to_string(),
            // A valid snapshot replaces per-command login-shell startup. If the
            // snapshot vanished or failed to build, preserve the `-c -l` fallback.
            args: if self.mobile_shell || snapshot.is_some() {
                vec!["-c".into(), inner]
            } else {
                vec!["-c".into(), "-l".into(), inner]
            },
            cwd: Some(self.workspace.clone()),
            env: HashMap::new(),
            timeout: self.side_chat.then(|| std::time::Duration::from_secs(120)),
            stdin: None,
        };
        let sandboxed = if self.force_platform_sandbox {
            let policy = lingxi_core::host::sandbox::SandboxPolicy {
                network: mobile_linux_api::NetworkPolicy::Disabled,
                writable_paths: vec![],
                denied_paths: vec![],
                allow_subprocess: true,
                limits: mobile_linux_api::ResourceLimits::default(),
            };
            self.sandbox
                .prepare(pcmd, &policy)
                .map_err(|e| ShellRunError {
                    stdout: String::new(),
                    stderr: String::new(),
                    interrupted: false,
                    generic_message: Some(format!("{e}")),
                })?
        } else {
            self.sandbox
                .bypass_with_audit(pcmd, "prompt_shell_expansion")
        };
        if cancellation.is_cancelled() {
            self.sandbox_runner.cleanup_after_command().await;
            return Err(ShellRunError {
                stdout: String::new(),
                stderr: String::new(),
                interrupted: true,
                generic_message: None,
            });
        }
        let owner_prefix = if self.side_chat {
            self.owner.as_deref()
        } else {
            None
        };
        let run_result = run_prompt_shell_process(
            self.process.clone(),
            sandboxed,
            cancellation.clone(),
            owner_prefix,
        )
        .await;
        // The wrapped command has finished and been joined: tear down any
        // per-command sandbox state. No-op for the default `LegacyWrapRunner`.
        self.sandbox_runner.cleanup_after_command().await;
        if let Err(error) = &run_result {
            if self.side_chat
                && error
                    .to_string()
                    .contains(lingxi_core::host::process::SIDE_CHAT_CLEANUP_FAILURE)
            {
                if let Some(flag) = &self.cleanup_failed {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        match run_result {
            Ok(result) => match result.outcome {
                lingxi_core::host::process::ForegroundOutcome::Completed(out)
                    if cancellation.is_cancelled() || out.timed_out =>
                {
                    Err(ShellRunError {
                        stdout: out.stdout,
                        stderr: out.stderr,
                        interrupted: true,
                        generic_message: None,
                    })
                }
                // Non-zero exit is NOT an error here — TS `BashTool.call` returns
                // stdout/stderr without throwing on a non-zero status; only an
                // interruption throws. So every completed run yields `ShellOut`.
                lingxi_core::host::process::ForegroundOutcome::Completed(out) => Ok(ShellOut {
                    stdout: out.stdout,
                    stderr: out.stderr,
                    interrupted: false,
                }),
                lingxi_core::host::process::ForegroundOutcome::MovedToBackground(handle) => {
                    let _ = self.process.kill(&handle).await;
                    Err(ShellRunError {
                        stdout: String::new(),
                        stderr: String::new(),
                        interrupted: true,
                        generic_message: None,
                    })
                }
            },
            // A process error produced by the owner kill is still the
            // interruption path; unrelated spawn/I/O failures remain generic.
            Err(error)
                if cancellation.is_cancelled()
                    && !error
                        .to_string()
                        .contains(lingxi_core::host::process::SIDE_CHAT_CLEANUP_FAILURE) =>
            {
                Err(ShellRunError {
                    stdout: String::new(),
                    stderr: String::new(),
                    interrupted: true,
                    generic_message: None,
                })
            }
            // Spawn / I/O failure -> the TS `errorMessage(e)` generic path
            // (`[Error]\n{message}`).
            Err(e) => Err(ShellRunError {
                stdout: String::new(),
                stderr: String::new(),
                interrupted: false,
                generic_message: Some(format!("{e}")),
            }),
        }
    }
}

/// The REAL per-command permission gate for embedded prompt `!command`s.
///
/// Holds THAT command's effective [`PermissionPolicy`] (base rules + the
/// command's injected `allowed_tools`) and the live mode, and folds
/// [`PermissionPolicy::authorize_with_mode`]'s 3-valued
/// [`PermissionResult`] down to the 2-valued
/// [`ShellPermissionDecision`]:
///
/// * `Allow` → `Allow`
/// * `Deny { explanation }` → `Deny { message: explanation }`
/// * `Ask { prompt }` → `Deny { message: Some(prompt.message) }`
///
/// The `Ask → Deny` fold is REQUIRED for parity: claude-code's
/// `executeShellCommandsInPrompt` treats any `behavior !== 'allow'` (both `ask`
/// and `deny`) as a thrown `MalformedCommandError`. The check is SYNC and
/// prompt-free — an `Ask` verdict is never surfaced as a UI prompt.
struct PolicyShellPermissionGate {
    /// The per-command effective policy (base rules + this command's
    /// `allowed_tools`, in the `Command` source bucket).
    policy: Arc<PermissionPolicy>,
    /// The live permission mode to authorize under (threaded so a session that
    /// has entered plan-mode is honored, not the stale boot mode).
    mode: PermissionMode,
    /// The session's enforcing gate. When present and rule-aware, this is the
    /// source of truth for the LIVE mode and `updatedPermissions` overlay.
    live_gate: Option<Arc<dyn lingxi_core::host::permission_gate::PermissionGate>>,
    /// Frontmatter allow rules injected only for this prompt command.
    transient_allow_rules: Vec<String>,
    /// Actual registered command tool name (`Shell` on mobile, otherwise the
    /// frontmatter-selected desktop shell tool).
    mobile_shell: bool,
    shell_enabled: bool,
}

impl ShellPermissionGate for PolicyShellPermissionGate {
    fn check(
        &self,
        command: &Utf16JsonProjection,
        shell: Option<FrontmatterShell>,
    ) -> ShellPermissionDecision {
        if self.mobile_shell && !self.shell_enabled {
            return ShellPermissionDecision::Deny {
                message: Some(
                    "Mobile Shell is unavailable; prompt commands cannot run on the host shell"
                        .into(),
                ),
            };
        }
        // Shell selection is frontmatter-only (never settings.defaultShell):
        // `powershell` selects the PowerShell tool ONLY when it is enabled (the
        // Windows host), else fall back to Bash (promptShellExecution.ts:80-83).
        // Builtins pass `None` → Bash.
        let tool_name = if self.mobile_shell {
            "Shell"
        } else {
            match shell {
                Some(FrontmatterShell::PowerShell) if cfg!(target_os = "windows") => "PowerShell",
                _ => "Bash",
            }
        };
        let input = Utf16JsonProjection::object(vec![("command".into(), command.clone())])
            .expect("shell permission input is a JSON object");
        if let Some(decision) = self.live_gate.as_ref().and_then(|gate| {
            gate.check_noninteractive_with_allow_rules(
                tool_name,
                &input,
                &self.transient_allow_rules,
            )
        }) {
            return match decision {
                lingxi_core::host::permission_gate::NonInteractivePermissionDecision::Allow => {
                    ShellPermissionDecision::Allow
                }
                lingxi_core::host::permission_gate::NonInteractivePermissionDecision::Deny {
                    reason,
                } => ShellPermissionDecision::Deny { message: reason },
            };
        }
        match self
            .policy
            .authorize_with_mode(tool_name, &input, self.mode)
        {
            PermissionResult::Allow { .. } => ShellPermissionDecision::Allow,
            PermissionResult::Deny { explanation, .. } => ShellPermissionDecision::Deny {
                message: explanation,
            },
            PermissionResult::Ask { prompt, .. } => ShellPermissionDecision::Deny {
                message: Some(prompt.message),
            },
        }
    }
}

/// Build a FRESH effective policy = `base`'s rules/roots/sandbox/working-dirs +
/// `allowed_tools` injected as `Command`-source ALLOW rules.
///
/// Mirrors claude-code injecting a command's own `allowedTools` into a fresh
/// `toolPermissionContext.alwaysAllowRules.command` per `getPromptForCommand`
/// call, rather than mutating the shared boot policy. The full base rule set
/// (allow + deny + ask + the `Auto`-stripped-dangerous allow rules) is
/// reconstituted through [`PermissionPolicy::from_rules`], then the base's
/// roots / sandbox-auto-allow config / additional-working-dirs / bypass flags
/// are re-applied so shell content matching (`Bash(git add:*)` vs the command),
/// the sandbox auto-allow layer, and the read-only auto-allow all behave exactly
/// as they do for a model-initiated Bash call.
fn build_effective_policy(
    base: &PermissionPolicy,
    mode: PermissionMode,
    allowed_tools: &[String],
) -> PermissionPolicy {
    let mut rules: Vec<PermissionRule> = Vec::new();
    for bucket in [&base.allow_rules, &base.deny_rules, &base.ask_rules] {
        for rs in bucket.values() {
            rules.extend(rs.iter().cloned());
        }
    }
    // The `Auto`-stripped allow rules, so a strip→rebuild is mode-consistent
    // (re-added here, then re-stripped by `from_rules`'s `set_mode` iff `mode`
    // is `Auto`; a no-op in every other mode where `stripped_dangerous` is
    // empty).
    rules.extend(base.stripped_dangerous.iter().cloned());
    // Inject THIS command's declared allow-list as `Command`-source allow rules
    // (claude-code `alwaysAllowRules.command`). `from_rule_string` collapses a
    // `Tool()` / `Tool(*)` spec to a tool-wide rule and unescapes content.
    for spec in allowed_tools {
        rules.push(PermissionRule {
            value: PermissionRuleValue::from_rule_string(spec),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Command,
        });
    }
    let mut policy = PermissionPolicy::from_rules(mode, rules);
    if let Some(roots) = base.roots.clone() {
        policy = policy.with_roots(roots);
    }
    if let Some(sandbox_runtime) = base.sandbox_runtime.clone() {
        policy = policy.with_sandbox_runtime(sandbox_runtime);
    }
    if let Some(pwsh_parser) = base.pwsh_parser.clone() {
        policy = policy.with_pwsh_parser(pwsh_parser);
    }
    policy = policy.with_working_dirs(base.additional_working_dirs.clone());
    policy = policy
        .with_block_reads_outside_working_directories(base.block_reads_outside_working_directories);
    policy = policy.with_bypass_available(base.bypass_permissions_available);
    policy = policy.with_interactive_session(base.interactive_session);
    policy = policy.with_restricted(base.restricted);
    policy.bypass_killswitch_active = base.bypass_killswitch_active;
    policy.auto_mode_disabled = base.auto_mode_disabled;
    policy
}

/// Per-command [`ShellExpansionCtx`] factory closing over a
/// [`BuiltinToolContext`] (which now carries the base
/// [`PermissionPolicy`](BuiltinToolContext::permission_policy)).
///
/// Each [`ShellExpansionProvider::build`] call assembles the real host runner
/// plus a [`PolicyShellPermissionGate`] backed by a FRESH per-command effective
/// policy — so the three prompt-expansion surfaces (dispatcher / TUI / skill)
/// share one uniform, 1:1 gate.
pub struct PromptShellExpansionProvider {
    ctx: BuiltinToolContext,
    snapshot: Arc<ShellSnapshot>,
}

/// Convert the current host path into the same source-preserving text carrier
/// used by the Native prompt fallback. Windows paths retain their raw UTF-16
/// units; Unix paths use the platform's UTF-8 replacement behavior.
fn cwd_source(path: &Path) -> Utf16JsonProjection {
    #[cfg(windows)]
    let units = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().collect::<Vec<_>>()
    };

    #[cfg(not(windows))]
    let units = path.to_string_lossy().encode_utf16().collect::<Vec<_>>();

    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("cwd display text is derived from its UTF-16 source units")
}

/// Recover the path that Native receives after its host cwd has crossed the
/// JavaScript string boundary. On Unix, non-UTF-8 OS bytes become U+FFFD in
/// `process.cwd()` and are re-encoded as UTF-8 when passed back to `fs` or a
/// child process; using the original raw `PathBuf` would access a different
/// path. Windows `cwd_source` retains the original UTF-16 units, so preserve
/// the native `OsString` path there.
fn native_cwd_path(_path: &Path, source: &Utf16JsonProjection) -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(
            source
                .string_at("")
                .expect("cwd projection is a root string"),
        )
    }
    #[cfg(windows)]
    {
        let _ = source;
        _path.to_path_buf()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = source;
        _path.to_path_buf()
    }
}

/// Native hr(cwd) discovers the nearest .git entry before attempting the Git
/// query. A normal file or directory is enough; the contents are not
/// validated. For symlinks, Native first rejects network/device/automount
/// paths, then walks each path component with lstat and recursively expands
/// safe links. It does not impose general containment under the marker's
/// directory: a normal local target outside that directory is accepted.
///
/// Source anchors: Ue at 188254289 (src_188187055.js), Wi at 186078708
/// (src_186068928.js), Me at 188271316, and me at 188272365. The two chunks
/// are pinned in the r31 extracted-source manifest.
fn has_native_git_root_marker(cwd: &Path) -> bool {
    let mut current = cwd;
    loop {
        let marker = current.join(".git");
        if native_git_marker_entry(&marker, current) {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        if parent == current {
            return false;
        }
        current = parent;
    }
}

fn native_git_marker_entry(marker: &Path, containing_dir: &Path) -> bool {
    let Ok(link_metadata) = std::fs::symlink_metadata(marker) else {
        return false;
    };
    let file_type = link_metadata.file_type();
    if file_type.is_file() || file_type.is_dir() {
        return true;
    }
    if !file_type.is_symlink() {
        return false;
    }
    let Some(target_text) = native_symlink_target(marker) else {
        return false;
    };
    if native_git_link_path_is_unsafe(&target_text, containing_dir, None) {
        return false;
    }
    if !native_git_link_components_are_safe(&target_text, containing_dir, None) {
        return false;
    }
    std::fs::metadata(marker).is_ok_and(|metadata| metadata.is_file() || metadata.is_dir())
}

fn native_git_link_path_is_unsafe(target: &str, base: &Path, home_automount: Option<bool>) -> bool {
    let target_units = target.encode_utf16().collect::<Vec<_>>();
    let base_units = native_path_units(base);
    let resolved = native_resolve_path(base, Path::new(target));
    let resolved_units = native_path_units(&resolved);

    if native_gse(&target_units, &base_units) || native_gse(&resolved_units, &base_units) {
        return true;
    }

    let base_volume = native_path_volume_id(&base_units);
    for path in [&target_units, &resolved_units] {
        if native_path_volume_root(path).is_some() {
            let Some(path_volume) = native_path_volume_id(path) else {
                return true;
            };
            if Some(path_volume.as_slice()) != base_volume.as_deref() {
                return true;
            }
        }
        if native_has_network_component(path)
            || native_is_net_root(path)
            || native_is_special_volume_path(path)
            || native_is_automount_path(path, &base_units, home_automount)
        {
            return true;
        }
    }
    false
}

/// Source me checks the caller's joined path and then path.resolve(base,
/// target). Each check walks with lstat, rejects a link target before
/// following it, and recurses at most 40 links deep. A missing tail is safe
/// here; Ue's final stat decides whether the marker resolves to a file/dir.
fn native_git_link_components_are_safe(
    target: &str,
    base: &Path,
    home_automount: Option<bool>,
) -> bool {
    let target_path = Path::new(target);
    let joined_units = if target.starts_with('/') {
        target.encode_utf16().collect::<Vec<_>>()
    } else {
        let mut units = native_path_units(base);
        units.push(std::path::MAIN_SEPARATOR as u16);
        units.extend(target.encode_utf16());
        units
    };
    let Some(joined) = native_path_from_units(&joined_units) else {
        return false;
    };
    if !native_path_walk_is_safe(&joined, base, 40, home_automount) {
        return false;
    }

    let resolved = native_resolve_path(base, target_path);
    if native_path_units(&resolved) == joined_units {
        return true;
    }
    native_path_walk_is_safe(&resolved, base, 40, home_automount)
}

fn native_path_walk_is_safe(
    path: &Path,
    base: &Path,
    depth: u8,
    home_automount: Option<bool>,
) -> bool {
    if depth == 0 {
        return false;
    }

    let path_units = native_path_units(path);
    let rooted = path_units.first() == Some(&(b'/' as u16));
    let remainder = if rooted {
        &path_units[1..]
    } else {
        &path_units[..]
    };
    let components = native_split(remainder, false);
    let mut current = if rooted {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for (index, component) in components.iter().enumerate() {
        if component.is_empty() || native_units_eq_ascii(component, ".") {
            continue;
        }
        if native_units_eq_ascii(component, "..") {
            current.pop();
            continue;
        }
        let Some(component) = native_path_from_units(component) else {
            return false;
        };
        current.push(component);
        current = native_normalize_path(&current);

        let current_units = native_path_units(&current);
        let base_units = native_path_units(base);
        if native_path_is_unsafe(&current_units, &base_units, home_automount)
            || native_has_network_component(&current_units)
        {
            return false;
        }

        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::NotADirectory =>
            {
                continue;
            }
            Err(_) => return false,
        };
        let file_type = metadata.file_type();
        if file_type.is_file() || file_type.is_dir() {
            continue;
        }
        if !file_type.is_symlink() {
            return false;
        }

        let Some(link_text) = native_symlink_target(&current) else {
            return false;
        };
        let Some(parent) = current.parent() else {
            return false;
        };
        if native_git_link_path_is_unsafe(&link_text, parent, home_automount) {
            return false;
        }

        let mut expanded_units = if link_text.starts_with('/') {
            link_text.encode_utf16().collect::<Vec<_>>()
        } else {
            let mut units = native_path_units(parent);
            units.push(std::path::MAIN_SEPARATOR as u16);
            units.extend(link_text.encode_utf16());
            units
        };
        let tail = components
            .iter()
            .skip(index + 1)
            .filter(|component| !component.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        if !tail.is_empty() {
            if !expanded_units.ends_with(&[b'/' as u16]) {
                expanded_units.push(std::path::MAIN_SEPARATOR as u16);
            }
            for (tail_index, component) in tail.iter().enumerate() {
                if tail_index > 0 {
                    expanded_units.push(std::path::MAIN_SEPARATOR as u16);
                }
                expanded_units.extend(component);
            }
        }
        let Some(expanded) = native_path_from_units(&expanded_units) else {
            return false;
        };
        return native_path_walk_is_safe(&expanded, base, depth - 1, home_automount);
    }
    true
}

fn native_path_is_unsafe(path: &[u16], base: &[u16], home_automount: Option<bool>) -> bool {
    if native_is_special_volume_path(path) {
        return true;
    }
    if native_path_volume_root(path).is_some() {
        let Some(path_volume) = native_path_volume_id(path) else {
            return true;
        };
        if Some(path_volume.as_slice()) != native_path_volume_id(base).as_deref() {
            return true;
        }
    }
    native_is_automount_path(path, base, home_automount)
}

/// Source Wi: reject device paths, UNC host changes, network mounts, special
/// volume paths, and autofs roots before any filesystem walk of the target.
fn native_gse(path: &[u16], base: &[u16]) -> bool {
    if !native_is_absolute_source_path(path) {
        return false;
    }
    if native_is_device_prefix(path) || native_is_nt_path(path) || native_has_dot_alias(path) {
        return true;
    }
    match native_unc_host(path) {
        Some(host) => Some(host) != native_unc_host(base),
        None => true,
    }
}

fn native_is_absolute_source_path(path: &[u16]) -> bool {
    native_starts_with_two_separators(path) || native_is_nt_path(path)
}

fn native_starts_with_two_separators(path: &[u16]) -> bool {
    path.len() >= 2 && native_is_separator(path[0]) && native_is_separator(path[1])
}

fn native_is_separator(unit: u16) -> bool {
    unit == b'/' as u16 || unit == b'\\' as u16
}

fn native_is_device_prefix(path: &[u16]) -> bool {
    native_starts_with_two_separators(path)
        && path.len() >= 4
        && matches!(path[2], 0x2e | 0x3f)
        && native_is_separator(path[3])
}

fn native_is_nt_path(path: &[u16]) -> bool {
    fn has_nt_prefix(path: &[u16]) -> bool {
        path.len() >= 4
            && native_is_separator(path[0])
            && path[1] == b'?' as u16
            && path[2] == b'?' as u16
            && native_is_separator(path[3])
    }
    if has_nt_prefix(path) {
        return true;
    }
    if !path
        .windows(2)
        .any(|pair| pair == [b'?' as u16, b'?' as u16])
    {
        return false;
    }
    has_nt_prefix(&native_win32_normalize(path))
}

fn native_win32_normalize(path: &[u16]) -> Vec<u16> {
    let normalized = path
        .iter()
        .map(|unit| {
            if *unit == b'/' as u16 {
                b'\\' as u16
            } else {
                *unit
            }
        })
        .collect::<Vec<_>>();
    let mut prefix = Vec::new();
    let mut rest = normalized.as_slice();
    if native_starts_with_two_separators(rest) {
        prefix.extend_from_slice(&rest[..2]);
        rest = &rest[2..];
    } else if rest.len() >= 2 && rest[1] == b':' as u16 {
        prefix.extend_from_slice(&rest[..2]);
        rest = &rest[2..];
        if rest.first().is_some_and(|unit| native_is_separator(*unit)) {
            prefix.push(b'\\' as u16);
            rest = &rest[1..];
        }
    } else if rest.first().is_some_and(|unit| native_is_separator(*unit)) {
        prefix.push(b'\\' as u16);
        rest = &rest[1..];
    }

    let rooted = !prefix.is_empty();
    let mut parts: Vec<Vec<u16>> = Vec::new();
    for part in native_split(rest, true) {
        if part.is_empty() || native_units_eq_ascii(&part, ".") {
            continue;
        }
        if native_units_eq_ascii(&part, "..") {
            if parts
                .last()
                .is_some_and(|last| !native_units_eq_ascii(last, ".."))
            {
                parts.pop();
            } else if !rooted {
                parts.push(part);
            }
        } else {
            parts.push(part);
        }
    }
    let mut output = prefix;
    for part in parts {
        if !output.is_empty() && output.last() != Some(&(b'\\' as u16)) {
            output.push(b'\\' as u16);
        }
        output.extend(part);
    }
    output
}

fn native_has_dot_alias(path: &[u16]) -> bool {
    native_split(path, true).iter().any(|component| {
        !component.is_empty()
            && component[0] == b'.' as u16
            && component
                .iter()
                .all(|unit| *unit == b'.' as u16 || *unit == b' ' as u16)
    })
}

fn native_unc_host(path: &[u16]) -> Option<Vec<u16>> {
    if !native_starts_with_two_separators(path) {
        return None;
    }
    let mut index = 2;
    if path
        .get(index)
        .is_some_and(|unit| matches!(*unit, 0x2e | 0x3f))
    {
        index += 1;
        if !path
            .get(index)
            .is_some_and(|unit| native_is_separator(*unit))
        {
            return None;
        }
        index += 1;
        let unc_end = path[index..]
            .iter()
            .position(|unit| native_is_separator(*unit))?
            + index;
        if !native_units_eq_ascii(&path[index..unc_end], "unc") {
            return None;
        }
        index = unc_end + 1;
    }
    let end = path[index..]
        .iter()
        .position(|unit| native_is_separator(*unit))
        .map_or(path.len(), |offset| index + offset);
    if end == index {
        return None;
    }
    Some(native_ascii_lower(&path[index..end]))
}

fn native_path_volume_root(path: &[u16]) -> Option<Vec<u16>> {
    if path.first() != Some(&(b'/' as u16)) {
        return None;
    }
    let mut components: Vec<Vec<u16>> = Vec::new();
    for component in native_split(path, false) {
        if component.is_empty() || native_units_eq_ascii(&component, ".") {
            continue;
        }
        if native_units_eq_ascii(&component, "..") {
            components.pop();
            continue;
        }
        components.push(component);
        if (components.len() == 2 && native_units_eq_jr(&components[0], "net"))
            || (components.len() == 3
                && native_units_eq_jr(&components[0], "network")
                && native_units_eq_jr(&components[1], "servers"))
        {
            return Some(native_join_posix(&components));
        }
    }
    None
}

fn native_path_volume_id(path: &[u16]) -> Option<Vec<u16>> {
    if path.first() != Some(&(b'/' as u16)) {
        return None;
    }
    let mut components: Vec<Vec<u16>> = Vec::new();
    for component in native_split(path, false) {
        if component.is_empty() || native_units_eq_ascii(&component, ".") {
            continue;
        }
        if native_units_eq_ascii(&component, "..") {
            components.pop();
            continue;
        }
        components.push(component.clone());
        if components.len() == 2 && native_units_eq_jr(&components[0], "net") {
            return components.get(1).map(|host| native_ascii_lower(host));
        }
        if components.len() == 3
            && native_units_eq_jr(&components[0], "network")
            && native_units_eq_jr(&components[1], "servers")
        {
            return components.get(2).map(|host| native_ascii_lower(host));
        }
    }
    None
}

fn native_join_posix(components: &[Vec<u16>]) -> Vec<u16> {
    let mut result = vec![b'/' as u16];
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            result.push(b'/' as u16);
        }
        result.extend(component);
    }
    result
}

fn native_has_network_component(path: &[u16]) -> bool {
    if path.first() != Some(&(b'/' as u16)) {
        return false;
    }
    let mut components: Vec<Vec<u16>> = Vec::new();
    for component in native_split(path, false) {
        if component.is_empty() || native_units_eq_ascii(&component, ".") {
            continue;
        }
        if native_units_eq_ascii(&component, "..") {
            components.pop();
        } else {
            components.push(component);
            // Native HO/Mt only recognizes a top-level `network` component
            // after lexical `..` folding; nested directories named `network`
            // are ordinary local paths.
            if components.len() == 1
                && native_units_eq_ascii(components.last().expect("just pushed"), "network")
            {
                return true;
            }
        }
    }
    false
}

fn native_is_net_root(path: &[u16]) -> bool {
    if path.first() != Some(&(b'/' as u16)) {
        return false;
    }
    let parts = native_normalized_posix_parts(path);
    parts.len() == 1 && native_units_eq_ascii(&parts[0], "net")
}

fn native_is_special_volume_path(path: &[u16]) -> bool {
    if path.first() != Some(&(b'/' as u16)) {
        return false;
    }
    let mut components: Vec<Vec<u16>> = Vec::new();
    for component in native_split(path, false) {
        if component.is_empty() || native_units_eq_ascii(&component, ".") {
            continue;
        }
        if native_units_eq_ascii(&component, "..") {
            components.pop();
            continue;
        }
        components.push(component);
        if components.len() == 1
            && [".vol", ".file", ".nofollow", ".resolve"]
                .iter()
                .any(|name| native_units_eq_ascii(&components[0], name))
        {
            return true;
        }
    }
    false
}

fn native_normalized_posix_parts(path: &[u16]) -> Vec<Vec<u16>> {
    let mut components: Vec<Vec<u16>> = Vec::new();
    for component in native_split(path, false) {
        if component.is_empty() || native_units_eq_ascii(&component, ".") {
            continue;
        }
        if native_units_eq_ascii(&component, "..") {
            components.pop();
        } else {
            components.push(component);
        }
    }
    components
}

fn native_is_automount_path(path: &[u16], base: &[u16], home_automount: Option<bool>) -> bool {
    // pIr's default argument evaluates Ft() at call time, before its body
    // checks whether a path uses an automount namespace. Keep this lookup
    // after the caller's Wi/DN short-circuits but before pIr's own checks.
    let home_automount = home_automount.unwrap_or_else(native_home_is_automount);
    if native_has_network_component(path) || native_is_special_volume_path(path) {
        return true;
    }
    if path.first() != Some(&(b'/' as u16)) {
        return false;
    }
    let (base_complete, _) = native_automount_roots(base, home_automount);
    let (complete, partial) = native_automount_roots(path, home_automount);
    if complete.is_empty() && partial.is_empty() {
        return false;
    }
    // Native pIr compares the candidate's completed autofs root with the
    // base path's completed root. It does not require that root to match the
    // process user's current HOME; paths under one shared /home mount are
    // safe relative to one another.
    let Some(base_root) = base_complete.first() else {
        return true;
    };
    complete.iter().any(|root| root != base_root)
        || partial
            .iter()
            .any(|root| !native_units_starts_with(base_root, root))
}

fn native_automount_roots(path: &[u16], home_automount: bool) -> (Vec<Vec<u16>>, Vec<Vec<u16>>) {
    if path.first() != Some(&(b'/' as u16)) {
        return (Vec::new(), Vec::new());
    }
    let canonical = native_macos_data_path(path);
    let components = native_normalized_posix_parts(&canonical);
    let mut complete = Vec::new();
    let mut partial = Vec::new();
    for length in 1..=components.len().min(6) {
        let current = &components[..length];
        if native_units_eq_ascii(&current[0], "network") {
            if length == 1 {
                partial.push("/network/".encode_utf16().collect());
            } else if length == 2 {
                let mut root = "/network/".encode_utf16().collect::<Vec<_>>();
                root.extend(native_ascii_lower(&current[1]));
                root.push(b'/' as u16);
                partial.push(root);
            } else if length == 3 {
                let mut root = "/network/".encode_utf16().collect::<Vec<_>>();
                root.extend(native_ascii_lower(&current[1]));
                root.push(b'/' as u16);
                root.extend(native_ascii_lower(&current[2]));
                complete.push(root);
            }
        } else if home_automount && length == 2 && native_units_eq_ascii(&current[0], "home") {
            let mut root = "/home/".encode_utf16().collect::<Vec<_>>();
            root.extend(current[1].iter().copied());
            complete.push(root);
        }
    }
    (complete, partial)
}

fn native_macos_data_path(path: &[u16]) -> Vec<u16> {
    let components = native_normalized_posix_parts(path);
    if components.len() < 4
        || !native_units_eq_ascii(&components[0], "system")
        || !native_units_eq_ascii(&components[1], "volumes")
        || !native_units_eq_ascii(&components[2], "data")
    {
        return path.to_vec();
    }
    let aliases = [
        "usr/local",
        "usr/libexec/cups",
        "usr/share/snmp",
        "appleinternal",
        "applications",
        "library",
        "users",
        "volumes",
        "cores",
        "home",
        "media",
        "mnt",
        "opt",
        "pkg",
        "private",
        "sw",
    ];
    let remainder = &components[3..];
    if !aliases.iter().any(|alias| {
        let alias = alias.split('/').collect::<Vec<_>>();
        remainder.len() >= alias.len()
            && alias
                .iter()
                .zip(remainder.iter())
                .all(|(left, right)| native_units_eq_ascii(right, left))
    }) {
        return path.to_vec();
    }
    native_join_posix(remainder)
}

fn native_home_is_automount() -> bool {
    static HOME_AUTOMOUNT: OnceLock<std::sync::Mutex<Option<bool>>> = OnceLock::new();
    let cache = HOME_AUTOMOUNT.get_or_init(|| std::sync::Mutex::new(None));
    let mut cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(value) = *cached {
        return value;
    }

    // Native Ft memoizes only a normal return. Its outer catch returns true
    // without writing the module cache when an unexpected read/stat error
    // escapes. W recognizes ENOENT: a missing auto_master is ignored, and a
    // missing /home is a normal true result from Dt.homeIsOwnMount.
    let result = (|| -> Result<bool, ()> {
        let auto_master = match std::fs::read("/etc/auto_master") {
            Ok(contents) => String::from_utf8_lossy(&contents).into_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err(()),
        };
        let has_home_entry = auto_master.lines().any(|line| {
            let line = line.split('#').next().unwrap_or_default();
            let mut fields = line
                .split(lingxi_core::host::effort::javascript_whitespace)
                .filter(|field| !field.is_empty());
            fields
                .next()
                .is_some_and(|field| field.eq_ignore_ascii_case("/home"))
                && fields.next().is_some()
        });
        if has_home_entry {
            return Ok(true);
        }
        native_home_uses_separate_device(native_path_device)
    })();

    match result {
        Ok(value) => {
            *cached = Some(value);
            value
        }
        Err(()) => true,
    }
}

fn native_home_uses_separate_device(
    mut device_for_path: impl FnMut(&Path) -> std::io::Result<u64>,
) -> Result<bool, ()> {
    let home = match device_for_path(Path::new("/home")) {
        Ok(device) => device,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Err(()),
    };
    // Native Dt does not catch the /Users stat; any failure escapes Ft's
    // outer try/catch and therefore must not be memoized.
    let users = device_for_path(Path::new("/Users")).map_err(|_| ())?;
    Ok(home != users)
}

#[cfg(unix)]
fn native_path_device(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.dev())
}

#[cfg(windows)]
fn native_path_device(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::fs::OpenOptionsExt;

    // Open directories with no requested data access. As with metadata(), do
    // not set OPEN_REPARSE_POINT: the volume must identify the target of a
    // junction or symlink rather than the volume containing the link itself.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    lingxi_core::host::rooted_fs::opened_file_identity(&file)
        .map(|identity| identity.volume())
        .map_err(std::io::Error::other)
}

#[cfg(not(any(unix, windows)))]
fn native_path_device(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::other("file identity unsupported"))
}

fn native_path_units(path: &Path) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().collect()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().encode_utf16().collect()
    }
}

fn native_path_from_units(units: &[u16]) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_wide(units)))
    }
    #[cfg(not(windows))]
    {
        String::from_utf16(units).ok().map(PathBuf::from)
    }
}

fn native_resolve_path(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    native_normalize_path(&joined)
}

fn native_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

fn native_split(path: &[u16], include_backslash: bool) -> Vec<Vec<u16>> {
    let mut components = Vec::new();
    let mut start = 0;
    for (index, unit) in path.iter().enumerate() {
        if *unit == b'/' as u16 || (include_backslash && *unit == b'\\' as u16) {
            components.push(path[start..index].to_vec());
            start = index + 1;
        }
    }
    components.push(path[start..].to_vec());
    components
}

fn native_ascii_lower(path: &[u16]) -> Vec<u16> {
    path.iter().copied().map(native_ascii_lower_unit).collect()
}

fn native_ascii_lower_unit(unit: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&unit) {
        unit + (b'a' as u16 - b'A' as u16)
    } else {
        unit
    }
}

fn native_units_eq_ascii(units: &[u16], value: &str) -> bool {
    units.len() == value.len()
        && units
            .iter()
            .zip(value.bytes())
            .all(|(unit, byte)| native_ascii_lower_unit(*unit) == byte as u16)
}

fn native_units_eq_jr(units: &[u16], value: &str) -> bool {
    let ignored = |unit: &u16| {
        (0x200c..=0x200f).contains(unit)
            || (0x202a..=0x202e).contains(unit)
            || (0x206a..=0x206f).contains(unit)
            || *unit == 0xfeff
    };
    let filtered = units
        .iter()
        .copied()
        .filter(|unit| !ignored(unit))
        .collect::<Vec<_>>();
    let folded = String::from_utf16(&filtered)
        .ok()
        .map(|text| {
            text.chars()
                .flat_map(char::to_uppercase)
                .flat_map(char::to_lowercase)
                .collect::<String>()
                .encode_utf16()
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| native_ascii_lower(&filtered));
    let expected = value
        .chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .encode_utf16()
        .collect::<Vec<_>>();
    folded == expected
}

fn native_units_starts_with(units: &[u16], prefix: &[u16]) -> bool {
    units.len() >= prefix.len() && units[..prefix.len()] == *prefix
}

fn native_git_marker_shortcut(cwd: &Path, diskless: Option<bool>) -> bool {
    diskless == Some(false) && has_native_git_root_marker(cwd)
}

fn native_symlink_target(marker: &Path) -> Option<String> {
    let target = std::fs::read_link(marker).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = target.as_os_str().as_bytes();
        let text = std::str::from_utf8(bytes).ok()?;
        if text.contains('\0') || text.as_bytes() != bytes {
            return None;
        }
        Some(text.to_string())
    }
    #[cfg(not(unix))]
    {
        let text = target.to_str()?;
        if text.contains('\0') {
            return None;
        }
        Some(text.to_string())
    }
}

#[async_trait]
impl ShellExpansionProvider for PromptShellExpansionProvider {
    async fn git_worktree_context(&self) -> GitWorktreeContext {
        use lingxi_core::host::sandbox::ProcessCommand;

        // Read once so the returned cwd and Git query cannot observe different
        // SessionCwd generations.
        let cwd = self.ctx.cwd();
        let cwd_source = cwd_source(&cwd);
        let native_cwd = native_cwd_path(&cwd, &cwd_source);

        // Native hr() caches the marker result (including false) by the raw
        // cwd on the host-scoped Host state. The cache callback reads the
        // current diskless fact only on a miss; hits intentionally do not
        // revisit it. A marker is enough to return true and is not validated
        // as a Git repository, matching Native even for an arbitrary .git
        // file.
        let has_cached_git_root =
            self.ctx
                .host_git_root_cache
                .get_or_insert_with(&native_cwd, |key| {
                    let diskless = self
                        .ctx
                        .permission_gate
                        .as_ref()
                        .and_then(|gate| gate.permission_policy_snapshot())
                        .map(|snapshot| snapshot.classifier_host_capabilities.diskless);
                    native_git_marker_shortcut(key, diskless)
                });
        if has_cached_git_root {
            return GitWorktreeContext {
                cwd: cwd_source,
                inside: true,
            };
        }

        // Native Cn, bound from 2.1.295 chunk-12447fdn.js:
        // ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor="].
        // Ht() resolves git from PATH; ProcessRunner performs the same host
        // executable lookup for this fixed, read-only argv. The source helper's
        // default timeout is 10 * 60 * 1000ms.
        let command = ProcessCommand {
            command: "git".to_string(),
            args: [
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=",
                "rev-parse",
                "--is-inside-work-tree",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            cwd: Some(native_cwd),
            env: HashMap::new(),
            timeout: Some(Duration::from_millis(600_000)),
            stdin: None,
        };
        let sandboxed = self
            .ctx
            .sandbox
            .bypass_with_audit(command, "prompt_shell_git_worktree_probe");
        let inside = self.ctx.process.run(&sandboxed).await.is_ok_and(|output| {
            output.exit_code == 0
                && output
                    .stdout
                    .trim_matches(lingxi_core::host::effort::javascript_whitespace)
                    == "true"
        });

        GitWorktreeContext {
            cwd: cwd_source,
            inside,
        }
    }

    fn build(
        &self,
        allowed_tools: &[String],
        shell: Option<FrontmatterShell>,
    ) -> ShellExpansionCtx {
        self.build_for_call(
            allowed_tools,
            shell,
            self.ctx.session_id.map(|id| id.to_string()),
            None,
        )
    }

    fn build_for_call(
        &self,
        allowed_tools: &[String],
        _shell: Option<FrontmatterShell>,
        owner: Option<String>,
        cancel: Option<lingxi_core::host::CancellationToken>,
    ) -> ShellExpansionCtx {
        // NOTE: the `_shell` build-time hint is redundant with the per-command
        // `shell` that `execute_shell_commands_in_prompt` threads into both
        // `ShellPermissionGate::check` and `ShellRunner::run` (the SAME
        // frontmatter value), so the gate reads it there per call.
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();
        let mobile_shell = self.ctx.mobile_shell();
        let shell_enabled = mobile_shell.is_some_and(|carrier| carrier.enabled);
        let runner = Arc::new(PromptShellRunner {
            side_chat: self.ctx.side_chat,
            cleanup_failed: self.ctx.side_chat_cleanup_failed.clone(),
            owner: if self.ctx.side_chat {
                owner.map(|owner| format!("side:{owner}"))
            } else {
                owner
            },
            cancel,
            process: self.ctx.process.clone(),
            sandbox: self.ctx.sandbox.clone(),
            workspace: self.ctx.cwd(),
            sandbox_available: self.ctx.sandbox_available,
            sandbox_runtime: sandbox_runtime.clone(),
            platform: self.ctx.platform,
            sandbox_runner: self.ctx.sandbox_runner.clone(),
            force_platform_sandbox: self
                .ctx
                .mobile_shell()
                .is_some_and(|carrier| carrier.force_platform_sandbox),
            shell_path: mobile_shell.map_or_else(
                || resolve_shell_path().to_string(),
                |carrier| carrier.shell_path.clone(),
            ),
            mobile_shell: mobile_shell.is_some(),
            shell_enabled,
            snapshot: self.snapshot.clone(),
        });
        let mut effective = build_effective_policy(
            &self.ctx.permission_policy,
            self.ctx.permission_mode,
            allowed_tools,
        );
        if self.ctx.permission_policy.sandbox_runtime.is_some() {
            effective = effective.with_sandbox_runtime(permission::SandboxAutoAllowConfig::new(
                sandbox_runtime.enabled,
                sandbox_runtime.auto_allow_bash_if_sandboxed,
                sandbox_runtime.excluded_commands.clone(),
            ));
        }
        let gate = Arc::new(PolicyShellPermissionGate {
            policy: Arc::new(effective),
            mode: self.ctx.permission_mode,
            live_gate: self.ctx.permission_gate.clone(),
            transient_allow_rules: allowed_tools.to_vec(),
            mobile_shell: mobile_shell.is_some(),
            shell_enabled,
        });
        ShellExpansionCtx {
            runner,
            permission_gate: gate,
        }
    }
}

/// Construct the shared prompt shell-expansion provider from a live
/// [`BuiltinToolContext`]. Injected into the dispatcher
/// (`RegistrySlashDispatcher::with_shell_expansion`), the TUI, and used directly
/// by the `Skill` tool so all three expand embedded `!command`s through the same
/// real runner + policy-backed gate.
#[must_use]
pub fn build_prompt_shell_provider(ctx: &BuiltinToolContext) -> Arc<dyn ShellExpansionProvider> {
    Arc::new(PromptShellExpansionProvider {
        ctx: ctx.clone(),
        snapshot: Arc::new(ShellSnapshot::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
    use lingxi_core::host::sandbox::{
        Sandbox, SandboxCapability, SandboxError, SandboxFeatures, SandboxPolicy, SandboxedCommand,
        SandboxedTag,
    };
    use mobile_linux_api::SandboxBackend;
    use mobile_linux_api::{ProcessError, ProcessOutput};
    use permission::filesystem::FsRoots;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use telemetry::AnalyticsBus;
    use tool_api::test_support::{ctx_for_file_tools, make_dummy_fs};

    fn test_ctx() -> BuiltinToolContext {
        ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![PathBuf::from("/tmp")],
        )
    }

    #[cfg(unix)]
    #[test]
    fn native_cwd_filesystem_path_uses_the_javascript_string_projection() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let path = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/prompt-shell-\xff".to_vec(),
        ));
        let source = cwd_source(&path);
        let native_path = native_cwd_path(&path, &source);
        let source_text = source.string_at("").unwrap();

        assert_eq!(native_path.as_os_str().as_bytes(), source_text.as_bytes());
        assert_ne!(
            native_path.as_os_str().as_bytes(),
            path.as_os_str().as_bytes()
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_cwd_filesystem_path_preserves_windows_utf16_units() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xd800];
        let path = PathBuf::from(std::ffi::OsString::from_wide(&units));
        let source = cwd_source(&path);
        let native_path = native_cwd_path(&path, &source);

        assert_eq!(
            native_path.as_os_str().encode_wide().collect::<Vec<_>>(),
            units
        );
        assert_eq!(source.string_units("").as_deref(), Some(units.as_slice()));
    }

    #[test]
    fn native_home_device_comparison_preserves_fail_closed_behavior() {
        let devices = |home: std::io::Result<u64>, users: std::io::Result<u64>| {
            let mut values = [home, users].into_iter();
            native_home_uses_separate_device(|_| values.next().unwrap())
        };

        assert_eq!(devices(Ok(7), Ok(7)), Ok(false));
        assert_eq!(devices(Ok(7), Ok(8)), Ok(true));
        assert_eq!(
            devices(Err(std::io::ErrorKind::NotFound.into()), Ok(7)),
            Ok(true)
        );
        assert_eq!(
            devices(Err(std::io::ErrorKind::PermissionDenied.into()), Ok(7)),
            Err(())
        );
        assert_eq!(
            devices(Ok(7), Err(std::io::ErrorKind::NotFound.into())),
            Err(())
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn native_path_device_reads_the_volume_of_existing_directories() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let root_device = native_path_device(root.path()).unwrap();
        assert_eq!(native_path_device(&child).unwrap(), root_device);
        assert_eq!(
            native_path_device(&root.path().join("missing"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_path_device_follows_a_directory_junction() {
        use std::os::windows::fs::MetadataExt;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let junction = root.path().join("junction");
        std::fs::create_dir(&target).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mklink /J failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        assert_ne!(
            std::fs::symlink_metadata(&junction)
                .unwrap()
                .file_attributes()
                & FILE_ATTRIBUTE_REPARSE_POINT,
            0
        );
        assert_eq!(
            native_path_device(&junction).unwrap(),
            native_path_device(&target).unwrap()
        );
        // A dangling junction must fail: opening the reparse point itself
        // instead of following it would incorrectly keep returning its volume.
        std::fs::remove_dir(&target).unwrap();
        assert_eq!(
            native_path_device(&junction).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[derive(Default)]
    struct RecordingSandbox {
        prepare_calls: AtomicUsize,
        bypass_calls: AtomicUsize,
        last_network: Mutex<Option<mobile_linux_api::NetworkPolicy>>,
        last_process_args: Mutex<Option<Vec<String>>>,
    }

    #[async_trait]
    impl Sandbox for RecordingSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::AndroidProot
        }
        fn prepare(
            &self,
            cmd: lingxi_core::host::sandbox::ProcessCommand,
            policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, SandboxError> {
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_network.lock().unwrap() = Some(policy.network);
            *self.last_process_args.lock().unwrap() = Some(cmd.args.clone());
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::Wrapped {
                    backend: SandboxBackend::AndroidProot,
                },
            ))
        }
        fn bypass_with_audit(
            &self,
            cmd: lingxi_core::host::sandbox::ProcessCommand,
            reason: &str,
        ) -> SandboxedCommand {
            let _ = reason;
            self.bypass_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_process_args.lock().unwrap() = Some(cmd.args.clone());
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: "prompt-shell-test".into(),
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

    struct RecordingProcessRunner;

    #[async_trait]
    impl ProcessRunner for RecordingProcessRunner {
        async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            Ok(ProcessOutput {
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
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

    struct CancellationProcessRunner {
        started: tokio::sync::Notify,
        killed: lingxi_core::host::CancellationToken,
        kill_calls: AtomicUsize,
        reaped: std::sync::atomic::AtomicBool,
        owner: Mutex<Option<String>>,
    }

    #[async_trait]
    impl ProcessRunner for CancellationProcessRunner {
        async fn run(&self, command: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            // Snapshot and repository probes precede the owned command. Only
            // the actual prompt-shell child participates in cancellation.
            if command.process_owner().is_none() {
                return Ok(ProcessOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                });
            }
            *self.owner.lock().unwrap() = command.process_owner().map(str::to_owned);
            self.started.notify_one();
            self.killed.cancelled().await;
            self.reaped.store(true, Ordering::SeqCst);
            Ok(ProcessOutput {
                stdout: "stopped".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
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

        async fn kill_owner_processes(&self, owner: &str) -> Vec<u32> {
            assert_eq!(self.owner.lock().unwrap().as_deref(), Some(owner));
            self.kill_calls.fetch_add(1, Ordering::SeqCst);
            self.killed.cancel();
            vec![42]
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn prompt_shell_cancellation_kills_and_joins_owned_process() {
        let mut ctx = test_ctx();
        let runner = Arc::new(CancellationProcessRunner {
            started: tokio::sync::Notify::new(),
            killed: lingxi_core::host::CancellationToken::new(),
            kill_calls: AtomicUsize::new(0),
            reaped: std::sync::atomic::AtomicBool::new(false),
            owner: Mutex::new(None),
        });
        ctx.process = runner.clone();
        let expansion = build_prompt_shell_provider(&ctx).build(&[], None);
        let cancellation = lingxi_core::host::CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let run = tokio::spawn(async move {
            expansion
                .runner
                .run(&"sleep 30".into(), None, run_cancellation)
                .await
        });
        runner.started.notified().await;
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("the owner kill must let the same process future reap")
            .expect("shell-run task must not panic");
        assert!(
            result
                .expect_err("cancelled shell should return interrupted")
                .interrupted
        );
        assert!(runner.kill_calls.load(Ordering::SeqCst) >= 1);
        assert!(runner.reaped.load(Ordering::SeqCst));
    }

    struct GitProbeProcessRunner {
        commands: Mutex<Vec<lingxi_core::host::sandbox::ProcessCommand>>,
        stdout: String,
        exit_code: i32,
        timed_out: bool,
        fail: bool,
    }

    impl GitProbeProcessRunner {
        fn output(stdout: &str, exit_code: i32) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                stdout: stdout.to_string(),
                exit_code,
                timed_out: false,
                fail: false,
            }
        }

        fn failure() -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                stdout: String::new(),
                exit_code: 1,
                timed_out: false,
                fail: true,
            }
        }
    }

    #[async_trait]
    impl ProcessRunner for GitProbeProcessRunner {
        async fn run(&self, command: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.commands.lock().unwrap().push(command.inner().clone());
            if self.fail {
                return Err(ProcessError::Unsupported);
            }
            Ok(ProcessOutput {
                stdout: self.stdout.clone(),
                stderr: String::new(),
                exit_code: self.exit_code,
                timed_out: self.timed_out,
            })
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

    fn provider_at(
        cwd: &Path,
        process: Arc<dyn ProcessRunner>,
        sandbox: Arc<dyn Sandbox>,
        diskless: Option<bool>,
    ) -> PromptShellExpansionProvider {
        provider_with_context(test_ctx(), cwd, process, sandbox, diskless)
    }

    fn provider_with_context(
        mut ctx: BuiltinToolContext,
        cwd: &Path,
        process: Arc<dyn ProcessRunner>,
        sandbox: Arc<dyn Sandbox>,
        diskless: Option<bool>,
    ) -> PromptShellExpansionProvider {
        ctx.session_cwd = tool_api::session_cwd::SessionCwd::new(cwd.to_path_buf(), Vec::new());
        ctx.process = process;
        ctx.sandbox = sandbox;
        ctx.permission_gate = diskless.map(|diskless| {
            let gate = permission::PolicyPermissionGate::new(
                Arc::new(PermissionPolicy::new(PermissionMode::Default)),
                Arc::new(permission::headless_gate::DenyOnAskGate),
            );
            gate.set_classifier_host_capabilities(
                lingxi_core::host::permission_gate::PermissionClassifierHostCapabilities {
                    diskless,
                    ..Default::default()
                },
            );
            Arc::new(gate) as Arc<dyn lingxi_core::host::permission_gate::PermissionGate>
        });
        PromptShellExpansionProvider {
            ctx,
            snapshot: Arc::new(ShellSnapshot::new()),
        }
    }

    #[tokio::test]
    async fn git_worktree_context_uses_captured_cwd_native_argv_and_ecmascript_trim() {
        let temp = tempfile::tempdir().unwrap();
        let runner = Arc::new(GitProbeProcessRunner::output("\u{feff} true \r\n", 0));
        let sandbox = Arc::new(RecordingSandbox::default());
        let provider = provider_at(temp.path(), runner.clone(), sandbox.clone(), Some(true));

        let context = provider.git_worktree_context().await;

        assert!(context.inside);
        assert_eq!(
            context.cwd.string_at(""),
            Some(temp.path().to_string_lossy().as_ref())
        );
        let commands = runner.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "git");
        let expected_args: Vec<String> = [
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "rev-parse",
            "--is-inside-work-tree",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(commands[0].args, expected_args);
        assert_eq!(commands[0].cwd.as_deref(), Some(temp.path()));
        assert!(commands[0].env.is_empty());
        assert_eq!(commands[0].timeout, Some(Duration::from_millis(600_000)));
        assert_eq!(sandbox.bypass_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn git_worktree_context_fails_closed_on_git_errors_and_non_native_output() {
        let temp = tempfile::tempdir().unwrap();
        for runner in [
            Arc::new(GitProbeProcessRunner::output("true", 128)),
            Arc::new(GitProbeProcessRunner::output("\u{0085}true\u{0085}", 0)),
            Arc::new(GitProbeProcessRunner::output("TRUE", 0)),
            Arc::new(GitProbeProcessRunner::failure()),
        ] {
            let provider = provider_at(
                temp.path(),
                runner,
                Arc::new(RecordingSandbox::default()),
                Some(true),
            );
            assert!(!provider.git_worktree_context().await.inside);
        }
    }

    #[tokio::test]
    async fn git_marker_shortcut_preserves_native_malformed_marker_and_diskless_gate() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo").join("nested");
        std::fs::create_dir_all(&cwd).unwrap();
        let repo = cwd.parent().unwrap();
        std::fs::write(repo.join(".git"), "not a valid Git pointer").unwrap();

        assert!(has_native_git_root_marker(&cwd));
        assert!(native_git_marker_shortcut(&cwd, Some(false)));
        assert!(!native_git_marker_shortcut(&cwd, Some(true)));
        assert!(!native_git_marker_shortcut(&cwd, None));

        let cached_runner = Arc::new(GitProbeProcessRunner::output("false", 128));
        let cached_provider = provider_at(
            &cwd,
            cached_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            Some(false),
        );
        assert!(cached_provider.git_worktree_context().await.inside);
        assert!(cached_runner.commands.lock().unwrap().is_empty());

        let diskless_runner = Arc::new(GitProbeProcessRunner::output("false", 128));
        let diskless_provider = provider_at(
            &cwd,
            diskless_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            Some(true),
        );
        assert!(!diskless_provider.git_worktree_context().await.inside);
        assert_eq!(diskless_runner.commands.lock().unwrap().len(), 1);

        let unknown_runner = Arc::new(GitProbeProcessRunner::output("false", 128));
        let unknown_provider = provider_at(
            &cwd,
            unknown_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            None,
        );
        assert!(!unknown_provider.git_worktree_context().await.inside);
        assert_eq!(unknown_runner.commands.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn git_root_marker_cache_is_shared_across_providers_and_keeps_negative_results() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo").join("nested");
        std::fs::create_dir_all(&cwd).unwrap();
        let repo = cwd.parent().unwrap();
        std::fs::write(repo.join(".git"), "marker").unwrap();

        let host_ctx = test_ctx();
        let first_runner = Arc::new(GitProbeProcessRunner::failure());
        let first_provider = provider_with_context(
            host_ctx.clone(),
            &cwd,
            first_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            Some(false),
        );
        assert!(first_provider.git_worktree_context().await.inside);
        assert!(first_runner.commands.lock().unwrap().is_empty());

        // A second provider for the same host/cwd reuses the positive entry,
        // even though the marker disappeared and the diskless fact changed.
        std::fs::remove_file(repo.join(".git")).unwrap();
        let second_runner = Arc::new(GitProbeProcessRunner::failure());
        let second_provider = provider_with_context(
            host_ctx.clone(),
            &cwd,
            second_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            Some(true),
        );
        assert!(second_provider.git_worktree_context().await.inside);
        assert!(second_runner.commands.lock().unwrap().is_empty());

        // A separate host has a separate cache. Its initial negative lookup
        // remains cached when a marker appears later, so it proceeds to the
        // Git query instead of rescanning the filesystem.
        let separate_host_ctx = test_ctx();
        let negative_runner = Arc::new(GitProbeProcessRunner::output("false", 0));
        let negative_provider = provider_with_context(
            separate_host_ctx.clone(),
            &cwd,
            negative_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            None,
        );
        assert!(!negative_provider.git_worktree_context().await.inside);
        assert_eq!(negative_runner.commands.lock().unwrap().len(), 1);

        std::fs::write(repo.join(".git"), "marker").unwrap();
        let cached_negative_runner = Arc::new(GitProbeProcessRunner::output("false", 0));
        let cached_negative_provider = provider_with_context(
            separate_host_ctx,
            &cwd,
            cached_negative_runner.clone(),
            Arc::new(RecordingSandbox::default()),
            Some(false),
        );
        assert!(!cached_negative_provider.git_worktree_context().await.inside);
        assert_eq!(cached_negative_runner.commands.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn native_git_marker_symlinks_follow_source_safe_targets() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let sibling = temp.path().join("repo-sibling");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        std::fs::create_dir(repo.join(".git-target")).unwrap();
        symlink(".git-target", repo.join(".git")).unwrap();
        assert!(has_native_git_root_marker(&repo));

        std::fs::remove_file(repo.join(".git")).unwrap();
        symlink(&outside, repo.join(".git")).unwrap();
        // Native me/Me checks unsafe links; it does not require general
        // containment beneath the repository directory.
        assert!(has_native_git_root_marker(&repo));

        std::fs::remove_file(repo.join(".git")).unwrap();
        symlink(&sibling, repo.join(".git")).unwrap();
        assert!(has_native_git_root_marker(&repo));

        std::fs::remove_file(repo.join(".git")).unwrap();
        symlink("../outside", repo.join("alias")).unwrap();
        symlink("alias", repo.join(".git")).unwrap();
        assert!(has_native_git_root_marker(&repo));

        std::fs::remove_file(repo.join(".git")).unwrap();
        std::fs::remove_file(repo.join("alias")).unwrap();
        symlink(
            std::ffi::OsStr::from_bytes(b"invalid-\xff-utf8"),
            repo.join(".git"),
        )
        .unwrap();
        assert!(native_symlink_target(&repo.join(".git")).is_none());
        assert!(!has_native_git_root_marker(&repo));

        std::fs::remove_file(repo.join(".git")).unwrap();
        symlink("missing-target", repo.join(".git")).unwrap();
        assert!(!has_native_git_root_marker(&repo));

        std::fs::remove_file(repo.join(".git")).unwrap();
        symlink(".git", repo.join(".git")).unwrap();
        assert!(!has_native_git_root_marker(&repo));
    }

    #[test]
    fn native_git_link_filter_matches_network_device_and_autofs_roots() {
        let base = "/tmp/native-git-link/repo";
        let base_units = base.encode_utf16().collect::<Vec<_>>();

        for target in [
            "/net/build-server/share/repo",
            "/Network/Servers/build-server/share/repo",
            "/.vol/123/456",
            "/.file/id/path",
            "/.nofollow/path",
            "/.resolve/path",
            "//remote-host/share/repo",
        ] {
            assert!(
                native_git_link_path_is_unsafe(target, Path::new(base), Some(false)),
                "Native Wi must reject {target:?} before the component walk"
            );
        }

        assert!(!native_git_link_path_is_unsafe(
            "/tmp/native-git-link/shared-target",
            Path::new(base),
            Some(false)
        ));
        assert!(native_has_network_component(
            &"/network/local".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!native_has_network_component(
            &"/tmp/network/local".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!native_git_link_path_is_unsafe(
            "/tmp/network/local",
            Path::new(base),
            Some(false)
        ));
        assert!(native_is_automount_path(
            &"/home/other-user/repo".encode_utf16().collect::<Vec<_>>(),
            &"/home/current-user/work".encode_utf16().collect::<Vec<_>>(),
            Some(true)
        ));
        assert!(!native_is_automount_path(
            &"/home/current-user/repo".encode_utf16().collect::<Vec<_>>(),
            &"/home/current-user/work".encode_utf16().collect::<Vec<_>>(),
            Some(true)
        ));
        assert!(!native_is_automount_path(
            &"/home/other-user/repo".encode_utf16().collect::<Vec<_>>(),
            &"/home/current-user/work".encode_utf16().collect::<Vec<_>>(),
            Some(false)
        ));

        assert!(native_gse(
            &"\\\\?\\C:\\repo".encode_utf16().collect::<Vec<_>>(),
            &base_units
        ));
        assert!(!native_gse(
            &"\\\\same-host\\other-share\\repo"
                .encode_utf16()
                .collect::<Vec<_>>(),
            &"\\\\same-host\\repo".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(native_gse(
            &"//same-host/../share".encode_utf16().collect::<Vec<_>>(),
            &"//same-host/repo".encode_utf16().collect::<Vec<_>>()
        ));
    }

    /// The gate folds a 3-valued `PermissionResult` to a 2-valued decision:
    /// a read-only command auto-allows (`Allow`→`Allow`), and a non-read-only
    /// command with no matching rule falls through to a mode `Ask`, which is
    /// folded to `Deny` (TS treats `behavior !== 'allow'` as a thrown denial).
    #[test]
    fn gate_maps_readonly_allow_and_ask_to_deny() {
        let provider = build_prompt_shell_provider(&test_ctx());
        let ctx = provider.build(&[], None);
        // `git status` is read-only (roots-independent auto-allow).
        assert_eq!(
            ctx.permission_gate.check(&"git status".into(), None),
            ShellPermissionDecision::Allow
        );
        // `git push` is a write, in no allow-list → mode `Ask` → folded to `Deny`.
        assert!(matches!(
            ctx.permission_gate.check(&"git push".into(), None),
            ShellPermissionDecision::Deny { .. }
        ));
    }

    /// Injecting a command's `allowed_tools` into a FRESH effective policy allows
    /// a non-read-only command that would otherwise be denied — 1:1 with
    /// claude-code's per-command `alwaysAllowRules.command`. Requires roots on the
    /// base policy (shell content matching is roots-gated, as in production).
    #[test]
    fn injected_allowed_tools_allow_via_effective_policy() {
        let mut ctx = test_ctx();
        ctx.permission_policy = Arc::new(
            PermissionPolicy::new(PermissionMode::Default).with_roots(FsRoots {
                cwd: PathBuf::from("/tmp"),
                home: None,
                lingxi_home: PathBuf::from("/tmp"),
            }),
        );
        let provider = build_prompt_shell_provider(&ctx);

        // Without the injected rule: `git commit` is a write, no rule → Deny.
        let bare = provider.build(&[], None);
        assert!(matches!(
            bare.permission_gate.check(&"git commit -m x".into(), None),
            ShellPermissionDecision::Deny { .. }
        ));

        // With the command's own `Bash(git commit:*)` injected: Allow.
        let with_rule = provider.build(&["Bash(git commit:*)".to_string()], None);
        assert_eq!(
            with_rule
                .permission_gate
                .check(&"git commit -m x".into(), None),
            ShellPermissionDecision::Allow
        );
    }

    /// Building is per-command: two `build` calls with different allow-lists
    /// never leak rules into each other, and the shared base policy is never
    /// mutated (a later bare build still denies).
    #[test]
    fn each_build_is_fresh_and_never_mutates_base() {
        let mut ctx = test_ctx();
        ctx.permission_policy = Arc::new(
            PermissionPolicy::new(PermissionMode::Default).with_roots(FsRoots {
                cwd: PathBuf::from("/tmp"),
                home: None,
                lingxi_home: PathBuf::from("/tmp"),
            }),
        );
        let provider = build_prompt_shell_provider(&ctx);

        let _allowed = provider.build(&["Bash(git commit:*)".to_string()], None);
        // A subsequent build with NO allow-list must still deny `git commit`,
        // proving the previous injection did not persist onto the base policy.
        let fresh = provider.build(&[], None);
        assert!(matches!(
            fresh.permission_gate.check(&"git commit -m x".into(), None),
            ShellPermissionDecision::Deny { .. }
        ));
    }

    /// Stage 5 regression: the `/commit-push-pr` embedded body
    /// `gh pr view --json number 2>/dev/null || true` (commit_push_pr.rs:75) is
    /// a MIXED compound — a `Bash(gh pr view:*)`-rule-allowed `gh pr view …`
    /// next to a read-only `true`. With the command's own `ALLOWED_TOOLS`
    /// injected it must resolve to `Allow` (the compound-command allow
    /// composition), so `execute_shell_commands_in_prompt` no longer aborts the
    /// whole prompt. Uses the LITERAL commit-push-pr allow-list.
    #[test]
    fn commit_push_pr_gh_pr_view_or_true_compound_allows() {
        // The literal `commands/core/src/commit_push_pr.rs` ALLOWED_TOOLS.
        let allowed_tools: Vec<String> = [
            "Bash(git checkout --branch:*)",
            "Bash(git checkout -b:*)",
            "Bash(git add:*)",
            "Bash(git status:*)",
            "Bash(git push:*)",
            "Bash(git commit:*)",
            "Bash(gh pr create:*)",
            "Bash(gh pr edit:*)",
            "Bash(gh pr view:*)",
            "Bash(gh pr merge:*)",
            "ToolSearch",
            "mcp__slack__send_message",
            "mcp__claude_ai_Slack__slack_send_message",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let mut ctx = test_ctx();
        ctx.permission_policy = Arc::new(
            PermissionPolicy::new(PermissionMode::Default).with_roots(FsRoots {
                cwd: PathBuf::from("/tmp"),
                home: None,
                lingxi_home: PathBuf::from("/tmp"),
            }),
        );
        let provider = build_prompt_shell_provider(&ctx);
        let gate = provider.build(&allowed_tools, None);

        // The mixed compound → Allow (rule-allowed `gh pr view` + read-only `true`).
        assert_eq!(
            gate.permission_gate
                .check(&"gh pr view --json number 2>/dev/null || true".into(), None),
            ShellPermissionDecision::Allow,
        );
    }

    #[test]
    fn snapshot_command_sources_then_evals_with_safe_single_quote_escaping() {
        let path = Path::new("/tmp/has space/snapshot.sh");
        let command = command_with_snapshot("printf '%s' \"it's\"", "/bin/zsh", Some(path));
        assert!(command.starts_with(". '/tmp/has space/snapshot.sh' 2>/dev/null || true; eval "));
        assert!(command.contains("'\\''"));
        assert!(command.contains("setopt NO_EXTENDED_GLOB"));
    }

    #[tokio::test]
    async fn mobile_force_platform_sandbox_uses_prepare_not_bypass() {
        let mut ctx = test_ctx();
        let sandbox = Arc::new(RecordingSandbox::default());
        ctx.sandbox = sandbox.clone();
        ctx.process = Arc::new(RecordingProcessRunner);
        ctx.mobile_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        ));
        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&[], None);
        let out = expansion
            .runner
            .run(
                &"printf ok".into(),
                None,
                lingxi_core::host::CancellationToken::new(),
            )
            .await
            .expect("mobile prompt-shell run");
        assert_eq!(out.stdout, "ok");
        assert_eq!(sandbox.prepare_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sandbox.bypass_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            *sandbox.last_network.lock().unwrap(),
            Some(mobile_linux_api::NetworkPolicy::Disabled)
        );
    }

    #[tokio::test]
    async fn process_boundary_replaces_only_unpaired_units_with_text_encoder_output() {
        let mut ctx = test_ctx();
        let sandbox = Arc::new(RecordingSandbox::default());
        ctx.sandbox = sandbox.clone();
        ctx.process = Arc::new(RecordingProcessRunner);
        ctx.mobile_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        ));
        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&[], None);

        let mut units = "printf 'A".encode_utf16().collect::<Vec<_>>();
        units.push(0xd800);
        units.extend("Z'".encode_utf16());
        let command = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("raw process command fixture");
        expansion
            .runner
            .run(&command, None, lingxi_core::host::CancellationToken::new())
            .await
            .expect("process boundary accepts raw UTF-16 command source");

        let args = sandbox
            .last_process_args
            .lock()
            .unwrap()
            .clone()
            .expect("mobile sandbox received the command");
        assert_eq!(args.first().map(String::as_str), Some("-c"));
        assert!(args[1].contains("A\u{fffd}Z"));
        assert!(args[1]
            .encode_utf16()
            .all(|unit| unit != 0xd800 && unit != 0xdfff));
    }

    #[tokio::test]
    async fn disabled_mobile_carrier_never_falls_back_to_desktop_shell() {
        let mut ctx = test_ctx();
        ctx.process = Arc::new(RecordingProcessRunner);
        ctx.mobile_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            false,
            vec!["sh".into()],
            None,
        ));

        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&["Shell(*)".into()], None);

        assert!(matches!(
            expansion.permission_gate.check(&"printf ok".into(), None),
            ShellPermissionDecision::Deny { .. }
        ));
        let error = expansion
            .runner
            .run(
                &"printf ok".into(),
                None,
                lingxi_core::host::CancellationToken::new(),
            )
            .await
            .expect_err("disabled mobile shell must fail closed");
        assert_eq!(
            error.generic_message.as_deref(),
            Some("Mobile Shell is unavailable; prompt commands cannot run on the host shell")
        );
    }

    #[test]
    fn mobile_prompt_commands_authorize_as_shell_not_bash() {
        struct RecordingGate(Mutex<Vec<(String, Option<Vec<u16>>)>>);
        #[async_trait]
        impl lingxi_core::host::permission_gate::PermissionGate for RecordingGate {
            async fn check(
                &self,
                _tool_name: &str,
                _input: &Utf16JsonProjection,
            ) -> lingxi_core::host::permission_gate::PermissionDecision {
                lingxi_core::host::permission_gate::PermissionDecision::Allow
            }

            fn check_noninteractive_with_allow_rules(
                &self,
                tool_name: &str,
                input: &Utf16JsonProjection,
                _allow_rules: &[String],
            ) -> Option<lingxi_core::host::permission_gate::NonInteractivePermissionDecision>
            {
                self.0
                    .lock()
                    .unwrap()
                    .push((tool_name.to_string(), input.string_units("/command")));
                Some(lingxi_core::host::permission_gate::NonInteractivePermissionDecision::Allow)
            }
        }

        let mut ctx = test_ctx();
        let gate = Arc::new(RecordingGate(Mutex::new(Vec::new())));
        ctx.permission_gate = Some(gate.clone());
        ctx.mobile_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        ));

        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&[], None);
        let command = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&[u16::from(b'p'), u16::from(b'r'), 0xd800]),
            vec![u16::from(b'p'), u16::from(b'r'), 0xd800],
        )
        .expect("raw permission command fixture");
        assert_eq!(
            expansion.permission_gate.check(&command, None),
            ShellPermissionDecision::Allow
        );
        assert_eq!(
            *gate.0.lock().unwrap(),
            vec![("Shell".to_string(), command.string_units(""))]
        );
    }
}
