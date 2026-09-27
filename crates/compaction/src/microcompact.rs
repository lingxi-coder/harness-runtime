//! Microcompact — clears stale large tool results without involving the LLM.
//!
//! Two-pass port of TS `microCompact.ts`:
//! - Pass 1 (`collect_compactable_tool_ids`): walk **assistant** messages and
//!   collect, in encounter order, the IDs of every `ToolUse` block whose tool
//!   `name` is in [`compactable_tools`]. Keys on the assistant `tool_use` *name*,
//!   not the user `tool_result` block (TS `collectCompactableToolIds`,
//!   `microCompact.ts:226-241`).
//! - Pass 2 (`compact`): keep the last `max(1, keep_recent)` collected IDs,
//!   clear the rest. Over **user** messages, replace every `ToolResult` whose
//!   `tool_use_id` is in the clear-set (and not already the cleared placeholder)
//!   with [`TIME_BASED_MC_CLEARED_MESSAGE`], accumulating `tokens_saved`.
//!   Already-persisted previews survive. A no-op result is returned below
//!   20,000 candidate tokens (2.1.261 `hfn` / `bVn`).
//!
//! **Time-gap trigger:** TS `evaluateTimeBasedTrigger`
//! (`microCompact.ts:422-444`) computes the gap as `now - lastAssistant.timestamp`
//! and only fires when it exceeds `gapThresholdMinutes`. Conversation messages
//! do not carry timestamps in the Rust protocol, so the session persists the
//! last-assistant time out-of-band and the compaction orchestrator supplies it
//! to [`evaluate_time_based_trigger`]. Missing timing fails safe and does not
//! run microcompact. The lower-level [`MicroCompactor::compact`] remains a pure
//! clear/keep transform and therefore assumes its caller already passed the
//! time gate.

use protocol::{ContentBlock, ConversationMessage};
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

/// Placeholder text substituted for cleared tool results.
pub const TIME_BASED_MC_CLEARED_MESSAGE: &str = "[Old tool result content cleared]";

/// Minimum would-be token savings before a time-based microcompact is allowed
/// to clear anything. Mirrors TS `k5r = 20000` (binary v2.1.183, offset
/// 197329444). `o$i` computes the candidate clear-set + `tokensSaved` via the
/// scan-only `H5r`, then `if(o<k5r)return null;` — i.e. it abandons the whole
/// microcompact (clears NOTHING, leaving the message list untouched) when the
/// total savings would be under 20,000 tokens.
pub const MICROCOMPACT_MIN_TOKENS_SAVED: u64 = 20_000;

/// Set of tool names whose results microcompact is allowed to clear.
///
/// Mirrors TS `COMPACTABLE_TOOLS` (`microCompact.ts:41-50`):
/// `FILE_READ`, the shell tool names, `Grep`, `Glob`, `WebSearch`, `WebFetch`,
/// `FILE_EDIT`, `FILE_WRITE`.
#[must_use]
pub fn compactable_tools() -> HashSet<&'static str> {
    HashSet::from([
        "Read",
        "Bash",
        "PowerShell",
        "Grep",
        "Glob",
        "WebSearch",
        "WebFetch",
        "Edit",
        "Write",
    ])
}

/// Reset microcompact module state after a compaction.
///
/// TS `resetMicrocompactState` (`microCompact.ts:130`) resets the
/// cached-microcompact module state (`cachedMCState`) and clears
/// `pendingCacheEdits`. The Rust microcompact / cached-microcompact layers are
/// stateless (pure functions over passed-in history; see
/// [`crate::cached_microcompact::CachedMicrocompact`], a unit struct), so there
/// is no module-level mutable state to clear — this is a documented no-op kept
/// for call-site symmetry with the TS post-compact cleanup.
pub fn reset_microcompact_state() {
    // ⚠️ CMP-2, 2026-09-14: do NOT conclude from a binary grep that
    // `pendingCacheEdits` was removed upstream. It returns zero hits in
    // 2.1.270 — but so do `cachedMCState`, `microCompact` and
    // `resetMicrocompactState`, which this very comment cites, while the
    // control `cache_creation_input_tokens` returns 21 files. That whole class
    // of identifier is minified away, so absence is not evidence either way,
    // and the TS names above come from source, not from the binary.
    //
    // What IS verifiable at 2.1.270 is the shape of the clear itself, and this
    // port already has it: keep the last `keep_recent` compactable ids, clear
    // the rest, floor at 20k candidate tokens, `tengu_time_based_microcompact`
    // with `toolsCleared`/`toolsKept`/`tokensSaved`. The piece upstream has and
    // this does not is the optional `persist(content, tool_use_id)` the clear
    // calls before substituting its placeholder — see
    // `tool_api::content_replacement`, which carries the same finding from the
    // bookkeeping side.
    // No Rust module-level microcompact state to reset (stateless layer).
}

/// Rough token-count estimate mirroring TS `roughTokenCountEstimation`
/// (`tokenEstimation.ts:203-208`): `Math.round(content.length / 4)`, where the
/// length is measured in JavaScript UTF-16 code units.
///
/// Round-half-up over nonnegative integers is `(len + 2) / 4` in integer math.
#[must_use]
fn rough_token_count_estimation(content: &str) -> u64 {
    crate::post_compact::estimate_content_tokens(content)
}

/// 2.1.261 `BXo`: previews already point at persisted results and must survive
/// subsequent keep-recent passes. Structured content is never a placeholder.
fn already_compacted(content: &str, content_blocks: Option<&[serde_json::Value]>) -> bool {
    content_blocks.is_none()
        && (content == TIME_BASED_MC_CLEARED_MESSAGE || content.starts_with("<persisted-output>"))
}

/// 2.1.261 `FXo`: count the actual wire array, ignoring unrecognized blocks.
fn tool_result_tokens(content: &str, content_blocks: Option<&[serde_json::Value]>) -> u64 {
    content_blocks.map_or_else(
        || rough_token_count_estimation(content),
        |blocks| {
            blocks
                .iter()
                .map(
                    |block| match block.get("type").and_then(serde_json::Value::as_str) {
                        Some("text") => block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map_or(0, rough_token_count_estimation),
                        Some("image" | "document") => 2_000,
                        _ => 0,
                    },
                )
                .sum()
        },
    )
}

/// Configuration controlling time-based microcompact, mirroring TS
/// `TimeBasedMCConfig` (`timeBasedMCConfig.ts:20-34`).
#[derive(Debug, Clone)]
pub struct TimeBasedMCConfig {
    /// Whether time-based microcompact is enabled. **Default `false`** — matches
    /// TS `enabled: false`, so microcompact is a no-op inside the orchestrator
    /// unless explicitly turned on.
    pub enabled: bool,
    /// Idle-gap threshold in minutes; TS fires when the gap since the last
    /// assistant message exceeds this. Default `60`.
    pub gap_threshold_minutes: u64,
    /// How many of the most-recent compactable tool results to always keep.
    /// Floored at 1 at use-site. Default `5`.
    pub keep_recent: usize,
}

impl Default for TimeBasedMCConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gap_threshold_minutes: 60,
            keep_recent: 5,
        }
    }
}

/// Outcome of [`evaluate_time_based_trigger`]: the elapsed gap and the config.
#[derive(Debug, Clone)]
pub struct TimeBasedTrigger {
    /// Gap since the last assistant message, in (fractional) minutes.
    pub gap_minutes: f64,
}

/// Time-gap trigger predicate (TS `evaluateTimeBasedTrigger`,
/// `microCompact.ts:422-444`). Returns `Some` when microcompact should fire.
///
/// Because `protocol::ConversationMessage` has no per-message timestamp (and is
/// frozen), the caller must supply `last_assistant_timestamp` out-of-band.
/// Returns `None` when disabled, when there is no assistant timestamp, or when
/// the gap is below `gap_threshold_minutes`.
#[must_use]
#[allow(clippy::cast_precision_loss)] // gap thresholds are tiny (minutes)
pub fn evaluate_time_based_trigger(
    config: &TimeBasedMCConfig,
    last_assistant_timestamp: Option<SystemTime>,
    now: SystemTime,
) -> Option<TimeBasedTrigger> {
    if !config.enabled {
        return None;
    }
    let last = last_assistant_timestamp?;
    let gap = now.duration_since(last).unwrap_or(Duration::ZERO);
    let gap_minutes = gap.as_secs_f64() / 60.0;
    if !gap_minutes.is_finite() || gap_minutes < config.gap_threshold_minutes as f64 {
        return None;
    }
    Some(TimeBasedTrigger { gap_minutes })
}

/// Walk messages and collect `tool_use` IDs whose tool name is in
/// [`compactable_tools`], in encounter order. Keys on **assistant** `ToolUse`
/// blocks (TS `collectCompactableToolIds`, `microCompact.ts:226-241`).
#[must_use]
pub fn collect_compactable_tool_ids(messages: &[ConversationMessage]) -> Vec<protocol::ToolUseId> {
    let compactable = compactable_tools();
    let mut ids = Vec::new();
    for message in messages {
        if let ConversationMessage::Assistant { content, .. } = message {
            for block in content {
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    if compactable.contains(name.as_str()) {
                        ids.push(id.clone());
                    }
                }
            }
        }
    }
    ids
}

/// The `(tool_use_id, content)` pairs a keep-recent clear would hand to a
/// persist hook — upstream `NTn`'s `candidates`, whose `.content` `Sir` passes
/// to `persist(content, tool_use_id)` before substituting the placeholder.
///
/// Only results with TEXT content are returned: upstream guards with
/// `P.content ? await persist(...) : null`, and a result carrying image or
/// document blocks is never replaced by a file reference (see
/// [`Microcompactor::compact_with_persisted`]). Order follows the messages, so
/// a caller persisting in sequence writes oldest-first.
#[must_use]
pub fn keep_recent_persist_candidates(
    messages: &[ConversationMessage],
    keep_recent: usize,
) -> Vec<(protocol::ToolUseId, String)> {
    let estimate = estimate_keep_recent(messages, keep_recent);
    let mut out = Vec::new();
    for message in messages {
        let ConversationMessage::User { content, .. } = message else {
            continue;
        };
        for block in content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                content_blocks,
                ..
            } = block
            else {
                continue;
            };
            if !estimate.candidate_ids.contains(tool_use_id) {
                continue;
            }
            if content.is_empty() || has_media_blocks(content_blocks.as_deref()) {
                continue;
            }
            out.push((tool_use_id.clone(), content.clone()));
        }
    }
    out
}

/// `content_blocks` of a `ToolResult` block, if it is one.
#[must_use]
fn media_blocks_of(block: &ContentBlock) -> Option<Vec<serde_json::Value>> {
    match block {
        ContentBlock::ToolResult { content_blocks, .. } => content_blocks.clone(),
        _ => None,
    }
}

/// Does this result carry an image or document block? Upstream's `lCt` picks
/// the PLAIN placeholder for those even when a persisted reference exists —
/// the file would hold a JSON blob the model cannot usefully `Read`.
#[must_use]
fn has_media_blocks(blocks: Option<&[serde_json::Value]>) -> bool {
    blocks.is_some_and(|blocks| {
        blocks.iter().any(|b| {
            matches!(
                b.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "document")
            )
        })
    })
}

/// Scan-only estimate of what a keep-recent microcompact WOULD clear.
///
/// 1:1 with claude-code `ARs` (2.1.220 @232865874), which the binary factors
/// out precisely because two callers need it: `qsd` runs it to decide whether a
/// compact is worth doing, and the context-hint controller runs it to decide
/// whether to ASK the server for a hint at all — without mutating anything.
#[derive(Debug, Clone)]
pub struct KeepRecentEstimate {
    /// Tool-use ids whose results would be cleared.
    pub clear_set: HashSet<protocol::ToolUseId>,
    /// Ids with an uncleared result block (`new Set(candidates.map(id))`).
    pub candidate_ids: HashSet<protocol::ToolUseId>,
    /// Tool-use ids whose results would be kept (the most recent N).
    pub keep_set: HashSet<protocol::ToolUseId>,
    /// Number of tool-result blocks that would be cleared.
    pub cleared_count: usize,
    /// Approximate tokens the clear would free (TS `tokensSaved`).
    pub tokens_saved: u64,
}

/// Compute [`KeepRecentEstimate`] without touching `messages`.
///
/// `keep_recent` is floored at 1 (TS `Math.max(1, t)`): keeping 0 would clear
/// EVERY result, leaving the model with no working context.
#[must_use]
pub fn estimate_keep_recent(
    messages: &[ConversationMessage],
    keep_recent: usize,
) -> KeepRecentEstimate {
    let compactable_ids = collect_compactable_tool_ids(messages);
    let keep_count = keep_recent.max(1).min(compactable_ids.len());
    let keep_set: HashSet<protocol::ToolUseId> = compactable_ids
        [compactable_ids.len() - keep_count..]
        .iter()
        .cloned()
        .collect();
    let clear_set: HashSet<protocol::ToolUseId> = compactable_ids
        .iter()
        .filter(|&id| !keep_set.contains(id))
        .cloned()
        .collect();

    let mut cleared_count = 0usize;
    let mut tokens_saved = 0u64;
    let mut candidate_ids = HashSet::new();
    if !clear_set.is_empty() {
        for m in messages {
            if let ConversationMessage::User { content, .. } = m {
                for b in content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        content_blocks,
                        ..
                    } = b
                    {
                        // An already-cleared placeholder contributes nothing
                        // (TS `!Wxd` / `!LH_`).
                        if clear_set.contains(tool_use_id)
                            && !already_compacted(content, content_blocks.as_deref())
                        {
                            cleared_count += 1;
                            candidate_ids.insert(tool_use_id.clone());
                            tokens_saved = tokens_saved.saturating_add(tool_result_tokens(
                                content,
                                content_blocks.as_deref(),
                            ));
                        }
                    }
                }
            }
        }
    }

    KeepRecentEstimate {
        clear_set,
        candidate_ids,
        keep_set,
        cleared_count,
        tokens_saved,
    }
}

/// Stateful microcompactor that owns its configuration.
pub struct Microcompactor {
    /// Active configuration; see [`TimeBasedMCConfig`].
    pub config: TimeBasedMCConfig,
}

/// Result of running microcompact over a message list.
#[derive(Debug, Clone)]
pub struct MicrocompactResult {
    /// Messages with eligible tool results replaced by the cleared placeholder.
    pub messages: Vec<ConversationMessage>,
    /// Number of distinct tool-use ids whose results were cleared.
    pub cleared_count: usize,
    /// Approximate tokens freed by clearing (TS `tokensSaved`).
    pub tokens_saved: u64,
}

impl Microcompactor {
    /// Run microcompact (two-pass, TS `maybeTimeBasedMicrocompact`).
    ///
    /// Pass 1 collects compactable `tool_use` IDs from assistant messages; keeps
    /// the last `max(1, keep_recent)` and clears the rest. Pass 2 replaces the
    /// matching user `ToolResult` blocks with the cleared placeholder.
    ///
    /// Returns a **no-op** result (original messages unchanged, `cleared_count`
    /// and `tokens_saved` both `0`) when the clear-set is empty OR the total
    /// would-be savings are below [`MICROCOMPACT_MIN_TOKENS_SAVED`] (TS `k5r`,
    /// 20,000) — mirroring TS's `null` returns in `o$i`.
    ///
    /// Structure mirrors the binary: a scan-only pass (`H5r`) computes the
    /// candidate clear-set + `tokens_saved` **without mutating** the message
    /// list; if `tokens_saved < 20_000` (or the clear-set is empty) it abandons
    /// the whole microcompact and returns the **original** messages untouched;
    /// only then does the mutation pass (`DOt`) replace the matching tool
    /// results with the cleared placeholder.
    ///
    /// The `_now` argument is retained for signature stability; the count-based
    /// fallback does not consult it (see the module-level time-gap divergence).
    #[must_use]
    pub fn compact(
        &self,
        messages: Vec<ConversationMessage>,
        now: SystemTime,
    ) -> MicrocompactResult {
        self.compact_with_persisted(messages, now, &std::collections::HashMap::new())
    }

    /// [`Self::compact`] with the persist step's results folded in — upstream
    /// `Sir`, whose `v` map is built by awaiting `persist` per candidate and
    /// then handed to the pure `lCt`.
    ///
    /// `persisted` maps a cleared result's id to the reference text that
    /// replaces it. A missing entry, or a result carrying image/document
    /// blocks, falls back to [`TIME_BASED_MC_CLEARED_MESSAGE`] — upstream's
    /// `r?.get(id) ?? Z2e` plus its media guard. A persist FAILURE is therefore
    /// indistinguishable from no persist at all, which is the point: the clear
    /// still happens and the model is still told the content is gone.
    pub fn compact_with_persisted(
        &self,
        messages: Vec<ConversationMessage>,
        _now: SystemTime,
        persisted: &std::collections::HashMap<protocol::ToolUseId, String>,
    ) -> MicrocompactResult {
        // Pass 1 + the scan-only pass, both via `estimate_keep_recent` — the
        // oracle's own factoring (`qsd` calls `ARs`), so the "would this be
        // worth it" question has ONE implementation shared with the
        // context-hint controller instead of two that can drift.
        let KeepRecentEstimate {
            candidate_ids,
            tokens_saved,
            ..
        } = estimate_keep_recent(&messages, self.config.keep_recent);

        if candidate_ids.is_empty() {
            return Self::noop(messages);
        }

        // Floor check (TS `o$i`: `if(o<k5r)return null;`). Abandon the whole
        // microcompact — clearing nothing and returning the ORIGINAL messages —
        // when the total would-be savings are below the 20,000-token floor (this
        // also covers `tokens_saved == 0`, e.g. every match already cleared).
        if tokens_saved < MICROCOMPACT_MIN_TOKENS_SAVED {
            return Self::noop(messages);
        }

        // Mutation pass (TS `DOt`): replace matching tool_result blocks in user
        // messages with the cleared placeholder.
        let out: Vec<ConversationMessage> = messages
            .into_iter()
            .map(|m| {
                if let ConversationMessage::User {
                    id,
                    content,
                    is_meta,
                    is_compact_summary,
                    is_visible_in_transcript_only,
                } = m
                {
                    let new_content: Vec<ContentBlock> = content
                        .into_iter()
                        .map(|b| {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                is_error,
                                provider_tool_use_id,
                                ..
                            } = &b
                            {
                                if candidate_ids.contains(tool_use_id) {
                                    // `lCt`: a media-carrying result always
                                    // takes the plain placeholder, even when a
                                    // reference exists for it.
                                    let replacement =
                                        if has_media_blocks(media_blocks_of(&b).as_deref()) {
                                            TIME_BASED_MC_CLEARED_MESSAGE.to_string()
                                        } else {
                                            persisted.get(tool_use_id).cloned().unwrap_or_else(
                                                || TIME_BASED_MC_CLEARED_MESSAGE.to_string(),
                                            )
                                        };
                                    return ContentBlock::ToolResult {
                                        tool_use_id: tool_use_id.clone(),
                                        content: replacement,
                                        is_error: *is_error,
                                        // Preserve the provider id through content clearing.
                                        provider_tool_use_id: provider_tool_use_id.clone(),
                                        content_blocks: None,
                                    };
                                }
                            }
                            b
                        })
                        .collect();
                    ConversationMessage::User {
                        id,
                        content: new_content,
                        is_meta,
                        is_compact_summary,
                        is_visible_in_transcript_only,
                    }
                } else {
                    m
                }
            })
            .collect();

        MicrocompactResult {
            messages: out,
            cleared_count: candidate_ids.len(),
            tokens_saved,
        }
    }

    /// Build the no-op result: messages unchanged, nothing cleared.
    fn noop(messages: Vec<ConversationMessage>) -> MicrocompactResult {
        MicrocompactResult {
            messages,
            cleared_count: 0,
            tokens_saved: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{MessageId, ToolUseId};
    use serde_json::json;

    fn assistant_tool_use(name: &str, id: ToolUseId) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id,
                name: name.into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    fn user_tool_result(tool_use_id: ToolUseId, content: &str) -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id,
                content: content.into(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn result_content(msg: &ConversationMessage) -> &str {
        match msg {
            ConversationMessage::User { content, .. } => match &content[0] {
                ContentBlock::ToolResult { content, .. } => content.as_str(),
                _ => panic!("expected tool_result"),
            },
            _ => panic!("expected user message"),
        }
    }

    // Claude Code 2.1.261 hfn / BXo / FXo: persisted previews are already
    // compacted; nested wire content takes precedence over the display text.
    #[test]
    fn oracle_261_persisted_results_are_never_clear_candidates() {
        let persisted = ToolUseId::new();
        let large = ToolUseId::new();
        let recent = ToolUseId::new();
        let messages = vec![
            assistant_tool_use("Read", persisted.clone()),
            user_tool_result(
                persisted,
                &format!("<persisted-output>{}", "x".repeat(80_000)),
            ),
            assistant_tool_use("Read", large.clone()),
            user_tool_result(large, &"x".repeat(80_000)),
            assistant_tool_use("Read", recent.clone()),
            user_tool_result(recent, "recent"),
        ];
        let result = Microcompactor {
            config: TimeBasedMCConfig {
                keep_recent: 1,
                ..Default::default()
            },
        }
        .compact(messages.clone(), SystemTime::UNIX_EPOCH);
        assert_eq!(result.tokens_saved, 20_000);
        assert_eq!(result.cleared_count, 1);
        assert_eq!(result.messages[1], messages[1]);
        assert_eq!(
            result_content(&result.messages[3]),
            TIME_BASED_MC_CLEARED_MESSAGE
        );
    }

    #[test]
    fn oracle_261_microcompact_counts_utf16_and_nested_media() {
        let old = ToolUseId::new();
        let recent = ToolUseId::new();
        let mut nested = user_tool_result(old.clone(), "display text must not be counted");
        if let ConversationMessage::User { content, .. } = &mut nested {
            if let ContentBlock::ToolResult { content_blocks, .. } = &mut content[0] {
                *content_blocks = Some(vec![
                    json!({"type":"text", "text":"你好😀"}),
                    json!({"type":"image", "source":{}}),
                    json!({"type":"document", "source":{}}),
                    json!({"type":"unknown", "text":"x".repeat(80_000)}),
                ]);
            }
        }
        let messages = vec![
            assistant_tool_use("Read", old),
            nested,
            assistant_tool_use("Read", recent),
        ];
        let estimate = estimate_keep_recent(&messages, 1);
        assert_eq!(estimate.tokens_saved, 4_001);
        assert_eq!(rough_token_count_estimation("你好"), 1);
        assert_eq!(rough_token_count_estimation("😀"), 1);
    }

    /// (a) 8 compactable `tool_uses`, `keep_recent=5` → 3 oldest cleared, last 5
    /// kept — provided the 3 cleared results clear at least
    /// [`MICROCOMPACT_MIN_TOKENS_SAVED`] (20,000) tokens in total.
    #[test]
    fn keeps_last_five_clears_three_oldest() {
        let mut msgs = Vec::new();
        let mut ids = Vec::new();
        for i in 0..8 {
            let id = ToolUseId::new();
            ids.push(id.clone());
            msgs.push(assistant_tool_use("Read", id.clone()));
            // Large content (~10k tokens each) so the 3 oldest cleared together
            // clear well above the 20,000-token floor and the compact fires.
            msgs.push(user_tool_result(
                id,
                &format!("body-{i} {}", "x".repeat(40_000)),
            ));
        }
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 5,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        assert_eq!(r.cleared_count, 3);
        assert!(r.tokens_saved > 0);
        // The 3 oldest (indices 0,1,2) cleared; the last 5 (3..8) kept.
        let results: Vec<&ConversationMessage> = r
            .messages
            .iter()
            .filter(|m| matches!(m, ConversationMessage::User { .. }))
            .collect();
        for (i, res) in results.iter().enumerate() {
            if i < 3 {
                assert_eq!(
                    result_content(res),
                    TIME_BASED_MC_CLEARED_MESSAGE,
                    "msg {i}"
                );
            } else {
                assert_ne!(
                    result_content(res),
                    TIME_BASED_MC_CLEARED_MESSAGE,
                    "msg {i}"
                );
            }
        }
    }

    /// 20,000-token floor (TS `k5r`): a clear-set exists and would save tokens,
    /// but the total is below 20,000 → the whole microcompact is abandoned and
    /// the ORIGINAL (un-mutated) messages are returned, byte-for-byte.
    #[test]
    fn below_min_tokens_saved_floor_is_noop() {
        let mut msgs = Vec::new();
        let mut ids = Vec::new();
        // 8 results; with keep_recent=5 the 3 oldest form the clear-set. Each
        // body ~12 tokens → ~36 tokens would-be saved, far below 20,000.
        for i in 0..8 {
            let id = ToolUseId::new();
            ids.push(id.clone());
            msgs.push(assistant_tool_use("Read", id.clone()));
            msgs.push(user_tool_result(
                id,
                &format!("body-{i} {}", "x".repeat(40)),
            ));
        }
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 5,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        // Under the floor → no-op: nothing cleared, no tokens saved.
        assert_eq!(r.cleared_count, 0);
        assert_eq!(r.tokens_saved, 0);
        // The returned messages must be the ORIGINAL ones — NOT mutated to the
        // placeholder (faithful to the binary's compute-then-check ordering).
        for m in &r.messages {
            if matches!(m, ConversationMessage::User { .. }) {
                assert_ne!(result_content(m), TIME_BASED_MC_CLEARED_MESSAGE);
            }
        }
    }

    /// At exactly the 20,000-token floor the compact fires (`o<k5r` is strict
    /// `<`, so `== 20_000` is NOT below the floor).
    #[test]
    fn at_min_tokens_saved_floor_fires() {
        // One assistant Read (cleared) + one keeper, so keep_recent=1 leaves a
        // single clear candidate. rough_token_count_estimation = (len+2)/4, so
        // for tokens_saved == 20_000 we need len == 79_998 (79_998+2)/4 = 20_000.
        let mut msgs = Vec::new();
        let clear_id = ToolUseId::new();
        let keep_id = ToolUseId::new();
        msgs.push(assistant_tool_use("Read", clear_id.clone()));
        msgs.push(assistant_tool_use("Read", keep_id.clone()));
        msgs.push(user_tool_result(clear_id, &"x".repeat(79_998)));
        msgs.push(user_tool_result(keep_id, "kept"));
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 1,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        assert_eq!(r.tokens_saved, 20_000);
        assert_eq!(r.cleared_count, 1);
    }

    /// (b) already-cleared blocks are not re-counted (no tokens saved → no-op).
    #[test]
    fn already_cleared_not_recounted() {
        let mut msgs = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..8 {
            let id = ToolUseId::new();
            ids.push(id.clone());
            msgs.push(assistant_tool_use("Read", id.clone()));
        }
        // The 3 oldest results are ALREADY the cleared placeholder.
        for (i, id) in ids.iter().enumerate() {
            let content = if i < 3 {
                TIME_BASED_MC_CLEARED_MESSAGE.to_string()
            } else {
                format!("fresh-{i} {}", "y".repeat(40))
            };
            msgs.push(user_tool_result(id.clone(), &content));
        }
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 5,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        // The clear-set is exactly the 3 oldest, which are already cleared →
        // nothing to do → tokens_saved == 0 → no-op.
        assert_eq!(r.cleared_count, 0);
        assert_eq!(r.tokens_saved, 0);
    }

    /// (c) a non-compactable tool (`TodoWrite`) is never cleared.
    #[test]
    fn non_compactable_tool_never_cleared() {
        let mut msgs = Vec::new();
        let mut ids = Vec::new();
        for i in 0..8 {
            let id = ToolUseId::new();
            ids.push(id.clone());
            msgs.push(assistant_tool_use("TodoWrite", id.clone()));
            msgs.push(user_tool_result(
                id,
                &format!("todo-{i} {}", "z".repeat(40)),
            ));
        }
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 5,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        // No compactable IDs → empty clear-set → no-op, nothing touched.
        assert_eq!(r.cleared_count, 0);
        assert_eq!(r.tokens_saved, 0);
        for m in &r.messages {
            if matches!(m, ConversationMessage::User { .. }) {
                assert_ne!(result_content(m), TIME_BASED_MC_CLEARED_MESSAGE);
            }
        }
    }

    /// (d) `tokens_saved == 0` → no-op. Compactable IDs exist and form a
    /// clear-set, but the matching results have empty content (0 tokens).
    #[test]
    fn zero_tokens_saved_is_noop() {
        let mut msgs = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..8 {
            let id = ToolUseId::new();
            ids.push(id.clone());
            msgs.push(assistant_tool_use("Read", id.clone()));
        }
        // All results empty → rough_token_count_estimation("") rounds to 0 only
        // for very short strings; use truly empty strings → tokens_saved stays 0.
        for id in &ids {
            msgs.push(user_tool_result(id.clone(), ""));
        }
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 5,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        assert_eq!(r.tokens_saved, 0);
        assert_eq!(r.cleared_count, 0);
        // No-op surfaces the (here, cleared but zero-token) messages; assert the
        // result is reported as a no-op regardless of in-place edits.
    }

    /// `keep_recent` floored at 1: `keep_recent=0` keeps the single most recent.
    #[test]
    fn keep_recent_floored_at_one() {
        let mut msgs = Vec::new();
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = ToolUseId::new();
            ids.push(id.clone());
            msgs.push(assistant_tool_use("Bash", id.clone()));
            // Large bodies so the 2 cleared exceed the 20,000-token floor.
            msgs.push(user_tool_result(
                id,
                &format!("out-{i} {}", "q".repeat(50_000)),
            ));
        }
        let mc = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 0,
                ..Default::default()
            },
        };
        let r = mc.compact(msgs, SystemTime::now());
        // keep last 1 → clear first 2.
        assert_eq!(r.cleared_count, 2);
    }

    /// `evaluate_time_based_trigger`: disabled config never fires.
    #[test]
    fn trigger_disabled_returns_none() {
        let cfg = TimeBasedMCConfig::default(); // enabled = false
        let now = SystemTime::now();
        let long_ago = now - Duration::from_secs(10 * 60 * 60);
        assert!(evaluate_time_based_trigger(&cfg, Some(long_ago), now).is_none());
    }

    /// `evaluate_time_based_trigger`: enabled + gap over threshold fires.
    #[test]
    fn trigger_enabled_over_threshold_fires() {
        let cfg = TimeBasedMCConfig {
            enabled: true,
            gap_threshold_minutes: 60,
            keep_recent: 5,
        };
        let now = SystemTime::now();
        let two_hours_ago = now - Duration::from_secs(2 * 60 * 60);
        let t = evaluate_time_based_trigger(&cfg, Some(two_hours_ago), now).expect("should fire");
        assert!(t.gap_minutes >= 60.0);
        // Below threshold does not fire.
        let recent = now - Duration::from_secs(5 * 60);
        assert!(evaluate_time_based_trigger(&cfg, Some(recent), now).is_none());
        // No timestamp does not fire.
        assert!(evaluate_time_based_trigger(&cfg, None, now).is_none());
    }

    // ---- CMP-2: the keep-recent clear's persist hook ----------------------

    #[test]
    fn candidates_carry_the_content_a_persist_hook_needs() {
        // Six compactable results, keep_recent = 2 ⇒ the four oldest are
        // candidates, and each must come back WITH its body: upstream's
        // `persist(P.content, P.tool_use_id)` has nothing to write otherwise.
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(assistant_tool_use("Bash", ToolUseId::from(format!("t{i}"))));
            msgs.push(user_tool_result(
                ToolUseId::from(format!("t{i}")),
                &format!("body-{i}"),
            ));
        }
        let got = keep_recent_persist_candidates(&msgs, 2);
        let ids: Vec<&str> = got.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["t0", "t1", "t2", "t3"]);
        assert_eq!(got[0].1, "body-0");
        assert_eq!(got[3].1, "body-3");
    }

    #[test]
    fn a_persisted_reference_replaces_the_bare_placeholder() {
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(assistant_tool_use("Bash", ToolUseId::from(format!("t{i}"))));
            msgs.push(user_tool_result(
                ToolUseId::from(format!("t{i}")),
                &"x".repeat(30_000),
            ));
        }
        let mut persisted = std::collections::HashMap::new();
        persisted.insert(
            ToolUseId::from("t0"),
            "<persisted-output>Tool result saved to: /t/t0.txt\n\nUse Read to view</persisted-output>"
                .to_string(),
        );
        let compactor = Microcompactor {
            config: TimeBasedMCConfig {
                enabled: true,
                keep_recent: 2,
                ..TimeBasedMCConfig::default()
            },
        };
        let out = compactor.compact_with_persisted(msgs, SystemTime::now(), &persisted);
        let cleared: Vec<String> = out
            .messages
            .iter()
            .filter_map(|m| match m {
                ConversationMessage::User { content, .. } => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id.as_str() == "t0" || tool_use_id.as_str() == "t1" => {
                    Some(content.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            cleared[0].starts_with("<persisted-output>Tool result saved to: /t/t0.txt"),
            "the id WITH a reference must get it: {:?}",
            cleared[0]
        );
        assert_eq!(
            cleared[1], TIME_BASED_MC_CLEARED_MESSAGE,
            "an id with no reference falls back to the bare placeholder — a \
             persist failure must still clear"
        );
    }

    #[test]
    fn a_media_carrying_result_is_never_given_a_file_reference() {
        // `lCt`'s guard: the file would hold a JSON blob the model cannot
        // usefully Read, so those always take the plain placeholder even when
        // a reference exists for them.
        let blocks = vec![serde_json::json!({"type": "image"})];
        assert!(has_media_blocks(Some(&blocks)));
        assert!(!has_media_blocks(Some(&[
            serde_json::json!({"type": "text"})
        ])));
        assert!(!has_media_blocks(None));

        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(assistant_tool_use("Bash", ToolUseId::from(format!("t{i}"))));
            let mut block = user_tool_result(ToolUseId::from(format!("t{i}")), &"x".repeat(30_000));
            if i == 0 {
                if let ConversationMessage::User { content, .. } = &mut block {
                    if let Some(ContentBlock::ToolResult { content_blocks, .. }) =
                        content.first_mut()
                    {
                        *content_blocks = Some(blocks.clone());
                    }
                }
            }
            msgs.push(block);
        }
        // …and it is not even offered to the persist hook.
        let candidates = keep_recent_persist_candidates(&msgs, 2);
        let ids: Vec<&str> = candidates.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2", "t3"], "t0 carries an image");
    }
}
