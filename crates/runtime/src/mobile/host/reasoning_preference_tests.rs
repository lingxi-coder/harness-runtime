mod reasoning_preference_tests {
    use super::*;
    use client::protocol::controls::ReasoningSelectionDto;

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
                command_api::builtins::effort::load_reasoning_default_selection_at(
                    &cfg.lingxi_home.join("settings.json")
                ),
                Some(lingxi_core::host::ReasoningSelection::Level { id: "high".into() })
            );
            let orchestrator: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                handle.inner().orchestrator.clone();
            assert_eq!(
                orchestrator
                    .conversation_controls()
                    .await
                    .unwrap()
                    .requested_reasoning_selection,
                lingxi_core::host::ReasoningSelection::Level { id: "high".into() }
            );
        });
        drop(handle);

        let (restarted, _) = build_submit_handle(tmp.path());
        restarted.runtime().block_on(async {
            let orchestrator: Arc<dyn lingxi_core::host::OrchestratorHandle> =
                restarted.inner().orchestrator.clone();
            assert_eq!(
                orchestrator
                    .conversation_controls()
                    .await
                    .unwrap()
                    .requested_reasoning_selection,
                lingxi_core::host::ReasoningSelection::Level { id: "high".into() }
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
                lingxi_core::host::ReasoningSelection::Level { id: "high".into() }
            );
        });
    }
}
