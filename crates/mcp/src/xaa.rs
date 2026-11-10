//! Cross-App Access (XAA) / Enterprise Managed Authorization (SEP-990).
//!
//! Faithful core port of `claude-code/src/services/mcp/xaa.ts` (512 lines).
//!
//! Obtains an MCP access token WITHOUT a browser consent screen by chaining:
//!   1. RFC 8693 Token Exchange at the IdP: `id_token` → ID-JAG.
//!   2. RFC 7523 JWT Bearer Grant at the AS: ID-JAG → `access_token`.
//!
//! Spec refs: ID-JAG (IETF draft), MCP ext-auth (SEP-990), RFC 8693 (Token
//! Exchange), RFC 7523 (JWT Bearer), RFC 9728 (PRM).
//!
//! Structure mirrors the TS module: four Layer-2 ops (discover PRM, discover
//! AS, request ID-JAG, exchange ID-JAG) + one Layer-3 orchestrator
//! ([`perform_cross_app_access`]) that composes them. All HTTP goes through the
//! injected [`HttpTransport`] (no `reqwest`); validation is hand-written serde
//! (no `zod`).
//!
//! The IdP-login/config/cache surface lives in [`crate::xaa_idp`]. MCP
//! token auth uses the native basic/post methods; generic account-library
//! client-assertion/JWT authentication is outside this MCP protocol path.
//!
//! The doc comments here lean on dense OAuth/OIDC vocabulary (IdP, OIDC,
//! ID-JAG, the RFC token-type URNs); backticking every term hurts readability,
//! so this module opts out of `clippy::doc_markdown` (matching the
//! established pattern in `tui/src/render/*`).
#![allow(clippy::doc_markdown)]

use base64::Engine;
use lingxi_core::host::HttpTransport;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{HttpMethod, HttpRequest};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
#[path = "xaa/native_322_tests.rs"]
mod native_322_tests;

/// XAA request deadline (xaa.ts `XAA_REQUEST_TIMEOUT_MS = 30000`).
const XAA_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// RFC 8693 token-exchange grant type.
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
/// RFC 7523 JWT-bearer grant type.
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// ID-JAG token type (the requested + issued type at the IdP exchange).
const ID_JAG_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:id-jag";
/// `id_token` subject-token type for the IdP exchange.
const ID_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:id_token";

/// XAA-flow failures.
#[derive(Debug, thiserror::Error)]
pub enum XaaError {
    /// PRM discovery failed or the PRM was inconsistent (resource mismatch).
    #[error("XAA: PRM discovery failed: {0}")]
    Prm(String),
    /// AS-metadata discovery failed (issuer mismatch, non-HTTPS endpoint, none).
    #[error("XAA: AS metadata discovery failed: {0}")]
    AsMetadata(String),
    /// The IdP token-exchange (id_token → ID-JAG) leg failed.
    ///
    /// Carries `should_clear_id_token` (xaa.ts `XaaTokenExchangeError`): on a
    /// 4xx / structurally-invalid body the cached `id_token` is bad and should
    /// be dropped; on a 5xx / transient non-JSON it may still be valid.
    #[error("{}", message.string_at("").unwrap_or_default())]
    TokenExchange {
        /// Human-readable, token-redacted error detail.
        message: Utf16JsonProjection,
        /// Whether the caller should drop the cached `id_token`.
        should_clear_id_token: bool,
    },
    /// The AS jwt-bearer (ID-JAG → access_token) leg failed.
    #[error("{}", .0.string_at("").unwrap_or_default())]
    JwtBearer(Utf16JsonProjection),
    /// No advertised authorization server supports the jwt-bearer grant.
    #[error("XAA: no authorization server supports jwt-bearer. Tried: {0}")]
    NoAuthServer(String),
}

impl XaaError {
    /// Preserve native UTF16 diagnostic units across the MCP error boundary.
    #[must_use]
    pub fn source_diagnostic(&self) -> Utf16JsonProjection {
        match self {
            Self::TokenExchange { message, .. } | Self::JwtBearer(message) => message.clone(),
            _ => self.to_string().into(),
        }
    }

    /// Whether the cached IdP `id_token` should be dropped in response to this
    /// failure (xaa.ts `XaaTokenExchangeError.shouldClearIdToken`, 267-273).
    ///
    /// Only a [`TokenExchange`](XaaError::TokenExchange) failure carries the
    /// signal: a 4xx (or structurally-invalid 200 body) means the `id_token`
    /// itself was rejected and must be re-acquired; a 5xx / transport failure
    /// is an IdP outage and the token is kept. Every other variant is `false`.
    #[must_use]
    pub fn should_clear_id_token(&self) -> bool {
        matches!(
            self,
            XaaError::TokenExchange {
                should_clear_id_token: true,
                ..
            }
        )
    }
}

impl From<XaaError> for lingxi_core::host::McpError {
    fn from(e: XaaError) -> Self {
        lingxi_core::host::McpError::OAuth(e.source_diagnostic())
    }
}

/// Auth method used when presenting the confidential-client secret to a token
/// endpoint (RFC 6749 §2.3.1). `Basic` is the SEP-990 conformance default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// `Authorization: Basic base64(urlencode(id):urlencode(secret))`.
    ClientSecretBasic,
    /// `client_id` + `client_secret` in the form body.
    ClientSecretPost,
}

// ---------------------------------------------------------------------------
// Token redaction (xaa.ts:86-97), including ECMAScript whitespace.
// ---------------------------------------------------------------------------

/// Redact token-bearing values from a (possibly raw) error body before logging,
/// matching the keys xaa.ts's `SENSITIVE_TOKEN_RE` redacts. A misbehaving AS
/// that echoes `subject_token`/`assertion`/`client_secret` in a 4xx envelope
/// must not leak into debug logs.
#[must_use]
pub fn redact_tokens(raw: &str) -> String {
    static TOKEN_PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = TOKEN_PATTERN.get_or_init(|| regex::Regex::new(
        r#""(access_token|refresh_token|id_token|assertion|subject_token|client_secret)"[\t\n\r\x0B\x0C \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*:[\t\n\r\x0B\x0C \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*"[^"]*""#
    ).expect("native token redaction regex"));
    pattern
        .replace_all(raw, |captures: &regex::Captures<'_>| {
            format!("\"{}\":\"[REDACTED]\"", &captures[1])
        })
        .into_owned()
}

// ---------------------------------------------------------------------------
// URL normalization (xaa.ts:56-67) — RFC 8414 §3.3 / RFC 9728 §3.3 compare.
// ---------------------------------------------------------------------------

/// Exact `new URL(value).href.replace(/\/$/, "")` normalization used by XAA.
/// Invalid input falls back to the original string with one trailing slash
/// removed, matching the JavaScript catch arm.
fn normalize_url(url: &str) -> String {
    let normalized = url::Url::parse(url).map_or_else(|_| url.to_string(), |url| url.to_string());
    normalized
        .strip_suffix('/')
        .unwrap_or(normalized.as_str())
        .to_string()
}

// ---------------------------------------------------------------------------
// HTTP helpers over the injected transport.
// ---------------------------------------------------------------------------

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

async fn get_document(
    http: &Arc<dyn HttpTransport>,
    url: &str,
    accept: bool,
) -> Result<lingxi_core::types::HttpResponse, String> {
    let mut headers = vec![("MCP-Protocol-Version".into(), MCP_PROTOCOL_VERSION.into())];
    if accept {
        headers.push(("Accept".into(), "application/json".into()));
    }
    http.request(HttpRequest {
        method: HttpMethod::Get,
        url: url.to_owned(),
        headers,
        body: None,
        body_bytes: None,
        timeout: Some(XAA_REQUEST_TIMEOUT),
    })
    .await
    .map_err(|error| format!("transport: {error}"))
}

fn well_known(base: &str, suffix: &str) -> Result<String, String> {
    let mut url = url::Url::parse(base).map_err(|error| error.to_string())?;
    let path = url
        .path()
        .strip_suffix('/')
        .unwrap_or(url.path())
        .to_owned();
    url.set_path(&format!("/.well-known/{suffix}{path}"));
    url.set_fragment(None);
    Ok(url.to_string())
}

fn metadata_candidates(base: &str) -> Result<Vec<(String, bool)>, String> {
    let url = url::Url::parse(base).map_err(|error| error.to_string())?;
    let origin = url.origin().ascii_serialization();
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let mut candidates = vec![
        (
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            false,
        ),
        (
            format!("{origin}/.well-known/openid-configuration{path}"),
            true,
        ),
    ];
    if !path.is_empty() {
        candidates.push((
            format!("{origin}{path}/.well-known/openid-configuration"),
            true,
        ));
    }
    Ok(candidates)
}

fn form_encode(form: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().copied())
        .finish()
}

fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn sliced_error(prefix: &str, raw: &str) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(redact_tokens(raw).encode_utf16().take(200));
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("native diagnostic units")
}

fn diagnostic_with_units(prefix: &str, value: &[u16]) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend_from_slice(value);
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("native diagnostic units")
}

fn json_stringify(raw: &str) -> String {
    Utf16JsonProjection::parse(raw)
        .and_then(|value| value.to_json_string())
        .unwrap_or_else(|_| raw.to_owned())
}

// ─── Layer 2: Discovery ─────────────────────────────────────────────────────

/// RFC 9728 protected-resource metadata.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProtectedResourceMetadata {
    /// The resource identifier the PRM describes (must match the MCP URL).
    pub resource: String,
    /// Authorization servers advertised for the resource.
    #[serde(default)]
    pub authorization_servers: Vec<String>,
}

/// RFC 8414 authorization-server metadata (XAA-relevant subset).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthorizationServerMetadata {
    /// Issuer identifier (must match the AS URL it was fetched from).
    pub issuer: String,
    /// Token endpoint (the jwt-bearer grant target; must be HTTPS).
    pub token_endpoint: String,
    /// Grant types the AS advertises, if any.
    #[serde(default)]
    pub grant_types_supported: Option<Vec<String>>,
    /// Token-endpoint client-auth methods, if any.
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

fn projection_strings(value: &Utf16JsonProjection, key: &str) -> Option<Vec<String>> {
    Some(
        value
            .get_property(key)?
            .array_entries()?
            .1
            .into_iter()
            .map(|(_, value)| value.as_str().expect("validated string array").to_owned())
            .collect(),
    )
}

/// RFC 9728 PRM discovery + §3.3 resource-mismatch validation (xaa.ts:135-165).
///
/// # Errors
/// [`XaaError::Prm`] on transport failure, a missing `resource`/
/// `authorization_servers`, or a resource that does not match `server_url`.
pub async fn discover_protected_resource(
    http: &Arc<dyn HttpTransport>,
    server_url: &str,
) -> Result<ProtectedResourceMetadata, XaaError> {
    let url = well_known(server_url, "oauth-protected-resource").map_err(XaaError::Prm)?;
    let mut response = get_document(http, &url, false)
        .await
        .map_err(XaaError::Prm)?;
    let server = url::Url::parse(server_url).map_err(|error| XaaError::Prm(error.to_string()))?;
    if server.path() != "/" && ((400..500).contains(&response.status) || response.status == 502) {
        response = get_document(
            http,
            server
                .join("/.well-known/oauth-protected-resource")
                .map_err(|error| XaaError::Prm(error.to_string()))?
                .as_str(),
            false,
        )
        .await
        .map_err(XaaError::Prm)?;
    }
    if !(200..300).contains(&response.status) {
        return Err(XaaError::Prm(format!(
            "HTTP {} fetching {url}",
            response.status
        )));
    }
    let validated = crate::oauth::schemas::validate_prm(&response.body).map_err(XaaError::Prm)?;
    let value = validated.projection();
    let prm = ProtectedResourceMetadata {
        resource: value
            .string_at("/resource")
            .expect("validated resource")
            .to_owned(),
        authorization_servers: projection_strings(value, "authorization_servers")
            .unwrap_or_default(),
    };
    if prm.resource.is_empty() || prm.authorization_servers.is_empty() {
        return Err(XaaError::Prm(
            "PRM missing resource or authorization_servers".into(),
        ));
    }
    if normalize_url(&prm.resource) != normalize_url(server_url) {
        return Err(XaaError::Prm(format!(
            "PRM resource mismatch: expected {server_url}, got {}",
            prm.resource
        )));
    }
    Ok(prm)
}

/// RFC 8414 AS-metadata discovery + §3.3 issuer-mismatch validation + the
/// HTTPS-only token-endpoint guard (xaa.ts:178-210).
///
/// # Errors
/// [`XaaError::AsMetadata`] on transport failure, missing metadata, an issuer
/// that does not match `as_url`, or a non-HTTPS token endpoint.
pub async fn discover_authorization_server(
    http: &Arc<dyn HttpTransport>,
    as_url: &str,
) -> Result<AuthorizationServerMetadata, XaaError> {
    Ok(discover_authorization_server_source(http, as_url).await?.0)
}

async fn discover_authorization_server_source(
    http: &Arc<dyn HttpTransport>,
    as_url: &str,
) -> Result<(AuthorizationServerMetadata, Utf16JsonProjection), XaaError> {
    let mut metadata = None;
    for (candidate, oidc) in metadata_candidates(as_url).map_err(XaaError::AsMetadata)? {
        let response = get_document(http, &candidate, true)
            .await
            .map_err(XaaError::AsMetadata)?;
        if !(200..300).contains(&response.status) {
            if (400..500).contains(&response.status) || response.status == 502 {
                continue;
            }
            return Err(XaaError::AsMetadata(format!(
                "HTTP {} fetching {candidate}",
                response.status
            )));
        }
        let validated = crate::oauth::schemas::validate_authorization(&response.body, oidc)
            .map_err(XaaError::AsMetadata)?;
        let value = validated.projection();
        metadata = Some((
            AuthorizationServerMetadata {
                issuer: value
                    .string_at("/issuer")
                    .expect("validated issuer")
                    .to_owned(),
                token_endpoint: value
                    .string_at("/token_endpoint")
                    .expect("validated endpoint")
                    .to_owned(),
                grant_types_supported: projection_strings(value, "grant_types_supported"),
                token_endpoint_auth_methods_supported: projection_strings(
                    value,
                    "token_endpoint_auth_methods_supported",
                ),
            },
            value
                .subprojection("/issuer")
                .map_err(|error| XaaError::AsMetadata(error.to_string()))?,
        ));
        break;
    }
    let (meta, issuer_source) =
        metadata.ok_or_else(|| XaaError::AsMetadata(format!("no valid metadata at {as_url}")))?;
    if meta.issuer.is_empty() || meta.token_endpoint.is_empty() {
        return Err(XaaError::AsMetadata(format!(
            "no valid metadata at {as_url}"
        )));
    }
    if normalize_url(&meta.issuer) != normalize_url(as_url) {
        return Err(XaaError::AsMetadata(format!(
            "issuer mismatch: expected {as_url}, got {}",
            meta.issuer
        )));
    }
    // RFC 8414 §3.3 / RFC 9728 §3 require HTTPS — refuse to POST an id_token +
    // client_secret over plaintext even if the issuer self-consistently
    // reported http:// (xaa.ts:195-202).
    if url::Url::parse(&meta.token_endpoint)
        .ok()
        .is_none_or(|endpoint| endpoint.scheme() != "https")
    {
        return Err(XaaError::AsMetadata(format!(
            "refusing non-HTTPS token endpoint: {}",
            meta.token_endpoint
        )));
    }
    Ok((meta, issuer_source))
}

// ─── Layer 2: Exchange ──────────────────────────────────────────────────────

/// Result of the IdP token exchange: the ID-JAG plus optional metadata.
#[derive(Clone)]
pub struct JwtAuthGrantResult {
    /// The ID-JAG (Identity Assertion Authorization Grant) JWT.
    pub jwt_auth_grant: String,
    /// Server-reported lifetime in seconds, if any.
    pub expires_in: Option<f64>,
    /// Granted scope, if any.
    pub scope: Option<String>,
    /// Native validated ID-JAG response retaining exact UTF16 token units.
    pub source: Utf16JsonProjection,
}

impl std::fmt::Debug for JwtAuthGrantResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JwtAuthGrantResult")
            .field("jwt_auth_grant", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("source", &"<redacted>")
            .finish()
    }
}

/// Inputs for the IdP token-exchange leg.
pub struct JwtAuthGrantRequest<'a> {
    /// IdP token endpoint (where the RFC 8693 exchange is POSTed).
    pub token_endpoint: &'a str,
    /// Audience (the target AS issuer).
    pub audience: &'a str,
    /// Resource (the MCP server / PRM `resource`).
    pub resource: &'a str,
    /// The user's OIDC `id_token` from the IdP login.
    pub id_token: &'a str,
    /// IdP-registered client id.
    pub client_id: &'a str,
    /// Optional IdP client secret (`client_secret_post` when present).
    pub client_secret: Option<&'a str>,
    /// Compatibility field; the current native XAA grant omits scope.
    pub scope: Option<&'a str>,
}

/// RFC 8693 Token Exchange at the IdP: `id_token` → ID-JAG (xaa.ts:233-310).
///
/// Validates that `issued_token_type` is the ID-JAG type. `client_secret` is
/// sent via `client_secret_post` when present (some IdPs register the client as
/// confidential even when advertising `none`).
///
/// # Errors
/// [`XaaError::TokenExchange`] with `should_clear_id_token` set per the
/// 4xx/5xx/structural rules in xaa.ts:265-304.
pub async fn request_jwt_authorization_grant(
    http: &Arc<dyn HttpTransport>,
    req: &JwtAuthGrantRequest<'_>,
) -> Result<JwtAuthGrantResult, XaaError> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", TOKEN_EXCHANGE_GRANT),
        ("requested_token_type", ID_JAG_TOKEN_TYPE),
        ("audience", req.audience),
        ("resource", req.resource),
        ("subject_token", req.id_token),
        ("subject_token_type", ID_TOKEN_TYPE),
        ("client_id", req.client_id),
    ];
    if let Some(secret) = req.client_secret.filter(|secret| !secret.is_empty()) {
        form.push(("client_secret", secret));
    }

    let http_req = HttpRequest {
        method: HttpMethod::Post,
        url: req.token_endpoint.to_string(),
        headers: vec![(
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        )],
        body: Some(form_encode(&form)),
        body_bytes: None,
        timeout: Some(XAA_REQUEST_TIMEOUT),
    };
    let resp = http
        .request(http_req)
        .await
        .map_err(|e| XaaError::TokenExchange {
            message: format!("XAA: token exchange failed: transport: {e}").into(),
            // Network/transport failure (captive portal etc.) — keep id_token.
            should_clear_id_token: false,
        })?;

    if !(200..300).contains(&resp.status) {
        // 4xx → id_token rejected, clear; 5xx → IdP outage, keep (xaa.ts:267-273).
        let should_clear = resp.status < 500;
        return Err(XaaError::TokenExchange {
            message: sliced_error(
                &format!("XAA: token exchange failed: HTTP {}: ", resp.status),
                &resp.body,
            ),
            should_clear_id_token: should_clear,
        });
    }

    if Utf16JsonProjection::parse(&resp.body).is_err() {
        return Err(XaaError::TokenExchange {
            message: format!(
                "XAA: token exchange returned non-JSON (captive portal?) at {}",
                req.token_endpoint
            )
            .into(),
            should_clear_id_token: false,
        });
    }
    let validated = crate::oauth::schemas::validate_xaa_exchange(&resp.body).map_err(|_| {
        XaaError::TokenExchange {
            message: format!(
                "XAA: token exchange response did not match expected shape: {}",
                redact_tokens(&json_stringify(&resp.body))
            )
            .into(),
            should_clear_id_token: true,
        }
    })?;
    let parsed = validated.projection();
    let Some(access_token) = parsed
        .string_at("/access_token")
        .filter(|token| !token.is_empty())
    else {
        return Err(XaaError::TokenExchange {
            message: format!(
                "XAA: token exchange response missing access_token: {}",
                redact_tokens(&validated.json().unwrap_or_default())
            )
            .into(),
            should_clear_id_token: true,
        });
    };
    if parsed.string_at("/issued_token_type") != Some(ID_JAG_TOKEN_TYPE) {
        let kind = parsed
            .string_units("/issued_token_type")
            .unwrap_or_else(|| "undefined".encode_utf16().collect());
        return Err(XaaError::TokenExchange {
            message: diagnostic_with_units(
                "XAA: token exchange returned unexpected issued_token_type: ",
                &kind,
            ),
            should_clear_id_token: true,
        });
    }
    Ok(JwtAuthGrantResult {
        jwt_auth_grant: access_token.to_owned(),
        expires_in: parsed.number_at("/expires_in"),
        scope: parsed.string_at("/scope").map(str::to_owned),
        source: validated.into_projection(),
    })
}

/// The XAA access-token result (jwt-bearer grant output).
#[derive(Clone)]
pub struct XaaTokenResult {
    /// The MCP access token.
    pub access_token: String,
    /// Token type (defaults to `Bearer` when the AS omits it).
    pub token_type: String,
    /// Lifetime in seconds, if any.
    pub expires_in: Option<f64>,
    /// Granted scope, if any.
    pub scope: Option<String>,
    /// Refresh token, if the AS issued one.
    pub refresh_token: Option<String>,
    /// Native validated token response, retaining exact UTF16 secret units for storage.
    pub source: Utf16JsonProjection,
}

impl std::fmt::Debug for XaaTokenResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("XaaTokenResult")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("source", &"<redacted>")
            .finish()
    }
}

/// The full XAA result: the token set plus the AS issuer URL that must be
/// persisted as `discoveryState.authorizationServerUrl` so refresh / revocation
/// can locate the AS later (xaa.ts:320-328).
#[derive(Debug, Clone)]
pub struct XaaResult {
    /// The minted token set.
    pub tokens: XaaTokenResult,
    /// AS issuer discovered via PRM (persist for later refresh/revoke).
    pub authorization_server_url: String,
    /// Exact native issuer text for the saved registration binding.
    pub authorization_server_source: Utf16JsonProjection,
}

/// Inputs for the AS jwt-bearer leg.
pub struct JwtBearerRequest<'a> {
    /// AS token endpoint.
    pub token_endpoint: &'a str,
    /// The ID-JAG assertion (from the IdP exchange).
    pub assertion: &'a str,
    /// AS-registered confidential client id.
    pub client_id: &'a str,
    /// AS-registered confidential client secret.
    pub client_secret: &'a str,
    /// Client-auth method (defaults to `Basic` per SEP-990 conformance).
    pub auth_method: AuthMethod,
    /// Compatibility field; the current native XAA grant omits scope.
    pub scope: Option<&'a str>,
}

/// RFC 7523 JWT Bearer Grant at the AS: ID-JAG → access_token (xaa.ts:337-394).
///
/// # Errors
/// [`XaaError::JwtBearer`] on transport failure, non-2xx, non-JSON, or a body
/// missing `access_token`.
pub async fn exchange_jwt_auth_grant(
    http: &Arc<dyn HttpTransport>,
    req: &JwtBearerRequest<'_>,
) -> Result<XaaTokenResult, XaaError> {
    exchange_jwt_auth_grant_with_source(http, req, None).await
}

/// Source-aware AS client secret: basic preserves encodeURIComponent's
/// rejection of unpaired units; post uses the native URLSearchParams USV view.
pub async fn exchange_jwt_auth_grant_with_source(
    http: &Arc<dyn HttpTransport>,
    req: &JwtBearerRequest<'_>,
    client_secret_source: Option<&Utf16JsonProjection>,
) -> Result<XaaTokenResult, XaaError> {
    let source_units = client_secret_source.and_then(|source| source.string_units(""));
    let source_secret = source_units.as_deref().map(String::from_utf16_lossy);
    let client_secret = source_secret.as_deref().unwrap_or(req.client_secret);
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", JWT_BEARER_GRANT),
        ("assertion", req.assertion),
    ];

    let mut headers: Vec<(String, String)> = vec![(
        "content-type".into(),
        "application/x-www-form-urlencoded".into(),
    )];
    match req.auth_method {
        AuthMethod::ClientSecretBasic => {
            let secret = match source_units.as_deref() {
                Some(units) => String::from_utf16(units).map_err(|_| {
                    XaaError::JwtBearer("String contained an illegal UTF-16 sequence.".into())
                })?,
                None => req.client_secret.to_owned(),
            };
            let basic = format!(
                "{}:{}",
                encode_uri_component(req.client_id),
                encode_uri_component(&secret)
            );
            headers.push((
                "authorization".into(),
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(basic)
                ),
            ));
        }
        AuthMethod::ClientSecretPost => {
            form.push(("client_id", req.client_id));
            form.push(("client_secret", client_secret));
        }
    }

    let http_req = HttpRequest {
        method: HttpMethod::Post,
        url: req.token_endpoint.to_string(),
        headers,
        body: Some(form_encode(&form)),
        body_bytes: None,
        timeout: Some(XAA_REQUEST_TIMEOUT),
    };
    let resp = http.request(http_req).await.map_err(|e| {
        XaaError::JwtBearer(format!("XAA: jwt-bearer grant failed: transport: {e}").into())
    })?;
    if !(200..300).contains(&resp.status) {
        return Err(XaaError::JwtBearer(sliced_error(
            &format!("XAA: jwt-bearer grant failed: HTTP {}: ", resp.status),
            &resp.body,
        )));
    }
    if Utf16JsonProjection::parse(&resp.body).is_err() {
        return Err(XaaError::JwtBearer(
            format!(
                "XAA: jwt-bearer grant returned non-JSON (captive portal?) at {}",
                req.token_endpoint
            )
            .into(),
        ));
    }
    let validated = crate::oauth::schemas::validate_xaa_tokens(&resp.body).map_err(|_| {
        XaaError::JwtBearer(
            format!(
                "XAA: jwt-bearer response did not match expected shape: {}",
                redact_tokens(&json_stringify(&resp.body))
            )
            .into(),
        )
    })?;
    let parsed = validated.projection();
    Ok(XaaTokenResult {
        access_token: parsed
            .string_at("/access_token")
            .expect("validated token string")
            .to_owned(),
        token_type: parsed
            .string_at("/token_type")
            .expect("validated/defaulted token type")
            .to_owned(),
        expires_in: parsed.number_at("/expires_in"),
        scope: parsed.string_at("/scope").map(str::to_owned),
        refresh_token: parsed.string_at("/refresh_token").map(str::to_owned),
        source: validated.into_projection(),
    })
}

// ─── Layer 3: Orchestrator ──────────────────────────────────────────────────

/// Config for the full XAA flow (xaa.ts `XaaConfig`, 402-415).
pub struct XaaConfig<'a> {
    /// Client id registered at the MCP server's authorization server.
    pub client_id: &'a str,
    /// Client secret for the MCP server's authorization server.
    pub client_secret: &'a str,
    /// Client id registered at the IdP (for the token-exchange request).
    pub idp_client_id: &'a str,
    /// Optional IdP client secret (`client_secret_post`).
    pub idp_client_secret: Option<&'a str>,
    /// The user's OIDC `id_token` from the IdP login.
    pub idp_id_token: &'a str,
    /// IdP token endpoint (where to send the RFC 8693 token-exchange).
    pub idp_token_endpoint: &'a str,
}

/// Full XAA flow: PRM → AS metadata → token-exchange → jwt-bearer →
/// access_token (xaa.ts `performCrossAppAccess`, 426-511).
///
/// Tries each PRM-advertised AS in order; `grant_types_supported` is optional
/// per RFC 8414 §2, so an AS is skipped only if it advertises a list that omits
/// jwt-bearer. The AS auth method is chosen from
/// `token_endpoint_auth_methods_supported` (basic unless only post is offered).
///
/// # Errors
/// Any [`XaaError`] from the constituent legs, or [`XaaError::NoAuthServer`] if
/// no advertised AS supports the jwt-bearer grant.
pub async fn perform_cross_app_access(
    http: &Arc<dyn HttpTransport>,
    server_url: &str,
    config: &XaaConfig<'_>,
) -> Result<XaaResult, XaaError> {
    perform_cross_app_access_with_issuer(http, server_url, config, None).await
}

/// XAA access bound to the AS used for the saved client registration. A PRM
/// issuer change cannot send the saved confidential client secret to a new AS.
pub async fn perform_cross_app_access_with_issuer(
    http: &Arc<dyn HttpTransport>,
    server_url: &str,
    config: &XaaConfig<'_>,
    pinned_issuer: Option<&str>,
) -> Result<XaaResult, XaaError> {
    perform_cross_app_access_with_source(http, server_url, config, pinned_issuer, None).await
}

/// XAA with exact saved client-secret text supplied independently of the
/// provider-neutral compatibility String configuration.
pub async fn perform_cross_app_access_with_source(
    http: &Arc<dyn HttpTransport>,
    server_url: &str,
    config: &XaaConfig<'_>,
    pinned_issuer: Option<&str>,
    client_secret_source: Option<&Utf16JsonProjection>,
) -> Result<XaaResult, XaaError> {
    let prm = discover_protected_resource(http, server_url).await?;

    // Try each advertised AS in order.
    let mut as_meta: Option<(AuthorizationServerMetadata, Utf16JsonProjection)> = None;
    let mut as_errors: Vec<String> = Vec::new();
    let candidates = prm
        .authorization_servers
        .iter()
        .filter(|issuer| {
            pinned_issuer.is_none_or(|pinned| normalize_url(issuer) == normalize_url(pinned))
        })
        .collect::<Vec<_>>();
    if pinned_issuer.is_some() && candidates.is_empty() {
        return Err(XaaError::AsMetadata(format!("authorization server changed: signed in with {}, server now names [{}]. Remove this MCP server and add it again with its client secret.", pinned_issuer.unwrap_or_default(), prm.authorization_servers.join(", "))));
    }
    for as_url in candidates {
        let (candidate, issuer_source) =
            match discover_authorization_server_source(http, as_url).await {
                Ok(c) => c,
                Err(e) => {
                    as_errors.push(format!("{as_url}: {e}"));
                    continue;
                }
            };
        if let Some(grants) = &candidate.grant_types_supported {
            if !grants.iter().any(|g| g == JWT_BEARER_GRANT) {
                as_errors.push(format!(
                    "{as_url}: does not advertise jwt-bearer grant (supported: {})",
                    grants.join(", ")
                ));
                continue;
            }
        }
        as_meta = Some((candidate, issuer_source));
        break;
    }
    let Some((as_meta, issuer_source)) = as_meta else {
        return Err(XaaError::NoAuthServer(as_errors.join("; ")));
    };

    // Pick auth method from what the AS advertises (xaa.ts:472-481).
    let auth_method = match &as_meta.token_endpoint_auth_methods_supported {
        Some(m)
            if !m.iter().any(|s| s == "client_secret_basic")
                && m.iter().any(|s| s == "client_secret_post") =>
        {
            AuthMethod::ClientSecretPost
        }
        _ => AuthMethod::ClientSecretBasic,
    };

    // IdP token-exchange: id_token → ID-JAG.
    let jag = request_jwt_authorization_grant(
        http,
        &JwtAuthGrantRequest {
            token_endpoint: config.idp_token_endpoint,
            audience: &as_meta.issuer,
            resource: &prm.resource,
            id_token: config.idp_id_token,
            client_id: config.idp_client_id,
            client_secret: config.idp_client_secret,
            scope: None,
        },
    )
    .await?;

    // AS jwt-bearer: ID-JAG → access_token.
    let tokens = exchange_jwt_auth_grant_with_source(
        http,
        &JwtBearerRequest {
            token_endpoint: &as_meta.token_endpoint,
            assertion: &jag.jwt_auth_grant,
            client_id: config.client_id,
            client_secret: config.client_secret,
            auth_method,
            scope: None,
        },
        client_secret_source,
    )
    .await?;

    Ok(XaaResult {
        tokens,
        authorization_server_url: as_meta.issuer,
        authorization_server_source: issuer_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_url_lowercases_drops_default_port_and_trailing_slash() {
        assert_eq!(
            normalize_url("HTTPS://AS.Example.COM:443/Path/"),
            "https://as.example.com/Path"
        );
        assert_eq!(
            normalize_url("https://as.example.com/"),
            "https://as.example.com"
        );
        assert_eq!(
            normalize_url("https://as.example.com:8443"),
            "https://as.example.com:8443"
        );
        assert_eq!(
            normalize_url("https://as.example.com/path//"),
            "https://as.example.com/path/",
            "only the final slash is removed"
        );
    }

    #[test]
    fn redact_tokens_masks_known_keys_only() {
        let raw =
            r#"{"error":"bad","subject_token":"secret-jwt","scope":"a b","client_secret":"shh"}"#;
        let red = redact_tokens(raw);
        assert!(red.contains(r#""subject_token":"[REDACTED]""#), "{red}");
        assert!(red.contains(r#""client_secret":"[REDACTED]""#), "{red}");
        // Non-sensitive keys survive verbatim.
        assert!(red.contains(r#""error":"bad""#));
        assert!(red.contains(r#""scope":"a b""#));
        assert!(!red.contains("secret-jwt"));
    }
}
