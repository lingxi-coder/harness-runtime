//! Shared active-work predicates (Claude Code 2.1.263 `MI`/`n3t`, `X_n`/`r3t`).

use crate::task_registry::TaskRecord;

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed")
}

/// Work that can still produce a delegated result. An idle persistent teammate
/// is alive but is not doing work. Parked local agents already carry Completed.
///
/// Remote long-running sessions have no producer in this port (the remote-agent
/// handler is a stub); when that producer is implemented its `isLongRunning`
/// state must be threaded here to exclude those sessions as well.
pub fn is_active_delegated_task(task: &TaskRecord) -> bool {
    matches!(
        task.task_type.as_str(),
        "local_agent" | "remote_agent" | "in_process_teammate" | "local_workflow"
    ) && !is_terminal(&task.status)
        && !(task.task_type == "in_process_teammate" && task.is_idle)
}

/// A live shell includes pending and paused rows, not only Running rows.
pub fn is_live_shell_task(task: &TaskRecord) -> bool {
    task.task_type == "local_bash" && !is_terminal(&task.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_activity_truth_table() {
        for kind in [
            "local_agent",
            "remote_agent",
            "in_process_teammate",
            "local_workflow",
            "local_bash",
            "monitor_ws",
            "mcp_task",
            "dream",
            "local_fusion",
        ] {
            for status in [
                "pending",
                "running",
                "paused",
                "completed",
                "failed",
                "killed",
            ] {
                for idle in [false, true] {
                    let task = TaskRecord {
                        task_type: kind.into(),
                        status: status.into(),
                        is_idle: idle,
                        ..Default::default()
                    };
                    let live = ["pending", "running", "paused"].contains(&status);
                    assert_eq!(
                        is_live_shell_task(&task),
                        kind == "local_bash" && live,
                        "{kind}/{status}/{idle}"
                    );
                    let delegated = [
                        "local_agent",
                        "remote_agent",
                        "in_process_teammate",
                        "local_workflow",
                    ]
                    .contains(&kind);
                    assert_eq!(
                        is_active_delegated_task(&task),
                        delegated && live && (kind != "in_process_teammate" || !idle),
                        "{kind}/{status}/{idle}"
                    );
                }
            }
        }
    }
}
