use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use llm_runtime::LlmError;
use protocol::{ContentBlock, ConversationMessage, MediaAnalysis, MessageId, MessageRole};
use sidequery::{
    filter_messages_to_fingerprints, prepare_media_for_nonvision, PreparedDelegation, VisionPacket,
    PROMPT_VERSION,
};

use crate::conversation::{
    ConversationOrchestrator, ModelCallPath, ModelCallPreparer, OutgoingHistoryRewriter,
    PreparedModelCall,
};
use crate::error::OrchestratorError;

const MEDIA_PROGRESS_NAME: &str = "VisionDelegation";

pub(crate) struct VisionModelCallPreparer {
    enabled: bool,
}

impl VisionModelCallPreparer {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self { enabled: true }
    }

    #[must_use]
    pub(crate) fn with_enabled(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl Default for VisionModelCallPreparer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelCallPreparer for VisionModelCallPreparer {
    async fn prepare(
        &self,
        orch: &ConversationOrchestrator,
        _path: ModelCallPath,
        _system_prompt: Option<&str>,
        cancel: Option<&tokio_util::sync::CancellationToken>,
        draft: PreparedModelCall,
    ) -> Result<PreparedModelCall, OrchestratorError> {
        let route = orch
            .api
            .resolve_media_route(&draft.model, draft.model_profile.as_deref())
            .map_err(OrchestratorError::ApiCall)?;

        if route.main.capabilities.vision {
            return Ok(finish_prepared_call(
                draft,
                true,
                route.main.capabilities.documents,
                None,
            ));
        }
        let prepared_full =
            prepare_for_main(&draft.history_snapshot, route.main.capabilities.documents)?;
        if prepared_full.media.is_empty() {
            return Ok(finish_prepared_call(
                draft,
                false,
                route.main.capabilities.documents,
                Some(prepared_full.rewritten_messages),
            ));
        }
        if !self.enabled {
            return Err(OrchestratorError::ApiCall(
                LlmError::MediaDelegationUnavailable {
                    message: "vision delegation is disabled".to_string(),
                },
            ));
        }
        let delegate = route.vision_delegate.as_ref().ok_or_else(|| {
            OrchestratorError::ApiCall(LlmError::MediaDelegationUnavailable {
                message: format!(
                    "model '{}' cannot accept images and no vision delegate is configured",
                    route.main.display_model
                ),
            })
        })?;

        let Some(question_index) = last_real_user_index(&draft.history_snapshot) else {
            return Ok(finish_prepared_call(
                draft,
                false,
                route.main.capabilities.documents,
                Some(prepared_full.rewritten_messages),
            ));
        };
        let question_key = draft.history_snapshot[question_index].id().to_string();
        let question_messages = draft.history_snapshot[question_index..].to_vec();
        let prepared_question = prepare_for_delegate(&question_messages)?;
        let question_fingerprints = prepared_question
            .media
            .iter()
            .map(|media| media.fingerprint.clone())
            .collect::<Vec<_>>();
        if question_fingerprints.is_empty() {
            return Ok(finish_prepared_call(
                draft,
                false,
                route.main.capabilities.documents,
                Some(prepared_full.rewritten_messages),
            ));
        }

        let covered = covered_delegate_fingerprints(
            &draft.history_snapshot,
            &question_key,
            &delegate.request_model,
        );
        let uncovered: HashSet<String> = question_fingerprints
            .iter()
            .filter(|fingerprint| !covered.contains(*fingerprint))
            .cloned()
            .collect();
        if uncovered.is_empty() {
            return Ok(finish_prepared_call(
                draft,
                false,
                route.main.capabilities.documents,
                Some(prepared_full.rewritten_messages),
            ));
        }

        let packet = VisionPacket {
            question_key: question_key.clone(),
            model: delegate.request_model.clone(),
            profile: Some(delegate.profile_name.clone()),
            current_user_text: latest_real_user_text(&draft.history_snapshot),
            prior_text_context: prior_text_context(&draft.history_snapshot, question_index),
            media_messages: filter_messages_to_fingerprints(&question_messages, &uncovered)
                .map_err(as_media_error)?,
        };
        let progress_id = format!("vision-delegation:{question_key}");
        let status = format!(
            "正在用 `{}` 分析 {} 张图片",
            delegate.request_model,
            uncovered.len()
        );
        let mut cost_scope = orch
            .model_runtime
            .cost_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if cost_scope.is_none() {
            if let Some(tracker) = orch.model_runtime.cost_tracker.as_ref() {
                let session_id = orch.session.lock().await.session_id;
                cost_scope = Some(tracker.session_scope(session_id));
            }
        }
        if let Some(scope) = cost_scope.as_ref() {
            scope.preflight().await.map_err(|error| {
                OrchestratorError::Internal(format!(
                    "vision cost durability preflight failed: {error}"
                ))
            })?;
        }
        orch.output
            .emit_hook_progress_started(
                &progress_id,
                MEDIA_PROGRESS_NAME,
                MEDIA_PROGRESS_NAME,
                Some(&status),
            )
            .await;
        let progress = VisionProgressGuard::new(orch.output.clone(), progress_id);
        // The progress sink may itself await. Re-check the captured session
        // immediately before the paid delegation so a freeze that landed
        // while opening progress cannot leak one more provider request.
        if let Some(scope) = cost_scope.as_ref() {
            scope.preflight().await.map_err(|error| {
                OrchestratorError::Internal(format!(
                    "vision cost durability preflight failed: {error}"
                ))
            })?;
        }
        let analyzed = if let Some(cancel) = cancel {
            tokio::select! {
                result = orch.api.analyze_vision_delegation(packet) => {
                    result.map_err(OrchestratorError::ApiCall)
                }
                () = cancel.cancelled() => Err(OrchestratorError::VisionDelegationCancelled),
            }
        } else {
            orch.api
                .analyze_vision_delegation(packet)
                .await
                .map_err(OrchestratorError::ApiCall)
        };
        let owned_cost_response = match &analyzed {
            Ok(result) if result.api_calls > 0 => cost_scope.as_ref().map(|scope| {
                (
                    scope.submit_model_response(cost::CostModelResponse {
                        model_ref: crate::cost_wiring::model_ref_from_string(
                            &delegate.request_model,
                            Some(delegate.profile_name.as_str()),
                        ),
                        usage: result.usage,
                        duration: result.elapsed,
                        retries: result.retry_count,
                        cache_read_input_tokens: result.usage.tokens.cache_read,
                        cache_creation_input_tokens: result
                            .usage
                            .tokens
                            .cache_write
                            .saturating_add(result.usage.tokens.cache_write_1h),
                        is_batch_request: false,
                        bus: orch.model_runtime.analytics_bus.clone(),
                    }),
                    result.api_calls,
                )
            }),
            Err(OrchestratorError::ApiCall(LlmError::MediaDelegationPartial {
                accounting,
                ..
            })) if accounting.api_calls > 0 => cost_scope.as_ref().map(|scope| {
                let usage = cost::Usage {
                    tokens: cost::TokenUsage {
                        input: accounting.input_tokens,
                        output: accounting.output_tokens,
                        cache_write: accounting.cache_write,
                        cache_read: accounting.cache_read,
                        reasoning_output: accounting.reasoning_output,
                        cache_write_1h: accounting.cache_write_1h,
                    },
                    server_tool_use: None,
                    speed: None,
                };
                (
                    scope.submit_model_response(cost::CostModelResponse {
                        model_ref: crate::cost_wiring::model_ref_from_string(
                            &delegate.request_model,
                            Some(delegate.profile_name.as_str()),
                        ),
                        usage,
                        duration: accounting.elapsed(),
                        retries: accounting.retry_count,
                        cache_read_input_tokens: usage.tokens.cache_read,
                        cache_creation_input_tokens: usage
                            .tokens
                            .cache_write
                            .saturating_add(usage.tokens.cache_write_1h),
                        is_batch_request: false,
                        bus: orch.model_runtime.analytics_bus.clone(),
                    }),
                    accounting.api_calls,
                )
            }),
            _ => None,
        };
        progress.finish().await;
        if let Some((receipt, api_calls)) = owned_cost_response {
            let settlement = receipt.settle().await;
            orch.model_runtime
                .api_calls_recorded
                .fetch_add(api_calls, std::sync::atomic::Ordering::SeqCst);
            if let Err(error) = settlement.persistence_result() {
                orch.note_cost_settlement_failure(error).await;
            }
        }
        let analyzed = match analyzed {
            Ok(analyzed) => analyzed,
            Err(OrchestratorError::ApiCall(LlmError::MediaDelegationPartial {
                message,
                accounting: _,
            })) => {
                return Err(OrchestratorError::ApiCall(
                    LlmError::MediaDelegationUnavailable { message },
                ));
            }
            Err(error) => return Err(error),
        };
        ensure_not_cancelled(cancel)?;

        let appended = maybe_append_analysis(
            orch,
            &question_key,
            &question_fingerprints,
            analyzed.analysis,
            cancel,
        )
        .await?;
        let Some(message) = appended else {
            return Err(OrchestratorError::ApiCall(
                LlmError::MediaDelegationUnavailable {
                    message:
                        "conversation changed while vision delegation was running; retry the turn"
                            .to_string(),
                },
            ));
        };
        if let Err(error) = ensure_not_cancelled(cancel) {
            discard_analysis_message(orch, &message).await;
            return Err(error);
        }
        orch.persist_message_to_jsonl(&message).await;
        let refreshed_history = {
            let session = orch.session.lock().await;
            session.history.clone()
        };
        let rewritten = prepare_for_main(&refreshed_history, route.main.capabilities.documents)?
            .rewritten_messages;
        Ok(finish_prepared_call(
            draft,
            false,
            route.main.capabilities.documents,
            Some(rewritten),
        ))
    }
}

struct VisionProgressGuard {
    output: Arc<dyn platform_api::OutputStream>,
    id: String,
    armed: bool,
}

impl VisionProgressGuard {
    fn new(output: Arc<dyn platform_api::OutputStream>, id: String) -> Self {
        Self {
            output,
            id,
            armed: true,
        }
    }

    async fn finish(mut self) {
        self.armed = false;
        self.output.emit_hook_progress_finished(&self.id).await;
    }
}

impl Drop for VisionProgressGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let output = self.output.clone();
        let id = self.id.clone();
        runtime.spawn(async move {
            output.emit_hook_progress_finished(&id).await;
        });
    }
}

struct VisionHistoryRewriter {
    documents_supported: bool,
}

#[async_trait]
impl OutgoingHistoryRewriter for VisionHistoryRewriter {
    async fn rewrite(
        &self,
        _orch: &ConversationOrchestrator,
        raw_history: Vec<ConversationMessage>,
    ) -> Result<Vec<ConversationMessage>, OrchestratorError> {
        Ok(prepare_for_main(&raw_history, self.documents_supported)?.rewritten_messages)
    }
}

fn finish_prepared_call(
    draft: PreparedModelCall,
    native_vision: bool,
    documents_supported: bool,
    rewritten_history: Option<Vec<ConversationMessage>>,
) -> PreparedModelCall {
    if native_vision {
        return draft;
    }
    PreparedModelCall {
        history_snapshot: rewritten_history.unwrap_or(draft.history_snapshot),
        model: draft.model,
        model_profile: draft.model_profile,
        outgoing_history_rewriter: Some(Arc::new(VisionHistoryRewriter {
            documents_supported,
        })),
    }
}

async fn maybe_append_analysis(
    orch: &ConversationOrchestrator,
    expected_question_key: &str,
    expected_fingerprints: &[String],
    analysis: MediaAnalysis,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Option<ConversationMessage>, OrchestratorError> {
    if analysis.media_fingerprints.is_empty() {
        return Ok(None);
    }

    let mut session = orch.session.lock().await;
    ensure_not_cancelled(cancel)?;
    let Some(question_index) = last_real_user_index(&session.history) else {
        return Ok(None);
    };
    if session.history[question_index].id().to_string() != expected_question_key {
        return Ok(None);
    }
    let prepared_question = prepare_for_delegate(&session.history[question_index..])?;
    let current_fingerprints = prepared_question
        .media
        .iter()
        .map(|media| media.fingerprint.clone())
        .collect::<Vec<_>>();
    if current_fingerprints != expected_fingerprints {
        return Ok(None);
    }

    let message = ConversationMessage::user_media_analysis(MessageId::new(), analysis);
    session.history.push(message.clone());
    Ok(Some(message))
}

fn ensure_not_cancelled(
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(), OrchestratorError> {
    if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        Err(OrchestratorError::VisionDelegationCancelled)
    } else {
        Ok(())
    }
}

async fn discard_analysis_message(orch: &ConversationOrchestrator, message: &ConversationMessage) {
    let id = message.id().to_string();
    let mut session = orch.session.lock().await;
    session.history.retain(|entry| entry.id().to_string() != id);
}

fn prepare_for_main(
    messages: &[ConversationMessage],
    documents_supported: bool,
) -> Result<PreparedDelegation, OrchestratorError> {
    prepare_media_for_nonvision(messages, documents_supported).map_err(as_media_error)
}

fn prepare_for_delegate(
    messages: &[ConversationMessage],
) -> Result<PreparedDelegation, OrchestratorError> {
    prepare_media_for_nonvision(messages, true).map_err(as_media_error)
}

fn covered_delegate_fingerprints(
    history: &[ConversationMessage],
    question_key: &str,
    delegate_model: &str,
) -> HashSet<String> {
    history
        .iter()
        .flat_map(|message| match message {
            ConversationMessage::User { content, .. } => content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::MediaAnalysis { analysis } => Some(analysis),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ConversationMessage::Assistant { .. } | ConversationMessage::System { .. } => {
                Vec::new()
            }
        })
        .filter(|analysis| {
            analysis.question_key == question_key
                && analysis.model == delegate_model
                && analysis.prompt_version == PROMPT_VERSION
        })
        .flat_map(|analysis| analysis.media_fingerprints.iter().cloned())
        .collect()
}

fn last_real_user_index(history: &[ConversationMessage]) -> Option<usize> {
    history
        .iter()
        .rposition(|message| matches!(message, ConversationMessage::User { is_meta: false, .. }))
}

fn latest_real_user_text(history: &[ConversationMessage]) -> String {
    history
        .iter()
        .rev()
        .find_map(|message| match message {
            ConversationMessage::User {
                content,
                is_meta: false,
                ..
            } => Some(join_text_blocks(content)),
            _ => None,
        })
        .unwrap_or_default()
}

fn prior_text_context(
    history: &[ConversationMessage],
    question_index: usize,
) -> Vec<(MessageRole, String)> {
    let mut out = Vec::new();
    for message in history[..question_index].iter().rev() {
        match message {
            ConversationMessage::User {
                content,
                is_meta: false,
                ..
            } => {
                let text = join_text_blocks(content);
                if !text.is_empty() {
                    out.push((MessageRole::User, text));
                }
            }
            ConversationMessage::Assistant { content, .. } => {
                let text = join_text_blocks(content);
                if !text.is_empty() {
                    out.push((MessageRole::Assistant, text));
                }
            }
            ConversationMessage::System { .. } | ConversationMessage::User { .. } => {}
        }
        if out.len() == 2 {
            break;
        }
    }
    out.reverse();
    out
}

fn join_text_blocks(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn as_media_error(error: impl std::fmt::Display) -> OrchestratorError {
    OrchestratorError::ApiCall(LlmError::MediaDelegationUnavailable {
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidequery::VisionDelegationResult;

    use crate::test_support::{
        mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
        NoOpPermissionGate, StaticMemoryProvider,
    };
    use tool_api::registry::ToolRegistry;

    struct VisionAwareMockApiClient {
        inner: MockApiClient,
        route: llm_runtime::MediaRoute,
        result: std::sync::Mutex<Option<VisionDelegationResult>>,
        packets: std::sync::Mutex<Vec<VisionPacket>>,
        block_delegation: bool,
    }

    impl VisionAwareMockApiClient {
        fn new(
            responses: Vec<llm_runtime::LlmResponse>,
            route: llm_runtime::MediaRoute,
            result: VisionDelegationResult,
        ) -> Self {
            Self {
                inner: MockApiClient::new(responses),
                route,
                result: std::sync::Mutex::new(Some(result)),
                packets: std::sync::Mutex::new(Vec::new()),
                block_delegation: false,
            }
        }

        fn blocking(route: llm_runtime::MediaRoute) -> Self {
            Self {
                inner: MockApiClient::new(vec![]),
                route,
                result: std::sync::Mutex::new(None),
                packets: std::sync::Mutex::new(Vec::new()),
                block_delegation: true,
            }
        }

        async fn captured_msgs(&self) -> Vec<Vec<ConversationMessage>> {
            self.inner.captured_msgs().await
        }

        fn captured_packets(&self) -> Vec<VisionPacket> {
            self.packets.lock().unwrap().clone()
        }
    }

    #[test]
    fn post_response_cancellation_guard_rejects_commit() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            ensure_not_cancelled(Some(&cancel)),
            Err(OrchestratorError::VisionDelegationCancelled)
        ));
    }

    #[async_trait]
    impl crate::conversation::OrchestratorApiClient for VisionAwareMockApiClient {
        async fn messages_create(
            &self,
            model: &str,
            profile: Option<&str>,
            system: Option<&str>,
            msgs: Vec<ConversationMessage>,
            tools: Vec<serde_json::Value>,
        ) -> Result<llm_runtime::LlmResponse, LlmError> {
            self.inner
                .messages_create(model, profile, system, msgs, tools)
                .await
        }

        fn resolve_media_route(
            &self,
            _model: &str,
            _profile: Option<&str>,
        ) -> Result<llm_runtime::MediaRoute, LlmError> {
            Ok(self.route.clone())
        }

        async fn analyze_vision_delegation(
            &self,
            packet: VisionPacket,
        ) -> Result<VisionDelegationResult, LlmError> {
            self.packets.lock().unwrap().push(packet);
            if self.block_delegation {
                return std::future::pending().await;
            }
            self.result
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| LlmError::Transport {
                    message: "vision delegation script exhausted".to_string(),
                })
        }
    }

    fn media_route() -> llm_runtime::MediaRoute {
        llm_runtime::MediaRoute {
            main: llm_runtime::ResolvedRoute {
                provider_id: llm_runtime::ProviderId::OpenAICompatible {
                    name: "test".to_string(),
                },
                profile_name: "test".to_string(),
                request_model: "text-only".to_string(),
                display_model: "text-only".to_string(),
                pricing_model: llm_runtime::PricingModelRef {
                    pricing_provider_id: llm_runtime::ProviderId::OpenAICompatible {
                        name: "test".to_string(),
                    },
                    billing_model: "text-only".to_string(),
                    request_model: "text-only".to_string(),
                    display_model: "text-only".to_string(),
                },
                capabilities: llm_runtime::Capabilities {
                    streaming: true,
                    tools: true,
                    vision: false,
                    documents: false,
                    reasoning: false,
                    structured_output: false,
                },
                connection_chain: Vec::new(),
                failover: Default::default(),
            },
            vision_delegate: Some(llm_runtime::ResolvedRoute {
                provider_id: llm_runtime::ProviderId::OpenAICompatible {
                    name: "test".to_string(),
                },
                profile_name: "test".to_string(),
                request_model: "vision".to_string(),
                display_model: "vision".to_string(),
                pricing_model: llm_runtime::PricingModelRef {
                    pricing_provider_id: llm_runtime::ProviderId::OpenAICompatible {
                        name: "test".to_string(),
                    },
                    billing_model: "vision".to_string(),
                    request_model: "vision".to_string(),
                    display_model: "vision".to_string(),
                },
                capabilities: llm_runtime::Capabilities {
                    streaming: true,
                    tools: false,
                    vision: true,
                    documents: false,
                    reasoning: false,
                    structured_output: false,
                },
                connection_chain: Vec::new(),
                failover: Default::default(),
            }),
        }
    }

    #[test]
    fn prior_question_analysis_does_not_cover_repeated_current_media() {
        let history = vec![ConversationMessage::user_media_analysis(
            MessageId::new(),
            MediaAnalysis {
                question_key: "old-question".to_string(),
                media_fingerprints: vec!["same-image".to_string()],
                model: "vision".to_string(),
                prompt_version: PROMPT_VERSION,
                created_at: std::time::SystemTime::UNIX_EPOCH,
                task_findings: vec![],
                media: vec![],
                cross_media_findings: vec![],
                truncated: false,
            },
        )];

        assert!(covered_delegate_fingerprints(&history, "new-question", "vision").is_empty());
        assert!(
            covered_delegate_fingerprints(&history, "old-question", "vision")
                .contains("same-image")
        );
    }

    #[tokio::test]
    async fn cancellation_discards_delegate_result_and_clears_progress() {
        let user_message = ConversationMessage::user_with_images(
            MessageId::new(),
            "look".to_string(),
            vec![protocol::ImageSource::Url {
                url: "https://example.com/cancel.png".to_string(),
            }],
        );
        let api = Arc::new(VisionAwareMockApiClient::blocking(media_route()));
        let output = Arc::new(MockOutputStream::new());
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            api,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        {
            let mut session = orch.session.lock().await;
            session.history.push(user_message.clone());
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let result = VisionModelCallPreparer::new()
            .prepare(
                &orch,
                ModelCallPath::Streaming,
                None,
                Some(&cancel),
                PreparedModelCall {
                    history_snapshot: vec![user_message],
                    model: "text-only".to_string(),
                    model_profile: Some("test".to_string()),
                    outgoing_history_rewriter: None,
                },
            )
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("cancel must stop the delegate"),
        };
        assert!(matches!(
            error,
            OrchestratorError::VisionDelegationCancelled
        ));
        let session = orch.session.lock().await;
        assert_eq!(session.history.len(), 1);
        drop(session);
    }

    #[tokio::test]
    async fn preparer_rewrites_outgoing_image_and_persists_media_analysis() {
        let user_message = ConversationMessage::user_with_images(
            MessageId::new(),
            "look".to_string(),
            vec![protocol::ImageSource::Url {
                url: "https://example.com/a.png".to_string(),
            }],
        );
        let question_key = user_message.id().to_string();
        let fingerprint =
            sidequery::collect_media_fingerprints(&[ConversationMessage::user_with_images(
                MessageId::new(),
                String::new(),
                vec![protocol::ImageSource::Url {
                    url: "https://example.com/a.png".to_string(),
                }],
            )])
            .expect("fingerprint")
            .remove(0);
        let api = Arc::new(VisionAwareMockApiClient::new(
            vec![mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "ok".to_string(),
                    cache_control: None,
                }],
                Some("end_turn"),
            )],
            media_route(),
            VisionDelegationResult {
                analysis: MediaAnalysis {
                    question_key,
                    media_fingerprints: vec![fingerprint],
                    model: "vision".to_string(),
                    prompt_version: PROMPT_VERSION,
                    created_at: std::time::SystemTime::UNIX_EPOCH,
                    task_findings: vec!["found".to_string()],
                    media: vec![],
                    cross_media_findings: vec![],
                    truncated: false,
                },
                usage: cost::Usage::default(),
                elapsed: std::time::Duration::from_millis(1),
                retry_count: 0,
                api_calls: 1,
            },
        ));
        let output = Arc::new(MockOutputStream::new());
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_model_call_preparer(Arc::new(VisionModelCallPreparer::new()));
        {
            let mut session = orch.session.lock().await;
            session.history.push(user_message);
        }

        let _ = crate::turn_loop::execute_one_turn_with_recovery_tracked(&orch, None, None)
            .await
            .expect("turn succeeds");

        let calls = api.captured_msgs().await;
        assert_eq!(calls.len(), 1);
        let rendered = serde_json::to_string(&calls[0]).expect("serialize request");
        assert!(rendered.contains("see media analysis"));
        assert!(!rendered.contains("\"type\":\"image\""));
        assert_eq!(api.captured_packets().len(), 1);
        assert!(!output.snapshot().await.is_empty());
    }
}
