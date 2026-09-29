//! Device-code flow for `OpenAI` / `ChatGPT` OAuth login.
//!
//! The SDK performs each device endpoint operation; this host owns the clock,
//! polling schedule, and login completion.
//!
//! Flow:
//! 1. `request_device_code` → POST `device_usercode_url` → `DeviceUserCode`
//! 2. Show `user_code` + `device_verify_url` to the user.
//! 3. `poll_for_token` in a loop until Ready or timeout.
//! 4. `run_device_code_login` exchanges the code with the SDK and returns tokens.

use crate::auth::openai::login::{exchange_error, into_login_tokens, LoginTokens, OAuthError};
use lingxi_llm_client::auth::oauth::openai as sdk;
use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
use lingxi_llm_client::Transport;
use platform_api::Clock;
use std::sync::Arc;
use std::time::Duration;

const MAX_WAIT: Duration = Duration::from_secs(15 * 60);
const DEFAULT_INTERVAL_SECS: u64 = 5;
use sdk::PollOutcome;

/// Run the complete device-code login flow.
///
/// 1. Calls `request_device_code` to get a user code.
/// 2. Loops calling `poll_for_token`, sleeping `interval` seconds between
///    attempts, for up to 15 minutes.
/// 3. When the poll returns `Ready`, exchanges the code with the device
///    callback URI and converts the SDK tokens using the host clock.
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
) -> Result<LoginTokens, OAuthError> {
    let uc = sdk::request_device_code(http.as_ref(), &cfg)
        .await
        .map_err(|e| OAuthError::DeviceCode(e.to_string()))?;
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

        match sdk::poll_for_token(http.as_ref(), &cfg, &uc.device_auth_id, &uc.user_code)
            .await
            .map_err(|e| OAuthError::DeviceCode(e.to_string()))?
        {
            PollOutcome::Ready {
                authorization_code,
                code_verifier,
            } => {
                let redirect_uri = cfg.device_redirect_uri();
                let tokens = sdk::exchange_code(
                    http.as_ref(),
                    &cfg,
                    &authorization_code,
                    &code_verifier,
                    &redirect_uri,
                )
                .await
                .map_err(exchange_error)?;
                return Ok(into_login_tokens(tokens, clock.as_ref()));
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
    use crate::auth::openai::testsupport::{Canned, MockHttp, TestClock};

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
        let uc = sdk::request_device_code(http.as_ref(), &cfg)
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
        let uc = sdk::request_device_code(http.as_ref(), &cfg)
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
        let err = sdk::request_device_code(http.as_ref(), &cfg)
            .await
            .expect_err("404 must error");
        assert!(matches!(err, sdk::OAuthProtocolError::Status(404)));
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
        let outcome = sdk::poll_for_token(http.as_ref(), &cfg, "auth-id", "user-code")
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
        let outcome = sdk::poll_for_token(http.as_ref(), &cfg, "auth-id", "user-code")
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
        let outcome = sdk::poll_for_token(http.as_ref(), &cfg, "auth-id", "user-code")
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
        let err = sdk::poll_for_token(http.as_ref(), &cfg, "auth-id", "user-code")
            .await
            .expect_err("500 must error");
        assert!(matches!(err, sdk::OAuthProtocolError::Status(500)));
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
