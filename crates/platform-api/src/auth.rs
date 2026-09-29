//! Auth surface used by `/login` and `/logout` slash commands.
//!
//! Implemented by `lingxi-anthropic-oauth::handle::OAuthHandle`. The trait
//! lives in `platform-api` so `lingxi-commands` does not take a direct dep
//! on the oauth crate (decoupling). See plan M5-11 Task 3.

use async_trait::async_trait;
use thiserror::Error;

/// Successful-login payload returned by [`AuthHandle::login`] and
/// [`AuthHandle::current_user`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginInfo {
    /// Email address identifying the signed-in user.
    pub email: String,
    /// Anthropic organization id (`"org_..."` prefix).
    pub org_id: String,
}

/// Errors returned by [`AuthHandle`] methods.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// Login flow exceeded its 60-second deadline.
    #[error("login flow timed out")]
    Timeout,
    /// User cancelled the login flow (closed the browser / pressed Cancel).
    #[error("user cancelled login")]
    Cancelled,
    /// Network-layer error.
    #[error("network error: {0}")]
    Network(String),
    /// Server-side rejection (4xx/5xx response with a reason).
    #[error("server rejected: {0}")]
    ServerError(String),
}

/// Public auth surface — interactive sign-in / sign-out + current-user
/// snapshot. The concrete impl wraps `lingxi-anthropic-oauth`'s
/// SDK OAuth operations plus the host secret-storage layer.
#[async_trait]
pub trait AuthHandle: Send + Sync {
    /// Run an interactive OAuth code-flow (PKCE) login. Blocks until the
    /// user completes the browser flow + redirects back, or the timeout
    /// elapses.
    async fn login(&self) -> Result<LoginInfo, AuthError>;

    /// Clear stored credentials. Idempotent.
    async fn logout(&self) -> Result<(), AuthError>;

    /// Snapshot the current logged-in user (if any). Used by `/status`.
    async fn current_user(&self) -> Option<LoginInfo>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn _auth_handle_is_object_safe() {
        let _: Option<Box<dyn AuthHandle>> = None;
    }

    #[test]
    fn auth_error_display_timeout() {
        let e = AuthError::Timeout;
        assert_eq!(e.to_string(), "login flow timed out");
    }

    #[test]
    fn auth_error_display_network() {
        let e = AuthError::Network("dns".into());
        assert_eq!(e.to_string(), "network error: dns");
    }

    #[test]
    fn auth_error_display_cancelled() {
        let e = AuthError::Cancelled;
        assert_eq!(e.to_string(), "user cancelled login");
    }

    #[test]
    fn auth_error_display_server() {
        let e = AuthError::ServerError("bad scopes".into());
        assert_eq!(e.to_string(), "server rejected: bad scopes");
    }

    #[test]
    fn login_info_clone_equality() {
        let a = LoginInfo {
            email: "u@x.com".into(),
            org_id: "org_1".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
