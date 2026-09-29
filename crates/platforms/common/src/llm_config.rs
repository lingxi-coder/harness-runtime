//! Shared built-in Anthropic `ClientConfig` for `ModelRuntime`.
//!
//! Both `engine-desktop` and `engine-mobile` need an identical 10-entry
//! Claude-4-generation model table. Keeping it here (a crate both hosts already
//! depend on) eliminates the duplicate that previously lived in each host and
//! ensures they can never silently drift.
//!
//! ## Model table (`display_model` → `billing_model`)
//!
//! | `display_model`              | `billing_model`      | reasoning |
//! |------------------------------|----------------------|-----------|
//! | claude-sonnet-4-20250514     | claude-sonnet-4      | true      |
//! | claude-sonnet-4-5-20250929   | claude-sonnet-4-5    | true      |
//! | claude-sonnet-4-6            | claude-sonnet-4-6    | true      |
//! | claude-opus-4-20250514       | claude-opus-4        | true      |
//! | claude-opus-4-1-20250805     | claude-opus-4-1      | true      |
//! | claude-opus-4-5-20251101     | claude-opus-4-5      | true      |
//! | claude-opus-4-6              | claude-opus-4-6      | true      |
//! | claude-opus-4-7              | claude-opus-4-7      | true      |  ← orchestrator DEFAULT_MODEL
//! | claude-haiku-4-20250307      | claude-haiku-4       | true      |
//! | claude-haiku-4-5             | claude-haiku-4-5     | true      |
//!
//! All Claude-4-generation models support the Anthropic `thinking` field
//! (`modelSupportsThinking` → true for every non-`claude-3-*` first-party
//! model), so every entry carries `reasoning: true` — the thinking field is sent
//! by default (adaptive where supported, fixed budget otherwise). This gates
//! `llm_runtime::validate_capabilities`'s reasoning check.
//!
//! ## Auth strategies
//!
//! `oauth_path = false` → `ApiKey` + `CredentialConfig::Env { var: "ANTHROPIC_API_KEY" }`
//! `oauth_path = true`  → `OAuthBearer` + `CredentialConfig::HostManaged { id: "anthropic_oauth" }`
//!
//! All models get streaming, tools, vision, documents, and `reasoning: true`
//! (every Claude-4-generation model supports the `thinking` field).

use std::collections::BTreeMap;

/// Parsed routing overrides from the settings `routing` object.
///
/// Returned by [`parse_routing_overrides`].  Hosts pass these into the adapter
/// constructor so the retry/fallback machinery uses the settings-configured
/// values rather than the compile-time defaults.
///
/// ## Fields
///
/// - `fallback`: per-model fallback chain. Key is the **display model** of
///   the primary model (alias-resolved); value is an ordered list of fallback
///   targets (display models, each validated against `cfg`). The adapter walks
///   the chain on consecutive overload events: chain[0] fires first, chain[1]
///   when chain[0] is also overloaded, and so on until exhausted.
/// - `max_retries`: `routing.retry.maxAttempts` parsed as `u32`. When `None`,
///   `LINGXI_MAX_RETRIES` env (then `DEFAULT_MAX_RETRIES`) applies.
/// - `backoff_ms`: `routing.retry.backoffMs` as the base-delay for the
///   exponential backoff ladder's first rung (overrides `BASE_DELAY_MS` = 500,
///   keeping `min(b * 2^attempt, 32000)` growth + cap). When `None`, the
///   default ladder `min(500 * 2^attempt, 32000)` is used.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RoutingOverrides {
    /// Per-model fallback chains: display-model → ordered Vec of display-models.
    pub fallback: BTreeMap<String, Vec<String>>,
    /// `routing.retry.maxAttempts` override.
    pub max_retries: Option<u32>,
    /// `routing.retry.backoffMs` override (first rung of the jitter ladder).
    pub backoff_ms: Option<u64>,
}

use llm_runtime::{
    anthropic_provider_profile, parse_provider_profiles_strict, AuthStrategy, ClientConfig,
    CredentialConfig, LlmError, ProviderCredentialMode, ProviderParseOptions,
};

/// Build the built-in Anthropic [`ClientConfig`] for [`llm_runtime::ModelRuntime`].
///
/// One [`ProviderProfile`] with the full Claude-4-generation model table (10
/// entries). See module-level docs for the complete table and auth strategy
/// description.
///
/// **3c note:** `modelProviders` settings merging (multi-provider routing) is
/// deferred to Plan 3c; this profile covers the default Anthropic-only path.
#[must_use]
pub fn builtin_anthropic_config(api_base: &str, oauth_path: bool) -> ClientConfig {
    let (auth, credential) = if oauth_path {
        (
            AuthStrategy::OAuthBearer,
            CredentialConfig::HostManaged {
                id: "anthropic_oauth".to_string(),
            },
        )
    } else {
        (
            AuthStrategy::ApiKey,
            CredentialConfig::Env {
                var: "ANTHROPIC_API_KEY".to_string(),
            },
        )
    };

    ClientConfig {
        providers: vec![anthropic_provider_profile(api_base, auth, credential)],
    }
}

/// Parse the settings `providers` and `routing` objects into `cfg`, appending
/// custom [`ProviderProfile`]s and injecting model aliases from
/// `routing.aliases`.
///
/// ## Provider entry shape
///
/// ```json
/// {
///   "<profile_name>": {
///     "type": "openai" | "openai-responses" | "anthropic" | "gemini"
///           | "azure-openai" | "bedrock-claude" | "vertex-claude" | "vertex-gemini",
///     "baseUrl": "<url>",
///     "apiKeyEnv": "<ENV_VAR>",
///     "models": [
///       { "id": "<display_model>", "aliases": ["<alias>", …]?, "capabilities": {…}? }
///     ]
///   }
/// }
/// ```
///
/// `models` is **required**; an absent or empty list is an error.
///
/// ## Routing shape
///
/// ```json
/// {
///   "aliases": { "<alias>": "<profile_name>/<model_id>", … }
/// }
/// ```
///
/// - `aliases`: wired — each alias is pushed onto the target
///   [`ModelProfile`], resolved across all providers in `cfg` (including the
///   builtin Anthropic profile). An unknown `profile_name/model_id` target
///   is an [`LlmError::InvalidRequest`].
/// - `fallback` / `retry`: **parsed but inert** — `fallback_model` comes from
///   `DesktopConfig`/argv today; retry policy comes from
///   `LINGXI_MAX_RETRIES`. Wiring them is future work and is documented
///   in `core/src/settings/schema.rs`.
///
/// ## Errors
///
/// - Unknown `"type"` → [`LlmError::InvalidRequest`] naming the type string.
/// - Missing / empty `"models"` → [`LlmError::InvalidRequest`].
/// - Duplicate `profile_name` → [`LlmError::InvalidRequest`].
/// - Unknown alias target (`profile/model`) → [`LlmError::InvalidRequest`].
///
/// # Errors
///
/// Returns [`LlmError::InvalidRequest`] for any validation failure described above.
#[allow(clippy::too_many_lines)]
pub fn apply_settings_providers(
    cfg: &mut ClientConfig,
    providers: &BTreeMap<String, serde_json::Value>,
    routing: Option<&serde_json::Value>,
) -> Result<(), LlmError> {
    for profile_name in providers.keys() {
        if cfg
            .providers
            .iter()
            .any(|p| p.profile_name == *profile_name)
        {
            return Err(LlmError::InvalidRequest {
                message: format!("duplicate provider profile name: {profile_name:?}"),
            });
        }
    }

    let parsed = parse_provider_profiles_strict(
        providers,
        ProviderParseOptions {
            credential_mode: ProviderCredentialMode::Env,
            models_required: true,
        },
    )?;
    cfg.providers
        .extend(parsed.into_iter().map(|parsed| parsed.profile));
    apply_routing_aliases(cfg, routing)?;
    Ok(())
}

/// Resolve a display model from `cfg` using the `profile/model` target string.
///
/// Returns the `display_model` string of the resolved model (same as the
/// `model_id` part in most cases, but normalised via the registry).
fn resolve_display_model<'a>(
    cfg: &'a llm_runtime::ClientConfig,
    profile_part: &str,
    model_part: &str,
) -> Option<&'a str> {
    cfg.providers
        .iter()
        .find(|p| p.profile_name == profile_part)
        .and_then(|p| {
            p.models.iter().find(|m| {
                m.display_model == model_part || m.aliases.contains(&model_part.to_string())
            })
        })
        .map(|m| m.display_model.as_str())
}

/// Parse `routing.fallback`, `routing.retry.maxAttempts`, and
/// `routing.retry.backoffMs` from the settings `routing` object.
///
/// ## Fallback shape
///
/// ```json
/// "fallback": { "<primary-model-or-alias>": ["<profile/model>", ...] }
/// ```
///
/// - Key is normalised to the display model via registry-style resolution
///   against `cfg` (same as how `apply_routing_aliases` resolves targets).
///   When the key doesn't resolve as a `profile/model` it is tried as a bare
///   display model or alias across all providers.
/// - The **full chain** is validated and stored.  Every entry must resolve to
///   a known `profile/model`; an unknown entry is an [`LlmError::InvalidRequest`].
///   The adapter walks the chain in order: chain[0] fires first on the initial
///   overload fallback, chain[1] when chain[0] is also overloaded, and so on
///   until exhausted.
///
/// ## Retry shape
///
/// ```json
/// "retry": { "maxAttempts": 5, "backoffMs": 1000 }
/// ```
///
/// - `maxAttempts` (u32) → [`RoutingOverrides::max_retries`].
/// - `backoffMs` (u64) → [`RoutingOverrides::backoff_ms`].
///
/// ## Precedence (adapter)
///
/// `LINGXI_MAX_RETRIES` env > `routing.retry.maxAttempts` > `DEFAULT_MAX_RETRIES` (10).
/// Per-model `routing.fallback` entry wins over the adapter's global `fallback_model`.
///
/// # Errors
///
/// Returns [`LlmError::InvalidRequest`] when any fallback target in the chain
/// cannot be resolved in `cfg`.
pub fn parse_routing_overrides(
    routing: &serde_json::Value,
    cfg: &llm_runtime::ClientConfig,
) -> Result<RoutingOverrides, llm_runtime::LlmError> {
    let mut overrides = RoutingOverrides::default();

    // ── fallback ──────────────────────────────────────────────────────────────
    if let Some(fallback_map) = routing
        .get("fallback")
        .and_then(serde_json::Value::as_object)
    {
        for (key, chain_val) in fallback_map {
            // Resolve the KEY to a display model.
            // The key may be "profile/model" or a bare alias/display model.
            let key_display = if let Some((profile_part, model_part)) = key.split_once('/') {
                resolve_display_model(cfg, profile_part, model_part)
                    .map_or_else(|| key.clone(), str::to_string)
            } else {
                // Bare name: search all providers for an alias or display match.
                cfg.providers
                    .iter()
                    .flat_map(|p| p.models.iter())
                    .find(|m| m.display_model == *key || m.aliases.contains(key))
                    .map_or_else(|| key.clone(), |m| m.display_model.clone())
            };

            // chain_val must be a non-empty array; every entry is validated.
            let chain =
                chain_val
                    .as_array()
                    .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
                        message: format!(
                    "routing.fallback[{key:?}]: value must be an array of \"profile/model\" strings"
                ),
                    })?;
            if chain.is_empty() {
                return Err(llm_runtime::LlmError::InvalidRequest {
                    message: format!(
                        "routing.fallback[{key:?}]: chain must have at least one entry"
                    ),
                });
            }
            // Validate every entry in the chain and collect display models.
            let mut resolved_chain: Vec<String> = Vec::with_capacity(chain.len());
            for (i, entry_val) in chain.iter().enumerate() {
                let target =
                    entry_val
                        .as_str()
                        .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
                            message: format!(
                        "routing.fallback[{key:?}]: chain[{i}] must be a \"profile/model\" string"
                    ),
                        })?;
                let (profile_part, model_part) = target.split_once('/').ok_or_else(|| {
                    llm_runtime::LlmError::InvalidRequest {
                        message: format!(
                            "routing.fallback[{key:?}]: chain[{i}] target {target:?} must be \"profile/model\""
                        ),
                    }
                })?;
                let target_display = resolve_display_model(cfg, profile_part, model_part)
                    .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
                        message: format!(
                            "routing.fallback[{key:?}]: chain[{i}] target {target:?} not found in any configured profile"
                        ),
                    })?
                    .to_string();
                resolved_chain.push(target_display);
            }

            overrides.fallback.insert(key_display, resolved_chain);
        }
    }

    // ── retry ────────────────────────────────────────────────────────────────
    if let Some(retry_obj) = routing.get("retry").and_then(serde_json::Value::as_object) {
        if let Some(max_attempts_val) = retry_obj.get("maxAttempts") {
            let n =
                max_attempts_val
                    .as_u64()
                    .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
                        message: "routing.retry.maxAttempts must be a non-negative integer"
                            .to_string(),
                    })?;
            overrides.max_retries = Some(u32::try_from(n).unwrap_or(u32::MAX));
        }
        if let Some(backoff_val) = retry_obj.get("backoffMs") {
            let n = backoff_val
                .as_u64()
                .ok_or_else(|| llm_runtime::LlmError::InvalidRequest {
                    message: "routing.retry.backoffMs must be a non-negative integer".to_string(),
                })?;
            if n == 0 {
                // 0 would collapse the jitter ladder to zero-delay retries (a
                // tight retry loop hammering the provider) — reject up front.
                return Err(llm_runtime::LlmError::InvalidRequest {
                    message:
                        "routing.retry.backoffMs must be >= 1 (0 would disable backoff entirely)"
                            .to_string(),
                });
            }
            overrides.backoff_ms = Some(n);
        }
    }

    Ok(overrides)
}

/// Wire `routing.aliases` into the target model profiles already in `cfg`.
///
/// `routing.fallback` and `routing.retry` are parsed by [`parse_routing_overrides`]
/// separately and threaded into the adapter constructor.
fn apply_routing_aliases(
    cfg: &mut ClientConfig,
    routing: Option<&serde_json::Value>,
) -> Result<(), LlmError> {
    let Some(routing) = routing else {
        return Ok(());
    };
    let Some(aliases_map) = routing
        .get("aliases")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(());
    };

    for (alias, target_val) in aliases_map {
        let target = target_val
            .as_str()
            .ok_or_else(|| LlmError::InvalidRequest {
                message: format!("routing.aliases[{alias:?}]: value must be a string"),
            })?;

        // target is "profile_name/model_id"
        let (profile_part, model_part) =
            target
                .split_once('/')
                .ok_or_else(|| LlmError::InvalidRequest {
                    message: format!(
                        "routing.aliases[{alias:?}]: target {target:?} must be \"profile/model\""
                    ),
                })?;

        // Find the matching model in cfg.providers (including builtin).
        let model = cfg
            .providers
            .iter_mut()
            .find(|p| p.profile_name == profile_part)
            .and_then(|p| p.models.iter_mut().find(|m| m.display_model == model_part))
            .ok_or_else(|| LlmError::InvalidRequest {
                message: format!(
                    "routing.aliases[{alias:?}]: target {target:?} not found in any configured profile"
                ),
            })?;

        if !model.aliases.contains(&alias.to_string()) {
            model.aliases.push(alias.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "llm_config_test.rs"]
mod llm_config_test;
