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
    async fn emit_text(&self, text: &Utf16JsonProjection) {
        self.sink.source_text(text).await;
    }
    async fn emit_system_notice(&self, body: &Utf16JsonProjection, is_error: bool) {
        if is_error {
            self.sink
                .source_error("transcript_persistence_failed", body)
                .await;
        } else {
            self.sink.command_output_source("system", body).await;
        }
    }
    async fn emit_mod_log(&self, plugin: &str, text: &Utf16JsonProjection) {
        self.sink.mod_log(plugin, text).await;
    }
    async fn emit_mod_toast(&self, plugin: &str, text: &Utf16JsonProjection, timeout_ms: u64) {
        self.sink.mod_toast(plugin, text, timeout_ms).await;
    }
    async fn emit_mod_status(&self, plugin: &str, text: Option<&Utf16JsonProjection>) {
        self.sink.mod_status(plugin, text).await;
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
        self.sink.tool_call(tool, input).await;
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
        self.sink.tool_result(tool, result).await;
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
        async fn tool_call(&self, _tool: &str, _input: &Utf16JsonProjection) {}
        async fn tool_result(&self, _tool: &str, _result: &Utf16JsonProjection) {}
        async fn tool_heartbeat(&self, _id: &str, _tool: &str, _elapsed_ms: u64) {}
        async fn command_output(&self, _name: &str, _display: &str) {}
        async fn mod_log(&self, plugin: &str, text: &Utf16JsonProjection) {
            self.mod_logs.lock().await.push((
                plugin.to_string(),
                String::from_utf16_lossy(&text.string_units("").unwrap()),
            ));
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
    #[tokio::test]
    async fn json_adapter_preserves_source_units_in_every_output_channel() {
        use crate::headless::io::Output;
        use crate::headless::output::JsonSink;
        use tokio::io::AsyncReadExt;
        for units in [
            vec![0xd800],
            vec![0xd83d, 0xde00],
            vec![],
            vec![0xdc00, 0x2028],
        ] {
            let text =
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone())
                    .unwrap();
            let object = Utf16JsonProjection::object_units(vec![
                ("field".encode_utf16().collect(), text.clone()),
                (vec![0xd800], text.clone()),
            ])
            .unwrap();
            let (writer, mut reader) = tokio::io::duplex(16384);
            let sink = Arc::new(JsonSink::new(
                lingxi_core::types::SessionId::parse_prefixed(
                    "11111111-2222-3333-4444-555555555555",
                )
                .unwrap(),
                Output::new(writer),
            ));
            let adapter = SinkAdapter::new(sink.clone());
            let id = lingxi_core::types::ToolUseId::from("source-test");
            adapter.emit_text(&text).await;
            adapter.emit_tool_call(&id, "test", &object).await;
            adapter
                .emit_tool_result(&id, "test", &text, &object, None)
                .await;
            adapter
                .emit_tool_result_denied(&id, "test", &text, &object, "permission", None)
                .await;
            adapter.emit_system_notice(&text, false).await;
            adapter.emit_system_notice(&text, true).await;
            adapter.emit_mod_log("test", &text).await;
            adapter.emit_mod_toast("test", &text, 10).await;
            adapter.emit_mod_status("test", Some(&text)).await;
            adapter.emit_mod_status("test", None).await;
            sink.command_output_source("test", &text).await;
            drop(adapter);
            drop(sink);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            let output = String::from_utf8(bytes).unwrap();
            let frames: Vec<_> = output
                .lines()
                .map(|line| Utf16JsonProjection::parse(line).unwrap())
                .collect();
            assert_eq!(frames.len(), 11);
            for (index, path) in [
                "/content",
                "/input/field",
                "/result/field",
                "/result/field",
                "/display",
                "/message",
                "/text",
                "/text",
                "/text",
            ]
            .iter()
            .enumerate()
            {
                assert_eq!(
                    frames[index].string_units(path),
                    Some(units.clone()),
                    "channel {index}"
                );
            }
            for index in 1..=3 {
                let pointer = if index == 1 { "/input" } else { "/result" };
                let object = frames[index].at_pointer(pointer).unwrap();
                assert_eq!(
                    object.get_property_units(&[0xd800]).unwrap().code_units(),
                    Some(units.as_slice())
                );
            }
            assert!(frames[9].at_pointer("/text").unwrap().is_null());
            assert_eq!(frames[10].string_units("/display"), Some(units));
        }
    }

    #[tokio::test]
    async fn plain_adapter_renders_lone_surrogates_without_losing_tool_objects() {
        use crate::headless::io::Output;
        use crate::headless::output::PlainSink;
        use tokio::io::AsyncReadExt;
        let text = Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        let object = Utf16JsonProjection::object(vec![("field".into(), text.clone())]).unwrap();
        let (writer, mut reader) = tokio::io::duplex(4096);
        let sink = Arc::new(PlainSink::new(
            Output::new(writer),
            Output::new(tokio::io::sink()),
        ));
        let adapter = SinkAdapter::new(sink.clone());
        adapter.emit_text(&text).await;
        adapter
            .emit_tool_call(
                &lingxi_core::types::ToolUseId::from("test"),
                "read",
                &object,
            )
            .await;
        adapter.emit_system_notice(&text, false).await;
        drop(adapter);
        drop(sink);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "�[tool: read]\n�\n");
    }
}
