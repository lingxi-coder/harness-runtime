//! Presentation-only projection from canonical SDK responses and streams.
//! Provider block assembly is owned by the SDK; history serialization remains
//! stable at this host boundary. Replay companions exist only in that durable
//! history representation: SDK execution and block assembly never use them.
//! History input conversion consumes those companions once before SDK dispatch.
use crate::upstream::{error, usage};
use crate::*;
use lingxi_llm_client::{self as client, protocol as wire};
use std::collections::BTreeSet;

fn invalid(error: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest {
        message: error.to_string(),
    }
}
pub(crate) use client::replay::{has_replay_metadata, native_cited_text};
fn companion(
    block: &wire::ContentBlock,
    family: wire::ProtocolFamily,
) -> Result<ContentBlock, LlmError> {
    Ok(ContentBlock::ProviderContent {
        protocol: serde_json::to_value(family)
            .map_err(invalid)?
            .as_str()
            .unwrap()
            .into(),
        value: json!({"type":"lingxi_replay_metadata", "block":serde_json::to_value(block).map_err(invalid)?}),
    })
}

fn upstream_metadata(metadata: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !metadata.is_object() {
        *metadata = if metadata.is_null() {
            json!({})
        } else {
            json!({"native": metadata.take()})
        };
    }
    let namespace = metadata
        .as_object_mut()
        .expect("metadata object")
        .entry("llm_client")
        .or_insert_with(|| json!({}));
    if !namespace.is_object() {
        *namespace = json!({"native": namespace.take()});
    }
    namespace.as_object_mut().expect("upstream metadata object")
}

fn append_observation(metadata: &mut Value, key: &str, value: Value) {
    let entries = upstream_metadata(metadata)
        .entry(key)
        .or_insert_with(|| json!([]));
    entries
        .as_array_mut()
        .expect("observation array")
        .push(value);
}

fn host_block(block: wire::ContentBlock) -> Result<ContentBlock, LlmError> {
    Ok(match block {
        wire::ContentBlock::Text { text, .. } => ContentBlock::Text {
            text,
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
            if matches!(
                value["type"].as_str(),
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
            clear_provider_observation_metadata(&mut value);
            if protocol == wire::ProtocolFamily::AnthropicMessages
                && matches!(
                    value["type"].as_str(),
                    Some("server_tool_use" | "connector_text" | "advisor_tool_result")
                )
                && value.as_object().is_some_and(|object| {
                    object.keys().all(|key| match value["type"].as_str() {
                        Some("server_tool_use") => {
                            matches!(key.as_str(), "type" | "id" | "name" | "input")
                        }
                        Some("connector_text") => {
                            matches!(key.as_str(), "type" | "connector_text" | "signature")
                        }
                        Some("advisor_tool_result") => matches!(
                            key.as_str(),
                            "type" | "tool_use_id" | "content" | "is_error"
                        ),
                        _ => false,
                    })
                })
            {
                serde_json::from_value(value).map_err(invalid)?
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
    pub(crate) observation: (wire::UsageReport, wire::InferenceReport),
    pub(crate) family: wire::ProtocolFamily,
    blocks: BTreeSet<u32>,
    closed: BTreeSet<u32>,
    reasoning_blocks: BTreeSet<u32>,
    pub(crate) metadata: Value,
    done: bool,
    started: bool,
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
            if has_replay_metadata(&block) {
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
                            self.observe_server_fallback_model(&start.hop.model);
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
                                out.push(self.server_event(event));
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
                                    out.push(self.server_event(event));
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
                                HistoryContentDelta::CitationsDelta {
                                    citation: citation.clone(),
                                }
                            }
                            NativeDelta::ConnectorText(connector_text) => {
                                HistoryContentDelta::ConnectorTextDelta {
                                    connector_text: connector_text.into(),
                                }
                            }
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
                    if self.started {
                        continue;
                    }
                    self.started = true;
                    out.push(HistoryEvent::MessageStart {
                        response: Box::new(HistoryResponse {
                            id: response_id.map(|id| id.as_str().into()).unwrap_or_default(),
                            model,
                            content: vec![],
                            stop_reason: None,
                            stop_details: None,
                            usage: usage(&self.observation.0, &self.observation.1)
                                .map(|(u, _)| u)
                                .unwrap_or_default(),
                            cost: None,
                            provider_metadata: self.metadata.clone(),
                        }),
                    });
                }
                wire::StreamEvent::TextDelta { block, text } => {
                    let index = wire_block_index(block)?;
                    self.start(
                        index,
                        ContentBlock::Text {
                            text: String::new(),
                            cache_control: None,
                        },
                        &mut out,
                    );
                    out.push(HistoryEvent::ContentBlockDelta {
                        index,
                        delta: HistoryContentDelta::TextDelta { text },
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
                            input: json!({}),
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
                        if let Some(TextBlock::Known { text, citations }) =
                            stream_observation::text_block(&value)
                        {
                            let text = text.to_owned();
                            let citations = citations.map(|value| value.cloned());
                            if !self.blocks.contains(&index) {
                                self.start(
                                    index,
                                    ContentBlock::Text {
                                        text: String::new(),
                                        citations: citations.clone(),
                                        cache_control: None,
                                    },
                                    &mut out,
                                );
                                if !text.is_empty() {
                                    out.push(HistoryEvent::ContentBlockDelta {
                                        index,
                                        delta: HistoryContentDelta::TextDelta { text },
                                    });
                                }
                            }
                            out.push(HistoryEvent::ContentBlockDelta {
                                index,
                                delta: HistoryContentDelta::TextDelta { text: text.into() },
                            });
                        }
                        companion(&native, protocol)?
                    } else {
                        host_block(native)?
                    };
                    self.start(index | 0x8000_0000, projected, &mut out);
                }
                wire::StreamEvent::End {
                    stop_reason,
                    usage: report,
                    inference,
                } => {
                    if !self.done {
                        self.flush_replay(&mut out)?;
                        if usage(&report, &inference).is_none() && !self.metadata.is_null() {
                            // Observations can exist without billable token measurements.
                            // Preserve them in a host-only transcript companion rather
                            // than fabricating a zero-usage report. Replay filters this tag.
                            let index = (0..=u32::MAX)
                                .rev()
                                .find(|index| !self.blocks.contains(index))
                                .ok_or_else(|| {
                                    invalid("no stream index available for provider observations")
                                })?;
                            self.start(index, ContentBlock::ProviderContent {
                                protocol: serde_json::to_value(self.family).map_err(invalid)?.as_str().unwrap().into(),
                                value: json!({"type":"lingxi_observation", "metadata": self.metadata}),
                            }, &mut out);
                        }
                        self.done = true;
                        for index in self.blocks.difference(&self.closed) {
                            out.push(HistoryEvent::ContentBlockStop { index: *index });
                        }
                        out.push(HistoryEvent::MessageDelta {
                            delta: HistoryMessageDelta {
                                stop_reason: Some(stop(stop_reason)),
                                stop_details: self
                                    .metadata
                                    .get("stop_details")
                                    .filter(|value| !value.is_null())
                                    .map(|value| {
                                        serde_json::from_value(value.clone()).map_err(invalid)
                                    })
                                    .transpose()?,
                            },
                            usage: usage(&report, &inference).map(|(mut u, _)| {
                                if !self.metadata.is_null() {
                                    u.provider_metadata["stream"] = self.metadata.clone();
                                }
                                u
                            }),
                        });
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
                                    let mut visible_start = block.clone();
                                    if let Some(object) = visible_start.as_object_mut() {
                                        object.insert("text".into(), Value::String(String::new()));
                                    }
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
                                    ..
                                } => {
                                    self.start(
                                        index,
                                        ContentBlock::Text {
                                            text: String::new(),
                                            citations: Some(citations.cloned()),
                                            cache_control: None,
                                        },
                                        &mut out,
                                    );
                                }
                                TextBlock::Known { .. } => {}
                            }
                        }
                        if let Some(stop) = observation.stop_delta {
                            self.stop_delta_seen = true;
                            out.push(HistoryEvent::MessageDelta {
                                delta: HistoryMessageDelta {
                                    stop_reason: Some(stop.reason.into()),
                                    stop_details: stop
                                        .details
                                        .map(|value| {
                                            serde_json::from_value(value.clone()).map_err(invalid)
                                        })
                                        .transpose()?,
                                },
                                usage: usage(&self.observation.0, &self.observation.1).map(
                                    |(mut u, _)| {
                                        if !self.metadata.is_null() {
                                            u.provider_metadata["stream"] = self.metadata.clone();
                                        }
                                        u
                                    },
                                ),
                            });
                        }
                    }
                    append_observation(
                        &mut self.metadata,
                        "provider_events",
                        json!({"protocol": protocol, "payload": payload}),
                    );
                }
                wire::StreamEvent::WebSearch { result } => {
                    out.push(HistoryEvent::WebSearch {
                        result: result.clone(),
                    });
                    append_observation(
                        &mut self.metadata,
                        "web_search",
                        serde_json::to_value(result).map_err(invalid)?,
                    );
                }
                wire::StreamEvent::FileSearch { result } => {
                    append_observation(
                        &mut self.metadata,
                        "file_search",
                        serde_json::to_value(result).map_err(invalid)?,
                    );
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
    response: ProviderResponse,
    protocol: wire::ProtocolFamily,
) -> Result<HistoryResponse, LlmError> {
    project_model_response(decoded, protocol, response.body_json, response.request_id)
}

pub(crate) fn project_model_response(
    decoded: wire::ChatResponse,
    protocol: wire::ProtocolFamily,
    mut metadata: Value,
    request_id: Option<String>,
) -> Result<HistoryResponse, LlmError> {
    clear_computer_binding(&mut metadata);
    let native_stop_details = decoded.anthropic_stop_details().cloned();
    let normalized = usage(&decoded.usage, &decoded.inference)
        .map(|(u, _)| u)
        .unwrap_or_default();
    let mut content = Vec::new();
    for block in decoded.message.content {
        let replay = if has_replay_metadata(&block) {
            Some(companion(&block, protocol)?)
        } else {
            None
        };
        content.push(native_host_block(block, protocol)?);
        content.extend(replay);
    }
    for (key, value) in [
        ("web_search", serde_json::to_value(&decoded.web_search)),
        ("file_search", serde_json::to_value(&decoded.file_search)),
        (
            "native_metadata",
            serde_json::to_value(
                (!decoded.native_metadata.is_empty()).then_some(&decoded.native_metadata),
            ),
        ),
        (
            "response_cache",
            serde_json::to_value(&decoded.response_cache),
        ),
        ("continuation", serde_json::to_value(&decoded.continuation)),
        (
            "executed_profile",
            serde_json::to_value(&decoded.executed_profile),
        ),
        (
            "inference",
            serde_json::to_value(
                (decoded.inference != wire::InferenceReport::default())
                    .then_some(&decoded.inference),
            ),
        ),
    ] {
        let value = value.map_err(invalid)?;
        if !value.is_null() {
            upstream_metadata(&mut metadata).insert(key.into(), value);
        }
    }
    Ok(HistoryResponse {
        id: decoded
            .response_id
            .map(|id| id.as_str().to_owned())
            .or(request_id)
            .unwrap_or_else(|| metadata["id"].as_str().unwrap_or_default().into()),
        model: decoded.model,
        content,
        stop_reason: Some(stop(decoded.stop_reason)),
        stop_details: metadata
            .get("stop_details")
            .filter(|v| !v.is_null())
            .cloned()
            .or(native_stop_details)
            .map(|v| serde_json::from_value(v).map_err(invalid))
            .transpose()?,
        usage: normalized,
        cost: None,
        provider_metadata: metadata,
    })
}

impl HistoryProjector {
    pub(crate) fn observe_model_metadata(&mut self, stream: &client::ModelStream) {
        if let Some(continuation) = stream.continuation() {
            upstream_metadata(&mut self.metadata).insert(
                "continuation".into(),
                serde_json::to_value(continuation).expect("continuation is serializable"),
            );
        }
        if let Some(fallback) = stream.anthropic_fallback() {
            upstream_metadata(&mut self.metadata).insert(
                "anthropic_fallback".into(),
                serde_json::to_value(fallback)
                    .expect("fallback observation contains finite JSON values"),
            );
        }
        if let Some(details) = stream.anthropic_stop_details() {
            if !self.metadata.is_object() {
                self.metadata = json!({});
            }
            self.metadata["stop_details"] = details.clone();
        }
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
    pub(crate) fn projection(family: wire::ProtocolFamily, mut metadata: Value) -> Self {
        clear_computer_binding(&mut metadata);
        clear_host_projection_fields(&mut metadata);
        Self {
            observation: Default::default(),
            family,
            blocks: BTreeSet::new(),
            closed: BTreeSet::new(),
            reasoning_blocks: BTreeSet::new(),
            metadata,
            done: false,
            started: false,
            assembly: client::stream_assembly::StreamAccumulator::new(),
            pending_projection: BTreeSet::new(),
            pending_error: None,
        }
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

    fn batch(events: Vec<Result<wire::StreamEvent, wire::LlmError>>) -> client::StreamBatch {
        client::StreamBatch {
            events,
            usage: Default::default(),
            inference: Default::default(),
            finished: false,
        }
    }

    #[test]
    fn sdk_assembles_signed_tool_replay_at_the_history_edge() {
        let mut projection =
            HistoryProjector::projection(wire::ProtocolFamily::GeminiGenerateContent, Value::Null);
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
                } => Some(&value["block"]),
                _ => None,
            })
            .expect("durable replay metadata");
        assert_eq!(native["input"], json!({"q":"rust"}));
        assert_eq!(native["thought_signature"], "signed");
        assert_eq!(native["provider_id"], "native-call");
        assert_eq!(native["toolset_name"], "browser");
    }

    #[test]
    fn successful_events_in_a_failing_batch_are_presented_before_error() {
        let mut projection =
            HistoryProjector::projection(wire::ProtocolFamily::OpenAiChat, Value::Null);
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
        let mut projection =
            HistoryProjector::projection(wire::ProtocolFamily::OpenAiChat, Value::Null);
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
        assert!(response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall { id, name, input } if id == "call-real" && name == "lookup" && input == &json!({"q":"answer"}))));
        assert!(!response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall { id, name, .. } if id.is_empty() || name.is_empty())));
    }

    #[tokio::test]
    async fn completed_connector_is_salvaged_when_next_frame_fails() {
        let mut projection =
            HistoryProjector::projection(wire::ProtocolFamily::AnthropicMessages, Value::Null);
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
                        payload,
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
        let mut projection =
            HistoryProjector::projection(wire::ProtocolFamily::AnthropicMessages, Value::Null);
        let mut events = Vec::new();
        for payload in [
            json!({"type":"content_block_start","index":2,"content_block":{"type":"connector_text","connector_text":"saved"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"connector_text_delta","connector_text":" result"}}),
        ] {
            events.extend(
                projection
                    .project_batch(batch(vec![Ok(wire::StreamEvent::ProviderEvent {
                        protocol: wire::ProtocolFamily::AnthropicMessages,
                        payload,
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
                        payload: json!({"type":"content_block_stop","index":2}),
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
        let mut projection =
            HistoryProjector::projection(wire::ProtocolFamily::AnthropicMessages, Value::Null);
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

    #[test]
    fn typed_native_response_and_stream_remain_lossless_at_host_boundary() {
        let native = wire::ContentBlock::Native {
            value: wire::NativeExtension::new(
                "openai.responses.computer_call.v1",
                json!({"id":"item","call_id":"call","actions":[{"type":"screenshot"}]}),
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
            provider_metadata: Value::Null,
        };
        assert_eq!(
            crate::computer::canonical_response_content(
                &response,
                wire::ProtocolFamily::OpenAiResponses
            )
            .unwrap(),
            vec![native.clone()]
        );
        let mut projector =
            HistoryProjector::projection(wire::ProtocolFamily::OpenAiResponses, Value::Null);
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
            assert!(
                host_block(wire::ContentBlock::ProviderContent {
                    protocol: wire::ProtocolFamily::AnthropicMessages,
                    value: json!({"type":kind})
                })
                .is_err()
            );
        }
    }
}
use lingxi_llm_client::providers::anthropic::stream_observation::{self, NativeDelta, TextBlock};
use serde_json::{Value, json};

pub(crate) fn attach_computer_binding(
    metadata: &mut Value,
    binding: &lingxi_core::host::NativeContinuationBinding,
) {
    upstream_metadata(metadata).insert(
        "computer_binding".into(),
        serde_json::to_value(binding).expect("captured binding serializes"),
    );
}

fn clear_computer_binding(metadata: &mut Value) {
    if let Some(namespace) = metadata
        .get_mut("llm_client")
        .and_then(Value::as_object_mut)
    {
        namespace.remove("computer_binding");
    }
}

fn native_host_block(
    block: wire::ContentBlock,
    protocol: wire::ProtocolFamily,
) -> Result<ContentBlock, LlmError> {
    if matches!(block, wire::ContentBlock::Native { .. }) {
        Ok(ContentBlock::ProviderContent {
            protocol: serde_json::to_value(protocol)
                .map_err(invalid)?
                .as_str()
                .ok_or_else(|| invalid("protocol is not a string"))?
                .into(),
            value: json!({"type":"lingxi_native_content", "block":block}),
        })
    } else {
        host_block(block)
    }
}
