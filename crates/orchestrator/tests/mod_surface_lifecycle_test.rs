//! Mod render surfaces follow their engine lifecycle without duplicate callbacks.

use std::sync::Arc;

use hooks::mods::{ModHost, ModSessionContext};
use lingxi_core::host::OrchestratorHandle;
use orchestrator::config::ModRenderSurface;
use orchestrator::mod_surface_roster::{ModSurfaceDetachReason, ModSurfaceRoster};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};

async fn mod_logs(output: &MockOutputStream) -> Vec<String> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn ui_lifecycle_updates_roster_before_events_and_survives_in_place_switches() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("surface-lifecycle.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.attach', async ($, e, next) => {
            const surfaces = await $.session.surfaces();
            $.ui.log(`attach:${e.clientId}:${surfaces.join(',')}`);
            return next(e);
          });
          on('session.detach', async ($, e, next) => {
            const surfaces = await $.session.surfaces();
            $.ui.log(`detach:${e.clientId}:${e.reason}:${surfaces.join(',')}`);
            return next(e);
          });
        }"#,
    )
    .unwrap();

    let host = ModHost::start(None).await.unwrap();
    host.load(
        "surface-lifecycle",
        directory.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert!(host.has_event("session.attach"));
    assert!(host.has_event("session.detach"));
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let registry_host = registry.mod_host().expect("loaded ModHost in registry");
    assert!(
        Arc::ptr_eq(&host, &registry_host),
        "HookRegistry must dispatch through the exact loaded ModHost"
    );
    drop(registry_host);
    let output = Arc::new(MockOutputStream::new());
    let roster = Arc::new(ModSurfaceRoster::default());
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig {
            interactive_session: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
    .with_mod_surface_roster(roster.clone());

    assert!(
        orchestrator
            .mod_ui_attach("renderer-a", ModRenderSurface::Desktop)
            .await
    );
    assert_eq!(
        ModSessionContext::surfaces(&orchestrator),
        vec!["terminal".to_string(), "desktop".to_string()]
    );
    // Duplicate attach is idempotent and does not raise a second event.
    assert!(
        !orchestrator
            .mod_ui_attach("renderer-a", ModRenderSurface::Vscode)
            .await
    );
    assert!(
        orchestrator
            .mod_ui_attach("renderer-b", ModRenderSurface::Desktop)
            .await
    );

    let initial_id = orchestrator.current_session_id().await;
    orchestrator.clear_session().await.unwrap();
    assert_ne!(orchestrator.current_session_id().await, initial_id);
    assert_eq!(
        ModSessionContext::surfaces(&orchestrator),
        vec!["terminal".to_string(), "desktop".to_string()],
        "native clear keeps its attached UI clients on the live process"
    );

    let resumed_id = lingxi_core::types::SessionId::new();
    orchestrator
        .resume_session(
            resumed_id,
            vec![],
            None,
            None,
            lingxi_core::host::ResumeRuntimeSnapshot::default(),
        )
        .await
        .unwrap();
    assert_eq!(orchestrator.current_session_id().await, resumed_id);
    assert_eq!(
        ModSessionContext::surfaces(&orchestrator),
        vec!["terminal".to_string(), "desktop".to_string()],
        "native resume also preserves attached UI clients"
    );

    // A genuinely ending session removes every still-attached client. A
    // repeated end cleanup sees an empty roster and emits nothing twice.
    orchestrator
        .mod_ui_detach_all(ModSurfaceDetachReason::End)
        .await;
    let after_end = mod_logs(&output).await;
    orchestrator
        .mod_ui_detach_all(ModSurfaceDetachReason::End)
        .await;
    assert_eq!(
        ModSessionContext::surfaces(&orchestrator),
        vec!["terminal".to_string()]
    );

    assert!(
        after_end
            .iter()
            .any(|log| log == "attach:renderer-a:terminal,desktop"),
        "missing renderer-a attach ModLog; actual ModLog entries: {after_end:?}"
    );
    assert!(
        after_end
            .iter()
            .any(|log| log == "attach:renderer-b:terminal,desktop"),
        "missing renderer-b attach ModLog; actual ModLog entries: {after_end:?}"
    );
    assert!(
        after_end
            .iter()
            .any(|log| log == "detach:renderer-a:end:terminal,desktop"),
        "missing renderer-a detach ModLog; actual ModLog entries: {after_end:?}"
    );
    assert!(
        after_end
            .iter()
            .any(|log| log == "detach:renderer-b:end:terminal"),
        "missing renderer-b detach ModLog; actual ModLog entries: {after_end:?}"
    );
    assert_eq!(
        after_end
            .iter()
            .filter(|log| log.starts_with("detach:"))
            .count(),
        2,
        "clear/resume must not synthesize detach and end cleanup must be once per client"
    );
    assert_eq!(mod_logs(&output).await, after_end);
}
