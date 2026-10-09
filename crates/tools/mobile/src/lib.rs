//! `tool-mobile` — native device services exposed as typed builtin tools.
//!
//! Folds the former one-crate-per-tool split (`tool-share` / `tool-camera` /
//! `tool-voice` / `tool-notification` / `tool-clipboard` / `tool-speech`) into a
//! single crate of submodules. Every tool here is pure Rust that routes to an
//! `Arc<dyn …Control>` capability carried in [`tool_api::BuiltinToolContext`].
//!
//! ## Two composition roots, two entry points
//!
//! - `engine-mobile` calls [`register_all`] — tools backed by its Swift / Kotlin
//!   services injected via `UniFFI`, including device state and personal data.
//! - `engine-desktop` calls [`register_audio`] — ONLY `voice` + `speech`. Their
//!   capabilities are `bridge_server::audio_bridge::AudioBridge`, which proxies
//!   each trait call to the connected Electron client over the wire. The desktop
//!   has no counterpart for camera / share / notification / clipboard, so those
//!   four stay mobile-only.
//!
//! The crate NAME is therefore no longer accurate — `voice` and `speech` are not
//! mobile-exclusive any more. Deliberately NOT renamed: a rename churns every
//! call site and path for no behavioural gain, and this note is the cheaper fix.

#![forbid(unsafe_code)]

mod audio_support;
pub mod calendar;
pub mod camera;
pub mod clipboard;
pub mod contacts;
pub mod device_status;
mod device_support;
pub mod haptics;
pub mod location;
pub mod notification;
pub mod open_url;
pub mod share;
pub mod speech;
pub mod voice;

pub use calendar::CalendarTool;
pub use camera::CameraTool;
pub use clipboard::ClipboardTool;
pub use contacts::ContactsTool;
pub use device_status::DeviceStatusTool;
pub use haptics::HapticsTool;
pub use location::LocationTool;
pub use notification::NotificationTool;
pub use open_url::OpenUrlTool;
pub use share::ShareTool;
pub use speech::SpeechTool;
pub use voice::VoiceTool;

/// Register the typed device tools backed by this host into `reg`.
///
/// Replaces the former per-crate `tool_<name>::register_all` calls the
/// composition root made; the ordering matches the previous wiring.
///
/// Audio actions are projected from supported operations. Readiness such as
/// permission, model installation, or device busy state does not remove them.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;

    if ctx.camera.is_some() {
        camera::register_all(reg, ctx.clone());
    }
    register_audio(reg, &ctx);
    if ctx.notifications.is_some() {
        notification::register_all(reg, ctx.clone());
    }
    if ctx.clipboard.is_some() {
        clipboard::register_all(reg, ctx.clone());
    }
    if ctx.share.is_some() {
        share::register_all(reg, ctx.clone());
    }
    if ctx.location.is_some() {
        reg.register_builtin(Arc::new(LocationTool::new(ctx.clone())));
    }
    if ctx.device_status.is_some() {
        reg.register_builtin(Arc::new(DeviceStatusTool::new(ctx.clone())));
    }
    if ctx.haptics.is_some() {
        reg.register_builtin(Arc::new(HapticsTool::new(ctx.clone())));
    }
    if ctx.deep_link.is_some() {
        reg.register_builtin(Arc::new(OpenUrlTool::new(ctx.clone())));
    }
    if ctx.calendar.is_some() {
        reg.register_builtin(Arc::new(CalendarTool::new(ctx.clone())));
    }
    if ctx.contacts.is_some() {
        reg.register_builtin(Arc::new(ContactsTool::new(ctx)));
    }
}

/// Register audio tools whenever an audio service is wired. Their per-request
/// enabled state and schema snapshot project the service's current support,
/// since desktop capability snapshots can arrive after tool construction.
///
/// This is the desktop composition root's entry point. It is narrower than
/// [`register_all`] in two deliberate ways:
///
/// 1. **Only audio tools.** The desktop's audio comes from the Electron
///    client's microphone and speaker; it has no device camera, share sheet,
///    push-notification service or mobile clipboard behind the other four, and
///    registering them would widen the desktop tool surface with tools that
///    could only ever fail.
/// 2. **Gated on support, not readiness.** Supported actions remain advertised
///    while permission is needed, a model is missing, or the device is busy.
///    Unsupported operations do not appear in the live model-facing schema.
pub fn register_audio(reg: &mut tool_api::ToolRegistry, ctx: &tool_api::BuiltinToolContext) {
    if ctx.audio.is_none() {
        return;
    }
    voice::register_all(reg, ctx.clone());
    speech::register_all(reg, ctx.clone());
}

#[cfg(test)]
mod register_audio_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use lingxi_core::host::audio::{
        AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioOperation, AudioOperationContext,
        AudioOperationId, AudioOperationKind, AudioOperationSuccess, AudioService,
    };
    use mobile_linux_api::ProcessOutput;
    use tool_api::{BuiltinToolContext, ToolRegistry};

    struct StubAudio(Vec<AudioOperationKind>);

    #[async_trait]
    impl AudioService for StubAudio {
        fn capabilities(&self) -> AudioCapabilitySnapshot {
            AudioCapabilitySnapshot {
                service_epoch: 1,
                support_revision: 1,
                supported_operations: self.0.clone(),
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            _context: AudioOperationContext,
            _operation: AudioOperation,
        ) -> Result<AudioOperationSuccess, AudioError> {
            Err(AudioError::new(AudioErrorKind::Unavailable, "test stub"))
        }

        async fn cancel(&self, _identity: AudioOperationId) -> Result<(), AudioError> {
            Ok(())
        }
    }

    fn bare_ctx() -> BuiltinToolContext {
        tool_api::test_support::shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }

    fn audio_names(ctx: &BuiltinToolContext) -> Vec<String> {
        let mut reg = ToolRegistry::new();
        super::register_audio(&mut reg, ctx);
        reg.all_names()
    }

    #[test]
    fn register_audio_registers_nothing_without_an_audio_service() {
        assert!(
            audio_names(&bare_ctx()).is_empty(),
            "with no capability wired, a tool that could only ever fail must not \
             be advertised at all"
        );
    }

    #[test]
    fn register_audio_registers_only_supported_operations() {
        let audio = Arc::new(StubAudio(vec![
            AudioOperationKind::Record,
            AudioOperationKind::Listen,
            AudioOperationKind::Speak,
        ]));
        let ctx = BuiltinToolContext {
            audio: Some(audio),
            ..bare_ctx()
        };
        let names = audio_names(&ctx);
        assert_eq!(
            names,
            vec!["voice".to_string(), "speech".to_string()],
            "exactly the two audio tools, in the same order `register_all` uses"
        );
    }

    #[test]
    fn register_audio_projects_current_support_without_gating_on_readiness() {
        let recorder_only = BuiltinToolContext {
            audio: Some(Arc::new(StubAudio(vec![AudioOperationKind::Record]))),
            ..bare_ctx()
        };
        assert_eq!(
            audio_names(&recorder_only),
            vec!["voice".to_string(), "speech".to_string()],
            "both tools remain registered so later capability snapshots can update their schemas"
        );

        let playback_only = BuiltinToolContext {
            audio: Some(Arc::new(StubAudio(vec![AudioOperationKind::Speak]))),
            ..bare_ctx()
        };
        assert_eq!(
            audio_names(&playback_only),
            vec!["voice".to_string(), "speech".to_string()],
            "service support is projected by the tools without readiness gating"
        );
    }

    #[test]
    fn register_all_omits_every_tool_without_a_backend() {
        let mut reg = ToolRegistry::new();
        super::register_all(&mut reg, bare_ctx());
        assert!(
            reg.all_names().is_empty(),
            "unwired native services must not be advertised"
        );
    }
}

#[cfg(test)]
mod device_tools_tests;
