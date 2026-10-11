use super::{DefaultModelSelection, PoolSubagentSpawner};
use crate::definition::AgentModel;
use crate::model_resolution::{
    is_relative_model_alias, resolve_user_model_selection, ModelResolutionContext,
    ModelResolutionError, ModelRouteFacts, ResolvedModelSelection,
};
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

    pub(super) fn context_for_route(
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
        let parent_model = request
            .parent_model_override
            .as_deref()
            .map(trim_route_text)
            .filter(|model| !model.is_empty());
        let Some(parent_model) = parent_model else {
            return self.resolved_default_selection();
        };

        let parent_profile = request
            .parent_model_profile_override
            .as_deref()
            .map(trim_route_text)
            .filter(|profile| !profile.is_empty());

        // An explicit immediate-parent route is sufficient to resolve the
        // child. The unrelated live root route may have gone away since this
        // nested agent started, so use it only as an optional source of richer
        // profile facts when it still agrees with the explicit override.
        let live = self.resolved_default_selection().ok().flatten();
        if let Some(selection) = live.filter(|selection| {
            selection.model == parent_model
                && parent_profile
                    .is_none_or(|profile| selection.model_profile.as_deref() == Some(profile))
        }) {
            return Ok(Some(selection));
        }

        let context = self.context_for_route(parent_model, parent_profile)?;
        Ok(Some(DefaultModelSelection {
            model: parent_model.to_string(),
            model_profile: context.route.profile.clone(),
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

    /// Choose the effective model once, then retain the route that resolved it.
    /// An unavailable definition default never runs before a caller override.
    pub(super) fn resolve_child_model_selection(
        &self,
        preference: &AgentModel,
        parent: Option<&DefaultModelSelection>,
        model_profile: Option<&str>,
    ) -> Result<Option<ResolvedModelSelection>, ModelResolutionError> {
        let override_model = std::env::var(branding::SUBAGENT_MODEL_ENV)
            .ok()
            .filter(|value| !value.is_empty());
        let selected_preference = override_model
            .as_ref()
            .map(|model| AgentModel::Explicit(model.clone()))
            .unwrap_or_else(|| preference.clone());
        let model_profile = override_model.is_none().then_some(model_profile).flatten();
        let empty_context = ModelResolutionContext::default();
        let context = parent
            .map(|parent| &parent.model_resolution_context)
            .unwrap_or(&empty_context);
        let route_provider =
            |model: &str, profile: Option<&str>| self.context_for_route(model, profile);
        let inherit = || -> Result<Option<ResolvedModelSelection>, ModelResolutionError> {
            let Some(parent) = parent else {
                return Ok(None);
            };
            let model = self.resolve_model_pref(&AgentModel::Inherit, &parent.model, context)?;
            resolve_user_model_selection(
                &model,
                parent.model_profile.as_deref(),
                context,
                &route_provider,
            )
            .map(Some)
        };
        let spec = match &selected_preference {
            AgentModel::Inherit => return inherit(),
            AgentModel::Alias(spec) | AgentModel::Explicit(spec) => spec,
        };
        let barred = |model: &str| {
            self.model_restriction
                .as_ref()
                .is_some_and(|(enforcement, _)| {
                    llm_runtime::model::allowlist::model_allowed_under(enforcement, model)
                        == Some(false)
                })
        };
        let fallback = || {
            tracing::warn!(
                "Subagent model \"{spec}{}",
                llm_runtime::model::allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
            );
            if parent.is_none() {
                return Err(ModelResolutionError::RouteUnavailable {
                    model: spec.clone(),
                    profile: model_profile.map(str::to_owned),
                    reason: "model is not permitted and no parent model is available".into(),
                });
            }
            inherit()
        };
        if model_profile.is_none()
            && is_relative_model_alias(spec)
            && !crate::model_resolution::is_registered_concrete_model(spec, context)
            && crate::model_resolution::has_relative_model_preference(spec, context)
        {
            if let Some(parent) = parent {
                let model =
                    self.resolve_model_pref(&selected_preference, &parent.model, context)?;
                return resolve_user_model_selection(
                    &model,
                    parent.model_profile.as_deref(),
                    context,
                    &route_provider,
                )
                .map(Some);
            }
        }
        let mut selected =
            match resolve_user_model_selection(spec, model_profile, context, &route_provider) {
                Ok(selected) => selected,
                Err(_) if !is_relative_model_alias(spec) && barred(spec) => return fallback(),
                Err(error) => return Err(error),
            };
        if barred(&selected.model) {
            return fallback();
        }
        if override_model.is_none() {
            if let Some(parent) =
                parent.filter(|parent| parent.model_profile == selected.model_profile)
            {
                let model = self.resolve_model_pref(
                    &AgentModel::Explicit(selected.model.clone()),
                    &parent.model,
                    &selected.model_resolution_context,
                )?;
                selected = resolve_user_model_selection(
                    &model,
                    selected.model_profile.as_deref(),
                    &selected.model_resolution_context,
                    &route_provider,
                )?;
            }
        }
        Ok(Some(selected))
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
