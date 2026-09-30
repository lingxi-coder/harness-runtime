//! Orchestrator-side errors.
//!
//! `MaxTurnsReached` carries the byte-locked Display literal
//! `"Reached maximum number of turns (<n>)"` — verified against
//! `claude-code/src/QueryEngine.ts:870` on 2026-05-25.
//!
//! Task 7: `RepeatedOverloaded` carries the byte-locked
//! `"Repeated 529 Overloaded errors"` copy from
//! `claude-code/src/services/api/errors.ts:166`.

use llm_runtime::LlmError;
use thiserror::Error;

/// Byte-locked copy for the "Repeated 529 Overloaded errors" error message.
///
/// Locked against `claude-code/src/services/api/errors.ts:166`:
/// ```ts
/// export const REPEATED_529_ERROR_MESSAGE = 'Repeated 529 Overloaded errors'
/// ```
/// Thrown by `withRetry.ts:359-362` for external, non-sandbox callers when
/// `consecutive_overloaded >= MAX_529_RETRIES` and no fallback model is
/// configured.  The Rust equivalent is [`OrchestratorError::RepeatedOverloaded`].
pub const REPEATED_529_ERROR_MESSAGE: &str = "Repeated 529 Overloaded errors";

/// Failure modes of `ConversationOrchestrator::run_turn`.
///
/// `MaxTurnsReached` is byte-locked against `QueryEngine.ts:870`. Do not
/// change the format string without a corresponding spec amendment.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// The configured `max_turns` budget was exhausted before the model
    /// emitted `stop_reason == "end_turn"`.
    #[error("Reached maximum number of turns ({max_turns})")]
    MaxTurnsReached {
        /// The configured ceiling that was reached.
        max_turns: u32,
    },

    /// The configured `max_budget_nano_usd` cost ceiling was reached before the
    /// model emitted `end_turn`. 1:1 with claude-code's `error_max_budget_usd`
    /// (`QueryEngine.ts:983-999`, "Reached maximum budget ($X)").
    ///
    /// claude interpolates the raw `maxBudgetUsd` JS number with no `toFixed`
    /// (`($${maxBudgetUsd})`), so `$5` renders `($5)` and `$1.5` renders
    /// `($1.5)`. Rust's `{}` for `f64` uses the same shortest round-trip
    /// stringification, so we match byte-for-byte for realistic budgets
    /// instead of forcing two decimals.
    #[error("Reached maximum budget (${})", (*budget_nano_usd as f64) / 1_000_000_000.0)]
    MaxBudgetReached {
        /// The cost ceiling that was reached, in nano-USD.
        budget_nano_usd: u64,
    },

    /// Permission policy terminated a prompt-avoiding agent instead of
    /// returning a recoverable tool denial.
    #[error("{message}")]
    PermissionAbort {
        /// Byte-exact terminal message from the permission gate.
        message: String,
    },

    /// The model API call failed (transport, rate-limit, context overflow,
    /// etc). Wraps `llm_runtime::LlmError` — the live path flows through
    /// `ProviderApiAdapter → ModelRuntime`.
    ///
    /// Note: `LlmError::Overloaded { repeated: true }` is **not** wrapped here;
    /// it is converted to [`OrchestratorError::RepeatedOverloaded`] instead.
    /// See the manual `From<LlmError>` impl below.
    #[error("api call failed: {0}")]
    ApiCall(LlmError),

    /// A non-tool runtime error inside the orchestrator. Used for
    /// internal invariants (unexpected content block variant in the
    /// hot path, etc.). Test stubs use this for synthetic failures.
    #[error("orchestrator internal error: {0}")]
    Internal(String),

    /// Mid-stream byte-level error from the streaming transport. Surfaced
    /// when the SSE chunk fails to decode or the HTTP body is cut.
    ///
    /// No `#[from]` impl — the batched `ApiCall` variant already claims
    /// `LlmError`. Convert manually at the streaming call site via
    /// `OrchestratorError::Streaming(llm_err)`.
    #[error("streaming transport error: {0}")]
    Streaming(LlmError),

    /// Stream produced an event that violates the per-block protocol
    /// (out-of-order delta, double stop, type mismatch, malformed
    /// `tool_use` input JSON).
    #[error("streaming protocol violation: {0}")]
    StreamingProtocol(String),

    /// Stream ended cleanly before a `message_stop` arrived. Mirrors
    /// claude-code's "stream completed without `message_start`" fallback
    /// (claude.ts:2353) — surfaced as an explicit error rather than
    /// silently retrying.
    #[error("stream ended without message_stop event")]
    StreamEndedWithoutStop,

    /// Compaction layer surfaced an error. (M6-08)
    #[error("compaction failed: {0}")]
    Compaction(#[from] compaction::CompactionError),

    /// `force_compact` was cancelled mid-run by the supplied
    /// `CancellationToken`. (M6-08)
    #[error("compaction cancelled")]
    CompactionCancelled,

    /// The user cancelled while the pre-call image sidecar was in flight.
    #[error("vision delegation cancelled")]
    VisionDelegationCancelled,

    /// The API returned `Overloaded` (529) on every retry attempt and
    /// `consecutive_overloaded >= MAX_529_RETRIES` for an external,
    /// non-sandbox caller with no fallback model configured.
    ///
    /// Display is byte-locked to `errors.ts:166`:
    /// `"Repeated 529 Overloaded errors"` — do not change without a
    /// corresponding spec amendment.
    ///
    /// Task 7 (claude.ts parity): `ProviderApiAdapter` returns
    /// `LlmError::Overloaded { repeated: true }` from the
    /// `DriveStep::RepeatedOverloaded` arm; the `From<LlmError>` impl on
    /// `OrchestratorError` converts it to this variant.
    #[error("{}", REPEATED_529_ERROR_MESSAGE)]
    RepeatedOverloaded,

    /// A terminal 429 with a composed user-facing explanation. For Anthropic,
    /// this is the limits-specific `getRateLimitErrorMessage` text; for an
    /// OpenRouter free model it preserves the provider's `error.message` and
    /// adds retry/model-switch guidance.
    ///
    /// Task 6 (llm-runtime future-work batch 5): produced by the public turn
    /// drivers when the turn dies on `LlmError::RateLimited` AND the API
    /// client recorded a composed explanation
    /// (`OrchestratorApiClient::last_rate_limit_error_message`). Without that
    /// context the generic `ApiCall(RateLimited)` surface is unchanged.
    #[error("{message}")]
    RateLimitRejected {
        /// The pre-composed user-facing copy.
        message: String,
    },
}

/// Convert `LlmError` → `OrchestratorError`.
///
/// `Overloaded { repeated: true }` maps to [`OrchestratorError::RepeatedOverloaded`]
/// so the byte-locked "Repeated 529 Overloaded errors" message surfaces correctly
/// (errors.ts:166).  All other variants wrap as [`OrchestratorError::ApiCall`].
impl From<LlmError> for OrchestratorError {
    fn from(e: LlmError) -> Self {
        match e {
            LlmError::Overloaded { repeated: true } => OrchestratorError::RepeatedOverloaded,
            other => OrchestratorError::ApiCall(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 3b (Task 7): byte-locked copy for the "Repeated 529 Overloaded errors" error.
    ///
    /// Source: `claude-code/src/services/api/errors.ts:166`
    /// ```ts
    /// export const REPEATED_529_ERROR_MESSAGE = 'Repeated 529 Overloaded errors'
    /// ```
    /// Thrown by `withRetry.ts:359-362` for external, non-sandbox callers when
    /// `consecutive_overloaded >= MAX_529_RETRIES` and no fallback model is configured.
    #[test]
    fn repeated_529_terminal_renders_byte_locked_copy() {
        // Locked against errors.ts:166: REPEATED_529_ERROR_MESSAGE = 'Repeated 529 Overloaded errors'
        assert_eq!(
            REPEATED_529_ERROR_MESSAGE, "Repeated 529 Overloaded errors",
            "REPEATED_529_ERROR_MESSAGE constant must be byte-locked"
        );
        let err = OrchestratorError::RepeatedOverloaded;
        assert_eq!(
            err.to_string(),
            "Repeated 529 Overloaded errors",
            "OrchestratorError::RepeatedOverloaded must display the byte-locked copy"
        );
    }

    /// Task 6 (batch 5): `RateLimitRejected` Display is the composed copy
    /// verbatim — no prefix, no suffix (the TS error content is the message
    /// itself, errors.ts:521-524).
    #[test]
    fn rate_limit_rejected_display_is_message_verbatim() {
        let err = OrchestratorError::RateLimitRejected {
            message: "You've hit your weekly limit · resets 3pm".into(),
        };
        assert_eq!(err.to_string(), "You've hit your weekly limit · resets 3pm");
    }

    #[test]
    fn max_turns_reached_display_is_byte_locked_against_query_engine_ts_870() {
        // Source: claude-code/src/QueryEngine.ts:870
        //   `Reached maximum number of turns (${message.attachment.maxTurns})`
        let err = OrchestratorError::MaxTurnsReached { max_turns: 30 };
        assert_eq!(err.to_string(), "Reached maximum number of turns (30)");
    }

    #[test]
    fn max_turns_reached_display_handles_one() {
        let err = OrchestratorError::MaxTurnsReached { max_turns: 1 };
        assert_eq!(err.to_string(), "Reached maximum number of turns (1)");
    }

    #[test]
    fn max_turns_reached_display_handles_large_number() {
        let err = OrchestratorError::MaxTurnsReached { max_turns: 9999 };
        assert_eq!(err.to_string(), "Reached maximum number of turns (9999)");
    }

    #[test]
    fn max_budget_reached_display_formats_usd() {
        // nano-USD → raw JS-number rendering (claude "Reached maximum budget
        // ($X)"): no toFixed, so integers and short decimals drop trailing
        // zeros — `$5` not `$5.00`, `$1.5` not `$1.50`.
        let err = OrchestratorError::MaxBudgetReached {
            budget_nano_usd: 5_000_000_000,
        };
        assert_eq!(err.to_string(), "Reached maximum budget ($5)");
        let err2 = OrchestratorError::MaxBudgetReached {
            budget_nano_usd: 1_500_000_000,
        };
        assert_eq!(err2.to_string(), "Reached maximum budget ($1.5)");
    }

    #[test]
    fn permission_abort_display_is_message_verbatim() {
        let err = OrchestratorError::PermissionAbort {
            message: "Agent aborted: too many classifier denials in headless mode".into(),
        };
        assert_eq!(
            err.to_string(),
            "Agent aborted: too many classifier denials in headless mode"
        );
    }

    #[test]
    fn internal_display_carries_payload() {
        let err = OrchestratorError::Internal("synthetic".into());
        assert_eq!(err.to_string(), "orchestrator internal error: synthetic");
    }

    #[test]
    fn streaming_display_starts_with_locked_prefix() {
        let e = OrchestratorError::Streaming(llm_runtime::LlmError::Transport {
            message: "nope".into(),
        });
        let s = format!("{e}");
        assert!(s.starts_with("streaming transport error: "), "{s}");
    }

    #[test]
    fn streaming_protocol_display_carries_inner() {
        let e = OrchestratorError::StreamingProtocol("block 3 has no start".into());
        assert_eq!(
            format!("{e}"),
            "streaming protocol violation: block 3 has no start"
        );
    }

    #[test]
    fn stream_ended_without_stop_display_is_locked() {
        assert_eq!(
            format!("{}", OrchestratorError::StreamEndedWithoutStop),
            "stream ended without message_stop event"
        );
    }

    #[test]
    fn compaction_variant_projects_to_string() {
        // CompactionError::Api wraps llm_runtime::LlmError; construct via the
        // string variant for simplicity.
        let compact_err = compaction::CompactionError::Internal("test".into());
        let e = OrchestratorError::Compaction(compact_err);
        let s = e.to_string();
        assert!(s.contains("compaction"), "got: {s}");
    }

    #[test]
    fn compaction_cancelled_renders() {
        let e = OrchestratorError::CompactionCancelled;
        assert_eq!(e.to_string(), "compaction cancelled");
    }

    // ── Fix 2: From<LlmError> routes repeated bit correctly ──────────────────

    /// `LlmError::Overloaded { repeated: true }` must convert to
    /// `OrchestratorError::RepeatedOverloaded` (not `ApiCall`).
    #[test]
    fn overloaded_repeated_true_converts_to_repeated_overloaded() {
        let e: OrchestratorError = LlmError::Overloaded { repeated: true }.into();
        assert!(
            matches!(e, OrchestratorError::RepeatedOverloaded),
            "Overloaded {{ repeated: true }} must convert to RepeatedOverloaded, got {e:?}"
        );
        assert_eq!(e.to_string(), REPEATED_529_ERROR_MESSAGE);
    }

    /// `LlmError::Overloaded { repeated: false }` must convert to `ApiCall`
    /// (the normal error path).
    #[test]
    fn overloaded_repeated_false_converts_to_api_call() {
        let e: OrchestratorError = LlmError::Overloaded { repeated: false }.into();
        assert!(
            matches!(e, OrchestratorError::ApiCall(_)),
            "Overloaded {{ repeated: false }} must convert to ApiCall, got {e:?}"
        );
    }

    /// All non-overloaded `LlmError` variants must convert to `ApiCall`.
    #[test]
    fn non_overloaded_llm_errors_convert_to_api_call() {
        let variants: Vec<LlmError> = vec![
            LlmError::Authentication {
                message: String::new(),
            },
            LlmError::PermissionDenied {
                message: String::new(),
            },
            LlmError::ProviderInternal,
            LlmError::Transport {
                message: "t".into(),
            },
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            LlmError::ContextOverflow { token_gap: 0 },
            LlmError::InvalidRequest {
                message: "bad".into(),
            },
        ];
        for variant in variants {
            let e: OrchestratorError = variant.clone().into();
            assert!(
                matches!(e, OrchestratorError::ApiCall(_)),
                "{variant:?} must convert to ApiCall, got {e:?}"
            );
        }
    }
}
