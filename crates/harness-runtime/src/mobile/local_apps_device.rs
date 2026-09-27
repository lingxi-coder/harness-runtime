//! Live device-capability handles for the local-apps bridge.
//!
//! `ProfileApps` is cached process-wide while engines rebuild per connection
//! (see `local_apps_profile`). The Swift/Kotlin-backed device objects belong
//! to ONE connection's platform, so — exactly like `SharedLlm` — the broker
//! must never pin them: it reads through this cell on every bridge call, and
//! [`crate::mobile::local_apps_profile::profile_apps`] swaps the whole set on every
//! (re)build, cached hit or not. A bare `OnceLock<Arc<dyn CameraControl>>`
//! here would dispatch a fresh connection's capture into a torn-down engine's
//! Swift object.

use platform_api::{
    audio::AudioService, CalendarProvider, CameraControl, Clipboard, ContactsProvider,
    DeepLinkOpener, DeviceStatusProvider, HapticService, LocationProvider, NotificationService,
    SharingService,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Per-app cap on retained media. Two default-preset photos plus a long
/// recording fit; past that the oldest handle is evicted.
const MAX_MEDIA_ENTRIES_PER_APP: usize = 8;
/// Per-app byte cap, checked after the entry count.
const MAX_MEDIA_BYTES_PER_APP: usize = 16 * 1024 * 1024;

/// One retained device capture, addressable by handle.
#[derive(Clone)]
pub(crate) struct MediaEntry {
    /// MIME type, e.g. `image/jpeg` or `audio/m4a`.
    pub(crate) media_type: String,
    /// Raw (already decoded) bytes.
    pub(crate) bytes: Arc<Vec<u8>>,
}

/// Engine-side, per-app store of what the device ops just captured.
///
/// It exists because even the bridge's bounded long-context lane should not
/// carry base64 that the engine already owns. A handle skips a multi-megabyte
/// round trip out to JS and back, along with base64's size inflation.
///
/// Deliberately in memory only and bounded per app: this is a hand-off
/// buffer between two bridge calls, not storage. An app that wants to KEEP a
/// photo writes it to its own collection.
#[derive(Default)]
pub(crate) struct MediaCache {
    /// `app_id` -> insertion-ordered `(handle, entry)` pairs.
    entries: RwLock<HashMap<String, Vec<(String, MediaEntry)>>>,
}

impl MediaCache {
    /// Retain `entry` for `app_id` and return its handle.
    pub(crate) fn put(&self, app_id: &str, handle: String, entry: MediaEntry) -> String {
        let mut entries = self.entries.write().expect("media cache poisoned");
        let per_app = entries.entry(app_id.to_string()).or_default();
        per_app.push((handle.clone(), entry));
        while per_app.len() > MAX_MEDIA_ENTRIES_PER_APP
            || per_app
                .iter()
                .map(|(_, entry)| entry.bytes.len())
                .sum::<usize>()
                > MAX_MEDIA_BYTES_PER_APP
        {
            per_app.remove(0);
        }
        handle
    }

    /// Look one up. Handles stay valid until evicted — a chat that attaches
    /// the same photo twice works.
    pub(crate) fn get(&self, app_id: &str, handle: &str) -> Option<MediaEntry> {
        self.entries
            .read()
            .expect("media cache poisoned")
            .get(app_id)?
            .iter()
            .find(|(id, _)| id == handle)
            .map(|(_, entry)| entry.clone())
    }

    /// Drop everything an app retained (its runtime stopped, or it was
    /// deleted).
    pub(crate) fn clear_app(&self, app_id: &str) {
        self.entries
            .write()
            .expect("media cache poisoned")
            .remove(app_id);
    }
}

/// One connection's device handles, as read from its `Platform`. Every slot
/// is optional — a Store build without a runtime, a stub platform, or a
/// desktop host simply exposes none, and the bridge fails typed instead.
#[derive(Clone, Default)]
pub(crate) struct DeviceCapabilities {
    pub(crate) camera: Option<Arc<dyn CameraControl>>,
    /// Shared device AudioService. Local App capture, live transcription and
    /// silent synthesis use the same service as tools and Computer Use.
    pub(crate) audio: Option<Arc<dyn AudioService>>,
    pub(crate) location: Option<Arc<dyn LocationProvider>>,
    pub(crate) notifications: Option<Arc<dyn NotificationService>>,
    pub(crate) clipboard: Option<Arc<dyn Clipboard>>,
    pub(crate) share: Option<Arc<dyn SharingService>>,
    pub(crate) device_status: Option<Arc<dyn DeviceStatusProvider>>,
    pub(crate) haptics: Option<Arc<dyn HapticService>>,
    pub(crate) deep_link: Option<Arc<dyn DeepLinkOpener>>,
    pub(crate) calendar: Option<Arc<dyn CalendarProvider>>,
    pub(crate) contacts: Option<Arc<dyn ContactsProvider>>,
}

/// Mirror of `SharedLlm` for device handles: read fresh on every use,
/// swapped whole on every engine (re)build.
pub(crate) struct SharedDeviceCapabilities(RwLock<DeviceCapabilities>);

impl SharedDeviceCapabilities {
    pub(crate) fn new(devices: DeviceCapabilities) -> Self {
        Self(RwLock::new(devices))
    }

    /// The current handle set. Callers read per bridge call, never cache.
    pub(crate) fn current(&self) -> DeviceCapabilities {
        self.0
            .read()
            .expect("shared device capabilities poisoned")
            .clone()
    }

    /// Swap in a fresh connection's handles.
    pub(crate) fn replace(&self, devices: DeviceCapabilities) {
        *self.0.write().expect("shared device capabilities poisoned") = devices;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_api::{CameraError, CapturePhotoOpts, CapturedImage};

    struct FakeCamera;

    #[async_trait]
    impl CameraControl for FakeCamera {
        async fn capture_photo(
            &self,
            _opts: CapturePhotoOpts,
        ) -> Result<CapturedImage, CameraError> {
            Err(CameraError::DeviceUnavailable)
        }

        async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
            Err(CameraError::DeviceUnavailable)
        }
    }

    fn entry(size: usize) -> MediaEntry {
        MediaEntry {
            media_type: "image/jpeg".into(),
            bytes: Arc::new(vec![0u8; size]),
        }
    }

    #[test]
    fn a_handle_round_trips_and_is_scoped_to_its_app() {
        let cache = MediaCache::default();
        cache.put("app-a", "m-1".into(), entry(3));
        assert!(cache.get("app-a", "m-1").is_some());
        assert!(
            cache.get("app-b", "m-1").is_none(),
            "one app must never be able to attach another app's capture"
        );
        cache.clear_app("app-a");
        assert!(cache.get("app-a", "m-1").is_none());
    }

    #[test]
    fn the_cache_evicts_oldest_first_by_count_and_by_bytes() {
        let cache = MediaCache::default();
        for i in 0..12 {
            cache.put("app-a", format!("m-{i}"), entry(1));
        }
        assert!(cache.get("app-a", "m-0").is_none(), "count cap evicts");
        assert!(cache.get("app-a", "m-11").is_some());

        let cache = MediaCache::default();
        cache.put("app-a", "big-1".into(), entry(10 * 1024 * 1024));
        cache.put("app-a", "big-2".into(), entry(10 * 1024 * 1024));
        assert!(cache.get("app-a", "big-1").is_none(), "byte cap evicts");
        assert!(cache.get("app-a", "big-2").is_some());
    }

    /// The stale-handle fix in one assertion: after `replace`, `current`
    /// serves the NEW connection's handles — a reader that pinned the old
    /// set would keep dispatching into a dead engine's platform objects.
    #[test]
    fn replace_swaps_the_live_handle_set() {
        let shared = SharedDeviceCapabilities::new(DeviceCapabilities::default());
        assert!(shared.current().camera.is_none());

        shared.replace(DeviceCapabilities {
            camera: Some(Arc::new(FakeCamera)),
            ..DeviceCapabilities::default()
        });
        assert!(shared.current().camera.is_some());
        assert!(shared.current().audio.is_none());
    }
}
