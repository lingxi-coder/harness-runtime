//! Pure in-memory aggregate for one app plus the runtime state machine
//! (spec §C).
//!
//! Every transition is an explicit method returning a typed [`AppError`];
//! anything outside the exhaustive runtime transition table fails with
//! `invalid_request`. The aggregate performs NO I/O —
//! [`crate::service::AppService`] owns persistence and event emission around
//! these methods.

use crate::error::AppError;
use crate::storage;
use crate::types::{
    AppRecord, AppRuntimeMode, AppRuntimeRecord, AppRuntimeState, APPS_SCHEMA_VERSION,
};

/// True iff `from -> to` is one of the legal runtime transitions:
/// `stopped -> starting -> running -> stopping -> stopped`,
/// `starting -> failed`, `running -> failed`, `stopped -> failed`,
/// `failed -> starting`, `failed -> stopped`, `stopping -> failed`,
/// `failed -> stopping`.
///
/// `stopped -> failed` is legal because a start attempt can fail before the
/// record ever leaves `stopped`: `start_reserved_runtime` refuses to bind
/// without a promoted `build/store/dist/index.html`, and its siblings fail the
/// same way with no runtime mount or a squatted permanent port. Without this
/// edge the failure write is rejected and the reason is discarded, leaving an
/// app that cannot start and carries no `lastError` saying why. `failed ->
/// starting` keeps it recoverable.
#[must_use]
pub fn runtime_transition_allowed(from: AppRuntimeState, to: AppRuntimeState) -> bool {
    use AppRuntimeState::{Failed, Running, Starting, Stopped, Stopping};
    matches!(
        (from, to),
        (Stopped | Failed, Starting)
            | (Starting, Running)
            | (Running, Stopping)
            | (Stopping, Stopped)
            | (Stopped | Starting | Running, Failed)
            // Teardown of a runtime that never came up: app deletion, host
            // shutdown reconciliation, or the user pressing Stop on a row
            // showing the failure banner. Without this the write is rejected
            // and the state, timestamp and event are discarded — the same
            // dropped-bookkeeping failure `stopped -> failed` was added to fix.
            | (Failed, Stopped)
            // A listener can die while an explicit stop is already in flight
            // (`Stopping`), and a failed runtime can still be explicitly
            // stopped. Both writes use `let _ =`, so rejecting them discards
            // the state, the `lastError` and the event silently.
            | (Stopping, Failed)
            | (Failed, Stopping)
    )
}

fn monotonic_runtime_timestamp(previous: u64, requested: u64) -> Result<u64, AppError> {
    if requested > previous {
        Ok(requested)
    } else {
        previous.checked_add(1).ok_or_else(|| {
            AppError::InvalidRequest("runtime generation timestamp exhausted".into())
        })
    }
}

/// The full in-memory state of one app: record + runtime record (mirroring
/// the two persisted documents).
#[derive(Debug, Clone, PartialEq)]
pub struct AppState {
    /// Index/mirror record.
    pub record: AppRecord,
    /// Runtime record (`runtime.json`).
    pub runtime: AppRuntimeRecord,
}

impl AppState {
    /// Build the aggregate for a brand-new app in `draft`.
    #[must_use]
    pub fn create(
        id: String,
        name: String,
        brief: String,
        conversation_id: Option<String>,
        now_ms: u64,
    ) -> Self {
        Self::create_with_git(
            id,
            name,
            brief,
            conversation_id,
            crate::types::DEFAULT_GIT_VERSION_CONTROL,
            now_ms,
        )
    }

    /// Build a new app with an explicit Git version-control choice.
    #[must_use]
    pub fn create_with_git(
        id: String,
        name: String,
        brief: String,
        conversation_id: Option<String>,
        git_enabled: bool,
        now_ms: u64,
    ) -> Self {
        let workspace_rel = storage::workspace_rel_str(&id);
        Self {
            record: AppRecord {
                id: id.clone(),
                name,
                brief,
                workflow_model: None,
                mcp_intent: None,
                git_enabled,
                init_session_id: None,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                conversation_id,
                // The origin scope is a create-path fact the constructor's
                // callers (seed/test helpers) do not have. `AppService`'s
                // create path fills it in right after construction, the same
                // way it does for `workflow_model` and `scaffolded`.
                origin_cwd: None,
                workspace_rel,
                // Every direct caller of this constructor (seed/test
                // helpers plus the service's Scaffolded-mode path) builds an
                // already-formed app. `AppService`'s Shell mode is the one
                // exception and overwrites this to `false` right after
                // construction — see `CreateMode` in `service.rs`.
                scaffolded: true,
            },
            runtime: AppRuntimeRecord {
                schema_version: APPS_SCHEMA_VERSION,
                app_id: id,
                state: AppRuntimeState::Stopped,
                mode: None,
                port: None,
                pid: None,
                last_error: None,
                updated_at_ms: now_ms,
            },
        }
    }

    /// Update the runtime record (spec §C). A state change must follow the
    /// runtime transition table; a same-state call just refreshes
    /// `pid`/`last_error`. `port` semantics: `Some(p)` assigns the port when
    /// none is set and must equal the existing one otherwise (a port is NEVER
    /// reassigned — `IndexedDB` origin stability); `None` keeps the current
    /// port. `pid`/`last_error` are overwritten with the given values.
    pub fn set_runtime(
        &mut self,
        next: AppRuntimeState,
        port: Option<u16>,
        pid: Option<u32>,
        last_error: Option<String>,
        now_ms: u64,
    ) -> Result<(), AppError> {
        let current = self.runtime.state;
        if next != current && !runtime_transition_allowed(current, next) {
            return Err(AppError::InvalidRequest(format!(
                "invalid runtime transition {current} -> {next} for app {}",
                self.record.id
            )));
        }
        let next_timestamp = monotonic_runtime_timestamp(self.runtime.updated_at_ms, now_ms)?;
        if let Some(new_port) = port {
            match self.runtime.port {
                Some(existing) if existing != new_port => {
                    return Err(AppError::InvalidRequest(format!(
                        "app {} port is pinned to {existing} and can never be reassigned (requested {new_port})",
                        self.record.id
                    )));
                }
                _ => self.runtime.port = Some(new_port),
            }
        }
        self.runtime.state = next;
        self.runtime.pid = pid;
        self.runtime.last_error = last_error;
        self.runtime.updated_at_ms = next_timestamp;
        Ok(())
    }

    /// Persist the distribution-selected runtime mode independently from the
    /// process-state transition table.
    pub fn set_runtime_mode(&mut self, mode: AppRuntimeMode, now_ms: u64) -> Result<(), AppError> {
        let next_timestamp = monotonic_runtime_timestamp(self.runtime.updated_at_ms, now_ms)?;
        self.runtime.mode = Some(mode);
        self.runtime.updated_at_ms = next_timestamp;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppErrorCode;

    fn app() -> AppState {
        AppState::create(
            "abc123".into(),
            "Test".into(),
            "a test app".into(),
            None,
            10,
        )
    }

    #[test]
    fn a_new_app_starts_with_a_stopped_runtime() {
        let a = app();
        assert_eq!(a.record.created_at_ms, 10);
        assert_eq!(a.record.updated_at_ms, 10);
        assert!(a.record.git_enabled, "git is on by default");
        assert_eq!(a.record.workspace_rel, "apps/abc123/workspace");
        assert_eq!(a.runtime.app_id, "abc123");
        assert_eq!(a.runtime.state, AppRuntimeState::Stopped);
        assert_eq!(a.runtime.mode, None);
        assert_eq!(a.runtime.port, None);
    }

    #[test]
    fn create_with_git_records_the_explicit_choice() {
        let a = AppState::create_with_git(
            "nogit".into(),
            "No Git".into(),
            "an app without git".into(),
            Some("conv-1".into()),
            false,
            7,
        );
        assert!(!a.record.git_enabled);
        assert_eq!(a.record.conversation_id.as_deref(), Some("conv-1"));
    }

    #[test]
    fn runtime_transition_table_is_exhaustive() {
        use AppRuntimeState::{Failed, Running, Starting, Stopped, Stopping};
        let all = [Stopped, Starting, Running, Stopping, Failed];
        let legal = [
            (Stopped, Starting),
            (Starting, Running),
            (Running, Stopping),
            (Stopping, Stopped),
            (Starting, Failed),
            (Running, Failed),
            // A start attempt that fails before leaving `stopped`.
            (Stopped, Failed),
            (Failed, Starting),
            // Teardown of a runtime that never came up.
            (Failed, Stopped),
            // A listener death racing an in-flight stop, and stopping a
            // runtime that is already failed.
            (Stopping, Failed),
            (Failed, Stopping),
        ];
        for from in all {
            for to in all {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    runtime_transition_allowed(from, to),
                    expected,
                    "transition {from} -> {to}"
                );
                if from == to {
                    continue; // same-state is a record refresh, not a transition
                }
                let mut a = app();
                a.runtime.state = from;
                let result = a.set_runtime(to, None, None, None, 11);
                if expected {
                    assert!(result.is_ok(), "{from} -> {to} should be legal");
                    assert_eq!(a.runtime.state, to);
                } else {
                    let err = result.expect_err(&format!("{from} -> {to} should be illegal"));
                    assert_eq!(err.code(), AppErrorCode::InvalidRequest);
                    assert_eq!(a.runtime.state, from, "failed transition must not commit");
                }
            }
        }
    }

    #[test]
    fn runtime_same_state_refreshes_record() {
        let mut a = app();
        a.set_runtime(AppRuntimeState::Starting, Some(3005), Some(77), None, 11)
            .unwrap();
        a.set_runtime(
            AppRuntimeState::Starting,
            None,
            Some(78),
            Some("slow boot".into()),
            12,
        )
        .unwrap();
        assert_eq!(a.runtime.pid, Some(78));
        assert_eq!(a.runtime.last_error.as_deref(), Some("slow boot"));
        assert_eq!(a.runtime.port, Some(3005), "None keeps the pinned port");
    }

    #[test]
    fn runtime_port_is_never_reassigned() {
        let mut a = app();
        a.set_runtime(AppRuntimeState::Starting, Some(3005), None, None, 11)
            .unwrap();
        // Same port is fine.
        a.set_runtime(AppRuntimeState::Running, Some(3005), Some(1), None, 12)
            .unwrap();
        // A different port is rejected and nothing else changes.
        let err = a
            .set_runtime(AppRuntimeState::Failed, Some(3006), None, None, 13)
            .unwrap_err();
        assert_eq!(err.code(), AppErrorCode::InvalidRequest);
        assert_eq!(a.runtime.port, Some(3005));
        assert_eq!(a.runtime.state, AppRuntimeState::Running);
    }

    /// A runtime start can fail BEFORE the record ever leaves `stopped`:
    /// `start_reserved_runtime` refuses to bind without
    /// `build/store/dist/index.html`, and two sibling callers (no runtime
    /// mount, squatted permanent port) reach the same path. That failure has
    /// to be recordable — otherwise the state, the `lastError`, and the
    /// `RuntimeChanged` event are all discarded and nothing can ever read why
    /// the app will not start.
    #[test]
    fn a_start_failure_from_stopped_persists_the_reason() {
        let mut a = app();
        assert_eq!(a.runtime.state, AppRuntimeState::Stopped);
        a.set_runtime(
            AppRuntimeState::Failed,
            None,
            None,
            Some("static build output is missing index.html; generate the app first".into()),
            42,
        )
        .expect("a start failure from stopped must be recordable");
        assert_eq!(a.runtime.state, AppRuntimeState::Failed);
        assert_eq!(
            a.runtime.last_error.as_deref(),
            Some("static build output is missing index.html; generate the app first")
        );
        assert_eq!(a.runtime.updated_at_ms, 42);
        // The app must still be able to leave the state it just entered.
        assert!(runtime_transition_allowed(
            AppRuntimeState::Failed,
            AppRuntimeState::Starting
        ));
    }

    #[test]
    fn set_runtime_mode_stamps_the_mode_and_timestamp() {
        let mut a = app();
        a.set_runtime_mode(AppRuntimeMode::StaticExport, 42)
            .unwrap();
        assert_eq!(a.runtime.mode, Some(AppRuntimeMode::StaticExport));
        assert_eq!(a.runtime.updated_at_ms, 42);
        a.set_runtime_mode(AppRuntimeMode::NextProduction, 43)
            .unwrap();
        assert_eq!(a.runtime.mode, Some(AppRuntimeMode::NextProduction));
    }

    #[test]
    fn runtime_timestamps_advance_when_clock_repeats() {
        let mut a = app();
        a.set_runtime(AppRuntimeState::Starting, None, None, None, 10)
            .unwrap();
        assert_eq!(a.runtime.updated_at_ms, 11);
        a.set_runtime(AppRuntimeState::Running, None, None, None, 10)
            .unwrap();
        assert_eq!(a.runtime.updated_at_ms, 12);
        a.set_runtime_mode(AppRuntimeMode::StaticExport, 10)
            .unwrap();
        assert_eq!(a.runtime.updated_at_ms, 13);
    }

    #[test]
    fn runtime_timestamp_exhaustion_fails_closed() {
        let mut a = app();
        a.runtime.updated_at_ms = u64::MAX;
        let before = a.clone();
        let error = a
            .set_runtime(
                AppRuntimeState::Starting,
                Some(3210),
                Some(99),
                Some("must not land".into()),
                u64::MAX,
            )
            .unwrap_err();
        assert_eq!(error.code(), AppErrorCode::InvalidRequest);
        assert_eq!(
            a, before,
            "timestamp allocation must precede every mutation"
        );

        let error = a
            .set_runtime_mode(AppRuntimeMode::StaticExport, u64::MAX)
            .unwrap_err();
        assert_eq!(error.code(), AppErrorCode::InvalidRequest);
        assert_eq!(a, before, "mode mutation must also fail atomically");
    }
}
