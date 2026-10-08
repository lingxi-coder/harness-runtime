//! Context-hint negotiation — claude-code `createContextHintController` (`e1y`,
//! 2.1.220 @237520650) and its `handleHintReject` / `applyHintEdits` pair.
//!
//! This is a CLIENT-SERVER negotiation, not a local compaction policy. When the
//! main REPL thread is about to send a request that a keep-recent microcompact
//! could shrink by at least [`MICROCOMPACT_MIN_TOKENS_SAVED`], the
//! client adds a beta header plus
//! `context_hint: {enabled: true, target_tokens_saved: N}` to the body, telling
//! the server "I can free N tokens if you need me to". The server may then:
//!
//! | Response | Oracle | Meaning |
//! |---|---|---|
//! | 422 / 424 | `Ptp` → `handleHintReject` | do it — client microcompacts and retries |
//! | 400 + `Unexpected value` + `anthropic-beta` | `Mtp` | beta unsupported — strip it, compact NOTHING |
//! | 409 | `Ltp` | server busy — fall back, compact nothing |
//! | 529 | `is529Error` | overloaded — fall back, compact nothing |
//! | stream `invalid_request_error` with NO status | `Htp` | classify, then act on the non-streaming fallback |
//!
//! # Gate — OFF by default, deliberately
//!
//! The oracle's gate is `Ke("tengu_hazel_osprey", !1)`, whose binary default is
//! FALSE and which this machine's `cachedGrowthBookFeatures` shows the server
//! delivering as `false` — so the whole controller is inert in real Claude Code
//! today (Anthropic is staging it: the companion floor `tengu_hazel_osprey_floor`
//! IS delivered, as 75000). LingXi therefore ships it register-but-disable
//! behind [`CONTEXT_HINT_ENV`], default off, exactly like other flag-gated
//! ports. Turning it on by default would make LingXi send a body field the
//! oracle never sends — anti-parity, not parity.
//!
//! # Where the facts come from
//!
//! [`HttpErrorFacts`] used to be unfillable: `map_error_status` sent
//! `400 | 422` to one `LlmError` variant and 424/409 both to `ProviderInternal`,
//! so the four classifiers could not tell apart the cases the oracle must.
//!
//! That is fixed at the source rather than here. Provider decoders now store
//! the SDK's `${status} ${body}` text (`providers::api_error_message`), which is
//! how claude-code carries status provenance in the first place, and
//! [`LlmError::http_status`] parses it back off. [`HttpErrorFacts::from_error`]
//! builds the facts from any decoded error.

use crate::microcompact::{
    estimate_keep_recent, Microcompactor, TimeBasedMCConfig, MICROCOMPACT_MIN_TOKENS_SAVED,
};
use lingxi_core::types::ConversationMessage;
use std::collections::HashSet;
use std::time::SystemTime;

/// Env gate standing in for the oracle's `tengu_hazel_osprey`. Any non-empty
/// value that is not `"0"`/`"false"` turns the controller on; unset (the
/// default) leaves it inert.
pub const CONTEXT_HINT_ENV: &str = "LINGXI_CONTEXT_HINT";

/// The beta header the hint rides on — oracle `_9i = Wv("context_hint",
/// "context-hint-2026-04-09")`.
pub const CONTEXT_HINT_BETA_HEADER: &str = "context-hint-2026-04-09";

/// The beta's registry name (the first half of `Wv`).
pub const CONTEXT_HINT_BETA_NAME: &str = "context_hint";

/// `target_tokens_saved` sent to the server — oracle `QMy = 75000`, the default
/// of the `tengu_hazel_osprey_floor` gate. Omitted from the body when zero.
pub const CONTEXT_HINT_TARGET_TOKENS_SAVED: u64 = 75_000;

/// How many recent tool results the hint's microcompact keeps — oracle
/// `$tp = 5`. NOTE this is the CONTROLLER's keep-recent, independent of
/// [`TimeBasedMCConfig::keep_recent`]'s own default.
pub const CONTEXT_HINT_KEEP_RECENT: usize = 5;

/// Is the context-hint controller enabled? Mirrors `Dtp()`.
#[must_use]
pub fn context_hint_enabled() -> bool {
    match std::env::var(CONTEXT_HINT_ENV) {
        Ok(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

/// Use the same per-block token estimator as compaction and PTL retry (2.1.261
/// `Og`), including nested results and JS UTF-16 text lengths.
#[must_use]
fn estimate_message_tokens(messages: &[ConversationMessage]) -> u64 {
    crate::grouping::estimate_tokens_for_range(messages)
}

/// The HTTP facts the oracle's four classifiers key on.
///
/// A struct rather than `LlmError` because `LlmError` no longer carries them —
/// see the module docs. Populating this is the caller's job.
#[derive(Debug, Clone, Default)]
pub struct HttpErrorFacts {
    /// HTTP status, when the error reached the client as a response.
    pub status: Option<u16>,
    /// `error.error.type` from the provider envelope, e.g.
    /// `"invalid_request_error"`.
    pub error_type: Option<String>,
    /// The error message, matched verbatim by [`is_unsupported_beta`].
    pub message: String,
    /// Provider request id, echoed into telemetry.
    pub request_id: Option<String>,
    /// Whether the caller classified this as a 529 overload.
    pub is_overloaded: bool,
}

impl HttpErrorFacts {
    /// Build the facts from a decoded [`LlmError`].
    ///
    /// `status` comes from the `${status} ` prefix the provider decoders write;
    /// `message` is the decoded text itself, which is what
    /// [`is_unsupported_beta`] matches on. `is_overloaded` covers the oracle's
    /// `is529Error`, which is a separate predicate there because a 529 never
    /// reaches the status branches.
    ///
    /// `error_type` stays `None` here: it is only set on the STREAM path, where
    /// the envelope arrives with an `invalid_request_error` type and no status
    /// at all (oracle `Htp`). Callers on that path fill it in themselves.
    #[must_use]
    pub fn from_error(err: &llm_runtime::LlmError) -> Self {
        Self {
            status: err.http_status(),
            error_type: None,
            message: err.to_string(),
            request_id: None,
            is_overloaded: matches!(err, llm_runtime::LlmError::Overloaded { .. }),
        }
    }
}

/// `Ptp` — the server ASKS for the compact. 422 or 424.
#[must_use]
pub fn is_hint_reject(f: &HttpErrorFacts) -> bool {
    matches!(f.status, Some(422 | 424))
}

/// `Htp` — a STREAM error that means the same thing: an `invalid_request_error`
/// envelope that arrived with NO status. The status check is `!==void 0`, so an
/// error carrying any status (even 422) is NOT this case.
#[must_use]
pub fn is_stream_hint_reject(f: &HttpErrorFacts) -> bool {
    f.status.is_none() && f.error_type.as_deref() == Some("invalid_request_error")
}

/// `Ltp` — the server is busy. 409.
#[must_use]
pub fn is_hint_busy(f: &HttpErrorFacts) -> bool {
    f.status == Some(409)
}

/// `Mtp` — the endpoint does not know this beta: 400 whose message mentions
/// BOTH `Unexpected value` and `anthropic-beta`. Strip the beta and send the
/// messages through UNCHANGED; do not compact.
#[must_use]
pub fn is_unsupported_beta(f: &HttpErrorFacts) -> bool {
    f.status == Some(400)
        && f.message.contains("Unexpected value")
        && f.message.contains("anthropic-beta")
}

/// What [`ContextHintController::build_request_params`] contributes to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextHintRequestParams {
    /// Beta header to add — always [`CONTEXT_HINT_BETA_HEADER`].
    pub beta: &'static str,
    /// The `context_hint` body object, or `None` when the estimated savings are
    /// below the floor. The oracle still sends the BETA in that case and only
    /// omits the body (`body: s ? {...} : null`).
    pub body: Option<serde_json::Value>,
}

/// Telemetry payload for `tengu_context_hint_reject` (oracle `Ftp`).
///
/// Returned rather than emitted: this crate has no analytics dependency and
/// emits no events of its own today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextHintRejectEvent {
    /// Provider request id.
    pub request_id: Option<String>,
    /// Token estimate before the edits.
    pub pre_compact_token_estimate: u64,
    /// Token estimate after the edits.
    pub post_compact_token_estimate: u64,
    /// `pre - post`, as the oracle computes it at the call site.
    pub tokens_saved: u64,
    /// Whether a microcompact actually ran.
    pub mc_applied: bool,
    /// Tokens the microcompact itself reported saving.
    pub mc_tokens_saved: u64,
}

/// Telemetry payload for `tengu_context_hint_busy_fallback` (oracle `JBo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextHintBusyEvent {
    /// Provider request id.
    pub request_id: Option<String>,
    /// The status that caused the fallback: 400, 409 or 529.
    pub status: u16,
}

/// The `[CONTEXT_HINT_REJECT]` log line — oracle `jtp`'s
/// ``w(`[CONTEXT_HINT_REJECT] mc=${!!n} tokensSaved=${n?.tokensSaved??0}`)``.
///
/// A function, not an inline `format!`, so the exact bytes are assertable. The
/// oracle logs this UNCONDITIONALLY — including the below-floor case, where it
/// reads `mc=false tokensSaved=0`. An early return that skips it is a silent
/// byte-level divergence (this port had one).
#[must_use]
pub fn hint_reject_log_line(mc_applied: bool, tokens_saved: u64) -> String {
    format!("[CONTEXT_HINT_REJECT] mc={mc_applied} tokensSaved={tokens_saved}")
}

/// The `[KEEP-RECENT MC]` log line as `qsd` emits it on the context-hint
/// trigger: ``[KEEP-RECENT MC] context_hint trigger, cleared ${s.size} tool
/// results (~${o} tokens), kept last ${n.size}``.
///
/// Note `(~` and the comma placement — both are easy to paraphrase and both are
/// load-bearing for a log-scraping comparison.
#[must_use]
pub fn keep_recent_mc_log_line(cleared: usize, tokens_saved: u64, kept: usize) -> String {
    format!(
        "[KEEP-RECENT MC] context_hint trigger, cleared {cleared} tool results (~{tokens_saved} tokens), kept last {kept}"
    )
}

/// Result of `applyHintEdits` (`jtp`).
#[derive(Debug, Clone)]
pub struct HintEdits {
    /// Messages after the edits — the ORIGINAL list when no compact applied.
    pub messages: Vec<ConversationMessage>,
    /// Tool-use ids whose results were cleared. EMPTY when nothing applied
    /// (oracle `Utp = new Set`).
    pub cleared_ids: HashSet<lingxi_core::types::ToolUseId>,
    /// Whether a microcompact ran.
    pub mc_applied: bool,
    /// Tokens the microcompact reported saving.
    pub mc_tokens_saved: u64,
    /// Token estimate before the edits.
    pub pre_compact_token_estimate: u64,
    /// Token estimate after the edits.
    pub post_compact_token_estimate: u64,
    /// The `[CONTEXT_HINT_REJECT]` line that was logged for these edits.
    ///
    /// Surfaced for the same reason the telemetry payloads are returned rather
    /// than emitted: this crate has no analytics dependency and no log-capture
    /// harness, so a line that is only handed to `tracing` cannot be asserted.
    /// Dropping the no-op branch's log is otherwise an invisible regression —
    /// it was one, until this field made it testable.
    pub log_line: String,
}

/// `applyHintEdits` (`jtp`) — run the keep-recent microcompact the hint promised.
///
/// # Divergence (reason)
/// The oracle passes `persist: ZMy`, which writes each cleared tool result to
/// the session's `tool-results/` directory and substitutes
/// `<persisted-output>Tool result saved to: …</persisted-output>` instead of the
/// bare placeholder. LingXi's [`Microcompactor`] has no persist callback — a
/// SEPARATE, already-tracked gap ("keep-recent microcompact persist callback").
/// Cleared results therefore get [`crate::microcompact::TIME_BASED_MC_CLEARED_MESSAGE`]
/// here. Wiring persist belongs with that gap, not this one; doing it inside
/// this module would fork a second persistence path.
#[must_use]
pub fn apply_hint_edits(messages: Vec<ConversationMessage>) -> HintEdits {
    apply_hint_edits_with_persisted(messages, &std::collections::HashMap::new())
}

/// [`apply_hint_edits`] with the persist step's results folded in.
///
/// `persisted` maps a cleared result's id to the `<persisted-output>Tool result
/// saved to: …</persisted-output>` substitution — build it with
/// [`crate::microcompact::keep_recent_persist_candidates`] plus
/// `orchestrator::tool_result_persistence`. An EMPTY map is the pre-persist
/// behaviour exactly: every cleared result gets the bare placeholder.
#[must_use]
pub fn apply_hint_edits_with_persisted(
    messages: Vec<ConversationMessage>,
    persisted: &std::collections::HashMap<lingxi_core::types::ToolUseId, String>,
) -> HintEdits {
    let pre = estimate_message_tokens(&messages);
    let estimate = estimate_keep_recent(&messages, CONTEXT_HINT_KEEP_RECENT);

    // `qsd` returns null below the floor, and `jtp` then keeps the ORIGINAL
    // messages (`o = n ? n.messages : e`) with empty cleared sets.
    if estimate.clear_set.is_empty() || estimate.tokens_saved < MICROCOMPACT_MIN_TOKENS_SAVED {
        // Logged here too: `jtp` calls `w(...)` after the `qsd` null-check, not
        // inside it, so the no-op case still reports `mc=false tokensSaved=0`.
        let log_line = hint_reject_log_line(false, 0);
        tracing::debug!("{}", log_line);
        return HintEdits {
            log_line,
            cleared_ids: HashSet::new(),
            mc_applied: false,
            mc_tokens_saved: 0,
            pre_compact_token_estimate: pre,
            post_compact_token_estimate: pre,
            messages,
        };
    }

    let compactor = Microcompactor {
        config: TimeBasedMCConfig {
            enabled: true,
            keep_recent: CONTEXT_HINT_KEEP_RECENT,
            ..TimeBasedMCConfig::default()
        },
    };
    let result = compactor.compact_with_persisted(messages, SystemTime::now(), persisted);
    let post = estimate_message_tokens(&result.messages);
    // `qsd`'s own line fires first (it logs inside the compact), then `jtp`'s.
    tracing::debug!(
        "{}",
        keep_recent_mc_log_line(
            result.cleared_count,
            result.tokens_saved,
            estimate.keep_set.len()
        )
    );
    let log_line = hint_reject_log_line(true, result.tokens_saved);
    tracing::debug!("{}", log_line);
    HintEdits {
        log_line,
        messages: result.messages,
        cleared_ids: estimate.candidate_ids,
        mc_applied: true,
        mc_tokens_saved: result.tokens_saved,
        pre_compact_token_estimate: pre,
        post_compact_token_estimate: post,
    }
}

/// `handleHintReject` (`b8s`) — apply the edits and build the telemetry payload.
///
/// `tokens_saved` on the event is `pre - post`, which is NOT the same number as
/// `mc_tokens_saved`: the first measures the whole message list, the second only
/// the cleared tool results.
#[must_use]
pub fn handle_hint_reject(
    messages: Vec<ConversationMessage>,
    request_id: Option<String>,
) -> (HintEdits, ContextHintRejectEvent) {
    handle_hint_reject_with_persisted(messages, request_id, &std::collections::HashMap::new())
}

/// [`handle_hint_reject`] carrying the persist step's results.
#[must_use]
pub fn handle_hint_reject_with_persisted(
    messages: Vec<ConversationMessage>,
    request_id: Option<String>,
    persisted: &std::collections::HashMap<lingxi_core::types::ToolUseId, String>,
) -> (HintEdits, ContextHintRejectEvent) {
    let edits = apply_hint_edits_with_persisted(messages, persisted);
    let event = ContextHintRejectEvent {
        request_id,
        pre_compact_token_estimate: edits.pre_compact_token_estimate,
        post_compact_token_estimate: edits.post_compact_token_estimate,
        tokens_saved: edits
            .pre_compact_token_estimate
            .saturating_sub(edits.post_compact_token_estimate),
        mc_applied: edits.mc_applied,
        mc_tokens_saved: edits.mc_tokens_saved,
    };
    (edits, event)
}

/// What the controller decided a request error means.
#[derive(Debug, Clone)]
pub enum HintErrorOutcome {
    /// Apply the edits and retry (422/424).
    Reject(Box<HintEdits>, ContextHintRejectEvent),
    /// Strip the beta; messages pass through UNCHANGED (400 unsupported beta).
    StripBeta(ContextHintBusyEvent),
    /// Server busy or overloaded; no edits (409 / 529).
    Busy(ContextHintBusyEvent),
    /// Not a context-hint error — the caller handles it normally.
    NotHandled,
}

/// The per-request state machine — oracle `e1y`'s returned object.
///
/// Three latches, exactly as the oracle keeps them: `done` (its `r`) makes every
/// hook a no-op once the controller has acted or been stripped, `sent` (its `n`)
/// records whether THIS request actually carried the hint, and
/// `stream_classified` (its `o`) carries a stream-error classification forward to
/// the non-streaming fallback.
#[derive(Debug)]
pub struct ContextHintController {
    active: bool,
    sent: bool,
    done: bool,
    stream_classified: bool,
}

/// `createContextHintController` (`e1y`).
///
/// Returns `None` — no controller at all — unless first-party betas are included
/// AND the query source starts with `repl_main_thread`. Subagents, SDK callers
/// and side queries never negotiate.
#[must_use]
pub fn create_context_hint_controller(
    include_first_party_betas: bool,
    query_source: &str,
) -> Option<ContextHintController> {
    if !include_first_party_betas {
        return None;
    }
    if !query_source.starts_with("repl_main_thread") {
        return None;
    }
    Some(ContextHintController {
        active: context_hint_enabled(),
        sent: false,
        done: false,
        stream_classified: false,
    })
}

impl ContextHintController {
    /// Whether the gate is on. A controller can exist but be inactive — that is
    /// the default state today.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// `buildRequestParams` — what to add to the outgoing request.
    ///
    /// Resets `sent` FIRST (the oracle's `n=!1` before the guards), so a request
    /// built while inactive or finished correctly records that no hint rode on
    /// it and the error hooks stand down.
    pub fn build_request_params(
        &mut self,
        messages: &[ConversationMessage],
    ) -> Option<ContextHintRequestParams> {
        self.sent = false;
        if !self.active || self.done {
            return None;
        }
        self.sent = true;
        let worth_it = estimate_keep_recent(messages, CONTEXT_HINT_KEEP_RECENT).tokens_saved
            >= MICROCOMPACT_MIN_TOKENS_SAVED;
        let body = worth_it.then(|| {
            let mut hint = serde_json::Map::new();
            hint.insert("enabled".into(), serde_json::Value::Bool(true));
            if CONTEXT_HINT_TARGET_TOKENS_SAVED > 0 {
                hint.insert(
                    "target_tokens_saved".into(),
                    serde_json::Value::from(CONTEXT_HINT_TARGET_TOKENS_SAVED),
                );
            }
            serde_json::json!({ "context_hint": serde_json::Value::Object(hint) })
        });
        Some(ContextHintRequestParams {
            beta: CONTEXT_HINT_BETA_HEADER,
            body,
        })
    }

    /// `onRequestError` — classify a non-streaming failure.
    ///
    /// Every handled branch latches `done`, so one request gets at most one
    /// hint-driven compact.
    pub fn on_request_error(
        &mut self,
        facts: &HttpErrorFacts,
        messages: Vec<ConversationMessage>,
    ) -> HintErrorOutcome {
        self.on_request_error_with_persisted(facts, messages, &std::collections::HashMap::new())
    }

    /// [`Self::on_request_error`] carrying the persist step's results, used by
    /// the turn loop after it has written the about-to-be-cleared results to
    /// the session's `tool-results/` directory.
    pub fn on_request_error_with_persisted(
        &mut self,
        facts: &HttpErrorFacts,
        messages: Vec<ConversationMessage>,
        persisted: &std::collections::HashMap<lingxi_core::types::ToolUseId, String>,
    ) -> HintErrorOutcome {
        if !self.sent || self.done {
            return HintErrorOutcome::NotHandled;
        }
        let request_id = facts.request_id.clone();
        if is_hint_reject(facts) {
            self.done = true;
            let (edits, event) = handle_hint_reject_with_persisted(messages, request_id, persisted);
            return HintErrorOutcome::Reject(Box::new(edits), event);
        }
        if is_unsupported_beta(facts) {
            self.done = true;
            return HintErrorOutcome::StripBeta(ContextHintBusyEvent {
                request_id,
                status: 400,
            });
        }
        if is_hint_busy(facts) {
            self.done = true;
            return HintErrorOutcome::Busy(ContextHintBusyEvent {
                request_id,
                status: 409,
            });
        }
        if facts.is_overloaded {
            self.done = true;
            return HintErrorOutcome::Busy(ContextHintBusyEvent {
                request_id,
                status: 529,
            });
        }
        HintErrorOutcome::NotHandled
    }

    /// `classifyStreamError` — record whether a stream failure looks like a hint
    /// reject, WITHOUT acting on it. The action happens in
    /// [`Self::on_stream_fallback`].
    pub fn classify_stream_error(&mut self, facts: &HttpErrorFacts) -> bool {
        self.stream_classified = false;
        if !self.sent || self.done {
            return false;
        }
        if !is_stream_hint_reject(facts) {
            return false;
        }
        self.stream_classified = true;
        true
    }

    /// `onStreamFallback` — act on a previously classified stream error.
    ///
    /// Latches `done` UNCONDITIONALLY, even when nothing was classified: after
    /// a fallback the controller is spent either way.
    pub fn on_stream_fallback(
        &mut self,
        messages: Vec<ConversationMessage>,
        request_id: Option<String>,
    ) -> Option<(HintEdits, ContextHintRejectEvent)> {
        let classified = self.stream_classified;
        self.done = true;
        classified.then(|| handle_hint_reject(messages, request_id))
    }

    /// `strip` — retire the controller without acting.
    pub fn strip(&mut self) {
        self.done = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};

    /// `n` compactable tool_use/tool_result pairs, each result big enough that
    /// clearing all but the last 5 clears the 20 000-token floor.
    fn history(n: usize, result_len: usize) -> Vec<ConversationMessage> {
        let mut out = Vec::new();
        for i in 0..n {
            let id = ToolUseId::from(format!("t{i}"));
            out.push(ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: id.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                    provider_id: None,
                }],
                stop_reason: None,
            });
            out.push(ConversationMessage::User { api_message_override: None,
                id: MessageId::new(),
                content: vec![ContentBlock::ToolResult { content_projection: None,
                    tool_use_id: id,
                    content: "x".repeat(result_len),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            });
        }
        out
    }

    /// Big enough to clear the floor: 10 pairs, 5 cleared, ~40k chars each.
    fn big_history() -> Vec<ConversationMessage> {
        history(10, 40_000)
    }

    fn active_controller() -> ContextHintController {
        ContextHintController {
            active: true,
            sent: false,
            done: false,
            stream_classified: false,
        }
    }

    #[test]
    fn controller_is_only_created_for_the_first_party_main_thread() {
        assert!(create_context_hint_controller(true, "repl_main_thread").is_some());
        assert!(create_context_hint_controller(true, "repl_main_thread_fallback").is_some());
        assert!(
            create_context_hint_controller(false, "repl_main_thread").is_none(),
            "no first-party betas, no negotiation"
        );
        assert!(
            create_context_hint_controller(true, "sdk").is_none(),
            "only the main REPL thread negotiates"
        );
    }

    #[test]
    fn an_inactive_controller_contributes_nothing() {
        // The DEFAULT state: gate off, so no beta and no body ever go out.
        let mut c = ContextHintController {
            active: false,
            sent: false,
            done: false,
            stream_classified: false,
        };
        assert!(c.build_request_params(&big_history()).is_none());
    }

    #[test]
    fn the_hint_body_is_sent_only_when_the_savings_clear_the_floor() {
        let mut c = active_controller();
        let params = c
            .build_request_params(&big_history())
            .expect("active controller always contributes the beta");
        assert_eq!(params.beta, CONTEXT_HINT_BETA_HEADER);
        assert_eq!(
            params.body,
            Some(serde_json::json!({
                "context_hint": { "enabled": true, "target_tokens_saved": 75_000 }
            }))
        );

        // Tiny history → nothing worth offering. The BETA still rides along;
        // only the body is dropped (`body: s ? {...} : null`).
        let mut c = active_controller();
        let params = c
            .build_request_params(&history(10, 8))
            .expect("the beta is still contributed");
        assert_eq!(params.beta, CONTEXT_HINT_BETA_HEADER);
        assert_eq!(params.body, None, "below the floor, no body");
    }

    #[test]
    fn a_422_asks_the_client_to_compact_and_retry() {
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);

        let facts = HttpErrorFacts {
            status: Some(422),
            request_id: Some("req_1".into()),
            ..HttpErrorFacts::default()
        };
        match c.on_request_error(&facts, msgs) {
            HintErrorOutcome::Reject(edits, event) => {
                assert!(edits.mc_applied, "422 must actually compact");
                assert_eq!(
                    edits.log_line,
                    format!(
                        "[CONTEXT_HINT_REJECT] mc=true tokensSaved={}",
                        edits.mc_tokens_saved
                    )
                );
                assert!(!edits.cleared_ids.is_empty());
                assert_eq!(event.request_id.as_deref(), Some("req_1"));
                assert!(event.post_compact_token_estimate < event.pre_compact_token_estimate);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    /// End to end from a REAL decoded provider error — the step that was
    /// impossible before the status prefix existed. A 422 and a 400 used to
    /// decode to the same `LlmError::InvalidRequest`, so no classifier could
    /// separate "compact and retry" from "strip the beta".
    #[test]
    fn facts_come_back_out_of_a_real_decoded_error() {
        let reject = llm_runtime::LlmError::InvalidRequest {
            message: r#"422 {"type":"error","error":{"message":"context hint"}}"#.to_string(),
        };
        let facts = HttpErrorFacts::from_error(&reject);
        assert_eq!(facts.status, Some(422));
        assert!(
            is_hint_reject(&facts),
            "422 must reach the compact-and-retry arm"
        );
        assert!(!is_unsupported_beta(&facts));

        let unsupported = llm_runtime::LlmError::InvalidRequest {
            message: "400 Unexpected value for the anthropic-beta header".to_string(),
        };
        let facts = HttpErrorFacts::from_error(&unsupported);
        assert_eq!(facts.status, Some(400));
        assert!(is_unsupported_beta(&facts), "400 must reach the strip arm");
        assert!(
            !is_hint_reject(&facts),
            "the two must NOT collapse together again"
        );

        // 529 has no status branch in the oracle either — it is its own predicate.
        let overloaded = llm_runtime::LlmError::Overloaded { repeated: false };
        assert!(HttpErrorFacts::from_error(&overloaded).is_overloaded);
    }

    /// A driven controller run using only decoded errors.
    #[test]
    fn a_decoded_422_drives_the_controller_to_reject() {
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);
        let err = llm_runtime::LlmError::InvalidRequest {
            message: r#"422 {"type":"error"}"#.to_string(),
        };
        match c.on_request_error(&HttpErrorFacts::from_error(&err), msgs) {
            HintErrorOutcome::Reject(edits, _) => assert!(edits.mc_applied),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn a_424_is_a_hint_reject_too_but_a_423_is_not() {
        assert!(is_hint_reject(&HttpErrorFacts {
            status: Some(424),
            ..HttpErrorFacts::default()
        }));
        assert!(!is_hint_reject(&HttpErrorFacts {
            status: Some(423),
            ..HttpErrorFacts::default()
        }));
    }

    #[test]
    fn an_unsupported_beta_strips_without_compacting() {
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);

        let facts = HttpErrorFacts {
            status: Some(400),
            message:
                "Unexpected value(s) `context-hint-2026-04-09` for the `anthropic-beta` header"
                    .into(),
            ..HttpErrorFacts::default()
        };
        assert!(matches!(
            c.on_request_error(&facts, msgs),
            HintErrorOutcome::StripBeta(_)
        ));

        // A 400 that does NOT name the beta header is somebody else's problem.
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);
        let other = HttpErrorFacts {
            status: Some(400),
            message: "messages.0: invalid role".into(),
            ..HttpErrorFacts::default()
        };
        assert!(matches!(
            c.on_request_error(&other, msgs),
            HintErrorOutcome::NotHandled
        ));
    }

    #[test]
    fn busy_and_overloaded_fall_back_without_editing() {
        for (facts, want) in [
            (
                HttpErrorFacts {
                    status: Some(409),
                    ..HttpErrorFacts::default()
                },
                409u16,
            ),
            (
                HttpErrorFacts {
                    is_overloaded: true,
                    ..HttpErrorFacts::default()
                },
                529,
            ),
        ] {
            let mut c = active_controller();
            let msgs = big_history();
            c.build_request_params(&msgs);
            match c.on_request_error(&facts, msgs) {
                HintErrorOutcome::Busy(e) => assert_eq!(e.status, want),
                other => panic!("expected Busy({want}), got {other:?}"),
            }
        }
    }

    #[test]
    fn an_error_on_a_request_that_carried_no_hint_is_not_handled() {
        // `!n` — the controller never offered anything, so a 422 from some
        // other cause must not trigger a compact.
        let mut c = active_controller();
        let facts = HttpErrorFacts {
            status: Some(422),
            ..HttpErrorFacts::default()
        };
        assert!(matches!(
            c.on_request_error(&facts, big_history()),
            HintErrorOutcome::NotHandled
        ));
    }

    #[test]
    fn the_controller_acts_at_most_once() {
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);
        let facts = HttpErrorFacts {
            status: Some(422),
            ..HttpErrorFacts::default()
        };
        assert!(matches!(
            c.on_request_error(&facts, msgs.clone()),
            HintErrorOutcome::Reject(..)
        ));
        // `r=!0` latched: a second failure is no longer ours, and
        // `buildRequestParams` stops contributing.
        assert!(matches!(
            c.on_request_error(&facts, msgs.clone()),
            HintErrorOutcome::NotHandled
        ));
        assert!(c.build_request_params(&msgs).is_none());
    }

    #[test]
    fn a_statusless_invalid_request_stream_error_is_classified_then_acted_on() {
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);

        let facts = HttpErrorFacts {
            status: None,
            error_type: Some("invalid_request_error".into()),
            ..HttpErrorFacts::default()
        };
        assert!(c.classify_stream_error(&facts));
        let (edits, _) = c
            .on_stream_fallback(msgs, Some("req_s".into()))
            .expect("a classified stream error compacts on fallback");
        assert!(edits.mc_applied);
    }

    #[test]
    fn a_stream_error_carrying_a_status_is_not_a_hint_reject() {
        // `if(e.status!==void 0)return!1` — even a 422 fails this check, because
        // the streaming path only ever sees the statusless envelope.
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);
        assert!(!c.classify_stream_error(&HttpErrorFacts {
            status: Some(422),
            error_type: Some("invalid_request_error".into()),
            ..HttpErrorFacts::default()
        }));
        assert!(
            c.on_stream_fallback(msgs, None).is_none(),
            "nothing classified, nothing applied"
        );
    }

    #[test]
    fn an_unclassified_stream_fallback_still_retires_the_controller() {
        let mut c = active_controller();
        let msgs = big_history();
        c.build_request_params(&msgs);
        assert!(c.on_stream_fallback(msgs.clone(), None).is_none());
        assert!(
            c.build_request_params(&msgs).is_none(),
            "`r=!0` runs before the `a` check, so the controller is spent"
        );
    }

    #[test]
    fn apply_hint_edits_below_the_floor_changes_nothing() {
        let msgs = history(10, 8);
        let before = msgs.clone();
        let edits = apply_hint_edits(msgs);
        assert!(!edits.mc_applied);
        assert!(edits.cleared_ids.is_empty());
        assert_eq!(edits.messages.len(), before.len());
        assert_eq!(
            edits.pre_compact_token_estimate, edits.post_compact_token_estimate,
            "no edits, no change in the estimate"
        );
        assert_eq!(
            edits.log_line, "[CONTEXT_HINT_REJECT] mc=false tokensSaved=0",
            "the oracle logs this branch too — `w(...)` sits after the null-check, not inside it"
        );
    }

    /// Byte-exact log lines. The oracle's are template literals, so a
    /// paraphrase ("cleared 3 tool results (~40000 tokens)" vs "~40000") is
    /// invisible until someone diffs logs across the two clients.
    #[test]
    fn log_lines_are_byte_exact() {
        assert_eq!(
            hint_reject_log_line(true, 40_000),
            "[CONTEXT_HINT_REJECT] mc=true tokensSaved=40000"
        );
        // The no-op shape the oracle still emits — `mc=${!!n}` on a null `n`.
        assert_eq!(
            hint_reject_log_line(false, 0),
            "[CONTEXT_HINT_REJECT] mc=false tokensSaved=0"
        );
        assert_eq!(
            keep_recent_mc_log_line(3, 40_000, 5),
            "[KEEP-RECENT MC] context_hint trigger, cleared 3 tool results (~40000 tokens), kept last 5"
        );
    }

    #[test]
    fn strip_retires_the_controller() {
        let mut c = active_controller();
        c.strip();
        assert!(c.build_request_params(&big_history()).is_none());
    }
}
