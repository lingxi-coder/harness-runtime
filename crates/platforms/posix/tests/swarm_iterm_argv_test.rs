//! Exercise the real process boundary with a deterministic it2 executable.
use lingxi_core::host::{PanePosition, SwarmBackend, SwarmLayout};
use lingxi_core::types::AgentId;
use platform_posix::swarm::ITermSwarmBackend;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn iterm_uses_native_split_run_and_close_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("it2");
    let log = dir.path().join("argv");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$IT2_TEST_LOG"
if [ "$1 $2" = 'session split' ]; then
  printf 'Created new pane: test-pane\n'
fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let previous_path = std::env::var_os("PATH");
    let previous_session = std::env::var_os("ITERM_SESSION_ID");
    std::env::set_var("PATH", dir.path());
    std::env::set_var("IT2_TEST_LOG", &log);
    std::env::set_var("ITERM_SESSION_ID", "w0t0p0:leader-session");
    let backend = ITermSwarmBackend::new();
    let session = backend
        .start_swarm(SwarmLayout::LeaderFollower)
        .await
        .unwrap();
    let pane = backend
        .create_teammate_pane(&AgentId::new(), PanePosition::Right)
        .await
        .unwrap();
    let metadata = backend.pane_metadata(&pane).await.unwrap();
    assert_eq!(metadata.backend_type, "iterm2");
    backend
        .send_command_to_pane(&pane, "lingxi --agent-name tester")
        .await
        .unwrap();
    backend.destroy_swarm(session).await.unwrap();
    if let Some(path) = previous_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    if let Some(session) = previous_session {
        std::env::set_var("ITERM_SESSION_ID", session);
    } else {
        std::env::remove_var("ITERM_SESSION_ID");
    }
    std::env::remove_var("IT2_TEST_LOG");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "session split -v -s leader-session\nsession send -s test-pane \u{15}\nsession run -s test-pane lingxi --agent-name tester\nsession close -f -s test-pane\n");
}
