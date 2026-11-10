//! Caller-owned SDK registrations coexist with ordinary configured clients.
use super::*;

impl McpRegistry {
    /// Allocate an SDK's independent routing slot only when its authored name
    /// is already owned by an ordinary client. Public identity stays exact.
    pub async fn finalize_control_server_names(&self, configs: &mut [McpServerConfig]) {
        let existing = self
            .connections
            .read()
            .await
            .values()
            .map(|state| state.config().clone())
            .collect::<Vec<_>>();
        let ordinary = existing
            .iter()
            .filter(|config| !config.metadata.sdk_control_owned)
            .cloned()
            .collect::<Vec<_>>();
        let mut routing_existing = ordinary.clone();
        routing_existing.extend(
            existing
                .iter()
                .filter(|config| config.metadata.sdk_control_owned)
                .cloned(),
        );
        crate::finalize_mcp_server_names_against(configs, &routing_existing);
        let mut used = existing
            .iter()
            .map(|config| config.name.clone())
            .chain(configs.iter().map(|config| config.name.clone()))
            .collect::<HashSet<_>>();
        for config in configs
            .iter_mut()
            .filter(|config| config.metadata.sdk_control_owned)
        {
            let authored = config.authored_name();
            if let Some(previous) = existing.iter().find(|previous| {
                previous.metadata.sdk_control_owned
                    && previous.name_matches_utf16(authored.utf16_units())
            }) {
                config.name = previous.name.clone();
                config.metadata.authored_name = previous.metadata.authored_name.clone();
            } else if ordinary.iter().any(|previous| {
                previous.name_matches_utf16(authored.utf16_units()) || previous.name == config.name
            }) {
                let key = loop {
                    let key = format!(
                        "__lingxi_sdk_control_{}",
                        lingxi_core::types::McpConnectionId::new()
                    );
                    if used.insert(key.clone()) {
                        break key;
                    }
                };
                config.metadata.authored_name = Some(authored);
                config.name = key;
            }
            if let McpTransportSpec::SdkControl { control_channel_id } = &mut config.spec {
                *control_channel_id = config.name.clone();
            }
        }
    }

    /// Actual control registrations, independently of any ordinary same-name
    /// row. Entries retain their first-registration SDK settings.
    pub async fn sdk_control_config(&self, units: &[u16]) -> Option<McpServerConfig> {
        self.connections
            .read()
            .await
            .values()
            .map(McpConnectionState::config)
            .find(|config| config.metadata.sdk_control_owned && config.name_matches_utf16(units))
            .cloned()
    }

    pub async fn sdk_control_is_connected(&self, units: &[u16]) -> bool {
        self.connections.read().await.values().any(|state| {
            state.config().metadata.sdk_control_owned
                && state.config().name_matches_utf16(units)
                && matches!(state, McpConnectionState::Connected { .. })
        })
    }

    /// Native status/reconnect/toggle use ordinary owners first; direct calls
    /// subsequently choose the first connected candidate from this ordering.
    pub async fn control_candidates(
        &self,
        units: &[u16],
        normalized_match: bool,
    ) -> Vec<(String, McpConnectionState)> {
        let order = self.control_server_names().await;
        let connections = self.connections.read().await;
        let normalized = crate::McpServerName::from_utf16(units.to_vec()).normalized();
        let mut candidates = order
            .into_iter()
            .filter_map(|key| {
                let state = connections.get(&key)?;
                if key != state.config().name {
                    return None;
                }
                (state.config().name_matches_utf16(units)
                    || normalized_match && state.config().normalized_name_for_mcp() == normalized)
                    .then(|| (key, state.clone()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, state)| {
            matches!(state.config().spec, McpTransportSpec::SdkControl { .. })
        });
        candidates
    }

    pub fn subscribe_model_tool_changes(&self) -> broadcast::Receiver<()> {
        self.model_tool_changes.subscribe()
    }

    pub(crate) fn claim_sdk_model_namespace(&self, config: &McpServerConfig) {
        if config.metadata.sdk_control_owned {
            self.sdk_control_catalog_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(config.normalized_name_for_mcp(), Some(config.name.clone()));
            let _ = self.model_tool_changes.send(());
        }
    }

    pub(crate) fn clear_sdk_model_namespace(&self, config: &McpServerConfig) {
        if config.metadata.sdk_control_owned {
            self.sdk_control_catalog_owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(config.normalized_name_for_mcp(), None);
            let _ = self.model_tool_changes.send(());
        }
    }

    pub(crate) fn record_control_catalog_publication(&self, key: &str, config: &McpServerConfig) {
        if key != config.name {
            return;
        }
        let mut owners = self
            .sdk_control_catalog_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner) = owners.get_mut(&config.normalized_name_for_mcp()) {
            *owner = Some(key.to_owned());
        }
    }

    pub(crate) fn project_control_model_catalog(
        &self,
        connections: &mut HashMap<String, McpConnectionState>,
    ) {
        let owners = self
            .sdk_control_catalog_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, state) in connections.iter_mut() {
            let namespace = state.config().normalized_name_for_mcp();
            if owners
                .get(&namespace)
                .is_some_and(|owner| owner.as_ref() != Some(key))
            {
                match state {
                    McpConnectionState::Connected { tools, .. }
                    | McpConnectionState::Cached { tools, .. } => tools.clear(),
                    _ => {}
                }
            }
        }
    }

    pub async fn control_status_tools(
        &self,
        config: &McpServerConfig,
    ) -> Vec<lingxi_core::host::McpToolDto> {
        let connections = self.connections.read().await;
        let owners = self
            .sdk_control_catalog_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = match owners.get(&config.normalized_name_for_mcp()) {
            Some(Some(key)) => key,
            Some(None) => return Vec::new(),
            None => &config.name,
        };
        match connections.get(key) {
            Some(
                McpConnectionState::Connected { tools, .. }
                | McpConnectionState::Cached { tools, .. },
            ) => tools.clone(),
            _ => Vec::new(),
        }
    }
}
