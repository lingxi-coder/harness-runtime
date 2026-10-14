//! Presentation-only projection from canonical SDK responses and streams.
//! Provider block assembly is owned by the SDK; history serialization remains
//! stable at this host boundary. Replay companions exist only in that durable
//! history representation: SDK execution and block assembly never use them.
//! History input conversion consumes those companions once before SDK dispatch.
use crate::upstream::{error, usage};
use crate::*;
use lingxi_llm_client::providers::anthropic::stream_observation::{self, NativeDelta, TextBlock};
use lingxi_llm_client::{self as client, protocol as wire};
use std::collections::BTreeSet;

fn invalid(error: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest {
        message: error.to_string(),
    }
}

fn stop_details(payload: &wire::JsonPayload) -> Result<HistoryStopDetails, LlmError> {
    serde_json::from_str(payload.as_json_source()).map_err(invalid)
}

fn serialized_payload(value: &impl serde::Serialize) -> Result<wire::JsonPayload, LlmError> {
    let source = serde_json::to_string(value).map_err(invalid)?;
    wire::JsonPayload::parse(&source).map_err(invalid)
}

fn empty_source_object() -> wire::JsonPayload {
    wire::JsonPayload::parse("{}").expect("empty object is valid JSON")
}

fn source_object_or_native(
    value: &wire::JsonPayload,
    null_is_empty: bool,
) -> Result<wire::JsonPayload, LlmError> {
    if value.is_root_object() {
        return Ok(value.clone());
    }
    if null_is_empty && value.as_json_source().trim() == "null" {
        return Ok(empty_source_object());
    }
    empty_source_object()
        .with_object_field("native", value)
        .map_err(invalid)
}

fn metadata_and_namespace(
    metadata: &wire::JsonPayload,
) -> Result<(wire::JsonPayload, wire::JsonPayload), LlmError> {
    let root = source_object_or_native(metadata, true)?;
    let namespace = match root.payload_at("/llm_client") {
        Some(value) => source_object_or_native(&value, true)?,
        None => empty_source_object(),
    };
    Ok((root, namespace))
}

fn normalize_upstream_metadata(metadata: &mut wire::JsonPayload) -> Result<(), LlmError> {
    let (root, namespace) = metadata_and_namespace(metadata)?;
    *metadata = root
        .with_object_field("llm_client", &namespace)
        .map_err(invalid)?;
    Ok(())
}

fn set_root_metadata_field(
    metadata: &mut wire::JsonPayload,
    key: &str,
    value: &wire::JsonPayload,
) -> Result<(), LlmError> {
    *metadata = source_object_or_native(metadata, true)?
        .with_object_field(key, value)
        .map_err(invalid)?;
    Ok(())
}

fn set_upstream_metadata_field(
    metadata: &mut wire::JsonPayload,
    key: &str,
    value: &wire::JsonPayload,
) -> Result<(), LlmError> {
    let (root, namespace) = metadata_and_namespace(metadata)?;
    let namespace = namespace.with_object_field(key, value).map_err(invalid)?;
    *metadata = root
        .with_object_field("llm_client", &namespace)
        .map_err(invalid)?;
    Ok(())
}

fn remove_upstream_metadata_field(
    metadata: &mut wire::JsonPayload,
    key: &str,
) -> Result<bool, LlmError> {
    if !metadata.is_root_object() {
        return Ok(false);
    }
    let Some(namespace) = metadata
        .payload_at("/llm_client")
        .filter(wire::JsonPayload::is_root_object)
    else {
        return Ok(false);
    };
    let pointer = format!("/{}", key.replace('~', "~0").replace('/', "~1"));
    if namespace.raw_source_at(&pointer).is_none() {
        return Ok(false);
    }
    let namespace = namespace.without_object_field(key).map_err(invalid)?;
    *metadata = metadata
        .with_object_field("llm_client", &namespace)
        .map_err(invalid)?;
    Ok(true)
}

fn clear_provider_observation_metadata(value: &mut wire::JsonPayload) -> Result<(), LlmError> {
    if value.string_at("/type").as_deref() != Some("lingxi_observation") {
        return Ok(());
    }
    let Some(metadata) = value
        .payload_at("/metadata")
        .filter(|metadata| metadata.is_root_object())
    else {
        return Ok(());
    };
    if metadata.raw_source_at("/llm_client").is_some() {
        let metadata = metadata
            .without_object_field("llm_client")
            .map_err(invalid)?;
        *value = value
            .with_object_field("metadata", &metadata)
            .map_err(invalid)?;
    }
    Ok(())
}

pub(crate) use client::replay::has_replay_metadata;

fn needs_replay_companion(block: &wire::ContentBlock) -> bool {
    match block {
        // ProviderContent already is its complete replay representation.
        wire::ContentBlock::ProviderContent { .. } => false,
        // Citation presence now has a native field on the durable Text block.
        wire::ContentBlock::Text {
            thought_signature, ..
        } => thought_signature.is_some(),
        _ => has_replay_metadata(block),
    }
}

fn is_native_text_provider_content(block: &wire::ContentBlock) -> bool {
    matches!(
        block,
        wire::ContentBlock::ProviderContent {
            protocol: wire::ProtocolFamily::AnthropicMessages,
            value,
        } if value.string_at("/type").as_deref() == Some("text")
    )
}

fn source_has_only_fields(payload: &wire::JsonPayload, fields: &[&str]) -> bool {
    let Some(actual_count) = payload.object_len("") else {
        return false;
    };
    let known_count = fields
        .iter()
        .filter(|field| payload.raw_source_at(&format!("/{field}")).is_some())
        .count();
    actual_count == known_count
}

fn source_string_has_unpaired_units(payload: &wire::JsonPayload, pointer: &str) -> bool {
    payload
        .payload_at(pointer)
        .and_then(|value| value.root_string_code_units())
        .is_some_and(|units| String::from_utf16(&units).is_err())
}

fn companion(
    block: &wire::ContentBlock,
    family: wire::ProtocolFamily,
) -> Result<ContentBlock, LlmError> {
    let protocol = serde_json::to_value(family)
        .map_err(invalid)?
        .as_str()
        .unwrap()
        .to_owned();
    let block = serde_json::to_string(block).map_err(invalid)?;
    let value = wire::JsonPayload::parse(&format!(
        r#"{{"type":"lingxi_replay_metadata","block":{block}}}"#
    ))
    .map_err(invalid)?;
    Ok(ContentBlock::ProviderContent { protocol, value })
}

pub(crate) fn append_observation(
    metadata: &mut wire::JsonPayload,
    key: &str,
    value: wire::JsonPayload,
) -> Result<(), LlmError> {
    let (root, namespace) = metadata_and_namespace(metadata)?;
    let pointer = format!("/{}", key.replace('~', "~0").replace('/', "~1"));
    let mut items = namespace
        .payload_at(&pointer)
        .filter(wire::JsonPayload::is_root_array)
        .map(|array| {
            (0..array.array_len("").unwrap_or_default())
                .filter_map(|index| array.array_item(index))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    items.push(value);
    let source = items
        .iter()
        .map(wire::JsonPayload::as_json_source)
        .collect::<Vec<_>>()
        .join(",");
    let array = wire::JsonPayload::parse(&format!("[{source}]")).map_err(invalid)?;
    let namespace = namespace.with_object_field(key, &array).map_err(invalid)?;
    *metadata = root
        .with_object_field("llm_client", &namespace)
        .map_err(invalid)?;
    Ok(())
}

pub(crate) fn attach_server_fallback_cost_quote(
    metadata: &mut wire::JsonPayload,
    quote: wire::JsonPayload,
) -> Result<(), LlmError> {
    set_upstream_metadata_field(
        metadata,
        crate::history::SERVER_FALLBACK_COST_QUOTE_KEY,
        &quote,
    )
}

pub(crate) fn attach_computer_binding(
    metadata: &mut wire::JsonPayload,
    binding: &lingxi_core::host::NativeContinuationBinding,
) -> Result<(), LlmError> {
    let binding = serialized_payload(binding)?;
    set_upstream_metadata_field(metadata, "computer_binding", &binding)
}

fn clear_computer_binding(metadata: &mut wire::JsonPayload) -> Result<(), LlmError> {
    remove_upstream_metadata_field(metadata, "computer_binding").map(|_| ())
}

fn host_block(block: wire::ContentBlock) -> Result<ContentBlock, LlmError> {
    Ok(match block {
        wire::ContentBlock::Text {
            text, citations, ..
        } => ContentBlock::Text {
            text,
            citations,
            cache_control: None,
        },
        wire::ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations,
            ..
        } => ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            citations,
            cache_control: None,
        },
        wire::ContentBlock::Thinking { text, signature } => {
            ContentBlock::Reasoning { text, signature }
        }
        wire::ContentBlock::RedactedThinking { data } => ContentBlock::RedactedThinking { data },
        wire::ContentBlock::ToolUse {
            id, name, input, ..
        } => ContentBlock::ToolCall {
            id: id.as_str().into(),
            name,
            input,
        },
        wire::ContentBlock::ProviderContent {
            protocol,
            mut value,
        } => {
            let block_type = value.string_at("/type");
            if matches!(
                block_type.as_deref(),
                Some(
                    "lingxi_computer_binding"
                        | "lingxi_computer_receipt"
                        | "lingxi_computer_continuation"
                        | "lingxi_computer_receipt_ack"
                        | "lingxi_computer_abandoned"
                        | "lingxi_native_content"
                )
            ) {
                return Err(invalid(
                    "provider content cannot manufacture host computer history markers",
                ));
            }
            // This is raw provider-owned content. The observation carrier is
            // also used by the host projector, but projector-created carriers
            // bypass `host_block`; remove its reserved namespace here so a
            // provider cannot forge host response metadata through a native
            // block with the same type label.
            clear_provider_observation_metadata(&mut value)?;
            let allowed_fields: Option<&'static [&'static str]> = match block_type.as_deref() {
                Some("server_tool_use") => Some(&["type", "id", "name", "input"]),
                Some("connector_text") => Some(&["type", "connector_text", "signature"]),
                Some("advisor_tool_result") => {
                    Some(&["type", "tool_use_id", "content", "is_error"])
                }
                _ => None,
            };
            let typed_string_fields_are_safe = match block_type.as_deref() {
                Some("server_tool_use") => {
                    !source_string_has_unpaired_units(&value, "/id")
                        && !source_string_has_unpaired_units(&value, "/name")
                }
                Some("connector_text") => {
                    !source_string_has_unpaired_units(&value, "/connector_text")
                        && !source_string_has_unpaired_units(&value, "/signature")
                }
                Some("advisor_tool_result") => {
                    !source_string_has_unpaired_units(&value, "/tool_use_id")
                }
                _ => false,
            };
            if protocol == wire::ProtocolFamily::AnthropicMessages
                && typed_string_fields_are_safe
                && allowed_fields.is_some_and(|fields| source_has_only_fields(&value, fields))
            {
                serde_json::from_str(value.as_json_source()).map_err(invalid)?
            } else {
                ContentBlock::ProviderContent {
                    protocol: serde_json::to_value(protocol)
                        .map_err(invalid)?
                        .as_str()
                        .unwrap()
                        .into(),
                    value,
                }
            }
        }
        _ => {
            return Err(LlmError::UnsupportedCapability {
                capability: "non-conversation output block".into(),
            });
        }
    })
}

fn native_host_block(
    block: wire::ContentBlock,
    protocol: wire::ProtocolFamily,
) -> Result<ContentBlock, LlmError> {
    if matches!(block, wire::ContentBlock::Native { .. }) {
        let protocol = serde_json::to_value(protocol)
            .map_err(invalid)?
            .as_str()
            .ok_or_else(|| invalid("protocol is not a string"))?
            .to_owned();
        let block = serde_json::to_string(&block).map_err(invalid)?;
        let value = wire::JsonPayload::parse(&format!(
            r#"{{"type":"lingxi_native_content","block":{block}}}"#
        ))
        .map_err(invalid)?;
        Ok(ContentBlock::ProviderContent { protocol, value })
    } else {
        host_block(block)
    }
}
fn stop(reason: wire::StopReason) -> String {
    match reason {
        wire::StopReason::EndTurn => "end_turn".into(),
        wire::StopReason::ToolUse => "tool_use".into(),
        wire::StopReason::MaxTokens => "max_tokens".into(),
        wire::StopReason::StopSequence => "stop_sequence".into(),
        wire::StopReason::Refusal => "refusal".into(),
        wire::StopReason::Other(s) => s,
    }
}

// These adapters exercise the host projection against SDK wire fixtures. They
// are unit-test code only: the test-support Cargo feature cannot expose them.

fn wire_block_index(block: usize) -> Result<u32, LlmError> {
    u32::try_from(block)
        .ok()
        .filter(|index| *index < 0x8000_0000)
        .ok_or_else(|| invalid("provider output block index exceeds the host range"))
}

pub(crate) struct HistoryProjector {
    server_lane: Option<client::providers::anthropic::fallback_request::ServerLane>,
    server_profile: String,
    request_model: String,
    server_request_id: Option<String>,
    server_response_model: Option<String>,
    pending_server_hop: Option<client::providers::anthropic::fallback_response::FallbackHop>,
    server_event_emitted: bool,
    current_response_id: Option<String>,
    pub(crate) observation: (wire::UsageReport, wire::InferenceReport),
    pub(crate) family: wire::ProtocolFamily,
    blocks: BTreeSet<u32>,
    closed: BTreeSet<u32>,
    reasoning_blocks: BTreeSet<u32>,
    opaque_text_blocks: BTreeSet<u32>,
    pub(crate) metadata: wire::JsonPayload,
    /// Latest exact Native Anthropic resumability payload observed on the
    /// stream. An explicit JSON null remains present as `Some(null)`.
    resumable: Option<wire::JsonPayload>,
    force_observation_snapshot: bool,
    done: bool,
    started: bool,
    stop_delta_seen: bool,
    assembly: client::stream_assembly::StreamAccumulator,
    pending_projection: BTreeSet<usize>,
    pending_error: Option<LlmError>,
}
impl std::fmt::Debug for HistoryProjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamStreamHistoryProjector")
            .field("family", &self.family)
            .finish()
    }
}
impl HistoryProjector {
    pub(crate) fn admit_server_fallback(
        &mut self,
        lane: Option<client::providers::anthropic::fallback_request::ServerLane>,
        profile: String,
        model: String,
        request_id: Option<String>,
    ) -> Result<(), LlmError> {
        self.server_lane = lane;
        self.server_profile = profile;
        self.request_model = model.clone();
        // Merely arming a lane does not imply that the response used a
        // fallback model. Only a valid hop or sticky terminal observation may
        // override the provider's actual message-start model.
        self.server_response_model = None;
        self.server_request_id = request_id;
        if self.server_lane.is_some() {
            clear_host_projection_fields(&mut self.metadata)?;
        }
        Ok(())
    }
    pub(crate) fn observe_server_fallback_cost_quote(
        &mut self,
        quote: wire::JsonPayload,
    ) -> Result<(), LlmError> {
        attach_server_fallback_cost_quote(&mut self.metadata, quote)
    }
    pub(crate) fn clear_server_fallback_cost_quote(&mut self) -> Result<(), LlmError> {
        let removed = remove_upstream_metadata_field(
            &mut self.metadata,
            crate::history::SERVER_FALLBACK_COST_QUOTE_KEY,
        )?;
        self.force_observation_snapshot |= removed;
        Ok(())
    }
    fn observe_server_fallback_model(&mut self, model: &str) -> Result<(), LlmError> {
        self.server_response_model = Some(model.to_owned());
        self.assembly.set_response_model(model);
        set_upstream_metadata_field(
            &mut self.metadata,
            "response_model",
            &wire::JsonPayload::string(model),
        )
    }
    fn server_event(
        &mut self,
        event: client::providers::anthropic::fallback_response::ServerFallbackEvent,
    ) -> Result<HistoryEvent, LlmError> {
        self.server_event_emitted = true;
        self.server_response_model = Some(event.to_model.clone());
        self.assembly.apply_server_fallback(&event);
        for index in &event.discarded_blocks {
            self.pending_projection.remove(index);
        }
        let lane = self.server_lane.clone().expect("admitted lane");
        append_observation(
            &mut self.metadata,
            "server_fallback_events",
            serialized_payload(&crate::history::HistoryServerFallback {
                event: event.clone(),
                profile: self.server_profile.clone(),
                lane: lane.clone(),
            })?,
        )?;
        Ok(HistoryEvent::ServerFallback {
            event: Box::new(event),
            profile: self.server_profile.clone(),
            lane,
        })
    }
    fn flush_replay(&mut self, out: &mut Vec<HistoryEvent>) -> Result<(), LlmError> {
        let mut ready = Vec::new();
        for position in std::mem::take(&mut self.pending_projection) {
            let index = wire_block_index(position)?;
            let Some(block) = self.assembly.content_at(position) else {
                self.pending_projection.insert(position);
                continue;
            };
            // Native payloads and materialized tools are already complete.
            // Text signatures must wait for their explicit block/turn boundary.
            if !self.assembly.is_terminal()
                && !self.closed.contains(&index)
                && !matches!(
                    block,
                    wire::ContentBlock::ToolUse { .. } | wire::ContentBlock::ProviderContent { .. }
                )
            {
                self.pending_projection.insert(position);
                continue;
            }
            if is_native_text_provider_content(&block) {
                // Cited text is handled at the canonical source block index:
                // recognized fields update Text, and unknown fields remain one
                // ProviderContent block. Neither needs a high-index companion.
                continue;
            }
            if needs_replay_companion(&block) {
                ready.push((index | 0x8000_0000, companion(&block, self.family)?));
            } else if let wire::ContentBlock::ProviderContent { .. } = block {
                ready.push((index | 0x8000_0000, host_block(block)?));
            }
        }
        for (index, block) in ready {
            self.start(index, block, out);
            let original = index & 0x7fff_ffff;
            if self.assembly.block_finished(original as usize)
                && self.closed.insert(original)
                && self.blocks.contains(&original)
            {
                out.push(HistoryEvent::ContentBlockStop { index: original });
            }
            if self.closed.contains(&original) && self.closed.insert(index) {
                out.push(HistoryEvent::ContentBlockStop { index });
            }
        }
        Ok(())
    }

    fn start(&mut self, index: u32, block: ContentBlock, out: &mut Vec<HistoryEvent>) {
        if !self.started {
            self.started = true;
            out.push(HistoryEvent::MessageStart {
                response: Box::new(HistoryResponse {
                    id: String::new(),
                    model: String::new(),
                    content: vec![],
                    stop_reason: None,
                    stop_details: None,
                    usage: ExecutionUsage::default(),
                    cost: None,
                    provider_metadata: self.metadata.clone(),
                    resumable: None,
                }),
            });
        }
        if self.blocks.insert(index) {
            out.push(HistoryEvent::ContentBlockStart {
                index,
                content_block: block,
            });
        }
    }
    pub(crate) fn events(
        &mut self,
        events: Vec<Result<wire::StreamEvent, wire::LlmError>>,
    ) -> Result<Vec<HistoryEvent>, LlmError> {
        let mut out = Vec::new();
        for event in events {
            let event = match event {
                Ok(event) => event,
                Err(source) => {
                    let error = error(source);
                    if self.server_lane.is_some() && !self.server_event_emitted {
                        if let Some(hop) = self.pending_server_hop.take() {
                            let event =
                                client::providers::anthropic::fallback_response::terminal_event(
                                    &self.request_model,
                                    Some(&hop),
                                    None,
                                    self.server_request_id.clone(),
                                    None,
                                )
                                .expect("pending hop");
                            out.push(self.server_event(event)?);
                        }
                    }
                    self.flush_replay(&mut out)?;
                    if out.is_empty() {
                        return Err(error);
                    }
                    self.pending_error = Some(error);
                    break;
                }
            };
            match &event {
                wire::StreamEvent::BlockEnd { block }
                | wire::StreamEvent::ProviderContent { block, .. }
                | wire::StreamEvent::ThoughtSignature { block, .. } => {
                    self.pending_projection.insert(*block);
                }
                wire::StreamEvent::ProviderEvent { protocol, payload } => {
                    if let Some(index) = stream_observation::provider_event(*protocol, payload)
                        .and_then(|event| event.block_index)
                        .and_then(|index| usize::try_from(index).ok())
                    {
                        self.pending_projection.insert(index);
                    }
                }
                wire::StreamEvent::End { .. } => self.pending_projection.extend(
                    self.blocks
                        .iter()
                        .map(|index| (index & 0x7fff_ffff) as usize),
                ),
                _ => {}
            }
            self.assembly.observe(&event);
            match event {
                wire::StreamEvent::Native { block, value } => {
                    let index = wire_block_index(block)?;
                    self.start(
                        index | 0x8000_0000,
                        native_host_block(wire::ContentBlock::Native { value }, self.family)?,
                        &mut out,
                    );
                }
                wire::StreamEvent::NativeControl { protocol, control } => {
                    if protocol != wire::ProtocolFamily::AnthropicMessages
                        || self.server_lane.is_none()
                    {
                        continue;
                    }
                    use client::providers::anthropic::fallback_response::{self, FallbackControl};
                    let Some(control) = control.decode::<FallbackControl>().ok() else {
                        continue;
                    };
                    match control {
                        FallbackControl::Start { start } => {
                            self.observe_server_fallback_model(&start.hop.model)?;
                            if let Some(response_id) = self.current_response_id.as_deref() {
                                self.assembly.set_response_id(response_id);
                            }
                            out.push(HistoryEvent::ResponseObserved {
                                model: start.hop.model.clone(),
                                response_id: self.current_response_id.clone(),
                            });
                            let completed = self.assembly.completed_blocks();
                            if completed.is_empty() {
                                self.pending_server_hop = Some(start.hop.clone());
                            } else {
                                let event = fallback_response::stream_event(
                                    &start.hop,
                                    &completed,
                                    self.server_request_id.clone(),
                                );
                                out.push(self.server_event(event)?);
                            }
                        }
                        FallbackControl::Boundary {
                            stop_reason,
                            iterations,
                            ..
                        } => {
                            let hop = self.pending_server_hop.take();
                            if !self.server_event_emitted {
                                if let Some(event) = fallback_response::terminal_event(
                                    &self.request_model,
                                    hop.as_ref(),
                                    Some(&iterations),
                                    self.server_request_id.clone(),
                                    stop_reason.clone(),
                                ) {
                                    out.push(self.server_event(event)?);
                                }
                            }
                        }
                    }
                }
                wire::StreamEvent::BlockEnd { block } => {
                    let index = wire_block_index(block)?;
                    // Record SDK completion even when the presentation block is
                    // delayed (native connectors and late tool identities).
                    let newly_closed = self.closed.insert(index);
                    if newly_closed && self.blocks.contains(&index) {
                        out.push(HistoryEvent::ContentBlockStop { index });
                    }
                    let native_index = index | 0x8000_0000;
                    if self.blocks.contains(&native_index) && self.closed.insert(native_index) {
                        out.push(HistoryEvent::ContentBlockStop {
                            index: native_index,
                        });
                    }
                }
                wire::StreamEvent::NativeDelta {
                    block,
                    protocol,
                    delta,
                } => {
                    if let Some(delta) = stream_observation::native_delta(protocol, &delta) {
                        let delta = match delta {
                            NativeDelta::Citation(citation) => {
                                HistoryContentDelta::CitationsDelta { citation }
                            }
                            NativeDelta::ConnectorText {
                                utf16_code_units, ..
                            } => HistoryContentDelta::ConnectorTextDelta {
                                connector_text: wire::JsonPayloadFragment::from_utf16_code_units(
                                    utf16_code_units,
                                ),
                            },
                        };
                        if self.blocks.contains(&(wire_block_index(block)?)) {
                            out.push(HistoryEvent::ContentBlockDelta {
                                index: wire_block_index(block)?,
                                delta,
                            });
                        }
                    }
                }
                wire::StreamEvent::Start { model, response_id } => {
                    let response_id = response_id.map(|id| id.as_str().to_owned());
                    self.current_response_id = response_id.clone();
                    if self.started {
                        continue;
                    }
                    self.started = true;
                    out.push(HistoryEvent::MessageStart {
                        response: Box::new(HistoryResponse {
                            id: response_id.unwrap_or_default(),
                            model,
                            content: vec![],
                            stop_reason: None,
                            stop_details: None,
                            usage: usage(&self.observation.0, &self.observation.1)
                                .map(|(u, _)| u)
                                .unwrap_or_default(),
                            cost: None,
                            provider_metadata: self.metadata.clone(),
                            resumable: None,
                        }),
                    });
                }
                wire::StreamEvent::TextDelta { block, text } => {
                    let index = wire_block_index(block)?;
                    self.start(
                        index,
                        ContentBlock::Text {
                            text: String::new(),
                            citations: None,
                            cache_control: None,
                        },
                        &mut out,
                    );
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::TextDelta { text },
                    });
                }
                wire::StreamEvent::TextDeltaJsUtf16 {
                    block,
                    text,
                    utf16_code_units,
                } => {
                    let index = wire_block_index(block)?;
                    self.start(
                        index,
                        ContentBlock::Text {
                            text: String::new(),
                            citations: None,
                            cache_control: None,
                        },
                        &mut out,
                    );
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::TextJsUtf16Delta {
                            text,
                            utf16_code_units,
                        },
                    });
                }
                wire::StreamEvent::ReasoningDelta { block, text } => {
                    let index = wire_block_index(block)?;
                    self.reasoning_blocks.insert(index);
                    self.start(
                        index,
                        ContentBlock::Reasoning {
                            text: String::new(),
                            signature: None,
                        },
                        &mut out,
                    );
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::ThinkingDelta { thinking: text },
                    });
                }
                wire::StreamEvent::ThoughtSignature { block, signature } => {
                    let index = wire_block_index(block)?;
                    if self.blocks.contains(&index) && !self.reasoning_blocks.contains(&index) {
                        continue;
                    }
                    self.reasoning_blocks.insert(index);
                    self.start(
                        wire_block_index(block)?,
                        ContentBlock::Reasoning {
                            text: String::new(),
                            signature: None,
                        },
                        &mut out,
                    );
                    out.push(HistoryEvent::ContentBlockDelta {
                        index: wire_block_index(block)?,
                        delta: HistoryContentDelta::SignatureDelta { signature },
                    });
                }
                wire::StreamEvent::RedactedThinking { block, data } => self.start(
                    wire_block_index(block)?,
                    ContentBlock::RedactedThinking { data },
                    &mut out,
                ),
                wire::StreamEvent::ToolCallDelta {
                    block,
                    id,
                    name,
                    arguments_fragment,
                    provider_id: _,
                    caller: _,
                    toolset_name: _,
                } => {
                    let index = wire_block_index(block)?;
                    let (id, name, arguments_fragment) = if self.blocks.contains(&index) {
                        (id, name, arguments_fragment)
                    } else {
                        let Some(wire::StreamEvent::ToolCallDelta {
                            id,
                            name,
                            arguments_fragment,
                            ..
                        }) = self.assembly.tool_progress(block)
                        else {
                            continue;
                        };
                        if id.as_str().is_empty() || name.is_empty() {
                            continue;
                        }
                        (id, name, arguments_fragment)
                    };
                    self.start(
                        index,
                        ContentBlock::ToolCall {
                            id: id.as_str().into(),
                            name,
                            input: wire::JsonPayload::parse("{}")
                                .expect("empty object is valid JSON"),
                        },
                        &mut out,
                    );
                    if !arguments_fragment.is_empty() {
                        out.push(HistoryEvent::ContentBlockDelta {
                            index,
                            delta: HistoryContentDelta::InputJsonDelta {
                                partial_json: arguments_fragment,
                            },
                        });
                    }
                }
                wire::StreamEvent::ProviderContent {
                    block,
                    protocol,
                    value,
                } => {
                    let index = wire_block_index(block)?;
                    if protocol == wire::ProtocolFamily::AnthropicMessages {
                        if let Some(TextBlock::Known {
                            text,
                            utf16_code_units,
                            citations,
                        }) = stream_observation::text_block(&value)
                        {
                            if !self.blocks.contains(&index) {
                                let is_lossy = String::from_utf16(&utf16_code_units).is_err();
                                let block = if is_lossy {
                                    ContentBlock::TextJsUtf16 {
                                        text: String::new(),
                                        utf16_code_units: Vec::new(),
                                        citations: citations.clone(),
                                        cache_control: None,
                                    }
                                } else {
                                    ContentBlock::Text {
                                        text: String::new(),
                                        citations: citations.clone(),
                                        cache_control: None,
                                    }
                                };
                                self.start(index, block, &mut out);
                                if !utf16_code_units.is_empty() {
                                    out.push(HistoryEvent::ContentBlockDelta {
                                        index,
                                        delta: if is_lossy {
                                            HistoryContentDelta::TextJsUtf16Delta {
                                                text,
                                                utf16_code_units,
                                            }
                                        } else {
                                            HistoryContentDelta::TextDelta { text }
                                        },
                                    });
                                }
                            }
                            out.push(HistoryEvent::ContentBlockDelta {
                                index,
                                delta: HistoryContentDelta::TextCitations {
                                    citations: citations.clone(),
                                },
                            });
                            self.opaque_text_blocks.remove(&index);
                            continue;
                        }
                        if value.string_at("/type").as_deref() == Some("text") {
                            // A typed Text carrier cannot hold these unknown
                            // provider fields, so keep their complete source
                            // block at its original position. TextDelta events
                            // have already streamed into this same block.
                            let native_value = value.clone();
                            let native = wire::ContentBlock::ProviderContent { protocol, value };
                            if self.opaque_text_blocks.remove(&index)
                                && self.blocks.contains(&index)
                            {
                                out.push(HistoryEvent::ContentBlockDelta {
                                    index,
                                    delta: HistoryContentDelta::ProviderContentSnapshot {
                                        value: native_value,
                                    },
                                });
                            } else {
                                self.start(index, host_block(native)?, &mut out);
                            }
                            continue;
                        }
                    }
                    let native = wire::ContentBlock::ProviderContent { protocol, value };
                    self.start(index | 0x8000_0000, host_block(native)?, &mut out);
                }
                wire::StreamEvent::End {
                    stop_reason,
                    usage: report,
                    inference,
                } => {
                    if !self.done {
                        if let Some(model) = self.server_response_model.as_deref() {
                            set_upstream_metadata_field(
                                &mut self.metadata,
                                "response_model",
                                &wire::JsonPayload::string(model),
                            )?;
                        }
                        self.flush_replay(&mut out)?;
                        if (usage(&report, &inference).is_none() || self.force_observation_snapshot)
                            && self.metadata.as_json_source().trim() != "null"
                        {
                            // Observations can exist without billable token measurements.
                            // Preserve them in a host-only transcript companion rather
                            // than fabricating a zero-usage report. Replay filters this tag.
                            // Raw provider observations cannot retain this
                            // namespace; it identifies our own companion.
                            normalize_upstream_metadata(&mut self.metadata)?;
                            let index = (0..=u32::MAX)
                                .rev()
                                .find(|index| !self.blocks.contains(index))
                                .ok_or_else(|| {
                                    invalid("no stream index available for provider observations")
                                })?;
                            let protocol = serde_json::to_value(self.family)
                                .map_err(invalid)?
                                .as_str()
                                .unwrap()
                                .to_owned();
                            let value = wire::JsonPayload::parse(&format!(
                                r#"{{"type":"lingxi_observation","metadata":{}}}"#,
                                self.metadata.as_json_source()
                            ))
                            .map_err(invalid)?;
                            self.start(
                                index,
                                ContentBlock::ProviderContent { protocol, value },
                                &mut out,
                            );
                            self.force_observation_snapshot = false;
                        }
                        self.done = true;
                        for index in self.blocks.difference(&self.closed) {
                            out.push(HistoryEvent::ContentBlockStop { index: *index });
                        }
                        let mut final_usage = usage(&report, &inference);
                        let usage_is_final =
                            final_usage.as_ref().is_some_and(|(_, completeness)| {
                                *completeness == ModelAttemptUsageCompleteness::Complete
                            });
                        if self.metadata.as_json_source().trim() != "null" {
                            if let Some((usage, _)) = final_usage.as_mut() {
                                set_root_metadata_field(
                                    &mut usage.provider_metadata,
                                    "stream",
                                    &self.metadata,
                                )?;
                            }
                        }
                        if !self.stop_delta_seen || usage_is_final {
                            out.push(HistoryEvent::MessageDelta {
                                delta: HistoryMessageDelta {
                                    stop_reason: (!self.stop_delta_seen).then(|| stop(stop_reason)),
                                    stop_details: self
                                        .metadata
                                        .payload_at("/stop_details")
                                        .filter(|value| value.as_json_source().trim() != "null")
                                        .map(|value| stop_details(&value))
                                        .transpose()?,
                                    resumable: self.resumable.clone(),
                                },
                                usage: final_usage.map(|(usage, _)| usage),
                            });
                        }
                        out.push(HistoryEvent::MessageStop);
                    }
                }
                wire::StreamEvent::ProviderEvent { protocol, payload } => {
                    if let Some(observation) =
                        stream_observation::provider_event(protocol, &payload)
                    {
                        if let (Some(raw_index), Some(text_start)) =
                            (observation.block_index, observation.text_start)
                        {
                            let index =
                                wire_block_index(usize::try_from(raw_index).map_err(invalid)?)?;
                            match text_start {
                                TextBlock::Opaque(block) => {
                                    self.opaque_text_blocks.insert(index);
                                    let visible_text = wire::JsonPayload::string("");
                                    let visible_start = block
                                        .with_object_field("text", &visible_text)
                                        .map_err(invalid)?;
                                    self.start(
                                        index,
                                        host_block(wire::ContentBlock::ProviderContent {
                                            protocol: wire::ProtocolFamily::AnthropicMessages,
                                            value: visible_start,
                                        })?,
                                        &mut out,
                                    );
                                }
                                TextBlock::Known {
                                    citations: Some(citations),
                                    utf16_code_units,
                                    ..
                                } => {
                                    let block = if String::from_utf16(&utf16_code_units).is_err() {
                                        ContentBlock::TextJsUtf16 {
                                            text: String::new(),
                                            utf16_code_units: Vec::new(),
                                            citations: Some(citations),
                                            cache_control: None,
                                        }
                                    } else {
                                        ContentBlock::Text {
                                            text: String::new(),
                                            citations: Some(citations),
                                            cache_control: None,
                                        }
                                    };
                                    self.start(index, block, &mut out);
                                }
                                TextBlock::Known { .. } => {}
                            }
                        }
                        if let Some(stop) = observation.stop_delta {
                            self.stop_delta_seen = true;
                            let mut observed_usage =
                                usage(&self.observation.0, &self.observation.1);
                            if self.metadata.as_json_source().trim() != "null" {
                                if let Some((usage, _)) = observed_usage.as_mut() {
                                    set_root_metadata_field(
                                        &mut usage.provider_metadata,
                                        "stream",
                                        &self.metadata,
                                    )?;
                                }
                            }
                            out.push(HistoryEvent::MessageDelta {
                                delta: HistoryMessageDelta {
                                    stop_reason: Some(stop.reason.into()),
                                    stop_details: stop
                                        .details
                                        .map(|value| {
                                            serde_json::from_str(value.as_json_source())
                                                .map_err(invalid)
                                        })
                                        .transpose()?,
                                    resumable: self.resumable.clone(),
                                },
                                usage: observed_usage.map(|(usage, _)| usage),
                            });
                        }
                    }
                    let event_source = format!(
                        "{{\"protocol\":{},\"payload\":{}}}",
                        serde_json::to_string(&protocol).map_err(invalid)?,
                        payload.as_json_source()
                    );
                    let event = wire::JsonPayload::parse(&event_source).map_err(invalid)?;
                    append_observation(&mut self.metadata, "provider_events", event)?;
                }
                wire::StreamEvent::WebSearch { result } => {
                    out.push(HistoryEvent::WebSearch {
                        result: result.clone(),
                    });
                    append_observation(
                        &mut self.metadata,
                        "web_search",
                        serialized_payload(&result)?,
                    )?;
                }
                wire::StreamEvent::FileSearch { result } => {
                    append_observation(
                        &mut self.metadata,
                        "file_search",
                        serialized_payload(&result)?,
                    )?;
                }
                wire::StreamEvent::Inference { .. } => {}
            }
        }
        if !self.done {
            self.flush_replay(&mut out)?;
        }
        Ok(out)
    }
}

pub(crate) fn project_response(
    decoded: wire::ChatResponse,
    mut response: ProviderResponse,
    protocol: wire::ProtocolFamily,
) -> Result<HistoryResponse, LlmError> {
    clear_host_projection_fields(&mut response.body_json)?;
    project_model_response(decoded, protocol, response.body_json, response.request_id)
}

fn clear_host_projection_fields(metadata: &mut wire::JsonPayload) -> Result<(), LlmError> {
    for field in [
        "server_fallback_events",
        "per_turn_effort",
        "response_model",
        crate::history::SERVER_FALLBACK_COST_QUOTE_KEY,
    ] {
        remove_upstream_metadata_field(metadata, field)?;
    }
    Ok(())
}

pub(crate) fn project_model_response(
    decoded: wire::ChatResponse,
    protocol: wire::ProtocolFamily,
    mut metadata: wire::JsonPayload,
    request_id: Option<String>,
) -> Result<HistoryResponse, LlmError> {
    clear_computer_binding(&mut metadata)?;
    clear_host_projection_fields(&mut metadata)?;
    // A collected fallback response has the same request-local identity as a
    // streamed response. A body-supplied namespace cannot author this value.
    remove_upstream_metadata_field(&mut metadata, "http_request_id")?;
    if let Some(request_id) = request_id.as_ref() {
        set_upstream_metadata_field(
            &mut metadata,
            "http_request_id",
            &serialized_payload(request_id)?,
        )?;
    }
    set_upstream_metadata_field(
        &mut metadata,
        "non_streaming_response",
        &serialized_payload(&true)?,
    )?;
    if !decoded.tool_input_issues.is_empty() {
        set_upstream_metadata_field(
            &mut metadata,
            "tool_input_issues",
            &serialized_payload(&decoded.tool_input_issues)?,
        )?;
    }
    let native_stop_details = decoded
        .anthropic_stop_details()
        .map(stop_details)
        .transpose()?;
    let resumable = decoded.anthropic_resumable().cloned();
    if let Some(fallback) = decoded.anthropic_fallback() {
        set_upstream_metadata_field(
            &mut metadata,
            "anthropic_fallback",
            &serialized_payload(fallback)?,
        )?;
    }
    let normalized = usage(&decoded.usage, &decoded.inference)
        .map(|(u, _)| u)
        .unwrap_or_default();
    let mut content = Vec::new();
    for block in decoded.message.content {
        let replay = if needs_replay_companion(&block) {
            Some(companion(&block, protocol)?)
        } else {
            None
        };
        content.push(native_host_block(block, protocol)?);
        content.extend(replay);
    }
    for (key, value) in [
        ("web_search", serialized_payload(&decoded.web_search)?),
        ("file_search", serialized_payload(&decoded.file_search)?),
        (
            "native_metadata",
            serialized_payload(
                &(!decoded.native_metadata.is_empty()).then_some(&decoded.native_metadata),
            )?,
        ),
        (
            "response_cache",
            serialized_payload(&decoded.response_cache)?,
        ),
        ("continuation", serialized_payload(&decoded.continuation)?),
        (
            "executed_profile",
            serialized_payload(&decoded.executed_profile)?,
        ),
        (
            "inference",
            serialized_payload(
                &(decoded.inference != wire::InferenceReport::default())
                    .then_some(&decoded.inference),
            )?,
        ),
    ] {
        if value.as_json_source().trim() != "null" {
            set_upstream_metadata_field(&mut metadata, key, &value)?;
        }
    }
    let metadata_stop_details = metadata
        .payload_at("/stop_details")
        .filter(|value| value.as_json_source().trim() != "null")
        .map(|value| stop_details(&value))
        .transpose()?;
    Ok(HistoryResponse {
        id: decoded
            .response_id
            .map(|id| id.as_str().to_owned())
            .or(request_id)
            .unwrap_or_else(|| metadata.string_at("/id").unwrap_or_default()),
        model: decoded.model,
        content,
        stop_reason: Some(stop(decoded.stop_reason)),
        stop_details: metadata_stop_details.or(native_stop_details),
        usage: normalized,
        cost: None,
        provider_metadata: metadata,
        resumable,
    })
}

impl HistoryProjector {
    pub(crate) fn observe_model_metadata(
        &mut self,
        stream: &client::ModelStream,
    ) -> Result<(), LlmError> {
        if let Some(continuation) = stream.continuation() {
            set_upstream_metadata_field(
                &mut self.metadata,
                "continuation",
                &serialized_payload(continuation)?,
            )?;
        }
        if let Some(fallback) = stream.anthropic_fallback() {
            set_upstream_metadata_field(
                &mut self.metadata,
                "anthropic_fallback",
                &serialized_payload(fallback)?,
            )?;
        }
        if let Some(details) = stream.anthropic_stop_details() {
            set_root_metadata_field(&mut self.metadata, "stop_details", details)?;
        }
        if let Some(resumable) = stream.anthropic_resumable() {
            self.resumable = Some(resumable.clone());
        }
        Ok(())
    }
    pub(crate) fn observed_usage(&self) -> Option<(ExecutionUsage, ModelAttemptUsageCompleteness)> {
        usage(&self.observation.0, &self.observation.1)
    }
    pub(crate) fn take_error(&mut self) -> Option<LlmError> {
        self.pending_error.take()
    }
    pub(crate) fn finish(&mut self) -> Result<Vec<HistoryEvent>, LlmError> {
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        Ok(Vec::new())
    }
    pub(crate) fn projection(
        family: wire::ProtocolFamily,
        mut metadata: wire::JsonPayload,
    ) -> Result<Self, LlmError> {
        clear_computer_binding(&mut metadata)?;
        clear_host_projection_fields(&mut metadata)?;
        Ok(Self {
            server_lane: None,
            server_profile: String::new(),
            request_model: String::new(),
            server_request_id: None,
            server_response_model: None,
            pending_server_hop: None,
            server_event_emitted: false,
            current_response_id: None,
            observation: Default::default(),
            family,
            blocks: BTreeSet::new(),
            closed: BTreeSet::new(),
            reasoning_blocks: BTreeSet::new(),
            opaque_text_blocks: BTreeSet::new(),
            metadata,
            resumable: None,
            force_observation_snapshot: false,
            done: false,
            started: false,
            stop_delta_seen: false,
            assembly: client::stream_assembly::StreamAccumulator::new(),
            pending_projection: BTreeSet::new(),
            pending_error: None,
        })
    }
    pub(crate) fn project_batch(
        &mut self,
        batch: client::StreamBatch,
    ) -> Result<Vec<HistoryEvent>, LlmError> {
        self.observation = (batch.usage.clone(), batch.inference.clone());
        let result = self.events(batch.events);
        // Empty and failing batches can still contain billing observations.
        self.assembly
            .observe_batch(&client::StreamBatch {
                events: Vec::new(),
                usage: batch.usage,
                inference: batch.inference,
                finished: batch.finished,
            })
            .map_err(error)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn source(value: &Value) -> wire::JsonPayload {
        wire::JsonPayload::from_generated_value(value).expect("test JSON is source-backed")
    }

    #[test]
    fn typed_native_response_and_stream_remain_lossless_at_host_boundary() {
        let native = wire::ContentBlock::Native {
            value: wire::NativeExtension::new(
                "openai.responses.computer_call.v1",
                source(&json!({
                    "id": "item",
                    "call_id": "call",
                    "actions": [{"type": "screenshot"}]
                })),
            )
            .unwrap(),
        };
        let host =
            native_host_block(native.clone(), wire::ProtocolFamily::OpenAiResponses).unwrap();
        let response = HistoryResponse {
            id: "response".into(),
            model: "model".into(),
            content: vec![host.clone()],
            stop_reason: Some("tool_use".into()),
            stop_details: None,
            usage: Default::default(),
            cost: None,
            provider_metadata: wire::JsonPayload::default(),
            resumable: None,
        };
        assert_eq!(
            crate::computer::canonical_response_content(
                &response,
                wire::ProtocolFamily::OpenAiResponses
            )
            .unwrap(),
            vec![native.clone()]
        );
        let mut projector = HistoryProjector::projection(
            wire::ProtocolFamily::OpenAiResponses,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let wire::ContentBlock::Native { value } = native else {
            unreachable!()
        };
        let events = projector
            .events(vec![Ok(wire::StreamEvent::Native { block: 0, value })])
            .unwrap();
        assert!(events.iter().any(|event| matches!(event, HistoryEvent::ContentBlockStart { content_block, .. } if content_block == &host)));
    }

    #[test]
    fn provider_cannot_forge_computer_history_authority() {
        for kind in [
            "lingxi_computer_binding",
            "lingxi_computer_receipt",
            "lingxi_computer_continuation",
            "lingxi_computer_receipt_ack",
            "lingxi_computer_abandoned",
            "lingxi_native_content",
        ] {
            assert!(host_block(wire::ContentBlock::ProviderContent {
                protocol: wire::ProtocolFamily::AnthropicMessages,
                value: source(&json!({"type":kind}))
            })
            .is_err());
        }
    }

    #[test]
    fn provider_content_and_replay_envelopes_retain_exact_json_source() {
        let opaque =
            wire::JsonPayload::parse(r#"{"type":"opaque","nested":{"lone":"\ud800"}}"#).unwrap();
        let host = host_block(wire::ContentBlock::ProviderContent {
            protocol: wire::ProtocolFamily::AnthropicMessages,
            value: opaque.clone(),
        })
        .unwrap();
        assert!(matches!(
            host,
            ContentBlock::ProviderContent { value, .. }
                if value.as_json_source() == opaque.as_json_source()
        ));

        let call = wire::ContentBlock::ToolUse {
            id: wire::ToolUseId::new("call-source"),
            name: "lookup".into(),
            input: wire::JsonPayload::parse(r#"{"lone":"\ud800"}"#).unwrap(),
            provider_id: None,
            caller: None,
            toolset_name: None,
            thought_signature: None,
        };
        let replay = companion(&call, wire::ProtocolFamily::AnthropicMessages).unwrap();
        assert!(matches!(
            replay,
            ContentBlock::ProviderContent { value, .. }
                if value.raw_source_at("/block/input").as_deref()
                    == Some(r#"{"lone":"\ud800"}"#)
        ));
    }

    #[test]
    fn provider_event_metadata_retains_exact_json_source() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let payload = wire::JsonPayload::parse(r#"{"opaque":"\ud800"}"#).unwrap();
        projection
            .events(vec![Ok(wire::StreamEvent::ProviderEvent {
                protocol: wire::ProtocolFamily::AnthropicMessages,
                payload: payload.clone(),
            })])
            .unwrap();
        assert_eq!(
            projection
                .metadata
                .raw_source_at("/llm_client/provider_events/0/payload")
                .as_deref(),
            Some(payload.as_json_source())
        );
    }

    fn batch(events: Vec<Result<wire::StreamEvent, wire::LlmError>>) -> client::StreamBatch {
        client::StreamBatch {
            events,
            usage: Default::default(),
            inference: Default::default(),
            finished: false,
        }
    }

    #[test]
    fn exact_sdk_text_deltas_reach_the_host_utf16_delta_carrier() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let events = projection
            .events(vec![
                Ok(wire::StreamEvent::TextDeltaJsUtf16 {
                    block: 0,
                    text: "�".into(),
                    utf16_code_units: vec![0xd800],
                }),
                Ok(wire::StreamEvent::BlockEnd { block: 0 }),
            ])
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            HistoryEvent::ContentBlockDelta {
                delta: HistoryContentDelta::TextJsUtf16Delta { text, utf16_code_units },
                ..
            } if text == "�" && utf16_code_units == &[0xd800]
        )));
    }

    #[test]
    fn completed_sdk_text_carrier_maps_to_host_durable_text_carrier() {
        let projected = host_block(wire::ContentBlock::TextJsUtf16 {
            text: "A�B".into(),
            utf16_code_units: vec![0x41, 0xd800, 0x42],
            thought_signature: None,
            citations: Some(None),
        })
        .unwrap();
        assert!(matches!(
            projected,
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations: Some(None),
                ..
            } if text == "A�B" && utf16_code_units == vec![0x41, 0xd800, 0x42]
        ));
    }

    fn fallback_start(model: &str) -> wire::StreamEvent {
        use client::providers::anthropic::fallback_response::{
            FallbackControl, FallbackHop, FallbackStart,
        };
        wire::StreamEvent::NativeControl {
            protocol: wire::ProtocolFamily::AnthropicMessages,
            control: wire::NativeExtension::from_typed(FallbackControl::Start {
                start: FallbackStart {
                    index: serde_json::Number::from(0),
                    hop: FallbackHop {
                        from_model: "primary".into(),
                        model: model.into(),
                        reason: "refusal".into(),
                        category: Some("cyber".into()),
                    },
                },
            })
            .unwrap(),
        }
    }

    #[test]
    fn every_admitted_fallback_start_emits_row_identity_without_a_controller_event() {
        use client::providers::anthropic::fallback_request::{LaneMode, ServerLane};

        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        projection
            .admit_server_fallback(
                Some(ServerLane {
                    for_model: "primary".into(),
                    model: "fallback".into(),
                    mode: LaneMode::Explicit,
                }),
                "first_party".into(),
                "primary".into(),
                None,
            )
            .unwrap();

        let events = projection
            .events(vec![
                Ok(wire::StreamEvent::Start {
                    model: "primary".into(),
                    response_id: Some(wire::ResponseId::new("resp-inner")),
                }),
                Ok(fallback_start("fallback-one")),
                Ok(wire::StreamEvent::Start {
                    model: "fallback-one".into(),
                    response_id: Some(wire::ResponseId::new("resp-inner-two")),
                }),
                Ok(fallback_start("fallback-two")),
            ])
            .unwrap();

        let observed = events
            .iter()
            .filter_map(|event| match event {
                HistoryEvent::ResponseObserved { model, response_id } => {
                    Some((model.as_str(), response_id.as_deref()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                ("fallback-one", Some("resp-inner")),
                ("fallback-two", Some("resp-inner-two")),
            ]
        );
        let response = projection.assembly.snapshot().response;
        assert_eq!(response.model, "fallback-two");
        assert_eq!(
            response.response_id.as_ref().map(|id| id.as_str()),
            Some("resp-inner-two")
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, HistoryEvent::ServerFallback { .. })));
    }

    #[test]
    fn sdk_assembles_signed_tool_replay_at_the_history_edge() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::GeminiGenerateContent,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let events = projection
            .project_batch(batch(vec![
                Ok(wire::StreamEvent::ToolCallDelta {
                    block: 7,
                    id: wire::ToolUseId::new("call"),
                    provider_id: Some("native-call".into()),
                    caller: None,
                    toolset_name: Some("browser".into()),
                    name: "lookup".into(),
                    arguments_fragment: "{\"q\":".into(),
                }),
                Ok(wire::StreamEvent::ThoughtSignature {
                    block: 7,
                    signature: "signed".into(),
                }),
                Ok(wire::StreamEvent::ToolCallDelta {
                    block: 7,
                    id: wire::ToolUseId::new("call"),
                    provider_id: None,
                    caller: None,
                    toolset_name: None,
                    name: "lookup".into(),
                    arguments_fragment: "\"rust\"}".into(),
                }),
                Ok(wire::StreamEvent::BlockEnd { block: 7 }),
            ]))
            .unwrap();
        assert!(!events.iter().any(|event| matches!(
            event,
            HistoryEvent::ContentBlockDelta {
                delta: HistoryContentDelta::SignatureDelta { .. },
                ..
            }
        )));
        let native = events
            .iter()
            .find_map(|event| match event {
                HistoryEvent::ContentBlockStart {
                    content_block: ContentBlock::ProviderContent { value, .. },
                    ..
                } => value.payload_at("/block"),
                _ => None,
            })
            .expect("durable replay metadata");
        assert_eq!(
            native.raw_source_at("/input").as_deref(),
            Some(r#"{"q":"rust"}"#)
        );
        assert_eq!(
            native.string_at("/thought_signature").as_deref(),
            Some("signed")
        );
        assert_eq!(
            native.string_at("/provider_id").as_deref(),
            Some("native-call")
        );
        assert_eq!(
            native.string_at("/toolset_name").as_deref(),
            Some("browser")
        );
    }

    #[test]
    fn successful_events_in_a_failing_batch_are_presented_before_error() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::OpenAiChat,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let events = projection
            .project_batch(batch(vec![
                Ok(wire::StreamEvent::TextDelta {
                    block: 0,
                    text: "partial".into(),
                }),
                Err(wire::LlmError::StreamInterrupted {
                    message: "connection reset".into(),
                }),
            ]))
            .unwrap();
        assert!(events.iter().any(|event| matches!(event, HistoryEvent::ContentBlockDelta { delta: HistoryContentDelta::TextDelta { text }, .. } if text == "partial")));
        assert!(matches!(
            projection.take_error(),
            Some(LlmError::StreamInterrupted { .. })
        ));
    }

    #[tokio::test]
    async fn deferred_tool_identity_and_early_arguments_reach_history_together() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::OpenAiChat,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let events = projection
            .project_batch(batch(vec![
                Ok(wire::StreamEvent::ToolCallDelta {
                    block: 3,
                    id: wire::ToolUseId::new(""),
                    provider_id: None,
                    caller: None,
                    toolset_name: None,
                    name: String::new(),
                    arguments_fragment: "{\"q\":".into(),
                }),
                Ok(wire::StreamEvent::ToolCallDelta {
                    block: 3,
                    id: wire::ToolUseId::new("call-real"),
                    provider_id: Some("call-real".into()),
                    caller: None,
                    toolset_name: None,
                    name: "lookup".into(),
                    arguments_fragment: "\"answer\"}".into(),
                }),
                Ok(wire::StreamEvent::BlockEnd { block: 3 }),
                Ok(wire::StreamEvent::End {
                    stop_reason: wire::StopReason::ToolUse,
                    usage: Default::default(),
                    inference: Default::default(),
                }),
            ]))
            .unwrap();
        let response = crate::stream_accumulator::accumulate_stream_salvaging(Box::pin(
            futures::stream::iter(events.into_iter().map(Ok)),
        ))
        .await
        .unwrap();
        assert!(response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall { id, name, input } if id == "call-real" && name == "lookup" && input.string_at("/q").as_deref() == Some("answer"))));
        assert!(!response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall { id, name, .. } if id.is_empty() || name.is_empty())));
    }

    #[tokio::test]
    async fn completed_connector_is_salvaged_when_next_frame_fails() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let mut events = Vec::new();
        for payload in [
            json!({"type":"content_block_start","index":4,"content_block":{"type":"connector_text","connector_text":"hello","signature":"signed"}}),
            json!({"type":"content_block_delta","index":4,"delta":{"type":"connector_text_delta","connector_text":" world"}}),
            json!({"type":"content_block_stop","index":4}),
        ] {
            events.extend(
                projection
                    .project_batch(batch(vec![Ok(wire::StreamEvent::ProviderEvent {
                        protocol: wire::ProtocolFamily::AnthropicMessages,
                        payload: source(&payload),
                    })]))
                    .unwrap()
                    .into_iter()
                    .map(Ok),
            );
        }
        events.push(Err(LlmError::StreamInterrupted {
            message: "lost connection".into(),
        }));
        let (partial, _) = crate::stream_accumulator::accumulate_stream_salvaging(Box::pin(
            futures::stream::iter(events),
        ))
        .await
        .unwrap_err();
        assert!(partial.iter().any(|block| matches!(block, ContentBlock::ConnectorText { connector_text, signature } if connector_text == "hello world" && signature.as_deref() == Some("signed"))));
    }

    #[tokio::test]
    async fn connector_close_and_failure_in_one_batch_preserve_completed_content() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let mut events = Vec::new();
        for payload in [
            json!({"type":"content_block_start","index":2,"content_block":{"type":"connector_text","connector_text":"saved"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"connector_text_delta","connector_text":" result"}}),
        ] {
            events.extend(
                projection
                    .project_batch(batch(vec![Ok(wire::StreamEvent::ProviderEvent {
                        protocol: wire::ProtocolFamily::AnthropicMessages,
                        payload: source(&payload),
                    })]))
                    .unwrap()
                    .into_iter()
                    .map(Ok),
            );
        }
        events.extend(
            projection
                .project_batch(batch(vec![
                    Ok(wire::StreamEvent::ProviderEvent {
                        protocol: wire::ProtocolFamily::AnthropicMessages,
                        payload: source(&json!({"type":"content_block_stop","index":2})),
                    }),
                    Ok(wire::StreamEvent::BlockEnd { block: 2 }),
                    Err(wire::LlmError::StreamInterrupted {
                        message: "reset".into(),
                    }),
                ]))
                .unwrap()
                .into_iter()
                .map(Ok),
        );
        events.push(Err(projection.take_error().expect("deferred stream error")));
        let (partial, _) = crate::stream_accumulator::accumulate_stream_salvaging(Box::pin(
            futures::stream::iter(events),
        ))
        .await
        .unwrap_err();
        assert!(partial.iter().any(|block| matches!(block, ContentBlock::ConnectorText { connector_text, .. } if connector_text == "saved result")));
    }

    #[test]
    fn empty_batches_preserve_partial_accounting() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let mut report = batch(Vec::new());
        report.usage = wire::UsageReport::measured(
            wire::Usage {
                input_tokens: 9,
                ..Default::default()
            },
            wire::UsageState::Partial,
        );
        assert!(projection.project_batch(report).unwrap().is_empty());
        let (usage, completeness) = projection.observed_usage().expect("usage");
        assert_eq!(usage.counts().input_tokens, 9);
        assert_eq!(completeness, ModelAttemptUsageCompleteness::Partial);
        assert_eq!(
            projection
                .assembly
                .snapshot()
                .response
                .usage
                .usage
                .unwrap()
                .input_tokens,
            9
        );
    }
}
#[cfg(test)]
mod stop_delta_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_terminal_delta_reaches_host_once_before_stop() {
        let mut projection = HistoryProjector::projection(
            wire::ProtocolFamily::AnthropicMessages,
            wire::JsonPayload::default(),
        )
        .unwrap();
        let frame = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}});
        let events = projection
            .events(vec![Ok(wire::StreamEvent::ProviderEvent {
                protocol: wire::ProtocolFamily::AnthropicMessages,
                payload: wire::JsonPayload::from_generated_value(&frame).unwrap(),
            })])
            .unwrap();
        assert!(
            matches!(&events[..],[HistoryEvent::MessageDelta {delta,..}] if delta.stop_reason.as_deref()==Some("end_turn"))
        );
        let end = projection
            .events(vec![Ok(wire::StreamEvent::End {
                stop_reason: wire::StopReason::EndTurn,
                usage: Default::default(),
                inference: Default::default(),
            })])
            .unwrap();
        assert!(!end
            .iter()
            .any(|event| matches!(event, HistoryEvent::MessageDelta { .. })));
        assert!(end
            .iter()
            .any(|event| matches!(event, HistoryEvent::MessageStop)));
    }
}

pub(crate) fn attach_output_budget(
    metadata: &mut wire::JsonPayload,
    observation: Value,
) -> Result<(), LlmError> {
    set_upstream_metadata_field(
        metadata,
        "output_budget",
        &serialized_payload(&observation)?,
    )
}
