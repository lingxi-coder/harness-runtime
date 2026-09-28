use super::*;

/// r1-prompt-layer-20: the backticked-tool-name scanner used to be declared
/// INSIDE the one test that ran it, so no other model-facing text could be put
/// through it. Hoisted here so every caller in the module can reuse it.
///
/// Names that legitimately appear as a bare backticked CamelCase span in
/// Host-authored, model-facing text but are NOT local-app tools. Anything else
/// shaped like a bare tool name is a typo or a retired name the model will try
/// to call and fail on.
#[cfg(test)]
const KNOWN_NON_LOCAL_APP_TOOL_NAMES: &[&str] = &["AskUserQuestion", "Skill", "Workflow"];

/// r2-tests-honesty-008: the scanner's `take_while(is_ascii_alphanumeric)`
/// tokeniser stopped at the first `-` or `:`, so a plugin-qualified skill id —
/// the one spelling that actually resolves — was invisible to it and a typo in
/// it could never be caught. These are the plugin-qualified ids the contracts
/// and skills are allowed to name.
#[cfg(test)]
const KNOWN_PLUGIN_QUALIFIED_IDS: &[&str] = &[
    "lingxi-local-app:apple-design",
    "lingxi-local-app:create-local-app",
    "lingxi-local-app:local-app-build",
    "lingxi-local-app:local-app-mcp-authoring",
    "lingxi-local-app:local-app-use-test",
];

/// Assert that every backticked span in `contract` that is SHAPED like a tool
/// or skill name is one that actually exists.
///
/// Three shapes are checked, and a span matching none of them (prose, a path,
/// a JSON example, a lower-case field name) is deliberately left alone:
///
/// 1. a span whose leading ASCII-alphanumeric run starts with `LocalApp` —
///    this is what catches `` `LocalAppBuild {"app_id":"…"}` `` examples;
/// 2. a bare CamelCase span the text immediately calls a *tool* (`` `X` tool ``)
///    — checked against `LOCAL_APP_TOOLS` plus
///    [`KNOWN_NON_LOCAL_APP_TOOL_NAMES`]; this is the half of
///    r2-tests-honesty-008 that the alphanumeric `take_while` could not see,
///    since it only ever compared the leading run against `LocalApp`;
/// 3. a span shaped `<plugin>:<name>` in lower kebab case — a plugin-qualified
///    skill or workflow id, checked against [`KNOWN_PLUGIN_QUALIFIED_IDS`].
#[cfg(test)]
fn assert_only_real_tool_names(contract: &str, label: &str) {
    // r2-tests-honesty-008: the scan below pairs backticks POSITIONALLY
    // (1st-2nd, 3rd-4th, …) with no balance check, so a single stray backtick
    // anywhere in `contract` silently shifts every later span and can hide a
    // planted bad name behind an innocuous one. Fail loudly and specifically
    // instead of scanning a text this gate cannot actually parse.
    assert!(
        contract.matches('`').count() % 2 == 0,
        "{label} has an ODD number of backticks, so this scanner cannot pair them \
         into spans without silently shifting every one after the stray mark: {contract}"
    );
    let mut offset = 0;
    // r4-tests-honesty-08: this scanner only asserts INSIDE a span it finds,
    // so if the contract stops using backticks altogether the `while` body
    // never runs and the whole scan silently no-ops — indistinguishable from
    // every span having checked out clean. Count the spans it actually walked
    // and require at least one.
    let mut spans = 0usize;
    while let Some(found) = contract[offset..].find('`') {
        let start = offset + found + 1;
        let Some(found_end) = contract[start..].find('`') else {
            break;
        };
        let end = start + found_end;
        let span = &contract[start..end];
        spans += 1;
        let name: String = span
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if let Some(rest) = name.strip_prefix("LocalApp") {
            if !rest.is_empty() {
                assert!(
                    crate::mobile::local_apps_tools::LOCAL_APP_TOOLS
                        .iter()
                        .any(|&(tool_name, _, _)| tool_name == name),
                    "{label} names backticked tool `{name}`, which is not in \
                     LOCAL_APP_TOOLS and cannot be called: {contract}"
                );
            }
        }
        // Deliberately NOT "every backticked CamelCase span": these contracts
        // legitimately backtick component and API names (`IonRouterOutlet`,
        // `IonPage`, `Routes`, `Route`), and flagging those would make the
        // gate cry wolf until someone deleted it. The idiom that actually
        // means "call this" is `` `X` tool ``, which is exactly how the
        // guided contract names `Skill` — the case r2-tests-honesty-008
        // recorded as invisible, because `take_while(is_ascii_alphanumeric)`
        // only ever compared against the `LocalApp` prefix.
        let is_bare_tool_name = !span.is_empty()
            && span.chars().all(|c| c.is_ascii_alphanumeric())
            && span.starts_with(|c: char| c.is_ascii_uppercase())
            && contract[end + 1..].starts_with(" tool");
        if is_bare_tool_name {
            assert!(
                crate::mobile::local_apps_tools::LOCAL_APP_TOOLS
                    .iter()
                    .any(|&(tool_name, _, _)| tool_name == span)
                    || KNOWN_NON_LOCAL_APP_TOOL_NAMES.contains(&span),
                "{label} names backticked tool `{span}`, which is neither a LOCAL_APP_TOOLS \
                 entry nor one of the known non-local-app tools \
                 {KNOWN_NON_LOCAL_APP_TOOL_NAMES:?}: {contract}"
            );
        }
        let is_plugin_qualified_id = span.matches(':').count() == 1
            && span.split(':').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        if is_plugin_qualified_id {
            assert!(
                KNOWN_PLUGIN_QUALIFIED_IDS.contains(&span),
                "{label} names backticked plugin-qualified id `{span}`, which is not one of \
                 {KNOWN_PLUGIN_QUALIFIED_IDS:?} and will not resolve: {contract}"
            );
        }
        offset = end + 1;
    }
    assert!(
        spans > 0,
        "{label} has no backtick-delimited spans at all, so this scanner never ran: {contract}"
    );
}

/// r1-prompt-layer-20: the backtick scanner above is the right shape for a
/// workspace contract, where a tool name is always a backticked span — and the
/// WRONG shape for the shipped prompt FILES. The next-step guidance strings
/// name a tool mid-sentence with no backticks at all, so the leading-run
/// tokeniser never sees it; in `SKILL.md` a tool name may appear in a fenced
/// block or in bare prose.
///
/// So scan those files for the `LocalApp…` TOKEN wherever it appears and
/// require each one to be a real entry in `LOCAL_APP_TOOLS`. The token count
/// is asserted non-zero so a gate that scanned the wrong file, or a file that
/// stopped naming tools, cannot report "all clear".
#[cfg(test)]
fn assert_only_real_local_app_tool_tokens(text: &str, label: &str) {
    let bytes = text.as_bytes();
    let mut checked = 0usize;
    let mut cursor = 0usize;
    while let Some(relative) = text[cursor..].find("LocalApp") {
        let start = cursor + relative;
        let preceded_by_identifier =
            start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let end = start
            + text[start..]
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(text.len() - start);
        let token = &text[start..end];
        cursor = end.max(start + 1);
        // `LocalApps`/`LocalApp` on their own are prose ("the LocalApp tools"),
        // not a call; a token glued to a preceding identifier is a Rust/JS
        // symbol such as `LocalAppsHostBroker`'s caller, not a tool name.
        if preceded_by_identifier || token == "LocalApp" || token == "LocalApps" {
            continue;
        }
        checked += 1;
        assert!(
            crate::mobile::local_apps_tools::LOCAL_APP_TOOLS
                .iter()
                .any(|&(tool_name, _, _)| tool_name == token),
            "{label} names `{token}`, which is not in LOCAL_APP_TOOLS and cannot be called"
        );
    }
    assert!(
        checked > 0,
        "{label} names no LocalApp* tool at all, so this scanner never ran — it is \
         reading the wrong file or the file stopped naming tools"
    );
}
use async_trait::async_trait;
use client_adapter::{ClientEventSink, MockSink};
use futures_util::stream;
use local_apps::test_support::FixedClock;
use local_apps::{storage, AppState, NoopAppEventObserver};
use mobile_linux_api::{
    LinuxCommandRequest, LinuxEnforcementReceipt, LinuxProcessHandle, MobileLinuxCapability,
    MobileLinuxError, MobileLinuxRuntimeMode, MobileLinuxTaskSnapshot, MobileLinuxTaskStatus,
    NetworkPolicy, PtyOpenRequest, PtySessionHandle, PtySize, RootfsState, RootfsStatus,
    SandboxBackend,
};
use serde_json::json;
use std::fs;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tempfile::TempDir;

#[test]
fn canonical_cwd_string_keeps_the_same_spelling_after_the_leaf_directory_is_deleted() {
    // r1-engine-core-010: mint runs while the workspace directory still
    // exists; a later cleanup (e.g. `remove_app_session_file`) can run
    // after it is gone. A bare `canonicalize(..).unwrap_or(raw)` gives
    // those two calls DIFFERENT spellings on a symlink-split platform
    // (`/var` vs `/private/var`), so the cleanup misses the catalog
    // directory the mint actually wrote to.
    let tmp = TempDir::new().unwrap();
    let leaf = tmp.path().join("workspace");
    std::fs::create_dir_all(&leaf).unwrap();
    let while_present = canonical_cwd_string(&leaf);
    std::fs::remove_dir_all(&leaf).unwrap();
    let after_deleted = canonical_cwd_string(&leaf);
    assert_eq!(
        while_present, after_deleted,
        "mint (leaf exists) and a later cleanup (leaf deleted) must agree \
         on the same catalog directory spelling"
    );
}

#[derive(Default)]
struct NoopClientEventSink;

#[async_trait]
impl ClientEventSink for NoopClientEventSink {
    async fn emit(&self, _event: ClientEvent) {}
}

fn mock_pnpm_lockfile(package_json: &[u8]) -> Result<Vec<u8>, String> {
    let dependencies = effective_package_dependency_specifiers(package_json)?;
    let mut lockfile = String::from(
        "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n\nimporters:\n\n  .:\n    dependencies:\n",
    );
    for (package, version) in dependencies {
        let package = serde_json::to_string(&package)
            .map_err(|error| format!("serialize mock lock package: {error}"))?;
        let version = serde_json::to_string(&version)
            .map_err(|error| format!("serialize mock lock specifier: {error}"))?;
        lockfile.push_str(&format!(
            "      {package}:\n        specifier: {version}\n        version: {version}\n"
        ));
    }
    lockfile.push_str("\npackages: {}\n");
    Ok(lockfile.into_bytes())
}

struct MockTask {
    snapshot: Mutex<MobileLinuxTaskSnapshot>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

struct MockMobileLinuxRuntime {
    spawn_delay: Duration,
    spawn_count: AtomicUsize,
    next_task_id: AtomicU64,
    tasks: Mutex<HashMap<String, Arc<MockTask>>>,
    last_request: Mutex<Option<LinuxCommandRequest>>,
    isolated_requests: Mutex<Vec<LinuxCommandRequest>>,
    pnpm_node_modules_entries: Mutex<Vec<Vec<String>>>,
    resolved_pnpm_lockfile: Mutex<Option<Vec<u8>>>,
    enforcement_receipt: AtomicBool,
    fail_kill: AtomicBool,
    fail_build: AtomicBool,
    fail_frozen_install: AtomicBool,
    inject_lifecycle_script: AtomicBool,
    omit_staged_vite_marker: AtomicBool,
}

impl MockMobileLinuxRuntime {
    fn new(spawn_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            spawn_delay,
            spawn_count: AtomicUsize::new(0),
            next_task_id: AtomicU64::new(1),
            tasks: Mutex::new(HashMap::new()),
            last_request: Mutex::new(None),
            isolated_requests: Mutex::new(Vec::new()),
            pnpm_node_modules_entries: Mutex::new(Vec::new()),
            resolved_pnpm_lockfile: Mutex::new(None),
            enforcement_receipt: AtomicBool::new(true),
            fail_kill: AtomicBool::new(false),
            fail_build: AtomicBool::new(false),
            fail_frozen_install: AtomicBool::new(false),
            inject_lifecycle_script: AtomicBool::new(false),
            omit_staged_vite_marker: AtomicBool::new(false),
        })
    }

    fn set_fail_kill(&self, fail: bool) {
        self.fail_kill.store(fail, Ordering::SeqCst);
    }

    fn set_enforcement_receipt(&self, enforced: bool) {
        self.enforcement_receipt.store(enforced, Ordering::SeqCst);
    }

    fn set_omit_staged_vite_marker(&self, omit: bool) {
        self.omit_staged_vite_marker.store(omit, Ordering::SeqCst);
    }

    fn set_fail_build(&self, fail: bool) {
        self.fail_build.store(fail, Ordering::SeqCst);
    }

    fn set_fail_frozen_install(&self, fail: bool) {
        self.fail_frozen_install.store(fail, Ordering::SeqCst);
    }

    async fn set_resolved_pnpm_lockfile(&self, lockfile: Vec<u8>) {
        *self.resolved_pnpm_lockfile.lock().await = Some(lockfile);
    }

    fn set_inject_lifecycle_script(&self, inject: bool) {
        self.inject_lifecycle_script.store(inject, Ordering::SeqCst);
    }

    async fn isolated_requests(&self) -> Vec<LinuxCommandRequest> {
        self.isolated_requests.lock().await.clone()
    }

    async fn pnpm_node_modules_entries(&self) -> Vec<Vec<String>> {
        self.pnpm_node_modules_entries.lock().await.clone()
    }

    async fn recorded_request(&self) -> LinuxCommandRequest {
        self.last_request
            .lock()
            .await
            .clone()
            .expect("spawn request recorded")
    }

    fn enforce_network_policy(request: &LinuxCommandRequest) -> Result<(), MobileLinuxError> {
        if matches!(request.network, NetworkPolicy::LoopbackOnly)
            && request.resource_limits.max_memory_mb == Some(800)
        {
            Ok(())
        } else {
            Err(MobileLinuxError::InvalidRequest(
                "full local-app runtime requires loopback-only networking and 800 MiB".into(),
            ))
        }
    }

    fn spawn_count(&self) -> usize {
        self.spawn_count.load(Ordering::SeqCst)
    }

    async fn first_task_id(&self) -> String {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Some(task_id) = self.tasks.lock().await.keys().next().cloned() {
                    return task_id;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task created")
    }

    async fn complete_task(&self, task_id: &str, status: MobileLinuxTaskStatus, detail: &str) {
        let task = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .expect("task exists");
        {
            let mut snapshot = task.snapshot.lock().await;
            snapshot.status = status;
            snapshot.finished_at_ms = Some(2);
            snapshot.detail = Some(detail.to_string());
            snapshot.exit_code = Some(match status {
                MobileLinuxTaskStatus::Completed => 0,
                _ => 1,
            });
        }
        let shutdown = { task.shutdown.lock().await.take() };
        if let Some(shutdown) = shutdown {
            let _ = shutdown.send(());
        }
    }
}

#[async_trait]
impl MobileLinuxRuntime for MockMobileLinuxRuntime {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::IosIsh
    }

    fn mode(&self) -> MobileLinuxRuntimeMode {
        MobileLinuxRuntimeMode::MobileLinux
    }

    async fn probe_capability(&self) -> MobileLinuxCapability {
        MobileLinuxCapability {
            available: true,
            backend: self.backend(),
            mode: self.mode(),
            reason: None,
            streaming_output: false,
            background_processes: true,
            pty: false,
            bind_mounts: true,
            rootfs_integrity: false,
        }
    }

    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(self.rootfs_status().await?)
    }

    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        Ok(())
    }

    async fn run(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<mobile_linux_api::LinuxCommandResult, MobileLinuxError> {
        Self::enforce_network_policy(&request)?;
        Err(MobileLinuxError::Unsupported)
    }

    async fn run_isolated(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<mobile_linux_api::LinuxCommandResult, MobileLinuxError> {
        *self.last_request.lock().await = Some(request.clone());
        self.isolated_requests.lock().await.push(request.clone());
        let build_mount = request.mounts.first().ok_or_else(|| {
            MobileLinuxError::InvalidRequest("missing LocalAppBuild mount".into())
        })?;
        let guest_cwd = request.cwd.clone().ok_or_else(|| {
            MobileLinuxError::InvalidRequest("missing dependency staging cwd".into())
        })?;
        let relative = guest_cwd
            .strip_prefix(&build_mount.guest_path)
            .map(|suffix| suffix.trim_start_matches('/'))
            .ok_or_else(|| {
                MobileLinuxError::InvalidRequest(
                    "dependency staging cwd is outside the mounted workspace".into(),
                )
            })?;
        let host_cwd = if relative.is_empty() {
            build_mount.host_path.clone()
        } else {
            build_mount.host_path.join(relative)
        };
        if matches!(request.command.as_str(), "/usr/bin/pnpm") {
            let mut entries = fs::read_dir(host_cwd.join("node_modules"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            entries.sort();
            self.pnpm_node_modules_entries.lock().await.push(entries);
        }
        if matches!(request.command.as_str(), "/usr/bin/pnpm")
            && request.args.iter().any(|arg| arg == "--frozen-lockfile")
            && self.fail_frozen_install.load(Ordering::SeqCst)
        {
            return Ok(mobile_linux_api::LinuxCommandResult {
                stdout: String::new(),
                stderr: "synthetic frozen install failure".into(),
                exit_code: 1,
                timed_out: false,
                cancelled: false,
                enforcement: mobile_linux_api::LinuxEnforcementReceipt {
                    network_policy_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                    memory_limit_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                },
            });
        }
        if matches!(request.command.as_str(), "/usr/bin/pnpm")
            && request.args.iter().any(|arg| arg == "--no-frozen-lockfile")
        {
            let lockfile = match self.resolved_pnpm_lockfile.lock().await.clone() {
                Some(lockfile) => lockfile,
                None => {
                    let package = fs::read(host_cwd.join("package.json")).map_err(|error| {
                        MobileLinuxError::Io(format!(
                            "read fake resolution input package: {error}"
                        ))
                    })?;
                    mock_pnpm_lockfile(&package).map_err(MobileLinuxError::Io)?
                }
            };
            fs::write(host_cwd.join("pnpm-lock.yaml"), lockfile).map_err(|error| {
                MobileLinuxError::Io(format!("write fake resolved lockfile: {error}"))
            })?;
        }
        if matches!(request.command.as_str(), "/usr/bin/pnpm")
            && request.args.iter().any(|arg| arg == "--lockfile-only")
        {
            return Ok(mobile_linux_api::LinuxCommandResult {
                stdout: "lockfile resolved".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
                cancelled: false,
                enforcement: mobile_linux_api::LinuxEnforcementReceipt {
                    network_policy_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                    memory_limit_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                },
            });
        }
        if matches!(request.command.as_str(), "/usr/bin/node") {
            if self.fail_build.load(Ordering::SeqCst) {
                return Ok(mobile_linux_api::LinuxCommandResult {
                    stdout: String::new(),
                    stderr: "synthetic build failure".into(),
                    exit_code: 1,
                    timed_out: false,
                    cancelled: false,
                    enforcement: mobile_linux_api::LinuxEnforcementReceipt {
                        network_policy_enforced: self
                            .enforcement_receipt
                            .load(Ordering::SeqCst),
                        memory_limit_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                    },
                });
            }
            let output_rel = request
                .args
                .windows(2)
                .find_map(|pair| (pair[0] == "--outDir").then_some(pair[1].as_str()))
                .ok_or_else(|| {
                    MobileLinuxError::InvalidRequest("missing Vite --outDir".into())
                })?;
            let output = host_cwd.join(output_rel);
            fs::create_dir_all(&output).map_err(|error| {
                MobileLinuxError::Io(format!("create fake build output: {error}"))
            })?;
            fs::write(
                output.join("index.html"),
                b"<!doctype html><title>built</title>",
            )
            .map_err(|error| {
                MobileLinuxError::Io(format!("write fake build output: {error}"))
            })?;
            return Ok(mobile_linux_api::LinuxCommandResult {
                stdout: "built".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
                cancelled: false,
                enforcement: mobile_linux_api::LinuxEnforcementReceipt {
                    network_policy_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                    memory_limit_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                },
            });
        }
        fs::create_dir_all(host_cwd.join("node_modules")).map_err(|error| {
            MobileLinuxError::Io(format!("create fake node_modules root: {error}"))
        })?;
        if !self.omit_staged_vite_marker.load(Ordering::SeqCst) {
            let vite = host_cwd.join("node_modules/vite/bin/vite.js");
            fs::create_dir_all(vite.parent().expect("vite parent")).map_err(|error| {
                MobileLinuxError::Io(format!("create fake install tree: {error}"))
            })?;
            fs::write(&vite, b"#!/usr/bin/env node\n").map_err(|error| {
                MobileLinuxError::Io(format!("write fake vite binary: {error}"))
            })?;
            fs::write(
                host_cwd.join("node_modules/vite/package.json"),
                r#"{"name":"vite","version":"8.2.1","license":"MIT"}"#,
            )
            .map_err(|error| {
                MobileLinuxError::Io(format!("write fake vite manifest: {error}"))
            })?;
        }
        fs::write(host_cwd.join("node_modules/react.js"), b"react")
            .map_err(|error| MobileLinuxError::Io(format!("write fake dependency: {error}")))?;
        fs::create_dir_all(host_cwd.join("node_modules/react")).map_err(|error| {
            MobileLinuxError::Io(format!("create fake react package dir: {error}"))
        })?;
        let react_manifest = if self.inject_lifecycle_script.load(Ordering::SeqCst) {
            r#"{"name":"react","version":"19.2.8","license":"MIT","scripts":{"install":"echo unsafe"}}"#
        } else {
            r#"{"name":"react","version":"19.2.8","license":"MIT"}"#
        };
        fs::write(
            host_cwd.join("node_modules/react/package.json"),
            react_manifest,
        )
        .map_err(|error| MobileLinuxError::Io(format!("write fake react manifest: {error}")))?;
        let effective_package: Value =
            serde_json::from_slice(&fs::read(host_cwd.join("package.json")).map_err(
                |error| MobileLinuxError::Io(format!("read fake install package: {error}")),
            )?)
            .map_err(|error| {
                MobileLinuxError::Io(format!("parse fake install package: {error}"))
            })?;
        for (package, version) in effective_package
            .get("dependencies")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let Some(version) = version.as_str() else {
                continue;
            };
            let package_root = host_cwd
                .join("node_modules")
                .join(dependency_package_path(package));
            let package_manifest = package_root.join("package.json");
            if package_manifest.is_file() {
                continue;
            }
            fs::create_dir_all(&package_root).map_err(|error| {
                MobileLinuxError::Io(format!("create fake installed package: {error}"))
            })?;
            fs::write(
                &package_manifest,
                serde_json::to_vec(&json!({
                    "name": package,
                    "version": version,
                    "license": "MIT",
                }))
                .expect("serialize fake installed package"),
            )
            .map_err(|error| {
                MobileLinuxError::Io(format!("write fake installed package: {error}"))
            })?;
        }
        Ok(mobile_linux_api::LinuxCommandResult {
            stdout: "ok".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            cancelled: false,
            enforcement: mobile_linux_api::LinuxEnforcementReceipt {
                network_policy_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                memory_limit_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
            },
        })
    }

    async fn spawn_background(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        Self::enforce_network_policy(&request)?;
        *self.last_request.lock().await = Some(request.clone());
        self.spawn_count.fetch_add(1, Ordering::SeqCst);
        if !self.spawn_delay.is_zero() {
            sleep(self.spawn_delay).await;
        }
        let port = request
            .args
            .windows(2)
            .find_map(|window| (window[0] == "--port").then(|| window[1].parse::<u16>().ok()))
            .flatten()
            .ok_or_else(|| MobileLinuxError::InvalidRequest("missing --port".into()))?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|error| {
                MobileLinuxError::Io(format!("bind test runtime loopback: {error}"))
            })?;
        let (shutdown, mut receiver) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut receiver => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((mut stream, _)) => {
                                let _ = stream.shutdown().await;
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
        let task_id = format!("task-{}", self.next_task_id.fetch_add(1, Ordering::SeqCst));
        self.tasks.lock().await.insert(
            task_id.clone(),
            Arc::new(MockTask {
                snapshot: Mutex::new(MobileLinuxTaskSnapshot {
                    task_id: task_id.clone(),
                    status: MobileLinuxTaskStatus::Backgrounded,
                    command: request.command,
                    started_at_ms: Some(1),
                    finished_at_ms: None,
                    exit_code: None,
                    detail: None,
                }),
                shutdown: Mutex::new(Some(shutdown)),
            }),
        );
        Ok(LinuxProcessHandle {
            id: task_id,
            enforcement: LinuxEnforcementReceipt {
                network_policy_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
                memory_limit_enforced: self.enforcement_receipt.load(Ordering::SeqCst),
            },
        })
    }

    async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        if let Some(task) = self.tasks.lock().await.get(&handle.id).cloned() {
            {
                let mut snapshot = task.snapshot.lock().await;
                snapshot.status = MobileLinuxTaskStatus::Cancelled;
                snapshot.finished_at_ms = Some(2);
                snapshot.exit_code = Some(1);
                snapshot.detail = Some("killed".into());
            }
            if let Some(shutdown) = task.shutdown.lock().await.take() {
                let _ = shutdown.send(());
            }
        }
        if self.fail_kill.load(Ordering::SeqCst) {
            // Models the iSH `BACKGROUND_REAP_BUDGET` miss: the kill was
            // issued, only the exit confirmation timed out.
            return Err(MobileLinuxError::Io(format!(
                "background task {} did not reap within 3 seconds",
                handle.id
            )));
        }
        Ok(())
    }

    async fn open_pty(
        &self,
        _request: PtyOpenRequest,
    ) -> Result<PtySessionHandle, MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }

    async fn write_pty(
        &self,
        _handle: &PtySessionHandle,
        _input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }

    async fn resize_pty(
        &self,
        _handle: &PtySessionHandle,
        _size: PtySize,
    ) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }

    async fn close_pty(&self, _handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        Err(MobileLinuxError::Unsupported)
    }

    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(RootfsStatus {
            state: RootfsState::Ready,
            backend: self.backend(),
            mode: self.mode(),
            platform: "test".into(),
            abi: "test".into(),
            version: None,
            managed_root: None,
            active_root: None,
            staged_root: None,
            archive_sha256: None,
            installed_size_bytes: None,
            writable_guest_paths: vec![],
            last_error: None,
        })
    }

    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.rootfs_status().await
    }

    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.rootfs_status().await
    }

    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.rootfs_status().await
    }

    async fn configure_mounts(&self, _mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        Ok(())
    }

    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        let tasks = self.tasks.lock().await;
        let mut snapshots = Vec::with_capacity(tasks.len());
        for task in tasks.values() {
            snapshots.push(task.snapshot.lock().await.clone());
        }
        Ok(snapshots)
    }

    async fn task_status(
        &self,
        task_id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        let task = self.tasks.lock().await.get(task_id).cloned();
        Ok(match task {
            Some(task) => Some(task.snapshot.lock().await.clone()),
            None => None,
        })
    }
}

async fn test_service(root: &TempDir) -> Arc<AppService> {
    Arc::new(
        AppService::load(
            root.path(),
            Arc::new(FixedClock::new(1)),
            Arc::new(NoopAppEventObserver),
        )
        .await
        .expect("load app service"),
    )
}

fn create_configured_runtime_root(root: &TempDir) -> PathBuf {
    let runtime_root = root.path().join("runtime-root");
    fs::create_dir_all(&runtime_root).expect("create runtime root");
    runtime_root
}

fn create_configured_digest_runtime_root(root: &TempDir) -> PathBuf {
    let runtime_container = root.path().join("runtime-root");
    fs::create_dir_all(&runtime_container).expect("create runtime container");
    let runtime_root = runtime_container
        .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    fs::create_dir_all(&runtime_root).expect("create digest runtime root");
    runtime_root
}

fn write_runtime_seed_ready_marker(runtime_root: &Path) {
    let digest = runtime_root
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .expect("digest runtime root leaf");
    let marker = runtime_root
        .parent()
        .expect("digest runtime root parent")
        .join(format!(".{digest}.ready"));
    fs::write(marker, digest).expect("write ready marker");
}

fn create_runtime_root(root: &TempDir) -> PathBuf {
    let runtime_root = create_configured_runtime_root(root);
    let vite_bin = runtime_root.join("node_modules/vite/bin/vite.js");
    fs::create_dir_all(vite_bin.parent().unwrap()).expect("create Vite runtime root");
    fs::write(&vite_bin, b"#!/usr/bin/env node\n").expect("write Vite bin");
    runtime_root
}

#[tokio::test]
async fn await_fixed_runtime_root_waits_for_a_configured_seed_to_finish_staging() {
    let root = TempDir::new().expect("tempdir");
    let runtime_root = create_configured_digest_runtime_root(&root);
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        Some(runtime_root.clone()),
    );
    let runtime_root_for_seed = runtime_root.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(150)).await;
        let vite_bin = runtime_root_for_seed.join("node_modules/vite/bin/vite.js");
        fs::create_dir_all(vite_bin.parent().expect("vite bin parent"))
            .expect("create staged runtime root");
        fs::write(&vite_bin, b"#!/usr/bin/env node\n").expect("write staged Vite bin");
        write_runtime_seed_ready_marker(&runtime_root_for_seed);
    });

    let ready = broker
        .await_fixed_runtime_root(Duration::from_secs(1))
        .await
        .expect("wait for runtime seed");

    assert_eq!(ready, runtime_root);
}

#[tokio::test]
async fn await_fixed_runtime_root_times_out_when_the_seed_never_becomes_ready() {
    let root = TempDir::new().expect("tempdir");
    let runtime_root = create_configured_digest_runtime_root(&root);
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        Some(runtime_root.clone()),
    );

    let error = broker
        .await_fixed_runtime_root(Duration::from_millis(250))
        .await
        .expect_err("unready runtime seed must time out");

    assert!(error.contains("runtime root is configured at"), "{error}");
    assert!(error.contains("waited 250 ms"), "{error}");
}

#[tokio::test]
async fn await_fixed_runtime_root_accepts_an_immutable_bundle_root_without_a_ready_marker() {
    let root = TempDir::new().expect("tempdir");
    let runtime_root = create_runtime_root(&root);
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        Some(runtime_root.clone()),
    );

    let ready = broker
        .await_fixed_runtime_root(Duration::from_millis(50))
        .await
        .expect("bundle root should stay ready without a marker");

    assert_eq!(ready, runtime_root);
}

#[tokio::test]
async fn await_fixed_runtime_root_fails_fast_when_staging_wrote_a_failure_marker() {
    let root = TempDir::new().expect("tempdir");
    let runtime_root = create_configured_digest_runtime_root(&root);
    let failure_marker = runtime_root
        .parent()
        .expect("runtime root parent")
        .join(".0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.failed");
    fs::write(
        &failure_marker,
        "runtime seed inventory validation failed before publish",
    )
    .expect("write failure marker");
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        Some(runtime_root.clone()),
    );

    let started = tokio::time::Instant::now();
    let error = broker
        .await_fixed_runtime_root(Duration::from_secs(1))
        .await
        .expect_err("failure marker must fail fast");

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "failure marker should stop polling early",
    );
    assert!(
        error.contains("runtime seed inventory validation failed before publish"),
        "{error}"
    );
}

#[tokio::test]
async fn await_fixed_runtime_root_rejects_a_corrupt_ready_marker() {
    let root = TempDir::new().expect("tempdir");
    let runtime_root = create_configured_digest_runtime_root(&root);
    let vite_bin = runtime_root.join("node_modules/vite/bin/vite.js");
    fs::create_dir_all(vite_bin.parent().expect("vite bin parent"))
        .expect("create staged runtime root");
    fs::write(&vite_bin, b"#!/usr/bin/env node\n").expect("write staged Vite bin");
    let marker = runtime_root
        .parent()
        .expect("runtime root parent")
        .join(".0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.ready");
    fs::write(&marker, "wrong-digest").expect("write corrupt ready marker");
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        Some(runtime_root.clone()),
    );

    let error = broker
        .await_fixed_runtime_root(Duration::from_millis(50))
        .await
        .expect_err("corrupt ready marker must fail");

    assert!(
        error.contains("must contain exactly its digest leaf"),
        "{error}"
    );
}

async fn create_broker(
    full_runtime: bool,
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
) -> (TempDir, Arc<AppService>, Arc<LocalAppsHostBroker>) {
    create_broker_over(TempDir::new().expect("tempdir"), full_runtime, mobile_linux).await
}

/// [`create_broker`] over a root somebody else prepared — the seam
/// [`seed_app_fixture`] needs, because a seeded app has to be on disk
/// BEFORE the service loads it.
async fn create_broker_over(
    root: TempDir,
    full_runtime: bool,
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
) -> (TempDir, Arc<AppService>, Arc<LocalAppsHostBroker>) {
    let service = test_service(&root).await;
    let runtime_root = full_runtime.then(|| create_runtime_root(&root));
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        mobile_linux,
        full_runtime,
        runtime_root,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    (root, service, broker)
}

async fn create_app_fixture(root: &TempDir, service: &Arc<AppService>, name: &str) -> String {
    let record = service
        .create_app(Some(name), "a test app", None)
        .await
        .expect("create app");
    seed_launchable_runtime_fixture(root.path(), &record, name);
    service
        .commit_scaffold(&record.id, name, "a test app", None, None)
        .await
        .expect("commit fixture scaffold");
    record.id
}

/// Give an app an immutable v3 active-build receipt.
///
/// Without one `derive_publication_state` answers `Draft` and
/// `emit_managed_mcp_inventory` skips the app entirely, so a fixture that
/// forgets this reports "all clear" over an EMPTY listing.
fn publish_fixture_build(root: &TempDir, app_id: &str, build_id: &str) -> AppLayout {
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.to_string()).expect("layout");
    fs::write(
        root.path().join(layout.build_rel(false)).join("build.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 3,
            "buildId": build_id,
            "buildKey": build_id,
            "runtimeContractSha256": "0".repeat(64),
            "dependencySnapshotSha256": "0".repeat(64),
            "outputSha256": "0".repeat(64),
        }))
        .expect("serialize build receipt"),
    )
    .expect("write build receipt");
    layout
}

struct HostQaFixture {
    _root: TempDir,
    broker: Arc<LocalAppsHostBroker>,
    sink: Arc<MockSink>,
    app_id: String,
    workflow_run_id: String,
    qa_handle: String,
    layout: AppLayout,
}

async fn host_qa_fixture() -> HostQaFixture {
    host_qa_fixture_with_mobile_linux(None).await
}

async fn host_qa_fixture_with_mobile_linux(
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
) -> HostQaFixture {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        mobile_linux,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    assert!(broker
        .attach_host_environment(host_environment(
            platform_api::MobileHostOs::Ios,
            platform_api::MobileDeviceClass::Phone,
        ))
        .is_ok());
    let app_id = create_app_fixture(&root, &service, "Host QA").await;
    let layout = AppLayout::new(root.path(), &app_id).expect("layout");
    let mut manifest = load_manifest(&layout).expect("manifest");
    manifest.revision = manifest.revision.saturating_add(1).max(1);
    manifest.collections = vec![
        local_apps::DataCollectionSchema {
            id: "chores".into(),
            name: "Chores".into(),
            fields: vec![local_apps::DataFieldSchema {
                id: "title".into(),
                label: "Title".into(),
                kind: local_apps::DataFieldKind::Text,
                required: true,
                enum_options: Vec::new(),
            }],
        },
        local_apps::DataCollectionSchema {
            id: "notes".into(),
            name: "Notes".into(),
            fields: vec![local_apps::DataFieldSchema {
                id: "title".into(),
                label: "Title".into(),
                kind: local_apps::DataFieldKind::Text,
                required: true,
                enum_options: Vec::new(),
            }],
        },
    ];
    local_apps::save_manifest(&layout, &manifest).expect("save QA manifest");
    AppDataStore::with_cached(layout.clone(), |store| {
        store
            .migrate_manifest(&manifest, false, now_ms())
            .map(|_| ())
    })
    .expect("bind QA database to manifest");
    let spec: local_apps::AppAuthoringSpec = serde_json::from_str(include_str!(
        "../../../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
    ))
    .expect("authoring fixture");
    let contract = local_apps::AppAuthoringContract {
        version: local_apps::AUTHORING_SCHEMA_VERSION,
        revision: 1,
        app_id: app_id.clone(),
        runtime_profile: manifest.runtime_profile.clone().expect("runtime profile"),
        spec,
    };
    let authoring_sha256 = local_apps::save_authoring_contract(&layout, &contract)
        .expect("save authoring contract");
    let build_path = root.path().join(layout.build_rel(false)).join("build.json");
    let mut build: Value =
        serde_json::from_slice(&fs::read(&build_path).expect("build receipt"))
            .expect("parse build receipt");
    build["runtimeContractSha256"] =
        Value::String(manifest.runtime_contract_hash().expect("runtime digest"));
    build["dependencySnapshotSha256"] = Value::String(
        manifest
            .dependency_snapshot_hash()
            .expect("dependency digest"),
    );
    build["authoringContractSha256"] = Value::String(authoring_sha256);
    fs::write(
        &build_path,
        serde_json::to_vec_pretty(&build).expect("serialize build receipt"),
    )
    .expect("select authoring contract");
    broker
        .start_runtime(&app_id)
        .await
        .expect("start static QA runtime");
    let workflow_run_id = "workflow-host-qa".to_string();
    let begun = broker
        .qa_begin(json!({
            "app_id": app_id,
            "workflow_run_id": workflow_run_id,
            "verification_strategy": "balanced",
        }))
        .await
        .expect("begin Host QA");
    let qa_handle = begun["qa_handle"].as_str().expect("QA handle").to_string();
    HostQaFixture {
        _root: root,
        broker,
        sink,
        app_id,
        workflow_run_id,
        qa_handle,
        layout,
    }
}

#[tokio::test]
async fn qa_begin_exposes_durable_upstream_ledger_for_a_fresh_handle() {
    let fixture = host_qa_fixture().await;
    let original = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("initial QA session");
    local_apps::qa_cleanup_session(&fixture.layout, &fixture.qa_handle)
        .expect("cleanup initial QA session");
    let mut ledger_identity = original.identity.clone();
    ledger_identity.qa_handle = local_apps::ids::generate_qa_handle();
    local_apps::qa_begin_with_scope(
        &fixture.layout,
        ledger_identity.clone(),
        original.scenario_requirements,
        original.verification_scope,
        vec![local_apps::QaUpstreamFailure {
            id: "source:build-smoke".into(),
            message: "source smoke failure must be repaired".into(),
            introduced_at_ms: now_ms().saturating_sub(1).max(1),
        }],
        now_ms(),
    )
    .expect("persist upstream finding in Host ledger");
    local_apps::qa_cleanup_session(&fixture.layout, &ledger_identity.qa_handle)
        .expect("cleanup seeded ledger session");

    let begun = fixture
        .broker
        .qa_begin(json!({
            "app_id": fixture.app_id,
            "workflow_run_id": fixture.workflow_run_id,
            "verification_strategy": "balanced",
        }))
        .await
        .expect("fresh QA begin reads the durable ledger");
    assert_eq!(begun["upstream_failures"][0]["id"], "source:build-smoke");
    assert_eq!(
        begun["upstream_failures"][0]["message"],
        "source smoke failure must be repaired"
    );
    assert_eq!(begun["upstream_findings"][0]["id"], "source:build-smoke");
    let fresh_handle = begun["qa_handle"].as_str().expect("fresh handle");
    local_apps::qa_cleanup_session(&fixture.layout, fresh_handle)
        .expect("cleanup fresh QA session");
}

#[tokio::test]
async fn host_qa_terminal_rejects_current_device_scope_changes_before_publication() {
    let fixture = host_qa_fixture().await;
    let candidate = finalize_passing_host_qa(&fixture).await;
    let original_contract_sha256 =
        crate::mobile::local_apps_build::active_build_authoring_contract_sha256(
            &fixture.layout,
        )
        .expect("active authoring selector")
        .expect("active authoring contract");
    let mut changed_contract = local_apps::authoring::load_authoring_contract(
        &fixture.layout,
        &original_contract_sha256,
    )
    .expect("load active authoring contract");
    changed_contract.spec.targets[0].form_factor = "tablet".into();
    let mut current_target = changed_contract.spec.targets[0].clone();
    current_target.id = "current-device".into();
    current_target.form_factor = "iphone".into();
    changed_contract.spec.targets.push(current_target);
    let mut current_presentation = changed_contract.spec.design.presentations[0].clone();
    current_presentation.target_id = "current-device".into();
    changed_contract
        .spec
        .design
        .presentations
        .push(current_presentation);
    changed_contract.spec.acceptance_checks[0]
        .target_ids
        .push("current-device".into());
    let changed_contract_sha256 =
        local_apps::authoring::save_authoring_contract(&fixture.layout, &changed_contract)
            .expect("persist changed authoring contract");
    let build_path = fixture
        .layout
        .root()
        .join(fixture.layout.build_rel(false))
        .join("build.json");
    let mut build: Value =
        serde_json::from_slice(&fs::read(&build_path).expect("build receipt"))
            .expect("parse build receipt");
    build["authoringContractSha256"] = Value::String(changed_contract_sha256);
    fs::write(
        &build_path,
        serde_json::to_vec_pretty(&build).expect("serialize changed build receipt"),
    )
    .expect("select changed authoring contract");

    let error = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result_for(&candidate),
        )
        .await
        .expect_err("terminal publication must revalidate current Host scope");
    assert!(
        error.contains("current Host device scope changed since QA"),
        "{error}"
    );
}

#[tokio::test]
async fn qa_requests_reject_out_of_scope_targets_before_native_or_data_side_effects() {
    let fixture = host_qa_fixture().await;
    fixture
        .broker
        .session_permissions
        .lock()
        .await
        .grant(&fixture.app_id, AppCapability::UiControl);
    let before = fixture
        .broker
        .query_data_value(json!({
            "app_id": fixture.app_id,
            "collection": "chores",
        }))
        .await
        .expect("read initial collection");
    let wrong_target_write = fixture
        .broker
        .mutate_data_value(
            json!({
                "app_id": fixture.app_id,
                "qa_handle": fixture.qa_handle,
                "scenario_id": "primary-action",
                "target_id": "android-tablet",
                "collection": "chores",
                "operations": [{
                    "kind": "upsert",
                    "recordId": "must-not-write",
                    "document": {"title": "blocked"},
                }],
            }),
            false,
            None,
        )
        .await
        .expect_err("out-of-scope target must fail before data mutation");
    assert!(
        wrong_target_write.contains("qa_target_unavailable"),
        "{wrong_target_write}"
    );
    let after = fixture
        .broker
        .query_data_value(json!({
            "app_id": fixture.app_id,
            "collection": "chores",
        }))
        .await
        .expect("read collection after rejected mutation");
    assert_eq!(after["records"], before["records"]);

    let wrong_scenario = fixture
        .broker
        .act_on_ui(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_id": "android-only",
            "target_id": "primary",
            "action": "click",
            "target": {"element_id": "submit"},
        }))
        .await
        .expect_err("unknown scenario must fail before native UI dispatch");
    assert!(
        wrong_scenario.contains("qa_scenario_invalid"),
        "{wrong_scenario}"
    );
    assert!(
        fixture
            .sink
            .events()
            .await
            .into_iter()
            .all(|event| !matches!(
                event,
                ClientEvent::AppEvent {
                    event: AppEventDto::AppUiRequest { .. }
                }
            )),
        "rejected QA requests must not dispatch native UI"
    );
}

async fn finalize_passing_host_qa(fixture: &HostQaFixture) -> Value {
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
    });
    let action_event = fixture
        .broker
        .begin_qa_action(&input)
        .await
        .expect("open QA action")
        .expect("QA event id");
    let bridge_result = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "qa-fixture-write".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "chores",
                    "operations": [{"kind": "upsert", "recordId": "row-1", "document": {"title": "Sweep"}}],
                })
                .to_string(),
            ),
        })
        .await
        .expect("perform page bridge mutation");
    assert_eq!(bridge_result["results"][0]["recordId"], "row-1");
    assert_eq!(bridge_result["results"][0]["revision"], 1);
    let second_bridge_result = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "qa-fixture-write-notes".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "notes",
                    "operations": [{"kind": "upsert", "recordId": "note-1", "document": {"title": "Remember"}}],
                })
                .to_string(),
            ),
        })
        .await
        .expect("perform second page bridge mutation");
    assert_eq!(second_bridge_result["results"][0]["recordId"], "note-1");
    assert_eq!(second_bridge_result["results"][0]["revision"], 1);
    let after_write = fixture
        .broker
        .query_data_value(json!({"app_id": fixture.app_id, "collection": "chores"}))
        .await
        .expect("query after page write");
    assert_eq!(
        after_write["records"][0]["recordId"], "row-1",
        "the page bridge write must be immediately visible before native attribution settles"
    );

    let runtime = fixture
        .broker
        .service()
        .expect("service")
        .runtime_record(&fixture.app_id)
        .await
        .expect("runtime record");
    let runtime_url = crate::mobile::local_apps_bridge::runtime_preview_url(&runtime)
        .expect("runtime preview URL");
    let result = fixture
        .broker
        .qa_ui_response_value(
            &input,
            json!({
                "lingxi_qa": {
                    "version": 1,
                    "requested_runtime_url": runtime_url,
                    "loaded_runtime_url": runtime_url,
                    "platform": "ios",
                    "form_factor": "iphone",
                    "navigation_generation": 1,
                    "device_model": "iPhone fixture",
                },
                "result": {"clicked": true},
            }),
        )
        .await
        .expect("validate native attestation");
    let action = fixture
        .broker
        .take_qa_action(&fixture.app_id, &action_event)
        .await
        .expect("close QA action window");
    fixture
        .broker
        .record_qa_observation(&input, "act_on_ui", result, action_event, None)
        .await
        .expect("record UI action");
    fixture
        .broker
        .commit_qa_action_mutations(&fixture.app_id, &action)
        .await
        .expect("commit page mutation evidence attribution");
    fixture
        .broker
        .record_qa_observation(
            &input,
            "inspect_ui",
            json!({"elements": [{"role": "row", "name": "Sweep"}]}),
            fixture.broker.request_id("qa-inspect"),
            None,
        )
        .await
        .expect("record inspect evidence");
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let capture = fixture
        .broker
        .qa_ui_response_value(
            &input,
            json!({
                "lingxi_qa": {
                    "version": 1,
                    "requested_runtime_url": runtime_url,
                    "loaded_runtime_url": runtime_url,
                    "platform": "ios",
                    "form_factor": "iphone",
                    "navigation_generation": 1,
                    "device_model": "iPhone fixture",
                },
                "result": {"image": {"data": png, "mime_type": "image/png"}},
            }),
        )
        .await
        .expect("validate native capture attestation");
    fixture
        .broker
        .record_qa_observation(
            &input,
            "capture_ui",
            capture,
            fixture.broker.request_id("qa-capture"),
            None,
        )
        .await
        .expect("record image evidence");
    fixture
        .broker
        .query_data_value(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_id": "primary-action",
            "target_id": "primary",
            "collection": "chores",
            "filters": [{"fieldId": "title", "operator": "equal", "value": "Sweep"}],
        }))
        .await
        .expect("record matching query evidence");
    fixture
        .broker
        .query_data_value(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_id": "primary-action",
            "target_id": "primary",
            "collection": "notes",
            "filters": [{"fieldId": "title", "operator": "equal", "value": "Remember"}],
        }))
        .await
        .expect("record second matching query evidence");
    let session = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("load QA session");
    let evidence_ids = session
        .evidence
        .iter()
        .map(|evidence| evidence.evidence_id.clone())
        .collect::<Vec<_>>();
    fixture
        .broker
        .qa_finalize(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_judgements": [{
                "scenario_id": "primary-action",
                "status": "passed",
                "evidence_ids": evidence_ids,
                "summary": "UI action persisted and remained visible",
            }],
            "findings": [],
        }))
        .await
        .expect("finalize Host QA")
}

fn workflow_result_for(candidate: &Value) -> Value {
    json!({
        "ok": true,
        "summary": "Host QA passed",
        "receipt_id": candidate["receipt"]["receipt_id"],
        "verification": candidate,
    })
}

async fn publish_passing_host_qa_and_emit(fixture: &HostQaFixture) {
    let candidate = finalize_passing_host_qa(fixture).await;
    let prepared = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result_for(&candidate),
        )
        .await
        .expect("prepare passing QA publication");
    fixture
        .broker
        .commit_prepared_workflow_qa_publication(&prepared)
        .expect("publish passing QA receipt");
    fixture
        .broker
        .emit_committed_workflow_qa_summary(&prepared)
        .await;
}

fn sync_fixture_dependency_roots(broker: &LocalAppsHostBroker, layout: &AppLayout) {
    let workspace = layout.root().join(layout.workspace_rel());
    fs::copy(
        workspace.join(crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL),
        workspace.join("package.json"),
    )
    .expect("restore fixture package.json from its trusted snapshot");
    fs::copy(
        workspace.join(crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL),
        workspace.join("pnpm-lock.yaml"),
    )
    .expect("restore fixture lockfile from its trusted snapshot");
    let target = crate::mobile::local_apps_build::detect_build_target(layout)
        .expect("fixture build target");
    crate::mobile::local_apps_build::restore_host_managed_files(&workspace, target)
        .expect("restore fixture host-managed dependency roots");
    assert!(
        LocalAppsHostBroker::dependency_inputs_match(layout)
            .expect("validate fixture dependency roots"),
        "the synthetic QA fixture must satisfy the same dependency preflight as LocalAppBuild"
    );
    let lock_digest = LocalAppsHostBroker::dependency_lock_digest(layout)
        .expect("fixture dependency lock digest");
    let snapshot_root = broker.dependency_snapshot_root(&lock_digest, PNPM_TOOLCHAIN_KEY);
    LocalAppsHostBroker::publish_dependency_snapshot(
        &workspace.join("node_modules"),
        &snapshot_root,
        &lock_digest,
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("publish fixture dependency snapshot");
    assert!(
        LocalAppsHostBroker::workspace_dependencies_match_snapshot(
            &workspace,
            &snapshot_root,
            &lock_digest,
            PNPM_TOOLCHAIN_KEY
        )
        .expect("validate fixture dependency snapshot"),
        "the synthetic QA fixture must not reinstall dependencies during the build"
    );
}

#[tokio::test]
async fn host_qa_roundtrip_publishes_only_after_terminal_commit_and_survives_stop() {
    let fixture = host_qa_fixture().await;
    let candidate = finalize_passing_host_qa(&fixture).await;
    assert_eq!(candidate["ok"], true, "canonical Host result must pass");
    assert_eq!(candidate["status"], "candidate");

    let session = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("load QA session for causal assertions");
    let writes = session
        .evidence
        .iter()
        .filter(|evidence| evidence.kind == local_apps::QaEvidenceKind::BridgeWrite)
        .collect::<Vec<_>>();
    let queries = session
        .evidence
        .iter()
        .filter(|evidence| evidence.kind == local_apps::QaEvidenceKind::Query)
        .collect::<Vec<_>>();
    assert_eq!(
        writes.len(),
        2,
        "one UI click should retain both page writes"
    );
    assert_eq!(
        queries.len(),
        2,
        "both collection queries should be retained"
    );
    for query in queries {
        let content = match local_apps::qa_read_evidence(
            &fixture.layout,
            &fixture.qa_handle,
            &query.evidence_id,
        )
        .expect("read query evidence")
        {
            local_apps::QaEvidenceBlock::Json { content, .. } => content,
            _ => panic!("query evidence must be JSON"),
        };
        let query_collection = content["records"][0]["collection"]
            .as_str()
            .expect("query collection");
        let caused_by = query.caused_by.as_deref().expect("query causal write");
        let write = writes
            .iter()
            .find(|write| write.event_id == caused_by)
            .expect("causal write exists");
        let write_content = match local_apps::qa_read_evidence(
            &fixture.layout,
            &fixture.qa_handle,
            &write.evidence_id,
        )
        .expect("read bridge evidence")
        {
            local_apps::QaEvidenceBlock::Json { content, .. } => content,
            _ => panic!("bridge evidence must be JSON"),
        };
        assert_eq!(
            write_content["results"][0]["collection"], query_collection,
            "query causality must follow the returned collection, not latest write"
        );
    }

    let capture_id = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("QA session remains readable through verifier")
        .evidence
        .into_iter()
        .find(|evidence| evidence.kind == local_apps::QaEvidenceKind::Capture)
        .expect("capture evidence")
        .evidence_id;
    let image = fixture
        .broker
        .qa_read_evidence(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "evidence_id": capture_id,
        }))
        .await
        .expect("verifier reads actual capture");
    assert_eq!(image["content"]["type"], "image");
    assert!(image["content"]["data"]
        .as_str()
        .is_some_and(|data| !data.is_empty()));

    assert_eq!(
        fixture
            .broker
            .qa_ui_verification_summary(&fixture.app_id)
            .await
            .status,
        LocalAppVerificationStatusDto::Unverified,
        "a passing candidate is not published proof"
    );
    let prepared = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result_for(&candidate),
        )
        .await
        .expect("prepare terminal publication");
    fixture
        .broker
        .commit_prepared_workflow_qa_publication(&prepared)
        .expect("commit published QA receipt");
    fixture
        .broker
        .stop_runtime(&fixture.app_id)
        .await
        .expect("stop runtime");
    assert_eq!(
        fixture
            .broker
            .qa_ui_verification_summary(&fixture.app_id)
            .await
            .status,
        LocalAppVerificationStatusDto::Passed,
        "stopping the same immutable build must not erase historical verification"
    );
    fixture
        .broker
        .emit_committed_workflow_qa_summary(&prepared)
        .await;
    assert!(
        local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle).is_err(),
        "post-terminal cleanup must remove raw session artifacts"
    );
    let emitted = fixture
        .sink
        .events()
        .await
        .into_iter()
        .find_map(|event| match event {
            ClientEvent::AppEvent {
                event:
                    AppEventDto::VerificationSummaryChanged {
                        app_id,
                        publication_state,
                        ui_verification,
                        ..
                    },
            } if app_id == fixture.app_id => Some((publication_state, ui_verification)),
            _ => None,
        })
        .expect("postcommit emits this app's verification summary");
    assert_eq!(emitted.0, AppWorkflowStateDto::PublishedVerified);
    assert_eq!(emitted.1.status, LocalAppVerificationStatusDto::Passed);
    assert_eq!(emitted.1.code.as_deref(), Some("ui_verification_passed"));
}

#[tokio::test]
async fn rebuilding_a_verified_no_mcp_app_emits_unverified_only_after_build_commit() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let fixture = host_qa_fixture_with_mobile_linux(Some(runtime.clone())).await;
    assert!(
        load_manifest(&fixture.layout)
            .expect("manifest")
            .active_mcp_catalog
            .is_none(),
        "the regression must exercise the usual no-MCP app path"
    );
    publish_passing_host_qa_and_emit(&fixture).await;
    let old_build_id = crate::mobile::local_apps_build::active_build_id(&fixture.layout)
        .expect("active build")
        .expect("published build id");
    let passed_event_count = fixture
        .sink
        .events()
        .await
        .iter()
        .filter(|event| {
            matches!(
                event,
                ClientEvent::AppEvent {
                    event: AppEventDto::VerificationSummaryChanged { app_id, .. }
                } if app_id == &fixture.app_id
            )
        })
        .count();

    sync_fixture_dependency_roots(&fixture.broker, &fixture.layout);
    runtime.set_fail_build(true);
    fixture
        .broker
        .build_app(json!({"app_id": fixture.app_id}))
        .await
        .expect_err("synthetic build failure");
    assert_eq!(
        crate::mobile::local_apps_build::active_build_id(&fixture.layout)
            .expect("active build after failure")
            .as_deref(),
        Some(old_build_id.as_str()),
        "a failed build must preserve the verified active receipt"
    );
    assert_eq!(
        fixture
            .broker
            .qa_ui_verification_summary(&fixture.app_id)
            .await
            .status,
        LocalAppVerificationStatusDto::Passed,
        "a failed build must preserve the previous immutable QA publication"
    );
    assert_eq!(
        fixture
            .sink
            .events()
            .await
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ClientEvent::AppEvent {
                        event: AppEventDto::VerificationSummaryChanged { app_id, .. }
                    } if app_id == &fixture.app_id
                )
            })
            .count(),
        passed_event_count,
        "a failed candidate must not invalidate the published client summary"
    );

    runtime.set_fail_build(false);
    fixture
        .broker
        .build_app(json!({"app_id": fixture.app_id}))
        .await
        .expect("commit replacement build");
    assert_ne!(
        crate::mobile::local_apps_build::active_build_id(&fixture.layout)
            .expect("replacement active build")
            .as_deref(),
        Some(old_build_id.as_str()),
        "the successful build must actually replace the identity under test"
    );
    let (publication_state, ui_verification) = fixture
        .sink
        .events()
        .await
        .into_iter()
        .rev()
        .find_map(|event| match event {
            ClientEvent::AppEvent {
                event:
                    AppEventDto::VerificationSummaryChanged {
                        app_id,
                        publication_state,
                        ui_verification,
                        ..
                    },
            } if app_id == fixture.app_id => Some((publication_state, ui_verification)),
            _ => None,
        })
        .expect("a committed no-MCP rebuild must refresh this app's summary");
    assert_eq!(publication_state, AppWorkflowStateDto::PublishedUnverified);
    assert_eq!(
        ui_verification.status,
        LocalAppVerificationStatusDto::Unverified
    );
    assert_eq!(
        ui_verification.code.as_deref(),
        Some("ui_verification_required")
    );
}

#[tokio::test]
async fn qa_terminal_uses_host_canonical_fields_and_rejects_restart_before_commit() {
    let fixture = host_qa_fixture().await;
    let candidate = finalize_passing_host_qa(&fixture).await;
    let mut untrusted = workflow_result_for(&candidate);
    untrusted["app_id"] = Value::String("foreign-app".into());
    untrusted["workflow_run_id"] = Value::String("foreign-run".into());
    untrusted["findings"] = json!([{"id": "invented", "blocking": false}]);
    untrusted["verification"] = json!({"status": "model-claimed-passed"});
    let prepared = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            untrusted,
        )
        .await
        .expect("prepare trusted terminal result");
    let canonical = prepared.canonical_result();
    assert_eq!(canonical["app_id"], fixture.app_id);
    assert_eq!(canonical["workflow_run_id"], fixture.workflow_run_id);
    assert_eq!(canonical["findings"], json!([]));
    assert_eq!(
        canonical["verification"]["receipt"]["receipt_id"],
        candidate["receipt"]["receipt_id"]
    );
    assert_eq!(
        canonical["verification"]["result"]["identity"]["qa_handle"],
        fixture.qa_handle
    );
    assert_eq!(
        canonical["workflow_result_diagnostic"]["app_id"],
        "foreign-app"
    );

    let old_generation = fixture
        .broker
        .runtime_identity(&fixture.app_id)
        .await
        .expect("runtime identity")
        .expect("running identity")
        .0;
    fixture
        .broker
        .stop_runtime(&fixture.app_id)
        .await
        .expect("stop before terminal commit");
    fixture
        .broker
        .start_runtime(&fixture.app_id)
        .await
        .expect("restart before terminal commit");
    let new_generation = fixture
        .broker
        .runtime_identity(&fixture.app_id)
        .await
        .expect("runtime identity")
        .expect("restarted identity")
        .0;
    assert_ne!(old_generation, new_generation);
    let error = fixture
        .broker
        .commit_prepared_workflow_qa_publication(&prepared)
        .expect_err("a restarted runtime invalidates an in-flight prepared candidate");
    assert!(error.contains("runtime generation"), "{error}");
    let summary = fixture
        .broker
        .qa_ui_verification_summary(&fixture.app_id)
        .await;
    assert_eq!(summary.status, LocalAppVerificationStatusDto::Unverified);
    assert_eq!(summary.code.as_deref(), Some("ui_verification_required"));
}

#[tokio::test]
async fn host_qa_terminal_rejects_wrong_scope_and_every_stale_identity() {
    let fixture = host_qa_fixture().await;
    let candidate = finalize_passing_host_qa(&fixture).await;
    let workflow_result = workflow_result_for(&candidate);
    let receipt_id = candidate["receipt"]["receipt_id"]
        .as_str()
        .expect("receipt id");

    let wrong_app = fixture
        .broker
        .prepare_workflow_qa_outcome(
            "zzzzzzzz",
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result.clone(),
        )
        .await
        .expect_err("foreign app must not resolve this receipt");
    assert!(wrong_app.contains("receipt"), "{wrong_app}");
    let wrong_run = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            "other-run",
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result.clone(),
        )
        .await
        .expect_err("foreign workflow run must fail");
    assert!(wrong_run.contains("workflow run mismatch"), "{wrong_run}");
    let wrong_strategy = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Thorough,
            workflow_result.clone(),
        )
        .await
        .expect_err("authenticated strategy mismatch must fail");
    assert!(
        wrong_strategy.contains("verification strategy"),
        "{wrong_strategy}"
    );
    let missing_receipt = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            json!({"receipt_id": "qa-missing"}),
        )
        .await
        .expect_err("unknown receipt must fail");
    assert!(missing_receipt.contains("receipt"), "{missing_receipt}");

    let original_generation = {
        let mut runtimes = fixture.broker.runtimes.lock().await;
        let entry = runtimes.get_mut(&fixture.app_id).expect("runtime entry");
        let original = entry.generation;
        entry.generation = original.saturating_add(1);
        original
    };
    let stale_generation = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result.clone(),
        )
        .await
        .expect_err("changed runtime generation must fail");
    assert!(
        stale_generation.contains("stale_runtime"),
        "{stale_generation}"
    );
    fixture
        .broker
        .runtimes
        .lock()
        .await
        .get_mut(&fixture.app_id)
        .expect("runtime entry")
        .generation = original_generation;

    let build_path = fixture
        .layout
        .root()
        .join(fixture.layout.build_rel(false))
        .join("build.json");
    let original_build = fs::read(&build_path).expect("build receipt");
    let mut stale_build: Value =
        serde_json::from_slice(&original_build).expect("parse build receipt");
    stale_build["buildId"] = Value::String("new-build".into());
    fs::write(
        &build_path,
        serde_json::to_vec_pretty(&stale_build).expect("serialize stale build"),
    )
    .expect("replace build identity");
    let wrong_build = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result.clone(),
        )
        .await
        .expect_err("changed active build must fail");
    assert!(wrong_build.contains("stale_build"), "{wrong_build}");
    fs::write(&build_path, &original_build).expect("restore build receipt");

    let manifest = load_manifest(&fixture.layout).expect("manifest");
    let mut changed_manifest = manifest.clone();
    changed_manifest
        .dependency_snapshot
        .as_mut()
        .expect("dependency snapshot")
        .requested_sha256 = "a".repeat(64);
    local_apps::save_manifest(&fixture.layout, &changed_manifest)
        .expect("save changed dependency identity");
    let wrong_dependency = fixture
        .broker
        .prepare_workflow_qa_outcome(
            &fixture.app_id,
            &fixture.workflow_run_id,
            local_apps::QaVerificationStrategy::Balanced,
            workflow_result,
        )
        .await
        .expect_err("changed dependency snapshot must fail");
    assert!(
        wrong_dependency.contains("stale_manifest"),
        "{wrong_dependency}"
    );
    local_apps::save_manifest(&fixture.layout, &manifest).expect("restore manifest");
    assert_eq!(
        local_apps::load_qa_receipt(&fixture.layout, receipt_id)
            .expect("original receipt remains immutable")
            .receipt_id,
        receipt_id
    );
}

#[tokio::test]
async fn cancelled_qa_action_keeps_real_write_but_drops_ui_evidence() {
    let fixture = host_qa_fixture().await;
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
    });
    let event_id = fixture
        .broker
        .begin_qa_action(&input)
        .await
        .expect("begin action")
        .expect("QA action id");
    fixture
        .broker
        .mutate_data_value(
            json!({
                "app_id": fixture.app_id,
                "collection": "chores",
                "operations": [{"kind": "upsert", "recordId": "cancelled", "document": {"title": "Cancelled"}}],
            }),
            false,
            Some(&event_id),
        )
        .await
        .expect("perform page write");
    let direct_result = fixture
        .broker
        .mutate_data_value(
            json!({
                "app_id": fixture.app_id,
                "qa_handle": fixture.qa_handle,
                "scenario_id": "primary-action",
                "target_id": "primary",
                "collection": "chores",
                "operations": [{"kind": "upsert", "recordId": "seed", "document": {"title": "Seed"}}],
            }),
            false,
            None,
        )
        .await
        .expect("direct seed mutation");
    assert_eq!(direct_result["results"][0]["recordId"], "seed");
    assert_eq!(
        fixture
            .broker
            .qa_inflight_actions
            .lock()
            .await
            .get(&fixture.app_id)
            .expect("active action")
            .pending_bridge_results
            .len(),
        1,
        "a direct/background mutation during the window must not be laundered into UI evidence"
    );
    fixture
        .broker
        .end_qa_action(&fixture.app_id, &event_id)
        .await;
    let after_cancel = fixture
        .broker
        .query_data_value(json!({"app_id": fixture.app_id, "collection": "chores"}))
        .await
        .expect("query after cancel");
    let record_ids = after_cancel["records"]
        .as_array()
        .expect("records")
        .iter()
        .filter_map(|record| record["recordId"].as_str())
        .collect::<HashSet<_>>();
    assert_eq!(record_ids, HashSet::from(["cancelled", "seed"]));
    let session =
        local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle).expect("QA session");
    assert!(
        session
            .evidence
            .iter()
            .all(|evidence| evidence.kind != local_apps::QaEvidenceKind::BridgeWrite),
        "direct mutation must not be recorded as a page bridge write"
    );
}

#[tokio::test]
async fn real_qa_ui_roundtrip_returns_only_callable_host_evidence_ids() {
    let fixture = host_qa_fixture().await;
    fixture
        .broker
        .session_permissions
        .lock()
        .await
        .grant(&fixture.app_id, AppCapability::UiControl);
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
        "action": "click",
        "target": {"element_id": "submit"},
    });
    let action_task = tokio::spawn({
        let broker = fixture.broker.clone();
        let input = input.clone();
        async move { broker.act_on_ui(input).await }
    });
    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) =
                fixture
                    .sink
                    .events()
                    .await
                    .into_iter()
                    .find_map(|event| match event {
                        ClientEvent::AppEvent {
                            event: AppEventDto::AppUiRequest { request },
                        } if request.app_id == fixture.app_id => Some(request.request_id),
                        _ => None,
                    })
            {
                break request_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real act_on_ui reaches its native request");

    let bridge_result = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "real-roundtrip-write".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "chores",
                    "operations": [{
                        "kind": "upsert",
                        "recordId": "real-roundtrip-row",
                        "document": {"title": "Persisted"},
                    }],
                })
                .to_string(),
            ),
        })
        .await
        .expect("real page bridge write");
    assert_eq!(
        bridge_result["results"][0]["recordId"],
        "real-roundtrip-row"
    );
    let runtime = fixture
        .broker
        .service()
        .expect("service")
        .runtime_record(&fixture.app_id)
        .await
        .expect("runtime record");
    let runtime_url = crate::mobile::local_apps_bridge::runtime_preview_url(&runtime)
        .expect("runtime preview URL");
    let mut loaded_url = url::Url::parse(&runtime_url).expect("runtime URL");
    loaded_url.set_path("/business/chores");
    loaded_url
        .query_pairs_mut()
        .append_pair("selected", "real-roundtrip-row");
    loaded_url.set_fragment(Some("detail"));
    assert!(
        fixture
            .broker
            .resolve_ui(
                &request_id,
                AppAuthorizationDecisionDto::AllowOnce,
                Some(
                    json!({
                        "lingxi_qa": {
                            "version": 1,
                            "requested_runtime_url": runtime_url,
                            "loaded_runtime_url": loaded_url,
                            "platform": "ios",
                            "form_factor": "iphone",
                            "navigation_generation": 1,
                            "device_model": "iPhone fixture",
                        },
                        "result": {
                            "clicked": true,
                            "qa_handle": "qa_spoofed",
                            "qa_evidence_ids": ["qa-evidence-spoofed"],
                        },
                    })
                    .to_string(),
                ),
                None,
            )
            .await,
        "native response resolves the real tool request"
    );
    let action_result = action_task
        .await
        .expect("join real QA action")
        .expect("real QA action succeeds");
    assert_eq!(action_result["qa_handle"], fixture.qa_handle);
    let action_evidence_ids = action_result["qa_evidence_ids"]
        .as_array()
        .expect("action evidence ids")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        action_evidence_ids.len(),
        3,
        "native provenance, UI action and bridge write must all be returned"
    );
    assert!(
        !action_evidence_ids.contains(&"qa-evidence-spoofed"),
        "native/app-controlled metadata must not enter the Host evidence envelope"
    );
    for evidence_id in &action_evidence_ids {
        fixture
            .broker
            .qa_read_evidence(json!({
                "app_id": fixture.app_id,
                "qa_handle": fixture.qa_handle,
                "evidence_id": evidence_id,
            }))
            .await
            .unwrap_or_else(|error| {
                panic!("returned action evidence {evidence_id:?} must be readable: {error}")
            });
    }
    let kinds = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("QA session")
        .evidence
        .into_iter()
        .filter(|evidence| action_evidence_ids.contains(&evidence.evidence_id.as_str()))
        .map(|evidence| evidence.kind)
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&local_apps::QaEvidenceKind::NativeTargetProvenance)
            && kinds.contains(&local_apps::QaEvidenceKind::UiAction)
            && kinds.contains(&local_apps::QaEvidenceKind::BridgeWrite),
        "returned IDs must cover native, action and bridge evidence; got {kinds:?}"
    );

    let query_result = fixture
        .broker
        .query_data_value(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_id": "primary-action",
            "target_id": "primary",
            "collection": "chores",
            "filters": [{
                "fieldId": "title",
                "operator": "equal",
                "value": "Persisted",
            }],
        }))
        .await
        .expect("real QA query");
    assert_eq!(query_result["records"][0]["recordId"], "real-roundtrip-row");
    assert_eq!(query_result["qa_handle"], fixture.qa_handle);
    let query_evidence_ids = query_result["qa_evidence_ids"]
        .as_array()
        .expect("query evidence ids");
    assert_eq!(query_evidence_ids.len(), 1);
    fixture
        .broker
        .qa_read_evidence(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "evidence_id": query_evidence_ids[0],
        }))
        .await
        .expect("returned query evidence is readable");

    let mut routed_request = url::Url::parse(&runtime_url).expect("runtime URL");
    routed_request.set_path("/must-not-be-requested");
    let wrong_route = fixture
        .broker
        .qa_ui_response_value(
            &input,
            json!({
                "lingxi_qa": {
                    "version": 1,
                    "requested_runtime_url": routed_request,
                    "loaded_runtime_url": loaded_url,
                    "platform": "ios",
                    "form_factor": "iphone",
                    "navigation_generation": 2,
                },
                "result": {},
            }),
        )
        .await
        .expect_err("the Host-requested URL must remain strict and origin-rooted");
    assert!(
        wrong_route.contains("native_attestation_invalid"),
        "{wrong_route}"
    );

    let mut wrong_marker = loaded_url.clone();
    wrong_marker.set_query(Some("lingxi_runtime=999999"));
    let wrong_marker = fixture
        .broker
        .qa_ui_response_value(
            &input,
            json!({
                "lingxi_qa": {
                    "version": 1,
                    "requested_runtime_url": runtime_url,
                    "loaded_runtime_url": wrong_marker,
                    "platform": "ios",
                    "form_factor": "iphone",
                    "navigation_generation": 3,
                },
                "result": {},
            }),
        )
        .await
        .expect_err("the loaded route must retain the requested runtime marker");
    assert!(
        wrong_marker.contains("not the requested runtime generation"),
        "{wrong_marker}"
    );
}

#[tokio::test]
async fn dropped_real_act_on_ui_future_deactivates_attribution_and_keeps_page_write() {
    let fixture = host_qa_fixture().await;
    fixture
        .broker
        .session_permissions
        .lock()
        .await
        .grant(&fixture.app_id, AppCapability::UiControl);
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
        "action": "click",
        "target": {"element_id": "submit"},
    });
    let action_task = tokio::spawn({
        let broker = fixture.broker.clone();
        async move { broker.act_on_ui(input).await }
    });
    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) =
                fixture
                    .sink
                    .events()
                    .await
                    .into_iter()
                    .find_map(|event| match event {
                        ClientEvent::AppEvent {
                            event: AppEventDto::AppUiRequest { request },
                        } if request.app_id == fixture.app_id => Some(request.request_id),
                        _ => None,
                    })
            {
                break request_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real act_on_ui reaches its native request");
    assert!(
        fixture
            .broker
            .qa_active_actions
            .lock()
            .expect("active action map")
            .contains_key(&fixture.app_id),
        "the real tool future must own an active attribution window while native UI is pending"
    );

    let bridge_result = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "cancelled-tool-write".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "chores",
                    "operations": [{
                        "kind": "upsert",
                        "recordId": "cancelled-tool-row",
                        "document": {"title": "Still saved"},
                    }],
                })
                .to_string(),
            ),
        })
        .await
        .expect("page write completes while native UI is pending");
    assert_eq!(
        bridge_result["results"][0]["recordId"],
        "cancelled-tool-row"
    );

    action_task.abort();
    assert!(action_task
        .await
        .expect_err("aborted tool future must not complete")
        .is_cancelled());
    assert!(
        !fixture
            .broker
            .qa_active_actions
            .lock()
            .expect("active action map")
            .contains_key(&fixture.app_id),
        "dropping the actual tool future must synchronously close attribution"
    );
    assert!(
        !fixture
            .broker
            .resolve_ui(
                &request_id,
                AppAuthorizationDecisionDto::AllowOnce,
                Some("{}".into()),
                None,
            )
            .await,
        "the dropped tool receiver must reject and remove its late native response"
    );
    let stored = fixture
        .broker
        .query_data_value(json!({"app_id": fixture.app_id, "collection": "chores"}))
        .await
        .expect("read after cancelled tool");
    assert_eq!(stored["records"][0]["recordId"], "cancelled-tool-row");
    assert!(
        local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
            .expect("QA session")
            .evidence
            .iter()
            .all(|evidence| evidence.kind != local_apps::QaEvidenceKind::BridgeWrite),
        "cancelled native UI must drop only pending evidence attribution"
    );

    let restarted = fixture
        .broker
        .begin_qa_action(&json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_id": "primary-action",
            "target_id": "primary",
        }))
        .await
        .expect("restart after dropping real tool future")
        .expect("restarted QA action");
    fixture
        .broker
        .end_qa_action(&fixture.app_id, &restarted)
        .await;
}

#[tokio::test]
async fn qa_attribution_overflow_does_not_change_page_write_results_or_errors() {
    let fixture = host_qa_fixture().await;
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
    });
    let event_id = fixture
        .broker
        .begin_qa_action(&input)
        .await
        .expect("begin action")
        .expect("QA action id");
    fixture
        .broker
        .qa_inflight_actions
        .lock()
        .await
        .get_mut(&fixture.app_id)
        .expect("active action")
        .pending_bridge_results =
        vec![json!({"already": "attributed"}); local_apps::MAX_MUTATION_BATCH_SIZE];

    let bridge_result = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "overflow-write".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "chores",
                    "operations": [{
                        "kind": "upsert",
                        "recordId": "overflow-row",
                        "document": {"title": "Overflow still saves"},
                    }],
                })
                .to_string(),
            ),
        })
        .await
        .expect("attribution overflow must not replace the business response");
    assert_eq!(bridge_result["results"][0]["recordId"], "overflow-row");
    assert_eq!(bridge_result["results"][0]["revision"], 1);
    let stored = fixture
        .broker
        .query_data_value(json!({"app_id": fixture.app_id, "collection": "chores"}))
        .await
        .expect("overflow write remains readable");
    assert_eq!(stored["records"][0]["recordId"], "overflow-row");

    let invalid = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "overflow-invalid".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "missing",
                    "operations": [{"kind": "delete", "recordId": "nope"}],
                })
                .to_string(),
            ),
        })
        .await
        .expect_err("a real datastore error must not be hidden by QA attribution");
    assert!(
        invalid.message.contains("collection"),
        "expected the datastore error, got: {}",
        invalid.message
    );
    fixture
        .broker
        .end_qa_action(&fixture.app_id, &event_id)
        .await;
}

#[tokio::test]
async fn dropped_qa_action_guard_deactivates_window_and_allows_restart() {
    let fixture = host_qa_fixture().await;
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
    });
    let Some((first_event, guard)) = fixture
        .broker
        .begin_qa_action_with_guard(&input)
        .await
        .expect("begin guarded QA action")
    else {
        panic!("QA fixture must return an action window");
    };
    drop(guard);

    let second_event = fixture
        .broker
        .begin_qa_action(&input)
        .await
        .expect("restart after cancellation")
        .expect("restarted QA action");
    assert_ne!(first_event, second_event);
    fixture
        .broker
        .end_qa_action(&fixture.app_id, &second_event)
        .await;
}

#[tokio::test]
async fn qa_action_waits_for_a_page_write_issued_after_native_success() {
    let fixture = host_qa_fixture().await;
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
    });
    let event_id = fixture
        .broker
        .begin_qa_action(&input)
        .await
        .expect("begin action")
        .expect("QA action id");

    // Native reports successful click evaluation before the handler's
    // asynchronously scheduled bridge mutation has entered the Host.
    let settlement = tokio::spawn({
        let broker = fixture.broker.clone();
        let app_id = fixture.app_id.clone();
        let event_id = event_id.clone();
        async move { broker.settle_qa_action(&app_id, &event_id).await }
    });
    sleep(Duration::from_millis(75)).await;
    let first_bridge = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "late-ui-read".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::QueryData,
            payload_json: Some(json!({"collection": "chores"}).to_string()),
        })
        .await
        .expect("first awaited bridge request remains inside action window");
    assert_eq!(first_bridge["records"], json!([]));
    // This begins after the original native-success grace. The preceding
    // bridge activity is what keeps the chained handler window alive.
    sleep(Duration::from_millis(75)).await;
    let bridge_result = fixture
        .broker
        .execute_bridge_inner(&AppBridgeRequestDto {
            request_id: "late-ui-write".into(),
            app_id: fixture.app_id.clone(),
            operation: AppBridgeOperationDto::MutateData,
            payload_json: Some(
                json!({
                    "collection": "chores",
                    "operations": [{
                        "kind": "upsert",
                        "recordId": "late-row",
                        "document": {"title": "Late sweep"},
                    }],
                })
                .to_string(),
            ),
        })
        .await
        .expect("late page bridge write remains inside bounded action window");
    assert_eq!(bridge_result["results"][0]["recordId"], "late-row");
    let before_commit = fixture
        .broker
        .query_data_value(json!({"app_id": fixture.app_id, "collection": "chores"}))
        .await
        .expect("query before native success commit");
    assert_eq!(before_commit["records"][0]["recordId"], "late-row");

    let action = settlement
        .await
        .expect("join QA settlement")
        .expect("settle QA action");
    assert_eq!(action.pending_bridge_results.len(), 1);
    fixture
        .broker
        .record_qa_observation(
            &input,
            "act_on_ui",
            json!({"clicked": true}),
            event_id,
            None,
        )
        .await
        .expect("record successful action");
    fixture
        .broker
        .commit_qa_action_mutations(&fixture.app_id, &action)
        .await
        .expect("commit late page write after native success");
    let changed = fixture
        .broker
        .query_data_value(json!({
            "app_id": fixture.app_id,
            "qa_handle": fixture.qa_handle,
            "scenario_id": "primary-action",
            "target_id": "primary",
            "collection": "chores",
            "filters": [{"fieldId": "title", "operator": "equal", "value": "Late sweep"}],
        }))
        .await
        .expect("query actual changed row");
    assert_eq!(changed["records"].as_array().map(Vec::len), Some(1));
    assert_eq!(changed["records"][0]["collection"], "chores");
    assert_eq!(changed["records"][0]["recordId"], "late-row");
    assert_eq!(changed["records"][0]["revision"], 1);

    let session = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("load QA evidence");
    let bridge = session
        .evidence
        .iter()
        .find(|evidence| evidence.kind == local_apps::QaEvidenceKind::BridgeWrite)
        .expect("actual bridge evidence");
    let bridge_id = bridge.evidence_id.clone();
    let bridge_event_id = bridge.event_id.clone();
    let bridge_content =
        match local_apps::qa_read_evidence(&fixture.layout, &fixture.qa_handle, &bridge_id)
            .expect("read actual bridge artifact")
        {
            local_apps::QaEvidenceBlock::Json { content, .. } => content,
            other => panic!("bridge evidence must be JSON, got {other:?}"),
        };
    assert_eq!(bridge_content["results"][0]["collection"], "chores");
    assert_eq!(bridge_content["results"][0]["recordId"], "late-row");
    assert_eq!(bridge_content["results"][0]["revision"], 1);
    let query = session
        .evidence
        .iter()
        .find(|evidence| evidence.kind == local_apps::QaEvidenceKind::Query)
        .expect("actual query evidence");
    assert_eq!(query.caused_by.as_deref(), Some(bridge_event_id.as_str()));
    let query_content = match local_apps::qa_read_evidence(
        &fixture.layout,
        &fixture.qa_handle,
        &query.evidence_id,
    )
    .expect("read actual query artifact")
    {
        local_apps::QaEvidenceBlock::Json { content, .. } => content,
        other => panic!("query evidence must be JSON, got {other:?}"),
    };
    assert_eq!(query_content["records"][0]["recordId"], "late-row");
}

#[tokio::test]
async fn authoring_candidate_run_and_base_are_rechecked_at_the_build_lock_boundary() {
    let fixture = host_qa_fixture().await;
    let base = crate::mobile::local_apps_build::active_build_authoring_contract_sha256(
        &fixture.layout,
    )
    .expect("active authoring selector")
    .expect("active contract digest");
    let spec: local_apps::AppAuthoringSpec = serde_json::from_str(include_str!(
        "../../../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
    ))
    .expect("authoring fixture");
    let staged = fixture
        .broker
        .local_app_contract(json!({
            "operation": "stage",
            "app_id": fixture.app_id,
            "workflow_run_id": "workflow-update",
            "base_contract_sha256": base,
            "spec": spec,
        }))
        .await
        .expect("stage update contract");
    let handle = staged["contract_handle"].as_str().expect("contract handle");
    let wrong_run = fixture
        .broker
        .authoring_contract_for_build(
            &fixture.layout,
            &json!({"contract_handle": handle, "workflow_run_id": "other-run"}),
        )
        .expect_err("candidate handle must remain workflow-bound");
    assert!(
        wrong_run.contains("workflow binding mismatch"),
        "{wrong_run}"
    );

    let build_path = fixture
        .layout
        .root()
        .join(fixture.layout.build_rel(false))
        .join("build.json");
    let mut build: Value = serde_json::from_slice(&fs::read(&build_path).expect("receipt"))
        .expect("parse receipt");
    build["authoringContractSha256"] = Value::String("f".repeat(64));
    fs::write(
        &build_path,
        serde_json::to_vec_pretty(&build).expect("serialize receipt"),
    )
    .expect("change active contract selector");
    let stale = fixture
        .broker
        .verify_authoring_candidate_digest(
            &fixture.layout,
            staged["contract_sha256"]
                .as_str()
                .expect("candidate digest"),
        )
        .expect_err("candidate base changed before locked build");
    assert!(
        stale.contains("candidate base no longer matches"),
        "{stale}"
    );
}

/// r3-never-wired-05: `LocalAppVerificationStatusDto::Failed` had zero
/// producers while both clients carried localized copy for it, and the
/// condition that should have produced it — an active MCP catalog whose
/// recorded identity does not match the app and build pointing at it —
/// instead did `return Err(...)` out of `emit_managed_mcp_inventory`.
/// That aborted the WHOLE listing: one corrupt app made every other app
/// disappear from both clients' Apps surfaces.
#[tokio::test]
async fn a_corrupt_active_mcp_catalog_fails_only_that_app_and_still_lists_the_others() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker =
        LocalAppsHostBroker::new(root.path().to_path_buf(), sink.clone(), None, false, None);
    assert!(broker.attach_service(service.clone()).is_ok());
    let healthy = create_app_fixture(&root, &service, "Healthy").await;
    let corrupt = create_app_fixture(&root, &service, "Corrupt").await;
    publish_fixture_build(&root, &healthy, "healthy-build");
    let layout = publish_fixture_build(&root, &corrupt, "corrupt-build");

    // The plant: a catalog that records SOMEBODY ELSE's app id, published
    // as this app's active catalog.
    let catalog = json!({
        "appId": "not-this-app",
        "buildId": "corrupt-build",
        "tools": [],
        "execution": [],
    });
    let catalog_sha256 =
        local_apps::approval_contract_sha256(catalog.clone()).expect("catalog digest");
    local_apps::save_mcp_catalog(&layout, &catalog_sha256, &catalog).expect("save catalog");
    let mut manifest = load_manifest(&layout).expect("fixture manifest");
    if manifest.revision == 0 {
        manifest.revision = 1;
    }
    manifest.active_mcp_catalog = Some(local_apps::AppMcpCatalogRef {
        build_id: "corrupt-build".into(),
        manifest_revision: manifest.revision,
        authoring_revision: 1,
        user_goal_sha256: "0".repeat(64),
        proposal_sha256: "0".repeat(64),
        approval_contract_sha256: "0".repeat(64),
        tool_surface_sha256: "0".repeat(64),
        catalog_sha256: catalog_sha256.clone(),
        mcp_verification_sha256: "0".repeat(64),
    });
    local_apps::save_manifest(&layout, &manifest).expect("publish the corrupt catalog pointer");

    broker
        .emit_managed_mcp_inventory()
        .await
        .expect("one app's corrupt catalog must not fail the whole inventory listing");

    let servers = sink
        .events()
        .await
        .into_iter()
        .rev()
        .find_map(|event| match event {
            ClientEvent::AppEvent {
                event: AppEventDto::ManagedMcpInventoryChanged { servers },
            } => Some(servers),
            _ => None,
        })
        .expect("a managed MCP inventory event");
    assert_eq!(
        servers.len(),
        2,
        "both apps must still be listed; the corrupt one must not take the healthy one \
         down with it: {servers:?}"
    );
    let corrupt_row = servers
        .iter()
        .find(|server| server.app_id == corrupt)
        .expect("the corrupt app is listed");
    assert_eq!(
        corrupt_row.mcp_verification.status,
        LocalAppVerificationStatusDto::Failed,
        "the corrupt app must be reported Failed -- this is the production producer for a \
         variant both clients render: {:?}",
        corrupt_row.mcp_verification
    );
    assert_eq!(
        corrupt_row.mcp_verification.code.as_deref(),
        Some("active_state_corrupt"),
        "a Failed summary must carry a code: `code == None` is the arm both clients map to \
         'verification passed', so a failure must never reach it: {:?}",
        corrupt_row.mcp_verification
    );
    assert!(
        corrupt_row.tools.is_empty() && corrupt_row.catalog_sha256.is_empty(),
        "nothing derived from an untrusted catalog may be presented: {corrupt_row:?}"
    );
    assert_eq!(
        corrupt_row.status,
        ManagedLocalAppMcpStatusDto::Error,
        "the corrupt app's MCP status: {corrupt_row:?}"
    );
    let healthy_row = servers
        .iter()
        .find(|server| server.app_id == healthy)
        .expect("the healthy app is listed");
    assert_eq!(
        healthy_row.mcp_verification.status,
        LocalAppVerificationStatusDto::Unverified,
        "the healthy app keeps its own (no-catalog) summary: {healthy_row:?}"
    );
}

/// r1-never-wired-06: `pending_verification_gates` was a hardcoded pair,
/// so a `create_without_mcp` confirmation — an app that will have no MCP
/// surface at all — still promised the user an MCP QA gate that nothing
/// would ever run for it. The `ui_runner` row stays unconditional because
/// it is unconditionally true on this Host.
///
/// Both ids are live client contracts: iOS switches on them in
/// `LocalAppApprovalSheets.swift`'s `localizedGateLabel` /
/// `localizedGateDetail` to render localized copy in place of the English
/// `label`/`detail` sent from here.
#[test]
fn pending_verification_gates_promises_mcp_qa_only_when_mcp_tools_are_being_approved() {
    let without_mcp = LocalAppsHostBroker::pending_verification_gates(0);
    assert!(
        without_mcp.iter().all(|gate| gate.gate_id != "mcp_qa"),
        "an approval with no proposed tool must not promise an MCP QA gate: {without_mcp:?}"
    );
    assert_eq!(
        without_mcp
            .iter()
            .filter(|gate| gate.gate_id == "ui_runner")
            .count(),
        1,
        "the ui_runner row is unconditional: {without_mcp:?}"
    );
    let with_mcp = LocalAppsHostBroker::pending_verification_gates(2);
    assert!(
        with_mcp.iter().any(|gate| gate.gate_id == "mcp_qa"),
        "an approval that IS granting tools must still promise MCP QA: {with_mcp:?}"
    );
    for gate in &with_mcp {
        assert!(
            matches!(gate.gate_id.as_str(), "mcp_qa" | "ui_runner"),
            "`{}` is a gate_id no client localizes; add it to iOS's localizedGateLabel (and \
             the Android equivalent) in the same change: {gate:?}",
            gate.gate_id
        );
    }
}

#[tokio::test]
async fn failed_native_qa_action_is_authenticated_and_persisted_without_roundtrip_credit() {
    let fixture = host_qa_fixture().await;
    fixture
        .broker
        .session_permissions
        .lock()
        .await
        .grant(&fixture.app_id, AppCapability::UiControl);
    let input = json!({
        "app_id": fixture.app_id,
        "qa_handle": fixture.qa_handle,
        "scenario_id": "primary-action",
        "target_id": "primary",
        "action": "click",
        "target": {"element_id": "submit"},
    });
    let action_task = tokio::spawn({
        let broker = fixture.broker.clone();
        let input = input.clone();
        async move { broker.act_on_ui(input).await }
    });
    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) =
                fixture
                    .sink
                    .events()
                    .await
                    .into_iter()
                    .find_map(|event| match event {
                        ClientEvent::AppEvent {
                            event: AppEventDto::AppUiRequest { request },
                        } if request.app_id == fixture.app_id => Some(request.request_id),
                        _ => None,
                    })
            {
                break request_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed QA action reaches its native request");
    let runtime = fixture
        .broker
        .service()
        .expect("service")
        .runtime_record(&fixture.app_id)
        .await
        .expect("runtime record");
    let runtime_url = crate::mobile::local_apps_bridge::runtime_preview_url(&runtime)
        .expect("runtime preview URL");
    assert!(
        fixture
            .broker
            .resolve_ui(
                &request_id,
                AppAuthorizationDecisionDto::AllowOnce,
                Some(
                    json!({
                        "lingxi_qa": {
                            "version": 1,
                            "requested_runtime_url": runtime_url,
                            "loaded_runtime_url": runtime_url,
                            "platform": "ios",
                            "form_factor": "iphone",
                            "navigation_generation": 1,
                            "device_model": "iPhone fixture",
                        },
                        "result": {"ok": false, "error": "submit rejected by app"},
                    })
                    .to_string(),
                ),
                None,
            )
            .await,
        "native error envelope resolves the real tool request"
    );
    let action_result = action_task
        .await
        .expect("join failed QA action")
        .expect("authenticated failure remains a structured result");
    assert_eq!(action_result["ok"], false);
    assert_eq!(action_result["error"], "submit rejected by app");
    let evidence_ids = action_result["qa_evidence_ids"]
        .as_array()
        .expect("failed action evidence IDs");
    assert!(
        evidence_ids.len() >= 2,
        "native provenance and failed UI evidence"
    );
    let session = local_apps::load_qa_session(&fixture.layout, &fixture.qa_handle)
        .expect("QA session remains readable");
    let ui = session
        .evidence
        .iter()
        .find(|evidence| evidence.kind == local_apps::QaEvidenceKind::UiAction)
        .expect("failed UI attempt is persisted");
    let content = match local_apps::qa_read_evidence(
        &fixture.layout,
        &fixture.qa_handle,
        &ui.evidence_id,
    )
    .expect("read failed UI evidence")
    {
        local_apps::QaEvidenceBlock::Json { content, .. } => content,
        _ => panic!("UI evidence must be JSON"),
    };
    assert_eq!(content["ok"], false);
}

/// r3-failure-paths-02: a native approval was announced exactly once, so
/// an Android client whose Activity was destroyed while the engine stayed
/// alive headlessly lost the sheet with no way to get it back — the engine
/// then blocked for the whole five-minute `APPROVAL_TIMEOUT` and failed
/// the workflow. `reemit_pending_native_approvals` is the reattach path,
/// wired into `PluginCommandDto::GetManagedMcpInventory` (the snapshot
/// command both clients already send when they bind).
#[tokio::test]
async fn a_pending_native_approval_is_re_announced_verbatim_to_a_reattaching_client() {
    let root = TempDir::new().expect("tempdir");
    let sink = MockSink::arc();
    let broker =
        LocalAppsHostBroker::new(root.path().to_path_buf(), sink.clone(), None, false, None);
    // The fixture event carries the request id, so "the same sheet came
    // back" is checked on the wire content, not on a count alone.
    let event = |request_id: &str| AppEventDto::LocalAppOperationFailed {
        app_id: Some("app-reattach".into()),
        code: client_protocol::local_apps::LocalAppPluginErrorCodeDto::PluginDisabled,
        message: "r3-failure-paths-02 fixture event; only its request_id is under test".into(),
        request_id: Some(request_id.to_string()),
    };
    let announcements = |events: Vec<ClientEvent>| {
        events
            .into_iter()
            .filter(|emitted| {
                matches!(
                    emitted,
                    ClientEvent::AppEvent {
                        event: AppEventDto::LocalAppOperationFailed {
                            request_id: Some(request_id),
                            ..
                        },
                    } if request_id == "req-reattach"
                )
            })
            .count()
    };

    let wait = broker.wait_for_native_approval_with_timeout(
        &broker.pending_create_confirmations,
        "req-reattach".into(),
        "app-reattach",
        event("req-reattach"),
        Duration::from_secs(30),
    );
    tokio::pin!(wait);
    // Park the wait past its insert-and-emit, short of its deadline.
    assert!(
        timeout(Duration::from_millis(50), &mut wait).await.is_err(),
        "the approval must still be pending for the reattach path to have anything to re-emit"
    );
    assert_eq!(
        announcements(sink.events().await),
        1,
        "the original announcement"
    );

    broker.reemit_pending_native_approvals().await;
    assert_eq!(
        announcements(sink.events().await),
        2,
        "a reattaching client must be handed the pending sheet again, with the SAME \
         request_id its answer has to carry"
    );

    assert!(
        broker
            .resolve_create_confirmation("req-reattach", true)
            .await,
        "the re-announced request id must still resolve the wait it names"
    );
    assert_eq!(wait.await, Ok(true), "the parked wait settles");
    broker.reemit_pending_native_approvals().await;
    assert_eq!(
        announcements(sink.events().await),
        2,
        "an approval that has been answered must NOT be re-announced -- a resolved sheet \
         coming back is a phantom prompt whose answer goes nowhere"
    );
}

/// HAND-scanner-residue: the tool-name scanners covered the workspace
/// contracts, the shipped prompt files and the next-step guidance, but two
/// model-facing sources the Host authors were never fed to them — the
/// shell gate's refusal, which is the text that tells an agent how to get
/// an unformed app moving, and the descriptions in the static host tool
/// catalog, which is what the model reads when deciding what to call.
///
/// Both are read from the RUNNING code (the refusal through a real gated
/// call, the descriptions through `host_tool_catalog`), not copied, so a
/// reworded literal cannot leave this gate scanning a stale copy.
#[tokio::test]
async fn host_authored_model_facing_text_names_no_tool_outside_local_app_tools() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let transport =
        crate::mobile::local_apps_mcp::LocalAppsMcpTransport::new(root.path().to_path_buf());
    assert!(transport.attach_service(Arc::clone(&service)).is_ok());
    let refused = transport
        .call_host_operation("build", json!({ "app_id": shell.id }))
        .await
        .expect("a domain refusal is a tool result, not a transport error");
    assert!(refused.is_error, "the shell gate must refuse: {refused:?}");
    let refusal = refused
        .content
        .get(0)
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("the refusal carries text")
        .to_string();
    // Vacuity guards: prove this is the SHELL GATE's refusal and that it
    // actually carries both shapes the two scanners look for, so a
    // reworded or re-routed refusal fails loudly instead of being scanned
    // for nothing.
    assert!(
        refusal.contains("app_not_scaffolded"),
        "read something other than the shell gate's refusal: {refusal}"
    );
    assert!(
        refusal.matches('`').count() >= 2 && refusal.contains("LocalAppScaffold"),
        "the shell-gate refusal no longer carries a backticked span and a bare LocalApp* \
         token, so neither scanner below has anything to check: {refusal}"
    );
    assert_only_real_tool_names(&refusal, "the shell gate refusal");
    assert_only_real_local_app_tool_tokens(&refusal, "the shell gate refusal");

    // The static host-operation catalog: this is the description text the
    // model is shown for every builtin LocalApp* tool.
    let catalog = crate::mobile::local_apps_mcp::LocalAppsMcpTransport::host_tool_catalog();
    assert!(
        !catalog.is_empty(),
        "read an empty host tool catalog, so nothing below was scanned"
    );
    let mut token_scanned = 0usize;
    let mut backtick_scanned = 0usize;
    for tool in &catalog {
        let label = format!("the `{}` tool description", tool.tool_name);
        if tool.description.contains("LocalApp") {
            assert_only_real_local_app_tool_tokens(&tool.description, &label);
            token_scanned += 1;
        }
        if tool.description.contains('`') {
            assert_only_real_tool_names(&tool.description, &label);
            backtick_scanned += 1;
        }
    }
    assert!(
        token_scanned > 0 && backtick_scanned > 0,
        "no tool description names a LocalApp* token ({token_scanned}) or carries a \
         backticked span ({backtick_scanned}), so this half of the gate scanned nothing"
    );
}

/// The gate for the two sources above: both scanners are NEGATIVE, so
/// nothing else proves they can fire on this text at all. Plant a dead
/// tool name in a copy of each real source and require the scan to reject
/// it BY NAME.
#[tokio::test]
async fn the_scanners_reject_a_dead_tool_name_planted_in_each_new_source() {
    fn rejection(scan: impl FnOnce() + std::panic::UnwindSafe, what: &str) -> String {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(scan);
        std::panic::set_hook(previous);
        let payload = outcome.err().unwrap_or_else(|| {
            panic!("the scanner accepted a planted dead tool name in {what}")
        });
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_else(|| panic!("the scanner's panic carried no message for {what}"))
    }

    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let transport =
        crate::mobile::local_apps_mcp::LocalAppsMcpTransport::new(root.path().to_path_buf());
    assert!(transport.attach_service(Arc::clone(&service)).is_ok());
    let refusal = transport
        .call_host_operation("build", json!({ "app_id": shell.id }))
        .await
        .expect("a domain refusal is a tool result")
        .content
        .get(0)
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("the refusal carries text")
        .to_string();
    // The plant: the retired runtime-confirmation tool, in the same two
    // shapes the real refusal already uses.
    let planted_refusal = refusal.replace("LocalAppScaffold", "LocalAppConfirmRuntime");
    assert_ne!(
        planted_refusal, refusal,
        "the plant did not apply, so the assertions below would pass on unplanted text"
    );
    let message = rejection(
        {
            let planted = planted_refusal.clone();
            move || assert_only_real_local_app_tool_tokens(&planted, "the planted refusal")
        },
        "the shell gate refusal",
    );
    assert!(
        message.contains("LocalAppConfirmRuntime"),
        "the token scan of the refusal rejected the plant without naming it: {message}"
    );
    let message = rejection(
        move || assert_only_real_tool_names(&planted_refusal, "the planted refusal"),
        "the shell gate refusal",
    );
    assert!(
        message.contains("LocalAppConfirmRuntime"),
        "the backtick scan of the refusal rejected the plant without naming it: {message}"
    );

    let catalog = crate::mobile::local_apps_mcp::LocalAppsMcpTransport::host_tool_catalog();
    let described = catalog
        .iter()
        .find(|tool| tool.description.contains("LocalAppPrepare"))
        .expect("a host tool description names LocalAppPrepare");
    let planted_description = described
        .description
        .replace("LocalAppPrepare", "LocalAppConfirmRuntime");
    assert_ne!(
        planted_description, described.description,
        "the plant did not apply to the description"
    );
    let message = rejection(
        move || {
            assert_only_real_local_app_tool_tokens(
                &planted_description,
                "the planted description",
            )
        },
        "a host tool description",
    );
    assert!(
        message.contains("LocalAppConfirmRuntime"),
        "the token scan of a tool description rejected the plant without naming it: {message}"
    );
}

#[tokio::test]
async fn execute_mcp_flow_runs_the_host_reloaded_typed_binding() {
    let (root, service, broker) = create_broker(false, None).await;
    let app_id = create_app_fixture(&root, &service, "Typed MCP Flow").await;
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.clone()).expect("layout");

    // The fixture's build receipt predates the v3 build-id field. Replace
    // it with the smallest valid immutable active-build receipt so the
    // runtime exercises the same pair check as a published app.
    let build_id = "typed-flow-build";
    fs::write(
        root.path().join(layout.build_rel(false)).join("build.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 3,
            "buildId": build_id,
            "buildKey": "typed-flow",
            "runtimeContractSha256": "0".repeat(64),
            "dependencySnapshotSha256": "0".repeat(64),
            "outputSha256": "0".repeat(64),
        }))
        .expect("serialize build receipt"),
    )
    .expect("write build receipt");

    let input_schema = json!({
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "required": ["query"],
        "additionalProperties": false,
    });
    let output_schema = runtime_record_output_schema();
    let flow = local_apps::FlowDefinition {
        flow_id: "runtime-status-flow".into(),
        version: 1,
        steps: vec![
            local_apps::FlowStep {
                step_id: "status1".into(),
                capability: local_apps::CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: r#"{"query":null}"#.into(),
            },
            local_apps::FlowStep {
                step_id: "status2".into(),
                capability: local_apps::CapabilityId::RuntimeStatus,
                depends_on: vec!["status1".into()],
                input_json: r#"{"previous":null}"#.into(),
            },
        ],
    };
    let context = local_apps::AppMcpFlowContext {
        app_id: app_id.clone(),
        source: local_apps::FlowSource::Active,
        flow,
        input_schema: input_schema.clone(),
        output_schema: output_schema.clone(),
        step_output_schemas: std::collections::BTreeMap::from([
            ("status1".into(), runtime_status_step_output_schema()),
            ("status2".into(), runtime_status_step_output_schema()),
        ]),
    };
    let context_sha256 =
        value_sha256(&serde_json::to_value(&context).expect("serialize active Flow context"))
            .expect("active Flow context digest");
    let workspace = root.path().join(layout.workspace_rel());
    fs::create_dir_all(workspace.join(".lingxi")).expect("create flow context directory");
    fs::write(
        workspace.join(".lingxi/mcp-flow-contexts.json"),
        serde_json::to_vec_pretty(&json!({"runtime-status-flow": context}))
            .expect("serialize flow contexts"),
    )
    .expect("write flow contexts");

    let mut definition =
        platform_api::McpToolDefinitionDto::new("runtime_status", input_schema);
    definition.output_schema = Some(output_schema);
    let binding = json!({
        "flowId": "runtime-status-flow",
        "inputs": {
            "query": {"tool_input": {"json_pointer": "/query"}},
            "previous": {
                "step_output": {"step_id": "status1", "json_pointer": "/runtime"}
            }
        },
        "result": {
            "step_output": {"step_id": "status2", "json_pointer": "/runtime"}
        }
    });
    let catalog = json!({
        "appId": app_id,
        "buildId": build_id,
        "tools": [{
            "definition": definition,
            "flow": binding,
            "ceiling": "allow",
        }],
        "execution": [{
            "definition": definition,
            "flow": binding,
            "ceiling": "allow",
            "contextSha256": context_sha256,
        }],
    });
    let catalog_sha256 =
        local_apps::approval_contract_sha256(catalog.clone()).expect("catalog digest");
    local_apps::save_mcp_catalog(&layout, &catalog_sha256, &catalog).expect("save catalog");

    let mut manifest = load_manifest(&layout).expect("fixture manifest");
    if manifest.revision == 0 {
        manifest.revision = 1;
    }
    manifest.active_mcp_catalog = Some(local_apps::AppMcpCatalogRef {
        build_id: build_id.into(),
        manifest_revision: manifest.revision,
        authoring_revision: 1,
        user_goal_sha256: "0".repeat(64),
        proposal_sha256: "0".repeat(64),
        approval_contract_sha256: "0".repeat(64),
        tool_surface_sha256: "0".repeat(64),
        catalog_sha256: catalog_sha256.clone(),
        mcp_verification_sha256: "0".repeat(64),
    });
    local_apps::save_manifest(&layout, &manifest).expect("publish catalog pointer");

    let result = broker
        .execute_mcp_flow_value(json!({
            "app_id": app_id,
            "tool_name": "runtime_status",
            "catalog_sha256": catalog_sha256,
            "input": {"query": "hello"},
        }))
        .await
        .expect("typed MCP flow succeeds");
    assert!(result.is_object(), "structured StepOutput result: {result}");
    assert!(
        result.get("state").is_some(),
        "runtime status is returned: {result}"
    );
}

#[tokio::test]
async fn published_rebuild_rebinds_the_active_mcp_catalog_to_the_new_build() {
    let (root, service, broker) = create_broker(false, None).await;
    let app_id = create_app_fixture(&root, &service, "Rebind MCP Build").await;
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.clone()).expect("layout");

    let initial_build_id = "build-1";
    fs::write(
        root.path().join(layout.build_rel(false)).join("build.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 3,
            "buildId": initial_build_id,
            "buildKey": "typed-flow",
            "runtimeContractSha256": "0".repeat(64),
            "dependencySnapshotSha256": "0".repeat(64),
            "outputSha256": "0".repeat(64),
        }))
        .expect("serialize build receipt"),
    )
    .expect("write build receipt");

    let flow_context = local_apps::AppMcpFlowContext {
        app_id: app_id.clone(),
        source: local_apps::FlowSource::Active,
        flow: local_apps::FlowDefinition {
            flow_id: "flow".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status".into(),
                capability: local_apps::CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        },
        input_schema: json!({"type":"object","additionalProperties":false}),
        output_schema: runtime_status_step_output_schema(),
        step_output_schemas: std::collections::BTreeMap::from([(
            "status".into(),
            runtime_status_step_output_schema(),
        )]),
    };
    let context_sha256 =
        value_sha256(&serde_json::to_value(&flow_context).expect("serialize flow context"))
            .expect("flow context digest");
    let workspace = root.path().join(layout.workspace_rel());
    fs::create_dir_all(workspace.join(".lingxi")).expect("create context directory");
    fs::write(
        workspace.join(".lingxi/mcp-flow-contexts.json"),
        serde_json::to_vec_pretty(&json!({"flow": flow_context}))
            .expect("serialize flow contexts"),
    )
    .expect("write flow contexts");

    let catalog = json!({
        "appId": app_id,
        "buildId": initial_build_id,
        "tools": [{
            "definition": {
                "name": "read_value",
                "inputSchema": {"type":"object","additionalProperties":false}
            },
            "flow": {"flowId":"flow","inputs":{},"result":{"literal":{"ok":true}}},
            "ceiling": "allow",
        }],
        "execution": [{
            "flow": {"flowId":"flow"},
            "contextSha256": context_sha256,
        }],
    });
    let catalog_sha256 = local_apps::hash_mcp_catalog(catalog.clone()).expect("catalog hash");
    local_apps::save_mcp_catalog(&layout, &catalog_sha256, &catalog).expect("save catalog");
    let mut manifest = load_manifest(&layout).expect("manifest");
    manifest.revision = manifest.revision.max(1);
    manifest.active_mcp_catalog = Some(local_apps::AppMcpCatalogRef {
        build_id: initial_build_id.into(),
        manifest_revision: manifest.revision,
        authoring_revision: 1,
        user_goal_sha256: "0".repeat(64),
        proposal_sha256: "0".repeat(64),
        approval_contract_sha256: "0".repeat(64),
        tool_surface_sha256: "1".repeat(64),
        catalog_sha256: catalog_sha256.clone(),
        mcp_verification_sha256: "0".repeat(64),
    });
    local_apps::save_manifest(&layout, &manifest).expect("publish catalog");

    let rebuilt_build_id = "build-2";
    fs::write(
        root.path().join(layout.build_rel(false)).join("build.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 3,
            "buildId": rebuilt_build_id,
            "buildKey": "typed-flow",
            "runtimeContractSha256": "0".repeat(64),
            "dependencySnapshotSha256": "0".repeat(64),
            "outputSha256": "0".repeat(64),
        }))
        .expect("serialize replacement build receipt"),
    )
    .expect("write replacement build receipt");

    broker
        .rebind_active_mcp_catalog_to_current_build(&app_id, &layout)
        .await
        .expect("rebind active catalog to current build");

    let rebound = load_manifest(&layout).expect("reload manifest");
    let active = rebound
        .active_mcp_catalog
        .as_ref()
        .expect("published app keeps active catalog");
    assert_eq!(active.build_id, rebuilt_build_id);
    assert_ne!(active.catalog_sha256, catalog_sha256);
    let rebound_catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
        .expect("load rebound catalog");
    assert_eq!(
        rebound_catalog.get("buildId").and_then(Value::as_str),
        Some(rebuilt_build_id)
    );
}

fn collect_fixture_files(current: &Path, files: &mut Vec<PathBuf>) {
    let metadata = fs::symlink_metadata(current).expect("inspect fixture output");
    assert!(
        !metadata.file_type().is_symlink(),
        "fixture output must not contain symlinks: {}",
        current.display()
    );
    if metadata.is_dir() {
        for entry in fs::read_dir(current).expect("read fixture output") {
            let entry = entry.expect("fixture output entry");
            collect_fixture_files(&entry.path(), files);
        }
    } else if metadata.is_file() {
        files.push(current.to_path_buf());
    } else {
        panic!(
            "fixture output must be a regular file or directory: {}",
            current.display()
        );
    }
}

fn fixture_output_digest(root: &Path) -> String {
    let mut files = Vec::new();
    collect_fixture_files(root, &mut files);
    files.sort();
    let mut hasher = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .expect("fixture output stays under the root");
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(&path).expect("read fixture output file"));
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn write_fixture_package_manifest(node_modules: &Path, package: &str, version: &str) {
    let package_dir = node_modules.join(package);
    fs::create_dir_all(&package_dir).expect("create fixture package directory");
    fs::write(
        package_dir.join("package.json"),
        format!("{{\"name\":\"{package}\",\"version\":\"{version}\"}}\n"),
    )
    .expect("write fixture package manifest");
}

fn seed_launchable_runtime_fixture(root: &Path, record: &local_apps::AppRecord, name: &str) {
    let layout = AppLayout::new(root.to_path_buf(), record.id.clone()).expect("layout");
    let workspace = root.join(layout.workspace_rel());
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("published react-dom runtime profile");
    let artifacts =
        scaffold_runtime_profile(Some(binding.clone()), local_apps::AppSurface::Dom)
            .expect("react-dom scaffold artifacts");
    stamp_scaffold_identity(
        &layout,
        name,
        &artifacts.binding,
        builtin_template_origin(&artifacts.binding),
    )
    .expect("stamp fixture scaffold");
    persist_runtime_profile_files(&workspace, &artifacts)
        .expect("persist fixture runtime profile files");

    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("react-dom runtime contract");
    for &(relative, bytes) in contract.editable_files {
        if relative == "app/mcp-widget/package.json" {
            crate::mobile::local_apps_build::write_file(&workspace, relative, bytes, true)
                .expect("seed fixture widget importer");
        }
    }
    let node_modules = workspace.join("node_modules");
    for &(package, version) in contract.core_packages {
        write_fixture_package_manifest(&node_modules, package, version);
    }
    let vite_bin = node_modules.join("vite/bin/vite.js");
    fs::create_dir_all(vite_bin.parent().expect("vite bin parent"))
        .expect("create fixture vite bin dir");
    fs::write(&vite_bin, b"#!/usr/bin/env node\n").expect("write fixture vite marker");
    fs::write(workspace.join("vite.config.mjs"), "export default {};\n")
        .expect("mark fixture as a Vite app");
    let tree_sha256 =
        dependency_tree_digest(&workspace.join("node_modules")).expect("dependency tree");
    refresh_runtime_profile_snapshot(&layout, &tree_sha256, None)
        .expect("refresh fixture dependency snapshot");
    let manifest = load_manifest(&layout).expect("fixture manifest");
    storage::save_dependency_record(
        root,
        &local_apps::AppDependencyRecord {
            schema_version: local_apps::APPS_SCHEMA_VERSION,
            app_id: record.id.clone(),
            state: local_apps::AppDependencyState::Ready,
            lockfile_sha256: manifest
                .dependency_snapshot
                .as_ref()
                .map(|snapshot| snapshot.lockfile_sha256.clone()),
            toolchain_key: manifest
                .dependency_snapshot
                .as_ref()
                .map(|snapshot| snapshot.toolchain_key.clone()),
            install_attempts: 1,
            last_error: None,
            updated_at_ms: record.updated_at_ms,
        },
    )
    .expect("save fixture dependency record");

    let static_dist = root
        .join(layout.build_rel(false))
        .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR);
    fs::create_dir_all(&static_dist).expect("create static dist");
    fs::write(static_dist.join("index.html"), "<html>ok</html>").expect("write index.html");
    let full_build = root.join(layout.build_rel(true));
    fs::create_dir_all(&full_build).expect("create full build");
    let output_sha256 = fixture_output_digest(&static_dist);
    let build_receipt = json!({
        "version": 3,
        "buildId": output_sha256,
        "buildKey": "fixture-static-build",
        "runtimeContractSha256": manifest.runtime_contract_hash().expect("runtime contract hash"),
        "dependencySnapshotSha256": manifest.dependency_snapshot_hash().expect("dependency snapshot hash"),
        "outputSha256": output_sha256,
    });
    fs::write(
        root.join(layout.build_rel(false)).join("build.json"),
        serde_json::to_vec_pretty(&build_receipt).expect("serialize fixture build receipt"),
    )
    .expect("write fixture build receipt");
}

/// Land the host-owned scaffold the way `LocalAppScaffold` does — through
/// `land_scaffold`, the one production landing point (`LocalAppScaffold`
/// reaches it via `scaffold_shell_app_value`) — minus the dependency
/// install, which none of the LINGXI.md / device-context assertions below
/// look at.
async fn land_test_scaffold(broker: &Arc<LocalAppsHostBroker>, record: &local_apps::AppRecord) {
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("published react-dom runtime profile");
    let (build_lock, recovery_lock, recovery) = broker
        .land_scaffold(record, local_apps::AppSurface::Dom, Some(binding), None)
        .await
        .expect("land scaffold");
    recovery.commit().expect("commit scaffold recovery");
    drop(build_lock);
    drop(recovery_lock);
}

async fn scaffolded_lingxi(
    full_runtime: bool,
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
) -> (String, String) {
    let (root, service, broker) = create_broker(full_runtime, mobile_linux).await;
    let record = service
        .create_app(Some("Tracker"), "a test app", None)
        .await
        .expect("create app");
    land_test_scaffold(&broker, &record).await;
    let layout = AppLayout::new(root.path().to_path_buf(), record.id.clone()).expect("layout");
    let lingxi =
        std::fs::read_to_string(root.path().join(layout.workspace_rel()).join("LINGXI.md"))
            .expect("read LINGXI.md");
    (record.id, lingxi)
}

/// Creation, not just `LocalAppManifest`, records the target: an app the
/// agent never declares a manifest for still knows what it was built on.
#[tokio::test]
async fn scaffold_records_the_host_device_context() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    assert!(broker
        .attach_host_environment(host_environment(
            platform_api::MobileHostOs::Ios,
            platform_api::MobileDeviceClass::Tablet,
        ))
        .is_ok());
    let record = service
        .create_app(Some("Scaffolded"), "a test app", None)
        .await
        .expect("create app");
    land_test_scaffold(&broker, &record).await;

    let layout = AppLayout::new(root.path().to_path_buf(), record.id).expect("layout");
    let recorded = load_manifest(&layout)
        .expect("manifest")
        .device_context
        .expect("creation records the native target");
    assert_eq!(recorded.os, "ios");
    assert_eq!(recorded.form_factor, "ipad");
}

#[tokio::test]
async fn scaffold_writes_capability_neutral_lingxi_when_toolchain_is_available() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_app_id, lingxi) = scaffolded_lingxi(true, Some(runtime)).await;
    assert!(lingxi.contains("Do not run `npm create vite`"), "{lingxi}");
    // The contract is READ BY A MODEL as a set of examples to copy. An
    // unsubstituted placeholder or a doubled brace is a malformed call the
    // agent will faithfully reproduce, get a schema error from, and then
    // start improvising around — which is exactly the build flailing this
    // file exists to prevent. `build_preview` is a plain `&str`, so its
    // braces were never processed by the enclosing `format!`.
    // Catch ANY `{ident}` placeholder, not just `{id}` — a future fragment
    // added as a plain `&str` would leak `{name}`/`{brief}` the same way.
    // JSON examples in the contract are `{"key":...}`, so requiring a bare
    // lower-snake identifier between the braces does not false-positive.
    let leaked: Vec<&str> = lingxi
        .match_indices('{')
        .filter_map(|(start, _)| {
            let rest = &lingxi[start + 1..];
            let end = rest.find('}')?;
            let inner = &rest[..end];
            (!inner.is_empty() && inner.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
                .then_some(&lingxi[start..start + end + 2])
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "unsubstituted placeholder(s) {leaked:?} in the agent contract: {lingxi}"
    );
    assert!(
        !lingxi.contains("{{") && !lingxi.contains("}}"),
        "doubled braces leaked into the agent contract: {lingxi}"
    );
    assert!(
        lingxi.contains(&format!("LocalAppBuild {{\"app_id\":\"{_app_id}\"}}")),
        "the build example must carry this app's real id: {lingxi}"
    );
    // A failed build is the exact moment the agent goes off-script. The
    // contract must name the recovery path AND forbid improvising an
    // alternate build command — there is no second build path here.
    assert!(
        lingxi.contains("do NOT try a different build command"),
        "{lingxi}"
    );
    assert!(
        lingxi.contains("LocalAppInstallDeps"),
        "dependency state is the most common build failure; the tool that \
         reports it must be named: {lingxi}"
    );
    // Verification tools were documented only in the skill, so an agent
    // working from the workspace had to rediscover them.
    assert!(lingxi.contains("LocalAppQueryData"), "{lingxi}");
    assert!(lingxi.contains("LocalAppInspectUi"), "{lingxi}");
    assert!(
        lingxi.contains("do not run a package manager in this local-app workspace"),
        "{lingxi}"
    );
    assert!(
        lingxi
            .contains("Host-managed files are `.gitignore`, `package.json`, `pnpm-lock.yaml`"),
        "{lingxi}"
    );
    assert!(
        lingxi.contains("sole writable `LocalAppBuild` root"),
        "{lingxi}"
    );
    assert!(
        lingxi.contains("directly from this workspace as the sole writable mount"),
        "{lingxi}"
    );
    assert!(!lingxi.contains("isolated workspace mount"), "{lingxi}");
    assert!(lingxi.contains("`build/store/dist/`"), "{lingxi}");
}

#[tokio::test]
async fn scaffold_writes_capability_neutral_lingxi_when_shell_is_missing() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_app_id, lingxi) = scaffolded_lingxi(true, Some(runtime)).await;
    assert!(
        lingxi.contains("repository-verified Vite + Ionic foundation"),
        "{lingxi}"
    );
    assert!(lingxi.contains("Do not run `npm create vite`"), "{lingxi}");
    assert!(lingxi.contains("do not run a package manager in this local-app workspace"));
    assert!(!lingxi.contains("vite-fallback"), "{lingxi}");
}

#[tokio::test]
async fn persisted_lingxi_does_not_bake_in_toolchain_availability() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(true, Some(runtime)).await;
    let record = service
        .create_app(Some("Tracker"), "a test app", None)
        .await
        .expect("create app");
    land_test_scaffold(&broker, &record).await;
    let layout = AppLayout::new(root.path().to_path_buf(), record.id).expect("layout");
    let lingxi =
        std::fs::read_to_string(root.path().join(layout.workspace_rel()).join("LINGXI.md"))
            .expect("read LINGXI.md");

    assert!(lingxi.contains("do not run a package manager in this local-app workspace"));
    assert!(broker
        .create_next_step()
        .contains("Do not recreate the app"));
    assert!(broker
        .create_next_step()
        .contains("do not install dependencies yet"));
}

// ---- §C.1 `LocalAppScaffold` — the create transaction --------------

/// The "+" button's shell, exactly as `host.rs` creates one: no brief, no
/// surface, `scaffolded == false`, and the GUIDED workspace contract on
/// disk so a test can prove the formal one replaced it.
async fn shell_app_fixture(
    broker: &Arc<LocalAppsHostBroker>,
    service: &Arc<AppService>,
) -> local_apps::AppRecord {
    let record = service
        .create_app_with_mode(None, "", None, local_apps::CreateMode::Shell, None)
        .await
        .expect("create shell app");
    assert!(
        !record.scaffolded,
        "the fixture must actually be the state these tests name"
    );
    assert_eq!(record.name, local_apps::service::PLACEHOLDER_APP_NAME);
    assert_eq!(record.brief, "");
    broker
        .write_guided_contract_value(&record)
        .await
        .expect("write the guided contract");
    record
}

/// A chat-origin app's recorded brief must survive the thin bootstrap
/// handoff without triggering a second Host-authored interview.
#[tokio::test]
async fn a_chat_origin_shells_guided_contract_does_not_re_ask_the_opening_question() {
    let (root, service, broker) = create_broker(false, None).await;
    let record = service
        .create_app_with_mode(
            None,
            "a todo list app with reminders",
            Some("conv-123".to_string()),
            local_apps::CreateMode::Shell,
            None,
        )
        .await
        .expect("create chat-origin shell app");
    assert!(
        record.conversation_id.is_some(),
        "the fixture must actually carry the origin conversation these tests name"
    );
    broker
        .write_guided_contract_value(&record)
        .await
        .expect("write the guided contract");
    let guided = fs::read_to_string(workspace_of(&root, &record.id).join("LINGXI.md"))
        .expect("read the guided contract");
    assert!(
        !guided.contains("ask the user what they want to build"),
        "a chat-origin app must not be told to re-ask the opening question the user \
         already answered: {guided}"
    );
    assert!(
        guided.contains("Recorded brief: \"a todo list app with reminders\""),
        "the bootstrap must carry the recorded brief into the coordinator: {guided}"
    );
    assert!(
        guided.contains("Immediately use the `Skill` tool"),
        "the bootstrap must hand the recorded brief to the create coordinator immediately: \
         {guided}"
    );
}

fn scaffold_input(app_id: &str, name: &str, brief: &str, _surface: &str) -> Value {
    json!({
        "app_id": app_id,
        "name": name,
        "brief": brief,
    })
}

fn template_id_for_scaffold_surface(surface: &str) -> &'static str {
    match surface {
        "dom" => "react-dom-r4",
        "canvas" => "canvas-2d-r4",
        other => panic!("unsupported test scaffold surface {other}"),
    }
}

const TEST_DEFAULT_APP_NAME: &str = "Test App";
const TEST_DEFAULT_APP_BRIEF: &str = "test app brief";

async fn stage_react_dom_authoring_contract(
    broker: &Arc<LocalAppsHostBroker>,
    app_id: &str,
    workflow_run_id: &str,
    validated_selection_handle: &str,
) -> String {
    let spec: local_apps::AppAuthoringSpec = serde_json::from_str(include_str!(
        "../../../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
    ))
    .expect("authoring fixture");
    broker
        .local_app_contract(json!({
            "operation": "stage",
            "app_id": app_id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": validated_selection_handle,
            "spec": spec,
        }))
        .await
        .expect("stage authoring contract")["contract_handle"]
        .as_str()
        .expect("contract handle")
        .to_string()
}

async fn approved_create_receipt(
    broker: &Arc<LocalAppsHostBroker>,
    app_id: &str,
    surface: &str,
) -> (String, String) {
    approved_create_receipt_with_design(
        broker,
        app_id,
        surface,
        None,
        TEST_DEFAULT_APP_NAME,
        TEST_DEFAULT_APP_BRIEF,
    )
    .await
}

async fn approved_create_receipt_with_design(
    broker: &Arc<LocalAppsHostBroker>,
    app_id: &str,
    surface: &str,
    design_spec: Option<Value>,
    name: &str,
    brief: &str,
) -> (String, String) {
    let workflow_run_id = format!("wf_create_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            app_id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": app_id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": template_id_for_scaffold_surface(surface),
            "reason": "test create receipt",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let mut authoring_value: Value = serde_json::from_str(include_str!(
        "../../../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
    ))
    .expect("authoring fixture");
    if surface == "canvas" {
        authoring_value["design"]["canvas"] = json!({
            "scene": "Test scene",
            "phases": ["ready", "running"],
            "controls": ["tap"],
            "hud": "Score",
        });
        authoring_value["acceptance_checks"][0]["evidence"] = json!(["capture"]);
    }
    let authoring_spec: local_apps::AppAuthoringSpec =
        serde_json::from_value(authoring_value).expect("profile-compatible authoring spec");
    let staged_contract = broker
        .local_app_contract(json!({
            "operation": "stage",
            "app_id": app_id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "spec": authoring_spec,
        }))
        .await
        .expect("stage authoring contract");
    // r2-tests-honesty-001: a real design-less create OMITS the
    // `design_spec` key entirely — `json!({"design_spec": design_spec})`
    // with `design_spec: None` instead serializes a literal JSON `null`,
    // which `stage_create`'s `input.get("design_spec")` sees as
    // `Some(Value::Null)`, not `None`, so it writes `design-spec.json`
    // containing 4 bytes of `null` and digests those bytes into
    // `design_spec_sha256` — a value no real design-less create can
    // produce. Insert the key only when a design spec is actually
    // supplied so this fixture exercises the branch it claims to.
    let has_design_spec = design_spec.is_some();
    let mut stage_request = json!({
        "app_id": app_id,
        "workflow_run_id": workflow_run_id,
        "validated_selection_handle": handle,
        "quality_level": if surface == "canvas" { "balanced" } else { "fast" },
        "name": name,
        "brief": brief,
        "contract_handle": staged_contract["contract_handle"],
    });
    if let Some(design_spec) = design_spec {
        stage_request["design_spec"] = design_spec;
    }
    let stage = broker
        .stage_create(stage_request)
        .await
        .expect("stage create");
    assert_eq!(
        stage
            .get("design_spec_sha256")
            .map_or(false, |v| !v.is_null()),
        has_design_spec,
        "design_spec_sha256 presence must track whether a design spec was staged: {stage}"
    );
    write_initial_staging_flow_contexts(broker, app_id, &workflow_run_id, handle);
    let approval_contract_sha256 =
        persist_initial_mcp_candidate_fixture(broker, app_id, &workflow_run_id);
    // The native CREATE confirmation sheet is retired: today the user
    // answers a PLAN, and `LocalAppPrepare` seals the create approval
    // itself with `CreateApprovalAuthority::ApprovedPlan`. A bare
    // `approve_mcp_proposal` now raises the LIVE MCP-proposal sheet
    // (`McpProposalApprovalRequested`) instead, so this fixture parks on
    // `pending_mcp_proposal_approvals` and answers that one.
    let approval = tokio::spawn({
        let broker = broker.clone();
        let app_id = app_id.to_string();
        let workflow_run_id = workflow_run_id.clone();
        let approval_contract_sha256 = approval_contract_sha256.clone();
        async move {
            broker
                .approve_mcp_proposal(json!({
                    "app_id": app_id,
                    "workflow_run_id": workflow_run_id,
                    "approval_contract_sha256": approval_contract_sha256,
                }))
                .await
        }
    });
    tokio::pin!(approval);
    let request_id = tokio::select! {
        found = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(request_id) = broker
                    .pending_mcp_proposal_approvals
                    .lock()
                    .await
                    .keys()
                    .next()
                    .cloned()
                {
                    break request_id;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }) => found.expect("mcp proposal approval request"),
        result = &mut approval => panic!(
            "approval returned before raising the MCP-proposal approval: {:?}",
            result.expect("approval task did not panic")
        ),
    };
    assert!(
        broker
            .resolve_mcp_proposal_approval(&request_id, true)
            .await,
        "approval resolver must consume the pending MCP-proposal approval"
    );
    let approved = approval
        .await
        .expect("approval task")
        .expect("approved proposal");
    (
        workflow_run_id,
        approved["receipt_id"]
            .as_str()
            .expect("create receipt id")
            .to_string(),
    )
}

async fn confirmed_scaffold_input(
    broker: &Arc<LocalAppsHostBroker>,
    app_id: &str,
    name: &str,
    brief: &str,
    surface: &str,
) -> Value {
    // Stage the SAME name/brief that will be echoed into the scaffold call
    // below, so these fixtures exercise the ordinary case where the model's
    // echo matches what it staged — the mismatch case (staged wins) has its
    // own dedicated test. `LocalAppStageCreate` enforces the same
    // non-empty/max-length bounds `LocalAppScaffold` does, so a handful of
    // this helper's callers deliberately pass a value ONLY meant to trip
    // scaffold's own front-door validation (an empty/whitespace name or
    // brief, an over-long name) — staging that value would fail before the
    // scaffold call under test ever runs. Stage a safe placeholder in that
    // case; the (possibly invalid) `name`/`brief` args still reach the
    // returned scaffold input unchanged, so the validation under test
    // still sees exactly what the caller asked for.
    let stage_name =
        if !name.trim().is_empty() && name.len() <= local_apps::service::MAX_NAME_BYTES {
            name
        } else {
            TEST_DEFAULT_APP_NAME
        };
    let stage_brief =
        if !brief.trim().is_empty() && brief.len() <= local_apps::service::MAX_BRIEF_BYTES {
            brief
        } else {
            TEST_DEFAULT_APP_BRIEF
        };
    let (workflow_run_id, receipt_id) = approved_create_receipt_with_design(
        broker,
        app_id,
        surface,
        None,
        stage_name,
        stage_brief,
    )
    .await;
    json!({
        "app_id": app_id,
        "name": name,
        "brief": brief,
        "workflow_run_id": workflow_run_id,
        "receipt_id": receipt_id,
    })
}

fn initial_mcp_proposal_fixture(app_id: &str, manifest_revision: u64) -> Value {
    serde_json::to_value(initial_mcp_proposal_model(app_id, manifest_revision))
        .expect("serialize initial MCP proposal fixture")
}

fn runtime_record_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schemaVersion": {"type": "integer"},
            "appId": {"type": "string"},
            "state": {"type": "string"},
            "mode": {"type": "string"},
            "port": {"type": "integer"},
            "pid": {"type": "integer"},
            "lastError": {"type": "string"},
            "updatedAtMs": {"type": "integer"}
        },
        "required": ["schemaVersion", "appId", "state", "updatedAtMs"],
        "additionalProperties": false
    })
}

fn runtime_status_step_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "app_id": {"type": "string"},
            "runtime": runtime_record_output_schema()
        },
        "required": ["app_id", "runtime"],
        "additionalProperties": false
    })
}

fn initial_mcp_proposal_model(
    app_id: &str,
    manifest_revision: u64,
) -> local_apps::AppMcpProposal {
    local_apps::AppMcpProposal {
        app_id: app_id.to_string(),
        manifest_revision,
        user_goal_sha256: "2".repeat(64),
        summary: "Expose the staged status check as one MCP tool.".into(),
        tools: vec![local_apps::AppMcpToolProposal {
            name: "runtime_status".into(),
            title: Some("Runtime status".into()),
            description: Some("Read the current runtime status from the staged flow.".into()),
            input_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false,
            }),
            output_schema: Some(runtime_record_output_schema()),
            semantic_flow_id: "runtime-status-flow".into(),
            inputs: std::collections::BTreeMap::from([(
                "query".into(),
                local_apps::FlowValueBinding::ToolInput {
                    json_pointer: "/query".into(),
                },
            )]),
            result: local_apps::FlowValueBinding::StepOutput {
                step_id: "status1".into(),
                json_pointer: "/runtime".into(),
            },
        }],
        required_flow_changes: Vec::new(),
        excluded_capabilities: Vec::new(),
    }
}

fn persist_initial_mcp_candidate_fixture(
    broker: &Arc<LocalAppsHostBroker>,
    app_id: &str,
    workflow_run_id: &str,
) -> String {
    let layout = broker.layout(app_id).expect("layout");
    let manifest = load_manifest(&layout).expect("manifest");
    let proposal = initial_mcp_proposal_model(app_id, manifest.revision);
    let input_schema = json!({
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "required": ["query"],
        "additionalProperties": false,
    });
    let output_schema = runtime_record_output_schema();
    let mut definition =
        platform_api::McpToolDefinitionDto::new("runtime_status", input_schema);
    definition.title = Some("Runtime status".into());
    definition.description =
        Some("Read the current runtime status from the staged flow.".into());
    definition.output_schema = Some(output_schema);
    let flow = local_apps::AppMcpFlowBinding {
        flow_id: "runtime-status-flow".into(),
        inputs: std::collections::BTreeMap::from([(
            "query".into(),
            local_apps::FlowValueBinding::ToolInput {
                json_pointer: "/query".into(),
            },
        )]),
        result: local_apps::FlowValueBinding::StepOutput {
            step_id: "status1".into(),
            json_pointer: "/runtime".into(),
        },
    };
    let validated = local_apps::ValidatedAppMcpProposal {
        proposal: proposal.clone(),
        tools: vec![local_apps::HostValidatedMcpTool {
            definition: definition.clone(),
            flow,
            ceiling: platform_api::McpPermissionCeiling::Allow,
        }],
        proposal_sha256: local_apps::approval_contract_sha256(
            serde_json::to_value(&proposal).expect("serialize proposal"),
        )
        .expect("proposal digest"),
        tool_surface_sha256: local_apps::approval_contract_sha256(
            serde_json::to_value(vec![definition.clone()]).expect("serialize tool surface"),
        )
        .expect("tool surface digest"),
    };
    let create_context = broker
        .load_create_proposal_context(app_id, workflow_run_id)
        .expect("create proposal context");
    let review_surface = LocalAppsHostBroker::build_mcp_review_surface(
        &manifest,
        &validated,
        manifest.active_mcp_catalog.as_ref(),
        Some(&create_context),
    );
    let approval_contract_sha256 =
        local_apps::approval_contract_sha256(review_surface.clone()).expect("approval digest");
    let journal = local_apps::McpCandidateJournal {
        schema_version: local_apps::APPS_SCHEMA_VERSION,
        app_id: app_id.to_string(),
        workflow_run_id: workflow_run_id.to_string(),
        stage: local_apps::McpAuthoringStage::Prepared,
        previous_build_id: crate::mobile::local_apps_build::active_build_id(&layout)
            .expect("active build id"),
        previous_catalog_sha256: manifest
            .active_mcp_catalog
            .as_ref()
            .map(|catalog| catalog.catalog_sha256.clone()),
        proposal_sha256: validated.proposal_sha256.clone(),
        approval_contract_sha256: approval_contract_sha256.clone(),
        tool_surface_sha256: validated.tool_surface_sha256.clone(),
        catalog_sha256: None,
        integrity_sha256: String::new(),
    }
    .seal()
    .expect("seal candidate journal");
    local_apps::save_candidate_journal(&layout, &journal).expect("save candidate journal");
    broker
        .save_mcp_candidate(
            app_id,
            workflow_run_id,
            &PersistedMcpCandidate {
                validated,
                approval_contract_sha256: approval_contract_sha256.clone(),
                review_surface,
                verification_sha256: None,
                catalog_sha256: None,
                qa_context_sha256: None,
            },
        )
        .expect("save staged candidate");
    approval_contract_sha256
}

fn write_initial_staging_flow_contexts(
    broker: &Arc<LocalAppsHostBroker>,
    app_id: &str,
    workflow_run_id: &str,
    handle: &str,
) {
    let staging_root = broker.create_staging_root(app_id, workflow_run_id, handle);
    fs::create_dir_all(staging_root.join(".lingxi")).expect("create staging flow context dir");
    let context = local_apps::AppMcpFlowContext {
        app_id: app_id.to_string(),
        source: local_apps::FlowSource::Staging,
        flow: local_apps::FlowDefinition {
            flow_id: "runtime-status-flow".into(),
            version: 1,
            steps: vec![local_apps::FlowStep {
                step_id: "status1".into(),
                capability: local_apps::CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: r#"{"query":null}"#.into(),
            }],
        },
        input_schema: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
            "additionalProperties": false,
        }),
        output_schema: runtime_record_output_schema(),
        step_output_schemas: std::collections::BTreeMap::from([(
            "status1".into(),
            runtime_status_step_output_schema(),
        )]),
    };
    fs::write(
        staging_root.join(".lingxi/mcp-flow-contexts.json"),
        serde_json::to_vec_pretty(&json!({"runtime-status-flow": context}))
            .expect("serialize staging flow contexts"),
    )
    .expect("write staging flow contexts");
}

fn workspace_of(root: &TempDir, app_id: &str) -> PathBuf {
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.to_string()).expect("layout");
    root.path().join(layout.workspace_rel())
}

fn dependency_baseline_for(
    layout: &AppLayout,
    dependency_record: &local_apps::AppDependencyRecord,
) -> DependencyBaselineIdentity {
    LocalAppsHostBroker::load_trusted_dependency_baseline(layout, dependency_record)
        .expect("trusted dependency baseline")
        .3
}

/// Break the LAST step of the landing (§C.1 step 3e, the formal
/// `LINGXI.md`) by putting a DIRECTORY where that file must be written.
///
/// Chosen deliberately over corrupting an earlier step: it lets every
/// preceding step SUCCEED, so the atomicity tests below prove the commit
/// point held even when the landing got all the way to its final write —
/// the interleaving a half-commit would actually survive. `LINGXI.md` is
/// in `FIRST_SCAFFOLD_PRESERVED`, so the wipe leaves the directory alone.
fn break_the_final_landing_step(root: &TempDir, app_id: &str) {
    let contract = workspace_of(root, app_id).join("LINGXI.md");
    let _ = fs::remove_file(&contract);
    fs::create_dir_all(contract.join("occupied")).expect("occupy the contract path");
}

fn repair_the_final_landing_step(root: &TempDir, app_id: &str) {
    let contract = workspace_of(root, app_id).join("LINGXI.md");
    fs::remove_dir_all(&contract).expect("free the contract path");
}

fn break_index_commit(root: &TempDir) -> Vec<u8> {
    let index = root.path().join(local_apps::storage::index_rel());
    let original = fs::read(&index).expect("read index before injected failure");
    fs::remove_file(&index).expect("remove index before injected failure");
    fs::create_dir_all(&index).expect("occupy index path");
    original
}

fn repair_index_commit(root: &TempDir, original: &[u8]) {
    let index = root.path().join(local_apps::storage::index_rel());
    fs::remove_dir_all(&index).expect("free index path");
    fs::write(index, original).expect("restore index after injected failure");
}

/// The shell is only an identity/no-write guard. Product discovery and the
/// technical recommendations belong to the create coordinator, which plans
/// the app with the user and has the Host prepare the workspace from the
/// plan the user approves; that plan approval IS the create confirmation,
/// so there is no second native sheet. MCP exposure is an optional
/// post-create capability rather than a prerequisite questionnaire.
#[tokio::test]
async fn guided_shell_delegates_without_technical_or_mcp_prerequisites() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let guided = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("read the guided contract");

    assert!(
        guided.contains("Immediately use the `Skill` tool")
            && guided.contains("lingxi-local-app:create-local-app"),
        "the shell must immediately enter the plugin-qualified create coordinator: {guided}"
    );
    assert!(
        guided.contains("plans the app with the user")
            && guided.contains("prepares this workspace from the plan the user approves"),
        "the shell must delegate planning and prepare-from-the-approved-plan to the create \
         coordinator, whose plan approval is the create confirmation: {guided}"
    );
    assert!(
        guided.contains("optionally expose named business capabilities"),
        "MCP exposure must remain optional and post-create: {guided}"
    );
    for forbidden in [
        "target **shape**",
        "`dom` —",
        "`canvas` —",
        "LocalAppTemplateCatalog",
        "mcpSuggestions",
        "2-3 concrete named recommendations",
        "Once the name, shape, and MCP choice are confirmed",
        "live external service or data source",
        "must be the user's confirmed choice",
    ] {
        assert!(
            !guided.contains(forbidden),
            "the thin shell must not contain `{forbidden}`: {guided}"
        );
    }
}

#[tokio::test]
async fn scaffold_requires_a_unified_create_receipt() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;

    let error = broker
        .scaffold_shell_app_value(scaffold_input(&shell.id, "A", "b", "dom"))
        .await
        .expect_err("scaffold must fail closed without a native-confirmed receipt");
    assert!(error.contains("receipt_id is required"), "{error}");
    assert!(!service.record(&shell.id).await.expect("record").scaffolded);
    assert!(
        fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
            .expect("guided contract")
            .contains("has no shape yet")
    );
}

#[tokio::test]
async fn authoring_dispatch_preserves_validation_errors() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let transport =
        crate::mobile::local_apps_mcp::LocalAppsMcpTransport::new(root.path().to_path_buf());
    assert!(transport.attach_service(Arc::clone(&service)).is_ok());
    assert!(transport.attach_host(broker.clone()).is_ok());
    let result = transport
        .call_host_operation(
            "contract",
            json!({
                "operation": "stage", "app_id": shell.id,
            }),
        )
        .await
        .expect("domain error remains a tool result");
    assert!(result.is_error, "{result:?}");
    assert!(
        result.content[0]["text"]
            .as_str()
            .expect("tool error text")
            .contains("missing non-empty \"workflow_run_id\""),
        "{result:?}"
    );

    let host: &dyn LocalAppsMcpHost = broker.as_ref();
    for result in [
        host.qa_begin(json!({})).await,
        host.qa_read_evidence(json!({})).await,
        host.qa_finalize(json!({})).await,
    ] {
        let error = result.expect_err("missing app id must fail validation");
        assert!(error.contains("missing non-empty \"app_id\""), "{error}");
    }
}

#[tokio::test]
async fn staged_create_approval_does_not_author_or_enable_mcp() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workflow_run_id = format!("wf_plain_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id.clone(),
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "plain create without MCP",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let authoring_spec: local_apps::AppAuthoringSpec = serde_json::from_str(include_str!(
        "../../../../../local-apps/tests/fixtures/authoring-spec.valid-null-canvas.json"
    ))
    .expect("authoring fixture");
    let transport =
        crate::mobile::local_apps_mcp::LocalAppsMcpTransport::new(root.path().to_path_buf());
    assert!(transport.attach_service(Arc::clone(&service)).is_ok());
    assert!(transport.attach_host(broker.clone()).is_ok());
    let result = transport
        .call_host_operation(
            "contract",
            json!({
                "operation": "stage",
                "app_id": shell.id,
                "workflow_run_id": workflow_run_id,
                "validated_selection_handle": handle,
                "spec": authoring_spec,
            }),
        )
        .await
        .expect("stage authoring contract through MCP");
    assert!(!result.is_error, "{result:?}");
    let contract = result.structured_content.expect("staged contract");
    broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "contract_handle": contract["contract_handle"],
            "quality_level": "fast",
            "name": "Plain create",
            "brief": "MCP remains optional",
        }))
        .await
        .expect("stage create");
    write_initial_staging_flow_contexts(&broker, &shell.id, &workflow_run_id, handle);

    // The native create-confirmation sheet is retired. The plan approval is
    // the only producer of `CreateApprovalAuthority::ApprovedPlan` in
    // production (`local_apps_prepare::finish_create_scaffold`), so this
    // fixture takes exactly the authority the Host takes once the user has
    // approved a plan; a bare `approve_mcp_proposal` (NativeSheet) is now
    // refused with `create_requires_approved_plan`.
    let approved = broker
        .approve_mcp_proposal_with(
            json!({
                "app_id": shell.id,
                "workflow_run_id": workflow_run_id,
                "create_without_mcp": true,
            }),
            CreateApprovalAuthority::ApprovedPlan,
        )
        .await
        .expect("approve plain create under the approved-plan authority");
    assert_eq!(approved["status"], "create_approved_no_mcp");
    let receipt_id = approved["receipt_id"]
        .as_str()
        .expect("create receipt")
        .to_string();

    let candidate = broker
        .load_mcp_candidate(&shell.id, &workflow_run_id)
        .expect("Host-owned create candidate");
    assert!(candidate.validated.tools.is_empty());
    broker
        .scaffold_shell_app_value(json!({
            "app_id": shell.id,
            "name": "Plain create",
            "brief": "MCP remains optional",
            "workflow_run_id": workflow_run_id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("scaffold plain create");
    assert!(service.record(&shell.id).await.expect("record").scaffolded);
    let manifest = load_manifest(&broker.layout(&shell.id).expect("layout")).expect("manifest");
    assert!(manifest.active_mcp_catalog.is_none());
    assert!(
        local_apps::load_candidate_journal(&broker.layout(&shell.id).expect("layout")).is_err()
    );
    assert!(
        !load_mcp_settings(&broker.layout(&shell.id).expect("layout"))
            .expect("MCP settings")
            .enabled
    );
}

/// `stage_create` must refuse to re-stage once THIS RUN's create
/// candidate journal is already sealed at `Approved` — the native create
/// sheet already rendered these staged bytes and the user already
/// answered for them. Without a guard, a second `stage_create` for the
/// same run silently overwrites `design-spec.json`/`evidence.json` under
/// an approval bound to the OLD content, so the approved receipt would
/// end up committing bytes nobody actually confirmed
/// (`r1-backlog-scaffold-build-14`).
///
/// The refusal is scoped to the approved RUN, and the second half of this
/// test pins that scope: staging is per-run
/// (`.lingxi-build-state/template-candidates/<app>/<run>/staging/…`), so
/// a DIFFERENT run cannot reach the approved run's bytes and must stay
/// allowed — otherwise the remedy the error message itself prescribes
/// ("start a new workflow run") would be refused too, and an app whose
/// `LocalAppScaffold` was rejected after approval could never be staged
/// again in any run.
#[tokio::test]
async fn stage_create_after_approval_is_refused_and_cannot_rewrite_the_approved_bytes() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let (first_workflow_run_id, _receipt_id) =
        approved_create_receipt(&broker, &shell.id, "dom").await;
    let sealed_journal_before =
        local_apps::load_candidate_journal(&broker.layout(&shell.id).expect("layout"))
            .expect("sealed journal");
    assert_eq!(
        sealed_journal_before.stage,
        local_apps::McpAuthoringStage::Approved
    );

    // The exact bytes the native sheet rendered, in the approved run.
    let approved_handle = broker
        .create_selection_handle_for_run(&shell.id, &first_workflow_run_id)
        .expect("the approved run's selection handle");
    let approved_evidence_path = broker
        .create_staging_root(&shell.id, &first_workflow_run_id, &approved_handle)
        .join("evidence.json");
    let evidence_before =
        fs::read_to_string(&approved_evidence_path).expect("approved staging evidence");
    assert!(
        evidence_before.contains(TEST_DEFAULT_APP_NAME),
        "the approved evidence must carry the confirmed name: {evidence_before}"
    );

    // Re-staging INSIDE the approved run — the hazard — must be refused.
    let error = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": first_workflow_run_id,
            "validated_selection_handle": approved_handle,
            "quality_level": "fast",
            "name": "a different name the user never saw",
            "brief": "a different brief the user never saw",
        }))
        .await
        .expect_err("re-staging an already-approved create candidate must be refused");
    assert!(
        error.contains("create_staging_rejected") && error.contains("already approved"),
        "expected the re-stage refusal to name both the code and the reason, got: {error}"
    );

    // And it must refuse BEFORE writing anything: the approved bytes the
    // user answered for are byte-identical afterwards.
    let evidence_after = fs::read_to_string(&approved_evidence_path)
        .expect("approved staging evidence still present");
    assert_eq!(
        evidence_before, evidence_after,
        "the refusal must not have rewritten the approved staging evidence"
    );
    assert!(
        !evidence_after.contains("a different name the user never saw"),
        "the unconfirmed name must never reach the approved staging evidence: {evidence_after}"
    );

    // The refusal is a pure read-side check: the sealed journal approved
    // for `first_workflow_run_id` is untouched.
    let sealed_journal_after =
        local_apps::load_candidate_journal(&broker.layout(&shell.id).expect("layout"))
            .expect("sealed journal still present");
    assert_eq!(sealed_journal_after.workflow_run_id, first_workflow_run_id);
    assert_eq!(
        sealed_journal_after.stage,
        local_apps::McpAuthoringStage::Approved
    );

    // A genuinely NEW workflow run — the remedy the error names — is
    // still allowed, because it writes its own staging tree and cannot
    // touch the approved run's bytes.
    let second_workflow_run_id = format!("wf_restage_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &second_workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": second_workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": template_id_for_scaffold_surface("dom"),
            "reason": "a fresh run after the first approval",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection for the second run");
    let second_handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle")
        .to_string();
    broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": second_workflow_run_id,
            "validated_selection_handle": second_handle,
            "quality_level": "fast",
            "name": "a name for the fresh run",
            "brief": "a brief for the fresh run",
        }))
        .await
        .expect("a NEW workflow run must still be allowed to stage");
    // The approved run's bytes are still exactly what the sheet showed.
    assert_eq!(
        evidence_before,
        fs::read_to_string(&approved_evidence_path).expect("approved staging evidence"),
        "a second run must write its own staging tree, never the approved run's"
    );
}

/// r2-tests-honesty-013: `stage_create` is the only production writer of
/// `evidence.json` and always writes `name`/`brief`, so the ONLY way
/// either key is missing is on-disk corruption or a pre-fix staging tree
/// left over from before this shape existed. `load_create_proposal_context`
/// hard-fails on that with `create_staging_evidence_invalid`, but nothing
/// pinned the label — a future edit could silently fall back to the shell
/// record's `untitled` placeholder instead and every other test would
/// stay green, because they all go through `stage_create` itself.
#[tokio::test]
async fn staged_evidence_missing_name_or_brief_is_a_named_hard_fail() {
    async fn stage_with_evidence_missing(
        broker: &Arc<LocalAppsHostBroker>,
        app_id: &str,
        drop_key: &str,
    ) -> String {
        let workflow_run_id = format!(
            "wf_evidence_gap_{drop_key}_{}",
            uuid::Uuid::new_v4().simple()
        );
        let catalog = crate::mobile::local_app_template_catalog::catalog_view()
            .expect("template catalog");
        let selector_capability =
            crate::mobile::local_app_template_catalog::issue_selector_capability(
                &broker.root,
                app_id,
                &workflow_run_id,
            )
            .expect("selector capability");
        let selection = broker
            .validate_template_selection(json!({
                "app_id": app_id,
                "workflow_run_id": workflow_run_id,
                "catalog_digest": catalog.catalog_digest,
                "template_id": "react-dom-r4",
                "reason": "evidence gap test",
                "rejected": [],
                "selector_capability": selector_capability,
            }))
            .await
            .expect("validated selection");
        let handle = selection["validated_selection_handle"]
            .as_str()
            .expect("selection handle")
            .to_string();
        broker
            .stage_create(json!({
                "app_id": app_id,
                "workflow_run_id": workflow_run_id,
                "validated_selection_handle": handle,
                "quality_level": "fast",
                "name": TEST_DEFAULT_APP_NAME,
                "brief": TEST_DEFAULT_APP_BRIEF,
            }))
            .await
            .expect("stage create");
        let evidence_path = broker
            .create_staging_root(app_id, &workflow_run_id, &handle)
            .join("evidence.json");
        let mut evidence: Value =
            serde_json::from_str(&fs::read_to_string(&evidence_path).expect("staged evidence"))
                .expect("parse staged evidence");
        evidence
            .as_object_mut()
            .expect("evidence object")
            .remove(drop_key);
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&evidence).expect("serialize corrupted evidence"),
        )
        .expect("rewrite staged evidence without the key under test");
        workflow_run_id
    }

    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;

    let missing_name_run = stage_with_evidence_missing(&broker, &shell.id, "name").await;
    let error = broker
        .load_create_proposal_context(&shell.id, &missing_name_run)
        .expect_err("staged evidence missing `name` must be a named hard fail");
    assert!(
        error.contains("create_staging_evidence_invalid") && error.contains("missing name"),
        "expected the missing-name hard fail to be named, got: {error}"
    );

    let missing_brief_run = stage_with_evidence_missing(&broker, &shell.id, "brief").await;
    let error = broker
        .load_create_proposal_context(&shell.id, &missing_brief_run)
        .expect_err("staged evidence missing `brief` must be a named hard fail");
    assert!(
        error.contains("create_staging_evidence_invalid") && error.contains("missing brief"),
        "expected the missing-brief hard fail to be named, got: {error}"
    );
}

/// r4-failure-paths-09 (b): `stage_create` is atomic per FILE but was not
/// atomic as a transaction — an I/O failure anywhere between
/// `create_dir_all(<staging>)` and the final `evidence.json` rename left a
/// half-materialized tree under
/// `.lingxi-build-state/template-candidates/<app>/<run>/staging/<handle>`
/// that nothing ever reclaimed, and `load_create_proposal_context` reads
/// `design-spec.json` out of exactly that directory.
///
/// Poison the ONE step that a test can make fail from outside without
/// touching the production code: `stage_create` creates
/// `<staging>/.lingxi/` for the seeded MCP flow contexts, so planting a
/// regular FILE at that path makes `create_dir_all` fail there. With the
/// reaper the whole staging tree is gone afterwards; without it the tree
/// (including the `.lingxi` poison) survives, which is what this pins.
#[tokio::test]
async fn a_failed_stage_create_reclaims_its_partial_staging_tree() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workflow_run_id = format!("wf_partial_stage_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "partial staging reclaim test",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle")
        .to_string();

    let staging_root = broker.create_staging_root(&shell.id, &workflow_run_id, &handle);
    fs::create_dir_all(&staging_root).expect("pre-create the staging root");
    // A regular file where `stage_create` needs a directory.
    fs::write(staging_root.join(".lingxi"), b"poison").expect("plant the poison file");
    // Vacuity guard: everything below is meaningless unless the poison is
    // really sitting on the path `stage_create` is about to use.
    assert!(
        staging_root.join(".lingxi").is_file(),
        "the poison must be a FILE at <staging>/.lingxi or stage_create never fails here"
    );

    let error = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "quality_level": "fast",
            "name": TEST_DEFAULT_APP_NAME,
            "brief": TEST_DEFAULT_APP_BRIEF,
        }))
        .await
        .expect_err("stage_create must fail once <staging>/.lingxi is a regular file");
    assert!(
        error.contains("create staged MCP flow context directory"),
        "the failure must be the planted one, not some earlier refusal that never \
         reached the staging materialization: {error}"
    );
    assert!(
        !staging_root.exists(),
        "a failed stage_create must reclaim its partial staging tree, but {} still exists",
        staging_root.display()
    );
}

/// The other side of the same guard: a COMPLETED staging tree — one that
/// carries the `evidence.json` commit marker — must survive a later failed
/// `stage_create` for the same run and handle. Without this the reaper
/// would be a data-loss bug of its own, because re-staging the same
/// not-yet-approved run is explicitly allowed.
#[tokio::test]
async fn a_committed_staging_tree_survives_a_later_failed_stage_create() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workflow_run_id = format!("wf_committed_stage_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "committed staging survival test",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle")
        .to_string();
    let stage_input = json!({
        "app_id": shell.id,
        "workflow_run_id": workflow_run_id,
        "validated_selection_handle": handle,
        "quality_level": "fast",
        "name": TEST_DEFAULT_APP_NAME,
        "brief": TEST_DEFAULT_APP_BRIEF,
    });
    broker
        .stage_create(stage_input.clone())
        .await
        .expect("first stage_create must succeed");
    let staging_root = broker.create_staging_root(&shell.id, &workflow_run_id, &handle);
    assert!(
        staging_root.join("evidence.json").is_file(),
        "vacuity guard: the first stage must have left the commit marker behind"
    );

    // Poison the SAME staging tree so the second call fails after
    // `create_dir_all(<staging>)` — the marker must protect it anyway.
    fs::remove_dir_all(staging_root.join(".lingxi")).expect("clear the staged context dir");
    fs::write(staging_root.join(".lingxi"), b"poison").expect("plant the poison file");
    let error = broker
        .stage_create(stage_input)
        .await
        .expect_err("the second stage_create must fail on the planted poison");
    assert!(
        error.contains("create staged MCP flow context directory"),
        "the failure must be the planted one: {error}"
    );
    assert!(
        staging_root.join("evidence.json").is_file(),
        "a completed staging candidate must survive a later failed stage_create, but {} \
         lost its commit marker",
        staging_root.display()
    );
}

/// r4-failure-paths-09 (a): `stage_create` records the digest of the exact
/// `design-spec.json` bytes it commits as `evidence.json`'s
/// `designSpecSha256`, and `load_create_proposal_context` re-hashes
/// whatever `design-spec.json` it finds — but nothing compared the two, so
/// a design spec left behind by an earlier run in the same staging
/// directory was adopted silently as this run's confirmed design.
#[tokio::test]
async fn a_design_spec_that_does_not_match_its_recorded_digest_is_a_named_hard_fail() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workflow_run_id = format!("wf_design_digest_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "design digest test",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle")
        .to_string();
    broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "quality_level": "fast",
            "name": TEST_DEFAULT_APP_NAME,
            "brief": TEST_DEFAULT_APP_BRIEF,
            "design_spec": {"acceptance_checks": ["the staged spec"]},
        }))
        .await
        .expect("stage create with a design spec");
    let staging_root = broker.create_staging_root(&shell.id, &workflow_run_id, &handle);
    let design_path = staging_root.join("design-spec.json");
    // Vacuity guard: the swap below only tests anything if a design spec
    // was staged at all and evidence really recorded a digest for it.
    assert!(
        design_path.is_file(),
        "the staged design spec must exist before it can be swapped"
    );
    let evidence: Value = serde_json::from_str(
        &fs::read_to_string(staging_root.join("evidence.json")).expect("staged evidence"),
    )
    .expect("parse staged evidence");
    assert!(
        evidence["designSpecSha256"].as_str().is_some(),
        "evidence.json must record designSpecSha256 for this gate to compare anything: \
         {evidence}"
    );
    // A clean staging tree must still load — otherwise a green failure
    // below would prove nothing about the digest comparison.
    broker
        .load_create_proposal_context(&shell.id, &workflow_run_id)
        .expect("an untouched staging tree must still load");

    fs::write(
        &design_path,
        serde_json::to_vec_pretty(&json!({"acceptance_checks": ["a foreign spec"]}))
            .expect("serialize the foreign design spec"),
    )
    .expect("swap the staged design spec");
    let error = broker
        .load_create_proposal_context(&shell.id, &workflow_run_id)
        .expect_err("a design spec that does not match its recorded digest must be refused");
    assert!(
        error.contains("create_staging_evidence_invalid") && error.contains("designSpecSha256"),
        "expected a named designSpecSha256 mismatch, got: {error}"
    );

    // The absent/present mismatch is the same defect: deleting the file
    // while evidence still records a digest must also refuse.
    fs::remove_file(&design_path).expect("remove the staged design spec");
    let error = broker
        .load_create_proposal_context(&shell.id, &workflow_run_id)
        .expect_err("a missing design spec that evidence says was staged must be refused");
    assert!(
        error.contains("create_staging_evidence_invalid") && error.contains("designSpecSha256"),
        "expected a named designSpecSha256 mismatch for the missing file, got: {error}"
    );
}

/// WP-C gate (`r4-engine-core-01`): a SECOND `create_without_mcp` call for
/// a `workflow_run_id` whose candidate journal is already sealed at
/// `Approved` must reuse that approval, the sealed journal stays
/// `Approved` rather than being rewritten back to `Prepared`, and the
/// answer must be one the create flow can actually spend: a non-empty
/// `receipt_id` that `LocalAppScaffold` can claim. Before the reuse arm
/// existed, the second call rebuilt a fresh `Prepared` journal and
/// solicited a fresh answer unconditionally. A reuse arm that answers
/// with `receipt_id: null` would still die — the scaffold step requires a
/// non-empty receipt whenever `approved` is true — which is why the
/// receipt is asserted here too.
///
/// The retired native create-confirmation sheet is deliberately NOT
/// exercised: the fixture approves under
/// [`CreateApprovalAuthority::ApprovedPlan`], the authority the
/// plan-driven `LocalAppPrepare` path passes in production.
///
/// The tail covers the one case where reuse legitimately cannot hand out a
/// receipt: a scaffold already holds a live claim. That must fail fast
/// with a named `create_approval_in_flight:` error, so no answer is
/// solicited and then discarded.
#[tokio::test]
async fn approve_mcp_proposal_reuses_an_already_approved_create_journal_without_a_second_native_sheet(
) {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, _service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &_service).await;
    let workflow_run_id = format!("wf_reuse_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id.clone(),
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "reuse arm coverage",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let contract_handle =
        stage_react_dom_authoring_contract(&broker, &shell.id, &workflow_run_id, handle).await;
    broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "contract_handle": contract_handle,
            "quality_level": "fast",
            "name": "Reuse arm",
            "brief": "second approval call must reuse the sealed journal",
        }))
        .await
        .expect("stage create");
    write_initial_staging_flow_contexts(&broker, &shell.id, &workflow_run_id, handle);

    // The native create-confirmation sheet is retired; the Host seals a
    // create approval itself once the user has approved a plan
    // (`CreateApprovalAuthority::ApprovedPlan`, the only production
    // producer). A bare `approve_mcp_proposal` is refused with
    // `create_requires_approved_plan`, so this fixture approves under the
    // same authority the prepare path uses.
    let first_approved = broker
        .approve_mcp_proposal_with(
            json!({
                "app_id": shell.id,
                "workflow_run_id": workflow_run_id,
                "create_without_mcp": true,
            }),
            CreateApprovalAuthority::ApprovedPlan,
        )
        .await
        .expect("approve plain create");
    assert_eq!(first_approved["status"], "create_approved_no_mcp");
    let first_receipt_id = first_approved["receipt_id"]
        .as_str()
        .expect("first create receipt")
        .to_string();

    let sealed_journal =
        local_apps::load_candidate_journal(&broker.layout(&shell.id).expect("layout"))
            .expect("sealed journal after first approval");
    assert_eq!(
        sealed_journal.stage,
        local_apps::McpAuthoringStage::Approved
    );

    // A SECOND identical call must reuse the sealed approval and return
    // promptly — it must NOT rebuild a `Prepared` journal or solicit a
    // fresh answer.
    let second_approved = broker
        .approve_mcp_proposal_with(
            json!({
                "app_id": shell.id,
                "workflow_run_id": workflow_run_id,
                "create_without_mcp": true,
            }),
            CreateApprovalAuthority::ApprovedPlan,
        )
        .await
        .expect("reuse arm must return the existing approval, not an error");
    assert_eq!(second_approved["status"], "create_approved_no_mcp");
    assert_eq!(second_approved["approved"], true);
    assert!(
        broker.pending_create_confirmations.lock().await.is_empty(),
        "the reuse arm must leave no native create-confirmation sheet outstanding"
    );

    // The reuse answer must be USABLE, not merely prompt. `LocalAppPrepare`
    // rejects `approved: true` without a non-empty `receipt_id`
    // (`prepare_state_invalid: the sealed create approval has no receipt`),
    // so a `null` receipt here would replace the re-asked-approval dead end
    // with an unnamed failure on a creation the user actually approved.
    let second_receipt_id = second_approved["receipt_id"].as_str().unwrap_or_else(|| {
        panic!(
            "the reuse arm returned receipt_id={:?}, which is not a string: \
             LocalAppPrepare requires a non-empty receipt_id whenever approved is true, \
             so this retry can never complete",
            second_approved["receipt_id"]
        )
    });
    assert!(
        !second_receipt_id.is_empty(),
        "the reuse arm returned an empty receipt_id; LocalAppPrepare cannot consume it"
    );
    let second_receipt_id = second_receipt_id.to_string();

    // The sealed journal must be untouched — still Approved, not
    // downgraded back to Prepared by a rebuilt candidate.
    let journal_after =
        local_apps::load_candidate_journal(&broker.layout(&shell.id).expect("layout"))
            .expect("journal must still exist after the reuse call");
    assert_eq!(
        journal_after.stage,
        local_apps::McpAuthoringStage::Approved,
        "the reuse arm must not rewrite the sealed Approved journal back to Prepared"
    );
    assert_eq!(
        journal_after.proposal_sha256, sealed_journal.proposal_sha256,
        "the reuse arm must not mint a new candidate proposal over the sealed one"
    );

    // The reused receipt must actually be spendable by the scaffold step.
    broker
        .pending_mcp_receipts
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .claim_candidate(
            &second_receipt_id,
            &shell.id,
            &workflow_run_id,
            &journal_after.approval_contract_sha256,
            &journal_after.proposal_sha256,
            now_ms(),
        )
        .expect(
            "the receipt handed back by the reuse arm must be claimable by LocalAppScaffold",
        );

    // With that claim now live (an in-flight scaffold), a THIRD call must
    // still fail fast with a named in-flight error rather than minting a
    // second live receipt.
    let third_error = broker
        .approve_mcp_proposal_with(
            json!({
                "app_id": shell.id,
                "workflow_run_id": workflow_run_id,
                "create_without_mcp": true,
            }),
            CreateApprovalAuthority::ApprovedPlan,
        )
        .await
        .expect_err(
            "a third call while the receipt is claimed must not hand out a second live receipt",
        );
    assert!(
        third_error.starts_with("create_approval_in_flight:"),
        "the in-flight refusal must be named, got {third_error:?}"
    );
    assert!(
        broker.pending_create_confirmations.lock().await.is_empty(),
        "the in-flight refusal must not leave a native create-confirmation sheet outstanding"
    );
    assert_ne!(
        second_receipt_id, first_receipt_id,
        "the reuse arm must mint its own receipt: handing back the first one would collide \
         with a scaffold that already claimed it"
    );
}

#[tokio::test]
async fn invalid_workflow_model_releases_the_unified_create_receipt_claim() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let (workflow_run_id, receipt_id) =
        approved_create_receipt(&broker, &shell.id, "dom").await;

    let error = broker
        .scaffold_shell_app_value(json!({
            "app_id": shell.id,
            "name": "bad workflow model",
            "brief": "b",
            "workflow_run_id": workflow_run_id,
            "receipt_id": receipt_id,
            "workflow_model": 123,
        }))
        .await
        .expect_err("invalid workflow_model must fail before scaffold");
    assert!(error.contains("workflow_model must be a string"), "{error}");

    broker
        .scaffold_shell_app_value(json!({
            "app_id": shell.id,
            "name": "valid workflow model",
            "brief": "b",
            "workflow_run_id": workflow_run_id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("claim must have been released for retry");
    assert!(service.record(&shell.id).await.expect("record").scaffolded);
}

#[tokio::test]
async fn create_review_surface_binds_staged_design_spec_digest() {
    let (_root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workflow_run_id = format!("wf_design_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "bind design review surface",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let contract_handle =
        stage_react_dom_authoring_contract(&broker, &shell.id, &workflow_run_id, handle).await;
    let design_spec = json!({
        "runtime_family": "react_dom",
        "acceptance_checks": ["render list", "save item"],
        "summary": "two-screen recipe list"
    });
    let stage = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "contract_handle": contract_handle,
            "quality_level": "balanced",
            "name": "Recipe list",
            "brief": "two-screen recipe list",
            "design_spec": design_spec,
        }))
        .await
        .expect("stage create");
    assert_eq!(stage["ok"], true);
    assert!(stage["design_spec_sha256"].as_str().is_some(), "{stage}");
    write_initial_staging_flow_contexts(&broker, &shell.id, &workflow_run_id, handle);

    let manifest = load_manifest(&broker.layout(&shell.id).expect("layout")).expect("manifest");
    let validated = broker
        .validate_mcp_proposal(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "proposal": initial_mcp_proposal_fixture(&shell.id, manifest.revision),
        }))
        .await
        .expect("validate staged proposal");
    assert_eq!(
        validated["review_surface"]["initialCreate"]["designSpecSha256"],
        stage["design_spec_sha256"]
    );
    assert_eq!(
        validated["review_surface"]["initialCreate"]["designSpec"]["summary"],
        "two-screen recipe list"
    );
}

#[tokio::test]
async fn unscaffolded_create_scaffolds_builds_and_promotes_from_a_staged_candidate() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        Some(runtime.clone()),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;

    let workflow_run_id = format!("wf_e2e_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "full create e2e",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let contract_handle =
        stage_react_dom_authoring_contract(&broker, &shell.id, &workflow_run_id, handle).await;
    // WP5 gate (a): a distinctive staged name/brief, never echoed anywhere
    // else in this test, so the assertion below can only pass if the
    // native confirmation sheet actually rendered what was staged here.
    const STAGED_NAME: &str = "记账本";
    const STAGED_BRIEF: &str = "记录日常收支的小工具";
    let stage = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "contract_handle": contract_handle,
            "quality_level": "balanced",
            "name": STAGED_NAME,
            "brief": STAGED_BRIEF,
            "design_spec": {
                "runtime_family": "react_dom",
                "acceptance_checks": ["render shell"],
                "summary": "e2e design"
            }
        }))
        .await
        .expect("stage create");
    assert_eq!(stage["ok"], true);
    write_initial_staging_flow_contexts(&broker, &shell.id, &workflow_run_id, handle);
    let layout = broker.layout(&shell.id).expect("layout");
    let manifest = load_manifest(&layout).expect("manifest");
    let validated = broker
        .validate_mcp_proposal(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "proposal": initial_mcp_proposal_fixture(&shell.id, manifest.revision),
        }))
        .await
        .expect("validate mcp proposal");
    let approval_contract_sha256 = validated["approval_contract_sha256"]
        .as_str()
        .expect("approval digest")
        .to_string();

    let approval_task = tokio::spawn({
        let broker = broker.clone();
        let app_id = shell.id.clone();
        let workflow_run_id = workflow_run_id.clone();
        async move {
            broker
                .approve_mcp_proposal(json!({
                    "app_id": app_id,
                    "workflow_run_id": workflow_run_id,
                    "approval_contract_sha256": approval_contract_sha256,
                }))
                .await
        }
    });

    // The retired native create-confirmation sheet is replaced by the LIVE
    // MCP-proposal approval (`McpProposalApprovalRequested`); this fixture
    // answers that one. The staged name/brief no longer render on a sheet,
    // but they must still reach the COMMITTED record (asserted below).
    let request_id = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) = broker
                .pending_mcp_proposal_approvals
                .lock()
                .await
                .keys()
                .next()
                .cloned()
            {
                break request_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mcp proposal approval request");
    assert!(
        broker
            .resolve_mcp_proposal_approval(&request_id, true)
            .await
    );
    let approval = approval_task
        .await
        .expect("approval task")
        .expect("approved");
    let receipt_id = approval["receipt_id"].as_str().expect("receipt id");

    // WP5 gate (b): echo back a DIFFERENT name/brief than what was staged.
    // The candidate staged through `LocalAppStageCreate` must still win —
    // proving `LocalAppScaffold` commits the staged values, not whatever
    // the model happens to send at scaffold time.
    broker
        .scaffold_shell_app_value(json!({
            "app_id": shell.id,
            "name": "Model Echoed A Different Name",
            "brief": "model echoed a different brief",
            "workflow_run_id": workflow_run_id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("scaffold from unified create receipt");
    let record = service.record(&shell.id).await.expect("record");
    assert!(record.scaffolded);
    assert_eq!(
        record.name, STAGED_NAME,
        "scaffold must commit the staged name, not the model's echoed value"
    );
    assert_eq!(
        record.brief, STAGED_BRIEF,
        "scaffold must commit the staged brief, not the model's echoed value"
    );
    let manifest = load_manifest(&layout).expect("scaffolded manifest");
    assert_eq!(
        manifest
            .template_origin
            .as_ref()
            .expect("template origin")
            .template_id,
        "react-dom-r4"
    );
    assert!(
        workspace_of(&root, &shell.id).join("app/app.jsx").is_file(),
        "staged template must be committed into the real workspace"
    );
    let active_contexts: BTreeMap<String, local_apps::AppMcpFlowContext> =
        serde_json::from_slice(
            &fs::read(workspace_of(&root, &shell.id).join(".lingxi/mcp-flow-contexts.json"))
                .expect("active MCP flow contexts"),
        )
        .expect("parse active MCP flow contexts");
    assert_eq!(
        active_contexts
            .get("runtime-status-flow")
            .expect("runtime-status-flow")
            .source,
        local_apps::FlowSource::Active
    );

    broker
        .build_app(json!({ "app_id": shell.id }))
        .await
        .expect("build scaffolded app");
    let qa = broker
        .qa_mcp_candidate(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
        }))
        .await
        .expect("qa candidate");
    assert_eq!(qa["isolation"], "passed");
    assert_eq!(
        qa["isolation_evidence"]["rejection"]
            .as_str()
            .expect("cross-app rejection")
            .split(':')
            .next(),
        Some("cross_app_flow")
    );
    let promoted = broker
        .promote_mcp_candidate(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
        }))
        .await
        .expect("promote candidate");
    assert_eq!(promoted["publication_state"], "published_unverified");
    // r2-never-wired-01: `AppEventDto::VerificationSummaryChanged` had
    // zero producers, so a client's per-app verification fields could
    // never become non-nil. Promoting an MCP candidate is the point at
    // which the app first leaves Draft, so it must emit the summary.
    let verification_publication_state = sink
        .events()
        .await
        .into_iter()
        .rev()
        .find_map(|event| match event {
            ClientEvent::AppEvent {
                event:
                    AppEventDto::VerificationSummaryChanged {
                        app_id,
                        publication_state,
                        ..
                    },
            } if app_id == shell.id => Some(publication_state),
            _ => None,
        })
        .expect("promoting an MCP candidate must emit VerificationSummaryChanged for the app");
    assert_eq!(
        verification_publication_state,
        AppWorkflowStateDto::PublishedUnverified
    );
    let promoted_manifest = load_manifest(&layout).expect("promoted manifest");
    assert!(promoted_manifest.active_mcp_catalog.is_some());
    let promoted_settings = load_mcp_settings(&layout).expect("promoted MCP settings");
    assert!(
        !promoted_settings.enabled,
        "an approved Local App MCP surface must remain off until the user enables it"
    );
    assert!(
        promoted_settings
            .enabled_tools
            .iter()
            .any(|name| name == "runtime_status"),
        "promotion may preselect approved tools without exposing them"
    );
    let flow_result = broker
        .execute_mcp_flow_value(json!({
            "app_id": shell.id,
            "tool_name": "runtime_status",
            "catalog_sha256": promoted["catalog_sha256"],
            "input": {"query": "hello"},
        }))
        .await
        .expect("promoted tool executes through active contexts");
    assert!(flow_result.get("state").is_some(), "{flow_result}");
}

/// WP-MCP-intent gate: an mcp_intent staged through `LocalAppStageCreate`
/// must survive the whole stage → approve → scaffold chain onto the
/// COMMITTED record, and the formal contract `LocalAppScaffold` writes
/// must carry it — the same shape as
/// `unscaffolded_create_uses_single_confirmation_then_scaffolds_builds_and_promotes`'s
/// WP5 gate for name/brief, but for the new field.
#[tokio::test]
async fn staged_mcp_intent_survives_create_and_reaches_the_formal_contract() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        Some(runtime.clone()),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;

    let workflow_run_id = format!("wf_mcp_intent_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "mcp intent staging e2e",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let contract_handle =
        stage_react_dom_authoring_contract(&broker, &shell.id, &workflow_run_id, handle).await;
    const STAGED_NAME: &str = "记账本";
    const STAGED_BRIEF: &str = "记录日常收支的小工具";
    let stage = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "contract_handle": contract_handle,
            "quality_level": "fast",
            "name": STAGED_NAME,
            "brief": STAGED_BRIEF,
            "mcp_intent": {"status": "requested", "capabilities": ["github", "google-drive"]},
        }))
        .await
        .expect("stage create");
    assert_eq!(stage["ok"], true);
    write_initial_staging_flow_contexts(&broker, &shell.id, &workflow_run_id, handle);
    let layout = broker.layout(&shell.id).expect("layout");
    let manifest = load_manifest(&layout).expect("manifest");
    let validated = broker
        .validate_mcp_proposal(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "proposal": initial_mcp_proposal_fixture(&shell.id, manifest.revision),
        }))
        .await
        .expect("validate mcp proposal");
    let approval_contract_sha256 = validated["approval_contract_sha256"]
        .as_str()
        .expect("approval digest")
        .to_string();

    let approval_task = tokio::spawn({
        let broker = broker.clone();
        let app_id = shell.id.clone();
        let workflow_run_id = workflow_run_id.clone();
        async move {
            broker
                .approve_mcp_proposal(json!({
                    "app_id": app_id,
                    "workflow_run_id": workflow_run_id,
                    "approval_contract_sha256": approval_contract_sha256,
                }))
                .await
        }
    });
    // The retired create-confirmation sheet was replaced by the LIVE
    // MCP-proposal approval (`McpProposalApprovalRequested`); a bare
    // `approve_mcp_proposal` without `create_without_mcp` takes that path.
    let request_id = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) = broker
                .pending_mcp_proposal_approvals
                .lock()
                .await
                .keys()
                .next()
                .cloned()
            {
                break request_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mcp proposal approval request");
    assert!(
        broker
            .resolve_mcp_proposal_approval(&request_id, true)
            .await
    );
    let approval = approval_task
        .await
        .expect("approval task")
        .expect("approved");
    let receipt_id = approval["receipt_id"].as_str().expect("receipt id");

    broker
        .scaffold_shell_app_value(json!({
            "app_id": shell.id,
            "name": STAGED_NAME,
            "brief": STAGED_BRIEF,
            "workflow_run_id": workflow_run_id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("scaffold from unified create receipt");

    let record = service.record(&shell.id).await.expect("record");
    assert!(record.scaffolded);
    assert_eq!(
        record.mcp_intent,
        Some(local_apps::AppMcpIntent::Requested {
            capabilities: vec!["github".to_string(), "google-drive".to_string()]
        }),
        "the staged mcp_intent must land on the committed record, not be dropped or genericized"
    );

    let contract = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("read the formal contract");
    assert!(
        contract.contains("MCP intent: asked during creation; the user asked for MCP access to github, google-drive."),
        "the formal contract must carry the recorded MCP intent: {contract}"
    );

    // Scaffold success is a reclaim point too: the run's create staging
    // (a full template copy plus evidence.json) is spent once the
    // scaffold commits.
    let staging = broker
        .root
        .join(".lingxi-build-state/template-candidates")
        .join(&shell.id)
        .join(&workflow_run_id)
        .join("staging");
    assert!(
        !staging.exists(),
        "a committed scaffold must reclaim its create staging: {}",
        staging.display()
    );
}

/// Cheap unit-level complement to the e2e gate above: exercises every
/// `mcp_intent` shape `formal_workspace_contract` renders — including
/// `Declined`, which the full pipeline test above does not cover — without
/// paying for a broker/service fixture.
#[test]
fn mcp_intent_contract_line_covers_every_shape() {
    assert_eq!(mcp_intent_contract_line(None), "");
    assert_eq!(
        mcp_intent_contract_line(Some(&local_apps::AppMcpIntent::Declined)),
        "MCP intent: asked during creation; the user declined MCP for this app.\n\n"
    );
    assert_eq!(
        mcp_intent_contract_line(Some(&local_apps::AppMcpIntent::Requested {
            capabilities: vec!["github".to_string(), "google-drive".to_string()]
        })),
        "MCP intent: asked during creation; the user asked for MCP access to github, google-drive.\n\n"
    );
}

/// WP-MCP-intent gate, second state: `Declined` must cross the STAGING
/// seam, not just render.
///
/// `staged_mcp_intent_survives_create_and_reaches_the_formal_contract`
/// stages only `requested`; `commit_scaffold_distinguishes_never_asked_from_declined`
/// starts BELOW staging; `mcp_intent_contract_line_covers_every_shape` does
/// no serde at all. So nothing proved `{"status":"declined"}` survives
/// `parse_staged_mcp_intent` → `evidence.json` → `load_create_proposal_context`.
/// Declined is the whole reason this field is not a bool: if it were the one
/// shape that failed to round-trip, every declining user would silently
/// become "never asked" and be re-prompted forever, and the rest of the
/// suite would stay green. This stops at the scaffold seed rather than
/// running the full confirmation e2e — the seed is the value
/// `scaffold_shell_app_value` commits, and the e2e above already pins the
/// seed → record → contract half.
#[tokio::test]
async fn a_staged_declined_mcp_intent_survives_the_staging_seam() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        Some(runtime.clone()),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;

    let workflow_run_id = format!("wf_mcp_declined_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "declined mcp intent staging",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let stage = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "quality_level": "fast",
            "name": "无 MCP 的记账本",
            "brief": "用户问过 MCP 但拒绝了",
            "mcp_intent": {"status": "declined"},
        }))
        .await
        .expect("stage create with a declined intent");
    assert_eq!(stage["ok"], true);
    write_initial_staging_flow_contexts(&broker, &shell.id, &workflow_run_id, handle);

    let seed = broker
        .load_create_scaffold_seed(&shell.id, &workflow_run_id)
        .expect("scaffold seed reads the staged evidence back");
    assert_eq!(
        seed.mcp_intent,
        Some(local_apps::AppMcpIntent::Declined),
        "a staged `declined` must read back as Declined, never collapse into None \
         (\"never asked\") — that collapse is what would re-prompt the user forever"
    );
    assert_ne!(
        seed.mcp_intent, None,
        "asked-and-declined must stay distinguishable from never-asked at the staging seam"
    );
    assert_eq!(
        mcp_intent_contract_line(seed.mcp_intent.as_ref()),
        "MCP intent: asked during creation; the user declined MCP for this app.\n\n"
    );
}

/// r1-backlog-scaffold-build-07: the multi-minute native create
/// confirmation sits BETWEEN `stage_create` digesting the template into
/// `evidence.json`'s `stagedFiles` and `load_create_scaffold_seed`
/// copying that template into the real workspace. Before this fix,
/// nothing re-checked the digests at landing time, so a file tampered
/// with during that window would be copied in unverified.
#[tokio::test]
async fn create_scaffold_seed_rejects_a_staged_template_file_tampered_after_staging() {
    let (_root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;

    let workflow_run_id = format!("wf_tamper_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "staged template tamper test",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle")
        .to_string();
    let stage = broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "quality_level": "fast",
            "name": "Tamper target",
            "brief": "template file changes after staging",
        }))
        .await
        .expect("stage create");
    assert_eq!(stage["ok"], true);

    // Before tampering, the seed must load cleanly — proves the gate is
    // not merely refusing everything.
    assert!(
        broker
            .load_create_scaffold_seed(&shell.id, &workflow_run_id)
            .is_ok(),
        "an untouched staged template must load"
    );

    // Simulate the tamper window: rewrite one staged template file after
    // `stage_create` already digested it into `evidence.json`.
    let tampered_path = broker
        .create_staging_root(&shell.id, &workflow_run_id, &handle)
        .join("template")
        .join(".lingxi/dependencies/requested.json");
    std::fs::write(&tampered_path, b"{\"tampered\": true}")
        .expect("overwrite staged template file");

    let error = broker
        .load_create_scaffold_seed(&shell.id, &workflow_run_id)
        .expect_err("a staged template file changed after staging must be rejected");
    assert!(
        error.contains("create_staging_template_invalid")
            && error.contains(".lingxi/dependencies/requested.json"),
        "expected the mismatch to name the tampered path, got: {error}"
    );

    // And landing must actually go through this gate: the app is never
    // scaffolded from tampered bytes.
    assert!(!service.record(&shell.id).await.expect("record").scaffolded);
}

#[tokio::test]
async fn qa_mcp_candidate_rejects_tampered_active_contexts() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let layout = broker.layout(&shell.id).expect("layout");
    let (workflow_run_id, receipt_id) =
        approved_create_receipt(&broker, &shell.id, "dom").await;
    broker
        .scaffold_shell_app_value(json!({
            "app_id": shell.id,
            "name": "Tampered contexts",
            "brief": "qa must re-read active contexts",
            "workflow_run_id": workflow_run_id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("scaffold");
    broker
        .build_app(json!({ "app_id": shell.id }))
        .await
        .expect("build scaffolded app");
    let workspace = workspace_of(&root, &shell.id);
    let mut contexts: BTreeMap<String, local_apps::AppMcpFlowContext> = serde_json::from_slice(
        &fs::read(workspace.join(".lingxi/mcp-flow-contexts.json"))
            .expect("active MCP flow contexts"),
    )
    .expect("parse active MCP flow contexts");
    contexts
        .get_mut("runtime-status-flow")
        .expect("runtime-status-flow")
        .app_id = "other-app".into();
    fs::write(
        workspace.join(".lingxi/mcp-flow-contexts.json"),
        serde_json::to_vec_pretty(&contexts).expect("serialize tampered contexts"),
    )
    .expect("write tampered contexts");

    let error = broker
        .qa_mcp_candidate(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
        }))
        .await
        .expect_err("QA must reject tampered active contexts");
    assert!(error.contains("cross_app_flow"), "{error}");

    let context = contexts
        .get_mut("runtime-status-flow")
        .expect("runtime-status-flow");
    context.app_id = shell.id.clone();
    context.flow.steps[0].capability = local_apps::CapabilityId::DataMutate;
    fs::write(
        workspace.join(".lingxi/mcp-flow-contexts.json"),
        serde_json::to_vec_pretty(&contexts).expect("serialize ceiling-drift contexts"),
    )
    .expect("write ceiling-drift contexts");
    let error = broker
        .qa_mcp_candidate(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
        }))
        .await
        .expect_err("QA must reject a Flow whose permission ceiling drifted");
    assert!(error.contains("permission_ceiling_drift"), "{error}");
    assert_eq!(
        local_apps::load_candidate_journal(&layout)
            .expect("candidate journal")
            .stage,
        local_apps::McpAuthoringStage::Approved,
        "a failed QA probe must not advance the durable journal"
    );
}

#[tokio::test]
async fn runtime_profiles_report_availability_cache_download_and_migrations() {
    let (_root, _service, broker) = create_broker(false, None).await;
    let profiles = broker
        .runtime_profiles_value(json!({}))
        .await
        .expect("runtime profiles")["profiles"]
        .as_array()
        .expect("profiles array")
        .clone();
    let react = profiles
        .iter()
        .find(|entry| entry["family"] == "react_dom")
        .expect("react_dom profile");
    assert_eq!(react["cache_status"], "download_required");
    assert_eq!(react["download_status"], "download_required");
    assert_eq!(react["available_migrations"], json!([]));

    for family in ["three_3d", "phaser_2d"] {
        let entry = profiles
            .iter()
            .find(|entry| entry["family"] == family)
            .unwrap_or_else(|| panic!("{family} profile"));
        assert_eq!(entry["cache_status"], "download_required");
        assert_eq!(
            entry["download_status"], "download_required",
            "source bundle availability must not claim an installed dependency tree"
        );
    }

    let babylon = profiles
        .iter()
        .find(|entry| entry["family"] == "babylon_3d")
        .expect("babylon profile");
    assert_eq!(babylon["available"], false);
    assert_eq!(babylon["cache_status"], "unavailable");
    assert_eq!(babylon["download_status"], "gated");
    assert_eq!(babylon["available_migrations"], json!([]));
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_profile_dependency_status_distinguishes_seed_and_shared_cache() {
    let root = TempDir::new().expect("tempdir");
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        AppRuntimeProfile::ReactDom,
    )
    .expect("react profile binding");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("react profile contract");
    let lock_digest = crate::mobile::local_app_runtime_profiles::lockfile_sha256(contract);

    // A configured seed with the exact lock is bundled, even though its
    // dependency tree has not been copied into the shared cache.
    let runtime_root = create_bundled_seed(root.path(), &lock_digest);
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        Some(runtime_root.clone()),
    );
    assert_eq!(
        broker.runtime_profile_dependency_availability(
            AppRuntimeProfile::ReactDom,
            binding.revision,
        ),
        RuntimeProfileDependencyAvailability::Bundled,
    );

    // Once the exact lock is represented by a verified shared snapshot,
    // the provenance changes to cached. The selector must not keep
    // claiming that it will use the device bundle.
    let snapshot = broker.dependency_snapshot_root(&lock_digest, PNPM_TOOLCHAIN_KEY);
    assert!(LocalAppsHostBroker::adopt_bundled_dependency_seed(
        &runtime_root,
        &lock_digest,
        &snapshot,
        PNPM_TOOLCHAIN_KEY
    )
    .expect("adopt matching seed"));
    assert_eq!(
        broker.runtime_profile_dependency_availability(
            AppRuntimeProfile::ReactDom,
            binding.revision,
        ),
        RuntimeProfileDependencyAvailability::Cached,
    );
}

#[tokio::test]
async fn a_failed_scaffold_releases_the_receipt_claim_for_retry() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let original_manifest = load_manifest(
        &AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout"),
    )
    .expect("shell manifest");
    let original_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("shell dependency record");
    let retriable_input =
        confirmed_scaffold_input(&broker, &shell.id, "打飞机", "b", "canvas").await;
    break_the_final_landing_step(&root, &shell.id);

    let first = broker
        .scaffold_shell_app_value(retriable_input.clone())
        .await
        .expect_err("broken landing must fail");
    assert!(first.contains("LINGXI.md"), "{first}");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    assert_eq!(
        load_manifest(&layout).expect("restored manifest"),
        original_manifest,
        "landing failure must restore the shell manifest"
    );
    assert_eq!(
        service
            .dependency_record(&shell.id)
            .await
            .expect("restored dependency record"),
        original_dependency,
        "landing failure must restore the service dependency cache"
    );
    assert!(
        !root
            .path()
            .join(local_apps::storage::scaffold_recovery_journal_rel(
                &shell.id
            ))
            .exists(),
        "a synchronous rollback must remove its recovery journal"
    );
    assert!(
        workspace_of(&root, &shell.id).join("LINGXI.md").is_dir(),
        "the exact shell workspace must be restored, including the injected failure fixture"
    );
    repair_the_final_landing_step(&root, &shell.id);

    broker
        .scaffold_shell_app_value(retriable_input)
        .await
        .expect("same receipt can retry after the claim is released");
    assert!(service.record(&shell.id).await.expect("record").scaffolded);
}

#[tokio::test]
async fn scaffold_failure_after_dependency_snapshot_restores_shell_before_record_commit() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let original_manifest = load_manifest(&layout).expect("shell manifest");
    let original_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("shell dependency record");
    let original_guided = fs::read(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("guided workspace contract");
    let original_index = break_index_commit(&root);
    let retriable_input = confirmed_scaffold_input(
        &broker,
        &shell.id,
        "回滚测试",
        "dependency snapshot then commit failure",
        "dom",
    )
    .await;

    let error = broker
        .scaffold_shell_app_value(retriable_input.clone())
        .await
        .expect_err("the occupied index must fail after dependency snapshot");
    assert!(error.contains("index.json"), "{error}");
    repair_index_commit(&root, &original_index);

    let after = service.record(&shell.id).await.expect("shell record");
    assert!(
        !after.scaffolded,
        "record.scaffolded is the final commit point"
    );
    assert_eq!(
        load_manifest(&layout).expect("restored manifest"),
        original_manifest
    );
    assert_eq!(
        service
            .dependency_record(&shell.id)
            .await
            .expect("restored dependency record"),
        original_dependency
    );
    assert_eq!(
        fs::read(workspace_of(&root, &shell.id).join("LINGXI.md"))
            .expect("restored guided contract"),
        original_guided
    );
    assert!(
        !root
            .path()
            .join(local_apps::storage::scaffold_recovery_journal_rel(
                &shell.id
            ))
            .exists(),
        "rollback must remove the durable journal after restoring the shell"
    );

    // The receipt claim is released and the repaired shell can retry from
    // the exact pre-landing state.
    broker
        .scaffold_shell_app_value(retriable_input)
        .await
        .expect("same receipt retries after rollback");
    assert!(service.record(&shell.id).await.expect("record").scaffolded);
}

#[tokio::test]
async fn cold_start_recovers_a_partial_scaffold_before_loading_the_app() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        None,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let original_manifest = load_manifest(&layout).expect("shell manifest");
    let original_guided =
        fs::read(workspace_of(&root, &shell.id).join("LINGXI.md")).expect("guided contract");
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::Canvas2d,
    )
    .expect("published canvas profile");
    let artifacts = scaffold_runtime_profile(Some(binding), local_apps::AppSurface::Canvas)
        .expect("scaffold artifacts");
    let target = crate::mobile::local_apps_build::LocalAppBuildTarget::from_runtime_binding(
        &artifacts.binding,
    )
    .expect("build target");
    let build_lock =
        local_apps::storage::lock_app_build(root.path(), &shell.id).expect("build lock");
    let recovery = local_apps::storage::begin_scaffold_recovery(
        root.path(),
        &shell.id,
        "冷启动回滚",
        "crash recovery",
    )
    .expect("durable recovery journal");
    stamp_scaffold_identity(
        &layout,
        "冷启动回滚",
        &artifacts.binding,
        builtin_template_origin(&artifacts.binding),
    )
    .expect("stamp partial identity");
    crate::mobile::local_apps_build::scaffold_workspace_initialized(&layout, target, true)
        .expect("land partial workspace");
    persist_runtime_profile_files(&workspace_of(&root, &shell.id), &artifacts)
        .expect("persist partial runtime files");
    // Simulate process death: neither commit nor rollback runs.
    std::mem::forget(recovery);
    drop(build_lock);
    drop(broker);
    drop(service);

    let loaded = local_apps::storage::load_all(root.path()).expect("cold-start recovery");
    assert_eq!(loaded.len(), 1);
    assert!(!loaded[0].record.scaffolded);
    assert_eq!(
        load_manifest(&layout).expect("restored manifest"),
        original_manifest
    );
    assert_eq!(
        fs::read(workspace_of(&root, &shell.id).join("LINGXI.md"))
            .expect("restored guided contract"),
        original_guided
    );
    assert!(
        !root
            .path()
            .join(local_apps::storage::scaffold_recovery_journal_rel(
                &shell.id
            ))
            .exists(),
        "cold-start recovery must consume the journal"
    );
}

#[tokio::test]
async fn runtime_profile_apps_fail_closed_on_dependency_input_drift() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "漂移测试", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    fs::write(
        workspace_of(&root, &shell.id).join("package.json"),
        "{\n  \"name\": \"tampered\"\n}\n",
    )
    .expect("tamper package.json");

    let error = broker
        .ensure_dependency_install(&shell.id, false)
        .await
        .expect_err("runtime-profile apps must not auto-repair dependency drift");
    assert!(error.contains("dependencies_dirty"), "{error}");
    // WP8: the drift error must not send the agent to retry
    // `LocalAppConfirmDependencyChange`/`LocalAppUpdateDependencies`. Both ARE
    // model-callable builtins; what they cannot do is repair snapshot drift, because
    // `update_dependencies` applies a host-minted receipt rather than re-deriving
    // the snapshot. See the reason recorded at the emit site.
    assert!(
        !error.contains("LocalAppConfirmDependencyChange")
            && !error.contains("LocalAppUpdateDependencies"),
        "{error}"
    );
    assert!(error.contains("report the drift"), "{error}");
}

/// r4-failure-paths-01: `ensure_dependency_install` transitions the
/// dependency record to `Installing` itself (its own
/// `start_dependency_install` call) and then spawns `run_dependency_install`
/// to do the actual work. That worker used to call the SAME transition
/// again through `install_scaffold_dependencies`, which always fails once
/// the state is already `Installing` -- so the install body never ran and
/// the record was stuck `Installing` forever, with `LocalAppBuild` polling
/// the full ten-minute timeout on every later attempt.
///
/// Requeue an already-scaffolded (dependencies `Ready`) app to exercise a
/// route where `ensure_dependency_install` itself performs the
/// `Installing` transition and spawns the worker (a `Queued` or `Failed`
/// record reaches that same transition without any requeue), then assert
/// the install the worker was spawned to run actually completes to
/// `Ready` -- not merely that no error is returned.
#[tokio::test]
async fn ensure_dependency_install_runs_the_install_its_worker_owns() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "双重安装", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let after_scaffold = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record after scaffold");
    assert_eq!(after_scaffold.state, AppDependencyState::Ready);

    // The same requeue `ensure_dependency_install` performs itself when it
    // detects a stale snapshot (local_apps_host.rs:3611-3616). This is the
    // route this test simulates; a `Queued` or `Failed` record reaches the
    // very same `start_dependency_install` at :3617 with no requeue at all.
    service
        .queue_dependency_install(&shell.id)
        .await
        .expect("requeue dependency install");

    let started = broker
        .ensure_dependency_install(&shell.id, false)
        .await
        .expect("ensure_dependency_install must accept a queued record");
    assert_eq!(
        started.state,
        AppDependencyState::Installing,
        "ensure_dependency_install must own the Queued -> Installing transition itself"
    );

    let mut observed = started.clone();
    for _ in 0..200 {
        observed = service
            .dependency_record(&shell.id)
            .await
            .expect("poll dependency record");
        if observed.state != AppDependencyState::Installing {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The terminal state is the whole criterion, and it is load-bearing:
    // `Ready` is only ever written by
    // `complete_dependency_install_with_metadata`, which on this path is
    // only reachable from `run_started_dependency_install`'s Ok arm --
    // i.e. only after `dependency_install_once` actually ran and
    // succeeded.
    //
    // Deliberately NOT asserted here, because neither discriminates the
    // bug from the fix on this path:
    //  - `install_attempts`: bumped by `start_dependency_install`, which
    //    `ensure_dependency_install` calls itself at :3617 before the
    //    worker is ever spawned, so it rises identically in the broken
    //    build.
    //  - a pnpm invocation count on the mock runtime: this requeue reuses
    //    the scaffold's dependency snapshot, so `dependency_install_once`
    //    takes the `snapshot_ready` fast path (:4455) and issues no
    //    isolated command at all -- measured: zero requests even when the
    //    install genuinely runs.
    assert_eq!(
        observed.state,
        AppDependencyState::Ready,
        "the worker spawned by ensure_dependency_install never completed the install it \
         started -- record left at {:?} (last_error={:?}) after a 2s poll window instead of \
         Ready; this is the double-start where the worker re-invokes \
         start_dependency_install and always fails because the state is already Installing, \
         so dependency_install_once is never reached and the app can never be built",
        observed.state,
        observed.last_error,
    );
}

/// A `ReceiptClaim` guard dropped WITHOUT `commit_claimed_candidate` ever
/// running — the shape of a Stop, a panic, or the 529 non-streaming
/// fallback dropping `scaffold_shell_app_value`'s future mid-transaction
/// — must release the claim, leaving the app's receipt slot re-issuable.
///
/// REGRESSION: before this guard existed, only the explicit `Err` arm at
/// the end of `scaffold_shell_app_value` called `release_claim`; any OTHER
/// exit (a dropped future among them) left the claim held forever, and
/// `McpReceiptBook::issue`'s in-use check answered `receipt_in_use` for
/// the life of the process — the app could never be created.
#[test]
fn a_dropped_receipt_claim_guard_leaves_the_slot_re_issuable() {
    let digest = "0".repeat(64);
    let claimed = local_apps::McpConfirmationReceipt::new(
        "app",
        "run-1",
        digest.clone(),
        digest.clone(),
        0,
    );
    let receipt_id = claimed.receipt_id.clone();
    let book = Arc::new(std::sync::Mutex::new(local_apps::McpReceiptBook::default()));
    book.lock().unwrap().issue(claimed).unwrap();
    book.lock()
        .unwrap()
        .claim_candidate(&receipt_id, "app", "run-1", &digest, &digest, 0)
        .unwrap();

    {
        // Simulates the scaffold transaction's future being dropped mid-
        // flight: the guard leaves scope WITHOUT `commit_claimed_candidate`
        // (the only other place that used to clear `claimed`) ever running.
        let _guard = ReceiptClaim::held(Arc::clone(&book), receipt_id.clone());
    }

    let reissued =
        local_apps::McpConfirmationReceipt::new("app", "run-2", digest.clone(), digest, 0);
    book.lock().unwrap().issue(reissued).expect(
        "the dropped ReceiptClaim guard must have released the claim so a fresh \
         receipt can be issued for this app -- otherwise the app can never be \
         (re)created until the process restarts",
    );
}

/// The guard is only worth having if the PRODUCTION transaction actually
/// holds one. The test above pins `ReceiptClaim`'s `Drop`; this one pins
/// the `let _receipt_claim = ReceiptClaim::held(..)` binding at the top of
/// `scaffold_shell_app_value` — delete that one line and this test goes
/// red while the one above stays green.
///
/// The drop is placed strictly between `claim_candidate` and
/// `commit_claimed_candidate` WITHOUT a timing race: the test takes
/// `lock_app_build` out from under the transaction first, and
/// `land_scaffold` blocks on that lock, so the future can never reach the
/// commit while the contender is alive. What is dropped is the real
/// `scaffold_shell_app_value` future — the shape of a Stop, a panic
/// unwinding the caller, or the 529 non-streaming fallback abandoning it.
///
/// It is also independent of the TTL escape hatch in
/// `McpReceiptBook::issue`: the re-issue below is minted at the same
/// `now_ms()` as the leaked slot, so the slot has NOT expired and the TTL
/// clause cannot be what lets the new receipt through.
#[tokio::test]
async fn dropping_the_scaffold_future_releases_the_claim_it_took() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let input = confirmed_scaffold_input(&broker, &shell.id, "打飞机", "b", "canvas").await;

    // Hold the per-app build lock so the transaction parks inside
    // `land_scaffold` instead of racing us to its commit point.
    let contender = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        let app_id = shell.id.clone();
        move || local_apps::storage::lock_app_build(&root, &app_id)
    })
    .await
    .expect("join the contender")
    .expect("the contender must take the build lock first");

    {
        let mut scaffolding = Box::pin(broker.scaffold_shell_app_value(input));
        assert!(
            timeout(Duration::from_millis(400), &mut scaffolding)
                .await
                .is_err(),
            "the transaction must still be IN FLIGHT — past `claim_candidate` \
             and parked on the build lock, short of `commit_claimed_candidate` \
             — for its drop to be the thing under test"
        );
        // ...and here it is dropped, never polled again: nothing further
        // in its body ever runs, `Err` arm included.
    }

    let digest = "0".repeat(64);
    let reissued = local_apps::McpConfirmationReceipt::new(
        shell.id.clone(),
        "run-after-the-drop",
        digest.clone(),
        digest,
        now_ms(),
    );
    broker
        .pending_mcp_receipts
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .issue(reissued)
        .expect(
            "dropping `scaffold_shell_app_value`'s own future must release the receipt \
             claim it took -- otherwise `McpReceiptBook::issue` answers `receipt_in_use` \
             for this app for the life of the process and it can never be created",
        );

    drop(contender);
}

/// r4-failure-paths-06: a create approval that does NOT complete must not
/// leave a durable `Prepared` candidate journal or candidate file on disk.
/// The native create-confirmation sheet is retired — a model-driven
/// `approve_mcp_proposal` (NativeSheet) is now refused with
/// `create_requires_approved_plan` — and by the time that refusal is
/// reached `McpCreateCandidateGuard` has already written the `Prepared`
/// journal/candidate, so its drop is exactly the cleanup under test.
#[tokio::test]
async fn a_refused_native_create_approval_cleans_up_the_candidate_state() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workflow_run_id = format!("wf_drop_create_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let selection = broker
        .validate_template_selection(json!({
            "app_id": shell.id.clone(),
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "drop mid create-approval",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect("validated selection");
    let handle = selection["validated_selection_handle"]
        .as_str()
        .expect("selection handle");
    let contract_handle =
        stage_react_dom_authoring_contract(&broker, &shell.id, &workflow_run_id, handle).await;
    broker
        .stage_create(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "contract_handle": contract_handle,
            "quality_level": "fast",
            "name": "Dropped create approval",
            "brief": "r4-failure-paths-06 regression",
        }))
        .await
        .expect("stage create");

    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");

    // A model-driven (NativeSheet) create approval is refused by name: the
    // create authority now comes only from the user's approved plan.
    let error = broker
        .approve_mcp_proposal(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "create_without_mcp": true,
        }))
        .await
        .expect_err(
            "a model-driven create approval must be refused: the approved plan is the only \
             authority that can seal a create",
        );
    assert!(
        error.starts_with("create_requires_approved_plan:"),
        "the refusal must be named, got {error:?}"
    );

    assert!(
        local_apps::load_candidate_journal(&layout).is_err(),
        "r4-failure-paths-06: a create approval that never completes must not leave a \
         durable Prepared candidate journal on disk"
    );
    assert!(
        broker
            .load_mcp_candidate(&shell.id, &workflow_run_id)
            .is_err(),
        "r4-failure-paths-06: a create approval that never completes must not leave a \
         durable candidate file on disk"
    );
}

fn dummy_native_approval_event() -> AppEventDto {
    AppEventDto::LocalAppOperationFailed {
        app_id: None,
        code: client_protocol::local_apps::LocalAppPluginErrorCodeDto::PluginDisabled,
        message: "r4-tests-honesty-05 fixture event; content is not under test".into(),
        request_id: None,
    }
}

/// r4-tests-honesty-05: the one-pending-per-app `approval_pending` guard
/// had zero test references anywhere. A second concurrent wait for the
/// SAME app on the SAME pending map must be refused rather than silently
/// replacing or racing the first.
#[tokio::test]
async fn wait_for_native_approval_refuses_a_second_concurrent_wait_for_the_same_app() {
    let root = TempDir::new().expect("tempdir");
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    let pending: Mutex<HashMap<String, PendingNativeApproval>> = Mutex::new(HashMap::new());

    let first = {
        let broker = &broker;
        let pending = &pending;
        Box::pin(broker.wait_for_native_approval_with_timeout(
            pending,
            "req-1".into(),
            "app-shared",
            dummy_native_approval_event(),
            Duration::from_secs(30),
        ))
    };
    tokio::pin!(first);
    // Park the first wait past its pending-map insert, short of its
    // (30s) deadline, so the second call below races a REAL pending
    // entry rather than an empty map.
    assert!(
        timeout(Duration::from_millis(50), &mut first)
            .await
            .is_err(),
        "the first wait must still be in flight for the guard to be exercised"
    );

    let second = broker
        .wait_for_native_approval_with_timeout(
            &pending,
            "req-2".into(),
            "app-shared",
            dummy_native_approval_event(),
            Duration::from_secs(30),
        )
        .await;
    assert_eq!(
        second,
        Err("approval_pending: this Local App already has a pending approval".into()),
        "a second wait for the same app must be refused while the first is pending, got {second:?}"
    );
}

/// r4-tests-honesty-05: the native-approval `Err(_)` timeout arm had zero
/// test references anywhere. It must both name the timeout AND clear the
/// pending-map entry it inserted -- otherwise a timed-out approval wedges
/// the app under the `approval_pending` guard above for the life of the
/// process.
#[tokio::test]
async fn wait_for_native_approval_timeout_arm_names_the_timeout_and_clears_the_pending_entry() {
    let root = TempDir::new().expect("tempdir");
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    let pending: Mutex<HashMap<String, PendingNativeApproval>> = Mutex::new(HashMap::new());

    let result = broker
        .wait_for_native_approval_with_timeout(
            &pending,
            "req-timeout".into(),
            "app-timeout",
            dummy_native_approval_event(),
            Duration::from_millis(20),
        )
        .await;
    assert_eq!(
        result,
        Err("native Local App approval timed out".into()),
        "got {result:?}"
    );
    assert!(
        pending.lock().await.is_empty(),
        "a timed-out approval must clear its own pending-map entry, or the \
         approval_pending guard wedges this app forever"
    );
}

/// r1-failure-paths-002: an aborted approval sheet (cancelled OR timed
/// out) was never proactively retracted on any client -- only the
/// in-flight workflow call learned about it via its `Err`. Clients
/// discard their pending sheet keyed on `request_id` (iOS's
/// `discardPendingApproval`), so this must emit a real
/// `AppEventDto::LocalAppOperationFailed` naming that exact
/// `request_id`/`app_id`, not merely "some event happened". The dummy
/// fixture event handed to `wait_for_native_approval_with_timeout` is
/// itself (coincidentally) a `LocalAppOperationFailed` with
/// `request_id: None`, so every assertion below keys off `request_id ==
/// Some(...)` to distinguish the NEW retraction emit from that unrelated
/// initial-request emit.
#[tokio::test]
async fn wait_for_native_approval_cancelled_and_timed_out_arms_emit_local_app_operation_failed()
{
    let root = TempDir::new().expect("tempdir");
    let sink = MockSink::arc();
    let broker =
        LocalAppsHostBroker::new(root.path().to_path_buf(), sink.clone(), None, false, None);
    let pending: Mutex<HashMap<String, PendingNativeApproval>> = Mutex::new(HashMap::new());

    // --- cancelled arm: drop the sender out from under the wait without
    // resolving it, exactly as a forced cleanup (e.g. app deletion) would.
    let wait = Box::pin(broker.wait_for_native_approval_with_timeout(
        &pending,
        "req-cancel".into(),
        "app-cancel",
        dummy_native_approval_event(),
        Duration::from_secs(30),
    ));
    tokio::pin!(wait);
    assert!(
        timeout(Duration::from_millis(50), &mut wait).await.is_err(),
        "the wait must still be in flight for the cancel to be exercised"
    );
    let dropped = pending.lock().await.remove("req-cancel");
    assert!(dropped.is_some(), "pending entry must exist to drop");
    drop(dropped);
    let result = wait.await;
    assert_eq!(
        result,
        Err("native Local App approval was cancelled".into()),
        "got {result:?}"
    );

    let events = sink.events().await;
    let cancel_failures: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::AppEvent {
                event:
                    AppEventDto::LocalAppOperationFailed {
                        app_id,
                        message,
                        request_id: Some(request_id),
                        ..
                    },
            } if request_id == "req-cancel" => Some((app_id.clone(), message.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancel_failures.len(),
        1,
        "expected exactly one LocalAppOperationFailed naming req-cancel, got {events:?}"
    );
    assert_eq!(cancel_failures[0].0, Some("app-cancel".to_string()));
    assert!(
        cancel_failures[0].1.contains("cancelled"),
        "message must name the cancellation, got {:?}",
        cancel_failures[0].1
    );

    // --- timeout arm ---
    let result = broker
        .wait_for_native_approval_with_timeout(
            &pending,
            "req-timeout-emit".into(),
            "app-timeout-emit",
            dummy_native_approval_event(),
            Duration::from_millis(20),
        )
        .await;
    assert_eq!(
        result,
        Err("native Local App approval timed out".into()),
        "got {result:?}"
    );

    let events = sink.events().await;
    let timeout_failures: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ClientEvent::AppEvent {
                event:
                    AppEventDto::LocalAppOperationFailed {
                        app_id,
                        message,
                        request_id: Some(request_id),
                        ..
                    },
            } if request_id == "req-timeout-emit" => Some((app_id.clone(), message.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        timeout_failures.len(),
        1,
        "expected exactly one LocalAppOperationFailed naming req-timeout-emit, got {events:?}"
    );
    assert_eq!(timeout_failures[0].0, Some("app-timeout-emit".to_string()));
    assert!(
        timeout_failures[0].1.contains("timed out"),
        "message must name the timeout, got {:?}",
        timeout_failures[0].1
    );
}

/// WP8 (corrector): the behavioural test above pins ONE of the three host
/// copies that used to send an agent to a tool name it cannot call. The
/// copy that mattered most was a different one — `local_apps_build.rs`'s
/// `validate_dependency_snapshot_files`, reachable from the MODEL-callable
/// `LocalAppBuild` (`build_app` -> `build_workspace` ->
/// `build_workspace_locked`) — and a gate that covers only the copy that
/// was fixed makes the FAMILY look handled when it is not.
///
/// So scan the production text of both files for an imperative naming an
/// operation the model has no way to invoke.
///
/// The dead set is DERIVED, not listed: it is every operation in the host
/// provider catalog that has no row in `LOCAL_APP_TOOLS`. An earlier
/// revision of this test hardcoded `ConfirmDependencyChange` /
/// `UpdateDependencies` and asserted in its own prose that they were
/// unreachable; wiring those two rows (r2-never-wired-02) turned the
/// assertion into a green test pinning a world that no longer existed,
/// and forbade the copy that had become CORRECT ("call
/// `LocalAppConfirmDependencyChange`"). Deriving the set is what keeps
/// this gate from rotting the same way again. Note the transport refusing
/// a static spelling is NOT the discriminator — the MCP surface refuses
/// `build` too (`the_mcp_surface_no_longer_serves_the_static_host_operations`)
/// and `LocalAppBuild` is very much live. Membership in `LOCAL_APP_TOOLS`
/// is the whole test.
///
/// The needles are ASSEMBLED at runtime rather than written out, because
/// this test's own source is inside one of the two files it scans — a
/// literal needle here would match itself and the gate could never go red.
/// Mentioning a name in prose (as the comments above and this one do) is
/// deliberately still allowed: the defect is the imperative, not the name.
///
/// An empty dead set makes this vacuous, and correctly so: if every
/// catalog operation has a tool row, no copy can point at one that does
/// not.
#[test]
fn no_host_error_copy_tells_the_model_to_call_an_operation_with_no_tool_row() {
    let wired: std::collections::HashSet<&str> =
        crate::mobile::local_apps_tools::LOCAL_APP_TOOLS
            .iter()
            .map(|&(_, operation, _)| operation)
            .collect();
    // Both spellings a model-facing sentence could plausibly use for an
    // operation that has no tool row: the provider-side operation name and
    // the `LocalApp*` name it WOULD have had.
    let mut dead: Vec<String> = Vec::new();
    for tool in crate::mobile::local_apps_mcp::LocalAppsMcpTransport::host_tool_catalog() {
        let operation = tool.tool_name().to_string();
        if wired.contains(operation.as_str()) {
            continue;
        }
        let camel: String = operation
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect();
        dead.push(format!("LocalApp{camel}"));
        dead.push(operation);
    }
    let verbs = ["use ", "run ", "call ", "retry ", "invoke "];
    let sources = [
        ("local_apps_host.rs", include_str!("../../local_apps_host.rs")),
        ("local_apps_host/approvals.rs", include_str!("../approvals.rs")),
        ("local_apps_host/bridge_operations.rs", include_str!("../bridge_operations.rs")),
        ("local_apps_host/data_operations.rs", include_str!("../data_operations.rs")),
        ("local_apps_host/dependency_install.rs", include_str!("../dependency_install.rs")),
        ("local_apps_host/dependency_integrity.rs", include_str!("../dependency_integrity.rs")),
        ("local_apps_host/dependency_recovery.rs", include_str!("../dependency_recovery.rs")),
        ("local_apps_host/mcp_publication.rs", include_str!("../mcp_publication.rs")),
        ("local_apps_host/runtime_lifecycle.rs", include_str!("../runtime_lifecycle.rs")),
        ("local_apps_host/static_server.rs", include_str!("../static_server.rs")),
        ("local_apps_build.rs", include_str!("../../local_apps_build.rs")),
    ];
    let mut hits: Vec<String> = Vec::new();
    for (name, src) in sources {
        for (index, line) in src.lines().enumerate() {
            let lowered = line.to_lowercase();
            for op in &dead {
                let lowered_op = op.to_lowercase();
                for verb in verbs {
                    let needle = format!("{verb}{lowered_op}");
                    if lowered.contains(&needle) {
                        hits.push(format!(
                            "{name}:{}: `{}` — {}",
                            index + 1,
                            line.trim(),
                            needle
                        ));
                    }
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "host copy must not tell a caller to invoke an operation that has no row in \
         LOCAL_APP_TOOLS and therefore no model-callable name; it must tell the agent \
         to report the drift instead. Dead operations today: {dead:?}. Offending \
         line(s):\n{}",
        hits.join("\n")
    );
}

#[tokio::test]
async fn remove_only_dependency_change_skips_native_confirmation() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "移除依赖", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependencies");
    requested.insert("dayjs".into(), "1.11.13".into());
    let dependency_record = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &dependency_record),
            LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
                .expect("requested json"),
            LocalAppsHostBroker::build_effective_package_json(contract, &requested)
                .expect("effective package"),
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");
    broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt.receipt_id,
        }))
        .await
        .expect("seed committed dependency baseline");
    let request_start = runtime.isolated_requests().await.len();

    let result = broker
        .confirm_dependency_change(json!({
            "app_id": shell.id,
            "changes": [{"kind": "remove", "package": "dayjs"}],
        }))
        .await
        .expect("remove-only confirmation should not require native approval");
    assert_eq!(result["ok"], true);
    assert!(result["receipt"]["id"].as_str().is_some(), "{result}");
    broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": result["receipt"]["id"].as_str().expect("receipt id"),
        }))
        .await
        .expect("remove-only dependency update");
    let requests = runtime.isolated_requests().await;
    let dependency_requests: Vec<_> = requests[request_start..]
        .iter()
        .filter(|request| request.command == "/usr/bin/pnpm")
        .collect();
    assert_eq!(dependency_requests.len(), 2, "{dependency_requests:?}");
    assert!(
        dependency_requests
            .iter()
            .all(|request| request.network == NetworkPolicy::Disabled),
        "remove-only updates must not perform a networked dependency resolution: {dependency_requests:?}"
    );
}

/// A Stop during the native dependency-review sheet must not orphan the
/// pending entry.
///
/// `local_apps_tools.rs` gives `confirm_dependency_change`
/// `InterruptBehavior::Cancel`, so ESC drops this future while it is
/// parked on `timeout(APPROVAL_TIMEOUT, receiver)` — a path neither
/// straight-line `remove` arm can ever run on.
#[tokio::test]
async fn dropping_the_dependency_confirmation_future_clears_the_pending_entry() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖中断", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let mut confirmation = Box::pin(broker.confirm_dependency_change(json!({
        "app_id": shell.id,
        "changes": [{"kind": "add", "package": "dayjs", "version": "1.11.13"}],
    })));
    let parked = timeout(Duration::from_millis(400), confirmation.as_mut()).await;
    assert!(
        parked.is_err(),
        "an add-kind change must park on the native review await, got {parked:?}"
    );
    assert_eq!(
        broker
            .pending_dependency_change_confirmations
            .lock()
            .await
            .len(),
        1,
        "the parked confirmation must be registered before the sheet is shown"
    );

    drop(confirmation);
    assert!(
        broker
            .pending_dependency_change_confirmations
            .lock()
            .await
            .is_empty(),
        "a Stop during the native dependency review must not orphan its pending \
         confirmation entry — the user's later tap would resolve into a receiver \
         nobody holds"
    );
}

#[tokio::test]
async fn dependency_change_confirmation_fails_closed_on_tampered_requested_baseline() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖篡改", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let workspace = workspace_of(&root, &shell.id);
    fs::write(
        workspace.join(crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL),
        "{\n  \"dependencies\": {\n    \"dayjs\": \"1.11.13\"\n  }\n}\n",
    )
    .expect("tamper requested dependency baseline");

    let error = broker
        .confirm_dependency_change(json!({
            "app_id": shell.id,
            "changes": [{"kind": "add", "package": "nanoid", "version": "5.1.6"}],
        }))
        .await
        .expect_err("tampered requested baseline must fail closed");
    assert!(error.contains("dependencies_dirty"), "{error}");
    assert!(
        error.contains("outside the host-managed dependency flow"),
        "{error}"
    );
}

#[tokio::test]
async fn dependency_add_uses_dedicated_native_confirmation_before_receipt() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        Some(runtime.clone()),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "确认依赖", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let requests_before_confirmation = runtime.isolated_requests().await.len();

    let request = tokio::spawn({
        let broker = broker.clone();
        let app_id = shell.id.clone();
        async move {
            broker
                .confirm_dependency_change(json!({
                    "app_id": app_id,
                    "changes": [{
                        "kind": "add",
                        "package": "dayjs",
                        "version": "1.11.13"
                    }]
                }))
                .await
        }
    });

    let confirmation = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request) = sink.events().await.into_iter().find_map(|event| {
                if let ClientEvent::AppEvent {
                    event: AppEventDto::AppDependencyChangeConfirmationRequested { request },
                } = event
                {
                    Some(request)
                } else {
                    None
                }
            }) {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("dedicated confirmation event");
    assert_eq!(confirmation.app_id, shell.id);
    assert_eq!(confirmation.changes.len(), 1);
    assert_eq!(confirmation.changes[0].package, "dayjs");
    assert_eq!(
        confirmation.changes[0].cache_status,
        "unknown_until_resolution"
    );
    assert_eq!(confirmation.changes[0].download_status, "may_be_required");
    assert_eq!(confirmation.license_risk, "unknown_until_resolution");
    assert_eq!(confirmation.sbom_risk, "unknown_until_resolution");
    assert!(confirmation.lifecycle_scripts_blocked);
    assert!(confirmation.native_addons_blocked);
    assert_eq!(
        confirmation.rollback_policy,
        "rollback_on_validation_failure"
    );
    assert_eq!(
        runtime.isolated_requests().await.len(),
        requests_before_confirmation,
        "confirmation must not install or resolve dependencies before approval"
    );
    assert!(
        broker
            .pending_dependency_change_receipts
            .lock()
            .await
            .get(&shell.id)
            .is_none(),
        "a receipt must not exist before approval"
    );

    assert!(
        broker
            .resolve_dependency_change_confirmation(&confirmation.request_id, true)
            .await
    );
    let result = request
        .await
        .expect("confirmation task")
        .expect("approved dependency change");
    assert!(result["receipt"]["id"].as_str().is_some(), "{result}");
}

#[tokio::test]
async fn dependency_confirmation_does_not_wait_on_unrelated_global_build_lock() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        Some(runtime),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "锁隔离", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let global_build_lock = broker.build_lock();
    let _global_build_guard = global_build_lock.lock().await;
    let request = tokio::spawn({
        let broker = broker.clone();
        let app_id = shell.id.clone();
        async move {
            broker
                .confirm_dependency_change(json!({
                    "app_id": app_id,
                    "changes": [{
                        "kind": "add",
                        "package": "dayjs",
                        "version": "1.11.13"
                    }]
                }))
                .await
        }
    });
    let confirmation = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request) = sink.events().await.into_iter().find_map(|event| {
                if let ClientEvent::AppEvent {
                    event: AppEventDto::AppDependencyChangeConfirmationRequested { request },
                } = event
                {
                    Some(request)
                } else {
                    None
                }
            }) {
                break request;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("dedicated confirmation event");
    assert!(
        broker
            .resolve_dependency_change_confirmation(&confirmation.request_id, true)
            .await
    );
    let result = timeout(Duration::from_millis(500), request)
        .await
        .expect("receipt recheck must not wait on the unrelated build lock")
        .expect("confirmation task")
        .expect("approved dependency change");
    assert!(result["receipt"]["id"].as_str().is_some(), "{result}");
}

#[tokio::test]
async fn dependency_add_denial_does_not_issue_receipt() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        sink.clone(),
        Some(runtime),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "拒绝依赖", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let request = tokio::spawn({
        let broker = broker.clone();
        let app_id = shell.id.clone();
        async move {
            broker
                .confirm_dependency_change(json!({
                    "app_id": app_id,
                    "changes": [{
                        "kind": "update",
                        "package": "dayjs",
                        "version": "1.11.14"
                    }]
                }))
                .await
        }
    });
    let request_id = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(request_id) = sink.events().await.into_iter().find_map(|event| {
                if let ClientEvent::AppEvent {
                    event: AppEventDto::AppDependencyChangeConfirmationRequested { request },
                } = event
                {
                    Some(request.request_id)
                } else {
                    None
                }
            }) {
                break request_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("dedicated confirmation event");
    assert!(
        broker
            .resolve_dependency_change_confirmation(&request_id, false)
            .await
    );
    let error = request
        .await
        .expect("confirmation task")
        .expect_err("denial must fail closed");
    assert!(error.contains("denied"), "{error}");
    assert!(
        broker
            .pending_dependency_change_receipts
            .lock()
            .await
            .get(&shell.id)
            .is_none(),
        "a denied change must not issue a receipt"
    );
}

#[tokio::test]
async fn dependency_update_resolves_then_verifies_with_frozen_network_denied_install() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖两阶段", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let previous_package = fs::read(workspace.join("package.json")).expect("baseline package");
    let previous_lock_digest = LocalAppsHostBroker::dependency_lock_digest(&layout)
        .expect("baseline dependency lock digest");
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let requested_json = LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
        .expect("requested JSON");
    let effective_package_json =
        LocalAppsHostBroker::build_effective_package_json(contract, &requested)
            .expect("effective package");
    let dependency_record = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &dependency_record),
            requested_json,
            effective_package_json.clone(),
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");
    let receipt_id = receipt.receipt_id;

    runtime
        .set_resolved_pnpm_lockfile(
            fs::read(workspace.join("pnpm-lock.yaml")).expect("baseline lockfile"),
        )
        .await;
    let error = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt_id.clone(),
        }))
        .await
        .expect_err("a resolver must not reuse the baseline lock for a changed package");
    assert!(error.contains("dayjs@1.11.13"), "{error}");
    assert_eq!(
        fs::read(workspace.join("package.json")).expect("rolled back package"),
        previous_package,
        "a lock/package mismatch must restore authoritative package state"
    );
    assert!(
        !workspace
            .join(".lingxi-build-state/dependency-staging")
            .exists(),
        "a lock/package mismatch must discard the untrusted staging tree"
    );

    runtime
        .set_resolved_pnpm_lockfile(
            mock_pnpm_lockfile(&effective_package_json).expect("matching resolved lockfile"),
        )
        .await;
    let request_start = runtime.isolated_requests().await.len();

    broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("dependency update");
    assert!(
        !LocalAppsHostBroker::dependency_update_recovery_path(&layout).exists(),
        "successful dependency update must remove its committed recovery journal"
    );

    let requests = runtime.isolated_requests().await;
    let dependency_requests: Vec<_> = requests[request_start..]
        .iter()
        .filter(|request| request.command == "/usr/bin/pnpm")
        .collect();
    assert_eq!(dependency_requests.len(), 2, "{dependency_requests:?}");
    let resolution = dependency_requests[0];
    assert_eq!(resolution.network, NetworkPolicy::Allowed);
    assert!(
        resolution
            .args
            .iter()
            .any(|arg| arg == "--no-frozen-lockfile"),
        "resolution must be allowed to produce a new lockfile: {resolution:?}"
    );
    assert!(
        !resolution.args.iter().any(|arg| arg == "--lockfile-only"),
        "networked resolution must preheat the store with a full install: {resolution:?}"
    );
    assert!(!resolution.args.iter().any(|arg| arg == "--frozen-lockfile"));
    assert!(resolution.args.iter().any(|arg| arg == "--ignore-scripts"));
    assert!(resolution.args.iter().any(|arg| arg == "--no-runtime"));

    let frozen = dependency_requests[1];
    assert_eq!(frozen.network, NetworkPolicy::Disabled);
    for flag in ["--frozen-lockfile", "--ignore-scripts", "--no-runtime"] {
        assert!(
            frozen.args.iter().any(|arg| arg == flag),
            "frozen verification must include {flag}: {frozen:?}"
        );
    }
    assert!(!frozen.args.iter().any(|arg| arg == "--no-frozen-lockfile"));
    assert!(!frozen.args.iter().any(|arg| arg == "--lockfile-only"));

    let updated_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("updated dependency record");
    assert_ne!(
        updated_dependency.lockfile_sha256.as_deref(),
        Some(previous_lock_digest.as_str()),
        "the resolver fixture must not let a changed package reuse the baseline lock"
    );
    let sbom: Value = serde_json::from_slice(
        &fs::read(workspace.join(crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL))
            .expect("updated dependency SBOM"),
    )
    .expect("updated dependency SBOM JSON");
    assert!(
        sbom["packages"]
            .as_array()
            .expect("SBOM packages")
            .iter()
            .any(|package| package["name"] == "dayjs"),
        "the cold frozen tree must actually represent the requested dependency graph"
    );

    assert!(
        !workspace
            .join(".lingxi-build-state/dependency-staging/node_modules")
            .exists(),
        "successful dependency update must clean its staging tree after publication"
    );
}

#[tokio::test]
async fn dependency_update_warm_snapshot_skips_frozen_install() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖热缓存", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let previous_lock_digest = LocalAppsHostBroker::dependency_lock_digest(&layout)
        .expect("current dependency lock digest");
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let requested_json = LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
        .expect("requested json");
    let effective_package_json =
        LocalAppsHostBroker::build_effective_package_json(contract, &requested)
            .expect("effective package");
    let resolved_lockfile =
        mock_pnpm_lockfile(&effective_package_json).expect("resolved lockfile fixture");
    validate_resolved_dependency_lock(&effective_package_json, &resolved_lockfile)
        .expect("warm lockfile fixture must represent the requested graph");
    let lock_digest = format!("{:x}", Sha256::digest(&resolved_lockfile));
    assert_ne!(
        lock_digest, previous_lock_digest,
        "the fixture must prove lookup by the newly resolved lock, not the old app lock"
    );
    runtime.set_resolved_pnpm_lockfile(resolved_lockfile).await;

    let snapshot_source = root.path().join("warm-update/node_modules");
    clone_or_copy_tree(&workspace.join("node_modules"), &snapshot_source)
        .expect("copy resolved dependency tree fixture");
    fs::create_dir_all(snapshot_source.join("dayjs")).expect("dayjs package directory");
    fs::write(
        snapshot_source.join("dayjs/package.json"),
        r#"{"name":"dayjs","version":"1.11.13","license":"MIT"}"#,
    )
    .expect("dayjs package manifest");
    let snapshot = broker.dependency_snapshot_root(&lock_digest, PNPM_TOOLCHAIN_KEY);
    LocalAppsHostBroker::publish_dependency_snapshot(
        &snapshot_source,
        &snapshot,
        &lock_digest,
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("preseed exact verified dependency snapshot");
    assert!(LocalAppsHostBroker::dependency_snapshot_is_ready(
        &snapshot,
        &lock_digest,
        PNPM_TOOLCHAIN_KEY
    )
    .expect("preseeded snapshot readiness"));

    let dependency_record = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &dependency_record),
            requested_json,
            effective_package_json,
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");

    runtime.set_fail_frozen_install(true);
    let request_start = runtime.isolated_requests().await.len();
    broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt.receipt_id,
        }))
        .await
        .expect("warm snapshot update must not need frozen pnpm");
    let requests = runtime.isolated_requests().await;
    let dependency_requests: Vec<_> = requests[request_start..]
        .iter()
        .filter(|request| request.command == "/usr/bin/pnpm")
        .collect();
    assert_eq!(dependency_requests.len(), 1, "{dependency_requests:?}");
    assert_eq!(dependency_requests[0].network, NetworkPolicy::Allowed);
    assert!(
        !dependency_requests[0]
            .args
            .iter()
            .any(|arg| arg == "--frozen-lockfile"),
        "a verified snapshot hit must skip the second frozen install: {dependency_requests:?}"
    );
    let dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("updated dependency record");
    assert_eq!(
        dependency.lockfile_sha256.as_deref(),
        Some(lock_digest.as_str()),
        "the committed dependency record must bind the resolved lock digest"
    );
    let sbom: Value = serde_json::from_slice(
        &fs::read(workspace.join(crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL))
            .expect("updated dependency SBOM"),
    )
    .expect("updated dependency SBOM JSON");
    assert!(
        sbom["packages"]
            .as_array()
            .expect("SBOM packages")
            .iter()
            .any(|package| package["name"] == "dayjs"),
        "the warm snapshot's verified inventory must feed the fresh app-specific SBOM"
    );
}

#[tokio::test]
async fn stale_dependency_receipt_cannot_overwrite_a_newer_committed_baseline() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖过期回归", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let baseline_a_record = service
        .dependency_record(&shell.id)
        .await
        .expect("baseline A dependency record");
    let baseline_a = dependency_baseline_for(&layout, &baseline_a_record);
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let requested_json = LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
        .expect("requested json");
    let effective_package_json =
        LocalAppsHostBroker::build_effective_package_json(contract, &requested)
            .expect("effective package");
    let fresh_receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            baseline_a.clone(),
            requested_json.clone(),
            effective_package_json.clone(),
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("fresh receipt");
    broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": fresh_receipt.receipt_id,
        }))
        .await
        .expect("commit newer dependency baseline");

    let stale_receipt_id = "dependency-change-stale".to_string();
    broker
        .pending_dependency_change_receipts
        .lock()
        .await
        .insert(
            shell.id.clone(),
            PendingDependencyChangeReceipt {
                receipt_id: stale_receipt_id.clone(),
                app_id: shell.id.clone(),
                baseline: baseline_a,
                requested_json,
                effective_package_json,
                issued_at_ms: now_ms(),
                expires_at_ms: now_ms() + APPROVAL_RECEIPT_TTL.as_millis() as u64,
                summary: vec![DependencyChange {
                    kind: DependencyChangeKind::Add,
                    package: "dayjs".into(),
                    version: Some("1.11.13".into()),
                }],
                claimed: false,
            },
        );

    let error = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": stale_receipt_id,
        }))
        .await
        .expect_err("stale receipt must fail closed");
    assert!(error.contains("reconfirm before applying"), "{error}");
    assert!(
        broker
            .pending_dependency_change_receipts
            .lock()
            .await
            .get(&shell.id)
            .is_none(),
        "stale receipt must be consumed after rejection"
    );
    assert_eq!(
        LocalAppsHostBroker::load_requested_dependency_map(&workspace)
            .expect("current requested dependency map")
            .get("dayjs")
            .map(String::as_str),
        Some("1.11.13"),
        "the newer committed baseline must remain authoritative"
    );
}

#[tokio::test]
async fn frozen_dependency_install_failure_rolls_back_and_releases_receipt_claim() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖冻结失败", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let previous_package = fs::read(workspace.join("package.json")).expect("package.json");
    let previous_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &previous_dependency),
            LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
                .expect("requested json"),
            LocalAppsHostBroker::build_effective_package_json(contract, &requested)
                .expect("effective package"),
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");
    let receipt_id = receipt.receipt_id;

    runtime.set_fail_frozen_install(true);
    let error = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect_err("frozen install failure must abort the update");
    assert!(
        error.contains("synthetic frozen install failure"),
        "{error}"
    );
    assert_eq!(
        fs::read(workspace.join("package.json")).expect("restored package"),
        previous_package
    );
    assert!(
        workspace.join("node_modules/vite/bin/vite.js").is_file(),
        "failed verification must leave the previous dependency tree"
    );
    let current_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("current dependency record");
    assert_eq!(current_dependency.state, AppDependencyState::Ready);
    assert_eq!(
        current_dependency.lockfile_sha256,
        previous_dependency.lockfile_sha256
    );

    runtime.set_fail_frozen_install(false);
    let retried = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("failed frozen install releases the receipt claim");
    assert_eq!(retried["ok"], true);
}

#[tokio::test]
async fn dependency_update_rejects_lifecycle_scripts_before_snapshot_and_releases_receipt() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖脚本", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let previous_package = fs::read(workspace.join("package.json")).expect("package.json");
    let previous_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &previous_dependency),
            LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
                .expect("requested json"),
            LocalAppsHostBroker::build_effective_package_json(contract, &requested)
                .expect("effective package"),
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");
    let receipt_id = receipt.receipt_id;

    runtime.set_inject_lifecycle_script(true);
    let error = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect_err("lifecycle script must abort the update before snapshot publication");
    assert!(error.contains("react"), "{error}");
    assert!(error.contains("install"), "{error}");
    assert_eq!(
        fs::read(workspace.join("package.json")).expect("restored package"),
        previous_package
    );
    assert!(workspace.join("node_modules/vite/bin/vite.js").is_file());
    let current_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("current dependency record");
    assert_eq!(current_dependency.state, AppDependencyState::Ready);
    assert_eq!(
        current_dependency.lockfile_sha256,
        previous_dependency.lockfile_sha256
    );

    runtime.set_inject_lifecycle_script(false);
    let retried = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt_id,
        }))
        .await
        .expect("lifecycle-script rejection releases the receipt claim");
    assert_eq!(retried["ok"], true);
}

#[tokio::test]
async fn dependency_update_rolls_back_authoritative_files_when_finalize_fails() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖回滚", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    runtime.set_omit_staged_vite_marker(true);

    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let workspace = root.path().join(layout.workspace_rel());
    let previous_package = fs::read(workspace.join("package.json")).expect("package.json");
    let previous_requested =
        fs::read(workspace.join(crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL))
            .expect("requested.json");
    let previous_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");

    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let requested_json = LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
        .expect("requested json");
    let effective_package_json =
        LocalAppsHostBroker::build_effective_package_json(contract, &requested)
            .expect("effective package");
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &previous_dependency),
            requested_json,
            effective_package_json,
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");

    let error = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt.receipt_id,
        }))
        .await
        .expect_err("missing staged vite marker must fail finalize");
    assert!(error.contains("staged Vite executable"), "{error}");
    assert_eq!(
        fs::read(workspace.join("package.json")).expect("restored package"),
        previous_package
    );
    assert_eq!(
        fs::read(workspace.join(crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL))
            .expect("restored requested"),
        previous_requested
    );
    assert!(
        workspace.join("node_modules/vite/bin/vite.js").is_file(),
        "previous dependency tree must be restored"
    );
    let current_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("current dependency record");
    assert_eq!(current_dependency.state, AppDependencyState::Ready);
    assert_eq!(
        current_dependency.lockfile_sha256,
        previous_dependency.lockfile_sha256
    );
    assert_eq!(
        current_dependency.toolchain_key,
        previous_dependency.toolchain_key
    );
}

#[tokio::test]
async fn dependency_update_builds_before_consuming_and_restores_the_old_build_on_failure() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖构建", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let builder = crate::mobile::local_apps_build::LocalAppBuilder {
        mobile_linux: broker.mobile_linux(),
        host: &broker,
    };
    builder
        .build_workspace(&layout)
        .await
        .expect("initial production build");
    let built_index = layout
        .root()
        .join(layout.build_rel(false))
        .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR)
        .join("index.html");
    let old_index = fs::read(&built_index).expect("old build output");

    let workspace = root.path().join(layout.workspace_rel());
    let binding = load_manifest(&layout)
        .expect("manifest")
        .runtime_profile
        .expect("runtime profile");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("runtime contract");
    let mut requested = LocalAppsHostBroker::load_requested_dependency_map(&workspace)
        .expect("requested dependency map");
    requested.insert("dayjs".into(), "1.11.13".into());
    let previous_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("dependency record");
    let receipt = broker
        .issue_dependency_change_receipt(
            &shell.id,
            dependency_baseline_for(&layout, &previous_dependency),
            LocalAppsHostBroker::serialize_requested_dependency_map(&requested)
                .expect("requested json"),
            LocalAppsHostBroker::build_effective_package_json(contract, &requested)
                .expect("effective package"),
            vec![DependencyChange {
                kind: DependencyChangeKind::Add,
                package: "dayjs".into(),
                version: Some("1.11.13".into()),
            }],
        )
        .await
        .expect("issue dependency receipt");

    runtime.set_fail_build(true);
    let error = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt.receipt_id,
        }))
        .await
        .expect_err("production build failure must roll back the dependency update");
    assert!(error.contains("production build failed"), "{error}");
    assert_eq!(
        fs::read(&built_index).expect("restored old build"),
        old_index
    );
    crate::mobile::local_apps_build::validate_build_for_launch(&layout)
        .expect("restored build receipt remains launchable");

    runtime.set_fail_build(false);
    let retried = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt.receipt_id,
        }))
        .await
        .expect("failed build releases the receipt claim for retry");
    assert_eq!(retried["ok"], true);
    crate::mobile::local_apps_build::validate_build_for_launch(&layout)
        .expect("successful dependency update writes a launchable build receipt");
    let replay = broker
        .update_dependencies(json!({
            "app_id": shell.id,
            "receipt_id": receipt.receipt_id,
        }))
        .await
        .expect_err("successful build consumes the receipt");
    assert!(
        replay.contains("missing or was already consumed"),
        "{replay}"
    );
}

#[tokio::test]
async fn dependency_update_cold_start_recovers_an_in_progress_journal() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖冷启动", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let builder = crate::mobile::local_apps_build::LocalAppBuilder {
        mobile_linux: broker.mobile_linux(),
        host: &broker,
    };
    builder
        .build_workspace(&layout)
        .await
        .expect("initial production build");
    let workspace = root.path().join(layout.workspace_rel());
    let old_manifest = load_manifest(&layout).expect("old manifest");
    let old_package = fs::read(workspace.join("package.json")).expect("old package");
    let old_tree_digest =
        dependency_tree_digest(&workspace.join("node_modules")).expect("old dependency tree");
    let build_index = root
        .path()
        .join(layout.build_rel(false))
        .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR)
        .join("index.html");
    let old_build_index = fs::read(&build_index).expect("old build output");
    let old_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("old dependency record");
    let rollback = broker
        .capture_dependency_update_rollback(&layout, old_dependency.clone())
        .expect("capture durable rollback");
    let journal = LocalAppsHostBroker::dependency_update_recovery_journal(
        &layout,
        &rollback,
        DependencyUpdateRecoveryStatus::InProgress,
    )
    .expect("build recovery journal");
    LocalAppsHostBroker::write_dependency_update_recovery_journal(&layout, &journal)
        .expect("write recovery journal");

    // Simulate process death after new workspace files, dependency tree,
    // manifest, build and dependency state were partially published.
    fs::write(workspace.join("package.json"), b"{\"name\":\"new\"}\n")
        .expect("write partial package");
    let mut new_manifest = old_manifest.clone();
    new_manifest.name = "partial-new".into();
    new_manifest.revision += 1;
    local_apps::save_manifest(&layout, &new_manifest).expect("write partial manifest");
    LocalAppsHostBroker::remove_owned_path(&workspace.join("node_modules"))
        .expect("remove old tree");
    fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("new tree");
    fs::write(
        workspace.join("node_modules/vite/bin/vite.js"),
        b"partial-new",
    )
    .expect("new tree marker");
    fs::write(&build_index, b"partial-new-build").expect("partial build");
    fs::create_dir_all(
        workspace
            .join(".lingxi-build-state")
            .join("dependency-staging"),
    )
    .expect("partial staging");
    service
        .start_dependency_install(&shell.id)
        .await
        .expect("mark dependency update in progress");
    drop(rollback);
    drop(broker);
    drop(service);

    // The broker constructor runs recovery before AppService::load, so the
    // service observes the same exact old dependency record as disk.
    let restarted = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        Some(runtime),
        false,
        None,
    );
    let restarted_service = test_service(&root).await;
    assert!(restarted.attach_service(restarted_service.clone()).is_ok());
    assert_eq!(
        load_manifest(&layout).expect("restored manifest"),
        old_manifest
    );
    assert_eq!(
        fs::read(workspace.join("package.json")).expect("restored package"),
        old_package
    );
    assert_eq!(
        dependency_tree_digest(&workspace.join("node_modules")).expect("restored tree"),
        old_tree_digest
    );
    assert_eq!(
        fs::read(build_index).expect("restored build"),
        old_build_index
    );
    assert_eq!(
        restarted_service
            .dependency_record(&shell.id)
            .await
            .expect("restored dependency record"),
        old_dependency
    );
    assert!(
        !LocalAppsHostBroker::dependency_update_recovery_path(&layout).exists(),
        "boot recovery must consume the dependency journal"
    );
    assert!(
        !workspace
            .join(".lingxi-build-state/dependency-staging")
            .exists(),
        "boot recovery must remove interrupted staging"
    );
}

#[tokio::test]
async fn dependency_update_cold_start_cleans_a_committed_journal_without_rollback() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime.clone())).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "依赖提交恢复", "b", "dom").await,
        )
        .await
        .expect("scaffold");
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let builder = crate::mobile::local_apps_build::LocalAppBuilder {
        mobile_linux: broker.mobile_linux(),
        host: &broker,
    };
    builder
        .build_workspace(&layout)
        .await
        .expect("initial production build");
    let workspace = root.path().join(layout.workspace_rel());
    let mut new_manifest = load_manifest(&layout).expect("manifest");
    new_manifest.name = "committed-new".into();
    new_manifest.revision += 1;
    let old_dependency = service
        .dependency_record(&shell.id)
        .await
        .expect("old dependency record");
    let rollback = broker
        .capture_dependency_update_rollback(&layout, old_dependency)
        .expect("capture durable rollback");
    let node_modules_backup = rollback
        .node_modules_backup
        .clone()
        .expect("fixture has dependency tree");
    let build_backup = rollback
        .build_backup
        .clone()
        .expect("fixture has production build");
    let journal = LocalAppsHostBroker::dependency_update_recovery_journal(
        &layout,
        &rollback,
        DependencyUpdateRecoveryStatus::Committed,
    )
    .expect("build committed recovery journal");
    LocalAppsHostBroker::write_dependency_update_recovery_journal(&layout, &journal)
        .expect("write committed recovery journal");
    local_apps::save_manifest(&layout, &new_manifest).expect("write committed manifest");
    fs::write(
        workspace.join("package.json"),
        b"{\"name\":\"committed-new\"}\n",
    )
    .expect("write committed package");
    LocalAppsHostBroker::remove_owned_path(&workspace.join("node_modules"))
        .expect("remove old tree");
    fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("new tree");
    fs::write(
        workspace.join("node_modules/vite/bin/vite.js"),
        b"committed-new",
    )
    .expect("new tree marker");
    let build_index = root
        .path()
        .join(layout.build_rel(false))
        .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR)
        .join("index.html");
    fs::write(&build_index, b"committed-new-build").expect("committed build");
    fs::create_dir_all(
        workspace
            .join(".lingxi-build-state")
            .join("dependency-staging"),
    )
    .expect("committed staging");
    service
        .start_dependency_install(&shell.id)
        .await
        .expect("mark committed dependency state");
    drop(rollback);
    drop(broker);
    drop(service);

    let restarted = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        Some(runtime),
        false,
        None,
    );
    let restarted_service = test_service(&root).await;
    assert!(restarted.attach_service(restarted_service.clone()).is_ok());
    assert_eq!(
        load_manifest(&layout).expect("committed manifest"),
        new_manifest
    );
    assert_eq!(
        fs::read(workspace.join("package.json")).expect("committed package"),
        b"{\"name\":\"committed-new\"}\n"
    );
    assert_eq!(
        fs::read(workspace.join("node_modules/vite/bin/vite.js")).expect("committed tree"),
        b"committed-new"
    );
    assert_eq!(
        fs::read(build_index).expect("committed build"),
        b"committed-new-build"
    );
    assert_eq!(
        restarted_service
            .dependency_record(&shell.id)
            .await
            .expect("committed dependency record")
            .state,
        AppDependencyState::Installing
    );
    assert!(!node_modules_backup.exists());
    assert!(!build_backup.exists());
    assert!(!LocalAppsHostBroker::dependency_update_recovery_path(&layout).exists());
    assert!(!workspace
        .join(".lingxi-build-state/dependency-staging")
        .exists());
}

/// The whole point of the flow: what the user confirmed in the interview
/// reaches BOTH the record and `LINGXI.md`.
///
/// The contract assertions are not decoration. `workspace/LINGXI.md` is
/// written exactly once and is the only channel that reaches the model on
/// every turn; rendering it from the CREATION record instead of the
/// proposed one writes `# Local App: untitled` with an empty brief and
/// loses the entire interview, permanently, while every record assertion
/// above still passes.
#[tokio::test]
async fn scaffold_commits_all_four_fields_and_writes_the_formal_contract() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let guided = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("read the guided contract");
    assert!(
        guided.contains("has no shape yet"),
        "the fixture must start on the guided contract: {guided}"
    );

    let mut input =
        confirmed_scaffold_input(&broker, &shell.id, "打飞机", "一个竖版射击小游戏", "canvas")
            .await;
    input["workflow_model"] = json!("anthropic/claude-opus-4");
    let value = broker
        .scaffold_shell_app_value(input)
        .await
        .expect("scaffold");

    let record = service.record(&shell.id).await.expect("record");
    assert!(record.scaffolded, "the commit point must have run");
    assert_eq!(record.name, "打飞机");
    assert_eq!(record.brief, "一个竖版射击小游戏");
    assert_eq!(
        record.workflow_model.as_deref(),
        Some("anthropic/claude-opus-4"),
        "the confirmed workflow model must be persisted by the same commit"
    );
    assert_eq!(
        value.get("app").and_then(|app| app.get("scaffolded")),
        Some(&json!(true)),
        "the tool result must echo the COMMITTED record: {value}"
    );
    let next_step = value
        .get("next_step")
        .and_then(Value::as_str)
        .expect("the result must carry a next step");
    assert!(
        next_step.contains("LINGXI.md"),
        "the agent must be sent back to the contract that just replaced              the guided one: {next_step}"
    );

    let contract = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("read the formal contract");
    assert!(
        contract.contains("# Local App: 打飞机"),
        "must render the CONFIRMED name, not `untitled`: {contract}"
    );
    assert!(
        contract.contains("Brief: 一个竖版射击小游戏"),
        "must render the CONFIRMED brief: {contract}"
    );
    assert!(
        !contract.contains("has no shape yet"),
        "the guided contract must be overwritten, not appended to"
    );
    assert!(
        contract.contains("This app's surface is `canvas`"),
        "the contract must be the one for the CONFIRMED surface: {contract}"
    );
    for workflow in [
        crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID,
        crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID,
        crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID,
    ] {
        assert!(
            !contract.contains(workflow),
            "the contract must not name a build workflow: the host authorizes one and \
             refuses any other, the model does not choose it — found `{workflow}` in \
             {contract}"
        );
    }
    assert!(
        contract.contains("runtime profile `canvas_2d` revision `4`"),
        "the formal contract must mirror the persisted profile identity: {contract}"
    );
    assert!(
        contract.contains("informational mirror")
            && contract
                .contains("persisted manifest binding and host catalog are authoritative"),
        "LINGXI.md must not become the runtime profile authority: {contract}"
    );
    assert!(
        contract.contains("lib/frame-loop.js") && !contract.contains("src/game/frame-loop.js"),
        "the managed Canvas frame helper must not be presented as editable: {contract}"
    );

    // The surface is on the manifest, and the seed is the canvas one.
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    let manifest = load_manifest(&layout).expect("manifest");
    assert_eq!(manifest.surface, Some(local_apps::AppSurface::Canvas));
    assert_eq!(manifest.name, "打飞机");
    assert!(
        manifest.dependency_snapshot.is_some(),
        "the commit point must not expose a scaffolded app without a verified dependency snapshot"
    );
    assert!(workspace_of(&root, &shell.id)
        .join("app/screens/game-screen.jsx")
        .is_file());
    assert_eq!(
        service
            .dependency_record(&shell.id)
            .await
            .expect("dependency record")
            .state,
        local_apps::AppDependencyState::Ready
    );
}

/// Phase -1 (P-1.4), §19.3: the Host contract carries no workflow/skill/
/// agent names. `formal_workspace_contract` used to tell the model which
/// build workflow to launch and which one NOT to launch; that authority
/// is now the Host's alone — `LocalAppPluginBinding::resolve` computes the
/// one workflow authorized for a build target and `enforce` refuses a
/// caller-supplied mismatch by naming both ids in the error, so the model
/// never needs (and must never be told) a workflow name to act correctly.
/// This pins the absence for BOTH surfaces, not just the one the test
/// above happens to exercise, so a name reintroduced on only one branch
/// of `formal_workspace_contract`'s `match` still goes red — and over the
/// next-step guidance family as well, which is model-visible tool-result
/// prose that no test other than the component scanner covered.
#[tokio::test]
async fn lingxi_md_contract_prose_names_no_workflow() {
    // The needle set is derived from the PRODUCTION registry, never a
    // pair of names typed in here. Phase 9's live source is the plugin
    // workflow inventory rather than the removed built-in registry.
    let workflows = vec![
        crate::mobile::local_app_plugin_binding::PLUGIN_BUILD_WORKFLOW_ID,
        crate::mobile::local_app_plugin_binding::PLUGIN_USE_TEST_WORKFLOW_ID,
        crate::mobile::local_app_plugin_binding::PLUGIN_MCP_AUTHORING_WORKFLOW_ID,
    ];
    // An empty needle set would make every assertion below vacuously
    // true, which is the failure mode this whole test exists to prevent.
    assert!(
        !workflows.is_empty(),
        "the build-workflow registry must be non-trivially populated, or the absence \
         assertions below prove nothing: {workflows:?}"
    );

    for surface in ["dom", "canvas"] {
        let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
        let (root, service, broker) = create_broker(false, Some(runtime)).await;
        let shell = shell_app_fixture(&broker, &service).await;

        // r3-tests-honesty-07: the GUIDED contract is the text the create
        // interview actually reads — it is written to `LINGXI.md` the
        // moment the shell exists, before any surface or workflow is
        // chosen, and stays in place until the formal contract below
        // overwrites it. The absence gate above only ever scanned the
        // POST-scaffold file; a workflow id planted in
        // `guided_workspace_contract` would leave that gate green for
        // the entire lifetime of every unscaffolded shell.
        //
        // Both scans ACCUMULATE into `violations` and assert once at the
        // end of the surface, rather than asserting inline: this arm runs
        // BEFORE the post-scaffold arm below, so an inline `assert!` here
        // would take the formal-contract half of the same test down with
        // it — a new gate hollowing out the older one behind it.
        let mut violations: Vec<String> = Vec::new();
        let guided_contract =
            fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
                .expect("read the guided contract");
        for workflow in &workflows {
            if guided_contract.contains(workflow) {
                violations.push(format!(
                    "surface {surface}: the GUIDED (pre-scaffold) contract must name no \
                     build workflow — found `{workflow}` in {guided_contract}"
                ));
            }
        }

        let input =
            confirmed_scaffold_input(&broker, &shell.id, "测试", "一个测试应用", surface).await;
        broker
            .scaffold_shell_app_value(input)
            .await
            .expect("scaffold");
        let contract = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
            .expect("read the formal contract");
        for workflow in &workflows {
            if contract.contains(workflow) {
                violations.push(format!(
                    "surface {surface}: the FORMAL contract must name no build workflow — \
                     the host authorizes one and refuses any other, the model does not \
                     choose it — found `{workflow}` in {contract}"
                ));
            }
        }
        assert!(
            violations.is_empty(),
            "{}",
            violations.join("\n--- next violation ---\n")
        );
    }

    // The contract file is not the only Host-authored prose the model
    // reads. The next-step guidance family is returned INSIDE the
    // `LocalAppCreate` / `LocalAppScaffold` tool results, so a workflow
    // name there reaches the model on exactly the turn it is deciding
    // what to do next — and it is otherwise guarded only by the component
    // scanner, whose documented ritual (change the constant, change the
    // allowlist in the same diff) is a sanctioned route back in.
    // Demonstrated by mutation: a build-workflow name planted in
    // `scaffold_next_step_guidance` left this test GREEN before this arm
    // existed, while only the scanner fired.
    for (generator, prose) in [
        ("scaffold_next_step_guidance", scaffold_next_step_guidance()),
        ("create_next_step_guidance", create_next_step_guidance()),
    ] {
        for workflow in &workflows {
            assert!(
                !prose.contains(workflow),
                "{generator} must name no build workflow — it is model-visible tool-result \
                 prose, and a name here reintroduces the model↔workflow-name coupling the \
                 Host-side resolve/enforce exists to remove — found `{workflow}` in {prose}"
            );
        }
    }
}

/// The formal contract's "Host-managed files are …" sentence must name
/// every `lib/` file the profile's OWN `.lingxi/source-policy.json`
/// reserves. That policy is the same set
/// `permission::workspace_lease::host_owned_relative` and
/// `local_apps_build::restore_host_managed_files` enforce, so a file the
/// prose leaves out is a file the agent is never told it may not edit:
/// the lease accepts the `Edit`, the next build silently reverts it, and
/// the only trace is a `tracing::warn!` while the model loops against a
/// file it cannot change.
///
/// The needle set is DERIVED from the shipped profile contracts, never
/// typed here, and every profile must contribute at least one needle —
/// otherwise this assertion would be vacuous. Demonstrated red by
/// dropping `lib/frame-loop.js` from the canvas-family sentence: the
/// Phaser and Babylon profiles ship it beside their engine adapter and
/// reserve BOTH in their policy, while the prose named only the adapter.
#[tokio::test]
async fn the_formal_contract_names_every_host_managed_helper_its_profile_reserves() {
    let (_root, service, broker) = create_broker(false, None).await;
    let record = shell_app_fixture(&broker, &service).await;
    let mut checked = Vec::new();
    for family in [
        local_apps::AppRuntimeProfile::ReactDom,
        local_apps::AppRuntimeProfile::Canvas2d,
        local_apps::AppRuntimeProfile::Three3d,
        local_apps::AppRuntimeProfile::Phaser2d,
        local_apps::AppRuntimeProfile::Babylon3d,
    ] {
        // `babylon_3d` is deliberately gated out of this host build
        // (`UNAVAILABLE_PROFILES`), so it has no current binding to
        // render. Skipping it is why `checked` is asserted below.
        let Ok(binding) =
            crate::mobile::local_app_runtime_profiles::current_binding_for_family(family)
        else {
            continue;
        };
        let profile_contract =
            crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
                .expect("the current binding resolves to its contract");
        let policy_bytes = profile_contract
            .managed_files
            .iter()
            .find(|(path, _)| *path == ".lingxi/source-policy.json")
            .map(|(_, bytes)| *bytes)
            .expect("every profile ships a source policy");
        let policy: Value =
            serde_json::from_slice(policy_bytes).expect("source policy json parses");
        let reserved_helpers = policy
            .get("host_managed_paths")
            .and_then(Value::as_array)
            .expect("source policy host_managed_paths")
            .iter()
            .filter_map(Value::as_str)
            .filter(|path| path.starts_with("lib/"))
            .collect::<Vec<_>>();
        assert!(
            !reserved_helpers.is_empty(),
            "{family}: an empty needle set would make this test vacuous"
        );
        let contract = formal_workspace_contract(&record, &binding);
        for helper in reserved_helpers {
            assert!(
                contract.contains(&format!("`{helper}`")),
                "{family}: the formal workspace contract must name host-managed \
                 `{helper}`, or the agent is never told it may not edit it: {contract}"
            );
        }
        checked.push(family);
    }
    assert!(
        checked.contains(&local_apps::AppRuntimeProfile::Phaser2d) && checked.len() >= 4,
        "this gate is only meaningful while the scaffoldable profiles — Phaser above all, \
         which ships `lib/frame-loop.js` BESIDE its engine adapter — are actually \
         rendered here, got {checked:?}"
    );
}

/// WP8: every backticked `LocalApp*` token in a workspace contract must
/// name a REAL builtin tool, or the model is told to call something that
/// fails with an unrelated "tool not found" error it has no way to
/// diagnose. This is a forward-looking regression gate.
///
/// It does not, by itself, catch the actual pre-fix defect: the guided
/// contract's steps 4-5 sent the model to "用运行时确认工具" (a "runtime
/// confirmation tool") that was never spelled as a tool name at all —
/// plain Chinese prose, no backticks, protocol 10.0.0 having removed the
/// path it once named — and then to call `LocalAppScaffold` (itself a
/// real, existing tool) directly with a runtime-profile receipt the real
/// path never produces. A scanner that only understands backticked
/// `LocalApp*` spans walks right past prose like that, so this test pins
/// the absence of that specific phrase directly alongside the general
/// scan, rather than pretending the scan alone would have caught it.
#[tokio::test]
async fn workspace_contracts_name_no_local_app_tool_outside_local_app_tools() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let guided = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("read the guided contract");
    // Vacuity guard FIRST. The negative assertion below is only meaningful if
    // this text is the contract at all: when the contract was translated to
    // English its needle was still the Chinese "运行时确认工具", which the
    // English text can never contain, so the gate passed for a reason that had
    // nothing to do with what it guards. A negative assertion whose needle is
    // in the wrong language is indistinguishable from a green one.
    assert!(
        guided.contains("Local App (new, not yet shaped)"),
        "read the wrong file, or the contract's own header moved — every \
         assertion below is vacuous until this one holds: {guided}"
    );
    assert!(
        !guided.to_lowercase().contains("runtime confirmation tool"),
        "the guided contract must not send the model to a \"runtime confirmation \
         tool\" that does not exist — protocol 10.0.0 removed that path; the real \
         path is the create-local-app skill's unified create flow, which stages a \
         candidate, raises one native confirmation, and only then calls \
         LocalAppScaffold: {guided}"
    );
    // r1-tests-honesty-17: the negative assertion above and the tool-name
    // scanner below both stay green if the handoff step is deleted
    // outright — neither one requires the guided contract to actually
    // SAY how to get out of the shell. Pin that positively: the contract
    // must instruct the `Skill` tool with the plugin-qualified name.
    assert!(
        guided.contains("the `Skill` tool"),
        "the guided contract must positively instruct the model to use the \
         `Skill` tool to escape the shell, not just avoid naming the retired \
         runtime confirmation tool: {guided}"
    );
    assert!(
        guided.contains("lingxi-local-app:create-local-app"),
        "the guided contract must name the create skill by its exact, \
         plugin-qualified id — the bare name does not resolve: {guided}"
    );
    assert_only_real_tool_names(&guided, "the guided contract");

    for surface in ["dom", "canvas"] {
        let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
        let (root, service, broker) = create_broker(false, Some(runtime)).await;
        let shell = shell_app_fixture(&broker, &service).await;
        let input = confirmed_scaffold_input(
            &broker,
            &shell.id,
            "契约扫描",
            "扫描正文的工具名",
            surface,
        )
        .await;
        broker
            .scaffold_shell_app_value(input)
            .await
            .expect("scaffold");
        let formal = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
            .expect("read the formal contract");
        assert_only_real_tool_names(&formal, &format!("the formal {surface} contract"));
    }
}

/// r1-prompt-layer-20: the scanner used to run on the two workspace
/// contracts and nothing else, so the OTHER model-facing text the Host
/// ships could name a tool that does not exist and stay green forever.
/// The `create-local-app` skill is `include_str!`-able from here and
/// spells local-app tool names, so put it through the same gate. (The
/// build workflow script it used to scan alongside is retired with that
/// workflow.)
///
/// The skill file is the plugin mirror — the copy a plugin-loaded skill
/// actually reads. `verify-local-app-supply-chain.py`'s
/// `validate_create_skill` pins it byte-identical to `skills/`, so
/// scanning one scans both.
#[test]
fn shipped_prompt_files_name_no_tool_outside_local_app_tools() {
    let skill =
        include_str!("../../../../../plugins/lingxi-local-app/skills/create-local-app/SKILL.md");
    // Vacuity guard: a gate that scans the wrong file, or a file that
    // stopped naming tools at all, must not read as "all clear".
    assert!(
        skill.contains("name: create-local-app"),
        "read the wrong file for the create skill"
    );
    assert_only_real_tool_names(skill, "create-local-app/SKILL.md");
    assert_only_real_local_app_tool_tokens(skill, "create-local-app/SKILL.md");
}

/// r1-prompt-layer-20, re-opened: the other Host-authored, model-facing
/// prose is the next-step guidance family, and it reaches the model INSIDE
/// a tool result — [`create_next_step_guidance`] is returned in the
/// `LocalAppCreate` result, [`scaffold_next_step_guidance`] as the
/// `"next_step"` field of the `LocalAppScaffold` result. Both spell tool
/// names as BARE tokens (`LocalAppBuild`, `LocalAppScaffold`) and the create
/// one also names the backticked plugin-qualified skill id that is the only
/// spelling which resolves — and neither string was put through either
/// scanner. The one pre-existing test that walks both
/// (`the_workspace_contract_and_next_step_guidance_name_no_build_workflow`)
/// asserts only that they name no BUILD WORKFLOW, so a typo'd, renamed or
/// retired TOOL name sails straight past it and reaches the model on the
/// exact turn it is deciding what to call next.
#[test]
fn next_step_guidance_names_no_tool_outside_local_app_tools() {
    let create = create_next_step_guidance();
    let scaffold = scaffold_next_step_guidance();
    // Vacuity guards: these two are the strings the create/scaffold
    // results actually carry, so if a rewrite drops the sentence that
    // makes each one identifiable this test must fail loudly rather than
    // scan some other prose and report "all clear".
    assert!(
        create.contains("init_session_id"),
        "create_next_step_guidance no longer points at the app's own session; \
         confirm this is still the create-result prose before relaxing this guard: {create}"
    );
    assert!(
        scaffold.contains("LINGXI.md"),
        "scaffold_next_step_guidance no longer tells the agent to re-read the contract; \
         confirm this is still the scaffold-result prose before relaxing this guard: {scaffold}"
    );
    // The token scanner is the one that matters here: both strings name
    // their tools mid-sentence with no backticks, exactly the shape the
    // backtick scanner cannot see.
    assert_only_real_local_app_tool_tokens(&create, "create_next_step_guidance");
    assert_only_real_local_app_tool_tokens(&scaffold, "scaffold_next_step_guidance");
    // And the backtick scanner on top for the create string, because it is
    // the one that carries `lingxi-local-app:create-local-app` — a
    // plugin-qualified id the guidance itself says is the only spelling
    // that resolves, so a typo in it is silently unrecoverable for the
    // model.
    assert_only_real_tool_names(&create, "create_next_step_guidance");
    // `scaffold_next_step_guidance` carries no backticked span at all, so
    // running the backtick scanner over it would trip that scanner's own
    // `spans > 0` vacuity guard. Pin the reason instead of skipping in
    // silence: the day it grows one, this fails and says where to wire it.
    assert!(
        !scaffold.contains('`'),
        "scaffold_next_step_guidance has grown a backticked span, which is now unscanned — \
         add it to the assert_only_real_tool_names calls above: {scaffold}"
    );
}

/// The gate for the gate. Every assertion in
/// `assert_only_real_tool_names` is a NEGATIVE one — it only fires on text
/// nobody ships — so nothing in the suite proves it can fire at all, and a
/// tokeniser regression would look exactly like a clean scan. Feed it one
/// known-bad sample per rule and require the panic to NAME the offending
/// span, plus a positive control so a scanner that panicked on everything
/// could not pass this test either.
#[test]
fn the_tool_name_scanner_actually_rejects_the_shapes_it_exists_to_catch() {
    fn rejection_message(sample: &'static str) -> String {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome =
            std::panic::catch_unwind(|| assert_only_real_tool_names(sample, "the sample"));
        std::panic::set_hook(previous);
        let payload = outcome.err().unwrap_or_else(|| {
            panic!("the scanner accepted a sample it must reject: {sample}")
        });
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_else(|| panic!("the scanner's panic carried no message: {sample}"))
    }

    // Positive control FIRST: if this panics, every rejection below is
    // meaningless because the scanner rejects everything.
    assert_only_real_tool_names(
        "call `LocalAppBuild {\"app_id\":\"x\"}`, ask with `AskUserQuestion`, then use \
         the `Skill` tool for `lingxi-local-app:create-local-app`.",
        "the positive control",
    );

    for (sample, needle) in [
        // Rule 1: a `LocalApp*` prefix that is not a real tool.
        (
            "call `LocalAppConfirmRuntime {\"app_id\":\"x\"}` now",
            "LocalAppConfirmRuntime",
        ),
        // Rule 2: a bare CamelCase name the old alphanumeric tokeniser
        // walked straight past because it does not start with LocalApp.
        ("use the `Skil` tool to continue", "`Skil`"),
        // Rule 3: a plugin-qualified id the old tokeniser truncated at the
        // first `-`, so a typo in it could never be seen.
        (
            "start `lingxi-local-app:create-local-application` to continue",
            "lingxi-local-app:create-local-application",
        ),
        // The balance guard.
        (
            "a `stray backtick `LocalAppScaffold` here",
            "ODD number of backticks",
        ),
        // The vacuity guard.
        ("no backticks at all in this text", "no backtick-delimited"),
    ] {
        let message = rejection_message(sample);
        assert!(
            message.contains(needle),
            "the scanner rejected {sample:?} but its message never named {needle:?}: {message}"
        );
    }

    // Same treatment for the token scanner that covers the shipped prompt
    // files, including its vacuity guard.
    fn token_rejection_message(sample: &'static str) -> String {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(|| {
            assert_only_real_local_app_tool_tokens(sample, "the sample")
        });
        std::panic::set_hook(previous);
        let payload = outcome.err().unwrap_or_else(|| {
            panic!("the token scanner accepted a sample it must reject: {sample}")
        });
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_else(|| panic!("the token scanner's panic carried no message"))
    }
    assert_only_real_local_app_tool_tokens(
        "then call LocalAppScaffold and LocalAppBuild.",
        "the token positive control",
    );
    for (sample, needle) in [
        (
            "then call LocalAppConfirmRuntime.",
            "LocalAppConfirmRuntime",
        ),
        ("this text names no tool at all", "never ran"),
    ] {
        let message = token_rejection_message(sample);
        assert!(
            message.contains(needle),
            "the token scanner rejected {sample:?} but its message never named \
             {needle:?}: {message}"
        );
    }
}

/// The branch's central guarantee, pinned at its PRODUCTION call site:
/// not one byte written before the user confirmed reaches the real app.
///
/// `land_scaffold` passes `first_scaffold = true` to
/// `scaffold_workspace_initialized`. Everything else that covers the wipe
/// calls that function DIRECTLY with `true`, which proves the mechanism
/// works and proves nothing about the caller: flipping the production
/// argument to `false` left the whole suite green while pre-confirmation
/// source survived into the formed app. This test goes through
/// `scaffold_shell_app_value`, so the argument itself is what it pins —
/// verified by mutation (flip it to `false` and this test names
/// `app/app.js`).
#[tokio::test]
async fn the_production_landing_wipes_what_the_interview_wrote() {
    // What an agent that ignored the guided contract leaves behind while
    // the interview is still running. `app/app.js` is the one that MATTERS
    // and the reason a per-path overwrite is not enough: Vite resolves
    // `.js` ahead of `.jsx`, so it out-resolves the seeded `app/app.jsx`
    // and the seed ships as dead code.
    const PRE_CONFIRMATION: &[&str] = &[
        "app/app.js",
        "app/screens/guessed-screen.jsx",
        "src/stores/premature-store.js",
        "notes.md",
    ];
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let workspace = workspace_of(&root, &shell.id);

    for relative in PRE_CONFIRMATION {
        let path = workspace.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create parent");
        fs::write(&path, b"written before the user confirmed anything").expect("write");
    }
    // Host-owned state on the SAME tree, so a wipe that took too much
    // would be caught here rather than by a build minutes later.
    let installed = workspace.join("node_modules/.installed-marker");
    fs::create_dir_all(installed.parent().expect("a parent")).expect("create node_modules");
    fs::write(&installed, b"installed").expect("write");

    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "打飞机", "一个竖版射击小游戏", "dom")
                .await,
        )
        .await
        .expect("scaffold");

    for relative in PRE_CONFIRMATION {
        assert!(
            !workspace.join(relative).exists(),
            "{relative} was written before the user confirmed anything and must not \
             survive the landing"
        );
    }
    assert!(
        workspace.join("app/app.jsx").is_file(),
        "the seed must be what is on disk after the wipe"
    );
    assert!(
        workspace.join("node_modules/vite/bin/vite.js").is_file(),
        "the wipe may rebuild node_modules, but the committed app must finish with \
         host-managed dependencies installed"
    );
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    assert_eq!(
        load_manifest(&layout)
            .expect("the manifest must survive the wipe")
            .name,
        "打飞机",
        "`.lingxi/` holds the manifest the landing had already stamped"
    );
}

/// `workflow_model` is OPTIONAL, and omitting it must PRESERVE whatever
/// the create carried rather than clearing it — a shell create can already
/// name a model, and a scaffold that simply did not mention one must not
/// drop the user's choice.
#[tokio::test]
async fn omitting_the_workflow_model_preserves_the_one_the_create_chose() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let record = service
        .create_app_with_git_and_workflow_model_and_initializer(
            None,
            "",
            None,
            None,
            false,
            Some("openai/gpt-5"),
            local_apps::CreateMode::Shell,
            None,
            |_| async { Ok(()) },
        )
        .await
        .expect("create shell app with a model");
    assert_eq!(record.workflow_model.as_deref(), Some("openai/gpt-5"));

    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &record.id, "A", "b", "dom").await,
        )
        .await
        .expect("scaffold");

    let after = service.record(&record.id).await.expect("record");
    assert_eq!(
        after.workflow_model.as_deref(),
        Some("openai/gpt-5"),
        "an omitted workflow_model must not clear the create-time choice"
    );
}

/// §C.1 step 4. A landing failure must leave the record EXACTLY as the
/// create wrote it. The half-commit this forbids — a real name with
/// `scaffolded == false` — is the worst of both states: the user sees a
/// finished-looking app in the library that still opens the interview.
#[tokio::test]
async fn a_failed_landing_persists_none_of_the_four_fields() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    break_the_final_landing_step(&root, &shell.id);

    let mut input =
        confirmed_scaffold_input(&broker, &shell.id, "打飞机", "一个竖版射击小游戏", "canvas")
            .await;
    input["workflow_model"] = json!("anthropic/claude-opus-4");
    let error = broker
        .scaffold_shell_app_value(input)
        .await
        .expect_err("the landing must fail");
    assert!(
        error.contains("LINGXI.md"),
        "the failure must name the step that broke, not something else: {error}"
    );

    let after = service.record(&shell.id).await.expect("record");
    assert!(!after.scaffolded, "the commit point never ran");
    assert_eq!(
        after.name,
        local_apps::service::PLACEHOLDER_APP_NAME,
        "the name must NOT be half-committed"
    );
    assert_eq!(after.brief, "", "the brief must NOT be half-committed");
    assert_eq!(
        after.workflow_model, None,
        "the workflow model must NOT be half-committed"
    );
}

/// The reservation is IN-PROCESS and RAII. Had it been modelled on
/// `set_init_session` — a set-once write to a PERSISTENT field — this
/// retry would be refused forever and the draft would be bricked.
#[tokio::test]
async fn the_reservation_is_released_on_the_failure_path_so_a_retry_can_land() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    break_the_final_landing_step(&root, &shell.id);
    let input = confirmed_scaffold_input(&broker, &shell.id, "打飞机", "b", "canvas").await;
    broker
        .scaffold_shell_app_value(input.clone())
        .await
        .expect_err("the first attempt must fail");
    repair_the_final_landing_step(&root, &shell.id);

    broker
        .scaffold_shell_app_value(input)
        .await
        .expect("the retry must land");

    let after = service.record(&shell.id).await.expect("record");
    assert!(after.scaffolded);
    assert_eq!(after.name, "打飞机");
}

/// §C.1 step 1. Two scaffolds of the same app: exactly one wins, and the
/// loser is refused BY THE RESERVATION — asserted on the stable
/// `scaffold_in_flight` prefix so the test cannot pass because the second
/// call failed for some unrelated reason.
#[tokio::test]
async fn two_concurrent_scaffolds_reject_the_second_at_the_in_process_reservation() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (_root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let input = confirmed_scaffold_input(&broker, &shell.id, "A", "b", "dom").await;
    let (first, second) = tokio::join!(
        broker.scaffold_shell_app_value(input.clone()),
        broker.scaffold_shell_app_value(input),
    );
    assert!(
        first.is_ok() ^ second.is_ok(),
        "exactly one must win: {first:?} / {second:?}"
    );
    let refusal = first.err().or(second.err()).expect("one must be refused");
    assert!(
        refusal.contains("scaffold_in_flight"),
        "the loser must be stopped by the reservation, not by anything else: {refusal}"
    );
}

/// An OUTCOME test for scaffold-versus-delete: whichever wins, the delete
/// completes, no record survives, and no directory is left behind.
///
/// ⚠️ Honest about what it does NOT prove: it does not discriminate on
/// `lock_app_build`. Removing the lock entirely leaves this test green,
/// because on a wall-clock race the delete finishes the whole trash
/// removal before the landing's first write and `wipe_editable_surface`
/// then refuses a workspace that is not there. The orphan the lock
/// prevents needs the delete to land INSIDE the seed loop, a window no
/// timing-based test can be made to hit reliably. What actually pins the
/// lock is
/// [`the_landing_takes_the_build_lock_first_and_hands_it_back_held`].
#[tokio::test]
async fn a_concurrent_delete_cannot_orphan_a_scaffold_in_flight() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let app_dir = root.path().join("apps").join(&shell.id);
    assert!(app_dir.is_dir(), "the fixture must exist to be raced");
    let input = confirmed_scaffold_input(&broker, &shell.id, "A", "b", "dom").await;

    let (scaffolded, deleted) = tokio::join!(
        broker.scaffold_shell_app_value(input),
        service.delete_app(&shell.id),
    );
    deleted.expect("the delete must complete");
    assert!(
        service.record(&shell.id).await.is_err(),
        "a completed delete must leave no record, however the scaffold ended: {scaffolded:?}"
    );
    assert!(
        !app_dir.exists(),
        "no orphan workspace may survive the delete (scaffold outcome: {scaffolded:?})"
    );
}

/// §C.1 step 3a, and the discriminating test for it: the landing takes
/// `lock_app_build` BEFORE it touches anything, and the guard it hands
/// back is still held — which is what keeps a concurrent delete out of
/// both the seed loop and the window before the commit point.
///
/// A landing that took no lock would sail past a contender that already
/// holds it, and the first timeout below would not fire.
#[tokio::test]
async fn the_landing_takes_the_build_lock_first_and_hands_it_back_held() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let mut proposed = shell.clone();
    proposed.name = "A".into();
    proposed.brief = "b".into();
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("published react-dom runtime profile");

    let contend = |root: PathBuf, app_id: String| {
        tokio::task::spawn_blocking(move || local_apps::storage::lock_app_build(&root, &app_id))
    };
    let contender = contend(root.path().to_path_buf(), shell.id.clone())
        .await
        .expect("join the contender")
        .expect("the contender must get the lock first");

    let landing = tokio::spawn({
        let broker = Arc::clone(&broker);
        let binding = binding.clone();
        async move {
            broker
                .land_scaffold(&proposed, local_apps::AppSurface::Dom, Some(binding), None)
                .await
        }
    });
    let mut landing = landing;
    assert!(
        timeout(Duration::from_millis(400), &mut landing)
            .await
            .is_err(),
        "the landing must WAIT for the build lock before it writes anything"
    );
    assert!(
        !workspace_of(&root, &shell.id).join("app/app.jsx").exists(),
        "and it must not have seeded while it was waiting"
    );

    drop(contender);
    let held = timeout(Duration::from_secs(30), landing)
        .await
        .expect("the landing must proceed once the lock is free")
        .expect("join the landing")
        .expect("the landing must succeed");
    assert!(workspace_of(&root, &shell.id).join("app/app.jsx").is_file());

    let (held_build, held_recovery, recovery) = held;

    assert!(
        timeout(
            Duration::from_millis(400),
            contend(root.path().to_path_buf(), shell.id.clone()),
        )
        .await
        .is_err(),
        "the landing must STILL hold the lock when it returns, so the \
         commit point runs under it"
    );
    recovery
        .rollback()
        .expect("discard the uncommitted landing");
    drop(held_build);
    drop(held_recovery);
    timeout(
        Duration::from_secs(30),
        contend(root.path().to_path_buf(), shell.id.clone()),
    )
    .await
    .expect("the lock must become free once the landing's guard drops")
    .expect("join the contender")
    .expect("acquire the freed build lock");
}

/// A formed app is refused. Its workspace holds the user's own source and
/// a second landing WIPES the editable surface before seeding, so this
/// refusal is what stands between a stray tool call and the user's work.
#[tokio::test]
async fn scaffolding_a_formed_app_is_rejected() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "A", "b", "dom").await,
        )
        .await
        .expect("the first scaffold must land");
    let workspace = workspace_of(&root, &shell.id);
    fs::write(
        workspace.join("app/screens/mine.jsx"),
        b"// the user's own work",
    )
    .expect("write user source");

    let workflow_run_id = format!("wf_rescaffold_{}", uuid::Uuid::new_v4().simple());
    let catalog =
        crate::mobile::local_app_template_catalog::catalog_view().expect("template catalog");
    let selector_capability =
        crate::mobile::local_app_template_catalog::issue_selector_capability(
            &broker.root,
            &shell.id,
            &workflow_run_id,
        )
        .expect("selector capability");
    let error = broker
        .validate_template_selection(json!({
            "app_id": shell.id,
            "workflow_run_id": workflow_run_id,
            "catalog_digest": catalog.catalog_digest,
            "template_id": "react-dom-r4",
            "reason": "attempt to resurface a formed app",
            "rejected": [],
            "selector_capability": selector_capability,
        }))
        .await
        .expect_err("a formed app must be rejected before a second receipt is issued");
    assert!(error.contains("already"), "got {error}");

    assert!(
        workspace.join("app/screens/mine.jsx").is_file(),
        "the refusal must have happened BEFORE the wipe"
    );
    let after = service.record(&shell.id).await.expect("record");
    assert_eq!(after.name, "A", "the committed name must be untouched");
    let contract = fs::read_to_string(workspace.join("LINGXI.md")).expect("read the contract");
    assert!(
        contract.contains("# Local App: A"),
        "the one-and-only contract must be untouched: {contract}"
    );
}

/// §C.1 step 2, and it must refuse BEFORE touching the workspace: an
/// invalid argument may not cost the user the interview's workspace.
#[tokio::test]
async fn scaffold_rejects_an_empty_brief_and_an_unknown_surface() {
    let (root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;

    for (name, brief, expected) in [
        ("A", "   ", "brief must be a non-empty string"),
        ("   ", "b", "name must be a non-empty string"),
    ] {
        let error = broker
            .scaffold_shell_app_value(
                confirmed_scaffold_input(&broker, &shell.id, name, brief, "dom").await,
            )
            .await
            .expect_err("must be rejected");
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    }
    let mut surface_override =
        confirmed_scaffold_input(&broker, &shell.id, "A", "b", "dom").await;
    surface_override["surface"] = json!("webgl");
    let error = broker
        .scaffold_shell_app_value(surface_override)
        .await
        .expect_err("surface overrides must be rejected");
    assert!(
        error.contains("Host-issued scaffold receipt is authoritative"),
        "got {error}"
    );
    let over_long = "x".repeat(local_apps::service::MAX_NAME_BYTES + 1);
    let error = broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, &over_long, "b", "dom").await,
        )
        .await
        .expect_err("an over-long name must be rejected");
    assert!(error.contains("limit"), "got {error}");

    let after = service.record(&shell.id).await.expect("record");
    assert!(!after.scaffolded);
    let contract = fs::read_to_string(workspace_of(&root, &shell.id).join("LINGXI.md"))
        .expect("read the contract");
    assert!(
        contract.contains("has no shape yet"),
        "a rejected argument must not have touched the workspace: {contract}"
    );
}

/// r3-tests-honesty-06: `stage_create`'s own name/brief bound is the one
/// that governs the value actually committed at scaffold time — every
/// other test drives `scaffold_shell_app_value` through
/// `confirmed_scaffold_input`, whose helper deliberately substitutes a
/// safe placeholder whenever the caller's name/brief would fail this
/// exact check, so `stage_create`'s bound itself was never directly
/// exercised. Send an over-long value straight to `stage_create`.
#[tokio::test]
async fn stage_create_rejects_an_over_long_name_or_brief() {
    let (_root, service, broker) = create_broker(false, None).await;
    let shell = shell_app_fixture(&broker, &service).await;

    async fn stage_input(
        broker: &Arc<LocalAppsHostBroker>,
        app_id: &str,
        name: &str,
        brief: &str,
    ) -> Value {
        let workflow_run_id = format!("wf_bound_{}", uuid::Uuid::new_v4().simple());
        let catalog = crate::mobile::local_app_template_catalog::catalog_view()
            .expect("template catalog");
        let selector_capability =
            crate::mobile::local_app_template_catalog::issue_selector_capability(
                &broker.root,
                app_id,
                &workflow_run_id,
            )
            .expect("selector capability");
        let selection = broker
            .validate_template_selection(json!({
                "app_id": app_id,
                "workflow_run_id": workflow_run_id,
                "catalog_digest": catalog.catalog_digest,
                "template_id": "react-dom-r4",
                "reason": "stage_create bound test",
                "rejected": [],
                "selector_capability": selector_capability,
            }))
            .await
            .expect("validated selection");
        let handle = selection["validated_selection_handle"]
            .as_str()
            .expect("selection handle");
        json!({
            "app_id": app_id,
            "workflow_run_id": workflow_run_id,
            "validated_selection_handle": handle,
            "quality_level": "fast",
            "name": name,
            "brief": brief,
        })
    }

    let over_long_name = "x".repeat(local_apps::service::MAX_NAME_BYTES + 1);
    let error = broker
        .stage_create(stage_input(&broker, &shell.id, &over_long_name, "b").await)
        .await
        .expect_err("an over-long name must be rejected by stage_create itself");
    assert!(
        error.contains("invalid_argument: name is") && error.contains("limit"),
        "expected stage_create's own name-bound message, got: {error}"
    );

    let over_long_brief = "y".repeat(local_apps::service::MAX_BRIEF_BYTES + 1);
    let error = broker
        .stage_create(stage_input(&broker, &shell.id, "A", &over_long_brief).await)
        .await
        .expect_err("an over-long brief must be rejected by stage_create itself");
    assert!(
        error.contains("invalid_argument: brief is") && error.contains("limit"),
        "expected stage_create's own brief-bound message, got: {error}"
    );

    let error = broker
        .stage_create(stage_input(&broker, &shell.id, "   ", "b").await)
        .await
        .expect_err("an empty/whitespace name must be rejected by stage_create itself");
    assert!(
        error.contains("name must be a non-empty string"),
        "expected stage_create's own empty-name message, got: {error}"
    );

    assert!(
        !service.record(&shell.id).await.expect("record").scaffolded,
        "a rejected stage_create must not have scaffolded the app"
    );
}

/// §C.1.4. `AppManifest::hash()` serialises the WHOLE struct INCLUDING
/// `name`, and `AppDataStore::ensure_manifest` compares it against the
/// SQLite `_lingxi_schema.manifest_hash`. Writing `name` after a store
/// exists breaks EVERY data read and write with "database manifest
/// mismatch" — which is why the first landing is the only write window,
/// and why renaming an app is not offered at all.
#[tokio::test]
async fn the_manifest_name_may_only_be_written_before_any_database_exists() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(false, Some(runtime)).await;
    let shell = shell_app_fixture(&broker, &service).await;
    let layout = AppLayout::new(root.path().to_path_buf(), shell.id.clone()).expect("layout");
    assert!(
        !layout.database_path().exists(),
        "the write window is exactly 'no data store yet'"
    );

    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(&broker, &shell.id, "A", "b", "dom").await,
        )
        .await
        .expect("the first landing must be allowed to write the name");
    assert_eq!(load_manifest(&layout).expect("manifest").name, "A");

    let opened = layout.clone();
    tokio::task::spawn_blocking(move || AppDataStore::open(opened).map(|_| ()))
        .await
        .expect("join data store worker")
        .expect("open the app data store");
    assert!(layout.database_path().exists(), "the store must be on disk");

    let artifacts = scaffold_runtime_profile(
        Some(
            crate::mobile::local_app_runtime_profiles::current_binding_for_family(
                local_apps::AppRuntimeProfile::ReactDom,
            )
            .expect("published react-dom runtime profile"),
        ),
        local_apps::AppSurface::Dom,
    )
    .expect("react-dom profile");
    let error = stamp_scaffold_identity(
        &layout,
        "B",
        &artifacts.binding,
        builtin_template_origin(&artifacts.binding),
    )
    .expect_err("a name rewrite after the store exists must be refused");
    assert!(error.contains("database"), "got {error}");
    assert_eq!(
        load_manifest(&layout).expect("manifest").name,
        "A",
        "the refusal must have happened before the write"
    );
}

/// `create_app_fixture` with a CHOSEN id, for the one test whose exercised
/// PORT is derived from the app id.
///
/// `AppService::create_app` mints a random id by design and offers no way
/// to supply one, so an app created through it makes
/// `derived_window_slot` land somewhere different on every run: six
/// consecutive runs of the test below probed 26208, 20613, 26371, 24337,
/// 30061 and 27589.  Every one passed, which is the problem — the run that
/// eventually does not cannot be repeated.
///
/// The app documents come from `save_app_files`, the same `storage` writer
/// `create_app` commits, and the service then LOADS them, so the RECORD
/// the start path below sees is the one a process restart would hand it.
/// That is the whole of the fidelity claim, and two things sit outside it.
///
/// The index goes through `save_index`, which REPLACES `apps/index.json`
/// wholesale, not `create_app`'s locked, merging `save_index_preserving` —
/// hence two constraints: run this BEFORE the service loads the root, and
/// only once per root.
///
/// And `create_app` also writes `manifest.json` and `permissions.json`
/// (`save_manifest` / `save_permissions`, neither of which lives in
/// `storage`) while this writes neither.  `load_permissions` returns the
/// deny-by-default state when its file is absent, but `load_manifest`
/// returns `NotFound`, so a seeded app cannot stand in for a created one on
/// a manifest-reading path — the bridge and capability handlers above.
fn seed_app_fixture(root: &TempDir, app_id: &str, name: &str) {
    let app = AppState::create(
        app_id.to_string(),
        name.to_string(),
        "a test app".to_string(),
        None,
        1,
    );
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.to_string()).expect("layout");
    layout.initialize().expect("initialize the seeded layout");
    storage::save_app_files(root.path(), &app).expect("persist the seeded app documents");
    local_apps::save_manifest(
        &layout,
        &local_apps::AppManifest::for_new_app(app.record.id.clone(), app.record.name.clone()),
    )
    .expect("persist the seeded manifest");
    local_apps::save_permissions(&layout, &local_apps::AppPermissions::default())
        .expect("persist the seeded permissions");
    local_apps::save_workspace_permission_settings(&layout)
        .expect("persist the seeded workspace permissions");
    storage::save_index(root.path(), std::slice::from_ref(&app.record))
        .expect("persist the seeded index");
    seed_launchable_runtime_fixture(root.path(), &app.record, name);
}

/// A registry of its own for a probe that drives `bind_stable_loopback`
/// directly.  Production hands it the BROKER's — see
/// `a_first_start_skips_a_port_a_concurrent_start_has_leased`, which is
/// what holds that wiring in place.
fn test_leases() -> PortLeases {
    Arc::new(std::sync::Mutex::new(HashMap::new()))
}

/// An `AppService` holding no apps, for the probes that drive
/// `bind_stable_loopback` directly and hand it their sibling pins by hand.
///
/// The allocator re-reads the pins from this service after leasing a
/// candidate, so an EMPTY one is what keeps those probes saying what their
/// names say: the re-read contributes no exclusion, leaving the passed
/// snapshot as the only one in play.  A probe that wants the re-read itself
/// seeds a real pin instead — see
/// `a_pin_that_lands_after_the_snapshot_is_caught_before_the_choice_sticks`.
///
/// The returned `TempDir` has to outlive the service; binding it to `_`
/// drops it immediately and pulls the app root out from under the load.
async fn empty_registry() -> (TempDir, Arc<AppService>) {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    (root, service)
}

async fn wait_until<F, Fut>(label: &str, timeout_duration: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    timeout(timeout_duration, async {
        loop {
            if condition().await {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

#[test]
fn static_paths_reject_traversal_and_encoding() {
    assert_eq!(safe_static_path("/"), Some(PathBuf::from("index.html")));
    assert_eq!(
        safe_static_path("/assets/app.js?v=1"),
        Some(PathBuf::from("assets/app.js"))
    );
    assert_eq!(safe_static_path("/../secret"), None);
    assert_eq!(safe_static_path("/%2e%2e/secret"), None);
    assert_eq!(safe_static_path("/assets\\secret"), None);
}

#[test]
fn static_assets_use_safe_cache_policy() {
    assert_eq!(
        static_cache_control(Path::new("assets/index-0123abcd.js")),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(static_cache_control(Path::new("index.html")), "no-cache");
    assert_eq!(
        static_cache_control(Path::new("assets/runtime.js")),
        "no-store"
    );
    assert!(is_hashed_asset(Path::new(
        "assets/nested/chunk-deadbeef.css"
    )));
    assert!(!is_hashed_asset(Path::new("assets/chunk-short.js")));
}

#[test]
fn if_none_match_supports_weak_lists_and_wildcard() {
    assert!(etag_matches("W/\"abc\", \"def\"", "\"def\""));
    assert!(etag_matches("*", "\"anything\""));
    assert!(!etag_matches("\"old\"", "\"new\""));
}

#[test]
fn local_app_perf_diagnostic_is_opt_in_and_has_stable_safe_fields() {
    assert_eq!(
        local_app_perf_diagnostic_line(
            false,
            "dependency_snapshot_verify",
            Duration::from_micros(42),
        ),
        None,
        "the Local App timing diagnostic must be silent unless explicitly enabled"
    );
    assert_eq!(
        local_app_perf_diagnostic_line(
            true,
            "dependency_snapshot_verify",
            Duration::from_micros(42),
        )
        .as_deref(),
        Some("[local-app-perf] phase=dependency_snapshot_verify elapsed_us=42"),
        "diagnostics expose only a fixed phase and elapsed duration"
    );
}

#[test]
fn dependency_snapshot_is_atomic_and_reusable() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("install/node_modules");
    fs::create_dir_all(source.join("vite/bin")).expect("source tree");
    fs::write(source.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    fs::write(source.join("react.js"), b"react").expect("dependency");
    let snapshot = root.path().join("cache/snapshot");
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("publish snapshot");
    assert!(LocalAppsHostBroker::dependency_snapshot_is_ready(
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY
    )
    .expect("validate snapshot"));
    let marker = snapshot.join(DEPENDENCY_SNAPSHOT_READY_FILE);
    fs::remove_file(&marker).expect("remove marker for malformed-cache probe");
    fs::write(&marker, [0xff]).expect("write malformed snapshot marker");
    assert!(!LocalAppsHostBroker::dependency_snapshot_is_ready(
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY
    )
    .expect("malformed marker is a safe miss"));
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("rebuild snapshot with malformed marker");
    fs::remove_file(snapshot.join("node_modules/react.js"))
        .expect("remove read-only file for corruption probe");
    fs::write(snapshot.join("node_modules/react.js"), b"tampered")
        .expect("replace snapshot dependency bytes");
    assert!(
        !LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("revalidate modified snapshot"),
        "an earlier successful lookup must not memoize trust after tree bytes change"
    );
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("rebuild modified snapshot");
    assert!(LocalAppsHostBroker::dependency_snapshot_is_ready(
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY
    )
    .expect("validate rebuilt snapshot"));
    let invalid_package = snapshot.join("node_modules/untrusted");
    fs::create_dir_all(&invalid_package).expect("invalid cached package directory");
    fs::write(
        invalid_package.join("package.json"),
        r#"{"name":"untrusted","version":"1.0.0","scripts":{"install":"node install.js"}}"#,
    )
    .expect("invalid cached package manifest");
    assert!(
        !LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("invalid cached tree is a safe miss"),
        "a cached tree that fails dependency validation must be rebuilt, not trusted"
    );
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("rebuild invalid cached tree");
    assert!(LocalAppsHostBroker::dependency_snapshot_is_ready(
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY
    )
    .expect("validate tree after invalid-cache rebuild"));
    fs::write(source.join("react.js"), b"replacement source")
        .expect("change later source tree");
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("a valid snapshot is already published");
    assert_eq!(
        fs::read(snapshot.join("node_modules/react.js")).expect("published dependency"),
        b"react",
        "a verified immutable snapshot must not be replaced by a later same-lock source"
    );

    let staging = root.path().join("staging");
    fs::create_dir_all(&staging).expect("staging");
    LocalAppsHostBroker::materialize_dependency_snapshot(&snapshot, &staging)
        .expect("materialize snapshot");
    assert_eq!(
        fs::read(staging.join("node_modules/react.js")).expect("materialized dependency"),
        b"react"
    );

    let workspace = root.path().join("workspace");
    LocalAppsHostBroker::materialize_dependency_snapshot(&snapshot, &workspace)
        .expect("materialize workspace dependencies");
    let tree_digest = dependency_tree_digest(&workspace.join("node_modules"))
        .expect("workspace dependency digest");
    let attestation_path = workspace.join(WORKSPACE_DEPENDENCY_ATTESTATION_FILE);
    fs::create_dir_all(attestation_path.parent().expect("attestation parent"))
        .expect("attestation directory");
    fs::write(
        &attestation_path,
        dependency_attestation("lock-digest", &tree_digest, PNPM_TOOLCHAIN_KEY),
    )
    .expect("workspace attestation");
    assert!(LocalAppsHostBroker::workspace_dependencies_match_snapshot(
        &workspace,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY
    )
    .expect("match attested workspace dependencies"));
}

#[test]
fn dependency_snapshot_inventory_is_verified_and_reused_for_sbom() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("install/node_modules");
    fs::create_dir_all(source.join("vite/bin")).expect("source tree");
    fs::write(source.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    write_fixture_package_manifest(&source, "vite", "6.0.0");
    write_fixture_package_manifest(&source, "react", "19.2.8");
    let snapshot = root.path().join("cache/snapshot");
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("publish snapshot");
    assert!(
        dependency_snapshot_inventory_path(&snapshot).is_file(),
        "verified snapshots carry their package inventory beside the immutable tree"
    );
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("published react-dom runtime profile");
    let missing_workspace = root.path().join("no-workspace-node-modules");
    let sbom = installed_dependency_sbom_with_inventory(
        &missing_workspace,
        &binding,
        &dependency_tree_digest(&snapshot.join("node_modules")).expect("tree digest"),
        Some((&snapshot, "lock-digest")),
    )
    .expect("a valid inventory avoids rescanning the workspace tree");
    let document: Value = serde_json::from_slice(&sbom).expect("sbom json");
    assert!(
        document["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .any(|package| package["name"] == "vite"),
        "the cached package inventory must feed the profile SBOM"
    );
    let canvas_binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::Canvas2d,
    )
    .expect("published canvas runtime profile");
    let canvas_sbom = installed_dependency_sbom_with_inventory(
        &missing_workspace,
        &canvas_binding,
        &dependency_tree_digest(&snapshot.join("node_modules")).expect("tree digest"),
        Some((&snapshot, "lock-digest")),
    )
    .expect("the shared inventory can feed a second profile's fresh SBOM");
    let canvas_document: Value =
        serde_json::from_slice(&canvas_sbom).expect("canvas SBOM JSON");
    assert_ne!(
        document["documentNamespace"], canvas_document["documentNamespace"],
        "shared package inventory must not reuse another profile's document identity"
    );
    assert_ne!(
        document["documentDescribes"], canvas_document["documentDescribes"],
        "shared package inventory must not reuse another profile's SPDX root"
    );

    let inventory_path = dependency_snapshot_inventory_path(&snapshot);
    let mut unbound: Value =
        serde_json::from_slice(&fs::read(&inventory_path).expect("inventory"))
            .expect("inventory JSON");
    unbound["unbound_metadata"] = json!("must not be ignored");
    fs::remove_file(&inventory_path).expect("remove inventory for unknown-field probe");
    fs::write(
        &inventory_path,
        serde_json::to_vec_pretty(&unbound).expect("inventory with unknown field"),
    )
    .expect("write inventory with unknown field");
    assert!(
        !LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("unknown inventory fields are a cache miss"),
        "unbound sidecar fields must not sit outside the inventory digest"
    );
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("an inventory with unknown fields must be rebuilt");

    let mut noncanonical: VerifiedDependencyInventory =
        serde_json::from_slice(&fs::read(&inventory_path).expect("inventory"))
            .expect("inventory JSON");
    noncanonical.packages.reverse();
    noncanonical.inventory_digest =
        dependency_inventory_digest(&noncanonical).expect("reordered inventory digest");
    fs::remove_file(&inventory_path).expect("remove immutable inventory for ordering probe");
    fs::write(
        &inventory_path,
        serde_json::to_vec_pretty(&noncanonical).expect("reordered inventory JSON"),
    )
    .expect("write reordered inventory");
    assert!(
        !LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("noncanonical inventory is a cache miss"),
        "an inventory with a valid self-digest must still use canonical package ordering"
    );
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("a noncanonical sidecar must rebuild from the verified source tree");

    let mut inventory: Value =
        serde_json::from_slice(&fs::read(&inventory_path).expect("inventory"))
            .expect("inventory json");
    inventory["tree_digest"] = json!("tampered");
    fs::remove_file(&inventory_path).expect("remove immutable inventory for corruption probe");
    fs::write(
        &inventory_path,
        serde_json::to_vec_pretty(&inventory).expect("tampered inventory json"),
    )
    .expect("tamper inventory");
    assert!(
        !LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("tampered inventory is a cache miss"),
        "a sidecar with mismatched provenance must not create ready trust"
    );
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("a malformed sidecar must rebuild from the verified source tree");
    assert!(
        LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("rebuilt snapshot"),
        "rebuilding a bad cache must restore a verified snapshot"
    );
}

#[test]
fn dependency_tree_digest_frames_file_boundaries() {
    let root = TempDir::new().expect("tempdir");
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&first).expect("first tree");
    fs::create_dir_all(&second).expect("second tree");
    fs::write(first.join("a"), b"bc").expect("first file");
    fs::write(first.join("d"), b"X").expect("first boundary file");
    fs::write(second.join("a"), b"b").expect("second file");
    fs::write(second.join("cd"), b"X").expect("second boundary file");

    assert_ne!(
        dependency_tree_digest(&first).expect("first digest"),
        dependency_tree_digest(&second).expect("second digest"),
        "path/content boundaries must be unambiguous",
    );
}

#[cfg(unix)]
#[test]
fn dependency_snapshot_rejects_symlink_entries() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("node_modules");
    fs::create_dir_all(&source).expect("source tree");
    fs::write(root.path().join("outside"), b"outside").expect("outside");
    std::os::unix::fs::symlink(root.path().join("outside"), source.join("escape"))
        .expect("symlink");
    let error = validate_dependency_tree(&source).expect_err("symlink must be rejected");
    assert!(error.contains("symlink"), "{error}");
}

#[test]
fn dependency_snapshot_rejects_native_node_addons() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("node_modules/pkg");
    fs::create_dir_all(&source).expect("source tree");
    fs::write(source.join("binding.node"), b"native").expect("native addon");
    let error = validate_dependency_tree(root.path().join("node_modules").as_path())
        .expect_err("native addons must be rejected");
    assert!(error.contains("native Node addon"), "{error}");
}

#[cfg(unix)]
#[test]
fn dependency_snapshot_rejects_native_node_addon_symlink_paths() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("node_modules/pkg");
    fs::create_dir_all(&source).expect("source tree");
    fs::write(source.join("payload.bin"), b"payload").expect("payload");
    std::os::unix::fs::symlink("payload.bin", source.join("binding.node"))
        .expect("native addon symlink");

    let error = validate_dependency_tree(root.path().join("node_modules").as_path())
        .expect_err("native addon symlink path must be rejected");
    assert!(error.contains("native Node addon symlink"), "{error}");
}

#[cfg(unix)]
#[test]
fn dependency_snapshot_rejects_symlinks_that_resolve_to_native_addons() {
    let root = TempDir::new().expect("tempdir");
    let pkg = root.path().join("node_modules/pkg");
    let bin = root.path().join("node_modules/.bin");
    fs::create_dir_all(&pkg).expect("package tree");
    fs::create_dir_all(&bin).expect("bin tree");
    fs::write(pkg.join("binding.node"), b"native").expect("native addon");
    std::os::unix::fs::symlink("../pkg/binding.node", bin.join("native-shim"))
        .expect("native shim");

    let error = validate_dependency_tree(root.path().join("node_modules").as_path())
        .expect_err("symlink target native addon must be rejected");
    assert!(error.contains("native Node addon"), "{error}");
}

#[test]
fn dependency_native_bindings_accept_each_pinned_version_only() {
    for (package, version, binary) in TRUSTED_TOOLCHAIN_NATIVE_BINDINGS {
        let root = TempDir::new().unwrap();
        let node_modules = root.path().join("node_modules");
        let package_dir = node_modules.join(package);
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(package_dir.join(binary), b"\x7fELFfixture").unwrap();
        fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec(&json!({"name": package, "version": version})).unwrap(),
        )
        .unwrap();
        validate_dependency_tree(&node_modules)
            .unwrap_or_else(|error| panic!("{package}@{version}: {error}"));
        fs::write(
            package_dir.join("package.json"),
            serde_json::to_vec(&json!({"name": package, "version": "99.0.0"})).unwrap(),
        )
        .unwrap();
        assert!(
            validate_dependency_tree(&node_modules).is_err(),
            "unreviewed {package} version must fail"
        );
    }
}

#[test]
fn dependency_snapshot_accepts_trusted_toolchain_native_bindings_and_prepare_metadata() {
    let root = TempDir::new().expect("tempdir");
    let node_modules = root.path().join("node_modules");
    for (package, version, _) in TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS {
        let package_dir = node_modules.join(package);
        fs::create_dir_all(&package_dir).expect("lifecycle package dir");
        fs::write(
            package_dir.join("package.json"),
            format!(
                "{{\"name\":\"{package}\",\"version\":\"{version}\",\"scripts\":{{\"prepare\":\"node prepare.js\"}}}}\n"
            ),
        )
        .expect("lifecycle package manifest");
    }
    for (package, version, binary) in TRUSTED_TOOLCHAIN_NATIVE_BINDINGS {
        let package_dir = node_modules.join(package);
        fs::create_dir_all(&package_dir).expect("binding dir");
        fs::write(
            package_dir.join("package.json"),
            format!("{{\"name\":\"{package}\",\"version\":\"{version}\"}}\n"),
        )
        .expect("binding manifest");
        fs::write(package_dir.join(binary), b"\x7fELFfixture").expect("binding binary");
    }

    validate_dependency_tree(&node_modules)
        .expect("trusted fixed-toolchain binding and prepare metadata are allowed");
}

#[cfg(unix)]
#[test]
fn dependency_snapshot_accepts_trusted_toolchain_entries_through_a_canonicalized_parent() {
    let root = TempDir::new().expect("tempdir");
    let real_root = root.path().join("real");
    let node_modules = real_root.join("node_modules");
    for (package, version, _) in TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS {
        let package_dir = node_modules.join(package);
        fs::create_dir_all(&package_dir).expect("lifecycle package dir");
        fs::write(
            package_dir.join("package.json"),
            format!(
                "{{\"name\":\"{package}\",\"version\":\"{version}\",\"scripts\":{{\"prepare\":\"node prepare.js\"}}}}\n"
            ),
        )
        .expect("lifecycle package manifest");
    }
    for (package, version, binary) in TRUSTED_TOOLCHAIN_NATIVE_BINDINGS {
        let package_dir = node_modules.join(package);
        fs::create_dir_all(&package_dir).expect("binding dir");
        fs::write(
            package_dir.join("package.json"),
            format!("{{\"name\":\"{package}\",\"version\":\"{version}\"}}\n"),
        )
        .expect("binding manifest");
        fs::write(package_dir.join(binary), b"\x7fELFfixture").expect("binding binary");
    }
    let alias_root = root.path().join("alias");
    std::os::unix::fs::symlink(&real_root, &alias_root).expect("alias root");

    validate_dependency_tree(&alias_root.join("node_modules"))
        .expect("trusted entries must survive canonical root/path spelling differences");
}

#[test]
fn dependency_snapshot_accepts_trusted_hoisted_lifecycle_manifests_recursively() {
    let root = TempDir::new().expect("tempdir");
    let node_modules = root.path().join("node_modules");
    let package_dir = node_modules.join("balanced-match");
    fs::create_dir_all(package_dir.join("dist")).expect("package dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"balanced-match","version":"4.0.4","scripts":{"prepare":"node prepare.js"}}"#,
    )
    .expect("package manifest");
    fs::write(package_dir.join("dist/index.js"), "export {};\n").expect("nested file");

    validate_dependency_tree(&node_modules)
        .expect("recursive validation must keep the hoisted dependency root stable");
}

#[test]
fn dependency_snapshot_rejects_untrusted_lifecycle_scripts() {
    let root = TempDir::new().expect("tempdir");
    let package_dir = root.path().join("node_modules/dayjs");
    fs::create_dir_all(&package_dir).expect("package dir");
    fs::write(
        package_dir.join("package.json"),
        r#"{"name":"dayjs","version":"1.11.13","scripts":{"prepare":"node build.js"}}"#,
    )
    .expect("package manifest");
    let error = validate_dependency_tree(root.path().join("node_modules").as_path())
        .expect_err("arbitrary dependency lifecycle metadata must fail closed");
    assert!(
        error.contains("forbidden lifecycle script prepare"),
        "{error}"
    );
}

#[test]
fn dependency_versions_reject_non_registry_and_alias_protocols() {
    for version in [
        "file:../pkg",
        "workspace:*",
        "patch:left-pad@1.3.0#./left-pad.patch",
        "portal:../pkg",
        "catalog:default",
        "npm:react@19.2.8",
        "git+https://example.invalid/repo.git",
    ] {
        let error = LocalAppsHostBroker::validate_dependency_version(version)
            .expect_err("only ordinary npm registry versions are accepted");
        assert!(error.contains("npm registry only"), "{version}: {error}");
    }
    for version in ["1.2.3", "^1.2.3", "~1.2.3", ">=1 <2", "latest"] {
        LocalAppsHostBroker::validate_dependency_version(version)
            .unwrap_or_else(|error| panic!("ordinary registry version {version}: {error}"));
    }
}

#[tokio::test]
async fn dependency_install_preserves_current_toolchain_and_cache_identity() {
    use crate::mobile::local_apps_build::{scaffold_workspace, LocalAppBuildTarget};
    for target in [
        LocalAppBuildTarget::ReactDomR4,
        LocalAppBuildTarget::Canvas2dR4,
    ] {
        let toolchain = RuntimeToolchain::Current;
        let root = TempDir::new().unwrap();
        let service = test_service(&root).await;
        let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
        let broker = LocalAppsHostBroker::new(
            root.path().to_path_buf(),
            MockSink::arc(),
            Some(runtime.clone()),
            false,
            None,
        );
        assert!(broker.attach_service(service.clone()).is_ok());
        let shell = shell_app_fixture(&broker, &service).await;
        let layout = broker.layout(&shell.id).unwrap();
        scaffold_workspace(&layout, target).unwrap();
        let workspace = layout.root().join(layout.workspace_rel());
        let widget_path = "app/mcp-widget/package.json";
        let staged = LocalAppsHostBroker::prepare_dependency_staging(&layout).unwrap();
        assert_eq!(
            staged.join(widget_path).exists(),
            toolchain == RuntimeToolchain::Current,
            "the current workspace stages the widget importer"
        );
        let before = fs::read(workspace.join("pnpm-workspace.yaml")).unwrap();
        let completion = broker
            .dependency_install_once(&layout, &shell.id)
            .await
            .expect("install with exact pinned toolchain");
        assert_eq!(completion.toolchain_key, toolchain.key());
        let requests = runtime.isolated_requests().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].command, toolchain.pnpm_command());
        assert_eq!(
            requests[0].env.get("PATH").map(String::as_str),
            Some(toolchain.path())
        );
        assert_eq!(
            fs::read(workspace.join("pnpm-workspace.yaml")).unwrap(),
            before,
            "dependency installation must not rewrite the pinned workspace contract"
        );
        let snapshot =
            broker.dependency_snapshot_root(&completion.lockfile_sha256, toolchain.key());
        assert!(LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            &completion.lockfile_sha256,
            toolchain.key()
        )
        .unwrap());
        let other = "unrecognized-toolchain";
        assert_ne!(
            snapshot,
            broker.dependency_snapshot_root(&completion.lockfile_sha256, other)
        );
        assert!(
            !LocalAppsHostBroker::dependency_snapshot_is_ready(
                &snapshot,
                &completion.lockfile_sha256,
                other
            )
            .unwrap(),
            "another toolchain cannot trust this marker/inventory"
        );
        let manifest = load_manifest(&layout).unwrap();
        assert_eq!(
            manifest.dependency_snapshot.unwrap().toolchain_key,
            toolchain.key()
        );
        assert!(
            fs::read_to_string(workspace.join(WORKSPACE_DEPENDENCY_ATTESTATION_FILE))
                .unwrap()
                .contains(toolchain.key())
        );
        let warm = broker
            .dependency_install_once(&layout, &shell.id)
            .await
            .expect("same-toolchain warm cache");
        assert_eq!(warm.toolchain_key, toolchain.key());
        assert_eq!(
            runtime.isolated_requests().await.len(),
            1,
            "warm snapshot must not reinstall"
        );
    }
}

#[tokio::test]
async fn dependency_staging_preserves_the_pinned_widget_importer() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        Some(MockMobileLinuxRuntime::new(Duration::ZERO)),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let shell = shell_app_fixture(&broker, &service).await;
    broker
        .scaffold_shell_app_value(
            confirmed_scaffold_input(
                &broker,
                &shell.id,
                "Widget staging",
                "widget test",
                "dom",
            )
            .await,
        )
        .await
        .expect("scaffold");
    let layout = broker.layout(&shell.id).expect("layout");
    let workspace = layout.root().join(layout.workspace_rel());
    let path = "app/mcp-widget/package.json";
    let expected = fs::read(workspace.join(path)).expect("seeded widget manifest");
    let staging =
        LocalAppsHostBroker::prepare_dependency_staging(&layout).expect("stage dependencies");
    assert_eq!(fs::read(staging.join(path)).unwrap(), expected);
    fs::write(
        workspace.join(path),
        b"{\"dependencies\":{\"unapproved\":\"1.0.0\"}}",
    )
    .unwrap();
    assert!(!LocalAppsHostBroker::dependency_inputs_match(&layout).unwrap());
    let staging = LocalAppsHostBroker::prepare_dependency_staging(&layout)
        .expect("stage pinned dependencies");
    assert_eq!(
        fs::read(staging.join(path)).unwrap(),
        expected,
        "editable metadata cannot introduce unapproved dependencies"
    );
}

#[test]
fn pnpm_lock_documents_preserve_legacy_and_reject_ambiguous_graphs() {
    let legacy = include_bytes!(
        "../../../../../plugins/lingxi-local-app/assets/templates/react-dom/r4/pnpm-lock.yaml"
    );
    let legacy_package = include_bytes!(
        "../../../../../plugins/lingxi-local-app/assets/templates/react-dom/r4/package.json"
    );
    validate_resolved_dependency_lock(legacy_package, legacy)
        .expect("pnpm 11 single-document lock");
    let current = include_str!(
        "../../../../../plugins/lingxi-local-app/assets/templates/react-dom/r4/pnpm-lock.yaml"
    );
    let package = include_bytes!(
        "../../../../../plugins/lingxi-local-app/assets/templates/react-dom/r4/package.json"
    );
    validate_resolved_dependency_lock(package, current.as_bytes())
        .expect("pnpm 12 configuration plus dependency documents");
    let docs: Vec<_> = current
        .split("---\n")
        .filter(|doc| !doc.trim().is_empty())
        .collect();
    assert_eq!(docs.len(), 2);
    for (input, reason) in [
        (
            format!("{current}\n---\n{}", docs[1]),
            "multiple dependency documents",
        ),
        (
            format!("---\n{}---\n{}---\n{}", docs[0], docs[0], docs[1]),
            "ambiguous configuration documents",
        ),
        (
            format!("---\n{}---\n{}", docs[1], docs[0]),
            "ambiguous configuration documents",
        ),
        (
            format!("{current}\nimporters:\n  .:\n    dependencies:\n"),
            "repeats the importers map",
        ),
        (
            format!(
                "---\n{}\nimporters:\n  .:\n    configDependencies: {{}}\n---\n{}",
                docs[0], docs[1]
            ),
            "repeats the importers map",
        ),
        (
            current.replace(
                "specifier: 19.3.0",
                "specifier: 19.3.0\n        specifier: 19.3.0",
            ),
            "repeats the specifier",
        ),
    ] {
        let error = validate_resolved_dependency_lock(package, input.as_bytes())
            .expect_err("ambiguous or malformed locks fail closed");
        assert!(error.contains(reason), "{error}");
    }
}

#[test]
fn resolved_dependency_lock_must_match_effective_root_specifiers() {
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("react binding");
    let contract = crate::mobile::local_app_runtime_profiles::contract_for_binding(&binding)
        .expect("react contract");
    let package =
        LocalAppsHostBroker::dependency_manifest_bytes(contract).expect("profile package.json");
    let lockfile = contract
        .managed_files
        .iter()
        .find_map(|(relative, bytes)| (*relative == "pnpm-lock.yaml").then_some(*bytes))
        .expect("profile pnpm lockfile");
    validate_resolved_dependency_lock(package, lockfile)
        .expect("the published profile lock must represent its effective package");

    let mut changed: Value = serde_json::from_slice(package).expect("package JSON");
    changed["dependencies"]["dayjs"] = json!("1.11.13");
    let changed = serde_json::to_vec(&changed).expect("changed package JSON");
    let error = validate_resolved_dependency_lock(&changed, lockfile)
        .expect_err("the baseline lock cannot stand in for an added dependency");
    assert!(error.contains("dayjs@1.11.13"), "{error}");

    let resolved = mock_pnpm_lockfile(&changed).expect("mock resolved lockfile");
    validate_resolved_dependency_lock(&changed, &resolved)
        .expect("the realistic test resolver must mint the requested root graph");

    let duplicate_importers = format!(
        "{}\nimporters:\n  .:\n    dependencies:\n      dayjs:\n        specifier: 0.0.0\n        version: 0.0.0\n",
        String::from_utf8(resolved).expect("UTF-8 mock lockfile")
    );
    let error = validate_resolved_dependency_lock(&changed, duplicate_importers.as_bytes())
        .expect_err("duplicate top-level importer maps must fail closed");
    assert!(error.contains("repeats the importers map"), "{error}");
}

/// The shape `pnpm install` ACTUALLY produces for this template: every
/// package with a `bin` field gets a relative shim under `node_modules/.bin`
/// that points back inside the tree. Measured against the pinned template
/// lockfile with the engine's exact flags, that is `.bin/{jiti,nanoid,
/// rolldown,vite}` -- four internal relative symlinks, with `nodeLinker:
/// hoisted` keeping `.pnpm/` itself symlink-free.
///
/// `dependency_snapshot_rejects_symlink_entries` above only ever builds a
/// symlink that ESCAPES the tree, so it pins the real security invariant
/// while never crossing the line this fixture crosses. Rejecting internal
/// shims too means `publish_dependency_snapshot` fails on every install
/// that pnpm completes successfully, so the snapshot cache can never be
/// populated and every app re-runs a full install.
#[cfg(unix)]
#[test]
fn dependency_snapshot_accepts_internal_bin_shims() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("install/node_modules");
    fs::create_dir_all(source.join("vite/bin")).expect("source tree");
    fs::write(source.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    fs::create_dir_all(source.join(".bin")).expect("bin dir");
    std::os::unix::fs::symlink("../vite/bin/vite.js", source.join(".bin/vite"))
        .expect("bin shim");

    let snapshot = root.path().join("cache/snapshot");
    LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("a pnpm tree with internal bin shims must publish");
    assert!(LocalAppsHostBroker::dependency_snapshot_is_ready(
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY
    )
    .expect("validate snapshot"));

    let staging = root.path().join("staging");
    fs::create_dir_all(&staging).expect("staging");
    LocalAppsHostBroker::materialize_dependency_snapshot(&snapshot, &staging)
        .expect("materialize snapshot");
    // The shim must survive as a shim: Node resolves `.bin/vite` through the
    // link, so materializing it as a dangling entry would break the build
    // just as surely as dropping it.
    let shim = staging.join("node_modules/.bin/vite");
    let shim_metadata = fs::symlink_metadata(&shim).expect("materialized bin shim");
    assert!(
        shim_metadata.file_type().is_symlink(),
        "bin shim must stay a symlink"
    );
    assert_eq!(
        fs::read(&shim).expect("shim resolves to its target"),
        b"vite"
    );
}

/// Loosening "no symlinks" to "no escaping symlink" only holds if the
/// containment check is enforced where the bytes actually move. Validation
/// runs on the staged COPY, so a copy layer that faithfully reproduced an
/// escaping link would already have read through it.
#[cfg(unix)]
#[test]
fn dependency_snapshot_still_refuses_escaping_shims() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("install/node_modules");
    fs::create_dir_all(source.join("vite/bin")).expect("source tree");
    fs::write(source.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    fs::write(root.path().join("secret"), b"secret").expect("outside file");
    fs::create_dir_all(source.join(".bin")).expect("bin dir");
    std::os::unix::fs::symlink("../../../secret", source.join(".bin/exfil"))
        .expect("escaping shim");

    let snapshot = root.path().join("cache/snapshot");
    let error = LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect_err("an escaping shim must not publish");
    assert!(error.contains("escapes the tree"), "{error}");
    assert!(
        !snapshot.join("node_modules/.bin/exfil").exists(),
        "a refused publish must leave no snapshot behind"
    );
}

/// An absolute target can point inside the tree at publish time and still be
/// wrong: the snapshot is consumed from a different directory than it was
/// built in, so the link would silently re-point at the app that created it.
#[cfg(unix)]
#[test]
fn dependency_snapshot_refuses_absolute_shims_that_currently_resolve_inside() {
    let root = TempDir::new().expect("tempdir");
    let source = root.path().join("install/node_modules");
    fs::create_dir_all(source.join("vite/bin")).expect("source tree");
    fs::write(source.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    fs::create_dir_all(source.join(".bin")).expect("bin dir");
    std::os::unix::fs::symlink(source.join("vite/bin/vite.js"), source.join(".bin/vite"))
        .expect("absolute shim");

    let snapshot = root.path().join("cache/snapshot");
    let error = LocalAppsHostBroker::publish_dependency_snapshot(
        &source,
        &snapshot,
        "lock-digest",
        PNPM_TOOLCHAIN_KEY,
    )
    .expect_err("an absolute shim must not publish");
    assert!(error.contains("must be relative"), "{error}");
}

/// The attestation has to see the difference between a shim and a regular
/// file holding that same path as text, or swapping one for the other would
/// leave `dependency_snapshot_is_ready` satisfied.
#[cfg(unix)]
#[test]
fn dependency_tree_digest_separates_a_shim_from_its_target_text() {
    let root = TempDir::new().expect("tempdir");
    let linked = root.path().join("linked/node_modules");
    fs::create_dir_all(linked.join("vite/bin")).expect("linked tree");
    fs::write(linked.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    std::os::unix::fs::symlink("../vite/bin/vite.js", linked.join("shim")).expect("shim");

    let plain = root.path().join("plain/node_modules");
    fs::create_dir_all(plain.join("vite/bin")).expect("plain tree");
    fs::write(plain.join("vite/bin/vite.js"), b"vite").expect("vite marker");
    fs::write(plain.join("shim"), b"../vite/bin/vite.js").expect("plain shim");

    assert_ne!(
        dependency_tree_digest(&linked).expect("linked digest"),
        dependency_tree_digest(&plain).expect("plain digest"),
    );
}

#[test]
fn dependency_sbom_spdx_ids_are_collision_free_for_punctuation_variants() {
    let root = TempDir::new().expect("tempdir");
    let node_modules = root.path().join("node_modules");
    fs::create_dir_all(&node_modules).expect("node_modules");
    write_fixture_package_manifest(&node_modules, "a.b", "1_0");
    write_fixture_package_manifest(&node_modules, "a-b", "1.0");
    let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
        local_apps::AppRuntimeProfile::ReactDom,
    )
    .expect("binding");
    let sbom =
        installed_dependency_sbom(&node_modules, &binding, &"d".repeat(64)).expect("sbom");
    let document: Value = serde_json::from_slice(&sbom).expect("sbom json");
    let packages = document
        .get("packages")
        .and_then(Value::as_array)
        .expect("packages");
    let ids = packages
        .iter()
        .filter_map(|package| package.get("SPDXID").and_then(Value::as_str))
        .filter(|id| id.starts_with("SPDXRef-Package-"))
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "distinct packages must not collide");
    let relationships = document
        .get("relationships")
        .and_then(Value::as_array)
        .expect("relationships");
    for id in ids {
        assert!(
            relationships.iter().any(|relationship| {
                relationship
                    .get("relatedSpdxElement")
                    .and_then(Value::as_str)
                    == Some(id)
            }),
            "relationship must target {id}"
        );
    }
}

/// Build a staged seed shaped the way `stage-local-app-runtime.py` emits
/// one: a `node_modules` tree with a real `vite/bin/vite.js`, the `.bin`
/// shims pnpm writes, and a `runtime-manifest.json` naming the lockfile the
/// tree was resolved from.
#[cfg(unix)]
fn create_bundled_seed(root: &Path, manifest_lock_digest: &str) -> PathBuf {
    let seed = root.join("bundle/local-app-runtime");
    let node_modules = seed.join("node_modules");
    fs::create_dir_all(node_modules.join("vite/bin")).expect("seed tree");
    fs::write(
        node_modules.join("vite/bin/vite.js"),
        b"#!/usr/bin/env node\n",
    )
    .expect("seed vite");
    fs::create_dir_all(node_modules.join(".bin")).expect("seed bin dir");
    std::os::unix::fs::symlink("../vite/bin/vite.js", node_modules.join(".bin/vite"))
        .expect("seed shim");
    fs::write(
        seed.join("runtime-manifest.json"),
        format!(
            "{{\"schema_version\":1,\"pnpm_lock_sha256\":\"{manifest_lock_digest}\",\"read_only\":true}}\n"
        ),
    )
    .expect("seed manifest");
    seed
}

/// The whole point of shipping the seed: a device that has never installed
/// anything gets a ready snapshot without a package manager, a Linux guest,
/// or a network round trip.
#[cfg(unix)]
#[test]
fn bundled_seed_becomes_the_snapshot_for_its_own_lockfile() {
    let root = TempDir::new().expect("tempdir");
    let seed = create_bundled_seed(root.path(), "lock-digest");
    let snapshot = root.path().join("cache/snapshot");

    let adopted = LocalAppsHostBroker::adopt_bundled_dependency_seed(
        &seed,
        "lock-digest",
        &snapshot,
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("adopt bundled seed");

    assert!(adopted, "a matching seed must be adopted");
    assert!(
        LocalAppsHostBroker::dependency_snapshot_is_ready(
            &snapshot,
            "lock-digest",
            PNPM_TOOLCHAIN_KEY
        )
        .expect("validate adopted snapshot"),
        "the adopted snapshot must satisfy the same readiness check a real install produces"
    );
    assert!(
        fs::symlink_metadata(snapshot.join("node_modules/.bin/vite"))
            .expect("adopted bin shim")
            .file_type()
            .is_symlink()
    );
}

/// Editing `package.json` re-resolves the lockfile, and the bundled tree no
/// longer describes those dependencies. Adopting it anyway would install
/// the wrong packages under an attestation claiming they were right.
#[cfg(unix)]
#[test]
fn bundled_seed_is_declined_when_the_app_lockfile_has_drifted() {
    let root = TempDir::new().expect("tempdir");
    let seed = create_bundled_seed(root.path(), "bundled-digest");
    let snapshot = root.path().join("cache/snapshot");

    let adopted = LocalAppsHostBroker::adopt_bundled_dependency_seed(
        &seed,
        "the-apps-own-digest",
        &snapshot,
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("evaluate bundled seed");

    assert!(!adopted, "a seed for a different lockfile must be declined");
    assert!(
        !snapshot.exists(),
        "declining must not leave a partial snapshot behind"
    );
}

/// Store builds ship no seed at all, so absence is an ordinary outcome and
/// must not fail the install that would otherwise proceed over the network.
#[test]
fn bundled_seed_absence_is_not_an_error() {
    let root = TempDir::new().expect("tempdir");
    let adopted = LocalAppsHostBroker::adopt_bundled_dependency_seed(
        &root.path().join("no-such-bundle"),
        "lock-digest",
        &root.path().join("cache/snapshot"),
        PNPM_TOOLCHAIN_KEY,
    )
    .expect("absent seed must not be an error");
    assert!(!adopted);
}

/// Asserted as one whole string, not by `contains` on the directives that
/// are interesting today: a CSP is only as strong as its most permissive
/// directive, so the thing worth locking is the WHOLE policy — a widened
/// `connect-src` or a dropped `object-src` is exactly what a substring
/// check cannot see.
#[test]
fn the_served_policy_allows_wasm_and_workers_and_nothing_else_new() {
    assert_eq!(
        LOCAL_APP_CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; media-src 'self' data: blob:; worker-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
    );
}

/// `instantiateStreaming` rejects anything but `application/wasm`, and this
/// server sends `nosniff`, so the default `application/octet-stream` would
/// fail the streaming path with a MIME error that reads nothing like the
/// CSP refusal it is not.
#[test]
fn wasm_is_served_with_the_type_streaming_instantiation_requires() {
    assert_eq!(
        content_type(Path::new("assets/physics-a1b2c3d4.wasm")),
        "application/wasm"
    );
}

#[test]
fn network_bridge_rejects_local_addresses() {
    assert!(!public_ip("127.0.0.1".parse().unwrap()));
    assert!(!public_ip("10.0.0.1".parse().unwrap()));
    assert!(!public_ip("100.64.0.1".parse().unwrap()));
    assert!(!public_ip("198.18.0.1".parse().unwrap()));
    assert!(!public_ip("224.0.0.1".parse().unwrap()));
    assert!(!public_ip("::1".parse().unwrap()));
    assert!(!public_ip("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!public_ip("64:ff9b::7f00:1".parse().unwrap()));
    assert!(!public_ip("ff02::1".parse().unwrap()));
    assert!(public_ip("1.1.1.1".parse().unwrap()));
    assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
}

#[test]
fn normalize_query_accepts_numeric_offset_and_sort_aliases() {
    let query = normalize_query(&json!({
        "collection": "items",
        "sort": {
            "kind": "field",
            "field_id": "score",
            "direction": "desc"
        },
        "offset": 7
    }))
    .unwrap();
    assert_eq!(query.collection, "items");
    assert_eq!(query.sort_key, Some(DataSortKey::Field("score".into())));
    assert_eq!(query.sort_direction, DataSortDirection::Descending);
    assert_eq!(query.offset, 7);

    let legacy = normalize_query(&json!({
        "collection": "items",
        "sortKey": "updatedAt",
        "sortDirection": "ascending"
    }))
    .unwrap();
    assert_eq!(legacy.sort_key, Some(DataSortKey::UpdatedAt));
    assert_eq!(legacy.sort_direction, DataSortDirection::Ascending);
}

#[test]
fn normalize_data_wire_accepts_canonical_shapes_and_rejects_guesses() {
    let query = normalize_query(&json!({
        "collection": "items",
        "filters": [{
            "fieldId": "score",
            "operator": "greater_than",
            "value": 10
        }]
    }))
    .expect("canonical query filter");
    assert_eq!(query.filters[0].field_id, "score");
    assert_eq!(
        query.filters[0].operator,
        local_apps::DataFilterOperator::GreaterThan
    );

    let mutations = normalize_mutations(&json!({
        "collection": "items",
        "operations": [{
            "kind": "upsert",
            "recordId": "best",
            "document": {"score": 42}
        }, {
            "kind": "delete",
            "recordId": "old",
            "expectedRevision": 2
        }]
    }))
    .expect("canonical tagged mutations");
    assert_eq!(mutations.len(), 2);
    assert!(matches!(mutations[0], DataMutation::Upsert { .. }));
    assert!(matches!(mutations[1], DataMutation::Delete { .. }));

    let guessed = normalize_mutations(&json!({
        "collection": "items",
        "operations": [{"action": "create", "record": {"score": 42}}]
    }))
    .expect_err("the broken generated shape must stay invalid");
    assert!(guessed.contains("missing field `kind`"), "{guessed}");
}

#[test]
fn normalize_query_rejects_cursor_and_invalid_page_bounds() {
    assert_eq!(
        normalize_query(&json!({"collection": "items", "cursor": "7"})).unwrap_err(),
        "query cursor is unsupported; use numeric offset"
    );
    assert_eq!(
        normalize_query(&json!({"collection": "items", "offset": -1})).unwrap_err(),
        "query offset must be a non-negative integer"
    );
    assert_eq!(
        normalize_query(&json!({"collection": "items", "limit": 101})).unwrap_err(),
        "query limit must be between 1 and 100"
    );
}

#[test]
fn normalize_ui_target_accepts_string_and_structured_object() {
    assert_eq!(
        normalize_ui_target(Some(&json!("submit-button"))).unwrap(),
        Some(AppUiTargetDto {
            element_id: Some("submit-button".into()),
            role: None,
            name: None,
        })
    );
    assert_eq!(
        normalize_ui_target(Some(&json!({
            "role": "button",
            "name": "Save"
        })))
        .unwrap(),
        Some(AppUiTargetDto {
            element_id: None,
            role: Some("button".into()),
            name: Some("Save".into()),
        })
    );
}

#[test]
fn capture_without_a_rect_keeps_the_whole_frame_behaviour() {
    let value = capture_ui_value(&json!({ "app_id": "demo" })).unwrap();
    assert_eq!(
        value, None,
        "no rect means the whole view, exactly as before"
    );
}

#[test]
fn capture_with_a_rect_serializes_it_into_the_opaque_value() {
    let value = capture_ui_value(&json!({
        "app_id": "demo",
        "rect": { "x": 10, "y": 20, "width": 120, "height": 80 }
    }))
    .unwrap()
    .expect("a rect must produce a value payload");
    let parsed: Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["rect"]["x"], 10);
    assert_eq!(parsed["rect"]["width"], 120);
}

#[test]
fn capture_rejects_a_malformed_or_non_positive_size_rect() {
    for bad in [
        json!({ "x": 0, "y": 0, "width": 0, "height": 10 }),
        json!({ "x": 0, "y": 0, "width": 10, "height": -5 }),
        // Not a number at all: the finiteness filter is what rejects this,
        // and it is the check that must survive the negative-origin one
        // being dropped below.
        json!({ "x": "0", "y": 0, "width": 10, "height": 10 }),
        json!({ "x": 0, "y": 0, "width": 10 }),
    ] {
        let out = capture_ui_value(&json!({ "app_id": "demo", "rect": bad }));
        assert!(
            out.is_err(),
            "invalid rect must be refused host-side: {bad}"
        );
    }
}

/// A NEGATIVE origin is the common case, not an error.
///
/// `getBoundingClientRect().top` is negative for anything scrolled above
/// the fold, and that is exactly what `inspect_ui`'s `elements[].rect`
/// hands the agent — so "inspect, take an element's rect, capture it"
/// produced a hard tool error on the most natural flow there is. Clamping
/// belongs to the client, which is the only side that knows the real
/// viewport (`intersection` on iOS, `coerceIn` in `cropSourceRect` on
/// Android); a host-side refusal made that client code unreachable.
#[test]
fn capture_accepts_a_negative_origin_and_leaves_clamping_to_the_client() {
    let value = capture_ui_value(&json!({
        "app_id": "demo",
        "rect": { "x": -50, "y": -40.5, "width": 200, "height": 150 }
    }))
    .expect("a scrolled-above-the-fold element rect must not be refused")
    .expect("a rect must produce a value payload");
    let parsed: Value = serde_json::from_str(&value).unwrap();
    assert_eq!(
        parsed["rect"]["x"], -50,
        "the origin must reach the client UNCHANGED so the client can clamp it"
    );
    assert_eq!(parsed["rect"]["y"], -40.5);
    assert_eq!(parsed["rect"]["width"], 200);
}

/// An explicit `"rect": null` means "not applicable", which is a WHOLE-VIEW
/// capture — not a malformed request.
///
/// `input.get("rect")` answers `Some(Value::Null)` for it, so the
/// absent-rect guard never fired and every field lookup below then failed,
/// turning a routine model habit into a hard tool error.
#[test]
fn capture_treats_an_explicit_null_rect_as_a_whole_view_capture() {
    let value = capture_ui_value(&json!({ "app_id": "demo", "rect": Value::Null }))
        .expect("an explicit null rect is not malformed");
    assert_eq!(
        value, None,
        "an explicit null must collapse to the same no-value payload as an absent rect"
    );
}

#[test]
fn create_staging_quality_gate_rejects_fast_canvas_profiles() {
    assert!(
        validate_create_stage_quality("fast", local_apps::AppRuntimeProfile::ReactDom).is_ok()
    );
    let error = validate_create_stage_quality("fast", local_apps::AppRuntimeProfile::Canvas2d)
        .expect_err("canvas create staging must reject fast quality");
    assert!(error.contains("balanced or thorough"));
    let error = validate_create_stage_quality("turbo", local_apps::AppRuntimeProfile::ReactDom)
        .expect_err("unknown quality must fail closed");
    assert!(error.contains("quality_level"));
}

#[tokio::test]
async fn response_limit_is_enforced_while_streaming() {
    let chunks = vec![
        Ok::<Vec<u8>, &'static str>(vec![0; 1024 * 1024]),
        Ok::<Vec<u8>, &'static str>(vec![0; 1024 * 1024]),
        Ok::<Vec<u8>, &'static str>(vec![1]),
    ];
    let error = read_limited_stream(
        stream::iter(chunks),
        MAX_NETWORK_RESPONSE_BYTES,
        "read network response",
        "network response exceeds 2 MiB",
    )
    .await
    .unwrap_err();
    assert_eq!(error, "network response exceeds 2 MiB");
}

#[tokio::test]
async fn concurrent_starts_reuse_one_static_runtime() {
    let runtime = MockMobileLinuxRuntime::new(Duration::from_millis(40));
    let (root, service, broker) = create_broker(true, Some(runtime.clone())).await;
    let app_id = create_app_fixture(&root, &service, "Concurrent").await;

    let (first, second, third) = tokio::join!(
        broker.manage_runtime_value(json!({"app_id": app_id, "action": "start"})),
        broker.manage_runtime_value(json!({"app_id": app_id, "action": "open"})),
        broker.manage_runtime_value(json!({"app_id": app_id, "action": "resume"})),
    );

    let urls: Vec<String> = [first, second, third]
        .into_iter()
        .map(|result| {
            result
                .expect("runtime start succeeds")
                .get("url")
                .and_then(Value::as_str)
                .expect("url present")
                .to_string()
        })
        .collect();
    assert!(urls.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(runtime.spawn_count(), 0);
    assert_eq!(broker.runtimes.lock().await.len(), 1);
    assert_eq!(
        service
            .runtime_record(&app_id)
            .await
            .expect("runtime record")
            .mode,
        Some(AppRuntimeMode::StaticExport)
    );
}

#[tokio::test]
async fn static_runtimes_are_not_counted_against_the_node_quota() {
    let (root, service, broker) = create_broker(false, None).await;
    let app_a = create_app_fixture(&root, &service, "Static A").await;
    let app_b = create_app_fixture(&root, &service, "Static B").await;

    broker
        .manage_runtime_value(json!({"app_id": app_a, "action": "start"}))
        .await
        .expect("first static runtime starts");
    broker
        .manage_runtime_value(json!({"app_id": app_b, "action": "start"}))
        .await
        .expect("second static runtime starts");

    assert_eq!(broker.runtimes.lock().await.len(), 2);
}

#[tokio::test]
async fn vite_apps_use_the_static_runtime_even_in_a_full_build() {
    let mobile_linux = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(true, Some(mobile_linux.clone())).await;
    let app_id = create_app_fixture(&root, &service, "Vite Static").await;

    broker
        .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
        .await
        .expect("Vite static runtime starts");

    assert_eq!(mobile_linux.spawn_count(), 0);
    assert_eq!(
        service
            .runtime_record(&app_id)
            .await
            .expect("runtime record")
            .mode,
        Some(AppRuntimeMode::StaticExport)
    );
}

#[tokio::test]
async fn legacy_next_runtime_mode_is_migrated_on_start() {
    let mobile_linux = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(true, Some(mobile_linux.clone())).await;
    let app_id = create_app_fixture(&root, &service, "Legacy Mode").await;
    service
        .set_runtime_mode(&app_id, AppRuntimeMode::NextProduction)
        .await
        .expect("persist legacy runtime mode");

    broker
        .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
        .await
        .expect("legacy runtime mode starts through static export");

    assert_eq!(mobile_linux.spawn_count(), 0);
    assert_eq!(
        service
            .runtime_record(&app_id)
            .await
            .expect("runtime record")
            .mode,
        Some(AppRuntimeMode::StaticExport)
    );
}

/// Lowest ephemeral floor across the shipped platforms: Linux/Android
/// `net.ipv4.ip_local_port_range` starts here, iOS/macOS
/// `net.inet.ip.portrange.first` at 49152.  At or above it the kernel can
/// hand the port to any other process's socket.
const LOWEST_SHIPPED_EPHEMERAL_FLOOR: u16 = 32_768;

/// An app's port is permanent (`bind_stable_loopback`), so it has to come
/// out of a window the kernel never allocates on its own.
///
/// This is also the fix for a `--workspace` flake: the full-runtime host
/// reserves the port, RELEASES it and lets the child bind it, so while the
/// window overlapped the ephemeral range the OS could hand that port to
/// somebody else in between — the mock runtime then failed with
/// "bind test runtime loopback: Address already in use (os error 48)" and
/// took a test with it.  Reproduced 1 run in 20 with the old window by
/// churning ephemeral ports alongside the suite; 0 in 60 with this one.
#[tokio::test]
async fn derived_app_ports_stay_below_every_shipped_platform_ephemeral_floor() {
    let (_registry_root, registry) = empty_registry().await;
    // Real minted ids (eight lowercase hex, `ids::generate_app_id`).  Eight
    // of these ten drew a port at or above the Android floor from the old
    // 30000..50000 window.
    for app_id in [
        "0f3a91cc", "a71b04de", "5c92f8b1", "deadbeef", "00000000", "ffffffff", "9a1c7e40",
        "3b6d20af", "7e0091cd", "c4f5a3b2",
    ] {
        let (listener, port, _lease) =
            bind_stable_loopback(app_id, None, &[], &test_leases(), &registry)
                .await
                .expect("derive a port");
        assert!(
            port < LOWEST_SHIPPED_EPHEMERAL_FLOOR,
            "app {app_id} was pinned to {port}, which the kernel can hand out ephemerally"
        );
        assert!(
            (APP_PORT_WINDOW_FIRST..APP_PORT_WINDOW_FIRST + APP_PORT_WINDOW_LEN)
                .contains(&port),
            "app {app_id} port {port} is outside the derived window"
        );
        assert_eq!(listener.local_addr().expect("local addr").port(), port);
    }
}

/// A squatted port is the one start failure the app cannot work around: the
/// port is permanent, so the error has to NAME it or the user is told
/// nothing they can act on.  Three of `start_reserved_runtime`'s bail-outs
/// (this one, the missing runtime mount, the missing static build) run
/// BEFORE the record leaves `stopped`, where `stopped -> failed` is not a
/// transition the table has — so letting that bookkeeping rejection
/// propagate replaced the real detail with "invalid runtime transition
/// stopped -> failed for app <id>".
#[tokio::test]
async fn a_squatted_app_port_reports_the_squat_not_a_bookkeeping_rejection() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(true, Some(runtime)).await;
    let app_id = create_app_fixture(&root, &service, "Squatted").await;
    let squatter = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind a squatter");
    let pinned = squatter.local_addr().expect("local addr").port();
    // Assign the port the way a first start does, then park the app
    // stopped with somebody else still sitting on it.
    for (state, port) in [
        (AppRuntimeState::Starting, Some(pinned)),
        (AppRuntimeState::Running, None),
        (AppRuntimeState::Stopping, None),
        (AppRuntimeState::Stopped, None),
    ] {
        service
            .update_runtime_record(&app_id, state, port, None, None)
            .await
            .expect("seed the app's permanent port");
    }

    let error = broker
        .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
        .await
        .expect_err("a squatted permanent port fails the start");
    assert!(
        error.contains(&format!("stable app port {pinned} is unavailable")),
        "{error}"
    );
    drop(squatter);
}

/// Two well-formed ids can derive the SAME window slot — `6b4cb242` and
/// `c3baea9e` both land on 30809 — and a pin outlives the runtime that made
/// it.  With the first app merely STOPPED its permanent port probes free,
/// so a bind-only check pins it to the second app as well; from then on
/// neither can start while the other runs (neither port can be moved) and
/// on Android both apps share one WebView origin's `localStorage` /
/// `IndexedDB`.
#[tokio::test]
async fn a_derived_port_skips_a_slot_a_stopped_sibling_app_has_pinned() {
    let (first_id, second_id) = ("6b4cb242", "c3baea9e");
    assert_eq!(
        derived_window_slot(first_id),
        derived_window_slot(second_id),
        "the fixture pair no longer collides, so this probe would pass vacuously"
    );

    let (_registry_root, registry) = empty_registry().await;
    let leases = test_leases();
    let (listener, first_port, first_lease) =
        bind_stable_loopback(first_id, None, &[], &leases, &registry)
            .await
            .expect("the first app derives a port");
    // The first app is stopped: nothing holds the port, only the pin
    // survives — precisely the state a bind probe cannot distinguish.  The
    // LEASE goes too, and the shared registry is deliberate: only the pin
    // may be why the second app moves off the slot.
    drop(listener);
    drop(first_lease);

    let (second_listener, second_port, _second_lease) = bind_stable_loopback(
        second_id,
        None,
        &[(first_id.to_string(), first_port)],
        &leases,
        &registry,
    )
    .await
    .expect("the second app derives a port");
    assert_ne!(
        second_port, first_port,
        "app {second_id} was pinned to {second_port}, which app {first_id} owns forever"
    );
    assert_eq!(
        second_listener.local_addr().expect("local addr").port(),
        second_port
    );
}

/// A pair that has ALREADY collided cannot be repaired at bind time — both
/// records are permanent — so the only thing left is to say WHICH app is
/// holding the port.  The negative half carries equal weight: a foreign
/// squatter must not be reported as a sibling app, and the un-actionable
/// "recreate one of them" advice must not appear when nothing collided.
#[tokio::test]
async fn a_pinned_port_held_by_a_sibling_app_names_the_sibling() {
    let holder = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = holder.local_addr().expect("local addr").port();

    let (_registry_root, registry) = empty_registry().await;
    let named = bind_stable_loopback(
        "starter",
        Some(port),
        &[("sibling-app".into(), port)],
        &test_leases(),
        &registry,
    )
    .await
    .expect_err("a held pinned port fails the start");
    assert!(
        named.contains(&format!("stable app port {port} is unavailable")),
        "{named}"
    );
    assert!(named.contains("sibling-app"), "{named}");
    assert!(named.contains("can never be reassigned"), "{named}");

    let foreign = bind_stable_loopback(
        "starter",
        Some(port),
        &[("other-app".into(), port.wrapping_add(1))],
        &test_leases(),
        &registry,
    )
    .await
    .expect_err("a squatted pinned port still fails");
    assert!(
        foreign.contains(&format!("stable app port {port} is unavailable")),
        "{foreign}"
    );
    assert!(!foreign.contains("other-app"), "{foreign}");
    assert!(!foreign.contains("can never be reassigned"), "{foreign}");
    drop(holder);
}

/// The production half of the same defect: `start_reserved_runtime` has to
/// COLLECT the sibling pins, or the exclusion above is never reached by a
/// real start.  The sibling is parked stopped, so its permanent port probes
/// free at bind time.
///
/// The starter is SEEDED with a fixed id rather than created with a minted
/// one: the contested port is derived from the id, so a minted id made this
/// test bind a different port on every run (see [`seed_app_fixture`]).  The
/// id is a well-formed minted-shape id whose slot no other test in this
/// file derives, so the two ports this test touches — 23356 and whatever
/// the skip lands on next — collide with nothing else in the binary.  The
/// sibling keeps its minted id: it never starts, and its pin is written
/// explicitly, so nothing about it is derived.
#[tokio::test]
async fn a_first_start_skips_a_port_a_stopped_sibling_app_already_owns() {
    const STARTER_ID: &str = "43b026c4";
    const STARTER_FIRST_PORT: u16 = 23_356;

    let root = TempDir::new().expect("tempdir");
    seed_app_fixture(&root, STARTER_ID, "Starter");
    let (root, service, broker) = create_broker_over(root, false, None).await;
    let starter = STARTER_ID.to_string();
    let sibling = create_app_fixture(&root, &service, "Sibling").await;
    let contested = APP_PORT_WINDOW_FIRST + derived_window_slot(&starter);
    // Computed from the production derivation, then held against the
    // documented value: if the derivation moves, this says so instead of
    // quietly exercising some other port.
    assert_eq!(
        contested, STARTER_FIRST_PORT,
        "app {starter} no longer derives the documented port; \
         re-pick the fixture id and update the doc comment"
    );
    // Nothing may HOLD the contested port, or the assertion below would
    // pass for the wrong reason.
    drop(
        TcpListener::bind(("127.0.0.1", contested))
            .await
            .expect("the contested port is free in this environment"),
    );
    for (state, port) in [
        (AppRuntimeState::Starting, Some(contested)),
        (AppRuntimeState::Running, None),
        (AppRuntimeState::Stopping, None),
        (AppRuntimeState::Stopped, None),
    ] {
        service
            .update_runtime_record(&sibling, state, port, None, None)
            .await
            .expect("pin the contested port on the sibling, then park it stopped");
    }

    broker
        .manage_runtime_value(json!({"app_id": starter, "action": "start"}))
        .await
        .expect("the start succeeds on a port the sibling does not own");

    let pinned = service
        .runtime_record(&starter)
        .await
        .expect("runtime record")
        .port
        .expect("the start pinned a port");
    assert_ne!(
        pinned, contested,
        "app {starter} was pinned to {contested}, which app {sibling} owns forever"
    );
    assert!(
        (APP_PORT_WINDOW_FIRST..APP_PORT_WINDOW_FIRST + APP_PORT_WINDOW_LEN).contains(&pinned),
        "app {starter} port {pinned} is outside the derived window"
    );
}

/// The stretch a pin snapshot cannot describe.  A chosen port only reaches
/// the records ~11 ms later (see [`PortLeases`]), and on the full runtime
/// the probe listener is released BEFORE then on purpose — so for that
/// stretch the port is in no snapshot, is held by nothing, and binds
/// cleanly for the next app that scans to it.  Both apps then own it
/// forever and neither can run while the other does.
///
/// Driven through the reservation API instead of by racing two real
/// starts: a race reproduces the collision only sometimes, so it would pass
/// for the wrong reason on most runs and rot without anyone noticing.  The
/// port here is leased and deliberately NOT bound — precisely the state the
/// production window leaves it in — so an allocator that does not consult
/// the leases takes it on every run, not on a lucky one.
#[tokio::test]
async fn an_unpersisted_lease_moves_the_next_allocation_off_that_port() {
    const APP_ID: &str = "1a2b3c4d";
    const FIRST_CHOICE: u16 = 29_728;
    let first_choice = APP_PORT_WINDOW_FIRST + derived_window_slot(APP_ID);
    // Computed from the production derivation, then held against the
    // documented value, so a moved derivation says so instead of quietly
    // exercising some other port.
    assert_eq!(
        first_choice, FIRST_CHOICE,
        "app {APP_ID} no longer derives the documented port; \
         re-pick the fixture id and update the doc comment"
    );
    let (_registry_root, registry) = empty_registry().await;
    let leases = test_leases();

    // A sibling start that has CHOSEN this port and not yet persisted it.
    let concurrent = PortLease::take(&leases, "sibling-app", first_choice)
        .expect("the port is unleased before the sibling takes it");

    let (listener, port, lease) = bind_stable_loopback(APP_ID, None, &[], &leases, &registry)
        .await
        .expect("the app still gets a port");
    assert_ne!(
        port, first_choice,
        "app {APP_ID} was pinned to {first_choice}, which a sibling had already chosen"
    );
    assert!(
        (APP_PORT_WINDOW_FIRST..APP_PORT_WINDOW_FIRST + APP_PORT_WINDOW_LEN).contains(&port),
        "app {APP_ID} port {port} is outside the derived window"
    );
    // Choice and reservation are ONE step: the port it did take is already
    // spoken for, before any record has heard of it.
    assert!(
        PortLease::take(&leases, "third-app", port).is_none(),
        "port {port} was chosen but left free for a concurrent allocator"
    );

    // Dropped without a commit — every bail-out between the choice and the
    // persist ends here — so the port returns to the pool instead of being
    // lost for the life of the process.
    drop(listener);
    drop(lease);
    let reclaimed = PortLease::take(&leases, "third-app", port)
        .expect("a lease dropped without a commit releases its port");
    drop(reclaimed);
    // Committing releases it too; what `commit` buys is the order, not a
    // different effect.  The pin has taken over by then.
    PortLease::take(&leases, "fourth-app", port)
        .expect("free again")
        .commit();
    let after_commit = PortLease::take(&leases, "fifth-app", port);
    assert!(
        after_commit.is_some(),
        "port {port} stayed leased after its start committed"
    );
    drop(after_commit);

    // With the sibling gone the derivation is deterministic again: an app
    // that moved port on restart would orphan its own WebView storage.
    drop(concurrent);
    let (again, port_again, _lease) =
        bind_stable_loopback(APP_ID, None, &[], &leases, &registry)
            .await
            .expect("the app derives its port");
    assert_eq!(
        port_again, first_choice,
        "the first choice must stay deterministic while the slot is free"
    );
    drop(again);
}

/// The hand-off a lease and a gate BOTH miss, and the re-read that catches
/// it.
///
/// An allocator samples "persisted pins UNION live leases" in two steps:
/// the pins first (in the caller, before `bind_stable_loopback`), the lease
/// second.  A sibling that persists its pin and then commits its lease in
/// between lands in neither half — the pin read was too early, the lease
/// sample too late — and the allocation gate does not help, because the
/// sibling's persist and commit both run after it has left that gate.  Both
/// apps then pin the same port and neither can run while the other does.
///
/// SEEDED, not raced, and the seed is exactly the post-hand-off state: the
/// sibling's pin is in the records (persisted) and NOT in the leases
/// (committed), while the snapshot handed to the allocator is the one that
/// was read before either happened — an empty slice.  Every ordering
/// question is therefore already settled when the call starts, so the
/// allocator either consults the records again after leasing its candidate
/// or takes the sibling's port on every single run.
///
/// The control arm is the other half of the point.  Nothing binds the
/// contested port here — the sibling is merely pinned — so an allocator
/// that moved off it because some unrelated process happened to hold it
/// would look identical to one that read the records.  Proving the port is
/// free on THIS machine first is what tells those two apart; if it is not
/// free the control fails loudly instead of handing the real assertion a
/// free pass.
#[tokio::test]
async fn a_pin_that_lands_after_the_snapshot_is_caught_before_the_choice_sticks() {
    const APP_ID: &str = "8c6d31fa";
    const FIRST_CHOICE: u16 = 27_262;
    let first_choice = APP_PORT_WINDOW_FIRST + derived_window_slot(APP_ID);
    let next_choice =
        APP_PORT_WINDOW_FIRST + (derived_window_slot(APP_ID) + 1) % APP_PORT_WINDOW_LEN;
    assert_eq!(
        first_choice, FIRST_CHOICE,
        "app {APP_ID} no longer derives the documented port; \
         re-pick the fixture id and update the doc comment"
    );

    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sibling = create_app_fixture(&root, &service, "Hand-off").await;
    let leases = test_leases();

    // CONTROL: with the records still empty of pins, the derivation lands
    // on the contested port and the port is genuinely available here.  A
    // failure at this line means the fixture port is occupied by something
    // outside this test, and the assertion below would have passed without
    // proving anything.
    let (control, control_port, control_lease) =
        bind_stable_loopback(APP_ID, None, &[], &leases, &service)
            .await
            .expect("the app derives its port with nothing pinned");
    assert_eq!(
        control_port, first_choice,
        "port {first_choice} is not free on this machine (or the derivation moved), \
         so the contested arm below cannot distinguish the re-read from a busy port"
    );
    drop(control);
    drop(control_lease);

    // The hand-off, in the order production performs it: the pin becomes
    // durable FIRST, and only then is the lease released.  From here the
    // port is in the records and in no lease.
    let sibling_lease = PortLease::take(&leases, &sibling, first_choice)
        .expect("the sibling leases the port it is about to persist");
    service
        .update_runtime_record(
            &sibling,
            AppRuntimeState::Starting,
            Some(first_choice),
            None,
            None,
        )
        .await
        .expect("the sibling persists its pin");
    sibling_lease.commit();
    assert!(
        lock_port_leases(&leases).is_empty(),
        "the seed must leave the port in the records ONLY; a lease still held \
         would let the take alone move the allocation off it"
    );

    // The snapshot is the one the caller read BEFORE that hand-off, so the
    // pre-check cannot exclude the port and the lease is taken on it.  Only
    // a read of the records after that take can reject it.
    let (listener, port, lease) = bind_stable_loopback(APP_ID, None, &[], &leases, &service)
        .await
        .expect("the app still gets a port");
    assert_ne!(
        port, first_choice,
        "app {APP_ID} was pinned to {first_choice}, which app {sibling} persisted \
         after the snapshot was read; both apps now own it forever"
    );
    assert_eq!(
        port, next_choice,
        "the scan skipped more than the contested candidate, so something other \
         than the sibling's pin moved it"
    );
    // The rejected candidate went back to the pool: a lease dropped only on
    // the success path would strand every port the re-read rejects.
    assert!(
        lock_port_leases(&leases).get(&first_choice).is_none(),
        "port {first_choice} stayed leased after the re-read rejected it"
    );
    drop(listener);
    drop(lease);
}

/// The production half of the same defect: `start_reserved_runtime` has to
/// hand the BROKER's registry to the allocator, or the exclusion above is
/// never reached by a real start.  Deterministic — the sibling's lease is
/// planted before the start rather than raced against it.
///
/// Seeded with a fixed id for the reason [`seed_app_fixture`] documents:
/// the contested port is derived from the id, so a minted one would probe
/// a different port on every run.  This id's slot is derived by no other
/// test in the binary.
///
/// The control arm exists because the contested port here is held by
/// NOTHING — the sibling has only leased it — so a start that moved off it
/// because an unrelated process on this machine happened to be sitting on
/// that port produces exactly the same green as a start that consulted the
/// broker's leases.  Deriving the port through the production allocator
/// first, against a registry with no lease planted, is what separates them:
/// if the port is not free the control fails and says so, instead of the
/// real assertion passing for the environment's reason.
#[tokio::test]
async fn a_first_start_skips_a_port_a_concurrent_start_has_leased() {
    const STARTER_ID: &str = "b17cc0de";
    const STARTER_FIRST_PORT: u16 = 26_141;

    let root = TempDir::new().expect("tempdir");
    seed_app_fixture(&root, STARTER_ID, "Leased");
    let (_root, service, broker) = create_broker_over(root, false, None).await;
    let contested = APP_PORT_WINDOW_FIRST + derived_window_slot(STARTER_ID);
    let next_slot =
        APP_PORT_WINDOW_FIRST + (derived_window_slot(STARTER_ID) + 1) % APP_PORT_WINDOW_LEN;
    assert_eq!(
        contested, STARTER_FIRST_PORT,
        "app {STARTER_ID} no longer derives the documented port; \
         re-pick the fixture id and update the doc comment"
    );

    // CONTROL, before anything is planted: the production allocator lands
    // on the contested port, so it is free on this machine and the skip
    // below can only be the lease's doing.  Its own lease registry, so
    // nothing survives into the start; the listener is released for the
    // same reason.  Releasing it leaves the kernel's brief rebind refusal
    // on that port, which is harmless here — the start rejects the
    // candidate at `PortLease::take`, before any probe touches it.
    let (control, control_port, control_lease) =
        bind_stable_loopback(STARTER_ID, None, &[], &test_leases(), &service)
            .await
            .expect("the starter derives its port with nothing leased");
    assert_eq!(
        control_port, contested,
        "port {contested} is not free on this machine (or the derivation moved), \
         so the assertion below cannot distinguish the lease from a busy port"
    );
    drop(control);
    drop(control_lease);

    // Nothing binds it and no record mentions it — the only thing that can
    // move the start off this port is the lease.
    let concurrent = PortLease::take(&broker.port_leases, "sibling-app", contested)
        .expect("the port is unleased before the sibling takes it");

    broker
        .manage_runtime_value(json!({"app_id": STARTER_ID, "action": "start"}))
        .await
        .expect("the start succeeds on a port no concurrent start holds");

    let pinned = service
        .runtime_record(STARTER_ID)
        .await
        .expect("runtime record")
        .port
        .expect("the start pinned a port");
    assert_ne!(
        pinned, contested,
        "app {STARTER_ID} was pinned to {contested}, which a concurrent start had chosen"
    );
    assert_eq!(
        pinned, next_slot,
        "the start skipped more than the leased candidate, so something other \
         than the broker's leases moved it"
    );
    drop(concurrent);
    // The start committed its own lease once the pin was durable: a port
    // still held after that is one the profile never gets back.
    assert!(
        lock_port_leases(&broker.port_leases).is_empty(),
        "a finished start left a port leased"
    );
}

/// One hole a lease alone cannot cover: the pin snapshot is read BEFORE a
/// lease is taken, so without a gate two allocators can sit between those
/// same two steps at once, read the same pins, and choose the same port.
///
/// WHAT THIS PINS.  With the gate held, a start reaches neither the choice
/// nor the pin it implies: no lease appears in the broker's registry and no
/// port reaches the record.  That rules out a gate taken after the LEASE or
/// after the PERSIST, which the older "the start has not finished"
/// assertion alone could not, since any gate anywhere on the start path
/// satisfies it.
///
/// It does NOT rule out a gate taken after the SNAPSHOT: move the lock to
/// just past `sibling_pinned_ports` and all three assertions still pass,
/// because the start blocks before leasing either way.  That lower edge is
/// as unobservable from outside as the upper edge below, and for the same
/// reason.  Saying it was pinned was the tenth false comment here.
///
/// WHAT IT CANNOT PIN — the gate's UPPER edge, that it is RELEASED before
/// `update_runtime_record`.  That release is invisible from outside: the
/// only external handle on it is acquiring the mutex, and the instant to
/// try is between a start's choice and its persist, which is exactly the
/// interval no observer can name without the start path telling it.  Take
/// the gate too early and the start is still blocked on it; too late and it
/// is already released either way.  Pinning it needs the start path
/// instrumented — a barrier the test releases after the choice — and that
/// is production machinery existing only for a test, so it is not here.
/// The upper edge is held by `port_allocation`'s doc comment and by review,
/// not by this test, and nothing below should be read as covering it.
///
/// The control start is what keeps the gated assertion honest: it measures
/// what an UNGATED start costs on this machine and sizes the wait from
/// that, so "not finished yet" cannot quietly degrade into "not finished
/// yet because everything here is slow".
#[tokio::test]
async fn port_allocation_is_serialized_across_one_brokers_starts() {
    let (root, service, broker) = create_broker(false, None).await;
    let control_id = create_app_fixture(&root, &service, "Ungated").await;
    let app_id = create_app_fixture(&root, &service, "Gated").await;

    let control_began = tokio::time::Instant::now();
    broker
        .manage_runtime_value(json!({"app_id": control_id, "action": "start"}))
        .await
        .expect("an ungated start succeeds");
    let ungated = control_began.elapsed();

    let gate = broker.port_allocation.lock().await;
    let start = tokio::spawn({
        let broker = broker.clone();
        let app_id = app_id.clone();
        async move {
            broker
                .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
                .await
        }
    });
    // Twenty times what a start just cost here, floored at the historical
    // 300 ms and capped so a pathological control cannot hang the suite.
    let budget = (ungated * 20).clamp(Duration::from_millis(300), Duration::from_secs(5));
    sleep(budget).await;
    assert!(
        !start.is_finished(),
        "a start finished within {budget:?} while the allocation gate was held \
         (an ungated start took {ungated:?} here), so it never took the gate"
    );
    assert!(
        lock_port_leases(&broker.port_leases).is_empty(),
        "a start leased a port while the allocation gate was held, so the gate \
         is taken after the choice"
    );
    assert_eq!(
        service
            .runtime_record(&app_id)
            .await
            .expect("runtime record")
            .port,
        None,
        "a start pinned a port while the allocation gate was held"
    );

    drop(gate);
    timeout(Duration::from_secs(10), start)
        .await
        .expect("the start is released by the gate")
        .expect("join the start")
        .expect("the start succeeds once the gate is free");
}

/// The invariant `bind_stable_loopback`'s "no fallback" reasoning rests on,
/// made executable: it lives in `AppState::set_runtime` (another crate), so
/// nothing here would notice it being lifted.  If this ever goes red, a
/// port fallback becomes possible AND that doc comment is wrong.
#[tokio::test]
async fn the_pinned_app_port_can_never_be_reassigned() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let app_id = create_app_fixture(&root, &service, "Pinned").await;

    service
        .update_runtime_record(&app_id, AppRuntimeState::Starting, Some(20_123), None, None)
        .await
        .expect("the first start assigns the port");
    let same = service
        .update_runtime_record(&app_id, AppRuntimeState::Running, Some(20_123), None, None)
        .await
        .expect("re-recording the same port is what every later start does");
    assert_eq!(same.port, Some(20_123));

    let error = service
        .update_runtime_record(&app_id, AppRuntimeState::Running, Some(20_124), None, None)
        .await
        .expect_err("a moved port is refused, so a fallback would fail here instead");
    assert!(
        error.to_string().contains("can never be reassigned"),
        "{error}"
    );
    let record = service
        .runtime_record(&app_id)
        .await
        .expect("runtime record");
    assert_eq!(record.port, Some(20_123));
}

#[tokio::test]
async fn explicit_stop_remains_stopped() {
    let runtime = MockMobileLinuxRuntime::new(Duration::ZERO);
    let (root, service, broker) = create_broker(true, Some(runtime)).await;
    let app_id = create_app_fixture(&root, &service, "Stop").await;

    broker
        .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
        .await
        .expect("runtime starts");
    broker
        .manage_runtime_value(json!({"app_id": app_id, "action": "stop"}))
        .await
        .expect("runtime stops");
    sleep(STATIC_ACCEPT_RETRY * 2).await;

    let runtime_record = service
        .runtime_record(&app_id)
        .await
        .expect("runtime record");
    assert_eq!(runtime_record.state, AppRuntimeState::Stopped);
    assert!(!broker.runtimes.lock().await.contains_key(&app_id));
}

#[tokio::test]
async fn abandoned_runtime_reservation_is_released_for_the_next_start() {
    let (root, service, broker) = create_broker(false, None).await;
    let app_id = create_app_fixture(&root, &service, "Abandoned").await;
    // Keep the static-only start deterministically inside its reservation.
    // The old version relied on the removed full-runtime spawn delay, so
    // the start could already be Running by the time the test aborted it.
    let allocation = broker.port_allocation.lock().await;

    let start = tokio::spawn({
        let broker = broker.clone();
        let app_id = app_id.clone();
        async move {
            broker
                .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
                .await
        }
    });
    wait_until("runtime reservation", Duration::from_secs(3), || {
        let broker = broker.clone();
        let app_id = app_id.clone();
        async move { broker.runtimes.lock().await.contains_key(&app_id) }
    })
    .await;
    start.abort();
    let _ = start.await;

    wait_until("reservation released", Duration::from_secs(3), || {
        let broker = broker.clone();
        let app_id = app_id.clone();
        async move { !broker.runtimes.lock().await.contains_key(&app_id) }
    })
    .await;
    drop(allocation);

    let restarted = timeout(
        Duration::from_secs(10),
        broker.manage_runtime_value(json!({"app_id": app_id, "action": "start"})),
    )
    .await
    .expect("a later start is not blocked by the abandoned reservation")
    .expect("restart succeeds");
    assert_eq!(restarted["state"], "running");
}

#[tokio::test]
async fn stop_during_start_keeps_the_reservation_intact() {
    let runtime = MockMobileLinuxRuntime::new(Duration::from_millis(40));
    let (root, service, broker) = create_broker(true, Some(runtime)).await;
    let app_id = create_app_fixture(&root, &service, "Race").await;

    let (start, stop) = tokio::join!(
        broker.manage_runtime_value(json!({"app_id": app_id, "action": "start"})),
        broker.manage_runtime_value(json!({"app_id": app_id, "action": "stop"})),
    );

    assert_eq!(
        start.expect("the start still completes")["state"],
        "running"
    );
    let stop_error = stop.expect_err("a stop during a start is refused");
    assert!(stop_error.contains("still starting"), "{stop_error}");
    assert_eq!(broker.runtimes.lock().await.len(), 1);
}

/// Declare `capability` in the app's persisted manifest, the way an
/// `update_manifest` call reaches it.
fn declare_capability(root: &TempDir, app_id: &str, capability: AppCapability) {
    let layout = AppLayout::new(root.path().to_path_buf(), app_id).expect("layout");
    let mut manifest = load_manifest(&layout).expect("fixture manifest");
    manifest.capabilities.push(capability);
    local_apps::save_manifest(&layout, &manifest).expect("declare capability");
}

/// Build the host facts a native client reports for one device.
fn host_environment(
    host_os: platform_api::MobileHostOs,
    device_class: platform_api::MobileDeviceClass,
) -> platform_api::MobileHostEnvironment {
    platform_api::MobileHostEnvironment::new(
        host_os,
        Some("19.0".into()),
        device_class,
        platform_api::MobileExecutionTarget::PhysicalDevice,
        platform_api::MobileLaunchMode::Interactive,
    )
}

/// The reported iPhone failure: the agent was asked to declare the native
/// device context, but the only device facts it can see are the runtime
/// reminder's `Host OS: iOS` plus `Device class: phone` — and
/// `(ios, phone)` is exactly the pair the manifest validator rejects. The
/// host owns these facts, so it stamps them itself and the agent never
/// supplies them.
#[tokio::test]
async fn the_host_stamps_the_iphone_device_context_the_agent_cannot_name() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    assert!(broker
        .attach_host_environment(host_environment(
            platform_api::MobileHostOs::Ios,
            platform_api::MobileDeviceClass::Phone,
        ))
        .is_ok());
    let app_id = create_app_fixture(&root, &service, "Device").await;

    let result = broker
        .update_manifest(json!({"app_id": app_id}))
        .await
        .expect("the host derives the device context without the agent");
    assert_eq!(result["device_context"]["os"], "ios");
    assert_eq!(result["device_context"]["formFactor"], "iphone");

    let layout = AppLayout::new(root.path().to_path_buf(), app_id).expect("layout");
    let recorded = load_manifest(&layout)
        .expect("manifest")
        .device_context
        .expect("the host records the confirmed native target");
    assert_eq!(recorded.os, "ios");
    assert_eq!(recorded.form_factor, "iphone");
}

/// The same derivation on the other platform, where the reminder's
/// vocabulary happens to match the manifest's.
#[tokio::test]
async fn the_host_stamps_the_android_tablet_device_context() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    assert!(broker
        .attach_host_environment(host_environment(
            platform_api::MobileHostOs::Android,
            platform_api::MobileDeviceClass::Tablet,
        ))
        .is_ok());
    let app_id = create_app_fixture(&root, &service, "Tablet").await;

    let result = broker
        .update_manifest(json!({"app_id": app_id}))
        .await
        .expect("the host derives the device context without the agent");
    assert_eq!(result["device_context"]["os"], "android");
    assert_eq!(result["device_context"]["formFactor"], "tablet");
}

/// An unclassified host records NO context rather than a guessed one:
/// `DeviceContext` is documented as absent-means-unknown, and every
/// os/form-factor pair naming a real platform would be a fabrication.
#[tokio::test]
async fn an_unclassified_host_records_no_device_context() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    assert!(broker
        .attach_host_environment(host_environment(
            platform_api::MobileHostOs::Ios,
            platform_api::MobileDeviceClass::Unknown,
        ))
        .is_ok());
    let app_id = create_app_fixture(&root, &service, "Unclassified").await;

    let result = broker
        .update_manifest(json!({"app_id": app_id}))
        .await
        .expect("an unclassified host still updates the manifest");
    assert!(result["device_context"].is_null(), "{result}");
}

/// A model-authored `device_context` is no longer part of the contract:
/// the tool schema rejects the key outright rather than letting a guessed
/// pair reach the validator.
#[tokio::test]
async fn an_agent_supplied_device_context_never_overrides_the_host() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    assert!(broker
        .attach_host_environment(host_environment(
            platform_api::MobileHostOs::Ios,
            platform_api::MobileDeviceClass::Tablet,
        ))
        .is_ok());
    let app_id = create_app_fixture(&root, &service, "Ignored").await;

    let result = broker
        .update_manifest(json!({
            "app_id": app_id,
            "device_context": {"os": "ios", "formFactor": "phone"},
        }))
        .await
        .expect("a stray key never fails the call");
    assert_eq!(result["device_context"]["formFactor"], "ipad");
}

/// The surface is fixed at creation, and `update_manifest` is the one
/// mutation path an agent can reach after that. Its refusal was the only
/// member of the family without a test — `create` (`local_apps_mcp.rs`:
/// `create_rejects_runtime_profile_and_surface_overrides`), `scaffold`
/// (`scaffold_rejects_an_empty_brief_and_an_unknown_surface`) and the
/// shell-mode create (`host.rs`:
/// `create_app_in_shell_mode_rejects_a_surface`) are all covered — so
/// deleting the four-line `if` was a silent green.
///
/// It must REFUSE, not ignore: `manifest.surface` is carried through the
/// load-modify-save untouched, so a dropped refusal returns `ok` to an
/// agent that then believes it converted the app, while the workspace on
/// disk still holds the other scaffold's source.
#[tokio::test]
async fn update_manifest_rejects_a_caller_supplied_surface() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        MockSink::arc(),
        None,
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    let app_id = create_app_fixture(&root, &service, "Fixed").await;
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.clone()).expect("layout");
    let before = load_manifest(&layout).expect("manifest").surface;

    let error = broker
        .update_manifest(json!({
            "app_id": app_id,
            "surface": "canvas",
        }))
        .await
        .expect_err("a caller-supplied surface must be refused, not silently ignored");
    assert!(
        error.contains("an app's surface is fixed when the app is created"),
        "got {error}"
    );

    let after = load_manifest(&layout).expect("manifest").surface;
    assert_eq!(
        before, after,
        "the refusal must happen before the manifest is saved"
    );
}

#[tokio::test]
async fn an_undeclared_capability_is_refused_without_prompting() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker =
        LocalAppsHostBroker::new(root.path().to_path_buf(), sink.clone(), None, false, None);
    assert!(broker.attach_service(service.clone()).is_ok());
    let app_id = create_app_fixture(&root, &service, "Undeclared").await;

    let failure = timeout(
        Duration::from_secs(2),
        broker.authorize_declared_capability(
            &app_id,
            AppCapability::Camera,
            AppCapabilityKindDto::Camera,
            "test reason",
        ),
    )
    .await
    .expect("the refusal must not wait on any approval")
    .expect_err("an undeclared capability must be refused");
    assert_eq!(failure.code, Some("capability_not_declared"));
    assert!(
        sink.is_empty().await,
        "an undeclared capability must never raise a user prompt"
    );
}

#[tokio::test]
async fn a_declared_capability_with_a_persisted_grant_passes_silently() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker =
        LocalAppsHostBroker::new(root.path().to_path_buf(), sink.clone(), None, false, None);
    assert!(broker.attach_service(service.clone()).is_ok());
    let app_id = create_app_fixture(&root, &service, "Granted").await;
    declare_capability(&root, &app_id, AppCapability::Microphone);
    let layout = AppLayout::new(root.path().to_path_buf(), app_id.clone()).expect("layout");
    let mut permissions = load_permissions(&layout).expect("permissions");
    permissions.grant(AppCapability::Microphone);
    save_permissions(&layout, &permissions).expect("persist grant");

    broker
        .authorize_declared_capability(
            &app_id,
            AppCapability::Microphone,
            AppCapabilityKindDto::Microphone,
            "test reason",
        )
        .await
        .expect("a persisted grant authorizes silently");
    assert!(
        sink.is_empty().await,
        "a persisted grant must not re-prompt the user"
    );
}

#[tokio::test]
async fn a_declared_capability_denial_carries_the_permission_denied_code() {
    let root = TempDir::new().expect("tempdir");
    let service = test_service(&root).await;
    let sink = MockSink::arc();
    let broker =
        LocalAppsHostBroker::new(root.path().to_path_buf(), sink.clone(), None, false, None);
    assert!(broker.attach_service(service.clone()).is_ok());
    let app_id = create_app_fixture(&root, &service, "Denied").await;
    declare_capability(&root, &app_id, AppCapability::Camera);

    let resolver = {
        let sink = sink.clone();
        let broker = broker.clone();
        tokio::spawn(async move {
            loop {
                for event in sink.events().await {
                    if let ClientEvent::AppEvent {
                        event: AppEventDto::AppCapabilityRequested { request },
                    } = event
                    {
                        assert_eq!(request.capability, AppCapabilityKindDto::Camera);
                        assert!(
                            broker
                                .resolve_capability(
                                    &request.request_id,
                                    AppAuthorizationDecisionDto::Deny,
                                )
                                .await
                        );
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let failure = timeout(
        Duration::from_secs(5),
        broker.authorize_declared_capability(
            &app_id,
            AppCapability::Camera,
            AppCapabilityKindDto::Camera,
            "test reason",
        ),
    )
    .await
    .expect("the denial resolves promptly")
    .expect_err("a denied capability must fail");
    assert_eq!(failure.code, Some("permission_denied"));
    resolver.await.expect("resolver completes");
}

#[test]
fn static_server_outlives_the_engine_runtime_that_started_it() {
    let engine_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("engine runtime");
    let (_root, _service, broker, port) = engine_runtime.block_on(async {
        let (root, service, broker) = create_broker(false, None).await;
        let app_id = create_app_fixture(&root, &service, "Survivor").await;
        let started = broker
            .manage_runtime_value(json!({"app_id": app_id, "action": "start"}))
            .await
            .expect("runtime starts");
        let url = started["url"].as_str().expect("url present").to_string();
        let port: u16 = url.rsplit(':').next().expect("port").parse().expect("port");
        (root, service, broker, port)
    });
    drop(engine_runtime);

    let probe = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    probe.block_on(async {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the static server outlives the engine runtime that started it");
        // Connecting only proves the socket is still OPEN: the kernel
        // completes the handshake into the listen backlog even when nobody
        // will ever accept it.  Only a served response proves the listener
        // is still registered with a live I/O driver.
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write request");
        // Read until the BODY arrives, not once: a single `read` returns
        // whatever one poll produced, and under load that is routinely the
        // header block alone (`Content-Length: 15` with the 15 bytes still
        // in flight), which asserted against the body as a served-nothing
        // failure.
        let mut buffer = vec![0u8; 1024];
        let mut response = String::new();
        while !response.contains("<html>ok</html>") {
            let count = timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .expect(
                    "the surviving static server answers instead of stranding the connection",
                )
                .expect("read response");
            assert_ne!(count, 0, "the connection closed mid-response: {response}");
            response.push_str(&String::from_utf8_lossy(&buffer[..count]));
        }
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    });
    drop(broker);
}

/// Binds on a runtime that is then dropped, which is exactly what happens to
/// a listener created on a `MobileEngineHandle`'s runtime: tokio invalidates
/// the registration and every later `accept()` fails.
fn listener_whose_io_driver_is_gone() -> TcpListener {
    let doomed = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("doomed runtime");
    let listener = doomed.block_on(async {
        TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback")
    });
    drop(doomed);
    listener
}

#[test]
fn static_accept_errors_are_retried_but_not_forever() {
    let listener = listener_whose_io_driver_is_gone();
    let root = TempDir::new().expect("tempdir");
    // Held for the whole test: a dropped sender would end the loop through
    // the SHUTDOWN arm and prove nothing about the error bound.
    let (_shutdown, receiver) = oneshot::channel();
    let join = crate::mobile::local_apps_profile::worker_runtime().spawn(run_static_server(
        listener,
        root.path().to_path_buf(),
        receiver,
    ));

    let probe = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    let outcome = probe.block_on(async {
        timeout(Duration::from_secs(30), join)
            .await
            .expect("the accept loop gives up instead of spinning at 20 Hz forever")
            .expect("the static server task did not panic")
    });
    assert!(outcome
        .expect("a listener that can never accept is a fatal exit, not a clean shutdown")
        .contains("stopped accepting connections"),);
}

#[test]
fn a_dead_static_server_fails_the_runtime_record_and_frees_the_entry() {
    let listener = listener_whose_io_driver_is_gone();
    let port = listener.local_addr().expect("local addr").port();

    let harness = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("harness runtime");
    harness.block_on(async move {
        let (root, service, broker) = create_broker(false, None).await;
        let app_id = create_app_fixture(&root, &service, "Dead").await;
        let generation = 7;
        for state in [AppRuntimeState::Starting, AppRuntimeState::Running] {
            service
                .update_runtime_record(&app_id, state, Some(port), None, None)
                .await
                .expect("record the started runtime");
        }
        let (shutdown, receiver) = oneshot::channel();
        broker.runtimes.lock().await.insert(
            app_id.clone(),
            RuntimeEntry {
                state: RuntimeEntryState::Running {
                    handle: RuntimeHandle::Static { shutdown },
                },
                last_used: 1,
                generation,
                build_id: "test-build".into(),
            },
        );
        let layout = AppLayout::new(root.path().to_path_buf(), app_id.clone()).expect("layout");
        let publication_cell = broker
            .runtime_publication_cell(&app_id)
            .expect("publication cell");
        *publication_cell.write().expect("publication identity") =
            Some(RuntimePublicationIdentity {
                generation,
                build_id: "test-build".into(),
            });
        broker.spawn_static_server(
            service.clone(),
            app_id.clone(),
            generation,
            publication_cell.clone(),
            listener,
            root.path()
                .join(layout.build_rel(false))
                .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR),
            receiver,
        );

        // Both halves of the static reconciliation path:
        // waits: `reconcile_static_runtime_exit` removes the entry BEFORE it
        // writes the record, so waking on the removal alone and reading the
        // record next reports the pre-write `running` under load.
        wait_until(
            "the dead static server to retire its own entry and record",
            Duration::from_secs(30),
            || {
                let broker = broker.clone();
                let service = service.clone();
                let app_id = app_id.clone();
                async move {
                    !broker.runtimes.lock().await.contains_key(&app_id)
                        && service
                            .runtime_record(&app_id)
                            .await
                            .is_ok_and(|record| record.state == AppRuntimeState::Failed)
                }
            },
        )
        .await;

        assert!(
            publication_cell
                .read()
                .expect("publication identity")
                .is_none(),
            "dead runtime must invalidate synchronous QA publication identity"
        );

        let record = service
            .runtime_record(&app_id)
            .await
            .expect("runtime record");
        assert_eq!(record.state, AppRuntimeState::Failed);
        assert!(record
            .last_error
            .expect("the accept failure is preserved")
            .contains("stopped accepting connections"));
    });
}

// ------------------------------------------------------------------
// Task 9: the pinned init session's title.
//
// A shell app's init session is minted while `record.name` is still the
// `untitled` placeholder, and that title lands in a PERSISTED session
// directory. Scaffolding renames it — but only when the user has not
// renamed it first, and the boot sweep must apply the SAME rule.
// ------------------------------------------------------------------

/// A shell app with a pinned init session, plus everything needed to read
/// and rewrite that session's title.
struct PinnedShell {
    root: TempDir,
    service: Arc<AppService>,
    broker: Arc<LocalAppsHostBroker>,
    lingxi_home: PathBuf,
    fs: Arc<dyn platform_api::FileSystem>,
    app_id: String,
    init_session_id: String,
    /// Captured at creation so the transcript path is derived exactly the
    /// way production derives it, from the record's own workspace.
    workspace_rel: String,
}

impl PinnedShell {
    fn transcript(&self) -> PathBuf {
        self.lingxi_home
            .join("projects")
            .join(session::jsonl::path::project_dir_name(
                &canonical_cwd_string(&self.root.path().join(&self.workspace_rel)),
            ))
            .join(format!("{}.jsonl", self.init_session_id))
    }

    /// The title the session catalog would resolve for this session.
    fn title(&self) -> String {
        let transcript =
            fs::read_to_string(self.transcript()).expect("read the pinned transcript");
        latest_custom_title(&transcript, &self.init_session_id)
            .expect("the pinned session always carries a custom-title")
            .0
    }

    /// The user renaming the session themselves — `/rename`'s channel
    /// (`append_custom_title`), which carries NO `mobileEmptySession`.
    async fn user_rename(&self, title: &str) {
        session::jsonl::writer::JsonlWriter::new(self.transcript(), self.fs.clone())
            .append_custom_title(&self.init_session_id, title)
            .await
            .expect("user rename");
    }

    /// Run the transcript past `JsonlWriter`'s REAL 32 KiB metadata
    /// backstop, which is what an interview of any length does to this
    /// transcript.
    ///
    /// Deliberately NOT a hand-written unmarked `custom-title` line: the
    /// record has to come out of `plan_re_append` itself, so the test
    /// keeps pinning the production behaviour if that rebuild ever changes
    /// shape. `append_file_history_snapshot` accounts its bytes against
    /// the backstop counter without polling it; the next side-record
    /// append is what fires the poll. Both are ordinary public writer
    /// calls — no test-only hook.
    async fn trip_the_metadata_backstop(&self) {
        let writer =
            session::jsonl::writer::JsonlWriter::new(self.transcript(), self.fs.clone());
        writer
            .append_file_history_snapshot(&json!({
                "type": "file-history-snapshot",
                "sessionId": self.init_session_id,
                "messageId": "interview",
                "snapshot": "x".repeat(
                    session::jsonl::re_append::METADATA_REAPPEND_BACKSTOP_BYTES,
                ),
            }))
            .await
            .expect("bulk interview transcript");
        writer
            .append_permission_mode("default")
            .await
            .expect("the append that polls the backstop");
        assert!(
            !self.latest_title_record_carries_the_marker(),
            "the backstop must really have re-emitted the title UNMARKED — without \
             that this test proves nothing"
        );
    }

    /// Whether the LAST `custom-title` on disk still carries
    /// `mobileEmptySession`. Only a probe: nothing in production may
    /// decide anything from the last record alone.
    fn latest_title_record_carries_the_marker(&self) -> bool {
        let transcript =
            fs::read_to_string(self.transcript()).expect("read the pinned transcript");
        let mut marked = false;
        for line in transcript.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("custom-title")
                || value.get("sessionId").and_then(Value::as_str)
                    != Some(self.init_session_id.as_str())
            {
                continue;
            }
            marked = value.get("mobileEmptySession").and_then(Value::as_u64) == Some(1);
        }
        marked
    }

    async fn scaffold(&self, name: &str) -> Result<Value, String> {
        let input = confirmed_scaffold_input(
            &self.broker,
            &self.app_id,
            name,
            "a confirmed brief",
            "canvas",
        )
        .await;
        self.broker.scaffold_shell_app_value(input).await
    }

    async fn run_boot_backfill_sweep(&self) {
        crate::mobile::host::run_app_boot_backfill_sweep(
            self.lingxi_home.clone(),
            self.root.path().to_string_lossy().to_string(),
            self.root.path().to_path_buf(),
            self.fs.clone(),
            self.service.clone(),
            self.broker.clone(),
        )
        .await;
    }
}

/// The "+" button's state: an unscaffolded shell whose pinned init session
/// is titled with the `untitled` placeholder.
async fn pinned_shell() -> PinnedShell {
    let root = TempDir::new().expect("tempdir");
    let lingxi_home = root.path().join(".lingxi");
    fs::create_dir_all(&lingxi_home).expect("create lingxi home");
    let fs_impl: Arc<dyn platform_api::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(root.path().to_path_buf()),
    );
    let service = test_service(&root).await;
    let broker = LocalAppsHostBroker::new(
        root.path().to_path_buf(),
        Arc::new(NoopClientEventSink),
        Some(MockMobileLinuxRuntime::new(Duration::ZERO)),
        false,
        None,
    );
    assert!(broker.attach_service(service.clone()).is_ok());
    assert!(broker
        .attach_session_catalog(SessionCatalog {
            lingxi_home: lingxi_home.clone(),
            fs: fs_impl.clone(),
        })
        .is_ok());

    let record = service
        .create_app_with_mode(None, "", None, local_apps::CreateMode::Shell, None)
        .await
        .expect("create the shell app");
    assert!(!record.scaffolded);
    assert_eq!(record.name, local_apps::PLACEHOLDER_APP_NAME);

    let init_session_id = crate::mobile::host::mint_app_init_session(
        &lingxi_home,
        &root.path().to_string_lossy(),
        root.path(),
        fs_impl.clone(),
        &record,
    )
    .await
    .expect("mint the pinned init session");
    service
        .set_init_session(&record.id, &init_session_id)
        .await
        .expect("pin the init session");

    let shell = PinnedShell {
        root,
        service,
        broker,
        lingxi_home,
        fs: fs_impl,
        app_id: record.id,
        init_session_id,
        workspace_rel: record.workspace_rel.clone(),
    };
    // The defect this task exists for: the placeholder is already on disk.
    assert_eq!(shell.title(), local_apps::PLACEHOLDER_APP_NAME);
    shell
}

#[tokio::test]
async fn scaffold_renames_the_pinned_session_when_the_user_never_renamed_it() {
    let shell = pinned_shell().await;

    shell.scaffold("打飞机").await.expect("scaffold");

    assert_eq!(shell.title(), "打飞机");
}

/// A rename that fails is only "retryable" if something actually retries
/// it. The scaffold has already committed by then and is NOT rolled back,
/// so the boot sweep is the whole of that guarantee.
#[cfg(unix)]
#[tokio::test]
async fn the_boot_sweep_reconciles_a_title_a_failed_rename_left_behind() {
    use std::os::unix::fs::PermissionsExt;

    let shell = pinned_shell().await;
    // Make the append genuinely fail: a read-only transcript cannot be
    // opened for append. This is the real failure path, not a skipped one.
    let transcript = shell.transcript();
    fs::set_permissions(&transcript, fs::Permissions::from_mode(0o444))
        .expect("make the transcript read-only");

    shell
        .scaffold("打飞机")
        .await
        .expect("a failed rename must not roll the scaffold back");

    fs::set_permissions(&transcript, fs::Permissions::from_mode(0o644))
        .expect("restore the transcript");
    assert_eq!(
        shell.title(),
        local_apps::PLACEHOLDER_APP_NAME,
        "the rename really did fail, so the retry has something to repair"
    );
    assert!(
        shell
            .service
            .record(&shell.app_id)
            .await
            .expect("record")
            .scaffolded,
        "the scaffold itself committed"
    );

    shell.run_boot_backfill_sweep().await;

    assert_eq!(
        shell.title(),
        "打飞机",
        "a failed rename must have a real trigger that fixes it later"
    );
}

/// The real flow, not the shortest one: an interview long enough to trip
/// the transcript writer's 32 KiB metadata backstop still gets its title.
///
/// This is the test whose absence made the whole reconciliation invisible.
/// The backstop re-emits the title as a PLAIN `custom-title`, so a
/// predicate that read the marker off the LAST record declined for every
/// app created through this flow and they all kept `untitled` forever —
/// with `scaffold_renames_the_pinned_session_when_the_user_never_renamed_it`
/// (a transcript of two lines) staying green throughout.
#[tokio::test]
async fn the_rename_survives_the_metadata_backstop_a_real_interview_trips() {
    let shell = pinned_shell().await;
    shell.trip_the_metadata_backstop().await;

    shell.scaffold("打飞机").await.expect("scaffold");

    assert_eq!(
        shell.title(),
        "打飞机",
        "an interview longer than 32 KiB must not cost the app its name"
    );
}

/// The other half, and the one that must never regress: tolerating the
/// backstop's unmarked echo must not make a real `/rename` overwritable.
///
/// After `/rename`, the backstop echoes the USER'S title unmarked — text
/// the anchor never carried — so the predicate declines, immediately and
/// on every later boot sweep.
#[tokio::test]
async fn a_user_rename_still_wins_after_the_backstop_echoes_it() {
    let shell = pinned_shell().await;
    shell.user_rename("我的宝贝项目").await;
    shell.trip_the_metadata_backstop().await;
    assert_eq!(
        shell.title(),
        "我的宝贝项目",
        "the backstop echoes the user's title, so that is what the scaffold sees"
    );

    shell.scaffold("打飞机").await.expect("scaffold");
    shell.run_boot_backfill_sweep().await;

    assert_eq!(shell.title(), "我的宝贝项目");
}

#[tokio::test]
async fn an_immediate_rename_never_clobbers_a_user_rename() {
    let shell = pinned_shell().await;
    shell.user_rename("我的宝贝项目").await;

    shell.scaffold("打飞机").await.expect("scaffold");

    assert_eq!(shell.title(), "我的宝贝项目");
}

#[tokio::test]
async fn the_boot_sweep_never_clobbers_a_user_rename_either() {
    let shell = pinned_shell().await;
    shell.user_rename("我的宝贝项目").await;
    shell.scaffold("打飞机").await.expect("scaffold");

    shell.run_boot_backfill_sweep().await;

    assert_eq!(shell.title(), "我的宝贝项目");
}

/// `workspace/LINGXI.md` is the ONE channel that reaches the model for an
/// unscaffolded shell (`r3-e2e-trace-01`). Nothing in the create
/// transaction ever revisits it after the initial write, so if it is ever
/// lost — a partial restore, a wiped workspace mount — the boot sweep must
/// be the thing that notices and rewrites it; otherwise the interview
/// never restarts and the agent sees an ordinary empty directory.
#[tokio::test]
async fn boot_sweep_repairs_a_missing_guided_workspace_contract() {
    let shell = pinned_shell().await;
    let lingxi_md = workspace_of(&shell.root, &shell.app_id).join("LINGXI.md");
    // `pinned_shell()` builds its record straight off `AppService`,
    // bypassing the broker's create-time initializer hook (the one that
    // normally writes the guided contract) — so this fixture's workspace
    // starts with no `LINGXI.md` at all, which is exactly the "lost"
    // state this test needs. Removing it too makes that starting point
    // explicit regardless of what the fixture happens to do.
    let _ = fs::remove_file(&lingxi_md);
    assert!(
        !lingxi_md.exists(),
        "the guided contract must be absent before the sweep runs"
    );

    shell.run_boot_backfill_sweep().await;

    let repaired = fs::read_to_string(&lingxi_md)
        .expect("the boot sweep must rewrite a missing guided workspace contract");
    assert!(
        repaired.contains("has no shape yet"),
        "the repaired file must be the real guided contract, not a stub: {repaired}"
    );
}

/// Same repair, but for a TRUNCATED file rather than an absent one — an
/// interrupted write can leave bytes on disk that are not the contract.
#[tokio::test]
async fn boot_sweep_repairs_a_truncated_guided_workspace_contract() {
    let shell = pinned_shell().await;
    let lingxi_md = workspace_of(&shell.root, &shell.app_id).join("LINGXI.md");
    fs::write(&lingxi_md, "")
        .expect("truncate the guided contract to simulate a partial write");

    shell.run_boot_backfill_sweep().await;

    let repaired =
        fs::read_to_string(&lingxi_md).expect("guided contract still present after repair");
    assert!(
        repaired.contains("has no shape yet"),
        "a truncated guided contract must be rewritten, not left empty: {repaired}"
    );
}

/// The repair must be scoped to UNSCAFFOLDED shells: once an app is
/// formed, `workspace/LINGXI.md` carries the FORMAL contract, and step 0
/// rewriting it back to the guided text on every boot would erase the
/// surface-specific rules the formal contract exists to state.
#[tokio::test]
async fn boot_sweep_never_rewrites_a_formed_apps_formal_contract() {
    let shell = pinned_shell().await;
    shell.scaffold("打飞机").await.expect("scaffold");
    let lingxi_md = workspace_of(&shell.root, &shell.app_id).join("LINGXI.md");
    let formal_before = fs::read_to_string(&lingxi_md).expect("formal contract");
    assert!(
        !formal_before.contains("has no shape yet"),
        "a formed app's contract must already be the FORMAL one: {formal_before}"
    );

    shell.run_boot_backfill_sweep().await;

    let formal_after = fs::read_to_string(&lingxi_md).expect("formal contract after sweep");
    assert_eq!(
        formal_before, formal_after,
        "step 0 must never overwrite a formed app's formal contract with the guided one"
    );
}

/// Clause 1 of the predicate, pinned directly: an app still in its
/// interview keeps the placeholder title even when its record already
/// carries a real name. Driven through `reconcile_app_init_session_title`
/// rather than a whole scaffold, because the app paths cannot currently
/// produce this state — the point is that the rule survives a refactor
/// that lets them.
#[tokio::test]
async fn reconciliation_waits_for_the_scaffold_commit_before_renaming() {
    let shell = pinned_shell().await;
    let mut record = shell.service.record(&shell.app_id).await.expect("record");
    record.name = "打飞机".into();
    assert!(!record.scaffolded);

    let renamed = reconcile_app_init_session_title(
        &shell.lingxi_home,
        shell.root.path(),
        shell.fs.clone(),
        &record,
    )
    .await
    .expect("reconcile");

    assert!(!renamed, "an unscaffolded shell is not renamed");
    assert_eq!(shell.title(), local_apps::PLACEHOLDER_APP_NAME);

    // The same record, one field later: the commit is the only thing that
    // was missing.
    record.scaffolded = true;
    assert!(reconcile_app_init_session_title(
        &shell.lingxi_home,
        shell.root.path(),
        shell.fs.clone(),
        &record,
    )
    .await
    .expect("reconcile"));
    assert_eq!(shell.title(), "打飞机");
}

/// The discriminator, stated as a unit. Three writers share the
/// `custom-title` channel, only one of them marks its records, and a
/// fourth — the writer's own 32 KiB metadata backstop — re-emits whatever
/// the title currently is, UNMARKED. So the question is never "is the last
/// record marked" but "did anyone write text mobile did not".
#[test]
fn a_placeholder_is_told_from_a_user_rename_by_text_against_the_anchor() {
    let session = "11111111-2222-3333-4444-555555555555";
    let anchor = format!(
        r#"{{"type":"custom-title","customTitle":"untitled","sessionId":"{session}","mobileEmptySession":1}}"#
    );
    // What `plan_re_append` writes when the backstop fires: the anchor's
    // own text, rebuilt without the marker.
    let backstop_echo = format!(
        r#"{{"type":"custom-title","customTitle":"untitled","sessionId":"{session}"}}"#
    );
    let user_rename = format!(
        r#"{{"type":"custom-title","customTitle":"我的宝贝项目","sessionId":"{session}"}}"#
    );
    let other_session = r#"{"type":"custom-title","customTitle":"elsewhere","sessionId":"99999999-2222-3333-4444-555555555555"}"#;

    assert!(latest_custom_title_is_mobile_placeholder(&anchor, session));
    // An unmarked record echoing the anchor's text is the backstop, not a
    // user. Reading the marker off the last record here is what made
    // `reconcile_app_init_session_title` unreachable in production.
    assert!(latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{backstop_echo}"),
        session
    ));
    // Text mobile never wrote, after the anchor: a user rename, and it
    // stays one however many times the backstop echoes it afterwards.
    assert!(!latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{user_rename}"),
        session
    ));
    assert!(!latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{user_rename}\n{user_rename}"),
        session
    ));
    // An unmarked record with no anchor before it — a `session::branch`
    // fork's title — is superseded by an anchor that follows it.
    assert!(!latest_custom_title_is_mobile_placeholder(
        &user_rename,
        session
    ));
    assert!(latest_custom_title_is_mobile_placeholder(
        &format!("{user_rename}\n{anchor}"),
        session
    ));
    // A record for another session never decides this one.
    assert!(latest_custom_title_is_mobile_placeholder(
        &format!("{anchor}\n{other_session}"),
        session
    ));
    // Nothing this host anchored: leave it alone.
    assert!(!latest_custom_title_is_mobile_placeholder("", session));
    // The effective title is still the LAST record's, marked or not.
    assert_eq!(
        latest_custom_title(&format!("{anchor}\n{user_rename}"), session)
            .expect("a title")
            .0,
        "我的宝贝项目"
    );
}
