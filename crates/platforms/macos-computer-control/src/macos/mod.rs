//! Real macOS `ComputerControl` implementation.
//!
//! Backed entirely by pure-Rust crates rather than a bundled Swift module:
//! `enigo` (mouse/keyboard), `xcap` (screen capture — `ScreenCaptureKit`
//! under the hood), `arboard` (clipboard, functionally the same `pbcopy`/
//! `pbpaste` round trip claude-code's own executor uses), and
//! `objc2-app-kit` (`NSWorkspace`/`NSRunningApplication` for app
//! enumeration, frontmost detection, and hide/unhide).

mod apps;
mod keymap;
mod tcc;

use async_trait::async_trait;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use lingxi_core::host::computer_control::{
    AppInfo, ComputerControl, ComputerError, DisplayInfo, Screenshot,
};
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
use objc2_foundation::NSString;

fn encode_png(img: image::RgbaImage) -> Result<Vec<u8>, ComputerError> {
    let (width, height) = (img.width(), img.height());
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|e| ComputerError::Other(format!("png encode failed: {e}")))?;
    let _ = (width, height); // dims are read back from the encoded image by callers via Screenshot fields
    Ok(bytes)
}

fn primary_monitor() -> Result<xcap::Monitor, ComputerError> {
    let monitors = xcap::Monitor::all()
        .map_err(|e| ComputerError::Other(format!("listing displays failed: {e}")))?;
    monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| xcap::Monitor::all().ok().and_then(|v| v.into_iter().next()))
        .ok_or_else(|| ComputerError::Other("no display found".into()))
}

/// The monitor `screenshot`/`zoom` should capture: whichever id
/// `select_display` last pinned, or the primary display when nothing (or
/// "auto") is pinned. A pin that no longer matches a connected display
/// (unplugged since `select_display`) is a hard error rather than a silent
/// fallback — the model asked for a specific screen, and it would rather
/// hear "gone" than see a different one without knowing.
fn target_monitor(pinned: Option<u32>) -> Result<xcap::Monitor, ComputerError> {
    let Some(id) = pinned else {
        return primary_monitor();
    };
    xcap::Monitor::all()
        .map_err(|e| ComputerError::Other(format!("listing displays failed: {e}")))?
        .into_iter()
        .find(|m| m.id().ok() == Some(id))
        .ok_or_else(|| ComputerError::Other(format!("display {id} is no longer connected")))
}

fn monitor_error(e: &xcap::XCapError) -> ComputerError {
    ComputerError::Other(format!("display error: {e}"))
}

/// Screen-pixel coordinate → enigo's signed API. Real displays never
/// approach `i32::MAX` pixels wide, so the wrap this cast could theoretically
/// produce never happens in practice.
#[allow(clippy::cast_possible_wrap)]
fn px(v: u32) -> i32 {
    v as i32
}

/// enigo's signed cursor location → our unsigned coordinate space. Clamped to
/// 0 first, so this never actually loses a sign — the cursor is always
/// on-screen (non-negative) when this is called.
#[allow(clippy::cast_sign_loss)]
fn unpx(v: i32) -> u32 {
    v.max(0) as u32
}

/// Real macOS automation backend.
///
/// Holds only the `select_display` pin (`Mutex<Option<u32>>` — cheap,
/// `Send + Sync`). Everything else is constructed fresh per call: `Enigo`
/// wraps a raw `CGEventSource` handle that is neither `Send` nor `Sync`, so
/// it can't live as a field on a type shared behind `Arc<dyn
/// ComputerControl>` (the trait requires `Send + Sync`). Each call
/// constructs, uses, and drops its own `Enigo` entirely within one
/// synchronous closure — never held across an `.await` — so the type never
/// needs to cross a thread boundary.
pub struct MacosComputerControl {
    selected_display: std::sync::Mutex<Option<u32>>,
}

impl Default for MacosComputerControl {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosComputerControl {
    /// Construct the backend. Cheap — no native handle is opened until an
    /// actual action runs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            selected_display: std::sync::Mutex::new(None),
        }
    }

    /// The currently pinned display id, if `select_display` has pinned one.
    fn pinned_display(&self) -> Option<u32> {
        *self
            .selected_display
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The pinned display's origin `(x, y)` in the global desktop coordinate
    /// space enigo's `Coordinate::Abs` operates in, or `(0, 0)` for
    /// automatic/primary selection — macOS defines the primary display's
    /// origin as `(0, 0)`, so "no translation" is already exactly correct
    /// there. Every pixel-coordinate action below adds this offset before
    /// handing coordinates to enigo, and [`Self::cursor_position`] subtracts
    /// it back out, so a caller's coordinates always stay relative to
    /// whichever display `screenshot`/`zoom` are currently capturing —
    /// without this a click computed from a secondary display's screenshot
    /// would land on the primary display instead.
    fn target_origin(&self) -> Result<(i32, i32), ComputerError> {
        let Some(id) = self.pinned_display() else {
            return Ok((0, 0));
        };
        let monitor = xcap::Monitor::all()
            .map_err(|e| ComputerError::Other(format!("listing displays failed: {e}")))?
            .into_iter()
            .find(|m| m.id().ok() == Some(id))
            .ok_or_else(|| ComputerError::Other(format!("display {id} is no longer connected")))?;
        Ok((
            monitor.x().map_err(|e| monitor_error(&e))?,
            monitor.y().map_err(|e| monitor_error(&e))?,
        ))
    }

    // `&self` is unused (Enigo is constructed fresh per call, see the struct
    // doc), but kept for a consistent `self.with_enigo(...)` call-site shape
    // alongside every other method here.
    #[allow(clippy::unused_self)]
    fn with_enigo<T>(
        &self,
        f: impl FnOnce(&mut Enigo) -> enigo::InputResult<T>,
    ) -> Result<T, ComputerError> {
        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| ComputerError::Other(format!("enigo init failed: {e}")))?;
        f(&mut enigo).map_err(|e| ComputerError::Other(format!("input error: {e}")))
    }

    /// Press `chord.modifiers` in order, click `chord.main`, release
    /// modifiers in reverse — matches `withModifiers`/`key()` semantics.
    ///
    /// Tracks which modifiers actually landed (`pressed`) so a mid-press
    /// failure still releases everything that WAS pressed, not just the ones
    /// before the failure point — otherwise a transient press error on e.g.
    /// the 2nd of 3 modifiers would leave the 1st stuck held on the real
    /// keyboard forever.
    fn press_chord(enigo: &mut Enigo, chord: &keymap::Chord) -> enigo::InputResult<()> {
        let mut pressed = Vec::with_capacity(chord.modifiers.len());
        let press_result = (|| {
            for m in &chord.modifiers {
                enigo.key(*m, Direction::Press)?;
                pressed.push(*m);
            }
            enigo.key(chord.main, Direction::Click)
        })();
        for m in pressed.iter().rev() {
            // Best-effort release — a release failure must not mask the
            // original error, matching `releasePressed`'s swallow-on-throw.
            let _ = enigo.key(*m, Direction::Release);
        }
        press_result
    }
}

fn running_app_by_bundle_id(bundle_id: &str) -> Option<objc2::rc::Retained<NSRunningApplication>> {
    let ns_id = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&ns_id);
    apps.iter().next()
}

fn app_info_from(app: &NSRunningApplication) -> Option<AppInfo> {
    let bundle_id = app.bundleIdentifier()?.to_string();
    let display_name = app
        .localizedName()
        .map_or_else(|| bundle_id.clone(), |s| s.to_string());
    Some(AppInfo {
        bundle_id,
        display_name,
    })
}

#[async_trait]
impl ComputerControl for MacosComputerControl {
    async fn screenshot(&self) -> Result<Screenshot, ComputerError> {
        let monitor = target_monitor(self.pinned_display())?;
        let img = monitor.capture_image().map_err(|e| monitor_error(&e))?;
        let (width, height) = (img.width(), img.height());
        let png_bytes = encode_png(img)?;
        Ok(Screenshot {
            width,
            height,
            png_bytes,
        })
    }

    async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
        let monitor = target_monitor(self.pinned_display())?;
        Ok((
            monitor.width().map_err(|e| monitor_error(&e))?,
            monitor.height().map_err(|e| monitor_error(&e))?,
        ))
    }

    async fn mouse_move(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs))
    }

    async fn left_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs)?;
            e.button(Button::Left, Direction::Click)
        })
    }

    async fn right_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs)?;
            e.button(Button::Right, Direction::Click)
        })
    }

    async fn double_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs)?;
            e.button(Button::Left, Direction::Click)?;
            e.button(Button::Left, Direction::Click)
        })
    }

    async fn type_text(&self, text: String) -> Result<(), ComputerError> {
        self.with_enigo(|e| e.text(&text))
    }

    async fn key(&self, key: String) -> Result<(), ComputerError> {
        let chord = keymap::parse_chord(&key)
            .ok_or_else(|| ComputerError::Other(format!("unrecognized key name: {key}")))?;
        self.with_enigo(|e| Self::press_chord(e, &chord))
    }

    async fn scroll(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs)?;
            if dy != 0 {
                e.scroll(dy, Axis::Vertical)?;
            }
            if dx != 0 {
                e.scroll(dx, Axis::Horizontal)?;
            }
            Ok(())
        })
    }

    async fn middle_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs)?;
            e.button(Button::Middle, Direction::Click)
        })
    }

    async fn triple_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            e.move_mouse(px(x) + ox, px(y) + oy, Coordinate::Abs)?;
            e.button(Button::Left, Direction::Click)?;
            e.button(Button::Left, Direction::Click)?;
            e.button(Button::Left, Direction::Click)
        })
    }

    async fn drag(&self, from: Option<(u32, u32)>, to: (u32, u32)) -> Result<(), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| {
            if let Some((fx, fy)) = from {
                e.move_mouse(px(fx) + ox, px(fy) + oy, Coordinate::Abs)?;
            }
            e.button(Button::Left, Direction::Press)?;
            let result = e.move_mouse(px(to.0) + ox, px(to.1) + oy, Coordinate::Abs);
            // Always release, even if the move failed — otherwise the button
            // stays stuck-down (matches executor.ts's drag `finally`).
            let release = e.button(Button::Left, Direction::Release);
            result.and(release)
        })
    }

    async fn mouse_down(&self) -> Result<(), ComputerError> {
        self.with_enigo(|e| e.button(Button::Left, Direction::Press))
    }

    async fn mouse_up(&self) -> Result<(), ComputerError> {
        self.with_enigo(|e| e.button(Button::Left, Direction::Release))
    }

    async fn cursor_position(&self) -> Result<(u32, u32), ComputerError> {
        let (ox, oy) = self.target_origin()?;
        self.with_enigo(|e| e.location())
            .map(|(x, y)| (unpx(x - ox), unpx(y - oy)))
    }

    async fn hold_key(&self, key: String, duration_ms: u64) -> Result<(), ComputerError> {
        let chord = keymap::parse_chord(&key)
            .ok_or_else(|| ComputerError::Other(format!("unrecognized key name: {key}")))?;
        let mut all = chord.modifiers.clone();
        all.push(chord.main);
        // Press, tracking what actually landed — a mid-press failure (e.g. the
        // 2nd of 3 keys) must still release the ones that DID press, not skip
        // straight to returning the error and leaving them stuck held.
        let press_result = self.with_enigo(|e| {
            let mut pressed = Vec::with_capacity(all.len());
            let result = (|| {
                for k in &all {
                    e.key(*k, Direction::Press)?;
                    pressed.push(*k);
                }
                Ok(())
            })();
            if result.is_err() {
                for k in pressed.iter().rev() {
                    let _ = e.key(*k, Direction::Release);
                }
            }
            result
        });
        press_result?;
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
        // The release MUST run even if constructing a fresh `Enigo` for it
        // transiently fails: `with_enigo` bails via `?` on
        // `Enigo::new()`'s own error BEFORE ever calling the closure that
        // does the releasing — a single failed construction here would skip
        // the release loop entirely and leave every key in `all` physically
        // held with no second chance (unlike the press-phase failure above,
        // which at least runs its own compensating release inside the SAME
        // `with_enigo` call). Retry construction a few times with a short
        // backoff — a construction failure moments after the press phase
        // just succeeded is far more likely to be transient than a real,
        // sustained permission loss.
        let mut last_err = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            match self.with_enigo(|e| {
                for k in all.iter().rev() {
                    let _ = e.key(*k, Direction::Release);
                }
                Ok(())
            }) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(ComputerError::Other(format!(
            "hold_key release failed after 3 attempts — key(s) may still be physically held: {}",
            last_err.expect("loop always runs at least once")
        )))
    }

    async fn zoom(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Screenshot, ComputerError> {
        // xcap 0.5's `Monitor` has no `capture_region` — crop the full-display
        // capture instead (still a single native capture call, just cropped
        // client-side rather than by the OS).
        let monitor = target_monitor(self.pinned_display())?;
        let full = monitor.capture_image().map_err(|e| monitor_error(&e))?;
        let (full_w, full_h) = (full.width(), full.height());
        let x = x.min(full_w);
        let y = y.min(full_h);
        let w = w.min(full_w.saturating_sub(x));
        let h = h.min(full_h.saturating_sub(y));
        let cropped = image::imageops::crop_imm(&full, x, y, w, h).to_image();
        let (width, height) = (cropped.width(), cropped.height());
        let png_bytes = encode_png(cropped)?;
        Ok(Screenshot {
            width,
            height,
            png_bytes,
        })
    }

    async fn read_clipboard(&self) -> Result<String, ComputerError> {
        let mut cb = arboard::Clipboard::new()
            .map_err(|e| ComputerError::Other(format!("clipboard init failed: {e}")))?;
        cb.get_text()
            .map_err(|e| ComputerError::Other(format!("clipboard read failed: {e}")))
    }

    async fn write_clipboard(&self, text: String) -> Result<(), ComputerError> {
        let mut cb = arboard::Clipboard::new()
            .map_err(|e| ComputerError::Other(format!("clipboard init failed: {e}")))?;
        cb.set_text(text)
            .map_err(|e| ComputerError::Other(format!("clipboard write failed: {e}")))
    }

    async fn open_application(&self, name_or_bundle_id: String) -> Result<(), ComputerError> {
        // Resolve a display name to a bundle id via the installed-apps scan;
        // an argument that's already a bundle id (contains a dot, matches an
        // installed app verbatim) is passed straight through.
        let installed = apps::list_installed_apps();
        let bundle_id = installed
            .iter()
            .find(|a| a.bundle_id == name_or_bundle_id)
            .or_else(|| {
                installed
                    .iter()
                    .find(|a| a.display_name.eq_ignore_ascii_case(&name_or_bundle_id))
            })
            .map(|a| a.bundle_id.clone())
            .unwrap_or(name_or_bundle_id);

        // `open -b <bundle-id>` is the standard, supported macOS CLI entry
        // point for launching-by-identifier — simpler and more robust than
        // bridging NSWorkspace's async `openApplicationAtURL:` completion
        // handler into an `async fn`.
        let status = std::process::Command::new("open")
            .arg("-b")
            .arg(&bundle_id)
            .status()
            .map_err(|e| ComputerError::Other(format!("`open -b {bundle_id}` failed: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(ComputerError::Other(format!(
                "`open -b {bundle_id}` exited with {status}"
            )))
        }
    }

    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>, ComputerError> {
        Ok(apps::list_installed_apps())
    }

    async fn list_running_apps(&self) -> Result<Vec<AppInfo>, ComputerError> {
        let workspace = NSWorkspace::sharedWorkspace();
        let running = workspace.runningApplications();
        let mut out = Vec::new();
        for app in &*running {
            if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                continue; // foreground/dock-visible apps only, matching upstream's intent
            }
            if let Some(info) = app_info_from(&app) {
                out.push(info);
            }
        }
        Ok(out)
    }

    async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
        let workspace = NSWorkspace::sharedWorkspace();
        Ok(workspace
            .frontmostApplication()
            .and_then(|app| app_info_from(&app)))
    }

    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, ComputerError> {
        let monitors = xcap::Monitor::all().map_err(|e| monitor_error(&e))?;
        let mut out = Vec::with_capacity(monitors.len());
        for m in &monitors {
            out.push(DisplayInfo {
                id: m.id().map_err(|e| monitor_error(&e))?,
                name: m.name().unwrap_or_else(|_| "Unknown Display".into()),
                width: m.width().map_err(|e| monitor_error(&e))?,
                height: m.height().map_err(|e| monitor_error(&e))?,
                is_primary: m.is_primary().unwrap_or(false),
            });
        }
        Ok(out)
    }

    async fn select_display(&self, id: Option<u32>) -> Result<(), ComputerError> {
        if let Some(id) = id {
            let monitors = xcap::Monitor::all()
                .map_err(|e| ComputerError::Other(format!("listing displays failed: {e}")))?;
            if !monitors.iter().any(|m| m.id().ok() == Some(id)) {
                return Err(ComputerError::Other(format!("display {id} not found")));
            }
        }
        *self
            .selected_display
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = id;
        Ok(())
    }

    async fn hide_app(&self, bundle_id: &str) -> Result<(), ComputerError> {
        match running_app_by_bundle_id(bundle_id) {
            Some(app) => {
                app.hide();
                Ok(())
            }
            None => Ok(()), // not running — nothing to hide, not an error
        }
    }

    async fn unhide_apps(&self, bundle_ids: &[String]) -> Result<(), ComputerError> {
        for id in bundle_ids {
            if let Some(app) = running_app_by_bundle_id(id) {
                app.unhide();
            }
        }
        Ok(())
    }

    async fn check_os_permissions(&self) -> Option<(bool, bool)> {
        Some(tcc::check_os_permissions())
    }
}
