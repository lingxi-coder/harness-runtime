//! Host-owned facts for the lifetime of model-issued tool-use blocks.
//!
//! These values describe runtime control and are deliberately not part of a
//! provider or SDK wire format.

use crate::types::ToolUseId;
use std::collections::HashSet;

/// Native `la()` concurrency default when the typed environment value is
/// absent or rejected.
pub const DEFAULT_MAX_TOOL_USE_CONCURRENCY: usize = 10;

/// Read the host-branded tool-use concurrency limit.
#[must_use]
pub fn max_tool_use_concurrency() -> usize {
    max_tool_use_concurrency_from(
        std::env::var("LINGXI_MAX_TOOL_USE_CONCURRENCY")
            .ok()
            .as_deref(),
    )
}

/// Resolve the concurrency limit with Native typed-integer environment
/// semantics: parse through [`crate::host::env::parse_int_env`], require a
/// finite value of at least one, and otherwise use 10. The Native parser has
/// no configured maximum; values beyond this platform's `usize` capacity
/// saturate only because the executor's capacity counter is a `usize`.
#[must_use]
pub fn max_tool_use_concurrency_from(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_MAX_TOOL_USE_CONCURRENCY;
    };
    let parsed = crate::host::env::parse_int_env(raw);
    if !parsed.is_finite() || parsed < 1.0 {
        DEFAULT_MAX_TOOL_USE_CONCURRENCY
    } else if parsed >= usize::MAX as f64 {
        usize::MAX
    } else {
        parsed as usize
    }
}

/// Why a host removed one or more outstanding tool-use ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolUseRemovalReason {
    /// An admitted server fallback discarded outstanding tool-use blocks.
    FallbackSweep,
}

/// Host-owned removal of outstanding tool-use ids.
///
/// This is intentionally not serializable: adapters may report block indices,
/// but only the host that owns the live query can resolve those indices to the
/// provider-issued [`ToolUseId`] values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseRemoval {
    /// Canonical provider-issued tool-use ids removed from the live query.
    pub ids: Vec<ToolUseId>,
    /// Optional reason retained for host-side lifecycle accounting.
    pub reason: Option<ToolUseRemovalReason>,
}

/// Query-local tracker for the Native outstanding-tool / Agent-idle relation.
///
/// Native marks an Agent idle only when at least one outstanding tool use is an
/// Agent invocation and every outstanding tool use is an Agent invocation.
/// Tool results and host removals both clear ids from both sets.
#[derive(Debug, Default)]
pub struct ToolUseLifecycleTracker {
    outstanding: HashSet<ToolUseId>,
    outstanding_agents: HashSet<ToolUseId>,
}

impl ToolUseLifecycleTracker {
    /// Whether this query currently satisfies the Native local-Agent idle
    /// predicate.
    #[must_use]
    pub fn is_agent_idle(&self) -> bool {
        !self.outstanding_agents.is_empty()
            && self.outstanding_agents.len() == self.outstanding.len()
    }

    /// Add every tool use from one completed assistant row, then report whether
    /// the idle predicate changed. `is_agent` must come from the caller's
    /// resolved tool identity, not guessed from unrelated JSON metadata.
    pub fn observe_assistant_row(
        &mut self,
        tool_uses: impl IntoIterator<Item = (ToolUseId, bool)>,
    ) -> Option<bool> {
        let before = self.is_agent_idle();
        for (id, is_agent) in tool_uses {
            self.outstanding.insert(id.clone());
            if is_agent {
                self.outstanding_agents.insert(id);
            }
        }
        let after = self.is_agent_idle();
        (before != after).then_some(after)
    }

    /// Remove a completed tool use from both outstanding sets and report a
    /// changed idle fact. Unknown and duplicate ids are harmless.
    pub fn observe_tool_result(&mut self, id: &ToolUseId) -> Option<bool> {
        self.remove_ids(std::iter::once(id))
    }

    /// Apply a host-owned sweep. The reason is metadata only; every listed id
    /// is removed for every reason.
    pub fn apply_removal(&mut self, removal: &ToolUseRemoval) -> Option<bool> {
        self.remove_ids(removal.ids.iter())
    }

    fn remove_ids<'a>(&mut self, ids: impl IntoIterator<Item = &'a ToolUseId>) -> Option<bool> {
        let before = self.is_agent_idle();
        for id in ids {
            self.outstanding.remove(id);
            self.outstanding_agents.remove(id);
        }
        let after = self.is_agent_idle();
        (before != after).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_limit_uses_typed_integer_env_semantics() {
        for (raw, expected) in [
            (None, DEFAULT_MAX_TOOL_USE_CONCURRENCY),
            (Some(""), DEFAULT_MAX_TOOL_USE_CONCURRENCY),
            (Some("0"), DEFAULT_MAX_TOOL_USE_CONCURRENCY),
            (Some("-2"), DEFAULT_MAX_TOOL_USE_CONCURRENCY),
            (Some("1.5e0"), DEFAULT_MAX_TOOL_USE_CONCURRENCY),
            (Some("1e400"), DEFAULT_MAX_TOOL_USE_CONCURRENCY),
            (Some("3.9"), 3),
            (Some("12junk"), 12),
            (Some("1_000"), 1_000),
            (Some("1e2"), 100),
            (Some("25"), 25),
        ] {
            assert_eq!(max_tool_use_concurrency_from(raw), expected, "{raw:?}");
        }
        assert_eq!(max_tool_use_concurrency_from(Some("1e100")), usize::MAX);
    }

    fn id(value: &str) -> ToolUseId {
        ToolUseId::from(value)
    }

    #[test]
    fn idle_requires_nonempty_agent_subset_equal_to_all_outstanding_uses() {
        let mut tracker = ToolUseLifecycleTracker::default();
        assert_eq!(
            tracker.observe_assistant_row([(id("agent-1"), true)]),
            Some(true)
        );
        assert!(tracker.is_agent_idle());

        assert_eq!(
            tracker.observe_assistant_row([(id("read-1"), false)]),
            Some(false)
        );
        assert!(!tracker.is_agent_idle());

        assert_eq!(tracker.observe_tool_result(&id("read-1")), Some(true));
        assert!(tracker.is_agent_idle());
    }

    #[test]
    fn tool_result_and_fallback_sweep_remove_ids_idempotently() {
        let mut tracker = ToolUseLifecycleTracker::default();
        tracker.observe_assistant_row([
            (id("agent-1"), true),
            (id("agent-2"), true),
            (id("read-1"), false),
        ]);
        assert_eq!(tracker.observe_tool_result(&id("read-1")), Some(true));
        assert_eq!(
            tracker.apply_removal(&ToolUseRemoval {
                ids: vec![id("agent-1"), id("missing")],
                reason: Some(ToolUseRemovalReason::FallbackSweep),
            }),
            None
        );
        assert!(
            tracker.is_agent_idle(),
            "the remaining outstanding use is still an Agent"
        );
        assert_eq!(
            tracker.apply_removal(&ToolUseRemoval {
                ids: vec![id("agent-1")],
                reason: Some(ToolUseRemovalReason::FallbackSweep),
            }),
            None
        );
        assert_eq!(tracker.observe_tool_result(&id("agent-2")), Some(false));
        assert!(!tracker.is_agent_idle());
    }
}
