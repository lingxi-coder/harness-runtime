use crate::conversation::{ApiErrorEnvelope, ConversationOrchestrator};
use crate::error::OrchestratorError;
#[doc = " Byte-exact user-facing message surfaced when the prompt is too long and the"]
#[doc = " reactive 413 recovery (Batch 5) is exhausted. 1:1 with claude-code"]
#[doc = " `errors.ts` `PROMPT_TOO_LONG_ERROR_MESSAGE = 'Prompt is too long'`."]
#[doc = ""]
#[doc = " Re-exported from `model::prompt_too_long` (the orchestrator's own copy),"]
#[doc = " which is the authoritative source for this string in this crate."]
use crate::model::prompt_too_long::PROMPT_TOO_LONG_ERROR_MESSAGE;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use llm_runtime::LlmError;

/// Append the byte-exact [`PROMPT_TOO_LONG_ERROR_MESSAGE`] as an assistant text
/// message to history (and emit it to the output stream), returning its id so
/// the caller can end the turn. Mirrors the TS path where the prompt-too-long
/// error is surfaced as the assistant turn before the loop terminates.
///
/// `pub(crate)` so the streaming turn driver's RECOV.1 blocking-limit preempt
/// (`conversation.rs`) can surface the same byte-exact message as the batched
/// path before ending the turn.
///
/// SC-04 (claude-code 2.1.238): the content is `Fol(compactFailure) ?? _V` —
/// when the rescue compaction for THIS call failed, the message becomes
/// `Prompt is too long · automatic compaction failed: <first line, ≤300 cols>`
/// (`ep({content:Fol(qn)??_V,error:"invalid_request",…})`, cc-238.js
/// @228721216 and its reactive twin @228749977). With no recorded failure the
/// bare [`PROMPT_TOO_LONG_ERROR_MESSAGE`] is surfaced exactly as before. The
/// detail is CONSUMED here (one-shot), so a later turn can never inherit it.
/// SLASH-04 (NEW in claude-code 2.1.238) — the `/goal` auto-teardown on a turn
/// that died for a reason the user cannot retry past. Oracle `Cqf`
/// (@292182815, statsig `tengu_quartz_pipit`, **default `true`**):
///
/// ```js
/// function*Cqf(e,t,r,n){try{
///   if(!it("tengu_quartz_pipit",!0)||!e||t.agentId||t.abortController.signal.aborted||PH(r)!=="main")return;
///   let o=w4v(n);if(o===null)return;let{label:i,errorCode:s}=v4v[o];
///   t.sessionHooksRegistry.remove(zt(),"Stop",{type:"prompt",prompt:e.condition}),
///   cFe(e,o==="context_limit"?"context_limit":"api_error"),de("goal_met",s),
///   yield{type:"active_goal",value:void 0},yield bOi(!0,e.condition),
///   yield jBt(`Goal cleared after an unrecoverable error (${i}): "${Yl(e.condition,T4v,!0)}". Run /goal again to continue.`,"warning")
/// }catch(o){Ce(o)}}
/// ```
///
/// The bucket map is `w4v` (@292182951), read verbatim:
///
/// ```js
/// switch(e.reason){
///   case"image_error":case"model_error":case"malformed_tool_use_exhausted":
///   case"aborted_streaming":case"aborted_tools":case"stop_hook_prevented":
///   case"hook_stopped":case"tool_deferred":case"max_turns":
///   case"background_requested":case"completed":return null;
///   case"blocking_limit":case"prompt_too_long":case"rapid_refill_breaker":return"context_limit";
///   case"api_error":if(e.isTransient)return null;
///     switch(e.errorKind){
///       case"overloaded":case"server_error":case"max_output_tokens":case"rate_limit":
///       case"invalid_request":case"unknown":case void 0:return null;
///       case"authentication_failed":case"oauth_org_not_allowed":
///         return V.CLAUDE_CODE_REMOTE||j2()||BYt()!==null?null:"auth";
///       case"account_on_hold":return"auth";
///       case"billing_error":return"billing";
///       case"model_not_found":return"model_unavailable"}}
/// ```
///
/// NAMING NOTE — the oracle's `api_error` reason is spelled `model_error` in
/// this port. Upstream, the graceful api-error catch produces an
/// `isApiErrorMessage` assistant message and the loop then returns
/// `{reason:"api_error",errorKind:Wr.error,isTransient:mal(Wr)}` (@292258007);
/// the oracle's OWN `model_error` reason is the unrelated `model_blocked`
/// / queryLoop-invariant arm (@292249827). LingXi's `Err(e)` arm below is the
/// FORMER, so it is classified with [`GoalClearReason::ApiError`] carrying
/// `classify_api_error`'s category as `errorKind` — not with the oracle's
/// no-clear `model_error` case. Getting this backwards would make the whole
/// api-error family silently non-clearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalClearBucket {
    /// `auth` — "authentication failed" / `cleared_auth`.
    Auth,
    /// `billing` — "credit balance too low" / `cleared_billing`.
    Billing,
    VerificationRequired,
    /// `context_limit` — "context limit reached" / `cleared_context_limit`.
    ContextLimit,
    /// `model_unavailable` — "model unavailable" / `cleared_model_unavailable`.
    ModelUnavailable,
}

impl GoalClearBucket {
    /// `v4v[o].label` (@292183854) — interpolated into the warning's parens.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auth => "authentication failed",
            Self::Billing => "credit balance too low",
            Self::VerificationRequired => "organization verification required",
            Self::ContextLimit => "context limit reached",
            Self::ModelUnavailable => "model unavailable",
        }
    }

    /// `v4v[o].errorCode` — the `de("goal_met", s)` telemetry property.
    pub(crate) fn error_code(self) -> &'static str {
        match self {
            Self::Auth => "cleared_auth",
            Self::Billing => "cleared_billing",
            Self::VerificationRequired => "cleared_verification_required",
            Self::ContextLimit => "cleared_context_limit",
            Self::ModelUnavailable => "cleared_model_unavailable",
        }
    }
}

/// The terminal-reason shape `w4v` switches on, narrowed to the reasons this
/// port's turn loop can actually produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalClearReason<'a> {
    /// `blocking_limit` / `prompt_too_long` / `rapid_refill_breaker`.
    ContextLimit,
    /// The oracle's `api_error` (this port's `model_error` stop_reason), with
    /// `classify_api_error`'s category as `errorKind` and `mal(Wr)`'s verdict.
    ApiError {
        /// `e.errorKind` — `None` maps to the `case void 0` no-clear arm.
        error_kind: Option<&'a str>,
        /// `e.isTransient` = `mal(Wr)` (@296630038):
        /// `e.apiErrorIsTransient===!0||e.error==="overloaded"||e.error==="server_error"`.
        is_transient: bool,
    },
}

/// `w4v(n)` — reason (+ errorKind) to bucket, or `None` for "do not clear".
pub(crate) fn goal_clear_bucket(reason: GoalClearReason<'_>) -> Option<GoalClearBucket> {
    match reason {
        GoalClearReason::ContextLimit => Some(GoalClearBucket::ContextLimit),
        GoalClearReason::ApiError { is_transient, .. } if is_transient => None,
        GoalClearReason::ApiError { error_kind, .. } => match error_kind {
            // `authentication_failed | oauth_org_not_allowed` clear UNLESS the
            // session is remote. LingXi has no remote surface (an accepted
            // divergence), so only the env half of `CLAUDE_CODE_REMOTE ||
            // j2() || BYt()!==null` is observable — and it is what an operator
            // can actually set.
            Some("authentication_failed" | "oauth_org_not_allowed") => {
                if std::env::var("CLAUDE_CODE_REMOTE").is_ok_and(|v| !v.is_empty()) {
                    None
                } else {
                    Some(GoalClearBucket::Auth)
                }
            }
            Some("account_on_hold") => Some(GoalClearBucket::Auth),
            Some("billing_error") => Some(GoalClearBucket::Billing),
            Some("verification_required") => Some(GoalClearBucket::VerificationRequired),
            Some("model_not_found") => Some(GoalClearBucket::ModelUnavailable),
            // `overloaded | server_error | max_output_tokens | rate_limit |
            // invalid_request | unknown | void 0` and anything unrecognised.
            _ => None,
        },
    }
}

/// `T4v` — the condition-truncation width inside the warning's quotes.
pub(crate) const GOAL_CLEAR_CONDITION_WIDTH: usize = 80;

/// The statsig gate, DEFAULT TRUE (`it("tengu_quartz_pipit",!0)`), so this path
/// is live in a default install.
pub(super) const GOAL_CLEAR_FLAG: &str = "tengu_quartz_pipit";

/// Oracle `is(e,t)` (@283759767) — grapheme-wise truncate to DISPLAY WIDTH `t`,
/// appending U+2026 which itself occupies one column of the budget:
///
/// ```js
/// function is(e,t){if(ar(e)<=t)return e;if(t<=1)return"\u2026";
///   let r=0,n="";for(let{segment:o}of H_().segment(e)){let i=ar(o);if(r+i>t-1)break;n+=o,r+=i}
///   return n+"\u2026"}
/// ```
pub(super) fn truncate_to_display_width_with_ellipsis(s: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation as _;
    use unicode_width::UnicodeWidthStr as _;
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > max_width - 1 {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('\u{2026}');
    out
}

/// Oracle `Yl(e,t,r)` (@283760333) with `r = true`, the form `Cqf` calls:
///
/// ```js
/// function Yl(e,t,r=!1){let n=e;
///   if(r){let o=e.indexOf("\n");
///     if(o!==-1){if(n=e.substring(0,o),ar(n)+1>t)return is(`${n}\u2026`,t);return `${n}\u2026`}}
///   if(ar(n)<=t)return n;return is(n,t)}
/// ```
///
/// The multi-line branch is NOT a width truncation: a condition containing a
/// newline is cut to its FIRST LINE and gains an ellipsis even when it is short,
/// and only then is width-clamped. A naive "truncate to 80 chars" would emit
/// different bytes for every multi-line goal.
pub(super) fn truncate_goal_condition(condition: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthStr as _;
    if let Some(nl) = condition.find('\n') {
        let first = &condition[..nl];
        let with_ellipsis = format!("{first}\u{2026}");
        return if first.width() + 1 > max_width {
            truncate_to_display_width_with_ellipsis(&with_ellipsis, max_width)
        } else {
            with_ellipsis
        };
    }
    if condition.width() <= max_width {
        return condition.to_string();
    }
    truncate_to_display_width_with_ellipsis(condition, max_width)
}

/// `` `Goal cleared after an unrecoverable error (${i}): "${Yl(e.condition,T4v,!0)}". Run /goal again to continue.` ``
pub(crate) fn goal_cleared_after_error_message(
    label: &str,
    condition: &Utf16JsonProjection,
) -> Utf16JsonProjection {
    let units = condition
        .string_units("")
        .expect("goal condition is source text");
    // Grapheme width is computed on the display image; slice the original
    // units at the same UTF-16 offset so unpaired units survive the notice.
    let display = String::from_utf16_lossy(&units);
    let truncated = truncate_goal_condition(&display, GOAL_CLEAR_CONDITION_WIDTH);
    let (prefix, suffix) = if truncated == display {
        (units.as_slice(), "")
    } else if let Some(prefix) = truncated.strip_suffix('…') {
        (&units[..prefix.encode_utf16().count()], "…")
    } else {
        (&units[..truncated.encode_utf16().count()], "")
    };
    source_error_affixes(
        &format!("Goal cleared after an unrecoverable error ({label}): \""),
        prefix,
        &format!("{suffix}\". Run /goal again to continue."),
    )
}

/// Run the `Cqf` teardown for a turn that just ended with `reason`.
///
/// Preconditions, in the oracle's order:
/// * `it("tengu_quartz_pipit",!0)` — default true;
/// * `!e` — a goal must be active;
/// * `t.agentId` / `PH(r)!=="main"` — main agent only. Structurally satisfied
///   here: subagents never run through `ConversationOrchestrator` (they use
///   `agent::runner`), so every orchestrator reaching this function IS the main
///   agent. Recorded rather than re-checked because there is no `agentId` to
///   read.
/// * `t.abortController.signal.aborted` — an aborted turn ends as
///   `TurnOutcome::Cancelled` on a different path and never reaches the four
///   call sites below.
///
/// Effects: remove the session-scoped `Stop` prompt hook + clear the active
/// goal (both in `clear_active_goal_state_and_hook`), fire the `goal_met`
/// telemetry with the bucket's errorCode, and surface the warning as a SYSTEM
/// notice — `jBt(text,"warning")`, not an assistant message, so it never enters
/// the model-facing history.
/// The retry/pause tiers of oracle `Kps` — what an active goal says about a
/// turn that ended badly but does not warrant clearing the goal.
///
/// Reached only from the `else` arm of `goal_clear_bucket`, so the clear tier
/// keeps its existing behaviour untouched.
///
pub(super) async fn announce_goal_interruption(
    orch: &ConversationOrchestrator,
    reason: GoalClearReason<'_>,
) {
    use crate::prompt::goal_interruption::{
        classify_api_error_interruption_with_facts, ApiErrorInterruptionFacts,
    };
    let GoalClearReason::ApiError {
        error_kind,
        is_transient,
    } = reason
    else {
        return;
    };
    // `quotaLimits` (the account's usage cap) and the host's `hasIntent()` wait
    // have no port analogue yet, so a rate limit reports the burst-limit
    // sentence. Both refinements only change WHICH pause sentence is shown.
    let Some(interruption) = classify_api_error_interruption_with_facts(
        error_kind,
        is_transient,
        ApiErrorInterruptionFacts {
            host_handles_auth: std::env::var("CLAUDE_CODE_REMOTE")
                .is_ok_and(|value| !value.is_empty()),
            ..ApiErrorInterruptionFacts::default()
        },
    ) else {
        return;
    };
    orch.handle_goal_interruption(interruption).await;
}

pub(crate) async fn clear_goal_after_unrecoverable_error(
    orch: &ConversationOrchestrator,
    reason: GoalClearReason<'_>,
) {
    if !telemetry::flag_bool(GOAL_CLEAR_FLAG, true) {
        return;
    }
    // `!e` — cheap read first, so the common no-goal turn does no extra work.
    // Bound explicitly so the session guard is released before the awaits below.
    let has_goal = {
        let s = orch.session.lock().await;
        s.active_goal.is_some()
    };
    if !has_goal {
        return;
    }
    let Some(bucket) = goal_clear_bucket(reason) else {
        // OR-4 (2.1.269): not every bad turn CLEARS the goal. The two tiers
        // 2.1.269 added — retry and pause — live here; before them a turn that
        // failed without qualifying for a clear left the goal silently sitting
        // there, which is the reported stall.
        announce_goal_interruption(orch, reason).await;
        return;
    };
    // `t.sessionHooksRegistry.remove(...)` + `cFe(e, …)` + `yield {type:"active_goal",value:void 0}`
    // are one operation in this port: the state clear and the Stop-hook removal
    // are inseparable here.
    //
    // DIVERGENCE (recorded): the oracle stamps the goal-status attachment with
    // `context_limit` / `api_error`; `lingxi_core::host::GoalStatusKind` has only
    // `Set|Cleared|Achieved`, and widening it would change a serialized
    // transcript enum, so the teardown records `Cleared`.
    // `kB(e, d==="context_limit" ? "context_limit" : "api_error")` — upstream
    // discriminates on the BUCKET, and `GoalClearBucket::ContextLimit` is
    // reachable only from `GoalClearReason::ContextLimit` (every `ApiError` arm
    // yields `Auth` / `Billing` / `ModelUnavailable` / no-clear), so testing the
    // bucket here is the same test.
    let cleared_reason = if bucket == GoalClearBucket::ContextLimit {
        lingxi_core::host::GoalClearedReason::ContextLimit
    } else {
        lingxi_core::host::GoalClearedReason::ApiError
    };
    let Some(goal) = orch.clear_active_goal_state_and_hook(cleared_reason).await else {
        return;
    };
    // `de("goal_met", s)` — the failure-flavoured twin of the success event.
    telemetry::emit_command_failed("goal_met", bucket.error_code());
    let text = goal_cleared_after_error_message(bucket.label(), &goal.condition);
    orch.output.emit_system_notice(&text, false).await;
}

pub(crate) async fn surface_prompt_too_long(orch: &ConversationOrchestrator) -> MessageId {
    let compact_failure = orch
        .compaction_runtime
        .compaction_tracking
        .lock()
        .await
        .last_compact_failure_detail
        .take();
    let text = compact_failure
        .as_deref()
        .and_then(crate::api_error_copy::automatic_compaction_failed_text)
        .unwrap_or_else(|| PROMPT_TOO_LONG_ERROR_MESSAGE.to_string());
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: assistant_id,
        content: vec![ContentBlock::Text {
            text: text.clone(),
            citations: None,
        }],
        stop_reason: Some("prompt_too_long".to_string()),
        resumable: None,
    };
    {
        let mut s = orch.session.lock().await;
        s.history.push(assistant_msg.clone());
    }
    // Non-streaming (batched) parity (claude.ts:2571): this surfaces the
    // prompt-too-long assistant turn on the BATCHED path, so persist ONE merged
    // assistant line (here a single text block → one line either way) — the
    // per-block split is streaming-only.
    orch.persist_message_to_jsonl(&assistant_msg).await;
    orch.output.emit_text(&text.into()).await;
    assistant_id
}

/// Surface the #54 rapid-refill (thrashing) breaker message on the reactive PTL
/// path and end the turn.
///
/// 1:1 with claude-code v2.1.183 (`bin/claude.exe` offset 202942256): the
/// reactive arm, on `kho(state) >= f6n`, emits the
/// `tengu_auto_compact_rapid_refill_breaker` telemetry and surfaces the
/// byte-exact thrashing message `Rho` as an `invalid_request` assistant error,
/// ending the turn with `reason:"rapid_refill_breaker"`. We surface it on the
/// same channel as [`surface_prompt_too_long`] (a stop-reason-bearing assistant
/// message + emit), so the turn ends cleanly.
pub(crate) async fn surface_rapid_refill_thrashing(orch: &ConversationOrchestrator) -> MessageId {
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: assistant_id,
        content: vec![ContentBlock::Text {
            text: compaction::RAPID_REFILL_THRASHING_MESSAGE.to_string(),
            citations: None,
        }],
        // The binary surfaces this as `error:"invalid_request"`.
        stop_reason: Some("invalid_request".to_string()),
        resumable: None,
    };
    {
        let mut s = orch.session.lock().await;
        s.history.push(assistant_msg.clone());
    }
    orch.persist_message_to_jsonl(&assistant_msg).await;
    orch.output
        .emit_text(&compaction::RAPID_REFILL_THRASHING_MESSAGE.into())
        .await;
    assistant_id
}

/// Build the user-visible `API Error: …` text claude-code surfaces for the
/// terminal stop_reasons it reports as errors: `max_tokens` (recovery
/// exhausted), `model_context_window_exceeded`, and `refusal`
/// (without a configured fallback). Returns `None` for every other terminal
/// (`stop_sequence` / `pause_turn` / …), which end silently.
///
/// Shared by the streaming ([`ConversationOrchestrator`] turn loop) and batched
/// ([`surface_terminal_api_error`]) terminal arms so both paths surface
/// byte-identical text (claude-code `claude.ts:2266/2279`, `U2e`). The refusal
/// The selected request model, source `stop_details`, request ID, and refused
/// response ID are passed through by both paths so route labels and refusal
/// details remain tied to the response being surfaced.
#[must_use]
pub(crate) fn terminal_api_error_text(
    model: &str,
    interactive: bool,
    stop_reason: &str,
    request_id: Option<&str>,
    stop_details: Option<&llm_runtime::HistoryStopDetails>,
    response_message_id: Option<&str>,
) -> Option<Utf16JsonProjection> {
    let scalar = match stop_reason {
        "max_tokens" => Some(format!(
            "API Error: Claude's response exceeded the {} output token maximum. To configure this behavior, set the LINGXI_MAX_OUTPUT_TOKENS environment variable.",
            compaction::max_output_tokens_for_model(model)
        )),
        "model_context_window_exceeded" => {
            Some("API Error: The model has reached its context window limit.".to_string())
        }
        "refusal" => {
            let snapshot = crate::server_fallback::refusal_text_snapshot(model);
            return Some(terminal_refusal_api_error_text(
                model, interactive, request_id, stop_details, response_message_id,
                snapshot.as_ref(),
            ));
        }
        _ => None,
    };
    scalar.map(Into::into)
}

/// Native 2.1.319 `e9t`, with route facts captured before the physical request.
/// A missing host snapshot remains unresolved: a marketing label alone never
/// establishes Native `fmt`/`Xp` eligibility or first-party provider identity.
fn terminal_refusal_api_error_text(
    model: &str,
    interactive: bool,
    request_id: Option<&str>,
    stop_details: Option<&llm_runtime::HistoryStopDetails>,
    response_message_id: Option<&str>,
    snapshot: Option<&lingxi_core::host::refusal_api_text::RefusalApiTextSnapshot>,
) -> Utf16JsonProjection {
    use lingxi_core::host::refusal_api_text::{
        CyberVerificationStatus, RefusalApiTextEvent, RefusalApiTextSnapshot,
        RefusalBrandCopyOwned, RefusalProviderKind,
    };
    let source = stop_details
        .and_then(|details| Utf16JsonProjection::parse(details.source().as_json_source()).ok());
    let category_units = source
        .as_ref()
        .and_then(|source| source.string_units("/category"));
    let category = category_units
        .as_deref()
        .and_then(|units| String::from_utf16(units).ok());
    let source_text = |pointer: &str| {
        source
            .as_ref()
            .and_then(|source| source.string_units(pointer))
            .and_then(|units| String::from_utf16(&units).ok())
    };
    let cyber_verification = if category.as_deref() == Some("cyber") {
        match source_text("/cyber_verification/type").as_deref() {
            Some("active") => match source_text("/cyber_verification/tier").as_deref() {
                Some("a") => Some(CyberVerificationStatus::SpecializedAccess),
                Some("b") => Some(CyberVerificationStatus::RedTeamAccess),
                Some("c") => Some(CyberVerificationStatus::DefenseAccess),
                _ => None,
            },
            Some("inactive") => match source_text("/cyber_verification/reason").as_deref() {
                Some("not_assigned_but_organization_approved") => {
                    Some(CyberVerificationStatus::NotAssignedButOrganizationApproved)
                }
                Some("organization_not_approved") => {
                    Some(CyberVerificationStatus::OrganizationNotApproved)
                }
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let mut facts = snapshot.cloned().unwrap_or_else(|| RefusalApiTextSnapshot {
        serving_model: Some(model.to_owned()),
        model_eligible: false,
        display_label: Some(
            crate::prompt::env_meta::marketing_name_for_model(model)
                .unwrap_or(model)
                .to_owned(),
        ),
        model_family: None,
        fable_copy_suppressed: false,
        opus_5_5_exception: false,
        help_url: None,
        provider: RefusalProviderKind::Other,
        interactive,
        feedback_eligible: false,
        brand: RefusalBrandCopyOwned {
            api_error_prefix: "API Error".to_owned(),
            product_name: branding::PRODUCT_NAME.to_owned(),
            generic_model_label: branding::PRODUCT_NAME.to_owned(),
        },
    });
    // Rebrand product identity only; model labels and provider/legal URLs are
    // resolved route facts and retain their original identity.
    facts.brand.product_name = branding::PRODUCT_NAME.to_owned();
    facts.brand.generic_model_label = branding::PRODUCT_NAME.to_owned();
    let event = RefusalApiTextEvent {
        category_utf16: category_units.as_deref(),
        message_id: response_message_id,
        cyber_verification,
        ..RefusalApiTextEvent::default()
    };
    match facts.format_with_event(category.as_deref(), request_id, event) {
        Ok(text) => text.into(),
        Err(error) => {
            tracing::warn!(%error, "resolved refusal facts could not render native text");
            // Missing host help facts must not turn a refusal into a panic or
            // erase the event's validated identifiers.
            facts.model_eligible = false;
            facts
                .format_with_event(category.as_deref(), request_id, event)
                .unwrap_or_else(|_| {
                    "API Error: LingXi can't help with this. Start a new session to continue."
                        .to_owned()
                })
                .into()
        }
    }
}

/// Surface the terminal `API Error: …` assistant message on the BATCHED path
/// (the streaming twin inlines the same persist+emit before `emit_end_turn`).
///
/// Builds the text via [`terminal_api_error_text`]; when `Some`, pushes a
/// stop-reason-bearing assistant message into history, persists it (the
/// synthetic-envelope JSONL line), and emits the text. The caller still returns
/// [`TurnStepOutcome::Ended`], whose driver fires the end-of-turn bookkeeping
/// (`emit_end_turn`) exactly once — this helper deliberately does NOT emit the
/// end-of-turn marker. Returns `Some(assistant_id)` of the surfaced message, or
/// `None` when `stop_reason` is not one of the three error terminals.
pub(crate) async fn surface_terminal_api_error(
    orch: &ConversationOrchestrator,
    request_model: &str,
    stop_reason: &str,
    stop_details: Option<&llm_runtime::HistoryStopDetails>,
    response_message_id: Option<&str>,
) -> Option<MessageId> {
    let interactive = orch.prompt_is_interactive();
    // The just-completed call's Anthropic `request-id` — for the refusal
    // message's `\nRequest ID: …` suffix (recorded by the adapter from the
    // response headers; same slot the JSONL `requestId` reads from).
    let request_id = orch.api.last_request_id();
    let text = terminal_api_error_text(
        request_model,
        interactive,
        stop_reason,
        request_id.as_deref(),
        stop_details,
        response_message_id,
    )?;
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: assistant_id,
        content: vec![ContentBlock::TextJsUtf16 {
            text: text
                .string_at("")
                .expect("source terminal error")
                .to_owned(),
            utf16_code_units: text.string_units("").expect("source terminal error"),
            citations: None,
        }],
        stop_reason: Some(stop_reason.to_string()),
        resumable: None,
    };
    {
        let mut s = orch.session.lock().await;
        s.history.push(assistant_msg.clone());
    }
    // Top-level api-error envelope per builder/stop_reason (verified vs the
    // 2.1.195 binary + on-disk transcripts):
    // - `max_tokens` / `model_context_window_exceeded`: claude-code's
    //   `ql({content,apiError:"max_output_tokens",error:"max_output_tokens"})` →
    //   `error:"max_output_tokens"` (no HTTP status), inner `stop_sequence`.
    // - `refusal`: the `fje("refusal", …)` builder keeps inner
    //   `stop_reason:"refusal"` and tags `error:"invalid_request"` (the sole
    //   on-disk refusal line: `stop_reason:"refusal", error:"invalid_request"`).
    let env = match stop_reason {
        "max_tokens" | "model_context_window_exceeded" => ApiErrorEnvelope {
            request_id: None,
            api_error: Some("max_output_tokens"),
            error: Some("max_output_tokens"),
            api_error_status: None,
            inner_stop_reason: None,
            inner_stop_details: None,
            refused_message_id: None,
            truncated_after_output: false,
            terminal_reason: None,
        },
        "refusal" => ApiErrorEnvelope {
            request_id,
            api_error: None,
            error: Some("invalid_request"),
            api_error_status: None,
            inner_stop_reason: Some("refusal"),
            inner_stop_details: stop_details.cloned(),
            refused_message_id: lingxi_core::host::refusal_api_text::sanitized_refusal_id(
                response_message_id,
            )
            .map(str::to_owned),
            truncated_after_output: false,
            terminal_reason: None,
        },
        // `terminal_api_error_text` returned `Some` only for the three reasons
        // above; any other value can't reach here.
        _ => ApiErrorEnvelope::default(),
    };
    orch.persist_api_error_message_to_jsonl(&assistant_msg, env.clone())
        .await;
    let terminal_reason = if stop_reason == "refusal" {
        "api_error"
    } else {
        stop_reason
    };
    orch.emit_synthetic_api_error_message(&assistant_msg, &env, terminal_reason, true)
        .await;
    Some(assistant_id)
}

/// Whether a turn error has DEDICATED downstream handling and must propagate as
/// a hard `Err` instead of being caught as a graceful `model_error` (#10):
/// - `RateLimited` — the `run_turn*` wrapper re-maps it onto the limits-specific
///   copy + emits the terminal rate-limit snapshot (`enrich_api_error` /
///   `emit_terminal_rate_limit_if_changed`).
/// - `Overloaded` / `RepeatedOverloaded` — the byte-locked "Repeated 529
///   Overloaded errors" surface (`errors.ts:166`).
/// - `PermissionAbort` — the auto-mode denial breaker deliberately terminates
///   a prompt-avoiding agent and must not be converted into `model_error`.
/// Mirrors claude-code, whose top-level `catch` is reached only AFTER the retry
/// layer has handled 429/529; everything else falls through to `model_error`.
#[must_use]
pub(crate) fn is_carveout_propagated(e: &OrchestratorError) -> bool {
    match e {
        OrchestratorError::PermissionAbort { .. } | OrchestratorError::RepeatedOverloaded => true,
        OrchestratorError::ApiCall(error) | OrchestratorError::Streaming(error) => matches!(
            error.classification_source(),
            LlmError::RateLimited { .. }
                | LlmError::Overloaded { .. }
                | LlmError::RequestDispatchRejected { .. }
        ),
        _ => false,
    }
}

pub(super) async fn maybe_checkpoint_for_trigger(
    orch: &ConversationOrchestrator,
    trigger: session::CheckpointTrigger,
) {
    let (session_id, todos) = {
        let s = orch.session.lock().await;
        (
            session::checkpoint_session_key(s.session_id),
            s.todos.clone(),
        )
    };
    let cwd = orch.session_cwd.cwd();
    let gates = session::CheckpointGates {
        // `Dn()` — print mode / SDK / scheduled-headless never gets a
        // checkpoint, because nothing would ever tell the user it happened.
        non_interactive: !orch.prompt_is_interactive(),
        // `Ca()` — no remote-workspace concept in the port.
        remote_workspace: false,
        // `Vs("allow_local_checkpoint_commit")` defaults to ALLOWED upstream —
        // i.e. Claude Code writes a WIP commit into the user's repository on
        // the first rate limit of a session. The port has no managed-policy
        // feature registry to express that check, and "should LingXi commit
        // into a user's repo uninvited?" is a product call, not an engineering
        // one. So the mechanism is complete and wired, and this one boolean
        // reads an explicit opt-in until that call is made. Flipping it to
        // `true` matches upstream exactly.
        policy_allows: session::local_checkpoint_commit_allowed(),
    };
    let _ = session::dispatch_rate_limit_checkpoint(session::OwnedCheckpointRequest {
        session_id,
        trigger,
        todos,
        cwd,
        gates,
    });
}

/// SC-02 — fire the rate-limit resume checkpoint on a rate-limited turn end.
///
/// Mirrors the oracle's call shape exactly: fire-and-forget, errors swallowed
/// (`…then(({performRateLimitCheckpoint:Yr})=>Yr({todos,trigger:"rate_limited"}))
/// .catch(()=>{})`). The checkpoint runs several `git` subprocesses, so it goes
/// on a detached OS thread rather than blocking the turn's teardown.
///
/// The session crate installs a session-keyed `Running` latch synchronously
/// before the detached worker starts, so a near-limit trigger and a later 429
/// cannot race into duplicate checkpoint work.
pub(super) async fn maybe_checkpoint_on_rate_limit(
    orch: &ConversationOrchestrator,
    error: &OrchestratorError,
) {
    let is_rate_limited = match error {
        OrchestratorError::ApiCall(error) | OrchestratorError::Streaming(error) => {
            matches!(error.classification_source(), LlmError::RateLimited { .. })
        }
        _ => false,
    };
    if !is_rate_limited {
        return;
    }
    maybe_checkpoint_for_trigger(orch, session::CheckpointTrigger::RateLimited).await;
}

/// Surface a `model_error` turn-end (port of `query.ts:955-997`'s top-level
/// `catch`). A runtime error that escaped the API layer (not PTL/overflow/rate/
/// overload — those propagate upstream) is NOT a hard failure: log
/// `tengu_query_error`, yield the classified presentation text as an `isApiErrorMessage`
/// assistant (provider HTTP errors carry Native's `API Error: ` prefix), end with
/// `reason:'model_error'`. Session survives. `yieldMissingToolResultBlocks` is a
/// no-op (history appends only after success); `queryDepth=0` (subagents bypass
/// this orchestrator); per-turn `assistantMessages`/`toolUses` counts omitted.
pub(crate) async fn surface_model_error(
    orch: &ConversationOrchestrator,
    error_text: Utf16JsonProjection,
    env: ApiErrorEnvelope,
    provider_status: Option<u16>,
) -> MessageId {
    if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "queryChainId".into(),
            telemetry::AnalyticsValue::String(orch.query_chain_id.clone()),
        );
        metadata.insert("queryDepth".into(), telemetry::AnalyticsValue::Int(0));
        bus.log_event("tengu_query_error", metadata).await;
    }
    let reason = env.terminal_reason.unwrap_or("api_error");
    let id = surface_api_error_notice_with_terminal_notification(orch, error_text, env, true).await;
    orch.note_turn_api_error_terminal_reason(reason);
    orch.note_turn_api_error_status(provider_status);
    id
}

/// Persist + emit an api-error assistant message (the `createAssistantAPIErrorMessage`
/// shape) WITHOUT the `tengu_query_error` telemetry that the top-level `model_error`
/// catch logs. Shared by [`surface_model_error`] and the P1-04 partial-stream
/// finalize notice (cc 2.1.199 yields the incomplete-response notice via `tu(...)`
/// directly, not through the top-level catch — so it does NOT fire `tengu_query_error`).
pub(crate) async fn surface_api_error_notice(
    orch: &ConversationOrchestrator,
    error_text: Utf16JsonProjection,
    env: ApiErrorEnvelope,
) -> MessageId {
    surface_api_error_notice_with_terminal_notification(orch, error_text, env, false).await
}

async fn surface_api_error_notice_with_terminal_notification(
    orch: &ConversationOrchestrator,
    error_text: Utf16JsonProjection,
    env: ApiErrorEnvelope,
    notify_terminal_error: bool,
) -> MessageId {
    orch.note_turn_api_error_status(env.api_error_status);
    // `createAssistantAPIErrorMessage({ content })` renders `content` verbatim,
    // falling back to the `NO_CONTENT_MESSAGE` placeholder when empty.
    let error_text = if error_text.string_at("") == Some("") {
        "(no content)".into()
    } else {
        error_text
    };
    let text = error_text
        .string_at("")
        .expect("API error content must be a string projection")
        .to_owned();
    let utf16_code_units = error_text
        .string_units("")
        .expect("API error content must carry exact string code units");
    let content = if utf16_code_units == text.encode_utf16().collect::<Vec<_>>() {
        ContentBlock::Text {
            text: text.clone(),
            citations: None,
        }
    } else {
        ContentBlock::TextJsUtf16 {
            text: text.clone(),
            utf16_code_units,
            citations: None,
        }
    };
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: assistant_id,
        content: vec![content],
        stop_reason: Some("model_error".to_string()),
        resumable: None,
    };
    {
        let mut s = orch.session.lock().await;
        s.history.push(assistant_msg.clone());
    }
    // The top-level `model_error` catch builds the assistant line via
    // `createAssistantAPIErrorMessage({content})` (content verbatim, inner
    // `stop_reason` stays `"stop_sequence"`). The top-level api-error envelope
    // — `error` category + optional `apiErrorStatus` — is computed by the
    // per-request classifier (`Flp`/`KNn`, ported as
    // [`crate::conversation::classify_api_error`]) at the call site from the
    // TYPED error and passed in here (the classifier deferral is now CLOSED).
    orch.persist_api_error_message_to_jsonl(&assistant_msg, env.clone())
        .await;
    orch.emit_synthetic_api_error_message(
        &assistant_msg,
        &env,
        env.terminal_reason.unwrap_or("api_error"),
        notify_terminal_error,
    )
    .await;
    assistant_id
}

/// Produce the display text associated with an orchestrator error without
/// throwing away provider code units when the underlying LLM error has an
/// exact Source carrier. Non-LLM errors are host-generated UTF-8 text.
pub(crate) fn orchestrator_error_text_source(
    error: &OrchestratorError,
    model: &str,
) -> Utf16JsonProjection {
    match error {
        OrchestratorError::Settings(error) => error.message_projection(),
        OrchestratorError::MaxStructuredOutputRetries {
            max_retries,
            last_error: Some(detail),
        } => source_error_affixes(
            &format!(
                "Failed to provide valid structured output after {max_retries} attempts — last StructuredOutput error: "
            ),
            &detail
                .string_units("")
                .expect("structured output error is a source string"),
            "",
        ),
        OrchestratorError::ApiCall(error) | OrchestratorError::Streaming(error) => {
            let native = error.native_anthropic_error_facts();
            if native.is_some() && error.http_status() == Some(404) {
                return format!("There's an issue with the selected model ({model}). It may not exist or you may not have access to it. Run --model to pick a different model.").into();
            }
            let text = llm_runtime::error_display_source(error);
            if let Some(facts) = native {
                if error.http_status().is_some_and(|status| status >= 500) {
                    let mut message = text
                        .string_units("")
                        .expect("error text is a source string");
                    // Native Zce(e).replace(/[.!?\u2026]+$/, "") operates on
                    // the original JavaScript string, including lone units.
                    while message
                        .last()
                        .is_some_and(|unit| matches!(unit, 0x2e | 0x21 | 0x3f | 0x2026))
                    {
                        message.pop();
                    }
                    let suffix = format!(
                        ". This is a server-side issue, usually temporary — try again in a moment.{}",
                        native_server_status_hint(facts),
                    );
                    return source_error_affixes("API Error: ", &message, &suffix);
                }
            }
            // Native 2.1.293 writes this prefix into both its assistant JSONL
            // row and final CLI result. A local validation error has no SDK
            // provider-text carrier and must not acquire an HTTP identity.
            let prefix =
                error
                    .native_anthropic_error_facts()
                    .and_then(|_| match error.http_status() {
                        Some(400 | 422) => Some("API Error: "),
                        Some(401 | 403) => Some("Failed to authenticate. API Error: "),
                        _ => None,
                    });
            if let Some(prefix) = prefix {
                source_error_affixes(
                    prefix,
                    &text
                        .string_units("")
                        .expect("error text is a source string"),
                    "",
                )
            } else {
                text
            }
        }
        _ => error.to_string().into(),
    }
}

fn source_error_affixes(prefix: &str, text: &[u16], suffix: &str) -> Utf16JsonProjection {
    let units = prefix
        .encode_utf16()
        .chain(text.iter().copied())
        .chain(suffix.encode_utf16())
        .collect::<Vec<_>>();
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("affixing source text preserves its UTF-16 units")
}

/// Native 2.1.293 jJn. Route facts come from the dispatched SDK request, so a
/// profile's gateway hint cannot accidentally use another profile's endpoint.
fn native_server_status_hint(
    facts: &lingxi_llm_client::protocol::NativeAnthropicErrorFacts,
) -> String {
    use lingxi_llm_client::protocol::ProtocolFamily;
    let provider = match facts.protocol {
        ProtocolFamily::BedrockClaude => "bedrock",
        ProtocolFamily::VertexClaude => "vertex",
        ProtocolFamily::FoundryClaude => "foundry",
        _ => facts.provider_id.as_str(),
    };
    match provider {
        "anthropic"
            if std::env::var("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL")
                .is_ok_and(|value| !value.is_empty()) =>
        {
            " If it persists, check https://status.claude.com.".into()
        }
        "anthropic" => match facts.request_authority.as_deref() {
            None | Some("api.anthropic.com") => {
                " If it persists, check https://status.claude.com.".into()
            }
            Some(authority) => {
                format!(" If it persists, check your inference gateway ({authority}).")
            }
        },
        "anthropicAws" => " If it persists, check https://status.claude.com.".into(),
        "anthropicGoogleCloud" => {
            " If it persists, check https://status.claude.com and Google Cloud's status page."
                .into()
        }
        other => {
            let label = match other {
                "bedrock" => "Amazon Bedrock",
                "vertex" => "Google Vertex AI",
                "foundry" => "Microsoft Foundry",
                "mantle" => "Amazon Bedrock (Mantle)",
                "gateway" => "Cloud gateway",
                other => other,
            };
            format!(" If it persists, check your {label} service status.")
        }
    }
}

#[cfg(test)]
mod source_error_tests {
    use super::*;
    use lingxi_llm_client::protocol::{
        NativeAnthropicErrorFacts, ProtocolFamily, ProviderErrorText, ProviderId,
    };

    #[test]
    fn native_319_noninteractive_refusal_uses_the_frozen_provider_facts() {
        use lingxi_core::host::refusal_api_text::{
            RefusalApiTextSnapshot, RefusalBrandCopyOwned, RefusalProviderKind,
        };
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../core/tests/fixtures/refusal_api_text_2_1_319.json"
        ))
        .unwrap();
        for row in fixture["cases"].as_array().unwrap().iter().take(2) {
            let input = &row["input"];
            let snapshot = RefusalApiTextSnapshot {
                serving_model: Some("claude-sonnet-4-5".into()),
                model_eligible: false,
                display_label: Some("Sonnet 4.5".into()),
                model_family: Some("sonnet".into()),
                fable_copy_suppressed: false,
                opus_5_5_exception: false,
                help_url: None,
                provider: RefusalProviderKind::FirstParty,
                interactive: false,
                feedback_eligible: true,
                brand: RefusalBrandCopyOwned {
                    api_error_prefix: "API Error".into(),
                    product_name: "LingXi".into(),
                    generic_model_label: "LingXi".into(),
                },
            };
            let details = input["details"].as_object().map(|_| {
                llm_runtime::HistoryStopDetails::parse(&input["details"].to_string()).unwrap()
            });
            let text = terminal_refusal_api_error_text(
                "claude-sonnet-4-5",
                false,
                input["requestId"].as_str(),
                details.as_ref(),
                input["messageId"].as_str(),
                Some(&snapshot),
            );
            assert_eq!(text.string_at(""), row["expected"]["content"].as_str());
        }
    }

    #[test]
    fn native_319_source_category_units_reach_visible_text_and_json_without_a_lossy_view() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../core/tests/fixtures/refusal_api_text_2_1_319.json"
        ))
        .unwrap();
        for row in fixture["cases"].as_array().unwrap() {
            let input = &row["input"];
            let Some(raw) = input["sourceStopDetails"].as_str() else {
                continue;
            };
            let details = llm_runtime::HistoryStopDetails::parse(raw).unwrap();
            let expected_category = input["categoryUtf16"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| unit.as_u64().unwrap() as u16)
                .collect::<Vec<_>>();
            let preserved = Utf16JsonProjection::parse(details.source().as_json_source()).unwrap();
            assert_eq!(preserved.string_units("/category"), Some(expected_category));
            let text = terminal_api_error_text(
                "claude-sonnet-4-5",
                false,
                "refusal",
                input["requestId"].as_str(),
                Some(&details),
                input["messageId"].as_str(),
            )
            .unwrap();
            let expected = row["expected"]["contentUtf16"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| unit.as_u64().unwrap() as u16)
                .collect::<Vec<_>>();
            assert_eq!(
                text.string_units(""),
                Some(expected.clone()),
                "{}",
                row["name"]
            );
            let encoded = Utf16JsonProjection::parse(&text.to_json_string().unwrap()).unwrap();
            assert_eq!(encoded.string_units(""), Some(expected), "{}", row["name"]);
        }
    }

    #[test]
    fn goal_notice_retains_unpaired_units_before_a_truncated_newline() {
        let condition = Utf16JsonProjection::parse(r#""ship \ud800\nthen rest""#).unwrap();
        let result = goal_cleared_after_error_message("context limit reached", &condition);
        let expected = "Goal cleared after an unrecoverable error (context limit reached): \"ship "
            .encode_utf16()
            .chain([0xd800, 0x2026])
            .chain("\". Run /goal again to continue.".encode_utf16())
            .collect::<Vec<_>>();
        assert_eq!(result.string_units(""), Some(expected));
    }

    fn error(units: Vec<u16>, native: bool) -> OrchestratorError {
        OrchestratorError::ApiCall(LlmError::ProviderErrorText {
            source: Box::new(LlmError::InvalidRequest {
                message: "500 fixture".into(),
            }),
            text: ProviderErrorText::new(String::from_utf16_lossy(&units), units).unwrap(),
            native_anthropic_facts: native.then(|| NativeAnthropicErrorFacts {
                provider_id: ProviderId::from("anthropic"),
                protocol: ProtocolFamily::AnthropicMessages,
                request_authority: Some("127.0.0.1:57288".into()),
                thinking_resumption_refused: false,
                thinking_resumption_beta_requested: None,
                thinking_resumption_beta_refused: false,
                native_thinking_binding_controls_beta_requested: None,
                native_thinking_binding_controls_error_evidence: None,
                native_hq_model_source: None,
                native_hq_model_provider_family: None,
                native_hq_known_error_evidence: None,
                native_hq_hrn15_evidence: None,
                native_thinking_type_rejection: None,
                response: None,
            }),
        })
    }

    #[test]
    fn native_server_error_trims_only_trailing_punctuation_without_losing_source() {
        for ending in ["", ".!?…", ". "] {
            let units = "500 provider "
                .encode_utf16()
                .chain([0xd800])
                .chain(ending.encode_utf16())
                .collect::<Vec<_>>();
            let rendered = orchestrator_error_text_source(&error(units, true), "claude-sonnet-4-5");
            let retained_ending = if ending == ". " { ending } else { "" };
            let expected = "API Error: 500 provider ".encode_utf16().chain([0xd800]).chain(retained_ending.encode_utf16()).chain(". This is a server-side issue, usually temporary — try again in a moment. If it persists, check your inference gateway (127.0.0.1:57288).".encode_utf16()).collect::<Vec<_>>();
            assert_eq!(rendered.string_units(""), Some(expected));
        }
    }

    #[test]
    fn other_provider_error_keeps_its_existing_presentation() {
        let units = "500 provider "
            .encode_utf16()
            .chain([0xd800])
            .collect::<Vec<_>>();
        assert_eq!(
            orchestrator_error_text_source(&error(units.clone(), false), "model").string_units(""),
            Some(units)
        );
    }

    #[test]
    fn settings_error_text_keeps_its_source_utf16_units() {
        let message = Utf16JsonProjection::parse(r#""managed policy \ud800 failed""#).unwrap();
        let diagnostics = Utf16JsonProjection::parse(
            r#"{"kind":"read","message":"managed policy \ud800 failed"}"#,
        )
        .unwrap();
        let error =
            OrchestratorError::Settings(lingxi_core::settings::SettingsError::source_failure(
                "incomplete_managed_selection",
                message.clone(),
                diagnostics,
                None,
            ));

        assert_eq!(
            orchestrator_error_text_source(&error, "model").string_units(""),
            message.string_units("")
        );
    }
}

/// Machine-readable category survives graceful model-error presentation.
pub(crate) fn model_error_kind(error: &OrchestratorError) -> &'static str {
    match error {
        OrchestratorError::ApiCall(inner) | OrchestratorError::Streaming(inner) => match inner
            .classification_source()
        {
            LlmError::InvalidRequest { .. } => "invalid_request",
            LlmError::MalformedToolInput { .. } => "malformed_tool_input",
            LlmError::UnsupportedCapability { .. } => "unsupported_capability",
            LlmError::Transport { .. } | LlmError::TransportTimeout { .. } => "transport_error",
            LlmError::Authentication { .. } | LlmError::OAuthRefreshDead => "authentication_failed",
            LlmError::PermissionDenied { .. } => "permission_denied",
            LlmError::StreamInterrupted { .. } => "stream_interrupted",
            LlmError::RequestDispatchRejected { .. } => "dispatch_rejected",
            _ => "provider_error",
        },
        OrchestratorError::StreamingProtocol(_) => "stream_protocol_error",
        OrchestratorError::StreamEndedWithoutStop => "stream_incomplete",
        OrchestratorError::Internal(_) => "internal_error",
        OrchestratorError::PermissionAbort { .. } => "tool_permission_aborted",
        _ => "execution_failed",
    }
}

#[cfg(test)]
mod model_error_kind_regression {
    use super::*;

    #[test]
    fn typed_category_does_not_include_provider_text() {
        let error = OrchestratorError::ApiCall(LlmError::InvalidRequest {
            message: "private provider body, argument and credential".into(),
        });
        assert_eq!(model_error_kind(&error), "invalid_request");
        let error = OrchestratorError::Internal("private path and request".into());
        assert_eq!(model_error_kind(&error), "internal_error");
    }
}

#[cfg(test)]
mod api_error_publication_tests {
    use super::*;
    use lingxi_core::host::orchestrator::{CostSnapshot, OutputStream, TerminalApiErrorMessage};
    use lingxi_core::host::OutputEvent;
    use lingxi_core::types::ToolUseId;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct RequestApi;
    #[async_trait::async_trait]
    impl crate::conversation::OrchestratorApiClient for RequestApi {
        async fn messages_create(
            &self,
            _request: crate::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, LlmError> {
            unreachable!()
        }
        fn last_request_id(&self) -> Option<String> {
            Some("req_publication_319".into())
        }
    }

    #[derive(Default)]
    struct TypedOnlySink {
        rows: tokio::sync::Mutex<Vec<TerminalApiErrorMessage>>,
        text_callbacks: AtomicUsize,
        legacy_callbacks: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl OutputStream for TypedOnlySink {
        async fn emit_text(&self, _text: &Utf16JsonProjection) {
            self.text_callbacks.fetch_add(1, Ordering::SeqCst);
        }
        async fn emit_tool_call(&self, _id: &ToolUseId, _tool: &str, _input: &Utf16JsonProjection) {
        }
        async fn emit_tool_result(
            &self,
            _id: &ToolUseId,
            _tool: &str,
            _text: &Utf16JsonProjection,
            _result: &Utf16JsonProjection,
            _meta: Option<&Utf16JsonProjection>,
        ) {
        }
        async fn emit_end_turn(&self, _reason: &str, _cost: &CostSnapshot) {}
        async fn emit_terminal_api_error(
            &self,
            _status: Option<u16>,
            _terminal: &str,
            _stop: &str,
        ) {
            self.legacy_callbacks.fetch_add(1, Ordering::SeqCst);
        }
        async fn emit_terminal_api_error_message(&self, row: &TerminalApiErrorMessage) {
            self.rows.lock().await.push(row.clone());
        }
    }

    fn orchestrator(output: Arc<dyn OutputStream>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(RequestApi),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(crate::test_support::NoOpPermissionGate),
            output,
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }

    async fn publish_cases(orch: &ConversationOrchestrator) {
        let details = llm_runtime::HistoryStopDetails::parse(
            r#"{"category":"A\ud800B","explanation":"\udc00"}"#,
        )
        .unwrap();
        surface_terminal_api_error(
            orch,
            "claude-sonnet-4-5",
            "refusal",
            Some(&details),
            Some("msg_publication_319"),
        )
        .await
        .unwrap();
        for stop in ["max_tokens", "model_context_window_exceeded"] {
            surface_terminal_api_error(orch, "claude-sonnet-4-5", stop, None, None)
                .await
                .unwrap();
        }
        surface_model_error(
            orch,
            Utf16JsonProjection::parse(r#""model \ud800 failure""#).unwrap(),
            ApiErrorEnvelope {
                error: Some("server_error"),
                api_error_status: Some(500),
                ..Default::default()
            },
            Some(500),
        )
        .await;
        surface_api_error_notice(
            orch,
            "partial response notice".into(),
            ApiErrorEnvelope {
                error: Some("server_error"),
                truncated_after_output: true,
                ..Default::default()
            },
        )
        .await;
        orch.stage_refusal_api_error_row(
            "claude-sonnet-4-5",
            Some(&details),
            Some("msg_staged_319"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn terminal_model_partial_and_staged_errors_publish_one_typed_row_each() {
        let sink = Arc::new(TypedOnlySink::default());
        let orch = orchestrator(sink.clone());
        publish_cases(&orch).await;
        let rows = sink.rows.lock().await;
        assert_eq!(rows.len(), 6);
        assert_eq!(
            rows.iter()
                .map(|row| row.notify_terminal_error)
                .collect::<Vec<_>>(),
            vec![true, true, true, true, false, true]
        );
        assert_eq!(sink.text_callbacks.load(Ordering::SeqCst), 0);
        assert_eq!(
            sink.legacy_callbacks.load(Ordering::SeqCst),
            0,
            "typed consumers must not also receive legacy terminal callbacks"
        );
        let refusal = &rows[0];
        assert_eq!(refusal.request_id.as_deref(), Some("req_publication_319"));
        assert_eq!(
            refusal.refused_message_id.as_deref(),
            Some("msg_publication_319")
        );
        assert_eq!(refusal.error.as_deref(), Some("invalid_request"));
        assert!(refusal.api_error.is_none());
        assert_eq!(refusal.terminal_reason, "api_error");
        assert_eq!(refusal.stop_reason, "refusal");
        assert_eq!(
            refusal.message.string_units("/stop_details/category"),
            Some(vec![65, 0xd800, 66])
        );
        for row in &rows[1..3] {
            assert_eq!(row.api_error.as_deref(), Some("max_output_tokens"));
            assert_eq!(row.error.as_deref(), Some("max_output_tokens"));
            assert_eq!(row.stop_reason, "stop_sequence");
            assert!(row.request_id.is_none());
        }
        assert_eq!(rows[3].api_error_status, Some(500));
        assert_eq!(
            rows[3].message.string_units("/content/0/text"),
            Some(
                "model "
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(" failure".encode_utf16())
                    .collect()
            )
        );
        assert_eq!(rows[4].error.as_deref(), Some("server_error"));
        assert_eq!(
            rows[5].refused_message_id.as_deref(),
            Some("msg_staged_319")
        );
        for row in rows.iter() {
            assert_ne!(
                row.message.string_units("/id"),
                Some(row.uuid.as_uuid().to_string().encode_utf16().collect())
            );
        }
    }

    #[tokio::test]
    async fn synthetic_inner_id_and_creation_time_match_durable_and_public_projections() {
        let native: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/synthetic_api_error_identity_2_1_319.json"
        ))
        .unwrap();
        for row in native["cases"].as_array().unwrap() {
            assert_ne!(row["expected"]["outer_uuid"], row["expected"]["inner_id"]);
            assert_eq!(row["expected"]["allocations"].as_array().unwrap().len(), 2);
        }
        let root = tempfile::tempdir().unwrap();
        let session_id = lingxi_core::types::SessionId::new();
        let path = root.path().join(format!("{}.jsonl", session_id.as_uuid()));
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let sink = Arc::new(TypedOnlySink::default());
        let orch = orchestrator(sink.clone())
            .with_session_id(session_id)
            .with_jsonl_writer(writer);
        orch.initialize_conversation_mode().await.unwrap();
        let message = ConversationMessage::Assistant {
            id: MessageId::new(),
            per_turn_effort: None,
            content: vec![ContentBlock::Text {
                text: "synthetic".into(),
                citations: None,
            }],
            stop_reason: Some("refusal".into()),
            resumable: None,
        };
        let env = ApiErrorEnvelope {
            error: Some("invalid_request"),
            inner_stop_reason: Some("refusal"),
            inner_stop_details: Some(
                llm_runtime::HistoryStopDetails::parse(r#"{"category":"A\ud800B"}"#).unwrap(),
            ),
            ..Default::default()
        };
        orch.persist_api_error_message_to_jsonl(&message, env.clone())
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        orch.emit_synthetic_api_error_message(&message, &env, "api_error", true)
            .await;
        let rows = sink.rows.lock().await;
        let public = &rows[0];
        let persisted = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| Utf16JsonProjection::parse(line).ok())
            .find(|row| {
                row.string_units("/uuid")
                    == Some(message.id().as_uuid().to_string().encode_utf16().collect())
            })
            .unwrap();
        assert_ne!(
            public.message.string_units("/id"),
            Some(public.uuid.as_uuid().to_string().encode_utf16().collect())
        );
        assert_eq!(
            persisted.string_units("/timestamp"),
            Some(public.timestamp.encode_utf16().collect())
        );
        assert_eq!(
            persisted
                .subprojection("/message")
                .unwrap()
                .to_json_string()
                .unwrap(),
            public.message.to_json_string().unwrap()
        );
        let repeated = orch.to_jsonl_message_with_inner_id(
            &message,
            &session_id.as_uuid().to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&env),
        );
        assert_eq!(
            repeated.message.string_units("/id"),
            public.message.string_units("/id")
        );
        assert_eq!(repeated.timestamp, public.timestamp);
        let explicit = orch.to_jsonl_message_with_inner_id(
            &message,
            &session_id.as_uuid().to_string(),
            None,
            None,
            None,
            None,
            Some("explicit-provider-id"),
            None,
            None,
            None,
            Some(&env),
        );
        assert_eq!(
            explicit.message.string_units("/id"),
            Some("explicit-provider-id".encode_utf16().collect())
        );
        let provider = orch.to_jsonl_message_with_inner_id(
            &message,
            &session_id.as_uuid().to_string(),
            None,
            None,
            None,
            None,
            Some("actual-provider-id"),
            Some("provider-model"),
            None,
            None,
            None,
        );
        assert_eq!(
            provider.message.string_units("/id"),
            Some("actual-provider-id".encode_utf16().collect())
        );
    }

    #[tokio::test]
    async fn default_tui_keeps_recoverable_partial_notice_text_only() {
        let sink = Arc::new(crate::test_support::MockOutputStream::new());
        let orch = orchestrator(sink.clone());
        publish_cases(&orch).await;
        let events = sink.snapshot().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, OutputEvent::Text { .. }))
                .count(),
            6
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, OutputEvent::TerminalApiError { .. }))
                .count(),
            5
        );
    }

    #[tokio::test]
    async fn recoverable_notice_followed_by_success_never_notifies_legacy_terminal_error() {
        let sink = Arc::new(crate::test_support::MockOutputStream::new());
        let orch = orchestrator(sink.clone());
        surface_api_error_notice(
            &orch,
            "partial response notice".into(),
            ApiErrorEnvelope {
                error: Some("server_error"),
                truncated_after_output: true,
                ..Default::default()
            },
        )
        .await;
        sink.emit_text(&"recovered response".into()).await;
        sink.emit_end_turn("end_turn", &CostSnapshot::default())
            .await;
        let events = sink.snapshot().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, OutputEvent::Text { .. }))
                .count(),
            2
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, OutputEvent::TerminalApiError { .. })));
        assert!(events.iter().any(|event| matches!(event, OutputEvent::EndTurn {stop_reason, ..} if stop_reason == "end_turn")));
    }
}
