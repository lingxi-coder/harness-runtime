//! Session state model. Persistence lives in `lingxi-session` (Plan 10).

use crate::token::Usage;
use crate::types::utf16_json::Utf16JsonProjection;
use crate::types::{ConversationMessage, MessageId, SessionId, ToolUseId};
use serde::Deserializer;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

pub(crate) fn deserialize_todo_source_text<'de, D>(
    deserializer: D,
) -> Result<Utf16JsonProjection, D::Error>
where
    D: Deserializer<'de>,
{
    let value = crate::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if value.string_units("").is_some() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("expected a JSON string"))
    }
}

fn empty_todo_source_text() -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::new(), Vec::new())
        .expect("empty text is a valid source projection")
}

/// Running total of `Usage` across all turns in a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CumulativeUsage(pub Usage);

impl CumulativeUsage {
    /// Accumulate a per-turn `Usage` into the cumulative total.
    pub fn add(&mut self, u: &Usage) {
        self.0.add(u);
    }
}

/// One entry in a session's todo list. Wire-locked by spec §7 line 486.
///
/// `status` serialises to one of three literals: `"pending"`, `"in_progress"`,
/// or `"completed"` — see [`TodoState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    /// Stable identifier. claude-code `TodoItemSchema` is
    /// `{ content, status, activeForm }` — the model-facing `TodoWrite` input
    /// carries no `id`, so `default` lets id-less input deserialize, and
    /// `skip_serializing_if` keeps the empty id out of the tool OUTPUT to
    /// match TS `newTodos` (whose items omit `id`). The field type stays
    /// `String` so existing readers keep compiling.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Human-readable description of the todo, retaining exact source UTF-16.
    #[serde(
        deserialize_with = "deserialize_todo_source_text",
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize"
    )]
    pub content: Utf16JsonProjection,
    /// Lifecycle state of this todo. Wire-locked.
    pub status: TodoState,
    /// Present-tense form shown while the todo is in progress (claude-code
    /// `TodoItemSchema.activeForm`). Wire key is camelCase `activeForm`.
    #[serde(
        rename = "activeForm",
        default = "empty_todo_source_text",
        deserialize_with = "deserialize_todo_source_text",
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize"
    )]
    pub active_form: Utf16JsonProjection,
}

/// Wire-locked todo lifecycle states.
///
/// Serialises to `"pending"` / `"in_progress"` / `"completed"` per
/// spec §7 line 486. The aliases `"done"` and `"todo"` are explicitly
/// rejected by `TodoWriteTool`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    /// Not yet started.
    Pending,
    /// Actively being worked on (at most one per todo list).
    InProgress,
    /// Finished.
    Completed,
}

/// How a goal came to be active — the `origin` field of every `tengu_goal_*`
/// analytics event.
///
/// Upstream 2.1.267 resolves it in `y(e,t)` (`src_161508826.js`), which reads a
/// `queuedGoalOrigin` staged by `ProposeGoal` and otherwise returns the literal
/// `"user"`, so the value is never absent. `ProposeGoal`'s two spellings
/// (`proposal_direct` / `proposal_approved`) are unreachable here — LingXi does
/// not ship that tool (see the accepted-divergence register) — which leaves the
/// two values below.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalOrigin {
    /// `y()`'s fallback: the user set this goal in this session.
    #[default]
    User,
    /// `mon` (`src_182607998.js`) — the goal came back with a resumed session.
    Restored,
}

/// Origin of the raw model-setting value that drives Native main-loop model
/// selection. A user model pick is session-authoritative and must survive
/// settings refreshes; configured values may be refreshed from the current
/// CLI/settings precedence.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionModelSettingSource {
    /// Current configured CLI/environment/settings value.
    #[default]
    Configured,
    /// A user selected a model in this live session.
    UserSelection,
}

impl GoalOrigin {
    /// The analytics spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Restored => "restored",
        }
    }
}

/// Active session-scoped `/goal` state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveGoalState {
    /// User-supplied goal condition.
    #[serde(with = "crate::types::utf16_json::raw_json_serde")]
    pub condition: Utf16JsonProjection,
    /// When the goal became active.
    pub set_at: SystemTime,
    /// Most recent stop-time evaluation reason, when available.
    #[serde(
        default,
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub last_reason: Option<Utf16JsonProjection>,
    /// Number of completed Stop evaluations for this goal.
    #[serde(default)]
    pub iterations: u64,
    /// Cumulative session tokens when the goal was set.
    #[serde(default)]
    pub tokens_at_start: u64,
    /// How the goal became active. Upstream keeps `origin` on the in-memory
    /// app-state object only — the persisted `goal_status` schema is
    /// `{condition, iterations, set_at, tokens_at_start, last_reason}` — so this
    /// is `skip`ped rather than added to a pinned wire shape. It survives a
    /// compact boundary through
    /// [`CompactActiveGoalState`](../../protocol/messages/struct.CompactActiveGoalState.html),
    /// which mirrors upstream's raw dump of the same object, and the resume fold
    /// re-derives it as [`GoalOrigin::Restored`] regardless of what was stored.
    #[serde(skip)]
    pub origin: GoalOrigin,
}

/// Timestamp sidecar for timing-sensitive history policies.
///
/// Kept outside [`ConversationMessage`] so its frozen provider/wire shape is
/// unchanged. Legacy persisted state defaults to no timestamp, which makes
/// time-based microcompaction fail safe (no cleanup).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageTimingState {
    /// Commit time of the newest assistant message known to this session.
    #[serde(default)]
    pub last_assistant_at: Option<SystemTime>,
}

/// In-memory model of a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// Unique identifier for this session.
    pub session_id: SessionId,
    /// Ordered conversation history.
    pub history: Vec<ConversationMessage>,
    /// Genuine user prompts removed from the active history by compaction.
    /// `$.session.turns()` adds this to the prompts still present in `history`.
    /// A full JSONL resume rebuilds the original rows and starts at zero.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub compacted_user_turns: u64,
    /// User rows whose JSONL envelope carried `isVirtual: true`. This flag is
    /// independent of `isMeta`, but the model-facing message type omits it.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub virtual_user_messages: HashSet<MessageId>,
    /// Original assistant grouping flags `(isVirtual, resumedFromIncompleteThinking)`
    /// retained when JSONL rows are projected into protocol messages.
    #[serde(default)]
    pub hook_message_grouping: HashMap<MessageId, (bool, bool)>,
    /// Cumulative token usage across all turns.
    pub usage: CumulativeUsage,
    /// Model identifier (e.g. `"claude-opus-4-7"`).
    pub model: String,
    /// Provider profile pinned alongside `model` (disambiguates shared ids
    /// across providers). `None` = resolve unscoped. Set by `switch_model`.
    #[serde(default)]
    pub model_profile: Option<String>,
    /// Exact user-selected model source plus its known routing anchor and
    /// display identity. Plan-mode aliases and main-request source use inspect
    /// this binding without ever replacing the routed model above.
    #[serde(default)]
    pub user_model_setting: Option<crate::host::ModelSourceBinding>,
    /// Provenance for the configured raw model setting. A live session pick
    /// uses `UserOrEnv` provenance and is distinguished by `user_model_setting_source`.
    #[serde(default)]
    pub user_model_setting_provenance: crate::host::ModelProvenance,
    /// Whether the raw model setting is still configured state or a session
    /// user selection that settings refresh must preserve.
    #[serde(default)]
    pub user_model_setting_source: SessionModelSettingSource,
    /// Active todo list (M4-04). Mutated by `TodoWriteTool`. Defaults to empty
    /// on deserialize so pre-M4-04 persisted sessions still load.
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    /// Active session-scoped `/goal`. Defaults to `None` on deserialize.
    #[serde(default)]
    pub active_goal: Option<ActiveGoalState>,
    /// Timing state adjacent to history; never inferred from message count.
    #[serde(default)]
    pub message_timing: MessageTimingState,
    /// Source-typed row identity used to reconstruct native request projection
    /// details that do not fit the frozen provider message shape.
    #[serde(
        default,
        skip_serializing_if = "crate::host::NativeRequestHistoryMetadata::is_empty"
    )]
    pub native_request_history: crate::host::NativeRequestHistoryMetadata,
    /// Persisted Ultracode reminder state. The transcript replay path
    /// reconstructs these fields from the meta reminder messages, so cold
    /// resume and in-process resume continue the same sparse cadence.
    #[serde(default)]
    pub ultracode_active: bool,
    /// Number of non-meta user turns since the last Ultracode enter/sparse
    /// reminder.
    #[serde(default)]
    pub ultracode_non_meta_turns_since_reminder: u32,
    /// Plan-mode flag (M4-04). Flipped by `EnterPlanModeTool` /
    /// `ExitPlanModeTool`. Defaults to `false` on deserialize.
    #[serde(default)]
    pub plan_mode: bool,
    /// Full-vs-sparse plan-mode reminder tracking — the per-turn plan-mode
    /// reminder is `full` (206 `LU_`) on the FIRST plan-mode turn and `sparse`
    /// (206 `MU_`) after (206 `reminderType`). Reset to `false` on plan-mode
    /// ENTRY (`EnterPlanModeTool` / `set_plan_mode(true)`) so a re-entered plan
    /// mode replays the full reminder. The orchestrator sets it `true` after the
    /// first injection. Never persisted meaningfully (transient like the todo
    /// counters); defaults to `false` on deserialize.
    #[serde(default)]
    pub plan_reminder_shown: bool,
    /// 2.1.266 `NM` — "this session has exited plan mode at least once".
    /// `ExitPlanMode` sets it; the plan-mode reminder provider consumes it to
    /// emit ONE `plan_mode_reentry` attachment on the next entry that finds an
    /// existing plan file (`if(nPt()&&y!==null){…;NM(!1)}`), then clears it.
    #[serde(default)]
    pub plan_mode_exited: bool,
    /// 2.1.266 `Vz` — "a `plan_mode_exit` reminder is owed". `ExitPlanMode` sets
    /// it (`NM(!0),Vz(!0)`); the provider clears it whether or not it emits
    /// (`Z_s` calls `Vz(!1)` on both the still-in-plan-mode early return and the
    /// emit path).
    #[serde(default)]
    pub plan_mode_exit_pending: bool,
    /// Finding #73 — assistant turns since the last `TodoWrite` (V1) /
    /// `TaskCreate`|`TaskUpdate` (V2) tool call. The per-turn todo-reminder
    /// (`L4p`/`N4p` `turnsSinceLastTodoWrite`/`turnsSinceLastTaskManagement`)
    /// fires only once this reaches `reminder::TURNS_SINCE_WRITE` (`10`). Reset
    /// to `0` whenever a qualifying tool call is observed in a turn's assistant
    /// response; incremented once per assistant turn. The binary recomputes
    /// this by scanning the message log; this engine never persists the
    /// reminder attachment to history, so it is tracked here as explicit
    /// session state. Defaults to `0` on deserialize (pre-#73 sessions).
    #[serde(default)]
    pub turns_since_last_todo_write: u32,
    /// Finding #73 — assistant turns since the last todo/task reminder fired
    /// (`turnsSinceLastReminder`). The reminder fires only once this reaches
    /// `reminder::TURNS_BETWEEN_REMINDERS` (`10`), then resets to `0`.
    /// Incremented once per assistant turn. Defaults to `0` on deserialize.
    #[serde(default)]
    pub turns_since_last_reminder: u32,
    /// In-memory association of each tool-injected conversation message
    /// (`MessageId`) to the `tool_use_id` of the tool that injected it
    /// (the Skill tool's OWN `tool_use` block id). Faithful port of TS
    /// `tagMessagesWithToolUseID` (`tools/utils.ts:12-25`), which stamps each
    /// injected `UserMessage` with `sourceToolUseID`. TS treats this as an
    /// UNTYPED extra prop that is NEVER serialized to the JSONL transcript —
    /// the sole reader is `getToolUseID` (`utils/messages.ts:2765-2793`) for
    /// TUI grouping. Mirroring that, this is a side-table marked
    /// `#[serde(skip)]` so it stays purely in-memory and never reaches the
    /// wire (JSONL bytes stay byte-identical). No Rust consumer reads it yet
    /// (the TUI grouping is not ported).
    #[serde(skip)]
    pub injected_message_sources: HashMap<MessageId, ToolUseId>,
    /// Message ids whose JSONL user envelope carried
    /// `isVisibleInTranscriptOnly: true`.
    ///
    /// These messages must remain in the model history (compact summaries are
    /// load-bearing context), but renderers must not present them as ordinary
    /// user input. The protocol's frozen `ConversationMessage::User` shape has
    /// no slot for this independent Claude envelope flag, so resume preserves
    /// it in a session side table instead of collapsing it into `is_meta`
    /// (which has different last-real-user semantics).
    #[serde(default)]
    pub transcript_only_messages: HashSet<MessageId>,
    /// Message ids whose JSONL user envelope carried
    /// `isCompactSummary: true`. Kept separately from
    /// [`Self::transcript_only_messages`] because the two flags are independent
    /// on the wire even though Claude normally writes both for compact
    /// summaries.
    #[serde(default)]
    pub compact_summary_messages: HashSet<MessageId>,
    /// UI/transcript messages intentionally excluded from every model-facing
    /// history snapshot. Unlike `isMeta`, this is a hard context boundary.
    #[serde(default)]
    pub model_context_excluded_messages: HashSet<MessageId>,
    /// Whether a thinking-signature recovery marker has been observed.
    /// The actual outbound scope lives in `thinking_stripped_messages`; this
    /// compatibility flag must never disable freshly generated thinking.
    #[serde(default)]
    pub thinking_signature_stripped: bool,
    /// First rejected thinking block per historical assistant message. Fresh
    /// assistant IDs remain eligible to round-trip thinking after recovery.
    #[serde(default)]
    pub thinking_stripped_messages: HashMap<MessageId, usize>,
}

impl SessionState {
    /// Construct a fresh session with no history.
    #[must_use]
    pub fn empty(session_id: SessionId, model: String) -> Self {
        Self {
            session_id,
            history: Vec::new(),
            compacted_user_turns: 0,
            virtual_user_messages: HashSet::new(),
            hook_message_grouping: HashMap::new(),
            usage: CumulativeUsage::default(),
            model,
            model_profile: None,
            user_model_setting: None,
            user_model_setting_provenance: crate::host::ModelProvenance::default(),
            user_model_setting_source: SessionModelSettingSource::Configured,
            todos: Vec::new(),
            active_goal: None,
            message_timing: MessageTimingState::default(),
            native_request_history: crate::host::NativeRequestHistoryMetadata::default(),
            ultracode_active: false,
            ultracode_non_meta_turns_since_reminder: 0,
            plan_mode: false,
            plan_reminder_shown: false,
            plan_mode_exited: false,
            plan_mode_exit_pending: false,
            turns_since_last_todo_write: 0,
            turns_since_last_reminder: 0,
            injected_message_sources: HashMap::new(),
            transcript_only_messages: HashSet::new(),
            compact_summary_messages: HashSet::new(),
            model_context_excluded_messages: HashSet::new(),
            thinking_signature_stripped: false,
            thinking_stripped_messages: HashMap::new(),
        }
    }

    /// Clone the history that may be sent to a model or model-backed
    /// compactor, omitting transcript-only completion envelopes.
    #[must_use]
    pub fn model_context_history(&self) -> Vec<ConversationMessage> {
        self.history
            .iter()
            .filter(|message| !self.model_context_excluded_messages.contains(&message.id()))
            .cloned()
            .collect()
    }

    /// Replace only the model-visible portion of history while retaining
    /// excluded transcript messages. Recovery/compaction paths use this when
    /// they rewrite the request history in place.
    pub fn replace_model_context_history(&mut self, history: Vec<ConversationMessage>) {
        let before = self.real_user_turns_in_history();
        let excluded = self
            .history
            .iter()
            .filter(|message| self.model_context_excluded_messages.contains(&message.id()))
            .cloned()
            .collect::<Vec<_>>();
        self.history = history;
        self.history.extend(excluded);
        let after = self.real_user_turns_in_history();
        self.compacted_user_turns = self
            .compacted_user_turns
            .saturating_add(before.saturating_sub(after));
    }

    /// Number of actual user prompts in this session, including those removed
    /// from model context by earlier compactions.
    #[must_use]
    pub fn real_user_turns(&self) -> u64 {
        self.compacted_user_turns
            .saturating_add(self.real_user_turns_in_history())
    }

    /// Whether the input row with `message_id` is still eligible for Native's
    /// cold redelivery path. Claude Code's `isTurnUnanswered` accepts a
    /// persisted row with no later user/assistant messages, or a tail ending
    /// in an `interruptedByShutdown: true` user row whose intervening rows are
    /// only assistant output and the recognized transcript furniture.
    #[must_use]
    pub fn message_turn_is_unanswered(&self, message_id: MessageId) -> bool {
        let Some(anchor) = self
            .history
            .iter()
            .position(|message| message.id() == message_id)
        else {
            return false;
        };

        let following = self.history[anchor + 1..]
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    ConversationMessage::User { .. } | ConversationMessage::Assistant { .. }
                )
            })
            .collect::<Vec<_>>();
        let Some(last) = following.last() else {
            return true;
        };
        if !matches!(
            last,
            ConversationMessage::User {
                interrupted_by_shutdown: Some(true),
                ..
            }
        ) {
            return false;
        }

        following[..following.len() - 1]
            .iter()
            .rev()
            .all(|message| match message {
                ConversationMessage::Assistant { .. } => true,
                ConversationMessage::User {
                    is_compact_summary: true,
                    ..
                } => true,
                ConversationMessage::User {
                    content, is_meta, ..
                } => {
                    native_interruption_notice(content)
                        || content.iter().any(|block| {
                            matches!(block, crate::types::ContentBlock::ToolResult { .. })
                        })
                        || (*is_meta && native_engine_meta_notice(content))
                }
                ConversationMessage::System { .. } => {
                    unreachable!("system rows are excluded from Native's unanswered-turn scan")
                }
            })
    }

    fn real_user_turns_in_history(&self) -> u64 {
        self.history
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    ConversationMessage::User {
                        id,
                        content,
                        is_meta: false,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
                        ..
                    } if !self.virtual_user_messages.contains(id)
                        && !self.compact_summary_messages.contains(id)
                        && !self.transcript_only_messages.contains(id)
                        && !content.iter().any(|block| matches!(block, crate::types::ContentBlock::ToolResult { .. }))
                )
            })
            .count() as u64
    }
}

const NATIVE_INTERRUPTION_PREFIXES: &[&str] = &[
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
    "[Tool call did not complete: the turn was ended to deliver the message that follows. Nothing refused it; re-run it if still needed.]",
    "[Tool call interrupted: the session ended before this call's result was recorded, so its outcome is unknown. Check whether it took effect before relying on it or running it again.]",
    "[Tool call result not in this copy: this session was copied from another session before that session recorded this call's result. The call may have finished there, may still be running there, or may never have run. Check whether it took effect before relying on it or running it again.]",
    "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed.",
    "[Tool call skipped: the turn was stopped before this call ran, by the check whose denial is on another call in this batch. Nothing refused this call and it had no effects; re-run it if still needed.]",
    "[Tool call skipped: the turn ended to deliver the message that follows before this call ran. Nothing refused it; re-run it if still needed.]",
];

const NATIVE_ENGINE_META_PREFIXES: &[&str] = &[
    "[structured-output-enforce]",
    "[projects-reply-gate]",
    "You ended the turn without calling SendUserMessage.",
    "Goal check-in: «",
];

const NATIVE_ENGINE_META_EXACT: &[&str] = &[
    "The previous response failed to produce a valid tool call. Please retry the tool call now.",
    "Your tool call was malformed and could not be parsed. Please retry.",
    "[Your previous response had no visible output. Please continue and produce a user-visible response.]",
    "The PermissionDenied hook indicated you may retry this tool call.",
    "Your response above was stopped by a safety classifier — this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded.",
    "Your response above was stopped by a safety classifier — this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded. Exception: a tool call whose result reads \"Interrupted\" was already running when the response was stopped; it may have partially or fully completed.",
    "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.",
    "Output token limit hit while you were still reasoning, before this response produced any text or tool call. That reasoning was not kept and is not part of this conversation, so there is nothing to resume. Continue from the conversation above: reason much more briefly and produce your answer or next tool call well within the output limit.",
    "Your response was cut off because it exceeded the output token limit. Continue from where you left off. What you write next is shown as a separate block below the text so far, so begin at the start of the line, table row, list item or sentence that was cut, writing it again in full, and if you were inside a code block or a table, open it again first (the same code fence and language, the table's header row). Repeat nothing before that point and do not comment on the interruption. Keep going until the response is complete.",
    "Your response above was cut off mid-stream. Resume directly from where it stops — no apology, no recap. If none of it survived, answer the request from the start.",
    "Your response above was cut off mid-stream and only your next message is delivered. Write the complete response again from the start — no apology, no mention of the cut-off.",
];

const NATIVE_ENGINE_META_HOOK_EVENTS: &[&str] =
    &["Stop", "TeammateIdle", "TaskCreated", "TaskCompleted"];
const NATIVE_ENGINE_META_HOOK_FEEDBACK_SUFFIX: &str = " hook feedback:\n";

fn units_start_with(units: &[u16], prefix: &str) -> bool {
    let prefix = prefix.encode_utf16().collect::<Vec<_>>();
    units.starts_with(&prefix)
}

fn user_block_text_units(block: &crate::types::ContentBlock) -> Option<Vec<u16>> {
    use crate::types::ContentBlock;

    match block {
        ContentBlock::Text { text, .. } => Some(text.encode_utf16().collect()),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        _ => None,
    }
}

/// Native `HS`: a user row whose text blocks all begin with one of the
/// transcript's recognized interruption/tool-delivery prefixes.
fn native_interruption_notice(content: &[crate::types::ContentBlock]) -> bool {
    use crate::types::ContentBlock;

    !content.is_empty()
        && content.iter().all(|block| {
            let units = match block {
                ContentBlock::Text { text, .. } => text.encode_utf16().collect::<Vec<_>>(),
                ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => utf16_code_units.clone(),
                ContentBlock::ToolResult {
                    content,
                    is_error: Some(true),
                    ..
                } => content.string_units("").unwrap_or_default(),
                _ => return false,
            };
            NATIVE_INTERRUPTION_PREFIXES
                .iter()
                .any(|prefix| units_start_with(&units, prefix))
        })
}

/// Native `I4e`: only recognized internal meta rows are ignored while it
/// scans backward from a shutdown interruption. Arbitrary `isMeta` messages
/// remain substantive user rows and close the unanswered window.
fn native_engine_meta_notice(content: &[crate::types::ContentBlock]) -> bool {
    let Some(first) = content.first().and_then(user_block_text_units) else {
        return false;
    };

    if NATIVE_ENGINE_META_PREFIXES
        .iter()
        .any(|prefix| units_start_with(&first, prefix))
    {
        return true;
    }

    if NATIVE_ENGINE_META_EXACT
        .iter()
        .any(|text| first.as_slice() == text.encode_utf16().collect::<Vec<_>>())
    {
        return true;
    }

    NATIVE_ENGINE_META_HOOK_EVENTS.iter().any(|event| {
        units_start_with(
            &first,
            &format!("{event}{NATIVE_ENGINE_META_HOOK_FEEDBACK_SUFFIX}"),
        )
    })
}

#[cfg(test)]
mod user_model_setting_source_tests {
    use super::{SessionModelSettingSource, SessionState};
    use crate::types::utf16_json::Utf16JsonProjection;

    #[test]
    fn session_keeps_the_exact_selection_units_with_its_original_route_anchor() {
        let units = "opusplan"
            .encode_utf16()
            .chain([0xD800])
            .collect::<Vec<_>>();
        let source =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units.clone())
                .expect("exact selection source");
        let mut session = SessionState::empty(crate::types::SessionId::new(), "claude-opus".into());
        session.user_model_setting = Some(crate::host::ModelSourceBinding {
            source: source.clone(),
            effective_source: source,
            route_anchor: crate::host::ModelRouteAnchor {
                model: "claude-opus".into(),
                profile: Some("anthropic".into()),
            },
        });
        session.user_model_setting_source = SessionModelSettingSource::UserSelection;

        let json = serde_json::to_string(&session).expect("serialize current session state");
        assert!(json.contains("\"source\":\"opusplan\\ud800\""));
        assert!(json.contains("\"route_anchor\""));

        let restored: SessionState =
            serde_json::from_str(&json).expect("restore current session state");
        assert_eq!(
            restored
                .user_model_setting
                .as_ref()
                .and_then(|selection| selection.source.string_units("")),
            Some(units)
        );
        assert_eq!(
            restored
                .user_model_setting
                .as_ref()
                .map(|selection| selection.route_anchor.profile.as_deref()),
            Some(Some("anthropic"))
        );
    }
}

#[cfg(test)]
mod mod_session_turn_tests {
    use super::*;
    use crate::types::{ContentBlock, ToolUseId};

    #[test]
    fn real_user_turns_survive_repeated_compaction_and_roundtrip() {
        let mut session = SessionState::empty(SessionId::nil(), "model".into());
        let first = ConversationMessage::user(MessageId::new(), "first".into());
        let second = ConversationMessage::user(MessageId::new(), "second".into());
        let virtual_user = ConversationMessage::user(MessageId::new(), "virtual".into());
        session.virtual_user_messages.insert(virtual_user.id());
        session.history.extend([
            first,
            ConversationMessage::user_meta(MessageId::new(), "meta".into()),
            virtual_user,
            ConversationMessage::User {
                id: MessageId::new(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: ToolUseId::new(),
                    content: crate::types::utf16_json::Utf16JsonProjection::root_string(
                        "result".into(),
                        "result".encode_utf16().collect(),
                    )
                    .unwrap(),
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
                api_message_override: None,
                interrupted_by_shutdown: None,
                refused_message_id: None,
            },
            second.clone(),
        ]);
        assert_eq!(session.real_user_turns(), 2);

        session.replace_model_context_history(vec![
            ConversationMessage::compact_summary(MessageId::new(), "summary".into()),
            second,
        ]);
        assert_eq!(session.compacted_user_turns, 1);
        assert_eq!(session.real_user_turns(), 2);

        let third = ConversationMessage::user(MessageId::new(), "third".into());
        session.history.push(third.clone());
        assert_eq!(session.real_user_turns(), 3);
        session.replace_model_context_history(vec![
            ConversationMessage::compact_summary(MessageId::new(), "summary 2".into()),
            third,
        ]);
        assert_eq!(session.compacted_user_turns, 2);
        assert_eq!(session.real_user_turns(), 3);

        let restored: SessionState =
            serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
        assert_eq!(restored.real_user_turns(), 3);
    }
}

#[cfg(test)]
mod unanswered_turn_tests {
    use super::SessionState;
    use super::{
        NATIVE_ENGINE_META_EXACT, NATIVE_ENGINE_META_HOOK_EVENTS,
        NATIVE_ENGINE_META_HOOK_FEEDBACK_SUFFIX, NATIVE_ENGINE_META_PREFIXES,
    };
    use crate::types::{ContentBlock, ConversationMessage, MessageId, SessionId, ToolUseId};

    fn state_with(history: Vec<ConversationMessage>) -> SessionState {
        let mut session = SessionState::empty(SessionId::new(), "model".into());
        session.history = history;
        session
    }

    fn shutdown_marker() -> ConversationMessage {
        let mut row =
            ConversationMessage::user(MessageId::new(), "[Request interrupted by user]".into());
        if let ConversationMessage::User {
            interrupted_by_shutdown,
            ..
        } = &mut row
        {
            *interrupted_by_shutdown = Some(true);
        }
        row
    }

    #[test]
    fn shutdown_tail_is_unanswered_through_native_interruption_furniture() {
        let anchor = ConversationMessage::user(MessageId::new(), "request".into());
        let anchor_id = anchor.id();
        let tool_result = ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "partial tool result".into(),
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
        };
        let session = state_with(vec![
            anchor,
            ConversationMessage::Assistant {
                per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: "partial answer".into(),
                    citations: None,
                }],
                stop_reason: None,
                resumable: None,
            },
            tool_result,
            ConversationMessage::compact_summary(MessageId::new(), "summary".into()),
            ConversationMessage::user_meta(MessageId::new(), "Goal check-in: «progress".into()),
            ConversationMessage::user(MessageId::new(), "[Request interrupted by user]".into()),
            shutdown_marker(),
        ]);

        assert!(session.message_turn_is_unanswered(anchor_id));
    }

    #[test]
    fn only_shutdown_tail_or_no_response_keeps_a_persisted_turn_unanswered() {
        let anchor = ConversationMessage::user(MessageId::new(), "request".into());
        let anchor_id = anchor.id();
        assert!(state_with(vec![anchor.clone()]).message_turn_is_unanswered(anchor_id));

        let unflagged =
            ConversationMessage::user(MessageId::new(), "[Request interrupted by user]".into());
        assert!(!state_with(vec![anchor.clone(), unflagged]).message_turn_is_unanswered(anchor_id));

        let mut explicit_false =
            ConversationMessage::user(MessageId::new(), "[Request interrupted by user]".into());
        if let ConversationMessage::User {
            interrupted_by_shutdown,
            ..
        } = &mut explicit_false
        {
            *interrupted_by_shutdown = Some(false);
        }
        assert!(
            !state_with(vec![anchor.clone(), explicit_false]).message_turn_is_unanswered(anchor_id)
        );

        let intervening_user =
            ConversationMessage::user(MessageId::new(), "new human input".into());
        assert!(
            !state_with(vec![anchor.clone(), intervening_user, shutdown_marker()])
                .message_turn_is_unanswered(anchor_id)
        );

        assert!(!state_with(vec![ConversationMessage::user(
            MessageId::new(),
            "other".into(),
        )])
        .message_turn_is_unanswered(anchor_id));

        let ordinary_notice =
            ConversationMessage::user(MessageId::new(), "Goal check-in: «progress".into());
        assert!(
            !state_with(vec![anchor, ordinary_notice, shutdown_marker()])
                .message_turn_is_unanswered(anchor_id),
            "Native I4e only skips recognized internal notices when isMeta is true"
        );
    }

    #[test]
    fn native_engine_meta_allowlist_requires_exact_text_and_meta_flag() {
        let anchor = ConversationMessage::user(MessageId::new(), "request".into());
        let anchor_id = anchor.id();
        let recognized = [
            "[structured-output-enforce] additional detail",
            "[projects-reply-gate] additional detail",
            "You ended the turn without calling SendUserMessage. additional detail",
            "Goal check-in: «progress",
            "The previous response failed to produce a valid tool call. Please retry the tool call now.",
            "Your tool call was malformed and could not be parsed. Please retry.",
            "[Your previous response had no visible output. Please continue and produce a user-visible response.]",
            "The PermissionDenied hook indicated you may retry this tool call.",
            "Your response above was stopped by a safety classifier — this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded.",
            "Your response above was stopped by a safety classifier — this is not a tool or API error. The rest of it was withheld, and tool calls in it that had not finished did not run. Do not produce that content again, even reworded. Exception: a tool call whose result reads \"Interrupted\" was already running when the response was stopped; it may have partially or fully completed.",
            "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.",
            "Output token limit hit while you were still reasoning, before this response produced any text or tool call. That reasoning was not kept and is not part of this conversation, so there is nothing to resume. Continue from the conversation above: reason much more briefly and produce your answer or next tool call well within the output limit.",
            "Your response was cut off because it exceeded the output token limit. Continue from where you left off. What you write next is shown as a separate block below the text so far, so begin at the start of the line, table row, list item or sentence that was cut, writing it again in full, and if you were inside a code block or a table, open it again first (the same code fence and language, the table's header row). Repeat nothing before that point and do not comment on the interruption. Keep going until the response is complete.",
            "Your response above was cut off mid-stream. Resume directly from where it stops — no apology, no recap. If none of it survived, answer the request from the start.",
            "Your response above was cut off mid-stream and only your next message is delivered. Write the complete response again from the start — no apology, no mention of the cut-off.",
            "Stop hook feedback:\nstop detail",
            "TeammateIdle hook feedback:\nidle detail",
            "TaskCreated hook feedback:\ncreated detail",
            "TaskCompleted hook feedback:\ncompleted detail",
        ];

        for text in recognized {
            let meta = ConversationMessage::user_meta(MessageId::new(), text.into());
            assert!(
                state_with(vec![anchor.clone(), meta, shutdown_marker()])
                    .message_turn_is_unanswered(anchor_id),
                "Native internal notice was not recognized: {text}"
            );

            let ordinary = ConversationMessage::user(MessageId::new(), text.into());
            assert!(
                !state_with(vec![anchor.clone(), ordinary, shutdown_marker()])
                    .message_turn_is_unanswered(anchor_id),
                "ordinary user text must not be treated as Native internal meta: {text}"
            );
        }

        for near_miss in [
            "Your previous response had no visible output. Please continue and produce a user-visible response.",
            "Your response was cut off because it exceeded the output token limit. Continue from where you left off. What you write next is shown as a separate block below the text so far, so begin at the start of the line, list item or sentence that was cut.",
        ] {
            let meta = ConversationMessage::user_meta(MessageId::new(), near_miss.into());
            assert!(
                !state_with(vec![anchor.clone(), meta, shutdown_marker()])
                    .message_turn_is_unanswered(anchor_id),
                "near-miss text must not be treated as an exact Native internal notice: {near_miss}"
            );
        }
    }

    #[test]
    fn native_meta_classifier_matches_extracted_295_utf16_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/native-meta-allowlist-295.json")).unwrap();
        assert_eq!(
            fixture["native"]["binarySha256"],
            "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
        );
        assert_eq!(
            fixture["source"]["i4e"]["sha256"],
            "45aa5349661a7e8ea971e78ab67a1512977a96c1af2615d24e0626e930703a21"
        );

        let fixture_strings = |name: &str| {
            fixture["classification"][name]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["text"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let assert_utf16_carriers = |name: &str| {
            for item in fixture["classification"][name].as_array().unwrap() {
                let text = item["text"].as_str().unwrap();
                let expected = item["utf16CodeUnits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|unit| unit.as_u64().unwrap() as u16)
                    .collect::<Vec<_>>();
                assert_eq!(text.encode_utf16().collect::<Vec<_>>(), expected);
            }
        };

        assert_eq!(
            fixture_strings("prefixes"),
            NATIVE_ENGINE_META_PREFIXES
                .iter()
                .map(|text| (*text).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fixture_strings("exact"),
            NATIVE_ENGINE_META_EXACT
                .iter()
                .map(|text| (*text).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            fixture["classification"]["hookEvents"],
            serde_json::json!(NATIVE_ENGINE_META_HOOK_EVENTS)
        );
        assert_eq!(
            fixture["classification"]["hookFeedbackSuffix"]["text"],
            NATIVE_ENGINE_META_HOOK_FEEDBACK_SUFFIX
        );
        assert_utf16_carriers("prefixes");
        assert_utf16_carriers("exact");

        let suffix = fixture["classification"]["hookFeedbackSuffix"]["utf16CodeUnits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| unit.as_u64().unwrap() as u16)
            .collect::<Vec<_>>();
        assert_eq!(
            NATIVE_ENGINE_META_HOOK_FEEDBACK_SUFFIX
                .encode_utf16()
                .collect::<Vec<_>>(),
            suffix
        );
        assert!(fixture["classification"]["requiresIsMeta"]
            .as_bool()
            .unwrap());
    }
}

#[cfg(test)]
mod m4_04_session_extension_tests {
    use super::*;
    use crate::types::SessionId;

    fn todo_text(text: &str) -> Utf16JsonProjection {
        let units = text.encode_utf16().collect::<Vec<_>>();
        Utf16JsonProjection::root_string(text.to_owned(), units)
            .expect("todo test text is valid JSON")
    }

    #[test]
    fn fresh_session_has_empty_todos_and_plan_mode_false() {
        let s = SessionState::empty(SessionId::nil(), "x".into());
        assert!(s.todos.is_empty(), "fresh session must have no todos");
        assert!(!s.plan_mode, "fresh session must not be in plan mode");
    }

    #[test]
    fn active_goal_condition_and_reason_keep_exact_utf16_in_session_json() {
        let source_text = |units: Vec<u16>| {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                .expect("source text is valid JSON")
        };
        let condition_units = vec![b'c' as u16, 0xd800, b'd' as u16];
        let reason_units = vec![b'r' as u16, 0xdfff];
        let mut session = SessionState::empty(SessionId::nil(), "x".into());
        session.active_goal = Some(ActiveGoalState {
            condition: source_text(condition_units.clone()),
            set_at: SystemTime::UNIX_EPOCH,
            last_reason: Some(source_text(reason_units.clone())),
            iterations: 7,
            tokens_at_start: 23,
            origin: GoalOrigin::User,
        });

        let serialized = serde_json::to_string(&session).expect("serialize session");
        let restored: SessionState = serde_json::from_str(&serialized).expect("restore session");
        let goal = restored.active_goal.expect("restore active goal");
        assert_eq!(goal.condition.string_units(""), Some(condition_units));
        assert_eq!(
            goal.last_reason.unwrap().string_units(""),
            Some(reason_units)
        );
        assert_eq!(goal.iterations, 7);
        assert_eq!(goal.tokens_at_start, 23);
    }

    #[test]
    fn session_with_todos_round_trips_via_json() {
        let mut s = SessionState::empty(SessionId::nil(), "x".into());
        s.todos.push(TodoItem {
            id: "t1".into(),
            content: todo_text("buy milk"),
            status: TodoState::Pending,
            active_form: todo_text("Buying milk"),
        });
        s.todos.push(TodoItem {
            id: "t2".into(),
            content: todo_text("write plan"),
            status: TodoState::InProgress,
            active_form: todo_text("Writing plan"),
        });
        s.plan_mode = true;
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            json.contains(r#""status":"pending""#),
            "pending wire literal: {json}"
        );
        assert!(
            json.contains(r#""status":"in_progress""#),
            "in_progress wire literal: {json}"
        );
        assert!(
            json.contains(r#""plan_mode":true"#),
            "plan_mode wire literal: {json}"
        );
        let back: SessionState = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back.todos.len(), 2);
        assert_eq!(back.todos[0].id, "t1");
        assert_eq!(back.todos[0].status, TodoState::Pending);
        assert_eq!(back.todos[1].status, TodoState::InProgress);
        assert!(back.plan_mode);
    }

    #[test]
    fn todo_text_round_trips_unpaired_utf16_through_session_json() {
        let content_units = vec![b'a' as u16, 0xD800, b'b' as u16];
        let active_units = vec![0xDC00, b'x' as u16];
        let source_text = |units: &[u16]| {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
                .expect("source todo text is valid JSON")
        };
        let mut session = SessionState::empty(SessionId::nil(), "x".into());
        session.todos.push(TodoItem {
            id: "exact".into(),
            content: source_text(&content_units),
            status: TodoState::InProgress,
            active_form: source_text(&active_units),
        });

        let persisted = serde_json::to_string(&session).expect("serialize session");
        let restored: SessionState = serde_json::from_str(&persisted).expect("restore session");
        assert_eq!(
            restored.todos[0].content.string_units(""),
            Some(content_units.clone())
        );
        assert_eq!(
            restored.todos[0].active_form.string_units(""),
            Some(active_units.clone())
        );
    }

    #[test]
    fn todo_defaults_omitted_active_form_to_empty_source_text() {
        let item = r#"{"id":"t1","content":"buy milk","status":"pending"}"#;
        let parsed: TodoItem = serde_json::from_str(item).unwrap();
        assert_eq!(parsed.active_form.string_at(""), Some(""));
    }

    #[test]
    fn todo_state_serializes_to_locked_literals() {
        assert_eq!(
            serde_json::to_string(&TodoState::Pending).unwrap(),
            r#""pending""#
        );
        assert_eq!(
            serde_json::to_string(&TodoState::InProgress).unwrap(),
            r#""in_progress""#
        );
        assert_eq!(
            serde_json::to_string(&TodoState::Completed).unwrap(),
            r#""completed""#
        );
    }

    #[test]
    fn todo_state_rejects_done_alias() {
        let err = serde_json::from_str::<TodoState>(r#""done""#).expect_err(
            "'done' must NOT be accepted (spec §7 locks pending/in_progress/completed)",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown variant"),
            "msg should name unknown variant: {msg}"
        );
        assert!(msg.contains("done"), "msg should echo the bad input: {msg}");
    }

    #[test]
    fn todo_state_rejects_todo_alias() {
        let err = serde_json::from_str::<TodoState>(r#""todo""#)
            .expect_err("'todo' must NOT be accepted");
        let msg = format!("{err}");
        assert!(msg.contains("unknown variant"), "msg: {msg}");
    }
}
