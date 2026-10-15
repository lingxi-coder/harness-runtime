//! Request ownership for thinking-signature recovery. The scope follows a query
//! through retries and lazy streams; sharing a service never shares its history.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    server_betas: HashMap<
        (String, String, crate::ProtocolFamily),
        lingxi_llm_client::providers::anthropic::fallback_request::ServerBetaState,
    >,
    betas: lingxi_llm_client::providers::anthropic::beta_repair::ConversationBetaState,
    marked: HashMap<lingxi_core::types::MessageId, usize>,
    pending_snapshot: bool,
    stripped: bool,
    native_tool_search: NativeToolSearchState,
    native_thinking_binding_controls: NativeThinkingBindingControlsAttempt,
    context_1m_refused_models: HashSet<Vec<u16>>,
    recorder: Option<Arc<RecoveryRecorder>>,
}

/// Conversation-local Native tool-change latches used by `pfr` and the
/// deferred-tool request normalizer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeToolSearchState {
    /// A main request was actually dispatched with the MCTC beta header.
    pub mid_conversation_tool_changes_sent: bool,
    /// Native classified a sent tool-change header/block as refused.
    pub tool_change_header_refused: bool,
}

/// Conversation-local Native TBC membership. Native records the beta in its
/// conversation `nt` set during request construction; later disabled-thinking
/// requests can retain membership until rejection or context reset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeThinkingBindingControlsState {
    /// A request previously registered this beta in the conversation set.
    pub beta_sent: bool,
    /// The captured conversation state rejected this beta.
    pub beta_rejected: bool,
}

/// Captured Native `nt` identity for one logical query. Reset replaces the
/// active handle, so a response from an older in-flight query can mutate only
/// the state it captured before the reset.
#[derive(Clone, Default)]
pub struct NativeThinkingBindingControlsAttempt(Arc<Mutex<NativeThinkingBindingControlsState>>);

impl NativeThinkingBindingControlsAttempt {
    #[must_use]
    pub fn state(&self) -> NativeThinkingBindingControlsState {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Register Native `yD(nt, uB)` before dispatch on this captured query.
    pub fn mark_beta_sent(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .beta_sent = true;
    }

    /// Apply Native `Wm(nt, uB)` to this captured query, never to a newer
    /// active conversation state installed by reset.
    pub fn refuse_beta(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.beta_sent = false;
        state.beta_rejected = true;
    }
}

/// Cloneable identity owned by one conversation or worker query.
#[derive(Clone, Default)]
pub struct ThinkingRecoveryScope(Arc<Mutex<State>>);

/// Durable recorder invoked before retrying a rejected history snapshot.
pub type RecoveryRecorder = dyn Fn(HashMap<lingxi_core::types::MessageId, usize>) -> crate::BoxFuture<'static, ()>
    + Send
    + Sync;

impl std::fmt::Debug for ThinkingRecoveryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThinkingRecoveryScope")
            .field("messages", &self.messages())
            .finish_non_exhaustive()
    }
}

impl PartialEq for ThinkingRecoveryScope {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod beta_tests {
    use super::*;
    use lingxi_llm_client::providers::anthropic::{
        beta_repair::Beta,
        fallback_request::LaneMode,
        thinking_display::{DisplayProbe, DisplayProbeBudget, ProbeAdmission},
    };
    #[test]
    fn context_reset_detaches_inflight_beta_commit_without_resetting_signature_history() {
        let scope = ThinkingRecoveryScope::default();
        scope.arm(true);
        let captured = scope.beta_rejections();
        let mut probe = DisplayProbe::default();
        assert!(probe.on_error(
            400,
            ProbeAdmission {
                header_sent: true,
                ..Default::default()
            },
            &DisplayProbeBudget::default()
        ));
        scope.reset_beta_rejections();
        assert!(probe.on_success(&captured));
        captured.reject(Beta::ThinkingTokenCount);
        assert!(captured.rejected(Beta::ThinkingDisplayUpdates));
        assert!(!scope
            .beta_rejections()
            .rejected(Beta::ThinkingDisplayUpdates));
        assert!(!scope.beta_rejections().rejected(Beta::ThinkingTokenCount));
        assert!(scope.stripped());
    }

    #[test]
    fn server_fallback_beta_latches_partition_and_detach_on_context_reset() {
        let scope = ThinkingRecoveryScope::default();
        let provider = crate::ProviderId::AnthropicFirstParty;
        let protocol = crate::ProtocolFamily::AnthropicMessages;
        let direct = scope.server_fallback_betas(&provider, "direct", protocol);
        direct.reject(LaneMode::Default);

        assert!(direct.snapshot().default_rejected);
        assert!(
            !scope
                .server_fallback_betas(&provider, "alternate", protocol)
                .snapshot()
                .default_rejected,
            "profiles own separate server-fallback beta latches"
        );
        assert!(
            !scope
                .server_fallback_betas(
                    &crate::ProviderId::Custom {
                        name: "gateway".into(),
                    },
                    "direct",
                    protocol,
                )
                .snapshot()
                .default_rejected,
            "providers own separate server-fallback beta latches"
        );
        assert!(
            !scope
                .server_fallback_betas(&provider, "direct", crate::ProtocolFamily::FoundryClaude,)
                .snapshot()
                .default_rejected,
            "protocols own separate server-fallback beta latches"
        );

        scope.reset_beta_rejections();
        assert!(direct.snapshot().default_rejected);
        assert!(
            !scope
                .server_fallback_betas(&provider, "direct", protocol)
                .snapshot()
                .default_rejected,
            "a context reset detaches old in-flight handles"
        );
    }

    #[test]
    fn beta_rejection_latches_detach_on_context_reset() {
        let scope = ThinkingRecoveryScope::default();
        scope
            .beta_rejections()
            .reject(Beta::MidConversationToolChanges);
        scope.beta_rejections().reject(Beta::AfkMode);
        assert!(scope.beta_rejected(Beta::MidConversationToolChanges));
        assert!(scope.beta_rejected(Beta::AfkMode));

        scope.reset_beta_rejections();
        assert!(!scope.beta_rejected(Beta::MidConversationToolChanges));
        assert!(!scope.beta_rejected(Beta::AfkMode));
    }

    #[test]
    fn native_tool_search_latches_require_dispatch_and_reset_with_the_conversation() {
        let scope = ThinkingRecoveryScope::default();
        assert_eq!(
            scope.native_tool_search_state(),
            NativeToolSearchState::default()
        );
        scope.mark_mid_conversation_tool_changes_sent();
        assert!(
            scope
                .native_tool_search_state()
                .mid_conversation_tool_changes_sent
        );
        scope.refuse_mid_conversation_tool_changes();
        assert!(scope.native_tool_search_state().tool_change_header_refused);

        scope.reset_beta_rejections();
        assert_eq!(
            scope.native_tool_search_state(),
            NativeToolSearchState::default()
        );
    }

    #[test]
    fn native_tbc_planned_membership_and_rejection_reset_with_the_conversation() {
        let scope = ThinkingRecoveryScope::default();
        let same = scope.clone();
        let next_conversation = ThinkingRecoveryScope::default();
        assert!(!scope.native_thinking_binding_controls_state().beta_sent);

        same.mark_native_thinking_binding_controls_beta_sent();
        assert!(scope.native_thinking_binding_controls_state().beta_sent);
        scope.refuse_native_thinking_binding_controls_beta();
        assert!(!scope.native_thinking_binding_controls_state().beta_sent);
        assert!(scope.beta_rejected(Beta::ThinkingBindingControls));
        assert!(
            !next_conversation
                .native_thinking_binding_controls_state()
                .beta_sent
        );

        scope.reset_beta_rejections();
        assert!(!scope.native_thinking_binding_controls_state().beta_sent);
        assert!(!scope.beta_rejected(Beta::ThinkingBindingControls));
    }

    #[test]
    fn late_tbc_refusal_after_reset_only_mutates_the_captured_query() {
        let scope = ThinkingRecoveryScope::default();
        scope.mark_native_thinking_binding_controls_beta_sent();
        let in_flight_nt = scope.native_thinking_binding_controls_attempt();

        // Compaction/reset replaces Native's active sticky-beta object while
        // the provider response is still pending.
        scope.reset_beta_rejections();

        // The old response then completes with a definite TBC refusal. Wm
        // applies to its captured `nt`, not the newly active conversation.
        scope.refuse_native_thinking_binding_controls_beta_for_attempt(&in_flight_nt);
        assert!(!in_flight_nt.state().beta_sent);
        assert!(in_flight_nt.state().beta_rejected);
        assert!(!scope.native_thinking_binding_controls_state().beta_sent);
        assert!(!scope.beta_rejected(Beta::ThinkingBindingControls));

        // A fresh query captures the reset state and can register TBC again.
        let fresh_nt = scope.native_thinking_binding_controls_attempt();
        fresh_nt.mark_beta_sent();
        assert!(scope.native_thinking_binding_controls_state().beta_sent);
        assert!(!scope.beta_rejected(Beta::ThinkingBindingControls));
    }

    #[test]
    fn context_1m_refusal_latch_uses_the_current_scope_after_reset() {
        let scope = ThinkingRecoveryScope::default();
        let key = "claude-sonnet-4-20250514"
            .encode_utf16()
            .collect::<Vec<_>>();
        let same = scope.clone();
        let next_conversation = ThinkingRecoveryScope::default();

        assert!(!scope.context_1m_refused(&key));
        same.mark_context_1m_refused(&key);
        assert!(scope.context_1m_refused(&key));
        assert!(!next_conversation.context_1m_refused(&key));

        scope.reset_beta_rejections();
        assert!(!same.context_1m_refused(&key));

        // Native `iEr` resolves the host latch when the response settles, so
        // an older query may mark the replacement set after reset.
        same.mark_context_1m_refused(&key);
        assert!(scope.context_1m_refused(&key));
    }
}

impl ThinkingRecoveryScope {
    /// Server betas follow a conversation and its provider profile. This keeps
    /// the authorized multi-provider picker from moving first-party latches.
    pub fn server_fallback_betas(
        &self,
        provider: &crate::ProviderId,
        profile: &str,
        protocol: crate::ProtocolFamily,
    ) -> lingxi_llm_client::providers::anthropic::fallback_request::ServerBetaState {
        let key = (
            serde_json::to_string(provider).expect("provider identity is JSON"),
            profile.to_owned(),
            protocol,
        );
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .server_betas
            .entry(key)
            .or_default()
            .clone()
    }

    /// SDK display rejection belongs to this conversation, independently of
    /// signature stripping and the lifetime of a shared API service.
    pub fn beta_rejections(
        &self,
    ) -> lingxi_llm_client::providers::anthropic::beta_repair::ConversationBetaState {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .betas
            .clone()
    }

    /// Current query-scoped latches used by Native `pfr`/tool-search flow.
    #[must_use]
    pub fn native_tool_search_state(&self) -> NativeToolSearchState {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .native_tool_search
            .clone()
    }

    /// Record the first actual dispatch that carried MCTC. Header computation
    /// or preparation alone is not sufficient to advance Native's `sent` set.
    pub fn mark_mid_conversation_tool_changes_sent(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .native_tool_search
            .mid_conversation_tool_changes_sent = true;
    }

    /// Retire the mid-conversation tool-change fallback after Native's exact
    /// header/block refusal classifier reports a refusal for a sent change.
    pub fn refuse_mid_conversation_tool_changes(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .native_tool_search
            .tool_change_header_refused = true;
    }

    /// Snapshot Native's conversation `nt` membership for the TBC beta.
    #[must_use]
    pub fn native_thinking_binding_controls_state(&self) -> NativeThinkingBindingControlsState {
        self.native_thinking_binding_controls_attempt().state()
    }

    /// Capture the active sticky-beta object when constructing a logical
    /// request. Later resets detach this handle from newly started requests.
    #[must_use]
    pub fn native_thinking_binding_controls_attempt(&self) -> NativeThinkingBindingControlsAttempt {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .native_thinking_binding_controls
            .clone()
    }

    /// Register planned Native TBC membership before the request draft is
    /// sealed. This mirrors `yD(nt, uB)` and is not a dispatch-success marker.
    pub fn mark_native_thinking_binding_controls_beta_sent(&self) {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.native_thinking_binding_controls.mark_beta_sent();
    }

    /// Apply Native `Wm(nt, uB)`: a refused TBC is removed from the sent set
    /// and retained in the conversation rejection set.
    pub fn refuse_native_thinking_binding_controls_beta(&self) {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.native_thinking_binding_controls.refuse_beta();
        state.betas.reject(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls,
        );
    }

    /// Apply a late query refusal to its captured Native `nt`. Mirror the
    /// rejection into the currently active shared beta state only if reset
    /// has not replaced that identity in the meantime.
    pub(crate) fn refuse_native_thinking_binding_controls_beta_for_attempt(
        &self,
        attempt: &NativeThinkingBindingControlsAttempt,
    ) {
        attempt.refuse_beta();
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::ptr_eq(&state.native_thinking_binding_controls.0, &attempt.0) {
            state.betas.reject(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingBindingControls,
            );
        }
    }

    /// Whether Native previously completed a successful no-context-1m retry
    /// for this exact `Fn(model)` UTF-16 key in the current conversation.
    #[must_use]
    pub fn context_1m_refused(&self, model_key: &[u16]) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .context_1m_refused_models
            .contains(model_key)
    }

    /// Commit Native's ordinary context-1m refusal only after a successful
    /// non-stream response or the first accepted streaming MessageStart.
    pub fn mark_context_1m_refused(&self, model_key: &[u16]) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .context_1m_refused_models
            .insert(model_key.to_vec());
    }

    /// Whether this conversation has observed a rejection for one of its
    /// computed Anthropic beta headers.
    #[must_use]
    pub fn beta_rejected(
        &self,
        beta: lingxi_llm_client::providers::anthropic::beta_repair::Beta,
    ) -> bool {
        self.beta_rejections().rejected(beta)
    }

    /// Replace the latch identity on context reset. An older in-flight request
    /// may settle its captured scope without rejecting betas in the new scope.
    pub fn reset_beta_rejections(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.betas = Default::default();
        state.server_betas = Default::default();
        state.native_tool_search = Default::default();
        // Native reset installs a new sticky-beta object. Existing requests
        // retain the old handle, just as their captured `nt` does.
        state.native_thinking_binding_controls = Default::default();
        state.context_1m_refused_models.clear();
    }

    /// Snapshot of rejected historical block ranges.
    pub fn messages(&self) -> HashMap<lingxi_core::types::MessageId, usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .marked
            .clone()
    }

    /// Merge restored or newly rejected ranges, preserving the earliest index.
    pub fn merge(&self, messages: HashMap<lingxi_core::types::MessageId, usize>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (id, from) in messages {
            state
                .marked
                .entry(id)
                .and_modify(|v| *v = (*v).min(from))
                .or_insert(from);
        }
        state.stripped = !state.marked.is_empty();
        state.pending_snapshot = false;
    }

    pub(crate) fn rejected(&self, messages: HashMap<lingxi_core::types::MessageId, usize>) {
        self.merge(messages);
        self.0.lock().unwrap_or_else(|e| e.into_inner()).stripped = true;
    }

    /// Attach this query's transcript writer. Never copied into side queries.
    pub fn set_recorder(&self, recorder: Arc<RecoveryRecorder>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).recorder = Some(recorder);
    }

    pub(crate) async fn persist(&self) {
        let (recorder, messages) = {
            let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            (state.recorder.clone(), state.marked.clone())
        };
        if let Some(recorder) = recorder {
            recorder(messages).await;
        }
    }

    pub(crate) fn stripped(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).stripped
    }

    pub(crate) fn arm(&self, stripped: bool) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.stripped = stripped;
        state.pending_snapshot = stripped;
        if !stripped {
            state.marked.clear();
        }
    }

    pub(crate) fn capture(&self, ids: &[lingxi_core::types::MessageId]) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending_snapshot {
            state.pending_snapshot = false;
            state.marked.extend(ids.iter().map(|id| (*id, 0)));
        }
    }
}

tokio::task_local! {
    static QUERY_SCOPE: ThinkingRecoveryScope;
}

/// Run provider operations under an explicitly owned query context. A request
/// captures this handle before returning a lazy stream, so polling may happen
/// outside the task-local scope without changing recovery ownership.
pub async fn scope_thinking_recovery<F: std::future::Future>(
    scope: ThinkingRecoveryScope,
    future: F,
) -> F::Output {
    QUERY_SCOPE.scope(scope, future).await
}

/// The active query handle, when the caller established an explicit scope.
pub fn current() -> Option<ThinkingRecoveryScope> {
    QUERY_SCOPE.try_with(Clone::clone).ok()
}

/// Assemble a side query against a copy of its parent's existing rejections.
/// New failures belong only to that request, even when history IDs are shared.
pub(crate) fn isolated<F: FnOnce() -> R, R>(build: F) -> R {
    let scope = ThinkingRecoveryScope::default();
    if let Some(parent) = current() {
        scope.merge(parent.messages());
    }
    QUERY_SCOPE.sync_scope(scope, build)
}
