//! Engine implementations of the `/connect` seams (`command_api::builtins::connect`).
//!
//! [`EngineCredentialWriter`] bridges the command-layer `ConnectCredentialWriter`
//! onto a host secure-input port + the keychain (`CredentialManager::set_provider_key`).
//! The secure prompt is rendered by the tui; tests inject a mock prompt.

use async_trait::async_trait;
use command_api::builtins::{ConnectCredentialWriter, ConnectError};
use secret::CredentialManager;
use std::sync::Arc;

/// Host port for a masked secret prompt. The tui implements this against its
/// secure-input widget; headless/test callers inject a canned value.
#[async_trait]
pub trait SecureKeyPrompt: Send + Sync {
    /// Prompt the user (masked) for the API key of `provider_label`.
    /// Returns `None` if the user cancelled.
    async fn prompt(&self, provider_label: &str) -> Option<String>;
}

/// A headless no-op prompt (returns `None` — cancels). The default when no tui
/// prompt is wired (`DesktopConfig.connect_prompt == None`).
pub struct NoopKeyPrompt;
#[async_trait]
impl SecureKeyPrompt for NoopKeyPrompt {
    async fn prompt(&self, _provider_label: &str) -> Option<String> {
        None
    }
}

/// Engine writer: prompt via the host port, then persist under `credential_id`.
pub struct EngineCredentialWriter {
    credentials: Arc<CredentialManager>,
    prompt: Arc<dyn SecureKeyPrompt>,
}

impl EngineCredentialWriter {
    /// Construct over the shared credential manager + host prompt port.
    #[must_use]
    pub fn new(credentials: Arc<CredentialManager>, prompt: Arc<dyn SecureKeyPrompt>) -> Self {
        Self {
            credentials,
            prompt,
        }
    }
}

#[async_trait]
impl ConnectCredentialWriter for EngineCredentialWriter {
    async fn prompt_and_store_key(&self, credential_id: &str) -> Result<(), ConnectError> {
        let Some(key) = self.prompt.prompt(credential_id).await else {
            return Err(ConnectError::Cancelled);
        };
        if key.trim().is_empty() {
            return Err(ConnectError::Cancelled);
        }
        self.credentials
            .set_provider_key(credential_id, &key)
            .await
            .map_err(|e| ConnectError::Storage(e.to_string()))
    }
}

use command_api::builtins::{
    ChatGptConnectDriver, CopilotConnectDriver, CopilotConnectStep, OAuthConnectDriver,
};
use lingxi_core::host::AuthHandle;
use lingxi_core::settings::enterprise::{
    ForceLoginOrgPin, OrgMembershipCheck, check_org_membership,
};
use llm_runtime::auth::openai as openai_oauth;
use llm_runtime::services::sdk;
use sdk::auth::oauth::copilot::{CopilotLogin, DeviceCodeResponse, PollOutcome};
use sdk::transport::Transport;
use std::sync::Mutex as StdMutex;

/// Credential id under which the GitHub Copilot OAuth token is stored. Matches
/// the catalog preset's `profile_name`.
const COPILOT_CREDENTIAL_ID: &str = "github-copilot";

/// Sleep port so the poll loop is testable without real time.
#[async_trait]
pub trait PollSleeper: Send + Sync {
    /// Sleep for `secs` seconds before the next poll.
    async fn sleep_secs(&self, secs: u64);
}

/// Production sleeper backed by `tokio::time::sleep`.
pub struct TokioSleeper;
#[async_trait]
impl PollSleeper for TokioSleeper {
    async fn sleep_secs(&self, secs: u64) {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    }
}

/// Native best-effort browser opener for the Copilot device flow: shells the
/// platform "open" command (`open` on macOS, `xdg-open` on Linux, `cmd /c start`
/// on Windows), detached, ignoring any failure. Mirrors `mcp_on_authorization_url`'s
/// opener — there is no browser-open util in this workspace.
#[must_use]
pub fn native_browser_opener() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|url: &str| {
        tracing::info!(
            target: "lingxi::connect::copilot",
            verification_url = %url,
            "GitHub Copilot: opening the device sign-in page (or visit it manually):\n  {url}",
        );
        #[cfg(target_os = "macos")]
        let cmd: Option<(&str, &[&str])> = Some(("open", &[]));
        #[cfg(target_os = "linux")]
        let cmd: Option<(&str, &[&str])> = Some(("xdg-open", &[]));
        #[cfg(target_os = "windows")]
        let cmd: Option<(&str, &[&str])> = Some(("cmd", &["/c", "start", ""]));
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        let cmd: Option<(&str, &[&str])> = None;
        if let Some((program, prefix)) = cmd {
            let _ = std::process::Command::new(program)
                .args(prefix)
                .arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    })
}

/// Engine Copilot device-flow driver: owns the pure `CopilotLogin` state machine,
/// the sleep port, and the keychain. `begin` returns the displayable step (and
/// caches the `DeviceCodeResponse`); `poll_to_completion` runs the SlowDown/Pending
/// loop and stores the token on success.
pub struct EngineCopilotConnect {
    credentials: Arc<CredentialManager>,
    login: CopilotLogin,
    sleeper: Arc<dyn PollSleeper>,
    cached: StdMutex<Option<DeviceCodeResponse>>,
    /// Best-effort browser opener invoked by [`Self::begin`] with the GitHub
    /// device-verification URL, so selecting Copilot in `/connect` sends the
    /// user straight to the web sign-in. No-op default (tests / headless); the
    /// production `new()` wires [`native_browser_opener`].
    browser: Arc<dyn Fn(&str) + Send + Sync>,
}

impl EngineCopilotConnect {
    /// Production constructor over the shared SDK HTTP transport.
    #[must_use]
    pub fn new(credentials: Arc<CredentialManager>, transport: Arc<dyn Transport>) -> Self {
        Self::with_parts(
            credentials,
            CopilotLogin::new(
                transport,
                llm_runtime::auth::copilot::copilot_client_id(),
                llm_runtime::auth::copilot::COPILOT_USER_AGENT,
            ),
            Arc::new(TokioSleeper),
        )
        .with_browser(native_browser_opener())
    }

    /// Construct over an injected `CopilotLogin` + sleeper (test seam). The
    /// browser opener defaults to a no-op; production wires one via
    /// [`Self::with_browser`].
    #[must_use]
    pub fn with_parts(
        credentials: Arc<CredentialManager>,
        login: CopilotLogin,
        sleeper: Arc<dyn PollSleeper>,
    ) -> Self {
        Self {
            credentials,
            login,
            sleeper,
            cached: StdMutex::new(None),
            browser: Arc::new(|_| {}),
        }
    }

    /// Attach a browser opener invoked by `begin()` with the device-verification
    /// URL (best-effort; the screen still shows the URL as the manual fallback).
    #[must_use]
    pub fn with_browser(mut self, browser: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.browser = browser;
        self
    }
}

#[async_trait]
impl CopilotConnectDriver for EngineCopilotConnect {
    async fn begin(&self, domain: Option<&str>) -> Result<CopilotConnectStep, ConnectError> {
        let dc = self
            .login
            .begin(domain.unwrap_or(sdk::auth::oauth::copilot::DEFAULT_GITHUB_DOMAIN))
            .await
            .map_err(|e| ConnectError::Network(e.to_string()))?;
        let step = CopilotConnectStep {
            user_code: dc.user_code.clone(),
            verification_uri: dc.verification_uri.clone(),
        };
        *self.cached.lock().unwrap() = Some(dc);
        // Best-effort: launch the GitHub device-verification page so the user
        // signs in on the web. The `/connect` screen still shows the code + URL
        // as the copy-by-hand fallback if the browser fails to open.
        (self.browser)(&step.verification_uri);
        Ok(step)
    }

    async fn poll_to_completion(&self, _step: &CopilotConnectStep) -> Result<(), ConnectError> {
        let dc = self
            .cached
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ConnectError::Network("begin() was not called".to_string()))?;
        loop {
            match self
                .login
                .poll_once(&dc)
                .await
                .map_err(|e| ConnectError::Network(e.to_string()))?
            {
                PollOutcome::Success(secret) => {
                    // §10 frozen-crate exception accessor (Task 7).
                    let token = secret.token_for_storage().to_string();
                    return self
                        .credentials
                        .set_provider_key(COPILOT_CREDENTIAL_ID, &token)
                        .await
                        .map_err(|e| ConnectError::Storage(e.to_string()));
                }
                PollOutcome::Pending { interval_secs }
                | PollOutcome::SlowDown { interval_secs } => {
                    self.sleeper.sleep_secs(interval_secs).await;
                }
                PollOutcome::Failed { error } => return Err(ConnectError::DeviceFailed(error)),
            }
        }
    }
}

/// Engine implementation of [`ChatGptConnectDriver`]: drives the OpenAI
/// ChatGPT-account OAuth login (browser PKCE, device-code fallback) and
/// persists the tokens. Returns a human-facing success message including the
/// minted account id.
pub struct EngineChatGptConnect {
    handle: Arc<openai_oauth::handle::OpenAiOAuthHandle>,
}

impl EngineChatGptConnect {
    /// Production constructor: uses a real browser opener.
    #[must_use]
    pub fn new(handle: Arc<openai_oauth::OpenAiOAuthHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl ChatGptConnectDriver for EngineChatGptConnect {
    async fn connect(&self) -> Result<String, ConnectError> {
        let info = self
            .handle
            .login()
            .await
            .map_err(|e| ConnectError::Network(e.to_string()))?;
        let account = info.account_id.unwrap_or_else(|| "?".into());
        Ok(format!("Connected chatgpt (account {account})."))
    }
}

/// Engine implementation of [`OAuthConnectDriver`]: dispatches the TUI `/connect`
/// browser sign-in per provider to the existing backend — Anthropic Pro/Max via
/// the shared [`AuthHandle`] (the same flow `/login` runs), OpenAI ChatGPT via the
/// [`ChatGptConnectDriver`]. Those backends persist the tokens.
pub struct EngineOAuthConnect {
    auth: Arc<dyn AuthHandle>,
    chatgpt: Arc<dyn ChatGptConnectDriver>,
    /// Test seam: a fixed `forceLoginOrgUUID` pin. `None` in production — the pin
    /// is read fresh from managed settings at each login via
    /// [`crate::desktop::managed_force_login_org_pin`].
    org_pin_override: Option<ForceLoginOrgPin>,
}

impl EngineOAuthConnect {
    /// Construct over the Anthropic auth handle + the ChatGPT connect driver.
    #[must_use]
    pub fn new(auth: Arc<dyn AuthHandle>, chatgpt: Arc<dyn ChatGptConnectDriver>) -> Self {
        Self {
            auth,
            chatgpt,
            org_pin_override: None,
        }
    }

    /// (test seam) Force a fixed `forceLoginOrgUUID` pin instead of reading the
    /// managed policy tiers, so the org-membership enforcement is exercisable
    /// without a live managed-settings directory.
    #[cfg(test)]
    #[must_use]
    fn with_org_pin_override(mut self, pin: ForceLoginOrgPin) -> Self {
        self.org_pin_override = Some(pin);
        self
    }

    /// Resolve the `forceLoginOrgUUID` pin: the test override if set, else the
    /// live managed policy tiers.
    async fn resolve_org_pin(&self) -> ForceLoginOrgPin {
        match &self.org_pin_override {
            Some(p) => p.clone(),
            None => crate::desktop::managed_force_login_org_pin().await,
        }
    }
}

#[async_trait]
impl OAuthConnectDriver for EngineOAuthConnect {
    async fn login(&self, provider_id: &str) -> Result<String, ConnectError> {
        match provider_id {
            "anthropic" => match self.auth.login().await {
                // (H-BIN-09) Enforce the managed `forceLoginOrgUUID` org pin: a
                // completed sign-in whose resolved organization is not permitted
                // is REJECTED, and its just-persisted credential rolled back, so
                // the user is never left authenticated to a forbidden org. The
                // account's resolved org (`LoginInfo.org_id`) is the single-member
                // membership set the pin is checked against.
                Ok(info) => {
                    let pin = self.resolve_org_pin().await;
                    match check_org_membership(&pin, std::slice::from_ref(&info.org_id)) {
                        OrgMembershipCheck::Permitted => {
                            Ok(format!("Connected Anthropic ({}).", info.email))
                        }
                        OrgMembershipCheck::Denied(message) => {
                            // Best-effort rollback of the credential handle wrote.
                            let _ = self.auth.logout().await;
                            Err(ConnectError::LoginPolicyDenied(message))
                        }
                    }
                }
                Err(lingxi_core::host::AuthError::Cancelled) => Err(ConnectError::Cancelled),
                Err(e) => Err(ConnectError::Network(e.to_string())),
            },
            "openai-chatgpt" => self.chatgpt.connect().await,
            other => Err(ConnectError::Network(format!(
                "no browser sign-in for provider '{other}'"
            ))),
        }
    }
}

/// Managed `forceLoginOrgUUID` enforcement for the interactive `/login` slash
/// command (parity 2.1.207 H-BIN-09). The `/connect` picker enforces the pin
/// inline in [`EngineOAuthConnect::login`]; `/login` ([`command_api::builtins::LoginHandler`])
/// is a distinct surface driving the same Anthropic `auth` handle, so it gets
/// the SAME policy injected via [`command_api::builtins::LoginOrgPolicy`]. Reads the pin
/// fresh at each login from the managed policy tiers
/// ([`crate::desktop::managed_force_login_org_pin`]) and defers to the landed
/// [`check_org_membership`].
pub struct DesktopLoginOrgPolicy;

#[async_trait]
impl command_api::builtins::LoginOrgPolicy for DesktopLoginOrgPolicy {
    async fn check(&self, account_org_ids: &[String]) -> Result<(), String> {
        let pin = crate::desktop::managed_force_login_org_pin().await;
        match check_org_membership(&pin, account_org_ids) {
            OrgMembershipCheck::Permitted => Ok(()),
            OrgMembershipCheck::Denied(message) => Err(message),
        }
    }
}

#[cfg(test)]
mod oauth_connect_tests {
    use super::*;
    use lingxi_core::host::{AuthError, AuthHandle, LoginInfo};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct OkAuth;
    #[async_trait]
    impl AuthHandle for OkAuth {
        fn register_account_change_observer(
            &self,
            _observer: std::sync::Weak<dyn lingxi_core::host::auth::AccountChangeObserver>,
        ) {
        }

        async fn login(&self) -> Result<LoginInfo, AuthError> {
            Ok(LoginInfo {
                email: "me@example.com".into(),
                org_id: "org_1".into(),
            })
        }
        async fn logout(&self) -> Result<(), AuthError> {
            Ok(())
        }
        async fn current_user(&self) -> Option<LoginInfo> {
            None
        }
    }

    /// Auth double that returns a configurable `(email, org_id)` and records
    /// whether `logout` (the org-pin rollback) was called.
    struct RecordingAuth {
        email: String,
        org_id: String,
        logged_out: AtomicBool,
    }
    impl RecordingAuth {
        fn new(email: &str, org_id: &str) -> Self {
            Self {
                email: email.into(),
                org_id: org_id.into(),
                logged_out: AtomicBool::new(false),
            }
        }
        fn was_logged_out(&self) -> bool {
            self.logged_out.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl AuthHandle for RecordingAuth {
        fn register_account_change_observer(
            &self,
            _observer: std::sync::Weak<dyn lingxi_core::host::auth::AccountChangeObserver>,
        ) {
        }

        async fn login(&self) -> Result<LoginInfo, AuthError> {
            Ok(LoginInfo {
                email: self.email.clone(),
                org_id: self.org_id.clone(),
            })
        }
        async fn logout(&self) -> Result<(), AuthError> {
            self.logged_out.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn current_user(&self) -> Option<LoginInfo> {
            None
        }
    }
    struct CancelAuth;
    #[async_trait]
    impl AuthHandle for CancelAuth {
        fn register_account_change_observer(
            &self,
            _observer: std::sync::Weak<dyn lingxi_core::host::auth::AccountChangeObserver>,
        ) {
        }

        async fn login(&self) -> Result<LoginInfo, AuthError> {
            Err(AuthError::Cancelled)
        }
        async fn logout(&self) -> Result<(), AuthError> {
            Ok(())
        }
        async fn current_user(&self) -> Option<LoginInfo> {
            None
        }
    }
    struct OkChatGpt;
    #[async_trait]
    impl ChatGptConnectDriver for OkChatGpt {
        async fn connect(&self) -> Result<String, ConnectError> {
            Ok("Connected chatgpt (account a1).".to_string())
        }
    }

    #[tokio::test]
    async fn dispatches_by_provider_id() {
        // Unset pin override keeps the test hermetic (no managed-settings read).
        let d = EngineOAuthConnect::new(Arc::new(OkAuth), Arc::new(OkChatGpt))
            .with_org_pin_override(ForceLoginOrgPin::Unset);
        assert!(
            d.login("anthropic")
                .await
                .unwrap()
                .contains("me@example.com")
        );
        assert!(d.login("openai-chatgpt").await.unwrap().contains("chatgpt"));
        // Unknown provider → a clear error, never a panic.
        assert!(matches!(
            d.login("github-copilot").await,
            Err(ConnectError::Network(_))
        ));
    }

    #[tokio::test]
    async fn maps_anthropic_cancel_to_connect_cancelled() {
        let d = EngineOAuthConnect::new(Arc::new(CancelAuth), Arc::new(OkChatGpt));
        assert!(matches!(
            d.login("anthropic").await,
            Err(ConnectError::Cancelled)
        ));
    }

    // ── forceLoginOrgUUID org-pin enforcement (H-BIN-09) ─────────────────────

    #[tokio::test]
    async fn org_pin_permits_member_and_keeps_credential() {
        let auth = Arc::new(RecordingAuth::new("me@example.com", "org_ok"));
        let d = EngineOAuthConnect::new(auth.clone(), Arc::new(OkChatGpt)).with_org_pin_override(
            ForceLoginOrgPin::Pinned(vec!["org_ok".into(), "org_other".into()]),
        );
        assert_eq!(
            d.login("anthropic").await.unwrap(),
            "Connected Anthropic (me@example.com)."
        );
        assert!(
            !auth.was_logged_out(),
            "a permitted login is not rolled back"
        );
    }

    #[tokio::test]
    async fn org_pin_unset_is_unrestricted() {
        let auth = Arc::new(RecordingAuth::new("me@example.com", "any_org"));
        let d = EngineOAuthConnect::new(auth.clone(), Arc::new(OkChatGpt))
            .with_org_pin_override(ForceLoginOrgPin::Unset);
        assert!(
            d.login("anthropic")
                .await
                .unwrap()
                .contains("me@example.com")
        );
        assert!(!auth.was_logged_out());
    }

    #[tokio::test]
    async fn org_pin_denies_nonmember_and_rolls_back_byte_exact() {
        let auth = Arc::new(RecordingAuth::new("me@example.com", "org_bad"));
        let d = EngineOAuthConnect::new(auth.clone(), Arc::new(OkChatGpt))
            .with_org_pin_override(ForceLoginOrgPin::Pinned(vec!["org_ok".into()]));
        match d.login("anthropic").await {
            Err(ConnectError::LoginPolicyDenied(m)) => assert_eq!(
                m,
                "Your authentication token belongs to organization org_bad,\n\
but this machine requires organization org_ok.\n\n\
Please log in with a permitted organization: lingxi-cli auth login"
            ),
            other => panic!("expected LoginPolicyDenied, got {other:?}"),
        }
        assert!(
            auth.was_logged_out(),
            "a forbidden login must roll back the persisted credential"
        );
    }

    #[tokio::test]
    async fn org_pin_array_mismatch_uses_plural_clause() {
        let auth = Arc::new(RecordingAuth::new("me@example.com", "org_bad"));
        let d = EngineOAuthConnect::new(auth, Arc::new(OkChatGpt)).with_org_pin_override(
            ForceLoginOrgPin::Pinned(vec!["org_a".into(), "org_b".into()]),
        );
        match d.login("anthropic").await {
            Err(ConnectError::LoginPolicyDenied(m)) => assert!(
                m.contains("but this machine requires one of these organizations: org_a, org_b."),
                "{m}"
            ),
            other => panic!("expected LoginPolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn org_pin_empty_array_is_admin_error() {
        let auth = Arc::new(RecordingAuth::new("me@example.com", "org_bad"));
        let d = EngineOAuthConnect::new(auth.clone(), Arc::new(OkChatGpt))
            .with_org_pin_override(ForceLoginOrgPin::EmptyArray);
        match d.login("anthropic").await {
            Err(ConnectError::LoginPolicyDenied(m)) => assert_eq!(
                m,
                "forceLoginOrgUUID in managed settings is set to an empty array.\n\
No organizations are permitted. This is almost certainly a misconfiguration.\n\
Contact your administrator."
            ),
            other => panic!("expected LoginPolicyDenied, got {other:?}"),
        }
        assert!(auth.was_logged_out());
    }

    #[tokio::test]
    async fn org_pin_invalid_is_admin_error() {
        let auth = Arc::new(RecordingAuth::new("me@example.com", "org_bad"));
        let d = EngineOAuthConnect::new(auth, Arc::new(OkChatGpt))
            .with_org_pin_override(ForceLoginOrgPin::Invalid);
        match d.login("anthropic").await {
            Err(ConnectError::LoginPolicyDenied(m)) => assert_eq!(
                m,
                "\"forceLoginOrgUUID\" was present but invalid; no organization is permitted to log in until it is fixed."
            ),
            other => panic!("expected LoginPolicyDenied, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::{
        Clock, HttpTransport, SecureStorage, SecureStorageBackend, SecureStorageError,
    };
    use lingxi_core::types::SecureStorageData;
    use platform_posix::{PosixClock, PosixHttp};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    /// In-memory `(service, account) -> data` store. The real
    /// `platform_posix` secure storage backend is keychain/OS-backed and not
    /// available in the test harness, so the roundtrip test needs a real
    /// in-memory store; this mirrors the `MemStorage` double used by `secret`'s
    /// own credential tests.
    #[derive(Default)]
    struct MemStorage {
        map: StdMutex<HashMap<(String, String), SecureStorageData>>,
    }
    #[async_trait]
    impl SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), data);
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|(s, _)| s == service)
                .map(|(_, a)| a.clone())
                .collect())
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::PlainText
        }
    }

    struct CannedPrompt(Option<String>);
    #[async_trait]
    impl SecureKeyPrompt for CannedPrompt {
        async fn prompt(&self, _label: &str) -> Option<String> {
            self.0.clone()
        }
    }

    fn manager() -> Arc<CredentialManager> {
        let storage: Arc<dyn SecureStorage> = Arc::new(MemStorage::default());
        let clock: Arc<dyn Clock> = Arc::new(PosixClock::new());
        let http: Arc<dyn HttpTransport> = Arc::new(PosixHttp::new());
        Arc::new(CredentialManager::new(storage, clock, http))
    }

    #[tokio::test]
    async fn store_roundtrips_through_keychain() {
        let cm = manager();
        let writer = EngineCredentialWriter::new(
            cm.clone(),
            Arc::new(CannedPrompt(Some("sk-test-123".into()))),
        );
        writer
            .prompt_and_store_key("openrouter")
            .await
            .expect("store ok");
        let got = cm
            .get_provider_key("openrouter")
            .await
            .expect("read ok")
            .expect("present");
        assert_eq!(got.expose_secret(), "sk-test-123");
    }

    #[tokio::test]
    async fn cancelled_prompt_yields_cancelled() {
        let cm = manager();
        let writer = EngineCredentialWriter::new(cm, Arc::new(CannedPrompt(None)));
        match writer.prompt_and_store_key("deepseek").await {
            Err(ConnectError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_key_is_cancelled_not_stored() {
        let cm = manager();
        let writer =
            EngineCredentialWriter::new(cm.clone(), Arc::new(CannedPrompt(Some("   ".into()))));
        match writer.prompt_and_store_key("deepseek").await {
            Err(ConnectError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert!(
            cm.get_provider_key("deepseek")
                .await
                .expect("read")
                .is_none()
        );
    }

    use command_api::builtins::CopilotConnectDriver;
    use sdk::auth::oauth::copilot::{COPILOT_CLIENT_ID, CopilotLogin};
    use sdk::transport::{
        HttpRequest as SdkHttpRequest, HttpResponse as SdkHttpResponse, StreamResponse, Transport,
    };
    use serde_json::{Value, json};

    struct ScriptedCopilotTransport {
        device: Value,
        tokens: StdMutex<std::collections::VecDeque<Value>>,
    }
    #[async_trait]
    impl Transport for ScriptedCopilotTransport {
        async fn send(
            &self,
            request: SdkHttpRequest,
        ) -> Result<StreamResponse, sdk::protocol::LlmError> {
            let v = if request.url.contains("device/code") {
                self.device.clone()
            } else {
                self.tokens
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| json!({ "error": "expired_token" }))
            };
            Ok(SdkHttpResponse {
                status: 200,
                headers: vec![],
                body: serde_json::to_vec(&v).unwrap().into(),
            }
            .into())
        }
    }

    struct InstantSleeper;
    #[async_trait]
    impl PollSleeper for InstantSleeper {
        async fn sleep_secs(&self, _secs: u64) {}
    }

    fn copilot_driver(
        cm: Arc<CredentialManager>,
        device: Value,
        tokens: Vec<Value>,
    ) -> EngineCopilotConnect {
        let http = ScriptedCopilotTransport {
            device,
            tokens: StdMutex::new(tokens.into_iter().collect()),
        };
        EngineCopilotConnect::with_parts(
            cm,
            CopilotLogin::new(Arc::new(http), COPILOT_CLIENT_ID, "LingXi-Code"),
            Arc::new(InstantSleeper),
        )
    }

    #[tokio::test]
    async fn begin_surfaces_user_code_and_uri() {
        let cm = manager();
        let driver = copilot_driver(
            cm,
            json!({ "user_code": "WDJB-MJHT", "verification_uri": "https://github.com/login/device", "device_code": "dev-1", "interval": 1 }),
            vec![],
        );
        let step = driver.begin(None).await.expect("begin ok");
        assert_eq!(step.user_code, "WDJB-MJHT");
        assert_eq!(step.verification_uri, "https://github.com/login/device");
    }

    #[tokio::test]
    async fn poll_advances_through_pending_then_stores_token() {
        let cm = manager();
        let driver = copilot_driver(
            cm.clone(),
            json!({ "user_code": "AAAA-BBBB", "verification_uri": "https://github.com/login/device", "device_code": "dev-2", "interval": 1 }),
            vec![
                json!({ "error": "authorization_pending" }),
                json!({ "error": "slow_down" }),
                json!({ "access_token": "ght_live_token" }),
            ],
        );
        let step = driver.begin(None).await.expect("begin");
        driver.poll_to_completion(&step).await.expect("poll ok");
        let got = cm
            .get_provider_key("github-copilot")
            .await
            .expect("read")
            .expect("present");
        assert_eq!(got.expose_secret(), "ght_live_token");
    }

    #[tokio::test]
    async fn poll_terminal_error_is_surfaced_and_not_stored() {
        let cm = manager();
        let driver = copilot_driver(
            cm.clone(),
            json!({ "user_code": "CCCC-DDDD", "verification_uri": "https://github.com/login/device", "device_code": "dev-3", "interval": 1 }),
            vec![json!({ "error": "access_denied" })],
        );
        let step = driver.begin(None).await.expect("begin");
        match driver.poll_to_completion(&step).await {
            Err(ConnectError::DeviceFailed(e)) => assert_eq!(e, "access_denied"),
            other => panic!("expected DeviceFailed, got {other:?}"),
        }
        assert!(
            cm.get_provider_key("github-copilot")
                .await
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn uses_vscode_copilot_client_id() {
        assert_eq!(COPILOT_CLIENT_ID, "Iv1.b507a08c87ecfe98");
    }
}
