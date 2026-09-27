//! OTEL **recording foundation** — the config-gated abstraction that the
//! ~20 `claude_code.*` instrument record sites will call, plus a default
//! byte-noop and an in-memory/console recorder for tests and local debug.
//!
//! ## What this lands (finding H-09, bounded)
//!
//! - [`MetricRecorder`] / [`LogRecorder`]: the two traits every record site
//!   depends on. Metrics carry the `claude_code.*` counter/histogram schema
//!   ([`super::metrics`]); logs carry the `claude_code.events` signal
//!   ([`super::logs`]) with the 2.1.214 attribute surface (`message.uuid`,
//!   `client_request_id`, `tool_source`, and the content-body attributes).
//! - [`NoopRecorder`]: the default when the master gate is off — every call is a
//!   byte-noop, matching CC (`o7u()` false ⇒ no meter/logger is ever built).
//! - [`InMemoryRecorder`]: consumes an [`OtelConfig`], accumulates counter sums
//!   and histogram value-lists keyed by (instrument, attribute-set), buffers log
//!   records, and honors the content-length cap ([`truncate_content`], byte-
//!   faithful to the binary `$1()` truncation marker). Doubles as the console
//!   exporter's backing store via [`InMemoryRecorder::render_console`].
//! - [`recorder_from_config`]: the factory — `Some(config)` when the gate is on
//!   yields the in-memory recorder; the gate-off path yields [`NoopRecorder`].
//!
//! ## Record-site coverage (live via [`super::runtime`])
//!
//! The OTLP/console/prometheus egress now exists ([`super::runtime`]), and the
//! record sites are wired — most through the analytics-bus bridge
//! (`crate::bus` mirrors every post-privacy-gate event into
//! `runtime::mirror_analytics_event`), the rest through direct
//! `runtime::record_*` helpers:
//!
//! - `session.count` — `install_process` at CLI start.
//! - `token.usage` / `cost.usage` / `llm_request` — bridged from
//!   `tengu_api_success` (+ failed/rate-limited/cancelled statuses).
//! - `lines_of_code.count` — `record_lines_of_code_change` (Edit/Write apply).
//! - `pull_request.count` / `commit.count` — `record_git_operation_counters`
//!   (the Bash tool's completion seam; the `mEo` counter subset, see
//!   [`super::git_ops`]).
//! - `tool.execution` / `tool.blocked_on_user` / `bash.subprocess` /
//!   `mcp.rpc` / `subagent.spawn` / `compaction` — bridged from the
//!   `tengu_tool_*` / compaction event families.
//! - `code_edit_tool.decision` — `record_tool_permission_decision` (attrs:
//!   `decision`, `source`, `tool_name`, `language` via
//!   [`super::code_language`]).
//! - `active_time.total` — REGISTERED but never recorded: the 2.1.220 binary
//!   defines the instrument (`Mt.activeTimeCounter`) and its getter yet has NO
//!   `add()` call site, so a record site here would be anti-parity.
//! - `hook` — `emit_hook_lifecycle`.
//! - `claude_code.events` logs — `assistant_response` (turn loop, gated),
//!   `tool_decision` (`record_tool_permission_decision`), and the
//!   `api_request` / `api_error` / `tool_result` CC-name translations in the
//!   bridge; large content attrs remain behind the [`super::logs`]
//!   `OTEL_LOG_*` opt-ins. `user_prompt` is emitted once from the shared
//!   prompt-submit seam before `UserPromptSubmit` hooks run.
//! - `claude_code.tracing` spans — start/stop lifecycle spans only.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::config::OtelConfig;

/// An OTEL attribute value — the scalar subset CC attaches to `claude_code.*`
/// instruments and `claude_code.events` records.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    /// String attribute (e.g. `model`, `tool_name`, `tool_source`, `message.uuid`).
    Str(String),
    /// Signed integer attribute (e.g. `duration_ms`, `attempt`).
    Int(i64),
    /// Floating-point attribute.
    Float(f64),
    /// Boolean attribute (e.g. `success`).
    Bool(bool),
}

impl AttrValue {
    /// Canonical, comparison-stable string form used to key an attribute set
    /// (floats via `{:?}` so `NaN`/precision are deterministic across a run).
    fn canonical(&self) -> String {
        match self {
            AttrValue::Str(s) => format!("s:{s}"),
            AttrValue::Int(i) => format!("i:{i}"),
            AttrValue::Float(f) => format!("f:{f:?}"),
            AttrValue::Bool(b) => format!("b:{b}"),
        }
    }
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        AttrValue::Str(v.to_string())
    }
}
impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        AttrValue::Str(v)
    }
}
impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        AttrValue::Int(v)
    }
}
impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        AttrValue::Bool(v)
    }
}
impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        AttrValue::Float(v)
    }
}

/// An ordered attribute set. `BTreeMap` gives a deterministic key ordering so the
/// canonical series key (and test assertions) are stable.
pub type Attributes = BTreeMap<String, AttrValue>;

/// Canonical, order-independent key for an (instrument, attribute-set) time
/// series — the accumulation bucket key. `BTreeMap` iteration is already sorted,
/// so equal attribute sets always produce the same key.
fn series_key(instrument: &str, attrs: &Attributes) -> String {
    let mut key = String::from(instrument);
    for (k, v) in attrs {
        key.push('\u{1f}'); // unit separator — safe against key/value collisions
        key.push_str(k);
        key.push('=');
        key.push_str(&v.canonical());
    }
    key
}

/// Result of applying the content-length cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedContent {
    /// The (possibly-truncated) content — carries the CC truncation marker when
    /// `truncated` is true and the budget left room for it.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
}

/// Apply the per-record content cap, byte-faithful to the binary `$1(e)`:
///
/// ```text
/// let t = max;
/// if (e.length <= t) return { content: e, truncated: false };
/// let n = `\n\n[TRUNCATED - Content exceeds ${t>=1024 ? `${Math.floor(t/1024)}KB` : `${t} character`} limit]`;
/// if (n.length >= t) return { content: e.slice(0, t), truncated: true };
/// return { content: e.slice(0, t - n.length) + n, truncated: true };
/// ```
///
/// `max` is the effective cap from [`super::config::compute_content_max_length`]
/// (a `<= 0` budget hard-truncates everything to the empty string, matching a
/// `NaN`/zero JS min feeding `String.slice`).
///
/// Length and slicing use UTF-16 code units (`String.length` / `String.slice`
/// semantics) so ASCII/BMP content matches CC exactly; a cut that would fall
/// mid-surrogate-pair is nudged to the nearest whole scalar (Rust cannot hold a
/// lone surrogate) — the only, documented, divergence from raw JS `slice`.
#[must_use]
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "t is clamped to >= 0; content lengths are well within usize/i64 range"
)]
pub fn truncate_content(content: &str, max: i64) -> TruncatedContent {
    let t = max.max(0);
    let len_utf16 = utf16_len(content);
    if (len_utf16 as i64) <= t {
        return TruncatedContent {
            content: content.to_string(),
            truncated: false,
        };
    }
    let t = t as usize;
    let marker = truncation_marker(t);
    let marker_len = utf16_len(&marker);
    if marker_len >= t {
        // No room for the marker — hard-cut to the budget.
        return TruncatedContent {
            content: slice_utf16(content, t),
            truncated: true,
        };
    }
    let head = slice_utf16(content, t - marker_len);
    TruncatedContent {
        content: format!("{head}{marker}"),
        truncated: true,
    }
}

/// The CC truncation marker (two leading newlines, per the 2.1.215 binary —
/// verified `\n\n[TRUNCATED…`, string-table length 0x1f=31): `\n\n[TRUNCATED -
/// Content exceeds {N}KB limit]` for `t >= 1024`, else `\n\n[TRUNCATED - Content
/// exceeds {t} character limit]` (singular `character`, matching the template).
fn truncation_marker(t: usize) -> String {
    if t >= 1024 {
        format!("\n\n[TRUNCATED - Content exceeds {}KB limit]", t / 1024)
    } else {
        format!("\n\n[TRUNCATED - Content exceeds {t} character limit]")
    }
}

/// UTF-16 code-unit length (`String.prototype.length`).
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Take the leading `n` UTF-16 code units, on a whole-scalar boundary. Never
/// splits a surrogate pair (see [`truncate_content`] docs).
fn slice_utf16(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in s.chars() {
        let w = ch.len_utf16();
        if units + w > n {
            break;
        }
        out.push(ch);
        units += w;
    }
    out
}

/// A single `claude_code.events` log record.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    /// Event name (the log body), e.g. `user_prompt`, `assistant_response`,
    /// `tool_result`, `api_request`. Corresponds to the binary `Ec(name, …)`
    /// first argument.
    pub event_name: String,
    /// Record attributes — the small metadata (`message.uuid`, `model`,
    /// `tool_name`, `tool_source`, `client_request_id`, …) plus any content body
    /// attributes (already passed through [`truncate_content`] by the caller when
    /// the corresponding `OTEL_LOG_*` gate is on).
    pub attributes: Attributes,
}

/// Records `claude_code.*` counters and histograms.
pub trait MetricRecorder: Send + Sync {
    /// Add `value` to the `claude_code.*` **counter** named `instrument`, keyed by
    /// `attrs` (the binary `counter.add(value, attrs)`).
    fn add_counter(&self, instrument: &str, value: f64, attrs: &Attributes);

    /// Record `value` into the `claude_code.*` **histogram** named `instrument`,
    /// keyed by `attrs` (the binary `histogram.record(value, attrs)`).
    fn record_histogram(&self, instrument: &str, value: f64, attrs: &Attributes);
}

/// Emits `claude_code.events` log records.
pub trait LogRecorder: Send + Sync {
    /// Emit one log record (the binary `logger.emit({ body, attributes })`).
    fn emit_log(&self, record: LogRecord);
}

/// Default byte-noop recorder: used when the master gate is off. Every method is
/// an empty body, so with the gate off the subsystem never allocates or mutates
/// — matching CC, which never constructs a meter/logger when `o7u()` is false.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRecorder;

impl MetricRecorder for NoopRecorder {
    #[inline]
    fn add_counter(&self, _instrument: &str, _value: f64, _attrs: &Attributes) {}
    #[inline]
    fn record_histogram(&self, _instrument: &str, _value: f64, _attrs: &Attributes) {}
}

impl LogRecorder for NoopRecorder {
    #[inline]
    fn emit_log(&self, _record: LogRecord) {}
}

/// One accumulated counter series.
#[derive(Debug, Clone, PartialEq)]
pub struct CounterSeries {
    /// Instrument name (e.g. `claude_code.token.usage`).
    pub instrument: String,
    /// The attribute set for this series.
    pub attributes: Attributes,
    /// Cumulative sum of all `add_counter` calls for this series.
    pub value: f64,
}

/// One accumulated histogram series.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramSeries {
    /// Instrument name (e.g. `claude_code.mcp.rpc`).
    pub instrument: String,
    /// The attribute set for this series.
    pub attributes: Attributes,
    /// Every recorded value, in call order.
    pub values: Vec<f64>,
}

#[derive(Debug, Default)]
struct RecorderState {
    counters: BTreeMap<String, CounterSeries>,
    histograms: BTreeMap<String, HistogramSeries>,
    logs: Vec<LogRecord>,
}

/// Config-gated, in-memory recorder. Accumulates counter sums and histogram
/// value-lists per (instrument, attribute-set) series, buffers log records, and
/// exposes the content cap resolved from the [`OtelConfig`]. When the config's
/// master gate is off it behaves exactly like [`NoopRecorder`] (every mutation is
/// dropped), preserving byte-noop parity even if a site wrongly holds one.
#[derive(Debug)]
pub struct InMemoryRecorder {
    enabled: bool,
    content_max_length: i64,
    state: Mutex<RecorderState>,
}

impl InMemoryRecorder {
    /// Build from a resolved [`OtelConfig`]. Reads the master gate and the
    /// content cap; nothing here touches the process env.
    #[must_use]
    pub fn new(config: &OtelConfig) -> Self {
        InMemoryRecorder {
            enabled: config.enabled,
            content_max_length: config.content_max_length,
            state: Mutex::new(RecorderState::default()),
        }
    }

    /// Whether the master gate is on for this recorder.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The effective content-length cap this recorder applies (from the config).
    #[must_use]
    pub fn content_max_length(&self) -> i64 {
        self.content_max_length
    }

    /// Apply this recorder's content cap to `content` (convenience wrapper over
    /// [`truncate_content`]). Record sites call this on the large body attributes
    /// before attaching them, exactly where the binary calls `$1()`.
    #[must_use]
    pub fn cap_content(&self, content: &str) -> TruncatedContent {
        truncate_content(content, self.content_max_length)
    }

    /// Snapshot of accumulated counter series, sorted by canonical series key.
    #[must_use]
    pub fn counters(&self) -> Vec<CounterSeries> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counters
            .values()
            .cloned()
            .collect()
    }

    /// Snapshot of accumulated histogram series, sorted by canonical series key.
    #[must_use]
    pub fn histograms(&self) -> Vec<HistogramSeries> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .histograms
            .values()
            .cloned()
            .collect()
    }

    /// Buffered log records, in emit order.
    #[must_use]
    pub fn logs(&self) -> Vec<LogRecord> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .logs
            .clone()
    }

    /// Cumulative value of a single counter series (0.0 if never recorded).
    #[must_use]
    pub fn counter_value(&self, instrument: &str, attrs: &Attributes) -> f64 {
        let key = series_key(instrument, attrs);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counters
            .get(&key)
            .map_or(0.0, |s| s.value)
    }

    /// Recorded values of a single histogram series (empty if never recorded).
    #[must_use]
    pub fn histogram_values(&self, instrument: &str, attrs: &Attributes) -> Vec<f64> {
        let key = series_key(instrument, attrs);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .histograms
            .get(&key)
            .map(|s| s.values.clone())
            .unwrap_or_default()
    }

    /// Render the accumulated state as a stable, human-readable console dump —
    /// the backing formatter for the `console` exporter selection. Deterministic
    /// ordering (series keys sort, logs stay in emit order).
    #[must_use]
    pub fn render_console(&self) -> String {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = String::new();
        for s in state.counters.values() {
            out.push_str(&format!(
                "counter {} {} {:?}\n",
                s.instrument, s.value, s.attributes
            ));
        }
        for s in state.histograms.values() {
            out.push_str(&format!(
                "histogram {} {:?} {:?}\n",
                s.instrument, s.values, s.attributes
            ));
        }
        for r in &state.logs {
            out.push_str(&format!("log {} {:?}\n", r.event_name, r.attributes));
        }
        out
    }
}

impl MetricRecorder for InMemoryRecorder {
    fn add_counter(&self, instrument: &str, value: f64, attrs: &Attributes) {
        if !self.enabled {
            return; // gate off ⇒ byte-noop
        }
        let key = series_key(instrument, attrs);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .counters
            .entry(key)
            .or_insert_with(|| CounterSeries {
                instrument: instrument.to_string(),
                attributes: attrs.clone(),
                value: 0.0,
            })
            .value += value;
    }

    fn record_histogram(&self, instrument: &str, value: f64, attrs: &Attributes) {
        if !self.enabled {
            return;
        }
        let key = series_key(instrument, attrs);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .histograms
            .entry(key)
            .or_insert_with(|| HistogramSeries {
                instrument: instrument.to_string(),
                attributes: attrs.clone(),
                values: Vec::new(),
            })
            .values
            .push(value);
    }
}

impl LogRecorder for InMemoryRecorder {
    fn emit_log(&self, record: LogRecord) {
        if !self.enabled {
            return;
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .logs
            .push(record);
    }
}

/// A recorder that satisfies both metric and log recording.
pub trait Recorder: MetricRecorder + LogRecorder {}
impl<T: MetricRecorder + LogRecorder> Recorder for T {}

/// The boot factory: `Some(config)` (gate on) yields an [`InMemoryRecorder`];
/// `None` (gate off — the [`super::init_from_env`] byte-noop path) yields a
/// [`NoopRecorder`]. Returned boxed so the ~20 record sites depend only on the
/// trait, letting follow-up (1) swap in the OTLP recorder without touching them.
#[must_use]
pub fn recorder_from_config(config: Option<&OtelConfig>) -> Box<dyn Recorder> {
    match config {
        Some(cfg) if cfg.enabled => Box::new(InMemoryRecorder::new(cfg)),
        _ => Box::new(NoopRecorder),
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "exact-value assertions on integral f64 sums and small test-fixture casts"
)]
mod tests {
    use super::super::config::{DEFAULT_CONTENT_MAX_LENGTH, ENV_ENABLE_TELEMETRY};
    use super::*;

    fn enabled_config() -> OtelConfig {
        OtelConfig::from_lookup(|k| (k == ENV_ENABLE_TELEMETRY).then(|| "1".to_string()))
    }

    fn disabled_config() -> OtelConfig {
        OtelConfig::from_lookup(|_| None)
    }

    fn attrs(pairs: &[(&str, AttrValue)]) -> Attributes {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn counter_accumulates_per_series() {
        let rec = InMemoryRecorder::new(&enabled_config());
        let a = attrs(&[("model", "sonnet".into()), ("type", "input".into())]);
        let b = attrs(&[("model", "sonnet".into()), ("type", "output".into())]);
        rec.add_counter(super::super::metrics::TOKEN_USAGE, 100.0, &a);
        rec.add_counter(super::super::metrics::TOKEN_USAGE, 50.0, &a);
        rec.add_counter(super::super::metrics::TOKEN_USAGE, 7.0, &b);

        assert_eq!(
            rec.counter_value(super::super::metrics::TOKEN_USAGE, &a),
            150.0
        );
        assert_eq!(
            rec.counter_value(super::super::metrics::TOKEN_USAGE, &b),
            7.0
        );
        // Two distinct series accumulated.
        assert_eq!(rec.counters().len(), 2);
    }

    #[test]
    fn attribute_order_does_not_split_series() {
        let rec = InMemoryRecorder::new(&enabled_config());
        // Same pairs, different insertion order ⇒ same series (BTreeMap sorts).
        let a: Attributes = [("b", AttrValue::Int(2)), ("a", AttrValue::Int(1))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let b: Attributes = [("a", AttrValue::Int(1)), ("b", AttrValue::Int(2))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        rec.add_counter(super::super::metrics::SESSION_COUNT, 1.0, &a);
        rec.add_counter(super::super::metrics::SESSION_COUNT, 1.0, &b);
        assert_eq!(rec.counters().len(), 1);
        assert_eq!(
            rec.counter_value(super::super::metrics::SESSION_COUNT, &a),
            2.0
        );
    }

    #[test]
    fn histogram_keeps_every_value() {
        let rec = InMemoryRecorder::new(&enabled_config());
        let a = attrs(&[("tool_name", "Bash".into()), ("success", true.into())]);
        for v in [12.0, 3.5, 99.0] {
            rec.record_histogram(super::super::metrics::MCP_RPC, v, &a);
        }
        assert_eq!(
            rec.histogram_values(super::super::metrics::MCP_RPC, &a),
            vec![12.0, 3.5, 99.0]
        );
        assert_eq!(rec.histograms().len(), 1);
    }

    #[test]
    fn log_records_buffer_in_order() {
        let rec = InMemoryRecorder::new(&enabled_config());
        rec.emit_log(LogRecord {
            event_name: "user_prompt".to_string(),
            attributes: attrs(&[("message.uuid", "u-1".into())]),
        });
        rec.emit_log(LogRecord {
            event_name: "assistant_response".to_string(),
            attributes: attrs(&[
                ("message.uuid", "u-2".into()),
                ("client_request_id", "req-9".into()),
            ]),
        });
        let logs = rec.logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].event_name, "user_prompt");
        assert_eq!(logs[1].event_name, "assistant_response");
        assert_eq!(
            logs[1].attributes.get("client_request_id"),
            Some(&AttrValue::Str("req-9".to_string()))
        );
    }

    #[test]
    fn tool_source_attribute_round_trips() {
        // 2.1.214 `tool_source` (binary `lVn`): builtin / mcp / sdk_host_builtin_mcp.
        let rec = InMemoryRecorder::new(&enabled_config());
        let a = attrs(&[
            ("tool_name", "mcp__x__y".into()),
            ("tool_source", "mcp".into()),
        ]);
        rec.add_counter(super::super::metrics::TOOL_EXECUTION, 1.0, &a);
        assert_eq!(
            rec.counters()[0].attributes.get("tool_source"),
            Some(&"mcp".into())
        );
    }

    #[test]
    fn disabled_config_is_byte_noop() {
        let rec = InMemoryRecorder::new(&disabled_config());
        assert!(!rec.is_enabled());
        let a = attrs(&[("model", "x".into())]);
        rec.add_counter(super::super::metrics::COST_USAGE, 1.0, &a);
        rec.record_histogram(super::super::metrics::HOOK, 5.0, &a);
        rec.emit_log(LogRecord {
            event_name: "user_prompt".to_string(),
            attributes: Attributes::new(),
        });
        assert!(rec.counters().is_empty());
        assert!(rec.histograms().is_empty());
        assert!(rec.logs().is_empty());
    }

    #[test]
    fn noop_recorder_drops_everything() {
        let rec = NoopRecorder;
        let a = Attributes::new();
        rec.add_counter("claude_code.session.count", 1.0, &a);
        rec.record_histogram("claude_code.hook", 1.0, &a);
        rec.emit_log(LogRecord {
            event_name: "x".to_string(),
            attributes: a,
        });
        // No state, no panic — pure noop.
    }

    #[test]
    fn factory_gate_selects_recorder() {
        let on = enabled_config();
        let off = disabled_config();
        // Gate on: records land (verified via a downcast-free behavioral probe —
        // we re-create the in-memory recorder to observe accumulation).
        let rec_on = InMemoryRecorder::new(&on);
        rec_on.add_counter("claude_code.session.count", 1.0, &Attributes::new());
        assert_eq!(rec_on.counters().len(), 1);
        // Gate off ⇒ factory yields a byte-noop.
        let boxed = recorder_from_config(Some(&off));
        boxed.add_counter("claude_code.session.count", 1.0, &Attributes::new());
        boxed.emit_log(LogRecord {
            event_name: "user_prompt".to_string(),
            attributes: Attributes::new(),
        });
        // Nothing to assert on a Noop besides "did not panic"; the None path:
        let none = recorder_from_config(None);
        none.record_histogram("claude_code.hook", 1.0, &Attributes::new());
    }

    // -- content cap ---------------------------------------------------------

    #[test]
    fn short_content_is_untouched() {
        let out = truncate_content("hello", DEFAULT_CONTENT_MAX_LENGTH);
        assert!(!out.truncated);
        assert_eq!(out.content, "hello");
    }

    #[test]
    fn content_at_exactly_cap_is_untouched() {
        let s = "x".repeat(10);
        let out = truncate_content(&s, 10);
        assert!(!out.truncated);
        assert_eq!(out.content, s);
    }

    #[test]
    fn small_cap_uses_singular_character_marker() {
        // t < 1024 ⇒ "{t} character limit"; content longer than t.
        let s = "x".repeat(100);
        let out = truncate_content(&s, 60);
        assert!(out.truncated);
        let marker = "\n\n[TRUNCATED - Content exceeds 60 character limit]";
        assert!(out.content.ends_with(marker), "got: {:?}", out.content);
        // head + marker == exactly the budget (60).
        assert_eq!(utf16_len(&out.content), 60);
    }

    #[test]
    fn large_cap_uses_kb_marker() {
        let cap = 2048; // 2 KiB
        let s = "y".repeat(5000);
        let out = truncate_content(&s, cap);
        assert!(out.truncated);
        let marker = "\n\n[TRUNCATED - Content exceeds 2KB limit]";
        assert!(
            out.content.ends_with(marker),
            "got tail: {:?}",
            &out.content[out.content.len().saturating_sub(60)..]
        );
        assert_eq!(utf16_len(&out.content), cap as usize);
    }

    #[test]
    fn cap_smaller_than_marker_hard_cuts() {
        // marker length >= t ⇒ plain slice(0, t), no marker.
        let s = "abcdefghij";
        let out = truncate_content(s, 5);
        assert!(out.truncated);
        assert_eq!(out.content, "abcde");
    }

    #[test]
    fn zero_cap_truncates_to_empty() {
        let out = truncate_content("anything", 0);
        assert!(out.truncated);
        assert_eq!(out.content, "");
    }

    #[test]
    fn negative_cap_clamps_to_zero() {
        let out = truncate_content("anything", -5);
        assert!(out.truncated);
        assert_eq!(out.content, "");
    }

    #[test]
    fn cap_content_uses_config_length() {
        let cfg = OtelConfig::from_lookup(|k| match k {
            _ if k == ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "LINGXI_OTEL_CONTENT_MAX_LENGTH" => Some("50".to_string()),
            _ => None,
        });
        let rec = InMemoryRecorder::new(&cfg);
        assert_eq!(rec.content_max_length(), 50);
        let out = rec.cap_content(&"z".repeat(200));
        assert!(out.truncated);
        assert_eq!(utf16_len(&out.content), 50);
    }

    #[test]
    fn utf16_length_counts_astral_as_two() {
        // "😀" is one scalar, two UTF-16 code units.
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_len("a😀b"), 4);
    }
}
