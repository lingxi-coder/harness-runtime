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

use crate::android_ui::AndroidUiAutomation;
use crate::audio::AudioService;
use crate::calendar::CalendarProvider;
use crate::camera::CameraControl;
use crate::clipboard::Clipboard;
use crate::clock::Clock;
use crate::computer_control::ComputerControl;
use crate::contacts::ContactsProvider;
use crate::deep_link::DeepLinkOpener;
use crate::device_status::DeviceStatusProvider;
use crate::filesystem::FileSystem;
use crate::haptics::HapticService;
use crate::http::HttpTransport;
use crate::location::LocationProvider;
use crate::notification::NotificationService;
use crate::process::ProcessRunner;
use crate::sandbox::Sandbox;
use crate::secure_storage::SecureStorage;
use crate::share::SharingService;
use crate::worktree::WorktreeManager;
use mobile_linux_api::MobileLinuxRuntime;
use std::sync::Arc;

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

    /// Native camera + photo library, if the platform has one.
    fn camera(&self) -> Option<Arc<dyn CameraControl>> {
        None
    }
    /// Device-local unified audio service, if the platform has audio support.
    fn audio_service(&self) -> Option<Arc<dyn AudioService>> {
        None
    }
    /// Native one-shot location provider, if the platform has one.
    fn location(&self) -> Option<Arc<dyn LocationProvider>> {
        None
    }
    /// Native share sheet, if the platform has one.
    fn share(&self) -> Option<Arc<dyn SharingService>> {
        None
    }
    /// Native system notifications (engine-driven post), if available.
    fn notifications(&self) -> Option<Arc<dyn NotificationService>> {
        None
    }
    /// Native system clipboard (engine-driven read/write), if available.
    fn clipboard(&self) -> Option<Arc<dyn Clipboard>> {
        None
    }
    /// Non-sensitive device status, if the platform exposes it.
    fn device_status(&self) -> Option<Arc<dyn DeviceStatusProvider>> {
        None
    }
    /// Bounded haptic feedback, if the platform exposes it.
    fn haptics(&self) -> Option<Arc<dyn HapticService>> {
        None
    }
    /// External deep-link opener, if the platform exposes it.
    fn deep_link(&self) -> Option<Arc<dyn DeepLinkOpener>> {
        None
    }
    /// Native read-only calendar provider, if available.
    fn calendar(&self) -> Option<Arc<dyn CalendarProvider>> {
        None
    }
    /// Native read-only contacts provider, if available.
    fn contacts(&self) -> Option<Arc<dyn ContactsProvider>> {
        None
    }
    /// Screen-capture + input automation backend, if available.
    fn computer_control(&self) -> Option<Arc<dyn ComputerControl>> {
        None
    }
    /// Android-native accessibility + gesture automation, if this is a Direct
    /// Android build with a live host bridge. Desktop/iOS keep the default.
    fn android_ui_automation(&self) -> Option<Arc<dyn AndroidUiAutomation>> {
        None
    }
    /// OS-native secure credential store (iOS Keychain / Android Keystore), if
    /// the platform provides one. When `None`, the composition root falls back to
    /// a non-persisting development stub — so OAuth `/login` (which must persist
    /// tokens) is gated off. A device platform injects a real, encrypted store
    /// here so the engine's `CredentialManager` persists secrets to the OS vault
    /// instead of the plaintext fallback.
    fn secure_storage(&self) -> Option<Arc<dyn SecureStorage>> {
        None
    }
    /// Mobile-only Linux userspace runtime (Android PRoot / iOS iSH bridge),
    /// when the platform wires one. Desktop platforms keep the default `None`,
    /// preserving the existing execution stack unchanged.
    fn mobile_linux(&self) -> Option<Arc<dyn MobileLinuxRuntime>> {
        None
    }
}
