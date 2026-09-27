//! OpenTelemetry monitoring config: the full env surface CC 2.1.207 consumes.
//!
//! This module parses the enterprise "Monitoring" env surface into a typed
//! [`OtelConfig`]. It is the **load-bearing parity** for H-BIN-06: the exact
//! env-var names, precedence rules, boolean/numeric parse semantics, and
//! defaults must match the binary byte-for-byte. Actual OTLP wire egress (the
//! SDK exporters/readers that consume this config) is the documented remainder.
//!
//! ## Rebrand policy (see the repo env-var rule)
//!
//! The four CC gate-family vars `CLAUDE_CODE_OTEL_*` are renamed to
//! `LINGXI_OTEL_*` (matching the existing `LINGXI_OTEL_DIAG_STDERR` sibling in
//! the hook-env denylist), and the master toggle `CLAUDE_CODE_ENABLE_TELEMETRY`
//! is renamed to `LINGXI_ENABLE_TELEMETRY`. Every **standard OpenTelemetry
//! spec** var (`OTEL_*`) keeps its canonical spelling — those names are part of
//! the `OTel` wire contract and enterprise collectors/dashboards key on them, so
//! renaming would break interop (the same reasoning that keeps `tengu_*` names).

use std::collections::BTreeMap;

/// Provenance of a telemetry config value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigValueSource {
    /// Ordinary process environment / non-managed caller lookup.
    Runtime,
    /// Enterprise managed policy settings.
    Managed,
}

/// One looked-up telemetry config value plus its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValue {
    /// The raw string value.
    pub value: String,
    /// Where the value came from.
    pub source: ConfigValueSource,
}

/// Master enable gate (rebrand of CC `CLAUDE_CODE_ENABLE_TELEMETRY`, binary
/// `o7u(){return ct(process.env.CLAUDE_CODE_ENABLE_TELEMETRY)}`).
pub const ENV_ENABLE_TELEMETRY: &str = "LINGXI_ENABLE_TELEMETRY";

/// Explicit opt-in required before detailed OpenTelemetry traces are exported.
///
/// Unlike the rebranded LingXi gate family, this keeps Claude Code's canonical
/// spelling because managed enterprise settings and deployment templates use
/// this beta-contract name directly.
pub const ENV_ENHANCED_TELEMETRY_BETA: &str = "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA";

/// Force-flush deadline in ms (rebrand of CC `CLAUDE_CODE_OTEL_FLUSH_TIMEOUT_MS`,
/// binary default `vde(...,5000)`).
pub const ENV_FLUSH_TIMEOUT_MS: &str = "LINGXI_OTEL_FLUSH_TIMEOUT_MS";

/// Shutdown deadline in ms (rebrand of CC `CLAUDE_CODE_OTEL_SHUTDOWN_TIMEOUT_MS`,
/// binary default `vde(...,2000)`).
pub const ENV_SHUTDOWN_TIMEOUT_MS: &str = "LINGXI_OTEL_SHUTDOWN_TIMEOUT_MS";

/// `otelHeadersHelper` re-invocation debounce window in ms (rebrand of CC
/// `CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS`, binary default `nSh=1740000`).
pub const ENV_HEADERS_HELPER_DEBOUNCE_MS: &str = "LINGXI_OTEL_HEADERS_HELPER_DEBOUNCE_MS";

/// Route OpenTelemetry SDK internal diagnostics to stderr (rebrand of CC
/// `CLAUDE_CODE_OTEL_DIAG_STDERR`; already present in the hook-env denylist).
pub const ENV_DIAG_STDERR: &str = "LINGXI_OTEL_DIAG_STDERR";

/// Per-record content length cap in characters (rebrand of CC
/// `CLAUDE_CODE_OTEL_CONTENT_MAX_LENGTH`; binary default `frg=61440`). Applied to
/// the potentially-large body attributes on the `claude_code.events` log signal
/// (`prompt` / `response` / `tool_result` / `tool_parameters`). See
/// [`compute_content_max_length`] for the full `Math.min` precedence against the
/// three standard `OTEL_*_VALUE_LENGTH_LIMIT` spec vars.
pub const ENV_CONTENT_MAX_LENGTH: &str = "LINGXI_OTEL_CONTENT_MAX_LENGTH";

// -- Standard OpenTelemetry value-length-limit spec vars (verbatim; the content
//    cap is the `Math.min` of the gate var and these). --------------------------

/// Generic attribute value length limit (`OTEL_ATTRIBUTE_VALUE_LENGTH_LIMIT`).
pub const ENV_ATTRIBUTE_VALUE_LENGTH_LIMIT: &str = "OTEL_ATTRIBUTE_VALUE_LENGTH_LIMIT";
/// Log-record attribute value length limit (`OTEL_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT`).
pub const ENV_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT: &str =
    "OTEL_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT";
/// Span attribute value length limit (`OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT`).
pub const ENV_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT: &str = "OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT";

// -- Standard OpenTelemetry spec vars (kept verbatim; NEVER rebranded) --------

/// Per-signal metrics exporter selection (`otlp` / `console` / `prometheus` / `none`).
pub const ENV_METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";
/// Per-signal logs exporter selection.
pub const ENV_LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";
/// Per-signal traces exporter selection.
pub const ENV_TRACES_EXPORTER: &str = "OTEL_TRACES_EXPORTER";

/// Generic OTLP endpoint (base; per-signal vars override).
pub const ENV_OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// Generic OTLP headers (comma list of `k=v`; per-signal vars override).
pub const ENV_OTLP_HEADERS: &str = "OTEL_EXPORTER_OTLP_HEADERS";
/// Generic OTLP protocol (`grpc` / `http/protobuf` / `http/json`; per-signal overrides).
pub const ENV_OTLP_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
/// Generic OTLP compression (`gzip` / `none`).
pub const ENV_OTLP_COMPRESSION: &str = "OTEL_EXPORTER_OTLP_COMPRESSION";
/// Generic OTLP request timeout in ms.
pub const ENV_OTLP_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TIMEOUT";
/// Generic OTLP insecure-transport toggle.
pub const ENV_OTLP_INSECURE: &str = "OTEL_EXPORTER_OTLP_INSECURE";
/// Server CA certificate path (TLS verify).
pub const ENV_OTLP_CERTIFICATE: &str = "OTEL_EXPORTER_OTLP_CERTIFICATE";
/// Client private-key path (mTLS).
pub const ENV_OTLP_CLIENT_KEY: &str = "OTEL_EXPORTER_OTLP_CLIENT_KEY";
/// Client certificate path (mTLS).
pub const ENV_OTLP_CLIENT_CERTIFICATE: &str = "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE";

/// Metric reader export interval in ms (binary default `oJg=60000`).
pub const ENV_METRIC_EXPORT_INTERVAL: &str = "OTEL_METRIC_EXPORT_INTERVAL";
/// Log record export interval in ms (binary default `5000`).
pub const ENV_LOGS_EXPORT_INTERVAL: &str = "OTEL_LOGS_EXPORT_INTERVAL";

/// Resource attributes (`k=v,k=v`), merged into the OTLP resource.
pub const ENV_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";
/// `service.name` resource attribute override (binary default `"claude-code"`).
pub const ENV_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";

/// Traces sampler selection (e.g. `parentbased_always_on`).
pub const ENV_TRACES_SAMPLER: &str = "OTEL_TRACES_SAMPLER";
/// Traces sampler argument (e.g. ratio for `traceidratio`).
pub const ENV_TRACES_SAMPLER_ARG: &str = "OTEL_TRACES_SAMPLER_ARG";

/// Binary default for [`ENV_FLUSH_TIMEOUT_MS`] (`vde(...,5000)`).
pub const DEFAULT_FLUSH_TIMEOUT_MS: i64 = 5000;
/// Binary default for [`ENV_SHUTDOWN_TIMEOUT_MS`] (`vde(...,2000)`).
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: i64 = 2000;
/// Binary default for [`ENV_HEADERS_HELPER_DEBOUNCE_MS`] (`nSh=1740000`, 29 min).
pub const DEFAULT_HEADERS_HELPER_DEBOUNCE_MS: i64 = 1_740_000;
/// Binary default for [`ENV_METRIC_EXPORT_INTERVAL`] (`oJg=60000`).
pub const DEFAULT_METRIC_EXPORT_INTERVAL_MS: i64 = 60_000;
/// Binary default for [`ENV_LOGS_EXPORT_INTERVAL`] (`5000`).
pub const DEFAULT_LOGS_EXPORT_INTERVAL_MS: i64 = 5000;
/// Binary default for `service.name` (`t["service.name"]||"claude-code"`).
pub const DEFAULT_SERVICE_NAME: &str = "claude-code";
/// Binary default for [`ENV_CONTENT_MAX_LENGTH`] (`var frg=61440`, i.e. 60 KiB).
pub const DEFAULT_CONTENT_MAX_LENGTH: i64 = 61440;

/// JS `Number(s)` coercion, byte-faithful enough for the numeric env vars CC
/// feeds to `Math.min` in the content-cap computation (`mrg()`): the raw string
/// is compared/min-ed as a JS number, NOT via `parseInt`. Trimmed; empty ⇒ `0`
/// (JS `Number("")===0`); an otherwise-unparseable value ⇒ `NaN`.
///
/// Realistic length-limit env values are plain decimal integers, for which this
/// matches JS exactly. The rare JS-only spellings (hex `0x…`, `Infinity`) fall
/// through to `NaN` here — documented divergence, immaterial to the cap.
#[must_use]
pub fn js_number(raw: &str) -> f64 {
    let t = raw.trim();
    if t.is_empty() {
        return 0.0; // JS: Number("") === 0
    }
    // Rust's f64 parse accepts "inf"/"infinity"/"nan" case-insensitively and all
    // decimal int/float forms — a superset of what we need, with hex ⇒ NaN.
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// JS `Math.min` semantics: **any** `NaN` argument makes the whole result `NaN`
/// (Rust's [`f64::min`] instead *ignores* NaN, which would silently pick a
/// finite fallback — the wrong behavior for parity).
#[must_use]
fn js_min(values: &[f64]) -> f64 {
    let mut acc = f64::INFINITY;
    for &v in values {
        if v.is_nan() {
            return f64::NAN;
        }
        if v < acc {
            acc = v;
        }
    }
    acc
}

/// Compute the effective per-record content length cap, byte-faithful to the
/// binary `mrg()`:
/// `Math.min(CLAUDE_CODE_OTEL_CONTENT_MAX_LENGTH ?? 61440,
///           OTEL_ATTRIBUTE_VALUE_LENGTH_LIMIT ?? Infinity,
///           OTEL_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT ?? Infinity,
///           OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT ?? Infinity)`.
///
/// Returned as an `i64` character budget: a `NaN` min (an unparseable explicit
/// override) clamps to `0`, `+Infinity` (impossible while the content default is
/// finite) clamps to [`i64::MAX`], and a fractional value floors toward zero —
/// all matching how JS would then feed the value into `String.slice`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "modeling JS Number/Math.min semantics; realistic caps are well within f64/i64 range"
)]
pub fn compute_content_max_length(get: &impl Fn(&str) -> Option<String>) -> i64 {
    let num = |var: &str, default: f64| -> f64 {
        match get(var) {
            None => default,
            Some(v) => js_number(&v),
        }
    };
    let m = js_min(&[
        num(ENV_CONTENT_MAX_LENGTH, DEFAULT_CONTENT_MAX_LENGTH as f64),
        num(ENV_ATTRIBUTE_VALUE_LENGTH_LIMIT, f64::INFINITY),
        num(ENV_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT, f64::INFINITY),
        num(ENV_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT, f64::INFINITY),
    ]);
    if m.is_nan() {
        return 0;
    }
    if m >= i64::MAX as f64 {
        return i64::MAX;
    }
    if m <= 0.0 {
        return 0;
    }
    m.floor() as i64
}

/// Truthy env-var parse, byte-faithful to the binary `ct()`:
/// `if(!e)return!1;` then lower-case + trim and membership-test against
/// `["1","true","yes","on"]`. Anything else is falsy.
#[must_use]
pub fn env_truthy(raw: &str) -> bool {
    let t = raw.trim().to_ascii_lowercase();
    matches!(t.as_str(), "1" | "true" | "yes" | "on")
}

/// Boolean env with a default, byte-faithful to the binary `hNr()`:
/// `if(r===void 0)return t; return ct(r)`. Absent ⇒ `default`; present ⇒
/// [`env_truthy`] (an explicit non-truthy value like `"0"` overrides a
/// `true` default with `false`).
#[must_use]
pub fn bool_env(raw: Option<&str>, default: bool) -> bool {
    match raw {
        None => default,
        Some(v) => env_truthy(v),
    }
}

/// Integer env with a default, byte-faithful to the binary `vde()`:
/// `if(e===void 0)return t; let r=parseInt(e,10); return Number.isNaN(r)?t:r`.
///
/// Uses JS `parseInt` base-10 semantics (leading optional sign + digit run;
/// trailing non-digits ignored; no leading digit ⇒ `NaN` ⇒ default), which is
/// *looser* than Rust's `str::parse` — `"5000ms"` parses to `5000`, matching CC.
#[must_use]
pub fn int_env(raw: Option<&str>, default: i64) -> i64 {
    match raw {
        None => default,
        Some(v) => parse_int_js(v).unwrap_or(default),
    }
}

/// JS `parseInt(s, 10)`: skip leading ASCII whitespace, optional `+`/`-`, then
/// consume the leading run of decimal digits. Returns `None` (`NaN`) when no
/// digit follows the optional sign.
#[must_use]
pub fn parse_int_js(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None; // no digits ⇒ NaN
    }
    // `i128` accumulation avoids overflow on absurd inputs; clamp into i64.
    let mut acc: i128 = 0;
    for &b in &bytes[start..i] {
        acc = acc * 10 + i128::from(b - b'0');
        if acc > i128::from(i64::MAX) {
            acc = i128::from(i64::MAX);
            break;
        }
    }
    if neg {
        acc = -acc;
    }
    // `acc` is already clamped into the i64 range above, so this never fails.
    Some(i64::try_from(acc.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).unwrap_or(0))
}

/// A single telemetry signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Metrics signal (`OTEL_METRICS_EXPORTER`, `OTEL_METRIC_EXPORT_INTERVAL`).
    Metrics,
    /// Logs signal (`OTEL_LOGS_EXPORTER`, `OTEL_LOGS_EXPORT_INTERVAL`).
    Logs,
    /// Traces signal (`OTEL_TRACES_EXPORTER`, sampler vars).
    Traces,
}

impl Signal {
    /// The `OTEL_EXPORTER_OTLP_<SIGNAL>_` infix used by per-signal override vars
    /// (`METRICS` / `LOGS` / `TRACES`).
    #[must_use]
    pub fn otlp_infix(self) -> &'static str {
        match self {
            Signal::Metrics => "METRICS",
            Signal::Logs => "LOGS",
            Signal::Traces => "TRACES",
        }
    }
}

/// Exporter selection for a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExporterKind {
    /// OTLP exporter (`otlp`) — the default when the selection var is unset.
    Otlp,
    /// Console/debug exporter (`console`).
    Console,
    /// Prometheus pull exporter (`prometheus`; metrics only).
    Prometheus,
    /// Explicitly disabled (`none`).
    None,
    /// Unrecognised selection — kept verbatim (CC throws
    /// `Unknown exporter type` at reader-build time; we defer that to egress).
    Other(String),
}

impl ExporterKind {
    /// Parse a raw selection string. Empty/unset ⇒ [`ExporterKind::Otlp`]
    /// (`OTel` spec default). Case-insensitive on the four known kinds.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            None | Some("") => ExporterKind::Otlp,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "otlp" => ExporterKind::Otlp,
                "console" => ExporterKind::Console,
                "prometheus" => ExporterKind::Prometheus,
                "none" => ExporterKind::None,
                _ => ExporterKind::Other(v.to_string()),
            },
        }
    }
}

/// OTLP transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    /// gRPC transport (`grpc`).
    Grpc,
    /// HTTP + protobuf body (`http/protobuf`) — `OTel` spec default.
    HttpProtobuf,
    /// HTTP + JSON body (`http/json`).
    HttpJson,
}

impl OtlpProtocol {
    /// Parse a protocol string. Unset ⇒ [`OtlpProtocol::HttpProtobuf`] (spec
    /// default). Unknown values also fall back to the default (CC surfaces the
    /// bad value via the `OTEL_EXPORTER_OTLP_..._PROTOCOL env var:` diagnostic
    /// at egress time).
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("grpc") => OtlpProtocol::Grpc,
            Some("http/json") => OtlpProtocol::HttpJson,
            _ => OtlpProtocol::HttpProtobuf,
        }
    }
}

/// `OTEL_METRICS_INCLUDE_*` attribute toggles, with the binary defaults from
/// `C4h` (`SESSION_ID:true, VERSION:false, ACCOUNT_UUID:true, ENTRYPOINT:false,
/// RESOURCE_ATTRIBUTES:true`).
// Deliberately one bool per CC env flag — a faithful mirror of the binary's
// `C4h` map, not a state struct to be collapsed.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsInclude {
    /// `OTEL_METRICS_INCLUDE_SESSION_ID` (default `true`).
    pub session_id: bool,
    /// `OTEL_METRICS_INCLUDE_VERSION` (default `false`).
    pub version: bool,
    /// `OTEL_METRICS_INCLUDE_ACCOUNT_UUID` (default `true`).
    pub account_uuid: bool,
    /// `OTEL_METRICS_INCLUDE_ENTRYPOINT` (default `false`).
    pub entrypoint: bool,
    /// `OTEL_METRICS_INCLUDE_RESOURCE_ATTRIBUTES` (default `true`).
    pub resource_attributes: bool,
}

impl MetricsInclude {
    /// Binary `C4h` defaults, used when [`ENV_ENABLE_TELEMETRY`] is on but the
    /// individual include vars are unset.
    #[must_use]
    pub const fn defaults() -> Self {
        MetricsInclude {
            session_id: true,
            version: false,
            account_uuid: true,
            entrypoint: false,
            resource_attributes: true,
        }
    }

    fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        let d = MetricsInclude::defaults();
        MetricsInclude {
            session_id: bool_env(
                get("OTEL_METRICS_INCLUDE_SESSION_ID").as_deref(),
                d.session_id,
            ),
            version: bool_env(get("OTEL_METRICS_INCLUDE_VERSION").as_deref(), d.version),
            account_uuid: bool_env(
                get("OTEL_METRICS_INCLUDE_ACCOUNT_UUID").as_deref(),
                d.account_uuid,
            ),
            entrypoint: bool_env(
                get("OTEL_METRICS_INCLUDE_ENTRYPOINT").as_deref(),
                d.entrypoint,
            ),
            resource_attributes: bool_env(
                get("OTEL_METRICS_INCLUDE_RESOURCE_ATTRIBUTES").as_deref(),
                d.resource_attributes,
            ),
        }
    }
}

/// `OTEL_LOG_*` opt-in flags controlling whether the `claude_code.events` log
/// signal captures potentially-sensitive bodies. All default `false` (privacy).
// One bool per CC `OTEL_LOG_*` env flag — a faithful mirror, not collapsible.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogIncludeFlags {
    /// `OTEL_LOG_USER_PROMPTS` — include user prompt text.
    pub user_prompts: bool,
    /// `OTEL_LOG_TOOL_DETAILS` — include tool call parameters.
    pub tool_details: bool,
    /// `OTEL_LOG_TOOL_CONTENT` — include tool result content.
    pub tool_content: bool,
    /// `OTEL_LOG_ASSISTANT_RESPONSES` — include assistant response text.
    pub assistant_responses: bool,
    /// `OTEL_LOG_RAW_API_BODIES` — include raw API request/response bodies.
    pub raw_api_bodies: bool,
}

impl LogIncludeFlags {
    fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        LogIncludeFlags {
            user_prompts: bool_env(get("OTEL_LOG_USER_PROMPTS").as_deref(), false),
            tool_details: bool_env(get("OTEL_LOG_TOOL_DETAILS").as_deref(), false),
            tool_content: bool_env(get("OTEL_LOG_TOOL_CONTENT").as_deref(), false),
            assistant_responses: bool_env(get("OTEL_LOG_ASSISTANT_RESPONSES").as_deref(), false),
            raw_api_bodies: bool_env(get("OTEL_LOG_RAW_API_BODIES").as_deref(), false),
        }
    }
}

/// Gate-family timeouts (the rebranded `LINGXI_OTEL_*` vars) plus the diag flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateTimeouts {
    /// Force-flush deadline in ms (default [`DEFAULT_FLUSH_TIMEOUT_MS`]).
    pub flush_timeout_ms: i64,
    /// Shutdown deadline in ms (default [`DEFAULT_SHUTDOWN_TIMEOUT_MS`]).
    pub shutdown_timeout_ms: i64,
    /// `otelHeadersHelper` debounce window in ms (default
    /// [`DEFAULT_HEADERS_HELPER_DEBOUNCE_MS`]).
    pub headers_helper_debounce_ms: i64,
    /// Route SDK diagnostics to stderr (default `false`).
    pub diag_stderr: bool,
}

impl GateTimeouts {
    fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        GateTimeouts {
            flush_timeout_ms: int_env(
                get(ENV_FLUSH_TIMEOUT_MS).as_deref(),
                DEFAULT_FLUSH_TIMEOUT_MS,
            ),
            shutdown_timeout_ms: int_env(
                get(ENV_SHUTDOWN_TIMEOUT_MS).as_deref(),
                DEFAULT_SHUTDOWN_TIMEOUT_MS,
            ),
            headers_helper_debounce_ms: int_env(
                get(ENV_HEADERS_HELPER_DEBOUNCE_MS).as_deref(),
                DEFAULT_HEADERS_HELPER_DEBOUNCE_MS,
            ),
            diag_stderr: bool_env(get(ENV_DIAG_STDERR).as_deref(), false),
        }
    }
}

/// Resolved OTLP exporter transport config for one signal (the "exporter
/// scaffolding"). Endpoint/headers/protocol resolve per-signal-first, then the
/// generic `OTEL_EXPORTER_OTLP_*` fallback, matching the binary precedence
/// `process.env.OTEL_EXPORTER_OTLP_<SIGNAL>_X ?? process.env.OTEL_EXPORTER_OTLP_X`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpExporterConfig {
    /// The signal this exporter serves.
    pub signal: Signal,
    /// Selected exporter kind (`otlp` unless overridden).
    pub kind: ExporterKind,
    /// Resolved OTLP endpoint (per-signal ?? generic), if any.
    pub endpoint: Option<String>,
    /// Resolved transport protocol (per-signal ?? generic ?? spec default).
    pub protocol: OtlpProtocol,
    /// Resolved static headers (per-signal ?? generic), parsed to a map.
    pub headers: BTreeMap<String, String>,
    /// Client CA / mTLS material.
    pub certificate: Option<String>,
    /// Client mTLS private-key path.
    pub client_key: Option<String>,
    /// Client mTLS certificate path.
    pub client_certificate: Option<String>,
    /// Compression selection (`gzip` / `none`), verbatim.
    pub compression: Option<String>,
    /// Request timeout in ms, if set.
    pub timeout_ms: Option<i64>,
    /// Insecure-transport toggle.
    pub insecure: bool,
    /// Reader/exporter export interval in ms (metrics/logs; `None` for traces).
    pub export_interval_ms: Option<i64>,
}

impl OtlpExporterConfig {
    fn from_lookup_with_source(signal: Signal, get: &impl Fn(&str) -> Option<ConfigValue>) -> Self {
        let infix = signal.otlp_infix();
        // Per-signal-first, then generic fallback (binary `?? / ||` precedence).
        //
        // 2.1.217 managed-settings nuance: when the GENERIC endpoint is owned by
        // managed policy, lower-scope per-signal endpoint overrides must not
        // redirect one signal away from the managed collector. Preserve standard
        // per-signal precedence for all non-managed cases, and for same-source
        // managed signal-specific overrides.
        let per_then_generic = |suffix: &str, generic: &str| -> Option<ConfigValue> {
            let per = get(&format!("OTEL_EXPORTER_OTLP_{infix}_{suffix}"));
            let generic = get(generic);
            match (per, generic) {
                (Some(per), Some(generic))
                    if suffix == "ENDPOINT"
                        && generic.source == ConfigValueSource::Managed
                        && per.source != ConfigValueSource::Managed =>
                {
                    Some(generic)
                }
                (Some(per), _) => Some(per),
                (None, generic) => generic,
            }
        };

        let kind = ExporterKind::parse(
            get(match signal {
                Signal::Metrics => ENV_METRICS_EXPORTER,
                Signal::Logs => ENV_LOGS_EXPORTER,
                Signal::Traces => ENV_TRACES_EXPORTER,
            })
            .map(|v| v.value)
            .as_deref(),
        );

        let protocol = OtlpProtocol::parse(
            per_then_generic("PROTOCOL", ENV_OTLP_PROTOCOL)
                .map(|v| v.value)
                .as_deref(),
        );

        let headers = parse_otlp_headers(
            per_then_generic("HEADERS", ENV_OTLP_HEADERS)
                .map(|v| v.value)
                .as_deref(),
        );

        let export_interval_ms = match signal {
            Signal::Metrics => Some(int_env(
                get(ENV_METRIC_EXPORT_INTERVAL)
                    .as_ref()
                    .map(|v| v.value.as_str()),
                DEFAULT_METRIC_EXPORT_INTERVAL_MS,
            )),
            Signal::Logs => Some(int_env(
                get(ENV_LOGS_EXPORT_INTERVAL)
                    .as_ref()
                    .map(|v| v.value.as_str()),
                DEFAULT_LOGS_EXPORT_INTERVAL_MS,
            )),
            Signal::Traces => None,
        };

        OtlpExporterConfig {
            signal,
            kind,
            endpoint: per_then_generic("ENDPOINT", ENV_OTLP_ENDPOINT).map(|v| v.value),
            protocol,
            headers,
            certificate: get(ENV_OTLP_CERTIFICATE).map(|v| v.value),
            client_key: get(ENV_OTLP_CLIENT_KEY).map(|v| v.value),
            client_certificate: get(ENV_OTLP_CLIENT_CERTIFICATE).map(|v| v.value),
            compression: get(ENV_OTLP_COMPRESSION).map(|v| v.value),
            timeout_ms: get(ENV_OTLP_TIMEOUT)
                .as_ref()
                .and_then(|v| parse_int_js(&v.value)),
            insecure: bool_env(
                get(ENV_OTLP_INSECURE).as_ref().map(|v| v.value.as_str()),
                false,
            ),
            export_interval_ms,
        }
    }
}

/// Parse an `OTEL_EXPORTER_OTLP_HEADERS`-style comma list of `k=v` pairs,
/// byte-faithful to the binary `s7u()`:
/// `for(let r of t.split(",")){let[n,...o]=r.split("=");if(n&&o.length>0)
/// e[n.trim()]=o.join("=").trim()}`. Values may contain `=`; empty keys and
/// value-less entries are dropped.
#[must_use]
pub fn parse_otlp_headers(raw: Option<&str>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(raw) = raw else {
        return out;
    };
    for part in raw.split(',') {
        let mut it = part.splitn(2, '=');
        let key = it.next().unwrap_or("");
        // `o.length>0` ⇒ there must be an `=` producing a value segment.
        let Some(value) = it.next() else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        out.insert(key.trim().to_string(), value.trim().to_string());
    }
    out
}

/// The full parsed OTEL monitoring config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtelConfig {
    /// Master gate ([`ENV_ENABLE_TELEMETRY`] truthy).
    pub enabled: bool,
    /// Additional explicit opt-in required for trace export.
    pub enhanced_telemetry: bool,
    /// Metrics exporter transport config.
    pub metrics: OtlpExporterConfig,
    /// Logs exporter transport config.
    pub logs: OtlpExporterConfig,
    /// Traces exporter transport config.
    pub traces: OtlpExporterConfig,
    /// `OTEL_METRICS_INCLUDE_*` attribute toggles.
    pub metrics_include: MetricsInclude,
    /// `OTEL_LOG_*` body-capture opt-ins.
    pub log_include: LogIncludeFlags,
    /// Gate-family timeouts + diag flag.
    pub timeouts: GateTimeouts,
    /// `OTEL_SERVICE_NAME` (default [`DEFAULT_SERVICE_NAME`]).
    pub service_name: String,
    /// Raw `OTEL_RESOURCE_ATTRIBUTES` (`k=v,k=v`), if set.
    pub resource_attributes: Option<String>,
    /// `OTEL_TRACES_SAMPLER`, if set.
    pub traces_sampler: Option<String>,
    /// `OTEL_TRACES_SAMPLER_ARG`, if set.
    pub traces_sampler_arg: Option<String>,
    /// Effective per-record content length cap in characters (binary `mrg()`);
    /// see [`compute_content_max_length`]. Applied by the recording layer to the
    /// large body attributes on the log signal.
    pub content_max_length: i64,
}

impl OtelConfig {
    /// Parse config from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Parse config from the process environment with a higher-priority managed
    /// override map. Managed keys preserve their provenance so generic managed
    /// OTLP endpoint values can override lower-scope per-signal endpoint vars
    /// without changing normal environment-only precedence.
    #[must_use]
    pub fn from_env_with_managed(managed: &std::collections::BTreeMap<String, String>) -> Self {
        Self::from_lookup_with_source(|k| {
            if let Some(value) = managed.get(k) {
                return Some(ConfigValue {
                    value: value.clone(),
                    source: ConfigValueSource::Managed,
                });
            }
            std::env::var(k).ok().map(|value| ConfigValue {
                value,
                source: ConfigValueSource::Runtime,
            })
        })
    }

    /// Parse config from an arbitrary lookup closure — the testable core (no
    /// global process-env access, so parse tests never race).
    #[must_use]
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self::from_lookup_with_source(|k| {
            get(k).map(|value| ConfigValue {
                value,
                source: ConfigValueSource::Runtime,
            })
        })
    }

    /// Parse config from a lookup that also reports value provenance.
    #[must_use]
    pub fn from_lookup_with_source(get: impl Fn(&str) -> Option<ConfigValue>) -> Self {
        OtelConfig {
            enabled: bool_env(
                get(ENV_ENABLE_TELEMETRY).as_ref().map(|v| v.value.as_str()),
                false,
            ),
            enhanced_telemetry: bool_env(
                get(ENV_ENHANCED_TELEMETRY_BETA)
                    .as_ref()
                    .map(|v| v.value.as_str()),
                false,
            ),
            metrics: OtlpExporterConfig::from_lookup_with_source(Signal::Metrics, &get),
            logs: OtlpExporterConfig::from_lookup_with_source(Signal::Logs, &get),
            traces: OtlpExporterConfig::from_lookup_with_source(Signal::Traces, &get),
            metrics_include: MetricsInclude::from_lookup(&|k| get(k).map(|v| v.value)),
            log_include: LogIncludeFlags::from_lookup(&|k| get(k).map(|v| v.value)),
            timeouts: GateTimeouts::from_lookup(&|k| get(k).map(|v| v.value)),
            service_name: get(ENV_SERVICE_NAME)
                .map(|v| v.value)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string()),
            resource_attributes: get(ENV_RESOURCE_ATTRIBUTES).map(|v| v.value),
            traces_sampler: get(ENV_TRACES_SAMPLER).map(|v| v.value),
            traces_sampler_arg: get(ENV_TRACES_SAMPLER_ARG).map(|v| v.value),
            content_max_length: compute_content_max_length(&|k| get(k).map(|v| v.value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn truthy_matches_binary_ct() {
        for v in ["1", "true", "TRUE", "  Yes ", "on", "ON"] {
            assert!(env_truthy(v), "{v:?} should be truthy");
        }
        for v in ["0", "false", "no", "off", "", "2", "enabled"] {
            assert!(!env_truthy(v), "{v:?} should be falsy");
        }
    }

    #[test]
    fn bool_env_default_and_override() {
        assert!(bool_env(None, true));
        assert!(!bool_env(None, false));
        // Explicit "0" overrides a true default (hNr returns ct(r)).
        assert!(!bool_env(Some("0"), true));
        assert!(bool_env(Some("yes"), false));
    }

    #[test]
    fn int_env_parseint_semantics() {
        assert_eq!(int_env(None, 5000), 5000);
        assert_eq!(int_env(Some("2500"), 5000), 2500);
        assert_eq!(int_env(Some("5000ms"), 9), 5000); // parseInt ignores trailing
        assert_eq!(int_env(Some("abc"), 9), 9); // NaN ⇒ default
        assert_eq!(int_env(Some("  42"), 0), 42);
        assert_eq!(int_env(Some("-7"), 0), -7);
        assert_eq!(parse_int_js("+8x"), Some(8));
        assert_eq!(parse_int_js("x8"), None);
    }

    #[test]
    fn gate_off_by_default_is_byte_noop() {
        let cfg = OtelConfig::from_lookup(|_| None);
        assert!(!cfg.enabled, "master gate must be OFF when unset");
        // Defaults still resolve so downstream never panics on absent env.
        assert_eq!(cfg.timeouts.flush_timeout_ms, DEFAULT_FLUSH_TIMEOUT_MS);
        assert_eq!(
            cfg.timeouts.shutdown_timeout_ms,
            DEFAULT_SHUTDOWN_TIMEOUT_MS
        );
        assert_eq!(
            cfg.timeouts.headers_helper_debounce_ms,
            DEFAULT_HEADERS_HELPER_DEBOUNCE_MS
        );
        assert_eq!(cfg.service_name, DEFAULT_SERVICE_NAME);
        assert_eq!(cfg.metrics_include, MetricsInclude::defaults());
        assert_eq!(cfg.log_include, LogIncludeFlags::default());
        assert_eq!(cfg.metrics.kind, ExporterKind::Otlp);
    }

    #[test]
    fn gate_on_via_lingxi_var() {
        let cfg = OtelConfig::from_lookup(lookup(&[(ENV_ENABLE_TELEMETRY, "1")]));
        assert!(cfg.enabled);
    }

    #[test]
    fn enhanced_telemetry_requires_explicit_truthy_beta_gate() {
        let unset = OtelConfig::from_lookup(lookup(&[(ENV_ENABLE_TELEMETRY, "1")]));
        assert!(!unset.enhanced_telemetry);

        let disabled = OtelConfig::from_lookup(lookup(&[
            (ENV_ENABLE_TELEMETRY, "1"),
            (ENV_ENHANCED_TELEMETRY_BETA, "0"),
        ]));
        assert!(!disabled.enhanced_telemetry);

        let enabled = OtelConfig::from_lookup(lookup(&[
            (ENV_ENABLE_TELEMETRY, "1"),
            (ENV_ENHANCED_TELEMETRY_BETA, " yes "),
        ]));
        assert!(enabled.enhanced_telemetry);
    }

    #[test]
    fn per_signal_protocol_overrides_generic() {
        let cfg = OtelConfig::from_lookup(lookup(&[
            (ENV_OTLP_PROTOCOL, "grpc"),
            ("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "http/json"),
        ]));
        // Metrics uses its per-signal override.
        assert_eq!(cfg.metrics.protocol, OtlpProtocol::HttpJson);
        // Logs falls back to the generic.
        assert_eq!(cfg.logs.protocol, OtlpProtocol::Grpc);
    }

    #[test]
    fn per_signal_endpoint_and_headers_precedence() {
        let cfg = OtelConfig::from_lookup(lookup(&[
            (ENV_OTLP_ENDPOINT, "http://generic:4318"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://traces:4317"),
            (ENV_OTLP_HEADERS, "authorization=Bearer g,x-tenant=acme"),
            (
                "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
                "authorization=Bearer logs",
            ),
        ]));
        assert_eq!(cfg.traces.endpoint.as_deref(), Some("http://traces:4317"));
        assert_eq!(cfg.metrics.endpoint.as_deref(), Some("http://generic:4318"));
        // Logs headers override the generic entirely.
        assert_eq!(
            cfg.logs.headers.get("authorization").map(String::as_str),
            Some("Bearer logs")
        );
        // Metrics inherits the generic headers.
        assert_eq!(
            cfg.metrics.headers.get("x-tenant").map(String::as_str),
            Some("acme")
        );
    }

    #[test]
    fn managed_generic_endpoint_overrides_lower_signal_specific_endpoint() {
        let cfg = OtelConfig::from_lookup_with_source(|key| match key {
            ENV_OTLP_ENDPOINT => Some(ConfigValue {
                value: "http://managed:4318".to_string(),
                source: ConfigValueSource::Managed,
            }),
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => Some(ConfigValue {
                value: "http://runtime-traces:4317".to_string(),
                source: ConfigValueSource::Runtime,
            }),
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT" => Some(ConfigValue {
                value: "http://runtime-logs:4317".to_string(),
                source: ConfigValueSource::Runtime,
            }),
            _ => None,
        });
        assert_eq!(cfg.metrics.endpoint.as_deref(), Some("http://managed:4318"));
        assert_eq!(cfg.logs.endpoint.as_deref(), Some("http://managed:4318"));
        assert_eq!(cfg.traces.endpoint.as_deref(), Some("http://managed:4318"));
    }

    #[test]
    fn managed_signal_specific_endpoint_still_overrides_managed_generic() {
        let cfg = OtelConfig::from_lookup_with_source(|key| match key {
            ENV_OTLP_ENDPOINT => Some(ConfigValue {
                value: "http://managed:4318".to_string(),
                source: ConfigValueSource::Managed,
            }),
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => Some(ConfigValue {
                value: "http://managed-traces:4317".to_string(),
                source: ConfigValueSource::Managed,
            }),
            _ => None,
        });
        assert_eq!(cfg.metrics.endpoint.as_deref(), Some("http://managed:4318"));
        assert_eq!(
            cfg.traces.endpoint.as_deref(),
            Some("http://managed-traces:4317")
        );
    }

    #[test]
    fn header_parse_keeps_equals_in_value() {
        let h = parse_otlp_headers(Some("k=a=b=c,,empty=,=noykey,ok=v"));
        assert_eq!(h.get("k").map(String::as_str), Some("a=b=c"));
        assert_eq!(h.get("empty").map(String::as_str), Some(""));
        assert_eq!(h.get("ok").map(String::as_str), Some("v"));
        // Empty key dropped; value-less `,,` segments dropped.
        assert!(!h.contains_key(""));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn metrics_include_defaults_match_binary_c4h() {
        let d = MetricsInclude::defaults();
        assert!(d.session_id);
        assert!(!d.version);
        assert!(d.account_uuid);
        assert!(!d.entrypoint);
        assert!(d.resource_attributes);
    }

    #[test]
    fn exporter_kind_parse() {
        assert_eq!(ExporterKind::parse(None), ExporterKind::Otlp);
        assert_eq!(ExporterKind::parse(Some("")), ExporterKind::Otlp);
        assert_eq!(ExporterKind::parse(Some("Console")), ExporterKind::Console);
        assert_eq!(ExporterKind::parse(Some("none")), ExporterKind::None);
        assert_eq!(
            ExporterKind::parse(Some("prometheus")),
            ExporterKind::Prometheus
        );
        assert_eq!(
            ExporterKind::parse(Some("weird")),
            ExporterKind::Other("weird".to_string())
        );
    }

    #[test]
    fn content_max_length_defaults_to_binary_frg() {
        let cfg = OtelConfig::from_lookup(|_| None);
        assert_eq!(cfg.content_max_length, DEFAULT_CONTENT_MAX_LENGTH);
        assert_eq!(cfg.content_max_length, 61440);
    }

    #[test]
    fn content_max_length_gate_var_overrides_default() {
        let cfg = OtelConfig::from_lookup(lookup(&[(ENV_CONTENT_MAX_LENGTH, "1000")]));
        assert_eq!(cfg.content_max_length, 1000);
    }

    #[test]
    fn content_max_length_is_math_min_over_limits() {
        // The smallest of the four wins (binary `Math.min`).
        let cfg = OtelConfig::from_lookup(lookup(&[
            (ENV_CONTENT_MAX_LENGTH, "61440"),
            (ENV_ATTRIBUTE_VALUE_LENGTH_LIMIT, "8192"),
            (ENV_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT, "500"),
            (ENV_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT, "9000"),
        ]));
        assert_eq!(cfg.content_max_length, 500);
    }

    #[test]
    fn content_max_length_unset_limits_are_infinity() {
        // The three OTEL limits unset ⇒ Infinity ⇒ the gate var (or its default)
        // governs, never the limits.
        let cfg = OtelConfig::from_lookup(lookup(&[(ENV_CONTENT_MAX_LENGTH, "20000")]));
        assert_eq!(cfg.content_max_length, 20000);
    }

    #[test]
    fn content_max_length_nan_override_clamps_to_zero() {
        // JS `Math.min(NaN, …)` === NaN; feeding NaN into `String.slice` yields an
        // empty cut, which our i64 budget models as 0.
        let cfg = OtelConfig::from_lookup(lookup(&[(ENV_CONTENT_MAX_LENGTH, "not-a-number")]));
        assert_eq!(cfg.content_max_length, 0);
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "asserting exact JS Number coercion of integral/decimal literals"
    )]
    fn js_number_matches_js_coercion() {
        assert_eq!(js_number("1000"), 1000.0);
        assert_eq!(js_number("  42  "), 42.0);
        assert_eq!(js_number(""), 0.0); // Number("") === 0
        assert_eq!(js_number("   "), 0.0);
        assert!(js_number("abc").is_nan());
        assert_eq!(js_number("2.5"), 2.5);
    }

    #[test]
    fn intervals_default_to_binary_constants() {
        let cfg = OtelConfig::from_lookup(|_| None);
        assert_eq!(
            cfg.metrics.export_interval_ms,
            Some(DEFAULT_METRIC_EXPORT_INTERVAL_MS)
        );
        assert_eq!(
            cfg.logs.export_interval_ms,
            Some(DEFAULT_LOGS_EXPORT_INTERVAL_MS)
        );
        assert_eq!(cfg.traces.export_interval_ms, None);
    }
}
