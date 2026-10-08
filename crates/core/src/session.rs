//! Session state model. Persistence lives in `lingxi-session` (Plan 10).

use crate::token::Usage;
use crate::types::{ConversationMessage, MessageId, SessionId, ToolUseId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
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
    /// Human-readable description of the todo.
    pub content: String,
    /// Lifecycle state of this todo. Wire-locked.
    pub status: TodoState,
    /// Present-tense form shown while the todo is in progress (claude-code
    /// `TodoItemSchema.activeForm`). Wire key is camelCase `activeForm`;
    /// `default` keeps pre-`activeForm` persisted sessions deserializable.
    #[serde(rename = "activeForm", default)]
    pub active_form: String,
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
    pub condition: String,
    /// When the goal became active.
    pub set_at: SystemTime,
    /// Most recent stop-time evaluation reason, when available.
    #[serde(default)]
    pub last_reason: Option<String>,
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
            todos: Vec::new(),
            active_goal: None,
            message_timing: MessageTimingState::default(),
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
                     .. } if !self.virtual_user_messages.contains(id)
                        && !self.compact_summary_messages.contains(id)
                        && !self.transcript_only_messages.contains(id)
                        && !content.iter().any(|block| matches!(block, crate::types::ContentBlock::ToolResult { .. }))
                )
            })
            .count() as u64
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
            ConversationMessage::User { api_message_override: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: ToolUseId::new(),
                    content: "result".into(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
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
mod m4_04_session_extension_tests {
    use super::*;
    use crate::types::SessionId;

    #[test]
    fn fresh_session_has_empty_todos_and_plan_mode_false() {
        let s = SessionState::empty(SessionId::nil(), "x".into());
        assert!(s.todos.is_empty(), "fresh session must have no todos");
        assert!(!s.plan_mode, "fresh session must not be in plan mode");
    }

    #[test]
    fn session_with_todos_round_trips_via_json() {
        let mut s = SessionState::empty(SessionId::nil(), "x".into());
        s.todos.push(TodoItem {
            id: "t1".into(),
            content: "buy milk".into(),
            status: TodoState::Pending,
            active_form: "Buying milk".into(),
        });
        s.todos.push(TodoItem {
            id: "t2".into(),
            content: "write plan".into(),
            status: TodoState::InProgress,
            active_form: "Writing plan".into(),
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
