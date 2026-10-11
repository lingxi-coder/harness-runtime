//! The ordinary McpClient tool path fulfils through its existing inbound hooks.
use async_trait::async_trait;
use lingxi_core::host::{McpNegotiatedProtocol, McpProtocolEra};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use mcp::{
    hook_dispatch::{ElicitationHookOutcome, ElicitationHookRequest, HookDispatcher},
    McpClient,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn source(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).expect("test fixture is JSON")
}

struct Hooks {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl HookDispatcher for Hooks {
    async fn dispatch_elicitation(
        &self,
        request: ElicitationHookRequest,
    ) -> ElicitationHookOutcome {
        assert_eq!(request.params.string_at("/message"), Some("Choose"));
        self.entered.notify_one();
        self.release.notified().await;
        ElicitationHookOutcome::Respond(source(
            &json!({"action":"accept","content":{"selected":"yes"}}),
        ))
    }
}
#[tokio::test]
async fn ordinary_stdio_tool_uses_registered_roots_and_elicitation_counter() {
    let (client_read, mut peer_write) = tokio::io::duplex(65536);
    let (peer_read, client_write) = tokio::io::duplex(65536);
    let connection = Arc::new(jsonrpc::Connection::new_line_delimited(
        client_read,
        client_write,
    ));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let mut peer = tokio::spawn(async move {
        let mut lines = BufReader::new(peer_read).lines();
        for round in 0..2 {
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request["method"], "tools/call");
            captured.lock().unwrap().push(request.clone());
            if round == 0 {
                let capabilities =
                    &request["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"];
                assert_eq!(capabilities["roots"]["listChanged"], true);
                assert_eq!(capabilities["elicitation"]["form"], json!({}));
                assert_eq!(capabilities["elicitation"]["url"], json!({}));
            }
            let result = if round == 0 {
                json!({"resultType":"input_required","requestState":"opaque","inputRequests":{"roots-key":{"id":"forged","jsonrpc":"bad","method":"roots/list"},"elicit-key":{"method":"elicitation/create","params":{"message":"Choose","requestedSchema":{"type":"object","properties":{"selected":{"type":"string"}}}}}}})
            } else {
                assert_eq!(
                    request["params"]["arguments"],
                    json!({"question":"original"})
                );
                assert_eq!(request["params"]["requestState"], "opaque");
                assert_eq!(
                    request["params"]["inputResponses"]["elicit-key"],
                    json!({"action":"accept","content":{"selected":"yes"}})
                );
                assert_eq!(
                    request["params"]["inputResponses"]["roots-key"]["roots"][0]["uri"],
                    "file:///tmp/ordinary-main"
                );
                json!({"resultType":"complete","content":[{"type":"text","text":"done"}]})
            };
            peer_write
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let client = Arc::new(
        McpClient::with_hook_dispatcher(
            "s",
            PathBuf::from("/tmp/ordinary-main"),
            connection,
            Some(Arc::new(Hooks {
                entered: entered.clone(),
                release: release.clone(),
            })),
        )
        .await
        .with_negotiated_protocol(McpNegotiatedProtocol {
            era: McpProtocolEra::Modern,
            version: "2026-07-28".into(),
        }),
    );
    let c = client.clone();
    let mut call = tokio::spawn(async move {
        c.call_tool_with_timeout(
            "mcp__s__worker",
            source(&json!({"question":"original"})),
            Duration::from_secs(2),
        )
        .await
    });
    tokio::select! {
        call_result = &mut call => panic!(
            "elicitation hook was not entered; MCP call completed first: {call_result:?}"
        ),
        peer_result = &mut peer => panic!(
            "elicitation hook was not entered; stdio peer completed first: {peer_result:?}"
        ),
        entered = tokio::time::timeout(Duration::from_secs(3), entered.notified()) => {
            assert!(entered.is_ok(), "elicitation hook was not entered within 3 seconds");
        }
    }
    assert!(client.has_pending_elicitation());
    release.notify_one();
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.content.string_at("/0/text"), Some("done"));
    assert!(!client.has_pending_elicitation());
    peer.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn cancelling_embedded_elicitation_releases_existing_pending_counter() {
    let (client_read, mut peer_write) = tokio::io::duplex(65536);
    let (peer_read, client_write) = tokio::io::duplex(65536);
    let connection = Arc::new(jsonrpc::Connection::new_line_delimited(
        client_read,
        client_write,
    ));
    let mut peer = tokio::spawn(async move {
        let mut lines = BufReader::new(peer_read).lines();
        let request: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        peer_write.write_all(format!("{}\n",json!({"jsonrpc":"2.0","id":request["id"],"result":{"resultType":"input_required","inputRequests":{"e":{"method":"elicitation/create","params":{"message":"Choose","requestedSchema":{"type":"object","properties":{}}}}}}})).as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let client = Arc::new(
        McpClient::with_hook_dispatcher(
            "s",
            PathBuf::from("/tmp"),
            connection,
            Some(Arc::new(Hooks {
                entered: entered.clone(),
                release,
            })),
        )
        .await
        .with_negotiated_protocol(McpNegotiatedProtocol {
            era: McpProtocolEra::Modern,
            version: "2026-07-28".into(),
        }),
    );
    let c = client.clone();
    let mut call = tokio::spawn(async move {
        c.call_tool_with_timeout("mcp__s__worker", source(&json!({})), Duration::from_secs(2))
            .await
    });
    tokio::select! {
        call_result = &mut call => panic!(
            "elicitation hook was not entered before cancellation; MCP call completed first: {call_result:?}"
        ),
        peer_result = &mut peer => panic!(
            "elicitation hook was not entered before cancellation; stdio peer completed first: {peer_result:?}"
        ),
        entered = tokio::time::timeout(Duration::from_secs(3), entered.notified()) => {
            assert!(entered.is_ok(), "elicitation hook was not entered within 3 seconds");
        }
    }
    assert!(client.has_pending_elicitation());
    call.abort();
    let _ = call.await;
    assert!(!client.has_pending_elicitation());
    peer.abort();
}
