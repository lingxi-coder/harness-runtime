//! Output sinks: plain stdout vs JSON-NDJSON stdout.
//!
//! Locked NDJSON schema per plan M5-12 Task 0 step 3:
//!
//! ```text
//! {"event":"turn_start","ts":"<rfc3339-Z>","session_id":"<uuid>"}
//! {"event":"text","ts":"<rfc3339-Z>","content":"<chunk>"}
//! {"event":"tool_call","ts":"<rfc3339-Z>","tool":"<name>","input":{…}}
//! {"event":"tool_result","ts":"<rfc3339-Z>","tool":"<name>","result":{…}}
//! {"event":"turn_end","ts":"<rfc3339-Z>","stop_reason":"<r>","cost":{…}}
//! {"event":"command_output","ts":"<rfc3339-Z>","name":"<n>","display":"<d>"}
//! {"event":"error","ts":"<rfc3339-Z>","code":"<c>","message":"<m>"}
//! ```
//!
//! Every line is a JSON object terminated by `\n` (LF only, even on
//! Windows — `--json` is a machine-readable mode).

use crate::headless::io::Output;
use async_trait::async_trait;
use chrono::Utc;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::json;

/// Sink for CLI output. Implementations live in this module.
#[async_trait]
pub trait OutputSink: Send + Sync {
    /// Emit free-form text. In plain mode goes straight to stdout; in JSON
    /// mode goes as a `{"event":"text",…}` line.
    async fn text(&self, s: &str);
    /// Emit assistant text with exact source units in machine-readable sinks.
    async fn source_text(&self, text: &Utf16JsonProjection) {
        self.text(&display_projection(text)).await;
    }
    /// Emit a `turn_start` marker. No-op in plain mode.
    async fn turn_start(&self);
    /// Emit a `turn_end` marker with stop reason + cost. No-op in plain
    /// mode except that it terminates emitted assistant text with one newline.
    async fn turn_end(&self, stop_reason: &str, total_usd: f64, in_tokens: u64, out_tokens: u64);
    /// Emit a tool-call announcement.
    async fn tool_call(&self, tool: &str, input: &Utf16JsonProjection);
    /// Emit a tool-result announcement.
    async fn tool_result(&self, tool: &str, result: &Utf16JsonProjection);
    /// Emit a coalescible liveness update for a still-running tool.
    async fn tool_heartbeat(&self, id: &str, tool: &str, elapsed_ms: u64);
    /// Emit the output of a slash command.
    async fn command_output(&self, name: &str, display: &str);
    /// Emit command output without converting source units through a Rust string.
    async fn command_output_source(&self, name: &str, display: &Utf16JsonProjection) {
        self.command_output(name, &display_projection(display))
            .await;
    }
    /// Emit a Mod log line outside assistant/model text.
    async fn mod_log(&self, plugin: &str, text: &Utf16JsonProjection);
    /// Transient notification. Plain output has no persistent toast surface.
    async fn mod_toast(&self, _plugin: &str, _text: &Utf16JsonProjection, _timeout_ms: u64) {}
    /// Pinned status; plain output has no live status surface.
    async fn mod_status(&self, _plugin: &str, _text: Option<&Utf16JsonProjection>) {}
    /// Emit an error. Plain mode goes to stderr; JSON mode goes to stdout.
    async fn error(&self, code: &str, message: &str);
    /// Emit a source-backed error. Human sinks use the projection's display
    /// text; machine-readable sinks preserve its exact UTF-16 string value.
    async fn source_error(&self, code: &str, message: &Utf16JsonProjection) {
        self.error(code, &display_projection(message)).await;
    }
}

/// Plain-text stdout sink (default).
pub struct PlainSink {
    out: Output,
    err: Output,
    text_state: tokio::sync::Mutex<PlainTextState>,
}

#[derive(Default)]
struct PlainTextState {
    wrote_text: bool,
    ends_with_newline: bool,
}

impl PlainSink {
    /// Construct a sink using the host-provided stdout and diagnostic writers.
    #[must_use]
    pub fn new(out: Output, err: Output) -> Self {
        Self {
            out,
            err,
            text_state: tokio::sync::Mutex::new(PlainTextState::default()),
        }
    }

    async fn write_line(&self, line: &str) {
        let mut state = self.text_state.lock().await;
        if self.out.write_line(line).await.is_ok() {
            state.ends_with_newline = true;
        }
    }
}

#[async_trait]
impl OutputSink for PlainSink {
    async fn text(&self, s: &str) {
        let mut state = self.text_state.lock().await;
        if self.out.write_record(s.as_bytes()).await.is_ok() && !s.is_empty() {
            state.wrote_text = true;
            state.ends_with_newline = s.ends_with('\n');
        }
    }
    async fn turn_start(&self) {
        *self.text_state.lock().await = PlainTextState::default();
    }
    async fn turn_end(&self, _r: &str, _u: f64, _i: u64, _o: u64) {
        let mut state = self.text_state.lock().await;
        if state.wrote_text && !state.ends_with_newline {
            let _ = self.out.write_record(b"\n").await;
        }
        *state = PlainTextState::default();
    }
    async fn tool_call(&self, tool: &str, _input: &Utf16JsonProjection) {
        self.write_line(&format!("[tool: {tool}]")).await;
    }
    async fn tool_result(&self, _tool: &str, _result: &Utf16JsonProjection) {
        if self.err.is_terminal() {
            let _ = self.err.write_record(b"\r\x1b[2K").await;
        }
    }
    async fn tool_heartbeat(&self, _id: &str, tool: &str, elapsed_ms: u64) {
        if self.err.is_terminal() {
            let seconds = elapsed_ms / 1_000;
            let _ = self
                .err
                .write_record(format!("\r\x1b[2K[tool: {tool} · {seconds}s]").as_bytes())
                .await;
        }
    }
    async fn command_output(&self, _name: &str, display: &str) {
        self.write_line(display).await;
    }
    async fn mod_log(&self, plugin: &str, text: &Utf16JsonProjection) {
        self.write_line(&format!("{plugin}: {}", display_projection(text)))
            .await;
    }
    async fn error(&self, _code: &str, message: &str) {
        let _ = self.err.write_line(&format!("lingxi-cli: {message}")).await;
    }
}

/// JSON-NDJSON stdout sink (one JSON object per line).
pub struct JsonSink {
    out: Output,
    session_id: String,
}

impl JsonSink {
    /// Construct a new sink keyed to the given session id (used in
    /// `turn_start` events).
    #[must_use]
    pub fn new(session_id: lingxi_core::types::SessionId, out: Output) -> Self {
        Self {
            out,
            session_id: session_id.to_string(),
        }
    }

    async fn emit(&self, obj: serde_json::Value) {
        let line = crate::headless::stream_json::serialize_ndjson_line(&obj);
        let _ = self.out.write_record(line.as_bytes()).await;
    }

    async fn emit_projected(&self, frame: &Utf16JsonProjection) {
        let line = crate::headless::stream_json::serialize_projected_ndjson_line(frame)
            .expect("source output frame is valid JSON");
        let _ = self.out.write_record(line.as_bytes()).await;
    }

    async fn emit_source(
        &self,
        frame: serde_json::Value,
        pointer: &str,
        source: &Utf16JsonProjection,
    ) {
        let mut frame =
            Utf16JsonProjection::from_serde_value(&frame).expect("machine frame is valid JSON");
        frame
            .replace_pointer_projection(pointer, source.clone())
            .expect("source field exists");
        self.emit_projected(&frame).await;
    }

    fn ts() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }
}

#[async_trait]
impl OutputSink for JsonSink {
    async fn text(&self, s: &str) {
        self.emit(json!({"event": "text", "ts": Self::ts(), "content": s}))
            .await;
    }
    async fn source_text(&self, text: &Utf16JsonProjection) {
        self.emit_source(
            json!({"event": "text", "ts": Self::ts(), "content": null}),
            "/content",
            text,
        )
        .await;
    }
    async fn turn_start(&self) {
        self.emit(json!({
            "event": "turn_start",
            "ts": Self::ts(),
            "session_id": self.session_id,
        }))
        .await;
    }
    async fn turn_end(&self, stop: &str, usd: f64, i: u64, o: u64) {
        self.emit(json!({
            "event": "turn_end",
            "ts": Self::ts(),
            "stop_reason": stop,
            "cost": {"total_usd": usd, "input_tokens": i, "output_tokens": o},
        }))
        .await;
    }
    async fn tool_call(&self, tool: &str, input: &Utf16JsonProjection) {
        self.emit_source(
            json!({"event":"tool_call", "tool":tool, "ts":Self::ts(), "input":null}),
            "/input",
            input,
        )
        .await;
    }
    async fn tool_result(&self, tool: &str, result: &Utf16JsonProjection) {
        self.emit_source(
            json!({"event":"tool_result", "tool":tool, "ts":Self::ts(), "result":null}),
            "/result",
            result,
        )
        .await;
    }
    async fn tool_heartbeat(&self, id: &str, tool: &str, elapsed_ms: u64) {
        self.emit(json!({
            "event": "tool_heartbeat",
            "ts": Self::ts(),
            "id": id,
            "tool": tool,
            "elapsed_ms": elapsed_ms,
        }))
        .await;
    }
    async fn command_output(&self, name: &str, display: &str) {
        self.emit(json!({
            "event": "command_output",
            "ts": Self::ts(),
            "name": name,
            "display": display,
        }))
        .await;
    }
    async fn command_output_source(&self, name: &str, display: &Utf16JsonProjection) {
        self.emit_source(
            json!({"event": "command_output", "ts": Self::ts(), "name": name, "display": null}),
            "/display",
            display,
        )
        .await;
    }
    async fn mod_log(&self, plugin: &str, text: &Utf16JsonProjection) {
        self.emit_source(
            json!({"event":"ui_log", "plugin":plugin, "ts":Self::ts(), "text":null}),
            "/text",
            text,
        )
        .await;
    }
    async fn mod_toast(&self, plugin: &str, text: &Utf16JsonProjection, timeout_ms: u64) {
        self.emit_source(json!({"event":"ui_toast", "plugin":plugin, "timeout_ms":timeout_ms, "ts":Self::ts(), "text":null}), "/text", text).await;
    }
    async fn mod_status(&self, plugin: &str, text: Option<&Utf16JsonProjection>) {
        if let Some(text) = text {
            self.emit_source(
                json!({"event":"ui_status", "plugin":plugin, "ts":Self::ts(), "text":null}),
                "/text",
                text,
            )
            .await;
        } else {
            self.emit(json!({"event":"ui_status", "plugin":plugin, "ts":Self::ts(), "text":null}))
                .await;
        }
    }
    async fn error(&self, code: &str, message: &str) {
        self.emit(json!({
            "event": "error",
            "ts": Self::ts(),
            "code": code,
            "message": message,
        }))
        .await;
    }
    async fn source_error(&self, code: &str, message: &Utf16JsonProjection) {
        let frame = Utf16JsonProjection::object(vec![
            ("event".into(), "error".into()),
            ("ts".into(), Self::ts().into()),
            ("code".into(), code.into()),
            ("message".into(), message.clone()),
        ])
        .expect("source error frame has valid fields");
        self.emit_projected(&frame).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn ts_format_rfc3339_z_seconds() {
        let s = JsonSink::ts();
        // Format: "YYYY-MM-DDTHH:MM:SSZ" (no millis, no offset, terminal Z).
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20);
    }

    #[tokio::test]
    async fn plain_sink_command_output_writes_to_stdout() {
        // We can't capture stdout from inside the same process easily;
        // this test exists to guarantee the method doesn't panic with
        // an unusual character set.
        let sink = PlainSink::new(
            Output::new(tokio::io::sink()),
            Output::new(tokio::io::sink()),
        );
        sink.command_output("clear", "Conversation cleared.").await;
    }

    #[tokio::test]
    async fn plain_sink_terminates_text_output_once_at_turn_end() {
        use tokio::io::AsyncReadExt;

        for (chunks, expected) in [
            (
                vec!["fixed native ", "stub result"],
                "fixed native stub result\n",
            ),
            (
                vec!["fixed native stub result\n"],
                "fixed native stub result\n",
            ),
        ] {
            let (writer, mut reader) = tokio::io::duplex(256);
            let sink = PlainSink::new(Output::new(writer), Output::new(tokio::io::sink()));
            sink.turn_start().await;
            for chunk in chunks {
                sink.text(chunk).await;
            }
            sink.turn_end("end_turn", 0.0, 0, 0).await;
            drop(sink);

            let mut actual = Vec::new();
            reader
                .read_to_end(&mut actual)
                .await
                .expect("read complete plain text output");
            assert_eq!(actual, expected.as_bytes());
        }
    }

    #[tokio::test]
    async fn plain_sink_line_output_marks_the_stdout_terminator() {
        use tokio::io::AsyncReadExt;

        let (writer, mut reader) = tokio::io::duplex(256);
        let sink = PlainSink::new(Output::new(writer), Output::new(tokio::io::sink()));
        sink.turn_start().await;
        sink.text("assistant text").await;
        sink.tool_call(
            "Read",
            &Utf16JsonProjection::from_serde_value(&serde_json::Value::Null),
        )
        .await;
        sink.turn_end("end_turn", 0.0, 0, 0).await;
        drop(sink);

        let mut actual = Vec::new();
        reader
            .read_to_end(&mut actual)
            .await
            .expect("read complete plain text output");
        assert_eq!(actual, b"assistant text[tool: Read]\n");
    }

    #[tokio::test]
    async fn json_source_error_writes_exact_utf16_message_frame_bytes() {
        let cases = [
            ("lone surrogate", vec![0xd800], r#""message":"\ud800""#),
            ("surrogate pair", vec![0xd83d, 0xde00], "\"message\":\"😀\""),
            ("empty string", Vec::new(), r#""message":"""#),
        ];

        for (case, units, encoded_message) in cases {
            let (writer, mut reader) = tokio::io::duplex(1024);
            let output = Output::new(writer);
            let session_id = lingxi_core::types::SessionId::parse_prefixed(
                "11111111-2222-3333-4444-555555555555",
            )
            .expect("test session id");
            let sink = JsonSink::new(session_id, output);
            let message =
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone())
                    .expect("source error message is a root string");

            sink.source_error("startup", &message).await;

            let mut bytes = Vec::new();
            loop {
                let mut byte = [0; 1];
                reader
                    .read_exact(&mut byte)
                    .await
                    .expect("read serialized machine frame");
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            let encoded = std::str::from_utf8(&bytes[..bytes.len() - 1])
                .expect("machine frame is UTF-8 JSON bytes");
            assert!(encoded.contains(encoded_message), "{case}: {encoded}");

            let frame = Utf16JsonProjection::parse(encoded).expect("serialized frame is JSON");
            assert_eq!(frame.string_at("/event"), Some("error"), "{case}");
            assert_eq!(frame.string_at("/code"), Some("startup"), "{case}");
            assert_eq!(frame.string_units("/message"), Some(units), "{case}");

            let field_order = ["\"event\"", "\"ts\"", "\"code\"", "\"message\""];
            let positions = field_order.map(|field| {
                encoded
                    .find(field)
                    .unwrap_or_else(|| panic!("{case}: missing {field}"))
            });
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{case}");
        }
    }
}

fn display_projection(text: &Utf16JsonProjection) -> String {
    String::from_utf16_lossy(&text.string_units("").expect("output text is a root string"))
}
