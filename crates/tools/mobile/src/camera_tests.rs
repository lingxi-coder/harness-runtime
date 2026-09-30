use super::*;
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
    assert!(file["base64"].as_str().unwrap().len() <= 5_242_880);
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
