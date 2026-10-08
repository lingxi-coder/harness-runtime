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
use lingxi_core::types::{ContentBlock, ToolUseId};
use serde_json::Value;
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
        citations: Option<Option<Value>>,
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
        citations: Option<Option<Value>>,
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
        input: Value,
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
    /// Used for `ToolUse` (raw `partial_json` concat).
    json_buf: String,
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
        let text_buf = match &kind {
            BlockKind::Text { initial_text, .. } => initial_text.clone(),
            _ => String::new(),
        };
        self.blocks.insert(
            index,
            BlockState {
                kind,
                text_buf,
                json_buf: String::new(),
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
            BlockKind::Thinking | BlockKind::PreservedText(_) => {
                state.text_buf.push_str(text);
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
            BlockKind::PreservedText(_) => Ok(()),
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
    pub fn append_json(&mut self, index: u32, partial: &str) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &state.kind {
            // `server_tool_use` (and any future Preserved block) may stream its
            // input via `input_json_delta`; buffer it and merge on stop.
            BlockKind::ToolUse { .. } | BlockKind::PreservedText(_) | BlockKind::Preserved(_) => {
                state.json_buf.push_str(partial);
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
    pub fn append_citation(&mut self, index: u32, citation: Value) -> Result<(), StreamingError> {
        let state = self
            .blocks
            .get_mut(&index)
            .ok_or(StreamingError::BlockNotFound { index })?;
        match &mut state.kind {
            BlockKind::Text { citations, .. } => match citations {
                None | Some(None) => *citations = Some(Some(Value::Array(vec![citation]))),
                Some(Some(Value::Array(values))) => values.push(citation),
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
        citations: Option<Option<Value>>,
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
        value: Value,
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
                state.text_buf = value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
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
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str::<Value>(&state.json_buf).map_err(|e| {
                        StreamingError::ToolUseJsonParse {
                            index,
                            reason: e.to_string(),
                            buffer: state.json_buf.clone(),
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
                if let ContentBlock::ProviderContent { value, .. } = &mut block {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("text".into(), Value::String(state.text_buf));
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
                        if let Ok(parsed) = serde_json::from_str::<Value>(&state.json_buf) {
                            *input = parsed;
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
                    if let ContentBlock::ProviderContent { value, .. } = &mut block {
                        if let Some(object) = value.as_object_mut() {
                            object.insert("text".into(), Value::String(state.text_buf.clone()));
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
                        citations: citations.clone(),
                    },
                    None => ContentBlock::Text {
                        text: state.text_buf.clone(),
                        citations: citations.clone(),
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
