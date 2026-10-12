//! Host implementation of Claude Code's process-local `proxyAuthHelper`.
//!
//! LLM and MCP requests share this cache through the SDK transport trait. The
//! helper process itself stays in the host layer; the SDK only observes real
//! proxy challenges and asks this object to refresh its cached header.

use lingxi_llm_client::transport::ProxyAuthHelper;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TTL_MS: f64 = 300_000.0;
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

/// Host facts used by Native's helper settings gate and command placement.
#[derive(Debug, Clone, Default)]
pub struct ProxyAuthHelperConfig {
    /// User home used as the working directory for non-project helper sources.
    pub home_dir: Option<PathBuf>,
    /// Current project directory used by project and local helper sources.
    pub project_dir: Option<PathBuf>,
    /// Whether the winning helper setting comes from project or local settings.
    pub from_project_or_local: bool,
    /// Native person-and-policy eligibility supplied by the host.
    pub source_is_person_and_policy: bool,
    /// Whether this host is running an interactive query.
    pub interactive: bool,
    /// Trust snapshot used when no live workspace trust provider is installed.
    pub workspace_trusted: bool,
}

#[derive(Debug, Clone)]
struct CachedValue {
    value: String,
    timestamp_ms: f64,
}

#[derive(Default)]
struct State {
    cached: Option<CachedValue>,
    pending_challenge: Option<Vec<u8>>,
}

#[derive(Clone)]
struct HelperSettings {
    command_utf16: Vec<u16>,
    config: ProxyAuthHelperConfig,
}

struct Inner {
    settings: RwLock<HelperSettings>,
    state: Mutex<State>,
    workspace_trusted_provider: RwLock<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
}

/// Shared process-local Native proxy-auth cache and asynchronous shell runner.
pub struct NativeProxyAuthHelper {
    inner: Arc<Inner>,
}

impl NativeProxyAuthHelper {
    /// Create a helper from the exact settings UTF-16 string. The conversion to
    /// an operating-system command happens only at the shell boundary.
    #[must_use]
    pub fn new(command_utf16: Vec<u16>, config: ProxyAuthHelperConfig) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                settings: RwLock::new(HelperSettings {
                    command_utf16,
                    config,
                }),
                state: Mutex::new(State::default()),
                workspace_trusted_provider: RwLock::new(None),
            }),
        })
    }

    /// Replace the currently effective setting without replacing the shared
    /// Native cache. The settings watcher uses this on the same session Arc.
    pub fn update_config(&self, command_utf16: Vec<u16>, config: ProxyAuthHelperConfig) {
        if let Ok(mut settings) = self.inner.settings.write() {
            settings.command_utf16 = command_utf16;
            settings.config = config;
        }
    }

    /// Read workspace trust from the live host source whenever Native decides
    /// whether a project/local helper may execute. The provider is session
    /// scoped and survives settings updates on this same helper/cache Arc.
    pub fn set_workspace_trusted_provider(&self, provider: Arc<dyn Fn() -> bool + Send + Sync>) {
        if let Ok(mut current) = self.inner.workspace_trusted_provider.write() {
            *current = Some(provider);
        }
    }

    /// Start Native's nonblocking cache warm-up. The helper process continues
    /// in the background, matching `lAo()`'s fire-and-forget `_Ue()` call.
    pub async fn warm(&self) {
        let Ok((from_project_or_local, fallback_trust)) =
            self.inner.settings.read().map(|settings| {
                (
                    settings.config.from_project_or_local,
                    settings.config.workspace_trusted,
                )
            })
        else {
            return;
        };
        if from_project_or_local && !current_workspace_trust(&self.inner, fallback_trust) {
            return;
        }
        self.start_refresh();
    }

    fn start_refresh(&self) {
        let Ok(settings) = self.inner.settings.read().map(|settings| settings.clone()) else {
            return;
        };
        if !helper_enabled() || settings.command_utf16.is_empty() {
            return;
        }
        let challenge = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            if state.pending_challenge.is_none()
                && state
                    .cached
                    .as_ref()
                    .is_some_and(|cached| now_ms() - cached.timestamp_ms < helper_ttl_ms())
            {
                return;
            }
            state.pending_challenge.take()
        };

        let inner = self.inner.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            refresh(inner, settings, challenge).await;
        });
    }
}

impl ProxyAuthHelper for NativeProxyAuthHelper {
    fn cached_proxy_authorization(&self) -> Option<String> {
        self.inner
            .state
            .lock()
            .ok()?
            .cached
            .as_ref()
            .map(|cached| cached.value.clone())
    }

    fn on_proxy_auth_challenge(&self, challenge: Option<&[u8]>) -> bool {
        let configured = self
            .inner
            .settings
            .read()
            .is_ok_and(|settings| !settings.command_utf16.is_empty());
        if !helper_enabled() || !configured {
            return false;
        }
        if let Ok(mut state) = self.inner.state.lock() {
            // Native Mnn clears the current header and stores the challenge
            // before the fire-and-forget helper callback starts.
            state.cached = None;
            state.pending_challenge = challenge
                .filter(|challenge| !challenge.is_empty())
                .map(<[u8]>::to_vec);
        }
        self.start_refresh();
        true
    }
}

async fn refresh(inner: Arc<Inner>, settings: HelperSettings, challenge: Option<Vec<u8>>) {
    if !helper_enabled() || settings.command_utf16.is_empty() {
        return;
    }

    let from_project_or_local = settings.config.from_project_or_local;
    if settings.config.source_is_person_and_policy && from_project_or_local {
        return;
    }
    if from_project_or_local
        && !settings.config.interactive
        && !current_workspace_trust(&inner, settings.config.workspace_trusted)
    {
        return;
    }

    match run_helper(&settings, challenge.as_deref()).await {
        Ok(Some(value)) => {
            if let Ok(mut state) = inner.state.lock() {
                state.cached = Some(CachedValue {
                    value,
                    timestamp_ms: now_ms(),
                });
            }
        }
        Ok(None) => {}
        Err(failure) => {
            tracing::error!("proxyAuthHelper failed: {failure}");
        }
    }
}

fn current_workspace_trust(inner: &Inner, fallback: bool) -> bool {
    let provider = inner
        .workspace_trusted_provider
        .read()
        .ok()
        .and_then(|provider| provider.clone());
    provider.map_or(fallback, |provider| provider())
}

#[derive(Debug)]
enum HelperFailure {
    TimedOut,
    Exited(Option<i32>, String),
    Spawn(String),
}

impl std::fmt::Display for HelperFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => f.write_str("timed out"),
            Self::Exited(code, stderr) => {
                let reason = format!(
                    "exited {}",
                    code.map_or_else(|| "null".into(), |v| v.to_string())
                );
                if stderr.is_empty() {
                    f.write_str(&reason)
                } else {
                    write!(f, "{reason}: {stderr}")
                }
            }
            Self::Spawn(error) => write!(f, "could not start: {error}"),
        }
    }
}

async fn run_helper(
    settings: &HelperSettings,
    challenge: Option<&[u8]>,
) -> Result<Option<String>, HelperFailure> {
    let command = String::from_utf16_lossy(&settings.command_utf16);
    let mut process = shell_command(&command);
    process
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let working_directory = if settings.config.source_is_person_and_policy {
        settings.config.home_dir.as_deref()
    } else {
        settings.config.project_dir.as_deref()
    };
    if let Some(working_directory) = working_directory {
        process.current_dir(working_directory);
    }

    if let Some(proxy_url) = process_proxy_url() {
        process.env("CLAUDE_CODE_PROXY_URL", &proxy_url);
        if let Ok(url) = url::Url::parse(&proxy_url) {
            if let Some(host) = url.host_str() {
                process.env("CLAUDE_CODE_PROXY_HOST", host);
            }
        }
    }
    if let Some(challenge) = challenge.filter(|challenge| !challenge.is_empty()) {
        process.env("CLAUDE_CODE_PROXY_AUTHENTICATE", latin1_string(challenge));
    }

    let output = match tokio::time::timeout(HELPER_TIMEOUT, process.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(HelperFailure::Spawn(error.to_string())),
        Err(_) => return Err(HelperFailure::TimedOut),
    };
    if !output.status.success() {
        return Err(HelperFailure::Exited(
            output.status.code(),
            trim_js_whitespace(&String::from_utf8_lossy(&output.stderr)).to_owned(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = trim_js_whitespace(&stdout);
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_owned()))
}

#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("sh");
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("cmd.exe");
    process.arg("/d").arg("/s").arg("/c").arg(command);
    process
}

#[cfg(not(any(unix, windows)))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("sh");
    process.arg("-c").arg(command);
    process
}

fn process_proxy_url() -> Option<String> {
    ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
}

fn helper_enabled() -> bool {
    std::env::var_os("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER").is_some_and(|value| !value.is_empty())
}

fn helper_ttl_ms() -> f64 {
    std::env::var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS")
        .ok()
        .map(|value| parse_js_number(&value))
        .filter(|value| !value.is_nan() && *value >= 0.0)
        .unwrap_or(DEFAULT_TTL_MS)
}

fn parse_js_number(value: &str) -> f64 {
    let value = trim_js_whitespace(value);
    if value.is_empty() {
        return 0.0;
    }
    match value {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16_u32),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut number = 0.0;
            for digit in digits.chars() {
                let Some(digit) = digit.to_digit(radix) else {
                    return f64::NAN;
                };
                number = number * f64::from(radix) + f64::from(digit);
            }
            return number;
        }
    }
    if !is_js_decimal_number(value) {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}

fn is_js_decimal_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(&b'+') | Some(&b'-')));
    let before_decimal_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let before_decimal = index > before_decimal_start;
    let mut after_decimal = false;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let after_decimal_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        after_decimal = index > after_decimal_start;
    }
    if !before_decimal && !after_decimal {
        return false;
    }
    if matches!(bytes.get(index), Some(&b'e') | Some(&b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(&b'+') | Some(&b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_millis() as f64)
}

fn trim_js_whitespace(value: &str) -> &str {
    value.trim_matches(lingxi_core::host::effort::javascript_whitespace)
}

fn latin1_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        helper_enabled, helper_ttl_ms, now_ms, process_proxy_url, trim_js_whitespace,
        NativeProxyAuthHelper, ProxyAuthHelperConfig,
    };
    use lingxi_llm_client::transport::ProxyAuthHelper as _;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn helper_gate_uses_javascript_truthiness_and_ttl_uses_javascript_number() {
        let _guard = env_lock();
        let old_enabled = std::env::var_os("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER");
        let old_ttl = std::env::var_os("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS");
        {
            std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "0");
        }
        assert!(
            helper_enabled(),
            "the Native gate accepts any non-empty string"
        );
        {
            std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "");
        }
        assert!(!helper_enabled());
        {
            std::env::remove_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS");
        }
        assert_eq!(helper_ttl_ms(), 300_000.0);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "1500");
        }
        assert_eq!(helper_ttl_ms(), 1_500.0);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "1_500");
        }
        assert_eq!(helper_ttl_ms(), 300_000.0);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "1.5");
        }
        assert_eq!(helper_ttl_ms(), 1.5);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "0x10");
        }
        assert_eq!(helper_ttl_ms(), 16.0);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "");
        }
        assert_eq!(helper_ttl_ms(), 0.0);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "Infinity");
        }
        assert_eq!(helper_ttl_ms(), f64::INFINITY);
        {
            std::env::set_var("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", "-1");
        }
        assert_eq!(helper_ttl_ms(), 300_000.0);
        restore_env("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", old_enabled);
        restore_env("CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS", old_ttl);
    }

    #[test]
    fn retry_eligibility_uses_only_the_enabled_nonempty_command_gate() {
        let _guard = env_lock();
        let old_enabled = std::env::var_os("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER");
        {
            std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "0");
        }
        let configured = NativeProxyAuthHelper::new(
            " ".encode_utf16().collect(),
            ProxyAuthHelperConfig {
                from_project_or_local: true,
                interactive: false,
                workspace_trusted: false,
                ..Default::default()
            },
        );
        assert!(configured.on_proxy_auth_challenge(None));
        assert!(
            !NativeProxyAuthHelper::new(Vec::new(), ProxyAuthHelperConfig::default())
                .on_proxy_auth_challenge(None)
        );
        {
            std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "");
        }
        assert!(!configured.on_proxy_auth_challenge(None));
        restore_env("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", old_enabled);
    }

    #[test]
    fn helper_uses_native_proxy_priority_and_ecmascript_trim() {
        let _guard = env_lock();
        let keys = ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"];
        let old = keys.map(|key| (key, std::env::var_os(key)));
        {
            std::env::set_var("https_proxy", "http://first.example:3128");
            std::env::set_var("HTTPS_PROXY", "http://second.example:3128");
            std::env::remove_var("http_proxy");
            std::env::remove_var("HTTP_PROXY");
        }
        assert_eq!(
            process_proxy_url().as_deref(),
            Some("http://first.example:3128")
        );
        assert_eq!(trim_js_whitespace("\u{feff} token\u{2028}"), "token");
        {
            for (key, value) in old {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_process_trims_output_and_preserves_cached_value_on_empty_result() {
        let _guard = env_lock();
        let old_enabled = std::env::var_os("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER");
        {
            std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "1");
        }
        let helper = NativeProxyAuthHelper::new(
            "printf '\\357\\273\\277  token\\342\\200\\250\\n'"
                .encode_utf16()
                .collect(),
            ProxyAuthHelperConfig::default(),
        );
        let settings = helper.inner.settings.read().unwrap().clone();
        let value = super::run_helper(&settings, None)
            .await
            .expect("helper process should succeed")
            .expect("trimmed token should remain");
        assert_eq!(value, "token");
        let empty = NativeProxyAuthHelper::new(
            "printf '  \\357\\273\\277  '\n".encode_utf16().collect(),
            ProxyAuthHelperConfig::default(),
        );
        empty.inner.state.lock().unwrap().cached = Some(super::CachedValue {
            value: "stale".to_owned(),
            timestamp_ms: now_ms(),
        });
        let empty_settings = empty.inner.settings.read().unwrap().clone();
        super::refresh(empty.inner.clone(), empty_settings, None).await;
        assert_eq!(
            empty.cached_proxy_authorization().as_deref(),
            Some("stale"),
            "empty helper output leaves the previous cached value in place"
        );
        restore_env("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", old_enabled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_407_callback_clears_cache_and_refreshes_with_header_bytes() {
        let _guard = env_lock();
        let old_enabled = std::env::var_os("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER");
        {
            std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "false");
        }
        let helper = NativeProxyAuthHelper::new(
            "printf 'auth:%s' \"$CLAUDE_CODE_PROXY_AUTHENTICATE\""
                .encode_utf16()
                .collect(),
            ProxyAuthHelperConfig::default(),
        );
        helper.inner.state.lock().unwrap().cached = Some(super::CachedValue {
            value: "expired".to_owned(),
            timestamp_ms: now_ms(),
        });
        let challenge = b"Basic realm=\x80";
        assert!(helper.on_proxy_auth_challenge(Some(challenge)));
        assert_eq!(helper.cached_proxy_authorization(), None);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if helper.cached_proxy_authorization().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("helper process should refresh the cache");
        assert_eq!(
            helper.cached_proxy_authorization().as_deref(),
            Some("auth:Basic realm=\u{80}")
        );
        restore_env("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", old_enabled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_workspace_trust_provider_survives_settings_update_and_opens_execution_gate() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = env_lock();
        let old_enabled = std::env::var_os("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER");
        std::env::set_var("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", "false");
        let config = ProxyAuthHelperConfig {
            from_project_or_local: true,
            interactive: false,
            workspace_trusted: false,
            ..Default::default()
        };
        let helper = NativeProxyAuthHelper::new(
            "printf 'fresh-token'".encode_utf16().collect(),
            config.clone(),
        );
        let trusted = std::sync::Arc::new(AtomicBool::new(false));
        let trusted_provider = trusted.clone();
        helper.set_workspace_trusted_provider(std::sync::Arc::new(move || {
            trusted_provider.load(Ordering::SeqCst)
        }));
        helper.inner.state.lock().unwrap().cached = Some(super::CachedValue {
            value: "existing-cache".into(),
            timestamp_ms: now_ms(),
        });

        helper.update_config("printf 'fresh-token'".encode_utf16().collect(), config);
        assert_eq!(
            helper.cached_proxy_authorization().as_deref(),
            Some("existing-cache"),
            "refreshing settings must retain the process-local cache"
        );

        assert!(helper.on_proxy_auth_challenge(None));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(helper.cached_proxy_authorization(), None);

        trusted.store(true, Ordering::SeqCst);
        assert!(helper.on_proxy_auth_challenge(None));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while helper.cached_proxy_authorization().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the live trust grant should allow the project helper to refresh");
        assert_eq!(
            helper.cached_proxy_authorization().as_deref(),
            Some("fresh-token")
        );
        restore_env("CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER", old_enabled);
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
