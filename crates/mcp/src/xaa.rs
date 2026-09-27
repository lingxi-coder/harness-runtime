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
//! **Residual (the IdP-login surface, NOT ported — see `oauth.rs` module
//! docs):** `getXaaIdpSettings`/`acquireIdpIdToken` (the OIDC browser pop),
//! `discoverOidc`, the keychain `id_token` cache, the `xaaRefresh` silent path,
//! the AS `client_secret` config seam, and analytics. The exchange engine here
//! is driven with a supplied `id_token` + AS `client_secret` (the
//! conformance-style path), which is what `performMCPXaaAuth` calls once those
//! inputs are gathered.
//!
//! The doc comments here lean on dense OAuth/OIDC vocabulary (IdP, OIDC,
//! ID-JAG, the RFC token-type URNs); backticking every term hurts readability,
//! so this module opts out of `clippy::doc_markdown` (matching the
//! established pattern in `tui/src/render/*`).
#![allow(clippy::doc_markdown)]

use base64::Engine;
use platform_api::HttpTransport;
use protocol::{HttpMethod, HttpRequest};
use std::sync::Arc;
use std::time::Duration;

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
    #[error("XAA: token exchange failed: {message}")]
    TokenExchange {
        /// Human-readable, token-redacted error detail.
        message: String,
        /// Whether the caller should drop the cached `id_token`.
        should_clear_id_token: bool,
    },
    /// The AS jwt-bearer (ID-JAG → access_token) leg failed.
    #[error("XAA: jwt-bearer grant failed: {0}")]
    JwtBearer(String),
    /// No advertised authorization server supports the jwt-bearer grant.
    #[error("XAA: no authorization server supports jwt-bearer. Tried: {0}")]
    NoAuthServer(String),
}

impl XaaError {
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

impl From<XaaError> for platform_api::McpError {
    fn from(e: XaaError) -> Self {
        platform_api::McpError::OAuth(e.to_string())
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
// Token redaction (xaa.ts:86-97). Hand-rolled to avoid a regex dep.
// ---------------------------------------------------------------------------

/// Redact token-bearing values from a (possibly raw) error body before logging,
/// matching the keys xaa.ts's `SENSITIVE_TOKEN_RE` redacts. A misbehaving AS
/// that echoes `subject_token`/`assertion`/`client_secret` in a 4xx envelope
/// must not leak into debug logs.
#[must_use]
pub fn redact_tokens(raw: &str) -> String {
    const KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "id_token",
        "assertion",
        "subject_token",
        "client_secret",
    ];
    let mut out = raw.to_string();
    for key in KEYS {
        let needle = format!("\"{key}\"");
        let replacement = format!("\"{key}\":\"[REDACTED]\"");
        // Replace every `"key"<ws>:<ws>"<value>"` → `"key":"[REDACTED]"`.
        // Re-scan from `search_from` after each replacement so repeats are
        // caught and the loop always makes forward progress.
        let mut search_from = 0usize;
        while let Some(rel) = out[search_from..].find(&needle) {
            let kpos = search_from + rel;
            let after = kpos + needle.len();
            let bytes = out.as_bytes();
            // Skip whitespace, expect `:`, skip whitespace, expect `"`.
            let mut i = after;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b':' {
                search_from = after;
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'"' {
                search_from = after;
                continue;
            }
            let val_start = i; // opening quote
            let Some(rel_end) = out[val_start + 1..].find('"') else {
                break;
            };
            let val_end = val_start + 1 + rel_end + 1; // past closing quote
            out.replace_range(kpos..val_end, &replacement);
            search_from = kpos + replacement.len();
        }
    }
    out
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

/// GET a `.well-known` document and parse it as JSON.
async fn get_json<T: serde::de::DeserializeOwned>(
    http: &Arc<dyn HttpTransport>,
    url: &str,
) -> Result<T, String> {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: vec![("accept".into(), "application/json".into())],
        body: None,
        body_bytes: None,
        timeout: Some(XAA_REQUEST_TIMEOUT),
    };
    let resp = http
        .request(req)
        .await
        .map_err(|e| format!("transport: {e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("HTTP {} fetching {url}", resp.status));
    }
    serde_json::from_str(&resp.body).map_err(|e| format!("decode {url}: {e}"))
}

/// Insert the well-known segment after the host (RFC 8414 §3 / RFC 9728).
fn well_known(base: &str, suffix: &str) -> String {
    match base.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            let path = path.trim_end_matches('/');
            if path.is_empty() {
                format!("{scheme}://{host}/.well-known/{suffix}")
            } else {
                format!("{scheme}://{host}/.well-known/{suffix}/{path}")
            }
        }
        None => format!("{base}/.well-known/{suffix}"),
    }
}

fn form_encode(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
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

/// RFC 9728 PRM discovery + §3.3 resource-mismatch validation (xaa.ts:135-165).
///
/// # Errors
/// [`XaaError::Prm`] on transport failure, a missing `resource`/
/// `authorization_servers`, or a resource that does not match `server_url`.
pub async fn discover_protected_resource(
    http: &Arc<dyn HttpTransport>,
    server_url: &str,
) -> Result<ProtectedResourceMetadata, XaaError> {
    let url = well_known(server_url, "oauth-protected-resource");
    let prm: ProtectedResourceMetadata = get_json(http, &url).await.map_err(XaaError::Prm)?;
    if prm.resource.is_empty() || prm.authorization_servers.first().is_none() {
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
    let url = well_known(as_url, "oauth-authorization-server");
    let meta: AuthorizationServerMetadata = get_json(http, &url)
        .await
        .map_err(|_| XaaError::AsMetadata(format!("no valid metadata at {as_url}")))?;
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
    Ok(meta)
}

// ─── Layer 2: Exchange ──────────────────────────────────────────────────────

/// Result of the IdP token exchange: the ID-JAG plus optional metadata.
#[derive(Debug, Clone)]
pub struct JwtAuthGrantResult {
    /// The ID-JAG (Identity Assertion Authorization Grant) JWT.
    pub jwt_auth_grant: String,
    /// Server-reported lifetime in seconds, if any.
    pub expires_in: Option<u64>,
    /// Granted scope, if any.
    pub scope: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct TokenExchangeResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    issued_token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
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
    /// Optional requested scope.
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
    if let Some(secret) = req.client_secret {
        form.push(("client_secret", secret));
    }
    if let Some(scope) = req.scope {
        form.push(("scope", scope));
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
            message: format!("transport: {e}"),
            // Network/transport failure (captive portal etc.) — keep id_token.
            should_clear_id_token: false,
        })?;

    if !(200..300).contains(&resp.status) {
        let body = redact_tokens(&resp.body);
        // Truncate to <=200 bytes on a UTF-8 char boundary (JS `.slice(0,200)`
        // is panic-free; a raw byte slice would panic mid-codepoint).
        let cut = (0..=body.len().min(200))
            .rev()
            .find(|&i| body.is_char_boundary(i))
            .unwrap_or(0);
        let body = &body[..cut];
        // 4xx → id_token rejected, clear; 5xx → IdP outage, keep (xaa.ts:267-273).
        let should_clear = resp.status < 500;
        return Err(XaaError::TokenExchange {
            message: format!("HTTP {}: {body}", resp.status),
            should_clear_id_token: should_clear,
        });
    }

    let parsed: TokenExchangeResponse =
        serde_json::from_str(&resp.body).map_err(|_| XaaError::TokenExchange {
            // 200 but structurally invalid — protocol violation, clear.
            message: format!(
                "token exchange response did not match expected shape: {}",
                redact_tokens(&resp.body)
            ),
            should_clear_id_token: true,
        })?;

    let Some(access_token) = parsed.access_token else {
        return Err(XaaError::TokenExchange {
            message: "token exchange response missing access_token".into(),
            should_clear_id_token: true,
        });
    };
    if parsed.issued_token_type.as_deref() != Some(ID_JAG_TOKEN_TYPE) {
        return Err(XaaError::TokenExchange {
            message: format!(
                "token exchange returned unexpected issued_token_type: {:?}",
                parsed.issued_token_type
            ),
            should_clear_id_token: true,
        });
    }
    Ok(JwtAuthGrantResult {
        jwt_auth_grant: access_token,
        expires_in: parsed.expires_in,
        scope: parsed.scope,
    })
}

/// The XAA access-token result (jwt-bearer grant output).
#[derive(Debug, Clone)]
pub struct XaaTokenResult {
    /// The MCP access token.
    pub access_token: String,
    /// Token type (defaults to `Bearer` when the AS omits it).
    pub token_type: String,
    /// Lifetime in seconds, if any.
    pub expires_in: Option<u64>,
    /// Granted scope, if any.
    pub scope: Option<String>,
    /// Refresh token, if the AS issued one.
    pub refresh_token: Option<String>,
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
}

#[derive(Debug, serde::Deserialize)]
struct JwtBearerResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
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
    /// Optional requested scope.
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
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", JWT_BEARER_GRANT),
        ("assertion", req.assertion),
    ];
    if let Some(scope) = req.scope {
        form.push(("scope", scope));
    }

    let mut headers: Vec<(String, String)> = vec![(
        "content-type".into(),
        "application/x-www-form-urlencoded".into(),
    )];
    match req.auth_method {
        AuthMethod::ClientSecretBasic => {
            let basic = format!(
                "{}:{}",
                urlencoding::encode(req.client_id),
                urlencoding::encode(req.client_secret)
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
            form.push(("client_secret", req.client_secret));
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
    let resp = http
        .request(http_req)
        .await
        .map_err(|e| XaaError::JwtBearer(format!("transport: {e}")))?;
    if !(200..300).contains(&resp.status) {
        let body = redact_tokens(&resp.body);
        // Truncate to <=200 bytes on a UTF-8 char boundary (JS `.slice(0,200)`
        // is panic-free; a raw byte slice would panic mid-codepoint).
        let cut = (0..=body.len().min(200))
            .rev()
            .find(|&i| body.is_char_boundary(i))
            .unwrap_or(0);
        let body = &body[..cut];
        return Err(XaaError::JwtBearer(format!("HTTP {}: {body}", resp.status)));
    }
    let parsed: JwtBearerResponse = serde_json::from_str(&resp.body).map_err(|_| {
        XaaError::JwtBearer(format!(
            "response did not match expected shape: {}",
            redact_tokens(&resp.body)
        ))
    })?;
    if parsed.access_token.is_empty() {
        return Err(XaaError::JwtBearer("response missing access_token".into()));
    }
    Ok(XaaTokenResult {
        access_token: parsed.access_token,
        // Many ASes omit token_type since Bearer is the only value (RFC 6750).
        token_type: parsed.token_type.unwrap_or_else(|| "Bearer".into()),
        expires_in: parsed.expires_in,
        scope: parsed.scope,
        refresh_token: parsed.refresh_token,
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
    let prm = discover_protected_resource(http, server_url).await?;

    // Try each advertised AS in order.
    let mut as_meta: Option<AuthorizationServerMetadata> = None;
    let mut as_errors: Vec<String> = Vec::new();
    for as_url in &prm.authorization_servers {
        let candidate = match discover_authorization_server(http, as_url).await {
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
        as_meta = Some(candidate);
        break;
    }
    let Some(as_meta) = as_meta else {
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
    let tokens = exchange_jwt_auth_grant(
        http,
        &JwtBearerRequest {
            token_endpoint: &as_meta.token_endpoint,
            assertion: &jag.jwt_auth_grant,
            client_id: config.client_id,
            client_secret: config.client_secret,
            auth_method,
            scope: None,
        },
    )
    .await?;

    Ok(XaaResult {
        tokens,
        authorization_server_url: as_meta.issuer,
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
