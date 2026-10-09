//! Mobile capability profile of the shared Harness runtime.
//!
//! Android and iOS inject their platform capabilities into this existing
//! assembly. The mobile profile is usable by Rust hosts; UniFFI independently
//! enables foreign bindings without changing the selected runtime behavior.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 40 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 26 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

/// Rust-only host identity and separately compiled runtime identity.
pub use command_api::builtins::{runtime_build_info, BuildInfo};
use command_api::CommandRegistry;

use command_api::builtins::{
    register_all_builtin_commands, register_core_batch_1, register_core_batch_2,
    register_core_batch_4, register_core_batch_5,
};
use lingxi_core::host::{AuthHandle, OrchestratorHandle};
use skill_api::SkillRegistry;
use std::sync::Arc;
use tool_api::{BuiltinToolContext, ToolRegistry};
use tool_ui::ask_user_question::AskUserQuestionResolver;

// F3-03: the shared mobile session-host module — `MobileConfig` +
// `build_mobile(MobileConfig, Platform, listener, sink) -> MobileRuntime`. It
// The mobile profile owns runtime wiring and adapter sinks independently of
// foreign bindings. Both native packagers consume this same host implementation.
#[cfg(feature = "mobile")]
mod audio_service;
#[cfg(feature = "mobile")]
mod host;

// Audit fix (#14): the disk-backed Skill loader the FFI host wires so the mobile
// Skill tool resolves on-disk `.lingxi/commands` / `.lingxi/skills` under the
// app-private root. Its dependencies are selected by the mobile runtime profile.
#[cfg(feature = "mobile")]
mod skill_loader;

#[cfg(feature = "mobile")]
mod device_skills;
#[cfg(feature = "mobile")]
pub mod visualization_host;
// v3 Phase 1: workflow-on-mobile composition pieces (launcher + deferred
// invoker), consumed by the `host` build path.
#[cfg(feature = "mobile")]
mod transcript;
#[cfg(feature = "mobile")]
mod turn_durability;
#[cfg(feature = "mobile")]
mod workflow_support;

#[cfg(feature = "mobile")]
mod mcp_transport;
#[cfg(feature = "mobile")]
mod mobile_lsp;

#[cfg(feature = "mobile")]
pub use client::protocol::listings::{
    ModelBillingModeDto, ModelCapabilitiesDto, ModelDetailsDto, ModelPricingDto,
    ModelPricingTierDto, SessionModeDto,
};
#[cfg(feature = "mobile")]
pub use host::{
    build_mobile, build_mobile_engine, build_mobile_engine_inner, build_mobile_inner,
    parse_mobile_provider_config_json, CronDueOccurrenceDto, CronFireStatusDto, CronTaskDto,
    FiredCronJobDto, MobileBuildError, MobileConfig, MobileCronStoreHandle, MobileEngineError,
    MobileEngineHandle, MobileOAuthSessionDto, MobileOAuthStateDto, MobileRuntime,
    ProviderCatalogEntryDto, ProviderConnectionTestDto,
};
#[cfg(feature = "mobile")]
pub use session::jsonl::SessionMode as MobileSessionMode;

// F3-06: the host-only walking-skeleton support — a portable fake `Platform`
// shim (fs/http/clock stubs over a temp root), a recording `ClientEventListener`,
// a collecting `PermissionRequestSink`, and a streaming-injecting engine
// constructor. Lives behind the `uniffi` feature (it names the FFI-surface
// types) and is exposed so both the in-crate F3-03/F3-05 unit tests AND the
// `tests/skeleton_test.rs` integration test build the SAME off-device host. The
// real device `Platform` is `cfg(target_os)`-gated, so this shim is what proves
// the skeleton on CI — exactly the spec §8 "prove from a Swift/Kotlin unit test"
// smoke path, runnable on the host.
#[cfg(feature = "mobile")]
pub mod test_support;

// F3-04: re-export the FFI-visible adapter types both packager crates name when
// they call `build_mobile_engine` (the foreign `ClientEventListener` they
// register and the `PermissionRequestSink` the gate emits to). Re-exporting them
// from the shared host crate keeps the FFI crates free of a direct
// `client-adapter` import for these types — the shared host is the single seam.
#[cfg(feature = "mobile")]
pub use audio_service::{
    from_native_audio_service, max_audio_payload_bytes, AudioFfiError, NativeAudioService,
};
#[cfg(feature = "mobile")]
pub use client::adapter::{ClientEventListener, ListenerSink, PermissionRequestSink};

/// Mobile engine knobs.
#[derive(Clone, Debug)]
pub struct MobileEngineConfig {
    /// Model id the mobile build defaults to.
    pub default_model: String,
}

impl Default for MobileEngineConfig {
    fn default() -> Self {
        Self {
            // The host's boot default for the Anthropic route. Keep it
            // provider-qualified so the shared Claude model ids exposed by
            // Copilot cannot make the fresh-session default ambiguous.
            default_model: lingxi_core::host::qualified_model_ref(
                lingxi_core::host::provider_default_model("anthropic").unwrap_or("claude-sonnet-5"),
                Some("anthropic"),
            ),
        }
    }
}

/// Assemble the mobile builtin **tool** registry from a freshly-built
/// [`BuiltinToolContext`] (whose `camera`/`voice`/`share` handles come from the
/// mobile `Platform`).
#[must_use]
pub fn mobile_tool_registry(ctx: BuiltinToolContext) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_mobile_tools(&mut reg, ctx);
    reg
}

#[cfg(feature = "mobile")]
pub(crate) const MOBILE_CHAT_TOOL_ALLOWLIST: &[&str] = &[
    "camera",
    "voice",
    "speech",
    "notification",
    "clipboard",
    "share",
    "location",
    "device_status",
    "haptics",
    "open_url",
    "calendar",
    "contacts",
    "AskUserQuestion",
    "Glob",
    "Grep",
    "Read",
    "Skill",
    "StructuredOutput",
    "WebFetch",
    "WebSearch",
    // Chat may write files and run commands; every call still goes through
    // the permission prompt, and skills keep their shell-expansion ban.
    "Write",
    "Shell",
    "Visualization",
];

/// Chat tools that only ever run behind a per-call permission prompt. A
/// Chat-visible skill may not pre-authorize them through `allowed-tools`.
#[cfg(feature = "mobile")]
pub(crate) const MOBILE_CHAT_PROMPTED_TOOLS: &[&str] = &["Write", "Shell"];

#[cfg(feature = "mobile")]
pub(crate) fn apply_mobile_session_tool_policy(
    registry: &mut ToolRegistry,
    mode: session::jsonl::SessionMode,
) {
    if mode == session::jsonl::SessionMode::Chat {
        let allowlist = MOBILE_CHAT_TOOL_ALLOWLIST
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        registry.set_session_tool_allowlist(&allowlist);
    }
}

/// Register the mobile tool set into an existing registry, with the `Skill` tool
/// INERT (the hermetic `EmptySkillLoader`). Used by tests and the non-FFI host
/// build. The FFI host instead calls [`register_mobile_tools_with_skill_loader`]
/// to wire a disk-backed loader (audit fix #14).
pub fn register_mobile_tools(reg: &mut ToolRegistry, ctx: BuiltinToolContext) {
    register_mobile_non_skill_tools_with_ask_resolver(reg, ctx.clone(), None, None);
    // Skill tool with the hermetic `EmptySkillLoader` (no on-disk discovery).
    tool_skill::register_all(reg, ctx);
}

/// Audit fix (#14): register the mobile tool set with a FUNCTIONAL `Skill` tool
/// backed by `skill_loader` (the disk-backed `MobileDiskSkillLoader`) instead of
/// the inert `EmptySkillLoader`, so model-invoked skills resolve against the
/// device's on-disk `.lingxi/commands` / `.lingxi/skills`. uniffi-gated because
/// the loader impl needs `async-trait` (an FFI-only optional dep) and only the
/// FFI host wires a real loader.
#[cfg(feature = "mobile")]
pub fn register_mobile_tools_with_skill_loader(
    reg: &mut ToolRegistry,
    ctx: BuiltinToolContext,
    config_home: std::path::PathBuf,
    skill_loader: Arc<dyn tool_skill::skill::SkillLoader>,
) {
    register_mobile_non_skill_tools_with_ask_resolver(reg, ctx.clone(), Some(config_home), None);
    reg.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
        ctx,
        skill_loader,
    )));
}

/// Every mobile tool EXCEPT `Skill` (whose loader differs by build). Builtin
/// wire order is locale-sorted at enumeration time, so registration order is
/// immaterial. Hosts that surface a live questionnaire bridge inject a custom
/// `AskUserQuestion` resolver here so the registry never contains duplicate
/// same-name builtins.
fn register_mobile_non_skill_tools_with_ask_resolver(
    reg: &mut ToolRegistry,
    ctx: BuiltinToolContext,
    config_home: Option<std::path::PathBuf>,
    ask_resolver: Option<Arc<dyn AskUserQuestionResolver>>,
) -> (
    tool_cron::WakeupSchedulerCell,
    Arc<std::sync::atomic::AtomicBool>,
) {
    // ----- cross-platform subset (also linked by harness-runtime::desktop) -----------
    tool_file::register_all(reg, ctx.clone());
    if let Some(config_home) = config_home {
        tool_task::register_mobile_with_config_home(reg, ctx.clone(), config_home);
    } else {
        tool_task::register_mobile(reg, ctx.clone());
    }
    tool_web::register_all(reg, ctx.clone(), None);
    tool_plan::register_all(reg, ctx.clone());
    tool_meta::register_all(reg, ctx.clone());
    // Audit fix (#7): the cron tools (Create/List/Delete/RemoteTrigger) are
    // registered, but mobile starts NO `cron::CronScheduler` (the desktop root is
    // the only place one runs) and wires no `task_registry` for it to fire into —
    // a backgrounded app has no long-running daemon. So a created cron job is
    // saved/listed/deletable but does NOT auto-fire on this platform; CronCreate's
    // result text says so (see schedule_cron.rs `scheduler_active`). RemoteTrigger
    // is independent of the local scheduler (it triggers a cloud-side run).
    let wakeup_scheduler = tool_cron::register_all_with_auth(reg, ctx.clone(), None);
    // Mobile Linux carries plugin-provided language servers over its raw
    // stdio transport. The tool remains self-gated until a plugin server is
    // registered and the platform transport reports available.
    #[cfg(feature = "mobile")]
    tool_lsp::register_all(reg, ctx.clone());
    match ask_resolver {
        Some(resolver) => tool_ui::register_all_with_ask_resolver(reg, ctx.clone(), resolver),
        None => tool_ui::register_all(reg, ctx.clone()),
    }
    // ----- mobile-exclusive tools ------------------------------------------
    // camera / audio / notification / clipboard / share, folded into
    // the single `tool-mobile` crate.
    tool_mobile::register_all(reg, ctx.clone());
    // P3/P4: mobile shell tool. The composition root pre-gates it so a selected
    // but unavailable mobile-linux runtime never silently falls back to legacy.
    tool_shell_mobile::register_all(reg, ctx.clone());
    // P4: mobile structured git tool. Same pre-gate rule as shell.
    tool_git_mobile::register_all(reg, ctx);
    wakeup_scheduler
}

/// Audit fix (#14): the FFI sibling of [`mobile_tool_registry`] that wires a
/// disk-backed `Skill` loader (the mobile composition root passes the
/// `MobileDiskSkillLoader` it built from the device's app-private root).
#[cfg(feature = "mobile")]
#[must_use]
pub fn mobile_tool_registry_with_skill_loader(
    ctx: BuiltinToolContext,
    config_home: std::path::PathBuf,
    skill_loader: Arc<dyn tool_skill::skill::SkillLoader>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_mobile_tools_with_skill_loader(&mut reg, ctx, config_home, skill_loader);
    reg
}

#[cfg(feature = "mobile")]
#[must_use]
/// FFI host variant of [`mobile_tool_registry_with_skill_loader`] that installs
/// a live `AskUserQuestion` resolver exactly once, preserving builtin lookup
/// order while keeping the disk-backed `Skill` loader.
pub fn mobile_tool_registry_with_skill_loader_and_ask_resolver(
    ctx: BuiltinToolContext,
    config_home: std::path::PathBuf,
    skill_loader: Arc<dyn tool_skill::skill::SkillLoader>,
    ask_resolver: Arc<dyn AskUserQuestionResolver>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_mobile_non_skill_tools_with_ask_resolver(
        &mut reg,
        ctx.clone(),
        Some(config_home),
        Some(ask_resolver),
    );
    reg.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
        ctx,
        skill_loader,
    )));
    reg
}

/// Internal host builder retains the cell that ScheduleWakeup actually reads.
#[cfg(feature = "mobile")]
pub(crate) fn mobile_tool_registry_with_wakeup(
    ctx: BuiltinToolContext,
    config_home: std::path::PathBuf,
    skill_loader: Arc<dyn tool_skill::skill::SkillLoader>,
    ask_resolver: Option<Arc<dyn AskUserQuestionResolver>>,
) -> (
    ToolRegistry,
    tool_cron::WakeupSchedulerCell,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let mut reg = ToolRegistry::new();
    let (cell, armed) = register_mobile_non_skill_tools_with_ask_resolver(
        &mut reg,
        ctx.clone(),
        Some(config_home),
        ask_resolver,
    );
    reg.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
        ctx,
        skill_loader,
    )));
    (reg, cell, armed)
}

/// Register Android Computer Use only when both the Direct-build Cargo feature
/// and a live native host are present. Play builds pass no host and compile
/// without the feature. Direct foreground and headless engines intentionally
/// share the host; its in-memory active-session grants remain the security gate.
pub fn register_android_ui_automation(
    reg: &mut ToolRegistry,
    automation: Option<Arc<dyn lingxi_core::host::AndroidUiAutomation>>,
) {
    #[cfg(feature = "android-computer-use")]
    if let Some(automation) = automation {
        tool_android_use::register_all(reg, automation);
    }
    #[cfg(not(feature = "android-computer-use"))]
    let _ = (reg, automation);
}

#[cfg(test)]
mod android_ui_registration_tests {
    use super::register_android_ui_automation;
    use async_trait::async_trait;
    use lingxi_core::host::{
        AndroidAccessRequest, AndroidAction, AndroidActionResult, AndroidAppInfo,
        AndroidAutomationError, AndroidAutomationStatus, AndroidNodeQuery, AndroidScreenshot,
        AndroidUiAutomation, AndroidUiNode, AndroidUiSnapshot, AndroidWaitCondition,
    };
    use std::sync::Arc;

    struct StubAutomation;

    fn unsupported<T>() -> Result<T, AndroidAutomationError> {
        Err(AndroidAutomationError::Unsupported("test stub".into()))
    }

    #[async_trait]
    impl AndroidUiAutomation for StubAutomation {
        async fn status(&self) -> Result<AndroidAutomationStatus, AndroidAutomationError> {
            unsupported()
        }

        async fn request_access(
            &self,
            _request: AndroidAccessRequest,
        ) -> Result<Vec<AndroidAppInfo>, AndroidAutomationError> {
            unsupported()
        }

        async fn list_granted_apps(&self) -> Result<Vec<AndroidAppInfo>, AndroidAutomationError> {
            unsupported()
        }

        async fn screenshot(&self) -> Result<AndroidScreenshot, AndroidAutomationError> {
            unsupported()
        }

        async fn ui_tree(&self) -> Result<AndroidUiSnapshot, AndroidAutomationError> {
            unsupported()
        }

        async fn find_nodes(
            &self,
            _query: AndroidNodeQuery,
        ) -> Result<Vec<AndroidUiNode>, AndroidAutomationError> {
            unsupported()
        }

        async fn inspect_node(
            &self,
            _node_id: String,
        ) -> Result<AndroidUiNode, AndroidAutomationError> {
            unsupported()
        }

        async fn perform(
            &self,
            _action: AndroidAction,
        ) -> Result<AndroidActionResult, AndroidAutomationError> {
            unsupported()
        }

        async fn wait_for(
            &self,
            _condition: AndroidWaitCondition,
            _timeout_ms: u64,
        ) -> Result<AndroidActionResult, AndroidAutomationError> {
            unsupported()
        }

        async fn stop(&self) -> Result<(), AndroidAutomationError> {
            Ok(())
        }
    }

    #[test]
    fn registration_follows_the_direct_feature_gate() {
        let mut registry = tool_api::ToolRegistry::new();
        register_android_ui_automation(&mut registry, Some(Arc::new(StubAutomation)));
        #[cfg(feature = "android-computer-use")]
        assert!(registry.find_by_name("android_use").is_some());
        #[cfg(not(feature = "android-computer-use"))]
        assert!(registry.find_by_name("android_use").is_none());
    }
}

/// Assemble the mobile builtin **skill** registry.
///
/// Plugin skills are not bundled here: the plugin registry is their only source.
#[must_use]
pub fn mobile_skill_registry() -> SkillRegistry {
    let mut reg = SkillRegistry::new();
    skill_api::register_mobile(&mut reg);
    reg
}

/// Assemble the mobile slash-command registry: the core handlers plus the
/// mobile-only handlers (`/mobile` `/voice` `/share` `/camera`).
#[must_use]
pub fn mobile_command_registry(
    handle: Arc<dyn OrchestratorHandle>,
    auth: Arc<dyn AuthHandle>,
) -> CommandRegistry {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    register_mobile_bundled_prompt_commands(&mut reg);
    register_core_batch_1(&mut reg, handle.clone());
    register_core_batch_2(&mut reg, handle.clone(), auth);
    register_core_batch_4(&mut reg, handle.clone());
    register_core_batch_5(&mut reg, handle);
    // Batch 8 (`/fork`, `/goal`, `/recap`, `/reload-skills`, `/skill-doctor`,
    // `/stop`) is wired by the uniffi composition root (`host::build_mobile`)
    // right after this returns, because it needs the shared
    // `Arc<tokio::sync::RwLock<CommandRegistry>>` slot (tokio is a
    // `uniffi`-gated optional dependency here, unavailable in this default-lean
    // lib build).
    //
    // Mobile-only command handlers: currently none — the mobile command names
    // (/mobile, /voice, /share, /camera) are served as command-core
    // unimplemented stubs. Register real mobile handlers on `reg` directly here
    // when implemented.
    reg
}

/// Register the mobile authoritative bundled prompt catalog (for example
/// `/loop`). Plugin skills are file-backed Plugin commands and are added by
/// `PluginManager` after this base catalog is installed.
pub(crate) fn register_mobile_bundled_prompt_commands(reg: &mut CommandRegistry) {
    // Canonical custom precommit skills must keep their origin/body across the
    // boot/reload bundled reseed; Bash only suggests the custom implementations.
    let custom_precommit: Vec<_> = reg
        .list_all()
        .into_iter()
        .filter(|command| {
            matches!(command.name.as_str(), "verify" | "simplify")
                && matches!(
                    command.loaded_from.as_deref(),
                    Some("skills" | "commands_DEPRECATED")
                )
        })
        .cloned()
        .collect();
    // Bundled programmatic skills (`/loop`), mirroring desktop. Gated on the cron
    // kill-switch (loop.ts:83); mobile starts no cron scheduler so a scheduled
    // job is inert, but the skill's listing/usage path is harmless and faithful.
    let cron_enabled = tool_cron::cron_tools_enabled();
    command_api::builtins::register_bundled_skills(reg, cron_enabled);
    for command in custom_precommit {
        reg.register_command(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    use mobile_linux_api::ProcessOutput;

    use std::collections::HashMap;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};
    use tool_api::tool_trait::{ToolError, ToolStaticContext};

    struct NoopSkillLoader;

    #[async_trait]
    impl tool_skill::skill::SkillLoader for NoopSkillLoader {
        async fn load(
            &self,
            _name: &str,
        ) -> Result<Option<tool_skill::skill::SkillDescriptor>, ToolError> {
            Ok(None)
        }
    }

    struct FirstAnswerResolver;

    #[async_trait]
    impl AskUserQuestionResolver for FirstAnswerResolver {
        async fn resolve(
            &self,
            questions: &[tool_ui::ask_user_question::Question],
            _non_interactive: bool,
        ) -> Result<HashMap<String, String>, ToolError> {
            Ok(questions
                .iter()
                .filter_map(|question| {
                    question
                        .options
                        .first()
                        .map(|option| (question.question.clone(), option.label.clone()))
                })
                .collect())
        }
    }

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    #[cfg(feature = "mobile")]
    #[test]
    fn chat_profile_exposes_read_tools_and_available_native_device_tools() {
        let ctx = shell_test_ctx(dummy_out());
        let code_registry = mobile_tool_registry(ctx.clone());
        assert!(code_registry.find_by_name("Write").is_some());
        assert!(code_registry.find_by_name("TaskCreate").is_some());

        let mut chat_registry = mobile_tool_registry(ctx);
        apply_mobile_session_tool_policy(&mut chat_registry, session::jsonl::SessionMode::Chat);
        let names = chat_registry
            .available_tools(&ToolStaticContext::default())
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("Read"));
        assert!(names.contains("WebFetch"));
        assert!(names.contains("AskUserQuestion"));
        assert!(names
            .iter()
            .all(|name| MOBILE_CHAT_TOOL_ALLOWLIST.contains(&name.as_str())));
        for denied in [
            "Write",
            "Edit",
            "NotebookEdit",
            "Shell",
            "Git",
            "LSP",
            "Workflow",
            "TaskCreate",
            "CronCreate",
            "ToolSearch",
        ] {
            assert!(
                chat_registry.find_by_name(denied).is_none(),
                "{denied} must not be reachable in Chat mode"
            );
        }
    }

    #[cfg(feature = "mobile")]
    #[tokio::test]
    async fn custom_mobile_ask_resolver_does_not_duplicate_builtin() {
        let registry = mobile_tool_registry_with_skill_loader_and_ask_resolver(
            shell_test_ctx(dummy_out()),
            std::env::temp_dir(),
            Arc::new(NoopSkillLoader),
            Arc::new(FirstAnswerResolver),
        );
        let tools = registry.available_tools(&ToolStaticContext::default());
        let ask_count = tools
            .iter()
            .filter(|tool| tool.name() == tool_ui::ask_user_question::ASK_USER_QUESTION_TOOL_NAME)
            .count();
        assert_eq!(ask_count, 1);
        let ask = registry
            .find_by_name(tool_ui::ask_user_question::ASK_USER_QUESTION_TOOL_NAME)
            .expect("AskUserQuestion registered");
        let result = ask
            .call(
                serde_json::json!({
                    "questions": [{
                        "question": "Pick one?",
                        "header": "Choice",
                        "options": [
                            { "label": "Alpha", "description": "first" },
                            { "label": "Beta", "description": "second" }
                        ]
                    }]
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("custom resolver should answer");
        assert_eq!(result.data["answers"]["Pick one?"].as_str(), Some("Alpha"));
    }

    #[cfg(feature = "mobile")]
    #[tokio::test]
    async fn mobile_task_create_persists_under_the_app_config_home() {
        let temp = tempfile::tempdir().expect("app container");
        let config_home = temp.path().join(branding::DOT_DIR);
        let registry = mobile_tool_registry_with_skill_loader(
            shell_test_ctx(dummy_out()),
            config_home.clone(),
            Arc::new(NoopSkillLoader),
        );
        let task_create = registry
            .find_by_name(tool_task::task::TASK_CREATE_TOOL_NAME)
            .expect("TaskCreate registered");

        task_create
            .call(
                serde_json::json!({
                    "subject": "Generate report",
                    "description": "Exercise the iOS app-private task store"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("TaskCreate writes inside the app container");

        let task_files = std::fs::read_dir(config_home.join("tasks"))
            .expect("tasks root created")
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .flat_map(|entry| {
                std::fs::read_dir(entry.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count();
        assert_eq!(task_files, 1, "one task persisted in the injected home");
    }
}

#[cfg(feature = "mobile")]
mod agent_resume;
