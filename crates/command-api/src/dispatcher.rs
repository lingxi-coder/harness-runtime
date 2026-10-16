//! Implementation of [`lingxi_core::host::SlashCommandDispatcher`] that routes
//! `/<name> <args>` into the in-crate [`CommandRegistry`].
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md` Task 5.

use crate::expand::{expand_markdown_command, ExpandCtx};
use crate::model::{
    source_concat, CommandResult, CommandSource, NativeCommandType, NativePromptPreparation,
    PromptPart, SlashCommandKind,
};
use crate::parser::{
    is_js_whitespace_unit, parse_slash_command, tokenize_args_source, ParsedSlashCommand,
};
use crate::registry::CommandRegistry;
use crate::shell_expansion::{
    execute_shell_commands_in_prompt, GitWorktreeContext, ShellExpansionCtx,
    ShellExpansionProvider, ShellOut, ShellPermissionDecision, ShellPermissionGate, ShellRunError,
    ShellRunner,
};
use crate::ModCommandRunContext;
use async_trait::async_trait;
use hooks::events::{HookEvent, PromptExpansionType};
use hooks::registry::HookContext;
use hooks::HookExecutorImpl;
use lingxi_core::host::permission_gate::PermissionGate;
use lingxi_core::host::{CancellationToken, SlashCommandDispatcher, SlashDispatchResult};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::McpConnectionId;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

type Ujp = Utf16JsonProjection;

/// Build Native's user-facing command header for a prompt command.
fn native_prompt_command_header(command: &crate::model::SlashCommand, raw_args: &Ujp) -> Ujp {
    let mut header = vec![
        PromptPart::Text("<command-message>"),
        PromptPart::Source(&command.name),
        PromptPart::Text("</command-message>\n<command-name>/"),
        PromptPart::Source(&command.name),
        PromptPart::Text("</command-name>"),
    ];
    if raw_args
        .string_units("")
        .is_some_and(|units| !units.is_empty())
    {
        header.push(PromptPart::Text("\n<command-args>"));
        header.push(PromptPart::Source(raw_args));
        header.push(PromptPart::Text("</command-args>"));
    }
    source_concat(header)
}

/// Build Native's command-header/body pair for a non-directory prompt command.
fn native_prompt_turn_projections(
    command: &crate::model::SlashCommand,
    raw_args: &Ujp,
    expanded_body: &Ujp,
) -> (Ujp, Ujp) {
    (
        native_prompt_command_header(command, raw_args),
        expanded_body.clone(),
    )
}

/// Build Native's separate header/body user rows for a directory Skill.
fn native_skill_turn_projections(
    command: &crate::model::SlashCommand,
    raw_args: &Ujp,
    expanded_body: &Ujp,
) -> Result<(Ujp, Ujp), &'static str> {
    let skill_root = command
        .skill_root
        .as_deref()
        .ok_or("directory Skill has no base directory")?;
    let skill_root = skill_root
        .to_str()
        .ok_or("Skill base directory is not representable as UTF-8")?;
    let command_row = native_prompt_command_header(command, raw_args);
    let body_row = source_concat([
        PromptPart::Text("Base directory for this skill: "),
        PromptPart::Text(skill_root),
        PromptPart::Text("\n\n"),
        PromptPart::Source(expanded_body),
    ]);
    Ok((command_row, body_row))
}

fn user_prompt_expansion_control_result(
    aggregate: &hooks::response::AggregateHookResult,
    command_name: &Ujp,
    raw_args: &Ujp,
) -> Option<Ujp> {
    for (_, result) in &aggregate.all_results {
        let Some(response) = result.response.as_ref() else {
            continue;
        };
        if response.decision == Some(hooks::response::HookDecision::Block) {
            let mut text =
                hooks::ExactHookText::from_text("UserPromptExpansion operation blocked by hook:\n");
            let reason = response
                .reason
                .as_ref()
                .filter(|reason| !reason.is_empty())
                .cloned()
                .unwrap_or_else(|| hooks::ExactHookText::from_text("Blocked by hook"));
            text.push(&reason);
            if !response.suppress_original_prompt {
                text.push_text("\n\nOriginal prompt: /");
                text.push(
                    &hooks::ExactHookText::from_json_projection(command_name, "")
                        .expect("command name is a root string"),
                );
                let args = raw_args.string_units("").unwrap_or_default();
                if !args.is_empty() {
                    text.push_text(" ");
                    text.push(
                        &hooks::ExactHookText::from_json_projection(raw_args, "")
                            .expect("command arguments are a root string"),
                    );
                }
            }
            return Some(text.into_projection());
        }
        if response.prevent_continuation {
            let mut text = hooks::ExactHookText::from_text("Operation stopped by hook");
            if let Some(reason) = response.reason.as_ref().filter(|reason| !reason.is_empty()) {
                text.push_text(": ");
                text.push(reason);
            }
            return Some(text.into_projection());
        }
    }
    None
}

/// Selected model identity supplied by the owning session for bundled prompts.
pub type BundledPromptModelProvider = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<String>, String>> + Send>,
        > + Send
        + Sync,
>;

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
            Utf16JsonProjection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Utf16JsonProjection, String>> + Send>,
        > + Send
        + Sync,
>;

/// Resolves a live MCP prompt through the connection generation that
/// advertised it. Hosts without MCP leave this unwired.
pub type McpPromptResolver = Arc<
    dyn Fn(
            McpConnectionId,
            Utf16JsonProjection,
            Utf16JsonProjection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Utf16JsonProjection, String>> + Send>,
        > + Send
        + Sync,
>;

/// Observes a successfully dispatched prompt skill. The desktop composition
/// root uses this late-bound seam to arm plugin `on-skill-invoke` monitors;
/// hosts without plugin monitors leave it unwired.
pub type SkillInvocationObserver = Arc<
    dyn Fn(Utf16JsonProjection) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Host-owned Native `skill.prompt` transformation for a prompt command's
/// completed text. The first projection is the canonical skill/command name;
/// the second is the producer's text before Native's header/body envelope.
pub type SkillPromptTransformer = Arc<
    dyn Fn(
            Utf16JsonProjection,
            Utf16JsonProjection,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Utf16JsonProjection, Utf16JsonProjection>>
                    + Send
                    + 'static,
            >,
        > + Send
        + Sync,
>;

/// Host bridge that runs `command.run` middleware around a resolved ordinary
/// command. The supplied dispatcher exposes the core path so a hook's `next`
/// can execute rewritten arguments without recursively entering the same chain.
#[async_trait]
pub trait ModCommandRunInterceptor: Send + Sync {
    /// Dispatch a resolved command through Mods, preserving the core result's
    /// prompt and side effects when a hook calls `next`.
    async fn run(
        &self,
        dispatcher: &RegistrySlashDispatcher,
        command: &Utf16JsonProjection,
        args: &Utf16JsonProjection,
        context: ModCommandRunContext,
        cancellation: CancellationToken,
    ) -> SlashDispatchResult;
}

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
        _command: &Utf16JsonProjection,
        _shell: Option<crate::FrontmatterShell>,

        _cancellation: lingxi_core::host::CancellationToken,
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
        _command: &Utf16JsonProjection,
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
    /// Foreground owner supplied by a Side Chat session, when applicable.
    expansion_owner: Option<String>,
    mod_command_interceptor: Option<Arc<dyn ModCommandRunInterceptor>>,
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
    /// non-Native-Prompt `InjectMessage` arm delivers content verbatim. Native
    /// Prompt producer output is never scanned here; its producer owns any
    /// source-authored expansion. When wired via [`Self::with_shell_expansion`],
    /// each expansion builds a FRESH per-command [`ShellExpansionCtx`] (that command's
    /// `allowed_tools` injected on top of the base policy) before running
    /// [`execute_shell_commands_in_prompt`], 1:1 with claude-code's
    /// `executeShellCommandsInPrompt` at the COMMAND layer.
    shell_expansion: Option<Arc<dyn ShellExpansionProvider>>,
    /// Session-local throttler over the persistent skill usage map. `None`
    /// keeps embedders and tests side-effect free.
    skill_usage_session: Option<crate::skill_usage::SkillUsageSession>,
    /// Host-owned background launcher for bundled commands declaring
    /// `context: fork`. `None` fails closed instead of executing inline.
    background_prompt_launcher: Option<BackgroundPromptLauncher>,
    /// Live `prompts/get` bridge. Kept host-owned so `command-api` does not
    /// depend on the concrete MCP registry crate.
    mcp_prompt_resolver: Option<McpPromptResolver>,
    /// Best-effort observer for successful prompt-skill dispatches.
    skill_invocation_observer: Option<SkillInvocationObserver>,
    /// Mods `skill.prompt` hook. `None` preserves the producer text exactly.
    skill_prompt_transformer: Option<SkillPromptTransformer>,
    /// Whether builtin `InjectMessage` results become model turns. Thin clients
    /// enable this because they have no TUI loop to consume injected prompts;
    /// the default stays display-only for existing embedded/mobile callers.
    injected_messages_as_turns: bool,
    prompt_paths: Option<Arc<dyn Fn() -> (PathBuf, PathBuf) + Send + Sync>>,
    bundled_prompt_model: Option<BundledPromptModelProvider>,
    /// Permission gate that receives each input's frontmatter
    /// `disallowed-tools` (claude-code `Tbt(setToolPermissionContext, …)`).
    /// `None` — every existing caller — leaves the field inert, so a host that
    /// wires no gate behaves exactly as before.
    permission_gate: Option<Arc<dyn PermissionGate>>,
    /// Optional admission boundary for the Headless ordinary slash path.
    /// Interactive hosts keep the generic dispatcher behavior.
    dispatch_mode: SlashDispatchMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SlashDispatchMode {
    #[default]
    Generic,
    Headless,
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
    async fn command_input_denies(
        &self,
        raw: &Utf16JsonProjection,
    ) -> Vec<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        if !starts_with_raw_slash(raw) {
            return Vec::new();
        }
        let Some(parsed) = parse_slash_command(raw) else {
            return Vec::new();
        };
        let reg = self.registry.read().await;
        let Some((command, _)) = resolve_command_invocation(&reg, parsed) else {
            return Vec::new();
        };
        match &command.kind {
            SlashCommandKind::Markdown { frontmatter, .. }
            | SlashCommandKind::Plugin { frontmatter, .. }
            | SlashCommandKind::Bundled { frontmatter, .. } => frontmatter
                .disallowed_tools
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(lingxi_core::types::utf16_json::Utf16JsonProjection::from)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Construct a dispatcher backed by the given shared registry.
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>) -> Self {
        Self {
            registry,
            expansion_owner: None,
            mod_command_interceptor: None,
            expansion_hooks: None,
            shell_expansion: None,
            skill_usage_session: None,
            background_prompt_launcher: None,
            mcp_prompt_resolver: None,
            skill_invocation_observer: None,
            skill_prompt_transformer: None,
            injected_messages_as_turns: false,
            prompt_paths: None,
            bundled_prompt_model: None,
            permission_gate: None,
            dispatch_mode: SlashDispatchMode::Generic,
        }
    }

    /// Intercept ordinary slash commands at the command-run boundary.
    #[must_use]
    pub fn with_mod_command_interceptor(
        mut self,
        interceptor: Arc<dyn ModCommandRunInterceptor>,
    ) -> Self {
        self.mod_command_interceptor = Some(interceptor);
        self
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

    /// Supply the live selected model; an absent provider means absent model context.
    #[must_use]
    pub fn with_bundled_prompt_model(mut self, provider: BundledPromptModelProvider) -> Self {
        self.bundled_prompt_model = Some(provider);
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
    /// frontmatter `allowed_tools` injected), and eligible non-Native-Prompt
    /// builtin `InjectMessage` commands expand their embedded bodies with their
    /// own `allowed_tools`. A strict no-op when
    /// unset (the default): the stubs deny, and `InjectMessage` content is
    /// verbatim. MCP-sourced commands are NEVER expanded (claude-code
    /// `loadedFrom === 'mcp'` carve-out), even when wired.
    #[must_use]
    pub fn with_shell_expansion(mut self, provider: Arc<dyn ShellExpansionProvider>) -> Self {
        self.shell_expansion = Some(provider);
        self
    }

    /// Wire the session-scoped usage recorder shared with the Skill tool.
    #[must_use]
    pub fn with_skill_usage_session(
        mut self,
        session: crate::skill_usage::SkillUsageSession,
    ) -> Self {
        self.skill_usage_session = Some(session);
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

    /// Wire the host's `skill.prompt` Mods callback. It runs once after a
    /// Native prompt producer returns text and before the command header/body
    /// rows are built; hosts without Mods leave it unset.
    #[must_use]
    pub fn with_skill_prompt_transformer(mut self, transformer: SkillPromptTransformer) -> Self {
        self.skill_prompt_transformer = Some(transformer);
        self
    }

    async fn observe_skill_invocation(&self, name: &Utf16JsonProjection) {
        if let Some(observer) = self.skill_invocation_observer.as_ref() {
            observer(name.clone()).await;
        }
    }

    fn record_skill_usage(&self, name: &Utf16JsonProjection) {
        if let Some(session) = self.skill_usage_session.as_ref() {
            if let Err(error) = session.record(name) {
                tracing::warn!(command = %source_display(name), %error, "failed to record skill usage");
            }
        }
    }

    /// Wire the `UserPromptExpansion` hook (#39): the engine's hook executor
    /// plus a [`HookContext`] provider. After this, a prompt slash command fires
    /// the event before its prompt producer runs, faithful to claude-code
    /// `Ajs`→`nt`→`Qe` (2.1.295 source chunk `src_215528992.js`). A strict
    /// no-op when no `UserPromptExpansion` hook is registered (the executor
    /// itself no-ops on an empty match set).
    #[must_use]
    pub fn with_expansion_hooks(
        mut self,
        executor: Arc<HookExecutorImpl>,
        context: ExpansionHookContextProvider,
    ) -> Self {
        self.expansion_hooks = Some(ExpansionHooks { executor, context });
        self
    }

    /// Fire the `UserPromptExpansion` hook for a prompt slash command before
    /// its producer runs. The Native event carries the `/name args` line, and
    /// successful hook history rows stay deferred for the prompt turn. Strict
    /// no-op when no expansion hook is wired.
    async fn fire_user_prompt_expansion(
        &self,
        name: &Utf16JsonProjection,
        raw_args: &Utf16JsonProjection,
        source: CommandSource,
        defer_history_rows: bool,
    ) -> hooks::response::AggregateHookResult {
        let Some(eh) = self.expansion_hooks.as_ref() else {
            return hooks::response::AggregateHookResult::default();
        };
        // Keep the command name exact while adding generated slash/space text.
        let mut prompt_units = vec![b'/' as u16];
        prompt_units.extend(name.string_units("").unwrap_or_default());
        let raw_arg_units = raw_args.string_units("").unwrap_or_default();
        if !raw_arg_units.is_empty() {
            prompt_units.push(b' ' as u16);
            prompt_units.extend_from_slice(&raw_arg_units);
        }
        let prompt = projection_from_units(prompt_units);
        // claude-code `e.source==="mcp" ? "mcp_prompt" : "slash_command"`.
        let expansion_type = if matches!(source, CommandSource::Mcp) {
            PromptExpansionType::McpPrompt
        } else {
            PromptExpansionType::SlashCommand
        };
        let mut ctx = (eh.context)().await;
        ctx.defer_prompt_expansion_history_rows = defer_history_rows;
        eh.executor
            .execute(
                HookEvent::UserPromptExpansion {
                    expansion_type,
                    command_name: name.clone(),
                    command_args: raw_args.clone(),
                    command_source: Some(command_source_str(source).into()),
                    prompt,
                },
                ctx,
            )
            .await
    }

    /// Run Native's pre-producer hook gate for every command whose source
    /// metadata says `type: "prompt"`. Its caller must first release the
    /// registry read lock, then invoke this before building, resolving, or
    /// shell-expanding the prompt.
    async fn prepare_native_prompt_expansion(
        &self,
        command: &crate::model::SlashCommand,
        raw_args: &Utf16JsonProjection,
    ) -> Result<Option<hooks::response::AggregateHookResult>, Utf16JsonProjection> {
        if command.native_type != Some(NativeCommandType::Prompt) {
            return Ok(None);
        }

        let aggregate = self
            .fire_user_prompt_expansion(&command.name, raw_args, command.source, true)
            .await;
        if let Some(display) =
            user_prompt_expansion_control_result(&aggregate, &command.name, raw_args)
        {
            return Err(display);
        }
        Ok(Some(aggregate))
    }

    async fn transform_native_prompt_body(
        &self,
        command: &crate::model::SlashCommand,
        body: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
        if command.native_type != Some(NativeCommandType::Prompt) {
            return Ok(body);
        }
        match self.skill_prompt_transformer.as_ref() {
            Some(transformer) => transformer(command.name.clone(), body).await,
            None => Ok(body),
        }
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
            expansion_owner: self.expansion_owner.clone(),
            mod_command_interceptor: self.mod_command_interceptor.clone(),
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
            skill_usage_session: self.skill_usage_session.clone(),
            background_prompt_launcher: self.background_prompt_launcher.clone(),
            mcp_prompt_resolver: self.mcp_prompt_resolver.clone(),
            skill_invocation_observer: self.skill_invocation_observer.clone(),
            skill_prompt_transformer: self.skill_prompt_transformer.clone(),
            injected_messages_as_turns: self.injected_messages_as_turns,
            prompt_paths: self.prompt_paths.clone(),
            bundled_prompt_model: self.bundled_prompt_model.clone(),
            // The gate must travel too: a shared dispatcher that did not clear
            // the command denies would leave the previous skill's restrictions
            // standing for every input routed through the clone.
            permission_gate: self.permission_gate.clone(),
            dispatch_mode: self.dispatch_mode,
        }
    }

    fn injected_message_result(&self, content: Utf16JsonProjection) -> SlashDispatchResult {
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
        commands.sort_by(|a, b| a.name.string_units("").cmp(&b.name.string_units("")));
        commands
    }

    /// Return the live command catalog used by user-facing slash palettes and
    /// settings surfaces. Unlike [`Self::list_commands`], this keeps manual-only
    /// commands, merges dynamic aliases, and excludes only commands that users
    /// cannot invoke in the current environment.
    pub async fn list_palette_commands(&self) -> Vec<crate::model::SlashCommand> {
        let registry = self.registry.read().await;
        let mut commands = registry.palette_commands();
        commands.sort_by(|a, b| a.name.string_units("").cmp(&b.name.string_units("")));
        commands
    }

    /// Return the Native headless command catalog used by unknown-slash
    /// fallback reminders. Unlike palette listings, this preserves registry
    /// source order and separates the prompt-invocable names from the count of
    /// all currently available commands.
    pub async fn headless_command_catalog(&self) -> crate::model::HeadlessCommandCatalog {
        self.registry.read().await.headless_command_catalog()
    }

    /// Format the locked unknown-command literal.
    ///
    /// Public so callers can render the same string outside the dispatch loop
    /// (e.g. when reporting an error from a `/help` lookup that finds a
    /// dangling alias).
    #[must_use]
    pub fn unknown_command_literal(name: &Utf16JsonProjection) -> Utf16JsonProjection {
        crate::model::source_concat([
            crate::model::PromptPart::Text("Unknown command: /"),
            crate::model::PromptPart::Source(name),
        ])
    }
}

impl RegistrySlashDispatcher {
    /// Dispatch one ordinary Headless slash input through Native's currently
    /// admitted command pool. This boundary is intentionally separate from the
    /// generic interactive dispatcher and control-request path.
    pub async fn dispatch_headless(
        &self,
        raw: Utf16JsonProjection,
        cancellation: CancellationToken,
    ) -> SlashDispatchResult {
        let mut dispatcher = self.clone_shared();
        dispatcher.dispatch_mode = SlashDispatchMode::Headless;
        dispatcher.dispatch(raw, cancellation).await
    }

    /// Reject a resolved command outside the active dispatch mode before its
    /// permission restrictions, Mod middleware, hook, or handler can run.
    async fn headless_admission_rejection(
        &self,
        raw: &Utf16JsonProjection,
    ) -> Option<SlashDispatchResult> {
        if self.dispatch_mode != SlashDispatchMode::Headless || !starts_with_raw_slash(raw) {
            return None;
        }
        let parsed = parse_slash_command(raw)?;
        let registry = self.registry.read().await;
        let (command, parsed) = resolve_command_invocation(&registry, parsed)?;
        if registry.headless_dispatch_admitted(&command) {
            return None;
        }
        let name = parsed.name;
        let suggestion = registry.headless_unknown_command_suggestion(&name);
        Some(SlashDispatchResult::Unknown {
            display: Self::unknown_command_literal(&name),
            name,
            suggestion,
        })
    }

    /// Dispatch a child-session command with its lifecycle owner and
    /// cancellation propagated through the existing source-preserving path.
    pub async fn dispatch_with_cancel(
        &self,
        raw: Utf16JsonProjection,
        owner: String,
        cancellation: CancellationToken,
    ) -> SlashDispatchResult {
        let mut dispatcher = self.clone_shared();
        dispatcher.expansion_owner = Some(owner);
        dispatcher.dispatch(raw, cancellation).await
    }

    /// Execute an already-admitted slash command without firing Mod middleware.
    /// A `command.run` core callback uses this to prevent recursive dispatch.
    pub async fn dispatch_without_mod_hooks(
        &self,
        raw: &Utf16JsonProjection,
        cancellation: CancellationToken,
    ) -> SlashDispatchResult {
        // The top-level Native consumer checks `startsWith("/")` before it
        // calls `Iv`, even though `Iv` itself trims the full line. Preserve
        // that distinction so a leading-space slash remains an ordinary turn.
        if !starts_with_raw_slash(raw) {
            return SlashDispatchResult::NotASlashCommand;
        }

        // Parse into name + args via the Native-compatible source parser.
        let Some(parsed) = parse_slash_command(raw) else {
            return SlashDispatchResult::ParseFailure { input: raw.clone() };
        };

        // Look up the exact source name, including any unpaired UTF-16 units.
        let reg = self.registry.read().await;
        let unknown_name = parsed.name.clone();
        let Some((command, parsed)) = resolve_command_invocation(&reg, parsed) else {
            // Native only reports a valid, unregistered command as Unknown.
            // A source name outside its ASCII command-name grammar falls back
            // to a normal model turn using the untouched input projection.
            if !has_native_command_name_syntax(&unknown_name) {
                return SlashDispatchResult::RunAsTurn {
                    prompt: raw.clone(),
                };
            }
            let suggestion = reg.headless_unknown_command_suggestion(&unknown_name);
            return SlashDispatchResult::Unknown {
                display: Self::unknown_command_literal(&unknown_name),
                name: unknown_name,
                suggestion,
            };
        };

        if self.dispatch_mode == SlashDispatchMode::Headless
            && !reg.headless_dispatch_admitted(&command)
        {
            if let Some(gate) = self.permission_gate.as_ref() {
                gate.set_command_input_denies(&[], false);
            }
            let name = parsed.name.clone();
            let suggestion = reg.headless_unknown_command_suggestion(&name);
            return SlashDispatchResult::Unknown {
                display: Self::unknown_command_literal(&name),
                name,
                suggestion,
            };
        }

        // Side Chat admission runs before usage, hooks, prompt builders, or
        // handlers so rejected work cannot trigger an expansion side effect.
        if !reg.execution_allowed(&command) {
            return SlashDispatchResult::Unknown {
                name: parsed.name.clone(),
                display: source_concat([
                    PromptPart::Text("/"),
                    PromptPart::Source(&parsed.name),
                    PromptPart::Text(" is unavailable in Side Chat"),
                ]),
                suggestion: None,
            };
        }

        // Native `Pp` filters disabled command factories before dispatch.
        // Check the resolved canonical command so its aliases obey the same
        // `isEnabled` and environment gates.
        if !crate::registry::native_factory_enabled(&command) {
            return SlashDispatchResult::Unknown {
                name: parsed.name.clone(),
                display: Self::unknown_command_literal(&parsed.name),
                suggestion: None,
            };
        }

        // Native EVt records prompt commands after resolution/eligibility and
        // before expansion. The session handle applies its per-name 60-second
        // persistence throttle while retaining the canonical registry name.
        if command.is_prompt() && command.user_invocable != Some(false) {
            self.record_skill_usage(&command.name);
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
            let prompt_expansion = match self
                .prepare_native_prompt_expansion(&command, &parsed.raw_args)
                .await
            {
                Ok(aggregate) => aggregate,
                Err(display) => return SlashDispatchResult::Handled { display },
            };
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
                    let model = match self
                        .bundled_prompt_model
                        .as_ref()
                        .filter(|_| pf.needs_model_context())
                    {
                        Some(provider) => match provider().await {
                            Ok(model) => model,
                            Err(error) => {
                                return SlashDispatchResult::Handled {
                                    display: command_source_message(
                                        &command.name,
                                        &format!(" expansion failed: {error}"),
                                    ),
                                };
                            }
                        },
                        None => None,
                    };
                    let content = match pf.try_build_at(
                        &parsed.raw_args,
                        crate::BundledPromptContext {
                            project_root: &project_root,
                            cwd: &cwd,
                            is_preload: false,
                            main_loop_model: model.as_deref(),
                        },
                    ) {
                        Ok(content) => content,
                        Err(error) => {
                            return SlashDispatchResult::Handled {
                                display: command_source_message(
                                    &command.name,
                                    &format!(" expansion failed: {error}"),
                                ),
                            };
                        }
                    };
                    // Native `getPromptForCommand` expands source-authored
                    // `!`command`` blocks for bundled prompts too. The
                    // prompt's frontmatter supplies both the command allow
                    // list and shell selector; user/config text is escaped by
                    // its prompt factory before reaching this path.
                    let content = if contains_embedded_shell_commands(&content) {
                        match self.shell_expansion.as_ref() {
                            Some(provider) => {
                                let allowed_tools =
                                    frontmatter.allowed_tools.clone().unwrap_or_default();
                                let shell_ctx = provider.build_for_call(
                                    &allowed_tools,
                                    frontmatter.shell,
                                    self.expansion_owner.clone(),
                                    Some(cancellation.clone()),
                                );
                                match execute_shell_commands_in_prompt(
                                    &content,
                                    &shell_ctx,
                                    &command.name,
                                    frontmatter.shell,
                                    cancellation.clone(),
                                )
                                .await
                                {
                                    Ok(expanded) => expanded,
                                    Err(error) => {
                                        return SlashDispatchResult::Handled {
                                            display: command_source_message(
                                                &command.name,
                                                &format!(" expansion failed: {error}"),
                                            ),
                                        };
                                    }
                                }
                            }
                            // Preserve the established no-provider behavior
                            // for hosts that cannot execute embedded commands.
                            None => content,
                        }
                    } else {
                        content
                    };
                    let content = match self.transform_native_prompt_body(&command, content).await {
                        Ok(content) => content,
                        Err(display) => return SlashDispatchResult::Handled { display },
                    };
                    self.observe_skill_invocation(&command.name).await;
                    if prompt_expansion.is_none() {
                        // Preserve the prior extension behavior for bundled
                        // builders without Native prompt metadata.
                        self.fire_user_prompt_expansion(
                            &command.name,
                            &parsed.raw_args,
                            command.source,
                            false,
                        )
                        .await;
                    }
                    if frontmatter.context.as_deref() == Some("fork")
                        && frontmatter.background.unwrap_or(true)
                    {
                        return match self.background_prompt_launcher.as_ref() {
                            Some(launcher) => match launcher(content).await {
                                Ok(display) => SlashDispatchResult::Handled { display },
                                Err(error) => SlashDispatchResult::Handled {
                                    display: crate::model::source_concat([
                                        crate::model::PromptPart::Text("Could not start /"),
                                        crate::model::PromptPart::Source(&command.name),
                                        crate::model::PromptPart::Text(&format!(
                                            " in the background: {error}"
                                        )),
                                    ]),
                                },
                            },
                            None => SlashDispatchResult::Handled {
                                display: crate::model::source_concat([
                                    crate::model::PromptPart::Text("Could not start /"),
                                    crate::model::PromptPart::Source(&command.name),
                                    crate::model::PromptPart::Text(
                                        " in the background: no background launcher is wired",
                                    ),
                                ]),
                            },
                        };
                    }
                    if let Some(aggregate) = prompt_expansion {
                        let (command_row, body_row) =
                            native_prompt_turn_projections(&command, &parsed.raw_args, &content);
                        SlashDispatchResult::RunSkillAsTurn {
                            command: command_row,
                            body: body_row,
                            hook_messages: aggregate.prompt_expansion_history_rows,
                        }
                    } else {
                        // Keep the established route for local bundled builders
                        // that do not carry Native prompt metadata.
                        SlashDispatchResult::RunAsTurn {
                            prompt: content.into(),
                        }
                    }
                }
                None => SlashDispatchResult::Unknown {
                    name: parsed.name.clone(),
                    display: Self::unknown_command_literal(&parsed.name),
                    suggestion: None,
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
            let prompt_expansion = match self
                .prepare_native_prompt_expansion(&command, &parsed.raw_args)
                .await
            {
                Ok(aggregate) => aggregate,
                Err(display) => return SlashDispatchResult::Handled { display },
            };

            let positional = split_mcp_prompt_arguments(&parsed.raw_args);
            let wire_arguments = match bind_mcp_prompt_arguments(&arguments, &positional) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return SlashDispatchResult::Handled {
                        display: command_source_message(&command.name, &format!(": {error}")),
                    };
                }
            };
            let Some(resolver) = resolver else {
                return SlashDispatchResult::Handled {
                    display: command_source_message(
                        &command.name,
                        ": MCP prompt expansion is unavailable in this host",
                    ),
                };
            };

            if prompt_expansion.is_none() {
                // Preserve the established behavior for MCP prompt facades
                // that do not carry Native `type: "prompt"` metadata.
                self.fire_user_prompt_expansion(
                    &command.name,
                    &parsed.raw_args,
                    CommandSource::Mcp,
                    false,
                )
                .await;
            }
            return match resolver(connection_id, prompt_name, wire_arguments).await {
                Ok(result) => match render_mcp_prompt_result(&result) {
                    Ok(prompt) => match self.transform_native_prompt_body(&command, prompt).await {
                        Ok(prompt) => match prompt_expansion {
                            Some(aggregate) => {
                                let (command_row, body_row) = native_prompt_turn_projections(
                                    &command,
                                    &parsed.raw_args,
                                    &prompt,
                                );
                                SlashDispatchResult::RunSkillAsTurn {
                                    command: command_row,
                                    body: body_row,
                                    hook_messages: aggregate.prompt_expansion_history_rows,
                                }
                            }
                            None => SlashDispatchResult::RunAsTurn { prompt },
                        },
                        Err(display) => SlashDispatchResult::Handled { display },
                    },
                    Err(error) => SlashDispatchResult::Handled {
                        display: command_source_message(
                            &command.name,
                            &format!(": invalid MCP prompt result: {error}"),
                        ),
                    },
                },
                Err(error) => SlashDispatchResult::Handled {
                    display: command_source_message(
                        &command.name,
                        &format!(": MCP prompt expansion failed: {error}"),
                    ),
                },
            };
        }

        if matches!(
            command.kind,
            SlashCommandKind::Markdown { .. } | SlashCommandKind::Plugin { .. }
        ) {
            drop(reg);
            let native_prompt_expansion = match self
                .prepare_native_prompt_expansion(&command, &parsed.raw_args)
                .await
            {
                Ok(aggregate) => aggregate,
                Err(display) => return SlashDispatchResult::Handled { display },
            };
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
                Some(provider) if !matches!(command.source, CommandSource::Mcp) => provider
                    .build_for_call(
                        &allowed_tools,
                        fm_shell,
                        self.expansion_owner.clone(),
                        Some(cancellation.clone()),
                    ),
                _ => ShellExpansionCtx {
                    runner: Arc::new(UnavailableShellRunner),
                    permission_gate: Arc::new(DenyShellPermissionGate),
                },
            };
            let expand_ctx = ExpandCtx {
                session_id: "",
                shell: &shell_ctx,
                cancellation: cancellation.clone(),
            };
            let prompt_expansion = if let Some(aggregate) = native_prompt_expansion {
                Some(aggregate)
            } else if command.skill_root.is_some() {
                let aggregate = self
                    .fire_user_prompt_expansion(
                        &command.name,
                        &parsed.raw_args,
                        command.source,
                        true,
                    )
                    .await;
                if let Some(display) = user_prompt_expansion_control_result(
                    &aggregate,
                    &command.name,
                    &parsed.raw_args,
                ) {
                    return SlashDispatchResult::Handled { display };
                }
                Some(aggregate)
            } else {
                None
            };
            return match expand_markdown_command(&command, &parsed, &expand_ctx).await {
                Ok(content) => {
                    let content = match self
                        .transform_native_prompt_body(&command, content.into())
                        .await
                    {
                        Ok(content) => content,
                        Err(display) => return SlashDispatchResult::Handled { display },
                    };
                    self.observe_skill_invocation(&command.name).await;
                    if let Some(aggregate) = prompt_expansion {
                        let (command_row, body_row) = if command.skill_root.is_some() {
                            match native_skill_turn_projections(
                                &command,
                                &parsed.raw_args,
                                &content,
                            ) {
                                Ok(rows) => rows,
                                Err(error) => {
                                    return SlashDispatchResult::Handled {
                                        display: command_source_message(
                                            &command.name,
                                            &format!(" expansion failed: {error}"),
                                        ),
                                    };
                                }
                            }
                        } else {
                            native_prompt_turn_projections(&command, &parsed.raw_args, &content)
                        };
                        return SlashDispatchResult::RunSkillAsTurn {
                            command: command_row,
                            body: body_row,
                            hook_messages: aggregate.prompt_expansion_history_rows,
                        };
                    }
                    // Preserve the prior post-expansion hook behavior for
                    // locally mapped Markdown/Plugin prompts without Native
                    // prompt metadata.
                    let _ = self
                        .fire_user_prompt_expansion(
                            &command.name,
                            &parsed.raw_args,
                            command.source,
                            false,
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
                    display: command_source_message(
                        &command.name,
                        &format!(" expansion failed: {e}"),
                    ),
                },
            };
        }

        let Some(handler) = reg.get_handler(&parsed.name) else {
            return SlashDispatchResult::Unknown {
                name: parsed.name.clone(),
                display: Self::unknown_command_literal(&parsed.name),
                suggestion: None,
            };
        };

        // 5. Drop the registry lock before awaiting handler (handler may
        //    re-lock the registry or take a while).
        drop(reg);
        let prompt_expansion = match self
            .prepare_native_prompt_expansion(&command, &parsed.raw_args)
            .await
        {
            Ok(aggregate) => aggregate,
            Err(display) => return SlashDispatchResult::Handled { display },
        };

        let native_preparation = if command.native_type == Some(NativeCommandType::Prompt) {
            handler.native_prompt_preparation()
        } else {
            None
        };
        let mut native_shell_expansion = false;
        let mut outside_worktree_body = None;
        if let Some(NativePromptPreparation::GitWorktreeShell {
            outside_worktree_prefix,
            outside_worktree_suffix,
        }) = native_preparation.as_ref()
        {
            let Some(provider) = self.shell_expansion.as_ref() else {
                return SlashDispatchResult::Handled {
                    display: command_source_message(
                        &command.name,
                        ": Git worktree and shell preparation are unavailable in this host",
                    ),
                };
            };
            let GitWorktreeContext { cwd, inside } = provider.git_worktree_context().await;
            if inside {
                native_shell_expansion = true;
            } else {
                outside_worktree_body = Some(source_concat([
                    PromptPart::Source(outside_worktree_prefix),
                    PromptPart::Source(&cwd),
                    PromptPart::Source(outside_worktree_suffix),
                ]));
            }
        }

        let result = if let Some(content) = outside_worktree_body {
            CommandResult::InjectMessage { content }
        } else {
            handler
                .handle_with_cancel(&parsed, cancellation.clone())
                .await
        };

        match result {
            // M5-09 placeholders only ever return Done — but route the
            // other variants safely anyway so M5-10/M5-11 can extend.
            CommandResult::Done { display } | CommandResult::EmitEffects { display, .. } => {
                SlashDispatchResult::Handled {
                    display: display.unwrap_or_else(|| Utf16JsonProjection::from("")),
                }
            }
            CommandResult::InjectMessage { content } => {
                // Non-Native-Prompt local injectors may own raw shell blocks.
                // Native prompt output is scanned only when its source producer
                // explicitly declares that preparation. Other Native prompts
                // (including generated reports containing backticks) are never
                // inspected here.
                let content = if native_shell_expansion {
                    let provider = self
                        .shell_expansion
                        .as_ref()
                        .expect("Native shell preparation requires its provider");
                    let allowed: Vec<String> = handler
                        .allowed_tools()
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect();
                    let shell_ctx = provider.build_for_call(
                        &allowed,
                        None,
                        self.expansion_owner.clone(),
                        Some(cancellation.clone()),
                    );
                    match execute_shell_commands_in_prompt(
                        &content,
                        &shell_ctx,
                        &command.name,
                        None,
                        cancellation.clone(),
                    )
                    .await
                    {
                        Ok(expanded) => expanded,
                        Err(error) => {
                            let source_error = error.source_message();
                            return SlashDispatchResult::Handled {
                                display: source_concat([
                                    PromptPart::Source(&command.name),
                                    PromptPart::Text(" expansion failed: "),
                                    PromptPart::Source(&source_error),
                                ]),
                            };
                        }
                    }
                } else {
                    match self.shell_expansion.as_ref() {
                        Some(provider)
                            if prompt_expansion.is_none()
                                && !matches!(command.source, CommandSource::Mcp)
                                && contains_embedded_shell_commands(&content) =>
                        {
                            let allowed: Vec<String> = handler
                                .allowed_tools()
                                .iter()
                                .map(|s| (*s).to_string())
                                .collect();
                            let shell_ctx = provider.build_for_call(
                                &allowed,
                                None,
                                self.expansion_owner.clone(),
                                Some(cancellation.clone()),
                            );
                            match execute_shell_commands_in_prompt(
                                &content,
                                &shell_ctx,
                                &command.name,
                                None,
                                cancellation.clone(),
                            )
                            .await
                            {
                                Ok(expanded) => expanded,
                                Err(error) => {
                                    let source_error = error.source_message();
                                    return SlashDispatchResult::Handled {
                                        display: source_concat([
                                            PromptPart::Source(&command.name),
                                            PromptPart::Text(" expansion failed: "),
                                            PromptPart::Source(&source_error),
                                        ]),
                                    };
                                }
                            }
                        }
                        _ => content,
                    }
                };

                let content = match self.transform_native_prompt_body(&command, content).await {
                    Ok(content) => content,
                    Err(display) => return SlashDispatchResult::Handled { display },
                };
                if let Some(aggregate) = prompt_expansion {
                    let (command_row, body_row) =
                        native_prompt_turn_projections(&command, &parsed.raw_args, &content);
                    SlashDispatchResult::RunSkillAsTurn {
                        command: command_row,
                        body: body_row,
                        hook_messages: aggregate.prompt_expansion_history_rows,
                    }
                } else {
                    self.injected_message_result(content)
                }
            }
            CommandResult::Failed { source } => SlashDispatchResult::Failed {
                diagnostic: source.diagnostic_projection(),
            },
            CommandResult::RequestConfirmation { prompt, .. } => SlashDispatchResult::Handled {
                display: prompt.into(),
            },
        }
    }
}

#[async_trait]
impl SlashCommandDispatcher for RegistrySlashDispatcher {
    async fn dispatch(
        &self,
        raw: Utf16JsonProjection,
        cancellation: CancellationToken,
    ) -> SlashDispatchResult {
        if let Some(rejection) = self.headless_admission_rejection(&raw).await {
            // An excluded command becomes ordinary model input. It must not
            // inherit the previous skill's per-input denies or install the
            // excluded command's own frontmatter restrictions.
            if let Some(gate) = self.permission_gate.as_ref() {
                gate.set_command_input_denies(&[], false);
            }
            return rejection;
        }

        // Clear or replace this input's command-scoped tool restrictions even
        // when a Mod answers without calling the core command.
        if let Some(gate) = self.permission_gate.as_ref() {
            let denies = self.command_input_denies(&raw).await;
            gate.set_command_input_denies(&denies, false);
        }
        let intercept_parse = starts_with_raw_slash(&raw)
            .then(|| parse_slash_command(&raw))
            .flatten();
        if let (Some(interceptor), Some(parsed)) =
            (self.mod_command_interceptor.as_ref(), intercept_parse)
        {
            let resolved = {
                let registry = self.registry.read().await;
                resolve_command_invocation(&registry, parsed).and_then(|(command, parsed)| {
                    (crate::registry::native_factory_enabled(&command)
                        && registry.execution_allowed(&command)
                        && registry.mod_owner_of(&command.name).is_none())
                    .then_some((command.name.clone(), parsed.raw_args))
                })
            };
            if let Some((command, args)) = resolved {
                return interceptor
                    .run(
                        self,
                        &command,
                        &args,
                        crate::mod_catalog::current_command_context(),
                        cancellation.clone(),
                    )
                    .await;
            }
        }
        self.dispatch_without_mod_hooks(&raw, cancellation).await
    }
}

fn starts_with_raw_slash(input: &Utf16JsonProjection) -> bool {
    input
        .string_units("")
        .is_some_and(|units| units.first().copied() == Some(b'/' as u16))
}

fn contains_embedded_shell_commands(content: &Utf16JsonProjection) -> bool {
    content.string_units("").is_some_and(|units| {
        units
            .windows(4)
            .any(|window| window == [b'`' as u16, b'`' as u16, b'`' as u16, b'!' as u16])
            || units
                .windows(2)
                .any(|window| window == [b'!' as u16, b'`' as u16])
    })
}

fn has_native_command_name_syntax(name: &Utf16JsonProjection) -> bool {
    let Some(units) = name.string_units("") else {
        return false;
    };
    let Some((first, rest)) = units.split_first() else {
        return false;
    };
    let is_ascii_alphanumeric = |unit: u16| {
        (b'a' as u16..=b'z' as u16).contains(&unit)
            || (b'A' as u16..=b'Z' as u16).contains(&unit)
            || (b'0' as u16..=b'9' as u16).contains(&unit)
    };
    let is_allowed_rest = |unit: u16| {
        is_ascii_alphanumeric(unit)
            || unit == b'_' as u16
            || unit == b':' as u16
            || unit == b'-' as u16
    };
    (is_ascii_alphanumeric(*first) || *first == b'_' as u16)
        && rest.iter().copied().all(is_allowed_rest)
}

/// Resolve the Native direct-name/alias route first, then its fallback
/// `/<namespace> <command> ...` route. The returned parse retains source units
/// for the command name and the remaining raw argument tail.
fn resolve_command_invocation(
    registry: &CommandRegistry,
    parsed: ParsedSlashCommand,
) -> Option<(crate::model::SlashCommand, ParsedSlashCommand)> {
    if let Some(command) = registry.resolve(&parsed.name) {
        return Some((command.clone(), parsed));
    }

    let raw_args = parsed.raw_args.string_units("")?;
    let command_end = raw_args
        .iter()
        .position(|unit| is_js_whitespace_unit(*unit))
        .unwrap_or(raw_args.len());
    if command_end == 0 {
        return None;
    }

    let mut namespaced_units = parsed.name.string_units("")?.to_vec();
    namespaced_units.push(b':' as u16);
    namespaced_units.extend_from_slice(&raw_args[..command_end]);
    let name = projection_from_units(namespaced_units);
    let command = registry.resolve(&name)?.clone();

    let mut tail_start = command_end;
    while tail_start < raw_args.len() && is_js_whitespace_unit(raw_args[tail_start]) {
        tail_start += 1;
    }
    let remaining = raw_args[tail_start..].to_vec();
    let raw_args = projection_from_units(remaining.clone());
    let positional_args = tokenize_args_source(&remaining);
    Some((
        command,
        ParsedSlashCommand {
            name,
            raw_args,
            positional_args,
        },
    ))
}

fn split_mcp_prompt_arguments(raw_args: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
    let Some(units) = raw_args.string_units("") else {
        return Vec::new();
    };
    let mut values = Vec::new();
    let mut start = None;
    for (index, unit) in units.iter().copied().enumerate() {
        if is_js_whitespace_unit(unit) {
            if let Some(start) = start.take() {
                values.push(projection_from_units(units[start..index].to_vec()));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        values.push(projection_from_units(units[start..].to_vec()));
    }
    values
}

fn bind_mcp_prompt_arguments(
    declarations: &[lingxi_core::host::McpPromptArgumentDto],
    positional: &[Utf16JsonProjection],
) -> Result<Ujp, String> {
    let mut fields = Vec::with_capacity(declarations.len().min(positional.len()));
    for (index, declaration) in declarations.iter().enumerate() {
        match positional.get(index) {
            Some(value) => fields.push((
                declaration.name.string_units("").unwrap_or_default(),
                value.clone(),
            )),
            None if declaration.required => {
                return Err(format!(
                    "missing required argument <{}>",
                    source_display(&declaration.name)
                ));
            }
            None => {}
        }
    }
    Ujp::object_units(fields).map_err(|error| error.to_string())
}

fn render_mcp_prompt_result(result: &Ujp) -> Result<Ujp, String> {
    let messages = result
        .at_pointer("")
        .and_then(|root| root.get_property("messages"))
        .and_then(|messages| messages.array_len())
        .ok_or_else(|| "missing messages array".to_string())?;
    if messages == 0 {
        return Err("messages array is empty".to_string());
    }

    let mut rendered = Vec::with_capacity(messages);
    for index in 0..messages {
        let message = result
            .subprojection(&format!("/messages/{index}"))
            .map_err(|_| "message is missing content".to_string())?;
        let role = message
            .at_pointer("")
            .and_then(|message| message.get_property("role"))
            .and_then(|role| role.code_units().map(<[u16]>::to_vec))
            .unwrap_or_else(|| "user".encode_utf16().collect());
        let content = message
            .subprojection("/content")
            .map_err(|_| "message is missing content".to_string())?;
        let text = render_mcp_prompt_content(&content)?;
        rendered.push((role, text));
    }

    let user_role = "user".encode_utf16().collect::<Vec<_>>();
    if rendered.len() == 1 && rendered[0].0 == user_role {
        return Ok(rendered.pop().expect("one element").1);
    }

    let mut units = Vec::new();
    for (index, (role, content)) in rendered.into_iter().enumerate() {
        if index > 0 {
            units.extend("\n\n".encode_utf16());
        }
        units.push(b'[' as u16);
        units.extend(role);
        units.extend("]\n".encode_utf16());
        units.extend(content.string_units("").unwrap_or_default());
    }
    Ok(projection_from_units(units))
}

/// Render MCP prompt content without converting its source strings through a
/// recursive `serde_json::Value`. Array traversal is iterative and flat.
fn render_mcp_prompt_content(content: &Ujp) -> Result<Ujp, String> {
    enum Work {
        Projection(Ujp),
        Newline,
    }

    let mut pending = vec![Work::Projection(content.clone())];
    let mut output = Vec::new();
    while let Some(work) = pending.pop() {
        let Work::Projection(source) = work else {
            output.push(b'\n' as u16);
            continue;
        };
        let root = source
            .at_pointer("")
            .ok_or_else(|| "invalid prompt content".to_string())?;
        if let Some(units) = root.code_units() {
            output.extend_from_slice(units);
            continue;
        }
        if let Some(length) = root.array_len() {
            for index in (0..length).rev() {
                let child = source
                    .subprojection(&format!("/{index}"))
                    .map_err(|error| error.to_string())?;
                pending.push(Work::Projection(child));
                if index > 0 {
                    pending.push(Work::Newline);
                }
            }
            continue;
        }

        let kind = root.get_property("type").and_then(|value| value.as_str());
        if kind == Some("text") {
            let text = source
                .subprojection("/text")
                .map_err(|_| "text content is missing text".to_string())?;
            if text.string_units("").is_none() {
                return Err("text content is missing text".to_string());
            }
            pending.push(Work::Projection(text));
            continue;
        }
        if kind == Some("resource") {
            let resource = source
                .subprojection("/resource")
                .map_err(|_| "resource content is missing resource".to_string())?;
            let resource_view = resource
                .at_pointer("")
                .ok_or_else(|| "resource content is missing resource".to_string())?;
            if resource_view
                .get_property("text")
                .and_then(|text| text.code_units())
                .is_some()
            {
                pending.push(Work::Projection(
                    resource
                        .subprojection("/text")
                        .map_err(|_| "resource content is missing text".to_string())?,
                ));
            } else {
                output.extend(
                    Ujp::from(resource.to_json_string().map_err(|e| e.to_string())?)
                        .string_units("")
                        .unwrap_or_default(),
                );
            }
            continue;
        }

        output.extend(
            Ujp::from(source.to_json_string().map_err(|error| error.to_string())?)
                .string_units("")
                .unwrap_or_default(),
        );
    }
    Ok(projection_from_units(output))
}

fn projection_from_units(units: Vec<u16>) -> Ujp {
    let display = String::from_utf16_lossy(&units);
    Ujp::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

/// Convert to the safe display form only for UI/log/error output.
fn source_display(value: &Ujp) -> String {
    value.string_at("").unwrap_or_default().to_owned()
}

fn command_source_message(name: &Ujp, suffix: &str) -> Ujp {
    crate::model::source_concat([
        crate::model::PromptPart::Source(name),
        crate::model::PromptPart::Text(suffix),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_support::names::{core_description, BUILTIN_COMMAND_NAMES};
    use crate::builtin_support::unimplemented::UnimplementedCommandHandler;
    use crate::model::{
        BuiltinCommandHandler, CommandFrontmatter, CommandSource, SlashCommand, SlashCommandKind,
    };
    use crate::registry::CommandRegistry;
    use std::path::PathBuf;

    fn prompt_text(prompt: &Utf16JsonProjection) -> &str {
        prompt.string_at("").expect("test prompt is valid Unicode")
    }

    fn display_text(display: &Utf16JsonProjection) -> &str {
        display
            .string_at("")
            .expect("test display is a root string")
    }

    fn expected_native_prompt_header(name: &[u16], raw_args: &[u16]) -> Vec<u16> {
        let mut header = "<command-message>".encode_utf16().collect::<Vec<_>>();
        header.extend_from_slice(name);
        header.extend("</command-message>\n<command-name>/".encode_utf16());
        header.extend_from_slice(name);
        header.extend("</command-name>".encode_utf16());
        if !raw_args.is_empty() {
            header.extend("\n<command-args>".encode_utf16());
            header.extend_from_slice(raw_args);
            header.extend("</command-args>".encode_utf16());
        }
        header
    }

    #[test]
    fn user_prompt_expansion_control_is_consumed_before_body_expansion() {
        use hooks::response::{
            AggregateHookResult, ExactHookText, HookDecision, HookOutcome, HookResponse, HookResult,
        };

        let mut aggregate = AggregateHookResult::default();
        aggregate.all_results.push((
            lingxi_core::types::HookId::new(),
            HookResult {
                outcome: HookOutcome::Success,
                stdout: ExactHookText::default(),
                stderr: ExactHookText::default(),
                exit_code: None,
                response: Some(HookResponse {
                    decision: Some(HookDecision::Block),
                    ..HookResponse::default()
                }),
            },
        ));
        let display = user_prompt_expansion_control_result(
            &aggregate,
            &Utf16JsonProjection::from("skill"),
            &Utf16JsonProjection::from(""),
        )
        .expect("the first blocking UPE result is handled locally");
        assert_eq!(
            display.string_at(""),
            Some(
                "UserPromptExpansion operation blocked by hook:\nBlocked by hook\n\nOriginal prompt: /skill"
            )
        );

        aggregate.all_results[0].1.response = Some(HookResponse {
            prevent_continuation: true,
            reason: Some(ExactHookText::from_text("denied by policy")),
            ..HookResponse::default()
        });
        let stopped = user_prompt_expansion_control_result(
            &aggregate,
            &Utf16JsonProjection::from("skill"),
            &Utf16JsonProjection::from("secret args"),
        )
        .expect("preventContinuation also stops before expansion");
        assert_eq!(
            stopped.string_at(""),
            Some("Operation stopped by hook: denied by policy")
        );
    }

    struct LiteralBundledPrompt;

    impl crate::model::BundledPromptFn for LiteralBundledPrompt {
        fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
            "Native doctor prompt".into()
        }
    }

    #[tokio::test]
    async fn empty_slash_command_is_parse_failure_with_original_input() {
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(CommandRegistry::new())));

        for text in ["/", "/  \t", "/\u{2003}", "/ (MCP)"] {
            let raw = Ujp::from(text);
            assert_eq!(
                dispatcher
                    .dispatch(raw.clone(), lingxi_core::host::CancellationToken::new())
                    .await,
                SlashDispatchResult::ParseFailure { input: raw },
                "input must remain intact for the host's mode-specific fallback"
            );
        }
    }

    #[tokio::test]
    async fn leading_space_slash_stays_an_ordinary_input_at_dispatch_boundary() {
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(CommandRegistry::new())));
        assert_eq!(
            dispatcher
                .dispatch(
                    Ujp::from(" /not-a-command"),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::NotASlashCommand
        );
    }

    #[tokio::test]
    async fn unresolved_names_outside_native_grammar_fall_back_with_exact_source() {
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(CommandRegistry::new())));
        let units = vec![b'/' as u16, 0xd800, b' ' as u16, b'x' as u16];
        let raw = Ujp::root_string(String::from_utf16_lossy(&units), units).unwrap();
        assert_eq!(
            dispatcher
                .dispatch(raw.clone(), lingxi_core::host::CancellationToken::new())
                .await,
            SlashDispatchResult::RunAsTurn { prompt: raw }
        );
    }

    #[tokio::test]
    async fn valid_unregistered_name_remains_unknown() {
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(CommandRegistry::new())));
        assert_eq!(
            dispatcher
                .dispatch(
                    Ujp::from("/missing-command"),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::Unknown {
                name: Ujp::from("missing-command"),
                display: "Unknown command: /missing-command".into(),
                suggestion: None,
            }
        );
    }

    #[tokio::test]
    async fn dispatch_routes_exact_command_and_raw_prompt_utf16_units() {
        let name = Ujp::root_string(
            "custom�".into(),
            vec![
                b'c' as u16,
                b'u' as u16,
                b's' as u16,
                b't' as u16,
                b'o' as u16,
                b'm' as u16,
                0xd800,
            ],
        )
        .unwrap();
        let mut raw_units = vec![b'/' as u16];
        raw_units.extend(name.string_units("").unwrap());
        raw_units.push(b' ' as u16);
        raw_units.extend([b'x' as u16, 0xdfff]);
        let raw = Ujp::root_string(String::from_utf16_lossy(&raw_units), raw_units).unwrap();

        let mut registry = CommandRegistry::new();
        registry.register_command(crate::model::SlashCommand {
            name: name.clone(),
            description: "custom prompt".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/custom.md"),
                frontmatter: crate::model::CommandFrontmatter::default(),
                prompt_template: "$ARGUMENTS".into(),
            },
            ..crate::model::SlashCommand::default()
        });
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));

        let SlashDispatchResult::RunSkillAsTurn {
            command,
            body,
            hook_messages,
        } = dispatcher
            .dispatch_without_mod_hooks(&raw, lingxi_core::host::CancellationToken::new())
            .await
        else {
            panic!("exact source command should resolve and expand")
        };
        let name_units = name.string_units("").unwrap();
        let args_units = vec![b'x' as u16, 0xdfff];
        assert_eq!(
            command.string_units(""),
            Some(expected_native_prompt_header(&name_units, &args_units))
        );
        assert_eq!(body.string_units(""), Some(args_units));
        assert!(hook_messages.is_empty());
    }

    #[tokio::test]
    async fn namespaced_dispatch_keeps_exact_name_and_argument_units() {
        let name_units = [
            "team:review".encode_utf16().collect::<Vec<_>>(),
            vec![0xd800],
        ]
        .concat();
        let name =
            Ujp::root_string(String::from_utf16_lossy(&name_units), name_units.clone()).unwrap();
        let command = SlashCommand {
            name,
            description: "review".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/source-review.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "$ARGUMENTS".into(),
            },
            ..SlashCommand::default()
        };
        let mut registry = CommandRegistry::new();
        registry.register_command(command);
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));

        let raw_units = [
            vec![b'/' as u16],
            "team\u{2003}review".encode_utf16().collect(),
            vec![0xd800],
            " arg".encode_utf16().collect(),
            vec![0xdfff],
        ]
        .concat();
        let prefixed_units = [vec![0xfeff, b' ' as u16], raw_units.clone()].concat();
        let prefixed =
            Ujp::root_string(String::from_utf16_lossy(&prefixed_units), prefixed_units).unwrap();
        assert!(matches!(
            dispatcher
                .dispatch_without_mod_hooks(&prefixed, lingxi_core::host::CancellationToken::new())
                .await,
            SlashDispatchResult::NotASlashCommand
        ));
        let raw = Ujp::root_string(String::from_utf16_lossy(&raw_units), raw_units).unwrap();
        let SlashDispatchResult::RunSkillAsTurn {
            command,
            body,
            hook_messages,
        } = dispatcher
            .dispatch_without_mod_hooks(&raw, lingxi_core::host::CancellationToken::new())
            .await
        else {
            panic!("Native namespace fallback should resolve the exact command source")
        };
        let args_units = ["arg".encode_utf16().collect::<Vec<_>>(), vec![0xdfff]].concat();
        assert_eq!(
            command.string_units(""),
            Some(expected_native_prompt_header(&name_units, &args_units))
        );
        assert_eq!(body.string_units(""), Some(args_units));
        assert!(hook_messages.is_empty());
    }

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
        reg.register_alias("continue".into(), "resume".into());
        RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn stateless_bundled_prompt_does_not_require_a_model_route() {
        let mut registry = CommandRegistry::new();
        crate::builtins::register_bundled_skills(&mut registry, true);
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_bundled_prompt_model(Arc::new(|| {
                Box::pin(async { Err("model route is unavailable".into()) })
            }));
        assert!(
            matches!(dispatcher.dispatch("/dataviz revenue".into(), lingxi_core::host::CancellationToken::new()).await,
            SlashDispatchResult::RunSkillAsTurn { body, .. } if prompt_text(&body).contains("revenue"))
        );
        assert!(
            matches!(dispatcher.dispatch("/loop check".into(), lingxi_core::host::CancellationToken::new()).await,
            SlashDispatchResult::Handled { display } if display_text(&display) == "loop expansion failed: model route is unavailable")
        );
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn bundled_loop_reads_live_model_and_shared_clone_keeps_it() {
        let _guard = crate::builtins::bundled::loop_skill::LOOP_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let model = Arc::new(RwLock::new("claude-fable-5".to_string()));
        let mut registry = CommandRegistry::new();
        crate::builtins::register_bundled_skills(&mut registry, true);
        let provider_model = model.clone();
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_bundled_prompt_model(Arc::new(move || {
                let model = provider_model.clone();
                Box::pin(async move { Ok(Some(model.read().await.clone())) })
            }));
        let get_prompt = |result| match result {
            SlashDispatchResult::RunSkillAsTurn { body, .. } => body,
            other => panic!("expected model turn, got {other:?}"),
        };
        let first = get_prompt(
            dispatcher
                .dispatch(
                    "/loop check the deploy".into(),
                    lingxi_core::host::CancellationToken::new(),
                )
                .await,
        );
        assert!(prompt_text(&first).contains("3. **Briefly confirm**"));
        assert!(prompt_text(&first).contains("immediately BEFORE calling ScheduleWakeup"));
        *model.write().await = "claude-sonnet-5-5".into();
        let second = get_prompt(
            dispatcher
                .clone_shared()
                .dispatch(
                    "/loop check the deploy".into(),
                    lingxi_core::host::CancellationToken::new(),
                )
                .await,
        );
        assert!(prompt_text(&second).contains("3. **Decide whether the loop continues.**"));
        assert!(prompt_text(&second).contains("4. **After the wakeup is armed, briefly confirm**"));
    }

    #[tokio::test]
    async fn dispatches_known_command_to_locked_stub() {
        let d = seeded_dispatcher();
        let result = d
            .dispatch(
                "/teleport".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        match result {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(
                    display_text(&display),
                    "teleport: not implemented in v0.6.0 (M5)"
                );
            }
            other => panic!("expected Handled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatches_known_command_with_args_to_locked_stub() {
        let d = seeded_dispatcher();
        let result = d
            .dispatch(
                "/clear --force".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        match result {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(
                    display_text(&display),
                    "clear: not implemented in v0.6.0 (M5)"
                );
            }
            other => panic!("expected Handled, got {other:?}"),
        }
    }

    fn headless_test_prompt(name: &str, disable_non_interactive: bool) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: "headless test prompt".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            disable_non_interactive,
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from(format!("/tmp/{name}.md")),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "body".into(),
            },
            ..SlashCommand::default()
        }
    }

    struct OwnHeadlessHandler {
        name: &'static str,
        non_interactive: bool,
    }

    #[async_trait]
    impl crate::model::BuiltinCommandHandler for OwnHeadlessHandler {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "LingXi command fixture"
        }

        fn supports_non_interactive(&self) -> bool {
            self.non_interactive
        }

        async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
            CommandResult::Done {
                display: Some(self.name.into()),
            }
        }
    }

    #[tokio::test]
    async fn headless_own_commands_require_explicit_capability_without_native_catalog_rows() {
        let mut registry = CommandRegistry::new();
        for (name, non_interactive) in [("own-supported", true), ("own-interactive", false)] {
            registry.register_builtin_handler(Arc::new(OwnHeadlessHandler {
                name,
                non_interactive,
            }));
        }
        assert!(registry.headless_init_commands().is_empty());
        assert_eq!(
            registry.headless_command_catalog().available_command_count,
            0
        );
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        assert!(matches!(
            dispatcher.dispatch_headless("/own-supported".into(), CancellationToken::new()).await,
            SlashDispatchResult::Handled { display } if display.string_at("") == Some("own-supported")
        ));
        assert!(matches!(
            dispatcher
                .dispatch_headless("/own-interactive".into(), CancellationToken::new())
                .await,
            SlashDispatchResult::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn headless_dispatch_rejects_out_of_pool_before_side_effects() {
        let mut registry = CommandRegistry::new();
        let mut excluded = headless_test_prompt("manual-only", true);
        if let SlashCommandKind::Markdown { frontmatter, .. } = &mut excluded.kind {
            frontmatter.disallowed_tools = Some(vec!["Bash".into()]);
        }
        registry.register_command(excluded);
        let permission_gate = Arc::new(RecordingDenyGate::default());
        let middleware_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_permission_gate(permission_gate.clone())
            .with_mod_command_interceptor(Arc::new(CountingInterceptor {
                calls: middleware_calls.clone(),
                rewritten: "/manual-only".into(),
            }));

        match dispatcher
            .dispatch_headless(
                "/manual-only".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Unknown { name, display, .. } => {
                assert_eq!(name.string_at(""), Some("manual-only"));
                assert_eq!(display_text(&display), "Unknown command: /manual-only");
            }
            other => panic!("out-of-pool command must be unknown in Headless: {other:?}"),
        }
        assert_eq!(
            middleware_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            *permission_gate.calls.lock().unwrap(),
            vec![(Vec::new(), false)],
            "fallback clears previous skill restrictions without applying the excluded command's denies"
        );

        assert!(matches!(
            dispatcher
                .dispatch(
                    "/manual-only".into(),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::RunSkillAsTurn { .. }
        ));
        assert_eq!(
            middleware_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the generic dispatcher keeps its interactive behavior"
        );
    }

    struct CountingInterceptor {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        rewritten: Utf16JsonProjection,
    }

    #[async_trait]
    impl ModCommandRunInterceptor for CountingInterceptor {
        async fn run(
            &self,
            dispatcher: &RegistrySlashDispatcher,
            _command: &Utf16JsonProjection,
            _args: &Utf16JsonProjection,
            _context: ModCommandRunContext,
            cancellation: lingxi_core::host::CancellationToken,
        ) -> SlashDispatchResult {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            dispatcher
                .dispatch_without_mod_hooks(&self.rewritten, cancellation)
                .await
        }
    }

    #[tokio::test]
    async fn headless_admission_survives_mod_next_rewriting() {
        let mut registry = CommandRegistry::new();
        let mut allowed = headless_test_prompt("allowed", false);
        if let SlashCommandKind::Markdown { frontmatter, .. } = &mut allowed.kind {
            frontmatter.disallowed_tools = Some(vec!["Write".into()]);
        }
        registry.register_command(allowed);
        registry.register_command(headless_test_prompt("rewritten-manual", true));
        let middleware_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let permission_gate = Arc::new(RecordingDenyGate::default());
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_permission_gate(permission_gate.clone())
            .with_mod_command_interceptor(Arc::new(CountingInterceptor {
                calls: middleware_calls.clone(),
                rewritten: "/rewritten-manual".into(),
            }));

        match dispatcher
            .dispatch_headless(
                "/allowed".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Unknown { name, display, .. } => {
                assert_eq!(name.string_at(""), Some("rewritten-manual"));
                assert_eq!(display_text(&display), "Unknown command: /rewritten-manual");
            }
            other => panic!("Mod next must keep the Headless admission mode: {other:?}"),
        }
        assert_eq!(
            middleware_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            *permission_gate.calls.lock().unwrap(),
            vec![(vec!["Write".into()], false), (Vec::new(), false)],
            "Mod next falling back to ordinary input releases the original skill's denies"
        );
    }

    #[tokio::test]
    async fn dispatches_markdown_command_via_registry_resolve() {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "demo".into(),
            description: "Demo".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/demo.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "Use $ARGUMENTS".into(),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        // Markdown prompt commands run AS a turn (claude-code `type: "prompt"`).
        match d
            .dispatch(
                "/demo this".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>demo</command-message>\n<command-name>/demo</command-name>\n<command-args>this</command-args>"
                );
                assert_eq!(prompt_text(&body), "Use this");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected Native markdown prompt turn, got {other:?}"),
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
            fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
                let trimmed = crate::model::trim_source_text(args);
                if trimmed
                    .string_units("")
                    .is_some_and(|units| units.is_empty())
                {
                    "USAGE".into()
                } else {
                    crate::model::source_concat([
                        crate::model::PromptPart::Text("BUILT["),
                        crate::model::PromptPart::Source(&trimmed),
                        crate::model::PromptPart::Text("]"),
                    ])
                }
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "loop".into(),
            description: "Loop".into(),
            source: CommandSource::Bundled,
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter::default(),
                prompt_fn: Some(Arc::new(B)),
            },
            loaded_from: Some("bundled".to_string()),
            user_invocable: Some(true),
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        // Native prompt commands preserve a separate command header and meta
        // body in the model turn.
        match d
            .dispatch(
                "/loop 5m /babysit-prs".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>loop</command-message>\n<command-name>/loop</command-name>\n<command-args>5m /babysit-prs</command-args>"
                );
                assert_eq!(prompt_text(&body), "BUILT[5m /babysit-prs]");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected bundled skill to run-as-turn, got {other:?}"),
        }
        // Empty args still produce the same prompt turn, never Unknown.
        match d
            .dispatch("/loop".into(), lingxi_core::host::CancellationToken::new())
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>loop</command-message>\n<command-name>/loop</command-name>"
                );
                assert_eq!(prompt_text(&body), "USAGE");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected usage run-as-turn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_context_bundled_skill_uses_background_launcher_not_main_turn() {
        struct ReviewPrompt;
        impl crate::model::BundledPromptFn for ReviewPrompt {
            fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
                let trimmed = crate::model::trim_source_text(args);
                crate::model::source_concat([
                    crate::model::PromptPart::Text("REVIEW["),
                    crate::model::PromptPart::Source(&trimmed),
                    crate::model::PromptPart::Text("]"),
                ])
            }
        }

        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "code-review".into(),
            description: "Review".into(),
            source: CommandSource::Bundled,
            native_type: Some(NativeCommandType::Prompt),
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
                    Ok("started review agent".into())
                })
            }));

        assert_eq!(
            d.dispatch(
                "/code-review high --fix src".into(),
                lingxi_core::host::CancellationToken::new()
            )
            .await,
            SlashDispatchResult::Handled {
                display: "started review agent".into()
            }
        );
        assert_eq!(
            prompt_text(&launched.lock().await[0]),
            "REVIEW[high --fix src]"
        );
    }

    #[tokio::test]
    async fn fork_context_bundled_skill_fails_closed_without_launcher() {
        struct ReviewPrompt;
        impl crate::model::BundledPromptFn for ReviewPrompt {
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                "review".into()
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "code-review".into(),
            description: "Review".into(),
            source: CommandSource::Bundled,
            native_type: Some(NativeCommandType::Prompt),
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

        match d
            .dispatch(
                "/code-review".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert!(display_text(&display).contains("no background launcher is wired"));
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
            name: "commit".into(),
            description: "Custom commit".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/commit.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "Custom $ARGUMENTS".into(),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        match d
            .dispatch(
                "/commit now".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>commit</command-message>\n<command-name>/commit</command-name>\n<command-args>now</command-args>"
                );
                assert_eq!(prompt_text(&body), "Custom now");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected custom Native markdown turn, got {other:?}"),
        }
    }

    /// End-to-end guard: the `continue` alias survives the real dispatch path.
    /// `/continue` must route through the alias-aware `get_handler` to the
    /// `resume` handler — yielding `Handled`, not `Unknown`. (Mirrors
    /// claude-code's `aliases: ['continue']` on `/resume`.)
    #[tokio::test]
    async fn dispatches_continue_alias_to_resume() {
        let d = seeded_dispatcher_with_resume_alias();
        let result = d
            .dispatch(
                "/continue".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        match result {
            SlashDispatchResult::Handled { display } => {
                // Seeded with the stub handler under "resume", so the alias
                // routes there and we get the resume stub literal back.
                assert_eq!(
                    display_text(&display),
                    "resume: not implemented in v0.6.0 (M5)"
                );
            }
            other => panic!("expected Handled for /continue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_command_returns_locked_literal() {
        let d = seeded_dispatcher();
        let result = d
            .dispatch(
                "/notacommand".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        match result {
            SlashDispatchResult::Unknown {
                name,
                display,
                suggestion,
            } => {
                assert_eq!(name.string_at(""), Some("notacommand"));
                assert_eq!(display_text(&display), "Unknown command: /notacommand");
                assert!(suggestion.is_none());
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_command_carries_native_name_alias_and_tie_suggestion() {
        use lingxi_core::types::SettingsScope;
        use std::path::PathBuf;

        let prompt = |canonical: &str, display: &str, aliases: &[&str]| SlashCommand {
            name: canonical.into(),
            description: "prompt".into(),
            source: CommandSource::Settings(SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from(format!("/commands/{canonical}.md")),
                frontmatter: Default::default(),
                prompt_template: String::new().into(),
            },
            native_type: Some(crate::model::NativeCommandType::Prompt),
            user_facing_name: Some(display.into()),
            aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
            ..SlashCommand::default()
        };
        let mut registry = CommandRegistry::new();
        registry.register_command(prompt("canonical-first", "bat", &["comet"]));
        registry.register_command(prompt("canonical-second", "mat", &[]));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));

        for (input, expected) in [
            ("/cat", Some("bat")),
            ("/comex", Some("comet")),
            ("/zzzzzz", None),
        ] {
            match dispatcher
                .dispatch(input.into(), lingxi_core::host::CancellationToken::new())
                .await
            {
                SlashDispatchResult::Unknown { suggestion, .. } => {
                    assert_eq!(
                        suggestion
                            .and_then(|name| name.string_at("").map(str::to_owned))
                            .as_deref(),
                        expected,
                        "input {input}"
                    );
                }
                other => panic!("expected Unknown for {input}, got {other:?}"),
            }
        }
    }

    /// The Native `/doctor` bundled factory carries the `DISABLE_DOCTOR_COMMAND`
    /// gate. Its `/checkup` alias must be hidden by the same canonical factory
    /// predicate, while the enabled default resolves to the bundled prompt.
    #[tokio::test]
    async fn env_disabled_bundled_factory_does_not_resolve_through_alias() {
        let _g = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        let var = "DISABLE_DOCTOR_COMMAND";
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "doctor".into(),
            description: "Native doctor prompt".into(),
            source: CommandSource::Bundled,
            native_source: Some("built-in".into()),
            native_type: Some(crate::model::NativeCommandType::Prompt),
            aliases: vec!["checkup".into()],
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter::default(),
                prompt_fn: Some(Arc::new(LiteralBundledPrompt)),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));

        // Default: /doctor resolves to the native bundled prompt.
        std::env::remove_var(var);
        assert!(
            matches!(d.dispatch("/doctor".into(), lingxi_core::host::CancellationToken::new()).await,
            SlashDispatchResult::RunSkillAsTurn { body, .. } if prompt_text(&body) == "Native doctor prompt")
        );

        // The gate applies after alias resolution to the canonical factory.
        std::env::set_var(var, "1");
        match d
            .dispatch(
                "/checkup".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Unknown {
                name,
                display,
                suggestion,
            } => {
                assert_eq!(name.string_at(""), Some("checkup"));
                assert_eq!(display_text(&display), "Unknown command: /checkup");
                assert!(suggestion.is_none());
            }
            other => panic!("expected Unknown for gated /checkup, got {other:?}"),
        }
        std::env::remove_var(var);
    }

    #[tokio::test]
    async fn non_slash_input_is_not_a_slash_command() {
        let d = seeded_dispatcher();
        let result = d
            .dispatch(
                "hello world".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, SlashDispatchResult::NotASlashCommand));
    }

    #[tokio::test]
    async fn empty_input_is_not_a_slash_command() {
        let d = seeded_dispatcher();
        let result = d
            .dispatch("".into(), lingxi_core::host::CancellationToken::new())
            .await;
        assert!(matches!(result, SlashDispatchResult::NotASlashCommand));
    }

    #[tokio::test]
    async fn just_slash_is_a_parse_failure() {
        let d = seeded_dispatcher();
        let raw = Ujp::from("/");
        assert_eq!(
            d.dispatch(raw.clone(), lingxi_core::host::CancellationToken::new())
                .await,
            SlashDispatchResult::ParseFailure { input: raw }
        );
    }

    #[tokio::test]
    async fn uppercase_name_is_unknown() {
        let d = seeded_dispatcher();
        // Per Task 0 step 4 L3 expansion: the parser does NOT case-fold; the
        // registry only holds lowercase keys.
        let result = d
            .dispatch("/CLEAR".into(), lingxi_core::host::CancellationToken::new())
            .await;
        match result {
            SlashDispatchResult::Unknown {
                name,
                display,
                suggestion,
            } => {
                assert_eq!(name.string_at(""), Some("CLEAR"));
                assert_eq!(display_text(&display), "Unknown command: /CLEAR");
                assert!(suggestion.is_none());
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
            name: "deploy".into(),
            description: "Deploy".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: std::path::PathBuf::from("/x/deploy.md"),
                frontmatter: crate::model::CommandFrontmatter::default(),
                prompt_template: String::new().into(),
            },
            argument_names: vec!["env".into(), "region".into()],
            ..SlashCommand::default()
        });

        // The accessor's handle observes that same command + its argNames.
        let via_accessor = d.registry();
        let guard = via_accessor.read().await;
        let cmd = guard
            .resolve(&"deploy".into())
            .expect("command visible via registry()");
        assert_eq!(
            cmd.argument_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            vec!["env", "region"]
        );
    }

    #[tokio::test]
    async fn list_commands_excludes_model_hidden_entries() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "visible".into(),
            description: "Visible".into(),
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "hidden".into(),
            description: "Hidden".into(),
            disable_model_invocation: true,
            ..SlashCommand::default()
        });

        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_commands().await;

        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            vec!["visible"]
        );
    }

    #[tokio::test]
    async fn list_palette_commands_keeps_manual_only_and_aliases() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "review".into(),
            description: "Review".into(),
            disable_model_invocation: true,
            aliases: vec!["rv".into()],
            argument_hint: Some("[target]".into()),
            ..SlashCommand::default()
        });
        registry.register_alias("check".into(), "review".into());

        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_palette_commands().await;

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name.string_at(""), Some("review"));
        assert_eq!(
            commands[0]
                .aliases
                .iter()
                .map(|alias| alias.string_at("").unwrap())
                .collect::<Vec<_>>(),
            vec!["check", "rv"]
        );
        assert_eq!(
            commands[0]
                .argument_hint
                .as_ref()
                .and_then(|hint| hint.string_at("")),
            Some("[target]")
        );
    }

    #[tokio::test]
    async fn list_palette_commands_keeps_hidden_entries_for_exact_matching() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "auto-mode-setup".into(),
            description: "Hidden in bare palette".into(),
            ..SlashCommand::default()
        });

        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let commands = dispatcher.list_palette_commands().await;

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name.string_at(""), Some("auto-mode-setup"));
    }

    #[tokio::test]
    async fn list_palette_commands_excludes_non_user_and_env_disabled() {
        let _guard = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "hidden-skill".into(),
            description: "Hidden".into(),
            user_invocable: Some(false),
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "doctor".into(),
            description: "Doctor".into(),
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
    // Claude Code 2.1.295 fires `UserPromptExpansion` before every direct
    // `type:"prompt"` command's `getPromptForCommand` (`Ajs`→`nt`→`Qe`), with
    // command metadata: `expansion_type` (`slash_command` | `mcp_prompt`),
    // `command_name`, `command_args` (the raw arg string), `command_source`
    // (the command's provenance), and `prompt` (the `/name args` line). These
    // tests prove prompt producers are gated before their handler runs, and
    // that non-prompt builtins and unwired dispatchers remain no-ops.

    use async_trait::async_trait;
    use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
    use hooks::events::HookEventType;
    use hooks::executor::BuiltinHookHandler;
    use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
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
        command_name: Utf16JsonProjection,
        command_args: Utf16JsonProjection,
        command_source: Option<Utf16JsonProjection>,
        prompt: Utf16JsonProjection,
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
                stdout: String::new().into(),
                stderr: String::new().into(),
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
            on_failure: None,
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
            name: "demo".into(),
            description: "Demo".into(),
            source,
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/demo.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "Use $ARGUMENTS".into(),
            },
            ..SlashCommand::default()
        });
        Arc::new(RwLock::new(reg))
    }

    #[tokio::test]
    async fn prompt_dispatch_uses_session_throttle_and_persists_across_sessions() {
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
        .with_skill_usage_session(crate::skill_usage::SkillUsageSession::new(root.clone()));

        assert!(matches!(
            dispatcher
                .dispatch(
                    "/demo one".into(),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::RunSkillAsTurn { .. }
        ));
        assert!(matches!(
            dispatcher
                .dispatch(
                    "/demo two".into(),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::RunSkillAsTurn { .. }
        ));
        let usage = crate::skill_usage::read_skill_usage(&root).unwrap();
        let skill_key = "demo".encode_utf16().collect::<Vec<_>>();
        assert_eq!(usage.get(&skill_key).map(|entry| entry.count), Some(1));

        let next_session = RegistrySlashDispatcher::new(registry_with_demo_markdown(
            CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        ))
        .with_skill_usage_session(crate::skill_usage::SkillUsageSession::new(root.clone()));
        assert!(matches!(
            next_session
                .dispatch(
                    "/demo three".into(),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::RunSkillAsTurn { .. }
        ));
        let usage = crate::skill_usage::read_skill_usage(&root).unwrap();
        assert_eq!(usage.get(&skill_key).map(|entry| entry.count), Some(2));
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
        match d
            .dispatch(
                "/demo this and that".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>demo</command-message>\n<command-name>/demo</command-name>\n<command-args>this and that</command-args>"
                );
                assert_eq!(prompt_text(&body), "Use this and that");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected Native markdown prompt turn, got {other:?}"),
        }

        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![FiredExpansion {
                expansion_type: PromptExpansionType::SlashCommand,
                command_name: "demo".into(),
                command_args: "this and that".into(),
                command_source: Some("project".into()),
                // claude-code `o = `/${e.name} ${t}`` — the command LINE, not
                // the expanded body.
                prompt: "/demo this and that".into(),
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

        let _ = d
            .dispatch(
                "/demo arg".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;

        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "exactly one expansion: {seen:?}");
        assert_eq!(seen[0].expansion_type, PromptExpansionType::McpPrompt);
        assert_eq!(seen[0].command_source, Some("mcp".into()));
        assert_eq!(seen[0].prompt, "/demo arg".into());
    }

    fn registry_with_mcp_prompt(connection_id: McpConnectionId) -> Arc<RwLock<CommandRegistry>> {
        let mut registry = CommandRegistry::new();
        let command = crate::mcp_prompts::mcp_prompt_commands(&[(
            "github".to_string(),
            connection_id,
            lingxi_core::host::McpPromptDto {
                name: "review".into(),
                description: Some("Review a pull request".into()),
                arguments: vec![
                    lingxi_core::host::McpPromptArgumentDto {
                        name: "repository".into(),
                        description: None,
                        required: true,
                    },
                    lingxi_core::host::McpPromptArgumentDto {
                        name: "focus".into(),
                        description: None,
                        required: false,
                    },
                ],
            },
        )]);
        registry.register_command(command.into_iter().next().unwrap());
        Arc::new(RwLock::new(registry))
    }

    #[tokio::test]
    async fn mcp_prompt_dispatch_binds_arguments_and_runs_rendered_prompt() {
        let connection_id = McpConnectionId::new();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (expansion_executor, expansion_log) = recording_expansion_executor().await;
        let expansion_context: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let resolver_seen = seen.clone();
        let resolver: McpPromptResolver = Arc::new(move |id, name, arguments| {
            let seen = resolver_seen.clone();
            Box::pin(async move {
                *seen.lock().unwrap() = Some((id, name, arguments));
                Utf16JsonProjection::from_serde_value(&serde_json::json!({
                    "messages": [{
                        "role": "user",
                        "content": {"type": "text", "text": "Review src/lib.rs"}
                    }]
                }))
                .map_err(|error| error.to_string())
            })
        });
        let dispatcher = RegistrySlashDispatcher::new(registry_with_mcp_prompt(connection_id))
            .with_mcp_prompt_resolver(resolver)
            .with_expansion_hooks(expansion_executor, expansion_context);

        match dispatcher
            .dispatch(
                "/github review lingxi \"unsafe code\" ignored".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>mcp__github__review</command-message>\n<command-name>/mcp__github__review</command-name>\n<command-args>lingxi \"unsafe code\" ignored</command-args>"
                );
                assert_eq!(prompt_text(&body), "Review src/lib.rs");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected Native MCP prompt turn, got {other:?}"),
        }
        let call = seen.lock().unwrap().clone().expect("resolver call");
        assert_eq!(call.0, connection_id);
        assert_eq!(call.1, "review".into());
        assert_eq!(
            call.2,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({
                "repository": "lingxi",
                "focus": "\"unsafe"
            }))
            .unwrap()
        );

        assert!(matches!(
            dispatcher
                .dispatch(
                    "/github:review (MCP) lingxi focus ignored".into(),
                    lingxi_core::host::CancellationToken::new()
                )
                .await,
            SlashDispatchResult::RunSkillAsTurn { .. }
        ));
        let marker_call = seen.lock().unwrap().clone().expect("marker resolver call");
        assert_eq!(
            marker_call.2,
            Utf16JsonProjection::from_serde_value(&serde_json::json!({
                "repository": "lingxi",
                "focus": "focus"
            }))
            .unwrap()
        );
        let expansions = expansion_log.lock().unwrap().clone();
        assert_eq!(
            expansions.len(),
            2,
            "one Native hook run per MCP prompt: {expansions:?}"
        );
        assert!(expansions
            .iter()
            .all(|row| row.expansion_type == PromptExpansionType::McpPrompt));
    }

    #[tokio::test]
    async fn mcp_prompt_expansion_control_precedes_argument_binding() {
        let connection_id = McpConnectionId::new();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resolver_called = called.clone();
        let resolver: McpPromptResolver = Arc::new(move |_, _, _| {
            resolver_called.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { unreachable!("required argument must fail before resolver") })
        });
        let hook_registry = Arc::new(RwLock::new(HookRegistry::new()));
        hook_registry
            .write()
            .await
            .register(user_prompt_expansion_hook());
        let mut executor =
            HookExecutorImpl::new(hook_registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        executor.register_builtin(Arc::new(BlockingExpansionHandler));
        let context: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let dispatcher = RegistrySlashDispatcher::new(registry_with_mcp_prompt(connection_id))
            .with_mcp_prompt_resolver(resolver)
            .with_expansion_hooks(Arc::new(executor), context);

        match dispatcher
            .dispatch(
                "/github:review".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert!(display_text(&display).contains("blocked before prompt generation"));
            }
            other => panic!("expected UPE block before argument binding, got {other:?}"),
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

        let _ = d
            .dispatch("/demo".into(), lingxi_core::host::CancellationToken::new())
            .await;

        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "exactly one expansion: {seen:?}");
        assert_eq!(seen[0].command_args.string_at(""), Some(""));
        assert_eq!(
            seen[0]
                .command_source
                .as_ref()
                .and_then(|source| source.string_at("")),
            Some("user")
        );
        assert_eq!(seen[0].prompt.string_at(""), Some("/demo"));
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

        match d
            .dispatch(
                "/demo x".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn { body, .. } => {
                assert_eq!(prompt_text(&body), "Use x")
            }
            other => panic!("expected Native markdown prompt turn, got {other:?}"),
        }

        assert!(
            log.lock().unwrap().is_empty(),
            "an unwired dispatcher must not fire UserPromptExpansion"
        );
    }

    /// A local builtin command is not a Native prompt producer, so it does not
    /// fire `UserPromptExpansion` even when the hook is wired.
    #[tokio::test]
    async fn non_prompt_builtin_does_not_fire_user_prompt_expansion() {
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

        let _ = d
            .dispatch("/clear".into(), lingxi_core::host::CancellationToken::new())
            .await;

        assert!(
            log.lock().unwrap().is_empty(),
            "a builtin handler command must not fire UserPromptExpansion"
        );
    }

    struct CountedPromptHandler {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::model::BuiltinCommandHandler for CountedPromptHandler {
        async fn handle(&self, _args: &crate::parser::ParsedSlashCommand) -> CommandResult {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            CommandResult::InjectMessage {
                content: "generated prompt".into(),
            }
        }

        fn name(&self) -> &str {
            "prompt-producer"
        }

        fn description(&self) -> &str {
            "Prompt producer test"
        }

        fn native_type(&self) -> Option<NativeCommandType> {
            Some(NativeCommandType::Prompt)
        }
    }

    struct BlockingExpansionHandler;

    #[async_trait]
    impl BuiltinHookHandler for BlockingExpansionHandler {
        fn id(&self) -> &str {
            "record-user-prompt-expansion"
        }

        async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
            HookResult {
                outcome: HookOutcome::Success,
                stdout: String::new().into(),
                stderr: String::new().into(),
                exit_code: None,
                response: Some(HookResponse {
                    decision: Some(HookDecision::Block),
                    reason: Some("blocked before prompt generation".into()),
                    ..Default::default()
                }),
            }
        }
    }

    #[tokio::test]
    async fn prompt_expansion_block_precedes_prompt_handler_side_effects() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut commands = CommandRegistry::new();
        commands.register_builtin_handler(Arc::new(CountedPromptHandler {
            calls: calls.clone(),
        }));

        let hook_registry = Arc::new(RwLock::new(HookRegistry::new()));
        hook_registry
            .write()
            .await
            .register(user_prompt_expansion_hook());
        let mut executor =
            HookExecutorImpl::new(hook_registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        executor.register_builtin(Arc::new(BlockingExpansionHandler));
        let context: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(commands)))
            .with_expansion_hooks(Arc::new(executor), context);

        match dispatcher
            .dispatch(
                "/prompt-producer arg".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => assert!(
                display_text(&display).contains("blocked before prompt generation"),
                "hook block must be surfaced before the producer runs"
            ),
            other => panic!("expected a blocked prompt command, got {other:?}"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn skill_prompt_transformer_runs_before_native_prompt_envelope_and_survives_clone() {
        let transformed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let transformer_calls = transformed.clone();
        let transformer: SkillPromptTransformer = Arc::new(move |skill, text| {
            let transformer_calls = transformer_calls.clone();
            Box::pin(async move {
                transformer_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::model::source_concat([
                    crate::model::PromptPart::Source(&skill),
                    crate::model::PromptPart::Text(": "),
                    crate::model::PromptPart::Source(&text),
                ]))
            })
        });
        let mut commands = CommandRegistry::new();
        commands.register_builtin_handler(Arc::new(CountedPromptHandler {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(commands)))
            .with_skill_prompt_transformer(transformer);

        for dispatcher in [dispatcher.clone_shared(), dispatcher] {
            match dispatcher
                .dispatch(
                    "/prompt-producer arg".into(),
                    lingxi_core::host::CancellationToken::new(),
                )
                .await
            {
                SlashDispatchResult::RunSkillAsTurn {
                    command,
                    body,
                    hook_messages,
                } => {
                    assert_eq!(
                        prompt_text(&command),
                        "<command-message>prompt-producer</command-message>\n<command-name>/prompt-producer</command-name>\n<command-args>arg</command-args>"
                    );
                    assert_eq!(prompt_text(&body), "prompt-producer: generated prompt");
                    assert!(hook_messages.is_empty());
                }
                other => panic!("expected transformed Native prompt turn, got {other:?}"),
            }
        }
        assert_eq!(transformed.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn skill_prompt_transformer_error_stays_display_only() {
        let mut commands = CommandRegistry::new();
        commands.register_builtin_handler(Arc::new(CountedPromptHandler {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(commands)))
            .with_skill_prompt_transformer(Arc::new(|_, _| {
                Box::pin(async { Err(Utf16JsonProjection::from("skill.prompt failed")) })
            }));

        match dispatcher
            .dispatch(
                "/prompt-producer arg".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display_text(&display), "skill.prompt failed");
            }
            other => panic!("transformer errors must not create a model turn: {other:?}"),
        }
    }

    #[tokio::test]
    async fn prompt_expansion_block_precedes_bundled_prompt_builder() {
        struct CountedBuilder(Arc<std::sync::atomic::AtomicUsize>);
        impl crate::model::BundledPromptFn for CountedBuilder {
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "built prompt".into()
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "prompt-builder".into(),
            description: "Native prompt builder".into(),
            source: CommandSource::Bundled,
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter::default(),
                prompt_fn: Some(Arc::new(CountedBuilder(calls.clone()))),
            },
            ..SlashCommand::default()
        });

        let hook_registry = Arc::new(RwLock::new(HookRegistry::new()));
        hook_registry
            .write()
            .await
            .register(user_prompt_expansion_hook());
        let mut executor =
            HookExecutorImpl::new(hook_registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        executor.register_builtin(Arc::new(BlockingExpansionHandler));
        let context: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_expansion_hooks(Arc::new(executor), context);

        match dispatcher
            .dispatch(
                "/prompt-builder arg".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert!(display_text(&display).contains("blocked before prompt generation"))
            }
            other => panic!("expected UPE block before bundled builder, got {other:?}"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // ---- #3 embedded shell-expansion wiring ----
    //
    // A wired `ShellExpansionProvider` expands `!`cmd`` bodies in BOTH the
    // markdown/plugin arm AND the builtin `InjectMessage` arm, injecting that
    // command's `allowed_tools`; a permission-deny aborts the whole prompt; and
    // MCP-sourced commands are NEVER handed to the real provider (carve-out).
    // An UNWIRED dispatcher stays a strict no-op (covered by the markdown/
    // builtin tests above, which never wire `with_shell_expansion`).

    /// Echoes `OUT[<cmd>]` as stdout for any command and records exact inputs.
    struct EchoShellRunner {
        commands: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl ShellRunner for EchoShellRunner {
        async fn run(
            &self,
            command: &Utf16JsonProjection,
            _shell: Option<crate::FrontmatterShell>,

            _cancellation: lingxi_core::host::CancellationToken,
        ) -> Result<ShellOut, ShellRunError> {
            self.commands
                .lock()
                .unwrap()
                .push(command.string_at("").unwrap_or_default().to_owned());
            Ok(ShellOut {
                stdout: format!("OUT[{}]", command.string_at("").unwrap_or_default()),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }
    struct AllowGate;
    impl ShellPermissionGate for AllowGate {
        fn check(
            &self,
            _c: &Utf16JsonProjection,
            _s: Option<crate::FrontmatterShell>,
        ) -> ShellPermissionDecision {
            ShellPermissionDecision::Allow
        }
    }
    struct DenyGate {
        message: Utf16JsonProjection,
    }
    impl ShellPermissionGate for DenyGate {
        fn check(
            &self,
            _c: &Utf16JsonProjection,
            _s: Option<crate::FrontmatterShell>,
        ) -> ShellPermissionDecision {
            ShellPermissionDecision::Deny {
                message: Some(self.message.clone()),
            }
        }
    }
    /// Fake provider: records the `allowed_tools` it is built with and returns an
    /// echo runner behind an allow- or deny-gate.
    struct FakeProvider {
        deny: bool,
        seen_allowed: std::sync::Mutex<Vec<Vec<String>>>,
        inside_worktree: bool,
        cwd: Utf16JsonProjection,
        worktree_context_calls: std::sync::atomic::AtomicUsize,
        shell_commands: Arc<std::sync::Mutex<Vec<String>>>,
        deny_message: Utf16JsonProjection,
    }
    impl FakeProvider {
        fn new(deny: bool) -> Self {
            Self::with_worktree(deny, false, "/test/outside")
        }

        fn with_worktree(deny: bool, inside_worktree: bool, cwd: &str) -> Self {
            Self {
                deny,
                seen_allowed: std::sync::Mutex::new(Vec::new()),
                inside_worktree,
                cwd: cwd.into(),
                worktree_context_calls: std::sync::atomic::AtomicUsize::new(0),
                shell_commands: Arc::new(std::sync::Mutex::new(Vec::new())),
                deny_message: "not allowed".into(),
            }
        }

        fn with_deny_message(mut self, message: Utf16JsonProjection) -> Self {
            self.deny_message = message;
            self
        }
    }
    #[async_trait]
    impl ShellExpansionProvider for FakeProvider {
        async fn git_worktree_context(&self) -> GitWorktreeContext {
            self.worktree_context_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            GitWorktreeContext {
                cwd: self.cwd.clone(),
                inside: self.inside_worktree,
            }
        }

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
                Arc::new(DenyGate {
                    message: self.deny_message.clone(),
                })
            } else {
                Arc::new(AllowGate)
            };
            ShellExpansionCtx {
                runner: Arc::new(EchoShellRunner {
                    commands: self.shell_commands.clone(),
                }),
                permission_gate,
            }
        }
    }

    struct UnpreparedNativePromptHandler;
    #[async_trait]
    impl crate::model::BuiltinCommandHandler for UnpreparedNativePromptHandler {
        async fn handle(&self, _args: &crate::parser::ParsedSlashCommand) -> CommandResult {
            CommandResult::InjectMessage {
                content: "Final report mentions !`git status` as text.".into(),
            }
        }

        fn name(&self) -> &str {
            "insights"
        }

        fn description(&self) -> &str {
            "Native report prompt"
        }

        fn native_type(&self) -> Option<NativeCommandType> {
            Some(NativeCommandType::Prompt)
        }
    }

    #[cfg(feature = "builtin-handlers")]
    fn security_review_registry() -> CommandRegistry {
        let mut registry = CommandRegistry::new();
        registry.register_builtin_handler(Arc::new(crate::builtins::SecurityReviewHandler::new()));
        registry
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn security_review_outside_worktree_uses_native_message_and_skips_shell() {
        let provider = Arc::new(FakeProvider::with_worktree(
            false,
            false,
            "/tmp/security-review-outside",
        ));
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(security_review_registry())))
                .with_shell_expansion(provider.clone());

        match dispatcher.dispatch("/security-review".into(), lingxi_core::host::CancellationToken::new()).await {
            SlashDispatchResult::RunSkillAsTurn { body, .. } => assert_eq!(
                prompt_text(&body),
                "Tell the user: /security-review needs to run inside a git repository, but the current working directory (`/tmp/security-review-outside`) is not one.\n\nIf the repository is in a subdirectory, `cd` into it first and then re-run /security-review.\n\nIf this is a self-hosted runner session created without a `git_repository` source, either add one at session creation so the runner clones it and sets the working directory, or `cd` into the cloned repo before running the review."
            ),
            other => panic!("expected Native outside-worktree prompt, got {other:?}"),
        }

        assert_eq!(
            provider
                .worktree_context_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(provider.seen_allowed.lock().unwrap().is_empty());
        assert!(provider.shell_commands.lock().unwrap().is_empty());
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn security_review_worktree_expands_four_native_commands_through_policy() {
        let provider = Arc::new(FakeProvider::with_worktree(false, true, "/repo"));
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(security_review_registry())))
                .with_shell_expansion(provider.clone());

        match dispatcher
            .dispatch(
                "/security-review".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn { body, .. } => {
                let text = prompt_text(&body);
                for expected in [
                    "OUT[git status]",
                    "OUT[git diff --name-only origin/HEAD...]",
                    "OUT[git log --no-decorate origin/HEAD...]",
                    "OUT[git diff origin/HEAD...]",
                ] {
                    assert!(text.contains(expected), "missing {expected:?}");
                }
                assert!(!text.contains("!`git status`"));
            }
            other => panic!("expected expanded Native security-review turn, got {other:?}"),
        }

        assert_eq!(
            provider
                .worktree_context_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            provider.seen_allowed.lock().unwrap().as_slice(),
            &[crate::builtins::SecurityReviewHandler::new()
                .allowed_tools()
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>()],
            "Native allowed-tools must reach the per-command permission policy"
        );
        assert_eq!(
            provider.shell_commands.lock().unwrap().as_slice(),
            &[
                "git status",
                "git diff --name-only origin/HEAD...",
                "git log --no-decorate origin/HEAD...",
                "git diff origin/HEAD...",
            ]
        );
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn security_review_permission_denial_stops_before_running_shell() {
        let provider = Arc::new(FakeProvider::with_worktree(true, true, "/repo"));
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(security_review_registry())))
                .with_shell_expansion(provider.clone());

        match dispatcher
            .dispatch(
                "/security-review".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                let display = display_text(&display);
                assert!(display.starts_with("security-review expansion failed: "));
                assert!(display.contains("git status"));
                assert!(display.contains("not allowed"));
            }
            other => panic!("expected policy denial, got {other:?}"),
        }
        assert_eq!(
            provider.shell_commands.lock().unwrap().as_slice(),
            &[] as &[String],
            "permission denial must prevent process execution"
        );
        assert_eq!(provider.seen_allowed.lock().unwrap().len(), 1);
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn security_review_without_shell_provider_fails_closed_without_fake_cwd() {
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(security_review_registry())));

        match dispatcher
            .dispatch(
                "/security-review".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => assert_eq!(
                display_text(&display),
                "security-review: Git worktree and shell preparation are unavailable in this host"
            ),
            other => panic!("expected explicit host-capability failure, got {other:?}"),
        }
    }

    #[cfg(feature = "builtin-handlers")]
    #[tokio::test]
    async fn security_review_upe_block_precedes_worktree_and_shell_provider() {
        let provider = Arc::new(FakeProvider::with_worktree(true, true, "/repo"));
        let hook_registry = Arc::new(RwLock::new(HookRegistry::new()));
        hook_registry
            .write()
            .await
            .register(user_prompt_expansion_hook());
        let mut executor =
            HookExecutorImpl::new(hook_registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        executor.register_builtin(Arc::new(BlockingExpansionHandler));
        let context: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(security_review_registry())))
                .with_shell_expansion(provider.clone())
                .with_expansion_hooks(Arc::new(executor), context);

        match dispatcher
            .dispatch(
                "/security-review".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert!(display_text(&display).contains("blocked before prompt generation"))
            }
            other => panic!("expected UPE block before worktree check, got {other:?}"),
        }
        assert_eq!(
            provider
                .worktree_context_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(provider.seen_allowed.lock().unwrap().is_empty());
        assert!(provider.shell_commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unprepared_native_prompt_never_queries_worktree_or_scans_body() {
        let mut registry = CommandRegistry::new();
        registry.register_builtin_handler(Arc::new(UnpreparedNativePromptHandler));
        let provider = Arc::new(FakeProvider::with_worktree(true, true, "/repo"));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_shell_expansion(provider.clone());

        match dispatcher
            .dispatch(
                "/insights".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn { body, .. } => assert_eq!(
                prompt_text(&body),
                "Final report mentions !`git status` as text."
            ),
            other => panic!("expected untouched Native report turn, got {other:?}"),
        }
        assert_eq!(
            provider
                .worktree_context_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(provider.seen_allowed.lock().unwrap().is_empty());
        assert!(provider.shell_commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prompt_expansion_block_precedes_markdown_shell_provider() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "prompt-shell".into(),
            description: "Native prompt shell test".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/prompt-shell.md"),
                frontmatter: CommandFrontmatter {
                    allowed_tools: Some(vec!["Bash(echo:*)".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_template: "before !`echo hi` after".into(),
            },
            ..SlashCommand::default()
        });
        let provider = Arc::new(FakeProvider::new(false));
        let hook_registry = Arc::new(RwLock::new(HookRegistry::new()));
        hook_registry
            .write()
            .await
            .register(user_prompt_expansion_hook());
        let mut executor =
            HookExecutorImpl::new(hook_registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        executor.register_builtin(Arc::new(BlockingExpansionHandler));
        let context: ExpansionHookContextProvider =
            Arc::new(|| Box::pin(async { HookContext::default() }));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_shell_expansion(provider.clone())
            .with_expansion_hooks(Arc::new(executor), context);

        match dispatcher
            .dispatch(
                "/prompt-shell".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert!(display_text(&display).contains("blocked before prompt generation"))
            }
            other => panic!("expected UPE block before Markdown shell expansion, got {other:?}"),
        }
        assert!(provider.seen_allowed.lock().unwrap().is_empty());
    }

    /// A builtin handler returning `InjectMessage` with an embedded `!`cmd`` body
    /// and a declared allow-list (mirrors `/commit`'s shape).
    struct EmbeddedInjectHandler;
    #[async_trait]
    impl crate::model::BuiltinCommandHandler for EmbeddedInjectHandler {
        async fn handle(&self, _args: &crate::parser::ParsedSlashCommand) -> CommandResult {
            CommandResult::InjectMessage {
                content: "commit: !`git status` now".into(),
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

    struct EmbeddedBundledPrompt;

    impl crate::model::BundledPromptFn for EmbeddedBundledPrompt {
        fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
            "before !`echo hi` after".into()
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
            name: "demo".into(),
            description: "Demo".into(),
            source,
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/x/demo.md"),
                frontmatter: CommandFrontmatter {
                    allowed_tools: allowed,
                    ..CommandFrontmatter::default()
                },
                prompt_template: body.to_string().into(),
            },
            ..SlashCommand::default()
        });
        reg
    }

    /// A wired provider expands a markdown command's `!`echo hi`` to the runner's
    /// stdout, and the frontmatter `allowed_tools` reaches `provider.build`.
    #[tokio::test]
    async fn wired_provider_expands_markdown_embedded_shell() {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "demo".into(),
            description: "Demo".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/x/demo.md"),
                frontmatter: CommandFrontmatter {
                    allowed_tools: Some(vec!["Bash(echo:*)".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_template: "before !`echo hi` after".into(),
            },
            ..SlashCommand::default()
        });
        let provider = Arc::new(FakeProvider::new(false));
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_shell_expansion(provider.clone());
        match d
            .dispatch("/demo".into(), lingxi_core::host::CancellationToken::new())
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>demo</command-message>\n<command-name>/demo</command-name>"
                );
                assert_eq!(prompt_text(&body), "before OUT[echo hi] after");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected expanded Native prompt turn, got {other:?}"),
        }
        assert_eq!(
            provider.seen_allowed.lock().unwrap().as_slice(),
            &[vec!["Bash(echo:*)".to_string()]],
            "the frontmatter allow-list must reach the provider"
        );
    }

    #[tokio::test]
    async fn wired_provider_expands_bundled_prompt_with_frontmatter_policy() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "prompt-shell".into(),
            description: "Bundled shell test".into(),
            source: CommandSource::Bundled,
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter {
                    allowed_tools: Some(vec!["Bash(echo:*)".to_string()]),
                    shell: Some(crate::FrontmatterShell::Bash),
                    ..CommandFrontmatter::default()
                },
                prompt_fn: Some(Arc::new(EmbeddedBundledPrompt)),
            },
            ..SlashCommand::default()
        });
        let provider = Arc::new(FakeProvider::new(false));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)))
            .with_shell_expansion(provider.clone());

        match dispatcher
            .dispatch(
                "/prompt-shell".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>prompt-shell</command-message>\n<command-name>/prompt-shell</command-name>"
                );
                assert_eq!(prompt_text(&body), "before OUT[echo hi] after");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected expanded bundled Native prompt turn, got {other:?}"),
        }
        assert_eq!(
            provider.seen_allowed.lock().unwrap().as_slice(),
            &[vec!["Bash(echo:*)".to_string()]],
            "bundled frontmatter must supply the shell allow-list"
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
        let provider = Arc::new(FakeProvider::new(true));
        let d =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg))).with_shell_expansion(provider);
        match d.dispatch("/demo".into(), lingxi_core::host::CancellationToken::new()).await {
            SlashDispatchResult::Handled { display } => assert_eq!(
                display_text(&display),
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
        let provider = Arc::new(FakeProvider::new(false));
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_shell_expansion(provider.clone());
        match d
            .dispatch(
                "/commit".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display_text(&display), "commit: OUT[git status] now");
            }
            other => panic!("expected expanded InjectMessage, got {other:?}"),
        }
        assert_eq!(
            provider.seen_allowed.lock().unwrap().as_slice(),
            &[vec!["Bash(git status:*)".to_string()]],
            "the handler's allow-list must reach the provider"
        );
    }

    #[tokio::test]
    async fn wired_injectmessage_shell_error_preserves_utf16_source_units() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(EmbeddedInjectHandler));
        let reason_units: Vec<u16> = "permission denied: "
            .encode_utf16()
            .chain([0xd800])
            .collect();
        let reason = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&reason_units),
            reason_units.clone(),
        )
        .expect("error reason uses a root-string UTF-16 projection");
        let provider = Arc::new(FakeProvider::new(true).with_deny_message(reason));
        let dispatcher =
            RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg))).with_shell_expansion(provider);

        match dispatcher
            .dispatch(
                "/commit".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                let mut expected = concat!(
                    "commit expansion failed: ",
                    "Shell command permission check failed for pattern \"!`git status`\": "
                )
                .encode_utf16()
                .collect::<Vec<_>>();
                expected.extend(reason_units);
                assert_eq!(display.string_units(""), Some(expected));
            }
            other => panic!("expected expansion-failed display, got {other:?}"),
        }
    }

    /// An unwired builtin `InjectMessage` command delivers its RAW `!`cmd``
    /// template verbatim — strict no-op (no expansion), byte-identical to today.
    #[tokio::test]
    async fn unwired_builtin_injectmessage_is_verbatim() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(EmbeddedInjectHandler));
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        match d
            .dispatch(
                "/commit".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert_eq!(display_text(&display), "commit: !`git status` now");
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

        match dispatcher
            .dispatch(
                "/commit".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await
        {
            SlashDispatchResult::RunAsTurn { prompt } => {
                assert_eq!(prompt_text(&prompt), "commit: !`git status` now");
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
        let provider = Arc::new(FakeProvider::new(false));
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)))
            .with_shell_expansion(provider.clone());
        match d
            .dispatch("/demo".into(), lingxi_core::host::CancellationToken::new())
            .await
        {
            SlashDispatchResult::Handled { display } => {
                assert!(
                    display_text(&display).contains("expansion failed"),
                    "got {display:?}"
                );
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
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                panic!("unchecked expansion");
            }
            fn try_build_at(
                &self,
                args: &Utf16JsonProjection,
                context: crate::BundledPromptContext<'_>,
            ) -> std::io::Result<Utf16JsonProjection> {
                let root = context.project_root;
                let cwd = context.cwd;
                let preload = context.is_preload;
                assert_eq!(args.string_at(""), Some("task"));
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
        assert!(
            matches!(dispatcher.dispatch("/checked-loop task".into(), lingxi_core::host::CancellationToken::new()).await,
            SlashDispatchResult::Handled { display } if display_text(&display) == "checked-loop expansion failed: loop.md denied")
        );
    }
    // ---- MP-1: `disallowed-tools` reaches the permission gate --------------

    /// Records every `set_command_input_denies` call the dispatcher makes.
    #[derive(Default)]
    struct RecordingDenyGate {
        calls: std::sync::Mutex<
            Vec<(
                Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
                bool,
            )>,
        >,
    }
    #[async_trait]
    impl PermissionGate for RecordingDenyGate {
        async fn check(
            &self,
            _name: &str,
            _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            lingxi_core::host::permission_gate::PermissionDecision::Allow
        }
        fn set_command_input_denies(
            &self,
            specs: &[lingxi_core::types::utf16_json::Utf16JsonProjection],
            union: bool,
        ) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((specs.to_vec(), union));
        }
    }

    fn dispatcher_with_recording_gate() -> (RegistrySlashDispatcher, Arc<RecordingDenyGate>) {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "tight".into(),
            description: "A skill that narrows its own tools".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/tight.md"),
                frontmatter: CommandFrontmatter {
                    disallowed_tools: Some(vec!["Bash".to_string(), "Write".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_template: "body".into(),
            },
            ..SlashCommand::default()
        });
        reg.register_command(SlashCommand {
            name: "open".into(),
            description: "A skill that narrows nothing".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/open.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "body".into(),
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
        let _ = d
            .dispatch("/tight".into(), lingxi_core::host::CancellationToken::new())
            .await;
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![(vec!["Bash".into(), "Write".into()], false)],
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
        let _ = d
            .dispatch("/tight".into(), lingxi_core::host::CancellationToken::new())
            .await;
        let _ = d
            .dispatch(
                "just a normal prompt".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        let _ = d
            .dispatch("/open".into(), lingxi_core::host::CancellationToken::new())
            .await;
        let _ = d
            .dispatch(
                "/no-such-command".into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        let _ = d
            .dispatch("/".into(), lingxi_core::host::CancellationToken::new())
            .await;
        let calls = gate.calls.lock().unwrap().clone();
        let calls = calls
            .iter()
            .map(|(names, interactive)| {
                (
                    names
                        .iter()
                        .map(|name| name.string_at("").unwrap().to_owned())
                        .collect::<Vec<_>>(),
                    *interactive,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls,
            vec![
                (vec!["Bash".into(), "Write".into()], false),
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
            name: "tight".into(),
            description: "d".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from("/tmp/tight.md"),
                frontmatter: CommandFrontmatter {
                    disallowed_tools: Some(vec!["Bash".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_template: "body".into(),
            },
            ..SlashCommand::default()
        });
        let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
        match d
            .dispatch("/tight".into(), lingxi_core::host::CancellationToken::new())
            .await
        {
            SlashDispatchResult::RunSkillAsTurn {
                command,
                body,
                hook_messages,
            } => {
                assert_eq!(
                    prompt_text(&command),
                    "<command-message>tight</command-message>\n<command-name>/tight</command-name>"
                );
                assert_eq!(prompt_text(&body), "body");
                assert!(hook_messages.is_empty());
            }
            other => panic!("expected Native prompt turn without a gate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_shared_clone_keeps_publishing_the_denies() {
        // A clone that dropped the gate would leave the previous skill's denies
        // standing for every input routed through it.
        let (d, gate) = dispatcher_with_recording_gate();
        let shared = d.clone_shared();
        let _ = shared
            .dispatch("/tight".into(), lingxi_core::host::CancellationToken::new())
            .await;
        assert_eq!(
            gate.calls.lock().unwrap().len(),
            1,
            "clone_shared must carry the gate"
        );
    }
}
