use super::*;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> core_foundation::base::CFTypeRef;
    fn AXUIElementCopyElementAtPosition(
        element: core_foundation::base::CFTypeRef,
        x: f32,
        y: f32,
        result: *mut core_foundation::base::CFTypeRef,
    ) -> i32;
    fn AXUIElementGetPid(element: core_foundation::base::CFTypeRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(
        element: core_foundation::base::CFTypeRef,
        seconds: f32,
    ) -> i32;
}

fn owner(window: &xcap::Window) -> Option<AppInfo> {
    let pid = i32::try_from(window.pid().ok()?).ok()?;
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app_info_from(&app)
}

fn target_ax_failure(
    trusted: bool,
    stage: &str,
    status: Option<i32>,
    has_value: Option<bool>,
    has_pid: Option<bool>,
) -> ComputerError {
    // Only known stages, raw AX status and presence bits are retained. Never
    // include AX values, application content, titles, PIDs or coordinates.
    let facts = format!(
        "AX target stage={stage}, status={status:?}, hasValue={has_value:?}, hasPid={has_pid:?}"
    );
    if !trusted {
        return ComputerError::PermissionDenied(format!(
            "macOS Accessibility permission is not granted. Grant it in System Settings and call request_access again. ({facts})"
        ));
    }
    // An unsupported/busy target is not an OS grant revocation. This still
    // refuses the effect: no window-rectangle or guessed-application fallback.
    ComputerError::Other(format!(
        "Cannot verify the application at the input location because Accessibility target messaging is unavailable. Take a fresh screenshot and retry. ({facts})"
    ))
}

fn current_target_ax_failure(
    stage: &str,
    status: Option<i32>,
    has_value: Option<bool>,
    has_pid: Option<bool>,
) -> ComputerError {
    target_ax_failure(
        tcc::accessibility_granted(),
        stage,
        status,
        has_value,
        has_pid,
    )
}

impl MacosComputerControl {
    /// Compose only individually captured authorized windows on an opaque canvas.
    /// Never starts from an unfiltered desktop image. Unknown windows mask content.
    pub(super) fn capture_allowed(
        &self,
        allowed: &[String],
    ) -> Result<image::RgbaImage, ComputerError> {
        let geometry = self.geometry()?;
        let mut canvas = image::RgbaImage::from_pixel(
            geometry.pixel_width,
            geometry.pixel_height,
            image::Rgba([0, 0, 0, 255]),
        );
        let windows = xcap::Window::all().map_err(|e| monitor_error(&e))?;
        // CoreGraphics' on-screen window inventory is front-to-back.
        for window in windows.into_iter().rev() {
            let x = window.x().map_err(|e| monitor_error(&e))?;
            let y = window.y().map_err(|e| monitor_error(&e))?;
            let width = window.width().map_err(|e| monitor_error(&e))?;
            let height = window.height().map_err(|e| monitor_error(&e))?;
            let left = ((f64::from(x) - geometry.origin_x) * geometry.scale).round() as i64;
            let top = ((f64::from(y) - geometry.origin_y) * geometry.scale).round() as i64;
            let width = (f64::from(width) * geometry.scale).round() as u32;
            let height = (f64::from(height) * geometry.scale).round() as u32;
            if width == 0
                || height == 0
                || left >= i64::from(canvas.width())
                || top >= i64::from(canvas.height())
                || left + i64::from(width) <= 0
                || top + i64::from(height) <= 0
            {
                continue;
            }
            // Bound temporary allocations even if a window has corrupt geometry.
            if width > 32768 || height > 32768 || u64::from(width) * u64::from(height) > 67_108_864
            {
                return Err(ComputerError::Other(
                    "window geometry exceeds capture budget".into(),
                ));
            }
            let approved = owner(&window).is_some_and(|app| allowed.contains(&app.bundle_id));
            let pixels = if approved {
                let captured = window.capture_image().map_err(|e| monitor_error(&e))?;
                image::imageops::resize(
                    &captured,
                    width,
                    height,
                    image::imageops::FilterType::Nearest,
                )
            } else {
                image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]))
            };
            image::imageops::replace(&mut canvas, &pixels, left, top);
        }
        Ok(canvas)
    }

    pub(super) fn app_at(&self, x: u32, y: u32) -> Result<Option<AppInfo>, ComputerError> {
        use core_foundation::base::{CFType, TCFType};
        let (x, y) = self.target_point(x, y)?;
        if !tcc::accessibility_granted() {
            return Err(target_ax_failure(false, "trust_check", None, None, None));
        }
        // Window rectangles do not establish an input target: click-through and
        // transparent windows can expose a different application's hit region.
        let system = unsafe { AXUIElementCreateSystemWide() };
        if system.is_null() {
            return Err(current_target_ax_failure(
                "create_systemwide",
                None,
                Some(false),
                None,
            ));
        }
        let system = unsafe { CFType::wrap_under_create_rule(system) };
        let timeout_status =
            unsafe { AXUIElementSetMessagingTimeout(system.as_concrete_TypeRef(), 0.25) };
        if timeout_status != 0 {
            return Err(current_target_ax_failure(
                "set_timeout",
                Some(timeout_status),
                None,
                None,
            ));
        }
        let mut element = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyElementAtPosition(
                system.as_concrete_TypeRef(),
                x as f32,
                y as f32,
                &mut element,
            )
        };
        let has_value = !element.is_null();
        // Honor Copy ownership even for an unexpected error/value pair.
        let element = has_value.then(|| unsafe { CFType::wrap_under_create_rule(element) });
        if status != 0 || !has_value {
            return Err(current_target_ax_failure(
                "hit_test",
                Some(status),
                Some(has_value),
                None,
            ));
        }
        let element = element.expect("a successful AX hit test has an owned value");
        let mut pid = 0;
        let pid_status = unsafe { AXUIElementGetPid(element.as_concrete_TypeRef(), &mut pid) };
        if pid_status != 0 || pid <= 0 {
            return Err(current_target_ax_failure(
                "get_pid",
                Some(pid_status),
                Some(true),
                Some(pid > 0),
            ));
        }
        Ok(
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .as_deref()
                .and_then(app_info_from),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{target_ax_failure, ComputerError};

    #[test]
    fn trusted_unsupported_and_busy_targets_are_not_permission_denials() {
        for status in [-25208, -25204, -25211, -1, 0] {
            let error = target_ax_failure(true, "hit_test", Some(status), Some(false), None);
            assert!(matches!(error, ComputerError::Other(_)));
            let message = error.to_string();
            assert!(message.contains("stage=hit_test"));
            assert!(message.contains(&format!("status=Some({status})")));
            assert!(message.contains("hasValue=Some(false)"));
            assert!(!message.contains("Grant it in System Settings"));
        }
    }

    #[test]
    fn confirmed_missing_grant_is_a_permission_denial_at_every_failure_stage() {
        for stage in [
            "trust_check",
            "create_systemwide",
            "set_timeout",
            "hit_test",
            "get_pid",
        ] {
            let error = target_ax_failure(false, stage, Some(-25208), Some(false), None);
            assert!(matches!(error, ComputerError::PermissionDenied(_)));
            assert!(error
                .to_string()
                .contains("Accessibility permission is not granted"));
        }
    }

    #[test]
    fn unavailable_setup_or_owner_preserves_bounded_diagnostics_without_reauthorization() {
        for (stage, status, value, pid) in [
            ("create_systemwide", None, Some(false), None),
            ("set_timeout", Some(-25202), None, None),
            ("get_pid", Some(0), Some(true), Some(false)),
        ] {
            let error = target_ax_failure(true, stage, status, value, pid);
            assert!(matches!(error, ComputerError::Other(_)));
            let message = error.to_string();
            assert!(message.contains(&format!("stage={stage}")));
            assert!(message.contains(&format!("status={status:?}")));
            assert!(message.contains(&format!("hasValue={value:?}")));
            assert!(message.contains(&format!("hasPid={pid:?}")));
        }
    }
}

impl MacosComputerControl {
    pub(super) async fn capture_native_filtered(
        &self,
        request: &lingxi_core::host::computer_control::FilteredCaptureRequest,
        allowed: &[String],
    ) -> Result<Option<lingxi_core::host::computer_control::FilteredCapture>, ComputerError> {
        use lingxi_core::host::computer_control::FilteredCapture;
        if !xcap::native_filtered_capture_available() {
            return Ok(None);
        }
        let geometry = self.geometry()?;
        if geometry.display_id != request.display_id
            || !geometry.scale.is_finite()
            || geometry.scale <= 0.
        {
            return Err(ComputerError::Other(
                "native capture display geometry changed".into(),
            ));
        }
        let (source_rect, source_width, source_height) =
            if let Some([x, y, w, h]) = request.source_rect {
                let logical_width = f64::from(geometry.pixel_width) / geometry.scale;
                let logical_height = f64::from(geometry.pixel_height) / geometry.scale;
                if ![x, y, w, h].iter().all(|n| n.is_finite())
                    || x < 0.
                    || y < 0.
                    || w <= 0.
                    || h <= 0.
                    || x + w > logical_width + 1e-9
                    || y + h > logical_height + 1e-9
                {
                    return Err(ComputerError::Other(
                        "native capture region outside display".into(),
                    ));
                }
                (
                    Some([x, y, w, h]),
                    (w * geometry.scale).round().max(1.) as u32,
                    (h * geometry.scale).round().max(1.) as u32,
                )
            } else {
                (None, geometry.pixel_width, geometry.pixel_height)
            };
        let saved = geometry.clone();
        let request = request.clone();
        let allowed = allowed.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            let image = xcap::filtered_display_image(
                request.display_id,
                &allowed,
                branding::MANAGED_PREFERENCES_DOMAIN,
                source_rect,
                request.output_width,
                request.output_height,
            )
            .map_err(|e| monitor_error(&e))?;
            let jpeg_bytes = jpeg::encode_cgimage(&image)?;
            Ok::<_, ComputerError>(FilteredCapture {
                jpeg_bytes,
                output_width: request.output_width,
                output_height: request.output_height,
                source_width,
                source_height,
                geometry: saved,
            })
        })
        .await
        .map_err(|e| ComputerError::Other(format!("native capture worker failed: {e}")))??;
        if self.geometry()? != geometry {
            return Err(ComputerError::Other(
                "display geometry changed during native capture".into(),
            ));
        }
        Ok(Some(result))
    }
}
