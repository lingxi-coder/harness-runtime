//! Adapter that bridges [`lingxi_core::host::OutputStream`] emissions from
//! the orchestrator into the CLI's [`crate::headless::output::OutputSink`].
//!
//! Wired in M5-12 Task 9: when the orchestrator emits a `Text` content
//! block, a tool-call, or an end-of-turn cost snapshot, the adapter
//! forwards the event through the configured sink. In JSON mode that
//! produces an NDJSON line; in plain mode it writes to stdout.

use crate::headless::output::OutputSink;
use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OutputStream};
use std::sync::Arc;

/// Concrete adapter — owns an `Arc<dyn OutputSink>` and projects every
/// `OutputStream` method through it.
pub struct SinkAdapter {
    sink: Arc<dyn OutputSink>,
}

impl SinkAdapter {
    /// Construct an adapter forwarding to the given sink.
    #[must_use]
    pub fn new(sink: Arc<dyn OutputSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl OutputStream for SinkAdapter {
    async fn emit_text(&self, text: &str, _utf16_code_units: Option<&[u16]>) {
        self.sink.text(text).await;
    }
    async fn emit_system_notice(&self, body: &str, is_error: bool) {
        if is_error {
            self.sink.error("transcript_persistence_failed", body).await;
        } else {
            self.sink.command_output("system", body).await;
        }
    }
    async fn emit_mod_log(&self, plugin: &str, text: &str) {
        self.sink.mod_log(plugin, text).await;
    }
    async fn emit_mod_toast(&self, plugin: &str, text: &str, timeout_ms: u64) {
        self.sink.mod_toast(plugin, text, timeout_ms).await;
    }
    async fn emit_mod_status(&self, plugin: &str, text: Option<&str>) {
        self.sink.mod_status(plugin, text).await;
    }
    async fn emit_tool_call(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        tool: &str,
        input: &serde_json::Value,
        _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) {
        // CLI sinks (plain stdout + NDJSON) do not surface the tool_use_id
        // today — they're orientated on the wire-level event stream where
        // the id is implicit in dispatch order. M6-04 keeps the parameter
        // for forward compatibility; M6-09 may wire it into NDJSON.
        self.sink.tool_call(tool, input).await;
    }
    async fn emit_tool_result(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        tool: &str,
        _model_text: &str,
        result: &serde_json::Value,
        _projection: Option<&lingxi_core::host::ToolResultProjection>,
    ) {
        self.sink.tool_result(tool, result).await;
    }
    async fn emit_tool_heartbeat(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        elapsed_ms: u64,
    ) {
        self.sink
            .tool_heartbeat(id.as_str(), tool, elapsed_ms)
            .await;
    }
    async fn emit_end_turn(&self, stop_reason: &str, cost: &CostSnapshot) {
        self.sink
            .turn_end(
                stop_reason,
                cost.total_usd,
                cost.input_tokens,
                cost.output_tokens,
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        errors: Mutex<Vec<(String, String)>>,
        mod_logs: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl OutputSink for RecordingSink {
        async fn text(&self, _s: &str) {}
        async fn turn_start(&self) {}
        async fn turn_end(&self, _stop_reason: &str, _usd: f64, _input: u64, _output: u64) {}
        async fn tool_call(&self, _tool: &str, _input: &serde_json::Value) {}
        async fn tool_result(&self, _tool: &str, _result: &serde_json::Value) {}
        async fn tool_heartbeat(&self, _id: &str, _tool: &str, _elapsed_ms: u64) {}
        async fn command_output(&self, _name: &str, _display: &str) {}
        async fn mod_log(&self, plugin: &str, text: &str) {
            self.mod_logs
                .lock()
                .await
                .push((plugin.to_string(), text.to_string()));
        }
        async fn error(&self, code: &str, message: &str) {
            self.errors
                .lock()
                .await
                .push((code.to_string(), message.to_string()));
        }
    }

    #[tokio::test]
    async fn error_system_notice_uses_plain_and_json_sink_error_channel() {
        let sink = Arc::new(RecordingSink::default());
        let adapter = SinkAdapter::new(sink.clone());
        adapter
            .emit_system_notice("transcript persistence failed", true)
            .await;

        assert_eq!(
            sink.errors.lock().await.as_slice(),
            &[(
                "transcript_persistence_failed".to_string(),
                "transcript persistence failed".to_string(),
            )]
        );
    }

    #[tokio::test]
    async fn mod_log_keeps_the_plugin_and_text_fields() {
        let sink = Arc::new(RecordingSink::default());
        SinkAdapter::new(sink.clone())
            .emit_mod_log("review", "Found a mismatch")
            .await;
        assert_eq!(
            sink.mod_logs.lock().await.as_slice(),
            &[("review".to_string(), "Found a mismatch".to_string())]
        );
    }
}
