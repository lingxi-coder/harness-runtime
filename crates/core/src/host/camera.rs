//! `CameraControl` — photo capture + library picker seam (M8-P10).
//!
//! Implemented natively in Swift/Kotlin via `UniFFI` (P12) and injected into the
//! mobile `Platform`. The `tool-camera` tool (P11) routes through it so Rust
//! can request a photo without knowing the native camera API.

use async_trait::async_trait;
use thiserror::Error;

/// Which camera to use for capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPosition {
    /// Front ("selfie") camera.
    Front,
    /// Rear camera.
    Back,
}

/// Options for a photo capture.
#[derive(Debug, Clone)]
pub struct CapturePhotoOpts {
    /// Which camera to use.
    pub position: CameraPosition,
    /// Whether to present the native edit/crop UI after capture.
    pub allow_editing: bool,
}

/// A captured (or picked) image, JPEG-encoded.
#[derive(Debug, Clone)]
pub struct CapturedImage {
    /// JPEG-encoded image bytes.
    pub jpeg_bytes: Vec<u8>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Failure modes for [`CameraControl`] operations.
#[derive(Debug, Clone, Error)]
pub enum CameraError {
    /// The user denied camera/photo-library permission.
    #[error("camera permission denied")]
    PermissionDenied,
    /// The user cancelled the capture/picker.
    #[error("camera capture cancelled")]
    Cancelled,
    /// No camera hardware is available.
    #[error("camera device unavailable")]
    DeviceUnavailable,
    /// Any other native failure.
    #[error("camera error: {0}")]
    Other(String),
}

/// Native camera + photo-library access.
#[async_trait]
pub trait CameraControl: Send + Sync {
    /// Capture a photo with the native camera UI.
    async fn capture_photo(&self, opts: CapturePhotoOpts) -> Result<CapturedImage, CameraError>;
    /// Pick an existing image from the photo library.
    async fn pick_from_library(&self) -> Result<CapturedImage, CameraError>;

    /// Capture a photo downscaled to at most `max_dimension` px on its longer
    /// side, re-encoded at `jpeg_quality` (0.0..=1.0). Scaling happens on the
    /// NATIVE side (Rust ships no image codec); the default delegates to the
    /// full-size capture so existing implementors keep working unchanged.
    async fn capture_photo_sized(
        &self,
        opts: CapturePhotoOpts,
        _max_dimension: u32,
        _jpeg_quality: f32,
    ) -> Result<CapturedImage, CameraError> {
        self.capture_photo(opts).await
    }

    /// Library pick with the same native-side downscale contract as
    /// [`CameraControl::capture_photo_sized`].
    async fn pick_from_library_sized(
        &self,
        _max_dimension: u32,
        _jpeg_quality: f32,
    ) -> Result<CapturedImage, CameraError> {
        self.pick_from_library().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Implements ONLY the two original methods — proving the `*_sized`
    /// variants have working default bodies that delegate, so existing
    /// implementors (android-aar, stubs) keep compiling unchanged.
    struct BaseOnlyCamera;

    #[async_trait]
    impl CameraControl for BaseOnlyCamera {
        async fn capture_photo(
            &self,
            _opts: CapturePhotoOpts,
        ) -> Result<CapturedImage, CameraError> {
            Ok(CapturedImage {
                jpeg_bytes: vec![1, 2, 3],
                width: 4000,
                height: 3000,
            })
        }

        async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
            Ok(CapturedImage {
                jpeg_bytes: vec![9],
                width: 100,
                height: 50,
            })
        }
    }

    #[tokio::test]
    async fn sized_capture_defaults_to_the_full_size_capture() {
        let camera = BaseOnlyCamera;
        let opts = CapturePhotoOpts {
            position: CameraPosition::Back,
            allow_editing: false,
        };
        let image = camera
            .capture_photo_sized(opts, 1280, 0.8)
            .await
            .expect("default capture");
        assert_eq!(image.jpeg_bytes, vec![1, 2, 3]);
        assert_eq!((image.width, image.height), (4000, 3000));
    }

    #[tokio::test]
    async fn sized_pick_defaults_to_the_full_size_pick() {
        let camera = BaseOnlyCamera;
        let image = camera
            .pick_from_library_sized(1280, 0.8)
            .await
            .expect("default pick");
        assert_eq!(image.jpeg_bytes, vec![9]);
    }
}
