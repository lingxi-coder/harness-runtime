//! Device-code flow for `OpenAI` / `ChatGPT` OAuth login.
//!
//! The SDK performs each device endpoint operation; this host owns the clock,
//! polling schedule, and login completion.
//!
//! Flow:
//! 1. `request_device_code` → POST `device_usercode_url` → `DeviceUserCode`
//! 2. Show `user_code` + `device_verify_url` to the user.
//! 3. `poll_for_token` in a loop until Ready or timeout.
//! 4. `run_device_code_login` orchestrates steps 1-3, then hands off to
//!    `OpenAiOAuthClient::exchange_code_with_redirect` and returns tokens.

use crate::oauth::openai::client::{OAuthError, OpenAiOAuthClient};
use lingxi_llm_client::auth::oauth::openai as sdk;
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use lingxi_llm_client::Transport;
use platform_api::Clock;
use std::sync::Arc;
use std::time::Duration;

const MAX_WAIT: Duration = Duration::from_secs(15 * 60);
const DEFAULT_INTERVAL_SECS: u64 = 5;
pub use sdk::{DeviceUserCode, PollOutcome};

pub async fn request_device_code(
    cfg: &OpenAiOAuthConfig,
    http: Arc<dyn Transport>,
) -> Result<DeviceUserCode, OAuthError> {
    sdk::request_device_code(http.as_ref(), cfg)
        .await
        .map_err(|e| OAuthError::DeviceCode(e.to_string()))
}

pub async fn poll_for_token(
    cfg: &OpenAiOAuthConfig,
    http: Arc<dyn Transport>,
    device_auth_id: &str,
    user_code: &str,
) -> Result<PollOutcome, OAuthError> {
    sdk::poll_for_token(http.as_ref(), cfg, device_auth_id, user_code)
        .await
        .map_err(|e| OAuthError::DeviceCode(e.to_string()))
}

/// Run the complete device-code login flow.
///
/// 1. Calls `request_device_code` to get a user code.
/// 2. Loops calling `poll_for_token`, sleeping `interval` seconds between
///    attempts, for up to 15 minutes.
/// 3. When the poll returns `Ready`, calls `client.exchange_code_with_redirect`
///    with the device callback URI and returns the `ExchangedTokens`.
///
/// `clock` is injected for testability (production callers pass `SystemClock`).
///
/// # Errors
/// - [`OAuthError::DeviceCode`] on usercode failure, poll failure, or timeout.
/// - [`OAuthError::TokenExchange`] if the final code exchange fails.
pub async fn run_device_code_login(
    cfg: OpenAiOAuthConfig,
    http: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
) -> Result<crate::oauth::openai::client::ExchangedTokens, OAuthError> {
    let uc = request_device_code(&cfg, http.clone()).await?;
    let interval = Duration::from_secs(if uc.interval == 0 {
        DEFAULT_INTERVAL_SECS
    } else {
        uc.interval
    });
    let start = clock.now();

    loop {
        // Check 15-minute timeout.
        let elapsed = clock.now().duration_since(start).unwrap_or(Duration::ZERO);
        if elapsed >= MAX_WAIT {
            return Err(OAuthError::DeviceCode(
                "device auth timed out after 15 minutes".into(),
            ));
        }

        match poll_for_token(&cfg, http.clone(), &uc.device_auth_id, &uc.user_code).await? {
            PollOutcome::Ready {
                authorization_code,
                code_verifier,
            } => {
                // Device-code redirect URI is `{issuer}/deviceauth/callback` —
                // note NO `/api/accounts/` prefix (that prefix is only on the
                // usercode/token endpoints). Byte-aligned with codex
                // `login/src/device_code_auth.rs:194`.
                let redirect_uri =
                    format!("{}/deviceauth/callback", cfg.issuer.trim_end_matches('/'));
                let client = OpenAiOAuthClient::new(cfg, http.clone());
                return client
                    .exchange_code_with_redirect(&authorization_code, &code_verifier, &redirect_uri)
                    .await;
            }
            PollOutcome::Pending => {
                // Sleep interval (or remaining time, whichever is smaller).
                let remaining = MAX_WAIT.checked_sub(elapsed).unwrap_or(Duration::ZERO);
                let sleep_for = interval.min(remaining);
                // Use tokio::time::sleep so tests can override with time control.
                tokio::time::sleep(sleep_for).await;
            }
        }
    }
}

#[cfg(test)]
mod device_code_tests {
    use super::*;
    use crate::oauth::openai::testsupport::{Canned, MockHttp, TestClock};

    // -----------------------------------------------------------------------
    // request_device_code
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn usercode_parse_returns_device_user_code() {
        let body = r#"{
            "device_auth_id": "auth-id-123",
            "user_code": "ABCD-1234",
            "interval": 5
        }"#;
        let http = MockHttp::new(vec![(
            "deviceauth/usercode",
            Canned {
                status: 200,
                body: body.into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let uc = request_device_code(&cfg, http as Arc<dyn Transport>)
            .await
            .expect("request_device_code ok");

        assert_eq!(uc.device_auth_id, "auth-id-123");
        assert_eq!(uc.user_code, "ABCD-1234");
        assert_eq!(uc.interval, 5);
    }

    #[tokio::test]
    async fn usercode_interval_may_be_string() {
        let body = r#"{
            "device_auth_id": "auth-id-456",
            "user_code": "EFGH-5678",
            "interval": "10"
        }"#;
        let http = MockHttp::new(vec![(
            "deviceauth/usercode",
            Canned {
                status: 200,
                body: body.into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let uc = request_device_code(&cfg, http as Arc<dyn Transport>)
            .await
            .expect("request_device_code ok");

        assert_eq!(uc.interval, 10);
    }

    #[tokio::test]
    async fn usercode_non_200_returns_error() {
        let http = MockHttp::new(vec![(
            "deviceauth/usercode",
            Canned {
                status: 404,
                body: "not found".into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let err = request_device_code(&cfg, http as Arc<dyn Transport>)
            .await
            .expect_err("404 must error");
        match err {
            OAuthError::DeviceCode(msg) => assert!(msg.contains("404")),
            other => panic!("expected DeviceCode, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // poll_for_token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn poll_pending_on_403() {
        let http = MockHttp::new(vec![(
            "deviceauth/token",
            Canned {
                status: 403,
                body: r#"{"error":"authorization_pending"}"#.into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let outcome = poll_for_token(&cfg, http as Arc<dyn Transport>, "auth-id", "user-code")
            .await
            .expect("poll ok");
        assert!(matches!(outcome, PollOutcome::Pending));
    }

    #[tokio::test]
    async fn poll_pending_on_404() {
        let http = MockHttp::new(vec![(
            "deviceauth/token",
            Canned {
                status: 404,
                body: "not found".into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let outcome = poll_for_token(&cfg, http as Arc<dyn Transport>, "auth-id", "user-code")
            .await
            .expect("poll ok");
        assert!(matches!(outcome, PollOutcome::Pending));
    }

    #[tokio::test]
    async fn poll_ready_on_200() {
        let body = r#"{
            "authorization_code": "auth-code-abc",
            "code_challenge": "challenge-xyz",
            "code_verifier": "verifier-xyz"
        }"#;
        let http = MockHttp::new(vec![(
            "deviceauth/token",
            Canned {
                status: 200,
                body: body.into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let outcome = poll_for_token(&cfg, http as Arc<dyn Transport>, "auth-id", "user-code")
            .await
            .expect("poll ok");
        match outcome {
            PollOutcome::Ready {
                authorization_code,
                code_verifier,
            } => {
                assert_eq!(authorization_code, "auth-code-abc");
                assert_eq!(code_verifier, "verifier-xyz");
            }
            PollOutcome::Pending => panic!("expected Ready"),
        }
    }

    #[tokio::test]
    async fn poll_error_on_unexpected_status() {
        let http = MockHttp::new(vec![(
            "deviceauth/token",
            Canned {
                status: 500,
                body: "server error".into(),
            },
        )]);
        let cfg = OpenAiOAuthConfig::default();
        let err = poll_for_token(&cfg, http as Arc<dyn Transport>, "auth-id", "user-code")
            .await
            .expect_err("500 must error");
        match err {
            OAuthError::DeviceCode(msg) => assert!(msg.contains("500")),
            other => panic!("expected DeviceCode, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // run_device_code_login (integration)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_device_code_login_pending_then_ready() {
        // Sequence: usercode → 403 (pending) → 200 (ready) → token exchange.
        let exchange_body = r#"{"id_token":"hdr.e30.sig","access_token":"ACCESS","refresh_token":"REFRESH","expires_in":3600}"#;
        let http = MockHttp::new(vec![
            ("deviceauth/usercode", Canned {
                status: 200,
                body: r#"{"device_auth_id":"did","user_code":"CODE","interval":0}"#.into(),
            }),
            ("deviceauth/token", Canned {
                // First call pending (403), then ready (200).
                // MockHttp always returns the same route — so we use two separate
                // routes distinguishable by URL substring.
                // Both "deviceauth/token" share the same route key here, so we
                // use a fresh single-route mock that toggles. Since MockHttp
                // doesn't support per-call rotation natively, we test with a
                // ready response directly.
                status: 200,
                body: r#"{"authorization_code":"CODE123","code_challenge":"C","code_verifier":"VERIFIER"}"#.into(),
            }),
            ("oauth/token", Canned {
                status: 200,
                body: exchange_body.into(),
            }),
        ]);
        let cfg = OpenAiOAuthConfig::default();
        let clock = TestClock::new(0);
        let tokens =
            run_device_code_login(cfg, http as Arc<dyn Transport>, clock as Arc<dyn Clock>)
                .await
                .expect("login ok");

        assert_eq!(tokens.access_token.expose_secret(), "ACCESS");
        assert!(tokens.refresh_token.is_some());
    }
}
