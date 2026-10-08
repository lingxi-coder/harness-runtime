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

fn record_applied_route(route: crate::query_model::ModelRoute) {
    let _ = QUERY_STATE.try_with(|state| {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .applied_route = Some(route);
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
    orch.output.emit_system_notice(&content, false).await;
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
            .as_deref(),
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
    lingxi_core::types::ConversationMessage::System { api_system: None,
        id,
        content,
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
    record_applied_route(transition.to.clone());

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
) -> (lingxi_core::types::MessageId, &'static str) {
    let timestamp = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    if info.event.reason == "refusal" {
        let details = llm_runtime::HistoryStopDetails {
            category: info.event.api_refusal_category.clone(),
            explanation: None,
        };
        let (request_route, applied_route) = request_routes_for_fallback();
        let model = match applied_route.or(request_route) {
            Some(route) => route.model,
            None => orch.session.lock().await.model.clone(),
        };
        // The shared current-Native refusal formatter is being pinned
        // separately; preserve this existing text until that source audit lands.
        let text = crate::turn_loop::terminal_api_error_text(
            &model,
            orch.prompt_is_interactive(),
            "refusal",
            info.event.request_id.as_deref(),
            Some(&details),
        )
        .expect("terminal refusal always has user-facing text");
        let mut row = lingxi_core::host::ServerFallbackApiErrorRow::new(&text, timestamp);
        row.set_refusal(
            info.event.request_id.clone(),
            serde_json::json!({
                "type": "refusal",
                "category": info.event.api_refusal_category,
                "explanation": null,
                "fallback_credit_token": null,
                "fallback_has_prefill_claim": null,
                "recommended_model": null,
            }),
        );
        let id = surface_declined_row(orch, &row).await;
        return (id, "refusal");
    }

    let row = lingxi_core::host::ServerFallbackApiErrorRow::new(
        lingxi_core::host::refusal_server_control::SERVER_FALLBACK_ALLOWLIST_ERROR,
        timestamp,
    );
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
    for block in &row.message.content {
        if let lingxi_core::types::ContentBlock::Text { text, .. } = block {
            orch.output.emit_text(text, None).await;
        }
    }
    row.uuid
}
