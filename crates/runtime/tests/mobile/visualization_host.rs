//! The mobile UniFFI visualization host end to end: mount authorization,
//! single-use documents with the sandbox CSP, CAS state and unmount.

use std::collections::HashMap;
use std::sync::Arc;

use harness_runtime::mobile::visualization_host::{VisualizationHost, VisualizationThemeDto};

#[test]
fn mounts_serve_documents_and_gate_state_through_the_ffi_surface() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::PosixFileSystem::new(home.path().to_path_buf()),
    );
    let host = VisualizationHost::for_config_home(
        fs.clone(),
        home.path(),
        "https://lingxi-visualization.invalid",
        runtime.handle().clone(),
    )
    .unwrap();
    assert!(VisualizationHost::for_config_home(
        fs.clone(),
        home.path(),
        "https://x.test/path",
        runtime.handle().clone()
    )
    .is_none());
    let session = lingxi_core::types::SessionId::new();
    let store = harness_runtime::inline_visualization::shared_store(fs, home.path());
    let reference = runtime
        .block_on(store.publish(
            &visualization::Publisher {
                root_session: session.as_uuid(),
                agent_id: None,
            },
            None,
            "Chart",
            "<div id=\"widget\">hi</div>",
            1,
        ))
        .unwrap()
        .reference;
    let dark = VisualizationThemeDto {
        dark: true,
        tokens: HashMap::from([("--color-background-primary".into(), "#000".into())]),
    };
    assert!(
        host.mount(
            lingxi_core::types::SessionId::new().to_string(),
            reference.id.to_string(),
            1,
            dark.clone(),
            "en".into(),
            false
        )
        .is_none(),
        "another conversation cannot mount this widget"
    );
    let mount = host
        .mount(
            session.to_string(),
            reference.id.to_string(),
            1,
            dark,
            "zh-CN".into(),
            false,
        )
        .unwrap();
    assert_eq!(mount.title, "Chart");
    let path = mount.doc_url.trim_start_matches(&host.origin()).to_string();
    let document = host.serve(path.clone());
    assert_eq!(
        (document.status, document.mime_type.as_str()),
        (200, "text/html")
    );
    assert!(document
        .headers
        .iter()
        .any(|header| header.name == "Content-Security-Policy"
            && header.value.starts_with("sandbox allow-scripts;")));
    let html = String::from_utf8(document.body).unwrap();
    assert!(
        html.contains("data-theme=\"dark\"") && html.contains("--color-background-primary:#000;")
    );
    assert_eq!(host.serve(path).status, 404, "documents are single use");
    assert_eq!(host.serve("/shell.html".into()).status, 200);
    assert_eq!(
        host.serve("/asset/d3.min.js".into()).mime_type,
        "text/javascript"
    );

    let saved = host.write_state(
        mount.token.clone(),
        mount.generation,
        0,
        "{}".into(),
        "null".into(),
    );
    assert!(saved.saved && saved.version == 1);
    let conflict = host.write_state(
        mount.token.clone(),
        mount.generation,
        0,
        "{}".into(),
        "null".into(),
    );
    assert_eq!(conflict.reason.as_deref(), Some("conflict"));
    assert!(conflict
        .current_state_json
        .unwrap()
        .contains("\"version\":1"));
    assert_eq!(host.list(session.to_string()).len(), 1);
    host.unmount(mount.token.clone());
    assert_eq!(
        host.write_state(mount.token, mount.generation, 1, "{}".into(), "null".into())
            .reason
            .as_deref(),
        Some("stale_mount")
    );
    assert!(host.shell_url().ends_with("/shell.html"));
    assert!(host.third_party_notices().contains("Apache License"));
}
