//! Trusted context room shared by the tools in one model query.
//!
//! Claude Code 2.1.322 `Lco` computes the Read allowance from the larger of
//! measured assistant usage and the host's held context, plus successful Read
//! claims. Cloning a tool context must retain the same claim counter.

use std::sync::{Arc, Mutex};

use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// Exact text counting on the provider/account route owning a Read allowance.
/// Tools supply source text; the host owns request construction and credentials.
#[async_trait::async_trait]
pub trait ReadTokenCounter: Send + Sync {
    /// `None` and failures select the native local estimate. A successful zero
    /// count remains authoritative, matching JavaScript's nullish fallback.
    async fn count_read_tokens(&self, content: &Utf16JsonProjection)
        -> Result<Option<u64>, String>;
}

#[derive(Debug, Default)]
struct ReadContextRoomState {
    claimed: u64,
    // Keep outstanding ownership separate from the saturating permanent total.
    // Otherwise a sibling's saturated claim could be erased by a rollback.
    reserved: u128,
    latest_usage: u64,
}

impl ReadContextRoomState {
    fn occupied_claims(&self) -> u64 {
        self.claimed
            .saturating_add(u64::try_from(self.reserved).unwrap_or(u64::MAX))
    }
}

/// One admitted Read's unpublished claim. Dropping a failed or cancelled call
/// releases exactly this reservation; committing makes the claim permanent.
#[must_use = "dropping the claim releases its unpublished token reservation"]
#[derive(Debug)]
pub struct ReadTokenClaim<'a> {
    room: &'a ReadContextRoom,
    tokens: u64,
    committed: bool,
}

impl ReadTokenClaim<'_> {
    /// Keep this claim after the Read result is ready to return successfully.
    pub fn commit(mut self) {
        let mut state = self
            .room
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved -= u128::from(self.tokens);
        state.claimed = state.claimed.saturating_add(self.tokens);
        self.committed = true;
    }
}

impl Drop for ReadTokenClaim<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let mut state = self
                .room
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reserved -= u128::from(self.tokens);
        }
    }
}

/// A trusted Read counter with optional context room and shared accounting.
pub struct ReadContextRoom {
    threshold: Option<u64>,
    held: u64,
    state: Mutex<ReadContextRoomState>,
    route: Option<(String, Option<String>)>,
    counter: Option<Arc<dyn ReadTokenCounter>>,
}

impl std::fmt::Debug for ReadContextRoom {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadContextRoom")
            .field("threshold", &self.threshold)
            .field("held", &self.held)
            .field("state", &self.state)
            .field("route", &self.route)
            .field("has_counter", &self.counter.is_some())
            .finish()
    }
}

impl ReadContextRoom {
    /// Bind an allowance to a trusted compaction threshold and context estimate.
    #[must_use]
    pub fn new(threshold: u64, held: u64) -> Self {
        Self {
            threshold: Some(threshold),
            held,
            state: Mutex::new(ReadContextRoomState::default()),
            route: None,
            counter: None,
        }
    }

    /// Keep exact normal-cap counting on a resolved route whose context
    /// ceiling is unknown. This does not authorize a larger Read.
    #[must_use]
    pub fn counter_only() -> Self {
        Self {
            threshold: None,
            ..Self::new(0, 0)
        }
    }

    /// Whether the host established context room for `allow_large`.
    #[must_use]
    pub const fn has_context_room(&self) -> bool {
        self.threshold.is_some()
    }

    /// Pin the provider route that supplied the threshold. A tool-context model
    /// override cannot reuse an unrelated model's larger context allowance.
    #[must_use]
    pub fn with_route(mut self, model: String, profile: Option<String>) -> Self {
        self.route = Some((model, profile));
        self
    }

    /// Attach a trusted provider counter already pinned to this room's route.
    #[must_use]
    pub fn with_token_counter(mut self, counter: Arc<dyn ReadTokenCounter>) -> Self {
        self.counter = Some(counter);
        self
    }

    /// Refine the local estimate only above native `maxTokens / 4` admission.
    /// The caller owns cancellation of this future. No room-state lock is held
    /// over provider I/O; final admission still rechecks and claims atomically.
    pub async fn refine_tokens(
        &self,
        content: &Utf16JsonProjection,
        estimate: u64,
        max_tokens: u64,
    ) -> u64 {
        if estimate == 0 || estimate <= max_tokens / 4 {
            return estimate;
        }
        let Some(counter) = self.counter.as_ref() else {
            return estimate;
        };
        counter
            .count_read_tokens(content)
            .await
            .ok()
            .flatten()
            .unwrap_or(estimate)
    }

    /// Whether the current invocation still uses this allowance's route.
    #[must_use]
    pub fn matches_route(&self, model: &str, profile: Option<&str>) -> bool {
        self.route
            .as_ref()
            .is_none_or(|(expected_model, expected_profile)| {
                expected_model == model && expected_profile.as_deref() == profile
            })
    }

    /// Successful claims plus admitted Reads awaiting result publication.
    #[must_use]
    pub fn claimed(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .occupied_claims()
    }

    /// Native `max(0, floor((threshold - max(usage, held) - claimed) * .7))`.
    /// The floating multiplication intentionally matches JavaScript rounding.
    #[must_use]
    pub fn room_tokens(&self, latest_usage: u64) -> u64 {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.room_tokens_for(&state, latest_usage)
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn room_tokens_for(&self, state: &ReadContextRoomState, latest_usage: u64) -> u64 {
        let Some(threshold) = self.threshold else {
            return 0;
        };
        let occupied = latest_usage
            .max(state.latest_usage)
            .max(self.held)
            .saturating_add(state.occupied_claims());
        (threshold.saturating_sub(occupied) as f64 * 0.7).floor() as u64
    }

    /// Supply authoritative usage for the owning assistant response when the
    /// stream has made it available. Later snapshots cannot reduce a hold.
    pub fn observe_usage(&self, tokens: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.latest_usage = state.latest_usage.max(tokens);
    }

    /// Record a successful Read's counted output. Failed calls do not claim.
    pub fn claim(&self, tokens: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.claimed = state.claimed.saturating_add(tokens);
    }

    /// Atomically admit a completed Read against its refreshed Native token
    /// cap and account for it before the caller exposes its result. JavaScript
    /// executes this final cap-check and claim without yielding; the lock
    /// preserves that ordering when Rust runs sibling reads on separate threads.
    ///
    /// Native retains the ordinary default allowance when the available large-
    /// read room falls below it. Only the lifted allowance is contextual. A
    /// rejection returns the fresh per-call cap and leaves claims unchanged.
    ///
    /// # Errors
    /// Returns the refreshed cap when `tokens` would exceed it.
    pub fn try_claim(
        &self,
        tokens: u64,
        initial_max_tokens: u64,
        default_max_tokens: u64,
    ) -> Result<(), u64> {
        self.try_reserve(tokens, initial_max_tokens, default_max_tokens)
            .map(ReadTokenClaim::commit)
    }

    /// Atomically reserve a completed Read against the refreshed native cap.
    /// Siblings see this hold immediately; the owner commits only when its
    /// successful result is ready. Dropping the guard releases only its tokens
    /// and never changes authoritative usage or other siblings' claims.
    ///
    /// # Errors
    /// Returns the refreshed cap when `tokens` would exceed it.
    pub fn try_reserve(
        &self,
        tokens: u64,
        initial_max_tokens: u64,
        default_max_tokens: u64,
    ) -> Result<ReadTokenClaim<'_>, u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cap = initial_max_tokens.min(default_max_tokens.max(self.room_tokens_for(&state, 0)));
        if tokens > cap {
            return Err(cap);
        }
        state.reserved += u128::from(tokens);
        Ok(ReadTokenClaim {
            room: self,
            tokens,
            committed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadContextRoom, ReadTokenCounter};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn native_room_uses_larger_usage_then_subtracts_claims() {
        let room = ReadContextRoom::new(100_000, 10_000);
        assert_eq!(room.room_tokens(0), 62_999);
        assert_eq!(room.room_tokens(20_000), 56_000);
        room.claim(5_000);
        assert_eq!(room.room_tokens(20_000), 52_500);
        assert_eq!(room.room_tokens(100_000), 0);
    }

    #[test]
    fn cloned_contexts_share_all_concurrent_claims() {
        let room = Arc::new(ReadContextRoom::new(100_000, 10_000));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let same_query = room.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        same_query.claim(10);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(room.claimed(), 8_000);
        assert_eq!(room.room_tokens(0), 57_399);
        room.claim(u64::MAX);
        room.claim(1);
        assert_eq!(room.claimed(), u64::MAX);
        assert_eq!(room.room_tokens(0), 0);
    }

    #[test]
    fn model_or_profile_change_cannot_reuse_the_previous_room() {
        let room = ReadContextRoom::new(100_000, 0)
            .with_route("claude-opus-5".into(), Some("account-a".into()));
        assert!(room.matches_route("claude-opus-5", Some("account-a")));
        assert!(!room.matches_route("claude-opus-5", None));
        assert!(!room.matches_route("claude-sonnet-4-6", Some("account-a")));
    }

    #[test]
    fn sibling_large_reads_recheck_and_claim_in_one_admission() {
        let room = Arc::new(ReadContextRoom::new(100_000, 10_000));
        let ready = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let room = room.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    let initial_cap = room.room_tokens(0);
                    ready.wait(); // Every sibling has observed the same old room.
                    room.try_claim(50_000, initial_cap, 25_000)
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|cap| *cap == 28_000));
        assert_eq!(room.claimed(), 50_000);
        assert_eq!(room.room_tokens(0), 28_000);
    }

    #[test]
    fn final_admission_observes_usage_and_retains_the_default_allowance() {
        let room = ReadContextRoom::new(100_000, 10_000);
        let initial_cap = room.room_tokens(0);
        room.observe_usage(95_000);
        assert_eq!(room.try_claim(50_000, initial_cap, 25_000), Err(25_000));
        assert_eq!(room.claimed(), 0);
        assert_eq!(room.try_claim(25_000, initial_cap, 25_000), Ok(()));
        assert_eq!(room.claimed(), 25_000);
    }

    struct Counter {
        calls: AtomicUsize,
        outcome: Result<Option<u64>, String>,
    }

    #[async_trait::async_trait]
    impl ReadTokenCounter for Counter {
        async fn count_read_tokens(
            &self,
            content: &Utf16JsonProjection,
        ) -> Result<Option<u64>, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(content.string_units(""), Some(vec![b'a' as u16, 0xd800]));
            self.outcome.clone()
        }
    }

    #[tokio::test]
    async fn exact_counter_obeys_native_band_and_fallback_without_losing_utf16() {
        let content = Utf16JsonProjection::parse(r#""a\ud800""#).unwrap();
        for outcome in [
            Ok(Some(99)),
            Ok(Some(0)),
            Ok(None),
            Err("unavailable".into()),
        ] {
            let counter = Arc::new(Counter {
                calls: AtomicUsize::new(0),
                outcome: outcome.clone(),
            });
            let room = ReadContextRoom::new(100_000, 0).with_token_counter(counter.clone());
            assert_eq!(room.refine_tokens(&content, 0, 100).await, 0);
            assert_eq!(room.refine_tokens(&content, 25, 100).await, 25);
            assert_eq!(counter.calls.load(Ordering::Relaxed), 0);
            assert_eq!(
                room.refine_tokens(&content, 26, 100).await,
                outcome.ok().flatten().unwrap_or(26)
            );
            assert_eq!(counter.calls.load(Ordering::Relaxed), 1);
            assert_eq!(room.claimed(), 0, "counting cannot publish a Read claim");
        }
        let offline = ReadContextRoom::new(100_000, 0);
        assert_eq!(offline.refine_tokens(&content, 26, 100).await, 26);
    }

    #[tokio::test]
    async fn counter_only_refines_exact_text_without_authorizing_context_room() {
        let counter = Arc::new(Counter {
            calls: AtomicUsize::new(0),
            outcome: Ok(Some(99)),
        });
        let room = ReadContextRoom::counter_only().with_token_counter(counter.clone());
        assert!(!room.has_context_room());
        assert!(ReadContextRoom::new(0, 0).has_context_room());
        assert_eq!(room.room_tokens(0), 0);
        let content = Utf16JsonProjection::parse(r#""a\ud800""#).unwrap();
        assert_eq!(room.refine_tokens(&content, 26, 100).await, 99);
        assert_eq!(counter.calls.load(Ordering::Relaxed), 1);
        assert_eq!(room.claimed(), 0);
        assert_eq!(room.try_claim(25_001, 60_000, 25_000), Err(25_000));
        assert_eq!(room.claimed(), 0);
    }

    #[test]
    fn reservation_rollback_retains_committed_siblings_even_after_saturation() {
        let room = ReadContextRoom::new(u64::MAX, 0);
        let held = room.try_reserve(1_000, 10_000, 10_000).unwrap();
        room.claim(u64::MAX);
        assert_eq!(room.claimed(), u64::MAX);
        drop(held);
        assert_eq!(room.claimed(), u64::MAX);
        assert_eq!(room.room_tokens(0), 0);
    }

    #[test]
    fn reservation_commit_and_rollback_each_change_only_their_owned_claim() {
        let room = ReadContextRoom::new(100_000, 0);
        let first = room.try_reserve(10_000, 70_000, 25_000).unwrap();
        let second = room.try_reserve(20_000, 70_000, 25_000).unwrap();
        assert_eq!(room.claimed(), 30_000);
        second.commit();
        assert_eq!(room.claimed(), 30_000);
        drop(first);
        assert_eq!(room.claimed(), 20_000);
        let third = room.try_reserve(7_000, 70_000, 25_000).unwrap();
        assert_eq!(room.claimed(), 27_000);
        drop(third);
        assert_eq!(room.claimed(), 20_000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_result_after_exact_count_and_admission_releases_only_its_reservation() {
        let counter = Arc::new(Counter {
            calls: AtomicUsize::new(0),
            outcome: Ok(Some(50_000)),
        });
        let room = Arc::new(ReadContextRoom::new(100_000, 10_000).with_token_counter(counter));
        room.claim(5_000);
        let (admitted, observed) = tokio::sync::oneshot::channel();
        let task_room = room.clone();
        let invocation = tokio::spawn(async move {
            let source = Utf16JsonProjection::parse(r#""a\ud800""#).unwrap();
            let tokens = task_room.refine_tokens(&source, 26, 100).await;
            let _unpublished = task_room
                .try_reserve(tokens, task_room.room_tokens(0), 25_000)
                .unwrap();
            admitted.send(()).unwrap();
            // A deterministic result-publication boundary, equivalent to a
            // sink awaiting completion after the Read's count was admitted.
            std::future::pending::<()>().await;
        });
        observed.await.unwrap();
        assert_eq!(room.claimed(), 55_000);
        room.observe_usage(20_000);
        room.try_claim(7_000, 25_000, 25_000).unwrap();
        assert_eq!(room.claimed(), 62_000);
        invocation.abort();
        assert!(invocation.await.unwrap_err().is_cancelled());
        assert_eq!(room.claimed(), 12_000);
        assert_eq!(room.room_tokens(0), 47_600);
        room.observe_usage(1);
        assert_eq!(
            room.room_tokens(0),
            47_600,
            "rollback cannot lower observed usage"
        );
    }
}
