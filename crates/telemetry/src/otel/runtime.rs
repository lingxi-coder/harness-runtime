use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{
    FutureExt as _, Span as _, TraceContextExt, Tracer as _, TracerProvider as _,
};
use opentelemetry::Context;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{
    Compression, Protocol as OtlpWireProtocol, WithExportConfig, WithHttpConfig,
};
use opentelemetry_prometheus::exporter as prometheus_exporter;
use opentelemetry_sdk::logs::{
    BatchConfigBuilder as LogBatchConfigBuilder, BatchLogProcessor, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder as TraceBatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider,
};
use opentelemetry_sdk::Resource;
use prometheus::{Encoder, Registry, TextEncoder};

use super::code_language::language_for_path;
use super::config::{ExporterKind, OtelConfig, OtlpExporterConfig, OtlpProtocol};
use super::export_policy::GatedExporter;
use super::git_ops::git_counter_hits;
use super::metrics;
use super::policy::{self, filter_attributes, ExportPolicy};
use super::record::{truncate_content, AttrValue, Attributes};
use crate::sink::{AnalyticsValue, LogEventMetadata};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

static ACTIVE_RUNTIME: OnceLock<RwLock<Option<Arc<OtelRuntime>>>> = OnceLock::new();

fn runtime_slot() -> &'static RwLock<Option<Arc<OtelRuntime>>> {
    ACTIVE_RUNTIME.get_or_init(|| RwLock::new(None))
}

#[derive(Debug, Clone)]
/// Drop guard for a process-level OTEL runtime installed via [`install_process`].
pub struct TelemetryGuard {
    runtime: Option<Arc<OtelRuntime>>,
}

impl TelemetryGuard {
    /// Construct an inert guard with no installed runtime.
    #[must_use]
    pub fn disabled() -> Self {
        TelemetryGuard { runtime: None }
    }

    /// Flush without blocking an async host's executor.
    pub async fn force_flush_async(&self) -> Result<(), String> {
        let Some(runtime) = self.runtime.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || force_flush_runtime(&runtime))
            .await
            .map_err(|_| "telemetry flush worker failed".to_string())?
    }

    /// Explicit async shutdown for hosts that can await lifecycle completion.
    /// Drop still provides a bounded synchronous fallback for early returns.
    pub async fn shutdown_async(mut self) -> Result<(), String> {
        let Some(runtime) = self.runtime.take() else {
            return Ok(());
        };
        detach_runtime(&runtime);
        tokio::task::spawn_blocking(move || runtime.shutdown())
            .await
            .map_err(|_| "telemetry shutdown worker failed".to_string())
    }

    /// Flush accepted diagnostics for deterministic shutdown or validation.
    /// Disabled/revoked providers remain silent, including queued records.
    pub fn force_flush(&self) -> Result<(), String> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        force_flush_runtime(runtime)
    }
}

fn force_flush_runtime(runtime: &OtelRuntime) -> Result<(), String> {
    if !runtime.policy.allowed() {
        return Ok(());
    }
    let timeout = duration_ms(runtime.config.timeouts.flush_timeout_ms);
    if let Some(metrics) = &runtime.metrics {
        let provider = metrics.provider.clone();
        run_with_timeout("metrics.force_flush", timeout, move || {
            provider.force_flush().map_err(|err| err.to_string())
        })?;
    }
    if let Some(logs) = &runtime.logs {
        let provider = logs.clone();
        run_with_timeout("logs.force_flush", timeout, move || {
            provider.force_flush().map_err(|err| err.to_string())
        })?;
    }
    if let Some(traces) = &runtime.traces {
        let provider = traces.clone();
        run_with_timeout("traces.force_flush", timeout, move || {
            provider.force_flush().map_err(|err| err.to_string())
        })?;
    }
    Ok(())
}

fn detach_runtime(runtime: &Arc<OtelRuntime>) {
    if let Ok(mut slot) = runtime_slot().write() {
        if slot
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, runtime))
        {
            slot.take();
        }
    }
}

/// Revoke reporting before draining providers. Pending batches are discarded.
/// Already in-flight requests cannot be retracted; their normal timeouts apply.
pub fn disable_process() {
    if let Some(runtime) = revoke_active_runtime() {
        runtime.shutdown();
    }
}

/// Revoke immediately, then await bounded teardown away from the async host.
/// A caller may acknowledge opt-out after this returns; queued exports stay
/// revoked even if the waiting future is cancelled during teardown.
pub async fn disable_process_async() {
    if let Some(runtime) = revoke_active_runtime() {
        if tokio::task::spawn_blocking(move || runtime.shutdown())
            .await
            .is_err()
        {
            emit_diag(true, "OpenTelemetry shutdown worker failed", false);
        }
    }
}

fn revoke_active_runtime() -> Option<Arc<OtelRuntime>> {
    let mut slot = runtime_slot().write().ok()?;
    let runtime = slot.take()?;
    runtime.policy.revoke();
    Some(runtime)
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        detach_runtime(&runtime);
        runtime.shutdown();
    }
}

#[derive(Debug)]
struct OtelRuntime {
    config: OtelConfig,
    entrypoint: &'static str,
    started_at: Instant,
    resource: Resource,
    metrics: Option<MetricsRuntime>,
    logs: Option<SdkLoggerProvider>,
    traces: Option<SdkTracerProvider>,
    shutdown: AtomicBool,
    policy: Arc<ExportPolicy>,
    #[cfg(test)]
    debug: Mutex<DebugMirrorState>,
}

struct MetricsRuntime {
    provider: SdkMeterProvider,
    prometheus_registry: Option<Registry>,
    counters: Mutex<HashMap<String, opentelemetry::metrics::Counter<f64>>>,
    histograms: Mutex<HashMap<String, opentelemetry::metrics::Histogram<f64>>>,
}

impl std::fmt::Debug for MetricsRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsRuntime")
            .field("prometheus_registry", &self.prometheus_registry.is_some())
            .finish_non_exhaustive()
    }
}

impl MetricsRuntime {
    fn new(provider: SdkMeterProvider, prometheus_registry: Option<Registry>) -> Self {
        Self {
            provider,
            prometheus_registry,
            counters: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Default, Clone)]
struct DebugMirrorState {
    counters: Vec<DebugMetricSample>,
    histograms: Vec<DebugMetricSample>,
    logs: Vec<DebugLogSample>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
struct DebugMetricSample {
    instrument: String,
    value: f64,
    attributes: Attributes,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
struct DebugLogSample {
    event_name: String,
    attributes: Attributes,
}

#[derive(Debug, Clone)]
struct MetricUpdate {
    instrument: &'static str,
    value: f64,
    attributes: Attributes,
    kind: MetricUpdateKind,
}

/// W3C trace headers captured from the active OpenTelemetry parent context.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SerializedTraceContext {
    /// Canonical `traceparent` header value.
    pub traceparent: String,
    /// Optional `tracestate` header value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetricUpdateKind {
    Counter,
    Histogram,
}

impl OtelRuntime {
    fn new(
        config: OtelConfig,
        entrypoint: &'static str,
        record_session_metric: bool,
    ) -> Result<Self, String> {
        let policy = Arc::new(ExportPolicy::new(config.enabled));
        let resource = build_resource(&config);
        let metrics = build_metrics_runtime(&config, resource.clone(), &policy)?;
        let logs = build_logger_provider(&config, resource.clone(), &policy)?;
        let traces = build_tracer_provider(&config, resource.clone(), &policy)?;

        let runtime = OtelRuntime {
            config,
            entrypoint,
            started_at: Instant::now(),
            resource,
            metrics,
            logs,
            traces,
            shutdown: AtomicBool::new(false),
            policy,
            #[cfg(test)]
            debug: Mutex::new(DebugMirrorState::default()),
        };

        runtime.emit_span("lingxi.telemetry.start", runtime.process_attributes());
        runtime.emit_log_event(
            "telemetry_started",
            &attrs_from_pairs(&[
                ("entrypoint", AttrValue::from(entrypoint)),
                (
                    "metrics_exporter",
                    AttrValue::from(exporter_kind_name(&runtime.config.metrics.kind)),
                ),
                (
                    "logs_exporter",
                    AttrValue::from(exporter_kind_name(&runtime.config.logs.kind)),
                ),
                (
                    "traces_exporter",
                    AttrValue::from(exporter_kind_name(&runtime.config.traces.kind)),
                ),
            ]),
        );
        if record_session_metric {
            runtime.record_counter(metrics::SESSION_COUNT, 1.0, &Attributes::new());
        }
        Ok(runtime)
    }

    fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }

        let uptime_ms = self.started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
        let shutdown_attrs = attrs_from_pairs(&[
            ("entrypoint", AttrValue::from(self.entrypoint)),
            ("uptime_ms", AttrValue::from(uptime_ms)),
        ]);
        self.emit_log_event("telemetry_stopped", &shutdown_attrs);
        self.emit_span(
            "lingxi.telemetry.stop",
            self.process_attributes_with_extra([KeyValue::new("uptime_ms", uptime_ms)]),
        );

        let flush_timeout = duration_ms(self.config.timeouts.flush_timeout_ms);
        let shutdown_timeout = duration_ms(self.config.timeouts.shutdown_timeout_ms);

        if let Some(metrics) = &self.metrics {
            let provider = metrics.provider.clone();
            let _ = run_with_timeout("metrics.force_flush", flush_timeout, move || {
                provider.force_flush().map_err(|err| err.to_string())
            });
        }
        if let Some(logs) = &self.logs {
            let provider = logs.clone();
            let _ = run_with_timeout("logs.force_flush", flush_timeout, move || {
                provider.force_flush().map_err(|err| err.to_string())
            });
        }
        if let Some(traces) = &self.traces {
            let provider = traces.clone();
            let _ = run_with_timeout("traces.force_flush", flush_timeout, move || {
                provider.force_flush().map_err(|err| err.to_string())
            });
        }

        if let Some(metrics) = &self.metrics {
            let provider = metrics.provider.clone();
            let _ = run_with_timeout("metrics.shutdown", shutdown_timeout, move || {
                provider.shutdown().map_err(|err| err.to_string())
            });
        }
        if let Some(logs) = &self.logs {
            let provider = logs.clone();
            let _ = run_with_timeout("logs.shutdown", shutdown_timeout, move || {
                provider.shutdown().map_err(|err| err.to_string())
            });
        }
        if let Some(traces) = &self.traces {
            let provider = traces.clone();
            let _ = run_with_timeout("traces.shutdown", shutdown_timeout, move || {
                provider.shutdown().map_err(|err| err.to_string())
            });
        }
    }

    fn process_attributes(&self) -> Vec<KeyValue> {
        self.process_attributes_with_extra(std::iter::empty())
    }

    fn process_attributes_with_extra<I>(&self, extra: I) -> Vec<KeyValue>
    where
        I: IntoIterator<Item = KeyValue>,
    {
        let mut out = vec![KeyValue::new("entrypoint", self.entrypoint)];
        if self.config.metrics_include.version {
            out.push(KeyValue::new("version", env!("CARGO_PKG_VERSION")));
        }
        if self.config.metrics_include.resource_attributes {
            out.extend(resource_kvs(&self.resource));
        }
        out.extend(extra);
        out
    }

    fn record_counter(&self, instrument: &str, value: f64, attrs: &Attributes) {
        if !self.policy.allowed() || !value.is_finite() || value < 0.0 {
            return;
        }
        let Some(metrics) = &self.metrics else {
            return;
        };
        let key_values = self.metric_attributes(attrs);
        {
            let mut map = metrics
                .counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let counter = map.entry(instrument.to_string()).or_insert_with(|| {
                metrics
                    .provider
                    .meter(metrics::METER_NAME)
                    .f64_counter(instrument.to_string())
                    .build()
            });
            counter.add(value, &key_values);
        }
        #[cfg(test)]
        self.debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counters
            .push(DebugMetricSample {
                instrument: instrument.to_string(),
                value,
                attributes: attrs.clone(),
            });
    }

    fn record_histogram(&self, instrument: &str, value: f64, attrs: &Attributes) {
        if !self.policy.allowed() || !value.is_finite() {
            return;
        }
        let Some(metrics) = &self.metrics else {
            return;
        };
        let key_values = self.metric_attributes(attrs);
        {
            let mut map = metrics
                .histograms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let histogram = map.entry(instrument.to_string()).or_insert_with(|| {
                metrics
                    .provider
                    .meter(metrics::METER_NAME)
                    .f64_histogram(instrument.to_string())
                    .build()
            });
            histogram.record(value, &key_values);
        }
        #[cfg(test)]
        self.debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .histograms
            .push(DebugMetricSample {
                instrument: instrument.to_string(),
                value,
                attributes: attrs.clone(),
            });
    }

    fn metric_attributes(&self, attrs: &Attributes) -> Vec<KeyValue> {
        let mut out = Vec::new();
        if self.config.metrics_include.entrypoint {
            out.push(KeyValue::new("entrypoint", self.entrypoint));
        }
        if self.config.metrics_include.version {
            out.push(KeyValue::new("version", env!("CARGO_PKG_VERSION")));
        }
        if self.config.metrics_include.resource_attributes {
            out.extend(resource_kvs(&self.resource));
        }
        out.extend(attrs_to_kvs(&filter_attributes(attrs, true)));
        out
    }

    fn emit_log_event(&self, event_name: &'static str, attrs: &Attributes) {
        if !self.policy.allowed() {
            return;
        }
        let Some(provider) = &self.logs else {
            #[cfg(test)]
            self.debug
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .logs
                .push(DebugLogSample {
                    event_name: event_name.to_string(),
                    attributes: attrs.clone(),
                });
            return;
        };
        let logger = provider.logger(super::logs::EVENTS_SIGNAL);
        let mut record = logger.create_log_record();
        record.set_event_name(event_name);
        record.set_body(AnyValue::String(event_name.into()));
        record.add_attributes(log_attrs(&filter_attributes(attrs, false)));
        set_log_trace_context(&mut record);
        logger.emit(record);
        #[cfg(test)]
        self.debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .logs
            .push(DebugLogSample {
                event_name: event_name.to_string(),
                attributes: attrs.clone(),
            });
    }

    fn emit_dynamic_log_event(&self, body_name: &str, attrs: &Attributes) {
        if !self.policy.allowed() {
            return;
        }
        let body_name = if policy::safe_label(body_name) {
            body_name
        } else {
            "analytics_event"
        };
        let mut dynamic_attrs = attrs.clone();
        dynamic_attrs.insert(
            "analytics_event".into(),
            AttrValue::from(body_name.to_string()),
        );
        let Some(provider) = &self.logs else {
            #[cfg(test)]
            self.debug
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .logs
                .push(DebugLogSample {
                    event_name: body_name.to_string(),
                    attributes: dynamic_attrs,
                });
            return;
        };
        let logger = provider.logger(super::logs::EVENTS_SIGNAL);
        let mut record = logger.create_log_record();
        record.set_event_name("analytics_bus_event");
        record.set_body(AnyValue::String(body_name.to_string().into()));
        record.add_attributes(log_attrs(&filter_attributes(&dynamic_attrs, false)));
        set_log_trace_context(&mut record);
        logger.emit(record);
        #[cfg(test)]
        self.debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .logs
            .push(DebugLogSample {
                event_name: body_name.to_string(),
                attributes: dynamic_attrs,
            });
    }

    fn emit_span(&self, name: &'static str, attrs: Vec<KeyValue>) {
        if !self.policy.allowed() {
            return;
        }
        let Some(provider) = &self.traces else {
            return;
        };
        let tracer = provider.tracer(super::logs::TRACING_SIGNAL);
        let mut span = tracer
            .span_builder(name)
            .with_attributes(filter_span_kvs(attrs))
            .start(&tracer);
        span.end();
    }
}

/// Install the process-scoped OTEL runtime from the current environment.
///
/// This is default-off and fail-open: when the master gate is off, or exporter
/// setup fails, the returned guard is inert and the process continues without
/// telemetry side effects.
pub fn install_process(entrypoint: &'static str, record_session_metric: bool) -> TelemetryGuard {
    let Some(config) = super::init_from_env() else {
        disable_process();
        return TelemetryGuard::disabled();
    };
    install_process_with_config(entrypoint, record_session_metric, config)
}

/// Initialize providers without blocking an async host's executor. The Tokio
/// blocking pool preserves its runtime handle for tonic/gRPC clients, while
/// the blocking HTTP client's own constructor is isolated from Tokio below.
pub async fn install_process_with_config_async(
    entrypoint: &'static str,
    record_session_metric: bool,
    config: OtelConfig,
) -> TelemetryGuard {
    match tokio::task::spawn_blocking(move || {
        install_process_with_config(entrypoint, record_session_metric, config)
    })
    .await
    {
        Ok(guard) => guard,
        Err(_) => {
            emit_diag(true, "OpenTelemetry initialization worker failed", false);
            TelemetryGuard::disabled()
        }
    }
}

/// Install the process-scoped OTEL runtime from an already-resolved config.
pub fn install_process_with_config(
    entrypoint: &'static str,
    record_session_metric: bool,
    config: OtelConfig,
) -> TelemetryGuard {
    if !config.enabled || policy::privacy_disabled() {
        disable_process();
        return TelemetryGuard::disabled();
    }
    match OtelRuntime::new(config, entrypoint, record_session_metric) {
        Ok(runtime) => {
            let runtime = Arc::new(runtime);
            if let Ok(mut slot) = runtime_slot().write() {
                if let Some(previous) = slot.replace(runtime.clone()) {
                    previous.policy.revoke();
                    previous.shutdown();
                }
            }
            TelemetryGuard {
                runtime: Some(runtime),
            }
        }
        Err(err) => {
            emit_diag(
                true,
                &format!("OpenTelemetry initialization failed for {entrypoint}: {err}"),
                false,
            );
            TelemetryGuard::disabled()
        }
    }
}

/// Record one floating-point counter update against the active OTEL runtime.
pub fn record_counter(instrument: &str, value: f64, attrs: &Attributes) {
    with_runtime(|runtime| runtime.record_counter(instrument, value, attrs));
}

/// Record one floating-point histogram sample against the active OTEL runtime.
pub fn record_histogram(instrument: &str, value: f64, attrs: &Attributes) {
    with_runtime(|runtime| runtime.record_histogram(instrument, value, attrs));
}

/// Mirror one post-privacy-gate analytics-bus event into the active OTEL
/// runtime's metric and log signals.
pub fn mirror_analytics_event(name: &str, metadata: &LogEventMetadata) {
    with_runtime(|runtime| runtime.mirror_analytics_event(name, metadata));
}

/// Record added/removed line counts from a concrete code-edit operation.
/// Record a lines-added/removed change on `claude_code.lines_of_code.count`.
///
/// Attributes are `type` and `model`, per the counter's OWN description
/// ("...the 'type' attribute indicating whether lines were added or removed
/// and the 'model' attribute indicating which model made the change") and its
/// record site `Zen` (2.1.220 @232432655):
///
/// ```js
/// j2n()?.add(n, {type:"added",   model:t}),
/// j2n()?.add(o, {type:"removed", model:t})
/// ```
///
/// This port emitted `type="add"/"remove"` and a `tool_name` key instead — so
/// the series was unjoinable with the oracle's on BOTH attributes, and the
/// question the metric exists to answer ("which MODEL wrote this code") was
/// unanswerable because the tool name was recorded in its place.
pub fn record_lines_of_code_change(model: &str, added: u64, removed: u64) {
    with_runtime(|runtime| {
        if added > 0 {
            runtime.record_counter(
                metrics::LINES_OF_CODE_COUNT,
                added as f64,
                &attrs_from_optional_pairs([
                    ("type", Some(AttrValue::from("added"))),
                    ("model", Some(AttrValue::from(model.to_string()))),
                ]),
            );
        }
        if removed > 0 {
            runtime.record_counter(
                metrics::LINES_OF_CODE_COUNT,
                removed as f64,
                &attrs_from_optional_pairs([
                    ("type", Some(AttrValue::from("removed"))),
                    ("model", Some(AttrValue::from(model.to_string()))),
                ]),
            );
        }
    });
}

/// Record one hook lifecycle event directly into OTEL.
pub fn emit_hook_lifecycle(
    phase: &'static str,
    status: &'static str,
    tool_name: &str,
    duration_ms: Option<u64>,
) {
    with_runtime(|runtime| {
        let attrs = attrs_from_optional_pairs([
            ("phase", Some(AttrValue::from(phase))),
            ("status", Some(AttrValue::from(status))),
            ("tool_name", Some(AttrValue::from(tool_name.to_string()))),
        ]);
        runtime.record_counter(metrics::HOOK, 1.0, &attrs);
        if let Some(duration_ms) = duration_ms {
            runtime.record_histogram(metrics::HOOK, duration_ms as f64, &attrs);
        }
        runtime.emit_dynamic_log_event(
            match (phase, status) {
                ("pre", "started") => "hook_pre_started",
                ("pre", "completed") => "hook_pre_completed",
                ("post", "started") => "hook_post_started",
                ("post", "completed") => "hook_post_completed",
                _ => "hook_event",
            },
            &attrs,
        );
    });
}

/// Emit the opt-in `assistant_response` OTEL log event with truncation applied.
pub fn emit_assistant_response_log(
    request_id: &str,
    model: &str,
    stop_reason: &str,
    input_tokens: u64,
    output_tokens: u64,
    body: &str,
) {
    with_runtime(|runtime| {
        if !runtime.config.log_include.assistant_responses {
            return;
        }
        let truncated = truncate_content(body, runtime.config.content_max_length);
        let attrs = attrs_from_pairs(&[
            ("request_id", AttrValue::from(request_id.to_string())),
            ("model", AttrValue::from(model.to_string())),
            ("stop_reason", AttrValue::from(stop_reason.to_string())),
            ("input_tokens", AttrValue::from(input_tokens as i64)),
            ("output_tokens", AttrValue::from(output_tokens as i64)),
            ("body", AttrValue::from(truncated.content)),
            ("body_truncated", AttrValue::from(truncated.truncated)),
        ]);
        runtime.emit_log_event("assistant_response", &attrs);
    });
}

/// Emit a named OTEL log event directly against the active runtime.
pub fn emit_named_log_event(name: &'static str, attrs: &Attributes) {
    with_runtime(|runtime| runtime.emit_log_event(name, attrs));
}

/// Emit one `claude_code.user_prompt` record at the prompt-submit seam.
///
/// The record itself is always emitted while telemetry is active. Prompt text
/// is the literal `<REDACTED>` unless `OTEL_LOG_USER_PROMPTS` opted in; even
/// then known token shapes are redacted and the configured content cap applies.
pub fn emit_user_prompt_log(prompt: &str, prompt_id: &str, message_uuid: &str) {
    with_runtime(|runtime| {
        let body = if runtime.config.log_include.user_prompts {
            redact_and_cap("prompt", prompt, runtime.config.content_max_length)
        } else {
            "<REDACTED>".to_string()
        };
        let attrs = attrs_from_pairs(&[
            (
                "prompt_length",
                AttrValue::from(prompt.encode_utf16().count().to_string()),
            ),
            ("prompt", AttrValue::from(body.clone())),
            ("prompt_text", AttrValue::from(body)),
            ("prompt.id", AttrValue::from(prompt_id.to_string())),
            ("message.uuid", AttrValue::from(message_uuid.to_string())),
        ]);
        runtime.emit_log_event("user_prompt", &attrs);
    });
}

/// The code-editing tools whose permission decisions feed the
/// `claude_code.code_edit_tool.decision` counter (binary `pI_`).
const CODE_EDIT_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];

/// Record one RESOLVED tool-permission decision into OTEL — the twin of the
/// binary's adjacent `XNr()?.add(1, await ICs(...))` counter and
/// `vc("tool_decision", …)` log record (2.1.220, the `toolDecisions`
/// bookkeeping site).
///
/// * `decision` — `"accept"` / `"reject"` (CC `c`).
/// * `source` — the `eQ_(decisionReason, behavior)` label: `"config"` for a
///   mode/classifier/safety-check decision or one with no reason, `"hook"`, the
///   `ZX_` rule-scope labels (`user_permanent` / `user_temporary` /
///   `user_reject`), or `"user_abort"` for an aborted permission request.
/// * `file_path` — the tool's `getPath` result (Edit/Write `file_path`,
///   NotebookEdit `notebook_path`); when present the counter carries a
///   `language` attribute resolved via the highlight.js registry (`UMt`),
///   including the explicit `"unknown"` fallback — only a MISSING path omits
///   the attribute (CC `...o&&{language:o}`).
/// * `is_mcp` — whether the tool came from an MCP server, feeding the log
///   record's `tool_source` (CC `Jro(mcpInfo)`).
///
/// The counter fires only for [`CODE_EDIT_TOOLS`] (`xCs`); the `tool_decision`
/// log record fires for every tool. Byte-noop when no runtime is installed.
///
/// The counter and the log record DISAGREE on `tool_name` by design: `ICs`
/// returns the raw `e.name`, while the log record anonymizes it through
/// [`log_tool_name`].
pub fn record_tool_permission_decision(
    tool_name: &str,
    tool_use_id: &str,
    file_path: Option<&str>,
    decision: &str,
    source: &str,
    is_mcp: bool,
    tool_input: Option<&Utf16JsonProjection>,
) {
    with_runtime(|runtime| {
        if CODE_EDIT_TOOLS.contains(&tool_name) {
            let mut attrs = attrs_from_pairs(&[
                ("decision", AttrValue::from(decision.to_string())),
                ("source", AttrValue::from(source.to_string())),
                ("tool_name", AttrValue::from(tool_name.to_string())),
            ]);
            if let Some(path) = file_path {
                attrs.insert("language".into(), AttrValue::from(language_for_path(path)));
            }
            runtime.record_counter(metrics::CODE_EDIT_TOOL_DECISION, 1.0, &attrs);
        }
        // CC log-record attrs are strings (`vc("tool_decision", {decision:c,
        // source:f, tool_name:ua(n.name), tool_use_id:a, ...Jro(n.mcpInfo),
        // …})`). `tool_source` is spread UNCONDITIONALLY — `Jro` always returns
        // a value, so every record carries it. `sdk_host_builtin_mcp` needs
        // `Yro` (`serverType === "sdk" && $Me()`), which no port surface
        // produces, so only builtin/mcp are reachable.
        let mut log_attrs = attrs_from_pairs(&[
            ("decision", AttrValue::from(decision.to_string())),
            ("source", AttrValue::from(source.to_string())),
            (
                "tool_name",
                AttrValue::from(log_tool_name(tool_name).to_string()),
            ),
            ("tool_use_id", AttrValue::from(tool_use_id.to_string())),
            (
                "tool_source",
                AttrValue::from(if is_mcp { "mcp" } else { "builtin" }.to_string()),
            ),
        ]);
        if runtime.config.log_include.tool_details {
            if let Some(parameters) = tool_parameters(tool_name, tool_input) {
                log_attrs.insert(
                    "tool_parameters".into(),
                    AttrValue::from(
                        truncate_content(&parameters, runtime.config.content_max_length).content,
                    ),
                );
            }
        }
        runtime.emit_log_event("tool_decision", &log_attrs);
    });
}

/// Build the bounded tool-detail object used by `tool_decision`/`tool_result`.
fn tool_parameters(tool_name: &str, input: Option<&Utf16JsonProjection>) -> Option<String> {
    let input = input?;
    input.object_entries()?;
    let mut out = Vec::new();
    let copy_string =
        |out: &mut Vec<(String, Utf16JsonProjection)>, out_key: &str, input_key: &str| {
            if let Some(value) = input.string_at(&format!("/{input_key}")) {
                out.push((
                    out_key.to_string(),
                    redact_secret_value(out_key, value).into(),
                ));
            }
        };

    match tool_name {
        "Bash" | "PowerShell" => {
            copy_string(&mut out, "bash_command", "command");
            copy_string(&mut out, "full_command", "command");
            copy_string(&mut out, "description", "description");
            if let Some(timeout) = input
                .at_pointer("/timeout")
                .and_then(JsonValueView::as_number)
                .filter(|value| {
                    value.is_finite()
                        && *value >= 0.0
                        && value.fract() == 0.0
                        && *value < u64::MAX as f64
                })
            {
                out.push((
                    "timeout".into(),
                    Utf16JsonProjection::parse(&timeout.to_string())
                        .expect("numeric timeout is valid JSON"),
                ));
            }
            if let Some(disabled) = input
                .at_pointer("/dangerouslyDisableSandbox")
                .and_then(JsonValueView::as_bool)
            {
                out.push((
                    "dangerouslyDisableSandbox".into(),
                    Utf16JsonProjection::parse(if disabled { "true" } else { "false" })
                        .expect("boolean is valid JSON"),
                ));
            }
        }
        "Skill" => {
            copy_string(&mut out, "skill_name", "skill");
        }
        "Agent" => {
            copy_string(&mut out, "subagent_type", "subagent_type");
        }
        _ => {}
    }
    if let Some(rest) = tool_name.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__") {
            out.push(("mcp_server_name".into(), server.into()));
            out.push(("mcp_tool_name".into(), tool.into()));
        }
    }

    if out.is_empty() {
        return None;
    }
    out.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Utf16JsonProjection::object(out).ok()?.to_json_string().ok()
}

/// claude-code `ua(name)`: the tool name as it appears in a `claude_code.events`
/// LOG record — every `mcp__*` tool collapses to the literal `"mcp_tool"` so a
/// server/tool name never reaches the collector. Non-MCP names pass through.
///
/// CC's `JYi` exemption list rescues exactly two names from the collapse —
/// `mcp__workspace__bash` → `Bash` and `mcp__workspace__web_fetch` →
/// `WebFetch`, the managed-MCP wire ids of two BUILT-IN tools. LingXi ships
/// those tools under their plain names and never emits the `mcp__workspace__`
/// ids, so the exemption is inert here (`rg mcp__workspace__` finds nothing).
///
/// The metric counters keep the RAW name (CC `ICs` returns `tool_name: e.name`);
/// only log records are anonymized.
fn log_tool_name(tool_name: &str) -> &str {
    if tool_name.starts_with("mcp__") {
        "mcp_tool"
    } else {
        tool_name
    }
}

/// Record the `claude_code.commit.count` / `claude_code.pull_request.count`
/// increments for one COMPLETED foreground bash command — the counter subset of
/// the binary's `mEo(command, code, output)` (see [`super::git_ops`]).
///
/// Byte-noop when no runtime is installed (the command scan itself runs inside
/// the runtime closure, so a disabled process pays only the slot read); no-op
/// for non-zero exit codes (CC `if(t!==0)return`). Neither counter carries
/// attributes (`add(1)` bare in the binary).
pub fn record_git_operation_counters(command: &str, exit_code: i32) {
    with_runtime(|runtime| {
        if exit_code != 0 {
            return;
        }
        let hits = git_counter_hits(command);
        if hits.commits > 0 {
            runtime.record_counter(
                metrics::COMMIT_COUNT,
                f64::from(hits.commits),
                &Attributes::new(),
            );
        }
        if hits.pull_requests > 0 {
            runtime.record_counter(
                metrics::PULL_REQUEST_COUNT,
                f64::from(hits.pull_requests),
                &Attributes::new(),
            );
        }
    });
}

/// Render the active Prometheus registry, when the metrics exporter is
/// configured for `prometheus`.
#[must_use]
pub fn prometheus_text() -> Option<String> {
    let slot = runtime_slot().read().ok()?;
    let runtime = slot.as_ref()?;
    if !runtime.policy.allowed() {
        return None;
    }
    let metrics = runtime.metrics.as_ref()?;
    let registry = metrics.prometheus_registry.as_ref()?;
    let encoder = TextEncoder::new();
    let families = registry.gather();
    let mut bytes = Vec::new();
    encoder.encode(&families, &mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// Serialize the currently-attached OpenTelemetry span context, if any.
pub fn capture_current_trace_context() -> Option<SerializedTraceContext> {
    let propagator = TraceContextPropagator::new();
    Context::map_current(|cx| {
        if !cx.span().span_context().is_valid() {
            return None;
        }
        let mut carrier = TraceCarrier::default();
        propagator.inject_context(cx, &mut carrier);
        carrier.into_serialized()
    })
}

/// Run `f` with `trace_context` temporarily attached as the current parent.
pub fn with_trace_context<T>(
    trace_context: Option<&SerializedTraceContext>,
    f: impl FnOnce() -> T,
) -> T {
    let Some(trace_context) = trace_context else {
        return f();
    };
    let propagator = TraceContextPropagator::new();
    let carrier = TraceCarrier::from_serialized(trace_context);
    let cx = propagator.extract(&carrier);
    if !cx.span().span_context().is_valid() {
        return f();
    }
    let _guard = cx.attach();
    f()
}

/// Run `future` with `trace_context` attached while it is being polled.
pub fn with_trace_context_future<T>(
    trace_context: Option<&SerializedTraceContext>,
    future: impl std::future::Future<Output = T>,
) -> impl std::future::Future<Output = T> {
    // Box before constructing this wrapper's state machine. Agent futures can
    // be large; storing one in each await branch multiplies stack pressure even
    // when telemetry is disabled.
    let future = Box::pin(future);
    let trace_context = trace_context.cloned();
    async move {
        let Some(trace_context) = trace_context else {
            return future.await;
        };
        let propagator = TraceContextPropagator::new();
        let carrier = TraceCarrier::from_serialized(&trace_context);
        let cx = propagator.extract(&carrier);
        if !cx.span().span_context().is_valid() {
            return future.await;
        }
        future.with_context(cx).await
    }
}

/// Capture the parent now, before a future is handed to a task scheduler.
pub fn bind_current_trace_context<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    let context = capture_current_trace_context();
    let future = Box::pin(future);
    async move { with_trace_context_future(context.as_ref(), future).await }
}

/// Run one async turn future inside a fresh OTEL span when tracing is enabled.
pub fn with_turn_span<T>(
    span_name: &'static str,
    future: impl std::future::Future<Output = T>,
) -> impl std::future::Future<Output = T> {
    with_operation_span(span_name, Attributes::new(), future)
}

/// Run an operation with the current W3C parent and end it even if cancelled.
/// Only the diagnostic allowlist reaches the outgoing span.
pub fn with_operation_span<T>(
    span_name: &'static str,
    attributes: Attributes,
    future: impl std::future::Future<Output = T>,
) -> impl std::future::Future<Output = T> {
    let future = Box::pin(future);
    async move {
        // Parent lookup remains at poll time so surrounding context restorers
        // establish the parent before this operation starts.
        let span = start_operation_span(span_name, attributes);
        let Some(cx) = span.context.clone() else {
            return future.await;
        };
        let result = future.with_context(cx).await;
        span.set_outcome("completed");
        result
    }
}

/// An operation whose lifetime may span stream polling or multiple futures.
/// Drop ends the span; absent an explicit outcome it records cancellation.
pub struct OperationSpan {
    context: Option<Context>,
    outcome_set: AtomicBool,
}

impl OperationSpan {
    /// The operation's W3C context for polling owned futures and streams.
    pub fn trace_context(&self) -> Option<SerializedTraceContext> {
        let cx = self.context.as_ref()?;
        let _guard = cx.clone().attach();
        capture_current_trace_context()
    }

    /// Bounded outcome labels only; never attach an error message or content.
    pub fn set_outcome(&self, outcome: &'static str) {
        let Some(cx) = &self.context else {
            return;
        };
        if !matches!(outcome, "success" | "error" | "cancelled" | "completed") {
            return;
        }
        self.outcome_set.store(true, Ordering::Release);
        cx.span().set_attribute(KeyValue::new("outcome", outcome));
        if outcome == "error" {
            cx.span()
                .set_status(opentelemetry::trace::Status::error("operation_failed"));
        }
    }
}

impl Drop for OperationSpan {
    fn drop(&mut self) {
        if let Some(cx) = &self.context {
            if !self.outcome_set.load(Ordering::Acquire) {
                cx.span()
                    .set_attribute(KeyValue::new("outcome", "cancelled"));
            }
            cx.span().end();
        }
    }
}

/// Start a child operation of the current W3C context, or a root operation.
pub fn start_operation_span(span_name: &'static str, attributes: Attributes) -> OperationSpan {
    OperationSpan {
        context: current_operation_span_context(span_name, &attributes),
        outcome_set: AtomicBool::new(false),
    }
}

fn current_operation_span_context(span_name: &'static str, attrs: &Attributes) -> Option<Context> {
    let slot = runtime_slot().read().ok()?;
    let runtime = slot.as_ref()?;
    if runtime.shutdown.load(Ordering::Relaxed) || !runtime.policy.allowed() {
        return None;
    }
    let provider = runtime.traces.as_ref()?;
    let tracer = provider.tracer(super::logs::TRACING_SIGNAL);
    let mut attributes = runtime.process_attributes();
    attributes.extend(attrs_to_kvs(&filter_attributes(attrs, false)));
    let span = tracer
        .span_builder(span_name)
        .with_attributes(attributes)
        .start(&tracer);
    Some(Context::current_with_span(span))
}

fn with_runtime(f: impl FnOnce(&OtelRuntime)) {
    if let Ok(slot) = runtime_slot().read() {
        if let Some(runtime) = slot.as_ref() {
            if !runtime.shutdown.load(Ordering::Relaxed) && runtime.policy.allowed() {
                f(runtime);
            }
        }
    }
}

#[derive(Default)]
struct TraceCarrier {
    traceparent: Option<String>,
    tracestate: Option<String>,
}

impl TraceCarrier {
    fn from_serialized(serialized: &SerializedTraceContext) -> Self {
        // Only fixed-width W3C traceparent crosses this boundary. Arbitrary
        // vendor tracestate can contain client data and is not propagated.
        let parent = &serialized.traceparent;
        if parent.len() != 55 || !parent.is_ascii() {
            return Self::default();
        }
        Self {
            traceparent: Some(parent.clone()),
            tracestate: None,
        }
    }

    fn into_serialized(self) -> Option<SerializedTraceContext> {
        self.traceparent.map(|traceparent| SerializedTraceContext {
            traceparent,
            tracestate: None,
        })
    }
}

impl Injector for TraceCarrier {
    fn set(&mut self, key: &str, value: String) {
        if key.eq_ignore_ascii_case("traceparent") {
            self.traceparent = Some(value);
        } else if key.eq_ignore_ascii_case("tracestate") {
            self.tracestate = Some(value);
        }
    }
}

impl Extractor for TraceCarrier {
    fn get(&self, key: &str) -> Option<&str> {
        if key.eq_ignore_ascii_case("traceparent") {
            self.traceparent.as_deref()
        } else if key.eq_ignore_ascii_case("tracestate") {
            self.tracestate.as_deref()
        } else {
            None
        }
    }

    fn keys(&self) -> Vec<&str> {
        let mut keys = Vec::new();
        if self.traceparent.is_some() {
            keys.push("traceparent");
        }
        if self.tracestate.is_some() {
            keys.push("tracestate");
        }
        keys
    }
}

impl OtelRuntime {
    fn mirror_analytics_event(&self, name: &str, metadata: &LogEventMetadata) {
        for update in metric_updates_for_event(name, metadata) {
            match update.kind {
                MetricUpdateKind::Counter => {
                    self.record_counter(update.instrument, update.value, &update.attributes);
                }
                MetricUpdateKind::Histogram => {
                    self.record_histogram(update.instrument, update.value, &update.attributes);
                }
            }
        }

        let attrs = analytics_log_attrs(&self.config, metadata);
        self.emit_dynamic_log_event(name, &attrs);

        // CC-name translation: collectors keying on the binary's
        // `claude_code.events` bodies (`api_request` / `api_error` /
        // `tool_result`) must match even though the port's bus carries
        // `tengu_*` names. The tengu passthrough above is kept for the extra
        // events CC has no named twin for.
        if let Some((cc_name, cc_attrs)) = cc_named_log_for_event(name, metadata) {
            self.emit_log_event(cc_name, &cc_attrs);
        }
    }
}

/// Translate one analytics-bus event into the binary's named
/// `claude_code.events` record, where a faithful twin exists:
///
/// * `tengu_api_success` → `api_request` (CC attr names: `input_tokens`,
///   `cache_read_tokens`, `cost_usd`/`cost_usd_micros`, `duration_ms`,
///   `speed`, `query_source`, …). Numeric attrs stay numeric, as in CC.
/// * `tengu_api_request_failed` / `tengu_api_rate_limited` → `api_error`
///   (`error` carries the port's stable `error_kind` label — CC puts the
///   provider error message there, which the bus does not transport).
/// * the tool `_completed`/`_failed`/`_timeout` families → `tool_result`
///   (`success`/`duration_ms` are STRING attrs, matching `vc("tool_result",
///   {success:"true", duration_ms:String(…)})`). The event set mirrors the
///   metric bridge's `tool.execution` coverage, including its
///   `tool_name_for_event` name inference.
///
/// `user_prompt` and `tool_decision` have no bus twin; they are emitted at
/// their sites via [`emit_named_log_event`] / [`record_tool_permission_decision`].
fn cc_named_log_for_event(
    name: &str,
    metadata: &LogEventMetadata,
) -> Option<(&'static str, Attributes)> {
    match name {
        "tengu_api_success" => {
            let mut attrs = attrs_from_optional_pairs([
                ("model", stable_string(metadata, "model")),
                (
                    "input_tokens",
                    int_value(metadata, "inputTokens").map(AttrValue::from),
                ),
                (
                    "output_tokens",
                    int_value(metadata, "outputTokens").map(AttrValue::from),
                ),
                (
                    "cache_read_tokens",
                    int_value(metadata, "cachedInputTokens").map(AttrValue::from),
                ),
                (
                    "cache_creation_tokens",
                    int_value(metadata, "uncachedInputTokens").map(AttrValue::from),
                ),
                (
                    "cost_usd",
                    float_value(metadata, "costUSD").map(AttrValue::from),
                ),
                (
                    "duration_ms",
                    int_value(metadata, "durationMs").map(AttrValue::from),
                ),
                ("request_id", stable_string(metadata, "requestId")),
                ("query_source", stable_string(metadata, "querySource")),
            ]);
            if let Some(cost_usd) = float_value(metadata, "costUSD") {
                // CC `cost_usd_micros: Math.round(V*1e6)`.
                attrs.insert(
                    "cost_usd_micros".into(),
                    AttrValue::from((cost_usd * 1e6).round() as i64),
                );
            }
            // CC `speed: O ? "fast" : "normal"` — from the bus `fastMode` bool.
            if let Some(AnalyticsValue::Bool(fast)) = metadata.get("fastMode") {
                attrs.insert(
                    "speed".into(),
                    AttrValue::from(if *fast { "fast" } else { "normal" }),
                );
            }
            Some(("api_request", attrs))
        }
        "tengu_api_request_failed" | "tengu_api_rate_limited" => {
            let mut attrs = attrs_from_optional_pairs([
                ("model", stable_string(metadata, "model")),
                ("error", stable_string(metadata, "error_kind")),
                ("request_id", stable_string(metadata, "request_id")),
                (
                    "status_code",
                    int_value(metadata, "status_code").map(AttrValue::from),
                ),
            ]);
            if name == "tengu_api_rate_limited" && !attrs.contains_key("error") {
                attrs.insert("error".into(), AttrValue::from("rate_limited"));
            }
            Some(("api_error", attrs))
        }
        _ => {
            if !(name.contains("_completed")
                || name.contains("_failed")
                || name.contains("_timeout"))
            {
                return None;
            }
            let tool_name = tool_name_for_event(name, metadata)?;
            let success = if tool_status(name) == "completed" {
                "true"
            } else {
                "false"
            };
            let mut attrs = attrs_from_pairs(&[
                ("tool_name", tool_name),
                ("success", AttrValue::from(success)),
            ]);
            if let Some(duration_ms) = int_value(metadata, "duration_ms") {
                // CC stringifies (`duration_ms: String(se)`).
                attrs.insert(
                    "duration_ms".into(),
                    AttrValue::from(duration_ms.to_string()),
                );
            }
            Some(("tool_result", attrs))
        }
    }
}

fn metric_updates_for_event(name: &str, metadata: &LogEventMetadata) -> Vec<MetricUpdate> {
    let mut out = Vec::new();
    match name {
        "tengu_api_success" => {
            let shared = attrs_from_optional_pairs([
                ("model", stable_string(metadata, "model")),
                ("provider", stable_string(metadata, "provider")),
                ("status", Some(AttrValue::from("success"))),
                ("query_source", stable_string(metadata, "querySource")),
            ]);
            out.push(counter_update(metrics::LLM_REQUEST, 1.0, shared.clone()));
            if let Some(duration_ms) = int_value(metadata, "durationMs") {
                out.push(histogram_update(
                    metrics::LLM_REQUEST,
                    duration_ms as f64,
                    shared.clone(),
                ));
            }
            for (key, token_type) in [
                ("inputTokens", "input"),
                ("outputTokens", "output"),
                ("cachedInputTokens", "cache_read"),
                ("uncachedInputTokens", "cache_create"),
            ] {
                if let Some(value) = int_value(metadata, key) {
                    out.push(counter_update(
                        metrics::TOKEN_USAGE,
                        value as f64,
                        attrs_from_optional_pairs([
                            ("model", stable_string(metadata, "model")),
                            ("provider", stable_string(metadata, "provider")),
                            ("token_type", Some(AttrValue::from(token_type))),
                        ]),
                    ));
                }
            }
            if let Some(cost_usd) = float_value(metadata, "costUSD") {
                out.push(counter_update(
                    metrics::COST_USAGE,
                    cost_usd,
                    attrs_from_optional_pairs([
                        ("model", stable_string(metadata, "model")),
                        ("provider", stable_string(metadata, "provider")),
                    ]),
                ));
            }
        }
        "tengu_api_request_failed" => {
            out.push(counter_update(
                metrics::LLM_REQUEST,
                1.0,
                attrs_from_optional_pairs([
                    ("model", stable_string(metadata, "model")),
                    ("status", Some(AttrValue::from("failed"))),
                    ("error_kind", stable_string(metadata, "error_kind")),
                ]),
            ));
        }
        "tengu_api_rate_limited" => {
            out.push(counter_update(
                metrics::LLM_REQUEST,
                1.0,
                attrs_from_optional_pairs([
                    ("model", stable_string(metadata, "model")),
                    ("status", Some(AttrValue::from("rate_limited"))),
                ]),
            ));
        }
        "tengu_api_request_cancelled" => {
            out.push(counter_update(
                metrics::LLM_REQUEST,
                1.0,
                attrs_from_optional_pairs([
                    ("model", stable_string(metadata, "model")),
                    ("status", Some(AttrValue::from("cancelled"))),
                ]),
            ));
        }
        "tengu_tool_permission_requested" => {
            out.push(counter_update(
                metrics::TOOL_BLOCKED_ON_USER,
                1.0,
                attrs_from_optional_pairs([("tool_name", tool_name_for_event(name, metadata))]),
            ));
        }
        "tengu_tool_bash_completed" | "tengu_tool_bash_timeout" | "tengu_tool_bash_failed" => {
            out.extend(tool_execution_updates("Bash", name, metadata));
            out.push(counter_update(
                metrics::BASH_SUBPROCESS,
                1.0,
                attrs_from_optional_pairs([("status", Some(AttrValue::from(tool_status(name))))]),
            ));
            if let Some(duration_ms) = int_value(metadata, "duration_ms") {
                out.push(histogram_update(
                    metrics::BASH_SUBPROCESS,
                    duration_ms as f64,
                    attrs_from_optional_pairs([(
                        "status",
                        Some(AttrValue::from(tool_status(name))),
                    )]),
                ));
            }
        }
        "tengu_tool_mcp_invoked" | "tengu_tool_mcp_completed" | "tengu_tool_mcp_failed" => {
            out.extend(tool_execution_updates("Mcp", name, metadata));
            if name != "tengu_tool_mcp_invoked" {
                let status = tool_status(name);
                out.push(counter_update(
                    metrics::MCP_RPC,
                    1.0,
                    attrs_from_optional_pairs([("status", Some(AttrValue::from(status)))]),
                ));
                if let Some(duration_ms) = int_value(metadata, "duration_ms") {
                    out.push(histogram_update(
                        metrics::MCP_RPC,
                        duration_ms as f64,
                        attrs_from_optional_pairs([("status", Some(AttrValue::from(status)))]),
                    ));
                }
            }
        }
        "tengu_tool_task_dispatched" | "tengu_tool_agent_started" => {
            out.push(counter_update(
                metrics::SUBAGENT_SPAWN,
                1.0,
                attrs_from_optional_pairs([("tool_name", tool_name_for_event(name, metadata))]),
            ));
        }
        "tengu_tool_task_completed" | "tengu_tool_agent_completed" | "tengu_tool_agent_failed" => {
            out.extend(tool_execution_updates(
                tool_name_for_event(name, metadata)
                    .as_ref()
                    .and_then(attr_str)
                    .unwrap_or("Agent"),
                name,
                metadata,
            ));
            if let Some(duration_ms) = int_value(metadata, "duration_ms") {
                out.push(histogram_update(
                    metrics::SUBAGENT_SPAWN,
                    duration_ms as f64,
                    attrs_from_optional_pairs([(
                        "status",
                        Some(AttrValue::from(tool_status(name))),
                    )]),
                ));
            }
        }
        "tengu_auto_compact_prefix_overflow"
        | "tengu_auto_compact_rapid_refill_breaker"
        | "tengu_command_compact_completed"
        | "tengu_command_compact_failed" => {
            let status = if name.ends_with("_failed") {
                "failed"
            } else {
                "completed"
            };
            out.push(counter_update(
                metrics::COMPACTION,
                1.0,
                attrs_from_optional_pairs([("status", Some(AttrValue::from(status)))]),
            ));
            if let Some(duration_ms) = int_value(metadata, "duration_ms") {
                out.push(histogram_update(
                    metrics::COMPACTION,
                    duration_ms as f64,
                    attrs_from_optional_pairs([("status", Some(AttrValue::from(status)))]),
                ));
            }
        }
        _ => {
            if let Some(tool_name) = tool_name_for_event(name, metadata) {
                if name.contains("_completed")
                    || name.contains("_failed")
                    || name.contains("_timeout")
                {
                    let tool_name_str = attr_str(&tool_name).unwrap_or("tool");
                    out.extend(tool_execution_updates(tool_name_str, name, metadata));
                }
            }
        }
    }
    out
}

fn tool_execution_updates(
    tool_name: &str,
    event_name: &str,
    metadata: &LogEventMetadata,
) -> Vec<MetricUpdate> {
    let mut out = vec![counter_update(
        metrics::TOOL_EXECUTION,
        1.0,
        attrs_from_optional_pairs([
            ("tool_name", Some(AttrValue::from(tool_name.to_string()))),
            ("status", Some(AttrValue::from(tool_status(event_name)))),
        ]),
    )];
    if tool_name == "Mcp" {
        if let Some(duration_ms) = int_value(metadata, "duration_ms") {
            out.push(histogram_update(
                metrics::TOOL_EXECUTION,
                duration_ms as f64,
                attrs_from_optional_pairs([
                    ("tool_name", Some(AttrValue::from(tool_name.to_string()))),
                    ("status", Some(AttrValue::from(tool_status(event_name)))),
                ]),
            ));
        }
    }
    out
}

fn tool_status(event_name: &str) -> &'static str {
    if event_name.ends_with("_failed") {
        "failed"
    } else if event_name.ends_with("_timeout") {
        "timeout"
    } else {
        "completed"
    }
}

fn tool_name_for_event(event_name: &str, metadata: &LogEventMetadata) -> Option<AttrValue> {
    if let Some(name) = stable_string(metadata, "tool_name") {
        return Some(name);
    }
    let name = if event_name.contains("_bash_") {
        "Bash"
    } else if event_name.contains("_mcp_") {
        "Mcp"
    } else if event_name.contains("_task_") {
        "Agent"
    } else if event_name.contains("_agent_") {
        "Agent"
    } else if event_name.contains("_edit_") {
        "Edit"
    } else if event_name.contains("_write_") {
        "Write"
    } else if event_name.contains("_read_") {
        "Read"
    } else if event_name.contains("_web_fetch_") {
        "WebFetch"
    } else if event_name.contains("_web_search_") {
        "WebSearch"
    } else if event_name.contains("_enter_worktree_") {
        "EnterWorktree"
    } else if event_name.contains("_exit_worktree_") {
        "ExitWorktree"
    } else if event_name.contains("_schedule_cron_") {
        "ScheduleCron"
    } else if event_name.contains("_cron_delete_") {
        "CronDelete"
    } else if event_name.contains("_cron_list_") {
        "CronList"
    } else if event_name.contains("_tool_search_") {
        "ToolSearch"
    } else if event_name.contains("_config_") {
        "Config"
    } else {
        return None;
    };
    Some(AttrValue::from(name))
}

fn analytics_log_attrs(config: &OtelConfig, metadata: &LogEventMetadata) -> Attributes {
    let mut attrs = Attributes::new();
    for (key, value) in metadata {
        if key.starts_with("_PROTO_") {
            if let Some((log_key, body)) = gated_proto_attr(config, key, value) {
                attrs.insert(log_key, body);
            }
            continue;
        }
        if let Some(attr) = analytics_value_to_attr(key, value) {
            attrs.insert(key.clone(), attr);
        }
    }
    attrs
}

fn gated_proto_attr(
    config: &OtelConfig,
    key: &str,
    value: &AnalyticsValue,
) -> Option<(String, AttrValue)> {
    let gate = if key.contains("prompt") || key.contains("query") {
        config.log_include.user_prompts
    } else if key.contains("command")
        || key.contains("url")
        || key.contains("slug")
        || key.contains("server_name")
        || key.contains("tool_name")
        || key.contains("branch_name")
    {
        config.log_include.tool_details
    } else if key.contains("reason") {
        config.log_include.user_prompts || config.log_include.tool_content
    } else {
        false
    };
    if !gate {
        return None;
    }
    let AnalyticsValue::String(raw) = value else {
        return analytics_value_to_attr(key, value)
            .map(|attr| (key.trim_start_matches("_PROTO_").to_ascii_lowercase(), attr));
    };
    let sanitized = redact_and_cap(key, raw, config.content_max_length);
    Some((
        key.trim_start_matches("_PROTO_").to_ascii_lowercase(),
        AttrValue::from(sanitized),
    ))
}

fn analytics_value_to_attr(key: &str, value: &AnalyticsValue) -> Option<AttrValue> {
    match value {
        AnalyticsValue::Bool(v) => Some(AttrValue::from(*v)),
        AnalyticsValue::Int(v) => Some(AttrValue::from(*v)),
        AnalyticsValue::Float(v) => Some(AttrValue::from(*v)),
        AnalyticsValue::String(v) => Some(AttrValue::from(redact_secret_value(key, v))),
        AnalyticsValue::None => None,
    }
}

fn redact_and_cap(key: &str, value: &str, max_len: i64) -> String {
    let redacted = redact_secret_value(key, value);
    truncate_content(&redacted, max_len).content
}

fn redact_secret_value(key: &str, value: &str) -> String {
    let lowered = key.to_ascii_lowercase();
    // Analytics producers use snake_case, HTTP-style hyphenated names and
    // occasionally dotted keys. Normalize those separators before matching so
    // `x-api-key` cannot bypass the same policy as `api_key`.
    let normalized = lowered.replace(['-', '.'], "_");
    if lowered.contains("secret")
        || lowered.contains("token")
        || lowered.contains("authorization")
        || normalized.contains("api_key")
        || lowered.contains("password")
    {
        return "[REDACTED]".to_string();
    }
    let mut out = value.to_string();
    for marker in ["Bearer ", "bearer ", "sk-", "ctx7sk-", "ghp_"] {
        if let Some(index) = out.find(marker) {
            let suffix_end = out[index + marker.len()..]
                .find(char::is_whitespace)
                .map(|delta| index + marker.len() + delta)
                .unwrap_or(out.len());
            out.replace_range(index..suffix_end, "[REDACTED]");
        }
    }
    out
}

fn counter_update(instrument: &'static str, value: f64, attributes: Attributes) -> MetricUpdate {
    MetricUpdate {
        instrument,
        value,
        attributes,
        kind: MetricUpdateKind::Counter,
    }
}

fn histogram_update(instrument: &'static str, value: f64, attributes: Attributes) -> MetricUpdate {
    MetricUpdate {
        instrument,
        value,
        attributes,
        kind: MetricUpdateKind::Histogram,
    }
}

fn stable_string(metadata: &LogEventMetadata, key: &str) -> Option<AttrValue> {
    let AnalyticsValue::String(value) = metadata.get(key)? else {
        return None;
    };
    Some(AttrValue::from(redact_secret_value(key, value)))
}

fn int_value(metadata: &LogEventMetadata, key: &str) -> Option<i64> {
    match metadata.get(key)? {
        AnalyticsValue::Int(value) => Some(*value),
        AnalyticsValue::Float(value) => Some(*value as i64),
        _ => None,
    }
}

fn float_value(metadata: &LogEventMetadata, key: &str) -> Option<f64> {
    match metadata.get(key)? {
        AnalyticsValue::Float(value) => Some(*value),
        AnalyticsValue::Int(value) => Some(*value as f64),
        _ => None,
    }
}

fn attrs_from_optional_pairs<const N: usize>(pairs: [(&str, Option<AttrValue>); N]) -> Attributes {
    pairs
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_string(), value)))
        .collect()
}

fn attr_str(value: &AttrValue) -> Option<&str> {
    match value {
        AttrValue::Str(value) => Some(value.as_str()),
        _ => None,
    }
}

fn build_metrics_runtime(
    config: &OtelConfig,
    resource: Resource,
    policy: &Arc<ExportPolicy>,
) -> Result<Option<MetricsRuntime>, String> {
    if !config.enabled || !policy.allowed() {
        return Ok(None);
    }
    let provider = match &config.metrics.kind {
        ExporterKind::None => return Ok(None),
        ExporterKind::Prometheus => {
            let registry = Registry::new();
            let reader = prometheus_exporter()
                .with_registry(registry.clone())
                .build()
                .map_err(|err| err.to_string())?;
            let provider = SdkMeterProvider::builder()
                .with_resource(resource)
                .with_reader(reader)
                .build();
            return Ok(Some(MetricsRuntime::new(provider, Some(registry))));
        }
        ExporterKind::Console => {
            let reader = PeriodicReader::builder(GatedExporter::new(
                opentelemetry_stdout::MetricExporter::default(),
                policy,
            ))
            .with_interval(duration_ms_opt(config.metrics.export_interval_ms))
            .build();
            SdkMeterProvider::builder()
                .with_resource(resource)
                .with_reader(reader)
                .build()
        }
        ExporterKind::Otlp => {
            let exporter = build_metric_exporter(&config.metrics)?;
            let reader = PeriodicReader::builder(GatedExporter::new(exporter, policy))
                .with_interval(duration_ms_opt(config.metrics.export_interval_ms))
                .build();
            SdkMeterProvider::builder()
                .with_resource(resource)
                .with_reader(reader)
                .build()
        }
        ExporterKind::Other(other) => {
            emit_diag(
                false,
                &format!("unsupported OTEL metrics exporter `{other}`; metrics disabled"),
                config.timeouts.diag_stderr,
            );
            return Ok(None);
        }
    };

    Ok(Some(MetricsRuntime::new(provider, None)))
}

fn build_logger_provider(
    config: &OtelConfig,
    resource: Resource,
    policy: &Arc<ExportPolicy>,
) -> Result<Option<SdkLoggerProvider>, String> {
    if !config.enabled || !policy.allowed() {
        return Ok(None);
    }
    let provider = match &config.logs.kind {
        ExporterKind::None => return Ok(None),
        ExporterKind::Prometheus => {
            emit_diag(
                false,
                "OTEL logs exporter `prometheus` is unsupported; logs disabled",
                config.timeouts.diag_stderr,
            );
            return Ok(None);
        }
        ExporterKind::Console => {
            let batch = BatchLogProcessor::builder(GatedExporter::new(
                opentelemetry_stdout::LogExporter::default(),
                policy,
            ))
            .with_batch_config(log_batch_config(config))
            .build();
            SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_log_processor(batch)
                .build()
        }
        ExporterKind::Otlp => {
            let exporter = build_log_exporter(&config.logs)?;
            let batch = BatchLogProcessor::builder(GatedExporter::new(exporter, policy))
                .with_batch_config(log_batch_config(config))
                .build();
            SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_log_processor(batch)
                .build()
        }
        ExporterKind::Other(other) => {
            emit_diag(
                false,
                &format!("unsupported OTEL logs exporter `{other}`; logs disabled"),
                config.timeouts.diag_stderr,
            );
            return Ok(None);
        }
    };

    Ok(Some(provider))
}

fn build_tracer_provider(
    config: &OtelConfig,
    resource: Resource,
    policy: &Arc<ExportPolicy>,
) -> Result<Option<SdkTracerProvider>, String> {
    if !config.enabled || !policy.allowed() {
        return Ok(None);
    }

    let provider = match &config.traces.kind {
        ExporterKind::None => return Ok(None),
        ExporterKind::Prometheus => {
            emit_diag(
                false,
                "OTEL traces exporter `prometheus` is unsupported; traces disabled",
                config.timeouts.diag_stderr,
            );
            return Ok(None);
        }
        ExporterKind::Console => {
            let batch = BatchSpanProcessor::builder(GatedExporter::new(
                opentelemetry_stdout::SpanExporter::default(),
                policy,
            ))
            .with_batch_config(trace_batch_config(config))
            .build();
            SdkTracerProvider::builder()
                .with_resource(resource)
                .with_span_processor(batch)
                .with_sampler(trace_sampler(config))
                .build()
        }
        ExporterKind::Otlp => {
            let exporter = build_span_exporter(&config.traces)?;
            let batch = BatchSpanProcessor::builder(GatedExporter::new(exporter, policy))
                .with_batch_config(trace_batch_config(config))
                .build();
            SdkTracerProvider::builder()
                .with_resource(resource)
                .with_span_processor(batch)
                .with_sampler(trace_sampler(config))
                .build()
        }
        ExporterKind::Other(other) => {
            emit_diag(
                false,
                &format!("unsupported OTEL traces exporter `{other}`; traces disabled"),
                config.timeouts.diag_stderr,
            );
            return Ok(None);
        }
    };

    Ok(Some(provider))
}

fn build_metric_exporter(
    config: &OtlpExporterConfig,
) -> Result<opentelemetry_otlp::MetricExporter, String> {
    match config.protocol {
        OtlpProtocol::Grpc => {
            let mut builder = opentelemetry_otlp::MetricExporter::builder().with_tonic();
            builder = apply_grpc_export_config(builder, config)?;
            builder.build().map_err(|err| err.to_string())
        }
        OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
            let mut builder = opentelemetry_otlp::MetricExporter::builder().with_http();
            builder = apply_http_export_config(builder, config)?;
            builder.build().map_err(|err| err.to_string())
        }
    }
}

fn build_log_exporter(
    config: &OtlpExporterConfig,
) -> Result<opentelemetry_otlp::LogExporter, String> {
    match config.protocol {
        OtlpProtocol::Grpc => {
            let mut builder = opentelemetry_otlp::LogExporter::builder().with_tonic();
            builder = apply_grpc_export_config(builder, config)?;
            builder.build().map_err(|err| err.to_string())
        }
        OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
            let mut builder = opentelemetry_otlp::LogExporter::builder().with_http();
            builder = apply_http_export_config(builder, config)?;
            builder.build().map_err(|err| err.to_string())
        }
    }
}

fn build_span_exporter(
    config: &OtlpExporterConfig,
) -> Result<opentelemetry_otlp::SpanExporter, String> {
    match config.protocol {
        OtlpProtocol::Grpc => {
            let mut builder = opentelemetry_otlp::SpanExporter::builder().with_tonic();
            builder = apply_grpc_export_config(builder, config)?;
            builder.build().map_err(|err| err.to_string())
        }
        OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
            let mut builder = opentelemetry_otlp::SpanExporter::builder().with_http();
            builder = apply_http_export_config(builder, config)?;
            builder.build().map_err(|err| err.to_string())
        }
    }
}

fn apply_http_export_config<T>(mut builder: T, config: &OtlpExporterConfig) -> Result<T, String>
where
    T: WithExportConfig + WithHttpConfig,
{
    if let Some(endpoint) = normalize_http_endpoint(config)? {
        builder = builder.with_endpoint(endpoint);
    }
    builder = builder.with_protocol(protocol_for(config.protocol));
    if let Some(timeout_ms) = config.timeout_ms {
        builder = builder.with_timeout(duration_ms(timeout_ms));
    }
    if !config.headers.is_empty() {
        builder = builder.with_headers(
            config
                .headers
                .clone()
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );
    }
    if compression_from_config(config.compression.as_deref())?.is_some() {
        return Err(
            "OTLP HTTP compression is not supported on the Rust-1.82-compatible OpenTelemetry line"
                .to_string(),
        );
    }
    builder = builder.with_http_client(build_http_client(config)?);
    Ok(builder)
}

fn apply_grpc_export_config<T>(mut builder: T, config: &OtlpExporterConfig) -> Result<T, String>
where
    T: WithExportConfig + opentelemetry_otlp::WithTonicConfig,
{
    if let Some(endpoint) = normalize_grpc_endpoint(config) {
        builder = builder.with_endpoint(endpoint);
    }
    builder = builder.with_protocol(OtlpWireProtocol::Grpc);
    if let Some(timeout_ms) = config.timeout_ms {
        builder = builder.with_timeout(duration_ms(timeout_ms));
    }
    if !config.headers.is_empty() {
        builder = builder.with_metadata(metadata_from_headers(&config.headers)?);
    }
    if let Some(compression) = compression_from_config(config.compression.as_deref())? {
        builder = builder.with_compression(compression);
    }
    if let Some(tls) = build_tonic_tls_config(config)? {
        builder = builder.with_tls_config(tls);
    }
    Ok(builder)
}

fn build_http_client(config: &OtlpExporterConfig) -> Result<reqwest::blocking::Client, String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        let config = config.clone();
        return thread::Builder::new()
            .name("lingxi-otel-http-init".into())
            .spawn(move || build_http_client_blocking(&config))
            .map_err(|_| "failed to spawn OTLP HTTP initialization worker".to_string())?
            .join()
            .map_err(|_| "OTLP HTTP initialization worker failed".to_string())?;
    }
    build_http_client_blocking(config)
}

fn build_http_client_blocking(
    config: &OtlpExporterConfig,
) -> Result<reqwest::blocking::Client, String> {
    let mut builder = reqwest::blocking::Client::builder();
    if let Some(timeout_ms) = config.timeout_ms {
        builder = builder.timeout(duration_ms(timeout_ms));
    }
    if config.insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(path) = &config.certificate {
        let cert = fs::read(path)
            .map_err(|err| format!("failed to read OTLP CA certificate `{path}`: {err}"))?;
        let cert = reqwest::Certificate::from_pem(&cert).map_err(|err| err.to_string())?;
        builder = builder.add_root_certificate(cert);
    }
    match (&config.client_certificate, &config.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert = fs::read(cert_path).map_err(|err| {
                format!("failed to read OTLP client certificate `{cert_path}`: {err}")
            })?;
            let key = fs::read(key_path)
                .map_err(|err| format!("failed to read OTLP client key `{key_path}`: {err}"))?;
            let mut pem = cert;
            pem.push(b'\n');
            pem.extend(key);
            let identity = reqwest::Identity::from_pem(&pem).map_err(|err| err.to_string())?;
            builder = builder.identity(identity);
        }
        (None, None) => {}
        _ => {
            return Err(
                "OTLP HTTP mTLS requires both client certificate and client key paths".to_string(),
            );
        }
    }
    builder.build().map_err(|err| err.to_string())
}

fn build_tonic_tls_config(
    config: &OtlpExporterConfig,
) -> Result<Option<tonic::transport::ClientTlsConfig>, String> {
    let needs_tls = config
        .endpoint
        .as_deref()
        .is_some_and(|endpoint| endpoint.starts_with("https://"));
    if !needs_tls && config.certificate.is_none() && config.client_certificate.is_none() {
        return Ok(None);
    }

    let mut tls = tonic::transport::ClientTlsConfig::new();
    if let Some(endpoint) = &config.endpoint {
        if let Ok(url) = reqwest::Url::parse(endpoint) {
            if let Some(host) = url.host_str() {
                tls = tls.domain_name(host.to_string());
            }
        }
    }
    if let Some(path) = &config.certificate {
        let cert = fs::read(path)
            .map_err(|err| format!("failed to read OTLP CA certificate `{path}`: {err}"))?;
        tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(cert));
    }
    match (&config.client_certificate, &config.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert = fs::read(cert_path).map_err(|err| {
                format!("failed to read OTLP client certificate `{cert_path}`: {err}")
            })?;
            let key = fs::read(key_path)
                .map_err(|err| format!("failed to read OTLP client key `{key_path}`: {err}"))?;
            tls = tls.identity(tonic::transport::Identity::from_pem(cert, key));
        }
        (None, None) => {}
        _ => {
            return Err(
                "OTLP gRPC mTLS requires both client certificate and client key paths".to_string(),
            );
        }
    }
    Ok(Some(tls))
}

fn metadata_from_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<tonic::metadata::MetadataMap, String> {
    let mut metadata = tonic::metadata::MetadataMap::new();
    for (key, value) in headers {
        let metadata_key: tonic::metadata::AsciiMetadataKey = key
            .parse()
            .map_err(|err| format!("invalid OTLP gRPC metadata key `{key}`: {err}"))?;
        let parsed = value
            .parse()
            .map_err(|err| format!("invalid OTLP gRPC metadata value for `{key}`: {err}"))?;
        metadata.insert(metadata_key, parsed);
    }
    Ok(metadata)
}

fn protocol_for(protocol: OtlpProtocol) -> OtlpWireProtocol {
    match protocol {
        OtlpProtocol::Grpc => OtlpWireProtocol::Grpc,
        OtlpProtocol::HttpProtobuf => OtlpWireProtocol::HttpBinary,
        OtlpProtocol::HttpJson => OtlpWireProtocol::HttpJson,
    }
}

fn compression_from_config(raw: Option<&str>) -> Result<Option<Compression>, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some("none") => Ok(None),
        Some("gzip") => Ok(Some(Compression::Gzip)),
        Some(other) => Err(format!("unsupported OTLP compression `{other}`")),
    }
}

fn normalize_http_endpoint(config: &OtlpExporterConfig) -> Result<Option<String>, String> {
    let Some(endpoint) = config.endpoint.clone() else {
        return Ok(None);
    };
    let mut url = reqwest::Url::parse(&endpoint).map_err(|err| err.to_string())?;
    if !config.endpoint_is_signal_specific {
        let suffix = match config.signal {
            super::config::Signal::Metrics => "/v1/metrics",
            super::config::Signal::Logs => "/v1/logs",
            super::config::Signal::Traces => "/v1/traces",
        };
        url.set_path(&format!("{}{suffix}", url.path().trim_end_matches('/')));
    }
    Ok(Some(url.to_string()))
}

fn normalize_grpc_endpoint(config: &OtlpExporterConfig) -> Option<String> {
    let Some(endpoint) = config.endpoint.clone() else {
        return None;
    };
    if config.insecure && endpoint.starts_with("https://") {
        Some(format!("http://{}", &endpoint["https://".len()..]))
    } else {
        Some(endpoint)
    }
}

fn build_resource(config: &OtelConfig) -> Resource {
    let service_name = if policy::safe_label(&config.service_name) {
        config.service_name.as_str()
    } else {
        super::config::DEFAULT_SERVICE_NAME
    };
    let mut builder = Resource::builder_empty()
        .with_service_name(service_name.to_owned())
        .with_attributes([
            KeyValue::new("service.instance.id", new_instance_id()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("os.type", std::env::consts::OS),
            KeyValue::new("host.arch", std::env::consts::ARCH),
        ]);
    let attrs = config
        .resource_attributes
        .as_deref()
        .map(parse_resource_attributes)
        .unwrap_or_default();
    if !attrs.is_empty() {
        builder = builder.with_attributes(attrs);
    }
    builder.build()
}

fn parse_resource_attributes(raw: &str) -> Vec<KeyValue> {
    raw.split(',')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            if !policy::resource_key_allowed(key) || !policy::safe_label(value) {
                return None;
            }
            Some(KeyValue::new(key.to_string(), value.to_string()))
        })
        .collect()
}

fn resource_kvs(resource: &Resource) -> Vec<KeyValue> {
    resource
        .iter()
        // The resource distinguishes cumulative metric writers. Copying its
        // ephemeral identity into each datapoint would create a user-facing
        // high-cardinality dimension, so keep it exclusively on the resource.
        .filter(|(key, _)| key.as_str() != "service.instance.id")
        .map(|(key, value)| KeyValue::new(key.to_string(), value.clone()))
        .collect()
}

fn new_instance_id() -> String {
    use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};
    let mut bytes = RandomIdGenerator::default().new_trace_id().to_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = opentelemetry::trace::TraceId::from_bytes(bytes).to_string();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn attrs_to_kvs(attrs: &Attributes) -> Vec<KeyValue> {
    attrs
        .iter()
        .map(|(key, value)| attr_to_key_value(key, value))
        .collect()
}

fn filter_span_kvs(attrs: Vec<KeyValue>) -> Vec<KeyValue> {
    attrs.into_iter().filter(|attr| {
        let key = attr.key.as_str();
        if policy::resource_key_allowed(key) {
            return matches!(&attr.value, opentelemetry::Value::String(value) if policy::safe_label(value.as_str()));
        }
        let value = match &attr.value {
            opentelemetry::Value::String(value) => AttrValue::from(value.as_str()),
            opentelemetry::Value::Bool(value) => AttrValue::from(*value),
            opentelemetry::Value::I64(value) => AttrValue::from(*value),
            opentelemetry::Value::F64(value) => AttrValue::from(*value),
            _ => return false,
        };
        filter_attributes(&attrs_from_pairs(&[(key, value)]), false).contains_key(key)
    }).collect()
}

fn set_log_trace_context(record: &mut impl opentelemetry::logs::LogRecord) {
    Context::map_current(|context| {
        let span = context.span();
        let span_context = span.span_context();
        if span_context.is_valid() {
            record.set_trace_context(
                span_context.trace_id(),
                span_context.span_id(),
                Some(span_context.trace_flags()),
            );
        }
    });
}

fn log_attrs(attrs: &Attributes) -> Vec<(String, AnyValue)> {
    attrs
        .iter()
        .map(|(key, value)| (key.clone(), attr_to_log_value(value)))
        .collect()
}

fn attr_to_key_value(key: &str, value: &AttrValue) -> KeyValue {
    match value {
        AttrValue::Str(v) => KeyValue::new(key.to_string(), v.clone()),
        AttrValue::Int(v) => KeyValue::new(key.to_string(), *v),
        AttrValue::Float(v) => KeyValue::new(key.to_string(), *v),
        AttrValue::Bool(v) => KeyValue::new(key.to_string(), *v),
    }
}

fn attr_to_log_value(value: &AttrValue) -> AnyValue {
    match value {
        AttrValue::Str(v) => AnyValue::String(v.clone().into()),
        AttrValue::Int(v) => AnyValue::Int(*v),
        AttrValue::Float(v) => AnyValue::Double(*v),
        AttrValue::Bool(v) => AnyValue::Boolean(*v),
    }
}

fn attrs_from_pairs(pairs: &[(&str, AttrValue)]) -> Attributes {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

fn log_batch_config(config: &OtelConfig) -> opentelemetry_sdk::logs::BatchConfig {
    LogBatchConfigBuilder::default()
        .with_scheduled_delay(duration_ms_opt(config.logs.export_interval_ms))
        .build()
}

fn trace_batch_config(_config: &OtelConfig) -> opentelemetry_sdk::trace::BatchConfig {
    TraceBatchConfigBuilder::default().build()
}

fn trace_sampler(config: &OtelConfig) -> opentelemetry_sdk::trace::Sampler {
    use opentelemetry_sdk::trace::Sampler;

    match config.traces_sampler.as_deref() {
        Some("always_on") => Sampler::AlwaysOn,
        Some("always_off") => Sampler::AlwaysOff,
        Some("traceidratio") => {
            Sampler::TraceIdRatioBased(parse_ratio(config.traces_sampler_arg.as_deref()))
        }
        Some("parentbased_always_off") => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        Some("parentbased_traceidratio") => Sampler::ParentBased(Box::new(
            Sampler::TraceIdRatioBased(parse_ratio(config.traces_sampler_arg.as_deref())),
        )),
        _ => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
    }
}

fn parse_ratio(raw: Option<&str>) -> f64 {
    raw.and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}

fn exporter_kind_name(kind: &ExporterKind) -> &'static str {
    match kind {
        ExporterKind::Otlp => "otlp",
        ExporterKind::Console => "console",
        ExporterKind::Prometheus => "prometheus",
        ExporterKind::None => "none",
        ExporterKind::Other(_) => "other",
    }
}

fn duration_ms(ms: i64) -> Duration {
    Duration::from_millis(ms.max(0) as u64)
}

fn duration_ms_opt(ms: Option<i64>) -> Duration {
    duration_ms(ms.unwrap_or(0))
}

fn run_with_timeout(
    label: &'static str,
    timeout: Duration,
    f: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "{label} timed out after {} ms",
            timeout.as_millis()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(format!("{label} worker disconnected")),
    }
}

fn emit_diag(error: bool, message: &str, stderr_gate: bool) {
    if error {
        tracing::warn!(target: "telemetry::otel", "{message}");
    } else {
        tracing::debug!(target: "telemetry::otel", "{message}");
    }
    if stderr_gate {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::AnalyticsBus;
    use opentelemetry::trace::{
        Span, SpanContext, SpanId, Status, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use tokio::runtime::Runtime;

    use crate::TEST_POLICY_LOCK as RUNTIME_SLOT_LOCK;

    struct TestSpan(SpanContext);

    impl Span for TestSpan {
        fn add_event<T>(&mut self, _name: T, _attributes: Vec<KeyValue>)
        where
            T: Into<std::borrow::Cow<'static, str>>,
        {
        }

        fn add_event_with_timestamp<T>(
            &mut self,
            _name: T,
            _timestamp: std::time::SystemTime,
            _attributes: Vec<KeyValue>,
        ) where
            T: Into<std::borrow::Cow<'static, str>>,
        {
        }

        fn add_link(&mut self, _span_context: SpanContext, _attributes: Vec<KeyValue>) {}

        fn span_context(&self) -> &SpanContext {
            &self.0
        }

        fn is_recording(&self) -> bool {
            false
        }

        fn set_attribute(&mut self, _attribute: KeyValue) {}

        fn set_status(&mut self, _status: Status) {}

        fn update_name<T>(&mut self, _new_name: T)
        where
            T: Into<std::borrow::Cow<'static, str>>,
        {
        }

        fn end(&mut self) {}

        fn end_with_timestamp(&mut self, _timestamp: std::time::SystemTime) {}
    }
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    fn enabled_lookup(port: u16) -> OtelConfig {
        OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            super::super::config::ENV_ENHANCED_TELEMETRY_BETA => Some("1".to_string()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(format!("http://127.0.0.1:{port}")),
            "OTEL_EXPORTER_OTLP_HEADERS" => Some("authorization=Bearer%20test".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("otlp".to_string()),
            "OTEL_LOGS_EXPORTER" => Some("otlp".to_string()),
            "OTEL_TRACES_EXPORTER" => Some("otlp".to_string()),
            "OTEL_LOG_ASSISTANT_RESPONSES" => Some("1".to_string()),
            "OTEL_SERVICE_NAME" => Some("telemetry-test".to_string()),
            _ => None,
        })
    }

    #[test]
    fn http_endpoint_appends_signal_to_generic_base_but_preserves_specific_endpoint() {
        let generic = OtelConfig::from_lookup(|key| match key {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("https://collector.example/otel/".into()),
            _ => None,
        });
        assert_eq!(
            normalize_http_endpoint(&generic.traces).unwrap().as_deref(),
            Some("https://collector.example/otel/v1/traces")
        );
        assert_eq!(
            normalize_http_endpoint(&generic.logs).unwrap().as_deref(),
            Some("https://collector.example/otel/v1/logs")
        );
        assert_eq!(
            normalize_http_endpoint(&generic.metrics)
                .unwrap()
                .as_deref(),
            Some("https://collector.example/otel/v1/metrics")
        );
        let specific = OtelConfig::from_lookup(|key| match key {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("https://collector.example/otel".into()),
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => {
                Some("https://collector.example/custom-traces".into())
            }
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT" => Some("https://collector.example/".into()),
            _ => None,
        });
        assert_eq!(
            normalize_http_endpoint(&specific.traces)
                .unwrap()
                .as_deref(),
            Some("https://collector.example/custom-traces")
        );
        assert_eq!(
            normalize_http_endpoint(&specific.logs).unwrap().as_deref(),
            Some("https://collector.example/")
        );
        assert_eq!(
            normalize_grpc_endpoint(&specific.metrics).as_deref(),
            Some("https://collector.example/otel")
        );
    }

    #[test]
    fn resource_instance_is_ephemeral_generated_and_never_a_metric_dimension() {
        let mut config = local_runtime_config();
        config.resource_attributes = Some(
            "service.instance.id=PRIVATE_USER_ID,user.id=PRIVATE_USER,service.version=1.2.3".into(),
        );
        let first = build_resource(&config);
        let second = build_resource(&config);
        let instance_key = opentelemetry::Key::from_static_str("service.instance.id");
        let first_id = first
            .get(&instance_key)
            .expect("generated instance id")
            .to_string();
        let second_id = second
            .get(&instance_key)
            .expect("generated instance id")
            .to_string();
        assert_ne!(first_id, second_id);
        assert_eq!(first_id.len(), 36);
        assert_eq!(&first_id[14..15], "4");
        assert!(matches!(first_id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        for index in [8, 13, 18, 23] {
            assert_eq!(first_id.as_bytes()[index], b'-');
        }
        assert!(!first_id.contains("PRIVATE"));
        assert!(first
            .get(&opentelemetry::Key::from_static_str("user.id"))
            .is_none());
        assert!(!resource_kvs(&first)
            .iter()
            .any(|attr| attr.key.as_str() == "service.instance.id"));
        assert!(filter_attributes(
            &attrs_from_pairs(&[("service.instance.id", AttrValue::from("PRIVATE_USER_ID"))]),
            true
        )
        .is_empty());
    }

    fn local_runtime_config() -> OtelConfig {
        OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("prometheus".to_string()),
            "OTEL_LOGS_EXPORTER" => Some("none".to_string()),
            "OTEL_TRACES_EXPORTER" => Some("none".to_string()),
            _ => None,
        })
    }

    fn local_runtime_with_logs(log_user_prompts: bool) -> OtelConfig {
        OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("none".to_string()),
            "OTEL_LOGS_EXPORTER" => Some("none".to_string()),
            "OTEL_TRACES_EXPORTER" => Some("none".to_string()),
            "OTEL_LOG_USER_PROMPTS" if log_user_prompts => Some("1".to_string()),
            _ => None,
        })
    }

    fn local_runtime_with_tool_details() -> OtelConfig {
        OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("none".to_string()),
            "OTEL_LOGS_EXPORTER" => Some("none".to_string()),
            "OTEL_TRACES_EXPORTER" => Some("none".to_string()),
            "OTEL_LOG_TOOL_DETAILS" => Some("1".to_string()),
            _ => None,
        })
    }

    fn install_test_runtime(config: OtelConfig) -> Arc<OtelRuntime> {
        let runtime = Arc::new(OtelRuntime::new(config, "test-entry", false).expect("runtime"));
        *runtime_slot().write().unwrap() = Some(runtime.clone());
        runtime
    }

    fn clear_runtime() {
        if let Some(runtime) = runtime_slot().write().unwrap().take() {
            runtime.shutdown();
        }
    }

    fn snapshot(runtime: &Arc<OtelRuntime>) -> DebugMirrorState {
        runtime
            .debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[test]
    fn capture_current_trace_context_serializes_w3c_headers() {
        let span_context = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace id"),
            SpanId::from_hex("00f067aa0ba902b7").expect("span id"),
            TraceFlags::SAMPLED,
            true,
            TraceState::from_str("foo=bar").expect("tracestate"),
        );
        let _guard = Context::current_with_span(TestSpan(span_context)).attach();

        let serialized = capture_current_trace_context().expect("serialized current context");
        assert_eq!(
            serialized.traceparent,
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        assert!(
            serialized.tracestate.is_none(),
            "vendor state is not exported"
        );
    }

    #[test]
    fn with_trace_context_restores_the_serialized_parent() {
        let restored = SerializedTraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: None,
        };

        let inside = with_trace_context(Some(&restored), capture_current_trace_context)
            .expect("restored current context");
        assert_eq!(inside, restored);
        assert!(capture_current_trace_context().is_none());
    }

    #[test]
    fn incoming_context_rejects_oversized_parent_and_discards_vendor_state() {
        let valid = SerializedTraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: Some("private=PRIVATE_PAYLOAD".into()),
        };
        let restored = with_trace_context(Some(&valid), capture_current_trace_context).unwrap();
        assert_eq!(restored.traceparent, valid.traceparent);
        assert!(restored.tracestate.is_none());
        let invalid = SerializedTraceContext {
            traceparent: "x".repeat(4096),
            tracestate: None,
        };
        assert!(with_trace_context(Some(&invalid), capture_current_trace_context).is_none());
    }

    #[tokio::test]
    async fn with_trace_context_future_restores_the_serialized_parent() {
        let restored = SerializedTraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: None,
        };

        let inside = with_trace_context_future(Some(&restored), async {
            tokio::task::yield_now().await;
            capture_current_trace_context()
        })
        .await
        .expect("restored current context");
        assert_eq!(inside, restored);
        assert!(capture_current_trace_context().is_none());
    }

    fn trace_console_runtime_config() -> OtelConfig {
        OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            super::super::config::ENV_ENHANCED_TELEMETRY_BETA => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("none".to_string()),
            "OTEL_LOGS_EXPORTER" => Some("none".to_string()),
            "OTEL_TRACES_EXPORTER" => Some("console".to_string()),
            _ => None,
        })
    }

    struct LargeFuture([u8; 256 * 1024]);

    impl std::future::Future for LargeFuture {
        type Output = u8;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<u8> {
            std::task::Poll::Ready(self.0[0])
        }
    }

    #[test]
    fn telemetry_wrappers_bound_large_future_size_including_disabled_path() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        clear_runtime();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let turn = with_turn_span("lingxi.large.turn", LargeFuture([7; 256 * 1024]));
        assert!(
            std::mem::size_of_val(&turn) < 1024,
            "turn wrapper retains large inline future"
        );
        assert_eq!(runtime.block_on(turn), 7);
        let operation = with_operation_span(
            "lingxi.large.operation",
            Attributes::new(),
            LargeFuture([8; 256 * 1024]),
        );
        assert!(std::mem::size_of_val(&operation) < 1024);
        assert_eq!(runtime.block_on(operation), 8);
        let context = with_trace_context_future(None, LargeFuture([9; 256 * 1024]));
        assert!(std::mem::size_of_val(&context) < 1024);
        assert_eq!(runtime.block_on(context), 9);
        let bound = bind_current_trace_context(LargeFuture([10; 256 * 1024]));
        assert!(std::mem::size_of_val(&bound) < 1024);
        assert_eq!(runtime.block_on(bound), 10);
    }

    #[tokio::test]
    async fn with_turn_span_keeps_trace_context_across_await() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        let _guard =
            install_process_with_config("turn-test", false, trace_console_runtime_config());

        let inside = with_turn_span("lingxi.turn.test", async {
            tokio::task::yield_now().await;
            capture_current_trace_context()
        })
        .await
        .expect("turn span context");

        assert!(
            inside.traceparent.starts_with("00-"),
            "expected a W3C traceparent, got {}",
            inside.traceparent
        );
        assert_eq!(inside.traceparent.len(), 55, "traceparent width");
        assert!(capture_current_trace_context().is_none());
    }

    fn spawn_http_collector() -> (
        u16,
        Arc<Mutex<Vec<CapturedRequest>>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test collector");
        let port = listener.local_addr().unwrap().port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking collector");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let shared = captured.clone();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for _ in 0..3 {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        result => panic!("collector did not receive three signals: {result:?}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut header_buf = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read_exact(&mut byte).is_ok() {
                    header_buf.push(byte[0]);
                    if header_buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let header_text = String::from_utf8(header_buf.clone()).expect("header utf8");
                let mut lines = header_text.split("\r\n");
                let first = lines.next().unwrap();
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut headers = HashMap::new();
                let mut content_length = 0usize;
                for line in lines {
                    if line.is_empty() {
                        continue;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        let value = value.trim().to_string();
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.parse().unwrap_or(0);
                        }
                        headers.insert(name.to_ascii_lowercase(), value);
                    }
                }
                let mut body = vec![0; content_length];
                stream.read_exact(&mut body).expect("body");
                shared.lock().unwrap().push(CapturedRequest {
                    path,
                    headers,
                    body,
                });
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .expect("response");
            }
        });
        (port, captured, handle)
    }

    #[test]
    fn http_runtime_exports_all_three_signals() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        let (port, captured, handle) = spawn_http_collector();
        let runtime = OtelRuntime::new(enabled_lookup(port), "test-entry", true).expect("runtime");

        runtime.record_counter(metrics::SESSION_COUNT, 1.0, &Attributes::new());
        runtime.emit_log_event(
            "assistant_response",
            &attrs_from_pairs(&[("body", AttrValue::from("ok"))]),
        );
        runtime.emit_span("test.span", vec![KeyValue::new("entrypoint", "test-entry")]);
        runtime.shutdown();

        handle.join().expect("collector join");

        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().any(|request| request.path == "/v1/metrics"));
        assert!(requests.iter().any(|request| request.path == "/v1/logs"));
        assert!(requests.iter().any(|request| request.path == "/v1/traces"));
        for request in requests {
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer test")
            );
            assert!(!request.body.is_empty());
        }
    }

    #[test]
    fn generic_base_path_and_encoded_auth_headers_reach_all_signal_endpoints() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        let (port, captured, collector) = spawn_http_collector();
        let mut config = enabled_lookup(port);
        for exporter in [&mut config.metrics, &mut config.logs, &mut config.traces] {
            exporter.endpoint = Some(format!("http://127.0.0.1:{port}/otel"));
        }
        let runtime = OtelRuntime::new(config, "path-test", true).unwrap();
        runtime.shutdown();
        collector.join().unwrap();
        let requests = captured.lock().unwrap();
        for signal in ["metrics", "logs", "traces"] {
            let request = requests
                .iter()
                .find(|request| request.path == format!("/otel/v1/{signal}"))
                .expect("signal path appended to base");
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer test")
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_install_flush_and_drop_are_safe_inside_current_thread_runtime() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        clear_runtime();
        let (port, captured, collector) = spawn_http_collector();
        // Exercises the synchronous fallback used by existing async consumers.
        // reqwest::blocking construction previously panicked here before a turn.
        let guard = install_process_with_config("async-sync-fallback", true, enabled_lookup(port));
        with_operation_span("lingxi.async.test", Attributes::new(), async {
            emit_named_log_event("async_event", &Attributes::new());
        })
        .await;
        guard.force_flush().unwrap();
        disable_process();
        drop(guard);
        collector.join().unwrap();
        assert_eq!(captured.lock().unwrap().len(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_http_lifecycle_exports_without_blocking_host_executor() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        clear_runtime();
        let (port, captured, collector) = spawn_http_collector();
        let guard =
            install_process_with_config_async("async-host", true, enabled_lookup(port)).await;
        assert!(
            guard.runtime.is_some(),
            "async initialization must not silently fail"
        );
        with_operation_span("lingxi.async.request", Attributes::new(), async {
            tokio::task::yield_now().await;
            emit_named_log_event("async_event", &Attributes::new());
        })
        .await;
        guard.force_flush_async().await.unwrap();
        disable_process_async().await;
        assert!(runtime_slot().read().unwrap().is_none());
        assert!(!guard.runtime.as_ref().unwrap().policy.allowed());
        guard.shutdown_async().await.unwrap();
        collector.join().unwrap();
        assert_eq!(captured.lock().unwrap().len(), 3);
    }

    #[test]
    fn wire_export_preserves_parent_log_correlation_and_ends_cancelled_spans_without_content() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        clear_runtime();
        let (port, captured, collector) = spawn_http_collector();
        let mut config = enabled_lookup(port);
        config.metrics.protocol = OtlpProtocol::HttpJson;
        config.logs.protocol = OtlpProtocol::HttpJson;
        config.traces.protocol = OtlpProtocol::HttpJson;
        config.resource_attributes = Some("user.id=PRIVATE_RESOURCE,service.version=1.2.3".into());
        let guard = install_process_with_config("wire-test", true, config);
        let attributes = attrs_from_pairs(&[
            ("status", AttrValue::from("success")),
            ("prompt", AttrValue::from("PRIVATE_PROMPT")),
            ("tool_parameters", AttrValue::from("PRIVATE_TOOL")),
            ("session_id", AttrValue::from("PRIVATE_SESSION")),
            ("path", AttrValue::from("PRIVATE_PATH")),
        ]);
        Runtime::new().unwrap().block_on(with_operation_span(
            "lingxi.test.root",
            attributes.clone(),
            async {
                with_operation_span("lingxi.test.child", attributes.clone(), async {
                    emit_named_log_event("test_correlated_event", &attributes);
                    record_counter(metrics::SESSION_COUNT, 1.0, &attributes);
                })
                .await;
                let cancelled = tokio::time::timeout(
                    Duration::from_millis(1),
                    with_operation_span(
                        "lingxi.test.cancelled",
                        Attributes::new(),
                        std::future::pending::<()>(),
                    ),
                )
                .await;
                assert!(cancelled.is_err());
            },
        ));
        drop(guard);
        collector.join().unwrap();
        let requests = captured.lock().unwrap();
        for request in requests.iter() {
            let body = String::from_utf8_lossy(&request.body);
            assert!(
                !body.contains("PRIVATE_"),
                "private field escaped on {}",
                request.path
            );
        }
        let trace: serde_json::Value = serde_json::from_slice(
            &requests
                .iter()
                .find(|r| r.path == "/v1/traces")
                .unwrap()
                .body,
        )
        .unwrap();
        let spans: Vec<&serde_json::Value> = trace["resourceSpans"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|resource| resource["scopeSpans"].as_array().unwrap())
            .flat_map(|scope| scope["spans"].as_array().unwrap())
            .collect();
        let root = spans
            .iter()
            .find(|span| span["name"] == "lingxi.test.root")
            .unwrap();
        let child = spans
            .iter()
            .find(|span| span["name"] == "lingxi.test.child")
            .unwrap();
        let cancelled = spans
            .iter()
            .find(|span| span["name"] == "lingxi.test.cancelled")
            .unwrap();
        assert_eq!(root["traceId"], child["traceId"]);
        assert_eq!(root["spanId"], child["parentSpanId"]);
        assert_eq!(root["spanId"], cancelled["parentSpanId"]);
        assert!(cancelled["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|attr| attr["key"] == "outcome" && attr["value"]["stringValue"] == "cancelled"));
        let logs: serde_json::Value =
            serde_json::from_slice(&requests.iter().find(|r| r.path == "/v1/logs").unwrap().body)
                .unwrap();
        let correlated = logs["resourceLogs"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|resource| resource["scopeLogs"].as_array().unwrap())
            .flat_map(|scope| scope["logRecords"].as_array().unwrap())
            .find(|log| log["body"]["stringValue"] == "test_correlated_event")
            .unwrap();
        assert_eq!(correlated["traceId"], child["traceId"]);
        assert_eq!(correlated["spanId"], child["spanId"]);
    }

    #[test]
    fn disabled_config_never_constructs_providers_or_connects() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        clear_runtime();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind collector");
        listener.set_nonblocking(true).unwrap();
        let mut config = enabled_lookup(listener.local_addr().unwrap().port());
        config.enabled = false;
        let guard = install_process_with_config("disabled", true, config);
        record_counter(metrics::SESSION_COUNT, 1.0, &Attributes::new());
        emit_named_log_event("disabled_event", &Attributes::new());
        guard.force_flush().unwrap();
        drop(guard);
        assert!(runtime_slot().read().unwrap().is_none());
        assert!(
            matches!(listener.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn normal_enabled_telemetry_includes_traces_without_legacy_beta_gate() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        let config = OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("prometheus".to_string()),
            "OTEL_LOGS_EXPORTER" | "OTEL_TRACES_EXPORTER" => Some("console".to_string()),
            _ => None,
        });
        assert!(!config.enhanced_telemetry);
        let runtime = OtelRuntime::new(config, "test-entry", false).expect("runtime");
        assert!(runtime.metrics.is_some());
        assert!(runtime.logs.is_some());
        assert!(runtime.traces.is_some());
        runtime.shutdown();
    }

    #[test]
    fn revoke_discards_queued_otlp_batches_before_shutdown() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        clear_runtime();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind collector");
        listener.set_nonblocking(true).unwrap();
        let guard = install_process_with_config(
            "revoked",
            true,
            enabled_lookup(listener.local_addr().unwrap().port()),
        );
        emit_named_log_event("queued_event", &Attributes::new());
        disable_process();
        guard.force_flush().unwrap();
        drop(guard);
        assert!(runtime_slot().read().unwrap().is_none());
        assert!(
            matches!(listener.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn managed_generic_endpoint_keeps_all_signals_on_managed_collector() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        let (port, captured, handle) = spawn_http_collector();
        let config = OtelConfig::from_lookup_with_source(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some(super::super::config::ConfigValue {
                value: "1".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            super::super::config::ENV_ENHANCED_TELEMETRY_BETA => {
                Some(super::super::config::ConfigValue {
                    value: "1".to_string(),
                    source: super::super::config::ConfigValueSource::Runtime,
                })
            }
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(super::super::config::ConfigValue {
                value: format!("http://127.0.0.1:{port}"),
                source: super::super::config::ConfigValueSource::Managed,
            }),
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => Some(super::super::config::ConfigValue {
                value: "http://127.0.0.1:1".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT" => Some(super::super::config::ConfigValue {
                value: "http://127.0.0.1:1".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT" => Some(super::super::config::ConfigValue {
                value: "http://127.0.0.1:1".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            "OTEL_METRICS_EXPORTER" => Some(super::super::config::ConfigValue {
                value: "otlp".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            "OTEL_LOGS_EXPORTER" => Some(super::super::config::ConfigValue {
                value: "otlp".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            "OTEL_TRACES_EXPORTER" => Some(super::super::config::ConfigValue {
                value: "otlp".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            "OTEL_LOG_ASSISTANT_RESPONSES" => Some(super::super::config::ConfigValue {
                value: "1".to_string(),
                source: super::super::config::ConfigValueSource::Runtime,
            }),
            _ => None,
        });
        let runtime = OtelRuntime::new(config, "test-entry", true).expect("runtime");
        runtime.record_counter(metrics::SESSION_COUNT, 1.0, &Attributes::new());
        runtime.emit_log_event(
            "assistant_response",
            &attrs_from_pairs(&[("body", AttrValue::from("ok"))]),
        );
        runtime.emit_span("test.span", vec![KeyValue::new("entrypoint", "test-entry")]);
        runtime.shutdown();

        handle.join().expect("collector join");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().any(|request| request.path == "/v1/metrics"));
        assert!(requests.iter().any(|request| request.path == "/v1/logs"));
        assert!(requests.iter().any(|request| request.path == "/v1/traces"));
    }

    #[test]
    fn prometheus_exporter_renders_metrics_text() {
        let _lock = RUNTIME_SLOT_LOCK.lock().unwrap();
        let config = OtelConfig::from_lookup(|key| match key {
            super::super::config::ENV_ENABLE_TELEMETRY => Some("1".to_string()),
            "OTEL_METRICS_EXPORTER" => Some("prometheus".to_string()),
            _ => None,
        });
        let runtime = OtelRuntime::new(config, "prom", true).expect("runtime");
        runtime.record_counter(metrics::SESSION_COUNT, 1.0, &Attributes::new());
        let text = {
            let registry = runtime
                .metrics
                .as_ref()
                .and_then(|metrics| metrics.prometheus_registry.as_ref())
                .expect("registry");
            let encoder = TextEncoder::new();
            let mut bytes = Vec::new();
            encoder.encode(&registry.gather(), &mut bytes).unwrap();
            String::from_utf8(bytes).unwrap()
        };
        assert!(text.contains("claude_code_session_count_total"));
        runtime.shutdown();
    }

    #[test]
    fn bus_api_success_maps_metrics_and_log() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());
        let bus = AnalyticsBus::new();
        let task = async {
            let mut md = LogEventMetadata::new();
            md.insert(
                "model".into(),
                AnalyticsValue::String("claude-opus-4-6".into()),
            );
            md.insert(
                "provider".into(),
                AnalyticsValue::String("anthropic".into()),
            );
            md.insert(
                "querySource".into(),
                AnalyticsValue::String("repl_main_thread".into()),
            );
            md.insert("inputTokens".into(), AnalyticsValue::Int(1000));
            md.insert("outputTokens".into(), AnalyticsValue::Int(500));
            md.insert("cachedInputTokens".into(), AnalyticsValue::Int(128));
            md.insert("uncachedInputTokens".into(), AnalyticsValue::Int(64));
            md.insert("costUSD".into(), AnalyticsValue::Float(0.0175));
            md.insert("durationMs".into(), AnalyticsValue::Int(250));
            bus.log_event("tengu_api_success", md).await;
        };
        Runtime::new().unwrap().block_on(task);

        let debug = snapshot(&runtime);
        assert!(debug.counters.iter().any(|sample| {
            sample.instrument == metrics::TOKEN_USAGE
                && sample.attributes.get("token_type") == Some(&AttrValue::from("input"))
                && (sample.value - 1000.0).abs() < f64::EPSILON
        }));
        assert!(debug.counters.iter().any(|sample| {
            sample.instrument == metrics::COST_USAGE && (sample.value - 0.0175).abs() < 1e-12
        }));
        assert!(debug.counters.iter().any(|sample| {
            sample.instrument == metrics::LLM_REQUEST
                && sample.attributes.get("status") == Some(&AttrValue::from("success"))
        }));
        assert!(debug.histograms.iter().any(|sample| {
            sample.instrument == metrics::LLM_REQUEST && (sample.value - 250.0).abs() < f64::EPSILON
        }));
        assert!(debug
            .logs
            .iter()
            .any(|sample| sample.event_name == "tengu_api_success"));
        clear_runtime();
    }

    #[test]
    fn bus_tool_and_blocked_mappings_emit_metrics() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());
        let bus = AnalyticsBus::new();
        let task = async {
            let mut blocked = LogEventMetadata::new();
            blocked.insert("tool_name".into(), AnalyticsValue::String("Bash".into()));
            bus.log_event("tengu_tool_permission_requested", blocked)
                .await;

            let mut bash = LogEventMetadata::new();
            bash.insert("duration_ms".into(), AnalyticsValue::Int(42));
            bus.log_event("tengu_tool_bash_completed", bash).await;

            let mut mcp = LogEventMetadata::new();
            mcp.insert("duration_ms".into(), AnalyticsValue::Int(77));
            bus.log_event("tengu_tool_mcp_completed", mcp).await;

            bus.log_event("tengu_tool_task_dispatched", LogEventMetadata::new())
                .await;
        };
        Runtime::new().unwrap().block_on(task);

        let debug = snapshot(&runtime);
        assert!(debug
            .counters
            .iter()
            .any(|sample| { sample.instrument == metrics::TOOL_BLOCKED_ON_USER }));
        assert!(debug.counters.iter().any(|sample| {
            sample.instrument == metrics::BASH_SUBPROCESS
                && sample.attributes.get("status") == Some(&AttrValue::from("completed"))
        }));
        assert!(debug.histograms.iter().any(|sample| {
            sample.instrument == metrics::MCP_RPC && (sample.value - 77.0).abs() < f64::EPSILON
        }));
        assert!(debug
            .counters
            .iter()
            .any(|sample| sample.instrument == metrics::SUBAGENT_SPAWN
                && sample.attributes.get("tool_name") == Some(&AttrValue::from("Agent"))));
        clear_runtime();
    }

    #[test]
    fn proto_prompt_logging_is_opt_in_and_redacted() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let bus = AnalyticsBus::new();

        let runtime_off = install_test_runtime(local_runtime_with_logs(false));
        let task = async {
            let mut md = LogEventMetadata::new();
            md.insert(
                "_PROTO_prompt".into(),
                AnalyticsValue::String("Bearer secret sk-live-token".into()),
            );
            bus.log_event("tengu_tool_schedule_cron_completed", md)
                .await;
        };
        Runtime::new().unwrap().block_on(task);
        let debug_off = snapshot(&runtime_off);
        assert!(debug_off
            .logs
            .iter()
            .all(|sample| !sample.attributes.contains_key("prompt")));
        clear_runtime();

        let runtime_on = install_test_runtime(local_runtime_with_logs(true));
        Runtime::new().unwrap().block_on(async {
            let mut md = LogEventMetadata::new();
            md.insert(
                "_PROTO_prompt".into(),
                AnalyticsValue::String("Bearer secret sk-live-token".into()),
            );
            bus.log_event("tengu_tool_schedule_cron_completed", md)
                .await;
        });
        let debug_on = snapshot(&runtime_on);
        let prompt = debug_on
            .logs
            .iter()
            .find(|sample| sample.event_name == "tengu_tool_schedule_cron_completed")
            .and_then(|sample| sample.attributes.get("prompt"))
            .and_then(attr_str)
            .expect("prompt log");
        assert!(prompt.contains("[REDACTED]"));
        clear_runtime();
    }

    #[test]
    fn user_prompt_record_is_always_present_but_body_is_opt_in() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();

        let runtime_off = install_test_runtime(local_runtime_with_logs(false));
        emit_user_prompt_log("hello sk-secret", "prompt-1", "message-1");
        let off = snapshot(&runtime_off);
        let record = off
            .logs
            .iter()
            .find(|sample| sample.event_name == "user_prompt")
            .expect("record emitted with body disabled");
        assert_eq!(
            record.attributes.get("prompt"),
            Some(&AttrValue::from("<REDACTED>"))
        );
        assert_eq!(
            record.attributes.get("prompt_text"),
            record.attributes.get("prompt")
        );
        assert_eq!(
            record.attributes.get("prompt.id"),
            Some(&AttrValue::from("prompt-1"))
        );
        clear_runtime();

        let runtime_on = install_test_runtime(local_runtime_with_logs(true));
        emit_user_prompt_log("hello sk-secret", "prompt-2", "message-2");
        let on = snapshot(&runtime_on);
        let record = on
            .logs
            .iter()
            .find(|sample| sample.event_name == "user_prompt")
            .expect("opt-in record");
        assert_eq!(
            record.attributes.get("prompt"),
            Some(&AttrValue::from("hello [REDACTED]"))
        );
        assert_eq!(
            record.attributes.get("prompt_text"),
            record.attributes.get("prompt")
        );
        clear_runtime();
    }

    #[test]
    fn redaction_covers_hyphenated_and_dotted_api_key_names() {
        for key in ["x-api-key", "provider.api.key", "API_KEY"] {
            assert_eq!(
                redact_secret_value(key, "plain-secret-value"),
                "[REDACTED]",
                "secret-bearing key {key} must be redacted by name"
            );
        }
    }

    #[test]
    fn bus_api_success_translates_to_api_request_log() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());
        let bus = AnalyticsBus::new();
        Runtime::new().unwrap().block_on(async {
            let mut md = LogEventMetadata::new();
            md.insert(
                "model".into(),
                AnalyticsValue::String("claude-opus-4-6".into()),
            );
            md.insert("inputTokens".into(), AnalyticsValue::Int(1000));
            md.insert("outputTokens".into(), AnalyticsValue::Int(500));
            md.insert("cachedInputTokens".into(), AnalyticsValue::Int(128));
            md.insert("uncachedInputTokens".into(), AnalyticsValue::Int(64));
            md.insert("costUSD".into(), AnalyticsValue::Float(0.0175));
            md.insert("durationMs".into(), AnalyticsValue::Int(250));
            md.insert("requestId".into(), AnalyticsValue::String("req-1".into()));
            md.insert("querySource".into(), AnalyticsValue::String("sdk".into()));
            md.insert("fastMode".into(), AnalyticsValue::Bool(false));
            bus.log_event("tengu_api_success", md).await;
        });
        let debug = snapshot(&runtime);
        let api_request = debug
            .logs
            .iter()
            .find(|sample| sample.event_name == "api_request")
            .expect("api_request log record");
        assert_eq!(
            api_request.attributes.get("input_tokens"),
            Some(&AttrValue::Int(1000))
        );
        assert_eq!(
            api_request.attributes.get("cache_read_tokens"),
            Some(&AttrValue::Int(128))
        );
        assert_eq!(
            api_request.attributes.get("cost_usd_micros"),
            Some(&AttrValue::Int(17500))
        );
        assert_eq!(
            api_request.attributes.get("speed"),
            Some(&AttrValue::from("normal"))
        );
        assert_eq!(
            api_request.attributes.get("query_source"),
            Some(&AttrValue::from("sdk"))
        );
        // The tengu passthrough record is still emitted alongside.
        assert!(debug
            .logs
            .iter()
            .any(|sample| sample.event_name == "tengu_api_success"));
        clear_runtime();
    }

    #[test]
    fn bus_api_failure_translates_to_api_error_log() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());
        let bus = AnalyticsBus::new();
        Runtime::new().unwrap().block_on(async {
            let mut md = LogEventMetadata::new();
            md.insert("model".into(), AnalyticsValue::String("m".into()));
            md.insert(
                "error_kind".into(),
                AnalyticsValue::String("overloaded".into()),
            );
            md.insert("status_code".into(), AnalyticsValue::Int(529));
            bus.log_event("tengu_api_request_failed", md).await;
        });
        let debug = snapshot(&runtime);
        let api_error = debug
            .logs
            .iter()
            .find(|sample| sample.event_name == "api_error")
            .expect("api_error log record");
        assert_eq!(
            api_error.attributes.get("error"),
            Some(&AttrValue::from("overloaded"))
        );
        assert_eq!(
            api_error.attributes.get("status_code"),
            Some(&AttrValue::Int(529))
        );
        clear_runtime();
    }

    #[test]
    fn bus_tool_completion_translates_to_tool_result_log() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());
        let bus = AnalyticsBus::new();
        Runtime::new().unwrap().block_on(async {
            let mut md = LogEventMetadata::new();
            md.insert("duration_ms".into(), AnalyticsValue::Int(42));
            bus.log_event("tengu_tool_bash_completed", md).await;
            bus.log_event("tengu_tool_write_failed", LogEventMetadata::new())
                .await;
            // Non-terminal lifecycle events must NOT synthesize a tool_result.
            bus.log_event("tengu_tool_bash_started", LogEventMetadata::new())
                .await;
        });
        let debug = snapshot(&runtime);
        let results: Vec<_> = debug
            .logs
            .iter()
            .filter(|sample| sample.event_name == "tool_result")
            .collect();
        assert_eq!(results.len(), 2);
        // CC stringifies success/duration_ms in the log record.
        assert!(results.iter().any(|sample| {
            sample.attributes.get("tool_name") == Some(&AttrValue::from("Bash"))
                && sample.attributes.get("success") == Some(&AttrValue::from("true"))
                && sample.attributes.get("duration_ms") == Some(&AttrValue::from("42"))
        }));
        assert!(results.iter().any(|sample| {
            sample.attributes.get("tool_name") == Some(&AttrValue::from("Write"))
                && sample.attributes.get("success") == Some(&AttrValue::from("false"))
        }));
        clear_runtime();
    }

    #[test]
    fn tool_permission_decision_records_counter_and_log() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());

        record_tool_permission_decision(
            "Edit",
            "toolu_1",
            Some("/tmp/main.rs"),
            "accept",
            "config",
            false,
            None,
        );
        record_tool_permission_decision(
            "Bash",
            "toolu_2",
            None,
            "reject",
            "user_reject",
            false,
            None,
        );
        record_tool_permission_decision(
            "mcp__srv__do",
            "toolu_3",
            None,
            "accept",
            "config",
            true,
            None,
        );

        let debug = snapshot(&runtime);
        // Counter fires only for the code-edit tools (`pI_`), with language.
        let decisions: Vec<_> = debug
            .counters
            .iter()
            .filter(|sample| sample.instrument == metrics::CODE_EDIT_TOOL_DECISION)
            .collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].attributes.get("decision"),
            Some(&AttrValue::from("accept"))
        );
        assert_eq!(
            decisions[0].attributes.get("source"),
            Some(&AttrValue::from("config"))
        );
        assert_eq!(
            decisions[0].attributes.get("language"),
            Some(&AttrValue::from("Rust"))
        );
        // The tool_decision log record fires for EVERY tool.
        let logs: Vec<_> = debug
            .logs
            .iter()
            .filter(|sample| sample.event_name == "tool_decision")
            .collect();
        assert_eq!(logs.len(), 3);
        assert!(logs.iter().any(|sample| {
            sample.attributes.get("tool_use_id") == Some(&AttrValue::from("toolu_2"))
                && sample.attributes.get("decision") == Some(&AttrValue::from("reject"))
        }));
        // `...Jro(n.mcpInfo)` is spread unconditionally — builtin tools carry
        // `tool_source: "builtin"`, MCP tools `"mcp"`.
        let builtin = logs
            .iter()
            .find(|s| s.attributes.get("tool_use_id") == Some(&AttrValue::from("toolu_1")))
            .expect("Edit record");
        assert_eq!(
            builtin.attributes.get("tool_source"),
            Some(&AttrValue::from("builtin"))
        );
        assert_eq!(
            builtin.attributes.get("tool_name"),
            Some(&AttrValue::from("Edit"))
        );
        // `ua()` collapses every `mcp__*` name to the literal "mcp_tool" in the
        // LOG record, so no server/tool name reaches the collector.
        let mcp = logs
            .iter()
            .find(|s| s.attributes.get("tool_use_id") == Some(&AttrValue::from("toolu_3")))
            .expect("MCP record");
        assert_eq!(
            mcp.attributes.get("tool_name"),
            Some(&AttrValue::from("mcp_tool"))
        );
        assert_eq!(
            mcp.attributes.get("tool_source"),
            Some(&AttrValue::from("mcp"))
        );
        clear_runtime();
    }

    #[test]
    fn tool_permission_parameters_are_opt_in_bounded_and_redacted() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_with_tool_details());

        let input_json = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer sk-secret' https://example.invalid",
            "description": "safe summary",
            "timeout": 10_000
        });
        let input = Utf16JsonProjection::from_serde_value(&input_json).unwrap();
        record_tool_permission_decision(
            "Bash",
            "toolu_details",
            None,
            "accept",
            "user_temporary",
            false,
            Some(&input),
        );

        let debug = snapshot(&runtime);
        let parameters = debug
            .logs
            .iter()
            .find(|sample| sample.event_name == "tool_decision")
            .and_then(|sample| sample.attributes.get("tool_parameters"))
            .expect("tool details opt-in attaches bounded parameters");
        let AttrValue::Str(parameters) = parameters else {
            panic!("tool_parameters must be a string");
        };
        assert!(parameters.contains("[REDACTED]"));
        assert!(!parameters.contains("sk-secret"));
        assert!(parameters.contains("safe summary"));
        clear_runtime();
    }

    #[test]
    fn code_edit_decision_counter_keeps_the_raw_tool_name() {
        // `ICs` returns `tool_name: e.name` RAW — only the log record is
        // anonymized, so the counter must not be collapsed to "mcp_tool".
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());

        record_tool_permission_decision("Edit", "toolu_1", None, "accept", "config", false, None);

        let debug = snapshot(&runtime);
        let decisions: Vec<_> = debug
            .counters
            .iter()
            .filter(|sample| sample.instrument == metrics::CODE_EDIT_TOOL_DECISION)
            .collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].attributes.get("tool_name"),
            Some(&AttrValue::from("Edit"))
        );
        assert_eq!(decisions[0].attributes.get("tool_source"), None);
        clear_runtime();
    }

    #[test]
    fn git_operation_counters_gate_on_exit_code() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let runtime = install_test_runtime(local_runtime_config());

        record_git_operation_counters("git commit -m x && gh pr create", 0);
        record_git_operation_counters("git commit -m y", 1); // non-zero exit: no-op

        let debug = snapshot(&runtime);
        let commit = debug
            .counters
            .iter()
            .filter(|sample| sample.instrument == metrics::COMMIT_COUNT)
            .fold(0.0, |acc, sample| acc + sample.value);
        let pr = debug
            .counters
            .iter()
            .filter(|sample| sample.instrument == metrics::PULL_REQUEST_COUNT)
            .fold(0.0, |acc, sample| acc + sample.value);
        assert!((commit - 1.0).abs() < f64::EPSILON);
        assert!((pr - 1.0).abs() < f64::EPSILON);
        clear_runtime();
    }

    #[test]
    fn bus_without_runtime_records_nothing() {
        let _lock = RUNTIME_SLOT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_runtime();
        let bus = AnalyticsBus::new();
        Runtime::new().unwrap().block_on(async {
            bus.log_event("tengu_api_success", LogEventMetadata::new())
                .await;
        });
        assert!(runtime_slot().read().unwrap().is_none());
    }
}
