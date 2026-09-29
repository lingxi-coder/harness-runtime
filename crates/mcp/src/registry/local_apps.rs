use super::{
    is_local_app_id, is_local_app_tool_name, is_sha256, ConversationExport, LocalAppExposure,
    LocalAppExposureUpdate, ManagedLocalAppResource, ManagedLocalAppRuntime, ManagedLocalAppServer,
    McpCatalogChanged, McpCatalogKind, McpRegistry, LOCAL_APP_MAX_EXPOSED,
    LOCAL_APP_MAX_IN_FLIGHT_PER_APP, LOCAL_APP_MAX_IN_FLIGHT_PER_CONVERSATION,
};
use crate::connection::McpConnectionState;
use platform_api::McpError;
use protocol::McpConnectionId;

impl McpRegistry {
    pub(super) async fn managed_local_app_connection_id(
        &self,
        server_name: &str,
    ) -> McpConnectionId {
        let connections = self.connections.read().await;
        match connections.get(server_name) {
            Some(McpConnectionState::Connected { connection_id, .. })
            | Some(McpConnectionState::Cached { connection_id, .. })
            | Some(McpConnectionState::HealthChecking { connection_id, .. }) => *connection_id,
            _ => McpConnectionId::new(),
        }
    }
    pub(super) fn managed_local_app_runtime_changed(
        previous: &ManagedLocalAppRuntime,
        current: &ManagedLocalAppRuntime,
    ) -> (bool, bool) {
        (
            previous.enabled != current.enabled || previous.enabled_tools != current.enabled_tools,
            previous.enabled != current.enabled || previous.resource != current.resource,
        )
    }
    pub(super) async fn remove_local_app_exposures(&self, app_id: &str) -> bool {
        let mut conversations = self.local_app_exposures.write().await;
        let mut removed = false;
        for state in conversations.values_mut() {
            if state.entries.remove(app_id).is_some() {
                state.next_generation = state.next_generation.saturating_add(1);
                removed = true;
            }
        }
        removed
    }
    /// Register or refresh one published Local App logical server. Only a
    /// changed tool surface advances the logical generation and emits the
    /// shared tools/list_changed notification; build/execution-only changes
    /// update the catalog pointer without invalidating connections.
    pub async fn register_managed_local_app(
        &self,
        scope: ConversationExport,
        catalog_sha256: String,
        _surface_changed: bool,
    ) -> Result<ManagedLocalAppServer, McpError> {
        if !is_sha256(&catalog_sha256) {
            return Err(McpError::Internal(
                "invalid Local App catalog identity".into(),
            ));
        }
        let mut apps = self.managed_local_apps.write().await;
        // The catalog commit is the authority for whether the exposed tool
        // surface changed. Do not trust a caller-supplied boolean: a stale or
        // forged hint must not produce duplicate listChanged notifications,
        // nor suppress one when a new surface is actually committed.
        let actual_surface_changed = apps.get(&scope.app_id).is_none_or(|server| {
            server.scope.listed_tool_surface_sha256 != scope.listed_tool_surface_sha256
        });
        let previous = apps.get(&scope.app_id).cloned();
        let actual_catalog_changed = previous
            .as_ref()
            .is_some_and(|server| server.catalog_sha256 != catalog_sha256);
        let generation = previous
            .as_ref()
            .map(|server| server.surface_generation + u64::from(actual_surface_changed))
            .unwrap_or(1);
        let server = ManagedLocalAppServer {
            scope: scope.clone(),
            catalog_sha256,
            surface_generation: generation,
        };
        apps.insert(scope.app_id.clone(), server.clone());
        drop(apps);
        self.managed_local_app_runtime
            .write()
            .await
            .entry(scope.app_id.clone())
            .or_insert_with(ManagedLocalAppRuntime::default);
        if actual_surface_changed {
            let connection_id = self
                .managed_local_app_connection_id(&scope.server_name())
                .await;
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            });
        }
        if previous.is_some() && actual_catalog_changed {
            let connection_id = self
                .managed_local_app_connection_id(&scope.server_name())
                .await;
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: None,
            });
        }
        Ok(server)
    }
    /// Remove a published Local App logical server after Host has stopped new
    /// calls. The notification tells consumers to evict its exposed tools.
    pub async fn unregister_managed_local_app(&self, app_id: &str) -> Result<bool, McpError> {
        if !is_local_app_id(app_id) {
            return Err(McpError::Internal("invalid Local App identity".into()));
        }
        let removed = self.managed_local_apps.write().await.remove(app_id);
        if removed.is_some() {
            self.managed_local_app_runtime.write().await.remove(app_id);
            // A deleted app can no longer be selected or called. Remove its
            // logical exposure from every conversation in the same commit
            // boundary; no stale FQN survives deletion.
            self.remove_local_app_exposures(app_id).await;
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: format!("local_app_{app_id}"),
                connection_id: McpConnectionId::new(),
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            });
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: format!("local_app_{app_id}"),
                connection_id: McpConnectionId::new(),
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: None,
            });
        }
        Ok(removed.is_some())
    }
    /// Lightweight logical-server count; all entries continue to use this
    /// registry's one physical transport substrate.
    pub async fn managed_local_app_count(&self) -> usize {
        self.managed_local_apps.read().await.len()
    }
    /// Snapshot every published Local App logical server.
    pub async fn managed_local_apps(&self) -> Vec<ManagedLocalAppServer> {
        let mut apps: Vec<ManagedLocalAppServer> = self
            .managed_local_apps
            .read()
            .await
            .values()
            .cloned()
            .collect();
        apps.sort_by(|left, right| left.scope.app_id.cmp(&right.scope.app_id));
        apps
    }
    /// There is exactly one physical transport owned by this registry.
    #[must_use]
    pub fn physical_transport_count(&self) -> usize {
        1
    }
    pub async fn managed_local_app(&self, app_id: &str) -> Option<ManagedLocalAppServer> {
        self.managed_local_apps.read().await.get(app_id).cloned()
    }
    /// Snapshot the Host-owned runtime overlay for one managed Local App.
    pub async fn managed_local_app_runtime(&self, app_id: &str) -> Option<ManagedLocalAppRuntime> {
        self.managed_local_app_runtime
            .read()
            .await
            .get(app_id)
            .cloned()
    }
    /// Update the Host-owned runtime overlay for one managed Local App.
    ///
    /// This is the intended seam for service enable/disable, per-tool
    /// allowlists, and per-app widget resource publication without changing
    /// the immutable published catalog record.
    pub async fn set_managed_local_app_runtime(
        &self,
        app_id: &str,
        enabled: bool,
        enabled_tools: Option<Vec<String>>,
        resource: Option<ManagedLocalAppResource>,
    ) -> Result<ManagedLocalAppRuntime, McpError> {
        if !is_local_app_id(app_id) {
            return Err(McpError::Internal("invalid Local App identity".into()));
        }
        let Some(server) = self.managed_local_app(app_id).await else {
            return Err(McpError::ToolNotFound(app_id.into()));
        };
        let enabled_tools = enabled_tools
            .map(|tools| {
                let mut normalized = Vec::with_capacity(tools.len());
                for tool in tools {
                    if !is_local_app_tool_name(&tool) {
                        return Err(McpError::ToolNotFound(tool));
                    }
                    let _ = server.scope.tool_full_name(&tool)?;
                    if !normalized.iter().any(|existing| existing == &tool) {
                        normalized.push(tool);
                    }
                }
                normalized.sort();
                Ok(normalized)
            })
            .transpose()?;
        if let Some(resource) = resource.as_ref() {
            if resource.uri.trim().is_empty() || resource.name.trim().is_empty() {
                return Err(McpError::Internal(
                    "managed Local App resource metadata is incomplete".into(),
                ));
            }
        }
        let mut runtimes = self.managed_local_app_runtime.write().await;
        let previous = runtimes.get(app_id).cloned().unwrap_or_default();
        let resource_generation_changed =
            previous.resource != resource || previous.enabled != enabled;
        let runtime = ManagedLocalAppRuntime {
            enabled,
            enabled_tools,
            resource,
            resource_generation: previous.resource_generation
                + u64::from(resource_generation_changed),
        };
        let (tools_changed, resources_changed) =
            Self::managed_local_app_runtime_changed(&previous, &runtime);
        runtimes.insert(app_id.to_string(), runtime.clone());
        drop(runtimes);
        if !enabled {
            self.remove_local_app_exposures(app_id).await;
        }
        let connection_id = self
            .managed_local_app_connection_id(&server.scope.server_name())
            .await;
        if tools_changed {
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: server.scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            });
        }
        if resources_changed {
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: server.scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: None,
            });
        }
        Ok(runtime)
    }
    /// Expose a published Local App in one conversation. Exposure is lazy and
    /// bounded: at most eight logical apps are retained, with unpinned,
    /// idle least-recently-used entries evicted first. A pinned entry is the
    /// only hard pin; merely listing or calling an app keeps it recent but
    /// does not make it ineligible for eviction.
    pub async fn expose_managed_local_app_with_diff(
        &self,
        conversation_id: &str,
        app_id: &str,
        pin: bool,
    ) -> Result<LocalAppExposureUpdate, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        if self.managed_local_app(app_id).await.is_none() {
            return Err(McpError::ToolNotFound(app_id.into()));
        }
        if self
            .managed_local_app_runtime(app_id)
            .await
            .is_some_and(|runtime| !runtime.enabled)
        {
            return Err(McpError::ToolNotFound(app_id.into()));
        }

        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .entry(conversation_id.to_string())
            .or_default();
        state.next_sequence = state.next_sequence.saturating_add(1);
        if let Some(entry) = state.entries.get_mut(app_id) {
            entry.last_used = state.next_sequence;
            if pin && !entry.pinned {
                state.next_generation = state.next_generation.saturating_add(1);
                entry.pinned = true;
                entry.exposure_generation = state.next_generation;
            }
            return Ok(LocalAppExposureUpdate {
                exposure: entry.clone(),
                evicted_app_id: None,
            });
        }

        let mut evicted_app_id = None;
        if state.entries.len() >= LOCAL_APP_MAX_EXPOSED {
            let evict = state
                .entries
                .iter()
                .filter(|(_, entry)| !entry.pinned && entry.in_flight == 0)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| id.clone());
            let Some(evict) = evict else {
                let mut pinned: Vec<&str> = state
                    .entries
                    .values()
                    .filter(|entry| entry.pinned)
                    .map(|entry| entry.app_id.as_str())
                    .collect();
                pinned.sort_unstable();
                return Err(McpError::Internal(format!(
                    "exposure_capacity_reached: pinned apps [{}]",
                    pinned.join(",")
                )));
            };
            state.entries.remove(&evict);
            evicted_app_id = Some(evict);
        }

        state.next_generation = state.next_generation.saturating_add(1);
        let entry = LocalAppExposure {
            app_id: app_id.to_string(),
            pinned: pin,
            in_flight: 0,
            last_used: state.next_sequence,
            exposure_generation: state.next_generation,
        };
        state.entries.insert(app_id.to_string(), entry.clone());
        Ok(LocalAppExposureUpdate {
            exposure: entry,
            evicted_app_id,
        })
    }
    pub async fn expose_managed_local_app(
        &self,
        conversation_id: &str,
        app_id: &str,
        pin: bool,
    ) -> Result<LocalAppExposure, McpError> {
        Ok(self
            .expose_managed_local_app_with_diff(conversation_id, app_id, pin)
            .await?
            .exposure)
    }
    /// Mark an already exposed app as recently used without hard-pinning it.
    pub async fn touch_local_app_exposure(
        &self,
        conversation_id: &str,
        app_id: &str,
    ) -> Result<LocalAppExposure, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .get_mut(conversation_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let entry = state
            .entries
            .get_mut(app_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        entry.last_used = state.next_sequence;
        Ok(entry.clone())
    }
    /// Change the hard-pin bit for one exposed app. Pin state is explicit and
    /// therefore advances the exposure generation independently of catalog or
    /// authoring revisions.
    pub async fn pin_local_app_exposure(
        &self,
        conversation_id: &str,
        app_id: &str,
        pinned: bool,
    ) -> Result<LocalAppExposure, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .get_mut(conversation_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        let changed = state
            .entries
            .get(app_id)
            .map(|entry| entry.pinned != pinned)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        if changed {
            state.next_generation = state.next_generation.saturating_add(1);
        }
        let entry = state.entries.get_mut(app_id).expect("checked above");
        if changed {
            entry.exposure_generation = state.next_generation;
        }
        entry.pinned = pinned;
        Ok(entry.clone())
    }
    /// Begin one call through an exposed app. The registry rejects calls
    /// instead of queueing them without bound; callers must release the lease
    /// with [`Self::end_local_app_call`] on completion/cancellation.
    pub async fn begin_local_app_call(
        &self,
        conversation_id: &str,
        app_id: &str,
    ) -> Result<LocalAppExposure, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .get_mut(conversation_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        let total_in_flight: usize = state.entries.values().map(|entry| entry.in_flight).sum();
        let entry = state
            .entries
            .get_mut(app_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        if entry.in_flight >= LOCAL_APP_MAX_IN_FLIGHT_PER_APP
            || total_in_flight >= LOCAL_APP_MAX_IN_FLIGHT_PER_CONVERSATION
        {
            return Err(McpError::Internal(
                "rate_limited: retry after 1000ms".into(),
            ));
        }
        entry.in_flight += 1;
        state.next_sequence = state.next_sequence.saturating_add(1);
        entry.last_used = state.next_sequence;
        Ok(entry.clone())
    }
    /// Release a call lease. Releasing an unknown lease is intentionally
    /// idempotent so timeout/cancel cleanup cannot turn into a second error.
    pub async fn end_local_app_call(&self, conversation_id: &str, app_id: &str) {
        let mut conversations = self.local_app_exposures.write().await;
        if let Some(state) = conversations.get_mut(conversation_id) {
            if let Some(entry) = state.entries.get_mut(app_id) {
                entry.in_flight = entry.in_flight.saturating_sub(1);
            }
        }
    }
    /// Snapshot the logical exposure metadata for one conversation in recency
    /// order. Tool DTOs are intentionally not part of this API.
    pub async fn local_app_exposures(&self, conversation_id: &str) -> Vec<LocalAppExposure> {
        let conversations = self.local_app_exposures.read().await;
        let Some(state) = conversations.get(conversation_id) else {
            return Vec::new();
        };
        let mut entries: Vec<LocalAppExposure> = state.entries.values().cloned().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
        entries
    }
}
