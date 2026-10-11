//! Native 2.1.296 `Qy`/`ld`: a closed SDK input waits for delegated work and
//! its completion notification before publishing the result and suggestion.
use lingxi_core::host::task_registry::TaskRecord;
use std::collections::{HashMap, HashSet};

// Native vGr = 2 * W6t + 60000, W6t = 120000 (SubagentStop hook ceiling).
pub(super) const NOTIFICATION_WAIT_MS: u64 = 300_000;

#[derive(Default)]
pub(super) struct SdkResultHold {
    waits: HashMap<String, (u64, bool)>,
}

impl SdkResultHold {
    pub(super) fn should_hold(
        &mut self,
        input_closed: bool,
        tasks: &[TaskRecord],
        now: u64,
    ) -> bool {
        if !input_closed {
            return false;
        }
        let delegated = |task: &&TaskRecord| {
            (task.task_type == "local_workflow"
                || task.task_type == "local_agent" && !task.is_observer)
                && task.is_backgrounded != Some(false)
        };
        if tasks
            .iter()
            .filter(delegated)
            .any(|task| matches!(task.status.as_str(), "running" | "pending"))
        {
            return true;
        }
        let mut keep = HashSet::new();
        let mut pending = false;
        for task in tasks.iter().filter(delegated).filter(|task| {
            matches!(task.status.as_str(), "completed" | "failed" | "killed") && !task.notified
        }) {
            keep.insert(task.task_id.clone());
            let (first_seen, expired) = self
                .waits
                .entry(task.task_id.clone())
                .or_insert((now, false));
            if *expired {
                continue;
            }
            if now.saturating_sub(*first_seen) >= NOTIFICATION_WAIT_MS {
                *expired = true;
                tracing::warn!(task_id = %task.task_id, "terminal task completion notification did not enqueue within 300000ms");
            } else {
                pending = true;
            }
        }
        self.waits.retain(|id, _| keep.contains(id));
        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_delegated_result_hold_and_notification_expiry() {
        let mut policy = SdkResultHold::default();
        let mut task = TaskRecord {
            task_id: "a1".into(),
            task_type: "local_agent".into(),
            status: "running".into(),
            is_backgrounded: Some(true),
            ..Default::default()
        };
        assert!(!policy.should_hold(false, &[task.clone()], 0));
        assert!(policy.should_hold(true, &[task.clone()], 0));
        task.is_observer = true;
        assert!(!policy.should_hold(true, &[task.clone()], 0));
        task.is_observer = false;
        task.is_backgrounded = Some(false);
        assert!(!policy.should_hold(true, &[task.clone()], 0));
        task.is_backgrounded = Some(true);
        task.status = "completed".into();
        assert!(policy.should_hold(true, &[task.clone()], 10));
        assert!(policy.should_hold(true, &[task.clone()], 300_009));
        assert!(!policy.should_hold(true, &[task.clone()], 300_010));
        assert!(
            !policy.should_hold(true, &[task.clone()], 5),
            "expiry is sticky after clock rollback"
        );
        task.notified = true;
        assert!(!policy.should_hold(true, &[task.clone()], 300_010));
        task.notified = false;
        assert!(
            policy.should_hold(true, &[task], 300_010),
            "new unnotified observation gets its own wait"
        );
    }
}
