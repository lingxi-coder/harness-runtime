//! Check revocation at the last boundary, including batches queued before opt-out.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::{
    data::ResourceMetrics, exporter::PushMetricExporter, Temporality,
};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;

use super::policy::ExportPolicy;

#[derive(Debug)]
pub(crate) struct GatedExporter<E> {
    inner: E,
    policy: Arc<ExportPolicy>,
}

impl<E> GatedExporter<E> {
    pub(crate) fn new(inner: E, policy: &Arc<ExportPolicy>) -> Self {
        Self {
            inner,
            policy: policy.clone(),
        }
    }
}

impl<E: PushMetricExporter> PushMetricExporter for GatedExporter<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        if !self.policy.allowed() {
            return Ok(());
        }
        self.inner.export(metrics).await
    }

    fn force_flush(&self) -> OTelSdkResult {
        if !self.policy.allowed() {
            return Ok(());
        }
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

impl<E: LogExporter> LogExporter for GatedExporter<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        if !self.policy.allowed() {
            return Ok(());
        }
        self.inner.export(batch).await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: SpanExporter> SpanExporter for GatedExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        if !self.policy.allowed() {
            return Ok(());
        }
        self.inner.export(batch).await
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&mut self) -> OTelSdkResult {
        if !self.policy.allowed() {
            return Ok(());
        }
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}
