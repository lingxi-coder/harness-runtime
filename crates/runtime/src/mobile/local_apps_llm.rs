//! 本地应用的 LLM 接缝（v3）：只剩运行中应用的自由文本 `chat` 调用。
//!
//! 设计器/生成管线（出题 / 出方案 / 写码）已整体移除——应用源码现在由
//! 会话中的 agent 直接编辑，构建走 MCP `build` 工具。模型经
//! [`LocalAppsModel`] trait 注入，使桥接逻辑能脱离真实 `ApiService` 单测。
//!
//! `AppError::LlmUnavailable` 用于模型够不到（离线/鉴权失败/超时）；
//! `AppError::LlmOutputRejected` 用于模型答了但答案不合格。两者在客户端
//! 渲染不同文案、提供不同操作，绝不能混用。

use async_trait::async_trait;
use futures_util::StreamExt;
use lingxi_core::types::{ConversationMessage, MediaAnalysis, MessageId, MessageRole};
use llm_runtime::{ApiService, ContentBlock, HistoryContentDelta, HistoryEvent};
use local_apps::AppError;
use sha2::{Digest, Sha256};
use sidequery::{
    filter_messages_to_fingerprints, prepare_media_for_nonvision, ProviderSideQueryClient,
    VisionDelegationService, VisionPacket,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

pub use local_app_builder_service::llm::{
    ChatMessage, ChatOutcome, ChatPart, ChatRequest, ChatRole, ChatStreamEvent, LocalAppsLlm,
    LocalAppsModel, ModelStream,
};

/// Lower one app-supplied part to the conversation vocabulary.
fn chat_part_block(part: ChatPart) -> lingxi_core::types::ContentBlock {
    match part {
        ChatPart::Text(text) => lingxi_core::types::ContentBlock::Text { text, citations: None },
        ChatPart::Image { media_type, base64 } => lingxi_core::types::ContentBlock::Image {
            source: lingxi_core::types::ImageSource::Base64 {
                media_type,
                data: base64,
            },
        },
        ChatPart::Document { media_type, base64 } => lingxi_core::types::ContentBlock::Document {
            source: lingxi_core::types::DocumentSource::Base64 {
                media_type,
                data: base64,
            },
        },
    }
}

/// What the service is told of one provider event: its text, and a stop reason
/// whenever one is reported. Everything else (reasoning, tool use, search,
/// block framing) stays inside the engine boundary.
fn stream_event_of(event: HistoryEvent) -> Option<ChatStreamEvent> {
    match event {
        HistoryEvent::ContentBlockDelta {
            delta: HistoryContentDelta::TextDelta { text },
            ..
        } => Some(ChatStreamEvent::TextDelta(text)),
        HistoryEvent::MessageDelta { delta, .. } => {
            delta.stop_reason.map(ChatStreamEvent::StopReason)
        }
        HistoryEvent::Completed { response } => response
            .stop_reason
            .clone()
            .map(ChatStreamEvent::StopReason),
        HistoryEvent::WebSearch { .. }
        | HistoryEvent::MessageStart { .. }
        | HistoryEvent::ContentBlockStart { .. }
        | HistoryEvent::ContentBlockDelta { .. }
        | HistoryEvent::ContentBlockStop { .. }
        | HistoryEvent::MessageStop => None,
    }
}

/// Project an assistant reply onto its text, matching the repo's existing
/// fold (`orchestrator::hook_prompt_runner::extract_text`): `Text` AND
/// `ConnectorText` contribute, tool-use / reasoning blocks do not, and the
/// pieces are joined bare. Dropping `ConnectorText` would hand the page
/// `ok: true` with an empty answer whenever a reply arrives on that block.
fn extract_chat_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            ContentBlock::ConnectorText { connector_text, .. } => Some(connector_text.as_str()),
            _ => None,
        })
        .collect()
}

/// Real [`LocalAppsModel`] over the shared `ApiService`.
pub struct ApiServiceModel {
    service: Arc<ApiService>,
    /// `(model, profile)` behind ONE `RwLock`, NOT two independent locks:
    /// `ClientCommand::SetModel` updates this in place (via
    /// [`LocalAppsModel::set_model`]) so app-initiated calls follow a live
    /// `/model` switch instead of staying pinned to whatever
    /// `default_model_id`/`default_model_profile` were at engine build time
    /// — the same class of "silently stale after the user changed
    /// something" bug `SharedLlm` closes for a reconnect. Two separate locks
    /// (an earlier version of this code) let a read land BETWEEN `set_model`'s
    /// two writes and see a new model id paired with the OLD provider
    /// profile, routing the new model through the wrong provider — a single
    /// lock over the pair makes that torn read structurally impossible.
    selection: RwLock<(String, Option<String>)>,
    vision_delegation_enabled: bool,
    vision_cache: Mutex<DelegationCache>,
    cost_tracker: Option<Arc<cost::CostTracker>>,
    api_calls_recorded: Option<Arc<std::sync::atomic::AtomicU32>>,
}

#[derive(Default)]
struct DelegationCache {
    order: VecDeque<String>,
    entries: HashMap<String, CachedAnalysis>,
}

struct CachedAnalysis {
    scope: String,
    analysis: MediaAnalysis,
    /// The complete media set sent to the delegate for this cached result.
    /// The public cache key is intentionally scoped to the current user turn;
    /// this secondary set lets us reject stale task/cross-media findings when
    /// the surrounding transcript changes while still reusing observations.
    input_fingerprints: Vec<String>,
}

impl DelegationCache {
    fn get_with_inputs(&self, key: &str) -> Option<(MediaAnalysis, Vec<String>)> {
        self.entries
            .get(key)
            .map(|entry| (entry.analysis.clone(), entry.input_fingerprints.clone()))
    }

    fn put(
        &mut self,
        key: String,
        scope: String,
        value: MediaAnalysis,
        input_fingerprints: Vec<String>,
    ) {
        if let Some(existing) = self.entries.get_mut(&key) {
            let previous = existing.analysis.clone();
            let previous_inputs = existing.input_fingerprints.clone();
            existing.analysis =
                merge_cached_analysis(&previous, &previous_inputs, value, &input_fingerprints);
            existing.scope = scope;
            existing.input_fingerprints = input_fingerprints;
            return;
        }
        self.order.push_back(key.clone());
        while self.order.len() > 128 {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
        self.entries.insert(
            key,
            CachedAnalysis {
                scope,
                analysis: value,
                input_fingerprints,
            },
        );
    }

    fn covering(&self, scope: &str, model: &str, fingerprints: &[String]) -> Vec<MediaAnalysis> {
        let wanted = fingerprints.iter().collect::<HashSet<_>>();
        let mut claimed = HashSet::new();
        let mut selected = Vec::new();
        for key in self.order.iter().rev() {
            let Some(entry) = self.entries.get(key) else {
                continue;
            };
            if entry.scope != scope
                || entry.analysis.model != model
                || entry.analysis.prompt_version != sidequery::PROMPT_VERSION
            {
                continue;
            }
            let retained = entry
                .input_fingerprints
                .iter()
                .filter(|fingerprint| {
                    wanted.contains(fingerprint) && !claimed.contains(*fingerprint)
                })
                .cloned()
                .collect::<Vec<_>>();
            if retained.is_empty() {
                continue;
            }
            let retained_set = retained.iter().cloned().collect::<HashSet<_>>();
            let mut trimmed = entry.analysis.clone();
            trimmed.media_fingerprints = retained;
            trimmed
                .media
                .retain(|observation| retained_set.contains(&observation.fingerprint));
            claimed.extend(trimmed.media_fingerprints.iter().cloned());
            selected.push(trimmed);
            if claimed.len() == wanted.len() {
                break;
            }
        }
        selected
    }
}

struct PreparedChatCall {
    model: String,
    profile: Option<String>,
    system: Option<String>,
    messages: Vec<ConversationMessage>,
    max_tokens: u32,
    temperature: Option<f32>,
}

impl ApiServiceModel {
    #[must_use]
    pub fn new(
        service: Arc<ApiService>,
        model: impl Into<String>,
        profile: Option<String>,
        vision_delegation_enabled: bool,
    ) -> Self {
        Self {
            service,
            selection: RwLock::new((model.into(), profile)),
            vision_delegation_enabled,
            vision_cache: Mutex::new(DelegationCache::default()),
            cost_tracker: None,
            api_calls_recorded: None,
        }
    }

    #[must_use]
    pub fn with_cost_tracking(
        mut self,
        tracker: Arc<cost::CostTracker>,
        api_calls_recorded: Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        self.cost_tracker = Some(tracker);
        self.api_calls_recorded = Some(api_calls_recorded);
        self
    }

    async fn record_delegation_cost(
        &self,
        delegate_model: &str,
        delegate_profile: &str,
        result: &sidequery::VisionDelegationResult,
    ) {
        self.record_delegation_accounting(
            delegate_model,
            delegate_profile,
            result.usage,
            result.elapsed,
            result.retry_count,
            result.api_calls,
        )
        .await;
    }

    async fn record_delegation_accounting(
        &self,
        delegate_model: &str,
        delegate_profile: &str,
        usage: cost::Usage,
        elapsed: std::time::Duration,
        retry_count: u32,
        api_calls: u32,
    ) {
        if api_calls == 0 {
            return;
        }
        if let Some(tracker) = self.cost_tracker.as_ref() {
            tracker
                .record_api_response_v2(
                    orchestrator::cost_wiring::model_ref_from_string(
                        delegate_model,
                        Some(delegate_profile),
                    ),
                    usage,
                    elapsed,
                    retry_count,
                    usage.tokens.cache_read,
                    usage
                        .tokens
                        .cache_write
                        .saturating_add(usage.tokens.cache_write_1h),
                    false,
                    None,
                )
                .await;
        }
        if let Some(counter) = self.api_calls_recorded.as_ref() {
            counter.fetch_add(api_calls, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn delegate_cache_key(
        scope: &str,
        question_text: &str,
        fingerprints: &[String],
        delegate_model: &str,
    ) -> String {
        let question_key = Self::question_key(question_text, fingerprints);
        let mut hasher = Sha256::new();
        hasher.update(scope.as_bytes());
        hasher.update(b"\0");
        hasher.update(question_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(delegate_model.as_bytes());
        hasher.update(b"\0");
        hasher.update(sidequery::PROMPT_VERSION.to_string().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn question_key(question_text: &str, fingerprints: &[String]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(normalize_text(question_text).as_bytes());
        hasher.update(b"\0");
        for fingerprint in fingerprints {
            hasher.update(fingerprint.as_bytes());
            hasher.update(b"\0");
        }
        format!("{:x}", hasher.finalize())
    }

    fn cache_get(
        &self,
        key: &str,
        expected_fingerprints: &[String],
        covered_fingerprints: &HashSet<String>,
    ) -> Option<MediaAnalysis> {
        self.vision_cache
            .lock()
            .expect("vision cache lock poisoned")
            .get_with_inputs(key)
            .and_then(|(analysis, cached)| {
                let complete = expected_fingerprints.iter().all(|fingerprint| {
                    analysis.media_fingerprints.contains(fingerprint)
                        || covered_fingerprints.contains(fingerprint)
                });
                (same_fingerprint_set(&cached, expected_fingerprints) && complete)
                    .then_some(analysis)
            })
    }

    fn cache_put(
        &self,
        key: String,
        scope: String,
        value: MediaAnalysis,
        input_fingerprints: Vec<String>,
    ) {
        let mut cache = self
            .vision_cache
            .lock()
            .expect("vision cache lock poisoned");
        cache.put(key, scope, value, input_fingerprints);
    }

    fn cache_covering(
        &self,
        scope: &str,
        model: &str,
        fingerprints: &[String],
    ) -> Vec<MediaAnalysis> {
        self.vision_cache
            .lock()
            .expect("vision cache lock poisoned")
            .covering(scope, model, fingerprints)
    }

    async fn prepare_request(&self, request: ChatRequest) -> Result<PreparedChatCall, AppError> {
        let (model, profile) = self
            .selection
            .read()
            .expect("selection lock poisoned")
            .clone();
        let mut messages = lower_messages(request.messages.clone());
        let route = self
            .service
            .resolve_media_route(&model, profile.as_deref())
            .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?;
        if route.main.capabilities.vision {
            return Ok(PreparedChatCall {
                model,
                profile,
                system: request.system,
                messages,
                max_tokens: request.max_tokens,
                temperature: request.temperature,
            });
        }
        let main_prepared =
            prepare_media_for_nonvision(&messages, route.main.capabilities.documents)
                .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?;
        let fingerprints = main_prepared
            .media
            .iter()
            .map(|media| media.fingerprint.clone())
            .collect::<Vec<_>>();
        if fingerprints.is_empty() {
            return Ok(PreparedChatCall {
                model,
                profile,
                system: request.system,
                messages,
                max_tokens: request.max_tokens,
                temperature: request.temperature,
            });
        }
        if !self.vision_delegation_enabled {
            return Err(AppError::LlmUnavailable(
                "media delegation unavailable: vision delegation is disabled".into(),
            ));
        }
        let delegate = route.vision_delegate.ok_or_else(|| {
            AppError::LlmUnavailable(
                "media delegation unavailable: no vision delegate is configured for this provider"
                    .into(),
            )
        })?;
        let question_text = latest_user_text(&request.messages);
        let latest_user_index = request
            .messages
            .iter()
            .rposition(|message| message.role == ChatRole::User)
            .unwrap_or(0);
        let cache_scope = request
            .cache_scope
            .clone()
            .unwrap_or_else(|| "local-app".to_string());
        let prior_text_context = prior_text_messages(&request.messages);
        let latest_user_media = prepare_media_for_nonvision(&messages[latest_user_index..], true)
            .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?;
        let current_ordered_fingerprints = latest_user_media
            .media
            .iter()
            .map(|media| media.fingerprint.clone())
            .collect::<Vec<_>>();
        let question_key = Self::question_key(&question_text, &current_ordered_fingerprints);
        let cache_key = Self::delegate_cache_key(
            &cache_scope,
            &question_text,
            &current_ordered_fingerprints,
            &delegate.request_model,
        );
        let historical_fingerprints =
            prepare_media_for_nonvision(&messages[..latest_user_index], true)
                .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?
                .media
                .into_iter()
                .map(|media| media.fingerprint)
                .collect::<Vec<_>>();
        let current_fingerprints = current_ordered_fingerprints
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let reusable = self
            .cache_covering(
                &cache_scope,
                &delegate.request_model,
                &historical_fingerprints,
            )
            .into_iter()
            .collect::<Vec<_>>();
        let reusable = dedupe_reusable_historical_analyses(
            reusable,
            &historical_fingerprints,
            &current_fingerprints,
        );
        let reusable_fingerprints = reusable
            .iter()
            .flat_map(|analysis| analysis.media_fingerprints.iter().cloned())
            .collect::<HashSet<_>>();
        let current_analysis =
            match self.cache_get(&cache_key, &fingerprints, &reusable_fingerprints) {
                Some(cached) => Some(cached),
                None => {
                    let wanted = delegation_wanted_fingerprints(
                        &historical_fingerprints,
                        &reusable_fingerprints,
                        current_fingerprints,
                    );
                    if wanted.is_empty() {
                        None
                    } else {
                        let media_messages = filter_messages_to_fingerprints(&messages, &wanted)
                            .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?;
                        let service = VisionDelegationService::new(Arc::new(
                            ProviderSideQueryClient::from_service(self.service.clone()),
                        ));
                        let packet = VisionPacket {
                            question_key: question_key.clone(),
                            model: delegate.request_model.clone(),
                            profile: Some(delegate.profile_name.clone()),
                            current_user_text: question_text.clone(),
                            prior_text_context,
                            media_messages,
                        };
                        let result = match service.analyze(packet).await {
                            Ok(result) => result,
                            Err(sidequery::SideQueryError::Partial {
                                source,
                                usage,
                                elapsed,
                                retry_count,
                                api_calls,
                            }) => {
                                self.record_delegation_accounting(
                                    &delegate.request_model,
                                    &delegate.profile_name,
                                    usage,
                                    elapsed,
                                    retry_count,
                                    api_calls,
                                )
                                .await;
                                return Err(AppError::LlmUnavailable(format!("{source}")));
                            }
                            Err(error) => {
                                return Err(AppError::LlmUnavailable(format!("{error}")));
                            }
                        };
                        self.record_delegation_cost(
                            &delegate.request_model,
                            &delegate.profile_name,
                            &result,
                        )
                        .await;
                        self.cache_put(
                            cache_key.clone(),
                            cache_scope.clone(),
                            result.analysis.clone(),
                            fingerprints.clone(),
                        );
                        Some(result.analysis)
                    }
                }
            };
        let current_covered = current_analysis
            .as_ref()
            .map(|analysis| {
                analysis
                    .media_fingerprints
                    .iter()
                    .cloned()
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let reusable = reusable
            .into_iter()
            .filter_map(|analysis| subtract_fingerprints(analysis, &current_covered))
            .collect::<Vec<_>>();
        messages = main_prepared.rewritten_messages;
        let mut injected = std::collections::HashSet::new();
        for analysis in reusable.into_iter().chain(current_analysis) {
            let identity = format!(
                "{}\0{}\0{}",
                analysis.question_key,
                analysis.model,
                analysis.media_fingerprints.join("\0")
            );
            if injected.insert(identity) {
                messages.push(ConversationMessage::user_media_analysis(
                    MessageId::new(),
                    analysis,
                ));
            }
        }
        Ok(PreparedChatCall {
            model,
            profile,
            system: request.system,
            messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
        })
    }
}

fn delegation_wanted_fingerprints(
    historical: &[String],
    reusable: &HashSet<String>,
    current: HashSet<String>,
) -> HashSet<String> {
    historical
        .iter()
        .filter(|fingerprint| !reusable.contains(*fingerprint))
        .cloned()
        .chain(current)
        .collect()
}

fn dedupe_reusable_historical_analyses(
    analyses: Vec<MediaAnalysis>,
    historical: &[String],
    current: &HashSet<String>,
) -> Vec<MediaAnalysis> {
    let mut covered = HashSet::new();
    let mut reusable = Vec::new();
    for analysis in analyses {
        let Some(trimmed) = reusable_historical_analysis(analysis, historical, current) else {
            continue;
        };
        let Some(deduped) = subtract_fingerprints(trimmed, &covered) else {
            continue;
        };
        covered.extend(deduped.media_fingerprints.iter().cloned());
        reusable.push(deduped);
    }
    reusable
}

fn reusable_historical_analysis(
    analysis: MediaAnalysis,
    historical: &[String],
    current: &HashSet<String>,
) -> Option<MediaAnalysis> {
    let allowed = historical
        .iter()
        .filter(|fingerprint| !current.contains(*fingerprint))
        .cloned()
        .collect::<HashSet<_>>();
    let retained_fingerprints = analysis
        .media_fingerprints
        .iter()
        .filter(|fingerprint| allowed.contains(*fingerprint))
        .cloned()
        .collect::<Vec<_>>();
    if retained_fingerprints.is_empty() {
        return None;
    }
    let mut trimmed = analysis;
    trimmed.media_fingerprints = retained_fingerprints;
    trimmed
        .media
        .retain(|observation| allowed.contains(&observation.fingerprint));
    trimmed.task_findings.clear();
    trimmed.cross_media_findings.clear();
    trimmed.truncated = false;
    Some(trimmed)
}

fn subtract_fingerprints(
    analysis: MediaAnalysis,
    excluded: &HashSet<String>,
) -> Option<MediaAnalysis> {
    if excluded.is_empty() {
        return Some(analysis);
    }
    let retained = analysis
        .media_fingerprints
        .iter()
        .filter(|fingerprint| !excluded.contains(*fingerprint))
        .cloned()
        .collect::<Vec<_>>();
    if retained.is_empty() {
        return None;
    }
    if retained.len() == analysis.media_fingerprints.len() {
        return Some(analysis);
    }
    let mut trimmed = analysis;
    trimmed.media_fingerprints = retained.to_vec();
    trimmed
        .media
        .retain(|observation| !excluded.contains(&observation.fingerprint));
    trimmed.task_findings.clear();
    trimmed.cross_media_findings.clear();
    Some(trimmed)
}

fn merge_cached_analysis(
    previous: &MediaAnalysis,
    previous_inputs: &[String],
    fresh: MediaAnalysis,
    fresh_inputs: &[String],
) -> MediaAnalysis {
    let allowed = fresh_inputs.iter().collect::<HashSet<_>>();
    let fresh_fingerprints = fresh
        .media_fingerprints
        .iter()
        .filter(|fingerprint| allowed.contains(fingerprint))
        .cloned()
        .collect::<HashSet<_>>();
    let mut observations = fresh
        .media
        .iter()
        .cloned()
        .map(|observation| (observation.fingerprint.clone(), observation))
        .collect::<HashMap<_, _>>();
    for observation in &previous.media {
        if allowed.contains(&observation.fingerprint) {
            observations
                .entry(observation.fingerprint.clone())
                .or_insert_with(|| observation.clone());
        }
    }
    let mut merged = fresh;
    merged.media_fingerprints = fresh_inputs
        .iter()
        .filter(|fingerprint| {
            fresh_fingerprints.contains(*fingerprint)
                || previous
                    .media_fingerprints
                    .iter()
                    .any(|previous| previous == *fingerprint)
        })
        .cloned()
        .collect();
    merged.media = merged
        .media_fingerprints
        .iter()
        .filter_map(|fingerprint| observations.remove(fingerprint))
        .collect();
    if same_fingerprint_set(previous_inputs, fresh_inputs) {
        // A later request may only analyze the current image because the
        // historical observations were already covered; keep the complete
        // task-level findings from the same-context entry.
        merged.task_findings = previous.task_findings.clone();
        merged.cross_media_findings = previous.cross_media_findings.clone();
        merged.truncated |= previous.truncated;
    } else {
        // The surrounding image set changed. Per-image observations remain
        // reusable, but findings that compare or summarize the old set do not.
        merged.task_findings.clear();
        merged.cross_media_findings.clear();
    }
    merged
}

fn same_fingerprint_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left.iter().collect::<HashSet<_>>() == right.iter().collect::<HashSet<_>>()
}

#[async_trait]
impl LocalAppsModel for ApiServiceModel {
    /// An app-initiated free-text call.
    ///
    /// Non-streaming on purpose: `messages_create_side_query` is the only
    /// free-text entry point on `ApiService` that accepts `max_tokens` and
    /// `temperature`, and an app-initiated call spending the user's quota
    /// must carry an explicit budget. There is also nothing to stream INTO —
    /// the bridge is strictly request/response, so a token stream would have
    /// no channel to the page (see the module docs on the push channel that
    /// would be needed first).
    async fn chat(&self, request: ChatRequest) -> Result<ChatOutcome, AppError> {
        let prepared = self.prepare_request(request).await?;
        let response = self
            .service
            .messages_create_side_query(
                &prepared.model,
                prepared.profile.as_deref(),
                prepared.system.as_deref(),
                prepared.messages,
                vec![],
                Some(prepared.max_tokens),
                None,
                vec![],
                prepared.temperature,
                None,
            )
            .await
            .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?;
        Ok(ChatOutcome {
            text: extract_chat_text(&response.content),
            stop_reason: response.stop_reason,
        })
    }

    async fn stream(&self, request: ChatRequest) -> Result<ModelStream, AppError> {
        let prepared = self.prepare_request(request).await?;
        let stream = self
            .service
            .messages_create_side_query_stream(
                &prepared.model,
                prepared.profile.as_deref(),
                prepared.system.as_deref(),
                prepared.messages,
                vec![],
                Some(prepared.max_tokens),
                None,
                vec![],
                prepared.temperature,
                None,
            )
            .await
            .map_err(|error| AppError::LlmUnavailable(format!("{error}")))?;
        Ok(Box::pin(stream.filter_map(|event| async move {
            match event {
                Ok(event) => stream_event_of(event).map(Ok),
                Err(error) => Some(Err(AppError::LlmUnavailable(format!("{error}")))),
            }
        })))
    }

    fn set_model(&self, model: String, profile: Option<String>) {
        *self.selection.write().expect("selection lock poisoned") = (model, profile);
    }
}

fn lower_messages(messages: Vec<ChatMessage>) -> Vec<ConversationMessage> {
    messages
        .into_iter()
        .map(|message| {
            let content: Vec<lingxi_core::types::ContentBlock> =
                message.content.into_iter().map(chat_part_block).collect();
            match message.role {
                ChatRole::User => ConversationMessage::User { api_message_override: None,
                    id: MessageId::new(),
                    content,
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                },
                ChatRole::Assistant => ConversationMessage::Assistant { per_turn_effort: None,
                    id: MessageId::new(),
                    content,
                    stop_reason: None,
                },
            }
        })
        .collect()
}

fn latest_user_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::User)
        .map(message_text)
        .unwrap_or_default()
}

fn prior_text_messages(messages: &[ChatMessage]) -> Vec<(MessageRole, String)> {
    let mut out = Vec::new();
    let mut seen_current_user = false;
    for message in messages.iter().rev() {
        if message.role == ChatRole::User && !seen_current_user {
            seen_current_user = true;
            continue;
        }
        let text = message_text(message);
        if text.is_empty() {
            continue;
        }
        let role = match message.role {
            ChatRole::User => MessageRole::User,
            ChatRole::Assistant => MessageRole::Assistant,
        };
        out.push((role, text));
        if out.len() == 2 {
            break;
        }
    }
    out.reverse();
    out
}

fn message_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ChatPart::Text(text) => Some(text.as_str()),
            ChatPart::Image { .. } | ChatPart::Document { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{AppError, ChatOutcome, ChatRequest, LocalAppsModel};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// 按顺序吐出预置 chat 响应的假模型。
    pub(crate) struct ScriptedModel {
        chat_responses: Mutex<Vec<Result<ChatOutcome, AppError>>>,
        chat_requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedModel {
        /// A model with NO scripted answers — any call fails loudly with
        /// "ran out of responses" rather than silently returning something
        /// plausible. The no-op double for wiring tests.
        pub(crate) fn new() -> Arc<Self> {
            Self::with_chat(Vec::new())
        }

        /// The double scripted for [`LocalAppsModel::chat`].
        pub(crate) fn with_chat(responses: Vec<Result<ChatOutcome, AppError>>) -> Arc<Self> {
            Arc::new(Self {
                chat_responses: Mutex::new(responses),
                chat_requests: Mutex::new(Vec::new()),
            })
        }

        /// The `index`-th `chat` call's request (0-based, call order).
        pub(crate) fn chat_request_at(&self, index: usize) -> ChatRequest {
            self.chat_requests.lock().expect("lock")[index].clone()
        }

        /// How many times `chat` has been called so far.
        #[allow(dead_code)]
        pub(crate) fn chat_call_count(&self) -> usize {
            self.chat_requests.lock().expect("lock").len()
        }
    }

    #[async_trait]
    impl LocalAppsModel for ScriptedModel {
        async fn chat(&self, request: ChatRequest) -> Result<ChatOutcome, AppError> {
            self.chat_requests.lock().expect("lock").push(request);
            let mut responses = self.chat_responses.lock().expect("lock");
            if responses.is_empty() {
                return Err(AppError::Io(
                    "the scripted model ran out of chat responses".into(),
                ));
            }
            responses.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::ScriptedModel;
    use super::*;

    fn cached_analysis(question_key: &str) -> MediaAnalysis {
        MediaAnalysis {
            question_key: question_key.to_string(),
            media_fingerprints: vec![question_key.to_string()],
            model: "vision".to_string(),
            prompt_version: sidequery::PROMPT_VERSION,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            task_findings: vec![],
            media: vec![],
            cross_media_findings: vec![],
            truncated: false,
        }
    }

    #[test]
    fn vision_cache_key_is_scoped_by_app_and_question() {
        let fingerprints = vec!["fp".to_string()];
        let base =
            ApiServiceModel::delegate_cache_key("app-a", "question-a", &fingerprints, "vision");
        assert_ne!(
            base,
            ApiServiceModel::delegate_cache_key("app-b", "question-a", &fingerprints, "vision")
        );
        assert_ne!(
            base,
            ApiServiceModel::delegate_cache_key("app-a", "question-b", &fingerprints, "vision")
        );
    }

    #[test]
    fn vision_cache_key_reuses_analysis_when_prior_text_context_changes() {
        let fingerprints = vec!["fp".to_string()];
        let key_for_context = |_prior_text_context: &str| {
            ApiServiceModel::delegate_cache_key("app-a", "question", &fingerprints, "vision")
        };
        assert_eq!(
            key_for_context("prior answer"),
            key_for_context("different prior answer")
        );
    }

    #[test]
    fn vision_cache_evicts_fifo_at_128_entries() {
        let mut cache = DelegationCache::default();
        for index in 0..129 {
            let key = format!("key-{index}");
            cache.put(
                key.clone(),
                "app".to_string(),
                cached_analysis(&key),
                vec![key.clone()],
            );
        }
        assert_eq!(cache.entries.len(), 128);
        assert!(!cache.entries.contains_key("key-0"));
        assert!(cache.entries.contains_key("key-1"));
        assert!(cache.entries.contains_key("key-128"));
    }

    #[test]
    fn partial_history_cache_miss_selects_only_missing_and_current_media() {
        let historical = vec!["old-covered".to_string(), "old-missing".to_string()];
        let reusable = HashSet::from(["old-covered".to_string()]);
        let current = HashSet::from(["current".to_string()]);
        let wanted = delegation_wanted_fingerprints(&historical, &reusable, current);
        assert_eq!(
            wanted,
            HashSet::from(["old-missing".to_string(), "current".to_string()])
        );
    }

    #[test]
    fn cache_hit_requires_the_same_full_media_context() {
        let mut cached = cached_analysis("question");
        cached.media_fingerprints = vec!["historical".into(), "current".into()];
        let mut cache = DelegationCache::default();
        cache.put(
            "key".into(),
            "app".into(),
            cached,
            vec!["historical".into(), "current".into()],
        );
        let (_, inputs) = cache.get_with_inputs("key").expect("cache entry");
        assert!(same_fingerprint_set(
            &inputs,
            &["current".into(), "historical".into()]
        ));
        assert!(!same_fingerprint_set(&inputs, &["current".into()]));
    }

    #[test]
    fn current_analysis_coverage_removes_duplicate_historical_sidecars() {
        let mut cached = cached_analysis("question");
        cached.media_fingerprints = vec!["historical".into(), "current".into()];
        cached.task_findings = vec!["cross-image finding".into()];
        let trimmed = subtract_fingerprints(cached, &HashSet::from(["current".into()]))
            .expect("historical observation remains");
        assert_eq!(trimmed.media_fingerprints, vec!["historical"]);
        assert!(trimmed.task_findings.is_empty());
    }

    #[test]
    fn reusable_history_prefers_latest_entry_and_preserves_only_unique_coverage() {
        let mut latest = cached_analysis("latest-question");
        latest.media_fingerprints = vec!["shared".into()];
        latest.task_findings = vec!["latest".into()];

        let mut older = cached_analysis("older-question");
        older.media_fingerprints = vec!["shared".into(), "missing".into()];
        older.task_findings = vec!["older".into()];

        let reusable = dedupe_reusable_historical_analyses(
            vec![latest, older],
            &["shared".into(), "missing".into()],
            &HashSet::new(),
        );

        assert_eq!(reusable.len(), 2);
        assert_eq!(reusable[0].question_key, "latest-question");
        assert_eq!(reusable[0].media_fingerprints, vec!["shared"]);
        assert_eq!(reusable[1].question_key, "older-question");
        assert_eq!(reusable[1].media_fingerprints, vec!["missing"]);
        let covered = reusable
            .iter()
            .flat_map(|analysis| analysis.media_fingerprints.iter().cloned())
            .collect::<HashSet<_>>();
        assert_eq!(covered, HashSet::from(["shared".into(), "missing".into()]));
        assert!(reusable
            .iter()
            .all(|analysis| analysis.task_findings.is_empty()
                && analysis.cross_media_findings.is_empty()
                && !analysis.truncated));
    }

    #[test]
    fn covering_ignores_stale_observations_not_present_in_latest_inputs() {
        let mut cache = DelegationCache::default();
        let mut initial = cached_analysis("question");
        initial.media_fingerprints = vec!["historical".into()];
        cache.put(
            "key".into(),
            "app".into(),
            initial.clone(),
            vec!["historical".into()],
        );
        cache.put(
            "key".into(),
            "app".into(),
            MediaAnalysis {
                media_fingerprints: vec!["historical".into(), "current".into()],
                ..initial
            },
            vec!["current".into()],
        );

        assert!(
            cache
                .covering("app", "vision", &["historical".into()])
                .is_empty(),
            "historical reuse must follow the latest cached input set, not stale merged observations"
        );
    }

    /// The free-text seam's answer is whatever prose the model wrote.
    /// Pinning the extraction against hand-built blocks keeps a reasoning
    /// model's thinking out of the app's answer with no `ApiService`
    /// involved.
    #[test]
    fn a_stream_carries_only_text_and_stop_reasons() {
        use llm_runtime::{HistoryContentDelta, HistoryMessageDelta};
        assert_eq!(
            stream_event_of(HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: HistoryContentDelta::TextDelta { text: "hi".into() },
            }),
            Some(ChatStreamEvent::TextDelta("hi".into()))
        );
        let delta = |stop_reason: Option<&str>| HistoryEvent::MessageDelta {
            delta: HistoryMessageDelta {
                stop_reason: stop_reason.map(str::to_string),
                stop_details: None,
            },
            usage: None,
        };
        assert_eq!(
            stream_event_of(delta(Some("end_turn"))),
            Some(ChatStreamEvent::StopReason("end_turn".into()))
        );
        // A message delta that reports no stop reason says nothing.
        assert_eq!(stream_event_of(delta(None)), None);
        assert_eq!(stream_event_of(HistoryEvent::MessageStop), None);
    }

    #[test]
    fn chat_text_concatenates_text_blocks_and_drops_thinking() {
        let content = vec![
            ContentBlock::Reasoning {
                text: "先想一下".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: "答案第一段".into(),
                cache_control: None, citations: None,
            },
            ContentBlock::Text {
                text: "，第二段".into(),
                cache_control: None, citations: None,
            },
        ];
        assert_eq!(extract_chat_text(&content), "答案第一段，第二段");
    }

    #[tokio::test]
    async fn chat_hands_the_whole_request_to_the_model_and_returns_its_answer() {
        let model = ScriptedModel::with_chat(vec![Ok(ChatOutcome {
            text: "好的".into(),
            stop_reason: Some("end_turn".into()),
        })]);
        let llm = LocalAppsLlm::new(model.clone());

        let outcome = llm
            .chat(ChatRequest {
                system: Some("你是这个应用的助手".into()),
                messages: vec![
                    ChatMessage {
                        role: ChatRole::User,
                        content: vec![ChatPart::Text("第一句".into())],
                    },
                    ChatMessage {
                        role: ChatRole::Assistant,
                        content: vec![ChatPart::Text("上一轮回答".into())],
                    },
                    ChatMessage {
                        role: ChatRole::User,
                        content: vec![ChatPart::Text("第二句".into())],
                    },
                ],
                max_tokens: 512,
                temperature: Some(0.3),
                cache_scope: None,
            })
            .await
            .expect("chat");

        assert_eq!(outcome.text, "好的");
        assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
        let seen = model.chat_request_at(0);
        assert_eq!(seen.max_tokens, 512);
        assert_eq!(seen.temperature, Some(0.3));
        assert_eq!(seen.system.as_deref(), Some("你是这个应用的助手"));
        assert_eq!(
            seen.messages.len(),
            3,
            "multi-turn context must reach the model verbatim, not be flattened"
        );
    }

    /// A photo the app just captured has to reach the provider as a real
    /// vision block — not as a base64 string glued into the prompt text,
    /// which is what an app would be forced into if this mapping were
    /// missing (and which no provider can actually look at).
    #[test]
    fn an_image_part_lowers_to_a_provider_vision_block() {
        let block = chat_part_block(ChatPart::Image {
            media_type: "image/jpeg".into(),
            base64: "AQID".into(),
        });
        assert!(matches!(
            block,
            lingxi_core::types::ContentBlock::Image {
                source: lingxi_core::types::ImageSource::Base64 { media_type, data }
            } if media_type == "image/jpeg" && data == "AQID"
        ));

        let block = chat_part_block(ChatPart::Document {
            media_type: "application/pdf".into(),
            base64: "JVBER".into(),
        });
        assert!(matches!(
            block,
            lingxi_core::types::ContentBlock::Document {
                source: lingxi_core::types::DocumentSource::Base64 { media_type, .. }
            } if media_type == "application/pdf"
        ));
    }

    #[tokio::test]
    async fn chat_surfaces_an_unreachable_model_as_llm_unavailable() {
        let model = ScriptedModel::with_chat(vec![Err(AppError::LlmUnavailable("offline".into()))]);
        let llm = LocalAppsLlm::new(model);
        let error = llm
            .chat(ChatRequest {
                system: None,
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatPart::Text("你好".into())],
                }],
                max_tokens: 128,
                temperature: None,
                cache_scope: None,
            })
            .await
            .expect_err("an unreachable model must not read as a refusal");
        assert!(matches!(error, AppError::LlmUnavailable(_)), "{error:?}");
    }

    /// A [`LocalAppsModel`] double that only records `set_model` calls —
    /// proves [`LocalAppsLlm::set_model`] actually delegates to the
    /// underlying model instead of silently no-op'ing (the DEFAULT trait
    /// method every OTHER test double relies on).
    struct RecordingModel {
        calls: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }

    #[async_trait]
    impl LocalAppsModel for RecordingModel {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatOutcome, AppError> {
            unreachable!("not exercised by this test")
        }

        fn set_model(&self, model: String, profile: Option<String>) {
            self.calls.lock().expect("lock").push((model, profile));
        }
    }

    /// PINS the Important-3 fix from the Task 11 review: `ClientCommand::
    /// SetModel` must reach the local-apps LLM, not just the orchestrator's
    /// own model selection — otherwise app-initiated `llm.chat` stays
    /// silently pinned to whatever was live at engine build time forever.
    #[test]
    fn set_model_delegates_to_the_underlying_model() {
        let model = Arc::new(RecordingModel {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let llm = LocalAppsLlm::new(model.clone());
        llm.set_model("claude-opus-5".into(), Some("anthropic".into()));
        llm.set_model("gpt-5.5".into(), None);
        assert_eq!(
            model.calls.lock().expect("lock").as_slice(),
            &[
                ("claude-opus-5".to_string(), Some("anthropic".to_string())),
                ("gpt-5.5".to_string(), None),
            ]
        );
    }
}
