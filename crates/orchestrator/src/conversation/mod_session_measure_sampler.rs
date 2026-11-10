//! Event-driven `session.measure` sampling and coalescing.

use super::ModSessionMeasureSnapshot;
use serde_json::Value;
use std::sync::Mutex;

/// The source that asked the host to sample usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModSessionMeasureReason {
    /// A rate-limit status or utilization snapshot changed.
    Limits,
    /// A main-loop turn settled.
    Turn,
}

/// One requested sample, retained while another Mod measurement is running.
pub(crate) struct ModSessionMeasureRequest {
    epoch: u64,
    reason: ModSessionMeasureReason,
    snapshot: ModSessionMeasureSnapshot,
}

impl ModSessionMeasureRequest {
    fn merge(self, newer: Self) -> Self {
        let reason = if self.reason == ModSessionMeasureReason::Turn
            || newer.reason == ModSessionMeasureReason::Turn
        {
            ModSessionMeasureReason::Turn
        } else {
            ModSessionMeasureReason::Limits
        };
        Self {
            epoch: newer.epoch,
            reason,
            snapshot: newer.snapshot,
        }
    }
}

/// Serializes samples and retains one merged request while a Mod is handling
/// the current `session.measure` event.
#[derive(Default)]
pub(crate) struct ModSessionMeasureSampler {
    state: Mutex<SamplerState>,
}

#[derive(Default)]
struct SamplerState {
    epoch: u64,
    running: bool,
    pending: Option<ModSessionMeasureRequest>,
    previous: Option<PreviousMeasure>,
}

struct PreviousMeasure {
    context: Value,
    rate_limits: Value,
    cost_usd: Option<f64>,
    limit_status: Option<String>,
}

impl ModSessionMeasureSampler {
    pub(crate) fn reset(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let epoch = state.epoch.wrapping_add(1);
        *state = SamplerState {
            epoch,
            ..SamplerState::default()
        };
    }

    /// Enqueue a sample. The caller should dispatch the returned request; a
    /// missing result means it was merged into the in-flight request's tail.
    pub(crate) fn enqueue(
        &self,
        reason: ModSessionMeasureReason,
        snapshot: ModSessionMeasureSnapshot,
    ) -> Option<ModSessionMeasureRequest> {
        self.enqueue_for_owner(reason, snapshot, None)
    }

    pub(crate) fn enqueue_if_current(
        &self,
        reason: ModSessionMeasureReason,
        snapshot: ModSessionMeasureSnapshot,
        owner: &tokio_util::sync::CancellationToken,
    ) -> Option<ModSessionMeasureRequest> {
        self.enqueue_for_owner(reason, snapshot, Some(owner))
    }

    fn enqueue_for_owner(
        &self,
        reason: ModSessionMeasureReason,
        snapshot: ModSessionMeasureSnapshot,
        owner: Option<&tokio_util::sync::CancellationToken>,
    ) -> Option<ModSessionMeasureRequest> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Reset retires this token before taking the same mutex. An old
        // dispatch therefore either lands before reset (and is erased) or is
        // rejected here; it cannot occupy the replacement epoch's slot.
        if owner.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return None;
        }
        let request = ModSessionMeasureRequest {
            epoch: state.epoch,
            reason,
            snapshot,
        };
        if state.running {
            state.pending = Some(match state.pending.take() {
                Some(pending) => pending.merge(request),
                None => request,
            });
            return None;
        }
        state.running = true;
        Some(request)
    }

    /// Finish one dispatched or suppressed request, returning any coalesced
    /// tail request before releasing the single-flight slot.
    pub(crate) fn finish_one(
        &self,
        request: &ModSessionMeasureRequest,
    ) -> Option<ModSessionMeasureRequest> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if request.epoch != state.epoch {
            return None;
        }
        if let Some(pending) = state.pending.take() {
            return Some(pending);
        }
        state.running = false;
        None
    }

    /// Prepare the exact `session.measure` input for this request, updating
    /// the last-dispatched snapshot only when the reason's change gate opens.
    pub(crate) fn prepare(&self, request: &ModSessionMeasureRequest) -> Option<Value> {
        let snapshot = &request.snapshot;
        let context = snapshot.input.get("context").cloned().unwrap_or_default();
        let rate_limits = snapshot
            .input
            .get("rateLimits")
            .cloned()
            .unwrap_or_default();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if request.epoch != state.epoch {
            return None;
        }

        let context_changed = state
            .previous
            .as_ref()
            .is_none_or(|old| old.context != context);
        let rate_limits_changed = state.previous.as_ref().map_or_else(
            || rate_limits.as_array().is_some_and(|rows| !rows.is_empty()),
            |old| {
                old.limit_status != snapshot.limit_status
                    || limit_windows_moved_by_whole_point(&old.rate_limits, &rate_limits)
            },
        );
        let cost_changed = state.previous.as_ref().map_or_else(
            || snapshot.cost_usd.is_some(),
            |old| old.cost_usd != snapshot.cost_usd,
        );

        let mut changed = Vec::with_capacity(3);
        if context_changed {
            changed.push("context");
        }
        if rate_limits_changed {
            changed.push("rateLimits");
        }
        if cost_changed {
            changed.push("cost");
        }

        let should_dispatch = match request.reason {
            ModSessionMeasureReason::Limits => rate_limits_changed,
            ModSessionMeasureReason::Turn => !changed.is_empty(),
        };
        if !should_dispatch {
            return None;
        }

        let previous_rate_limits = state
            .previous
            .as_ref()
            .filter(|_| !rate_limits_changed)
            .map_or(rate_limits.clone(), |old| old.rate_limits.clone());
        state.previous = Some(PreviousMeasure {
            context,
            rate_limits: previous_rate_limits,
            cost_usd: snapshot.cost_usd,
            limit_status: snapshot.limit_status.clone(),
        });

        let mut input = snapshot.input.clone();
        input["changed"] = serde_json::json!(changed);
        Some(input)
    }
}

fn limit_windows_moved_by_whole_point(previous: &Value, current: &Value) -> bool {
    let Some(previous) = previous.as_array() else {
        return current.as_array().is_some_and(|limits| !limits.is_empty());
    };
    let Some(current) = current.as_array() else {
        return true;
    };
    if previous.len() != current.len() {
        return true;
    }
    current.iter().any(|limit| {
        let Some(kind) = limit.get("kind").and_then(Value::as_str) else {
            return true;
        };
        let Some(percent) = limit.get("percentUsed").and_then(Value::as_f64) else {
            return true;
        };
        let Some(old_percent) = previous
            .iter()
            .find(|old| old.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|old| old.get("percentUsed"))
            .and_then(Value::as_f64)
        else {
            return true;
        };
        (percent - old_percent).abs() >= 1.0 - 1e-9
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        context_version: u64,
        percent: Option<f64>,
        cost_usd: Option<f64>,
        limit_status: Option<&str>,
    ) -> ModSessionMeasureSnapshot {
        let rate_limits = percent.map_or_else(Vec::new, |percent| {
            vec![serde_json::json!({"kind":"five_hour","percentUsed":percent})]
        });
        let mut input = serde_json::json!({
            "context":{"version":context_version},
            "rateLimits":rate_limits,
        });
        if let Some(cost) = cost_usd {
            input["cost"] = serde_json::json!({"usd":cost});
        }
        ModSessionMeasureSnapshot {
            input,
            cost_usd,
            limit_status: limit_status.map(str::to_owned),
        }
    }

    #[test]
    fn limits_events_only_dispatch_for_rate_changes_and_accumulate_subpoint_moves() {
        let sampler = ModSessionMeasureSampler::default();
        let first = sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(1, Some(25.0), None, Some("allowed")),
            )
            .unwrap();
        assert_eq!(
            sampler.prepare(&first).unwrap()["changed"],
            serde_json::json!(["context", "rateLimits"])
        );
        assert!(sampler.finish_one(&first).is_none());

        let half_point = sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(2, Some(25.5), None, Some("allowed")),
            )
            .unwrap();
        assert!(sampler.prepare(&half_point).is_none());
        assert!(sampler.finish_one(&half_point).is_none());

        let whole_point = sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(3, Some(26.0), None, Some("allowed")),
            )
            .unwrap();
        assert_eq!(
            sampler.prepare(&whole_point).unwrap()["changed"],
            serde_json::json!(["context", "rateLimits"])
        );
        assert!(sampler.finish_one(&whole_point).is_none());
    }

    #[test]
    fn turn_reason_dispatches_context_or_cost_changes_without_limit_changes() {
        let sampler = ModSessionMeasureSampler::default();
        let first = sampler
            .enqueue(ModSessionMeasureReason::Turn, snapshot(1, None, None, None))
            .unwrap();
        sampler.prepare(&first).unwrap();
        sampler.finish_one(&first);

        let changed_context = sampler
            .enqueue(ModSessionMeasureReason::Turn, snapshot(2, None, None, None))
            .unwrap();
        assert_eq!(
            sampler.prepare(&changed_context).unwrap()["changed"],
            serde_json::json!(["context"])
        );
        assert!(sampler.finish_one(&changed_context).is_none());

        let limits_only_ignores_that_same_context_change = sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(3, None, None, None),
            )
            .unwrap();
        assert!(sampler
            .prepare(&limits_only_ignores_that_same_context_change)
            .is_none());
        assert!(sampler
            .finish_one(&limits_only_ignores_that_same_context_change)
            .is_none());
    }

    #[test]
    fn limit_status_change_is_a_rate_limit_change() {
        let sampler = ModSessionMeasureSampler::default();
        let first = sampler
            .enqueue(
                ModSessionMeasureReason::Turn,
                snapshot(1, Some(12.0), None, Some("allowed")),
            )
            .unwrap();
        sampler.prepare(&first).unwrap();
        sampler.finish_one(&first);

        let status = sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(1, Some(12.0), None, Some("allowed_warning")),
            )
            .unwrap();
        assert_eq!(
            sampler.prepare(&status).unwrap()["changed"],
            serde_json::json!(["rateLimits"])
        );
        assert!(sampler.finish_one(&status).is_none());
    }

    #[test]
    fn in_flight_requests_merge_to_latest_snapshot_and_turn_reason_wins() {
        let sampler = ModSessionMeasureSampler::default();
        let first = sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(1, Some(10.0), None, Some("allowed")),
            )
            .unwrap();
        assert!(sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(2, Some(10.5), None, Some("allowed")),
            )
            .is_none());
        assert!(sampler
            .enqueue(
                ModSessionMeasureReason::Turn,
                snapshot(3, Some(11.0), Some(0.25), Some("allowed")),
            )
            .is_none());
        sampler.prepare(&first);

        let pending = sampler.finish_one(&first).unwrap();
        assert_eq!(pending.reason, ModSessionMeasureReason::Turn);
        assert_eq!(pending.snapshot.input["context"]["version"], 3);
        assert_eq!(pending.snapshot.cost_usd, Some(0.25));
        assert_eq!(
            sampler.prepare(&pending).unwrap()["changed"],
            serde_json::json!(["context", "rateLimits", "cost"])
        );
        assert!(sampler.finish_one(&pending).is_none());

        assert!(sampler
            .enqueue(
                ModSessionMeasureReason::Limits,
                snapshot(4, Some(12.0), Some(0.25), Some("allowed")),
            )
            .is_some());
    }

    #[test]
    fn reset_retires_in_flight_samples_without_releasing_the_new_slot() {
        let sampler = ModSessionMeasureSampler::default();
        let old = sampler
            .enqueue(ModSessionMeasureReason::Turn, snapshot(1, None, None, None))
            .unwrap();
        sampler.prepare(&old).unwrap();
        sampler.reset();
        let current = sampler
            .enqueue(ModSessionMeasureReason::Turn, snapshot(1, None, None, None))
            .unwrap();
        assert!(sampler.prepare(&old).is_none());
        assert!(sampler.finish_one(&old).is_none());
        assert!(sampler
            .enqueue(ModSessionMeasureReason::Turn, snapshot(2, None, None, None))
            .is_none());
        assert_eq!(
            sampler.prepare(&current).unwrap()["changed"],
            serde_json::json!(["context"])
        );
        let pending = sampler.finish_one(&current).unwrap();
        assert_eq!(pending.snapshot.input["context"]["version"], 2);
        assert!(sampler.finish_one(&pending).is_none());
    }

    #[test]
    fn retired_queued_request_cannot_occupy_the_replacement_epoch() {
        let sampler = ModSessionMeasureSampler::default();
        let owner = tokio_util::sync::CancellationToken::new();
        let old = sampler
            .enqueue_if_current(
                ModSessionMeasureReason::Turn,
                snapshot(1, None, None, None),
                &owner,
            )
            .unwrap();
        owner.cancel();
        sampler.reset();
        assert!(sampler
            .enqueue_if_current(
                ModSessionMeasureReason::Turn,
                snapshot(2, None, None, None),
                &owner,
            )
            .is_none());
        let current = sampler
            .enqueue(ModSessionMeasureReason::Turn, snapshot(3, None, None, None))
            .expect("a retired queued request must leave the replacement slot free");
        assert!(sampler.finish_one(&old).is_none());
        assert_eq!(sampler.prepare(&current).unwrap()["context"]["version"], 3);
        assert!(sampler.finish_one(&current).is_none());
    }
}
