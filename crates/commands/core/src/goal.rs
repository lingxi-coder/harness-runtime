//! `/goal` — set a session-scoped goal Claude checks before stopping, show the
//! active goal, or clear it early.
//!
//! Ported from the claude-code v2.1.198 binary. `strings -a` on the shipped
//! Mach-O turned up TWO command objects sharing the name `"goal"`:
//!
//! ```text
//! {type:"local-jsx",name:"goal",description:"Set a goal Claude checks before
//!   stopping",argumentHint:"[<condition> | clear]",immediate:!0,load:...}
//! {type:"local",name:"goal",supportsNonInteractive:!0,thinClientDispatch:
//!   "post-text",description:"Set a goal — keep working until the condition is
//!   met",get isHidden(){return!hr()},isEnabled:()=>hr()||Qi(),load:...}
//! ```
//! exported as `default` (the interactive `local-jsx` TUI dialog) and the
//! named export `goalNonInteractive` (the `local`/`supportsNonInteractive`
//! variant). LingXi is a headless engine, so this handler ports the SECOND
//! object's body — the plain-text branch function located alongside it:
//!
//! ```text
//! ...`${WMl(o.lastReason)}`:"";return{type:"text",value:
//!   `Goal active: ${o.condition} (${s})${i}`}}
//! if(trr(n)){let o=IEt(t);return{type:"text",value:o===null?"No goal set":
//!   `Goal cleared: ${o}`}}
//! if(n.length>wEt)return bt("goal_set","too_long"),{type:"text",value:
//!   `Goal condition is limited to ${wEt} characters (got ${n.length})`};
//! let r=kEt(n,t);if(r!==null)return{type:"text",value:r};
//! return{type:"query",value:`Goal set: ${n}`,prompt:nrr(n)};
//! ```
//!
//! `kEt(n,t)` is the trust/hooks-restricted gate (returns a fixed message on
//! failure, `null` on success); `nrr(n)` builds the model-facing directive
//! injected once the goal is accepted. `trr(n)` is the case-insensitive
//! clear-token membership test.
//!
//! ## Headless mapping onto `CommandResult`
//!
//! The binary's non-interactive result is `{type, value, prompt?}`: `value` is
//! always a short line for the invoking surface, and a successful `set` ALSO
//! carries `prompt` — the text queried to the model as the next turn. LingXi's
//! [`CommandResult`] has no variant carrying both a display line and an
//! injected turn at once, so the success path follows the explicit spec
//! instruction and returns [`CommandResult::InjectMessage`] with the directive
//! text (below), matching the `/review` handler's `InjectMessage`-only
//! precedent (`commands/core/src/review.rs`) rather than [`CommandResult::Done`].
//! The `status`/`clear`/`too-long` branches have no `prompt`, so they map onto
//! `CommandResult::Done` (the `/effort`/`/model` precedent).
//!
//! ## Remaining note
//!
//! * **`lastReasonSuffix` formatting is NOT byte-verified.** The task's
//!   locked output-string list gives every OTHER literal verbatim but leaves
//!   this one as a `${lastReasonSuffix}` placeholder; the one targeted
//!   `strings` probe that reached it was truncated mid-fragment
//!   (`` `${WMl(o.lastReason)}` ``) with no confirmed static prefix text. Since
//!   the binary fragment is incomplete, [`GoalHandler::status`]'s suffix format
//!   is a best-effort placeholder, clearly marked, not a confirmed port.
//!
//! ## PORTED — SLASH-04: auto-clear on an unrecoverable turn error
//!
//! claude-code 2.1.238 added `Cqf` (@292182815, statsig `tengu_quartz_pipit`
//! **default `true`**), which tears the goal down when a turn dies for a
//! reason the user cannot retry past. It is absent from 2.1.220, so this is
//! genuine 2.1.238 drift.
//!
//! It does NOT live in this file: the trigger is the conversation turn-error
//! path, so the port lives with the turn loop that owns the terminal reasons —
//! `orchestrator::turn_loop::clear_goal_after_unrecoverable_error`, called from
//! all four terminal arms of `execute_one_turn_with_recovery_tracked`
//! (`PromptTooLong`, `BlockingLimit`, `RapidRefillBreaker` → `context_limit`,
//! and the graceful api-error `Err(e)` arm → the errorKind switch). The
//! teardown reuses this command's own clear path
//! (`clear_active_goal_state_and_hook`), so the Stop hook is removed exactly as
//! `/goal clear` removes it. The byte-level spec, kept here for reference:
//!
//! ```text
//! if(!it("tengu_quartz_pipit",!0)||!e||t.agentId||t.abortController.signal.aborted||PH(r)!=="main")return;
//! let o=w4v(n);if(o===null)return;let{label:i,errorCode:s}=v4v[o];
//! t.sessionHooksRegistry.remove(zt(),"Stop",{type:"prompt",prompt:e.condition}),
//! cFe(e,o==="context_limit"?"context_limit":"api_error"),de("goal_met",s),
//! yield{type:"active_goal",value:void 0},yield bOi(!0,e.condition),
//! yield jBt(`Goal cleared after an unrecoverable error (${i}): "${Yl(e.condition,T4v,!0)}". Run /goal again to continue.`,"warning")
//! ```
//!
//! Preconditions: main agent only (`agentId` unset, `PH(r)==="main"`), not
//! aborted, a goal is active. `T4v=80` is the condition-truncation width.
//! Label / telemetry map (`v4v`, @292183854):
//!
//! ```text
//! auth             -> "authentication failed"    / cleared_auth
//! billing          -> "credit balance too low"   / cleared_billing
//! context_limit    -> "context limit reached"    / cleared_context_limit
//! model_unavailable-> "model unavailable"        / cleared_model_unavailable
//! ```
//!
//! Reason -> bucket (`w4v`): `blocking_limit | prompt_too_long |
//! rapid_refill_breaker` -> `context_limit`; `api_error` -> `null` when
//! `isTransient`, else by `errorKind`: `authentication_failed |
//! oauth_org_not_allowed` -> `auth` UNLESS remote (`CLAUDE_CODE_REMOTE ||
//! j2() || BYt()!==null`), `account_on_hold` -> `auth`, `billing_error` ->
//! `billing`, `model_not_found` -> `model_unavailable`, and `overloaded |
//! server_error | max_output_tokens | rate_limit | invalid_request | unknown |
//! undefined` -> `null`. Every non-`api_error` reason (`image_error`,
//! `model_error`, `malformed_tool_use_exhausted`, `aborted_streaming`,
//! `aborted_tools`, `stop_hook_prevented`, `hook_stopped`, `tool_deferred`,
//! `max_turns`, `background_requested`, `completed`) -> `null` (no clear).

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
#[cfg(test)]
use platform_api::ActiveGoalSnapshot;
use platform_api::OrchestratorHandle;
use std::sync::Arc;
#[cfg(test)]
use std::time::SystemTime;

/// `wEt` — the max goal-condition length (v2.1.198).
const MAX_CONDITION_CHARS: usize = 4000;

/// `trr(n)` — case-insensitive clear-token membership. Order/spelling per the
/// task spec (not independently re-derived from the binary beyond the
/// located `trr(n)` call site).
const CLEAR_TOKENS: &[&str] = &["clear", "stop", "off", "reset", "none", "cancel"];

/// Literal, shared by both the "no active goal" status branch and the "clear
/// with nothing to clear" branch (binary: `o===null?"No goal set"`, and the
/// status branch's `else` arm reaching the same text).
const NO_GOAL_SET: &str = "No goal set";

/// The empty-arg STATUS branch's no-goal line — 2.1.266 `src_185633862.js`:
/// `if(!t)return{type:"text",value:"No goal set. Usage: \`/goal <condition>\`"}`.
/// Distinct from [`NO_GOAL_SET`], which is only the clear branch's
/// `o===null?"No goal set"` arm.
const NO_GOAL_SET_USAGE: &str = "No goal set. Usage: `/goal <condition>`";

/// `kEt`'s trust-gate failure message, verbatim from the locked output-string
/// list. The value comes from the composition root's effective workspace
/// trust decision through [`OrchestratorHandle::workspace_trusted`].
const TRUST_GATE_MESSAGE: &str =
    "/goal is only available in trusted workspaces. Restart, accept the trust dialog, and try again.";

/// `kEt`'s hooks-restricted failure message, verbatim from the locked
/// output-string list. The value comes from merged managed/user/project hook
/// policy through [`OrchestratorHandle::hooks_restricted`].
const HOOKS_RESTRICTED_MESSAGE: &str = "/goal can't run while hooks are restricted (disableAllHooks or allowManagedHooksOnly is set in settings or by policy).";

/// `bt("goal_set","too_long")` — the binary's own telemetry call on the
/// too-long guard. Passed straight through `telemetry::emit_command_failed`
/// (a generic `tracing::error!(event=.., error=..)` transport) rather than
/// through the LingXi `tengu_command_<name>_*` batch-1/2 convention
/// (`telemetry::tengu::command`), since `goal` was never one of those batches
/// and this file may not add a new constant to that shared module.
const TELEMETRY_GOAL_SET_EVENT: &str = "goal_set";
const TELEMETRY_TOO_LONG_PROPERTY: &str = "too_long";

/// `tengu_stop_hook_removed` — the binary's clear-path telemetry event (per
/// the task spec), fired when an existing goal's `Stop` hook is torn down.
/// Fired when `/goal clear` removes the session-scoped named Stop Prompt hook,
/// carrying the cleared condition as `details`.
const TELEMETRY_STOP_HOOK_REMOVED: &str = "tengu_stop_hook_removed";

/// `Goal condition is limited to {n} characters (got {got})` — verbatim.
fn too_long_message(got: usize) -> String {
    format!("Goal condition is limited to {MAX_CONDITION_CHARS} characters (got {got})")
}

/// `nrr(n)` — the fixed model-facing directive injected once a goal is
/// accepted. Verbatim from the locked output-string list.
fn directive_for(condition: &str) -> String {
    format!(
        "A session-scoped Stop hook is now active with condition: \"{condition}\". Briefly \
acknowledge the goal, then immediately start (or continue) working toward it — treat the \
condition itself as your directive and do not pause to ask the user what to do. The hook will \
block stopping until the condition holds. It auto-clears once the condition is met — do not \
tell the user to run `/goal clear` after success; that's only for clearing a goal early."
    )
}

/// `/goal` handler — set, show, or clear a session-scoped stop-gating goal.
#[derive(Clone)]
pub struct GoalHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl GoalHandler {
    /// Construct a `GoalHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }

    /// The empty-arg status branch — 2.1.266 `src_185633862.js`:
    ///
    /// ```js
    /// let r=t.iterations===0?"not yet evaluated":`${t.iterations} ${x(t.iterations,"turn")}`,
    ///     n=t.lastReason?`\nLast check: ${Hr(t.lastReason.trim())}`:"";
    /// return{type:"text",value:`Goal active: ${t.condition} (${r})${n}`}
    /// ```
    ///
    /// The parenthetical is the ITERATION COUNT, not elapsed time, there is no
    /// `Evaluations:` line, and the last reason is truncated to its first line
    /// by `Hr`. The earlier port guessed all three (its module doc flagged the
    /// `lastReason` suffix as unverified); the executable settles them.
    async fn status(&self) -> String {
        match self.handle.get_active_goal().await {
            None => NO_GOAL_SET_USAGE.to_string(),
            Some(g) => {
                let evaluations = match g.iterations {
                    0 => "not yet evaluated".to_string(),
                    1 => "1 turn".to_string(),
                    count => format!("{count} turns"),
                };
                let last_check = match g.last_reason.as_deref().filter(|reason| !reason.is_empty())
                {
                    Some(reason) => {
                        format!("\nLast check: {}", first_line(trim_js_whitespace(reason)))
                    }
                    None => String::new(),
                };
                format!("Goal active: {} ({evaluations}){last_check}", g.condition)
            }
        }
    }

    /// The clear-token branch (binary: `o===null?"No goal set":\`Goal
    /// cleared: ${o}\``).
    async fn clear(&self) -> String {
        match self.handle.clear_active_goal().await {
            None => NO_GOAL_SET.to_string(),
            Some(g) => {
                telemetry::emit_command_completed(TELEMETRY_STOP_HOOK_REMOVED, &g.condition);
                format!("Goal cleared: {}", g.condition)
            }
        }
    }

    /// The valid-condition branch: gate check, then set + inject the
    /// directive (binary: `` {type:"query",value:`Goal set: ${n}`,
    /// prompt:nrr(n)} `` — see the module doc's `CommandResult` mapping note
    /// for why only the `prompt` half is representable here).
    async fn set(&self, condition: &str) -> CommandResult {
        // 2.1.270 `ust`: hooks restrictions take precedence over trust.
        if self.handle.hooks_restricted().await {
            return CommandResult::Done {
                display: Some(HOOKS_RESTRICTED_MESSAGE.to_string()),
            };
        }
        if !self.handle.workspace_trusted().await {
            return CommandResult::Done {
                display: Some(TRUST_GATE_MESSAGE.to_string()),
            };
        }
        self.handle.set_active_goal(condition).await;
        CommandResult::InjectMessage {
            content: directive_for(condition),
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for GoalHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let trimmed = trim_js_whitespace(&args.raw_args);

        // 1) empty → status.
        if trimmed.is_empty() {
            return CommandResult::Done {
                display: Some(self.status().await),
            };
        }

        // 2) case-insensitive clear token → clear.
        if CLEAR_TOKENS.contains(&trimmed.to_lowercase().as_str()) {
            return CommandResult::Done {
                display: Some(self.clear().await),
            };
        }

        // 2.1.270 `e.length`: JavaScript counts UTF-16 code units.
        let n = trimmed.encode_utf16().count();
        if n > MAX_CONDITION_CHARS {
            telemetry::emit_command_failed(TELEMETRY_GOAL_SET_EVENT, TELEMETRY_TOO_LONG_PROPERTY);
            return CommandResult::Done {
                display: Some(too_long_message(n)),
            };
        }

        // 4) else → gate + set.
        self.set(trimmed).await
    }

    fn name(&self) -> &str {
        "goal"
    }

    fn description(&self) -> &str {
        // Verbatim from the v2.1.198 `local`/non-interactive command object
        // (`name:"goal",...,description:"Set a goal — keep working until the
        // condition is met"`). Not routed through `core_description` — like
        // `effort.rs`, this command is actually implemented, and `names.rs`
        // (locked, not editable by this port) has no `"goal"` arm yet.
        "Set a goal — keep working until the condition is met"
    }
}

/// Match ECMAScript `String.trim()`, including BOM but excluding U+0085.
fn trim_js_whitespace(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// `Hr(t)` = `pt(t,"\n")` (2.1.266 `src_157781101.js`) — everything before the
/// first newline, so a multi-line evaluator reason renders as ONE line.
fn first_line(s: &str) -> &str {
    match s.find('\n') {
        Some(idx) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    #[test]
    fn goal_directive_matches_latest_oracle_bytes() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/goal_2_1_270.json")).unwrap();
        let expected = fixture["directive"]
            .as_str()
            .unwrap()
            .replace("{condition}", "ship it");
        assert_eq!(directive_for("ship it").as_bytes(), expected.as_bytes());
    }

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "goal".to_string(),
            raw_args: raw.to_string(),
            positional_args: vec![],
        }
    }

    fn handler() -> GoalHandler {
        GoalHandler::new(Arc::new(MockOrchestratorHandle::new()))
    }

    fn mock_handler() -> (Arc<MockOrchestratorHandle>, GoalHandler) {
        let handle = Arc::new(MockOrchestratorHandle::new());
        let handler = GoalHandler::new(handle.clone());
        (handle, handler)
    }

    #[tokio::test]
    async fn empty_arg_with_no_goal_reports_no_goal_set() {
        // The STATUS branch carries the usage hint; only the CLEAR branch is the
        // bare "No goal set" (2.1.266 `src_185633862.js`).
        let h = handler();
        match h.handle(&args("")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "No goal set. Usage: `/goal <condition>`");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        // Whitespace-only args trim to empty too.
        match h.handle(&args("   ")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "No goal set. Usage: `/goal <condition>`");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clear_with_no_goal_reports_no_goal_set() {
        let h = handler();
        for token in ["clear", "STOP", "Off", "reset", "none", "cancel"] {
            match h.handle(&args(token)).await {
                CommandResult::Done { display: Some(s) } => assert_eq!(s, "No goal set"),
                other => panic!("expected Done, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn too_long_condition_is_rejected() {
        let h = handler();
        let long = "x".repeat(4001);
        match h.handle(&args(&long)).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Goal condition is limited to 4000 characters (got 4001)");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        // Exactly at the limit is accepted (goes on to the set branch).
        let exact = "y".repeat(4000);
        match h.handle(&args(&exact)).await {
            CommandResult::InjectMessage { content } => assert!(content.contains(&exact)),
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn latest_oracle_counts_utf16_units_for_goal_limit() {
        let h = handler();
        let exact = "😀".repeat(2000);
        assert!(matches!(
            h.handle(&args(&exact)).await,
            CommandResult::InjectMessage { .. }
        ));
        let long = format!("{exact}x");
        match h.handle(&args(&long)).await {
            CommandResult::Done {
                display: Some(text),
            } => assert_eq!(
                text,
                "Goal condition is limited to 4000 characters (got 4001)"
            ),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn latest_oracle_uses_javascript_trim_and_reason_truthiness() {
        let (handle, h) = mock_handler();
        assert!(matches!(
            h.handle(&args("\u{feff}clear\u{feff}")).await,
            CommandResult::Done { .. }
        ));
        assert!(matches!(
            h.handle(&args("\u{85}")).await,
            CommandResult::InjectMessage { .. }
        ));
        for (reason, suffix) in [
            ("", ""),
            ("   ", "\nLast check: "),
            ("\u{feff}evidence\u{feff}", "\nLast check: evidence"),
        ] {
            handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
                condition: "ship".to_string(),
                set_at: SystemTime::now(),
                last_reason: Some(reason.to_string()),
                iterations: 1,
                tokens_at_start: 0,
            }));
            assert_eq!(
                h.status().await,
                format!("Goal active: ship (1 turn){suffix}")
            );
        }
    }

    #[tokio::test]
    async fn setting_a_goal_injects_the_fixed_directive() {
        let h = handler();
        match h.handle(&args("ship the release notes")).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.starts_with(
                    "A session-scoped Stop hook is now active with condition: \"ship the release notes\"."
                ));
                assert!(content.contains("do not pause to ask the user what to do"));
                assert!(content.contains(
                    "do not tell the user to run `/goal clear` after success; that's only for clearing a goal early."
                ));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn untrusted_workspace_blocks_goal_before_state_changes() {
        let (handle, h) = mock_handler();
        handle.set_workspace_trusted(false);

        match h.handle(&args("ship it")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, TRUST_GATE_MESSAGE);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(
            handle.get_active_goal().await.is_none(),
            "the trust gate must reject before /goal mutates state or registers hooks"
        );
    }

    #[tokio::test]
    async fn latest_oracle_checks_hook_policy_before_workspace_trust() {
        let (handle, h) = mock_handler();
        handle.set_workspace_trusted(false);
        handle.set_hooks_restricted(true);
        match h.handle(&args("ship")).await {
            CommandResult::Done {
                display: Some(text),
            } => assert_eq!(text, HOOKS_RESTRICTED_MESSAGE),
            other => panic!("expected hook-policy rejection, got {other:?}"),
        }
        assert!(handle.get_active_goal().await.is_none());
    }

    #[tokio::test]
    async fn restricted_hooks_block_goal_before_state_changes() {
        let (handle, h) = mock_handler();
        handle.set_hooks_restricted(true);

        match h.handle(&args("ship it")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, HOOKS_RESTRICTED_MESSAGE);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(
            handle.get_active_goal().await.is_none(),
            "the managed-hooks restriction must reject before /goal mutates state or registers hooks"
        );
    }

    #[tokio::test]
    async fn e2e_status_after_set_then_clear_round_trip() {
        // The full lifecycle through the SAME handler instance a registry
        // would hold: set → status reflects it → clear removes it → status
        // reports none again.
        let h = handler();

        match h.handle(&args("finish the migration")).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.contains("finish the migration"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }

        match h.handle(&args("")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Goal active: finish the migration (not yet evaluated)");
            }
            other => panic!("expected Done, got {other:?}"),
        }

        match h.handle(&args("clear")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Goal cleared: finish the migration");
            }
            other => panic!("expected Done, got {other:?}"),
        }

        match h.handle(&args("status")).await {
            // "status" is not a recognized clear token, so with no goal
            // active it falls through to the `set` branch and injects a
            // directive for the literal condition "status" — mirroring the
            // binary, which has no separate `current`/`status` alias for
            // `/goal` (only the empty-arg branch shows status).
            CommandResult::InjectMessage { content } => assert!(content.contains("\"status\"")),
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_pluralizes_turns_and_separates_last_check() {
        let (handle, h) = mock_handler();
        handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
            condition: "ship".to_string(),
            set_at: SystemTime::now(),
            last_reason: Some("tests pending".to_string()),
            iterations: 1,
            tokens_at_start: 100,
        }));
        let one = h.status().await;
        assert_eq!(one, "Goal active: ship (1 turn)\nLast check: tests pending");

        handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
            condition: "ship".to_string(),
            set_at: SystemTime::now(),
            last_reason: Some("review pending".to_string()),
            iterations: 2,
            tokens_at_start: 100,
        }));
        let many = h.status().await;
        assert_eq!(
            many,
            "Goal active: ship (2 turns)\nLast check: review pending"
        );

        // `Hr(t)` keeps only the first line of a multi-line evaluator reason.
        handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
            condition: "ship".to_string(),
            set_at: SystemTime::now(),
            last_reason: Some("  first line\nsecond line\nthird  ".to_string()),
            iterations: 3,
            tokens_at_start: 100,
        }));
        assert_eq!(
            h.status().await,
            "Goal active: ship (3 turns)\nLast check: first line"
        );
    }

    #[tokio::test]
    async fn name_and_description() {
        let h = handler();
        assert_eq!(h.name(), "goal");
        assert_eq!(
            h.description(),
            "Set a goal — keep working until the condition is met"
        );
    }
}
