//! `max_tokens` context-overflow detection + re-shrink arithmetic.
//!
//! When Anthropic or an OpenAI-compatible router rejects a request because the
//! prompt plus the requested output budget exceed the model's context window,
//! it returns a **400** that reports the server-counted input, requested output
//! and context limit. Anthropic's error has the shape:
//!
//! ```text
//! input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000
//! ```
//!
//! claude-code parses those three numbers, recomputes a safe `max_tokens` that
//! still fits the window, and retries the request **once more** with the
//! reduced cap instead of failing the turn.
//!
//! This module ports the two pure helpers that drive that behaviour:
//!
//! * [`parse_max_tokens_overflow`] — the `Claude Code` parser plus `OpenRouter`'s
//!   text/tool-input breakdown.
//! * [`adjusted_max_tokens`] — the `Claude Code` safety-buffer and output-floor
//!   arithmetic, with an additional guard against a thinking minimum that
//!   cannot fit.
//!
//! The retry wiring (mutating the request body and re-looping) lives in the
//! service's streaming and non-streaming drivers; this module stays pure so
//! the arithmetic can be unit-tested in isolation.

#![forbid(unsafe_code)]

use regex::Regex;
use std::sync::OnceLock;

/// Floor on the re-shrunk output-token budget. Byte-locked against claude-code
/// `FLOOR_OUTPUT_TOKENS` (`withRetry.ts:53`): the adjusted `max_tokens` is never
/// allowed below this, even if the available context is smaller — in which case
/// we instead give up and surface the original 400 (see
/// [`adjusted_max_tokens`]).
pub const FLOOR_OUTPUT_TOKENS: u64 = 3000;

/// Safety buffer subtracted from the context limit before recomputing the
/// output budget. Byte-locked against claude-code `safetyBuffer` (`withRetry.ts:393`).
pub const SAFETY_BUFFER: u64 = 1000;

/// The three numbers parsed out of a `max_tokens` context-overflow 400.
///
/// Field names mirror the TS object returned by
/// `parseMaxTokensContextOverflowError`: `{ inputTokens, maxTokens, contextLimit }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow {
    /// `A` in `A + B > C` — the prompt / input token count the server measured.
    pub input_tokens: u64,
    /// `B` in `A + B > C` — the `max_tokens` we asked for.
    pub max_tokens: u64,
    /// `C` in `A + B > C` — the model's context-window limit.
    pub context_limit: u64,
}

/// Compiled overflow regex, lazily built once.
///
/// Pattern is byte-identical to claude-code `withRetry.ts:571`:
/// ``/input length and `max_tokens` exceed context limit: (\d+) \+ (\d+) > (\d+)/``.
fn overflow_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The pattern is a fixed literal; compilation cannot fail. Using
        // `expect` keeps the helper infallible without a fallible public API.
        Regex::new(r"input length and `max_tokens` exceed context limit: (\d+) \+ (\d+) > (\d+)")
            .expect("overflow regex is a valid, fixed pattern")
    })
}

fn openrouter_overflow_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?is)maximum context length is ([\d,]+) tokens.*?requested about ([\d,]+) tokens \(([\d,]+) of text input, ([\d,]+) of tool input, ([\d,]+) in the output\)",
        )
        .expect("OpenRouter overflow regex is a valid, fixed pattern")
    })
}

fn parse_number(value: &str) -> Option<u64> {
    value.replace(',', "").parse::<u64>().ok()
}

/// Parse a `max_tokens` context-overflow **400** into its three numbers.
///
/// Supports `Claude Code`'s Anthropic shape and `OpenRouter`'s equivalent
/// text/tool-input breakdown:
///
/// 1. Returns `None` unless `status == 400` (`:557`).
/// 2. For Anthropic, applies the upstream literal gate and capture regex.
/// 3. For `OpenRouter`, sums the reported text and tool input and uses the
///    reported output and context values.
///
/// `body` is the server's error message text (claude-code reads
/// `error.message`); this helper also accepts a complete JSON error envelope.
///
/// Note: the three captures are `\d+`, so the values always parse; the TS
/// `isNaN` guard (`:590`) is structurally unreachable here and is therefore not
/// reproduced as a separate branch.
#[must_use]
pub fn parse_max_tokens_overflow(status: u16, body: &str) -> Option<Overflow> {
    if status != 400 {
        return None;
    }
    if body.contains("input length and `max_tokens` exceed context limit") {
        let caps = overflow_regex().captures(body)?;
        return Some(Overflow {
            input_tokens: parse_number(caps.get(1)?.as_str())?,
            max_tokens: parse_number(caps.get(2)?.as_str())?,
            context_limit: parse_number(caps.get(3)?.as_str())?,
        });
    }

    let caps = openrouter_overflow_regex().captures(body)?;
    let context_limit = parse_number(caps.get(1)?.as_str())?;
    let text_input = parse_number(caps.get(3)?.as_str())?;
    let tool_input = parse_number(caps.get(4)?.as_str())?;
    Some(Overflow {
        input_tokens: text_input.saturating_add(tool_input),
        max_tokens: parse_number(caps.get(5)?.as_str())?,
        context_limit,
    })
}

/// Parse the overflow shape out of an llm-runtime `InvalidRequest` message.
///
/// Delegates to [`parse_max_tokens_overflow`] with `status = 400`, which is
/// the HTTP status Anthropic and `OpenRouter` return for this class of error.
/// Used by the retry drivers to detect context-overflow `InvalidRequest`
/// errors and compute an adjusted `max_tokens`.
#[must_use]
pub fn parse_overflow_message(message: &str) -> Option<Overflow> {
    parse_max_tokens_overflow(400, message)
}

/// Recompute a safe `max_tokens` from a parsed [`Overflow`], or `None` to give
/// up (surface the original 400).
///
/// Based on the overflow branch arithmetic in Claude Code
/// (`withRetry.ts:391-415`):
///
/// ```text
/// availableContext = max(0, contextLimit - inputTokens - 1000)
/// if availableContext < 3000:          // FLOOR_OUTPUT_TOKENS
///     return None                       // throw error (give up)
/// minRequired = thinkingBudget + 1      // thinkingBudget = 0 when disabled
/// if minRequired > availableContext: return None
/// adjusted = availableContext
/// ```
///
/// `thinking_budget` is the extended-thinking budget in tokens (0 when thinking
/// is disabled), threaded from the request options. The `+ 1` guarantees at
/// least one output token beyond the thinking budget.
///
/// The result is returned as a `u32` to match the request body's `max_tokens`
/// wire type. The clamp arithmetic happens in `u64` to avoid overflow; the
/// final value is bounded by `context_limit` (a realistic window ≤ a few
/// million) so the `u32` cast is lossless in practice. If, pathologically, the
/// value exceeds `u32::MAX` it is saturated to `u32::MAX`.
#[must_use]
pub fn adjusted_max_tokens(overflow: Overflow, thinking_budget: u64) -> Option<u32> {
    // availableContext = max(0, contextLimit - inputTokens - safetyBuffer).
    // saturating_sub gives the `max(0, ...)` clamp for free.
    let available_context = overflow
        .context_limit
        .saturating_sub(overflow.input_tokens)
        .saturating_sub(SAFETY_BUFFER);

    if available_context < FLOOR_OUTPUT_TOKENS {
        // Give up: not enough room even at the floor. Caller surfaces the
        // original 400 (claude-code `throw error`, withRetry.ts:404).
        return None;
    }

    // minRequired = thinkingBudget + 1 (at least one output token).
    let min_required = thinking_budget.saturating_add(1);
    if min_required > available_context {
        return None;
    }

    Some(u32::try_from(available_context).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    /// The exact sample message from the TS source comment (`withRetry.ts:569`).
    const SAMPLE: &str =
        "input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000";

    #[test]
    fn parses_exact_sample() {
        let got = parse_max_tokens_overflow(400, SAMPLE).expect("should parse");
        assert_eq!(
            got,
            Overflow {
                input_tokens: 188_059,
                max_tokens: 20_000,
                context_limit: 200_000,
            }
        );
    }

    #[test]
    fn parses_when_embedded_in_a_larger_json_body() {
        // The real 400 body wraps the message in an error envelope; the
        // substring + regex must still find it.
        let body = format!(
            r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{SAMPLE}"}}}}"#
        );
        let got = parse_max_tokens_overflow(400, &body).expect("should parse from envelope");
        assert_eq!(got.input_tokens, 188_059);
        assert_eq!(got.max_tokens, 20_000);
        assert_eq!(got.context_limit, 200_000);
    }

    #[test]
    fn rejects_non_400_status() {
        // Same message, wrong status → None (TS `:557`).
        assert_eq!(parse_max_tokens_overflow(429, SAMPLE), None);
        assert_eq!(parse_max_tokens_overflow(500, SAMPLE), None);
        assert_eq!(parse_max_tokens_overflow(200, SAMPLE), None);
    }

    #[test]
    fn rejects_400_without_marker() {
        assert_eq!(
            parse_max_tokens_overflow(400, "some other invalid_request_error"),
            None
        );
    }

    #[test]
    fn rejects_marker_present_but_numbers_missing() {
        // The substring gate passes but the `A + B > C` regex does not match,
        // so the whole thing is None (TS `match.length !== 4`, `:574`).
        let body = "input length and `max_tokens` exceed context limit (no numbers here)";
        assert_eq!(parse_max_tokens_overflow(400, body), None);
    }

    #[test]
    fn parses_arbitrary_numbers() {
        let body = "input length and `max_tokens` exceed context limit: 5 + 7 > 11";
        assert_eq!(
            parse_max_tokens_overflow(400, body),
            Some(Overflow {
                input_tokens: 5,
                max_tokens: 7,
                context_limit: 11,
            })
        );
    }

    #[test]
    fn parses_openrouter_text_and_tool_input_breakdown() {
        let body = "This endpoint's maximum context length is 256000 tokens. However, you requested about 260085 tokens (10691 of text input, 24791 of tool input, 224603 in the output). Please reduce the length of either one.";
        assert_eq!(
            parse_max_tokens_overflow(400, body),
            Some(Overflow {
                input_tokens: 35_482,
                max_tokens: 224_603,
                context_limit: 256_000,
            })
        );
    }

    // --- parse_overflow_message ---

    #[test]
    fn parse_overflow_message_canonical_message_parses() {
        let msg = "input length and `max_tokens` exceed context limit: 188059 + 20000 > 200000";
        let got = parse_overflow_message(msg).expect("canonical message should parse");
        assert_eq!(got.input_tokens, 188_059);
        assert_eq!(got.max_tokens, 20_000);
        assert_eq!(got.context_limit, 200_000);
    }

    #[test]
    fn parse_overflow_message_unrelated_returns_none() {
        assert_eq!(parse_overflow_message("some unrelated error message"), None);
    }
}

#[cfg(test)]
mod adjust_tests {
    use super::*;

    /// The worked example from the spec: input 188059, limit 200000, thinking 0
    /// → available = 200000 - 188059 - 1000 = 10941 → max(3000, 10941, 1) = 10941.
    #[test]
    fn spec_sample_thinking_zero() {
        let overflow = Overflow {
            input_tokens: 188_059,
            max_tokens: 20_000,
            context_limit: 200_000,
        };
        assert_eq!(adjusted_max_tokens(overflow, 0), Some(10_941));
    }

    #[test]
    fn floor_wins_when_available_below_floor_but_room_remains() {
        // available = 100000 - 96000 - 1000 = 3000 (== floor) → not < floor →
        // return 3000.
        let overflow = Overflow {
            input_tokens: 96_000,
            max_tokens: 50_000,
            context_limit: 100_000,
        };
        assert_eq!(adjusted_max_tokens(overflow, 0), Some(3000u32));
    }

    #[test]
    fn gives_up_when_available_strictly_below_floor() {
        // available = 100000 - 96001 - 1000 = 2999 (< 3000) → None (give up).
        let overflow = Overflow {
            input_tokens: 96_001,
            max_tokens: 50_000,
            context_limit: 100_000,
        };
        assert_eq!(adjusted_max_tokens(overflow, 0), None);
    }

    #[test]
    fn tiny_context_gives_up() {
        // Spec "tiny-context" case: nearly no room → None.
        let overflow = Overflow {
            input_tokens: 199_000,
            max_tokens: 20_000,
            context_limit: 200_000,
        };
        // available = 200000 - 199000 - 1000 = 0 < 3000 → None.
        assert_eq!(adjusted_max_tokens(overflow, 0), None);
    }

    #[test]
    fn thinking_budget_that_cannot_fit_gives_up() {
        // available = 10941; a 15001-token minimum cannot fit and must not
        // manufacture another overflowing retry.
        let overflow = Overflow {
            input_tokens: 188_059,
            max_tokens: 20_000,
            context_limit: 200_000,
        };
        assert_eq!(adjusted_max_tokens(overflow, 15_000), None);
    }

    #[test]
    fn thinking_budget_below_available_does_not_raise() {
        // thinking 5000 → minRequired 5001 < available 10941 → stays 10941.
        let overflow = Overflow {
            input_tokens: 188_059,
            max_tokens: 20_000,
            context_limit: 200_000,
        };
        assert_eq!(adjusted_max_tokens(overflow, 5_000), Some(10_941));
    }

    #[test]
    fn saturates_to_zero_available_on_underflow() {
        // inputTokens > contextLimit → saturating_sub clamps to 0 → < floor → None.
        let overflow = Overflow {
            input_tokens: 300_000,
            max_tokens: 20_000,
            context_limit: 200_000,
        };
        assert_eq!(adjusted_max_tokens(overflow, 0), None);
    }

    #[test]
    fn constants_are_byte_locked() {
        assert_eq!(FLOOR_OUTPUT_TOKENS, 3000);
        assert_eq!(SAFETY_BUFFER, 1000);
    }
}
