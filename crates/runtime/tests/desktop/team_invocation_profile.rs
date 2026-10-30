//! Private-profile acceptance using the real Desktop assembly and Bash tool.
//! No model, real account, network service or owner data is used.
#![cfg(target_os = "macos")]

use harness_runtime::desktop::team_invocation::{
    TeamInvocationIdentity, TeamInvocationLimits, TeamInvocationProfile,
};
use harness_runtime::desktop::{self, DesktopConfig};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tool_api::Tool;

fn identity() -> TeamInvocationIdentity {
    TeamInvocationIdentity {
        owner_uid: "owner".into(),
        bot_key: "bot:owner:host:bot".into(),
        team_id: "team-001".into(),
        conversation_id: "conv-001".into(),
        origin_human_uid: "alice".into(),
        membership_generation: 1,
    }
}
fn root() -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, path)
}
fn profile(root: &Path, id: TeamInvocationIdentity) -> TeamInvocationProfile {
    TeamInvocationProfile::prepare(
        root,
        id,
        uuid::Uuid::new_v4(),
        "Host-selected persona and immutable Skill snapshot".into(),
        TeamInvocationLimits::default(),
    )
    .unwrap()
}
fn config(profile: &TeamInvocationProfile) -> DesktopConfig {
    let mut cfg = DesktopConfig::default();
    cfg.default_model = "anthropic/claude-sonnet-4-20250514".into();
    profile.apply_to(&mut cfg).unwrap();
    cfg
}

#[test]
fn every_principal_component_changes_the_private_namespace() {
    let (_dir, base) = root();
    let original = identity();
    let first = profile(&base, original.clone());
    let mut roots = std::collections::HashSet::from([first.scope_root().to_path_buf()]);
    for field in 0..6 {
        let mut changed = original.clone();
        match field {
            0 => changed.owner_uid.push('2'),
            1 => changed.bot_key.push('2'),
            2 => changed.team_id.push('2'),
            3 => changed.conversation_id.push('2'),
            4 => changed.origin_human_uid = "bob".into(),
            _ => changed.membership_generation += 1,
        }
        assert!(roots.insert(profile(&base, changed).scope_root().to_path_buf()));
    }
    assert_eq!(first.scope_root(), profile(&base, original).scope_root());
}

#[test]
fn instructions_are_immutable_private_snapshots_and_inputs_are_byte_bounded() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let inputs = std::fs::read_dir(p.scope_root().join("inputs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(std::fs::metadata(&inputs).unwrap().mode() & 0o777, 0o400);
    for child in ["home", "workspace", "tmp", "shell-home", "inputs"] {
        assert_eq!(
            std::fs::metadata(p.scope_root().join(child))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    p.validate_input(&"x".repeat(32768)).unwrap();
    assert!(p.validate_input(&"界".repeat(10923)).is_err());
    assert!(p.validate_input("\0").is_err());
    assert!(p.validate_input("   ").is_err());
    // A corrupted snapshot cannot be silently replaced or accepted.
    std::fs::set_permissions(&inputs, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(inputs, "tampered").unwrap();
    assert!(p.apply_to(&mut config_base()).is_err());
}

fn config_base() -> DesktopConfig {
    let mut cfg = DesktopConfig::default();
    cfg.default_model = "anthropic/claude-sonnet-4-20250514".into();
    cfg
}

#[test]
fn path_components_and_snapshots_refuse_symlink_redirection() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let outside = base.join("outside");
    std::fs::create_dir(&outside).unwrap();
    let workspace = p.scope_root().join("workspace");
    std::fs::remove_dir(&workspace).unwrap();
    std::os::unix::fs::symlink(outside, workspace).unwrap();
    assert!(p.apply_to(&mut config_base()).is_err());
}

#[test]
fn scope_roots_cannot_be_interpreted_as_sandbox_patterns() {
    use std::os::unix::fs::DirBuilderExt;
    let (_dir, base) = root();
    for name in ["scope*", "scope?", "scope[1]", "scope\nnewline"] {
        let path = base.join(name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        assert!(TeamInvocationProfile::prepare(
            &path,
            identity(),
            uuid::Uuid::new_v4(),
            "persona".into(),
            Default::default()
        )
        .is_err());
    }
}

#[test]
fn applying_a_profile_discards_owner_capabilities_and_never_resets_shared_authority() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let mut cfg = config_base();
    cfg.cwd = PathBuf::from("/owner/repository");
    cfg.lingxi_home = PathBuf::from("/owner/home");
    cfg.api_key_helper = Some("owner helper".into());
    cfg.memory_provider = Some(Arc::new(
        orchestrator::test_support::StaticMemoryProvider::empty(),
    ));
    cfg.mcp_paths = vec![PathBuf::from("/owner/.mcp.json")];
    cfg.add_dir = vec![PathBuf::from("/owner")];
    cfg.cli_plugin_dirs = vec![PathBuf::from("/owner/plugins")];
    cfg.allow_dangerously_skip_permissions = true;
    cfg.use_noop_permission_gate = true;
    cfg.inline_visualization = true;
    cfg.local_apps = true;
    cfg.worktree_launch = Some("owner-worktree".into());
    let (owner_questions, _) = tokio::sync::mpsc::channel(1);
    cfg.ask_user_question_tx = Some(owner_questions);
    cfg.provider_profiles = Some(std::collections::BTreeMap::from([
        ("anthropic".into(), serde_json::json!({"models":[]})),
        (
            "owner-other".into(),
            serde_json::json!({"apiKey":"synthetic-owner-key"}),
        ),
    ]));
    cfg.flag_settings =
        Some(serde_json::from_value(serde_json::json!({"sandbox":{"enabled":false}})).unwrap());
    p.apply_to(&mut cfg).unwrap();
    assert_eq!(cfg.cwd, p.scope_root().join("workspace"));
    assert_eq!(cfg.lingxi_home, p.scope_root().join("home"));
    assert!(
        cfg.api_key_helper.is_none() && cfg.memory_provider.is_none() && cfg.mcp_paths.is_empty()
    );
    assert!(
        cfg.add_dir.is_empty() && cfg.cli_plugin_dirs.is_empty() && cfg.flag_settings.is_none()
    );
    assert!(
        !cfg.allow_dangerously_skip_permissions
            && !cfg.use_noop_permission_gate
            && !cfg.inline_visualization
            && !cfg.local_apps
    );
    assert!(cfg.worktree_launch.is_none() && cfg.system_prompt_override.is_none());
    assert!(cfg.ask_user_question_tx.is_none());
    assert_eq!(
        cfg.provider_profiles
            .as_ref()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["anthropic"]
    );
    assert_eq!(cfg.setting_source_scope, (false, false));
    assert_eq!(cfg.max_turns, Some(12));
    let quota = cfg.scope_output_quota.clone().unwrap();
    let captured = cfg.session_tool_grant_lease.clone().unwrap();
    assert!(captured.allows_builtin("Bash") && captured.allows_builtin("AskUserQuestion"));
    for tool in [
        "Read",
        "Edit",
        "Agent",
        "ToolSearch",
        "Computer",
        "Skill",
        "MCP",
    ] {
        assert!(!captured.allows_builtin(tool));
    }
    p.apply_to(&mut cfg).unwrap();
    assert!(Arc::ptr_eq(
        &quota,
        cfg.scope_output_quota.as_ref().unwrap()
    ));
    p.clone().revoke();
    assert!(quota.is_revoked());
    assert!(!captured.allows_builtin("Bash"));
    assert!(p.apply_to(&mut cfg).is_err());
    assert!(p.validate_input("next").is_err());
}

#[test]
fn invalid_principals_limits_and_unqualified_models_fail_closed() {
    let (_dir, base) = root();
    let mut id = identity();
    id.membership_generation = 0;
    assert!(TeamInvocationProfile::prepare(
        &base,
        id,
        uuid::Uuid::new_v4(),
        "persona".into(),
        Default::default()
    )
    .is_err());
    let limits = TeamInvocationLimits {
        max_turns: std::num::NonZeroU32::new(21).unwrap(),
        ..Default::default()
    };
    assert!(TeamInvocationProfile::prepare(
        &base,
        identity(),
        uuid::Uuid::new_v4(),
        "persona".into(),
        limits
    )
    .is_err());
    let p = profile(&base, identity());
    let mut cfg = DesktopConfig::default();
    cfg.default_model = "model-without-profile".into();
    assert!(p.apply_to(&mut cfg).is_err());
}

#[test]
fn forged_worktree_and_live_toggle_cannot_expand_the_frozen_shell_grant() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let workspace = p.scope_root().join("workspace");
    let fake = workspace.join("fake.git");
    std::fs::create_dir(&fake).unwrap();
    std::fs::write(workspace.join(".git"), "gitdir: fake.git\n").unwrap();
    std::fs::write(fake.join("commondir"), base.to_str().unwrap()).unwrap();
    let mut ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.isolated_shell = Some(p.shell_policy());
    ctx.sandbox_runtime.enabled = false;
    ctx.sandbox_runtime.excluded_commands = vec!["*".into()];
    ctx.sandbox_enabled_override = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let runtime = ctx.sandbox_runtime_at(&workspace, sandbox::root::SandboxRootScope::Session);
    assert!(runtime.enabled && runtime.fail_if_unavailable && !runtime.allow_unsandboxed_commands);
    assert!(runtime.excluded_commands.is_empty());
    assert_eq!(
        runtime.filesystem.allow_write,
        p.shell_policy().sandbox().filesystem.allow_write
    );
    assert!(!runtime
        .filesystem
        .allow_write
        .contains(&base.to_string_lossy().into_owned()));
}

#[derive(Default)]
struct NoopPermissionSink;
#[async_trait::async_trait]
impl client::adapter::PermissionRequestSink for NoopPermissionSink {
    async fn emit_request(&self, _: client::protocol::permission::PermissionRequest) {}
}
async fn build(p: &TeamInvocationProfile) -> desktop::DesktopRuntime {
    desktop::build(
        config(p),
        Arc::new(orchestrator::test_support::MockOutputStream::new()),
        Arc::new(NoopPermissionSink),
    )
    .await
    .unwrap()
}
async fn call(tool: &Arc<dyn Tool>, command: &str) -> tool_api::ToolCallResult {
    let input = Json::from_serde_value(&serde_json::json!({"command":command})).unwrap();
    let (tx, _rx) = tool_api::progress_channel();
    tool.call(input, tool_api::test_support::fresh_ctx(), tx)
        .await
        .unwrap()
}
fn assert_os_denied(result: &tool_api::ToolCallResult) {
    // Completed Bash calls report process status in metadata; `is_error` only
    // describes interruption and is not an exit-code assertion.
    let data = result.data.to_json_string().unwrap();
    assert!(
        result
            .data
            .string_at("/returnCodeInterpretation")
            .is_some_and(|text| text.starts_with("Command failed with exit code")),
        "{data}"
    );
    assert!(
        result
            .data
            .string_at("/stderr")
            .is_some_and(|text| text.contains("Operation not permitted")
                || text.contains("Permission denied")),
        "{data}"
    );
    assert!(
        !data.contains("sandbox_apply:"),
        "sandbox must launch successfully: {data}"
    );
}
fn quote(p: &Path) -> String {
    format!("'{}'", p.to_str().unwrap().replace('\'', "'\\''"))
}

// This integration binary runs with --test-threads=1. Restore every fixture
// variable even when an assertion fails; never print inherited values.
struct EnvironmentFixture(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl EnvironmentFixture {
    fn set(values: &[(&'static str, &std::ffi::OsStr)]) -> Self {
        let old = values
            .iter()
            .map(|(name, value)| {
                let old = std::env::var_os(name);
                std::env::set_var(name, value);
                (*name, old)
            })
            .collect();
        Self(old)
    }
}
impl Drop for EnvironmentFixture {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

struct NoMemoryQuery;
#[async_trait::async_trait]
impl sidequery::SideQueryClient for NoMemoryQuery {
    async fn query(
        &self,
        _: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        panic!("root validation must not call a model");
    }
}

#[test]
fn explicit_memory_factories_never_honor_the_owner_environment() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let owner_config = base.join("synthetic-owner-config");
    let _environment = EnvironmentFixture::set(&[("LINGXI_CONFIG_DIR", owner_config.as_os_str())]);
    let query: Arc<dyn sidequery::SideQueryClient> = Arc::new(NoMemoryQuery);
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> =
        Arc::new(platform_posix::PosixRuntime::new());
    let cwd = p.scope_root().join("workspace");
    let home = p.scope_root().join("home");
    let ordinary =
        orchestrator::prompt::build_memdir_prefetch(query.clone(), runtime.clone(), &base, &cwd);
    assert!(ordinary.user_memdir().unwrap().starts_with(&owner_config));
    let scoped = orchestrator::prompt::memory_block::build_memdir_prefetch_at(
        query.clone(),
        runtime.clone(),
        &home,
        &cwd,
    );
    assert!(scoped.user_memdir().unwrap().starts_with(&home));
    assert!(!scoped.user_memdir().unwrap().starts_with(&owner_config));
    let extractor = orchestrator::prompt::memory_block::build_session_memory_handle_at(
        query,
        "model".into(),
        Default::default(),
        home.clone(),
        runtime,
    );
    assert_eq!(extractor.config_home, home);
}

#[tokio::test]
async fn actual_assembly_and_shell_confine_files_environment_memory_and_controls() {
    let (_dir, base) = root();
    let (human_questions, _human_rx) = tokio::sync::mpsc::channel(1);
    let p = profile(&base, identity()).with_original_human_question_sender(human_questions);
    let outside = base.join("owner-secret");
    std::fs::write(&outside, "synthetic-private-fixture").unwrap();
    let owner = base.join("synthetic-owner");
    std::fs::create_dir_all(owner.join(".lingxi")).unwrap();
    std::fs::write(
        owner.join(".lingxi/CLAUDE.md"),
        "SYNTHETIC_OWNER_INSTRUCTIONS_MUST_NOT_APPEAR",
    )
    .unwrap();
    std::fs::write(
        base.join("CLAUDE.md"),
        "SYNTHETIC_ANCESTOR_INSTRUCTIONS_MUST_NOT_APPEAR",
    )
    .unwrap();
    let marker = base.join("startup-must-not-run");
    let startup = base.join("bash-env");
    std::fs::write(&startup, format!("printf leaked > {}", quote(&marker))).unwrap();
    std::fs::write(owner.join(".lingxi/settings.json"),
        serde_json::json!({"apiKeyHelper":format!("touch {}", quote(&marker)),
            "hooks":{"SessionStart":[{"hooks":[{"type":"command","command":format!("touch {}", quote(&marker))}]}]},
            "sandbox":{"enabled":false}}).to_string()).unwrap();
    let _environment = EnvironmentFixture::set(&[
        ("HOME", owner.as_os_str()),
        ("LINGXI_CONFIG_DIR", owner.join(".lingxi").as_os_str()),
        (
            "LINGXI_TEAM_TEST_SECRET",
            std::ffi::OsStr::new("synthetic-env-secret"),
        ),
        ("BASH_ENV", startup.as_os_str()),
    ]);
    let rt = build(&p).await;
    assert_eq!(rt.settings_watcher.task_count(), 0);
    assert!(rt.hook_registry.read().await.all_hooks().is_empty());
    assert!(rt.plugin_runtime.is_none());
    assert!(rt.orchestrator.has_memory_prefetch());
    assert!(!rt.sandbox_desc_fallback);
    let prompt = rt
        .orchestrator
        .assemble_system_prompt_preview()
        .await
        .unwrap();
    let memory_roots = memory::memdir::paths::memdir_roots_at(
        &p.scope_root().join("home"),
        &p.scope_root().join("workspace"),
        false,
    );
    assert!(prompt.contains(memory_roots.user_memdir.to_str().unwrap()));
    assert!(prompt.contains("Host-selected persona and immutable Skill snapshot"));
    assert!(!prompt.contains("SYNTHETIC_OWNER_INSTRUCTIONS_MUST_NOT_APPEAR"));
    assert!(!prompt.contains("SYNTHETIC_ANCESTOR_INSTRUCTIONS_MUST_NOT_APPEAR"));
    let registry = rt.orchestrator.shared_tool_registry();
    let mut names = registry.all_names();
    names.sort();
    assert_eq!(names, vec!["AskUserQuestion", "Bash"]);
    assert!(registry.find_by_name("Read").is_none());
    // Even a read-only shell command needs the host approval adapter. A Team
    // member is not wired to this adapter. Draining denies the unanswered ask.
    let gate = rt.enforcing_permission_gate.as_ref().unwrap().clone();
    let check = tokio::spawn(async move {
        gate.check(
            "Bash",
            &Json::from_serde_value(&serde_json::json!({"command":"pwd"})).unwrap(),
        )
        .await
    });
    let adapter = rt.permission_gate.as_ref().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while adapter.pending_count().await == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Team Bash must request host approval");
    adapter.drain().await;
    assert!(matches!(
        check.await.unwrap(),
        lingxi_core::host::PermissionDecision::Deny { .. }
    ));
    assert!(!rt.shell_expansion.git_worktree_context().await.inside);
    let expansion = rt.shell_expansion.build(&["Bash(*)".into()], None);
    assert!(expansion
        .runner
        .run(&"printf should-not-run".into(), None, Default::default())
        .await
        .is_err());
    // Simulate the owner's tool approval, then call the actual registered
    // Bash implementation. No permission is supplied by a Team member.
    let bash = registry.find_by_name("Bash").unwrap();
    rt.sandbox_toggle.store(false, Ordering::SeqCst);
    let env = call(&bash, "printf 'HOME=%s\\nTMP=%s\\nPATH=%s\\n' \"$HOME\" \"$TMPDIR\" \"$PATH\"; printf 'clean:%s' \"${LINGXI_TEAM_TEST_SECRET-unset}\"; printf ok > local.txt").await;
    assert!(!env.is_error, "{}", env.data.to_json_string().unwrap());
    let data = env.data.to_json_string().unwrap();
    assert!(
        data.contains("shell-home") && data.contains("clean:unset"),
        "{data}"
    );
    assert!(!data.contains("synthetic-env-secret"));
    assert!(
        !marker.exists(),
        "owner startup files/helpers/hooks must not execute"
    );
    assert_eq!(
        std::fs::read_to_string(p.scope_root().join("workspace/local.txt")).unwrap(),
        "ok"
    );
    let result = call(&bash, &format!("cat {}", quote(&outside))).await;
    assert_os_denied(&result);
    assert!(!result
        .data
        .to_json_string()
        .unwrap()
        .contains("synthetic-private-fixture"));
    let inputs = std::fs::read_dir(p.scope_root().join("inputs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    for path in [
        &outside,
        &inputs,
        &p.scope_root().join("home/settings.json"),
    ] {
        assert_os_denied(&call(&bash, &format!("printf changed > {}", quote(path))).await);
    }
    let link = p.scope_root().join("workspace/escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert_os_denied(&call(&bash, "cat escape").await);
    let hard_link = call(
        &bash,
        &format!("ln {} hard-escape && cat hard-escape", quote(&outside)),
    )
    .await;
    assert_os_denied(&hard_link);
    assert!(!hard_link
        .data
        .to_json_string()
        .unwrap()
        .contains("synthetic-private-fixture"));
    // A real loopback listener proves network failure is the sandbox policy,
    // rather than a missing host or an offline external service.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_os_denied(
        &call(
            &bash,
            &format!("printf blocked > /dev/tcp/127.0.0.1/{port}"),
        )
        .await,
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    // cwd tracking has no unconfined readback or persisted model cwd.
    call(&bash, "mkdir -p sub; cd sub; printf inside").await;
    let cwd = call(&bash, "pwd -P").await.data.to_json_string().unwrap();
    assert!(cwd.contains(p.scope_root().join("workspace").to_str().unwrap()));
    assert!(!cwd.contains("workspace/sub"));
    rt.session_lifecycle.shutdown_and_drain().await;
    assert!(!p.scope_root().join("home/.credentials.json").exists());
}

#[tokio::test]
async fn private_bash_refuses_unavailable_sandbox_and_explicit_background() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let mut ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.isolated_shell = Some(p.shell_policy());
    ctx.sandbox_available = false;
    let bash = tool_shell::BashTool::new(ctx);
    let (tx, _rx) = tool_api::progress_channel();
    assert!(bash
        .call(
            Json::from_serde_value(&serde_json::json!({"command":"printf blocked"})).unwrap(),
            tool_api::test_support::fresh_ctx(),
            tx
        )
        .await
        .is_err());
    // A fresh, available context prevents sandbox failure from satisfying the
    // explicit-background assertion for the wrong reason.
    let mut available = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: "ok".into(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    available.isolated_shell = Some(p.shell_policy());
    available.sandbox_available = true;
    let bash = tool_shell::BashTool::new(available);
    let (tx, _rx) = tool_api::progress_channel();
    let error = bash
        .call(
            Json::from_serde_value(
                &serde_json::json!({"command":"printf blocked","run_in_background":true}),
            )
            .unwrap(),
            tool_api::test_support::fresh_ctx(),
            tx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, tool_api::ToolError::InvalidInput(message) if message.to_json_string().unwrap().contains("Background shell execution"))
    );
}

#[tokio::test]
async fn public_headless_service_reenforces_the_profile_and_retains_its_own_policy_cache() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let marker = base.join("helper-must-not-run");
    let mut cfg = config(&p);
    // Host code can accidentally edit a config after apply_to. Every public
    // service entry must freeze it again before opening credentials/helpers.
    cfg.api_key_helper = Some(format!("touch {}; printf synthetic", quote(&marker)));
    cfg.fallback_model = Some("owner-other/private-model".into());
    let _environment = EnvironmentFixture::set(&[
        (
            "ANTHROPIC_AUTH_TOKEN",
            std::ffi::OsStr::new("synthetic-owner-oauth"),
        ),
        (
            "OPENAI_PERSONAL_ACCESS_TOKEN",
            std::ffi::OsStr::new("synthetic-owner-pat"),
        ),
    ]);
    let stack = desktop::resolve_llm_stack(&cfg).await.unwrap();
    assert!(stack.resolved_anthropic_api_key.is_none() && !stack.has_oauth_token);
    let _service = desktop::build_api_service(&cfg, &cfg.cwd).await.unwrap();
    assert!(!marker.exists());
    p.revoke();
    assert!(desktop::build_api_service(&cfg, &cfg.cwd).await.is_err());
}

#[test]
fn selecting_another_provider_discards_the_unrelated_anthropic_credential_slot() {
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let mut cfg = config_base();
    cfg.default_model = "selected/model".into();
    cfg.api_key = "synthetic-owner-key".into();
    cfg.api_base = "https://synthetic-owner.invalid".into();
    cfg.provider_profiles = Some(std::collections::BTreeMap::from([
        ("selected".into(), serde_json::json!({"models":[]})),
        (
            "owner-other".into(),
            serde_json::json!({"apiKey":"synthetic-unrelated-key"}),
        ),
    ]));
    p.apply_to(&mut cfg).unwrap();
    assert!(cfg.api_key.is_empty());
    assert_eq!(cfg.api_key_source, DesktopConfig::default().api_key_source);
    assert_eq!(cfg.api_base, DesktopConfig::default().api_base);
    assert_eq!(
        cfg.provider_profiles
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["selected"]
    );
}

#[tokio::test]
async fn untrusted_git_metadata_cannot_run_host_commands_at_boot_prompt_or_transcript_append() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, base) = root();
    let p = profile(&base, identity());
    let repo = base.join("synthetic-owner-repo");
    let empty_templates = base.join("empty-templates");
    std::fs::create_dir(&empty_templates).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", base.join("synthetic-home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TEMPLATE_DIR", &empty_templates)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", repo.to_str().unwrap()]);
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "-qm",
        "SYNTHETIC_OWNER_COMMIT_MUST_NOT_APPEAR",
    ]);
    let marker = base.join("unconfined-fsmonitor-must-not-run");
    let hook = base.join("fixture-fsmonitor");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf executed > {}\nprintf '\\000'\n",
            quote(&marker)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "config",
        "core.fsmonitor",
        hook.to_str().unwrap(),
    ]);
    git(&[
        "-C",
        repo.to_str().unwrap(),
        "config",
        "core.worktree",
        repo.to_str().unwrap(),
    ]);
    let workspace = p.scope_root().join("workspace");
    std::fs::write(
        workspace.join(".git"),
        format!("gitdir: {}\n", repo.join(".git").display()),
    )
    .unwrap();
    // Positive control: this valid fixture follows the external .git pointer
    // and executes fsmonitor when an ordinary host Git probe runs.
    git(&["-C", workspace.to_str().unwrap(), "status", "--porcelain"]);
    assert!(marker.exists());
    std::fs::remove_file(&marker).unwrap();
    let rt = build(&p).await;
    assert!(
        !marker.exists(),
        "Team boot must not run a subagent Git probe"
    );
    let prompt = rt
        .orchestrator
        .assemble_system_prompt_preview()
        .await
        .unwrap();
    assert!(
        !marker.exists(),
        "Team prompt must not probe model-controlled Git"
    );
    assert!(!prompt.contains("SYNTHETIC_OWNER_COMMIT_MUST_NOT_APPEAR"));
    rt.orchestrator
        .append_external_history_message(lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "synthetic seed".into(),
        ))
        .await;
    let transcript = std::fs::read_to_string(config(&p).session_transcript_path.unwrap()).unwrap();
    assert!(!transcript.contains("gitBranch"));
    assert!(!marker.exists());
    rt.session_lifecycle.shutdown_and_drain().await;
}
