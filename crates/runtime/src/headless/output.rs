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
use serde_json::json;

/// Sink for CLI output. Implementations live in this module.
#[async_trait]
pub trait OutputSink: Send + Sync {
    /// Emit free-form text. In plain mode goes straight to stdout; in JSON
    /// mode goes as a `{"event":"text",…}` line.
    async fn text(&self, s: &str);
    /// Emit a `turn_start` marker. No-op in plain mode.
    async fn turn_start(&self);
    /// Emit a `turn_end` marker with stop reason + cost. No-op in plain
    /// mode (the orchestrator's `OutputStream::emit_end_turn` already
    /// prints to stdout there).
    async fn turn_end(&self, stop_reason: &str, total_usd: f64, in_tokens: u64, out_tokens: u64);
    /// Emit a tool-call announcement.
    async fn tool_call(&self, tool: &str, input: &serde_json::Value);
    /// Emit a tool-result announcement.
    async fn tool_result(&self, tool: &str, result: &serde_json::Value);
    /// Emit a coalescible liveness update for a still-running tool.
    async fn tool_heartbeat(&self, id: &str, tool: &str, elapsed_ms: u64);
    /// Emit the output of a slash command.
    async fn command_output(&self, name: &str, display: &str);
    /// Emit a Mod log line outside assistant/model text.
    async fn mod_log(&self, plugin: &str, text: &str);
    /// Transient notification. Plain output has no persistent toast surface.
    async fn mod_toast(&self, _plugin: &str, _text: &str, _timeout_ms: u64) {}
    /// Pinned status; plain output has no live status surface.
    async fn mod_status(&self, _plugin: &str, _text: Option<&str>) {}
    /// Emit an error. Plain mode goes to stderr; JSON mode goes to stdout.
    async fn error(&self, code: &str, message: &str);
}

/// Plain-text stdout sink (default).
pub struct PlainSink {
    out: Output,
    err: Output,
}

impl PlainSink {
    /// Construct a sink using the host-provided stdout and diagnostic writers.
    #[must_use]
    pub fn new(out: Output, err: Output) -> Self {
        Self { out, err }
    }
}

#[async_trait]
impl OutputSink for PlainSink {
    async fn text(&self, s: &str) {
        let _ = self.out.write_record(s.as_bytes()).await;
    }
    async fn turn_start(&self) {}
    async fn turn_end(&self, _r: &str, _u: f64, _i: u64, _o: u64) {}
    async fn tool_call(&self, tool: &str, _input: &serde_json::Value) {
        let _ = self.out.write_line(&format!("[tool: {tool}]")).await;
    }
    async fn tool_result(&self, _tool: &str, _result: &serde_json::Value) {
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
        let _ = self.out.write_line(display).await;
    }
    async fn mod_log(&self, plugin: &str, text: &str) {
        let _ = self.out.write_line(&format!("{plugin}: {text}")).await;
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
    async fn tool_call(&self, tool: &str, input: &serde_json::Value) {
        self.emit(json!({
            "event": "tool_call",
            "ts": Self::ts(),
            "tool": tool,
            "input": input,
        }))
        .await;
    }
    async fn tool_result(&self, tool: &str, result: &serde_json::Value) {
        self.emit(json!({
            "event": "tool_result",
            "ts": Self::ts(),
            "tool": tool,
            "result": result,
        }))
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
    async fn mod_log(&self, plugin: &str, text: &str) {
        self.emit(json!({
            "event": "ui_log",
            "ts": Self::ts(),
            "plugin": plugin,
            "text": text,
        }))
        .await;
    }
    async fn mod_toast(&self, plugin: &str, text: &str, timeout_ms: u64) {
        self.emit(json!({
            "event": "ui_toast",
            "ts": Self::ts(),
            "plugin": plugin,
            "text": text,
            "timeout_ms": timeout_ms,
        }))
        .await;
    }
    async fn mod_status(&self, plugin: &str, text: Option<&str>) {
        self.emit(json!({
            "event": "ui_status",
            "ts": Self::ts(),
            "plugin": plugin,
            "text": text,
        }))
        .await;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
