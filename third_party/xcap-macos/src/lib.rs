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
