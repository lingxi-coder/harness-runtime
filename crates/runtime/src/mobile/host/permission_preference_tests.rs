mod permission_preference_tests {
    use super::super::permission_preference;
    use super::*;

    #[test]
    fn permission_preference_success_survives_engine_restart_and_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (handle, listener) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "plan".into(),
                })
                .await
                .unwrap();
            assert_eq!(
                permission_preference::load(&cfg.lingxi_home),
                Some(permission::PermissionMode::Plan)
            );
            assert!(drained(&listener).await.iter().any(|event| matches!(event,
                Ev::PermissionModeChanged { mode } if mode == "plan")));
        });
        drop(handle);

        let (restarted, _) = build_submit_handle(tmp.path());
        restarted.runtime().block_on(async {
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                restarted.inner().orchestrator.clone();
            assert_eq!(
                orchestrator.permission_mode().await.as_deref(),
                Some("plan")
            );
            assert!(orchestrator.plan_mode().await);
            restarted
                .submit(ClientCommand::SetPermissionMode {
                    mode: "acceptEdits".into(),
                })
                .await
                .unwrap();
            restarted
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            assert_eq!(
                orchestrator.permission_mode().await.as_deref(),
                Some("acceptEdits")
            );
            assert!(!orchestrator.plan_mode().await);
            assert_eq!(
                permission_preference::load(&cfg.lingxi_home),
                Some(permission::PermissionMode::AcceptEdits)
            );
        });
    }

    #[test]
    fn permission_preference_latest_choice_overrides_an_old_plan_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let (session_id, _) = seed_replay_valid_session(tmp.path());
        let cfg = test_config(tmp.path());
        let path =
            session::jsonl::session_path(&cfg.lingxi_home, &cfg.cwd.to_string_lossy(), &session_id);
        let mut transcript = std::fs::read_to_string(&path).unwrap();
        transcript.push_str(&format!(
            "{}\n",
            serde_json::json!({
                "type": "permission-mode", "permissionMode": "plan", "sessionId": session_id,
            })
        ));
        std::fs::write(&path, transcript).unwrap();
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "acceptEdits".into(),
                })
                .await
                .unwrap();
            handle
                .submit(ClientCommand::ResumeSession {
                    session_id,
                    cwd: None,
                })
                .await
                .unwrap();
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_eq!(
                orchestrator.permission_mode().await.as_deref(),
                Some("acceptEdits")
            );
            assert!(!orchestrator.plan_mode().await);
            assert_eq!(
                permission_preference::load(&cfg.lingxi_home),
                Some(permission::PermissionMode::AcceptEdits)
            );
        });
    }

    #[test]
    fn permission_preference_rejected_selection_keeps_previous_saved_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
        std::fs::write(
            cfg.lingxi_home.join("settings.json"),
            r#"{"permissions":{"disableBypassPermissionsMode":"disable"}}"#,
        )
        .unwrap();
        let (handle, listener) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "plan".into(),
                })
                .await
                .unwrap();
            listener.received.lock().await.clear();
            assert!(handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "bypassPermissions".into()
                })
                .await
                .is_err());
            assert_eq!(
                permission_preference::load(&cfg.lingxi_home),
                Some(permission::PermissionMode::Plan)
            );
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_eq!(
                orchestrator.permission_mode().await.as_deref(),
                Some("plan")
            );
            assert!(orchestrator.plan_mode().await);
            assert!(!drained(&listener)
                .await
                .iter()
                .any(|event| matches!(event, Ev::PermissionModeChanged { .. })));
        });
    }

    #[test]
    fn permission_preference_save_failure_rolls_back_without_success_event() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (handle, listener) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "plan".into(),
                })
                .await
                .unwrap();
            let path = cfg.lingxi_home.join("last-permission-mode.json");
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            listener.received.lock().await.clear();
            assert!(handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "acceptEdits".into()
                })
                .await
                .is_err());
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_eq!(
                orchestrator.permission_mode().await.as_deref(),
                Some("plan")
            );
            assert!(orchestrator.plan_mode().await);
            assert!(!drained(&listener)
                .await
                .iter()
                .any(|event| matches!(event, Ev::PermissionModeChanged { .. })));
        });
    }
    #[test]
    fn permission_preference_default_and_dont_ask_survive_real_restart() {
        for mode in ["default", "dontAsk"] {
            let tmp = tempfile::tempdir().unwrap();
            let (handle, _) = build_submit_handle(tmp.path());
            handle.runtime().block_on(async {
                handle
                    .submit(ClientCommand::SetPermissionMode { mode: mode.into() })
                    .await
                    .unwrap();
            });
            drop(handle);
            let (restarted, _) = build_submit_handle(tmp.path());
            restarted.runtime().block_on(async {
                let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                    restarted.inner().orchestrator.clone();
                assert_eq!(orchestrator.permission_mode().await.as_deref(), Some(mode));
            });
        }
    }

    #[test]
    fn permission_preference_disabled_bypass_does_not_block_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        permission_preference::save(&cfg.lingxi_home, "bypassPermissions").unwrap();
        std::fs::write(
            cfg.lingxi_home.join("settings.json"),
            r#"{"permissions":{"disableBypassPermissionsMode":"disable"}}"#,
        )
        .unwrap();
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_ne!(
                orchestrator.permission_mode().await.as_deref(),
                Some("bypassPermissions")
            );
            handle
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            assert_ne!(
                orchestrator.permission_mode().await.as_deref(),
                Some("bypassPermissions")
            );
            assert!(handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "bypassPermissions".into()
                })
                .await
                .is_err());
        });
    }
    #[test]
    fn permission_preference_headless_neither_inherits_nor_overwrites_user_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path());
        permission_preference::save(&cfg.lingxi_home, "plan").unwrap();
        cfg.host_environment = Some(platform_api::MobileHostEnvironment::new(
            platform_api::MobileHostOs::Ios,
            None,
            platform_api::MobileDeviceClass::Phone,
            platform_api::MobileExecutionTarget::PhysicalDevice,
            platform_api::MobileLaunchMode::ScheduledHeadless,
        ));
        let home = cfg.lingxi_home.clone();
        let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
        handle.runtime().block_on(async {
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_ne!(
                orchestrator.permission_mode().await.as_deref(),
                Some("plan")
            );
            assert!(!orchestrator.plan_mode().await);
            handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "acceptEdits".into(),
                })
                .await
                .unwrap();
            assert_eq!(
                orchestrator.permission_mode().await.as_deref(),
                Some("acceptEdits")
            );
            assert!(!orchestrator.plan_mode().await);
            assert_eq!(
                permission_preference::load(&home),
                Some(permission::PermissionMode::Plan)
            );
            handle
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            assert_ne!(
                orchestrator.permission_mode().await.as_deref(),
                Some("plan")
            );
            assert!(!orchestrator.plan_mode().await);
            assert_eq!(
                permission_preference::load(&home),
                Some(permission::PermissionMode::Plan)
            );
        });
    }
    #[test]
    fn permission_preference_failed_save_preserves_requested_and_effective_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        std::fs::create_dir_all(&cfg.lingxi_home).unwrap();
        std::fs::write(
            cfg.lingxi_home.join("settings.json"),
            r#"{"disableAutoMode":"disable"}"#,
        )
        .unwrap();
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            let previous_effective = orchestrator.permission_mode().await;
            let previous_requested = handle
                .inner()
                .requested_permission_mode
                .lock()
                .unwrap()
                .clone();
            assert_eq!(previous_requested, "auto");
            assert_eq!(previous_effective.as_deref(), Some("default"));
            std::fs::create_dir_all(cfg.lingxi_home.join("last-permission-mode.json")).unwrap();
            assert!(handle
                .submit(ClientCommand::SetPermissionMode {
                    mode: "plan".into()
                })
                .await
                .is_err());
            assert_eq!(orchestrator.permission_mode().await, previous_effective);
            assert_eq!(
                *handle.inner().requested_permission_mode.lock().unwrap(),
                previous_requested
            );
            assert!(!orchestrator.plan_mode().await);
        });
    }
}
