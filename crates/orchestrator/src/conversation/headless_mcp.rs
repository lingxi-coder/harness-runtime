//! SDK controls over the session's existing MCP registry and UI roster.
use super::*;
use lingxi_core::host::{McpActionState, McpError, McpTransportSpec};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ToolUseId;
use serde_json::{json, Value};

#[path = "headless_mcp_auth.rs"]
mod auth;
#[path = "headless_mcp_control_tool.rs"]
mod control_tool;
#[path = "headless_mcp_servers.rs"]
mod servers;

fn control_rpc_error(error: &mcp::McpClientError) -> Utf16JsonProjection {
    if let mcp::McpClientError::Rpc(message) = error {
        let units = message.string_units("").unwrap_or_default();
        let prefix = "router: remote error: code="
            .encode_utf16()
            .collect::<Vec<_>>();
        let delimiter = ", message=".encode_utf16().collect::<Vec<_>>();
        if units.starts_with(&prefix) {
            if let Some(at) = units
                .windows(delimiter.len())
                .position(|part| part == delimiter)
            {
                return source_text_units(units[at + delimiter.len()..].to_vec());
            }
        }
    }
    error
        .source_diagnostic()
        .unwrap_or_else(|| error.to_string().into())
}

fn control_error_display(error: Utf16JsonProjection) -> String {
    error.string_at("").unwrap_or_default().to_owned()
}

fn cap_control_error(error: Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = error
        .string_units("")
        .unwrap_or_default()
        .into_iter()
        .take(2000)
        .collect::<Vec<_>>();
    if units
        .last()
        .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
    {
        units.pop();
    }
    source_text_units(units)
}

struct ParsedControlMcpTool {
    server_name: String,
    server_name_units: Vec<u16>,
    tool_name: Option<String>,
    tool_name_source: Option<Utf16JsonProjection>,
}

fn parse_control_mcp_tool_name(value: &Utf16JsonProjection) -> Option<ParsedControlMcpTool> {
    let units = value.string_units("")?;
    if !units.starts_with(&[109, 99, 112, 95, 95]) {
        return None;
    }
    let rest = &units[5..];
    let delimiter = rest.windows(2).position(|pair| pair == [95, 95]);
    let server = &rest[..delimiter.unwrap_or(rest.len())];
    if server.is_empty() {
        return None;
    }
    let tool = delimiter.map(|at| &rest[at + 2..]);
    Some(ParsedControlMcpTool {
        server_name: mcp::normalization::sanitize_control_name_units(server),
        server_name_units: server.to_vec(),
        tool_name: tool.map(String::from_utf16_lossy),
        tool_name_source: tool.map(|units| {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
                .expect("source tool name")
        }),
    })
}

struct McpControlTarget {
    parsed: ParsedControlMcpTool,
    server: String,
    normalized_server: String,
    client: Arc<mcp::McpClient>,
}

impl ConversationOrchestrator {
    /// Resolve and validate a direct MCP target before any file staging.
    pub async fn prepare_mcp_control_call(
        &self,
        full_name: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        self.prepare_mcp_control_call_source(&full_name.into(), cancel)
            .await
            .map_err(control_error_display)
    }

    /// Source-preserving direct-control target validation.
    pub async fn prepare_mcp_control_call_source(
        &self,
        full_name: &Utf16JsonProjection,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), Utf16JsonProjection> {
        self.mcp_control_target(full_name, cancel).await.map(|_| ())
    }

    async fn mcp_control_target(
        &self,
        full_name: &Utf16JsonProjection,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<McpControlTarget, Utf16JsonProjection> {
        let parsed = parse_control_mcp_tool_name(full_name)
            .filter(|name| name.tool_name.as_ref().is_some_and(|name| !name.is_empty()))
            .ok_or_else(|| {
                format!(
                    "Not a fully-qualified MCP tool name: {}",
                    mcp::normalization::sanitize_control_name_units(
                        &full_name.string_units("").unwrap_or_default()
                    )
                )
            })?;
        let registry = self
            .mcp_registry
            .as_ref()
            .ok_or_else(|| format!("MCP server not connected: {}", parsed.server_name))?;
        let candidates = registry
            .control_candidates(&parsed.server_name_units, true)
            .await;
        let selected = candidates
            .iter()
            .find(|(_, state)| {
                !state.config().disabled
                    && matches!(state, mcp::McpConnectionState::Connected { .. })
            })
            .or_else(|| {
                candidates.iter().find(|(_, state)| {
                    !state.config().disabled
                        && matches!(state, mcp::McpConnectionState::Cached { .. })
                })
            })
            .or_else(|| candidates.first());
        let config = selected
            .map(|(_, state)| state.config().clone())
            .ok_or_else(|| format!("MCP server not connected: {}", parsed.server_name))?;
        if config.disabled {
            return Err(format!("MCP server not connected: {}", parsed.server_name).into());
        }
        let server = config.name.clone();
        let normalized_server = config.normalized_name_for_mcp();
        // Keep startup/lazy connection ownership in the registry. A direct
        // control request does not create a second transport or model turn.
        let client = tokio::select! {
            _ = cancel.cancelled() => return Err(format!("mcp_call cancelled by client: {}", parsed.server_name).into()),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), registry.ensure_connected_client_by_key(&server)) => result
                .map_err(|_| format!("MCP server not connected: {}", parsed.server_name))?
                .map_err(|_| format!("MCP server not connected: {}", parsed.server_name))?,
        };
        if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
            return Err(format!("mcp_call does not support SDK MCP servers. SDK servers are caller-provided — invoke {} directly.", parsed.server_name).into());
        }
        Ok(McpControlTarget {
            parsed,
            server,
            normalized_server,
            client,
        })
    }

    /// Execute the host's direct MCP control call independently of the model
    /// provider. SDK servers belong to the caller and must be invoked there.
    /// An optional permission check uses the normal owned tool dispatcher.
    pub async fn call_mcp_control_tool(
        self: &Arc<Self>,
        full_name: &str,
        input: Utf16JsonProjection,
        check_permissions: bool,
        program_result: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::McpToolResultDto, String> {
        self.call_mcp_control_tool_source(
            &full_name.into(),
            input,
            check_permissions,
            program_result,
            cancel,
        )
        .await
        .map_err(control_error_display)
    }

    /// The caller's FQN and raw fallback name retain JavaScript string units.
    pub async fn call_mcp_control_tool_source(
        self: &Arc<Self>,
        full_name: &Utf16JsonProjection,
        input: Utf16JsonProjection,
        check_permissions: bool,
        program_result: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<lingxi_core::host::McpToolResultDto, Utf16JsonProjection> {
        self.call_mcp_control_tool_source_with_reason(
            full_name,
            input,
            check_permissions,
            program_result,
            cancel,
            Arc::new(std::sync::Mutex::new("AbortError: remote-cancel".into())),
        )
        .await
        .map_err(cap_control_error)
    }

    /// Source cancellation ownership shared with a staged execution deadline.
    pub async fn call_mcp_control_tool_source_with_reason(
        self: &Arc<Self>,
        full_name: &Utf16JsonProjection,
        input: Utf16JsonProjection,
        check_permissions: bool,
        program_result: bool,
        cancel: tokio_util::sync::CancellationToken,
        cancellation_reason: Arc<std::sync::Mutex<Utf16JsonProjection>>,
    ) -> Result<lingxi_core::host::McpToolResultDto, Utf16JsonProjection> {
        let McpControlTarget {
            parsed,
            server,
            normalized_server,
            client,
        } = self.mcp_control_target(full_name, cancel.clone()).await?;
        let registry = self.mcp_registry.as_ref().expect("resolved MCP registry");
        let requested_tool = parsed.tool_name.expect("validated tool name");
        let known = registry
            .connections
            .read()
            .await
            .get(&server)
            .and_then(|state| match state {
                mcp::McpConnectionState::Connected { tools, .. }
                | mcp::McpConnectionState::Cached { tools, .. } => tools
                    .iter()
                    .find(|tool| full_name.string_at("") == Some(tool.full_name.as_str()))
                    .cloned(),
                _ => None,
            });
        let raw_tool = known
            .as_ref()
            .map_or(requested_tool, |tool| tool.tool_name.clone());
        let wire_name = known.as_ref().map_or_else(
            || {
                parsed
                    .tool_name_source
                    .clone()
                    .expect("validated tool name")
            },
            |tool| tool.wire_tool_name_source(),
        );
        let canonical = format!(
            "mcp__{normalized_server}__{}",
            mcp::normalization::normalize_name_for_mcp(&raw_tool)
        );
        let dispatch_name = canonical.clone();
        if !check_permissions {
            let result = tokio::select! {
                _ = cancel.cancelled() => Err(format!("mcp_call cancelled by client: {}", parsed.server_name).into()),
                result = client.call_control_tool_with_wire_name(&dispatch_name,wire_name,input,cancellation_reason,None) => result.map_err(|error| control_rpc_error(&error)),
            }?;
            return self
                .finish_mcp_control_result(result, &server, &raw_tool, program_result)
                .await;
        }
        let base = self.find_tool_for_dispatch(&canonical).ok_or_else(|| {
            "The tool did not run: this session has no such tool on that server.".to_owned()
        })?;
        let id = ToolUseId::from(format!("toolu_mcp_call_{}", uuid::Uuid::new_v4().simple()));
        let result = Arc::new(tokio::sync::Mutex::new(None));
        let failure = Arc::new(tokio::sync::Mutex::new(None));
        let prepared = Arc::new(control_tool::ControlMcpTool {
            base,
            owner: self.clone(),
            client,
            dispatch_name,
            server: server.clone(),
            diagnostic_server: parsed.server_name,
            raw_tool,
            wire_name,
            cancellation_reason,
            program_result,
            expected_id: id.clone(),
            result: result.clone(),
            failure: failure.clone(),
        });
        let meta_may_change = self
            .control_mcp_has_result_observers(&canonical, &input, &id)
            .await;
        let tool_uses = vec![(id.clone(), canonical.clone(), input.clone(), None)];
        let mut context =
            crate::turn_loop::streaming_tool_context_base(self, self.snapshot_history().await)
                .await;
        context.cancellation_scope = tool_api::context::ToolCancellationScope::HostRequest;
        let dispatched =
            crate::turn_loop::tool_dispatch::dispatch_tool_uses_tracked_deferred_with_facts(
                self,
                &tool_uses,
                Some(cancel.clone()),
                None,
                None,
                Some(prepared),
                Some(context),
                None,
                None,
            )
            .await
            .map_err(|error| format!("The tool did not run: {error}"))?;
        let (mut blocks, _, _, modifiers) =
            crate::turn_loop::tool_dispatch::finish_direct_tool_dispatch(self, dispatched)
                .await
                .map_err(|error| format!("The tool did not run: {error}"))?;
        crate::turn_loop::apply_model_context_modifiers(self, modifiers)
            .await
            .map_err(|error| format!("The tool did not run: {error}"))?;
        let _ = self.take_queued_hook_attachments(&id).await;
        if let Some(error) = failure.lock().await.take() {
            return Err(error);
        }
        let captured = result.lock().await.take();
        if captured.is_some() && cancel.is_cancelled() {
            return Err("The tool ran, and its result was held back: the call was cancelled before its hooks finished.".into());
        }
        let block = blocks.pop().ok_or_else(|| {
            "The tool did not run: no permission decision was reached.".to_owned()
        })?;
        let source = self
            .transcript
            .tool_use_results
            .lock()
            .await
            .remove(&id.to_string());
        let _ = self
            .transcript
            .tool_use_mcp_meta
            .lock()
            .await
            .remove(&id.to_string());
        let lingxi_core::types::ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        else {
            return Err("The tool did not run: no permission decision was reached.".into());
        };
        let Some(mut result) = captured else {
            return Err(format!(
                "The tool did not run: {}",
                content
                    .string_at("")
                    .unwrap_or("no permission decision was reached.")
            )
            .into());
        };
        if is_error == Some(true) {
            return Err(format!(
                "The tool ran, and its result was held back: {}",
                content.string_at("").unwrap_or("the call was blocked.")
            )
            .into());
        }
        if let Some(source) = source {
            result.content = source;
        }
        if meta_may_change
            || self
                .control_mcp_has_result_observers(&canonical, &input, &id)
                .await
        {
            result.structured_content = None;
            result.meta = None;
        }
        Ok(result)
    }

    async fn control_mcp_has_result_observers(
        &self,
        name: &str,
        input: &Utf16JsonProjection,
        id: &ToolUseId,
    ) -> bool {
        let Some(registry) = &self.lifecycle_runtime.hook_registry else {
            return false;
        };
        let context = self.expansion_hook_context().await;
        let event = HookEvent::PostToolUse {
            tool_name: name.into(),
            tool_input: input.clone(),
            tool_output: Utf16JsonProjection::parse("null").unwrap(),
            tool_use_id: id.clone(),
            duration_ms: None,
        };
        let registry = registry.read().await;
        registry
            .match_event(&event, &context)
            .into_iter()
            .any(|hook| {
                !matches!(
                    hook.executor,
                    hooks::definition::HookExecutor::Builtin { .. }
                )
            })
            || registry
                .mod_host()
                .is_some_and(|host| host.has_event("tool.call"))
    }

    async fn finish_mcp_control_result(
        &self,
        mut result: lingxi_core::host::McpToolResultDto,
        server: &str,
        tool: &str,
        program_result: bool,
    ) -> Result<lingxi_core::host::McpToolResultDto, Utf16JsonProjection> {
        if result.is_error {
            return Err(tool_mcp::native_mcp_error_detail(
                &result.content,
                result.error.as_ref(),
            ));
        }
        if program_result {
            let full = Utf16JsonProjection::array(vec![
                result.content.clone(),
                result
                    .structured_content
                    .clone()
                    .unwrap_or_else(|| Utf16JsonProjection::parse("null").unwrap()),
                result
                    .meta
                    .clone()
                    .unwrap_or_else(|| Utf16JsonProjection::parse("null").unwrap()),
            ])
            .unwrap();
            if full
                .to_json_string()
                .map_err(|error| error.to_string())?
                .encode_utf16()
                .count()
                > 10_485_760
            {
                return Err("The tool ran, but its result is over the 10485760-character limit of an mcp_call sent with result_not_for_model, so this mcp_call got no result. Check whether the call took effect before retrying mcp_call, and ask the tool for a smaller result if you retry.".into());
            }
            return Ok(result);
        }
        let session = self.session.lock().await;
        let model = session.model.clone();
        let profile = session.model_profile.clone();
        let id = session.session_id.as_uuid().to_string();
        drop(session);
        let home = self
            .config_home
            .clone()
            .unwrap_or_else(|| self.current_cwd());
        let directory = crate::tool_result_persistence::tool_results_dir(
            &home,
            &self.current_cwd().to_string_lossy(),
            &id,
        );
        let seed = || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            (now, uuid::Uuid::new_v4().simple().to_string())
        };
        let persist = tool_mcp::transform_result::PersistContext {
            output_dir: &directory,
            seed_factory: &seed,
        };
        let content = if let Some(structured) = &result.structured_content {
            let json = structured
                .to_json_string()
                .map_err(|error| error.to_string())?;
            let non_text = (0..result.content.array_len("").unwrap_or(0))
                .filter_map(|index| result.content.subprojection(&format!("/{index}")).ok())
                .filter(|block| block.string_at("/type") != Some("text"))
                .collect::<Vec<_>>();
            if non_text.is_empty() {
                Utf16JsonProjection::from(json)
            } else {
                let transformed = tool_mcp::transform_result::transform_result_content_source(
                    &Utf16JsonProjection::array(non_text).unwrap(),
                    server,
                    persist,
                );
                let mut content = (0..transformed.array_len("").unwrap_or(0))
                    .filter_map(|index| transformed.subprojection(&format!("/{index}")).ok())
                    .collect::<Vec<_>>();
                content.push(source_object(vec![
                    ("type".into(), "text".into()),
                    ("text".into(), json.into()),
                ])?);
                Utf16JsonProjection::array(content).map_err(source_error)?
            }
        } else {
            tool_mcp::transform_result::transform_result_content_source(
                &result.content,
                server,
                persist,
            )
        };
        let count = if tool_mcp::large_output::mcp_content_needs_exact_count_source(&content) {
            if let Some(blocks) = crate::provider_adapter::mcp_content_text_blocks(&content) {
                let message = ConversationMessage::User {
                    id: MessageId::new(),
                    content: blocks,
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    turn_companion: None,
                    usage_limit_note: None,
                    origin: None,
                    turn_origin: None,
                    api_message_override: None,
                    interrupted_by_shutdown: None,
                    refused_message_id: None,
                };
                match self
                    .api
                    .count_tokens_exact(&model, profile.as_deref(), None, vec![message], Vec::new())
                    .await
                {
                    Ok(Some(count)) => tool_mcp::ExactCountOutcome::Counted(count),
                    Ok(None) => tool_mcp::ExactCountOutcome::Unsupported,
                    Err(_) => tool_mcp::ExactCountOutcome::CountFailed,
                }
            } else {
                tool_mcp::ExactCountOutcome::Unsupported
            }
        } else {
            tool_mcp::ExactCountOutcome::Unsupported
        };
        result.content = tool_mcp::process_mcp_result_with_exact_count_source(
            &content,
            server,
            tool,
            &directory,
            seed().0,
            count,
        );
        Ok(result)
    }

    /// Resolve `session.mcp.connect({server})` only against this plugin's
    /// already-registered dynamic MCP configuration. The storage identity is
    /// host-minted by the successful Mod load handshake; no worker field can
    /// choose another plugin's config.
    /// Resolve the top-level Mods MCP operation through the registry's
    /// host-registered plugin owner, never a worker-supplied identity.
    pub(crate) async fn mod_mcp_connect(
        &self,
        plugin_storage_id: &str,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
        let server_units = input
            .string_units("/server")
            .filter(|units| !units.is_empty())
            .ok_or_else(|| {
                hooks::mods::ModError::Hook("takes { server } (a non-empty string)".into())
            })?;
        let Ok(server_key) = String::from_utf16(&server_units) else {
            return mcp_unlisted_result(&server_units);
        };
        let Some(registry) = self.mcp_registry.as_ref() else {
            return mcp_unlisted_result(&server_units);
        };

        let Some(config) = registry
            .plugin_server_for_owner(plugin_storage_id, &server_key)
            .await
        else {
            return mcp_unlisted_result(&server_units);
        };
        if let Some(config_error) = config.config_error.as_deref() {
            let message = format!(
                "The MCP server \"{}\" cannot be resolved: {config_error}",
                config.name
            );
            return mcp_connect_result(false, None, Some("failed"), Some(&message));
        }
        if config.disabled {
            let message = format!(
                "The MCP server \"{}\" is turned off; turn it on in /mcp.",
                config.name
            );
            return mcp_connect_result(false, None, Some("disabled"), Some(&message));
        }

        match registry
            .connect_plugin_server(plugin_storage_id, &server_key)
            .await
        {
            Ok(Some(mcp::PluginMcpConnectOutcome::Connected { server, .. })) => {
                mcp_connect_result(true, Some(&server), None, None)
            }
            Ok(Some(mcp::PluginMcpConnectOutcome::Refused(refusal))) => {
                mcp_connect_result_with_message(
                    false,
                    None,
                    Some(&refusal.reason),
                    Some(refusal.message),
                )
            }
            Ok(None) => {
                let message = format!(
                    "The MCP server \"{}\" was taken out: its plugin unloaded while it connected.",
                    config.name
                );
                mcp_connect_result(false, None, Some("failed"), Some(&message))
            }
            Err(error) => {
                let action_state = registry
                    .action_states()
                    .await
                    .into_iter()
                    .find(|(name, _)| name == &config.name)
                    .map(|(_, state)| state);
                if matches!(
                    &error,
                    McpError::HttpResponse {
                        status: 401 | 403,
                        ..
                    } | McpError::OAuth(_)
                ) || action_state == Some(McpActionState::NeedsAuth)
                {
                    let message = format!(
                        "The MCP server \"{}\" needs you to sign in; run /mcp to authenticate.",
                        config.name
                    );
                    mcp_connect_result(false, None, Some("auth"), Some(&message))
                } else {
                    let message = mcp_connect_failed_message(&config.name, &error);
                    mcp_connect_result_with_message(false, None, Some("failed"), Some(message))
                }
            }
        }
    }

    /// Apply restrictions before the initialize reply, independently of I/O.
    pub async fn apply_sdk_mcp_initialize_policies(&self, configs: &[mcp::McpServerConfig]) {
        if let Some(registry) = &self.mcp_registry {
            mcp::tool_policy::apply_sdk_initialize_policies(registry, configs, self.perms.as_ref())
                .await;
        }
    }

    /// Allocate SDK-init private handles against the entire live catalog.
    pub async fn finalize_sdk_mcp_initialize_names(&self, configs: &mut [mcp::McpServerConfig]) {
        if let Some(registry) = &self.mcp_registry {
            registry.finalize_control_server_names(configs).await;
        }
        for config in configs {
            if let McpTransportSpec::SdkControl { control_channel_id } = &mut config.spec {
                *control_channel_id = config.name.clone();
            }
        }
    }

    /// Register SDK-hosted servers through independent connection owners.
    /// The registry publishes their ordinary catalog and connection events.
    pub async fn connect_sdk_mcp_servers(&self, configs: Vec<mcp::McpServerConfig>) {
        let Some(registry) = &self.mcp_registry else {
            return;
        };
        self.apply_sdk_mcp_initialize_policies(&configs).await;
        futures::future::join_all(configs.into_iter().map(|config| async move {
            // Each server starts without waiting for an earlier host reply.
            // The registry owns publication and records any failed handshake.
            let _ = registry.register_sdk_initialize(config).await;
        }))
        .await;
        mcp::tool_policy::synchronize_permission_policies(registry, self.perms.as_ref()).await;
    }

    /// SDK status uses the same current registry catalog, including tools
    /// withheld from model admission and bounded MCP Apps metadata.
    pub async fn sdk_mcp_status(&self) -> Result<Utf16JsonProjection, String> {
        let Some(registry) = &self.mcp_registry else {
            return source_object(vec![(
                "mcpServers".into(),
                Utf16JsonProjection::array(Vec::new()).map_err(source_error)?,
            )]);
        };
        let order = registry.control_server_names().await;
        let snapshot = registry.connections.read().await.clone();
        let mut rows = order
            .into_iter()
            .filter_map(|key| snapshot.get(&key).cloned().map(|state| (key, state)))
            .collect::<Vec<_>>();
        rows.sort_by_key(|(_, state)| {
            matches!(state.config().spec, McpTransportSpec::SdkControl { .. })
        });
        let mut seen = std::collections::HashSet::new();
        let mut servers = Vec::new();
        for (name, state) in rows {
            let config = state.config();
            if name != config.name {
                continue;
            }
            if !seen.insert(config.authored_name_units()) {
                continue;
            }
            let status = if config.disabled {
                "disabled"
            } else {
                match &state {
                    mcp::McpConnectionState::Connected { .. }
                    | mcp::McpConnectionState::Cached { .. } => "connected",
                    mcp::McpConnectionState::Connecting { .. }
                    | mcp::McpConnectionState::Reconnecting { .. } => "pending",
                    mcp::McpConnectionState::AwaitingOAuth { .. }
                    | mcp::McpConnectionState::NeedsAuth { .. } => "needs-auth",
                    mcp::McpConnectionState::Failed { .. } => "failed",
                    _ => "disconnected",
                }
            };
            let mut server = source_object(vec![
                (
                    "name".into(),
                    source_text_units(config.authored_name_units()),
                ),
                ("status".into(), source_text(status)),
            ])?;
            if matches!(
                state,
                mcp::McpConnectionState::Connected { .. } | mcp::McpConnectionState::Cached { .. }
            ) {
                if let Some(info) = registry
                    .server_metadata(&name)
                    .await
                    .and_then(|metadata| metadata.server_info)
                {
                    server.set_field("serverInfo", info).map_err(source_error)?;
                }
            }
            if let mcp::McpConnectionState::Failed { error, .. } = &state {
                server
                    .set_field("error", source_text(error))
                    .map_err(source_error)?;
            }
            if let Some(public_config) = mcp_control_status_config(config)? {
                server
                    .set_field("config", public_config)
                    .map_err(source_error)?;
            }
            let transport = config.metadata.transport.as_deref().unwrap_or_else(|| {
                if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
                    "sdk"
                } else {
                    config.spec.kind()
                }
            });
            server
                .set_field("transport", source_text(transport))
                .map_err(source_error)?;
            let scope = match config.scope {
                mcp::ConfigScope::Dynamic => "dynamic",
                mcp::ConfigScope::Agent => "agent",
                mcp::ConfigScope::ClaudeAi => "claudeai",
                mcp::ConfigScope::Enterprise => "enterprise",
                mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Local) => "local",
                mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User) => "user",
                mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Project) => "project",
                mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed) => "managed",
            };
            server
                .set_field("scope", source_text(scope))
                .map_err(source_error)?;
            server
                .set_field(
                    "source",
                    source_text(&mcp::McpServerProvenance::from_config(config).source),
                )
                .map_err(source_error)?;
            if matches!(
                state,
                mcp::McpConnectionState::Connected { .. } | mcp::McpConnectionState::Cached { .. }
            ) {
                let definitions = registry
                    .control_status_tools(config)
                    .await
                    .iter()
                    .map(mcp_tool_status_definition)
                    .collect::<Result<Vec<_>, _>>()?;
                server
                    .set_field(
                        "tools",
                        Utf16JsonProjection::array(definitions).map_err(source_error)?,
                    )
                    .map_err(source_error)?;
            }
            servers.push(server);
        }
        source_object(vec![(
            "mcpServers".into(),
            Utf16JsonProjection::array(servers).map_err(source_error)?,
        )])
    }

    pub async fn sdk_mcp_message_is_connected(&self, units: &[u16]) -> bool {
        match &self.mcp_registry {
            Some(registry) => registry.sdk_control_is_connected(units).await,
            None => false,
        }
    }

    /// Native SDK MCP UI resource read. This shares the live registry and
    /// returns the current client DTO's first text content block.
    pub async fn read_sdk_mcp_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<Utf16JsonProjection, String> {
        let registry = self
            .mcp_registry
            .as_ref()
            .ok_or_else(|| format!("Server not found: {server}"))?;
        if !registry
            .server_names()
            .await
            .iter()
            .any(|name| name == server)
        {
            return Err(format!("Server not found: {server}"));
        }
        let config = registry
            .get_config(server)
            .await
            .ok_or_else(|| format!("Server not found: {server}"))?;
        let state = registry
            .action_states()
            .await
            .into_iter()
            .find(|(name, _)| name == server)
            .map(|(_, state)| state);
        if config.disabled || state == Some(McpActionState::Disabled) {
            return Err(format!(
                "MCP server {server} is disabled — enable it (mcp_toggle) before reading"
            ));
        }
        if state == Some(McpActionState::NeedsApproval) {
            return Err(format!("MCP server {server} is not approved for this project — approve it in /mcp before reading"));
        }
        if state != Some(McpActionState::Connected) {
            let status = match state {
                Some(McpActionState::Pending) => "pending",
                Some(McpActionState::NeedsAuth) => "needs-auth",
                Some(McpActionState::Failed) => "failed",
                _ => "disconnected",
            };
            return Err(format!(
                "MCP server {server} is not connected (status: {status})"
            ));
        }
        if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
            return Err("mcp_read_resource does not support SDK MCP servers: the caller hosts them and can read them directly".into());
        }
        let client = registry
            .ensure_connected_client(server)
            .await
            .map_err(|error| format!("mcp_read_resource: {error}"))?;
        let uri_source = source_text(uri);
        let raw = client
            .read_resource(&uri_source)
            .await
            .map_err(|error| format!("mcp_read_resource: {error}"))?;
        let mut item = source_object(vec![("uri".into(), raw.uri), ("text".into(), raw.content)])?;
        if let Some(mime_type) = raw.mime_type {
            item.set_field("mimeType", source_text(&mime_type))
                .map_err(source_error)?;
        }
        if let Some(meta) = raw.meta {
            if let Some(meta) = native_resource_meta_projected(meta)? {
                item.set_field("_meta", meta).map_err(source_error)?;
            }
        }
        let contents = Utf16JsonProjection::array(vec![item]).map_err(source_error)?;
        let result = source_object(vec![("contents".into(), contents)])?;
        if result
            .to_json_string()
            .map_err(source_error)?
            .encode_utf16()
            .count()
            > 10_485_760
        {
            return Err(
                "mcp_read_resource: the resource exceeds the 10485760-character limit".into(),
            );
        }
        Ok(result)
    }

    /// Attach through the same roster before raising the Mod session event.
    pub fn prepare_sdk_ui_surface_attach(
        &self,
        client_id: &str,
        surface: crate::config::ModRenderSurface,
        viewport: Option<Value>,
        answers: Option<Vec<lingxi_core::host::ModRemoteUiAnswer>>,
    ) -> (Vec<String>, Option<Value>) {
        if let Some(host) = self.remote_ui_host() {
            host.attach(client_id, surface.as_str(), answers);
        }
        let event = if self
            .mod_surface_roster
            .attach(client_id.to_owned(), surface)
            .unwrap_or(false)
        {
            let mut event = json!({"surface":surface.as_str(),"clientId":client_id});
            if let Some(viewport) = viewport {
                event["viewport"] = viewport;
            }
            Some(event)
        } else {
            None
        };
        (hooks::mods::ModSessionContext::surfaces(self), event)
    }

    pub fn prepare_sdk_ui_surface_detach(
        &self,
        client: &str,
    ) -> (bool, Vec<String>, Option<Value>) {
        let detached = self.mod_surface_roster.detach(client);
        self.detach_sdk_ui_client(client);
        let event = detached.as_ref().map(|attachment| {
            json!({"surface":attachment.surface.as_str(),"clientId":client,"reason":"detach"})
        });
        (
            detached.is_some(),
            hooks::mods::ModSessionContext::surfaces(self),
            event,
        )
    }

    /// ui_render implicitly joins its renderer on the same session roster.
    pub fn prepare_sdk_ui_render_attach(&self, request: &Utf16JsonProjection) -> Option<Value> {
        let surface = match request.string_at("/surface")? {
            "desktop" => crate::config::ModRenderSurface::Desktop,
            "mobile" => crate::config::ModRenderSurface::Mobile,
            "vscode" => crate::config::ModRenderSurface::Vscode,
            _ => return None,
        };
        if let Some(client) = request.string_at("/client_id") {
            return self
                .prepare_sdk_ui_surface_attach(
                    client,
                    surface,
                    request
                        .subprojection("/viewport")
                        .ok()
                        .and_then(projection_to_value),
                    None,
                )
                .1;
        }
        let client = format!("{}:default", surface.as_str());
        if let Some(host) = self.remote_ui_host() {
            host.attach(&client, surface.as_str(), None);
        }
        if !self
            .mod_surface_roster
            .attach_default(surface)
            .unwrap_or(false)
        {
            return None;
        }
        let mut event = json!({"surface":surface.as_str(),"clientId":client});
        if let Some(viewport) = request
            .subprojection("/viewport")
            .ok()
            .and_then(projection_to_value)
        {
            event["viewport"] = viewport;
        }
        Some(event)
    }

    pub async fn dispatch_sdk_ui_surface_event(&self, name: &str, event: Option<Value>) {
        if let Some(event) = event {
            self.dispatch_mod_surface_event(name, event).await;
        }
    }
}

fn mcp_control_status_config(
    config: &mcp::McpServerConfig,
) -> Result<Option<Utf16JsonProjection>, String> {
    match &config.spec {
        McpTransportSpec::Http { url, .. } | McpTransportSpec::Sse { url, .. } => {
            source_object(vec![
                ("type".into(), source_text(config.spec.kind())),
                ("url".into(), source_text(url)),
            ])
            .map(Some)
        }
        McpTransportSpec::Stdio { command, args, .. } => source_object(vec![
            ("type".into(), "stdio".into()),
            ("command".into(), source_text(command)),
            ("args".into(), source_value(json!(args))?),
        ])
        .map(Some),
        _ => Ok(None),
    }
}

fn mcp_tool_status_definition(
    tool: &lingxi_core::host::McpToolDto,
) -> Result<Utf16JsonProjection, String> {
    let mut fields = Vec::new();
    if let Some(annotations) = &tool.annotations {
        for (name, value) in [
            ("readOnly", annotations.read_only_hint),
            ("destructive", annotations.destructive_hint),
            ("openWorld", annotations.open_world_hint),
        ] {
            if value == Some(true) {
                fields.push((name.to_owned(), source_value(json!(true))?));
            }
        }
    }
    let mut definition = source_object(vec![
        ("name".into(), tool.wire_tool_name_source()),
        ("annotations".into(), source_object(fields)?),
    ])?;
    if let Some(meta) = &tool.meta {
        if let Some(meta) = native_tool_ui_meta(meta.clone())? {
            definition.set_field("_meta", meta).map_err(source_error)?;
        }
    }
    Ok(definition)
}

fn native_tool_ui_meta(meta: Utf16JsonProjection) -> Result<Option<Utf16JsonProjection>, String> {
    meta.validate().map_err(source_error)?;
    if meta.object_entries().is_none() {
        return Ok(None);
    }
    let mut result = source_object(Vec::new())?;
    if let Ok(ui) = meta.subprojection("/ui") {
        let valid = ui.object_entries().is_some()
            && (!ui.contains_property("resourceUri")
                || ui
                    .string_at("/resourceUri")
                    .is_some_and(|uri| uri.starts_with("ui://")))
            && (!ui.contains_property("visibility")
                || ui.at_pointer("/visibility").is_some_and(|visibility| {
                    visibility.array_len().is_some_and(|length| {
                        (0..length).all(|index| {
                            visibility
                                .get_index(index)
                                .and_then(|entry| entry.as_str())
                                .is_some_and(|entry| matches!(entry, "model" | "app"))
                        })
                    })
                }));
        if valid
            && ui
                .to_json_string()
                .map_err(source_error)?
                .encode_utf16()
                .count()
                <= 4096
        {
            result.set_field("ui", ui).map_err(source_error)?;
        }
    }
    if let Ok(uri) = meta.subprojection("/ui~1resourceUri") {
        if uri
            .string_at("")
            .is_some_and(|uri| uri.starts_with("ui://"))
            && uri
                .string_units("")
                .is_some_and(|units| units.len() <= 4096)
        {
            result
                .set_field("ui/resourceUri", uri)
                .map_err(source_error)?;
        }
    }
    Ok((!result
        .object_entries()
        .is_some_and(|entries| entries.is_empty()))
    .then_some(result))
}

fn native_resource_meta_projected(
    meta: Utf16JsonProjection,
) -> Result<Option<Utf16JsonProjection>, String> {
    meta.validate().map_err(source_error)?;
    let Some(entries) = meta.object_entries() else {
        return Ok(None);
    };
    let prefix = "com.anthropic/".encode_utf16().collect::<Vec<_>>();
    let mut fields = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.code_units.starts_with(&prefix) {
            continue;
        }
        fields.push((
            entry.code_units.to_vec(),
            meta.subprojection_property_units(entry.code_units)
                .map_err(source_error)?,
        ));
    }
    if fields.is_empty() {
        Ok(None)
    } else {
        Utf16JsonProjection::object_units(fields)
            .map(Some)
            .map_err(source_error)
    }
}

fn mcp_connect_result(
    is_connected: bool,
    server: Option<&str>,
    reason: Option<&str>,
    message: Option<&str>,
) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    mcp_connect_result_with_message(is_connected, server, reason, message.map(source_text))
}

fn mcp_connect_result_with_message(
    is_connected: bool,
    server: Option<&str>,
    reason: Option<&str>,
    message: Option<Utf16JsonProjection>,
) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    let mut fields = vec![(
        "isConnected".to_owned(),
        source_value(json!(is_connected)).map_err(hooks::mods::ModError::Protocol)?,
    )];
    if let Some(server) = server {
        fields.push(("server".to_owned(), source_text(server)));
    }
    if let Some(reason) = reason {
        fields.push(("reason".to_owned(), source_text(reason)));
    }
    if let Some(message) = message {
        fields.push(("message".to_owned(), message));
    }
    source_object(fields).map_err(hooks::mods::ModError::Protocol)
}

fn mcp_unlisted_result(server_units: &[u16]) -> Result<Utf16JsonProjection, hooks::mods::ModError> {
    let mut message_units = "This plugin's manifest lists no MCP server named \""
        .encode_utf16()
        .collect::<Vec<_>>();
    message_units.extend_from_slice(server_units);
    message_units.extend("\"".encode_utf16());
    message_units.extend(".".encode_utf16());
    let message = source_text_units(message_units);
    mcp_connect_result_with_message(false, None, Some("unlisted"), Some(message))
}

fn mcp_connect_failed_message(server_name: &str, error: &McpError) -> Utf16JsonProjection {
    let mut message_units = format!("The MCP server \"{server_name}\" did not connect: ")
        .encode_utf16()
        .collect::<Vec<_>>();
    let detail_units = error
        .source_diagnostic()
        .and_then(|detail| detail.string_units("").map(|units| units.to_vec()))
        .unwrap_or_else(|| error.to_string().encode_utf16().collect());
    message_units.extend(detail_units);
    message_units.push(u16::from(b'.'));
    source_text_units(message_units)
}

fn source_text_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("the display form is the UTF-16 lossy decoding of its source units")
}

fn source_object(
    fields: Vec<(String, Utf16JsonProjection)>,
) -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::object(fields).map_err(source_error)
}

fn source_value(value: Value) -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::from_serde_value(&value).map_err(source_error)
}

fn source_text(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust strings are valid UTF-16 projections")
}

fn source_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn projection_to_value(source: Utf16JsonProjection) -> Option<Value> {
    let encoded = source.to_json_string().ok()?;
    serde_json::from_str(&encoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_322_direct_control_fqn_parser_preserves_authored_units() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../mcp/tests/fixtures/mcp_sdk_control_names_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let units = case["inputUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
                .collect::<Vec<_>>();
            let source =
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).unwrap();
            let parsed = parse_control_mcp_tool_name(&source);
            if case["parsed"].is_null() {
                assert!(parsed.is_none(), "{}", case["name"]);
                continue;
            }
            let parsed = parsed.unwrap();
            let expected = |key: &str| {
                case["parsed"][key].as_array().map(|units| {
                    units
                        .iter()
                        .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
                        .collect::<Vec<_>>()
                })
            };
            assert_eq!(
                parsed.server_name_units,
                expected("serverUnits").unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                parsed
                    .tool_name_source
                    .and_then(|source| source.string_units("").map(|units| units.to_vec())),
                expected("toolUnits"),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn mcp_connect_result_uses_native_success_and_refusal_fields() {
        let success = mcp_connect_result(true, Some("plugin:demo:jira"), None, None).unwrap();
        assert_eq!(
            success.to_json_string().unwrap(),
            r#"{"isConnected":true,"server":"plugin:demo:jira"}"#
        );

        let refusal = mcp_connect_result(
            false,
            None,
            Some("unlisted"),
            Some("This plugin's manifest lists no MCP server named \"jira\"."),
        )
        .unwrap();
        assert_eq!(
            refusal.to_json_string().unwrap(),
            r#"{"isConnected":false,"reason":"unlisted","message":"This plugin's manifest lists no MCP server named \"jira\"."}"#
        );
    }

    #[test]
    fn mcp_connect_unlisted_message_preserves_raw_server_utf16() {
        let server_units = [0xd800];
        let result = mcp_unlisted_result(&server_units).unwrap();
        assert_eq!(
            result.to_json_string().unwrap(),
            r#"{"isConnected":false,"reason":"unlisted","message":"This plugin's manifest lists no MCP server named \"\ud800\"."}"#
        );
        let expected_units = "This plugin's manifest lists no MCP server named \""
            .encode_utf16()
            .chain(server_units)
            .chain("\".".encode_utf16())
            .collect::<Vec<_>>();
        assert_eq!(result.string_units("/message"), Some(expected_units));
    }

    #[test]
    fn tool_ui_meta_matches_native_filter_and_preserves_unknown_ui_units() {
        let meta = Utf16JsonProjection::parse(r#"{"ui":{"resourceUri":"ui://fixture/view","visibility":["model","app"],"custom":"x","\ud800":"\udfff"},"ui/resourceUri":"ui://fixture/legacy","private":true}"#).unwrap();
        let filtered = native_tool_ui_meta(meta).unwrap().unwrap();
        assert_eq!(
            filtered.to_json_string().unwrap(),
            r#"{"ui":{"resourceUri":"ui://fixture/view","visibility":["model","app"],"custom":"x","\ud800":"\udfff"},"ui/resourceUri":"ui://fixture/legacy"}"#
        );
        let bad = Utf16JsonProjection::from_serde_value(&json!({
            "ui":{"resourceUri":"https://fixture/view","visibility":["model"]},
            "ui/resourceUri":"ui://fixture/valid"
        }))
        .unwrap();
        assert_eq!(
            native_tool_ui_meta(bad)
                .unwrap()
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"ui/resourceUri":"ui://fixture/valid"}"#
        );
        let unsupported = Utf16JsonProjection::from_serde_value(&json!({
            "ui":{"visibility":["unknown"]},"private":true
        }))
        .unwrap();
        assert!(native_tool_ui_meta(unsupported).unwrap().is_none());
    }

    #[test]
    fn sdk_resource_omits_private_metadata_and_empty_object() {
        let meta =
            Utf16JsonProjection::parse(r#"{"com.anthropic/private":1,"example/key":"\ud800"}"#)
                .unwrap();
        let filtered = native_resource_meta_projected(meta).unwrap().unwrap();
        assert_eq!(
            filtered.to_json_string().unwrap(),
            r#"{"example/key":"\ud800"}"#
        );
        let private_only = Utf16JsonProjection::from_serde_value(&json!({
            "com.anthropic/private": 1
        }))
        .unwrap();
        assert!(native_resource_meta_projected(private_only)
            .unwrap()
            .is_none());
    }
}
