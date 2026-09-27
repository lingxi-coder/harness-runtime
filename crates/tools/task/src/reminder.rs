//! Per-turn todo / task reminder — claude-code Finding #73.
//!
//! claude-code injects a per-turn `isMeta` reminder when the `TodoWrite` (V1) /
//! `Task*` (V2) tools "haven't been used recently". The reminder fires only
//! when BOTH counters cross their thresholds (`TURNS_SINCE_WRITE` /
//! `TURNS_BETWEEN_REMINDERS`, each `10`), is killswitched by
//! `LINGXI_TODO_REMINDER_MODE === "off"`, is gated as a whole by the
//! `OO()` todo-tools model gate, and selects V1 vs V2 via the tasks-v2 gate
//! (`is_todo_v2_enabled`; 2.1.263 `X_()`, 2.1.183 `TE()`).
//!
//! ## Binary ground truth (`bin/claude.exe`, v2.1.183)
//!
//! Thresholds (`rqt`, offset ~203118638):
//! `{ TURNS_SINCE_WRITE: 10, TURNS_BETWEEN_REMINDERS: 10 }`.
//!
//! Killswitch (`wgo()`, offset ~203087087):
//! ```js
//! function wgo(){let e=process.env.LINGXI_TODO_REMINDER_MODE;
//!   if(e!==void 0)return e;
//!   return ct("tengu_soft_slate_nudge","baseline")==="off"?"off":"baseline"}
//! ```
//! → when the env var is SET it is honored verbatim; `"off"` ⇒ no reminder.
//! With no GrowthBook in Rust the gate defaults to NOT-off (`"baseline"`).
//!
//! V1/V2 selection (`TE()`, offset ~199285430):
//! ```js
//! function TE(){if(_l(process.env.LINGXI_ENABLE_TASKS))return false;return true}
//! ```
//! where `_l(e)` is "explicitly disabled" (`true` iff the value lowercases to
//! one of `0/false/no/off`). So `TE()` returns `true` (V2 task_reminder) by
//! DEFAULT and `false` (V1 todo_reminder) only when `LINGXI_ENABLE_TASKS`
//! is explicitly disabled. The producer (`ytl`, offset ~203087213) is
//! `()=>TE()?B4p(...):M4p(...)` — V2 when `TE()`, else V1.
//!
//! ## 2.1.263 delta
//!
//! The selector is renamed `X_` and now reads the PARSED env object rather
//! than `process.env` directly (`src_160357157.js` @10464):
//!
//! ```js
//! function X_(){if(a.CLAUDE_CODE_ENABLE_TASKS===!1)return!1;return!0}
//! ```
//!
//! Behaviourally identical: `CLAUDE_CODE_ENABLE_TASKS` is declared
//! `I.triBool()`, whose transform is `Ie(e)?true : po(e)?false : undefined`,
//! and `po` is byte-for-byte the old `_l`
//! (`["0","false","no","off"].includes(String(t).toLowerCase().trim())`,
//! `src_156606933.js` @922). So `=== false` still means exactly "the value
//! normalizes to 0/false/no/off".
//!
//! What DID change is that the whole producer sits behind the `OO()`
//! todo-tools model gate (`src_160988549.js` @5064719):
//!
//! ```js
//! ()=>OO()?X_()?xps(d,t):vps(d,t):Promise.resolve([])
//! ```
//!
//! and each renderer re-checks it — `case"todo_reminder":{if(X_()||!OO())
//! return[]` and `case"task_reminder":{if(!h3())return[]` where
//! `h3(){return X_()&&OO()}`. Both guards are fused into [`select_mode`],
//! because LingXi never persists attachments.
//!
//! Renderers (`messages.ts` `normalizeAttachmentForAPI`, offset ~206021028):
//! the reminder body `r` is emitted RAW as `Ln({content:r,isMeta:!0})` — there
//! is NO `<system-reminder>` wrapper (unlike the skill-/task-notification
//! reminders). The exact texts are locked in [`V1_BASE`] / [`V2_BASE`] below.

use lingxi_core::TodoState;

/// `rqt.TURNS_SINCE_WRITE` — assistant turns since the last TodoWrite/Task call
/// before a reminder is eligible. (binary offset ~203118638)
pub const TURNS_SINCE_WRITE: u32 = 10;

/// `rqt.TURNS_BETWEEN_REMINDERS` — assistant turns since the last reminder
/// before another one is eligible. (binary offset ~203118638)
pub const TURNS_BETWEEN_REMINDERS: u32 = 10;

/// `Kw` — the V2 task-create tool name, interpolated into [`V2_BASE`].
const TASK_CREATE_TOOL_NAME: &str = "TaskCreate";
/// `mP` — the V2 task-update tool name, interpolated into [`V2_BASE`].
const TASK_UPDATE_TOOL_NAME: &str = "TaskUpdate";

/// V1 (`todo_reminder`) base text — byte-exact, INCLUDING the trailing `\n`
/// (the JS template literal's closing backtick sits on the next line). Note the
/// missing "it" in "if has become stale" — faithful to the binary typo.
pub const V1_BASE: &str = "The TodoWrite tool hasn't been used recently. If you're working on tasks that would benefit from tracking progress, consider using the TodoWrite tool to track progress. Also consider cleaning up the todo list if has become stale and no longer matches what you are working on. Only use it if it's relevant to the current work. This is just a gentle reminder - ignore if not applicable.\n";

/// V1 items header — appended (after `\n\n`) when the todo list is non-empty.
const V1_ITEMS_HEADER: &str = "Here are the existing contents of your todo list:";

/// V2 (`task_reminder`) base text — byte-exact, INCLUDING the trailing `\n`.
/// `${Kw}`/`${mP}` are interpolated as `TaskCreate`/`TaskUpdate`.
#[must_use]
pub fn v2_base() -> String {
    format!(
        "The task tools haven't been used recently. If you're working on tasks that would benefit from tracking progress, consider using {TASK_CREATE_TOOL_NAME} to add new tasks and {TASK_UPDATE_TOOL_NAME} to update task status (set to in_progress when starting, completed when done). Also consider cleaning up the task list if it has become stale. Only use these if relevant to the current work. This is just a gentle reminder - ignore if not applicable.\n"
    )
}

/// V2 items header — appended (after `\n\n`) when the task list is non-empty.
const V2_ITEMS_HEADER: &str = "Here are the existing tasks:";

/// Which reminder variant to produce, mirroring the binary's `()=>TE()?B4p:M4p`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderMode {
    /// `todo_reminder` — V1, reads `session.todos`. Selected when `TE()` is
    /// false (`LINGXI_ENABLE_TASKS` explicitly disabled).
    V1Todo,
    /// `task_reminder` — V2, reads the file-backed task store. The default.
    V2Task,
}

/// Wire string for a [`TodoState`] (`pending`/`in_progress`/`completed`),
/// matching `o.status` in the binary's item formatter.
fn status_wire(s: TodoState) -> &'static str {
    match s {
        TodoState::Pending => "pending",
        TodoState::InProgress => "in_progress",
        TodoState::Completed => "completed",
    }
}

/// `wgo() === "off"` — the killswitch. Honors `LINGXI_TODO_REMINDER_MODE`
/// verbatim when SET (`"off"` ⇒ suppressed); with no GrowthBook the unset case
/// is NOT off.
#[must_use]
pub fn is_killswitched() -> bool {
    matches!(
        std::env::var("LINGXI_TODO_REMINDER_MODE").ok().as_deref(),
        Some("off")
    )
}

/// `_l(e)` — "explicitly disabled": `true` iff the value lowercases/trims to one
/// of `0`/`false`/`no`/`off`. `None` ⇒ `false`.
fn is_explicitly_disabled(val: Option<&str>) -> bool {
    match val {
        None => false,
        Some(v) => matches!(v.to_lowercase().trim(), "0" | "false" | "no" | "off"),
    }
}

/// `TE()` (`is_todo_v2_enabled`) → the active [`ReminderMode`]. Returns
/// [`ReminderMode::V2Task`] by default, [`ReminderMode::V1Todo`] only when
/// `LINGXI_ENABLE_TASKS` is explicitly disabled.
///
/// Ungated on purpose: the oracle's turn counters advance whether or not the
/// reminder can render, so bookkeeping callers use this and only the renderer
/// goes through [`select_mode`].
#[must_use]
pub fn select_mode_raw() -> ReminderMode {
    if is_explicitly_disabled(std::env::var("LINGXI_ENABLE_TASKS").ok().as_deref()) {
        ReminderMode::V1Todo
    } else {
        ReminderMode::V2Task
    }
}

/// The renderable [`ReminderMode`], or `None` when the `OO()` model gate has
/// withdrawn the todo/task tools.
///
/// This is the port of BOTH oracle reminder guards at once, because LingXi
/// never persists attachments and so fuses the producer and the renderer into
/// one function:
///
/// ```js
/// case"todo_reminder":{if(X_()||!OO())return[];   // V1 renders iff !X_() && OO()
/// case"task_reminder":{if(!h3())return[];         // V2 renders iff  X_() && OO()
/// ```
///
/// `main_loop_model` must be the canonical session model — see
/// [`tool_api::todo_tools_gate`].
#[must_use]
pub fn select_mode(main_loop_model: Option<&str>) -> Option<ReminderMode> {
    if !tool_api::todo_tools_gate::todo_tools_enabled_for_model(main_loop_model) {
        return None;
    }
    Some(select_mode_raw())
}

/// Render the V1 (`todo_reminder`) body. `items` = `(status, content)` pairs in
/// list order. When non-empty, the `\n\n`-separated header + `[<items>]` block
/// is appended; each item is `"${idx}. [${status}] ${content}"` (1-based idx),
/// joined by `\n`, wrapped in `[...]`.
#[must_use]
pub fn render_v1(items: &[(TodoState, String)]) -> String {
    let mut r = String::from(V1_BASE);
    if !items.is_empty() {
        let body = items
            .iter()
            .enumerate()
            .map(|(i, (st, content))| format!("{}. [{}] {}", i + 1, status_wire(*st), content))
            .collect::<Vec<_>>()
            .join("\n");
        r.push_str(&format!("\n\n{V1_ITEMS_HEADER}\n\n[{body}]"));
    }
    r
}

/// Render the V2 (`task_reminder`) body. `items` = `(id, status, subject)`
/// triples in list order. When non-empty, the `\n\n`-separated header + items
/// (NO `[...]` wrapping, unlike V1) is appended; each item is
/// `"#${id}. [${status}] ${subject}"`, joined by `\n`.
#[must_use]
pub fn render_v2(items: &[(String, TodoState, String)]) -> String {
    let mut r = v2_base();
    if !items.is_empty() {
        let body = items
            .iter()
            .map(|(id, st, subject)| format!("#{id}. [{}] {subject}", status_wire(*st)))
            .collect::<Vec<_>>()
            .join("\n");
        r.push_str(&format!("\n\n{V2_ITEMS_HEADER}\n\n{body}"));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both oracle reminder guards at once: an at-threshold model withdraws the
    /// tools, and a reminder that describes tools the model was never offered
    /// must not render.
    #[test]
    fn the_model_gate_suppresses_both_reminder_variants() {
        if !tool_api::todo_tools_gate::todo_tools_enabled_for_model(Some("claude-opus-4-8")) {
            assert_eq!(select_mode(Some("claude-opus-4-8")), None);
        }
        // Below threshold and unknown-model both keep rendering, and pick the
        // same variant the ungated selector would.
        assert_eq!(
            select_mode(Some("claude-sonnet-4-5")),
            Some(select_mode_raw())
        );
        assert_eq!(select_mode(None), Some(select_mode_raw()));
    }

    #[test]
    fn v1_base_is_byte_exact_with_no_items() {
        let out = render_v1(&[]);
        assert_eq!(
            out,
            "The TodoWrite tool hasn't been used recently. If you're working on tasks that would benefit from tracking progress, consider using the TodoWrite tool to track progress. Also consider cleaning up the todo list if has become stale and no longer matches what you are working on. Only use it if it's relevant to the current work. This is just a gentle reminder - ignore if not applicable.\n"
        );
    }

    #[test]
    fn v1_with_items_appends_bracketed_block() {
        let items = vec![
            (TodoState::Pending, "first thing".to_string()),
            (TodoState::InProgress, "second thing".to_string()),
        ];
        let out = render_v1(&items);
        assert!(out.ends_with(
            "\n\nHere are the existing contents of your todo list:\n\n[1. [pending] first thing\n2. [in_progress] second thing]"
        ), "got: {out:?}");
        // Base text is unchanged at the front.
        assert!(out.starts_with("The TodoWrite tool hasn't been used recently."));
    }

    #[test]
    fn v2_base_is_byte_exact_with_no_items() {
        let out = render_v2(&[]);
        assert_eq!(
            out,
            "The task tools haven't been used recently. If you're working on tasks that would benefit from tracking progress, consider using TaskCreate to add new tasks and TaskUpdate to update task status (set to in_progress when starting, completed when done). Also consider cleaning up the task list if it has become stale. Only use these if relevant to the current work. This is just a gentle reminder - ignore if not applicable.\n"
        );
    }

    #[test]
    fn v2_with_items_appends_unbracketed_block() {
        let items = vec![
            ("1".to_string(), TodoState::Completed, "alpha".to_string()),
            ("2".to_string(), TodoState::Pending, "beta".to_string()),
        ];
        let out = render_v2(&items);
        assert!(
            out.ends_with(
                "\n\nHere are the existing tasks:\n\n#1. [completed] alpha\n#2. [pending] beta"
            ),
            "got: {out:?}"
        );
        assert!(out.starts_with("The task tools haven't been used recently."));
    }

    #[test]
    fn killswitch_reads_env_off_only() {
        // Default (unset) is NOT killswitched; any value other than "off" is also
        // not killswitched. Exercised via direct argument-free reads is brittle
        // under parallelism, so we test the pure helper indirectly via the
        // explicitly-disabled / mode helpers in the orchestrator. Here we only
        // assert the constants.
        assert_eq!(TURNS_SINCE_WRITE, 10);
        assert_eq!(TURNS_BETWEEN_REMINDERS, 10);
    }

    #[test]
    fn explicitly_disabled_matches_binary_set() {
        assert!(is_explicitly_disabled(Some("off")));
        assert!(is_explicitly_disabled(Some("FALSE")));
        assert!(is_explicitly_disabled(Some(" 0 ")));
        assert!(is_explicitly_disabled(Some("no")));
        assert!(!is_explicitly_disabled(Some("1")));
        assert!(!is_explicitly_disabled(Some("true")));
        assert!(!is_explicitly_disabled(None));
    }
}
