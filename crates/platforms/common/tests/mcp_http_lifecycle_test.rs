//! Integration tests for MCP HTTP discovery, event reconnection, and session cleanup.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::post,
    Json, Router,
};
use lingxi_core::host::McpTransportSpec;
use mcp::{ConfigScope, McpRegistry, McpServerConfig, McpServerMetadata};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, Mutex};

struct StateData {
    running: AtomicBool,
    gets: AtomicUsize,
    deletes: AtomicUsize,
    delay_delete: AtomicBool,
    supported: bool,
    events: broadcast::Sender<&'static str>,
    headers: Mutex<HeaderMap>,
}
struct RawConnections(Arc<platform_common::RemoteMcpTransport>);
impl mcp::RawConnectionProvider for RawConnections {
    fn connection_for(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Option<Arc<jsonrpc::Connection>> {
        self.0.connection_for(id)
    }
}
async fn post_frame(State(state): State<Arc<StateData>>, Json(frame): Json<Value>) -> Response {
    if frame["method"] == "notifications/initialized" {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = match frame["method"].as_str().unwrap_or("") {
        "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"apps","version":"1.0"}}),
        "tools/list" => json!({"tools":if state.running.load(Ordering::SeqCst) { vec![json!({"name":"app_tool","description":"App operation","inputSchema":{"type":"object","properties":{}}})] } else {vec![]}}),
        "ping" => json!({}),
        _ => return Json(json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Method not found"}})).into_response(),
    };
    let mut response =
        Json(json!({"jsonrpc":"2.0","id":frame["id"],"result":result})).into_response();
    if frame["method"] == "initialize" {
        response
            .headers_mut()
            .insert("mcp-session-id", "app-session".parse().unwrap());
    }
    response
}
async fn get_events(State(state): State<Arc<StateData>>, headers: HeaderMap) -> Response {
    state.gets.fetch_add(1, Ordering::SeqCst);
    *state.headers.lock().await = headers;
    if !state.supported {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let stream = futures::stream::unfold(state.events.subscribe(), |mut receiver| async move {
        match receiver.recv().await {
            Ok("close") | Err(_) => None,
            Ok(_) => Some((Ok::<_, Infallible>(Event::default().event("message").id("catalog-change").data(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}).to_string())), receiver)),
        }
    });
    Sse::new(stream).into_response()
}
async fn delete_session(State(state): State<Arc<StateData>>, headers: HeaderMap) -> StatusCode {
    assert_eq!(headers["mcp-session-id"], "app-session");
    if state.delay_delete.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    state.deletes.fetch_add(1, Ordering::SeqCst);
    StatusCode::OK
}
async fn fixture(supported: bool) -> (Arc<StateData>, McpRegistry, tokio::task::JoinHandle<()>) {
    let (events, _) = broadcast::channel(16);
    let state = Arc::new(StateData {
        running: AtomicBool::new(false),
        gets: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
        delay_delete: AtomicBool::new(false),
        supported,
        events,
        headers: Mutex::new(HeaderMap::new()),
    });
    let app = Router::new()
        .route(
            "/mcp",
            post(post_frame).get(get_events).delete(delete_session),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let transport = Arc::new(platform_common::RemoteMcpTransport::new());
    let registry =
        McpRegistry::with_raw_conn(transport.clone(), Arc::new(RawConnections(transport)));
    registry
        .connect(McpServerConfig {
            name: "apps".into(),
            spec: McpTransportSpec::Http {
                url,
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
            scope: ConfigScope::Dynamic,
            disabled: false,
            timeout_ms: Some(2000),
            discovery_cache: Some(false),
            always_load: false,
            tools: vec![],
            tool_permissions: Default::default(),
            config_error: None,
            metadata: McpServerMetadata::default(),
        })
        .await
        .unwrap();
    (state, registry, server)
}
async fn wait_for(mut check: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_existing_rust_registry_observes_app_start_stop_and_get_only_reconnection() {
    let (state, registry, server) = fixture(true).await;
    assert!(registry.tool_identity_catalog().await.is_empty());
    wait_for(async || state.gets.load(Ordering::SeqCst) == 1 && state.events.receiver_count() > 0)
        .await;
    assert_eq!(state.headers.lock().await["mcp-session-id"], "app-session");
    assert_eq!(
        state.headers.lock().await["mcp-protocol-version"],
        "2025-11-25"
    );
    let mut changes = registry.subscribe_catalog_changes();
    state.running.store(true, Ordering::SeqCst);
    state.events.send("change").unwrap();
    let change = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let change = changes.recv().await.unwrap();
            if change.kind == mcp::McpCatalogKind::Tools {
                break change;
            }
        }
    })
    .await
    .unwrap();
    registry.refresh_catalog(&change).await.unwrap();
    wait_for(async || registry.tool_identity_catalog().await.len() == 1).await;
    state.events.send("close").unwrap();
    state.running.store(false, Ordering::SeqCst);
    wait_for(async || state.gets.load(Ordering::SeqCst) >= 2 && state.events.receiver_count() > 0)
        .await;
    assert_eq!(
        state.headers.lock().await["last-event-id"],
        "catalog-change"
    );
    // No notification is emitted while GET is offline: reattachment must invalidate tools.
    let change = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let change = changes.recv().await.unwrap();
            if change.kind == mcp::McpCatalogKind::Tools {
                break change;
            }
        }
    })
    .await
    .unwrap();
    registry.refresh_catalog(&change).await.unwrap();
    wait_for(async || registry.tool_identity_catalog().await.is_empty()).await;
    registry.disconnect("apps").await.unwrap();
    assert_eq!(
        state.deletes.load(Ordering::SeqCst),
        1,
        "async disconnect drains DELETE before returning"
    );
    let gets = state.gets.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        state.gets.load(Ordering::SeqCst),
        gets,
        "closed clients never reconnect"
    );
    server.abort();
}

#[tokio::test]
async fn optional_get_405_keeps_post_calls_and_terminates_the_session_once() {
    let (state, registry, server) = fixture(false).await;
    wait_for(async || state.gets.load(Ordering::SeqCst) == 1).await;
    assert!(registry.tool_identity_catalog().await.is_empty());
    registry.disconnect("apps").await.unwrap();
    assert_eq!(state.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(state.gets.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn cancelled_disconnect_retains_the_http_cleanup_receipt_for_retry() {
    let (state, registry, server) = fixture(true).await;
    state.delay_delete.store(true, Ordering::SeqCst);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), registry.disconnect("apps"))
            .await
            .is_err()
    );
    assert_eq!(state.deletes.load(Ordering::SeqCst), 0);
    registry.disconnect("apps").await.unwrap();
    assert_eq!(state.deletes.load(Ordering::SeqCst), 1);
    server.abort();
}
