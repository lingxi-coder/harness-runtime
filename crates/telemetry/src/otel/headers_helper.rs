//! `otelHeadersHelper` — dynamic OTLP export-header provider.
//!
//! CC (binary `RRi()` / `oSh()` / `ARi()`) lets an enterprise settings key
//! `otelHeadersHelper` name a script whose stdout is a JSON object of header
//! `k:v` strings (e.g. a short-lived bearer token). The result is cached and
//! re-invoked at most once per debounce window
//! ([`super::config::ENV_HEADERS_HELPER_DEBOUNCE_MS`], default 29 min); a
//! failure is cached so downstream export proceeds header-less rather than
//! blocking, and the failure is surfaced once on stderr.
//!
//! This module ports the **validation + cache state machine** byte-for-byte
//! (the load-bearing parity: the exact error strings, debounce window, and
//! failure-caching semantics). The actual subprocess spawn is injected by the
//! caller via [`HeadersHelperState::resolve`], keeping this low-level telemetry
//! crate free of a process-exec dependency. Wiring a real spawner into the
//! export path is the documented remainder.

use std::collections::BTreeMap;

/// Byte-exact validation/diagnostic strings from the 2.1.207 binary. Kept as
/// public constants so any consumer (doctor/status/export) reuses the identical
/// wording.
pub mod messages {
    /// Empty stdout from the helper.
    pub const DID_NOT_RETURN_VALID: &str = "otelHeadersHelper did not return a valid value";
    /// Non-object (array / scalar / null) JSON.
    pub const MUST_RETURN_JSON_OBJECT: &str =
        "otelHeadersHelper must return a JSON object with string key-value pairs";
    /// Prefix for a non-string value entry; the binary appends `"<key>": <typeof>`.
    pub const NON_STRING_VALUE_PREFIX: &str =
        "otelHeadersHelper returned non-string value for key ";
    /// Prefix for the one-shot stderr line when the helper fails.
    pub const FAILED_UNAVAILABLE_PREFIX: &str =
        "otelHeadersHelper failed (OpenTelemetry export headers unavailable): ";
    /// Prefix used by the doctor/status surface reporting the cached last failure.
    pub const CONFIGURED_BUT_FAILED_PREFIX: &str =
        "otelHeadersHelper is configured but its last invocation failed: ";
    /// Prefix used when the settings-layer header read itself errors.
    pub const SETTINGS_READ_ERROR_PREFIX: &str =
        "Error getting OpenTelemetry headers from otelHeadersHelper (in settings): ";
}

/// JS `typeof` of a JSON value, used to build the
/// [`messages::NON_STRING_VALUE_PREFIX`] tail. Note `typeof null === "object"`
/// and arrays are `"object"` in JS.
#[must_use]
pub fn js_typeof(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        // `typeof null === "object"`; arrays and objects are `"object"` too.
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            "object"
        }
    }
}

/// Outcome of running the helper subprocess (mirrors the binary's `execa`-style
/// result object: `{failed,timedOut,exitCode,signal,stdout,stderr}`).
#[derive(Debug, Clone, Default)]
pub struct ExecOutcome {
    /// Whether the process failed (non-zero exit, signal, timeout, or spawn error).
    pub failed: bool,
    /// Whether the process was killed by the 30s timeout.
    pub timed_out: bool,
    /// Numeric exit code, if the process exited normally.
    pub exit_code: Option<i32>,
    /// Terminating signal name, if killed by a signal.
    pub signal: Option<String>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// Build the failure message for a failed [`ExecOutcome`], byte-faithful to the
/// binary:
/// ```text
/// if(o.timedOut) a="timed out";
/// else if(typeof o.exitCode==="number") a=`exited ${o.exitCode}`;
/// else if(o.signal) a=`was killed by ${o.signal}`;
/// else a="could not be started";
/// let l=o.stderr?.trim(); throw Error(l?`${a}: ${l}`:a)
/// ```
#[must_use]
pub fn build_failure_message(o: &ExecOutcome) -> String {
    let a = if o.timed_out {
        "timed out".to_string()
    } else if let Some(code) = o.exit_code {
        format!("exited {code}")
    } else if let Some(sig) = &o.signal {
        format!("was killed by {sig}")
    } else {
        "could not be started".to_string()
    };
    let l = o.stderr.trim();
    if l.is_empty() {
        a
    } else {
        format!("{a}: {l}")
    }
}

/// Validate helper stdout into a header map, byte-faithful to the binary's
/// post-exec validation block. On any violation returns the exact CC error
/// string.
///
/// # Errors
///
/// Returns the byte-exact CC error message when stdout is empty
/// ([`messages::DID_NOT_RETURN_VALID`]), is not a JSON object
/// ([`messages::MUST_RETURN_JSON_OBJECT`]; also on JSON parse failure, matching
/// CC's outer catch of the `JSON.parse` throw), or contains a non-string value
/// ([`messages::NON_STRING_VALUE_PREFIX`]`"<key>": <typeof>`).
pub fn validate_helper_output(stdout: &str) -> Result<BTreeMap<String, String>, String> {
    let i = stdout.trim();
    if i.is_empty() {
        return Err(messages::DID_NOT_RETURN_VALID.to_string());
    }
    // `Nt(i)` = JSON.parse; a throw here is caught by the outer catch and
    // surfaced as the raw parse-error message (a "failure", not the object
    // shape error). We mirror that: a parse error is a failure string.
    let parsed: serde_json::Value = match serde_json::from_str(i) {
        Ok(v) => v,
        Err(e) => return Err(e.to_string()),
    };
    // typeof !== "object" || null || Array ⇒ the object-shape error.
    let serde_json::Value::Object(obj) = &parsed else {
        return Err(messages::MUST_RETURN_JSON_OBJECT.to_string());
    };
    let mut out = BTreeMap::new();
    for (k, v) in obj {
        match v {
            serde_json::Value::String(s) => {
                out.insert(k.clone(), s.clone());
            }
            other => {
                return Err(format!(
                    "{}\"{}\": {}",
                    messages::NON_STRING_VALUE_PREFIX,
                    k,
                    js_typeof(other)
                ));
            }
        }
    }
    Ok(out)
}

/// Cache + debounce state machine for the helper (binary globals
/// `JPr`/`iRi`/`XPr`/`ujt`). Holds the last successful headers, the timestamp
/// they were cached (ms), and the last failure string. Not `Send`-shared here;
/// callers wrap it in whatever sync primitive their runtime needs.
#[derive(Debug, Clone, Default)]
pub struct HeadersHelperState {
    /// Debounce window in ms (from [`super::config::GateTimeouts`]).
    debounce_ms: i64,
    /// Last successful headers (`JPr`).
    cached_headers: Option<BTreeMap<String, String>>,
    /// Monotonic ms timestamp the headers were cached (`iRi`).
    cached_at_ms: i64,
    /// Last failure message (`XPr`); `Some` means the last attempt failed.
    last_failure: Option<String>,
    /// Whether the one-shot stderr line has already been emitted for the
    /// current failure (drives the binary's `XPr===null` stderr guard).
    failure_reported: bool,
}

/// What [`HeadersHelperState::resolve`] decided to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// The debounce window is still open; the cached headers were returned
    /// without re-running the helper.
    Cached(BTreeMap<String, String>),
    /// The helper ran and succeeded; headers were refreshed and cached.
    Refreshed(BTreeMap<String, String>),
    /// The helper ran and failed; an empty header map is returned (export
    /// proceeds header-less) and the failure is cached. `report_to_stderr` is
    /// `true` only the first time a failure is observed (binary `XPr===null`).
    Failed {
        /// The byte-exact failure message (`FAILED_UNAVAILABLE_PREFIX` tail).
        message: String,
        /// Whether the caller should emit the one-shot stderr line now.
        report_to_stderr: bool,
    },
}

impl HeadersHelperState {
    /// Construct a state machine with the configured debounce window.
    #[must_use]
    pub fn new(debounce_ms: i64) -> Self {
        HeadersHelperState {
            debounce_ms,
            ..Default::default()
        }
    }

    /// The cached last-failure string, if the last invocation failed (binary
    /// `ARi()` — powers the doctor/status
    /// [`messages::CONFIGURED_BUT_FAILED_PREFIX`] line).
    #[must_use]
    pub fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    /// Clear all cached state (binary `oSh()` — the settings-changed reset).
    pub fn reset(&mut self) {
        self.cached_headers = None;
        self.cached_at_ms = 0;
        self.last_failure = None;
        self.failure_reported = false;
    }

    /// Resolve the current headers, running `exec` only when the debounce
    /// window has elapsed (or nothing is cached yet). `now_ms` is the current
    /// monotonic clock in ms; `exec` performs the actual subprocess run and
    /// returns its [`ExecOutcome`] (injected so this crate needs no process
    /// dependency and tests are deterministic).
    ///
    /// Mirrors binary `RRi()`: cache-hit within debounce ⇒ [`ResolveOutcome::Cached`];
    /// otherwise run `exec`, validate, and cache success or failure.
    pub fn resolve(&mut self, now_ms: i64, exec: impl FnOnce() -> ExecOutcome) -> ResolveOutcome {
        // Debounce: `if(JPr&&Date.now()-iRi<t)return JPr`.
        if let Some(cached) = &self.cached_headers {
            if now_ms.saturating_sub(self.cached_at_ms) < self.debounce_ms {
                return ResolveOutcome::Cached(cached.clone());
            }
        }

        let outcome = exec();
        let result = if outcome.failed {
            Err(build_failure_message(&outcome))
        } else {
            validate_helper_output(&outcome.stdout)
        };

        match result {
            Ok(headers) => {
                // `return JPr=s,iRi=Date.now(),XPr=null,JPr`.
                self.cached_headers = Some(headers.clone());
                self.cached_at_ms = now_ms;
                self.last_failure = None;
                self.failure_reported = false;
                ResolveOutcome::Refreshed(headers)
            }
            Err(message) => {
                // `XPr===null && cn()` ⇒ report once, then `XPr=n`.
                let report_to_stderr = self.last_failure.is_none() && !self.failure_reported;
                self.last_failure = Some(message.clone());
                self.failure_reported = true;
                ResolveOutcome::Failed {
                    message,
                    report_to_stderr,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typeof_matches_js_semantics() {
        assert_eq!(js_typeof(&serde_json::json!(null)), "object");
        assert_eq!(js_typeof(&serde_json::json!(true)), "boolean");
        assert_eq!(js_typeof(&serde_json::json!(3)), "number");
        assert_eq!(js_typeof(&serde_json::json!("s")), "string");
        assert_eq!(js_typeof(&serde_json::json!([1, 2])), "object");
        assert_eq!(js_typeof(&serde_json::json!({"a":1})), "object");
    }

    #[test]
    fn validate_empty_stdout() {
        assert_eq!(
            validate_helper_output("   \n ").unwrap_err(),
            "otelHeadersHelper did not return a valid value"
        );
    }

    #[test]
    fn validate_non_object() {
        assert_eq!(
            validate_helper_output("[\"a\"]").unwrap_err(),
            "otelHeadersHelper must return a JSON object with string key-value pairs"
        );
        assert_eq!(
            validate_helper_output("42").unwrap_err(),
            "otelHeadersHelper must return a JSON object with string key-value pairs"
        );
        assert_eq!(
            validate_helper_output("null").unwrap_err(),
            "otelHeadersHelper must return a JSON object with string key-value pairs"
        );
    }

    #[test]
    fn validate_non_string_value() {
        // typeof number
        assert_eq!(
            validate_helper_output(r#"{"Authorization":123}"#).unwrap_err(),
            "otelHeadersHelper returned non-string value for key \"Authorization\": number"
        );
        // typeof object (nested)
        assert_eq!(
            validate_helper_output(r#"{"x":{"y":1}}"#).unwrap_err(),
            "otelHeadersHelper returned non-string value for key \"x\": object"
        );
        // typeof boolean
        assert_eq!(
            validate_helper_output(r#"{"b":true}"#).unwrap_err(),
            "otelHeadersHelper returned non-string value for key \"b\": boolean"
        );
    }

    #[test]
    fn validate_valid_object() {
        let h =
            validate_helper_output(r#"{"Authorization":"Bearer x","x-tenant":"acme"}"#).unwrap();
        assert_eq!(h.get("Authorization").map(String::as_str), Some("Bearer x"));
        assert_eq!(h.get("x-tenant").map(String::as_str), Some("acme"));
    }

    #[test]
    fn failure_message_variants() {
        assert_eq!(
            build_failure_message(&ExecOutcome {
                failed: true,
                timed_out: true,
                ..Default::default()
            }),
            "timed out"
        );
        assert_eq!(
            build_failure_message(&ExecOutcome {
                failed: true,
                exit_code: Some(3),
                ..Default::default()
            }),
            "exited 3"
        );
        assert_eq!(
            build_failure_message(&ExecOutcome {
                failed: true,
                signal: Some("SIGKILL".to_string()),
                ..Default::default()
            }),
            "was killed by SIGKILL"
        );
        assert_eq!(
            build_failure_message(&ExecOutcome {
                failed: true,
                ..Default::default()
            }),
            "could not be started"
        );
        // stderr suffix
        assert_eq!(
            build_failure_message(&ExecOutcome {
                failed: true,
                exit_code: Some(1),
                stderr: "  boom  ".to_string(),
                ..Default::default()
            }),
            "exited 1: boom"
        );
    }

    fn ok_exec(json: &'static str) -> impl FnOnce() -> ExecOutcome {
        move || ExecOutcome {
            failed: false,
            stdout: json.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_debounces_within_window() {
        let mut st = HeadersHelperState::new(1000);
        let first = st.resolve(0, ok_exec(r#"{"a":"1"}"#));
        assert!(matches!(first, ResolveOutcome::Refreshed(_)));

        // Within the window: cached, exec must NOT run (panics if it does).
        let cached = st.resolve(500, || panic!("exec must not run within debounce"));
        match cached {
            ResolveOutcome::Cached(h) => assert_eq!(h.get("a").map(String::as_str), Some("1")),
            other => panic!("expected Cached, got {other:?}"),
        }

        // After the window: re-runs.
        let refreshed = st.resolve(1000, ok_exec(r#"{"a":"2"}"#));
        match refreshed {
            ResolveOutcome::Refreshed(h) => assert_eq!(h.get("a").map(String::as_str), Some("2")),
            other => panic!("expected Refreshed, got {other:?}"),
        }
    }

    #[test]
    fn resolve_caches_failure_and_reports_once() {
        let mut st = HeadersHelperState::new(1000);
        let fail = || ExecOutcome {
            failed: true,
            exit_code: Some(2),
            stderr: "nope".to_string(),
            ..Default::default()
        };

        let first = st.resolve(0, fail);
        match first {
            ResolveOutcome::Failed {
                message,
                report_to_stderr,
            } => {
                assert_eq!(message, "exited 2: nope");
                assert!(report_to_stderr, "first failure reports to stderr");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(st.last_failure(), Some("exited 2: nope"));

        // A subsequent failure does NOT re-report to stderr (XPr!==null).
        let second = st.resolve(10, fail);
        match second {
            ResolveOutcome::Failed {
                report_to_stderr, ..
            } => assert!(!report_to_stderr, "second failure is silent"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn reset_clears_state() {
        let mut st = HeadersHelperState::new(1000);
        st.resolve(0, ok_exec(r#"{"a":"1"}"#));
        st.reset();
        assert_eq!(st.last_failure(), None);
        // After reset, exec runs again even at t=0 (no cache).
        let r = st.resolve(0, ok_exec(r#"{"a":"9"}"#));
        assert!(matches!(r, ResolveOutcome::Refreshed(_)));
    }
}
