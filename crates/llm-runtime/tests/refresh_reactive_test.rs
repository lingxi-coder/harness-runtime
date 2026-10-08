//! Reactive refresh invoked directly on `RefreshDriver::refresh` (inherent).
//!
//! Locks the double-check-after-acquire behavior: two near-simultaneous calls
//! with the same `prev_token_hash` must collapse to one HTTP refresh.
//!
//! Loom version (full thread-interleaving exploration) lives in
//! `refresh_single_flight_test.rs` — this file is a tokio-level smoke test.

use async_trait::async_trait;
use lingxi_core::host::Clock;
use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::{HttpRequest, StreamResponse, Transport};
use llm_runtime::auth::anthropic::refresh::{AuthState, RefreshDriver};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Counting HTTP transport: every `send()` increments `calls`. Always returns
/// a fresh token in the JSON body.
struct CountingTransport {
    calls: Arc<AtomicU32>,
    new_access_token: String,
    new_refresh_token: String,
    expires_in_secs: u64,
}

#[async_trait]
impl Transport for CountingTransport {
    async fn send(
        &self,
        _req: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Simulate the IdP's token-endpoint JSON response.
        let body = format!(
            r#"{{"access_token":"{}","refresh_token":"{}","expires_in":{},"scope":"read:user write:messages read:projects"}}"#,
            self.new_access_token, self.new_refresh_token, self.expires_in_secs,
        );
        Ok(StreamResponse {
            status: 200,
            headers: vec![],
            body: Box::pin(futures::stream::once(async move {
                Ok(bytes::Bytes::from(body))
            })),
        })
    }
}

struct FixedClock(SystemTime);
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

fn make_state(initial_access: &str) -> (Arc<AuthState>, Arc<AtomicU32>) {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let calls = Arc::new(AtomicU32::new(0));
    let transport: Arc<dyn Transport> = Arc::new(CountingTransport {
        calls: calls.clone(),
        new_access_token: "FRESH_ACCESS".into(),
        new_refresh_token: "FRESH_REFRESH".into(),
        expires_in_secs: 3600,
    });
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    ));
    let state = AuthState::new(
        cfg,
        Secret::new(initial_access.to_string()),
        Some(Secret::new("INITIAL_REFRESH".to_string())),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_010),
        lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0).scopes, // expires in 10s
        transport,
        clock,
        None, // no AnalyticsBus in this smoke test
        None, // no CredentialManager — refresh.rs MUST handle Option (Task 4 step 4)
    );
    (state, calls)
}

#[tokio::test]
async fn refresh_with_matching_hash_makes_one_http_call() {
    let (state, calls) = make_state("INITIAL_ACCESS");
    let driver = RefreshDriver::new(state.clone());

    // Compute the current token's hash.
    let prev = state.token.read().await.token_hash();
    let result = driver.refresh(prev).await;
    assert!(result.is_ok(), "refresh should succeed: {result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one HTTP call");

    // Verify token rotated.
    let new_hash = state.token.read().await.token_hash();
    assert_ne!(new_hash, prev, "token must have rotated");
}

#[tokio::test]
async fn refresh_with_stale_hash_skips_http_call() {
    let (state, calls) = make_state("INITIAL_ACCESS");
    let driver = RefreshDriver::new(state.clone());

    // First refresh rotates the token.
    let prev_initial = state.token.read().await.token_hash();
    let _ = driver
        .refresh(prev_initial)
        .await
        .expect("first refresh ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Second call with the OLD hash → double-check-after-acquire sees the
    // hash mismatch and returns the now-current token without another HTTP call.
    let r = driver.refresh(prev_initial).await;
    assert!(
        r.is_ok(),
        "second refresh with stale hash should still return a token",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "stale-hash call must NOT trigger a second HTTP refresh",
    );
}

#[tokio::test]
async fn concurrent_refresh_with_same_hash_collapses_to_one_call() {
    let (state, calls) = make_state("INITIAL_ACCESS");
    let driver = Arc::new(RefreshDriver::new(state.clone()));
    let prev = state.token.read().await.token_hash();

    // Spawn three concurrent refresh attempts with the SAME prev_token_hash.
    let h1 = {
        let d = driver.clone();
        tokio::spawn(async move { d.refresh(prev).await })
    };
    let h2 = {
        let d = driver.clone();
        tokio::spawn(async move { d.refresh(prev).await })
    };
    let h3 = {
        let d = driver.clone();
        tokio::spawn(async move { d.refresh(prev).await })
    };

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();
    let r3 = h3.await.unwrap();
    assert!(r1.is_ok() && r2.is_ok() && r3.is_ok());
    // Exactly ONE call escaped the single-flight lock + double-check.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "concurrent same-hash refresh must collapse to one HTTP call",
    );
}
