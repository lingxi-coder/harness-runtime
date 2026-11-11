//! Trusted context room shared by the tools in one model query.
//!
//! Claude Code 2.1.322 `Lco` computes the Read allowance from the larger of
//! measured assistant usage and the host's held context, plus successful Read
//! claims. Cloning a tool context must retain the same claim counter.

use std::sync::Mutex;

#[derive(Debug, Default)]
struct ReadContextRoomState {
    claimed: u64,
    latest_usage: u64,
}

/// A host-supplied context ceiling and shared successful-Read accounting.
#[derive(Debug)]
pub struct ReadContextRoom {
    threshold: u64,
    held: u64,
    state: Mutex<ReadContextRoomState>,
    route: Option<(String, Option<String>)>,
}

impl ReadContextRoom {
    /// Bind an allowance to a trusted compaction threshold and context estimate.
    #[must_use]
    pub fn new(threshold: u64, held: u64) -> Self {
        Self {
            threshold,
            held,
            state: Mutex::new(ReadContextRoomState::default()),
            route: None,
        }
    }

    /// Pin the provider route that supplied the threshold. A tool-context model
    /// override cannot reuse an unrelated model's larger context allowance.
    #[must_use]
    pub fn with_route(mut self, model: String, profile: Option<String>) -> Self {
        self.route = Some((model, profile));
        self
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

    /// Tokens already claimed by successful Read results in this query.
    #[must_use]
    pub fn claimed(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .claimed
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
        let occupied = latest_usage
            .max(state.latest_usage)
            .max(self.held)
            .saturating_add(state.claimed);
        (self.threshold.saturating_sub(occupied) as f64 * 0.7).floor() as u64
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cap = initial_max_tokens.min(default_max_tokens.max(self.room_tokens_for(&state, 0)));
        if tokens > cap {
            return Err(cap);
        }
        state.claimed = state.claimed.saturating_add(tokens);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ReadContextRoom;
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
}
