//! Process-level LingXi diagnostics: default-off metrics, logs and traces.
//!
//! `LINGXI_ENABLE_TELEMETRY` enables the configured standard `OTEL_*` exporters.
//! Privacy vetoes (`DO_NOT_TRACK`, `DISABLE_TELEMETRY`, nonessential traffic)
//! override enablement. Hosts call [`disable_process`] before shutting down an
//! opted-out session; queued exports are then discarded. In-flight requests
//! retain their bounded transport timeout.
//!
//! All outgoing signals use a diagnostic field allowlist. Prompt/response text,
//! tool parameters/results, paths and credentials are never exported, including
//! when legacy body-capture flags are present. Metrics exclude identity fields.
//! Existing `claude_code.*` instrument names remain stable for consumers, while
//! the default service name is `lingxi` and operation spans use `lingxi.*`.
//!
//! Internal analytics sinks are separate from this outbound diagnostic policy.
//! Their callers remain responsible for any independent network sink policy.

mod code_language;
pub mod config;
mod export_policy;
mod git_ops;
pub mod headers_helper;
pub mod logs;
pub mod metrics;
mod policy;
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
    bind_current_trace_context, capture_current_trace_context, disable_process,
    disable_process_async, emit_assistant_response_log, emit_hook_lifecycle, emit_named_log_event,
    emit_user_prompt_log, install_process, install_process_with_config,
    install_process_with_config_async, prometheus_text, record_counter,
    record_git_operation_counters, record_histogram, record_lines_of_code_change,
    record_tool_permission_decision, start_operation_span, with_operation_span, with_trace_context,
    with_trace_context_future, with_turn_span, OperationSpan, SerializedTraceContext,
    TelemetryGuard,
};

/// Whether the OpenTelemetry monitoring stack is enabled for this process
/// (binary `o7u()`: `ct(process.env.CLAUDE_CODE_ENABLE_TELEMETRY)`, rebranded
/// to [`config::ENV_ENABLE_TELEMETRY`]). When `false` the whole subsystem is a
/// byte-noop.
#[must_use]
pub fn telemetry_enabled() -> bool {
    if policy::privacy_disabled() {
        return false;
    }
    match std::env::var(config::ENV_ENABLE_TELEMETRY) {
        Ok(v) => env_truthy(&v),
        Err(_) => false,
    }
}

/// Resolve the full monitoring config from the process env, returning `None`
/// when the master gate is off (so callers can early-out without constructing
/// any exporter state — the default byte-noop path).
///
/// Process hosts use this at startup before constructing exporter state.
#[must_use]
pub fn init_from_env() -> Option<OtelConfig> {
    let cfg = OtelConfig::from_env();
    if cfg.enabled && !policy::privacy_disabled() {
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
