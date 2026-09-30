//! `silent_turn_reminder` — the per-turn nudge that fires when the model has
//! worked several turns in a row without saying anything to the user.
//!
//! New in Claude Code 2.1.238. Oracle anatomy (offsets are into
//! `~/.local/share/claude/versions/2.1.238`):
//!
//! * TEXT `a3m` @ **296477528**:
//!   `"The user hasn't heard from you in a while. As you continue, keep them
//!   updated when there's something to tell — a finding, a change of
//!   plan."`
//! * `c3m()` — `CLAUDE_CODE_SILENT_TURN_REMINDER_TEXT` overrides, else the
//!   GrowthBook value `tengu_hushed_lark_text`, else `a3m`.
//! * `d3m()`/`xjT()` — `CLAUDE_CODE_SILENT_TURN_REMINDER_TURNS`, else
//!   GrowthBook `tengu_hushed_lark`, else `s3m = 5`.
//! * `l3m = 3` — at most three reminders per silent stretch.
//! * `u3m(model)` — `CLAUDE_CODE_SILENT_TURN_REMINDER` env, else the model
//!   capability `silent_turn_reminder` (`JJr`, @296558?): **no capability ⇒
//!   OFF**. In 2.1.263 `jfr` also enables the Fable 5.1 prompt bundle; the
//!   port consults the shared model-capability registry for these defaults.
//! * producer `K4T(e)` @ **296525255**:
//!   ```js
//!   let {turnsSinceLastReminder:t, remindersInStretch:r} = Ezm(e);
//!   if (r >= l3m || t < d3m()) return [];
//!   return [{type:"silent_turn_reminder", text:c3m()}]
//!   ```
//! * fan-out gate @ **296520120**: main agent only (`p = !t.agentId`), no new
//!   user prompt this step (`e === null && !s?.isRegularUserPrompt`), not in
//!   focus/brief transcript mode (`!CDt()`), and `u3m(model)`.
//! * renderer @ **296738727**:
//!   `silent_turn_reminder:(e)=>[kn({content:NT(e.text),isMeta:!0})]` — i.e.
//!   the usual `<system-reminder>\n{text}\n</system-reminder>` envelope on a
//!   meta user message. The envelope is applied by the injection site in
//!   `conversation.rs`, exactly like every other per-turn reminder; this module
//!   stays pure so its unit tests can pin the bare oracle bytes.
//!
//! The stretch scan `Ezm` @ **296525002** walks the message list BACKWARDS:
//!
//! ```js
//! function Ezm(e){let t=0,r=0,n,o=!1,i=()=>{
//!   if(n===void 0)return!1; if(o)return!0; if(r===0)t++;
//!   n=void 0,o=!1;return!1};
//!  for(let s=e.length-1;s>=0;s--){let a=e[s];
//!   if(a!==void 0&&WBt(a))continue;                       // isVirtual rows
//!   if(a?.type==="assistant"){let l=a.message.id;
//!     if(l!==n){if(i())return{...};n=l}
//!     …if(spoke) o=!0; continue}
//!   if(i())return{...};
//!   if(a?.type==="attachment"&&a.attachment.type==="silent_turn_reminder"){r++;continue}
//!   if(a?.type==="user"&&!a.isMeta&&!sxl(a.message.content))break}
//!  i();return{turnsSinceLastReminder:t,remindersInStretch:r}}
//! ```
//!
//! An assistant turn "speaks" (`G4T` + `V4T`) when it carries a text block
//! whose trimmed value is neither `""`, `"(no content)"` (`UP` @283631389) nor
//! `"No response requested."` (`TZ` @283631407), OR a `tool_use` in
//! `V4T = new Set([dy, Iio, U4T, UU, Tbe])` — the user-facing tools
//! (AskUserQuestion, Brief, LEGACY_BRIEF, …). See [`SPEAKING_TOOL_NAMES`].
//!
//! `sxl(content)` (@296542062) = "this user message carries a `tool_result`
//! block", so a tool-result user line does NOT end the stretch; only a real
//! (non-meta, non-tool-result) user turn does.
//!
//! PORT SEAM: the oracle keeps its emitted attachments IN the message list, so
//! `remindersInStretch` falls out of the same walk. LingXi's per-turn reminders
//! are TRANSIENT (appended to the outgoing snapshot only, never to
//! `session.history` / JSONL), so the orchestrator records the history LENGTH
//! at each emission instead and [`scan_silent_stretch`] splices those marks
//! back into the walk at the boundary where the oracle's attachment row sat.

use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

/// `a3m` @ 296477528 — the reminder body, byte-exact (U+2014 EM DASH).
pub const SILENT_TURN_REMINDER_TEXT: &str = "The user hasn't heard from you in a while. As you continue, keep them updated when there's something to tell \u{2014} a finding, a change of plan.";

/// `s3m = 5` — silent assistant turns required before the first reminder.
pub const DEFAULT_SILENT_TURN_REMINDER_TURNS: usize = 5;

/// `l3m = 3` — the cap on reminders inside one silent stretch.
pub const MAX_REMINDERS_PER_STRETCH: usize = 3;

/// `UP` @283631389 — a text block equal to this does not count as speaking.
const NO_CONTENT_SENTINEL: &str = "(no content)";

/// `TZ` @283631407 — ditto.
const NO_RESPONSE_REQUESTED_SENTINEL: &str = "No response requested.";

/// 2.1.263 `Ffs = new Set([Es,nge,Cfs,Wh,BT])`: AskUserQuestion,
/// Brief and its legacy alias, ExitPlanMode, and SendUserFile. LingXi does
/// not ship the legacy Brief alias; all four current tool names count.
pub const SPEAKING_TOOL_NAMES: &[&str] =
    &["AskUserQuestion", "Brief", "ExitPlanMode", "SendUserFile"];

/// 2.1.263 `jfr(model)`: an explicit env override wins; otherwise use the
/// Fable 5.1 prompt bundle or the model's silent-turn capability.
#[must_use]
pub fn is_enabled(model: &str) -> bool {
    let raw = std::env::var("CLAUDE_CODE_SILENT_TURN_REMINDER").ok();
    enabled_with_override(model, raw.as_deref())
}

fn enabled_with_override(model: &str, raw: Option<&str>) -> bool {
    if raw.is_some() {
        return lingxi_core::host::env::is_env_truthy(raw);
    }
    let capabilities = lingxi_core::host::model_capabilities::capabilities_for_loose(model);
    capabilities.contains(&"fable_5_1_prompt_bundle")
        || capabilities.contains(&"silent_turn_reminder")
}

/// `c3m()` — the reminder text, with the env override applied.
///
/// The oracle returns `V.CLAUDE_CODE_SILENT_TURN_REMINDER_TEXT` verbatim when
/// it is defined (only the GrowthBook fallback is trim-checked), so an empty
/// override really does produce an empty body.
#[must_use]
pub fn reminder_text() -> String {
    std::env::var("CLAUDE_CODE_SILENT_TURN_REMINDER_TEXT")
        .unwrap_or_else(|_| SILENT_TURN_REMINDER_TEXT.to_string())
}

/// `d3m()`/`xjT()` — silent turns required between reminders.
#[must_use]
pub fn turns_between_reminders() -> usize {
    match std::env::var("CLAUDE_CODE_SILENT_TURN_REMINDER_TURNS") {
        Ok(raw) => raw
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 1.0)
            .map_or(DEFAULT_SILENT_TURN_REMINDER_TURNS, |v| v as usize),
        Err(_) => DEFAULT_SILENT_TURN_REMINDER_TURNS,
    }
}

/// The `Ezm` result pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SilentStretch {
    /// `turnsSinceLastReminder` — silent assistant turns after the most recent
    /// reminder in this stretch (or since the stretch began).
    pub turns_since_last_reminder: usize,
    /// `remindersInStretch` — reminders already emitted in this stretch.
    pub reminders_in_stretch: usize,
}

/// One row of the reconstructed oracle message list.
enum Row {
    Assistant { id: MessageId, spoke: bool },
    Reminder,
    RealUser,
    Other,
}

/// `G4T(text)` — a text block that actually says something.
fn text_speaks(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && trimmed != NO_CONTENT_SENTINEL
        && trimmed != NO_RESPONSE_REQUESTED_SENTINEL
}

/// The `o=!0` predicate inside `Ezm`.
fn assistant_spoke(content: &[ContentBlock]) -> bool {
    content.iter().any(|b| match b {
        ContentBlock::Text { text } => text_speaks(text),
        ContentBlock::ToolUse { name, .. } => SPEAKING_TOOL_NAMES.contains(&name.as_str()),
        _ => false,
    })
}

/// `sxl(content)` @296542062 — does this user line carry a `tool_result`?
fn carries_tool_result(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

fn row_for(msg: &ConversationMessage) -> Row {
    match msg {
        ConversationMessage::Assistant { id, content, .. } => Row::Assistant {
            id: *id,
            spoke: assistant_spoke(content),
        },
        ConversationMessage::User {
            content, is_meta, ..
        } => {
            if !*is_meta && !carries_tool_result(content) {
                Row::RealUser
            } else {
                Row::Other
            }
        }
        _ => Row::Other,
    }
}

/// The `i()` flush closure. Returns `true` when the walk must stop because the
/// group that just closed SPOKE.
fn flush_group(
    current: &mut Option<MessageId>,
    spoke: &mut bool,
    turns: &mut usize,
    reminders: usize,
) -> bool {
    if current.is_none() {
        return false;
    }
    if *spoke {
        return true;
    }
    if reminders == 0 {
        *turns += 1;
    }
    *current = None;
    *spoke = false;
    false
}

/// `Ezm(messages)` — scan the tail of `history` for the current silent stretch.
///
/// `reminder_marks` holds the `history.len()` value captured at each previous
/// emission (see the PORT SEAM note in the module docs); a mark of `n` sits
/// between `history[n - 1]` and `history[n]`, which is exactly where the
/// oracle's `silent_turn_reminder` attachment row lived.
#[must_use]
pub fn scan_silent_stretch(
    history: &[ConversationMessage],
    reminder_marks: &[usize],
) -> SilentStretch {
    let mut rows: Vec<Row> = Vec::with_capacity(history.len() + reminder_marks.len());
    for boundary in 0..=history.len() {
        for mark in reminder_marks {
            if *mark == boundary {
                rows.push(Row::Reminder);
            }
        }
        if let Some(msg) = history.get(boundary) {
            rows.push(row_for(msg));
        }
    }

    let mut turns = 0usize;
    let mut reminders = 0usize;
    let mut current: Option<MessageId> = None;
    let mut spoke = false;

    for row in rows.iter().rev() {
        match row {
            Row::Assistant { id, spoke: s } => {
                if current != Some(*id) {
                    if flush_group(&mut current, &mut spoke, &mut turns, reminders) {
                        return SilentStretch {
                            turns_since_last_reminder: turns,
                            reminders_in_stretch: reminders,
                        };
                    }
                    current = Some(*id);
                }
                if *s {
                    spoke = true;
                }
            }
            other => {
                if flush_group(&mut current, &mut spoke, &mut turns, reminders) {
                    return SilentStretch {
                        turns_since_last_reminder: turns,
                        reminders_in_stretch: reminders,
                    };
                }
                match other {
                    Row::Reminder => reminders += 1,
                    Row::RealUser => break,
                    _ => {}
                }
            }
        }
    }
    flush_group(&mut current, &mut spoke, &mut turns, reminders);
    SilentStretch {
        turns_since_last_reminder: turns,
        reminders_in_stretch: reminders,
    }
}

/// `K4T`'s guard: `if (r >= l3m || t < d3m()) return []`.
#[must_use]
pub fn should_emit(stretch: SilentStretch, turns_required: usize) -> bool {
    stretch.reminders_in_stretch < MAX_REMINDERS_PER_STRETCH
        && stretch.turns_since_last_reminder >= turns_required
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::ToolUseId;
    use serde_json::json;

    fn silent_assistant() -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::new(),
                name: "Bash".into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    fn speaking_assistant() -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "Here is what I found.".into(),
            }],
            stop_reason: None,
        }
    }

    fn tool_result_user() -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "ok".into(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn real_user() -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), "do the thing".into())
    }

    /// The text is the oracle's `a3m` verbatim, em dash included.
    #[test]
    fn reminder_text_is_byte_exact_against_2_1_238() {
        assert_eq!(
            SILENT_TURN_REMINDER_TEXT,
            "The user hasn't heard from you in a while. As you continue, keep them updated when there's something to tell — a finding, a change of plan."
        );
        assert!(SILENT_TURN_REMINDER_TEXT.contains('\u{2014}'));
        assert!(!SILENT_TURN_REMINDER_TEXT.contains(" - "));
    }

    #[test]
    fn thresholds_match_the_oracle_constants() {
        assert_eq!(DEFAULT_SILENT_TURN_REMINDER_TURNS, 5);
        assert_eq!(MAX_REMINDERS_PER_STRETCH, 3);
    }

    #[test]
    fn silent_tool_only_turns_accumulate() {
        let history = vec![
            real_user(),
            silent_assistant(),
            tool_result_user(),
            silent_assistant(),
            tool_result_user(),
        ];
        let s = scan_silent_stretch(&history, &[]);
        assert_eq!(s.turns_since_last_reminder, 2);
        assert_eq!(s.reminders_in_stretch, 0);
    }

    #[test]
    fn a_speaking_assistant_turn_ends_the_stretch() {
        let history = vec![
            real_user(),
            silent_assistant(),
            tool_result_user(),
            speaking_assistant(),
            silent_assistant(),
            tool_result_user(),
        ];
        // Only the trailing silent turn counts; the speaking turn stops the walk.
        assert_eq!(
            scan_silent_stretch(&history, &[]).turns_since_last_reminder,
            1
        );
    }

    #[test]
    fn silent_reminder_defaults_follow_2_1_263_model_capabilities() {
        for model in [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "anthropic/claude-fable-5-1",
        ] {
            assert!(enabled_with_override(model, None), "{model}");
            assert!(!enabled_with_override(model, Some("0")), "{model}");
        }
        for model in ["claude-opus-5", "claude-sonnet-4-6", "other-model"] {
            assert!(!enabled_with_override(model, None), "{model}");
            assert!(enabled_with_override(model, Some("1")), "{model}");
        }
    }

    #[test]
    fn a_user_facing_tool_call_counts_as_speaking() {
        for name in ["AskUserQuestion", "Brief", "ExitPlanMode", "SendUserFile"] {
            let asked = ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: name.into(),
                    input: json!({}),
                    provider_id: None,
                }],
                stop_reason: None,
            };
            let history = vec![real_user(), asked, tool_result_user(), silent_assistant()];
            assert_eq!(
                scan_silent_stretch(&history, &[]).turns_since_last_reminder,
                1
            );
        }
    }

    #[test]
    fn no_content_and_no_response_requested_do_not_count_as_speaking() {
        for sentinel in [NO_CONTENT_SENTINEL, NO_RESPONSE_REQUESTED_SENTINEL, "  "] {
            let quiet = ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: sentinel.into(),
                }],
                stop_reason: None,
            };
            let history = vec![real_user(), quiet, tool_result_user(), silent_assistant()];
            assert_eq!(
                scan_silent_stretch(&history, &[]).turns_since_last_reminder,
                2,
                "{sentinel:?} must not end the stretch"
            );
        }
    }

    #[test]
    fn a_real_user_turn_resets_the_stretch() {
        let history = vec![
            silent_assistant(),
            tool_result_user(),
            real_user(),
            silent_assistant(),
            tool_result_user(),
        ];
        assert_eq!(
            scan_silent_stretch(&history, &[]).turns_since_last_reminder,
            1
        );
    }

    /// `if(r===0)t++` — once the walk passes a reminder mark, later (older)
    /// silent turns stop feeding `turnsSinceLastReminder`.
    #[test]
    fn turns_are_counted_only_after_the_most_recent_reminder() {
        let history = vec![
            real_user(),
            silent_assistant(),
            tool_result_user(),
            silent_assistant(),
            tool_result_user(),
            silent_assistant(),
            tool_result_user(),
        ];
        // A reminder was emitted when history had 5 entries — i.e. after the
        // second tool-result line, so exactly one silent turn follows it.
        let s = scan_silent_stretch(&history, &[5]);
        assert_eq!(s.turns_since_last_reminder, 1);
        assert_eq!(s.reminders_in_stretch, 1);
    }

    #[test]
    fn three_reminders_in_one_stretch_stops_further_emission() {
        let stretch = SilentStretch {
            turns_since_last_reminder: 99,
            reminders_in_stretch: MAX_REMINDERS_PER_STRETCH,
        };
        assert!(!should_emit(stretch, DEFAULT_SILENT_TURN_REMINDER_TURNS));
    }

    #[test]
    fn below_the_turn_threshold_does_not_emit() {
        let stretch = SilentStretch {
            turns_since_last_reminder: 4,
            reminders_in_stretch: 0,
        };
        assert!(!should_emit(stretch, 5));
        let stretch = SilentStretch {
            turns_since_last_reminder: 5,
            reminders_in_stretch: 0,
        };
        assert!(should_emit(stretch, 5));
    }
}
