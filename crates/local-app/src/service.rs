//! The response a host returns for a request inside an app's WebView.
//!
//! Each app runs at its own origin, keyed by its id (Android
//! `https://<id>.lingxi-app.invalid`, iOS a loopback port per app), so its
//! `localStorage` and IndexedDB survive new revisions and stay apart from
//! every other app. The host forwards `(id, path)`; which document answers,
//! and with which headers, is decided here for every platform alike.

use std::sync::Arc;

use crate::store::{AppSummary, LocalAppId, LocalAppStore, SourceFormat, StoreError};

/// What `window.__lingxiAppBoot` starts as in a document rendered from a
/// fragment. Serving the document puts the app's initial state in its place.
pub const BOOT_PLACEHOLDER: &str = "/*lingxi-app-boot*/null";

/// JSON text safe to place inside a `<script>` element.
fn script_json(value: &serde_json::Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The policy every app document is served with. The same sandbox and
/// sources as installed market apps, plus `data:` and `blob:` for `fetch`,
/// which single-file builds use to reach their inlined assets.
pub const CONTENT_SECURITY_POLICY: &str = "sandbox allow-scripts allow-forms allow-modals allow-downloads allow-same-origin; default-src 'self' data: blob: https:; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob: https:; style-src 'self' 'unsafe-inline' https:; connect-src 'self' data: blob: https: wss:; worker-src 'none'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Device features no app may ask for.
pub const PERMISSIONS_POLICY: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=()";

/// A response for the host's request handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResponse {
    /// HTTP status: 200 or 404.
    pub status: u16,
    /// Headers, including `Content-Type` and the policies.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl HostResponse {
    fn not_found() -> Self {
        Self {
            status: 404,
            headers: vec![
                ("Content-Type".into(), "text/plain; charset=utf-8".into()),
                ("Cache-Control".into(), "no-store".into()),
                (
                    "Content-Security-Policy".into(),
                    CONTENT_SECURITY_POLICY.into(),
                ),
            ],
            body: b"not found".to_vec(),
        }
    }

    fn document(html: String) -> Self {
        Self {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html; charset=utf-8".into()),
                ("Cache-Control".into(), "no-store".into()),
                ("X-Content-Type-Options".into(), "nosniff".into()),
                ("Referrer-Policy".into(), "no-referrer".into()),
                (
                    "Content-Security-Policy".into(),
                    CONTENT_SECURITY_POLICY.into(),
                ),
                ("Permissions-Policy".into(), PERMISSIONS_POLICY.into()),
            ],
            body: html.into_bytes(),
        }
    }
}

/// Host service over one app library.
pub struct LocalAppService {
    store: Arc<LocalAppStore>,
}

impl LocalAppService {
    /// The service over `store`.
    #[must_use]
    pub fn new(store: Arc<LocalAppStore>) -> Self {
        Self { store }
    }

    /// The store behind this service.
    #[must_use]
    pub fn store(&self) -> &Arc<LocalAppStore> {
        &self.store
    }

    /// Answer one request of app `id` for `path` (the URL path, without
    /// query or fragment). An app is one document: `/` and `/index.html`
    /// serve its current revision, anything else is a 404. A document
    /// rendered from a fragment gets the app's initial state, which its
    /// runtime uses on the first launch only.
    pub async fn serve(&self, id: &str, path: &str) -> HostResponse {
        let Some(id) = LocalAppId::parse(id) else {
            return HostResponse::not_found();
        };
        if path != "/" && path != "/index.html" {
            return HostResponse::not_found();
        }
        let Ok(app) = self.store.read_current(&id).await else {
            return HostResponse::not_found();
        };
        let mut html = app.html;
        if app.summary.format == SourceFormat::Fragment {
            let state = self
                .store
                .read_initial_state(&id)
                .await
                .ok()
                .flatten()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
            if let Some(state) = state {
                html = html.replacen(BOOT_PLACEHOLDER, &script_json(&state), 1);
            }
        }
        HostResponse::document(html)
    }

    /// Every app, most recently updated first.
    ///
    /// # Errors
    /// Store failure.
    pub async fn list(&self) -> Result<Vec<AppSummary>, StoreError> {
        self.store.list().await
    }

    /// One app, or `None` when the library has no such app.
    pub async fn get(&self, id: &str) -> Option<AppSummary> {
        self.store.get(&LocalAppId::parse(id)?).await.ok()
    }

    /// Delete an app. `false` when it was not there.
    ///
    /// # Errors
    /// Store failure.
    pub async fn delete(&self, id: &str) -> Result<bool, StoreError> {
        let Some(id) = LocalAppId::parse(id) else {
            return Ok(false);
        };
        self.store.delete(&id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::FileSystem;

    fn header<'a>(response: &'a HostResponse, name: &str) -> Option<&'a str> {
        response
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn serves_the_current_revision_with_the_app_policies() {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let store = Arc::new(LocalAppStore::new(fs, dir.path().to_path_buf()));
        let id = store
            .publish(None, Some("App"), None, "<!doctype html>v1", 0)
            .await
            .unwrap()
            .id;
        store
            .publish(Some(&id), None, None, "<!doctype html>v2", 1)
            .await
            .unwrap();
        let service = LocalAppService::new(store);
        for path in ["/", "/index.html"] {
            let response = service.serve(id.as_str(), path).await;
            assert_eq!(response.status, 200, "{path}");
            assert_eq!(response.body, b"<!doctype html>v2");
            assert_eq!(
                header(&response, "Content-Type"),
                Some("text/html; charset=utf-8")
            );
            assert!(header(&response, "Content-Security-Policy")
                .unwrap()
                .starts_with("sandbox allow-scripts"));
            assert!(header(&response, "Permissions-Policy")
                .unwrap()
                .contains("camera=()"));
        }
        for (app, path) in [
            (id.as_str(), "/favicon.ico"),
            (id.as_str(), "/../index.html"),
            ("a0000000000", "/"),
            ("../x", "/"),
        ] {
            assert_eq!(service.serve(app, path).await.status, 404, "{app}{path}");
        }
        assert_eq!(service.get(id.as_str()).await.unwrap().revision, 2);
        assert_eq!(service.list().await.unwrap().len(), 1);
        assert!(service.delete(id.as_str()).await.unwrap());
        assert_eq!(service.serve(id.as_str(), "/").await.status, 404);
        assert!(!service.delete("Bad Id").await.unwrap());
    }

    #[tokio::test]
    async fn fragment_apps_are_served_with_their_initial_state() {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let store = Arc::new(LocalAppStore::new(fs, dir.path().to_path_buf()));
        let document =
            format!("<!doctype html><script>window.__lingxiAppBoot={BOOT_PLACEHOLDER};</script>");
        let state =
            r#"{"version":2,"modelContent":"</script><script>x()</script>","privateContent":null}"#;
        let id = store
            .publish_revision(
                None,
                Some("Chart"),
                None,
                crate::store::NewRevision {
                    document: &document,
                    fragment: Some("<p>f</p>"),
                    saved_from: None,
                    initial_state: Some(state),
                },
                0,
            )
            .await
            .unwrap()
            .id;
        let service = LocalAppService::new(store.clone());
        let served = String::from_utf8(service.serve(id.as_str(), "/").await.body).unwrap();
        let boot = served
            .split("window.__lingxiAppBoot=")
            .nth(1)
            .and_then(|rest| rest.split(";</script>").next())
            .unwrap();
        assert!(!boot.contains('<'), "{boot}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(boot).unwrap(),
            serde_json::from_str::<serde_json::Value>(state).unwrap()
        );
        assert!(
            !store
                .read_current(&id)
                .await
                .unwrap()
                .html
                .contains("modelContent"),
            "the stored document never carries the state"
        );

        // A document app keeps the text as written.
        let plain = store
            .publish(None, Some("Plain"), None, &document, 0)
            .await
            .unwrap()
            .id;
        let served = service.serve(plain.as_str(), "/").await.body;
        assert_eq!(served, document.as_bytes());
    }
}
