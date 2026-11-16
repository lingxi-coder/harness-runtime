#[cfg(not(target_os = "macos"))]
compile_error!("the vendored xcap package supports only macOS");

#[cfg(target_os = "macos")]
mod error;
#[cfg(target_os = "macos")]
mod monitor;
#[cfg(target_os = "macos")]
mod video_recorder;
#[cfg(target_os = "macos")]
mod window;

#[cfg(target_os = "macos")]
#[path = "macos/mod.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub use image;

#[cfg(target_os = "macos")]
pub use error::{XCapError, XCapResult};
#[cfg(target_os = "macos")]
pub use monitor::Monitor;
#[cfg(target_os = "macos")]
pub use window::Window;

#[cfg(target_os = "macos")]
pub use video_recorder::Frame;
#[cfg(target_os = "macos")]
pub use video_recorder::VideoRecorder;

/// Capture one explicitly identified window via ScreenCaptureKit. Bounds are
/// checked against the live SCWindow; geometry never substitutes for identity.
#[cfg(target_os = "macos")]
pub fn capture_window_by_id(id: u32, bounds: [f64; 4]) -> XCapResult<image::RgbaImage> {
    platform::capture::capture(
        objc2_core_foundation::CGRect {
            origin: objc2_core_foundation::CGPoint::new(bounds[0], bounds[1]),
            size: objc2_core_foundation::CGSize::new(bounds[2], bounds[3]),
        },
        objc2_core_graphics::CGWindowListOption::OptionIncludingWindow,
        id,
    )
}

/// Direct filtered native display CGImage; no RGBA/profile conversion.
#[cfg(target_os = "macos")]
pub fn filtered_display_image(
    display_id: u32,
    allowed: &[String],
    host_root: &str,
    source_rect: Option<[f64; 4]>,
    width: u32,
    height: u32,
) -> XCapResult<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    platform::capture::filtered_display_image(
        display_id,
        allowed,
        host_root,
        source_rect,
        width,
        height,
    )
}

#[cfg(target_os = "macos")]
pub fn native_filtered_capture_available() -> bool {
    objc2::runtime::AnyClass::get(c"SCScreenshotManager").is_some()
}

#[cfg(target_os = "macos")]
pub fn native_window_image(
    id: u32,
    bounds: [f64; 4],
    target: Option<(u32, u32)>,
) -> XCapResult<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    platform::capture::native_window_image(id, bounds, target)
}
