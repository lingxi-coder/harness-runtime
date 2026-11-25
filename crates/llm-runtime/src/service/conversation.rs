//! Conversation and WebSocket continuation leases.

use crate::dispatch_header::DispatchHeaderState;
use crate::service::ApiService;
use crate::{LlmError, LlmRequest, PromptCacheQuerySource};
use lingxi_core::types::ConversationMessage;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct ConversationSlot {
    pub(super) generation: u64,
    pub(super) session: Option<lingxi_llm_client::ConversationSession>,
}

/// Only the owner lease can return a session. Reset/compaction advances the
/// generation so an old stream cannot reinstall retired state on drop.
pub(super) struct ConversationLease {
    session: lingxi_llm_client::ConversationSession,
    slot: Option<Arc<Mutex<ConversationSlot>>>,
    generation: u64,
    pool: Option<Arc<Mutex<Vec<lingxi_llm_client::ConversationSession>>>>,
}
impl ConversationLease {
    pub(super) fn checkout(slot: &Arc<Mutex<ConversationSlot>>) -> Self {
        let mut state = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.session.take() {
            Some(session) => Self {
                session,
                slot: Some(slot.clone()),
                generation: state.generation,
                pool: None,
            },
            None => Self::independent(),
        }
    }
    pub(super) fn independent() -> Self {
        Self {
            session: Default::default(),
            slot: None,
            generation: 0,
            pool: None,
        }
    }
    pub(super) fn auxiliary(
        pool: &Arc<Mutex<Vec<lingxi_llm_client::ConversationSession>>>,
    ) -> Self {
        let mut session = pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .unwrap_or_default();
        session.clear_history_preserving_transport();
        Self {
            session,
            slot: None,
            generation: 0,
            pool: Some(pool.clone()),
        }
    }
    pub(super) fn session(&mut self) -> &mut lingxi_llm_client::ConversationSession {
        &mut self.session
    }
}
impl Drop for ConversationLease {
    fn drop(&mut self) {
        if let Some(pool) = &self.pool {
            self.session.clear_history_preserving_transport();
            let mut pool = pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pool.len() < 4 {
                pool.push(std::mem::take(&mut self.session));
            }
        }

        if let Some(slot) = &self.slot {
            let mut state = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.generation == self.generation {
                state.session = Some(std::mem::take(&mut self.session));
            }
        }
    }
}

impl ApiService {
    // ── OpenAI Responses WebSocket preconnect ────────────────────────────────

    /// Best-effort startup preconnect for OpenAI Responses WebSocket profiles.
    ///
    /// This opens the WebSocket handshake only; no prompt payload is sent.
    /// Callers intentionally ignore failures so normal HTTP/SSE or later WS
    /// connect paths remain authoritative.
    pub async fn preconnect_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<(), LlmError> {
        let mut request = LlmRequest::new(model);
        request.profile = profile.map(str::to_string);
        request.stream = true;
        let mut lease = ConversationLease::checkout(&self.responses_ws_session);
        let session = lease.session();
        self.client()
            .preconnect_websocket(
                &request,
                self.transport.clone(),
                session.responses_transport_mut(),
            )
            .await
    }

    /// Spawn [`Self::preconnect_responses_websocket`] on the current runtime and
    /// discard errors. Intended for engine startup where latency reduction must
    /// never block session initialization.
    pub fn spawn_responses_websocket_preconnect(
        self: &Arc<Self>,
        model: String,
        profile: Option<String>,
    ) {
        let adapter = Arc::clone(self);
        tokio::spawn(async move {
            let _ = adapter
                .preconnect_responses_websocket(&model, profile.as_deref())
                .await;
        });
    }

    /// Check startup prewarm eligibility before assembling the prompt and tools.
    pub fn supports_responses_websocket_prewarm(&self, model: &str, profile: Option<&str>) -> bool {
        self.client()
            .supports_responses_websocket(model, profile)
            .unwrap_or(false)
    }

    /// Best-effort startup **prewarm** for OpenAI Responses WebSocket providers:
    /// send the provided (empty-history) request with `generate=false` over the
    /// session so the handshake + first round-trip are warm. Backs the
    /// `OrchestratorApiClient::prewarm_responses_websocket` trait override.
    pub async fn prewarm_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&crate::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<crate::ToolDeclaration>,
        skip_global_cache_for_system_prompt: bool,
    ) -> Result<(), LlmError> {
        // An optional latency optimization must not send an unmetered side
        // request for an invocation with a hard generation ceiling.
        if self.scope_output_quota.is_some() {
            return Ok(());
        }
        // Account selection belongs to the next real business request. A
        // speculative prewarm must neither consume its recovery probe lease
        // nor establish a socket authenticated to an unselected base profile.
        if self.client().account_coordinator(model, profile)?.is_some() {
            return Ok(());
        }
        let mut req = self.build_request(
            model,
            profile,
            system,
            messages,
            tools,
            true,
            None,
            skip_global_cache_for_system_prompt,
            PromptCacheQuerySource::Unspecified,
        )?;
        self.refresh_effort_settings(&mut req)?;
        req.execution.resolve_native_effort = true;
        let request_client = self.client();
        let mut prepared = request_client
            .prepare_on(&req, self.transport.clone(), Default::default())
            .await?;
        // Responses-WebSocket prewarm only — never a first-party Anthropic
        // route, so the dispatch gate is inert here.
        self.inject_stream_headers(&mut prepared, DispatchHeaderState::default())?;
        let mut lease = ConversationLease::checkout(&self.responses_ws_session);
        let session = lease.session();
        request_client
            .prewarm_prepared_websocket(
                prepared,
                self.transport.clone(),
                session.responses_transport_mut(),
            )
            .await
    }

    /// Close any reusable Responses WebSocket session held by this service. Backs
    /// the `OrchestratorApiClient::close_responses_websocket_session` trait
    /// override.
    pub async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        let mut session = self.retire_conversation_context();
        session.close().await.map_err(crate::upstream::error)
    }

    fn retire_conversation_context(&self) -> lingxi_llm_client::ConversationSession {
        *self
            .history_projection_cache
            .lock()
            .expect("history projection cache") = Default::default();
        let mut slot = self
            .responses_ws_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.generation = slot.generation.wrapping_add(1);
        slot.session.replace(Default::default()).unwrap_or_default()
    }

    /// Invalidate derived history and remote continuation at a committed history
    /// rewrite. In-flight leases cannot repopulate the new generation.
    pub fn reset_conversation_context(&self) {
        drop(self.retire_conversation_context());
    }
}
