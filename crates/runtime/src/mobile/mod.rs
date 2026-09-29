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
use platform_api::{AuthHandle, OrchestratorHandle};
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
// v3 Phase 1: workflow-on-mobile composition pieces (launcher + deferred
// invoker), consumed by the `host` build path.
#[cfg(feature = "mobile")]
mod turn_durability;
#[cfg(feature = "mobile")]
mod workflow_support;

// First-party local-app host operations as ORDINARY builtin tools (they used
// to be reachable only as `mcp__local_apps__*`, which gave them third-party
// MCP permission semantics they were never meant to have).
#[cfg(feature = "mobile")]
pub mod local_apps_tools;

// LOCAL-APPS (phase 1): the domain ⇄ protocol bridge for the engine-owned
// `local_apps::AppService` — the observer that lowers domain events onto the
// client event sink plus the DTO lowering/raising helpers the `submit` command
// arms use. uniffi-gated like `host` (it names the client-protocol DTO surface,
// which is pulled only under that feature).
#[cfg(feature = "mobile")]
mod local_apps_bridge;

// P1.4: mobile atomic materialization with digest verification (§19.2) for
// the compiled-in builtin plugin bundle the packer (`local_apps::packer`)
// produces. uniffi-gated like its `local_apps_*` siblings — it names
// `local_apps::PackedFile`, which is pulled only under this feature.
#[cfg(feature = "mobile")]
pub mod builtin_bundle;
#[cfg(feature = "mobile")]
mod local_app_plugin_binding;
#[cfg(feature = "mobile")]
mod local_app_runtime_profiles;
#[cfg(feature = "mobile")]
mod local_app_template_catalog;
#[cfg(feature = "mobile")]
mod local_apps_build;
#[cfg(feature = "mobile")]
mod local_apps_host;
// Live per-connection device handles (camera / audio / location /
// notifications) behind a SharedLlm-style swap cell — see the module doc for
// why a bare OnceLock would pin a torn-down engine's Swift objects.
#[cfg(feature = "mobile")]
mod local_apps_device;
// LOCAL-APPS (v3): the app-facing LLM seam — `LocalAppsModel` + the
// `ApiService`-backed `chat` used by the `llm.chat` bridge operation. The
// designer/generation pipeline that used to live behind this seam is gone.
#[cfg(feature = "mobile")]
mod local_apps_llm;
#[cfg(feature = "mobile")]
mod local_apps_mcp;
#[cfg(feature = "mobile")]
mod local_apps_profile;
#[cfg(feature = "mobile")]
mod mcp_transport;
#[cfg(feature = "mobile")]
mod mobile_lsp;
// The Host-owned record of a USER-approved plan. `LocalAppPrepare` reads it so
// a model cannot land a template on a self-reported "the user approved".
#[cfg(feature = "mobile")]
mod plan_approval;

#[cfg(feature = "mobile")]
pub use client::protocol::listings::{
    ModelBillingModeDto, ModelCapabilitiesDto, ModelDetailsDto, ModelPricingDto,
    ModelPricingTierDto, SessionModeDto,
};
#[cfg(feature = "mobile")]
pub use host::{
    build_mobile, build_mobile_engine, build_mobile_engine_inner, build_mobile_inner,
    parse_mobile_provider_config_json, CronDueOccurrenceDto, CronFireStatusDto, CronTaskDto,
    FiredCronJobDto, LocalAppBackgroundRunDto, MobileBuildError, MobileConfig,
    MobileCronStoreHandle, MobileEngineError, MobileEngineHandle, MobileOAuthSessionDto,
    MobileOAuthStateDto, MobileRuntime, ProviderCatalogEntryDto, ProviderConnectionTestDto,
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

// ---------------------------------------------------------------------------
// P1.6 (§19.2) — mobile composes `PluginManager` for the first time.
//
// Mobile's plugin surface is exactly ONE compiled-in plugin
// (`lingxi-local-app`, the on-device Local App authoring/build/test workflow
// bundle under `plugins/lingxi-local-app/`). Unlike `harness-runtime::desktop`, mobile
// has no `~/.claude/plugins` cache to scan, no marketplace, and no
// `--plugin-dir`/`--add-dir` session surface — so mobile never calls
// `PluginManager::install` at all. The single compiled-in plugin is
// registered through `register_verified_builtin`, the door P0a.6 opened
// specifically for a plugin whose `(id, manifest, install_dir)` triple never
// passed through untrusted, attacker-influenced input.
//
// `register_mobile_builtin_plugin` refuses anything whose `manifest.source`
// is not already `PluginSource::BuiltIn` — defense in depth from the
// opposite direction of `PluginManager::install`'s own unconditional
// rejection of that SAME variant, so a manifest that ever passed through a
// network/local-path install arm cannot be smuggled through this door and
// come out re-labeled trusted.
//
// Remaining work is intentionally narrow: §19.0's binary-size/startup
// budgets (`local-apps/src/performance_thresholds.rs`'s `load_baseline()`)
// still need a production measurement call site. The compiled-in bundle and
// verified registration path are wired by `builtin_bundle` plus
// `host::build_mobile_inner`; this module remains the manifest/composition
// seam and does not own runtime measurement.
/// The one plugin name mobile ever discovers (§19.2's completion condition).
#[cfg(feature = "mobile")]
pub const MOBILE_BUILTIN_PLUGIN_NAME: &str = "lingxi-local-app";

/// Manifest fallback used only when the bare `enabledPlugins` key is absent.
#[cfg(feature = "mobile")]
pub const MOBILE_BUILTIN_PLUGIN_DEFAULT_ENABLED: bool =
    builtin_bundle::COMPILED_PLUGIN_DEFAULT_ENABLED;

/// The compiled-in identity of that one plugin, as a fixed UUID.
///
/// Deliberately a CONSTANT, not `PluginId::new()`. `PluginId` is the key that
/// `PluginManager`'s state map, `disable`/`unload_plugin`, and — security
/// relevantly — `PluginBlocklist::is_blocked` all match on. A fresh v4 UUID
/// per call would mean:
///   - two `register_mobile_builtin_plugins` calls against one manager
///     register the SAME plugin twice, under two different ids, so
///     "mobile loads exactly one plugin" stops holding at the manager;
///   - `disable`/`unload_plugin` cannot be handed a stable id by anything
///     that did not itself just call [`mobile_builtin_plugins`]; and
///   - no managed-settings blocklist entry could ever name this plugin,
///     because its id would differ on every boot. `plugin::manager`'s own
///     `register_verified_builtin` doc calls out exactly that failure ("…
///     `PluginId::new()` is a fresh v4 UUID per process, so the check runs
///     but can only match an id the host minted and blocked within the same
///     run").
///
/// The bytes are hand-picked (`6c69 6e67 7869 4c41` is ASCII `lingxiLA`) so
/// the value is visibly a compiled-in constant rather than a captured random
/// id somebody could be tempted to "refresh".
#[cfg(feature = "mobile")]
pub const MOBILE_BUILTIN_PLUGIN_UUID: u128 = 0x6c69_6e67_7869_4c41_0000_0000_0000_0001;

/// The compiled-in [`protocol::PluginId`] for [`MOBILE_BUILTIN_PLUGIN_NAME`].
#[cfg(feature = "mobile")]
#[must_use]
pub fn mobile_builtin_plugin_id() -> protocol::PluginId {
    protocol::PluginId::from_uuid(uuid::Uuid::from_u128(MOBILE_BUILTIN_PLUGIN_UUID))
}

/// Build the compiled-in Local App plugin manifest from its packaged
/// `.lingxi-plugin/plugin.json`. The build script parses and validates that
/// asset and generates these constants, so malformed metadata fails the build
/// rather than panicking during mobile boot. Host lifecycle fields remain
/// owned here.
#[cfg(feature = "mobile")]
fn mobile_builtin_plugin_manifest() -> (protocol::PluginId, plugin::PluginManifest) {
    let id = mobile_builtin_plugin_id();
    let manifest = plugin::PluginManifest {
        id,
        name: builtin_bundle::COMPILED_PLUGIN_NAME.to_string(),
        display_name: Some(builtin_bundle::COMPILED_PLUGIN_DISPLAY_NAME.to_string()),
        default_enabled: builtin_bundle::COMPILED_PLUGIN_DEFAULT_ENABLED,
        version: builtin_bundle::COMPILED_PLUGIN_VERSION.to_string(),
        description: builtin_bundle::COMPILED_PLUGIN_DESCRIPTION.to_string(),
        author: Some(builtin_bundle::COMPILED_PLUGIN_AUTHOR.to_string()),
        author_email: None,
        author_url: None,
        homepage: None,
        source: plugin::PluginSource::BuiltIn,
        components: plugin::PluginComponents::default(),
        trust_level: plugin::default_trust_for_source(&plugin::PluginSource::BuiltIn),
        depends_on: Vec::new(),
        dependencies: Vec::new(),
        user_config: None,
        channels: Vec::new(),
        settings: std::collections::HashMap::new(),
        settings_declared: false,
        keywords: Vec::new(),
        license: None,
        repository: None,
        metadata: None,
    };
    (id, manifest)
}

/// Every plugin mobile ships, compiled in. Exactly one today: the pinned
/// [`MOBILE_BUILTIN_PLUGIN_NAME`] builtin (§19.2's completion condition).
/// Mobile deliberately has no disk-scan/marketplace/session-plugin discovery
/// path, so this IS the entire mobile plugin surface, not a subset of some
/// larger discovered set a caller is expected to filter.
#[cfg(feature = "mobile")]
#[must_use]
pub fn mobile_builtin_plugins() -> Vec<(
    protocol::PluginId,
    plugin::PluginManifest,
    std::path::PathBuf,
)> {
    let (id, manifest) = mobile_builtin_plugin_manifest();
    vec![(id, manifest, std::path::PathBuf::new())]
}

/// Register one mobile-builtin plugin into `manager` through the verified-
/// builtin door, refusing anything whose `manifest.source` is not already
/// `PluginSource::BuiltIn`.
///
/// This refusal is mobile-specific defense in depth, not something
/// `register_verified_builtin` itself enforces (that method stamps `source`
/// to `BuiltIn` unconditionally by design, trusting a caller that already
/// did its own verification before calling it). Gating here too means a
/// manifest that ever passed through a network/local-path install arm cannot
/// reach this door and quietly come out re-labeled trusted.
#[cfg(feature = "mobile")]
pub async fn register_mobile_builtin_plugin(
    manager: &plugin::PluginManager,
    id: &protocol::PluginId,
    manifest: plugin::PluginManifest,
    install_dir: std::path::PathBuf,
) -> Result<(), plugin::PluginManagerError> {
    if !matches!(manifest.source, plugin::PluginSource::BuiltIn) {
        return Err(plugin::PluginManagerError::Io(format!(
            "refusing to register {:?} through the mobile builtin-plugin door: \
             source must be PluginSource::BuiltIn, got {:?}",
            manifest.name, manifest.source
        )));
    }
    manager
        .register_verified_builtin(id, manifest, install_dir)
        .await
}

/// Register every mobile-builtin plugin (today: exactly the one
/// [`mobile_builtin_plugins`] names) into `manager`.
///
/// This is the PRE-materialization shape: every `install_dir` is
/// `PathBuf::new()` and every manifest's `components` is
/// `PluginComponents::default()` (see [`mobile_builtin_plugin_manifest`]), so
/// registering it is a manifest over nothing — `PluginManager::load_plugin`
/// trivially succeeds having loaded zero commands/agents/skills. Kept for the
/// tests below that predate P1.10's boot wiring; the production boot path
/// (`host::build_mobile_inner`) calls
/// [`register_mobile_builtin_plugins_materialized`] instead.
#[cfg(feature = "mobile")]
pub async fn register_mobile_builtin_plugins(
    manager: &plugin::PluginManager,
) -> Result<(), plugin::PluginManagerError> {
    for (id, manifest, install_dir) in mobile_builtin_plugins() {
        register_mobile_builtin_plugin(manager, &id, manifest, install_dir).await?;
    }
    Ok(())
}

/// P1.10 (§19.2) — classify the compiled-in plugin's resolved packer
/// inventory into the [`plugin::PluginComponents`] `PluginManager::load_plugin` reads,
/// following the exact directory convention `plugins/lingxi-local-app/` ships
/// under: every `agents/*.md` path becomes an agent component, every
/// `skills/<name>/SKILL.md` a skill component, every `workflows/*.js` a
/// workflow component. `schemas/*.json` files are content the agents/skills
/// reference by path, not components of their own, so they classify into
/// none of the three and are silently skipped here (still present — digest-
/// verified — at the materialized root either way).
///
/// Driven by `inventory` (the packer's OWN resolved path list for this exact
/// materialized root) rather than by a second, hand-maintained path list, so
/// this can never name a component the materialized root does not actually
/// contain.
#[cfg(feature = "mobile")]
fn mobile_builtin_plugin_components(
    inventory: &[local_apps::PackedFile],
) -> plugin::PluginComponents {
    let mut components = plugin::PluginComponents::default();
    components.lsp_servers = serde_json::from_str(builtin_bundle::COMPILED_PLUGIN_LSP_SERVERS_JSON)
        .expect("build-time validated builtin Plugin LSP declarations");
    for (name, config) in &mut components.lsp_servers {
        // Public plugin manifests key LSP records by name; the map key is
        // authoritative, just as it is in plugin discovery on desktop.
        config.name.clone_from(name);
    }
    for entry in inventory {
        let path = entry.path.as_str();
        let component = plugin::ComponentPath {
            path: std::path::PathBuf::from(path),
            metadata: None,
        };
        if path.starts_with("agents/") && path.ends_with(".md") {
            components.agents.push(component);
        } else if path.starts_with("skills/") && path.ends_with("/SKILL.md") {
            components.skills.push(component);
        } else if path.starts_with("workflows/") && path.ends_with(".js") {
            components.workflows.push(component);
        }
    }
    components
}

/// P1.10 (§19.2) — materialize the compiled-in plugin bundle to a verified,
/// digest-checked root (via [`builtin_bundle::materialize_compiled_in_plugin_bundle`])
/// and build this crate's one compiled-in plugin's manifest with
/// `components` populated FROM that root's own resolved inventory, rather
/// than left at `PluginComponents::default()`.
///
/// # Errors
/// [`builtin_bundle::BuiltinBundleError::BuiltinBundleUnavailable`] under the
/// same conditions [`builtin_bundle::materialize_compiled_in_plugin_bundle`]
/// can fail — never a panic, and never a manifest handed back pointing at an
/// unverified root.
#[cfg(feature = "mobile")]
pub fn materialize_mobile_builtin_plugin(
    bundle_root: &std::path::Path,
    previous_verified_root: Option<&std::path::Path>,
) -> Result<
    (
        protocol::PluginId,
        plugin::PluginManifest,
        std::path::PathBuf,
    ),
    builtin_bundle::BuiltinBundleError,
> {
    let (root, inventory) =
        builtin_bundle::materialize_compiled_in_plugin_bundle(bundle_root, previous_verified_root)?;
    let root = std::fs::canonicalize(&root).map_err(|error| {
        builtin_bundle::BuiltinBundleError::BuiltinBundleUnavailable(format!(
            "failed to canonicalize verified builtin bundle root {}: {error}",
            root.display()
        ))
    })?;
    let (id, mut manifest) = mobile_builtin_plugin_manifest();
    manifest.components = mobile_builtin_plugin_components(&inventory);
    Ok((id, manifest, root))
}

/// P1.10 (§19.2) — the production boot-path sibling of
/// [`register_mobile_builtin_plugins`]: materialize the compiled-in bundle to
/// a verified root FIRST, then register the plugin against THAT root instead
/// of `PathBuf::new()`. `host::build_mobile_inner` calls this, not the
/// pre-materialization function above.
///
/// A materialization failure is reported (never panics) and leaves the
/// plugin unregistered for this boot — the caller decides whether that is
/// fatal; today `build_mobile_inner` logs and continues, matching how
/// [`register_mobile_builtin_plugins`]'s own failure was already handled
/// before this task.
#[cfg(feature = "mobile")]
pub async fn register_mobile_builtin_plugins_materialized(
    manager: &plugin::PluginManager,
    bundle_root: &std::path::Path,
    previous_verified_root: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, plugin::PluginManagerError> {
    let (id, manifest, root) =
        materialize_mobile_builtin_plugin(bundle_root, previous_verified_root)
            .map_err(|error| plugin::PluginManagerError::Io(error.to_string()))?;
    register_mobile_builtin_plugin(manager, &id, manifest, root.clone()).await?;
    Ok(root)
}

#[cfg(all(test, feature = "mobile"))]
mod mobile_plugin_composition_tests {
    use super::*;
    use plugin::{PluginManager, PluginSource};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// A fully-wired `PluginManager` (every registry live, no mocks) rooted at
    /// `root` — the same shape `plugin::manager::register_verified_builtin_tests`'s
    /// own `build_manager` uses, built from `platform-posix-minimal`'s fakes
    /// (already an harness-runtime::mobile dependency) instead of the full `platforms/posix`
    /// crate desktop links.
    fn build_manager(root: &std::path::Path) -> PluginManager {
        let credentials = Arc::new(secret::CredentialManager::new(
            Arc::new(platform_posix_minimal::PlainTextSecureStorage::new()),
            Arc::new(platform_posix_minimal::PosixClock::new()),
            Arc::new(platform_posix_minimal::PosixHttp::new()),
        ));
        PluginManager::new(
            root.to_path_buf(),
            Arc::new(platform_posix_minimal::PosixFileSystem::new(
                root.to_path_buf(),
            )),
            Arc::new(platform_posix_minimal::PosixHttp::new()),
            Arc::new(platform_posix_minimal::PosixRuntime::new()),
            credentials,
            Arc::new(plugin::StrictPluginOnlyPolicy::empty()),
            Arc::new(RwLock::new(CommandRegistry::new())),
            Arc::new(RwLock::new(SkillRegistry::new())),
            Arc::new(RwLock::new(hooks::HookRegistry::new())),
            Arc::new(RwLock::new(outputstyles::OutputStyleRegistry::new())),
            Arc::new(mcp::McpRegistry::new(Arc::new(
                platform_posix_minimal::PosixMcp::new(),
            ))),
            Arc::new(lsp::LspRegistry::new(Arc::new(
                platform_posix_minimal::PosixLsp::new(),
            ))),
            Arc::new(RwLock::new(ToolRegistry::new())),
        )
    }

    /// §19.2's completion condition, asserted against PRODUCTION discovery —
    /// [`mobile_builtin_plugins`] — not a hand-built vector standing in for
    /// it, and against the plugin's OWN name, not merely a count.
    #[tokio::test]
    async fn mobile_discovers_exactly_one_plugin() {
        let discovered = mobile_builtin_plugins();
        assert_eq!(
            discovered.len(),
            1,
            "mobile must discover exactly one compiled-in plugin"
        );
        let (discovered_id, manifest, _) = &discovered[0];
        // Against the LITERAL §19.2 pins, not against
        // `MOBILE_BUILTIN_PLUGIN_NAME` — the manifest is BUILT from that
        // constant, so `manifest.name == MOBILE_BUILTIN_PLUGIN_NAME` is a
        // tautology that stays green through any rename. The completion
        // condition names a specific string; the test has to too.
        assert_eq!(
            manifest.name, "lingxi-local-app",
            "the one discovered plugin must be named lingxi-local-app, not merely \
             counted"
        );
        assert!(
            matches!(manifest.source, PluginSource::BuiltIn),
            "the one discovered plugin must be sourced BuiltIn, got {:?}",
            manifest.source
        );

        // Round-trip through a REAL `PluginManager`: if `mobile_builtin_plugins`
        // ever regressed to returning an empty list (the House Defect this
        // brief warns about — "a discovery path that finds nothing"),
        // `register_mobile_builtin_plugins` would register nothing and
        // `loaded_plugin_ids` would come back empty, failing this too — so
        // this is not satisfied by the manifest list alone.
        let tmp = tempfile::tempdir().expect("temp install dir");
        let manager = build_manager(tmp.path());
        register_mobile_builtin_plugins(&manager)
            .await
            .expect("the mobile builtin plugin must register cleanly");
        // NAME what was counted: the Loaded set must be exactly the id
        // production discovery returned, not "some one plugin". A bare
        // `len() == 1` cannot tell the builtin apart from anything else that
        // happened to end up Loaded.
        assert_eq!(
            manager.loaded_plugin_ids().await,
            vec![*discovered_id],
            "the Loaded set after mobile's plugin boot must be exactly the id \
             mobile_builtin_plugins() returned"
        );
    }

    #[test]
    fn local_app_plugin_no_longer_owns_the_native_typescript_lsp() {
        let components = mobile_builtin_plugin_components(&[]);
        assert!(components.lsp_servers.is_empty());
        let config = crate::mobile::mobile_lsp::global_typescript_lsp_config();
        assert_eq!(
            config.name,
            crate::mobile::mobile_lsp::GLOBAL_TYPESCRIPT_LSP_SERVER_NAME
        );
        assert_eq!(
            config.command,
            "/opt/lingxi/toolchains/typescript/7.0.2/tsc"
        );
        assert_eq!(config.args, ["--lsp", "--stdio"]);
        assert_eq!(
            config.extension_to_language.get(".jsx").map(String::as_str),
            Some("javascriptreact")
        );
        assert_eq!(config.startup_timeout, Some(20_000));
        assert_eq!(config.shutdown_timeout, Some(3_000));
        assert_eq!(config.max_restarts, Some(2));
        assert_eq!(config.diagnostics, Some(true));
    }

    /// The compiled-in plugin's `PluginId` must be a STABLE constant, not a
    /// fresh v4 per call.
    ///
    /// `PluginId` is what `PluginManager`'s state map, `disable`/
    /// `unload_plugin` and `PluginBlocklist::is_blocked` all key on. This is
    /// the paired non-vacuous case for `mobile_discovers_exactly_one_plugin`,
    /// whose `len() == 1` is trivially satisfied because it registers exactly
    /// once — an id that changes per call keeps that assertion green while
    /// breaking the invariant it is supposed to stand for.
    #[tokio::test]
    async fn mobile_builtin_plugin_identity_is_stable_across_calls() {
        let first = mobile_builtin_plugins();
        let second = mobile_builtin_plugins();
        assert_eq!(
            first[0].0, second[0].0,
            "two calls to mobile_builtin_plugins() must yield the SAME PluginId; \
             a per-call id makes the blocklist unfireable and double-registers \
             the plugin"
        );
        assert_eq!(
            first[0].0,
            mobile_builtin_plugin_id(),
            "discovery must hand out the compiled-in constant id"
        );
        // And the constant itself is FROZEN, pinned against a literal rather
        // than against `MOBILE_BUILTIN_PLUGIN_UUID` (which would be a
        // tautology): this id is what per-plugin settings, disable state and
        // blocklist entries are keyed by on device, so changing it silently
        // orphans all of them across an app upgrade.
        assert_eq!(
            mobile_builtin_plugin_id().to_string(),
            "plg:6c696e67-7869-4c41-0000-000000000001",
            "the compiled-in plugin id is an on-device identity; changing it \
             orphans every persisted per-plugin record keyed by it"
        );

        // The observable consequence: registering mobile's plugin set twice
        // against ONE manager must leave exactly one Loaded plugin. With a
        // per-call id the second pass inserts a second state-map entry under
        // a second id and this comes back with two.
        let tmp = tempfile::tempdir().expect("temp install dir");
        let manager = build_manager(tmp.path());
        register_mobile_builtin_plugins(&manager)
            .await
            .expect("first registration must succeed");
        register_mobile_builtin_plugins(&manager)
            .await
            .expect("re-registering the compiled-in plugin must be idempotent");
        assert_eq!(
            manager.loaded_plugin_ids().await,
            vec![mobile_builtin_plugin_id()],
            "registering mobile's builtin set twice must leave exactly the one \
             compiled-in id Loaded, not two entries for the same plugin"
        );
    }

    /// The negative half: a manifest whose source is anything OTHER than
    /// `PluginSource::BuiltIn` must be refused by mobile's own registration
    /// door, not merely "the one plugin present happens to be BuiltIn"
    /// (which a test that never tries a second source cannot distinguish from
    /// "there was nothing else to reject").
    #[tokio::test]
    async fn mobile_loads_only_the_builtin_source() {
        let tmp = tempfile::tempdir().expect("temp install dir");
        let manager = build_manager(tmp.path());

        // Positive control FIRST, so the "was anything rejected?" question is
        // answered against a NON-EMPTY manager. Asserting "zero loaded" over a
        // manager that never had anything in it cannot distinguish "the
        // refusal registered nothing" from "there was nothing here anyway".
        register_mobile_builtin_plugins(&manager)
            .await
            .expect("the real mobile builtin (source already BuiltIn) must register cleanly");
        let builtin_id = mobile_builtin_plugin_id();
        assert_eq!(
            manager.loaded_plugin_ids().await,
            vec![builtin_id],
            "the door must ACCEPT a BuiltIn source — otherwise the refusals below \
             would just be 'this door rejects everything'"
        );

        // Now the negatives: the SAME manifest shape mobile ships, under a
        // DISTINCT id (so a smuggled-in plugin would ADD a state-map entry
        // rather than silently overwrite the builtin's), with `source` swapped
        // to each non-`BuiltIn` variant in turn — as if it had reached this
        // call site via a network / local-path install arm instead of mobile's
        // own verified-builtin construction. Two variants, not one, so a guard
        // that ever narrowed to a single rejected variant is caught.
        let rogue_sources = [
            PluginSource::LocalPath {
                path: std::path::PathBuf::from("/definitely-not-verified"),
            },
            PluginSource::OfficialMarketplace {
                name: MOBILE_BUILTIN_PLUGIN_NAME.to_string(),
            },
        ];
        let rogue_id =
            protocol::PluginId::from_uuid(uuid::Uuid::from_u128(MOBILE_BUILTIN_PLUGIN_UUID + 1));
        assert_ne!(
            rogue_id, builtin_id,
            "the rogue must carry an id of its own, or a successful smuggle \
             would overwrite the builtin instead of showing up as an extra entry"
        );
        let mut refusals = 0_usize;
        for rogue_source in rogue_sources {
            refusals += 1;
            let (_, mut manifest) = mobile_builtin_plugin_manifest();
            manifest.source = rogue_source.clone();

            let error = register_mobile_builtin_plugin(
                &manager,
                &rogue_id,
                manifest,
                tmp.path().to_path_buf(),
            )
            .await
            .expect_err("a non-BuiltIn source must be refused, not silently loaded");
            match error {
                plugin::PluginManagerError::Io(msg) => assert!(
                    msg.contains("BuiltIn") && msg.contains(MOBILE_BUILTIN_PLUGIN_NAME),
                    "refusal must name both the required source and the refused plugin: {msg}"
                ),
                other => panic!("expected PluginManagerError::Io, got {other:?}"),
            }

            // The refusal must have changed nothing: the builtin is still the
            // ONE Loaded plugin and the rogue id is absent. `register_verified_builtin`
            // stamps `source = BuiltIn` unconditionally, so without the guard
            // this rogue would land here re-labeled trusted and this comes back
            // with two ids.
            assert_eq!(
                manager.loaded_plugin_ids().await,
                vec![builtin_id],
                "a refused {rogue_source:?} must leave the Loaded set exactly as it \
                 was — only the compiled-in builtin"
            );
        }

        // Probe-fired control: everything above lives inside a `for` over a
        // fixture list, so an empty (or accidentally-emptied) `rogue_sources`
        // would make this whole test pass having rejected nothing at all.
        assert_eq!(
            refusals, 2,
            "the refusal body must actually have run, once per non-BuiltIn \
             variant — a test that rejects nothing proves nothing"
        );
    }
}

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
            default_model: platform_api::qualified_model_ref(
                platform_api::provider_default_model("anthropic").unwrap_or("claude-sonnet-5"),
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
    "AskUserQuestion",
    "Glob",
    "Grep",
    "Read",
    "Skill",
    "StructuredOutput",
    "WebFetch",
    "WebSearch",
];

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
    automation: Option<Arc<dyn platform_api::AndroidUiAutomation>>,
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
    use platform_api::{
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
/// Local App skills are no longer bundled here. The returned registry is kept
/// as a compatibility seam for packager callers while the verified Plugin
/// registry is the only production source for those skills.
#[must_use]
pub fn mobile_skill_registry() -> SkillRegistry {
    let mut reg = SkillRegistry::new();
    skill_api::register_mobile(&mut reg);
    reg
}

/// Return skill names from the build-time verified Local App Plugin inventory.
/// This is the only production/test seam for component-name derivation after
/// mobile bundled Local App skill bodies were removed.
#[cfg(feature = "mobile")]
#[must_use]
pub fn mobile_plugin_skill_names() -> Vec<String> {
    builtin_bundle::compiled_plugin_skill_names()
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
/// `/loop`). Local App skills are file-backed Plugin commands and are added by
/// `PluginManager` after this base catalog is installed.
pub(crate) fn register_mobile_bundled_prompt_commands(reg: &mut CommandRegistry) {
    // Bundled programmatic skills (`/loop`), mirroring desktop. Gated on the cron
    // kill-switch (loop.ts:83); mobile starts no cron scheduler so a scheduled
    // job is inert, but the skill's listing/usage path is harmless and faithful.
    let cron_enabled = tool_cron::cron_tools_enabled();
    command_api::builtins::register_bundled_skills(reg, cron_enabled);
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
    fn chat_profile_exposes_only_the_read_only_mobile_allowlist() {
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
            "camera",
            "notification",
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
                    "subject": "Generate local app",
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
