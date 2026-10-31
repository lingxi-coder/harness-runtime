//! `Platform` — the aggregate OS-capability seam (M8-P10).
//!
//! A composition root (`engine-desktop` / `engine-mobile`) is handed one
//! `Arc<dyn Platform>` and reads every OS handle from it: filesystem, HTTP,
//! clock, process, sandbox, worktree. Mobile builds additionally expose the
//! native device capabilities (camera, voice, share) — these default to `None`
//! so desktop platforms need not implement them. `computer_control` likewise
//! defaults to `None` (a desktop automation backend or a mobile `UniFFI` impl
//! supplies it).
//!
//! This is the single seam that lets the *same* core agent logic run on every
//! OS: the platform crate (`platform-posix` / `platform-ios` / …) decides the
//! concrete handles; library crates stay `#[cfg]`-free.

use crate::host::android_ui::AndroidUiAutomation;
use crate::host::calendar::CalendarProvider;
use crate::host::clipboard::Clipboard;
use crate::host::clock::Clock;
use crate::host::computer_control::ComputerControl;
use crate::host::contacts::ContactsProvider;
use crate::host::deep_link::DeepLinkOpener;
use crate::host::device_status::DeviceStatusProvider;
use crate::host::filesystem::FileSystem;
use crate::host::haptics::HapticService;
use crate::host::http::HttpTransport;
use crate::host::location::LocationProvider;
use crate::host::notification::NotificationService;
use crate::host::process::ProcessRunner;
use crate::host::sandbox::Sandbox;
use crate::host::secure_storage::SecureStorage;
use crate::host::share::SharingService;
use crate::host::worktree::WorktreeManager;
use media_api::audio::AudioService;
use media_api::camera::CameraControl;
use mobile_linux_api::MobileLinuxRuntime;
use std::sync::Arc;

/// Optional device handles provided together by a mobile platform.
///
/// Each handle keeps its own typed capability and shared ownership. The
/// composition root remains responsible for selecting and owning the native
/// implementation; an absent handle means that capability is unavailable.
#[derive(Clone, Default)]
pub struct DeviceServices {
    /// Native camera and photo library.
    pub camera: Option<Arc<dyn CameraControl>>,
    /// Device-local unified audio service.
    pub audio: Option<Arc<dyn AudioService>>,
    /// Native one-shot location provider.
    pub location: Option<Arc<dyn LocationProvider>>,
    /// Native share sheet.
    pub share: Option<Arc<dyn SharingService>>,
    /// Native system notification service.
    pub notifications: Option<Arc<dyn NotificationService>>,
    /// Native system clipboard.
    pub clipboard: Option<Arc<dyn Clipboard>>,
    /// Non-sensitive device status provider.
    pub device_status: Option<Arc<dyn DeviceStatusProvider>>,
    /// Bounded haptic feedback service.
    pub haptics: Option<Arc<dyn HapticService>>,
    /// External deep-link opener.
    pub deep_link: Option<Arc<dyn DeepLinkOpener>>,
    /// Native read-only calendar provider.
    pub calendar: Option<Arc<dyn CalendarProvider>>,
    /// Native read-only contacts provider.
    pub contacts: Option<Arc<dyn ContactsProvider>>,
    /// Screen-capture and input automation backend.
    pub computer_control: Option<Arc<dyn ComputerControl>>,
    /// Android accessibility and gesture automation.
    pub android_ui_automation: Option<Arc<dyn AndroidUiAutomation>>,
    /// OS-native secure credential store.
    pub secure_storage: Option<Arc<dyn SecureStorage>>,
    /// Mobile Linux runtime selected by the platform.
    pub mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
}

/// Aggregate of the OS handles a built engine needs.
pub trait Platform: Send + Sync {
    /// Sandboxed filesystem access.
    fn filesystem(&self) -> Arc<dyn FileSystem>;
    /// HTTP transport (provider requests, web tools).
    fn http(&self) -> Arc<dyn HttpTransport>;
    /// Wall-clock.
    fn clock(&self) -> Arc<dyn Clock>;
    /// Subprocess runner (shell tools).
    fn process(&self) -> Arc<dyn ProcessRunner>;
    /// Sandbox seam for wrapping subprocess commands.
    fn sandbox(&self) -> Arc<dyn Sandbox>;
    /// Git worktree manager.
    fn worktree(&self) -> Arc<dyn WorktreeManager>;

    // ----- mobile / automation capabilities (None unless provided) ---------

    /// Optional typed device handles. Platforms can continue to override any
    /// individual capability directly; desktop need not provide this aggregate.
    fn device_services(&self) -> Option<&DeviceServices> {
        None
    }

    /// Native camera + photo library, if the platform has one.
    fn camera(&self) -> Option<Arc<dyn CameraControl>> {
        self.device_services()
            .and_then(|services| services.camera.clone())
    }
    /// Device-local unified audio service, if the platform has audio support.
    fn audio_service(&self) -> Option<Arc<dyn AudioService>> {
        self.device_services()
            .and_then(|services| services.audio.clone())
    }
    /// Native one-shot location provider, if the platform has one.
    fn location(&self) -> Option<Arc<dyn LocationProvider>> {
        self.device_services()
            .and_then(|services| services.location.clone())
    }
    /// Native share sheet, if the platform has one.
    fn share(&self) -> Option<Arc<dyn SharingService>> {
        self.device_services()
            .and_then(|services| services.share.clone())
    }
    /// Native system notifications (engine-driven post), if available.
    fn notifications(&self) -> Option<Arc<dyn NotificationService>> {
        self.device_services()
            .and_then(|services| services.notifications.clone())
    }
    /// Native system clipboard (engine-driven read/write), if available.
    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        self.device_services()
            .and_then(|services| services.clipboard.clone())
    }
    /// Non-sensitive device status, if the platform exposes it.
    fn device_status(&self) -> Option<Arc<dyn DeviceStatusProvider>> {
        self.device_services()
            .and_then(|services| services.device_status.clone())
    }
    /// Bounded haptic feedback, if the platform exposes it.
    fn haptics(&self) -> Option<Arc<dyn HapticService>> {
        self.device_services()
            .and_then(|services| services.haptics.clone())
    }
    /// External deep-link opener, if the platform exposes it.
    fn deep_link(&self) -> Option<Arc<dyn DeepLinkOpener>> {
        self.device_services()
            .and_then(|services| services.deep_link.clone())
    }
    /// Native read-only calendar provider, if available.
    fn calendar(&self) -> Option<Arc<dyn CalendarProvider>> {
        self.device_services()
            .and_then(|services| services.calendar.clone())
    }
    /// Native read-only contacts provider, if available.
    fn contacts(&self) -> Option<Arc<dyn ContactsProvider>> {
        self.device_services()
            .and_then(|services| services.contacts.clone())
    }
    /// Screen-capture + input automation backend, if available.
    fn computer_control(&self) -> Option<Arc<dyn ComputerControl>> {
        self.device_services()
            .and_then(|services| services.computer_control.clone())
    }
    /// Android-native accessibility + gesture automation, if this is a Direct
    /// Android build with a live host bridge. Desktop/iOS keep the default.
    fn android_ui_automation(&self) -> Option<Arc<dyn AndroidUiAutomation>> {
        self.device_services()
            .and_then(|services| services.android_ui_automation.clone())
    }
    /// OS-native secure credential store (iOS Keychain / Android Keystore), if
    /// the platform provides one. When `None`, the composition root falls back to
    /// a non-persisting development stub — so OAuth `/login` (which must persist
    /// tokens) is gated off. A device platform injects a real, encrypted store
    /// here so the engine's `CredentialManager` persists secrets to the OS vault
    /// instead of the plaintext fallback.
    fn secure_storage(&self) -> Option<Arc<dyn SecureStorage>> {
        self.device_services()
            .and_then(|services| services.secure_storage.clone())
    }
    /// Mobile-only Linux userspace runtime (Android PRoot / iOS iSH bridge),
    /// when the platform wires one. Desktop platforms keep the default `None`,
    /// preserving the existing execution stack unchanged.
    fn mobile_linux(&self) -> Option<Arc<dyn MobileLinuxRuntime>> {
        self.device_services()
            .and_then(|services| services.mobile_linux.clone())
    }
}
