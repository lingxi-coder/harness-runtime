//! Shared provider settings parsing and provider identity helpers.
//!
//! This module owns the multi-provider semantics that used to be duplicated in
//! host/platform crates: settings provider kinds, provider profile construction,
//! credential handling mode, model capability parsing, pricing overrides, and
//! model-string/provider identity helpers.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::{
    AuthStrategy, AzureConfig, Capabilities, ConnectionSpec, CredentialConfig, FailoverTriggers,
    LlmError, ModelProfile, PricingConfig, PricingOverride, ProtocolFamily, ProviderId,
    ProviderProfile, SigningConfig,
};

const SUPPORTED_PROVIDER_TYPES: &str = "openai, openai-responses, anthropic, gemini, azure-openai, bedrock-claude, vertex-claude, vertex-gemini, foundry-claude";

/// Provider kinds accepted in `settings.providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI-compatible Chat Completions wire.
    OpenAi,
    /// OpenAI Responses wire.
    OpenAiResponses,
    /// Anthropic Messages wire.
    Anthropic,
    /// Gemini first-party generateContent wire.
    Gemini,
    /// Azure OpenAI deployments wire.
    AzureOpenAi,
    /// Anthropic Claude on AWS Bedrock.
    BedrockClaude,
    /// Anthropic Claude on Vertex AI.
    VertexClaude,
    /// Gemini on Vertex AI.
    VertexGemini,
    /// Anthropic Claude on Azure AI Foundry.
    FoundryClaude,
}

impl ProviderKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "openai" => Some(Self::OpenAi),
            "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            "azure-openai" => Some(Self::AzureOpenAi),
            "bedrock-claude" => Some(Self::BedrockClaude),
            "vertex-claude" => Some(Self::VertexClaude),
            "vertex-gemini" => Some(Self::VertexGemini),
            "foundry-claude" => Some(Self::FoundryClaude),
            _ => None,
        }
    }

    fn protocol(self) -> ProtocolFamily {
        match self {
            Self::OpenAi => ProtocolFamily::OpenAiChat,
            Self::OpenAiResponses => ProtocolFamily::OpenAiResponses,
            Self::Anthropic => ProtocolFamily::AnthropicMessages,
            Self::Gemini => ProtocolFamily::GeminiGenerateContent,
            Self::AzureOpenAi => ProtocolFamily::AzureOpenAi,
            Self::BedrockClaude => ProtocolFamily::BedrockClaude,
            Self::VertexClaude => ProtocolFamily::VertexClaude,
            Self::VertexGemini => ProtocolFamily::VertexGemini,
            Self::FoundryClaude => ProtocolFamily::FoundryClaude,
        }
    }

    fn auth(self) -> AuthStrategy {
        match self {
            Self::AzureOpenAi => AuthStrategy::AzureToken,
            Self::BedrockClaude => AuthStrategy::AwsSigV4,
            Self::VertexClaude | Self::VertexGemini => AuthStrategy::GcpToken,
            // Foundry: a plain API key uses `x-api-key` (Anthropic-style) — the
            // default. AAD-token (`Authorization: Bearer`) profiles set
            // `auth = Bearer` explicitly. See `foundry_claude` codec docs and
            // the `(ApiKey, FoundryClaude)` auth arm in `client.rs`.
            Self::FoundryClaude => AuthStrategy::ApiKey,
            Self::OpenAi | Self::OpenAiResponses | Self::Anthropic | Self::Gemini => {
                AuthStrategy::ApiKey
            }
        }
    }

    fn provider_id(self, profile_name: &str) -> ProviderId {
        match self {
            Self::OpenAi | Self::OpenAiResponses => ProviderId::OpenAICompatible {
                name: profile_name.to_string(),
            },
            Self::Anthropic => ProviderId::AnthropicFirstParty,
            Self::Gemini => ProviderId::Gemini,
            Self::AzureOpenAi => ProviderId::AzureOpenAI,
            Self::BedrockClaude => ProviderId::BedrockClaude,
            Self::VertexClaude => ProviderId::VertexClaude,
            Self::VertexGemini => ProviderId::VertexGemini,
            Self::FoundryClaude => ProviderId::FoundryClaude,
        }
    }

    fn requires_base_url(self) -> bool {
        !matches!(self, Self::BedrockClaude)
    }

    fn requires_api_key_env(self) -> bool {
        !matches!(self, Self::BedrockClaude)
    }
}

/// Credential behavior for parsed user provider profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCredentialMode {
    /// Resolve provider credentials from the configured environment variable.
    Env,
    /// Retain `apiKeyEnv` as metadata and leave the profile credential deferred.
    DeferredStatic,
}

/// Options controlling provider settings parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderParseOptions {
    /// How API-key credentials should be represented in the returned profile.
    pub credential_mode: ProviderCredentialMode,
    /// Whether `models` must be present and non-empty.
    pub models_required: bool,
}

impl ProviderParseOptions {
    /// Strict platform parser behavior: credentials come from env and models are required.
    #[must_use]
    pub const fn strict_env() -> Self {
        Self {
            credential_mode: ProviderCredentialMode::Env,
            models_required: true,
        }
    }

    /// Lenient provider-config behavior: credentials are deferred and models may be absent.
    #[must_use]
    pub const fn lenient_deferred_static() -> Self {
        Self {
            credential_mode: ProviderCredentialMode::DeferredStatic,
            models_required: false,
        }
    }
}

/// A parsed user profile plus the original API-key env var, when one exists.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUserProvider {
    /// Built provider profile.
    pub profile: ProviderProfile,
    /// `apiKeyEnv` captured for host credential metadata.
    pub env_var: Option<String>,
}

/// Strictly parse `settings.providers` into provider profiles.
///
/// # Errors
///
/// Returns [`LlmError::InvalidRequest`] for the first invalid provider or model
/// entry.
pub fn parse_provider_profiles_strict(
    providers: &BTreeMap<String, Value>,
    options: ProviderParseOptions,
) -> Result<Vec<ParsedUserProvider>, LlmError> {
    let mut out = Vec::with_capacity(providers.len());
    for (name, value) in providers {
        let parsed = parse_one_provider(name, value, options, true)
            .map_err(|message| LlmError::InvalidRequest { message })?;
        out.extend(parsed.into_iter().map(|p| p.provider));
    }
    Ok(out)
}

/// Leniently parse `settings.providers`, skipping invalid providers and
/// returning warning strings instead of errors.
#[must_use]
pub fn parse_provider_profiles_lenient(
    providers: &BTreeMap<String, Value>,
    options: ProviderParseOptions,
) -> (Vec<ParsedUserProvider>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (name, value) in providers {
        match parse_one_provider(name, value, options, false) {
            Ok(parsed) => {
                for one in parsed {
                    warnings.extend(one.warnings);
                    out.push(one.provider);
                }
            }
            Err(message) => warnings.push(format!("{message}; skipped")),
        }
    }
    (out, warnings)
}

/// One desugared connection: a flat provider entry the existing validator can
/// parse, plus the group identity to stamp onto the profile it produces.
#[derive(Debug)]
struct ExpandedConnection {
    profile_name: String,
    entry: Value,
    identity: ConnectionSpec,
    /// Keychain credential id from `credentialIds`, when this is one key slot.
    credential_override: Option<String>,
}

/// Shallow-merge a connection's overrides over the provider-level defaults.
///
/// Shallow is deliberate: a connection that redeclares `models` or `pricing`
/// means "this endpoint serves exactly these", not "add to the provider's list".
fn merge_connection(base: &Map<String, Value>, overrides: &Map<String, Value>) -> Value {
    let mut merged = base.clone();
    merged.remove("connections");
    merged.remove("fallback");
    merged.remove("credentialIds");
    for (k, v) in overrides {
        if k == "id" || k == "credentialIds" {
            continue;
        }
        merged.insert(k.clone(), v.clone());
    }
    Value::Object(merged)
}

/// Expand one `credentialIds` list into sibling key slots of the same connection.
///
/// Each slot is a full profile differing ONLY in its credential, so key rotation
/// reuses the connection failover walk instead of needing a key index threaded
/// through the auth path. Only the first is selectable; the rest exist to be
/// failed over onto.
fn expand_key_slots(
    profile_name: &str,
    entry: &Value,
    group: &str,
    connection_id: &str,
    keys: &[String],
    failover: FailoverTriggers,
    order: &mut u32,
    out: &mut Vec<ExpandedConnection>,
) {
    for (slot, key) in keys.iter().enumerate() {
        out.push(ExpandedConnection {
            profile_name: format!("{profile_name}#{slot}"),
            entry: entry.clone(),
            identity: ConnectionSpec {
                group: Some(group.to_string()),
                connection_id: Some(connection_id.to_string()),
                order: *order,
                hidden: slot > 0,
                failover,
            },
            credential_override: Some(key.clone()),
        });
        *order += 1;
    }
}

/// Read a `credentialIds` list, rejecting anything that is not a list of non-empty
/// distinct strings.
///
/// These are KEYCHAIN IDS, never secrets. The field is deliberately not called
/// `apiKeys`: a settings key spelled that way invites pasting a real key into
/// `settings.json`, which is exactly what the clients' secret-free guards reject.
fn parse_credential_ids(
    obj: &Map<String, Value>,
    label: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = obj.get("credentialIds") else {
        return Ok(None);
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| format!("{label}: \"credentialIds\" must be an array of credential ids"))?;
    if arr.is_empty() {
        return Err(format!("{label}: \"credentialIds\" must not be empty"));
    }
    let mut keys = Vec::with_capacity(arr.len());
    for item in arr {
        let key = item.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
            format!("{label}: \"credentialIds\" entries must be non-empty strings")
        })?;
        if keys.iter().any(|existing| existing == key) {
            return Err(format!("{label}: duplicate credentialIds entry {key:?}"));
        }
        keys.push(key.to_string());
    }
    Ok(Some(keys))
}

/// Read `fallback.on` into a trigger set.
///
/// A provider that declares connections but no `fallback` gets
/// [`FailoverTriggers::DEFAULT`] — declaring several ways to reach a provider
/// and then not using them on failure would be a surprising default.
fn parse_failover(obj: &Map<String, Value>, name: &str) -> Result<FailoverTriggers, String> {
    let Some(fallback) = obj.get("fallback") else {
        return Ok(FailoverTriggers::DEFAULT);
    };
    let fallback = fallback
        .as_object()
        .ok_or_else(|| format!("provider {name:?}: \"fallback\" must be an object"))?;
    let Some(on) = fallback.get("on") else {
        return Ok(FailoverTriggers::DEFAULT);
    };
    let list = on
        .as_array()
        .ok_or_else(|| format!("provider {name:?}: \"fallback.on\" must be an array"))?;
    let mut triggers = FailoverTriggers::NONE;
    for item in list {
        let token = item
            .as_str()
            .ok_or_else(|| format!("provider {name:?}: \"fallback.on\" entries must be strings"))?;
        // Name the bad token rather than silently dropping it: a typo here
        // disables failover, which is invisible until the day it is needed.
        triggers.apply_name(token).ok_or_else(|| {
            format!(
                "provider {name:?}: unknown fallback.on trigger {token:?} (supported: rate_limit, overloaded, server_error, network, auth)"
            )
        })?;
    }
    Ok(triggers)
}

/// Desugar a provider entry into its connections.
///
/// No `connections` and no `credentialIds` reproduces the historical single flat
/// profile exactly, identity included, so every pre-existing config and every
/// built-in preset is untouched.
fn expand_connections(
    name: &str,
    obj: &Map<String, Value>,
) -> Result<Vec<ExpandedConnection>, String> {
    let provider_keys = parse_credential_ids(obj, &format!("provider {name:?}"))?;
    let failover = parse_failover(obj, name)?;

    let Some(raw) = obj.get("connections") else {
        let entry = merge_connection(obj, &Map::new());
        let mut out = Vec::new();
        let mut order = 0;
        match provider_keys {
            Some(keys) => expand_key_slots(
                name, &entry, name, "default", &keys, failover, &mut order, &mut out,
            ),
            // No connections and no credentialIds: the historical single profile.
            // Its chain is always empty, so it never fails over regardless.
            None => out.push(ExpandedConnection {
                profile_name: name.to_string(),
                entry,
                identity: ConnectionSpec::default(),
                credential_override: None,
            }),
        }
        return Ok(out);
    };

    let list = raw
        .as_array()
        .ok_or_else(|| format!("provider {name:?}: \"connections\" must be an array"))?;
    if list.is_empty() {
        return Err(format!(
            "provider {name:?}: \"connections\" must not be empty (omit it for a single-connection provider)"
        ));
    }

    let mut out = Vec::new();
    let mut order = 0;
    let mut seen: Vec<String> = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let conn = item
            .as_object()
            .ok_or_else(|| format!("provider {name:?}: connections[{index}] is not an object"))?;
        let id = conn
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!("provider {name:?}: connections[{index}] needs a non-empty string \"id\"")
            })?;
        if id.contains('/') || id.contains(':') || id.contains('#') {
            return Err(format!(
                "provider {name:?}: connection id {id:?} must not contain '/', ':' or '#' (they separate a qualified model reference)"
            ));
        }
        if seen.iter().any(|existing| existing == id) {
            return Err(format!("provider {name:?}: duplicate connection id {id:?}"));
        }
        seen.push(id.to_string());

        let entry = merge_connection(obj, conn);
        let profile_name = format!("{name}:{id}");
        let label = format!("provider {name:?} connection {id:?}");
        let keys = match parse_credential_ids(conn, &label)? {
            Some(keys) => Some(keys),
            None => provider_keys.clone(),
        };
        match keys {
            Some(keys) => {
                expand_key_slots(
                    &profile_name,
                    &entry,
                    name,
                    id,
                    &keys,
                    failover,
                    &mut order,
                    &mut out,
                );
            }
            None => {
                out.push(ExpandedConnection {
                    profile_name,
                    entry,
                    identity: ConnectionSpec {
                        group: Some(name.to_string()),
                        connection_id: Some(id.to_string()),
                        order,
                        hidden: false,
                        failover,
                    },
                    credential_override: None,
                });
                order += 1;
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct ProviderParseResult {
    provider: ParsedUserProvider,
    warnings: Vec<String>,
}

/// Parse one `settings.providers` entry into its connections.
///
/// A provider with `connections` (or `credentialIds`) yields several profiles that
/// share one group; everything else yields exactly one, byte-identical to the
/// pre-`connections` behaviour.
fn parse_one_provider(
    name: &str,
    value: &Value,
    options: ProviderParseOptions,
    strict: bool,
) -> Result<Vec<ProviderParseResult>, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("provider {name:?}: entry is not an object"))?;
    let expanded = expand_connections(name, obj)?;

    let mut out = Vec::with_capacity(expanded.len());
    for connection in expanded {
        let mut parsed = parse_flat_provider(
            name,
            &connection.profile_name,
            &connection.entry,
            options,
            strict,
            connection.credential_override.as_deref(),
        )?;
        parsed.provider.profile.connection = connection.identity;
        out.push(parsed);
    }
    Ok(out)
}

/// Parse ONE already-desugared flat provider entry.
///
/// `group` is the provider the entry belongs to and drives `provider_id`, so
/// every connection of one provider shares a billing/pricing identity.
/// `profile_name` is this connection's own name. For a provider written without
/// `connections` the two are equal, which is exactly the historical behaviour.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn parse_flat_provider(
    group: &str,
    profile_name: &str,
    value: &Value,
    options: ProviderParseOptions,
    strict: bool,
    credential_override: Option<&str>,
) -> Result<ProviderParseResult, String> {
    // Error labels and `parse_models` name the CONNECTION, which is what the
    // operator needs to see; for a flat provider it is the provider name.
    let name = profile_name;
    let mut warnings = Vec::new();
    let obj = value
        .as_object()
        .ok_or_else(|| format!("provider {name:?}: entry is not an object"))?;

    let type_str = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("provider {name:?}: missing string \"type\""))?;
    let kind = ProviderKind::parse(type_str).ok_or_else(|| {
        format!(
            "provider {name:?}: unknown type {type_str:?} (supported: {SUPPORTED_PROVIDER_TYPES})"
        )
    })?;
    let protocol = kind.protocol();
    let auth = kind.auth();

    let supports_websockets =
        parse_optional_bool(obj, name, "supportsWebsockets")?.unwrap_or(false);
    let supports_websocket_compression =
        parse_optional_bool(obj, name, "supportsWebsocketCompression")?.unwrap_or(false);
    let websocket_connect_timeout_ms = parse_optional_u64(obj, name, "websocketConnectTimeoutMs")?;
    let vision_delegate = parse_optional_string(obj, name, "visionDelegate")?;

    if supports_websockets && matches!(kind, ProviderKind::BedrockClaude) {
        return Err(format!(
            "provider {name:?}: supportsWebsockets is not supported for AWS SigV4/Bedrock providers"
        ));
    }
    if supports_websockets && !matches!(protocol, ProtocolFamily::OpenAiResponses) {
        return Err(format!(
            "provider {name:?}: supportsWebsockets is only valid for openai-responses providers"
        ));
    }
    if supports_websocket_compression {
        return Err(format!(
            "provider {name:?}: supportsWebsocketCompression is not supported by this build"
        ));
    }

    let (base_url, signing) = if matches!(kind, ProviderKind::BedrockClaude) {
        let region = required_non_empty_string(obj, name, "region", Some("bedrock-claude"))?;
        let base_url = obj
            .get("baseUrl")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || format!("https://bedrock-runtime.{region}.amazonaws.com"),
                str::to_string,
            );
        (
            base_url,
            Some(SigningConfig {
                region,
                service: "bedrock".to_string(),
            }),
        )
    } else if kind.requires_base_url() {
        (required_non_empty_string(obj, name, "baseUrl", None)?, None)
    } else {
        unreachable!("all non-bedrock provider kinds require baseUrl")
    };

    let azure = if matches!(kind, ProviderKind::AzureOpenAi) {
        Some(AzureConfig {
            api_version: required_non_empty_string(obj, name, "apiVersion", Some("azure-openai"))?,
        })
    } else {
        None
    };

    let env_var = obj
        .get("apiKeyEnv")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if kind.requires_api_key_env()
        && options.credential_mode == ProviderCredentialMode::Env
        && env_var.is_none()
        // A `credentialIds` slot names a stored credential outright, so there is no
        // env var to require.
        && credential_override.is_none()
    {
        return Err(format!(
            "provider {name:?}: \"apiKeyEnv\" is required and must not be empty"
        ));
    }

    let credential = if let Some(id) = credential_override {
        CredentialConfig::Static { id: id.to_string() }
    } else if matches!(kind, ProviderKind::BedrockClaude) {
        CredentialConfig::HostManaged {
            id: "bedrock_sigv4".to_string(),
        }
    } else {
        match options.credential_mode {
            ProviderCredentialMode::Env => CredentialConfig::Env {
                var: env_var.clone().expect("apiKeyEnv was validated above"),
            },
            // A connection inherits the PROVIDER's stored credential unless it
            // named its own through `credentialIds`. Without this, `assemble`'s
            // "no credential => Static{profile_name}" rule would look for
            // `deepseek:cn`, while every editor saves the key under `deepseek` —
            // so a multi-connection provider authenticated through the UI would
            // fail auth with no indication why. It also makes the ordinary case
            // right: one key, two regions.
            ProviderCredentialMode::DeferredStatic if group != profile_name => {
                CredentialConfig::Static {
                    id: group.to_string(),
                }
            }
            ProviderCredentialMode::DeferredStatic => CredentialConfig::None,
        }
    };

    let mut models = parse_models(
        obj.get("models"),
        name,
        options.models_required,
        strict,
        &mut warnings,
    )?;
    let (mut pricing, display_overrides) = if let Some(pricing_val) = obj.get("pricing") {
        parse_pricing_overrides(name, pricing_val, &models)?
    } else {
        (PricingConfig::default(), Vec::new())
    };
    if let Some(value) = obj.get("billingMode") {
        let raw = value
            .as_str()
            .ok_or_else(|| format!("provider {name:?}: billingMode must be a string"))?;
        pricing.billing_mode = match raw {
            "perToken" => platform_api::ModelBillingMode::PerToken,
            "subscription" => platform_api::ModelBillingMode::Subscription,
            "free" => platform_api::ModelBillingMode::Free,
            "unknown" => platform_api::ModelBillingMode::Unknown,
            _ => {
                return Err(format!(
                    "provider {name:?}: unsupported billingMode {raw:?}"
                ));
            }
        };
    }
    if !pricing.overrides.is_empty() {
        // A concrete per-model price sheet is stronger evidence than a broad
        // provider billing hint and must drive both cost lookup and display.
        pricing.billing_mode = platform_api::ModelBillingMode::PerToken;
    }
    for (model_id, model_pricing) in display_overrides {
        if let Some(model) = models
            .iter_mut()
            .find(|model| model.display_model == model_id)
        {
            model.metadata.pricing = Some(model_pricing);
        }
    }

    Ok(ProviderParseResult {
        provider: ParsedUserProvider {
            profile: ProviderProfile {
                wire_profile: None,
                regions: obj.get("regions").map(|value| serde_json::from_value(value.clone()).map_err(|_| format!("provider {profile_name:?}: regions must contain china_mainland or international"))).transpose()?.unwrap_or_else(lingxi_llm_client::protocol::Region::all),
                provider_id: kind.provider_id(group),
                profile_name: profile_name.to_string(),
                base_url,
                protocol,
                auth,
                credential,
                models,
                pricing,
                signing,
                azure,
                supports_websockets,
                supports_websocket_compression,
                websocket_connect_timeout_ms,
                vision_delegate,
                connection: Default::default(),
            },
            env_var,
        },
        warnings,
    })
}

fn parse_optional_bool(
    obj: &Map<String, Value>,
    provider_name: &str,
    key: &str,
) -> Result<Option<bool>, String> {
    obj.get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("provider {provider_name:?}: \"{key}\" must be a boolean"))
        })
        .transpose()
}

fn parse_optional_u64(
    obj: &Map<String, Value>,
    provider_name: &str,
    key: &str,
) -> Result<Option<u64>, String> {
    obj.get(key)
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                format!("provider {provider_name:?}: \"{key}\" must be an unsigned integer")
            })
        })
        .transpose()
}

fn parse_optional_string(
    obj: &Map<String, Value>,
    provider_name: &str,
    key: &str,
) -> Result<Option<String>, String> {
    obj.get(key)
        .map(|value| {
            value
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    format!("provider {provider_name:?}: \"{key}\" must be a non-empty string")
                })
        })
        .transpose()
}

fn required_non_empty_string(
    obj: &Map<String, Value>,
    provider_name: &str,
    key: &str,
    kind_hint: Option<&str>,
) -> Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| match kind_hint {
            Some(kind) => {
                format!("provider {provider_name:?}: missing non-empty \"{key}\" for {kind}")
            }
            None => {
                format!("provider {provider_name:?}: \"{key}\" is required and must not be empty")
            }
        })
}

fn parse_models(
    value: Option<&Value>,
    provider_name: &str,
    models_required: bool,
    strict: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<ModelProfile>, String> {
    let Some(value) = value else {
        if models_required {
            return Err(format!(
                "provider {provider_name:?}: \"models\" is required (no auto-discovery)"
            ));
        }
        return Ok(Vec::new());
    };

    let Some(models) = value.as_array() else {
        if strict {
            return Err(format!(
                "provider {provider_name:?}: \"models\" must be an array"
            ));
        }
        warnings.push(format!(
            "provider {provider_name:?}: \"models\" must be an array; treating as empty"
        ));
        return Ok(Vec::new());
    };

    if models_required && models.is_empty() {
        return Err(format!(
            "provider {provider_name:?}: \"models\" must have at least one entry"
        ));
    }

    let mut parsed = Vec::with_capacity(models.len());
    for model in models {
        match parse_model_entry(provider_name, model) {
            Ok(profile) => parsed.push(profile),
            Err(message) if strict => return Err(message),
            Err(message) => warnings.push(format!("{message}; skipped")),
        }
    }

    if models_required && parsed.is_empty() {
        return Err(format!(
            "provider {provider_name:?}: \"models\" must have at least one entry"
        ));
    }

    Ok(parsed)
}

fn parse_model_entry(provider_name: &str, value: &Value) -> Result<ModelProfile, String> {
    match value {
        Value::String(id) => Ok(ModelProfile {
            display_model: id.clone(),
            request_model: id.clone(),
            billing_model: id.clone(),
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: permissive_caps(),
        }),
        Value::Object(obj) => {
            let id = obj.get("id").and_then(Value::as_str).ok_or_else(|| {
                format!("provider {provider_name:?}: model entry missing string \"id\"")
            })?;
            let aliases = obj
                .get("aliases")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|alias| alias.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let metadata = obj
                .get("metadata")
                .cloned()
                .map(serde_json::from_value::<platform_api::ModelMetadata>)
                .transpose()
                .map_err(|error| {
                    format!("provider {provider_name:?}: model {id:?} metadata is invalid: {error}")
                })?
                .unwrap_or_default();
            Ok(ModelProfile {
                display_model: id.to_string(),
                request_model: id.to_string(),
                billing_model: id.to_string(),
                aliases,
                description: None,
                metadata,
                capabilities: parse_capabilities(obj.get("capabilities")),
            })
        }
        _ => Err(format!(
            "provider {provider_name:?}: model entry is not a string or object"
        )),
    }
}

fn permissive_caps() -> Capabilities {
    Capabilities {
        streaming: true,
        tools: true,
        vision: true,
        documents: true,
        reasoning: true,
        structured_output: true,
    }
}

fn parse_capabilities(caps_val: Option<&Value>) -> Capabilities {
    let Some(caps_val) = caps_val else {
        return Capabilities {
            streaming: true,
            tools: true,
            vision: false,
            documents: false,
            reasoning: false,
            structured_output: false,
        };
    };
    let flag = |key: &str, default: bool| -> bool {
        caps_val
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(default)
    };
    Capabilities {
        streaming: flag("streaming", true),
        tools: flag("tools", true),
        vision: flag("vision", false),
        documents: flag("documents", false),
        reasoning: flag("reasoning", false),
        structured_output: flag("structuredOutput", false),
    }
}

fn parse_pricing_overrides(
    profile_name: &str,
    pricing_val: &Value,
    model_profiles: &[ModelProfile],
) -> Result<(PricingConfig, Vec<(String, platform_api::ModelPricing)>), String> {
    const KNOWN_PRICING_KEYS: &[&str] = &[
        "inputPerMtok",
        "outputPerMtok",
        "cacheWritePerMtok",
        "cacheReadPerMtok",
        "reasoningPerMtok",
    ];

    let pricing_obj = pricing_val.as_object().ok_or_else(|| {
        format!(
            "provider {profile_name:?}: \"pricing\" must be an object (got {})",
            json_type_name(pricing_val)
        )
    })?;

    let known_models: BTreeSet<&str> = model_profiles
        .iter()
        .map(|model| model.display_model.as_str())
        .collect();
    let mut overrides = Vec::with_capacity(pricing_obj.len());
    let mut display_overrides = Vec::with_capacity(pricing_obj.len());

    for (model_id, model_pricing_val) in pricing_obj {
        if !known_models.contains(model_id.as_str()) {
            return Err(format!(
                "provider {profile_name:?}: pricing key {model_id:?} is not in the models list - add the model first or remove the override"
            ));
        }

        let model_pricing_obj = model_pricing_val.as_object().ok_or_else(|| {
            format!("provider {profile_name:?}: pricing[{model_id:?}] must be an object")
        })?;

        for key in model_pricing_obj.keys() {
            if !KNOWN_PRICING_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "provider {profile_name:?}: pricing[{model_id:?}] unknown key {key:?} (known: inputPerMtok, outputPerMtok, cacheWritePerMtok, cacheReadPerMtok, reasoningPerMtok)"
                ));
            }
        }

        let input_per_million = parse_price_field(
            profile_name,
            model_id,
            model_pricing_obj,
            "inputPerMtok",
            true,
        )?;
        let output_per_million = parse_price_field(
            profile_name,
            model_id,
            model_pricing_obj,
            "outputPerMtok",
            true,
        )?;
        let cache_write_per_million = parse_price_field(
            profile_name,
            model_id,
            model_pricing_obj,
            "cacheWritePerMtok",
            false,
        )?;
        let cache_read_per_million = parse_price_field(
            profile_name,
            model_id,
            model_pricing_obj,
            "cacheReadPerMtok",
            false,
        )?;
        let reasoning_per_million = parse_price_field(
            profile_name,
            model_id,
            model_pricing_obj,
            "reasoningPerMtok",
            false,
        )?;

        overrides.push((
            model_id.clone(),
            PricingOverride {
                input_per_million: input_per_million.unwrap_or(0.0),
                output_per_million: output_per_million.unwrap_or(0.0),
                cache_write_per_million: cache_write_per_million.unwrap_or(0.0),
                cache_read_per_million: cache_read_per_million.unwrap_or(0.0),
                reasoning_per_million: reasoning_per_million.or(output_per_million).unwrap_or(0.0),
            },
        ));
        display_overrides.push((
            model_id.clone(),
            platform_api::ModelPricing {
                billing_mode: platform_api::ModelBillingMode::PerToken,
                input_per_million,
                output_per_million,
                cache_read_per_million,
                cache_write_per_million,
                reasoning_per_million,
                tiers: Vec::new(),
                source: Some("userOverride".to_string()),
            },
        ));
    }

    Ok((
        PricingConfig {
            billing_mode: if overrides.is_empty() {
                platform_api::ModelBillingMode::Unknown
            } else {
                platform_api::ModelBillingMode::PerToken
            },
            require_priced: false,
            overrides,
        },
        display_overrides,
    ))
}

fn parse_price_field(
    profile_name: &str,
    model_id: &str,
    obj: &Map<String, Value>,
    key: &str,
    required: bool,
) -> Result<Option<f64>, String> {
    match obj.get(key) {
        None if required => Err(format!(
            "provider {profile_name:?}: pricing[{model_id:?}] missing required field {key:?}"
        )),
        None => Ok(None),
        Some(val) => {
            let v = val.as_f64().ok_or_else(|| {
                format!(
                    "provider {profile_name:?}: pricing[{model_id:?}].{key} must be a number, got {val}"
                )
            })?;
            if v < 0.0 {
                return Err(format!(
                    "provider {profile_name:?}: pricing[{model_id:?}].{key} must be >= 0 (got {v})"
                ));
            }
            Ok(Some(v))
        }
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Array(_) => "array",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Null => "null",
        Value::Object(_) => "object",
    }
}

fn anthropic_metadata(model: &str) -> platform_api::ModelMetadata {
    use crate::model::context_window::{context_window_for_model, max_output_tokens_for_model};

    let rates = match model {
        "claude-sonnet-5" => Some((2.0, 10.0, 0.2, 2.5)),
        "claude-opus-5" => Some((5.0, 25.0, 0.5, 6.25)),
        "claude-fable-5-1" | "claude-mythos-5-1" => Some((10.0, 50.0, 0.25, 12.5)),
        "claude-haiku-4-5" => Some((1.0, 5.0, 0.1, 1.25)),
        "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => Some((5.0, 25.0, 0.5, 6.25)),
        "claude-sonnet-4-6" => Some((3.0, 15.0, 0.3, 3.75)),
        _ => None,
    };
    platform_api::ModelMetadata {
        input_modalities: vec!["text".to_string(), "image".to_string(), "pdf".to_string()],
        output_modalities: vec!["text".to_string()],
        context_window_tokens: Some(context_window_for_model(model, &[])),
        max_output_tokens: Some(
            if matches!(
                model,
                "claude-fable-5-1" | "claude-mythos-5-1" | "claude-opus-5" | "claude-sonnet-5"
            ) {
                128_000
            } else if model == "claude-haiku-4-5" {
                64_000
            } else {
                max_output_tokens_for_model(model)
            },
        ),
        pricing: rates.map(
            |(input, output, cache_read, cache_write)| platform_api::ModelPricing {
                billing_mode: platform_api::ModelBillingMode::PerToken,
                input_per_million: Some(input),
                output_per_million: Some(output),
                cache_read_per_million: Some(cache_read),
                cache_write_per_million: Some(cache_write),
                source: Some("official".to_string()),
                ..platform_api::ModelPricing::default()
            },
        ),
        ..platform_api::ModelMetadata::default()
    }
}

/// Built-in Anthropic Claude model profiles used by desktop/mobile hosts.
#[must_use]
pub fn anthropic_model_profiles() -> Vec<ModelProfile> {
    fn model(display: &str, billing: &str, aliases: &[&str], reasoning: bool) -> ModelProfile {
        ModelProfile {
            display_model: display.to_string(),
            request_model: display.to_string(),
            billing_model: billing.to_string(),
            aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
            description: None,
            metadata: anthropic_metadata(display),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                vision: true,
                documents: true,
                reasoning,
                // WP11/F0xx: this was hard-coded `false` for every Anthropic
                // model, but the Anthropic codec fully implements structured
                // output — `providers/anthropic.rs::base_body` encodes
                // `ResponseFormat::JsonSchema` as `output_config.format` and
                // unconditionally lights the `structured-outputs-2025-12-15`
                // beta whenever `request.response_format.is_some()`
                // (`providers/anthropic.rs` beta-header block), and
                // `sidequery::ProviderSideQueryClient::query_json_schema`
                // routes EVERY provider (Anthropic included) through
                // `ApiService::stream_json_schema_with_thinking` unconditionally
                // — there is no Anthropic-specific skip anywhere in that path.
                // The `false` here was a fill-in gap, not a real capability
                // limit: `llm_runtime::protocol::validate_capabilities` reads
                // this same bit to gate ANY `response_format` request (not
                // just Fusion — `auto_mode_propose.rs` calls
                // `stream_json_schema` too), so leaving it `false` silently
                // broke every structured-output call against an Anthropic
                // model, and (compounded by Fusion catalog-availability
                // filtering) made a pure-Anthropic Fusion install fail its
                // `resolve_analyst` preflight with `StructuredOutputUnsupported`
                // on every run.
                structured_output: true,
            },
        }
    }

    vec![
        model(
            "claude-sonnet-4-20250514",
            "claude-sonnet-4",
            &["claude-sonnet-4", "claude-sonnet", "claude"],
            true,
        ),
        model(
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5",
            &["claude-sonnet-4-5"],
            true,
        ),
        model("claude-sonnet-4-6", "claude-sonnet-4-6", &[], true),
        // Sonnet 5 — the 2.1.198 default first-party model (registry id
        // "claude-sonnet-5"; alias table `sonnet.default` points here).
        model("claude-sonnet-5", "claude-sonnet-5", &[], true),
        model(
            "claude-opus-4-20250514",
            "claude-opus-4",
            &["claude-opus-4", "claude-opus"],
            true,
        ),
        model(
            "claude-opus-4-1-20250805",
            "claude-opus-4-1",
            &["claude-opus-4-1"],
            true,
        ),
        model(
            "claude-opus-4-5-20251101",
            "claude-opus-4-5",
            &["claude-opus-4-5"],
            true,
        ),
        model("claude-opus-4-6", "claude-opus-4-6", &[], true),
        model("claude-opus-4-7", "claude-opus-4-7", &[], true),
        model("claude-opus-4-8", "claude-opus-4-8", &[], true),
        // Opus 5 — 2.1.219 flagship. Present in the oracle's model table
        // (`provider_ids.first_party = "claude-opus-5"`, vertex region
        // `VERTEX_REGION_CLAUDE_5_OPUS`, 1M native context, capabilities
        // incl. `lean_prompt` / `refusal_fallback` / `opus_5_prompt_bundle`)
        // but absent from this port entirely, so it never reached the /model
        // picker or any of the model-gated paths below.
        model("claude-opus-5", "claude-opus-5", &[], true),
        model(
            "claude-haiku-4-20250307",
            "claude-haiku-4",
            &["claude-haiku-4", "claude-haiku"],
            true,
        ),
        model("claude-haiku-4-5", "claude-haiku-4-5", &[], true),
        // Fable 5.1 — Anthropic's latest long-horizon reasoning model. Fable 5
        // is intentionally not retained as an alias or compatibility entry.
        model("claude-fable-5-1", "claude-fable-5-1", &[], true),
        // Mythos 5.1 shares Fable 5.1's model and remains invite-only.
        model("claude-mythos-5-1", "claude-mythos-5-1", &[], true),
    ]
}

/// Construct the built-in Anthropic provider profile with caller-selected auth.
#[must_use]
pub fn anthropic_provider_profile(
    api_base: &str,
    auth: AuthStrategy,
    credential: CredentialConfig,
) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: lingxi_llm_client::protocol::Region::all(),
        provider_id: ProviderId::AnthropicFirstParty,
        profile_name: "anthropic".to_string(),
        base_url: api_base.to_string(),
        protocol: ProtocolFamily::AnthropicMessages,
        auth,
        credential,
        models: anthropic_model_profiles(),
        pricing: PricingConfig {
            billing_mode: platform_api::ModelBillingMode::PerToken,
            ..PricingConfig::default()
        },
        signing: None,
        azure: None,
        supports_websockets: false,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: None,
        vision_delegate: None,
        connection: Default::default(),
    }
}

/// Split a user model reference into `(profile, model)` with Claude Code
/// back-compat semantics.
#[must_use]
pub fn split_profile_model(model_ref: &str) -> (String, String) {
    if model_ref.starts_with("claude-") {
        return ("anthropic".to_string(), model_ref.to_string());
    }
    match model_ref.split_once('/') {
        Some((profile, bare)) if !profile.is_empty() && !bare.is_empty() => {
            (profile.to_string(), bare.to_string())
        }
        _ => ("anthropic".to_string(), model_ref.to_string()),
    }
}

/// Normalize a resolved provider profile to the pricing provider identity used
/// by shared pricing catalogs.
#[must_use]
pub fn pricing_provider_id_for_profile(profile_name: &str, provider_id: &ProviderId) -> ProviderId {
    match profile_name {
        "anthropic" => ProviderId::AnthropicFirstParty,
        "openai" | "azure" => ProviderId::OpenAI,
        "gemini" | "vertex" => ProviderId::Gemini,
        "bedrock" => ProviderId::BedrockClaude,
        _ => match provider_id {
            ProviderId::AzureOpenAI | ProviderId::OpenAI => ProviderId::OpenAI,
            ProviderId::VertexGemini | ProviderId::VertexClaude | ProviderId::Gemini => {
                ProviderId::Gemini
            }
            ProviderId::BedrockClaude => ProviderId::BedrockClaude,
            // Foundry hosts Claude models — price them via the Anthropic catalog.
            ProviderId::FoundryClaude | ProviderId::AnthropicFirstParty => {
                ProviderId::AnthropicFirstParty
            }
            // One vendor may expose the same model both as a metered API and
            // as a subscription (for example zai and zai-coding). Prices are
            // attached to the connection profile, not the vendor identity.
            ProviderId::OpenAICompatible { .. } => ProviderId::OpenAICompatible {
                name: profile_name.to_string(),
            },
            ProviderId::Custom { .. } => ProviderId::Custom {
                name: profile_name.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(name: &str, value: Value) -> BTreeMap<String, Value> {
        let mut providers = BTreeMap::new();
        providers.insert(name.to_string(), value);
        providers
    }

    #[test]
    fn deferred_credentials_allow_absent_env_but_env_mode_requires_it() {
        let providers = one(
            "custom",
            json!({
                "type": "openai", "baseUrl": "https://example.com/v1",
                "models": [{"id": "test-model"}]
            }),
        );
        let parsed = parse_provider_profiles_strict(
            &providers,
            ProviderParseOptions::lenient_deferred_static(),
        )
        .expect("secure host credentials do not require an environment variable");
        assert_eq!(parsed[0].env_var, None);
        assert_eq!(parsed[0].profile.credential, CredentialConfig::None);
        assert!(
            parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env()).is_err()
        );
    }

    /// The current catalog carries Mythos 5.1 immediately after Fable 5.1.
    #[test]
    fn anthropic_catalog_uses_fable_and_mythos_5_1() {
        let profiles = anthropic_model_profiles();
        let fable = profiles
            .iter()
            .find(|p| p.display_model == "claude-fable-5-1")
            .expect("fable-5.1 present in catalog");
        assert_eq!(fable.metadata.context_window_tokens, Some(1_000_000));
        assert_eq!(fable.metadata.max_output_tokens, Some(128_000));
        let pricing = fable.metadata.pricing.as_ref().expect("official pricing");
        assert_eq!(pricing.input_per_million, Some(10.0));
        assert_eq!(pricing.output_per_million, Some(50.0));
        assert_eq!(pricing.cache_read_per_million, Some(0.25));
        assert!(!profiles.iter().any(|p| p.display_model == "claude-fable-5"));

        let mythos = profiles
            .iter()
            .find(|p| p.display_model == "claude-mythos-5-1")
            .expect("mythos-5.1 present in catalog");
        assert_eq!(mythos.billing_model, "claude-mythos-5-1");
        assert!(mythos.aliases.is_empty());
        assert!(mythos.capabilities.reasoning);
        // Ordered immediately after fable-5.1.
        let fable_idx = profiles
            .iter()
            .position(|p| p.display_model == "claude-fable-5-1")
            .expect("fable-5.1 present");
        let mythos_idx = profiles
            .iter()
            .position(|p| p.display_model == "claude-mythos-5-1")
            .expect("mythos-5.1 present");
        assert_eq!(mythos_idx, fable_idx + 1);
    }

    /// WP11: `anthropic_model_profiles()` previously hard-coded
    /// `structured_output: false` for every Anthropic model — a fill-in gap,
    /// not a real capability limit (the Anthropic codec fully encodes
    /// `ResponseFormat::JsonSchema` via `output_config` and the
    /// `structured-outputs-2025-12-15` beta). Pin every model's bit to
    /// `true`, not just one, so a future regression that flips only some
    /// rows back to `false` is caught.
    #[test]
    fn every_anthropic_model_supports_structured_output() {
        let profiles = anthropic_model_profiles();
        assert!(
            !profiles.is_empty(),
            "anthropic_model_profiles() must not be empty"
        );
        let unsupported: Vec<&str> = profiles
            .iter()
            .filter(|m| !m.capabilities.structured_output)
            .map(|m| m.display_model.as_str())
            .collect();
        assert!(
            unsupported.is_empty(),
            "every Anthropic model must advertise structured_output: true, \
             found unsupported: {unsupported:?}"
        );
    }

    #[test]
    fn strict_parser_covers_all_provider_kinds_with_explicit_identities() {
        let providers: BTreeMap<String, Value> = serde_json::from_value(json!({
            "openai_chat": {
                "type": "openai",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "models": [{"id": "gpt-4o"}]
            },
            "openai_responses": {
                "type": "openai-responses",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "models": [{"id": "gpt-5"}]
            },
            "anthropic_user": {
                "type": "anthropic",
                "baseUrl": "https://api.anthropic.com",
                "apiKeyEnv": "ANTHROPIC_API_KEY",
                "models": [{"id": "claude-sonnet-4-20250514"}]
            },
            "gemini_user": {
                "type": "gemini",
                "baseUrl": "https://generativelanguage.googleapis.com/v1beta",
                "apiKeyEnv": "GEMINI_API_KEY",
                "models": [{"id": "gemini-2.5-pro"}]
            },
            "azure_user": {
                "type": "azure-openai",
                "baseUrl": "https://example.openai.azure.com",
                "apiKeyEnv": "AZURE_OPENAI_API_KEY",
                "apiVersion": "2024-02-01",
                "models": [{"id": "gpt-4o"}]
            },
            "bedrock_user": {
                "type": "bedrock-claude",
                "region": "us-east-1",
                "models": [{"id": "anthropic.claude-3-5-sonnet-20241022-v2:0"}]
            },
            "vertex_claude_user": {
                "type": "vertex-claude",
                "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
                "apiKeyEnv": "GOOGLE_OAUTH_TOKEN",
                "models": [{"id": "claude-sonnet-4@20250514"}]
            },
            "vertex_gemini_user": {
                "type": "vertex-gemini",
                "baseUrl": "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
                "apiKeyEnv": "GOOGLE_OAUTH_TOKEN",
                "models": [{"id": "gemini-2.5-pro"}]
            }
        }))
        .unwrap();

        let parsed = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("providers parse");
        let by_name: BTreeMap<_, _> = parsed
            .iter()
            .map(|p| (p.profile.profile_name.as_str(), &p.profile))
            .collect();

        assert_eq!(
            by_name["openai_chat"].provider_id,
            ProviderId::OpenAICompatible {
                name: "openai_chat".to_string()
            }
        );
        assert_eq!(
            by_name["openai_responses"].protocol,
            ProtocolFamily::OpenAiResponses
        );
        assert_eq!(
            by_name["anthropic_user"].provider_id,
            ProviderId::AnthropicFirstParty
        );
        assert_eq!(by_name["gemini_user"].provider_id, ProviderId::Gemini);
        assert_eq!(by_name["azure_user"].provider_id, ProviderId::AzureOpenAI);
        assert_eq!(
            by_name["bedrock_user"].provider_id,
            ProviderId::BedrockClaude
        );
        assert_eq!(
            by_name["vertex_claude_user"].provider_id,
            ProviderId::VertexClaude
        );
        assert_eq!(
            by_name["vertex_gemini_user"].provider_id,
            ProviderId::VertexGemini
        );
        assert_eq!(by_name["azure_user"].auth, AuthStrategy::AzureToken);
        assert_eq!(by_name["bedrock_user"].auth, AuthStrategy::AwsSigV4);
        assert_eq!(
            by_name["bedrock_user"].base_url,
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
        assert_eq!(
            by_name["bedrock_user"]
                .signing
                .as_ref()
                .map(|s| s.service.as_str()),
            Some("bedrock")
        );
    }

    /// `foundry-claude` parses to the Foundry identity/protocol with `x-api-key`
    /// (ApiKey) auth (parity 2.1.207 H-BIN-10). Pricing normalizes to Anthropic.
    #[test]
    fn foundry_claude_kind_parses_to_foundry_identity() {
        let providers = one(
            "foundry_user",
            json!({
                "type": "foundry-claude",
                "baseUrl": "https://my-res.services.ai.azure.com/anthropic/",
                "apiKeyEnv": "ANTHROPIC_FOUNDRY_API_KEY",
                "models": [{"id": "claude-sonnet-4-5"}]
            }),
        );

        let parsed = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("providers parse");
        let profile = &parsed[0].profile;

        assert_eq!(profile.provider_id, ProviderId::FoundryClaude);
        assert_eq!(profile.protocol, ProtocolFamily::FoundryClaude);
        assert_eq!(profile.auth, AuthStrategy::ApiKey);
        assert_eq!(
            profile.base_url,
            "https://my-res.services.ai.azure.com/anthropic/"
        );
        // Claude-on-Foundry bills through the Anthropic pricing namespace.
        assert_eq!(
            pricing_provider_id_for_profile("foundry_user", &profile.provider_id),
            ProviderId::AnthropicFirstParty
        );
    }

    #[test]
    fn lenient_parser_skips_invalid_provider_with_warning() {
        let providers = one("bad", json!({ "type": "cohere", "baseUrl": "https://x" }));

        let (parsed, warnings) = parse_provider_profiles_lenient(
            &providers,
            ProviderParseOptions::lenient_deferred_static(),
        );

        assert!(parsed.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown type"));
        assert!(warnings[0].contains("skipped"));
    }

    #[test]
    fn strict_parser_returns_invalid_request() {
        let providers = one("bad", json!({ "type": "openai", "baseUrl": "https://x" }));

        let err = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect_err("missing apiKeyEnv should error");

        assert!(
            matches!(err, LlmError::InvalidRequest { ref message } if message.contains("apiKeyEnv")),
            "got {err:?}"
        );
    }

    #[test]
    fn credential_modes_switch_between_env_and_deferred() {
        let providers = one(
            "groq",
            json!({
                "type": "openai",
                "baseUrl": "https://api.groq.com/openai/v1",
                "apiKeyEnv": "GROQ_API_KEY",
                "models": [{"id": "llama-3.3-70b"}]
            }),
        );

        let strict = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("strict parse");
        assert_eq!(
            strict[0].profile.credential,
            CredentialConfig::Env {
                var: "GROQ_API_KEY".to_string()
            }
        );

        let (lenient, warnings) = parse_provider_profiles_lenient(
            &providers,
            ProviderParseOptions::lenient_deferred_static(),
        );
        assert!(warnings.is_empty());
        assert_eq!(lenient[0].profile.credential, CredentialConfig::None);
        assert_eq!(lenient[0].env_var.as_deref(), Some("GROQ_API_KEY"));
    }

    #[test]
    fn model_less_lenient_entry_is_preserved_for_assemble_to_drop() {
        let providers = one(
            "listing",
            json!({
                "type": "openai",
                "baseUrl": "https://x",
                "apiKeyEnv": "OPENAI_API_KEY"
            }),
        );

        let (parsed, warnings) = parse_provider_profiles_lenient(
            &providers,
            ProviderParseOptions::lenient_deferred_static(),
        );

        assert!(warnings.is_empty());
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].profile.models.is_empty());
    }

    #[test]
    fn websocket_flags_only_parse_for_openai_responses() {
        let providers = one(
            "responses",
            json!({
                "type": "openai-responses",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "supportsWebsockets": true,
                "websocketConnectTimeoutMs": 1500,
                "models": [{"id": "gpt-5"}]
            }),
        );

        let parsed = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("providers parse");

        assert!(parsed[0].profile.supports_websockets);
        assert_eq!(parsed[0].profile.websocket_connect_timeout_ms, Some(1500));
    }

    #[test]
    fn websocket_flags_reject_invalid_protocols_and_compression() {
        let chat = one(
            "chat",
            json!({
                "type": "openai",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "supportsWebsockets": true,
                "models": [{"id": "gpt-4o"}]
            }),
        );
        let err = parse_provider_profiles_strict(&chat, ProviderParseOptions::strict_env())
            .expect_err("chat websocket should reject");
        assert!(
            matches!(err, LlmError::InvalidRequest { ref message } if message.contains("openai-responses"))
        );

        let compression = one(
            "responses",
            json!({
                "type": "openai-responses",
                "baseUrl": "https://api.openai.com/v1",
                "apiKeyEnv": "OPENAI_API_KEY",
                "supportsWebsockets": true,
                "supportsWebsocketCompression": true,
                "models": [{"id": "gpt-5"}]
            }),
        );
        let err = parse_provider_profiles_strict(&compression, ProviderParseOptions::strict_env())
            .expect_err("compression should reject");
        assert!(
            matches!(err, LlmError::InvalidRequest { ref message } if message.contains("not supported"))
        );
    }

    #[test]
    fn pricing_overrides_parse_and_validate() {
        let providers = one(
            "groq",
            json!({
                "type": "openai",
                "baseUrl": "https://api.groq.com/openai/v1",
                "apiKeyEnv": "GROQ_API_KEY",
                "models": [{"id": "llama"}],
                "pricing": {
                    "llama": {
                        "inputPerMtok": 1.0,
                        "outputPerMtok": 2.0,
                        "cacheReadPerMtok": 0.25
                    }
                }
            }),
        );

        let parsed = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("providers parse");

        assert_eq!(parsed[0].profile.pricing.overrides.len(), 1);
        let (_, pricing) = &parsed[0].profile.pricing.overrides[0];
        assert_eq!(pricing.input_per_million, 1.0);
        assert_eq!(pricing.output_per_million, 2.0);
        assert_eq!(pricing.cache_read_per_million, 0.25);
        assert_eq!(pricing.reasoning_per_million, 2.0);
        let display = parsed[0].profile.models[0]
            .metadata
            .pricing
            .as_ref()
            .expect("display pricing override");
        assert_eq!(
            display.billing_mode,
            platform_api::ModelBillingMode::PerToken
        );
        assert_eq!(display.input_per_million, Some(1.0));
        assert_eq!(display.output_per_million, Some(2.0));
        assert_eq!(display.cache_read_per_million, Some(0.25));
        assert_eq!(display.cache_write_per_million, None);
        assert_eq!(display.reasoning_per_million, None);
        assert_eq!(display.source.as_deref(), Some("userOverride"));
    }

    #[test]
    fn explicit_zero_price_remains_distinct_from_missing_optional_prices() {
        let providers = one(
            "free-output",
            json!({
                "type": "openai",
                "baseUrl": "https://example.com/v1",
                "apiKeyEnv": "EXAMPLE_API_KEY",
                "billingMode": "subscription",
                "models": [{"id": "custom"}],
                "pricing": {
                    "custom": {
                        "inputPerMtok": 1.0,
                        "outputPerMtok": 0.0,
                        "cacheReadPerMtok": 0.0
                    }
                }
            }),
        );
        let parsed = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("provider parses");
        let profile = &parsed[0].profile;
        assert_eq!(
            profile.pricing.billing_mode,
            platform_api::ModelBillingMode::PerToken
        );
        let display = profile.models[0]
            .metadata
            .pricing
            .as_ref()
            .expect("display pricing override");
        assert_eq!(display.output_per_million, Some(0.0));
        assert_eq!(display.cache_read_per_million, Some(0.0));
        assert_eq!(display.cache_write_per_million, None);
        assert_eq!(display.reasoning_per_million, None);
    }

    #[test]
    fn custom_model_metadata_and_billing_mode_round_trip_into_profile() {
        let providers = one(
            "custom",
            json!({
                "type": "openai",
                "baseUrl": "https://example.com/v1",
                "apiKeyEnv": "CUSTOM_API_KEY",
                "billingMode": "subscription",
                "models": [{
                    "id": "custom-vision",
                    "capabilities": {"vision": true, "reasoning": true},
                    "metadata": {
                        "status": "beta",
                        "inputModalities": ["text", "image"],
                        "outputModalities": ["text"],
                        "contextWindowTokens": 128000,
                        "maxOutputTokens": 32000
                    }
                }]
            }),
        );
        let parsed = parse_provider_profiles_strict(&providers, ProviderParseOptions::strict_env())
            .expect("provider metadata parses");
        let profile = &parsed[0].profile;
        assert_eq!(
            profile.pricing.billing_mode,
            platform_api::ModelBillingMode::Subscription
        );
        let model = &profile.models[0];
        assert_eq!(model.metadata.status.as_deref(), Some("beta"));
        assert_eq!(model.metadata.context_window_tokens, Some(128_000));
        assert_eq!(model.metadata.max_input_tokens, None);
        assert!(model.capabilities.vision);
    }

    #[test]
    fn anthropic_profile_helper_preserves_model_table() {
        let profile = anthropic_provider_profile(
            "https://api.anthropic.com",
            AuthStrategy::ApiKey,
            CredentialConfig::Env {
                var: "ANTHROPIC_API_KEY".to_string(),
            },
        );

        assert_eq!(profile.profile_name, "anthropic");
        // Fifteen first-party entries, including current Fable/Mythos 5.1.
        assert_eq!(profile.models.len(), 15);
        assert!(profile
            .models
            .iter()
            .any(|m| m.display_model == "claude-opus-4-7"));
        assert!(profile
            .models
            .iter()
            .any(|m| m.display_model == "claude-sonnet-5"));
        assert!(profile.models.iter().all(|m| m.capabilities.reasoning));
    }

    #[test]
    fn split_profile_model_matches_claude_code_backcompat() {
        assert_eq!(
            split_profile_model("openai/gpt-4o"),
            ("openai".to_string(), "gpt-4o".to_string())
        );
        assert_eq!(
            split_profile_model("claude-3-5/sonnet"),
            ("anthropic".to_string(), "claude-3-5/sonnet".to_string())
        );
        assert_eq!(
            split_profile_model("some-model"),
            ("anthropic".to_string(), "some-model".to_string())
        );
    }

    #[test]
    fn pricing_provider_helper_normalizes_managed_clouds() {
        assert_eq!(
            pricing_provider_id_for_profile("azure-user", &ProviderId::AzureOpenAI),
            ProviderId::OpenAI
        );
        assert_eq!(
            pricing_provider_id_for_profile("vertex-claude-user", &ProviderId::VertexClaude),
            ProviderId::Gemini
        );
        assert_eq!(
            pricing_provider_id_for_profile("bedrock-user", &ProviderId::BedrockClaude),
            ProviderId::BedrockClaude
        );
        assert_eq!(
            pricing_provider_id_for_profile(
                "groq",
                &ProviderId::OpenAICompatible {
                    name: "groq".to_string()
                }
            ),
            ProviderId::OpenAICompatible {
                name: "groq".to_string()
            }
        );
        assert_eq!(
            pricing_provider_id_for_profile(
                "zai-coding",
                &ProviderId::OpenAICompatible {
                    name: "zhipu".to_string()
                }
            ),
            ProviderId::OpenAICompatible {
                name: "zai-coding".to_string()
            }
        );
    }
}
