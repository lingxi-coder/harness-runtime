//! Main-query control for admitted first-party server fallback observations.
//!
//! The SDK owns decoding and lane admission. This module only applies host
//! policy, query accounting, and session model selection after that boundary.

use crate::conversation::ConversationOrchestrator;
use crate::error::OrchestratorError;
use std::future::Future;

/// Result returned to the stream pump before it dispatches any received-model
/// content or tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerFallbackAdmission {
    /// The event passed host policy and its session/query effects were applied.
    Applied,
    /// Host model policy rejected the received model; the response must be
    /// discarded without partial-response salvage.
    Declined,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QueryState {
    /// Number of visible refusal/sticky server fallbacks consumed by this
    /// query. It continues increasing after the `>= 2` future-request gate;
    /// the controller does not cap received events at two.
    pub(crate) fallback_count: usize,
    /// Once a server-selected model is declined, later requests in this query
    /// cannot ask the server fallback lane again.
    pub(crate) declined: bool,
    /// Logical model/profile used by the physical request currently being
    /// processed. `None` profile means ordinary provider resolution.
    request_route: Option<crate::query_model::ModelRoute>,
    /// Latest query-local server route (`ge`/`jn`) for this physical request.
    /// This advances for child queries too, independently of session state.
    applied_route: Option<crate::query_model::ModelRoute>,
    /// Text policy is frozen before dispatch/model mutation, independently of
    /// the provider response's event-specific refusal category and IDs.
    request_refusal_text: Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>,
    applied_refusal_text: Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>,
    /// Native `Q`: the latest queued model-refusal notice, flushed by `He()`
    /// at a terminal, decline, or catch boundary. The host-only notice
    /// metadata preserves Native's enqueue-time timestamp for the JSONL row.
    pending_notice: Option<lingxi_core::types::ConversationMessage>,
}

tokio::task_local! {
    static QUERY_STATE: std::sync::Mutex<QueryState>;
}

/// Give one top-level main query its own fallback counter and decline latch.
pub(crate) async fn scope_query<F: Future>(future: F) -> F::Output {
    QUERY_STATE
        .scope(std::sync::Mutex::new(QueryState::default()), future)
        .await
}

/// Run one main query with a final queued-notice flush. Normal success,
/// decline, and recovery paths flush at their native ordering point; this
/// wrapper is the catch/early-return safety net.
pub(crate) async fn scope_query_and_flush<F: Future>(
    orch: &ConversationOrchestrator,
    future: F,
) -> F::Output {
    scope_query(async {
        let result = future.await;
        flush_pending_notice(orch).await;
        result
    })
    .await
}

/// Facts consumed while building the next server-lane request policy.
pub(crate) fn query_state() -> QueryState {
    QUERY_STATE
        .try_with(|state| {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .unwrap_or_default()
}

/// Snapshot the logical route immediately before each physical provider call.
/// A fresh request starts from its selected model, independently of the
/// session state observed when a later server fallback arrives.
pub(crate) fn record_request_route(route: crate::query_model::ModelRoute) {
    let _ = QUERY_STATE.try_with(|state| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.request_route = Some(route);
        state.applied_route = None;
        state.request_refusal_text = None;
        state.applied_refusal_text = None;
    });
}

/// Capture refusal presentation from the same route as the physical call.
pub(crate) fn record_request_route_with_refusal_text(
    orch: &ConversationOrchestrator,
    route: crate::query_model::ModelRoute,
) {
    let text = resolve_refusal_text(orch, &route);
    record_request_route_with_frozen_refusal_text(route, text);
}

/// Publish facts already captured by a physical call running in a spawned
/// Mod worker. The query must never resolve that worker's route a second time.
pub(crate) fn record_request_route_with_frozen_refusal_text(
    route: crate::query_model::ModelRoute,
    text: Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>,
) {
    record_request_route(route);
    let _ = QUERY_STATE.try_with(|state| {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_refusal_text = text;
    });
}

fn request_routes_for_fallback() -> (
    Option<crate::query_model::ModelRoute>,
    Option<crate::query_model::ModelRoute>,
) {
    QUERY_STATE
        .try_with(|state| {
            let state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.request_route.clone(), state.applied_route.clone())
        })
        .unwrap_or_default()
}

pub(crate) fn resolve_refusal_text(
    orch: &ConversationOrchestrator,
    route: &crate::query_model::ModelRoute,
) -> Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot> {
    resolve_refusal_text_for_api(orch.api.as_ref(), route)
}

pub(crate) fn resolve_refusal_text_for_api(
    api: &dyn crate::conversation::OrchestratorApiClient,
    route: &crate::query_model::ModelRoute,
) -> Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot> {
    match api.refusal_api_text_snapshot(&route.model, route.profile.as_deref()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(model = %route.model, %error, "could not freeze refusal text facts");
            None
        }
    }
}

/// Preserve exact selected-model facts even after a server model transition.
pub(crate) fn refusal_text_snapshot(
    model: &str,
) -> Option<lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot> {
    QUERY_STATE
        .try_with(|state| {
            let state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .applied_route
                .as_ref()
                .is_some_and(|route| route.model == model)
            {
                state.applied_refusal_text.clone()
            } else if state
                .request_route
                .as_ref()
                .is_some_and(|route| route.model == model)
            {
                state.request_refusal_text.clone()
            } else {
                None
            }
        })
        .ok()
        .flatten()
}

fn record_applied_route(orch: &ConversationOrchestrator, route: crate::query_model::ModelRoute) {
    let text = resolve_refusal_text(orch, &route);
    let _ = QUERY_STATE.try_with(|state| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.applied_route = Some(route);
        state.applied_refusal_text = text;
    });
}

pub(crate) fn record_event() {
    let _ = QUERY_STATE.try_with(|state| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.fallback_count += 1;
    });
}

fn mark_declined() {
    let _ = QUERY_STATE.try_with(|state| {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .declined = true;
    });
}

fn queue_notice(notice: lingxi_core::types::ConversationMessage) {
    let _ = QUERY_STATE.try_with(|state| {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_notice = Some(notice);
    });
}

/// Flush Native `He()`'s queued notice. Both local and session-scoped notices
/// are transcript rows; scope controls route effects and copy, not persistence.
pub(crate) async fn flush_pending_notice(
    orch: &ConversationOrchestrator,
) -> Option<lingxi_core::types::MessageId> {
    let notice = QUERY_STATE
        .try_with(|state| {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_notice
                .take()
        })
        .ok()
        .flatten()?;
    let (id, content) = match &notice {
        lingxi_core::types::ConversationMessage::System { id, content, .. } => {
            (*id, content.clone())
        }
        _ => return None,
    };
    orch.session.lock().await.history.push(notice.clone());
    orch.persist_message_to_jsonl(&notice).await;
    if content.string_at("").is_some() {
        orch.output.emit_system_notice(&content, false).await;
    }
    Some(id)
}

/// Validate a server-received model against the host's trusted model policy.
/// Managed policy has native tri-state precedence. An inactive managed policy
/// falls through to the regular settings allowlist and exact resolved default.
#[must_use]
pub(crate) fn received_model_allowed(orch: &ConversationOrchestrator, received: &str) -> bool {
    use llm_runtime::model::allowlist::{is_model_allowed, model_allowed_under};

    let received_identity = lingxi_core::host::refusal_state::wire_identity(received);

    if let Some(decision) = orch
        .config
        .server_fallback_model_enforcement
        .as_ref()
        .and_then(|policy| model_allowed_under(policy, &received_identity))
    {
        return decision;
    }

    let regular = is_model_allowed(
        &received_identity,
        orch.config
            .server_fallback_regular_available_models
            .as_ref(),
        Some(&orch.config.server_fallback_regular_model_overrides),
    );
    let is_resolved_default = orch
        .config
        .server_fallback_default_model
        .as_deref()
        .is_some_and(|default_model| {
            normalized_identity(default_model) == normalized_identity(received)
        });
    regular || is_resolved_default
}

fn normalized_identity(model: &str) -> String {
    lingxi_core::host::refusal_state::wire_identity(model)
        .trim()
        .to_lowercase()
}

fn make_notice(
    orch: &ConversationOrchestrator,
    info: &llm_runtime::history::HistoryServerFallback,
    transition: &crate::query_model::ServerFallbackTransition,
    scope: lingxi_core::host::refusal_server_control::BannerScope,
) -> lingxi_core::types::ConversationMessage {
    let content = crate::query_model::server_fallback_notice_content(
        orch,
        transition,
        scope,
        &info.event.retained_text,
        info.event.api_refusal_category.as_deref(),
    );
    let id = lingxi_core::types::MessageId::new();
    let scope_value = match scope {
        lingxi_core::host::refusal_server_control::BannerScope::Session => "session",
        lingxi_core::host::refusal_server_control::BannerScope::Local => "local",
    };
    lingxi_core::types::ConversationMessage::System {
        api_system: None,
        id,
        content: content.into(),
        subtype: Some("model_refusal_fallback".into()),
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: Some(lingxi_core::types::RefusalFallbackMetadata {
            // Native sHo emits this fixed trigger for both refusal and sticky
            // server events; the visible copy is selected from event.reason.
            trigger: "refusal".into(),
            direction: "retry".into(),
            scope: Some(scope_value.into()),
            original_model: transition.from.model.clone(),
            fallback_model: transition.to.model.clone(),
            request_id: info.event.request_id.clone(),
            api_refusal_category: info.event.api_refusal_category.clone(),
            // Native sHo writes this key as explicit JSON null.
            api_refusal_explanation: None,
            notice_timestamp: Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            // Host-only profile fields preserve multi-provider restoration.
            // They do not add a model-route marker or change native model
            // restoration (`previousAppStateModel` remains the launch model).
            previous_profile: Some(transition.restore.profile.clone()),
            serving_profile: Some(transition.to.profile.clone()),
            ..Default::default()
        }),
    }
}

/// Apply one SDK-admitted server fallback observation before the response is
/// finalized or any received-model tool can run.
pub(crate) async fn handle(
    orch: &ConversationOrchestrator,
    info: &llm_runtime::history::HistoryServerFallback,
    discarded_had_tool_use: bool,
) -> Result<ServerFallbackAdmission, OrchestratorError> {
    let chosen_model = lingxi_core::host::refusal_server_control::resolve_received_model(
        Some(&info.lane.model),
        &info.event.to_model,
    );
    let (is_main_thread, emits_local_scope) = query_scope(
        &orch.config.query_source,
        orch.config.fork_origin.as_deref(),
    );
    let effects = lingxi_core::host::refusal_server_control::control(
        lingxi_core::host::refusal_server_control::Input {
            reason: &info.event.reason,
            discarded_had_tool_use,
            is_main_thread,
            emits_local_scope,
        },
    );

    // Nonvisible server control events are observations only. They do not
    // consume query fallback admission, inspect the model policy, arm headers,
    // or change the session route.
    if !effects.user_visible {
        return Ok(ServerFallbackAdmission::Applied);
    }

    if !received_model_allowed(orch, &info.event.to_model) {
        mark_declined();
        return Ok(ServerFallbackAdmission::Declined);
    }
    record_event();

    let (request_route, previous_applied_route) = request_routes_for_fallback();
    let from = previous_applied_route
        .or(request_route)
        .or_else(crate::query_model::current)
        .unwrap_or_else(|| crate::query_model::ModelRoute {
            model: info.lane.for_model.clone(),
            profile: Some(info.profile.clone()),
        });
    let mut transition = crate::query_model::ServerFallbackTransition {
        from: from.clone(),
        to: crate::query_model::ModelRoute {
            model: chosen_model.clone(),
            profile: Some(info.profile.clone()),
        },
        restore: from,
    };
    if effects.swap_session {
        let session_transition = orch
            .apply_server_fallback_session_model(&chosen_model, &info.profile)
            .await;
        transition.restore = session_transition.restore;
        if info.event.reason == "refusal"
            && info.event.api_refusal_category.as_deref() == Some("cyber")
        {
            orch.model_runtime
                .refusal_selection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .arm_header(info.event.request_id.clone());
        }
    }
    record_applied_route(orch, transition.to.clone());

    // Native compares the query-local selected model (`ge`), not the app
    // state snapshot used to restore a session latch.
    if effects.show_banner && transition.from.model != transition.to.model {
        queue_notice(make_notice(orch, info, &transition, effects.banner_scope));
    }

    Ok(ServerFallbackAdmission::Applied)
}

/// Native `fe`/`Je` and `Is` are query-source and fork-origin predicates.
pub(crate) fn query_scope(query_source: &str, fork_origin: Option<&str>) -> (bool, bool) {
    (
        query_source.starts_with(crate::config::QUERY_SOURCE_REPL_MAIN_THREAD)
            || query_source == crate::config::QUERY_SOURCE_SDK,
        query_source == "side_question" || fork_origin == Some("btw"),
    )
}

/// Surface an allowlist decline using the existing terminal refusal text for
/// refusal events, or the native invalid-request body for sticky events.
pub(crate) async fn surface_declined(
    orch: &ConversationOrchestrator,
    info: &llm_runtime::history::HistoryServerFallback,
    response_message_id: Option<&str>,
) -> (lingxi_core::types::MessageId, &'static str) {
    let timestamp = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    if info.event.reason == "refusal" {
        let details = llm_runtime::HistoryStopDetails::from_parts(
            info.event.api_refusal_category.as_deref(),
            None,
        )
        .expect("server fallback refusal details form a valid object");
        let (request_route, applied_route) = request_routes_for_fallback();
        let model = match applied_route.or(request_route) {
            Some(route) => route.model,
            None => orch.session.lock().await.model.clone(),
        };
        // The query's selected-model facts were frozen before its provider call.
        let text = crate::turn_loop::terminal_api_error_text(
            &model,
            orch.prompt_is_interactive(),
            "refusal",
            info.event.request_id.as_deref(),
            Some(&details),
            response_message_id,
        )
        .expect("terminal refusal always has user-facing text");
        let mut row = lingxi_core::host::ServerFallbackApiErrorRow::new(&text, timestamp);
        let stop_details = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &serde_json::json!({
                "type": "refusal",
                "category": info.event.api_refusal_category,
                "explanation": null,
                "fallback_credit_token": null,
                "fallback_has_prefill_claim": null,
                "recommended_model": null,
            }),
        )
        .expect("generated refusal details are valid JSON");
        row.set_refusal(info.event.request_id.clone(), stop_details);
        row.set_refused_message_id(response_message_id);
        let id = surface_declined_row(orch, &row).await;
        return (id, "refusal");
    }

    let text = lingxi_core::types::utf16_json::Utf16JsonProjection::from(
        lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR,
    );
    let row = lingxi_core::host::ServerFallbackApiErrorRow::new(&text, timestamp);
    let id = surface_declined_row(orch, &row).await;
    (id, "model_error")
}

async fn surface_declined_row(
    orch: &ConversationOrchestrator,
    row: &lingxi_core::host::ServerFallbackApiErrorRow,
) -> lingxi_core::types::MessageId {
    let raw = row.query_message();
    // Native awaits appended(Ro), then pushes/yields the original Ro. Accepted
    // storage projections must not replace K or dispatch a second append hook.
    let stored = orch.append_streamed_query_row(&raw, None).await;
    orch.session.lock().await.history.push(raw);
    orch.persist_preappended_server_fallback_api_error_row(&stored, row)
        .await;
    // Publish the original Native row, not the accepted storage projection or
    // a second synthetic construction with a different inner id/time.
    let message = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
        &serde_json::to_string(&row.message)
            .expect("host-created synthetic message serializes as source JSON"),
    )
    .expect("host-created synthetic message is source JSON");
    orch.output
        .emit_terminal_api_error_message(
            &lingxi_core::host::orchestrator::TerminalApiErrorMessage {
                uuid: row.uuid,
                timestamp: row.timestamp.clone(),
                message,
                api_error: None,
                error: Some(row.error.clone()),
                api_error_status: None,
                request_id: row.request_id.clone(),
                refused_message_id: row.refused_message_id.clone(),
                terminal_reason: "api_error".into(),
                stop_reason: row.message.stop_reason.clone(),
                notify_terminal_error: false,
            },
        )
        .await;
    row.uuid
}

#[cfg(test)]
mod refusal_row_tests {
    use super::*;
    use lingxi_core::host::orchestrator::{CostSnapshot, OutputStream, TerminalApiErrorMessage};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::ToolUseId;
    use std::sync::Arc;

    #[derive(Default)]
    struct TypedOnlySink {
        rows: tokio::sync::Mutex<Vec<TerminalApiErrorMessage>>,
        text_callbacks: std::sync::atomic::AtomicUsize,
        terminal_callbacks: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl OutputStream for TypedOnlySink {
        async fn emit_text(&self, _text: &Utf16JsonProjection) {
            self.text_callbacks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn emit_tool_call(&self, _id: &ToolUseId, _tool: &str, _input: &Utf16JsonProjection) {
        }
        async fn emit_tool_result(
            &self,
            _id: &ToolUseId,
            _tool: &str,
            _text: &Utf16JsonProjection,
            _result: &Utf16JsonProjection,
            _meta: Option<&Utf16JsonProjection>,
        ) {
        }
        async fn emit_end_turn(&self, _reason: &str, _cost: &CostSnapshot) {}
        async fn emit_terminal_api_error(&self, _status: Option<u16>, _reason: &str, _stop: &str) {
            self.terminal_callbacks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        async fn emit_terminal_api_error_message(&self, row: &TerminalApiErrorMessage) {
            self.rows.lock().await.push(row.clone());
        }
    }

    fn memory_orchestrator(output: Arc<dyn OutputStream>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            output,
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }

    #[tokio::test]
    async fn declined_row_typed_publication_keeps_original_identity_body_and_time_once() {
        let sink = Arc::new(TypedOnlySink::default());
        let orch = memory_orchestrator(sink.clone());
        let mut original = lingxi_core::host::ServerFallbackApiErrorRow::new(
            &"original declined text".into(),
            "2000-01-01T00:00:00.000Z".into(),
        );
        original.set_refusal(
            Some("req_original".into()),
            Utf16JsonProjection::parse(r#"{"category":"A\ud800B","explanation":null}"#).unwrap(),
        );
        original.set_refused_message_id(Some("msg_original"));
        let id = surface_declined_row(&orch, &original).await;
        let rows = sink.rows.lock().await;
        assert_eq!(rows.len(), 1);
        let public = &rows[0];
        assert_eq!(id, original.uuid);
        assert_eq!(public.uuid, original.uuid);
        assert_eq!(public.timestamp, original.timestamp);
        assert_ne!(original.uuid, original.message.id);
        assert_eq!(
            public.message.string_units("/id"),
            Some(
                original
                    .message
                    .id
                    .as_uuid()
                    .to_string()
                    .encode_utf16()
                    .collect()
            )
        );
        let expected =
            Utf16JsonProjection::parse(&serde_json::to_string(&original.message).unwrap()).unwrap();
        assert_eq!(
            public.message.to_json_string().unwrap(),
            expected.to_json_string().unwrap()
        );
        assert_eq!(
            public.message.string_units("/stop_details/category"),
            Some(vec![65, 0xd800, 66])
        );
        assert_eq!(public.error.as_deref(), Some(original.error.as_str()));
        assert_eq!(public.request_id, original.request_id);
        assert_eq!(public.refused_message_id, original.refused_message_id);
        assert_eq!(public.stop_reason, original.message.stop_reason);
        assert_eq!(public.terminal_reason, "api_error");
        assert!(public.api_error.is_none());
        assert!(public.api_error_status.is_none());
        assert!(!public.notify_terminal_error);
        assert_eq!(
            sink.text_callbacks
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            sink.terminal_callbacks
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn declined_row_default_publication_preserves_text_only_legacy_policy() {
        let sink = Arc::new(crate::test_support::MockOutputStream::new());
        let orch = memory_orchestrator(sink.clone());
        let original = lingxi_core::host::ServerFallbackApiErrorRow::new(
            &"original declined text".into(),
            "2000-01-01T00:00:00.000Z".into(),
        );
        surface_declined_row(&orch, &original).await;
        let events = sink.snapshot().await;
        let texts = events
            .iter()
            .filter_map(|event| match event {
                lingxi_core::host::OutputEvent::Text { text } => {
                    Some(text.string_units("").unwrap())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["original declined text".encode_utf16().collect::<Vec<_>>()]
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            lingxi_core::host::OutputEvent::TerminalApiError { .. }
        )));
    }

    #[tokio::test]
    async fn declined_refusal_persists_only_a_native_validated_response_id() {
        let root = tempfile::tempdir().unwrap();
        let session_id = lingxi_core::types::SessionId::new();
        let path = root.path().join(format!("{}.jsonl", session_id.as_uuid()));
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                model: "claude-opus-4-8".into(),
                ..Default::default()
            },
            Arc::new(crate::test_support::MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            Arc::new(crate::test_support::MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_session_id(session_id)
        .with_jsonl_writer(writer);
        orch.initialize_conversation_mode()
            .await
            .expect("fixed Code session profile");
        let info: llm_runtime::history::HistoryServerFallback = serde_json::from_value(serde_json::json!({
            "event": {"fromModel":"claude-opus-4-8","toModel":"declined-model","reason":"refusal",
                "apiRefusalCategory":"bio","midStream":false,"requestId":"req_declined",
                "discardedBlocks":[],"retainedBlocks":[],"retainedText":"","finalStopReason":"end_turn"},
            "profile":"first-party-anthropic",
            "lane":{"forModel":"claude-opus-4-8","model":"declined-model","mode":"explicit"}
        })).unwrap();
        for (message_id, expected) in [("msg_declined", Some("msg_declined")), ("invalid/id", None)]
        {
            let (id, reason) = surface_declined(&orch, &info, Some(message_id)).await;
            assert_eq!(reason, "refusal");
            let encoded = std::fs::read_to_string(&path).unwrap();
            let row = encoded
                .lines()
                .filter_map(|line| Utf16JsonProjection::parse(line).ok())
                .find(|row| {
                    row.string_units("/uuid")
                        == Some(id.as_uuid().to_string().encode_utf16().collect())
                })
                .expect("declined row reached the actual JSONL writer");
            assert_eq!(
                row.string_units("/refusedMessageId"),
                expected.map(|id| id.encode_utf16().collect())
            );
            let text = row.string_units("/message/content/0/text").unwrap();
            let suffix = format!("\n\nMessage ID: {message_id}")
                .encode_utf16()
                .collect::<Vec<_>>();
            assert_eq!(text.ends_with(&suffix), expected.is_some());
        }
    }
}
