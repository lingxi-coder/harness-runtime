//! Native realtime Agent execution through the ordinary Harness tool dispatcher.
use super::*;
use futures::{stream::FuturesUnordered, StreamExt};
use lingxi_core::types::ToolUseId;
use lingxi_llm_client::realtime::{
    RealtimeClose, RealtimeControl, RealtimeEvent, RealtimeEvents, RealtimeHistoryItem,
    RealtimeInput, RealtimeRole, RealtimeTranscriptDirection,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Host snapshot used to configure an SDK agent connector before dispatch.
#[derive(Clone, Debug)]
pub struct RealtimeAgentContext {
    /// Owning current Agent session.
    pub session_id: String,
    /// Exact current session provider profile.
    pub profile_name: Option<String>,
    /// Current Agent system instructions.
    pub instructions: String,
    /// Normalized text and tool history; connector imports it without a response.
    pub history: Vec<RealtimeHistoryItem>,
    /// Current permission-filtered tool catalog in function schema form.
    pub tools: Vec<serde_json::Value>,
}
/// Bounded host controls and native playback evidence.
#[derive(Debug)]
pub enum RealtimeAgentInput {
    /// Provider-neutral input, excluding model-controlled route/configuration.
    Provider(RealtimeInput),
    /// Native player drained this item; only now may its transcript enter history.
    PlaybackCompleted { item_id: Option<String> },
}
/// Hard session and resource bounds, independent from chat retry budgets.
#[derive(Clone, Copy, Debug)]
pub struct RealtimeAgentLimits {
    /// Absolute session lifetime, including tool and permission waits.
    pub max_duration: Duration,
    /// Idle lifetime; paused while a tool or permission request is pending.
    pub idle_timeout: Duration,
    /// Maximum simultaneous tool requests.
    pub max_pending_tools: usize,
    /// Maximum accumulated text bytes per transcript item.
    pub max_transcript_bytes: usize,
    /// Maximum pending transcript items awaiting playback acknowledgements.
    pub max_pending_transcripts: usize,
}
impl Default for RealtimeAgentLimits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(30 * 60),
            idle_timeout: Duration::from_secs(2 * 60),
            max_pending_tools: 8,
            max_transcript_bytes: 1024 * 1024,
            max_pending_transcripts: 64,
        }
    }
}
/// A session's terminal reason. Provider reconnect/replay is never automatic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealtimeAgentEnd {
    Closed,
    Cancelled,
    Deadline,
    Idle,
    ToolStopped,
}
fn failure(message: impl Into<String>) -> OrchestratorError {
    OrchestratorError::Internal(message.into())
}
fn audio_tool(name: &str) -> bool {
    matches!(name, "speech" | "voice")
}

/// Convert model-visible history without dropping unsupported user media.
pub fn realtime_history(
    history: &[ConversationMessage],
) -> Result<Vec<RealtimeHistoryItem>, OrchestratorError> {
    let mut output = Vec::new();
    let mut tool_ids = HashMap::new();
    for message in history {
        let (role, blocks) = match message {
            ConversationMessage::System {
                content,
                api_system,
                ..
            } => {
                if api_system.is_some() {
                    output.push(RealtimeHistoryItem::Message {
                        item_id: None,
                        role: RealtimeRole::System,
                        text: content.clone(),
                    });
                }
                continue;
            }
            ConversationMessage::User {
                content,
                is_visible_in_transcript_only,
                ..
            } => {
                if *is_visible_in_transcript_only {
                    continue;
                }
                (RealtimeRole::User, content)
            }
            ConversationMessage::Assistant { content, .. } => (RealtimeRole::Assistant, content),
        };
        for block in blocks {
            match block {
                ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
                    if !text.is_empty() {
                        output.push(RealtimeHistoryItem::Message {
                            item_id: None,
                            role,
                            text: text.clone(),
                        });
                    }
                }
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    provider_id,
                    ..
                } => {
                    let call_id = provider_id.clone().unwrap_or_else(|| id.to_string());
                    tool_ids.insert(id.clone(), (call_id.clone(), name.clone()));
                    output.push(RealtimeHistoryItem::ToolCall {
                        item_id: None,
                        call_id,
                        name: name.clone(),
                        arguments: input.clone(),
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    content_blocks,
                    provider_tool_use_id,
                    ..
                } => {
                    let (mapped, name) = tool_ids
                        .get(tool_use_id)
                        .cloned()
                        .ok_or_else(|| failure("realtime history has an unmatched tool result"))?;
                    output.push(RealtimeHistoryItem::ToolResult {
                        item_id: None,
                        call_id: provider_tool_use_id.clone().unwrap_or(mapped),
                        name: Some(name),
                        output: serde_json::json!({"content":content,"is_error":is_error,"content_blocks":content_blocks}),
                    });
                }
                // Signed provider reasoning is not portable between conversation
                // protocols, and is never presented as ordinary transcript text.
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ProviderContent { .. } => {}
                _ => {
                    return Err(failure(
                        "native realtime cannot import this session's media/history block",
                    ))
                }
            }
        }
    }
    Ok(output)
}
struct RealtimeGuard {
    control: RealtimeControl,
    cancel: CancellationToken,
}
impl Drop for RealtimeGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let control = self.control.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    control.abort(RealtimeClose {
                        code: 1000,
                        reason: "host realtime ended".into(),
                    }),
                )
                .await;
            });
        }
    }
}
#[derive(Default)]
struct TranscriptLedger {
    output: HashMap<Option<String>, String>,
    heard: HashSet<Option<String>>,
    discarded: HashSet<Option<String>>,
    finalized_input: HashSet<Option<String>>,
    finalized_output: HashSet<Option<String>>,
    generated: HashSet<Option<String>>,
}
impl TranscriptLedger {
    fn interrupt(&mut self) {
        self.discarded.extend(self.output.keys().cloned());
        self.discarded.extend(self.generated.drain());
        self.discarded.extend(self.heard.iter().cloned());
        self.output.clear();
        self.heard.clear();
    }
    fn output_final(
        &mut self,
        key: Option<String>,
        text: String,
        limits: RealtimeAgentLimits,
    ) -> Result<Option<String>, OrchestratorError> {
        if self.discarded.contains(&key) || self.finalized_output.contains(&key) {
            return Ok(None);
        }
        if text.len() > limits.max_transcript_bytes {
            return Err(failure("realtime transcript exceeds its byte bound"));
        }
        if self.heard.remove(&key) {
            self.finalized_output.insert(key.clone());
            self.generated.remove(&key);
            return Ok(Some(text));
        }
        if !self.output.contains_key(&key) && self.output.len() >= limits.max_pending_transcripts {
            return Err(failure("realtime playback transcript queue is full"));
        }
        self.output.insert(key, text);
        Ok(None)
    }
    fn playback(&mut self, key: Option<String>) -> Option<String> {
        if self.discarded.contains(&key) || self.finalized_output.contains(&key) {
            return None;
        }
        let text = self.output.remove(&key);
        if text.is_none() {
            if self.generated.contains(&key) {
                self.heard.insert(key);
            }
        } else {
            self.finalized_output.insert(key.clone());
            self.generated.remove(&key);
        }
        text
    }
}
impl ConversationOrchestrator {
    /// Resolve the current inference route without requesting credentials or
    /// selecting a provider from model spelling. Default sessions use the same
    /// registry resolver as an ordinary Agent request.
    pub async fn current_audio_profile(&self) -> Result<String, OrchestratorError> {
        let (model, profile) = self.current_prompt_route().await;
        self.resolve_audio_profile(&model, profile)
    }
    /// Atomically bind a native caller to one session and its effective profile.
    pub async fn current_audio_binding(&self) -> Result<(String, String), OrchestratorError> {
        let session = self.session.lock().await;
        let profile = self.resolve_audio_profile(&session.model, session.model_profile.clone())?;
        Ok((session.session_id.as_uuid().to_string(), profile))
    }
    fn resolve_audio_profile(
        &self,
        model: &str,
        profile: Option<String>,
    ) -> Result<String, OrchestratorError> {
        match profile.filter(|value| !value.trim().is_empty()) {
            Some(profile) => Ok(profile),
            None => self
                .api
                .resolve_media_route(model, None)
                .map(|route| route.main.profile_name)
                .map_err(|_| {
                    failure("current inference route has no authoritative audio provider profile")
                }),
        }
    }
    /// Prepare current Agent history and tool policy for an SDK native connector.
    pub async fn prepare_realtime_agent(&self) -> Result<RealtimeAgentContext, OrchestratorError> {
        let _gate = self.turn_gate.lock().await;
        let (session_id, history) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                session.model_context_history(),
            )
        };
        let profile_name = Some(self.current_audio_profile().await?);
        let tools = self.build_wire_tools().await.0.into_iter().filter_map(|tool| {
            let object = tool.as_object()?;
            let name = object.get("name")?.as_str()?;
            if audio_tool(name) { return None; }
            Some(serde_json::json!({
                "name":name,
                "description":object.get("description").cloned().unwrap_or(serde_json::Value::String(String::new())),
                "parameters":object.get("input_schema").cloned().unwrap_or_else(||serde_json::json!({"type":"object"})),
            }))
        }).collect();
        Ok(RealtimeAgentContext {
            session_id,
            profile_name,
            instructions: self.effective_system_prompt().await,
            history: realtime_history(&history)?,
            tools,
        })
    }
    async fn append_realtime_text(&self, text: String, user: bool) {
        if text.trim().is_empty() {
            return;
        }
        let message = if user {
            ConversationMessage::user(MessageId::new(), text)
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text,
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                per_turn_effort: None,
            }
        };
        self.session.lock().await.history.push(message.clone());
        self.persist_message_to_jsonl(&message).await;
    }
    async fn realtime_dispatch_tool(
        &self,
        call_id: String,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        assistant: MessageId,
        cancel: CancellationToken,
    ) -> Result<(String, serde_json::Value, bool), OrchestratorError> {
        let uses = vec![(id.clone(), name, input, Some(call_id.clone()))];
        let deferred = crate::turn_loop::tool_dispatch::dispatch_tool_uses_tracked_deferred(
            self,
            &uses,
            Some(cancel),
            Some(assistant),
        )
        .await?;
        let (results, prevent, injected, modifiers) =
            crate::turn_loop::tool_dispatch::finish_direct_tool_dispatch(self, deferred).await?;
        let requires_restart = !injected.is_empty() || !modifiers.is_empty();
        let value=results.first().map(|block|match block{ContentBlock::ToolResult{content,is_error,content_blocks,..}=>serde_json::json!({"content":content,"is_error":is_error,"content_blocks":content_blocks}),_=>serde_json::to_value(block).unwrap_or(serde_json::Value::Null)}).unwrap_or_else(||serde_json::json!({"is_error":true,"content":"tool produced no result"}));
        let message = ConversationMessage::User {
            id: MessageId::new(),
            content: results,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            api_message_override: None,
        };
        self.session.lock().await.history.push(message.clone());
        self.persist_message_to_jsonl(&message).await;
        self.flush_hook_attachments(&id).await;
        crate::turn_loop::append_tool_injected_messages(self, injected, None).await;
        crate::turn_loop::apply_model_context_modifiers(self, modifiers).await?;
        Ok((call_id, value, prevent || requires_restart))
    }
    /// Drive one native session while keeping the shared Agent permission,
    /// sandbox, hook, cancellation and transcript paths authoritative.
    /// The host must already run the SDK driver and import `prepared.history`.
    pub async fn run_realtime_agent(
        self: &Arc<Self>,
        prepared: RealtimeAgentContext,
        control: RealtimeControl,
        mut events: RealtimeEvents,
        mut inputs: mpsc::Receiver<RealtimeAgentInput>,
        output: mpsc::Sender<RealtimeEvent>,
        limits: RealtimeAgentLimits,
        cancel: CancellationToken,
    ) -> Result<RealtimeAgentEnd, OrchestratorError> {
        let session_cancel = cancel.clone();
        let _guard = RealtimeGuard {
            control: control.clone(),
            cancel: session_cancel.clone(),
        };
        if !control.capabilities().agent_conversation() {
            return Err(failure(
                "provider realtime lacks required Agent history, tools or transcripts",
            ));
        }
        if limits.max_duration.is_zero()
            || limits.idle_timeout.is_zero()
            || limits.max_pending_tools == 0
            || limits.max_transcript_bytes == 0
            || limits.max_pending_transcripts == 0
        {
            return Err(failure("native realtime resource limits must be positive"));
        }
        let _gate = tokio::select! {biased;_ = session_cancel.cancelled()=>return Ok(RealtimeAgentEnd::Cancelled),gate=self.turn_gate.lock()=>gate};
        {
            let session = self.session.lock().await;
            if session.session_id.as_uuid().to_string() != prepared.session_id
                || realtime_history(&session.model_context_history())? != prepared.history
            {
                return Err(failure(
                    "Agent session history changed before realtime admission",
                ));
            }
        }
        if Some(self.current_audio_profile().await?) != prepared.profile_name {
            return Err(failure(
                "Agent provider profile changed before realtime admission",
            ));
        }
        let allowed: HashSet<String> = prepared
            .tools
            .iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(str::to_owned)
            })
            .collect();
        let deadline = Instant::now() + limits.max_duration;
        let mut idle = Instant::now() + limits.idle_timeout;
        let mut ledger = TranscriptLedger::default();
        let mut tool_tokens: HashMap<String, CancellationToken> = HashMap::new();
        let mut seen_calls = HashSet::new();
        let mut usage_turns = HashSet::new();
        let mut pending = FuturesUnordered::new();
        // Each native call enters the common batch dispatcher independently.
        // Serialize those batches so mutating tools cannot overlap outside its
        // per-batch scheduler's concurrency safety boundary.
        let tool_dispatch = Arc::new(tokio::sync::Mutex::new(()));
        let result = async {
        loop {
            tokio::select! {
                biased;
                _ = session_cancel.cancelled()=>return Ok(RealtimeAgentEnd::Cancelled),
                _ = tokio::time::sleep_until(deadline)=>return Ok(RealtimeAgentEnd::Deadline),
                _ = tokio::time::sleep_until(idle), if pending.is_empty()=>return Ok(RealtimeAgentEnd::Idle),
                completed = pending.next(), if !pending.is_empty()=>{
                    let (call_id,value,prevent)=completed.expect("nonempty tool futures")?;let cancelled = tool_tokens.remove(&call_id).is_some_and(|token| token.is_cancelled());idle=Instant::now()+limits.idle_timeout;
                    if cancelled { continue; }
                    control.send(RealtimeInput::ToolResult{call_id,output:value}).map_err(|error|failure(error.to_string()))?;
                    if prevent{return Ok(RealtimeAgentEnd::ToolStopped);}
                    // Submit every concurrent result before requesting the next response.
                    if pending.is_empty(){control.continue_response().map_err(|error|failure(error.to_string()))?;}
                },
                input = inputs.recv()=>{
                    let Some(input)=input else{return Ok(RealtimeAgentEnd::Closed);};idle=Instant::now()+limits.idle_timeout;
                    match input {
                        RealtimeAgentInput::PlaybackCompleted{item_id}=>{if let Some(text)=ledger.playback(item_id){self.append_realtime_text(text,false).await;}},
                        RealtimeAgentInput::Provider(input)=>{
                            // Hosts cannot bypass the Agent dispatcher with forged results/history.
                            if matches!(input,RealtimeInput::ImportHistory{..}|RealtimeInput::DeleteItem{..}|RealtimeInput::ToolResult{..}|RealtimeInput::ToolResults{..}){return Err(failure("host realtime input cannot replace Agent history or tool results"));}
                            if matches!(input,RealtimeInput::Interrupt){ledger.interrupt();for token in tool_tokens.values(){CancellationToken::cancel(token);}}
                            if let RealtimeInput::Text(text)=&input{self.append_realtime_text(text.clone(),true).await;}
                            control.send(input).map_err(|error|failure(error.to_string()))?;
                        }
                    }
                },
                event = events.next()=>{
                    let Some(event)=event else{return Ok(RealtimeAgentEnd::Closed);};idle=Instant::now()+limits.idle_timeout;
                    if seen_calls.len() + usage_turns.len() + ledger.finalized_input.len() + ledger.finalized_output.len() + ledger.discarded.len() > limits.max_pending_transcripts.saturating_mul(64) { return Err(failure("realtime session history resource bound reached")); }
                    match &event {
                        RealtimeEvent::ToolCall{call_id,name,arguments}=>{
                            if !seen_calls.insert(call_id.clone()){return Err(failure("provider repeated a realtime tool call identity"));}
                            if pending.len()>=limits.max_pending_tools{return Err(failure("realtime tool queue is full"));}
                            if audio_tool(name)||!allowed.contains(name)||self.find_dispatchable_tool(name).is_none(){return Err(failure("provider requested an unavailable realtime Agent tool"));}
                            let id=ToolUseId::new();let assistant=MessageId::new();let message=ConversationMessage::Assistant{id:assistant,content:vec![ContentBlock::ToolUse{id:id.clone(),name:name.clone(),input:arguments.clone(),input_projection:None,provider_id:Some(call_id.clone())}],stop_reason:Some("tool_use".into()),per_turn_effort:None};
                            self.session.lock().await.history.push(message.clone());self.persist_message_to_jsonl(&message).await;
                            let token=session_cancel.child_token();tool_tokens.insert(call_id.clone(),token.clone());let orch=self.clone();let call_id=call_id.clone();let name=name.clone();let arguments=arguments.clone();
                            let tool_dispatch = tool_dispatch.clone();
                            pending.push(async move {
                                let _dispatch = tool_dispatch.lock().await;
                                orch.realtime_dispatch_tool(call_id,id,name,arguments,assistant,token).await
                            });
                        },
                        RealtimeEvent::ToolCancelled{call_ids}=>{for id in call_ids{if let Some(token)=tool_tokens.get(id){token.cancel();}}},
                        RealtimeEvent::Interrupted|RealtimeEvent::UserSpeechStarted=>{ ledger.interrupt(); for token in tool_tokens.values() { token.cancel(); } },
                        RealtimeEvent::AudioDelta { item_id, .. } => {
                            if !ledger.generated.contains(item_id) && ledger.generated.len() >= limits.max_pending_transcripts { return Err(failure("realtime pending playback queue is full")); }
                            ledger.generated.insert(item_id.clone());
                        },
                        RealtimeEvent::Usage { turn_id: Some(turn_id), input_tokens, output_tokens, .. } => {
                            if usage_turns.insert(turn_id.clone()) {
                                let mut session = self.session.lock().await;
                                session.usage.0.input_tokens = session.usage.0.input_tokens.saturating_add(input_tokens.unwrap_or(0));
                                session.usage.0.output_tokens = session.usage.0.output_tokens.saturating_add(output_tokens.unwrap_or(0));
                            }
                        },
                        RealtimeEvent::Transcript{direction,text,item_id,final_chunk:true,..}=>{
                            if text.len()>limits.max_transcript_bytes{return Err(failure("realtime transcript exceeds its byte bound"));}
                            match direction {
                                RealtimeTranscriptDirection::Input=>{if ledger.finalized_input.insert(item_id.clone()){self.append_realtime_text(text.clone(),true).await;}},
                                RealtimeTranscriptDirection::Output=>{if let Some(text)=ledger.output_final(item_id.clone(),text.clone(),limits)?{self.append_realtime_text(text,false).await;}},
                            }
                        },
                        RealtimeEvent::ProviderError{message,..}|RealtimeEvent::ConnectionInterrupted{message}=>return Err(failure(format!("native realtime provider failed: {message}"))),
                        RealtimeEvent::Closed{..}=>return Ok(RealtimeAgentEnd::Closed),
                        _=>{},
                    }
                    output.try_send(event).map_err(|error|failure(format!("realtime output queue unavailable: {error}")))?;
                }
            }
        }
        }.await;
        // Close the duplex transport immediately, then let the common tool
        // cancellation policy finish its cleanup. Block-policy mutations must
        // never be force-dropped merely because the audio session ended.
        session_cancel.cancel();
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            control.abort(RealtimeClose {
                code: 1000,
                reason: "host realtime ended".into(),
            }),
        )
        .await;
        while pending.next().await.is_some() {}
        result
    }
}

#[cfg(test)]
#[path = "realtime_tests.rs"]
mod tests;
