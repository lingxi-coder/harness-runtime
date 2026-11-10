use super::*;

fn error_field(
    errors: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
    name: &mcp::McpServerName,
    detail: Utf16JsonProjection,
) {
    if let Some(field) = errors.iter_mut().find(|(key, _)| key == name.utf16_units()) {
        field.1 = detail;
    } else {
        errors.push((name.utf16_units().to_vec(), detail));
    }
}
fn owned(config: &mcp::McpServerConfig) -> bool {
    config.scope == mcp::ConfigScope::Dynamic
        && mcp::McpServerProvenance::from_config(config).source != "plugin"
}
fn protected(config: &mcp::McpServerConfig, names: &[mcp::McpServerName]) -> bool {
    names.iter().any(|name| {
        config.name_matches_utf16(name.utf16_units())
            || config.normalized_name_for_mcp() == name.normalized()
    })
}
fn same_ordinary_config(a: &mcp::McpServerConfig, b: &mcp::McpServerConfig) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.metadata.cli_owned = false;
    b.metadata.cli_owned = false;
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

impl ConversationOrchestrator {
    /// Native reconnect is a real transport lifecycle operation. SDK servers
    /// are owned by the caller and have the native control rejection.
    pub async fn reconnect_mcp_from_control(
        &self,
        name: &Utf16JsonProjection,
    ) -> Result<(), Utf16JsonProjection> {
        let Some(units) = name.string_units("") else {
            return Err("mcp_reconnect: serverName must be a string".into());
        };
        let not_found = || -> Utf16JsonProjection {
            format!(
                "Server not found: {}",
                mcp::normalization::sanitize_control_name_units(&units)
            )
            .into()
        };
        let registry = self.mcp_registry.as_ref().ok_or_else(not_found)?;
        let config = registry
            .connections
            .read()
            .await
            .values()
            .map(|state| state.config())
            .find(|config| config.name_matches_utf16(&units))
            .cloned()
            .ok_or_else(not_found)?;
        if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
            return Err("SDK servers should be handled in print.ts".into());
        }
        registry
            .reconnect_preserving_auth(&config.name)
            .await
            .map_err(|error| {
                error
                    .source_diagnostic()
                    .unwrap_or_else(|| error.to_string().into())
            })
    }

    pub async fn toggle_mcp_from_control(
        &self,
        name: &Utf16JsonProjection,
        enabled: &Utf16JsonProjection,
        retire_sdk: &(dyn Fn(&[String]) + Send + Sync),
    ) -> Result<(), Utf16JsonProjection> {
        let Some(units) = name.string_units("") else {
            return Err("mcp_toggle: serverName must be a string".into());
        };
        let Some(enabled) = enabled.at_pointer("").and_then(|value| value.as_bool()) else {
            return Err("mcp_toggle: enabled must be a boolean".into());
        };
        let missing = || {
            Utf16JsonProjection::from(format!(
                "Server not found: {}",
                mcp::normalization::sanitize_control_name_units(&units)
            ))
        };
        let registry = self.mcp_registry.as_ref().ok_or_else(missing)?;
        let config = registry
            .connections
            .read()
            .await
            .values()
            .map(|state| state.config())
            .find(|config| config.name_matches_utf16(&units))
            .cloned()
            .ok_or_else(missing)?;
        if !enabled && matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
            // A pending SDK handshake must be retired before taking its
            // registry lifecycle lock. Its caller need not answer initialize.
            retire_sdk(std::slice::from_ref(&config.name));
        }
        registry
            .set_disabled(&config.name, !enabled)
            .await
            .map_err(|error| {
                error
                    .source_diagnostic()
                    .unwrap_or_else(|| error.to_string().into())
            })?;
        mcp::tool_policy::synchronize_permission_policies(registry, self.perms.as_ref()).await;
        Ok(())
    }

    /// Reconcile only host-controlled dynamic entries. Settings, agent and
    /// plugin ownership remain in their existing registries and admission paths.
    /// SDK connects are returned for admission after the outer success reply.
    pub async fn set_mcp_servers_from_control(
        &self,
        mut incoming: Vec<mcp::McpServerConfig>,
        initial_errors: Utf16JsonProjection,
        remote: bool,
        operator_names: &[mcp::McpServerName],
        retire_sdk: &(dyn Fn(&[String]) + Send + Sync),
    ) -> Result<(Utf16JsonProjection, Vec<mcp::McpServerConfig>), String> {
        let Some(registry) = &self.mcp_registry else {
            return Err("MCP registry is unavailable".into());
        };
        registry.finalize_authored_server_names(&mut incoming).await;
        for config in &mut incoming {
            if let McpTransportSpec::SdkControl { control_channel_id } = &mut config.spec {
                *control_channel_id = config.name.clone();
            }
        }
        let mut errors = initial_errors
            .object_entries()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| {
                Ok((
                    entry.code_units.to_vec(),
                    entry.value.to_projection().map_err(source_error)?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let current_order = registry.control_server_names().await;
        let current = registry.connections.read().await.clone();
        incoming.retain(|config| {
            let collision = current.values().find(|state| {
                state
                    .config()
                    .name_matches_utf16(&config.authored_name_units())
            });
            let denied = protected(config, operator_names)
                || collision.is_some_and(|state| !owned(state.config()));
            if denied {
                error_field(
                    &mut errors,
                    &config.authored_name(),
                    "Configured via --mcp-config on this host (operator-supplied); ignored".into(),
                );
            }
            !denied
        });
        let has_sdk = incoming
            .iter()
            .any(|config| matches!(config.spec, McpTransportSpec::SdkControl { .. }));
        let mut to_retire = Vec::new();
        for key in current_order {
            let Some(state) = current.get(&key) else {
                continue;
            };
            let config = state.config();
            if !owned(config) || protected(config, operator_names) {
                continue;
            }
            let sdk = matches!(config.spec, McpTransportSpec::SdkControl { .. });
            if remote && !has_sdk && sdk {
                continue;
            }
            let desired = incoming
                .iter()
                .find(|incoming| incoming.name_matches_utf16(&config.authored_name_units()));
            let changed = desired.is_some_and(|next| {
                matches!(next.spec, McpTransportSpec::SdkControl { .. }) != sdk
                    || (!sdk && !same_ordinary_config(config, next))
            });
            if desired.is_none() || changed {
                to_retire.push((
                    config.clone(),
                    desired.is_none()
                        || desired.is_some_and(|next| {
                            matches!(next.spec, McpTransportSpec::SdkControl { .. }) != sdk
                        }),
                ));
            }
        }
        retire_sdk(
            &to_retire
                .iter()
                .filter(|(config, _)| matches!(config.spec, McpTransportSpec::SdkControl { .. }))
                .map(|(config, _)| config.name.clone())
                .collect::<Vec<_>>(),
        );
        let mut removed = Vec::new();
        let mut removed_keys = Vec::new();
        for (config, report_removed) in to_retire {
            match registry.retire_control_server(&config).await {
                Ok(true) => {
                    removed_keys.push(config.name.clone());
                    if report_removed {
                        removed.push(source_text_units(config.authored_name_units()));
                    }
                }
                Ok(false) => {}
                Err(error) => error_field(
                    &mut errors,
                    &config.authored_name(),
                    error
                        .source_diagnostic()
                        .unwrap_or_else(|| error.to_string().into()),
                ),
            }
        }
        let mut added = Vec::new();
        let mut connect_sdk = Vec::new();
        let mut policies = Vec::new();
        for config in incoming {
            let name = config.authored_name();
            let existing = registry.get_config_by_key(&config.name).await;
            if let Some(existing) = existing {
                if matches!(existing.spec, McpTransportSpec::SdkControl { .. })
                    && matches!(config.spec, McpTransportSpec::SdkControl { .. })
                {
                    registry.replace_sdk_control_tools(&config).await;
                    policies.push(config);
                    continue;
                }
                if same_ordinary_config(&existing, &config) {
                    continue;
                }
            }
            if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
                match registry.seed_sdk_control_pending(config.clone()).await {
                    Ok(()) => {
                        added.push(source_text_units(name.utf16_units().to_vec()));
                        policies.push(config.clone());
                        connect_sdk.push(config);
                    }
                    Err(error) => error_field(
                        &mut errors,
                        &name,
                        error
                            .source_diagnostic()
                            .unwrap_or_else(|| error.to_string().into()),
                    ),
                }
            } else {
                added.push(source_text_units(name.utf16_units().to_vec()));
                if let Err(error) = registry.connect(config.clone()).await {
                    let detail = config
                        .config_error
                        .clone()
                        .map(Utf16JsonProjection::from)
                        .or_else(|| error.source_diagnostic())
                        .unwrap_or_else(|| error.to_string().into());
                    error_field(&mut errors, &name, detail.clone());
                    if registry.get_config_by_key(&config.name).await.is_none() {
                        registry.connections.write().await.insert(
                            config.name.clone(),
                            mcp::McpConnectionState::Failed {
                                config,
                                error: detail
                                    .string_at("")
                                    .unwrap_or("Connection failed")
                                    .to_owned(),
                                attempts: 1,
                            },
                        );
                    }
                }
            }
        }
        mcp::tool_policy::replace_sdk_control_policies(
            registry,
            &policies,
            &removed_keys,
            self.perms.as_ref(),
        )
        .await;
        mcp::tool_policy::synchronize_permission_policies(registry, self.perms.as_ref()).await;
        let result = source_object(vec![
            (
                "added".into(),
                Utf16JsonProjection::array(added).map_err(source_error)?,
            ),
            (
                "removed".into(),
                Utf16JsonProjection::array(removed).map_err(source_error)?,
            ),
            (
                "errors".into(),
                Utf16JsonProjection::object_units(errors).map_err(source_error)?,
            ),
        ])?;
        Ok((result, connect_sdk))
    }

    /// Finish SDK admission after set_servers has acknowledged its new roster.
    pub async fn connect_control_mcp_servers(&self, configs: Vec<mcp::McpServerConfig>) {
        let Some(registry) = &self.mcp_registry else {
            return;
        };
        futures::future::join_all(configs.into_iter().map(|config| async move {
            let _ = registry.connect_seeded_sdk_control(config).await;
        }))
        .await;
        mcp::tool_policy::synchronize_permission_policies(registry, self.perms.as_ref()).await;
    }

    pub fn replace_control_mcp_denied_tools(&self, tools: Vec<String>) {
        self.perms.replace_sdk_mcp_denied_tools(tools);
    }
}
