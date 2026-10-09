//! The host-facing service every platform drives: issue mount tokens, answer
//! the scheme handler's requests, and gate state writes on a live mount.
//!
//! A host only forwards `(path)` from its WebView scheme handler and relays
//! shell messages; routing, authorization, document assembly, CSP headers and
//! state CAS all happen here, identically on Electron, iOS and Android.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use uuid::Uuid;

use crate::assets;
use crate::document::{self, BootState, DocumentParams, Theme, DOC_PATH};
use crate::reference::VisualizationRef;
use crate::store::{StateWrite, StoreError, VisualizationStore, VisualizationSummary};

/// Live mounts kept before the oldest is evicted.
pub const MAX_MOUNTS: usize = 64;

/// What a host asks to mount.
#[derive(Debug, Clone)]
pub struct MountRequest {
    /// Root session whose store holds the revision.
    pub root_session: Uuid,
    /// The revision.
    pub reference: VisualizationRef,
    /// Theme at mount time.
    pub theme: Theme,
    /// UI language.
    pub locale: String,
    /// Whether the mount starts expanded.
    pub expanded: bool,
}

/// A granted mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountTicket {
    /// Capability for this mount's document and state writes.
    pub token: String,
    /// Mount generation; stamped on every shell message.
    pub generation: u64,
    /// URL the shell loads into its sandboxed frame.
    pub doc_url: String,
    /// Revision title.
    pub title: String,
}

/// A response for the host's scheme handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResponse {
    /// HTTP status.
    pub status: u16,
    /// Headers, including `Content-Type` and any CSP.
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
            ],
            body: b"not found".to_vec(),
        }
    }

    fn ok(content_type: &str, body: Vec<u8>, extra: Vec<(String, String)>) -> Self {
        let mut headers = vec![
            ("Content-Type".to_string(), content_type.to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
            ("Referrer-Policy".to_string(), "no-referrer".to_string()),
            (
                "Cross-Origin-Resource-Policy".to_string(),
                "cross-origin".to_string(),
            ),
        ];
        headers.extend(extra);
        Self {
            status: 200,
            headers,
            body,
        }
    }
}

/// Outcome of a state write from a mount.
#[derive(Debug, Clone, PartialEq)]
pub enum MountStateWrite {
    /// Durable at `version`.
    Saved {
        /// New version.
        version: u64,
    },
    /// Not written.
    Rejected {
        /// Machine-readable reason: `stale_mount`, `conflict`, `too_large`, `invalid`, `unavailable`.
        reason: String,
        /// The winning state on `conflict`.
        current: Option<BootState>,
    },
}

#[derive(Debug, Clone)]
struct Mount {
    request: MountRequest,
    generation: u64,
    title: String,
    document_served: bool,
    created: u64,
}

/// Visualization host service for one origin.
pub struct VisualizationService {
    store: Arc<VisualizationStore>,
    origin: String,
    generation: AtomicU64,
    mounts: Mutex<HashMap<String, Mount>>,
}

impl VisualizationService {
    /// A service answering for `origin` (e.g. `lingxi-viz://visualization`).
    ///
    /// # Errors
    /// [`StoreError::Invalid`] when `origin` is not `scheme://host`.
    pub fn new(store: Arc<VisualizationStore>, origin: &str) -> Result<Self, StoreError> {
        if !document::valid_origin(origin) {
            return Err(StoreError::Invalid(format!(
                "invalid visualization origin {origin:?}"
            )));
        }
        Ok(Self {
            store,
            origin: origin.to_string(),
            generation: AtomicU64::new(0),
            mounts: Mutex::new(HashMap::new()),
        })
    }

    /// The store behind this service.
    #[must_use]
    pub fn store(&self) -> &Arc<VisualizationStore> {
        &self.store
    }

    /// The origin this service answers for.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// URL of the trusted shell page.
    #[must_use]
    pub fn shell_url(&self) -> String {
        format!("{}/shell.html", self.origin)
    }

    fn mounts(&self) -> std::sync::MutexGuard<'_, HashMap<String, Mount>> {
        self.mounts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Authorize and register a mount. Each call yields a fresh token and a
    /// newer generation, so a remount retires every late reply of the old one.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for a forged, foreign or swept reference.
    pub async fn mount(&self, request: MountRequest) -> Result<MountTicket, StoreError> {
        let revision = self
            .store
            .read_revision(request.root_session, &request.reference)
            .await?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let ticket = MountTicket {
            doc_url: format!("{}{DOC_PATH}{token}", self.origin),
            token: token.clone(),
            generation,
            title: revision.title.clone(),
        };
        let mut mounts = self.mounts();
        if mounts.len() >= MAX_MOUNTS {
            if let Some(oldest) = mounts
                .iter()
                .min_by_key(|(_, mount)| mount.created)
                .map(|(token, _)| token.clone())
            {
                mounts.remove(&oldest);
            }
        }
        mounts.insert(
            token,
            Mount {
                request,
                generation,
                title: revision.title,
                document_served: false,
                created: generation,
            },
        );
        Ok(ticket)
    }

    /// Retire a mount; later document loads and state writes on it fail.
    pub fn unmount(&self, token: &str) {
        self.mounts().remove(token);
    }

    /// Retire every mount of a root session (session switch, reload).
    pub fn unmount_session(&self, root_session: Uuid) {
        self.mounts()
            .retain(|_, mount| mount.request.root_session != root_session);
    }

    /// Answer one scheme-handler request for `path` (the URL path, without
    /// query or fragment).
    pub async fn serve(&self, path: &str) -> HostResponse {
        if let Some(token) = path.strip_prefix(DOC_PATH) {
            return self.serve_document(token).await;
        }
        let Some(asset) = assets::find(path) else {
            return HostResponse::not_found();
        };
        let extra = if asset.path == "/shell.html" {
            vec![(
                "Content-Security-Policy".to_string(),
                document::shell_csp(&self.origin),
            )]
        } else {
            Vec::new()
        };
        HostResponse::ok(asset.content_type, asset.bytes.to_vec(), extra)
    }

    async fn serve_document(&self, token: &str) -> HostResponse {
        // Single use: a reload of the content frame is treated as a crash and
        // must come back through a fresh mount.
        let mount = {
            let mut mounts = self.mounts();
            match mounts.get_mut(token) {
                Some(mount) if !mount.document_served => {
                    mount.document_served = true;
                    mount.clone()
                }
                _ => return HostResponse::not_found(),
            }
        };
        let Ok(revision) = self
            .store
            .read_revision(mount.request.root_session, &mount.request.reference)
            .await
        else {
            return HostResponse::not_found();
        };
        let state = self
            .store
            .read_state(mount.request.root_session, &mount.request.reference)
            .await
            .unwrap_or_default();
        let html = document::render_content_document(&DocumentParams {
            origin: &self.origin,
            title: &mount.title,
            fragment: &revision.fragment,
            theme: &mount.request.theme,
            locale: &mount.request.locale,
            generation: mount.generation,
            state: &state,
            expanded: mount.request.expanded,
        });
        HostResponse::ok(
            "text/html; charset=utf-8",
            html.into_bytes(),
            vec![(
                "Content-Security-Policy".to_string(),
                document::content_csp(&self.origin),
            )],
        )
    }

    fn live_mount(&self, token: &str, generation: u64) -> Option<Mount> {
        self.mounts()
            .get(token)
            .filter(|mount| mount.generation == generation)
            .cloned()
    }

    /// Confirmed state for a live mount.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the mount is gone or the revision was swept.
    pub async fn read_state(&self, token: &str, generation: u64) -> Result<BootState, StoreError> {
        let mount = self
            .live_mount(token, generation)
            .ok_or_else(|| StoreError::NotFound("mount".into()))?;
        self.store
            .read_state(mount.request.root_session, &mount.request.reference)
            .await
    }

    /// Compare-and-swap a state write from a live mount.
    pub async fn write_state(
        &self,
        token: &str,
        generation: u64,
        base_version: u64,
        model_content_json: &str,
        private_content_json: &str,
    ) -> MountStateWrite {
        let Some(mount) = self.live_mount(token, generation) else {
            return MountStateWrite::Rejected {
                reason: "stale_mount".into(),
                current: None,
            };
        };
        match self
            .store
            .write_state(
                mount.request.root_session,
                &mount.request.reference,
                base_version,
                model_content_json,
                private_content_json,
            )
            .await
        {
            Ok(StateWrite::Saved { version }) => MountStateWrite::Saved { version },
            Ok(StateWrite::Conflict { current }) => MountStateWrite::Rejected {
                reason: "conflict".into(),
                current: Some(current),
            },
            Err(StoreError::Quota(_)) => MountStateWrite::Rejected {
                reason: "too_large".into(),
                current: None,
            },
            Err(StoreError::Invalid(_)) => MountStateWrite::Rejected {
                reason: "invalid".into(),
                current: None,
            },
            Err(_) => MountStateWrite::Rejected {
                reason: "unavailable".into(),
                current: None,
            },
        }
    }

    /// Visualizations of a root session.
    ///
    /// # Errors
    /// Store failure.
    pub async fn list(&self, root_session: Uuid) -> Result<Vec<VisualizationSummary>, StoreError> {
        self.store.list(root_session).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Publisher;
    use lingxi_core::host::FileSystem;

    const ORIGIN: &str = "lingxi-viz://visualization";

    async fn fixture() -> (
        tempfile::TempDir,
        VisualizationService,
        Uuid,
        VisualizationRef,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let store = Arc::new(VisualizationStore::new(fs, dir.path().to_path_buf()));
        let session = Uuid::new_v4();
        let reference = store
            .publish(
                &Publisher {
                    root_session: session,
                    agent_id: None,
                },
                None,
                "Chart",
                "<div id=\"widget\">hello</div>",
                0,
            )
            .await
            .unwrap()
            .reference;
        let service = VisualizationService::new(store, ORIGIN).unwrap();
        (dir, service, session, reference)
    }

    fn request(session: Uuid, reference: &VisualizationRef) -> MountRequest {
        MountRequest {
            root_session: session,
            reference: reference.clone(),
            theme: Theme::default(),
            locale: "en".into(),
            expanded: false,
        }
    }

    fn header<'a>(response: &'a HostResponse, name: &str) -> Option<&'a str> {
        response
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn document_is_served_once_with_a_sandbox_csp() {
        let (_dir, service, session, reference) = fixture().await;
        let ticket = service.mount(request(session, &reference)).await.unwrap();
        assert!(ticket
            .doc_url
            .starts_with("lingxi-viz://visualization/doc/"));
        let path = ticket.doc_url.trim_start_matches(ORIGIN);
        let first = service.serve(path).await;
        assert_eq!(first.status, 200);
        assert!(header(&first, "Content-Security-Policy")
            .unwrap()
            .starts_with("sandbox allow-scripts;"));
        let body = String::from_utf8(first.body).unwrap();
        assert!(body.contains("<div id=\"widget\">hello</div>"));
        assert!(body.contains(&format!("\"generation\":{}", ticket.generation)));
        assert_eq!(
            service.serve(path).await.status,
            404,
            "a token's document is single use"
        );
    }

    #[tokio::test]
    async fn foreign_sessions_cannot_mount_and_stale_mounts_cannot_write() {
        let (_dir, service, session, reference) = fixture().await;
        assert!(matches!(
            service.mount(request(Uuid::new_v4(), &reference)).await,
            Err(StoreError::NotFound(_))
        ));
        let first = service.mount(request(session, &reference)).await.unwrap();
        let second = service.mount(request(session, &reference)).await.unwrap();
        assert!(second.generation > first.generation);
        assert_eq!(
            service
                .write_state(&first.token, second.generation, 0, "1", "null")
                .await,
            MountStateWrite::Rejected {
                reason: "stale_mount".into(),
                current: None
            }
        );
        assert_eq!(
            service
                .write_state(&first.token, first.generation, 0, "1", "null")
                .await,
            MountStateWrite::Saved { version: 1 }
        );
        let MountStateWrite::Rejected { reason, current } = service
            .write_state(&second.token, second.generation, 0, "2", "null")
            .await
        else {
            panic!("two mounts must not overwrite each other");
        };
        assert_eq!(reason, "conflict");
        assert_eq!(current.unwrap().version, 1);
        service.unmount(&first.token);
        assert!(matches!(
            service
                .write_state(&first.token, first.generation, 1, "3", "null")
                .await,
            MountStateWrite::Rejected { .. }
        ));
        service.unmount_session(session);
        assert!(service
            .read_state(&second.token, second.generation)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn shell_and_assets_route_with_headers() {
        let (_dir, service, _session, _reference) = fixture().await;
        let shell = service.serve("/shell.html").await;
        assert_eq!(shell.status, 200);
        assert!(header(&shell, "Content-Security-Policy")
            .unwrap()
            .contains("script-src lingxi-viz://visualization/shell.js"));
        assert_eq!(
            header(&service.serve("/asset/d3.min.js").await, "Content-Type"),
            Some("text/javascript; charset=utf-8")
        );
        for path in [
            "/",
            "/asset/",
            "/asset/../shell.js",
            "/doc/",
            "/doc/forged",
            "/state",
            "/shell.html?x",
        ] {
            assert_eq!(service.serve(path).await.status, 404, "{path}");
        }
        assert_eq!(service.shell_url(), "lingxi-viz://visualization/shell.html");
        assert!(VisualizationService::new(service.store().clone(), "https://x.test/path").is_err());
    }
}
