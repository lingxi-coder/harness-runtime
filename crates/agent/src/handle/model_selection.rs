use super::{DefaultModelSelection, PoolSubagentSpawner};
use crate::definition::AgentModel;
use platform_api::subagent_spawn::{SubagentSpawnError, SubagentSpawnRequest};

impl PoolSubagentSpawner {
    pub(super) fn resolve_provider_first_party(&self, profile: Option<&str>) -> Option<bool> {
        profile.and_then(|profile| {
            self.provider_first_party_resolver
                .get()
                .and_then(|resolve| resolve(profile))
        })
    }
    pub(super) fn resolved_default_selection(&self) -> Option<DefaultModelSelection> {
        if let Some(provider) = self.default_model_selection_provider.get() {
            return provider().filter(|selection| !selection.model.trim().is_empty());
        }
        if let Some(provider) = self.default_model_provider.get() {
            if let Some(model) = provider() {
                if !model.trim().is_empty() {
                    return Some(DefaultModelSelection {
                        model,
                        model_profile: None,
                        provider_first_party: self.session_provider_first_party,
                    });
                }
            }
        }
        self.default_model
            .clone()
            .map(|model| DefaultModelSelection {
                model,
                model_profile: None,
                provider_first_party: self.session_provider_first_party,
            })
    }
    /// The effective default parent / main-loop model at spawn time: the LIVE
    /// source ([`Self::default_model_provider`]) when wired and returning a
    /// non-empty value, else the boot snapshot [`Self::default_model`]. This is
    /// the anchor for `AgentModel::Inherit` + family-alias resolution when a spawn
    /// request carries no `parent_model_override` (claude-code `getMainLoopModel()`).
    pub(super) fn resolved_default_model(&self) -> Option<String> {
        self.resolved_default_selection()
            .map(|selection| selection.model)
    }
    pub(super) fn effective_parent_selection(
        &self,
        request: &SubagentSpawnRequest,
    ) -> Option<DefaultModelSelection> {
        let live = self.resolved_default_selection();
        let parent_model = request
            .parent_model_override
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let parent_model = match parent_model {
            Some(model) => model,
            None => return live,
        };
        // `model_profile` is backward-compatible wire storage for two distinct
        // cases. With an explicit request.model it pins the CHILD. Without one
        // it is the immediate PARENT's profile hint threaded by AgentTool.
        let parent_profile = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .is_none()
            .then(|| request.model_profile.clone())
            .flatten();

        // When the override names the live selection, reuse the whole atomic
        // selection, including its authoritative provider classification. This
        // handles arbitrary user profile names without interpreting them.
        if let Some(selection) = live.filter(|selection| {
            selection.model == parent_model
                && parent_profile
                    .as_ref()
                    .is_none_or(|profile| selection.model_profile.as_ref() == Some(profile))
        }) {
            return Some(selection);
        }

        Some(DefaultModelSelection {
            model: parent_model.to_string(),
            model_profile: parent_profile.clone(),
            provider_first_party: self
                .resolve_provider_first_party(parent_profile.as_deref())
                // Legacy serialized requests predate the authoritative bit. The
                // boot session value preserves their old behavior without
                // guessing from a profile name; every new nested path threads it.
                .unwrap_or(self.session_provider_first_party),
        })
    }
    /// The parent / main-loop model this spawn resolves `AgentModel::Inherit` +
    /// bare family aliases against: the request's `parent_model_override` (the
    /// LIVE session model / immediate parent model threaded by `AgentTool`,
    /// claude-code `AgentTool.tsx:418`) when present and non-empty, else the
    /// spawner's own [`Self::resolved_default_model`] (boot/live fallback for the
    /// non-`AgentTool` spawn paths).
    pub(super) fn effective_parent_model(&self, request: &SubagentSpawnRequest) -> Option<String> {
        self.effective_parent_selection(request)
            .map(|selection| selection.model)
    }
    /// Resolve a spawn's model preference to a concrete wire id, applying the
    /// managed `availableModels` restriction when one is wired (subagent
    /// inherit-on-barred + plan-mode upgrade gating, binary `ble`/`RF`). Without a
    /// restriction this is exactly [`crate::model_resolution::resolve_agent_model`]
    /// (byte-identical legacy). Warnings are logged (the binary de-duplicates via a
    /// process-wide `SN` set; a per-spawn `warn!` is an acceptable non-visible
    /// divergence for a log line).
    pub(super) fn resolve_model_pref(&self, model: &AgentModel, parent_model: &str) -> String {
        match &self.model_restriction {
            Some((enforcement, catalog)) => {
                let restriction = crate::model_resolution::ModelRestriction {
                    enforcement,
                    catalog,
                };
                crate::model_resolution::resolve_agent_model_restricted(
                    model,
                    parent_model,
                    self.permission_mode,
                    self.model_setting.as_deref(),
                    Some(restriction),
                    &mut |m| tracing::warn!("{m}"),
                )
            }
            None => crate::model_resolution::resolve_agent_model(
                model,
                parent_model,
                self.permission_mode,
                self.model_setting.as_deref(),
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
        let Some(parent_model) = parent_model else {
            return Err(SubagentSpawnError::Runtime(format!(
                "subagent model {model:?} is not permitted and no parent model is available"
            )));
        };
        Ok((
            self.resolve_model_pref(&AgentModel::Inherit, parent_model),
            false,
        ))
    }
}
