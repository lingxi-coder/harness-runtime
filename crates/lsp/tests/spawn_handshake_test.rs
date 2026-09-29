//! Integration test: spawn the mock LSP binary through `PosixLspTransport`
//! and drive a full initialize -> shutdown cycle.
//!
//! This is the end-to-end exercise of the ENOENT guard + connection setup +
//! request round-trip.

use lingxi_core::host::LspServerConfig;
use std::collections::HashMap;
use test_harness::mocks::mock_lsp_server::mock_lsp_server_path;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn mock_config() -> LspServerConfig {
    let bin = mock_lsp_server_path();
    let mut ext = HashMap::new();
    ext.insert(".rs".into(), "rust".into());
    LspServerConfig {
        name: "mock".into(),
        command: bin.to_string_lossy().into_owned(),
        args: vec![],
        env: HashMap::new(),
        trigger_languages: vec!["rust".into()],
        root_dir_markers: vec![],
        initialization_options: None,
        extension_to_language: ext,
        ..Default::default()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posix_spawn_and_initialize_round_trip() {
    use lingxi_core::host::LspTransport;
    use platform_posix::lsp::PosixLspTransport;

    // The mock binary must be present. Cargo does not cross-build artifacts
    // for integration tests of another package automatically — the test
    // runner is expected to invoke `cargo build -p lingxi-test-harness
    // --bin mock_lsp_server` first. We skip gracefully if missing.
    let candidate = mock_lsp_server_path();
    if !candidate.exists() {
        eprintln!(
            "skipping spawn_handshake_test: mock_lsp_server not built at {}",
            candidate.display()
        );
        return;
    }

    let transport = PosixLspTransport::new();
    let conn = transport
        .start_server(&mock_config())
        .await
        .expect("spawn succeeded");

    let caps = transport
        .initialize(&conn, "file:///tmp")
        .await
        .expect("initialize ok");
    assert!(caps.hover, "mock advertises hover");
    assert!(caps.definition, "mock advertises definition");

    transport
        .terminate(conn.connection_id)
        .await
        .expect("terminate ok");
}

/// Verifies the ENOENT-guard: spawning a non-existent binary surfaces a
/// clear `LspError::Transport` instead of panicking. This works on every
/// platform where we have `PosixLspTransport`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_spawn_enoent_is_clean_error() {
    use lingxi_core::host::LspTransport;
    use platform_posix::lsp::PosixLspTransport;
    let mut config = mock_config();
    config.command = "/nonexistent/path/to/lsp-server".into();
    let transport = PosixLspTransport::new();
    let err = transport
        .start_server(&config)
        .await
        .expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("spawn")
            || msg.contains("not found")
            || msg.contains("No such file")
            || msg.contains("os error"),
        "ENOENT surfaces as Transport error, got: {msg}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_spawn_uses_absolute_workspace_folder_as_child_cwd() {
    use lingxi_core::host::LspTransport;
    use platform_posix::lsp::PosixLspTransport;
    use tempfile::tempdir;
    use tokio::time::{sleep, timeout, Duration};

    let workspace = tempdir().expect("workspace tempdir");
    let witness = tempdir().expect("witness tempdir");
    let witness_path = witness.path().join("cwd.txt");

    let mut env = HashMap::new();
    env.insert("OUT".into(), witness_path.to_string_lossy().into_owned());

    let workspace_folder = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace path");
    let config = LspServerConfig {
        name: "cwd-witness".into(),
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "pwd -P > \"$OUT\"".into()],
        env,
        workspace_folder: Some(workspace_folder.to_string_lossy().into_owned()),
        ..Default::default()
    };

    let transport = PosixLspTransport::new();
    let conn = transport
        .start_server(&config)
        .await
        .expect("spawn succeeded");

    let observed = timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(&witness_path) {
                break contents;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child wrote cwd witness");

    transport
        .terminate(conn.connection_id)
        .await
        .expect("terminate ok");

    assert_eq!(observed.trim_end(), workspace_folder.to_string_lossy());
}
