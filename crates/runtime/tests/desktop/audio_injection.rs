//! The desktop audio capability seam, both halves of it.
//!
//! `DesktopConfig::audio` → the shared `AudioService` tool context → whether
//! the `voice` and `speech` tools are registered at all. Two properties are
//! pinned here, and the second one is the reason this file exists:
//!
//! 1. **Injection.** A capability put on the config reaches the tool context
//!    `build` assembles. Asserted through the registry, because that is the only
//!    honest observation point: the context itself is consumed by `build`, and a
//!    capability that reached it but registered nothing would be invisible to
//!    the model anyway.
//! 2. **The gate is real in BOTH directions.** With no capability, the two tools
//!    must be ABSENT — not registered-but-failing. A tool the model can call but
//!    that can only ever error is worse than an absent tool: it burns a turn and
//!    teaches the model something false about what this app can do. The
//!    no-capability case is therefore asserted as a statement about the
//!    registry's contents, not as "nothing panicked".
//!
//! The capability is per-tool, not all-or-nothing: `voice` needs a recorder,
//! `speech` needs a recognizer or a synthesizer. The two half-wired cases below
//! pin that, so a future edit cannot collapse the gate into one `is_some()`
//! without a named failure.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use async_trait::async_trait;
use harness_runtime::desktop::{build, desktop_tool_registry, DesktopAudio, DesktopConfig};
use platform_api::audio::{
    AudioCapabilitySnapshot, AudioError, AudioErrorKind, AudioOperation, AudioOperationContext,
    AudioOperationKind, AudioOperationSuccess, AudioService,
};
use platform_api::process::ProcessOutput;
use tool_api::BuiltinToolContext;

/// A stand-in for `bridge_server::audio_bridge::AudioBridge`. Nothing here is
/// ever called — these tests ask what the composition
/// root does with a capability, not what the capability does — but it has to be
/// a real implementation for the root to accept it.
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

    async fn cancel(
        &self,
        _identity: platform_api::audio::AudioOperationId,
    ) -> Result<(), AudioError> {
        Ok(())
    }
}

/// The fully-stubbed context the desktop registry factory is exercised with —
/// the same one the locked tool-list snapshot uses, so "what changed" here is
/// exactly the audio fields.
fn stub_ctx() -> BuiltinToolContext {
    tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    })
}

/// The desktop tool names assembled from `ctx`.
fn registered_tool_names(ctx: BuiltinToolContext) -> Vec<String> {
    desktop_tool_registry(ctx, None, None).all_names()
}

#[test]
fn without_an_audio_capability_the_desktop_registers_neither_audio_tool() {
    let names = registered_tool_names(stub_ctx());
    assert!(
        !names.contains(&"speech".to_string()),
        "the speech tool must not be registered with no recognizer or synthesizer \
         behind it; registered: {names:?}"
    );
    assert!(
        !names.contains(&"voice".to_string()),
        "the voice tool must not be registered with no recorder behind it; \
         registered: {names:?}"
    );
}

#[test]
fn with_an_audio_capability_the_desktop_registers_speech_and_voice() {
    let audio = DesktopAudio::from_single(Arc::new(StubAudio(vec![
        AudioOperationKind::Record,
        AudioOperationKind::Listen,
        AudioOperationKind::Speak,
    ])));
    let ctx = BuiltinToolContext {
        audio: Some(audio.service.clone()),
        ..stub_ctx()
    };
    let names = registered_tool_names(ctx);
    assert!(
        names.contains(&"speech".to_string()),
        "a wired recognizer + synthesizer must register the speech tool; \
         registered: {names:?}"
    );
    assert!(
        names.contains(&"voice".to_string()),
        "a wired recorder must register the voice tool; registered: {names:?}"
    );
}

#[test]
fn a_recorder_alone_registers_voice_but_not_speech() {
    let ctx = BuiltinToolContext {
        audio: Some(Arc::new(StubAudio(vec![AudioOperationKind::Record]))),
        ..stub_ctx()
    };
    let names = registered_tool_names(ctx);
    assert!(
        names.contains(&"voice".to_string()),
        "a wired recorder must register the voice tool; registered: {names:?}"
    );
    assert!(
        !names.contains(&"speech".to_string()),
        "a recorder is not a recognizer: the speech tool must stay absent; \
         registered: {names:?}"
    );
}

#[test]
fn a_recognizer_alone_registers_speech_but_not_voice() {
    let ctx = BuiltinToolContext {
        audio: Some(Arc::new(StubAudio(vec![AudioOperationKind::Listen]))),
        ..stub_ctx()
    };
    let names = registered_tool_names(ctx);
    assert!(
        names.contains(&"speech".to_string()),
        "a wired recognizer must register the speech tool (its `speak` action \
         reports the missing synthesizer per-call); registered: {names:?}"
    );
    assert!(
        !names.contains(&"voice".to_string()),
        "a recognizer is not a raw recorder: the voice tool must stay absent; \
         registered: {names:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The injection point itself: `DesktopConfig::audio` → `build` → the registry.
// ─────────────────────────────────────────────────────────────────────────────

/// A no-op permission sink; these builds never push a permission request.
struct NoopPermissionSink;

#[async_trait]
impl client::adapter::PermissionRequestSink for NoopPermissionSink {
    async fn emit_request(&self, _request: client::protocol::permission::PermissionRequest) {}
}

/// A deterministic, env-free config rooted at a temp dir. `isolated_credential_storage`
/// keeps a developer's login keychain out of the build; `session_persistence: false`
/// keeps it off the real transcript store.
fn sandbox_config(audio: Option<DesktopAudio>) -> (tempfile::TempDir, DesktopConfig) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let cfg = DesktopConfig {
        cwd: cwd.clone(),
        lingxi_home: cwd.join(".lingxi"),
        isolated_credential_storage: true,
        session_persistence: false,
        audio,
        ..DesktopConfig::default()
    };
    (tmp, cfg)
}

async fn run_build(cfg: DesktopConfig) -> harness_runtime::desktop::DesktopRuntime {
    let output: Arc<dyn platform_api::OutputStream> =
        Arc::new(orchestrator::test_support::MockOutputStream::new());
    let perm_sink: Arc<dyn client::adapter::PermissionRequestSink> = Arc::new(NoopPermissionSink);
    // `build` returns a large future; boxing it keeps this test off the stack
    // limit and quiet under `clippy::large_futures`.
    Box::pin(build(cfg, output, perm_sink))
        .await
        .expect("the sandbox config must build")
}

#[tokio::test]
async fn a_build_with_no_audio_config_advertises_no_audio_tools() {
    let (_tmp, cfg) = sandbox_config(None);
    let runtime = Box::pin(run_build(cfg)).await;
    assert!(
        !runtime.has_audio(),
        "a config with no audio must not conjure a capability"
    );
    let names = runtime.registered_tool_names();
    assert!(
        !names.contains(&"voice".to_string()),
        "no capability ⇒ the voice tool must not be in the assembled registry; \
         registered: {names:?}"
    );
    assert!(
        !names.contains(&"speech".to_string()),
        "no capability ⇒ the speech tool must not be in the assembled registry; \
         registered: {names:?}"
    );
}

#[tokio::test]
async fn a_build_with_an_audio_config_advertises_both_audio_tools() {
    let (_tmp, cfg) = sandbox_config(Some(DesktopAudio::from_single(Arc::new(StubAudio(vec![
        AudioOperationKind::Record,
        AudioOperationKind::Listen,
        AudioOperationKind::Speak,
    ])))));
    let runtime = Box::pin(run_build(cfg)).await;
    assert!(
        runtime.has_audio(),
        "the runtime must surface the capability it was built with"
    );
    // The registry is the proof that the capability reached the TOOL CONTEXT,
    // and it is a SEPARATE observation from `has_audio` above (which reads the
    // capability itself). Registration is gated on supported AudioService
    // operations, so a build that carried the config field onto the runtime but
    // dropped it on the way to the context passes the first assertion and fails
    // these two.
    let names = runtime.registered_tool_names();
    assert!(
        names.contains(&"voice".to_string()),
        "DesktopConfig::audio must reach the tool context and register the voice \
         tool; registered: {names:?}"
    );
    assert!(
        names.contains(&"speech".to_string()),
        "DesktopConfig::audio must reach the tool context and register the speech \
         tool; registered: {names:?}"
    );
}
