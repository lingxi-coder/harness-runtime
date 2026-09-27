//! GitHub Copilot device-flow login (RFC 8628) over an injected JSON-POST seam.
//!
//! The crate owns no HTTP client (transport is always injected). [`CopilotLogin`]
//! holds the pure begin/poll state machine + backoff math; the host implements
//! [`CopilotHttp`] and drives the sleep/retry loop and any UI.

use serde_json::{json, Value};

use crate::copilot::auth::CopilotSecret;
use crate::transport::BoxFuture;
use crate::LlmError;

/// GitHub OAuth client id for the VS Code Copilot extension. This is the
/// DEFAULT because GitHub's Copilot token-exchange endpoint only authorizes
/// tokens minted by the VS Code Copilot OAuth client. Override via
/// [`copilot_client_id`] / `LINGXI_COPILOT_CLIENT_ID` only if you have a
/// Copilot-authorized replacement app.
///
/// Note: GitHub Copilot's token-exchange endpoint (`copilot_internal/v2/token`)
/// only authorizes tokens minted by the **VS Code Copilot OAuth client**. This
/// is the shared client id every third-party Copilot integration uses (opencode,
/// zed, copilot.vim, LiteLLM, …); a custom app is NOT Copilot-authorized and its
/// tokens get `404 Not Found` at exchange. Override via
/// [`copilot_client_id`] / `LINGXI_COPILOT_CLIENT_ID`.
pub const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Resolve the GitHub OAuth App client id for the Copilot device flow. Defaults
/// to [`COPILOT_CLIENT_ID`]; override with the `LINGXI_COPILOT_CLIENT_ID` env var
/// once a LingXi-branded, Copilot-authorized GitHub OAuth App exists (so the
/// consent page reads "LingXi" instead of "opencode").
#[must_use]
pub fn copilot_client_id() -> String {
    std::env::var("LINGXI_COPILOT_CLIENT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| COPILOT_CLIENT_ID.to_string())
}

/// Default GitHub host (the `github.com` public deployment). GitHub Enterprise
/// logins pass their own domain (e.g. `company.ghe.com`) so the device-flow URLs
/// become `https://<domain>/login/...` (opencode's `getUrls(domain)`).
pub const DEFAULT_GITHUB_DOMAIN: &str = "github.com";

/// Device-code endpoint for a given GitHub host.
fn device_code_url(domain: &str) -> String {
    format!("https://{domain}/login/device/code")
}

/// Access-token (poll) endpoint for a given GitHub host.
fn access_token_url(domain: &str) -> String {
    format!("https://{domain}/login/oauth/access_token")
}
/// GitHub endpoint that exchanges a GitHub OAuth token for a short-lived
/// Copilot bearer token used against `api.githubcopilot.com`.
pub const COPILOT_TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
/// Re-exchange the Copilot token this many seconds before its `expires_at` so a
/// request never rides an about-to-expire token. See [`ExchangedToken::is_fresh`].
pub const COPILOT_TOKEN_REFRESH_SKEW_SECS: u64 = 300;

/// Editor-identifying headers GitHub's `copilot_internal/v2/token` endpoint
/// validates: without a recognized `Editor-Version` + `Editor-Plugin-Version`
/// pair it returns `404 Not Found` even for a Copilot-entitled account. Values
/// mirror the GitHub Copilot Chat client (the set VS Code / opencode / copilot
/// editor plugins send).
pub const COPILOT_EDITOR_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
pub const COPILOT_EDITOR_VERSION: &str = "vscode/1.99.3";
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// Minimal JSON seam for the device-flow calls + the Copilot token exchange.
///
/// Host implementations MUST send `Accept: application/json` (GitHub otherwise
/// form-encodes the response) and a `User-Agent`. Transport/TLS/timeout failures
/// map to [`LlmError::Transport`].
pub trait CopilotHttp: Send + Sync {
    /// POST `url` with a JSON body; return the parsed JSON response.
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        body: &'a Value,
    ) -> BoxFuture<'a, Result<Value, LlmError>>;

    /// GET `url` with the supplied request headers; return the parsed JSON
    /// response. Used by the Copilot token exchange, which requires an
    /// `Authorization: token <oauth>` header rather than a JSON body.
    ///
    /// Defaulted to keep existing device-flow-only hosts compiling; a host that
    /// wants Copilot token exchange MUST override this.
    fn get_json<'a>(
        &'a self,
        url: &'a str,
        _headers: &'a [(&'a str, String)],
    ) -> BoxFuture<'a, Result<Value, LlmError>> {
        let url = url.to_string();
        Box::pin(async move {
            Err(LlmError::InvalidRequest {
                message: format!(
                    "CopilotHttp::get_json not implemented by host (needed for {url})"
                ),
            })
        })
    }
}

/// Device-code grant returned by [`CopilotLogin::begin`].
#[derive(Debug, Clone)]
pub struct DeviceCodeResponse {
    /// Code the user types at `verification_uri`.
    pub user_code: String,
    /// URL the user opens to authorize.
    pub verification_uri: String,
    /// Opaque device code used when polling.
    pub device_code: String,
    /// Server-recommended polling interval (seconds).
    pub interval_secs: u64,
    /// GitHub host the flow runs against (`github.com` or an Enterprise domain).
    /// Carried so [`CopilotLogin::poll_once`] hits the SAME host's token URL.
    pub domain: String,
}

/// Classified result of one [`CopilotLogin::poll_once`].
#[derive(Debug)]
pub enum PollOutcome {
    /// Authorization complete; carries the GitHub OAuth token.
    Success(CopilotSecret),
    /// Not authorized yet; sleep `interval_secs` and poll again.
    Pending {
        /// Seconds to wait before the next poll.
        interval_secs: u64,
    },
    /// Server asked us to slow down; sleep `interval_secs` and poll again.
    SlowDown {
        /// Backed-off seconds to wait (RFC 8628 §3.5).
        interval_secs: u64,
    },
    /// Terminal failure (e.g. `access_denied`, `expired_token`).
    Failed {
        /// Server-reported error code.
        error: String,
    },
}

/// Device-flow login driver. The host owns the sleep+retry loop.
pub struct CopilotLogin<H: CopilotHttp> {
    http: H,
    client_id: String,
}

impl<H: CopilotHttp> CopilotLogin<H> {
    /// Create a login driver over the given HTTP seam.
    #[must_use]
    pub fn new(http: H) -> Self {
        Self {
            http,
            client_id: copilot_client_id(),
        }
    }

    /// Step 1 — request a device code against `domain` (`github.com` for the
    /// public deployment, or an Enterprise host like `company.ghe.com`). The
    /// caller displays `user_code` + `verification_uri`, then polls.
    pub async fn begin(&self, domain: &str) -> Result<DeviceCodeResponse, LlmError> {
        let body = json!({ "client_id": self.client_id, "scope": "read:user" });
        let v = self.http.post_json(&device_code_url(domain), &body).await?;
        Ok(DeviceCodeResponse {
            user_code: str_field(&v, "user_code")?,
            verification_uri: str_field(&v, "verification_uri")?,
            device_code: str_field(&v, "device_code")?,
            interval_secs: v.get("interval").and_then(Value::as_u64).unwrap_or(5),
            domain: domain.to_string(),
        })
    }

    /// Step 2 — poll once for the token. Returns a classified [`PollOutcome`]
    /// with the recommended next interval; the caller sleeps and re-polls on
    /// `Pending`/`SlowDown`.
    pub async fn poll_once(&self, dc: &DeviceCodeResponse) -> Result<PollOutcome, LlmError> {
        let body = json!({
            "client_id": self.client_id,
            "device_code": dc.device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        });
        let v = self
            .http
            .post_json(&access_token_url(&dc.domain), &body)
            .await?;

        if let Some(token) = v.get("access_token").and_then(Value::as_str) {
            return Ok(PollOutcome::Success(CopilotSecret::new(token)));
        }
        match v.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => Ok(PollOutcome::Pending {
                interval_secs: dc.interval_secs,
            }),
            Some("slow_down") => {
                // RFC 8628 §3.5: add 5s, or use the server-provided interval.
                let server = v.get("interval").and_then(Value::as_u64);
                Ok(PollOutcome::SlowDown {
                    interval_secs: server.unwrap_or(dc.interval_secs + 5),
                })
            }
            Some(other) => Ok(PollOutcome::Failed {
                error: other.to_string(),
            }),
            None => Ok(PollOutcome::Failed {
                error: "no access_token and no error in response".to_string(),
            }),
        }
    }
}

/// A short-lived Copilot bearer token minted from a GitHub OAuth token.
///
/// The bearer is held in a redacting [`CopilotSecret`]; only `expires_at` (a
/// Unix-seconds timestamp) and the freshness math are public. The host caches
/// this and re-exchanges when [`ExchangedToken::is_fresh`] turns false.
#[derive(Clone)]
pub struct ExchangedToken {
    /// The Copilot bearer token to send to `api.githubcopilot.com`.
    secret: CopilotSecret,
    /// Unix-seconds expiry as reported by GitHub (`expires_at`).
    pub expires_at: u64,
}

impl ExchangedToken {
    /// The bearer credential, ready to hand to [`crate::copilot::auth::CopilotAuthenticator`].
    #[must_use]
    pub fn secret(&self) -> &CopilotSecret {
        &self.secret
    }

    /// Consume into the bearer credential.
    #[must_use]
    pub fn into_secret(self) -> CopilotSecret {
        self.secret
    }

    /// The exchanged Copilot bearer string. `pub(crate)` so the credential layer
    /// ([`crate::CopilotExchangeCredentialProvider`]) can hand it to the
    /// authenticator; never logged (the wrapping types stay redacting).
    #[must_use]
    pub(crate) fn bearer(&self) -> &str {
        self.secret.token_for_storage()
    }

    /// True if the token is still safely usable at `now_unix_secs`, i.e. it does
    /// not expire within [`COPILOT_TOKEN_REFRESH_SKEW_SECS`]. The host caches the
    /// token and calls [`exchange_copilot_token`] again once this returns false.
    #[must_use]
    pub fn is_fresh(&self, now_unix_secs: u64) -> bool {
        self.expires_at > now_unix_secs.saturating_add(COPILOT_TOKEN_REFRESH_SKEW_SECS)
    }
}

impl std::fmt::Debug for ExchangedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the bearer; only the non-sensitive expiry.
        f.debug_struct("ExchangedToken")
            .field("secret", &self.secret)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Exchange a GitHub OAuth token for a short-lived Copilot bearer token.
///
/// GETs [`COPILOT_TOKEN_EXCHANGE_URL`] with `Authorization: token <oauth_token>`
/// and parses the `{ "token": "...", "expires_at": <unix_secs>, ... }` body via
/// [`parse_exchange_response`]. The returned [`ExchangedToken`] should be cached
/// with its `expires_at` and re-exchanged once [`ExchangedToken::is_fresh`] is
/// false (see [`COPILOT_TOKEN_REFRESH_SKEW_SECS`]).
///
/// `oauth_token` is the raw GitHub OAuth token (from
/// [`CopilotSecret::token_for_storage`]); it is sent only in the `Authorization`
/// header and never logged.
pub async fn exchange_copilot_token<H: CopilotHttp + ?Sized>(
    http: &H,
    oauth_token: &str,
) -> Result<ExchangedToken, LlmError> {
    let headers = [
        ("Authorization", format!("token {oauth_token}")),
        ("Editor-Version", COPILOT_EDITOR_VERSION.to_string()),
        (
            "Editor-Plugin-Version",
            COPILOT_EDITOR_PLUGIN_VERSION.to_string(),
        ),
        ("Copilot-Integration-Id", COPILOT_INTEGRATION_ID.to_string()),
        ("User-Agent", COPILOT_EDITOR_USER_AGENT.to_string()),
    ];
    let v = http.get_json(COPILOT_TOKEN_EXCHANGE_URL, &headers).await?;
    parse_exchange_response(&v)
}

/// Parse a `copilot_internal/v2/token` JSON body into an [`ExchangedToken`].
///
/// Requires a non-empty `token`; `expires_at` is the Unix-seconds expiry.
fn parse_exchange_response(v: &Value) -> Result<ExchangedToken, LlmError> {
    let token = v
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::InvalidRequest {
            message: exchange_failure_detail(v),
        })?;
    if token.is_empty() {
        return Err(LlmError::InvalidRequest {
            message: "copilot token-exchange response had an empty 'token'".to_string(),
        });
    }
    let expires_at =
        v.get("expires_at")
            .and_then(Value::as_u64)
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "copilot token-exchange response missing numeric 'expires_at'".to_string(),
            })?;
    Ok(ExchangedToken {
        secret: CopilotSecret::new(token),
        expires_at,
    })
}

/// Build a diagnostic message for a token-exchange body that lacks `token`.
/// Surfaces GitHub's own `message` when present (the real cause), else lists the
/// fields that DID come back so the failure isn't silently mislabeled.
fn exchange_failure_detail(v: &Value) -> String {
    if let Some(msg) = v.get("message").and_then(Value::as_str) {
        // GitHub returns 404 "Not Found" from copilot_internal/v2/token when the
        // account has no usable Copilot access — make that actionable.
        if msg.eq_ignore_ascii_case("not found") {
            return "GitHub Copilot is not available for this account: no active \
                    Copilot subscription, or your organization hasn't authorized \
                    this app for Copilot. Check github.com/settings/copilot."
                .to_string();
        }
        return format!("copilot token-exchange failed: {msg}");
    }
    let keys: Vec<&str> = v
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    format!(
        "copilot token-exchange response missing 'token' (got fields: [{}]) — \
         likely no active Copilot subscription on this GitHub account, or the \
         OAuth app lacks Copilot authorization",
        keys.join(", ")
    )
}

fn str_field(v: &Value, key: &str) -> Result<String, LlmError> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| LlmError::InvalidRequest {
            message: format!("copilot device-flow response missing '{key}'"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockHttp(Value);
    impl CopilotHttp for MockHttp {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _body: &'a Value,
        ) -> BoxFuture<'a, Result<Value, LlmError>> {
            let v = self.0.clone();
            Box::pin(async move { Ok(v) })
        }
    }

    fn dc() -> DeviceCodeResponse {
        DeviceCodeResponse {
            user_code: "WDJB-MJHT".to_string(),
            verification_uri: "https://github.com/login/device".to_string(),
            device_code: "dev-code".to_string(),
            interval_secs: 5,
            domain: "github.com".to_string(),
        }
    }

    #[tokio::test]
    async fn begin_parses_device_code() {
        let login = CopilotLogin::new(MockHttp(json!({
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://github.com/login/device",
            "device_code": "dev-code",
            "interval": 7
        })));
        let parsed = login.begin("github.com").await.expect("begin ok");
        assert_eq!(parsed.user_code, "WDJB-MJHT");
        assert_eq!(parsed.device_code, "dev-code");
        assert_eq!(parsed.interval_secs, 7);
        assert_eq!(parsed.domain, "github.com");
    }

    #[tokio::test]
    async fn poll_success_yields_token() {
        let login = CopilotLogin::new(MockHttp(json!({ "access_token": "ght_abc" })));
        match login.poll_once(&dc()).await.expect("poll ok") {
            PollOutcome::Success(secret) => {
                assert!(!format!("{secret:?}").contains("ght_abc"));
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_pending_returns_device_interval() {
        let login = CopilotLogin::new(MockHttp(json!({ "error": "authorization_pending" })));
        match login.poll_once(&dc()).await.unwrap() {
            PollOutcome::Pending { interval_secs } => assert_eq!(interval_secs, 5),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_slow_down_without_server_interval_adds_five() {
        let login = CopilotLogin::new(MockHttp(json!({ "error": "slow_down" })));
        match login.poll_once(&dc()).await.unwrap() {
            PollOutcome::SlowDown { interval_secs } => assert_eq!(interval_secs, 10),
            other => panic!("expected SlowDown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_slow_down_uses_server_interval_when_present() {
        let login = CopilotLogin::new(MockHttp(json!({ "error": "slow_down", "interval": 42 })));
        match login.poll_once(&dc()).await.unwrap() {
            PollOutcome::SlowDown { interval_secs } => assert_eq!(interval_secs, 42),
            other => panic!("expected SlowDown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_other_error_is_terminal() {
        let login = CopilotLogin::new(MockHttp(json!({ "error": "access_denied" })));
        match login.poll_once(&dc()).await.unwrap() {
            PollOutcome::Failed { error } => assert_eq!(error, "access_denied"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn parse_exchange_response_extracts_token_and_expiry() {
        let v = json!({
            "token": "tid=abc;exp=123;sku=copilot",
            "expires_at": 1_900_000_000_u64,
            "refresh_in": 1500
        });
        let exchanged = parse_exchange_response(&v).expect("parses");
        assert_eq!(exchanged.expires_at, 1_900_000_000);
        // Bearer is usable but never leaked via Debug.
        assert_eq!(
            exchanged.secret().token_for_storage(),
            "tid=abc;exp=123;sku=copilot"
        );
        assert!(!format!("{exchanged:?}").contains("tid=abc"));
    }

    #[test]
    fn parse_exchange_response_rejects_missing_fields() {
        // Missing token.
        assert!(parse_exchange_response(&json!({ "expires_at": 1_u64 })).is_err());
        // Empty token.
        assert!(parse_exchange_response(&json!({ "token": "", "expires_at": 1_u64 })).is_err());
        // Missing / non-numeric expires_at.
        assert!(parse_exchange_response(&json!({ "token": "x" })).is_err());
        assert!(parse_exchange_response(&json!({ "token": "x", "expires_at": "soon" })).is_err());
    }

    #[test]
    fn exchanged_token_freshness_respects_skew() {
        let t = parse_exchange_response(&json!({ "token": "x", "expires_at": 1000_u64 }))
            .expect("parses");
        // Far before expiry (minus skew) => fresh.
        assert!(t.is_fresh(1000 - COPILOT_TOKEN_REFRESH_SKEW_SECS - 1));
        // Inside the skew window => stale, should re-exchange.
        assert!(!t.is_fresh(1000 - COPILOT_TOKEN_REFRESH_SKEW_SECS));
        assert!(!t.is_fresh(2000));
    }

    struct GetMock {
        body: Value,
        seen_headers: std::sync::Mutex<Vec<(String, String)>>,
    }
    impl CopilotHttp for GetMock {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _body: &'a Value,
        ) -> BoxFuture<'a, Result<Value, LlmError>> {
            Box::pin(async { unreachable!("exchange uses get_json") })
        }
        fn get_json<'a>(
            &'a self,
            _url: &'a str,
            headers: &'a [(&'a str, String)],
        ) -> BoxFuture<'a, Result<Value, LlmError>> {
            *self.seen_headers.lock().unwrap() = headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect();
            let v = self.body.clone();
            Box::pin(async move { Ok(v) })
        }
    }

    #[tokio::test]
    async fn exchange_sends_token_auth_header_and_parses() {
        let mock = GetMock {
            body: json!({ "token": "copilot-bearer", "expires_at": 1_900_000_000_u64 }),
            seen_headers: std::sync::Mutex::new(Vec::new()),
        };
        let exchanged = exchange_copilot_token(&mock, "ght_oauth")
            .await
            .expect("ok");
        assert_eq!(exchanged.expires_at, 1_900_000_000);
        assert_eq!(exchanged.secret().token_for_storage(), "copilot-bearer");
        // Auth header is the GitHub `token <oauth>` scheme, not `Bearer`.
        let headers = mock.seen_headers.lock().unwrap().clone();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(header("Authorization"), Some("token ght_oauth"));
        assert_eq!(header("Editor-Version"), Some(COPILOT_EDITOR_VERSION));
        assert_eq!(
            header("Editor-Plugin-Version"),
            Some(COPILOT_EDITOR_PLUGIN_VERSION)
        );
        assert_eq!(
            header("Copilot-Integration-Id"),
            Some(COPILOT_INTEGRATION_ID)
        );
        assert_eq!(header("User-Agent"), Some(COPILOT_EDITOR_USER_AGENT));
    }

    #[tokio::test]
    async fn default_get_json_errors_for_device_flow_only_hosts() {
        // The device-flow MockHttp does not override get_json; exchange must fail
        // cleanly rather than silently succeed.
        let res = exchange_copilot_token(&MockHttp(json!({})), "ght_oauth").await;
        assert!(res.is_err());
    }
}
