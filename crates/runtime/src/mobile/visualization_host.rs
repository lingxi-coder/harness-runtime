//! Native-host surface for inline visualizations on iOS and Android.
//!
//! The WKWebView / Android WebView host forwards every request for its
//! dedicated origin to [`VisualizationHost::serve`] and relays shell messages
//! to the mount and state methods here. Authorization, document assembly,
//! CSP headers and compare-and-swap state all live in
//! [`visualization::VisualizationService`], identical to the desktop host.
//!
//! Every method is synchronous over the engine handle's tokio runtime:
//! Android's `shouldInterceptRequest` is a blocking callback, and iOS calls
//! these from a background queue. Never call them on a UI thread.

use std::collections::HashMap;
use std::sync::Arc;

use visualization::document::{Theme, ThemeMode};
use visualization::{
    MountRequest, MountStateWrite, VisualizationId, VisualizationRef, VisualizationService,
};

/// Theme the native host is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct VisualizationThemeDto {
    /// `true` for dark appearance.
    pub dark: bool,
    /// Values for the upstream CSS variables (`--color-background-primary`, …).
    pub tokens: HashMap<String, String>,
}

/// A granted mount: what the shell needs to load the content frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct VisualizationMountDto {
    /// Capability for this mount's document and state writes.
    pub token: String,
    /// Generation stamped on every shell message of this mount.
    pub generation: u64,
    /// URL the shell loads into its sandboxed frame.
    pub doc_url: String,
    /// Revision title.
    pub title: String,
}

/// One response header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct VisualizationHeaderDto {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// A response for the WebView's scheme handler / request interceptor.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct VisualizationResponseDto {
    /// HTTP status (200 or 404).
    pub status: u16,
    /// MIME type without parameters, for APIs that take it separately.
    pub mime_type: String,
    /// Every header, including `Content-Type` and any CSP.
    pub headers: Vec<VisualizationHeaderDto>,
    /// Body bytes.
    pub body: Vec<u8>,
}

/// Outcome of a state write.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct VisualizationStateWriteDto {
    /// The state is durable at `version`.
    pub saved: bool,
    /// New version when saved, otherwise the winning version on conflict (or 0).
    pub version: u64,
    /// Rejection reason: `stale_mount`, `conflict`, `too_large`, `invalid`, `unavailable`.
    pub reason: Option<String>,
    /// On `conflict`, the winning state as `{version, modelContent, privateContent}` JSON.
    pub current_state_json: Option<String>,
}

/// One stored revision.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct VisualizationRevisionDto {
    /// Visualization id.
    pub id: String,
    /// Revision number.
    pub revision: u32,
    /// Title at that revision.
    pub title: String,
    /// Publish time, Unix milliseconds.
    pub created_at_ms: u64,
}

/// Visualization host bound to one origin, e.g. `lingxi-viz://visualization`
/// (iOS) or `https://lingxi-visualization.invalid` (Android).
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct VisualizationHost {
    service: VisualizationService,
    runtime: tokio::runtime::Handle,
}

impl VisualizationHost {
    /// A host over the shared store of `config_home`, answering for
    /// `origin`. `None` when `origin` is not `scheme://host`.
    #[must_use]
    pub fn for_config_home(
        fs: Arc<dyn lingxi_core::host::FileSystem>,
        config_home: &std::path::Path,
        origin: &str,
        runtime: tokio::runtime::Handle,
    ) -> Option<Self> {
        let store = crate::inline_visualization::shared_store(fs, config_home);
        let service = VisualizationService::new(store, origin).ok()?;
        Some(Self { service, runtime })
    }
}

fn session_uuid(session_id: &str) -> Option<uuid::Uuid> {
    lingxi_core::types::SessionId::parse_prefixed(session_id).map(|id| id.as_uuid())
}

fn theme(theme: VisualizationThemeDto) -> Theme {
    Theme {
        mode: if theme.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        tokens: theme.tokens.into_iter().collect(),
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl VisualizationHost {
    /// URL the dedicated WebView loads first: the trusted shell page.
    #[must_use]
    pub fn shell_url(&self) -> String {
        self.service.shell_url()
    }

    /// Origin every request of this host belongs to.
    #[must_use]
    pub fn origin(&self) -> String {
        self.service.origin().to_string()
    }

    /// Authorize and register a mount of revision `revision` of `id` in the
    /// conversation `session_id`. `None` means the reference is forged,
    /// foreign or no longer stored: show the "unavailable" card.
    #[must_use]
    pub fn mount(
        &self,
        session_id: String,
        id: String,
        revision: u32,
        theme_dto: VisualizationThemeDto,
        locale: String,
        expanded: bool,
    ) -> Option<VisualizationMountDto> {
        let root_session = session_uuid(&session_id)?;
        let id = VisualizationId::parse(&id)?;
        let request = MountRequest {
            root_session,
            reference: VisualizationRef { id, revision },
            theme: theme(theme_dto),
            locale,
            expanded,
        };
        let ticket = self.runtime.block_on(self.service.mount(request)).ok()?;
        Some(VisualizationMountDto {
            token: ticket.token,
            generation: ticket.generation,
            doc_url: ticket.doc_url,
            title: ticket.title,
        })
    }

    /// Retire a mount; its late replies and state writes are rejected.
    pub fn unmount(&self, token: String) {
        self.service.unmount(&token);
    }

    /// Retire every mount of a conversation (session switch or reload).
    pub fn unmount_session(&self, session_id: String) {
        if let Some(root_session) = session_uuid(&session_id) {
            self.service.unmount_session(root_session);
        }
    }

    /// Answer one request for this origin. `path` is the URL path without
    /// query or fragment; anything outside the shell, assets and live mount
    /// documents is a 404.
    #[must_use]
    pub fn serve(&self, path: String) -> VisualizationResponseDto {
        let response = self.runtime.block_on(self.service.serve(&path));
        let mime_type = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
            .map(|(_, value)| value.split(';').next().unwrap_or(value).trim().to_string())
            .unwrap_or_else(|| "text/plain".to_string());
        VisualizationResponseDto {
            status: response.status,
            mime_type,
            headers: response
                .headers
                .into_iter()
                .map(|(name, value)| VisualizationHeaderDto { name, value })
                .collect(),
            body: response.body,
        }
    }

    /// Compare-and-swap a state write from the live mount `token`.
    #[must_use]
    pub fn write_state(
        &self,
        token: String,
        generation: u64,
        base_version: u64,
        model_content_json: String,
        private_content_json: String,
    ) -> VisualizationStateWriteDto {
        match self.runtime.block_on(self.service.write_state(
            &token,
            generation,
            base_version,
            &model_content_json,
            &private_content_json,
        )) {
            MountStateWrite::Saved { version } => VisualizationStateWriteDto {
                saved: true,
                version,
                reason: None,
                current_state_json: None,
            },
            MountStateWrite::Rejected { reason, current } => VisualizationStateWriteDto {
                saved: false,
                version: current.as_ref().map_or(0, |state| state.version),
                reason: Some(reason),
                current_state_json: current.and_then(|state| serde_json::to_string(&state).ok()),
            },
        }
    }

    /// Every stored revision of a conversation, oldest first per id.
    #[must_use]
    pub fn list(&self, session_id: String) -> Vec<VisualizationRevisionDto> {
        let Some(root_session) = session_uuid(&session_id) else {
            return Vec::new();
        };
        self.runtime
            .block_on(self.service.list(root_session))
            .unwrap_or_default()
            .into_iter()
            .flat_map(|summary| {
                let id = summary.id.as_str().to_string();
                summary
                    .revisions
                    .into_iter()
                    .map(move |revision| VisualizationRevisionDto {
                        id: id.clone(),
                        revision: revision.revision,
                        title: revision.title,
                        created_at_ms: revision.created_at_ms,
                    })
            })
            .collect()
    }

    /// License notices the app's "open source" screen must show.
    #[must_use]
    pub fn third_party_notices(&self) -> String {
        visualization::assets::third_party_notices()
    }
}
