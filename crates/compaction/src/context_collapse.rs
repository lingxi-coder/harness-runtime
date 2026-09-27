//! Read-time context-collapse projection.
//!
//! Claude Code keeps the original REPL history intact and stores collapse
//! decisions separately. Each committed decision replaces one inclusive,
//! contiguous UUID span with a synthetic meta-user message whose body is the
//! byte-stable `<collapsed id="…">…</collapsed>` envelope. Replaying commits
//! in order matters: a later collapse may use an earlier summary UUID as one of
//! its boundaries.

use protocol::{ConversationMessage, MessageId};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// Process environment gate for the collapse runtime.
///
/// The product keeps its clean-break `LINGXI_*` namespace. Like the existing
/// history-snip gate, only the exact bytes `"1"` and `"true"` enable it.
pub const CONTEXT_COLLAPSE_ENV: &str = "LINGXI_CONTEXT_COLLAPSE";

/// Persisted context-collapse commit discriminator.
pub const COMMIT_RECORD_TYPE: &str = "marble-origami-commit";
/// Persisted staged-state snapshot discriminator.
pub const SNAPSHOT_RECORD_TYPE: &str = "marble-origami-snapshot";
/// Persisted reset tombstone discriminator.
pub const RESET_RECORD_TYPE: &str = "marble-origami-reset";

/// Whether context collapse is active for this process.
///
/// Absent, non-Unicode, mixed-case, whitespace-padded, or any other value is
/// false, keeping the feature disabled by default.
#[must_use]
pub fn is_context_collapse_enabled() -> bool {
    std::env::var(CONTEXT_COLLAPSE_ENV).is_ok_and(|value| value == "1" || value == "true")
}

/// A collapse candidate produced by the context-analysis agent but not yet
/// visible in the model-facing projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedCollapse {
    /// Inclusive first message UUID of the candidate span.
    pub start_uuid: String,
    /// Inclusive last message UUID of the candidate span.
    pub end_uuid: String,
    /// Plain summary text to wrap when the candidate is committed.
    pub summary: String,
    /// Context-agent risk score.
    pub risk: f64,
    /// JavaScript epoch milliseconds at which the candidate was staged.
    pub staged_at: u64,
}

/// Append-only persisted commit payload.
///
/// Field declaration order intentionally matches Claude Code's object-spread
/// order so `serde_json::to_string` produces the same payload ordering when a
/// writer prepends `type` and `sessionId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCollapseCommit {
    /// Zero-padded, sixteen-digit decimal collapse identifier.
    pub collapse_id: String,
    /// UUID of the synthetic summary placeholder.
    pub summary_uuid: String,
    /// Complete `<collapsed id="…">…</collapsed>` body.
    pub summary_content: String,
    /// Plain summary text used by context inspection.
    pub summary: String,
    /// Inclusive first archived message UUID.
    pub first_archived_uuid: String,
    /// Inclusive last archived message UUID.
    pub last_archived_uuid: String,
}

/// Last-wins snapshot of staged candidates and spawn-trigger state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCollapseSnapshot {
    /// Candidates waiting to be committed.
    pub staged: Vec<StagedCollapse>,
    /// Whether the background collapse producer is armed.
    pub armed: bool,
    /// Token count at the most recent producer spawn.
    pub last_spawn_tokens: u64,
}

/// Reset tombstone payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCollapseReset {
    /// Human-readable reset reason persisted with the tombstone.
    pub reason: String,
}

/// Health counters exposed by context inspection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextCollapseHealth {
    /// Context-agent spawns attempted.
    pub total_spawns: u64,
    /// Context-agent failures observed.
    pub total_errors: u64,
    /// Last producer error, if any.
    pub last_error: Option<String>,
    /// Whether the empty-spawn warning has already been emitted.
    pub empty_spawn_warning_emitted: bool,
    /// Producer runs that returned no candidates.
    pub total_empty_spawns: u64,
}

/// Current context-collapse statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextCollapseStats {
    /// Commit-log length. Stale commits are still counted until a reset, matching
    /// the persisted log; projection silently skips missing boundaries.
    pub collapsed_spans: usize,
    /// Number of source messages replaced during the latest projection.
    pub collapsed_messages: usize,
    /// Candidates currently waiting to be committed.
    pub staged_spans: usize,
    /// Producer health counters.
    pub health: ContextCollapseHealth,
}

/// Result of replaying the committed collapse log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapseResult {
    /// Model-facing projected history.
    pub messages: Vec<ConversationMessage>,
    /// Number of committed spans that resolved in this history.
    pub collapsed_spans: usize,
    /// Number of messages replaced by those spans.
    pub collapsed_messages: usize,
}

/// Result of draining staged candidates after a prompt-too-long response.
#[derive(Debug, Clone, PartialEq)]
pub struct DrainResult {
    /// Newly committed payloads, in commit order.
    pub commits: Vec<ContextCollapseCommit>,
    /// Projected history after the drain.
    pub messages: Vec<ConversationMessage>,
    /// Last-wins snapshot to persist after the staged queue changed.
    pub snapshot: ContextCollapseSnapshot,
}

#[derive(Debug)]
struct ContextCollapseState {
    commits: Vec<ContextCollapseCommit>,
    staged: Vec<StagedCollapse>,
    armed: bool,
    last_spawn_tokens: u64,
    next_collapse_id: u64,
    collapsed_messages: usize,
    health: ContextCollapseHealth,
}

impl Default for ContextCollapseState {
    fn default() -> Self {
        Self {
            commits: Vec::new(),
            staged: Vec::new(),
            armed: false,
            last_spawn_tokens: 0,
            next_collapse_id: 1,
            collapsed_messages: 0,
            health: ContextCollapseHealth::default(),
        }
    }
}

/// Session-owned context-collapse store.
///
/// Methods are synchronous and never hold the mutex across I/O. Callers persist
/// returned commit/snapshot/reset payloads through the session writer.
#[derive(Debug, Default)]
pub struct ContextCollapse {
    state: Mutex<ContextCollapseState>,
}

impl ContextCollapse {
    /// Return the current stats snapshot.
    #[must_use]
    pub fn stats(&self) -> ContextCollapseStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ContextCollapseStats {
            collapsed_spans: state.commits.len(),
            collapsed_messages: state.collapsed_messages,
            staged_spans: state.staged.len(),
            health: state.health.clone(),
        }
    }

    /// Replace producer trigger state without changing the staged/committed log.
    pub fn set_spawn_state(&self, armed: bool, last_spawn_tokens: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.armed = armed;
        state.last_spawn_tokens = last_spawn_tokens;
    }

    /// Append one context-agent candidate to the staged queue.
    pub fn stage(&self, collapse: StagedCollapse) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .staged
            .push(collapse);
    }

    /// Return the last-wins persisted snapshot shape.
    #[must_use]
    pub fn snapshot(&self) -> ContextCollapseSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot_of(&state)
    }

    /// Restore the ordered commit log and optional last-wins staged snapshot.
    ///
    /// Existing state is always cleared first. The next ID is reseeded from the
    /// largest valid decimal `collapseId`; malformed IDs stay replayable but do
    /// not affect the counter.
    pub fn restore_from_entries(
        &self,
        commits: Vec<ContextCollapseCommit>,
        snapshot: Option<ContextCollapseSnapshot>,
    ) {
        let next_collapse_id = commits
            .iter()
            .filter_map(|commit| commit.collapse_id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        let snapshot = snapshot.unwrap_or(ContextCollapseSnapshot {
            staged: Vec::new(),
            armed: false,
            last_spawn_tokens: 0,
        });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = ContextCollapseState {
            commits,
            staged: snapshot.staged,
            armed: snapshot.armed,
            last_spawn_tokens: snapshot.last_spawn_tokens,
            next_collapse_id,
            collapsed_messages: 0,
            health: ContextCollapseHealth::default(),
        };
    }

    /// Parse and restore records returned by the tolerant JSONL reader.
    ///
    /// Unknown outer fields (`type`, `sessionId`) are intentionally ignored by
    /// Serde, so the reader's complete side-record values can be passed through.
    pub fn restore_from_json_entries(
        &self,
        commits: &[serde_json::Value],
        snapshot: Option<&serde_json::Value>,
    ) -> Result<(), serde_json::Error> {
        let commits = commits
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<Vec<ContextCollapseCommit>, _>>()?;
        let snapshot = snapshot.cloned().map(serde_json::from_value).transpose()?;
        self.restore_from_entries(commits, snapshot);
        Ok(())
    }

    /// Replay the committed log over a raw history without mutating that history.
    #[must_use]
    pub fn project_view(&self, messages: Vec<ConversationMessage>) -> CollapseResult {
        let commits = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commits
            .clone();
        let result = project_commits(messages, &commits);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .collapsed_messages = result.collapsed_messages;
        result
    }

    /// Generate the current committed projection.
    ///
    /// Candidate production/selection is deliberately outside this pure store;
    /// this operation never invents a span or calls an LLM.
    #[must_use]
    pub fn apply_collapses_if_needed(&self, messages: Vec<ConversationMessage>) -> CollapseResult {
        self.project_view(messages)
    }

    /// Commit every valid staged span and return the post-drain projection.
    ///
    /// This is the prompt-too-long recovery primitive. Each candidate is
    /// resolved against the view produced by earlier commits, enabling nested
    /// spans that reference an earlier summary UUID. Missing/reversed spans are
    /// discarded from the staged queue and produce no commit.
    #[must_use]
    pub fn recover_from_overflow(&self, messages: Vec<ConversationMessage>) -> DrainResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut projected = project_commits(messages, &state.commits).messages;
        let staged = std::mem::take(&mut state.staged);
        let mut committed = Vec::new();

        for candidate in staged {
            if find_span(&projected, &candidate.start_uuid, &candidate.end_uuid).is_none() {
                continue;
            }
            let commit = make_commit(&mut state, candidate);
            let one = project_commits(projected, std::slice::from_ref(&commit));
            projected = one.messages;
            state.collapsed_messages = state
                .collapsed_messages
                .saturating_add(one.collapsed_messages);
            state.commits.push(commit.clone());
            committed.push(commit);
        }

        DrainResult {
            commits: committed,
            messages: projected,
            snapshot: snapshot_of(&state),
        }
    }

    /// Clear every committed/staged collapse and return the reset tombstone.
    #[must_use]
    pub fn reset(&self, reason: impl Into<String>) -> ContextCollapseReset {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = ContextCollapseState::default();
        ContextCollapseReset {
            reason: reason.into(),
        }
    }
}

fn snapshot_of(state: &ContextCollapseState) -> ContextCollapseSnapshot {
    ContextCollapseSnapshot {
        staged: state.staged.clone(),
        armed: state.armed,
        last_spawn_tokens: state.last_spawn_tokens,
    }
}

fn make_commit(
    state: &mut ContextCollapseState,
    candidate: StagedCollapse,
) -> ContextCollapseCommit {
    let collapse_id = format!("{:016}", state.next_collapse_id);
    state.next_collapse_id = state.next_collapse_id.saturating_add(1);
    let summary_uuid = MessageId::new().as_uuid().to_string();
    let summary_content = format!(
        "<collapsed id=\"{collapse_id}\">{}</collapsed>",
        candidate.summary
    );
    ContextCollapseCommit {
        collapse_id,
        summary_uuid,
        summary_content,
        summary: candidate.summary,
        first_archived_uuid: candidate.start_uuid,
        last_archived_uuid: candidate.end_uuid,
    }
}

fn project_commits(
    mut messages: Vec<ConversationMessage>,
    commits: &[ContextCollapseCommit],
) -> CollapseResult {
    let mut collapsed_spans = 0usize;
    let mut collapsed_messages = 0usize;
    for commit in commits {
        let Some((start, end)) = find_span(
            &messages,
            &commit.first_archived_uuid,
            &commit.last_archived_uuid,
        ) else {
            continue;
        };
        let Some(summary_id) = MessageId::parse_prefixed(&commit.summary_uuid) else {
            continue;
        };
        collapsed_spans = collapsed_spans.saturating_add(1);
        collapsed_messages = collapsed_messages.saturating_add(end - start + 1);
        messages.splice(
            start..=end,
            [ConversationMessage::user_meta(
                summary_id,
                commit.summary_content.clone(),
            )],
        );
    }
    CollapseResult {
        messages,
        collapsed_spans,
        collapsed_messages,
    }
}

fn find_span(
    messages: &[ConversationMessage],
    first_uuid: &str,
    last_uuid: &str,
) -> Option<(usize, usize)> {
    let first = messages
        .iter()
        .position(|message| message.id().as_uuid().to_string() == first_uuid)?;
    let last = messages
        .iter()
        .enumerate()
        .skip(first)
        .find_map(|(idx, message)| {
            (message.id().as_uuid().to_string() == last_uuid).then_some(idx)
        })?;
    Some((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn id(value: &str) -> MessageId {
        MessageId::parse_prefixed(value).expect("valid UUID")
    }

    fn user(value: &str, text: &str) -> ConversationMessage {
        ConversationMessage::user(id(value), text.to_string())
    }

    fn commit(
        collapse_id: &str,
        summary_uuid: &str,
        first: &str,
        last: &str,
        summary: &str,
    ) -> ContextCollapseCommit {
        ContextCollapseCommit {
            collapse_id: collapse_id.to_string(),
            summary_uuid: summary_uuid.to_string(),
            summary_content: format!("<collapsed id=\"{collapse_id}\">{summary}</collapsed>"),
            summary: summary.to_string(),
            first_archived_uuid: first.to_string(),
            last_archived_uuid: last.to_string(),
        }
    }

    #[test]
    fn gate_is_disabled_by_default_and_parses_exact_env_bool() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var(CONTEXT_COLLAPSE_ENV).ok();
        for (value, expected) in [
            (None, false),
            (Some("1"), true),
            (Some("true"), true),
            (Some("TRUE"), false),
            (Some(" true"), false),
            (Some("0"), false),
        ] {
            match value {
                Some(value) => std::env::set_var(CONTEXT_COLLAPSE_ENV, value),
                None => std::env::remove_var(CONTEXT_COLLAPSE_ENV),
            }
            assert_eq!(is_context_collapse_enabled(), expected, "value={value:?}");
        }
        match saved {
            Some(value) => std::env::set_var(CONTEXT_COLLAPSE_ENV, value),
            None => std::env::remove_var(CONTEXT_COLLAPSE_ENV),
        }
    }

    #[test]
    fn commit_payload_serializes_in_claude_field_order() {
        let payload = commit(
            "0000000000000001",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            "summary",
        );
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            r#"{"collapseId":"0000000000000001","summaryUuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","summaryContent":"<collapsed id=\"0000000000000001\">summary</collapsed>","summary":"summary","firstArchivedUuid":"11111111-1111-4111-8111-111111111111","lastArchivedUuid":"22222222-2222-4222-8222-222222222222"}"#
        );
    }

    #[test]
    fn project_view_replaces_inclusive_span_with_meta_summary() {
        let first = "11111111-1111-4111-8111-111111111111";
        let last = "33333333-3333-4333-8333-333333333333";
        let tail = "44444444-4444-4444-8444-444444444444";
        let summary_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let store = ContextCollapse::default();
        store.restore_from_entries(
            vec![commit(
                "0000000000000001",
                summary_id,
                first,
                last,
                "folded",
            )],
            None,
        );

        let result = store.project_view(vec![
            user(first, "one"),
            user("22222222-2222-4222-8222-222222222222", "two"),
            user(last, "three"),
            user(tail, "tail"),
        ]);

        assert_eq!(result.collapsed_spans, 1);
        assert_eq!(result.collapsed_messages, 3);
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].id(), id(summary_id));
        assert_eq!(
            result.messages[0].text_content(),
            "<collapsed id=\"0000000000000001\">folded</collapsed>"
        );
        assert!(result.messages[0].is_meta());
        assert_eq!(result.messages[1].id(), id(tail));
    }

    #[test]
    fn commits_replay_in_order_and_allow_nested_summary_boundaries() {
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let c = "33333333-3333-4333-8333-333333333333";
        let summary_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let summary_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let store = ContextCollapse::default();
        store.restore_from_entries(
            vec![
                commit("0000000000000001", summary_a, a, b, "first"),
                commit("0000000000000002", summary_b, summary_a, c, "second"),
            ],
            None,
        );

        let result = store.project_view(vec![user(a, "a"), user(b, "b"), user(c, "c")]);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].id(), id(summary_b));
        assert_eq!(result.collapsed_spans, 2);
        assert_eq!(result.collapsed_messages, 4);
    }

    #[test]
    fn overflow_drain_commits_valid_staged_spans_and_reseeds_ids() {
        let a = "11111111-1111-4111-8111-111111111111";
        let b = "22222222-2222-4222-8222-222222222222";
        let store = ContextCollapse::default();
        store.restore_from_entries(
            vec![commit(
                "0000000000000009",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "99999999-9999-4999-8999-999999999999",
                "99999999-9999-4999-8999-999999999999",
                "stale",
            )],
            Some(ContextCollapseSnapshot {
                staged: vec![StagedCollapse {
                    start_uuid: a.to_string(),
                    end_uuid: b.to_string(),
                    summary: "drained".to_string(),
                    risk: 0.25,
                    staged_at: 123,
                }],
                armed: true,
                last_spawn_tokens: 90_000,
            }),
        );

        let result = store.recover_from_overflow(vec![user(a, "a"), user(b, "b")]);
        assert_eq!(result.commits.len(), 1);
        assert_eq!(result.commits[0].collapse_id, "0000000000000010");
        assert_eq!(
            result.commits[0].summary_content,
            "<collapsed id=\"0000000000000010\">drained</collapsed>"
        );
        assert!(result.snapshot.staged.is_empty());
        assert!(result.snapshot.armed);
        assert_eq!(result.snapshot.last_spawn_tokens, 90_000);
        assert_eq!(result.messages.len(), 1);
    }

    #[test]
    fn reset_clears_commits_staged_and_counter() {
        let store = ContextCollapse::default();
        store.restore_from_entries(
            vec![commit(
                "0000000000000012",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "11111111-1111-4111-8111-111111111111",
                "11111111-1111-4111-8111-111111111111",
                "old",
            )],
            Some(ContextCollapseSnapshot {
                staged: vec![StagedCollapse {
                    start_uuid: "11111111-1111-4111-8111-111111111111".to_string(),
                    end_uuid: "11111111-1111-4111-8111-111111111111".to_string(),
                    summary: "pending".to_string(),
                    risk: 0.0,
                    staged_at: 0,
                }],
                armed: true,
                last_spawn_tokens: 1,
            }),
        );

        assert_eq!(store.reset("compact").reason, "compact");
        assert_eq!(store.stats(), ContextCollapseStats::default());
        assert_eq!(store.snapshot().last_spawn_tokens, 0);
    }
}
