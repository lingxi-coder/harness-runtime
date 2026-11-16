//! Explicit, ignored real macOS acceptance. The launcher owns only two
//! disposable org.lingxi.raw-probe.* fixtures; this test never launches apps,
//! changes permissions, switches Spaces, or chooses a user's application.
use super::*;
use base64::Engine;
use lingxi_core::host::computer_control::ComputerControl;
use platform_macos_computer_control::MacosComputerControl;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const TARGET: &str = "org.lingxi.raw-probe.target";
const SHADOW: &str = "org.lingxi.raw-probe.shadow";
fn fixture(root: &Path, name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(root.join(format!("{name}.json"))).expect("live owned fixture JSON"),
    )
    .expect("valid fixture JSON")
}
fn wire(result: &ToolCallResult) -> Value {
    serde_json::from_str(&result.data.to_json_string().unwrap()).unwrap()
}
fn text(result: &ToolCallResult) -> String {
    wire(result)
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
fn counter(value: &Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("fixture omitted {key}: {value}"))
}
fn window(value: &Value) -> u64 {
    counter(value, "window")
}
fn spaces(value: &Value) -> Vec<u64> {
    value["spaces"]
        .as_array()
        .expect("live fixture must report actual Space memberships")
        .iter()
        .map(|space| space.as_u64().expect("Space ID must be positive integer"))
        .collect()
}
fn cursor(value: &Value) -> [f64; 2] {
    [
        value["cursor"][0].as_f64().expect("cursor x"),
        value["cursor"][1].as_f64().expect("cursor y"),
    ]
}
fn surface(before: &Value, after: &Value, shadow_pid: u64) {
    assert_eq!(
        counter(after, "frontmost"),
        shadow_pid,
        "background tool activated another app: {after}"
    );
    assert_eq!(after["frontmost"], before["frontmost"]);
    assert_eq!(
        after["activations"], before["activations"],
        "background tool changed NSWorkspace activation count"
    );
    assert_eq!(
        cursor(after),
        cursor(before),
        "background tool moved physical cursor"
    );
    assert_eq!(
        after["text"], before["text"],
        "selection/key test changed fixture content"
    );
    assert_eq!(after["window"], before["window"]);
    assert_eq!(after["pid"], before["pid"]);
}
async fn settled(root: &Path, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let current = fixture(root, "target");
        if predicate(&current) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "fixture effect not observed before deadline: {current}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}
async fn call(
    tool: &ComputerTool,
    action: &str,
    input: Value,
    ctx: &ToolUseContext,
) -> ToolCallResult {
    tool.call_protocol(
        action,
        Utf16JsonProjection::parse(&input.to_string()).unwrap(),
        ctx.clone(),
        tool_api::test_support::fresh_tx(),
    )
    .await
    .unwrap_or_else(|e| panic!("real {action} dispatch error: {e}"))
}
fn successful(action: &str, result: &ToolCallResult) {
    assert!(!result.is_error, "{action} refused: {}", text(result));
}

#[tokio::test]
#[ignore = "requires explicit LINGXI_RAW_NATIVE_PROBE_DIR and parent-owned live target/shadow app fixtures"]
async fn actual_background_protocol_drag_key_and_current_space_guard() {
    let root = PathBuf::from(
        std::env::var_os("LINGXI_RAW_NATIVE_PROBE_DIR").expect("explicit fixture dir required"),
    );
    assert!(root.is_absolute(), "fixture path must be absolute");
    let before = fixture(&root, "target");
    let shadow_before = fixture(&root, "shadow");
    let shadow_pid = counter(&shadow_before, "pid");
    assert_ne!(counter(&before, "pid"), shadow_pid);
    assert_eq!(
        counter(&before, "frontmost"),
        shadow_pid,
        "launcher must first make only the owned shadow frontmost"
    );
    assert_eq!(counter(&shadow_before, "frontmost"), shadow_pid);
    assert_eq!(
        before["selection"][1], 0,
        "launcher must start with an unselected target text field"
    );
    assert!(
        before["textStartWindow"].is_array() && before["textEndWindow"].is_array(),
        "launcher must run the glyph-coordinate fixture revision"
    );
    let backend = Arc::new(MacosComputerControl::new());
    assert!(
        backend.supports_app_scoped_raw_input(),
        "actual native raw-input support required"
    );
    assert!(
        backend.supports_app_scoped_space_move(),
        "actual native Space move support required"
    );
    let names = protocol_tool_names_for_host(backend.as_ref(), true);
    assert_eq!(
        names.len(),
        42,
        "actual native host must publish full protocol surface"
    );
    assert!(names
        .iter()
        .any(|name| name == "mcp__computer-use__app_drag"));
    assert!(names
        .iter()
        .any(|name| name == "mcp__computer-use__app_bring_to_current_space"));
    assert_eq!(
        backend.frontmost_app().await.unwrap().unwrap().bundle_id,
        SHADOW,
        "frontmost identity must be the exact owned fixture"
    );
    let fs = tool_api::test_support::make_dummy_fs();
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let mut builtin = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
    builtin.computer_control = Some(backend);
    let lock_home = root.join(format!("tool-native-lock-{}", std::process::id()));
    let tool = ComputerTool::with_access_resolver(builtin, Arc::new(AutoGrantResolver))
        .with_lock_home(lock_home);
    {
        let mut state = tool.state.lock().unwrap();
        state.grant_app(TARGET.into(), AppTier::Full);
        state.grant_app(SHADOW.into(), AppTier::Read);
    }
    // Every call uses this exact owner/turn context, including the screenshot's
    // cached coordinate frame and per-window background lease.
    let ctx = tool_api::test_support::fresh_ctx();
    let listed = call(&tool, "app_list_windows", json!({"app":TARGET}), &ctx).await;
    successful("app_list_windows", &listed);
    let live_windows: Value =
        serde_json::from_str(&text(&listed)).expect("native window list JSON");
    let initial_bounds = live_windows
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["window_id"].as_u64() == Some(window(&before)))
        .expect("fixture's exact current auxiliary window must appear in native listing")["bounds"]
        .clone();
    let bring = call(
        &tool,
        "app_bring_to_current_space",
        json!({"app":TARGET,"window_id":window(&before)}),
        &ctx,
    )
    .await;
    let bring_text = text(&bring);
    let destination = counter(&before, "activeSpace");
    let source_spaces = spaces(&before);
    assert!(
        !source_spaces.is_empty(),
        "fixture needs a classified initial Space"
    );
    let requires_move = !source_spaces.contains(&destination);
    let mut actual_move_proved = false;
    let classification = if requires_move {
        successful("app_bring_to_current_space", &bring);
        assert!(bring_text.contains("is now on the current Space"),
            "off-Space fixture requires an actual move result, not already-here/guard: {bring_text}");
        let moved = settled(&root, |value| {
            let current = spaces(value);
            current.contains(&destination)
                && source_spaces.iter().all(|source| !current.contains(source))
        })
        .await;
        assert_eq!(
            counter(&moved, "activeSpace"),
            destination,
            "bringing a window must not switch the user's current Space"
        );
        surface(&before, &moved, shadow_pid);
        actual_move_proved = true;
        "actual_move"
    } else if bring.is_error {
        assert!(
            bring_text.contains("full-screen Space") || bring_text.contains("full-screen app"),
            "expected real fullscreen guard, not an unavailable/native failure: {bring_text}"
        );
        "fullscreen_guard"
    } else {
        assert!(
            bring_text.contains("already on the current Space"),
            "this acceptance route must not unexpectedly move the fixture: {bring_text}"
        );
        "already_here"
    };
    // Off-Space windows may have no live AXWindow until brought here. The
    // move itself uses independently checked CG owner metadata; only after its
    // verified arrival do screenshot/drag/key require the live AX window.
    let refreshed = call(&tool, "app_list_windows", json!({"app":TARGET}), &ctx).await;
    successful("app_list_windows after bring", &refreshed);
    let refreshed_windows: Value = serde_json::from_str(&text(&refreshed)).unwrap();
    let live_bounds = refreshed_windows
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["window_id"].as_u64() == Some(window(&before)))
        .expect("exact moved fixture window must remain listed")["bounds"]
        .clone();
    let capture_fixture = fixture(&root, "target");
    let shot = call(
        &tool,
        "app_screenshot",
        json!({"app":TARGET,"window_id":window(&before),"scale":1}),
        &ctx,
    )
    .await;
    successful("app_screenshot", &shot);
    let shot_wire = wire(&shot);
    let image_block = shot_wire
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "image")
        .expect("real screenshot image content");
    let encoded = base64::engine::general_purpose::STANDARD
        .decode(image_block["data"].as_str().unwrap())
        .unwrap();
    let image = image::load_from_memory(&encoded).expect("actual native JPEG payload");
    let frame_w = live_bounds["w"]
        .as_f64()
        .expect("native logical window width");
    let frame_h = live_bounds["h"]
        .as_f64()
        .expect("native logical window height");
    assert_eq!(
        capture_fixture["frame"][2].as_f64(),
        Some(frame_w),
        "fixture/native window width disagree"
    );
    assert_eq!(
        capture_fixture["frame"][3].as_f64(),
        Some(frame_h),
        "fixture/native window height disagree"
    );
    let map = |name: &str| {
        [
            capture_fixture[name][0].as_f64().unwrap() * f64::from(image.width()) / frame_w,
            capture_fixture[name][1].as_f64().unwrap() * f64::from(image.height()) / frame_h,
        ]
    };
    let start = map("textStartWindow");
    let end = map("textEndWindow");
    let drag = call(
        &tool,
        "app_drag",
        json!({"app":TARGET,"window_id":window(&before),"coordinate":start,"to_coordinate":end}),
        &ctx,
    )
    .await;
    successful("app_drag", &drag);
    let after_drag = settled(&root, |v| v["selection"][1].as_u64().is_some_and(|n| n > 0)).await;
    surface(&before, &after_drag, shadow_pid);
    let shadow_drag = fixture(&root, "shadow");
    surface(&shadow_before, &shadow_drag, shadow_pid);
    let key = call(
        &tool,
        "app_key",
        json!({"app":TARGET,"window_id":window(&before),"target":"focused","combo":"cmd+a"}),
        &ctx,
    )
    .await;
    successful("app_key", &key);
    assert!(
        text(&key).contains("via raw input"),
        "cmd+a must exercise actual raw-key delivery, not silently pass via another path: {}",
        text(&key)
    );
    let text_units = before["text"].as_str().unwrap().encode_utf16().count() as u64;
    let after_key = settled(&root, |v| v["selection"][1].as_u64() == Some(text_units)).await;
    surface(&before, &after_key, shadow_pid);
    surface(&shadow_before, &fixture(&root, "shadow"), shadow_pid);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = fixture(&root, "target");
    surface(&before, &after, shadow_pid);
    assert_eq!(
        counter(&after, "activeSpace"),
        destination,
        "background tool must preserve the active Space"
    );
    let final_spaces = spaces(&after);
    if requires_move {
        assert!(final_spaces.contains(&destination));
        assert!(source_spaces
            .iter()
            .all(|source| !final_spaces.contains(source)));
    }
    surface(&shadow_before, &fixture(&root, "shadow"), shadow_pid);
    // The canvas exposes no AXPress action. This must traverse the native
    // process-directed raw mouse fallback and produce a measured view event.
    let canvas = [
        100.0 * f64::from(image.width()) / frame_w,
        (frame_h - 80.0) * f64::from(image.height()) / frame_h,
    ];
    let clicked = call(
        &tool,
        "app_click",
        json!({"app":TARGET,"window_id":window(&before),"coordinate":canvas}),
        &ctx,
    )
    .await;
    successful("app_click", &clicked);
    assert!(
        text(&clicked).contains("via raw input"),
        "canvas must use real raw mouse path: {}",
        text(&clicked)
    );
    let downs = counter(&after_key, "downs");
    let after_click = settled(&root, |v| {
        counter(v, "downs") == downs + 1 && counter(v, "ups") == counter(&after_key, "ups") + 1
    })
    .await;
    surface(&before, &after_click, shadow_pid);
    surface(&shadow_before, &fixture(&root, "shadow"), shadow_pid);
    let triple = call(
        &tool,
        "app_click",
        json!({"app":TARGET,"window_id":window(&before),"coordinate":canvas,"count":3}),
        &ctx,
    )
    .await;
    successful("app_click count3", &triple);
    let after_triple = settled(&root, |v| {
        counter(v, "downs") == downs + 4 && counter(v, "ups") == counter(&after_key, "ups") + 4
    })
    .await;
    surface(&before, &after_triple, shadow_pid);
    surface(&shadow_before, &fixture(&root, "shadow"), shadow_pid);
    let release = call(
        &tool,
        "app_release",
        json!({"app":TARGET,"window_id":window(&before)}),
        &ctx,
    )
    .await;
    successful("app_release", &release);
    let receipt = json!({"test":"actual_background_protocol_drag_key_and_current_space_guard","realComputerTool":true,"realMacosBackend":true,"sameTurnContext":true,"fixtureBundles":[TARGET,SHADOW],"targetPid":before["pid"],"shadowPid":shadow_pid,"windowId":before["window"],"imageDimensions":[image.width(),image.height()],"logicalWindowDimensions":[frame_w,frame_h],"nativeWindowBounds":live_bounds,"initialNativeWindowBounds":initial_bounds,"moveBeforeScreenshot":true,"imageCoordinates":{"from":start,"to":end},"drag":{"text":text(&drag),"selection":after_drag["selection"]},"key":{"text":text(&key),"selection":after_key["selection"]},"rawMouse":{"text":text(&clicked),"coordinate":canvas,"downs":after_click["downs"],"ups":after_click["ups"],"tripleClickText":text(&triple),"downsAfterTriple":after_triple["downs"],"upsAfterTriple":after_triple["ups"]},"bring":{"text":bring_text,"classification":classification,"actualMoveProved":actual_move_proved,"requiresMove":requires_move,"sourceSpaces":source_spaces,"destinationSpace":destination,"finalSpaces":final_spaces},"invariants":{"foregroundStable":true,"cursorStable":true,"activationCountStable":true,"contentsUnchanged":true}});
    std::fs::write(
        root.join("native-tool-acceptance.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    println!("{}", receipt);
}
