use super::diagnostics::{
    emit_oauth_flow_failure, emit_xaa_oauth_flow_success, oauth_refresh_failure_reason,
    xaa_flow_failure_stage, xaa_provider_failure_stage,
};
use super::{GrantProvenance, McpRegistry, OAuthDeps};
use crate::connection::{McpConnectionState, McpServerConfig};
use crate::oauth;
use platform_api::{McpError, McpTransportSpec};
use std::time::{Duration, SystemTime};

/// Borrow the `oauth` config block of an SSE/HTTP spec, if present. Other
/// transports (stdio, websocket, …) never carry OAuth → `None`.
pub(super) fn spec_oauth(spec: &McpTransportSpec) -> Option<&platform_api::McpOAuthConfigDto> {
    match spec {
        McpTransportSpec::Sse { oauth, .. } | McpTransportSpec::Http { oauth, .. } => {
            oauth.as_ref()
        }
        _ => None,
    }
}

/// Endpoint URL of an SSE/HTTP spec (used as the OAuth `server_url` for
/// discovery and the `getServerKey` hash). Empty for non-remote specs.
pub(super) fn spec_url(spec: &McpTransportSpec) -> &str {
    match spec {
        McpTransportSpec::Sse { url, .. } | McpTransportSpec::Http { url, .. } => url,
        _ => "",
    }
}

/// Clone `spec` with `Authorization: Bearer <token>` set in its headers map.
/// Only SSE/HTTP specs carry headers; other variants are returned unchanged.
pub(super) fn inject_bearer(spec: &McpTransportSpec, access_token: &str) -> McpTransportSpec {
    let bearer = format!("Bearer {access_token}");
    match spec.clone() {
        McpTransportSpec::Sse {
            url,
            mut headers,
            headers_helper,
            oauth,
        } => {
            headers.insert("Authorization".into(), bearer);
            McpTransportSpec::Sse {
                url,
                headers,
                headers_helper,
                oauth,
            }
        }
        McpTransportSpec::Http {
            url,
            mut headers,
            headers_helper,
            oauth,
        } => {
            headers.insert("Authorization".into(), bearer);
            McpTransportSpec::Http {
                url,
                headers,
                headers_helper,
                oauth,
            }
        }
        other => other,
    }
}

/// Faithful-core 401 detection. The error reaching here is always the result
/// of `connect_attempt` (transport `connect` + `initialize`): SSE's pre-flight
/// GET returns `McpError::HttpResponse` directly on a non-2xx status
/// (`platforms/common/src/mcp_sse.rs`), and Streamable HTTP's `initialize`
/// unwraps the same shape from a synthetic JSON-RPC error's structured
/// `data: {httpStatus, wwwAuthenticate}` (`handshake_error`,
/// `platforms/posix/src/mcp.rs` — mirrored on any platform that wires a real
/// HTTP transport). The substring match on `"401"` remains as a fallback for
/// any OTHER path that still flattens to a string (e.g. a raw connection
/// failure whose message happens to mention a status code) — it is not
/// expected to be the primary match for a real 401 any more.
pub(super) fn error_is_401(e: &McpError) -> bool {
    matches!(e, McpError::HttpResponse { status: 401, .. })
        || matches!(
            e,
            McpError::Connection(m) | McpError::Handshake(m) if m.contains("401")
        )
}

pub(crate) fn error_is_auth_response(error: &McpError) -> bool {
    error_is_401(error)
        || matches!(error, McpError::HttpResponse { status: 403, .. })
        || matches!(
            error,
            McpError::Connection(message) | McpError::Handshake(message)
                if message.contains("403")
        )
}

/// Faithful-core 403 `insufficient_scope` step-up detection (auth.ts
/// `wrapFetchWithStepUpDetection`, 1354-1374). The primary path is now
/// structural: `connect_attempt`'s error carries a genuine `www_authenticate`
/// header value in `McpError::HttpResponse` (see `error_is_401`'s note — SSE's
/// pre-flight GET, and Streamable HTTP's `initialize` via `handshake_error`),
/// so `www_authenticate.contains("insufficient_scope")` and
/// `extract_scope_from_www_auth` run against the real header text. The
/// `Connection`/`Handshake` string arms remain as a substring-matched
/// fallback (`"403"` + `"insufficient_scope"`, extracting a `scope="…"`/
/// `scope=…` token per RFC 6750 §3 — the same shape as the SDK's
/// `extractFieldFromWwwAuth`) for any error shape that still flattens to a
/// string. Returns the elevated scope when present.
///
/// A tool-call-time 403 (post-connect, i.e. `McpClient::call_tool_with_progress`
/// rather than `connect_attempt`) is a SEPARATE path: `mcp/src/client.rs`'s
/// `mcp_client_error_from_rpc` already reconstructs a structured
/// `McpClientError::HttpResponse` from the same
/// `MCP_HTTP_STATUS=…;WWW_AUTHENTICATE=…` marker, consumed by
/// `call_tool_with_auth_retry`'s `is_auth_response()` check — this function
/// is never called on that path, so it is out of scope here.
pub(super) fn error_is_403_insufficient_scope(e: &McpError) -> Option<String> {
    if let McpError::HttpResponse {
        status: 403,
        www_authenticate: Some(value),
    } = e
    {
        return value
            .contains("insufficient_scope")
            .then(|| extract_scope_from_www_auth(value))
            .flatten();
    }
    let (McpError::Connection(msg) | McpError::Handshake(msg)) = e else {
        return None;
    };
    if !(msg.contains("403") && msg.contains("insufficient_scope")) {
        return None;
    }
    extract_scope_from_www_auth(msg)
}

/// Extract a `resource_metadata` challenge param (RFC 9728) from a connect
/// failure's `WWW-Authenticate` header, for both the 401-reauth and 403
/// step-up call sites (§24c: oracle `Be`/`H0e`, threaded into `discover_
/// auth_server_metadata` so a server that publishes its Protected Resource
/// Metadata somewhere other than the well-known guess still resolves).
/// Structural only: `McpError::HttpResponse` (the shape `connect_attempt`'s
/// error now carries — see `error_is_401`'s note) is the sole source; the
/// string-flattened `Connection`/`Handshake` fallback shapes elsewhere in this
/// file don't carry a real header to reparse, so they yield `None` here and
/// discovery falls back to its well-known guess, exactly as before this
/// finding.
pub(super) fn error_resource_metadata_url(e: &McpError) -> Option<String> {
    let McpError::HttpResponse {
        www_authenticate: Some(value),
        ..
    } = e
    else {
        return None;
    };
    oauth::parse_www_authenticate_challenge(value).resource_metadata_url
}

/// Hand-rolled equivalent of `wwwAuth.match(/scope=(?:"([^"]+)"|([^\s,]+))/)`
/// (auth.ts:1365). Finds the first `scope=` and returns its value, honoring an
/// optional double-quoted form; an unquoted value runs to the first whitespace
/// or comma. Avoids a `regex` dependency.
pub(super) fn extract_scope_from_www_auth(s: &str) -> Option<String> {
    let idx = s.find("scope=")?;
    let rest = &s[idx + "scope=".len()..];
    if let Some(after_quote) = rest.strip_prefix('"') {
        // Quoted: up to the next `"`.
        let end = after_quote.find('"')?;
        let scope = &after_quote[..end];
        return (!scope.is_empty()).then(|| scope.to_string());
    }
    // Unquoted: up to the first whitespace or comma.
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(rest.len());
    let scope = &rest[..end];
    (!scope.is_empty()).then(|| scope.to_string())
}

impl McpRegistry {
    /// Resolve the spec to connect with, attaching a Bearer token for OAuth
    /// servers. Returns `(spec, Some(server_key))` for an OAuth-configured
    /// SSE/HTTP server (token loaded → refreshed-on-expiry → freshly minted via
    /// the interactive flow), or `(config.spec.clone(), None)` for static-token
    /// servers and any server when the OAuth seam is unwired.
    ///
    /// Mirrors claude-code's per-connect token resolution (`auth.ts` `tokens()`
    /// + `useManageMCPConnections` attaching the Authorization header).
    pub(super) async fn resolve_oauth_spec(
        &self,
        config: &McpServerConfig,
    ) -> Result<(McpTransportSpec, Option<String>, Option<GrantProvenance>), McpError> {
        let Some(oauth_cfg) = spec_oauth(&config.spec) else {
            return Ok((config.spec.clone(), None, Some(GrantProvenance::unbound())));
        };
        let Some(deps) = &self.oauth else {
            return Ok((config.spec.clone(), None, None));
        };
        let key = oauth::server_key(&config.name, &config.spec);
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);

        // XAA (cross-app-access, SEP-990): when `oauth.xaa` is set, XAA is the
        // ONLY auth path — never fall through to the consent flow (auth.ts:857-
        // 900). Gated by `LINGXI_ENABLE_XAA` (mirror of CLAUDE_CODE_ENABLE_XAA);
        // a flagged server with the env unset hard-fails with actionable copy.
        if oauth_cfg.xaa == Some(true) {
            let token = self.resolve_xaa_token(config, &key, deps).await?;
            return Ok((
                inject_bearer(&config.spec, token.access_token.expose_secret()),
                Some(key),
                GrantProvenance::from_tokens(&token),
            ));
        }

        // 1. Stored token, unexpired → use it.
        // 2. Stored token, expired with a refresh token → refresh, persist.
        // 3. No usable token → run the interactive flow, persist.
        let token = match oauth::load_tokens(&deps.storage, &key).await? {
            // Unexpired stored token → use it directly.
            Some(stored) if deps.clock.now() < stored.expires_at() => stored.into_tokens(),
            // Expired stored token.
            Some(stored) => {
                match stored.refresh_token.clone() {
                    Some(refresh) => {
                        // Proactive (pre-connect) refresh: no live challenge
                        // exists yet, so discovery uses the well-known guess.
                        let meta = match oauth::discover_auth_server_metadata(
                            &deps.http,
                            spec_url(&config.spec),
                            oauth_cfg.auth_server_metadata_url.as_deref(),
                            None,
                        )
                        .await
                        {
                            Ok(meta) => meta,
                            Err(error) => {
                                oauth::emit_oauth_refresh_failure(
                                    &telemetry_ctx,
                                    oauth_refresh_failure_reason(&error),
                                );
                                return Err(error.into());
                            }
                        };
                        // Prefer the client_id the stored tokens were minted
                        // with (DCR-issued OR configured) so silent refresh
                        // re-sends it; fall back to the configured id, then ""
                        // (auth.ts clientInformation(), 1482-1506).
                        let client_id = stored
                            .client_id
                            .clone()
                            .or_else(|| oauth_cfg.client_id.clone())
                            .unwrap_or_default();
                        let refreshed = match oauth::refresh_tokens(
                            &deps.http,
                            &deps.clock,
                            &meta,
                            &client_id,
                            None, // public client — no confidential secret to send
                            &refresh,
                        )
                        .await
                        {
                            Ok(refreshed) => refreshed,
                            Err(error) => {
                                oauth::emit_oauth_refresh_failure(
                                    &telemetry_ctx,
                                    oauth_refresh_failure_reason(&error),
                                );
                                return Err(error.into());
                            }
                        };
                        oauth::save_tokens_with_telemetry(
                            &deps.storage,
                            &deps.clock,
                            &key,
                            &refreshed,
                            Some(&telemetry_ctx),
                        )
                        .await?;
                        oauth::emit_oauth_refresh_success(&telemetry_ctx);
                        refreshed
                    }
                    None => {
                        self.run_interactive_oauth(config, oauth_cfg, &key, deps, None, None)
                            .await?
                    }
                }
            }
            None => {
                self.run_interactive_oauth(config, oauth_cfg, &key, deps, None, None)
                    .await?
            }
        };

        Ok((
            inject_bearer(&config.spec, token.access_token.expose_secret()),
            Some(key),
            GrantProvenance::from_tokens(&token),
        ))
    }
    /// Re-authenticate after a 401: refresh if a refresh token is stored, else
    /// run a fresh interactive flow, then return the spec with the new Bearer.
    /// `resource_metadata_url` (§24c) is the `resource_metadata` param parsed
    /// from the triggering 401's `WWW-Authenticate` challenge, when it carried
    /// a structured one — threaded into discovery in place of the well-known
    /// guess.
    pub(super) async fn reauth_oauth_spec(
        &self,
        config: &McpServerConfig,
        resource_metadata_url: Option<&str>,
    ) -> Result<(McpTransportSpec, Option<GrantProvenance>), McpError> {
        let deps = self
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::OAuth("oauth seam not wired".into()))?;
        let oauth_cfg = spec_oauth(&config.spec)
            .ok_or_else(|| McpError::OAuth("server has no oauth config".into()))?;
        let key = oauth::server_key(&config.name, &config.spec);
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);

        // §26b delta 3: an XAA-flagged server's 401 must stay on the XAA
        // path — never fall through to the refresh-or-interactive-consent
        // logic below, matching this module's own guarantee (see
        // `resolve_oauth_spec`) that XAA is the ONLY auth path. The server
        // just rejected whatever was cached, so `force_fresh` skips reusing
        // it: a stored refresh token drives the ordinary refresh grant; its
        // absence (or rejection) drives a fresh silent IdP+AS exchange.
        // Interactive consent is never reachable from this arm.
        if oauth_cfg.xaa == Some(true) {
            let token = self
                .resolve_xaa_token_inner(config, &key, deps, true, resource_metadata_url)
                .await?;
            return Ok((
                inject_bearer(&config.spec, token.access_token.expose_secret()),
                GrantProvenance::from_tokens(&token),
            ));
        }

        let stored = oauth::load_tokens(&deps.storage, &key).await?;
        let token = match stored.as_ref().and_then(|t| t.refresh_token.clone()) {
            Some(refresh) => {
                let meta = match oauth::discover_auth_server_metadata(
                    &deps.http,
                    spec_url(&config.spec),
                    oauth_cfg.auth_server_metadata_url.as_deref(),
                    resource_metadata_url,
                )
                .await
                {
                    Ok(meta) => meta,
                    Err(error) => {
                        oauth::emit_oauth_refresh_failure(
                            &telemetry_ctx,
                            oauth_refresh_failure_reason(&error),
                        );
                        return Err(error.into());
                    }
                };
                // Prefer the persisted (DCR-issued or configured) client_id so
                // refresh re-sends it (auth.ts clientInformation(), 1482-1506).
                let client_id = stored
                    .as_ref()
                    .and_then(|t| t.client_id.clone())
                    .or_else(|| oauth_cfg.client_id.clone())
                    .unwrap_or_default();
                match oauth::refresh_tokens(
                    &deps.http,
                    &deps.clock,
                    &meta,
                    &client_id,
                    None, // public client — no confidential secret to send
                    &refresh,
                )
                .await
                {
                    Ok(t) => {
                        oauth::save_tokens_with_telemetry(
                            &deps.storage,
                            &deps.clock,
                            &key,
                            &t,
                            Some(&telemetry_ctx),
                        )
                        .await?;
                        oauth::emit_oauth_refresh_success(&telemetry_ctx);
                        t
                    }
                    // Refresh token rejected → fall back to a fresh flow.
                    Err(oauth::OAuthError::RefreshRejected(_)) => {
                        oauth::emit_oauth_refresh_failure(&telemetry_ctx, "invalid_grant");
                        self.run_interactive_oauth(
                            config,
                            oauth_cfg,
                            &key,
                            deps,
                            None,
                            resource_metadata_url,
                        )
                        .await?
                    }
                    Err(e) => {
                        oauth::emit_oauth_refresh_failure(
                            &telemetry_ctx,
                            oauth_refresh_failure_reason(&e),
                        );
                        return Err(e.into());
                    }
                }
            }
            None => {
                self.run_interactive_oauth(
                    config,
                    oauth_cfg,
                    &key,
                    deps,
                    None,
                    resource_metadata_url,
                )
                .await?
            }
        };

        Ok((
            inject_bearer(&config.spec, token.access_token.expose_secret()),
            GrantProvenance::from_tokens(&token),
        ))
    }
    /// Step-up re-auth after a 403 `insufficient_scope`: persist the required
    /// `scope` onto the stored entry (auth.ts `markStepUpPending`/`stepUpScope`,
    /// 1896), then run a fresh interactive flow requesting that elevated scope
    /// and return the spec with the new Bearer. A refresh CANNOT elevate scope
    /// (RFC 6749 §6), so this always drives the PKCE flow. `resource_metadata_url`
    /// (§24c) is the `resource_metadata` param parsed from the SAME 403
    /// challenge that carried the elevated `scope`, when present (oracle
    /// `_stepUpAuthorize`: `if(e.resourceMetadataUrl)this._resourceMetadataUrl=
    /// e.resourceMetadataUrl`).
    pub(super) async fn step_up_oauth_spec(
        &self,
        config: &McpServerConfig,
        scope: &str,
        resource_metadata_url: Option<&str>,
    ) -> Result<(McpTransportSpec, Option<GrantProvenance>), McpError> {
        let deps = self
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::OAuth("oauth seam not wired".into()))?;
        let oauth_cfg = spec_oauth(&config.spec)
            .ok_or_else(|| McpError::OAuth("server has no oauth config".into()))?;
        let key = oauth::server_key(&config.name, &config.spec);
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);

        // §26b delta 3, second arm: an XAA-flagged server's 403 must stay on
        // the XAA path exactly as its 401 does (`reauth_oauth_spec` above).
        // Oracle `pEr` — the ONE entry point to the consent flow — opens with
        // `if(t.oauth?.xaa){ ...await gt(...); return }` (@182200767), so no
        // step-up, cached `stepUpScope`, or elevated-scope request can ever
        // reach `redirectToAuthorization` for an XAA server. Without this
        // guard the 403 branch WINS the race (it is evaluated before the 401
        // branch in `connect_locked_inner`) and binds a loopback listener +
        // opens a browser on an enterprise deployment that has no interactive
        // consent surface at all. The scope persist below is skipped for the
        // same reason: the oracle only writes `stepUpScope` from inside
        // `redirectToAuthorization`, which XAA never reaches.
        if oauth_cfg.xaa == Some(true) {
            let token = self
                .resolve_xaa_token_inner(config, &key, deps, true, resource_metadata_url)
                .await?;
            return Ok((
                inject_bearer(&config.spec, token.access_token.expose_secret()),
                GrantProvenance::from_tokens(&token),
            ));
        }

        // Persist the elevated scope so it survives even if the interactive flow
        // is interrupted and resumed later (auth.ts caches it on the stored
        // entry). Best-effort: a storage failure must not block the step-up.
        if let Ok(Some(mut stored)) = oauth::load_tokens(&deps.storage, &key).await {
            stored.step_up_scope = Some(scope.to_string());
            let _ = oauth::store_tokens_with_telemetry(
                &deps.storage,
                &deps.clock,
                &key,
                &stored,
                Some(&telemetry_ctx),
            )
            .await;
        }

        let token = self
            .run_interactive_oauth(
                config,
                oauth_cfg,
                &key,
                deps,
                Some(scope),
                resource_metadata_url,
            )
            .await?;
        Ok((
            inject_bearer(&config.spec, token.access_token.expose_secret()),
            GrantProvenance::from_tokens(&token),
        ))
    }
    /// Resolve an access token for an XAA-flagged server (auth.ts
    /// `performMCPXaaAuth`, 664-845). Thin wrapper over
    /// [`Self::resolve_xaa_token_inner`] for the per-connect (non-401) resolve
    /// path, which may reuse a cached token.
    pub(super) async fn resolve_xaa_token(
        &self,
        config: &McpServerConfig,
        key: &str,
        deps: &OAuthDeps,
    ) -> Result<oauth::Tokens, McpError> {
        self.resolve_xaa_token_inner(config, key, deps, false, None)
            .await
    }
    /// Core of the XAA resolve chain (oracle `tokens()`, @182213696), shared by
    /// the per-connect resolve ([`Self::resolve_xaa_token`], `force_fresh:
    /// false` — may reuse a cached token) and the post-401 re-auth
    /// ([`Self::reauth_oauth_spec`]'s xaa arm, `force_fresh: true` — the
    /// server just rejected whatever is cached, so the cache-hit branches
    /// below are skipped and a refresh/exchange is always attempted).
    /// `resource_metadata_url` (§24c) is the live 401 challenge's
    /// `resource_metadata` param, when any (`None` from the per-connect
    /// wrapper, which has no live challenge to read).
    ///
    /// Gating (auth.ts:871-876): `LINGXI_ENABLE_XAA` must be truthy or this
    /// hard-fails with actionable copy. Single-flight (§26b delta 2, oracle
    /// `_refreshInProgress`): callers for the SAME `key` serialize on
    /// [`Self::xaa_refresh_lock`], so at most one refresh/exchange runs at a
    /// time; a resolver that has to wait re-reads storage once it acquires the
    /// lock and reuses whatever the winner just persisted instead of
    /// re-exchanging.
    ///
    /// §26b delta 4: once a refresh token is on file, it ALWAYS takes the
    /// ordinary refresh route (`oauth::refresh_tokens`, whose confidential-
    /// client auth method is chosen from the AS's advertised
    /// `token_endpoint_auth_methods_supported`) — the full IdP+AS exchange
    /// ([`crate::xaa::perform_cross_app_access`]) is reserved for "no refresh
    /// token stored" (never had one, or the AS just rejected it, which falls
    /// through rather than opening an interactive flow — XAA is never
    /// interactive). §26b delta 1: absent a refresh token, the exchange is
    /// silent-triggered only when the access token is missing or expires
    /// within 300s (oracle `!n?.refreshToken && (!n?.accessToken ||
    /// (n.expiresAt-Date.now())/1000<=300)`); otherwise the cached access
    /// token is reused as-is. That same 300s window ALSO bounds reuse on the
    /// refresh-token arm (oracle `tokens()`'s `r<=300&&n.refreshToken`
    /// proactive refresh, which runs for every server after the XAA block).
    /// `force_fresh` skips both cache-hit checks.
    pub(super) async fn resolve_xaa_token_inner(
        &self,
        config: &McpServerConfig,
        key: &str,
        deps: &OAuthDeps,
        force_fresh: bool,
        resource_metadata_url: Option<&str>,
    ) -> Result<oauth::Tokens, McpError> {
        // Gate on the enable flag (mirror of CLAUDE_CODE_ENABLE_XAA).
        if !platform_api::env::is_env_truthy(std::env::var("LINGXI_ENABLE_XAA").ok().as_deref()) {
            return Err(McpError::OAuth(format!(
                "XAA is not enabled (set LINGXI_ENABLE_XAA=1). Remove 'xaa' from \
                 server '{}' to use the standard consent flow.",
                config.name
            )));
        }

        // Single-flight: serialize concurrent resolves for this server key.
        let lock = self.xaa_refresh_lock(key);
        let _single_flight = lock.lock().await;

        let stored = oauth::load_tokens(&deps.storage, key).await?;

        match stored {
            Some(s) => {
                if let Some(refresh) = s.refresh_token.clone() {
                    // Delta 4: a refresh token on file takes the ordinary
                    // refresh route — never the full exchange below.
                    //
                    // Reuse is bounded by the SAME 300s proactive window the
                    // no-refresh-token arm below uses: oracle `tokens()`
                    // (@182213696) runs `if(r!=null&&r<=300&&n.refreshToken
                    // &&!d){...refreshAuthorization(n.refreshToken)...}`
                    // AFTER the XAA block, so a stored refresh token does not
                    // exempt a token from proactive refresh — it is what
                    // makes proactive refresh possible. Reusing until hard
                    // expiry instead hands the transport a token seconds from
                    // death, buying an avoidable 401 + reauth round-trip (or
                    // a connect failure if it lapses mid-handshake).
                    let expiring_soon = s
                        .expires_at()
                        .duration_since(deps.clock.now())
                        .map(|remaining| remaining <= Duration::from_secs(300))
                        .unwrap_or(true);
                    if !force_fresh && !expiring_soon {
                        return Ok(s.into_tokens());
                    }
                    let meta = oauth::discover_auth_server_metadata(
                        &deps.http,
                        spec_url(&config.spec),
                        None,
                        resource_metadata_url,
                    )
                    .await?;
                    let client_id = s.client_id.clone().unwrap_or_default();
                    let client_secret = s.client_secret.clone();
                    match oauth::refresh_tokens(
                        &deps.http,
                        &deps.clock,
                        &meta,
                        &client_id,
                        client_secret.as_deref(),
                        &refresh,
                    )
                    .await
                    {
                        Ok(refreshed) => {
                            let stored_new = oauth::StoredTokens {
                                access_token: refreshed.access_token.expose_secret().clone(),
                                refresh_token: refreshed
                                    .refresh_token
                                    .as_ref()
                                    .map(|t| t.expose_secret().clone()),
                                expires_at_unix: refreshed
                                    .expires_at
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs(),
                                client_id: Some(client_id),
                                client_secret,
                                step_up_scope: None,
                            };
                            oauth::store_tokens(&deps.storage, &deps.clock, key, &stored_new)
                                .await
                                .map_err(McpError::from)?;
                            return Ok(stored_new.into_tokens());
                        }
                        // The refresh token itself is dead — fall through to
                        // the silent IdP+AS exchange below rather than a
                        // fresh interactive flow (XAA is never interactive).
                        Err(oauth::OAuthError::RefreshRejected(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                } else if !force_fresh {
                    // Delta 1: no refresh token — reuse the cached access
                    // token outright unless it is missing or expiring within
                    // 300s (`Err` from `duration_since` means already past).
                    let expiring_soon = s
                        .expires_at()
                        .duration_since(deps.clock.now())
                        .map(|remaining| remaining <= Duration::from_secs(300))
                        .unwrap_or(true);
                    if !expiring_soon {
                        return Ok(s.into_tokens());
                    }
                    tracing::debug!(
                        server = %config.name,
                        "XAA: access_token expiring, attempting silent exchange"
                    );
                }
            }
            None => {
                tracing::debug!(
                    server = %config.name,
                    "XAA: no access_token yet, attempting silent exchange"
                );
            }
        }

        // The IdP-login + AS-secret surface is supplied by the host provider.
        // Absent it, XAA cannot proceed — a clearly-noted residual seam.
        let provider = deps.xaa_config.as_ref().ok_or_else(|| {
            McpError::OAuth(format!(
                "XAA: no IdP connection configured for server '{}'. The XAA \
                 IdP-login/secret config surface (getXaaIdpSettings / \
                 acquireIdpIdToken / mcpOAuthClientConfig) is not yet wired \
                 (residual).",
                config.name
            ))
        })?;
        let server_url = spec_url(&config.spec);
        // The oracle performs this read before entering the failure-telemetry
        // try/catch. A storage failure therefore aborts the flow rather than
        // being mislabeled as an IdP cache miss.
        let id_token_cache_hit = provider
            .peek_id_token_cache_hit(&config.name, server_url)
            .await?;
        let inputs = match provider.xaa_inputs(&config.name, server_url).await {
            Ok(Some(inputs)) => inputs,
            Ok(None) => {
                emit_oauth_flow_failure(&telemetry::tengu::mcp::OAuthFlowFailurePayload {
                    auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
                    xaa_failure_stage: telemetry::Verified::assert_safe("idp_login".to_string()),
                    id_token_cache_hit,
                });
                return Err(McpError::OAuth(format!(
                    "XAA: server '{}' is not XAA-provisioned (no IdP/AS inputs).",
                    config.name
                )));
            }
            Err(error) => {
                emit_oauth_flow_failure(&telemetry::tengu::mcp::OAuthFlowFailurePayload {
                    auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
                    xaa_failure_stage: telemetry::Verified::assert_safe(
                        xaa_provider_failure_stage(&error).to_string(),
                    ),
                    id_token_cache_hit,
                });
                return Err(error);
            }
        };

        let result = match crate::xaa::perform_cross_app_access(
            &deps.http,
            spec_url(&config.spec),
            &crate::xaa::XaaConfig {
                client_id: &inputs.client_id,
                client_secret: &inputs.client_secret,
                idp_client_id: &inputs.idp_client_id,
                idp_client_secret: inputs.idp_client_secret.as_deref(),
                idp_id_token: &inputs.idp_id_token,
                idp_token_endpoint: &inputs.idp_token_endpoint,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(server = %config.name, "XAA silent exchange failed: {e}");
                emit_oauth_flow_failure(&telemetry::tengu::mcp::OAuthFlowFailurePayload {
                    auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
                    xaa_failure_stage: telemetry::Verified::assert_safe(
                        xaa_flow_failure_stage(&e).to_string(),
                    ),
                    id_token_cache_hit,
                });
                // 4xx token-exchange ⇒ the cached id_token was rejected; drop it
                // so the next resolve re-acquires (auth.ts:1840-1847
                // `clearIdpIdToken(idp.issuer)`). 5xx (IdP outage) keeps it.
                // Best-effort: a clear failure must not mask the exchange error.
                if e.should_clear_id_token() {
                    let _ = provider.clear_id_token().await;
                }
                return Err(e.into());
            }
        };

        // Persist: carry the AS confidential client_id/secret so refresh +
        // RFC-7009 revocation can authenticate the confidential client
        // (auth.ts:807-825 token-save). `expires_in` (when present) sets expiry.
        let expires_at = match result.tokens.expires_in {
            Some(secs) => deps.clock.now() + Duration::from_secs(secs),
            // No expiry advertised → treat as already-stale so the next connect
            // re-runs the exchange (XAA tokens are cheap to re-mint, silent).
            None => deps.clock.now(),
        };
        let stored = oauth::StoredTokens {
            access_token: result.tokens.access_token.clone(),
            refresh_token: result.tokens.refresh_token.clone(),
            expires_at_unix: expires_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            client_id: Some(inputs.client_id.clone()),
            client_secret: Some(inputs.client_secret.clone()),
            step_up_scope: None,
        };
        oauth::store_tokens(&deps.storage, &deps.clock, key, &stored)
            .await
            .map_err(McpError::from)?;
        emit_xaa_oauth_flow_success(&telemetry::tengu::mcp::OAuthXaaFlowSuccessPayload {
            auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
            id_token_cache_hit,
        });

        Ok(stored.into_tokens())
    }
    /// Drive the full interactive OAuth flow and persist the resulting tokens.
    ///
    /// `scope_override` carries an elevated scope cached from a prior 403
    /// `insufficient_scope` step-up (auth.ts `cachedStepUpScope`); when set the
    /// authorize URL requests it instead of the advertised scope. On a
    /// successful grant the persisted `step_up_scope` is cleared (auth.ts:1705).
    /// `resource_metadata_url` (§24c) carries a `resource_metadata` challenge
    /// param from the live 401/403 that triggered this flow (`None` for the
    /// proactive, no-challenge callers — a fresh server with no stored token,
    /// or a stale-token silent refresh that hasn't hit the wire yet).
    pub(super) async fn run_interactive_oauth(
        &self,
        config: &McpServerConfig,
        oauth_cfg: &platform_api::McpOAuthConfigDto,
        key: &str,
        deps: &OAuthDeps,
        scope_override: Option<&str>,
        resource_metadata_url: Option<&str>,
    ) -> Result<oauth::Tokens, McpError> {
        // Effective elevated scope: an explicit override (the 403 step-up path)
        // wins; otherwise honor any `step_up_scope` cached on the stored entry
        // from a previous 403 `insufficient_scope` (auth.ts:906-909
        // `cachedStepUpScope`). The stored blob is about to be overwritten by the
        // fresh grant, so we read it before driving the flow.
        let pinned_scope = oauth_cfg
            .scopes
            .as_deref()
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_string);
        let cached_scope = match (pinned_scope, scope_override) {
            (Some(scope), _) => Some(scope),
            (None, Some(scope)) => Some(scope.to_string()),
            (None, None) => oauth::load_tokens(&deps.storage, key)
                .await
                .ok()
                .flatten()
                .and_then(|t| t.step_up_scope),
        };

        // Surface `AwaitingOAuth` while the user completes the browser flow.
        let callback_port = oauth_cfg.callback_port.unwrap_or(0);
        self.connections.write().await.insert(
            config.name.clone(),
            McpConnectionState::AwaitingOAuth {
                config: config.clone(),
                callback_port,
            },
        );
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
        let tokens = oauth::perform_oauth_flow_for_reauth(
            &deps.http,
            &deps.clock,
            oauth_cfg,
            &config.name,
            spec_url(&config.spec),
            &deps.on_authorization_url,
            Some(&telemetry_ctx),
            cached_scope.as_deref(),
            resource_metadata_url,
        )
        .await?;
        // A fresh grant clears any pending step-up scope (auth.ts:1705): the new
        // tokens carry the elevated scope, so the cache must not linger.
        oauth::save_tokens_with_telemetry(
            &deps.storage,
            &deps.clock,
            key,
            &tokens,
            Some(&telemetry_ctx),
        )
        .await?;
        Ok(tokens)
    }
}
