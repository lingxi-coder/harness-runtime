//! Coordinator mode toggle.
//!
//! When enabled, the host wires coordinator-only tools (`team_create`,
//! `team_delete`, `send_message`, `synthetic_output`) into the tool registry.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::coordinator::MODE_SWITCHED;
use telemetry::AnalyticsBus;

/// In-memory coordinator-mode flag.
#[derive(Default)]
pub struct CoordinatorMode {
    enabled: AtomicBool,
    /// True if the session was started directly in coordinator mode (vs.
    /// upgraded later via a mode-switch).
    pub session_started_as_coordinator: bool,
    /// Optional analytics bus used by [`CoordinatorMode::match_session_mode`] to
    /// fire `tengu_coordinator_mode_switched` when a resume flips the mode.
    /// `None` (the default) ⇒ the switch is logged via `tracing` only. Wired by
    /// the composition root via [`CoordinatorMode::with_analytics_bus`] so the
    /// `new()` / `default()` constructors stay argument-free (the build hot path
    /// constructs the mode before the orchestrator bus, then attaches it).
    bus: Option<Arc<AnalyticsBus>>,
}

impl std::fmt::Debug for CoordinatorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinatorMode")
            .field("enabled", &self.enabled.load(Ordering::Acquire))
            .field(
                "session_started_as_coordinator",
                &self.session_started_as_coordinator,
            )
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl CoordinatorMode {
    /// Construct a disabled mode.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Attach an analytics bus so [`Self::match_session_mode`] can fire
    /// `tengu_coordinator_mode_switched` on a resume flip. Builder-style so the
    /// composition root can do `CoordinatorMode::new().with_analytics_bus(bus)`.
    #[must_use]
    pub fn with_analytics_bus(mut self, bus: Arc<AnalyticsBus>) -> Self {
        self.bus = Some(bus);
        self
    }
    /// Whether coordinator mode is currently active.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    /// Switch into coordinator mode.
    pub fn enter(&self) {
        self.enabled.store(true, Ordering::Release);
    }
    /// Leave coordinator mode.
    pub fn exit(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    /// Reconcile the live coordinator mode against a resumed session's persisted
    /// mode — 1:1 with `matchSessionMode` (`coordinatorMode.ts:49-78`).
    ///
    /// `persisted` is the resumed session's stored mode:
    /// - `None` ⇒ old session predating mode tracking; do nothing, return `None`.
    /// - `Some(true)` ⇒ session was a coordinator session.
    /// - `Some(false)` ⇒ session was a normal (non-coordinator) session.
    ///
    /// When the persisted mode differs from the current mode, this flips the
    /// live flag ([`Self::enter`] / [`Self::exit`] — the Rust analog of TS
    /// mutating `process.env.CLAUDE_CODE_COORDINATOR_MODE`, which
    /// `isCoordinatorMode()` reads live), fires `tengu_coordinator_mode_switched`
    /// `{ to: "coordinator" | "normal" }`, and returns the warning message:
    /// `"Entered coordinator mode to match resumed session."` or
    /// `"Exited coordinator mode to match resumed session."`. On a match it is a
    /// no-op and returns `None`.
    pub fn match_session_mode(&self, persisted: Option<bool>) -> Option<String> {
        // No stored mode (old session before mode tracking) — do nothing.
        let session_is_coordinator = persisted?;

        let current_is_coordinator = self.is_enabled();
        if current_is_coordinator == session_is_coordinator {
            return None;
        }

        // Flip the live flag to match the resumed session.
        if session_is_coordinator {
            self.enter();
        } else {
            self.exit();
        }

        let to = if session_is_coordinator {
            "coordinator"
        } else {
            "normal"
        };

        // Fire `tengu_coordinator_mode_switched { to }`. When a bus is attached,
        // log through it (fire-and-forget on the current runtime); otherwise fall
        // back to a tracing event so the switch is always observable.
        match (&self.bus, tokio::runtime::Handle::try_current()) {
            (Some(bus), Ok(handle)) => {
                let bus = bus.clone();
                let mut md: LogEventMetadata = LogEventMetadata::new();
                md.insert("to".into(), AnalyticsValue::String(to.to_string()));
                // Fire-and-forget: `match_session_mode` is sync (per the
                // requested signature) but the bus is async, so log on the
                // current runtime without blocking the resume path.
                handle.spawn(async move {
                    bus.log_event(MODE_SWITCHED, md).await;
                });
            }
            _ => {
                // No bus attached, or called outside a tokio runtime: fall back
                // to a tracing event so the switch is always observable.
                tracing::info!(
                    event = MODE_SWITCHED,
                    to,
                    "coordinator mode switched on resume"
                );
            }
        }

        Some(if session_is_coordinator {
            "Entered coordinator mode to match resumed session.".to_string()
        } else {
            "Exited coordinator mode to match resumed session.".to_string()
        })
    }
}

/// Live coordinator-mode seam consumed by `AgentTool` via
/// [`tool_api::BuiltinToolContext::coordinator_mode`]. Reads the same atomic
/// flag as [`CoordinatorMode::is_enabled`], so a mid-session switch is observed
/// immediately (the fork-subagent gate + slim coordinator prompt stay in sync).
impl platform_api::coordinator_mode::CoordinatorModeHandle for CoordinatorMode {
    fn is_enabled(&self) -> bool {
        CoordinatorMode::is_enabled(self)
    }
}

/// Result of a mode-switch tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModeSwitchResult {
    /// Successfully entered coordinator mode.
    EnteredCoordinator,
    /// Successfully exited coordinator mode.
    ExitedCoordinator,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_session_mode_none_is_noop() {
        let mode = CoordinatorMode::new(); // disabled
        assert_eq!(mode.match_session_mode(None), None);
        assert!(!mode.is_enabled(), "no flip on None");

        mode.enter();
        assert_eq!(mode.match_session_mode(None), None);
        assert!(mode.is_enabled(), "no flip on None even when enabled");
    }

    #[test]
    fn match_session_mode_noop_on_match() {
        // current normal, persisted normal -> no-op.
        let mode = CoordinatorMode::new();
        assert_eq!(mode.match_session_mode(Some(false)), None);
        assert!(!mode.is_enabled());

        // current coordinator, persisted coordinator -> no-op.
        let mode = CoordinatorMode::new();
        mode.enter();
        assert_eq!(mode.match_session_mode(Some(true)), None);
        assert!(mode.is_enabled());
    }

    #[tokio::test]
    async fn match_session_mode_enters_on_mismatch() {
        // current normal, persisted coordinator -> enter + message.
        let mode = CoordinatorMode::new();
        assert!(!mode.is_enabled());
        let msg = mode.match_session_mode(Some(true));
        assert_eq!(
            msg.as_deref(),
            Some("Entered coordinator mode to match resumed session.")
        );
        assert!(mode.is_enabled(), "must have flipped ON");
    }

    #[tokio::test]
    async fn match_session_mode_exits_on_mismatch() {
        // current coordinator, persisted normal -> exit + message.
        let mode = CoordinatorMode::new();
        mode.enter();
        assert!(mode.is_enabled());
        let msg = mode.match_session_mode(Some(false));
        assert_eq!(
            msg.as_deref(),
            Some("Exited coordinator mode to match resumed session.")
        );
        assert!(!mode.is_enabled(), "must have flipped OFF");
    }

    #[tokio::test]
    async fn match_session_mode_fires_telemetry_through_bus() {
        use telemetry::sinks::InMemorySink;
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;

        let mode = CoordinatorMode::new().with_analytics_bus(bus);
        let msg = mode.match_session_mode(Some(true));
        assert!(msg.is_some());

        // The detached spawn logs asynchronously — yield until it lands.
        for _ in 0..1000 {
            if !sink.events().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let events = sink.events().await;
        assert!(
            events.iter().any(|e| e.name == MODE_SWITCHED),
            "tengu_coordinator_mode_switched must be emitted; got {events:?}"
        );
    }
}
