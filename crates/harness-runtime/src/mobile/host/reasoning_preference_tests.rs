mod reasoning_preference_tests {
    use super::*;
    use client_protocol::controls::ReasoningSelectionDto;

    #[test]
    fn reasoning_choice_survives_engine_restart_and_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (handle, _) = build_submit_handle(tmp.path());
        handle.runtime().block_on(async {
            handle
                .submit(ClientCommand::SetReasoningSelection {
                    selection: ReasoningSelectionDto::Level { id: "high".into() },
                })
                .await
                .unwrap();
            assert_eq!(
                command_core::effort::load_reasoning_default_selection_at(
                    &cfg.lingxi_home.join("settings.json")
                ),
                Some(platform_api::ReasoningSelection::Level { id: "high".into() })
            );
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_eq!(
                orchestrator
                    .conversation_controls()
                    .await
                    .unwrap()
                    .requested_reasoning_selection,
                platform_api::ReasoningSelection::Level { id: "high".into() }
            );
        });
        drop(handle);

        let (restarted, _) = build_submit_handle(tmp.path());
        restarted.runtime().block_on(async {
            let orchestrator: Arc<dyn platform_api::OrchestratorHandle> =
                restarted.inner().orchestrator.clone();
            assert_eq!(
                orchestrator
                    .conversation_controls()
                    .await
                    .unwrap()
                    .requested_reasoning_selection,
                platform_api::ReasoningSelection::Level { id: "high".into() }
            );
            restarted
                .submit(ClientCommand::NewSession {
                    cwd: None,
                    model: None,
                })
                .await
                .unwrap();
            assert_eq!(
                orchestrator
                    .conversation_controls()
                    .await
                    .unwrap()
                    .requested_reasoning_selection,
                platform_api::ReasoningSelection::Level { id: "high".into() }
            );
        });
    }
}
