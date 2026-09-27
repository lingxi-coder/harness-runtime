//! OpenTelemetry enterprise **Monitoring** subsystem (CC 2.1.207 parity,
//! finding H-BIN-06).
//!
//! CC ships a first-party, default-off OpenTelemetry monitoring stack distinct
//! from its `tengu_*` Statsig product telemetry: when
//! `CLAUDE_CODE_ENABLE_TELEMETRY` is truthy it stands up metric/log/trace
//! exporters (OTLP/console/prometheus) driven entirely by the standard `OTEL_*`
//! env surface plus a small `CLAUDE_CODE_OTEL_*` gate family and an
//! `otelHeadersHelper` settings hook for dynamic export headers.
//! Detailed traces additionally require the explicit
//! `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA` opt-in; metrics and logs do not.
//!
//! ## What this module ports (the load-bearing parity)
//!
//! - [`config`]: the **full env surface** — the master gate, per-signal
//!   exporter selection, OTLP endpoint/headers/protocol (incl. per-signal
//!   override precedence) + mTLS material, export intervals, the
//!   `OTEL_METRICS_INCLUDE_*` attribute toggles, the `OTEL_LOG_*` opt-ins, and
//!   the gate-family timeouts — with byte-faithful boolean/numeric parse
//!   semantics and defaults lifted from the binary.
//! - [`headers_helper`]: the `otelHeadersHelper` validation + debounce + failure
//!   caching state machine, with the byte-exact CC error strings.
//! - [`metrics`]: the `claude_code.*` instrument-name schema + meter identity.
//! - [`logs`]: the `claude_code.events` log signal names + `OTEL_LOG_*` gate
//!   helpers (the orchestrator's opt-in `assistant_response` log line reads
//!   [`logs::assistant_responses_enabled`]).
//! - [`runtime`]: the default-off, fail-open OTLP / Prometheus provider
//!   lifecycle used by the process entrypoints, including flush + shutdown,
//!   HTTP/gRPC transport configuration, TLS/header handling, and a small global
//!   recording surface for core record sites.
//! - [`record`]: the **recording foundation** (finding H-09) — the
//!   [`record::MetricRecorder`]/[`record::LogRecorder`] traits every record site
//!   depends on, a byte-noop default ([`record::NoopRecorder`]) for the gate-off
//!   path, and a config-gated in-memory/console recorder
//!   ([`record::InMemoryRecorder`]) that accumulates counters/histograms/logs and
//!   honors the `LINGXI_OTEL_CONTENT_MAX_LENGTH` content cap.
//!
//! ## Rebrand policy
//!
//! `CLAUDE_CODE_ENABLE_TELEMETRY` → `LINGXI_ENABLE_TELEMETRY` and
//! `CLAUDE_CODE_OTEL_*` → `LINGXI_OTEL_*` (matching the existing
//! `LINGXI_OTEL_DIAG_STDERR` sibling in the hook-env denylist). Every standard
//! `OTEL_*` spec var and every `claude_code.*` / `com.anthropic.claude_code`
//! schema identifier is kept verbatim (`OTel` wire contract; collectors key on
//! them). See [`config`] for the full rationale.
//!
//! ## Remainder (see the coverage list in [`record`])
//!
//! The provider/runtime layer exists ([`runtime`]) and the `claude_code.*`
//! record sites are wired — via the analytics-bus bridge
//! (`runtime::mirror_analytics_event`) plus the direct
//! `record_lines_of_code_change` / `record_git_operation_counters` /
//! `record_tool_permission_decision` / `emit_hook_lifecycle` /
//! `emit_user_prompt_log` helpers. `active_time.total` is intentionally
//! registered-but-unrecorded — the 2.1.220 binary defines the instrument but
//! has no `add()` call site.

mod code_language;
pub mod config;
mod git_ops;
pub mod headers_helper;
pub mod logs;
pub mod metrics;
pub mod record;
/// Process-level OpenTelemetry provider lifecycle and narrow live record sites.
pub mod runtime;

pub use config::{
    bool_env, compute_content_max_length, env_truthy, int_env, js_number, ConfigValue,
    ConfigValueSource, ExporterKind, GateTimeouts, LogIncludeFlags, MetricsInclude, OtelConfig,
    OtlpExporterConfig, OtlpProtocol, Signal, DEFAULT_CONTENT_MAX_LENGTH, ENV_CONTENT_MAX_LENGTH,
    ENV_ENABLE_TELEMETRY, ENV_ENHANCED_TELEMETRY_BETA,
};
pub use headers_helper::{validate_helper_output, ExecOutcome, HeadersHelperState, ResolveOutcome};
pub use record::{
    recorder_from_config, truncate_content, AttrValue, Attributes, CounterSeries, HistogramSeries,
    InMemoryRecorder, LogRecord, LogRecorder, MetricRecorder, NoopRecorder, Recorder,
    TruncatedContent,
};
pub use runtime::{
    capture_current_trace_context, emit_assistant_response_log, emit_hook_lifecycle,
    emit_named_log_event, emit_user_prompt_log, install_process, install_process_with_config,
    prometheus_text, record_counter, record_git_operation_counters, record_histogram,
    record_lines_of_code_change, record_tool_permission_decision, with_trace_context,
    with_trace_context_future, with_turn_span, SerializedTraceContext, TelemetryGuard,
};

/// Whether the OpenTelemetry monitoring stack is enabled for this process
/// (binary `o7u()`: `ct(process.env.CLAUDE_CODE_ENABLE_TELEMETRY)`, rebranded
/// to [`config::ENV_ENABLE_TELEMETRY`]). When `false` the whole subsystem is a
/// byte-noop.
#[must_use]
pub fn telemetry_enabled() -> bool {
    match std::env::var(config::ENV_ENABLE_TELEMETRY) {
        Ok(v) => env_truthy(&v),
        Err(_) => false,
    }
}

/// Resolve the full monitoring config from the process env, returning `None`
/// when the master gate is off (so callers can early-out without constructing
/// any exporter state — the default byte-noop path).
///
/// This is the intended boot seam: a future egress-wiring commit calls this at
/// startup and, on `Some`, constructs the OTLP exporters/readers described by
/// the returned [`config::OtelConfig`].
#[must_use]
pub fn init_from_env() -> Option<OtelConfig> {
    let cfg = OtelConfig::from_env();
    if cfg.enabled {
        Some(cfg)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_returns_none_when_gate_off() {
        // Gate parsed from an explicit lookup (never touches process env).
        let off = OtelConfig::from_lookup(|_| None);
        assert!(!off.enabled);
        let on = OtelConfig::from_lookup(|k| {
            (k == config::ENV_ENABLE_TELEMETRY).then(|| "true".to_string())
        });
        assert!(on.enabled);
    }
}
