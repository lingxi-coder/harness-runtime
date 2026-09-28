use super::*;

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
            workspace_root: "/workspace".into(),
        }),
        Some(&unavailable_mobile_linux_capability()),
    )
    .expect("carrier present");
    assert!(!gated.enabled);
}

fn ios_host() -> platform_api::MobileHostEnvironment {
    platform_api::MobileHostEnvironment::new(
        platform_api::MobileHostOs::Ios,
        Some("19.0".into()),
        platform_api::MobileDeviceClass::Phone,
        platform_api::MobileExecutionTarget::PhysicalDevice,
        platform_api::MobileLaunchMode::Interactive,
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
        std::path::PathBuf::from("/workspace/app"),
        vec![std::path::PathBuf::from("/workspace/app")],
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
        platform_api::MobileToolRuntime::Unavailable
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
        guest_path: "/workspace/app".into(),
        read_only: false,
        purpose: mobile_linux_api::MountPurpose::Workspace,
    }];

    assert_eq!(
        model_visible_mobile_cwd(std::path::Path::new("/native/workspace/src"), &mounts, true,)
            .as_deref(),
        Some("/workspace/app/src")
    );
    assert_eq!(
        model_visible_mobile_cwd(std::path::Path::new("/workspace/app/src"), &mounts, true)
            .as_deref(),
        Some("/workspace/app/src")
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
        guest_path: "/workspace/app".into(),
        read_only: false,
        purpose: mobile_linux_api::MountPurpose::Workspace,
    }];
    let provider_mounts = mounts.clone();
    let provider = Arc::new(move |override_cwd: Option<&std::path::Path>| {
        let cwd = override_cwd.unwrap_or_else(|| std::path::Path::new("/native/workspace"));
        model_visible_mobile_cwd(cwd, &provider_mounts, true)
    });
    let environment = platform_api::MobileRuntimeEnvironment::new(
        ios_host(),
        platform_api::MobileToolRuntime::MobileLinuxGuest,
        Some("/workspace/app".into()),
        Some("/bin/sh".into()),
        Some("mobile-linux".into()),
        platform_api::MobileNetworkPolicy::PermissionMediated,
        platform_api::MobileLifecyclePolicy::IosFiniteBackgroundAssertion,
    );
    let renderer = build_mobile_subagent_env_renderer(
        std::path::PathBuf::from(
            "/private/var/mobile/Containers/Data/Application/secret/workspace",
        ),
        Some(&environment),
        provider,
    );

    let base = renderer("claude-opus-4-8[1m]", None);
    assert!(base.contains("Working directory: /workspace/app\n"));
    assert!(!base.contains("/private/var/mobile/Containers/Data/Application/secret"));

    let mapped = renderer(
        "claude-opus-4-8[1m]",
        Some(std::path::Path::new("/native/workspace/src")),
    );
    assert!(mapped.contains("Working directory: /workspace/app/src\n"));
    assert!(!mapped.contains("/native/workspace/src"));

    let unmapped = renderer(
        "claude-opus-4-8[1m]",
        Some(std::path::Path::new("/private/var/mobile/worktree")),
    );
    assert!(unmapped.contains("Working directory: /workspace/app\n"));
    assert!(!unmapped.contains("/private/var/mobile/worktree"));
}

#[test]
fn only_explicit_scheduled_launches_use_headless_prompt_semantics() {
    let mut host = ios_host();
    assert!(mobile_launch_is_interactive(Some(&host)));
    host.launch_mode = platform_api::MobileLaunchMode::Unknown;
    assert!(mobile_launch_is_interactive(Some(&host)));
    assert!(mobile_launch_is_interactive(None));
    host.launch_mode = platform_api::MobileLaunchMode::ScheduledHeadless;
    assert!(!mobile_launch_is_interactive(Some(&host)));
}
