//! What a Local App workspace looks like on disk, and which of its files the host owns.
//!
//! The permission crate's workspace leases are generic: a lease covers one workspace, and the registry asks a
//! [`WorkspaceProfile`] what the workspace is. This is the profile of the Local App product: the
//! `apps/<id>/workspace` and `local-app-<id>` roots, the guest's `/workspace/...` spellings, the build files the
//! host generates and re-writes (so a model must not), and the host operations (`LocalApp*` tools) a build lease
//! may authorize for its own app.

use permission::{WorkspacePermissionLeaseRegistry, WorkspaceProfile};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The Local App workspace profile.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalAppWorkspaceProfile;

impl LocalAppWorkspaceProfile {
    /// A lease registry that holds workspaces to this profile.
    #[must_use]
    pub fn registry() -> Arc<WorkspacePermissionLeaseRegistry> {
        WorkspacePermissionLeaseRegistry::new(Arc::new(Self))
    }
}

impl WorkspaceProfile for LocalAppWorkspaceProfile {
    fn workspace_id(&self, root: &Path) -> Option<String> {
        local_app_id(root)
    }

    fn root_matches_id(&self, root: &Path, id: &str) -> bool {
        workspace_root_matches_app_id(root, id)
    }

    // AppService's persisted invariant is exactly `apps/<id>/workspace`.
    fn workspace_root(&self, data_root: &Path, id: &str) -> PathBuf {
        data_root.join("apps").join(id).join("workspace")
    }

    fn is_exact_root(&self, root: &Path, id: &str) -> bool {
        is_exact_local_app_root(root, id)
    }

    // Mobile Linux prompts use `/workspace/<id>`; older builds used `/workspace/local-app-<id>`. The
    // order is the order the aliases are tried and translated in.
    fn guest_prefixes(&self, id: &str) -> Vec<String> {
        vec![format!("/workspace/local-app-{id}"), format!("/workspace/{id}")]
    }

    // The local-app host operations are BUILTIN tools (`LocalApp*`), not an MCP server — see
    // `mobile::local_apps_tools` for why they moved. The `app_id` equality check is what keeps a lease for one
    // app from authorizing an operation aimed at a sibling.
    fn tool_decision(&self, tool_name: &str, input: &serde_json::Value, id: &str) -> Option<bool> {
        if !tool_name.starts_with("LocalApp") {
            return None;
        }
        let allowed = matches!(
            tool_name,
            "LocalAppBuild"
                | "LocalAppLogs"
                | "LocalAppRuntime"
                | "LocalAppManifest"
                | "LocalAppQueryData"
        );
        Some(allowed && input.get("app_id").and_then(serde_json::Value::as_str) == Some(id))
    }

    fn is_host_owned(&self, relative: &Path) -> bool {
        // Replacing the workspace root or the `lib/` directory would also replace host-managed descendants such
        // as the bridge and platform adapter.
        relative.as_os_str().is_empty() || relative == Path::new("lib") || host_owned_relative(relative)
    }
}

fn local_app_id(root: &Path) -> Option<String> {
    if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
        if let Some(app_id) = name.strip_prefix("local-app-") {
            return (!app_id.is_empty()).then(|| app_id.to_owned());
        }
    }
    let app_root = root.parent()?;
    let apps_root = app_root.parent()?;
    if root.file_name().and_then(|name| name.to_str()) != Some("workspace")
        || apps_root.file_name().and_then(|name| name.to_str()) != Some("apps")
    {
        return None;
    }
    app_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn workspace_root_matches_app_id(root: &Path, app_id: &str) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if let Some(mounted_id) = name.strip_prefix("local-app-") {
        return mounted_id == app_id;
    }
    if name != "workspace" {
        return true;
    }
    let Some(app_root) = root.parent() else {
        return true;
    };
    let Some(apps_root) = app_root.parent() else {
        return true;
    };
    if apps_root.file_name().and_then(|name| name.to_str()) != Some("apps") {
        return true;
    }
    app_root.file_name().and_then(|name| name.to_str()) == Some(app_id)
}

fn is_exact_local_app_root(root: &Path, app_id: &str) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name == format!("local-app-{app_id}") {
        return true;
    }
    if name != "workspace" {
        return false;
    }
    let Some(app_root) = root.parent() else {
        return false;
    };
    let Some(apps_root) = app_root.parent() else {
        return false;
    };
    apps_root.file_name().and_then(|name| name.to_str()) == Some("apps")
        && app_root.file_name().and_then(|name| name.to_str()) == Some(app_id)
}

fn host_owned_relative(relative: &Path) -> bool {
    if relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name == ".lingxi" || name == ".lingxi-build-state"
        )
    }) {
        return true;
    }
    // Any BUILD-CONFIG file the toolchain AUTO-DISCOVERS and then executes as
    // Node code. Matched by shape rather than an exact list because the search
    // space belongs to the tool, not to us: Vite walks `DEFAULT_CONFIG_FILES`
    // and PostCSS walks lilconfig's `getDefaultSearchPlaces`, and both have
    // gained spellings across versions — an exact list rots into a hole on the
    // next bump.
    //
    // Bounded on BOTH axes, or it would swallow ordinary source:
    //   - LOCATION: only the workspace root and `.config/`, the only two
    //     directories either tool searches. `src/vite.config.helper.js` is
    //     app-owned and stays writable.
    //   - EXTENSION: only executable/config extensions, so a file merely
    //     BEGINNING with `vite.config.` is not swallowed either.
    {
        let mut components = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            });
        let (dir, file) = match (components.next(), components.next(), components.next()) {
            (Some(file), None, _) => (None, Some(file)),
            (Some(dir), Some(file), None) => (Some(dir), Some(file)),
            _ => (None, None),
        };
        if matches!(dir, None | Some(".config")) {
            if let Some(name) = file {
                let stem = name.strip_prefix('.').unwrap_or(name);
                const CONFIG_EXTENSIONS: &[&str] = &[
                    "js", "cjs", "mjs", "ts", "cts", "mts", "json", "yaml", "yml",
                ];
                let is_config_of = |tool: &str| {
                    let rc = format!("{tool}rc");
                    if stem == rc {
                        return true;
                    }
                    for prefix in [format!("{rc}."), format!("{tool}.config.")] {
                        if let Some(ext) = stem.strip_prefix(&prefix) {
                            if CONFIG_EXTENSIONS.contains(&ext) {
                                return true;
                            }
                        }
                    }
                    false
                };
                if is_config_of("vite") || is_config_of("postcss") || is_config_of("tailwind") {
                    return true;
                }
            }
        }
    }
    relative.file_name().is_some_and(|name| name == "LINGXI.md")
        || relative.starts_with("node_modules")
        || matches!(
            relative.to_str(),
            Some(
                "index.html"
                    // EVERY Vite config spelling, not just the one the template
                    // ships. Vite self-resolves from `DEFAULT_CONFIG_FILES` and
                    // `vite.config.js` sorts FIRST, ahead of the `.mjs` the host
                    // writes, so creating any other spelling shadows the host
                    // config — and a Vite config is executed Node code. `Edit`
                    // creates files on a nonexistent path, so the workspace's
                    // own `Edit(./**)` grant reaches this with no new permission.
                    | "vite.config.js"
                    | "vite.config.mjs"
                    | "vite.config.ts"
                    | "vite.config.cjs"
                    | "vite.config.mts"
                    | "vite.config.cts"
                    | "package.json"
                    | "pnpm-lock.yaml"
                    // Declared host-managed by the generated LINGXI.md. These
                    // were enforced nowhere, so `Edit(./**)` overwrote them and
                    // `restore_host_managed_files` reverted the edit with only
                    // a `tracing::warn!` — the model loops against a file it
                    // cannot change and is never told why.
                    | ".gitignore"
                    | "jsconfig.json"
                    | "lib/lingxi-provider.jsx"
                    | "lib/frame-loop.js"
                    | "lib/phaser-runtime.js"
                    | "lib/babylon-runtime.js"
                    | "styles/foundation.css"
                    // Copied into the install staging tree by
                    // `prepare_dependency_staging`, and the install runs with
                    // the network ENABLED.
                    | "pnpm-workspace.yaml"
                    | "lib/device-context.js"
                    | "lib/lingxi-bridge.js"
                    | "lib/platform-adapter.js"
            )
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use permission::{FsRoots, WorkspaceLeaseInfo};
    use tempfile::tempdir;

    fn registry() -> Arc<WorkspacePermissionLeaseRegistry> {
        WorkspacePermissionLeaseRegistry::new(Arc::new(LocalAppWorkspaceProfile))
    }

    fn roots(root: &Path) -> FsRoots {
        FsRoots {
            cwd: root.to_path_buf(),
            home: None,
            lingxi_home: root.to_path_buf(),
        }
    }

    #[test]
    fn lease_allows_workspace_file_and_rejects_outside() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app", root.clone());
        let fs = roots(&root);
        assert!(registry.allows(
            "Write",
            &serde_json::json!({"file_path":"src/App.jsx"}),
            &fs
        ));
        assert!(!registry.allows("Write", &serde_json::json!({"file_path":"../secret"}), &fs));
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":".lingxi/source-policy.json"}),
            &fs
        ));
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":"lib/lingxi-bridge.js"}),
            &fs
        ));
        assert!(!registry.allows("Write", &serde_json::json!({"file_path":"LINGXI.md"}), &fs));
        for path in [
            ".",
            "lib",
            "index.html",
            "vite.config.mjs",
            "package.json",
            "pnpm-lock.yaml",
            "lib/device-context.js",
            "lib/platform-adapter.js",
            "node_modules/vite/bin/vite.js",
        ] {
            assert!(
                !registry.allows("Write", &serde_json::json!({"file_path": path}), &fs),
                "host-managed build infrastructure must not be writable: {path}"
            );
        }
        assert!(registry.allows(
            "Write",
            &serde_json::json!({"file_path":"app/main.jsx"}),
            &fs
        ));
    }

    #[test]
    fn host_owned_metadata_is_readable_but_never_writable() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(root.join(".lingxi")).unwrap();
        std::fs::write(root.join(".lingxi/settings.local.json"), b"{}\n").unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app", root.clone());
        let fs = roots(&root);
        let settings = serde_json::json!({"file_path":".lingxi/settings.local.json"});

        assert!(registry.allows("Read", &settings, &fs));
        assert!(!registry.allows("Write", &settings, &fs));
        assert!(registry.denies_host_owned("Write", &settings, &fs));
        assert!(!registry.denies_host_owned("Read", &settings, &fs));
        assert!(registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command":"echo x > .lingxi/settings.local.json"}),
            &fs
        ));
        assert!(registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command":"rm -rf .lingxi"}),
            &fs
        ));
        for command in ["rm -rf .", "rm -rf lib", "mv lib lib.bak", "rm -rf *"] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "destructive parent or glob must not bypass host ownership: {command}"
            );
        }
        assert!(registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command":"printf x > package.json"}),
            &fs
        ));
        for command in ["rm -rf app/old.jsx", "mkdir components"] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "shell mutation must use structured file tools: {command}"
            );
        }
        for command in [
            "TARGET=. rm -rf \"$TARGET\"",
            "env TARGET=. rm -rf \"$TARGET\"",
            "rm -rf {package.json,app}",
            "TARGET=package.json; echo x > \"$TARGET\"",
            "python3 -c 'open(\"package.json\", \"w\").write(\"{}\")'",
            "npm install",
            "find . -fprint package.json",
            "find . -fprintf package.json x",
            "find . -files0-from list",
        ] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "shell expansion must not bypass the structured mutation boundary: {command}"
            );
        }
    }

    #[test]
    fn drop_revokes_lease() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let fs = roots(&root);
        let lease = registry.begin_unchecked("app", root);
        assert!(!registry.active().is_empty());
        drop(lease);
        assert!(registry.active().is_empty());
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":"src/App.jsx"}),
            &fs
        ));
    }

    #[test]
    fn shell_lease_preserves_network_and_interpreter_approval() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"echo ok > out.txt"}),
            &fs
        ));
        assert!(registry.allows(
            "Bash",
            &serde_json::json!({"command":"cat src/App.jsx"}),
            &fs
        ));
        assert!(!registry.allows("Bash", &serde_json::json!({"command":"npm install"}), &fs));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"curl https://example.com"}),
            &fs
        ));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"node -e 'process.exit(0)'"}),
            &fs
        ));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"echo $(curl https://example.com)"}),
            &fs
        ));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"echo `curl https://example.com`"}),
            &fs
        ));
        for command in [
            "find . -fprint out.txt",
            "find . -fprintf out.txt x",
            "find . -files0-from list",
        ] {
            assert!(
                !registry.allows("Bash", &serde_json::json!({"command": command}), &fs),
                "side-effecting or path-indirect find must not be lease-authorized: {command}"
            );
        }
    }

    /// The id grammar exists twice on purpose: the service's contracts crate owns it and `tasks` keeps a
    /// copy so its dependents do not pull the service in. This is the one place that sees both, so it runs
    /// one corpus through the original and through the scope constructors (which use the copy).
    #[test]
    fn the_task_scope_accepts_exactly_the_ids_the_service_accepts() {
        let max_len = "a".repeat(54);
        let too_long = "a".repeat(55);
        let corpus = [
            "a", "0", "abc-123", "9-", max_len.as_str(), "", "-leading-dash", "Upper", "under_score",
            "spa ce", "..", "../evil", "a/b", "a\\b", "a.b", "über", too_long.as_str(),
        ];
        let mut accepted = 0;
        for id in corpus {
            let expected = local_app_contracts::ids::is_valid_app_id(id);
            for made in [
                tasks::scope::ManagedWorkflowScope::for_build(id),
                tasks::scope::ManagedWorkflowScope::for_use_test(id),
                tasks::scope::ManagedWorkflowScope::for_mcp_authoring(id),
            ] {
                assert_eq!(made.is_ok(), expected, "the grammars disagree on {id:?}");
            }
            accepted += usize::from(expected);
        }
        assert_eq!(accepted, 5, "vacuity: the corpus must contain both valid and invalid ids");
    }

    /// The lease root is derived from the requested app, so a workflow cannot borrow the session cwd or
    /// another app's workspace.
    #[test]
    fn the_lease_root_is_derived_from_the_requested_app() {
        let registry = registry();
        let data_root = PathBuf::from("/profile");
        assert_eq!(
            registry.workspace_root_for(&data_root, "app-a"),
            PathBuf::from("/profile/apps/app-a/workspace")
        );
        assert_eq!(
            registry.workspace_root_for(&data_root, "app-b"),
            PathBuf::from("/profile/apps/app-b/workspace")
        );
        assert_ne!(
            registry.workspace_root_for(&data_root, "app-a"),
            registry.workspace_root_for(&data_root, "app-b")
        );
    }

    #[test]
    fn production_lease_translates_only_the_bound_guest_workspace_alias() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let lease = registry.begin_bound("app-a", &root).unwrap();
        let fs = roots(&root);

        assert!(registry.allows_for_token(
            Some(lease.token()),
            "Write",
            &serde_json::json!({"file_path":"/workspace/local-app-app-a/src/App.jsx"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({
                "command":"echo ok > /workspace/local-app-app-a/src/out.txt"
            }),
            &fs
        ));
        assert!(registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({
                "command":"cat /workspace/local-app-app-a/src/out.txt"
            }),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Write",
            &serde_json::json!({"file_path":"/workspace/local-app-app-b/src/App.jsx"}),
            &fs
        ));
    }

    /// The host-owned HARD DENY is reachable only through a forwarded token.
    ///
    /// `denies_host_owned_for_token` had no test at all, and in production the
    /// token never arrived: `DeferredToolInvoker` inherited the trait's
    /// delegating default and dropped it, so the `let Some(token) = token else
    /// { return false }` early return disabled this guard entirely. Forwarding
    /// the token (engine-mobile `workflow_support.rs`, engine-desktop `lib.rs`)
    /// turns it on for the first time — pin both polarities so the activation
    /// is deliberate and a future pass-through wrapper cannot silently undo it.
    #[test]
    fn production_host_owned_deny_requires_the_forwarded_token() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let lease = registry.begin_bound("app-a", &root).unwrap();
        let fs = roots(&root);

        // A host-owned file is denied outright when the token is present.
        assert!(
            registry.denies_host_owned_for_token(
                Some(lease.token()),
                "Write",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/package.json"}),
                &fs
            ),
            "a leased workflow must not rewrite host-owned package.json"
        );
        assert!(
            registry.denies_host_owned_for_token(
                Some(lease.token()),
                "Edit",
                &serde_json::json!({
                    "file_path":"/workspace/local-app-app-a/.lingxi-build-state/dependency-update-recovery.json"
                }),
                &fs
            ),
            "a leased workflow must not forge dependency recovery state"
        );

        // Generated source under the same lease stays writable.
        assert!(
            !registry.denies_host_owned_for_token(
                Some(lease.token()),
                "Write",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/src/App.jsx"}),
                &fs
            ),
            "generated source is not host-owned"
        );

        // DOCUMENTS the `let Some(token) = token else { return false }` arm —
        // it is NOT a regression anchor. This assertion stays green precisely
        // WHEN the bug recurs, so it cannot catch a wrapper that drops the
        // token again. The real guards are the forwarding tests in
        // engine-mobile `workflow_support.rs` and engine-desktop `lib.rs`,
        // which assert the token reaches the inner invoker.
        assert!(
            !registry.denies_host_owned_for_token(
                None,
                "Write",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/package.json"}),
                &fs
            ),
            "a dropped token silently disables the host-owned deny"
        );
    }

    #[test]
    fn local_app_shell_path_escape_is_hard_denied_before_global_allow_rules() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let fs = roots(&root);

        assert!(
            !registry().escapes_workspace(
                "Bash",
                &serde_json::json!({"command":"cat src/App.jsx"}),
                &fs
            )
        );
        assert!(
            registry().escapes_workspace(
                "Bash",
                &serde_json::json!({"command":"cat /etc/passwd"}),
                &fs
            )
        );
        assert!(
            registry().escapes_workspace(
                "Bash",
                &serde_json::json!({"command":"cd /tmp && cat src/App.jsx"}),
                &fs
            )
        );
    }

    #[test]
    fn local_app_tool_allowlist_is_app_scoped() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app-a", root.clone());
        let fs = roots(&root);
        assert!(registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-a"}), &fs));
        assert!(!registry.allows(
            "LocalAppDeleteApp",
            &serde_json::json!({"app_id":"app-a"}),
            &fs
        ));
        assert!(!registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-b"}), &fs));
    }

    #[test]
    fn local_app_layout_binds_mcp_lease_to_workspace_app_id() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app-b", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-b"}), &fs));
        assert!(!registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-a"}), &fs));
    }

    #[test]
    fn guest_mount_root_binds_lease_to_exact_app_id() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("local-app-app-a");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let lease = registry.begin_unchecked("app-b", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "LocalAppBuild",
            &serde_json::json!({"app_id":"app-b"}),
            &fs
        ));
        drop(lease);
        let lease = registry.begin_unchecked("app-a", root.clone());
        assert!(registry.allows_for_token(
            Some(lease.token()),
            "LocalAppBuild",
            &serde_json::json!({"app_id":"app-a"}),
            &fs
        ));
    }

    #[test]
    fn production_local_app_lease_rejects_generic_or_mismatched_roots() {
        let dir = tempdir().unwrap();
        let registry = registry();
        let generic = dir.path().join("workspace");
        std::fs::create_dir_all(&generic).unwrap();
        assert!(registry.begin_bound("app-a", generic).is_err());

        let wrong = dir.path().join("apps/app-b/workspace");
        std::fs::create_dir_all(&wrong).unwrap();
        assert!(registry.begin_bound("app-a", wrong).is_err());
    }

    #[test]
    fn production_local_app_lease_accepts_canonical_host_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let lease = registry.begin_bound("app-a", &root).unwrap();
        assert_eq!(
            registry.active(),
            vec![WorkspaceLeaseInfo {
                workspace_id: "app-a".into(),
                root: std::fs::canonicalize(root).unwrap(),
            }]
        );
        drop(lease);
        assert!(registry.active().is_empty());
    }

    /// The lease authorizes the local-app host operations under their BUILTIN
    /// names. They used to be `mcp__local_apps__*`; that spelling gave them
    /// third-party-MCP permission semantics they were never meant to have, so
    /// they were moved to ordinary builtin tools.
    ///
    /// The `app_id` equality check is the scoping that makes this safe and
    /// must survive the rename: a lease for app A must not authorize an
    /// operation aimed at app B.
    #[test]
    fn the_lease_authorizes_local_app_tools_by_their_builtin_names() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let lease = registry.begin_bound("app-a", &root).unwrap();
        let fs = roots(&root);

        for name in [
            "LocalAppBuild",
            "LocalAppLogs",
            "LocalAppRuntime",
            "LocalAppManifest",
            "LocalAppQueryData",
        ] {
            assert!(
                registry.allows_for_token(
                    Some(lease.token()),
                    name,
                    &serde_json::json!({"app_id": "app-a"}),
                    &fs
                ),
                "{name} must be lease-authorized for its own app"
            );
            assert!(
                !registry.allows_for_token(
                    Some(lease.token()),
                    name,
                    &serde_json::json!({"app_id": "app-b"}),
                    &fs
                ),
                "{name} must NOT reach a sibling app"
            );
        }

        // An operation outside the build loop is not lease-authorized.
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "LocalAppMutateData",
            &serde_json::json!({"app_id": "app-a"}),
            &fs
        ));
    }

    /// Every Vite config SPELLING must be host-owned, not just the one the
    /// template ships.
    ///
    /// Vite self-resolves its config from `DEFAULT_CONFIG_FILES`, and
    /// `vite.config.js` is FIRST in that list — ahead of the `vite.config.mjs`
    /// the host writes (verified against vite's own dist). A Vite config is
    /// executed Node code, so a model that creates `vite.config.js` shadows the
    /// host config and runs arbitrary code inside the build. `Edit` creates
    /// files on a nonexistent path, so the existing `Edit(./**)` grant reaches
    /// it without any new permission.
    ///
    /// `pnpm-workspace.yaml` is the same class: `prepare_dependency_staging`
    /// copies it into the install staging directory, and the install runs with
    /// the network ENABLED.
    #[test]
    fn every_host_config_spelling_is_host_owned() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let fs = roots(&root);

        // Everything the generated LINGXI.md declares host-managed must
        // actually BE host-owned. Four of these were declared to the agent but
        // enforced nowhere, so the workspace's own `Edit(./**)` grant
        // overwrote them and `restore_host_managed_files` silently reverted
        // the edit — the model loops against a file it cannot change.
        for name in [
            ".gitignore",
            "jsconfig.json",
            "lib/lingxi-provider.jsx",
            "lib/frame-loop.js",
            "lib/phaser-runtime.js",
            "lib/babylon-runtime.js",
            "styles/foundation.css",
            "vite.config.js",
            "vite.config.mjs",
            "vite.config.ts",
            "vite.config.cjs",
            "vite.config.mts",
            "vite.config.cts",
            "pnpm-workspace.yaml",
            // PostCSS is the SAME hole as the Vite config: the template sets
            // no `css.postcss` key and imports CSS, so Vite runs
            // `lilconfig("postcss").search(root)` on every build and executes
            // whatever it finds as Node code. `--config` pins only the Vite
            // config and does not affect this search. Filenames read from
            // vite's own `getDefaultSearchPlaces`.
            "postcss.config.js",
            "postcss.config.cjs",
            "postcss.config.mjs",
            ".postcssrc.js",
            ".postcssrc.cjs",
            ".postcssrc.mjs",
            ".postcssrc.json",
            ".config/postcssrc.js",
            ".config/postcssrc.cjs",
            ".config/postcssrc.mjs",
        ] {
            assert!(
                registry().denies_host_owned_for_workspace(
                    "Write",
                    &serde_json::json!({
                        "file_path": format!("/workspace/local-app-app-a/{name}")
                    }),
                    &fs,
                ),
                "{name} must be host-owned"
            );
        }

        // An ordinary source file with a similar name stays writable.
        assert!(
            !registry().denies_host_owned_for_workspace(
                "Write",
                &serde_json::json!({
                    "file_path": "/workspace/local-app-app-a/src/vite.config.helper.js"
                }),
                &fs,
            ),
            "app-owned source must stay writable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_app_settings_and_symlink_escape_stay_denied_without_a_lease() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join(".lingxi")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join(".lingxi/settings.local.json"), b"{}\n").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let fs = roots(&root);

        assert!(
            registry().denies_host_owned_for_workspace(
                "Edit",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/.lingxi/settings.local.json"}),
                &fs,
            )
        );
        assert!(
            registry().denies_host_owned_for_workspace(
                "Bash",
                &serde_json::json!({"command":"npm install"}),
                &fs,
            )
        );
        assert!(
            !registry().denies_host_owned_for_workspace(
                "Bash",
                &serde_json::json!({"command":"cat src/App.jsx"}),
                &fs,
            )
        );
        assert!(
            registry().escapes_workspace(
                "Edit",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/link/new.txt"}),
                &fs,
            )
        );
        assert!(
            !registry().escapes_workspace(
                "Edit",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/src/new.txt"}),
                &fs,
            )
        );
    }

    #[test]
    fn concurrent_leases_require_the_matching_token() {
        let dir = tempdir().unwrap();
        let root_a = dir.path().join("workspace-a");
        let root_b = dir.path().join("workspace-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let registry = registry();
        let lease_a = registry.begin_unchecked("app-a", root_a.clone());
        let lease_b = registry.begin_unchecked("app-b", root_b.clone());
        let fs_a = roots(&root_a);
        assert!(registry.allows_for_token(
            Some(lease_a.token()),
            "Write",
            &serde_json::json!({"file_path":"src/a.txt"}),
            &fs_a
        ));
        assert!(!registry.allows_for_token(
            Some(lease_b.token()),
            "Write",
            &serde_json::json!({"file_path":root_a.join("src/a.txt").to_string_lossy()}),
            &fs_a
        ));
    }

    #[test]
    fn shell_lease_allows_cd_but_rejects_nested_actions() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let lease = registry.begin_unchecked("app", root.clone());
        let fs = roots(&root);
        assert!(registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({"command":"cd . && grep -rn foo src/"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({"command":"find . -exec curl https://example.com {} +"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({"command":"sed -n 'e curl https://example.com' README.md"}),
            &fs
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_even_when_destination_is_missing() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":"link/new.txt"}),
            &fs
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_alias_is_compared_after_canonicalization() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("app", root.clone());
        let fs = roots(&root);
        let canonical = std::fs::canonicalize(&root).unwrap();
        let Some(raw) = canonical
            .to_str()
            .and_then(|path| path.strip_prefix("/private"))
        else {
            return;
        };
        assert!(registry.allows(
            "Write",
            &serde_json::json!({"file_path": format!("{raw}/alias.txt")}),
            &fs
        ));
    }
}
