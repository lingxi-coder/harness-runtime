//! MCP-only OAuth registration, discovery and cross-process refresh state.
//! Model-provider accounts never enter this namespace.
use super::OAuthError;
use lingxi_core::host::{Clock, SecureStorage};
use lingxi_core::host::{HttpError, HttpTransport, McpTransportSpec};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{HttpMethod, HttpRequest, HttpResponse};
use lingxi_core::types::{Secret, SecretKindDto, SecureStorageData, SecureStorageMetadata};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const SERVICE: &str = "mcp-oauth-session";

/// Native Bve/rcr attach configured non-Authorization headers only to the
/// protected resource's exact origin. Redirects repeat that origin check.
pub(crate) fn configured_http(
    inner: &Arc<dyn HttpTransport>,
    spec: &McpTransportSpec,
) -> Arc<dyn HttpTransport> {
    configured_http_mode(inner, spec, false)
}
pub(crate) fn configured_revocation_http(
    inner: &Arc<dyn HttpTransport>,
    spec: &McpTransportSpec,
) -> Arc<dyn HttpTransport> {
    configured_http_mode(inner, spec, true)
}
fn configured_http_mode(
    inner: &Arc<dyn HttpTransport>,
    spec: &McpTransportSpec,
    revocation: bool,
) -> Arc<dyn HttpTransport> {
    let (url, headers) = match spec {
        McpTransportSpec::Http { url, headers, .. }
        | McpTransportSpec::Sse { url, headers, .. } => (url, headers),
        _ => return inner.clone(),
    };
    if headers.is_empty() {
        return inner.clone();
    }
    Arc::new(ConfiguredOAuthHttp {
        inner: inner.clone(),
        revocation,
        origin: url::Url::parse(url).ok().map(|url| url.origin()),
        headers: headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect(),
    })
}
struct ConfiguredOAuthHttp {
    inner: Arc<dyn HttpTransport>,
    revocation: bool,
    origin: Option<url::Origin>,
    headers: Vec<(String, String)>,
}
#[async_trait::async_trait]
impl HttpTransport for ConfiguredOAuthHttp {
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let mut request = request;
        let mut target = request.url.clone();
        let limit = if self.revocation { 21 } else { 5 };
        for redirect in 0..=limit {
            let same_origin = url::Url::parse(&target)
                .ok()
                .is_some_and(|url| Some(url.origin()) == self.origin);
            let mut current = request.clone();
            current.url = target.clone();
            if !same_origin {
                return self.inner.request(current).await;
            }
            let mut headers = self.headers.clone();
            for (name, value) in &request.headers {
                headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
                headers.push((name.to_ascii_lowercase(), value.clone()));
            }
            current.headers = headers;
            let response = self.inner.request_no_follow(current).await?;
            let location = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value);
            if !(300..400).contains(&response.status) || location.is_none() || redirect == limit {
                return Ok(response);
            }
            let next = url::Url::parse(&target)
                .ok()
                .and_then(|url| url.join(location.unwrap()).ok())
                .ok_or_else(|| HttpError::InvalidRequest("invalid OAuth redirect URL".into()))?;
            let allowed = if self.revocation {
                Some(next.origin()) == self.origin
            } else if request.method == HttpMethod::Get {
                matches!(next.scheme(), "http" | "https")
            } else {
                [307, 308].contains(&response.status) && Some(next.origin()) == self.origin
            };
            if !allowed {
                if self.revocation {
                    return Err(HttpError::InvalidRequest(
                        "Refused a redirect to another origin".into(),
                    ));
                }
                return Ok(response);
            }
            if self.revocation
                && (([301, 302].contains(&response.status) && request.method == HttpMethod::Post)
                    || (response.status == 303
                        && request.method != HttpMethod::Get
                        && request.method != HttpMethod::Head))
            {
                request.method = HttpMethod::Get;
                request.body = None;
                request.body_bytes = None;
                request
                    .headers
                    .retain(|(name, _)| !name.to_ascii_lowercase().starts_with("content-"));
            }
            target = next.to_string();
        }
        unreachable!()
    }
    async fn stream_sse(
        &self,
        request: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        self.inner.stream_sse(request).await
    }
}

/// Configurable public MCP client identity. A hosted metadata document is
/// opt-in; no model provider's identity or account is used as a default.
#[derive(Debug, Clone)]
pub struct McpOAuthIdentity {
    pub client_name: String,
    pub client_metadata_url: Option<String>,
    pub refresh_lock_dir: Option<PathBuf>,
    /// Native issuer-echo enforcement rollout. Observe is the default;
    /// enforcement rejects cross-origin or unparseable issuer echoes.
    pub strict_issuer_echo: bool,
}
impl Default for McpOAuthIdentity {
    fn default() -> Self {
        Self {
            client_name: branding::PRODUCT_NAME.into(),
            client_metadata_url: std::env::var("MCP_OAUTH_CLIENT_METADATA_URL")
                .ok()
                .filter(|url| !url.is_empty()),
            refresh_lock_dir: None,
            strict_issuer_echo: false,
        }
    }
}
impl McpOAuthIdentity {
    pub(crate) fn metadata_url(&self) -> Result<Option<&str>, OAuthError> {
        let Some(value) = self.client_metadata_url.as_deref() else {
            return Ok(None);
        };
        let valid =
            url::Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.path() != "/");
        if !valid {
            return Err(OAuthError::Registration(format!("clientMetadataUrl must be a valid HTTPS URL with a non-root pathname, got: {value}")));
        }
        Ok(Some(value))
    }
    fn lock_root(&self) -> PathBuf {
        if let Some(root) = &self.refresh_lock_dir {
            return root.clone();
        }
        #[cfg(test)]
        {
            std::env::temp_dir().join("lingxi-mcp-refresh-tests")
        }
        #[cfg(not(test))]
        {
            let user_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            branding::config_home(&user_home, std::env::var_os(branding::CONFIG_DIR_ENV))
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub(crate) struct OAuthSessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_registration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_issuer: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_client_secret_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<StoredDiscovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_response: Option<String>,
}
impl std::fmt::Debug for OAuthSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSessionState")
            .field(
                "has_client_registration",
                &self.client_registration.is_some(),
            )
            .field(
                "has_configured_secret",
                &self.configured_client_secret.is_some(),
            )
            .field("discovery", &self.discovery)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredDiscovery {
    pub authorization_server_url: String,
    pub oauth_metadata_found: bool,
    pub metadata_binding: MetadataBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_metadata: Option<String>,
    #[serde(default)]
    pub xaa_pinned: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum MetadataBinding {
    Unstamped,
    Default,
    Configured(String),
}

#[derive(Clone)]
pub(crate) struct ClientInformation {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub token_endpoint_auth_method: Option<String>,
    pub raw: String,
}
impl OAuthSessionState {
    pub(crate) fn resolve_client(
        &self,
        configured_client: Option<&str>,
        issuer: &[u16],
        redirect: Option<&str>,
        identity: &McpOAuthIdentity,
    ) -> Option<ClientInformation> {
        let configured_client = configured_client.filter(|id| !id.is_empty());
        let parsed = self
            .client_registration
            .as_deref()
            .and_then(|raw| Utf16JsonProjection::parse(raw).ok());
        let stored_id = parsed
            .as_ref()
            .and_then(|value| value.string_at("/client_id"))
            .filter(|id| !id.is_empty());
        let issuer_matches = self
            .client_issuer
            .as_ref()
            .is_none_or(|stored| issuer_equal(stored, issuer));
        if stored_id.is_some() && !issuer_matches {
            return None;
        }
        let redirect_matches = redirect.is_none_or(|redirect| {
            let is_cimd =
                identity.client_metadata_url.as_deref() == stored_id && stored_id.is_some();
            (!is_cimd || loopback_callback(redirect))
                && self
                    .redirect_uri
                    .as_deref()
                    .map(|old| redirect_origin(old) == redirect_origin(redirect))
                    .unwrap_or_else(|| redirect.starts_with("http://localhost"))
        });
        if issuer_matches && redirect_matches {
            if let (Some(value), Some(client_id), Some(raw)) = (
                parsed.as_ref(),
                stored_id,
                self.client_registration.as_ref(),
            ) {
                return Some(ClientInformation {
                    client_id: client_id.into(),
                    client_secret: value
                        .string_at("/client_secret")
                        .map(str::to_owned)
                        .or_else(|| {
                            (Some(client_id) == configured_client)
                                .then(|| self.configured_client_secret.clone())
                                .flatten()
                        }),
                    token_endpoint_auth_method: value
                        .string_at("/token_endpoint_auth_method")
                        .map(str::to_owned),
                    raw: raw.clone(),
                });
            }
        }
        configured_client.map(|client_id| ClientInformation {
            client_id: client_id.into(),
            client_secret: self.configured_client_secret.clone(),
            token_endpoint_auth_method: None,
            raw: match self.configured_client_secret_source.as_deref() {
                Some(secret) => format!(
                    "{{\"client_id\":{},\"client_secret\":{secret}}}",
                    serde_json::to_string(client_id).unwrap()
                ),
                None => serde_json::json!({"client_id":client_id}).to_string(),
            },
        })
    }
    pub(crate) fn save_client(
        &mut self,
        client: &ClientInformation,
        issuer: Vec<u16>,
        redirect: &str,
    ) {
        self.client_registration = Some(client.raw.clone());
        self.client_issuer = Some(issuer);
        self.redirect_uri = Some(redirect.into());
    }
    pub(crate) fn invalidate(&mut self, scope: &str) {
        match scope {
            "all" | "client" => {
                self.client_registration = None;
                self.client_issuer = None;
                self.redirect_uri = None;
                if scope == "all" {
                    self.token_response = None;
                }
            }
            "tokens" => self.token_response = None,
            "discovery" => {
                if let Some(discovery) = self.discovery.as_mut() {
                    discovery.authorization_metadata = None;
                }
            }
            _ => {}
        }
    }
}

fn redirect_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    Some(format!("{}://{}", url.scheme(), url.host_str()?))
}

pub(crate) fn saved_callback_port(state: &OAuthSessionState) -> Option<u16> {
    let client = Utf16JsonProjection::parse(state.client_registration.as_deref()?).ok()?;
    client.string_at("/client_id").filter(|id| !id.is_empty())?;
    let redirect = state.redirect_uri.as_deref()?;
    if !loopback_callback(redirect) {
        return None;
    }
    let port = url::Url::parse(redirect)
        .ok()?
        .port()
        .filter(|port| *port != 0)?;
    (redirect.replace("//127.0.0.1:", "//localhost:")
        == format!("http://localhost:{port}/callback"))
    .then_some(port)
}
pub(crate) fn loopback_callback(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
            && url.path() == "/callback"
    })
}
pub(crate) fn issuer_equal(first: &[u16], second: &[u16]) -> bool {
    first == second
        || first.strip_suffix(&[u16::from(b'/')]) == Some(second)
        || second.strip_suffix(&[u16::from(b'/')]) == Some(first)
}

pub(crate) async fn load(
    storage: &Arc<dyn SecureStorage>,
    key: &str,
) -> Result<OAuthSessionState, OAuthError> {
    let data = storage
        .retrieve(SERVICE, key)
        .await
        .map_err(|error| OAuthError::Token(format!("storage retrieve OAuth session: {error}")))?;
    data.map(|data| {
        serde_json::from_slice(data.expose_secret_bytes())
            .map_err(|error| OAuthError::Token(format!("decode OAuth session: {error}")))
    })
    .transpose()
    .map(Option::unwrap_or_default)
}
pub(crate) async fn save(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    key: &str,
    state: &OAuthSessionState,
) -> Result<(), OAuthError> {
    let bytes = serde_json::to_vec(state).map_err(|error| OAuthError::Token(error.to_string()))?;
    storage
        .store(
            SERVICE,
            key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: clock.now(),
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_session".into()),
                },
            ),
        )
        .await
        .map_err(|error| OAuthError::Token(format!("storage store OAuth session: {error}")))
}

pub(crate) async fn clear_auth(
    storage: &Arc<dyn SecureStorage>,
    key: &str,
) -> Result<(), OAuthError> {
    super::invalidate_flow_discovery(storage, key);
    let mut state = load(storage, key).await?;
    state.invalidate("all");
    state.discovery = None;
    if state.configured_client_secret.is_none() {
        return storage
            .delete(SERVICE, key)
            .await
            .map_err(|error| OAuthError::Token(error.to_string()));
    }
    let bytes = serde_json::to_vec(&state).map_err(|error| OAuthError::Token(error.to_string()))?;
    storage
        .store(
            SERVICE,
            key,
            SecureStorageData::new(
                bytes,
                SecureStorageMetadata {
                    created_at: SystemTime::now(),
                    last_accessed: None,
                    kind: SecretKindDto("mcp_oauth_session".into()),
                },
            ),
        )
        .await
        .map_err(|error| OAuthError::Token(error.to_string()))
}

/// Configure a confidential MCP client secret in the MCP secure namespace.
/// `key` is `oauth::server_key`, never a model-provider account identifier.
pub async fn configure_client_secret(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    key: &str,
    secret: Option<&Secret<String>>,
) -> Result<(), OAuthError> {
    let mut state = load(storage, key).await?;
    state.configured_client_secret = secret.map(|secret| secret.expose_secret().clone());
    state.configured_client_secret_source = secret.map(|secret| {
        serde_json::to_string(secret.expose_secret()).expect("Rust string serializes")
    });
    save(storage, clock, key, &state).await
}

/// Source-preserving form of [`configure_client_secret`]. The projection must
/// be a JSON string; UTF-16 units remain encrypted in the MCP namespace.
pub async fn configure_client_secret_source(
    storage: &Arc<dyn SecureStorage>,
    clock: &Arc<dyn Clock>,
    key: &str,
    secret: Option<&Utf16JsonProjection>,
) -> Result<(), OAuthError> {
    let mut state = load(storage, key).await?;
    if let Some(secret) = secret {
        let display = secret
            .string_at("")
            .ok_or_else(|| OAuthError::Token("MCP client secret must be a JSON string".into()))?;
        state.configured_client_secret = Some(display.into());
        state.configured_client_secret_source = Some(
            secret
                .to_json_string()
                .map_err(|error| OAuthError::Token(error.to_string()))?,
        );
    } else {
        state.configured_client_secret = None;
        state.configured_client_secret_source = None;
    }
    save(storage, clock, key, &state).await
}

/// Source-observed proper-lockfile policy: target mcp-refresh-KEY.lock,
/// directory target.lock, five attempts,60s stale lease and5s heartbeat.
pub(crate) struct RefreshLock {
    path: PathBuf,
    expected_mtime: Arc<Mutex<SystemTime>>,
    heartbeat: tokio::task::JoinHandle<()>,
}
impl RefreshLock {
    pub(crate) async fn acquire(identity: &McpOAuthIdentity, key: &str) -> Option<Self> {
        let root = identity.lock_root();
        if std::fs::create_dir_all(&root).is_err() {
            return None;
        }
        let safe_key: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let path = root.join(format!("mcp-refresh-{safe_key}.lock.lock"));
        for _ in 0..5 {
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let mtime = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::now());
                    let expected_mtime = Arc::new(Mutex::new(mtime));
                    let lease_path = path.clone();
                    let lease_time = expected_mtime.clone();
                    let heartbeat = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            let Ok(metadata) = std::fs::metadata(&lease_path) else {
                                break;
                            };
                            if metadata.modified().ok() != Some(*lease_time.lock().unwrap()) {
                                break;
                            }
                            let now = SystemTime::now();
                            if std::fs::File::open(&lease_path)
                                .and_then(|file| file.set_modified(now))
                                .is_err()
                            {
                                break;
                            }
                            if let Ok(value) =
                                std::fs::metadata(&lease_path).and_then(|m| m.modified())
                            {
                                *lease_time.lock().unwrap() = value;
                            }
                        }
                    });
                    return Some(Self {
                        path,
                        expected_mtime,
                        heartbeat,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                        .is_some_and(|age| age > Duration::from_secs(60));
                    if stale {
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(1000 + rand::random::<u64>() % 1000))
                        .await;
                }
                Err(_) => return None,
            }
        }
        None
    }
}
impl Drop for RefreshLock {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if std::fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok()
            == Some(*self.expected_mtime.lock().unwrap())
        {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
