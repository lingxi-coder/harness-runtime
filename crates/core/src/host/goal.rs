//! Host capabilities for nonblocking, model-proposed session goals.
//!
//! The host owns user approval and queued `/goal` dispatch. Returning from the
//! tool never activates a goal: the ordinary slash-command path does that when
//! its kickoff is delivered.

use async_trait::async_trait;

use crate::types::utf16_json::Utf16JsonProjection;

/// Authenticated ingress facts for a slash invocation. Payload text and
/// persisted transcript fields must never mint these facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalCommandSource {
    /// The transport dispatched this invocation over a bridge.
    pub dispatched_over_bridge: bool,
    /// That transport verified a relayed Slack/Teams/project human turn.
    pub submission_verified_slack_human_turn: bool,
}

tokio::task_local! {
    static GOAL_COMMAND_SOURCE: GoalCommandSource;
}

/// Scope the actual transport's verification result to one command dispatch.
pub async fn with_goal_command_source<F: std::future::Future>(
    source: GoalCommandSource,
    dispatch: F,
) -> F::Output {
    GOAL_COMMAND_SOURCE.scope(source, dispatch).await
}

/// Read ingress proof without borrowing another task's or a previous turn's.
#[must_use]
pub fn current_goal_command_source() -> GoalCommandSource {
    GOAL_COMMAND_SOURCE
        .try_with(|source| *source)
        .unwrap_or_default()
}

/// Consent policy read only from trusted user, command-line and managed layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelProposedGoals {
    /// The proposal's `ask_user` parameter chooses whether approval is needed.
    #[default]
    Auto,
    /// Every proposal requires a user decision.
    AlwaysAsk,
    /// Model proposals are disabled; a user may still type `/goal`.
    Disabled,
}

impl ModelProposedGoals {
    /// Native settings spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::AlwaysAsk => "alwaysAsk",
            Self::Disabled => "disabled",
        }
    }
}

/// Live host facts consulted at invocation time, rather than model input.
#[derive(Debug, Clone, Default)]
pub struct GoalProposalState {
    /// This is an interactive local main session.
    pub interactive_local: bool,
    /// Exact evaluator gate refusal, if trust or hooks policy blocks a goal.
    pub evaluator_gate_error: Option<String>,
    /// The enforcing permission gate is currently in plan mode.
    pub plan_mode: bool,
    /// Current trusted consent policy.
    pub setting: ModelProposedGoals,
    /// A real user decision transport is mounted for this session.
    pub dialog_available: bool,
}

/// Nonblocking proposal transport supplied by the owning session.
#[async_trait]
pub trait GoalProposalHost: Send + Sync {
    /// Synchronous catalog admission, including session shape and live setting.
    /// The tool separately checks the native `tengu_propose_goal` feature flag.
    fn is_available(&self) -> bool;

    /// Refresh authoritative invocation-time policy and transport facts.
    async fn state(&self) -> GoalProposalState;

    /// Start an approval or enqueue a direct kickoff, then return immediately.
    /// The host atomically rejects a second pending proposal, rechecks policy
    /// after approval, and discards stale decisions after a session switch.
    ///
    /// # Errors
    /// Returns the exact model-facing refusal; no kickoff was accepted.
    async fn submit(&self, condition: Utf16JsonProjection, ask_user: bool) -> Result<(), String>;
}

/// Native `n3(H5e(condition)).trim()`: one display line, no invisible format,
/// private-use, unassigned or ill-formed UTF-16 characters, and eight-column
/// tab stops. The returned text is the exact condition the user approves.
#[must_use]
pub fn canonicalize_goal_proposal_condition(units: &[u16]) -> String {
    use unicode_general_category::{get_general_category, GeneralCategory};
    use unicode_width::UnicodeWidthStr as _;

    let mut clean = String::new();
    let mut in_line_break = false;
    for ch in char::decode_utf16(units.iter().copied()).filter_map(Result::ok) {
        if matches!(ch, '\r' | '\n' | '\u{2028}' | '\u{2029}') {
            if !in_line_break {
                clean.push(' ');
            }
            in_line_break = true;
            continue;
        }
        in_line_break = false;
        if matches!(
            get_general_category(ch),
            GeneralCategory::Format | GeneralCategory::PrivateUse | GeneralCategory::Unassigned
        ) || is_default_ignorable(ch)
        {
            continue;
        }
        let codepoint = u32::from(ch);
        if (codepoint < 32 && ch != '\t') || (127..=159).contains(&codepoint) {
            clean.push('\u{fffd}');
        } else {
            clean.push(ch);
        }
    }

    let mut expanded = String::with_capacity(clean.len());
    let mut column = 0usize;
    for part in clean.split_inclusive('\t') {
        if let Some(text) = part.strip_suffix('\t') {
            expanded.push_str(text);
            column += text.width();
            let spaces = 8 - column % 8;
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            expanded.push_str(part);
        }
    }
    expanded
        .trim_matches(|ch: char| (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}')
        .to_owned()
}

fn is_default_ignorable(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x00ad
            | 0x034f
            | 0x061c
            | 0x115f..=0x1160
            | 0x17b4..=0x17b5
            | 0x180b..=0x180f
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x206f
            | 0x3164
            | 0xfe00..=0xfe0f
            | 0xfeff
            | 0xffa0
            | 0xfff0..=0xfffb
            | 0x16fe4
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0000..=0xe0fff
    )
}
