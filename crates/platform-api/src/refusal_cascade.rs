//! Refusal-fallback CASCADE routing (claude-code `qqi` / `jqi` / `FQt`,
//! 2.1.220 @228125792).
//!
//! When a model refuses, the session falls back to another one. Claude does not
//! fall back to a single model — it walks an ordered CHAIN, keyed by the API's
//! refusal category, skipping stages that cannot be resolved (unknown model,
//! or one already tried this episode) until it finds a reachable hop.
//!
//! Two properties make the chain more than a list:
//!
//! - **Already-tried stages are skipped.** Without that a cascade could loop
//!   back onto the model that just refused, or re-try a hop that already
//!   refused earlier in the same episode.
//! - **Every skipped stage is REPORTED** (one
//!   `tengu_refusal_fallback_route_declined` per stage, reason
//!   `chain_entry_unresolvable`, carrying its index). A chain that silently
//!   degrades to its last entry is indistinguishable from a chain that worked,
//!   which is exactly the case an operator needs to see.
//!
//! This module is the routing decision only — pure, and independent of the turn
//! loop. It is also the substrate the refusal-notice collapse needs: a hop that
//! a LATER hop supersedes is what makes a notice provisional
//! (`docs/refusal-retraction-DECOMPOSITION-2026-07-26.md`).

/// Maximum stages consulted from one chain (claude `Jug`).
pub const MAX_CHAIN_STAGES: usize = 3;

/// Why a refusal was not routed to a fallback (claude
/// `tengu_refusal_fallback_route_declined`'s `reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclineReason {
    /// No chain is configured for this model / category at all.
    Unmapped,
    /// A chain exists but no stage in it resolved.
    MappedTargetUnresolvable {
        /// How long the chain was.
        chain_length: usize,
    },
    /// One stage of a chain could not be resolved. Emitted per stage, so a
    /// chain that skipped two entries reports two of these.
    ChainEntryUnresolvable {
        /// The chain's length.
        chain_length: usize,
        /// 1-based index of the stage within the chain.
        chain_entry_index: usize,
        /// The stage's configured target.
        chain_entry: String,
    },
}

impl DeclineReason {
    /// The wire `reason` string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unmapped => "unmapped",
            Self::MappedTargetUnresolvable { .. } => "mapped_target_unresolvable",
            Self::ChainEntryUnresolvable { .. } => "chain_entry_unresolvable",
        }
    }
}

/// A resolved hop plus what the walk passed over to reach it (claude `qqi`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachableStage {
    /// The model this hop routes to.
    pub model: String,
    /// Stages before it that did not resolve, in order.
    pub skipped_stages: Vec<String>,
    /// Stages after it — the cascade's remaining budget.
    pub remaining_chain: Vec<String>,
}

/// How a refusal was routed (claude `jqi`'s `matched`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalRoute {
    /// A category chain matched.
    Category {
        /// The hop to take.
        stage: ReachableStage,
        /// The chain's full length, for the declined-stage reports.
        chain_length: usize,
    },
    /// No chain, but a session-wide armed fallback applies.
    CatchAll {
        /// The armed fallback model.
        model: String,
    },
    /// Nothing to route to.
    None {
        /// Why.
        reason: DeclineReason,
        /// Stages that were consulted and skipped, for reporting.
        skipped_stages: Vec<String>,
    },
}

/// Walk `chain` and return the first stage `resolve` accepts (claude `qqi`).
///
/// `resolve` returns `None` for a stage that cannot be used — an unknown model,
/// or one already tried. Only the first `MAX_CHAIN_STAGES` entries are
/// consulted, matching claude's cap.
#[must_use]
pub fn first_reachable_stage(
    chain: &[String],
    mut resolve: impl FnMut(&str) -> Option<String>,
) -> Option<ReachableStage> {
    let capped = &chain[..chain.len().min(MAX_CHAIN_STAGES)];
    capped.iter().enumerate().find_map(|(idx, stage)| {
        resolve(stage).map(|model| ReachableStage {
            model,
            skipped_stages: capped[..idx].to_vec(),
            remaining_chain: capped[idx + 1..].to_vec(),
        })
    })
}

/// Inputs to the routing decision.
#[derive(Debug, Clone)]
pub struct RouteInputs<'a> {
    /// The chain configured for this refusal's category, when there is one.
    pub chain: Option<&'a [String]>,
    /// The session-wide armed fallback, used only when no chain matched.
    pub armed_fallback_model: Option<&'a str>,
    /// Whether the armed fallback IS the model that just refused — routing to
    /// it would re-ask the model that already said no.
    pub armed_target_is_refusing_model: bool,
    /// Whether the catch-all fallback is enabled at all.
    pub catch_all_enabled: bool,
}

/// Route a refusal (claude `jqi`).
///
/// Order matters: a configured CHAIN wins over the catch-all, and a chain that
/// exists but resolves nowhere declines rather than silently falling through to
/// the catch-all — an operator who configured a chain gets told it failed
/// instead of quietly getting different behaviour.
#[must_use]
pub fn route_refusal(
    inputs: &RouteInputs<'_>,
    resolve: impl FnMut(&str) -> Option<String>,
) -> RefusalRoute {
    if let Some(chain) = inputs.chain {
        let chain_length = chain.len().min(MAX_CHAIN_STAGES);
        if let Some(stage) = first_reachable_stage(chain, resolve) {
            return RefusalRoute::Category {
                stage,
                chain_length,
            };
        }
        // Claude reports `chain.slice(0, -1)` here: the LAST stage's failure is
        // carried by the `mapped_target_unresolvable` decline itself, so
        // reporting it again as a skipped entry would double-count it.
        let skipped = chain[..chain_length.saturating_sub(1)].to_vec();
        return RefusalRoute::None {
            reason: DeclineReason::MappedTargetUnresolvable { chain_length },
            skipped_stages: skipped,
        };
    }
    if inputs.catch_all_enabled && !inputs.armed_target_is_refusing_model {
        if let Some(model) = inputs.armed_fallback_model {
            return RefusalRoute::CatchAll {
                model: model.to_string(),
            };
        }
    }
    RefusalRoute::None {
        reason: DeclineReason::Unmapped,
        skipped_stages: Vec::new(),
    }
}

/// The per-stage decline reports a route implies (claude `HXn` → `OXn`).
///
/// `first_stage_index` is 1-based, matching claude's `firstStageIndex: 1`, so
/// an operator reading the telemetry counts stages the way the config lists
/// them.
#[must_use]
pub fn decline_reports(route: &RefusalRoute) -> Vec<DeclineReason> {
    match route {
        // A catch-all skipped nothing.
        RefusalRoute::CatchAll { .. } => Vec::new(),
        RefusalRoute::Category {
            stage,
            chain_length,
        } => stage
            .skipped_stages
            .iter()
            .enumerate()
            .map(|(i, entry)| DeclineReason::ChainEntryUnresolvable {
                chain_length: *chain_length,
                chain_entry_index: i + 1,
                chain_entry: entry.clone(),
            })
            .collect(),
        RefusalRoute::None {
            reason: DeclineReason::Unmapped,
            ..
        } => vec![DeclineReason::Unmapped],
        RefusalRoute::None {
            reason,
            skipped_stages,
        } => {
            let chain_length = match reason {
                DeclineReason::MappedTargetUnresolvable { chain_length } => *chain_length,
                _ => skipped_stages.len(),
            };
            let mut out: Vec<DeclineReason> = skipped_stages
                .iter()
                .enumerate()
                .map(|(i, entry)| DeclineReason::ChainEntryUnresolvable {
                    chain_length,
                    chain_entry_index: i + 1,
                    chain_entry: entry.clone(),
                })
                .collect();
            out.push(reason.clone());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// The plain case: the first stage resolves, and the rest is the cascade's
    /// remaining budget.
    #[test]
    fn the_first_resolvable_stage_wins_and_keeps_the_remainder() {
        let c = chain(&["a", "b", "c"]);
        let got = first_reachable_stage(&c, |s| Some(s.to_uppercase())).unwrap();
        assert_eq!(got.model, "A");
        assert!(got.skipped_stages.is_empty());
        assert_eq!(got.remaining_chain, chain(&["b", "c"]));
    }

    /// Unresolvable stages are SKIPPED, not fatal — and they are recorded, so
    /// a chain that silently degraded to its last entry is still visible.
    #[test]
    fn unresolvable_stages_are_skipped_and_recorded() {
        let c = chain(&["gone", "also-gone", "real"]);
        let got = first_reachable_stage(&c, |s| (s == "real").then(|| s.to_string())).unwrap();
        assert_eq!(got.model, "real");
        assert_eq!(got.skipped_stages, chain(&["gone", "also-gone"]));
        assert!(got.remaining_chain.is_empty());
    }

    #[test]
    fn a_chain_that_resolves_nowhere_yields_nothing() {
        let c = chain(&["a", "b"]);
        assert!(first_reachable_stage(&c, |_| None).is_none());
    }

    /// Claude consults at most three stages; a longer chain is capped, and the
    /// cap is visible in `chain_length` rather than silently walking further.
    #[test]
    fn the_chain_is_capped_at_three_stages() {
        let c = chain(&["a", "b", "c", "d", "e"]);
        assert!(
            first_reachable_stage(&c, |s| (s == "d").then(|| s.to_string())).is_none(),
            "a stage past the cap is unreachable"
        );
        let got = first_reachable_stage(&c, |s| (s == "c").then(|| s.to_string())).unwrap();
        assert_eq!(got.model, "c");
        assert!(
            got.remaining_chain.is_empty(),
            "the remainder is capped too, not the raw tail"
        );
    }

    /// The point of `tried_models`: a cascade must not loop back onto a model
    /// that already refused this episode.
    #[test]
    fn an_already_tried_stage_is_skipped() {
        let c = chain(&["first", "second"]);
        let tried = ["first".to_string()];
        let got = first_reachable_stage(&c, |s| {
            (!tried.contains(&s.to_string())).then(|| s.to_string())
        })
        .unwrap();
        assert_eq!(got.model, "second");
        assert_eq!(got.skipped_stages, chain(&["first"]));
    }

    fn inputs(chain: Option<&[String]>) -> RouteInputs<'_> {
        RouteInputs {
            chain,
            armed_fallback_model: Some("armed"),
            armed_target_is_refusing_model: false,
            catch_all_enabled: true,
        }
    }

    #[test]
    fn a_matching_chain_routes_by_category() {
        let c = chain(&["a", "b"]);
        let route = route_refusal(&inputs(Some(&c)), |s| Some(s.to_string()));
        let RefusalRoute::Category {
            stage,
            chain_length,
        } = route
        else {
            panic!("expected a category route")
        };
        assert_eq!(stage.model, "a");
        assert_eq!(chain_length, 2);
    }

    /// A configured chain that resolves nowhere DECLINES — it must not fall
    /// through to the catch-all. An operator who configured a chain is told it
    /// failed instead of quietly getting different routing.
    #[test]
    fn an_unresolvable_chain_declines_rather_than_using_the_catch_all() {
        let c = chain(&["a", "b"]);
        let route = route_refusal(&inputs(Some(&c)), |_| None);
        assert_eq!(
            route,
            RefusalRoute::None {
                reason: DeclineReason::MappedTargetUnresolvable { chain_length: 2 },
                // The LAST stage's failure is carried by the decline itself, so
                // it is not also reported as a skipped entry.
                skipped_stages: chain(&["a"]),
            }
        );
    }

    #[test]
    fn no_chain_falls_back_to_the_catch_all() {
        assert_eq!(
            route_refusal(&inputs(None), |_| None),
            RefusalRoute::CatchAll {
                model: "armed".into()
            }
        );
    }

    /// Routing to the model that just refused would re-ask the model that
    /// already said no.
    #[test]
    fn the_catch_all_is_refused_when_it_is_the_refusing_model() {
        let mut i = inputs(None);
        i.armed_target_is_refusing_model = true;
        assert_eq!(
            route_refusal(&i, |_| None),
            RefusalRoute::None {
                reason: DeclineReason::Unmapped,
                skipped_stages: Vec::new()
            }
        );
    }

    #[test]
    fn no_chain_and_no_armed_model_is_unmapped() {
        let mut i = inputs(None);
        i.armed_fallback_model = None;
        assert!(matches!(
            route_refusal(&i, |_| None),
            RefusalRoute::None {
                reason: DeclineReason::Unmapped,
                ..
            }
        ));
    }

    /// Every skipped stage produces its own declined report, indexed 1-based
    /// the way the config lists them — two skipped entries, two reports.
    #[test]
    fn each_skipped_stage_reports_itself_with_a_one_based_index() {
        let c = chain(&["gone", "also-gone", "real"]);
        let route = route_refusal(&inputs(Some(&c)), |s| (s == "real").then(|| s.to_string()));
        let reports = decline_reports(&route);
        assert_eq!(
            reports,
            vec![
                DeclineReason::ChainEntryUnresolvable {
                    chain_length: 3,
                    chain_entry_index: 1,
                    chain_entry: "gone".into()
                },
                DeclineReason::ChainEntryUnresolvable {
                    chain_length: 3,
                    chain_entry_index: 2,
                    chain_entry: "also-gone".into()
                },
            ]
        );
    }

    /// A successful first hop skipped nothing, so it reports nothing — the
    /// telemetry is for degradation, not for every refusal.
    #[test]
    fn a_clean_route_reports_nothing() {
        let c = chain(&["a"]);
        let route = route_refusal(&inputs(Some(&c)), |s| Some(s.to_string()));
        assert!(decline_reports(&route).is_empty());
        assert!(decline_reports(&RefusalRoute::CatchAll { model: "x".into() }).is_empty());
    }

    /// A fully unresolvable chain reports each skipped entry AND the overall
    /// decline, so the operator sees both what was tried and that nothing took.
    #[test]
    fn an_unresolvable_chain_reports_the_entries_and_the_decline() {
        let c = chain(&["a", "b"]);
        let route = route_refusal(&inputs(Some(&c)), |_| None);
        let reports = decline_reports(&route);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].as_str(), "chain_entry_unresolvable");
        assert_eq!(reports[1].as_str(), "mapped_target_unresolvable");
    }

    #[test]
    fn decline_reasons_are_byte_locked() {
        assert_eq!(DeclineReason::Unmapped.as_str(), "unmapped");
        assert_eq!(
            DeclineReason::MappedTargetUnresolvable { chain_length: 1 }.as_str(),
            "mapped_target_unresolvable"
        );
        assert_eq!(
            DeclineReason::ChainEntryUnresolvable {
                chain_length: 1,
                chain_entry_index: 1,
                chain_entry: String::new()
            }
            .as_str(),
            "chain_entry_unresolvable"
        );
        assert_eq!(MAX_CHAIN_STAGES, 3);
    }
}
