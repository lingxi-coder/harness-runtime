//! Provider-neutral host OAuth refresh coordination.
//!
//! The caller holds the single-flight mutex across this preflight, its token
//! endpoint request, the in-memory swap, and persistence. Provider policy stays
//! with each driver.

use crate::LlmError;
use protocol::Secret;
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;

/// A bearer returned by a host refresh driver.
#[derive(Debug)]
pub struct BearerToken(pub Secret<String>);

/// Hash of the access token observed before attempting refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash(pub [u8; 32]);

#[derive(Debug, Clone, Error)]
pub enum OAuthHookError {
    #[error("refresh failed: {0}")]
    RefreshFailed(String),
    #[error("token stale; reload from store")]
    TokenStale,
    #[error("provider unreachable: {0}")]
    ProviderUnreachable(String),
}

pub(crate) fn token_hash(access_token: &Secret<String>) -> TokenHash {
    let mut hash = Sha256::new();
    hash.update(access_token.expose_secret().as_bytes());
    TokenHash(hash.finalize().into())
}

/// Minimal token access needed to perform the double-check under a read lock.
pub(crate) trait RefreshableToken {
    fn access_token(&self) -> &Secret<String>;
    fn refresh_token(&self) -> Option<&Secret<String>>;
}

pub(crate) enum Preflight {
    AlreadyRotated(BearerToken),
    RefreshWith(Secret<String>),
}

/// Called only while the provider's refresh mutex is held. A waiting caller
/// receives the rotated bearer and never sends a second refresh request.
pub(crate) async fn preflight<T: RefreshableToken>(
    token: &RwLock<T>,
    previous: TokenHash,
) -> Result<Preflight, OAuthHookError> {
    let current = token.read().await;
    if token_hash(current.access_token()) != previous {
        return Ok(Preflight::AlreadyRotated(BearerToken(Secret::new(
            current.access_token().expose_secret().clone(),
        ))));
    }
    let refresh = current
        .refresh_token()
        .ok_or_else(|| OAuthHookError::RefreshFailed("no refresh_token in state".into()))?;
    Ok(Preflight::RefreshWith(Secret::new(
        refresh.expose_secret().clone(),
    )))
}

pub(crate) fn llm_error_for(error: &OAuthHookError) -> LlmError {
    match error {
        OAuthHookError::RefreshFailed(_) => LlmError::OAuthRefreshDead,
        OAuthHookError::TokenStale | OAuthHookError::ProviderUnreachable(_) => {
            LlmError::Authentication {
                message: String::new(),
            }
        }
    }
}

/// Wake at half the remaining token lifetime, capped at five minutes.
pub(crate) fn proactive_lead(remaining: Duration, cap: Duration) -> Duration {
    Duration::from_secs(remaining.as_secs() / 2).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Token {
        access: Secret<String>,
        refresh: Option<Secret<String>>,
    }
    impl RefreshableToken for Token {
        fn access_token(&self) -> &Secret<String> {
            &self.access
        }
        fn refresh_token(&self) -> Option<&Secret<String>> {
            self.refresh.as_ref()
        }
    }
    #[tokio::test]
    async fn recheck_returns_rotated_bearer_without_using_old_refresh_token() {
        let old = Secret::new("old".to_owned());
        let previous = token_hash(&old);
        let token = RwLock::new(Token {
            access: Secret::new("new".to_owned()),
            refresh: None,
        });
        match preflight(&token, previous).await.expect("rotated") {
            Preflight::AlreadyRotated(bearer) => assert_eq!(bearer.0.expose_secret(), "new"),
            Preflight::RefreshWith(_) => panic!("would refresh with stale token"),
        }
    }

    #[tokio::test]
    async fn matching_hash_uses_current_refresh_token() {
        let token = RwLock::new(Token {
            access: Secret::new("current".to_owned()),
            refresh: Some(Secret::new("refresh".to_owned())),
        });
        let previous = token_hash(&token.read().await.access);
        match preflight(&token, previous).await.expect("refresh token") {
            Preflight::RefreshWith(refresh) => assert_eq!(refresh.expose_secret(), "refresh"),
            Preflight::AlreadyRotated(_) => panic!("hash should still match"),
        }
    }

    #[tokio::test]
    async fn missing_refresh_token_is_a_dead_session_when_hash_matches() {
        let token = RwLock::new(Token {
            access: Secret::new("current".to_owned()),
            refresh: None,
        });
        let previous = token_hash(&token.read().await.access);
        assert!(matches!(
            preflight(&token, previous).await,
            Err(OAuthHookError::RefreshFailed(_))
        ));
    }
}

pub(crate) async fn emit_refresh_started(
    bus: &Option<std::sync::Arc<telemetry::AnalyticsBus>>,
    event: &'static str,
    trigger: &str,
) {
    let Some(bus) = bus else { return };
    let mut metadata = telemetry::sink::LogEventMetadata::new();
    metadata.insert(
        "trigger".into(),
        telemetry::sink::AnalyticsValue::String(trigger.to_owned()),
    );
    bus.log_event(event, metadata).await;
}

pub(crate) async fn emit_refresh_succeeded(
    bus: &Option<std::sync::Arc<telemetry::AnalyticsBus>>,
    event: &'static str,
    trigger: &str,
    new_expiry_unix: i64,
    duration_ms: u64,
) {
    let Some(bus) = bus else { return };
    let mut metadata = telemetry::sink::LogEventMetadata::new();
    metadata.insert(
        "trigger".into(),
        telemetry::sink::AnalyticsValue::String(trigger.to_owned()),
    );
    metadata.insert(
        "new_expiry_unix".into(),
        telemetry::sink::AnalyticsValue::Int(new_expiry_unix),
    );
    metadata.insert(
        "duration_ms".into(),
        telemetry::sink::AnalyticsValue::Int(i64::try_from(duration_ms).unwrap_or(i64::MAX)),
    );
    bus.log_event(event, metadata).await;
}

pub(crate) async fn emit_refresh_failed(
    bus: &Option<std::sync::Arc<telemetry::AnalyticsBus>>,
    event: &'static str,
    trigger: &str,
    error_kind: &str,
) {
    let Some(bus) = bus else { return };
    let mut metadata = telemetry::sink::LogEventMetadata::new();
    metadata.insert(
        "trigger".into(),
        telemetry::sink::AnalyticsValue::String(trigger.to_owned()),
    );
    metadata.insert(
        "error_kind".into(),
        telemetry::sink::AnalyticsValue::String(error_kind.to_owned()),
    );
    bus.log_event(event, metadata).await;
}

pub(crate) async fn emit_proactive_canceled(
    bus: &Option<std::sync::Arc<telemetry::AnalyticsBus>>,
    event: &'static str,
    reason: &str,
) {
    let Some(bus) = bus else { return };
    let mut metadata = telemetry::sink::LogEventMetadata::new();
    metadata.insert(
        "reason".into(),
        telemetry::sink::AnalyticsValue::String(reason.to_owned()),
    );
    bus.log_event(event, metadata).await;
}
