//! Feed native helper-model policy with live host route/catalog/settings facts.
use super::*;
use llm_runtime::model::classifier_model_policy::{self as policy, ClassifierModelRestriction};
use serde_json::{Value, json};
use std::collections::BTreeMap;

impl ConversationOrchestrator {
    pub(super) async fn background_classifier_model(
        &self,
        main: &str,
        profile: Option<&str>,
        small: bool,
    ) -> (String, Option<String>) {
        let owned_profile = profile.map(str::to_owned);
        if !small {
            return (main.to_owned(), owned_profile);
        }
        let facts = self
            .api
            .classifier_model_route_facts(main, profile)
            .await
            .unwrap_or_default();
        let context = self
            .model_resolution_context_provider
            .as_ref()
            .and_then(|resolver| resolver.context_for_route(main, profile).ok());
        let (managed, merged) = if let Some(reader) = &self.mod_settings_reader {
            let Ok(managed) = reader.read(json!({"source":"policy"})).await else {
                return (main.into(), owned_profile);
            };
            let Ok(merged) = reader.read(json!({})).await else {
                return (main.into(), owned_profile);
            };
            (managed, merged)
        } else {
            (json!({}), json!({}))
        };
        let strings = |value: Option<&Value>| {
            value
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let denied = strings(managed.get("deniedModels"));
        let overrides: BTreeMap<String, String> = managed
            .get("modelOverrides")
            .and_then(Value::as_object)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.into())))
                    .collect()
            })
            .unwrap_or_default();
        let exact = managed.get("availableModelsMatch").and_then(Value::as_str) == Some("exact");
        let allowed = strings(managed.get("availableModels"));
        let resolve = |model: &str| {
            if let (Some(resolver), Some(context)) =
                (&self.model_resolution_context_provider, &context)
            {
                if let Ok(selection) = agent::model_resolution::resolve_user_model_selection(
                    model,
                    None,
                    context,
                    resolver.as_ref(),
                ) {
                    return (selection.model, selection.model_profile);
                }
            }
            match self.api.resolve_media_route(model, profile) {
                Ok(route) => (route.main.request_model, Some(route.main.profile_name)),
                Err(_) => (model.to_owned(), owned_profile.clone()),
            }
        };
        let env = |name| std::env::var(name).ok(); // presence, including empty, is significant
        let default_haiku_override = env("ANTHROPIC_DEFAULT_HAIKU_MODEL");
        let haiku = default_haiku_override.clone().or_else(|| {
            context
                .as_ref()
                .and_then(|context| context.family_defaults.haiku.clone())
        });
        let sonnet_override = env("ANTHROPIC_DEFAULT_SONNET_MODEL");
        let sonnet = sonnet_override.clone().or_else(|| {
            context
                .as_ref()
                .and_then(|context| context.family_defaults.sonnet.clone())
        });
        let available = |model: &str| {
            let resolved = resolve(model).0;
            !policy::classifier_model_denied(&resolved, &denied, &overrides)
                && llm_runtime::model::allowlist::is_model_allowed(
                    &resolved,
                    merged
                        .get("availableModels")
                        .and_then(Value::as_array)
                        .map(|_| strings(merged.get("availableModels")))
                        .as_deref(),
                    Some(&overrides),
                )
        };
        let mut haiku_available = haiku.as_deref().is_some_and(available);
        let mut sonnet_available = sonnet.as_deref().is_some_and(available);
        if let Some(reader) = &self.mod_settings_reader {
            if let Some(model) = &haiku {
                haiku_available &= matches!(
                    reader.model_allowed(&resolve(model).0).await,
                    Ok(None | Some(true))
                );
            }
            if let Some(model) = &sonnet {
                sonnet_available &= matches!(
                    reader.model_allowed(&resolve(model).0).await,
                    Ok(None | Some(true))
                );
            }
        }
        let default_env = env("ANTHROPIC_DEFAULT_MODEL");
        let default_model_from_env = self.config.classifier_model_default_env
            || if let Some(model) = default_env.as_deref() {
                let normalized = llm_runtime::model::allowlist::normalize(model);
                !matches!(
                    normalized.as_str(),
                    "default" | "inherit" | "haiku" | "opusplan"
                ) && merged
                    .get("enforceAvailableModels")
                    .and_then(Value::as_bool)
                    != Some(true)
                    && managed
                        .get("enforceAvailableModels")
                        .and_then(Value::as_bool)
                        != Some(true)
                    && available(model)
                    && resolve(model).0 == main
            } else {
                false
            };
        let input = policy::ClassifierModelSelection {
            route: facts,
            main_model: main.into(),
            small_fast_override: env("ANTHROPIC_SMALL_FAST_MODEL"),
            default_haiku_override,
            haiku_model: haiku,
            haiku_available,
            sonnet_model: sonnet,
            sonnet_available,
            explicit_model: self.config.classifier_model_explicit.unwrap_or(true)
                || merged.get("model").is_some_and(|value| !value.is_null())
                || env("ANTHROPIC_MODEL").is_some_and(|model| !model.is_empty()),
            default_model_from_env,
            opus_override: env("ANTHROPIC_DEFAULT_OPUS_MODEL")
                .is_some_and(|value| Some(value) != env("CLAUDE_CODE_3P_PROBE_WROTE_OPUS_DEFAULT")),
            sonnet_override: sonnet_override.is_some_and(|value| {
                Some(value) != env("CLAUDE_CODE_3P_PROBE_WROTE_SONNET_DEFAULT")
            }),
        };
        let (candidate, restriction) = policy::select_classifier_model(&input);
        let selection = resolve(&candidate);
        if restriction != ClassifierModelRestriction::None {
            let denied = policy::classifier_model_denied(&candidate, &denied, &overrides)
                || policy::classifier_model_denied(&selection.0, &denied, &overrides);
            let disallowed = restriction == ClassifierModelRestriction::DeniedOrExactAllowlist
                && exact
                && !policy::classifier_model_exact_allowed(&selection.0, &allowed, &overrides);
            if denied || disallowed {
                return (main.to_owned(), owned_profile);
            }
        }
        selection
    }
}
