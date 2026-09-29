//! Real tmux integration test. Run manually with:
//!   `cargo test -p lingxi-platform-posix --test swarm_tmux_integration_test -- --ignored`
//! Requires tmux >= 3.2 on PATH.

use lingxi_core::host::{PanePosition, SwarmBackend, SwarmLayout};
use lingxi_core::types::AgentId;
use platform_posix::swarm::TmuxBackend;

#[tokio::test]
#[ignore = "requires real tmux >= 3.2 on host"]
async fn start_swarm_outside_tmux_creates_external_session() {
    if std::env::var("TMUX").is_ok() {
        eprintln!("skip: this test must run OUTSIDE tmux");
        return;
    }
    if !TmuxBackend::new().is_available() {
        eprintln!("skip: tmux not available or version < 3.2");
        return;
    }

    let backend = TmuxBackend::new();
    let handle = backend
        .start_swarm(SwarmLayout::Tiled)
        .await
        .expect("start_swarm");
    assert_eq!(handle.session_name, "claude-swarm");

    // Create one pane, verify destroy.
    let _pane = backend
        .create_teammate_pane(&AgentId::new(), PanePosition::Right)
        .await
        .expect("create_teammate_pane");

    backend.destroy_swarm(handle).await.expect("destroy_swarm");
}
