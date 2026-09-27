//! One refusal-fallback hop, decided the same way for every turn loop.
//!
//! claude-code runs subagents through the SAME query generator as the main
//! thread, so its refusal cascade is shared by construction. This port has two
//! loops — `orchestrator`'s and the `agent` crate's subagent runner — and the
//! cascade lived only in the first, which is why a refusing subagent simply
//! ended its run.
//!
//! This bundles the parts that must behave identically in both: which model to
//! hop to ([`crate::refusal_cascade`]), the once-per-session latch, the
//! already-tried set, and the notice accumulate/collapse pair
//! ([`crate::refusal_notice`]). What it deliberately does NOT own is anything
//! host-shaped — swapping the model, running post-switch hooks, writing the
//! transcript frame — because those differ between the two loops and are the
//! caller's to do.

use crate::refusal_cascade::{
    decline_reports, route_refusal, DeclineReason, RefusalRoute, RouteInputs,
};
use crate::refusal_notice::{EmittedNotice, NoticeQueue, RefusalEpisode, RefusalNotice};

/// One accepted hop: where to go, and what the user should be told now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeHop {
    /// The model to serve the retry from.
    pub fallback_model: String,
    /// Notices ready to surface. Empty when this hop is held provisionally
    /// because a later hop may supersede it.
    pub notices: Vec<EmittedNotice>,
    /// Stages the walk passed over, for `tengu_refusal_fallback_route_declined`.
    pub declines: Vec<DeclineReason>,
}

/// The per-session (or per-subagent-run) cascade state.
#[derive(Debug, Default)]
pub struct RefusalCascadeState {
    tried: Vec<String>,
    latched: bool,
    episode: RefusalEpisode,
    queue: NoticeQueue,
}

impl RefusalCascadeState {
    /// Take the next hop away from `current_model`, or `None` when the cascade
    /// is exhausted, latched, or unconfigured.
    ///
    /// `chain` is the ordered fallback chain, supplied per call because each
    /// loop reads it from its own config. A single configured fallback model is
    /// exactly a one-element chain, which is why the historical single-model
    /// path needs no separate branch.
    ///
    /// `uuid` identifies this hop's notice; the caller supplies it so the same
    /// value can be stamped on whatever it writes to its transcript.
    pub fn next_hop(
        &mut self,
        chain: &[String],
        current_model: &str,
        uuid: String,
    ) -> Option<CascadeHop> {
        if chain.is_empty() {
            return None;
        }
        let tried = self.tried.clone();
        let route = route_refusal(
            &RouteInputs {
                chain: Some(chain),
                armed_fallback_model: None,
                armed_target_is_refusing_model: false,
                catch_all_enabled: false,
            },
            // A stage is reachable when this episode has not already routed to
            // it. The exclusion is `triedModels`, NOT "differs from the current
            // model": after a hop the current model IS the previous fallback,
            // and excluding it would stop a cleared session reaching it again.
            |stage| (!tried.iter().any(|m| m == stage)).then(|| stage.to_string()),
        );
        let declines = decline_reports(&route);
        let RefusalRoute::Category { stage, .. } = route else {
            return None;
        };
        // Once-per-session latch — applies only to a SINGLE-hop chain, the
        // historical shape. A real cascade is bounded by the chain itself:
        // every hop is consumed by `tried`, so the walk terminates without the
        // latch needing to cap it.
        if chain.len() <= 1 {
            if self.latched {
                return None;
            }
            self.latched = true;
        }
        let more_hops_possible = !stage.remaining_chain.is_empty();
        let fallback_model = stage.model;
        self.tried.push(fallback_model.clone());

        self.episode.merge(RefusalNotice {
            uuid: uuid.clone(),
            origin_model: current_model.to_string(),
            serving_model: fallback_model.clone(),
            ..RefusalNotice::default()
        });
        // A hop a LATER hop may supersede must not reach the user: "switched to
        // X" stops being true the moment the cascade moves past X. Hold it
        // provisionally and let the settling notice report the collapse.
        let taken = if more_hops_possible {
            self.episode.take_provisional(&uuid)
        } else {
            self.episode.settle()
        };
        let notices = match taken {
            Some(notice) => self.queue.accept(notice, more_hops_possible),
            None => Vec::new(),
        };
        Some(CascadeHop {
            fallback_model,
            notices,
            declines,
        })
    }

    /// Reset routing for a new session.
    ///
    /// Clears the latch and the tried set — and deliberately NOT the episode or
    /// the collapse queue, matching what `reset_refusal_fallback` and the
    /// resume path already do here. Whether a pending notice SHOULD survive a
    /// session clear is a real question, but changing it is its own change with
    /// its own gate.
    pub fn reset_routing(&mut self) {
        self.tried.clear();
        self.latched = false;
    }

    /// Whether the once-per-session latch has fired.
    #[must_use]
    pub fn is_latched(&self) -> bool {
        self.latched
    }
}
