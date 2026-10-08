//! Prompt hook executor — evaluates an inline single-turn LLM query.
//!
//! Byte-faithful port of `claude-code/src/utils/hooks/execPromptHook.ts`. A
//! `prompt` hook runs the hook prompt (with `$ARGUMENTS` substituted by the
//! serialized event payload) as a single-turn, non-streaming model query
//! against a fixed system prompt, then maps the model's `{ok, reason?}` JSON
//! response to a hook outcome:
//!
//! - `{"ok": true}`  ⇒ `HookOutcome::Success`, no decision (condition met).
//! - `{"ok": false, "reason": …}` ⇒ `HookOutcome::Success` carrying a
//!   `HookResponse` with `decision: Block`, `reason`, and
//!   `prevent_continuation: true` (`execPromptHook.ts:154-168` — the TS
//!   `outcome: 'blocking'` + `blockingError` + `preventContinuation: true` +
//!   `stopReason`). The Rust [`HookResponse`] models the block via
//!   `decision`/`reason`/`prevent_continuation`; the raw transport
//!   `HookOutcome` stays `Success` because the model call itself succeeded.
//! - response not valid JSON, or JSON that does not conform to the `{ok}`
//!   schema ⇒ `HookOutcome::Error` (`execPromptHook.ts:113-151`
//!   `outcome: 'non_blocking_error'`).
//! - the runner returning an error ⇒ `HookOutcome::Error`
//!   (`execPromptHook.ts:194-210`).
//! - the runner reporting cancellation/timeout ⇒ `HookOutcome::Cancelled` /
//!   `HookOutcome::Timeout` (`execPromptHook.ts:186-191` aborted-signal path).
//!
//! ## Seam ([`HookPromptRunner`])
//!
//! The hooks crate must NOT depend on the api-client. The single-turn model
//! call is therefore abstracted behind [`HookPromptRunner`]: the orchestrator
//! (where the api-client handle lives) implements it by reusing its existing
//! one-shot non-streaming `messages_create` call — the same call the
//! compaction summary / title-generation paths use — and injects it via
//! [`crate::HookExecutorImpl::with_prompt_runner`]. When no runner is wired the
//! arm is a structured "not wired" no-op (it never blocks).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::definition::HookDefinition;
use crate::response::{HookDecision, HookOutcome, HookResponse, HookResult};

/// Fixed system prompt the prompt hook evaluates against (`execPromptHook.ts`,
/// v2.1.263). Both evaluator prompts describe the current agent session.
pub(crate) const PROMPT_HOOK_SYSTEM_PROMPT: &str = r#"You are evaluating a hook condition in this agent session. Judge whether the user-provided condition is met.

Your response must be a JSON object with one of these shapes:
- {"ok": true, "reason": "<reason the condition is met>"}
- {"ok": false, "reason": "<reason the condition is not met>"}

Always include a "reason" field."#;

/// Stop evaluators must judge transcript evidence, including impossible conditions.
pub(crate) const STOP_PROMPT_HOOK_SYSTEM_PROMPT: &str = r#"You are evaluating a stop-condition hook in this agent session. Read the conversation transcript carefully, then judge whether the user-provided condition is satisfied.

Your response must be a JSON object with one of these shapes:
- {"ok": true, "reason": "<quote evidence from the transcript that satisfies the condition>"}
- {"ok": false, "reason": "<quote what is missing or what blocks the condition>"}
- {"ok": false, "impossible": true, "reason": "<explain why the condition can never be satisfied>"}

Always include a "reason" field, quoting specific text from the transcript whenever possible. If the transcript does not contain clear evidence that the condition is satisfied, return {"ok": false, "reason": "insufficient evidence in transcript"}.

Only use {"ok": false, "impossible": true} when the condition is genuinely unachievable in this session — for example: the condition is self-contradictory, it depends on a resource or capability that is unavailable, or the assistant has explicitly tried, exhausted reasonable approaches, and stated it cannot be done. Apply your own judgment when deciding this — the assistant claiming the goal is impossible is evidence, not proof; independently confirm the condition is genuinely unachievable rather than deferring to the assistant's self-assessment. Do not use it just because the goal has not been reached yet or because progress is slow. When in doubt, return {"ok": false} without "impossible"."#;

/// Default prompt-hook timeout (30 s — `execPromptHook.ts:55`
/// `hook.timeout ? hook.timeout * 1000 : 30000`).
pub const HOOK_PROMPT_TIMEOUT_MS: u64 = 30_000;

/// Live evaluator input, carried separately from the public hook JSON envelope.
#[derive(Debug, Clone, Default)]
pub struct PromptHookTranscript {
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    pub last_usage_tokens: usize,
    /// Original `(isVirtual, resumedFromIncompleteThinking)` assistant flags.
    pub message_grouping: std::collections::HashMap<lingxi_core::types::MessageId, (bool, bool)>,
}

#[derive(Debug, Clone)]
pub struct PromptHookRequest {
    /// Host-owned route of the immediate agent whose transcript is evaluated.
    pub model_selection: Option<crate::registry::HookModelSelection>,
    /// Current host history, including messages not yet persisted.
    pub transcript: Option<PromptHookTranscript>,
    /// Host-provided transcript path from the event envelope, never hook output.
    pub transcript_path: Option<std::path::PathBuf>,
    /// The user-turn prompt: the hook's `prompt` with `$ARGUMENTS` substituted
    /// by the serialized event payload (`execPromptHook.ts:35,42`).
    pub prompt: String,
    /// The fixed evaluation system prompt ([`PROMPT_HOOK_SYSTEM_PROMPT`];
    /// `execPromptHook.ts:64-70`). Passed explicitly so the runner stays a dumb
    /// transport with no hook-specific knowledge.
    pub system_prompt: String,
    /// Optional model override (`hook.model`; `execPromptHook.ts:79`). When
    /// `None` the runner falls back to its default small-fast model
    /// (`getSmallFastModel()`).
    pub model: Option<String>,
    /// Effective per-hook timeout (`execPromptHook.ts:55`). The runner SHOULD
    /// enforce it; the executor also surfaces a timeout result if the runner
    /// reports one.
    pub timeout: Duration,
}

/// Error surfaced by a [`HookPromptRunner`].
///
/// Mirrors the failure modes `execPromptHook.ts` distinguishes: an aborted /
/// timed-out query (`combinedSignal.aborted` ⇒ `outcome: 'cancelled'`) versus
/// any other thrown error (`outcome: 'non_blocking_error'`).
#[derive(Debug, thiserror::Error)]
pub enum PromptHookError {
    /// The query was cancelled (abort signal fired) before completing.
    #[error("prompt hook cancelled")]
    Cancelled,
    /// The query exceeded the effective timeout.
    #[error("prompt hook timed out after {0:?}")]
    Timeout(Duration),
    /// Any other failure (network, provider error, etc.).
    #[error("prompt hook query failed: {0}")]
    Query(String),
}

/// Runs a single-turn, non-streaming LLM query for a prompt hook and returns
/// the model's raw text response.
///
/// Implemented in the orchestrator over its existing one-shot
/// `OrchestratorApiClient::messages_create` call (the compaction-summary /
/// title-generation pattern). The hooks crate only knows this trait — it never
/// links the api-client, keeping the dependency edge one-way.
#[async_trait]
pub trait HookPromptRunner: Send + Sync {
    /// Execute the query, returning the assistant's concatenated text content
    /// (the analog of `extractTextContent(response.message.content)`;
    /// `execPromptHook.ts:105`).
    async fn run(&self, req: PromptHookRequest) -> Result<String, PromptHookError>;
}

/// Parsed `{ok, reason?}` model response (`hookHelpers.ts:16-24`
/// `hookResponseSchema`).
#[derive(Debug, Deserialize)]
struct PromptHookResponse {
    ok: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    impossible: bool,
}

/// Telemetry hint for the caller (parallels `HttpExecutionSignal` /
/// `AgentExecutionSignal`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptExecutionSignal {
    /// Query completed (success / parse-error / condition-not-met).
    Ok,
    /// Query exceeded the effective timeout.
    TimedOut,
    /// No [`HookPromptRunner`] was wired on the [`crate::HookExecutorImpl`].
    NotWired,
}

pub(crate) struct PromptExecutionOutcome {
    pub(crate) result: HookResult,
    pub(crate) signal: PromptExecutionSignal,
}

pub(crate) struct PromptExecutor {
    pub(crate) model_selection: Option<crate::registry::HookModelSelection>,
    pub(crate) transcript: Option<PromptHookTranscript>,
    pub(crate) runner: Option<Arc<dyn HookPromptRunner>>,
    pub(crate) timeout: Duration,
}

impl PromptExecutor {
    #[allow(dead_code)]
    pub(crate) fn new(runner: Option<Arc<dyn HookPromptRunner>>, timeout: Duration) -> Self {
        Self {
            model_selection: None,
            runner,
            timeout,
            transcript: None,
        }
    }

    /// Execute one Prompt hook.
    ///
    /// `prompt_template` is the hook's raw prompt; `payload_json` is the
    /// serialized event envelope spliced in via [`add_arguments_to_prompt`]
    /// (`addArgumentsToPrompt`; `execPromptHook.ts:35`).
    #[allow(
        clippy::too_many_lines,
        reason = "single execute path threads the runner call → parse → outcome mapping byte-faithfully to execPromptHook.ts"
    )]
    pub(crate) async fn execute(
        &self,
        hook: &HookDefinition,
        prompt_template: &str,
        model: Option<&str>,
        continue_on_block: bool,
        payload_json: &str,
    ) -> PromptExecutionOutcome {
        let Some(runner) = self.runner.clone() else {
            // `execPromptHook.ts` always has a model call available; here the
            // runner is optional. With none wired the arm is a strict no-op
            // structured "not wired" error — it never contributes a Block.
            return PromptExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Error,
                    stdout: String::new(),
                    stderr: format!("Hook {} failed: prompt executor not wired", hook.id),
                    exit_code: None,
                    response: None,
                },
                signal: PromptExecutionSignal::NotWired,
            };
        };

        // Replace `$ARGUMENTS` with the JSON input (`execPromptHook.ts:35`).
        let payload = serde_json::from_str::<serde_json::Value>(payload_json).ok();
        let is_stop = matches!(
            payload
                .as_ref()
                .and_then(|p| p.get("hook_event_name"))
                .and_then(serde_json::Value::as_str),
            Some("Stop" | "SubagentStop")
        );
        let condition = if is_stop {
            format!(
                "Based on the conversation transcript above, has the following stopping condition been satisfied? Answer based on transcript evidence only.\n\nCondition: {prompt_template}"
            )
        } else {
            prompt_template.to_owned()
        };
        let processed_prompt = add_arguments_to_prompt(&condition, payload_json);

        let req = PromptHookRequest {
            model_selection: self.model_selection.clone(),
            transcript: self.transcript.clone(),
            transcript_path: payload
                .as_ref()
                .and_then(|p| {
                    p.get("agent_transcript_path")
                        .or_else(|| p.get("transcript_path"))
                })
                .and_then(serde_json::Value::as_str)
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from),
            prompt: processed_prompt,
            system_prompt: if is_stop {
                STOP_PROMPT_HOOK_SYSTEM_PROMPT
            } else {
                PROMPT_HOOK_SYSTEM_PROMPT
            }
            .to_string(),
            model: model.map(str::to_owned),
            timeout: self.timeout,
        };

        let raw = match runner.run(req).await {
            Ok(text) => text,
            Err(PromptHookError::Cancelled) => {
                // `combinedSignal.aborted` ⇒ `outcome: 'cancelled'`
                // (`execPromptHook.ts:186-190`).
                return PromptExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Cancelled,
                        stdout: String::new(),
                        stderr: format!("Hook {} cancelled", hook.id),
                        exit_code: None,
                        response: None,
                    },
                    signal: PromptExecutionSignal::Ok,
                };
            }
            Err(PromptHookError::Timeout(d)) => {
                return PromptExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Timeout,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: timeout after {}ms",
                            hook.id,
                            d.as_millis()
                        ),
                        exit_code: None,
                        response: None,
                    },
                    signal: PromptExecutionSignal::TimedOut,
                };
            }
            Err(PromptHookError::Query(msg)) => {
                // Outer `catch` ⇒ `outcome: 'non_blocking_error'`
                // (`execPromptHook.ts:194-210`).
                return PromptExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Error executing prompt hook: {msg}"),
                        exit_code: None,
                        response: None,
                    },
                    signal: PromptExecutionSignal::Ok,
                };
            }
        };

        // `fullResponse = content.trim()` (`execPromptHook.ts:110`).
        let full_response = raw.trim().to_string();

        // `safeParseJSON(fullResponse)` (`execPromptHook.ts:113`).
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&full_response) else {
            // Parse failure ⇒ `non_blocking_error` with stderr "JSON validation
            // failed" + the raw response on stdout (`execPromptHook.ts:114-131`).
            return PromptExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Error,
                    stdout: full_response,
                    stderr: "JSON validation failed".to_string(),
                    exit_code: None,
                    response: None,
                },
                signal: PromptExecutionSignal::Ok,
            };
        };

        // `hookResponseSchema().safeParse(json)` (`execPromptHook.ts:133`).
        let parsed: PromptHookResponse = match serde_json::from_value(value) {
            Ok(p) => p,
            Err(e) => {
                return PromptExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Error,
                        stdout: full_response,
                        stderr: format!("Schema validation failed: {e}"),
                        exit_code: None,
                        response: None,
                    },
                    signal: PromptExecutionSignal::Ok,
                };
            }
        };

        if parsed.ok || (is_stop && parsed.impossible) {
            // Condition met ⇒ `outcome: 'success'` (`execPromptHook.ts:170-182`).
            PromptExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Success,
                    stdout: full_response,
                    stderr: String::new(),
                    exit_code: None,
                    response: (is_stop && !parsed.ok && parsed.impossible).then(|| HookResponse {
                        impossible: true,
                        reason: parsed.reason,
                        ..Default::default()
                    }),
                },
                signal: PromptExecutionSignal::Ok,
            }
        } else {
            // Condition NOT met ⇒ `outcome: 'blocking'` with a blocking error,
            // `preventContinuation: true`, and `stopReason` (`execPromptHook.ts:154-168`).
            // The Rust block is modeled on the parsed `HookResponse`:
            //   - `decision: Block` + `reason` is the blocking error surfaced to
            //     the user; the merge step copies it into the aggregate.
            //   - `prevent_continuation` carries the TS `preventContinuation`.
            let reason = parsed.reason.unwrap_or_default();
            PromptExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Success,
                    stdout: full_response,
                    stderr: String::new(),
                    exit_code: None,
                    response: Some(HookResponse {
                        decision: Some(HookDecision::Block),
                        // 2.1.195 surfaces the prompt-hook block as
                        // `[${e.prompt}]: ${reason}` — the hook's own (raw) prompt
                        // in brackets, then the model's reason (oracle off 519000).
                        reason: Some(format!("[{prompt_template}]: {reason}")),
                        // `continueOnBlock` (schemas/hooks.ts): default false →
                        // a block prevents continuation; true lets the turn proceed.
                        prevent_continuation: !is_stop && !continue_on_block,
                        ..Default::default()
                    }),
                },
                signal: PromptExecutionSignal::Ok,
            }
        }
    }
}

/// Splice the serialized hook input JSON into the prompt
/// (`addArgumentsToPrompt` → `substituteArguments`;
/// `utils/argumentSubstitution.ts:94-145`).
///
/// Ported behaviors (the load-bearing prompt-hook cases):
/// - `$ARGUMENTS` is replaced by the full JSON string.
/// - if the prompt contains NO placeholder and the JSON is non-empty,
///   `"\n\nARGUMENTS: {json}"` is appended.
///
/// Deviation (documented): the indexed (`$ARGUMENTS[n]`, `$n`) and named
/// (`$foo`) forms require `shell-quote` tokenization of the arguments string.
/// For a prompt hook the argument IS a single JSON blob (not a shell argument
/// list), and adding a `shell-quote` parser would pull a new external crate —
/// disallowed by scope. Those forms are therefore left untouched. The hook
/// schema documents `$ARGUMENTS` as the placeholder (`schemas/hooks.ts:70-73`),
/// which is the path exercised here.
pub(crate) fn add_arguments_to_prompt(prompt: &str, json_input: &str) -> String {
    if prompt.contains("$ARGUMENTS") {
        return prompt.replace("$ARGUMENTS", json_input);
    }
    if json_input.is_empty() {
        return prompt.to_string();
    }
    format!("{prompt}\n\nARGUMENTS: {json_input}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn goal_evaluator_preserves_oracle_logic_with_current_product_identity() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/goal_oracle_2_1_270.json"))
                .unwrap();
        assert_eq!(
            STOP_PROMPT_HOOK_SYSTEM_PROMPT.as_bytes(),
            fixture["stop_system_prompt"]
                .as_str()
                .unwrap()
                .replace("in Claude Code.", "in this agent session.")
                .as_bytes()
        );
    }

    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::HookEventType;
    use lingxi_core::types::HookId;
    use std::sync::Mutex;

    /// Records the request it received and returns a scripted result.
    struct MockRunner {
        recorded: Mutex<Vec<PromptHookRequest>>,
        result: Mutex<Option<Result<String, PromptHookError>>>,
    }
    impl MockRunner {
        fn ok(body: &str) -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Vec::new()),
                result: Mutex::new(Some(Ok(body.to_string()))),
            })
        }
        fn err(e: PromptHookError) -> Arc<Self> {
            Arc::new(Self {
                recorded: Mutex::new(Vec::new()),
                result: Mutex::new(Some(Err(e))),
            })
        }
    }
    #[async_trait]
    impl HookPromptRunner for MockRunner {
        async fn run(&self, req: PromptHookRequest) -> Result<String, PromptHookError> {
            self.recorded.lock().unwrap().push(req);
            self.result
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(PromptHookError::Query("no script".into())))
        }
    }

    fn make_prompt_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "test-prompt".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Prompt {
                prompt: "Is $ARGUMENTS safe?".into(),
                model: None,
                continue_on_block: false,
            },
            source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    #[tokio::test]
    async fn evaluator_forwards_live_history_outside_the_hook_payload() {
        let runner = MockRunner::ok(r#"{"ok":true,"reason":"yes"}"#);
        let mut exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        exec.transcript = Some(PromptHookTranscript {
            messages: vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "live evidence".into(),
            )],
            last_usage_tokens: 123,
            ..Default::default()
        });
        exec.execute(
            &make_prompt_hook(),
            "evaluate",
            None,
            false,
            r#"{"hook_event_name":"Stop"}"#,
        )
        .await;
        let calls = runner.recorded.lock().unwrap();
        let transcript = calls[0].transcript.as_ref().unwrap();
        assert_eq!(transcript.last_usage_tokens, 123);
        assert_eq!(transcript.messages.len(), 1);
        assert!(!calls[0].prompt.contains("live evidence"));
    }

    #[tokio::test]
    async fn stop_evaluator_uses_transcript_condition_without_preventing_continuation() {
        for event in ["Stop", "SubagentStop"] {
            let runner = MockRunner::ok(r#"{"ok":false,"reason":"tests still fail"}"#);
            let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
            let payload =
                serde_json::json!({"hook_event_name":event,"transcript_path":"/tmp/session.jsonl"})
                    .to_string();
            let outcome = exec
                .execute(&make_prompt_hook(), "all tests pass", None, false, &payload)
                .await;
            let response = outcome.result.response.unwrap();
            assert_eq!(response.decision, Some(HookDecision::Block));
            assert!(!response.prevent_continuation);
            let calls = runner.recorded.lock().unwrap();
            assert_eq!(calls[0].system_prompt, STOP_PROMPT_HOOK_SYSTEM_PROMPT);
            assert!(calls[0]
                .prompt
                .starts_with("Based on the conversation transcript above,"));
            assert_eq!(
                calls[0].transcript_path.as_deref(),
                Some(std::path::Path::new("/tmp/session.jsonl"))
            );
        }
    }

    #[tokio::test]
    async fn impossible_only_releases_stop_conditions() {
        for (event, blocked) in [
            ("Stop", false),
            ("SubagentStop", false),
            ("PreToolUse", true),
        ] {
            let runner = MockRunner::ok(
                r#"{"ok":false,"impossible":true,"reason":"unavailable capability"}"#,
            );
            let exec = PromptExecutor::new(Some(runner), Duration::from_secs(5));
            let payload = serde_json::json!({"hook_event_name":event}).to_string();
            let result = exec
                .execute(&make_prompt_hook(), "condition", None, false, &payload)
                .await
                .result;
            assert_eq!(
                result.response.as_ref().and_then(|r| r.decision),
                blocked.then_some(HookDecision::Block)
            );
            assert_eq!(
                result.response.as_ref().is_some_and(|r| r.impossible),
                !blocked
            );
        }
    }

    #[test]
    fn general_evaluator_blank_lines_match_oracle() {
        assert!(PROMPT_HOOK_SYSTEM_PROMPT.contains("is met.\n\nYour response"));
        assert!(PROMPT_HOOK_SYSTEM_PROMPT.contains("not met>\"}\n\nAlways include"));
    }

    #[tokio::test]
    async fn ok_true_is_success_with_no_decision() {
        let runner = MockRunner::ok(r#"{"ok": true}"#);
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec
            .execute(
                &hook,
                "Is $ARGUMENTS safe?",
                None,
                false,
                r#"{"tool":"Bash"}"#,
            )
            .await;

        assert_eq!(outcome.signal, PromptExecutionSignal::Ok);
        assert!(matches!(outcome.result.outcome, HookOutcome::Success));
        assert!(
            outcome.result.response.is_none(),
            "condition-met returns no decision"
        );
        // The request carried the substituted prompt + fixed system prompt.
        let recorded = runner.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].prompt, r#"Is {"tool":"Bash"} safe?"#);
        assert_eq!(recorded[0].system_prompt, PROMPT_HOOK_SYSTEM_PROMPT);
        assert_eq!(recorded[0].model, None);
    }

    #[tokio::test]
    async fn ok_false_blocks_with_reason_and_prevent_continuation() {
        let runner = MockRunner::ok(r#"{"ok": false, "reason": "rm -rf is dangerous"}"#);
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert_eq!(outcome.signal, PromptExecutionSignal::Ok);
        // The transport call succeeded; the block is carried on the response.
        assert!(matches!(outcome.result.outcome, HookOutcome::Success));
        let resp = outcome.result.response.expect("block response present");
        assert_eq!(resp.decision, Some(HookDecision::Block));
        assert_eq!(
            resp.reason.as_deref(),
            Some("[vet it]: rm -rf is dangerous")
        );
        assert!(resp.prevent_continuation);
    }

    #[tokio::test]
    async fn ok_false_with_continue_on_block_allows_continuation() {
        // `continueOnBlock: true` keeps decision:Block + reason but lets the turn
        // proceed (prevent_continuation = !continue_on_block = false).
        let runner = MockRunner::ok(r#"{"ok": false, "reason": "advisory only"}"#);
        let exec = PromptExecutor::new(Some(runner), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, true, "{}").await;

        let resp = outcome.result.response.expect("block response present");
        assert_eq!(resp.decision, Some(HookDecision::Block));
        assert!(
            !resp.prevent_continuation,
            "continueOnBlock=true → may continue"
        );
    }

    #[tokio::test]
    async fn ok_false_without_reason_blocks_with_empty_reason() {
        let runner = MockRunner::ok(r#"{"ok": false}"#);
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        let resp = outcome.result.response.expect("block response present");
        assert_eq!(resp.decision, Some(HookDecision::Block));
        assert_eq!(resp.reason.as_deref(), Some("[vet it]: "));
        assert!(resp.prevent_continuation);
    }

    #[tokio::test]
    async fn invalid_json_is_non_blocking_error() {
        let runner = MockRunner::ok("not json at all");
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert_eq!(outcome.signal, PromptExecutionSignal::Ok);
        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert_eq!(outcome.result.stderr, "JSON validation failed");
        assert_eq!(outcome.result.stdout, "not json at all");
        assert!(outcome.result.response.is_none());
    }

    #[tokio::test]
    async fn json_missing_ok_field_is_schema_error() {
        let runner = MockRunner::ok(r#"{"reason": "no ok field"}"#);
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert!(
            outcome
                .result
                .stderr
                .starts_with("Schema validation failed:"),
            "stderr was {:?}",
            outcome.result.stderr
        );
        assert!(outcome.result.response.is_none());
    }

    #[tokio::test]
    async fn no_runner_is_not_wired_noop() {
        let exec = PromptExecutor::new(None, Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert_eq!(outcome.signal, PromptExecutionSignal::NotWired);
        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert!(outcome.result.stderr.contains("prompt executor not wired"));
        // Crucially: no Block decision — a no-runner prompt hook never blocks.
        assert!(outcome.result.response.is_none());
    }

    #[tokio::test]
    async fn cancelled_runner_maps_to_cancelled_outcome() {
        let runner = MockRunner::err(PromptHookError::Cancelled);
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert_eq!(outcome.signal, PromptExecutionSignal::Ok);
        assert!(matches!(outcome.result.outcome, HookOutcome::Cancelled));
        assert!(outcome.result.response.is_none());
    }

    #[tokio::test]
    async fn timeout_runner_maps_to_timeout_outcome() {
        let runner = MockRunner::err(PromptHookError::Timeout(Duration::from_secs(30)));
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(30));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert_eq!(outcome.signal, PromptExecutionSignal::TimedOut);
        assert!(matches!(outcome.result.outcome, HookOutcome::Timeout));
    }

    #[tokio::test]
    async fn query_error_is_non_blocking_error() {
        let runner = MockRunner::err(PromptHookError::Query("provider 500".into()));
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let outcome = exec.execute(&hook, "vet it", None, false, "{}").await;

        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert!(
            outcome
                .result
                .stderr
                .contains("Error executing prompt hook: provider 500"),
            "stderr was {:?}",
            outcome.result.stderr
        );
        assert!(outcome.result.response.is_none());
    }

    #[tokio::test]
    async fn model_override_is_threaded_to_runner() {
        let runner = MockRunner::ok(r#"{"ok": true}"#);
        let exec = PromptExecutor::new(Some(runner.clone()), Duration::from_secs(5));
        let hook = make_prompt_hook();

        let _ = exec
            .execute(&hook, "vet it", Some("claude-sonnet-4-6"), false, "{}")
            .await;

        let recorded = runner.recorded.lock().unwrap();
        assert_eq!(recorded[0].model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn add_arguments_replaces_placeholder() {
        assert_eq!(
            add_arguments_to_prompt("check $ARGUMENTS now", r#"{"a":1}"#),
            r#"check {"a":1} now"#
        );
    }

    #[test]
    fn add_arguments_appends_when_no_placeholder() {
        assert_eq!(
            add_arguments_to_prompt("evaluate this", r#"{"a":1}"#),
            "evaluate this\n\nARGUMENTS: {\"a\":1}"
        );
    }

    #[test]
    fn add_arguments_no_placeholder_empty_json_leaves_prompt() {
        assert_eq!(
            add_arguments_to_prompt("evaluate this", ""),
            "evaluate this"
        );
    }
}
