//! Live per-model auto-compaction configuration without per-call disk reads.

use super::*;
use lingxi_core::host::auto_compact::{AutoCompactCommandSnapshot, AutoCompactWindow};

impl ConversationOrchestrator {
    /// Refresh admitted auto-compaction settings following a host settings edit.
    pub fn refresh_auto_compact_settings(
        &self,
        settings: lingxi_core::host::auto_compact::AutoCompactSettings,
    ) {
        self.model_runtime
            .native_model_settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .auto_compact = settings;
    }
    fn auto_compact_settings_key(&self, model: &str, profile: Option<&str>) -> String {
        // Native identity uses the selected SDK profile's aliases and canonical
        // model. Other providers retain a profile-qualified key so equal model
        // names on independent routes never share newly persisted preferences.
        self.api
            .effort_command_snapshot(model, profile)
            .ok()
            .flatten()
            .map(|snapshot| snapshot.settings_key)
            .unwrap_or_else(|| {
                profile.map_or_else(|| model.to_owned(), |profile| format!("{profile}/{model}"))
            })
    }

    pub(crate) fn auto_compact_settings_for(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> (bool, Option<u64>) {
        let key = self.auto_compact_settings_key(model, profile);
        let settings = self
            .model_runtime
            .native_model_settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let enabled =
            compaction::thresholds::is_auto_compact_enabled(settings.auto_compact.enabled());
        let window = settings.auto_compact.window_for(&key, |candidate| {
            self.auto_compact_settings_key(candidate, profile)
        });
        (enabled, window.and_then(AutoCompactWindow::tokens))
    }

    pub(crate) async fn current_auto_compact_command_snapshot(
        &self,
    ) -> Result<AutoCompactCommandSnapshot, lingxi_core::host::HandleError> {
        let (model, profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        let settings_key = self.auto_compact_settings_key(&model, profile.as_deref());
        let native = self
            .api
            .effort_command_snapshot(&model, profile.as_deref())
            .map_err(|error| lingxi_core::host::HandleError::ActionFailed(error.to_string()))?
            .is_some();
        let (enabled, settings_window) = self.auto_compact_settings_for(&model, profile.as_deref());
        let mut resolved = compaction::thresholds::resolve_auto_compact_window(
            &model,
            &self.api.active_betas(),
            settings_window,
            enabled,
        );
        if let Some(model_window) = self.api.context_window_tokens(&model, profile.as_deref()) {
            if !native
                && !matches!(
                    resolved.source,
                    compaction::thresholds::AutoCompactWindowSource::Env
                        | compaction::thresholds::AutoCompactWindowSource::Settings
                )
            {
                resolved.configured = model_window;
            }
            resolved.window = model_window.min(resolved.configured);
        }
        let tuned = compaction::thresholds::resolve_auto_compact_window(
            &model,
            &self.api.active_betas(),
            None,
            enabled,
        );
        let display_name = crate::provider_adapter::display_name_for_model(&model)
            .map(|name| {
                if native {
                    name.strip_prefix("Claude ").unwrap_or(&name).to_owned()
                } else {
                    name
                }
            })
            .unwrap_or_else(|| settings_key.clone());
        Ok(AutoCompactCommandSnapshot {
            settings_key,
            display_name,
            enabled,
            window: resolved.window,
            configured: resolved.configured,
            source: resolved.source.as_str().to_owned(),
            recommend_auto: true,
            override_warning: matches!(
                resolved.source,
                compaction::thresholds::AutoCompactWindowSource::Env
                    | compaction::thresholds::AutoCompactWindowSource::Settings
            ) && resolved.window > tuned.window,
            user_settings_path: self
                .config_home
                .as_ref()
                .map(|home| home.join("settings.json")),
        })
    }

    pub(crate) fn publish_auto_compact_window(
        &self,
        settings_key: &str,
        window: AutoCompactWindow,
    ) {
        self.model_runtime
            .native_model_settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .auto_compact
            .set_user_window(settings_key, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use lingxi_core::host::auto_compact::{AutoCompactSettings, AutoCompactSettingsLayer};
    use lingxi_core::settings::tracer::Source;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    fn fixture(
        settings: &str,
    ) -> (
        ConversationOrchestrator,
        Arc<MockApiClient>,
        Arc<MockOutputStream>,
    ) {
        let api = Arc::new(MockApiClient::new(Vec::new()));
        let output = Arc::new(MockOutputStream::new());
        let orchestrator = ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                model: "compact-settings-model".into(),
                ..Default::default()
            },
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_native_model_settings_snapshot(
            agent::model_resolution::NativeModelSettingsSnapshot {
                auto_compact: AutoCompactSettings {
                    layers: vec![AutoCompactSettingsLayer::from_projection(
                        Source::User,
                        &Utf16JsonProjection::parse(settings).unwrap(),
                    )],
                },
                ..Default::default()
            },
        );
        (orchestrator, api, output)
    }

    #[tokio::test]
    async fn compact_settings_follow_current_profile_and_apply_edits_immediately() {
        let (orchestrator, _, _) = fixture(
            r#"{"autoCompactWindow":200000,"modelSettings":{"a/compact-settings-model":{"autoCompactWindow":500000},"b/compact-settings-model":{"autoCompactWindow":"auto"}}}"#,
        );
        assert_eq!(
            orchestrator.auto_compact_settings_for("compact-settings-model", Some("a")),
            (true, Some(500000))
        );
        assert_eq!(
            orchestrator.auto_compact_settings_for("compact-settings-model", Some("b")),
            (true, None)
        );
        orchestrator
            .publish_auto_compact_window("a/compact-settings-model", AutoCompactWindow::Auto);
        assert_eq!(
            orchestrator.auto_compact_settings_for("compact-settings-model", Some("a")),
            (true, None)
        );
        assert_eq!(
            orchestrator.auto_compact_settings_key("a/compact-settings-model", Some("a")),
            "a/a/compact-settings-model"
        );
    }

    #[tokio::test]
    async fn disabled_auto_compact_skips_proactive_and_reactive_overflow_but_allows_manual() {
        let (orchestrator, api, output) = fixture(r#"{"autoCompactEnabled":false}"#);
        let orchestrator =
            crate::test_support::with_scripted_compactor(orchestrator, "retained summary");
        let mut original = Vec::new();
        for index in 0..40 {
            let message = if index % 2 == 0 {
                ConversationMessage::user(
                    lingxi_core::types::MessageId::new(),
                    "prior user context ".repeat(30),
                )
            } else {
                ConversationMessage::Assistant {
                    per_turn_effort: None,
                    id: lingxi_core::types::MessageId::new(),
                    content: vec![lingxi_core::types::ContentBlock::Text {
                        text: "prior assistant context ".repeat(30),
                        citations: None,
                    }],
                    stop_reason: Some("end_turn".into()),
                    resumable: None,
                }
            };
            original.push(message);
        }
        orchestrator.session.lock().await.history = original.clone();
        orchestrator.maybe_compact_before_call(None).await;
        assert_eq!(orchestrator.session.lock().await.history, original);
        api.set_fail_with(Some(llm_runtime::LlmError::ContextOverflow {
            token_gap: 1000,
        }));
        let _outcome = orchestrator.run_turn("continue").await;
        let history = orchestrator.session.lock().await.history.clone();
        assert_eq!(&history[..original.len()], original.as_slice());
        assert!(!history.iter().any(compaction::is_compact_boundary));
        assert!(output.compaction_phase_snapshot().await.is_empty());
        api.set_fail_with(None);
        orchestrator
            .force_compact_with_cancel(CancellationToken::new())
            .await
            .expect("manual compact remains available");
        assert!(orchestrator
            .session
            .lock()
            .await
            .history
            .iter()
            .any(compaction::is_compact_boundary));
    }
}
