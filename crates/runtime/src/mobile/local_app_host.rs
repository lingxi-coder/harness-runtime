//! Native-host surface for local apps on iOS and Android.
//!
//! The Apps page lists and deletes apps here, and each app's WebView answers
//! every request for its own origin with [`LocalAppHost::serve`]. Which
//! document answers, and with which policies, lives in
//! [`local_app::LocalAppService`], identical on every platform.
//!
//! Every method is synchronous over the engine handle's tokio runtime:
//! Android's `shouldInterceptRequest` is a blocking callback, and iOS calls
//! these from a background queue. Never call them on a UI thread.

use std::sync::Arc;

use local_app::{AppSummary, LocalAppService};

/// One app of the device's library.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LocalAppDto {
    /// App id: lowercase letters and digits, usable as a hostname label.
    pub id: String,
    /// Display name.
    pub name: String,
    /// One line on what it does; may be empty.
    pub summary: String,
    /// Current revision.
    pub revision: u32,
    /// Size of the current document in bytes.
    pub bytes: u64,
    /// First publish, Unix milliseconds.
    pub created_at_ms: u64,
    /// Publish of the current revision, Unix milliseconds.
    pub updated_at_ms: u64,
}

/// One response header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LocalAppHeaderDto {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// A response for the app WebView's request handler.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LocalAppResponseDto {
    /// HTTP status (200 or 404).
    pub status: u16,
    /// MIME type without parameters, for APIs that take it separately.
    pub mime_type: String,
    /// Every header, including `Content-Type` and the policies.
    pub headers: Vec<LocalAppHeaderDto>,
    /// Body bytes.
    pub body: Vec<u8>,
}

fn dto(app: AppSummary) -> LocalAppDto {
    LocalAppDto {
        id: app.id.as_str().to_string(),
        name: app.name,
        summary: app.summary,
        revision: app.revision,
        bytes: app.bytes,
        created_at_ms: app.created_at_ms,
        updated_at_ms: app.updated_at_ms,
    }
}

/// Local app host over this engine's app library.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct LocalAppHost {
    service: LocalAppService,
    runtime: tokio::runtime::Handle,
}

impl LocalAppHost {
    /// A host over the shared library of `config_home`.
    #[must_use]
    pub fn for_config_home(
        fs: Arc<dyn lingxi_core::host::FileSystem>,
        config_home: &std::path::Path,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let store = crate::local_apps::shared_store(fs, config_home);
        Self {
            service: LocalAppService::new(store),
            runtime,
        }
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl LocalAppHost {
    /// Every app, most recently updated first. Empty when the library cannot
    /// be read.
    #[must_use]
    pub fn list(&self) -> Vec<LocalAppDto> {
        self.runtime
            .block_on(self.service.list())
            .unwrap_or_default()
            .into_iter()
            .map(dto)
            .collect()
    }

    /// One app, or `None` when it is not (or no longer) in the library.
    #[must_use]
    pub fn get(&self, id: String) -> Option<LocalAppDto> {
        self.runtime.block_on(self.service.get(&id)).map(dto)
    }

    /// Answer one request of app `id`. `path` is the URL path without query
    /// or fragment; anything but `/` and `/index.html` is a 404.
    #[must_use]
    pub fn serve(&self, id: String, path: String) -> LocalAppResponseDto {
        let response = self.runtime.block_on(self.service.serve(&id, &path));
        let mime_type = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
            .map(|(_, value)| value.split(';').next().unwrap_or(value).trim().to_string())
            .unwrap_or_else(|| "text/plain".to_string());
        LocalAppResponseDto {
            status: response.status,
            mime_type,
            headers: response
                .headers
                .into_iter()
                .map(|(name, value)| LocalAppHeaderDto { name, value })
                .collect(),
            body: response.body,
        }
    }

    /// Delete an app and its documents. The host clears the app's WebView
    /// storage itself. `false` when the app was not there or could not be
    /// deleted.
    #[must_use]
    pub fn delete(&self, id: String) -> bool {
        self.runtime
            .block_on(self.service.delete(&id))
            .unwrap_or(false)
    }
}
