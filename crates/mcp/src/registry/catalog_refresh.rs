use super::diagnostics::{
    emit_list_changed, emit_listen_reopen, forward_catalog_change, list_changed_payload,
    listen_reopen_payload,
};
#[cfg(test)]
use super::{
    maybe_pause_catalog_change_listener_for_test, notify_catalog_change_listener_closed_for_test,
    test_listener_reopen_park_jitter,
};
use super::{
    mcp_connection_timeout, McpCatalogChanged, McpCatalogKind, McpRegistry,
    ModernListenOpenTelemetry, LISTENER_REOPEN_GRACEFUL_DELAY,
    LISTENER_REOPEN_MAX_ATTEMPTS_PER_WINDOW, LISTENER_REOPEN_PARK, LISTENER_REOPEN_PARK_POLL,
    LISTENER_REOPEN_RETRY_DELAYS, LISTENER_REOPEN_STABLE_RESET, LISTENER_REOPEN_WINDOW,
    LISTEN_REOPEN_CAUSE,
};
use crate::connection::McpConnectionState;
use lingxi_core::host::{McpError, ServerCapabilitiesDto};
use lingxi_core::types::McpConnectionId;
use rand::Rng as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub(super) fn modern_listen_request_params(
    version: &str,
    elicitation: lingxi_core::host::McpElicitationMode,
    notifications: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "_meta": crate::client::modern_meta(version, elicitation),
        "notifications": notifications,
    })
}

pub(super) fn modern_listen_notifications_filter(
    capabilities: &ServerCapabilitiesDto,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut notifications = serde_json::Map::new();
    if capabilities.tools {
        notifications.insert(
            "toolsListChanged".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if capabilities.prompts {
        notifications.insert(
            "promptsListChanged".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if capabilities.resources {
        notifications.insert(
            "resourcesListChanged".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    (!notifications.is_empty()).then_some(notifications)
}

pub(super) fn notification_matches_subscription(
    notification: &jsonrpc::Notification,
    subscription_id: &jsonrpc::Id,
) -> bool {
    let Some(params) = notification.params.as_ref() else {
        return false;
    };
    let Some(meta) = params.get("_meta").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(observed) = meta.get("io.modelcontextprotocol/subscriptionId") else {
        return false;
    };
    match subscription_id {
        jsonrpc::Id::Number(expected) => observed.as_i64() == Some(*expected),
        jsonrpc::Id::String(expected) => observed.as_str() == Some(expected),
        jsonrpc::Id::StringUtf16(expected) => {
            notification
                .projection
                .as_ref()
                .and_then(|projection| {
                    projection.string_units("/params/_meta/io.modelcontextprotocol~1subscriptionId")
                })
                .as_ref()
                == Some(expected)
        }
    }
}

pub(super) fn listener_reopen_park_jitter() -> f64 {
    #[cfg(test)]
    if let Some(jitter) = test_listener_reopen_park_jitter() {
        return jitter;
    }
    rand::rng().random_range(0.8_f64..=1.2_f64)
}

impl McpRegistry {
    /// Return one refresh request for every catalog currently advertised by
    /// every SHARED connected/cached server. Consumers use this to recover
    /// deterministically after a lagged broadcast receiver instead of leaving a
    /// missed `list_changed` notification stale until the server happens to
    /// emit another one.
    pub async fn catalog_refresh_snapshot(&self) -> Vec<McpCatalogChanged> {
        let conns = self.connections.read().await;
        let mut server_names: Vec<&String> = conns.keys().collect();
        server_names.sort();
        let mut changes = Vec::new();
        for server_name in server_names {
            let Some(state) = conns.get(server_name) else {
                continue;
            };
            if state.config().name != *server_name {
                continue;
            }
            changes.extend(Self::active_catalog_snapshot(server_name, state));
        }
        changes
    }
    pub(super) async fn current_catalog_snapshot_for_connection(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) -> Vec<McpCatalogChanged> {
        let conns = self.connections.read().await;
        let Some(state) = conns.get(server_name) else {
            return Vec::new();
        };
        if state.config().name != server_name {
            return Vec::new();
        }
        Self::active_catalog_snapshot(server_name, state)
            .into_iter()
            .filter(|change| change.connection_id == connection_id)
            .collect()
    }
    pub(super) async fn publish_catalog_snapshot_for_connection(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        telemetry_cause: Option<&'static str>,
    ) {
        for mut change in self
            .current_catalog_snapshot_for_connection(server_name, connection_id)
            .await
        {
            change.telemetry_cause = telemetry_cause;
            let _ = self.catalog_changes.send(change);
        }
    }
    pub(super) fn active_catalog_snapshot(
        server_name: &str,
        state: &McpConnectionState,
    ) -> Vec<McpCatalogChanged> {
        let (connection_id, capabilities) = match state {
            McpConnectionState::Connected {
                connection_id,
                capabilities,
                ..
            }
            | McpConnectionState::Cached {
                connection_id,
                capabilities,
                ..
            } => (*connection_id, capabilities),
            _ => return Vec::new(),
        };
        let mut changes = Vec::new();
        for (supported, kind) in [
            (capabilities.tools, McpCatalogKind::Tools),
            (capabilities.prompts, McpCatalogKind::Prompts),
            (capabilities.resources, McpCatalogKind::Resources),
        ] {
            if supported {
                changes.push(McpCatalogChanged {
                    server_name: server_name.to_string(),
                    connection_id,
                    retired_connection_id: None,
                    kind,
                    telemetry_cause: None,
                });
            }
        }
        changes
    }
    /// Re-query the catalog named by `change` and atomically replace only that
    /// slice of the connected-state snapshot. A stale connection generation is
    /// ignored. The old catalog remains intact on RPC/decode failure.
    pub async fn refresh_catalog(
        &self,
        change: &McpCatalogChanged,
    ) -> Result<Option<McpConnectionId>, McpError> {
        let (current_id, supports_kind, previous_count) = {
            let conns = self.connections.read().await;
            match conns.get(&change.server_name) {
                Some(McpConnectionState::Connected {
                    connection_id,
                    capabilities,
                    tools,
                    prompts,
                    resources,
                    ..
                }) if *connection_id == change.connection_id => {
                    let previous_count = match change.kind {
                        McpCatalogKind::Tools => Some(tools.len()),
                        McpCatalogKind::Prompts => Some(prompts.len()),
                        McpCatalogKind::Resources => Some(resources.len()),
                    };
                    let supports_kind = match change.kind {
                        McpCatalogKind::Tools => capabilities.tools,
                        McpCatalogKind::Prompts => capabilities.prompts,
                        McpCatalogKind::Resources => capabilities.resources,
                    };
                    (*connection_id, supports_kind, previous_count)
                }
                Some(McpConnectionState::Cached { connection_id, .. })
                    if *connection_id == change.connection_id =>
                {
                    // §11 Stage 2/3 — a `Cached` server has no live
                    // connection to re-query: the cached catalog IS the
                    // freshest thing this port has for it. Report success
                    // unchanged so lag recovery can rebuild the model-facing
                    // partition from the last-known cached snapshot.
                    return Ok(Some(*connection_id));
                }
                _ => return Ok(None),
            }
        };
        if !supports_kind {
            return Ok(Some(current_id));
        }
        let Some(client) = self.get_client(&change.server_name).await else {
            return Err(McpError::Internal(format!(
                "MCP server \"{}\" has no live client for catalog refresh",
                change.server_name
            )));
        };

        enum Refreshed {
            Tools(Vec<lingxi_core::host::McpToolDto>),
            Prompts(Vec<lingxi_core::host::McpPromptDto>),
            Resources(Vec<lingxi_core::host::McpResourceDto>),
        }
        let refreshed = match change.kind {
            McpCatalogKind::Tools => client
                .list_tools()
                .await
                .map(Refreshed::Tools)
                .map_err(|e| McpError::Internal(e.to_string()))?,
            McpCatalogKind::Prompts => client
                .list_prompts()
                .await
                .map(Refreshed::Prompts)
                .map_err(|e| McpError::Internal(e.to_string()))?,
            McpCatalogKind::Resources => client
                .list_resources()
                .await
                .map(Refreshed::Resources)
                .map_err(|e| McpError::Internal(e.to_string()))?,
        };

        let mut conns = self.connections.write().await;
        let Some(McpConnectionState::Connected {
            connection_id,
            tools,
            prompts,
            resources,
            ..
        }) = conns.get_mut(&change.server_name)
        else {
            return Ok(None);
        };
        if *connection_id != current_id {
            return Ok(None);
        }
        let new_count = match refreshed {
            Refreshed::Tools(next) => {
                let new_count = next.len();
                *tools = next;
                new_count
            }
            Refreshed::Prompts(next) => {
                let new_count = next.len();
                *prompts = next;
                new_count
            }
            Refreshed::Resources(next) => {
                let new_count = next.len();
                *resources = next;
                new_count
            }
        };
        if let Some(cause) = change.telemetry_cause {
            emit_list_changed(&list_changed_payload(
                &change.server_name,
                change.kind,
                cause,
                previous_count,
                Some(new_count),
            ));
        }
        Ok(Some(current_id))
    }
    pub(super) fn spawn_catalog_change_listener(
        &self,
        server_name: String,
        connection_id: McpConnectionId,
        connection: Arc<jsonrpc::Connection>,
        negotiated: lingxi_core::host::McpNegotiatedProtocol,
        capabilities: ServerCapabilitiesDto,
        open_telemetry: Option<ModernListenOpenTelemetry>,
    ) {
        let registry = self.clone_for_background();
        let notifications = connection.notifications();
        tokio::spawn(async move {
            #[cfg(test)]
            maybe_pause_catalog_change_listener_for_test(connection_id).await;
            if negotiated.era == lingxi_core::host::McpProtocolEra::Modern {
                registry
                    .run_modern_catalog_change_listener(
                        server_name,
                        connection_id,
                        connection,
                        notifications,
                        capabilities,
                        negotiated,
                        open_telemetry.unwrap_or(ModernListenOpenTelemetry {
                            outcome: telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero,
                            attempts: 0,
                            trigger: telemetry::tengu::mcp::ListenReopenTrigger::Connect,
                        }),
                    )
                    .await;
                return;
            }

            let mut notifications = notifications;
            loop {
                let notification = match notifications.recv().await {
                    Ok(notification) => notification,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        notifications = connection.notifications();
                        registry
                            .publish_catalog_snapshot_for_connection(
                                &server_name,
                                connection_id,
                                Some(LISTEN_REOPEN_CAUSE),
                            )
                            .await;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                forward_catalog_change(
                    &registry.catalog_changes,
                    &server_name,
                    connection_id,
                    &notification.method,
                    Some("notification"),
                );
            }
        });
    }
    pub(super) async fn run_modern_catalog_change_listener(
        &self,
        server_name: String,
        connection_id: McpConnectionId,
        connection: Arc<jsonrpc::Connection>,
        mut notifications: broadcast::Receiver<jsonrpc::Notification>,
        capabilities: ServerCapabilitiesDto,
        negotiated: lingxi_core::host::McpNegotiatedProtocol,
        open_telemetry: ModernListenOpenTelemetry,
    ) {
        let Some(filter) = modern_listen_notifications_filter(&capabilities) else {
            return;
        };
        let Some(elicitation) = self
            .clients
            .read()
            .await
            .get(&server_name)
            .filter(|registered| {
                registered
                    .connection_id
                    .is_none_or(|id| id == connection_id)
            })
            .map(|registered| registered.client.elicitation_capabilities().modern)
        else {
            return;
        };
        let listen = match connection.start_call_unbounded(
            "subscriptions/listen",
            modern_listen_request_params(&negotiated.version, elicitation, filter),
        ) {
            Ok(listen) => listen,
            Err(_) => {
                #[cfg(test)]
                notify_catalog_change_listener_closed_for_test();
                self.handle_modern_catalog_listener_end(
                    &server_name,
                    connection_id,
                    telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                )
                .await;
                return;
            }
        };
        let subscription_id = listen.id().clone();
        let completion = listen.wait_value();
        tokio::pin!(completion);

        loop {
            tokio::select! {
                completion = &mut completion => {
                    #[cfg(test)]
                    notify_catalog_change_listener_closed_for_test();
                    let trigger = if completion.is_ok() {
                        telemetry::tengu::mcp::ListenReopenTrigger::Graceful
                    } else {
                        telemetry::tengu::mcp::ListenReopenTrigger::Remote
                    };
                    self.handle_modern_catalog_listener_end(&server_name, connection_id, trigger)
                        .await;
                    return;
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(notification) => {
                            if !notification_matches_subscription(&notification, &subscription_id) {
                                continue;
                            }
                            if notification.method == "notifications/subscriptions/acknowledged" {
                                self.record_listener_open(&server_name, open_telemetry)
                                    .await;
                                emit_listen_reopen(&listen_reopen_payload(
                                    &server_name,
                                    open_telemetry.outcome,
                                    open_telemetry.attempts,
                                    open_telemetry.trigger,
                                ));
                                continue;
                            }
                            forward_catalog_change(
                                &self.catalog_changes,
                                &server_name,
                                connection_id,
                                &notification.method,
                                Some("notification"),
                            );
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            self.publish_catalog_snapshot_for_connection(
                                &server_name,
                                connection_id,
                                Some(LISTEN_REOPEN_CAUSE),
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            #[cfg(test)]
                            notify_catalog_change_listener_closed_for_test();
                            self.handle_modern_catalog_listener_end(
                                &server_name,
                                connection_id,
                                telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                            )
                            .await;
                            return;
                        }
                    }
                }
            }
        }
    }
    pub(super) async fn handle_modern_catalog_listener_end(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        trigger: telemetry::tengu::mcp::ListenReopenTrigger,
    ) {
        let (mut delay_index, reopen_count) = self.prepare_modern_reopen_cycle(server_name).await;
        if reopen_count >= LISTENER_REOPEN_MAX_ATTEMPTS_PER_WINDOW {
            emit_listen_reopen(&listen_reopen_payload(
                server_name,
                telemetry::tengu::mcp::ListenReopenOutcome::BudgetExhausted,
                reopen_count as u32,
                trigger,
            ));
            emit_listen_reopen(&listen_reopen_payload(
                server_name,
                telemetry::tengu::mcp::ListenReopenOutcome::Parked,
                reopen_count as u32,
                trigger,
            ));
            if !self
                .wait_listener_reopen_park(server_name, connection_id)
                .await
            {
                emit_listen_reopen(&listen_reopen_payload(
                    server_name,
                    telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
                    0,
                    trigger,
                ));
                return;
            }
            delay_index = 0;
        }

        let mut last_error = None;
        for attempt in 1..=LISTENER_REOPEN_RETRY_DELAYS.len() {
            let capped_index = delay_index
                .saturating_add(attempt - 1)
                .min(LISTENER_REOPEN_RETRY_DELAYS.len() - 1);
            let mut delay = LISTENER_REOPEN_RETRY_DELAYS[capped_index];
            if attempt == 1 && trigger == telemetry::tengu::mcp::ListenReopenTrigger::Graceful {
                delay += LISTENER_REOPEN_GRACEFUL_DELAY;
            }
            if !self
                .wait_listener_reopen_delay(server_name, connection_id, delay)
                .await
            {
                emit_listen_reopen(&listen_reopen_payload(
                    server_name,
                    telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
                    (attempt - 1) as u32,
                    trigger,
                ));
                return;
            }
            match self
                .reopen_catalog_listener_generation(
                    server_name,
                    connection_id,
                    trigger,
                    attempt as u32,
                    capped_index,
                )
                .await
            {
                Ok(true) => return,
                Ok(false) => {
                    emit_listen_reopen(&listen_reopen_payload(
                        server_name,
                        telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
                        attempt as u32,
                        trigger,
                    ));
                    return;
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        let reason =
            last_error.unwrap_or_else(|| McpError::Connection("listen reopen failed".into()));
        self.mark_listener_generation_disconnected(server_name, connection_id, &reason)
            .await;
        emit_listen_reopen(&listen_reopen_payload(
            server_name,
            telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
            LISTENER_REOPEN_RETRY_DELAYS.len() as u32,
            trigger,
        ));
    }
    pub(super) async fn prepare_modern_reopen_cycle(&self, server_name: &str) -> (usize, usize) {
        let now = tokio::time::Instant::now();
        let mut states = self.listener_reopen_state.write().await;
        let state = states.entry(server_name.to_string()).or_default();
        if state.opened_at.take().is_some_and(|opened| {
            now.saturating_duration_since(opened) >= LISTENER_REOPEN_STABLE_RESET
        }) {
            state.delay_index = 0;
        }
        state
            .reopened_at
            .retain(|reopened| now.saturating_duration_since(*reopened) < LISTENER_REOPEN_WINDOW);
        (state.delay_index, state.reopened_at.len())
    }
    pub(super) async fn record_listener_open(
        &self,
        server_name: &str,
        open_telemetry: ModernListenOpenTelemetry,
    ) {
        let now = tokio::time::Instant::now();
        let mut states = self.listener_reopen_state.write().await;
        let state = states.entry(server_name.to_string()).or_default();
        state.opened_at = Some(now);
        match open_telemetry.outcome {
            telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero => {
                state.delay_index = 0;
            }
            telemetry::tengu::mcp::ListenReopenOutcome::Reopened => {
                state.delay_index = state
                    .delay_index
                    .saturating_add(1)
                    .min(LISTENER_REOPEN_RETRY_DELAYS.len().saturating_sub(1));
                state.reopened_at.retain(|reopened| {
                    now.saturating_duration_since(*reopened) < LISTENER_REOPEN_WINDOW
                });
                state.reopened_at.push(now);
            }
            telemetry::tengu::mcp::ListenReopenOutcome::GaveUp
            | telemetry::tengu::mcp::ListenReopenOutcome::BudgetExhausted
            | telemetry::tengu::mcp::ListenReopenOutcome::Parked => {}
        }
    }
    pub(super) async fn wait_listener_reopen_delay(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        delay: Duration,
    ) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = self.wait_until_listener_cancelled(server_name, connection_id) => false,
        }
    }
    pub(super) async fn wait_listener_reopen_park(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) -> bool {
        let jitter = listener_reopen_park_jitter();
        let park = LISTENER_REOPEN_PARK.mul_f64(jitter);
        let deadline = tokio::time::Instant::now() + park;
        loop {
            if !self
                .is_listener_generation_current(server_name, connection_id)
                .await
            {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return true;
            };
            tokio::time::sleep(std::cmp::min(remaining, LISTENER_REOPEN_PARK_POLL)).await;
        }
    }
    pub(super) async fn wait_until_listener_cancelled(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) {
        loop {
            if !self
                .is_listener_generation_current(server_name, connection_id)
                .await
            {
                return;
            }
            tokio::time::sleep(LISTENER_REOPEN_PARK_POLL).await;
        }
    }
    pub(super) async fn is_listener_generation_current(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) -> bool {
        let conns = self.connections.read().await;
        matches!(
            conns.get(server_name),
            Some(McpConnectionState::Connected {
                connection_id: current,
                config,
                ..
            }) if *current == connection_id && config.name == server_name && !config.disabled
        )
    }
    pub(super) async fn reopen_catalog_listener_generation(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        trigger: telemetry::tengu::mcp::ListenReopenTrigger,
        attempts: u32,
        delay_index: usize,
    ) -> Result<bool, McpError> {
        let lifecycle = self.lifecycle_lock(server_name);
        let _guard = lifecycle.lock().await;
        let Some(config) = ({
            let conns = self.connections.read().await;
            match conns.get(server_name) {
                Some(McpConnectionState::Connected {
                    connection_id: current,
                    config,
                    ..
                }) if *current == connection_id
                    && config.name == server_name
                    && !config.disabled =>
                {
                    Some(config.clone())
                }
                _ => None,
            }
        }) else {
            return Ok(false);
        };

        let negotiation_mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            config.metadata.transport.as_deref(),
            mcp_connection_timeout().as_millis() as u64,
        );
        let discovery = self
            .discover_live_connection(&config, negotiation_mode)
            .await?;
        let new_connection_id = self
            .install_live_discovery(
                server_name.to_string(),
                config,
                discovery,
                Some(connection_id),
                None,
                Some(ModernListenOpenTelemetry {
                    outcome: telemetry::tengu::mcp::ListenReopenOutcome::Reopened,
                    attempts,
                    trigger,
                }),
            )
            .await?;
        self.set_listener_reopen_delay_index(server_name, delay_index)
            .await;
        self.disconnect_or_schedule_cleanup(connection_id).await;
        let _ = new_connection_id;
        Ok(true)
    }
    pub(super) async fn set_listener_reopen_delay_index(
        &self,
        server_name: &str,
        delay_index: usize,
    ) {
        let mut states = self.listener_reopen_state.write().await;
        let state = states.entry(server_name.to_string()).or_default();
        state.delay_index = delay_index;
        state.opened_at = None;
    }
    pub(super) async fn mark_listener_generation_disconnected(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        error: &McpError,
    ) {
        let Some(config) = ({
            let conns = self.connections.read().await;
            match conns.get(server_name) {
                Some(McpConnectionState::Connected {
                    connection_id: current,
                    config,
                    ..
                }) if *current == connection_id => Some(config.clone()),
                _ => None,
            }
        }) else {
            return;
        };
        self.connections.write().await.insert(
            server_name.to_string(),
            McpConnectionState::Disconnected {
                config: config.clone(),
                last_error: Some(error.to_string()),
            },
        );
        self.clients.write().await.shift_remove(server_name);
        self.clear_prompt_predecessors_for_key(server_name).await;
        self.emit_retire_event_if_shared(&config, server_name, connection_id)
            .await;
    }
    pub(super) async fn publish_listener_reopen_catalog_changes(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        retired_connection_id: Option<McpConnectionId>,
        capabilities: &ServerCapabilitiesDto,
    ) {
        for kind in [
            McpCatalogKind::Tools,
            McpCatalogKind::Prompts,
            McpCatalogKind::Resources,
        ] {
            let supported = match kind {
                McpCatalogKind::Tools => capabilities.tools,
                McpCatalogKind::Prompts => capabilities.prompts,
                McpCatalogKind::Resources => capabilities.resources,
            };
            if !supported {
                continue;
            }
            self.publish_catalog_change(McpCatalogChanged {
                server_name: server_name.to_string(),
                connection_id,
                retired_connection_id: retired_connection_id
                    .filter(|_| kind == McpCatalogKind::Tools),
                kind,
                telemetry_cause: Some(LISTEN_REOPEN_CAUSE),
            })
            .await;
        }
    }
}
