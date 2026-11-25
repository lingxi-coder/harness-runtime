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
use lingxi_llm_client::exact_text::ExactText;
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
        initial_input: Option<Utf16JsonProjection>,
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

// Preserve the legacy start-event display until the first delta. The shared
// builder then owns all mutation; protocol snapshots and event lifetimes stay
// with this accumulator. In particular, a no-delta block is not normalized.
#[derive(Debug)]
enum TextBuffer {
    Initial(String, Option<Vec<u16>>),
    Active(ExactText),
}
impl TextBuffer {
    fn display(&self) -> &str {
        match self {
            Self::Initial(text, _) => text,
            Self::Active(text) => text.display(),
        }
    }
    fn units(&self) -> Option<&[u16]> {
        match self {
            Self::Initial(_, units) => units.as_deref(),
            Self::Active(text) => text.carried_units(),
        }
    }
    fn builder(&mut self) -> &mut ExactText {
        if matches!(self, Self::Initial(..)) {
            let Self::Initial(text, units) =
                std::mem::replace(self, Self::Initial(String::new(), None))
            else {
                unreachable!()
            };
            *self = Self::Active(match units {
                Some(units) => ExactText::from_units(units),
                None => ExactText::from_text(text),
            });
        }
        let Self::Active(text) = self else {
            unreachable!()
        };
        text
    }
    fn into_parts(self) -> (String, Option<Vec<u16>>) {
        match self {
            Self::Initial(text, units) => (text, units),
            Self::Active(text) => text.into_parts(),
        }
    }
}

/// Per-block state held during accumulation.
#[derive(Debug)]
struct BlockState {
    kind: BlockKind,
    /// Used for `Text` and `Thinking`.
    text_buf: TextBuffer,
    /// Whether accumulated deltas should replace the source block's text field.
    preserved_text_dirty: bool,
    /// Initial array entries and streamed deltas, populated on the first citation delta.
    citation_items: Vec<JsonPayload>,
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
                text_buf: TextBuffer::Initial(text_buf, text_utf16_code_units),
                preserved_text_dirty: false,
                citation_items: Vec::new(),
                json_buf: Vec::new(),
                signature: None,
            },
        );
        Ok(())
    }

    /// Replace an unfinished block with the authoritative terminal snapshot.
    /// It still passes through the ordinary stop/admission path exactly once.
    pub(crate) fn replace_completed_block(
        &mut self,
        index: u32,
        block: ContentBlock,
    ) -> Result<(), StreamingError> {
        self.start_block(index, BlockKind::Preserved(block))
    }

    pub(crate) fn open_indices(&self) -> impl Iterator<Item = u32> + '_ {
        self.blocks.keys().copied()
    }

    pub(crate) fn thinking_text(&self, index: u32) -> Option<&str> {
        let state = self.blocks.get(&index)?;
        matches!(state.kind, BlockKind::Thinking).then_some(state.text_buf.display())
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
            BlockKind::Text { .. } | BlockKind::Thinking => {
                state.text_buf.builder().push_str(text);
                Ok(())
            }
            BlockKind::PreservedText(_) => {
                state.text_buf.builder().push_str(text);
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
        match &mut state.kind {
            BlockKind::Text { .. } => {
                state.text_buf.builder().push_units(utf16_code_units);
                Ok(())
            }
            BlockKind::PreservedText(_) => {
                state.text_buf.builder().push_units(utf16_code_units);
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
    /// A start event's array seeds the list. Other non-null values are authoritative.
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
            BlockKind::Text { citations, .. } => {
                if state.citation_items.is_empty() {
                    if let Some(Some(initial)) = citations {
                        if !initial.is_root_array() {
                            return Ok(());
                        }
                        state.citation_items = serde_json::from_str(initial.as_json_source())
                            .map_err(|error| StreamingError::JsonPayload(error.to_string()))?;
                        *citations = None;
                    }
                }
                state.citation_items.push(citation);
            }
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
            } => {
                *current = citations;
                state.citation_items.clear();
            }
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
                state.text_buf = TextBuffer::Initial(
                    value.string_at("/text").unwrap_or_default().to_owned(),
                    value.string_units("/text"),
                );
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

    /// Append a signature fragment on a `Thinking` block (`signature_delta`).
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
                state
                    .signature
                    .get_or_insert_with(String::new)
                    .push_str(sig);
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
        let (text, exact_units) = state.text_buf.into_parts();
        let completed = match state.kind {
            BlockKind::Text { citations, .. } => CompletedBlock::Text {
                text,
                citations: assembled_citations(citations, &state.citation_items)?,
                utf16_code_units: exact_units,
            },
            BlockKind::Thinking => CompletedBlock::Thinking {
                thinking: text,
                signature: state.signature,
            },
            BlockKind::ToolUse {
                initial_input,
                id,
                name,
                provider_id,
            } => {
                let input = if state.json_buf.is_empty() {
                    initial_input.unwrap_or_else(|| {
                        Utf16JsonProjection::parse("{}").expect("empty JSON object is valid")
                    })
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
                    input,
                    id,
                    name,
                    provider_id,
                }
            }
            BlockKind::PreservedText(mut block) => {
                if state.preserved_text_dirty {
                    if let ContentBlock::ProviderContent { value, .. } = &mut block {
                        let units = exact_units.unwrap_or_else(|| text.encode_utf16().collect());
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
        self.incomplete_text_blocks_with_indices()
            .into_iter()
            .map(|(_, block)| block)
            .collect()
    }

    pub(crate) fn incomplete_text_blocks_with_indices(&self) -> Vec<(u32, ContentBlock)> {
        let mut blocks: Vec<_> = self.blocks.iter().collect();
        blocks.sort_unstable_by_key(|(index, _)| llm_runtime::stream_content_order(**index));
        blocks
            .into_iter()
            .filter_map(|(index, state)| match &state.kind {
                BlockKind::PreservedText(block) if !state.text_buf.display().is_empty() => {
                    let mut block = block.clone();
                    if state.preserved_text_dirty {
                        if let ContentBlock::ProviderContent { value, .. } = &mut block {
                            let units = state.text_buf.units().map_or_else(
                                || state.text_buf.display().encode_utf16().collect::<Vec<_>>(),
                                <[u16]>::to_vec,
                            );
                            replace_provider_text(value, &units)
                                .expect("provider text member exists on preserved text block");
                        }
                    }
                    Some((*index, block))
                }
                BlockKind::Text { citations, .. } if !state.text_buf.display().is_empty() => {
                    let citations = citations_to_core(
                        assembled_citations(citations.clone(), &state.citation_items)
                            .expect("an array of validated citation payloads is valid JSON"),
                    );
                    Some((
                        *index,
                        match state.text_buf.units() {
                            Some(utf16_code_units) => ContentBlock::TextJsUtf16 {
                                text: state.text_buf.display().to_owned(),
                                utf16_code_units: utf16_code_units.to_vec(),
                                citations,
                            },
                            None => ContentBlock::Text {
                                text: state.text_buf.display().to_owned(),
                                citations,
                            },
                        },
                    ))
                }
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

fn assembled_citations(
    original: Option<Option<JsonPayload>>,
    items: &[JsonPayload],
) -> Result<Option<Option<JsonPayload>>, StreamingError> {
    if items.is_empty() {
        return Ok(original);
    }
    // JsonPayload serializes its exact source, including numeric spellings and UTF-16 escapes.
    let array = serde_json::to_string(items)
        .and_then(|source| JsonPayload::parse(&source))
        .map_err(|error| StreamingError::JsonPayload(error.to_string()))?;
    Ok(Some(Some(array)))
}

#[cfg(test)]
mod citation_tests {
    use super::*;

    fn text_block(citations: Option<Option<JsonPayload>>) -> BlockAccumulator {
        let mut acc = BlockAccumulator::new();
        acc.start_block(
            0,
            BlockKind::Text {
                citations,
                utf16_code_units: None,
                initial_text: "visible".into(),
            },
        )
        .unwrap();
        acc
    }

    #[test]
    fn citation_deltas_preserve_exact_source_and_survive_partial_snapshots() {
        let seed = JsonPayload::parse(r#"[ { "n": 1.00, "s": "\ud800" } , 0e5 ]"#).unwrap();
        let mut acc = text_block(Some(Some(seed)));
        acc.append_citation(0, JsonPayload::parse(r#"{"s":"\u0041"}"#).unwrap())
            .unwrap();
        let snapshot = acc.incomplete_text_blocks();
        assert!(matches!(snapshot.as_slice(),
            [ContentBlock::Text { citations: Some(Some(array)), .. }]
                if array.string_units("/0/s") == Some(vec![0xd800])
                    && array.string_at("/2/s") == Some("A")
        ));
        acc.append_citation(0, JsonPayload::parse("false").unwrap())
            .unwrap();
        let CompletedBlock::Text {
            citations: Some(Some(array)),
            ..
        } = acc.stop_block(0).unwrap()
        else {
            panic!("expected text with a citation array");
        };
        assert_eq!(
            array.as_json_source(),
            r#"[{ "n": 1.00, "s": "\ud800" },0e5,{"s":"\u0041"},false]"#
        );
    }

    #[test]
    fn authoritative_citation_snapshots_replace_deltas_and_preserve_field_presence() {
        let null = Some(None);
        let empty = Some(Some(JsonPayload::parse("[ ]").unwrap()));
        let opaque = Some(Some(JsonPayload::parse(r#"{"provider":true}"#).unwrap()));
        let new_list = Some(Some(JsonPayload::parse("[2]").unwrap()));
        for (replacement, with_next_delta) in [
            (None, new_list.clone()),
            (null, new_list.clone()),
            (empty, new_list),
            (opaque.clone(), opaque),
        ] {
            for append_next in [false, true] {
                let mut acc = text_block(None);
                acc.append_citation(0, JsonPayload::parse("1").unwrap())
                    .unwrap();
                acc.set_text_citations(0, replacement.clone()).unwrap();
                if append_next {
                    acc.append_citation(0, JsonPayload::parse("2").unwrap())
                        .unwrap();
                }
                let CompletedBlock::Text { citations, .. } = acc.stop_block(0).unwrap() else {
                    panic!("expected a text block");
                };
                assert_eq!(
                    &citations,
                    if append_next {
                        &with_next_delta
                    } else {
                        &replacement
                    }
                );
            }
        }
    }
}

#[cfg(test)]
mod utf16_text_tests {
    use super::*;

    #[test]
    fn every_surrogate_partition_preserves_intermediate_and_final_text() {
        let source = [0x61, 0xd83d, 0xde00, 0xd800, 0xd801, 0xdc01, 0xdc00, 0x62];
        for split in 0..=source.len() {
            let mut accumulator = BlockAccumulator::new();
            accumulator
                .start_block(
                    0,
                    BlockKind::Text {
                        citations: None,
                        utf16_code_units: None,
                        initial_text: String::new(),
                    },
                )
                .unwrap();
            accumulator.append_text_utf16(0, &source[..split]).unwrap();
            let snapshot = accumulator.incomplete_text_blocks();
            if split > 0 {
                assert!(
                    matches!(&snapshot[0], ContentBlock::TextJsUtf16 { text, utf16_code_units, .. }
                    if text == &String::from_utf16_lossy(&source[..split]) && utf16_code_units == &source[..split])
                );
            }
            accumulator.append_text_utf16(0, &[]).unwrap();
            accumulator.append_text_utf16(0, &source[split..]).unwrap();
            accumulator.append_text(0, "suffix😀").unwrap();
            let mut expected = source.to_vec();
            expected.extend("suffix😀".encode_utf16());
            assert!(
                matches!(accumulator.stop_block(0).unwrap(), CompletedBlock::Text {
                text, utf16_code_units: Some(units), ..
            } if text == String::from_utf16_lossy(&expected) && units == expected)
            );
        }
    }

    #[test]
    fn preserved_text_recombines_frames_and_retains_lone_surrogates() {
        let mut accumulator = BlockAccumulator::new();
        accumulator
            .start_block(
                0,
                BlockKind::PreservedText(ContentBlock::ProviderContent {
                    protocol: "anthropic_messages".into(),
                    value: Utf16JsonProjection::parse(r#"{"type":"text","text":"prefix\ud83d"}"#)
                        .unwrap(),
                }),
            )
            .unwrap();
        accumulator.append_text_utf16(0, &[0xde00, 0xd800]).unwrap();
        accumulator.append_text(0, "tail").unwrap();
        let expected = [
            "prefix".encode_utf16().collect::<Vec<_>>(),
            vec![0xd83d, 0xde00, 0xd800],
            "tail".encode_utf16().collect(),
        ]
        .concat();
        for block in accumulator.incomplete_text_blocks().into_iter().chain(
            match accumulator.stop_block(0).unwrap() {
                CompletedBlock::Preserved(block) => Some(block),
                _ => None,
            },
        ) {
            assert!(matches!(block, ContentBlock::ProviderContent { value, .. }
                if value.string_units("/text") == Some(expected.clone())));
        }
    }

    #[test]
    fn initial_exact_units_are_authoritative_after_first_delta() {
        let mut accumulator = BlockAccumulator::new();
        accumulator
            .start_block(
                0,
                BlockKind::Text {
                    citations: None,
                    utf16_code_units: Some(vec![0xd83d]),
                    initial_text: "different display".into(),
                },
            )
            .unwrap();
        accumulator.append_text_utf16(0, &[0xde00]).unwrap();
        assert!(
            matches!(accumulator.stop_block(0).unwrap(), CompletedBlock::Text { text, .. } if text == "😀")
        );
    }

    #[test]
    fn start_without_a_delta_retains_the_existing_display_and_exact_shape() {
        let mut accumulator = BlockAccumulator::new();
        accumulator
            .start_block(
                0,
                BlockKind::Text {
                    citations: None,
                    utf16_code_units: Some(vec![0xd800]),
                    initial_text: "initial display".into(),
                },
            )
            .unwrap();
        assert!(
            matches!(accumulator.stop_block(0).unwrap(), CompletedBlock::Text {
            text, utf16_code_units: Some(units), ..
        } if text == "initial display" && units == [0xd800])
        );
    }

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
