//! Tools exposed by implicit session teams.
use crate::{SendMessageTool, TeamRegistry};
use lingxi_core::host::team_spawn::TeamSpawnSeam;
use std::sync::Arc;
use tool_api::Tool;

/// Teammates start through Agent; no explicit team lifecycle tools are exposed.
#[must_use]
pub fn coordinator_internal_tools(
    team: Arc<TeamRegistry>,
    spawn_seam: Arc<dyn TeamSpawnSeam>,
    preview: fn(&str, usize) -> String,
) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(
        SendMessageTool::new(team, preview).with_spawn_seam(spawn_seam),
    )]
}
