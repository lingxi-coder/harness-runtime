//! Android UI automation capability.
//!
//! The engine and tools only depend on this safe, data-only contract. Android
//! framework objects (`AccessibilityNodeInfo`, `Bitmap`, `MediaProjection`, …)
//! stay on the Kotlin side of the UniFFI boundary.

#![allow(missing_docs)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_ANDROID_UI_NODES: usize = 500;
pub const MAX_ANDROID_UI_DEPTH: usize = 50;
pub const MAX_ANDROID_UI_BATCH: usize = 20;
pub const MAX_ANDROID_UI_WAIT_MS: u64 = 30_000;
pub const MAX_ANDROID_AUDIO_LISTEN_MS: u64 = 60_000;
pub const MAX_ANDROID_AUDIO_SPEAK_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidAccessTier {
    Read,
    Click,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidAutomationSessionState {
    Inactive,
    Starting,
    Active,
    AwaitingApproval,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidCaptureMode {
    None,
    Accessibility,
    MediaProjection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AndroidAutomationStatus {
    pub service_enabled: bool,
    pub session_state: AndroidAutomationSessionState,
    pub capture_mode: AndroidCaptureMode,
    pub active_package: Option<String>,
    pub display_width: u32,
    pub display_height: u32,
    pub remaining_ms: Option<u64>,
    pub detail: Option<String>,
    #[serde(default)]
    pub audio_listen_enabled: bool,
    #[serde(default)]
    pub audio_speak_enabled: bool,
    #[serde(default)]
    pub audio_input_language: Option<String>,
    #[serde(default)]
    pub audio_voice: Option<String>,
    #[serde(default = "default_android_audio_speed")]
    pub audio_speed: f32,
}

fn default_android_audio_speed() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidAppInfo {
    pub package_name: String,
    pub display_name: String,
    pub tier: Option<AndroidAccessTier>,
    pub system_ui: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidAccessRequest {
    pub reason: String,
    pub apps: Vec<String>,
    pub tier: AndroidAccessTier,
    pub clipboard_read: bool,
    pub clipboard_write: bool,
    pub include_system_ui: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidUiNode {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub package_name: String,
    pub class_name: String,
    pub resource_id: Option<String>,
    pub text: Option<String>,
    pub content_description: Option<String>,
    pub bounds: AndroidRect,
    pub clickable: bool,
    pub long_clickable: bool,
    pub scrollable: bool,
    pub editable: bool,
    pub enabled: bool,
    pub selected: bool,
    pub checked: Option<bool>,
    pub password: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidUiSnapshot {
    pub package_name: String,
    pub activity_name: Option<String>,
    pub window_id: i32,
    pub generation: u64,
    pub captured_at_ms: u64,
    pub nodes: Vec<AndroidUiNode>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidNodeQuery {
    pub text: Option<String>,
    pub content_description: Option<String>,
    pub resource_id: Option<String>,
    pub class_name: Option<String>,
    pub clickable: Option<bool>,
    pub editable: Option<bool>,
    pub enabled: Option<bool>,
    pub limit: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidScreenshot {
    pub width: u32,
    pub height: u32,
    pub png_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidAudioListenRequest {
    pub language: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AndroidAudioTranscript {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AndroidAudioSpeakRequest {
    pub text: String,
    pub voice: Option<String>,
    pub speed: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidAudioSpeakResult {
    pub completed: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidGlobalAction {
    Back,
    Home,
    Recents,
    Notifications,
    QuickSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AndroidAction {
    Tap {
        node_id: Option<String>,
        x: Option<u32>,
        y: Option<u32>,
    },
    LongPress {
        node_id: Option<String>,
        x: Option<u32>,
        y: Option<u32>,
        duration_ms: u64,
    },
    SetText {
        node_id: Option<String>,
        text: String,
    },
    ClearText {
        node_id: Option<String>,
    },
    Global {
        action: AndroidGlobalAction,
    },
    Enter {
        node_id: Option<String>,
    },
    Direction {
        direction: String,
    },
    Scroll {
        node_id: Option<String>,
        x: Option<u32>,
        y: Option<u32>,
        direction: String,
        amount: u32,
    },
    Swipe {
        start_x: u32,
        start_y: u32,
        end_x: u32,
        end_y: u32,
        duration_ms: u64,
    },
    Pinch {
        center_x: u32,
        center_y: u32,
        scale: f32,
        duration_ms: u64,
    },
    OpenApp {
        package_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidActionResult {
    pub success: bool,
    pub package_name: Option<String>,
    pub generation: Option<u64>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AndroidWaitCondition {
    NodeAppears {
        query: AndroidNodeQuery,
    },
    NodeDisappears {
        query: AndroidNodeQuery,
    },
    Activity {
        package_name: String,
        activity_name: Option<String>,
    },
    Idle {
        quiet_ms: u64,
    },
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AndroidAutomationError {
    #[error("Android accessibility service is not enabled")]
    ServiceDisabled,
    #[error("Android Computer Use session is not active")]
    SessionInactive,
    #[error("Android Computer Use permission denied: {0}")]
    PermissionDenied(String),
    #[error("Android package is not allowed for this session: {0}")]
    TargetNotAllowed(String),
    #[error("Android Computer Use permission tier is insufficient: {0}")]
    TierInsufficient(String),
    #[error("Android surface is protected: {0}")]
    ProtectedSurface(String),
    #[error("Android UI node is stale: {0}")]
    StaleNode(String),
    #[error("Android UI operation timed out: {0}")]
    Timeout(String),
    #[error("Android UI operation is unsupported: {0}")]
    Unsupported(String),
    #[error("Android UI operation failed: {0}")]
    Other(String),
}

#[async_trait]
pub trait AndroidUiAutomation: Send + Sync {
    async fn status(&self) -> Result<AndroidAutomationStatus, AndroidAutomationError>;
    async fn request_access(
        &self,
        request: AndroidAccessRequest,
    ) -> Result<Vec<AndroidAppInfo>, AndroidAutomationError>;
    async fn list_granted_apps(&self) -> Result<Vec<AndroidAppInfo>, AndroidAutomationError>;
    async fn screenshot(&self) -> Result<AndroidScreenshot, AndroidAutomationError>;
    async fn ui_tree(&self) -> Result<AndroidUiSnapshot, AndroidAutomationError>;
    async fn find_nodes(
        &self,
        query: AndroidNodeQuery,
    ) -> Result<Vec<AndroidUiNode>, AndroidAutomationError>;
    async fn inspect_node(&self, node_id: String) -> Result<AndroidUiNode, AndroidAutomationError>;
    async fn perform(
        &self,
        action: AndroidAction,
    ) -> Result<AndroidActionResult, AndroidAutomationError>;
    async fn wait_for(
        &self,
        condition: AndroidWaitCondition,
        timeout_ms: u64,
    ) -> Result<AndroidActionResult, AndroidAutomationError>;
    async fn listen(
        &self,
        _request: AndroidAudioListenRequest,
    ) -> Result<AndroidAudioTranscript, AndroidAutomationError> {
        Err(AndroidAutomationError::Unsupported(
            "audio listening is not available on this Android host".into(),
        ))
    }
    async fn speak(
        &self,
        _request: AndroidAudioSpeakRequest,
    ) -> Result<AndroidAudioSpeakResult, AndroidAutomationError> {
        Err(AndroidAutomationError::Unsupported(
            "audio speech is not available on this Android host".into(),
        ))
    }
    async fn stop_audio(&self) -> Result<(), AndroidAutomationError> {
        Err(AndroidAutomationError::Unsupported(
            "audio control is not available on this Android host".into(),
        ))
    }
    async fn stop(&self) -> Result<(), AndroidAutomationError>;
}
