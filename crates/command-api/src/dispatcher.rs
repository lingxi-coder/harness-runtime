//! Implementation of [`lingxi_core::host::SlashCommandDispatcher`] that routes
//! `/<name> <args>` into the in-crate [`CommandRegistry`].
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md` Task 5.

use crate::expand::{expand_markdown_command, ExpandCtx};
use crate::model::{CommandResult, CommandSource, SlashCommandKind};
use crate::parser::parse_slash_command;
use crate::registry::CommandRegistry;
use crate::shell_expansion::{
    execute_shell_commands_in_prompt, ShellExpansionCtx, ShellExpansionProvider, ShellOut,
    ShellPermissionDecision, ShellPermissionGate, ShellRunError, ShellRunner,
};
use async_trait::async_trait;
use hooks::events::{HookEvent, PromptExpansionType};
use hooks::registry::HookContext;
use hooks::HookExecutorImpl;
use lingxi_core::host::permission_gate::PermissionGate;
use lingxi_core::host::{SlashCommandDispatcher, SlashDispatchResult};
use lingxi_core::types::McpConnectionId;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Supplies the per-dispatch [`HookContext`] (session id, cwd, transcript path,
/// permission mode) for the `UserPromptExpansion` hook. The dispatcher does not
/// own the session/engine state, so the wiring layer (which holds the
/// orchestrator) injects this provider. Mirrors claude-code's `b$t`, where the
/// base hook input is built from the tool-use context's `getAppState`
/// (minified `vd`).
///
/// Async because the orchestrator reads its `session_id` behind a lock
/// (`ConversationOrchestrator::expansion_hook_context`); the closure is invoked
/// only on a real markdown/MCP-prompt expansion, so the await is off the hot
/// path.
pub type ExpansionHookContextProvider = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = HookContext> + Send>>
        + Send
        + Sync,
>;

/// Launches a fork-context bundled command without growing the foreground
/// conversation. The composition root supplies the actual subagent runtime.
pub type BackgroundPromptLauncher = Arc<
    dyn Fn(
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// Resolves a live MCP prompt through the connection generation that
/// advertised it. Hosts without MCP leave this unwired.
pub type McpPromptResolver = Arc<
    dyn Fn(
            McpConnectionId,
            String,
            Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

/// Observes a successfully dispatched prompt skill. The desktop composition
/// root uses this late-bound seam to arm plugin `on-skill-invoke` monitors;
/// hosts without plugin monitors leave it unwired.
pub type SkillInvocationObserver = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// The `UserPromptExpansion` firing dependencies threaded into a
/// [`RegistrySlashDispatcher`]: the engine's hook executor plus the
/// context-provider. Both are present together or the feature is off.
struct ExpansionHooks {
    executor: Arc<HookExecutorImpl>,
    context: ExpansionHookContextProvider,
}

/// Map `LingXi`'s [`CommandSource`] to the lowercase `command_source` string
/// claude-code threads into the `UserPromptExpansion` payload (`r = e.source`,
/// values `"builtin" | "user" | "project" | "local" | "plugin" | "mcp"`; the
/// admin-managed tier is reported as `"managed"`). This is the per-command
/// provenance string, distinct from the `PascalCase` serde encoding of the enum.
fn command_source_str(source: CommandSource) -> &'static str {
    match source {
        CommandSource::Builtin => "builtin",
        CommandSource::Settings(lingxi_core::types::SettingsScope::User) => "user",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Project) => "project",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Local) => "local",
        CommandSource::Plugin => "plugin",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => "managed",
        CommandSource::Mcp => "mcp",
        // Programmatic bundled skills (TS `source: 'bundled'`).
        CommandSource::Bundled => "bundled",
    }
}

struct UnavailableShellRunner;

#[async_trait]
impl ShellRunner for UnavailableShellRunner {
    async fn run(
        &self,
        _command: &str,
        _shell: Option<crate::FrontmatterShell>,
    ) -> Result<ShellOut, ShellRunError> {
        Err(ShellRunError {
            stdout: String::new(),
            stderr: String::new(),
            interrupted: false,
            generic_message: Some("shell expansion is unavailable in slash dispatcher".into()),
        })
    }
}

struct DenyShellPermissionGate;

impl ShellPermissionGate for DenyShellPermissionGate {
    fn check(
        &self,
        _command: &str,
        _shell: Option<crate::FrontmatterShell>,
    ) -> ShellPermissionDecision {
        ShellPermissionDecision::Deny {
            message: Some("shell expansion is unavailable in slash dispatcher".into()),
        }
    }
}

/// Concrete `SlashCommandDispatcher` backed by an `Arc<RwLock<CommandRegistry>>`.
///
/// The registry is wrapped in an `RwLock` because plugin lifecycle events
/// ([`CommandRegistry::register_plugin_commands`] /
/// [`CommandRegistry::unregister_plugin`]) need exclusive write access at
/// runtime. Dispatching only takes a read lock.
pub struct RegistrySlashDispatcher {
    registry: Arc<RwLock<CommandRegistry>>,
    /// `UserPromptExpansion` firing deps (#39). `None` keeps the dispatcher a
    /// strict no-op for that event — the default for every existing caller and
    /// for hosts without a hook-capable engine. Wired by the composition root
    /// via [`Self::with_expansion_hooks`] once the orchestrator's executor
    /// exists.
    expansion_hooks: Option<ExpansionHooks>,
    /// Embedded-shell-expansion provider (#3). `None` (the default for every
    /// existing caller — e.g. the bridge-server command router) keeps the
    /// dispatcher a STRICT no-op: the markdown / plugin arm falls back to the
    /// [`UnavailableShellRunner`] / [`DenyShellPermissionGate`] stubs and the
    /// builtin `InjectMessage` arm delivers content verbatim, byte-identical to
    /// today. When wired via [`Self::with_shell_expansion`], each expansion
    /// builds a FRESH per-command [`ShellExpansionCtx`] (that command's
    /// `allowed_tools` injected on top of the base policy) before running
    /// [`execute_shell_commands_in_prompt`], 1:1 with claude-code's
    /// `executeShellCommandsInPrompt` at the COMMAND layer.
    shell_expansion: Option<Arc<dyn ShellExpansionProvider>>,
    /// Explicit config-home for persistent user/project skill usage counters.
    /// `None` keeps embedders and tests side-effect free.
    skill_usage_home: Option<PathBuf>,
    /// Host-owned background launcher for bundled commands declaring
    /// `context: fork`. `None` fails closed instead of executing inline.
    background_prompt_launcher: Option<BackgroundPromptLauncher>,
    /// Live `prompts/get` bridge. Kept host-owned so `command-api` does not
    /// depend on the concrete MCP registry crate.
    mcp_prompt_resolver: Option<McpPromptResolver>,
    /// Best-effort observer for successful prompt-skill dispatches.
    skill_invocation_observer: Option<SkillInvocationObserver>,
    /// Whether builtin `InjectMessage` results become model turns. Thin clients
    /// enable this because they have no TUI loop to consume injected prompts;
    /// the default stays display-only for existing embedded/mobile callers.
    injected_messages_as_turns: bool,
    prompt_paths: Option<Arc<dyn Fn() -> (PathBuf, PathBuf) + Send + Sync>>,
    /// Permission gate that receives each input's frontmatter
    /// `disallowed-tools` (claude-code `Tbt(setToolPermissionContext, …)`).
    /// `None` — every existing caller — leaves the field inert, so a host that
    /// wires no gate behaves exactly as before.
    permission_gate: Option<Arc<dyn PermissionGate>>,
}

impl RegistrySlashDispatcher {
    /// Wire the permission gate that this dispatcher tells about each input's
    /// `disallowed-tools` — see [`PermissionGate::set_command_input_denies`].
    /// Without it, a skill's `disallowed-tools` is parsed and then ignored on
    /// the INLINE path (the fork path scopes the spawned agent separately).
    #[must_use]
    pub fn with_permission_gate(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.permission_gate = Some(gate);
        self
    }

    /// This input's frontmatter `disallowed-tools`, or empty for anything that
    /// declares none — a plain prompt, an unknown name, a builtin.
    ///
    /// Resolved for EVERY input, including non-slash text, because the empty
    /// answer is what CLEARS the previous command's denies. Returning early for
    /// a plain prompt would make the last skill's restrictions permanent.
    async fn command_input_denies(&self, raw: &str) -> Vec<String> {
        let Some(parsed) = parse_slash_command(raw) else {
            return Vec::new();
        };
        let reg = self.registry.read().await;
        let Some(command) = reg.resolve(&parsed.name) else {
            return Vec::new();
        };
        match &command.kind {
            SlashCommandKind::Markdown { frontmatter, .. }
            | SlashCommandKind::Plugin { frontmatter, .. } => {
                frontmatter.disallowed_tools.clone().unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// Construct a dispatcher backed by the given shared registry.
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>) -> Self {
        Self {
            registry,
            expansion_hooks: None,
            shell_expansion: None,
            skill_usage_home: None,
            background_prompt_launcher: None,
            mcp_prompt_resolver: None,
            skill_invocation_observer: None,
            injected_messages_as_turns: false,
            prompt_paths: None,
            permission_gate: None,
        }
    }

    /// Supply the session project root and live cwd for checked bundled expansion.
    #[must_use]
    pub fn with_prompt_paths(
        mut self,
        paths: Arc<dyn Fn() -> (PathBuf, PathBuf) + Send + Sync>,
    ) -> Self {
        self.prompt_paths = Some(paths);
        self
    }

    /// Make builtin prompt-injection commands run through the host turn driver.
    #[must_use]
    pub fn with_injected_messages_as_turns(mut self) -> Self {
        self.injected_messages_as_turns = true;
        self
    }

    /// Wire the embedded-shell-expansion provider (#3). After this, expanding a
    /// markdown / plugin slash command runs its embedded shell substitutions
    /// bodies through the real host runner + policy-backed gate (that command's
    /// frontmatter `allowed_tools` injected), and the builtin `InjectMessage`
    /// commands (`/commit`, `/commit-push-pr`, `/security-review`) expand their
    /// embedded bodies with their own `allowed_tools`. A strict no-op when
    /// unset (the default): the stubs deny, and `InjectMessage` content is
    /// verbatim. MCP-sourced commands are NEVER expanded (claude-code
    /// `loadedFrom === 'mcp'` carve-out), even when wired.
    #[must_use]
    pub fn with_shell_expansion(mut self, provider: Arc<dyn ShellExpansionProvider>) -> Self {
        self.shell_expansion = Some(provider);
        self
    }

    /// Wire the same config home `/skill-doctor` reads.
    #[must_use]
    pub fn with_skill_usage_home(mut self, config_home: PathBuf) -> Self {
        self.skill_usage_home = Some(config_home);
        self
    }

    /// Wire the background subagent launcher used by fork-context bundled
    /// commands such as `/code-review`.
    #[must_use]
    pub fn with_background_prompt_launcher(mut self, launcher: BackgroundPromptLauncher) -> Self {
        self.background_prompt_launcher = Some(launcher);
        self
    }

    /// Wire live MCP prompt expansion.
    #[must_use]
    pub fn with_mcp_prompt_resolver(mut self, resolver: McpPromptResolver) -> Self {
        self.mcp_prompt_resolver = Some(resolver);
        self
    }

    /// Wire the observer used to arm plugin monitors on the first successful
    /// dispatch of a matching skill.
    #[must_use]
    pub fn with_skill_invocation_observer(mut self, observer: SkillInvocationObserver) -> Self {
        self.skill_invocation_observer = Some(observer);
        self
    }

    async fn observe_skill_invocation(&self, name: &str) {
        if let Some(observer) = self.skill_invocation_observer.as_ref() {
            observer(name.to_string()).await;
        }
    }

    /// Wire the `UserPromptExpansion` hook (#39): the engine's hook executor
    /// plus a [`HookContext`] provider. After this, expanding a markdown /
    /// MCP-prompt slash command fires the `UserPromptExpansion` event with the
    /// command metadata, faithful to claude-code `WFa`→`b$t` (BIN off
    /// 201270310). A no-op for handler/builtin commands (which are never
    /// "expanded") and a strict no-op when no `UserPromptExpansion` hook is
    /// registered (the executor itself no-ops on an empty match set).
    #[must_use]
    pub fn with_expansion_hooks(
        mut self,
        executor: Arc<HookExecutorImpl>,
        context: ExpansionHookContextProvider,
    ) -> Self {
        self.expansion_hooks = Some(ExpansionHooks { executor, context });
        self
    }

    /// Fire the `UserPromptExpansion` hook for a just-expanded markdown /
    /// MCP-prompt slash command, matching claude-code `WFa`'s firing site
    /// (after building the `/name args` prompt line, before the expanded
    /// prompt is queried). Strict no-op when no expansion hook is wired. The
    /// aggregate is discarded — best-effort, so a failing / blocking
    /// `UserPromptExpansion` hook never breaks dispatch.
    async fn fire_user_prompt_expansion(&self, name: &str, raw_args: &str, source: CommandSource) {
        let Some(eh) = self.expansion_hooks.as_ref() else {
            return;
        };
        // claude-code `o = t ? `/${e.name} ${t}` : `/${e.name}``.
        let prompt = if raw_args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {raw_args}")
        };
        // claude-code `e.source==="mcp" ? "mcp_prompt" : "slash_command"`.
        let expansion_type = if matches!(source, CommandSource::Mcp) {
            PromptExpansionType::McpPrompt
        } else {
            PromptExpansionType::SlashCommand
        };
        let ctx = (eh.context)().await;
        let _ = eh
            .executor
            .execute(
                HookEvent::UserPromptExpansion {
                    expansion_type,
                    command_name: name.to_string(),
                    command_args: raw_args.to_string(),
                    command_source: Some(command_source_str(source).to_string()),
                    prompt,
                },
                ctx,
            )
            .await;
    }

    /// The shared registry handle backing this dispatcher.
    ///
    /// (ARGS.3) Hands the same `Arc<RwLock<CommandRegistry>>` out so a host can
    /// read declared `argNames` once at init (e.g. the TUI's progressive
    /// argument-hint map) without owning a second dispatcher. Cloning the `Arc`
    /// keeps both views pointed at the SAME registry, so later
    /// register/unregister events are visible through either handle.
    #[must_use]
    pub fn registry(&self) -> Arc<RwLock<CommandRegistry>> {
        self.registry.clone()
    }

    /// A second dispatcher pointing at the SAME shared registry.
    ///
    /// Dispatching is identical (both clone the same `Arc<RwLock<CommandRegistry>>`).
    /// Used when a host keeps the original dispatcher (e.g. by value on a runtime
    /// struct) but a second consumer — like the bridge-server's command router —
    /// needs an owned `RegistrySlashDispatcher`/`Arc<dyn SlashCommandDispatcher>`
    /// over the same command set.
    #[must_use]
    pub fn clone_shared(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            // The `UserPromptExpansion` wiring (executor + context provider) is
            // cheap to clone (two `Arc`s) and must travel with the shared
            // dispatcher so every consumer fires the event identically.
            expansion_hooks: self.expansion_hooks.as_ref().map(|eh| ExpansionHooks {
                executor: eh.executor.clone(),
                context: eh.context.clone(),
            }),
            // The shell-expansion provider is a single `Arc`; it must travel with
            // the shared dispatcher so every consumer expands `!`cmd`` bodies
            // identically.
            shell_expansion: self.shell_expansion.clone(),
            skill_usage_home: self.skill_usage_home.clone(),
            background_prompt_launcher: self.background_prompt_launcher.clone(),
            mcp_prompt_resolver: self.mcp_prompt_resolver.clone(),
            skill_invocation_observer: self.skill_invocation_observer.clone(),
            injected_messages_as_turns: self.injected_messages_as_turns,
            prompt_paths: self.prompt_paths.clone(),
            // The gate must travel too: a shared dispatcher that did not clear
            // the command denies would leave the previous skill's restrictions
            // standing for every input routed through the clone.
            permission_gate: self.permission_gate.clone(),
        }
    }

    fn injected_message_result(&self, content: String) -> SlashDispatchResult {
        if self.injected_messages_as_turns {
            SlashDispatchResult::RunAsTurn { prompt: content }
        } else {
            SlashDispatchResult::Handled { display: content }
        }
    }

    /// Return the model-facing command catalog. Commands explicitly hidden from
    /// model invocation are intentionally excluded.
    pub async fn list_commands(&self) -> Vec<crate::model::SlashCommand> {
        let registry = self.registry.read().await;
        let mut commands: Vec<_> = registry
            .model_invocable_commands()
            .into_iter()
            .cloned()
            .collect();
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        commands
    }

    /// Return the live command catalog used by user-facing slash palettes and
    /// settings surfaces. Unlike [`Self::list_commands`], this keeps manual-only
    /// commands, merges dynamic aliases, and excludes only commands that users
    /// cannot invoke in the current environment.
    pub async fn list_palette_commands(&self) -> Vec<crate::model::SlashCommand> {
        let registry = self.registry.read().await;
        let mut commands = registry.palette_commands();
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        commands
    }

    /// Format the locked unknown-command literal.
    ///
    /// Public so callers can render the same string outside the dispatch loop
    /// (e.g. when reporting an error from a `/help` lookup that finds a
    /// dangling alias).
    #[must_use]
    pub fn unknown_command_literal(name: &str) -> String {
        format!("Unknown command: /{name}")
    }
}

#[async_trait]
impl SlashCommandDispatcher for RegistrySlashDispatcher {
    async fn dispatch(&self, raw: &str) -> SlashDispatchResult {
        // 0. `Tbt(setToolPermissionContext, result.disallowedTools ?? [])` —
        //    REPLACE, once per processed user input. Resolved BEFORE the
        //    early-outs below so a plain prompt, an unknown name, or a builtin
        //    all clear the previous command's denies; that clearing is what
        //    gives a skill's `disallowed-tools` its "until the next user
        //    message" lifetime.
        if let Some(gate) = self.permission_gate.as_ref() {
            let denies = self.command_input_denies(raw).await;
            gate.set_command_input_denies(&denies, false);
        }

        // 1. Detect slash prefix.
        if !raw.starts_with('/') {
            return SlashDispatchResult::NotASlashCommand;
        }

        // 2. "/" alone — parse returns Some(name=""), so we still get an
        //    Unknown branch below. But we surface it explicitly so the test
        //    is unambiguous.
        if raw == "/" {
            return SlashDispatchResult::Unknown {
                name: String::new(),
                display: Self::unknown_command_literal(""),
            };
        }

        // 3. Parse into name + args via the existing M1.15 parser.
        //    parse_slash_command expects input WITH leading '/' and returns
        //    Option<ParsedSlashCommand>.
        let Some(parsed) = parse_slash_command(raw) else {
            // parse_slash_command only returns None when the input does not
            // start with '/' — already handled above. Belt-and-braces:
            return SlashDispatchResult::NotASlashCommand;
        };

        // 4. Look up the command.
        let reg = self.registry.read().await;
        let Some(command) = reg.resolve(&parsed.name).cloned() else {
            return SlashDispatchResult::Unknown {
                name: parsed.name.clone(),
                display: Self::unknown_command_literal(&parsed.name),
            };
        };

        // 4a. Honor the `DISABLE_*_COMMAND` env gates for the affected builtins
        //     (claude-code `isEnabled:()=>!je.DISABLE_X`). A disabled command is
        //     dropped from findCommand's search, so it must NOT resolve — fall
        //     through to the Unknown branch. Keyed by the command's canonical
        //     name so a disabled command is unreachable via an alias too.
        if matches!(command.kind, SlashCommandKind::Builtin { .. })
            && crate::builtin_support::names::is_command_env_disabled(&command.name)
        {
            return SlashDispatchResult::Unknown {
                name: parsed.name.clone(),
                display: Self::unknown_command_literal(&parsed.name),
            };
        }

        // Programmatically-registered bundled skills (e.g. `/loop`) are
        // user-invocable (loop.ts:82 `userInvocable: true`). They expand via the
        // dynamic prompt builder (`getPromptForCommand`, loop.ts:84) instead of
        // static markdown templating, so a user-typed `/loop` produces its prompt
        // rather than resolving to Unknown (the bundled kind is not a registered
        // handler). Same display contract + `UserPromptExpansion` firing as the
        // markdown path below.
        if let SlashCommandKind::Bundled {
            frontmatter,
            prompt_fn,
        } = &command.kind
        {
            // The boot-registered instance always carries `prompt_fn`; a
            // deserialized one is inert by design (`#[serde(skip)]`). Clone the
            // builder so the registry lock can be dropped before building.
            let builder = prompt_fn.clone();
            drop(reg);
            return match builder {
                Some(pf) => {
                    let (project_root, cwd) = self.prompt_paths.as_ref().map_or_else(
                        || {
                            let cwd =
                                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                            (cwd.clone(), cwd)
                        },
                        |paths| paths(),
                    );
                    let content =
                        match pf.try_build_at(&parsed.raw_args, &project_root, &cwd, false) {
                            Ok(content) => content,
                            Err(error) => {
                                return SlashDispatchResult::Handled {
                                    display: format!("{} expansion failed: {error}", command.name),
                                }
                            }
                        };
                    self.observe_skill_invocation(&command.name).await;
                    self.fire_user_prompt_expansion(
                        &command.name,
                        &parsed.raw_args,
                        command.source,
                    )
                    .await;
                    if frontmatter.context.as_deref() == Some("fork")
                        && frontmatter.background.unwrap_or(true)
                    {
                        return match self.background_prompt_launcher.as_ref() {
                            Some(launcher) => match launcher(content).await {
                                Ok(display) => SlashDispatchResult::Handled { display },
                                Err(error) => SlashDispatchResult::Handled {
                                    display: format!(
                                        "Could not start /{} in the background: {error}",
                                        command.name
                                    ),
                                },
                            },
                            None => SlashDispatchResult::Handled {
                                display: format!(
                                    "Could not start /{} in the background: no background launcher is wired",
                                    command.name
                                ),
                            },
                        };
                    }
                    // claude-code bundled skills are `type: "prompt"` /
                    // `userInvocable: true` (cc_all.txt:480599): the expanded
                    // prompt becomes the user TURN, not display text. A typed
                    // `/loop 5m /foo` must actually run the model so the cron is
                    // scheduled + the prompt executed now.
                    SlashDispatchResult::RunAsTurn { prompt: content }
                }
                None => SlashDispatchResult::Unknown {
                    name: parsed.name.clone(),
                    display: Self::unknown_command_literal(&parsed.name),
                },
            };
        }

        if let SlashCommandKind::Mcp {
            connection_id,
            prompt_name,
            arguments,
        } = &command.kind
        {
            let connection_id = *connection_id;
            let prompt_name = prompt_name.clone();
            let arguments = arguments.clone();
            let resolver = self.mcp_prompt_resolver.clone();
            drop(reg);

            let wire_arguments =
                match bind_mcp_prompt_arguments(&arguments, &parsed.positional_args) {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        return SlashDispatchResult::Handled {
                            display: format!("{}: {error}", command.name),
                        };
                    }
                };
            let Some(resolver) = resolver else {
                return SlashDispatchResult::Handled {
                    display: format!(
                        "{}: MCP prompt expansion is unavailable in this host",
                        command.name
                    ),
                };
            };

            self.fire_user_prompt_expansion(&command.name, &parsed.raw_args, CommandSource::Mcp)
                .await;
            return match resolver(connection_id, prompt_name, Value::Object(wire_arguments)).await {
                Ok(result) => match render_mcp_prompt_result(&result) {
                    Ok(prompt) => {
                        if let Some(config_home) = self.skill_usage_home.as_deref() {
                            if let Err(error) =
                                crate::skill_usage::record_skill_usage(config_home, &command.name)
                            {
                                tracing::warn!(command = %command.name, %error, "failed to record MCP prompt usage");
                            }
                        }
                        SlashDispatchResult::RunAsTurn { prompt }
                    }
                    Err(error) => SlashDispatchResult::Handled {
                        display: format!("{}: invalid MCP prompt result: {error}", command.name),
                    },
                },
                Err(error) => SlashDispatchResult::Handled {
                    display: format!("{}: MCP prompt expansion failed: {error}", command.name),
                },
            };
        }

        if matches!(
            command.kind,
            SlashCommandKind::Markdown { .. } | SlashCommandKind::Plugin { .. }
        ) {
            drop(reg);
            // This command's frontmatter allow-list + selected shell (both drive
            // the real gate). `command` is an owned clone, so this reads after the
            // registry lock is dropped.
            let (allowed_tools, fm_shell) = match &command.kind {
                SlashCommandKind::Markdown { frontmatter, .. }
                | SlashCommandKind::Plugin { frontmatter, .. } => (
                    frontmatter.allowed_tools.clone().unwrap_or_default(),
                    frontmatter.shell,
                ),
                _ => (Vec::new(), None),
            };
            // Use the real provider when wired AND the command is not MCP-sourced
            // (claude-code `loadedFrom === 'mcp'` NEVER shell-expands). Otherwise
            // keep the strict-no-op stubs (byte-identical default): the stub gate
            // denies any embedded command, but a body with no `!`cmd`` patterns is
            // returned unchanged regardless, so unwired / MCP behavior is preserved.
            let shell_ctx = match self.shell_expansion.as_ref() {
                Some(provider) if !matches!(command.source, CommandSource::Mcp) => {
                    provider.build(&allowed_tools, fm_shell)
                }
                _ => ShellExpansionCtx {
                    runner: Arc::new(UnavailableShellRunner),
                    permission_gate: Arc::new(DenyShellPermissionGate),
                },
            };
            let expand_ctx = ExpandCtx {
                session_id: "",
                shell: &shell_ctx,
            };
            return match expand_markdown_command(&command, &parsed, &expand_ctx).await {
                Ok(content) => {
                    self.observe_skill_invocation(&command.name).await;
                    // `/skill-doctor` reads this persistent counter. Record
                    // file-backed and plugin skills; bundled/managed commands
                    // are not part of its diagnostic surface.
                    if matches!(
                        &command.kind,
                        SlashCommandKind::Markdown { .. } | SlashCommandKind::Plugin { .. }
                    ) {
                        if let Some(config_home) = self.skill_usage_home.as_deref() {
                            if let Err(error) =
                                crate::skill_usage::record_skill_usage(config_home, &command.name)
                            {
                                tracing::warn!(command = %command.name, %error, "failed to record skill usage");
                            }
                        }
                    }
                    // hooks #39: a markdown / MCP-prompt slash command WAS
                    // expanded — fire `UserPromptExpansion` (claude-code `WFa`
                    // →`b$t`) with the command metadata, at the same point the
                    // binary does: after the `/name args` line is formed and
                    // the body resolved, before the expanded prompt is queried.
                    self.fire_user_prompt_expansion(
                        &command.name,
                        &parsed.raw_args,
                        command.source,
                    )
                    .await;
                    // claude-code Markdown / Plugin prompt commands are
                    // `type: "prompt"`: the expanded body is submitted as the
                    // user turn (the model runs it), not printed. Mirrors the
                    // bundled-skill path above.
                    SlashDispatchResult::RunAsTurn { prompt: content }
                }
                // Expansion FAILURE stays display-only (there is no prompt to
                // run) — surface the error text to the user instead.
                Err(e) => SlashDispatchResult::Handled {
                    display: format!("{} expansion failed: {e}", command.name),
                },
            };
        }

        let Some(handler) = reg.get_handler(&parsed.name) else {
            return SlashDispatchResult::Unknown {
                name: parsed.name.clone(),
                display: Self::unknown_command_literal(&parsed.name),
            };
        };

        // 5. Drop the registry lock before awaiting handler (handler may
        //    re-lock the registry or take a while).
        drop(reg);
        let result = handler.handle(&parsed).await;

        match result {
            // M5-09 placeholders only ever return Done — but route the
            // other variants safely anyway so M5-10/M5-11 can extend.
            CommandResult::Done { display } | CommandResult::EmitEffects { display, .. } => {
                SlashDispatchResult::Handled {
                    display: display.unwrap_or_default(),
                }
            }
            CommandResult::InjectMessage { content } => {
                // Builtin prompt commands (`/commit`, `/commit-push-pr`,
                // `/security-review`) carry RAW `!`git …`` / ` ```! ` bodies. When
                // a shell-expansion provider is wired AND the command is not
                // MCP-sourced AND the content actually embeds a shell pattern, run
                // `execute_shell_commands_in_prompt` over it with THIS command's
                // `allowed_tools` (shell = None → Bash for builtins), 1:1 with
                // claude-code expanding the body inside `getPromptForCommand`
                // before it becomes the prompt. On a permission-deny / run failure
                // the WHOLE expansion aborts (patterns are never left in place) —
                // surface the error text instead of the unexpanded template. A
                // strict no-op (verbatim content) when unset / MCP / no patterns.
                let has_embedded = content.contains("```!") || content.contains("!`");
                match self.shell_expansion.as_ref() {
                    Some(provider)
                        if has_embedded && !matches!(command.source, CommandSource::Mcp) =>
                    {
                        let allowed: Vec<String> = handler
                            .allowed_tools()
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect();
                        let shell_ctx = provider.build(&allowed, None);
                        match execute_shell_commands_in_prompt(
                            &content,
                            &shell_ctx,
                            &format!("/{}", command.name),
                            None,
                        )
                        .await
                        {
                            Ok(expanded) => self.injected_message_result(expanded),
                            Err(e) => SlashDispatchResult::Handled {
                                display: format!("{} expansion failed: {e}", command.name),
                            },
                        }
                    }
                    _ => self.injected_message_result(content),
                }
            }
            CommandResult::RequestConfirmation { prompt, .. } => {
                SlashDispatchResult::Handled { display: prompt }
            }
        }
    }
}

fn bind_mcp_prompt_arguments(
    declarations: &[lingxi_core::host::McpPromptArgumentDto],
    positional: &[String],
) -> Result<Map<String, Value>, String> {
    if positional.len() > declarations.len() {
        return Err(format!(
            "expected at most {} argument{}, received {}",
            declarations.len(),
            if declarations.len() == 1 { "" } else { "s" },
            positional.len()
        ));
    }

    let mut bound = Map::new();
    for (index, declaration) in declarations.iter().enumerate() {
        match positional.get(index) {
            Some(value) => {
                bound.insert(declaration.name.clone(), Value::String(value.clone()));
            }
            None if declaration.required => {
                return Err(format!("missing required argument <{}>", declaration.name));
            }
            None => {}
        }
    }
    Ok(bound)
}

fn render_mcp_prompt_result(result: &Value) -> Result<String, String> {
    let messages = result
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages array".to_string())?;
    if messages.is_empty() {
        return Err("messages array is empty".to_string());
    }

    let mut rendered = Vec::with_capacity(messages.len());
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = message
            .get("content")
            .ok_or_else(|| "message is missing content".to_string())?;
        let text = render_mcp_prompt_content(content)?;
        rendered.push((role, text));
    }

    if rendered.len() == 1 && rendered[0].0 == "user" {
        return Ok(rendered.pop().expect("one element").1);
    }
    Ok(rendered
        .into_iter()
        .map(|(role, content)| format!("[{role}]\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn render_mcp_prompt_content(content: &Value) -> Result<String, String> {
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => parts
            .iter()
            .map(render_mcp_prompt_content)
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join("\n")),
        Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("text") => {
            object
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "text content is missing text".to_string())
        }
        Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("resource") => {
            let resource = object
                .get("resource")
                .ok_or_else(|| "resource content is missing resource".to_string())?;
            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                Ok(text.to_string())
            } else {
                serde_json::to_string(resource).map_err(|error| error.to_string())
            }
        }
        other => serde_json::to_string(other).map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_support::names::{core_description, BUILTIN_COMMAND_NAMES};
    use crate::builtin_support::unimplemented::UnimplementedCommandHandler;
    use crate::model::{CommandFrontmatter, CommandSource, SlashCommand, SlashCommandKind};
    use crate::registry::CommandRegistry;
    use std::path::PathBuf;

    /// Pins the per-source provenance token that reaches the
    /// `UserPromptExpansion` hook payload as `command_source`. Byte-locked to
    /// claude-code's own `source` values; `Local` is spelled `"local"` HERE and
    /// `"project, gitignored"` in [`crate::describe`], so the two surfaces must
    /// never be collapsed into one shared label.
    ///
    /// A newly added variant is caught by `command_source_str`'s own exhaustive
    /// `match`, which stops compiling until it is handled.
    #[test]
    fn command_source_str_pins_every_source_token() {
        for (source, expected) in [
            (CommandSource::Builtin, "builtin"),
            (
                CommandSource::Settings(lingxi_core::types::SettingsScope::User),
                "user",
            ),
            (
                CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
                "project",
            ),
            (
                CommandSource::Settings(lingxi_core::types::SettingsScope::Local),
                "local",
            ),
            (CommandSource::Plugin, "plugin"),
            (
                CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
                "managed",
            ),
            (CommandSource::Mcp, "mcp"),
            (CommandSource::Bundled, "bundled"),
        ] {
            assert_eq!(command_source_str(source), expected, "{source:?}");
        }
    }

    // Seed from API scaffolding so these dispatch tests also run without
    // the optional builtin handlers. The stubs preserve the locked responses.
    fn seeded_dispatcher() -> RegistrySlashDispatcher {
        let mut reg = CommandRegistry::new();
        for &name in BUILTIN_COMMAND_NAMES {
            reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
                name,
                core_description(name),
            )));
        }
        RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
    }

    /// Like [`seeded_dispatcher`] but also wires the `continue` → `resume` alias
    /// normally registered by the optional builtin handlers. Proves the alias survives
    /// the real dispatch path end-to-end.
    fn seeded_dispatcher_with_resume_alias() -> RegistrySlashDispatcher {
        let mut reg = CommandRegistry::new();
        for &name in BUILTIN_COMMAND_NAMES {
            reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
                name,
                core_description(name),
            )));
        }
        reg.register_alias("continue".to_string(), "resume".to_string());
        RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
    }

    #[tokio::test]
    async fn dispatches_known_command_to_locked_stub() {
        let d = seeded_dispatcher();
        let result = d.dispatch("/teleport").await;
        match result {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display, "teleport: not implemented in v0.6.0 (M5)");
            }
            other => panic!("expected Handled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatches_known_command_with_args_to_locked_stub() {
        let d = seeded_dispatcher();
        let result = d.dispatch("/clear --force").await;
        match result {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display, "clear: not implemented in v0.6.0 (M5)");
            }
            other => panic!("expected Handled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatches_markdown_command_via_registry_resolve() {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "demo".to_string(),
            description: "Demo".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/demo.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "Use $ARGUMENTS".to_string(),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        // Markdown prompt commands run AS a turn (claude-code `type: "prompt"`).
        match d.dispatch("/demo this").await {
            SlashDispatchResult::RunAsTurn { prompt } => assert_eq!(prompt, "Use this"),
            other => panic!("expected markdown command to run-as-turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatches_bundled_skill_via_dynamic_prompt_fn() {
        // Regression: a user-typed bundled skill (`/loop`) must expand via its
        // dynamic builder, NOT resolve to Unknown (it is not a registered
        // handler and not a Markdown kind). Mirrors loop.ts's empty→usage vs
        // non-empty→buildPrompt branch.
        struct B;
        impl crate::model::BundledPromptFn for B {
            fn build(&self, args: &str) -> String {
                if args.trim().is_empty() {
                    "USAGE".to_string()
                } else {
                    format!("BUILT[{}]", args.trim())
                }
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "loop".to_string(),
            description: "Loop".to_string(),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter::default(),
                prompt_fn: Some(Arc::new(B)),
            },
            loaded_from: Some("bundled".to_string()),
            user_invocable: Some(true),
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        // Non-empty args → built prompt, run AS a turn (claude-code bundled
        // skills are `type: "prompt"` / `userInvocable`).
        match d.dispatch("/loop 5m /babysit-prs").await {
            SlashDispatchResult::RunAsTurn { prompt } => {
                assert_eq!(prompt, "BUILT[5m /babysit-prs]");
            }
            other => panic!("expected bundled skill to run-as-turn, got {other:?}"),
        }
        // Empty args → usage. Still RunAsTurn (the model receives the usage as
        // its prompt and surfaces it) — never Unknown.
        match d.dispatch("/loop").await {
            SlashDispatchResult::RunAsTurn { prompt } => assert_eq!(prompt, "USAGE"),
            other => panic!("expected usage run-as-turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_context_bundled_skill_uses_background_launcher_not_main_turn() {
        struct ReviewPrompt;
        impl crate::model::BundledPromptFn for ReviewPrompt {
            fn build(&self, args: &str) -> String {
                format!("REVIEW[{}]", args.trim())
            }
        }

        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "code-review".to_string(),
            description: "Review".to_string(),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter {
                    context: Some("fork".to_string()),
                    background: Some(true),
                    ..CommandFrontmatter::default()
                },
                prompt_fn: Some(Arc::new(ReviewPrompt)),
            },
            loaded_from: Some("bundled".to_string()),
            user_invocable: Some(true),
            ..SlashCommand::default()
        });
        let launched = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = launched.clone();
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_background_prompt_launcher(Arc::new(move |prompt| {
                let captured = captured.clone();
                Box::pin(async move {
                    captured.lock().await.push(prompt);
                    Ok("started review agent".to_string())
                })
            }));

        assert_eq!(
            d.dispatch("/code-review high --fix src").await,
            SlashDispatchResult::Handled {
                display: "started review agent".to_string()
            }
        );
        assert_eq!(launched.lock().await.as_slice(), ["REVIEW[high --fix src]"]);
    }

    #[tokio::test]
    async fn fork_context_bundled_skill_fails_closed_without_launcher() {
        struct ReviewPrompt;
        impl crate::model::BundledPromptFn for ReviewPrompt {
            fn build(&self, _: &str) -> String {
                "review".to_string()
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "code-review".to_string(),
            description: "Review".to_string(),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter {
                    context: Some("fork".to_string()),
                    background: Some(true),
                    ..CommandFrontmatter::default()
                },
                prompt_fn: Some(Arc::new(ReviewPrompt)),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));

        match d.dispatch("/code-review").await {
            SlashDispatchResult::Handled { display } => {
                assert!(display.contains("no background launcher is wired"));
            }
            other => panic!("fork-context command must never run inline: {other:?}"),
        }
    }

    #[tokio::test]
    async fn markdown_command_shadows_same_named_builtin_in_dispatch() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
            "commit",
            core_description("commit"),
        )));
        reg.register_command(SlashCommand {
            name: "commit".to_string(),
            description: "Custom commit".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/commit.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "Custom $ARGUMENTS".to_string(),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        match d.dispatch("/commit now").await {
            SlashDispatchResult::RunAsTurn { prompt } => assert_eq!(prompt, "Custom now"),
            other => panic!("expected custom markdown run-as-turn, got {other:?}"),
        }
    }

    /// End-to-end guard: the `continue` alias survives the real dispatch path.
    /// `/continue` must route through the alias-aware `get_handler` to the
    /// `resume` handler — yielding `Handled`, not `Unknown`. (Mirrors
    /// claude-code's `aliases: ['continue']` on `/resume`.)
    #[tokio::test]
    async fn dispatches_continue_alias_to_resume() {
        let d = seeded_dispatcher_with_resume_alias();
        let result = d.dispatch("/continue").await;
        match result {
            SlashDispatchResult::Handled { display } => {
                // Seeded with the stub handler under "resume", so the alias
                // routes there and we get the resume stub literal back.
                assert_eq!(display, "resume: not implemented in v0.6.0 (M5)");
            }
            other => panic!("expected Handled for /continue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_command_returns_locked_literal() {
        let d = seeded_dispatcher();
        let result = d.dispatch("/notacommand").await;
        match result {
            SlashDispatchResult::Unknown { name, display } => {
                assert_eq!(name, "notacommand");
                assert_eq!(display, "Unknown command: /notacommand");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// #63: a builtin whose `DISABLE_*_COMMAND` env gate is tripped does NOT
    /// resolve — it falls through to the Unknown branch, mirroring claude-code's
    /// `isEnabled:()=>!je.DISABLE_X` dropping it from findCommand.
    #[tokio::test]
    async fn env_disabled_builtin_does_not_resolve() {
        let _g = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        let var = "DISABLE_DOCTOR_COMMAND";
        let d = seeded_dispatcher();

        // Default: /doctor resolves to its (stub) handler → Handled.
        std::env::remove_var(var);
        assert!(matches!(
            d.dispatch("/doctor").await,
            SlashDispatchResult::Handled { .. }
        ));

        // Gate tripped: /doctor is Unknown.
        std::env::set_var(var, "1");
        match d.dispatch("/doctor").await {
            SlashDispatchResult::Unknown { name, display } => {
                assert_eq!(name, "doctor");
                assert_eq!(display, "Unknown command: /doctor");
            }
            other => panic!("expected Unknown for gated /doctor, got {other:?}"),
        }
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn non_slash_input_is_not_a_slash_command() {
        let d = seeded_dispatcher();
        let result = d.dispatch("hello world").await;
        assert!(matches!(result, SlashDispatchResult::NotASlashCommand));
    }

    #[tokio::test]
    async fn empty_input_is_not_a_slash_command() {
        let d = seeded_dispatcher();
        let result = d.dispatch("").await;
        assert!(matches!(result, SlashDispatchResult::NotASlashCommand));
    }

    #[tokio::test]
    async fn just_slash_is_not_a_command() {
        let d = seeded_dispatcher();
        let result = d.dispatch("/").await;
        // After stripping the leading '/', the name is empty — treat as unknown.
        match result {
            SlashDispatchResult::Unknown { name, display } => {
                assert_eq!(name, "");
                assert_eq!(display, "Unknown command: /");
            }
            other => panic!("expected Unknown for '/', got {other:?}"),
        }
    }

    #[tokio::test]
    async fn uppercase_name_is_unknown() {
        let d = seeded_dispatcher();
        // Per Task 0 step 4 L3 expansion: the parser does NOT case-fold; the
        // registry only holds lowercase keys.
        let result = d.dispatch("/CLEAR").await;
        match result {
            SlashDispatchResult::Unknown { name, display } => {
                assert_eq!(name, "CLEAR");
                assert_eq!(display, "Unknown command: /CLEAR");
            }
            other => panic!("expected Unknown for /CLEAR, got {other:?}"),
        }
    }

    /// (ARGS.3) `registry()` hands back an `Arc` to the SAME shared registry:
    /// a markdown command registered through the dispatcher's `Arc` is visible
    /// through the returned handle, proving the live argument-hint seam reads
    /// the dispatcher's actual command set.
    #[tokio::test]
    async fn registry_accessor_shares_the_same_registry() {
        use crate::model::{CommandSource, SlashCommand, SlashCommandKind};

        let shared = Arc::new(RwLock::new(CommandRegistry::new()));
        let d = RegistrySlashDispatcher::new(shared.clone());

        // Register a markdown command (with argNames) via the ORIGINAL Arc.
        shared.write().await.register_command(SlashCommand {
            name: "deploy".to_string(),
            description: "Deploy".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: std::path::PathBuf::from("/x/deploy.md"),
                frontmatter: crate::model::CommandFrontmatter::default(),
                prompt_template: String::new(),
            },
            argument_names: vec!["env".to_string(), "region".to_string()],
            ..SlashCommand::default()
        });

        // The accessor's handle observes that same command + its argNames.
        let via_accessor = d.registry();
        let guard = via_accessor.read().await;
        let cmd = guard
            .resolve("deploy")
            .expect("command visible via registry()");
        assert_eq!(
            cmd.argument_names,
            vec!["env".to_string(), "region".to_string()]
        );
    }

    #[tokio::test]
    async fn list_commands_excludes_model_hidden_entries() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "visible".to_string(),
            description: "Visible".to_string(),
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "hidden".to_string(),
            description: "Hidden".to_string(),
            disable_model_invocation: true,
            ..SlashCommand::default()
        });

        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_commands().await;

        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            vec!["visible"]
        );
    }

    #[tokio::test]
    async fn list_palette_commands_keeps_manual_only_and_aliases() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "review".to_string(),
            description: "Review".to_string(),
            disable_model_invocation: true,
            aliases: vec!["rv".to_string()],
            argument_hint: Some("[target]".to_string()),
            ..SlashCommand::default()
        });
        registry.register_alias("check".to_string(), "review".to_string());

        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_palette_commands().await;

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(
            commands[0].aliases,
            vec!["check".to_string(), "rv".to_string()]
        );
        assert_eq!(commands[0].argument_hint.as_deref(), Some("[target]"));
    }

    #[tokio::test]
    async fn list_palette_commands_keeps_hidden_entries_for_exact_matching() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "auto-mode-setup".to_string(),
            description: "Hidden in bare palette".to_string(),
            ..SlashCommand::default()
        });

        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_palette_commands().await;

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "auto-mode-setup");
    }

    #[tokio::test]
    async fn list_palette_commands_excludes_non_user_and_env_disabled() {
        let _guard = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "hidden-skill".to_string(),
            description: "Hidden".to_string(),
            user_invocable: Some(false),
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "doctor".to_string(),
            description: "Doctor".to_string(),
            source: CommandSource::Builtin,
            ..SlashCommand::default()
        });
        std::env::set_var("DISABLE_DOCTOR_COMMAND", "1");
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_palette_commands().await;
        std::env::remove_var("DISABLE_DOCTOR_COMMAND");

        assert!(commands.is_empty());
    }

    // ---- #39 UserPromptExpansion firing (R-O1) ----
    //
    // claude-code fires `UserPromptExpansion` (`WFa`→`b$t`, BIN off 201270310)
    // the moment a markdown / MCP-prompt slash command is expanded, with the
    // command metadata: `expansion_type` (`slash_command` | `mcp_prompt`),
    // `command_name`, `command_args` (the raw arg string), `command_source`
    // (the command's provenance), and `prompt` (the `/name args` line). These
    // tests prove the dispatcher fires that event at the markdown-expansion
    // site once an executor + context provider are wired, and is a strict
    // no-op otherwise.

    use async_trait::async_trait;
    use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
    use hooks::events::HookEventType;
    use hooks::executor::BuiltinHookHandler;
    use hooks::response::{HookOutcome, HookResult};
    use hooks::HookRegistry;
    use lingxi_core::types::{HookId, HttpRequest, HttpResponse};
    use std::time::Duration;

    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: HttpRequest,
        ) -> Result<HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }
    struct UnusedRuntime;
    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _d: Duration) {}
        async fn cancel(
            &self,
            _h: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    /// One captured `UserPromptExpansion` payload.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct FiredExpansion {
        expansion_type: PromptExpansionType,
        command_name: String,
        command_args: String,
        command_source: Option<String>,
        prompt: String,
    }

    type ExpansionLog = std::sync::Mutex<Vec<FiredExpansion>>;

    struct RecordingExpansionHandler {
        log: Arc<ExpansionLog>,
    }
    #[async_trait]
    impl BuiltinHookHandler for RecordingExpansionHandler {
        fn id(&self) -> &str {
            "record-user-prompt-expansion"
        }
        async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
            if let HookEvent::UserPromptExpansion {
                expansion_type,
                command_name,
                command_args,
                command_source,
                prompt,
            } = event
            {
                self.log.lock().unwrap().push(FiredExpansion {
                    expansion_type: *expansion_type,
                    command_name: command_name.clone(),
                    command_args: command_args.clone(),
                    command_source: command_source.clone(),
                    prompt: prompt.clone(),
                });
            }
            HookResult {
                outcome: HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                response: None,
            }
        }
    }

    fn user_prompt_expansion_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "record-user-prompt-expansion".into(),
            events: vec![HookEventType::UserPromptExpansion],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "record-user-prompt-expansion".into(),
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

    /// Build a `HookExecutorImpl` carrying a registered `UserPromptExpansion`
    /// hook + the recording handler, returning it with the shared log.
    async fn recording_expansion_executor() -> (Arc<HookExecutorImpl>, Arc<ExpansionLog>) {
        let log: Arc<ExpansionLog> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        registry
            .write()
            .await
            .register(user_prompt_expansion_hook());
        let mut exec =
            HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        exec.register_builtin(Arc::new(RecordingExpansionHandler { log: log.clone() }));
        (Arc::new(exec), log)
    }

    fn registry_with_demo_markdown(source: CommandSource) -> Arc<RwLock<CommandRegistry>> {
        use crate::model::{CommandFrontmatter, SlashCommand};
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "demo".to_string(),
            description: "Demo".to_string(),
            source,
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/demo.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "Use $ARGUMENTS".to_string(),
            },
            ..SlashCommand::default()
        });
        Arc::new(RwLock::new(reg))
    }

    #[tokio::test]
    async fn successful_user_skill_expansion_records_usage() {
        let root = std::env::temp_dir().join(format!(
            "command-dispatch-skill-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dispatcher = RegistrySlashDispatcher::new(registry_with_demo_markdown(
            CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        ))
        .with_skill_usage_home(root.clone());

        assert!(matches!(
            dispatcher.dispatch("/demo one").await,
            SlashDispatchResult::RunAsTurn { .. }
        ));
        assert!(matches!(
            dispatcher.dispatch("/demo two").await,
            SlashDispatchResult::RunAsTurn { .. }
        ));
        let usage = crate::skill_usage::read_skill_usage(&root).unwrap();
        assert_eq!(usage.get("demo").map(|entry| entry.count), Some(2));
        std::fs::remove_dir_all(root).ok();
    }

    /// A wired dispatcher fires `UserPromptExpansion` when it expands a markdown
    /// slash command, carrying the byte-faithful metadata (claude-code `WFa`).
    #[tokio::test]
    async fn expanding_a_markdown_command_fires_user_prompt_expansion() {
        let (exec, log) = recording_expansion_executor().await;
        let ctx_session = lingxi_core::types::SessionId::new();
        let provider: ExpansionHookContextProvider = Arc::new(move || {
            Box::pin(async move {
                HookContext {
                    session_id: ctx_session,
                    ..Default::default()
                }
            })
        });
        let d = RegistrySlashDispatcher::new(registry_with_demo_markdown(CommandSource::Settings(
            lingxi_core::types::SettingsScope::Project,
        )))
        .with_expansion_hooks(exec, provider);

        // Expansion still produces the body, AND the hook fires.
        match d.dispatch("/demo this and that").await {
            SlashDispatchResult::RunAsTurn { prompt } => {
                assert_eq!(prompt, "Use this and that")
            }
            other => panic!("expected markdown expansion, got {other:?}"),
        }

        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![FiredExpansion {
                expansion_type: PromptExpansionType::SlashCommand,
                command_name: "demo".to_string(),
                command_args: "this and that".to_string(),
                command_source: Some("project".to_string()),
                // claude-code `o = `/${e.name} ${t}`` — the command LINE, not
                // the expanded body.
                prompt: "/demo this and that".to_string(),
            }],
            "UserPromptExpansion must fire once with byte-faithful metadata: {seen:?}"
        );
    }

    /// An MCP-prompt-sourced command reports `expansion_type: mcp_prompt` and
    /// `command_source: "mcp"` (claude-code `e.source==="mcp"?"mcp_prompt":…`).
    #[tokio::test]
    async fn expanding_an_mcp_prompt_command_reports_mcp_expansion_type() {
        let (exec, log) = recording_expansion_executor().await;
        let provider: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let d = RegistrySlashDispatcher::new(registry_with_demo_markdown(CommandSource::Mcp))
            .with_expansion_hooks(exec, provider);

        let _ = d.dispatch("/demo arg").await;

        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "exactly one expansion: {seen:?}");
        assert_eq!(seen[0].expansion_type, PromptExpansionType::McpPrompt);
        assert_eq!(seen[0].command_source, Some("mcp".to_string()));
        assert_eq!(seen[0].prompt, "/demo arg");
    }

    fn registry_with_mcp_prompt(connection_id: McpConnectionId) -> Arc<RwLock<CommandRegistry>> {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "github:review".to_string(),
            description: "Review a pull request".to_string(),
            source: CommandSource::Mcp,
            kind: SlashCommandKind::Mcp {
                connection_id,
                prompt_name: "review".to_string(),
                arguments: vec![
                    lingxi_core::host::McpPromptArgumentDto {
                        name: "repository".to_string(),
                        description: None,
                        required: true,
                    },
                    lingxi_core::host::McpPromptArgumentDto {
                        name: "focus".to_string(),
                        description: None,
                        required: false,
                    },
                ],
            },
            ..SlashCommand::default()
        });
        Arc::new(RwLock::new(registry))
    }

    #[tokio::test]
    async fn mcp_prompt_dispatch_binds_arguments_and_runs_rendered_prompt() {
        let connection_id = McpConnectionId::new();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let resolver_seen = seen.clone();
        let resolver: McpPromptResolver = Arc::new(move |id, name, arguments| {
            let seen = resolver_seen.clone();
            Box::pin(async move {
                *seen.lock().unwrap() = Some((id, name, arguments));
                Ok(serde_json::json!({
                    "messages": [{
                        "role": "user",
                        "content": {"type": "text", "text": "Review src/lib.rs"}
                    }]
                }))
            })
        });
        let dispatcher = RegistrySlashDispatcher::new(registry_with_mcp_prompt(connection_id))
            .with_mcp_prompt_resolver(resolver);

        match dispatcher
            .dispatch("/github:review lingxi \"unsafe code\"")
            .await
        {
            SlashDispatchResult::RunAsTurn { prompt } => {
                assert_eq!(prompt, "Review src/lib.rs");
            }
            other => panic!("expected MCP prompt turn, got {other:?}"),
        }
        let call = seen.lock().unwrap().clone().expect("resolver call");
        assert_eq!(call.0, connection_id);
        assert_eq!(call.1, "review");
        assert_eq!(
            call.2,
            serde_json::json!({"repository": "lingxi", "focus": "unsafe code"})
        );
    }

    #[tokio::test]
    async fn mcp_prompt_missing_required_argument_fails_before_wire_call() {
        let connection_id = McpConnectionId::new();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resolver_called = called.clone();
        let resolver: McpPromptResolver = Arc::new(move |_, _, _| {
            resolver_called.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { unreachable!("required argument must fail before resolver") })
        });
        let dispatcher = RegistrySlashDispatcher::new(registry_with_mcp_prompt(connection_id))
            .with_mcp_prompt_resolver(resolver);

        match dispatcher.dispatch("/github:review").await {
            SlashDispatchResult::Handled { display } => {
                assert!(display.contains("missing required argument <repository>"));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A command with NO args reports an empty `command_args` and the bare
    /// `/name` prompt (claude-code `o = `/${e.name}`` when `t` is empty).
    #[tokio::test]
    async fn expansion_with_no_args_uses_bare_command_prompt() {
        let (exec, log) = recording_expansion_executor().await;
        let provider: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let d = RegistrySlashDispatcher::new(registry_with_demo_markdown(CommandSource::Settings(
            lingxi_core::types::SettingsScope::User,
        )))
        .with_expansion_hooks(exec, provider);

        let _ = d.dispatch("/demo").await;

        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "exactly one expansion: {seen:?}");
        assert_eq!(seen[0].command_args, "");
        assert_eq!(seen[0].command_source, Some("user".to_string()));
        assert_eq!(seen[0].prompt, "/demo");
    }

    /// Without `with_expansion_hooks` wired, expanding a markdown command is a
    /// strict no-op for `UserPromptExpansion` — the default for every existing
    /// dispatcher caller. (The expansion itself still happens.)
    #[tokio::test]
    async fn unwired_dispatcher_does_not_fire_user_prompt_expansion() {
        let (exec, log) = recording_expansion_executor().await;
        // Note: executor exists but is NOT wired into the dispatcher.
        let _ = exec;
        let d = RegistrySlashDispatcher::new(registry_with_demo_markdown(CommandSource::Settings(
            lingxi_core::types::SettingsScope::Project,
        )));

        match d.dispatch("/demo x").await {
            SlashDispatchResult::RunAsTurn { prompt } => assert_eq!(prompt, "Use x"),
            other => panic!("expected markdown expansion, got {other:?}"),
        }

        assert!(
            log.lock().unwrap().is_empty(),
            "an unwired dispatcher must not fire UserPromptExpansion"
        );
    }

    /// A non-markdown (builtin handler) command never "expands", so it must NOT
    /// fire `UserPromptExpansion` even when the hook is wired — faithful to
    /// claude-code, where `b$t` fires only from the command-EXPANSION path.
    #[tokio::test]
    async fn builtin_command_does_not_fire_user_prompt_expansion() {
        let (exec, log) = recording_expansion_executor().await;
        let provider: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));

        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
            "clear",
            core_description("clear"),
        )));
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_expansion_hooks(exec, provider);

        let _ = d.dispatch("/clear").await;

        assert!(
            log.lock().unwrap().is_empty(),
            "a builtin handler command must not fire UserPromptExpansion"
        );
    }

    // ---- #3 embedded shell-expansion wiring ----
    //
    // A wired `ShellExpansionProvider` expands `!`cmd`` bodies in BOTH the
    // markdown/plugin arm AND the builtin `InjectMessage` arm, injecting that
    // command's `allowed_tools`; a permission-deny aborts the whole prompt; and
    // MCP-sourced commands are NEVER handed to the real provider (carve-out).
    // An UNWIRED dispatcher stays a strict no-op (covered by the markdown/
    // builtin tests above, which never wire `with_shell_expansion`).

    /// Echoes `OUT[<cmd>]` as stdout for any command.
    struct EchoShellRunner;
    #[async_trait]
    impl ShellRunner for EchoShellRunner {
        async fn run(
            &self,
            command: &str,
            _shell: Option<crate::FrontmatterShell>,
        ) -> Result<ShellOut, ShellRunError> {
            Ok(ShellOut {
                stdout: format!("OUT[{command}]"),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }
    struct AllowGate;
    impl ShellPermissionGate for AllowGate {
        fn check(&self, _c: &str, _s: Option<crate::FrontmatterShell>) -> ShellPermissionDecision {
            ShellPermissionDecision::Allow
        }
    }
    struct DenyGate;
    impl ShellPermissionGate for DenyGate {
        fn check(&self, _c: &str, _s: Option<crate::FrontmatterShell>) -> ShellPermissionDecision {
            ShellPermissionDecision::Deny {
                message: Some("not allowed".to_string()),
            }
        }
    }
    /// Fake provider: records the `allowed_tools` it is built with and returns an
    /// echo runner behind an allow- or deny-gate.
    struct FakeProvider {
        deny: bool,
        seen_allowed: std::sync::Mutex<Vec<Vec<String>>>,
    }
    impl ShellExpansionProvider for FakeProvider {
        fn build(
            &self,
            allowed_tools: &[String],
            _shell: Option<crate::FrontmatterShell>,
        ) -> ShellExpansionCtx {
            self.seen_allowed
                .lock()
                .unwrap()
                .push(allowed_tools.to_vec());
            let permission_gate: Arc<dyn ShellPermissionGate> = if self.deny {
                Arc::new(DenyGate)
            } else {
                Arc::new(AllowGate)
            };
            ShellExpansionCtx {
                runner: Arc::new(EchoShellRunner),
                permission_gate,
            }
        }
    }

    /// A builtin handler returning `InjectMessage` with an embedded `!`cmd`` body
    /// and a declared allow-list (mirrors `/commit`'s shape).
    struct EmbeddedInjectHandler;
    #[async_trait]
    impl crate::model::BuiltinCommandHandler for EmbeddedInjectHandler {
        async fn handle(&self, _args: &crate::parser::ParsedSlashCommand) -> CommandResult {
            CommandResult::InjectMessage {
                content: "commit: !`git status` now".to_string(),
            }
        }
        fn name(&self) -> &str {
            "commit"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn allowed_tools(&self) -> &'static [&'static str] {
            &["Bash(git status:*)"]
        }
    }

    fn markdown_with(
        source: CommandSource,
        body: &str,
        allowed: Option<Vec<String>>,
    ) -> CommandRegistry {
        use crate::model::CommandFrontmatter;
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "demo".to_string(),
            description: "Demo".to_string(),
            source,
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/x/demo.md"),
                frontmatter: CommandFrontmatter {
                    allowed_tools: allowed,
                    ..CommandFrontmatter::default()
                },
                prompt_template: body.to_string(),
            },
            ..SlashCommand::default()
        });
        reg
    }

    /// A wired provider expands a markdown command's `!`echo hi`` to the runner's
    /// stdout, and the frontmatter `allowed_tools` reaches `provider.build`.
    #[tokio::test]
    async fn wired_provider_expands_markdown_embedded_shell() {
        let reg = markdown_with(
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            "before !`echo hi` after",
            Some(vec!["Bash(echo:*)".to_string()]),
        );
        let provider = Arc::new(FakeProvider {
            deny: false,
            seen_allowed: std::sync::Mutex::new(Vec::new()),
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_shell_expansion(provider.clone());
        match d.dispatch("/demo").await {
            SlashDispatchResult::RunAsTurn { prompt } => {
                assert_eq!(prompt, "before OUT[echo hi] after");
            }
            other => panic!("expected expanded run-as-turn, got {other:?}"),
        }
        assert_eq!(
            provider.seen_allowed.lock().unwrap().as_slice(),
            &[vec!["Bash(echo:*)".to_string()]],
            "the frontmatter allow-list must reach the provider"
        );
    }

    /// A permission-deny aborts the whole prompt: the markdown arm surfaces the
    /// byte-exact expansion-failure display (patterns never left in place).
    #[tokio::test]
    async fn wired_deny_provider_aborts_markdown_expansion() {
        let reg = markdown_with(
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            "before !`echo hi` after",
            None,
        );
        let provider = Arc::new(FakeProvider {
            deny: true,
            seen_allowed: std::sync::Mutex::new(Vec::new()),
        });
        let d =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg))).with_shell_expansion(provider);
        match d.dispatch("/demo").await {
            SlashDispatchResult::Handled { display } => assert_eq!(
                display,
                "demo expansion failed: Shell command permission check failed for pattern \"!`echo hi`\": not allowed"
            ),
            other => panic!("expected expansion-failed display, got {other:?}"),
        }
    }

    /// A wired provider expands a builtin `InjectMessage` body, injecting the
    /// handler's `allowed_tools`.
    #[tokio::test]
    async fn wired_provider_expands_builtin_injectmessage() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(EmbeddedInjectHandler));
        let provider = Arc::new(FakeProvider {
            deny: false,
            seen_allowed: std::sync::Mutex::new(Vec::new()),
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_shell_expansion(provider.clone());
        match d.dispatch("/commit").await {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display, "commit: OUT[git status] now");
            }
            other => panic!("expected expanded InjectMessage, got {other:?}"),
        }
        assert_eq!(
            provider.seen_allowed.lock().unwrap().as_slice(),
            &[vec!["Bash(git status:*)".to_string()]],
            "the handler's allow-list must reach the provider"
        );
    }

    /// An unwired builtin `InjectMessage` command delivers its RAW `!`cmd``
    /// template verbatim — strict no-op (no expansion), byte-identical to today.
    #[tokio::test]
    async fn unwired_builtin_injectmessage_is_verbatim() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(EmbeddedInjectHandler));
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        match d.dispatch("/commit").await {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display, "commit: !`git status` now");
            }
            other => panic!("expected verbatim InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn thin_client_builtin_injectmessage_runs_as_a_turn() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(EmbeddedInjectHandler));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_injected_messages_as_turns();

        match dispatcher.dispatch("/commit").await {
            SlashDispatchResult::RunAsTurn { prompt } => {
                assert_eq!(prompt, "commit: !`git status` now");
            }
            other => panic!("expected thin-client InjectMessage turn, got {other:?}"),
        }
    }

    /// MCP carve-out: even with a wired ALLOW provider, an MCP-sourced markdown
    /// command's body is NEVER handed to the real provider (`loadedFrom==='mcp'`)
    /// — it falls to the deny stub, so `provider.build` is never called.
    #[tokio::test]
    async fn wired_provider_skips_mcp_markdown_expansion() {
        let reg = markdown_with(CommandSource::Mcp, "x !`echo hi` y", None);
        let provider = Arc::new(FakeProvider {
            deny: false,
            seen_allowed: std::sync::Mutex::new(Vec::new()),
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_shell_expansion(provider.clone());
        match d.dispatch("/demo").await {
            SlashDispatchResult::Handled { display } => {
                assert!(display.contains("expansion failed"), "got {display:?}");
            }
            other => panic!("expected stub-denied MCP expansion, got {other:?}"),
        }
        assert!(
            provider.seen_allowed.lock().unwrap().is_empty(),
            "the real provider must never be built for an MCP-sourced command"
        );
    }
    #[tokio::test]
    async fn bundled_checked_expansion_receives_paths_and_surfaces_read_errors() {
        struct Checked;
        impl crate::BundledPromptFn for Checked {
            fn build(&self, _: &str) -> String {
                panic!("unchecked expansion");
            }
            fn try_build_at(
                &self,
                args: &str,
                root: &std::path::Path,
                cwd: &std::path::Path,
                preload: bool,
            ) -> std::io::Result<String> {
                assert_eq!(args, "task");
                assert_eq!(root, std::path::Path::new("/project"));
                assert_eq!(cwd, std::path::Path::new("/project/subdir"));
                assert!(!preload);
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "loop.md denied",
                ))
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(crate::SlashCommand {
            name: "checked-loop".into(),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: Some(Arc::new(Checked)),
            },
            user_invocable: Some(true),
            ..Default::default()
        });
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_prompt_paths(Arc::new(|| {
                (PathBuf::from("/project"), PathBuf::from("/project/subdir"))
            }));
        assert!(matches!(dispatcher.dispatch("/checked-loop task").await,
            SlashDispatchResult::Handled { display } if display == "checked-loop expansion failed: loop.md denied"));
    }
    // ---- MP-1: `disallowed-tools` reaches the permission gate --------------

    /// Records every `set_command_input_denies` call the dispatcher makes.
    #[derive(Default)]
    struct RecordingDenyGate {
        calls: std::sync::Mutex<Vec<(Vec<String>, bool)>>,
    }
    #[async_trait]
    impl PermissionGate for RecordingDenyGate {
        async fn check(
            &self,
            _name: &str,
            _input: &Value,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            lingxi_core::host::permission_gate::PermissionDecision::Allow
        }
        fn set_command_input_denies(&self, specs: &[String], union: bool) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((specs.to_vec(), union));
        }
    }

    fn dispatcher_with_recording_gate() -> (RegistrySlashDispatcher, Arc<RecordingDenyGate>) {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "tight".to_string(),
            description: "A skill that narrows its own tools".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/tight.md"),
                frontmatter: CommandFrontmatter {
                    disallowed_tools: Some(vec!["Bash".to_string(), "Write".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_template: "body".to_string(),
            },
            ..SlashCommand::default()
        });
        reg.register_command(SlashCommand {
            name: "open".to_string(),
            description: "A skill that narrows nothing".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/open.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "body".to_string(),
            },
            ..SlashCommand::default()
        });
        let gate = Arc::new(RecordingDenyGate::default());
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_permission_gate(gate.clone());
        (d, gate)
    }

    #[tokio::test]
    async fn an_inline_skills_disallowed_tools_reaches_the_gate() {
        let (d, gate) = dispatcher_with_recording_gate();
        let _ = d.dispatch("/tight").await;
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![(vec!["Bash".to_string(), "Write".to_string()], false)],
            "the frontmatter denies must be published in REPLACE mode, once"
        );
    }

    #[tokio::test]
    async fn every_other_input_clears_the_previous_skills_denies() {
        // The clearing is the half that gives `disallowed-tools` a lifetime.
        // Upstream runs `Tbt` on every PROCESSED user input, so a plain prompt,
        // an unknown name, a builtin and a command declaring nothing all publish
        // an empty list.
        let (d, gate) = dispatcher_with_recording_gate();
        let _ = d.dispatch("/tight").await;
        let _ = d.dispatch("just a normal prompt").await;
        let _ = d.dispatch("/open").await;
        let _ = d.dispatch("/no-such-command").await;
        let _ = d.dispatch("/").await;
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                (vec!["Bash".to_string(), "Write".to_string()], false),
                (Vec::new(), false),
                (Vec::new(), false),
                (Vec::new(), false),
                (Vec::new(), false),
            ],
            "every input after the skill must clear what it denied"
        );
    }

    #[tokio::test]
    async fn a_dispatcher_with_no_gate_is_unchanged() {
        // Every existing host wires none; the field must stay inert for them.
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "tight".to_string(),
            description: "d".to_string(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/tight.md"),
                frontmatter: CommandFrontmatter {
                    disallowed_tools: Some(vec!["Bash".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_template: "body".to_string(),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        assert!(matches!(
            d.dispatch("/tight").await,
            SlashDispatchResult::RunAsTurn { .. }
        ));
    }

    #[tokio::test]
    async fn a_shared_clone_keeps_publishing_the_denies() {
        // A clone that dropped the gate would leave the previous skill's denies
        // standing for every input routed through it.
        let (d, gate) = dispatcher_with_recording_gate();
        let shared = d.clone_shared();
        let _ = shared.dispatch("/tight").await;
        assert_eq!(
            gate.calls.lock().unwrap().len(),
            1,
            "clone_shared must carry the gate"
        );
    }
}
