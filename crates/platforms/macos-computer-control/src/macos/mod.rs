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

pub struct MacosComputerControl {
    selected_display: std::sync::Mutex<Option<u32>>,
    input_worker: std::sync::mpsc::Sender<InputJob>,
    owned_mouse: std::sync::Arc<std::sync::atomic::AtomicU8>,
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
        let (sender, receiver) = std::sync::mpsc::channel::<InputJob>();
        let owned_mouse = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let worker_mouse = owned_mouse.clone();
        std::thread::Builder::new()
            .name("computer-input".into())
            .spawn(move || {
                let mut enigo = None;
                while let Ok(job) = receiver.recv() {
                    job(&mut enigo);
                }
                if let Some(backend) = enigo.as_mut() {
                    let _ = release_buttons_with(&worker_mouse, |button| {
                        send_button(backend, button, Direction::Release)
                    });
                }
                // Enigo releases any remaining owned keys on its owning thread.
            })
            .expect("computer input worker thread");
        Self {
            selected_display: std::sync::Mutex::new(None),
            input_worker: sender,
            owned_mouse,
        }
    }

    /// The currently pinned display id, if `select_display` has pinned one.
    fn pinned_display(&self) -> Option<u32> {
        *self
            .selected_display
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn with_enigo<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Enigo) -> enigo::InputResult<T> + Send + 'static,
    ) -> Result<T, ComputerError> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.input_worker
            .send(Box::new(move |backend| {
                let result = (|| {
                    if backend.is_none() {
                        *backend = Some(Enigo::new(&Settings::default()).map_err(|e| {
                            ComputerError::Other(format!("enigo init failed: {e}"))
                        })?);
                    }
                    f(backend.as_mut().expect("initialized"))
                        .map_err(|e| ComputerError::Other(format!("input error: {e}")))
                })();
                let _ = sender.send(result);
            }))
            .map_err(|_| ComputerError::Other("computer input worker stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| ComputerError::Other("computer input worker stopped".into()))?
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
        let held = enigo.held().0;
        Self::press_chord_with(&held, chord, |key, direction| enigo.key(key, direction))
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
    fn geometry(&self) -> Result<ComputerFrameGeometry, ComputerError> {
        let monitor = target_monitor(self.pinned_display())?;
        monitor_geometry(&monitor)
    }
    fn target_point(&self, x: u32, y: u32) -> Result<(i32, i32), ComputerError> {
        let geometry = self.geometry()?;
        pixel_to_global(&geometry, x, y)
    }

    fn press_chord_with(
        held: &[enigo::Key],
        chord: &keymap::Chord,
        mut send: impl FnMut(enigo::Key, Direction) -> enigo::InputResult<()>,
    ) -> enigo::InputResult<()> {
        let mut pressed = Vec::with_capacity(chord.modifiers.len());
        let press_result = (|| {
            for m in &chord.modifiers {
                if !held.contains(m) {
                    send(*m, Direction::Press)?;
                    pressed.push(*m);
                }
            }
            send(chord.main, Direction::Press)?;
            if !held.contains(&chord.main) {
                pressed.push(chord.main);
            }
            Ok(())
        })();
        let mut release_result = Ok(());
        for m in pressed.iter().rev() {
            if let Err(error) = send(*m, Direction::Release) {
                release_result = Err(error);
            }
        }
        press_result.and(release_result)
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
        let geometry = self.geometry()?;
        Ok((geometry.pixel_width, geometry.pixel_height))
    }

    async fn mouse_move(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (gx, gy) = self.target_point(x, y)?;
        self.with_enigo(move |e| e.move_mouse(gx, gy, Coordinate::Abs))
    }

    async fn left_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.mouse_click(x, y, "left").await
    }
    async fn right_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.mouse_click(x, y, "right").await
    }
    async fn double_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (x, y) = self.target_point(x, y)?;
        let owned = self.owned_mouse.clone();
        self.with_enigo(move |e| {
            e.move_mouse(x, y, Coordinate::Abs)?;
            click_button_with(&owned, 0, 2, |direction| send_button(e, 0, direction))
        })
    }
    async fn type_text(&self, text: String) -> Result<(), ComputerError> {
        self.with_enigo(move |e| {
            type_text_with(&text, |input| match input {
                TextInput::Text(text) => e.text(text),
                TextInput::Tab => Self::press_chord(
                    e,
                    &keymap::Chord {
                        main: enigo::Key::Tab,
                        modifiers: vec![],
                    },
                ),
            })
        })
    }

    async fn key(&self, key: String) -> Result<(), ComputerError> {
        let chord = keymap::parse_chord(&key)
            .ok_or_else(|| ComputerError::Other(format!("unrecognized key name: {key}")))?;
        self.with_enigo(move |e| Self::press_chord(e, &chord))
    }

    async fn scroll(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        let (gx, gy) = self.target_point(x, y)?;
        self.with_enigo(move |e| {
            e.move_mouse(gx, gy, Coordinate::Abs)?;
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
        self.mouse_click(x, y, "middle").await
    }
    async fn triple_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        let (x, y) = self.target_point(x, y)?;
        let owned = self.owned_mouse.clone();
        self.with_enigo(move |e| {
            e.move_mouse(x, y, Coordinate::Abs)?;
            click_button_with(&owned, 0, 3, |direction| send_button(e, 0, direction))
        })
    }

    async fn drag(&self, from: Option<(u32, u32)>, to: (u32, u32)) -> Result<(), ComputerError> {
        let from = from.map(|(x, y)| self.target_point(x, y)).transpose()?;
        let (tx, ty) = self.target_point(to.0, to.1)?;
        let owned = self.owned_mouse.clone();
        self.with_enigo(move |e| {
            if let Some((x, y)) = from {
                e.move_mouse(x, y, Coordinate::Abs)?;
            }
            e.button(Button::Left, Direction::Press)?;
            owned.fetch_or(1, std::sync::atomic::Ordering::AcqRel);
            let moved = e.move_mouse(tx, ty, Coordinate::Abs);
            let release = e.button(Button::Left, Direction::Release);
            if release.is_ok() {
                owned.fetch_and(!1, std::sync::atomic::Ordering::AcqRel);
            }
            moved.and(release)
        })
    }

    async fn mouse_down(&self) -> Result<(), ComputerError> {
        self.with_enigo(move |e| e.button(Button::Left, Direction::Press))?;
        self.owned_mouse
            .fetch_or(1, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }

    async fn mouse_up(&self) -> Result<(), ComputerError> {
        self.with_enigo(move |e| e.button(Button::Left, Direction::Release))?;
        self.owned_mouse
            .fetch_and(!1, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }

    async fn cursor_position(&self) -> Result<(u32, u32), ComputerError> {
        let geometry = self.geometry()?;
        let (x, y) = self.with_enigo(move |e| e.location())?;
        let x = (f64::from(x) - geometry.origin_x) * geometry.scale;
        let y = (f64::from(y) - geometry.origin_y) * geometry.scale;
        if x < 0.0
            || y < 0.0
            || x >= f64::from(geometry.pixel_width)
            || y >= f64::from(geometry.pixel_height)
        {
            return Err(ComputerError::Other(
                "cursor is outside the selected display; switch display to query its position"
                    .into(),
            ));
        }
        Ok((x.round() as u32, y.round() as u32))
    }

    async fn hold_key(&self, key: String, duration_ms: u64) -> Result<(), ComputerError> {
        let chord = keymap::parse_chord(&key)
            .ok_or_else(|| ComputerError::Other(format!("unrecognized key: {key}")))?;
        let mut keys = chord.modifiers;
        keys.push(chord.main);
        let press = keys.clone();
        self.with_enigo(move |e| {
            let mut pressed = Vec::new();
            for key in press {
                if let Err(error) = e.key(key, Direction::Press) {
                    for key in pressed.into_iter().rev() {
                        let _ = e.key(key, Direction::Release);
                    }
                    return Err(error);
                }
                pressed.push(key);
            }
            Ok(())
        })?;
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
        self.with_enigo(move |e| {
            let mut error = None;
            for key in keys.into_iter().rev() {
                if let Err(e) = e.key(key, Direction::Release) {
                    error = Some(e);
                }
            }
            error.map_or(Ok(()), Err)
        })
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
            let geometry = monitor_geometry(m)?;
            out.push(DisplayInfo {
                id: geometry.display_id,
                name: m.name().unwrap_or_else(|_| "Unknown Display".into()),
                width: geometry.pixel_width,
                height: geometry.pixel_height,
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
    fn desktop_lock_scope(&self) -> Option<std::path::PathBuf> {
        // Effective UID identifies the current user's macOS desktop. The
        // literal OS temporary root deliberately ignores HOME, TMPDIR and
        // the application's configurable data directory.
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        let uid = unsafe { geteuid() };
        Some(std::path::PathBuf::from(format!(
            "/private/tmp/lingxi-computer-desktop-{uid}"
        )))
    }
    fn capabilities(&self) -> ComputerBackendCapabilities {
        ComputerBackendCapabilities {
            held_keys: true,
            pixel_scroll: true,
            side_buttons: true,
            frame_geometry: true,
        }
    }
    async fn frame_geometry(&self) -> Result<Option<ComputerFrameGeometry>, ComputerError> {
        self.geometry().map(Some)
    }
    async fn validate_keys(&self, keys: &[String]) -> Result<(), ComputerError> {
        for key in keys {
            if keymap::parse_key_name(key).is_none() {
                return Err(ComputerError::Other(format!(
                    "unrecognized key name: {key}"
                )));
            }
        }
        Ok(())
    }
    async fn key_down(&self, key: String) -> Result<(), ComputerError> {
        let key = keymap::parse_key_name(&key)
            .ok_or_else(|| ComputerError::Other(format!("unrecognized key name: {key}")))?;
        self.with_enigo(move |e| e.key(key, Direction::Press))
    }
    async fn key_up(&self, key: String) -> Result<(), ComputerError> {
        let key = keymap::parse_key_name(&key)
            .ok_or_else(|| ComputerError::Other(format!("unrecognized key name: {key}")))?;
        self.with_enigo(move |e| e.key(key, Direction::Release))
    }
    async fn key_chord(&self, keys: Vec<String>) -> Result<(), ComputerError> {
        let parsed = keys
            .iter()
            .map(|key| {
                keymap::parse_key_name(key)
                    .ok_or_else(|| ComputerError::Other(format!("unrecognized key name: {key}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let Some((main, modifiers)) = parsed.split_last() else {
            return Err(ComputerError::Other("empty key chord".into()));
        };
        let chord = keymap::Chord {
            main: *main,
            modifiers: modifiers.to_vec(),
        };
        self.with_enigo(move |e| Self::press_chord(e, &chord))
    }
    async fn release_held_keys(&self) -> Result<(), ComputerError> {
        self.with_enigo(|enigo| {
            let (keys, raw_keys) = enigo.held();
            let mut result = Ok(());
            for key in keys {
                if let Err(error) = enigo.key(key, Direction::Release) {
                    result = Err(error);
                }
            }
            for key in raw_keys {
                if let Err(error) = enigo.raw(key, Direction::Release) {
                    result = Err(error);
                }
            }
            result
        })
    }
    async fn mouse_click(&self, x: u32, y: u32, button: &str) -> Result<(), ComputerError> {
        let button = mouse_button_id(button)?;
        let (x, y) = self.target_point(x, y)?;
        let owned = self.owned_mouse.clone();
        self.with_enigo(move |e| {
            e.move_mouse(x, y, Coordinate::Abs)?;
            click_button_with(&owned, button, 1, |direction| {
                send_button(e, button, direction)
            })
        })
    }
    async fn click_current(&self, button: &str, count: u32) -> Result<(), ComputerError> {
        let button = mouse_button_id(button)?;
        if count == 0 || count > 3 || (button >= 3 && count != 1) {
            return Err(ComputerError::Unsupported("mouse click count".into()));
        }
        let owned = self.owned_mouse.clone();
        self.with_enigo(move |e| {
            click_button_with(&owned, button, count, |direction| {
                send_button(e, button, direction)
            })
        })
    }
    async fn release_held_buttons(&self) -> Result<(), ComputerError> {
        let owned = self.owned_mouse.clone();
        self.with_enigo(move |e| {
            release_buttons_with(&owned, |button| send_button(e, button, Direction::Release))
        })
    }
    async fn scroll_current(&self, dx: i32, dy: i32, pixels: bool) -> Result<(), ComputerError> {
        if pixels {
            return post_pixel_scroll(dx, dy);
        }
        self.with_enigo(move |e| {
            if dy != 0 {
                e.scroll(dy, Axis::Vertical)?;
            }
            if dx != 0 {
                e.scroll(dx, Axis::Horizontal)?;
            }
            Ok(())
        })
    }
    async fn scroll_pixels(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        let (x, y) = self.target_point(x, y)?;
        self.with_enigo(move |e| e.move_mouse(x, y, Coordinate::Abs))?;
        post_pixel_scroll(dx, dy)
    }
}
use lingxi_core::host::computer_control::{
    AppInfo, ComputerBackendCapabilities, ComputerControl, ComputerError, ComputerFrameGeometry,
    DisplayInfo, Screenshot,
};

fn monitor_geometry(monitor: &xcap::Monitor) -> Result<ComputerFrameGeometry, ComputerError> {
    let display_id = monitor.id().map_err(|e| monitor_error(&e))?;
    let scale = f64::from(monitor.scale_factor().map_err(|e| monitor_error(&e))?);
    let origin_x = f64::from(monitor.x().map_err(|e| monitor_error(&e))?);
    let origin_y = f64::from(monitor.y().map_err(|e| monitor_error(&e))?);
    // xcap's macOS monitor bounds are global OS points, while this host
    // contract exposes capture pixels for both display listings and frames.
    let pixel_width =
        (f64::from(monitor.width().map_err(|e| monitor_error(&e))?) * scale).round() as u32;
    let pixel_height =
        (f64::from(monitor.height().map_err(|e| monitor_error(&e))?) * scale).round() as u32;
    Ok(ComputerFrameGeometry {
        display_id,
        pixel_width,
        pixel_height,
        origin_x,
        origin_y,
        scale,
        version: format!(
            "macos-v1:{display_id}:{pixel_width}:{pixel_height}:{origin_x}:{origin_y}:{scale}"
        ),
    })
}

/// Real macOS automation backend.
///
/// Native input handles live on one dedicated worker thread. This preserves
/// modifier flags and pressed inputs across asynchronous calls while keeping
/// the non-Send Enigo handle on its owning thread.
type InputJob = Box<dyn FnOnce(&mut Option<Enigo>) + Send>;

fn event_source() -> Result<core_graphics::event_source::CGEventSource, ComputerError> {
    core_graphics::event_source::CGEventSource::new(
        core_graphics::event_source::CGEventSourceStateID::HIDSystemState,
    )
    .map_err(|()| ComputerError::Other("CGEventSource creation failed".into()))
}

enum TextInput<'a> {
    Text(&'a str),
    Tab,
}

/// Keep Enigo's 20-scalar text chunks and leading-control workaround, but
/// route its physical Tab clicks through tracked Press/Release operations.
fn type_text_with<E>(
    text: &str,
    mut send: impl FnMut(TextInput<'_>) -> Result<(), E>,
) -> Result<(), E> {
    let mut starts = text
        .char_indices()
        .map(|(index, _)| index)
        .step_by(20)
        .peekable();
    while let Some(start) = starts.next() {
        let end = starts.peek().copied().unwrap_or(text.len());
        let mut chunk = &text[start..end];
        loop {
            if let Some(rest) = chunk.strip_prefix('\t') {
                send(TextInput::Tab)?;
                chunk = rest;
            } else if let Some(rest) = chunk.strip_prefix('\r') {
                send(TextInput::Text("\u{200B}\r"))?;
                chunk = rest;
            } else if let Some(rest) = chunk.strip_prefix('\n') {
                send(TextInput::Text("\u{200B}\n"))?;
                chunk = rest;
            } else {
                break;
            }
        }
        if !chunk.is_empty() {
            send(TextInput::Text(chunk))?;
        }
    }
    Ok(())
}
fn mouse_button_id(button: &str) -> Result<u8, ComputerError> {
    match button {
        "left" => Ok(0),
        "right" => Ok(1),
        "middle" => Ok(2),
        "back" => Ok(3),
        "forward" => Ok(4),
        _ => Err(ComputerError::Unsupported(format!("mouse button {button}"))),
    }
}
fn click_button_with<E>(
    owned: &std::sync::atomic::AtomicU8,
    button: u8,
    count: u32,
    mut send: impl FnMut(Direction) -> Result<(), E>,
) -> Result<(), E> {
    for _ in 0..count {
        send(Direction::Press)?;
        owned.fetch_or(1 << button, std::sync::atomic::Ordering::AcqRel);
        send(Direction::Release)?;
        owned.fetch_and(!(1 << button), std::sync::atomic::Ordering::AcqRel);
    }
    Ok(())
}
fn release_buttons_with<E>(
    owned: &std::sync::atomic::AtomicU8,
    mut send: impl FnMut(u8) -> Result<(), E>,
) -> Result<(), E> {
    let mut error = None;
    for button in 0..5 {
        if owned.load(std::sync::atomic::Ordering::Acquire) & (1 << button) != 0 {
            match send(button) {
                Ok(()) => {
                    owned.fetch_and(!(1 << button), std::sync::atomic::Ordering::AcqRel);
                }
                Err(e) => {
                    error = Some(e);
                }
            }
        }
    }
    error.map_or(Ok(()), Err)
}
fn send_button(enigo: &mut Enigo, button: u8, direction: Direction) -> enigo::InputResult<()> {
    match button {
        0 => enigo.button(Button::Left, direction),
        1 => enigo.button(Button::Right, direction),
        2 => enigo.button(Button::Middle, direction),
        _ => {
            let (x, y) = enigo.location()?;
            post_side_button_event(x, y, i64::from(button), direction)
                .map_err(|_| enigo::InputError::Simulate("side button event creation failed"))
        }
    }
}
fn post_side_button_event(
    x: i32,
    y: i32,
    button: i64,
    direction: Direction,
) -> Result<(), ComputerError> {
    use core_graphics::{
        event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField},
        geometry::CGPoint,
    };
    let source = event_source()?;
    let event_type = match direction {
        Direction::Press => CGEventType::OtherMouseDown,
        Direction::Release => CGEventType::OtherMouseUp,
        Direction::Click => return Err(ComputerError::Other("untracked side button click".into())),
    };
    let event = CGEvent::new_mouse_event(
        source.clone(),
        event_type,
        CGPoint::new(f64::from(x), f64::from(y)),
        CGMouseButton::Center,
    )
    .map_err(|()| ComputerError::Other("mouse event creation failed".into()))?;
    event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button);
    event.set_flags(
        CGEvent::new(source)
            .map_err(|()| ComputerError::Other("event flags unavailable".into()))?
            .get_flags(),
    );
    event.post(CGEventTapLocation::HID);
    Ok(())
}
fn post_pixel_scroll(dx: i32, dy: i32) -> Result<(), ComputerError> {
    use core_graphics::event::{CGEvent, CGEventTapLocation, ScrollEventUnit};
    let source = event_source()?;
    let flags = CGEvent::new(source.clone())
        .map_err(|()| ComputerError::Other("event flags unavailable".into()))?
        .get_flags();
    let event = CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, -dy, -dx, 0)
        .map_err(|()| ComputerError::Other("pixel scroll event creation failed".into()))?;
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn pixel_to_global(
    geometry: &ComputerFrameGeometry,
    x: u32,
    y: u32,
) -> Result<(i32, i32), ComputerError> {
    if x >= geometry.pixel_width
        || y >= geometry.pixel_height
        || !geometry.scale.is_finite()
        || geometry.scale <= 0.0
    {
        return Err(ComputerError::Other(
            "coordinate outside selected display".into(),
        ));
    }
    // Enigo accepts integer OS points. Keep every capture pixel within its
    // logical point cell; rounding can cross the display's far edge.
    Ok((
        (geometry.origin_x + f64::from(x) / geometry.scale).floor() as i32,
        (geometry.origin_y + f64::from(y) / geometry.scale).floor() as i32,
    ))
}
#[cfg(test)]
mod geometry_tests {
    use super::*;
    #[test]
    fn typed_leading_tabs_are_tracked_and_stop_text_after_a_release_failure() {
        let mut held = Vec::new();
        let mut events = Vec::new();
        let result = type_text_with("\r\n\ttext", |input| match input {
            TextInput::Text(text) => {
                events.push(text.to_string());
                Ok(())
            }
            TextInput::Tab => MacosComputerControl::press_chord_with(
                &[],
                &keymap::Chord {
                    main: enigo::Key::Tab,
                    modifiers: vec![],
                },
                |key, direction| {
                    assert_ne!(direction, Direction::Click);
                    events.push(format!("{key:?}:{direction:?}"));
                    if direction == Direction::Press {
                        held.push(key);
                        Ok(())
                    } else {
                        Err(enigo::InputError::Simulate("release failed"))
                    }
                },
            ),
        });
        assert!(result.is_err());
        assert_eq!(held, [enigo::Key::Tab]);
        assert_eq!(
            events,
            ["\u{200B}\r", "\u{200B}\n", "Tab:Press", "Tab:Release"]
        );
    }
    #[test]
    fn typed_chunks_keep_unicode_boundaries_and_embedded_tabs() {
        let prefix = "界".repeat(20);
        let text = format!("{prefix}\t\n\ta\tb");
        let mut events = Vec::new();
        type_text_with(&text, |input| {
            events.push(match input {
                TextInput::Text(text) => format!("text:{text}"),
                TextInput::Tab => "tab".into(),
            });
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(
            events,
            [
                format!("text:{prefix}"),
                "tab".into(),
                "text:\u{200B}\n".into(),
                "tab".into(),
                "text:a\tb".into()
            ]
        );
    }
    #[test]
    fn every_click_button_retains_failed_release_and_retries_cleanup() {
        use std::sync::atomic::{AtomicU8, Ordering};
        for button in 0..5 {
            let held = AtomicU8::new(0);
            let mut directions = Vec::new();
            let result = click_button_with(&held, button, 1, |direction| {
                directions.push(direction);
                if direction == Direction::Release {
                    Err("release failed")
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            assert_eq!(directions, [Direction::Press, Direction::Release]);
            assert_eq!(held.load(Ordering::Acquire), 1 << button);
            assert!(release_buttons_with(&held, |_| Err("still failing")).is_err());
            assert_eq!(held.load(Ordering::Acquire), 1 << button);
            release_buttons_with(&held, |released| {
                assert_eq!(released, button);
                Ok::<_, &str>(())
            })
            .unwrap();
            assert_eq!(held.load(Ordering::Acquire), 0);
        }
    }
    #[test]
    fn chord_release_failure_is_reported_and_other_modifiers_are_still_released() {
        let chord = keymap::Chord {
            main: enigo::Key::Unicode('a'),
            modifiers: vec![enigo::Key::Meta, enigo::Key::Shift],
        };
        for failed in [enigo::Key::Shift, chord.main] {
            let mut releases = Vec::new();
            let result = MacosComputerControl::press_chord_with(&[], &chord, |key, direction| {
                assert_ne!(
                    direction,
                    Direction::Click,
                    "compound input must track each successful press"
                );
                if direction == Direction::Release {
                    releases.push(key);
                    if key == failed {
                        return Err(enigo::InputError::InvalidInput("release failed".into()));
                    }
                }
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(
                releases,
                vec![chord.main, enigo::Key::Shift, enigo::Key::Meta]
            );
        }
    }
    #[test]
    fn capture_pixels_stay_inside_the_selected_display_point_bounds() {
        for (origin_x, origin_y, scale, width, height) in [
            (0.0, 0.0, 2.0, 2880, 1800),
            (1440.0, 900.0, 2.0, 2880, 1800),
            (-1440.0, -900.0, 2.0, 2880, 1800),
            (0.0, 0.0, 1.5, 6, 6),
        ] {
            let geometry = ComputerFrameGeometry {
                display_id: 2,
                pixel_width: width,
                pixel_height: height,
                origin_x,
                origin_y,
                scale,
                version: "edge-fixture".into(),
            };
            for (x, y) in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
                (1, 1),
            ] {
                let (global_x, global_y) = pixel_to_global(&geometry, x, y).unwrap();
                assert!(
                    f64::from(global_x) >= origin_x
                        && f64::from(global_x) < origin_x + f64::from(width) / scale
                        && f64::from(global_y) >= origin_y
                        && f64::from(global_y) < origin_y + f64::from(height) / scale,
                    "pixel ({x},{y}) escaped display bounds: ({global_x},{global_y}), origin=({origin_x},{origin_y}), scale={scale}"
                );
                assert_eq!(
                    (global_x, global_y),
                    (
                        (origin_x + f64::from(x) / scale).floor() as i32,
                        (origin_y + f64::from(y) / scale).floor() as i32
                    ),
                    "all pixels in a logical point cell must target that cell"
                );
            }
            assert!(pixel_to_global(&geometry, width, 0).is_err());
            assert!(pixel_to_global(&geometry, 0, height).is_err());
        }
    }

    #[test]
    fn retina_secondary_pixels_become_global_points() {
        let geometry = ComputerFrameGeometry {
            display_id: 2,
            pixel_width: 2880,
            pixel_height: 1800,
            origin_x: -1440.0,
            origin_y: 120.0,
            scale: 2.0,
            version: "v1".into(),
        };
        assert_eq!(pixel_to_global(&geometry, 200, 400).unwrap(), (-1340, 320));
        assert_eq!(pixel_to_global(&geometry, 0, 0).unwrap(), (-1440, 120));
        assert!(pixel_to_global(&geometry, 2880, 1).is_err());
    }
}
