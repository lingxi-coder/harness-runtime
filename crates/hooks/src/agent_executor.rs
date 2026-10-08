//! Agent hook executor — fires the hook by spawning a subagent.
//!
//! Spawns `agent_type` via the injected `Arc<dyn SubagentSpawner>`, splicing
//! the hook event payload into the subagent's initial prompt. Awaits the
//! terminal `SubagentResult` and treats `Completed { content }`'s string
//! content as the hook's JSON response body (parsed via
//! [`crate::hook_payload::parse_response`]).
//!
//! Default timeout: [`crate::executor::HOOK_AGENT_TIMEOUT_MS`] (60 s).
//! Cancellation: `SubagentResult::Killed` ⇒ `HookOutcome::Cancelled`.
//! `Failed { reason }` ⇒ `HookOutcome::Error`.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use lingxi_core::host::subagent_spawn::SubagentInheritance;
use lingxi_core::host::subagent_spawn::{SubagentResult, SubagentSpawnRequest, SubagentSpawner};

use crate::definition::HookDefinition;
use crate::hook_payload::parse_response;
use crate::response::{HookOutcome, HookResponse, HookResult};

/// Telemetry hint for the caller (parallels [`HttpExecutionSignal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentExecutionSignal {
    /// Spawn completed (success / failure / cancelled).
    Ok,
    /// Spawn exceeded the effective timeout.
    TimedOut,
    /// No subagent spawner was wired on the [`HookExecutorImpl`].
    NotWired,
}

pub(crate) struct AgentExecutionOutcome {
    pub(crate) result: HookResult,
    pub(crate) signal: AgentExecutionSignal,
}

pub(crate) struct AgentExecutor {
    pub(crate) spawner: Option<Arc<dyn SubagentSpawner>>,
    pub(crate) timeout: Duration,
}

impl AgentExecutor {
    #[allow(dead_code)]
    pub(crate) fn new(spawner: Option<Arc<dyn SubagentSpawner>>, timeout: Duration) -> Self {
        Self { spawner, timeout }
    }

    /// Execute one Agent hook.
    ///
    /// `ctx.inherit` carries the parent's tool invoker + budget enforcer Arcs;
    /// it MUST be the same Arc the parent orchestrator holds (the
    /// recursion-lock invariant — `Arc::ptr_eq` assertion in
    /// `lingxi-tools/tests/agent_tool_recursion_lock_test.rs` must keep
    /// passing).
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "single execute path threads many params + spans subagent spawn → poll → response parse"
    )]
    pub(crate) async fn execute(
        &self,
        hook: &HookDefinition,
        agent_type: &str,
        prompt_template: &str,
        payload_json: &str,
        expected_event: &'static str,
        ctx: &crate::registry::HookContext,
        model: Option<&str>,
    ) -> AgentExecutionOutcome {
        let Some(spawner) = self.spawner.clone() else {
            return AgentExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Error,
                    stdout: String::new(),
                    stderr: format!("Hook {} failed: agent executor not wired", hook.id),
                    exit_code: None,
                    response: None,
                },
                signal: AgentExecutionSignal::NotWired,
            };
        };
        let Some(inherit) = ctx.inherit.clone() else {
            return AgentExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Error,
                    stdout: String::new(),
                    stderr: format!(
                        "Hook {} failed: subagent inheritance missing on HookContext",
                        hook.id
                    ),
                    exit_code: None,
                    response: None,
                },
                signal: AgentExecutionSignal::NotWired,
            };
        };

        let req = SubagentSpawnRequest {
            stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::AgentScoped,
            agent_spawn_provenance: Default::default(),
            teammate_color: None,
            depth: ctx.agent_depth.map_or(0, |depth| depth.saturating_add(1)),
            parent_model_override: ctx
                .model_selection
                .as_ref()
                .map(|route| route.model.clone()),
            parent_model_profile_override: ctx
                .model_selection
                .as_ref()
                .and_then(|route| route.model_profile.clone()),
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            subagent_type: agent_type.to_string(),
            prompt: format!("{prompt_template}\n\n{payload_json}"),
            observer: None,
            context_paths: Vec::new(),
            // AgentTool spawn-surface parity params — the hook executor sets no
            // teammate/isolation/cwd override, but threads the agent hook's
            // optional `model` (claude-code `schemas/hooks.ts` agent `model`).
            description: None,
            model: model.map(str::to_string),
            model_profile: None,
            // Hook-driven agents run synchronously inside the hook timeout.
            run_in_background: false,
            name: None,
            team_name: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: ctx.agent_id,
            mode: None,
            isolation: None,
            cwd: None,
            worktree: None,
            // Non-fork path: the hook executor never forks a parent
            // conversation, so the fork-subagent fields stay unset.
            fork_context_messages: None,
            instruction_context: None,
            fork_parent_system_prompt: None,
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: None,
            tool_use_id: None,
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            origin_session_id: (!ctx.session_id.as_uuid().is_nil()).then_some(ctx.session_id),
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            handback_opt_in: false,
            parent_permission_mode: ctx.permission_mode.clone(),
            handback_enabled: None,
            handback_ends_turn_enabled: None,
            restored_handback_state: None,
            restored_handback_history: Vec::new(),
            restore_handback_start: None,
        };

        let fut = spawner.spawn(req, inherit);
        let spawn_outcome = match tokio::time::timeout(self.timeout, fut).await {
            Err(_) => {
                return AgentExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Timeout,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: timeout after {}ms",
                            hook.id,
                            self.timeout.as_millis()
                        ),
                        exit_code: None,
                        response: None,
                    },
                    signal: AgentExecutionSignal::TimedOut,
                };
            }
            Ok(r) => r,
        };

        match spawn_outcome {
            Ok(SubagentResult::Completed { content, .. }) => {
                // The subagent's `content` can be either a String or a JSON
                // object. Try a few projections.
                let raw_str = match &content {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let parsed: Option<HookResponse> = parse_response(&raw_str, expected_event).ok();
                AgentExecutionOutcome {
                    result: HookResult {
                        outcome: HookOutcome::Success,
                        stdout: raw_str,
                        stderr: String::new(),
                        exit_code: None,
                        response: parsed,
                    },
                    signal: AgentExecutionSignal::Ok,
                }
            }
            Ok(SubagentResult::Failed { reason, .. }) => AgentExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Error,
                    stdout: String::new(),
                    stderr: format!("Hook {} failed: subagent failed: {reason}", hook.id),
                    exit_code: None,
                    response: None,
                },
                signal: AgentExecutionSignal::Ok,
            },
            Ok(SubagentResult::Killed { .. }) => AgentExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Cancelled,
                    stdout: String::new(),
                    stderr: format!("Hook {} cancelled (subagent killed)", hook.id),
                    exit_code: None,
                    response: None,
                },
                signal: AgentExecutionSignal::Ok,
            },
            Err(e) => AgentExecutionOutcome {
                result: HookResult {
                    outcome: HookOutcome::Error,
                    stdout: String::new(),
                    stderr: format!("Hook {} failed: subagent spawn error: {e}", hook.id),
                    exit_code: None,
                    response: None,
                },
                signal: AgentExecutionSignal::Ok,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{HookExecutor as DefHookExecutor, HookSource};
    use crate::events::HookEventType;
    use async_trait::async_trait;
    use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
    use lingxi_core::host::subagent_spawn::{SubagentResult, SubagentSpawnError, SubagentUsage};
    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolInvoker, ToolInvokerError,
    };
    use lingxi_core::types::HookId;
    use serde_json::json;
    use std::sync::Mutex;

    struct InertInvoker;
    #[async_trait]
    impl ToolInvoker for InertInvoker {
        async fn invoke(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            Err(ToolInvokerError::Internal("inert".into()))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    struct InertBudget;
    #[async_trait]
    impl BudgetEnforcerHandle for InertBudget {
        async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    fn make_agent_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "test-agent".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Agent {
                agent_type: "general-purpose".into(),
                prompt: "vet this".into(),
                model: None,
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

    fn dummy_inherit() -> SubagentInheritance {
        SubagentInheritance {
            tool_invoker: Arc::new(InertInvoker),
            budget: Arc::new(InertBudget),
        }
    }

    struct MockSpawner {
        result: Mutex<Option<Result<SubagentResult, SubagentSpawnError>>>,
    }
    #[async_trait]
    impl SubagentSpawner for MockSpawner {
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            assert_eq!(
                request.stop_hook_scope,
                lingxi_core::host::subagent_spawn::SubagentStopScope::AgentScoped
            );
            self.result
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(SubagentSpawnError::Internal("no script".into())))
        }
    }

    #[tokio::test]
    async fn happy_path_completed_parses_response_json() {
        let spawner = Arc::new(MockSpawner {
            result: Mutex::new(Some(Ok(SubagentResult::Completed {
                handback: None,
                agent_id: lingxi_core::types::AgentId::new(),
                content: json!(r#"{"decision":"approve"}"#),
                usage: SubagentUsage::default(),
                total_tool_use_count: 0,
                total_duration_ms: 0,
                total_tokens: 0,
                assistant_message_count: 0,
                response_char_count: 0,
                last_request_id: None,
                cumulative_usage: SubagentUsage::default(),
                usage_complete: true,
            }))),
        });
        let exec = AgentExecutor::new(Some(spawner.clone()), Duration::from_secs(5));
        let hook = make_agent_hook();

        let outcome = exec
            .execute(
                &hook,
                "general-purpose",
                "vet this",
                "{}",
                "PreToolUse",
                &crate::registry::HookContext {
                    inherit: Some(dummy_inherit()),
                    ..Default::default()
                },
                None,
            )
            .await;

        assert_eq!(outcome.signal, AgentExecutionSignal::Ok);
        assert!(matches!(outcome.result.outcome, HookOutcome::Success));
        let resp = outcome.result.response.expect("response parsed");
        assert_eq!(resp.decision, Some(crate::response::HookDecision::Approve));
    }

    #[tokio::test]
    async fn no_spawner_returns_not_wired() {
        let exec = AgentExecutor::new(None, Duration::from_secs(5));
        let hook = make_agent_hook();

        let outcome = exec
            .execute(
                &hook,
                "general-purpose",
                "vet",
                "{}",
                "PreToolUse",
                &crate::registry::HookContext {
                    inherit: Some(dummy_inherit()),
                    ..Default::default()
                },
                None,
            )
            .await;

        assert_eq!(outcome.signal, AgentExecutionSignal::NotWired);
        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
    }

    #[tokio::test]
    async fn subagent_failed_returns_error() {
        let spawner = Arc::new(MockSpawner {
            result: Mutex::new(Some(Ok(SubagentResult::Failed {
                agent_id: lingxi_core::types::AgentId::new(),
                reason: "no API key".into(),
                usage: lingxi_core::host::subagent_spawn::SubagentUsage::default(),
            }))),
        });
        let exec = AgentExecutor::new(Some(spawner.clone()), Duration::from_secs(5));
        let hook = make_agent_hook();

        let outcome = exec
            .execute(
                &hook,
                "general-purpose",
                "x",
                "{}",
                "PreToolUse",
                &crate::registry::HookContext {
                    inherit: Some(dummy_inherit()),
                    ..Default::default()
                },
                None,
            )
            .await;

        assert!(matches!(outcome.result.outcome, HookOutcome::Error));
        assert!(outcome.result.stderr.contains("no API key"));
    }
}
