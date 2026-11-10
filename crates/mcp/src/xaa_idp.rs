//! XAA IdP login — the config + browser-login surface that feeds the
//! Cross-App-Access token-exchange chain ([`crate::xaa`]).
//!
//! Faithful core port of `claude-code/src/services/mcp/xaaIdpLogin.ts` (489
//! lines) plus the assembly half of `performMCPXaaAuth`
//! (`auth.ts:664-744`). This is the seam the engine wires so that an
//! `oauth.xaa==Some(true)` server can resolve a token without a per-server
//! consent screen:
//!
//! - [`XaaIdpSettings`] — the user-level `xaaIdp` settings (`{issuer, clientId,
//!   callbackPort}`), parsed from the merged settings tiers
//!   (`getXaaIdpSettings`, xaaIdpLogin.ts:47-49).
//! - [`discover_oidc`] — OIDC discovery against
//!   `{issuer}/.well-known/openid-configuration`, returning the
//!   authorization + token endpoints (`discoverOidc`, xaaIdpLogin.ts:202-237).
//! - id_token cache — keyed by normalized issuer in [`SecureStorage`], with the
//!   `exp`-claim TTL + 60s expiry buffer (`getCachedIdpIdToken` /
//!   `saveIdpIdToken`, xaaIdpLogin.ts:99-141).
//! - [`acquire_idp_id_token`] — return the cached id_token or run the OIDC
//!   authorization_code + PKCE browser flow once, extract `id_token` from the
//!   token response, cache it (`acquireIdpIdToken`, xaaIdpLogin.ts:401-487).
//!   Reuses the OAuth primitives in [`crate::oauth`] (PKCE/state generation,
//!   [`crate::oauth::CallbackListener`], authorize-URL build) — the only delta
//!   vs the consent flow is `scope=openid` and pulling `id_token` out.
//! - [`XaaIdpConfigProvider`] — the [`crate::registry::XaaConfigProvider`]
//!   implementation that assembles the [`crate::registry::XaaInputs`] bundle
//!   from settings, the AS `client_secret` (`mcpOAuthClientConfig[serverKey]`),
//!   the IdP client secret, the cached-or-acquired id_token, and the discovered
//!   IdP token endpoint (`performMCPXaaAuth`, auth.ts:676-744).
//!
//! Dense OAuth/OIDC vocabulary (IdP, OIDC, AS, PKCE) reads worse backticked;
//! match `xaa.rs` and opt out of the lint.
#![allow(clippy::doc_markdown)]

use crate::oauth::{self, OnAuthorizationUrl};
use crate::registry::{XaaConfigProvider, XaaInputs};
use lingxi_core::host::{Clock, HttpTransport, McpError, McpTransportSpec, SecureStorage};
use lingxi_core::types::utf16_json::{Utf16JsonBuilder, Utf16JsonProjection};
use lingxi_core::types::{HttpMethod, HttpRequest};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// IdP request deadline (xaaIdpLogin.ts `IDP_REQUEST_TIMEOUT_MS = 30000`).
const IDP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const IDP_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Expiry buffer applied to a cached id_token: a token within this window of
/// expiring is treated as a miss so the next attempt re-acquires
/// (xaaIdpLogin.ts `ID_TOKEN_EXPIRY_BUFFER_S = 60`).
const ID_TOKEN_EXPIRY_BUFFER: Duration = Duration::from_secs(60);

/// Expiry given to a pre-obtained id_token whose JWT carries no usable `exp`
/// claim (`Date.now() + 3600000`, xaaIdpLogin.ts:137).
const ID_TOKEN_NO_EXP_FALLBACK: Duration = Duration::from_secs(3600);

/// Secure-storage service for the per-issuer id_token cache. The TS keychain
/// blob nests these under `mcpXaaIdp[issuerKey]`; in the `(service, account)`
/// store we use this service with `account = issuer_key(issuer)`.
const XAA_IDP_SERVICE: &str = "xaa-idp";

/// Secure-storage service for the per-issuer IdP client secret
/// (`getIdpClientSecret` — TS `mcpXaaIdpConfig[issuerKey]`).
const XAA_IDP_CONFIG_SERVICE: &str = "xaa-idp-config";

/// Secure-storage service for the AS confidential-client config
/// (`mcpOAuthClientConfig[serverKey]`, auth.ts:1500 / 2409). `account =
/// oauth::server_key(name, spec)`.
const MCP_OAUTH_CLIENT_CONFIG_SERVICE: &str = "mcp-oauth-client-config";

// ---------------------------------------------------------------------------
// Settings (the `xaaIdp` key).
// ---------------------------------------------------------------------------

/// User-level XAA IdP connection settings (`settings.xaaIdp`,
/// xaaIdpLogin.ts:36-49).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct XaaIdpSettings {
    /// IdP issuer URL (the OIDC discovery base, and the id_token cache key).
    pub issuer: String,
    /// IdP-registered OAuth client id.
    #[serde(rename = "clientId")]
    pub client_id: String,
    /// Optional fixed loopback callback port. Used when the IdP client is
    /// pre-registered with a specific `http://localhost:{port}/callback`
    /// redirect (RFC 8252 §7.3). `None` → OS-assigned ephemeral port.
    #[serde(default, rename = "callbackPort")]
    pub callback_port: Option<u16>,
}

/// Only the `xaaIdp` slice of a settings tier matters here.
#[derive(Debug, serde::Deserialize)]
struct SettingsXaaIdpSlice {
    #[serde(default)]
    #[serde(rename = "xaaIdp")]
    xaa_idp: Option<XaaIdpSettings>,
}

impl XaaIdpSettings {
    /// Parse the merged `xaaIdp` settings from a list of raw settings-tier JSON
    /// strings (user → project → local; later tiers win). Returns `None` when
    /// no tier carries a parseable `xaaIdp` object — XAA stays opt-in.
    ///
    /// Mirrors `getXaaIdpSettings` (xaaIdpLogin.ts:47-49) over the same tier
    /// precedence the engine uses for sandbox settings
    /// (`sandbox_runtime_config_from_settings_tiers`).
    ///
    /// NOTE on the managed/policy + flag tiers: claude-code `getXaaIdpSettings`
    /// reads `getInitialSettings()` (settings.ts:674-726), which also merges a
    /// managed/policy (MDM) tier and a flag tier at the HIGHEST priority. This
    /// function takes whatever tiers the caller passes; the engine-desktop
    /// composition root deliberately passes only the user/project/local tiers —
    /// consistent with its permission and sandbox settings-VALUE readers, which
    /// also omit managed/flag. Wiring a managed settings.json *value* tier is a
    /// codebase-wide follow-up for all three consumers together, not specific to
    /// xaaIdp. See the comment at the engine `xaa_config` tier read.
    #[must_use]
    pub fn from_settings_tiers(raw_tiers: &[&str]) -> Option<Self> {
        let mut merged: Option<XaaIdpSettings> = None;
        for raw in raw_tiers {
            let Ok(parsed) = serde_json::from_str::<SettingsXaaIdpSlice>(raw) else {
                continue;
            };
            if let Some(s) = parsed.xaa_idp {
                merged = Some(s);
            }
        }
        merged
    }
}

// ---------------------------------------------------------------------------
// Issuer normalization (xaaIdpLogin.ts:84-93).
// ---------------------------------------------------------------------------

/// Native issuerKey: URL canonicalization, trailing pathname-slash removal,
/// lowercased host and retained query/fragment. URL serialization keeps the
/// root slash, unlike the XAA issuer-equality helper.
#[must_use]
pub fn issuer_key(issuer: &str) -> String {
    if let Ok(mut url) = url::Url::parse(issuer) {
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&path);
        if let Some(host) = url.host_str() {
            let host = host.to_lowercase();
            let _ = url.set_host(Some(&host));
        }
        return url.to_string();
    }
    issuer.trim_end_matches('/').to_owned()
}

// Read older LingXi cache keys during migration; writes use native URL
// canonicalization without touching other MCP/account credential services.
fn legacy_issuer_key(issuer: &str) -> String {
    let trimmed = issuer.trim_end_matches('/');
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_string();
    };
    let scheme_lc = scheme.to_ascii_lowercase();
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let authority_lc = authority.to_ascii_lowercase();
    match path {
        Some(p) => format!("{scheme_lc}://{authority_lc}/{p}"),
        None => format!("{scheme_lc}://{authority_lc}"),
    }
}

// ---------------------------------------------------------------------------
// OIDC discovery (xaaIdpLogin.ts:202-237).
// ---------------------------------------------------------------------------

/// OIDC discovery metadata (the XAA-relevant subset of
/// `OpenIdProviderDiscoveryMetadata`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct OidcMetadata {
    /// OIDC authorization endpoint (the user opens this; `response_type=code`).
    pub authorization_endpoint: String,
    /// OIDC token endpoint (`authorization_code` exchange AND the RFC 8693
    /// token-exchange target for the XAA chain).
    pub token_endpoint: String,
}

/// OIDC discovery against `{issuer}/.well-known/openid-configuration`
/// (xaaIdpLogin.ts:202-237).
///
/// OIDC Discovery §4.1 is a path *append*, not replace: a leading-slash
/// absolute reference would drop the issuer's path and break Azure AD / Okta
/// custom AS / Keycloak realms. We append `.well-known/...` to a
/// trailing-slash issuer base. The token endpoint must be HTTPS (the id_token +
/// client_secret are POSTed there).
///
/// # Errors
/// [`McpError::OAuth`] on transport failure, non-2xx, undecodable body, or a
/// non-HTTPS token endpoint.
pub async fn discover_oidc(
    http: &Arc<dyn HttpTransport>,
    issuer: &str,
) -> Result<OidcMetadata, McpError> {
    Ok(discover_oidc_document(http, issuer).await?.metadata)
}

#[derive(Debug)]
struct OidcMetadataDocument {
    metadata: OidcMetadata,
    issuer_units: Vec<u16>,
    authorization_response_iss_parameter_supported: bool,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

async fn discover_oidc_document(
    http: &Arc<dyn HttpTransport>,
    issuer: &str,
) -> Result<OidcMetadataDocument, McpError> {
    let base = if issuer.ends_with('/') {
        issuer.to_owned()
    } else {
        format!("{issuer}/")
    };
    let url = url::Url::parse(&base)
        .and_then(|base| base.join(".well-known/openid-configuration"))
        .map_err(|_| {
            McpError::OAuth(
                format!("XAA IdP: OIDC discovery failed: issuer is not a valid URL: {issuer}")
                    .into(),
            )
        })?;
    let response = http
        .request(HttpRequest {
            method: HttpMethod::Get,
            url: url.to_string(),
            headers: vec![("Accept".into(), "application/json".into())],
            body: None,
            body_bytes: None,
            timeout: Some(IDP_REQUEST_TIMEOUT),
        })
        .await
        .map_err(|error| {
            McpError::OAuth(format!("XAA IdP: OIDC discovery transport: {error}").into())
        })?;
    if !(200..300).contains(&response.status) {
        return Err(McpError::OAuth(
            format!(
                "XAA IdP: OIDC discovery failed: HTTP {} at {url}",
                response.status
            )
            .into(),
        ));
    }
    let validated =
        crate::oauth::schemas::validate_authorization(&response.body, true).map_err(|error| {
            McpError::OAuth(format!("XAA IdP: invalid OIDC metadata: {error}").into())
        })?;
    let value = validated.projection();
    let document = OidcMetadataDocument {
        metadata: OidcMetadata {
            authorization_endpoint: value
                .string_at("/authorization_endpoint")
                .expect("validated endpoint")
                .to_owned(),
            token_endpoint: value
                .string_at("/token_endpoint")
                .expect("validated endpoint")
                .to_owned(),
        },
        issuer_units: value.string_units("/issuer").expect("validated issuer"),
        authorization_response_iss_parameter_supported: value
            .get_property("authorization_response_iss_parameter_supported")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        token_endpoint_auth_methods_supported: value
            .get_property("token_endpoint_auth_methods_supported")
            .and_then(|value| value.array_entries())
            .map(|(_, entries)| {
                entries
                    .into_iter()
                    .map(|(_, value)| value.as_str().expect("validated string").to_owned())
                    .collect()
            }),
    };
    if url::Url::parse(&document.metadata.token_endpoint)
        .ok()
        .is_none_or(|endpoint| endpoint.scheme() != "https")
    {
        return Err(McpError::OAuth(
            format!(
                "XAA IdP: refusing non-HTTPS token endpoint: {}",
                document.metadata.token_endpoint
            )
            .into(),
        ));
    }
    Ok(document)
}

// ---------------------------------------------------------------------------
// id_token JWT exp parsing (xaaIdpLogin.ts:252-263).
// ---------------------------------------------------------------------------

/// Decode the `exp` claim (seconds since epoch) from a JWT without verifying
/// its signature. Returns `None` if parsing fails or `exp` is absent. Used only
/// to expose the legacy integer view; cache calculation uses exact JS numeric
/// claims, including fractional/negative values. See xaaIdpLogin.ts:239-251 for why no signature /
/// iss / aud validation is needed (the IdP validates its own token at the
/// RFC 8693 exchange).
#[must_use]
pub fn jwt_exp(jwt: &str) -> Option<u64> {
    jwt_exp_number(jwt)
        .filter(|value| {
            value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value < u64::MAX as f64
        })
        .map(|value| value as u64)
}

fn jwt_exp_number(jwt: &str) -> Option<f64> {
    use base64::Engine;
    let parts = jwt.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    // Buffer.from(payload,'base64url') accepts URL/base64 alphabets, optional
    // padding and ignored non-alphabet bytes. A trailing incomplete sextet is
    // ignored, matching Node/Bun Buffer decoding.
    let mut payload = parts[1]
        .bytes()
        .take_while(|byte| *byte != b'=')
        .filter(|byte| byte.is_ascii_alphanumeric() || b"+/-_".contains(byte))
        .map(|byte| match byte {
            b'-' => b'+',
            b'_' => b'/',
            _ => byte,
        })
        .collect::<Vec<_>>();
    if payload.len() % 4 == 1 {
        payload.pop();
    }
    while payload.len() % 4 != 0 {
        payload.push(b'=');
    }
    let decoder = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    );
    let bytes = decoder.decode(payload).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&text)
        .ok()?
        .get_property("exp")?
        .as_number()
}

fn epoch_millis(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(value) => value.as_millis() as f64,
        Err(value) => -(value.duration().as_millis() as f64),
    }
}

fn time_from_millis(milliseconds: f64) -> Option<SystemTime> {
    let duration = Duration::try_from_secs_f64(milliseconds.abs() / 1000.0).ok()?;
    if milliseconds.is_sign_negative() {
        UNIX_EPOCH.checked_sub(duration)
    } else {
        UNIX_EPOCH.checked_add(duration)
    }
}

// ---------------------------------------------------------------------------
// id_token cache (xaaIdpLogin.ts:99-150).
// ---------------------------------------------------------------------------

/// Read a cached id_token for `issuer`, or `None` if missing or within the 60s
/// expiry buffer (`getCachedIdpIdToken`, xaaIdpLogin.ts:99-107).
///
/// # Errors
/// [`McpError::OAuth`] on a storage backend error (a missing entry is `Ok(None)`).
pub async fn get_cached_id_token(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
) -> Result<Option<String>, McpError> {
    Ok(get_cached_id_token_source(storage, clock, issuer)
        .await?
        .and_then(|value| value.string_at("").map(str::to_owned)))
}

/// Exact cached IdP token text. The String API exposes its USV wire view;
/// this source API keeps unpaired UTF16 units without touching provider auth.
pub async fn get_cached_id_token_source(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
) -> Result<Option<Utf16JsonProjection>, McpError> {
    let key = issuer_key(issuer);
    let mut data = storage
        .retrieve(XAA_IDP_SERVICE, &key)
        .await
        .map_err(|error| {
            McpError::OAuth(format!("XAA IdP: id_token cache retrieve: {error}").into())
        })?;
    let legacy_key = legacy_issuer_key(issuer);
    if data.is_none() && legacy_key != key {
        data = storage
            .retrieve(XAA_IDP_SERVICE, &legacy_key)
            .await
            .map_err(|error| {
                McpError::OAuth(format!("XAA IdP: id_token cache retrieve: {error}").into())
            })?;
    }
    let Some(data) = data else { return Ok(None) };
    let cached =
        match Utf16JsonProjection::parse(&String::from_utf8_lossy(data.expose_secret_bytes())) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
    let numeric = |value: lingxi_core::types::utf16_json::JsonValueView<'_>| {
        value
            .as_number()
            .unwrap_or_else(|| crate::oauth::schemas::finite_js_number(value).unwrap_or(f64::NAN))
    };
    // Presence wins even for null/zero; only missing milliseconds use the
    // legacy second-based entry. Native subtraction preserves NaN behavior.
    let expires_at_millis = match cached.get_property("expires_at_millis") {
        Some(value) => numeric(value),
        None => {
            cached
                .get_property("expires_at_unix")
                .map_or(f64::NAN, numeric)
                * 1000.0
        }
    };
    if expires_at_millis - epoch_millis(clock.now()) <= ID_TOKEN_EXPIRY_BUFFER.as_millis() as f64 {
        return Ok(None);
    }
    if cached.string_at("/id_token").is_none() {
        return Ok(None);
    }
    Ok(cached.subprojection("/id_token").ok())
}

/// Persist an id_token for `issuer` with an absolute expiry (`saveIdpIdToken`,
/// xaaIdpLogin.ts:109-123).
#[cfg(test)]
async fn set_cached_id_token(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
    id_token: &str,
    expires_at: SystemTime,
) -> Result<(), McpError> {
    set_cached_id_token_source(
        storage,
        clock,
        issuer,
        &id_token.into(),
        epoch_millis(expires_at),
    )
    .await
}

async fn set_cached_id_token_source(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
    id_token: &Utf16JsonProjection,
    expires_at_millis: f64,
) -> Result<(), McpError> {
    let mut builder = Utf16JsonBuilder::new();
    let token = builder
        .import_projection(id_token)
        .map_err(|error| McpError::OAuth(error.to_string().into()))?;
    let expiry = builder.push_number(expires_at_millis);
    let legacy_seconds = builder.push_number(expires_at_millis / 1000.0);
    let root = builder
        .push_object(vec![
            ("id_token".to_owned(), token),
            ("expires_at_unix".to_owned(), legacy_seconds),
            ("expires_at_millis".to_owned(), expiry),
        ])
        .map_err(|error| McpError::OAuth(error.to_string().into()))?;
    let bytes = builder
        .finish(root)
        .and_then(|value| value.to_json_string())
        .map_err(|error| McpError::OAuth(error.to_string().into()))?
        .into_bytes();
    let metadata = lingxi_core::types::SecureStorageMetadata {
        created_at: clock.now(),
        last_accessed: None,
        kind: lingxi_core::types::SecretKindDto("xaa_idp_id_token".into()),
    };
    let data = lingxi_core::types::SecureStorageData::new(bytes, metadata);
    storage
        .store(XAA_IDP_SERVICE, &issuer_key(issuer), data)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: id_token cache store: {e}").into()))?;
    Ok(())
}

/// Cache a PRE-OBTAINED id_token JWT for `issuer`, returning its absolute
/// expiry (`saveIdpIdTokenFromJwt`, xaaIdpLogin.ts:125-141).
///
/// The expiry comes from the JWT's own `exp` claim. A token whose `exp` is
/// absent, non-numeric, or unparseable gets a one-hour expiry from now rather
/// than being rejected: this path exists for conformance/e2e runs against mock
/// IdPs, which routinely mint minimal tokens. The JWT is NOT otherwise
/// validated here — it is cached, not trusted, and the resource server is what
/// verifies it.
///
/// # Errors
/// [`McpError::OAuth`] on a storage backend error.
pub async fn save_id_token_from_jwt(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
    jwt: &str,
) -> Result<SystemTime, McpError> {
    let expiry = save_id_token_from_jwt_expiry(storage, clock, issuer, jwt).await?;
    let oauth::OAuthExpiry::Number(milliseconds) = expiry else {
        unreachable!("JWT save always computes expiry")
    };
    time_from_millis(milliseconds)
        .ok_or_else(|| McpError::OAuth("XAA IdP: token expiry is outside SystemTime range".into()))
}

/// Native numeric millisecond expiry for callers that preserve JS Number
/// values, including infinities (serialized to null by the storage writer).
pub async fn save_id_token_from_jwt_expiry(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
    jwt: &str,
) -> Result<oauth::OAuthExpiry, McpError> {
    let expires_at_millis = jwt_exp_number(jwt)
        .filter(|value| *value != 0.0 && !value.is_nan())
        .map(|value| value * 1000.0)
        .unwrap_or_else(|| epoch_millis(clock.now()) + ID_TOKEN_NO_EXP_FALLBACK.as_millis() as f64);
    set_cached_id_token_source(storage, clock, issuer, &jwt.into(), expires_at_millis).await?;
    Ok(oauth::OAuthExpiry::Number(expires_at_millis))
}

/// Remove a cached id_token for `issuer` (`clearIdpIdToken`,
/// xaaIdpLogin.ts:143-150). Best-effort; a missing entry is not an error.
///
/// # Errors
/// [`McpError::OAuth`] on a storage backend error.
pub async fn clear_cached_id_token(
    storage: &Arc<dyn SecureStorage>,
    issuer: &str,
) -> Result<(), McpError> {
    let key = issuer_key(issuer);
    let _ = storage.delete(XAA_IDP_SERVICE, &key).await;
    let legacy = legacy_issuer_key(issuer);
    if legacy != key {
        let _ = storage.delete(XAA_IDP_SERVICE, &legacy).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// IdP client secret (xaaIdpLogin.ts:177-181, `getIdpClientSecret`).
// ---------------------------------------------------------------------------

/// Read the IdP client secret for `issuer`, if any (`getIdpClientSecret`).
/// `None` → public IdP client (PKCE only). Read from the `xaa-idp-config`
/// service keyed by normalized issuer.
///
/// # Errors
/// [`McpError::OAuth`] on a storage backend error.
pub async fn get_idp_client_secret(
    storage: &Arc<dyn SecureStorage>,
    issuer: &str,
) -> Result<Option<String>, McpError> {
    let key = issuer_key(issuer);
    let value = read_client_secret(storage, XAA_IDP_CONFIG_SERVICE, &key).await?;
    if value.is_some() || key == legacy_issuer_key(issuer) {
        return Ok(value);
    }
    read_client_secret(storage, XAA_IDP_CONFIG_SERVICE, &legacy_issuer_key(issuer)).await
}

/// Persist the IdP client secret for `issuer` (`saveIdpClientSecret`,
/// xaaIdpLogin.ts:152-175).
///
/// # Errors
/// [`McpError::OAuth`] on a storage backend error. The caller surfaces this as
/// a WARNING, not a failure: by the time it runs, the settings write has
/// already succeeded, so the connection is configured and only the secret is
/// missing. Reporting it as a hard failure would tell the user nothing was
/// written when in fact most of it was.
pub async fn save_idp_client_secret(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
    client_secret: &str,
) -> Result<(), McpError> {
    let bytes = serde_json::to_vec(&serde_json::json!({ "clientSecret": client_secret }))
        .map_err(|e| McpError::OAuth(format!("XAA IdP: encode secret: {e}").into()))?;
    let metadata = lingxi_core::types::SecureStorageMetadata {
        created_at: clock.now(),
        last_accessed: None,
        kind: lingxi_core::types::SecretKindDto("xaa_idp_client_secret".into()),
    };
    let data = lingxi_core::types::SecureStorageData::new(bytes, metadata);
    storage
        .store(XAA_IDP_CONFIG_SERVICE, &issuer_key(issuer), data)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: secret store: {e}").into()))
}

/// Remove the IdP client secret for `issuer` (`clearIdpClientSecret`).
/// Best-effort; a missing entry is not an error.
///
/// # Errors
/// [`McpError::OAuth`] on a storage backend error.
pub async fn clear_idp_client_secret(
    storage: &Arc<dyn SecureStorage>,
    issuer: &str,
) -> Result<(), McpError> {
    let key = issuer_key(issuer);
    let _ = storage.delete(XAA_IDP_CONFIG_SERVICE, &key).await;
    let legacy = legacy_issuer_key(issuer);
    if legacy != key {
        let _ = storage.delete(XAA_IDP_CONFIG_SERVICE, &legacy).await;
    }
    Ok(())
}

/// Read the AS confidential-client secret for a server
/// (`mcpOAuthClientConfig[serverKey].clientSecret`, auth.ts:1500 / 2430-2437).
/// Read from the `mcp-oauth-client-config` service keyed by
/// [`oauth::server_key`].
async fn get_as_client_secret(
    storage: &Arc<dyn SecureStorage>,
    server_key: &str,
) -> Result<Option<String>, McpError> {
    Ok(
        read_client_secret(storage, MCP_OAUTH_CLIENT_CONFIG_SERVICE, server_key)
            .await?
            .filter(|secret| !secret.is_empty()),
    )
}

/// Shared `{ clientSecret }` reader for the two confidential-secret slots.
async fn read_client_secret(
    storage: &Arc<dyn SecureStorage>,
    service: &str,
    account: &str,
) -> Result<Option<String>, McpError> {
    let data = storage.retrieve(service, account).await.map_err(|e| {
        McpError::OAuth(format!("XAA IdP: secret retrieve ({service}): {e}").into())
    })?;
    let Some(data) = data else { return Ok(None) };
    let parsed = Utf16JsonProjection::parse(&String::from_utf8_lossy(data.expose_secret_bytes()))
        .map_err(|error| {
        McpError::OAuth(format!("XAA IdP: decode secret ({service}): {error}").into())
    })?;
    Ok(parsed.string_at("/clientSecret").map(str::to_owned))
}

// ---------------------------------------------------------------------------
// acquire_idp_id_token (xaaIdpLogin.ts:401-487).
// ---------------------------------------------------------------------------

/// Acquire an id_token from the IdP: return the cached token if still valid,
/// otherwise run the OIDC authorization_code + PKCE browser flow once and cache
/// the result (`acquireIdpIdToken`, xaaIdpLogin.ts:401-487).
///
/// Reuses [`crate::oauth`]: [`oauth::AuthServerMetadata`] +
/// [`oauth::build_authorize_url`] for the PKCE authorize URL,
/// [`oauth::CallbackListener`] for the loopback redirect — the only deltas vs
/// the consent flow are `scope=openid` and extracting `id_token` from the token
/// response. `on_authorization_url` surfaces the URL to the host (same callback
/// [`OAuthDeps`](crate::registry::OAuthDeps) uses).
///
/// # Errors
/// [`McpError::OAuth`] on discovery, callback, token-exchange, or a token
/// response without an `id_token` (i.e. the IdP ignored `scope=openid`).
pub async fn acquire_idp_id_token(
    http: &Arc<dyn HttpTransport>,
    clock: &Arc<dyn Clock>,
    storage: &Arc<dyn SecureStorage>,
    on_authorization_url: &OnAuthorizationUrl,
    settings: &XaaIdpSettings,
    idp_client_secret: Option<&str>,
) -> Result<String, McpError> {
    // Cache hit → done (no browser pop).
    if let Some(cached) = get_cached_id_token(storage, clock, &settings.issuer)
        .await?
        .filter(|token| !token.is_empty())
    {
        return Ok(cached);
    }

    // 1. OIDC discovery → authorization + token endpoints.
    let oidc_document = discover_oidc_document(http, &settings.issuer).await?;
    let oidc = &oidc_document.metadata;
    // Adapt to the oauth.rs metadata shape (authorize-URL build + exchange).
    let meta = oauth::AuthServerMetadata {
        authorization_endpoint: oidc.authorization_endpoint.clone(),
        token_endpoint: oidc.token_endpoint.clone(),
        registration_endpoint: None,
        scopes_supported: None,
        scope: None,
        default_scope: None,
        revocation_endpoint: None,
        revocation_endpoint_auth_methods_supported: None,
        token_endpoint_auth_methods_supported: oidc_document
            .token_endpoint_auth_methods_supported
            .clone(),
    };

    // 2. Bind the loopback listener FIRST so the redirect_uri is known before
    //    the authorize URL is built. A configured callback_port pins it.
    let listener = oauth::CallbackListener::bind(settings.callback_port.unwrap_or(0))
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: callback bind: {e}").into()))?;
    let port = listener.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    // 3. Authorize URL with scope=openid (the delta vs the consent flow), PKCE
    //    inside. Surface it to the host (browser-open / log).
    let (auth_url, verifier, state) =
        oauth::build_authorize_url(&meta, &settings.client_id, &redirect_uri, "openid");
    on_authorization_url(&auth_url);

    // 4. Wait for the redirect, validate state, capture the code.
    let params = tokio::time::timeout(
        IDP_LOGIN_TIMEOUT,
        listener.accept_xaa_with_issuer_units(
            &state,
            &redirect_uri,
            Some(oidc_document.issuer_units.as_slice()),
            oidc_document.authorization_response_iss_parameter_supported,
        ),
    )
    .await
    .map_err(|_| McpError::OAuth("XAA IdP: login timed out".into()))?
    .map_err(|error| McpError::OAuth(error.to_string().into()))?;

    // 5. Exchange the code for tokens, extracting the id_token. We POST the same
    //    authorization_code grant oauth::exchange_code does, but parse our own
    //    OIDC shape (oauth::Tokens drops id_token). `client_secret` is sent via
    //    client_secret_post when the IdP client is confidential.
    let (id_token, expires_in, id_token_source) = exchange_code_for_id_token(
        http,
        clock,
        &meta,
        &settings.client_id,
        idp_client_secret.filter(|secret| !secret.is_empty()),
        &params.code,
        &verifier,
        &redirect_uri,
    )
    .await?;

    // 6. Cache it — prefer the id_token's own `exp` claim, else the AS-provided
    //    `expires_in`, else +1h (xaaIdpLogin.ts:475-478:
    //    `expFromJwt ? expFromJwt : now + (expires_in ?? 3600)`).
    let expires_at_millis = jwt_exp_number(&id_token)
        .filter(|value| *value != 0.0 && !value.is_nan())
        .map(|value| value * 1000.0)
        .unwrap_or_else(|| epoch_millis(clock.now()) + expires_in.unwrap_or(3600.0) * 1000.0);
    // Native returns the acquired token when a cache write fails.
    let _ = set_cached_id_token_source(
        storage,
        clock,
        &settings.issuer,
        &id_token_source,
        expires_at_millis,
    )
    .await;

    Ok(id_token)
}

/// POST the `authorization_code` grant and return the `id_token` (the openid
/// delta over [`oauth::exchange_code`]). `client_secret` is sent via
/// `client_secret_post` when present (xaaIdpLogin.ts:418-432).
async fn exchange_code_for_id_token(
    http: &Arc<dyn HttpTransport>,
    clock: &Arc<dyn Clock>,
    metadata: &oauth::AuthServerMetadata,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(String, Option<f64>, Utf16JsonProjection), McpError> {
    let (_, body) = oauth::exchange_code_with_client_raw(
        http,
        clock,
        metadata,
        client_id,
        client_secret.filter(|secret| !secret.is_empty()),
        None,
        code,
        verifier,
        redirect_uri,
        None,
    )
    .await
    .map_err(|error| McpError::OAuth(error.to_string().into()))?;
    let parsed = crate::oauth::schemas::validate_tokens(&body)
        .map_err(|error| McpError::OAuth(error.into()))?;
    let projection = parsed.projection();
    let id_token = projection
        .string_at("/id_token")
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            McpError::OAuth("XAA IdP: token response missing id_token (check scope=openid)".into())
        })?
        .to_owned();
    Ok((
        id_token,
        projection.number_at("/expires_in"),
        projection
            .subprojection("/id_token")
            .map_err(|error| McpError::OAuth(error.to_string().into()))?,
    ))
}

// ---------------------------------------------------------------------------
// XaaIdpConfigProvider (performMCPXaaAuth assembly, auth.ts:676-744).
// ---------------------------------------------------------------------------

/// Host-side [`XaaConfigProvider`]: assembles the [`XaaInputs`] bundle for an
/// XAA-flagged server from the user `xaaIdp` settings + secure storage.
///
/// Construct with [`Self::new`]; the engine wires `Some(Arc::new(..))` into
/// [`OAuthDeps::xaa_config`](crate::registry::OAuthDeps) only when an `xaaIdp`
/// settings tier is present (XAA stays opt-in). When the per-server AS
/// `client_id` is unknown — i.e. the caller passes the server URL but the
/// provider can't see the per-server OAuth config — the provider needs the
/// AS `client_id` from the server's `oauth.clientId`. The registry supplies
/// that via [`Self::with_server_client_id`] resolution; see [`xaa_inputs`].
#[derive(Clone)]
pub struct XaaIdpConfigProvider {
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
    storage: Arc<dyn SecureStorage>,
    on_authorization_url: OnAuthorizationUrl,
    settings: XaaIdpSettings,
    /// Resolver from `server_name` → the AS-registered `client_id` + transport
    /// spec (so the AS `client_secret` can be looked up by `server_key`). The
    /// engine wires this from its known MCP configs. When a server isn't in the
    /// map, `xaa_inputs` returns `Ok(None)` (not XAA-provisioned).
    server_lookup: Arc<dyn ServerOAuthLookup>,
    server_credentials: Option<XaaServerCredentials>,
}

#[derive(Clone)]
struct XaaServerCredentials {
    name: String,
    key: String,
    client_id: Option<String>,
    storage: Arc<dyn SecureStorage>,
}

/// Seam the host implements so the provider can map a `server_name` to its
/// AS-registered `client_id` and transport spec (for the `server_key` secret
/// lookup). Mirrors how `performMCPXaaAuth` reads `serverConfig.oauth.clientId`
/// + `getServerKey(serverName, serverConfig)` (auth.ts:683 / 695).
#[async_trait::async_trait]
pub trait ServerOAuthLookup: Send + Sync {
    /// Return `(as_client_id, server_key)` for `server_name`, or `None` if the
    /// server is unknown / has no AS `client_id` (→ not XAA-provisioned).
    async fn lookup(&self, server_name: &str) -> Option<(String, String)>;
}

impl XaaIdpConfigProvider {
    /// Build a provider. `settings` is the parsed `xaaIdp` block; the four Arcs
    /// are the SAME http/clock/storage/on_authorization_url the engine builds
    /// for [`OAuthDeps`](crate::registry::OAuthDeps).
    #[must_use]
    pub fn new(
        http: Arc<dyn HttpTransport>,
        clock: Arc<dyn Clock>,
        storage: Arc<dyn SecureStorage>,
        on_authorization_url: OnAuthorizationUrl,
        settings: XaaIdpSettings,
        server_lookup: Arc<dyn ServerOAuthLookup>,
    ) -> Self {
        Self {
            http,
            clock,
            storage,
            on_authorization_url,
            settings,
            server_lookup,
            server_credentials: None,
        }
    }

    async fn lookup_server_credentials(
        &self,
        server_name: &str,
    ) -> Option<(String, String, Arc<dyn SecureStorage>)> {
        if let Some(bound) = self
            .server_credentials
            .as_ref()
            .filter(|bound| bound.name == server_name)
        {
            return Some((
                bound.client_id.clone()?,
                bound.key.clone(),
                bound.storage.clone(),
            ));
        }
        let (client_id, key) = self.server_lookup.lookup(server_name).await?;
        Some((client_id, key, self.storage.clone()))
    }
}

#[async_trait::async_trait]
impl XaaConfigProvider for XaaIdpConfigProvider {
    fn for_server_credentials(
        &self,
        server_name: &str,
        server_key: &str,
        client_id: Option<&str>,
        storage: Arc<dyn SecureStorage>,
    ) -> Option<Arc<dyn XaaConfigProvider>> {
        let mut provider = self.clone();
        provider.server_credentials = Some(XaaServerCredentials {
            name: server_name.to_owned(),
            key: server_key.to_owned(),
            client_id: client_id.map(str::to_owned),
            storage,
        });
        Some(Arc::new(provider))
    }

    async fn xaa_inputs(
        &self,
        server_name: &str,
        _server_url: &str,
    ) -> Result<Option<XaaInputs>, McpError> {
        // Resolve the AS client_id + server_key for this server. Unknown server
        // (or no AS client_id) → not XAA-provisioned (auth.ts:683-688 raises;
        // our seam returns Ok(None) so the registry emits its hard-fail copy).
        let Some((as_client_id, server_key, as_storage)) =
            self.lookup_server_credentials(server_name).await
        else {
            return Ok(None);
        };

        // AS confidential client_secret (mcpOAuthClientConfig[serverKey]). Absent
        // → can't run the confidential jwt-bearer leg; treat as not provisioned
        // (auth.ts:691-711 raises; we surface the actionable error to the user).
        let Some(as_client_secret) = get_as_client_secret(&as_storage, &server_key).await? else {
            return Err(McpError::OAuth(
                format!(
                    "XAA: AS client secret not found for '{server_name}'. \
                 Re-add the server with its --client-secret."
                )
                .into(),
            ));
        };

        // IdP client secret lives in its own slot, keyed by IdP issuer (a
        // different trust domain). Optional — absent → PKCE-only public client.
        let idp_client_secret = get_idp_client_secret(&self.storage, &self.settings.issuer).await?;

        // Acquire the id_token (cache hit or one OIDC browser pop).
        let idp_id_token = acquire_idp_id_token(
            &self.http,
            &self.clock,
            &self.storage,
            &self.on_authorization_url,
            &self.settings,
            idp_client_secret.as_deref(),
        )
        .await?;

        // Discover the IdP token endpoint for the RFC 8693 exchange.
        let oidc = discover_oidc(&self.http, &self.settings.issuer).await?;

        Ok(Some(XaaInputs {
            client_id: as_client_id,
            client_secret: as_client_secret,
            idp_client_id: self.settings.client_id.clone(),
            idp_client_secret,
            idp_id_token,
            idp_token_endpoint: oidc.token_endpoint,
        }))
    }

    async fn as_client_secret_source(
        &self,
        server_name: &str,
        _server_url: &str,
    ) -> Result<Option<Utf16JsonProjection>, McpError> {
        let Some((_, key, as_storage)) = self.lookup_server_credentials(server_name).await else {
            return Ok(None);
        };
        let Some(data) = as_storage
            .retrieve(MCP_OAUTH_CLIENT_CONFIG_SERVICE, &key)
            .await
            .map_err(|error| {
                McpError::OAuth(format!("XAA IdP: secret retrieve: {error}").into())
            })?
        else {
            return Ok(None);
        };
        let source =
            Utf16JsonProjection::parse(&String::from_utf8_lossy(data.expose_secret_bytes()))
                .map_err(|error| {
                    McpError::OAuth(format!("XAA IdP: decode secret: {error}").into())
                })?;
        if source.string_at("/clientSecret").is_none() {
            return Ok(None);
        }
        Ok(source.subprojection("/clientSecret").ok())
    }

    async fn peek_id_token_cache_hit(
        &self,
        server_name: &str,
        _server_url: &str,
    ) -> Result<bool, McpError> {
        // Match `performMCPXaaAuth`: the AS client config and both secret
        // lookups happen before `idTokenCacheHit` is sampled and before the
        // failure-telemetry try/catch begins. Repeating these read-only checks
        // in `xaa_inputs` keeps the trait seam stateless while preserving that
        // observable ordering.
        let Some((_as_client_id, server_key, as_storage)) =
            self.lookup_server_credentials(server_name).await
        else {
            return Err(McpError::OAuth(
                format!(
                    "XAA: server '{server_name}' is not XAA-provisioned (no AS client config)."
                )
                .into(),
            ));
        };
        if get_as_client_secret(&as_storage, &server_key)
            .await?
            .is_none()
        {
            return Err(McpError::OAuth(format!(
                "XAA: AS client secret not found for '{server_name}'. Re-add the server with its --client-secret."
            ).into()));
        }
        let _ = get_idp_client_secret(&self.storage, &self.settings.issuer).await?;
        Ok(
            get_cached_id_token(&self.storage, &self.clock, &self.settings.issuer)
                .await?
                .is_some_and(|token| !token.is_empty()),
        )
    }

    /// Drop this IdP's cached id_token (`clearIdpIdToken(idp.issuer)`,
    /// xaaIdpLogin.ts:143-150) so the next [`Self::xaa_inputs`] re-acquires.
    async fn clear_id_token(&self) -> Result<(), McpError> {
        clear_cached_id_token(&self.storage, &self.settings.issuer).await
    }
}

/// Compute the `mcpOAuthClientConfig` storage key for a server (the account a
/// [`ServerOAuthLookup`] returns). Re-exported convenience over
/// [`oauth::server_key`] so the engine builds keys the same way.
#[must_use]
pub fn server_oauth_config_key(name: &str, spec: &McpTransportSpec) -> String {
    oauth::server_key(name, spec)
}

/// A static, map-backed [`ServerOAuthLookup`] the engine builds from its known
/// MCP server configs. Maps `server_name` → `(as_client_id, server_key)` for
/// every server that has an `oauth.client_id` (only those can run XAA's
/// confidential jwt-bearer leg). Build with [`Self::from_configs`].
pub struct MapServerOAuthLookup {
    map: std::collections::HashMap<String, (String, String)>,
}

impl MapServerOAuthLookup {
    /// Build the lookup from `(name, client_id, server_key)` triples. Only
    /// servers with an AS `client_id` should be passed; others won't be XAA
    /// candidates (`lookup` → `None`).
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = (String, String, String)>) -> Self {
        Self {
            map: entries
                .into_iter()
                .map(|(name, cid, key)| (name, (cid, key)))
                .collect(),
        }
    }

    /// Build from `(name, &spec)` pairs: derives the `server_key` via
    /// [`oauth::server_key`] and pulls the AS `client_id` out of the spec's
    /// `oauth.client_id`. Servers without an AS `client_id` are skipped.
    #[must_use]
    pub fn from_specs<'a>(
        servers: impl IntoIterator<Item = (&'a str, &'a McpTransportSpec)>,
    ) -> Self {
        let entries = servers.into_iter().filter_map(|(name, spec)| {
            let client_id = match spec {
                McpTransportSpec::Sse { oauth, .. } | McpTransportSpec::Http { oauth, .. } => {
                    oauth.as_ref().and_then(|o| o.client_id.clone())
                }
                _ => None,
            }?;
            let key = oauth::server_key(name, spec);
            Some((name.to_string(), client_id, key))
        });
        Self::new(entries)
    }
}

#[async_trait::async_trait]
impl ServerOAuthLookup for MapServerOAuthLookup {
    async fn lookup(&self, server_name: &str) -> Option<(String, String)> {
        self.map.get(server_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- writers added for the `mcp xaa` CLI surface ------------------------

    /// Base64url (no padding) encode, for building test JWTs.
    fn b64url(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn jwt_with_claims(claims: &str) -> String {
        format!(
            "{}.{}.sig",
            b64url(br#"{"alg":"RS256"}"#),
            b64url(claims.as_bytes())
        )
    }

    fn mem() -> (Arc<dyn SecureStorage>, Arc<dyn Clock>) {
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        // 2026-01-01T00:00:00Z, so "now" is well before the 2030 test expiry.
        let clock: Arc<dyn Clock> =
            Arc::new(TestClock(UNIX_EPOCH + Duration::from_secs(1_767_225_600)));
        (storage, clock)
    }

    #[tokio::test]
    async fn saved_jwt_round_trips_and_uses_its_own_exp() {
        let (storage, clock) = mem();
        // exp = 2030-01-01T00:00:00Z
        let jwt = jwt_with_claims(r#"{"exp":1893456000,"sub":"u1"}"#);
        let expires_at = save_id_token_from_jwt(&storage, &clock, "https://idp.example", &jwt)
            .await
            .expect("save");
        assert_eq!(
            expires_at,
            UNIX_EPOCH + Duration::from_secs(1_893_456_000),
            "expiry must come from the JWT's own exp claim"
        );
        let got = get_cached_id_token(&storage, &clock, "https://idp.example")
            .await
            .expect("read");
        assert_eq!(got.as_deref(), Some(jwt.as_str()));
    }

    #[tokio::test]
    async fn a_jwt_without_exp_falls_back_to_one_hour() {
        let (storage, clock) = mem();
        let jwt = jwt_with_claims(r#"{"sub":"u1"}"#);
        let expires_at = save_id_token_from_jwt(&storage, &clock, "https://idp.example", &jwt)
            .await
            .expect("save");
        assert_eq!(expires_at, clock.now() + Duration::from_secs(3600));
        // Still readable: an hour is comfortably outside the 60s buffer.
        assert!(get_cached_id_token(&storage, &clock, "https://idp.example")
            .await
            .expect("read")
            .is_some());
    }

    #[tokio::test]
    async fn a_malformed_jwt_is_cached_with_the_fallback_not_rejected() {
        // This path exists for conformance runs against mock IdPs, which mint
        // minimal tokens. The token is cached, not trusted; the resource server
        // is what verifies it.
        let (storage, clock) = mem();
        let expires_at =
            save_id_token_from_jwt(&storage, &clock, "https://idp.example", "not-a-jwt")
                .await
                .expect("save");
        assert_eq!(expires_at, clock.now() + Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn an_already_expired_jwt_saves_but_reads_back_as_a_miss() {
        // The write must not silently succeed-and-hide: `show` has to report
        // "not logged in" rather than showing a token nothing will accept.
        let (storage, clock) = mem();
        let jwt = jwt_with_claims(r#"{"exp":1000000000}"#); // 2001
        save_id_token_from_jwt(&storage, &clock, "https://idp.example", &jwt)
            .await
            .expect("save");
        assert_eq!(
            get_cached_id_token(&storage, &clock, "https://idp.example")
                .await
                .expect("read"),
            None
        );
    }

    #[tokio::test]
    async fn a_token_inside_the_expiry_buffer_reads_as_a_miss() {
        let (storage, clock) = mem();
        // Expires 30s from now — inside the 60s buffer.
        let exp = (clock.now() + Duration::from_secs(30))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let jwt = jwt_with_claims(&format!(r#"{{"exp":{exp}}}"#));
        save_id_token_from_jwt(&storage, &clock, "https://idp.example", &jwt)
            .await
            .expect("save");
        assert_eq!(
            get_cached_id_token(&storage, &clock, "https://idp.example")
                .await
                .expect("read"),
            None
        );
    }

    #[tokio::test]
    async fn client_secret_round_trips_and_clears() {
        let (storage, clock) = mem();
        assert_eq!(
            get_idp_client_secret(&storage, "https://idp.example")
                .await
                .expect("read"),
            None
        );
        save_idp_client_secret(&storage, &clock, "https://idp.example", "s3cret")
            .await
            .expect("save");
        assert_eq!(
            get_idp_client_secret(&storage, "https://idp.example")
                .await
                .expect("read"),
            Some("s3cret".to_string())
        );
        clear_idp_client_secret(&storage, "https://idp.example")
            .await
            .expect("clear");
        assert_eq!(
            get_idp_client_secret(&storage, "https://idp.example")
                .await
                .expect("read"),
            None
        );
    }

    #[tokio::test]
    async fn writes_and_reads_agree_on_the_normalized_issuer_key() {
        // A cosmetic issuer difference must not create a second cache slot: a
        // user who runs `setup` with a trailing slash and `login` without one
        // would otherwise appear logged out immediately after logging in.
        let (storage, clock) = mem();
        let jwt = jwt_with_claims(r#"{"exp":1893456000}"#);
        save_id_token_from_jwt(&storage, &clock, "https://IdP.Example/", &jwt)
            .await
            .expect("save");
        assert!(
            get_cached_id_token(&storage, &clock, "https://idp.example")
                .await
                .expect("read")
                .is_some(),
            "trailing slash + host case must hit the same slot"
        );
    }

    #[tokio::test]
    async fn the_id_token_and_client_secret_live_in_separate_slots() {
        // Same account key, different service: clearing one must not clear the
        // other, or `setup --client-secret` would silently log the user out.
        let (storage, clock) = mem();
        let jwt = jwt_with_claims(r#"{"exp":1893456000}"#);
        save_id_token_from_jwt(&storage, &clock, "https://idp.example", &jwt)
            .await
            .expect("save");
        save_idp_client_secret(&storage, &clock, "https://idp.example", "s3cret")
            .await
            .expect("save");

        clear_cached_id_token(&storage, "https://idp.example")
            .await
            .expect("clear");
        assert_eq!(
            get_idp_client_secret(&storage, "https://idp.example")
                .await
                .expect("read"),
            Some("s3cret".to_string()),
            "clearing the id_token must not clear the client secret"
        );
    }

    use std::collections::HashMap;
    use tokio::sync::Mutex;

    // ----- test doubles -------------------------------------------------------

    struct TestClock(SystemTime);
    impl Clock for TestClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    /// Minimal in-memory SecureStorage keyed by (service, account).
    #[derive(Default)]
    struct MemStorage {
        map: Mutex<HashMap<(String, String), Vec<u8>>>,
    }
    impl MemStorage {
        async fn put_json(&self, service: &str, account: &str, json: &str) {
            self.map
                .lock()
                .await
                .insert((service.into(), account.into()), json.as_bytes().to_vec());
        }
    }
    #[async_trait::async_trait]
    impl SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: lingxi_core::types::SecureStorageData,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            self.map.lock().await.insert(
                (service.into(), account.into()),
                data.expose_secret_bytes().to_vec(),
            );
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<
            Option<lingxi_core::types::SecureStorageData>,
            lingxi_core::host::SecureStorageError,
        > {
            let map = self.map.lock().await;
            Ok(map.get(&(service.into(), account.into())).map(|bytes| {
                lingxi_core::types::SecureStorageData::new(
                    bytes.clone(),
                    lingxi_core::types::SecureStorageMetadata {
                        created_at: UNIX_EPOCH,
                        last_accessed: None,
                        kind: lingxi_core::types::SecretKindDto("test".into()),
                    },
                )
            }))
        }
        async fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            self.map
                .lock()
                .await
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
            let map = self.map.lock().await;
            Ok(map
                .keys()
                .filter(|(s, _)| s == service)
                .map(|(_, a)| a.clone())
                .collect())
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
            lingxi_core::host::SecureStorageBackend::PlainText
        }
    }

    /// Scripted HTTP transport: matches by (method, url-substring) → canned body.
    struct ScriptedHttp {
        routes: Vec<(HttpMethod, String, u16, String)>,
    }
    #[async_trait::async_trait]
    impl HttpTransport for ScriptedHttp {
        async fn request(
            &self,
            req: HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            for (method, needle, status, body) in &self.routes {
                if *method == req.method && req.url.contains(needle.as_str()) {
                    return Ok(lingxi_core::types::HttpResponse {
                        status: *status,
                        headers: vec![],
                        body: body.clone(),
                        body_bytes: Vec::new(),
                    });
                }
            }
            Err(lingxi_core::host::HttpError::Connection(format!(
                "no route for {}",
                req.url
            )))
        }
        async fn stream_sse(
            &self,
            _req: HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            unreachable!("XAA IdP tests never stream SSE")
        }
    }

    struct StaticLookup(Option<(String, String)>);
    #[async_trait::async_trait]
    impl ServerOAuthLookup for StaticLookup {
        async fn lookup(&self, _server_name: &str) -> Option<(String, String)> {
            self.0.clone()
        }
    }

    fn jwt_with_exp(exp: u64) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"none"}"#);
        let payload = b64.encode(format!(r#"{{"exp":{exp},"sub":"u"}}"#).as_bytes());
        format!("{header}.{payload}.")
    }

    // ----- (a) discover_oidc --------------------------------------------------

    #[tokio::test]
    async fn discover_oidc_parses_endpoints_and_appends_path() {
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![(
                HttpMethod::Get,
                "/tenant/.well-known/openid-configuration".into(),
                200,
                r#"{"authorization_endpoint":"https://idp.example.com/authorize","token_endpoint":"https://idp.example.com/token","issuer":"https://idp.example.com","jwks_uri":"https://idp.example.com/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#
                    .into(),
            )],
        });
        // Path-append (not replace): issuer carries a /tenant path.
        let meta = discover_oidc(&http, "https://idp.example.com/tenant")
            .await
            .expect("discovery ok");
        assert_eq!(
            meta.authorization_endpoint,
            "https://idp.example.com/authorize"
        );
        assert_eq!(meta.token_endpoint, "https://idp.example.com/token");
    }

    #[tokio::test]
    async fn discover_oidc_rejects_non_https_token_endpoint() {
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![(
                HttpMethod::Get,
                "openid-configuration".into(),
                200,
                r#"{"authorization_endpoint":"https://idp/authorize","token_endpoint":"http://idp/token","issuer":"https://idp","jwks_uri":"https://idp/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#
                    .into(),
            )],
        });
        let err = discover_oidc(&http, "https://idp").await.unwrap_err();
        assert!(err.to_string().contains("non-HTTPS"), "{err}");
    }

    // ----- (b) id_token cache round-trip + expiry -----------------------------

    #[tokio::test]
    async fn id_token_cache_round_trips_and_misses_on_expiry() {
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let clock: Arc<dyn Clock> = Arc::new(TestClock(now));
        let issuer = "https://idp.example.com/";

        // Fresh token (expires in 1h) round-trips.
        set_cached_id_token(
            &storage,
            &clock,
            issuer,
            "tok-1",
            now + Duration::from_secs(3600),
        )
        .await
        .unwrap();
        // Issuer-key normalization: a cosmetically-different issuer hits the slot.
        let got = get_cached_id_token(&storage, &clock, "https://IDP.example.com")
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("tok-1"));

        // A token within the 60s buffer of expiring is a MISS → forces re-acquire.
        set_cached_id_token(
            &storage,
            &clock,
            issuer,
            "tok-2",
            now + Duration::from_secs(30),
        )
        .await
        .unwrap();
        let miss = get_cached_id_token(&storage, &clock, issuer).await.unwrap();
        assert_eq!(
            miss, None,
            "token within expiry buffer must be a cache miss"
        );

        // Clear removes the slot.
        set_cached_id_token(
            &storage,
            &clock,
            issuer,
            "tok-3",
            now + Duration::from_secs(3600),
        )
        .await
        .unwrap();
        clear_cached_id_token(&storage, issuer).await.unwrap();
        assert_eq!(
            get_cached_id_token(&storage, &clock, issuer).await.unwrap(),
            None
        );
    }

    #[test]
    fn jwt_exp_decodes_exp_claim() {
        assert_eq!(jwt_exp(&jwt_with_exp(1_700_000_000)), Some(1_700_000_000));
        assert_eq!(jwt_exp("not-a-jwt"), None);
        assert_eq!(jwt_exp("a.b"), None); // only 2 segments
    }

    // ----- (c) acquire_idp_id_token drives PKCE flow + extracts/caches ---------

    #[tokio::test]
    async fn acquire_idp_id_token_runs_pkce_flow_and_caches() {
        let exp = 2_000_000_000;
        let id_token = jwt_with_exp(exp);
        let token_body = format!(
            r#"{{"access_token":"at","token_type":"Bearer","id_token":"{id_token}","expires_in":3600}}"#
        );
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![
                (
                    HttpMethod::Get,
                    "openid-configuration".into(),
                    200,
                    // authorize endpoint is unused (we drive the callback directly),
                    // but token endpoint must be HTTPS.
                    r#"{"authorization_endpoint":"https://idp/authorize","token_endpoint":"https://idp/token","issuer":"https://idp","jwks_uri":"https://idp/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#
                        .into(),
                ),
                (
                    HttpMethod::Post,
                    "https://idp/token".into(),
                    200,
                    token_body,
                ),
            ],
        });
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH + Duration::from_secs(1_000)));
        let settings = XaaIdpSettings {
            issuer: "https://idp".into(),
            client_id: "idp-client".into(),
            callback_port: None,
        };

        // The acquire flow binds an ephemeral loopback listener and waits for the
        // redirect. Drive it: capture the auth URL (to read state + port) and POST
        // the callback. on_authorization_url fires after the listener binds.
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap2 = captured.clone();
        let on_url: OnAuthorizationUrl = Arc::new(move |url: &str| {
            // Spawn a task to hit the loopback callback with the matching state.
            let url = url.to_string();
            let cap = cap2.clone();
            tokio::spawn(async move {
                *cap.lock().await = Some(url.clone());
                // Parse port from redirect_uri and state from the query.
                let port = url
                    .split("redirect_uri=")
                    .nth(1)
                    .and_then(|s| s.split("%3A").nth(2)) // http%3A%2F%2Flocalhost%3A{port}%2F...
                    .and_then(|s| s.split("%2F").next())
                    .and_then(|s| s.parse::<u16>().ok())
                    .expect("port");
                let state = url
                    .split("state=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .expect("state")
                    .to_string();
                // Brief retry loop: the listener may not be accepting yet.
                for _ in 0..50 {
                    if let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                        use tokio::io::AsyncWriteExt;
                        let req = format!(
                            "GET /callback?code=the-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                        );
                        let _ = s.write_all(req.as_bytes()).await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                panic!("could not connect to loopback callback");
            });
        });

        let got = acquire_idp_id_token(&http, &clock, &storage, &on_url, &settings, None)
            .await
            .expect("acquire ok");
        assert_eq!(got, id_token, "extracted id_token from token response");
        assert!(captured.lock().await.is_some(), "auth URL was surfaced");

        // Cached now → a second call is a pure cache hit (no token POST needed).
        // Swap in an HTTP that has NO token route to prove the cache short-circuits.
        let http_no_token: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp { routes: vec![] });
        let again =
            acquire_idp_id_token(&http_no_token, &clock, &storage, &on_url, &settings, None)
                .await
                .expect("cache hit");
        assert_eq!(again, id_token);
    }

    // ----- (d) XaaIdpConfigProvider assembles inputs / Ok(None) ---------------

    #[tokio::test]
    async fn provider_assembles_full_inputs() {
        let id_token = jwt_with_exp(2_000_000_000);
        let token_body =
            format!(r#"{{"access_token":"at","token_type":"Bearer","id_token":"{id_token}"}}"#);
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![
                (
                    HttpMethod::Get,
                    "openid-configuration".into(),
                    200,
                    r#"{"authorization_endpoint":"https://idp/authorize","token_endpoint":"https://idp/token","issuer":"https://idp","jwks_uri":"https://idp/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#
                        .into(),
                ),
                (
                    HttpMethod::Post,
                    "https://idp/token".into(),
                    200,
                    token_body,
                ),
            ],
        });
        let storage = Arc::new(MemStorage::default());
        // Seed the AS client secret at mcp-oauth-client-config[server_key].
        storage
            .put_json(
                MCP_OAUTH_CLIENT_CONFIG_SERVICE,
                "srv-key",
                r#"{"clientSecret":"as-secret"}"#,
            )
            .await;
        // Seed the IdP client secret.
        storage
            .put_json(
                XAA_IDP_CONFIG_SERVICE,
                &issuer_key("https://idp"),
                r#"{"clientSecret":"idp-secret"}"#,
            )
            .await;
        let storage: Arc<dyn SecureStorage> = storage;
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH + Duration::from_secs(1_000)));
        let settings = XaaIdpSettings {
            issuer: "https://idp".into(),
            client_id: "idp-client".into(),
            callback_port: None,
        };

        // Drive the loopback callback (same harness as test c).
        let on_url: OnAuthorizationUrl = Arc::new(move |url: &str| {
            let url = url.to_string();
            tokio::spawn(async move {
                let port = url
                    .split("redirect_uri=")
                    .nth(1)
                    .and_then(|s| s.split("%3A").nth(2))
                    .and_then(|s| s.split("%2F").next())
                    .and_then(|s| s.parse::<u16>().ok())
                    .unwrap();
                let state = url
                    .split("state=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .unwrap()
                    .to_string();
                for _ in 0..50 {
                    if let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                        use tokio::io::AsyncWriteExt;
                        let req = format!(
                            "GET /callback?code=c&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                        );
                        let _ = s.write_all(req.as_bytes()).await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        });

        let lookup: Arc<dyn ServerOAuthLookup> =
            Arc::new(StaticLookup(Some(("as-client".into(), "srv-key".into()))));
        let provider = XaaIdpConfigProvider::new(http, clock, storage, on_url, settings, lookup);

        let inputs = provider
            .xaa_inputs("acme", "https://mcp.acme.com")
            .await
            .expect("ok")
            .expect("provisioned");
        assert_eq!(inputs.client_id, "as-client");
        assert_eq!(inputs.client_secret, "as-secret");
        assert_eq!(inputs.idp_client_id, "idp-client");
        assert_eq!(inputs.idp_client_secret.as_deref(), Some("idp-secret"));
        assert_eq!(inputs.idp_id_token, id_token);
        assert_eq!(inputs.idp_token_endpoint, "https://idp/token");
    }

    #[tokio::test]
    async fn provider_returns_none_when_server_unknown() {
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp { routes: vec![] });
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH));
        let on_url: OnAuthorizationUrl = Arc::new(|_: &str| {});
        let settings = XaaIdpSettings {
            issuer: "https://idp".into(),
            client_id: "idp-client".into(),
            callback_port: None,
        };
        // Lookup returns None → not XAA-provisioned → Ok(None).
        let lookup: Arc<dyn ServerOAuthLookup> = Arc::new(StaticLookup(None));
        let provider = XaaIdpConfigProvider::new(http, clock, storage, on_url, settings, lookup);
        let out = provider.xaa_inputs("unknown", "https://x").await.unwrap();
        assert!(out.is_none(), "unknown server → Ok(None)");
    }

    // ----- (e) 4xx token-exchange clears the cached id_token; 5xx keeps it ----
    //
    // Mirrors resolve_xaa_token's new arm (registry.rs) composed against the
    // REAL provider: drive the IdP token-exchange leg (the only leg that carries
    // `should_clear_id_token`), then — exactly as the registry does — if
    // `err.should_clear_id_token()` is true, call `provider.clear_id_token()`.
    // Parity: auth.ts:1840-1847 `if (e.shouldClearIdToken) clearIdpIdToken(...)`.

    /// Build a provider over `http`/`storage` for `issuer` (no lookup/auth-url
    /// needed: these tests only exercise `clear_id_token`).
    fn provider_for(
        http: Arc<dyn HttpTransport>,
        storage: Arc<dyn SecureStorage>,
        clock: Arc<dyn Clock>,
        issuer: &str,
    ) -> XaaIdpConfigProvider {
        let on_url: OnAuthorizationUrl = Arc::new(|_: &str| {});
        let settings = XaaIdpSettings {
            issuer: issuer.into(),
            client_id: "idp-client".into(),
            callback_port: None,
        };
        let lookup: Arc<dyn ServerOAuthLookup> = Arc::new(StaticLookup(None));
        XaaIdpConfigProvider::new(http, clock, storage, on_url, settings, lookup)
    }

    /// Replay the registry's exchange-error arm: run the IdP token-exchange leg;
    /// on a `should_clear_id_token` error, clear the provider's cached id_token.
    async fn exchange_then_maybe_clear(
        provider: &XaaIdpConfigProvider,
        http: &Arc<dyn HttpTransport>,
        token_endpoint: &str,
    ) {
        let req = crate::xaa::JwtAuthGrantRequest {
            token_endpoint,
            audience: "https://as.example.com",
            resource: "https://mcp.acme.com",
            id_token: "the-id-token",
            client_id: "idp-client",
            client_secret: None,
            scope: None,
        };
        let res = crate::xaa::request_jwt_authorization_grant(http, &req).await;
        if let Err(e) = res {
            if e.should_clear_id_token() {
                provider.clear_id_token().await.expect("clear ok");
            }
        }
    }

    #[tokio::test]
    async fn token_exchange_4xx_clears_cached_id_token() {
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let clock: Arc<dyn Clock> = Arc::new(TestClock(now));
        let issuer = "https://idp.example.com";

        // Seed a fresh (1h) cached id_token for this issuer.
        set_cached_id_token(
            &storage,
            &clock,
            issuer,
            "tok-cached",
            now + Duration::from_secs(3600),
        )
        .await
        .unwrap();
        assert!(
            get_cached_id_token(&storage, &clock, issuer)
                .await
                .unwrap()
                .is_some(),
            "precondition: id_token is cached"
        );

        // Mock AS: the IdP token-exchange POST 4xx-rejects the id_token.
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![(
                HttpMethod::Post,
                "https://idp.example.com/token".into(),
                400,
                r#"{"error":"invalid_grant"}"#.into(),
            )],
        });
        let provider = provider_for(http.clone(), storage.clone(), clock.clone(), issuer);

        exchange_then_maybe_clear(&provider, &http, "https://idp.example.com/token").await;

        // 4xx ⇒ cached id_token cleared, so the next resolve re-acquires.
        assert_eq!(
            get_cached_id_token(&storage, &clock, issuer).await.unwrap(),
            None,
            "4xx token-exchange must clear the cached id_token"
        );
    }

    #[tokio::test]
    async fn token_exchange_5xx_keeps_cached_id_token() {
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let clock: Arc<dyn Clock> = Arc::new(TestClock(now));
        let issuer = "https://idp.example.com";

        set_cached_id_token(
            &storage,
            &clock,
            issuer,
            "tok-cached",
            now + Duration::from_secs(3600),
        )
        .await
        .unwrap();

        // Mock AS: the IdP token-exchange POST 5xx (IdP outage).
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![(
                HttpMethod::Post,
                "https://idp.example.com/token".into(),
                503,
                r#"{"error":"temporarily_unavailable"}"#.into(),
            )],
        });
        let provider = provider_for(http.clone(), storage.clone(), clock.clone(), issuer);

        exchange_then_maybe_clear(&provider, &http, "https://idp.example.com/token").await;

        // 5xx (IdP outage) ⇒ the cached id_token is KEPT.
        assert_eq!(
            get_cached_id_token(&storage, &clock, issuer)
                .await
                .unwrap()
                .as_deref(),
            Some("tok-cached"),
            "5xx token-exchange must keep the cached id_token"
        );
    }

    // ----- settings parse -----------------------------------------------------

    #[test]
    fn settings_parse_from_tiers_last_wins() {
        // No xaaIdp anywhere → None (opt-in).
        assert!(XaaIdpSettings::from_settings_tiers(&[r#"{"permissions":{}}"#]).is_none());
        // Present → parsed; later tier overrides earlier.
        let merged = XaaIdpSettings::from_settings_tiers(&[
            r#"{"xaaIdp":{"issuer":"https://a","clientId":"c1"}}"#,
            r#"{"xaaIdp":{"issuer":"https://b","clientId":"c2","callbackPort":7777}}"#,
        ])
        .expect("present");
        assert_eq!(merged.issuer, "https://b");
        assert_eq!(merged.client_id, "c2");
        assert_eq!(merged.callback_port, Some(7777));
    }

    #[test]
    fn native_322_idp_jwt_parser_matches_buffer_and_numeric_claims() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_xaa_2_1_322.json")).unwrap();
        for case in fixture["jwtCases"].as_array().unwrap() {
            assert_eq!(
                jwt_exp_number(case["token"].as_str().unwrap()),
                case["expected"].as_f64(),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn native_322_idp_numeric_cache_expiry_preserves_fraction_negative_zero_and_infinity() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_xaa_2_1_322.json")).unwrap();
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH + Duration::from_secs(1000)));
        for case in fixture["cacheExpiryCases"].as_array().unwrap() {
            let token = case["token"].as_str().unwrap();
            let expiry =
                save_id_token_from_jwt_expiry(&storage, &clock, "https://idp.example", token)
                    .await
                    .unwrap();
            assert_eq!(
                serde_json::to_string(&expiry).unwrap(),
                case["expected_json"].as_str().unwrap()
            );
            if case.get("nonfinite").is_some() {
                assert!(matches!(expiry,oauth::OAuthExpiry::Number(value) if value.is_infinite()));
                assert!(get_cached_id_token(&storage, &clock, "https://idp.example")
                    .await
                    .unwrap()
                    .is_none());
                assert!(
                    save_id_token_from_jwt(&storage, &clock, "https://idp.example", token)
                        .await
                        .is_err(),
                    "SystemTime compatibility API must not silently replace Infinity with1970"
                );
            }
        }
    }

    #[tokio::test]
    async fn native_322_idp_issuer_migration_reads_and_clears_legacy_cache_slots() {
        let memory = Arc::new(MemStorage::default());
        let storage: Arc<dyn SecureStorage> = memory.clone();
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH + Duration::from_secs(1000)));
        let issuer = "https://IDP.EXAMPLE:443/";
        assert_eq!(issuer_key(issuer), "https://idp.example/");
        let legacy = legacy_issuer_key(issuer);
        memory
            .put_json(
                XAA_IDP_SERVICE,
                &legacy,
                r#"{"id_token":"fake-legacy-token","expires_at_unix":2000000000}"#,
            )
            .await;
        memory
            .put_json(
                XAA_IDP_CONFIG_SERVICE,
                &legacy,
                r#"{"clientSecret":"fake-legacy-secret"}"#,
            )
            .await;
        assert_eq!(
            get_cached_id_token(&storage, &clock, issuer)
                .await
                .unwrap()
                .as_deref(),
            Some("fake-legacy-token")
        );
        assert_eq!(
            get_idp_client_secret(&storage, issuer)
                .await
                .unwrap()
                .as_deref(),
            Some("fake-legacy-secret")
        );
        clear_cached_id_token(&storage, issuer).await.unwrap();
        clear_idp_client_secret(&storage, issuer).await.unwrap();
        assert!(get_cached_id_token(&storage, &clock, issuer)
            .await
            .unwrap()
            .is_none());
        assert!(get_idp_client_secret(&storage, issuer)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn native_322_idp_cached_token_preserves_unpaired_source_units() {
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH));
        let token = Utf16JsonProjection::parse(r#""fake\ud800token""#).unwrap();
        set_cached_id_token_source(
            &storage,
            &clock,
            "https://idp.example",
            &token,
            2000000000.125 * 1000.0,
        )
        .await
        .unwrap();
        let restored = get_cached_id_token_source(&storage, &clock, "https://idp.example")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.string_units(""), token.string_units(""));
        assert_eq!(
            get_cached_id_token(&storage, &clock, "https://idp.example")
                .await
                .unwrap()
                .unwrap(),
            "fake\u{FFFD}token"
        );
        let data = storage
            .retrieve(XAA_IDP_SERVICE, &issuer_key("https://idp.example"))
            .await
            .unwrap()
            .unwrap();
        let stored =
            Utf16JsonProjection::parse(&String::from_utf8_lossy(data.expose_secret_bytes()))
                .unwrap();
        assert_eq!(stored.string_units("/id_token"), token.string_units(""));
        assert_eq!(stored.number_at("/expires_at_unix"), Some(2000000000.125));
    }

    #[tokio::test]
    async fn native_322_idp_exact_milliseconds_win_over_legacy_seconds_including_null() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_xaa_2_1_322.json")).unwrap();
        let memory = Arc::new(MemStorage::default());
        let storage: Arc<dyn SecureStorage> = memory.clone();
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH + Duration::from_secs(1000)));
        let key = issuer_key("https://idp.example");
        for case in fixture["cacheMillisecondsCases"].as_array().unwrap() {
            let fields = case["raw_expiry_json"]
                .as_str()
                .map(|raw| format!(",\"expires_at_millis\":{raw},\"expires_at_unix\":2000000000"))
                .unwrap_or_default();
            memory
                .put_json(
                    XAA_IDP_SERVICE,
                    &key,
                    &format!("{{\"id_token\":\"fake-token\"{fields}}}"),
                )
                .await;
            assert_eq!(
                get_cached_id_token_source(&storage, &clock, "https://idp.example")
                    .await
                    .unwrap()
                    .is_some(),
                case["cached"].as_bool().unwrap(),
                "{case}"
            );
        }
        let exact_millis = 545531393461.19696;
        assert_ne!(
            exact_millis / 1000.0 * 1000.0,
            exact_millis,
            "fixture exposes old millisecond rounding"
        );
        set_cached_id_token_source(
            &storage,
            &clock,
            "https://idp.example",
            &"fake-token".into(),
            exact_millis,
        )
        .await
        .unwrap();
        let stored = storage
            .retrieve(XAA_IDP_SERVICE, &key)
            .await
            .unwrap()
            .unwrap();
        let source =
            Utf16JsonProjection::parse(&String::from_utf8_lossy(stored.expose_secret_bytes()))
                .unwrap();
        assert_eq!(source.number_at("/expires_at_millis"), Some(exact_millis));
        assert_eq!(
            source.number_at("/expires_at_unix"),
            Some(exact_millis / 1000.0)
        );
    }

    #[tokio::test]
    async fn provider_binds_server_as_storage_without_rebinding_the_global_idp_cache() {
        let memory = Arc::new(MemStorage::default());
        let selected_memory = Arc::new(MemStorage::default());
        memory
            .put_json(
                MCP_OAUTH_CLIENT_CONFIG_SERVICE,
                "same-server-key",
                r#"{"clientSecret":"scalar-secret"}"#,
            )
            .await;
        selected_memory
            .put_json(
                MCP_OAUTH_CLIENT_CONFIG_SERVICE,
                "same-server-key",
                r#"{"clientSecret":"raw\ud800secret"}"#,
            )
            .await;
        memory
            .put_json(
                XAA_IDP_CONFIG_SERVICE,
                &issuer_key("https://idp.example"),
                r#"{"clientSecret":"global-idp-secret"}"#,
            )
            .await;
        let storage: Arc<dyn SecureStorage> = memory;
        let selected_storage: Arc<dyn SecureStorage> = selected_memory;
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let clock: Arc<dyn Clock> = Arc::new(TestClock(now));
        set_cached_id_token(
            &storage,
            &clock,
            "https://idp.example",
            "global-idp-token",
            now + Duration::from_secs(3_600),
        )
        .await
        .unwrap();
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![(
                HttpMethod::Get,
                "openid-configuration".into(),
                200,
                r#"{"authorization_endpoint":"https://idp.example/authorize","token_endpoint":"https://idp.example/token","issuer":"https://idp.example","jwks_uri":"https://idp.example/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#.into(),
            )],
        });
        let provider = XaaIdpConfigProvider::new(
            http,
            clock.clone(),
            storage.clone(),
            Arc::new(|_| panic!("the global MCP IdP cache must be reused")),
            XaaIdpSettings {
                issuer: "https://idp.example".into(),
                client_id: "idp-client".into(),
                callback_port: None,
            },
            Arc::new(StaticLookup(Some((
                "lookup-client".into(),
                "same-server-key".into(),
            )))),
        );
        let bound = provider
            .for_server_credentials(
                "utf16-d800",
                "same-server-key",
                Some("raw-bound-client"),
                selected_storage.clone(),
            )
            .expect("concrete provider binds host-proven credentials");
        assert!(bound
            .peek_id_token_cache_hit("utf16-d800", "https://mcp.example")
            .await
            .unwrap());
        let inputs = bound
            .xaa_inputs("utf16-d800", "https://mcp.example")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inputs.client_id, "raw-bound-client");
        assert_eq!(inputs.client_secret, "raw\u{fffd}secret");
        assert_eq!(
            inputs.idp_client_secret.as_deref(),
            Some("global-idp-secret")
        );
        assert_eq!(inputs.idp_id_token, "global-idp-token");
        assert_eq!(
            bound
                .as_client_secret_source("utf16-d800", "https://mcp.example")
                .await
                .unwrap()
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#""raw\ud800secret""#
        );
        assert_eq!(
            provider
                .as_client_secret_source("utf16-d800", "https://mcp.example")
                .await
                .unwrap()
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#""scalar-secret""#
        );
        bound.clear_id_token().await.unwrap();
        assert!(get_cached_id_token(&storage, &clock, "https://idp.example")
            .await
            .unwrap()
            .is_none());
        assert!(selected_storage
            .contains(MCP_OAUTH_CLIENT_CONFIG_SERVICE, "same-server-key")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn native_322_xaa_provider_exposes_exact_as_secret_separately_from_wire_view() {
        let memory = Arc::new(MemStorage::default());
        memory
            .put_json(
                MCP_OAUTH_CLIENT_CONFIG_SERVICE,
                "fake-server-key",
                r#"{"clientSecret":"fake\ud800secret"}"#,
            )
            .await;
        let storage: Arc<dyn SecureStorage> = memory;
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp { routes: Vec::new() });
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH));
        let provider = XaaIdpConfigProvider::new(
            http,
            clock,
            storage.clone(),
            Arc::new(|_| {}),
            XaaIdpSettings {
                issuer: "https://idp.example".into(),
                client_id: "fake-idp-client".into(),
                callback_port: None,
            },
            Arc::new(StaticLookup(Some((
                "fake-client".into(),
                "fake-server-key".into(),
            )))),
        );
        let source = provider
            .as_client_secret_source("fake-server", "https://mcp.example")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.to_json_string().unwrap(), r#""fake\ud800secret""#);
        assert_eq!(
            get_as_client_secret(&storage, "fake-server-key")
                .await
                .unwrap()
                .unwrap(),
            "fake\u{FFFD}secret"
        );
    }

    struct NativeWireCapture {
        seen: Mutex<Vec<HttpRequest>>,
        body: String,
    }
    #[async_trait::async_trait]
    impl HttpTransport for NativeWireCapture {
        async fn request(
            &self,
            request: HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            self.seen.lock().await.push(request);
            Ok(lingxi_core::types::HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: self.body.clone(),
                body_bytes: Vec::new(),
            })
        }
        async fn stream_sse(
            &self,
            _: HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::Connection("unused".into()))
        }
    }

    #[tokio::test]
    async fn native_322_idp_discovery_url_and_authored_headers() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_xaa_2_1_322.json")).unwrap();
        let body = r#"{"issuer":"https://idp.example","authorization_endpoint":"https://idp.example/authorize","token_endpoint":"https://idp.example/token","jwks_uri":"https://idp.example/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#;
        for case in fixture["oidcCases"].as_array().unwrap() {
            let capture = Arc::new(NativeWireCapture {
                seen: Mutex::new(Vec::new()),
                body: body.into(),
            });
            let http: Arc<dyn HttpTransport> = capture.clone();
            let result = discover_oidc(&http, case["input"].as_str().unwrap()).await;
            assert_eq!(result.is_err(), case.get("error").is_some());
            let seen = capture.seen.lock().await;
            if case.get("request").is_some() {
                assert_eq!(seen[0].url, case["request"]["url"].as_str().unwrap());
                assert_eq!(
                    seen[0].headers,
                    vec![("Accept".into(), "application/json".into())]
                );
            } else {
                assert!(seen.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn native_322_idp_metadata_keeps_issuer_units_without_rejecting_valid_source_strings() {
        let capture = Arc::new(NativeWireCapture { seen:Mutex::new(Vec::new()),body:r#"{"issuer":"https://idp.example/\ud800","authorization_endpoint":"https://idp.example/\ud800","token_endpoint":"https://idp.example/\udfff","jwks_uri":"https://idp.example/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}"#.into() });
        let http: Arc<dyn HttpTransport> = capture;
        let document = discover_oidc_document(&http, "https://idp.example")
            .await
            .unwrap();
        assert_eq!(
            document.metadata.authorization_endpoint,
            "https://idp.example/\u{FFFD}"
        );
        assert_eq!(
            document.metadata.token_endpoint,
            "https://idp.example/\u{FFFD}"
        );
        assert_eq!(document.issuer_units.last(), Some(&0xD800));
        assert_ne!(
            document.issuer_units,
            "https://idp.example/\u{FFFD}"
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn native_322_idp_code_exchange_uses_metadata_auth_selector_and_form_order() {
        let clock: Arc<dyn Clock> = Arc::new(TestClock(UNIX_EPOCH));
        for (supported, secret, basic, expected_tail) in [
            (
                vec!["client_secret_basic"],
                Some("fake-secret"),
                true,
                "redirect_uri=http%3A%2F%2Flocalhost%3A31337%2Fcallback",
            ),
            (
                vec!["client_secret_post"],
                Some("fake-secret"),
                false,
                "client_id=fake-client&client_secret=fake-secret",
            ),
            (
                vec!["none"],
                Some("fake-secret"),
                false,
                "client_id=fake-client",
            ),
            (Vec::new(), Some(""), false, "client_id=fake-client"),
        ] {
            let capture = Arc::new(NativeWireCapture { seen:Mutex::new(Vec::new()),body:r#"{"access_token":"fake","token_type":"Bearer","id_token":"fake-id-token","expires_in":"1.25"}"#.into() });
            let http: Arc<dyn HttpTransport> = capture.clone();
            let metadata:oauth::AuthServerMetadata = serde_json::from_value(serde_json::json!({"authorization_endpoint":"https://idp.example/authorize","token_endpoint":"https://idp.example/token","token_endpoint_auth_methods_supported":supported})).unwrap();
            let (token, expiry, _) = exchange_code_for_id_token(
                &http,
                &clock,
                &metadata,
                "fake-client",
                secret,
                "fake +~!*code",
                "fake-verifier",
                "http://localhost:31337/callback",
            )
            .await
            .unwrap();
            assert_eq!(token, "fake-id-token");
            assert_eq!(expiry, Some(1.25));
            let seen = capture.seen.lock().await;
            let body = seen[0].body.as_ref().unwrap();
            assert!(body.starts_with("grant_type=authorization_code&code=fake+%2B%7E%21*code&code_verifier=fake-verifier&redirect_uri="));
            assert!(body.ends_with(expected_tail));
            assert_eq!(
                seen[0]
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("authorization")),
                basic
            );
        }
    }
}
