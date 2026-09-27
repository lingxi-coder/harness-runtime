//! The terminal command boundary rejects control frames and uses respawn-pane.
use platform_api::{PaneId, SwarmBackend};
use platform_posix::swarm::TmuxBackend;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn tmux_launch_uses_respawn_and_does_not_type_control_characters() {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("tmux");
    let log = dir.path().join("argv");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$TMUX_TEST_LOG"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let previous_path = std::env::var_os("PATH");
    let previous_tmux = std::env::var_os("TMUX");
    std::env::set_var("PATH", dir.path());
    std::env::set_var("TMUX", "test-socket,123,0");
    std::env::set_var("TMUX_TEST_LOG", &log);
    let backend = TmuxBackend::new();
    let pane = PaneId { raw: "%4".into() };
    let refusal = backend
        .send_command_to_pane(&pane, "echo\nnext")
        .await
        .unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "Refusing to send command containing control character U+000A to terminal pane"
    );
    assert!(!log.exists());
    backend
        .send_command_to_pane(&pane, "lingxi --agent-name tester")
        .await
        .unwrap();
    let metadata = backend.pane_metadata(&pane).await.unwrap();
    assert_eq!(metadata.backend_type, "tmux");
    assert_eq!(metadata.session_name, "current");
    assert_eq!(metadata.window_name, "current");
    assert_eq!(metadata.pane_id, "%4");
    backend.kill_pane(&pane).await.unwrap();
    if let Some(path) = previous_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    if let Some(tmux) = previous_tmux {
        std::env::set_var("TMUX", tmux);
    } else {
        std::env::remove_var("TMUX");
    }
    std::env::remove_var("TMUX_TEST_LOG");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "-S test-socket set-option -p -t %4 remain-on-exit failed\n-S test-socket respawn-pane -k -t %4 -- lingxi --agent-name tester\n-S test-socket kill-pane -t %4\n");
}
