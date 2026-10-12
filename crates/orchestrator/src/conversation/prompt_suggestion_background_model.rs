//! Feed native helper-model policy with live host route/catalog/settings facts.
use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use llm_runtime::model::classifier_model_policy::{self as policy, ClassifierModelRestriction};
use std::collections::BTreeMap;

fn empty_settings_object() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("{}").expect("empty settings object is valid JSON")
}

fn settings_request(source: Option<&str>) -> Utf16JsonProjection {
    match source {
        Some(source) => {
            Utf16JsonProjection::parse(source).expect("static settings read request is valid JSON")
        }
        None => empty_settings_object(),
    }
}

fn setting_string(source: &Utf16JsonProjection, pointer: &str) -> Option<String> {
    String::from_utf16(&source.string_units(pointer)?).ok()
}

fn setting_bool(source: &Utf16JsonProjection, pointer: &str) -> Option<bool> {
    source.at_pointer(pointer).and_then(|value| value.as_bool())
}

fn setting_array(source: &Utf16JsonProjection, pointer: &str) -> Option<Utf16JsonProjection> {
    if source.at_pointer(pointer).is_none() {
        return None;
    }
    let value = source
        .subprojection(pointer)
        .unwrap_or_else(|_| Utf16JsonProjection::parse("[]").expect("empty array is JSON"));
    if value.array_len("").is_some() {
        Some(value)
    } else {
        // Native's managed-settings parser normalizes an invalid
        // availableModels container to an explicit empty floor.
        Some(Utf16JsonProjection::parse("[]").expect("empty array is JSON"))
    }
}

fn array_scalar_strings(array: &Utf16JsonProjection) -> Vec<String> {
    let Some(count) = array.array_len("") else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| setting_string(array, &format!("/{index}")))
        .collect()
}

fn setting_strings(source: &Utf16JsonProjection, pointer: &str) -> Vec<String> {
    setting_array(source, pointer)
        .as_ref()
        .map(array_scalar_strings)
        .unwrap_or_default()
}

fn setting_string_map(source: &Utf16JsonProjection, pointer: &str) -> BTreeMap<String, String> {
    let Ok(object) = source.subprojection(pointer) else {
        return BTreeMap::new();
    };
    object
        .object_entries()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let key = String::from_utf16(entry.code_units).ok()?;
            let value = String::from_utf16(entry.value.code_units()?).ok()?;
            Some((key, value))
        })
        .collect()
}

fn exact_setting_string(source: &Utf16JsonProjection, pointer: &str, expected: &str) -> bool {
    source
        .string_units(pointer)
        .is_some_and(|units| units == expected.encode_utf16().collect::<Vec<_>>())
}

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
            let Ok(managed) = reader
                .read(settings_request(Some(r#"{"source":"policy"}"#)))
                .await
            else {
                return (main.into(), owned_profile);
            };
            let Ok(merged) = reader.read(settings_request(None)).await else {
                return (main.into(), owned_profile);
            };
            (managed, merged)
        } else {
            (empty_settings_object(), empty_settings_object())
        };
        let denied = setting_strings(&managed, "/deniedModels");
        let overrides = setting_string_map(&managed, "/modelOverrides");
        let exact = exact_setting_string(&managed, "/availableModelsMatch", "exact");
        let managed_available_models = setting_array(&managed, "/availableModels");
        let allowed = managed_available_models
            .as_ref()
            .map(array_scalar_strings)
            .unwrap_or_default();
        let merged_available_models = setting_array(&merged, "/availableModels");
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
                    merged_available_models.as_ref(),
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
                ) && setting_bool(&merged, "/enforceAvailableModels") != Some(true)
                    && setting_bool(&managed, "/enforceAvailableModels") != Some(true)
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
                || merged
                    .at_pointer("/model")
                    .is_some_and(|value| !value.is_null())
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
