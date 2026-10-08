//! Request ownership for thinking-signature recovery. The scope follows a query
//! through retries and lazy streams; sharing a service never shares its history.
use std::collections::HashMap;
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
    recorder: Option<Arc<RecoveryRecorder>>,
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

    /// Replace the latch identity on context reset. An older in-flight request
    /// may settle its captured scope without rejecting betas in the new scope.
    pub fn reset_beta_rejections(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.betas = Default::default();
        state.server_betas = Default::default();
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
