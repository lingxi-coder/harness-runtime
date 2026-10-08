//! Per-block accumulator for the streaming SSE path.
//!
//! Tracks one `BlockState` per `index: u32` for the lifetime of a single
//! `messages.create` response. Each block starts via
//! [`BlockAccumulator::start_block`], receives 0..N appends, and is
//! finalized via [`BlockAccumulator::stop_block`] which returns a
//! [`CompletedBlock`]. The accumulator drops the entry on stop —
//! subsequent operations on the same `index` error with
//! `StreamingError::DoubleStop`.
//!
//! See M5-04 plan reverse-engineered byte-locks for the source
//! semantics (claude.ts:1995-2300).
#![forbid(unsafe_code)]

use super::StreamingError;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ToolUseId};
use lingxi_llm_client::protocol::{JsonPayload, JsonPayloadFragment};
use std::collections::HashMap;

/// Tag identifying what kind of content block is being accumulated.
///
/// Constructed from a `StreamEvent::ContentBlockStart` payload at the call
/// site (formerly `api_client::types::StreamEvent`; now
/// `llm_runtime::HistoryEvent`). `ToolUse` carries the API-provided id + name
/// verbatim; the input JSON is reassembled from `input_json_delta` chunks.
#[derive(Debug, Clone)]
pub enum BlockKind {
    /// A `text` block — accumulates `text_delta` chunks while retaining its
    /// provider field presence and, for the narrow UTF-16 case, exact wire text.
    Text {
        /// Exact provider field presence for `citations`.
        citations: Option<Option<JsonPayload>>,
        /// Exact UTF-16 text, when the block arrived in that representation.
        utf16_code_units: Option<Vec<u16>>,
        /// Text carried directly on the block start event, if any.
        initial_text: String,
    },
    /// A `tool_use` block — accumulates `input_json_delta` chunks.
    ToolUse {
        /// Stable identifier echoed back in the matching `ToolResult`.
        id: ToolUseId,
        /// Tool name (e.g. `"Read"`).
        name: String,
        /// Verbatim provider-issued tool-call id, preserved for egress replay.
        provider_id: Option<String>,
    },
    /// A `thinking` block — accumulates `thinking_delta` chunks.
    /// Signature (if any) is set via [`BlockAccumulator::set_signature`].
    Thinking,
    /// Opaque Anthropic text. Deltas update the text on its single raw source
    /// block; no typed Text sibling is created.
    PreservedText(ContentBlock),
    /// A low-frequency server-side block captured in full from the start event.
    Preserved(ContentBlock),
    /// Any other variant the accumulator cannot represent. Stores nothing;
    /// `stop_block` returns `CompletedBlock::Skipped`.
    Other,
}

impl BlockKind {
    fn name(&self) -> &'static str {
        match self {
            BlockKind::Text { .. } => "text",
            BlockKind::ToolUse { .. } => "tool_use",
            BlockKind::Thinking => "thinking",
            BlockKind::PreservedText(_) => "preserved_text",
            BlockKind::Preserved(_) => "preserved",
            BlockKind::Other => "other",
        }
    }
}

/// One finished content block, ready to be appended to the assistant
/// message and (if `ToolUse`) dispatched as a tool call.
#[derive(Debug, Clone)]
pub enum CompletedBlock {
    /// Plain text.
    Text {
        /// Concatenated body of all `text_delta` chunks.
        text: String,
        /// Exact provider field presence for `citations`.
        citations: Option<Option<JsonPayload>>,
        /// Exact UTF-16 text, when the block arrived in that representation.
        utf16_code_units: Option<Vec<u16>>,
    },
    /// Tool invocation, with reassembled JSON input.
    ToolUse {
        /// Tool use identifier.
        id: ToolUseId,
        /// Tool name.
        name: String,
        /// Reassembled tool input.
        input: Utf16JsonProjection,
        /// Verbatim provider-issued tool-call id, preserved for egress replay.
        provider_id: Option<String>,
    },
    /// Extended thinking.
    Thinking {
        /// Concatenated thinking body.
        thinking: String,
        /// Optional cryptographic signature.
        signature: Option<String>,
    },
    /// A low-frequency server-side block captured verbatim from
    /// `ContentBlockStart` (see [`BlockKind::Preserved`]). Caller appends it
    /// to the assistant message unchanged.
    Preserved(ContentBlock),
    /// A [`BlockKind::Other`] variant — caller drops it.
    Skipped,
}

/// Per-block state held during accumulation.
#[derive(Debug)]
struct BlockState {
    kind: BlockKind,
    /// Used for `Text` and `Thinking`.
    text_buf: String,
    /// Preserved provider text can carry lone UTF-16 units outside Rust String.
    text_utf16_code_units: Option<Vec<u16>>,
    /// Whether accumulated deltas should replace the source block's text field.
    preserved_text_dirty: bool,
    /// Used for `ToolUse` (raw `partial_json` concat).
    json_buf: Vec<u16>,
    /// Set by `signature_delta` on a `Thinking` block.
    signature: Option<String>,
}

/// In-progress accumulator. One per active stream consumer.
#[derive(Debug, Default)]
pub struct BlockAccumulator {
    blocks: HashMap<u32, BlockState>,
}

impl BlockAccumulator {
    /// Construct an empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
        }
    }

    /// Register a new block at `index` with the given kind. If `index`
    /// already exists, the previous entry is overwritten (mirrors
    /// claude.ts:1996-2070 which `contentBlocks[part.index] = { ... }`
    /// unconditionally).
    ///
    /// # Errors
    /// Never returns `Err` today; the `Result` shape is retained for
    /// future-proofing.
    pub fn start_block(&mut self, index: u32, kind: BlockKind) -> Result<(), StreamingError> {
        let (text_buf, text_utf16_code_units) = match &kind {
            BlockKind::Text {
                initial_text,
                utf16_code_units,
                ..
            } => (initial_text.clone(), utf16_code_units.clone()),
            BlockKind::PreservedText(ContentBlock::ProviderContent { value, .. }) => (
                value.string_at("/text").unwrap_or_default().to_owned(),
                value.string_units("/text"),
            ),
            _ => (String::new(), None),
        };
        self.blocks.insert(
            index,
            BlockState {
                kind,
                text_buf,
                text_utf16_code_units,
                preserved_text_dirty: false,
                json_buf: Vec::new(),
                signature: None,
            },
        );
        Ok(())
    }

    /// Append `text` to the `Text` or `Thinking` buffer of the block at
    /// `index`. Errors if no such block, or if the block is not
    /// text-shaped.
    ///
    /// # Errors
    /// `BlockNotFound` if `index` was never started; `TypeMismatch` if
    /// the block at `index` was started as a non-text variant.
    pub fn append_text(&mut self, index: u32, text: &str) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &mut state.kind {
            BlockKind::Text {
                utf16_code_units, ..
            } => {
                if let Some(utf16_code_units) = utf16_code_units {
                    utf16_code_units.extend(text.encode_utf16());
                    state.text_buf = String::from_utf16_lossy(utf16_code_units);
                } else {
                    state.text_buf.push_str(text);
                }
                Ok(())
            }
            BlockKind::Thinking => {
                state.text_buf.push_str(text);
                Ok(())
            }
            BlockKind::PreservedText(_) => {
                if let Some(units) = &mut state.text_utf16_code_units {
                    units.extend(text.encode_utf16());
                    state.text_buf = String::from_utf16_lossy(units);
                } else {
                    state.text_buf.push_str(text);
                }
                state.preserved_text_dirty = true;
                Ok(())
            }
            other => Err(StreamingError::TypeMismatch {
                index,
                expected: other.name(),
                got: "text_delta",
            }),
        }
    }

    /// Append an exact JavaScript UTF-16 text delta, merging surrogate halves
    /// across provider frames before projecting the valid UTF-8 display text.
    pub fn append_text_utf16(
        &mut self,
        index: u32,
        utf16_code_units: &[u16],
    ) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        let previous_text = state.text_buf.clone();
        match &mut state.kind {
            BlockKind::Text {
                utf16_code_units: units,
                ..
            } => {
                let units = units.get_or_insert_with(|| previous_text.encode_utf16().collect());
                units.extend_from_slice(utf16_code_units);
                state.text_buf = String::from_utf16_lossy(units);
                Ok(())
            }
            BlockKind::PreservedText(_) => {
                let units = state
                    .text_utf16_code_units
                    .get_or_insert_with(|| previous_text.encode_utf16().collect());
                units.extend_from_slice(utf16_code_units);
                state.text_buf = String::from_utf16_lossy(units);
                state.preserved_text_dirty = true;
                Ok(())
            }
            other => Err(StreamingError::TypeMismatch {
                index,
                expected: other.name(),
                got: "utf16_text_delta",
            }),
        }
    }

    /// Append a `partial_json` chunk to the `ToolUse` buffer.
    ///
    /// # Errors
    /// `BlockNotFound` if `index` was never started; `TypeMismatch` if
    /// the block at `index` was started as a non-tool variant.
    pub fn append_json(
        &mut self,
        index: u32,
        partial: &JsonPayloadFragment,
    ) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &state.kind {
            // `server_tool_use` (and any future Preserved block) may stream its
            // input via `input_json_delta`; buffer it and merge on stop.
            BlockKind::ToolUse { .. } | BlockKind::PreservedText(_) | BlockKind::Preserved(_) => {
                state.json_buf.extend_from_slice(partial.utf16_code_units());
                Ok(())
            }
            other => Err(StreamingError::TypeMismatch {
                index,
                expected: other.name(),
                got: "input_json_delta",
            }),
        }
    }

    /// Retain citation deltas on their text block for the persisted assistant row.
    /// A start event that already supplied a citation value remains authoritative.
    pub fn append_citation(
        &mut self,
        index: u32,
        citation: JsonPayload,
    ) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &mut state.kind {
            BlockKind::Text { citations, .. } => match citations {
                None | Some(None) => {
                    *citations = Some(Some(append_payload_array(None, &citation)?));
                }
                Some(Some(values)) if values.is_root_array() => {
                    *values = append_payload_array(Some(values), &citation)?;
                }
                Some(Some(_)) => {}
            },
            BlockKind::PreservedText(_) => {}
            other => {
                return Err(StreamingError::TypeMismatch {
                    index,
                    expected: other.name(),
                    got: "citations_delta",
                });
            }
        }
        Ok(())
    }

    /// Replace the Text citation-field snapshot after SDK block assembly.
    /// This preserves null and empty-array presence and makes the completed
    /// citation list authoritative over incremental deltas.
    pub fn set_text_citations(
        &mut self,
        index: u32,
        citations: Option<Option<JsonPayload>>,
    ) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &mut state.kind {
            BlockKind::Text {
                citations: current, ..
            } => *current = citations,
            other => {
                return Err(StreamingError::TypeMismatch {
                    index,
                    expected: other.name(),
                    got: "text_citations",
                });
            }
        }
        Ok(())
    }

    /// Replace an opaque text block with the decoder's complete native value.
    pub fn set_provider_content_snapshot(
        &mut self,
        index: u32,
        value: Utf16JsonProjection,
    ) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &mut state.kind {
            BlockKind::PreservedText(ContentBlock::ProviderContent {
                protocol,
                value: current,
            }) if protocol == "anthropic_messages" => {
                state.text_buf = value.string_at("/text").unwrap_or_default().to_owned();
                state.text_utf16_code_units = value.string_units("/text");
                state.preserved_text_dirty = false;
                *current = value;
                Ok(())
            }
            other => Err(StreamingError::TypeMismatch {
                index,
                expected: other.name(),
                got: "provider_content_snapshot",
            }),
        }
    }

    /// Set the signature on a `Thinking` block (from a `signature_delta`).
    ///
    /// # Errors
    /// `BlockNotFound` if `index` was never started; `TypeMismatch` if
    /// the block at `index` was started as a non-thinking variant.
    pub fn set_signature(&mut self, index: u32, sig: &str) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &state.kind {
            BlockKind::Thinking => {
                state.signature = Some(sig.to_string());
                Ok(())
            }
            other => Err(StreamingError::TypeMismatch {
                index,
                expected: other.name(),
                got: "signature_delta",
            }),
        }
    }

    /// Finalize the block at `index`, removing it from internal state
    /// and returning a `CompletedBlock`.
    ///
    /// # Errors
    /// `DoubleStop` if the block was already finalized (or never
    /// started). `ToolUseJsonParse` if a `ToolUse` block's accumulated
    /// `partial_json` buffer is not valid JSON.
    pub fn stop_block(&mut self, index: u32) -> Result<CompletedBlock, StreamingError> {
        let state = self
            .blocks
            .remove(&index)
            .ok_or(StreamingError::DoubleStop { index })?;
        let completed = match state.kind {
            BlockKind::Text {
                citations,
                utf16_code_units,
                ..
            } => CompletedBlock::Text {
                text: state.text_buf,
                citations,
                utf16_code_units,
            },
            BlockKind::Thinking => CompletedBlock::Thinking {
                thinking: state.text_buf,
                signature: state.signature,
            },
            BlockKind::ToolUse {
                id,
                name,
                provider_id,
            } => {
                let input = if state.json_buf.is_empty() {
                    Utf16JsonProjection::parse("{}").expect("empty JSON object is valid")
                } else {
                    Utf16JsonProjection::parse_utf16_json(&state.json_buf).map_err(|e| {
                        StreamingError::ToolUseJsonParse {
                            index,
                            reason: e.to_string(),
                            buffer: String::from_utf16_lossy(&state.json_buf),
                        }
                    })?
                };
                CompletedBlock::ToolUse {
                    id,
                    name,
                    input,
                    provider_id,
                }
            }
            BlockKind::PreservedText(mut block) => {
                if state.preserved_text_dirty {
                    if let ContentBlock::ProviderContent { value, .. } = &mut block {
                        let units = state.text_utf16_code_units.as_deref().map_or_else(
                            || state.text_buf.encode_utf16().collect::<Vec<_>>(),
                            <[u16]>::to_vec,
                        );
                        replace_provider_text(value, &units)
                            .map_err(|error| StreamingError::JsonPayload(error.to_string()))?;
                    }
                }
                CompletedBlock::Preserved(block)
            }
            BlockKind::Preserved(mut block) => {
                // Merge any `input_json_delta`-streamed input into a
                // `server_tool_use` block (the start event carries an empty/seed
                // input). Other Preserved blocks replay their start payload as-is.
                if !state.json_buf.is_empty() {
                    if let ContentBlock::ServerToolUse { input, .. } = &mut block {
                        let fragment =
                            JsonPayloadFragment::from_utf16_code_units(state.json_buf.clone());
                        if let Ok(parsed) = JsonPayload::parse_fragments([&fragment]) {
                            if let Ok(projection) =
                                Utf16JsonProjection::parse(parsed.as_json_source())
                            {
                                *input = projection;
                            }
                        }
                    }
                }
                CompletedBlock::Preserved(block)
            }
            BlockKind::Other => CompletedBlock::Skipped,
        };
        Ok(completed)
    }

    /// Snapshot visible text from blocks that have started but have not yet
    /// received `content_block_stop`.
    ///
    /// Text deltas are emitted to clients immediately, so losing this buffer on
    /// a transport close makes the persisted conversation disagree with what the
    /// user already saw. Tool JSON is deliberately excluded: an incomplete tool
    /// call is neither safe to persist nor executable. Thinking-only buffers are
    /// excluded as well because they are not user-visible answer content.
    #[must_use]
    pub(crate) fn incomplete_text_blocks(&self) -> Vec<ContentBlock> {
        let mut blocks: Vec<_> = self.blocks.iter().collect();
        blocks.sort_unstable_by_key(|(index, _)| **index);
        blocks
            .into_iter()
            .filter_map(|(_, state)| match &state.kind {
                BlockKind::PreservedText(block) if !state.text_buf.is_empty() => {
                    let mut block = block.clone();
                    if state.preserved_text_dirty {
                        if let ContentBlock::ProviderContent { value, .. } = &mut block {
                            let units = state.text_utf16_code_units.as_deref().map_or_else(
                                || state.text_buf.encode_utf16().collect::<Vec<_>>(),
                                <[u16]>::to_vec,
                            );
                            replace_provider_text(value, &units)
                                .expect("provider text member exists on preserved text block");
                        }
                    }
                    Some(block)
                }
                BlockKind::Text {
                    citations,
                    utf16_code_units,
                    ..
                } if !state.text_buf.is_empty() => Some(match utf16_code_units {
                    Some(utf16_code_units) => ContentBlock::TextJsUtf16 {
                        text: state.text_buf.clone(),
                        utf16_code_units: utf16_code_units.clone(),
                        citations: citations_to_core(citations.clone()),
                    },
                    None => ContentBlock::Text {
                        text: state.text_buf.clone(),
                        citations: citations_to_core(citations.clone()),
                    },
                }),
                _ => None,
            })
            .collect()
    }

    /// `true` when no blocks are currently in-flight.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.blocks.is_empty()
    }
}

fn replace_provider_text(
    value: &mut Utf16JsonProjection,
    code_units: &[u16],
) -> Result<(), lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    let text = Utf16JsonProjection::root_string(
        String::from_utf16_lossy(code_units),
        code_units.to_vec(),
    )?;
    value.replace_pointer_projection("/text", text)
}

fn citations_to_core(
    citations: Option<Option<JsonPayload>>,
) -> Option<Option<Utf16JsonProjection>> {
    citations.map(|citation| {
        citation.map(|payload| {
            Utf16JsonProjection::parse(payload.as_json_source())
                .expect("SDK citation payload is valid JSON source")
        })
    })
}

fn append_payload_array(
    existing: Option<&JsonPayload>,
    value: &JsonPayload,
) -> Result<JsonPayload, StreamingError> {
    let mut source = String::from("[");
    if let Some(existing) = existing {
        let length = existing.array_len("").ok_or_else(|| {
            StreamingError::JsonPayload("citation payload is not an array".into())
        })?;
        for index in 0..length {
            let item = existing.array_item(index).ok_or_else(|| {
                StreamingError::JsonPayload("citation array item has no source".into())
            })?;
            if index > 0 {
                source.push(',');
            }
            source.push_str(item.as_json_source());
        }
        if length > 0 {
            source.push(',');
        }
    }
    source.push_str(value.as_json_source());
    source.push(']');
    JsonPayload::parse(&source).map_err(|error| StreamingError::JsonPayload(error.to_string()))
}

#[cfg(test)]
mod utf16_text_tests {
    use super::*;

    #[test]
    fn adjacent_exact_deltas_recombine_surrogate_pairs() {
        let mut accumulator = BlockAccumulator::new();
        accumulator
            .start_block(
                3,
                BlockKind::Text {
                    citations: None,
                    utf16_code_units: None,
                    initial_text: String::new(),
                },
            )
            .unwrap();
        accumulator.append_text_utf16(3, &[0xd83d]).unwrap();
        accumulator.append_text_utf16(3, &[0xde00]).unwrap();
        assert!(matches!(
            accumulator.stop_block(3).unwrap(),
            CompletedBlock::Text {
                text,
                utf16_code_units: Some(units),
                ..
            } if text == "😀" && units == vec![0xd83d, 0xde00]
        ));
    }
}
