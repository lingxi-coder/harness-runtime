//! Per-turn todo / task reminder injection — claude-code Finding #73.
//!
//! claude-code injects a per-turn `isMeta` reminder when the `TodoWrite` (V1) /
//! `Task*` (V2) tools "haven't been used recently". This module is the
//! orchestrator-side glue: it owns the V2 task-source provider trait and the
//! gating/selection logic; the byte-locked reminder TEXT + renderers live in
//! [`tool_task::reminder`] (alongside the tools they describe).
//!
//! ## Where the reminder fires (binary `M4p`/`B4p`, `bin/claude.exe`)
//!
//! The producer is `()=>TE()?B4p(...):M4p(...)` ([`tool_task::reminder::select_mode`]).
//! Both branches gate on:
//! 1. the killswitch `wgo()!=="off"`
//!    ([`tool_task::reminder::is_killswitched`]);
//! 2. the relevant tool being present this turn — V1 requires `TodoWrite`
//!    (`gL`), V2 requires `TaskUpdate` (`mP`) (and the V2 case is additionally
//!    guarded by `TE()`); BOTH skip when the `Brief` tool (`rjn`) is present;
//! 3. a non-empty message history (`!e||e.length===0 ⇒ []`);
//! 4. BOTH counters reaching their thresholds:
//!    `turnsSinceLastTodoWrite >= 10 && turnsSinceLastReminder >= 10`.
//!
//! When it fires, the rendered body is wrapped in a `<system-reminder>`
//! envelope — the oracle's `Zy([kn({content:o,isMeta:!0})])` (2.1.238
//! @296690005 / @296690634; `Zy` @296675470 maps `NT` @296673554 =
//! `` `<system-reminder>\n${e}\n</system-reminder>` `` over every message) — as
//! a meta user message appended to
//! the per-turn OUTGOING snapshot only (never `session.history` / JSONL), and
//! `turns_since_last_reminder` is reset to `0`. The counters themselves are
//! tracked as explicit [`lingxi_core::SessionState`] fields
//! (`turns_since_last_todo_write` / `turns_since_last_reminder`), incremented
//! once per assistant turn and reset on the relevant tool call — the binary
//! recomputes them by scanning the message log, but this engine never persists
//! the reminder attachment, so it cannot scan for a prior reminder and tracks
//! the counter directly.

use async_trait::async_trait;
use lingxi_core::TodoState;

/// One V2 task surfaced to the `task_reminder`: `(id, status, subject)`,
/// mirroring the binary's `#${o.id}. [${o.status}] ${o.subject}` formatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReminderItem {
    /// Decimal task id (`"1".."N"`).
    pub id: String,
    /// Lifecycle state.
    pub status: TodoState,
    /// Brief imperative title.
    pub subject: String,
}

/// Supplies the V2 task list for the `task_reminder` body, in list order.
///
/// 1:1 with the binary's `B4p` reading `p9(KF())` (the file-backed task store
/// for the active list). `None` provider ⇒ the V2 reminder renders with no
/// items appended (base text only), matching an empty store. The desktop
/// composition root backs this with the `tool_task::todo_store::TodoStore`.
#[async_trait]
pub trait TodoReminderTaskProvider: Send + Sync {
    /// Snapshot the current V2 task list (sorted by numeric id ascending, as
    /// the store's `list()` already returns). `session_id` is the live
    /// orchestrator session, so providers can mirror the task tools' standalone
    /// session fallback after env/team precedence is exhausted.
    async fn task_items(&self, session_id: protocol::SessionId) -> Vec<TaskReminderItem>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticTasks(Vec<TaskReminderItem>);
    #[async_trait]
    impl TodoReminderTaskProvider for StaticTasks {
        async fn task_items(&self, _session_id: protocol::SessionId) -> Vec<TaskReminderItem> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn provider_returns_items() {
        let p = StaticTasks(vec![TaskReminderItem {
            id: "1".into(),
            status: TodoState::Pending,
            subject: "x".into(),
        }]);
        assert_eq!(p.task_items(protocol::SessionId::new()).await.len(), 1);
    }
}
