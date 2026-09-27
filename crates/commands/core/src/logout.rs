//! `/logout` — clear stored OAuth credentials.
//!
//! Locked display templates (`LingXi` UX, M5-11 T0 step 2 L10):
//!   - Success: `"Logged out."`
//!   - Failure prefix: `"Could not log out: "`

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::AuthHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/logout` handler — drives [`AuthHandle::logout`].
#[derive(Clone)]
pub struct LogoutHandler {
    auth: Arc<dyn AuthHandle>,
}

impl LogoutHandler {
    /// Construct a `LogoutHandler` bound to the given auth handle.
    #[must_use]
    pub fn new(auth: Arc<dyn AuthHandle>) -> Self {
        Self { auth }
    }
}

#[async_trait]
impl BuiltinCommandHandler for LogoutHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::LOGOUT_STARTED);
        match self.auth.logout().await {
            Ok(()) => {
                telemetry::emit_command_completed(cmd_evt::LOGOUT_COMPLETED, "");
                CommandResult::Done {
                    display: Some("Logged out.".to_string()),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::LOGOUT_FAILED, &msg);
                CommandResult::Done {
                    display: Some(format!("Could not log out: {msg}")),
                }
            }
        }
    }
    fn name(&self) -> &str {
        "logout"
    }
    fn description(&self) -> &str {
        core_description("logout")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login::tests::MockAuth;
    use platform_api::{AuthError, LoginInfo};

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "logout".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success_returns_locked_literal() {
        let m = Arc::new(MockAuth::ok(LoginInfo {
            email: "u@x.com".into(),
            org_id: "org_42".into(),
        }));
        let h = LogoutHandler::new(m);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Logged out.");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn failure_prefixes_error() {
        // MockAuth.logout() always returns Ok; need a different mock for a failure case.
        struct FailingAuth;
        #[async_trait]
        impl AuthHandle for FailingAuth {
            async fn login(&self) -> Result<LoginInfo, AuthError> {
                Err(AuthError::Cancelled)
            }
            async fn logout(&self) -> Result<(), AuthError> {
                Err(AuthError::Network("disk full".into()))
            }
            async fn current_user(&self) -> Option<LoginInfo> {
                None
            }
        }
        let m = Arc::new(FailingAuth);
        let h = LogoutHandler::new(m);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Could not log out: network error: disk full");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let m = Arc::new(MockAuth::ok(LoginInfo {
            email: "u@x.com".into(),
            org_id: "org_42".into(),
        }));
        let h = LogoutHandler::new(m);
        assert_eq!(h.name(), "logout");
        assert_eq!(h.description(), "Sign out from your Anthropic account");
    }
}
