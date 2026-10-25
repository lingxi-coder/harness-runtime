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
        // Window rectangles do not establish an input target: click-through and
        // transparent windows can expose a different application's hit region.
        let system = unsafe { AXUIElementCreateSystemWide() };
        if system.is_null() {
            return Err(ComputerError::PermissionDenied(
                "Accessibility hit testing is unavailable".into(),
            ));
        }
        let system = unsafe { CFType::wrap_under_create_rule(system) };
        if unsafe { AXUIElementSetMessagingTimeout(system.as_concrete_TypeRef(), 0.25) } != 0 {
            return Err(ComputerError::PermissionDenied(
                "Accessibility hit testing could not set a bounded timeout".into(),
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
        if status != 0 || element.is_null() {
            return Err(ComputerError::PermissionDenied(
                "Cannot establish the application at the input location".into(),
            ));
        }
        let element = unsafe { CFType::wrap_under_create_rule(element) };
        let mut pid = 0;
        if unsafe { AXUIElementGetPid(element.as_concrete_TypeRef(), &mut pid) } != 0 || pid <= 0 {
            return Err(ComputerError::PermissionDenied(
                "Cannot establish the input target owner".into(),
            ));
        }
        Ok(
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .as_deref()
                .and_then(app_info_from),
        )
    }
}
