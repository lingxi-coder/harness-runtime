//! Display-scoped MCP protocol over the existing guarded computer executor.
use super::protocol_result::{error, from_wire, text, wire};
use super::{
    json, ComputerTool, ToolCallResult, ToolError, ToolUseContext, Utf16JsonProjection, Value,
};
use base64::Engine;
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, GenericImageView};
use std::collections::HashMap;
use tool_api::progress::ToolProgressSender;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub(super) struct DisplayProtocolState {
    frames: HashMap<String, ProtocolFrame>,
    clipboard_stashes: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_tests::{ctx, make_tool, RecordingBackend};
    use std::sync::Arc;
    use tool_api::Tool;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../fixtures/display-2.1.325.json")).unwrap()
    }

    #[test]
    fn display_image_budget_matches_latest_source_cases() {
        for case in fixture()["scaleCases"].as_array().unwrap() {
            let width = case["input"][0].as_u64().unwrap() as u32;
            let height = case["input"][1].as_u64().unwrap() as u32;
            let (w, h) = budget_dimensions(width, height);
            assert_eq!(json!([w, h]), case["output"], "{case}");
        }
    }

    #[test]
    fn display_number_spelling_matches_ecmascript_interpolation() {
        for case in fixture()["numberCases"].as_array().unwrap() {
            assert_eq!(
                number_text(case["input"].as_f64().unwrap()),
                case["output"].as_str().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn fractional_native_zoom_source_geometry_matches_reviewed_js_bits() {
        for case in fixture()["zoomCases"].as_array().unwrap() {
            let input = &case["input"];
            let frame = ProtocolFrame {
                width: input["frameWidth"].as_u64().unwrap() as u32,
                height: input["frameHeight"].as_u64().unwrap() as u32,
                capture_width: input["captureWidth"].as_u64().unwrap() as u32,
                capture_height: input["captureHeight"].as_u64().unwrap() as u32,
                scale: input["scale"].as_f64().unwrap(),
                display_id: 7,
                origin_x: 0.,
                origin_y: 0.,
                explicitly_scaled: false,
                policy_revision: None,
            };
            let actual = protocol_zoom_region(&frame, &json!({"region":input["region"]})).unwrap();
            for (value, key) in actual.into_iter().zip(["x", "y", "w", "h"]) {
                assert_eq!(
                    value.to_bits(),
                    case["output"][key].as_f64().unwrap().to_bits(),
                    "{case}"
                );
            }
        }
    }

    #[tokio::test]
    async fn display_focus_continuity_matches_latest_source_cases() {
        for case in fixture()["focusCases"].as_array().unwrap() {
            let backend = Arc::new(RecordingBackend::default());
            let tool = make_tool(backend);
            let expected = case["input"]["expected"].as_object().map(|app| {
                lingxi_core::host::computer_control::AppInfo {
                    bundle_id: app["bundleId"].as_str().unwrap().into(),
                    display_name: app["displayName"].as_str().unwrap().into(),
                }
            });
            let output = tool
                .protocol_focus_delivery(
                    expected.as_ref(),
                    case["input"]["prefix"].as_str().unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                output.as_ref().map(wire).unwrap_or(Value::Null),
                case["output"]
            );
        }
    }

    #[tokio::test]
    async fn display_teach_anchor_uses_full_frame_after_a_half_scale_capture() {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend);
        let context = ctx();
        tool.call_display_protocol(
            "screenshot",
            tool_api::tool_trait::generated_builtin_result(&json!({"scale":0.5})),
            context.clone(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(
            tool.protocol_anchor(&[2.0, 1.0], &context).await,
            Some(json!([-99, 1]))
        );
        assert_eq!(tool.protocol_anchor(&[4.01, 1.0], &context).await, None);
        assert_eq!(tool.protocol_anchor(&[-1.0, 1.0], &context).await, None);
        tool.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn display_public_wire_matches_latest_controlled_source_cases() {
        for case in fixture()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| case["productionDifferential"] == true)
        {
            let backend = Arc::new(RecordingBackend::default());
            let tool = make_tool(backend.clone());
            let context = ctx();
            let result = tool
                .call_display_protocol(
                    case["action"].as_str().unwrap(),
                    tool_api::tool_trait::generated_builtin_result(&case["input"]),
                    context.clone(),
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_string(&wire(&result)).unwrap(),
                serde_json::to_string(&case["output"]).unwrap(),
                "{case}"
            );
            tool.cleanup_computer_inputs(&context).await.unwrap();
        }
    }

    #[tokio::test]
    async fn display_fractional_input_and_graphemes_reach_the_guarded_backend() {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        let context = ctx();
        let result = tool
            .call_display_protocol(
                "left_click",
                tool_api::tool_trait::generated_builtin_result(&json!({"coordinate":[1.25,0.4]})),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(backend.events().contains(&"click:2,0".into()));
        let result = tool
            .call_display_protocol(
                "type",
                tool_api::tool_trait::generated_builtin_result(&json!({"text":"Ae\u{301}😀\n\t"})),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            wire(&result),
            json!({"content":[{"type":"text","text":"Typed 5 grapheme(s)."}]})
        );
        for expected in [
            "type:A",
            "type:e\u{301}",
            "type:😀",
            "key:return",
            "key:tab",
        ] {
            assert!(
                backend.events().contains(&expected.to_owned()),
                "{expected}: {:?}",
                backend.events()
            );
        }
        tool.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn mcp_capture_budgets_original_pixels_once_for_screenshot_and_zoom() {
        use lingxi_core::host::computer_control::{
            AppInfo, ComputerControl, ComputerError, Screenshot,
        };
        struct LargeCapture {
            png: Vec<u8>,
            encoded: Option<Vec<u8>>,
            calls: Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
            fail: bool,
            native: bool,
            native_requests: Arc<
                std::sync::Mutex<Vec<lingxi_core::host::computer_control::FilteredCaptureRequest>>,
            >,
        }
        #[async_trait::async_trait]
        impl ComputerControl for LargeCapture {
            fn supports_native_filtered_capture(&self) -> bool {
                self.native
            }
            async fn frame_geometry(
                &self,
            ) -> Result<
                Option<lingxi_core::host::computer_control::ComputerFrameGeometry>,
                ComputerError,
            > {
                Ok(self.native.then(|| {
                    lingxi_core::host::computer_control::ComputerFrameGeometry {
                        display_id: 3,
                        pixel_width: 2300,
                        pixel_height: 1200,
                        origin_x: -200.,
                        origin_y: 0.,
                        scale: 1.,
                        version: "native-test".into(),
                    }
                }))
            }
            async fn capture_filtered_display(
                &self,
                request: &lingxi_core::host::computer_control::FilteredCaptureRequest,
                _allowed: &[String],
            ) -> Result<Option<lingxi_core::host::computer_control::FilteredCapture>, ComputerError>
            {
                if !self.native {
                    return Ok(None);
                }
                self.native_requests.lock().unwrap().push(request.clone());
                if self.fail {
                    return Err(ComputerError::Other("native capture failed".into()));
                }
                let (source_width, source_height) = request
                    .source_rect
                    .map(|r| (r[2].round() as u32, r[3].round() as u32))
                    .unwrap_or((2300, 1200));
                Ok(Some(lingxi_core::host::computer_control::FilteredCapture {
                    jpeg_bytes: self.encoded.clone().unwrap(),
                    source_width,
                    source_height,
                    output_width: request.output_width,
                    output_height: request.output_height,
                    geometry: self.frame_geometry().await?.unwrap(),
                }))
            }
            async fn encode_computer_jpeg(
                &self,
                png: &[u8],
            ) -> Result<Option<Vec<u8>>, ComputerError> {
                assert!(!self.native, "native capture must not be re-encoded");
                let image = image::load_from_memory(png).unwrap();
                assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
                self.calls.lock().unwrap().push(image.dimensions());
                if self.fail {
                    return Err(ComputerError::Other("native JPEG encode failed".into()));
                }
                Ok(self.encoded.clone())
            }
            async fn screenshot(&self) -> Result<Screenshot, ComputerError> {
                assert!(
                    !self.native,
                    "native capture must not fall back to raw screenshot"
                );
                Ok(Screenshot {
                    width: 2300,
                    height: 1200,
                    png_bytes: self.png.clone(),
                })
            }
            async fn zoom(
                &self,
                _: u32,
                _: u32,
                _: u32,
                _: u32,
            ) -> Result<Screenshot, ComputerError> {
                self.screenshot().await
            }
            async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
                Ok((2300, 1200))
            }
            async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
                Ok(Some(AppInfo {
                    bundle_id: "test.app".into(),
                    display_name: "Test".into(),
                }))
            }
            async fn mouse_move(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn left_click(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn right_click(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn double_click(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn type_text(&self, _: String) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn key(&self, _: String) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn scroll(&self, _: u32, _: u32, _: i32, _: i32) -> Result<(), ComputerError> {
                Ok(())
            }
        }
        let original =
            image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(2300, 1200, |x, y| {
                image::Rgb([
                    ((x * 73 + y * 11) % 256) as u8,
                    ((x + y * 97) % 256) as u8,
                    ((x * 13 + y * 31) % 256) as u8,
                ])
            }));
        let mut png = std::io::Cursor::new(Vec::new());
        original
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        let dimensions = budget_dimensions(2300, 1200);
        let mut expected = Vec::new();
        JpegEncoder::new_with_quality(&mut expected, 75)
            .encode_image(&original.resize_exact(dimensions.0, dimensions.1, FilterType::Lanczos3))
            .unwrap();
        // A generic first pass irreversibly changes these pixels.
        let generic = tool_api::util::image_budget::process_image(png.clone()).unwrap();
        assert!(generic.resized.is_some());
        let generic_bytes = base64::engine::general_purpose::STANDARD
            .decode(generic.base64)
            .unwrap();
        let mut twice = Vec::new();
        JpegEncoder::new_with_quality(&mut twice, 75)
            .encode_image(
                &image::load_from_memory(&generic_bytes)
                    .unwrap()
                    .resize_exact(dimensions.0, dimensions.1, FilterType::Lanczos3),
            )
            .unwrap();
        assert_ne!(expected, twice);
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        tool.ctx.computer_control = Some(Arc::new(LargeCapture {
            png: png.clone(),
            encoded: None,
            calls: calls.clone(),
            fail: false,
            native: false,
            native_requests: Arc::default(),
        }));
        let context = ctx();
        for action in ["screenshot", "zoom"] {
            let input = if action == "zoom" {
                json!({"region":[0,0,dimensions.0,dimensions.1]})
            } else {
                json!({})
            };
            let result = tool
                .call_display_protocol(
                    action,
                    tool_api::tool_trait::generated_builtin_result(&input),
                    context.clone(),
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            assert!(!result.is_error, "{}", wire(&result));
            let output = wire(&result);
            let image = output["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "image")
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(image["data"].as_str().unwrap())
                    .unwrap(),
                expected,
                "{action} must encode the original capture exactly once"
            );
        }
        assert_eq!(*calls.lock().unwrap(), vec![dimensions; 2]);
        calls.lock().unwrap().clear();
        let mut native_jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut native_jpeg, 60)
            .encode_image(&original.resize_exact(dimensions.0, dimensions.1, FilterType::Lanczos3))
            .unwrap();
        assert_ne!(native_jpeg, expected);
        tool.ctx.computer_control = Some(Arc::new(LargeCapture {
            png: png.clone(),
            encoded: Some(native_jpeg.clone()),
            calls: calls.clone(),
            fail: false,
            native: false,
            native_requests: Arc::default(),
        }));
        for action in ["screenshot", "zoom"] {
            let input = if action == "zoom" {
                json!({"region":[0,0,dimensions.0,dimensions.1]})
            } else {
                json!({})
            };
            let result = tool
                .call_display_protocol(
                    action,
                    tool_api::tool_trait::generated_builtin_result(&input),
                    context.clone(),
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            let output = wire(&result);
            let image = output["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "image")
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(image["data"].as_str().unwrap())
                    .unwrap(),
                native_jpeg,
                "native encoder payload must pass through unchanged"
            );
        }
        assert_eq!(*calls.lock().unwrap(), vec![dimensions; 2]);
        tool.ctx.computer_control = Some(Arc::new(LargeCapture {
            png,
            encoded: None,
            calls: calls.clone(),
            fail: true,
            native: false,
            native_requests: Arc::default(),
        }));
        let result = tool
            .call_display_protocol(
                "screenshot",
                tool_api::tool_trait::generated_builtin_result(&json!({})),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("native JPEG encode failed"));
        let native_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut native_bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut native_bytes, 60)
            .encode_image(&original.resize_exact(dimensions.0, dimensions.1, FilterType::Nearest))
            .unwrap();
        tool.ctx.computer_control = Some(Arc::new(LargeCapture {
            png: Vec::new(),
            encoded: Some(native_bytes.clone()),
            calls: calls.clone(),
            fail: false,
            native: true,
            native_requests: native_requests.clone(),
        }));
        for action in ["screenshot", "zoom"] {
            let input = if action == "zoom" {
                json!({"region":[0,0,dimensions.0,dimensions.1]})
            } else {
                json!({})
            };
            let result = tool
                .call_display_protocol(
                    action,
                    tool_api::tool_trait::generated_builtin_result(&input),
                    context.clone(),
                    tool_api::test_support::fresh_tx(),
                )
                .await
                .unwrap();
            assert!(!result.is_error, "{}", wire(&result));
            let output = wire(&result);
            let image = output["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "image")
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(image["data"].as_str().unwrap())
                    .unwrap(),
                native_bytes,
                "native filtered capture bytes must bypass every resize/encode pass"
            );
        }
        let requests = native_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].display_id, 3);
        assert_eq!(requests[0].source_rect, None);
        assert_eq!(requests[1].source_rect, Some([0., 0., 2300., 1200.]));
        assert!(requests
            .iter()
            .all(|r| (r.output_width, r.output_height) == dimensions));
        drop(requests);
        tool.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn clipboard_native_helpers_match_reviewed_325_effects_and_cleanup() {
        use lingxi_core::host::computer_control::{
            AppInfo, ComputerControl, ComputerError, Screenshot,
        };
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        };
        struct ClipboardBackend {
            config: Value,
            events: Mutex<Vec<Value>>,
            value: Mutex<String>,
            reads: AtomicUsize,
            no_tcc: AtomicBool,
            changed_focus: AtomicBool,
            read_effect: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
        }
        impl ClipboardBackend {
            fn new(config: Value) -> Self {
                Self {
                    config,
                    events: Mutex::new(vec![]),
                    value: Mutex::new("original".into()),
                    reads: AtomicUsize::new(0),
                    no_tcc: AtomicBool::new(false),
                    changed_focus: AtomicBool::new(false),
                    read_effect: Mutex::new(None),
                }
            }
            fn event(&self, value: Value) {
                self.events.lock().unwrap().push(value);
            }
        }
        #[async_trait::async_trait]
        impl ComputerControl for ClipboardBackend {
            async fn query_os_permissions(&self) -> Result<Option<(bool, bool)>, ComputerError> {
                Ok(
                    (self.config["noTcc"] == true || self.no_tcc.load(Ordering::SeqCst))
                        .then_some((false, false)),
                )
            }
            async fn read_clipboard(&self) -> Result<String, ComputerError> {
                self.event(json!(["read"]));
                if self.reads.fetch_add(1, Ordering::SeqCst) == 0
                    && self.config["firstReadFails"] == true
                {
                    return Err(ComputerError::Other("read failed".into()));
                }
                let text = self.value.lock().unwrap().clone();
                let effect = self.read_effect.lock().unwrap().take();
                if let Some(effect) = effect {
                    effect();
                }
                Ok(
                    if self.config["roundtripFails"] == true && text == "line1\nline2" {
                        "changed".into()
                    } else {
                        text
                    },
                )
            }
            async fn write_clipboard(&self, text: String) -> Result<(), ComputerError> {
                self.event(json!(["write", text]));
                if self.config["restoreFails"] == true && text == "original" {
                    return Err(ComputerError::Other("restore failed".into()));
                }
                *self.value.lock().unwrap() = text;
                Ok(())
            }
            async fn key(&self, key: String) -> Result<(), ComputerError> {
                self.event(if key == "cmd+v" {
                    json!(["keys", ["command", "v"]])
                } else {
                    json!(["key", key])
                });
                if self.config["keyFails"] == true {
                    return Err(ComputerError::Other("key failed".into()));
                }
                Ok(())
            }
            async fn type_text(&self, text: String) -> Result<(), ComputerError> {
                self.event(json!(["type",text,{"viaClipboard":false}]));
                Ok(())
            }
            async fn screenshot(&self) -> Result<Screenshot, ComputerError> {
                Err(ComputerError::Unsupported("screenshot".into()))
            }
            async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
                Ok((4, 4))
            }
            async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
                Ok(Some(AppInfo {
                    bundle_id: if self.changed_focus.load(Ordering::SeqCst) {
                        "other.app".into()
                    } else {
                        "test.app".into()
                    },
                    display_name: "Test".into(),
                }))
            }
            async fn mouse_move(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn left_click(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn right_click(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn double_click(&self, _: u32, _: u32) -> Result<(), ComputerError> {
                Ok(())
            }
            async fn scroll(&self, _: u32, _: u32, _: i32, _: i32) -> Result<(), ComputerError> {
                Ok(())
            }
        }
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/clipboard-2.1.325.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let backend = Arc::new(ClipboardBackend::new(
                case.get("config").cloned().unwrap_or_else(|| json!({})),
            ));
            let mut tool = make_tool(Arc::new(RecordingBackend::default()));
            tool.ctx.computer_control = Some(backend.clone());
            let context = ctx();
            tool.begin_computer_sequence(&context).await.unwrap();
            tool.state.lock().unwrap().grant_flags.clipboard_write = true;
            match case["kind"].as_str().unwrap() {
                "paste" => {
                    let approved = backend.frontmost_app().await.unwrap();
                    let result = tool
                        .protocol_clipboard_paste(
                            "line1\nline2",
                            approved.as_ref(),
                            &context,
                            &tool_api::test_support::fresh_tx(),
                        )
                        .await;
                    assert_eq!(result.is_err(), !case["error"].is_null(), "{case}");
                    if let Some(message) = case["error"].as_str() {
                        assert!(result.unwrap_err().to_string().contains(message));
                    }
                }
                "stash" => {
                    for clear in case["sequence"].as_array().unwrap() {
                        tool.protocol_clipboard_stash(clear.as_bool().unwrap(), &context)
                            .await
                            .unwrap();
                    }
                    let owner = ComputerTool::owner(&context).await;
                    assert_eq!(
                        tool.protocol_display
                            .lock()
                            .unwrap()
                            .clipboard_stashes
                            .get(&owner)
                            .map(|s| json!(s))
                            .unwrap_or(Value::Null),
                        case["stash"]
                    );
                }
                "type" => {
                    tool.state.lock().unwrap().grant_flags.clipboard_write =
                        case["clipboardWrite"].as_bool().unwrap();
                    let result = tool
                        .call_display_protocol(
                            "type",
                            tool_api::tool_trait::generated_builtin_result(
                                &json!({"text":"line1\nline2"}),
                            ),
                            context.clone(),
                            tool_api::test_support::fresh_tx(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(wire(&result), case["output"]);
                    if case["clipboardWrite"] == true {
                        assert!(backend
                            .events
                            .lock()
                            .unwrap()
                            .contains(&json!(["keys", ["command", "v"]])));
                        assert_eq!(*backend.value.lock().unwrap(), "original");
                    } else {
                        assert_eq!(
                            *backend.events.lock().unwrap(),
                            case["events"].as_array().unwrap().clone()
                        );
                    }
                }
                _ => unreachable!(),
            }
            if case["kind"] != "type" {
                let expected = case["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| e[0] != "sleep")
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(*backend.events.lock().unwrap(), expected, "{case}");
                assert_eq!(
                    *backend.value.lock().unwrap(),
                    case["clipboard"].as_str().unwrap()
                );
            }
            tool.cleanup_computer_inputs(&context).await.unwrap();
            if case["kind"] == "stash" {
                assert_eq!(*backend.value.lock().unwrap(), "original");
            }
        }
        // Integrate the guard with a click-only application and restoration
        // on the transition back to a full keyboard target.
        let backend = Arc::new(ClipboardBackend::new(json!({})));
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        tool.ctx.computer_control = Some(backend.clone());
        tool.state.lock().unwrap().allowed_apps[0].tier =
            super::super::permission_model::AppTier::Click;
        let context = ctx();
        tool.begin_computer_sequence(&context).await.unwrap();
        tool.protocol_clipboard_guard("left_click", &json!({"coordinate":[1,1]}), &context)
            .await
            .unwrap();
        assert_eq!(*backend.value.lock().unwrap(), "");
        tool.state.lock().unwrap().allowed_apps[0].tier =
            super::super::permission_model::AppTier::Full;
        tool.protocol_clipboard_guard("key", &json!({"text":"a"}), &context)
            .await
            .unwrap();
        assert_eq!(*backend.value.lock().unwrap(), "original");
        tool.cleanup_computer_inputs(&context).await.unwrap();
        let no_tcc = Arc::new(ClipboardBackend::new(json!({"noTcc":true})));
        let mut no_tcc_tool = make_tool(Arc::new(RecordingBackend::default()));
        no_tcc_tool.ctx.computer_control = Some(no_tcc.clone());
        no_tcc_tool.state.lock().unwrap().allowed_apps[0].tier =
            super::super::permission_model::AppTier::Click;
        assert!(no_tcc_tool
            .protocol_clipboard_guard("left_click", &json!({"coordinate":[1,1]}), &context)
            .await
            .is_err());
        assert!(no_tcc.events.lock().unwrap().is_empty());
        assert_eq!(*no_tcc.value.lock().unwrap(), "original");
        let owner = make_tool(Arc::new(RecordingBackend::default()));
        owner.begin_computer_sequence(&context).await.unwrap();
        let blocked = Arc::new(ClipboardBackend::new(json!({})));
        let mut blocked_tool = make_tool(Arc::new(RecordingBackend::default()))
            .with_lock_home(owner.lock_home.clone());
        blocked_tool.ctx.computer_control = Some(blocked.clone());
        blocked_tool.state.lock().unwrap().allowed_apps[0].tier =
            super::super::permission_model::AppTier::Click;
        assert!(blocked_tool
            .protocol_clipboard_guard("left_click", &json!({"coordinate":[1,1]}), &context)
            .await
            .is_err());
        assert!(blocked.events.lock().unwrap().is_empty());
        assert_eq!(*blocked.value.lock().unwrap(), "original");
        owner.cleanup_computer_inputs(&context).await.unwrap();

        // Interruptions while the old clipboard is being read must prevent
        // both the model's write and an unnecessary restoring write.
        for change in ["cancel", "clipboard_grant", "tcc", "focus"] {
            let backend = Arc::new(ClipboardBackend::new(json!({})));
            let mut tool = make_tool(Arc::new(RecordingBackend::default()));
            tool.ctx.computer_control = Some(backend.clone());
            tool.state.lock().unwrap().grant_flags.clipboard_write = true;
            let tool = Arc::new(tool);
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut context = ctx();
            context.cancel = Some(cancel.clone());
            tool.begin_computer_sequence(&context).await.unwrap();
            let approved = backend.frontmost_app().await.unwrap();
            let changed_backend = backend.clone();
            let changed_tool = tool.clone();
            *backend.read_effect.lock().unwrap() = Some(Box::new(move || match change {
                "cancel" => cancel.cancel(),
                "clipboard_grant" => {
                    changed_tool
                        .state
                        .lock()
                        .unwrap()
                        .grant_flags
                        .clipboard_write = false;
                }
                "tcc" => changed_backend.no_tcc.store(true, Ordering::SeqCst),
                "focus" => changed_backend.changed_focus.store(true, Ordering::SeqCst),
                _ => unreachable!(),
            }));
            let result = tool
                .protocol_clipboard_paste(
                    "line1\nline2",
                    approved.as_ref(),
                    &context,
                    &tool_api::test_support::fresh_tx(),
                )
                .await;
            assert!(
                result.is_err() || result.unwrap().is_some(),
                "{change} must abort before writing"
            );
            assert_eq!(*backend.events.lock().unwrap(), vec![json!(["read"])]);
            assert_eq!(*backend.value.lock().unwrap(), "original");
            tool.cleanup_computer_inputs(&context).await.unwrap();
        }

        // Restoring a saved user value is an undo, including when forward
        // effects have been cancelled or OS permission has been revoked.
        for change in ["cancel", "tcc"] {
            let backend = Arc::new(ClipboardBackend::new(json!({})));
            let mut tool = make_tool(Arc::new(RecordingBackend::default()));
            tool.ctx.computer_control = Some(backend.clone());
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut context = ctx();
            context.cancel = Some(cancel.clone());
            tool.begin_computer_sequence(&context).await.unwrap();
            tool.protocol_clipboard_stash(true, &context).await.unwrap();
            assert_eq!(*backend.value.lock().unwrap(), "");
            if change == "cancel" {
                cancel.cancel();
            } else {
                backend.no_tcc.store(true, Ordering::SeqCst);
            }
            tool.cleanup_computer_inputs(&context).await.unwrap();
            assert_eq!(*backend.value.lock().unwrap(), "original", "{change}");
            assert!(tool
                .protocol_display
                .lock()
                .unwrap()
                .clipboard_stashes
                .is_empty());
        }

        // A stale saved value cannot be restored after this owner has
        // released the desktop and someone else has changed the clipboard.
        let backend = Arc::new(ClipboardBackend::new(json!({})));
        let mut tool = make_tool(Arc::new(RecordingBackend::default()));
        tool.ctx.computer_control = Some(backend.clone());
        let context = ctx();
        tool.begin_computer_sequence(&context).await.unwrap();
        tool.protocol_clipboard_stash(true, &context).await.unwrap();
        tool.end_computer_sequence(&context).await.unwrap();
        *backend.value.lock().unwrap() = "outside owner".into();
        backend.events.lock().unwrap().clear();
        tool.restore_protocol_clipboard(&context).await.unwrap();
        assert_eq!(*backend.value.lock().unwrap(), "outside owner");
        assert!(backend.events.lock().unwrap().is_empty());
        assert_eq!(
            tool.protocol_display
                .lock()
                .unwrap()
                .clipboard_stashes
                .len(),
            1
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires controlled owned-window SCK reference directory"]
    async fn native_mcp_capture_matches_independent_sck_reference() {
        use lingxi_core::host::computer_control::ComputerControl;
        use tool_api::tool_trait::{PromptOptions, ToolStaticContext};
        let root = std::path::PathBuf::from(
            std::env::var_os("LINGXI_NATIVE_CAPTURE_REFERENCE_DIR")
                .expect("supply controlled reference directory"),
        );
        let config: Value =
            serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
        let backend = Arc::new(platform_macos_computer_control::MacosComputerControl::new());
        assert!(backend.supports_native_filtered_capture());
        backend
            .select_display(Some(config["displayId"].as_u64().unwrap() as u32))
            .await
            .unwrap();
        let geometry = backend.frame_geometry().await.unwrap().unwrap();
        let mut owner = make_tool(Arc::new(RecordingBackend::default()));
        owner.ctx.computer_control = Some(backend.clone());
        owner.state.lock().unwrap().grant_app(
            config["allowedBundleId"].as_str().unwrap().into(),
            super::super::permission_model::AppTier::Full,
        );
        let owner = Arc::new(owner);
        let mut registry = tool_api::ToolRegistry::new();
        super::super::protocol_tools::register(&mut registry, owner.clone());
        let tools = registry.available_tools(&ToolStaticContext::default());
        let declarations = tool_api::wire::tools_to_wire(&tools, &PromptOptions::default()).await;
        assert!(declarations
            .iter()
            .any(|t| t.name == "mcp__computer-use__screenshot"));
        let find = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name() == name)
                .unwrap()
                .clone()
        };
        let context = ctx();
        let result = find("mcp__computer-use__screenshot")
            .call(
                tool_api::tool_trait::generated_builtin_result(&json!({})),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", wire(&result));
        let image_bytes = |result: &ToolCallResult| {
            let output = wire(result);
            let block = output["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "image")
                .unwrap();
            assert_eq!(block["mimeType"], "image/jpeg");
            base64::engine::general_purpose::STANDARD
                .decode(block["data"].as_str().unwrap())
                .unwrap()
        };
        let screenshot = image_bytes(&result);
        std::fs::write(root.join("actual-screenshot.jpg"), &screenshot).unwrap();
        let actual = image::load_from_memory(&screenshot).unwrap().to_rgb8();
        let expected = image::open(root.join("screenshot.jpg")).unwrap().to_rgb8();
        let with_foreign = image::open(root.join("with-foreign.jpg"))
            .unwrap()
            .to_rgb8();
        assert_eq!(
            actual.dimensions(),
            budget_dimensions(geometry.pixel_width, geometry.pixel_height)
        );
        assert_eq!(actual.dimensions(), expected.dimensions());
        for key in ["foreignLogicalPoint", "ownedEdgeLogicalPoint"] {
            let point = config[key].as_array().unwrap();
            let x = (point[0].as_f64().unwrap()
                / (f64::from(geometry.pixel_width) / geometry.scale)
                * f64::from(actual.width()))
            .round() as u32;
            let y = (point[1].as_f64().unwrap()
                / (f64::from(geometry.pixel_height) / geometry.scale)
                * f64::from(actual.height()))
            .round() as u32;
            let crop = |image: &image::RgbImage| {
                image::imageops::crop_imm(image, x - 4, y - 4, 8, 8).to_image()
            };
            assert_eq!(
                crop(&actual),
                crop(&expected),
                "native SCK pixels differ at {key}"
            );
            if key == "foreignLogicalPoint" {
                assert_ne!(
                    crop(&expected),
                    crop(&with_foreign),
                    "controlled foreign marker must actually be visible in inclusion reference"
                );
            }
        }
        let frame = owner.display_frame(&context).await.unwrap().unwrap();
        assert_eq!(
            (frame.capture_width, frame.capture_height),
            (geometry.pixel_width, geometry.pixel_height)
        );
        assert_eq!(
            (
                frame.display_id,
                frame.origin_x,
                frame.origin_y,
                frame.scale
            ),
            (
                geometry.display_id,
                geometry.origin_x,
                geometry.origin_y,
                geometry.scale
            )
        );
        let result = find("mcp__computer-use__zoom")
            .call(
                tool_api::tool_trait::generated_builtin_result(&json!({"region":config["region"]})),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error, "{}", wire(&result));
        let zoom = image_bytes(&result);
        std::fs::write(root.join("actual-zoom.jpg"), &zoom).unwrap();
        assert_eq!(
            zoom,
            std::fs::read(root.join("zoom.jpg")).unwrap(),
            "static owned zoom must match independent SCK/AppKit bytes"
        );
        owner.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn display_capture_transcodes_actual_backend_pixels_and_retains_frame_scale() {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        let context = ctx();
        let result = tool
            .call_display_protocol(
                "screenshot",
                tool_api::tool_trait::generated_builtin_result(&json!({"scale":0.5})),
                context.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let blocks = wire(&result);
        assert_eq!(
            blocks["content"][0]["text"],
            "0.5-scale view; coordinate frame: 4x4."
        );
        assert_eq!(blocks["content"][1]["mimeType"], "image/jpeg");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(blocks["content"][1]["data"].as_str().unwrap())
            .unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!(image.dimensions(), (2, 2));
        assert!(image.to_rgb8().pixels().all(|pixel| pixel.0 == [0, 0, 0]));
        assert_eq!(
            backend
                .events()
                .iter()
                .filter(|event| *event == "screenshot")
                .count(),
            1
        );
        let frame = tool.display_frame(&context).await.unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (4, 4));
        let blocks = tool_api::tool_result_media::media_content_blocks(&result.data).unwrap();
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block.string_at("/type") == Some("image"))
                .count(),
            1
        );
        tool.cleanup_computer_inputs(&context).await.unwrap();
    }

    #[tokio::test]
    async fn batch_lowers_only_reached_items_and_preserves_prior_effects() {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        let input = Utf16JsonProjection::parse_utf16_json(
            &r#"{"actions":[{"action":"key","text":"a"},{"action":"type","text":"\ud800"},{"action":"key","text":"b"}]}"#.encode_utf16().collect::<Vec<_>>()
        ).unwrap();
        let result = tool
            .call_display_protocol(
                "computer_batch",
                input,
                ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(backend.events().iter().any(|event| event == "key:a"));
        assert!(!backend
            .events()
            .iter()
            .any(|event| event == "key:b" || event.starts_with("type:")));
        let output = wire(&result);
        assert_eq!(output["content"][0]["text"], "[1/3] key: Key pressed.");
        assert!(output["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("unpaired UTF-16 code unit"));
        assert_eq!(
            output["content"][2]["text"],
            "Batch stopped at actions[1] (type). 1 completed, 1 remaining."
        );
        tool.cleanup_computer_inputs(&ctx()).await.unwrap();
    }

    #[tokio::test]
    async fn display_preserves_utf16_until_native_lowering_without_replacing_text() {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        let input = Utf16JsonProjection::parse_utf16_json(
            &r#"{"text":"\ud800"}"#.encode_utf16().collect::<Vec<_>>(),
        )
        .unwrap();
        let result = tool
            .call_display_protocol("type", input, ctx(), tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(wire(&result)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unpaired UTF-16 code unit"));
        assert!(!backend
            .events()
            .iter()
            .any(|event| event.starts_with("type:")));
        let input = Utf16JsonProjection::parse_utf16_json(
            &r#"{"duration":0,"ignored":"\ud800"}"#.encode_utf16().collect::<Vec<_>>(),
        )
        .unwrap();
        let result = tool
            .call_display_protocol("wait", input, ctx(), tool_api::test_support::fresh_tx())
            .await
            .unwrap();
        assert_eq!(
            wire(&result),
            json!({"content":[{"type":"text","text":"Waited 0s."}]})
        );
    }
}

#[derive(Clone)]
struct ProtocolFrame {
    width: u32,
    height: u32,
    capture_width: u32,
    capture_height: u32,
    display_id: u32,
    origin_x: f64,
    origin_y: f64,
    scale: f64,
    explicitly_scaled: bool,
    policy_revision: Option<String>,
}

/// Source-replayed `men`: 28px tiles, 1568px/1568-token image budget.
pub(super) fn budget_dimensions(width: u32, height: u32) -> (u32, u32) {
    let tiles = |w: u32, h: u32| u64::from(w.div_ceil(28)) * u64::from(h.div_ceil(28));
    if width == 0 || height == 0 {
        return (width, height);
    }
    if width <= 1568 && height <= 1568 && tiles(width, height) <= 1568 {
        return (width, height);
    }
    if height > width {
        let (w, h) = budget_dimensions(height, width);
        return (h, w);
    }
    let ratio = f64::from(width) / f64::from(height);
    let (mut low, mut high) = (1, width);
    loop {
        if low + 1 == high {
            return (low, ((f64::from(low) / ratio).round() as u32).max(1));
        }
        let mid = low + (high - low) / 2;
        let h = ((f64::from(mid) / ratio).round() as u32).max(1);
        if mid <= 1568 && tiles(mid, h) <= 1568 {
            low = mid;
        } else {
            high = mid;
        }
    }
}

pub(super) fn protocol_scale(input: &Value) -> Result<Option<f64>, String> {
    let Some(value) = input.get("scale") else {
        return Ok(None);
    };
    let scale = value
        .as_f64()
        .filter(|v| v.is_finite() && (0.1..=1.0).contains(v));
    scale
        .map(Some)
        .ok_or_else(|| "scale must be a number in [0.1, 1] — e.g. 0.5 for a half-size image".into())
}

fn required_text<'a>(input: &'a Value, field: &str) -> Result<&'a str, String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("\"{field}\" must be a string."))
}

fn point(input: &Value, field: &str) -> Result<(f64, f64), String> {
    let value = input
        .get(field)
        .ok_or_else(|| format!("{field} is required"))?;
    let values = value
        .as_array()
        .filter(|values| values.len() == 2)
        .ok_or_else(|| format!("{field} must be an array of length 2"))?;
    let read = |value: &Value| value.as_f64().filter(|v| v.is_finite() && *v >= 0.0);
    read(&values[0])
        .zip(read(&values[1]))
        .ok_or_else(|| format!("{field} must be a tuple of non-negative finite numbers"))
}

fn duration(input: &Value) -> Result<f64, String> {
    let value = input
        .get("duration")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| "duration must be a number".to_owned())?;
    if value < 0.0 {
        return Err("duration must be non-negative".into());
    }
    if value > 100.0 {
        return Err("duration is too long. Duration is in seconds.".into());
    }
    Ok(value)
}

fn number_text(value: f64) -> String {
    // The shared source projection uses ryu-js, including ECMAScript's
    // exponent thresholds and -0 spelling. Rust's f64 Display is different.
    tool_api::tool_trait::generated_builtin_result(&json!(value))
        .to_json_string()
        .expect("generated protocol number is serializable")
}

fn coordinate_number(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value < i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn project_arguments(input: &Utf16JsonProjection, action: &str) -> Result<Value, String> {
    let mut output = json!({});
    let fields: &[&str] = match action {
        "screenshot" => &["scale"],
        "zoom" => &["region", "scale"],
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
            &["coordinate", "text"]
        }
        "mouse_move" => &["coordinate"],
        "left_click_drag" => &["coordinate", "start_coordinate"],
        "key" => &["text", "repeat"],
        "type" | "write_clipboard" => &["text"],
        "hold_key" => &["text", "duration"],
        "wait" => &["duration"],
        "scroll" => &["coordinate", "scroll_direction", "scroll_amount"],
        "open_application" => &["app"],
        "switch_display" => &["display"],
        _ => &[],
    };
    for field in std::iter::once("action").chain(fields.iter().copied()) {
        let Some(value) = input.get_property(field) else {
            continue;
        };
        let converted = if let Some(units) = value.code_units() {
            match String::from_utf16(units){
                Ok(value)=>json!(value),
                Err(_)=>return Err(format!("The native backend cannot encode an unpaired UTF-16 code unit in \"{field}\". No text was substituted or delivered.")),
            }
        } else if let Some(number) = value.as_number() {
            coordinate_number(number)
        } else if let Some(boolean) = value.as_bool() {
            json!(boolean)
        } else if value.is_null() {
            Value::Null
        } else if let Some(len) = value.array_len() {
            Value::Array(
                (0..len)
                    .map(|index| {
                        value.get_index(index).map_or(Value::Null, |item| {
                            if let Some(number) = item.as_number() {
                                coordinate_number(number)
                            } else if let Some(boolean) = item.as_bool() {
                                json!(boolean)
                            } else if let Some(units) = item.code_units() {
                                String::from_utf16(units)
                                    .map(Value::String)
                                    .unwrap_or(Value::Null)
                            } else {
                                Value::Null
                            }
                        })
                    })
                    .collect(),
            )
        } else {
            json!({})
        };
        output[field] = converted;
    }
    Ok(output)
}

fn protocol_zoom_region(frame: &ProtocolFrame, input: &Value) -> Result<[f64; 4], ToolError> {
    let values = input["region"]
        .as_array()
        .ok_or_else(|| ToolError::Internal("validated zoom region unavailable".into()))?;
    let value = |i: usize| {
        values[i]
            .as_f64()
            .ok_or_else(|| ToolError::Internal("validated zoom number unavailable".into()))
    };
    let (x0, y0, x1, y1) = (value(0)?, value(1)?, value(2)?, value(3)?);
    let dx = (f64::from(frame.capture_width) / frame.scale) / f64::from(frame.width);
    let dy = (f64::from(frame.capture_height) / frame.scale) / f64::from(frame.height);
    Ok([x0 * dx, y0 * dy, (x1 - x0) * dx, (y1 - y0) * dy])
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() && c != '\u{85}' || c == '\u{feff}')
}

fn safe_name(value: &str) -> Option<String> {
    static PATTERN: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r#"^[\p{L}\p{N}_ .&'"()+\-–—→⌘⌥⌃⇧]+$"#).expect("constant name pattern")
    });
    let value = js_trim(value)
        .replace(['‘', '’'], "'")
        .replace(['“', '”'], "\"")
        .replace('…', "...");
    (!value.is_empty() && value.encode_utf16().count() <= 40 && PATTERN.is_match(&value))
        .then_some(value)
}

pub(super) fn display_names(
    displays: &[lingxi_core::host::computer_control::DisplayInfo],
) -> HashMap<u32, String> {
    let mut sorted = displays.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|display| display.id);
    let mut counts = HashMap::<String, u32>::new();
    sorted
        .into_iter()
        .map(|display| {
            let label =
                safe_name(&display.name).unwrap_or_else(|| format!("display {}", display.id));
            let count = counts.entry(label.clone()).or_default();
            *count += 1;
            (
                display.id,
                if *count == 1 {
                    label
                } else {
                    format!("{label} ({count})")
                },
            )
        })
        .collect()
}

fn legacy_error(action: &str, error_value: &ToolError) -> ToolCallResult {
    let message = error_value.model_facing_message();
    let message = message.string_at("").unwrap_or("Computer operation failed");
    let kind = match error_value {
        ToolError::InvalidInput(_) => "bad_args",
        ToolError::PermissionDenied(_) if message.contains("No applications are granted") => {
            "allowlist_empty"
        }
        ToolError::PermissionDenied(_)
            if message.contains("not granted") || message.contains("grant") =>
        {
            "grant_flag_required"
        }
        ToolError::PermissionDenied(_) if message.contains("write_clipboard is blocked") => {
            "tier_insufficient"
        }
        ToolError::PermissionDenied(_) => "policy_denied",
        ToolError::Aborted => "",
        _ => "executor_threw",
    };
    if kind == "executor_threw" {
        error(&format!("Tool \"{action}\" failed: {message}"), kind)
    } else {
        error(message, kind)
    }
}

impl ComputerTool {
    /// Native teach anchors use the full screenshot coordinate frame even
    /// when the delivered JPEG was explicitly scaled down. Invalid/out-of-frame
    /// anchors are omitted; calculating an anchor never moves input focus.
    pub(super) async fn protocol_anchor(
        &self,
        anchor: &[f64],
        ctx: &ToolUseContext,
    ) -> Option<Value> {
        if anchor.len() != 2
            || anchor
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
        {
            return None;
        }
        let frame = self.display_frame(ctx).await.ok().flatten();
        let (
            capture_width,
            capture_height,
            reference_width,
            reference_height,
            scale,
            origin_x,
            origin_y,
        ) = if let Some(frame) = frame {
            if anchor[0] > f64::from(frame.width) || anchor[1] > f64::from(frame.height) {
                return None;
            }
            (
                frame.capture_width,
                frame.capture_height,
                frame.width,
                frame.height,
                frame.scale,
                frame.origin_x,
                frame.origin_y,
            )
        } else {
            let backend = self.ctx.computer_control.as_ref()?;
            let (width, height) = backend.display_size().await.ok()?;
            if anchor[0] > f64::from(width) || anchor[1] > f64::from(height) {
                return None;
            }
            let geometry = backend.frame_geometry().await.ok().flatten();
            (
                width,
                height,
                width,
                height,
                geometry.as_ref().map_or(1.0, |g| g.scale),
                geometry.as_ref().map_or(0.0, |g| g.origin_x),
                geometry.as_ref().map_or(0.0, |g| g.origin_y),
            )
        };
        if !scale.is_finite() || scale <= 0.0 || reference_width == 0 || reference_height == 0 {
            return None;
        }
        let width = f64::from(capture_width) / scale;
        let height = f64::from(capture_height) / scale;
        let x = (anchor[0] * width / f64::from(reference_width))
            .round()
            .clamp(0.0, (width - 1.0).max(0.0))
            + origin_x;
        let y = (anchor[1] * height / f64::from(reference_height))
            .round()
            .clamp(0.0, (height - 1.0).max(0.0))
            + origin_y;
        (x.is_finite() && y.is_finite())
            .then(|| json!([coordinate_number(x), coordinate_number(y)]))
    }

    pub(super) async fn call_display_protocol(
        &self,
        action: &str,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Validate only batch structure first. Preserve each exact UTF-16 item
        // until execution reaches it, so later lowering failures cannot erase
        // successful earlier effects.
        if action == "computer_batch" {
            return self.protocol_display_batch(&input, &ctx, &tx).await;
        }
        let input = match project_arguments(&input, action) {
            Ok(value) => value,
            Err(message) => return Ok(error(&message, "executor_threw")),
        };
        self.protocol_display_one(action, &input, &ctx, &tx, None)
            .await
    }

    async fn display_frame(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<Option<ProtocolFrame>, ToolError> {
        let owner = Self::owner(ctx).await;
        let revision = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .policy_revision
            .clone();
        Ok(self
            .protocol_display
            .lock()
            .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?
            .frames
            .get(&owner)
            .filter(|frame| frame.policy_revision == revision)
            .cloned())
    }

    async fn protocol_legacy(
        &self,
        action: &str,
        input: Value,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<Value, ToolError> {
        let format = if matches!(action, "screenshot" | "zoom") {
            super::CaptureFormat::Png
        } else {
            super::CaptureFormat::Generic
        };
        self.protocol_legacy_with_format(action, input, ctx, tx, format)
            .await
    }

    async fn protocol_legacy_with_format(
        &self,
        action: &str,
        mut input: Value,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
        format: super::CaptureFormat,
    ) -> Result<Value, ToolError> {
        self.protocol_clipboard_guard(action, &input, ctx).await?;
        input["action"] = Value::String(action.into());
        let result = self
            .call_with_capture_format(
                tool_api::tool_trait::generated_builtin_result(&input),
                ctx.clone(),
                tx.clone(),
                format,
            )
            .await?;
        serde_json::from_str(
            &result
                .data
                .to_json_string()
                .map_err(|e| ToolError::Internal(e.to_string()))?,
        )
        .map_err(|e| ToolError::Internal(e.to_string()))
    }

    async fn protocol_point(
        &self,
        input: &Value,
        field: &str,
        frame: Option<&ProtocolFrame>,
    ) -> Result<(u32, u32), String> {
        let (x, y) = point(input, field)?;
        let (width, height, scale) = if let Some(frame) = frame {
            if x > f64::from(frame.width) || y > f64::from(frame.height) {
                return Err(format!("{field} [{}, {}] is outside the coordinate frame ({}x{}) — coordinates are pixels in the full-resolution coordinate frame. Take a new screenshot and pick a point inside it.", number_text(x), number_text(y), frame.width, frame.height));
            }
            (frame.capture_width, frame.capture_height, frame.scale)
        } else {
            let backend = self
                .ctx
                .computer_control
                .as_ref()
                .ok_or_else(|| "computer unavailable".to_owned())?;
            let (width, height) = backend.display_size().await.map_err(|e| e.to_string())?;
            if x > f64::from(width) || y > f64::from(height) {
                return Err(format!("{field} is outside the display bounds, and there is no prior screenshot to interpret it against. Take a screenshot first."));
            }
            let scale = backend
                .frame_geometry()
                .await
                .ok()
                .flatten()
                .map_or(1.0, |g| g.scale);
            (width, height, scale)
        };
        let (frame_width, frame_height) =
            frame.map_or((width, height), |frame| (frame.width, frame.height));
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let convert = |value: f64, capture: u32, reference: u32| {
            // Native rounds logical points, then the existing backend consumes
            // capture pixels. Clamp the inclusive frame edge to the last point.
            let points = f64::from(capture) / scale;
            let logical = (value * points / f64::from(reference.max(1)))
                .round()
                .clamp(0.0, (points - 1.0).max(0.0));
            (logical * scale)
                .round()
                .clamp(0.0, f64::from(capture.saturating_sub(1))) as u32
        };
        Ok((
            convert(x, width, frame_width),
            convert(y, height, frame_height),
        ))
    }

    async fn protocol_display_one(
        &self,
        action: &str,
        input: &Value,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
        frozen: Option<&Option<ProtocolFrame>>,
    ) -> Result<ToolCallResult, ToolError> {
        let current = match frozen {
            Some(frame) => frame.clone(),
            None => self.display_frame(ctx).await?,
        };
        let mut adapted = json!({});
        if matches!(
            action,
            "left_click"
                | "double_click"
                | "triple_click"
                | "right_click"
                | "middle_click"
                | "left_click_drag"
        ) {
            let held = self
                .runtime
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
                .held_mouse;
            if held {
                if let Err(e) = self
                    .protocol_legacy("left_mouse_up", json!({}), ctx, tx)
                    .await
                {
                    return Ok(legacy_error(action, &e));
                }
            }
        }
        if matches!(
            action,
            "left_click"
                | "double_click"
                | "triple_click"
                | "right_click"
                | "middle_click"
                | "mouse_move"
                | "left_click_drag"
                | "scroll"
        ) {
            match self
                .protocol_point(input, "coordinate", current.as_ref())
                .await
            {
                Ok((x, y)) => adapted["coordinate"] = json!([x, y]),
                Err(message) => return Ok(error(&message, "bad_args")),
            }
        }
        match action {
            "screenshot" | "zoom" => {
                return self
                    .protocol_capture(action, input, current.as_ref(), ctx, tx)
                    .await
            }
            "type" => return self.protocol_type(input, ctx, tx).await,
            "cursor_position" => return self.protocol_cursor(current.as_ref(), ctx, tx).await,
            "key" => {
                let chord = match required_text(input, "text") {
                    Ok(text) => text,
                    Err(_) => return Ok(error("text is required", "bad_args")),
                };
                let repeat = match input.get("repeat") {
                    None => 1,
                    Some(value) => match value
                        .as_f64()
                        .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 1.0)
                    {
                        None => return Ok(error("repeat must be a positive integer", "bad_args")),
                        Some(n) if n > 100.0 => {
                            return Ok(error("repeat exceeds maximum of 100", "bad_args"))
                        }
                        Some(n) => n as usize,
                    },
                };
                self.protocol_clipboard_guard("key", input, ctx).await?;
                let approved = self.enforce_tier("key").await?;
                for index in 0..repeat {
                    if self.check_cancel(ctx).is_err() {
                        return Ok(error(&format!("Key repeat aborted after {index} of {repeat} presses (user interrupt)."), ""));
                    }
                    if index > 0 {
                        self.wait_cancellable(0.008, ctx).await?;
                    }
                    if let Some(block) = self
                        .protocol_focus_delivery(
                            approved.as_ref(),
                            &format!("Key repeat aborted after {index} of {repeat} presses"),
                        )
                        .await?
                    {
                        return Ok(block);
                    }
                    if let Err(e) = self
                        .protocol_legacy("key", json!({"text":chord}), ctx, tx)
                        .await
                    {
                        return Ok(legacy_error(action, &e));
                    }
                }
                return Ok(text(&if repeat > 1 {
                    format!("Key pressed {repeat} times.")
                } else {
                    "Key pressed.".into()
                }));
            }
            "wait" => {
                let seconds = match duration(input) {
                    Ok(value) => value,
                    Err(message) => return Ok(error(&message, "bad_args")),
                };
                if let Err(e) = self.wait_cancellable(seconds, ctx).await {
                    if matches!(e, ToolError::Aborted) {
                        return Ok(error("Wait aborted (user interrupt).", ""));
                    }
                    return Ok(legacy_error("wait", &e));
                }
                return Ok(text(&format!("Waited {}s.", number_text(seconds))));
            }
            "hold_key" => {
                match required_text(input, "text") {
                    Ok(value) => adapted["text"] = json!(value),
                    Err(message) => return Ok(error(&message, "bad_args")),
                }
                let seconds = match duration(input) {
                    Ok(value) => value,
                    Err(message) => return Ok(error(&message, "bad_args")),
                };
                adapted["duration"] = json!(seconds);
            }
            "open_application" => match required_text(input, "app") {
                Ok(value) => adapted["bundle_id"] = json!(value),
                Err(message) => return Ok(error(&message, "bad_args")),
            },
            "switch_display" => return self.protocol_switch_display(input, ctx, tx).await,
            "read_clipboard" => {}
            "write_clipboard" => {
                if !self
                    .state
                    .lock()
                    .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
                    .grant_flags
                    .clipboard_write
                {
                    return Ok(error("Clipboard write is not granted. Request `clipboardWrite` via request_access.","grant_flag_required"));
                }
                match required_text(input, "text") {
                    Ok(value) => adapted["text"] = json!(value),
                    Err(message) => return Ok(error(&message, "bad_args")),
                }
            }
            "scroll" => {
                let direction = input.get("scroll_direction").and_then(Value::as_str);
                if !matches!(direction, Some("up" | "down" | "left" | "right")) {
                    return Ok(error(
                        "scroll_direction must be 'up', 'down', 'left', or 'right'",
                        "bad_args",
                    ));
                }
                let amount = input.get("scroll_amount").and_then(Value::as_f64);
                let Some(amount) =
                    amount.filter(|v| v.is_finite() && v.fract() == 0.0 && *v >= 0.0)
                else {
                    return Ok(error(
                        "scroll_amount must be a non-negative int",
                        "bad_args",
                    ));
                };
                if amount > 100.0 {
                    return Ok(error("scroll_amount exceeds maximum of 100", "bad_args"));
                }
                adapted["scroll_direction"] = json!(direction);
                adapted["scroll_amount"] = json!(amount as u32);
            }
            "left_click_drag" => {
                if input.get("start_coordinate").is_some() {
                    match self
                        .protocol_point(input, "start_coordinate", current.as_ref())
                        .await
                    {
                        Ok((x, y)) => adapted["start_coordinate"] = json!([x, y]),
                        Err(message) => return Ok(error(&message, "bad_args")),
                    }
                }
            }
            "left_mouse_down" | "left_mouse_up" => {}
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
            | "mouse_move" => {}
            _ => return Ok(error(&format!("Unknown tool \"{action}\"."), "bad_args")),
        }
        let click = matches!(
            action,
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click"
        );
        let mut held = Vec::new();
        if click && input.get("text").is_some() {
            let chord = match input.get("text").and_then(Value::as_str) {
                Some(value) => value,
                None => return Ok(error("text must be a string", "bad_args")),
            };
            let flags = self
                .state
                .lock()
                .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
                .grant_flags;
            if super::key_policy::system_shortcut(chord, cfg!(target_os = "macos"))
                && !flags.system_key_combos
            {
                return Ok(error(&format!("The modifier chord \"{chord}\" would fire a system shortcut. Request the systemKeyCombos grant flag via request_access, or use only modifier keys (shift, ctrl, alt, cmd) in the text parameter."),"grant_flag_required"));
            }
            for key in chord.split('+').map(js_trim).filter(|key| !key.is_empty()) {
                match self
                    .protocol_legacy("key_down", json!({"text":key}), ctx, tx)
                    .await
                {
                    Ok(_) => held.push(super::canonical_key(key)),
                    Err(e) => {
                        self.release_keys(&held).await?;
                        return Ok(legacy_error(action, &e));
                    }
                }
            }
        }
        let result = self.protocol_legacy(action, adapted, ctx, tx).await;
        if !held.is_empty() {
            self.release_keys(&held).await?;
        }
        let data = match result {
            Ok(value) => value,
            Err(e) => return Ok(legacy_error(action, &e)),
        };
        let message = match action {
            "left_click" | "double_click" | "triple_click" | "right_click" | "middle_click" => {
                "Clicked.".into()
            }
            "mouse_move" => "Moved.".into(),
            "left_click_drag" => "Dragged.".into(),
            "scroll" => "Scrolled.".into(),
            "left_mouse_down" => "Mouse button pressed.".into(),
            "left_mouse_up" => "Mouse button released.".into(),
            "hold_key" => "Key held.".into(),
            "write_clipboard" => "Clipboard written.".into(),
            "wait" => format!("Waited {}s.", number_text(duration(input).unwrap_or(0.0))),
            "open_application" => {
                let app = input["app"].as_str().unwrap_or("");
                let count = if let Some(backend) = &self.ctx.computer_control {
                    backend.list_displays().await.map_or(1, |d| d.len())
                } else {
                    1
                };
                if count >= 2 {
                    format!("Opened \"{app}\". If it isn't visible in the next screenshot, it may have opened on a different monitor — use switch_display to check.")
                } else {
                    format!("Opened \"{app}\".")
                }
            }
            "read_clipboard" => {
                return Ok(super::protocol_result::json(
                    json!({"text":data.get("text").and_then(Value::as_str).unwrap_or("")}),
                ))
            }
            _ => return Ok(error(&format!("Unknown tool \"{action}\"."), "bad_args")),
        };
        Ok(text(&message))
    }

    async fn protocol_cursor(
        &self,
        frame: Option<&ProtocolFrame>,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let revision = self.sync_policy(ctx).await?;
        // Reuse the normal display guard/lease before this read-only global
        // query. The legacy cursor seam is local and cannot represent another monitor.
        if let Err(e) = self
            .protocol_legacy("display_size", json!({}), ctx, tx)
            .await
        {
            return Ok(legacy_error("cursor_position", &e));
        }
        let Some(backend) = &self.ctx.computer_control else {
            return Ok(error("Computer unavailable", "feature_unavailable"));
        };
        let position = backend
            .global_cursor_position()
            .await
            .map_err(|e| super::map_err(&e))?;
        let (x, y) = if let Some(point) = position {
            point
        } else {
            let data = match self
                .protocol_legacy("cursor_position", json!({}), ctx, tx)
                .await
            {
                Ok(value) => value,
                Err(e) => return Ok(legacy_error("cursor_position", &e)),
            };
            let x = data["x"].as_f64().unwrap_or(0.0);
            let y = data["y"].as_f64().unwrap_or(0.0);
            match backend
                .frame_geometry()
                .await
                .map_err(|e| super::map_err(&e))?
            {
                Some(geometry) => (
                    x / geometry.scale + geometry.origin_x,
                    y / geometry.scale + geometry.origin_y,
                ),
                None => (x, y),
            }
        };
        if !x.is_finite() || !y.is_finite() {
            return Ok(error(
                "Tool \"cursor_position\" failed: cursor position is unavailable",
                "executor_threw",
            ));
        }
        self.check_live_os_permissions(ctx).await?;
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        let value = if let Some(frame) = frame {
            let dx = x - frame.origin_x;
            let dy = y - frame.origin_y;
            let width = f64::from(frame.capture_width) / frame.scale;
            let height = f64::from(frame.capture_height) / frame.scale;
            if dx < 0.0 || dy < 0.0 || dx > width || dy > height {
                json!({"x":coordinate_number(x),"y":coordinate_number(y),"coordinateSpace":"logical_points","note":"cursor is on a different monitor than your last screenshot; take a fresh screenshot"})
            } else {
                let mut value = json!({"x":(dx*f64::from(frame.width)/width).round() as i64,"y":(dy*f64::from(frame.height)/height).round() as i64,"coordinateSpace":"image_pixels"});
                if frame.explicitly_scaled {
                    value["note"]=json!(format!("coordinates are in the {}x{} coordinate frame, not the scaled screenshot image's own pixels",frame.width,frame.height));
                }
                value
            }
        } else {
            json!({"x":coordinate_number(x),"y":coordinate_number(y),"coordinateSpace":"logical_points","note":"take a screenshot first for image-pixel coordinates"})
        };
        Ok(super::protocol_result::json(value))
    }

    async fn protocol_type(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let value = match required_text(input, "text") {
            Ok(value) => value,
            Err(message) => return Ok(error(&message, "bad_args")),
        };
        let graphemes = value.graphemes(true).collect::<Vec<_>>();
        self.protocol_clipboard_guard("type", input, ctx).await?;
        let approved = self.enforce_tier("type").await?;
        let clipboard_write = self
            .state
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .grant_flags
            .clipboard_write;
        if value.contains('\n') && clipboard_write {
            if let Some(block) = self
                .protocol_focus_delivery(approved.as_ref(), "Paste aborted before delivery")
                .await?
            {
                return Ok(block);
            }
            return match self
                .protocol_clipboard_paste(value, approved.as_ref(), ctx, tx)
                .await
            {
                Ok(None) => Ok(text("Typed (via clipboard).")),
                Ok(Some(block)) => Ok(block),
                Err(error) => Ok(legacy_error("type", &error)),
            };
        }
        for (index, grapheme) in graphemes.iter().enumerate() {
            if self.check_cancel(ctx).is_err() {
                return Ok(error(
                    &format!(
                        "Typing aborted after {index} of {} graphemes (user interrupt).",
                        graphemes.len()
                    ),
                    "",
                ));
            }
            self.wait_cancellable(0.008, ctx).await?;
            if let Some(block) = self
                .protocol_focus_delivery(
                    approved.as_ref(),
                    &format!(
                        "Typing aborted after {index} of {} graphemes",
                        graphemes.len()
                    ),
                )
                .await?
            {
                return Ok(block);
            }
            let (action, value) = match *grapheme {
                "\n" | "\r" | "\r\n" => ("key", json!({"text":"return"})),
                "\t" => ("key", json!({"text":"tab"})),
                value => ("type", json!({"text":value})),
            };
            if let Err(e) = self.protocol_legacy(action, value, ctx, tx).await {
                return Ok(legacy_error("type", &e));
            }
        }
        Ok(text(&format!("Typed {} grapheme(s).", graphemes.len())))
    }

    /// Stash is host/session owned, never a model input. Native Ue's failures
    /// are best effort; a failed restore retains the stash for a later retry.
    async fn protocol_clipboard_stash(
        &self,
        clear: bool,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let revision = self.sync_policy(ctx).await?;
        self.check_protocol_clipboard_fresh(ctx, revision.as_deref())
            .await?;
        let Some(backend) = self.ctx.computer_control.as_ref() else {
            return Ok(());
        };
        let owner = Self::owner(ctx).await;
        let previous = self
            .protocol_display
            .lock()
            .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?
            .clipboard_stashes
            .get(&owner)
            .cloned();
        if clear {
            if previous.is_none() {
                let text = backend.read_clipboard().await.unwrap_or_default();
                self.check_protocol_clipboard_fresh(ctx, revision.as_deref())
                    .await?;
                self.protocol_display
                    .lock()
                    .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?
                    .clipboard_stashes
                    .insert(owner, text);
            }
            self.check_protocol_clipboard_fresh(ctx, revision.as_deref())
                .await?;
            let _ = backend.write_clipboard(String::new()).await;
        } else if let Some(previous) = previous {
            self.check_protocol_clipboard_fresh(ctx, revision.as_deref())
                .await?;
            if backend.write_clipboard(previous).await.is_ok() {
                self.protocol_display
                    .lock()
                    .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?
                    .clipboard_stashes
                    .remove(&owner);
            }
        }
        Ok(())
    }

    async fn check_protocol_clipboard_fresh(
        &self,
        ctx: &ToolUseContext,
        revision: Option<&str>,
    ) -> Result<(), ToolError> {
        let owner = Self::owner(ctx).await;
        let owned = self.protocol_clipboard_lease_owned(&owner)?;
        if !owned {
            return Err(ToolError::PermissionDenied(
                "Clipboard effect requires this session's desktop lease.".into(),
            ));
        }
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision)?;
        self.check_live_os_permissions(ctx).await?;
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision)
    }

    fn protocol_clipboard_lease_owned(&self, owner: &str) -> Result<bool, ToolError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?;
        Ok(runtime.owner.as_deref() == Some(owner) && runtime.lease.is_some())
    }

    /// Undo an owned clipboard mutation even after cancellation or revocation.
    /// The retained desktop lease, rather than a new input grant, authorizes
    /// this restoration. Without that exact lease the saved value stays pending.
    async fn restore_owned_clipboard_value(
        &self,
        previous: String,
        ctx: &ToolUseContext,
    ) -> Result<bool, ToolError> {
        let owner = Self::owner(ctx).await;
        let Some(backend) = self.ctx.computer_control.as_ref() else {
            return Ok(false);
        };
        if !self.protocol_clipboard_lease_owned(&owner)? {
            return Ok(false);
        }
        Ok(backend.write_clipboard(previous).await.is_ok())
    }

    pub(super) async fn restore_protocol_clipboard(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let owner = Self::owner(ctx).await;
        let previous = self
            .protocol_display
            .lock()
            .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?
            .clipboard_stashes
            .get(&owner)
            .cloned();
        if let Some(previous) = previous {
            if self
                .restore_owned_clipboard_value(previous.clone(), ctx)
                .await?
            {
                let mut display = self
                    .protocol_display
                    .lock()
                    .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?;
                if display.clipboard_stashes.get(&owner) == Some(&previous) {
                    display.clipboard_stashes.remove(&owner);
                }
            }
        }
        Ok(())
    }

    async fn protocol_clipboard_guard(
        &self,
        action: &str,
        input: &Value,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let pointer = matches!(
            action,
            "mouse_move"
                | "left_click"
                | "double_click"
                | "triple_click"
                | "right_click"
                | "middle_click"
                | "scroll"
                | "left_click_drag"
                | "left_mouse_down"
        );
        let keyboard = matches!(action, "type" | "key" | "key_down" | "hold_key");
        if !(pointer || keyboard || matches!(action, "read_clipboard" | "write_clipboard")) {
            return Ok(());
        }
        if action == "read_clipboard" {
            self.require_grant_flag(
                super::permission_model::GrantFlags::clipboard_read_enabled,
                "clipboardRead",
            )?;
        }
        if action == "write_clipboard" {
            self.require_grant_flag(
                super::permission_model::GrantFlags::clipboard_write_enabled,
                "clipboardWrite",
            )?;
        }
        let revision = self.sync_policy(ctx).await?;
        self.check_cancel(ctx)?;
        self.check_live_os_permissions(ctx).await?;
        // The outer production sequence keeps the desktop file lease across
        // this prelude and the following legacy call. Standalone callers must
        // also acquire it before any clipboard effect; never nest active calls.
        let _lease = self.acquire_call(action, ctx).await?;
        let Some(backend) = self.ctx.computer_control.as_ref() else {
            return Ok(());
        };
        if backend
            .screen_locked()
            .await
            .map_err(|e| super::map_err(&e))?
        {
            return Err(ToolError::PermissionDenied("The screen is locked.".into()));
        }
        let app = if pointer && backend.supports_target_application() {
            if let Some(values) = input.get("coordinate").and_then(Value::as_array) {
                let x = values.first().and_then(Value::as_u64).unwrap_or(0) as u32;
                let y = values.get(1).and_then(Value::as_u64).unwrap_or(0) as u32;
                backend
                    .target_application(x, y)
                    .await
                    .map_err(|e| super::map_err(&e))?
            } else {
                backend
                    .frontmost_app()
                    .await
                    .map_err(|e| super::map_err(&e))?
            }
        } else {
            backend
                .frontmost_app()
                .await
                .map_err(|e| super::map_err(&e))?
        };
        let tier = app.as_ref().and_then(|app| {
            self.state
                .lock()
                .ok()?
                .allowed_apps
                .iter()
                .find(|allowed| allowed.bundle_id == app.bundle_id)
                .map(|allowed| allowed.tier)
        });
        if action == "write_clipboard" && tier == Some(super::permission_model::AppTier::Click) {
            let app = app.as_ref().expect("tier implies app");
            return Err(ToolError::PermissionDenied(format!("\"{}\" is a tier-\"click\" app and currently frontmost. write_clipboard is blocked because the next action would clear the clipboard anyway — a UI Paste button in this app cannot be used to inject text. Bring a tier-\"full\" app forward before writing to the clipboard.{}",safe_name(&app.display_name).unwrap_or_else(||app.bundle_id.clone()), " Do not attempt to work around this restriction — never use AppleScript, System Events, shell commands, or any other method to send clicks or keystrokes to this app.").into()));
        }
        self.check_policy_revision(revision.as_deref())?;
        self.check_cancel(ctx)?;
        self.check_live_os_permissions(ctx).await?;
        self.check_policy_revision(revision.as_deref())?;
        self.check_cancel(ctx)?;
        if tier == Some(super::permission_model::AppTier::Click) {
            self.protocol_clipboard_stash(true, ctx).await?;
        } else if !pointer {
            self.protocol_clipboard_stash(false, ctx).await?;
        }
        Ok(())
    }

    async fn protocol_clipboard_paste(
        &self,
        value: &str,
        approved: Option<&lingxi_core::host::computer_control::AppInfo>,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<Option<ToolCallResult>, ToolError> {
        let revision = self.sync_policy(ctx).await?;
        let input = tool_api::tool_trait::generated_builtin_result(&json!({"text":value}));
        let _lease = self.acquire_call("type", ctx).await?;
        if _lease.is_some() {
            self.ensure_escape_registration(ctx, tx).await?;
        }
        self.guard_effect("type", &input, "type", None, ctx, revision.as_deref())
            .await?;
        self.require_grant_flag(
            super::permission_model::GrantFlags::clipboard_write_enabled,
            "clipboardWrite",
        )?;
        let backend = self
            .ctx
            .computer_control
            .as_ref()
            .ok_or_else(|| ToolError::Internal("computer unavailable".into()))?;
        let previous = backend.read_clipboard().await.ok();
        let mut clipboard_write_attempted = false;
        let result = async {
            // The clipboard read is an async boundary: a user interrupt,
            // revoked grant or focus change must prevent the first write too.
            self.guard_effect("type", &input, "type", None, ctx, revision.as_deref())
                .await?;
            self.require_grant_flag(
                super::permission_model::GrantFlags::clipboard_write_enabled,
                "clipboardWrite",
            )?;
            if let Some(block) = self
                .protocol_focus_delivery(approved, "Paste aborted before delivery")
                .await?
            {
                return Ok(Some(block));
            }
            self.check_protocol_clipboard_fresh(ctx, revision.as_deref())
                .await?;
            self.require_grant_flag(
                super::permission_model::GrantFlags::clipboard_write_enabled,
                "clipboardWrite",
            )?;
            clipboard_write_attempted = true;
            backend
                .write_clipboard(value.to_owned())
                .await
                .map_err(|e| super::map_err(&e))?;
            if backend
                .read_clipboard()
                .await
                .map_err(|e| super::map_err(&e))?
                != value
            {
                return Err(ToolError::Internal(
                    "Clipboard write did not round-trip.".into(),
                ));
            }
            self.guard_effect("type", &input, "type", None, ctx, revision.as_deref())
                .await?;
            if let Some(block) = self
                .protocol_focus_delivery(approved, "Paste aborted before delivery")
                .await?
            {
                return Ok(Some(block));
            }
            self.mark_input_changed(ctx).await?;
            self.check_policy_revision(revision.as_deref())?;
            self.check_cancel(ctx)?;
            backend
                .key("cmd+v".into())
                .await
                .map_err(|e| super::map_err(&e))?;
            self.wait_cancellable(0.1, ctx).await?;
            self.check_policy_revision(revision.as_deref())?;
            self.check_live_os_permissions(ctx).await?;
            self.check_cancel(ctx)?;
            Ok(None)
        }
        .await;
        if clipboard_write_attempted {
            if let Some(previous) = previous {
                let _ = self.restore_owned_clipboard_value(previous, ctx).await;
            }
        }
        result
    }

    async fn protocol_focus_delivery(
        &self,
        approved: Option<&lingxi_core::host::computer_control::AppInfo>,
        prefix: &str,
    ) -> Result<Option<ToolCallResult>, ToolError> {
        let actual = if let Some(backend) = &self.ctx.computer_control {
            backend
                .frontmost_app()
                .await
                .map_err(|e| super::map_err(&e))?
        } else {
            None
        };
        if approved
            .zip(actual.as_ref())
            .is_some_and(|(expected, actual)| expected.bundle_id == actual.bundle_id)
        {
            return Ok(None);
        }
        let label = |app: &lingxi_core::host::computer_control::AppInfo| {
            safe_name(&app.display_name)
                .or_else(|| safe_name(&app.bundle_id))
                .unwrap_or_else(|| "(name withheld)".into())
        };
        let current = actual.as_ref().map_or_else(
            || "no app is frontmost (focus anomaly)".into(),
            |app| format!("\"{}\" became frontmost mid-delivery", label(app)),
        );
        let expected = approved.map_or_else(
            || "a different focus state".into(),
            |app| format!("\"{}\"", label(app)),
        );
        Ok(Some(error(&format!("{prefix}: {current}. Keystrokes follow key focus, so the remaining input was NOT delivered — it was approved for {expected} only. Take a screenshot to see where focus went and what was actually typed before retrying. Do not attempt to work around this restriction — never use AppleScript, System Events, shell commands, or any other method to send clicks or keystrokes to this app."),"delivery_focus_check_failed")))
    }

    async fn protocol_switch_display(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let revision = self.sync_policy(ctx).await?;
        self.check_live_os_permissions(ctx).await?;
        let name = match required_text(input, "display") {
            Ok(value) => value,
            Err(message) => return Ok(error(&message, "bad_args")),
        };
        if name.eq_ignore_ascii_case("auto") {
            let result = self
                .handle_switch_display(&tool_api::tool_trait::generated_builtin_result(
                    &json!({"display":"auto"}),
                ))
                .await;
            self.check_cancel(ctx)?;
            self.check_policy_revision(revision.as_deref())?;
            return Ok(match result {
                Ok(_) => {
                    text("Returned to automatic monitor selection. Call screenshot to continue.")
                }
                Err(e) => legacy_error("switch_display", &e),
            });
        }
        let Some(backend) = &self.ctx.computer_control else {
            return Ok(error(
                "Display switching is not available in this session.",
                "feature_unavailable",
            ));
        };
        let displays = match backend.list_displays().await {
            Ok(value) => value,
            Err(e) => {
                return Ok(error(
                    &format!("Failed to enumerate displays: {e}"),
                    "display_error",
                ))
            }
        };
        if displays.len() < 2 {
            return Ok(error(
                "Only one monitor is connected. There is nothing to switch to.",
                "bad_args",
            ));
        }
        let names = display_names(&displays);
        let Some(display) = displays.iter().find(|display| {
            names
                .get(&display.id)
                .is_some_and(|label| label.to_lowercase() == name.to_lowercase())
        }) else {
            let available = displays
                .iter()
                .filter_map(|d| names.get(&d.id))
                .map(|label| format!("\"{label}\""))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(error(
                &format!(
                    "No monitor named \"{name}\" is connected. Available monitors: {available}."
                ),
                "bad_args",
            ));
        };
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        backend
            .select_display(Some(display.id))
            .await
            .map_err(|e| super::map_err(&e))?;
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        self.state
            .lock()
            .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
            .pin_display(
                display.id,
                names.get(&display.id).map_or("", String::as_str),
            );
        Ok(text(&format!(
            "Switched to monitor \"{}\". Call screenshot to see it.",
            names[&display.id]
        )))
    }

    async fn protocol_capture(
        &self,
        action: &str,
        input: &Value,
        frame: Option<&ProtocolFrame>,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let revision = self.sync_policy(ctx).await?;
        let mut adapted = json!({});
        if action == "zoom" {
            let Some(region) = input
                .get("region")
                .and_then(Value::as_array)
                .filter(|values| values.len() == 4)
            else {
                return Ok(error(
                    "region must be an array of length 4: [x0, y0, x1, y1]",
                    "bad_args",
                ));
            };
            let values = region
                .iter()
                .map(|v| v.as_f64().filter(|n| n.is_finite() && *n >= 0.0))
                .collect::<Option<Vec<_>>>();
            let Some(values) = values else {
                return Ok(error(
                    "region values must be non-negative numbers",
                    "bad_args",
                ));
            };
            if values[2] <= values[0] {
                return Ok(error("region x1 must be greater than x0", "bad_args"));
            }
            if values[3] <= values[1] {
                return Ok(error("region y1 must be greater than y0", "bad_args"));
            }
            let Some(frame) = frame else {
                return Ok(error(
                    "take a screenshot before zooming (region coords are relative to it)",
                    "state_conflict",
                ));
            };
            if values[2] > f64::from(frame.width) || values[3] > f64::from(frame.height) {
                return Ok(error(
                    &format!(
                        "region exceeds the coordinate frame ({}×{})",
                        frame.width, frame.height
                    ),
                    "bad_args",
                ));
            }
            let x = |value: f64| {
                (value * f64::from(frame.capture_width) / f64::from(frame.width.max(1))).round()
                    as u32
            };
            let y = |value: f64| {
                (value * f64::from(frame.capture_height) / f64::from(frame.height.max(1))).round()
                    as u32
            };
            adapted["region"] = json!([x(values[0]), y(values[1]), x(values[2]), y(values[3])]);
        }
        let scale = match protocol_scale(input) {
            Ok(value) => value,
            Err(message) => return Ok(error(&message, "bad_args")),
        };
        let native_region = if action == "zoom" {
            Some(protocol_zoom_region(
                frame.expect("validated screenshot frame"),
                input,
            )?)
        } else {
            None
        };
        let (requested_source_width, requested_source_height) = if let Some(region) = native_region
        {
            let scale = frame.expect("validated screenshot frame").scale;
            (
                (region[2] * scale).round().max(1.) as u32,
                (region[3] * scale).round().max(1.) as u32,
            )
        } else {
            let size = self
                .protocol_legacy("display_size", json!({}), ctx, tx)
                .await?;
            (
                size["width"].as_u64().unwrap_or(0) as u32,
                size["height"].as_u64().unwrap_or(0) as u32,
            )
        };
        let (budget_width, budget_height) =
            budget_dimensions(requested_source_width, requested_source_height);
        let format = super::CaptureFormat::Native {
            region: native_region,
            width: (f64::from(budget_width) * scale.unwrap_or(1.0))
                .round()
                .max(1.0) as u32,
            height: (f64::from(budget_height) * scale.unwrap_or(1.0))
                .round()
                .max(1.0) as u32,
        };
        let mut data = match self
            .protocol_legacy_with_format(action, adapted, ctx, tx, format)
            .await
        {
            Ok(value) => value,
            Err(e) => return Ok(legacy_error(action, &e)),
        };
        let Some(encoded) = data.pointer("/file/base64").and_then(Value::as_str) else {
            return Err(ToolError::Internal(
                "captured image bytes unavailable".into(),
            ));
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| ToolError::Internal(e.to_string()))?;
        let actual =
            image::load_from_memory(&bytes).map_err(|e| ToolError::Internal(e.to_string()))?;
        let source_width = data
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(actual.width());
        let source_height = data
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(actual.height());
        let (reference_width, reference_height) = budget_dimensions(source_width, source_height);
        let factor = scale.unwrap_or(1.0);
        let target_width = (f64::from(reference_width) * factor).round().max(1.0) as u32;
        let target_height = (f64::from(reference_height) * factor).round().max(1.0) as u32;
        let actual = if actual.dimensions() == (target_width, target_height) {
            actual
        } else {
            actual.resize_exact(target_width, target_height, FilterType::Lanczos3)
        };
        let jpeg = if data["native_mcp_encoded"] == true {
            if actual.dimensions() != (target_width, target_height) {
                return Err(ToolError::Internal(
                    "Native capture returned unexpected JPEG dimensions".into(),
                ));
            }
            bytes
        } else {
            // Keep the native platform encoder's JPEG bytes intact. Only an
            // explicitly unavailable encoder uses the portable quality-75 path;
            // native failures remain visible instead of silently changing codecs.
            let mut png = std::io::Cursor::new(Vec::new());
            actual
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|e| ToolError::Internal(e.to_string()))?;
            let native = match self.ctx.computer_control.as_ref() {
                Some(backend) => backend
                    .encode_computer_jpeg(png.get_ref())
                    .await
                    .map_err(|e| super::map_err(&e))?,
                None => None,
            };
            let jpeg = match native {
                Some(jpeg) => jpeg,
                None => {
                    let mut jpeg = Vec::new();
                    JpegEncoder::new_with_quality(&mut jpeg, 75)
                        .encode_image(&actual)
                        .map_err(|e| ToolError::Internal(e.to_string()))?;
                    jpeg
                }
            };
            jpeg
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
        let mut content = Vec::new();
        let mut pending_frame = None;
        if action == "screenshot" {
            let owner = Self::owner(ctx).await;
            let previous = self.display_frame(ctx).await?;
            let metadata = data
                .get("computer_frame")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let id = metadata
                .get("display_id")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let captured = ProtocolFrame {
                width: reference_width,
                height: reference_height,
                capture_width: source_width,
                capture_height: source_height,
                display_id: id,
                origin_x: metadata
                    .pointer("/origin/0")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                origin_y: metadata
                    .pointer("/origin/1")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                scale: metadata.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                explicitly_scaled: factor < 1.0,
                policy_revision: self
                    .state
                    .lock()
                    .map_err(|_| ToolError::Internal("computer state poisoned".into()))?
                    .policy_revision
                    .clone(),
            };
            if let Some(backend) = &self.ctx.computer_control {
                if let Ok(displays) = backend.list_displays().await {
                    if displays.len() >= 2 {
                        let labels = display_names(&displays);
                        let label = |id| {
                            labels
                                .get(&id)
                                .cloned()
                                .unwrap_or_else(|| format!("display {id}"))
                        };
                        let others = displays
                            .iter()
                            .filter(|d| d.id != id)
                            .map(|d| format!("\"{}\"", label(d.id)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let suffix = if others.is_empty() {
                            String::new()
                        } else {
                            format!(" Other attached monitors: {others}. Use switch_display to capture a different monitor.")
                        };
                        let note = if previous.as_ref().is_none_or(|f| f.display_id == 0) {
                            Some(format!(
                                "This screenshot was taken on monitor \"{}\".{suffix}",
                                label(id)
                            ))
                        } else if previous.as_ref().is_some_and(|f| f.display_id != id) {
                            Some(format!("This screenshot was taken on monitor \"{}\", which is different from your previous screenshot (taken on \"{}\").{suffix}",label(id),label(previous.as_ref().unwrap().display_id)))
                        } else {
                            None
                        };
                        if let Some(note) = note {
                            content.push(json!({"type":"text","text":note}));
                        }
                    }
                }
            }
            if factor < 1.0 {
                content.push(json!({"type":"text","text":format!("{}-scale view; coordinate frame: {reference_width}x{reference_height}.",number_text(factor))}));
            }
            // Bind native model-observation tracking to the actual final JPEG,
            // not to the temporary legacy PNG returned by the guarded capture.
            data["file"] = json!({"base64":encoded,"type":"image/jpeg","dimensions":{"originalWidth":source_width,"originalHeight":source_height,"displayWidth":target_width,"displayHeight":target_height}});
            self.record_frame(&mut data, ctx).await?;
            pending_frame = Some((owner, captured));
        }
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        self.check_live_os_permissions(ctx).await?;
        self.check_cancel(ctx)?;
        self.check_policy_revision(revision.as_deref())?;
        if let Some((owner, captured)) = pending_frame {
            self.protocol_display
                .lock()
                .map_err(|_| ToolError::Internal("computer display state poisoned".into()))?
                .frames
                .insert(owner, captured);
        }
        content.push(json!({"type":"image","data":encoded,"mimeType":"image/jpeg"}));
        Ok(from_wire(json!({"content":content})))
    }

    async fn protocol_display_batch(
        &self,
        input: &Utf16JsonProjection,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        const ALLOWED: &[&str] = &[
            "key",
            "type",
            "mouse_move",
            "left_click",
            "left_click_drag",
            "right_click",
            "middle_click",
            "double_click",
            "triple_click",
            "scroll",
            "hold_key",
            "screenshot",
            "zoom",
            "cursor_position",
            "left_mouse_down",
            "left_mouse_up",
            "wait",
        ];
        let Some(source) = input.get_property("actions") else {
            return Ok(error("actions must be a non-empty array", "bad_args"));
        };
        let Some(count) = source.array_len().filter(|count| *count > 0) else {
            return Ok(error("actions must be a non-empty array", "bad_args"));
        };
        let mut actions = Vec::with_capacity(count);
        for index in 0..count {
            let item = source.get_index(index).expect("array index in range");
            if item.object_entries().is_none() {
                return Ok(error(
                    &format!("actions[{index}] must be an object"),
                    "bad_args",
                ));
            }
            let Some(action_value) = item.get_property("action") else {
                return Ok(error(
                    &format!("actions[{index}].action must be a string"),
                    "bad_args",
                ));
            };
            let Some(units) = action_value.code_units() else {
                return Ok(error(
                    &format!("actions[{index}].action must be a string"),
                    "bad_args",
                ));
            };
            let action = match String::from_utf16(units) {
                Ok(action) => action,
                Err(_) => return Ok(error("The native backend cannot encode an unpaired UTF-16 code unit in \"action\". No text was substituted or delivered.", "executor_threw")),
            };
            if !ALLOWED.contains(&action.as_str()) {
                return Ok(error(&format!("actions[{index}].action=\"{action}\" is not allowed in a batch. Allowed: {}.",ALLOWED.join(", ")),"bad_args"));
            }
            actions.push((
                action,
                item.to_projection()
                    .map_err(|e| ToolError::Internal(e.to_string()))?,
            ));
        }
        let frozen = self.display_frame(ctx).await?;
        let mut results = Vec::new();
        let mut failed = None;
        for (index, (action, item)) in actions.iter().enumerate() {
            if self.check_cancel(ctx).is_err() {
                self.release_owned_inputs(ctx).await?;
                return Ok(error(
                    &format!(
                        "Batch aborted after {} of {} actions (user interrupt).",
                        results.len(),
                        actions.len()
                    ),
                    "",
                ));
            }
            if index > 0 {
                self.wait_cancellable(0.01, ctx).await?;
            }
            let result = match project_arguments(item, action) {
                Err(message) => wire(&error(&message, "executor_threw")),
                Ok(arguments) => match self
                    .protocol_display_one(action, &arguments, ctx, tx, Some(&frozen))
                    .await
                {
                    Ok(result) => wire(&result),
                    Err(e) => wire(&error(
                        &format!(
                            "{action} threw: {}",
                            e.model_facing_message()
                                .string_at("")
                                .unwrap_or("operation failed")
                        ),
                        "executor_threw",
                    )),
                },
            };
            let is_error = result.get("isError").and_then(Value::as_bool) == Some(true);
            results.push((action.to_owned(), result));
            if is_error {
                self.release_owned_inputs(ctx).await?;
                failed = Some(index);
                break;
            }
        }
        let mut content = Vec::new();
        for (index, (action, result)) in results.iter().enumerate() {
            let blocks = result
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let messages = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .map(js_trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            let images = blocks
                .iter()
                .filter(|b| b["type"] == "image")
                .cloned()
                .collect::<Vec<_>>();
            let message = if messages.is_empty() {
                "ok".into()
            } else {
                messages.join("\n")
            };
            let prefix = if result.get("isError").and_then(Value::as_bool) == Some(true) {
                "FAILED — "
            } else {
                ""
            };
            let omitted = if failed.is_some() && !images.is_empty() {
                " [Image omitted due to error]"
            } else {
                ""
            };
            content.push(json!({"type":"text","text":format!("[{}/{}] {action}: {prefix}{message}{omitted}",index+1,actions.len())}));
            if failed.is_none() {
                content.extend(images);
            }
        }
        let mut result = json!({"content":content});
        if let Some(index) = failed {
            result["content"].as_array_mut().expect("generated array").push(json!({"type":"text","text":format!("Batch stopped at actions[{index}] ({}). {} completed, {} remaining.",results[index].0,index,actions.len()-index-1)}));
            result["isError"] = json!(true);
        }
        Ok(from_wire(result))
    }
}
