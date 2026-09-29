mod fast_mode_preference_tests {
    use super::super::fast_mode_preference;
    use super::*;

    #[test]
    fn fast_mode_choice_survives_engine_restart_and_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetFastMode { enabled: true })
                .await
                .unwrap();
            assert_eq!(fast_mode_preference::load(&cfg.lingxi_home), Some(true));
            let orchestrator: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert!(orchestrator.fast_mode().await);
        });
        drop(handle);

        let (restarted, _) = build_submit_handle(tmp.path());
        restarted.runtime().block_on(async {
            let orchestrator: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                restarted.inner().orchestrator.clone();
            assert!(orchestrator.fast_mode().await);
            restarted
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            assert!(orchestrator.fast_mode().await);
            restarted
                .submit(ClientCommand::SetFastMode { enabled: false })
                .await
                .unwrap();
            assert_eq!(fast_mode_preference::load(&cfg.lingxi_home), Some(false));
            assert!(!orchestrator.fast_mode().await);
        });
    }

    #[test]
    fn failed_fast_mode_preference_save_rolls_back_the_live_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetFastMode { enabled: true })
                .await
                .unwrap();
            let path = cfg.lingxi_home.join("last-fast-mode.json");
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(handle
                .submit(ClientCommand::SetFastMode { enabled: false })
                .await
                .is_err());
            let orchestrator: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert!(orchestrator.fast_mode().await);
            assert_eq!(fast_mode_preference::load(&cfg.lingxi_home), None);
        });
    }

    #[test]
    fn headless_fast_mode_does_not_inherit_or_overwrite_interactive_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path());
        fast_mode_preference::save(&cfg.lingxi_home, true).unwrap();
        cfg.host_environment = Some(lingxi_core::host::MobileHostEnvironment::new(
            lingxi_core::host::MobileHostOs::Android,
            None,
            lingxi_core::host::MobileDeviceClass::Phone,
            lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
            lingxi_core::host::MobileLaunchMode::ScheduledHeadless,
        ));
        let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
        handle.runtime().block_on(async {
            let orchestrator: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert!(!orchestrator.fast_mode().await);
            handle
                .submit(ClientCommand::SetFastMode { enabled: false })
                .await
                .unwrap();
            assert_eq!(
                fast_mode_preference::load(&test_config(tmp.path()).lingxi_home),
                Some(true)
            );
        });
    }
}
