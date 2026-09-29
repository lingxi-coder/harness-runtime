mod model_preference_tests {
    use super::super::model_preference;
    use super::*;

    async fn selected_model(handle: &MobileEngineHandle) -> String {
        let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> = handle.inner().orchestrator.clone();
        let snapshot = orch.get_status_snapshot().await;
        lingxi_core::host::qualified_model_ref(&snapshot.model, snapshot.model_profile.as_deref())
    }

    #[test]
    fn model_preference_success_preserves_duplicate_model_provider_across_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let home = test_config(tmp.path()).lingxi_home;
        for selected in ["github-copilot/gpt-5.6-sol", "openai/gpt-5.6-sol"] {
            let (handle, _) = build_submit_handle(tmp.path());
            handle.runtime().block_on(async {
                handle
                    .submit(ClientCommand::SetModel {
                        model: selected.into(),
                    })
                    .await
                    .unwrap();
                assert_eq!(selected_model(&handle).await, selected);
                assert_eq!(model_preference::load(&home).as_deref(), Some(selected));
            });
            drop(handle);
            let (restarted, _) = build_submit_handle(tmp.path());
            restarted.runtime().block_on(async {
                assert_eq!(selected_model(&restarted).await, selected);
                restarted
                    .submit(ClientCommand::NewSession {
                        cwd: None,
                        model: None,
                    })
                    .await
                    .unwrap();
                assert_eq!(selected_model(&restarted).await, selected);
            });
        }
    }

    #[test]
    fn model_preference_list_and_history_restore_do_not_create_preference() {
        let tmp = tempfile::tempdir().unwrap();
        let home = test_config(tmp.path()).lingxi_home;
        let (session_id, _) = seed_replay_valid_session(tmp.path());
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle.submit(ClientCommand::ListModels).await.unwrap();
            handle
                .submit(ClientCommand::ResumeSession {
                    session_id,
                    cwd: None,
                })
                .await
                .unwrap();
            handle
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            assert!(!home.join("last-model.json").exists());
        });
    }

    #[test]
    fn model_preference_rejected_choice_keeps_saved_and_live_model() {
        let tmp = tempfile::tempdir().unwrap();
        let home = test_config(tmp.path()).lingxi_home;
        let (handle, listener) = build_submit_handle(tmp.path());
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("last-model.json"), "{corrupt preference").unwrap();
        handle.runtime().block_on(async {
            let selected = "github-copilot/gpt-5.6-sol";
            handle
                .submit(ClientCommand::SetModel {
                    model: selected.into(),
                })
                .await
                .unwrap();
            listener.received.lock().await.clear();
            assert!(handle
                .submit(ClientCommand::SetModel {
                    model: "removed-provider/not-a-model".into()
                })
                .await
                .is_err());
            assert_eq!(selected_model(&handle).await, selected);
            assert_eq!(model_preference::load(&home).as_deref(), Some(selected));
            assert!(!drained(&listener)
                .await
                .iter()
                .any(|event| matches!(event, Ev::ModelChanged { .. })));
        });
    }

    #[test]
    fn model_preference_disk_failure_rolls_back_model_and_reasoning() {
        let tmp = tempfile::tempdir().unwrap();
        let home = test_config(tmp.path()).lingxi_home;
        let (handle, listener) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            let selected = "openai/gpt-5.6-sol";
            handle
                .submit(ClientCommand::SetModel {
                    model: selected.into(),
                })
                .await
                .unwrap();
            let orch: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            orch.set_reasoning_selection(lingxi_core::host::ReasoningSelection::Level {
                id: "high".into(),
            })
            .await
            .unwrap();
            let previous = orch
                .conversation_controls()
                .await
                .unwrap()
                .requested_reasoning_selection;
            let path = home.join("last-model.json");
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            listener.received.lock().await.clear();
            let error = handle
                .submit(ClientCommand::SetModel {
                    model: "anthropic/claude-sonnet-4-6".into(),
                })
                .await
                .expect_err("blocked preference destination must fail");
            assert!(
                format!("{error:?}").contains("save model preference failed"),
                "{error:?}"
            );
            assert_eq!(selected_model(&handle).await, selected);
            assert_eq!(
                orch.conversation_controls()
                    .await
                    .unwrap()
                    .requested_reasoning_selection,
                previous
            );
            assert!(path.is_dir());
            assert!(!drained(&listener)
                .await
                .iter()
                .any(|event| matches!(event, Ev::ModelChanged { .. })));
        });
    }

    #[test]
    fn model_preference_headless_neither_inherits_nor_overwrites_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path());
        let home = cfg.lingxi_home.clone();
        model_preference::save(&home, "github-copilot/gpt-5.6-sol").unwrap();
        cfg.default_model = "openai/gpt-5.6-sol".into();
        cfg.host_environment = Some(lingxi_core::host::MobileHostEnvironment::new(
            lingxi_core::host::MobileHostOs::Ios,
            None,
            lingxi_core::host::MobileDeviceClass::Phone,
            lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
            lingxi_core::host::MobileLaunchMode::ScheduledHeadless,
        ));
        let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
        handle.runtime().block_on(async {
            assert_eq!(selected_model(&handle).await, "openai/gpt-5.6-sol");
            handle
                .submit(ClientCommand::SetModel {
                    model: "openai/gpt-5.6-sol".into(),
                })
                .await
                .unwrap();
            handle
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: Some("openai/gpt-5.6-sol".into()),
                })
                .await
                .unwrap();
            assert_eq!(
                model_preference::load(&home).as_deref(),
                Some("github-copilot/gpt-5.6-sol")
            );
        });
    }

    #[test]
    fn model_preference_latest_choice_overrides_old_transcript_and_explicit_new_session_updates_it()
    {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (session_id, _) = seed_replay_valid_session(tmp.path());
        let path =
            session::jsonl::session_path(&cfg.lingxi_home, &cfg.cwd.to_string_lossy(), &session_id);
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
                if record["type"] == "assistant" {
                    record["message"]["model"] = "gpt-5.6-sol".into();
                    record["modelProfile"] = "openai".into();
                }
                format!("{record}\n")
            })
            .collect::<String>();
        std::fs::write(path, body).unwrap();
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetModel {
                    model: "github-copilot/gpt-5.6-sol".into(),
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
            assert_eq!(selected_model(&handle).await, "github-copilot/gpt-5.6-sol");
            assert_eq!(
                model_preference::load(&cfg.lingxi_home).as_deref(),
                Some("github-copilot/gpt-5.6-sol")
            );
            handle
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: Some("openai/gpt-5.6-sol".into()),
                })
                .await
                .unwrap();
            assert_eq!(selected_model(&handle).await, "openai/gpt-5.6-sol");
            assert_eq!(
                model_preference::load(&cfg.lingxi_home).as_deref(),
                Some("openai/gpt-5.6-sol")
            );
        });
    }

    #[test]
    fn model_preference_unavailable_saved_provider_falls_back_to_config_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(tmp.path());
        let home = cfg.lingxi_home.clone();
        let unavailable = "removed-provider/gpt-5.6-sol";
        model_preference::save(&home, unavailable).unwrap();
        cfg.default_model = "openai/gpt-5.6-sol".into();
        let (handle, _) = build_submit_handle_with_config(cfg, tmp.path());
        handle.runtime().block_on(async {
            assert_eq!(selected_model(&handle).await, "openai/gpt-5.6-sol");
            assert_eq!(model_preference::load(&home).as_deref(), Some(unavailable));
        });
    }
    #[test]
    fn model_preference_empty_history_resume_keeps_latest_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            let session_id = handle
                .inner()
                .orchestrator
                .current_session_id()
                .await
                .as_uuid()
                .to_string();
            handle
                .submit(ClientCommand::SetModel {
                    model: "github-copilot/gpt-5.6-sol".into(),
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
            assert_eq!(selected_model(&handle).await, "github-copilot/gpt-5.6-sol");
        });
    }
    #[test]
    fn model_preference_disabled_providers_never_restore_or_accept_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let (session_id, _) = seed_replay_valid_session(tmp.path());
        let mut cfg = test_config(tmp.path());
        let home = cfg.lingxi_home.clone();
        let disabled = "github-copilot/gpt-5.6-sol";
        model_preference::save(&home, disabled).unwrap();
        cfg.routing = Some(serde_json::json!({ "mobileEnabledProfiles": [] }));
        let (handle, listener) = build_submit_handle_with_config(cfg, tmp.path());
        handle.runtime().block_on(async {
            assert_ne!(selected_model(&handle).await, disabled);
            handle
                .submit(ClientCommand::ResumeSession {
                    session_id,
                    cwd: None,
                })
                .await
                .unwrap();
            assert_ne!(selected_model(&handle).await, disabled);
            listener.received.lock().await.clear();
            assert!(handle
                .submit(ClientCommand::SetModel {
                    model: disabled.into()
                })
                .await
                .is_err());
            assert_eq!(model_preference::load(&home).as_deref(), Some(disabled));
            assert!(!drained(&listener)
                .await
                .iter()
                .any(|event| matches!(event, Ev::ModelChanged { .. })));
        });
    }
    #[test]
    fn model_preference_slash_selection_survives_restart_and_only_success_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = test_config(tmp.path()).lingxi_home;
        let (handle, listener) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/model".into(),
                    turn_id: Some(1),
                })
                .await
                .unwrap();
            assert!(!home.join("last-model.json").exists());
            handle
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/model github-copilot/gpt-5.6-sol".into(),
                    turn_id: Some(2),
                })
                .await
                .unwrap();
            assert_eq!(selected_model(&handle).await, "github-copilot/gpt-5.6-sol");
            assert_eq!(
                model_preference::load(&home).as_deref(),
                Some("github-copilot/gpt-5.6-sol")
            );
            let saved = std::fs::read(home.join("last-model.json")).unwrap();
            listener.received.lock().await.clear();
            handle
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/model removed-provider/missing".into(),
                    turn_id: Some(3),
                })
                .await
                .unwrap();
            let events = drained(&listener).await;
            assert!(events.iter().any(|event| matches!(
                event,
                Ev::SlashCommandResult {
                    turn_id: Some(3),
                    is_error: true,
                    ..
                }
            )));
            assert!(!events
                .iter()
                .any(|event| matches!(event, Ev::ModelChanged { .. })));
            handle
                .submit(ClientCommand::RunSlashCommand {
                    raw: "/model".into(),
                    turn_id: Some(4),
                })
                .await
                .unwrap();
            assert_eq!(std::fs::read(home.join("last-model.json")).unwrap(), saved);
        });
        drop(handle);
        let (restarted, _) = build_submit_handle(tmp.path());
        restarted.runtime().block_on(async {
            assert_eq!(
                selected_model(&restarted).await,
                "github-copilot/gpt-5.6-sol"
            );
        });
    }
}
