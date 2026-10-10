use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::host::device_status::{DeviceStatus, DeviceStatusError, DeviceStatusProvider};
use lingxi_core::host::location::{LocationError, LocationFix, LocationProvider};
use lingxi_core::host::{
    Clock, FileSystem, HttpTransport, Platform, ProcessRunner, Sandbox, SlashCommandDispatcher,
    SlashDispatchResult, WorktreeManager,
};
use session::jsonl::SessionMode;
use tool_api::tool_trait::ToolStaticContext;

use super::{build_mobile, test_config, FakeListener, HostFakePlatform, RecordingPermissionSink};

const DEVICE_TOOLS: &[&str] = &[
    "camera",
    "voice",
    "speech",
    "clipboard",
    "share",
    "notification",
    "location",
    "device_status",
    "haptics",
    "open_url",
    "calendar",
    "contacts",
];

const DEVICE_SKILLS: &[&str] = &["mobile-device", "mobile-media", "mobile-personal-context"];

struct NativeDevice;

#[async_trait]
impl DeviceStatusProvider for NativeDevice {
    async fn status(&self) -> Result<DeviceStatus, DeviceStatusError> {
        Ok(DeviceStatus {
            battery_percent: Some(73.0),
            charging: Some(true),
            network: "wifi".into(),
            low_power_mode: None,
        })
    }
}

#[async_trait]
impl LocationProvider for NativeDevice {
    async fn current_location(&self) -> Result<LocationFix, LocationError> {
        Ok(LocationFix {
            latitude: 37.5,
            longitude: -122.2,
            accuracy_m: Some(8.0),
            timestamp_ms: 1_000,
        })
    }
}

/// Use the same portable host as other runtime tests, adding only the native
/// seams under test so availability must flow through production assembly.
struct DevicePlatform(HostFakePlatform);

impl Platform for DevicePlatform {
    fn filesystem(&self) -> Arc<dyn FileSystem> {
        self.0.filesystem()
    }
    fn http(&self) -> Arc<dyn HttpTransport> {
        self.0.http()
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.0.clock()
    }
    fn process(&self) -> Arc<dyn ProcessRunner> {
        self.0.process()
    }
    fn sandbox(&self) -> Arc<dyn Sandbox> {
        self.0.sandbox()
    }
    fn worktree(&self) -> Arc<dyn WorktreeManager> {
        self.0.worktree()
    }
    fn device_status(&self) -> Option<Arc<dyn DeviceStatusProvider>> {
        Some(Arc::new(NativeDevice))
    }
    fn location(&self) -> Option<Arc<dyn LocationProvider>> {
        Some(Arc::new(NativeDevice))
    }
}

#[tokio::test]
async fn native_device_tools_and_skills_are_wired_in_chat_and_code() {
    for mode in [SessionMode::Chat, SessionMode::Code] {
        let root = tempfile::tempdir().expect("tempdir");
        let mut config = test_config(root.path());
        config.session_mode = mode;
        let runtime = build_mobile(
            config,
            Arc::new(DevicePlatform(HostFakePlatform::new(root.path().into()))),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
        )
        .await
        .expect("build mobile runtime with native providers");

        let tools = runtime
            .mcp_tool_registry
            .available_tools(&ToolStaticContext::default());
        let native_names: Vec<_> = tools
            .iter()
            .map(|tool| tool.name())
            .filter(|name| DEVICE_TOOLS.contains(name))
            .collect();
        assert_eq!(
            native_names.len(),
            2,
            "only supplied backends: {native_names:?}"
        );
        for name in ["device_status", "location"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name() == name)
                .unwrap_or_else(|| panic!("{name} must be available in {mode:?}"));
            let input = serde_json::json!({});
            let ctx = tool_api::test_support::fresh_ctx();
            let permission = tool.check_permissions(&input, &ctx).await;
            if name == "location" {
                assert!(matches!(
                    permission,
                    permission::PermissionResult::Ask { .. }
                ));
            } else {
                assert!(matches!(
                    permission,
                    permission::PermissionResult::Allow { .. }
                ));
            }
            let (progress, _receiver) = tool_api::progress::progress_channel();
            let result = tool.call(input, ctx, progress).await.expect("native call");
            if name == "device_status" {
                assert_eq!(result.data["battery_percent"], 73.0);
                assert_eq!(result.data["network"], "wifi");
                assert!(result.data["low_power_mode"].is_null());
            } else {
                assert_eq!(result.data["latitude"], 37.5);
                assert_eq!(result.data["accuracy_m"], 8.0);
            }
        }

        // Keep checking the actual provider and loader retained by assembly;
        // constructing replacements here would miss broken production wiring.
        for reloaded in [false, true] {
            if reloaded {
                assert!(matches!(
                    runtime.dispatcher.dispatch("/reload-skills").await,
                    SlashDispatchResult::Handled { .. }
                ));
            }
            let listing = runtime.wired_skill_listing_provider.skill_entries().await;
            for (name, section, absent_section) in [
                ("mobile-device", "device_status", "clipboard"),
                ("mobile-personal-context", "location", "calendar"),
            ] {
                assert!(
                    listing.iter().any(|entry| entry.name == name),
                    "{name} must stay discoverable in {mode:?} after reload={reloaded}"
                );
                let skill = runtime
                    .wired_skill_loader
                    .load(name)
                    .await
                    .expect("load skill")
                    .expect("native skill present");
                assert!(
                    skill.allowed_tools.is_empty(),
                    "a skill must not grant permissions"
                );
                let body = skill.dynamic_body.expect("native prompt").build("");
                assert!(body.contains(&format!("## {section}\n")));
                assert!(!body.contains(&format!("## {absent_section}\n")));
            }
            assert!(!listing.iter().any(|entry| entry.name == "mobile-media"));
        }
    }
}

#[tokio::test]
async fn missing_device_backends_do_not_advertise_tools_or_skills() {
    for mode in [SessionMode::Chat, SessionMode::Code] {
        let root = tempfile::tempdir().expect("tempdir");
        let mut config = test_config(root.path());
        config.session_mode = mode;
        let runtime = build_mobile(
            config,
            Arc::new(HostFakePlatform::new(root.path().into())),
            Arc::new(FakeListener::default()),
            Arc::new(RecordingPermissionSink::default()),
        )
        .await
        .expect("build mobile runtime without native providers");
        let tools = runtime
            .mcp_tool_registry
            .available_tools(&ToolStaticContext::default());
        assert!(tools
            .iter()
            .all(|tool| !DEVICE_TOOLS.contains(&tool.name())));
        assert!(matches!(
            runtime.dispatcher.dispatch("/reload-skills").await,
            SlashDispatchResult::Handled { .. }
        ));
        let listing = runtime.wired_skill_listing_provider.skill_entries().await;
        for name in DEVICE_SKILLS {
            assert!(!listing.iter().any(|entry| entry.name == *name));
            assert!(runtime
                .wired_skill_loader
                .load(name)
                .await
                .expect("load absent native skill")
                .is_none());
        }
    }
}
