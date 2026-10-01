use super::*;
use base64::Engine;
use image::GenericImageView;
use lingxi_core::host::camera::{CameraControl, CapturedImage};
use std::sync::{Arc, Mutex};

struct FakeCamera {
    result: Result<CapturedImage, CameraError>,
    captures: Mutex<Vec<CapturePhotoOpts>>,
    sizing: Mutex<Vec<(u32, f32)>>,
}

impl FakeCamera {
    fn new(result: Result<CapturedImage, CameraError>) -> Self {
        Self {
            result,
            captures: Mutex::new(Vec::new()),
            sizing: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl CameraControl for FakeCamera {
    async fn capture_photo(&self, opts: CapturePhotoOpts) -> Result<CapturedImage, CameraError> {
        self.captures.lock().unwrap().push(opts);
        self.result.clone()
    }

    async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
        self.result.clone()
    }

    async fn capture_photo_sized(
        &self,
        opts: CapturePhotoOpts,
        max_dimension: u32,
        jpeg_quality: f32,
    ) -> Result<CapturedImage, CameraError> {
        self.sizing
            .lock()
            .unwrap()
            .push((max_dimension, jpeg_quality));
        self.capture_photo(opts).await
    }

    async fn pick_from_library_sized(
        &self,
        max_dimension: u32,
        jpeg_quality: f32,
    ) -> Result<CapturedImage, CameraError> {
        self.sizing
            .lock()
            .unwrap()
            .push((max_dimension, jpeg_quality));
        self.pick_from_library().await
    }
}

fn photo(width: u32, height: u32) -> CapturedImage {
    let image = image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Jpeg)
        .unwrap();
    CapturedImage {
        jpeg_bytes: bytes.into_inner(),
        width,
        height,
    }
}

fn ctx_with(camera: Option<Arc<dyn CameraControl>>) -> BuiltinToolContext {
    let mut ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.camera = camera;
    ctx
}

async fn call(tool: &CameraTool, action: &str) -> Result<ToolCallResult, ToolError> {
    tool.call(
        json!({ "action": action, "position": "front", "allow_editing": true }),
        tool_api::test_support::fresh_ctx(),
        tool_api::test_support::fresh_tx(),
    )
    .await
}

#[tokio::test]
async fn capture_and_library_results_reach_the_model_as_images() {
    for action in ["capture", "pick_from_library"] {
        let fake = Arc::new(FakeCamera::new(Ok(photo(4, 2))));
        let tool = CameraTool::new(ctx_with(Some(fake.clone())));
        let result = call(&tool, action).await.unwrap();
        let blocks = tool_api::tool_result_media::media_content_blocks(&result.data)
            .expect("camera result must produce a real model image block");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["media_type"], "image/jpeg");
        assert!(!blocks[0]["source"]["data"].as_str().unwrap().is_empty());
        assert_eq!(result.data["captured"], true);
        assert_eq!(result.data["cancelled"], false);
        assert!(result.model_content.as_ref().unwrap().len() < 200);
        assert!(!result.is_error);
        assert_eq!(*fake.sizing.lock().unwrap(), vec![(IMAGE_MAX_DIM, 0.8)]);
        let captures = fake.captures.lock().unwrap();
        if action == "capture" {
            assert_eq!(captures.len(), 1);
            assert_eq!(captures[0].position, CameraPosition::Front);
            assert!(captures[0].allow_editing);
        } else {
            assert!(captures.is_empty());
        }
    }
}

#[tokio::test]
async fn oversized_native_images_are_bounded_before_model_delivery() {
    let fake = Arc::new(FakeCamera::new(Ok(photo(4000, 8))));
    let tool = CameraTool::new(ctx_with(Some(fake)));
    let result = call(&tool, "capture").await.unwrap();
    let file = &result.data["file"];
    assert_eq!(file["dimensions"]["originalWidth"], 4000);
    assert_eq!(file["dimensions"]["displayWidth"], IMAGE_MAX_DIM);
    assert_eq!(result.data["width"], IMAGE_MAX_DIM);
    assert_eq!(result.data["height"], file["dimensions"]["displayHeight"]);
    assert!(file["base64"].as_str().unwrap().len() <= 5_242_880);
}

/// Like Android's bridge, this provider implements only the original methods:
/// the trait's sized fallback still hands Rust the untouched library JPEG.
struct OriginalLibraryCamera(CapturedImage);

#[async_trait]
impl CameraControl for OriginalLibraryCamera {
    async fn capture_photo(&self, _: CapturePhotoOpts) -> Result<CapturedImage, CameraError> {
        Ok(self.0.clone())
    }

    async fn pick_from_library(&self) -> Result<CapturedImage, CameraError> {
        Ok(self.0.clone())
    }
}

fn portrait_library_jpeg(width: u32, height: u32) -> CapturedImage {
    // The stored raster is horizontal. EXIF 6 displays its red left half at
    // the top and its blue right half at the bottom after a clockwise turn.
    let pixels = image::RgbImage::from_fn(width, height, |x, _| {
        image::Rgb(if x < width / 2 {
            [240, 0, 0]
        } else {
            [0, 0, 240]
        })
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&pixels)
        .unwrap();
    // Big-endian TIFF IFD0 containing one SHORT Orientation tag with value 6.
    let exif = [
        b'E', b'x', b'i', b'f', 0, 0, b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 1, 18, 0, 3, 0, 0, 0, 1,
        0, 6, 0, 0, 0, 0, 0, 0,
    ];
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
    segment.extend_from_slice(&exif);
    jpeg.splice(2..2, segment);
    CapturedImage {
        jpeg_bytes: jpeg,
        width,
        height,
    }
}

#[tokio::test]
async fn original_library_photos_reach_the_model_upright_with_matching_dimensions() {
    // Exercise both the native-sized fallback and the small-image path: EXIF
    // must be normalized even when the source needs no dimension reduction.
    for (width, height) in [(2100, 800), (210, 80)] {
        let source = portrait_library_jpeg(width, height);
        let tool = CameraTool::new(ctx_with(Some(Arc::new(OriginalLibraryCamera(source)))));
        let result = call(&tool, "pick_from_library").await.unwrap();
        let blocks = tool_api::tool_result_media::media_content_blocks(&result.data).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(blocks[0]["source"]["data"].as_str().unwrap())
            .unwrap();
        let delivered = image::load_from_memory(&bytes).unwrap();
        let (dw, dh) = delivered.dimensions();
        assert!(
            dw < dh,
            "portrait must stay upright in the model image block"
        );
        assert!(dh <= IMAGE_MAX_DIM);
        assert_eq!(result.data["width"], dw);
        assert_eq!(result.data["height"], dh);
        let top = delivered.get_pixel(dw / 2, dh / 4);
        let bottom = delivered.get_pixel(dw / 2, 3 * dh / 4);
        assert!(top[0] > 200 && top[2] < 30, "top must be red: {top:?}");
        assert!(
            bottom[2] > 200 && bottom[0] < 30,
            "bottom must be blue: {bottom:?}"
        );
        assert!(!result.is_error);
    }
}

#[tokio::test]
async fn cancelled_camera_actions_return_no_photo_and_no_error() {
    for action in ["capture", "pick_from_library"] {
        let fake = Arc::new(FakeCamera::new(Err(CameraError::Cancelled)));
        let tool = CameraTool::new(ctx_with(Some(fake)));
        let result = call(&tool, action).await.unwrap();
        assert_eq!(result.data["captured"], false);
        assert_eq!(result.data["cancelled"], true);
        assert!(!result.is_error);
        assert!(tool_api::tool_result_media::media_content_blocks(&result.data).is_none());
    }
}

#[tokio::test]
async fn corrupt_or_empty_camera_bytes_do_not_report_photo_success() {
    for bytes in [vec![], vec![1, 2, 3]] {
        let fake = Arc::new(FakeCamera::new(Ok(CapturedImage {
            jpeg_bytes: bytes,
            width: 4,
            height: 2,
        })));
        let tool = CameraTool::new(ctx_with(Some(fake)));
        assert!(matches!(
            call(&tool, "capture").await,
            Err(ToolError::Internal(_))
        ));
    }
}

#[tokio::test]
async fn missing_camera_is_disabled_and_returns_an_error() {
    let tool = CameraTool::new(ctx_with(None));
    assert!(!tool.is_enabled(&ToolStaticContext::default()));
    assert!(matches!(
        call(&tool, "capture").await,
        Err(ToolError::Internal(_))
    ));
}

#[tokio::test]
async fn permission_denied_does_not_disable_supported_camera() {
    let fake = Arc::new(FakeCamera::new(Err(CameraError::PermissionDenied)));
    let tool = CameraTool::new(ctx_with(Some(fake)));
    assert!(tool.is_enabled(&ToolStaticContext::default()));
    assert!(matches!(
        call(&tool, "capture").await,
        Err(ToolError::PermissionDenied(_))
    ));
}
