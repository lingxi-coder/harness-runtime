//! Mobile MCP transport composition.
//!
//! Local Apps and remote MCP servers have separate ownership boundaries.  The
//! composite only routes a connection id; it does not merge the Local Apps
//! registry into the remote transport or infer a route from a server name.

use async_trait::async_trait;
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError,
    McpNotificationStream, McpPromptDto, McpRawConnection, McpResourceContentDto, McpResourceDto,
    McpResourceTemplateDto, McpToolDto, McpToolResultDto, McpTransport, McpTransportKind,
    McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use platform_common::RemoteMcpTransport;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::mobile::local_apps_mcp::LocalAppsMcpTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    LocalApps,
    Remote,
}

/// Routes each live mobile MCP connection to its owning transport.
pub(crate) struct MobileMcpTransport {
    local_apps: Arc<LocalAppsMcpTransport>,
    remote: Arc<RemoteMcpTransport>,
    routes: Mutex<HashMap<McpConnectionId, Route>>,
}

impl MobileMcpTransport {
    pub(crate) fn new(
        local_apps: Arc<LocalAppsMcpTransport>,
        remote: Arc<RemoteMcpTransport>,
    ) -> Self {
        Self {
            local_apps,
            remote,
            routes: Mutex::new(HashMap::new()),
        }
    }

    fn route_for(&self, id: McpConnectionId) -> Result<Route, McpError> {
        self.routes
            .lock()
            .map_err(|_| McpError::Internal("mobile MCP route map poisoned".into()))?
            .get(&id)
            .copied()
            .ok_or_else(|| McpError::Connection(format!("no such connection: {id}")))
    }

    fn remember(&self, id: McpConnectionId, route: Route) -> Result<(), McpError> {
        self.routes
            .lock()
            .map_err(|_| McpError::Internal("mobile MCP route map poisoned".into()))?
            .insert(id, route);
        Ok(())
    }

    fn forget(&self, id: McpConnectionId) -> Result<(), McpError> {
        self.routes
            .lock()
            .map_err(|_| McpError::Internal("mobile MCP route map poisoned".into()))?
            .remove(&id);
        Ok(())
    }

    fn is_local(spec: &McpTransportSpec) -> bool {
        matches!(spec, McpTransportSpec::InProcess { .. })
    }

    fn is_remote(spec: &McpTransportSpec) -> bool {
        matches!(
            spec,
            McpTransportSpec::Sse { .. } | McpTransportSpec::Http { .. }
        )
    }
}

#[async_trait]
impl McpTransport for MobileMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let (route, connection) = if Self::is_local(spec) {
            (Route::LocalApps, self.local_apps.connect(spec).await?)
        } else if Self::is_remote(spec) {
            (Route::Remote, self.remote.connect(spec).await?)
        } else {
            return Err(McpError::UnsupportedTransport(spec.transport_kind()));
        };
        if let Err(error) = self.remember(connection.connection_id, route) {
            match route {
                Route::LocalApps => self.local_apps.disconnect_sync(connection.connection_id),
                Route::Remote => self.remote.disconnect_sync(connection.connection_id),
            }
            return Err(error);
        }
        Ok(connection)
    }

    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        let (route, result) = if Self::is_local(spec) {
            (
                Route::LocalApps,
                self.local_apps
                    .connect_and_initialize(spec, options)
                    .await?,
            )
        } else if Self::is_remote(spec) {
            (
                Route::Remote,
                self.remote.connect_and_initialize(spec, options).await?,
            )
        } else {
            return Err(McpError::UnsupportedTransport(spec.transport_kind()));
        };
        if let Err(error) = self.remember(result.connection.connection_id, route) {
            match route {
                Route::LocalApps => self
                    .local_apps
                    .disconnect_sync(result.connection.connection_id),
                Route::Remote => self.remote.disconnect_sync(result.connection.connection_id),
            }
            return Err(error);
        }
        Ok(result)
    }

    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.initialize(conn).await,
            Route::Remote => self.remote.initialize(conn).await,
        }
    }

    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.list_tools(conn).await,
            Route::Remote => self.remote.list_tools(conn).await,
        }
    }

    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.list_resources(conn).await,
            Route::Remote => self.remote.list_resources(conn).await,
        }
    }

    async fn list_resource_templates(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.list_resource_templates(conn).await,
            Route::Remote => self.remote.list_resource_templates(conn).await,
        }
    }

    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.list_prompts(conn).await,
            Route::Remote => self.remote.list_prompts(conn).await,
        }
    }

    async fn call_tool(
        &self,
        conn: &McpRawConnection,
        tool: &str,
        input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.call_tool(conn, tool, input).await,
            Route::Remote => self.remote.call_tool(conn, tool, input).await,
        }
    }

    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.read_resource(conn, uri).await,
            Route::Remote => self.remote.read_resource(conn, uri).await,
        }
    }

    async fn ping(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        match self.route_for(conn_id)? {
            Route::LocalApps => self.local_apps.ping(conn_id).await,
            Route::Remote => self.remote.ping(conn_id).await,
        }
    }

    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.notifications(conn).await,
            Route::Remote => self.remote.notifications(conn).await,
        }
    }

    async fn handle_elicitation(
        &self,
        conn: &McpRawConnection,
        request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        match self.route_for(conn.connection_id)? {
            Route::LocalApps => self.local_apps.handle_elicitation(conn, request).await,
            Route::Remote => self.remote.handle_elicitation(conn, request).await,
        }
    }

    async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        let route = self.route_for(conn_id)?;
        let result = match route {
            Route::LocalApps => self.local_apps.disconnect(conn_id).await,
            Route::Remote => self.remote.disconnect(conn_id).await,
        };
        if result.is_ok() {
            self.forget(conn_id)?;
        }
        result
    }

    fn disconnect_sync(&self, conn_id: McpConnectionId) {
        let route = self
            .routes
            .lock()
            .ok()
            .and_then(|mut routes| routes.remove(&conn_id));
        match route {
            Some(Route::LocalApps) => self.local_apps.disconnect_sync(conn_id),
            Some(Route::Remote) => self.remote.disconnect_sync(conn_id),
            None => {}
        }
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![
            McpTransportKind::InProcess,
            McpTransportKind::Sse,
            McpTransportKind::Http,
        ]
    }
}

impl mcp::RawConnectionProvider for MobileMcpTransport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
        match self.route_for(id).ok()? {
            Route::LocalApps => None,
            Route::Remote => self.remote.connection_for(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp::RawConnectionProvider;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::broadcast;

    async fn one_request_http_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // The test only needs one request (initialize); the HTTP writer
            // closes the connection after the response is consumed.
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = String::from_utf8_lossy(&bytes);
            let id = body
                .split("\"id\":")
                .nth(1)
                .and_then(|value| value.split([',', '}']).next())
                .unwrap_or("1");
            let response_body = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"mobile-route-test\",\"version\":\"test\"}}}}}}"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(), response_body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}/mcp")
    }

    fn remote_reply(body: &Value) -> Option<Value> {
        let id = body.get("id")?.clone();
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "mobile-e2e", "version": "test"}
            }),
            "tools/list" => serde_json::json!({
                "tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]
            }),
            "tools/call" => serde_json::json!({
                "content": [{"type": "text", "text": body.pointer("/params/arguments/text").and_then(Value::as_str).unwrap_or_default()}],
                "isError": false
            }),
            "resources/list" => serde_json::json!({
                "resources": [{"uri": "test://resource", "name": "resource", "mimeType": "text/plain"}]
            }),
            "resources/read" => serde_json::json!({
                "contents": [{"uri": "test://resource", "text": "resource body"}]
            }),
            "prompts/list" => serde_json::json!({
                "prompts": [{"name": "hello", "description": "Hello", "arguments": []}]
            }),
            "prompts/get" => serde_json::json!({
                "description": "Hello prompt",
                "messages": [{"role": "user", "content": {"type": "text", "text": "hello prompt"}}]
            }),
            "ping" => serde_json::json!({}),
            _ => serde_json::json!({}),
        };
        Some(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    async fn read_http_message(stream: &mut tokio::net::TcpStream) -> Option<(String, Value)> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let header = std::str::from_utf8(&bytes[..header_end]).ok()?;
        let method = header.split_whitespace().next()?.to_string();
        let content_length = header
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let body = if content_length == 0 {
            serde_json::json!({})
        } else {
            serde_json::from_slice(&bytes[header_end..header_end + content_length]).ok()?
        };
        Some((method, body))
    }

    async fn spawn_http_mock() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let count = count.clone();
                tokio::spawn(async move {
                    let Some((_method, body)) = read_http_message(&mut stream).await else {
                        return;
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    let Some(reply) = remote_reply(&body) else {
                        return;
                    };
                    let body = reply.to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("http://{address}/mcp"), requests, task)
    }

    async fn spawn_sse_mock() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (replies, _) = broadcast::channel::<String>(32);
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let replies = replies.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let Some((method, body)) = read_http_message(&mut stream).await else {
                        return;
                    };
                    if method == "GET" {
                        let mut rx = replies.subscribe();
                        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                        // The MCP SSE transport learns its POST url from a named
                        // `endpoint` event and completes the connect only once
                        // that event arrives (`platform-common`'s
                        // `SseEndpointMode::EndpointEvent`). This mock opened the
                        // stream and never sent it, so every connect sat until
                        // the 10s deadline — the client was doing exactly what
                        // the protocol says.
                        if stream
                            .write_all(b"event: endpoint\ndata: /mcp\n\n")
                            .await
                            .is_err()
                        {
                            return;
                        }
                        while let Ok(reply) = rx.recv().await {
                            let event = format!("data: {reply}\n\n");
                            if stream.write_all(event.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    } else {
                        count.fetch_add(1, Ordering::SeqCst);
                        if let Some(reply) = remote_reply(&body) {
                            let _ = replies.send(reply.to_string());
                        }
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        (format!("http://{address}/mcp"), requests, task)
    }

    fn remote_spec(kind: &str, url: String) -> McpTransportSpec {
        let headers = lingxi_core::host::McpHeaders::new();
        match kind {
            "sse" => McpTransportSpec::Sse {
                url,
                headers,
                headers_helper: None,
                oauth: None,
            },
            _ => McpTransportSpec::Http {
                url,
                headers,
                headers_helper: None,
                oauth: None,
            },
        }
    }

    async fn exercise_mobile_remote(kind: &str) {
        let root = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalAppsMcpTransport::new(root.path().join("local")));
        let remote = Arc::new(RemoteMcpTransport::new());
        let transport = MobileMcpTransport::new(local, remote);
        let (url, requests, server_task) = if kind == "sse" {
            spawn_sse_mock().await
        } else {
            spawn_http_mock().await
        };
        let result = transport
            .connect_and_initialize(
                &remote_spec(kind, url),
                McpConnectOptions {
                    expected_era: Some(lingxi_core::host::McpProtocolEra::Legacy),
                    deadline_ms: 10_000,
                    probe_timeout_ms: None,
                    elicitation: lingxi_core::host::McpElicitationCapabilities::default(),
                },
            )
            .await
            .unwrap();
        let connection_id = result.connection.connection_id;
        assert!(result.capabilities.tools);
        assert_eq!(
            transport
                .list_tools(&result.connection)
                .await
                .unwrap()
                .len(),
            1
        );
        let called = transport
            .call_tool(
                &result.connection,
                "echo",
                serde_json::json!({"text": "mobile"}),
            )
            .await
            .unwrap();
        assert_eq!(called.content[0]["text"], "mobile");
        assert_eq!(
            transport
                .list_resources(&result.connection)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            transport
                .read_resource(&result.connection, "test://resource")
                .await
                .unwrap()
                .content,
            "resource body"
        );
        assert_eq!(
            transport
                .list_prompts(&result.connection)
                .await
                .unwrap()
                .len(),
            1
        );

        let raw = transport
            .connection_for(connection_id)
            .expect("remote route must expose raw connection");
        let client = mcp::McpClient::new("mobile-e2e", root.path().to_path_buf(), raw).await;
        let prompt = client
            .get_prompt("hello", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(prompt["messages"][0]["content"]["text"], "hello prompt");

        transport.disconnect(connection_id).await.unwrap();
        assert!(transport.connection_for(connection_id).is_none());
        assert!(transport.ping(connection_id).await.is_err());
        assert!(requests.load(Ordering::SeqCst) >= 7);
        server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mobile_composite_http_roundtrip_covers_raw_catalog_and_disconnect() {
        exercise_mobile_remote("http").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mobile_composite_sse_roundtrip_covers_raw_catalog_and_disconnect() {
        exercise_mobile_remote("sse").await;
    }

    #[tokio::test]
    async fn route_raw_disconnect_and_unsupported_are_explicit() {
        let root = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let remote = Arc::new(RemoteMcpTransport::new());
        let transport = MobileMcpTransport::new(local, remote);

        let local_connection = transport
            .connect(&McpTransportSpec::InProcess {
                registry_key: "local_apps".into(),
            })
            .await
            .unwrap();
        assert!(
            transport
                .connection_for(local_connection.connection_id)
                .is_none(),
            "Local Apps is in-process and has no JSON-RPC raw connection"
        );
        transport
            .disconnect(local_connection.connection_id)
            .await
            .unwrap();
        assert!(matches!(
            transport.ping(local_connection.connection_id).await,
            Err(McpError::Connection(_))
        ));

        let unsupported = transport
            .connect(&McpTransportSpec::WebSocket {
                url: "ws://127.0.0.1:1".into(),
                headers: lingxi_core::host::McpHeaders::new(),
                headers_helper: None,
            })
            .await;
        assert!(matches!(
            unsupported,
            Err(McpError::UnsupportedTransport(McpTransportKind::WebSocket))
        ));
    }

    #[tokio::test]
    async fn remote_route_exposes_raw_connection_until_disconnect() {
        let root = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let remote = Arc::new(RemoteMcpTransport::new());
        let transport = MobileMcpTransport::new(local, remote);
        let url = one_request_http_server().await;
        let connection = transport
            .connect(&McpTransportSpec::Http {
                url,
                headers: lingxi_core::host::McpHeaders::new(),
                headers_helper: None,
                oauth: None,
            })
            .await
            .unwrap();
        assert!(transport.connection_for(connection.connection_id).is_some());
        tokio::time::timeout(Duration::from_secs(5), transport.initialize(&connection))
            .await
            .unwrap()
            .unwrap();
        transport
            .disconnect(connection.connection_id)
            .await
            .unwrap();
        assert!(transport.connection_for(connection.connection_id).is_none());
    }

    #[tokio::test]
    async fn remember_failure_cleans_up_underlying_local_and_remote_connections() {
        let root = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalAppsMcpTransport::new(root.path().join("local")));
        let remote = Arc::new(RemoteMcpTransport::new());
        let transport = MobileMcpTransport::new(local.clone(), remote.clone());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = transport.routes.lock().unwrap();
            panic!("poison route map for cleanup test");
        }));
        let error = transport
            .connect(&McpTransportSpec::InProcess {
                registry_key: "local_apps".into(),
            })
            .await
            .expect_err("poisoned route map must reject remember");
        assert!(matches!(error, McpError::Internal(_)));

        let remote_root = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalAppsMcpTransport::new(remote_root.path().join("local")));
        let remote = Arc::new(RemoteMcpTransport::new());
        let transport = MobileMcpTransport::new(local, remote.clone());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = transport.routes.lock().unwrap();
            panic!("poison route map for remote cleanup test");
        }));
        let url = one_request_http_server().await;
        let error = transport
            .connect_and_initialize(
                &McpTransportSpec::Http {
                    url,
                    headers: lingxi_core::host::McpHeaders::new(),
                    headers_helper: None,
                    oauth: None,
                },
                McpConnectOptions {
                    expected_era: Some(lingxi_core::host::McpProtocolEra::Legacy),
                    deadline_ms: 5_000,
                    probe_timeout_ms: None,
                    elicitation: lingxi_core::host::McpElicitationCapabilities::default(),
                },
            )
            .await
            .expect_err("poisoned route map must reject initialized remember");
        assert!(matches!(error, McpError::Internal(_)));
    }

    #[tokio::test]
    async fn local_apps_is_cache_ineligible_with_composite_registry() {
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("mcp-discovery-cache");
        let local = Arc::new(LocalAppsMcpTransport::new(root.path().join("apps")));
        let remote = Arc::new(RemoteMcpTransport::new());
        let transport = Arc::new(MobileMcpTransport::new(local, remote));
        let registry = mcp::McpRegistry::with_raw_conn(
            transport.clone() as Arc<dyn McpTransport>,
            transport.clone() as Arc<dyn mcp::RawConnectionProvider>,
        )
        .with_discovery_cache_store(mcp::DiscoveryCacheStore::new(&cache_root));
        registry
            .connect(mcp::McpServerConfig {
                name: "local_apps".into(),
                spec: McpTransportSpec::InProcess {
                    registry_key: "local_apps".into(),
                },
                scope: mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
                disabled: false,
                timeout_ms: None,
                always_load: true,
                tools: Vec::new(),
                tool_permissions: Default::default(),
                discovery_cache: None,
                config_error: None,
                metadata: Default::default(),
            })
            .await
            .unwrap();
        assert!(
            !cache_root.exists(),
            "Local Apps InProcess must never create discovery-cache entries"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mobile_discovery_cache_rebuild_hits_without_second_dial() {
        if std::env::var_os("MCP_MOBILE_CACHE_CHILD").is_none() {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mcp_transport::tests::mobile_discovery_cache_rebuild_hits_without_second_dial",
                    "--nocapture",
                ])
                .env("MCP_MOBILE_CACHE_CHILD", "1")
                .env(mcp::discovery_cache::ENV_ENABLED, "true")
                .output()
                .await
                .expect("spawn isolated cache test child");
            assert!(
                output.status.success(),
                "isolated cache test failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let cache_root = root.path().join("mcp-discovery-cache");
        let (url, requests, server_task) = spawn_http_mock().await;
        let config = mcp::McpServerConfig {
            name: "mobile-cache".into(),
            spec: remote_spec("http", url),
            scope: mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            discovery_cache: Some(true),
            config_error: None,
            metadata: Default::default(),
        };

        let first_local = Arc::new(LocalAppsMcpTransport::new(root.path().join("local-1")));
        let first_remote = Arc::new(RemoteMcpTransport::new());
        let first_transport = Arc::new(MobileMcpTransport::new(first_local, first_remote));
        let first_registry = mcp::McpRegistry::with_raw_conn(
            first_transport.clone() as Arc<dyn McpTransport>,
            first_transport.clone() as Arc<dyn mcp::RawConnectionProvider>,
        )
        .with_discovery_cache_store(mcp::DiscoveryCacheStore::new(&cache_root));
        first_registry.connect(config.clone()).await.unwrap();
        let first_request_count = requests.load(Ordering::SeqCst);
        assert!(first_request_count > 0, "initial discovery must dial HTTP");
        assert!(
            cache_root.exists(),
            "initial discovery must persist a cache"
        );

        // A second composition root gets a fresh route map and registry, but
        // the same app-private cache root. Closing the mock proves the hit is
        // served before the transport has any opportunity to dial.
        server_task.abort();
        let second_local = Arc::new(LocalAppsMcpTransport::new(root.path().join("local-2")));
        let second_remote = Arc::new(RemoteMcpTransport::new());
        let second_transport = Arc::new(MobileMcpTransport::new(second_local, second_remote));
        let second_registry = mcp::McpRegistry::with_raw_conn(
            second_transport.clone() as Arc<dyn McpTransport>,
            second_transport.clone() as Arc<dyn mcp::RawConnectionProvider>,
        )
        .with_discovery_cache_store(mcp::DiscoveryCacheStore::new(&cache_root));
        let cached_id = second_registry.connect(config).await.unwrap();
        assert!(matches!(
            second_registry.connections.read().await.get("mobile-cache"),
            Some(mcp::connection::McpConnectionState::Cached { .. })
        ));
        assert_eq!(requests.load(Ordering::SeqCst), first_request_count);
        assert!(
            second_transport.connection_for(cached_id).is_none(),
            "a cache hit must not install a live remote connection"
        );
    }
}
