//! `tool_search_usage_reminder` — the periodic nudge to search for tool schemas
//! that are not loaded yet.
//!
//! Present unchanged in 2.1.220 and 2.1.238, so this is a long-standing port gap
//! rather than 238 drift.
//!
//! Oracle anatomy (offsets into `~/.local/share/claude/versions/2.1.238`):
//!
//! * renderer @**296691260** (the `xBi` switch, right after `task_reminder`):
//!   ```js
//!   case"tool_search_usage_reminder":{let n=e.undiscoveredToolNames;if(n.length===0)return[];
//!     let o=e.undiscoveredCount-n.length,i=n.join(", ")+(o>0?` (+${o} more)`:"");
//!     return Zy([kn({content:`Some available tools' schemas are not loaded in this conversation yet: ${i}. Before concluding a capability is missing or building a workaround, use ${y0} to find and load relevant tools — keywords to search, or query "select:<name>[,<name>...]" for specific tools. Calling a tool before its schema is loaded will fail. This is just a gentle reminder - ignore if not applicable to the current work.`,isMeta:!0})])}
//!   ```
//! * producer `Uzm(messages, ctx, hadTaskReminder)` @**296553134**:
//!   ```js
//!   function Uzm(e,t,r){let n=Lda();if(n===null)return[];
//!     if(!e||e.length===0)return[];
//!     let{turnsSinceLastToolSearch:o,turnsSinceLastReminder:i}=R3T(e);
//!     if(o<n.everyNTurns||i<n.everyNTurns)return[];
//!     …if(mBr()!=="tst")return s("mode_not_tst");
//!     …if(!bjt(t.options.tools))return s("toolsearch_unavailable");
//!     let a=tFe(e),l=t.options.tools.filter((u)=>ume(u)&&!a.has(u.name)).map((u)=>u.name).sort();
//!     if(l.length===0)return s("no_undiscovered_tools");
//!     …if(c)return s("task_reminder_same_turn");
//!     return [{type:"tool_search_usage_reminder",undiscoveredToolNames:l.slice(0,n.maxNames),undiscoveredCount:l.length}]}
//!   ```
//! * turn counter `R3T` @**296552671** — walks messages backwards, counting
//!   ASSISTANT turns until it meets a `ToolSearch` tool_use
//!   (`turnsSinceLastToolSearch`) and until it meets a prior
//!   `tool_search_usage_reminder` attachment (`turnsSinceLastReminder`); a
//!   counter that never meets its landmark ends up as the total assistant count.
//!
//! # The gate is OFF by default — and this port keeps it that way
//!
//! `Lda()` is `g1n().toolSearchReminder` (@284244436), and `g1n` (@284243742)
//! reads the GrowthBook payload `juniper_shoal`:
//!
//! ```js
//! let n=t.marsh_lantern;
//! if(n===!0)r=Object.freeze({everyNTurns:N4d,maxNames:F4d});
//! else if(typeof n==="object"&&n!==null&&!Array.isArray(n)){…stride…span…}
//! ```
//!
//! With no server payload, `toolSearchReminder` is `null` and `Uzm` returns `[]`
//! on its first line. So this reminder ships **INERT** in a stock 2.1.238
//! install and moves no wire bytes; it is ported so that flipping the gate
//! matches upstream. The port's stand-in for the GrowthBook payload is
//! `LINGXI_TOOL_SEARCH_REMINDER` (truthy ⇒ `marsh_lantern:true`, i.e. the
//! `N4d`/`F4d` defaults) with `LINGXI_TOOL_SEARCH_REMINDER_STRIDE` /
//! `_SPAN` as the `{stride, span}` object form.

/// `N4d = 15` @284245586 — default `everyNTurns` (`stride`).
pub const DEFAULT_EVERY_N_TURNS: u32 = 15;

/// `F4d = 10` @284245586 — default `maxNames` (`span`).
pub const DEFAULT_MAX_NAMES: usize = 10;

/// The `Lda()` result: `{everyNTurns, maxNames}`, or `None` when the gate is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSearchReminderConfig {
    /// `everyNTurns` — the minimum assistant-turn gap on BOTH counters.
    pub every_n_turns: u32,
    /// `maxNames` — how many names the body lists before `(+N more)`.
    pub max_names: usize,
}

/// `Lda()` @284244436 — `None` (the default) when the gate is off.
///
/// `stride`/`span` are honoured only when they parse as integers `>= 1`,
/// matching `typeof o.stride==="number"&&Number.isInteger(o.stride)&&o.stride>=1
/// ?o.stride:N4d`.
#[must_use]
pub fn config() -> Option<ToolSearchReminderConfig> {
    let raw = std::env::var("LINGXI_TOOL_SEARCH_REMINDER").ok()?;
    let raw = raw.trim();
    if raw.is_empty() || matches!(raw, "0" | "false" | "no" | "off") {
        return None;
    }
    let stride = std::env::var("LINGXI_TOOL_SEARCH_REMINDER_STRIDE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_EVERY_N_TURNS);
    let span = std::env::var("LINGXI_TOOL_SEARCH_REMINDER_SPAN")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_MAX_NAMES);
    Some(ToolSearchReminderConfig {
        every_n_turns: stride,
        max_names: span,
    })
}

/// The renderer body (no `<system-reminder>` envelope — the injection site adds
/// it, like every other member of this family).
///
/// `names` is already `slice(0, maxNames)`; `undiscovered_count` is the FULL
/// count, so `count - names.len()` is the `(+N more)` remainder.
/// `tool_search_name` is the oracle's `${y0}`.
#[must_use]
pub fn render_reminder(
    names: &[String],
    undiscovered_count: usize,
    tool_search_name: &str,
) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let remainder = undiscovered_count.saturating_sub(names.len());
    let listed = if remainder > 0 {
        format!("{} (+{remainder} more)", names.join(", "))
    } else {
        names.join(", ")
    };
    Some(format!(
        "Some available tools' schemas are not loaded in this conversation yet: {listed}. \
Before concluding a capability is missing or building a workaround, use {tool_search_name} to \
find and load relevant tools \u{2014} keywords to search, or query \"select:<name>[,<name>...]\" \
for specific tools. Calling a tool before its schema is loaded will fail. This is just a gentle \
reminder - ignore if not applicable to the current work."
    ))
}

/// `R3T(messages)` @296552671 — the two backward turn counters.
///
/// ```js
/// function R3T(e){let t=-1,r=-1,n=0,o=0;
///  for(let i=e.length-1;i>=0;i--){let s=e[i];
///    if(s?.type==="assistant"){ if(vWi(s))continue;
///      if(t===-1&&…content.some((a)=>a.type==="tool_use"&&a.name===y0))t=i;
///      if(t===-1)n++; if(r===-1)o++;}
///    else if(r===-1&&s?.type==="attachment"&&s.attachment.type==="tool_search_usage_reminder")r=i;
///    if(t!==-1&&r!==-1)break}
///  return{turnsSinceLastToolSearch:n,turnsSinceLastReminder:o}}
/// ```
///
/// The oracle finds the previous reminder as an ATTACHMENT row in the message
/// list. LingXi's per-turn reminders are transient and never enter
/// `session.history`, so the emission points are recorded separately as
/// `history.len()` marks — the same technique
/// [`super::silent_turn::scan_silent_stretch`] uses.
#[must_use]
pub fn count_turns(
    history: &[lingxi_core::types::ConversationMessage],
    reminder_marks: &[usize],
    tool_search_name: &str,
) -> (u32, u32) {
    // A mark is `history.len()` captured at emission, so `history[mark]` is the
    // first message AFTER that reminder: an assistant row counts toward
    // `turnsSinceLastReminder` iff its index is `>= mark`.
    let last_mark = reminder_marks.iter().copied().max();
    let after_last_reminder = |idx: usize| match last_mark {
        Some(mark) => idx >= mark,
        None => true,
    };

    let mut since_tool_search = 0u32;
    let mut since_reminder = 0u32;
    let mut found_tool_search = false;
    for (idx, msg) in history.iter().enumerate().rev() {
        if found_tool_search && !after_last_reminder(idx) {
            break;
        }
        let lingxi_core::types::ConversationMessage::Assistant { content, .. } = msg else {
            continue;
        };
        if !found_tool_search
            && content.iter().any(|b| {
                matches!(b, lingxi_core::types::ContentBlock::ToolUse { name, .. } if name == tool_search_name)
            })
        {
            found_tool_search = true;
        }
        if !found_tool_search {
            since_tool_search = since_tool_search.saturating_add(1);
        }
        if after_last_reminder(idx) {
            since_reminder = since_reminder.saturating_add(1);
        }
    }
    (since_tool_search, since_reminder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("Tool{i}")).collect()
    }

    #[test]
    fn the_body_is_byte_exact_against_2_1_238() {
        assert_eq!(
            render_reminder(&["Alpha".into(), "Beta".into()], 2, "ToolSearch").unwrap(),
            "Some available tools' schemas are not loaded in this conversation yet: Alpha, Beta. Before concluding a capability is missing or building a workaround, use ToolSearch to find and load relevant tools \u{2014} keywords to search, or query \"select:<name>[,<name>...]\" for specific tools. Calling a tool before its schema is loaded will fail. This is just a gentle reminder - ignore if not applicable to the current work."
        );
    }

    #[test]
    fn the_remainder_renders_as_plus_n_more() {
        let body = render_reminder(&names(3), 12, "ToolSearch").unwrap();
        assert!(
            body.contains("yet: Tool0, Tool1, Tool2 (+9 more)."),
            "got: {body}"
        );
    }

    fn assistant(
        blocks: Vec<lingxi_core::types::ContentBlock>,
    ) -> lingxi_core::types::ConversationMessage {
        lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: blocks,
            stop_reason: None,
        }
    }

    fn text(s: &str) -> lingxi_core::types::ContentBlock {
        lingxi_core::types::ContentBlock::Text { text: s.into(), citations: None }
    }

    fn tool_search() -> lingxi_core::types::ContentBlock {
        lingxi_core::types::ContentBlock::ToolUse { input_projection: None,
            id: lingxi_core::types::ToolUseId::new(),
            name: "ToolSearch".into(),
            input: serde_json::json!({}),
            provider_id: None,
        }
    }

    #[test]
    fn with_no_landmarks_both_counters_are_the_assistant_count() {
        let history = vec![assistant(vec![text("a")]), assistant(vec![text("b")])];
        assert_eq!(count_turns(&history, &[], "ToolSearch"), (2, 2));
    }

    /// `if(t===-1&&…ToolSearch…)t=i; if(t===-1)n++` — the turn that CONTAINS the
    /// ToolSearch call is not counted, and the walk stops there.
    #[test]
    fn a_tool_search_call_stops_the_first_counter() {
        let history = vec![
            assistant(vec![text("old")]),
            assistant(vec![tool_search()]),
            assistant(vec![text("after")]),
            assistant(vec![text("after2")]),
        ];
        assert_eq!(count_turns(&history, &[], "ToolSearch").0, 2);
    }

    /// A reminder mark is `history.len()` at emission, so only assistant rows at
    /// or after that index count.
    #[test]
    fn a_reminder_mark_bounds_the_second_counter() {
        let history = vec![
            assistant(vec![text("a")]),
            assistant(vec![text("b")]),
            assistant(vec![text("c")]),
        ];
        assert_eq!(count_turns(&history, &[1], "ToolSearch").1, 2);
        // A mark at the very end ⇒ nothing has happened since.
        assert_eq!(count_turns(&history, &[3], "ToolSearch").1, 0);
        // Only the LATEST mark matters.
        assert_eq!(count_turns(&history, &[0, 2], "ToolSearch").1, 1);
    }

    #[test]
    fn an_empty_name_list_renders_nothing() {
        assert!(render_reminder(&[], 0, "ToolSearch").is_none());
        // `undiscoveredCount` alone never produces a body.
        assert!(render_reminder(&[], 7, "ToolSearch").is_none());
    }
}
