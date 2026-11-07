use super::*;
use client::adapter::PermissionRequestSink;
use mobile_linux_api::{MobileLinuxCapability, MobileLinuxRuntimeMode};
use tool_api::SessionCwd;

fn unavailable_mobile_linux_capability() -> MobileLinuxCapability {
    MobileLinuxCapability {
        available: false,
        backend: mobile_linux_api::SandboxBackend::IosIsh,
        mode: MobileLinuxRuntimeMode::MobileLinux,
        reason: Some("runtime unavailable".into()),
        streaming_output: false,
        background_processes: false,
        pty: false,
        bind_mounts: false,
        rootfs_integrity: false,
    }
}

#[test]
fn mode_default_permission_root_requires_the_matching_host_and_guest_owner() {
    use session::jsonl::SessionMode;
    let temp = tempfile::tempdir().unwrap();
    let dirs =
        crate::mobile::mobile_user_directories(temp.path().join("runtime").display().to_string())
            .unwrap();
    dirs.prepare().unwrap();
    let mut cfg = crate::mobile::test_support::test_config(std::path::Path::new(&dirs.chat_cwd));
    cfg.lingxi_home = std::path::Path::new(&dirs.config_home)
        .canonicalize()
        .unwrap();
    cfg.logical_cwd = Some(dirs.chat_guest_cwd.clone().into());
    cfg.session_mode = SessionMode::Chat;
    assert!(super::assembly::is_bound_mode_default_cwd(&cfg, &cfg.cwd));
    cfg.session_mode = SessionMode::Code;
    assert!(!super::assembly::is_bound_mode_default_cwd(&cfg, &cfg.cwd));
    cfg.session_mode = SessionMode::Chat;
    cfg.logical_cwd = Some(dirs.code_guest_cwd.clone().into());
    assert!(!super::assembly::is_bound_mode_default_cwd(&cfg, &cfg.cwd));
    cfg.logical_cwd = Some(dirs.chat_guest_cwd.clone().into());
    cfg.app_agent_host = true;
    assert!(!super::assembly::is_bound_mode_default_cwd(&cfg, &cfg.cwd));
}

#[tokio::test]
async fn mode_default_boot_policy_and_deny_globs_use_its_owner_cwd() {
    use crate::mobile::test_support::{
        test_config, CollectingPermissionSink, FakeListener, HostFakePlatform, MountTableRuntime,
    };
    use mobile_linux_api::{MobileLinuxRuntime, MountPurpose, MountSpec};
    use session::jsonl::SessionMode;

    for (mode, component, scheduler) in [
        (SessionMode::Chat, "chat", false),
        (SessionMode::Code, "code", false),
        (SessionMode::Code, "scheduled", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let dirs = crate::mobile::mobile_user_directories(
            temp.path().join("runtime").display().to_string(),
        )
        .unwrap();
        dirs.prepare().unwrap();
        let cwd = branding::workspace_home(std::path::Path::new(&dirs.config_home)).join(component);
        let guest =
            branding::workspace_home(std::path::Path::new(&dirs.config_guest_path)).join(component);
        let settings = cwd.join(branding::DOT_DIR);
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("settings.json"),
            r#"{"permissions":{"deny":["Read(/private/**)"]}}"#,
        )
        .unwrap();

        let linux = Arc::new(MountTableRuntime::default());
        linux
            .configure_mounts(vec![
                MountSpec {
                    host_path: dirs.projects.clone().into(),
                    guest_path: dirs.projects_guest_path.clone(),
                    read_only: false,
                    purpose: MountPurpose::Workspace,
                },
                MountSpec {
                    host_path: dirs.home.clone().into(),
                    guest_path: dirs.guest_home.clone(),
                    read_only: false,
                    purpose: MountPurpose::External,
                },
            ])
            .await
            .unwrap();
        let platform: Arc<dyn Platform> =
            Arc::new(HostFakePlatform::new(temp.path().to_path_buf()).with_mobile_linux(linux));
        let listener: Arc<dyn ClientEventListener> = Arc::new(FakeListener::default());
        let sink: Arc<dyn PermissionRequestSink> = Arc::new(CollectingPermissionSink::default());
        let mut cfg = test_config(&cwd);
        cfg.lingxi_home = dirs.config_home.clone().into();
        cfg.logical_cwd = Some(guest);
        cfg.session_mode = mode;
        cfg.scheduler_controller = scheduler;
        let runtime = build_mobile(cfg, platform, listener, sink).await.unwrap();
        let context = &runtime.mcp_tool_context;
        let expected_cwd = cwd.canonicalize().unwrap();
        assert_eq!(
            context.permission_policy.roots.as_ref().unwrap().cwd,
            expected_cwd
        );
        assert_eq!(
            context.read_deny_exclude_globs,
            vec!["/private/**".to_string()]
        );
        let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"file_path": expected_cwd.join("private/secret.txt").display().to_string()}),
        ).unwrap();
        assert!(matches!(
            context.permission_policy.authorize("Read", &input),
            permission::PermissionResult::Deny { .. }
        ));
    }
}

#[test]
fn mobile_shell_gate_disables_selected_but_unavailable_runtime() {
    let gated = gate_mobile_shell_ctx(
        Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        )),
        Some(&unavailable_mobile_linux_capability()),
    )
    .expect("carrier present");
    assert!(!gated.enabled);
}

#[test]
fn mobile_git_gate_disables_selected_but_unavailable_runtime() {
    let gated = gate_mobile_git_ctx(
        Some(tool_api::MobileGitToolCtx {
            enabled: true,
            has_token: true,
            workspace_root: "/home/lingxi/lingxi".into(),
        }),
        Some(&unavailable_mobile_linux_capability()),
    )
    .expect("carrier present");
    assert!(!gated.enabled);
}

fn ios_host() -> lingxi_core::host::MobileHostEnvironment {
    lingxi_core::host::MobileHostEnvironment::new(
        lingxi_core::host::MobileHostOs::Ios,
        Some("19.0".into()),
        lingxi_core::host::MobileDeviceClass::Phone,
        lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
        lingxi_core::host::MobileLaunchMode::Interactive,
    )
}

#[test]
fn unavailable_gated_shell_is_not_described_as_mobile_linux() {
    let gated = gate_mobile_shell_ctx(
        Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
            true,
            vec!["sh".into()],
            None,
        )),
        Some(&unavailable_mobile_linux_capability()),
    )
    .expect("carrier retained for registration gate");
    let session_cwd = SessionCwd::new(
        std::path::PathBuf::from("/home/lingxi/lingxi/app"),
        vec![std::path::PathBuf::from("/home/lingxi/lingxi/app")],
    );

    let environment = build_mobile_runtime_environment(
        Some(&ios_host()),
        Some(&gated),
        Some(&unavailable_mobile_linux_capability()),
        &session_cwd,
    )
    .expect("host context");

    assert_eq!(
        environment.tool_runtime,
        lingxi_core::host::MobileToolRuntime::Unavailable
    );
    assert_eq!(environment.guest_cwd(), None);
    let reminder = environment.render_body();
    assert!(reminder.contains("Shell runtime: unavailable"));
    assert!(!reminder.contains("/bin/sh"));
}

#[test]
fn workspace_prompt_paths_are_guest_only() {
    let mounts = [mobile_linux_api::MountSpec {
        host_path: std::path::PathBuf::from("/native/workspace"),
        guest_path: "/home/lingxi/lingxi/app".into(),
        read_only: false,
        purpose: mobile_linux_api::MountPurpose::Workspace,
    }];

    assert_eq!(
        model_visible_mobile_cwd(std::path::Path::new("/native/workspace/src"), &mounts, true,)
            .as_deref(),
        Some("/home/lingxi/lingxi/app/src")
    );
    assert_eq!(
        model_visible_mobile_cwd(
            std::path::Path::new("/home/lingxi/lingxi/app/src"),
            &mounts,
            true
        )
        .as_deref(),
        Some("/home/lingxi/lingxi/app/src")
    );
    assert_eq!(
        model_visible_mobile_cwd(
            std::path::Path::new("/private/var/mobile/worktree"),
            &mounts,
            true,
        ),
        None
    );
    assert_eq!(
        model_visible_mobile_cwd(
            std::path::Path::new("/native/workspace/src"),
            &mounts,
            false,
        ),
        None
    );
}

#[test]
fn mobile_subagent_env_renderer_uses_guest_paths_only() {
    let mounts = vec![mobile_linux_api::MountSpec {
        host_path: std::path::PathBuf::from("/native/workspace"),
        guest_path: "/home/lingxi/lingxi/app".into(),
        read_only: false,
        purpose: mobile_linux_api::MountPurpose::Workspace,
    }];
    let provider_mounts = mounts.clone();
    let provider = Arc::new(move |override_cwd: Option<&std::path::Path>| {
        let cwd = override_cwd.unwrap_or_else(|| std::path::Path::new("/native/workspace"));
        model_visible_mobile_cwd(cwd, &provider_mounts, true)
    });
    let environment = lingxi_core::host::MobileRuntimeEnvironment::new(
        ios_host(),
        lingxi_core::host::MobileToolRuntime::MobileLinuxGuest,
        Some("/home/lingxi/lingxi/app".into()),
        Some("/bin/sh".into()),
        Some("mobile-linux".into()),
        lingxi_core::host::MobileNetworkPolicy::PermissionMediated,
        lingxi_core::host::MobileLifecyclePolicy::IosFiniteBackgroundAssertion,
    );
    let renderer = build_mobile_subagent_env_renderer(
        std::path::PathBuf::from(
            "/private/var/mobile/Containers/Data/Application/secret/workspace",
        ),
        Some(&environment),
        provider,
    );

    let base = renderer("claude-opus-4-8[1m]", None);
    assert!(base.contains("Working directory: /home/lingxi/lingxi/app\n"));
    assert!(!base.contains("/private/var/mobile/Containers/Data/Application/secret"));

    let mapped = renderer(
        "claude-opus-4-8[1m]",
        Some(std::path::Path::new("/native/workspace/src")),
    );
    assert!(mapped.contains("Working directory: /home/lingxi/lingxi/app/src\n"));
    assert!(!mapped.contains("/native/workspace/src"));

    let unmapped = renderer(
        "claude-opus-4-8[1m]",
        Some(std::path::Path::new("/private/var/mobile/worktree")),
    );
    assert!(unmapped.contains("Working directory: /home/lingxi/lingxi/app\n"));
    assert!(!unmapped.contains("/private/var/mobile/worktree"));
}

#[test]
fn only_explicit_scheduled_launches_use_headless_prompt_semantics() {
    let mut host = ios_host();
    assert!(mobile_launch_is_interactive(Some(&host)));
    host.launch_mode = lingxi_core::host::MobileLaunchMode::Unknown;
    assert!(mobile_launch_is_interactive(Some(&host)));
    assert!(mobile_launch_is_interactive(None));
    host.launch_mode = lingxi_core::host::MobileLaunchMode::ScheduledHeadless;
    assert!(!mobile_launch_is_interactive(Some(&host)));
}
