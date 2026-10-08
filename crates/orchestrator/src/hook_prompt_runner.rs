//! Orchestrator-side [`hooks::HookPromptRunner`] implementation.
//!
//! The hooks crate runs a `prompt` hook (`execPromptHook.ts`) through the
//! [`hooks::HookPromptRunner`] seam WITHOUT depending on the api-client. This
//! adapter closes that seam from the orchestrator side: it reuses the exact
//! one-shot, non-streaming [`OrchestratorApiClient::messages_create`] call the
//! orchestrator already makes for its other non-conversational LLM passes
//! (compaction summary / conversation-title generation), so the prompt hook
//! rides the same provider / retry / telemetry plumbing.
//!
//! Wiring: the composition root constructs an [`ApiClientHookPromptRunner`]
//! over the same `Arc<dyn OrchestratorApiClient>` it hands the orchestrator,
//! then injects it via `HookExecutorImpl::with_prompt_runner` (an `Option`,
//! default `None`).
//!
//! Model overrides use the host's shared model/profile resolver. Native
//! Anthropic routes may select their configured small-fast alias; every
//! other route keeps the session model unless the hook explicitly overrides it.

use std::sync::{Arc, OnceLock, Weak};

use agent::model_resolution::{
    resolve_user_model_selection, resolve_user_specified_model, ModelProviderKind, ModelResolutionContext,
    ModelResolutionContextProvider, ResolvedModelSelection,
};
use async_trait::async_trait;
use hooks::{HookPromptRunner, PromptHookError, PromptHookRequest};
use lingxi_core::types::{ConversationMessage, MessageId};
use llm_runtime::{ContentBlock as LlmContentBlock, HistoryResponse, LlmError};

use crate::conversation::{ConversationOrchestrator, OrchestratorApiClient};

/// Where the evaluator learns which model the session it judges is talking to.
///
/// The runner is built before the orchestrator exists (the hook executor is a
/// constructor input of the orchestrator), so the session is bound late through
/// [`ApiClientHookPromptRunner::attach`] — the same cell shape as
/// [`crate::JsonlHookAttachmentSink`]. A test binds a fake instead.
#[async_trait]
pub trait HookSessionModel: Send + Sync {
    /// The session's live `(model, model_profile)`, or `None` once the session
    /// is gone.
    async fn session_model(&self) -> Option<(String, Option<String>)>;
}

#[async_trait]
impl HookSessionModel for Weak<ConversationOrchestrator> {
    async fn session_model(&self) -> Option<(String, Option<String>)> {
        let orch = self.upgrade()?;
        let session = orch.session.lock().await;
        Some((session.model.clone(), session.model_profile.clone()))
    }
}

/// Implements [`HookPromptRunner`] over the orchestrator's one-shot
/// non-streaming `messages_create` seam.
pub struct ApiClientHookPromptRunner {
    api: Arc<dyn OrchestratorApiClient>,
    model_resolution: Arc<dyn ModelResolutionContextProvider>,
    session: OnceLock<Arc<dyn HookSessionModel>>,
}

impl ApiClientHookPromptRunner {
    /// Build a runner over the shared api-client handle. Pass the SAME
    /// `Arc<dyn OrchestratorApiClient>` the orchestrator uses so the prompt hook
    /// shares the provider routing / auth / telemetry.
    #[must_use]
    pub fn new(
        api: Arc<dyn OrchestratorApiClient>,
        model_resolution: Arc<dyn ModelResolutionContextProvider>,
    ) -> Self {
        Self {
            api,
            model_resolution,
            session: OnceLock::new(),
        }
    }

    /// Bind the runner to the session whose transcript it evaluates.
    ///
    /// First call wins; the runner holds a `Weak`, so this creates no
    /// orchestrator↔hook-executor cycle.
    pub fn attach(&self, orch: &Arc<ConversationOrchestrator>) {
        self.attach_session_model(Arc::new(Arc::downgrade(orch)));
    }

    /// [`Self::attach`] with an arbitrary session-model source.
    pub fn attach_session_model(&self, session: Arc<dyn HookSessionModel>) {
        let _ = self.session.set(session);
    }

    /// Resolve the evaluator's complete route from the shared host authority.
    fn resolve_model(
        &self,
        override_model: Option<&str>,
        session: Option<(&str, Option<&str>)>,
    ) -> Result<ResolvedModelSelection, PromptHookError> {
        let context = match session {
            Some((model, profile)) => self.model_resolution.context_for_route(model, profile),
            None => match override_model {
                Some(model) => self.model_resolution.context_for_route(model, None),
                None => {
                    return Err(PromptHookError::Query(
                        "prompt hook current model route is unavailable".into(),
                    ));
                }
            },
        }
        .map_err(|error| PromptHookError::Query(error.to_string()))?;
        let native_provider = matches!(
            context.route.provider,
            Some(
                ModelProviderKind::FirstParty
                    | ModelProviderKind::Bedrock
                    | ModelProviderKind::Vertex
                    | ModelProviderKind::Foundry
            )
        );
        if let Some(model) = override_model {
            return resolve_user_model_selection(
                model,
                None,
                &context,
                self.model_resolution.as_ref(),
            )
            .map_err(|error| PromptHookError::Query(error.to_string()));
        }
        let small_fast_alias = if !native_provider {
            None
        } else if context.catalog_aliases.contains_key("small-fast") {
            Some("small-fast")
        } else if context.catalog_aliases.contains_key("haiku")
            || context.family_defaults.haiku.is_some()
        {
            Some("haiku")
        } else {
            None
        };
        if let Some(alias) = small_fast_alias {
            let model = resolve_user_specified_model(alias, &context)
                .map_err(|error| PromptHookError::Query(error.to_string()))?;
            return resolve_user_model_selection(
                &model,
                context.route.profile.as_deref(),
                &context,
                self.model_resolution.as_ref(),
            )
            .map_err(|error| PromptHookError::Query(error.to_string()));
        }
        Ok(ResolvedModelSelection {
            model: context.route.model.clone(),
            model_profile: context.route.profile.clone(),
            model_resolution_context: context,
        })
    }

    /// Concatenate the assistant message's text blocks (the analog of
    /// `extractTextContent(response.message.content)`; `execPromptHook.ts:105`).
    /// `Text` and `ConnectorText` blocks contribute; tool-use / reasoning blocks
    /// are ignored, matching `extractTextContent`'s text-only projection.
    fn extract_text(response: &HistoryResponse) -> String {
        let mut out = String::new();
        for block in &response.content {
            match block {
                LlmContentBlock::Text { text, .. } => out.push_str(text),
                LlmContentBlock::ConnectorText { connector_text, .. } => {
                    out.push_str(connector_text);
                }
                _ => {}
            }
        }
        out
    }

    /// Map an [`LlmError`] to a [`PromptHookError`]. A transport timeout becomes
    /// [`PromptHookError::Timeout`] (the `execPromptHook.ts` aborted-signal
    /// path); every other failure is a [`PromptHookError::Query`]
    /// (`outcome: 'non_blocking_error'`).
    fn map_error(err: LlmError) -> PromptHookError {
        match err {
            LlmError::Transport { ref message }
                if message.contains("timeout") || message.contains("Timeout") =>
            {
                // Best-effort: `LlmError::Transport` doesn't carry a Duration,
                // so we synthesize a zero-duration timeout for the hook error.
                PromptHookError::Timeout(std::time::Duration::ZERO)
            }
            other => PromptHookError::Query(other.to_string()),
        }
    }
}

/// Drop reasoning blocks from the transcript before it is judged.
///
/// The evaluator runs with thinking disabled and is told to judge transcript
/// evidence; a reasoning trace is not evidence, and it is the one block that
/// cannot cross providers — Anthropic refuses a `thinking` block without the
/// signature only its own models produce, and `DeepSeek` / `Kimi` traces never
/// carry one. An assistant message left empty keeps the placeholder the
/// signature-recovery path uses, so no message goes out without content.
fn strip_transcript_thinking(messages: &mut [ConversationMessage]) {
    let is_thinking = |block: &lingxi_core::types::ContentBlock| {
        matches!(
            block,
            lingxi_core::types::ContentBlock::Thinking { .. }
                | lingxi_core::types::ContentBlock::RedactedThinking { .. }
        )
    };
    for message in messages {
        let ConversationMessage::Assistant { content, .. } = message else {
            continue;
        };
        if !content.iter().any(is_thinking) {
            continue;
        }
        content.retain(|block| !is_thinking(block));
        if content.is_empty() {
            content.push(lingxi_core::types::ContentBlock::Text {
                text: "[Thinking removed]".into(), citations: None,
            });
        }
    }
}

#[async_trait]
impl HookPromptRunner for ApiClientHookPromptRunner {
    async fn run(&self, req: PromptHookRequest) -> Result<String, PromptHookError> {
        let session = match req.model_selection.as_ref() {
            Some(selection) => Some((selection.model.clone(), selection.model_profile.clone())),
            None => match self.session.get() {
                Some(session) => session.session_model().await,
                None => None,
            },
        };
        let selected = self.resolve_model(
            req.model.as_deref(),
            session
                .as_ref()
                .map(|(model, profile)| (model.as_str(), profile.as_deref())),
        )?;
        let model = selected.model;
        let profile = selected.model_profile;
        // Single user turn carrying the (already `$ARGUMENTS`-substituted)
        // hook prompt; the fixed evaluation system prompt is passed via
        // `system`. No tools are advertised — the prompt hook only needs the
        // model's `{ok, reason?}` JSON text (`execPromptHook.ts:62-100`).
        let hooks::PromptHookTranscript {
            messages: mut transcript,
            last_usage_tokens: last_usage,
            message_grouping,
        } = match req.transcript {
            Some(transcript) => transcript,
            None => load_hook_transcript(req.transcript_path.as_deref()).await?,
        };
        strip_transcript_thinking(&mut transcript);
        let budget = hook_transcript_budget(&selected.model_resolution_context);
        let query = async {
            let mut messages = if last_usage <= budget {
                transcript.clone()
            } else {
                bound_hook_transcript(&transcript, budget, &message_grouping)
            };
            messages.push(ConversationMessage::user(
                MessageId::new(),
                req.prompt.clone(),
            ));
            let response = self
                .api
                .messages_create(crate::OrchestratorApiRequest::HookPrompt(
                    crate::HookPromptRequest::new(
                        &model,
                        profile.as_deref(),
                        &req.system_prompt,
                        messages,
                    ),
                ))
                .await;
            let response = match response {
                Err(LlmError::ContextOverflow { .. }) if !transcript.is_empty() => {
                    let mut messages = if last_usage <= budget / 2 {
                        transcript.clone()
                    } else {
                        bound_hook_transcript(&transcript, budget / 2, &message_grouping)
                    };
                    messages.push(ConversationMessage::user(
                        MessageId::new(),
                        req.prompt.clone(),
                    ));
                    self.api
                        .messages_create(crate::OrchestratorApiRequest::HookPrompt(
                            crate::HookPromptRequest::new(
                                &model,
                                profile.as_deref(),
                                &req.system_prompt,
                                messages,
                            ),
                        ))
                        .await
                }
                other => other,
            };
            response.map_err(Self::map_error)
        };
        let query = llm_runtime::thinking_scope::scope_thinking_recovery(
            llm_runtime::thinking_scope::ThinkingRecoveryScope::default(),
            query,
        );
        let response = tokio::time::timeout(req.timeout, query)
            .await
            .map_err(|_| PromptHookError::Timeout(req.timeout))??;
        Ok(Self::extract_text(&response))
    }
}

fn hook_transcript_budget(context: &ModelResolutionContext) -> usize {
    if context.native_1m == Some(true) && !context.disable_1m_context {
        500_000
    } else {
        100_000
    }
}

/// Restore the host-selected transcript through the same tolerant, branch-aware
/// reader as resume. A missing transcript is empty; other I/O errors fail the
/// evaluator instead of silently judging an incomplete view.
async fn load_hook_transcript(
    path: Option<&std::path::Path>,
) -> Result<hooks::PromptHookTranscript, PromptHookError> {
    let Some(path) = path else {
        return Ok(hooks::PromptHookTranscript::default());
    };
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(hooks::PromptHookTranscript::default())
        }
        Err(e) => {
            return Err(PromptHookError::Query(format!(
                "Cannot read hook transcript: {e}"
            )));
        }
    };
    let loaded = session::jsonl::reader::route_lines(&text);
    let session_id = loaded
        .messages_in_order
        .last()
        .map(|m| m.session_id.as_str())
        .unwrap_or("");
    let (chain, _) = session::jsonl::loader::build_conversation_chain(&loaded, session_id);
    let rows = if chain.is_empty() {
        &loaded.messages_in_order
    } else {
        &chain
    };
    let usage = rows
        .iter()
        .rev()
        .find(|r| {
            r.message_type == "assistant"
                && r.message.get("usage").is_some()
                && r.message.get("model").and_then(serde_json::Value::as_str) != Some("<synthetic>")
        })
        .and_then(|r| r.message.get("usage"));
    let last_usage = usage
        .map(|usage| {
            [
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ]
            .iter()
            .map(|key| {
                usize::try_from(
                    usage
                        .get(*key)
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                )
                .unwrap_or(usize::MAX)
            })
            .fold(0usize, usize::saturating_add)
        })
        .unwrap_or(0);
    let state = crate::resume::state_from_messages(uuid::Uuid::nil(), rows);
    Ok(hooks::PromptHookTranscript {
        messages: state.history,
        last_usage_tokens: last_usage,
        message_grouping: state.hook_message_grouping,
    })
}

// 2.1.263 `vc` / `C0` / `qXr`: UTF-16 Math.round(length / 4),
// fixed media charge, and recursively sized tool-result bodies.
fn estimate_hook_content(value: &serde_json::Value) -> usize {
    fn text(value: &str) -> usize {
        (value.encode_utf16().count() + 2) / 4
    }
    if let Some(value) = value.as_str() {
        return text(value);
    }
    if let Some(values) = value.as_array() {
        return values.iter().map(estimate_hook_content).sum();
    }
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("image" | "document") => 2000,
        Some("text") => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(text)
            .unwrap_or(0),
        Some("thinking") => value
            .get("thinking")
            .and_then(serde_json::Value::as_str)
            .map(text)
            .unwrap_or(0),
        Some("redacted_thinking") => value
            .get("data")
            .and_then(serde_json::Value::as_str)
            .map(text)
            .unwrap_or(0),
        Some("tool_result") => value.get("content").map(estimate_hook_content).unwrap_or(0),
        Some("tool_use") => text(&format!(
            "{}{}",
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            value
                .get("input")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}))
        )),
        _ => text(&value.to_string()),
    }
}

/// Keep complete assistant turns, including their following tool results. The
/// latest turn is always retained, even if it alone exceeds the budget (`Wps`).
fn bound_hook_transcript(
    messages: &[ConversationMessage],
    token_budget: usize,
    metadata: &std::collections::HashMap<MessageId, (bool, bool)>,
) -> Vec<ConversationMessage> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut assistant_id = None;
    for (index, message) in messages.iter().enumerate() {
        if let ConversationMessage::Assistant { id, .. } = message {
            let (is_virtual, resumed) = metadata.get(id).copied().unwrap_or_default();
            if is_virtual {
                continue;
            }
            if index > start && assistant_id != Some(*id) && !resumed {
                groups.push(start..index);
                start = index;
            }
            assistant_id = Some(*id);
        }
    }
    if start < messages.len() {
        groups.push(start..messages.len());
    }
    let mut kept = messages.len();
    let mut tokens = 0;
    for group in groups.into_iter().rev() {
        let size: usize = messages[group.clone()]
            .iter()
            .map(|m| match m {
                ConversationMessage::User { content, .. }
                | ConversationMessage::Assistant { content, .. } => {
                    estimate_hook_content(&serde_json::to_value(content).unwrap_or_default())
                }
                _ => serde_json::to_string(m)
                    .map(|s| s.encode_utf16().count().div_ceil(4))
                    .unwrap_or(0),
            })
            .sum();
        if kept < messages.len() && tokens + size > token_budget {
            break;
        }
        tokens += size;
        kept = group.start;
    }
    if kept == 0 || messages.is_empty() {
        return messages.to_vec();
    }
    let mut result = vec![ConversationMessage::user(
        MessageId::new(),
        format!(
            "[Earlier conversation truncated to fit the hook evaluator's context window — {kept} earlier messages omitted. Evaluate the condition against the recent transcript below; if the required evidence may be in the omitted prefix, return {{\"ok\": false, \"reason\": \"insufficient evidence in transcript\"}}.]"
        ),
    )];
    result.extend_from_slice(&messages[kept..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::model_resolution::{FamilyModelDefaults, ModelResolutionError, ModelRouteFacts};
    use llm_runtime::{ExecutionUsage as Usage, HistoryResponse};
    use std::sync::Mutex;

    /// One recorded `messages_create` call: `(model, profile, system, messages)`.
    type RecordedCall = (
        String,
        Option<String>,
        Option<String>,
        Vec<ConversationMessage>,
    );

    /// A bound session with a fixed `(model, profile)`.
    struct FixedSession(String, Option<String>);
    #[async_trait]
    impl HookSessionModel for FixedSession {
        async fn session_model(&self) -> Option<(String, Option<String>)> {
            Some((self.0.clone(), self.1.clone()))
        }
    }

    struct TestRoutes(Vec<ModelResolutionContext>);

    impl ModelResolutionContextProvider for TestRoutes {
        fn context_for_route(
            &self,
            model: &str,
            profile: Option<&str>,
        ) -> Result<ModelResolutionContext, ModelResolutionError> {
            let qualified = self.0.iter().find_map(|context| {
                let profile = context.route.profile.as_deref()?;
                model
                    .strip_prefix(&format!("{profile}/"))
                    .map(|model| (model, profile))
            });
            let (model, profile) =
                qualified.map_or((model, profile), |(model, profile)| (model, Some(profile)));
            let candidates: Vec<_> = self
                .0
                .iter()
                .filter(|context| {
                    if profile.is_some() && context.route.profile.as_deref() != profile {
                        return false;
                    }
                    let selected = context
                        .catalog_aliases
                        .get(model)
                        .and_then(|models| (models.len() == 1).then(|| models[0].as_str()))
                        .or_else(|| context.family_defaults.get(model))
                        .unwrap_or(model);
                    context.route.model == selected
                })
                .collect();
            match candidates.as_slice() {
                [context] => Ok((**context).clone()),
                [] => Err(ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    reason: "not configured".into(),
                }),
                contexts => Err(ModelResolutionError::AmbiguousRoute {
                    model: model.into(),
                    profiles: contexts
                        .iter()
                        .filter_map(|context| context.route.profile.clone())
                        .collect(),
                }),
            }
        }
    }

    fn test_routes() -> Arc<TestRoutes> {
        let mut contexts = Vec::new();
        for (profile, provider, models, fast) in [
            (
                "configured",
                ModelProviderKind::Other,
                vec!["evaluator-model"],
                None,
            ),
            (
                "anthropic",
                ModelProviderKind::FirstParty,
                vec!["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-8"],
                Some("claude-haiku-4-5"),
            ),
            (
                "deployment",
                ModelProviderKind::FirstParty,
                vec!["native-main", "native-fast"],
                Some("native-fast"),
            ),
            (
                "native-only",
                ModelProviderKind::FirstParty,
                vec!["only-main"],
                None,
            ),
            (
                "native-small-fast",
                ModelProviderKind::FirstParty,
                vec!["override-main", "configured-small-fast", "native-fallback"],
                Some("native-fallback"),
            ),
            (
                "broken-small-fast",
                ModelProviderKind::FirstParty,
                vec!["broken-main"],
                None,
            ),
            (
                "deepseek:cn",
                ModelProviderKind::Other,
                vec!["deepseek-flash"],
                None,
            ),
            (
                "gateway",
                ModelProviderKind::Other,
                vec!["claude-unrelated", "foreign-fast"],
                Some("foreign-fast"),
            ),
            (
                "left",
                ModelProviderKind::Other,
                vec!["shared-model", "balanced-left"],
                None,
            ),
            (
                "right",
                ModelProviderKind::Other,
                vec!["shared-model", "balanced-right"],
                None,
            ),
        ] {
            for model in models {
                let mut catalog_aliases = std::collections::BTreeMap::new();
                if profile == "left" || profile == "right" {
                    catalog_aliases.insert("review".into(), vec![format!("balanced-{profile}")]);
                }
                let small_fast = match profile {
                    "native-small-fast" => Some("configured-small-fast"),
                    "broken-small-fast" => Some("balanced-right"),
                    "gateway" => Some("foreign-fast"),
                    _ => None,
                };
                if let Some(model) = small_fast {
                    catalog_aliases.insert("small-fast".into(), vec![model.into()]);
                }
                contexts.push(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.into(),
                        profile: Some(profile.into()),
                        provider: Some(provider),
                        ..Default::default()
                    },
                    family_defaults: FamilyModelDefaults {
                        haiku: fast.map(str::to_owned),
                        ..Default::default()
                    },
                    catalog_aliases,
                    native_1m: Some(model == "claude-opus-4-8"),
                    ..Default::default()
                });
            }
        }
        Arc::new(TestRoutes(contexts))
    }

    fn test_runner(api: Arc<dyn OrchestratorApiClient>) -> ApiClientHookPromptRunner {
        let runner = ApiClientHookPromptRunner::new(api, test_routes());
        runner.attach_session_model(Arc::new(FixedSession(
            "evaluator-model".into(),
            Some("configured".into()),
        )));
        runner
    }

    fn make_text_response(body: &str) -> HistoryResponse {
        HistoryResponse {
            id: "msg_1".into(),
            model: "claude-haiku-4-5".into(),
            content: vec![LlmContentBlock::Text {
                text: body.into(),
                cache_control: None, citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: Usage::default(),
            cost: None,
            provider_metadata: serde_json::Value::Null,
        }
    }

    /// Records each `messages_create` call and returns a scripted response.
    struct MockApi {
        recorded: Mutex<Vec<RecordedCall>>,
        response: Mutex<Option<Result<HistoryResponse, LlmError>>>,
    }
    impl MockApi {
        fn text(model_echo: &str, body: &str) -> Arc<Self> {
            let _ = model_echo;
            Arc::new(Self {
                recorded: Mutex::new(Vec::new()),
                response: Mutex::new(Some(Ok(make_text_response(body)))),
            })
        }
    }
    #[async_trait]
    impl OrchestratorApiClient for MockApi {
        async fn messages_create(
            &self,
            request: crate::OrchestratorApiRequest,
        ) -> Result<HistoryResponse, LlmError> {
            let (request_model, request_profile, request_system, msgs, _tools) = match request {
                crate::OrchestratorApiRequest::Main(request) => (
                    request.model,
                    request.profile,
                    request.system.map(|system| system.display_text()),
                    request.messages,
                    request.tools,
                ),
                crate::OrchestratorApiRequest::HookPrompt(request) => (
                    request.model,
                    request.profile,
                    Some(request.system),
                    request.messages,
                    Vec::new(),
                ),
            };
            let model = request_model.as_str();
            let profile = request_profile.as_deref();
            let system = request_system.as_deref();

            self.recorded.lock().unwrap().push((
                model.to_string(),
                profile.map(str::to_owned),
                system.map(str::to_owned),
                msgs,
            ));
            self.response
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Err(LlmError::Transport {
                    message: "exhausted".into(),
                }))
        }
    }

    fn req(prompt: &str, model: Option<&str>) -> PromptHookRequest {
        PromptHookRequest {
            model_selection: None,
            transcript: None,
            transcript_path: None,
            prompt: prompt.into(),
            system_prompt: "SYS".into(),
            model: model.map(str::to_owned),
            timeout: std::time::Duration::from_secs(30),
        }
    }

    #[test]
    fn transcript_content_estimator_matches_c0_utf16_and_media() {
        assert_eq!(estimate_hook_content(&serde_json::json!("😀😀😀")), 2);
        assert_eq!(
            estimate_hook_content(
                &serde_json::json!([{"type":"image"},{"type":"tool_result","content":"123456"}])
            ),
            2002
        );
    }

    #[tokio::test]
    async fn transcript_reader_replays_host_path_and_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let sid = uuid::Uuid::new_v4().to_string();
        let first = uuid::Uuid::new_v4().to_string();
        let second = uuid::Uuid::new_v4().to_string();
        let rows = [
            serde_json::json!({"type":"user","uuid":first,"parentUuid":null,"sessionId":sid,"message":{"role":"user","content":"run tests"}}),
            serde_json::json!({"type":"assistant","uuid":second,"parentUuid":first,"sessionId":sid,"message":{"role":"assistant","content":[{"type":"text","text":"tests passed"}],"usage":{"input_tokens":10,"output_tokens":5}}}),
        ];
        tokio::fs::write(
            &path,
            rows.iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .unwrap();
        let transcript = load_hook_transcript(Some(&path)).await.unwrap();
        let messages = transcript.messages;
        let usage = transcript.last_usage_tokens;
        assert_eq!(messages.len(), 2);
        assert_eq!(usage, 15);
        assert!(
            matches!(&messages[1], ConversationMessage::Assistant { content, .. } if matches!(&content[0], lingxi_core::types::ContentBlock::Text { text, .. } if text == "tests passed"))
        );
    }

    #[test]
    fn transcript_budget_keeps_last_assistant_and_its_tool_results() {
        let messages = vec![
            ConversationMessage::user(MessageId::new(), "old".repeat(100)),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "latest".into(), citations: None,
                }],
                stop_reason: None,
            },
            ConversationMessage::user(MessageId::new(), "tool result".into()),
        ];
        let kept = bound_hook_transcript(&messages, 1, &Default::default());
        assert_eq!(kept.len(), 3);
        assert!(matches!(&kept[1], ConversationMessage::Assistant { .. }));
        let ConversationMessage::User { content, .. } = &kept[0] else {
            panic!("truncation preface")
        };
        assert!(
            matches!(&content[0], lingxi_core::types::ContentBlock::Text { text, .. } if text.contains("1 earlier messages omitted"))
        );
        assert!(bound_hook_transcript(&[], 1, &Default::default()).is_empty());
    }

    #[test]
    fn transcript_budget_preserves_split_assistant_identity_across_tool_results() {
        let id = MessageId::new();
        let assistant = |text: &str| ConversationMessage::Assistant { per_turn_effort: None,
            id,
            content: vec![lingxi_core::types::ContentBlock::Text {
                text: text.into(),
                citations: None,
            }],
            stop_reason: None,
        };
        let messages = vec![
            ConversationMessage::user(MessageId::new(), "old".repeat(100)),
            assistant("first split block"),
            ConversationMessage::user(MessageId::new(), "result".into()),
            assistant("second split block"),
        ];
        let kept = bound_hook_transcript(&messages, 1, &Default::default());
        assert_eq!(kept.len(), 4);
        assert_eq!(&kept[1..], &messages[1..]);
    }

    #[test]
    fn resumed_and_virtual_rows_keep_original_grouping_after_resume_projection() {
        let sid = uuid::Uuid::new_v4().to_string();
        let ids: Vec<_> = (0..4).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let mut rows = vec![
            serde_json::json!({"type":"user","uuid":ids[0],"sessionId":sid,"message":{"role":"user","content":"old".repeat(100)}}),
        ];
        for (index, (virtual_row, resumed)) in [(false, false), (true, false), (false, true)]
            .into_iter()
            .enumerate()
        {
            rows.push(serde_json::json!({"type":"assistant","uuid":ids[index + 1],"sessionId":sid,"isVirtual":virtual_row,"resumedFromIncompleteThinking":resumed,"message":{"id":ids[index + 1],"role":"assistant","content":[{"type":"text","text":"part"}]}}));
        }
        let loaded = session::jsonl::reader::route_lines(
            &rows
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let state =
            crate::resume::state_from_messages(uuid::Uuid::nil(), &loaded.messages_in_order);
        assert_eq!(state.hook_message_grouping.len(), 2);
        let kept = bound_hook_transcript(&state.history, 1, &state.hook_message_grouping);
        assert_eq!(kept.len(), 4);
        assert_eq!(&kept[1..], &state.history[1..]);
    }

    #[test]
    fn evaluator_budget_uses_host_capability_facts() {
        let mut context = ModelResolutionContext::default();
        assert_eq!(hook_transcript_budget(&context), 100_000);
        context.native_1m = Some(true);
        assert_eq!(hook_transcript_budget(&context), 500_000);
        context.disable_1m_context = true;
        assert_eq!(hook_transcript_budget(&context), 100_000);
    }

    #[tokio::test]
    async fn live_transcript_wins_over_unreadable_persisted_path() {
        let api = MockApi::text("x", r#"{"ok":true}"#);
        let runner = test_runner(api.clone());
        let dir = tempfile::tempdir().unwrap();
        let mut request = req("judge", Some("claude-opus-4-8"));
        // A directory is not a readable JSONL file. A live snapshot must never
        // consult it, even when the writer hasn't persisted the new turn yet.
        request.transcript_path = Some(dir.path().to_path_buf());
        let messages = vec![
            ConversationMessage::user(MessageId::new(), "unpersisted evidence".into()),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: "just completed".into(), citations: None,
                }],
                stop_reason: None,
            },
        ];
        request.transcript = Some(hooks::PromptHookTranscript {
            messages: messages.clone(),
            last_usage_tokens: 300_000,
            ..Default::default()
        });
        runner.run(request).await.unwrap();
        let recorded = api.recorded.lock().unwrap();
        assert_eq!(&recorded[0].3[..2], messages.as_slice());
        assert_eq!(
            recorded[0].3.len(),
            3,
            "native 1M keeps the full transcript at 300k usage"
        );
    }

    #[tokio::test]
    async fn run_calls_messages_create_with_prompt_and_system_and_extracts_text() {
        let api = MockApi::text("claude-haiku-4-5", r#"{"ok": true}"#);
        let runner = test_runner(api.clone());

        let out = runner.run(req("is this safe?", None)).await.unwrap();

        assert_eq!(out, r#"{"ok": true}"#);
        let recorded = api.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let (model, profile, system, msgs) = &recorded[0];
        assert_eq!(model, "evaluator-model");
        assert_eq!(profile.as_deref(), Some("configured"));
        assert_eq!(system.as_deref(), Some("SYS"));
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ConversationMessage::User { content, .. } => {
                assert_eq!(
                    content,
                    &vec![lingxi_core::types::ContentBlock::Text {
                        text: "is this safe?".into(), citations: None
                    }]
                );
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_override_is_passed_through() {
        let api = MockApi::text("x", r#"{"ok": true}"#);
        let runner = test_runner(api.clone());

        let _ = runner
            .run(req("p", Some("claude-sonnet-4-6")))
            .await
            .unwrap();

        let recorded = api.recorded.lock().unwrap();
        assert_eq!(recorded[0].0, "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn timeout_error_maps_to_prompt_timeout() {
        let api = Arc::new(MockApi {
            recorded: Mutex::new(Vec::new()),
            response: Mutex::new(Some(Err(LlmError::Transport {
                message: "request timeout after 30s".into(),
            }))),
        });
        let runner = test_runner(api);

        let err = runner.run(req("p", None)).await.unwrap_err();
        assert!(matches!(err, PromptHookError::Timeout(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn other_error_maps_to_query_error() {
        let api = Arc::new(MockApi {
            recorded: Mutex::new(Vec::new()),
            response: Mutex::new(Some(Err(LlmError::Authentication {
                message: String::new(),
            }))),
        });
        let runner = test_runner(api);

        let err = runner.run(req("p", None)).await.unwrap_err();
        assert!(matches!(err, PromptHookError::Query(_)), "got {err:?}");
    }

    #[test]
    fn resolve_model_preserves_scoped_alias_and_concrete_overrides() {
        let runner = test_runner(MockApi::text("x", "{}"));
        for (override_model, expected) in [
            ("review", "balanced-left"),
            ("shared-model", "shared-model"),
        ] {
            let selected = runner
                .resolve_model(Some(override_model), Some(("shared-model", Some("left"))))
                .unwrap();
            assert_eq!(selected.model, expected);
            assert_eq!(selected.model_profile.as_deref(), Some("left"));
        }
        let selected = runner
            .resolve_model(
                Some("right/shared-model"),
                Some(("shared-model", Some("left"))),
            )
            .unwrap();
        assert_eq!(selected.model, "shared-model");
        assert_eq!(selected.model_profile.as_deref(), Some("right"));
    }

    #[test]
    fn native_defaults_use_configured_aliases_and_foreign_routes_keep_their_model() {
        let runner = test_runner(MockApi::text("x", "{}"));
        let resolve = |model: &str, profile: Option<&str>| {
            let selected = runner.resolve_model(None, Some((model, profile))).unwrap();
            (selected.model, selected.model_profile)
        };
        assert_eq!(
            resolve("native-main", Some("deployment")),
            ("native-fast".to_string(), Some("deployment".to_string()))
        );
        assert_eq!(
            resolve("only-main", Some("native-only")),
            ("only-main".to_string(), Some("native-only".to_string()))
        );
        assert_eq!(
            resolve("deepseek-flash", Some("deepseek:cn")),
            (
                "deepseek-flash".to_string(),
                Some("deepseek:cn".to_string())
            )
        );
        // Even a Claude-shaped id and a configured Haiku alias do not turn a
        // foreign provider into an Anthropic route.
        assert_eq!(
            resolve("claude-unrelated", None),
            ("claude-unrelated".to_string(), Some("gateway".to_string()))
        );
    }

    #[tokio::test]
    async fn unbound_or_ambiguous_routes_fail_before_model_communication() {
        let api = MockApi::text("x", "{}");
        let runner = ApiClientHookPromptRunner::new(api.clone(), test_routes());
        assert!(matches!(
            runner.run(req("judge", None)).await,
            Err(PromptHookError::Query(_))
        ));
        assert!(matches!(
            runner.run(req("judge", Some("shared-model"))).await,
            Err(PromptHookError::Query(_))
        ));
        assert!(api.recorded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn native_small_fast_prefers_scoped_configuration_over_haiku() {
        let api = MockApi::text("x", "{}");
        let runner = test_runner(api.clone());
        let mut request = req("judge", None);
        request.model_selection = Some(hooks::registry::HookModelSelection {
            model: "override-main".into(),
            model_profile: Some("native-small-fast".into()),
        });
        runner.run(request).await.unwrap();
        let recorded = api.recorded.lock().unwrap();
        assert_eq!(recorded[0].0, "configured-small-fast");
        assert_eq!(recorded[0].1.as_deref(), Some("native-small-fast"));
    }

    #[tokio::test]
    async fn configured_small_fast_cannot_escape_its_native_profile() {
        let api = MockApi::text("x", "{}");
        let runner = test_runner(api.clone());
        let mut request = req("judge", None);
        request.model_selection = Some(hooks::registry::HookModelSelection {
            model: "broken-main".into(),
            model_profile: Some("broken-small-fast".into()),
        });
        assert!(matches!(
            runner.run(request).await,
            Err(PromptHookError::Query(_))
        ));
        assert!(api.recorded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn child_hook_uses_host_route_before_the_root_session() {
        for (model_override, expected_model) in
            [(None, "shared-model"), (Some("review"), "balanced-right")]
        {
            let api = MockApi::text("x", "{}");
            let runner = ApiClientHookPromptRunner::new(api.clone(), test_routes());
            runner.attach_session_model(Arc::new(FixedSession(
                "shared-model".into(),
                Some("left".into()),
            )));
            let mut request = req("judge", model_override);
            request.model_selection = Some(hooks::registry::HookModelSelection {
                model: "shared-model".into(),
                model_profile: Some("right".into()),
            });
            runner.run(request).await.unwrap();
            let recorded = api.recorded.lock().unwrap();
            assert_eq!(recorded[0].0, expected_model);
            assert_eq!(recorded[0].1.as_deref(), Some("right"));
        }
    }

    #[tokio::test]
    async fn a_bound_deepseek_session_evaluates_on_its_own_model_and_profile() {
        let api = MockApi::text("x", r#"{"ok": true}"#);
        let runner = ApiClientHookPromptRunner::new(api.clone(), test_routes());
        runner.attach_session_model(Arc::new(FixedSession(
            "deepseek-flash".to_string(),
            Some("deepseek:cn".to_string()),
        )));

        runner.run(req("judge", None)).await.unwrap();

        let recorded = api.recorded.lock().unwrap();
        assert_eq!(recorded[0].0, "deepseek-flash");
        assert_eq!(recorded[0].1.as_deref(), Some("deepseek:cn"));
    }

    #[tokio::test]
    async fn reasoning_blocks_never_reach_the_evaluator() {
        let api = MockApi::text("x", r#"{"ok": true}"#);
        let runner = test_runner(api.clone());
        let mut request = req("judge", None);
        request.transcript = Some(hooks::PromptHookTranscript {
            messages: vec![
                ConversationMessage::user(MessageId::new(), "run tests".into()),
                ConversationMessage::Assistant { per_turn_effort: None,
                    id: MessageId::new(),
                    content: vec![
                        // A DeepSeek trace: no signature, which the Anthropic
                        // codec refuses outright.
                        lingxi_core::types::ContentBlock::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        },
                        lingxi_core::types::ContentBlock::Text {
                            text: "tests passed".into(), citations: None,
                        },
                    ],
                    stop_reason: None,
                },
                ConversationMessage::user(MessageId::new(), "and again".into()),
                ConversationMessage::Assistant { per_turn_effort: None,
                    id: MessageId::new(),
                    content: vec![
                        lingxi_core::types::ContentBlock::Thinking {
                            thinking: "only thinking".into(),
                            signature: Some("sig".into()),
                        },
                        lingxi_core::types::ContentBlock::RedactedThinking {
                            data: "opaque".into(),
                        },
                    ],
                    stop_reason: None,
                },
            ],
            last_usage_tokens: 10,
            ..Default::default()
        });

        runner.run(request).await.unwrap();

        let recorded = api.recorded.lock().unwrap();
        let sent = &recorded[0].3;
        assert_eq!(
            sent.len(),
            5,
            "4 transcript messages + the condition prompt"
        );
        let assistant_content = |index: usize| match &sent[index] {
            ConversationMessage::Assistant { content, .. } => content.clone(),
            other => panic!("expected assistant at {index}, got {other:?}"),
        };
        assert_eq!(
            assistant_content(1),
            vec![lingxi_core::types::ContentBlock::Text {
                text: "tests passed".into(), citations: None
            }]
        );
        // Signed and redacted traces go too: the evaluator judges text, and a
        // message must not go out empty.
        assert_eq!(
            assistant_content(3),
            vec![lingxi_core::types::ContentBlock::Text {
                text: "[Thinking removed]".into(), citations: None
            }]
        );
    }
}
