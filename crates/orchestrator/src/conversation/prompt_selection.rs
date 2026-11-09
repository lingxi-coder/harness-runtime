use super::ConversationOrchestrator;
use lingxi_core::host::prompt_policy::{resolve_prompt_policy, ResolvedPromptPolicy};
use std::sync::Arc;

tokio::task_local! {
    pub(crate) static PROMPT_POLICY_OVERRIDE: (crate::query_model::ModelRoute, Arc<ResolvedPromptPolicy>);
}

#[cfg(test)]
#[path = "prompt_selection_mod_tests.rs"]
mod physical_request_tests;

impl ConversationOrchestrator {
    pub(crate) async fn prompt_policy_for_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<Arc<ResolvedPromptPolicy>, lingxi_core::settings::SettingsError> {
        let mode = self.conversation_profile().await.mode;
        self.prompt_policy_for_mode_route(model, profile, mode)
            .await
    }

    pub(crate) async fn prompt_policy_for_mode_route(
        &self,
        model: &str,
        profile: Option<&str>,
        mode: lingxi_core::host::session_context::ConversationMode,
    ) -> Result<Arc<ResolvedPromptPolicy>, lingxi_core::settings::SettingsError> {
        let resolve = || async {
            let target = self
                .api
                .prompt_target(model, profile, mode)
                .map_err(|error| {
                    Self::source_settings_failure("prompt_route", error.to_string())
                })?;
            let policy = Arc::new(resolve_prompt_policy(target));
            tracing::debug!(
                provider = %policy.target.provider_id,
                model = %policy.target.canonical_model,
                mode = %mode.as_str(),
                bundle = %policy.bundle_id,
                version = %policy.bundle_version,
                rule = %policy.matched_rule,
                "resolved conversation prompt policy",
            );
            Ok::<_, lingxi_core::settings::SettingsError>(policy)
        };
        let policy = match crate::request_context::prompt_policy_cell(model, profile, mode) {
            Some(cell) => cell.get_or_try_init(resolve).await.cloned(),
            None => resolve().await,
        }?;
        crate::request_context::seed_prompt_policy_alias(&policy);
        Ok(policy)
    }

    pub(crate) async fn current_prompt_policy(
        &self,
    ) -> Result<Arc<ResolvedPromptPolicy>, lingxi_core::settings::SettingsError> {
        let (model, profile) = self.current_prompt_route().await;
        if let Ok(Some(policy)) = PROMPT_POLICY_OVERRIDE.try_with(|(route, policy)| {
            (route.model == model && route.profile == profile).then(|| Arc::clone(policy))
        }) {
            return Ok(policy);
        }
        let policy = self
            .prompt_policy_for_route(&model, profile.as_deref())
            .await?;
        self.reconcile_prompt_snapshot_policy(&policy).await;
        Ok(policy)
    }

    pub(crate) async fn current_prompt_uses_lean(&self) -> bool {
        self.current_prompt_policy()
            .await
            .map(|policy| crate::prompt::prompt_policy_is_lean(&policy))
            .unwrap_or(true)
    }

    pub(crate) async fn reconcile_prompt_snapshot_policy(&self, policy: &ResolvedPromptPolicy) {
        let expected = policy.metadata();
        let stale = {
            let mut snapshot = self.prompt_runtime.prompt_snapshot.lock().await;
            let stale = snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.prompt_policy.as_ref() != Some(&expected));
            if stale {
                if snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.append_system_prompt.is_some())
                {
                    self.prompt_runtime
                        .restore_prompt_snapshot_append_source(snapshot.as_ref());
                }
                *snapshot = None;
            }
            stale
        };
        if stale {
            *self.prompt_runtime.frozen_prompt_source_vector.lock().await = None;
            *self
                .prompt_runtime
                .pending_prompt_source_vector
                .lock()
                .await = None;
            *self.prompt_runtime.wire_tool_schema_cache.lock().await = None;
            self.prompt_runtime
                .prompt_snapshot_resume
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }

    pub(crate) async fn system_prompt_for_policy(
        &self,
        policy: &ResolvedPromptPolicy,
        prior: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    ) -> Result<
        Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        lingxi_core::settings::SettingsError,
    > {
        let fingerprint = policy.fingerprint();
        if prior.is_some()
            && crate::request_context::system_prompt_policy().as_deref()
                == Some(fingerprint.as_str())
        {
            return Ok(prior.cloned());
        }
        crate::turn_loop::boxed_turn_future(|| self.provider_system_prompt())
            .await
            .map(Some)
    }
}

#[derive(Clone)]
pub(crate) struct ReselectedPromptComponents {
    pub(crate) route: crate::query_model::ModelRoute,
    pub(crate) policy: Arc<ResolvedPromptPolicy>,
    pub(crate) system:
        Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    pub(crate) tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    pub(crate) available_tool_names:
        Option<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
    pub(crate) skip_global_cache: bool,
    pub(crate) request_history: Option<Vec<lingxi_core::types::ConversationMessage>>,
    pub(crate) reminders: super::PreparedReminders,
}

impl ConversationOrchestrator {
    /// A physical Mod request owns one route, policy, and exact source snapshot.
    pub(crate) async fn reselect_prompt_components(
        &self,
        previous: ReselectedPromptComponents,
        route: crate::query_model::ModelRoute,
    ) -> Result<ReselectedPromptComponents, lingxi_core::settings::SettingsError> {
        match crate::request_context::scope_if_missing(
            self,
            crate::turn_loop::boxed_turn_future(|| async {
                self.reselect_prompt_components_inner(previous, route)
                    .await
                    .map_err(crate::error::OrchestratorError::Settings)
            }),
        )
        .await
        {
            Ok(components) => Ok(components),
            Err(crate::error::OrchestratorError::Settings(error)) => Err(error),
            Err(error) => Err(Self::source_settings_failure(
                "prompt_components",
                error.to_string(),
            )),
        }
    }

    async fn reselect_prompt_components_inner(
        &self,
        previous: ReselectedPromptComponents,
        route: crate::query_model::ModelRoute,
    ) -> Result<ReselectedPromptComponents, lingxi_core::settings::SettingsError> {
        use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;

        let policy = if previous.route == route {
            Arc::clone(&previous.policy)
        } else {
            self.prompt_policy_for_mode_route(
                &route.model,
                route.profile.as_deref(),
                previous.policy.target.mode,
            )
            .await?
        };
        crate::query_model::ROUTE
            .scope(
                Some(route.clone()),
                PROMPT_POLICY_OVERRIDE.scope(
                    (route.clone(), Arc::clone(&policy)),
                    crate::turn_loop::boxed_turn_future(|| async {
                        let policy = self.current_prompt_policy().await?;
                        let components = if previous.policy.fingerprint() == policy.fingerprint() {
                            ReselectedPromptComponents {
                                route,
                                policy,
                                ..previous
                            }
                        } else {
                            self.reconcile_prompt_snapshot_policy(&policy).await;
                            let system = match previous.system {
                                Some(custom @ SystemPromptInput::CustomPrompt { .. })
                                | Some(custom @ SystemPromptInput::NativeCustomPrompt { .. }) => {
                                    Some(custom)
                                }
                                _ => Some(self.provider_system_prompt().await?),
                            };
                            let (tools, skip_global_cache, _, available_tool_names) =
                                self.build_wire_tools_with_complete().await?;
                            let reminders = self
                                .rerender_prompt_reminders(&previous.reminders, &policy, &tools)
                                .await;
                            let mut request_history = previous.request_history;
                            if let Some(history) = &mut request_history {
                                previous.reminders.reproject(history, &reminders);
                                self.project_dynamic_memory_context(history).await;
                            }
                            if matches!(system, Some(SystemPromptInput::SourceVector { .. })) {
                                self.record_prompt_snapshot_if_needed(system.as_ref(), &tools)
                                    .await;
                            }
                            ReselectedPromptComponents {
                                route,
                                policy,
                                system,
                                tools,
                                available_tool_names: Some(available_tool_names),
                                skip_global_cache,
                                request_history,
                                reminders,
                            }
                        };
                        // Preserve the caller's typed custom source even when a new
                        // bundle supplies the tools; no display/UTF-16 round trip.
                        self.save_current_turn_system_prompt(components.system.as_ref())
                            .await;
                        crate::request_context::set_system_prompt_policy(
                            components.policy.fingerprint(),
                        );
                        Ok(components)
                    }),
                ),
            )
            .await
    }
}
