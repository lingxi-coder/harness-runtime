//! Private-profile acceptance using the real Desktop assembly and Bash tool.
//! No model, real account, network service or owner data is used.
#![cfg(target_os = "macos")]

use harness_runtime::desktop::team_invocation::{
    BotChatScope, TeamInvocationLimits, TeamInvocationProfile,
};
use harness_runtime::desktop::DesktopConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn identity() -> BotChatScope {
    BotChatScope {
        shared_bot_id: "sharedbot_01".into(),
        team_id: "team_0001".into(),
        conversation_id: "chat_0001".into(),
        session_epoch: 1,
    }
}
fn root() -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, path)
}
fn profile(root: &Path, id: BotChatScope) -> TeamInvocationProfile {
    TeamInvocationProfile::prepare(
        root,
        id,
        uuid::Uuid::new_v4(),
        "Host-selected persona and immutable Skill snapshot".into(),
        TeamInvocationLimits::default(),
    )
    .unwrap()
}

#[test]
fn every_principal_component_changes_the_private_namespace() {
    let (_dir, base) = root();
    let original = identity();
    let first = profile(&base, original.clone());
    let mut roots = std::collections::HashSet::from([first.scope_root().to_path_buf()]);
    for field in 0..4 {
        let mut changed = original.clone();
        match field {
            0 => changed.shared_bot_id.push('2'),
            1 => changed.team_id.push('2'),
            2 => changed.conversation_id.push('2'),
            _ => changed.session_epoch += 1,
        }
        assert!(roots.insert(profile(&base, changed).scope_root().to_path_buf()));
    }
    assert_eq!(first.scope_root(), profile(&base, original).scope_root());
}

#[test]
fn instructions_are_immutable_private_snapshots_and_profile_authority_stays_fixed() {
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
    assert!(cfg.memory_provider.is_none() && cfg.mcp_paths.is_empty());
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
    assert!(captured.allows_builtin("TeamChatHistory") && captured.allows_builtin("TeamKnowledge"));
    assert!(!captured.allows_builtin("Bash") && !captured.allows_builtin("AskUserQuestion"));
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
}

#[test]
fn invalid_principals_limits_and_unqualified_models_fail_closed() {
    let (_dir, base) = root();
    let mut id = identity();
    id.session_epoch = 0;
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
