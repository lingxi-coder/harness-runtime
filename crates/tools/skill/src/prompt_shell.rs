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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use command_api::{
    FrontmatterShell, ShellExpansionCtx, ShellExpansionProvider, ShellOut, ShellPermissionDecision,
    ShellPermissionGate, ShellRunError, ShellRunner,
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
    process: Arc<dyn platform_api::process::ProcessRunner>,
    sandbox: Arc<dyn platform_api::sandbox::Sandbox>,
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
        process: &dyn platform_api::process::ProcessRunner,
        sandbox: &dyn platform_api::sandbox::Sandbox,
        workspace: &Path,
    ) -> Option<&Path> {
        use platform_api::sandbox::ProcessCommand;

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

#[async_trait]
impl ShellRunner for PromptShellRunner {
    async fn run(
        &self,
        command: &str,
        _shell: Option<FrontmatterShell>,
    ) -> Result<ShellOut, ShellRunError> {
        use platform_api::sandbox::ProcessCommand;
        use sandbox::decision::{should_use_sandbox, SandboxDecision};

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
        let snapshot = if self.mobile_shell {
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
        let spawn_cmd = command_with_snapshot(command, shell_path, snapshot);

        // ===== Sandbox decision (mirror of `BashTool::call`) =====
        // A prompt `!command` has NO per-command `dangerouslyDisableSandbox` flag
        // (that is a Bash-tool *input* field; prompt bodies have no such surface),
        // so we pass `false` for that override and otherwise feed the same inputs
        // bash does — 1:1 with claude-code `shouldUseSandbox.ts`. `unsandboxed_allowed`
        // is the canonical `are_unsandboxed_commands_allowed()` accessor.
        let decision = should_use_sandbox(
            command,
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
            timeout: None,
            stdin: None,
        };
        let sandboxed = if self.force_platform_sandbox {
            let policy = platform_api::sandbox::SandboxPolicy {
                network: platform_api::sandbox::NetworkPolicy::Disabled,
                writable_paths: vec![],
                denied_paths: vec![],
                allow_subprocess: true,
                limits: platform_api::sandbox::ResourceLimits::default(),
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
        let run_result = self.process.run(&sandboxed).await;
        // The wrapped command has finished: tear down any per-command sandbox
        // state. No-op for the default `LegacyWrapRunner`.
        self.sandbox_runner.cleanup_after_command().await;
        match run_result {
            // A timeout maps to the TS interrupted `ShellError` path so the engine
            // formats "Shell command interrupted …" (mirrors bash's timeout->error).
            Ok(out) if out.timed_out => Err(ShellRunError {
                stdout: out.stdout,
                stderr: out.stderr,
                interrupted: true,
                generic_message: None,
            }),
            // Non-zero exit is NOT an error here — TS `BashTool.call` returns
            // stdout/stderr without throwing on a non-zero status; only an
            // interruption throws. So every completed run yields `ShellOut`.
            Ok(out) => Ok(ShellOut {
                stdout: out.stdout,
                stderr: out.stderr,
                interrupted: false,
            }),
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
    live_gate: Option<Arc<dyn platform_api::permission_gate::PermissionGate>>,
    /// Frontmatter allow rules injected only for this prompt command.
    transient_allow_rules: Vec<String>,
    /// Actual registered command tool name (`Shell` on mobile, otherwise the
    /// frontmatter-selected desktop shell tool).
    mobile_shell: bool,
    shell_enabled: bool,
}

impl ShellPermissionGate for PolicyShellPermissionGate {
    fn check(&self, command: &str, shell: Option<FrontmatterShell>) -> ShellPermissionDecision {
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
        let input = serde_json::json!({ "command": command });
        if let Some(decision) = self.live_gate.as_ref().and_then(|gate| {
            gate.check_noninteractive_with_allow_rules(
                tool_name,
                &input,
                &self.transient_allow_rules,
            )
        }) {
            return match decision {
                platform_api::permission_gate::NonInteractivePermissionDecision::Allow => {
                    ShellPermissionDecision::Allow
                }
                platform_api::permission_gate::NonInteractivePermissionDecision::Deny {
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

impl ShellExpansionProvider for PromptShellExpansionProvider {
    fn build(
        &self,
        allowed_tools: &[String],
        _shell: Option<FrontmatterShell>,
    ) -> ShellExpansionCtx {
        // NOTE: the `_shell` build-time hint is redundant with the per-command
        // `shell` that `execute_shell_commands_in_prompt` threads into both
        // `ShellPermissionGate::check` and `ShellRunner::run` (the SAME
        // frontmatter value), so the gate reads it there per call.
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();
        let mobile_shell = self.ctx.mobile_shell();
        let shell_enabled = mobile_shell.is_some_and(|carrier| carrier.enabled);
        let runner = Arc::new(PromptShellRunner {
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
    use permission::filesystem::FsRoots;
    use platform_api::process::{ProcessError, ProcessHandle, ProcessOutput, ProcessRunner};
    use platform_api::sandbox::{
        Sandbox, SandboxBackend, SandboxCapability, SandboxError, SandboxFeatures, SandboxPolicy,
        SandboxedCommand, SandboxedTag,
    };
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

    #[derive(Default)]
    struct RecordingSandbox {
        prepare_calls: AtomicUsize,
        bypass_calls: AtomicUsize,
        last_network: Mutex<Option<platform_api::sandbox::NetworkPolicy>>,
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
            cmd: platform_api::sandbox::ProcessCommand,
            policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, SandboxError> {
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_network.lock().unwrap() = Some(policy.network);
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::Wrapped {
                    backend: SandboxBackend::AndroidProot,
                },
            ))
        }
        fn bypass_with_audit(
            &self,
            cmd: platform_api::sandbox::ProcessCommand,
            reason: &str,
        ) -> SandboxedCommand {
            let _ = reason;
            self.bypass_calls.fetch_add(1, Ordering::SeqCst);
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
            ctx.permission_gate.check("git status", None),
            ShellPermissionDecision::Allow
        );
        // `git push` is a write, in no allow-list → mode `Ask` → folded to `Deny`.
        assert!(matches!(
            ctx.permission_gate.check("git push", None),
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
            bare.permission_gate.check("git commit -m x", None),
            ShellPermissionDecision::Deny { .. }
        ));

        // With the command's own `Bash(git commit:*)` injected: Allow.
        let with_rule = provider.build(&["Bash(git commit:*)".to_string()], None);
        assert_eq!(
            with_rule.permission_gate.check("git commit -m x", None),
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
            fresh.permission_gate.check("git commit -m x", None),
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
                .check("gh pr view --json number 2>/dev/null || true", None),
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
        ctx.android_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        ));
        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&[], None);
        let out = expansion
            .runner
            .run("printf ok", None)
            .await
            .expect("mobile prompt-shell run");
        assert_eq!(out.stdout, "ok");
        assert_eq!(sandbox.prepare_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sandbox.bypass_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            *sandbox.last_network.lock().unwrap(),
            Some(platform_api::sandbox::NetworkPolicy::Disabled)
        );
    }

    #[tokio::test]
    async fn disabled_mobile_carrier_never_falls_back_to_desktop_shell() {
        let mut ctx = test_ctx();
        ctx.process = Arc::new(RecordingProcessRunner);
        ctx.android_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            false,
            vec!["sh".into()],
            None,
        ));

        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&["Shell(*)".into()], None);

        assert!(matches!(
            expansion.permission_gate.check("printf ok", None),
            ShellPermissionDecision::Deny { .. }
        ));
        let error = expansion
            .runner
            .run("printf ok", None)
            .await
            .expect_err("disabled mobile shell must fail closed");
        assert_eq!(
            error.generic_message.as_deref(),
            Some("Mobile Shell is unavailable; prompt commands cannot run on the host shell")
        );
    }

    #[test]
    fn mobile_prompt_commands_authorize_as_shell_not_bash() {
        struct RecordingGate(Mutex<Vec<String>>);
        #[async_trait]
        impl platform_api::permission_gate::PermissionGate for RecordingGate {
            async fn check(
                &self,
                _tool_name: &str,
                _input: &serde_json::Value,
            ) -> platform_api::permission_gate::PermissionDecision {
                platform_api::permission_gate::PermissionDecision::Allow
            }

            fn check_noninteractive_with_allow_rules(
                &self,
                tool_name: &str,
                _input: &serde_json::Value,
                _allow_rules: &[String],
            ) -> Option<platform_api::permission_gate::NonInteractivePermissionDecision>
            {
                self.0.lock().unwrap().push(tool_name.to_string());
                Some(platform_api::permission_gate::NonInteractivePermissionDecision::Allow)
            }
        }

        let mut ctx = test_ctx();
        let gate = Arc::new(RecordingGate(Mutex::new(Vec::new())));
        ctx.permission_gate = Some(gate.clone());
        ctx.android_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        ));

        let provider = build_prompt_shell_provider(&ctx);
        let expansion = provider.build(&[], None);
        assert_eq!(
            expansion.permission_gate.check("printf ok", None),
            ShellPermissionDecision::Allow
        );
        assert_eq!(*gate.0.lock().unwrap(), vec!["Shell"]);
    }
}
