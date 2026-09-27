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
use platform_api::{Clock, HttpTransport, McpError, McpTransportSpec, SecureStorage};
use protocol::{HttpMethod, HttpRequest};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// IdP request deadline (xaaIdpLogin.ts `IDP_REQUEST_TIMEOUT_MS = 30000`).
const IDP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Normalize an IdP issuer for use as a cache key: strip trailing slashes and
/// lowercase the host. Issuers from config vs OIDC discovery can differ
/// cosmetically but should hit the same cache slot (`issuerKey`).
///
/// We avoid a `url` dep (not in mcp's tree) and apply the same pragmatic subset
/// `xaa::normalize_url` uses: lowercase scheme + host, strip a trailing slash.
#[must_use]
pub fn issuer_key(issuer: &str) -> String {
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
    let base = if issuer.ends_with('/') {
        issuer.to_string()
    } else {
        format!("{issuer}/")
    };
    let url = format!("{base}.well-known/openid-configuration");
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: url.clone(),
        headers: vec![("accept".into(), "application/json".into())],
        body: None,
        body_bytes: None,
        timeout: Some(IDP_REQUEST_TIMEOUT),
    };
    let resp = http
        .request(req)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: OIDC discovery transport: {e}")))?;
    if !(200..300).contains(&resp.status) {
        return Err(McpError::OAuth(format!(
            "XAA IdP: OIDC discovery failed: HTTP {} at {url}",
            resp.status
        )));
    }
    let meta: OidcMetadata = serde_json::from_str(&resp.body).map_err(|_| {
        McpError::OAuth(format!(
            "XAA IdP: OIDC discovery returned invalid/non-JSON metadata at {url}"
        ))
    })?;
    if url::Url::parse(&meta.token_endpoint)
        .ok()
        .is_none_or(|endpoint| endpoint.scheme() != "https")
    {
        return Err(McpError::OAuth(format!(
            "XAA IdP: refusing non-HTTPS token endpoint: {}",
            meta.token_endpoint
        )));
    }
    Ok(meta)
}

// ---------------------------------------------------------------------------
// id_token JWT exp parsing (xaaIdpLogin.ts:252-263).
// ---------------------------------------------------------------------------

/// Decode the `exp` claim (seconds since epoch) from a JWT without verifying
/// its signature. Returns `None` if parsing fails or `exp` is absent. Used only
/// to derive a cache TTL — see xaaIdpLogin.ts:239-251 for why no signature /
/// iss / aud validation is needed (the IdP validates its own token at the
/// RFC 8693 exchange).
#[must_use]
pub fn jwt_exp(jwt: &str) -> Option<u64> {
    use base64::Engine;
    let mut parts = jwt.split('.');
    let (_h, payload, _s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None; // more than 3 segments — not a compact JWS.
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp").and_then(serde_json::Value::as_u64)
}

// ---------------------------------------------------------------------------
// id_token cache (xaaIdpLogin.ts:99-150).
// ---------------------------------------------------------------------------

/// On-disk shape for a cached id_token: the token plus its absolute expiry
/// (Unix seconds). Mirrors the TS `{ idToken, expiresAt }` slot.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CachedIdToken {
    id_token: String,
    /// Absolute expiry, seconds since the Unix epoch.
    expires_at_unix: u64,
}

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
    let key = issuer_key(issuer);
    let data = storage
        .retrieve(XAA_IDP_SERVICE, &key)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: id_token cache retrieve: {e}")))?;
    let Some(data) = data else { return Ok(None) };
    let cached: CachedIdToken = match serde_json::from_slice(data.expose_secret_bytes()) {
        Ok(c) => c,
        // A corrupt cache entry is a miss, not a hard error.
        Err(_) => return Ok(None),
    };
    let expires_at = UNIX_EPOCH + Duration::from_secs(cached.expires_at_unix);
    // Within the buffer of expiring (or already past) → treat as a miss.
    if expires_at <= clock.now() + ID_TOKEN_EXPIRY_BUFFER {
        return Ok(None);
    }
    Ok(Some(cached.id_token))
}

/// Persist an id_token for `issuer` with an absolute expiry (`saveIdpIdToken`,
/// xaaIdpLogin.ts:109-123).
async fn set_cached_id_token(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    issuer: &str,
    id_token: &str,
    expires_at: SystemTime,
) -> Result<(), McpError> {
    let cached = CachedIdToken {
        id_token: id_token.to_string(),
        expires_at_unix: expires_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let bytes = serde_json::to_vec(&cached)
        .map_err(|e| McpError::OAuth(format!("XAA IdP: encode id_token: {e}")))?;
    let metadata = protocol::SecureStorageMetadata {
        created_at: clock.now(),
        last_accessed: None,
        kind: protocol::SecretKindDto("xaa_idp_id_token".into()),
    };
    let data = protocol::SecureStorageData::new(bytes, metadata);
    storage
        .store(XAA_IDP_SERVICE, &issuer_key(issuer), data)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: id_token cache store: {e}")))?;
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
    let expires_at = match jwt_exp(jwt) {
        Some(exp) => UNIX_EPOCH + Duration::from_secs(exp),
        None => clock.now() + ID_TOKEN_NO_EXP_FALLBACK,
    };
    set_cached_id_token(storage, clock, issuer, jwt, expires_at).await?;
    Ok(expires_at)
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
    storage
        .delete(XAA_IDP_SERVICE, &issuer_key(issuer))
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: id_token cache delete: {e}")))
}

// ---------------------------------------------------------------------------
// IdP client secret (xaaIdpLogin.ts:177-181, `getIdpClientSecret`).
// ---------------------------------------------------------------------------

/// On-disk shape for the IdP client secret slot (`{ clientSecret }`).
#[derive(Debug, serde::Deserialize)]
struct StoredClientSecret {
    #[serde(rename = "clientSecret")]
    client_secret: String,
}

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
    read_client_secret(storage, XAA_IDP_CONFIG_SERVICE, &issuer_key(issuer)).await
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
        .map_err(|e| McpError::OAuth(format!("XAA IdP: encode secret: {e}")))?;
    let metadata = protocol::SecureStorageMetadata {
        created_at: clock.now(),
        last_accessed: None,
        kind: protocol::SecretKindDto("xaa_idp_client_secret".into()),
    };
    let data = protocol::SecureStorageData::new(bytes, metadata);
    storage
        .store(XAA_IDP_CONFIG_SERVICE, &issuer_key(issuer), data)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: secret store: {e}")))
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
    storage
        .delete(XAA_IDP_CONFIG_SERVICE, &issuer_key(issuer))
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: secret delete: {e}")))
}

/// Read the AS confidential-client secret for a server
/// (`mcpOAuthClientConfig[serverKey].clientSecret`, auth.ts:1500 / 2430-2437).
/// Read from the `mcp-oauth-client-config` service keyed by
/// [`oauth::server_key`].
async fn get_as_client_secret(
    storage: &Arc<dyn SecureStorage>,
    server_key: &str,
) -> Result<Option<String>, McpError> {
    read_client_secret(storage, MCP_OAUTH_CLIENT_CONFIG_SERVICE, server_key).await
}

/// Shared `{ clientSecret }` reader for the two confidential-secret slots.
async fn read_client_secret(
    storage: &Arc<dyn SecureStorage>,
    service: &str,
    account: &str,
) -> Result<Option<String>, McpError> {
    let data = storage
        .retrieve(service, account)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: secret retrieve ({service}): {e}")))?;
    let Some(data) = data else { return Ok(None) };
    let parsed: StoredClientSecret = serde_json::from_slice(data.expose_secret_bytes())
        .map_err(|e| McpError::OAuth(format!("XAA IdP: decode secret ({service}): {e}")))?;
    Ok(Some(parsed.client_secret))
}

// ---------------------------------------------------------------------------
// acquire_idp_id_token (xaaIdpLogin.ts:401-487).
// ---------------------------------------------------------------------------

/// `authorization_code`-grant token response, extracting the OIDC `id_token`.
/// The oauth.rs `Tokens` deliberately drops `id_token` (OAuth, not OIDC); we
/// parse our own minimal shape so the openid-scope response surfaces it.
#[derive(Debug, serde::Deserialize)]
struct OidcTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

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
    if let Some(cached) = get_cached_id_token(storage, clock, &settings.issuer).await? {
        return Ok(cached);
    }

    // 1. OIDC discovery → authorization + token endpoints.
    let oidc = discover_oidc(http, &settings.issuer).await?;
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
        token_endpoint_auth_methods_supported: None,
    };

    // 2. Bind the loopback listener FIRST so the redirect_uri is known before
    //    the authorize URL is built. A configured callback_port pins it.
    let listener = oauth::CallbackListener::bind(settings.callback_port.unwrap_or(0))
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: callback bind: {e}")))?;
    let port = listener.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    // 3. Authorize URL with scope=openid (the delta vs the consent flow), PKCE
    //    inside. Surface it to the host (browser-open / log).
    let (auth_url, verifier, state) =
        oauth::build_authorize_url(&meta, &settings.client_id, &redirect_uri, "openid");
    on_authorization_url(&auth_url);

    // 4. Wait for the redirect, validate state, capture the code.
    let params = listener
        .accept(&state, &redirect_uri)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: callback: {e}")))?;

    // 5. Exchange the code for tokens, extracting the id_token. We POST the same
    //    authorization_code grant oauth::exchange_code does, but parse our own
    //    OIDC shape (oauth::Tokens drops id_token). `client_secret` is sent via
    //    client_secret_post when the IdP client is confidential.
    let (id_token, expires_in) = exchange_code_for_id_token(
        http,
        &oidc.token_endpoint,
        &settings.client_id,
        idp_client_secret,
        &params.code,
        &verifier,
        &redirect_uri,
    )
    .await?;

    // 6. Cache it — prefer the id_token's own `exp` claim, else the AS-provided
    //    `expires_in`, else +1h (xaaIdpLogin.ts:475-478:
    //    `expFromJwt ? expFromJwt : now + (expires_in ?? 3600)`).
    let expires_at = match jwt_exp(&id_token) {
        Some(exp) => UNIX_EPOCH + Duration::from_secs(exp),
        None => clock.now() + Duration::from_secs(expires_in.unwrap_or(3600)),
    };
    set_cached_id_token(storage, clock, &settings.issuer, &id_token, expires_at).await?;

    Ok(id_token)
}

/// POST the `authorization_code` grant and return the `id_token` (the openid
/// delta over [`oauth::exchange_code`]). `client_secret` is sent via
/// `client_secret_post` when present (xaaIdpLogin.ts:418-432).
async fn exchange_code_for_id_token(
    http: &Arc<dyn HttpTransport>,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(String, Option<u64>), McpError> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let req = HttpRequest {
        method: HttpMethod::Post,
        url: token_endpoint.to_string(),
        headers: vec![
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("accept".into(), "application/json".into()),
        ],
        body: Some(body),
        body_bytes: None,
        timeout: Some(IDP_REQUEST_TIMEOUT),
    };
    let resp = http
        .request(req)
        .await
        .map_err(|e| McpError::OAuth(format!("XAA IdP: token exchange transport: {e}")))?;
    if !(200..300).contains(&resp.status) {
        return Err(McpError::OAuth(format!(
            "XAA IdP: token exchange failed: HTTP {}",
            resp.status
        )));
    }
    let parsed: OidcTokenResponse = serde_json::from_str(&resp.body)
        .map_err(|e| McpError::OAuth(format!("XAA IdP: decode token response: {e}")))?;
    let id_token = parsed.id_token.ok_or_else(|| {
        McpError::OAuth("XAA IdP: token response missing id_token (check scope=openid)".into())
    })?;
    // Return `expires_in` so the caller can use it as the cache-TTL fallback when
    // the id_token carries no `exp` claim (xaaIdpLogin.ts:475-478).
    Ok((id_token, parsed.expires_in))
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
        }
    }
}

#[async_trait::async_trait]
impl XaaConfigProvider for XaaIdpConfigProvider {
    async fn xaa_inputs(
        &self,
        server_name: &str,
        _server_url: &str,
    ) -> Result<Option<XaaInputs>, McpError> {
        // Resolve the AS client_id + server_key for this server. Unknown server
        // (or no AS client_id) → not XAA-provisioned (auth.ts:683-688 raises;
        // our seam returns Ok(None) so the registry emits its hard-fail copy).
        let Some((as_client_id, server_key)) = self.server_lookup.lookup(server_name).await else {
            return Ok(None);
        };

        // AS confidential client_secret (mcpOAuthClientConfig[serverKey]). Absent
        // → can't run the confidential jwt-bearer leg; treat as not provisioned
        // (auth.ts:691-711 raises; we surface the actionable error to the user).
        let Some(as_client_secret) = get_as_client_secret(&self.storage, &server_key).await? else {
            return Err(McpError::OAuth(format!(
                "XAA: AS client secret not found for '{server_name}'. \
                 Re-add the server with its --client-secret."
            )));
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
        let Some((_as_client_id, server_key)) = self.server_lookup.lookup(server_name).await else {
            return Err(McpError::OAuth(format!(
                "XAA: server '{server_name}' is not XAA-provisioned (no AS client config)."
            )));
        };
        if get_as_client_secret(&self.storage, &server_key)
            .await?
            .is_none()
        {
            return Err(McpError::OAuth(format!(
                "XAA: AS client secret not found for '{server_name}'. Re-add the server with its --client-secret."
            )));
        }
        let _ = get_idp_client_secret(&self.storage, &self.settings.issuer).await?;
        Ok(
            get_cached_id_token(&self.storage, &self.clock, &self.settings.issuer)
                .await?
                .is_some(),
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
            data: protocol::SecureStorageData,
        ) -> Result<(), platform_api::SecureStorageError> {
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
        ) -> Result<Option<protocol::SecureStorageData>, platform_api::SecureStorageError> {
            let map = self.map.lock().await;
            Ok(map.get(&(service.into(), account.into())).map(|bytes| {
                protocol::SecureStorageData::new(
                    bytes.clone(),
                    protocol::SecureStorageMetadata {
                        created_at: UNIX_EPOCH,
                        last_accessed: None,
                        kind: protocol::SecretKindDto("test".into()),
                    },
                )
            }))
        }
        async fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), platform_api::SecureStorageError> {
            self.map
                .lock()
                .await
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, platform_api::SecureStorageError> {
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
        fn backend(&self) -> platform_api::SecureStorageBackend {
            platform_api::SecureStorageBackend::PlainText
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
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            for (method, needle, status, body) in &self.routes {
                if *method == req.method && req.url.contains(needle.as_str()) {
                    return Ok(protocol::HttpResponse {
                        status: *status,
                        headers: vec![],
                        body: body.clone(),
                        body_bytes: Vec::new(),
                    });
                }
            }
            Err(platform_api::HttpError::Connection(format!(
                "no route for {}",
                req.url
            )))
        }
        async fn stream_sse(
            &self,
            _req: HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
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
                r#"{"authorization_endpoint":"https://idp.example.com/authorize",
                    "token_endpoint":"https://idp.example.com/token"}"#
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
                r#"{"authorization_endpoint":"https://idp/authorize",
                    "token_endpoint":"http://idp/token"}"#
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
        let token_body =
            format!(r#"{{"access_token":"at","id_token":"{id_token}","expires_in":3600}}"#);
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![
                (
                    HttpMethod::Get,
                    "openid-configuration".into(),
                    200,
                    // authorize endpoint is unused (we drive the callback directly),
                    // but token endpoint must be HTTPS.
                    r#"{"authorization_endpoint":"https://idp/authorize",
                        "token_endpoint":"https://idp/token"}"#
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
        let token_body = format!(r#"{{"access_token":"at","id_token":"{id_token}"}}"#);
        let http: Arc<dyn HttpTransport> = Arc::new(ScriptedHttp {
            routes: vec![
                (
                    HttpMethod::Get,
                    "openid-configuration".into(),
                    200,
                    r#"{"authorization_endpoint":"https://idp/authorize",
                        "token_endpoint":"https://idp/token"}"#
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
}
