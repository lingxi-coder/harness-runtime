//! One refusal-fallback hop, decided the same way for every turn loop.
//!
//! claude-code runs subagents through the SAME query generator as the main
//! thread, so its refusal cascade is shared by construction. This port has two
//! loops — `orchestrator`'s and the `agent` crate's subagent runner — and the
//! cascade lived only in the first, which is why a refusing subagent simply
//! ended its run.
//!
//! This bundles the parts that must behave identically in both: which model to
//! hop to ([`crate::host::refusal_cascade`]), the once-per-session latch, the
//! already-tried set, and the notice accumulate/collapse pair
//! ([`crate::host::refusal_notice`]). What it deliberately does NOT own is anything
//! host-shaped — swapping the model, running post-switch hooks, writing the
//! transcript frame — because those differ between the two loops and are the
//! caller's to do.

use crate::host::refusal_cascade::{
    decline_reports, route_refusal, DeclineReason, RefusalRoute, RouteInputs,
};
use crate::host::refusal_notice::{EmittedNotice, NoticeQueue, RefusalEpisode, RefusalNotice};

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
    active_target: Option<String>,
    episode: RefusalEpisode,
    queue: NoticeQueue,
}

impl RefusalCascadeState {
    /// The accepted local cascade target, supplied as trusted query context.
    pub fn target_model(&self) -> Option<&str> {
        self.active_target.as_deref()
    }

    /// A user model pick drops target ownership without rearming the episode's
    /// refusal routing. Native `oae` unsets only the model-selection latch.
    pub fn clear_target(&mut self) {
        self.active_target = None;
    }
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
        self.active_target = Some(fallback_model.clone());

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
        self.clear_target();
    }

    /// Whether the once-per-session latch has fired.
    #[must_use]
    pub fn is_latched(&self) -> bool {
        self.latched
    }
}

/// Native `$Fo` inputs captured by the query host, outside provider input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FallbackTargetContext {
    pub server_fallback: Option<crate::host::refusal_server::QueryPolicy>,
    pub user_model: String,
    pub turn_override: Option<String>,
    pub chain_step_model: Option<String>,
    /// Native `P1e` accepts a latch only while the raw main-loop override still
    /// equals its model. Identity equivalence alone cannot keep a stale latch.
    pub latched_model: Option<String>,
    pub main_loop_override: Option<String>,
    pub refusal_header_armed: bool,
    pub refusal_occurred: bool,
    pub refusal_lane_enabled: bool,
    pub refusal_origin_request_id: Option<String>,
}

impl FallbackTargetContext {
    pub fn is_target(&self, request_model: &str, identity: impl Fn(&str) -> String) -> bool {
        let request = identity(request_model);
        (identity(&self.user_model) != request
            && [&self.turn_override, &self.chain_step_model]
                .into_iter()
                .flatten()
                .any(|model| identity(model) == request))
            || self.latched_model.as_ref().is_some_and(|model| {
                self.main_loop_override.as_ref() == Some(model) && identity(model) == request
            })
    }
}

tokio::task_local! {
    static QUERY_FALLBACK_TARGET: FallbackTargetContext;
}

pub async fn scope_fallback_target<F: std::future::Future>(
    context: FallbackTargetContext,
    future: F,
) -> F::Output {
    QUERY_FALLBACK_TARGET.scope(context, future).await
}

pub fn current_fallback_target() -> Option<FallbackTargetContext> {
    QUERY_FALLBACK_TARGET.try_with(Clone::clone).ok()
}

#[cfg(test)]
mod target_context_tests {
    use super::*;
    #[tokio::test]
    async fn accepted_cascade_target_is_scoped_to_the_query_and_reset_removes_it() {
        let mut cascade = RefusalCascadeState::default();
        assert_eq!(cascade.target_model(), None);
        cascade
            .next_hop(&["target".into()], "origin", "notice".into())
            .unwrap();
        assert_eq!(cascade.target_model(), Some("target"));
        let context = FallbackTargetContext {
            user_model: "origin".into(),
            turn_override: cascade.target_model().map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(current_fallback_target(), None);
        scope_fallback_target(context.clone(), async {
            assert_eq!(current_fallback_target(), Some(context.clone()));
            scope_fallback_target(Default::default(), async {
                assert_eq!(current_fallback_target(), Some(Default::default()));
            })
            .await;
            assert_eq!(current_fallback_target(), Some(context.clone()));
        })
        .await;
        assert_eq!(current_fallback_target(), None);
        cascade.clear_target();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fallback_target_2_1_288.json"
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_value(cascade.target_model()).unwrap(),
            fixture["modelPickState"]["targetAfterPick"]
        );
        assert_eq!(
            cascade.is_latched(),
            fixture["modelPickState"]["routingStillLatched"]
                .as_bool()
                .unwrap()
        );
        assert!(cascade
            .next_hop(&["target".into()], "origin", "again".into())
            .is_none());
        cascade.reset_routing();
        assert_eq!(cascade.target_model(), None);
    }

    #[test]
    fn native_query_target_combines_user_override_chain_and_live_latch() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fallback_target_2_1_288.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let string = |name: &str| case[name].as_str().map(str::to_owned);
            let context = FallbackTargetContext {
                user_model: string("user_model").unwrap(),
                turn_override: string("turn_override"),
                chain_step_model: string("chain_step_model"),
                latched_model: string("latched_model"),
                main_loop_override: string("main_loop_override"),
                ..Default::default()
            };
            assert_eq!(
                context.is_target(case["request_model"].as_str().unwrap(), |model| {
                    fixture["identities"][model].as_str().unwrap().to_owned()
                }),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
