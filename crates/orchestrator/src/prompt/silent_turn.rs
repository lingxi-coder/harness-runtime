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
use std::collections::HashMap;

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

/// 2.1.319 `G$n(model)`: an explicit reminder override wins over model traits.
#[must_use]
pub fn is_enabled(model: &str) -> bool {
    let raw = std::env::var("CLAUDE_CODE_SILENT_TURN_REMINDER").ok();
    enabled_with_override(model, raw.as_deref())
}

fn enabled_with_override(model: &str, raw: Option<&str>) -> bool {
    if raw.is_some() {
        return lingxi_core::host::env::is_env_truthy(raw);
    }
    let environment = lingxi_core::host::model_capabilities::prompt_capabilities_environment();
    lingxi_core::host::model_capabilities::prompt_feature_enabled(
        model,
        "silent_turn_reminder",
        environment.as_deref(),
        None,
        None,
    )
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

#[must_use]
pub fn seconds_between_reminders() -> Option<f64> {
    let raw = std::env::var("CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS").ok();
    seconds_between_reminders_from(None, raw.as_deref())
}

#[must_use]
pub fn seconds_between_reminders_from(
    model_seconds: Option<f64>,
    environment: Option<&str>,
) -> Option<f64> {
    if let Some(raw) = environment {
        let raw = lingxi_core::host::effort::trim_js_whitespace(raw);
        return Some(if raw.is_empty() {
            0.0
        } else if let Some(digits) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
            u64::from_str_radix(digits, 16).map_or(f64::NAN, |value| value as f64)
        } else if let Some(digits) = raw.strip_prefix("0b").or_else(|| raw.strip_prefix("0B")) {
            u64::from_str_radix(digits, 2).map_or(f64::NAN, |value| value as f64)
        } else if let Some(digits) = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")) {
            u64::from_str_radix(digits, 8).map_or(f64::NAN, |value| value as f64)
        } else if raw.to_ascii_lowercase().contains("inf")
            && !matches!(raw, "Infinity" | "+Infinity" | "-Infinity")
        {
            f64::NAN
        } else {
            raw.parse::<f64>().unwrap_or(f64::NAN)
        });
    }
    model_seconds
        .filter(|seconds| seconds.is_finite() && *seconds >= 1.0)
        .map(f64::floor)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimedSilentStretch {
    pub stretch: SilentStretch,
    pub silent_since_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoredSilentClocks {
    pub message_timestamps: HashMap<MessageId, i64>,
    pub reminder_marks: Vec<usize>,
    pub reminder_timestamps: HashMap<usize, i64>,
}

/// Recover clocks from the selected transcript chain, aligned with its retained
/// history. Missing timestamps remain absent rather than being replaced by now.
#[must_use]
pub fn restore_silent_clocks(
    rows: &[session::jsonl::JsonlMessage],
    history: &[ConversationMessage],
) -> RestoredSilentClocks {
    let mut positions = HashMap::<MessageId, Vec<usize>>::new();
    for (index, message) in history.iter().enumerate() {
        positions.entry(message.id()).or_default().push(index);
    }
    let mut restored = RestoredSilentClocks::default();
    let mut boundary = 0;
    let mut aligned = false;
    let mut assistant_group: Option<(String, MessageId, Option<usize>)> = None;
    for row in rows {
        let Ok(uuid) = uuid::Uuid::parse_str(&row.uuid) else {
            continue;
        };
        let outer_id = MessageId::from_uuid(uuid);
        let timestamp = chrono::DateTime::parse_from_rfc3339(&row.timestamp)
            .ok()
            .map(|time| time.timestamp_millis());
        let payload = (row.message_type == "attachment")
            .then_some(row.attachment_projection.as_ref())
            .flatten();
        if row.message_type != "assistant" {
            assistant_group = None;
        }
        if payload.is_some_and(|payload| payload.string_at("/type") == Some("silent_turn_reminder"))
        {
            // A reminder before the first retained row belongs to context that
            // was compacted away, so it cannot count in this silent stretch.
            if aligned {
                restored.reminder_marks.push(boundary);
                if let Some(timestamp) = timestamp {
                    restored.reminder_timestamps.insert(boundary, timestamp);
                } else {
                    restored.reminder_timestamps.remove(&boundary);
                }
            }
            continue;
        }
        let (id, index) = if row.message_type == "assistant" {
            let key = row.message.string_at("/id").unwrap_or(&row.uuid);
            if let Some((previous, id, index)) = &assistant_group {
                if previous == key {
                    (*id, *index)
                } else {
                    let id = MessageId::from_uuid(uuid::Uuid::parse_str(key).unwrap_or(uuid));
                    let index = positions.get(&id).and_then(|positions| {
                        positions.iter().copied().find(|index| *index >= boundary)
                    });
                    assistant_group = Some((key.to_owned(), id, index));
                    (id, index)
                }
            } else {
                let id = MessageId::from_uuid(uuid::Uuid::parse_str(key).unwrap_or(uuid));
                let index = positions.get(&id).and_then(|positions| {
                    positions.iter().copied().find(|index| *index >= boundary)
                });
                assistant_group = Some((key.to_owned(), id, index));
                (id, index)
            }
        } else {
            // Queued human prompt attachments may project their source UUID
            // rather than the transcript envelope UUID.
            let source_id = payload
                .and_then(|payload| payload.string_at("/source_uuid"))
                .and_then(MessageId::parse_prefixed)
                .filter(|id| positions.contains_key(id));
            let id = source_id.unwrap_or(outer_id);
            let index = positions
                .get(&id)
                .and_then(|positions| positions.iter().copied().find(|index| *index >= boundary));
            (id, index)
        };
        if let Some(index) = index {
            aligned = true;
            boundary = boundary.max(index + 1);
            // Native reverse scanning observes the last row of an assistant
            // group first. A malformed last timestamp invalidates that clock.
            if let Some(timestamp) = timestamp {
                restored.message_timestamps.insert(id, timestamp);
            } else {
                restored.message_timestamps.remove(&id);
            }
        }
    }
    restored
}

/// One row of the reconstructed oracle message list.
enum Row {
    Assistant { id: MessageId, spoke: bool },
    Reminder,
    RealUser,
    MetaUser,
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
        ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } => {
            text_speaks(text)
        }
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            !thinking.trim().is_empty() && signature.as_deref().is_some_and(is_narration_signature)
        }
        ContentBlock::ToolUse { name, .. } => SPEAKING_TOOL_NAMES.contains(&name.as_str()),
        _ => false,
    })
}

/// Native `Uwn` reads the last length-delimited fields 2 → 1 → 8.
#[must_use]
pub fn is_narration_signature(signature: &str) -> bool {
    use base64::engine::{general_purpose, DecodePaddingMode, GeneralPurposeConfig};
    use base64::Engine;

    let compact: String = signature
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\n' | '\u{000c}' | '\r' | ' '))
        .collect();
    let padding = compact
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    if compact.len() % 4 == 1 || (padding != 0 && (compact.len() % 4 != 0 || padding > 2)) {
        return false;
    }
    let engine = general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let Ok(bytes) = engine.decode(compact) else {
        return false;
    };
    last_signature_bytes(&bytes, 2)
        .and_then(|outer| last_signature_bytes(outer, 1))
        .and_then(|inner| last_signature_bytes(inner, 8))
        == Some(b"narration".as_slice())
}

fn signature_varint(bytes: &[u8], cursor: &mut usize) -> Option<f64> {
    // Native `tst` uses JavaScript numbers, including when skipping an unknown
    // field whose ten-byte varint exceeds u64. Preserve that parser behavior.
    let mut value = 0.0;
    let mut multiplier = 1.0;
    for _ in 0..10 {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value += f64::from(byte & 0x7f) * multiplier;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        multiplier *= 128.0;
    }
    None
}

fn last_signature_bytes(bytes: &[u8], field: u64) -> Option<&[u8]> {
    let mut cursor = 0usize;
    let mut found = None;
    while cursor < bytes.len() {
        let key = signature_varint(bytes, &mut cursor)?;
        match (key % 8.0) as u8 {
            0 => {
                signature_varint(bytes, &mut cursor)?;
            }
            1 => cursor = cursor.checked_add(8)?,
            2 => {
                let length = signature_varint(bytes, &mut cursor)?;
                if length > bytes.len().saturating_sub(cursor) as f64 {
                    return None;
                }
                let length = length as usize;
                let end = cursor.checked_add(length)?;
                let value = bytes.get(cursor..end)?;
                if (key / 8.0).floor() == field as f64 {
                    found = Some(value);
                }
                cursor = end;
            }
            5 => cursor = cursor.checked_add(4)?,
            _ => return None,
        }
        if cursor > bytes.len() {
            return None;
        }
    }
    found
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
            content,
            is_meta,
            turn_companion,
            ..
        } => {
            if !carries_tool_result(content) {
                if *is_meta {
                    if *turn_companion == Some(Some(true)) {
                        Row::Other
                    } else {
                        Row::MetaUser
                    }
                } else {
                    Row::RealUser
                }
            } else {
                Row::Other
            }
        }
        _ => Row::Other,
    }
}

fn flush_group(
    current: &mut Option<MessageId>,
    spoke: &mut bool,
    turns: &mut usize,
    reminders: usize,
    group_time: Option<i64>,
    silent_since: &mut Option<i64>,
) -> bool {
    if current.is_none() {
        return false;
    }
    if *spoke {
        *silent_since = silent_since.or(group_time);
        return true;
    }
    if reminders == 0 {
        *turns += 1;
    }
    *current = None;
    *spoke = false;
    false
}

#[must_use]
pub fn scan_silent_stretch(
    history: &[ConversationMessage],
    reminder_marks: &[usize],
) -> SilentStretch {
    scan_silent_stretch_with_timestamps(history, reminder_marks, &HashMap::new(), &HashMap::new())
        .stretch
}

/// Native `fzr` over host-stamped message and transient attachment clocks.
#[must_use]
pub fn scan_silent_stretch_with_timestamps(
    history: &[ConversationMessage],
    reminder_marks: &[usize],
    message_timestamps: &HashMap<MessageId, i64>,
    reminder_timestamps: &HashMap<usize, i64>,
) -> TimedSilentStretch {
    let mut rows = Vec::with_capacity(history.len() + reminder_marks.len());
    for boundary in 0..=history.len() {
        for mark in reminder_marks {
            if *mark == boundary {
                rows.push((Row::Reminder, reminder_timestamps.get(mark).copied()));
            }
        }
        if let Some(message) = history.get(boundary) {
            rows.push((
                row_for(message),
                message_timestamps.get(&message.id()).copied(),
            ));
        }
    }

    let mut turns = 0usize;
    let mut reminders = 0usize;
    let mut current = None;
    let mut spoke = false;
    let mut silent_since = None;
    let mut group_time = None;
    let mut oldest_time = None;

    for (row, timestamp) in rows.iter().rev() {
        oldest_time = *timestamp;
        if let Row::Assistant {
            id,
            spoke: speaking,
        } = row
        {
            if current != Some(*id) {
                if flush_group(
                    &mut current,
                    &mut spoke,
                    &mut turns,
                    reminders,
                    group_time,
                    &mut silent_since,
                ) {
                    return TimedSilentStretch {
                        stretch: SilentStretch {
                            turns_since_last_reminder: turns,
                            reminders_in_stretch: reminders,
                        },
                        silent_since_ms: silent_since,
                    };
                }
                current = Some(*id);
                group_time = *timestamp;
            }
            spoke |= *speaking;
            continue;
        }
        if flush_group(
            &mut current,
            &mut spoke,
            &mut turns,
            reminders,
            group_time,
            &mut silent_since,
        ) {
            return TimedSilentStretch {
                stretch: SilentStretch {
                    turns_since_last_reminder: turns,
                    reminders_in_stretch: reminders,
                },
                silent_since_ms: silent_since,
            };
        }
        match row {
            Row::Reminder => {
                reminders += 1;
                silent_since = silent_since.or(*timestamp);
            }
            Row::RealUser => {
                silent_since = silent_since.or(*timestamp);
                break;
            }
            Row::MetaUser => silent_since = silent_since.or(*timestamp),
            _ => {}
        }
    }
    flush_group(
        &mut current,
        &mut spoke,
        &mut turns,
        reminders,
        group_time,
        &mut silent_since,
    );
    TimedSilentStretch {
        stretch: SilentStretch {
            turns_since_last_reminder: turns,
            reminders_in_stretch: reminders,
        },
        silent_since_ms: silent_since.or(oldest_time),
    }
}

/// `K4T`'s guard: `if (r >= l3m || t < d3m()) return []`.
#[must_use]
pub fn should_emit(stretch: SilentStretch, turns_required: usize) -> bool {
    stretch.reminders_in_stretch < MAX_REMINDERS_PER_STRETCH
        && stretch.turns_since_last_reminder >= turns_required
}

/// A configured seconds threshold replaces the turn-count cap (`pzr`, 2.1.319).
#[must_use]
pub fn should_emit_with_elapsed(
    stretch: SilentStretch,
    turns_required: usize,
    elapsed_ms: Option<i64>,
    seconds_required: Option<f64>,
) -> bool {
    match seconds_required {
        Some(required) => {
            stretch.turns_since_last_reminder >= 1
                && elapsed_ms.is_some_and(|elapsed| elapsed.div_euclid(1000) as f64 >= required)
        }
        None => should_emit(stretch, turns_required),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::ToolUseId;
    use serde_json::json;

    fn clock_row(
        kind: &str,
        uuid: u128,
        inner: Option<&str>,
        seconds: Option<i64>,
        attachment: Option<serde_json::Value>,
    ) -> session::jsonl::JsonlMessage {
        let mut row = json!({
            "type": kind,
            "uuid": uuid::Uuid::from_u128(uuid).to_string(),
            "parentUuid": null,
            "sessionId": uuid::Uuid::from_u128(999).to_string(),
            "message": {"role":kind,"content":[]},
        });
        if let Some(inner) = inner {
            row["message"]["id"] = inner.into();
        }
        if let Some(seconds) = seconds {
            row["timestamp"] = chrono::DateTime::from_timestamp(seconds, 0)
                .unwrap()
                .to_rfc3339()
                .into();
        }
        if let Some(attachment) = attachment {
            row["attachment"] = attachment;
        }
        serde_json::from_value(row).unwrap()
    }

    fn clock_history(kind: &str, uuid: u128) -> ConversationMessage {
        let id = MessageId::from_uuid(uuid::Uuid::from_u128(uuid));
        if kind == "assistant" {
            let mut row = silent_assistant();
            if let ConversationMessage::Assistant { id: stored, .. } = &mut row {
                *stored = id;
            }
            row
        } else {
            ConversationMessage::user(id, "task".into())
        }
    }

    #[test]
    fn restored_silent_clocks_follow_inner_assistant_groups_and_attachment_boundaries() {
        let inner = uuid::Uuid::from_u128(10).to_string();
        let rows = vec![
            clock_row("user", 1, None, Some(0), None),
            clock_row("assistant", 2, Some(&inner), Some(10), None),
            clock_row("assistant", 3, Some(&inner), Some(20), None),
            clock_row(
                "attachment",
                4,
                None,
                Some(30),
                Some(json!({"type":"silent_turn_reminder"})),
            ),
            clock_row("user", 5, None, Some(40), None),
            clock_row(
                "assistant",
                6,
                Some("msg_provider_non_uuid"),
                Some(45),
                None,
            ),
            clock_row(
                "assistant",
                7,
                Some("msg_provider_non_uuid"),
                Some(50),
                None,
            ),
            clock_row(
                "attachment",
                8,
                None,
                Some(55),
                Some(json!({"type":"silent_turn_reminder"})),
            ),
            clock_row("assistant", 9, None, None, None),
        ];
        let history = vec![
            clock_history("user", 1),
            clock_history("assistant", 10),
            clock_history("user", 5),
            clock_history("assistant", 6),
            clock_history("assistant", 9),
        ];
        let clocks = restore_silent_clocks(&rows, &history);
        assert_eq!(clocks.message_timestamps[&history[1].id()], 20_000);
        assert_eq!(clocks.message_timestamps[&history[3].id()], 50_000);
        assert!(!clocks.message_timestamps.contains_key(&history[4].id()));
        assert_eq!(clocks.reminder_marks, [2, 4]);
        assert_eq!(
            clocks.reminder_timestamps,
            HashMap::from([(2, 30_000), (4, 55_000)])
        );
    }

    #[test]
    fn restored_silent_clocks_ignore_discarded_context_and_missing_timestamps() {
        let retained_inner = uuid::Uuid::from_u128(20).to_string();
        let mut invalid = clock_row("assistant", 13, Some(&retained_inner), Some(30), None);
        invalid.timestamp = "bad timestamp".into();
        let rows = vec![
            clock_row("user", 1, None, Some(0), None),
            clock_row(
                "attachment",
                2,
                None,
                Some(5),
                Some(json!({"type":"silent_turn_reminder"})),
            ),
            clock_row("user", 10, None, Some(10), None),
            clock_row("assistant", 11, Some(&retained_inner), Some(20), None),
            invalid,
            clock_row(
                "attachment",
                14,
                None,
                None,
                Some(json!({"type":"silent_turn_reminder"})),
            ),
        ];
        let history = vec![clock_history("user", 10), clock_history("assistant", 20)];
        let clocks = restore_silent_clocks(&rows, &history);
        assert_eq!(
            clocks.message_timestamps,
            HashMap::from([(history[0].id(), 10_000)])
        );
        assert_eq!(clocks.reminder_marks, [2]);
        assert!(clocks.reminder_timestamps.is_empty());
    }

    #[test]
    fn restored_silent_clocks_use_queued_source_ids_and_latest_boundary_clock() {
        let source = uuid::Uuid::from_u128(30).to_string();
        let rows = vec![
            clock_row(
                "attachment",
                1,
                None,
                Some(10),
                Some(json!({"type":"queued_command","source_uuid":source})),
            ),
            clock_row(
                "attachment",
                2,
                None,
                Some(20),
                Some(json!({"type":"silent_turn_reminder"})),
            ),
            clock_row(
                "attachment",
                3,
                None,
                Some(30),
                Some(json!({"type":"silent_turn_reminder"})),
            ),
        ];
        let history = vec![clock_history("user", 30)];
        let clocks = restore_silent_clocks(&rows, &history);
        assert_eq!(clocks.message_timestamps[&history[0].id()], 10_000);
        assert_eq!(clocks.reminder_marks, [1, 1]);
        assert_eq!(clocks.reminder_timestamps, HashMap::from([(1, 30_000)]));
    }

    #[test]
    fn narration_signatures_match_319_native_parser() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_reminders_319.json"
        ))
        .unwrap();
        for case in fixture["narrationSignatures"].as_array().unwrap() {
            assert_eq!(
                is_narration_signature(case["signature"].as_str().unwrap()),
                case["narration"].as_bool().unwrap(),
                "{case}",
            );
        }
    }

    #[test]
    fn timed_reminder_matches_319_native_producer() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_reminders_319.json"
        ))
        .unwrap();
        for case in fixture["silentCases"].as_array().unwrap() {
            let stretch = SilentStretch {
                turns_since_last_reminder: case["stretch"]["turnsSinceLastReminder"]
                    .as_u64()
                    .unwrap() as usize,
                reminders_in_stretch: case["stretch"]["remindersInStretch"].as_u64().unwrap()
                    as usize,
            };
            let elapsed_ms = case["stretch"]["silentSince"].as_str().map(|since| {
                case["nowMs"].as_i64().unwrap()
                    - chrono::DateTime::parse_from_rfc3339(since)
                        .unwrap()
                        .timestamp_millis()
            });
            assert_eq!(
                should_emit_with_elapsed(
                    stretch,
                    DEFAULT_SILENT_TURN_REMINDER_TURNS,
                    elapsed_ms,
                    case["environment"]["CLAUDE_CODE_SILENT_TURN_REMINDER_SECONDS"].as_f64(),
                ),
                !case["attachments"].as_array().unwrap().is_empty(),
                "{case}",
            );
        }
    }

    #[test]
    fn timed_scan_matches_319_native_history_walk() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_reminders_319.json"
        ))
        .unwrap();
        for case in fixture["silentCases"].as_array().unwrap() {
            let mut history = Vec::new();
            let mut marks = Vec::new();
            let mut timestamps = HashMap::new();
            let mut reminder_timestamps = HashMap::new();
            let mut ids = HashMap::new();
            for row in case["messages"].as_array().unwrap() {
                let timestamp =
                    chrono::DateTime::parse_from_rfc3339(row["timestamp"].as_str().unwrap())
                        .unwrap()
                        .timestamp_millis();
                let message = match row["type"].as_str().unwrap() {
                    "attachment" => {
                        marks.push(history.len());
                        reminder_timestamps.insert(history.len(), timestamp);
                        continue;
                    }
                    "user" if row["message"]["content"].is_array() => tool_result_user(),
                    "user" => {
                        let mut message = real_user();
                        if let ConversationMessage::User {
                            is_meta,
                            turn_companion,
                            ..
                        } = &mut message
                        {
                            *is_meta = row["isMeta"].as_bool().unwrap_or(false);
                            *turn_companion = row["turnCompanion"].as_bool().map(Some);
                        }
                        message
                    }
                    "assistant" => {
                        let id = *ids
                            .entry(row["message"]["id"].as_str().unwrap().to_owned())
                            .or_insert_with(MessageId::new);
                        let content = row["message"]["content"].as_array().unwrap().iter().map(|block| {
                            match block["type"].as_str().unwrap() {
                                "thinking" => ContentBlock::Thinking {
                                    thinking: block["thinking"].as_str().unwrap().to_owned(),
                                    signature: block["signature"].as_str().map(str::to_owned),
                                },
                                "text" => ContentBlock::Text {
                                    text: block["text"].as_str().unwrap().to_owned(),
                                    citations: None,
                                },
                                _ => ContentBlock::ToolUse {
                                    id: ToolUseId::new(),
                                    name: block["name"].as_str().unwrap().to_owned(),
                                    input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
                                    provider_id: None,
                                },
                            }
                        }).collect();
                        ConversationMessage::Assistant {
                            per_turn_effort: None,
                            id,
                            content,
                            stop_reason: None,
                            resumable: None,
                        }
                    }
                    _ => unreachable!(),
                };
                timestamps.insert(message.id(), timestamp);
                history.push(message);
            }
            let actual = scan_silent_stretch_with_timestamps(
                &history,
                &marks,
                &timestamps,
                &reminder_timestamps,
            );
            assert_eq!(
                actual.stretch.turns_since_last_reminder as u64,
                case["stretch"]["turnsSinceLastReminder"].as_u64().unwrap(),
                "{case}"
            );
            assert_eq!(
                actual.stretch.reminders_in_stretch as u64,
                case["stretch"]["remindersInStretch"].as_u64().unwrap(),
                "{case}"
            );
            assert_eq!(
                actual.silent_since_ms,
                case["stretch"]["silentSince"].as_str().map(|value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .unwrap()
                        .timestamp_millis()
                }),
                "{case}"
            );
        }
    }

    fn silent_assistant() -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::new(),
                name: "Bash".into(),
                input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                    &json!({}),
                )
                .expect("generated tool fixture is representable"),
                provider_id: None,
            }],
            stop_reason: None,
            resumable: None,
        }
    }

    fn speaking_assistant() -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "Here is what I found.".into(),
                citations: None,
            }],
            stop_reason: None,
            resumable: None,
        }
    }

    fn tool_result_user() -> ConversationMessage {
        ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "ok".into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
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
    fn silent_reminder_defaults_follow_2_1_319_model_capabilities() {
        for model in [
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "anthropic/claude-fable-5-1",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
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
    fn silent_turn_text_and_limits_match_319_native_source() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_reminders_319.json"
        ))
        .unwrap();
        assert_eq!(
            SILENT_TURN_REMINDER_TEXT,
            fixture["silentTurn"]["body"].as_str().unwrap()
        );
        assert_eq!(
            DEFAULT_SILENT_TURN_REMINDER_TURNS as u64,
            fixture["silentTurn"]["turns"].as_u64().unwrap()
        );
        assert_eq!(
            MAX_REMINDERS_PER_STRETCH as u64,
            fixture["silentTurn"]["maxReminders"].as_u64().unwrap()
        );
    }

    #[test]
    fn a_user_facing_tool_call_counts_as_speaking() {
        for name in ["AskUserQuestion", "Brief", "ExitPlanMode", "SendUserFile"] {
            let asked = ConversationMessage::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: name.into(),
                    input: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                        &json!({}),
                    )
                    .expect("generated tool fixture is representable"),
                    provider_id: None,
                }],
                stop_reason: None,
                resumable: None,
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
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: sentinel.into(),
                    citations: None,
                }],
                stop_reason: None,
                resumable: None,
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
