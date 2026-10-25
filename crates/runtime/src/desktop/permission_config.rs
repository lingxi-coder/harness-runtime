#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;

use super::DesktopConfig;

#[path = "computer_permission_rules.rs"]
mod computer_permission_rules;
pub(super) use computer_permission_rules::{
    append_desktop_computer_permission_rules, DesktopComputerPermissionIntegration,
};

/// Parse just the settings fields the sandbox runtime consumes, retaining the
/// exact raw JSON until the filesystem path fields cross into Rust's OS-facing
/// `String` representation. Other top-level settings cannot cause this sandbox
/// tier to disappear merely because serde would reject an unrelated value.
fn sandbox_settings_tier_from_raw(raw: &str) -> Option<sandbox::runtime_config::SettingsJson> {
    use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson, SettingsPermissions};

    let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(raw).ok()?;
    let root = source.at_pointer("")?;
    root.object_entries()?;

    let sandbox = if let Some(value) = root.get_property("sandbox") {
        if value.is_null() {
            None
        } else {
            let raw_sandbox = source.subprojection("/sandbox").ok()?;
            let value = lingxi_core::settings::sandbox_execution_value(&raw_sandbox).ok()??;
            let value = serde_json::Value::Object(value.into_iter().collect());
            let mut sandbox = serde_json::from_value::<SandboxSettingsJson>(value).ok()?;
            if let Ok(raw_ignore_violations) = raw_sandbox.subprojection("/ignoreViolations") {
                if raw_ignore_violations
                    .at_pointer("")
                    .is_some_and(|value| value.object_entries().is_some())
                {
                    sandbox.ignore_violations_source = Some(raw_ignore_violations);
                }
            }
            Some(sandbox)
        }
    } else {
        None
    };

    let permissions = if let Some(value) = root.get_property("permissions") {
        if value.is_null() {
            None
        } else {
            let raw_permissions = source.subprojection("/permissions").ok()?;
            let json = raw_permissions.to_json_string().ok()?;
            Some(serde_json::from_str::<SettingsPermissions>(&json).ok()?)
        }
    } else {
        None
    };

    Some(SettingsJson {
        permissions,
        sandbox,
        settings_dir: None,
    })
}

/// Derive the [`permission::SandboxAutoAllowConfig`] the enforced
/// [`permission::PermissionPolicy`] consults from the same `settings.json`
/// tiers the policy block already reads.
///
/// `raw_tiers` is the per-tier raw `settings.json` text in ASCENDING priority
/// (user → project → local), exactly the order the enforcement block loads its
/// rules; a later tier's `sandbox` field overrides an earlier one (last write
/// wins), mirroring how `defaultMode` is resolved there. Each tier is parsed as
/// a [`sandbox::runtime_config::SettingsJson`]; only the `sandbox` subsection
/// (and its `permissions` are irrelevant to the three auto-allow fields) is
/// consulted, folded into one merged
/// [`sandbox::runtime_config::SandboxRuntimeConfig`] via
/// [`sandbox::policy_convert::convert_settings_to_runtime_config`].
///
/// The three fields the bash sandbox-auto-allow branch reads are then copied
/// out (`enabled`, `auto_allow_bash_if_sandboxed`, `excluded_commands`). One
/// faithfulness fix vs. the raw conversion: claude-code's
/// `isAutoAllowBashIfSandboxedEnabled()` defaults **true**, but the Rust
/// `SandboxRuntimeConfig::auto_allow_bash_if_sandboxed` is a bare `bool` that
/// `serde`-defaults to `false` and the converter only sets it when the settings
/// explicitly carry it. So we recover the explicit/absent distinction from the
/// per-tier [`sandbox::runtime_config::SandboxSettingsJson::auto_allow_bash_if_sandboxed`]
/// (`Option<bool>`): the last tier that set it wins; if NO tier set it the TS
/// default `true` applies.
#[must_use]
pub(super) fn sandbox_auto_allow_from_settings_tiers(
    raw_tiers: &[&str],
    settings_dir: &std::path::Path,
) -> permission::sandbox_auto_allow::SandboxAutoAllowConfig {
    use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson, SettingsPermissions};

    // Fold each tier's `sandbox` subsection, last write wins per the whole
    // subsection (matching how the converter consumes a single `SettingsJson`).
    let mut merged_sandbox: Option<SandboxSettingsJson> = None;
    // Accumulate the merged `permissions` across tiers so the converter sees the
    // full allow/deny/additionalDirectories feed (extend, not last-write-wins, for
    // the rule lists). The auto-allow config only reads `enabled` /
    // `excluded_commands`, but threading permissions keeps both folds symmetric.
    let mut merged_perms = SettingsPermissions::default();
    let mut saw_perms = false;
    // Track the explicit auto-allow override separately so the TS default (true)
    // can be applied only when NO tier set it.
    let mut explicit_auto_allow: Option<bool> = None;
    for raw in raw_tiers {
        let Some(parsed) = sandbox_settings_tier_from_raw(raw) else {
            continue;
        };
        if let Some(p) = parsed.permissions {
            saw_perms = true;
            merged_perms.allow.extend(p.allow);
            merged_perms.deny.extend(p.deny);
            merged_perms
                .additional_directories
                .extend(p.additional_directories);
        }
        if let Some(s) = parsed.sandbox {
            if let Some(v) = s.auto_allow_bash_if_sandboxed {
                explicit_auto_allow = Some(v);
            }
            merged_sandbox = Some(s);
        }
    }

    let runtime = sandbox::policy_convert::convert_settings_to_runtime_config(
        &SettingsJson {
            permissions: saw_perms.then_some(merged_perms),
            sandbox: merged_sandbox,
            settings_dir: Some(settings_dir.to_path_buf()),
        },
        // The auto-allow config reads only `enabled` / `excluded_commands`, so no
        // session seed context is needed (claude temp dir / settings-file /
        // worktree paths are owned by the posix `prepare` layer here).
        &sandbox::policy_convert::SandboxConvertContext::default(),
    );

    permission::sandbox_auto_allow::SandboxAutoAllowConfig::new(
        runtime.enabled,
        // claude-code `isAutoAllowBashIfSandboxedEnabled()` defaults TRUE.
        explicit_auto_allow.unwrap_or(true),
        runtime.excluded_commands,
    )
}

/// Add config-side MCP tool policy rules to the boot rule vector.
pub(super) fn append_mcp_permission_rules(
    rules: &mut Vec<permission::PermissionRule>,
    servers: &[mcp::McpServerConfig],
    allow_managed_permission_rules_only: bool,
) {
    if allow_managed_permission_rules_only {
        return;
    }
    for server in servers {
        rules.extend(permission::permission_rules_from_mcp_tool_policies(
            &server.name,
            &server.tools,
        ));
    }
}

pub(super) fn append_restricted_builtin_denies(
    rules: &mut Vec<permission::PermissionRule>,
    allowed_tools: Option<&[String]>,
) {
    const DEFAULT_RESTRICTED_DENIES: &[&str] = &["Bash", "PowerShell", "REPL", "WebFetch"];
    for tool_name in DEFAULT_RESTRICTED_DENIES {
        if allowed_tools.is_some_and(|tools| tools.iter().any(|tool| tool == tool_name)) {
            continue;
        }
        rules.push(permission::PermissionRule {
            value: permission::PermissionRuleValue {
                tool_name: (*tool_name).to_string(),
                rule_content: None,
                original_rule: None,
            },
            behavior: permission::PermissionBehavior::Deny,
            source: permission::PermissionRuleSource::CliArg,
        });
    }
}

/// (SANDBOX.1) Fold the `sandbox` subsection of the settings tiers (ascending
/// priority, last write wins) into a full [`sandbox::runtime_config::SandboxRuntimeConfig`].
///
/// Sibling of [`sandbox_auto_allow_from_settings_tiers`], but returns the whole
/// runtime config so the composition root can (a) decide `sandbox_available` from
/// `cfg.enabled` and (b) hand the live network/filesystem policy to the bash
/// sandbox path. Matches claude-code: the sandbox is opt-in via `sandbox.enabled`
/// (default OFF — an empty/absent subsection yields `enabled = false`).
/// (PERM.1) Decide whether `build()` wraps the base gate with
/// [`permission::PolicyPermissionGate`] (i.e. enforces deny/allow rules + mode +
/// sandbox-auto-allow). Pure so it is unit-testable; see the call site in
/// [`build`] for the full rationale.
///
/// - `BypassPermissions` mode (`--dangerously-skip-permissions`) STILL enforces:
///   claude-code never drops the permission layer — `checkPermissions` runs the
///   deny/ask rule walks first and bypass short-circuits to allow AFTER them
///   (`permissions.ts` step order: 1a deny … 2a bypass). The wrap is what
///   provides that auto-allow; without it the BASE gate decides every call, and
///   an interactive base (`TuiPermissionGate`) prompts on every tool use —
///   the exact opposite of bypass.
/// - An explicit `LINGXI_ENFORCE_PERMISSIONS` value wins: falsey
///   (`""|0|off|false|no`) ⇒ off, anything else ⇒ on.
/// - Unset ⇒ default-on ONLY for the CLI/desktop `NoOpPermissionGate` inner
///   (`use_noop_inner == true`); transport hosts (`AdapterPermissionGate`) keep
///   their prior env-opt-in behavior so their remote-driven gate is unchanged.
pub(super) fn should_enforce_permissions(
    env_value: Option<&str>,
    use_noop_inner: bool,
    mode: permission::PermissionMode,
) -> bool {
    let _ = mode;
    // `use_noop_inner` no longer gates the default: claude-code enforces ONE core
    // policy on every host, so transport hosts (the bridge-server's
    // AdapterPermissionGate) ALSO wrap with the local PolicyPermissionGate by
    // default — the adapter gate becomes the Ask-delegation transport (an
    // unresolved mutating Ask still forwards to the remote client), but local
    // deny/allow rules + defaultMode now bind regardless of what the client
    // replicates. The explicit env escape hatch + BypassPermissions still opt out.
    let _ = use_noop_inner;
    match env_value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "off" | "false" | "no"
        ),
        None => true,
    }
}

/// Everything the boot permission-policy construction folds out of the
/// settings tiers, produced by [`load_boot_permission_tiers_with_policy_snapshot`].
pub(super) struct BootPermissionTiers {
    /// Permission rules accumulated from every tier (bucketed by source at
    /// `PermissionPolicy::from_rules` time; `authorize` walks them by priority).
    pub(super) rules: Vec<permission::PermissionRule>,
    /// Highest-priority `permissions.defaultMode` (tiers are read in ascending
    /// priority, so the last write — the managed tier — wins).
    pub(super) mode: permission::PermissionMode,
    /// Explicit flag/managed defaults override a remembered user choice.
    pub(super) mode_preference_allowed: bool,
    /// Sticky `disableBypassPermissionsMode: "disable"` killswitch — true when
    /// ANY tier (managed included) disables `BypassPermissions` mode.
    pub(super) bypass_disabled: bool,
    /// Sticky `disableAutoMode: "disable"` killswitch (claude-code `Bpa()`) —
    /// true when ANY tier disables auto mode at either settings position. Set on
    /// the boot policy and applied at mode-load (auto → default downgrade).
    pub(super) auto_mode_disabled: bool,
    /// Sticky `autoMode.classifyAllShell` escalation (claude-code `QOi()`) — true
    /// when ANY tier sets `autoMode.classifyAllShell === true`. Set on the boot
    /// policy so every `Bash`/`PowerShell` allow rule is suspended in auto mode.
    pub(super) classify_all_shell: bool,
    /// Trusted-source AutoMode merge for classifier context. `None` means at
    /// least one contributing trusted source was unavailable or invalid.
    pub(super) classifier_auto_mode_settings:
        Option<lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings>,
    /// Union of every tier's `permissions.additionalDirectories` (raw paths;
    /// `authorize` resolves them against the policy roots via `expand_path`).
    pub(super) additional_working_dirs: permission::working_dirs::AdditionalWorkingDirs,
    /// Sticky OR of `permissions.blockReadsOutsideWorkingDirectories` across every
    /// settings tier — `true` in ANY source wins (oracle managed merge).
    pub(super) block_reads_outside_working_directories: bool,
    /// Raw tier texts in ASCENDING priority INCLUDING the managed tier(s) —
    /// feeds the sandbox-auto-allow derivation (last write wins, so a managed
    /// `sandbox.*` overrides user/project/local).
    pub(super) raw_tiers: Vec<String>,
    /// Enterprise gate that disables non-managed permission persistence.
    pub(super) allow_managed_permission_rules_only: bool,
}

/// Read the boot permission-settings tiers in ASCENDING priority — user →
/// project → local → managed (policySettings) — and fold them into rules +
/// scalars for the boot `PermissionPolicy` (parity 2.1.207 P1-10).
///
/// Tier semantics (claude-code `SETTING_SOURCES`: `userSettings→projectSettings
/// →localSettings→flagSettings→policySettings`, later overrides earlier;
/// `flagSettings` has no boot analog here — spec §4e):
/// - settings.local.json is read after project so an `AllowAlways` persisted
///   there is honored on the next enforced boot; rules from every tier
///   ACCUMULATE (deny-wins is behavior-first in `authorize`).
/// - `--setting-sources` scope `(include_user, include_project)` gates the
///   user tier and the project+local tiers respectively — but NOT the managed
///   tier: claude-code's `Xv()` unconditionally re-adds `"policySettings"` to
///   the allowed-source set, so managed rules can NEVER be excluded.
/// - Managed tiers (`managed-settings.json` + `managed-settings.d/*.json`,
///   already composed by the shared managed-policy selector) parse with
///   `PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed)` (`RKt()→Fwt("policySettings")`),
///   so enterprise deny/ask/allow rules bind on the boot policy and decisions
///   cite "enterprise managed settings". Managed `defaultMode` /
///   `disableBypassPermissionsMode` / `additionalDirectories` fold like any
///   other tier (read LAST → managed scalars win).
/// - `allowManagedPermissionRulesOnly` lockdown (claude-code `$wt()`): when ANY
///   managed tier sets the top-level flag true, only `PolicySettings`-sourced
///   rules are retained — "User, project, local, and CLI argument permission
///   rules are ignored." (Scalar folds are NOT affected; the schema scopes the
///   lockdown to permission RULES.)
fn merge_trusted_auto_mode_settings(
    trusted_raw_tiers: &[String],
) -> Option<lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings> {
    use lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    let mut merged = PermissionClassifierAutoModeSettings::default();
    for raw in trusted_raw_tiers {
        let source = Utf16JsonProjection::parse(raw).ok()?;
        let Some(auto_mode) = source.get_property("autoMode") else {
            continue;
        };
        if auto_mode.object_entries().is_none() {
            return None;
        }
        let auto_mode = source.subprojection("/autoMode").ok()?;
        for (key, destination) in [
            ("allow", &mut merged.allow),
            ("soft_deny", &mut merged.soft_deny),
            ("hard_deny", &mut merged.hard_deny),
            ("environment", &mut merged.environment),
        ] {
            let Some(values) = auto_mode.get_property(key) else {
                continue;
            };
            let count = values.array_len()?;
            for index in 0..count {
                let value = values.get_index(index)?;
                if value.code_units().is_none() {
                    return None;
                }
                destination.push(auto_mode.subprojection(&format!("/{key}/{index}")).ok()?);
            }
        }
        if let Some(value) = auto_mode.get_property("classifyAllShell") {
            merged.classify_all_shell |= value.as_bool()?;
        }
    }
    Some(merged)
}

/// Reload just the trusted sources that feed Native's classifier AutoMode
/// settings after the settings watcher observes a live user or policy write.
/// Project/local settings remain excluded. A configured CLI flag tier without
/// its original source text stays unknown because the typed settings model
/// drops the raw `autoMode` block.
pub(super) async fn reload_classifier_auto_mode_settings(
    user_settings_path: &std::path::Path,
    include_user_settings: bool,
    flag_settings_configured: bool,
    flag_settings_source: Option<&str>,
    managed_snapshot: &crate::desktop::settings_watch::ManagedSettingsSnapshot,
) -> Option<lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings> {
    if flag_settings_configured && flag_settings_source.is_none() {
        return None;
    }

    let mut trusted_raw_tiers = Vec::new();
    if include_user_settings {
        if let Ok(raw) = tokio::fs::read_to_string(user_settings_path).await {
            trusted_raw_tiers.push(raw);
        }
    }
    if let Some(raw) = flag_settings_source {
        trusted_raw_tiers.push(raw.to_owned());
    }
    if managed_snapshot.read_failed() {
        return None;
    }
    if let Some(raw) = managed_snapshot.effective_json() {
        trusted_raw_tiers.push(raw);
    }
    merge_trusted_auto_mode_settings(&trusted_raw_tiers)
}

pub(super) async fn load_boot_permission_tiers_with_policy_snapshot(
    lingxi_home: &std::path::Path,
    cwd: &std::path::Path,
    setting_source_scope: (bool, bool),
    flag_settings: Option<&lingxi_core::settings::SettingsJson>,
    flag_settings_source: Option<&str>,
    managed_snapshot: &crate::desktop::settings_watch::ManagedSettingsSnapshot,
) -> BootPermissionTiers {
    let mut rules = Vec::new();
    // With no explicit setting, new sessions start in Auto.  The resolved
    // mode is still passed through the existing model/provider/killswitch gate
    // below, so unsupported routes safely downgrade to Default.
    let mut mode = permission::PermissionMode::Auto;
    let mut bypass_disabled = false;
    let mut auto_mode_disabled = false;
    let mut classify_all_shell = false;
    let mut additional_working_dirs = permission::working_dirs::AdditionalWorkingDirs::new();
    let mut block_reads_outside_working_directories = false;
    // Retain each tier's raw text (in ascending priority) so the
    // sandbox-auto-allow config can be derived from the SAME settings.
    let mut raw_tiers: Vec<String> = Vec::new();
    // Native classifier policy reads only trusted user/flag/managed sources.
    // Keep this order independent from the broader user/project/local policy fold.
    let mut trusted_auto_mode_raw_tiers: Vec<String> = Vec::new();
    let mut trusted_auto_mode_sources_complete =
        flag_settings.is_none() || flag_settings_source.is_some();
    let (incl_user_settings, incl_project_settings) = setting_source_scope;
    for (path, source, included) in [
        (
            lingxi_home.join("settings.json"),
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User),
            incl_user_settings,
        ),
        (
            cwd.join(branding::DOT_DIR).join("settings.json"),
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            incl_project_settings,
        ),
        (
            cwd.join(branding::DOT_DIR).join("settings.local.json"),
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
            incl_project_settings,
        ),
    ] {
        if !included {
            continue;
        }
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            match permission::permission_rules_from_settings_json(&raw, source) {
                Ok(mut r) => {
                    // parity 2.1.210: warn when a `Write`/`NotebookEdit`/
                    // `MultiEdit`/`Glob` rule carries a path that no file-permission
                    // matcher will ever see (those tools share `Edit`/`Read` rules).
                    let display = path.display().to_string();
                    for rule in &r {
                        if let Some(line) =
                            permission::permission_rule_startup_warning(rule, &display)
                        {
                            tracing::warn!("{line}");
                        }
                    }
                    rules.append(&mut r);
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "skipping malformed settings permissions"
                ),
            }
            if let Some(m) = permission::default_mode_from_settings_json(&raw) {
                // MODE-SETTINGS-AUTO-TRUST-01 / 2.1.257 `C(e)`: auto AND
                // bypassPermissions are only honored from a TRUSTED tier
                // (policy/user/flag). Project/local are repo-controllable.
                if permission::default_mode_applies_from_source(m, source) {
                    mode = m; // later tiers read last → their defaultMode wins
                } else if m == permission::PermissionMode::BypassPermissions {
                    tracing::warn!(
                        source = ?source,
                        "{}",
                        permission::UNTRUSTED_BYPASS_DEFAULT_MODE_WARN
                    );
                } else {
                    tracing::warn!(
                        source = ?source,
                        "{}",
                        permission::UNTRUSTED_AUTO_DEFAULT_MODE_WARN
                    );
                }
            }
            if permission::bypass_permissions_disabled_from_settings_json(&raw) {
                bypass_disabled = true; // sticky: any tier disabling wins
            }
            if permission::auto_mode_disabled_from_settings_json(&raw) {
                auto_mode_disabled = true; // sticky: any tier disabling wins (Bpa)
            }
            if permission::classify_all_shell_from_settings_json(&raw) {
                classify_all_shell = true; // sticky: any tier enabling wins (QOi)
            }
            // (#34) Union this tier's additionalDirectories into the
            // working-dir set (claude-code merges across SETTING_SOURCES).
            additional_working_dirs.extend_from_source(
                permission::additional_directories_from_settings_json(&raw),
                source,
            );
            if permission::block_reads_outside_working_directories_from_settings_json(&raw) {
                block_reads_outside_working_directories = true; // sticky: any tier arming wins
            }
            if source
                == permission::PermissionRuleSource::Settings(
                    lingxi_core::types::SettingsScope::User,
                )
            {
                trusted_auto_mode_raw_tiers.push(raw.clone());
            }
            raw_tiers.push(raw); // ascending priority preserved for sandbox derivation
        }
    }
    if let Some(raw_source) = flag_settings_source {
        trusted_auto_mode_raw_tiers.push(raw_source.to_owned());
    }
    if let Some(raw) = flag_settings.and_then(|settings| serde_json::to_string(settings).ok()) {
        let source = permission::PermissionRuleSource::FlagSettings;
        match permission::permission_rules_from_settings_json(&raw, source) {
            Ok(mut r) => {
                for rule in &r {
                    if let Some(line) =
                        permission::permission_rule_startup_warning(rule, "flag settings")
                    {
                        tracing::warn!("{line}");
                    }
                }
                rules.append(&mut r);
            }
            Err(e) => tracing::warn!(error = %e, "skipping malformed flag settings permissions"),
        }
        if let Some(m) = permission::default_mode_from_settings_json(&raw) {
            if permission::default_mode_applies_from_source(m, source) {
                mode = m;
            } else if m == permission::PermissionMode::BypassPermissions {
                tracing::warn!(
                    source = ?source,
                    "{}",
                    permission::UNTRUSTED_BYPASS_DEFAULT_MODE_WARN
                );
            } else {
                tracing::warn!(
                    source = ?source,
                    "{}",
                    permission::UNTRUSTED_AUTO_DEFAULT_MODE_WARN
                );
            }
        }
        if permission::bypass_permissions_disabled_from_settings_json(&raw) {
            bypass_disabled = true;
        }
        if permission::auto_mode_disabled_from_settings_json(&raw) {
            auto_mode_disabled = true;
        }
        if permission::classify_all_shell_from_settings_json(&raw) {
            classify_all_shell = true;
        }
        additional_working_dirs.extend_from_source(
            permission::additional_directories_from_settings_json(&raw),
            source,
        );
        if permission::block_reads_outside_working_directories_from_settings_json(&raw) {
            block_reads_outside_working_directories = true;
        }
        raw_tiers.push(raw);
    }
    // Managed (policySettings) tier — HIGHEST priority, read LAST. Deliberately
    // NOT gated by `--setting-sources` (see the doc comment above).
    let managed_tiers = managed_snapshot
        .effective_json()
        .into_iter()
        .collect::<Vec<_>>();
    trusted_auto_mode_sources_complete &= !managed_snapshot.read_failed();
    for raw in &managed_tiers {
        trusted_auto_mode_raw_tiers.push(raw.clone());
        match permission::permission_rules_from_settings_json(
            raw,
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed),
        ) {
            Ok(mut r) => {
                // parity 2.1.210: same file-matcher warning for managed rules.
                for rule in &r {
                    if let Some(line) =
                        permission::permission_rule_startup_warning(rule, "managed policy settings")
                    {
                        tracing::warn!("{line}");
                    }
                }
                rules.append(&mut r);
            }
            Err(e) => tracing::warn!(
                error = %e,
                "skipping malformed managed settings permissions"
            ),
        }
        if let Some(m) = permission::default_mode_from_settings_json(raw) {
            mode = m; // managed read last → its defaultMode wins
        }
        if permission::bypass_permissions_disabled_from_settings_json(raw) {
            bypass_disabled = true; // managed killswitch binds (sticky)
        }
        if permission::auto_mode_disabled_from_settings_json(raw) {
            auto_mode_disabled = true; // managed auto-mode killswitch binds (sticky)
        }
        if permission::classify_all_shell_from_settings_json(raw) {
            classify_all_shell = true; // managed classifyAllShell binds (sticky, QOi)
        }
        additional_working_dirs.extend_from_source(
            permission::additional_directories_from_settings_json(raw),
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed),
        );
        if permission::block_reads_outside_working_directories_from_settings_json(raw) {
            block_reads_outside_working_directories = true; // managed arming binds (sticky)
        }
    }
    let allow_managed_permission_rules_only = managed_tiers
        .iter()
        .any(|raw| permission::allow_managed_permission_rules_only_from_settings_json(raw));
    if allow_managed_permission_rules_only {
        rules.retain(|r| {
            r.source
                == permission::PermissionRuleSource::Settings(
                    lingxi_core::types::SettingsScope::Managed,
                )
        });
    }
    let mode_preference_allowed = !managed_tiers
        .iter()
        .any(|raw| permission::default_mode_from_settings_json(raw).is_some())
        && flag_settings
            .and_then(|settings| serde_json::to_string(settings).ok())
            .and_then(|raw| permission::default_mode_from_settings_json(&raw))
            .is_none();
    raw_tiers.extend(managed_tiers);
    let classifier_auto_mode_settings = if trusted_auto_mode_sources_complete {
        merge_trusted_auto_mode_settings(&trusted_auto_mode_raw_tiers)
    } else {
        None
    };
    if managed_snapshot.read_failed() {
        // A present but unresolved admin source must not look like an absent
        // policy. Keep any recognized restrictions above and close the host's
        // ambient permission paths until the next successful refresh.
        mode = permission::PermissionMode::Default;
        bypass_disabled = true;
        auto_mode_disabled = true;
        block_reads_outside_working_directories = true;
    }
    BootPermissionTiers {
        rules,
        mode,
        mode_preference_allowed: mode_preference_allowed && !managed_snapshot.read_failed(),
        bypass_disabled,
        auto_mode_disabled,
        classify_all_shell,
        classifier_auto_mode_settings,
        additional_working_dirs,
        block_reads_outside_working_directories,
        raw_tiers,
        allow_managed_permission_rules_only: allow_managed_permission_rules_only
            || managed_snapshot.read_failed(),
    }
}

/// Build the MANAGED (`policySettings`) model-restriction view from the one
/// Native-selected policy projection. Incomplete source selection is a failed
/// policy read, not an absent policy. The selector has already preserved local
/// file order and the first applicable MDM candidate.
pub(super) fn managed_model_policy_source(
    snapshot: &crate::desktop::settings_watch::ManagedSettingsSnapshot,
) -> llm_runtime::model::allowlist::PolicySource {
    use llm_runtime::model::allowlist::{PolicyModelView, PolicySource};
    if snapshot.read_failed() {
        return PolicySource::Failed;
    }
    let Some(projection) = snapshot.effective_projection() else {
        return PolicySource::Loaded(PolicyModelView::default());
    };
    let available_models = match projection.get_property("availableModels") {
        None => None,
        Some(value) => {
            let Some(count) = value.array_len() else {
                return PolicySource::Failed;
            };
            if (0..count).any(|index| {
                value
                    .get_index(index)
                    .and_then(|entry| entry.code_units())
                    .is_none()
            }) {
                return PolicySource::Failed;
            }
            match projection.subprojection("/availableModels") {
                Ok(value) => Some(value),
                Err(_) => return PolicySource::Failed,
            }
        }
    };
    let enforce = match projection.get_property("enforceAvailableModels") {
        None => None,
        Some(value) => match value.as_bool() {
            Some(value) => Some(value),
            None => return PolicySource::Failed,
        },
    };
    let model_overrides = match projection.get_property("modelOverrides") {
        None => None,
        Some(_) => {
            let Ok(raw) = projection
                .subprojection("/modelOverrides")
                .and_then(|value| value.to_json_string())
            else {
                return PolicySource::Failed;
            };
            match serde_json::from_str::<std::collections::BTreeMap<String, String>>(&raw) {
                Ok(value) => Some(value),
                Err(_) => return PolicySource::Failed,
            }
        }
    };
    PolicySource::Loaded(PolicyModelView {
        available_models,
        enforce,
        model_overrides,
    })
}

/// Resolve the model setting's ownership from the same effective settings
/// snapshot used by the desktop composition root. Explicit CLI/env pins are
/// user-owned; a managed `settings.model` is administrator-owned; and a
/// missing setting falls back to the provider catalog tier.
#[must_use]
pub(super) fn model_provenance_for_config(
    cfg: &DesktopConfig,
    effective_settings: Option<&lingxi_core::settings::EffectiveSettings>,
) -> lingxi_core::host::ModelProvenance {
    if cfg.default_model_explicit || cfg.default_model_env_pinned {
        return lingxi_core::host::ModelProvenance::UserOrEnv;
    }
    match effective_settings.and_then(model_source_contributor) {
        Some(lingxi_core::settings::tracer::Source::Managed)
            if effective_settings.is_some_and(model_setting_is_nonempty) =>
        {
            lingxi_core::host::ModelProvenance::ManagedAdministratorDefault
        }
        Some(
            lingxi_core::settings::tracer::Source::Env
            | lingxi_core::settings::tracer::Source::User
            | lingxi_core::settings::tracer::Source::Project
            | lingxi_core::settings::tracer::Source::Local
            | lingxi_core::settings::tracer::Source::Cli,
        ) => lingxi_core::host::ModelProvenance::UserOrEnv,
        Some(lingxi_core::settings::tracer::Source::Managed)
        | Some(lingxi_core::settings::tracer::Source::Defaults)
        | None => lingxi_core::host::ModelProvenance::ProviderCatalogTier,
    }
}

/// Return the exact winning configured `settings.model` string projection.
///
/// The source projection is authoritative: a valid JavaScript UTF-16 string
/// from any admitted settings tier can contain an unpaired surrogate and
/// therefore be absent from the typed convenience field without being absent
/// from Native's effective settings. Explicit CLI and environment pins retain
/// ownership and suppress this configured-source carrier.
#[must_use]
pub(super) fn effective_model_source_for_config(
    cfg: &DesktopConfig,
    effective_settings: Option<&lingxi_core::settings::EffectiveSettings>,
) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
    if cfg.default_model_explicit || cfg.default_model_env_pinned {
        return None;
    }
    let settings = effective_settings?;
    let source = model_source_contributor(settings)?;
    if source == lingxi_core::settings::tracer::Source::Defaults {
        return None;
    }
    model_source_from_effective_settings(settings)
}

fn model_source_contributor(
    settings: &lingxi_core::settings::EffectiveSettings,
) -> Option<lingxi_core::settings::tracer::Source> {
    settings
        .source_field_contributors("model")
        .last()
        .copied()
        .or_else(|| {
            settings
                .effective_for("model")
                .and_then(|provenance| provenance.contributors.last().copied())
        })
}

fn model_setting_is_nonempty(settings: &lingxi_core::settings::EffectiveSettings) -> bool {
    model_source_from_effective_settings(settings).is_some()
        || settings
            .settings
            .model
            .as_deref()
            .is_some_and(|model| !lingxi_core::host::effort::trim_js_whitespace(model).is_empty())
}

fn model_source_from_effective_settings(
    settings: &lingxi_core::settings::EffectiveSettings,
) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
    let model = settings.source_field("model")?;
    let units = model.string_units("")?;
    // Native uses JavaScript truthiness for the configured model. Whitespace
    // strings are truthy; only the empty string is pruned here.
    (!units.is_empty()).then_some(model)
}

/// Return the effective managed `settings.model`, when it is the winning model
/// source and no explicit CLI/env pin is in force. This keeps the existing
/// provider-qualified model reference intact for later `parse_model_ref`.
#[must_use]
pub(super) fn managed_model_setting_for_config(
    cfg: &DesktopConfig,
    effective_settings: Option<&lingxi_core::settings::EffectiveSettings>,
) -> Option<String> {
    if cfg.default_model_explicit || cfg.default_model_env_pinned {
        return None;
    }
    let settings = effective_settings?;
    if model_source_contributor(settings) != Some(lingxi_core::settings::tracer::Source::Managed) {
        return None;
    }
    if let Some(source) = settings.source_field("model") {
        let units = source.string_units("")?;
        let model = String::from_utf16(&units).ok()?;
        return (!lingxi_core::host::effort::trim_js_whitespace(&model).is_empty())
            .then_some(model);
    }
    settings
        .settings
        .model
        .clone()
        .filter(|model| !lingxi_core::host::effort::trim_js_whitespace(model).is_empty())
}

/// The MANAGED `availableModels` allowlist + `modelOverrides` in effect for the
/// selection-restriction consumer surfaces (`/model` picker filter, subagent /
/// plan-mode model resolution — parity 2.1.207 H-BIN-08), or `(None, empty)`
/// when no policy restriction is active.
///
/// Reads the SAME managed policy tier the boot default-model constraint resolves
/// (`managed_model_policy_source` → `resolve_enforcement`); warnings are
/// suppressed here since the boot path already emits them once. A `Refused`
/// (policy failed to parse) or `Inactive` source yields `(None, empty)`, leaving
/// the consumer unrestricted — the boot default-model constraint owns the
/// fail-closed behavior for the parse-failure case.
pub async fn managed_model_allowlist(
    cfg: &super::DesktopConfig,
) -> (
    Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    std::collections::BTreeMap<String, String>,
) {
    use llm_runtime::model::allowlist::{self, ModelEnforcement};
    let managed = crate::desktop::settings_watch::managed_settings_snapshot_for_config(cfg).await;
    let source = managed_model_policy_source(&managed);
    match allowlist::resolve_enforcement(&source, &mut |_| {}) {
        ModelEnforcement::Active {
            allowlist,
            overrides,
        } => (Some(allowlist), overrides),
        _ => (None, std::collections::BTreeMap::new()),
    }
}

/// The MANAGED `forceLoginOrgUUID` org pin in effect for the interactive
/// Anthropic OAuth login (parity 2.1.207 H-BIN-09). Reads the managed policy
/// selected policy snapshot (the SAME source selection the permission and
/// model-default policies read) and folds `forceLoginOrgUUID` via
/// [`lingxi_core::settings::enterprise::fold_force_login_org_pin`] — the
/// highest-priority tier that sets it wins. `Unset` when no policy pins login
/// (the common case: login stays unrestricted). Read fresh at each login so a
/// mid-session managed-settings edit takes effect on the next sign-in.
pub(super) async fn managed_force_login_org_pin(
    cache: &crate::desktop::settings_watch::ManagedPolicySnapshotCache,
) -> lingxi_core::settings::enterprise::ForceLoginOrgPin {
    let Some(managed) = cache.snapshot() else {
        return lingxi_core::settings::enterprise::ForceLoginOrgPin::Invalid;
    };
    if managed.read_failed() {
        return lingxi_core::settings::enterprise::ForceLoginOrgPin::Invalid;
    }
    let selected = managed.effective_json().into_iter().collect::<Vec<_>>();
    lingxi_core::settings::enterprise::fold_force_login_org_pin(&selected)
}

/// Fold the MANAGED (`policySettings`) raw tiers into telemetry env overrides.
///
/// This preserves the enterprise provenance of OTEL-related keys without
/// mutating the process environment. Only the settings `env` object supplies
/// overrides. Unknown or non-scalar values are ignored; later managed tiers
/// override earlier ones.
pub async fn managed_otel_env_overrides(
    cfg: &super::DesktopConfig,
) -> std::collections::BTreeMap<String, String> {
    let managed = crate::desktop::settings_watch::managed_settings_snapshot_for_config(cfg).await;
    if managed.read_failed() {
        return std::collections::BTreeMap::new();
    }
    let selected = managed.effective_json().into_iter().collect::<Vec<_>>();
    fold_managed_otel_env_overrides(&selected)
}

pub(super) fn fold_managed_otel_env_overrides(
    tiers: &[String],
) -> std::collections::BTreeMap<String, String> {
    const KEYS: &[&str] = &[
        telemetry::otel::config::ENV_ENABLE_TELEMETRY,
        telemetry::otel::config::ENV_ENHANCED_TELEMETRY_BETA,
        telemetry::otel::config::ENV_FLUSH_TIMEOUT_MS,
        telemetry::otel::config::ENV_SHUTDOWN_TIMEOUT_MS,
        telemetry::otel::config::ENV_HEADERS_HELPER_DEBOUNCE_MS,
        telemetry::otel::config::ENV_DIAG_STDERR,
        telemetry::otel::config::ENV_CONTENT_MAX_LENGTH,
        telemetry::otel::config::ENV_ATTRIBUTE_VALUE_LENGTH_LIMIT,
        telemetry::otel::config::ENV_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT,
        telemetry::otel::config::ENV_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT,
        telemetry::otel::config::ENV_METRICS_EXPORTER,
        telemetry::otel::config::ENV_LOGS_EXPORTER,
        telemetry::otel::config::ENV_TRACES_EXPORTER,
        telemetry::otel::config::ENV_OTLP_ENDPOINT,
        telemetry::otel::config::ENV_OTLP_HEADERS,
        telemetry::otel::config::ENV_OTLP_PROTOCOL,
        telemetry::otel::config::ENV_OTLP_COMPRESSION,
        telemetry::otel::config::ENV_OTLP_TIMEOUT,
        telemetry::otel::config::ENV_OTLP_INSECURE,
        telemetry::otel::config::ENV_OTLP_CERTIFICATE,
        telemetry::otel::config::ENV_OTLP_CLIENT_KEY,
        telemetry::otel::config::ENV_OTLP_CLIENT_CERTIFICATE,
        telemetry::otel::config::ENV_METRIC_EXPORT_INTERVAL,
        telemetry::otel::config::ENV_LOGS_EXPORT_INTERVAL,
        telemetry::otel::config::ENV_RESOURCE_ATTRIBUTES,
        telemetry::otel::config::ENV_SERVICE_NAME,
        telemetry::otel::config::ENV_TRACES_SAMPLER,
        telemetry::otel::config::ENV_TRACES_SAMPLER_ARG,
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_METRICS_INCLUDE_SESSION_ID",
        "OTEL_METRICS_INCLUDE_VERSION",
        "OTEL_METRICS_INCLUDE_ACCOUNT_UUID",
        "OTEL_METRICS_INCLUDE_ENTRYPOINT",
        "OTEL_METRICS_INCLUDE_RESOURCE_ATTRIBUTES",
        "OTEL_LOG_USER_PROMPTS",
        "OTEL_LOG_TOOL_DETAILS",
        "OTEL_LOG_TOOL_CONTENT",
        "OTEL_LOG_ASSISTANT_RESPONSES",
        "OTEL_LOG_RAW_API_BODIES",
    ];

    fn insert_scalar_values(
        source: &serde_json::Map<String, serde_json::Value>,
        keys: &[&str],
        out: &mut std::collections::BTreeMap<String, String>,
    ) {
        for key in keys {
            let Some(value) = source.get(*key) else {
                continue;
            };
            let scalar = match value {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Bool(v) => Some(v.to_string()),
                serde_json::Value::Number(v) => Some(v.to_string()),
                _ => None,
            };
            if let Some(value) = scalar {
                out.insert((*key).to_string(), value);
            }
        }
    }

    let mut out = std::collections::BTreeMap::new();
    for raw in tiers {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw)
        else {
            continue;
        };

        if let Some(serde_json::Value::Object(env)) = map.get("env") {
            insert_scalar_values(env, KEYS, &mut out);
        }
    }
    out
}

/// Whether the live cron scheduler should run. Faithful to claude-code's
/// `isKairosCronEnabled` LOCAL kill-switch (`ScheduleCronTool/prompt.ts:34/38`):
/// the `CLAUDE_CODE_DISABLE_CRON` env override (truthy ⇒ cron OFF) "wins over"
/// the GrowthBook fleet flag. That flag defaults to `true`, so this wired-on
/// scheduler already matches the default-enabled fleet state — only the local
/// disable override was missing. (The remote GB gate itself is not portable —
/// LingXi has no GrowthBook substrate — but its default-true state is.)
pub(super) fn cron_scheduler_enabled(disable_cron_env: Option<&str>) -> bool {
    disable_cron_env.is_none_or(str::is_empty)
}

/// Expand a raw additional-working-dir entry (settings `additionalDirectories`
/// or CLI `--add-dir`) into an absolute path suitable for
/// [`tool_api::BuiltinToolContext::trusted_dirs`], mirroring the permission
/// policy's `expand_path`: `~`/`~/…` resolve against `home`, a relative path
/// resolves against `cwd`, an absolute path is taken verbatim. Lexical only —
/// `canonicalize_and_validate` still resolves symlinks/`..` on each file-tool
/// use, so the file-tool allowed set matches claude-code `FY(t)` (cwd +
/// additionalWorkingDirectories) rather than hard-blocking `--add-dir` roots.
pub(super) fn expand_trusted_dir(
    raw: &std::path::Path,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let s = raw.to_string_lossy();
    let t = s.trim();
    if t == "~" {
        home.map_or_else(|| std::path::PathBuf::from(t), std::path::Path::to_path_buf)
    } else if let Some(rest) = t.strip_prefix("~/") {
        home.map_or_else(|| std::path::PathBuf::from(t), |h| h.join(rest))
    } else {
        let p = std::path::Path::new(t);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    }
}

/// Fold the `sandbox` subsection of the settings tiers (ascending priority, last
/// write wins) into a full [`sandbox::runtime_config::SandboxRuntimeConfig`].
///
/// `ctx` carries the session/host seeds (`getClaudeTempDir()`, the settings-file
/// `deny_write` paths, the managed drop-in dir, `.lingxi/skills`, …). It is built
/// by the caller (the composition root has `lingxi_home`/`cwd`/`managed` in scope,
/// so the helper stays pure and unit-testable; tests pass a minimal seed). See
/// the `build()` call site and spec §5 for which seeds have a boot-time analog.
#[must_use]
pub(super) fn sandbox_runtime_config_from_settings_tiers(
    raw_tiers: &[&str],
    settings_dir: &std::path::Path,
    ctx: &sandbox::policy_convert::SandboxConvertContext,
) -> sandbox::runtime_config::SandboxRuntimeConfig {
    use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson, SettingsPermissions};

    let mut merged_sandbox: Option<SandboxSettingsJson> = None;
    // Accumulate the merged `permissions` across tiers (extend allow/deny/
    // additionalDirectories — these feed the filesystem allow_write / deny_read /
    // network allowed_domains derivation in `convert_settings_to_runtime_config`).
    let mut merged_perms = SettingsPermissions::default();
    let mut saw_perms = false;
    for raw in raw_tiers {
        let Some(parsed) = sandbox_settings_tier_from_raw(raw) else {
            continue;
        };
        if let Some(p) = parsed.permissions {
            saw_perms = true;
            merged_perms.allow.extend(p.allow);
            merged_perms.deny.extend(p.deny);
            merged_perms
                .additional_directories
                .extend(p.additional_directories);
        }
        if let Some(s) = parsed.sandbox {
            merged_sandbox = Some(s);
        }
    }
    sandbox::policy_convert::convert_settings_to_runtime_config(
        &SettingsJson {
            permissions: saw_perms.then_some(merged_perms),
            sandbox: merged_sandbox,
            settings_dir: Some(settings_dir.to_path_buf()),
        },
        ctx,
    )
}

/// Compute the managed-only sandbox overrides for the
/// `allowManagedDomainsOnly` / `allowManagedReadPathsOnly` enforcement. Parses
/// the MANAGED (`policySettings`) raw tiers ONLY — the per-source knowledge
/// claude-code uses via `getSettingsForSource('policySettings')`. When a flag is
/// set there, returns `Some(allowlist)` (the managed-source domains / read paths)
/// to OVERRIDE the merged config; `None` ⇒ no restriction. Threaded onto
/// [`SandboxConvertContext`] so [`convert_settings_to_runtime_config`] applies it.
pub(super) fn managed_only_sandbox_overrides(
    managed_raw_tiers: &[String],
    settings_dir: &std::path::Path,
) -> (Option<Vec<String>>, Option<Vec<String>>) {
    use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson, SettingsPermissions};
    let mut merged_sandbox: Option<SandboxSettingsJson> = None;
    let mut merged_perms = SettingsPermissions::default();
    let mut saw_perms = false;
    for raw in managed_raw_tiers {
        let Some(parsed) = sandbox_settings_tier_from_raw(raw) else {
            continue;
        };
        if let Some(p) = parsed.permissions {
            saw_perms = true;
            merged_perms.allow.extend(p.allow);
            merged_perms.deny.extend(p.deny);
            merged_perms
                .additional_directories
                .extend(p.additional_directories);
        }
        if let Some(s) = parsed.sandbox {
            merged_sandbox = Some(s);
        }
    }
    let managed = SettingsJson {
        permissions: saw_perms.then_some(merged_perms),
        sandbox: merged_sandbox,
        settings_dir: Some(settings_dir.to_path_buf()),
    };
    let domains_only = managed
        .sandbox
        .as_ref()
        .and_then(|s| s.network.as_ref())
        .is_some_and(|n| n.allow_managed_domains_only);
    let reads_only = managed
        .sandbox
        .as_ref()
        .and_then(|s| s.filesystem.as_ref())
        .is_some_and(|f| f.allow_managed_read_paths_only);
    let domains = domains_only.then(|| sandbox::policy_convert::managed_domain_allowlist(&managed));
    let reads = reads_only
        .then(|| sandbox::policy_convert::managed_read_path_allowlist(&managed, settings_dir));
    (domains, reads)
}

/// Resolve the SOURCE-RESTRICTED `allowAppleEvents` value. claude-code honors
/// this sandbox setting ONLY from user, managed/policy, or CLI `--settings`
/// (`flagSettings`) sources — project & local `.lingxi/settings*.json` are
/// IGNORED (sandbox-adapter.ts 2.1.207 @223928133:
/// `allowAppleEvents:[...managedSources, wr("flagSettings"), userSettings]
/// .map(z => z?.sandbox?.allowAppleEvents).find(z => z !== undefined)`).
/// First-defined wins in order managed/policy → flag → user. CC pre-folds the
/// file-based managed tiers into ONE object with a DEEP merge
/// (`loadManagedFileSettings` → `Fie(r, next, Bpe)`: base then drop-ins sorted,
/// later scalars override earlier but omitted fields are preserved), so
/// `sandbox.allowAppleEvents` is resolved PER-FIELD last-defined across the
/// tiers — a later drop-in that carries only a partial `sandbox` block (e.g.
/// `{"sandbox":{"network":…}}`) must NOT clobber an earlier tier's value.
/// Returns `None` when no honored source set it — matching CC's
/// `.find(...) === undefined ⇒ manager reads `false``. Threaded onto
/// [`SandboxConvertContext::allow_apple_events_override`].
pub(super) fn apple_events_override(
    managed_raw_tiers: &[String],
    flag_settings_raw: Option<&str>,
    user_settings_raw: Option<&str>,
) -> Option<bool> {
    // Managed/policy file tiers are deep-merged, so resolve `allowAppleEvents`
    // per-field last-defined (later drop-ins win, `None` tiers don't clobber).
    let mut merged_managed: Option<bool> = None;
    for raw in managed_raw_tiers {
        if let Some(parsed) = sandbox_settings_tier_from_raw(raw) {
            if let Some(v) = parsed.sandbox.and_then(|s| s.allow_apple_events) {
                merged_managed = Some(v);
            }
        }
    }
    if let Some(v) = merged_managed {
        return Some(v);
    }
    if let Some(v) = flag_settings_raw
        .and_then(sandbox_settings_tier_from_raw)
        .and_then(|s| s.sandbox)
        .and_then(|s| s.allow_apple_events)
    {
        return Some(v);
    }
    user_settings_raw
        .and_then(sandbox_settings_tier_from_raw)
        .and_then(|s| s.sandbox)
        .and_then(|s| s.allow_apple_events)
}

/// Resolve the SOURCE-RESTRICTED `sandbox.network.strictAllowlist` (2.1.219).
///
/// Same tier rule as [`apple_events_override`]: honored only from managed /
/// policy, CLI `--settings`, and user settings.
/// Project `.lingxi/settings.json` and `settings.local.json` are IGNORED — the
/// oracle's own description says so outright.
///
/// `None` ⇒ no honored source set it, and the converter clears the flag rather
/// than inheriting whatever a non-honored tier merged in.
pub(super) fn strict_allowlist_override(
    managed_raw_tiers: &[String],
    flag_settings_raw: Option<&str>,
    user_settings_raw: Option<&str>,
) -> Option<bool> {
    // Managed/policy file tiers are deep-merged; resolve per-field last-defined
    // so a partial drop-in cannot clobber an earlier tier's value.
    let mut merged_managed: Option<bool> = None;
    for raw in managed_raw_tiers {
        if let Some(v) = strict_allowlist_setting(raw) {
            merged_managed = Some(v);
        }
    }
    if let Some(v) = merged_managed {
        return Some(v);
    }
    if let Some(v) = flag_settings_raw.and_then(strict_allowlist_setting) {
        return Some(v);
    }
    user_settings_raw.and_then(strict_allowlist_setting)
}

/// Read the source-level value without collapsing an explicit `false` into the
/// serde default. `NetworkRestrictionConfig::strict_allowlist` is a runtime
/// `bool`, so deserializing the whole settings object cannot distinguish
/// `{"strictAllowlist": false}` from an omitted field. Source precedence needs
/// that distinction: a managed `false` must override a lower-tier user `true`.
pub(super) fn strict_allowlist_setting(raw: &str) -> Option<bool> {
    let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(raw).ok()?;
    let strict_allowlist = source
        .subprojection("/sandbox/network/strictAllowlist")
        .ok()?;
    serde_json::from_str::<bool>(&strict_allowlist.to_json_string().ok()?).ok()
}

/// Resolve the SOURCE-RESTRICTED `sandbox.ripgrep` (2.1.232).
///
/// Same tier rule as [`apple_events_override`]: honored only from managed /
/// policy, CLI `--settings`, and user settings.
/// Project `.lingxi/settings.json` and `settings.local.json` are IGNORED.
///
/// Managed file tiers are deep-merged per-field, so a later partial `ripgrep`
/// block keeps earlier fields unless it redefines them. An honored empty object
/// still counts as "set to defaults", so it overrides a lower-tier user value.
pub(super) fn ripgrep_override(
    managed_raw_tiers: &[String],
    flag_settings_raw: Option<&str>,
    user_settings_raw: Option<&str>,
) -> Option<sandbox::runtime_config::RipgrepConfig> {
    let mut merged_managed: Option<serde_json::Map<String, serde_json::Value>> = None;
    for raw in managed_raw_tiers {
        if let Some(obj) = ripgrep_object(raw) {
            merged_managed
                .get_or_insert_with(serde_json::Map::new)
                .extend(obj);
        }
    }
    if let Some(obj) = merged_managed {
        return ripgrep_config_from_object(&obj);
    }
    if let Some(cfg) = flag_settings_raw
        .and_then(|raw| ripgrep_object(raw).and_then(|obj| ripgrep_config_from_object(&obj)))
    {
        return Some(cfg);
    }
    user_settings_raw
        .and_then(|raw| ripgrep_object(raw).and_then(|obj| ripgrep_config_from_object(&obj)))
}

pub(super) fn ripgrep_object(raw: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let source = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(raw).ok()?;
    let ripgrep = source.subprojection("/sandbox/ripgrep").ok()?;
    let json = ripgrep.to_json_string().ok()?;
    serde_json::from_str::<serde_json::Value>(&json)
        .ok()?
        .as_object()
        .cloned()
}

pub(super) fn ripgrep_config_from_object(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<sandbox::runtime_config::RipgrepConfig> {
    let command = match obj.get("command") {
        Some(serde_json::Value::String(v)) => v.clone(),
        Some(_) => return None,
        None => String::new(),
    };
    let args = match obj.get("args") {
        Some(serde_json::Value::Array(values)) => {
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                let Some(s) = value.as_str() else {
                    return None;
                };
                out.push(s.to_string());
            }
            out
        }
        Some(_) => return None,
        None => Vec::new(),
    };
    let argv0 = match obj.get("argv0") {
        Some(serde_json::Value::String(v)) => Some(v.clone()),
        Some(serde_json::Value::Null) => None,
        Some(_) => return None,
        None => None,
    };
    Some(sandbox::runtime_config::RipgrepConfig {
        command,
        args,
        argv0,
    })
}

/// claude-code `getClaudeTempDir()` + `getClaudeTempDirName()` analog (Shell.ts:307),
/// identical to the canonical private `lingxi_temp_dir()` in `tool-shell`'s
/// `prompt.rs`: `baseTmpDir = LINGXI_TMPDIR || (windows ? tmpdir() : "/tmp")`,
/// realpath-resolved, name `claude` on Windows else `claude-{uid}`, joined with a
/// trailing separator. Seeded into the sandbox `allow_write` so the shell's
/// cwd-tracking file stays writable.
pub(super) fn lingxi_temp_dir() -> String {
    let base: std::path::PathBuf = std::env::var_os("LINGXI_TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                std::env::temp_dir()
            } else {
                std::path::PathBuf::from("/tmp")
            }
        });
    let resolved_base = std::fs::canonicalize(&base).unwrap_or(base);
    let name = if cfg!(target_os = "windows") {
        "claude".to_string()
    } else {
        format!("claude-{}", current_uid())
    };
    let joined = resolved_base.join(name);
    let mut s = joined.to_string_lossy().into_owned();
    s.push(std::path::MAIN_SEPARATOR);
    s
}

/// The real (not effective) UID, mirroring TS `process.getuid?.() ?? 0`.
/// `nix::unistd::getuid()` is a SAFE wrapper, so this crate keeps its
/// `#![forbid(unsafe_code)]` (same pattern as `apps/cli/src/bypass_env.rs`).
#[cfg(unix)]
pub(super) fn current_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

#[cfg(not(unix))]
pub(super) fn current_uid() -> u32 {
    0
}

/// Port of claude-code `isPlatformInEnabledList()` (sandbox-adapter.ts:505): is
/// the current platform in `sandbox.enabledPlatforms`? `None` (unset) ⇒ all
/// supported platforms allowed (`true`); empty list ⇒ none allowed (`false`,
/// which is how an operator turns the sandbox off everywhere); otherwise the list
/// must contain the current platform.
#[must_use]
pub fn platform_in_enabled_list(
    enabled: Option<&[sandbox::runtime_config::Platform]>,
    current: sandbox::runtime_config::Platform,
) -> bool {
    match enabled {
        None => true,
        Some([]) => false,
        Some(list) => list.contains(&current),
    }
}

#[cfg(test)]
mod sandbox_utf16_execution_tests {
    use super::{sandbox_runtime_config_from_settings_tiers, strict_allowlist_setting};
    use std::path::Path;

    #[test]
    fn real_project_settings_reach_runner_with_exact_ignore_violation_units() {
        use lingxi_core::settings::{
            FileLayerScope, LoadInputs, Settings, SettingsJson, SupplementalLayers,
        };
        use std::collections::BTreeMap;

        let project = tempfile::tempdir().expect("temporary project root");
        let settings_dir = project.path().join(branding::DOT_DIR);
        std::fs::create_dir_all(&settings_dir).expect("project settings directory");
        let settings_path = settings_dir.join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"model":"sibling-model","outputStyle":"\ud800","permissions":{"allow":["Edit(/tmp/permission-write)"]},"sandbox":{"enabled":true,"filesystem":{"allowWrite":["/tmp/path-\ud800"]},"ignoreViolations":{"filesystem":["valid-pattern","pattern-\ud800"],"network":["host-denied"]}}}"#,
        )
        .expect("write project settings");

        let env = BTreeMap::new();
        let scope = FileLayerScope {
            include_user: false,
            include_project: true,
            include_local: true,
        };
        let typed = Settings::load_with_layers_from_user_path(
            LoadInputs {
                env: &env,
                project_dir: project.path(),
                defaults: SettingsJson::default(),
            },
            scope,
            SupplementalLayers::default(),
            None,
        )
        .expect("load the actual project settings tier");
        assert_eq!(typed.settings.model.as_deref(), Some("sibling-model"));
        assert_eq!(
            typed
                .settings
                .output_style
                .as_ref()
                .and_then(|style| style.string_units("")),
            Some(vec![0xd800])
        );

        let raw = std::fs::read_to_string(&settings_path).expect("read source settings");
        let engine = sandbox_runtime_config_from_settings_tiers(
            &[raw.as_str()],
            project.path(),
            &sandbox::policy_convert::SandboxConvertContext::default(),
        );
        assert!(engine.enabled);
        assert!(engine
            .filesystem
            .allow_write
            .iter()
            .any(|path| path == "/tmp/path-\u{fffd}"));
        assert!(engine
            .filesystem
            .allow_write
            .iter()
            .any(|path| path.contains("permission-write")));

        let runner_config = super::super::sandbox_runner::to_runtime_config(&engine);
        let typed_ignore = runner_config
            .ignore_violations
            .as_ref()
            .expect("representable ignoreViolations siblings reach execution");
        assert_eq!(
            typed_ignore.get("filesystem"),
            Some(&vec!["valid-pattern".to_owned()])
        );
        assert_eq!(
            typed_ignore.get("network"),
            Some(&vec!["host-denied".to_owned()])
        );

        let exact_ignore = runner_config
            .ignore_violations_utf16
            .expect("the live runner retains the source projection");
        let filesystem_key = "filesystem".encode_utf16().collect::<Vec<u16>>();
        assert_eq!(
            exact_ignore
                .entries
                .iter()
                .find(|(key, _)| key == &filesystem_key)
                .map(|(_, patterns)| patterns.clone()),
            Some(vec![
                "valid-pattern".encode_utf16().collect::<Vec<_>>(),
                "pattern-"
                    .encode_utf16()
                    .chain([0xd800])
                    .collect::<Vec<_>>(),
            ])
        );
    }

    #[test]
    fn filesystem_surrogates_do_not_drop_sandbox_or_permission_settings() {
        let raw = r#"{
            "permissions": {
                "allow": ["Edit(/tmp/permission-write)"],
                "deny": ["Read(/private/permission-deny)"],
                "additionalDirectories": ["/tmp/extra-write"]
            },
            "sandbox": {
                "enabled": true,
                "filesystem": {
                    "allowWrite": ["/tmp/path-\ud800"],
                    "denyRead": ["/private/path-\udfff"]
                },
                "credentials": {"files": [{"path": "/private/credential-\ud800"}]},
                "ignoreViolations": {"filesystem": ["/tmp/ignored", "inert-\ud800-pattern"], "network": ["host-denied"]}
            }
        }"#;
        let cfg = sandbox_runtime_config_from_settings_tiers(
            &[raw],
            Path::new("/tmp"),
            &sandbox::policy_convert::SandboxConvertContext::default(),
        );

        assert!(
            cfg.enabled,
            "sandbox.enabled must survive path normalization"
        );
        assert!(cfg
            .filesystem
            .allow_write
            .iter()
            .any(|path| path == "/tmp/path-\u{fffd}"));
        assert!(cfg
            .filesystem
            .allow_write
            .iter()
            .any(|path| path == "/tmp/extra-write"));
        assert!(cfg
            .filesystem
            .allow_write
            .iter()
            .any(|path| path.contains("permission-write")));
        assert!(cfg
            .filesystem
            .deny_read
            .iter()
            .any(|path| path == "/private/path-\u{fffd}"));
        assert!(cfg
            .filesystem
            .deny_read
            .iter()
            .any(|path| path.contains("permission-deny")));
        assert!(cfg
            .filesystem
            .deny_read
            .iter()
            .any(|path| path == "/private/credential-\u{fffd}"));
        let filesystem_violations = cfg
            .ignore_violations
            .get("filesystem")
            .expect("filesystem ignoreViolations entry");
        assert_eq!(filesystem_violations.len(), 1);
        assert_eq!(filesystem_violations[0], "/tmp/ignored");
        assert_eq!(
            cfg.ignore_violations.get("network"),
            Some(&vec!["host-denied".to_owned()]),
            "a pattern outside Rust String must not discard valid ignoreViolations siblings"
        );
        let raw_ignore_violations = cfg
            .ignore_violations_source
            .as_ref()
            .expect("the source-preserving execution sidecar");
        let expected_pattern = "inert-"
            .encode_utf16()
            .chain([0xd800])
            .chain("-pattern".encode_utf16())
            .collect::<Vec<_>>();
        assert_eq!(
            raw_ignore_violations.string_units("/filesystem/1"),
            Some(expected_pattern),
            "the typed map and execution sidecar keep distinct source/Unicode views"
        );
    }

    #[test]
    fn source_restricted_scalar_parser_ignores_unrelated_surrogate_paths() {
        assert_eq!(
            strict_allowlist_setting(
                r#"{"sandbox":{"network":{"strictAllowlist":true},"filesystem":{"allowWrite":["/tmp/\ud800"]}}}"#
            ),
            Some(true)
        );
    }
}

#[cfg(test)]
mod model_source_projection_tests {
    use super::model_source_from_effective_settings;
    use lingxi_core::settings::{
        raw_layer_stack::SettingsRawLayer, tracer::Source, EffectiveSettings, SettingsJson,
    };
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    fn effective_with_model_sources(
        sources: &[Source],
        effective_model_json: &str,
    ) -> EffectiveSettings {
        let projection = Utf16JsonProjection::parse(effective_model_json)
            .expect("valid source-preserving settings JSON");
        EffectiveSettings {
            settings: SettingsJson::default(),
            trace: Default::default(),
            effort_layers: Vec::new(),
            fusion_layers: Vec::new(),
            source_projection: Some(projection.clone()),
            source_layers: sources
                .iter()
                .copied()
                .map(|source| SettingsRawLayer {
                    source,
                    path: None,
                    projection: projection.clone(),
                })
                .collect(),
            typed_view_unrepresented_fields: vec!["/model".into()],
            managed_typed_view_is_partial: true,
            source_incomplete: false,
            source_message: None,
            source_diagnostics: None,
        }
    }

    #[test]
    fn effective_model_source_retains_admitted_nonmanaged_surrogate() {
        for source in [Source::User, Source::Project, Source::Local, Source::Cli] {
            let effective = effective_with_model_sources(&[source], r#"{"model":"claude-\ud800"}"#);
            let model = model_source_from_effective_settings(&effective)
                .expect("an admitted non-managed model remains source-authoritative");
            assert_eq!(
                model.string_units(""),
                Some("claude-".encode_utf16().chain([0xd800]).collect())
            );
        }
    }

    #[test]
    fn effective_model_source_preserves_whitespace_and_prunes_only_empty() {
        let whitespace = effective_with_model_sources(&[Source::User], r#"{"model":" \t "}"#);
        let source = model_source_from_effective_settings(&whitespace)
            .expect("Native treats a nonempty whitespace model as truthy");
        assert_eq!(source.string_units(""), Some(vec![0x20, 0x09, 0x20]));

        let empty = effective_with_model_sources(&[Source::User], r#"{"model":""}"#);
        assert!(model_source_from_effective_settings(&empty).is_none());
    }

    #[test]
    fn effective_model_source_uses_highest_contributing_layer() {
        let effective = effective_with_model_sources(
            &[Source::User, Source::Project],
            r#"{"model":"project-model"}"#,
        );
        assert_eq!(
            effective.source_field_contributors("model").last(),
            Some(&Source::Project)
        );
        assert_eq!(
            model_source_from_effective_settings(&effective)
                .and_then(|model| model.string_at("").map(str::to_owned))
                .as_deref(),
            Some("project-model")
        );
    }
}
