//! The provider-event side of Claude Code's `turn.step` stream boundary.
//!
//! Every event gets an opaque, one-based ref, including the events represented
//! by a readable chunk. The ref belongs to one request only. Keep the original
//! event so a later middleware pass can forward engine events without decoding
//! or reserializing provider metadata.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ContentBlock, ExecutionUsage, HistoryContentDelta, HistoryEvent, HistoryMessageDelta,
    HistoryResponse,
};
use lingxi_core::types::utf16_json::{Utf16JsonProjection, Utf16JsonString};
use serde_json::{Value, json};

/// An original model event and the Mod chunk exposed for that event.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStepEvent {
    pub reference: u64,
    pub event: HistoryEvent,
    pub chunk: Utf16JsonProjection,
}

/// Converts physical response streams into the chunks read by one `turn.step`
/// dispatch. Concurrent `next(e)` calls share the ref table; each source keeps
/// its own ref list for its result summary.
#[derive(Default)]
pub struct TurnStepEncoder {
    events: Vec<TurnStepEvent>,
    streamed_responses: HashMap<u64, HistoryResponse>,
    response_timestamps_ms: HashMap<u64, u64>,
    model: Option<String>,
    blocks: HashMap<u32, BlockKind>,
}

struct NormalizedAssistantBlockRow<'a> {
    response_reference: u64,
    response: &'a HistoryResponse,
    block_index: Option<usize>,
    block: Option<&'a ContentBlock>,
    timestamp_ms: u64,
}

#[derive(Clone)]
struct ServerToolUseEntry {
    id: String,
    name: String,
    input: Value,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .filter(|timestamp| *timestamp != 0)
        .unwrap_or_default()
}

fn server_tool_use_fields(block: &ContentBlock) -> Option<(&str, &str, &Value)> {
    match block {
        ContentBlock::ServerToolUse { id, name, input } => Some((id, name, input)),
        ContentBlock::ProviderContent { value, .. }
            if matches!(
                value.get("type").and_then(Value::as_str),
                Some("server_tool_use" | "mcp_tool_use")
            ) =>
        {
            Some((
                value.get("id")?.as_str()?,
                value.get("name")?.as_str()?,
                value.get("input")?,
            ))
        }
        _ => None,
    }
}

fn server_tool_result_id(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::AdvisorToolResult { tool_use_id, .. }
        | ContentBlock::ToolResult { tool_call_id: tool_use_id, .. } => Some(tool_use_id),
        ContentBlock::ProviderContent { value, .. } => {
            value.get("tool_use_id").and_then(Value::as_str)
        }
        _ => None,
    }
}

fn record_server_tool_use(
    entries: &mut Vec<ServerToolUseEntry>,
    positions: &mut HashMap<String, usize>,
    block: &ContentBlock,
    timestamp_ms: u64,
) {
    if let Some((id, name, input)) = server_tool_use_fields(block) {
        if !positions.contains_key(id) {
            positions.insert(id.to_owned(), entries.len());
            entries.push(ServerToolUseEntry {
                id: id.to_owned(),
                name: name.to_owned(),
                input: input.clone(),
                started_at_ms: timestamp_ms,
                ended_at_ms: None,
            });
        }
        return;
    }

    let Some(position) = server_tool_result_id(block).and_then(|id| positions.get(id).copied())
    else {
        return;
    };
    if let Some(entry) = entries.get_mut(position) {
        if entry.ended_at_ms.is_none() {
            entry.ended_at_ms = Some(timestamp_ms);
        }
    }
}

fn server_tool_uses_value(entries: &[ServerToolUseEntry]) -> Option<Value> {
    (!entries.is_empty()).then(|| {
        Value::Array(
            entries
                .iter()
                .map(|entry| {
                    let mut value = json!({
                        "id":entry.id,
                        "name":entry.name,
                        "input":entry.input,
                        "startedAt":entry.started_at_ms,
                    });
                    if let Some(ended_at_ms) = entry.ended_at_ms {
                        value["endedAt"] = json!(ended_at_ms);
                    }
                    value
                })
                .collect(),
        )
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Tool,
    Other,
}

fn stop_reason(reason: Option<&str>) -> Option<&str> {
    reason.filter(|reason| {
        matches!(
            *reason,
            "end_turn"
                | "max_tokens"
                | "stop_sequence"
                | "tool_use"
                | "pause_turn"
                | "compaction"
                | "refusal"
                | "model_context_window_exceeded"
        )
    })
}

fn usage_value(usage: &ExecutionUsage, model: &str) -> Option<Value> {
    let counts = usage.report.usage?;
    Some(json!({
        "input_tokens":counts.input_tokens,
        "output_tokens":counts.output_tokens,
        "cache_read_input_tokens":counts.cache_read_tokens,
        "cache_creation_input_tokens":counts.cache_write_tokens,
        "model":model,
    }))
}

// ECMAScript String.prototype.trim() includes BOM (U+FEFF) and excludes NEL
// (U+0085). Rust str::trim() does the opposite for those two code points.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_trim_char)
}

fn is_js_trim_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn utf16_scalar_at(units: &[u16], index: usize) -> Option<(char, usize)> {
    let first = *units.get(index)?;
    if (0xd800..=0xdbff).contains(&first) {
        if let Some(second) = units
            .get(index + 1)
            .copied()
            .filter(|unit| (0xdc00..=0xdfff).contains(unit))
        {
            let high = u32::from(first) - 0xd800;
            let low = u32::from(second) - 0xdc00;
            let scalar = 0x1_0000 + (high << 10) + low;
            return char::from_u32(scalar).map(|value| (value, 2));
        }
        return Some(('\u{FFFD}', 1));
    }
    if (0xdc00..=0xdfff).contains(&first) {
        return Some(('\u{FFFD}', 1));
    }
    char::from_u32(u32::from(first)).map(|value| (value, 1))
}

fn utf16_scalar_before(units: &[u16], end: usize) -> Option<(char, usize)> {
    let last = *units.get(end.checked_sub(1)?)?;
    if (0xdc00..=0xdfff).contains(&last) {
        if let Some(first) = end
            .checked_sub(2)
            .and_then(|index| units.get(index))
            .copied()
        {
            if (0xd800..=0xdbff).contains(&first) {
                let high = u32::from(first) - 0xd800;
                let low = u32::from(last) - 0xdc00;
                let scalar = 0x1_0000 + (high << 10) + low;
                return char::from_u32(scalar).map(|value| (value, 2));
            }
        }
        return Some(('\u{FFFD}', 1));
    }
    if (0xd800..=0xdbff).contains(&last) {
        return Some(('\u{FFFD}', 1));
    }
    char::from_u32(u32::from(last)).map(|value| (value, 1))
}

fn trim_utf16_units(units: &[u16]) -> Vec<u16> {
    let mut start = 0;
    let mut end = units.len();
    while start < end {
        let Some((scalar, width)) = utf16_scalar_at(units, start) else {
            break;
        };
        if !is_js_trim_char(scalar) {
            break;
        }
        start += width;
    }
    while end > start {
        let Some((scalar, width)) = utf16_scalar_before(units, end) else {
            break;
        };
        if !is_js_trim_char(scalar) {
            break;
        }
        end -= width;
    }
    units[start..end].to_vec()
}

impl TurnStepEncoder {
    /// Record an event and return its Mod-visible chunk. Refs start at one,
    /// exactly as the 2.1.288 `Yn().hold()` table does.
    pub fn push(&mut self, event: HistoryEvent) -> &TurnStepEvent {
        let reference = self.events.len() as u64 + 1;
        let response_timestamp_ms =
            matches!(&event, HistoryEvent::Completed { .. }).then(now_millis);
        let chunk = match &event {
            HistoryEvent::MessageStart { response } => {
                self.model = Some(response.model.clone());
                self.blocks.clear();
                json!({"kind":"engine","ref":reference})
            }
            HistoryEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::ToolCall { id, name, .. },
            } => {
                self.blocks.insert(*index, BlockKind::Tool);
                json!({"kind":"tool","index":index,"id":id,"name":name,"ref":reference})
            }
            HistoryEvent::ContentBlockStart { index, .. } => {
                self.blocks.insert(*index, BlockKind::Other);
                json!({"kind":"engine","ref":reference})
            }
            HistoryEvent::ContentBlockDelta {
                index,
                delta: HistoryContentDelta::TextDelta { text },
            } => json!({"kind":"text","index":index,"text":text,"ref":reference}),
            HistoryEvent::ContentBlockDelta {
                index,
                delta: HistoryContentDelta::TextJsUtf16Delta { text, .. },
            } => json!({"kind":"text","index":index,"text":text,"ref":reference}),
            HistoryEvent::ContentBlockDelta {
                index,
                delta: HistoryContentDelta::ThinkingDelta { thinking },
            } => json!({"kind":"thinking","index":index,"text":thinking,"ref":reference}),
            HistoryEvent::ContentBlockDelta {
                index,
                delta: HistoryContentDelta::InputJsonDelta { partial_json },
            } if self.blocks.get(index) == Some(&BlockKind::Tool) => {
                json!({"kind":"input","index":index,"json":partial_json,"ref":reference})
            }
            HistoryEvent::MessageDelta { delta, usage } if delta.stop_reason.is_some() => {
                let reason = stop_reason(delta.stop_reason.as_deref());
                let reported = usage
                    .as_ref()
                    .zip(self.model.as_deref())
                    .and_then(|(usage, model)| usage_value(usage, model));
                json!({"kind":"stop","stopReason":reason,"usage":reported,"ref":reference})
            }
            _ => json!({"kind":"engine","ref":reference}),
        };
        let chunk = if let HistoryEvent::ContentBlockDelta {
            delta:
                HistoryContentDelta::TextJsUtf16Delta {
                    utf16_code_units, ..
                },
            ..
        } = &event
        {
            Utf16JsonProjection {
                value: chunk,
                strings: vec![Utf16JsonString {
                    pointer: "/text".into(),
                    code_units: utf16_code_units.clone(),
                }],
                keys: Vec::new(),
            }
        } else {
            Utf16JsonProjection::plain(chunk)
        };
        self.events.push(TurnStepEvent {
            reference,
            event,
            chunk,
        });
        if let Some(timestamp_ms) = response_timestamp_ms {
            self.response_timestamps_ms.insert(reference, timestamp_ms);
        }
        self.events.last().expect("event was just recorded")
    }

    /// Recover the original event for a chunk ref without a lossy JSON round
    /// trip. The decoder will use this for opaque `engine` pass-through.
    pub fn original(&self, reference: u64) -> Option<&HistoryEvent> {
        reference
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.events.get(index))
            .map(|held| &held.event)
    }

    /// The original Mod chunk for a ref, used when deciding whether a hook
    /// returned a byte-equivalent pass-through or a rewrite.
    pub fn chunk(&self, reference: u64) -> Option<&Utf16JsonProjection> {
        reference
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.events.get(index))
            .map(|held| &held.chunk)
    }

    /// The reference the next physical event will receive.
    pub fn next_reference(&self) -> u64 {
        self.events.len() as u64 + 1
    }

    /// Attach an assembled streamed assistant record to its terminal ref.
    /// Native `Kn` sees the completed assistant record alongside SSE events;
    /// our provider seam exposes only the SSE events, so retain that record
    /// without inventing an extra Mod-visible chunk or ref.
    pub fn note_stream_response(&mut self, terminal_ref: u64, response: HistoryResponse) {
        if matches!(self.original(terminal_ref), Some(HistoryEvent::MessageStop)) {
            self.streamed_responses.insert(terminal_ref, response);
            self.response_timestamps_ms
                .insert(terminal_ref, now_millis());
        }
    }

    /// The bottom stream's result, based on the completed assistant records
    /// rather than on fragments. This mirrors 2.1.288 `Kn`: text from each
    /// completed assistant message is joined with newlines, tool uses retain
    /// their order, and stop/usage come from the last completed message.
    pub fn result(&self, turn_id: &str, index: u32) -> Utf16JsonProjection {
        self.result_since(turn_id, index, 1)
    }

    /// Summarize a contiguous suffix. Use `result_for_references` for a
    /// forwarded source because another source may interleave its refs.
    pub fn result_since(&self, turn_id: &str, index: u32, first_ref: u64) -> Utf16JsonProjection {
        self.result_for_references(
            turn_id,
            index,
            &(first_ref..self.next_reference()).collect::<Vec<_>>(),
        )
    }

    /// Summarize only events that came from one forwarded `next(e)` stream.
    /// Concurrent sources may interleave refs in the shared wire table.
    pub fn result_for_references(
        &self,
        turn_id: &str,
        index: u32,
        references: &[u64],
    ) -> Utf16JsonProjection {
        let mut rows = Vec::<NormalizedAssistantBlockRow<'_>>::new();
        for reference in references {
            let Some(held) = reference
                .checked_sub(1)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| self.events.get(index))
            else {
                continue;
            };
            match &held.event {
                // Native's `streaming_fallback_began` drops every provisional
                // assistant row before the fallback response is admitted.
                // The current stream adapter expresses that boundary through
                // its host-owned response-model observation.
                HistoryEvent::ResponseObserved { .. } => {
                    rows.clear();
                    continue;
                }
                // Native `server_fallback` removes the source rows named by
                // `discardedMessages`. The Rust stream carries their original
                // API content-block ordinals instead of row UUIDs.
                HistoryEvent::ServerFallback { event, .. } => {
                    let affected_response = rows.last().map(|row| row.response_reference);
                    rows.retain(|row| {
                        Some(row.response_reference) != affected_response
                            || row.block_index.is_none_or(|block_index| {
                                !event.discarded_blocks.contains(&block_index)
                            })
                    });
                    continue;
                }
                _ => {}
            }
            let response = match &held.event {
                HistoryEvent::Completed { response } => Some(response.as_ref()),
                HistoryEvent::MessageStop => self.streamed_responses.get(reference),
                _ => None,
            };
            let Some(response) = response else { continue };
            let timestamp_ms = self
                .response_timestamps_ms
                .get(reference)
                .copied()
                .unwrap_or_else(now_millis);
            if response.content.is_empty() {
                rows.push(NormalizedAssistantBlockRow {
                    response_reference: *reference,
                    response,
                    block_index: None,
                    block: None,
                    timestamp_ms,
                });
            } else {
                rows.extend(
                    response
                        .content
                        .iter()
                        .enumerate()
                        .map(|(block_index, block)| NormalizedAssistantBlockRow {
                            response_reference: *reference,
                            response,
                            block_index: Some(block_index),
                            block: Some(block),
                            timestamp_ms,
                        }),
                );
            }
        }

        let mut answer = Vec::new();
        let mut answer_units = Vec::new();
        let mut answer_has_exact_utf16 = false;
        let mut tool_uses = Vec::new();
        let mut server_tool_uses = Vec::new();
        let mut server_tool_positions = HashMap::new();
        for row in &rows {
            let Some(block) = row.block else {
                continue;
            };
            match block {
                ContentBlock::Text { text, .. } => {
                    answer.push(text.as_str());
                    if answer.len() > 1 {
                        answer_units.push(b'\n' as u16);
                    }
                    answer_units.extend(text.encode_utf16());
                }
                ContentBlock::TextJsUtf16 {
                    text,
                    utf16_code_units,
                    ..
                } => {
                    answer.push(text.as_str());
                    if answer.len() > 1 {
                        answer_units.push(b'\n' as u16);
                    }
                    answer_units.extend(utf16_code_units);
                    answer_has_exact_utf16 = true;
                }
                ContentBlock::ToolCall { name, input, .. } => {
                    tool_uses.push(json!({"name":name,"input":input}));
                }
                _ => {}
            }
            record_server_tool_use(
                &mut server_tool_uses,
                &mut server_tool_positions,
                block,
                row.timestamp_ms,
            );
        }
        let last = rows.last().map(|row| &row.response);
        let display_answer = js_trim(&answer.join("\n")).to_owned();
        let exact_answer_units = answer_has_exact_utf16.then(|| trim_utf16_units(&answer_units));
        let answer = exact_answer_units
            .as_ref()
            .map(|units| String::from_utf16_lossy(units))
            .unwrap_or(display_answer);
        let mut result = json!({
            "turnId":turn_id,
            "index":index,
            "answer":answer,
            "toolUses":tool_uses,
            "stopReason":last.and_then(|response| stop_reason(response.stop_reason.as_deref())),
            "usage":last.and_then(|response| usage_value(&response.usage, &response.model)),
        });
        if let Some(server_tool_uses) = server_tool_uses_value(&server_tool_uses) {
            result["serverToolUses"] = server_tool_uses;
        }
        match exact_answer_units {
            Some(code_units) => {
                let projection = Utf16JsonProjection {
                    value: result,
                    strings: vec![Utf16JsonString {
                        pointer: "/answer".into(),
                        code_units,
                    }],
                    keys: Vec::new(),
                };
                debug_assert!(projection.validate().is_ok());
                projection
            }
            None => Utf16JsonProjection::plain(result),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputBlockKind {
    Text,
    Thinking,
    Tool,
    Other,
}

/// Rebuilds host stream events from chunks yielded by the outermost Mod. It
/// keeps original events when a chunk is unchanged, and synthesizes only the
/// changed pieces. Opaque refs never cross through JSON serialization.
pub struct TurnStepDecoder {
    model: String,
    started: bool,
    stopped: bool,
    ended: bool,
    blocks: BTreeMap<u32, OutputBlockKind>,
    thinking_starts: HashMap<u32, u64>,
}

impl TurnStepDecoder {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            started: false,
            stopped: false,
            ended: false,
            blocks: BTreeMap::new(),
            thinking_starts: HashMap::new(),
        }
    }

    fn start(&mut self, out: &mut Vec<HistoryEvent>) {
        if self.started && !self.ended {
            return;
        }
        self.started = true;
        self.stopped = false;
        self.ended = false;
        self.blocks.clear();
        self.thinking_starts.clear();
        out.push(HistoryEvent::MessageStart {
            response: Box::new(HistoryResponse {
                id: format!("msg_hook_{}", uuid::Uuid::new_v4()),
                model: self.model.clone(),
                content: Vec::new(),
                stop_reason: None,
                stop_details: None,
                usage: ExecutionUsage::default(),
                cost: None,
                provider_metadata: Value::Null,
            }),
        });
    }

    fn block(&mut self, index: u32, kind: OutputBlockKind, out: &mut Vec<HistoryEvent>) {
        self.start(out);
        if self.blocks.contains_key(&index) {
            return;
        }
        self.blocks.insert(index, kind);
        let content_block = match kind {
            OutputBlockKind::Text => ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None,
            },
            OutputBlockKind::Thinking => ContentBlock::Reasoning {
                text: String::new(),
                signature: None,
            },
            OutputBlockKind::Tool | OutputBlockKind::Other => return,
        };
        out.push(HistoryEvent::ContentBlockStart {
            index,
            content_block,
        });
    }

    fn replay_thinking(
        &mut self,
        index: u32,
        until: u64,
        wire: &TurnStepEncoder,
        out: &mut Vec<HistoryEvent>,
    ) {
        let start = self.thinking_starts.remove(&index).unwrap_or_default();
        out.extend(wire.events.iter().filter_map(|held| {
            (held.reference > start && held.reference < until)
                .then_some(&held.event)
                .and_then(|event| match event {
                    HistoryEvent::ContentBlockDelta {
                        index: delta_index,
                        delta:
                            HistoryContentDelta::ThinkingDelta { .. }
                            | HistoryContentDelta::SignatureDelta { .. },
                    } if *delta_index == index => Some(event.clone()),
                    _ => None,
                })
        }));
    }

    fn close_blocks(&mut self, out: &mut Vec<HistoryEvent>, wire: &TurnStepEncoder) {
        for (index, kind) in self.blocks.clone() {
            if kind == OutputBlockKind::Thinking {
                self.replay_thinking(index, wire.next_reference(), wire, out);
            }
            out.push(HistoryEvent::ContentBlockStop { index });
            self.blocks.remove(&index);
        }
    }

    /// Convert one yielded chunk to zero or more host events. A missing
    /// `engine` ref is dropped as in the native decoder; the worker's per-link
    /// checker normally rejects it before it reaches this point.
    pub fn consume(
        &mut self,
        chunk: &Utf16JsonProjection,
        wire: &TurnStepEncoder,
    ) -> Vec<HistoryEvent> {
        let value = &chunk.value;
        let mut out = Vec::new();
        let reference = value.get("ref").and_then(Value::as_u64);
        if value.get("kind").and_then(Value::as_str) == Some("engine") {
            let Some(event) = reference.and_then(|reference| wire.original(reference)) else {
                return out;
            };
            match event {
                HistoryEvent::Completed { .. } => {
                    // A non-streamed assistant record starts a fresh response
                    // boundary in the native decoder. Close any synthetic
                    // response assembled before this opaque record first.
                    out.extend(self.finish(wire));
                    self.started = false;
                    self.ended = true;
                }
                HistoryEvent::MessageStart { response } => {
                    // Native `fs` flushes any synthesized response before an
                    // opaque physical message_start enters the stream.
                    out.extend(self.finish(wire));
                    self.model = response.model.clone();
                    self.started = true;
                    self.stopped = false;
                    self.ended = false;
                    self.blocks.clear();
                    self.thinking_starts.clear();
                }
                HistoryEvent::ContentBlockStart {
                    index,
                    content_block,
                } => {
                    if matches!(content_block, ContentBlock::Reasoning { .. }) {
                        if let Some(reference) = reference {
                            self.thinking_starts.insert(*index, reference);
                        }
                    }
                    self.blocks.insert(
                        *index,
                        match content_block {
                            ContentBlock::Text { .. } | ContentBlock::TextJsUtf16 { .. } => {
                                OutputBlockKind::Text
                            }
                            ContentBlock::Reasoning { .. } => OutputBlockKind::Thinking,
                            _ => OutputBlockKind::Other,
                        },
                    );
                }
                HistoryEvent::ContentBlockStop { index } => {
                    // A tool chunk dropped by a hook never started a block.
                    let Some(kind) = self.blocks.remove(index) else {
                        return out;
                    };
                    if kind == OutputBlockKind::Thinking {
                        self.replay_thinking(*index, reference.unwrap_or(u64::MAX), wire, &mut out);
                    }
                }
                HistoryEvent::ContentBlockDelta {
                    index,
                    delta: HistoryContentDelta::SignatureDelta { .. },
                } if self.blocks.get(index) == Some(&OutputBlockKind::Thinking) => {
                    // Emit signed source deltas together at block stop. A Mod's
                    // thinking chunks affect display only.
                    return out;
                }
                HistoryEvent::MessageStop => {
                    self.close_blocks(&mut out, wire);
                    self.ended = true;
                }
                _ => {}
            }
            out.push(event.clone());
            return out;
        }
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| u32::try_from(index).ok());
        match value.get("kind").and_then(Value::as_str) {
            Some("thinking") => {
                // The caller emits this chunk to the UI. Signed history is
                // reconstructed from source refs at content-block stop.
                return out;
            }
            Some("text") => {
                let Some(index) = index else { return out };
                let Some(text) = value.get("text").and_then(Value::as_str) else {
                    return out;
                };
                self.block(index, OutputBlockKind::Text, &mut out);
                if let Some(event) = reference
                    .filter(|reference| wire.chunk(*reference) == Some(chunk))
                    .and_then(|reference| wire.original(reference))
                {
                    out.push(event.clone());
                } else if let Some(utf16_code_units) = chunk
                    .strings
                    .iter()
                    .find(|sidecar| sidecar.pointer == "/text")
                    .map(|sidecar| sidecar.code_units.clone())
                {
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::TextJsUtf16Delta {
                            text: text.to_owned(),
                            utf16_code_units,
                        },
                    });
                } else {
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::TextDelta {
                            text: text.to_owned(),
                        },
                    });
                }
            }
            Some("tool") => {
                let Some(index) = index else { return out };
                let (Some(id), Some(name)) = (
                    value.get("id").and_then(Value::as_str),
                    value.get("name").and_then(Value::as_str),
                ) else {
                    return out;
                };
                self.start(&mut out);
                self.blocks.insert(index, OutputBlockKind::Tool);
                if let Some(event) = reference
                    .filter(|reference| wire.chunk(*reference) == Some(chunk))
                    .and_then(|reference| wire.original(reference))
                {
                    out.push(event.clone());
                } else {
                    out.push(HistoryEvent::ContentBlockStart {
                        index,
                        content_block: ContentBlock::ToolCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            input: json!({}),
                        },
                    });
                }
            }
            Some("input") => {
                let Some(index) = index else { return out };
                if self.blocks.get(&index) != Some(&OutputBlockKind::Tool) {
                    return out;
                }
                let Some(json) = value.get("json").and_then(Value::as_str) else {
                    return out;
                };
                if let Some(event) = reference
                    .filter(|reference| wire.chunk(*reference) == Some(chunk))
                    .and_then(|reference| wire.original(reference))
                {
                    out.push(event.clone());
                } else {
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::InputJsonDelta {
                            partial_json: json.to_owned(),
                        },
                    });
                }
            }
            Some("stop") => {
                self.start(&mut out);
                self.close_blocks(&mut out, wire);
                self.stopped = true;
                if let Some(event) = reference
                    .filter(|reference| wire.chunk(*reference) == Some(chunk))
                    .and_then(|reference| wire.original(reference))
                {
                    out.push(event.clone());
                } else {
                    let usage = value
                        .get("usage")
                        .filter(|usage| !usage.is_null())
                        .map(|usage| {
                            let mut result = ExecutionUsage::default();
                            let counts = result.counts_mut();
                            counts.input_tokens =
                                usage["input_tokens"].as_u64().unwrap_or_default();
                            counts.output_tokens =
                                usage["output_tokens"].as_u64().unwrap_or_default();
                            counts.cache_read_tokens = usage["cache_read_input_tokens"]
                                .as_u64()
                                .unwrap_or_default();
                            counts.cache_write_tokens = usage["cache_creation_input_tokens"]
                                .as_u64()
                                .unwrap_or_default();
                            result
                        });
                    out.push(HistoryEvent::MessageDelta {
                        delta: HistoryMessageDelta {
                            stop_reason: value
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            stop_details: None,
                        },
                        usage,
                    });
                }
            }
            _ => {}
        }
        out
    }

    /// Close a synthetic response when a Mod finishes without forwarding the
    /// original message-stop frame.
    pub fn finish(&mut self, wire: &TurnStepEncoder) -> Vec<HistoryEvent> {
        if !self.started || self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.close_blocks(&mut out, wire);
        if !self.stopped {
            out.push(HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                },
                usage: None,
            });
        }
        out.push(HistoryEvent::MessageStop);
        self.ended = true;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutionUsage, HistoryMessageDelta, HistoryResponse};

    fn response() -> HistoryResponse {
        HistoryResponse {
            id: "msg_1".into(),
            model: "claude-sonnet-4-6".into(),
            content: vec![],
            stop_reason: None,
            stop_details: None,
            usage: ExecutionUsage::default(),
            cost: None,
            provider_metadata: json!({"opaque":true}),
        }
    }

    fn response_with(content: Vec<ContentBlock>) -> HistoryResponse {
        HistoryResponse {
            content,
            ..response()
        }
    }

    fn server_tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ServerToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    fn fallback_event(discarded_blocks: Vec<usize>) -> HistoryEvent {
        HistoryEvent::ServerFallback {
            event: Box::new(
                lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent {
                    from_model: "primary".into(),
                    to_model: "fallback".into(),
                    reason: "refusal".into(),
                    api_refusal_category: None,
                    mid_stream: true,
                    request_id: Some("req-1".into()),
                    discarded_blocks,
                    retained_blocks: Vec::new(),
                    retained_text: String::new(),
                    final_stop_reason: None,
                },
            ),
            profile: "profile".into(),
            lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane {
                for_model: "primary".into(),
                model: "fallback".into(),
                mode: lingxi_llm_client::providers::anthropic::fallback_request::LaneMode::Default,
            },
        }
    }

    fn completed_reference(encoder: &mut TurnStepEncoder, response: HistoryResponse) -> u64 {
        encoder
            .push(HistoryEvent::Completed {
                response: Box::new(response),
            })
            .reference
    }

    #[test]
    fn server_tool_uses_include_server_and_mcp_calls_with_native_end_times() {
        let mut encoder = TurnStepEncoder::default();
        let response = response_with(vec![
            server_tool_use("server-1", "web_search", json!({"query":"one"})),
            ContentBlock::AdvisorToolResult {
                tool_use_id: "server-1".into(),
                content: json!({"ok":true}),
                is_error: false,
            },
            // Native keeps the first record for a duplicate id, even after it
            // has received a corresponding tool result.
            server_tool_use("server-1", "rewritten", json!({"query":"two"})),
            ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: json!({
                    "type":"mcp_tool_use",
                    "id":"mcp-1",
                    "name":"lookup",
                    "input":{"key":"value"},
                }),
            },
            ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: json!({
                    "type":"mcp_tool_result",
                    "tool_use_id":"mcp-1",
                    "content":{"found":true},
                }),
            },
        ]);
        let reference = completed_reference(&mut encoder, response);

        let result = encoder.result_for_references("turn", 4, &[reference]);
        let calls = result.value["serverToolUses"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "server-1");
        assert_eq!(calls[0]["name"], "web_search");
        assert_eq!(calls[0]["input"], json!({"query":"one"}));
        assert_eq!(calls[1]["id"], "mcp-1");
        assert_eq!(calls[1]["name"], "lookup");
        assert_eq!(calls[1]["input"], json!({"key":"value"}));
        for call in calls {
            let started_at = call["startedAt"].as_u64().expect("Native timestamp");
            let ended_at = call["endedAt"].as_u64().expect("tool result timestamp");
            assert!(ended_at >= started_at);
        }
    }

    #[test]
    fn server_tool_uses_are_omitted_when_no_server_side_call_was_accepted() {
        let mut encoder = TurnStepEncoder::default();
        let reference = completed_reference(
            &mut encoder,
            response_with(vec![ContentBlock::ToolCall {
                id: "toolu-local".into(),
                name: "Read".into(),
                input: json!({"path":"a.txt"}),
            }]),
        );

        let result = encoder.result_for_references("turn", 0, &[reference]);
        assert_eq!(
            result.value["toolUses"],
            json!([{"name":"Read","input":{"path":"a.txt"}}])
        );
        assert!(result.value.get("serverToolUses").is_none());
    }

    #[test]
    fn server_tool_uses_include_the_streamed_message_stop_snapshot() {
        let mut encoder = TurnStepEncoder::default();
        let terminal = encoder.push(HistoryEvent::MessageStop).reference;
        encoder.note_stream_response(
            terminal,
            response_with(vec![server_tool_use(
                "server-streamed",
                "web_search",
                json!({}),
            )]),
        );

        let result = encoder.result_for_references("turn", 1, &[terminal]);
        assert_eq!(result.value["serverToolUses"][0]["id"], "server-streamed");
        assert!(result.value["serverToolUses"][0]["startedAt"].as_u64().is_some());
        assert!(result.value["serverToolUses"][0].get("endedAt").is_none());
    }

    #[test]
    fn response_observed_resets_provisional_server_and_local_tool_uses() {
        let mut encoder = TurnStepEncoder::default();
        let old = completed_reference(
            &mut encoder,
            response_with(vec![
                server_tool_use("server-old", "web_search", json!({})),
                ContentBlock::ToolCall {
                    id: "toolu-old".into(),
                    name: "ReadOld".into(),
                    input: json!({}),
                },
            ]),
        );
        let reset = encoder
            .push(HistoryEvent::ResponseObserved {
                model: "fallback".into(),
                response_id: Some("msg-fallback".into()),
            })
            .reference;
        let fallback = completed_reference(
            &mut encoder,
            response_with(vec![
                server_tool_use(
                    "server-new",
                    "web_fetch",
                    json!({"url":"https://example.test"}),
                ),
                ContentBlock::ToolCall {
                    id: "toolu-new".into(),
                    name: "ReadNew".into(),
                    input: json!({}),
                },
            ]),
        );

        let result = encoder.result_for_references("turn", 0, &[old, reset, fallback]);
        assert_eq!(result.value["toolUses"], json!([{"name":"ReadNew","input":{}}]));
        assert_eq!(
            result.value["serverToolUses"][0]["id"], "server-new",
            "a fallback stream reset must discard provisional server calls"
        );
        assert!(result.value["serverToolUses"][0].get("endedAt").is_none());
    }

    #[test]
    fn server_fallback_discards_server_and_local_calls_by_source_block_index() {
        let mut encoder = TurnStepEncoder::default();
        let response = completed_reference(
            &mut encoder,
            response_with(vec![
                server_tool_use("server-discarded", "web_search", json!({})),
                ContentBlock::ToolCall {
                    id: "toolu-discarded".into(),
                    name: "ReadDiscarded".into(),
                    input: json!({}),
                },
                server_tool_use("server-retained", "web_fetch", json!({})),
                ContentBlock::ToolCall {
                    id: "toolu-retained".into(),
                    name: "ReadRetained".into(),
                    input: json!({}),
                },
            ]),
        );
        let fallback = encoder.push(fallback_event(vec![0, 1])).reference;

        let result = encoder.result_for_references("turn", 0, &[response, fallback]);
        assert_eq!(
            result.value["toolUses"],
            json!([{"name":"ReadRetained","input":{}}])
        );
        let calls = result.value["serverToolUses"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "server-retained");
        assert_eq!(calls[0]["name"], "web_fetch");
        assert_eq!(calls[0]["input"], json!({}));
    }

    #[test]
    fn each_event_gets_a_ref_and_engine_events_keep_the_original() {
        let mut encoder = TurnStepEncoder::default();
        let start = HistoryEvent::MessageStart {
            response: Box::new(response()),
        };
        assert_eq!(
            encoder.push(start.clone()).chunk.value,
            json!({"kind":"engine","ref":1})
        );
        assert_eq!(encoder.original(1), Some(&start));
        assert_eq!(encoder.original(0), None);
        assert_eq!(encoder.original(2), None);
        assert_eq!(
            encoder.push(HistoryEvent::MessageStop).chunk.value,
            json!({"kind":"engine","ref":2})
        );
    }

    #[test]
    fn native_cost_quote_is_an_opaque_engine_event() {
        let quote = HistoryEvent::CostQuoteObserved {
            estimate: Some(crate::CostEstimate::unestimated(crate::PricingModelRef {
                pricing_provider_id: crate::ProviderId::AnthropicFirstParty,
                billing_model: "served-model".into(),
                request_model: "served-model".into(),
                display_model: "served-model".into(),
            })),
            native_server_fallback: true,
            summary_model: Some("served-model".into()),
        };
        let mut encoder = TurnStepEncoder::default();
        let chunk = encoder.push(quote.clone()).chunk.clone();
        assert_eq!(chunk.value, json!({"kind":"engine","ref":1}));

        let decoded = TurnStepDecoder::new("model").consume(&chunk, &encoder);
        assert_eq!(decoded, vec![quote]);
    }

    #[test]
    fn text_thinking_and_tool_arguments_map_to_ordered_chunks() {
        let mut encoder = TurnStepEncoder::default();
        encoder.push(HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None,
            },
        });
        assert_eq!(
            encoder
                .push(HistoryEvent::ContentBlockDelta {
                    index: 0,
                    delta: HistoryContentDelta::TextDelta {
                        text: "hello".into()
                    },
                })
                .chunk.value,
            json!({"kind":"text","index":0,"text":"hello","ref":2})
        );
        encoder.push(HistoryEvent::ContentBlockStart {
            index: 1,
            content_block: ContentBlock::Reasoning {
                text: String::new(),
                signature: None,
            },
        });
        assert_eq!(
            encoder
                .push(HistoryEvent::ContentBlockDelta {
                    index: 1,
                    delta: HistoryContentDelta::ThinkingDelta {
                        thinking: "thought".into()
                    },
                })
                .chunk.value,
            json!({"kind":"thinking","index":1,"text":"thought","ref":4})
        );
        assert_eq!(
            encoder
                .push(HistoryEvent::ContentBlockStart {
                    index: 2,
                    content_block: ContentBlock::ToolCall {
                        id: "toolu_1".into(),
                        name: "Read".into(),
                        input: json!({})
                    },
                })
                .chunk.value,
            json!({"kind":"tool","index":2,"id":"toolu_1","name":"Read","ref":5})
        );
        assert_eq!(
            encoder
                .push(HistoryEvent::ContentBlockDelta {
                    index: 2,
                    delta: HistoryContentDelta::InputJsonDelta {
                        partial_json: "{\"x\":".into()
                    },
                })
                .chunk.value,
            json!({"kind":"input","index":2,"json":"{\"x\":","ref":6})
        );
        // A partial JSON delta outside a tool block is opaque upstream.
        assert_eq!(
            encoder
                .push(HistoryEvent::ContentBlockDelta {
                    index: 0,
                    delta: HistoryContentDelta::InputJsonDelta {
                        partial_json: "bad".into()
                    },
                })
                .chunk.value,
            json!({"kind":"engine","ref":7})
        );
    }

    #[test]
    fn stop_uses_response_model_and_normalized_token_names() {
        let mut encoder = TurnStepEncoder::default();
        encoder.push(HistoryEvent::MessageStart {
            response: Box::new(response()),
        });
        let mut usage = ExecutionUsage::default();
        let counts = usage.counts_mut();
        counts.input_tokens = 10;
        counts.output_tokens = 4;
        counts.cache_read_tokens = 3;
        counts.cache_write_tokens = 2;
        assert_eq!(
            encoder
                .push(HistoryEvent::MessageDelta {
                    delta: HistoryMessageDelta {
                        stop_reason: Some("end_turn".into()),
                        stop_details: None
                    },
                    usage: Some(usage),
                })
                .chunk.value,
            json!({"kind":"stop","stopReason":"end_turn","usage":{
            "input_tokens":10,"output_tokens":4,"cache_read_input_tokens":3,
            "cache_creation_input_tokens":2,"model":"claude-sonnet-4-6"
        },"ref":2})
        );
    }

    #[test]
    fn stop_uses_complete_terminal_measurement_including_explicit_zeroes() {
        let mut response = response();
        let seed = response.usage.counts_mut();
        seed.input_tokens = 11;
        seed.cache_read_tokens = 13;
        seed.cache_write_tokens = 17;
        let mut encoder = TurnStepEncoder::default();
        encoder.push(HistoryEvent::MessageStart {
            response: Box::new(response),
        });
        let mut delta = ExecutionUsage::default();
        delta.counts_mut().output_tokens = 19;
        let chunk = &encoder
            .push(HistoryEvent::MessageDelta {
                delta: HistoryMessageDelta {
                    stop_reason: Some("tool_use".into()),
                    stop_details: None,
                },
                usage: Some(delta),
            })
            .chunk;
        assert_eq!(chunk.value["usage"]["input_tokens"], 0);
        assert_eq!(chunk.value["usage"]["cache_read_input_tokens"], 0);
        assert_eq!(chunk.value["usage"]["cache_creation_input_tokens"], 0);
        assert_eq!(chunk.value["usage"]["output_tokens"], 19);
    }

    #[test]
    fn result_summarizes_completed_assistant_record() {
        let mut encoder = TurnStepEncoder::default();
        assert_eq!(
            encoder.result("turn-1", 0).value,
            json!({
                "turnId":"turn-1","index":0,"answer":"","toolUses":[],"stopReason":null,"usage":null
            })
        );
        let mut completed = response();
        completed.stop_reason = Some("tool_use".into());
        completed.content = vec![
            ContentBlock::Text {
                text: "  first".into(),
                cache_control: None,
                citations: None,
            },
            ContentBlock::ToolCall {
                id: "toolu_1".into(),
                name: "Read".into(),
                input: json!({"path":"a"}),
            },
            ContentBlock::Text {
                text: "second  ".into(),
                cache_control: None,
                citations: None,
            },
        ];
        completed.usage.counts_mut().output_tokens = 7;
        encoder.push(HistoryEvent::Completed {
            response: Box::new(completed),
        });
        let result = encoder.result("turn-1", 2);
        assert_eq!(result.value["answer"], "first\nsecond");
        assert_eq!(
            result.value["toolUses"],
            json!([{"name":"Read","input":{"path":"a"}}])
        );
        assert_eq!(result.value["stopReason"], "tool_use");
        assert_eq!(result.value["usage"]["output_tokens"], 7);
    }

    #[test]
    fn result_for_references_excludes_interleaved_next_calls() {
        let mut encoder = TurnStepEncoder::default();
        let mut first = response();
        first.content = vec![ContentBlock::Text {
            text: "first source".into(),
            cache_control: None,
            citations: None,
        }];
        first.usage.counts_mut().output_tokens = 3;
        let first_ref = encoder
            .push(HistoryEvent::Completed {
                response: Box::new(first),
            })
            .reference;
        encoder.push(HistoryEvent::MessageStop);
        let mut second = response();
        second.content = vec![ContentBlock::Text {
            text: "second source".into(),
            cache_control: None,
            citations: None,
        }];
        second.usage.counts_mut().output_tokens = 5;
        let second_ref = encoder
            .push(HistoryEvent::Completed {
                response: Box::new(second),
            })
            .reference;
        let first_result = encoder.result_for_references("turn", 0, &[first_ref]);
        let second_result = encoder.result_for_references("turn", 0, &[second_ref]);
        assert_eq!(first_result.value["answer"], "first source");
        assert_eq!(first_result.value["usage"]["output_tokens"], 3);
        assert_eq!(second_result.value["answer"], "second source");
        assert_eq!(second_result.value["usage"]["output_tokens"], 5);
    }

    #[test]
    fn streamed_records_use_their_own_terminal_refs_for_results() {
        let mut encoder = TurnStepEncoder::default();
        let first_ref = encoder.push(HistoryEvent::MessageStop).reference;
        let second_ref = encoder.push(HistoryEvent::MessageStop).reference;
        let mut first = response();
        first.content = vec![ContentBlock::Text {
            text: " first stream ".into(),
            cache_control: None,
            citations: None,
        }];
        first.usage.counts_mut().output_tokens = 3;
        let mut second = response();
        second.content = vec![ContentBlock::Text {
            text: "second stream".into(),
            cache_control: None,
            citations: None,
        }];
        second.usage.counts_mut().output_tokens = 5;
        encoder.note_stream_response(first_ref, first);
        encoder.note_stream_response(second_ref, second);
        assert_eq!(encoder.next_reference(), 3);
        let first = encoder.result_for_references("turn", 0, &[first_ref]);
        let second = encoder.result_for_references("turn", 0, &[second_ref]);
        assert_eq!(first.value["answer"], "first stream");
        assert_eq!(first.value["usage"]["output_tokens"], 3);
        assert_eq!(second.value["answer"], "second stream");
        assert_eq!(second.value["usage"]["output_tokens"], 5);
    }

    #[test]
    fn result_uses_javascript_trim_semantics() {
        assert_eq!(js_trim("\u{feff} x \u{feff}"), "x");
        assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
    }

    #[test]
    fn decoder_rewrites_text_delta_and_preserves_opaque_envelope() {
        let mut wire = TurnStepEncoder::default();
        let start = wire
            .push(HistoryEvent::MessageStart {
                response: Box::new(response()),
            })
            .chunk
            .clone();
        let block = wire
            .push(HistoryEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                    cache_control: None,
                    citations: None,
                },
            })
            .chunk
            .clone();
        let mut text = wire
            .push(HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta { text: "old".into() },
            })
            .chunk
            .clone();
        text.value["text"] = json!("new");
        let mut decoder = TurnStepDecoder::new("model");
        assert_eq!(
            decoder.consume(&start, &wire),
            vec![wire.original(1).unwrap().clone()]
        );
        assert_eq!(
            decoder.consume(&block, &wire),
            vec![wire.original(2).unwrap().clone()]
        );
        assert_eq!(
            decoder.consume(&text, &wire),
            vec![HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta { text: "new".into() },
            }]
        );
    }

    #[test]
    fn decoder_closes_synthetic_response_before_opaque_completed_record() {
        let mut wire = TurnStepEncoder::default();
        let original = wire
            .push(HistoryEvent::Completed {
                response: Box::new(response()),
            })
            .chunk
            .clone();
        let mut decoder = TurnStepDecoder::new("model");
        let synthetic = decoder.consume(
            &Utf16JsonProjection::plain(json!({
                "kind": "text",
                "index": 0,
                "text": "before",
            })),
            &wire,
        );
        assert!(matches!(
            synthetic.first(),
            Some(HistoryEvent::MessageStart { .. })
        ));
        let boundary = decoder.consume(&original, &wire);
        assert!(matches!(
            boundary.first(),
            Some(HistoryEvent::ContentBlockStop { .. })
        ));
        assert!(matches!(
            boundary.get(1),
            Some(HistoryEvent::MessageDelta { .. })
        ));
        assert!(matches!(boundary.get(2), Some(HistoryEvent::MessageStop)));
        assert!(matches!(
            boundary.get(3),
            Some(HistoryEvent::Completed { .. })
        ));
        let after = decoder.consume(
            &Utf16JsonProjection::plain(json!({
                "kind": "text",
                "index": 0,
                "text": "after",
            })),
            &wire,
        );
        assert!(matches!(
            after.first(),
            Some(HistoryEvent::MessageStart { .. })
        ));
    }

    #[test]
    fn decoder_closes_synthetic_response_before_opaque_message_start() {
        let mut wire = TurnStepEncoder::default();
        let original = wire
            .push(HistoryEvent::MessageStart {
                response: Box::new(response()),
            })
            .chunk
            .clone();
        let mut decoder = TurnStepDecoder::new("model");
        decoder.consume(
            &Utf16JsonProjection::plain(json!({
                "kind": "text",
                "index": 0,
                "text": "before",
            })),
            &wire,
        );
        let boundary = decoder.consume(&original, &wire);
        assert!(matches!(
            boundary.first(),
            Some(HistoryEvent::ContentBlockStop { .. })
        ));
        assert!(matches!(
            boundary.get(1),
            Some(HistoryEvent::MessageDelta { .. })
        ));
        assert!(matches!(boundary.get(2), Some(HistoryEvent::MessageStop)));
        assert!(matches!(
            boundary.get(3),
            Some(HistoryEvent::MessageStart { .. })
        ));
        let next = decoder.consume(
            &Utf16JsonProjection::plain(json!({
                "kind": "text",
                "index": 0,
                "text": "after",
            })),
            &wire,
        );
        assert!(matches!(
            next.first(),
            Some(HistoryEvent::ContentBlockStart { .. })
        ));
    }

    #[test]
    fn decoder_drops_orphaned_tool_stop_when_hook_omits_tool() {
        let mut wire = TurnStepEncoder::default();
        let start = wire
            .push(HistoryEvent::MessageStart {
                response: Box::new(response()),
            })
            .chunk
            .clone();
        wire.push(HistoryEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolCall {
                id: "toolu_1".into(),
                name: "Read".into(),
                input: json!({}),
            },
        });
        let block_stop = wire
            .push(HistoryEvent::ContentBlockStop { index: 0 })
            .chunk
            .clone();
        let mut decoder = TurnStepDecoder::new("model");
        decoder.consume(&start, &wire);
        assert!(decoder.consume(&block_stop, &wire).is_empty());
    }

    #[test]
    fn decoder_envelopes_synthetic_text_and_closes_response() {
        let wire = TurnStepEncoder::default();
        let mut decoder = TurnStepDecoder::new("claude-sonnet-4-6");
        let events = decoder.consume(
            &Utf16JsonProjection::plain(json!({
                "kind": "text",
                "index": 0,
                "text": "hello",
            })),
            &wire,
        );
        assert!(matches!(events[0], HistoryEvent::MessageStart { .. }));
        assert!(matches!(
            events[1],
            HistoryEvent::ContentBlockStart { index: 0, .. }
        ));
        assert_eq!(
            events[2],
            HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta {
                    text: "hello".into()
                },
            }
        );
        let tail = decoder.consume(
            &Utf16JsonProjection::plain(json!({
                "kind": "stop",
                "stopReason": "end_turn",
                "usage": null,
            })),
            &wire,
        );
        assert_eq!(tail[0], HistoryEvent::ContentBlockStop { index: 0 });
        assert!(matches!(tail[1], HistoryEvent::MessageDelta { .. }));
        assert_eq!(decoder.finish(&wire), vec![HistoryEvent::MessageStop]);
    }
}
