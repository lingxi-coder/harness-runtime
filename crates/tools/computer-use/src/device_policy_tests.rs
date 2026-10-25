use super::execution_tests::{ctx, invoke, make_tool, RecordingBackend};
use super::*;
use lingxi_core::host::computer_control::AppInfo;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

struct PolicyFile(PathBuf);
impl PolicyFile {
    fn new(value: Value) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "computer-policy-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let file = Self(directory.join("policy.json"));
        file.replace(value);
        file
    }
    fn replace(&self, value: Value) {
        replace(&self.0, value);
    }
}
impl Drop for PolicyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}
fn replace(path: &Path, value: Value) {
    let next = path.with_extension("next");
    std::fs::write(&next, serde_json::to_vec(&value).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::rename(next, path).unwrap();
}
fn policy(revision: &str, enabled: bool, apps: Value) -> Value {
    json!({"revision":revision,"enabled":enabled,"apps":apps,"browser":{}})
}
fn app(permission: &str) -> Value {
    json!({"bundleId":"test.app","displayName":"Test","permission":permission})
}
async fn grant(tool: &ComputerTool) -> Result<ToolCallResult, ToolError> {
    invoke(
        tool,
        json!({"action":"request_access","reason":"edit the test application","apps":["test.app"]}),
        ctx(),
    )
    .await
}

#[tokio::test]
async fn device_policy_disable_and_reenable_does_not_revive_session_grants() {
    let file = PolicyFile::new(policy("initial", true, json!([])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    grant(&tool).await.unwrap();
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    file.replace(policy("disabled", false, json!([])));
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    file.replace(policy("reenabled", true, json!([])));
    assert!(invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx()
    )
    .await
    .is_err());
    assert_eq!(backend.events(), ["click:1,1"]);
    grant(&tool).await.unwrap();
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(backend.events(), ["click:1,1", "click:1,1"]);
}

#[tokio::test]
async fn removing_permanent_access_revokes_existing_capture_and_native_frame() {
    let file = PolicyFile::new(policy("allowed", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let context = ctx();
    invoke(&tool, json!({"action":"screenshot"}), context.clone())
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_some());
    assert_eq!(backend.captured_apps(), [vec!["test.app".to_owned()]]);
    file.replace(policy("revoked", true, json!([app("deny")])));
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_none());
    assert!(invoke(&tool, json!({"action":"screenshot"}), context)
        .await
        .is_err());
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    let result = grant(&tool).await.unwrap();
    assert_eq!(test_data(&result.data)["granted"], json!([]));
    assert_eq!(test_data(&result.data)["denied"], json!(["test.app"]));
    assert_eq!(backend.events(), ["screenshot"]);
}

#[tokio::test]
async fn stale_pending_approval_cannot_restore_access_after_policy_changes() {
    let file = PolicyFile::new(policy("before", true, json!([])));
    let mut tool =
        make_tool(Arc::new(RecordingBackend::default())).with_device_policy_path(file.0.clone());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tool.access_resolver = Arc::new(TuiBridgeResolver::new(sender));
    let request = grant(&tool);
    let response = async {
        let exchange = receiver.recv().await.unwrap();
        file.replace(policy("after", true, json!([app("deny")])));
        exchange
            .resp_tx
            .send(permission::computer_access::ComputerAccessResponse {
                granted_apps: vec!["test.app".into()],
                ..Default::default()
            })
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(request, response)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
}

#[tokio::test]
async fn resolver_cannot_grant_unrequested_apps_or_capabilities() {
    let file = PolicyFile::new(policy("initial", true, json!([])));
    let mut tool =
        make_tool(Arc::new(RecordingBackend::default())).with_device_policy_path(file.0.clone());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tool.access_resolver = Arc::new(TuiBridgeResolver::new(sender));
    let request = grant(&tool);
    let response = async {
        let exchange = receiver.recv().await.unwrap();
        exchange
            .resp_tx
            .send(permission::computer_access::ComputerAccessResponse {
                granted_apps: vec!["test.app".into(), "unrequested.app".into()],
                clipboard_read: true,
                clipboard_write: true,
                system_key_combos: true,
            })
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(request, response)
    })
    .await
    .unwrap();
    result.unwrap();
    let state = tool.state.lock().unwrap();
    assert_eq!(state.allowed_apps.len(), 1);
    assert!(state.tier_for("unrequested.app").is_none());
    assert!(!state.grant_flags.clipboard_read);
    assert!(!state.grant_flags.clipboard_write);
    assert!(!state.grant_flags.system_key_combos);
}

#[tokio::test]
async fn revocation_during_a_batch_stops_following_input() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    backend.replace_policy_on_click(file.0.clone(), policy("revoked", false, json!([])));
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let result = invoke(
        &tool,
        json!({"action":"computer_batch","actions":[
            {"action":"left_click","coordinate":[1,1]},
            {"action":"type","text":"must not appear"},
            {"action":"left_click","coordinate":[2,2]}
        ]}),
        ctx(),
    )
    .await
    .unwrap();
    assert!(test_data(&result.data).get("stepFailed").is_some());
    assert_eq!(backend.events(), ["click:1,1", "cleanup_buttons"]);
}

#[tokio::test]
async fn authorized_foreground_does_not_authorize_an_ungranted_target_window() {
    let backend = Arc::new(RecordingBackend::default());
    backend.use_target_checks(Some(AppInfo {
        bundle_id: "other.app".into(),
        display_name: "Other".into(),
    }));
    let tool = make_tool(backend.clone());
    assert!(invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx()
    )
    .await
    .is_err());
    assert!(backend.events().is_empty());
    backend.use_target_checks(Some(AppInfo {
        bundle_id: "test.app".into(),
        display_name: "Test".into(),
    }));
    invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(backend.events(), ["click:1,1"]);
}

#[tokio::test]
async fn corrupt_or_missing_device_policy_never_returns_a_capture() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    std::fs::write(&file.0, b"{incomplete").unwrap();
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    std::fs::remove_file(&file.0).unwrap();
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert_eq!(backend.events(), ["screenshot"]);
}

#[tokio::test]
async fn screenshot_requires_a_grant_and_browser_permissions_do_not_create_one() {
    let file = PolicyFile::new(
        json!({"revision":"browser-only","enabled":true,"apps":[],"browser":{"defaultBrowsing":"allow"}}),
    );
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert!(backend.events().is_empty());
}

#[tokio::test]
async fn clipboard_shortcut_grants_are_revoked_while_permanent_app_access_remains() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    invoke(&tool,json!({"action":"request_access","reason":"paste into the test application","apps":["test.app"],"clipboardRead":true}),ctx()).await.unwrap();
    invoke(&tool, json!({"action":"key","text":"cmd+v"}), ctx())
        .await
        .unwrap();
    file.replace(policy("revoked", true, json!([app("allow")])));
    assert!(invoke(&tool, json!({"action":"key","text":"cmd+v"}), ctx())
        .await
        .is_err());
    let state = tool.state.lock().unwrap();
    assert_eq!(state.tier_for("test.app"), Some(AppTier::Full));
    assert!(!state.grant_flags.clipboard_read);
    assert_eq!(backend.events(), ["key:cmd+v"]);
}

#[tokio::test]
async fn caller_selected_full_tier_cannot_override_browser_category() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    invoke(&tool,json!({"action":"request_access","reason":"inspect Chrome","apps":["com.google.Chrome"],"tier":"full"}),ctx()).await.unwrap();
    assert_eq!(
        tool.state.lock().unwrap().tier_for("com.google.Chrome"),
        Some(AppTier::Read)
    );
    backend.use_target_checks(Some(AppInfo {
        bundle_id: "com.google.Chrome".into(),
        display_name: "Chrome".into(),
    }));
    assert!(invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx()
    )
    .await
    .is_err());
    assert!(backend.events().is_empty());
}

#[tokio::test]
async fn policy_blocked_apps_cannot_be_auto_approved() {
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone());
    let result = invoke(
        &tool,
        json!({"action":"request_access","reason":"inspect Spotify","apps":["com.spotify.client"]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(test_data(&result.data)["granted"], json!([]));
    assert_eq!(
        test_data(&result.data)["denied"],
        json!(["com.spotify.client"])
    );
    assert!(tool
        .state
        .lock()
        .unwrap()
        .tier_for("com.spotify.client")
        .is_none());
    assert!(backend.events().is_empty());
}

#[tokio::test]
async fn malformed_request_access_never_reaches_a_resolver() {
    let tool = make_tool(Arc::new(RecordingBackend::default()));
    for input in [
        json!({"action":"request_access","apps":["test.app"]}),
        json!({"action":"request_access","reason":"inspect an application","apps":[17]}),
        json!({"action":"request_access","reason":"inspect an application","apps":"test.app"}),
    ] {
        assert!(matches!(
            invoke(&tool, input, ctx()).await,
            Err(ToolError::InvalidInput(_))
        ));
    }
}

#[tokio::test]
async fn tcc_setup_never_commits_app_grants_from_the_setup_panel() {
    let file = PolicyFile::new(policy("initial", true, json!([])));
    let backend = Arc::new(RecordingBackend::default());
    backend.set_os_permissions(Some((false, true)));
    let mut tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tool.access_resolver = Arc::new(TuiBridgeResolver::new(sender));
    let request = grant(&tool);
    let response = async {
        let exchange = receiver.recv().await.unwrap();
        assert!(exchange.request.tcc_state.is_some());
        assert!(exchange.request.apps.is_empty());
        backend.set_os_permissions(Some((true, true)));
        exchange
            .resp_tx
            .send(permission::computer_access::ComputerAccessResponse {
                granted_apps: vec!["test.app".into()],
                clipboard_read: true,
                clipboard_write: true,
                system_key_combos: true,
            })
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(request, response)
    })
    .await
    .unwrap();
    result.unwrap();
    let state = tool.state.lock().unwrap();
    assert!(state.allowed_apps.is_empty());
    assert!(!state.grant_flags.clipboard_read);
    assert!(!state.grant_flags.clipboard_write);
    assert!(!state.grant_flags.system_key_combos);
    assert!(backend.events().is_empty());
}

#[tokio::test]
async fn cancelling_a_tcc_setup_prompt_does_not_wait_for_a_user_response() {
    let file = PolicyFile::new(policy("initial", true, json!([])));
    let backend = Arc::new(RecordingBackend::default());
    backend.set_os_permissions(Some((false, false)));
    let mut tool = make_tool(backend).with_device_policy_path(file.0.clone());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tool.access_resolver = Arc::new(TuiBridgeResolver::new(sender));
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut context = ctx();
    context.cancel = Some(cancel.clone());
    let request = invoke(
        &tool,
        json!({"action":"request_access","reason":"set up macOS access","apps":["test.app"]}),
        context,
    );
    let response = async {
        let exchange = receiver.recv().await.unwrap();
        cancel.cancel();
        // Keep the reply channel alive: cancellation must not depend on its drop.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        exchange
    };
    let (result, _pending_exchange) =
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(request, response)
        })
        .await
        .expect("cancelling TCC setup must not wait on its pending UI reply");
    assert!(matches!(result, Err(ToolError::Aborted)));
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
}

#[tokio::test]
async fn policy_content_changes_invalidate_grants_even_if_revision_text_is_reused() {
    let file = PolicyFile::new(policy("same-label", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    file.replace(policy("same-label", true, json!([])));
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    assert_eq!(backend.events(), ["screenshot"]);
}

#[tokio::test]
async fn locked_display_blocks_input_capture_and_previous_native_frames() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let context = ctx();
    invoke(&tool, json!({"action":"screenshot"}), context.clone())
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_some());
    backend.set_locked(true);
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_none());
    assert!(
        invoke(&tool, json!({"action":"screenshot"}), context.clone())
            .await
            .is_err()
    );
    assert!(invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        context.clone()
    )
    .await
    .is_err());
    assert_eq!(backend.events(), ["screenshot"]);
    backend.set_locked(false);
    invoke(&tool, json!({"action":"screenshot"}), context)
        .await
        .unwrap();
    assert_eq!(backend.events(), ["screenshot", "screenshot"]);
}

#[tokio::test]
async fn disabling_control_releases_owned_input_before_rejecting_the_next_action() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let context = ctx();
    invoke(
        &tool,
        json!({"action":"key_down","text":"a"}),
        context.clone(),
    )
    .await
    .unwrap();
    file.replace(policy("disabled", false, json!([])));
    assert!(invoke(&tool, json!({"action":"screenshot"}), context)
        .await
        .is_err());
    assert_eq!(backend.events(), ["down:a", "up:a"]);
    assert!(tool.runtime.lock().unwrap().held_keys.is_empty());
}

#[tokio::test]
async fn host_cancellation_on_policy_revocation_interrupts_a_long_hold() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut context = ctx();
    context.cancel = Some(cancel.clone());
    let action = invoke(
        &tool,
        json!({"action":"hold_key","text":"a","duration":10}),
        context,
    );
    let revoke = async {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !backend.events().iter().any(|event| event == "down:a") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("hold should start before revocation");
        file.replace(policy("disabled", false, json!([])));
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(action, revoke)
    })
    .await
    .expect("revocation cancellation must not wait out the hold duration");
    assert!(matches!(result, Err(ToolError::Aborted)));
    assert_eq!(backend.events(), ["down:a", "up:a"]);
    assert!(tool.runtime.lock().unwrap().held_keys.is_empty());
}

fn target_app(bundle_id: &str) -> AppInfo {
    AppInfo {
        bundle_id: bundle_id.into(),
        display_name: bundle_id.into(),
    }
}

#[tokio::test]
async fn pointer_target_is_rechecked_after_awaited_display_validation() {
    let backend = Arc::new(RecordingBackend::default());
    backend.use_target_checks(Some(target_app("test.app")));
    backend.change_target_during_display_size(Some(target_app("other.app")));
    let tool = make_tool(backend.clone());
    let result = invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1]}),
        ctx(),
    )
    .await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
    assert!(
        backend.events().is_empty(),
        "no click may follow a target ownership change"
    );
    assert!(
        backend.checked_target_points().len() >= 2,
        "the target must be checked again after display_size yields"
    );
}

#[tokio::test]
async fn pointer_target_is_rechecked_after_awaited_geometry_validation() {
    let backend = Arc::new(RecordingBackend::default());
    backend.use_target_checks(Some(target_app("test.app")));
    let tool = make_tool(backend.clone());
    let context = ctx();
    invoke(&tool, json!({"action":"screenshot"}), context.clone())
        .await
        .unwrap();
    let frame = tool.native_computer_frame(&context).await.unwrap().unwrap();
    backend.change_target_during_geometry(Some(target_app("other.app")));
    let result = invoke(
        &tool,
        json!({"action":"left_click","coordinate":[1,1],"geometry_version":frame.geometry_version}),
        context,
    )
    .await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
    assert_eq!(backend.events(), ["screenshot"]);
}

#[tokio::test]
async fn current_cursor_click_uses_the_pinned_authorized_location() {
    let backend = Arc::new(RecordingBackend::default());
    backend.use_target_checks(Some(target_app("test.app")));
    backend.move_cursor_during_display_size((1, 1), (3, 3));
    let tool = make_tool(backend.clone());
    invoke(
        &tool,
        json!({"action":"left_click","use_current_cursor":true}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(backend.events(), ["click:1,1"]);
    let checked = backend.checked_target_points();
    assert!(!checked.is_empty());
    assert!(
        checked.iter().all(|point| *point == (1, 1)),
        "authorization and physical input must use the same pinned point: {checked:?}"
    );
}

#[tokio::test]
async fn target_aware_backend_accepts_path_only_drag_and_coordinate_free_mouse_holds() {
    let backend = Arc::new(RecordingBackend::default());
    backend.use_target_checks(Some(target_app("test.app")));
    let tool = make_tool(backend.clone());
    invoke(
        &tool,
        json!({"action":"left_click_drag","path":[[0,0],[1,2],[3,3]]}),
        ctx(),
    )
    .await
    .unwrap();
    assert_eq!(
        backend.events(),
        ["move:0,0", "mouse_down", "move:1,2", "move:3,3", "mouse_up"]
    );
    invoke(&tool, json!({"action":"left_mouse_down"}), ctx())
        .await
        .unwrap();
    invoke(&tool, json!({"action":"left_mouse_up"}), ctx())
        .await
        .unwrap();
    assert_eq!(
        backend.events(),
        [
            "move:0,0",
            "mouse_down",
            "move:1,2",
            "move:3,3",
            "mouse_up",
            "move:2,3",
            "mouse_down",
            "move:2,3",
            "mouse_up"
        ]
    );
    assert!(!tool.runtime.lock().unwrap().held_mouse);
}

#[tokio::test]
async fn policy_file_revocation_alone_interrupts_a_long_hold_within_one_second() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
    let context = ctx();
    assert!(
        context.cancel.is_none(),
        "this test must not rely on a host cancellation token"
    );
    let action = invoke(
        &tool,
        json!({"action":"hold_key","text":"a","duration":10}),
        context,
    );
    let revoke = async {
        while !backend.events().iter().any(|event| event == "down:a") {
            tokio::task::yield_now().await;
        }
        file.replace(policy("disabled", false, json!([])));
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(action, revoke)
    })
    .await
    .expect("a policy write must interrupt the 10-second hold without host cancellation");
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
    assert_eq!(backend.events(), ["down:a", "up:a"]);
    assert!(tool.runtime.lock().unwrap().held_keys.is_empty());
}

struct PolicyRaceBackend {
    inner: Arc<RecordingBackend>,
    geometry_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    target_cancel: Option<tokio_util::sync::CancellationToken>,
    target_calls: AtomicU64,
    effect_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    enumeration_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    switched: std::sync::atomic::AtomicBool,
    switch_on_key: bool,
    launched: std::sync::atomic::AtomicBool,
}
impl PolicyRaceBackend {
    fn new() -> Self {
        Self {
            inner: Arc::new(RecordingBackend::default()),
            geometry_hook: Mutex::new(None),
            target_cancel: None,
            target_calls: AtomicU64::new(0),
            effect_hook: Mutex::new(None),
            enumeration_hook: Mutex::new(None),
            switched: std::sync::atomic::AtomicBool::new(false),
            switch_on_key: false,
            launched: std::sync::atomic::AtomicBool::new(false),
        }
    }
}
#[async_trait]
impl lingxi_core::host::computer_control::ComputerControl for PolicyRaceBackend {
    fn capabilities(&self) -> lingxi_core::host::computer_control::ComputerBackendCapabilities {
        self.inner.capabilities()
    }
    async fn validate_keys(&self, keys: &[String]) -> Result<(), ComputerError> {
        self.inner.validate_keys(keys).await
    }
    fn supports_target_application(&self) -> bool {
        self.target_cancel.is_some()
    }
    async fn target_application(&self, _x: u32, _y: u32) -> Result<Option<AppInfo>, ComputerError> {
        self.target_calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        if let Some(cancel) = &self.target_cancel {
            cancel.cancel();
        }
        Ok(Some(target_app("test.app")))
    }
    async fn frame_geometry(
        &self,
    ) -> Result<Option<lingxi_core::host::computer_control::ComputerFrameGeometry>, ComputerError>
    {
        let geometry = self.inner.frame_geometry().await?;
        if let Some(hook) = self.geometry_hook.lock().unwrap().take() {
            hook();
        }
        Ok(geometry)
    }
    async fn frontmost_app(&self) -> Result<Option<AppInfo>, ComputerError> {
        if self.switched.load(Ordering::SeqCst) {
            return Ok(Some(target_app("other.app")));
        }
        self.inner.frontmost_app().await
    }
    async fn screenshot(
        &self,
    ) -> Result<lingxi_core::host::computer_control::Screenshot, ComputerError> {
        self.inner.screenshot().await
    }
    async fn display_size(&self) -> Result<(u32, u32), ComputerError> {
        self.inner.display_size().await
    }
    async fn mouse_move(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.inner.mouse_move(x, y).await?;
        if let Some(hook) = self.effect_hook.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
    async fn left_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.inner.left_click(x, y).await
    }
    async fn right_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.inner.right_click(x, y).await
    }
    async fn double_click(&self, x: u32, y: u32) -> Result<(), ComputerError> {
        self.inner.double_click(x, y).await
    }
    async fn type_text(&self, text: String) -> Result<(), ComputerError> {
        self.inner.type_text(text).await
    }
    async fn key(&self, key: String) -> Result<(), ComputerError> {
        self.inner.key(key).await?;
        if self.switch_on_key {
            self.switched.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    async fn mouse_down(&self) -> Result<(), ComputerError> {
        self.inner.mouse_down().await
    }
    async fn mouse_up(&self) -> Result<(), ComputerError> {
        self.inner.mouse_up().await
    }
    async fn list_installed_apps(&self) -> Result<Vec<AppInfo>, ComputerError> {
        if let Some(hook) = self.enumeration_hook.lock().unwrap().take() {
            hook();
        }
        Ok(vec![target_app("test.app")])
    }
    async fn open_application(&self, _name: String) -> Result<(), ComputerError> {
        self.launched.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn scroll(&self, x: u32, y: u32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        self.inner.scroll(x, y, dx, dy).await
    }
}
fn race_tool(backend: Arc<PolicyRaceBackend>, file: &PolicyFile) -> ComputerTool {
    let fs = tool_api::test_support::make_dummy_fs();
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let mut context = tool_api::test_support::ctx_for_file_tools(fs, bus, vec![]);
    context.computer_control = Some(backend);
    ComputerTool::with_access_resolver(context, Arc::new(AutoGrantResolver))
        .with_lock_home(file.0.parent().unwrap().join("locks"))
        .with_device_policy_path(file.0.clone())
}

#[tokio::test]
async fn policy_revocation_during_geometry_read_never_returns_a_ready_frame() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(PolicyRaceBackend::new());
    let tool = race_tool(backend.clone(), &file);
    let context = ctx();
    invoke(&tool, json!({"action":"screenshot"}), context.clone())
        .await
        .unwrap();
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_some());
    let path = file.0.clone();
    *backend.geometry_hook.lock().unwrap() = Some(Box::new(move || {
        replace(&path, policy("revoked-during-read", false, json!([])))
    }));
    assert!(tool
        .native_computer_frame(&context)
        .await
        .unwrap()
        .is_none());
    assert!(tool.runtime.lock().unwrap().frames.is_empty());
    assert_eq!(backend.inner.events(), ["screenshot"]);
}

#[cfg(unix)]
#[tokio::test]
async fn permissive_mode_or_symlink_policy_revokes_access_and_fails_closed() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for symlink_policy in [false, true] {
        let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone()).with_device_policy_path(file.0.clone());
        invoke(&tool, json!({"action":"screenshot"}), ctx())
            .await
            .unwrap();
        if symlink_policy {
            let actual = file.0.with_extension("actual");
            std::fs::rename(&file.0, &actual).unwrap();
            symlink(&actual, &file.0).unwrap();
        } else {
            std::fs::set_permissions(&file.0, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
            .await
            .is_err());
        assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
        assert_eq!(backend.events(), ["screenshot"]);
    }
}

#[tokio::test]
async fn cancelling_during_the_first_target_check_skips_remaining_path_checks() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut backend = PolicyRaceBackend::new();
    backend.target_cancel = Some(cancel.clone());
    let backend = Arc::new(backend);
    let tool = race_tool(backend.clone(), &file);
    let mut context = ctx();
    context.cancel = Some(cancel);
    let result = invoke(
        &tool,
        json!({"action":"left_click_drag","path":[[0,0],[1,1],[2,2],[3,3]]}),
        context,
    )
    .await;
    assert!(matches!(result, Err(ToolError::Aborted)));
    assert_eq!(backend.target_calls.load(Ordering::SeqCst), 1);
    assert!(backend.inner.events().is_empty());
}

#[tokio::test]
async fn revocation_during_drag_start_prevents_mouse_down() {
    for input in [
        json!({"action":"left_click_drag","path":[[1,1],[2,2]]}),
        json!({"action":"left_click_drag","start_coordinate":[1,1],"coordinate":[2,2]}),
        json!({"action":"left_mouse_down","coordinate":[1,1]}),
    ] {
        let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
        let backend = Arc::new(PolicyRaceBackend::new());
        let path = file.0.clone();
        *backend.effect_hook.lock().unwrap() = Some(Box::new(move || {
            replace(&path, policy("revoked", false, json!([])))
        }));
        let tool = race_tool(backend.clone(), &file);
        assert!(invoke(&tool, input, ctx()).await.is_err());
        assert_eq!(backend.inner.events(), ["move:1,1"]);
    }
}
#[tokio::test]
async fn key_repeat_stops_when_first_key_focuses_an_ungranted_app() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let mut backend = PolicyRaceBackend::new();
    backend.switch_on_key = true;
    let backend = Arc::new(backend);
    let tool = race_tool(backend.clone(), &file);
    assert!(
        invoke(&tool, json!({"action":"key","text":"a","repeat":3}), ctx())
            .await
            .is_err()
    );
    assert_eq!(backend.inner.events(), ["key:a"]);
}
#[tokio::test]
async fn revocation_during_app_resolution_prevents_launch() {
    let file = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let backend = Arc::new(PolicyRaceBackend::new());
    let path = file.0.clone();
    *backend.enumeration_hook.lock().unwrap() = Some(Box::new(move || {
        replace(&path, policy("revoked", false, json!([])))
    }));
    let tool = race_tool(backend.clone(), &file);
    assert!(invoke(
        &tool,
        json!({"action":"open_application","bundle_id":"test.app"}),
        ctx()
    )
    .await
    .is_err());
    assert!(!backend.launched.load(Ordering::SeqCst));
}

fn bind_source(envelope: &PolicyFile, source: &PolicyFile) {
    use sha2::{Digest, Sha256};
    let mut value = policy("host-authority", true, json!([app("allow")]));
    value["source"] = json!({"path":source.0,"sha256":format!("{:x}",Sha256::digest(std::fs::read(&source.0).unwrap()))});
    value["ownerPid"] = json!(std::process::id());
    envelope.replace(value);
}

#[tokio::test]
async fn source_authority_envelope_rejects_raw_configuration_changes_before_republish() {
    let source = PolicyFile::new(json!({"enabled":true,"apps":[app("allow")]}));
    let envelope = PolicyFile::new(policy("initial", true, json!([])));
    bind_source(&envelope, &source);
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(envelope.0.clone());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    let authority_bytes = std::fs::read(&envelope.0).unwrap();
    // A source edit must fail closed even before the host's watcher republishes.
    source.replace(json!({"enabled":false,"apps":[]}));
    assert_eq!(std::fs::read(&envelope.0).unwrap(), authority_bytes);
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    assert_eq!(backend.events(), ["screenshot"]);
}

#[cfg(unix)]
#[tokio::test]
async fn source_authority_envelope_rejects_missing_symlink_and_nonprivate_sources() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for failure in ["missing", "symlink", "nonprivate"] {
        let source = PolicyFile::new(json!({"enabled":true}));
        let envelope = PolicyFile::new(policy("initial", true, json!([])));
        bind_source(&envelope, &source);
        let backend = Arc::new(RecordingBackend::default());
        let tool = make_tool(backend.clone()).with_device_policy_path(envelope.0.clone());
        invoke(&tool, json!({"action":"screenshot"}), ctx())
            .await
            .unwrap();
        match failure {
            "missing" => std::fs::remove_file(&source.0).unwrap(),
            "symlink" => {
                let actual = source.0.with_extension("actual");
                std::fs::rename(&source.0, &actual).unwrap();
                symlink(&actual, &source.0).unwrap();
            }
            "nonprivate" => {
                std::fs::set_permissions(&source.0, std::fs::Permissions::from_mode(0o644)).unwrap()
            }
            _ => unreachable!(),
        }
        assert!(
            invoke(&tool, json!({"action":"screenshot"}), ctx())
                .await
                .is_err(),
            "{failure}"
        );
        assert!(
            tool.state.lock().unwrap().allowed_apps.is_empty(),
            "{failure}"
        );
        assert_eq!(backend.events(), ["screenshot"], "{failure}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn authority_envelope_rejects_an_exited_owner_process_without_reviving_grants() {
    let envelope = PolicyFile::new(policy("initial", true, json!([app("allow")])));
    let mut value = policy("host-authority", true, json!([app("allow")]));
    let mut owner = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    value["ownerPid"] = json!(owner.id());
    envelope.replace(value);
    let backend = Arc::new(RecordingBackend::default());
    let tool = make_tool(backend.clone()).with_device_policy_path(envelope.0.clone());
    invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .unwrap();
    owner.kill().unwrap();
    owner.wait().unwrap();
    assert!(invoke(&tool, json!({"action":"screenshot"}), ctx())
        .await
        .is_err());
    assert!(tool.state.lock().unwrap().allowed_apps.is_empty());
    assert_eq!(backend.events(), ["screenshot"]);
}
