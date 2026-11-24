//! Console exporters behind `OTEL_{METRICS,LOGS,TRACES}_EXPORTER=console`.
//!
//! Like claude-code's console exporters, each export writes a human-readable
//! block to stdout. The layout is for people reading a terminal, not a wire
//! format: one header line per batch, the resource attributes on the first
//! batch, then one line per record with indented data points or attributes.
//! Timestamps are RFC 3339 UTC with microseconds.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use opentelemetry::{KeyValue, SpanId};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;

fn time(t: SystemTime) -> String {
    jiff::Timestamp::try_from(t).map_or_else(|_| format!("{t:?}"), |ts| format!("{ts:.6}"))
}

fn key_values<'a>(attrs: impl IntoIterator<Item = &'a KeyValue>) -> String {
    let mut out = String::new();
    for kv in attrs {
        let _ = write!(out, " {}={}", kv.key, kv.value);
    }
    out
}

fn write_resource(out: &mut String, resource: &Resource) {
    if let Some(schema_url) = resource.schema_url() {
        let _ = writeln!(out, "  resource schema_url={schema_url}");
    }
    for (key, value) in resource.iter() {
        let _ = writeln!(out, "  resource {key}={value}");
    }
}

fn emit(block: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(block.as_bytes());
    let _ = stdout.flush();
}

/// Shutdown flag shared by the three exporters.
#[derive(Debug, Default)]
struct State {
    shut_down: AtomicBool,
    resource_emitted: AtomicBool,
}

impl State {
    fn check(&self) -> OTelSdkResult {
        if self.shut_down.load(Ordering::SeqCst) {
            Err(OTelSdkError::AlreadyShutdown)
        } else {
            Ok(())
        }
    }

    /// True exactly once: on the first export after construction.
    fn first_export(&self) -> bool {
        !self.resource_emitted.swap(true, Ordering::SeqCst)
    }

    fn shut_down(&self) -> OTelSdkResult {
        self.shut_down.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Console metrics exporter (cumulative temporality, the SDK default).
#[derive(Debug, Default)]
pub(crate) struct ConsoleMetricExporter {
    state: State,
}

fn write_metric_data<T: std::fmt::Debug + Copy>(out: &mut String, data: &MetricData<T>) {
    match data {
        MetricData::Gauge(gauge) => {
            let _ = writeln!(out, "    gauge at {}", time(gauge.time()));
            for point in gauge.data_points() {
                let _ = writeln!(
                    out,
                    "      {:?}{}",
                    point.value(),
                    key_values(point.attributes())
                );
            }
        }
        MetricData::Sum(sum) => {
            let _ = writeln!(
                out,
                "    sum {:?} monotonic={} {} .. {}",
                sum.temporality(),
                sum.is_monotonic(),
                time(sum.start_time()),
                time(sum.time()),
            );
            for point in sum.data_points() {
                let _ = writeln!(
                    out,
                    "      {:?}{}",
                    point.value(),
                    key_values(point.attributes())
                );
            }
        }
        MetricData::Histogram(histogram) => {
            let _ = writeln!(
                out,
                "    histogram {:?} {} .. {}",
                histogram.temporality(),
                time(histogram.start_time()),
                time(histogram.time()),
            );
            for point in histogram.data_points() {
                let _ = write!(out, "      count={} sum={:?}", point.count(), point.sum());
                if let Some(min) = point.min() {
                    let _ = write!(out, " min={min:?}");
                }
                if let Some(max) = point.max() {
                    let _ = write!(out, " max={max:?}");
                }
                let _ = writeln!(out, "{}", key_values(point.attributes()));
                let bounds: Vec<f64> = point.bounds().collect();
                let counts: Vec<u64> = point.bucket_counts().collect();
                if !bounds.is_empty() {
                    let _ = writeln!(out, "        bounds={bounds:?} counts={counts:?}");
                }
            }
        }
        MetricData::ExponentialHistogram(histogram) => {
            let _ = writeln!(
                out,
                "    exponential histogram {:?} {} .. {}",
                histogram.temporality(),
                time(histogram.start_time()),
                time(histogram.time()),
            );
        }
    }
}

impl PushMetricExporter for ConsoleMetricExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        self.state.check()?;
        let mut out = String::from("otel metrics\n");
        write_resource(&mut out, metrics.resource());
        for scope_metrics in metrics.scope_metrics() {
            let scope = scope_metrics.scope().name();
            for metric in scope_metrics.metrics() {
                let _ = writeln!(
                    out,
                    "  {} [{}] scope={scope} {}",
                    metric.name(),
                    metric.unit(),
                    metric.description(),
                );
                match metric.data() {
                    AggregatedMetrics::F64(data) => write_metric_data(&mut out, data),
                    AggregatedMetrics::U64(data) => write_metric_data(&mut out, data),
                    AggregatedMetrics::I64(data) => write_metric_data(&mut out, data),
                }
            }
        }
        emit(&out);
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        self.state.shut_down()
    }

    fn temporality(&self) -> Temporality {
        Temporality::default()
    }
}

/// Console log-record exporter.
#[derive(Debug)]
pub(crate) struct ConsoleLogExporter {
    state: State,
    resource: Resource,
}

impl Default for ConsoleLogExporter {
    fn default() -> Self {
        Self {
            state: State::default(),
            resource: Resource::builder_empty().build(),
        }
    }
}

impl LogExporter for ConsoleLogExporter {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        self.state.check()?;
        let mut out = String::from("otel logs\n");
        if self.state.first_export() {
            write_resource(&mut out, &self.resource);
        }
        for (record, scope) in batch.iter() {
            let at = record.timestamp().or(record.observed_timestamp()).map(time);
            let _ = write!(out, "  {}", at.as_deref().unwrap_or("-"));
            if let Some(severity) = record.severity_text() {
                let _ = write!(out, " {severity}");
            }
            if let Some(name) = record.event_name() {
                let _ = write!(out, " {name}");
            }
            let _ = write!(out, " scope={}", scope.name());
            if let Some(context) = record.trace_context() {
                let _ = write!(out, " trace={} span={}", context.trace_id, context.span_id);
            }
            if let Some(body) = record.body() {
                let _ = write!(out, " body={body:?}");
            }
            let _ = writeln!(out);
            for (key, value) in record.attributes_iter() {
                let _ = writeln!(out, "    {key}={value:?}");
            }
        }
        emit(&out);
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        self.state.shut_down()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.clone();
    }
}

/// Console span exporter.
#[derive(Debug)]
pub(crate) struct ConsoleSpanExporter {
    state: State,
    resource: Resource,
}

impl Default for ConsoleSpanExporter {
    fn default() -> Self {
        Self {
            state: State::default(),
            resource: Resource::builder_empty().build(),
        }
    }
}

impl SpanExporter for ConsoleSpanExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.state.check()?;
        let mut out = String::from("otel spans\n");
        if self.state.first_export() {
            write_resource(&mut out, &self.resource);
        }
        for span in batch {
            let parent = if span.parent_span_id == SpanId::INVALID {
                "root".to_owned()
            } else {
                span.parent_span_id.to_string()
            };
            let _ = writeln!(
                out,
                "  {} kind={:?} scope={} trace={} span={} parent={parent} {} .. {} status={:?}{}",
                span.name,
                span.span_kind,
                span.instrumentation_scope.name(),
                span.span_context.trace_id(),
                span.span_context.span_id(),
                time(span.start_time),
                time(span.end_time),
                span.status,
                key_values(&span.attributes),
            );
            for event in span.events.iter() {
                let _ = writeln!(
                    out,
                    "    event {} at {}{}",
                    event.name,
                    time(event.timestamp),
                    key_values(&event.attributes),
                );
            }
            for link in span.links.iter() {
                let _ = writeln!(
                    out,
                    "    link trace={} span={}{}",
                    link.span_context.trace_id(),
                    link.span_context.span_id(),
                    key_values(&link.attributes),
                );
            }
        }
        emit(&out);
        Ok(())
    }

    fn shutdown_with_timeout(&mut self, _timeout: Duration) -> OTelSdkResult {
        self.state.shut_down()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_rfc3339_utc_with_microseconds() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_micros(1_700_000_000_123_456);
        assert_eq!(time(t), "2023-11-14T22:13:20.123456Z");
    }

    #[test]
    fn key_values_are_space_prefixed_pairs() {
        let attrs = [KeyValue::new("a", 1_i64), KeyValue::new("b", "x")];
        assert_eq!(key_values(&attrs), " a=1 b=x");
    }

    #[test]
    fn exporters_refuse_after_shutdown() {
        let state = State::default();
        assert!(state.check().is_ok());
        assert!(state.first_export());
        assert!(!state.first_export());
        state.shut_down().unwrap();
        assert!(matches!(state.check(), Err(OTelSdkError::AlreadyShutdown)));
    }
}
