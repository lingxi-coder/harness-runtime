//! SDK controls over the session's existing MCP registry and UI roster.
use super::*;
use lingxi_core::host::{McpActionState, McpTransportSpec};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

impl ConversationOrchestrator {
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
            return Ok(Utf16JsonProjection::plain(json!({"mcpServers":[]})));
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
            let mut server = Utf16JsonProjection::plain(json!({"name":name,"status":status}));
            if let Some(info) = registry
                .server_metadata(&name)
                .await
                .and_then(|metadata| metadata.server_info)
            {
                server.value["serverInfo"] = info;
            }
            if let mcp::McpConnectionState::Failed { error, .. } = &state {
                server.value["error"] = json!(error);
            }
            if matches!(state.config().spec, McpTransportSpec::SdkControl { .. }) {
                server.value["scope"] = json!("dynamic");
                server.value["source"] = json!("sdk");
            }
            if let mcp::McpConnectionState::Connected { tools, .. }
            | mcp::McpConnectionState::Cached { tools, .. } = &state
            {
                let mut definitions = Vec::with_capacity(tools.len());
                for tool in tools {
                    let mut annotations = json!({});
                    if let Some(hints) = &tool.annotations {
                        for (key, enabled) in [
                            ("readOnly", hints.read_only_hint),
                            ("destructive", hints.destructive_hint),
                            ("openWorld", hints.open_world_hint),
                        ] {
                            if enabled == Some(true) {
                                annotations[key] = json!(true);
                            }
                        }
                    }
                    let mut definition = Utf16JsonProjection::plain(
                        json!({"name":tool.tool_name,"annotations":annotations}),
                    );
                    if let Some(source) = &tool.definition_projection {
                        source.validate().map_err(|error| error.to_string())?;
                        if let Ok(name) = source.subprojection("/name") {
                            if name.value == json!(tool.tool_name) {
                                definition
                                    .set_field("name", name)
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    let meta = match (&tool.definition_projection, &tool.meta) {
                        (_, None) => None,
                        (Some(source), Some(display)) => {
                            let projection = source
                                .subprojection("/_meta")
                                .map_err(|error| error.to_string())?;
                            if &projection.value != display {
                                return Err(
                                    "MCP tool metadata does not match its exact source".into()
                                );
                            }
                            Some(projection)
                        }
                        (None, Some(display)) => Some(Utf16JsonProjection::plain(display.clone())),
                    };
                    if let Some(meta) = meta {
                        if let Some(meta) = native_tool_ui_meta(meta)? {
                            definition
                                .set_field("_meta", meta)
                                .map_err(|error| error.to_string())?;
                        }
                    }
                    definitions.push(definition);
                }
                server
                    .set_field(
                        "tools",
                        Utf16JsonProjection::array(definitions)
                            .map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
            }
            servers.push(server);
        }
        let mut response = Utf16JsonProjection::plain(json!({}));
        response
            .set_field(
                "mcpServers",
                Utf16JsonProjection::array(servers).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
        Ok(response)
    }

    /// Native SDK MCP UI resource read. This shares the live registry and
    /// returns raw resource content instead of the model tool's saved blobs.
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
        let raw = client
            .read_resource_native(uri)
            .await
            .map_err(|error| format!("mcp_read_resource: {error}"))?;
        let contents = raw
            .value
            .get("contents")
            .and_then(Value::as_array)
            .ok_or_else(|| "mcp_read_resource: resources/read returned no contents".to_owned())?;
        let contents: Vec<_> = contents
            .iter()
            .enumerate()
            .map(|(index, content)| {
                let source = raw
                    .subprojection(&format!("/contents/{index}"))
                    .map_err(|error| error.to_string())?;
                let mut item = Utf16JsonProjection::plain(json!({}));
                for key in ["uri", "mimeType", "text", "blob", "_meta"] {
                    if let Some(value) = content.get(key) {
                        if key == "_meta" {
                            if let Some(meta) = native_resource_meta_projected(
                                source
                                    .subprojection("/_meta")
                                    .map_err(|error| error.to_string())?,
                            )
                            .map_err(|error| error.to_string())?
                            {
                                item.set_field(key, meta)
                                    .map_err(|error| error.to_string())?;
                            }
                        } else {
                            item.set_field(
                                key,
                                source
                                    .subprojection(&format!("/{key}"))
                                    .map_err(|error| error.to_string())?,
                            )
                            .map_err(|error| error.to_string())?;
                        }
                    }
                }
                Ok(item)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let contents = Utf16JsonProjection::array(contents).map_err(|error| error.to_string())?;
        if contents
            .to_json_string()
            .map_err(|error| error.to_string())?
            .encode_utf16()
            .count()
            > 10_485_760
        {
            return Err(
                "mcp_read_resource: the resource exceeds the 10485760-character limit".into(),
            );
        }
        let mut result = Utf16JsonProjection::plain(json!({}));
        result
            .set_field("contents", contents)
            .map_err(|error| error.to_string())?;
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
        let event=detached.as_ref().map(|attachment| json!({"surface":attachment.surface.as_str(),"clientId":client,"reason":"detach"}));
        (
            detached.is_some(),
            hooks::mods::ModSessionContext::surfaces(self),
            event,
        )
    }

    /// ui_render implicitly joins its renderer on the same session roster.
    pub fn prepare_sdk_ui_render_attach(&self, request: &Utf16JsonProjection) -> Option<Value> {
        let surface = match request.value.get("surface").and_then(Value::as_str)? {
            "desktop" => crate::config::ModRenderSurface::Desktop,
            "mobile" => crate::config::ModRenderSurface::Mobile,
            "vscode" => crate::config::ModRenderSurface::Vscode,
            _ => return None,
        };
        if let Some(client) = request.value.get("client_id").and_then(Value::as_str) {
            return self
                .prepare_sdk_ui_surface_attach(
                    client,
                    surface,
                    request.value.get("viewport").cloned(),
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
        if let Some(viewport) = request.value.get("viewport") {
            event["viewport"] = viewport.clone();
        }
        Some(event)
    }

    pub async fn dispatch_sdk_ui_surface_event(&self, name: &str, event: Option<Value>) {
        if let Some(event) = event {
            self.dispatch_mod_surface_event(name, event).await;
        }
    }
}

fn native_tool_ui_meta(meta: Utf16JsonProjection) -> Result<Option<Utf16JsonProjection>, String> {
    meta.validate().map_err(|error| error.to_string())?;
    if !meta.value.is_object() {
        return Ok(None);
    }
    let mut result = Utf16JsonProjection::plain(json!({}));
    if let Ok(ui) = meta.subprojection("/ui") {
        let valid = ui.value.is_object()
            && ui
                .value
                .get("resourceUri")
                .is_none_or(|uri| uri.as_str().is_some_and(|uri| uri.starts_with("ui://")))
            && ui.value.get("visibility").is_none_or(|visibility| {
                visibility.as_array().is_some_and(|entries| {
                    entries
                        .iter()
                        .all(|entry| matches!(entry.as_str(), Some("model" | "app")))
                })
            });
        if valid
            && ui
                .to_json_string()
                .map_err(|error| error.to_string())?
                .encode_utf16()
                .count()
                <= 4096
        {
            result
                .set_field("ui", ui)
                .map_err(|error| error.to_string())?;
        }
    }
    if let Ok(uri) = meta.subprojection("/ui~1resourceUri") {
        if uri
            .value
            .as_str()
            .is_some_and(|uri| uri.starts_with("ui://"))
            && uri
                .string_units("")
                .is_some_and(|units| units.len() <= 4096)
        {
            result
                .set_field("ui/resourceUri", uri)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok((!result.value.as_object().expect("object").is_empty()).then_some(result))
}

fn native_resource_meta(meta: &Value) -> Option<Value> {
    let mut meta = meta.as_object()?.clone();
    meta.retain(|key, _| !key.starts_with("com.anthropic/"));
    (!meta.is_empty()).then_some(Value::Object(meta))
}

fn native_resource_meta_projected(
    mut meta: Utf16JsonProjection,
) -> Result<Option<Utf16JsonProjection>, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    let Some(object) = meta.value.as_object() else {
        return Ok(None);
    };
    let prefix = "com.anthropic/".encode_utf16().collect::<Vec<_>>();
    let mut value = object.clone();
    value.retain(|key, _| !meta.key_units("", key).starts_with(&prefix));
    if value.is_empty() {
        return Ok(None);
    }
    meta.rebase_display_value(Value::Object(value))?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_ui_meta_matches_native_293_filter_and_preserves_unknown_ui_units() {
        let meta = Utf16JsonProjection::parse(r#"{"ui":{"resourceUri":"ui://fixture/view","visibility":["model","app"],"custom":"x","\ud800":"\udfff"},"ui/resourceUri":"ui://fixture/legacy","private":true}"#).unwrap();
        let filtered = native_tool_ui_meta(meta).unwrap().unwrap();
        assert_eq!(
            filtered.to_json_string().unwrap(),
            r#"{"ui":{"resourceUri":"ui://fixture/view","visibility":["model","app"],"custom":"x","\ud800":"\udfff"},"ui/resourceUri":"ui://fixture/legacy"}"#
        );
        let bad = Utf16JsonProjection::plain(
            json!({"ui":{"resourceUri":"https://fixture/view","visibility":["model"]},"ui/resourceUri":"ui://fixture/valid"}),
        );
        assert_eq!(
            native_tool_ui_meta(bad).unwrap().unwrap().value,
            json!({"ui/resourceUri":"ui://fixture/valid"})
        );
        assert!(native_tool_ui_meta(Utf16JsonProjection::plain(
            json!({"ui":{"visibility":["unknown"]},"private":true})
        ))
        .unwrap()
        .is_none());
        assert!(native_tool_ui_meta(Utf16JsonProjection::plain(
            json!({"ui":{"resourceUri":format!("ui://{}","x".repeat(4096))}})
        ))
        .unwrap()
        .is_none());
    }

    #[test]
    fn sdk_resource_omits_private_metadata_and_empty_object() {
        assert_eq!(
            native_resource_meta(&json!({"com.anthropic/private":1})),
            None
        );
        assert_eq!(
            native_resource_meta(&json!({"com.anthropic/private":1,"example/key":{"x":1}})),
            Some(json!({"example/key":{"x":1}}))
        );
    }
}
