//! `/login` — interactive OAuth (PKCE) login flow.
//!
//! Locked display templates (`LingXi` UX, M5-11 T0 step 2 L9):
//!   - Success: `"Logged in as {email} (org: {org_id})."`
//!   - Failure prefix: `"Could not log in: "`

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::AuthHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// Injected managed-policy enforcement for the interactive `/login` flow
/// (parity 2.1.207 H-BIN-09). The engine host supplies an implementation that
/// reads the managed `forceLoginOrgUUID` org pin and checks the just-
/// authenticated account's resolved organization against it, returning
/// `Err(message)` — the byte-exact admin/validation denial to surface — when
/// login is forbidden.
///
/// Injected (rather than read here) because `command-core` must not depend on
/// the settings/enterprise crate: the SAME pin `/connect` enforces via
/// `EngineOAuthConnect` is resolved by the engine host, so both interactive
/// login surfaces apply one policy. Left `None` for hosts without a managed
/// policy tier (mobile, tests) ⇒ login stays unrestricted.
#[async_trait]
pub trait LoginOrgPolicy: Send + Sync {
    /// Check the authenticated account's org membership set against the managed
    /// `forceLoginOrgUUID` pin. `Ok(())` permits the login; `Err(message)`
    /// forbids it and carries the byte-exact denial to display.
    async fn check(&self, account_org_ids: &[String]) -> Result<(), String>;
}

/// `/login` handler — drives the [`AuthHandle::login`] flow.
#[derive(Clone)]
pub struct LoginHandler {
    auth: Arc<dyn AuthHandle>,
    /// Optional managed `forceLoginOrgUUID` enforcement (parity 2.1.207
    /// H-BIN-09). `None` ⇒ login unrestricted (no managed policy tier).
    org_policy: Option<Arc<dyn LoginOrgPolicy>>,
}

impl LoginHandler {
    /// Construct a `LoginHandler` bound to the given auth handle.
    #[must_use]
    pub fn new(auth: Arc<dyn AuthHandle>) -> Self {
        Self {
            auth,
            org_policy: None,
        }
    }

    /// Bind the managed `forceLoginOrgUUID` enforcement seam (parity 2.1.207
    /// H-BIN-09). A completed sign-in whose resolved org the policy forbids is
    /// REJECTED and its just-persisted credential rolled back, so `/login`
    /// enforces the SAME org pin `/connect` (`EngineOAuthConnect`) does.
    #[must_use]
    pub fn with_org_policy(mut self, policy: Arc<dyn LoginOrgPolicy>) -> Self {
        self.org_policy = Some(policy);
        self
    }
}

#[async_trait]
impl BuiltinCommandHandler for LoginHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::LOGIN_STARTED);
        match self.auth.login().await {
            Ok(info) => {
                // (H-BIN-09) Enforce the managed `forceLoginOrgUUID` org pin: a
                // completed sign-in whose resolved org is not permitted is
                // REJECTED, and its just-persisted credential rolled back, so the
                // user is never left authenticated to a forbidden org. The
                // account's resolved org (`LoginInfo.org_id`) is the single-member
                // membership set the pin is checked against — matching `/connect`.
                if let Some(policy) = &self.org_policy {
                    if let Err(denial) = policy.check(std::slice::from_ref(&info.org_id)).await {
                        // Best-effort rollback of the credential the handle wrote.
                        let _ = self.auth.logout().await;
                        telemetry::emit_command_failed(cmd_evt::LOGIN_FAILED, &denial);
                        // Surface the byte-exact admin/validation message VERBATIM
                        // (no "Could not log in:" wrapper), matching `/connect`.
                        return CommandResult::Done {
                            display: Some(denial),
                        };
                    }
                }
                telemetry::emit_command_completed(cmd_evt::LOGIN_COMPLETED, &info.org_id);
                CommandResult::Done {
                    display: Some(format!(
                        "Logged in as {} (org: {}).",
                        info.email, info.org_id
                    )),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::LOGIN_FAILED, &msg);
                CommandResult::Done {
                    display: Some(format!("Could not log in: {msg}")),
                }
            }
        }
    }
    fn name(&self) -> &str {
        "login"
    }
    fn description(&self) -> &str {
        core_description("login")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lingxi_core::host::{AuthError, LoginInfo};
    use std::sync::Mutex as StdMutex;

    /// Test double for [`AuthHandle`]. Exposes the same `Ok` / `Err`
    /// constructors as the test helpers in M5-11 T9.
    pub struct MockAuth {
        result: StdMutex<Result<LoginInfo, AuthError>>,
    }
    impl MockAuth {
        pub fn ok(info: LoginInfo) -> Self {
            Self {
                result: StdMutex::new(Ok(info)),
            }
        }
        pub fn err(e: AuthError) -> Self {
            Self {
                result: StdMutex::new(Err(e)),
            }
        }
    }
    #[async_trait]
    impl AuthHandle for MockAuth {
        async fn login(&self) -> Result<LoginInfo, AuthError> {
            self.result.lock().unwrap().clone()
        }
        async fn logout(&self) -> Result<(), AuthError> {
            Ok(())
        }
        async fn current_user(&self) -> Option<LoginInfo> {
            self.result.lock().unwrap().as_ref().ok().cloned()
        }
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "login".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success() {
        let m = Arc::new(MockAuth::ok(LoginInfo {
            email: "u@x.com".into(),
            org_id: "org_42".into(),
        }));
        let h = LoginHandler::new(m);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Logged in as u@x.com (org: org_42).");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn timeout_error() {
        let m = Arc::new(MockAuth::err(AuthError::Timeout));
        let h = LoginHandler::new(m);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Could not log in: login flow timed out");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn network_error_propagates() {
        let m = Arc::new(MockAuth::err(AuthError::Network("dns failure".into())));
        let h = LoginHandler::new(m);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Could not log in: network error: dns failure");
        } else {
            panic!();
        }
    }

    // ── forceLoginOrgUUID org-pin enforcement (H-BIN-09) ─────────────────────

    /// Auth double that records whether `logout` (the org-pin rollback) ran.
    struct RollbackAuth {
        info: LoginInfo,
        logged_out: std::sync::atomic::AtomicBool,
    }
    impl RollbackAuth {
        fn new(org_id: &str) -> Self {
            Self {
                info: LoginInfo {
                    email: "me@example.com".into(),
                    org_id: org_id.into(),
                },
                logged_out: std::sync::atomic::AtomicBool::new(false),
            }
        }
        fn was_logged_out(&self) -> bool {
            self.logged_out.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl AuthHandle for RollbackAuth {
        async fn login(&self) -> Result<LoginInfo, AuthError> {
            Ok(self.info.clone())
        }
        async fn logout(&self) -> Result<(), AuthError> {
            self.logged_out
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn current_user(&self) -> Option<LoginInfo> {
            None
        }
    }

    /// Policy double that echoes a fixed verdict, recording the org set it saw.
    struct FixedPolicy {
        verdict: Result<(), String>,
        seen: StdMutex<Vec<String>>,
    }
    impl FixedPolicy {
        fn deny(message: &str) -> Self {
            Self {
                verdict: Err(message.to_string()),
                seen: StdMutex::new(Vec::new()),
            }
        }
        fn permit() -> Self {
            Self {
                verdict: Ok(()),
                seen: StdMutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl LoginOrgPolicy for FixedPolicy {
        async fn check(&self, account_org_ids: &[String]) -> Result<(), String> {
            *self.seen.lock().unwrap() = account_org_ids.to_vec();
            self.verdict.clone()
        }
    }

    #[tokio::test]
    async fn org_pin_denies_and_rolls_back_verbatim() {
        // The managed pin forbids this account's org: `/login` surfaces the
        // byte-exact denial VERBATIM (no "Could not log in:" wrapper) and rolls
        // back the just-persisted credential.
        let auth = Arc::new(RollbackAuth::new("org_bad"));
        let policy = Arc::new(FixedPolicy::deny(
            "Your authentication token belongs to organization org_bad,\n\
but this machine requires organization org_ok.\n\n\
Please log in with a permitted organization: lingxi-cli auth login",
        ));
        let h = LoginHandler::new(auth.clone()).with_org_policy(policy.clone());
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "Your authentication token belongs to organization org_bad,\n\
but this machine requires organization org_ok.\n\n\
Please log in with a permitted organization: lingxi-cli auth login"
            );
        } else {
            panic!("expected a Done display");
        }
        assert!(
            auth.was_logged_out(),
            "a forbidden /login must roll back the credential"
        );
        // The policy checked the account's single resolved org.
        assert_eq!(*policy.seen.lock().unwrap(), vec!["org_bad".to_string()]);
    }

    #[tokio::test]
    async fn org_pin_permits_member_and_keeps_credential() {
        // A permitted org: the normal success line renders and no rollback runs.
        let auth = Arc::new(RollbackAuth::new("org_ok"));
        let h = LoginHandler::new(auth.clone()).with_org_policy(Arc::new(FixedPolicy::permit()));
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Logged in as me@example.com (org: org_ok).");
        } else {
            panic!("expected a Done display");
        }
        assert!(
            !auth.was_logged_out(),
            "a permitted /login is not rolled back"
        );
    }

    #[tokio::test]
    async fn no_org_policy_leaves_login_unrestricted() {
        // Without an injected policy (mobile / no managed tier) login is not
        // gated: any org signs in and nothing is rolled back.
        let auth = Arc::new(RollbackAuth::new("any_org"));
        let h = LoginHandler::new(auth.clone());
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Logged in as me@example.com (org: any_org).");
        } else {
            panic!("expected a Done display");
        }
        assert!(!auth.was_logged_out());
    }
}
