//! SDK controls over the session's existing MCP registry and UI roster.
use super::*;
use lingxi_core::host::{McpActionState, McpError, McpToolAnnotationsDto, McpTransportSpec};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

impl ConversationOrchestrator {
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
            Ok(Some((public_name, _connection_id))) => {
                mcp_connect_result(true, Some(&public_name), None, None)
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

    /// Register SDK-hosted servers after the initialize reply has been queued.
    /// The registry publishes their ordinary catalog and connection events.
    pub async fn connect_sdk_mcp_servers(&self, configs: Vec<mcp::McpServerConfig>) {
        let Some(registry) = &self.mcp_registry else {
            return;
        };
        for config in configs {
            if registry.get_config(&config.name).await.is_some() {
                continue;
            }
            // A failure is recorded by the same registry as every platform
            // connection. It cannot retroactively alter the initialize reply.
            let _ = registry.connect(config).await;
        }
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
        let mut servers = Vec::new();
        for name in registry.server_names().await {
            let state = registry.connections.read().await.get(&name).cloned();
            let Some(state) = state else {
                continue;
            };
            let status = match &state {
                mcp::McpConnectionState::Connected { .. }
                | mcp::McpConnectionState::Cached { .. } => "connected",
                mcp::McpConnectionState::Connecting { .. }
                | mcp::McpConnectionState::Reconnecting { .. } => "pending",
                mcp::McpConnectionState::AwaitingOAuth { .. }
                | mcp::McpConnectionState::NeedsAuth { .. } => "needs-auth",
                mcp::McpConnectionState::Failed { .. } => "failed",
                _ => "disconnected",
            };
            let mut server = source_object(vec![
                ("name".into(), source_text(&name)),
                ("status".into(), source_text(status)),
            ])?;
            if let Some(info) = registry
                .server_metadata(&name)
                .await
                .and_then(|metadata| metadata.server_info)
            {
                server.set_field("serverInfo", info).map_err(source_error)?;
            }
            if let mcp::McpConnectionState::Failed { error, .. } = &state {
                server
                    .set_field("error", source_text(error))
                    .map_err(source_error)?;
            }
            if matches!(state.config().spec, McpTransportSpec::SdkControl { .. }) {
                server
                    .set_field("scope", source_text("dynamic"))
                    .map_err(source_error)?;
                server
                    .set_field("source", source_text("sdk"))
                    .map_err(source_error)?;
            }
            if let mcp::McpConnectionState::Connected { tools, .. }
            | mcp::McpConnectionState::Cached { tools, .. } = &state
            {
                let definitions = tools
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

fn mcp_tool_status_definition(
    tool: &lingxi_core::host::McpToolDto,
) -> Result<Utf16JsonProjection, String> {
    let annotations = match &tool.annotations {
        Some(annotations) => tool_annotations_projection(annotations)?,
        None => source_object(Vec::new())?,
    };
    let mut definition = source_object(vec![
        ("name".into(), source_text(&tool.tool_name)),
        ("description".into(), tool.description.clone()),
        ("inputSchema".into(), tool.input_schema.clone()),
        ("annotations".into(), annotations),
    ])?;
    if let Some(schema) = &tool.output_schema {
        definition
            .set_field("outputSchema", schema.clone())
            .map_err(source_error)?;
    }
    if let Some(meta) = &tool.meta {
        if let Some(meta) = native_tool_ui_meta(meta.clone())? {
            definition.set_field("_meta", meta).map_err(source_error)?;
        }
    }
    Ok(definition)
}

fn tool_annotations_projection(
    annotations: &McpToolAnnotationsDto,
) -> Result<Utf16JsonProjection, String> {
    let mut result = annotations.extra.clone();
    result.validate().map_err(source_error)?;
    if let Some(title) = &annotations.title {
        result
            .set_field("title", title.clone())
            .map_err(source_error)?;
    }
    for (field, value) in [
        ("readOnlyHint", annotations.read_only_hint),
        ("destructiveHint", annotations.destructive_hint),
        ("idempotentHint", annotations.idempotent_hint),
        ("openWorldHint", annotations.open_world_hint),
    ] {
        if let Some(value) = value {
            result
                .set_field(field, source_value(json!(value))?)
                .map_err(source_error)?;
        }
    }
    Ok(result)
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
