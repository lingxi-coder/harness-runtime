//! `/stop` — stop the current background session.
//!
//! Ported from claude-code v2.1.198's two `local` command objects both named
//! `stop` (one for each host shell — lingxi has no local/local-jsx split, so
//! they collapse to this single headless handler) and their shared core
//! `rpr(source)`. The oracle's `call()` body always runs its full sequence —
//! the `isEnabled` predicate below only gates *palette visibility*, matching
//! the binary's own separation of "is this command listed" from "what does
//! invoking it do":
//!
//! 1. **Visibility gate** — `isEnabled: () => LINGXI_SESSION_KIND === "bg"`.
//!    Identical predicate to the one already ported as
//!    `orchestrator::prompt::bg_session::from_env`'s `kind` check; mirrored
//!    here as [`is_bg_session`] rather than depending on the `orchestrator`
//!    crate (a dev-only dependency of this crate — see the gap note below).
//! 2. **Telemetry** — `tengu_bg_agent_action` with
//!    `{action: "stop", source: "stop_command", jobSessionId}`. lingxi has
//!    only one call path to `/stop`, so `source` is always the local
//!    equivalent of the binary's `"stop_command"` origin (the binary's other
//!    `"bridge"` source belongs to a different command object entirely and
//!    is not ported here).
//! 3. **Job-state rewrite** — if `LINGXI_JOB_DIR` is set (non-empty), rewrite
//!    `$LINGXI_JOB_DIR/state.json` to a terminal `stopped` state UNLESS it is
//!    already terminal (`done`/`failed`/`stopped`). Schema per
//!    `apps/cli/src/agents_registry.rs::JobState` (that module only reads
//!    job state today — this is the M8 gap its own doc comment calls out).
//!    [`stop_job_state`] operates on a raw `serde_json::Map` rather than that
//!    module's typed reader/struct so every existing key (including ones
//!    `JobState` doesn't model, e.g. `backend`/`routine`) round-trips
//!    untouched.
//! 4. **`job_stop_self` marker** — telemetry only.
//! 5. **Exit** — [`lingxi_core::host::OrchestratorHandle::request_exit`] (exists,
//!    used as-is), then `CommandResult::Done` with the locked
//!    `"Session stopped."` literal so the REPL loop observes
//!    `current_should_exit() == true` and exits with code 0.
//!
//! ## Known divergences (documented gaps, not blocking)
//!
//! * The binary only *prints* `"Session stopped."` when
//!   `CLAUDE_BG_BACKEND === "daemon"` — a daemon backend lingxi does not
//!   have. Rather than invent a matching gate that could never open, this
//!   handler renders the literal unconditionally as its `Done { display }` —
//!   simpler than the daemon-only stdout write and always reachable.
//! * `request_exit()` carries no reason/suppress-resume-hint parameter, so
//!   the binary's `Ci(0,"prompt_input_exit",{suppressResumeHint:!0})`
//!   distinction (suppressing the normal resume hint for this exit path) is
//!   not modeled — a minor cosmetic divergence, not a behavior gap.
//! * [`is_bg_session`] is exposed as the ported `isEnabled` predicate, but
//!   `crate::model::BuiltinCommandHandler` has no `is_enabled` hook to
//!   attach it to today. Wiring `/stop` into the palette-visibility surface
//!   (`command-api/src/builtin_support/names.rs`) so it is hidden outside a
//!   bg session is left to the registration layer.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The ported `isEnabled` predicate: `true` iff the process is running as a
/// background session (`LINGXI_SESSION_KIND=bg`). Identical condition to
/// `orchestrator::prompt::bg_session::from_env`'s `kind` gate, reproduced
/// locally so this crate does not need a non-dev dependency on `orchestrator`
/// (which is currently only a dev-dependency of `command-core`, used by
/// `test_support::MockOrchestratorHandle`).
#[must_use]
pub fn is_bg_session() -> bool {
    std::env::var("LINGXI_SESSION_KIND").as_deref() == Ok("bg")
}

/// `/stop` handler — stops the current background session: fires the two
/// ported telemetry events, best-effort-rewrites the job-state file, and
/// requests orchestrator exit.
#[derive(Clone)]
pub struct StopHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl StopHandler {
    /// Construct a `StopHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for StopHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let session_id = self.handle.current_session_id().await;

        // Step 2: tengu_bg_agent_action — the shared `rpr(source)` telemetry
        // call. Field names are snake_case per this crate's tracing
        // convention (see telemetry::lib.rs emit_* helpers); the canonical
        // wire key is camelCase `jobSessionId`.
        tracing::info!(
            event = "tengu_bg_agent_action",
            action = "stop",
            source = "stop_command",
            job_session_id = %session_id,
        );

        // Step 3: best-effort job-state rewrite, only when LINGXI_JOB_DIR is
        // set to a non-empty value (mirrors `from_env`'s `!e => None` guard).
        if let Some(job_dir) = std::env::var("LINGXI_JOB_DIR")
            .ok()
            .filter(|v| !v.is_empty())
        {
            stop_job_state(Path::new(&job_dir));
        }

        // Step 4: job_stop_self marker (telemetry only).
        tracing::info!(event = "job_stop_self");

        // Step 5: request exit and report done.
        self.handle.request_exit().await;
        CommandResult::Done {
            display: Some("Session stopped.".to_string()),
        }
    }

    fn name(&self) -> &str {
        "stop"
    }

    /// Byte-faithful to the claude-code v2.1.198 `stop` command object's
    /// `description`.
    fn description(&self) -> &str {
        "Stop this background session; transcript and worktree are kept"
    }
}

/// `<job_dir>/state.json`.
fn state_path(job_dir: &Path) -> PathBuf {
    job_dir.join("state.json")
}

/// Rewrite `<job_dir>/state.json` to a terminal `"stopped"` state, unless the
/// job is already terminal (`state` is `done`/`failed`/`stopped`). Best
/// effort throughout: any I/O or parse failure is silently swallowed — a
/// `/stop` invocation must never fail just because the job-state file is
/// missing or malformed. Operates on a raw JSON object so every field the
/// file already carries (including ones the typed `JobState` reader in
/// `apps/cli/src/agents_registry.rs` doesn't model) is preserved verbatim.
fn stop_job_state(job_dir: &Path) {
    let path = state_path(job_dir);
    let Ok(bytes) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&bytes) else {
        return;
    };
    let current_state = map.get("state").and_then(Value::as_str).unwrap_or("");
    if matches!(current_state, "done" | "failed" | "stopped") {
        return; // Already terminal — never clobber a prior outcome.
    }

    let now = now_rfc3339();
    let first_terminal_at = map
        .get("firstTerminalAt")
        .cloned()
        .unwrap_or_else(|| Value::String(now.clone()));

    map.insert("state".to_string(), Value::String("stopped".to_string()));
    map.insert(
        "detail".to_string(),
        Value::String("stopped from session".to_string()),
    );
    map.insert("tempo".to_string(), Value::String("idle".to_string()));
    map.remove("needs");
    map.remove("block");
    map.remove("inFlight");
    map.insert("updatedAt".to_string(), Value::String(now));
    map.insert("firstTerminalAt".to_string(), first_terminal_at);

    if let Ok(s) = serde_json::to_string(&Value::Object(map)) {
        let _ = std::fs::write(&path, s);
    }
}

/// Current UTC time as an RFC 3339 string (`YYYY-MM-DDTHH:MM:SSZ`). Computed
/// from the Unix epoch with Howard Hinnant's civil-date algorithm (public
/// domain) so no new dependency is required — mirrors `export.rs`'s
/// `timestamp_now`/`civil_timestamp` pair (duplicated locally rather than
/// reused since neither is `pub(crate)`).
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_rfc3339(secs)
}

/// Convert Unix seconds to an RFC 3339 UTC string. See `now_rfc3339`.
// `doe`/`doy`/`yoe` are the canonical variable names from Hinnant's
// derivation; renaming for `similar_names` would only obscure it.
#[allow(clippy::cast_possible_wrap, clippy::similar_names)]
fn civil_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    // civil_from_days (Hinnant). z = days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
// The `ENV_LOCK` guard is deliberately held across `.await` in several tests
// below (same rationale as `effort.rs`): it keeps the process-global
// `LINGXI_SESSION_KIND`/`LINGXI_JOB_DIR` vars stable for the duration of
// `handle()` so parallel tests in this binary can't race each other.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    /// Env-mutating tests (`LINGXI_SESSION_KIND` / `LINGXI_JOB_DIR`) must run
    /// serialized against each other and against any other test in this
    /// binary that reads those vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A fresh, uniquely-named temp directory (no `tempfile` dependency in
    /// this crate — same idiom as `skills.rs::tests::tmp_root` /
    /// `export.rs`). Callers are responsible for removing it.
    fn tmp_job_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("lingxi-stop-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp job dir");
        dir
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "stop".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[test]
    fn is_bg_session_matches_the_exact_predicate() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("LINGXI_SESSION_KIND");
        assert!(!is_bg_session());
        std::env::set_var("LINGXI_SESSION_KIND", "interactive");
        assert!(!is_bg_session());
        std::env::set_var("LINGXI_SESSION_KIND", "bg");
        assert!(is_bg_session());
        std::env::remove_var("LINGXI_SESSION_KIND");
    }

    #[tokio::test]
    async fn handle_requests_exit_and_returns_locked_literal() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("LINGXI_JOB_DIR");
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock.clone());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Session stopped.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(mock.was_exit_requested());
    }

    #[tokio::test]
    async fn handle_runs_fully_even_when_bg_gate_is_closed() {
        // Step 1's gate is palette-visibility only; `call()`'s internal
        // logic (this handler's `handle`) must run unconditionally.
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("LINGXI_SESSION_KIND");
        std::env::remove_var("LINGXI_JOB_DIR");
        assert!(!is_bg_session());
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock.clone());
        let result = h.handle(&args()).await;
        assert!(
            matches!(result, CommandResult::Done { display: Some(ref s) } if s == "Session stopped.")
        );
        assert!(mock.was_exit_requested());
    }

    #[tokio::test]
    async fn handle_rewrites_non_terminal_job_state_to_stopped() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tmp_job_dir("rewrite");
        std::fs::write(
            tmp.join("state.json"),
            r#"{"state":"working","tempo":"active","sessionId":"abc-1","needs":"a question","block":"waiting on user","inFlight":{"tasks":1,"queued":0,"kinds":[]},"cwd":"/p"}"#,
        )
        .unwrap();
        std::env::set_var("LINGXI_JOB_DIR", &tmp);
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock);
        h.handle(&args()).await;
        std::env::remove_var("LINGXI_JOB_DIR");

        let raw = std::fs::read_to_string(tmp.join("state.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["state"], "stopped");
        assert_eq!(v["detail"], "stopped from session");
        assert_eq!(v["tempo"], "idle");
        assert!(v.get("needs").is_none());
        assert!(v.get("block").is_none());
        assert!(v.get("inFlight").is_none());
        assert!(v.get("updatedAt").is_some());
        assert!(v.get("firstTerminalAt").is_some());
        // Untouched fields round-trip.
        assert_eq!(v["sessionId"], "abc-1");
        assert_eq!(v["cwd"], "/p");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn handle_never_clobbers_an_already_terminal_job() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tmp_job_dir("terminal");
        let original = r#"{"state":"done","tempo":"idle","detail":"pr opened","firstTerminalAt":"2026-01-01T00:00:00Z"}"#;
        std::fs::write(tmp.join("state.json"), original).unwrap();
        std::env::set_var("LINGXI_JOB_DIR", &tmp);
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock);
        h.handle(&args()).await;
        std::env::remove_var("LINGXI_JOB_DIR");

        let raw = std::fs::read_to_string(tmp.join("state.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["state"], "done");
        assert_eq!(v["detail"], "pr opened");
        assert_eq!(v["firstTerminalAt"], "2026-01-01T00:00:00Z");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn handle_ignores_missing_job_dir_without_failing() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("LINGXI_JOB_DIR", "/nonexistent/lingxi-job-dir-for-tests");
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock.clone());
        let result = h.handle(&args()).await;
        std::env::remove_var("LINGXI_JOB_DIR");
        assert!(matches!(result, CommandResult::Done { .. }));
        assert!(mock.was_exit_requested());
    }

    #[tokio::test]
    async fn handle_treats_empty_job_dir_as_unset() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("LINGXI_JOB_DIR", "");
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock.clone());
        let result = h.handle(&args()).await;
        std::env::remove_var("LINGXI_JOB_DIR");
        assert!(matches!(result, CommandResult::Done { .. }));
        assert!(mock.was_exit_requested());
    }

    #[test]
    fn stop_job_state_preserves_unmodeled_fields() {
        let tmp = tmp_job_dir("preserve");
        std::fs::write(
            tmp.join("state.json"),
            r#"{"state":"blocked","backend":"daemon","routine":"nightly","respawnFlags":["--bg"]}"#,
        )
        .unwrap();
        stop_job_state(&tmp);
        let raw = std::fs::read_to_string(tmp.join("state.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["backend"], "daemon");
        assert_eq!(v["routine"], "nightly");
        assert_eq!(v["respawnFlags"][0], "--bg");
        assert_eq!(v["state"], "stopped");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StopHandler::new(mock);
        assert_eq!(h.name(), "stop");
        assert_eq!(
            h.description(),
            "Stop this background session; transcript and worktree are kept"
        );
    }

    #[test]
    fn civil_rfc3339_matches_known_instant() {
        // 2026-07-04 is 20,638 days after the 1970-01-01 epoch (independently
        // verified via `datetime.date(2026,7,4) - datetime.date(1970,1,1)`).
        let secs = 20_638 * 86_400 + 3 * 3_600 + 4 * 60 + 5;
        assert_eq!(civil_rfc3339(secs), "2026-07-04T03:04:05Z");
    }
}
