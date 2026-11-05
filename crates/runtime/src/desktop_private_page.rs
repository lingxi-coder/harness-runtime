//! The bridge-server's `private_page` request: serve one revision.
//!
//! The Electron main process forwards its `lingxi-page://page` scheme handler
//! here. The stored revision is the author fragment. This module wraps it in
//! the document shell and returns the bytes. There is no public URL.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use private_page::document::{self, CONTENT_SECURITY_POLICY};
use private_page::{PageId, PageRef, PageStore};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Serve { id: String, revision: u32 },
}

/// Answers `private_page` requests for one bridge process.
pub struct DesktopPrivatePageHost {
    store: Arc<PageStore>,
}

impl DesktopPrivatePageHost {
    /// A host over the shared store of `config_home`.
    #[must_use]
    pub fn for_config_home(fs: Arc<dyn lingxi_core::host::FileSystem>, config_home: &Path) -> Self {
        Self {
            store: crate::private_page::shared_store(fs, config_home),
        }
    }

    /// Answer one request.
    ///
    /// # Errors
    /// The params do not decode as a known `op`.
    pub async fn handle(&self, params: Utf16JsonProjection) -> Result<Utf16JsonProjection, String> {
        let source = params
            .to_json_string()
            .map_err(|_| "invalid private page request".to_string())?;
        let request: Request = serde_json::from_str(&source)
            .map_err(|_| "invalid private page request".to_string())?;
        let Request::Serve { id, revision } = request;
        let response = match document_for(&self.store, &id, revision).await {
            Some(body) => json!({
                "status": 200,
                "headers": [
                    ["Content-Type", "text/html; charset=utf-8"],
                    ["Cache-Control", "no-store"],
                    ["X-Content-Type-Options", "nosniff"],
                    ["Content-Security-Policy", CONTENT_SECURITY_POLICY],
                ],
                "body_base64": base64::engine::general_purpose::STANDARD.encode(body),
            }),
            None => json!({
                "status": 404,
                "headers": [
                    ["Content-Type", "text/plain; charset=utf-8"],
                    ["Cache-Control", "no-store"],
                    ["X-Content-Type-Options", "nosniff"],
                ],
                "body_base64": base64::engine::general_purpose::STANDARD.encode("not found"),
            }),
        };
        Utf16JsonProjection::from_serde_value(&response)
            .map_err(|_| "invalid private page response".to_string())
    }
}

async fn document_for(store: &PageStore, id: &str, revision: u32) -> Option<String> {
    if revision == 0 {
        return None;
    }
    let id = PageId::parse(id)?;
    let stored = store.read(&PageRef { id, revision }).await.ok()?;
    Some(document::wrap(&stored.title, &stored.fragment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serve_wraps_the_fragment_and_misses_an_unknown_revision() {
        let home = tempfile::tempdir().unwrap();
        let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
            platform_posix::PosixFileSystem::new(home.path().to_path_buf()),
        );
        let host = DesktopPrivatePageHost::for_config_home(fs, home.path());
        let published = host
            .store
            .publish(None, "Notes", "<p>Hello</p>", 1)
            .await
            .unwrap();
        let params = Utf16JsonProjection::from_serde_value(&json!({
            "op": "serve",
            "id": published.reference.id.as_str(),
            "revision": published.reference.revision,
        }))
        .unwrap();
        let served = host.handle(params).await.unwrap();
        let value: Value = serde_json::from_str(&served.to_json_string().unwrap()).unwrap();
        assert_eq!(value["status"], 200);
        let headers = value["headers"].as_array().unwrap();
        assert!(headers.iter().any(|header| {
            header[0] == "Content-Security-Policy" && header[1] == CONTENT_SECURITY_POLICY
        }));
        let body = base64::engine::general_purpose::STANDARD
            .decode(value["body_base64"].as_str().unwrap())
            .unwrap();
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("<p>Hello</p>"));
        assert!(html.contains("<title>Notes</title>"));
        assert!(!html.contains("http://"));

        let missing = Utf16JsonProjection::from_serde_value(&json!({
            "op": "serve",
            "id": published.reference.id.as_str(),
            "revision": 9,
        }))
        .unwrap();
        let missed = host.handle(missing).await.unwrap();
        let missed: Value = serde_json::from_str(&missed.to_json_string().unwrap()).unwrap();
        assert_eq!(missed["status"], 404);
    }
}
