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
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::Value;
use std::sync::Arc;

fn output_value(projection: &Utf16JsonProjection) -> Value {
    let json = projection
        .to_json_string()
        .expect("OutputStream projections are valid JSON");
    serde_json::from_str(&json)
        .expect("CLI OutputSink values must be representable as serde_json::Value")
}

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
    async fn emit_text(&self, text: &Utf16JsonProjection) {
        self.sink
            .text(
                text.string_at("")
                    .expect("OutputStream text is a root string"),
            )
            .await;
    }
    async fn emit_system_notice(&self, body: &Utf16JsonProjection, is_error: bool) {
        let body = body
            .string_at("")
            .expect("OutputStream system notice is a root string");
        if is_error {
            self.sink.error("transcript_persistence_failed", body).await;
        } else {
            self.sink.command_output("system", body).await;
        }
    }
    async fn emit_mod_log(&self, plugin: &str, text: &Utf16JsonProjection) {
        self.sink
            .mod_log(
                plugin,
                text.string_at("")
                    .expect("OutputStream Mod log is a root string"),
            )
            .await;
    }
    async fn emit_mod_toast(&self, plugin: &str, text: &Utf16JsonProjection, timeout_ms: u64) {
        self.sink
            .mod_toast(
                plugin,
                text.string_at("")
                    .expect("OutputStream Mod toast is a root string"),
                timeout_ms,
            )
            .await;
    }
    async fn emit_mod_status(&self, plugin: &str, text: Option<&Utf16JsonProjection>) {
        self.sink
            .mod_status(
                plugin,
                text.map(|value| {
                    value
                        .string_at("")
                        .expect("OutputStream Mod status is a root string")
                }),
            )
            .await;
    }
    async fn emit_tool_call(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        tool: &str,
        input: &Utf16JsonProjection,
    ) {
        // CLI sinks (plain stdout + NDJSON) do not surface the tool_use_id
        // today — they're orientated on the wire-level event stream where
        // the id is implicit in dispatch order. M6-04 keeps the parameter
        // for forward compatibility; M6-09 may wire it into NDJSON.
        self.sink.tool_call(tool, &output_value(input)).await;
    }
    async fn emit_tool_result(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        tool: &str,
        _model_text: &Utf16JsonProjection,
        result: &Utf16JsonProjection,
        _mcp_meta: Option<&Utf16JsonProjection>,
    ) {
        // The CLI OutputSink exposes tool result data, not native SDK envelope
        // metadata; stream-json carries `mcpMeta` through its own adapter.
        self.sink.tool_result(tool, &output_value(result)).await;
    }
    async fn emit_tool_result_denied(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        tool: &str,
        _model_text: &Utf16JsonProjection,
        result: &Utf16JsonProjection,
        _denial_kind: &str,
        _projection: Option<&lingxi_core::host::ToolResultProjection>,
    ) {
        self.sink.tool_result(tool, &output_value(result)).await;
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
            .emit_system_notice(
                &Utf16JsonProjection::from("transcript persistence failed"),
                true,
            )
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
            .emit_mod_log("review", &Utf16JsonProjection::from("Found a mismatch"))
            .await;
        assert_eq!(
            sink.mod_logs.lock().await.as_slice(),
            &[("review".to_string(), "Found a mismatch".to_string())]
        );
    }
}
