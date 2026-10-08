use super::{DefaultModelSelection, PoolSubagentSpawner};
use crate::definition::AgentModel;
use crate::model_resolution::{ModelResolutionContext, ModelResolutionError, ModelRouteFacts};
use lingxi_core::host::subagent_spawn::{SubagentSpawnError, SubagentSpawnRequest};

fn trim_route_text(value: &str) -> &str {
    lingxi_core::host::effort::trim_js_whitespace(value)
}

impl PoolSubagentSpawner {
    fn unresolved_model_context(model: &str, profile: Option<&str>) -> ModelResolutionContext {
        ModelResolutionContext {
            route: ModelRouteFacts {
                model: model.to_owned(),
                profile: profile.map(str::to_owned),
                ..ModelRouteFacts::default()
            },
            ..ModelResolutionContext::default()
        }
    }

    fn context_for_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<ModelResolutionContext, ModelResolutionError> {
        match self.model_resolution_context_provider.get() {
            Some(provider) => provider.context_for_route(model, profile),
            None => Ok(Self::unresolved_model_context(model, profile)),
        }
    }

    pub(super) fn resolved_default_selection(
        &self,
    ) -> Result<Option<DefaultModelSelection>, ModelResolutionError> {
        // A live source is authoritative. If its route is temporarily
        // unavailable, do not silently replace it with a boot-time route.
        if let Some(provider) = self.default_model_selection_provider.get() {
            return provider().map(|selection| {
                selection.filter(|selection| !trim_route_text(&selection.model).is_empty())
            });
        }

        let model = self
            .default_model_provider
            .get()
            .and_then(|provider| provider())
            .filter(|model| !trim_route_text(model).is_empty())
            .or_else(|| self.default_model.clone());
        let Some(model) = model else {
            return Ok(None);
        };
        let context = self.context_for_route(&model, None)?;
        Ok(Some(DefaultModelSelection {
            model,
            model_profile: context.route.profile.clone(),
            model_resolution_context: context,
        }))
    }

    /// The default parent / main-loop model when a spawn has no explicit live
    /// parent override. Errors are intentionally surfaced by the spawn path;
    /// non-critical prompt renderers can use `None` if route facts are absent.
    pub(super) fn resolved_default_model(&self) -> Option<String> {
        self.resolved_default_selection()
            .ok()
            .flatten()
            .map(|selection| selection.model)
    }

    pub(super) fn effective_parent_selection(
        &self,
        request: &SubagentSpawnRequest,
    ) -> Result<Option<DefaultModelSelection>, ModelResolutionError> {
        let live = self.resolved_default_selection()?;
        let parent_model = request
            .parent_model_override
            .as_deref()
            .map(trim_route_text)
            .filter(|model| !model.is_empty());
        let Some(parent_model) = parent_model else {
            return Ok(live);
        };

        // `model_profile` is the immediate parent hint only when no child model
        // was explicitly supplied; otherwise it selects the child route.
        let parent_profile = request
            .model
            .as_deref()
            .map(trim_route_text)
            .filter(|model| !model.is_empty())
            .is_none()
            .then(|| request.model_profile.clone())
            .flatten();

        if let Some(selection) = live.filter(|selection| {
            selection.model == parent_model
                && parent_profile
                    .as_ref()
                    .is_none_or(|profile| selection.model_profile.as_ref() == Some(profile))
        }) {
            return Ok(Some(selection));
        }

        let context = self.context_for_route(parent_model, parent_profile.as_deref())?;
        Ok(Some(DefaultModelSelection {
            model: parent_model.to_string(),
            model_profile: context.route.profile.clone().or(parent_profile),
            model_resolution_context: context,
        }))
    }

    pub(super) fn effective_parent_model(&self, request: &SubagentSpawnRequest) -> Option<String> {
        self.effective_parent_selection(request)
            .ok()
            .flatten()
            .map(|selection| selection.model)
    }

    pub(super) fn resolve_model_pref(
        &self,
        model: &AgentModel,
        parent_model: &str,
        context: &ModelResolutionContext,
    ) -> Result<String, ModelResolutionError> {
        match &self.model_restriction {
            Some((enforcement, catalog)) => {
                let restriction = crate::model_resolution::ModelRestriction {
                    enforcement,
                    catalog,
                };
                crate::model_resolution::resolve_agent_model_restricted_with_context(
                    model,
                    parent_model,
                    self.permission_mode,
                    self.model_setting.as_deref(),
                    Some(restriction),
                    context,
                    &mut |message| tracing::warn!("{message}"),
                )
            }
            None => crate::model_resolution::resolve_agent_model_with_context(
                model,
                parent_model,
                self.permission_mode,
                self.model_setting.as_deref(),
                context,
            ),
        }
    }

    /// Apply the managed model allowlist to a provider-qualified concrete id
    /// without running it through Claude-family alias or Bedrock-prefix logic.
    /// Returns `false` when the requested provider/model was rejected and the
    /// permitted parent model had to be inherited instead.
    pub(super) fn resolve_provider_model_pref(
        &self,
        model: &str,
        parent_model: Option<&str>,
        context: Option<&ModelResolutionContext>,
    ) -> Result<(String, bool), SubagentSpawnError> {
        let barred = self
            .model_restriction
            .as_ref()
            .is_some_and(|(enforcement, _)| {
                llm_runtime::model::allowlist::model_allowed_under(enforcement, model)
                    == Some(false)
            });
        if !barred {
            return Ok((model.to_string(), true));
        }

        tracing::warn!(
            "Subagent model \"{model}{}",
            llm_runtime::model::allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
        );
        if let Some(parent_model) = parent_model {
            let Some(context) = context else {
                return Err(SubagentSpawnError::Runtime(
                    "model route context is unavailable for the selected parent route".into(),
                ));
            };
            let resolved = self
                .resolve_model_pref(&AgentModel::Inherit, parent_model, context)
                .map_err(|error| SubagentSpawnError::Runtime(error.to_string()))?;
            Ok((resolved, false))
        } else {
            Err(SubagentSpawnError::Runtime(format!(
                "subagent model {model:?} is not permitted and no parent model is available"
            )))
        }
    }
}
