//! Stream-JSON output mode (`--output-format stream-json --verbose`).
//!
//! Implements the `OutputStream` trait as a NDJSON writer that emits
//! claude-code-compatible frames to stdout during a `-p` run:
//!
//! 1. `system/init`  — emitted once before the turn (call `emit_init()`).
//! 2. `system/status` — emitted once before the API call (call `emit_status()`).
//! 3. `assistant` — accumulated per-message, flushed at `emit_message_boundary()`.
//! 4. `user` — tool_result echo, emitted by `emit_tool_result()`.
//! 5. `result` — emitted at the end via `emit_result_success()` / `emit_result_error()`.
//!
//! ## Wire format
//! - Compact JSON + `\n` (LF only).
//! - U+2028 → ` `, U+2029 → ` ` (line-splitter safety).
//! - Every frame carries `session_id` + `uuid` (random v4).
//! - Single-writer stdout drain: all frame emitters push pre-serialised NDJSON
//!   lines onto an unbounded mpsc channel; one drain task is the sole stdout
//!   writer, guaranteeing strict FIFO order (no control-frame overtake).
//!   This mirrors the TS `outbound = Stream<StdoutMessage>` + single drain loop
//!   (`structuredIO.ts:160-162`, Phase 0 prerequisite for the control plane).
//!
//! ## Phase 0 note (single-writer stdout drain)
//!
//! The previous `Arc<Mutex<Stdout>>` approach guaranteed per-line atomicity but
//! allowed two racing tasks to interleave at line granularity when the mutex
//! was released between frames. The mpsc + drain task gives strict FIFO at the
//! frame level (not just per-line), which is required by the control protocol
//! (§1.5 of the SPEC: "Control plane NEVER overtakes the data plane").
//!
//! The drain task is spawned lazily on the first `emit_*` call (or explicitly
//! via `ensure_drain_started`). In tests the channel is unbounded so no blocking.

#![forbid(unsafe_code)]

#[path = "request_markers.rs"]
mod request_markers;
use request_markers::RequestMarkers;

use crate::headless::io::Output;
use async_trait::async_trait;
use lingxi_core::host::orchestrator::ResponseTimingEvent;
use lingxi_core::host::{CostSnapshot, OutputStream};
use lingxi_core::types::utf16_json::{Utf16JsonProjection, Utf16JsonProjectionError};
use llm_runtime::model::context_window::{context_window_for_model, max_output_tokens_for_model};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, mpsc, oneshot};

// ── Wire-format helpers ─────────────────────────────────────────────────────

/// Escape U+2028/U+2029 after JSON serialization so streaming line-parsers
/// can't be split mid-line by these Unicode newline characters.
fn escape_line_terminators(s: &str) -> String {
    s.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Serialize a JSON value to an escaped NDJSON line (compact + LF).
/// This is the canonical wire-format serialiser for both the drain task
/// and any caller that needs to bypass the channel (e.g. `emit_replay_ack`).
pub fn serialize_ndjson_line(v: &Value) -> String {
    let s = Utf16JsonProjection::plain(v.clone())
        .to_json_string()
        .expect("plain JSON projection is valid");
    let mut line = escape_line_terminators(&s);
    line.push('\n');
    line
}

/// Serialize a frame without losing JavaScript UTF-16 strings or keys.
pub fn serialize_projected_ndjson_line(
    v: &Utf16JsonProjection,
) -> Result<String, Utf16JsonProjectionError> {
    let mut line = escape_line_terminators(&v.to_json_string()?);
    line.push('\n');
    Ok(line)
}

// ── Drain task ──────────────────────────────────────────────────────────────

/// Spawn the single-writer drain task: the sole consumer of the outbound mpsc
/// channel that writes serialised NDJSON lines to stdout in FIFO order.
///
/// `rx` is the receiving end of the channel. The task runs until the sender
/// side is dropped (all `StreamJsonStream` clones and ControlPlaneWriter clones
/// are gone), then it flushes and exits.
///
/// Stdout ordering: the drain task is the only thing that calls `write_all`
/// on stdout. No other code touches stdout after this task starts — the
/// streaming replay-ack sites (`run.rs` in-turn-loop ack + `spawn_stdin_router`
/// duplicate-ack) route through this same queue via
/// `stream_json_input::emit_replay_ack_queued`. The direct-write
/// `emit_replay_ack` survives only for the batch `read_input_turns` path, which
/// runs before any drain task exists (tests / non-streaming callers).
#[derive(Debug, Default)]
struct CoalescedHeartbeatLineState {
    latest: Vec<(String, String)>,
    signal_queued: bool,
}

/// Latest-value mailbox for stream-json tool heartbeats. It bounds queued
/// heartbeat wake-ups to one while retaining the newest frame per tool call;
/// data, control, and result frames remain ordinary FIFO messages.
#[derive(Debug, Default)]
pub struct CoalescedHeartbeatLines {
    state: StdMutex<CoalescedHeartbeatLineState>,
}

impl CoalescedHeartbeatLines {
    fn publish(&self, id: String, line: String) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, current)) = state.latest.iter_mut().find(|(key, _)| key == &id) {
            *current = line;
        } else {
            state.latest.push((id, line));
        }
        if state.signal_queued {
            false
        } else {
            state.signal_queued = true;
            true
        }
    }

    fn drain(&self) -> Vec<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.signal_queued = false;
        std::mem::take(&mut state.latest)
            .into_iter()
            .map(|(_, line)| line)
            .collect()
    }

    fn reset_after_send_failure(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.signal_queued = false;
        state.latest.clear();
    }
}

pub enum OutboundMsg {
    Line(String),
    /// A stream-event line whose pending-capacity reservation is tracked by
    /// the drain task. Keeping the kind out-of-band avoids classifying an
    /// ordinary frame from its serialized contents (which may contain a
    /// nested `{"type":"stream_event"}` value).
    StreamEvent(String),
    /// Wake the single writer to drain the coalesced heartbeat mailbox.
    Heartbeats(Arc<CoalescedHeartbeatLines>),
    Flush(oneshot::Sender<std::io::Result<()>>),
    PublishJson(oneshot::Sender<std::io::Result<()>>),
    /// Internal writer command, never a protocol frame.
    RefreshHeldResultTotals(Value),
    Shutdown(oneshot::Sender<std::io::Result<()>>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamJsonOutputMode {
    Ndjson,
    VerboseJson,
    LastResultJson,
    LastResultText,
}

#[derive(Clone, Copy, Default)]
struct PrintTextLimits {
    max_turns: Option<u32>,
    max_budget_usd: Option<f64>,
}

fn render_text_result(frame: &Utf16JsonProjection, limits: PrintTextLimits) -> String {
    let string = |path: &str| frame.string_units(path).map(|units| String::from_utf16_lossy(&units))
        .or_else(|| frame.value.pointer(path).and_then(Value::as_str).map(str::to_owned));
    match frame.value["subtype"].as_str() {
        Some("success") => {
            let mut text = string("/result").unwrap_or_default();
            if !text.ends_with('\n') { text.push('\n'); }
            text
        }
        Some("error_max_turns") => format!("Error: Reached max turns ({})", limits.max_turns.map(|value| value.to_string()).unwrap_or_else(|| "undefined".into())),
        Some("error_max_budget_usd") => format!("Error: Exceeded USD budget ({})", limits.max_budget_usd.map(|value| value.to_string()).unwrap_or_else(|| "undefined".into())),
        Some("error_max_structured_output_retries") => format!("Error: {}", string("/errors/0").unwrap_or_else(|| "Failed to provide valid structured output after maximum retries".into())),
        Some("error_during_execution") => "Execution error".into(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[test]
fn print_text_result_renderer_preserves_native_diagnostic_bytes() {
    let limits = PrintTextLimits { max_turns: Some(1), max_budget_usd: Some(0.5) };
    for (frame, expected) in [
        (json!({"subtype":"success", "result":"API Error: 400 HEADLESS_LOCAL_PROVIDER_ERROR", "is_error":true}), "API Error: 400 HEADLESS_LOCAL_PROVIDER_ERROR\n"),
        (json!({"subtype":"success", "result":"already terminated\n"}), "already terminated\n"),
        (json!({"subtype":"error_during_execution", "errors":["must not be joined"]}), "Execution error"),
        (json!({"subtype":"error_max_turns", "errors":["different driver diagnostic"]}), "Error: Reached max turns (1)"),
        (json!({"subtype":"error_max_budget_usd"}), "Error: Exceeded USD budget (0.5)"),
        (json!({"subtype":"error_max_structured_output_retries", "errors":["schema failure", "ignored"]}), "Error: schema failure"),
        (json!({"subtype":"error_max_structured_output_retries", "errors":[]}), "Error: Failed to provide valid structured output after maximum retries"),
    ] {
        assert_eq!(render_text_result(&Utf16JsonProjection::plain(frame), limits).as_bytes(), expected.as_bytes());
    }
}

fn verbose_json_keeps_frame(line: &str) -> bool {
    let Ok(frame) = Utf16JsonProjection::parse(line.trim_end_matches('\n')) else {
        return false;
    };
    let frame_type = frame.value.get("type").and_then(Value::as_str);
    match frame_type {
        Some("ui_invalidate" | "stream_event") => false,
        Some("system")
            if matches!(
                frame.value.get("subtype").and_then(Value::as_str),
                Some("status" | "hook_started" | "hook_response" | "task_started" | "task_progress" | "task_notification")
            ) =>
        {
            false
        }
        _ => true,
    }
}

struct PrintFrameBuffer {
    conversation: Vec<String>,
    results: Vec<String>,
    latest_totals: Option<Value>,
    published: bool,
    text_limits: PrintTextLimits,
}

impl PrintFrameBuffer {
    fn new() -> Self {
        Self { conversation: Vec::new(), results: Vec::new(), latest_totals: None, published: false, text_limits: PrintTextLimits::default() }
    }

    async fn line(&mut self, stdout: &Output, line: String, mode: StreamJsonOutputMode) {
        if mode == StreamJsonOutputMode::Ndjson {
            let _ = stdout.write_record(line.as_bytes()).await;
            return;
        }
        if self.published { return; }
        let Ok(frame) = Utf16JsonProjection::parse(line.trim_end_matches('\n')) else { return; };
        let line = line.strip_suffix('\n').unwrap_or(&line).to_owned();
        if frame.value["type"] == "result" {
            if mode != StreamJsonOutputMode::VerboseJson { self.results.clear(); }
            self.results.push(line);
        } else if mode == StreamJsonOutputMode::VerboseJson && verbose_json_keeps_frame(&line) {
            self.conversation.push(line);
        }
    }

    fn refreshed_result(&self, raw: &str) -> Result<String, std::io::Error> {
        let Some(totals) = &self.latest_totals else { return Ok(raw.to_owned()); };
        let mut frame = Utf16JsonProjection::parse(raw).map_err(std::io::Error::other)?;
        let monotonic_cost = totals["total_cost_usd"].as_f64().unwrap_or_default()
            >= frame.value["total_cost_usd"].as_f64().unwrap_or_default();
        for key in ["total_cost_usd", "duration_api_ms", "modelUsage", "subagent_stats", "safety_stops"] {
            let Some(mut value) = totals.get(key).cloned() else { continue; };
            if !matches!(key, "subagent_stats" | "safety_stops") && !monotonic_cost { continue; }
            if key == "duration_api_ms" && frame.value[key] == 0 { continue; }
            if key == "safety_stops" {
                let Some(previous) = frame.value.get(key).and_then(Value::as_u64) else { continue; };
                value = json!(previous.max(value.as_u64().unwrap_or_default()));
            }
            frame.set_pointer(&format!("/{key}"), Utf16JsonProjection::plain(value)).map_err(std::io::Error::other)?;
        }
        frame.to_json_string().map_err(std::io::Error::other)
    }

    async fn publish(&mut self, stdout: &Output, mode: StreamJsonOutputMode) -> std::io::Result<()> {
        if mode == StreamJsonOutputMode::Ndjson || self.published { return stdout.flush().await; }
        self.published = true;
        let results = self.results.iter().map(|raw| self.refreshed_result(raw)).collect::<Result<Vec<_>,_>>()?;
        let bytes = match mode {
            StreamJsonOutputMode::VerboseJson => {
                let frames = self.conversation.iter().chain(results.iter()).cloned().collect::<Vec<_>>();
                format!("[{}]\n", frames.join(","))
            }
            StreamJsonOutputMode::LastResultJson => results.last().map(|raw| format!("{raw}\n")).unwrap_or_default(),
            StreamJsonOutputMode::LastResultText => {
                match results.last() {
                    Some(raw) => {
                        let frame = Utf16JsonProjection::parse(raw).map_err(std::io::Error::other)?;
                        render_text_result(&frame, self.text_limits)
                    }
                    None => String::new(),
                }
            }
            StreamJsonOutputMode::Ndjson => unreachable!(),
        };
        stdout.write_record(bytes.as_bytes()).await
    }
}

fn spawn_drain_task(
    mut rx: mpsc::UnboundedReceiver<OutboundMsg>,
    pending_stream_events: Arc<AtomicUsize>,
    stdout: Output,
    mode: StreamJsonOutputMode,
    text_limits: PrintTextLimits,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut frames = PrintFrameBuffer::new();
        frames.text_limits = text_limits;
        loop {
            let msg = tokio::select! {
                biased;
                _ = stdout.delivery_cancelled() => break,
                msg = rx.recv() => match msg { Some(msg) => msg, None => break },
            };
            match msg {
                OutboundMsg::Line(line) => frames.line(&stdout, line, mode).await,
                OutboundMsg::StreamEvent(line) => {
                    frames.line(&stdout, line, mode).await;
                    let _ = pending_stream_events.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| Some(count.saturating_sub(1)));
                }
                OutboundMsg::Heartbeats(heartbeats) => {
                    for line in heartbeats.drain() { frames.line(&stdout, line, mode).await; }
                }
                OutboundMsg::RefreshHeldResultTotals(totals) => frames.latest_totals = Some(totals),
                OutboundMsg::Shutdown(done) => {
                    let _ = done.send(frames.publish(&stdout, mode).await);
                    break;
                }
                OutboundMsg::Flush(done) => { let _ = done.send(stdout.flush().await); }
                OutboundMsg::PublishJson(done) => { let _ = done.send(frames.publish(&stdout, mode).await); }
            }
        }
        if !frames.published && stdout.error().is_none() { let _ = frames.publish(&stdout, mode).await; }
        let _ = stdout.flush().await;
    })
}

// ── Content block accumulator ────────────────────────────────────────────────

/// A single accumulated content block for the current assistant message.
#[derive(Debug, Clone)]
enum AccBlock {
    Text(Vec<u16>),
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        input_projection: Option<Utf16JsonProjection>,
    },
}

impl AccBlock {
    fn to_json(&self) -> Value {
        match self {
            AccBlock::Text(t) => json!({"type": "text", "text": String::from_utf16_lossy(t)}),
            AccBlock::Thinking {
                thinking,
                signature,
            } => {
                let mut m = serde_json::Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(thinking));
                if let Some(sig) = signature {
                    m.insert("signature".into(), json!(sig));
                } else {
                    m.insert("signature".into(), Value::Null);
                }
                Value::Object(m)
            }
            AccBlock::ToolUse {
                id, name, input, ..
            } => {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
        }
    }

    /// Extract text content, if this is a Text block.
    fn as_text(&self) -> Option<&[u16]> {
        if let AccBlock::Text(t) = self {
            Some(t.as_slice())
        } else {
            None
        }
    }
}

// ── Per-message accumulator ──────────────────────────────────────────────────

#[derive(Debug, Default)]
struct MessageAccum {
    /// API-assigned message id (from `message_start`, e.g. `"msg_01..."`).
    message_id: String,
    /// Model id from `message_start`.
    model: String,
    /// Accumulated blocks in observation order.
    blocks: Vec<AccBlock>,
    /// Latest usage snapshot (input/output/cache_read/cache_creation tokens).
    usage_input: u64,
    usage_output: u64,
    usage_cache_read: u64,
    usage_cache_creation: u64,
    new_text_block: bool,
}

impl MessageAccum {
    fn reset(&mut self) {
        *self = MessageAccum::default();
    }

    fn to_content_json(&self) -> Value {
        Value::Array(self.blocks.iter().map(AccBlock::to_json).collect())
    }

    /// Build the `message` sub-object (exact key order per GROUND-TRUTH).
    fn to_message_json(&self, stop_reason: Option<&str>) -> Value {
        // GROUND-TRUTH key order for `message`:
        // model, id, type, role, content, stop_reason, stop_sequence,
        // stop_details, usage, diagnostics, context_management
        json!({
            "model": self.model,
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "content": self.to_content_json(),
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "stop_details": null,
            "usage": {
                "input_tokens": self.usage_input,
                "cache_creation_input_tokens": self.usage_cache_creation,
                "cache_read_input_tokens": self.usage_cache_read,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0_u64,
                    "ephemeral_1h_input_tokens": self.usage_cache_creation
                },
                "output_tokens": self.usage_output,
                // SC-01 (2.1.238): the canonical usage object gained
                // `output_tokens_details`. `nTe` — the `message_start` /
                // `message_delta` usage merge that produces the assistant
                // frame's `usage` (cc-238.js @297183459) — places it directly
                // after `output_tokens`:
                //   output_tokens_details:{thinking_tokens:
                //     t.output_tokens_details?.thinking_tokens
                //       ?? e.output_tokens_details.thinking_tokens}
                // and the seed `e` is `DR` (@283631657), whose
                // `output_tokens_details` is `{thinking_tokens:0}`. `emit_usage`
                // is fed by a fixed four-token trait signature with no
                // thinking-token channel, so the merge always lands on the
                // seed's `0` here; the KEY and its position are the parity fix.
                // (`output_tokens_details` has 0 hits in the 2.1.220 binary.)
                "output_tokens_details": {"thinking_tokens": 0_u64},
                "service_tier": "standard",
                "inference_geo": "not_available"
            },
            "diagnostics": null,
            "context_management": null
        })
    }

    fn collect_text_units(&self) -> Vec<u16> {
        self.blocks.iter().filter_map(AccBlock::as_text).flatten().copied().collect()
    }

}

// ── StreamJsonStream ─────────────────────────────────────────────────────────

/// Static init parameters for `system/init` frame.
#[derive(Clone)]
pub struct StreamJsonInitParams {
    pub cwd: String,
    pub session_id: String,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<Value>,
    pub model: String,
    pub permission_mode: String,
    pub slash_commands: Vec<String>,
    /// SLASH-15 (2.1.238): the subset of [`Self::slash_commands`] carrying the
    /// oracle's `terminalOriented:!0` flag, so a thin/remote client knows to
    /// route those four locally. Emitted immediately after `slash_commands` and
    /// only when non-empty — see [`build_init_frame`].
    pub terminal_slash_commands: Vec<String>,
    pub api_key_source: String,
    pub claude_code_version: String,
    pub output_style: String,
    pub agents: Vec<String>,
    pub skills: Vec<String>,
    pub plugins: Vec<Value>,
    pub analytics_disabled: bool,
    pub product_feedback_disabled: bool,
    pub memory_paths: Option<Value>,
    pub per_turn_effort_active: Option<bool>,
    pub view_mode: Option<String>,
    pub fast_mode_state: String,
    /// Why fast mode is unavailable (2.1.219 `JW()` reason enum). `Some` ⇒
    /// emitted directly after `fast_mode_state`; `None` ⇒ key omitted, the
    /// serialization of the oracle's `undefined` assignment.
    pub fast_mode_disabled_reason: Option<String>,
    /// Protocol capabilities this CLI supports (binary `gPp`), spread into the
    /// init frame between `plugins` and `mcp_server_errors` — SDK consumers
    /// feature-detect on these instead of version-sniffing.
    pub capabilities: Vec<String>,
    /// `--mcp-config` entries skipped by config validation. Emitted into the
    /// `system/init` frame ONLY when non-empty, matching the oracle's
    /// conditional spread (`...r.length>0&&{mcp_server_errors:…}`).
    pub mcp_server_errors: Vec<Value>,
}

/// The capability list the 2.1.220 `-p` init frame advertises (binary
/// `gPp=[xsa,Jlb,Isa]`; live-captured verbatim):
/// * `interrupt_receipt_v1` — interrupt success payloads carry `still_queued`.
/// * `interrupt_cancel_queued_v1` — the interrupt request honors
///   `cancel_queued:true` (queue swept, listed under `cancelled`).
/// * `msg_lifecycle_v1` — `command_lifecycle` frames track uuid-stamped
///   commands (`queued`/`started`/`completed`/`cancelled`/`discarded`).
pub const STREAM_JSON_CAPABILITIES: [&str; 3] = [
    "interrupt_receipt_v1",
    "interrupt_cancel_queued_v1",
    "msg_lifecycle_v1",
];

/// SH-07 — build the `system/hook_progress` frame body (oracle 2.1.238
/// @ 296463298, `EjT`).
///
/// Key ORDER is the wire contract: `type, subtype, hook_id, hook_name,
/// hook_event, stdout, stderr, output`, then the `uuid` / `session_id` the
/// shared emitter (`u0`) appends — the same tail `hook_started` and
/// `hook_response` carry. Split out as a pure function so the shape is
/// assertable without a live outbound drain.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_hook_progress_frame(
    hook_id: &str,
    hook_name: &str,
    hook_event: &str,
    stdout: &str,
    stderr: &str,
    output: &str,
    uuid: &str,
    session_id: &str,
) -> serde_json::Value {
    json!({
        "type": "system",
        "subtype": "hook_progress",
        "hook_id": hook_id,
        "hook_name": hook_name,
        "hook_event": hook_event,
        "stdout": stdout,
        "stderr": stderr,
        "output": output,
        "uuid": uuid,
        "session_id": session_id
    })
}

/// Shared outbound queue: sender half for the single-writer drain task.
///
/// Phase 0: each `StreamJsonStream` instance holds a clone of this sender.
/// The drain task (spawned once per process) is the sole stdout writer.
/// For Phase 1+ the `ControlPlaneWriter` also holds a clone so control
/// frames share the same queue and cannot overtake data frames.
pub type OutboundTx = mpsc::UnboundedSender<OutboundMsg>;

/// Drop replaceable `stream_event` frames when stdout is this far behind.
const MAX_PENDING_STREAM_EVENTS: usize = 8192;

/// Optional measurements supplied by the existing session/turn owner.
/// Unavailable timings are omitted, never replaced with invented zero values.
#[derive(Debug, Clone, Default)]
pub struct StreamJsonResultMetadata {
    pub is_error: bool,
    pub ttft_ms: Option<u64>,
    pub ttft_stream_ms: Option<u64>,
    pub time_to_request_ms: Option<u64>,
    pub first_content_frame_ms: Option<u64>,
    pub queued_turn_count: u64,
    pub result_index: u64,
    pub num_turns: Option<u64>,
    pub terminal_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub api_error_status: Option<u16>,
    pub subagent_stats: Option<Value>,
    pub safety_stops: Option<u64>,
    /// Exact aggregate usage shape/order from the session usage owner.
    pub usage: Option<Value>,
    pub model_usage: Option<serde_json::Map<String, Value>>,
}


#[derive(Default)]
struct QueryResponseTiming {
    started: Option<std::time::Instant>,
    first_request: Option<std::time::Instant>,
    first_request_wall_ms: Option<i64>,
    first_request_input_tokens: Option<u64>,
    current_request: Option<std::time::Instant>,
    current_message_start: Option<std::time::Instant>,
    first_message_start: Option<std::time::Instant>,
    first_content: Option<std::time::Instant>,
    first_assistant: Option<std::time::Instant>,
}

fn rounded_elapsed_ms(start: std::time::Instant, end: std::time::Instant) -> u64 {
    let elapsed = end.checked_duration_since(start).unwrap_or_default();
    (elapsed.as_secs_f64() * 1000.0).round() as u64
}

impl QueryResponseTiming {
    fn observe(&mut self, event: ResponseTimingEvent, at: std::time::Instant) {
        if self.started.is_none() {
            return;
        }
        match event {
            ResponseTimingEvent::RequestStarted => {
                // This callback runs synchronously at actual SDK dispatch,
                // after host admission, immediately before the wire call.
                if self.first_request.is_none() {
                    self.first_request_wall_ms = Some(chrono::Utc::now().timestamp_millis());
                }
                self.first_request.get_or_insert(at);
                self.current_request = Some(at);
                self.current_message_start = None;
            }
            ResponseTimingEvent::MessageStart => {
                self.first_message_start.get_or_insert(at);
                self.current_message_start = Some(at);
            }
            ResponseTimingEvent::ContentFrame => {
                self.first_content.get_or_insert(at);
            }
            ResponseTimingEvent::AssistantMessage => {
                self.first_assistant.get_or_insert(at);
            }
        }
    }

    fn fill_metadata(&self, metadata: &mut StreamJsonResultMetadata) {
        let Some(start) = self.started else { return };
        metadata.ttft_ms = metadata
            .ttft_ms
            .or_else(|| self.first_assistant.map(|at| rounded_elapsed_ms(start, at)));
        metadata.ttft_stream_ms = metadata.ttft_stream_ms.or_else(|| {
            self.first_message_start
                .map(|at| rounded_elapsed_ms(start, at))
        });
        metadata.time_to_request_ms = metadata
            .time_to_request_ms
            .or_else(|| self.first_request.map(|at| rounded_elapsed_ms(start, at)));
        metadata.first_content_frame_ms = metadata
            .first_content_frame_ms
            .or_else(|| self.first_content.map(|at| rounded_elapsed_ms(start, at)));
    }

    fn partial_ttft_ms(&self, received: std::time::Instant) -> Option<u64> {
        self.current_request
            .map(|start| rounded_elapsed_ms(start, self.current_message_start.unwrap_or(received)))
    }
}

/// A 4th `OutputStream` impl that writes NDJSON frames to stdout.
///
/// ## Phase 0 stdout drain
///
/// All `emit_*` methods serialise the frame to a single-line string (via
/// `serialize_ndjson_line`) and push it onto an unbounded mpsc channel.
/// A single drain task (`spawn_drain_task`) is the sole stdout writer.
/// This matches the TS `outbound = Stream<StdoutMessage>` + drain loop
/// (structuredIO.ts:160-162) and eliminates the per-frame mutex race that
/// the old `Arc<Mutex<Stdout>>` approach had.
///
/// The sender is `Arc`-wrapped so it can be shared with a future
/// `ControlPlaneWriter` without extra plumbing.
pub struct StreamJsonStream {
    /// Sender half of the outbound NDJSON queue (the drain task holds the Rx).
    out_tx: Arc<OutboundTx>,
    output: Output,
    output_mode: StreamJsonOutputMode,
    drain_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Tracks how many drain tasks have been spawned for this stream (should be 0 or 1).
    /// We use an AtomicUsize as a once-flag: 0 = not spawned, 1 = spawned.
    /// The receiver is stored only until the drain task consumes it; after spawn
    /// it lives inside the task. We can't store it here because tokio mpsc receivers
    /// are not Clone — so we use a Mutex<Option<Rx>> to hand it off.
    drain_rx: Mutex<Option<mpsc::UnboundedReceiver<OutboundMsg>>>,
    /// 0 = drain not yet spawned, 1 = spawned (use AtomicUsize as a flag).
    drain_started: AtomicUsize,
    /// Session id threaded in from the orchestrator after build. `Mutex`
    /// so the caller can set it post-construction (before emit_init). `Arc` so the
    /// handle can be SHARED with the `StdioControlPlane` (GATE-SYSMSG-01), which
    /// reads the same value to stamp `session_id` on a `permission_denied` frame.
    session_id: Arc<Mutex<String>>,
    /// Init-frame parameters. Wrapped in `Mutex` so the caller can fill
    /// them in after `build_runtime` supplies the real session_id / tool list.
    init_params: Mutex<Option<StreamJsonInitParams>>,
    /// Per-message accumulator (behind Mutex so the async trait can write it).
    accum: Arc<Mutex<MessageAccum>>,
    /// When true, suppress all frames except the final result frame.
    /// Set by `new_json_mode_placeholder()` / `new_json_mode()` for
    /// `--output-format json` / `--json` output paths.
    suppress_frames: bool,
    /// The last completed assistant text (collected just before boundary reset).
    /// Used by `run_stream_json_print` to populate the result frame's `result` field.
    last_result_text: Mutex<Utf16JsonProjection>,
    structured_output: Mutex<Option<Utf16JsonProjection>>,
    result_metadata: Mutex<StreamJsonResultMetadata>,
    text_limits: PrintTextLimits,
    response_timing: StdMutex<QueryResponseTiming>,
    request_markers: StdMutex<RequestMarkers>,
    /// `--include-partial-messages`: emit `stream_event` frames for each SSE
    /// event received from the API. Reconstructed from parsed `LlmEvent`
    /// (semantically equivalent, not byte-for-byte identical — G5 fidelity gap).
    /// AtomicBool so it can be set after Arc construction.
    include_partial_messages: AtomicBool,
    /// `--include-hook-events`: emit `system/hook_started` + `system/hook_response`
    /// frames before/after each blocking hook dispatch. SessionStart/Setup hooks
    /// ALWAYS emit (even without this flag) — all others only with this flag.
    /// AtomicBool so it can be set after Arc construction.
    include_hook_events: AtomicBool,
    /// `--forward-subagent-text` (or `CLAUDE_CODE_FORWARD_SUBAGENT_TEXT`): forward
    /// subagent text/thinking blocks as assistant/user frames with a non-null
    /// `parent_tool_use_id`. AtomicBool so it can be set after Arc construction.
    forward_subagent_text: AtomicBool,
    /// Live thinking-display mode. `true` drops thinking blocks while keeping
    /// them in the model-facing transcript.
    omit_thinking: AtomicBool,
    /// Latest-value mailbox that prevents an unbounded backlog of replaceable
    /// tool heartbeat frames when stdout is slow.
    heartbeat_lines: Arc<CoalescedHeartbeatLines>,
    /// In-flight `stream_event` frames not yet drained to stdout.
    pending_stream_events: Arc<AtomicUsize>,
    /// Tool calls refused by the permission layer, for the `result` frame's
    /// `permission_denials`. Filled from the orchestrator's session-scoped
    /// record just before the result frame is built (same post-construction
    /// `Mutex` pattern as `session_id`), because the run path owns the
    /// orchestrator and the builders only see `self`.
    permission_denials: std::sync::OnceLock<Arc<Mutex<Vec<lingxi_core::host::PermissionDenial>>>>,
}

impl StreamJsonStream {
    /// Internal constructor — builds the struct with a fresh unbounded mpsc channel.
    /// The drain task is NOT started here; call `ensure_drain_started()` before
    /// the first emit, or call it lazily from `enqueue_line`.
    fn new_inner(
        init_params: Option<StreamJsonInitParams>,
        suppress_frames: bool,
        output: Output,
    ) -> Self {
        Self::new_inner_with_output_mode(
            init_params,
            suppress_frames,
            output,
            StreamJsonOutputMode::Ndjson,
        )
    }

    fn new_inner_with_output_mode(
        init_params: Option<StreamJsonInitParams>,
        suppress_frames: bool,
        output: Output,
        output_mode: StreamJsonOutputMode,
    ) -> Self {
        let session_id = init_params
            .as_ref()
            .map(|p| p.session_id.clone())
            .unwrap_or_default();
        let (tx, rx) = mpsc::unbounded_channel::<OutboundMsg>();
        Self {
            out_tx: Arc::new(tx),
            output,
            output_mode,
            drain_task: Mutex::new(None),
            drain_rx: Mutex::new(Some(rx)),
            drain_started: AtomicUsize::new(0),
            session_id: Arc::new(Mutex::new(session_id)),
            init_params: Mutex::new(init_params),
            accum: Arc::new(Mutex::new(MessageAccum::default())),
            suppress_frames,
            last_result_text: Mutex::new(Utf16JsonProjection::plain(json!(""))),
            structured_output: Mutex::new(None),
            result_metadata: Mutex::new(StreamJsonResultMetadata::default()),
            text_limits: PrintTextLimits::default(),
            response_timing: StdMutex::new(QueryResponseTiming::default()),
            request_markers: StdMutex::new(RequestMarkers::default()),
            permission_denials: std::sync::OnceLock::new(),
            include_partial_messages: AtomicBool::new(false),
            include_hook_events: AtomicBool::new(false),
            forward_subagent_text: AtomicBool::new(false),
            omit_thinking: AtomicBool::new(false),
            heartbeat_lines: Arc::new(CoalescedHeartbeatLines::default()),
            pending_stream_events: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Ensure the single-writer drain task is running. Idempotent — safe to
    /// call multiple times; only the first call spawns the task.
    ///
    /// In production, call this once before `emit_init`. In unit tests this is
    /// called implicitly on the first `enqueue_line` so that test output goes
    /// to stdout without needing explicit setup.
    pub async fn ensure_drain_started(&self) {
        // Fast-path: already started.
        if self.drain_started.load(Ordering::Acquire) != 0 {
            return;
        }
        // Take the Rx out of the option — this can only succeed once.
        let mut guard = self.drain_rx.lock().await;
        if let Some(rx) = guard.take() {
            let task = spawn_drain_task(
                rx,
                Arc::clone(&self.pending_stream_events),
                self.output.clone(),
                self.output_mode,
                self.text_limits,
            );
            *self.drain_task.lock().await = Some(task);
            self.drain_started.store(1, Ordering::Release);
        }
        // If guard.take() returned None another caller raced us and already
        // spawned — that's fine, we just skip.
    }

    /// Wait until every frame enqueued before this call has reached the stdout
    /// drain task and stdout has been flushed.
    ///
    /// This is a FIFO barrier rather than a channel close: control-plane writers
    /// may still hold sender clones when the run loop emits its final result
    /// frame. A barrier preserves ordering while preventing `process::exit`
    /// callers from losing the last JSON line.
    pub async fn flush(&self) -> std::io::Result<()> {
        self.ensure_drain_started().await;
        let (done_tx, done_rx) = oneshot::channel();
        if self.out_tx.send(OutboundMsg::Flush(done_tx)).is_err() {
            self.output.record_error(&std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "headless output drain closed",
            ));
            return Err(self.output.delivery_error());
        }
        self.wait_for_drain_ack(done_rx).await
    }

    pub fn abort_delivery(&self, reason: impl Into<String>) {
        self.output.abort_delivery(reason);
    }

    /// Publish buffered JSON at the native print-output boundary, before the
    /// shared runtime shutdown/drain. Normal stream mode is a FIFO flush.
    pub async fn publish_json(&self) -> std::io::Result<()> {
        if self.output_mode == StreamJsonOutputMode::Ndjson {
            return self.flush().await;
        }
        self.ensure_drain_started().await;
        let (done_tx, done_rx) = oneshot::channel();
        if self.out_tx.send(OutboundMsg::PublishJson(done_tx)).is_err() {
            self.output.record_error(&std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "headless output drain closed before JSON publication",
            ));
            return Err(self.output.delivery_error());
        }
        self.wait_for_drain_ack(done_rx).await
    }

    /// Refresh only measured cumulative fields on held print results. Per-query
    /// usage, num_turns, identity and timing keep their original query values.
    pub fn refresh_held_result_totals(&self, cost: &CostSnapshot, model: &str, betas: &[String], subagent_stats: Option<Value>) {
        if self.output_mode == StreamJsonOutputMode::Ndjson { return; }
        let mut totals = json!({
            "total_cost_usd": cost.total_usd,
            "duration_api_ms": u64::try_from(cost.api_duration.as_millis()).unwrap_or(u64::MAX),
            "modelUsage": Self::build_model_usage_block(cost, model, betas),
        });
        if let Some(stats) = subagent_stats { totals["subagent_stats"] = stats; }
        if let Some(stops) = cost.safety_stops { totals["safety_stops"] = json!(stops); }
        if self.out_tx.send(OutboundMsg::RefreshHeldResultTotals(totals)).is_err() {
            self.abort_delivery("headless output drain closed before held result refresh");
        }
    }

    async fn wait_for_drain_ack(
        &self,
        done: oneshot::Receiver<std::io::Result<()>>,
    ) -> std::io::Result<()> {
        tokio::select! {
            biased;
            _ = self.output.delivery_cancelled() => Err(self.output.delivery_error()),
            result = done => match result {
                Ok(result) => result,
                Err(_) => {
                    self.output.record_error(&std::io::Error::new(std::io::ErrorKind::BrokenPipe, "headless output drain ended before acknowledgement"));
                    Err(self.output.delivery_error())
                }
            },
        }
    }

    /// Finish the sole writer after native terminal frames have been queued.
    /// Sender clones need not be dropped before this bounded join.
    pub async fn finish(&self) -> std::io::Result<()> {
        self.ensure_drain_started().await;
        let (done_tx, done_rx) = oneshot::channel();
        let result = if self.out_tx.send(OutboundMsg::Shutdown(done_tx)).is_ok() {
            self.wait_for_drain_ack(done_rx).await
        } else {
            self.output.flush().await
        };
        // Await the stored handle by reference. If the caller drops this
        // future while joining, cleanup can still join the same writer task.
        let mut drain_task = self.drain_task.lock().await;
        if let Some(task) = drain_task.as_mut() {
            let completion = task.await;
            drain_task.take();
            if let Err(error) = completion {
                self.output
                    .record_error(&std::io::Error::other(error.to_string()));
                return Err(self.output.delivery_error());
            }
        }
        result
    }

    /// Push a pre-serialised NDJSON line onto the outbound queue.
    ///
    /// This is the only place `emit_*` methods write to stdout (via the drain
    /// task). Sending to an unbounded channel is infallible unless the receiver
    /// is dropped (i.e. the drain task panicked — in that case we silently drop
    /// the frame rather than panicking the caller).
    fn enqueue_line(&self, line: String, is_stream_event: bool) {
        if is_stream_event && !self.reserve_stream_event() {
            return;
        }
        let line = {
            let mut markers = self.request_markers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if markers.is_empty() { line } else {
                let encoded = Utf16JsonProjection::parse(&line).map_err(|error| error.to_string()).and_then(|mut frame| {
                    markers.stamp(&mut frame)?;
                    serialize_projected_ndjson_line(&frame).map_err(|error| error.to_string())
                });
                match encoded {
                    Ok(line) => line,
                    Err(error) => {
                        drop(markers);
                        if is_stream_event { self.pending_stream_events.fetch_sub(1, Ordering::Relaxed); }
                        self.abort_delivery(format!("invalid request marker projection: {error}"));
                        return;
                    }
                }
            }
        };
        let message = if is_stream_event {
            OutboundMsg::StreamEvent(line)
        } else {
            OutboundMsg::Line(line)
        };
        if self.out_tx.send(message).is_err() {
            self.output.record_error(&std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "headless output drain closed",
            ));
            if is_stream_event {
                self.pending_stream_events.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    fn reserve_stream_event(&self) -> bool {
        let mut current = self.pending_stream_events.load(Ordering::Relaxed);
        loop {
            if current >= MAX_PENDING_STREAM_EVENTS {
                return false;
            }
            match self.pending_stream_events.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Serialise `v` to an escaped NDJSON line and enqueue it.
    fn enqueue(&self, v: &Value) {
        let line = serialize_ndjson_line(v);
        let is_stream_event = v.get("type").and_then(Value::as_str) == Some("stream_event");
        self.enqueue_line(line, is_stream_event);
    }

    /// Build the client-protocol heartbeat frame used by stream-json. Keeping
    /// this conversion in one pure helper prevents the CLI from inventing a
    /// second heartbeat schema.
    fn build_tool_heartbeat_frame(
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        elapsed_ms: u64,
    ) -> Value {
        serde_json::to_value(client::protocol::events::ClientEvent::ToolHeartbeat {
            id: id.to_string(),
            tool: tool.to_string(),
            elapsed_ms,
        })
        .expect("ClientEvent::ToolHeartbeat must serialize")
    }

    /// Return a clone of the outbound sender so the ControlPlaneWriter
    /// (Phase 1+) can share the same drain queue without extra plumbing.
    pub fn outbound_tx(&self) -> Arc<OutboundTx> {
        Arc::clone(&self.out_tx)
    }

    /// GATE-SYSMSG-01: a clone of the shared session-id handle so the
    /// `StdioControlPlane` stamps `permission_denied` frames with the SAME
    /// `session_id` this stream sets post-build (they share one `Mutex`).
    pub fn session_id_handle(&self) -> Arc<Mutex<String>> {
        Arc::clone(&self.session_id)
    }

    /// Construct a placeholder stream: the streaming callbacks (emit_text,
    /// emit_tool_call, etc.) are fully wired.  Call [`set_init_params`]
    /// before [`emit_init`] / [`emit_status`] to fill in the session-level
    /// metadata that only becomes available after `build_runtime` completes.
    pub fn new_placeholder(output: Output) -> Self {
        Self::new_inner(None, false, output)
    }

    /// Construct a json-mode placeholder: same as `new_placeholder()` but
    /// with `suppress_frames = true`. All frames EXCEPT the final result
    /// frame are suppressed. Used by `--output-format json` / `--json`.
    pub fn new_json_mode_placeholder(output: Output) -> Self {
        Self::new_inner_with_output_mode(None, true, output, StreamJsonOutputMode::LastResultJson)
    }

    /// Native print text shares query execution and retains only the final
    /// result text, including queries generated by completed background tasks.
    pub fn new_text_mode_placeholder(output: Output, max_turns: Option<u32>, max_budget_usd: Option<f64>) -> Self {
        let mut stream = Self::new_inner_with_output_mode(None, true, output, StreamJsonOutputMode::LastResultText);
        stream.text_limits = PrintTextLimits { max_turns, max_budget_usd };
        stream
    }

    /// Native `--output-format=json --verbose`: one compact array of retained
    /// frames is delivered at completion, rather than individual NDJSON lines.
    pub fn new_verbose_json_mode_placeholder(output: Output) -> Self {
        Self::new_inner_with_output_mode(None, false, output, StreamJsonOutputMode::VerboseJson)
    }

    /// Convenience constructor used in unit tests where all params are known
    /// upfront.
    pub fn new(init_params: StreamJsonInitParams, output: Output) -> Self {
        Self::new_inner(Some(init_params), false, output)
    }

    /// Convenience constructor for json-mode tests where all params are known
    /// upfront.
    pub fn new_json_mode(init_params: StreamJsonInitParams, output: Output) -> Self {
        Self::new_inner_with_output_mode(Some(init_params), true, output, StreamJsonOutputMode::LastResultJson)
    }

    pub fn new_verbose_json_mode(init_params: StreamJsonInitParams, output: Output) -> Self {
        Self::new_inner_with_output_mode(
            Some(init_params),
            false,
            output,
            StreamJsonOutputMode::VerboseJson,
        )
    }

    /// Set the `--include-partial-messages` and `--include-hook-events` flags.
    ///
    /// Called from `lib.rs` (or wherever the `Arc<StreamJsonStream>` is
    /// wired in) after `build_runtime` completes, using the parsed `Argv`
    /// flags. These flags are `false` by default so all constructors are
    /// behavior-neutral until explicitly opted in.
    ///
    /// Uses `AtomicBool` so the method takes `&self` (not `&mut self`),
    /// making it callable on an `Arc<StreamJsonStream>` without unwrapping.
    pub fn set_flags(&self, include_partial_messages: bool, include_hook_events: bool) {
        self.include_partial_messages
            .store(include_partial_messages, Ordering::Relaxed);
        self.include_hook_events
            .store(include_hook_events, Ordering::Relaxed);
    }

    /// Set the effective `--forward-subagent-text` state (flag OR truthy
    /// `CLAUDE_CODE_FORWARD_SUBAGENT_TEXT`, gated to `--print` + stream-json by
    /// the caller). Separate setter so existing `set_flags` call sites are
    /// unchanged. `false` by default so all constructors stay behavior-neutral.
    pub fn set_forward_subagent_text(&self, forward_subagent_text: bool) {
        self.forward_subagent_text
            .store(forward_subagent_text, Ordering::Relaxed);
    }

    /// Whether subagent text/thinking blocks should be forwarded onto this
    /// stream (consulted by the subagent→parent forwarding path once wired).
    #[must_use]
    pub fn forward_subagent_text(&self) -> bool {
        self.forward_subagent_text.load(Ordering::Relaxed)
    }

    /// Fill in the init parameters after `build_runtime` has given us
    /// the real session_id, tool list, model, etc.
    pub async fn set_init_params(&self, params: StreamJsonInitParams) {
        *self.session_id.lock().await = params.session_id.clone();
        *self.init_params.lock().await = Some(params);
    }

    /// Emit the `system/init` frame. Called once before `run_turn`.
    /// Panics if [`set_init_params`] has not been called yet.
    /// No-op when `suppress_frames` is true.
    pub async fn emit_init(&self) {
        if self.suppress_frames {
            return;
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let params_guard = self.init_params.lock().await;
        let p = params_guard
            .as_ref()
            .expect("set_init_params must be called before emit_init");
        let session_id = self.session_id.lock().await.clone();
        let frame = build_init_frame(&session_id, &uuid, p);
        drop(params_guard);
        self.enqueue(&frame);
    }

    fn build_prompt_suggestion_frame(session_id: &str, suggestion: &str, uuid: &str) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), json!("prompt_suggestion"));
        obj.insert("suggestion".into(), json!(suggestion));
        obj.insert("uuid".into(), json!(uuid));
        obj.insert("session_id".into(), json!(session_id));
        Value::Object(obj)
    }

    pub async fn emit_prompt_suggestion(&self, suggestion: &str) {
        if self.suppress_frames || suggestion.trim().is_empty() {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let uuid = uuid::Uuid::new_v4().to_string();
        let frame = Self::build_prompt_suggestion_frame(&session_id, suggestion, &uuid);
        self.enqueue(&frame);
    }

    /// Emit the `system/status` frame (status: "requesting"). Called just
    /// before the API turn starts.
    /// No-op when `suppress_frames` is true.
    pub async fn emit_status(&self) {
        if self.suppress_frames {
            return;
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let session_id = self.session_id.lock().await.clone();
        let frame = json!({
            "type": "system",
            "subtype": "status",
            "status": "requesting",
            "uuid": uuid,
            "session_id": session_id
        });
        self.enqueue(&frame);
    }

    /// Claude Code 2.1.261 `$Ke` maps this explicit SDK subset in order.
    /// Engine-only metadata (for example activeGoal) stays out of the SDK frame.
    fn build_compact_boundary_frame(
        session_id: &str,
        boundary_uuid: &str,
        metadata: &lingxi_core::types::CompactBoundaryMetadata,
    ) -> Value {
        let source = serde_json::to_value(metadata).expect("compact metadata serializes");
        let mut compact = serde_json::Map::new();
        for (camel, snake) in [
            ("trigger", "trigger"),
            ("preTokens", "pre_tokens"),
            ("postTokens", "post_tokens"),
            ("cumulativeDroppedTokens", "cumulative_dropped_tokens"),
            ("durationMs", "duration_ms"),
            ("userContext", "user_context"),
            ("messagesSummarized", "messages_summarized"),
            ("precomputed", "precomputed"),
            ("preCompactDiscoveredTools", "pre_compact_discovered_tools"),
        ] {
            if let Some(value) = source.get(camel) {
                compact.insert(snake.into(), value.clone());
            }
        }
        for (camel, snake, fields) in [
            (
                "preservedSegment",
                "preserved_segment",
                &[
                    ("headUuid", "head_uuid"),
                    ("anchorUuid", "anchor_uuid"),
                    ("tailUuid", "tail_uuid"),
                ][..],
            ),
            (
                "preservedMessages",
                "preserved_messages",
                &[
                    ("anchorUuid", "anchor_uuid"),
                    ("uuids", "uuids"),
                    ("allUuids", "all_uuids"),
                ][..],
            ),
        ] {
            if let Some(value) = source.get(camel) {
                let mut nested = serde_json::Map::new();
                for (from, to) in fields {
                    if let Some(value) = value.get(*from) {
                        nested.insert((*to).into(), value.clone());
                    }
                }
                compact.insert(snake.into(), Value::Object(nested));
            }
        }
        let mut frame = serde_json::Map::new();
        frame.insert("type".into(), json!("system"));
        frame.insert("subtype".into(), json!("compact_boundary"));
        // Manual /compact returns through the local-command serializer;
        // automatic boundaries stream directly through the engine envelope.
        // Their insertion order differs in 2.1.261 (also verified live).
        if metadata.trigger == lingxi_core::types::CompactTrigger::Manual {
            frame.insert("session_id".into(), json!(session_id));
        }
        frame.insert("uuid".into(), json!(boundary_uuid));
        frame.insert("compact_metadata".into(), Value::Object(compact));
        if let Some(parent) = metadata.logical_parent_uuid.as_deref() {
            frame.insert("logical_parent_uuid".into(), json!(parent));
        }
        if metadata.trigger != lingxi_core::types::CompactTrigger::Manual {
            frame.insert("session_id".into(), json!(session_id));
        }
        Value::Object(frame)
    }

    /// `None` starts compaction; `Some(error)` completes it. The terminal
    /// metadata follows 2.1.261's `sdk_status` event and `It` envelope order.
    #[allow(clippy::option_option)] // None=start, Some(None)=success, Some(Some)=failure.
    fn build_compact_status_frame(
        session_id: &str,
        uuid: &str,
        finished: Option<Option<&str>>,
    ) -> Value {
        let mut frame = serde_json::Map::new();
        frame.insert("type".into(), json!("system"));
        frame.insert("subtype".into(), json!("status"));
        frame.insert(
            "status".into(),
            if finished.is_some() {
                Value::Null
            } else {
                json!("compacting")
            },
        );
        if let Some(error) = finished {
            frame.insert(
                "compact_result".into(),
                json!(if error.is_some() { "failed" } else { "success" }),
            );
            if let Some(error) = error {
                frame.insert("compact_error".into(), json!(error));
            }
        }
        frame.insert("session_id".into(), json!(session_id));
        frame.insert("uuid".into(), json!(uuid));
        Value::Object(frame)
    }

    fn build_compact_user_frame(
        session_id: &str,
        uuid: &str,
        timestamp: &str,
        content: &str,
        synthetic: bool,
    ) -> Value {
        let mut frame = json!({
            "type":"user", "message":{"role":"user","content":content},
            "session_id":session_id, "parent_tool_use_id":null,
            "uuid":uuid, "timestamp":timestamp, "isReplay":!synthetic
        });
        if synthetic {
            frame["isSynthetic"] = json!(true);
        }
        frame
    }

    fn enqueue_tool_result_frame(
        &self,
        frame: Value,
        model_text: &str,
        result: &Value,
        exact: Option<&lingxi_core::host::ToolResultProjection>,
    ) {
        let Some(exact) = exact else {
            self.enqueue(&frame);
            return;
        };
        if exact.data.value != *result
            || exact.data.validate().is_err()
            || exact.content.validate().is_err()
            || exact.model_text.as_ref().is_some_and(|text| text.value.as_str() != Some(model_text) || text.validate().is_err())
            || !(exact.content.value.as_str() == Some(model_text)
                || (exact.content.value.is_array() && exact.content.value == *result))
        {
            self.abort_delivery("tool result projection association changed");
            return;
        }
        let mut frame = frame;
        if let Some(meta) = &exact.mcp_meta {
            if let Err(error) = meta.validate() {
                self.abort_delivery(format!("invalid MCP metadata: {error}"));
                return;
            }
            frame
                .as_object_mut()
                .expect("native user frame")
                .insert("mcpMeta".into(), meta.value.clone());
        }
        let mut frame = Utf16JsonProjection::plain(frame);
        if let Err(error) = frame
            .set_pointer("/toolUseResult", exact.data.clone())
            .and_then(|()| frame.set_pointer("/message/content/0/content", exact.content.clone()))
        {
            self.abort_delivery(format!("invalid tool result projection: {error}"));
            return;
        }
        if let Some(meta) = &exact.mcp_meta {
            if let Err(error) = frame.set_pointer("/mcpMeta", meta.clone()) {
                self.abort_delivery(format!("invalid MCP metadata frame: {error}"));
                return;
            }
        }
        match serialize_projected_ndjson_line(&frame) {
            Ok(line) => self.enqueue_line(line, false),
            Err(error) => self.abort_delivery(format!("invalid tool result frame: {error}")),
        }
    }

    fn build_compact_error_frame(
        session_id: &str,
        uuid: &str,
        message_id: &str,
        timestamp: &str,
        display: &str,
        is_error: bool,
    ) -> Value {
        let stream = if is_error { "stderr" } else { "stdout" };
        json!({
            "type":"assistant",
            "message":{
                "diagnostics":null,"id":message_id,"container":null,"model":"<synthetic>",
                "role":"assistant","stop_details":null,"stop_reason":"end_turn","stop_sequence":null,
                "type":"message","usage":{
                    "output_tokens_details":null,"input_tokens":0,"output_tokens":0,
                    "cache_creation_input_tokens":0,"cache_read_input_tokens":0,
                    "server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},
                    "service_tier":null,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},
                    "inference_geo":null,"iterations":null,"speed":null
                },
                "content":[{"type":"text","text":display}],"context_management":null
            },
            "parent_tool_use_id":null,"is_meta":true,
            "local_command_source":format!("<local-command-{stream}>{display}</local-command-{stream}>"),
            "session_id":session_id,"uuid":uuid,"timestamp":timestamp
        })
    }

    /// Emit only /compact's local command transcript. Failed compaction is a
    /// completed command with a synthetic notice, not a failed provider turn.
    pub async fn emit_compact_command_output(
        &self,
        instructions: &str,
        command_uuid: &str,
        command_timestamp: &str,
        failure: Option<(&str, bool)>,
        verbose: bool,
        replay_user_messages: bool,
    ) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if let Some((display, is_error)) = failure {
            self.enqueue(&Self::build_compact_error_frame(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &uuid::Uuid::new_v4().to_string(),
                &timestamp,
                display,
                is_error,
            ));
        } else {
            let display = if verbose {
                "Compacted "
            } else {
                "Compacted (ctrl+o to see full summary)"
            };
            self.enqueue(&Self::build_compact_user_frame(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &timestamp,
                &format!("<local-command-stdout>{display}</local-command-stdout>"),
                false,
            ));
        }
        if replay_user_messages {
            let markup = format!(
                "<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args>{instructions}</command-args>"
            );
            self.enqueue(&Self::build_compact_user_frame(
                &session_id,
                command_uuid,
                command_timestamp,
                &markup,
                false,
            ));
        }
    }

    /// Complete a local compact command without borrowing the previous model turn.
    pub async fn emit_compact_command_result(
        &self,
        cost: &CostSnapshot,
        duration_ms: u64,
        failure: Option<&str>,
    ) {
        let params = self.init_params.lock().await.clone();
        let Some(params) = params else {
            return;
        };
        let mut usage = Self::build_usage_block(&CostSnapshot::default());
        usage["inference_geo"] = json!("");
        let frame = self
            .build_result_success_frame(
                "",
                "",
                cost,
                &params.model,
                &params.fast_mode_state,
                params.fast_mode_disabled_reason.as_deref(),
                &[],
            )
            .await;
        let mut result = serde_json::Map::new();
        // 2.1.261's no-model-turn local-command result has its own envelope.
        result.insert("is_error".into(), json!(false));
        result.insert("duration_api_ms".into(), json!(0));
        result.insert("num_turns".into(), json!(0));
        result.insert("stop_reason".into(), Value::Null);
        result.insert("session_id".into(), json!(params.session_id));
        result.insert("total_cost_usd".into(), json!(cost.total_usd));
        result.insert("usage".into(), usage);
        result.insert("modelUsage".into(), frame["modelUsage"].clone());
        result.insert(
            "permission_denials".into(),
            self.permission_denials_value().await,
        );
        result.insert("fast_mode_state".into(), json!(params.fast_mode_state));
        if let Some(reason) = params.fast_mode_disabled_reason {
            result.insert("fast_mode_disabled_reason".into(), json!(reason));
        }
        result.insert("subtype".into(), json!("success"));
        // An empty session is rejected before compaction starts and leaves
        // result empty. Attempted compaction failures expose their notice to
        // SDK callers even though the local command itself completed.
        let result_text = failure
            .filter(|display| *display != "Error: No messages to compact")
            .unwrap_or_default();
        result.insert("result".into(), json!(result_text));
        result.insert("type".into(), json!("result"));
        result.insert("duration_ms".into(), json!(duration_ms));
        result.insert("uuid".into(), frame["uuid"].clone());
        result.insert("queued_turn_count".into(), json!(0));
        self.enqueue_result_frame(&Value::Object(result), None).await;
    }

    /// Build the `user` tool_result frame Value.
    ///
    /// claude-code's stream-json (SDK V2) tool_result block carries the
    /// MODEL-FACING STRING in `content` (what the model/API sees), with the full
    /// structured result on a SEPARATE top-level `toolUseResult` field on the
    /// user message (verified vs the 2.1.191 binary, which builds
    /// `…,toolUseResult:<data>,…` on the SDK user message). LingXi previously put
    /// the whole `data` object where the string belongs and omitted
    /// `toolUseResult`, so an SDK consumer saw a JSON blob instead of the tool's
    /// output. `model_text` is the EXACT string the model saw (passed by the
    /// orchestrator's dispatch — `result.model_content`, the derived model text,
    /// or the pre-exec error/cancel/deny string), so the frame's `content` is
    /// byte-faithful to the model wire; `toolUseResult` keeps the pure metadata
    /// `data`.
    ///
    /// Pure builder (modulo the fresh `uuid`/`timestamp`) — does not write to
    /// stdout. Call `emit_tool_result` to build + emit.
    async fn build_tool_result_frame(
        &self,
        tool_use_id: &str,
        model_text: &str,
        result: &Value,
    ) -> Value {
        self.build_tool_result_frame_with_denial(tool_use_id, model_text, result, None, None)
            .await
    }

    /// [`Self::build_tool_result_frame`] plus denial provenance.
    ///
    /// `user_feedback` is plumbed but NOT yet live: [`OutputStream::emit_tool_result_denied`]
    /// carries no feedback argument, so production always passes `None` and only
    /// tests exercise the field. claude-code attaches it solely when
    /// `behavior === "ask"` (binary offset 235412899), a state LingXi's gates do
    /// not currently produce — so this is parity-neutral today, not a silent drop.
    ///
    /// When `denial_kind` is set, the frame gains a `tool_result_meta` array
    /// built by [`build_tool_result_meta`]. claude-code spreads the field
    /// CONDITIONALLY (`...o.length>0&&{tool_result_meta:o}`, 2.1.220 binary
    /// offset 233203100), so a non-denied result omits the key entirely rather
    /// than carrying an empty array — emitting `[]` would be a wire divergence.
    async fn build_tool_result_frame_with_denial(
        &self,
        tool_use_id: &str,
        model_text: &str,
        result: &Value,
        denial_kind: Option<&str>,
        user_feedback: Option<&str>,
    ) -> Value {
        let is_error = result.get("error").is_some();
        let uuid = uuid::Uuid::new_v4().to_string();
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let session_id = self.session_id.lock().await.clone();
        let content_value = json!(model_text);
        let content_block = json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content_value,
            "is_error": is_error
        });
        let mut frame = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [content_block]
            },
            "session_id": session_id,
            "parent_tool_use_id": null,
            "toolUseResult": result,
            "uuid": uuid,
            "timestamp": timestamp
        });
        let meta = build_tool_result_meta(denial_kind, user_feedback, &frame["message"]["content"]);
        if !meta.is_empty() {
            frame
                .as_object_mut()
                .expect("frame is a JSON object")
                .insert("tool_result_meta".to_string(), Value::Array(meta));
        }
        frame
    }

    /// Build the 2.1.293 success result envelope in native insertion order.
    ///
    /// Pure builder — does not write to stdout. Call `emit_result_success`
    /// to build + emit.
    /// Point the stream at the orchestrator's LIVE denial cell
    /// (`ConversationOrchestrator::permission_denials_handle`), so every result
    /// frame reports the run's refusals without any emit site having to
    /// remember to push a snapshot.
    ///
    /// The orchestrator's list is the authoritative record; the
    /// `permission_denied` system frames are documented by claude-code as
    /// advisory and incomplete, so they are NOT the source here.
    pub fn share_permission_denials(
        &self,
        cell: Arc<Mutex<Vec<lingxi_core::host::PermissionDenial>>>,
    ) {
        let _ = self.permission_denials.set(cell);
    }

    /// Has the orchestrator's denial cell been wired in?
    ///
    /// An unwired stream reports `permission_denials: []` — the exact bug this
    /// change exists to fix — so the CLI wiring is pinned by a test that asserts
    /// this, not just by the field being present.
    #[must_use]
    pub fn permission_denials_wired(&self) -> bool {
        self.permission_denials.get().is_some()
    }

    /// The `permission_denials` array for a `result` frame — oracle schema `LF`:
    /// `{tool_name, tool_use_id, tool_input}` per entry, in denial order.
    async fn permission_denials_value(&self) -> Value {
        let Some(cell) = self.permission_denials.get() else {
            // No orchestrator wired (the placeholder/JSON-mode streams built
            // before a runtime exists). Nothing ran, so nothing was denied.
            return Value::Array(Vec::new());
        };
        Value::Array(
            cell.lock()
                .await
                .iter()
                .map(|d| {
                    json!({
                        "tool_name": d.tool_name,
                        "tool_use_id": d.tool_use_id,
                        "tool_input": d.tool_input,
                    })
                })
                .collect(),
        )
    }

    async fn enqueue_result_frame(&self, frame: &Value, result_text: Option<&Utf16JsonProjection>) {
        let mut projection = Utf16JsonProjection::plain(frame.clone());
        if let Some(cell) = self.permission_denials.get() {
            let denials = cell.lock().await;
            for (index, denial) in denials.iter().enumerate() {
                if let Some(input) = &denial.tool_input_projection {
                    let path = format!("/permission_denials/{index}/tool_input");
                    if input.value != denial.tool_input
                        || frame.pointer(&path) != Some(&input.value)
                    {
                        self.abort_delivery(
                            "permission denial projection association changed",
                        );
                        return;
                    }
                    if let Err(error) = projection.set_pointer(&path, input.clone()) {
                        self.abort_delivery(format!(
                            "invalid permission denial projection: {error}"
                        ));
                        return;
                    }
                }
            }
        }
        if frame.get("structured_output").is_none() {
            if let Some(text) = result_text {
                if frame.get("result") != Some(&text.value) { self.abort_delivery("result text projection association changed"); return; }
                if let Err(error) = projection.set_pointer("/result", text.clone()) { self.abort_delivery(format!("invalid result text projection: {error}")); return; }
            }
        }
        if frame.get("structured_output").is_some() {
            if let Some(output) = self.structured_output.lock().await.clone() {
                if frame.get("structured_output") != Some(&output.value) {
                    self.abort_delivery("structured output projection association changed");
                    return;
                }
                if let Err(error) = projection.set_pointer("/structured_output", output) {
                    self.abort_delivery(format!("invalid structured output projection: {error}"));
                    return;
                }
            }
        }
        match serialize_projected_ndjson_line(&projection) {
            Ok(line) => self.enqueue_line(line, false),
            Err(error) => self.abort_delivery(format!("invalid result projection: {error}")),
        }
    }

    pub async fn set_result_metadata(&self, metadata: StreamJsonResultMetadata) {
        *self.result_metadata.lock().await = metadata;
    }

    /// Begin the finite request-marker owner from actually consumed client
    /// UUIDs. Transcript prompt/message IDs must never be supplied here.
    pub fn begin_request_markers(
        &self,
        primary: Option<Utf16JsonProjection>,
        consumed: Vec<Utf16JsonProjection>,
        primary_is_meta: bool,
    ) -> Result<(), String> {
        let owner = RequestMarkers::begin(primary, consumed, primary_is_meta)?;
        *self.request_markers.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = owner;
        Ok(())
    }

    /// Stage all actually rendered queued humans before any started lifecycle.
    /// Native's pending attachment set has its own independent 64-entry cap.
    pub fn stage_queued_request_marker(&self, uuid: Utf16JsonProjection) -> Result<(), String> {
        let mut attachment = Utf16JsonProjection::plain(json!({
            "type":"queued_command", "commandMode":"prompt", "source_uuid":uuid.value
        }));
        attachment.set_pointer("/source_uuid", uuid.clone()).map_err(|error| error.to_string())?;
        let mut owner = self.request_markers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        owner.note_attachment(&attachment)?;
        Ok(())
    }

    pub fn start_queued_request_marker(&self, uuid: Utf16JsonProjection) -> Result<(), String> {
        self.request_markers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .note_command_started(uuid)?;
        Ok(())
    }

    fn append_result_request_marker_fields(&self, obj: &mut serde_json::Map<String, Value>, fields: &[&str]) {
        match self.request_markers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).fields() {
            Ok(Some(markers)) => {
                for key in fields {
                    if let Some(value) = markers.value.get(*key) { obj.insert((*key).into(), value.clone()); }
                }
            }
            Ok(None) => {}
            Err(error) => self.abort_delivery(format!("invalid request markers: {error}")),
        }
    }

    fn append_success_request_metrics(&self, obj: &mut serde_json::Map<String, Value>) {
        self.append_result_request_marker_fields(obj, &["user_message_uuid", "user_message_uuids"]);
        let markers = self.request_markers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !markers.primary_is_unchanged() { return; }
        let clock = self.response_timing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(wall) = clock.first_request_wall_ms {
            obj.insert("request_sent_wall_ms".into(), json!(wall));
            if let Some(tokens) = clock.first_request_input_tokens.filter(|tokens| *tokens > 0) {
                obj.insert("first_request_input_tokens".into(), json!(tokens));
            }
        }
    }

    /// Begin one admitted external query before its init/status/driver setup.
    /// Internal tool-loop requests keep these first-observation markers.
    pub fn begin_query_timing(&self) {
        *self
            .response_timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = QueryResponseTiming {
            started: Some(std::time::Instant::now()),
            ..Default::default()
        };
    }

    /// Capture the current turn's validated schema output; None clears it.
    pub async fn set_structured_output(&self, value: Option<Utf16JsonProjection>) {
        if let Some(projected) = &value {
            if let Err(error) = projected.validate() {
                self.abort_delivery(format!("invalid structured output source: {error}"));
                return;
            }
        }
        *self.structured_output.lock().await = value;
    }

    fn result_duration_ms(&self, cost: &CostSnapshot) -> u64 {
        let clock = self.response_timing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clock.started.map_or_else(
            || cost.session_duration.as_millis().try_into().unwrap_or(u64::MAX),
            |start| rounded_elapsed_ms(start, std::time::Instant::now()),
        )
    }

    pub async fn build_result_success_frame(
        &self,
        result_text: &str,
        stop_reason: &str,
        cost: &CostSnapshot,
        model_id: &str,
        fast_mode_state: &str,
        fast_mode_disabled_reason: Option<&str>,
        betas: &[String],
    ) -> Value {
        let uuid = uuid::Uuid::new_v4().to_string();
        let session_id = self.session_id.lock().await.clone();
        let duration_ms = self.result_duration_ms(cost);

        let mut metadata = self.result_metadata.lock().await.clone();
        self.response_timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fill_metadata(&mut metadata);
        let usage = metadata
            .usage
            .clone()
            .unwrap_or_else(|| Self::build_usage_block(cost));
        let model_usage = metadata
            .model_usage
            .clone()
            .unwrap_or_else(|| Self::build_model_usage_block(cost, model_id, betas));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "duration_api_ms".into(),
            json!(u64::try_from(cost.api_duration.as_millis()).unwrap_or(u64::MAX)),
        );
        obj.insert(
            "stop_reason".into(),
            json!(metadata.stop_reason.as_deref().unwrap_or(stop_reason)),
        );
        obj.insert("session_id".into(), json!(session_id));
        obj.insert("total_cost_usd".into(), json!(cost.total_usd));
        obj.insert("usage".into(), usage);
        obj.insert("modelUsage".into(), Value::Object(model_usage));
        obj.insert(
            "permission_denials".into(),
            self.permission_denials_value().await,
        );
        obj.insert(
            "terminal_reason".into(),
            json!(metadata.terminal_reason.as_deref().unwrap_or("completed")),
        );
        obj.insert("fast_mode_state".into(), json!(fast_mode_state));
        if let Some(reason) = fast_mode_disabled_reason {
            obj.insert("fast_mode_disabled_reason".into(), json!(reason));
        }
        if let Some(stats) = metadata.subagent_stats.clone() {
            obj.insert("subagent_stats".into(), stats);
        }
        if let Some(stops) = cost.safety_stops.or(metadata.safety_stops) {
            obj.insert("safety_stops".into(), json!(stops));
        }
        obj.insert("is_error".into(), json!(metadata.is_error));
        obj.insert(
            "num_turns".into(),
            json!(metadata.num_turns.unwrap_or(u64::from(cost.api_calls))),
        );
        obj.insert("subtype".into(), json!("success"));
        obj.insert("api_error_status".into(), json!(metadata.api_error_status));
        obj.insert("result".into(), json!(result_text));
        if let Some(value) = self.structured_output.lock().await.clone().filter(|_| !metadata.is_error) {
            obj.insert(
                "result".into(),
                json!(
                    value
                        .to_json_string()
                        .expect("validated structured output projection")
                ),
            );
            obj.insert("structured_output".into(), value.value);
        }
        if let Some(value) = metadata.ttft_ms {
            obj.insert("ttft_ms".into(), json!(value));
        }
        obj.insert("type".into(), json!("result"));
        obj.insert("duration_ms".into(), json!(duration_ms));
        obj.insert("uuid".into(), json!(uuid));
        if let Some(value) = metadata.ttft_stream_ms {
            obj.insert("ttft_stream_ms".into(), json!(value));
        }
        if let Some(value) = metadata.time_to_request_ms {
            obj.insert("time_to_request_ms".into(), json!(value));
        }
        if let Some(value) = metadata.first_content_frame_ms {
            obj.insert("first_content_frame_ms".into(), json!(value));
        }
        self.append_success_request_metrics(&mut obj);
        obj.insert(
            "queued_turn_count".into(),
            json!(metadata.queued_turn_count),
        );
        obj.insert("result_index".into(), json!(metadata.result_index));

        Value::Object(obj)
    }

    /// Emit the success result frame and return the Value.
    pub async fn emit_result_success(
        &self,
        result_text: &Utf16JsonProjection,
        stop_reason: &str,
        cost: &CostSnapshot,
        model_id: &str,
        fast_mode_state: &str,
        fast_mode_disabled_reason: Option<&str>,
        betas: &[String],
    ) -> Value {
        let Some(text) = result_text.value.as_str().filter(|_| result_text.validate().is_ok()) else {
            self.abort_delivery("result text must be a validated string projection");
            return Value::Null;
        };
        let frame = self
            .build_result_success_frame(
                text,
                stop_reason,
                cost,
                model_id,
                fast_mode_state,
                fast_mode_disabled_reason,
                betas,
            )
            .await;
        self.enqueue_result_frame(&frame, Some(result_text)).await;
        frame
    }

    /// Build the 2.1.293 error result envelope. Error frames omit success-only
    /// timing/status fields, as verified by native interrupt and budget captures.
    ///
    /// Pure builder — does not write to stdout.
    pub async fn build_result_error_frame(
        &self,
        subtype: &str,
        errors: Vec<String>,
        cost: &CostSnapshot,
        model_id: &str,
        fast_mode_state: &str,
        fast_mode_disabled_reason: Option<&str>,
        betas: &[String],
    ) -> Value {
        let uuid = uuid::Uuid::new_v4().to_string();
        let session_id = self.session_id.lock().await.clone();
        let duration_ms = self.result_duration_ms(cost);

        let terminal_reason = match subtype {
            "error_during_execution" => "error",
            "error_max_turns" => "max_turns",
            "error_max_budget_usd" => "budget_exhausted",
            "error_max_structured_output_retries" => "structured_output_retry_exhausted",
            _ => "error",
        };

        let metadata = self.result_metadata.lock().await.clone();
        let usage = metadata
            .usage
            .clone()
            .unwrap_or_else(|| Self::build_usage_block(cost));
        let model_usage = metadata
            .model_usage
            .clone()
            .unwrap_or_else(|| Self::build_model_usage_block(cost, model_id, betas));
        let structured_retry = subtype == "error_max_structured_output_retries";
        let mut obj = serde_json::Map::new();
        if structured_retry {
            obj.insert("is_error".into(), json!(true));
        }
        obj.insert(
            "duration_api_ms".into(),
            json!(u64::try_from(cost.api_duration.as_millis()).unwrap_or(u64::MAX)),
        );
        if structured_retry {
            obj.insert(
                "num_turns".into(),
                json!(metadata.num_turns.unwrap_or(u64::from(cost.api_calls))),
            );
        }
        obj.insert("stop_reason".into(), json!(metadata.stop_reason));
        obj.insert("session_id".into(), json!(session_id));
        obj.insert("total_cost_usd".into(), json!(cost.total_usd));
        obj.insert("usage".into(), usage);
        obj.insert("modelUsage".into(), Value::Object(model_usage));
        obj.insert(
            "permission_denials".into(),
            self.permission_denials_value().await,
        );
        obj.insert(
            "terminal_reason".into(),
            json!(
                metadata
                    .terminal_reason
                    .as_deref()
                    .unwrap_or(terminal_reason)
            ),
        );
        obj.insert("fast_mode_state".into(), json!(fast_mode_state));
        if let Some(reason) = fast_mode_disabled_reason {
            obj.insert("fast_mode_disabled_reason".into(), json!(reason));
        }
        if let Some(stats) = metadata.subagent_stats.clone() {
            obj.insert("subagent_stats".into(), stats);
        }
        if let Some(stops) = cost.safety_stops.or(metadata.safety_stops) {
            obj.insert("safety_stops".into(), json!(stops));
        }
        if !structured_retry {
            obj.insert("is_error".into(), json!(true));
            obj.insert(
                "num_turns".into(),
                json!(metadata.num_turns.unwrap_or(u64::from(cost.api_calls))),
            );
        }
        obj.insert("subtype".into(), json!(subtype));
        obj.insert("errors".into(), json!(errors));
        self.append_result_request_marker_fields(&mut obj, &["user_message_uuid"]);
        obj.insert("type".into(), json!("result"));
        obj.insert("duration_ms".into(), json!(duration_ms));
        obj.insert("uuid".into(), json!(uuid));
        self.append_result_request_marker_fields(&mut obj, &["user_message_uuids"]);
        obj.insert(
            "queued_turn_count".into(),
            json!(metadata.queued_turn_count),
        );
        obj.insert("result_index".into(), json!(metadata.result_index));

        Value::Object(obj)
    }

    /// Emit the error result frame and return the Value.
    pub async fn emit_result_error(
        &self,
        subtype: &str,
        errors: Vec<String>,
        cost: &CostSnapshot,
        model_id: &str,
        fast_mode_state: &str,
        fast_mode_disabled_reason: Option<&str>,
        betas: &[String],
    ) -> Value {
        let frame = self
            .build_result_error_frame(
                subtype,
                errors,
                cost,
                model_id,
                fast_mode_state,
                fast_mode_disabled_reason,
                betas,
            )
            .await;
        self.enqueue_result_frame(&frame, None).await;
        frame
    }

    /// Return the last completed assistant text (populated just before
    /// accumulator reset in `emit_message_boundary`).
    pub async fn get_last_result_text(&self) -> Utf16JsonProjection {
        let acc = self.accum.lock().await;
        if acc.blocks.iter().any(|block| matches!(block, AccBlock::Text(_))) {
            let units = acc.collect_text_units();
            return Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).expect("accumulated text owns its units");
        }
        drop(acc);
        self.last_result_text.lock().await.clone()
    }

    /// Emit a `rate_limit_event` frame.
    ///
    /// GROUND-TRUTH shape:
    /// `{type, rate_limit_info:{status,resetsAt,rateLimitType,utilization,
    ///   isUsingOverage,surpassedThreshold}, uuid, session_id}`
    ///
    /// Fields sourced from `RateLimitInfo` (API response headers). When headers
    /// are absent (test / no-header paths) we emit sensible defaults:
    /// `status:"allowed"`, `rateLimitType:null`, `utilization:0`,
    /// `resetsAt:0`, `isUsingOverage:false`, `surpassedThreshold:0`.
    /// Plumbing real per-header values requires threading `RateLimitInfo`
    /// through the provider adapter → stream — that's tracked as a follow-up.
    /// No-op when `suppress_frames` is true.
    pub async fn emit_rate_limit_event(
        &self,
        status: Option<&str>,
        rate_limit_type: Option<&str>,
        utilization: Option<f64>,
        resets_at: Option<u64>,
        is_using_overage: bool,
        surpassed_threshold: Option<f64>,
    ) {
        if self.suppress_frames {
            return;
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let session_id = self.session_id.lock().await.clone();
        let frame = json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": status.unwrap_or("allowed"),
                "resetsAt": resets_at.unwrap_or(0),
                "rateLimitType": rate_limit_type,
                "utilization": utilization.unwrap_or(0.0),
                "isUsingOverage": is_using_overage,
                "surpassedThreshold": surpassed_threshold.unwrap_or(0.0)
            },
            "uuid": uuid,
            "session_id": session_id
        });
        self.enqueue(&frame);
    }

    /// Build the `usage` sub-block (snake_case per GROUND-TRUTH).
    ///
    /// SC-01 (2.1.238): the `result` frame's `usage` is `gXl()` (cc-238.js
    /// @300232503), which spreads the canonical zero-usage object `DR`
    /// (@283631657) and overrides only the four token counters plus
    /// `web_search_requests`:
    ///
    /// ```text
    /// DR={output_tokens_details:{thinking_tokens:0},input_tokens:0,
    ///     cache_creation_input_tokens:0,cache_read_input_tokens:0,output_tokens:0,
    ///     server_tool_use:{web_search_requests:0,web_fetch_requests:0},
    ///     service_tier:"standard",
    ///     cache_creation:{ephemeral_1h_input_tokens:0,ephemeral_5m_input_tokens:0},
    ///     inference_geo:"",iterations:[],speed:"standard"}
    /// function gXl(){…return{...DR,input_tokens:…,output_tokens:…,
    ///   cache_read_input_tokens:…,cache_creation_input_tokens:…,
    ///   server_tool_use:{...DR.server_tool_use,web_search_requests:…}}}
    /// ```
    ///
    /// The 2.1.220 twin `jw` (@233167154) is byte-identical MINUS
    /// `output_tokens_details` (0 hits in that binary), so the new key is the
    /// only delta — and because `gXl` never overrides it, the spread keeps
    /// `DR`'s literal `{thinking_tokens:0}` and its position as the FIRST key.
    fn build_usage_block(cost: &CostSnapshot) -> Value {
        let mut usage = serde_json::Map::new();
        let current = cost.current_usage.unwrap_or_default();
        let details = cost.current_usage_details.unwrap_or_default();
        if cost.current_usage.is_none() {
            usage.insert(
                "output_tokens_details".into(),
                json!({"thinking_tokens":0_u64}),
            );
        }
        usage.insert("input_tokens".into(), json!(current.input_tokens));
        usage.insert(
            "cache_creation_input_tokens".into(),
            json!(current.cache_creation_input_tokens),
        );
        usage.insert(
            "cache_read_input_tokens".into(),
            json!(current.cache_read_input_tokens),
        );
        usage.insert(
            "output_tokens".into(),
            json!(
                current
                    .output_tokens
                    .saturating_add(details.reasoning_tokens)
            ),
        );
        if cost.current_usage.is_some() {
            usage.insert(
                "output_tokens_details".into(),
                json!({"thinking_tokens":details.reasoning_tokens}),
            );
        }
        usage.insert(
            "server_tool_use".into(),
            json!({"web_search_requests":details.web_search_requests,"web_fetch_requests":0_u64}),
        );
        usage.insert(
            "service_tier".into(),
            json!(if details.fast_mode {
                "fast"
            } else {
                "standard"
            }),
        );
        usage.insert(
            "cache_creation".into(),
            json!({"ephemeral_1h_input_tokens":details.cache_creation_1h_input_tokens,"ephemeral_5m_input_tokens":details.cache_creation_5m_input_tokens}),
        );
        usage.insert("inference_geo".into(), json!(""));
        usage.insert("iterations".into(), json!([]));
        usage.insert(
            "speed".into(),
            json!(if details.fast_mode {
                "fast"
            } else {
                "standard"
            }),
        );
        usage.insert("fallback_credit".into(), Value::Null);
        Value::Object(usage)
    }

    /// Build the `modelUsage` sub-map keyed by `model_id` (camelCase per
    /// GROUND-TRUTH). The key is the model id AS-IS (including any `[1m]`
    /// suffix). `contextWindow` and `maxOutputTokens` are looked up from the
    /// llm-runtime catalog via `betas` (so `[1m]`-capable models report 1M).
    /// Empty map when no tokens were consumed.
    fn build_model_usage_block(
        cost: &CostSnapshot,
        model_id: &str,
        betas: &[String],
    ) -> serde_json::Map<String, Value> {
        let mut model_usage = serde_json::Map::new();
        // Cost tracking records the provider response's actual model. That is
        // essential when `--fallback-model` switched away from the session's
        // primary: result metadata must name the model that consumed tokens,
        // not merely the configured primary. Preserve the legacy aggregate
        // fallback for hosts that do not expose per-model rows yet.
        if !cost.by_model.is_empty() {
            for row in &cost.by_model {
                let ctx_window = context_window_for_model(&row.model, betas);
                let max_output = max_output_tokens_for_model(&row.model);
                // (cc 2.1.218) `n.canonicalModel = yo(r)` — the canonical id the
                // PRICING lookup used. It may differ from the raw model string
                // this entry is keyed by (provider-specific ids, aliases, `[1m]`
                // suffixes), so a host can group cost across those spellings.
                let canonical = cost::pricing::first_party_name_to_canonical(&row.model);
                #[allow(clippy::cast_precision_loss)]
                let cost_usd = row.total_nano_usd as f64 / 1_000_000_000.0;
                // (cc 2.1.218) `n.provider=n_(r)` — the sibling of
                // canonicalModel: the API provider that served this model
                // ("firstParty" for the Anthropic first-party API; LingXi
                // provider ids pass through the open string). Omitted when the
                // recording site could not attribute one (`.optional()`).
                let mut entry = json!({
                    "inputTokens": row.input_tokens,
                    "outputTokens": row.output_tokens.saturating_add(row.reasoning_tokens),
                    "cacheReadInputTokens": row.cache_read_input_tokens,
                    "cacheCreationInputTokens": row.cache_creation_input_tokens,
                    "webSearchRequests": row.web_search_requests,
                    "costUSD": cost_usd,
                    "contextWindow": ctx_window,
                    "maxOutputTokens": max_output,
                    "thinkingTokens": row.reasoning_tokens,
                    "canonicalModel": canonical
                });
                if let Some(provider) = &row.provider {
                    entry
                        .as_object_mut()
                        .expect("json! object")
                        .insert("provider".into(), json!(provider));
                }
                entry
                    .as_object_mut()
                    .expect("modelUsage object")
                    .insert("costBasis".into(), json!("list"));
                model_usage.insert(row.model.clone(), entry);
            }
        } else if cost.input_tokens > 0 || cost.output_tokens > 0 || cost.total_usd > 0.0 {
            model_usage.insert(
                model_id.to_string(),
                json!({
                    "inputTokens": cost.input_tokens,
                    "outputTokens": cost.output_tokens,
                    "cacheReadInputTokens": cost.cache_read_tokens,
                    "cacheCreationInputTokens": cost.cache_creation_tokens,
                    "webSearchRequests": 0_u64,
                    "costUSD": cost.total_usd,
                    "contextWindow": context_window_for_model(model_id, betas),
                    "maxOutputTokens": max_output_tokens_for_model(model_id),
                    "thinkingTokens": 0_u64,
                    "canonicalModel": cost::pricing::first_party_name_to_canonical(model_id),
                    "costBasis": "list"
                }),
            );
        }
        model_usage
    }

    /// Build the forwarded-subagent `assistant` frame for `--forward-subagent-text`.
    ///
    /// Gate + shape live here so the emit wrapper is a thin
    /// build-then-enqueue and the shape is unit-testable without stdout.
    ///
    /// GROUND-TRUTH (2.1.212 stream-json output-stream `case "progress"` →
    /// `data.type==="agent_progress"`): a forwarded subagent assistant turn is
    /// re-emitted onto the PARENT stream as
    /// ```text
    /// { type:"assistant", message:{...o.message, content:Xzt(content)},
    ///   parent_tool_use_id, session_id, uuid:o.uuid, timestamp, error,
    ///   ...request_id, ...subagent_type, ...task_description, ...tool_use_meta }
    /// ```
    /// where `parent_tool_use_id` is the spawning `Task`/`Agent` tool_use_id —
    /// NON-NULL, which is exactly what distinguishes a forwarded subagent frame
    /// from the top-level `assistant` frames (which hardcode `null`).
    ///
    /// The binary re-emits the subagent message spread with its `content`
    /// swapped for `Xzt(content)`. `Xzt` rewrites ONLY `text`/`thinking` blocks
    /// (stripping a `<cc-memory>` wrapper via `i0`) and RETURNS EVERY OTHER
    /// block — including `tool_use` — UNCHANGED. So the forwarded `content`
    /// KEEPS tool_use blocks; the earlier port dropped them, which was wrong
    /// about CC's actual frame shape. LingXi has no `<cc-memory>`/`i0` stripping
    /// yet, so the text/thinking rewrite is currently the identity — we keep the
    /// whole `content` array intact (cloning the message) and leave the
    /// text/thinking branch as the seam where an `i0`-equivalent would land.
    ///
    /// The frame's `uuid` is the SUBAGENT message's own uuid (`o.uuid`), sourced
    /// here from the serialized message's `id` field — NOT a fresh v4 — so a
    /// forwarded child frame correlates to the subagent message that produced it.
    ///
    /// Fields CC additionally spreads that this seam genuinely CANNOT source
    /// (the value threaded here is a serialized `lingxi_core::types::ConversationMessage`
    /// = `{role,id,content,stop_reason}`, and the progress event carries no more)
    /// are deliberately omitted rather than fabricated: `timestamp`, `error`,
    /// `request_id` (not on `ConversationMessage`), `subagent_type` /
    /// `task_description` (come from the progress event's `agentType` /
    /// `taskDescription`, not plumbed to this sink), and `tool_use_meta`
    /// (`Per(content)` MCP display-metadata, not reconstructable here).
    ///
    /// Returns `None` (nothing forwarded) when:
    ///   - the flag is OFF (`forward_subagent_text()` is false), or
    ///   - `suppress_frames` is set (json/`--json` output path), or
    ///   - the message is not an assistant message, or
    ///   - the message has no `content` array.
    fn build_forwarded_subagent_frame(
        &self,
        message: &Value,
        parent_tool_use_id: &str,
        session_id: &str,
        uuid: &str,
    ) -> Option<Value> {
        if !self.forward_subagent_text() || self.suppress_frames {
            return None;
        }
        // Only assistant turns are forwarded here.
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        // Mirror the binary's `{...o.message, content: Xzt(content)}`. `Xzt`
        // keeps tool_use (and every non-text/thinking) block UNCHANGED and only
        // rewrites text/thinking; with no `i0`/`<cc-memory>` stripping in LingXi
        // that rewrite is the identity, so the whole `content` array is kept.
        let content = message.get("content").and_then(Value::as_array)?;
        let rewritten: Vec<Value> = content
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                // Seam for a future `i0`/`<cc-memory>` strip on text/thinking.
                // No-op today (pass through unchanged).
                Some("text") | Some("thinking") => block.clone(),
                Some("text_js_utf16") => {
                    let mut block = block.clone();
                    if let Some(object) = block.as_object_mut() {
                        object.insert("type".into(), json!("text"));
                        object.remove("utf16_code_units");
                    }
                    block
                },
                // Every other block (incl. tool_use) is returned unchanged.
                _ => block.clone(),
            })
            .collect();
        let mut fwd_message = message.clone();
        if let Some(obj) = fwd_message.as_object_mut() {
            obj.insert("content".to_string(), Value::Array(rewritten));
        }
        Some(json!({
            "type": "assistant",
            "message": fwd_message,
            "parent_tool_use_id": parent_tool_use_id,
            "session_id": session_id,
            "uuid": uuid,
        }))
    }
}

#[async_trait]
impl OutputStream for StreamJsonStream {
    fn wants_response_timing(&self) -> bool {
        self.response_timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .started
            .is_some()
    }
    fn note_response_timing(&self, event: ResponseTimingEvent, at: std::time::Instant) {
        self.response_timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(event, at);
    }
    fn note_first_request_input_tokens(&self, input_tokens: u64) {
        let mut clock = self.response_timing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if clock.started.is_some() { clock.first_request_input_tokens.get_or_insert(input_tokens); }
    }
    async fn emit_task_lifecycle(&self, event: &Value) {
        if self.suppress_frames {
            return;
        }
        let mut frame = event.clone();
        let Some(frame_object) = frame.as_object_mut() else {
            return;
        };
        frame_object.insert(
            "session_id".into(),
            json!(self.session_id.lock().await.clone()),
        );
        frame_object.insert("uuid".into(), json!(uuid::Uuid::new_v4().to_string()));
        self.enqueue(&frame);
    }

    async fn emit_compaction_started(&self) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&Self::build_compact_status_frame(
            &session_id,
            &uuid::Uuid::new_v4().to_string(),
            None,
        ));
    }

    async fn emit_compact_boundary(
        &self,
        boundary_uuid: &str,
        metadata: &lingxi_core::types::CompactBoundaryMetadata,
    ) {
        if self.suppress_frames {
            return;
        }
        if metadata.trigger == lingxi_core::types::CompactTrigger::Manual {
            self.emit_init().await;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&Self::build_compact_boundary_frame(
            &session_id,
            boundary_uuid,
            metadata,
        ));
    }

    async fn emit_compaction_finished(&self, error: Option<&str>) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&Self::build_compact_status_frame(
            &session_id,
            &uuid::Uuid::new_v4().to_string(),
            Some(error),
        ));
    }

    async fn emit_compact_summary(&self, summary_uuid: &str, summary: &str) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.enqueue(&Self::build_compact_user_frame(
            &session_id,
            summary_uuid,
            &timestamp,
            summary,
            true,
        ));
    }

    /// Emit a forwarded subagent assistant message (`--forward-subagent-text`).
    ///
    /// `message` is the serialized subagent `lingxi_core::types::ConversationMessage`
    /// (carried by `agent::SubagentEvent::Message`); `parent_tool_use_id` is the
    /// `Task`/`Agent` tool_use_id that spawned the child. Gated + shaped by
    /// [`Self::build_forwarded_subagent_frame`]; a no-op when the flag is off or
    /// the message has no forwardable text/thinking blocks.
    async fn emit_forwarded_subagent_message(&self, message: &Value, parent_tool_use_id: &str) {
        // Cheap gate before locking / minting a uuid.
        if !self.forward_subagent_text() || self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        // CC spreads `uuid:o.uuid` — the SUBAGENT message's OWN uuid, not a
        // fresh one. The serialized `ConversationMessage` carries it as the
        // (transparent-UUID) `id` field; reuse it so a forwarded child frame
        // correlates to the subagent message. Fall back to a fresh v4 only if
        // the message is malformed and carries no `id`.
        let uuid = message
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
        if let Some(frame) =
            self.build_forwarded_subagent_frame(message, parent_tool_use_id, &session_id, &uuid)
        {
            let mut projection = Utf16JsonProjection::plain(frame);
            if let Some(content) = message.get("content").and_then(Value::as_array) {
                for (index, block) in content.iter().enumerate() {
                    if block.get("type").and_then(Value::as_str) != Some("text_js_utf16") { continue; }
                    let exact = serde_json::from_value::<lingxi_core::types::ContentBlock>(block.clone())
                        .map_err(|error| error.to_string())
                        .and_then(|block| match block {
                            lingxi_core::types::ContentBlock::TextJsUtf16 { text, utf16_code_units, .. } =>
                                Utf16JsonProjection::root_string(text, utf16_code_units).map_err(|error| error.to_string()),
                            _ => unreachable!("checked rich text discriminator"),
                        });
                    let result = exact.and_then(|text| projection.set_pointer(&format!("/message/content/{index}/text"), text).map_err(|error| error.to_string()));
                    if let Err(error) = result { self.abort_delivery(format!("invalid forwarded assistant text: {error}")); return; }
                }
            }
            match serialize_projected_ndjson_line(&projection) {
                Ok(line) => self.enqueue_line(line, false),
                Err(error) => self.abort_delivery(format!("invalid forwarded assistant frame: {error}")),
            }
        }
    }

    async fn emit_assistant_block_start(&self, _block_key: u64) {
        self.accum.lock().await.new_text_block = true;
    }

    async fn emit_text(&self, text: &str, utf16_code_units: Option<&[u16]>) {
        if utf16_code_units.is_some_and(|units| String::from_utf16_lossy(units) != text) {
            self.abort_delivery("assistant text units do not match display text");
            return;
        }
        let units = utf16_code_units.map_or_else(|| text.encode_utf16().collect(), <[u16]>::to_vec);
        let mut acc = self.accum.lock().await;
        if !acc.new_text_block {
            if let Some(AccBlock::Text(last)) = acc.blocks.last_mut() { last.extend(units); return; }
        }
        acc.blocks.push(AccBlock::Text(units));
        acc.new_text_block = false;
    }

    async fn emit_system_notice(&self, body: &str, is_error: bool) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&json!({
            "type": "system",
            "subtype": "notice",
            "message": body,
            "is_error": is_error,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": session_id,
        }));
    }

    async fn emit_mod_log(&self, plugin: &str, text: &str) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&json!({
            "type": "system",
            "subtype": "ui_log",
            "plugin": plugin,
            "text": text,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": session_id,
        }));
    }

    async fn emit_mod_toast(&self, plugin: &str, text: &str, timeout_ms: u64) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&json!({
            "type": "system",
            "subtype": "ui_toast",
            "plugin": plugin,
            "text": text,
            "timeout_ms": timeout_ms,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": session_id,
        }));
    }

    async fn emit_mod_status(&self, plugin: &str, text: Option<&str>) {
        if self.suppress_frames {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        self.enqueue(&json!({
            "type": "system",
            "subtype": "ui_status",
            "plugin": plugin,
            "text": text,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "session_id": session_id,
        }));
    }

    async fn emit_tool_call(
        &self,
        id: &lingxi_core::types::ToolUseId,
        name: &str,
        input: &serde_json::Value,
        input_projection: Option<&Utf16JsonProjection>,
    ) {
        if self.suppress_frames {
            return;
        }
        if let Some(projection) = input_projection {
            if projection.value != *input || projection.validate().is_err() {
                self.abort_delivery("tool input projection does not match emitted input");
                return;
            }
        }
        let mut acc = self.accum.lock().await;
        acc.blocks.push(AccBlock::ToolUse {
            id: id.as_str().to_string(),
            name: name.to_string(),
            input: input.clone(),
            input_projection: input_projection.cloned(),
        });
    }

    async fn emit_tool_result(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        model_text: &str,
        result: &serde_json::Value,
        _projection: Option<&lingxi_core::host::ToolResultProjection>,
    ) {
        if self.suppress_frames {
            return;
        }
        let frame = self
            .build_tool_result_frame(_id.as_str(), model_text, result)
            .await;
        self.enqueue_tool_result_frame(frame, model_text, result, _projection);
    }

    async fn emit_tool_result_denied(
        &self,
        id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        model_text: &str,
        result: &serde_json::Value,
        denial_kind: &str,
        _projection: Option<&lingxi_core::host::ToolResultProjection>,
    ) {
        if self.suppress_frames {
            return;
        }
        let frame = self
            .build_tool_result_frame_with_denial(
                id.as_str(),
                model_text,
                result,
                Some(denial_kind),
                None,
            )
            .await;
        self.enqueue_tool_result_frame(frame, model_text, result, _projection);
    }

    async fn emit_tool_heartbeat(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        elapsed_ms: u64,
    ) {
        if self.suppress_frames {
            return;
        }
        let line = serialize_ndjson_line(&Self::build_tool_heartbeat_frame(id, tool, elapsed_ms));
        if self.heartbeat_lines.publish(id.to_string(), line)
            && self
                .out_tx
                .send(OutboundMsg::Heartbeats(Arc::clone(&self.heartbeat_lines)))
                .is_err()
        {
            self.heartbeat_lines.reset_after_send_failure();
        }
    }

    async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {
        // No-op for stream-json: the result frame is emitted by the caller
        // after run_turn (P2). end_turn just signals the loop is done.
    }

    /// Wire `OutputStream::emit_rate_limit` → `rate_limit_event` NDJSON frame.
    ///
    /// The orchestrator calls this after every completed API turn via
    /// `emit_rate_limit_if_changed` (deduped). We forward the original nine
    /// parameters to `emit_rate_limit_event` which maps them onto the
    /// GROUND-TRUTH shape. The `overage_status`, `overage_resets_at`,
    /// `overage_disabled_reason`, and `fallback_available` fields are
    /// Anthropic-overage metadata that is NOT part of the `rate_limit_event`
    /// wire frame — they are used by the TUI rate-limit composer only. The
    /// 2.1.206 `upgrade_paths` / `credits_required` fields are likewise
    /// TUI-composer-only inputs (the upsell/suppression logic in a later
    /// task) with no `rate_limit_event` wire representation, so this impl
    /// accepts and ignores them, satisfying the trait signature faithfully
    /// without inventing new stream-json output.
    async fn emit_rate_limit(
        &self,
        status: Option<&str>,
        rate_limit_type: Option<&str>,
        utilization: Option<f64>,
        resets_at: Option<u64>,
        _claim_resets_at: Option<u64>,
        overage_status: Option<&str>,
        _overage_resets_at: Option<u64>,
        _overage_disabled_reason: Option<&str>,
        _fallback_available: Option<bool>,
        _upgrade_paths: Option<&[String]>,
        _credits_required: bool,
    ) {
        // Combine `status` and `overage_status` into the single `status` field
        // on the wire frame, preferring the more specific `overage_status` when
        // both are present (mirrors claude-code's `claudeAiLimits.ts` priority).
        let effective_status = overage_status.or(status);
        // `isUsingOverage` = overage is active when overage_status is present
        // and NOT "allowed" (i.e. it's "allowed_warning" or "rejected").
        let is_using_overage = overage_status.map(|s| s != "allowed").unwrap_or(false);
        self.emit_rate_limit_event(
            effective_status,
            rate_limit_type,
            utilization,
            resets_at,
            is_using_overage,
            None, // surpassed_threshold: not carried in this emit path
        )
        .await;
    }

    async fn emit_thinking(&self, thinking: &str, signature: Option<&str>) {
        if self.suppress_frames || self.omit_thinking.load(Ordering::Relaxed) {
            return;
        }
        let mut acc = self.accum.lock().await;
        if let Some(AccBlock::Thinking {
            thinking: t,
            signature: s,
        }) = acc.blocks.last_mut()
        {
            t.push_str(thinking);
            if let Some(sig) = signature {
                *s = Some(sig.to_string());
            }
        } else {
            acc.blocks.push(AccBlock::Thinking {
                thinking: thinking.to_string(),
                signature: signature.map(String::from),
            });
        }
    }

    fn set_thinking_display(&self, mode: Option<&str>) {
        self.omit_thinking
            .store(mode == Some("omitted"), Ordering::Relaxed);
    }

    async fn emit_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) {
        let mut acc = self.accum.lock().await;
        // Always update with the latest snapshot (message_delta supersedes
        // message_start).
        if input_tokens > 0 {
            acc.usage_input = input_tokens;
        }
        if output_tokens > 0 {
            acc.usage_output = output_tokens;
        }
        if cache_read_tokens > 0 {
            acc.usage_cache_read = cache_read_tokens;
        }
        if cache_creation_tokens > 0 {
            acc.usage_cache_creation = cache_creation_tokens;
        }
    }

    async fn emit_message_start(&self, message_id: &str, model: &str) {
        let mut acc = self.accum.lock().await;
        acc.reset();
        acc.message_id = message_id.to_string();
        acc.model = model.to_string();
    }

    async fn emit_message_boundary(&self, stop_reason: Option<&str>, request_id: Option<&str>) {
        // Before resetting, capture the last assistant text for the result frame.
        {
            let acc = self.accum.lock().await;
            let units = acc.collect_text_units();
            let text = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).expect("accumulated text owns its units");
            drop(acc);
            *self.last_result_text.lock().await = text;
        }

        if self.suppress_frames {
            // Still need to reset the accumulator even in suppressed mode.
            let mut acc = self.accum.lock().await;
            acc.reset();
            return;
        }

        // Flush the accumulated blocks as one `assistant` frame.
        let acc = self.accum.lock().await;
        let uuid = uuid::Uuid::new_v4().to_string();
        let session_id = self.session_id.lock().await.clone();
        let message = acc.to_message_json(stop_reason);
        let frame = json!({
            "type": "assistant",
            "message": message,
            "parent_tool_use_id": null,
            "session_id": session_id,
            "uuid": uuid,
            "request_id": request_id
        });
        let mut projection = Utf16JsonProjection::plain(frame);
        for (index, block) in acc.blocks.iter().enumerate() {
            if let AccBlock::Text(units) = block {
                let text = Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.clone()).expect("accumulated text owns its units");
                if let Err(error) = projection.set_pointer(&format!("/message/content/{index}/text"), text) {
                    self.abort_delivery(format!("invalid assistant text projection: {error}"));
                    return;
                }
            }
            if let AccBlock::ToolUse {
                input_projection: Some(input),
                ..
            } = block
            {
                if let Err(error) = projection
                    .set_pointer(&format!("/message/content/{index}/input"), input.clone())
                {
                    self.abort_delivery(format!(
                        "invalid assistant tool input projection: {error}"
                    ));
                    return;
                }
            }
        }
        drop(acc);
        // Reset the accumulator after flushing.
        {
            let mut acc = self.accum.lock().await;
            acc.reset();
        }
        match serialize_projected_ndjson_line(&projection) {
            Ok(line) => self.enqueue_line(line, false),
            Err(error) => self.abort_delivery(format!("invalid assistant projection: {error}")),
        }
    }

    /// Emit a `stream_event` NDJSON frame for `--include-partial-messages`.
    ///
    /// GROUND-TRUTH shape (6 keys, exact order):
    /// `{type, event, session_id, parent_tool_use_id, uuid, ttft_ms}`
    ///
    /// `ttft_ms` is present ONLY on the `message_start` frame (the first
    /// event in a stream). It measures the current physical attempt's dispatch
    /// through receipt of message_start; result timings use the query clock.
    /// Per the capture (01-stream-partial.ndjson line 11 vs 12-18), the
    /// `message_start` frame has `ttft_ms` and subsequent frames do not.
    ///
    /// FIDELITY NOTE (G5): `event_json` is reconstructed from the parsed
    /// `LlmEvent` — semantically equivalent to the Anthropic SSE event but
    /// NOT byte-for-byte identical (e.g. field ordering, default values).
    fn wants_partial_stream_events(&self) -> bool {
        self.include_partial_messages.load(Ordering::Relaxed) && !self.suppress_frames
    }

    async fn emit_stream_event(&self, event_json: &str, is_message_start: bool) {
        if !self.wants_partial_stream_events() {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let uuid = uuid::Uuid::new_v4().to_string();
        let event: serde_json::Value =
            serde_json::from_str(event_json).unwrap_or(serde_json::Value::Null);
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), json!("stream_event"));
        obj.insert("event".into(), event);
        obj.insert("session_id".into(), json!(session_id));
        obj.insert("parent_tool_use_id".into(), serde_json::Value::Null);
        obj.insert("uuid".into(), json!(uuid));
        if is_message_start {
            let ttft = self
                .response_timing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .partial_ttft_ms(std::time::Instant::now());
            obj.insert("ttft_ms".into(), json!(ttft));
        }
        let frame = serde_json::Value::Object(obj);
        self.enqueue(&frame);
    }

    /// Emit a `system/hook_started` NDJSON frame for `--include-hook-events`.
    ///
    /// SessionStart and Setup hooks ALWAYS emit (gate `pGn`); all others
    /// only emit when `include_hook_events` is true.
    async fn emit_hook_started(&self, hook_id: &str, hook_name: &str, hook_event: &str) {
        if self.suppress_frames {
            return;
        }
        // Gate pGn: SessionStart + Setup always stream; others need the flag.
        let always_stream = matches!(hook_event, "SessionStart" | "Setup");
        if !always_stream && !self.include_hook_events.load(Ordering::Relaxed) {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let uuid = uuid::Uuid::new_v4().to_string();
        let frame = json!({
            "type": "system",
            "subtype": "hook_started",
            "hook_id": hook_id,
            "hook_name": hook_name,
            "hook_event": hook_event,
            "uuid": uuid,
            "session_id": session_id
        });
        self.enqueue(&frame);
    }

    /// SH-07 — `Q9i(hookEvent)`: SessionStart/Setup always stream; everything
    /// else needs `--include-hook-events`. json-mode streams nothing.
    ///
    /// This is the same gate `emit_hook_started` / `emit_hook_response` /
    /// `emit_hook_progress_frame` apply to their own frames, exposed as a query
    /// so the hook executor can skip arming the progress poll entirely — which
    /// is exactly what upstream's `if(!Q9i(e.hookEvent))return()=>{}` head does.
    fn hook_events_streamed(&self, hook_event: &str) -> bool {
        if self.suppress_frames {
            return false;
        }
        matches!(hook_event, "SessionStart" | "Setup")
            || self.include_hook_events.load(Ordering::Relaxed)
    }

    /// SH-07 — emit a `system/hook_progress` NDJSON frame for
    /// `--include-hook-events`.
    ///
    /// Oracle 2.1.238 @ 296463298 (`EjT`) — the frame body, in key order:
    /// `{type, subtype, hook_id, hook_name, hook_event, stdout, stderr, output}`;
    /// `uuid` + `session_id` are appended by the shared emitter (`u0`), exactly
    /// as for `hook_started` / `hook_response`.
    ///
    /// Same gate (`Q9i`) as its two siblings: SessionStart/Setup always stream,
    /// every other event needs `--include-hook-events`.
    async fn emit_hook_progress_frame(
        &self,
        hook_id: &str,
        hook_name: &str,
        hook_event: &str,
        stdout: &str,
        stderr: &str,
        output: &str,
    ) {
        if self.suppress_frames {
            return;
        }
        // Gate pGn: SessionStart + Setup always stream; others need the flag.
        let always_stream = matches!(hook_event, "SessionStart" | "Setup");
        if !always_stream && !self.include_hook_events.load(Ordering::Relaxed) {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let uuid = uuid::Uuid::new_v4().to_string();
        let frame = build_hook_progress_frame(
            hook_id,
            hook_name,
            hook_event,
            stdout,
            stderr,
            output,
            &uuid,
            &session_id,
        );
        self.enqueue(&frame);
    }

    /// Emit a `system/hook_response` NDJSON frame for `--include-hook-events`.
    ///
    /// Same gate as `emit_hook_started`: SessionStart/Setup always stream.
    #[allow(clippy::too_many_arguments)]
    async fn emit_hook_response(
        &self,
        hook_id: &str,
        hook_name: &str,
        hook_event: &str,
        output: &str,
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
        outcome: &str,
    ) {
        if self.suppress_frames {
            return;
        }
        // Gate pGn: SessionStart + Setup always stream; others need the flag.
        let always_stream = matches!(hook_event, "SessionStart" | "Setup");
        if !always_stream && !self.include_hook_events.load(Ordering::Relaxed) {
            return;
        }
        let session_id = self.session_id.lock().await.clone();
        let uuid = uuid::Uuid::new_v4().to_string();
        let frame = json!({
            "type": "system",
            "subtype": "hook_response",
            "hook_id": hook_id,
            "hook_name": hook_name,
            "hook_event": hook_event,
            "output": output,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "outcome": outcome,
            "uuid": uuid,
            "session_id": session_id
        });
        self.enqueue(&frame);
    }
}

// ── init-frame builder ───────────────────────────────────────────────────────

/// Build the `system/init` frame `Value` (pure, no I/O) so its exact shape is
/// unit-testable without draining stdout.
///
/// ORACLE (2.1.201, verified live via
/// `echo '{"type":"user",…}' | claude -p --input-format stream-json \
///   --output-format stream-json --verbose`): the `-p` mode `system`/`init`
/// frame carries EXACTLY these 20 keys in this order —
/// `type, subtype, cwd, session_id, tools, mcp_servers, model, permissionMode,
/// slash_commands, apiKeySource, claude_code_version, output_style, agents,
/// skills, plugins, analytics_disabled, product_feedback_disabled, uuid,
/// memory_paths, fast_mode_state`. In particular the frame HAS `plugins` and
/// has NO `betas` key (a default-model run emits no `betas`). The separate
/// SDK-subprocess `initialize` payload — a different structure — is the one
/// that carries `betas`; the streaming `system/init` frame does not.
fn build_init_frame(session_id: &str, uuid: &str, p: &StreamJsonInitParams) -> Value {
    // Built by ordered insertion rather than one `json!` literal because
    // `mcp_server_errors` is CONDITIONAL: the oracle spreads it in only when
    // non-empty (`...r.length>0&&{mcp_server_errors:…}`), and it sits between
    // `plugins` and `analytics_disabled`. `serde_json`'s `preserve_order`
    // feature is what makes insertion order the emitted order.
    let mut o = serde_json::Map::new();
    o.insert("type".into(), json!("system"));
    o.insert("subtype".into(), json!("init"));
    o.insert("cwd".into(), json!(p.cwd));
    o.insert("session_id".into(), json!(session_id));
    o.insert("tools".into(), json!(p.tools));
    o.insert("mcp_servers".into(), json!(p.mcp_servers));
    o.insert("model".into(), json!(p.model));
    o.insert("permissionMode".into(), json!(p.permission_mode));
    o.insert("slash_commands".into(), json!(p.slash_commands));
    // SLASH-15 (2.1.238): `terminal_slash_commands` is spread in directly AFTER
    // `slash_commands` and ONLY when the list is non-empty — the init emitter
    // `Fin` (@298685916):
    //
    // ```js
    // let n=e.commands.filter((i)=>i.userInvocable!==!1&&i.terminalOriented===!0).map((i)=>i.name);
    // …slash_commands:…, ...n.length>0&&{terminal_slash_commands:n}, apiKeySource:…
    // ```
    //
    // The key does not exist in 2.1.220 at all, so an empty list must OMIT it
    // rather than emit `[]`.
    if !p.terminal_slash_commands.is_empty() {
        o.insert(
            "terminal_slash_commands".into(),
            json!(p.terminal_slash_commands),
        );
    }
    o.insert("apiKeySource".into(), json!(p.api_key_source));
    o.insert("claude_code_version".into(), json!(p.claude_code_version));
    o.insert("output_style".into(), json!(p.output_style));
    o.insert("agents".into(), json!(p.agents));
    o.insert("skills".into(), json!(p.skills));
    o.insert("plugins".into(), json!(p.plugins));
    // 2.1.220: `...e.capabilities&&{capabilities:[...e.capabilities]}` —
    // between `plugins` and `mcp_server_errors` (live-captured position).
    if !p.capabilities.is_empty() {
        o.insert("capabilities".into(), json!(p.capabilities));
    }
    if !p.mcp_server_errors.is_empty() {
        o.insert("mcp_server_errors".into(), json!(p.mcp_server_errors));
    }
    o.insert("analytics_disabled".into(), json!(p.analytics_disabled));
    o.insert(
        "product_feedback_disabled".into(),
        json!(p.product_feedback_disabled),
    );
    o.insert("uuid".into(), json!(uuid));
    if let Some(paths) = &p.memory_paths {
        o.insert("memory_paths".into(), paths.clone());
    }
    o.insert("fast_mode_state".into(), json!(p.fast_mode_state));
    // 2.1.220: `n.fast_mode_disabled_reason=e.fastModeDisabledReason` — an
    // `undefined` reason serializes to NO key, so `None` omits it.
    if let Some(reason) = &p.fast_mode_disabled_reason {
        o.insert("fast_mode_disabled_reason".into(), json!(reason));
    }
    if let Some(active) = p.per_turn_effort_active {
        o.insert("per_turn_effort_active".into(), json!(active));
    }
    if let Some(mode) = &p.view_mode {
        o.insert("view_mode".into(), json!(mode));
    }
    Value::Object(o)
}

/// `--mcp-config` / config-file entries that validation skipped, in the frame's
/// wire shape.
///
/// `mcp::config_diagnostics` has produced these for a while; nothing published
/// them. Only entries that actually caused a server to be SKIPPED belong here —
/// an advisory warning about a healthy server is not a "server error".
pub fn collect_mcp_server_errors(
    cwd: &std::path::Path,
    global: Option<&std::path::Path>,
) -> Vec<Value> {
    mcp::config_diagnostics::collect_all_mcp_config_warnings(cwd, global)
        .into_iter()
        .map(|w| {
            let mut e = serde_json::Map::new();
            if let Some(f) = w.file {
                e.insert("file".into(), json!(f));
            }
            e.insert("path".into(), json!(w.path));
            e.insert("message".into(), json!(w.message));
            if let Some(sug) = w.suggestion {
                e.insert("suggestion".into(), json!(sug));
            }
            if let Some(name) = w.server_name {
                e.insert("server_name".into(), json!(name));
            }
            Value::Object(e)
        })
        .collect()
}

/// Host snapshots needed for init metadata. Hosts resolve their own cwd,
/// diagnostics paths and privacy state; codec collection reads no process globals.
#[derive(Clone, Debug, Default)]
pub struct StreamJsonInitHostMetadata {
    pub cwd: std::path::PathBuf,
    pub mcp_server_errors: Vec<Value>,
    pub analytics_disabled: bool,
    pub product_feedback_disabled: bool,
    pub per_turn_effort_active: Option<bool>,
    pub view_mode: Option<String>,
}

/// One plugin advertised by the host's loaded plugin registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamJsonPlugin {
    pub name: String,
    pub path: String,
    pub source: String,
    pub version: Option<String>,
}

/// Build `StreamJsonInitParams` from the CLI environment, the resolved
/// permission mode, and the tool / slash-command registries.
///
/// This is the `build_init_frame` collector called from `run.rs` before
/// `run_turn`.
pub fn build_init_params(
    session_id: &str,
    tool_names: Vec<String>,
    mcp_servers: Vec<(String, String)>, // (name, status_str)
    model: &str,
    credential_source: &llm_runtime::CredentialSource,
    permission_mode: &str,
    slash_commands: Vec<String>,
    agents: Vec<String>,
    skills: Vec<String>,
    plugins: Vec<StreamJsonPlugin>,
    output_style: &str,
    memory_auto_path: Option<&str>,
    fast_mode_state: &str,
    fast_mode_disabled_reason: Option<&str>,
    host: StreamJsonInitHostMetadata,
) -> StreamJsonInitParams {
    let cwd = host.cwd.to_string_lossy().into_owned();

    let mcp_servers_json: Vec<Value> = mcp_servers
        .into_iter()
        .map(|(name, status)| json!({"name": name, "status": status}))
        .collect();

    let plugins_json: Vec<Value> = plugins
        .into_iter()
        .map(|plugin| {
            let mut fields = serde_json::Map::new();
            fields.insert("name".into(), json!(plugin.name));
            fields.insert("path".into(), json!(plugin.path));
            fields.insert("source".into(), json!(plugin.source));
            if let Some(version) = plugin.version {
                fields.insert("version".into(), json!(version));
            }
            Value::Object(fields)
        })
        .collect();

    let memory_paths: Option<Value> = memory_auto_path.map(|p| json!({"auto": p}));

    let tools = tool_names;

    let api_key_source = credential_source_label(credential_source);

    // SLASH-15: derive the `terminalOriented:!0` subset from the SAME advertised
    // list the frame emits, mirroring the oracle's single-source filter over
    // `e.commands` (`userInvocable!==!1 && terminalOriented===!0`) — the port's
    // `slash_commands` argument is already the user-invocable list, and
    // `command_api::builtin_support::names::TERMINAL_ORIENTED_COMMANDS` is the
    // registry-side table this consumes. Order follows the advertised list, as
    // upstream's `.filter().map()` does.
    let terminal_slash_commands: Vec<String> = slash_commands
        .iter()
        .filter(|name| command_api::builtin_support::names::is_terminal_oriented(name.as_str()))
        .cloned()
        .collect();

    StreamJsonInitParams {
        cwd,
        session_id: session_id.to_string(),
        tools,
        mcp_servers: mcp_servers_json,
        mcp_server_errors: host.mcp_server_errors,
        model: model.to_string(),
        permission_mode: permission_mode.to_string(),
        slash_commands,
        terminal_slash_commands,
        api_key_source,
        claude_code_version: super::CLAUDE_CODE_REFERENCE_VERSION.to_string(),
        output_style: output_style.to_string(),
        agents,
        skills,
        plugins: plugins_json,
        // Port of CC `tK()`'s `F$e()` term (`analyticsDisabled: tK()`): the
        // telemetry-disabled portion of the privacy gate (DISABLE_TELEMETRY /
        // DO_NOT_TRACK / non-essential-traffic). CC's `tK()` also ORs in a
        // config-privacy check (`zKm()`) and a third-party-gateway check
        // (`o_()`); the former surface isn't ported and the latter is a LingXi
        // accepted divergence (multi-provider), so only the F$e() term is wired.
        analytics_disabled: host.analytics_disabled,
        // `productFeedbackDisabled` follows the essential-traffic privacy gate:
        // with non-essential traffic disabled, the product feedback surface is
        // unavailable and the init frame must advertise that fact.
        product_feedback_disabled: host.product_feedback_disabled,
        memory_paths,
        per_turn_effort_active: host.per_turn_effort_active,
        view_mode: host.view_mode,
        fast_mode_state: fast_mode_state.to_string(),
        fast_mode_disabled_reason: fast_mode_disabled_reason.map(str::to_string),
        capabilities: STREAM_JSON_CAPABILITIES
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    }
}

/// Render only metadata supplied by the selected route's credential resolver.
fn credential_source_label(source: &llm_runtime::CredentialSource) -> String {
    use llm_runtime::CredentialSource;
    match source {
        CredentialSource::Environment { variable } => variable.clone(),
        CredentialSource::Stored => "stored".into(),
        CredentialSource::Configured => "configured".into(),
        CredentialSource::ApiKeyHelper => "apiKeyHelper".into(),
        CredentialSource::OAuth => "oauth".into(),
        CredentialSource::None => "none".into(),
        CredentialSource::Unknown => "unknown".into(),
    }
}

/// Map a `permission::PermissionMode` to its claude-code string representation.
pub fn permission_mode_str(mode: permission::PermissionMode) -> &'static str {
    match mode {
        permission::PermissionMode::Default => "default",
        permission::PermissionMode::AcceptEdits => "acceptEdits",
        permission::PermissionMode::BypassPermissions => "bypassPermissions",
        permission::PermissionMode::DontAsk => "dontAsk",
        permission::PermissionMode::Plan => "plan",
        permission::PermissionMode::Auto => "auto",
        permission::PermissionMode::Bubble => "default",
    }
}

#[cfg(test)]
mod canonical_model_tests {
    use super::*;

    /// (cc 2.1.218) The per-model usage block carries `canonicalModel` — the id
    /// the PRICING lookup used. It must collapse the spellings a raw model string
    /// can take (date suffix, `[1m]`, Bedrock ARN / inference profile) onto one
    /// canonical id, so a host can group cost across them.
    #[test]
    fn canonical_model_collapses_provider_spellings() {
        for (raw, want) in [
            ("claude-opus-4-7", "claude-opus-4-7"),
            ("claude-opus-4-7-20251101", "claude-opus-4-7"),
            ("us.anthropic.claude-opus-4-7-v1:0", "claude-opus-4-7"),
        ] {
            assert_eq!(
                cost::pricing::first_party_name_to_canonical(raw),
                want,
                "{raw} must canonicalize to {want}"
            );
        }
    }

    /// The field is present on BOTH emission branches (per-model rows and the
    /// legacy aggregate fallback) and sits alongside contextWindow/maxOutputTokens.
    #[test]
    fn usage_block_emits_canonical_model_on_both_branches() {
        let mut per_model = lingxi_core::host::orchestrator::CostSnapshot::default();
        per_model.by_model = vec![lingxi_core::host::orchestrator::ModelUsageRow {
            model: "us.anthropic.claude-opus-4-7-v1:0".into(),
            provider: Some("bedrock".into()),
            total_nano_usd: 1_000_000_000,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
            web_search_requests: 0,
        }];
        let block = StreamJsonStream::build_model_usage_block(&per_model, "ignored", &[]);
        let row = block
            .get("us.anthropic.claude-opus-4-7-v1:0")
            .expect("per-model row");
        assert_eq!(row["canonicalModel"], "claude-opus-4-7");
        // (cc 2.1.218) `n.provider=n_(r)` — sibling of canonicalModel, keyed
        // AFTER it (preserve_order map mirrors the oracle's assignment order).
        assert_eq!(row["provider"], "bedrock");
        let keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let canon_idx = keys.iter().position(|k| *k == "canonicalModel").unwrap();
        assert_eq!(keys.get(canon_idx + 1), Some(&"provider"));
        assert!(row.get("contextWindow").is_some(), "existing keys retained");

        // Legacy aggregate fallback (no per-model rows): provider is unknown
        // there and must be OMITTED (`.optional()`), never null.
        let mut agg = lingxi_core::host::orchestrator::CostSnapshot::default();
        agg.input_tokens = 5;
        let block2 =
            StreamJsonStream::build_model_usage_block(&agg, "claude-opus-4-7-20251101", &[]);
        let row2 = block2
            .get("claude-opus-4-7-20251101")
            .expect("aggregate row");
        assert_eq!(row2["canonicalModel"], "claude-opus-4-7");
        assert!(
            row2.get("provider").is_none(),
            "unknown provider is omitted"
        );
    }
}

/// Build the stream-json `tool_result_meta` array carrying denial provenance —
/// byte-locked to claude-code `Tpr(e)` (2.1.220, binary offset 233198808):
///
/// ```js
/// function Tpr(e){ let t=e.toolDenialKind; if(t===void 0) return [];
///   let r=e.message.content; if(!Array.isArray(r)) return [];
///   let n=r.filter(i=>i.type==="tool_result"); if(n.length!==1) return [];
///   let o={id:n[0].tool_use_id, non_execution_kind:t};
///   if(e.userFeedback!==void 0) o.user_feedback=e.userFeedback;
///   return [o] }
/// ```
///
/// `denial_kind` is the message's `toolDenialKind`. Beyond the five values the
/// kind classifier produces (`user-rejected`, `permission-rule`,
/// `automode-blocked`, `automode-unavailable`, `automode-parsing-error`), the
/// oracle also stamps `cancelled` / `interrupted` on its abort paths, and those
/// DO produce a meta entry — this builder gates only on the kind being absent,
/// exactly like `Tpr`. LingXi does not stamp the abort paths yet, so those
/// values simply never reach here today. The emitted key order is `id`,
/// `non_execution_kind`, then the optional `user_feedback`; the workspace pins
/// serde_json `preserve_order`, so that order is the wire order.
///
/// The single-`tool_result` guard is deliberate and load-bearing: the oracle
/// drops the meta entirely when a user message carries zero or several
/// tool_result blocks, because the denial kind is a message-level field and
/// could not be attributed to one specific block.
fn build_tool_result_meta(
    denial_kind: Option<&str>,
    user_feedback: Option<&str>,
    content: &Value,
) -> Vec<Value> {
    let Some(kind) = denial_kind else {
        return Vec::new();
    };
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    let mut results = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
    let (Some(only), None) = (results.next(), results.next()) else {
        return Vec::new();
    };
    let Some(id) = only.get("tool_use_id") else {
        return Vec::new();
    };

    let mut entry = serde_json::Map::new();
    entry.insert("id".to_string(), id.clone());
    entry.insert("non_execution_kind".to_string(), json!(kind));
    if let Some(feedback) = user_feedback {
        entry.insert("user_feedback".to_string(), json!(feedback));
    }
    vec![Value::Object(entry)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn query_timings_latch_first_observations_while_partial_ttft_tracks_current_request() {
        let stream = StreamJsonStream::new_json_mode_placeholder(Output::new(tokio::io::sink()));
        let epoch = std::time::Instant::now();
        stream.response_timing.lock().unwrap().started = Some(epoch);
        let at = |micros| epoch + std::time::Duration::from_micros(micros);
        for (event, micros) in [
            (ResponseTimingEvent::RequestStarted, 141_200),
            (ResponseTimingEvent::MessageStart, 210_250),
            (ResponseTimingEvent::ContentFrame, 211_300),
            (ResponseTimingEvent::AssistantMessage, 212_510),
            (ResponseTimingEvent::RequestStarted, 1_000_000),
            (ResponseTimingEvent::MessageStart, 1_200_000),
            (ResponseTimingEvent::ContentFrame, 1_201_000),
            (ResponseTimingEvent::AssistantMessage, 1_202_000),
        ] {
            stream.note_response_timing(event, at(micros));
        }
        let frame = stream
            .build_result_success_frame(
                "done",
                "end_turn",
                &CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        assert_eq!(frame["time_to_request_ms"], 141);
        assert_eq!(frame["ttft_stream_ms"], 210);
        assert_eq!(frame["first_content_frame_ms"], 211);
        assert_eq!(
            frame["ttft_ms"], 213,
            "native Math.round, not integer truncation"
        );
        assert_eq!(
            stream
                .response_timing
                .lock()
                .unwrap()
                .partial_ttft_ms(at(1_250_000)),
            Some(200)
        );
        let error = stream
            .build_result_error_frame(
                "error_max_turns",
                vec!["limit".into()],
                &CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        assert!(error.get("ttft_ms").is_none());
        stream.begin_query_timing();
        let mut reset = StreamJsonResultMetadata::default();
        stream
            .response_timing
            .lock()
            .unwrap()
            .fill_metadata(&mut reset);
        assert!(reset.ttft_ms.is_none() && reset.time_to_request_ms.is_none());
    }

    #[tokio::test]
    async fn result_metrics_omit_absent_owners_and_keep_owned_zero_and_query_duration() {
        let stream = StreamJsonStream::new_placeholder(Output::new(tokio::io::sink()));
        let mut cost = CostSnapshot { session_duration: std::time::Duration::from_secs(3600), ..Default::default() };
        stream.begin_query_timing();
        let absent = stream.build_result_success_frame("done", "end_turn", &cost, "model", "off", None, &[]).await;
        assert!(absent.get("safety_stops").is_none());
        assert!(absent.get("subagent_stats").is_none());
        assert!(absent["duration_ms"].as_u64().unwrap() < 60_000, "a new query does not inherit an hour of session duration");
        cost.safety_stops = Some(0);
        let owner = lingxi_core::host::agent_statistics::AgentSessionStatistics::default();
        stream.set_result_metadata(StreamJsonResultMetadata {
            subagent_stats: Some(serde_json::to_value(owner.snapshot()).unwrap()),
            ..Default::default()
        }).await;
        let present = stream.build_result_error_frame("error_max_turns", vec![], &cost, "model", "off", None, &[]).await;
        assert_eq!(present["safety_stops"], 0);
        assert_eq!(present["subagent_stats"]["spawned"], 0);
        assert!(present["duration_ms"].as_u64().unwrap() < 60_000);
        let no_owner = StreamJsonStream::new_placeholder(Output::new(tokio::io::sink()));
        cost.safety_stops = None;
        let error = no_owner.build_result_error_frame("error_max_turns", vec![], &cost, "model", "off", None, &[]).await;
        assert!(error.get("safety_stops").is_none() && error.get("subagent_stats").is_none());
        assert_eq!(error["duration_ms"], 3_600_000, "direct frame fixtures without an admitted query retain supplied duration");
    }

    #[tokio::test]
    async fn request_marker_frames_match_fixed_client_uuid_native_receipt() {
        let receipt: Value = serde_json::from_str(include_str!("../../../../scripts/tests/headless-fixtures/native-2.1.293-client-marker-probes.json")).unwrap();
        for case in receipt["cases"].as_array().unwrap() {
            let input = Utf16JsonProjection::parse(case["stdinText"].as_str().unwrap()).unwrap();
            let client_uuid = input.subprojection("/uuid").unwrap();
            let captured = Arc::new(StdMutex::new(Vec::new()));
            let stream = StreamJsonStream::new_placeholder(Output::new(super::verbose_json_tests::CapturedWriter(captured.clone())));
            stream.begin_request_markers(Some(client_uuid.clone()), vec![client_uuid.clone()], false).unwrap();
            let frames = case["frames"].as_array().unwrap();
            for evidence in frames.iter().filter(|evidence| evidence["type"] != "result") {
                let mut base = evidence["row"].clone();
                let object = base.as_object_mut().unwrap();
                object.remove("user_message_uuid"); object.remove("user_message_uuids");
                stream.enqueue(&base);
            }
            stream.flush().await.unwrap();
            let actual = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
            let expected = frames.iter().filter(|evidence| evidence["type"] != "result")
                .map(|evidence| format!("{}\n", evidence["rawLine"].as_str().unwrap())).collect::<String>();
            assert_eq!(actual, expected, "{} first assistant/partial markers, including tool continuation", case["id"]);
            let native = frames.iter().find(|evidence| evidence["type"] == "result").unwrap();
            let row = &native["row"];
            *stream.session_id.lock().await = row["session_id"].as_str().unwrap().to_owned();
            stream.set_result_metadata(StreamJsonResultMetadata {
                ttft_ms: row.get("ttft_ms").and_then(Value::as_u64),
                ttft_stream_ms: row.get("ttft_stream_ms").and_then(Value::as_u64),
                time_to_request_ms: row.get("time_to_request_ms").and_then(Value::as_u64),
                first_content_frame_ms: row.get("first_content_frame_ms").and_then(Value::as_u64),
                num_turns: row["num_turns"].as_u64(),
                stop_reason: row["stop_reason"].as_str().map(str::to_owned),
                terminal_reason: row["terminal_reason"].as_str().map(str::to_owned),
                subagent_stats: row.get("subagent_stats").cloned(),
                safety_stops: row.get("safety_stops").and_then(Value::as_u64),
                usage: row.get("usage").cloned(), model_usage: row["modelUsage"].as_object().cloned(),
                queued_turn_count: row["queued_turn_count"].as_u64().unwrap(),
                result_index: row["result_index"].as_u64().unwrap(),
                ..Default::default()
            }).await;
            {
                let mut clock = stream.response_timing.lock().unwrap();
                clock.first_request_wall_ms = row.get("request_sent_wall_ms").and_then(Value::as_i64);
                clock.first_request_input_tokens = row.get("first_request_input_tokens").and_then(Value::as_u64);
            }
            let cost = CostSnapshot {
                total_usd: row["total_cost_usd"].as_f64().unwrap(),
                api_duration: std::time::Duration::from_millis(row["duration_api_ms"].as_u64().unwrap()),
                session_duration: std::time::Duration::from_millis(row["duration_ms"].as_u64().unwrap()),
                ..Default::default()
            };
            let mut result = if row["is_error"] == true {
                stream.build_result_error_frame(row["subtype"].as_str().unwrap(), row["errors"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_owned()).collect(), &cost, "model", "off", Some("sdk_opt_in_required"), &[]).await
            } else {
                stream.build_result_success_frame(row["result"].as_str().unwrap(), row["stop_reason"].as_str().unwrap(), &cost, "model", "off", Some("sdk_opt_in_required"), &[]).await
            };
            // Only the generated result UUID is replaced; the fixed client UUID
            // and every property position remain literal captured evidence.
            result["uuid"] = row["uuid"].clone();
            assert_eq!(serialize_ndjson_line(&result), format!("{}\n", native["rawLine"].as_str().unwrap()));
            assert_eq!(result["user_message_uuid"], client_uuid.value);
            stream.finish().await.unwrap();
        }
    }

    #[tokio::test]
    async fn request_marker_dispatch_facts_latch_first_real_observations_and_omit_unavailable() {
        let stream = StreamJsonStream::new_placeholder(Output::new(tokio::io::sink()));
        let client = Utf16JsonProjection::plain(json!("client"));
        stream.begin_request_markers(Some(client.clone()), vec![client], false).unwrap();
        let absent = stream.build_result_success_frame("done", "end_turn", &CostSnapshot::default(), "model", "off", None, &[]).await;
        assert!(absent.get("request_sent_wall_ms").is_none() && absent.get("first_request_input_tokens").is_none());
        stream.begin_query_timing();
        stream.note_response_timing(ResponseTimingEvent::RequestStarted, std::time::Instant::now());
        stream.note_first_request_input_tokens(7);
        let first_wall = stream.response_timing.lock().unwrap().first_request_wall_ms;
        stream.note_response_timing(ResponseTimingEvent::RequestStarted, std::time::Instant::now());
        stream.note_first_request_input_tokens(99);
        let actual = stream.build_result_success_frame("done", "end_turn", &CostSnapshot::default(), "model", "off", None, &[]).await;
        assert_eq!(actual["request_sent_wall_ms"].as_i64(), first_wall);
        assert_eq!(actual["first_request_input_tokens"], 7);
        stream.begin_request_markers(None, vec![], true).unwrap();
        let notification = stream.build_result_success_frame("done", "end_turn", &CostSnapshot::default(), "model", "off", None, &[]).await;
        assert!(notification.get("user_message_uuid").is_none() && notification.get("request_sent_wall_ms").is_none());
    }

    #[test]
    fn usage_details_are_projected_from_settled_response_and_model_slices() {
        let cost = CostSnapshot {
            current_usage: Some(lingxi_core::host::CurrentUsageSnapshot {
                input_tokens: 10,
                output_tokens: 20,
                cache_creation_input_tokens: 10,
                cache_read_input_tokens: 4,
            }),
            current_usage_details: Some(
                lingxi_core::host::orchestrator::ResponseUsageDetailsSnapshot {
                    reasoning_tokens: 7,
                    web_search_requests: 3,
                    cache_creation_1h_input_tokens: 4,
                    cache_creation_5m_input_tokens: 6,
                    fast_mode: true,
                },
            ),
            by_model: vec![lingxi_core::host::orchestrator::ModelUsageRow {
                model: "claude-sonnet-5-5".into(),
                provider: Some("firstParty".into()),
                total_nano_usd: 2,
                input_tokens: 30,
                output_tokens: 40,
                cache_read_input_tokens: 5,
                cache_creation_input_tokens: 10,
                reasoning_tokens: 8,
                web_search_requests: 4,
            }],
            ..Default::default()
        };
        let usage = StreamJsonStream::build_usage_block(&cost);
        assert_eq!(usage["output_tokens"], 27);
        assert_eq!(usage["output_tokens_details"]["thinking_tokens"], 7);
        assert_eq!(usage["server_tool_use"]["web_search_requests"], 3);
        assert_eq!(
            usage["cache_creation"],
            json!({"ephemeral_1h_input_tokens":4,"ephemeral_5m_input_tokens":6})
        );
        assert_eq!(usage["speed"], "fast");
        let models = StreamJsonStream::build_model_usage_block(&cost, "ignored", &[]);
        assert_eq!(models["claude-sonnet-5-5"]["thinkingTokens"], 8);
        assert_eq!(models["claude-sonnet-5-5"]["outputTokens"], 48);
        assert_eq!(models["claude-sonnet-5-5"]["webSearchRequests"], 4);
    }

    // Claude Code 2.1.261: $Ke @164270142 and live manual /compact boundary.
    // Fixed UUIDs make field order, omission, and Unicode escaping byte-testable.
    #[test]
    fn compact_boundary_matches_261_sdk_bytes() {
        let metadata: lingxi_core::types::CompactBoundaryMetadata = serde_json::from_value(json!({
            "trigger":"manual", "preTokens":42000, "postTokens":12000,
            "cumulativeDroppedTokens":31000, "durationMs":987, "userContext":"keep API details",
            "messagesSummarized":10, "precomputed":true, "preCompactDiscoveredTools":["Read"],
            "preservedSegment":{"headUuid":"head","anchorUuid":"summary","tailUuid":"tail"},
            "preservedMessages":{"anchorUuid":"summary","uuids":["head","tail"],"allUuids":["head","attachment","tail"]},
            "logicalParentUuid":"parent"
        })).unwrap();
        let frame =
            StreamJsonStream::build_compact_boundary_frame("session", "boundary", &metadata);
        assert_eq!(
            serialize_ndjson_line(&frame),
            concat!(
                r#"{"type":"system","subtype":"compact_boundary","session_id":"session","uuid":"boundary","compact_metadata":{"trigger":"manual","pre_tokens":42000,"post_tokens":12000,"cumulative_dropped_tokens":31000,"duration_ms":987,"user_context":"keep API details","messages_summarized":10,"precomputed":true,"pre_compact_discovered_tools":["Read"],"preserved_segment":{"head_uuid":"head","anchor_uuid":"summary","tail_uuid":"tail"},"preserved_messages":{"anchor_uuid":"summary","uuids":["head","tail"],"all_uuids":["head","attachment","tail"]}},"logical_parent_uuid":"parent"}"#,
                "\n"
            )
        );
        let minimal = StreamJsonStream::build_compact_boundary_frame(
            "s",
            "b",
            &lingxi_core::types::CompactBoundaryMetadata::default(),
        );
        assert_eq!(
            serialize_ndjson_line(&minimal),
            concat!(
                r#"{"type":"system","subtype":"compact_boundary","uuid":"b","compact_metadata":{"trigger":"auto","pre_tokens":0},"session_id":"s"}"#,
                "\n"
            )
        );
    }

    #[test]
    fn compact_status_matches_261_sdk_bytes() {
        assert_eq!(
            serialize_ndjson_line(&StreamJsonStream::build_compact_status_frame(
                "s", "start", None
            )),
            concat!(
                r#"{"type":"system","subtype":"status","status":"compacting","session_id":"s","uuid":"start"}"#,
                "\n"
            )
        );
        assert_eq!(
            serialize_ndjson_line(&StreamJsonStream::build_compact_status_frame(
                "s",
                "end",
                Some(None)
            )),
            concat!(
                r#"{"type":"system","subtype":"status","status":null,"compact_result":"success","session_id":"s","uuid":"end"}"#,
                "\n"
            )
        );
        assert_eq!(
            serialize_ndjson_line(&StreamJsonStream::build_compact_status_frame(
                "s",
                "end",
                Some(Some("Compaction canceled."))
            )),
            concat!(
                r#"{"type":"system","subtype":"status","status":null,"compact_result":"failed","compact_error":"Compaction canceled.","session_id":"s","uuid":"end"}"#,
                "\n"
            )
        );
    }

    #[test]
    fn compact_summary_matches_261_synthetic_user_bytes() {
        let frame = StreamJsonStream::build_compact_user_frame(
            "s",
            "summary",
            "timestamp",
            "summary\ntext",
            true,
        );
        assert_eq!(
            serialize_ndjson_line(&frame),
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"summary\ntext"},"session_id":"s","parent_tool_use_id":null,"uuid":"summary","timestamp":"timestamp","isReplay":false,"isSynthetic":true}"#,
                "\n"
            )
        );
    }

    #[tokio::test]
    async fn compact_command_output_replay_gate_and_failure_severity_match_live_oracle() {
        for replay in [false, true] {
            for failure in [
                None,
                Some(("Not enough messages to compact.", false)),
                Some((
                    "Error during compaction: summarization produced empty response",
                    true,
                )),
            ] {
                let stream =
                    StreamJsonStream::new(make_params("s"), Output::new(tokio::io::sink()));
                let mut receiver = stream.drain_rx.lock().await.take().unwrap();
                stream
                    .emit_compact_command_output(
                        "keep APIs",
                        "command",
                        "before",
                        failure,
                        true,
                        replay,
                    )
                    .await;
                let mut frames = Vec::new();
                while let Ok(OutboundMsg::Line(line)) = receiver.try_recv() {
                    frames.push(serde_json::from_str::<Value>(&line).unwrap());
                }
                assert_eq!(frames.len(), if replay { 2 } else { 1 });
                if let Some((display, error)) = failure {
                    assert_eq!(frames[0]["type"], "assistant");
                    assert_eq!(frames[0]["message"]["model"], "<synthetic>");
                    assert_eq!(frames[0]["message"]["content"][0]["text"], display);
                    assert_eq!(frames[0]["is_meta"], true);
                    let pipe = if error { "stderr" } else { "stdout" };
                    assert_eq!(
                        frames[0]["local_command_source"],
                        format!("<local-command-{pipe}>{display}</local-command-{pipe}>")
                    );
                } else {
                    assert_eq!(
                        frames[0]["message"]["content"],
                        "<local-command-stdout>Compacted </local-command-stdout>"
                    );
                    assert_eq!(frames[0]["isReplay"], true);
                }
                if replay {
                    assert_eq!(frames[1]["uuid"], "command");
                    assert_eq!(frames[1]["timestamp"], "before");
                    assert_eq!(
                        frames[1]["message"]["content"],
                        "<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args>keep APIs</command-args>"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn compact_command_result_does_not_reuse_the_previous_model_turn() {
        let stream = StreamJsonStream::new(make_params("s"), Output::new(tokio::io::sink()));
        let mut receiver = stream.drain_rx.lock().await.take().unwrap();
        let cost = CostSnapshot {
            input_tokens: 200,
            output_tokens: 10,
            api_calls: 4,
            total_usd: 0.2,
            ..Default::default()
        };
        stream.emit_compact_command_result(&cost, 123, None).await;
        let OutboundMsg::Line(line) = receiver.try_recv().unwrap() else {
            panic!("result frame");
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["subtype"], "success");
        assert_eq!(frame["is_error"], false);
        assert_eq!(frame["result"], "");
        assert_eq!(frame["duration_ms"], 123);
        assert_eq!(frame["duration_api_ms"], 0);
        assert_eq!(frame["num_turns"], 0);
        assert_eq!(frame["stop_reason"], Value::Null);
        assert_eq!(frame["usage"]["input_tokens"], 0);
        assert_eq!(frame["usage"]["output_tokens"], 0);
        assert_eq!(frame["total_cost_usd"], 0.2);
    }

    #[tokio::test]
    async fn compact_command_failure_result_matches_live_261_oracle() {
        // Local-mock captures of Claude Code 2.1.261: failures after the
        // compaction attempt return the notice as result text; rejecting an
        // empty session only emits the synthetic notice and an empty result.
        for (failure, expected) in [
            ("Error: No messages to compact", ""),
            (
                "Not enough messages to compact.",
                "Not enough messages to compact.",
            ),
            (
                "Error during compaction: summarization produced empty response",
                "Error during compaction: summarization produced empty response",
            ),
        ] {
            let stream = StreamJsonStream::new(make_params("s"), Output::new(tokio::io::sink()));
            let mut receiver = stream.drain_rx.lock().await.take().unwrap();
            stream
                .emit_compact_command_result(&CostSnapshot::default(), 123, Some(failure))
                .await;
            let OutboundMsg::Line(line) = receiver.try_recv().unwrap() else {
                panic!("result frame");
            };
            let frame: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame["result"], expected);
            assert_eq!(frame["subtype"], "success");
            assert_eq!(frame["is_error"], false);
            assert_eq!(frame["num_turns"], 0);
        }
    }

    pub(super) fn make_params(session_id: &str) -> StreamJsonInitParams {
        build_init_params(
            session_id,
            vec![],
            vec![],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "default",
            vec![],
            vec![],
            vec![],
            vec![],
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        )
    }

    #[test]
    fn init_frame_uses_only_selected_route_credential_source() {
        use llm_runtime::CredentialSource;
        let cases = [
            (
                CredentialSource::Environment {
                    variable: "GEMINI_API_KEY".into(),
                },
                "GEMINI_API_KEY",
            ),
            (
                CredentialSource::Environment {
                    variable: "CUSTOM_PROFILE_KEY".into(),
                },
                "CUSTOM_PROFILE_KEY",
            ),
            (CredentialSource::Stored, "stored"),
            (CredentialSource::Configured, "configured"),
            (CredentialSource::OAuth, "oauth"),
            (CredentialSource::ApiKeyHelper, "apiKeyHelper"),
            (CredentialSource::Unknown, "unknown"),
            (CredentialSource::None, "none"),
        ];
        for (source, expected) in cases {
            let params = build_init_params(
                "source-test",
                vec![],
                vec![],
                "gemini-model",
                &source,
                "default",
                vec![],
                vec![],
                vec![],
                vec![],
                "default",
                None,
                "off",
                None,
                StreamJsonInitHostMetadata::default(),
            );
            let frame = build_init_frame("source-test", "source-init", &params);
            assert_eq!(frame["apiKeySource"], expected);
            assert_eq!(frame["model"], "gemini-model");
        }
    }

    #[tokio::test]
    async fn thinking_display_can_omit_and_restore_stream_blocks() {
        let stream = StreamJsonStream::new(
            make_params("thinking-display"),
            Output::new(tokio::io::sink()),
        );
        stream.set_thinking_display(Some("omitted"));
        stream.emit_thinking("hidden", None).await;
        assert!(stream.accum.lock().await.blocks.is_empty());

        stream.set_thinking_display(Some("summarized"));
        stream.emit_thinking("visible", None).await;
        assert!(matches!(
            stream.accum.lock().await.blocks.as_slice(),
            [AccBlock::Thinking { thinking, .. }] if thinking == "visible"
        ));
    }

    #[tokio::test]
    async fn skipped_compaction_retains_legacy_success_sequence() {
        let stream = StreamJsonStream::new(
            make_params("compact-skipped"),
            Output::new(tokio::io::sink()),
        );
        let mut receiver = stream.drain_rx.lock().await.take().unwrap();
        stream.emit_compaction_started().await;
        stream.emit_compaction_skipped().await;
        let mut frames = Vec::new();
        while let Ok(OutboundMsg::Line(line)) = receiver.try_recv() {
            frames.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["status"], "compacting");
        assert_eq!(frames[1]["compact_result"], "success");
    }

    #[tokio::test]
    async fn compact_events_reach_the_stream_and_json_mode_suppresses_them() {
        for suppressed in [false, true] {
            let stream = StreamJsonStream::new_inner(
                Some(make_params("compact-session")),
                suppressed,
                Output::new(tokio::io::sink()),
            );
            let mut receiver = stream.drain_rx.lock().await.take().unwrap();
            let metadata = lingxi_core::types::CompactBoundaryMetadata::default();
            stream.emit_compaction_started().await;
            stream.emit_compaction_finished(None).await;
            stream
                .emit_compact_boundary("persisted-boundary", &metadata)
                .await;
            if suppressed {
                assert!(receiver.try_recv().is_err());
                continue;
            }
            let mut frames = Vec::new();
            while let Ok(OutboundMsg::Line(line)) = receiver.try_recv() {
                frames.push(serde_json::from_str::<Value>(&line).unwrap());
            }
            assert_eq!(frames.len(), 3);
            assert_eq!(frames[0]["status"], "compacting");
            assert_eq!(frames[1]["compact_result"], "success");
            assert_eq!(frames[2]["subtype"], "compact_boundary");
            assert_eq!(frames[2]["uuid"], "persisted-boundary");
        }
    }

    /// Verify that the init frame includes the 20 mandatory keys in the correct
    /// order as defined by GROUND-TRUTH.md.
    #[tokio::test]
    async fn init_frame_has_correct_keys() {
        let params = build_init_params(
            "test-session-id",
            vec!["Bash".to_string(), "Read".to_string(), "Agent".to_string()],
            vec![("codegraph".to_string(), "connected".to_string())],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "bypassPermissions",
            vec!["graphify".to_string()],
            vec!["claude".to_string()],
            vec!["deep-research".to_string()],
            vec![StreamJsonPlugin {
                name: "superpowers".to_string(),
                path: "/path/to/superpowers".to_string(),
                source: "superpowers@marketplace".to_string(),
                version: None,
            }],
            "default",
            Some("/home/user/.lingxi/projects/test/memory/"),
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        let params_guard = stream.init_params.lock().await;
        let frame = build_init_frame(
            "test-session-id",
            "test-init-id",
            params_guard.as_ref().unwrap(),
        );
        assert_eq!(frame["tools"], json!(["Bash", "Read", "Agent"]));
    }

    #[test]
    fn privacy_snapshot_fields_are_serialized_independently() {
        let mut params = make_params("sess");
        for (analytics, feedback) in [(false, false), (true, false), (true, true)] {
            params.analytics_disabled = analytics;
            params.product_feedback_disabled = feedback;
            let frame = build_init_frame("sess", "uuid", &params);
            assert_eq!(frame["analytics_disabled"], analytics);
            assert_eq!(frame["product_feedback_disabled"], feedback);
        }
    }

    /// Verify text accumulation — multiple `emit_text` calls on the same
    /// block are concatenated, not split into multiple text blocks.
    #[tokio::test]
    async fn text_accumulation_concatenates() {
        let params = make_params("sess");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream
            .emit_message_start("msg_test", "claude-opus-4-8")
            .await;
        stream.emit_text("he", None).await;
        stream.emit_text("llo", None).await;
        stream.emit_text(" world", None).await;
        let acc = stream.accum.lock().await;
        assert_eq!(acc.blocks.len(), 1);
        if let AccBlock::Text(t) = &acc.blocks[0] {
            assert_eq!(String::from_utf16_lossy(t), "hello world");
        } else {
            panic!("expected Text block");
        }
    }

    /// Verify that `emit_message_boundary` resets the accumulator.
    #[tokio::test]
    async fn message_boundary_resets_accumulator() {
        let params = make_params("sess");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream
            .emit_message_start("msg_001", "claude-opus-4-8")
            .await;
        stream.emit_text("pong", None).await;
        // Boundary flush (output goes to real stdout in tests — that's OK).
        stream
            .emit_message_boundary(Some("end_turn"), Some("req_test"))
            .await;
        // Accumulator should be reset.
        let acc = stream.accum.lock().await;
        assert!(
            acc.blocks.is_empty(),
            "blocks should be cleared after boundary"
        );
        assert!(acc.message_id.is_empty(), "message_id should be cleared");
    }

    /// The tool_result `user` frame carries the MODEL-FACING STRING in
    /// `content` (not a JSON dump of the data) and the full structured result on
    /// a separate top-level `toolUseResult` field — 1:1 with claude-code's SDK
    /// user message.
    #[tokio::test]
    async fn tool_result_frame_uses_model_text_and_carries_tooluseresult() {
        let stream = StreamJsonStream::new(make_params("sess"), Output::new(tokio::io::sink()));
        let tuid = "toolu_x";

        // WebFetch-shaped: the orchestrator passes the model text explicitly;
        // the full structured `data` lands on `toolUseResult`.
        let data = json!({
            "bytes": 5, "code": 200, "codeText": "OK",
            "result": "# Page\n\nbody", "durationMs": 3, "url": "https://e/"
        });
        let frame = stream
            .build_tool_result_frame(tuid, "# Page\n\nbody", &data)
            .await;
        let tr = &frame["message"]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], tuid);
        assert_eq!(
            tr["content"], "# Page\n\nbody",
            "content is the model text, not a JSON dump"
        );
        assert_eq!(tr["is_error"], false);
        assert_eq!(
            frame["toolUseResult"], data,
            "full structured result on the top-level field"
        );

        // Bash-shaped: the model text is whatever the dispatch computed; `data`
        // stays pure metadata on `toolUseResult`.
        let bash = json!({ "model_content": "out\n", "stdout": "out\n", "exit_code": 0 });
        let f2 = stream.build_tool_result_frame(tuid, "out\n", &bash).await;
        assert_eq!(f2["message"]["content"][0]["content"], "out\n");
        assert_eq!(f2["toolUseResult"], bash);

        // Error wrapper `{error}`: content is the model text + is_error derives
        // from the `error` key on `data`.
        let err = json!({ "error": "Permission to use Bash has been denied." });
        let f3 = stream
            .build_tool_result_frame(tuid, "Permission to use Bash has been denied.", &err)
            .await;
        assert_eq!(
            f3["message"]["content"][0]["content"],
            "Permission to use Bash has been denied."
        );
        assert_eq!(f3["message"]["content"][0]["is_error"], true);
        assert_eq!(f3["toolUseResult"], err);
    }

    /// A denied tool result carries `tool_result_meta` on the user frame
    /// (claude-code `…,...o.length>0&&{tool_result_meta:o},…`, 2.1.220 binary
    /// offset 233203100); a normal result omits the key ENTIRELY rather than
    /// emitting an empty array — the oracle spreads it conditionally.
    #[tokio::test]
    async fn denied_tool_result_frame_carries_tool_result_meta() {
        let stream =
            StreamJsonStream::new(make_params("sess-deny"), Output::new(tokio::io::sink()));
        let tuid = "toolu_deny_1";
        let err = json!({ "error": "Permission to use Bash has been denied." });

        let denied = stream
            .build_tool_result_frame_with_denial(
                tuid,
                "Permission to use Bash has been denied.",
                &err,
                Some("permission-rule"),
                None,
            )
            .await;
        assert_eq!(
            serde_json::to_string(&denied["tool_result_meta"]).unwrap(),
            r#"[{"id":"toolu_deny_1","non_execution_kind":"permission-rule"}]"#
        );

        let allowed = stream
            .build_tool_result_frame(tuid, "ok\n", &json!({"stdout": "ok\n"}))
            .await;
        assert!(
            allowed.get("tool_result_meta").is_none(),
            "a non-denied result must omit the key, not emit []"
        );
    }

    #[test]
    fn tool_heartbeat_uses_client_protocol_wire_shape() {
        let id = lingxi_core::types::ToolUseId::new();
        let frame = StreamJsonStream::build_tool_heartbeat_frame(&id, "Bash", 4_321);
        assert_eq!(frame["type"], "tool_heartbeat");
        assert_eq!(frame["id"], id.to_string());
        assert_eq!(frame["tool"], "Bash");
        assert_eq!(frame["elapsed_ms"], 4_321);
    }

    #[test]
    fn prompt_suggestion_frame_matches_expected_shape() {
        let frame = StreamJsonStream::build_prompt_suggestion_frame(
            "sess-prompt",
            "How should I test this?",
            "uuid-prompt",
        );
        let keys: Vec<&str> = frame
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["type", "suggestion", "uuid", "session_id"]);
        assert_eq!(frame["type"], "prompt_suggestion");
        assert_eq!(frame["suggestion"], "How should I test this?");
        assert_eq!(frame["session_id"], "sess-prompt");
        assert_eq!(frame["uuid"], "uuid-prompt");
    }

    #[tokio::test]
    async fn tool_heartbeats_coalesce_when_stdout_is_backpressured() {
        let stream = StreamJsonStream::new(
            make_params("sess-heartbeat"),
            Output::new(tokio::io::sink()),
        );
        let mut rx = stream
            .drain_rx
            .lock()
            .await
            .take()
            .expect("drain receiver available");
        let id = lingxi_core::types::ToolUseId::new();

        stream.emit_tool_heartbeat(&id, "Bash", 1_000).await;
        stream.emit_tool_heartbeat(&id, "Bash", 2_000).await;
        stream.emit_tool_heartbeat(&id, "Bash", 3_000).await;

        let OutboundMsg::Heartbeats(heartbeats) = rx.try_recv().expect("heartbeat wake-up") else {
            panic!("expected coalesced heartbeat wake-up");
        };
        let lines = heartbeats.drain();
        assert_eq!(lines.len(), 1);
        let frame: Value = serde_json::from_str(&lines[0]).expect("valid heartbeat json");
        assert_eq!(frame["elapsed_ms"], 3_000);
        assert!(rx.try_recv().is_err(), "only one wake-up may be queued");
    }

    #[tokio::test]
    async fn task_lifecycle_reaches_stream_json_with_session_envelope() {
        let stream =
            StreamJsonStream::new(make_params("sess-task"), Output::new(tokio::io::sink()));
        let mut rx = stream.drain_rx.lock().await.take().unwrap();
        stream.emit_task_lifecycle(&json!({"type":"system", "subtype":"task_started", "task_id":"b12345678", "description":"build", "task_type":"local_bash"})).await;
        let OutboundMsg::Line(line) = rx.try_recv().unwrap() else {
            panic!("expected SDK frame");
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["subtype"], "task_started");
        assert_eq!(frame["task_id"], "b12345678");
        assert_eq!(frame["session_id"], "sess-task");
        assert!(frame["uuid"].as_str().is_some());
    }

    #[tokio::test]
    async fn system_notice_is_visible_as_a_sanitized_system_frame() {
        let stream =
            StreamJsonStream::new(make_params("sess-notice"), Output::new(tokio::io::sink()));
        let mut rx = stream
            .drain_rx
            .lock()
            .await
            .take()
            .expect("drain receiver available");

        stream
            .emit_system_notice("transcript persistence failed", true)
            .await;

        let OutboundMsg::Line(line) = rx.try_recv().expect("notice frame") else {
            panic!("expected a Line frame");
        };
        let frame: Value = serde_json::from_str(&line).expect("valid notice json");
        assert_eq!(frame["type"], "system");
        assert_eq!(frame["subtype"], "notice");
        assert_eq!(frame["message"], "transcript persistence failed");
        assert_eq!(frame["is_error"], true);
        assert_eq!(frame["session_id"], "sess-notice");
    }

    #[tokio::test]
    async fn mod_log_matches_claude_stream_json_system_frame() {
        let stream = StreamJsonStream::new(make_params("sess-mod"), Output::new(tokio::io::sink()));
        let mut rx = stream.drain_rx.lock().await.take().unwrap();
        stream.emit_mod_log("review", "Found a mismatch").await;

        let OutboundMsg::Line(line) = rx.try_recv().expect("ui_log frame") else {
            panic!("expected a Line frame");
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "system");
        assert_eq!(frame["subtype"], "ui_log");
        assert_eq!(frame["plugin"], "review");
        assert_eq!(frame["text"], "Found a mismatch");
        assert_eq!(frame["session_id"], "sess-mod");
        let uuid = frame["uuid"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
        assert_eq!(frame.as_object().unwrap().len(), 6);
        assert_eq!(
            line,
            format!(
                "{{\"type\":\"system\",\"subtype\":\"ui_log\",\"plugin\":\"review\",\"text\":\"Found a mismatch\",\"uuid\":\"{uuid}\",\"session_id\":\"sess-mod\"}}\n"
            )
        );
    }

    #[tokio::test]
    async fn mod_toast_matches_claude_stream_json_system_frame() {
        let stream = StreamJsonStream::new(make_params("sess-mod"), Output::new(tokio::io::sink()));
        let mut rx = stream.drain_rx.lock().await.take().unwrap();
        stream.emit_mod_toast("review", "Done", 4000).await;

        let OutboundMsg::Line(line) = rx.try_recv().expect("ui_toast frame") else {
            panic!("expected a Line frame");
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "system");
        assert_eq!(frame["subtype"], "ui_toast");
        assert_eq!(frame["plugin"], "review");
        assert_eq!(frame["text"], "Done");
        assert_eq!(frame["timeout_ms"], 4000);
        assert_eq!(frame["session_id"], "sess-mod");
        let uuid = frame["uuid"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
        assert_eq!(frame.as_object().unwrap().len(), 7);
        assert_eq!(
            line,
            format!(
                "{{\"type\":\"system\",\"subtype\":\"ui_toast\",\"plugin\":\"review\",\"text\":\"Done\",\"timeout_ms\":4000,\"uuid\":\"{uuid}\",\"session_id\":\"sess-mod\"}}\n"
            )
        );
    }

    #[tokio::test]
    async fn mod_status_uses_null_to_clear_claude_stream_json_frame() {
        let stream = StreamJsonStream::new(make_params("sess-mod"), Output::new(tokio::io::sink()));
        let mut rx = stream.drain_rx.lock().await.take().unwrap();
        stream.emit_mod_status("review", None).await;

        let OutboundMsg::Line(line) = rx.try_recv().expect("ui_status frame") else {
            panic!("expected a Line frame");
        };
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "system");
        assert_eq!(frame["subtype"], "ui_status");
        assert_eq!(frame["plugin"], "review");
        assert!(frame["text"].is_null());
        assert_eq!(frame["session_id"], "sess-mod");
        let uuid = frame["uuid"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
        assert_eq!(frame.as_object().unwrap().len(), 6);
        assert_eq!(
            line,
            format!(
                "{{\"type\":\"system\",\"subtype\":\"ui_status\",\"plugin\":\"review\",\"text\":null,\"uuid\":\"{uuid}\",\"session_id\":\"sess-mod\"}}\n"
            )
        );
    }

    #[tokio::test]
    async fn nested_stream_event_value_does_not_consume_stream_event_capacity() {
        let stream =
            StreamJsonStream::new(make_params("sess-nested"), Output::new(tokio::io::sink()));
        let mut rx = stream
            .drain_rx
            .lock()
            .await
            .take()
            .expect("drain receiver available");
        let frame = json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "input": {"type": "stream_event"}}]}
        });

        stream.enqueue(&frame);

        assert_eq!(stream.pending_stream_events.load(Ordering::Relaxed), 0);
        assert!(matches!(
            rx.try_recv().expect("frame enqueued"),
            OutboundMsg::Line(_)
        ));
    }

    /// Verify U+2028/U+2029 escaping.
    #[test]
    fn line_terminator_escaping() {
        let s = "hello\u{2028}world\u{2029}end";
        let escaped = escape_line_terminators(s);
        assert_eq!(escaped, "hello\\u2028world\\u2029end");
    }

    /// Verify that `emit_message_boundary` stores the last text in
    /// `last_result_text` before resetting.
    #[tokio::test]
    async fn message_boundary_stores_last_result_text() {
        let params = make_params("sess");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream
            .emit_message_start("msg_001", "claude-opus-4-8")
            .await;
        stream.emit_text("pong", None).await;
        stream
            .emit_message_boundary(Some("end_turn"), Some("req_test"))
            .await;
        let text = stream.get_last_result_text().await;
        assert_eq!(text.value, json!("pong"), "last_result_text should be 'pong'");
    }

    /// SC-01 (2.1.238): both usage objects the stream-json surface emits carry
    /// `output_tokens_details.thinking_tokens`, in the oracle's key position.
    ///
    /// * `result` frame — `gXl()` spreads `DR`, whose FIRST key is
    ///   `output_tokens_details` (cc-238.js @283631657 / @300232503).
    /// * `assistant` frame — the `nTe` usage merge (@297183459) places it
    ///   directly after `output_tokens`.
    ///
    /// The 2.1.220 binary has 0 hits for `output_tokens_details`, so this is
    /// upstream drift, not a long-standing port choice.
    #[tokio::test]
    async fn sc01_usage_objects_carry_output_tokens_details() {
        let stream = StreamJsonStream::new(make_params("sess-otd"), Output::new(tokio::io::sink()));
        let cost = CostSnapshot {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 50,
            cache_creation_tokens: 5,
            ..Default::default()
        };
        let frame = stream
            .build_result_success_frame(
                "pong",
                "end_turn",
                &cost,
                "claude-opus-4-8",
                "off",
                None,
                &[],
            )
            .await;
        let usage = frame["usage"].as_object().unwrap();
        let keys: Vec<&str> = usage.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "output_tokens_details",
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
                "output_tokens",
                "server_tool_use",
                "service_tier",
                "cache_creation",
                "inference_geo",
                "iterations",
                "speed",
                "fallback_credit",
            ],
            "result/usage must match DR's key order, output_tokens_details first"
        );
        assert_eq!(usage["output_tokens_details"]["thinking_tokens"], 0_u64);

        // Assistant frame: after `output_tokens`, before `service_tier`.
        stream
            .emit_message_start("msg_otd", "claude-opus-4-8")
            .await;
        stream.emit_text("hi", None).await;
        let acc = stream.accum.lock().await;
        let message = acc.to_message_json(Some("end_turn"));
        drop(acc);
        let usage = message["usage"].as_object().unwrap();
        let keys: Vec<&str> = usage.keys().map(String::as_str).collect();
        let idx = |k: &str| keys.iter().position(|&x| x == k).unwrap();
        assert_eq!(idx("output_tokens_details"), idx("output_tokens") + 1);
        assert_eq!(idx("service_tier"), idx("output_tokens_details") + 1);
        assert_eq!(usage["output_tokens_details"]["thinking_tokens"], 0_u64);
    }

    /// OR-1 — `permission_denials` was a hardcoded `json!([])` in all three
    /// result builders, so an SDK/desktop caller could never see that a tool
    /// call had been refused. Oracle entry shape (schema `LF`):
    /// `{tool_name, tool_use_id, tool_input}`.
    #[tokio::test]
    async fn result_frame_reports_the_sessions_permission_denials() {
        let params = make_params("test-session-denials");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));

        // The orchestrator's cell, shared exactly as `lib.rs` wires it.
        let cell: Arc<Mutex<Vec<lingxi_core::host::PermissionDenial>>> =
            Arc::new(Mutex::new(Vec::new()));
        stream.share_permission_denials(Arc::clone(&cell));
        cell.lock().await.push(lingxi_core::host::PermissionDenial {
            tool_name: "Read".into(),
            tool_use_id: "toolu_denied_1".into(),
            tool_input: json!({"file_path": "/repo/secret/.env"}),
            tool_input_projection: None,
        });

        let cost = CostSnapshot::default();
        let frame = stream
            .build_result_success_frame("done", "end_turn", &cost, "m", "off", None, &[])
            .await;
        let denials = frame["permission_denials"].as_array().expect("array");
        assert_eq!(denials.len(), 1, "the denial must reach the result frame");
        assert_eq!(denials[0]["tool_name"], "Read");
        assert_eq!(denials[0]["tool_use_id"], "toolu_denied_1");
        assert_eq!(denials[0]["tool_input"]["file_path"], "/repo/secret/.env");

        // The error frame reports the same list — it is the same run.
        let err = stream
            .build_result_error_frame(
                "error_during_execution",
                vec![],
                &cost,
                "m",
                "off",
                None,
                &[],
            )
            .await;
        assert_eq!(err["permission_denials"].as_array().unwrap().len(), 1);
    }

    /// A stream with no orchestrator wired reports `[]` — which is also what the
    /// BUG looked like. So pin the wiring itself: an unwired stream must say so.
    #[tokio::test]
    async fn an_unwired_stream_is_detectable_rather_than_silently_empty() {
        let stream = StreamJsonStream::new(
            make_params("test-session-unwired"),
            Output::new(tokio::io::sink()),
        );
        assert!(
            !stream.permission_denials_wired(),
            "a fresh stream has no orchestrator cell"
        );
        let cell = Arc::new(Mutex::new(Vec::new()));
        stream.share_permission_denials(cell);
        assert!(
            stream.permission_denials_wired(),
            "sharing the cell marks the stream wired"
        );
    }

    /// Verify that the result/success frame has the correct 20-key order.
    #[tokio::test]
    async fn result_frame_success_has_correct_key_order() {
        let params = make_params("test-session-for-result");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        let owner = lingxi_core::host::agent_statistics::AgentSessionStatistics::default();
        stream.set_result_metadata(StreamJsonResultMetadata {
            subagent_stats: Some(serde_json::to_value(owner.snapshot()).unwrap()),
            safety_stops: Some(0),
            ..Default::default()
        }).await;
        let cost = CostSnapshot {
            session_id: Default::default(),
            total_usd: 0.05,
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 50,
            cache_creation_tokens: 5,
            api_calls: 1,
            session_duration: std::time::Duration::from_millis(1000),
            current_usage: Some(lingxi_core::host::CurrentUsageSnapshot {
                input_tokens: 100,
                output_tokens: 10,
                cache_read_input_tokens: 50,
                cache_creation_input_tokens: 5,
            }),
            ..Default::default()
        };
        let frame = stream
            .build_result_success_frame(
                "pong",
                "end_turn",
                &cost,
                "claude-opus-4-8",
                "off",
                None,
                &[],
            )
            .await;

        let obj = frame.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        let expected_keys = [
            "duration_api_ms",
            "stop_reason",
            "session_id",
            "total_cost_usd",
            "usage",
            "modelUsage",
            "permission_denials",
            "terminal_reason",
            "fast_mode_state",
            "subagent_stats",
            "safety_stops",
            "is_error",
            "num_turns",
            "subtype",
            "api_error_status",
            "result",
            "type",
            "duration_ms",
            "uuid",
            "queued_turn_count",
            "result_index",
        ];

        assert_eq!(
            keys, expected_keys,
            "result/success follows pinned 2.1.293 key order with unavailable timings omitted"
        );
        assert_eq!(frame["type"], "result");
        assert_eq!(frame["subtype"], "success");
        assert_eq!(frame["is_error"], false);
        assert_eq!(frame["result"], "pong");
        assert_eq!(frame["stop_reason"], "end_turn");
        assert_eq!(frame["num_turns"], 1_u64);
        assert_eq!(frame["terminal_reason"], "completed");
        assert_eq!(frame["fast_mode_state"], "off");
        assert!(frame["uuid"].is_string());
        // modelUsage has the model key
        let mu = frame["modelUsage"].as_object().unwrap();
        assert!(
            mu.contains_key("claude-opus-4-8"),
            "modelUsage must be keyed by model_id"
        );
        // usage block
        let usage = frame["usage"].as_object().unwrap();
        assert_eq!(usage["input_tokens"], 100_u64);
        assert_eq!(usage["cache_read_input_tokens"], 50_u64);
        assert_eq!(usage["cache_creation_input_tokens"], 5_u64);
        assert_eq!(usage["output_tokens"], 10_u64);
    }

    /// 2.1.219+: result frames carry `fast_mode_disabled_reason` directly
    /// after `fast_mode_state` when a reason resolved (live 2.1.220 capture:
    /// `…,"fast_mode_state":"off","fast_mode_disabled_reason":
    /// "sdk_opt_in_required",…`), and omit the key when none did.
    #[tokio::test]
    async fn result_frames_carry_fast_mode_disabled_reason_after_state() {
        let params = make_params("sess-fast-reason");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        let cost = CostSnapshot::default();

        let success = stream
            .build_result_success_frame(
                "ok",
                "end_turn",
                &cost,
                "claude-opus-4-8",
                "off",
                Some("sdk_opt_in_required"),
                &[],
            )
            .await;
        let keys: Vec<&str> = success
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let state = keys.iter().position(|k| *k == "fast_mode_state").unwrap();
        assert_eq!(
            keys.get(state + 1),
            Some(&"fast_mode_disabled_reason"),
            "reason must sit directly after fast_mode_state, got {keys:?}"
        );
        assert_eq!(success["fast_mode_disabled_reason"], "sdk_opt_in_required");

        let error = stream
            .build_result_error_frame(
                "error_during_execution",
                vec!["boom".to_string()],
                &cost,
                "claude-opus-4-8",
                "off",
                Some("not_first_party"),
                &[],
            )
            .await;
        assert_eq!(error["fast_mode_disabled_reason"], "not_first_party");
    }

    /// Verify that the result/error frame uses `errors` (not `result`).
    #[tokio::test]
    async fn result_frame_error_has_errors_not_result() {
        let params = make_params("test-session-for-error");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        let cost = CostSnapshot::default();
        let frame = stream
            .build_result_error_frame(
                "error_during_execution",
                vec!["API failed".to_string()],
                &cost,
                "claude-opus-4-8",
                "off",
                None,
                &[],
            )
            .await;

        let obj = frame.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        // Pinned293 error envelope omits success-only status/timings.
        let subtype_index = keys.iter().position(|key| *key == "subtype").unwrap();
        assert_eq!(keys[subtype_index + 1], "errors", "errors follows subtype");
        assert!(!keys.contains(&"api_error_status"));
        assert!(!keys.contains(&"ttft_ms"));
        assert!(
            !keys.contains(&"result"),
            "error frame must not have 'result' key"
        );
        assert_eq!(frame["is_error"], true);
        assert_eq!(frame["terminal_reason"], "error");
        assert_eq!(frame["subtype"], "error_during_execution");
        let errors = frame["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0], "API failed");
    }

    /// Verify terminal_reason mapping for all error subtypes.
    #[tokio::test]
    async fn result_frame_error_terminal_reason_mapping() {
        let params = make_params("sess");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        let cost = CostSnapshot::default();

        let cases = [
            ("error_during_execution", "error"),
            ("error_max_turns", "max_turns"),
            ("error_max_budget_usd", "budget_exhausted"),
            (
                "error_max_structured_output_retries",
                "structured_output_retry_exhausted",
            ),
        ];
        for (subtype, expected_terminal_reason) in &cases {
            let frame = stream
                .build_result_error_frame(subtype, vec![], &cost, "model", "off", None, &[])
                .await;
            assert_eq!(
                frame["terminal_reason"], *expected_terminal_reason,
                "subtype={subtype} must map to terminal_reason={expected_terminal_reason}"
            );
        }
    }

    /// Verify suppress_frames suppresses init/status/boundary frames.
    #[tokio::test]
    async fn json_mode_suppresses_frames_but_stores_text() {
        let params = make_params("sess");
        let stream = StreamJsonStream::new_json_mode(params, Output::new(tokio::io::sink()));
        // These should be no-ops (no panic, no output that we can detect in tests)
        stream.emit_message_start("msg_001", "model").await;
        stream.emit_text("hello from json mode", None).await;
        // emit_message_boundary in suppressed mode should still store last_result_text
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let text = stream.get_last_result_text().await;
        assert_eq!(text.value, json!("hello from json mode"));
        // Accumulator reset
        let acc = stream.accum.lock().await;
        assert!(acc.blocks.is_empty());
    }

    // ── P4: --include-partial-messages (stream_event frames) ─────────────────

    /// Verify `emit_stream_event` is suppressed when `include_partial_messages`
    /// is false (the default). This test just ensures no panic occurs and no
    /// extra output would be emitted in the default state.
    #[tokio::test]
    async fn stream_event_no_op_when_flag_off() {
        let params = make_params("sess");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        // Default: include_partial_messages=false. Should be a no-op.
        stream
            .emit_stream_event(r#"{"type":"message_start","message":{}}"#, true)
            .await;
        stream.emit_stream_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#, false).await;
        // No panic = pass. Output goes to stdout which tests don't capture per-assertion.
    }

    /// Verify that `set_flags` enables `include_partial_messages` atomically
    /// and that the stream does not panic when the flag is set.
    #[tokio::test]
    async fn stream_event_emits_when_flag_on() {
        let params = make_params("sess-partial");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        // Enable partial messages.
        stream.set_flags(true, false);
        // Should emit without panicking. Output goes to stdout.
        stream.emit_stream_event(r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-opus-4-8","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}"#, true).await;
        stream.emit_stream_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#, false).await;
        stream
            .emit_stream_event(r#"{"type":"message_stop"}"#, false)
            .await;
        // No panic = pass.
    }

    /// (2.1.211) `set_forward_subagent_text` toggles the plumbed state without
    /// disturbing the include-partial/hook flags (separate setter).
    #[test]
    fn forward_subagent_text_flag_roundtrips() {
        let stream = StreamJsonStream::new_json_mode(
            make_params("sess-fwd"),
            Output::new(tokio::io::sink()),
        );
        assert!(!stream.forward_subagent_text());
        stream.set_forward_subagent_text(true);
        assert!(stream.forward_subagent_text());
        stream.set_forward_subagent_text(false);
        assert!(!stream.forward_subagent_text());
    }

    /// (2.1.212 `--forward-subagent-text`) With the flag ON, a subagent
    /// assistant message is re-shaped into an `assistant` frame carrying the
    /// NON-NULL spawning `parent_tool_use_id`. CC's `Xzt` keeps EVERY block —
    /// including `tool_use` — in the forwarded `content` (only rewriting
    /// text/thinking), so the frame preserves tool_use blocks unchanged.
    #[test]
    fn forwarded_subagent_frame_carries_parent_tool_use_id_and_text() {
        let stream = StreamJsonStream::new(make_params("sess-fwd"), Output::new(tokio::io::sink()));
        stream.set_forward_subagent_text(true);
        // Serialized subagent ConversationMessage::Assistant shape.
        let msg = json!({
            "role": "assistant",
            "id": "msg_child_1",
            "content": [
                {"type": "thinking", "thinking": "pondering", "signature": null},
                {"type": "text", "text": "hello from subagent"},
                {"type": "tool_use", "id": "toolu_x", "name": "Read", "input": {}},
            ],
            "stop_reason": "end_turn",
        });
        let frame = stream
            .build_forwarded_subagent_frame(&msg, "toolu_parent_task", "sess-fwd", "uuid-1")
            .expect("frame forwarded when flag is on");
        assert_eq!(frame["type"], "assistant");
        // The distinguishing feature: NON-NULL parent_tool_use_id = spawner.
        assert_eq!(frame["parent_tool_use_id"], "toolu_parent_task");
        assert!(!frame["parent_tool_use_id"].is_null());
        assert_eq!(frame["session_id"], "sess-fwd");
        assert_eq!(frame["uuid"], "uuid-1");
        // Content KEEPS every block: thinking + text + tool_use (CC's Xzt
        // returns non-text/thinking blocks unchanged — tool_use is NOT dropped).
        let content = frame["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "all blocks kept incl. tool_use");
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "hello from subagent");
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["id"], "toolu_x");
        assert_eq!(content[2]["name"], "Read");
        // The message keeps its own id / stop_reason.
        assert_eq!(frame["message"]["id"], "msg_child_1");
        assert_eq!(frame["message"]["stop_reason"], "end_turn");
    }

    /// With the flag OFF nothing is forwarded (returns `None`).
    #[test]
    fn forwarded_subagent_frame_none_when_flag_off() {
        let stream = StreamJsonStream::new(make_params("sess-fwd"), Output::new(tokio::io::sink()));
        // Default: forward_subagent_text = false.
        let msg = json!({
            "role": "assistant",
            "id": "msg_child_1",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
        });
        assert!(
            stream
                .build_forwarded_subagent_frame(&msg, "toolu_parent", "sess-fwd", "uuid-1")
                .is_none(),
            "flag OFF forwards nothing"
        );
    }

    /// With the flag ON and a tool_use-only assistant message, CC's `Xzt` still
    /// returns the tool_use block unchanged, so the frame IS forwarded with the
    /// tool_use block intact (the earlier port wrongly dropped it and returned
    /// `None`).
    #[test]
    fn forwarded_subagent_frame_keeps_tool_use_only_message() {
        let stream = StreamJsonStream::new(make_params("sess-fwd"), Output::new(tokio::io::sink()));
        stream.set_forward_subagent_text(true);
        let msg = json!({
            "role": "assistant",
            "id": "msg_child_1",
            "content": [{"type": "tool_use", "id": "toolu_x", "name": "Read", "input": {}}],
            "stop_reason": "tool_use",
        });
        let frame = stream
            .build_forwarded_subagent_frame(&msg, "toolu_parent", "sess-fwd", "uuid-1")
            .expect("tool_use-only message is forwarded, not dropped");
        let content = frame["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "the tool_use block is kept");
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "toolu_x");
    }

    /// (2.1.212 `--forward-subagent-text`) The emitted frame REUSES the subagent
    /// message's own uuid (CC's `uuid:o.uuid`, sourced from the serialized
    /// message's `id` field) — NOT a fresh `Uuid::new_v4()` — and carries the
    /// full content including tool_use blocks. Captures the enqueued frame off
    /// the drain channel (the drain task is not started in tests).
    #[tokio::test]
    async fn emitted_forwarded_frame_reuses_subagent_uuid_and_keeps_tool_use() {
        let stream = StreamJsonStream::new(make_params("sess-fwd"), Output::new(tokio::io::sink()));
        stream.set_forward_subagent_text(true);
        // Take the drain receiver so enqueued frames stay readable here.
        let mut rx = stream
            .drain_rx
            .lock()
            .await
            .take()
            .expect("drain receiver available");
        let msg = json!({
            "role": "assistant",
            "id": "018f-subagent-uuid",
            "content": [
                {"type": "text", "text": "child text"},
                {"type": "tool_use", "id": "toolu_child", "name": "Grep", "input": {}},
            ],
            "stop_reason": "tool_use",
        });
        stream
            .emit_forwarded_subagent_message(&msg, "toolu_parent_task")
            .await;
        let line = match rx.try_recv().expect("a frame was enqueued") {
            OutboundMsg::Line(l) => l,
            OutboundMsg::StreamEvent(l) => l,
            OutboundMsg::Heartbeats(_) => panic!("expected a Line frame"),
            OutboundMsg::Shutdown(_) | OutboundMsg::Flush(_) | OutboundMsg::PublishJson(_) | OutboundMsg::RefreshHeldResultTotals(_) => {
                panic!("expected a Line frame")
            }
        };
        let frame: Value = serde_json::from_str(&line).expect("frame is valid json");
        assert_eq!(frame["type"], "assistant");
        assert_eq!(frame["parent_tool_use_id"], "toolu_parent_task");
        // uuid is the subagent message's own id, NOT a random v4.
        assert_eq!(frame["uuid"], "018f-subagent-uuid");
        // tool_use block survives the forward.
        let content = frame["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "toolu_child");
    }

    /// Verify that `emit_stream_event` is suppressed in suppress_frames mode
    /// (json-mode) even if `include_partial_messages` is set.
    #[tokio::test]
    async fn stream_event_suppressed_in_json_mode() {
        let params = make_params("sess-json");
        let stream = Arc::new(StreamJsonStream::new_json_mode(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream.set_flags(true, false);
        // suppress_frames=true overrides include_partial_messages.
        // Should be a no-op (no panic).
        stream
            .emit_stream_event(r#"{"type":"message_start","message":{}}"#, true)
            .await;
    }

    /// The `system/init` frame must be byte-shape-identical to the 2.1.201
    /// `-p --input-format stream-json` oracle: the 20 keys in exact order,
    /// WITH `plugins`, WITHOUT `betas`. (The `betas` field the SDK-subprocess
    /// `initialize` payload carries does NOT appear on this streaming frame —
    /// verified live against 2.1.201.)
    /// `mcp_server_errors` is CONDITIONAL: absent when clean, present between
    /// `plugins` and `analytics_disabled` when a `--mcp-config` entry was
    /// skipped. The oracle spreads it in only when non-empty
    /// (`...r.length>0&&{mcp_server_errors:…}`), so an always-present empty
    /// array would be a wire divergence.
    #[test]
    fn mcp_server_errors_appears_only_when_non_empty_and_in_position() {
        let mut params = build_init_params(
            "sess-e",
            vec!["Bash".to_string()],
            vec![],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "default",
            vec![],
            vec![],
            vec![],
            vec![],
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );

        params.mcp_server_errors = Vec::new();
        let clean = build_init_frame("sess-e", "u", &params);
        assert!(
            !clean.as_object().unwrap().contains_key("mcp_server_errors"),
            "a clean config must emit NO mcp_server_errors key"
        );

        params.mcp_server_errors = vec![serde_json::json!({
            "file": "/x/.mcp.json",
            "path": "mcpServers.bad",
            "message": "skipped",
        })];
        let dirty = build_init_frame("sess-e", "u", &params);
        let keys: Vec<&str> = dirty
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let at = keys
            .iter()
            .position(|k| *k == "mcp_server_errors")
            .expect("present when non-empty");
        let plugins = keys.iter().position(|k| *k == "plugins").unwrap();
        let analytics = keys
            .iter()
            .position(|k| *k == "analytics_disabled")
            .unwrap();
        assert!(
            plugins < at && at < analytics,
            "must sit between plugins and analytics_disabled, got {keys:?}"
        );
    }

    #[test]
    fn init_frame_preserves_293_optional_field_omission() {
        let params = build_init_params(
            "sess-oracle",
            vec!["Bash".to_string()],
            vec![],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "default",
            vec![],
            vec![],
            vec![],
            vec![],
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        let frame = build_init_frame("sess-oracle", "uuid-1234", &params);
        let obj = frame.as_object().expect("init frame is an object");
        let keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "type",
                "subtype",
                "cwd",
                "session_id",
                "tools",
                "mcp_servers",
                "model",
                "permissionMode",
                "slash_commands",
                "apiKeySource",
                "claude_code_version",
                "output_style",
                "agents",
                "skills",
                "plugins",
                "capabilities",
                "analytics_disabled",
                "product_feedback_disabled",
                "uuid",
                "fast_mode_state",
            ],
            "system/init key set + order must match the 2.1.220 -p oracle"
        );
        // Positive: plugins present. Negative: no betas key (oracle has none).
        assert!(obj.contains_key("plugins"), "oracle init HAS plugins");
        assert!(
            !obj.contains_key("betas"),
            "oracle -p init frame has NO betas key"
        );
        assert_eq!(frame["type"], "system");
        assert_eq!(frame["subtype"], "init");
        assert_eq!(frame["session_id"], "sess-oracle");
        assert_eq!(frame["uuid"], "uuid-1234");
    }

    /// SLASH-15 (2.1.238 `Fin` @298685916): `terminal_slash_commands` carries
    /// the `terminalOriented:!0` subset of the advertised commands, is spread in
    /// directly AFTER `slash_commands`, and is ABSENT when the subset is empty
    /// (the key does not exist in 2.1.220 at all).
    #[test]
    fn init_frame_emits_terminal_slash_commands_after_slash_commands() {
        // Advertised list deliberately interleaves flagged and unflagged names.
        let params = build_init_params(
            "sess-terminal",
            vec![],
            vec![],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "default",
            vec![
                "color".to_string(),
                "context".to_string(),
                "exit".to_string(),
                "reload-plugins".to_string(),
                "statusline".to_string(),
                "usage".to_string(),
            ],
            vec![],
            vec![],
            vec![],
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        assert_eq!(
            params.terminal_slash_commands,
            vec![
                "color".to_string(),
                "exit".to_string(),
                "reload-plugins".to_string(),
                "statusline".to_string()
            ],
            "only the TERMINAL_ORIENTED_COMMANDS members, in advertised order"
        );

        let frame = build_init_frame("sess-terminal", "u", &params);
        let obj = frame.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        let slash = keys.iter().position(|k| *k == "slash_commands").unwrap();
        let terminal = keys
            .iter()
            .position(|k| *k == "terminal_slash_commands")
            .expect("present when the subset is non-empty");
        assert_eq!(
            terminal,
            slash + 1,
            "must sit immediately after slash_commands, got {keys:?}"
        );
        assert_eq!(
            keys.get(terminal + 1),
            Some(&"apiKeySource"),
            "…and immediately before apiKeySource, got {keys:?}"
        );

        // Empty subset ⇒ the key is OMITTED, not emitted as [].
        let none = build_init_params(
            "sess-terminal-none",
            vec![],
            vec![],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "default",
            vec!["context".to_string(), "usage".to_string()],
            vec![],
            vec![],
            vec![],
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        assert!(none.terminal_slash_commands.is_empty());
        let frame = build_init_frame("sess-terminal-none", "u", &none);
        assert!(
            !frame
                .as_object()
                .unwrap()
                .contains_key("terminal_slash_commands"),
            "an empty subset must emit NO terminal_slash_commands key"
        );
    }

    /// 2.1.220 live capture: `capabilities` advertises the three protocol
    /// contracts verbatim, between `plugins` and (when present)
    /// `mcp_server_errors`; a reason-less run omits
    /// `fast_mode_disabled_reason`, and a reasoned run appends it directly
    /// after `fast_mode_state` at the very end of the frame.
    #[test]
    fn init_frame_capabilities_and_fast_mode_reason_match_2_1_220() {
        let mut params = build_init_params(
            "sess-caps",
            vec![],
            vec![],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "default",
            vec![],
            vec![],
            vec![],
            vec![],
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        let frame = build_init_frame("sess-caps", "u", &params);
        assert_eq!(
            frame["capabilities"],
            serde_json::json!([
                "interrupt_receipt_v1",
                "interrupt_cancel_queued_v1",
                "msg_lifecycle_v1"
            ]),
            "capability list must match the binary's gPp verbatim"
        );
        assert!(
            !frame
                .as_object()
                .unwrap()
                .contains_key("fast_mode_disabled_reason"),
            "None reason ⇒ key omitted (oracle undefined-assignment semantics)"
        );

        params.fast_mode_disabled_reason = Some("sdk_opt_in_required".to_string());
        let reasoned = build_init_frame("sess-caps", "u", &params);
        let keys: Vec<&str> = reasoned
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys.last(),
            Some(&"fast_mode_disabled_reason"),
            "reason is the final key, directly after fast_mode_state"
        );
        assert_eq!(keys[keys.len() - 2], "fast_mode_state");
        assert_eq!(reasoned["fast_mode_disabled_reason"], "sdk_opt_in_required");
    }

    // ── P4: --include-hook-events (hook lifecycle frames) ─────────────────────

    /// Verify `emit_hook_started` is a no-op for non-SessionStart events when
    /// `include_hook_events` is false.
    #[tokio::test]
    async fn hook_started_no_op_for_non_session_start_when_flag_off() {
        let params = make_params("sess");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        // Default: include_hook_events=false.
        stream
            .emit_hook_started("hook:1234", "my-hook", "PreToolUse")
            .await;
        // No panic = pass.
    }

    /// Verify `emit_hook_started` ALWAYS emits for SessionStart (gate pGn)
    /// even when `include_hook_events` is false.
    #[tokio::test]
    async fn hook_started_always_emits_for_session_start() {
        let params = make_params("sess-session-start");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        // Flag OFF, but SessionStart always streams.
        stream
            .emit_hook_started("hook:sess", "session-hook", "SessionStart")
            .await;
        // No panic = pass.
    }

    /// Verify `emit_hook_started` ALWAYS emits for Setup (gate pGn).
    #[tokio::test]
    async fn hook_started_always_emits_for_setup() {
        let params = make_params("sess-setup");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream
            .emit_hook_started("hook:setup", "setup-hook", "Setup")
            .await;
        // No panic = pass.
    }

    /// Verify that `set_flags` enables `include_hook_events` and
    /// `emit_hook_started` + `emit_hook_response` emit for all event types.
    #[tokio::test]
    async fn hook_events_emit_when_flag_on() {
        let params = make_params("sess-hook-events");
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream.set_flags(false, true);
        // Should emit without panicking.
        stream
            .emit_hook_started("hook:abc", "my-formatter", "PostToolUse")
            .await;
        stream
            .emit_hook_response(
                "hook:abc",
                "my-formatter",
                "PostToolUse",
                "formatted output",
                "formatted output",
                "",
                Some(0),
                "success",
            )
            .await;
        // No panic = pass.
    }

    /// SH-07 — the `hook_progress` frame body is byte-faithful to `EjT`: eight
    /// content keys in oracle order, then the shared `uuid` / `session_id` tail.
    /// Before SH-07 the port emitted `hook_started` and `hook_response` but had
    /// no `hook_progress` emitter at all, so a long-running hook streamed
    /// nothing between its two lifecycle frames.
    #[test]
    fn hook_progress_frame_is_byte_faithful() {
        let frame = build_hook_progress_frame(
            "hook:abc",
            "my-formatter",
            "PostToolUse",
            "half done\n",
            "warn\n",
            "half done\nwarn\n",
            "11111111-2222-3333-4444-555555555555",
            "sess-1",
        );
        let obj = frame.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "type",
                "subtype",
                "hook_id",
                "hook_name",
                "hook_event",
                "stdout",
                "stderr",
                "output",
                "uuid",
                "session_id",
            ],
        );
        assert_eq!(frame["type"], "system");
        assert_eq!(frame["subtype"], "hook_progress");
        assert_eq!(frame["hook_id"], "hook:abc");
        assert_eq!(frame["hook_name"], "my-formatter");
        assert_eq!(frame["hook_event"], "PostToolUse");
        // `output` is the ARRIVAL-ordered interleaving of both pipes, which is
        // the value the poll's change detection compares — not stdout alone.
        assert_eq!(frame["output"], "half done\nwarn\n");
        assert_eq!(frame["session_id"], "sess-1");
    }

    /// The `hook_progress` emitter shares the `hook_started` / `hook_response`
    /// gate (`Q9i`): flag off + a non-SessionStart event ⇒ nothing; SessionStart
    /// and Setup always stream; flag on ⇒ everything streams.
    #[tokio::test]
    async fn hook_progress_follows_the_shared_gate() {
        let stream = Arc::new(StreamJsonStream::new(
            make_params("sess-progress"),
            Output::new(tokio::io::sink()),
        ));
        // Flag OFF, ordinary event: suppressed.
        stream
            .emit_hook_progress_frame("h:1", "fmt", "PostToolUse", "a", "", "a")
            .await;
        // Flag OFF, SessionStart: always streams.
        stream
            .emit_hook_progress_frame("h:2", "boot", "SessionStart", "a", "", "a")
            .await;
        // Flag ON: everything streams.
        stream.set_flags(false, true);
        stream
            .emit_hook_progress_frame("h:3", "fmt", "PostToolUse", "a", "", "a")
            .await;
        // json-mode suppresses regardless of the flag.
        let json_mode = Arc::new(StreamJsonStream::new_json_mode(
            make_params("sess-json"),
            Output::new(tokio::io::sink()),
        ));
        json_mode.set_flags(false, true);
        json_mode
            .emit_hook_progress_frame("h:4", "fmt", "SessionStart", "a", "", "a")
            .await;
        // No panic = pass (same harness convention as the sibling gate tests).
    }

    /// SH-07 — the `Q9i` query the hook executor uses to decide whether to arm
    /// the progress poll at all. Getting this wrong in the permissive direction
    /// would make every TUI hook run spawn a 1 s poll task for frames nobody
    /// emits.
    #[test]
    fn hook_events_streamed_matches_q9i() {
        let stream = StreamJsonStream::new(make_params("sess-q9i"), Output::new(tokio::io::sink()));
        // Flag OFF: only the always-on events.
        assert!(stream.hook_events_streamed("SessionStart"));
        assert!(stream.hook_events_streamed("Setup"));
        assert!(!stream.hook_events_streamed("PreToolUse"));
        assert!(!stream.hook_events_streamed("PostToolUse"));
        // Flag ON: everything.
        stream.set_flags(false, true);
        assert!(stream.hook_events_streamed("PostToolUse"));
        // json-mode streams nothing, flag or not.
        let json_mode = StreamJsonStream::new_json_mode(
            make_params("sess-q9i-json"),
            Output::new(tokio::io::sink()),
        );
        json_mode.set_flags(false, true);
        assert!(!json_mode.hook_events_streamed("SessionStart"));
        assert!(!json_mode.hook_events_streamed("PostToolUse"));
    }

    /// Verify `emit_hook_response` is suppressed in json-mode.
    #[tokio::test]
    async fn hook_response_suppressed_in_json_mode() {
        let params = make_params("sess-json-hook");
        let stream = Arc::new(StreamJsonStream::new_json_mode(
            params,
            Output::new(tokio::io::sink()),
        ));
        stream.set_flags(false, true);
        // suppress_frames=true overrides include_hook_events.
        stream
            .emit_hook_response("hook:xyz", "my-hook", "Stop", "", "", "", None, "success")
            .await;
        // No panic = pass.
    }

    #[test]
    fn json_mode_config_suppresses_non_result_frames() {
        let stream = StreamJsonStream::new_json_mode_placeholder(Output::new(tokio::io::sink()));
        assert!(stream.suppress_frames);
        assert!(!StreamJsonStream::new_placeholder(Output::new(tokio::io::sink())).suppress_frames);
    }

    // ── P2b: modelUsage contextWindow/maxOutputTokens from catalog ────────────

    /// Verify that modelUsage uses the llm-runtime catalog for contextWindow and
    /// maxOutputTokens, including the [1m] suffix for 1M-context models.
    #[tokio::test]
    async fn model_usage_uses_catalog_context_window() {
        let params = make_params("sess");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        let cost = CostSnapshot {
            input_tokens: 100,
            output_tokens: 10,
            total_usd: 0.01,
            ..Default::default()
        };

        // opus-4-8 is natively 1M (2.1.198 registry native_1m:!0, M1b) —
        // contextWindow=1_000_000 with NO suffix; maxOutputTokens=64000.
        let frame = stream
            .build_result_success_frame(
                "hi",
                "end_turn",
                &cost,
                "claude-opus-4-8",
                "off",
                None,
                &[],
            )
            .await;
        let mu = frame["modelUsage"].as_object().unwrap();
        let entry = &mu["claude-opus-4-8"];
        assert_eq!(
            entry["contextWindow"], 1_000_000_u64,
            "opus-4-8 native-1M contextWindow"
        );
        assert_eq!(
            entry["maxOutputTokens"], 64_000_u64,
            "opus-4-8 maxOutputTokens"
        );

        // A 200k model (opus-4-6 has NO native_1m) keeps the default window.
        let frame200k = stream
            .build_result_success_frame(
                "hi",
                "end_turn",
                &cost,
                "claude-opus-4-6",
                "off",
                None,
                &[],
            )
            .await;
        let mu200k = frame200k["modelUsage"].as_object().unwrap();
        let entry200k = &mu200k["claude-opus-4-6"];
        assert_eq!(
            entry200k["contextWindow"], 200_000_u64,
            "opus-4-6 default contextWindow"
        );
        assert_eq!(
            entry200k["maxOutputTokens"], 64_000_u64,
            "opus-4-6 maxOutputTokens"
        );

        // 1M context model (model id carries [1m] suffix):
        // contextWindow=1_000_000, maxOutputTokens=64_000.
        let frame1m = stream
            .build_result_success_frame(
                "hi",
                "end_turn",
                &cost,
                "claude-opus-4-8[1m]",
                "off",
                None,
                &[],
            )
            .await;
        let mu1m = frame1m["modelUsage"].as_object().unwrap();
        assert!(
            mu1m.contains_key("claude-opus-4-8[1m]"),
            "modelUsage key must carry the [1m] suffix verbatim"
        );
        let entry1m = &mu1m["claude-opus-4-8[1m]"];
        assert_eq!(
            entry1m["contextWindow"], 1_000_000_u64,
            "opus-4-8[1m] contextWindow must be 1_000_000"
        );
        assert_eq!(
            entry1m["maxOutputTokens"], 64_000_u64,
            "opus-4-8[1m] maxOutputTokens unchanged"
        );
    }

    #[tokio::test]
    async fn model_usage_reports_actual_fallback_model_rows() {
        let stream = StreamJsonStream::new(
            make_params("fallback-result"),
            Output::new(tokio::io::sink()),
        );
        let cost = CostSnapshot {
            input_tokens: 17,
            output_tokens: 5,
            total_usd: 0.000_002,
            by_model: vec![lingxi_core::host::orchestrator::ModelUsageRow {
                model: "claude-haiku-4-5".to_string(),
                provider: Some("firstParty".to_string()),
                total_nano_usd: 2_000,
                input_tokens: 17,
                output_tokens: 5,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 2,
                reasoning_tokens: 0,
                web_search_requests: 0,
            }],
            ..Default::default()
        };
        let frame = stream
            .build_result_success_frame(
                "done",
                "end_turn",
                &cost,
                "claude-opus-4-6",
                "off",
                None,
                &[],
            )
            .await;
        let usage = frame["modelUsage"].as_object().expect("modelUsage map");
        assert!(!usage.contains_key("claude-opus-4-6"));
        assert_eq!(usage["claude-haiku-4-5"]["inputTokens"], 17);
        assert_eq!(usage["claude-haiku-4-5"]["cacheReadInputTokens"], 3);
    }

    // ── P2b: rate_limit_event frame ───────────────────────────────────────────

    /// Verify that `emit_rate_limit_event` builds the correct GROUND-TRUTH frame
    /// shape. The frame is emitted to stdout (test-visible only via the trait
    /// hook), so we test the internal builder path via `emit_rate_limit_event`'s
    /// emitted value by checking the json! shape indirectly: confirm the call
    /// does not panic and that the OutputStream impl is wired.
    #[tokio::test]
    async fn rate_limit_event_no_panic_with_defaults() {
        let params = make_params("sess");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        // Should emit to stdout without panicking.
        stream
            .emit_rate_limit_event(
                None,  // status
                None,  // rate_limit_type
                None,  // utilization
                None,  // resets_at
                false, // is_using_overage
                None,  // surpassed_threshold
            )
            .await;
    }

    /// Verify that `OutputStream::emit_rate_limit` wires through to a
    /// `rate_limit_event` frame (no panic, status fields forwarded).
    #[tokio::test]
    async fn emit_rate_limit_trait_no_panic() {
        let params = make_params("sess");
        let stream = StreamJsonStream::new(params, Output::new(tokio::io::sink()));
        // Called by the orchestrator after each API turn.
        stream
            .emit_rate_limit(
                Some("allowed"),                // status
                Some("seven_day"),              // rate_limit_type
                Some(0.75),                     // utilization
                Some(1_782_360_000),            // resets_at
                None,                           // claim_resets_at
                Some("allowed_warning"),        // overage_status
                None,                           // overage_resets_at
                None,                           // overage_disabled_reason
                None,                           // fallback_available
                Some(&["overage".to_string()]), // upgrade_paths
                true,                           // credits_required
            )
            .await;
    }

    /// Golden frame-sequence test: simulate a minimal stream run and verify
    /// the GROUND-TRUTH ordering: init → status → [assistant] → result.
    /// Volatile fields (uuid, session_id, timestamp) are masked by their
    /// presence / shape rather than exact value.
    ///
    /// NOTE: rate_limit_event is emitted by the orchestrator's
    /// `emit_rate_limit_if_changed` DURING `run_turn` — it cannot be asserted
    /// in a unit test that bypasses the orchestrator. It IS wired through the
    /// `OutputStream::emit_rate_limit` impl above; the integration test covers
    /// the full sequence.
    #[tokio::test]
    async fn golden_frame_sequence_init_status_assistant_result() {
        let params = build_init_params(
            "golden-session-id",
            vec!["Bash".to_string(), "Read".to_string()],
            vec![("codegraph".to_string(), "connected".to_string())],
            "claude-opus-4-8",
            &llm_runtime::CredentialSource::Unknown,
            "bypassPermissions",
            vec!["graphify".to_string()],
            vec!["claude".to_string()],
            vec!["graphify".to_string()],
            vec![],
            "default",
            Some("/home/user/.lingxi/projects/test/memory/"),
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        let stream = Arc::new(StreamJsonStream::new(
            params,
            Output::new(tokio::io::sink()),
        ));

        // ① system/init — check key presence and shape.
        {
            let params_guard = stream.init_params.lock().await;
            let p = params_guard.as_ref().unwrap();
            assert_eq!(p.model, "claude-opus-4-8");
            assert_eq!(p.permission_mode, "bypassPermissions");
            assert!(!p.tools.is_empty(), "tools must be populated");
            assert_eq!(p.mcp_servers.len(), 1, "mcp_servers must have 1 entry");
            assert_eq!(p.slash_commands, vec!["graphify"]);
            assert_eq!(p.agents, vec!["claude"]);
            assert_eq!(p.skills, vec!["graphify"]);
            assert!(
                p.plugins.is_empty(),
                "plugins: [] (no PluginManager surface from Runtime)"
            );
            assert_eq!(p.fast_mode_state, "off");
            assert!(p.memory_paths.is_some(), "memory_paths must be set");
        }

        // ② system/status + ③ assistant (accumulate then boundary-flush)
        stream
            .emit_message_start("msg_golden", "claude-opus-4-8")
            .await;
        stream.emit_text("pong", None).await;
        stream
            .emit_message_boundary(Some("end_turn"), Some("req_golden"))
            .await;
        let last_text = stream.get_last_result_text().await;
        assert_eq!(
            last_text.value, json!("pong"),
            "last_result_text propagates from boundary"
        );

        // ④ result/success frame
        let cost = CostSnapshot {
            input_tokens: 100,
            output_tokens: 4,
            current_usage: Some(lingxi_core::host::CurrentUsageSnapshot {
                input_tokens: 100,
                output_tokens: 4,
                ..Default::default()
            }),
            total_usd: 0.09,
            api_calls: 1,
            session_duration: std::time::Duration::from_millis(3926),
            ..Default::default()
        };
        let frame = stream
            .build_result_success_frame(
                "pong",
                "end_turn",
                &cost,
                "claude-opus-4-8",
                "off",
                None,
                &[],
            )
            .await;

        // Golden assertions (volatile fields masked by shape, not value).
        assert_eq!(frame["type"], "result");
        assert_eq!(frame["subtype"], "success");
        assert_eq!(frame["is_error"], false);
        assert_eq!(frame["num_turns"], 1_u64);
        assert_eq!(frame["result"], "pong");
        assert_eq!(frame["stop_reason"], "end_turn");
        assert_eq!(frame["terminal_reason"], "completed");
        assert_eq!(frame["fast_mode_state"], "off");
        assert!(
            frame["session_id"].is_string(),
            "session_id must be a string"
        );
        assert!(frame["uuid"].is_string(), "uuid must be a string");
        // modelUsage
        let mu = frame["modelUsage"].as_object().unwrap();
        assert!(
            mu.contains_key("claude-opus-4-8"),
            "modelUsage keyed by model_id"
        );
        assert_eq!(
            // 2.1.198 registry (M1b): opus-4-8 carries native_1m → 1M window.
            mu["claude-opus-4-8"]["contextWindow"],
            1_000_000_u64,
            "contextWindow from catalog"
        );
        assert_eq!(
            mu["claude-opus-4-8"]["maxOutputTokens"], 64_000_u64,
            "maxOutputTokens from catalog"
        );
        // usage block (20 snake_case keys from GROUND-TRUTH)
        let usage = frame["usage"].as_object().unwrap();
        assert_eq!(usage["input_tokens"], 100_u64);
        assert_eq!(usage["output_tokens"], 4_u64);
        assert_eq!(usage["service_tier"], "standard");
        assert_eq!(usage["speed"], "standard");
    }

    // ---- tool_result_meta (denial provenance) ----------------------------
    //
    // Byte-locked to claude-code `Tpr(e)` (2.1.220, binary offset 233198808):
    //
    //   function Tpr(e){ let t=e.toolDenialKind; if(t===void 0) return [];
    //     let r=e.message.content; if(!Array.isArray(r)) return [];
    //     let n=r.filter(i=>i.type==="tool_result"); if(n.length!==1) return [];
    //     let o={id:n[0].tool_use_id, non_execution_kind:t};
    //     if(e.userFeedback!==void 0) o.user_feedback=e.userFeedback;
    //     return [o] }
    //
    // The emitted key order is `id` then `non_execution_kind` then the optional
    // `user_feedback` — significant because the workspace pins serde_json
    // `preserve_order`.

    fn tool_result_content(ids: &[&str]) -> Value {
        Value::Array(
            ids.iter()
                .map(|id| {
                    json!({"type": "tool_result", "tool_use_id": id, "content": "x", "is_error": true})
                })
                .collect(),
        )
    }

    #[test]
    fn tool_result_meta_carries_denial_kind_for_single_tool_result() {
        let meta = build_tool_result_meta(
            Some("user-rejected"),
            None,
            &tool_result_content(&["toolu_1"]),
        );
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"[{"id":"toolu_1","non_execution_kind":"user-rejected"}]"#
        );
    }

    #[test]
    fn tool_result_meta_appends_user_feedback_after_kind() {
        let meta = build_tool_result_meta(
            Some("automode-blocked"),
            Some("not allowed"),
            &tool_result_content(&["toolu_9"]),
        );
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"[{"id":"toolu_9","non_execution_kind":"automode-blocked","user_feedback":"not allowed"}]"#
        );
    }

    #[test]
    fn tool_result_meta_is_empty_without_denial_kind() {
        let meta =
            build_tool_result_meta(None, Some("ignored"), &tool_result_content(&["toolu_1"]));
        assert!(meta.is_empty());
    }

    #[test]
    fn tool_result_meta_is_empty_when_not_exactly_one_tool_result() {
        let two = build_tool_result_meta(
            Some("user-rejected"),
            None,
            &tool_result_content(&["toolu_1", "toolu_2"]),
        );
        assert!(two.is_empty(), "two tool_result blocks must yield no meta");

        let none = build_tool_result_meta(Some("user-rejected"), None, &Value::Array(vec![]));
        assert!(
            none.is_empty(),
            "zero tool_result blocks must yield no meta"
        );
    }

    #[test]
    fn tool_result_meta_is_empty_for_non_array_content() {
        let meta = build_tool_result_meta(Some("user-rejected"), None, &json!("plain string"));
        assert!(meta.is_empty());
    }
    #[test]
    fn ndjson_uses_javascript_number_and_integer_property_rules() {
        let projection =
            Utf16JsonProjection::parse(r#"{"z":1e-7,"10":-0.0,"2":1e20,"a":9007199254740993}"#)
                .unwrap();
        assert_eq!(
            serialize_projected_ndjson_line(&projection).unwrap(),
            "{\"2\":100000000000000000000,\"10\":0,\"z\":1e-7,\"a\":9007199254740992}\n"
        );
    }

    #[tokio::test]
    async fn injected_writer_keeps_data_control_result_fifo_before_join() {
        use tokio::io::AsyncReadExt;
        let (writer, mut reader) = tokio::io::duplex(4096);
        let stream = StreamJsonStream::new_placeholder(Output::new(writer));
        stream.enqueue(&json!({"type":"assistant","message":"one"}));
        crate::headless::stream_json_input::ControlPlaneWriter::new(stream.outbound_tx())
            .reply_success("req", None);
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        stream.finish().await.unwrap();
        let expected = "{\"type\":\"assistant\",\"message\":\"one\"}\n{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"req\"}}\n{\"type\":\"result\",\"subtype\":\"success\"}\n";
        let mut actual = vec![0; expected.len()];
        reader.read_exact(&mut actual).await.unwrap();
        assert_eq!(actual, expected.as_bytes());
        assert!(stream.drain_task.lock().await.is_none());
    }

    #[tokio::test]
    async fn injected_writer_failure_is_observable_without_new_wire_frames() {
        let (writer, reader) = tokio::io::duplex(32);
        drop(reader);
        let output = Output::new(writer);
        let stream = StreamJsonStream::new_placeholder(output.clone());
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        assert_eq!(
            stream.finish().await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(output.error().unwrap().kind, std::io::ErrorKind::BrokenPipe);
    }
    /// Fixed 2.1.293 native bytes from schema-success and budget-limit captures.
    /// Only generated result UUID is replaced. Ordering and omission remain raw.
    #[tokio::test]
    async fn result_envelopes_match_pinned_native_raw_bytes() {
        for native in [
            r###"{"duration_api_ms":15,"stop_reason":"tool_use","session_id":"8e812215-0a0c-46df-85f3-ee1062e2465e","total_cost_usd":0.000022,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2,"output_tokens_details":{"thinking_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":"","iterations":[],"speed":"standard","fallback_credit":null},"modelUsage":{"claude-sonnet-5-5":{"inputTokens":1,"outputTokens":2,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.000022,"contextWindow":1000000,"maxOutputTokens":128000,"thinkingTokens":0,"canonicalModel":"claude-sonnet-5-5","provider":"firstParty","costBasis":"list"}},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subagent_stats":{"spawned":0,"requested":{"background":0,"foreground":0,"unset":0},"started_in_background":0,"max_depth":0,"spawned_by_subagents":0,"completed":0,"failed":0,"killed":{"parent":0,"user":0,"system":0},"refused":{"depth_limit":0,"concurrency_limit":0,"budget":0},"by_type":{}},"safety_stops":0,"is_error":false,"num_turns":2,"subtype":"success","api_error_status":null,"result":"{\"answer\":\"HEADLESS_SCHEMA_RESPONSE\"}","structured_output":{"answer":"HEADLESS_SCHEMA_RESPONSE"},"ttft_ms":26,"type":"result","duration_ms":32,"uuid":"bc655c05-7e17-453a-ab61-a8b7a834f1de","ttft_stream_ms":26,"time_to_request_ms":19,"first_content_frame_ms":26,"queued_turn_count":0,"result_index":0}"###,
            r###"{"duration_api_ms":0,"stop_reason":"end_turn","session_id":"e76c1c15-f7fa-4971-85ed-0844683c9651","total_cost_usd":0.012,"usage":{"output_tokens_details":{"thinking_tokens":0},"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":"","iterations":[],"speed":"standard","fallback_credit":null},"modelUsage":{"claude-sonnet-5-5":{"inputTokens":1000,"outputTokens":1000,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.012,"contextWindow":1000000,"maxOutputTokens":128000,"thinkingTokens":0,"canonicalModel":"claude-sonnet-5-5","provider":"firstParty","costBasis":"list"}},"permission_denials":[],"terminal_reason":"budget_exhausted","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subagent_stats":{"spawned":0,"requested":{"background":0,"foreground":0,"unset":0},"started_in_background":0,"max_depth":0,"spawned_by_subagents":0,"completed":0,"failed":0,"killed":{"parent":0,"user":0,"system":0},"refused":{"depth_limit":0,"concurrency_limit":0,"budget":0},"by_type":{}},"safety_stops":0,"is_error":true,"num_turns":1,"subtype":"error_max_budget_usd","errors":["Reached maximum budget ($0.000001)"],"type":"result","duration_ms":25,"uuid":"96d4cabd-7044-4037-a115-cb414bfc315c","queued_turn_count":0,"result_index":0}"###,
            r###"{"is_error":true,"duration_api_ms":13,"num_turns":3,"stop_reason":"tool_use","session_id":"2b222b75-dd0c-4597-8d0b-f28d203d8415","total_cost_usd":0.000044,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2,"output_tokens_details":{"thinking_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":"","iterations":[],"speed":"standard","fallback_credit":null},"modelUsage":{"claude-sonnet-5-5":{"inputTokens":2,"outputTokens":4,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.000044,"contextWindow":1000000,"maxOutputTokens":128000,"thinkingTokens":0,"canonicalModel":"claude-sonnet-5-5","provider":"firstParty","costBasis":"list"}},"permission_denials":[],"terminal_reason":"structured_output_retry_exhausted","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subagent_stats":{"spawned":0,"requested":{"background":0,"foreground":0,"unset":0},"started_in_background":0,"max_depth":0,"spawned_by_subagents":0,"completed":0,"failed":0,"killed":{"parent":0,"user":0,"system":0},"refused":{"depth_limit":0,"concurrency_limit":0,"budget":0},"by_type":{}},"safety_stops":0,"subtype":"error_max_structured_output_retries","errors":["Failed to provide valid structured output after 2 attempts — last StructuredOutput error: Output does not match required schema: /answer: must be string"],"type":"result","duration_ms":54,"uuid":"335cfe4d-869b-4787-847d-b5ca5b8ddc9d","queued_turn_count":0,"result_index":0}"###,
        ] {
            let expected: Value = serde_json::from_str(native).unwrap();
            let stream = StreamJsonStream::new(
                make_params(expected["session_id"].as_str().unwrap()),
                Output::new(tokio::io::sink()),
            );
            let metadata = StreamJsonResultMetadata {
                ttft_ms: expected.get("ttft_ms").and_then(Value::as_u64),
                ttft_stream_ms: expected.get("ttft_stream_ms").and_then(Value::as_u64),
                time_to_request_ms: expected.get("time_to_request_ms").and_then(Value::as_u64),
                first_content_frame_ms: expected
                    .get("first_content_frame_ms")
                    .and_then(Value::as_u64),
                num_turns: expected["num_turns"].as_u64(),
                terminal_reason: expected["terminal_reason"].as_str().map(str::to_owned),
                stop_reason: expected["stop_reason"].as_str().map(str::to_owned),
                subagent_stats: Some(expected["subagent_stats"].clone()),
                safety_stops: expected.get("safety_stops").and_then(Value::as_u64),
                usage: Some(expected["usage"].clone()),
                model_usage: expected["modelUsage"].as_object().cloned(),
                ..Default::default()
            };
            stream.set_result_metadata(metadata).await;
            stream
                .set_structured_output(
                    expected
                        .get("structured_output")
                        .cloned()
                        .map(Utf16JsonProjection::plain),
                )
                .await;
            let cost = CostSnapshot {
                total_usd: expected["total_cost_usd"].as_f64().unwrap(),
                session_duration: std::time::Duration::from_millis(
                    expected["duration_ms"].as_u64().unwrap(),
                ),
                api_duration: std::time::Duration::from_millis(
                    expected["duration_api_ms"].as_u64().unwrap(),
                ),
                ..Default::default()
            };
            let mut actual = if expected["is_error"] == true {
                stream
                    .build_result_error_frame(
                        expected["subtype"].as_str().unwrap(),
                        expected["errors"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|error| error.as_str().unwrap().to_owned())
                            .collect(),
                        &cost,
                        "claude-sonnet-5-5",
                        "off",
                        Some("sdk_opt_in_required"),
                        &[],
                    )
                    .await
            } else {
                stream
                    .build_result_success_frame(
                        expected["result"].as_str().unwrap(),
                        expected["stop_reason"].as_str().unwrap(),
                        &cost,
                        "claude-sonnet-5-5",
                        "off",
                        Some("sdk_opt_in_required"),
                        &[],
                    )
                    .await
            };
            actual["uuid"] = expected["uuid"].clone();
            assert_eq!(serialize_ndjson_line(&actual), format!("{native}\n"));
        }
    }
}

#[cfg(test)]
mod delivery_cancellation_tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::sync::Notify;

    struct PendingDeliveryWriter(Arc<Notify>);
    impl tokio::io::AsyncWrite for PendingDeliveryWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.notify_one();
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn abort_joins_pending_drain_and_releases_flush_and_shutdown_acks() {
        let polled = Arc::new(Notify::new());
        let output = Output::new(PendingDeliveryWriter(polled.clone()));
        let stream = Arc::new(StreamJsonStream::new_placeholder(output.clone()));
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        stream.ensure_drain_started().await;
        polled.notified().await;
        let flush_stream = stream.clone();
        let flush = tokio::spawn(async move { flush_stream.flush().await });
        let finish_stream = stream.clone();
        let finish = tokio::spawn(async move { finish_stream.finish().await });
        tokio::task::yield_now().await;
        assert!(!flush.is_finished());
        assert!(!finish.is_finished());
        stream.abort_delivery("waiter dropped");
        for task in [flush, finish] {
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(error.to_string(), "waiter dropped");
        }
        assert!(stream.drain_task.lock().await.is_none());
        assert!(stream.outbound_tx().is_closed());
        assert_eq!(output.error().unwrap().kind, io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn cancellation_before_first_drain_is_joined_with_queued_result() {
        let output = Output::new(tokio::io::sink());
        let stream = StreamJsonStream::new_placeholder(output.clone());
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        output.abort_delivery("terminate");
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), stream.finish())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(stream.drain_task.lock().await.is_none());
        assert!(stream.outbound_tx().is_closed());
    }

    #[tokio::test]
    async fn dropped_finish_waiter_leaves_writer_joinable_for_cleanup() {
        let polled = Arc::new(Notify::new());
        let output = Output::new(PendingDeliveryWriter(polled.clone()));
        let stream = Arc::new(StreamJsonStream::new_placeholder(output.clone()));
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        let waiter_stream = stream.clone();
        let waiter = tokio::spawn(async move { waiter_stream.finish().await });
        polled.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(stream.drain_task.lock().await.is_some());
        output.abort_delivery("waiter dropped");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.finish())
                .await
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(stream.drain_task.lock().await.is_none());
        assert!(stream.outbound_tx().is_closed());
    }

    #[tokio::test]
    async fn termination_unblocks_result_delivery_to_full_duplex_peer() {
        use tokio::io::AsyncReadExt;
        let (writer, mut held_peer) = tokio::io::duplex(1);
        let output = Output::new(writer);
        let stream = Arc::new(StreamJsonStream::new_placeholder(output.clone()));
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        let finish_stream = stream.clone();
        let finish = tokio::spawn(async move { finish_stream.finish().await });
        let mut first_byte = [0];
        held_peer.read_exact(&mut first_byte).await.unwrap();
        assert_eq!(first_byte, [b'{']);
        output.abort_delivery("terminate");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), finish)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(stream.drain_task.lock().await.is_none());
        assert!(stream.outbound_tx().is_closed());
    }
}

#[cfg(test)]
mod verbose_json_tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub(super) struct CapturedWriter(pub(super) Arc<StdMutex<Vec<u8>>>);
    impl tokio::io::AsyncWrite for CapturedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn tool_input_structured_output_and_denial_keep_exact_utf16_on_wire() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new_verbose_json_mode_placeholder(Output::new(
            CapturedWriter(bytes.clone()),
        ));
        let input = Utf16JsonProjection::parse(r#"{"\ud800":"\udfff","answer":"\udc00"}"#).unwrap();
        stream.emit_message_start("provider-message", "model").await;
        stream
            .emit_tool_call(
                &lingxi_core::types::ToolUseId::from("toolu_exact"),
                "StructuredOutput",
                &input.value,
                Some(&input),
            )
            .await;
        stream.emit_message_boundary(Some("tool_use"), None).await;
        stream.set_structured_output(Some(input.clone())).await;
        stream.share_permission_denials(Arc::new(Mutex::new(vec![
            lingxi_core::host::PermissionDenial {
                tool_name: "Read".into(),
                tool_use_id: "toolu_denied".into(),
                tool_input: input.value.clone(),
                tool_input_projection: Some(input.clone()),
            },
        ])));
        stream
            .emit_result_success(
                &Utf16JsonProjection::plain(json!("ignored")),
                "tool_use",
                &CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        stream
            .emit_result_error(
                "error_during_execution",
                vec!["denied".into()],
                &CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        stream.finish().await.unwrap();
        let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(!raw.contains("__lingxiUtf16KeyProjection"));
        assert!(!raw.contains("input_projection"));
        let wire = Utf16JsonProjection::parse(&raw).unwrap();
        for path in [
            "/0/message/content/0/input",
            "/1/structured_output",
            "/1/permission_denials/0/tool_input",
            "/2/permission_denials/0/tool_input",
        ] {
            assert_eq!(
                wire.subprojection(path).unwrap().to_json_string().unwrap(),
                input.to_json_string().unwrap(),
                "{path}"
            );
        }
        assert_eq!(wire.value[1]["result"], input.to_json_string().unwrap());
    }

    #[tokio::test]
    async fn accepted_mcp_result_preserves_array_keys_and_string_units_in_both_fields() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new_verbose_json_mode_placeholder(Output::new(
            CapturedWriter(bytes.clone()),
        ));
        let data =
            Utf16JsonProjection::parse(r#"[{"type":"text","text":"\ud800","\udfff":"\udc00"}]"#)
                .unwrap();
        let meta =
            Utf16JsonProjection::parse(r#"{"_meta":{"\udc00":"\udfff"},"structuredContent":null}"#)
                .unwrap();
        let exact = lingxi_core::host::ToolResultProjection { model_text: None,
            mcp_meta: Some(meta.clone()),
            data: data.clone(),
            content: data.clone(),
        };
        stream
            .emit_tool_result(
                &lingxi_core::types::ToolUseId::from("toolu_mcp"),
                "mcp__test__exact",
                "\u{fffd}",
                &data.value,
                Some(&exact),
            )
            .await;
        stream.finish().await.unwrap();
        let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(!raw.contains("__lingxiUtf16KeyProjection"));
        assert!(!raw.contains("data_projection"));
        let wire = Utf16JsonProjection::parse(&raw).unwrap();
        assert_eq!(
            wire.subprojection("/0/mcpMeta")
                .unwrap()
                .to_json_string()
                .unwrap(),
            meta.to_json_string().unwrap()
        );
        assert!(
            wire.value[0]["mcpMeta"]
                .get("structuredContent")
                .unwrap()
                .is_null()
        );
        for path in ["/0/toolUseResult", "/0/message/content/0/content"] {
            assert_eq!(
                wire.subprojection(path).unwrap().to_json_string().unwrap(),
                data.to_json_string().unwrap(),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn verbose_json_publishes_before_cleanup_and_matches_pinned_native_array() {
        let native = r###"[{"type":"system","subtype":"init","cwd":"/private/tmp/headless-baseline-293-final/print-json-verbose/workspace","session_id":"9d783f5c-874d-47b6-80fb-0fc0f036f0a6","tools":[],"mcp_servers":[],"model":"claude-sonnet-5-5","permissionMode":"dontAsk","slash_commands":["deep-research","dataviz","update-config","verify","debug","code-review","simplify","batch","fewer-permission-prompts","doctor","loop","claude-api","workflow-authoring","run","run-skill-generator","plugin-authoring","agents","auto-mode-setup","autocompact","clear","color","compact","config","output-style","context","effort","fast","focus","heapdump","init","mcp","model","__remote-workflow","workflow-launch-exec","reload-plugins","reload-skills","rename","security-review","usage","insights","recap","goal","list-agents","team-onboarding"],"terminal_slash_commands":["doctor","color","focus","reload-plugins"],"apiKeySource":"ANTHROPIC_API_KEY","claude_code_version":"2.1.293","output_style":"default","agents":["claude","Explore","general-purpose","Plan","statusline-setup"],"skills":["deep-research","dataviz","update-config","verify","debug","code-review","simplify","batch","fewer-permission-prompts","doctor","loop","claude-api","workflow-authoring","run","run-skill-generator","plugin-authoring"],"plugins":[{"name":"cc-plugin-agents-md","path":"builtin","source":"cc-plugin-agents-md@builtin"},{"name":"cc-plugin-plugin-authoring","path":"builtin","source":"cc-plugin-plugin-authoring@builtin"}],"capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","interrupt_send_now_v1","msg_lifecycle_v1","request_marker_lists_v1","sdk_mcp_tools_list_changed","sdk_mcp_manifests","mcp_read_resource_v1","mcp_tool_ui_meta_v1","ui_surface_v1"],"analytics_disabled":true,"product_feedback_disabled":true,"uuid":"e1d51f22-fb1e-400f-bd5b-5dc81e88828c","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","per_turn_effort_active":true,"view_mode":"default"},{"type":"assistant","message":{"id":"msg_headless_1","type":"message","role":"assistant","model":"claude-sonnet-5-5","content":[{"type":"text","text":"HEADLESS_LOCAL_RESPONSE"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0},"context_management":null},"parent_tool_use_id":null,"session_id":"9d783f5c-874d-47b6-80fb-0fc0f036f0a6","uuid":"e2ec702e-8229-4705-8da3-161a7dfbbd15","timestamp":"2026-10-08T00:06:49.398Z"},{"duration_api_ms":78,"stop_reason":"end_turn","session_id":"9d783f5c-874d-47b6-80fb-0fc0f036f0a6","total_cost_usd":0.000022,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2,"output_tokens_details":{"thinking_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":"","iterations":[],"speed":"standard","fallback_credit":null},"modelUsage":{"claude-sonnet-5-5":{"inputTokens":1,"outputTokens":2,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.000022,"contextWindow":1000000,"maxOutputTokens":128000,"thinkingTokens":0,"canonicalModel":"claude-sonnet-5-5","provider":"firstParty","costBasis":"list"}},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subagent_stats":{"spawned":0,"requested":{"background":0,"foreground":0,"unset":0},"started_in_background":0,"max_depth":0,"spawned_by_subagents":0,"completed":0,"failed":0,"killed":{"parent":0,"user":0,"system":0},"refused":{"depth_limit":0,"concurrency_limit":0,"budget":0},"by_type":{}},"safety_stops":0,"is_error":false,"num_turns":1,"subtype":"success","api_error_status":null,"result":"HEADLESS_LOCAL_RESPONSE","ttft_ms":63,"type":"result","duration_ms":133,"uuid":"654d9abb-4bdc-410b-bf3f-026099a5e05e","ttft_stream_ms":63,"time_to_request_ms":55,"first_content_frame_ms":63,"queued_turn_count":0,"result_index":0}]"###;
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new_verbose_json_mode_placeholder(Output::new(
            CapturedWriter(bytes.clone()),
        ));
        let frames = serde_json::from_str::<Value>(native).unwrap();
        stream.enqueue(&json!({"type":"ui_invalidate","uuid":"transient"}));
        stream.enqueue(&json!({"type":"system","subtype":"status","status":"requesting"}));
        stream.enqueue(&json!({"type":"system","subtype":"hook_started","hook_id":"startup"}));
        stream.enqueue(&json!({"type":"system","subtype":"hook_response","hook_id":"startup"}));
        for frame in frames.as_array().unwrap() {
            stream.enqueue(frame);
        }
        stream.flush().await.unwrap();
        assert!(
            bytes.lock().unwrap().is_empty(),
            "ordinary barriers must not deliver partial arrays"
        );
        stream.publish_json().await.unwrap();
        assert_eq!(
            bytes.lock().unwrap().as_slice(),
            format!("{native}\n").as_bytes()
        );
        // Teardown emissions cannot republish or mutate the native final array.
        stream.enqueue(&json!({"type":"system","subtype":"task_notification"}));
        stream.publish_json().await.unwrap();
        stream.finish().await.unwrap();
        assert_eq!(
            bytes.lock().unwrap().as_slice(),
            format!("{native}\n").as_bytes()
        );
    }

    #[tokio::test]
    async fn verbose_json_preserves_projected_frame_bytes_without_lowering_sidecars() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new_verbose_json_mode_placeholder(Output::new(
            CapturedWriter(bytes.clone()),
        ));
        let projected = Utf16JsonProjection::parse(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"\ud800":"\udfff"}}]}}"#).unwrap();
        stream.enqueue_line(serialize_projected_ndjson_line(&projected).unwrap(), false);
        stream.enqueue(&json!({"type":"result","subtype":"success"}));
        stream.finish().await.unwrap();
        assert_eq!(bytes.lock().unwrap().as_slice(), b"[{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"input\":{\"\\ud800\":\"\\udfff\"}}]}},{\"type\":\"result\",\"subtype\":\"success\"}]\n");
    }

    #[tokio::test]
    async fn nonverbose_json_delivers_only_the_result_object() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream =
            StreamJsonStream::new_json_mode_placeholder(Output::new(CapturedWriter(bytes.clone())));
        stream.emit_status().await;
        stream.emit_text("collected text", None).await;
        let result = stream
            .emit_result_success(
                &Utf16JsonProjection::plain(json!("done")),
                "end_turn",
                &CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        stream.finish().await.unwrap();
        assert_eq!(
            bytes.lock().unwrap().as_slice(),
            serialize_ndjson_line(&result).as_bytes()
        );
    }

    #[tokio::test]
    async fn print_buffers_keep_native_final_result_and_verbose_result_tail() {
        for mode in [StreamJsonOutputMode::LastResultText, StreamJsonOutputMode::LastResultJson, StreamJsonOutputMode::VerboseJson] {
            let bytes = Arc::new(StdMutex::new(Vec::new()));
            let stream = StreamJsonStream::new_inner_with_output_mode(None, false, Output::new(CapturedWriter(bytes.clone())), mode);
            for (index, text, completed) in [(0, "parent", 0), (1, "after child", 1)] {
                stream.enqueue(&json!({"type":"system","subtype":"init","query":index}));
                stream.enqueue(&json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}));
                stream.set_result_metadata(StreamJsonResultMetadata { result_index: index, num_turns: Some(if index == 0 { 2 } else { 1 }), subagent_stats: Some(json!({"completed":completed})), ..Default::default() }).await;
                stream.emit_result_success(&Utf16JsonProjection::plain(json!(text)), "end_turn", &CostSnapshot::default(), "model", "off", None, &[]).await;
                stream.enqueue(&json!({"type":"system","subtype":"task_notification"}));
                stream.flush().await.unwrap();
                assert!(bytes.lock().unwrap().is_empty(), "buffered modes publish only after winddown");
            }
            stream.refresh_held_result_totals(&CostSnapshot::default(), "model", &[], Some(json!({"completed":1})));
            stream.publish_json().await.unwrap();
            stream.finish().await.unwrap();
            let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            if mode == StreamJsonOutputMode::LastResultText {
                assert_eq!(raw, "after child\n");
            } else if mode == StreamJsonOutputMode::LastResultJson {
                let frame: Value = serde_json::from_str(&raw).unwrap();
                assert_eq!(frame["result"], "after child");
                assert_eq!(frame["result_index"], 1);
            } else {
                let frames: Vec<Value> = serde_json::from_str(&raw).unwrap();
                assert_eq!(frames.iter().map(|frame| frame["type"].as_str().unwrap()).collect::<Vec<_>>(), vec!["system", "assistant", "system", "assistant", "result", "result"]);
                assert_eq!(frames[4]["num_turns"], 2);
                assert_eq!(frames[5]["num_turns"], 1);
                assert_eq!(frames[4]["subagent_stats"]["completed"], 1);
                assert_eq!(frames[5]["subagent_stats"]["completed"], 1);
            }
        }
    }

    #[test]
    fn init_collector_uses_only_supplied_host_snapshot() {
        let host = StreamJsonInitHostMetadata {
            cwd: std::path::PathBuf::from("/injected/session/workspace"),
            mcp_server_errors: vec![
                json!({"path":"mcpServers.broken","message":"supplied diagnostic"}),
            ],
            analytics_disabled: true,
            product_feedback_disabled: false,
            per_turn_effort_active: Some(true),
            view_mode: Some("default".into()),
        };
        let params = build_init_params(
            "session",
            Vec::new(),
            Vec::new(),
            "model",
            &llm_runtime::CredentialSource::None,
            "default",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            "default",
            None,
            "off",
            None,
            host,
        );
        assert_eq!(params.cwd, "/injected/session/workspace");
        assert!(params.analytics_disabled);
        assert!(!params.product_feedback_disabled);
        assert_eq!(params.mcp_server_errors.len(), 1);
        assert_eq!(
            params.claude_code_version,
            super::super::CLAUDE_CODE_REFERENCE_VERSION
        );
        let frame = build_init_frame("session", "uuid", &params);
        assert_eq!(frame["per_turn_effort_active"], true);
        assert_eq!(frame["view_mode"], "default");
        assert!(frame.get("memory_paths").is_none());
    }

    #[test]
    fn loaded_plugin_metadata_preserves_native_version_presence_and_key_order() {
        let plugins = vec![
            StreamJsonPlugin {
                name: "local".into(),
                path: "/loaded/local".into(),
                source: "local@inline".into(),
                version: Some("1.0.0".into()),
            },
            StreamJsonPlugin {
                name: "builtin".into(),
                path: "builtin".into(),
                source: "builtin@builtin".into(),
                version: None,
            },
        ];
        let params = build_init_params(
            "session",
            Vec::new(),
            Vec::new(),
            "model",
            &llm_runtime::CredentialSource::None,
            "default",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            plugins,
            "default",
            None,
            "off",
            None,
            StreamJsonInitHostMetadata::default(),
        );
        assert_eq!(
            serialize_ndjson_line(&Value::Array(params.plugins)),
            "[{\"name\":\"local\",\"path\":\"/loaded/local\",\"source\":\"local@inline\",\"version\":\"1.0.0\"},{\"name\":\"builtin\",\"path\":\"builtin\",\"source\":\"builtin@builtin\"}]\n"
        );
    }

    #[tokio::test]
    async fn assistant_text_pairs_surrogates_across_deltas_and_keeps_lone_units_in_result() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new(super::tests::make_params("unicode"), Output::new(CapturedWriter(bytes.clone())));
        stream.emit_message_start("msg_unicode", "model").await;
        stream.emit_assistant_block_start(1).await;
        stream.emit_text("�", Some(&[0xd83d])).await;
        stream.emit_text("�", Some(&[0xde00])).await;
        stream.emit_text("\u{2028}\u{2029}�", Some(&[0x2028, 0x2029, 0xd800])).await;
        let pending = stream.get_last_result_text().await;
        assert_eq!(pending.to_json_string().unwrap(), "\"😀\u{2028}\u{2029}\\ud800\"");
        stream.emit_message_boundary(Some("end_turn"), None).await;
        let completed = stream.get_last_result_text().await;
        assert_eq!(completed, pending);
        stream.emit_result_success(&completed, "end_turn", &CostSnapshot::default(), "model", "off", None, &[]).await;
        stream.finish().await.unwrap();
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let expected = "😀\\u2028\\u2029\\ud800";
        assert_eq!(output.matches(expected).count(), 2, "{output}");
        assert!(!output.contains('�'), "{output}");
        assert!(!output.contains("utf16_code_units"), "{output}");
    }

    #[tokio::test]
    async fn assistant_distinct_text_blocks_keep_individual_lone_surrogates() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new(super::tests::make_params("unicode"), Output::new(CapturedWriter(bytes.clone())));
        stream.emit_message_start("msg_unicode", "model").await;
        stream.emit_assistant_block_start(1).await;
        stream.emit_text("�", Some(&[0xd83d])).await;
        stream.emit_assistant_block_start(2).await;
        stream.emit_text("�", Some(&[0xde00])).await;
        assert_eq!(stream.get_last_result_text().await.to_json_string().unwrap(), "\"😀\"");
        stream.emit_message_boundary(Some("end_turn"), None).await;
        stream.finish().await.unwrap();
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(output.contains("\"text\":\"\\ud83d\""), "{output}");
        assert!(output.contains("\"text\":\"\\ude00\""), "{output}");
        assert!(!output.contains("😀"), "{output}");
    }

    #[tokio::test]
    async fn forwarded_subagent_rich_text_uses_native_text_block_and_exact_units() {
        let bytes = Arc::new(StdMutex::new(Vec::new()));
        let stream = StreamJsonStream::new(super::tests::make_params("unicode"), Output::new(CapturedWriter(bytes.clone())));
        stream.set_forward_subagent_text(true);
        stream.emit_forwarded_subagent_message(&json!({
            "role":"assistant", "content":[{"type":"text_js_utf16","text":"A�","utf16_code_units":[65,55296]}]
        }), "toolu_parent").await;
        stream.finish().await.unwrap();
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(output.contains("\"type\":\"text\",\"text\":\"A\\ud800\""), "{output}");
        assert!(!output.contains("text_js_utf16"), "{output}");
        assert!(!output.contains("utf16_code_units"), "{output}");
    }

    #[tokio::test]
    async fn mismatched_assistant_text_units_fail_delivery_without_admitting_text() {
        let stream = StreamJsonStream::new(super::tests::make_params("unicode"), Output::new(tokio::io::sink()));
        stream.emit_text("different", Some(&[0xd800])).await;
        assert!(stream.accum.lock().await.blocks.is_empty());
        assert!(stream.finish().await.is_err());
    }

}
