//! The model-managed working plan (the TodoWrite / Task checklist).
//!
//! Moved here from `tui/src/bottom_pane/input_status.rs` and
//! `tui/src/chat_widget.rs` so the terminal and every client parse the same
//! payload into the same shape. The terminal keeps its ratatui styling; only
//! the data and the pure string logic live here.

use serde_json::Value;

/// Maximum task rows shown before a compact overflow line.
pub const MAX_VISIBLE_TASKS: usize = 5;

/// Claude's terminal-height-dependent task-list cap (`rows <= 10 ? 0 :
/// min(5, max(3, rows - 14))`). Tiny terminals hide the list completely;
/// normal terminals show three to five task rows.
///
/// Clients with a scrolling viewport should use [`MAX_VISIBLE_TASKS`] directly
/// — terminal rows mean nothing to them.
#[must_use]
pub fn max_visible_tasks(terminal_rows: u16) -> usize {
    if terminal_rows <= 10 {
        return 0;
    }
    usize::from(terminal_rows.saturating_sub(14).clamp(3, 5))
}

/// Presentation state for one item in the model-managed working plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTaskState {
    /// Not started yet.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished.
    Completed,
}

impl PlanTaskState {
    /// Parse the V2 Task tool wire status. STRICT: an unknown status is
    /// rejected. TodoWrite uses the forward-compatible
    /// [`from_todowrite_status`] instead.
    ///
    /// [`from_todowrite_status`]: PlanTaskState::from_todowrite_status
    #[must_use]
    pub fn from_wire(status: &str) -> Option<Self> {
        match status {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    /// Parse a TodoWrite item status. Mirrors `Spinner.tsx`'s forward-compatible
    /// `!== pending && !== completed` active predicate: anything else —
    /// INCLUDING a missing status — is [`InProgress`]. This asymmetry with
    /// [`from_wire`] is deliberate; do not "fix" it.
    ///
    /// [`InProgress`]: PlanTaskState::InProgress
    /// [`from_wire`]: PlanTaskState::from_wire
    #[must_use]
    pub fn from_todowrite_status(status: Option<&str>) -> Self {
        match status {
            Some("pending") => Self::Pending,
            Some("completed") => Self::Completed,
            _ => Self::InProgress,
        }
    }

    /// The checklist glyph: `◻` pending, `◼` in progress, `✔` completed.
    /// Data only — color and strikethrough stay with each renderer.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "◻",
            Self::InProgress => "◼",
            Self::Completed => "✔",
        }
    }
}

/// One planned task displayed above the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    /// Stable V2 task id. TodoWrite V1 items have no id.
    pub id: Option<String>,
    /// Imperative task title.
    pub subject: String,
    /// Present-continuous label used while the task is in progress.
    pub active_form: Option<String>,
    /// Current lifecycle state.
    pub state: PlanTaskState,
}

/// Parse TodoWrite's authoritative full-list input into the plan.
///
/// Items without a `content` string are DROPPED. `active_form` reads the
/// camelCase `activeForm`. `id` is always `None` — TodoWrite V1 has no ids.
#[must_use]
pub fn plan_tasks_from_todowrite_input(input: &Value) -> Vec<PlanTask> {
    input
        .get("todos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let subject = item.get("content").and_then(Value::as_str)?.to_string();
            Some(PlanTask {
                id: None,
                subject,
                active_form: item
                    .get("activeForm")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                state: PlanTaskState::from_todowrite_status(
                    item.get("status").and_then(Value::as_str),
                ),
            })
        })
        .collect()
}

/// Parse one V2 Task tool row. Requires `subject` and a known `status`.
#[must_use]
pub fn plan_task_from_v2_value(value: &Value) -> Option<PlanTask> {
    Some(PlanTask {
        id: value.get("id").and_then(Value::as_str).map(str::to_string),
        subject: value.get("subject").and_then(Value::as_str)?.to_string(),
        active_form: value
            .get("activeForm")
            .and_then(Value::as_str)
            .map(str::to_string),
        state: value
            .get("status")
            .and_then(Value::as_str)
            .and_then(PlanTaskState::from_wire)?,
    })
}

/// The first in-progress task — the one the spinner narrates.
#[must_use]
pub fn current_in_progress(tasks: &[PlanTask]) -> Option<&PlanTask> {
    tasks
        .iter()
        .find(|task| task.state == PlanTaskState::InProgress)
}

/// The overflow tail for a truncated plan block: `"+1 in progress, 2 pending"`.
///
/// Counts only the HIDDEN remainder, emits only non-zero clauses, and orders
/// them in progress → pending → completed. Returns `None` when nothing is
/// hidden. Each renderer supplies its own leading `"  … "` and its own
/// localization; this is the English/terminal spelling.
#[must_use]
pub fn overflow_summary(hidden: &[PlanTask]) -> Option<String> {
    if hidden.is_empty() {
        return None;
    }
    let count = |state: PlanTaskState| hidden.iter().filter(|t| t.state == state).count();
    let mut counts = Vec::new();
    let in_progress = count(PlanTaskState::InProgress);
    let pending = count(PlanTaskState::Pending);
    let completed = count(PlanTaskState::Completed);
    if in_progress > 0 {
        counts.push(format!("{in_progress} in progress"));
    }
    if pending > 0 {
        counts.push(format!("{pending} pending"));
    }
    if completed > 0 {
        counts.push(format!("{completed} completed"));
    }
    if counts.is_empty() {
        return None;
    }
    Some(format!("+{}", counts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(subject: &str, state: PlanTaskState) -> PlanTask {
        PlanTask {
            id: None,
            subject: subject.to_string(),
            active_form: None,
            state,
        }
    }

    #[test]
    fn todowrite_parses_content_status_and_active_form() {
        let tasks = plan_tasks_from_todowrite_input(&json!({
            "todos": [
                {"content": "Ship it", "status": "completed", "activeForm": "Shipping it"},
                {"content": "Test it", "status": "pending", "activeForm": "Testing it"},
            ]
        }));
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].subject, "Ship it");
        assert_eq!(tasks[0].state, PlanTaskState::Completed);
        assert_eq!(tasks[0].active_form.as_deref(), Some("Shipping it"));
        assert_eq!(tasks[1].state, PlanTaskState::Pending);
        assert!(tasks[0].id.is_none(), "TodoWrite V1 items carry no id");
    }

    #[test]
    fn todowrite_treats_unknown_and_missing_status_as_in_progress() {
        // Forward compatibility with claude-code's Spinner.tsx predicate.
        let tasks = plan_tasks_from_todowrite_input(&json!({
            "todos": [
                {"content": "a", "status": "in_progress"},
                {"content": "b", "status": "some_future_state"},
                {"content": "c"},
            ]
        }));
        assert_eq!(
            tasks.iter().map(|t| t.state).collect::<Vec<_>>(),
            vec![
                PlanTaskState::InProgress,
                PlanTaskState::InProgress,
                PlanTaskState::InProgress
            ]
        );
    }

    #[test]
    fn todowrite_drops_items_without_content() {
        let tasks = plan_tasks_from_todowrite_input(&json!({
            "todos": [
                {"status": "pending", "activeForm": "no content here"},
                {"content": "kept", "status": "pending"},
            ]
        }));
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].subject, "kept");
    }

    #[test]
    fn todowrite_tolerates_a_missing_or_wrongly_typed_todos_field() {
        assert!(plan_tasks_from_todowrite_input(&json!({})).is_empty());
        assert!(plan_tasks_from_todowrite_input(&json!({"todos": "nope"})).is_empty());
        assert!(plan_tasks_from_todowrite_input(&json!({"todos": []})).is_empty());
    }

    #[test]
    fn v2_rows_require_subject_and_a_known_status() {
        let ok = plan_task_from_v2_value(&json!({
            "id": "t1", "subject": "Do it", "status": "in_progress", "activeForm": "Doing it"
        }))
        .expect("valid row");
        assert_eq!(ok.id.as_deref(), Some("t1"));
        assert_eq!(ok.state, PlanTaskState::InProgress);
        // Strict, unlike TodoWrite.
        assert!(plan_task_from_v2_value(&json!({"subject": "x", "status": "weird"})).is_none());
        assert!(plan_task_from_v2_value(&json!({"status": "pending"})).is_none());
    }

    #[test]
    fn current_in_progress_finds_the_first_active_task() {
        let tasks = vec![
            task("done", PlanTaskState::Completed),
            task("now", PlanTaskState::InProgress),
            task("also now", PlanTaskState::InProgress),
        ];
        assert_eq!(current_in_progress(&tasks).unwrap().subject, "now");
        assert!(current_in_progress(&[task("todo", PlanTaskState::Pending)]).is_none());
    }

    #[test]
    fn overflow_summary_matches_the_claude_copy() {
        let hidden = vec![
            task("a", PlanTaskState::InProgress),
            task("b", PlanTaskState::Pending),
        ];
        assert_eq!(
            overflow_summary(&hidden).as_deref(),
            Some("+1 in progress, 1 pending")
        );
    }

    #[test]
    fn overflow_summary_omits_zero_clauses_and_keeps_the_order() {
        let hidden = vec![
            task("a", PlanTaskState::Completed),
            task("b", PlanTaskState::Completed),
        ];
        assert_eq!(overflow_summary(&hidden).as_deref(), Some("+2 completed"));

        let hidden = vec![
            task("a", PlanTaskState::Completed),
            task("b", PlanTaskState::InProgress),
            task("c", PlanTaskState::Pending),
        ];
        assert_eq!(
            overflow_summary(&hidden).as_deref(),
            Some("+1 in progress, 1 pending, 1 completed")
        );
        assert!(overflow_summary(&[]).is_none());
    }

    #[test]
    fn glyphs_and_terminal_cap_are_unchanged() {
        assert_eq!(PlanTaskState::Pending.glyph(), "◻");
        assert_eq!(PlanTaskState::InProgress.glyph(), "◼");
        assert_eq!(PlanTaskState::Completed.glyph(), "✔");
        assert_eq!(max_visible_tasks(10), 0);
        assert_eq!(max_visible_tasks(11), 3);
        assert_eq!(max_visible_tasks(20), 5);
        assert_eq!(max_visible_tasks(100), 5);
    }
}
