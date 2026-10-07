use super::*;
use lingxi_core::host::computer_control::{
    AppInfo, ComputerBackendCapabilities, ComputerControl, ComputerFrameGeometry, Screenshot,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingBackend {
    events: Mutex<Vec<String>>,
    geometry: AtomicU64,
    desktop_scope: Option<std::path::PathBuf>,
    mouse_release_failures: AtomicU64,
    chord_failure: std::sync::atomic::AtomicBool,
    key_cleanup_failures: AtomicU64,
    key_cleanup_unsupported: bool,
    click_failure: std::sync::atomic::AtomicBool,
    text_failure: std::sync::atomic::AtomicBool,
    button_cleanup_failures: AtomicU64,
}
impl RecordingBackend {
    fn event(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}
#[async_trait]
impl ComputerControl for RecordingBackend {
    fn desktop_lock_scope(&self) -> Option<std::path::PathBuf> {
        self.desktop_scope.clone()
    }
    fn capabilities(&self) -> ComputerBackendCapabilities {
        ComputerBackendCapabilities {
            held_keys: !self.key_cleanup_unsupported,
            pixel_scroll: true,
            side_buttons: true,
            frame_geometry: true,
        }
    }
    async fn frame_geometry(&self) -> Result<Option<ComputerFrameGeometry>, ComputerError> {
        Ok(Some(ComputerFrameGeometry {
            display_id: 1,
            pixel_width: 4,
            pixel_height: 4,
            origin_x: -100.0,
            origin_y: 0.0,
            scale: 2.0,
            version: format!("fake:{}", self.geometry.load(Ordering::Relaxed)),
        }))
    }
    async fn validate_keys(&self, keys: &[String]) -> Result<(), ComputerError> {
        if keys.iter().any(|k| k == "unknown") {
            Err(ComputerError::Other("unknown key".into()))
        } else {
            Ok(())
        }
    }
    async fn screenshot(&self) -> Result<Screenshot, ComputerError> {
        let mut bytes = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        self.event("screenshot".into());
        Ok(Screenshot {
            width: 4,
            height: 4,
            png_bytes: bytes.into_inner(),
        })
    }
    async fn zoom(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Screenshot, ComputerError> {
        self.event(format!("zoom:{x},{y}:{width},{height}"));
        let mut bytes = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        Ok(Screenshot {
            width,
            height,
            png_bytes: bytes.into_inner(),
        })
    }
    async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
        Ok((4, 4))
    }
    async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
        Ok(Some(AppInfo {
            bundle_id: "test.app".into(),
            display_name: "Test".into(),
        }))
    }
    async fn mouse_move(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.event(format!("move:{x},{y}"));
        Ok(())
    }
    async fn left_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.event(format!("click:{x},{y}"));
        Ok(())
    }
    async fn right_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.event(format!("right:{x},{y}"));
        Ok(())
    }
    async fn double_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.event(format!("double:{x},{y}"));
        Ok(())
    }
    async fn mouse_click(&self, x: u32, y: u32, button: &str) -> Result<(), ComputerError> {
        self.event(format!("{button}:{x},{y}"));
        if self.click_failure.load(Ordering::SeqCst) {
            return Err(ComputerError::Other("click release failed".into()));
        }
        Ok(())
    }
    async fn release_held_buttons(&self) -> Result<(), ComputerError> {
        self.event("cleanup_buttons".into());
        if self
            .button_cleanup_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ComputerError::Other("button cleanup failed".into()));
        }
        Ok(())
    }
    async fn mouse_down(&self) -> Result<(), ComputerError> {
        self.event("mouse_down".into());
        Ok(())
    }
    async fn mouse_up(&self) -> Result<(), ComputerError> {
        self.event("mouse_up".into());
        if self
            .mouse_release_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ComputerError::Other("mouse release failed".into()));
        }
        Ok(())
    }
    async fn type_text(&self, text: String) -> Result<(), ComputerError> {
        self.event(format!("type:{text}"));
        if self.text_failure.swap(false, Ordering::SeqCst) {
            return Err(ComputerError::Other("typed Tab release failed".into()));
        }
        Ok(())
    }
    async fn key(&self, key: String) -> Result<(), ComputerError> {
        self.event(format!("key:{key}"));
        if self.chord_failure.swap(false, Ordering::SeqCst) {
            return Err(ComputerError::Other("main key release failed".into()));
        }
        Ok(())
    }
    async fn key_chord(&self, keys: Vec<String>) -> Result<(), ComputerError> {
        self.event(format!("chord:{}", keys.join(",")));
        if self.chord_failure.swap(false, Ordering::SeqCst) {
            return Err(ComputerError::Other("modifier release failed".into()));
        }
        Ok(())
    }
    async fn release_held_keys(&self) -> Result<(), ComputerError> {
        if self.key_cleanup_unsupported {
            return Err(ComputerError::Unsupported("held keys".into()));
        }
        self.event("cleanup_keys".into());
        if self
            .key_cleanup_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ComputerError::Other("key cleanup failed".into()));
        }
        Ok(())
    }
    async fn key_down(&self, key: String) -> Result<(), ComputerError> {
        self.event(format!("down:{key}"));
        Ok(())
    }
    async fn key_up(&self, key: String) -> Result<(), ComputerError> {
        self.event(format!("up:{key}"));
        Ok(())
    }
    async fn scroll(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        self.event(format!("ticks:{x},{y}:{dx},{dy}"));
        Ok(())
    }
    async fn scroll_pixels(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        self.event(format!("pixels:{x},{y}:{dx},{dy}"));
        Ok(())
    }
    async fn cursor_position(&self) -> Result<(u32, u32), ComputerError> {
        Ok((2, 3))
    }
}
fn make_tool(cc: Arc<RecordingBackend>) -> ComputerTool {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let fs = tool_api::test_support::make_dummy_fs();
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let mut context = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
    context.computer_control = Some(cc);
    let tool = ComputerTool::with_access_resolver(context, Arc::new(AutoGrantResolver))
        .with_lock_home(std::env::temp_dir().join(format!(
            "computer-new-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )));
    tool.state
        .lock()
        .unwrap()
        .grant_app("test.app".into(), AppTier::Full);
    tool
}
async fn invoke(
    tool: &ComputerTool,
    input: Value,
    ctx: ToolUseContext,
) -> Result<ToolCallResult, ToolError> {
    let result = tool
        .call(input, ctx.clone(), tool_api::test_support::fresh_tx())
        .await?;
    let blocks = tool_api::tool_result_media::image_content_blocks(&result.data);
    tool.computer_model_output(
        &ctx,
        result.model_content.as_deref().unwrap_or_default(),
        blocks.as_deref(),
    )
    .await?;
    Ok(result)
}

fn ctx() -> ToolUseContext {
    tool_api::test_support::fresh_ctx()
}

#[tokio::test]
async fn typed_tabs_obey_held_key_grants_before_any_text_is_emitted() {
    for text in [
        "\t".to_owned(),
        format!("{}\t", "a".repeat(20)),
        "prefix\tsuffix".into(),
    ] {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        invoke(&tool, json!({"action":"key_down","text":"cmd"}), ctx())
            .await
            .unwrap();
        let result = invoke(&tool, json!({"action":"type","text":text}), ctx()).await;
        assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
        assert_eq!(backend.events(), ["down:cmd", "up:cmd"]);
    }
    for command_held in [false, true] {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        if command_held {
            tool.state.lock().unwrap().grant_flags.system_key_combos = true;
            invoke(&tool, json!({"action":"key_down","text":"cmd"}), ctx())
                .await
                .unwrap();
        }
        invoke(&tool, json!({"action":"type","text":"\t"}), ctx())
            .await
            .unwrap();
        assert!(backend.events().contains(&"type:\t".into()));
        tool.cleanup_computer_inputs(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn held_left_button_rejects_compound_left_clicks_and_cleans_up_the_hold() {
    for input in [
        json!({"action":"left_click","coordinate":[1,1]}),
        json!({"action":"double_click","coordinate":[1,1]}),
        json!({"action":"triple_click","coordinate":[1,1]}),
        json!({"action":"mouse_click","button":"left","coordinate":[1,1],"modifiers":["shift"]}),
    ] {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone());
        let owner = ctx();
        invoke(&tool, json!({"action":"left_mouse_down"}), owner.clone())
            .await
            .unwrap();
        let generation = tool.desktop_generation().unwrap();
        assert!(matches!(
            invoke(&tool, input, owner.clone()).await,
            Err(ToolError::InvalidInput(_))
        ));
        // Ordinary failure cleanup releases the old hold, never posts the
        // rejected click or its modifiers, and keeps both ledgers consistent.
        assert_eq!(backend.events(), ["mouse_down", "mouse_up"]);
        assert!(!tool.runtime.lock().unwrap().held_mouse);
        assert_eq!(tool.desktop_generation().unwrap(), generation + 1);
    }
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    let owner = ctx();
    invoke(&tool, json!({"action":"left_mouse_down"}), owner.clone())
        .await
        .unwrap();
    invoke(
        &tool,
        json!({"action":"mouse_click","button":"right","coordinate":[1,1]}),
        owner.clone(),
    )
    .await
    .unwrap();
    invoke(
        &tool,
        json!({"action":"mouse_move","coordinate":[2,2]}),
        owner.clone(),
    )
    .await
    .unwrap();
    assert!(tool.runtime.lock().unwrap().held_mouse);
    invoke(&tool, json!({"action":"left_mouse_up"}), owner.clone())
        .await
        .unwrap();
    assert!(!tool.runtime.lock().unwrap().held_mouse);
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        owner,
    )
    .await
    .unwrap();
    assert_eq!(
        backend.events(),
        [
            "mouse_down",
            "right:1,1",
            "move:2,2",
            "mouse_up",
            "click:1,1"
        ]
    );
}

#[tokio::test]
async fn computer_batch_delivers_ordered_images_and_only_adopts_the_final_full_observation() {
    for (actions, image_count, ready) in [
        (json!([{"action":"screenshot"}]), 1, true),
        (
            json!([{"action":"left_click","coordinate":[1,1]},{"action":"screenshot"}]),
            1,
            true,
        ),
        (
            json!([{"action":"screenshot"},{"action":"left_click","coordinate":[1,1]},{"action":"screenshot"}]),
            2,
            true,
        ),
        (
            json!([{"action":"screenshot"},{"action":"left_click","coordinate":[1,1]}]),
            1,
            false,
        ),
        (
            json!([{"action":"screenshot"},{"action":"zoom","region":[0,0,2,2]}]),
            2,
            false,
        ),
    ] {
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend);
        let owner = ctx();
        let result = tool
            .call(
                json!({"action":"computer_batch","actions":actions}),
                owner.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        assert!(result.data.get("results").is_some());
        let text = result
            .model_content
            .as_deref()
            .expect("batch media needs safe metadata text");
        assert!(
            !text.contains("base64"),
            "image bytes must not also be JSON text"
        );
        let blocks = tool_api::tool_result_media::media_content_blocks_for_tool(
            "computer",
            &result.data,
            text,
        )
        .expect("nested computer images must reach the ordinary media mapper");
        assert_eq!(
            blocks.iter().filter(|b| b["type"] == "image").count(),
            image_count
        );
        assert!(tool.native_computer_frame(&owner).await.unwrap().is_none());
        tool.computer_model_output(&owner, text, Some(&blocks))
            .await
            .unwrap();
        assert_eq!(
            tool.native_computer_frame(&owner).await.unwrap().is_some(),
            ready,
            "{text}"
        );
    }
}

#[tokio::test]
async fn batch_observation_rejects_a_removed_final_image_or_rewritten_geometry() {
    for change_geometry in [false, true] {
        let tool = make_tool(Arc::new(RecordingBackend::default()));
        let owner = ctx();
        let result = tool.call(json!({"action":"computer_batch","actions":[{"action":"screenshot"},{"action":"screenshot"}]}), owner.clone(), tool_api::test_support::fresh_tx())
            .await.unwrap();
        let mut text = result.model_content.unwrap();
        let mut blocks = tool_api::tool_result_media::media_content_blocks_for_tool(
            "computer",
            &result.data,
            &text,
        )
        .unwrap();
        if change_geometry {
            let mut metadata: Value = serde_json::from_str(&text).unwrap();
            metadata["results"][1]["result"]["computer_frame"]["width"] = json!(1);
            text = metadata.to_string();
        } else {
            // The two screenshots have identical pixels. An earlier image
            // alone still cannot stand in for the batch's final capture.
            blocks.pop();
        }
        tool.computer_model_output(&owner, &text, Some(&blocks))
            .await
            .unwrap();
        assert!(tool.native_computer_frame(&owner).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn failed_typed_tab_retains_dirty_keys_and_lease_until_cleanup_succeeds() {
    let backend = Arc::new(RecordingBackend::default());
    backend.text_failure.store(true, Ordering::SeqCst);
    backend.key_cleanup_failures.store(1, Ordering::SeqCst);
    let tool = make_tool(backend.clone());
    let owner = ctx();
    assert!(invoke(
        &tool,
        json!({"action":"type","text":"\tremaining"}),
        owner.clone()
    )
    .await
    .is_err());
    {
        let runtime = tool.runtime.lock().unwrap();
        assert!(runtime.backend_inputs_dirty);
        assert!(runtime.lease.is_some());
    }
    assert_eq!(backend.events(), ["type:\tremaining", "cleanup_keys"]);
    tool.cleanup_computer_inputs(&owner).await.unwrap();
    let runtime = tool.runtime.lock().unwrap();
    assert!(!runtime.backend_inputs_dirty);
    assert!(runtime.lease.is_none());
    assert_eq!(
        backend.events(),
        ["type:\tremaining", "cleanup_keys", "cleanup_keys"]
    );
}

#[tokio::test]
async fn untracked_text_failure_does_not_require_an_unsupported_key_cleanup() {
    let backend = Arc::new(RecordingBackend {
        key_cleanup_unsupported: true,
        ..Default::default()
    });
    backend.text_failure.store(true, Ordering::SeqCst);
    let tool = make_tool(backend.clone());
    let owner = ctx();
    let error = invoke(
        &tool,
        json!({"action":"type","text":"ordinary text"}),
        owner.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("typed Tab release failed"));
    assert!(!tool.runtime.lock().unwrap().backend_inputs_dirty);
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    invoke(&tool, json!({"action":"screenshot"}), owner)
        .await
        .unwrap();
    assert_eq!(backend.events(), ["type:ordinary text", "screenshot"]);
}

#[tokio::test]
async fn cleanup_revokes_current_and_frozen_observations_and_advances_shared_generation() {
    use lingxi_llm_client::protocol::computer::{ComputerOperation, ComputerPoint};
    for sequence in [false, true] {
        let backend = Arc::new(RecordingBackend {
            desktop_scope: Some(tempfile_scope()),
            ..Default::default()
        });
        let tool = make_tool(backend.clone());
        let peer = make_tool(backend.clone());
        let owner = ctx();
        if sequence {
            tool.begin_computer_sequence(&owner).await.unwrap();
        }
        invoke(&tool, json!({"action":"left_mouse_down"}), owner.clone())
            .await
            .unwrap();
        invoke(&tool, json!({"action":"screenshot"}), owner.clone())
            .await
            .unwrap();
        let frame = tool.native_computer_frame(&owner).await.unwrap().unwrap();
        let operation = ComputerOperation::Move {
            point: ComputerPoint { x: 1.0, y: 1.0 },
            modifiers: vec![],
        };
        let lowered = tool.lower_computer_operation(&operation, &frame).unwrap();
        let generation = tool.desktop_generation().unwrap();
        tool.cleanup_computer_inputs(&owner).await.unwrap();
        assert!(tool.native_computer_frame(&owner).await.unwrap().is_none());
        assert!(tool.lower_computer_operation(&operation, &frame).is_err());
        assert!(invoke(&tool, lowered, owner.clone()).await.is_err());
        if sequence {
            tool.end_computer_sequence(&owner).await.unwrap();
        }
        assert_eq!(peer.desktop_generation().unwrap(), generation + 1);
        assert_eq!(backend.events(), ["mouse_down", "screenshot", "mouse_up"]);
    }
}

#[tokio::test]
async fn failed_cleanup_invalidates_observation_and_retains_lease_until_retry() {
    let backend = Arc::new(RecordingBackend {
        mouse_release_failures: AtomicU64::new(1),
        ..Default::default()
    });
    let tool = make_tool(backend.clone());
    let owner = ctx();
    invoke(&tool, json!({"action":"left_mouse_down"}), owner.clone())
        .await
        .unwrap();
    invoke(&tool, json!({"action":"screenshot"}), owner.clone())
        .await
        .unwrap();
    let generation = tool.desktop_generation().unwrap();
    assert!(tool.cleanup_computer_inputs(&owner).await.is_err());
    assert_eq!(tool.desktop_generation().unwrap(), generation + 1);
    assert!(tool.native_computer_frame(&owner).await.unwrap().is_none());
    assert!(tool.runtime.lock().unwrap().lease.is_some());
    tool.cleanup_computer_inputs(&owner).await.unwrap();
    assert_eq!(tool.desktop_generation().unwrap(), generation + 2);
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    assert_eq!(
        backend.events(),
        ["mouse_down", "screenshot", "mouse_up", "mouse_up"]
    );
}

#[tokio::test]
async fn cleanup_releases_inputs_even_when_generation_update_fails_and_quarantines_the_lease() {
    let backend = Arc::new(RecordingBackend {
        key_cleanup_unsupported: true,
        ..Default::default()
    });
    let tool = make_tool(backend.clone());
    let owner = ctx();
    invoke(&tool, json!({"action":"left_mouse_down"}), owner.clone())
        .await
        .unwrap();
    let lock_file = tool.lock_home.join("computer-use.atomic.lock");
    std::fs::write(&lock_file, u64::MAX.to_le_bytes()).unwrap();
    invoke(&tool, json!({"action":"screenshot"}), owner.clone())
        .await
        .unwrap();
    assert!(tool.cleanup_computer_inputs(&owner).await.is_err());
    assert!(tool.native_computer_frame(&owner).await.unwrap().is_none());
    {
        let state = tool.runtime.lock().unwrap();
        assert!(!state.held_mouse);
        assert!(state.lease.is_some());
        assert!(!state.backend_inputs_dirty);
        assert!(state.observation_invalidation_pending);
    }
    assert_eq!(backend.events(), ["mouse_down", "screenshot", "mouse_up"]);
    std::fs::write(lock_file, 1_u64.to_le_bytes()).unwrap();
    tool.cleanup_computer_inputs(&owner).await.unwrap();
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    assert!(
        !tool
            .runtime
            .lock()
            .unwrap()
            .observation_invalidation_pending
    );
    assert_eq!(backend.events(), ["mouse_down", "screenshot", "mouse_up"]);
}

#[tokio::test]
async fn key_aliases_cannot_bypass_grants_or_split_held_key_identity() {
    for (modifier, main) in [
        ("cmd", "spacebar"),
        ("ctrl", "uparrow"),
        ("ctrl", "downarrow"),
    ] {
        for split in [false, true] {
            let backend = Arc::new(RecordingBackend::default());
            let tool = make_tool(backend.clone());
            let result = if split {
                invoke(&tool, json!({"action":"key_down","text":modifier}), ctx())
                    .await
                    .unwrap();
                invoke(&tool, json!({"action":"key_down","text":main}), ctx()).await
            } else {
                invoke(
                    &tool,
                    json!({"action":"key","keys":[modifier, main]}),
                    ctx(),
                )
                .await
            };
            assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
            assert!(!backend
                .events()
                .iter()
                .any(|event| event == &format!("down:{}", canonical_key(main))));
        }
    }
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    invoke(&tool, json!({"action":"key_down","text":"spacebar"}), ctx())
        .await
        .unwrap();
    invoke(&tool, json!({"action":"key_up","text":"space"}), ctx())
        .await
        .unwrap();
    assert_eq!(backend.events(), ["down:space", "up:space"]);
}

#[tokio::test]
async fn half_completed_click_keeps_lease_until_all_buttons_are_released() {
    let backend = Arc::new(RecordingBackend::default());
    backend.click_failure.store(true, Ordering::SeqCst);
    backend.button_cleanup_failures.store(1, Ordering::SeqCst);
    let tool = make_tool(backend.clone());
    let mut owner = ctx();
    owner.agent_id = Some(lingxi_core::types::AgentId::new());
    assert!(invoke(
        &tool,
        json!({"action":"mouse_click","button":"back","coordinate":[1,1]}),
        owner.clone()
    )
    .await
    .is_err());
    assert!(tool.runtime.lock().unwrap().backend_mouse_dirty);
    assert!(tool.runtime.lock().unwrap().lease.is_some());
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    tool.cleanup_computer_inputs(&owner).await.unwrap();
    assert!(!tool.runtime.lock().unwrap().backend_mouse_dirty);
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_enter_after_typing_retains_dirty_keys_until_owner_cleanup() {
    let backend = Arc::new(RecordingBackend::default());
    backend.chord_failure.store(true, Ordering::SeqCst);
    backend.key_cleanup_failures.store(1, Ordering::SeqCst);
    let tool = make_tool(backend.clone());
    assert!(invoke(
        &tool,
        json!({"action":"type","text":"hello","press_enter":true}),
        ctx()
    )
    .await
    .is_err());
    assert!(tool.runtime.lock().unwrap().backend_inputs_dirty);
    assert!(tool.runtime.lock().unwrap().lease.is_some());
    tool.cleanup_computer_inputs(&ctx()).await.unwrap();
    assert!(!tool.runtime.lock().unwrap().backend_inputs_dirty);
    assert!(tool.runtime.lock().unwrap().lease.is_none());
}

#[tokio::test]
async fn failed_drag_release_retains_owner_until_mouse_cleanup_succeeds() {
    let backend = Arc::new(RecordingBackend::default());
    backend.mouse_release_failures.store(2, Ordering::SeqCst);
    let tool = make_tool(backend.clone());
    let owner = ctx();
    let mut sibling = ctx();
    sibling.agent_id = Some(lingxi_core::types::AgentId::new());
    assert!(invoke(
        &tool,
        json!({"action":"left_click_drag","path":[[0,0],[1,1]]}),
        owner.clone()
    )
    .await
    .is_err());
    assert!(tool.runtime.lock().unwrap().held_mouse);
    assert!(tool.runtime.lock().unwrap().lease.is_some());
    assert!(
        invoke(&tool, json!({"action":"screenshot"}), sibling.clone())
            .await
            .is_err()
    );
    tool.cleanup_computer_inputs(&owner).await.unwrap();
    assert!(!tool.runtime.lock().unwrap().held_mouse);
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    invoke(&tool, json!({"action":"screenshot"}), sibling)
        .await
        .unwrap();
    assert_eq!(
        backend.events().iter().filter(|e| *e == "mouse_up").count(),
        3
    );
}

#[tokio::test]
async fn failed_chord_cleanup_keeps_lease_until_backend_keys_are_released() {
    let backend = Arc::new(RecordingBackend::default());
    backend.chord_failure.store(true, Ordering::SeqCst);
    backend.key_cleanup_failures.store(1, Ordering::SeqCst);
    let tool = make_tool(backend.clone());
    let owner = ctx();
    let mut sibling = ctx();
    sibling.agent_id = Some(lingxi_core::types::AgentId::new());
    assert!(invoke(
        &tool,
        json!({"action":"key","keys":["cmd","a"]}),
        owner.clone()
    )
    .await
    .is_err());
    assert!(tool.runtime.lock().unwrap().backend_inputs_dirty);
    assert!(
        invoke(&tool, json!({"action":"screenshot"}), sibling.clone())
            .await
            .is_err()
    );
    tool.cleanup_computer_inputs(&owner).await.unwrap();
    assert!(!tool.runtime.lock().unwrap().backend_inputs_dirty);
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    invoke(&tool, json!({"action":"screenshot"}), sibling)
        .await
        .unwrap();
    assert_eq!(
        backend
            .events()
            .iter()
            .filter(|e| *e == "cleanup_keys")
            .count(),
        2
    );
}

#[tokio::test]
async fn registry_agent_cleanup_releases_held_keys_for_only_the_requested_owner() {
    use lingxi_core::host::ToolInvoker;
    let backend = Arc::new(RecordingBackend::default());
    let tool = Arc::new(make_tool(backend.clone()));
    let mut registry = tool_api::registry::ToolRegistry::new();
    registry.register_builtin(tool.clone());
    let invoker = tool_api::RegistryToolInvoker::new(Arc::new(registry));
    let mut owner = ctx();
    let agent = lingxi_core::types::AgentId::new();
    let session = lingxi_core::types::SessionId::new();
    owner.agent_id = Some(agent);
    owner.origin_session_id = Some(session);
    invoke(
        &tool,
        json!({"action":"key_down","text":"shift"}),
        owner.clone(),
    )
    .await
    .unwrap();
    invoker
        .cleanup_computer_inputs(lingxi_core::types::AgentId::new(), Some(session))
        .await
        .unwrap();
    assert!(tool.runtime.lock().unwrap().lease.is_some());
    invoker
        .cleanup_computer_inputs(agent, Some(session))
        .await
        .unwrap();
    assert!(tool.runtime.lock().unwrap().lease.is_none());
    assert_eq!(backend.events(), ["down:shift", "up:shift"]);
}

#[tokio::test]
async fn path_modifiers_pixel_scroll_and_enter_preserve_sequence() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    invoke(
        &tool,
        json!({"action":"left_click_drag","path":[[0,0],[1,2],[3,3]],"modifiers":["shift"]}),
        ctx(),
    )
    .await
    .unwrap();
    invoke(
        &tool,
        json!({"action":"scroll","use_current_cursor":true,"pixel_delta":[10,-20]}),
        ctx(),
    )
    .await
    .unwrap();
    invoke(
        &tool,
        json!({"action":"scroll","coordinate":[1,1],"scroll_direction":"down","scroll_amount":2}),
        ctx(),
    )
    .await
    .unwrap();
    invoke(
        &tool,
        json!({"action":"type","text":"hello","press_enter":true}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(
        backend.events(),
        [
            "down:shift",
            "move:0,0",
            "mouse_down",
            "move:1,2",
            "move:3,3",
            "mouse_up",
            "up:shift",
            "pixels:2,3:10,-20",
            "ticks:1,1:0,2",
            "type:hello",
            "key:Return"
        ]
    );
}
#[tokio::test]
async fn invalid_batch_and_unknown_modifier_key_post_no_input() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    assert!(invoke(&tool,json!({"action":"computer_batch","actions":[{"action":"left_click","coordinate":[1,1]},{"action":"scroll","coordinate":[1,1],"pixel_delta":[1,2],"dx":1}]}),ctx()).await.is_err());
    assert!(invoke(
        &tool,
        json!({"action":"left_click","coordinate":[9,1],"modifiers":["shift"]}),
        ctx()
    )
    .await
    .is_err());
    assert!(invoke(
        &tool,
        json!({"action":"hold_key","keys":["shift","unknown"],"duration":0.1}),
        ctx()
    )
    .await
    .is_err());
    assert!(backend.events().is_empty());
}
#[tokio::test]
async fn cumulative_shortcut_is_denied_before_second_press() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    invoke(&tool, json!({"action":"key_down","text":"cmd"}), ctx())
        .await
        .unwrap();
    assert!(
        invoke(&tool, json!({"action":"key_down","text":"q"}), ctx())
            .await
            .is_err()
    );
    assert_eq!(backend.events(), ["down:cmd", "up:cmd"]);
}
#[tokio::test]
async fn held_modifier_survives_mouse_modifier_scope_and_owned_cleanup() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    invoke(&tool, json!({"action":"key_down","text":"shift"}), ctx())
        .await
        .unwrap();
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1],"modifiers":["shift","alt"]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(
        backend.events(),
        ["down:shift", "down:alt", "click:1,1", "up:alt"]
    );
    tool.cleanup_computer_inputs(&ctx()).await.unwrap();
    assert_eq!(backend.events().last().unwrap(), "up:shift");
}
#[tokio::test]
async fn cancel_hold_releases_input_and_wait_really_waits() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    let cancel = CancellationToken::new();
    let mut context = ctx();
    context.cancel = Some(cancel.clone());
    let future = invoke(
        &tool,
        json!({"action":"hold_key","keys":["shift","a"],"duration":300}),
        context,
    );
    let cancel_future = async {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(future, cancel_future);
    assert!(matches!(result, Err(ToolError::Aborted)));
    assert_eq!(
        backend.events(),
        ["down:shift", "down:a", "up:a", "up:shift"]
    );
    let started = std::time::Instant::now();
    invoke(&tool, json!({"action":"wait","duration":0.02}), ctx())
        .await
        .unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(20));
}
#[tokio::test]
async fn same_process_peer_and_sibling_agent_cannot_interleave_sequence() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    let peer = make_tool(backend.clone()).with_lock_home(tool.lock_home.clone());
    tool.begin_computer_sequence(&ctx()).await.unwrap();
    assert!(invoke(
        &peer,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx()
    )
    .await
    .is_err());
    let mut sibling = ctx();
    sibling.agent_id = Some(lingxi_core::types::AgentId::new());
    assert!(invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        sibling
    )
    .await
    .is_err());
    tool.end_computer_sequence(&ctx()).await.unwrap();
    invoke(
        &peer,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(backend.events(), ["click:1,1"]);
}
#[tokio::test]
async fn screenshot_readiness_is_owner_specific_and_geometry_is_rechecked() {
    use lingxi_llm_client::protocol::computer::{ComputerOperation, ComputerPoint};
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    let frame = tool.native_computer_frame(&ctx()).await.unwrap().unwrap();
    let mut sibling = ctx();
    sibling.agent_id = Some(lingxi_core::types::AgentId::new());
    assert!(tool
        .native_computer_frame(&sibling)
        .await
        .unwrap()
        .is_none());
    let input = tool
        .lower_computer_operation(
            &ComputerOperation::Move {
                point: ComputerPoint { x: 1.0, y: 2.0 },
                modifiers: vec![],
            },
            &frame,
        )
        .unwrap();
    backend.geometry.store(1, Ordering::Relaxed);
    assert!(invoke(&tool, input, ctx()).await.is_err());
    assert_eq!(backend.events(), ["screenshot"]);
}
#[tokio::test]
async fn cancelled_access_cannot_commit_late_grants() {
    let backend = Arc::new(RecordingBackend::default());
    let mut tool = make_tool(backend);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tool.access_resolver = Arc::new(TuiBridgeResolver::new(sender));
    let cancel = CancellationToken::new();
    let mut context = ctx();
    context.cancel = Some(cancel.clone());
    let request = invoke(
        &tool,
        json!({"action":"request_access","apps":["late.app"],"systemKeyCombos":true}),
        context,
    );
    let respond = async {
        let exchange = receiver.recv().await.unwrap();
        cancel.cancel();
        let _ = exchange
            .resp_tx
            .send(permission::computer_access::ComputerAccessResponse {
                granted_apps: vec!["late.app".into()],
                system_key_combos: true,
                ..Default::default()
            });
    };
    let (result, ()) = tokio::join!(request, respond);
    assert!(matches!(result, Err(ToolError::Aborted)));
    let state = tool.state.lock().unwrap();
    assert!(state.tier_for("late.app").is_none());
    assert!(!state.grant_flags.system_key_combos);
}

#[tokio::test]
async fn screenshot_requires_final_image_and_hook_replacement_invalidates_readiness() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend);
    let context = ctx();
    let _result = tool
        .call(
            json!({"action":"screenshot"}),
            context.clone(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_none());
    tool.computer_model_output(&context, "image removed", None)
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_none());
    let result = tool
        .call(
            json!({"action":"screenshot"}),
            context.clone(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    let mut blocks = tool_api::tool_result_media::image_content_blocks(&result.data).unwrap();
    blocks[0]["source"]["data"] = json!("replacement-image");
    tool.computer_model_output(&context, "image replaced", Some(&blocks))
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_none());
    let result = tool
        .call(
            json!({"action":"screenshot"}),
            context.clone(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    let blocks = tool_api::tool_result_media::image_content_blocks(&result.data).unwrap();
    tool.computer_model_output(&context, "captured", Some(&blocks))
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_some());
    let _ = result;
}

#[tokio::test]
async fn ordinary_input_requires_a_new_confirmed_screenshot() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend);
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_some());
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_none());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_some());
    tool.invalidate_computer_observation(&ctx()).await.unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_none());
}
#[tokio::test]
async fn native_sequence_preserves_frozen_geometry_but_requires_terminal_observation() {
    use lingxi_llm_client::protocol::computer::{ComputerOperation, ComputerPoint};
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend);
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    let frame = tool.native_computer_frame(&ctx()).await.unwrap().unwrap();
    tool.begin_computer_sequence(&ctx()).await.unwrap();
    let movement = ComputerOperation::Move {
        point: ComputerPoint { x: 1.0, y: 2.0 },
        modifiers: vec![],
    };
    let first = tool.lower_computer_operation(&movement, &frame).unwrap();
    invoke(&tool, first, ctx()).await.unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_none());
    let second = tool.lower_computer_operation(&movement, &frame).unwrap();
    invoke(&tool, second, ctx()).await.unwrap();
    tool.end_computer_sequence(&ctx()).await.unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_none());
    assert!(tool.lower_computer_operation(&movement, &frame).is_err());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    tool.begin_computer_sequence(&ctx()).await.unwrap();
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    tool.end_computer_sequence(&ctx()).await.unwrap();
    assert!(tool.native_computer_frame(&ctx()).await.unwrap().is_some());
}

#[tokio::test]
async fn different_config_homes_share_the_same_physical_desktop_lease() {
    let backend = Arc::new(RecordingBackend {
        desktop_scope: Some(
            std::env::temp_dir().join(format!("computer-shared-desktop-{}", std::process::id())),
        ),
        ..Default::default()
    });
    let first = make_tool(backend.clone());
    let second = make_tool(backend.clone());
    assert_ne!(first.lock_home, second.lock_home);
    first.begin_computer_sequence(&ctx()).await.unwrap();
    let error = invoke(
        &second,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ToolError::PermissionDenied(_)));
    assert!(backend.events().is_empty());
    invoke(
        &second,
        json!({"action":"request_access","apps":["test.app"]}),
        ctx(),
    )
    .await
    .unwrap();
    first.end_computer_sequence(&ctx()).await.unwrap();
    invoke(
        &second,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(backend.events(), ["click:1,1"]);
}

#[tokio::test]
async fn sibling_input_invalidates_observations_and_previously_lowered_work() {
    use lingxi_llm_client::protocol::computer::{ComputerOperation, ComputerPoint};
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    let first = ctx();
    let mut second = ctx();
    second.agent_id = Some(lingxi_core::types::AgentId::new());
    invoke(&tool, json!({"action":"screenshot"}), first.clone())
        .await
        .unwrap();
    let frame = tool.native_computer_frame(&first).await.unwrap().unwrap();
    let movement = ComputerOperation::Move {
        point: ComputerPoint { x: 2.0, y: 2.0 },
        modifiers: vec![],
    };
    let lowered = tool.lower_computer_operation(&movement, &frame).unwrap();
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        second,
    )
    .await
    .unwrap();
    assert!(tool.lower_computer_operation(&movement, &frame).is_err());
    assert!(invoke(&tool, lowered, first.clone()).await.is_err());
    assert!(tool.native_computer_frame(&first).await.unwrap().is_none());
    assert_eq!(backend.events(), ["screenshot", "click:1,1"]);
    invoke(&tool, json!({"action":"screenshot"}), first.clone())
        .await
        .unwrap();
    assert!(tool.native_computer_frame(&first).await.unwrap().is_some());
}

#[tokio::test]
async fn peer_tool_input_invalidates_frames_and_sequence_admission_on_the_same_desktop() {
    use lingxi_llm_client::protocol::computer::{ComputerOperation, ComputerPoint};
    let scope = tempfile_scope();
    let backend = Arc::new(RecordingBackend {
        desktop_scope: Some(scope.clone()),
        ..Default::default()
    });
    let first = make_tool(backend.clone());
    let peer = make_tool(backend.clone());
    invoke(&first, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    let frame = first.native_computer_frame(&ctx()).await.unwrap().unwrap();
    let movement = ComputerOperation::Move {
        point: ComputerPoint { x: 2.0, y: 2.0 },
        modifiers: vec![],
    };
    let lowered = first.lower_computer_operation(&movement, &frame).unwrap();
    invoke(
        &peer,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    first.begin_computer_sequence(&ctx()).await.unwrap();
    assert!(
        invoke(&first, lowered, ctx()).await.is_err(),
        "a newly acquired sequence must not bless an older generation"
    );
    first.end_computer_sequence(&ctx()).await.unwrap();
    assert!(first.lower_computer_operation(&movement, &frame).is_err());
    assert!(first.native_computer_frame(&ctx()).await.unwrap().is_none());
    assert_eq!(backend.events(), ["screenshot", "click:1,1"]);
    invoke(&first, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    assert!(first.native_computer_frame(&ctx()).await.unwrap().is_some());
}

fn tempfile_scope() -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "computer-generation-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[tokio::test]
async fn failed_partial_input_invalidates_peer_observation() {
    let backend = Arc::new(RecordingBackend {
        desktop_scope: Some(tempfile_scope()),
        ..Default::default()
    });
    let first = make_tool(backend.clone());
    let peer = make_tool(backend.clone());
    invoke(&first, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    backend.chord_failure.store(true, Ordering::SeqCst);
    assert!(invoke(&peer, json!({"action":"key","keys":["a"]}), ctx())
        .await
        .is_err());
    assert!(first.native_computer_frame(&ctx()).await.unwrap().is_none());
}
