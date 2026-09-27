use jsonrpc::Connection;
use lsp::LspRegistry;
use platform_api::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

struct CaptureTransport {
    id: protocol::McpConnectionId,
    connection: Arc<Connection>,
}

#[async_trait::async_trait]
impl LspTransport for CaptureTransport {
    async fn start_server(&self, _: &LspServerConfig) -> Result<LspRawConnection, LspError> {
        Ok(LspRawConnection {
            connection_id: self.id,
        })
    }

    async fn initialize(
        &self,
        _: &LspRawConnection,
        _: &str,
    ) -> Result<LspServerCapabilities, LspError> {
        Ok(LspServerCapabilities {
            text_document_sync: Some("full".to_string()),
            completion: false,
            hover: true,
            definition: true,
            references: true,
            diagnostics: true,
            symbols: true,
            formatting: false,
            rename: false,
            code_action: false,
        })
    }

    async fn request(&self, _: &LspRawConnection, _: &str, _: Value) -> Result<Value, LspError> {
        Err(LspError::Unavailable)
    }

    async fn notify(&self, _: &LspRawConnection, _: &str, _: Value) -> Result<(), LspError> {
        Err(LspError::Unavailable)
    }

    async fn connection(&self, _: protocol::McpConnectionId) -> Result<Arc<Connection>, LspError> {
        Ok(self.connection.clone())
    }

    async fn shutdown(&self, _: protocol::McpConnectionId) -> Result<(), LspError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

async fn read_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Value {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        reader.read_exact(&mut byte).await.unwrap();
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&header).unwrap();
    let length: usize = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn rust_config() -> LspServerConfig {
    LspServerConfig {
        name: "plugin:rust:server".to_string(),
        command: "rust-analyzer".to_string(),
        extension_to_language: HashMap::from([(".rs".to_string(), "rust".to_string())]),
        ..Default::default()
    }
}

#[tokio::test]
async fn successful_file_edits_open_change_and_save_with_monotonic_versions() {
    let (client_io, mut peer_io) = duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(client_io);
    let transport = Arc::new(CaptureTransport {
        id: protocol::McpConnectionId::new(),
        connection: Arc::new(Connection::new_lsp(reader, writer)),
    });
    let registry = LspRegistry::new(transport);
    registry
        .register_plugin_servers(protocol::PluginId::new(), vec![rust_config()])
        .await;

    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    let path = temp.path().to_path_buf();
    registry
        .sync_file_after_edit(&path, "fn first() {}\n")
        .await
        .unwrap();

    let opened = read_frame(&mut peer_io).await;
    assert_eq!(opened["method"], "textDocument/didOpen");
    assert_eq!(opened["params"]["textDocument"]["version"], 1);
    assert_eq!(opened["params"]["textDocument"]["text"], "fn first() {}\n");

    registry
        .sync_file_after_edit(&path, "fn second() {}\n")
        .await
        .unwrap();
    let changed = read_frame(&mut peer_io).await;
    assert_eq!(changed["method"], "textDocument/didChange");
    assert_eq!(changed["params"]["textDocument"]["version"], 2);
    assert_eq!(
        changed["params"]["contentChanges"],
        json!([{ "text": "fn second() {}\n" }])
    );
    let saved = read_frame(&mut peer_io).await;
    assert_eq!(saved["method"], "textDocument/didSave");
    assert_eq!(
        saved["params"]["textDocument"]["uri"],
        lsp_types::Url::from_file_path(Path::new(&path))
            .unwrap()
            .as_str()
    );

    peer_io.shutdown().await.unwrap();
}
