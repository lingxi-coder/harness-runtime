//! Session Goal command, checked against Claude Code 2.1.322.
//!
//! Display and model directive retain their original UTF-16 code units. The
//! accepted-set display uses the host's system-notice channel; the directive
//! follows the ordinary injected-turn path. Source receipts and replay cases
//! live in `scripts/tests/goal_322_oracle.mjs`.

use crate::model::{
    source_concat, source_from_units, trim_source_text, BuiltinCommandHandler, CommandResult,
    PromptPart,
};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
#[cfg(test)]
use lingxi_core::host::ActiveGoalSnapshot;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
const TRUST_GATE_MESSAGE: &str = "/goal is only available in trusted workspaces. Restart, accept the trust dialog, and try again.";

/// `kEt`'s hooks-restricted failure message, verbatim from the locked
/// output-string list. The value comes from merged managed/user/project hook
/// policy through [`OrchestratorHandle::hooks_restricted`].
const HOOKS_HELD_MESSAGE: &str = "/goal can't run yet: your organization's settings haven't been read, so it isn't known whether it allows hooks.";
const RELAYED_MESSAGE: &str = "This message was relayed from Slack, Teams or a project thread and may be someone else's, so /goal didn't set, clear or show the goal. To use /goal, send it yourself from the terminal, the Claude app, claude.ai/code or your own Remote Control app.";

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
fn directive_for(condition: &Utf16JsonProjection) -> Utf16JsonProjection {
    source_concat([
        PromptPart::Text("A session-scoped Stop hook is now active with condition: \""),
        PromptPart::Source(condition),
        PromptPart::Text(
            "\". Briefly acknowledge the goal, then immediately start (or continue) working toward it — treat the condition itself as your directive and do not pause to ask the user what to do. The hook will block stopping until the condition holds. It auto-clears once the condition is met — do not tell the user to run `/goal clear` after success; that's only for clearing a goal early.",
        ),
    ])
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
    async fn status(&self) -> Utf16JsonProjection {
        let Some(goal) = self.handle.get_active_goal().await else {
            return NO_GOAL_SET_USAGE.into();
        };
        let evaluations = match goal.iterations {
            0 => "not yet evaluated".to_owned(),
            1 => "1 turn".to_owned(),
            count => format!("{count} turns"),
        };
        let status = source_concat([
            PromptPart::Text("Goal active: "),
            PromptPart::Source(&goal.condition),
            PromptPart::Text(&format!(" ({evaluations})")),
        ]);
        let Some(reason) = goal.last_reason.as_ref().filter(|reason| {
            reason
                .string_units("")
                .is_some_and(|units| !units.is_empty())
        }) else {
            return status;
        };
        let trimmed = trim_source_text(reason);
        let units = trimmed.string_units("").unwrap_or_default();
        let first_line_end = units
            .iter()
            .position(|unit| *unit == 0x0a)
            .unwrap_or(units.len());
        let reason = source_from_units(units[..first_line_end].to_vec());
        source_concat([
            PromptPart::Source(&status),
            PromptPart::Text("\nLast check: "),
            PromptPart::Source(&reason),
        ])
    }

    async fn clear(&self) -> Utf16JsonProjection {
        match self.handle.clear_active_goal().await {
            None => NO_GOAL_SET.into(),
            Some(goal) => {
                telemetry::emit_command_completed(TELEMETRY_STOP_HOOK_REMOVED, "goal");
                source_concat([
                    PromptPart::Text("Goal cleared: "),
                    PromptPart::Source(&goal.condition),
                ])
            }
        }
    }

    async fn set(&self, condition: &Utf16JsonProjection) -> CommandResult {
        // 2.1.270 `ust`: hooks restrictions take precedence over trust.
        if self.handle.hooks_restricted().await {
            self.handle.retire_goal_set_origin(condition).await;
            telemetry::emit_command_failed(TELEMETRY_GOAL_SET_EVENT, "hooks_gate");
            return CommandResult::Done {
                display: Some((HOOKS_RESTRICTED_MESSAGE.to_string()).into()),
            };
        }
        if self.handle.goal_hooks_held().await {
            self.handle.retire_goal_set_origin(condition).await;
            telemetry::emit_command_failed(TELEMETRY_GOAL_SET_EVENT, "hooks_held");
            return CommandResult::Done {
                display: Some(HOOKS_HELD_MESSAGE.into()),
            };
        }
        if !self.handle.goal_is_non_interactive() && !self.handle.workspace_trusted().await {
            self.handle.retire_goal_set_origin(condition).await;
            telemetry::emit_command_failed(TELEMETRY_GOAL_SET_EVENT, "trust_gate");
            return CommandResult::Done {
                display: Some((TRUST_GATE_MESSAGE.to_string()).into()),
            };
        }
        self.handle.set_active_goal(condition).await;
        let display = source_concat([
            PromptPart::Text("Goal set: "),
            PromptPart::Source(condition),
        ]);
        self.handle.emit_background_system_notice(&display).await;
        CommandResult::InjectMessage {
            content: directive_for(condition).into(),
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for GoalHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let source = lingxi_core::host::goal::current_goal_command_source();
        if source.dispatched_over_bridge
            && source.submission_verified_slack_human_turn
            && telemetry::flag_bool("tengu_mellow_sunset", true)
        {
            return CommandResult::Done {
                display: Some(RELAYED_MESSAGE.into()),
            };
        }
        let trimmed = trim_source_text(&args.raw_args);

        // 1) empty → status.
        if trimmed
            .string_units("")
            .is_some_and(|units| units.is_empty())
        {
            return CommandResult::Done {
                display: Some(self.status().await),
            };
        }

        // 2) case-insensitive clear token → clear.
        let is_clear = trimmed
            .string_at("")
            .is_some_and(|value| CLEAR_TOKENS.contains(&value.to_lowercase().as_str()));
        if is_clear {
            return CommandResult::Done {
                display: Some(self.clear().await),
            };
        }

        // 2.1.270 `e.length`: JavaScript counts UTF-16 code units.
        let n = trimmed.string_units("").map_or(0, |units| units.len());
        if n > MAX_CONDITION_CHARS {
            telemetry::emit_command_failed(TELEMETRY_GOAL_SET_EVENT, TELEMETRY_TOO_LONG_PROPERTY);
            return CommandResult::Done {
                display: Some((too_long_message(n)).into()),
            };
        }

        // 4) else → gate + set.
        self.set(&trimmed).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;
    use orchestrator::test_support::MockOrchestratorHandle;

    #[test]
    fn goal_directive_matches_latest_oracle_bytes() {
        let fixture =
            Utf16JsonProjection::parse(include_str!("../../tests/fixtures/goal_2_1_322.json"))
                .unwrap();
        let expected = fixture
            .string_at("/directive")
            .unwrap()
            .replace("{condition}", "ship it");
        let actual = directive_for(&"ship it".into());
        assert_eq!(actual.string_at(""), Some(expected.as_str()));
    }

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "goal".into(),
            raw_args: raw.into(),
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
                assert_eq!(s, "No goal set. Usage: `/goal <condition>`".into());
            }
            other => panic!("expected Done, got {other:?}"),
        }
        // Whitespace-only args trim to empty too.
        match h.handle(&args("   ")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "No goal set. Usage: `/goal <condition>`".into());
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clear_with_no_goal_reports_no_goal_set() {
        let h = handler();
        for token in ["clear", "STOP", "Off", "reset", "none", "cancel"] {
            match h.handle(&args(token)).await {
                CommandResult::Done { display: Some(s) } => assert_eq!(s, "No goal set".into()),
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
                assert_eq!(
                    s,
                    "Goal condition is limited to 4000 characters (got 4001)".into()
                );
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
                "Goal condition is limited to 4000 characters (got 4001)".into()
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
                condition: "ship".into(),
                set_at: SystemTime::now(),
                last_reason: Some(reason.into()),
                iterations: 1,
                tokens_at_start: 0,
            }));
            assert_eq!(
                h.status().await,
                format!("Goal active: ship (1 turn){suffix}").into()
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
                assert_eq!(s, TRUST_GATE_MESSAGE.into());
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
            } => assert_eq!(text, HOOKS_RESTRICTED_MESSAGE.into()),
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
                assert_eq!(s, HOOKS_RESTRICTED_MESSAGE.into());
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
                assert_eq!(
                    s,
                    "Goal active: finish the migration (not yet evaluated)".into()
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }

        match h.handle(&args("clear")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Goal cleared: finish the migration".into());
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
            condition: "ship".into(),
            set_at: SystemTime::now(),
            last_reason: Some("tests pending".into()),
            iterations: 1,
            tokens_at_start: 100,
        }));
        let one = h.status().await;
        assert_eq!(
            one,
            "Goal active: ship (1 turn)\nLast check: tests pending".into()
        );

        handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
            condition: "ship".into(),
            set_at: SystemTime::now(),
            last_reason: Some("review pending".into()),
            iterations: 2,
            tokens_at_start: 100,
        }));
        let many = h.status().await;
        assert_eq!(
            many,
            "Goal active: ship (2 turns)\nLast check: review pending".into()
        );

        // `Hr(t)` keeps only the first line of a multi-line evaluator reason.
        handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
            condition: "ship".into(),
            set_at: SystemTime::now(),
            last_reason: Some("  first line\nsecond line\nthird  ".into()),
            iterations: 3,
            tokens_at_start: 100,
        }));
        assert_eq!(
            h.status().await,
            "Goal active: ship (3 turns)\nLast check: first line".into()
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

    #[tokio::test]
    async fn native_322_command_replay_preserves_exact_outputs_and_gate_order() {
        use lingxi_core::host::goal::{with_goal_command_source, GoalCommandSource};

        let fixture =
            Utf16JsonProjection::parse(include_str!("../../tests/fixtures/goal_2_1_322.json"))
                .unwrap();
        let commands = fixture.at_pointer("/commands").unwrap();
        for index in 0..commands.array_len().unwrap() {
            let case = commands.get_index(index).unwrap().to_projection().unwrap();
            let name = case.string_at("/name").unwrap();
            let boolean = |path: &str, fallback| {
                case.at_pointer(path)
                    .and_then(|value| value.as_bool())
                    .unwrap_or(fallback)
            };
            let (handle, handler) = mock_handler();
            handle.set_hooks_restricted(boolean("/gate/hooksRestricted", false));
            handle.set_goal_hooks_held(boolean("/gate/hooksHeld", false));
            handle.set_workspace_trusted(boolean("/gate/trusted", true));
            handle.set_goal_non_interactive(boolean("/gate/nonInteractive", false));
            if case
                .at_pointer("/state")
                .is_some_and(|state| !state.is_null())
            {
                handle.set_active_goal_snapshot(Some(ActiveGoalSnapshot {
                    condition: case
                        .at_pointer("/state/condition")
                        .unwrap()
                        .to_projection()
                        .unwrap(),
                    set_at: SystemTime::UNIX_EPOCH,
                    last_reason: case
                        .at_pointer("/state/lastReason")
                        .filter(|reason| !reason.is_null())
                        .map(|reason| reason.to_projection().unwrap()),
                    iterations: case
                        .at_pointer("/state/iterations")
                        .and_then(|value| value.as_number())
                        .unwrap_or(0.0) as u64,
                    tokens_at_start: 0,
                }));
            }
            let enabled = boolean("/relay/enabled", true);
            telemetry::test_set_flag("tengu_mellow_sunset", enabled);
            let source = GoalCommandSource {
                dispatched_over_bridge: boolean("/relay/dispatchedOverBridge", false),
                submission_verified_slack_human_turn: boolean(
                    "/relay/submissionVerifiedSlackHumanTurn",
                    false,
                ),
            };
            let args = ParsedSlashCommand {
                name: "goal".into(),
                raw_args: case.at_pointer("/args").unwrap().to_projection().unwrap(),
                positional_args: Vec::new(),
            };
            let result = with_goal_command_source(source, handler.handle(&args)).await;
            telemetry::test_set_flag("tengu_mellow_sunset", true);
            let expected_value = case
                .at_pointer("/expected/value")
                .unwrap()
                .to_projection()
                .unwrap();
            match (case.string_at("/expected/type"), result) {
                (
                    Some("text"),
                    CommandResult::Done {
                        display: Some(display),
                    },
                ) => {
                    assert_eq!(display, expected_value, "{name}");
                    assert!(handle.background_notices().is_empty(), "{name}");
                }
                (Some("query"), CommandResult::InjectMessage { content }) => {
                    let expected_prompt = case
                        .at_pointer("/expected/prompt")
                        .unwrap()
                        .to_projection()
                        .unwrap();
                    assert_eq!(content, expected_prompt, "{name}");
                    assert_eq!(handle.background_notices(), vec![expected_value], "{name}");
                }
                (_, other) => panic!("{name}: unexpected {other:?}"),
            }
            let actual_state = handle.get_active_goal().await;
            let expected_state = case.at_pointer("/afterState").unwrap();
            if expected_state.is_null() {
                assert!(actual_state.is_none(), "{name}: no goal is active");
            } else {
                let actual = actual_state.expect(name);
                let expected_condition = case
                    .at_pointer("/afterState/condition")
                    .unwrap()
                    .to_projection()
                    .unwrap();
                assert_eq!(actual.condition, expected_condition, "{name}: condition");
                let iterations = case
                    .at_pointer("/afterState/iterations")
                    .and_then(|value| value.as_number())
                    .unwrap_or(0.0) as u64;
                assert_eq!(actual.iterations, iterations, "{name}: iterations");
                let reason = case
                    .at_pointer("/afterState/lastReason")
                    .filter(|value| !value.is_null())
                    .map(|value| value.to_projection().unwrap());
                assert_eq!(actual.last_reason, reason, "{name}: last check");
            }
            assert_eq!(
                lingxi_core::host::goal::current_goal_command_source(),
                GoalCommandSource::default()
            );
        }
    }
}
