//! Credential-free projections of already resolved SDK conversation routes.
use super::{TeamProcessError, TeamProcessModel};
use lingxi_llm_client::protocol as wire;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const PRIVATE_CREDENTIAL: &str = "team-selected-model";
const MAX_PROFILE: usize = 64 * 1024;

/// A native-catalog selection, not a settings/provider-defaults decoder. The
/// external credential id selects one host key; the wire profile uses a private
/// slot and never contains that key or sibling credentials.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamModelRoute {
    pub model_provider: String,
    pub model_protocol: wire::ProtocolFamily,
    pub model_id: String,
    pub model_endpoint: String,
    pub credential_id: String,
    pub model_profile: wire::ProviderProfile,
}

pub(super) fn endpoint(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        let loopback = url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}
fn credential_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn static_protocol(profile: &wire::ProviderProfile) -> bool {
    (matches!(
        profile.auth,
        wire::AuthStrategy::ApiKey | wire::AuthStrategy::Bearer
    ) || (profile.auth == wire::AuthStrategy::AzureToken
        && profile.protocol == wire::ProtocolFamily::AzureOpenAi))
        && (profile.protocol != wire::ProtocolFamily::AzureOpenAi
            || profile.azure.as_ref().is_some_and(|azure| {
                azure.api_version.as_ref().is_some_and(|version| {
                    !version.is_empty()
                        && version.len() <= 128
                        && !version.chars().any(char::is_control)
                })
            }))
        && matches!(
            profile.protocol,
            wire::ProtocolFamily::AnthropicMessages
                | wire::ProtocolFamily::OpenAiChat
                | wire::ProtocolFamily::OpenAiResponses
                | wire::ProtocolFamily::GeminiGenerateContent
                | wire::ProtocolFamily::GeminiInteractions
                | wire::ProtocolFamily::FoundryClaude
                | wire::ProtocolFamily::AzureOpenAi
        )
        && profile.chat_enabled
}
pub(super) fn sanitized(
    mut profile: wire::ProviderProfile,
    model: &str,
) -> Result<wire::ProviderProfile, TeamProcessError> {
    if !static_protocol(&profile)
        || !endpoint(&profile.base_url)
        || !name(&profile.profile_name)
        || !name(model)
    {
        return Err(TeamProcessError::Model);
    }
    if matches!(profile.credential, wire::CredentialConfig::Static { .. }) {
        return Err(TeamProcessError::Model);
    }
    let mut models = profile
        .models
        .iter()
        .filter(|row| row.request_model == model)
        .cloned();
    let mut selected = models.next().ok_or(TeamProcessError::Model)?;
    if models.next().is_some()
        || selected.capability_support_for(wire::ModelCapability::Tools)
            != wire::CapabilitySupport::Supported
        || selected.capability_support_for(wire::ModelCapability::Streaming)
            != wire::CapabilitySupport::Supported
    {
        return Err(TeamProcessError::Model);
    }
    selected.info.features = selected.info.features.on_connection(&profile.info.features);
    selected.aliases.clear();
    // Only reviewed codec flags survive. Arbitrary body/headers can carry
    // credentials, hosted tools, instructions or another response's state.
    let mut extra = serde_json::Map::new();
    for key in ["supports_previous_response_id", "chat_pdf_only"] {
        if let Some(value) = profile.extra.get(key).and_then(Value::as_bool) {
            extra.insert(key.into(), value.into());
        }
    }
    if let Some(header) = profile
        .extra
        .get("credential_header")
        .and_then(Value::as_str)
    {
        if header.is_empty()
            || header.len() > 128
            || !header
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(TeamProcessError::Model);
        }
        extra.insert("credential_header".into(), header.into());
    }
    profile.credential = wire::CredentialConfig::HostManaged {
        key: PRIVATE_CREDENTIAL.into(),
    };
    let value = json!({
        "provider_id":profile.provider_id,"profile_name":profile.profile_name,
        "protocol":profile.protocol,"base_url":profile.base_url,"auth":profile.auth,
        "credential":profile.credential,"regions":wire::Region::all(),
        "inference":profile.inference,"info":{"features":profile.info.features},
        "models":[selected],"pricing":profile.pricing,"extra":extra,"azure":profile.azure,
    });
    let body = serde_json::to_vec(&value).map_err(|_| TeamProcessError::Model)?;
    if body.len() > MAX_PROFILE {
        return Err(TeamProcessError::Model);
    }
    let frozen: wire::ProviderProfile =
        serde_json::from_slice(&body).map_err(|_| TeamProcessError::Model)?;
    if serde_json::to_vec(&frozen)
        .map_err(|_| TeamProcessError::Model)?
        .len()
        > MAX_PROFILE
    {
        return Err(TeamProcessError::Model);
    }
    Ok(frozen)
}
impl TeamModelRoute {
    pub fn validate(&self) -> Result<(), TeamProcessError> {
        if !credential_id(&self.credential_id)
            || self.model_provider != self.model_profile.profile_name
            || self.model_protocol != self.model_profile.protocol
            || self.model_endpoint != self.model_profile.base_url
            || self.model_profile != sanitized(self.model_profile.clone(), &self.model_id)?
        {
            return Err(TeamProcessError::Model);
        }
        let host = self.host_profile()?;
        let registry = llm_runtime::ModelRegistry::from_config(llm_runtime::ClientConfig {
            providers: vec![host],
        })
        .map_err(|_| TeamProcessError::Model)?;
        let route = registry
            .resolve_in(&self.model_id, Some(&self.model_provider))
            .map_err(|_| TeamProcessError::Model)?;
        if route.request_model != self.model_id
            || route.profile_name != self.model_provider
            || !route.connection_chain.is_empty()
        {
            return Err(TeamProcessError::Model);
        }
        Ok(())
    }
    pub(crate) fn host_profile(&self) -> Result<llm_runtime::ProviderProfile, TeamProcessError> {
        let mut profile = llm_runtime::catalog_from_profiles(vec![self.model_profile.clone()])
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == self.model_provider)
            .ok_or(TeamProcessError::Model)?;
        profile.credential = llm_runtime::CredentialConfig::Static {
            id: PRIVATE_CREDENTIAL.into(),
        };
        profile.vision_delegate = None;
        profile.connection = Default::default();
        Ok(profile)
    }
}

/// Project the running catalog without another boot, credential read or network
/// request. Native default-format resolution precedes removal of all siblings.
pub(crate) fn routes(
    client: &llm_runtime::ModelRuntime,
    ids: &BTreeMap<String, String>,
    region: wire::Region,
) -> BTreeMap<(String, String), TeamModelRoute> {
    let listings = client.available_models();
    let mut identities = BTreeMap::new();
    for listing in &listings {
        *identities
            .entry((listing.profile_name.clone(), listing.request_model.clone()))
            .or_insert(0usize) += 1;
    }
    let profiles = client.provider_service_profiles();
    let catalog = lingxi_llm_client::client::RoutingCatalog::new(profiles, region);
    listings
        .into_iter()
        .filter_map(|listing| {
            if identities.get(&(listing.profile_name.clone(), listing.request_model.clone()))
                != Some(&1)
            {
                return None;
            }
            let resolved = catalog
                .resolve_in_prefer_native(&listing.display_model, Some(&listing.profile_name))
                .ok()?;
            let id = ids.get(&listing.profile_name)?;
            if !credential_id(id) || ids.get(&resolved.profile_name)? != id {
                return None;
            }
            let profile = catalog
                .profiles()
                .iter()
                .find(|p| p.profile_name == resolved.profile_name)?
                .clone();
            let profile = sanitized(profile, &resolved.request_model).ok()?;
            let route = TeamModelRoute {
                model_provider: profile.profile_name.clone(),
                model_protocol: profile.protocol,
                model_id: resolved.request_model,
                model_endpoint: profile.base_url.clone(),
                credential_id: id.clone(),
                model_profile: profile,
            };
            route.validate().ok()?;
            Some(((listing.profile_name, listing.request_model), route))
        })
        .collect()
}

impl TeamProcessModel {
    /// Consume one exact native route and one explicitly selected static key.
    pub fn selected_api_key(
        route: TeamModelRoute,
        api_key: String,
    ) -> Result<Self, TeamProcessError> {
        route.validate()?;
        if api_key.trim().is_empty()
            || api_key.len() > 8192
            || api_key.chars().any(char::is_control)
        {
            return Err(TeamProcessError::Model);
        }
        Ok(Self {
            model_id: route.model_id.clone(),
            endpoint: route.model_endpoint.clone(),
            api_key,
            route,
        })
    }
    /// The immutable nonsecret descriptor, suitable for trusted pipe fixtures.
    #[must_use]
    pub fn route(&self) -> &TeamModelRoute {
        &self.route
    }
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile(name: &str, protocol: &str, model: &str) -> wire::ProviderProfile {
        serde_json::from_value(json!({
            "provider_id":"fixture","profile_name":name,"base_url":"https://fixture.invalid/v1",
            "protocol":protocol,"auth":"api_key","credential":{"source":"host_managed","key":PRIVATE_CREDENTIAL},
            "models":[{"display_model":model,"request_model":model,"billing_model":model,
            "capability_support":{"streaming":"supported","tools":"supported"}}],"extra":{}
        })).unwrap()
    }
    fn route(profile: wire::ProviderProfile, model: &str) -> TeamModelRoute {
        TeamModelRoute {
            model_provider: profile.profile_name.clone(),
            model_protocol: profile.protocol,
            model_id: model.into(),
            model_endpoint: profile.base_url.clone(),
            credential_id: "fixture-key".into(),
            model_profile: profile,
        }
    }
    #[test]
    fn builtin_openai_luna_uses_its_real_default_responses_route() {
        let assembled = provider_config::assemble(provider_config::AssembleInputs {
            official_catalog: None,
            anthropic_api_base: "https://api.anthropic.com".into(),
            anthropic_models: Vec::new(),
            anthropic_has_api_key: false,
            anthropic_has_oauth: false,
            user_providers: BTreeMap::new(),
            routing: None,
        });
        let ids = assembled
            .credential_sources
            .iter()
            .map(|source| (source.profile_name.clone(), source.credential_id.clone()))
            .collect();
        let client = llm_runtime::ModelRuntime::from_config(assembled.client_config).unwrap();
        let projected = routes(&client, &ids, wire::Region::International);
        let selected = projected
            .get(&("openai".into(), "gpt-6-luna".into()))
            .expect("existing OpenAI API catalog must project the exact Luna route");
        assert_eq!(selected.model_id, "gpt-6-luna");
        assert_eq!(
            selected.model_protocol,
            wire::ProtocolFamily::OpenAiResponses
        );
        assert_eq!(selected.credential_id, "openai");
        assert_eq!(selected.model_profile.models.len(), 1);
        assert_eq!(selected.model_profile.models[0].request_model, "gpt-6-luna");
        assert!(!selected.model_provider.contains("chatgpt"));
        selected.validate().unwrap();
    }

    #[test]
    fn existing_static_codecs_share_one_frozen_route_contract() {
        for protocol in [
            "foundry_claude",
            "anthropic_messages",
            "open_ai_chat",
            "open_ai_responses",
            "gemini_generate_content",
            "gemini_interactions",
        ] {
            let model = "vendor/model-with-slashes";
            let selected = route(profile("fixture:connection#1", protocol, model), model);
            selected.validate().unwrap();
            let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
                providers: vec![selected.host_profile().unwrap()],
            })
            .unwrap();
            assert_eq!(client.available_models().len(), 1);
            assert_eq!(
                client
                    .protocol_for_model(model, Some(&selected.model_provider))
                    .unwrap(),
                serde_json::from_value(json!(protocol)).unwrap()
            );
            assert!(TeamProcessModel::selected_api_key(selected, "synthetic-key".into()).is_ok());
        }
    }
    #[test]
    fn projection_strips_owner_routes_and_worker_rejects_unsanitized_profiles() {
        let mut source = profile("fixture", "open_ai_responses", "gpt-fixture");
        source.extra = json!({"headers":{"Authorization":"MUST_NOT_ESCAPE"}, "body":{"instructions":"OWNER_PROMPT","tools":[{"type":"web_search"}],"previous_response_id":"OWNER_RESPONSE"},"supports_previous_response_id":true});
        source
            .connection
            .protocol_profiles
            .insert(wire::ProtocolFamily::OpenAiChat, "other".into());
        source.connection.default_protocol = Some(wire::ProtocolFamily::OpenAiChat);
        assert!(route(source.clone(), "gpt-fixture").validate().is_err());
        let frozen = sanitized(source, "gpt-fixture").unwrap();
        let json = serde_json::to_string(&frozen).unwrap();
        for secret in [
            "MUST_NOT_ESCAPE",
            "OWNER_PROMPT",
            "OWNER_RESPONSE",
            "web_search",
        ] {
            assert!(!json.contains(secret));
        }
        assert_eq!(frozen.extra, json!({"supports_previous_response_id":true}));
        assert!(frozen.connection.protocol_profiles.is_empty());
        route(frozen, "gpt-fixture").validate().unwrap();
    }
    #[test]
    fn static_selection_rejects_oauth_unknown_model_and_unsafe_endpoints() {
        let mut selected = route(
            profile("fixture", "open_ai_responses", "gpt-fixture"),
            "gpt-fixture",
        );
        selected.model_profile.auth = wire::AuthStrategy::ChatGptOAuth;
        assert!(selected.validate().is_err());
        selected.model_profile.auth = wire::AuthStrategy::ApiKey;
        selected.model_id = "unknown".into();
        assert!(selected.validate().is_err());
        for endpoint in [
            "http://remote.invalid/v1",
            "https://user:key@fixture.invalid",
            "https://fixture.invalid/v1?q=secret",
            "https://fixture.invalid/#secret",
        ] {
            assert!(!super::endpoint(endpoint));
        }
        for endpoint in [
            "https://fixture.invalid/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:1234/v1",
        ] {
            assert!(super::endpoint(endpoint));
        }
    }
    #[test]
    fn azure_static_api_key_keeps_its_existing_codec_and_api_version() {
        let mut source = profile("azure-fixture", "azure_open_ai", "deployment-fixture");
        source.auth = wire::AuthStrategy::AzureToken;
        source.azure = serde_json::from_value(json!({"api_version":"2024-02-01"})).unwrap();
        let selected = route(source.clone(), "deployment-fixture");
        selected.validate().unwrap();
        assert_eq!(selected.model_profile.azure, source.azure);
        source.azure = None;
        assert!(route(source, "deployment-fixture").validate().is_err());
    }

    #[test]
    fn exact_host_key_slot_uses_the_existing_broker_grammar() {
        for id in ["openai.prod", "key.primary", "key-1", "a_1", "1key"] {
            assert!(credential_id(id));
        }
        for id in ["UPPER", ".prefix", "key:other", "key/other", ""] {
            assert!(!credential_id(id));
        }
        assert!(!credential_id(&"a".repeat(65)));
    }

    #[test]
    fn native_default_format_uses_exact_host_slot_and_never_a_sibling_key() {
        let mut chat = profile("fixture", "open_ai_chat", "gpt-fixture");
        let responses = profile("fixture-responses", "open_ai_responses", "gpt-fixture");
        chat.connection.default_protocol = Some(wire::ProtocolFamily::OpenAiResponses);
        chat.connection.protocol_profiles.insert(
            wire::ProtocolFamily::OpenAiResponses,
            responses.profile_name.clone(),
        );
        let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
            providers: llm_runtime::catalog_from_profiles(vec![chat, responses]).providers,
        })
        .unwrap();
        let mut ids = BTreeMap::from([
            ("fixture".into(), "shared-key".into()),
            ("fixture-responses".into(), "shared-key".into()),
        ]);
        let projected = routes(&client, &ids, wire::Region::International);
        let selected = projected
            .get(&("fixture".into(), "gpt-fixture".into()))
            .unwrap();
        assert_eq!(selected.model_provider, "fixture-responses");
        assert_eq!(
            selected.model_protocol,
            wire::ProtocolFamily::OpenAiResponses
        );
        ids.insert("fixture-responses".into(), "another-key".into());
        assert!(!routes(&client, &ids, wire::Region::International)
            .contains_key(&("fixture".into(), "gpt-fixture".into())));
    }
}
