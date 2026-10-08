use crate::conversation::{ApiErrorEnvelope, ConversationOrchestrator};
use crate::error::OrchestratorError;
#[doc = " Byte-exact user-facing message surfaced when the prompt is too long and the"]
#[doc = " reactive 413 recovery (Batch 5) is exhausted. 1:1 with claude-code"]
#[doc = " `errors.ts` `PROMPT_TOO_LONG_ERROR_MESSAGE = 'Prompt is too long'`."]
#[doc = ""]
#[doc = " Re-exported from `model::prompt_too_long` (the orchestrator's own copy),"]
#[doc = " which is the authoritative source for this string in this crate."]
use crate::model::prompt_too_long::PROMPT_TOO_LONG_ERROR_MESSAGE;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
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
pub(crate) fn goal_cleared_after_error_message(label: &str, condition: &str) -> String {
    let truncated = truncate_goal_condition(condition, GOAL_CLEAR_CONDITION_WIDTH);
    format!(
        "Goal cleared after an unrecoverable error ({label}): \"{truncated}\". \
         Run /goal again to continue."
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
    use crate::prompt::goal_interruption::classify_api_error_interruption;
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
    let Some(interruption) =
        classify_api_error_interruption(error_kind, is_transient, false, false)
    else {
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
        id: assistant_id,
        content: vec![ContentBlock::Text { text: text.clone(), citations: None }],
        stop_reason: Some("prompt_too_long".to_string()),
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
    orch.output.emit_text(&text).await;
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
        id: assistant_id,
        content: vec![ContentBlock::Text {
            text: compaction::RAPID_REFILL_THRASHING_MESSAGE.to_string(), citations: None,
        }],
        // The binary surfaces this as `error:"invalid_request"`.
        stop_reason: Some("invalid_request".to_string()),
    };
    {
        let mut s = orch.session.lock().await;
        s.history.push(assistant_msg.clone());
    }
    orch.persist_message_to_jsonl(&assistant_msg).await;
    orch.output
        .emit_text(compaction::RAPID_REFILL_THRASHING_MESSAGE)
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
/// cyber/bio category variant, the `stop_details.explanation` clause, and the
/// `\n\nRequest ID: …` suffix remain residuals on BOTH paths — LingXi does not
/// thread `stop_details`/requestId into the terminal arm, so the non-cyber,
/// no-explanation path (the common terminal) fires.
#[must_use]
pub(crate) fn terminal_api_error_text(
    model: &str,
    interactive: bool,
    stop_reason: &str,
    request_id: Option<&str>,
    stop_details: Option<&llm_runtime::HistoryStopDetails>,
) -> Option<String> {
    match stop_reason {
        "max_tokens" => Some(format!(
            "API Error: Claude's response exceeded the {} output token maximum. To configure this behavior, set the LINGXI_MAX_OUTPUT_TOKENS environment variable.",
            compaction::max_output_tokens_for_model(model)
        )),
        "model_context_window_exceeded" => {
            Some("API Error: The model has reached its context window limit.".to_string())
        }
        "refusal" => {
            // Faithful port of the binary's `U2e` (@197278360): the message is
            // category-aware via `rnt(cat) = cat ∈ {"cyber","bio"}` and
            // `pd() = firstParty` (always true for LingXi's Anthropic path).
            let category = stop_details.and_then(|sd| sd.category.as_deref());
            let cyber_or_bio = matches!(category, Some("cyber" | "bio"));
            let is_cyber = matches!(category, Some("cyber"));
            let base = match crate::prompt::env_meta::marketing_name_for_model(model) {
                Some(label) => {
                    // LABEL branch. `m`/`f` are the interactive suffixes.
                    let m = if interactive {
                        "Double press esc to edit your last message, or try a different model with /model."
                    } else {
                        "Try rephrasing the request in a new session or change your model."
                    };
                    let f = if interactive {
                        "Send feedback with /feedback or learn more: https://support.claude.com/en/articles/15363606"
                    } else {
                        "Learn more: https://support.claude.com/en/articles/15363606"
                    };
                    // `h` (binary `U2e`): the cyber/bio variant (`Jct(cat)=cat∈
                    // {cyber,bio}`) appends `Saa`, the generic one a fixed tail.
                    //   Saa = `They may flag safe, normal content as well. ${elp}`
                    //   elp = `These measures let us bring you Mythos-level
                    //          capabilities sooner, and we're working to refine them.`
                    let a = if cyber_or_bio {
                        format!(
                            "{label}'s safeguards flagged this message (https://www.anthropic.com/legal/aup). They may flag safe, normal content as well. These measures let us bring you Mythos-level capabilities sooner, and we're working to refine them."
                        )
                    } else {
                        format!(
                            "{label}'s safeguards flagged this message (https://www.anthropic.com/legal/aup). This sometimes happens with safe, normal conversations."
                        )
                    };
                    // Frame `c = `${bT}: ${h} <brand> can't respond … with ${l}.\n\n${m}\n\n${f}``
                    // — DOUBLE `\n` separators (od -c verified on 2.1.195 @206804081;
                    // the `strings` dump misled an earlier pass into single `\n`).
                    // `<brand>` is the LingXi rebrand.
                    format!("API Error: {a} LingXi can't respond to this request with {label}.\n\n{m}\n\n{f}")
                }
                None => {
                    // NO-LABEL branch.
                    let m = if interactive {
                        "Please double press esc to edit your last message or start a new session for LingXi to assist with a different task."
                    } else {
                        "Try rephrasing the request in a new session or change your model."
                    };
                    if is_cyber {
                        // 2.1.206 (JS @217943553) replaced the old "apply for an
                        // exemption" message with the Cyber Verification Program
                        // interstitial (`t?.category==="cyber" && Xf()`; Xf() =
                        // the first-party cyber-safeguards gate, always on for the
                        // Anthropic path — the 195 code used the analogous
                        // `pd()`=firstParty). `m2 = n!=null ? Mf(n) : "This model"`
                        // = "This model" here (this is the no-marketing-name
                        // branch). The feedback tail is interactive-only (binary
                        // `g = p ? "" : "\n\nIf you were not…"`); there is NO
                        // `\n\n{m}\n\n{f}` tail — it was dropped in 206.
                        // Byte-verified: 'apply for an exemption' = 0 hits in 206;
                        // help-center URL and interstitial = 2 hits each.
                        let feedback_tail = if interactive {
                            "\n\nIf you were not engaging in a cybersecurity topic, please send feedback via /feedback."
                        } else {
                            ""
                        };
                        format!(
                            "API Error: This model has safety measures that flagged this message for a cybersecurity topic. To learn about the Cyber Verification Program and apply for access, visit our help center: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude.{feedback_tail}"
                        )
                    } else {
                        // Binary final `else`: `${bT}: <brand> is unable to respond
                        // … aup).${a} `+f` — `${a}` is the optional explanation
                        // clause (`a=i?` ${i}${punct}`:""`). Empty ⇒ `). {m}`;
                        // present ⇒ `). <explanation>[.] {m}`. (`f` == the port's
                        // `m` rephrase message.) 2.1.206 REMOVED the
                        // `military_weapons` arm entirely — 0 hits for
                        // 'weapons-related content' / 'military_weapons' in 206.
                        let clause = refusal_explanation_clause(
                            stop_details.and_then(|sd| sd.explanation.as_deref()),
                        );
                        format!(
                            "API Error: LingXi is unable to respond to this request, which appears to violate our Usage Policy (https://www.anthropic.com/legal/aup).{clause} {m}"
                        )
                    }
                }
            };
            // Binary `u = n ? `\n\nRequest ID: ${n}` : ""` — DOUBLE `\n`, appended to
            // `base` only when a request id is present (REFUSAL-ONLY surface).
            // (od -c verified on 2.1.195 @206805081; strings dump misled an earlier
            // pass into single `\n`.)
            let suffix = match request_id {
                Some(id) if !id.is_empty() => format!("\n\nRequest ID: {id}"),
                _ => String::new(),
            };
            Some(format!("{base}{suffix}"))
        }
        _ => None,
    }
}

/// The binary `U2e` explanation clause `${a}`:
///   `let s=400, i = o && o.length>s ? o.slice(0,s).trimEnd()+"…" : o,
///    a = i ? ` ${i}${/[.!?…]$/.test(i)?"":"."}` : ""`
/// where `o` is the refusal explanation. Returns `""` when absent/empty; else a
/// LEADING-space clause ` <explanation>` plus a terminal `.` when it does not
/// already end with `.`/`!`/`?`/`…`. The explanation is truncated (+ `…`) past
/// 400 chars — only then is its tail trimmed (matching `o.length>s` gating the
/// `trimEnd`). The cap is by `char` count (the port's truncation convention; JS
/// uses UTF-16 units — identical for the typical ASCII refusal text).
pub(super) fn refusal_explanation_clause(explanation: Option<&str>) -> String {
    let Some(o) = explanation.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    const CAP: usize = 400;
    let i = if o.chars().count() > CAP {
        let head: String = o.chars().take(CAP).collect();
        format!("{}\u{2026}", head.trim_end())
    } else {
        o.to_string()
    };
    if i.is_empty() {
        return String::new();
    }
    let ends_punct = i
        .chars()
        .last()
        .is_some_and(|c| matches!(c, '.' | '!' | '?' | '\u{2026}'));
    format!(" {i}{}", if ends_punct { "" } else { "." })
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
    stop_reason: &str,
    stop_details: Option<&llm_runtime::HistoryStopDetails>,
) -> Option<MessageId> {
    let (model, interactive) = {
        let s = orch.session.lock().await;
        (s.model.clone(), orch.prompt_is_interactive())
    };
    // The just-completed call's Anthropic `request-id` — for the refusal
    // message's `\nRequest ID: …` suffix (recorded by the adapter from the
    // response headers; same slot the JSONL `requestId` reads from).
    let request_id = orch.api.last_request_id();
    let text = terminal_api_error_text(
        &model,
        interactive,
        stop_reason,
        request_id.as_deref(),
        stop_details,
    )?;
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        id: assistant_id,
        content: vec![ContentBlock::Text { text: text.clone(), citations: None }],
        stop_reason: Some(stop_reason.to_string()),
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
            error: Some("max_output_tokens"),
            api_error_status: None,
            inner_stop_reason: None,
            truncated_after_output: false,
        },
        "refusal" => ApiErrorEnvelope {
            error: Some("invalid_request"),
            api_error_status: None,
            inner_stop_reason: Some("refusal"),
            truncated_after_output: false,
        },
        // `terminal_api_error_text` returned `Some` only for the three reasons
        // above; any other value can't reach here.
        _ => ApiErrorEnvelope::default(),
    };
    orch.persist_api_error_message_to_jsonl(&assistant_msg, env)
        .await;
    orch.output.emit_text(&text).await;
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
    matches!(
        e,
        OrchestratorError::PermissionAbort { .. }
            | OrchestratorError::RepeatedOverloaded
            | OrchestratorError::ApiCall(
                LlmError::RateLimited { .. }
                    | LlmError::Overloaded { .. }
                    | LlmError::RequestDispatchRejected { .. }
            )
            | OrchestratorError::Streaming(
                LlmError::RateLimited { .. }
                    | LlmError::Overloaded { .. }
                    | LlmError::RequestDispatchRejected { .. }
            )
    )
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
    if !matches!(
        error,
        OrchestratorError::ApiCall(LlmError::RateLimited { .. })
            | OrchestratorError::Streaming(LlmError::RateLimited { .. })
    ) {
        return;
    }
    maybe_checkpoint_for_trigger(orch, session::CheckpointTrigger::RateLimited).await;
}

/// Surface a `model_error` turn-end (port of `query.ts:955-997`'s top-level
/// `catch`). A runtime error that escaped the API layer (not PTL/overflow/rate/
/// overload — those propagate upstream) is NOT a hard failure: log
/// `tengu_query_error`, yield the raw text VERBATIM as an `isApiErrorMessage`
/// assistant (`createAssistantAPIErrorMessage`, no `API Error:` prefix), end with
/// `reason:'model_error'`. Session survives. `yieldMissingToolResultBlocks` is a
/// no-op (history appends only after success); `queryDepth=0` (subagents bypass
/// this orchestrator); per-turn `assistantMessages`/`toolUses` counts omitted.
pub(crate) async fn surface_model_error(
    orch: &ConversationOrchestrator,
    error_text: &str,
    env: ApiErrorEnvelope,
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
    surface_api_error_notice(orch, error_text, env).await
}

/// Persist + emit an api-error assistant message (the `createAssistantAPIErrorMessage`
/// shape) WITHOUT the `tengu_query_error` telemetry that the top-level `model_error`
/// catch logs. Shared by [`surface_model_error`] and the P1-04 partial-stream
/// finalize notice (cc 2.1.199 yields the incomplete-response notice via `tu(...)`
/// directly, not through the top-level catch — so it does NOT fire `tengu_query_error`).
pub(crate) async fn surface_api_error_notice(
    orch: &ConversationOrchestrator,
    error_text: &str,
    env: ApiErrorEnvelope,
) -> MessageId {
    // `createAssistantAPIErrorMessage({ content })` renders `content` verbatim,
    // falling back to the `NO_CONTENT_MESSAGE` placeholder when empty.
    let text = if error_text.is_empty() {
        "(no content)".to_string()
    } else {
        error_text.to_string()
    };
    let assistant_id = MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        id: assistant_id,
        content: vec![ContentBlock::Text { text: text.clone(), citations: None }],
        stop_reason: Some("model_error".to_string()),
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
    orch.persist_api_error_message_to_jsonl(&assistant_msg, env)
        .await;
    orch.output.emit_text(&text).await;
    assistant_id
}
