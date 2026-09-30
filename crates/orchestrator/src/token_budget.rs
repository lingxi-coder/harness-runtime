//! A3: token-budget auto-continuation (the `+500k` feature).
//!
//! Port of claude-code `src/query/tokenBudget.ts` + `src/utils/tokenBudget.ts`.
//!
//! When a user requests a token budget (e.g. `"+500k"`, `"use 2M tokens"`)
//! the turn loop keeps nudging the model to continue until ~90% of the
//! budget is spent or the per-continuation token delta shows diminishing
//! returns, instead of stopping at the first `end_turn`.
//!
//! This module ports four things 1:1:
//! - [`BudgetTracker`] + [`check_token_budget`] — the decision state machine
//!   (`COMPLETION_THRESHOLD = 0.9`, `DIMINISHING_THRESHOLD = 500`).
//! - [`budget_continuation_message`] — the byte-exact nudge string injected
//!   into history on a `continue` decision (note the U+2014 em-dash; numbers
//!   are grouped with en-US thousands separators).
//! - [`parse_token_budget`] — detect a budget request in a prompt.
//! - [`find_token_budget_positions`] — the char offsets of every budget
//!   token in a prompt (for highlighting / stripping).
//!
//! ## Regex parity (hand-written scanners)
//!
//! The `regex` crate is NOT a dependency of `orchestrator`, so the three TS
//! patterns are hand-written byte-faithful scanners that replicate the
//! `\s`-capture + index-offset behaviour of `findTokenBudgetPositions`:
//!
//! - `SHORTHAND_START_RE = /^\s*\+(\d+(?:\.\d+)?)\s*(k|m|b)\b/i`
//! - `SHORTHAND_END_RE   = /\s\+(\d+(?:\.\d+)?)\s*(k|m|b)\s*[.!?]?\s*$/i`
//! - `VERBOSE_RE         = /\b(?:use|spend)\s+(\d+(?:\.\d+)?)\s*(k|m|b)\s*tokens?\b/i`
//!
//! TS regexes index by UTF-16 code unit; all budget syntax is ASCII, so the
//! scanners operate over bytes (== chars == UTF-16 units for ASCII) and the
//! reported positions byte-match `match.index`.

/// Continue once the per-turn output tokens reach this fraction of budget.
/// 1:1 with TS `COMPLETION_THRESHOLD = 0.9`.
const COMPLETION_THRESHOLD: f64 = 0.9;

/// Below this per-continuation delta (after 3 continuations) the loop treats
/// further work as diminishing returns and stops. 1:1 with TS
/// `DIMINISHING_THRESHOLD = 500`.
const DIMINISHING_THRESHOLD: u64 = 500;

/// Per-conversation budget bookkeeping. 1:1 with the TS `BudgetTracker` type.
///
/// `started_at` records wall-clock at creation so the completion telemetry can
/// report `duration_ms`; it has no effect on the continue/stop decision.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    /// How many continuation nudges have been injected this conversation.
    pub continuation_count: u32,
    /// The output-token delta observed at the *previous* check.
    pub last_delta_tokens: u64,
    /// The cumulative `global_turn_tokens` observed at the previous check.
    pub last_global_turn_tokens: u64,
    /// Wall-clock at tracker creation (for `duration_ms` telemetry only).
    pub started_at: std::time::Instant,
}

impl BudgetTracker {
    /// Construct a fresh tracker (1:1 with TS `createBudgetTracker()`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            continuation_count: 0,
            last_delta_tokens: 0,
            last_global_turn_tokens: 0,
            started_at: std::time::Instant::now(),
        }
    }
}

impl Default for BudgetTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// The `tengu_token_budget_completed` telemetry payload (1:1 with the TS
/// `completionEvent` object on a `stop` decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionEvent {
    /// How many continuations happened before stopping.
    pub continuation_count: u32,
    /// Percent of budget consumed at stop (`Math.round`).
    pub pct: i64,
    /// Cumulative per-turn output tokens at stop.
    pub turn_tokens: u64,
    /// The active budget.
    pub budget: u64,
    /// Whether the stop was triggered by diminishing returns.
    pub diminishing_returns: bool,
    /// Wall-clock from tracker creation to stop, in milliseconds.
    pub duration_ms: u128,
}

/// The decision returned by [`check_token_budget`]. 1:1 with the TS
/// `TokenBudgetDecision` discriminated union (`ContinueDecision | StopDecision`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenBudgetDecision {
    /// Inject `nudge_message` and loop again.
    Continue {
        /// The byte-exact continuation nudge to append as a meta user message.
        nudge_message: String,
        /// The (post-increment) continuation count.
        continuation_count: u32,
        /// Percent of budget consumed (`Math.round`).
        pct: i64,
        /// Cumulative per-turn output tokens.
        turn_tokens: u64,
        /// The active budget.
        budget: u64,
    },
    /// Stop the loop. `completion_event` is `Some` only when at least one
    /// continuation happened (so there is something to report); `None` on the
    /// trivial "no budget / first `end_turn` under threshold with zero
    /// continuations" stop.
    Stop {
        /// The `tengu_token_budget_completed` telemetry payload, if any.
        completion_event: Option<CompletionEvent>,
    },
}

/// Round to nearest integer, half away from zero — matches JS `Math.round`
/// for the non-negative percentages this function produces (`Math.round(x)`
/// for `x >= 0` is `floor(x + 0.5)`).
#[allow(clippy::cast_possible_truncation)]
fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// Decide whether the turn loop should continue nudging the model or stop.
///
/// 1:1 with TS `checkTokenBudget(tracker, agentId, budget, globalTurnTokens)`.
///
/// - A set `agent_id` (sub-agent context), a `None`/non-positive budget → an
///   immediate `Stop { completion_event: None }`.
/// - Under [`COMPLETION_THRESHOLD`] and not diminishing → `Continue` (and the
///   tracker's `continuation_count` / delta bookkeeping is advanced, exactly
///   as the TS mutates the tracker in place).
/// - Otherwise `Stop`, with a `completion_event` when diminishing OR at least
///   one continuation has already happened.
#[must_use]
pub fn check_token_budget(
    tracker: &mut BudgetTracker,
    agent_id: Option<&str>,
    budget: Option<u64>,
    global_turn_tokens: u64,
) -> TokenBudgetDecision {
    // `if (agentId || budget === null || budget <= 0)`. `budget <= 0` collapses
    // into `budget == 0` for the unsigned port (a 0 budget is non-positive).
    let budget = match budget {
        Some(b) if b > 0 => b,
        _ => {
            return TokenBudgetDecision::Stop {
                completion_event: None,
            }
        }
    };
    if agent_id.is_some() {
        return TokenBudgetDecision::Stop {
            completion_event: None,
        };
    }

    let turn_tokens = global_turn_tokens;
    #[allow(clippy::cast_precision_loss)]
    let pct = js_round((turn_tokens as f64 / budget as f64) * 100.0);
    // TS: `globalTurnTokens - tracker.lastGlobalTurnTokens`. The cumulative
    // count is monotonic, so this never underflows in practice; saturate to be
    // safe (a non-monotonic caller would otherwise panic in debug).
    let delta_since_last_check = global_turn_tokens.saturating_sub(tracker.last_global_turn_tokens);

    let is_diminishing = tracker.continuation_count >= 3
        && delta_since_last_check < DIMINISHING_THRESHOLD
        && tracker.last_delta_tokens < DIMINISHING_THRESHOLD;

    // `!isDiminishing && turnTokens < budget * COMPLETION_THRESHOLD`
    #[allow(clippy::cast_precision_loss)]
    let under_threshold = (turn_tokens as f64) < (budget as f64) * COMPLETION_THRESHOLD;
    if !is_diminishing && under_threshold {
        tracker.continuation_count += 1;
        tracker.last_delta_tokens = delta_since_last_check;
        tracker.last_global_turn_tokens = global_turn_tokens;
        return TokenBudgetDecision::Continue {
            nudge_message: budget_continuation_message(pct, turn_tokens, budget),
            continuation_count: tracker.continuation_count,
            pct,
            turn_tokens,
            budget,
        };
    }

    if is_diminishing || tracker.continuation_count > 0 {
        return TokenBudgetDecision::Stop {
            completion_event: Some(CompletionEvent {
                continuation_count: tracker.continuation_count,
                pct,
                turn_tokens,
                budget,
                diminishing_returns: is_diminishing,
                duration_ms: tracker.started_at.elapsed().as_millis(),
            }),
        };
    }

    TokenBudgetDecision::Stop {
        completion_event: None,
    }
}

/// The byte-exact continuation nudge injected as a meta user message.
///
/// 1:1 with TS `getBudgetContinuationMessage(pct, turnTokens, budget)`:
/// `` `Stopped at ${pct}% of token target (${fmt(turnTokens)} / ${fmt(budget)}). Keep working — do not summarize.` ``
/// where `fmt` is `Intl.NumberFormat('en-US')` (comma thousands separators).
/// The `—` is a literal U+2014 EM DASH.
#[must_use]
pub fn budget_continuation_message(pct: i64, turn_tokens: u64, budget: u64) -> String {
    format!(
        "Stopped at {}% of token target ({} / {}). Keep working \u{2014} do not summarize.",
        pct,
        group_en_us(turn_tokens),
        group_en_us(budget),
    )
}

/// Format an unsigned integer with en-US thousands separators (commas every 3
/// digits, right-grouped). Matches `Intl.NumberFormat('en-US').format(n)` for
/// non-negative integers. Hand-written — no num-format crate (scope lock).
fn group_en_us(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let len = bytes.len();
    // Pre-size: digits + one comma per group boundary.
    let commas = (len.saturating_sub(1)) / 3;
    let mut out = String::with_capacity(len + commas);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        // `b` is an ASCII digit byte from `u64::to_string`.
        out.push(*b as char);
    }
    out
}

// ---------------------------------------------------------------------------
// parse_token_budget / find_token_budget_positions (hand-written scanners)
// ---------------------------------------------------------------------------

/// Multiplier for a `k`/`m`/`b` suffix (case-insensitive). 1:1 with TS
/// `MULTIPLIERS`.
fn multiplier(suffix: u8) -> Option<f64> {
    match suffix.to_ascii_lowercase() {
        b'k' => Some(1_000.0),
        b'm' => Some(1_000_000.0),
        b'b' => Some(1_000_000_000.0),
        _ => None,
    }
}

/// `parseFloat(value) * MULTIPLIERS[suffix]`, rounded to a `u64` budget.
fn parse_budget_match(value: &str, suffix: u8) -> Option<u64> {
    let v: f64 = value.parse().ok()?;
    let m = multiplier(suffix)?;
    // TS yields a JS number (float); downstream `budget` is compared as a
    // number. The Rust port stores budgets as `u64`; `2.5m` → `2_500_000`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((v * m) as u64)
}

/// Whether `b` is an ASCII whitespace byte for the purposes of JS `\s`.
///
/// JS `\s` matches space, `\t`, `\n`, `\r`, `\f`, `\v`, plus several Unicode
/// spaces. For the ASCII budget syntax the relevant set is the ASCII
/// whitespace class; we replicate exactly that (`[ \t\n\r\x0b\x0c]`).
fn is_js_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// A matched budget span over a byte slice: the captured number, the unit
/// byte, and the `[start, end)` byte offsets of the WHOLE match.
struct ShorthandMatch {
    value: String,
    suffix: u8,
    /// Offset of the whole match (`match.index`).
    match_start: usize,
    /// Offset just past the whole match (`match.index + match[0].length`).
    match_end: usize,
}

/// Scan a number `(\d+(?:\.\d+)?)` at `bytes[pos..]`. Returns the decimal
/// string and the index just past it, or `None` if no leading digit.
fn scan_number(bytes: &[u8], pos: usize) -> Option<(String, usize)> {
    let mut i = pos;
    let start = pos;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None; // need at least one digit
    }
    // Optional `(?:\.\d+)` — a dot FOLLOWED BY at least one digit.
    if i < bytes.len() && bytes[i] == b'.' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > i + 1 {
            i = j;
        }
    }
    Some((String::from_utf8_lossy(&bytes[start..i]).into_owned(), i))
}

/// Skip a run of `\s*` from `pos`. Returns the new index.
fn skip_spaces(bytes: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < bytes.len() && is_js_space(bytes[i]) {
        i += 1;
    }
    i
}

/// Is the byte a `k`/`m`/`b` unit (case-insensitive)?
fn is_unit(b: u8) -> bool {
    matches!(b.to_ascii_lowercase(), b'k' | b'm' | b'b')
}

/// `\b` after a unit byte: the boundary holds when the next char is NOT a
/// word char (`[A-Za-z0-9_]`). End-of-string is a boundary.
fn word_boundary_after(bytes: &[u8], pos: usize) -> bool {
    if pos >= bytes.len() {
        return true;
    }
    !is_word_char(bytes[pos])
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `SHORTHAND_START_RE = /^\s*\+(\d+(?:\.\d+)?)\s*(k|m|b)\b/i`.
/// Anchored at byte 0. The whole match (`match[0]`) includes the leading
/// `\s*` and the `+`, the number, the inner `\s*`, and the unit byte.
fn match_shorthand_start(bytes: &[u8]) -> Option<ShorthandMatch> {
    // `^\s*` — leading whitespace; the match begins at index 0.
    let after_ws = skip_spaces(bytes, 0);
    // `\+`
    if after_ws >= bytes.len() || bytes[after_ws] != b'+' {
        return None;
    }
    let num_start = after_ws + 1;
    let (value, after_num) = scan_number(bytes, num_start)?;
    // `\s*`
    let after_inner_ws = skip_spaces(bytes, after_num);
    // `(k|m|b)`
    if after_inner_ws >= bytes.len() || !is_unit(bytes[after_inner_ws]) {
        return None;
    }
    let suffix = bytes[after_inner_ws];
    let unit_end = after_inner_ws + 1;
    // `\b`
    if !word_boundary_after(bytes, unit_end) {
        return None;
    }
    Some(ShorthandMatch {
        value,
        suffix,
        match_start: 0, // `^` anchors match[0] at index 0 (incl. leading \s*)
        match_end: unit_end,
    })
}

/// `SHORTHAND_END_RE = /\s\+(\d+(?:\.\d+)?)\s*(k|m|b)\s*[.!?]?\s*$/i`.
/// The whole match starts at the single leading `\s` and runs to end-of-string
/// (`$`), including any trailing punctuation / whitespace.
///
/// Scans candidate start positions left-to-right (mirroring the JS engine's
/// leftmost-match semantics): the first leading-`\s` position whose tail
/// reaches `$` wins.
fn match_shorthand_end(bytes: &[u8]) -> Option<ShorthandMatch> {
    let len = bytes.len();
    for start in 0..len {
        // `\s` — exactly one whitespace byte.
        if !is_js_space(bytes[start]) {
            continue;
        }
        let mut i = start + 1;
        // `\+`
        if i >= len || bytes[i] != b'+' {
            continue;
        }
        i += 1;
        // `(\d+(?:\.\d+)?)`
        let Some((value, after_num)) = scan_number(bytes, i) else {
            continue;
        };
        i = after_num;
        // `\s*`
        i = skip_spaces(bytes, i);
        // `(k|m|b)`
        if i >= len || !is_unit(bytes[i]) {
            continue;
        }
        let suffix = bytes[i];
        i += 1;
        // `\s*`
        i = skip_spaces(bytes, i);
        // `[.!?]?`
        if i < len && matches!(bytes[i], b'.' | b'!' | b'?') {
            i += 1;
        }
        // `\s*`
        i = skip_spaces(bytes, i);
        // `$` — end of string.
        if i == len {
            return Some(ShorthandMatch {
                value,
                suffix,
                match_start: start,
                match_end: len, // `$` => match[0] runs to end-of-string
            });
        }
        // Otherwise this leading-\s start can't reach `$`; the leftmost-match
        // engine would still try the same start with the greedy `\s*` having
        // consumed differently, but for this grammar a failure here means no
        // match anchored at `start`; continue scanning further starts.
    }
    None
}

/// One VERBOSE match: number, unit, and `[start, end)` of the whole match.
struct VerboseMatch {
    value: String,
    suffix: u8,
    match_start: usize,
    match_end: usize,
}

/// `VERBOSE_RE = /\b(?:use|spend)\s+(\d+(?:\.\d+)?)\s*(k|m|b)\s*tokens?\b/i`.
/// Returns the leftmost match at or after `from`, or `None`.
fn match_verbose_at(bytes: &[u8], from: usize) -> Option<VerboseMatch> {
    let len = bytes.len();
    let mut start = from;
    while start < len {
        // `\b` before the keyword: position `start` is a word boundary iff the
        // char at `start` is a word char and the char before is not (or BOS).
        let here_word = is_word_char(bytes[start]);
        let prev_word = start > 0 && is_word_char(bytes[start - 1]);
        if !here_word || prev_word {
            start += 1;
            continue;
        }
        // `(?:use|spend)` (case-insensitive).
        let kw_len = if matches_ci(bytes, start, b"use") {
            3
        } else if matches_ci(bytes, start, b"spend") {
            5
        } else {
            start += 1;
            continue;
        };
        let mut i = start + kw_len;
        // `\s+` — one or more whitespace.
        let ws_start = i;
        i = skip_spaces(bytes, i);
        if i == ws_start {
            start += 1;
            continue;
        }
        // `(\d+(?:\.\d+)?)`
        let Some((value, after_num)) = scan_number(bytes, i) else {
            start += 1;
            continue;
        };
        i = after_num;
        // `\s*`
        i = skip_spaces(bytes, i);
        // `(k|m|b)`
        if i >= len || !is_unit(bytes[i]) {
            start += 1;
            continue;
        }
        let suffix = bytes[i];
        i += 1;
        // `\s*`
        i = skip_spaces(bytes, i);
        // `tokens?` — `token` optionally followed by `s`.
        if !matches_ci(bytes, i, b"token") {
            start += 1;
            continue;
        }
        i += 5;
        if i < len && (bytes[i] == b's' || bytes[i] == b'S') {
            i += 1;
        }
        // `\b` after `tokens?`.
        if !word_boundary_after(bytes, i) {
            start += 1;
            continue;
        }
        return Some(VerboseMatch {
            value,
            suffix,
            match_start: start,
            match_end: i,
        });
    }
    None
}

/// Case-insensitive ASCII compare of `needle` against `bytes[pos..]`.
fn matches_ci(bytes: &[u8], pos: usize, needle: &[u8]) -> bool {
    if pos + needle.len() > bytes.len() {
        return false;
    }
    bytes[pos..pos + needle.len()]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Parse a token budget out of a prompt, trying (in order) the start-anchored
/// shorthand, the end-anchored shorthand, then the verbose form. 1:1 with TS
/// `parseTokenBudget`. Returns `None` if no budget is requested.
#[must_use]
pub fn parse_token_budget(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if let Some(m) = match_shorthand_start(bytes) {
        return parse_budget_match(&m.value, m.suffix);
    }
    if let Some(m) = match_shorthand_end(bytes) {
        return parse_budget_match(&m.value, m.suffix);
    }
    if let Some(m) = match_verbose_at(bytes, 0) {
        return parse_budget_match(&m.value, m.suffix);
    }
    None
}

/// A `[start, end)` byte span of a budget token within a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPosition {
    /// Inclusive byte offset of the span start.
    pub start: usize,
    /// Exclusive byte offset of the span end.
    pub end: usize,
}

/// Find the byte spans of every budget token in `text`. 1:1 with TS
/// `findTokenBudgetPositions`, including the `\s`-capture offset arithmetic.
///
/// - The start-shorthand position offsets `match.index` (== 0) by the length of
///   the trimmed-leading whitespace, so `start` lands on the `+`.
/// - The end-shorthand position is `match.index + 1` (the regex captures one
///   leading `\s`), skipped if already covered by the start span.
/// - Every verbose match is appended with its raw `[index, index+len)`.
#[must_use]
pub fn find_token_budget_positions(text: &str) -> Vec<BudgetPosition> {
    let bytes = text.as_bytes();
    let mut positions: Vec<BudgetPosition> = Vec::new();

    // SHORTHAND_START: offset = index + match[0].len - trimStart(match[0]).len.
    // index is 0; match[0] spans [0, match_end); its trimmed-leading length is
    // (match_end - first-non-space). So offset == first-non-space index, i.e.
    // the position of the `+`.
    if let Some(m) = match_shorthand_start(bytes) {
        let trimmed_start = skip_spaces(bytes, 0); // first non-`\s` of match[0]
        positions.push(BudgetPosition {
            start: trimmed_start,
            end: m.match_end,
        });
    }

    // SHORTHAND_END: endStart = match.index + 1 (skip the leading captured \s).
    if let Some(m) = match_shorthand_end(bytes) {
        let end_start = m.match_start + 1;
        let already_covered = positions
            .iter()
            .any(|p| end_start >= p.start && end_start < p.end);
        if !already_covered {
            positions.push(BudgetPosition {
                start: end_start,
                end: m.match_end,
            });
        }
    }

    // VERBOSE (global): every non-overlapping match, scanning forward.
    let mut from = 0usize;
    while let Some(m) = match_verbose_at(bytes, from) {
        positions.push(BudgetPosition {
            start: m.match_start,
            end: m.match_end,
        });
        // `matchAll` advances past the whole match; if the match is empty it
        // would advance by one, but this grammar never matches empty.
        from = if m.match_end > m.match_start {
            m.match_end
        } else {
            m.match_start + 1
        };
    }

    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- check_token_budget (port of tokenBudget.test.ts) ----

    #[test]
    fn continues_under_90_percent_incrementing_count() {
        let mut tracker = BudgetTracker::new();
        // 100k / 500k = 20% < 90% → continue, count 1.
        let d = check_token_budget(&mut tracker, None, Some(500_000), 100_000);
        match d {
            TokenBudgetDecision::Continue {
                continuation_count,
                pct,
                turn_tokens,
                budget,
                ..
            } => {
                assert_eq!(continuation_count, 1);
                assert_eq!(pct, 20);
                assert_eq!(turn_tokens, 100_000);
                assert_eq!(budget, 500_000);
            }
            d @ TokenBudgetDecision::Stop { .. } => panic!("expected Continue, got {d:?}"),
        }
        assert_eq!(tracker.continuation_count, 1);
        assert_eq!(tracker.last_global_turn_tokens, 100_000);

        // 200k → 40% < 90% → continue, count 2.
        let d = check_token_budget(&mut tracker, None, Some(500_000), 200_000);
        match d {
            TokenBudgetDecision::Continue {
                continuation_count, ..
            } => assert_eq!(continuation_count, 2),
            d @ TokenBudgetDecision::Stop { .. } => panic!("expected Continue, got {d:?}"),
        }
        assert_eq!(tracker.continuation_count, 2);
        assert_eq!(tracker.last_delta_tokens, 100_000);
        assert_eq!(tracker.last_global_turn_tokens, 200_000);
    }

    #[test]
    fn stops_at_or_above_90_percent() {
        let mut tracker = BudgetTracker::new();
        // Prime one continuation so the stop carries a completion event.
        let _ = check_token_budget(&mut tracker, None, Some(500_000), 100_000);
        // 450k / 500k = 90% → NOT < 90% → stop.
        let d = check_token_budget(&mut tracker, None, Some(500_000), 450_000);
        match d {
            TokenBudgetDecision::Stop {
                completion_event: Some(ev),
            } => {
                assert_eq!(ev.pct, 90);
                assert!(!ev.diminishing_returns);
                assert_eq!(ev.continuation_count, 1);
                assert_eq!(ev.turn_tokens, 450_000);
                assert_eq!(ev.budget, 500_000);
            }
            other => panic!("expected Stop(Some), got {other:?}"),
        }
    }

    #[test]
    fn diminishing_returns_stop_after_3_continuations_under_500_delta() {
        let mut tracker = BudgetTracker::new();
        // Drive the count to >= 3 with small deltas while staying < 90%.
        // budget 1_000_000; 90% threshold is 900_000.
        // Step 1: 100 tokens → continue, count 1, delta 100.
        let _ = check_token_budget(&mut tracker, None, Some(1_000_000), 100);
        // Step 2: 200 → continue, count 2, delta 100.
        let _ = check_token_budget(&mut tracker, None, Some(1_000_000), 200);
        // Step 3: 300 → continue, count 3, delta 100 (lastDelta now 100 < 500).
        let _ = check_token_budget(&mut tracker, None, Some(1_000_000), 300);
        assert_eq!(tracker.continuation_count, 3);
        // Step 4: 400 → count >=3, delta 100 < 500, lastDelta 100 < 500 →
        // diminishing → stop with diminishingReturns true.
        let d = check_token_budget(&mut tracker, None, Some(1_000_000), 400);
        match d {
            TokenBudgetDecision::Stop {
                completion_event: Some(ev),
            } => {
                assert!(ev.diminishing_returns);
                assert_eq!(ev.continuation_count, 3);
            }
            other => panic!("expected diminishing Stop, got {other:?}"),
        }
    }

    #[test]
    fn no_diminishing_when_delta_large() {
        let mut tracker = BudgetTracker::new();
        // count to 3 with large deltas → never diminishing.
        let _ = check_token_budget(&mut tracker, None, Some(10_000_000), 1_000);
        let _ = check_token_budget(&mut tracker, None, Some(10_000_000), 600_000);
        let _ = check_token_budget(&mut tracker, None, Some(10_000_000), 1_200_000);
        assert_eq!(tracker.continuation_count, 3);
        // Step 4: delta 600k >= 500, still < 90% (1.8m/10m=18%) → continue.
        let d = check_token_budget(&mut tracker, None, Some(10_000_000), 1_800_000);
        assert!(matches!(d, TokenBudgetDecision::Continue { .. }));
        assert_eq!(tracker.continuation_count, 4);
    }

    #[test]
    fn agent_id_set_immediate_stop() {
        let mut tracker = BudgetTracker::new();
        let d = check_token_budget(&mut tracker, Some("agent-7"), Some(500_000), 1);
        assert_eq!(
            d,
            TokenBudgetDecision::Stop {
                completion_event: None
            }
        );
        // Tracker untouched.
        assert_eq!(tracker.continuation_count, 0);
    }

    #[test]
    fn null_or_nonpositive_budget_stops() {
        let mut tracker = BudgetTracker::new();
        assert_eq!(
            check_token_budget(&mut tracker, None, None, 100),
            TokenBudgetDecision::Stop {
                completion_event: None
            }
        );
        assert_eq!(
            check_token_budget(&mut tracker, None, Some(0), 100),
            TokenBudgetDecision::Stop {
                completion_event: None
            }
        );
    }

    #[test]
    fn stop_under_threshold_with_zero_continuations_has_no_event() {
        // First check already at/over 90% with zero prior continuations →
        // not under threshold, not diminishing, continuationCount == 0 → the
        // final `Stop { completion_event: None }` branch.
        let mut tracker = BudgetTracker::new();
        let d = check_token_budget(&mut tracker, None, Some(100), 95);
        assert_eq!(
            d,
            TokenBudgetDecision::Stop {
                completion_event: None
            }
        );
    }

    // ---- budget_continuation_message bytes ----

    #[test]
    fn continuation_message_is_byte_exact() {
        let msg = budget_continuation_message(73, 365_000, 500_000);
        assert_eq!(
            msg,
            "Stopped at 73% of token target (365,000 / 500,000). Keep working \u{2014} do not summarize."
        );
        // em-dash present, ASCII hyphen-minus absent in that slot.
        assert!(msg.contains('\u{2014}'));
        assert!(!msg.contains("working - do"));
    }

    #[test]
    fn en_us_grouping() {
        assert_eq!(group_en_us(0), "0");
        assert_eq!(group_en_us(1), "1");
        assert_eq!(group_en_us(12), "12");
        assert_eq!(group_en_us(123), "123");
        assert_eq!(group_en_us(1_234), "1,234");
        assert_eq!(group_en_us(12_345), "12,345");
        assert_eq!(group_en_us(123_456), "123,456");
        assert_eq!(group_en_us(1_234_567), "1,234,567");
        assert_eq!(group_en_us(2_500_000), "2,500,000");
    }

    // ---- parse_token_budget (the 3 regex parse cases) ----

    #[test]
    fn parse_shorthand_start_plus_500k() {
        assert_eq!(parse_token_budget("+500k"), Some(500_000));
        assert_eq!(parse_token_budget("+500k do the thing"), Some(500_000));
        assert_eq!(parse_token_budget("  +500K refactor"), Some(500_000));
    }

    #[test]
    fn parse_shorthand_end_plus_2_5m_end_anchored() {
        assert_eq!(
            parse_token_budget("do the big refactor +2.5m"),
            Some(2_500_000)
        );
        // trailing punctuation + whitespace allowed by SHORTHAND_END_RE.
        assert_eq!(parse_token_budget("go big +2.5m."), Some(2_500_000));
        assert_eq!(parse_token_budget("go big +2.5m !"), Some(2_500_000));
    }

    #[test]
    fn parse_verbose_use_2m_tokens() {
        assert_eq!(
            parse_token_budget("please use 2M tokens for this"),
            Some(2_000_000)
        );
        assert_eq!(parse_token_budget("spend 2m tokens"), Some(2_000_000));
        assert_eq!(parse_token_budget("use 1.5k token"), Some(1_500));
    }

    #[test]
    fn parse_returns_none_for_non_budget_text() {
        assert_eq!(parse_token_budget("just a normal message"), None);
        // `+500` without a unit doesn't match.
        assert_eq!(parse_token_budget("+500"), None);
        // mid-sentence `+500k` (not start-anchored, no leading space-anchored
        // end either, no verbose) → None.
        assert_eq!(parse_token_budget("cost+500k"), None);
        // billion suffix exercises the `b` multiplier path.
        assert_eq!(parse_token_budget("+1b"), Some(1_000_000_000));
    }

    // ---- find_token_budget_positions (offsets) ----

    #[test]
    fn positions_shorthand_start_offsets_past_leading_ws() {
        // "  +500k" — leading 2 spaces; start lands on '+' at index 2.
        let pos = find_token_budget_positions("  +500k");
        assert_eq!(pos, vec![BudgetPosition { start: 2, end: 7 }]);
    }

    #[test]
    fn positions_plus_500k_no_double_count() {
        // "+500k" matches BOTH start (index 0) and end (leading \s? none → end
        // RE needs a leading \s, so it does NOT match "+500k"). Only the start
        // span is reported.
        let pos = find_token_budget_positions("+500k");
        assert_eq!(pos, vec![BudgetPosition { start: 0, end: 5 }]);
    }

    #[test]
    fn positions_shorthand_end_offset_plus_one() {
        // "go +500k": end RE matches starting at the space (index 2); reported
        // start = index + 1 = 3 (the '+'); end = end-of-string = 8.
        let pos = find_token_budget_positions("go +500k");
        // start RE does NOT match (not start-anchored: 'g' precedes), so only
        // the end span is present.
        assert_eq!(pos, vec![BudgetPosition { start: 3, end: 8 }]);
    }

    #[test]
    fn positions_verbose_global() {
        // Two verbose matches.
        let text = "use 2m tokens then spend 3k tokens";
        let pos = find_token_budget_positions(text);
        // "use 2m tokens" at [0, 13); "spend 3k tokens" at [19, 34).
        assert_eq!(
            pos,
            vec![
                BudgetPosition { start: 0, end: 13 },
                BudgetPosition { start: 19, end: 34 },
            ]
        );
    }

    #[test]
    fn positions_start_and_end_both_without_double_count() {
        // "+500k and finish +2m" — start matches "+500k" [0,5); end matches
        // " +2m" (leading space at 16) → endStart 17, end 20. The endStart 17
        // is NOT inside [0,5) so both are reported.
        let pos = find_token_budget_positions("+500k and finish +2m");
        assert_eq!(
            pos,
            vec![
                BudgetPosition { start: 0, end: 5 },
                BudgetPosition { start: 17, end: 20 },
            ]
        );
    }
}
