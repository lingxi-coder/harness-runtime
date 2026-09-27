//! `ComputerControl` — screen-capture + input-automation seam (M8-P10).
//!
//! Implemented natively per platform: a desktop automation backend, or
//! Swift/Kotlin via `UniFFI` on mobile (the callback-interface annotations land
//! in P12). The `tool-computer-use` tool (P11b) routes through this trait, so
//! Rust drives the host's screen + input without knowing the backend.
//!
//! Contract alignment (parity with claude-code's `@ant/computer-use-mcp` /
//! `executor.ts`): every action the real product exposes has a method here.
//! Actions a given backend can't perform default to
//! [`ComputerError::Unsupported`] via a provided trait-method body, so a
//! partial implementor (or a future mobile `UniFFI` impl) only needs to
//! override what it actually supports — adding a method here never breaks an
//! existing implementor.

use async_trait::async_trait;
use thiserror::Error;

/// A captured screen image (PNG-encoded) plus its pixel dimensions. Reused for
/// both `screenshot` (full display) and `zoom` (region capture) — parity with
/// upstream's shared `ScreenshotResult` shape.
#[derive(Debug, Clone)]
pub struct Screenshot {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// PNG-encoded image bytes.
    pub png_bytes: Vec<u8>,
}

/// One connected display (`executor.ts` `listDisplays`/`DisplayGeometry`).
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// Backend-assigned display id (stable for the process lifetime).
    pub id: u32,
    /// Human-readable name shown in `switch_display` guidance (e.g. "Built-in
    /// Retina Display").
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this is the OS-designated primary/main display.
    pub is_primary: bool,
}

/// One installed or running application (`executor.ts`
/// `InstalledApp`/`RunningApp`/`FrontmostApp`).
#[derive(Debug, Clone)]
pub struct AppInfo {
    /// Bundle identifier (e.g. `com.tinyspeck.slackmacgap`) — the stable,
    /// locale-invariant identity used by the allowlist.
    pub bundle_id: String,
    /// Localized display name (e.g. "Slack").
    pub display_name: String,
}

/// Failure modes for [`ComputerControl`] operations.
#[derive(Debug, Clone, Error)]
pub enum ComputerError {
    /// The OS denied screen-recording / accessibility permission.
    #[error("computer-control permission denied: {0}")]
    PermissionDenied(String),
    /// The operation is not supported on this platform/backend.
    #[error("computer-control unsupported: {0}")]
    Unsupported(String),
    /// Any other backend failure.
    #[error("computer-control error: {0}")]
    Other(String),
}

/// Screen capture + mouse/keyboard automation.
///
/// Coordinates are in screen pixels with the origin at the top-left.
#[async_trait]
pub trait ComputerControl: Send + Sync {
    /// Capture the current (primary or last-targeted) display.
    async fn screenshot(&self) -> Result<Screenshot, ComputerError>;
    /// Return the primary display size in pixels `(width, height)`.
    async fn display_size(&self) -> Result<(u32, u32), ComputerError>;
    /// Move the cursor to `(x, y)`.
    async fn mouse_move(&self, x: u32, y: u32) -> Result<(), ComputerError>;
    /// Left-click at `(x, y)`.
    async fn left_click(&self, x: u32, y: u32) -> Result<(), ComputerError>;
    /// Right-click at `(x, y)`.
    async fn right_click(&self, x: u32, y: u32) -> Result<(), ComputerError>;
    /// Double-click at `(x, y)`.
    async fn double_click(&self, x: u32, y: u32) -> Result<(), ComputerError>;
    /// Type a string of text at the current focus.
    async fn type_text(&self, text: String) -> Result<(), ComputerError>;
    /// Press a named key (e.g. `"Return"`, `"cmd+a"`).
    async fn key(&self, key: String) -> Result<(), ComputerError>;
    /// Scroll by `(dx, dy)` ticks at `(x, y)`.
    async fn scroll(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError>;

    // ── M8-P11b extensions — full action-set parity ────────────────────────

    /// Middle-click (scroll-wheel click) at `(x, y)`.
    async fn middle_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let _ = (x, y);
        Err(ComputerError::Unsupported("middle_click".into()))
    }
    /// Triple-click at `(x, y)` (selects a line in most text editors).
    async fn triple_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let _ = (x, y);
        Err(ComputerError::Unsupported("triple_click".into()))
    }
    /// Press, move to `to`, and release. `from = None` drags from the current
    /// cursor position (parity with `left_click_drag`'s optional
    /// `start_coordinate`).
    async fn drag(&self, from: Option<(u32, u32)>, to: (u32, u32)) -> Result<(), ComputerError> {
        let _ = (from, to);
        Err(ComputerError::Unsupported("left_click_drag".into()))
    }
    /// Press the left mouse button at the current cursor position and leave
    /// it held (pairs with [`Self::mouse_up`]).
    async fn mouse_down(&self) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported("left_mouse_down".into()))
    }
    /// Release the left mouse button at the current cursor position.
    async fn mouse_up(&self) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported("left_mouse_up".into()))
    }
    /// Current cursor position `(x, y)`.
    async fn cursor_position(&self) -> Result<(u32, u32), ComputerError> {
        Err(ComputerError::Unsupported("cursor_position".into()))
    }
    /// Press and hold `key` for `duration_ms`, then release
    /// (parity with `holdKey`).
    async fn hold_key(&self, key: String, duration_ms: u64) -> Result<(), ComputerError> {
        let _ = (key, duration_ms);
        Err(ComputerError::Unsupported("hold_key".into()))
    }
    /// Capture a higher-resolution region `(x, y, w, h)` of the most recent
    /// screenshot's coordinate space (parity with `zoom`/`captureRegion`).
    async fn zoom(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Screenshot, ComputerError> {
        let _ = (x, y, w, h);
        Err(ComputerError::Unsupported("zoom".into()))
    }
    /// Read the system clipboard as text.
    async fn read_clipboard(&self) -> Result<String, ComputerError> {
        Err(ComputerError::Unsupported("read_clipboard".into()))
    }
    /// Write text to the system clipboard.
    async fn write_clipboard(&self, text: String) -> Result<(), ComputerError> {
        let _ = text;
        Err(ComputerError::Unsupported("write_clipboard".into()))
    }
    /// Launch/activate an application by display name or bundle id.
    async fn open_application(&self, name_or_bundle_id: String) -> Result<(), ComputerError> {
        let _ = name_or_bundle_id;
        Err(ComputerError::Unsupported("open_application".into()))
    }
    /// Enumerate installed applications (for `request_access`'s app picker).
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>, ComputerError> {
        Err(ComputerError::Unsupported("list_installed_apps".into()))
    }
    /// Enumerate currently running (foreground) applications.
    async fn list_running_apps(&self) -> Result<Vec<AppInfo>, ComputerError> {
        Err(ComputerError::Unsupported("list_running_apps".into()))
    }
    /// The currently frontmost application, if any.
    async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
        Err(ComputerError::Unsupported("frontmost_app".into()))
    }
    /// Enumerate connected displays (parity with `listDisplays`).
    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        Err(ComputerError::Unsupported("list_displays".into()))
    }
    /// Pin subsequent [`Self::screenshot`]/[`Self::zoom`] captures to the
    /// display with this id (one returned by [`Self::list_displays`]), or
    /// `None` to return to automatic (primary-display) selection. Parity
    /// with `switch_display`: the caller resolves a human-readable monitor
    /// name to an id via `list_displays` first, then pins it here — a
    /// single-display backend (or one with no display-switching concept,
    /// e.g. mobile) can leave this at its default.
    async fn select_display(&self, id: Option<u32>) -> Result<(), ComputerError> {
        let _ = id;
        Err(ComputerError::Unsupported("select_display".into()))
    }
    /// Hide one application's windows (parity with `prepareForAction`'s
    /// pre-action hide sequence).
    async fn hide_app(&self, bundle_id: &str) -> Result<(), ComputerError> {
        let _ = bundle_id;
        Err(ComputerError::Unsupported("hide_app".into()))
    }
    /// Unhide applications hidden during the turn (parity with
    /// `unhideComputerUseApps`, fired at turn-end cleanup).
    async fn unhide_apps(&self, bundle_ids: &[String]) -> Result<(), ComputerError> {
        let _ = bundle_ids;
        Err(ComputerError::Unsupported("unhide_apps".into()))
    }
    /// Whether the OS has granted the permissions automation needs
    /// (Accessibility + Screen Recording on macOS). `None` = the backend
    /// can't introspect this and the caller should just try the action.
    async fn check_os_permissions(&self) -> Option<(bool, bool)> {
        None
    }
}
