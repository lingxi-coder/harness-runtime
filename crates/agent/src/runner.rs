//! Subagent state-machine loop.
//!
//! [`run_subagent`] is the future that
//! [`crate::pool::StateMachinePool::allocate`] hands to the runtime.
//!
//! Two modes coexist, selected on [`SubagentContext::api_client`]:
//!
//! * **Real multi-turn loop** (`api_client = Some`): an imperative loop that
//!   mirrors the orchestrator's `execute_one_turn` — call the model, append
//!   the assistant turn, dispatch any `tool_use` blocks through the inherited
//!   [`lingxi_core::host::ToolInvoker`], feed the results back as a user message, and
//!   repeat until the model stops (`end_turn` / no tool use) or `max_turns`
//!   is hit. A `UserExit` / `UserInterrupt` arriving on `event_rx` aborts the
//!   loop and surfaces [`SubagentEvent::Killed`]. When
//!   [`crate::context::SubagentContext::persistent`] is set, the loop does not
//!   return on a terminal stop: it parks awaiting the next inbound
//!   [`lingxi_core::Event::UserMessage`], appends it to history, and runs the next
//!   turn-set — modelling a long-lived, message-driven teammate.
//! * **Legacy stub** (`api_client = None`): the M1.11 reducer-driven stub that
//!   completes after the first inbound event. Retained for back-compat with
//!   callers that haven't wired an API client yet.

use crate::context::SubagentContext;
use futures::StreamExt;
use lingxi_core::host::WorkflowQueryWatchdog;
use lingxi_core::types::{AgentId, ConversationMessage, MessageId};
use llm_runtime::{HistoryEvent, LlmError};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

/// Events emitted by [`run_subagent`] back to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubagentEvent {
    /// Periodic progress beacon while the agent is still running.
    Progress {
        /// Agent emitting the progress event.
        agent_id: AgentId,
        /// Number of tool calls processed so far.
        tool_use_count: u32,
        /// Cumulative token count consumed so far.
        token_count: u64,
    },
    /// Agent finished normally with `result`.
    Completed {
        /// Agent that completed.
        agent_id: AgentId,
        /// Final result payload (free-form JSON).
        result: serde_json::Value,
        /// Wire usage from the FINAL model response (the spawner translates this
        /// into `lingxi_core::host::SubagentUsage` + the result-level token total). The
        /// legacy stub path has no real round-trips and emits `ExecutionUsage::default()`.
        usage: llm_runtime::ExecutionUsage,
        /// Number of tool-use blocks executed across the run (claude
        /// `totalToolUseCount`). `0` on the stub path.
        total_tool_use_count: u64,
        /// Wall-clock duration of the run in milliseconds (claude
        /// `totalDurationMs`). `0` on the stub path.
        total_duration_ms: u64,
        /// Number of assistant messages produced across the run (claude
        /// `agentMessages.length`, fed into `tengu_agent_tool_completed`'s
        /// `assistant_message_count`). `0` on the stub path.
        assistant_message_count: u64,
        /// The FINAL assistant turn's provider request id (claude
        /// `lastAssistantMessage.requestId`) — used to gate
        /// `tengu_cache_eviction_hint`. `None` on the stub path.
        last_request_id: Option<String>,
        /// Cross-turn summed usage. Distinct from [`Self::Completed::usage`].
        #[serde(default)]
        cumulative_usage: llm_runtime::ExecutionUsage,
        /// `false` only on the CC 2.1.207 `api_error_partial` SALVAGE path
        /// (Finding [9]): a mid-stream provider error whose already-produced
        /// text is recovered as a `Completed` result instead of discarding
        /// it. On that path `usage`/`cumulative_usage` above are the STALE
        /// values from the last turn that completed successfully BEFORE the
        /// error — the failing turn's own (real, provider-billed) tokens are
        /// not included, because they were never captured. `true` on every
        /// other path (clean stop, max-turns exhaustion, stub) where the
        /// usage fields are the real, complete totals.
        #[serde(default = "usage_complete_default")]
        usage_complete: bool,
    },
    /// Agent terminated due to an error.
    Failed {
        /// Agent that failed.
        agent_id: AgentId,
        /// Human-readable error message.
        error: String,
        /// Cross-turn summed usage from every SUCCESSFUL turn before the one
        /// that failed (same accumulation [`Self::Completed::cumulative_usage`]
        /// carries). Finding [9]/[11]: a `Failed` termination (provider
        /// error, idle-timeout watchdog, max-turns/structured-output
        /// exhaustion) still reflects real, already-billed provider spend
        /// from any turn that succeeded before it — this lets a caller price
        /// that spend instead of settling it at $0. `ExecutionUsage::default()` on
        /// every path that made no real round-trip (spawn-time failure, the
        /// legacy stub).
        #[serde(default)]
        cumulative_usage: llm_runtime::ExecutionUsage,
    },
    /// Agent was cancelled by the host.
    Killed {
        /// Agent that was killed.
        agent_id: AgentId,
    },
    /// A raw message produced by the agent (assistant or tool result).
    Message {
        /// Agent that produced the message.
        agent_id: AgentId,
        /// Free-form message payload (full schema lands in Plan 09+).
        message: serde_json::Value,
    },
}

/// `#[serde(default = ...)]` for [`SubagentEvent::Completed::usage_complete`]
/// / [`lingxi_core::host::subagent_spawn::SubagentResult::Completed::usage_complete`]
/// — an older wire payload with no such field must decode as `true` (a
/// normal complete usage rollup), not `bool::default()`'s `false`.
fn usage_complete_default() -> bool {
    true
}

fn completed_result_text(result: &serde_json::Value) -> Option<String> {
    if let Some(text) = result.get("text").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let content = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| {
                    (block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(serde_json::Value::as_str))
                        .flatten()
                        .map(str::to_string)
                })
                .collect::<Vec<String>>()
        })?;
    if content.is_empty() {
        None
    } else {
        Some(content.join("\n"))
    }
}

/// Milliseconds elapsed since `start`, saturated into a `u64` (claude
/// `totalDurationMs`).
fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const USAGE_LIMIT_NEAR_WRAP_UP_FLAG: &str = "tengu_vellum_anchor";
const NEAR_LIMIT_WRAP_UP_NOTE: &str = "[Usage limit approaching. Checkpoint now: finish the current step, then list up to 3 short bullets of the most impactful remaining work. Don't start subagents or long-running work.]";

fn usage_limit_near_wrap_up_enabled() -> bool {
    ::telemetry::flag_bool(USAGE_LIMIT_NEAR_WRAP_UP_FLAG, false)
}

async fn maybe_emit_near_limit_wrap_up(
    history: &mut Vec<ConversationMessage>,
    ctx: &SubagentContext,
    api_client: &dyn crate::api::SubagentApiClient,
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
) {
    if ctx.depth == 0 {
        return;
    }
    // Oracle evaluation order is `depth > 0 && consumePendingHint() &&
    // flag("tengu_vellum_anchor", false)`: a disabled flag still consumes and
    // drops the one-shot hint, preventing a stale emission if flags refresh.
    if !api_client.consume_pending_near_limit_wrap_up_hint() || !usage_limit_near_wrap_up_enabled()
    {
        return;
    }

    // `Le()` is the owning session's print/SDK gate. A background child of an
    // interactive session is still interactive for this purpose, so `is_async`
    // must not suppress the checkpoint. The oracle starts this fire-and-forget
    // work before yielding either the UI notice or the model-visible note.
    api_client.record_usage_limit_near_wrap_up();
    let non_interactive = ctx.session_interactive == Some(false);
    if !non_interactive {
        api_client.dispatch_near_limit_checkpoint(crate::api::NearLimitCheckpointRequest {
            session_id: ctx.hook_session_id,
            cwd: ctx.hook_cwd.clone(),
            non_interactive,
        });
    }

    let note =
        ConversationMessage::user_meta(MessageId::new(), NEAR_LIMIT_WRAP_UP_NOTE.to_string());
    history.push(note.clone());
    emit_message(out_tx, agent_id, &note).await;
}

fn workflow_watchdog_timeout_error(phase: &str, timeout: Duration) -> LlmError {
    LlmError::TransportTimeout {
        message: format!(
            "workflow model query stalled while {phase} for {}ms",
            timeout.as_millis()
        ),
    }
}

fn is_workflow_watchdog_timeout(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::TransportTimeout { message }
            if message.starts_with("workflow model query stalled while ")
    )
}

/// Apply the workflow watchdog to one model-query phase. This helper is used
/// only for stream establishment; tool execution is deliberately outside every
/// call site, so a slow tool cannot consume the model-query idle budget.
async fn await_workflow_query_phase<T, F>(
    future: F,
    watchdog: Option<WorkflowQueryWatchdog>,
    phase: &'static str,
) -> Result<T, LlmError>
where
    F: Future<Output = Result<T, LlmError>>,
{
    let Some(policy) = watchdog else {
        return future.await;
    };
    let timeout = Duration::from_millis(policy.stall_timeout_ms);
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(workflow_watchdog_timeout_error(phase, timeout)),
    }
}

/// Wrap a response stream with a per-event idle timeout. Every successful
/// `next()` starts a fresh timeout, so total stream lifetime is unbounded while
/// progress continues. The wrapper yields one typed timeout error then closes.
fn with_workflow_stream_watchdog(
    stream: futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>>,
    watchdog: Option<WorkflowQueryWatchdog>,
) -> futures::stream::BoxStream<'static, Result<HistoryEvent, LlmError>> {
    let Some(policy) = watchdog else {
        return stream;
    };
    let timeout = Duration::from_millis(policy.stall_timeout_ms);
    futures::stream::unfold(
        (stream, false),
        move |(mut stream, terminated)| async move {
            if terminated {
                return None;
            }
            match tokio::time::timeout(timeout, stream.next()).await {
                Ok(Some(event)) => Some((event, (stream, false))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(workflow_watchdog_timeout_error(
                        "waiting for the next response event",
                        timeout,
                    )),
                    (stream, true),
                )),
            }
        },
    )
    .boxed()
}

/// A cancelled/dropped worker future must not leave a pending hook snapshot.
struct PromptTranscriptCancellationGuard {
    executor: Option<std::sync::Arc<hooks::HookExecutorImpl>>,
    session_id: lingxi_core::types::SessionId,
    agent_id: lingxi_core::types::AgentId,
    completed: bool,
}

impl Drop for PromptTranscriptCancellationGuard {
    fn drop(&mut self) {
        if !self.completed {
            if let Some(executor) = &self.executor {
                executor.take_agent_prompt_transcript(self.session_id, self.agent_id);
            }
        }
    }
}

/// Subagent state-machine loop.
///
/// When [`SubagentContext::api_client`] is `Some`, drives the real
/// multi-turn agentic loop (see [`run_subagent_loop`]). Otherwise falls back
/// to the legacy reducer-driven stub (see [`run_subagent_stub`]). Both emit
/// [`SubagentEvent`]s on `out_tx`.
pub async fn run_subagent(
    ctx: SubagentContext,
    event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
) {
    let mut snapshot_cleanup = PromptTranscriptCancellationGuard {
        executor: ctx.hook_executor.clone(),
        session_id: ctx.hook_session_id,
        agent_id: ctx.agent_id,
        completed: false,
    };
    let non_interactive = ctx
        .session_interactive
        .map_or(ctx.is_async, |interactive| !interactive || ctx.is_async);
    // claude-code `agentCacheTtlOverride`: the definition's
    // `experimental.cacheTtl` rides the whole run, so every round-trip this
    // agent makes sees it. Read before `ctx` moves into the inner future.
    let wants_1h_cache = ctx
        .agent_definition
        .cache_ttl
        .is_some_and(crate::definition::AgentCacheTtl::wants_1h);
    llm_runtime::scope_agent_cache_ttl(
        wants_1h_cache,
        llm_runtime::thinking_scope::scope_thinking_recovery(
            llm_runtime::thinking_scope::ThinkingRecoveryScope::default(),
            lingxi_core::host::session_flags::scope_non_interactive_session(
                non_interactive,
                run_subagent_inner(ctx, event_rx, out_tx),
            ),
        ),
    )
    .await;
    snapshot_cleanup.completed = true;
}

async fn run_subagent_inner(
    ctx: SubagentContext,
    event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
) {
    // Keep a cleanup handle outside the body: both the real loop and the stub
    // contain many terminal returns, while this dispatcher always regains
    // control after either future completes.
    let diagnostics_cleanup = ctx.new_diagnostics_source.clone();
    // G4 (frontmatter hooks): register the agent definition's frontmatter hooks
    // scoped to this child `agent_id` BEFORE the run and clear them AFTER —
    // claude `registerFrontmatterHooks(…, isAgent=true)` (runAgent.ts:557-575)
    // then `clearSessionHooks(agentId)` in the `runAgent` finally. `isAgent=true`
    // retargets each `Stop` subscription to `SubagentStop` (a subagent's loop end
    // fires `SubagentStop`). Wrapped here at the dispatcher so the clear runs
    // regardless of how the body returns (the loop has many early returns), and
    // so an un-wired `hook_executor` (tests / minimal builds) is a strict no-op.
    //
    // (cc 2.1.218 `mvo`) ORIGIN TRUST gate: registering these hooks installs
    // COMMANDS, so a definition whose folder has never been trusted must not get
    // them — the `--add-dir <untrusted-repo>` case, where the repo ships
    // `<dot>/agents/*.md` with a `hooks:` block. 2.1.217 registered
    // unconditionally; 2.1.218 skips + logs + counts instead.
    let frontmatter_cleanup = match &ctx.hook_executor {
        Some(_)
            if !ctx.agent_definition.frontmatter_hooks.is_empty()
                && ctx.strict_plugin_only_hooks
                && !crate::mcp_servers::plugin_trusted_source(ctx.agent_definition.source) =>
        {
            tracing::warn!(
                agent = %ctx.agent_definition.agent_type,
                "Skipping agent frontmatter hooks: strictPluginOnlyCustomization locks hooks to plugin-only sources"
            );
            None
        }
        Some(he)
            if !ctx.agent_definition.frontmatter_hooks.is_empty()
                && (!ctx.strict_plugin_only_hooks
                    || crate::mcp_servers::plugin_trusted_source(ctx.agent_definition.source)) =>
        {
            let cwd = ctx
                .cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            if crate::hooks_trust::agent_hooks_origin_trusted(&ctx.agent_definition, &cwd) {
                he.register_agent_hooks(
                    ctx.agent_id,
                    &ctx.agent_definition.frontmatter_hooks,
                    true,
                )
                .await;
                Some((he.clone(), ctx.agent_id))
            } else {
                crate::hooks_trust::report_untrusted_hooks(
                    &ctx.agent_definition,
                    &cwd,
                    crate::hooks_trust::HooksTrustSurface::Subagent,
                    false,
                );
                None
            }
        }
        _ => None,
    };

    // #9 (dead SubagentStop): when this agent registered frontmatter hooks, the
    // `Stop`→`SubagentStop` retargeted ones (registerFrontmatterHooks isAgent=true)
    // must actually FIRE at the loop's natural end — claude fires the subagent's
    // stop hooks INSIDE the child (`stopHooks.ts` via `query.ts`, keyed on
    // `toolUseContext.agentId`). The LingXi orchestrator-side `SubagentStop`
    // chokepoint runs AFTER this dispatcher returns (i.e. after `clear_agent_hooks`
    // below), so those retargeted frontmatter hooks would be dead code without an
    // in-child fire. We fire SubagentStop here, AGENT-SCOPED to this child's own
    // bucket (see `execute_agent_scoped`), so session / plugin `SubagentStop`
    // hooks are NOT double-fired — the chokepoint already covers those.
    //
    // To carry a faithful run status we proxy `out_tx`: forward every
    // `SubagentEvent` to the real channel while remembering the terminal one, so
    // the status maps to claude's outcome (`completed` / `failed` — `Killed` does
    // not fire SubagentStop, matching claude where an aborted child's loop does
    // not reach `stopHooks`). When there are NO frontmatter hooks (the common
    // case) we skip the proxy entirely and pass `out_tx` straight through, so the
    // hot path is byte-identical to legacy.
    let agent_scoped_stop = frontmatter_cleanup.as_ref().map(|(he, agent_id)| {
        // FIX 2: the agent-scoped SubagentStop carries `agent_transcript_path`
        // (claude-code `getAgentTranscriptPath(subagentId)`, coreSchemas.ts:556 /
        // utils/hooks.ts:3676). TS builds it as
        // `…/subagents[/subdir]/agent-${agentId}.jsonl`; LingXi mirrors the
        // `agent-<id>.jsonl` leaf under this child's `transcript_subdir`.
        // FIX C: the production spawn path (handle.rs) now seeds `transcript_subdir`
        // to the REAL session-scoped dir
        // `<lingxi_home>/projects/<sanitize(cwd)>/<session>/subagents` (threaded
        // from the composition root via `with_hook_context`), so this is the true
        // `getAgentTranscriptPath` location — not the former `/tmp` placeholder.
        // Tests / minimal builds that wire no subagents dir keep the `/tmp` default.
        let agent_transcript_path = ctx
            .transcript_subdir
            .join(format!("agent-{agent_id}.jsonl"));
        (
            he.clone(),
            *agent_id,
            ctx.agent_definition.agent_type.clone(),
            ctx.hook_session_id,
            ctx.hook_cwd.clone(),
            agent_transcript_path,
        )
    });

    let mut live_hook_transcript = hooks::PromptHookTranscript::default();
    let terminal_status = if agent_scoped_stop.is_some() {
        // Proxy: forward events, capture the terminal disposition.
        let (proxy_tx, mut proxy_rx) = mpsc::channel::<SubagentEvent>(16);
        let forwarder = {
            let real = out_tx.clone();
            tokio::spawn(async move {
                let mut terminal: Option<(&'static str, Option<String>)> = None;
                while let Some(ev) = proxy_rx.recv().await {
                    terminal = match &ev {
                        SubagentEvent::Completed { result, .. } => {
                            Some(("completed", completed_result_text(result)))
                        }
                        SubagentEvent::Failed { .. } => Some(("failed", None)),
                        // Killed does not fire SubagentStop (claude: an aborted
                        // child throws before reaching its stop hooks).
                        SubagentEvent::Killed { .. } => None,
                        // Non-terminal: keep whatever terminal we last saw.
                        _ => terminal,
                    };
                    // Best-effort forward; a closed receiver drops the rest.
                    if real.send(ev).await.is_err() {
                        break;
                    }
                }
                terminal
            })
        };
        // Run the body against the proxy, then drop our proxy sender so the
        // forwarder's `recv()` loop ends and we can read the captured status.
        if ctx.api_client.is_some() {
            run_subagent_loop(ctx, event_rx, proxy_tx, &mut live_hook_transcript).await;
        } else {
            run_subagent_stub(ctx, event_rx, proxy_tx).await;
        }
        forwarder.await.unwrap_or(None)
    } else {
        // No frontmatter hooks: straight passthrough, no proxy overhead.
        if ctx.api_client.is_some() {
            run_subagent_loop(ctx, event_rx, out_tx, &mut live_hook_transcript).await;
        } else {
            run_subagent_stub(ctx, event_rx, out_tx).await;
        }
        None
    };

    // Fire the agent-scoped SubagentStop BEFORE clearing the frontmatter hooks
    // (otherwise the retargeted Stop→SubagentStop hooks are already gone). Only
    // when the child reached a terminal that fires SubagentStop in claude
    // (`completed` / `failed`).
    if let (
        Some((he, agent_id, agent_type, session_id, cwd, agent_transcript_path)),
        Some((status, last_assistant_message)),
    ) = (agent_scoped_stop, terminal_status)
    {
        let stop_ctx = hooks::registry::HookContext {
            prompt_transcript: Some(live_hook_transcript),
            session_id,
            agent_id: Some(agent_id),
            cwd,
            agent_type: Some(agent_type.clone()),
            last_assistant_message,
            // FIX 2: SubagentStop carries the agent's own transcript path
            // (claude-code `agent_transcript_path`). See the tuple build above.
            agent_transcript_path: Some(agent_transcript_path),
            ..Default::default()
        };
        he.execute_agent_scoped(
            hooks::events::HookEvent::SubagentStop {
                agent_id,
                status: status.to_string(),
                // claude keys SubagentStop matchers on the subagent's type.
                agent_type,
            },
            stop_ctx,
            agent_id,
        )
        .await;
    }

    if let Some((he, agent_id)) = frontmatter_cleanup {
        he.clear_agent_hooks(agent_id).await;
    }
    if let Some(source) = diagnostics_cleanup {
        source.close().await;
    }
}

/// Resolve the wire model string from the agent definition.
///
/// This forwards the definition's model string verbatim. For the production
/// spawn path the model is ALREADY resolved to a concrete wire id at spawn time
/// by [`crate::model_resolution::resolve_agent_model`] (`Inherit` → parent /
/// main-loop model, bare family alias → concrete `claude-*` id), so the
/// definition here carries an `Explicit(...)` id and `resolve_model` simply
/// forwards it. Only when the spawner has no `default_model` wired (legacy /
/// tests) does this forward a raw `"inherit"` / bare alias — which then resolves
/// solely via any configured `routing.aliases`.
/// Validate a captured `StructuredOutput` input against the workflow
/// `agent({schema})` JSON Schema (claude-code's Ajv `validateSchema`/`compile`
/// inside the StructuredOutput tool `call`). Returns a concise leaf-error string
/// on mismatch, `Ok(())` on a valid input.
///
/// BEHAVIORAL parity only: PASS/FAIL matches claude-code (full JSON-Schema
/// validation), but the error-detail bytes differ from Ajv's
/// `${instancePath}: ${message}` form (boon's native messages are unportable —
/// the same intentional divergence as `orchestrator::schema_validation`). The
/// byte-exact part is the `Output does not match required schema: ` WRAPPER the
/// caller prepends. A schema that fails to COMPILE is treated as PASS (a LingXi
/// schema bug must not block the model), matching the binary's stance.
fn validate_structured_output(
    schema_json: Option<&str>,
    input: &serde_json::Value,
) -> Result<(), String> {
    let Some(schema_str) = schema_json else {
        return Ok(());
    };
    let schema: serde_json::Value = match serde_json::from_str(schema_str) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    const URL: &str = "mem://structured-output-schema";
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    if compiler.add_resource(URL, schema).is_err() {
        return Ok(());
    }
    let sch = match compiler.compile(URL, &mut schemas) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    match schemas.validate(input, sch) {
        Ok(()) => Ok(()),
        Err(err) => {
            let mut out = Vec::new();
            flatten_schema_error(&err, &mut out);
            Err(out.join(", "))
        }
    }
}

/// Flatten a boon validation error into concise `at '<loc>': <kind>` leaves
/// (mirrors `orchestrator::schema_validation::flatten`).
fn flatten_schema_error(err: &boon::ValidationError, out: &mut Vec<String>) {
    if err.causes.is_empty() {
        let loc = err.instance_location.to_string();
        let loc = if loc.is_empty() {
            "(root)".to_string()
        } else {
            loc
        };
        out.push(format!("at '{loc}': {}", err.kind));
    } else {
        for cause in &err.causes {
            flatten_schema_error(cause, out);
        }
    }
}

/// The workflow `agent({schema})` StructuredOutput retry cap — claude-code
/// `Fe.MAX_STRUCTURED_OUTPUT_RETRIES ?? OBp`, where `OBp = 5`. The env override
/// matches the binary's `parseInt(process.env.MAX_STRUCTURED_OUTPUT_RETRIES||"5")`.
fn structured_output_retry_cap() -> u32 {
    std::env::var("MAX_STRUCTURED_OUTPUT_RETRIES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(5)
}

pub(crate) fn resolve_model(ctx: &SubagentContext) -> String {
    match &ctx.agent_definition.model {
        crate::definition::AgentModel::Inherit => "inherit".to_string(),
        crate::definition::AgentModel::Alias(n) | crate::definition::AgentModel::Explicit(n) => {
            n.clone()
        }
    }
}

/// Extract the text blocks the final agent response surfaces, with claude's
/// backward-scan fallback.
///
/// Port of claude-code `finalizeAgentTool` (agentToolUtils.ts:304-317): take the
/// text blocks from the LAST assistant message; if it carried none (the loop
/// exited mid-turn on a pure `tool_use` turn), fall back to the most recent
/// assistant message in `history` that DOES have text blocks. Returns the raw
/// text strings (one per surviving text block) in source order — the caller maps
/// them into claude's `content: [{type:'text', text}]` array.
fn final_text_blocks(
    history: &[lingxi_core::types::ConversationMessage],
    final_assistant_blocks: &[lingxi_core::types::ContentBlock],
) -> Vec<String> {
    let texts_of = |blocks: &[lingxi_core::types::ContentBlock]| -> Vec<String> {
        blocks
            .iter()
            .filter_map(|b| match b {
                lingxi_core::types::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<String>>()
    };
    // 1. Text from the final assistant message.
    let primary = texts_of(final_assistant_blocks);
    if !primary.is_empty() {
        return primary;
    }
    // 2. Backward scan: most recent assistant message WITH text.
    for msg in history.iter().rev() {
        if let lingxi_core::types::ConversationMessage::Assistant { content, .. } = msg {
            let t = texts_of(content);
            if !t.is_empty() {
                return t;
            }
        }
    }
    Vec::new()
}

/// Build the terminal `Completed.result` JSON for a clean stop.
///
/// Carries claude's `content` array (`[{type:'text', text}]`, agentToolUtils.ts
/// `finalizeAgentTool` return) computed via the backward-scan
/// [`final_text_blocks`], plus the legacy `text`/`stop_reason` keys existing
/// consumers (and the runner's own tests) read. `AgentTool` reads `content` to
/// build claude's structured result + model-facing trailer; the joined `text`
/// stays for back-compat (`tasks::handlers::local_agent` / `dream`).
fn build_completed_result(
    history: &[lingxi_core::types::ConversationMessage],
    final_assistant_blocks: &[lingxi_core::types::ContentBlock],
    stop_reason: Option<&str>,
    serving_model: &str,
) -> serde_json::Value {
    // `ICe`: retracted messages come out before the answer is picked, so a
    // superseded hop's output cannot become the report.
    let live = drop_retracted(history);
    let mut blocks = final_text_blocks(&live, final_assistant_blocks);
    // The `⚠ {notice}` harness note. Upstream unshifts it in `iht` AFTER the
    // turn-limit note; here the turn-limit note is inserted at index 0 by the
    // agent tool's finalizer, so prepending here lands the pair in upstream's
    // order — turn-limit, then this, then the report.
    if let Some(notice) = local_refusal_notice(&live, serving_model) {
        blocks.insert(0, format!("\u{26A0} {notice}\n"));
    }
    let content: Vec<serde_json::Value> = blocks
        .iter()
        .map(|t| serde_json::json!({ "type": "text", "text": t }))
        .collect();
    serde_json::json!({
        "content": content,
        "text": blocks.join("\n"),
        "stop_reason": stop_reason,
    })
}

/// CC 2.1.207 subagent api-error classification (`CTy` / `zho`
/// `AgentApiErrorTerminationError`). Maps an [`llm_runtime::LlmError`] surfaced by
/// a mid-stream round-trip to `(errorKind, api_error_text)` when it is an API
/// TERMINATION whose kind is in `CTy = {rate_limit, overloaded, server_error}` —
/// the only kinds CC recovers as `api_error_partial` (every other kind rethrows
/// → `Failed`). The `api_error_text` is the model-visible `API Error: …` string
/// the query-loop finalize embeds into `zho.message`
/// (`yield tu({content:…,error:"server_error"})`); the finalize always tags the
/// synthesized message `server_error`, so the connection-close / stall variants
/// still qualify. The exact request-level rate-limit copy is NOT reproduced
/// here (it lives in the orchestrator's `errors.ts` port, which the agent crate
/// cannot depend on); a mid-stream 429 is surfaced with the server-error text,
/// which is what the finalize path yields.
fn classify_api_termination(e: &llm_runtime::LlmError) -> Option<(&'static str, &'static str)> {
    use llm_runtime::LlmError;
    match e {
        LlmError::Overloaded { .. } => Some((
            "overloaded",
            "API Error: Server error mid-response. The response above may be incomplete.",
        )),
        LlmError::RateLimited { .. } => Some((
            "rate_limit",
            "API Error: Server error mid-response. The response above may be incomplete.",
        )),
        LlmError::ProviderInternal => Some((
            "server_error",
            "API Error: Server error mid-response. The response above may be incomplete.",
        )),
        LlmError::Transport { .. } => Some((
            "server_error",
            "API Error: Connection lost mid-response. The response above may be incomplete.",
        )),
        // A stall is NOT in `CTy`. Claude Code 2.1.238 classifies it through
        // `xtt`:
        //
        //     e.message.startsWith("Stream idle timeout") ||
        //     e.name === "StreamIdleTimeoutError"   ->  "api_timeout"
        //
        // and `api_timeout` is absent from
        // `CTy = new Set(["rate_limit","overloaded","server_error"])`, so the
        // oracle RETHROWS a stalled stream instead of recovering it as
        // `api_error_partial`. The suspend variant says the same thing in its
        // own message -- "aborting to retry on a fresh connection" -- which is
        // a retry on a fresh connection, not a partial answer.
        //
        // The difference is one bit and it decides whether a caller learns
        // anything. Recovered-as-partial hands a workflow stage a `completed`
        // result whose content is "I was cut off and did nothing"; the stage
        // records it as output and moves on. Observed: three consecutive
        // generate attempts stalled, each "succeeded" with an empty recovery,
        // and the failure only surfaced at the verify stage as "the app is
        // still the unmodified template".
        //
        // Every OTHER `StreamInterrupted` -- a protocol violation, a stream
        // that ended before `message_stop` -- keeps the recovering
        // classification it was ported with. There is no oracle evidence to
        // move those, and moving them on the strength of this one would be
        // guessing.
        LlmError::StreamInterrupted { message }
            if message
                .starts_with(llm_runtime::model::stream_watchdog::STREAM_IDLE_TIMEOUT_PREFIX)
                || message
                    .starts_with(llm_runtime::model::stream_watchdog::STREAM_SUSPENDED_PREFIX) =>
        {
            None
        }
        LlmError::StreamInterrupted { .. } => Some((
            "server_error",
            "API Error: The response stopped arriving. The response above may be incomplete.",
        )),
        // Auth / permission / invalid-request / quota / context / TLS / cost /
        // unsupported-capability / model-unavailable are terminal — CC rethrows
        // (errorKind not in CTy), so they surface as `Failed`.
        _ => None,
    }
}

/// Build the CC 2.1.207 subagent `api_error_partial` result: the normal
/// completed result (final text blocks via the backward scan) with `cutoff_note`
/// prepended as the FIRST text block — claude's sync-agent recovery
/// (`On.content=[{type:"text",text:Dn},...On.content]`, status `"completed"`).
fn build_recovered_result(
    history: &[lingxi_core::types::ConversationMessage],
    final_assistant_blocks: &[lingxi_core::types::ContentBlock],
    cutoff_note: &str,
) -> serde_json::Value {
    let mut blocks = final_text_blocks(history, final_assistant_blocks);
    blocks.insert(0, cutoff_note.to_string());
    let content: Vec<serde_json::Value> = blocks
        .iter()
        .map(|t| serde_json::json!({ "type": "text", "text": t }))
        .collect();
    serde_json::json!({
        "content": content,
        "text": blocks.join("\n"),
        "stop_reason": serde_json::Value::Null,
    })
}

/// Assemble the CC 2.1.207 `cutoffNote` (`wTy`): the
/// `AgentApiErrorTerminationError` message (`Agent terminated early due to an
/// API error: {api_error_text}`) followed by the byte-locked incomplete-output
/// notice, joined by a blank line (two newlines) — the binary builds
/// `cutoffNote:` + "${e.message}\n\n" + "Everything below…".
fn build_cutoff_note(api_error_text: &str) -> String {
    format!(
        "Agent terminated early due to an API error: {api_error_text}\n\n\
Everything below is PARTIAL output recovered from the agent before it was cut off. The agent did NOT finish its task \u{2014} treat these results as incomplete."
    )
}

/// Byte-locked `formatSkillLoadingMetadata(skillName)` port
/// (claude `processSlashCommand.tsx:786`): the leading text block of a preloaded
/// skill's meta user message. claude ignores the `progressMessage` arg
/// (`_progressMessage` is unused), so this renders only the (resolved) name:
/// `<command-message>{name}</command-message>\n<command-name>{name}</command-name>\n<skill-format>true</skill-format>`.
fn format_skill_loading_metadata(skill_name: &str) -> String {
    format!(
        "<command-message>{skill_name}</command-message>\n\
<command-name>{skill_name}</command-name>\n\
<skill-format>true</skill-format>"
    )
}

/// Build the G4 (SubagentStart additionalContext) + G5 (skills) messages claude
/// `runAgent` prepends to a child's INITIAL messages before the query loop, in
/// claude's order: additionalContext (runAgent.ts:530-555) → skills
/// (runAgent.ts:577-646). (Frontmatter-hook registration — runAgent.ts:557-575,
/// ordered between them — is handled at the [`run_subagent`] dispatcher so the
/// clear is guaranteed; its registration is side-effecting, not message-producing,
/// so its position relative to these two message-producing steps is unobservable
/// in the child history.)
///
/// Returns the extra [`ConversationMessage`]s to append after the prompt seed.
/// A `None` `hook_executor` / `skill_loader` (tests / minimal builds) makes the
/// respective step a strict no-op, so the child history stays byte-identical to
/// legacy.
async fn build_preload_messages(
    ctx: &SubagentContext,
) -> Result<Vec<lingxi_core::types::ConversationMessage>, String> {
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

    let agent_type = ctx.agent_definition.agent_type.clone();
    let mut out: Vec<ConversationMessage> = Vec::new();

    // --- G4: SubagentStart hooks → additionalContext injection --------------
    // claude fires `executeSubagentStartHooks(agentId, agentType, signal)`,
    // collects every hook's `additionalContexts` into ONE `string[]`, and pushes
    // a SINGLE `hook_additional_context` user message into `initialMessages`
    // (runAgent.ts:530-555). That attachment renders (messages.ts:4117-4128 via
    // `wrapInSystemReminder`) as ONE `<system-reminder>` message:
    //   `<system-reminder>\nSubagentStart hook additional context: ` +
    //   contexts.join("\n") + `\n</system-reminder>`
    // An empty collection produces NO message (messages.ts:4118 early return).
    // We match those bytes exactly: one message, the `SubagentStart hook
    // additional context: ` prefix, the `\n`-join of all contexts.
    // G008: a Fusion panel is not an ordinary `Agent` tool spawn — it is one of
    // N concurrent provider round-trips the orchestrator's own subagent-hook
    // chokepoint (turn_loop.rs) already accounts for as a SINGLE `fusion` node
    // (see `fusion_tool_result`'s `subagentHooksFired` marker below). Firing a
    // real per-panel `SubagentStart` here — with no matching `SubagentStop`,
    // since a panel definition carries no frontmatter `Stop` hook — would leave
    // N starts and zero stops for every Fusion run. Skip it for this
    // `agent_type` on EVERY entrypoint (Agent tool, `/fusion`, workflow), so a
    // Fusion run's hook activity is exactly the chokepoint's one pair.
    if let Some(hooks) = &ctx.hook_executor {
        if agent_type != lingxi_core::host::FUSION_PANEL_TYPE {
            let hook_ctx = hooks::registry::HookContext {
                session_id: ctx.hook_session_id,
                agent_id: Some(ctx.agent_id),
                cwd: ctx.hook_cwd.clone(),
                agent_type: Some(agent_type.clone()),
                ..Default::default()
            };
            let agg = hooks
                .execute(
                    hooks::events::HookEvent::SubagentStart {
                        agent_id: ctx.agent_id,
                        agent_type: agent_type.clone(),
                        parent_agent_id: ctx.parent_agent_id,
                    },
                    hook_ctx,
                )
                .await;
            if !agg.additional_contexts.is_empty() {
                let joined = agg.additional_contexts.join("\n");
                out.push(ConversationMessage::user(
                    MessageId::new(),
                    format!(
                        "<system-reminder>\nSubagentStart hook additional context: {joined}\n</system-reminder>"
                    ),
                ));
            }
        }
    }

    // --- G5: skills preload -------------------------------------------------
    // claude resolves+loads each frontmatter skill and pushes a `isMeta` user
    // message whose first block is `formatSkillLoadingMetadata(skillName,
    // skill.progressMessage)` followed by the loaded content blocks
    // (runAgent.ts:577-646). A missing / non-prompt skill logs claude's exact
    // warn and is skipped.
    if let Some(loader) = &ctx.skill_loader {
        for skill_name in &ctx.agent_definition.skills {
            match loader
                .resolve_and_load(skill_name, &agent_type, ctx.cwd.as_deref())
                .await?
            {
                None => {
                    // claude runAgent.ts:600 — exact warn string.
                    tracing::warn!(
                        "[Agent: {agent_type}] Warning: Skill '{skill_name}' specified in frontmatter was not found"
                    );
                }
                Some(load) => {
                    tracing::debug!("[Agent: {agent_type}] Preloaded skill '{skill_name}'");
                    // Leading metadata text block + the loaded content blocks
                    // (claude `createUserMessage({ content: [metadata, ...content],
                    // isMeta: true })`). LingXi's `ConversationMessage::User` has
                    // marks the message meta so last-user-query selection and UI
                    // rendering never mistake a preload for user intent.
                    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(1 + load.content.len());
                    blocks.push(ContentBlock::Text {
                        text: format_skill_loading_metadata(&load.display_name),
                    });
                    blocks.extend(load.content);
                    out.push(ConversationMessage::User {
                        id: MessageId::new(),
                        content: blocks,
                        is_meta: true,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
                    });
                }
            }
        }
    }

    Ok(out)
}

/// Translate llm-runtime content blocks into protocol content blocks.
///
/// Mirrors the orchestrator's `translate_response_blocks`: `Text` /
/// `ToolCall` / `Reasoning` map through; server-side and other variants
/// are dropped.
fn translate_response_blocks(
    content: &[llm_runtime::ContentBlock],
) -> Vec<lingxi_core::types::ContentBlock> {
    content
        .iter()
        .filter_map(|b| match b {
            llm_runtime::ContentBlock::ProviderContent { protocol, value } => Some(lingxi_core::types::ContentBlock::ProviderContent { protocol: protocol.clone(), value: value.clone() }),

            llm_runtime::ContentBlock::Text { text, .. }
            | llm_runtime::ContentBlock::TextJsUtf16 { text, .. } => {
                Some(lingxi_core::types::ContentBlock::Text { text: text.clone() })
            }
            llm_runtime::ContentBlock::ToolCall { id, name, input } => {
                // (cc 2.1.218 `jYd`) Same literal-`\uXXXX` repair the orchestrator
                // applies — a subagent's tool inputs must be normalized too.
                let (input, _stats) =
                    llm_runtime::unicode_repair::repair_tool_input(name, input);
                Some(lingxi_core::types::ContentBlock::ToolUse {
                    // The provider-issued id IS the canonical ToolUseId (byte
                    // parity with claude-code); the provider_id sidecar stays None.
                    id: lingxi_core::types::ToolUseId::from(id.clone()),
                    name: name.clone(),
                    input,
                    provider_id: None,
                })
            }
            llm_runtime::ContentBlock::Reasoning { text, signature } => {
                Some(lingxi_core::types::ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                })
            }
            // Low-frequency server-side blocks: PRESERVED verbatim for resume/replay
            // byte parity (matches orchestrator::turn_loop::translate_response_blocks).
            llm_runtime::ContentBlock::RedactedThinking { data } => {
                Some(lingxi_core::types::ContentBlock::RedactedThinking { data: data.clone() })
            }
            llm_runtime::ContentBlock::ServerToolUse { id, name, input } => {
                Some(lingxi_core::types::ContentBlock::ServerToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                })
            }
            llm_runtime::ContentBlock::ConnectorText {
                connector_text,
                signature,
            } => Some(lingxi_core::types::ContentBlock::ConnectorText {
                connector_text: connector_text.clone(),
                signature: signature.clone(),
            }),
            llm_runtime::ContentBlock::AdvisorToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(lingxi_core::types::ContentBlock::AdvisorToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            // Input-only / non-output variants remain dropped on the response path.
            llm_runtime::ContentBlock::Image { .. }
            | llm_runtime::ContentBlock::ImageUrl { .. }
            | llm_runtime::ContentBlock::Document { .. }
            | llm_runtime::ContentBlock::ToolResult { .. }
            // cache_edits is a request-only directive — never in a response.
            | llm_runtime::ContentBlock::CacheEdits { .. } => None,
        })
        .collect()
}

/// Emit `msg` as a [`SubagentEvent::Message`] on `out_tx`.
async fn emit_message(
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    msg: &lingxi_core::types::ConversationMessage,
) {
    let _ = out_tx
        .send(SubagentEvent::Message {
            agent_id,
            message: serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
        })
        .await;
}

/// Publish a rest transition without ending the foreground event pump. A
/// Completed event would deallocate its runner, which still owns background
/// notifications. This marker stays out of model history and the transcript.
async fn emit_parked(out_tx: &mpsc::Sender<SubagentEvent>, agent_id: AgentId) {
    emit_message(
        out_tx,
        agent_id,
        &lingxi_core::types::ConversationMessage::System {
            id: lingxi_core::types::MessageId::new(),
            content: "idle".to_string(),
            subtype: Some("agent_idle".to_string()),
            compact_metadata: None,
            refusal_fallback: None,
        },
    )
    .await;
}

async fn emit_progress(
    out_tx: &mpsc::Sender<SubagentEvent>,
    agent_id: AgentId,
    tool_use_count: u64,
    token_count: u64,
) {
    let _ = out_tx
        .send(SubagentEvent::Progress {
            agent_id,
            tool_use_count: u32::try_from(tool_use_count).unwrap_or(u32::MAX),
            token_count,
        })
        .await;
}

async fn flush_transcript(
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &[lingxi_core::types::ConversationMessage],
    written: &mut usize,
) {
    let Some(writer) = transcript else {
        return;
    };
    // The watermark is normally <= history.len(). Be defensive around a
    // malformed restored history so transcript persistence can never panic and
    // mask the actual agent terminal event.
    let start = (*written).min(history.len());
    for message in &history[start..] {
        if writer.record(message).await.is_err() {
            break;
        }
        *written += 1;
    }
}

fn publish_prompt_hook_transcript(
    ctx: &SubagentContext,
    history: &[lingxi_core::types::ConversationMessage],
    usage: &llm_runtime::ExecutionUsage,
) {
    if let Some(executor) = &ctx.hook_executor {
        executor.publish_agent_prompt_transcript(
            ctx.hook_session_id,
            ctx.agent_id,
            hooks::PromptHookTranscript {
                messages: history.to_vec(),
                last_usage_tokens: usize::try_from(
                    usage
                        .counts()
                        .input_tokens
                        .saturating_add(
                            usage
                                .counts()
                                .output_tokens
                                .saturating_sub(usage.counts().reasoning_tokens),
                        )
                        .saturating_add(usage.counts().cache_read_tokens)
                        .saturating_add(usage.counts().cache_write_tokens),
                )
                .unwrap_or(usize::MAX),
                ..Default::default()
            },
        );
    }
}

async fn emit_failed(
    out_tx: &mpsc::Sender<SubagentEvent>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &[lingxi_core::types::ConversationMessage],
    written: &mut usize,
    agent_id: AgentId,
    error: String,
    cumulative_usage: llm_runtime::ExecutionUsage,
) {
    flush_transcript(transcript, history, written).await;
    if let Some(writer) = transcript {
        let _ = writer.record_terminal("failed", Some(&error)).await;
    }
    let _ = out_tx
        .send(SubagentEvent::Failed {
            agent_id,
            error,
            cumulative_usage,
        })
        .await;
}

async fn emit_killed(
    out_tx: &mpsc::Sender<SubagentEvent>,
    transcript: Option<&crate::transcript::AgentTranscriptWriter>,
    history: &[lingxi_core::types::ConversationMessage],
    written: &mut usize,
    agent_id: AgentId,
) {
    flush_transcript(transcript, history, written).await;
    if let Some(writer) = transcript {
        let _ = writer.record_terminal("cancelled", None).await;
    }
    let _ = out_tx.send(SubagentEvent::Killed { agent_id }).await;
}

/// Returns the companion note suffix (`yyo` in the binary, `nke` set) appended
/// to the allow-list-refusal error when a subagent tries to call a tool from
/// the "external companion" set that has been stripped from its pool.
///
/// Binary anchor: `function yyo(e,t,n,r)` at 203453056 in v2.1.186.
/// Trigger branch: `if (n && o && nke.has(o.name)) return …` where `n` = inside
/// a subagent, `o` = the resolved tool, `nke = HDd("external")`.
///
/// `HDd("external")` at 198067186:
/// ```text
/// new Set([lW, iO, qz, Qp, brt, tke, ...(e!=="ant"?[SI]:[]), Mh])
/// ```
/// Resolved (confirmed from binary):
/// - `lW`  = `"TaskOutput"`
/// - `iO`  = `"ExitPlanMode"`
/// - `qz`  = `"EnterPlanMode"`
/// - `Qp`  = `"AskUserQuestion"`
/// - `brt` = `"ConnectGitHub"`
/// - `tke` = `"WaitForMcpServers"`
/// - `SI`  = `"Workflow"` (non-ant only; `USER_TYPE !== "ant"`)
/// - `Mh`  = `"ScheduleWakeup"`
///
/// All subagent runners are inside a subagent by definition (`n` = true).
/// `is_ant` gates `Workflow` exactly like `HDd`'s ant-gate.
///
/// Returns `Some(note_suffix)` when the tool is in the `nke` set, `None`
/// otherwise. The note starts with `. ` to append to an in-progress sentence.
fn companion_note_for_disallowed_tool(tool_name: &str, is_ant: bool) -> Option<String> {
    // The static nke set elements always present for both ant and non-ant:
    const NKE_BASE: &[&str] = &[
        "TaskOutput",
        "ExitPlanMode",
        "EnterPlanMode",
        "AskUserQuestion",
        "ConnectGitHub",
        "WaitForMcpServers",
        "ScheduleWakeup",
    ];
    // "Workflow" is added for non-ant (HDd: `...(e!=="ant"?[SI]:[])`).
    let in_nke = NKE_BASE.contains(&tool_name) || (!is_ant && tool_name == "Workflow");
    if in_nke {
        // Binary §7 verbatim (leading `. ` — appended to an in-progress sentence):
        // `. ${toolName} is not available inside subagents. Complete the task with
        //  the tools provided and return findings to the orchestrator.`
        Some(format!(
            ". {tool_name} is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        ))
    } else {
        None
    }
}

/// Real multi-turn agentic loop.
///
/// Imperative — mirrors `orchestrator::turn_loop::execute_one_turn`: call the
/// model, append the assistant turn, dispatch `tool_use` blocks through the
/// inherited [`lingxi_core::host::ToolInvoker`], feed results back as a user message,
/// and repeat. Each model round-trip goes over the streaming seam
/// ([`crate::api::SubagentApiClient::messages_create_stream`] drained through
/// `crate::accumulator::accumulate_stream`) and races a `UserExit` /
/// `UserInterrupt` on `event_rx` via [`tokio::select!`]; a termination event
/// aborts the loop and surfaces [`SubagentEvent::Killed`].
#[allow(
    clippy::too_many_lines,
    reason = "imperative multi-turn agentic loop — splitting the turn body hurts readability"
)]
async fn run_subagent_loop(
    mut ctx: SubagentContext,
    mut event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
    live_hook_transcript: &mut hooks::PromptHookTranscript,
) {
    use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

    let agent_id = ctx.agent_id;
    let api_client = ctx
        .api_client
        .clone()
        .expect("run_subagent_loop requires an api_client");
    // Inherited budget enforcer (cloned Option<Arc> — cheap refcount bump).
    // `Some` consults the parent's cumulative cost once per turn; `None`
    // disables enforcement (legacy/test contexts).
    let budget = ctx.budget.clone();
    let mut model = resolve_model(&ctx);
    // Per-run refusal cascade. claude-code's subagents share the main thread's
    // because they share its query generator; here the loops are separate, so
    // each run walks its own chain (handed down on the context).
    let mut refusal_cascade = lingxi_core::host::refusal_driver::RefusalCascadeState::default();
    let system: Option<String> = ctx
        .rendered_system_prompt
        .as_ref()
        .map(std::string::ToString::to_string);
    // Wire tool definitions advertised to the model on every round-trip (empty
    // when the spawner wired none). Cloned per round-trip below.
    //
    // Structured output (claude-code workflow `agent({schema})`): when a schema
    // was requested, inject a synthetic `StructuredOutput` tool whose
    // `input_schema` IS the schema; its tool input is captured below as the
    // run's result. `force_structured_tool` drives validation/capture/retry-cap
    // (unchanged) — see `force_tool_choice_for_api` below for what actually
    // gets sent on the wire as `tool_choice`.
    let mut tool_schemas = ctx.tool_schemas.clone();
    let force_structured_tool: Option<&'static str> = if let Some(schema_str) = &ctx.schema {
        let input_schema: serde_json::Value = serde_json::from_str(schema_str)
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
        // The normal registry exposes a permissive StructuredOutput tool. A
        // workflow schema run must replace it with this invocation's schema,
        // not append a second declaration with the same provider-facing name.
        tool_schemas.retain(|tool| {
            tool.get("name").and_then(serde_json::Value::as_str) != Some("StructuredOutput")
        });
        tool_schemas.push(serde_json::json!({
            "name": "StructuredOutput",
            "description":
                "Return the final result as a single structured object matching the required schema.",
            "input_schema": input_schema,
        }));
        Some("StructuredOutput")
    } else {
        None
    };
    // P0-1 (2026-09-02): forcing `tool_choice` to StructuredOutput on EVERY
    // round-trip made a schema subagent unable to call any other advertised
    // tool for its whole run — verified against the claude-code 2.1.258
    // oracle binary (`~/.local/share/claude/versions/2.1.258`), whose shared
    // subagent turn loop hardcodes `toolChoice: void 0` on its single
    // `callModel` call site (offset ~164713374) regardless of
    // `requiresStructuredOutput`, and instead enforces StructuredOutput
    // purely by injecting an in-conversation nudge message once per turn the
    // model ends without a valid captured output (offset ~164629006, sentinel
    // `"[structured-output-enforce]"` at 161464131) — never by restricting
    // the tool_choice wire param. LingXi mirrors that: only pin `tool_choice`
    // when StructuredOutput is the ONLY tool being advertised (there is
    // nothing else the model could usefully call, so forcing changes
    // nothing observable and just removes one avoidable no-tool-called
    // round-trip); whenever other tools are present the model chooses
    // freely every round, and the existing SubagentStop nudge path below
    // (`:1949` as of this change) is the sole enforcement mechanism, exactly
    // as the oracle does. See `<scratchpad>/WP3-oracle.txt` for the full
    // extracted evidence.
    //
    // 🚨 CORRECTING THE RECORD: commit `1c9cbd695`'s message says `tool_choice`
    // is "forced after the nudge". That is WRONG and was never what shipped.
    // `force_tool_choice_for_api` is computed ONCE, HERE, before the turn loop
    // is entered, from the advertised-tool count alone; the nudge path below
    // deliberately does NOT re-arm it, and there is no other assignment to this
    // binding anywhere in the loop (it is a `let`, not a `let mut`). Under
    // `StructuredOutputMode::Forced` (the byte-parity default this comment was
    // originally written about), a run that advertises tools besides
    // StructuredOutput sends `tool_choice = None` on every round-trip, before
    // AND after the nudge — this binding alone is the whole story there.
    //
    // Round-3 review items 3/7/9/20 fix: under `StructuredOutputMode::WhenDone`
    // (Fusion panels), this binding is NOT the whole story any more — the wire
    // call below additionally pins on the run's designated last-chance turn
    // (final turn, or two consecutive idle turns) even when `tool_schemas.len()
    // != 1`, because that turn is the one place WhenDone's "forced only on the
    // last turn" contract (`platform-api/src/subagent_spawn.rs`) cannot be
    // honored by this binding's single-tool gate alone — see the wire call's
    // own comment below for the mode-aware condition.
    let force_tool_choice_for_api: Option<&'static str> =
        if force_structured_tool.is_some() && tool_schemas.len() == 1 {
            force_structured_tool
        } else {
            None
        };
    // Captured when the model calls the synthetic `StructuredOutput` tool with an
    // input that VALIDATES against the schema — that input becomes the run's
    // result, and the loop terminates.
    let mut structured_result: Option<serde_json::Value> = None;
    // claude-code `agent({schema})` run-scoped counters: `kn` (failed
    // StructuredOutput validations) and `ft` (in-conversation nudges injected
    // when the model ends a turn without calling StructuredOutput). `Yr` is the
    // retry cap (`MAX_STRUCTURED_OUTPUT_RETRIES ?? 5`).
    let mut structured_failed_count: u32 = 0;
    let mut structured_nudge_count: u32 = 0;
    let mut structured_truncation_retries: u32 = 0;
    // Separate run-scoped cap: parsing failures never reach schema validation.
    let mut structured_parse_retries: u32 = 0;
    let structured_parse_retry_cap = ctx.structured_output_parse_retries.min(2);
    let structured_retry_cap = structured_output_retry_cap();
    // `StructuredOutputMode::WhenDone` (Fusion panels — WP2a item 1): counts
    // CONSECUTIVE turns that produced no tool_use block at all (not even a
    // `StructuredOutput` call). Reset to 0 the moment any tool is called.
    // Reaching 2 forces `StructuredOutput` on the very next turn, same as
    // being on the run's last turn — see `force_this_turn` below.
    let mut whendone_idle_turns: u32 = 0;
    let force_every_turn = matches!(
        ctx.structured_output_mode,
        lingxi_core::host::subagent_spawn::StructuredOutputMode::Forced
    );
    // Per-agent tool allow-list enforced at dispatch (see below). Empty = no
    // restriction (the resolver has not filtered, e.g. `AgentToolPolicy::All`).
    // This is the dispatch-time guard the advertised set relies on: the
    // inherited `RegistryToolInvoker` itself does NOT check policy.
    let allowed_tools = ctx.allowed_tools.clone();

    // Seed history. A RESTORED agent is seeded from its persisted transcript
    // and nothing else: fork context, prompt and preload are all already inside
    // that history (they were persisted on the original run), so re-adding them
    // would duplicate context the agent has seen and re-fire `SubagentStart`
    // for a run that began in another process.
    let history = &mut live_hook_transcript.messages;
    if let Some(resumed) = &ctx.resumed_history {
        history.extend(resumed.iter().cloned());
        if let Some(scope) = llm_runtime::thinking_scope::current() {
            crate::transcript::restore_thinking_recovery(history, &scope);
        }
    } else {
        // Keep the engine-owned mobile snapshot at the fixed first-message
        // position, before the variable task/fork prompt. That preserves the
        // provider-cacheable prefix while leaving custom/fork SYSTEM bytes
        // untouched. A resumed transcript already contains this message.
        if let Some(reminder) = &ctx.mobile_runtime_environment_reminder {
            history.push(ConversationMessage::user_meta(
                MessageId::new(),
                reminder.to_string(),
            ));
        }
        if let Some(reminder) = &ctx.mobile_runtime_workspace_reminder {
            history.push(ConversationMessage::user_meta(
                MessageId::new(),
                reminder.to_string(),
            ));
        }

        // Fork-context prefix (if any) followed by the prompt.
        if let Some(fork) = &ctx.fork_context_messages {
            history.extend(fork.iter().cloned());
        }
        history.extend(ctx.prompt_messages.iter().cloned());

        // G4 + G5 (claude runAgent.ts:530-646): SubagentStart-hook
        // additionalContext injection then frontmatter skills preload, appended
        // to the child's INITIAL messages before the first turn. Strict no-op
        // (no extra messages) when neither `hook_executor` nor `skill_loader` is
        // wired, so legacy / test builds keep a byte-identical history.
        // (Frontmatter-hook registration is done at the `run_subagent`
        // dispatcher for guaranteed cleanup.)
        //
        // AG-6, verified at the 2.1.270 oracle 2026-09-14 — NOT a gap here, and
        // this line is why. 2.1.267 shipped "Fixed agent teammates and resumed
        // subagents moving SubagentStart hook context and preloaded skills out
        // of the prompt prefix on later turns, which broke prompt-cache reuse",
        // with a sibling for in-process teammates re-sending their first-turn
        // tool and skill announcements on the second turn. Both describe
        // content that belongs in the STABLE PREFIX being re-emitted into the
        // varying tail. Neither can happen here: the preload is seeded ONCE,
        // into the initial history, inside the branch a resume does not take —
        // `SubagentContext::resumed_history` REPLACES prompt + fork context +
        // preload rather than prefixing them, because all three are already in
        // the recovered history. The 2.1.265 half ("record the system prompt
        // and tool definitions once instead of re-rendering them") is
        // `SubagentContext::rendered_system_prompt`, and the resumed tool list
        // is pinned by the same SHA-256 definition snapshot AG-5 turns on.
        // Re-open this only with a measured prefix diff across two turns.
        match build_preload_messages(&ctx).await {
            Ok(messages) => history.extend(messages),
            Err(error) => {
                let _ = out_tx
                    .send(SubagentEvent::Failed {
                        agent_id,
                        error: format!("Could not preload agent skills: {error}"),
                        cumulative_usage: llm_runtime::ExecutionUsage::default(),
                    })
                    .await;
                return;
            }
        }
    }

    // Per-agent transcript. Appended by WATERMARK — everything in
    // `history[written..]` is flushed at each turn-set boundary — rather than
    // at each `history.push` site. There are five of those and a future sixth
    // would silently skip persistence; a watermark cannot miss one.
    //
    // The flush points are the turn-set boundaries because those are exactly
    // the RESUME boundaries: a persistent agent parks between turn-sets, so an
    // on-disk transcript that is complete at every park is complete at every
    // point anything could resume from.
    let transcript = ctx.transcript_fs.clone().map(|fs| {
        crate::transcript::AgentTranscriptWriter::new(
            ctx.transcript_subdir
                .join(format!("agent-{agent_id}.jsonl")),
            agent_id,
            fs,
        )
        .with_metadata(
            ctx.agent_name.clone(),
            Some(ctx.agent_definition.agent_type.clone()),
            Some(resolve_model(&ctx)),
            ctx.model_profile.clone(),
        )
        .with_correlation_id(ctx.correlation_id.clone())
    });
    if let (Some(writer), Some(scope)) =
        (transcript.as_ref(), llm_runtime::thinking_scope::current())
    {
        let writer = writer.clone();
        scope.set_recorder(std::sync::Arc::new(move |messages| {
            let writer = writer.clone();
            Box::pin(async move {
                if let Err(error) = writer.record_thinking_recovery(messages).await {
                    tracing::warn!(%error, "could not persist worker thinking recovery");
                }
            })
        }));
    }
    // Mark a child as live before its first round-trip. A persistent child may
    // later transition to `idle` without terminating; the lifecycle records
    // make that distinction observable to mobile clients tailing the file.
    if let Some(writer) = transcript.as_ref() {
        let _ = writer.record_terminal("running", None).await;
    }
    // A RESTORED run starts with its transcript already on disk, so its
    // watermark starts past the recovered messages — otherwise the first flush
    // would append the whole conversation a second time. A fresh run starts at
    // 0: its seeded prompt and preload are new and must be persisted.
    let mut transcript_written: usize = if ctx.resumed_history.is_some() {
        history.len()
    } else {
        0
    };

    // A fresh child's task is already real input before the provider answers.
    // Persist that seed now, rather than at the first turn boundary, so a
    // stalled first request still has an inspectable transcript. Publish the
    // caller-supplied prompt through the same typed message stream as later
    // turns; mobile clients can then render it immediately even when a
    // transcript load races this first append. Restored agents skip both paths
    // because their seed is already durable and replayable.
    if ctx.resumed_history.is_none() {
        flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
        for message in &ctx.prompt_messages {
            emit_message(&out_tx, agent_id, message).await;
        }
    }

    // A restored human-owned turn drains its typed inbox before the first API
    // request. The persisted-history watermark above excludes these new inputs.
    if ctx.resumed_history.is_some() && fold_task_notifications(&ctx, history).await {
        flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
    }

    let max_turns = ctx.agent_definition.max_turns;

    // Once the cancellation channel closes, no UserExit / UserInterrupt can
    // ever arrive, so we stop racing it and await the API future directly
    // (racing a perpetually-ready `recv() -> None` arm would busy-loop).
    let mut event_channel_open = true;

    // Result-level rollups carried onto the terminal `Completed` event so the
    // spawner can populate claude's `totalDurationMs` / `totalToolUseCount`
    // without re-deriving them. `run_start` spans the whole run (every turn-set
    // in persistent mode); `total_tool_use_count` accumulates `tool_uses.len()`
    // across turns. `last_usage` keeps the FINAL response usage (claude
    // `getTokenCountFromUsage` reads the LAST assistant usage, not a sum), so it
    // is overwritten — never accumulated — each turn.
    let run_start = std::time::Instant::now();
    let mut total_tool_use_count: u64 = 0;
    let mut last_usage = llm_runtime::ExecutionUsage::default();
    let mut cumulative_usage = llm_runtime::ExecutionUsage::default();
    let mut usage_complete = true;
    // claude `agentMessages.length` — assistant turns produced across the run
    // (one per round-trip) — and the FINAL turn's provider request id (claude
    // `lastAssistantMessage.requestId`), both surfaced on the terminal
    // `Completed` event so the spawner can emit `tengu_agent_tool_completed` /
    // `tengu_cache_eviction_hint`.
    let mut assistant_message_count: u64 = 0;
    let mut last_request_id: Option<String> = None;
    let mut notification_changes = ctx
        .task_registry
        .as_ref()
        .and_then(|registry| registry.subscribe_task_notifications());

    // Outer loop: one iteration per turn-set. In non-persistent mode the
    // turn-set runs exactly once (we `return` after it). In persistent mode the
    // runner parks at the bottom awaiting the next inbound `UserMessage` and
    // loops back here to run the next turn-set, retaining `history` across
    // turn-sets (matching the TS teammate's accumulated transcript). `max_turns`
    // is per-turn-set: the `_turn` counter re-zeroes each outer iteration, so
    // every injected message gets a fresh budget.
    loop {
        // Set to `true` when the inner turn loop hits a clean terminal stop (it has
        // already emitted its `Completed`). Stays `false` if the loop instead falls
        // through by exhausting `max_turns`, which needs the max-turns `Completed`.
        let mut terminated_cleanly = false;
        let mut foreground_parked = false;
        for turn_idx in 0..max_turns {
            fold_task_notifications(&ctx, history).await;
            // Per-turn budget gate. This is the achievable analog of
            // `QueryEngine.ts`'s `error_max_budget_usd` loop-terminator, built on
            // the same frozen seam `AgentTool`'s pre-spawn gate uses
            // (tools/agent/src/agent.rs:321): charge 0 to ask "is cumulative cost
            // already over the configured limit?" without pricing tokens (the
            // agent crate can't depend on lingxi-cost; the enforcer tracks cost
            // globally, exactly like TS reading `getTotalCost()`).
            //
            // Placement remains at the TOP of the turn (stop before spending)
            // rather than TS's post-message check, so an already-over-budget
            // child makes zero additional round-trips. The denial string uses
            // the same current/maximum bytes as Claude Code 2.1.217's background
            // task budget halt when the configured ceiling is available.
            if let Some(b) = &budget {
                if let Err(lingxi_core::host::budget::BudgetError::Exceeded { current_nano_usd }) =
                    b.check_and_charge(0).await
                {
                    // Stop with the 2.1.217 background-agent budget string.
                    #[allow(clippy::cast_precision_loss)]
                    let dollars = current_nano_usd as f64 / 1_000_000_000.0;
                    let error = b.max_session_nano_usd().map_or_else(
                        || format!("Budget exceeded (${dollars:.2}); stopped."),
                        |limit_nano_usd| {
                            let whole = limit_nano_usd / 1_000_000_000;
                            let fractional = limit_nano_usd % 1_000_000_000;
                            let maximum = if fractional == 0 {
                                whole.to_string()
                            } else {
                                let fraction = format!("{fractional:09}");
                                format!("{whole}.{}", fraction.trim_end_matches('0'))
                            };
                            format!(
                                "Budget limit reached (${dollars:.2} of ${maximum}); stopping background agents."
                            )
                        },
                    );
                    publish_prompt_hook_transcript(&ctx, history, &last_usage);
                    emit_failed(
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        error,
                        cumulative_usage.clone(),
                    )
                    .await;
                    return;
                }
                // `Ok` and `BudgetError::Internal` fall through to the round-trip:
                // TS has no analog branch that errors the loop on an internal
                // budget condition, so an internal failure is non-fatal here.
            }

            // Race the model round-trip against a user-termination event. A
            // UserExit / UserInterrupt on event_rx aborts the loop -> Killed.
            // Any other inbound event is ignored (the loop is self-driving) and
            // we re-issue the round-trip on the next iteration.
            //
            // The round-trip goes over the STREAMING seam: open the SSE stream and
            // drain it through `accumulate_stream` into the same `MessageResponse`
            // the non-streaming path produced (the default `messages_create_stream`
            // wraps `messages_create` losslessly, so a non-streaming client behaves
            // identically). Dropping this future on the termination arm cancels the
            // in-flight stream, exactly as dropping a non-streaming call would.
            // Per-request thinking-effort (claude-code `me.effort`): the subagent's
            // resolved effort (its definition's, possibly overridden by a workflow
            // `agent({effort})` opt at spawn) → `output_config.effort`.
            let effort_wire = ctx
                .agent_definition
                .effort
                .as_ref()
                .map(crate::definition::AgentEffort::to_wire);
            // `StructuredOutputMode::WhenDone` (Fusion panels, WP2a item 1): let
            // the model use its other tools with normal (auto) `tool_choice`
            // while turns remain; force `StructuredOutput` only on the run's
            // LAST turn, or once it has produced two consecutive turns with no
            // tool call at all (`whendone_idle_turns`, updated after the
            // round-trip below). `StructuredOutputMode::Forced` (the default,
            // byte-parity with pre-WP2a behavior) forces every turn
            // unconditionally. Computed once per turn (stable across any
            // watchdog retry of the SAME turn below).
            let is_last_turn = turn_idx + 1 == max_turns;
            let force_this_turn = force_structured_tool.is_some()
                && (force_every_turn || is_last_turn || whendone_idle_turns >= 2);
            // (M9 cc2.1.198 wake-on-message) Captured by the wake arm in the
            // select below and appended to `history` HERE, before the next
            // `api_call` is built, because the in-flight future immutably
            // borrows `history` inside the select.
            let mut wake_message: Option<String> = None;
            let watchdog = api_client.workflow_query_watchdog();
            let mut watchdog_retry_count = 0_u32;
            let model_attempt = match ctx
                .model_attempt
                .as_ref()
                .map(|context| context.fresh_call())
                .transpose()
            {
                Ok(context) => context,
                Err(error) => {
                    emit_failed(
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        error.to_string(),
                        cumulative_usage.clone(),
                    )
                    .await;
                    return;
                }
            };
            let retry_response_body = watchdog.is_some_and(|policy| policy.retry_response_body);
            let (response, local_only_response) = loop {
                // Only the opened body is retried here; ApiService already owns
                // connect-phase retries. Any server-side content revokes replay
                // even when its block never closes and cannot be salvaged.
                let stream_opened = std::sync::atomic::AtomicBool::new(false);
                let retryable_body = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                // (M9) A wake message injected below rides into the next
                // round-trip as a user turn. The runner is already active here.
                if let Some(content) = wake_message.take() {
                    history.push(ConversationMessage::user(MessageId::new(), content));
                }
                maybe_emit_near_limit_wrap_up(
                    history,
                    &ctx,
                    api_client.as_ref(),
                    &out_tx,
                    agent_id,
                )
                .await;
                let api_call = async {
                    let current_model = model.clone();
                    tracing::debug!(
                        agent_id = %agent_id,
                        model = %current_model,
                        event = "query_started"
                    );
                    // Provider routing (dual-LLM dual-PROVIDER): thread the
                    // per-spawn `model_profile` as the api client's `profile` so the
                    // round-trip targets the candidate's resolved provider. `None`
                    // ⇒ default/unscoped resolution (legacy). The `_in` variants
                    // default to the profile-less methods, so a client that only
                    // implements the legacy seam is unaffected.
                    //
                    // The error carries the partial content blocks completed
                    // before the failure so the arm below can SALVAGE them (CC
                    // 2.1.207 `api_error_partial`). A connect-phase error yields no
                    // partial (empty vec); a mid-stream error yields whatever
                    // blocks were finalized.
                    let profile = ctx.model_profile.as_deref();
                    let messages_for_api = cap_input_bytes(history, ctx.max_input_bytes_per_turn)
                        .map_err(|message| {
                        (Vec::new(), LlmError::InvalidRequest { message })
                    })?;
                    let call_opts = crate::api::SubagentApiCallOpts {
                        model_attempt: model_attempt.clone(),
                        max_output_tokens: ctx.max_output_tokens_per_turn,
                        query_source_label: ctx.query_source_label.clone(),
                    };
                    let open_stream = async {
                        // Combine both fixes, per-mode rather than by blind
                        // conjunction (round-3 review items 2/3/7/9/20 fix:
                        // the merge's plain `&&` made `force_this_turn`'s
                        // last-turn / two-idle-turn force permanently dead on
                        // the wire for every real Fusion panel, since a panel
                        // always advertises more than one tool). Under
                        // `StructuredOutputMode::Forced` (`force_every_turn`)
                        // the oracle-verified P0-1 gate is preserved exactly:
                        // pinning `tool_choice` is only safe when
                        // StructuredOutput is the ONLY advertised tool, since
                        // pinning it otherwise silently removes the model's
                        // ability to call any other tool for the rest of the
                        // turn — and under `Forced`, `force_this_turn` is
                        // true on EVERY turn, so a bare OR would re-pin every
                        // round-trip of a multi-tool schema subagent and
                        // regress P0-1. Under `StructuredOutputMode::WhenDone`
                        // (`!force_every_turn`), `force_this_turn` is true
                        // only on the run's designated last-chance turn (the
                        // final turn, or after two consecutive idle turns) —
                        // exactly the turn the policy singles out as having
                        // nothing else useful left to explore — so that turn
                        // pins `tool_choice` regardless of how many other
                        // tools are advertised, restoring the documented
                        // "forced only on the last turn" contract
                        // (`platform-api/src/subagent_spawn.rs`,
                        // `fusion/src/panel.rs`) for every real panel.
                        if force_this_turn
                            && (force_tool_choice_for_api.is_some() || !force_every_turn)
                        {
                            api_client
                                .messages_create_stream_forced_in_opts(
                                    &current_model,
                                    profile,
                                    system.as_deref(),
                                    messages_for_api,
                                    tool_schemas.clone(),
                                    force_structured_tool,
                                    effort_wire.clone(),
                                    call_opts,
                                )
                                .await
                        } else {
                            api_client
                                .messages_create_stream_in_opts(
                                    &current_model,
                                    profile,
                                    system.as_deref(),
                                    messages_for_api,
                                    tool_schemas.clone(),
                                    effort_wire.clone(),
                                    call_opts,
                                )
                                .await
                        }
                    };
                    let stream = await_workflow_query_phase(
                        open_stream,
                        watchdog,
                        "opening the response stream",
                    )
                    .await
                    .map_err(|e| (Vec::new(), e))?;
                    stream_opened.store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::debug!(
                        agent_id = %agent_id,
                        model = %current_model,
                        event = "stream_opened"
                    );
                    let stream = with_workflow_stream_watchdog(stream, watchdog);
                    let mut first_event_seen = false;
                    let stream = stream.inspect({
                        let out_tx = out_tx.clone();
                        let current_agent_id = agent_id;
                        let retryable_body = retryable_body.clone();
                        move |event| {
                            if retry_response_body {
                                let local_content = |block: &llm_runtime::ContentBlock| {
                                    matches!(
                                        block,
                                        llm_runtime::ContentBlock::Text { .. }
                                            | llm_runtime::ContentBlock::Reasoning { .. }
                                            | llm_runtime::ContentBlock::RedactedThinking { .. }
                                            | llm_runtime::ContentBlock::ToolCall { .. }
                                    )
                                };
                                let has_server_content = match event {
                                    Ok(HistoryEvent::ContentBlockStart {
                                        content_block, ..
                                    }) => !local_content(content_block),
                                    Ok(
                                        HistoryEvent::MessageStart { response }
                                        | HistoryEvent::Completed { response },
                                    ) => response.content.iter().any(|block| !local_content(block)),
                                    _ => false,
                                };
                                if has_server_content {
                                    retryable_body
                                        .store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            if first_event_seen || event.is_err() {
                                return;
                            }
                            first_event_seen = true;
                            tracing::debug!(
                                agent_id = %current_agent_id,
                                model = %current_model,
                                event = "first_event"
                            );
                            // Preserve stream order: a detached send can arrive
                            // after the response's terminal event. This beacon is
                            // best-effort, so a synchronous try_send is sufficient.
                            let _ = out_tx.try_send(SubagentEvent::Progress {
                                agent_id: current_agent_id,
                                tool_use_count: u32::try_from(total_tool_use_count)
                                    .unwrap_or(u32::MAX),
                                token_count: 0,
                            });
                        }
                    });
                    crate::accumulator::accumulate_stream_salvaging(Box::pin(stream)).await
                };
                let attempt_result = if !event_channel_open {
                    api_call.await
                } else if model_attempt.is_some() {
                    // Registered panel calls cannot silently abandon one wire
                    // owner and issue another for an unrelated event. Keep the
                    // same future until response or explicit user cancellation.
                    tokio::pin!(api_call);
                    loop {
                        tokio::select! {
                            biased;
                            ev = event_rx.recv(), if event_channel_open => {
                                match ev {
                                    Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                                        emit_killed(&out_tx, transcript.as_ref(), history,
                                            &mut transcript_written, agent_id).await;
                                        return;
                                    }
                                    None => event_channel_open = false,
                                    Some(_) => {}
                                }
                            }
                            response = &mut api_call => break response,
                        }
                    }
                } else {
                    tokio::select! {
                        biased;
                        ev = event_rx.recv() => {
                            match ev {
                                Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                                    if let Some(executor) = &ctx.hook_executor {
                                        executor.take_agent_prompt_transcript(ctx.hook_session_id, ctx.agent_id);
                                    }
                                    emit_killed(
                                        &out_tx,
                                        transcript.as_ref(),
                                        history,
                                        &mut transcript_written,
                                        agent_id,
                                    )
                                    .await;
                                    return;
                                }
                                // (M9 cc2.1.198 wake-on-message) messaging a stuck
                                // persistent teammate wakes it: drop the in-flight
                                // future, append the message to history (above), and
                                // re-issue the round-trip NOW — previously this fell
                                // into the catch-all below and silently DISCARDED the
                                // text. A registry-backed foreground runner can also
                                // become resumable after Ctrl+B, so preserve intentional
                                // messages there. Task notifications never use this arm.
                                Some(lingxi_core::Event::UserMessage { content, .. })
                                    if ctx.persistent || ctx.task_registry.is_some() =>
                                {
                                    wake_message = Some(content);
                                    continue;
                                }
                                // Non-termination event: drop the in-flight API future
                                // and retry the round-trip on the next iteration.
                                Some(_) => continue,
                                // Channel closed: stop racing it from now on.
                                None => {
                                    event_channel_open = false;
                                    continue;
                                }
                            }
                        }
                        resp = api_call => resp,
                    }
                };

                if let Err((_partial_blocks, error)) = &attempt_result {
                    let interrupted_body = retry_response_body
                        && matches!(error, LlmError::Transport { .. })
                        && stream_opened.load(std::sync::atomic::Ordering::Relaxed);
                    if (is_workflow_watchdog_timeout(error) || interrupted_body)
                        && (!retry_response_body
                            || retryable_body.load(std::sync::atomic::Ordering::Relaxed))
                        && model_attempt.is_none()
                        && watchdog.is_some_and(|policy| watchdog_retry_count < policy.max_retries)
                    {
                        // No local tool from this attempt has dispatched: the
                        // runner executes tools only after full accumulation.
                        // Keep earlier turns/results, discard this body, and
                        // mark its unavailable usage rather than claiming a
                        // complete total if the replacement call succeeds.
                        if retry_response_body {
                            usage_complete = false;
                        }
                        watchdog_retry_count = watchdog_retry_count.saturating_add(1);
                        let model_attempt = watchdog_retry_count.saturating_add(1);
                        let reason = error.to_string();
                        tracing::warn!(
                            agent_id = %agent_id,
                            attempt = model_attempt,
                            reason = %reason,
                            event = "query_retry"
                        );
                        api_client
                            .observe_workflow_query_retry(agent_id, model_attempt, reason)
                            .await;
                        continue;
                    }
                }
                break (
                    attempt_result,
                    retryable_body.load(std::sync::atomic::Ordering::Relaxed),
                );
            };

            let response = match response {
                Ok(r) => r,
                Err((partial_blocks, e)) => {
                    if let LlmError::MalformedToolInput {
                        tool_name,
                        has_other_tool_calls,
                        ..
                    } = &e
                    {
                        // The Host enables this policy only for Create Local App.
                        // Ordinary agents keep the existing StructuredOutput-only
                        // recovery policy, even when parse retries are configured.
                        let create_local_tool = retry_response_body
                            && ctx.tool_invoker.is_some()
                            && (allowed_tools.is_empty() || allowed_tools.contains(tool_name))
                            && tool_schemas.iter().any(|tool| {
                                tool.get("name").and_then(serde_json::Value::as_str)
                                    == Some(tool_name.as_str())
                                    && tool.get("input_schema").is_some()
                                    && tool.get("type").is_none()
                            });
                        let can_recover = (force_structured_tool == Some(tool_name.as_str())
                            || create_local_tool)
                            && (!retry_response_body || local_only_response)
                            && !has_other_tool_calls
                            && structured_parse_retry_cap > 0
                            && model_attempt.is_none();
                        if can_recover
                            && structured_parse_retries < structured_parse_retry_cap
                            && turn_idx + 1 < max_turns
                        {
                            structured_parse_retries += 1;
                            // The parse-error result has no authoritative usage;
                            // a later success must not imply all calls were counted.
                            usage_complete = false;
                            let reason = format!(
                                "{tool_name} JSON correction {structured_parse_retries}/{structured_parse_retry_cap}: {e}"
                            );
                            api_client
                                .observe_workflow_query_retry(
                                    agent_id,
                                    structured_parse_retries + 1,
                                    reason.clone(),
                                )
                                .await;
                            // Keep completed turns and their tool results. Nothing
                            // from this malformed response is dispatched or replayed.
                            let correction = ConversationMessage::user_meta(
                                MessageId::new(),
                                format!(
                                    "{reason}. The malformed response was discarded. Reuse the completed tool results above. Call {tool_name} again with one complete, concise JSON object matching its schema; omit unrelated fields and do not repeat completed tools."
                                ),
                            );
                            history.push(correction.clone());
                            emit_message(&out_tx, agent_id, &correction).await;
                            flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                                .await;
                            // Re-enter the normal turn boundary: budgets, cancellation,
                            // model call ownership and max_turns still apply.
                            continue;
                        }
                        let error = if can_recover && structured_parse_retries > 0 {
                            format!(
                                "{tool_name} JSON recovery stopped after {structured_parse_retries} retries (limit {structured_parse_retry_cap}, turn {}/{max_turns}): {e}",
                                turn_idx + 1
                            )
                        } else {
                            format!("subagent output error: {e}")
                        };
                        publish_prompt_hook_transcript(&ctx, history, &last_usage);
                        emit_failed(
                            &out_tx,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            agent_id,
                            error,
                            cumulative_usage.clone(),
                        )
                        .await;
                        return;
                    }
                    if is_workflow_watchdog_timeout(&e) {
                        publish_prompt_hook_transcript(&ctx, history, &last_usage);
                        emit_failed(
                            &out_tx,
                            transcript.as_ref(),
                            history,
                            &mut transcript_written,
                            agent_id,
                            format!(
                                "{} {e}",
                                lingxi_core::host::subagent_spawn::SUBAGENT_QUERY_TIMEOUT_REASON_PREFIX
                            ),
                            cumulative_usage.clone(),
                        )
                        .await;
                        return;
                    }
                    // CC 2.1.207 subagent `api_error_partial` recovery
                    // (`Wyd`/`wTy`): when the round-trip is cut off by an API
                    // TERMINATION whose kind is in `CTy`
                    // ({rate_limit,overloaded,server_error}) AND the agent has
                    // already produced content (a prior completed turn OR blocks
                    // salvaged from THIS turn before the cutoff), return the
                    // partial work as a `completed` result with the incomplete-
                    // output `cutoffNote` prepended — instead of failing the
                    // whole tool call and discarding everything the child did.
                    // Schema-bound roles cannot use this prose fallback: it
                    // bypasses structured-output validation and makes a failed
                    // workflow stage appear completed. Preserve the API error
                    // as Failed so workflow throwOnError can surface its cause.
                    // Every other kind (auth/invalid/quota/…) or an empty
                    // transcript also rethrows as `Failed`.
                    let salvaged = translate_response_blocks(&partial_blocks);
                    match classify_api_termination(&e) {
                        Some((_error_kind, api_error_text))
                            if ctx.schema.is_none()
                                && !final_text_blocks(history, &salvaged).is_empty() =>
                        {
                            let cutoff_note = build_cutoff_note(api_error_text);
                            let result = build_recovered_result(history, &salvaged, &cutoff_note);
                            if !salvaged.is_empty() {
                                let partial_message = ConversationMessage::Assistant {
                                    id: MessageId::new(),
                                    content: salvaged,
                                    stop_reason: Some("api_error".to_string()),
                                };
                                history.push(partial_message.clone());
                                emit_message(&out_tx, agent_id, &partial_message).await;
                                assistant_message_count = assistant_message_count.saturating_add(1);
                            }
                            // Make the transcript observable before publishing the
                            // terminal event. The receiver may release the runner as
                            // soon as it sees `Completed`.
                            flush_transcript(transcript.as_ref(), history, &mut transcript_written)
                                .await;
                            if let Some(writer) = transcript.as_ref() {
                                let _ = writer.record_terminal("completed", None).await;
                            }
                            publish_prompt_hook_transcript(&ctx, history, &last_usage);
                            let _ = out_tx
                                .send(SubagentEvent::Completed {
                                    agent_id,
                                    result,
                                    usage: last_usage.clone(),
                                    total_tool_use_count,
                                    total_duration_ms: elapsed_ms(run_start),
                                    assistant_message_count,
                                    last_request_id: last_request_id.clone(),
                                    cumulative_usage: cumulative_usage.clone(),
                                    // Finding [9]: this is the api_error_partial
                                    // salvage — `usage`/`cumulative_usage` above
                                    // are stale (last SUCCESSFUL turn), the
                                    // failed turn's real spend is not captured.
                                    usage_complete: false,
                                })
                                .await;
                            return;
                        }
                        _ => {
                            publish_prompt_hook_transcript(&ctx, history, &last_usage);
                            emit_failed(
                                &out_tx,
                                transcript.as_ref(),
                                history,
                                &mut transcript_written,
                                agent_id,
                                format!("subagent api error: {e}"),
                                cumulative_usage.clone(),
                            )
                            .await;
                            return;
                        }
                    }
                }
            };

            // Keep the FINAL response usage for the terminal `Completed` rollup
            // (claude `getTokenCountFromUsage` reads the LAST assistant usage — so
            // overwrite, never accumulate, to stay byte-faithful).
            //
            // WP2a item 2 (F002 sub-claim 3): `max_output_tokens_per_turn` is a
            // WIRE ceiling forwarded to the provider via
            // `SubagentApiCallOpts::max_output_tokens` (below) — it is NOT a
            // clamp on the REPORTED usage. Reporting min(real, ceiling) here
            // hid a provider overrun from `cumulative_usage`/settlement instead
            // of surfacing it; report the provider's real usage verbatim.
            last_usage = response.usage.clone();
            live_hook_transcript.last_usage_tokens = usize::try_from(
                last_usage
                    .counts()
                    .input_tokens
                    .saturating_add(
                        last_usage
                            .counts()
                            .output_tokens
                            .saturating_sub(last_usage.counts().reasoning_tokens),
                    )
                    .saturating_add(last_usage.counts().cache_read_tokens)
                    .saturating_add(last_usage.counts().cache_write_tokens),
            )
            .unwrap_or(usize::MAX);
            accumulate_usage(&mut cumulative_usage, &last_usage);
            emit_progress(
                &out_tx,
                agent_id,
                total_tool_use_count,
                cumulative_usage
                    .counts()
                    .input_tokens
                    .saturating_add(cumulative_usage.counts().cache_write_tokens)
                    .saturating_add(cumulative_usage.counts().cache_read_tokens)
                    .saturating_add(
                        cumulative_usage
                            .counts()
                            .output_tokens
                            .saturating_sub(cumulative_usage.counts().reasoning_tokens),
                    ),
            )
            .await;
            // Track the assistant-message count (claude `agentMessages.length`) and
            // the FINAL turn's provider request id (claude
            // `lastAssistantMessage.requestId`). One assistant turn per round-trip;
            // `response.id` is the provider response id (the `requestId` analog).
            assistant_message_count = assistant_message_count.saturating_add(1);
            last_request_id = if response.id.is_empty() {
                None
            } else {
                Some(response.id.clone())
            };

            // Build the assistant turn and append to history.
            let assistant_blocks = translate_response_blocks(&response.content);
            let stop_reason = response.stop_reason.clone();
            let assistant_msg = ConversationMessage::Assistant {
                id: MessageId::new(),
                content: assistant_blocks.clone(),
                stop_reason: stop_reason.clone(),
            };
            history.push(assistant_msg.clone());
            emit_message(&out_tx, agent_id, &assistant_msg).await;

            // Extract tool_use blocks.
            let tool_uses: Vec<(
                lingxi_core::types::ToolUseId,
                String,
                serde_json::Value,
                Option<String>,
            )> = assistant_blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        provider_id,
                    } => Some((id.clone(), name.clone(), input.clone(), provider_id.clone())),
                    _ => None,
                })
                .collect();

            // Accumulate the run-wide tool-use count (claude `totalToolUseCount`).
            total_tool_use_count = total_tool_use_count.saturating_add(tool_uses.len() as u64);

            // `StructuredOutputMode::WhenDone`: reset the idle streak the moment
            // ANY tool is called (including a `StructuredOutput` attempt — a
            // failed validation still means the model tried); otherwise count
            // this as one more consecutive no-tool turn. No-op under `Forced`
            // (every turn is already forced, so `whendone_idle_turns` is unread).
            let recoverable_structured_truncation = retry_response_body
                && force_structured_tool.is_some()
                && stop_reason.as_deref() == Some("max_tokens");
            if tool_uses.is_empty() && !recoverable_structured_truncation {
                whendone_idle_turns = whendone_idle_turns.saturating_add(1);
            } else if !tool_uses.is_empty() {
                whendone_idle_turns = 0;
            }

            // Dispatch any tool_use blocks FIRST, then decide loop disposition by
            // stop_reason — mirroring the orchestrator references. `execute_one_turn`
            // (turn_loop.rs:111) always dispatches tool_uses when present regardless
            // of stop_reason, and the streaming path (conversation.rs:815-839) only
            // continues on `Some("tool_use")` with non-empty tool_uses, terminating
            // on end_turn / None / any other reason. Deciding terminality *before*
            // dispatch (the prior `tool_uses.is_empty() || end_turn` test) silently
            // dropped tool calls on an `end_turn`+tool_use response (MAJOR #1) and
            // looped to max_turns on a truncated/refused turn that still carried
            // tool_uses (MAJOR #2).
            if !tool_uses.is_empty() {
                let file_write_requested = tool_uses
                    .iter()
                    .any(|(_, name, _, _)| matches!(name.as_str(), "Write" | "Edit"));
                // Dispatch each tool_use through the inherited invoker.
                let Some(invoker) = &ctx.tool_invoker else {
                    publish_prompt_hook_transcript(&ctx, history, &last_usage);
                    emit_failed(
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        "subagent requested a tool but no tool_invoker was inherited".to_string(),
                        cumulative_usage.clone(),
                    )
                    .await;
                    return;
                };

                let mut tool_results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
                for (tool_use_id, name, input, provider_id) in &tool_uses {
                    // Structured output: the synthetic `StructuredOutput` tool is not
                    // dispatched — its input IS the run's result, but ONLY when it
                    // VALIDATES against the schema (claude-code Ajv validation inside
                    // the tool `call`). A valid input is captured + a benign result fed
                    // back (the loop terminates below); an INVALID input feeds back an
                    // `is_error` ToolResult (`Output does not match required schema: …`)
                    // and increments the failed-validation count `kn` — the model sees
                    // the error and retries on the next turn (the `tool_use` stop keeps
                    // the loop going), up to the retry cap checked after dispatch.
                    if force_structured_tool == Some(name.as_str()) {
                        match validate_structured_output(ctx.schema.as_deref(), input) {
                            Ok(()) => {
                                structured_result = Some(input.clone());
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: tool_use_id.clone(),
                                    // claude's StructuredOutput tool returns
                                    // `data: "Structured output provided successfully"`
                                    // as the tool-result content (binary 2.1.195
                                    // strings :311431 / :438281).
                                    content: "Structured output provided successfully".to_string(),
                                    is_error: false,
                                    provider_tool_use_id: provider_id.clone(),
                                    content_blocks: None,
                                });
                            }
                            Err(detail) => {
                                structured_failed_count = structured_failed_count.saturating_add(1);
                                tool_results.push(ContentBlock::ToolResult {
                                    tool_use_id: tool_use_id.clone(),
                                    content: format!(
                                        "Output does not match required schema: {detail}"
                                    ),
                                    is_error: true,
                                    provider_tool_use_id: provider_id.clone(),
                                    content_blocks: None,
                                });
                            }
                        }
                        continue;
                    }
                    // Allow-list guard: when `allowed_tools` is non-empty, a model
                    // request for a tool outside it is refused WITHOUT dispatching
                    // (the inherited `RegistryToolInvoker` would otherwise run any
                    // registered tool by name). Surfaced as an `is_error` ToolResult
                    // so the model sees the refusal and can recover, mirroring how a
                    // tool error is fed back. Empty `allowed_tools` skips the guard.
                    if !allowed_tools.is_empty() && !allowed_tools.iter().any(|t| t == name) {
                        // yyo companion note (binary v2.1.186 §7): when the blocked tool
                        // is in the `nke` external companion set, append the byte-exact
                        // guidance suffix so the model knows the tool is a subagent
                        // boundary, not a typo. Gate on USER_TYPE like HDd.
                        let is_ant = std::env::var("USER_TYPE").is_ok_and(|v| v == "ant");
                        let note =
                            companion_note_for_disallowed_tool(name, is_ant).unwrap_or_default();
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: format!(
                                "tool {name:?} is not in this agent's allowed tools{note}"
                            ),
                            is_error: true,
                            provider_tool_use_id: provider_id.clone(),
                            content_blocks: None,
                        });
                        continue;
                    }
                    let inv_ctx = lingxi_core::host::tool_invoker::SubagentInvocationContext {
                        permission_pause_observer: ctx.task_registry.clone().map(|registry| {
                            lingxi_core::host::permission_gate::PermissionPauseObserver::new(
                                move |ms| {
                                    registry.add_permission_paused_ms(agent_id, ms);
                                },
                            )
                        }),
                        // The tools run on behalf of THIS agent. The field is
                        // named parent because it becomes the parent of any
                        // recursively spawned child, not this agent's parent.
                        parent_agent_id: Some(agent_id),
                        origin_session_id: ctx.origin_session_id,
                        // This is selected from the host-resolved definition,
                        // never from the model's tool input or telemetry. The
                        // hidden Fusion panel definition opts into deterministic
                        // WebFetch; every other Agent keeps ordinary behavior.
                        tool_execution_policy: if ctx.agent_definition.agent_type
                            == lingxi_core::host::FUSION_PANEL_TYPE
                        {
                            lingxi_core::host::tool_invoker::ToolExecutionPolicy::FusionPanel
                        } else {
                            lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary
                        },
                        // Swarm identity (claude-code `getAgentName()` /
                        // `getTeammateContext()?.teamName`): a teammate's dispatched
                        // tools see the teammate's DISPLAY name + team name so the
                        // swarm-only `TaskUpdate` side-effects key on them. `None`
                        // for one-shot subagents / the main thread.
                        agent_name: ctx.agent_name.clone(),
                        team_name: ctx.team_name.clone(),
                        // R1: an async (backgrounded) subagent runs its tools with
                        // is_non_interactive_session=true (claude-code runAgent.ts:668-672).
                        is_async: ctx.is_async,
                        is_non_interactive_session: ctx.is_async
                            || lingxi_core::host::session_flags::effective_non_interactive_session(
                            ),
                        // Whether this worker may surface a permission prompt to the
                        // user — drives the worker attribution on the prompt dialog
                        // (claude-code's worker permission badge).
                        can_show_permission_prompts: ctx.can_show_permission_prompts,
                        // Per-agent cwd (worktree isolation / explicit cwd) → the
                        // dispatched tools' working directory.
                        cwd: ctx.cwd.clone(),
                        // The REAL `tool_use` block id of THIS dispatching call
                        // (claude-code `createCanUseTool(toolUseID)`): threaded into
                        // the gate's `PermissionCheckContext` so a subagent's stdio
                        // `can_use_tool` prompt carries the byte-faithful id instead
                        // of a freshly minted one — matching the main loop's path.
                        tool_use_id: Some(tool_use_id.as_str().to_string()),
                        // Every tool in this batch belongs to the assistant turn built
                        // above. Preserve that exact id for per-call telemetry rather
                        // than minting a substitute at the invocation boundary.
                        assistant_message_id: Some(assistant_msg.id()),
                        // This subagent's own recursion depth (claude `agentContext.depth`)
                        // → mapped into the dispatched tool's `ToolUseContext.depth`, so a
                        // nested `Agent` call computes the grandchild's depth (`depth+1`)
                        // and the resolver applies the configured spawn-depth cap.
                        depth: ctx.depth,
                        observer: ctx
                            .observer
                            .as_ref()
                            .filter(|observer| {
                                observer.observe_subagents
                                    && ctx.depth < crate::observer::DEFAULT_OBSERVER_FANOUT_DEPTH
                            })
                            .cloned(),
                        // This subagent's OWN resolved main-loop model — so a NESTED
                        // `Agent` tool call resolves its child's model against THIS
                        // subagent's model (claude-code `runAgent.ts:678` seeds each
                        // child's `mainLoopModel: resolvedAgentModel`), read by the
                        // recursive AgentTool via `ToolUseContext.options.main_loop_model`
                        // (claude `AgentTool.tsx:418`). The RegistryToolInvoker maps this
                        // into that field; the definition's model is already the concrete
                        // Explicit id resolved at spawn time.
                        parent_model: Some(resolve_model(&ctx)),
                        parent_model_profile: ctx.model_profile.clone(),
                        // This subagent's EFFECTIVE permission mode (claude-code
                        // 2.1.207 Agent `mode` → the child's
                        // `toolPermissionContext.mode`, `wKe`/`ve`): threaded into
                        // the dispatch gate's `PermissionCheckContext` so the child's
                        // tool calls authorize under it (a `mode:"plan"` child gates
                        // mutations while reads stay frictionless). `None` = inherit
                        // the gate's live/boot mode.
                        mode_override: ctx.permission_mode_override.clone(),
                        request_source: None,
                        // Replay the fork-time command denies for every tool this subagent
                        // dispatches (claude `freezeCommandDenies`).
                        frozen_command_denies: ctx.frozen_command_denies.clone(),
                    };
                    match invoker
                        .invoke_detailed(name, input.clone(), inv_ctx, None)
                        .await
                    {
                        Ok(invocation) => {
                            let value = invocation.data;
                            // A subagent reads tool results through the same
                            // eyes the main loop does. Deriving the media blocks
                            // here — from the SHARED rule, not a copy of it — is
                            // what makes an image-returning tool usable at all
                            // from a Task or a workflow stage: without it the
                            // payload arrives as base64 TEXT, which the model
                            // cannot look at and which then sits in history for
                            // the rest of the run. `LocalAppCaptureUi` exists
                            // almost entirely for the `frontend-qa` verify
                            // stage, and that stage is a subagent.
                            let content_blocks =
                                tool_api::tool_result_media::media_content_blocks(&value);
                            let content = match (
                                content_blocks.as_ref().and_then(|_| {
                                    tool_api::tool_result_media::ephemeral_summary(&value)
                                }),
                                &value,
                            ) {
                                // The blocks carry the payload; the text says so
                                // in one line instead of repeating a megabyte of
                                // base64 beside them.
                                (Some(summary), _) => summary,
                                (None, serde_json::Value::String(s)) => s.clone(),
                                (None, other) => other.to_string(),
                            };
                            let content = invocation.model_content.unwrap_or(content);
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: tool_use_id.clone(),
                                content,
                                is_error: invocation.is_error,
                                provider_tool_use_id: provider_id.clone(),
                                content_blocks,
                            });
                        }
                        Err(lingxi_core::host::tool_invoker::ToolInvokerError::Abort(error)) => {
                            publish_prompt_hook_transcript(&ctx, history, &last_usage);
                            emit_failed(
                                &out_tx,
                                transcript.as_ref(),
                                history,
                                &mut transcript_written,
                                agent_id,
                                error,
                                cumulative_usage.clone(),
                            )
                            .await;
                            return;
                        }
                        Err(e) => {
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: tool_use_id.clone(),
                                // Match the main turn-loop convention
                                // (`turn_loop.rs` `"Error: {bare}"`): the bare
                                // model-facing message, NOT the `Display` form which
                                // would leak the LingXi-internal `ToolInvoker: …`
                                // prefix into the child's tool_result wire bytes.
                                content: e.model_tool_result_content(),
                                is_error: true,
                                provider_tool_use_id: provider_id.clone(),
                                content_blocks: None,
                            });
                        }
                    }
                }

                let tool_results_msg = ConversationMessage::User {
                    id: MessageId::new(),
                    content: tool_results,
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                };
                history.push(tool_results_msg.clone());
                emit_message(&out_tx, agent_id, &tool_results_msg).await;
                if file_write_requested {
                    if let Some(block) = match &ctx.new_diagnostics_source {
                        Some(source) => source.take_new_diagnostics_block().await,
                        None => None,
                    } {
                        let diagnostics_message = ConversationMessage::user_meta(
                            MessageId::new(),
                            format!("<system-reminder>\n{block}\n</system-reminder>"),
                        );
                        history.push(diagnostics_message.clone());
                        emit_message(&out_tx, agent_id, &diagnostics_message).await;
                    }
                }
            }

            // claude `agent({schema})`: `kn>0 && kn>=Yr && rn===undefined` → throw the
            // retry-cap-exceeded error (surfaced here as a terminal `Failed`). The
            // model's StructuredOutput validations have exhausted the cap (`Yr`,
            // `MAX_STRUCTURED_OUTPUT_RETRIES ?? 5`) with no valid output captured.
            if force_structured_tool.is_some()
                && structured_result.is_none()
                && structured_failed_count > 0
                && structured_failed_count >= structured_retry_cap
            {
                let calls = if structured_failed_count == 1 {
                    "call"
                } else {
                    "calls"
                };
                publish_prompt_hook_transcript(&ctx, history, &last_usage);
                emit_failed(
                    &out_tx,
                    transcript.as_ref(),
                    history,
                    &mut transcript_written,
                    agent_id,
                    format!(
                        "agent({{schema}}): StructuredOutput retry cap ({structured_retry_cap}) exceeded \u{2014} {structured_failed_count} failed {calls} with no valid output"
                    ),
                cumulative_usage.clone(),
                )
                .await;
                return;
            }

            // Loop disposition. Continue ONLY when the model asked to use tools and
            // actually emitted some; every other case is terminal — including
            // `end_turn`, a stream with no stop_reason (`None`), and any other
            // reason (max_tokens / stop_sequence / pause_turn / refusal), even when
            // the truncated turn carried tool_uses we just dispatched.
            // A captured structured output terminates the run (it IS the result),
            // even though the forced tool call carries a `tool_use` stop reason.
            // A `refusal` stop is NOT terminal while the cascade still has a
            // hop left: swap the model and re-issue this turn against it, the
            // same as both main-thread loops (`turn_loop.rs:1333`,
            // `drivers/mod.rs:3192`, which likewise just Continue — neither
            // tombstones the refused turn). Without this a refusing subagent
            // simply ended its run.
            if stop_reason.as_deref() == Some("refusal") {
                let frame_id = MessageId::new();
                let notice_uuid = frame_id.as_uuid().to_string();
                if let Some(hop) =
                    refusal_cascade.next_hop(&ctx.refusal_fallback_chain, &model, notice_uuid)
                {
                    for report in &hop.declines {
                        tracing::info!(
                            event = "tengu_refusal_fallback_route_declined",
                            reason = report.as_str(),
                        );
                    }
                    model = hop.fallback_model.clone();
                    for emitted in hop.notices {
                        history.push(refusal_fallback_frame(frame_id, &emitted.banner));
                    }
                    continue;
                }
            }
            let should_continue = stop_reason.as_deref() == Some("tool_use")
                && !tool_uses.is_empty()
                && structured_result.is_none();
            if !should_continue {
                // Host-verified Create flows may exhaust the output budget before
                // doing any work. This is not completion: let the model continue
                // with the existing tool results instead of demanding a final
                // StructuredOutput immediately. Both this run-scoped budget and
                // max_turns bound recovery; never replay an already dispatched tool.
                if recoverable_structured_truncation && structured_result.is_none() {
                    if structured_truncation_retries < 2 && turn_idx + 1 < max_turns {
                        structured_truncation_retries += 1;
                        let continuation = ConversationMessage::user(
                            MessageId::new(),
                            "Your response reached the output token limit before completing. Continue the unfinished work with the available tools, keeping reasoning concise. Preserve completed work and do not repeat successful tool calls. Call StructuredOutput only when the requested work is complete.".to_string(),
                        );
                        history.push(continuation.clone());
                        emit_message(&out_tx, agent_id, &continuation).await;
                        continue;
                    }
                    publish_prompt_hook_transcript(&ctx, history, &last_usage);
                    emit_failed(
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        "agent({schema}): output token limit reached before completion; continuation budget exhausted".to_string(),
                        cumulative_usage.clone(),
                    )
                    .await;
                    return;
                }
                // claude `agent({schema})` SubagentStop nudge: when the model ends a
                // turn without a captured (valid) StructuredOutput, inject an
                // in-conversation nudge and run another turn — up to 2 nudges (`ft`).
                // After the 2nd, give up with the byte-exact "completed without
                // calling" error. (The validation-RETRY case — StructuredOutput called
                // but its input failed — does NOT reach here: that turn's `tool_use`
                // stop keeps `should_continue` true, so the model retries until the
                // retry cap above fires.)
                if force_structured_tool.is_some() && structured_result.is_none() {
                    // `StructuredOutputMode::WhenDone`: a text-only turn that
                    // wasn't forced is not an anomaly — the model is still free
                    // to use its other tools on a later turn. Loop back WITHOUT
                    // the nudge-then-fail escalation below, which exists to
                    // catch a FORCED turn the provider still answered with no
                    // tool call (a genuine anomaly under either mode). The
                    // `whendone_idle_turns` counter (updated above) forces the
                    // next turn once it reaches 2, guaranteeing termination
                    // without relying on this escalation at all.
                    //
                    // The next round-trip's request is built straight from
                    // `history` (`cap_input_bytes(history, ..)` at the top of
                    // the turn loop) — every OTHER exit from this arm, and
                    // every tool-dispatch continuation, appends a user message
                    // (the nudge below, or the pushed tool_results) before
                    // looping, so the request's last message is always
                    // user-authored. This is the one path that did not: append
                    // a lightweight wrap-up reminder now so the WhenDone idle
                    // path keeps that invariant too, WITHOUT spending a
                    // `structured_nudge_count` slot (that budget is reserved
                    // for the FORCED-turn anomaly below; this is routine).
                    if !force_this_turn {
                        let wrap_up = ConversationMessage::user(
                            MessageId::new(),
                            "Continue working, or call StructuredOutput now if you have your answer.".to_string(),
                        );
                        history.push(wrap_up.clone());
                        emit_message(&out_tx, agent_id, &wrap_up).await;
                        continue;
                    }
                    if structured_nudge_count < 2 {
                        structured_nudge_count = structured_nudge_count.saturating_add(1);
                        let nudge = ConversationMessage::user(
                            MessageId::new(),
                            "You did not call StructuredOutput. You MUST call StructuredOutput to return your answer \u{2014} the tool input IS your answer. Call it now.".to_string(),
                        );
                        history.push(nudge.clone());
                        emit_message(&out_tx, agent_id, &nudge).await;
                        // Re-run the turn loop with the nudge appended (still bounded
                        // by `max_turns`).
                        continue;
                    }
                    publish_prompt_hook_transcript(&ctx, history, &last_usage);
                    emit_failed(
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                        // Byte-locked to claude 2.1.195 (binary strings :331985 /
                        // :514141, the workflow `agent({schema})` runtime): the give-up
                        // wording is SINGULAR "(after in-conversation nudge)" with no
                        // count. (claude's exact in-conversation nudge body and its
                        // nudge count are not discoverable static strings in the binary,
                        // so the port's nudge text + 2× retry are left as-is.)
                        "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)".to_string(),
                    cumulative_usage.clone(),
                    )
                    .await;
                    return;
                }
                // A completion that arrived during the model request is folded only
                // after its stream finished, before publishing a terminal event.
                if turn_idx + 1 < max_turns && fold_task_notifications(&ctx, history).await {
                    continue;
                }
                // claude `finalizeAgentTool`: the result's `content` is the LAST
                // assistant message's text blocks, with a backward-scan fallback to
                // the most recent assistant message that has text when the final turn
                // was tool-only (agentToolUtils.ts:304-317). `history` already holds
                // the current assistant turn (pushed above) + every prior turn.
                // A `schema` run returns the captured StructuredOutput tool input.
                let result = match structured_result.take() {
                    Some(structured) => structured,
                    None => build_completed_result(
                        history,
                        &assistant_blocks,
                        stop_reason.as_deref(),
                        &model,
                    ),
                };
                // Persist all messages before publishing the terminal event; the
                // consumer is allowed to tear down a one-shot runner immediately.
                flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
                foreground_parked = park_foreground_owner(
                    &ctx,
                    &result,
                    &last_usage,
                    total_tool_use_count,
                    elapsed_ms(run_start),
                )
                .await;
                if foreground_parked {
                    ctx.is_async = true;
                    if let Some(writer) = transcript.as_ref() {
                        let _ = writer.record_terminal("idle", None).await;
                    }
                    emit_parked(&out_tx, agent_id).await;
                    terminated_cleanly = true;
                    break;
                }
                // Keep lifecycle state alongside the transcript so clients that
                // discover an agent after completion can distinguish it from a
                // still-running child. Persistent agents retain their parked row
                // and are projected as idle by the session-agent listing.
                if let Some(writer) = transcript.as_ref() {
                    let status = if ctx.persistent { "idle" } else { "completed" };
                    let _ = writer.record_terminal(status, None).await;
                }
                publish_prompt_hook_transcript(&ctx, history, &last_usage);
                let _ = out_tx
                    .send(SubagentEvent::Completed {
                        agent_id,
                        result,
                        usage: last_usage.clone(),
                        total_tool_use_count,
                        total_duration_ms: elapsed_ms(run_start),
                        assistant_message_count,
                        last_request_id: last_request_id.clone(),
                        cumulative_usage: cumulative_usage.clone(),
                        usage_complete,
                    })
                    .await;
                // Terminal stop for this turn-set: leave the inner turn loop and
                // let the persist decision below choose between returning
                // (non-persistent) and parking for the next message (persistent).
                terminated_cleanly = true;
                break;
            }
            // Otherwise loop to the next turn.
        }

        if !terminated_cleanly {
            // A schema run has no "whatever work was produced" to hand back: the
            // caller asked for an object matching its schema, and
            // `{"reason":"max_turns_exhausted"}` is not one. claude-code checks for
            // a captured structured result AFTER the whole subagent attempt has
            // ended — for ANY exit reason, not just the nudge path — and throws the
            // same terminal error when there is none. Oracle 2.1.258
            // (`~/.local/share/claude/versions/2.1.258`) @172635430, in the workflow
            // `agent()` wrapper `mn`, after its `for await` over the turn loop:
            //   `let U = we && E.structured !== void 0 ? tVn(E.structured, …) : void 0;`
            //   … `if(we){ if(U===void 0) throw Error("agent({schema}): subagent`
            //   `completed without calling StructuredOutput (after in-conversation`
            //   `nudge)"); … }`
            // (`we` = schema-presence flag, `U` = the captured output). Until this
            // change's `tool_choice` relaxation the model was forced to call
            // StructuredOutput on every round, so a schema run terminated within one
            // valid call or `structured_retry_cap` invalid ones and could not reach
            // `max_turns` at all; now that it chooses freely, it can — so this exit
            // has to carry the schema contract too.
            if force_structured_tool.is_some() && structured_result.is_none() {
                publish_prompt_hook_transcript(&ctx, history, &last_usage);
                emit_failed(
                    &out_tx,
                    transcript.as_ref(),
                    history,
                    &mut transcript_written,
                    agent_id,
                    "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)".to_string(),
                    cumulative_usage.clone(),
                )
                .await;
                return;
            }
            // The inner loop fell through: `max_turns` exhausted without a terminal
            // stop. claude-code surfaces this as a completion carrying a max-turns
            // reason rather than a hard failure, so the parent can still consume
            // whatever work was produced.
            flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;
            if let Some(writer) = transcript.as_ref() {
                let status = if ctx.persistent { "idle" } else { "completed" };
                let _ = writer.record_terminal(status, None).await;
            }
            publish_prompt_hook_transcript(&ctx, history, &last_usage);
            // The oracle's `bft` (src_162329786.js @3532630) finalizes a
            // max-turns exit through the SAME path as any other completion: the
            // last assistant message's text blocks are the result content, and a
            // `max_turns_reached` attachment only adds a harness NOTE in front of
            // them. Dropping the blocks here made every turn-limited subagent
            // return `(Subagent completed but returned no output.)` — the caller
            // lost whatever partial work the agent had reported. Build the normal
            // result and carry the reason alongside it, so the reason readers
            // (`tasks::handlers::local_agent::max_turns_reached_from`,
            // `fusion::panel::max_turns_exhausted_detail`) still see it.
            let mut result = build_completed_result(history, &[], None, &model);
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "reason".to_string(),
                    serde_json::Value::String("max_turns_exhausted".to_string()),
                );
                obj.insert("max_turns".to_string(), serde_json::json!(max_turns));
            }
            foreground_parked = park_foreground_owner(
                &ctx,
                &result,
                &last_usage,
                total_tool_use_count,
                elapsed_ms(run_start),
            )
            .await;
            if foreground_parked {
                ctx.is_async = true;
                if let Some(writer) = transcript.as_ref() {
                    let _ = writer.record_terminal("idle", None).await;
                }
                emit_parked(&out_tx, agent_id).await;
            } else {
                let _ = out_tx
                    .send(SubagentEvent::Completed {
                        agent_id,
                        result,
                        usage: last_usage.clone(),
                        total_tool_use_count,
                        total_duration_ms: elapsed_ms(run_start),
                        assistant_message_count,
                        last_request_id: last_request_id.clone(),
                        cumulative_usage: cumulative_usage.clone(),
                        usage_complete,
                    })
                    .await;
            }
        }

        // Flush the turn-set's messages to the per-agent transcript. Runs for
        // BOTH dispositions below — a one-shot subagent's transcript is just as
        // much a record as a persistent one's, and the `SubagentStop` hook
        // reports its path either way. Best-effort: a transcript write failure
        // must never mask the agent's result.
        flush_transcript(transcript.as_ref(), history, &mut transcript_written).await;

        // ----- Persist decision ------------------------------------------------
        // Non-persistent (batch-8) behavior: end after one turn-set. This preserves
        // today's exact semantics — every existing call site sets `persistent`
        // false, so they `return` here as before.
        if !ctx.persistent && !foreground_parked {
            return;
        }

        // Persistent teammate: park awaiting the next inbound `UserMessage`. If the
        // event channel has already closed, no message can ever arrive again, so we
        // terminate gracefully.
        if !event_channel_open {
            if let Some(writer) = transcript.as_ref() {
                let _ = writer.record_terminal("completed", None).await;
            }
            return;
        }
        loop {
            // Subscribe was established before the turn; checking after registering
            // the revision prevents a notification between check and park being lost.
            // Completed is consumed asynchronously by the handler. Wait for
            // its rest acknowledgement before starting a notification turn,
            // otherwise the old Completed can park a newly running owner.
            let handler_rested = match &ctx.task_registry {
                Some(registry) => {
                    registry
                        .can_wake_agent_for_task_notification(agent_id)
                        .await
                }
                None => true,
            };
            let before_notifications = history.len();
            if handler_rested && fold_task_notifications(&ctx, history).await {
                for message in &history[before_notifications..] {
                    emit_message(&out_tx, agent_id, message).await;
                }
                break;
            }
            let event = tokio::select! {
                event = event_rx.recv() => event,
                changed = async {
                    match notification_changes.as_mut() {
                        Some(receiver) => receiver.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() { notification_changes = None; }
                    continue;
                }
            };
            match event {
                Some(lingxi_core::Event::UserMessage { content, .. }) => {
                    if let Some(writer) = transcript.as_ref() {
                        let _ = writer.record_terminal("running", None).await;
                    }
                    // Append the injected message to history (minting our own
                    // MessageId, consistent with the assistant-id minting above —
                    // the event's message_id / request_id are the host's bookkeeping)
                    // and resume the inner turn loop with a fresh `max_turns` budget.
                    let message = ConversationMessage::user(MessageId::new(), content);
                    history.push(message.clone());
                    // Publish the wake before the next provider call, which may
                    // stall: observers use this user message to leave idle.
                    emit_message(&out_tx, agent_id, &message).await;
                    break;
                }
                Some(lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt) => {
                    if let Some(executor) = &ctx.hook_executor {
                        executor.take_agent_prompt_transcript(ctx.hook_session_id, ctx.agent_id);
                    }
                    emit_killed(
                        &out_tx,
                        transcript.as_ref(),
                        history,
                        &mut transcript_written,
                        agent_id,
                    )
                    .await;
                    return;
                }
                // Ignore any other event while idle and keep parking.
                Some(_) => {}
                // Channel closed -> graceful terminal shutdown. A persistent
                // child is idle only while this channel remains open.
                None => {
                    if let Some(writer) = transcript.as_ref() {
                        let _ = writer.record_terminal("completed", None).await;
                    }
                    return;
                }
            }
        }
    }
}

async fn park_foreground_owner(
    ctx: &SubagentContext,
    result: &serde_json::Value,
    usage: &llm_runtime::ExecutionUsage,
    tool_uses: u64,
    duration_ms: u64,
) -> bool {
    if ctx.persistent {
        return false;
    }
    let Some(registry) = &ctx.task_registry else {
        return false;
    };
    registry
        .park_foreground_agent(
            ctx.agent_id,
            lingxi_core::host::task_registry::AgentTerminalOutcome {
                result: result
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                usage: Some(lingxi_core::host::task_registry::AgentRunUsage {
                    subagent_tokens: crate::handle::subagent_usage_from_llm_usage(usage)
                        .total_tokens,
                    tool_uses,
                    duration_ms,
                }),
                max_turns_reached: result.get("max_turns").and_then(serde_json::Value::as_u64),
                ..Default::default()
            },
        )
        .await
}

/// Fold only at model boundaries or while parked, never racing the provider future.
async fn fold_task_notifications(
    ctx: &SubagentContext,
    history: &mut Vec<ConversationMessage>,
) -> bool {
    let Some(registry) = &ctx.task_registry else {
        return false;
    };
    let notifications = registry
        .take_pending_task_notifications_for(Some(ctx.agent_id))
        .await
        .unwrap_or_default();
    let reminders = lingxi_core::host::task_notification::render_reminders_with_options(
        &notifications,
        false,
        telemetry::push_notifications_enabled(),
    );
    let human = registry.take_human_task_messages_for(ctx.agent_id).await;
    let any = !reminders.is_empty() || !human.is_empty();
    if any {
        registry
            .activate_agent_for_task_notification(ctx.agent_id)
            .await;
    }
    for reminder in reminders {
        history.push(ConversationMessage::user_meta(MessageId::new(), reminder));
    }
    for message in human {
        history.push(ConversationMessage::user_meta(MessageId::new(), message));
    }
    any
}

/// Legacy reducer-driven stub.
///
/// Drives [`lingxi_core::reduce`] over `event_rx` and emits [`SubagentEvent`]s on
/// `out_tx`. M1.11 stubs completion after the first event so the pool can be
/// wired end-to-end before the real agentic loop arrives. Selected when
/// [`SubagentContext::api_client`] is `None`.
async fn run_subagent_stub(
    ctx: SubagentContext,
    mut event_rx: mpsc::Receiver<lingxi_core::Event>,
    out_tx: mpsc::Sender<SubagentEvent>,
) {
    use lingxi_core::types::SessionId;
    use lingxi_core::{reduce, ConversationState, SessionState};

    let agent_id = ctx.agent_id;

    // Seed initial state. The runner's local SessionState is transient —
    // the orchestrator (M5-02) owns durable session persistence. We use
    // SessionId::nil() and an inherited model string projected from the
    // agent definition; both are placeholders the reducer accepts.
    let model = match &ctx.agent_definition.model {
        crate::definition::AgentModel::Inherit => "inherit".to_string(),
        crate::definition::AgentModel::Alias(n) | crate::definition::AgentModel::Explicit(n) => {
            n.clone()
        }
    };
    let mut state = ConversationState::Idle {
        session: SessionState::empty(SessionId::nil(), model),
    };
    // Track whether at least one Message has been emitted — informs the
    // EOF branch's choice between Completed(graceful) and Failed.
    let mut produced_useful_work = false;

    while let Some(event) = event_rx.recv().await {
        // Fast path: explicit user-termination events bypass reason-string
        // inspection and surface as Killed directly. The reducer's reason
        // strings are an implementation detail; the input event itself is
        // authoritative for the Killed signal. UserInterrupt in particular
        // does NOT reach Terminated via the M1 reducer (catch-all), so
        // without this fast path it would never produce Killed.
        if matches!(
            &event,
            lingxi_core::Event::UserExit | lingxi_core::Event::UserInterrupt
        ) {
            // Drive the reducer anyway for state consistency, but ignore
            // the resulting reason.
            let (new_state, _effects) = reduce(state, event);
            state = new_state;
            let _ = out_tx.send(SubagentEvent::Killed { agent_id }).await;
            // Suppress the unused-assignment lint by referencing `state`.
            let _ = &state;
            return;
        }

        // Capture whether this event represents a stream-end completion
        // BEFORE the reducer consumes it — we need to peek at the
        // final_message for the Message emit.
        let api_end_msg = match &event {
            lingxi_core::Event::ApiStreamEnd { final_message, .. } => Some(final_message.clone()),
            _ => None,
        };

        let (new_state, _effects) = reduce(state, event);
        state = new_state;

        if let Some(msg) = api_end_msg {
            produced_useful_work = true;
            let _ = out_tx
                .send(SubagentEvent::Message {
                    agent_id,
                    message: serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null),
                })
                .await;
        }

        if state.is_terminal() {
            // Inspect the terminal reason; the M1 reducer puts it on
            // `Terminated { reason, .. }`. Killed prefixes per Task 1 step 3.
            if let ConversationState::Terminated { reason, .. } = &state {
                if reason.starts_with("user_exit")
                    || reason.starts_with("user_interrupt")
                    || reason.starts_with("killed")
                {
                    let _ = out_tx.send(SubagentEvent::Killed { agent_id }).await;
                } else {
                    let _ = out_tx
                        .send(SubagentEvent::Completed {
                            agent_id,
                            result: serde_json::json!({ "reason": reason }),
                            // Stub path makes no real round-trips: no usage / no
                            // tool-use count / no measured duration.
                            usage: llm_runtime::ExecutionUsage::default(),
                            total_tool_use_count: 0,
                            total_duration_ms: 0,
                            assistant_message_count: 0,
                            last_request_id: None,
                            cumulative_usage: llm_runtime::ExecutionUsage::default(),
                            usage_complete: true,
                        })
                        .await;
                }
            }
            return;
        }
    }

    // event_rx closed before reaching Terminated. If we already emitted a
    // Message (the Task 2 happy path), it's a graceful end — emit Completed
    // with a synthetic reason. Otherwise (no useful work done), Task 4 will
    // refine this to Failed.
    if produced_useful_work {
        let _ = out_tx
            .send(SubagentEvent::Completed {
                agent_id,
                result: serde_json::json!({ "reason": "eof_graceful" }),
                // Stub path makes no real round-trips.
                usage: llm_runtime::ExecutionUsage::default(),
                total_tool_use_count: 0,
                total_duration_ms: 0,
                assistant_message_count: 0,
                last_request_id: None,
                cumulative_usage: llm_runtime::ExecutionUsage::default(),
                usage_complete: true,
            })
            .await;
    } else {
        let _ = out_tx
            .send(SubagentEvent::Failed {
                agent_id,
                error: "run_subagent: event channel closed without terminal state".into(),
                cumulative_usage: llm_runtime::ExecutionUsage::default(),
            })
            .await;
    }
}

fn accumulate_usage(acc: &mut llm_runtime::ExecutionUsage, turn: &llm_runtime::ExecutionUsage) {
    let current = acc.counts();
    let next = turn.counts();
    let counts = acc.counts_mut();
    counts.input_tokens = current.input_tokens.saturating_add(next.input_tokens);
    counts.output_tokens = current.output_tokens.saturating_add(next.output_tokens);
    counts.cache_write_tokens = current
        .cache_write_tokens
        .saturating_add(next.cache_write_tokens);
    counts.cache_read_tokens = current
        .cache_read_tokens
        .saturating_add(next.cache_read_tokens);
    counts.cache_write_1h_tokens = current
        .cache_write_1h_tokens
        .saturating_add(next.cache_write_1h_tokens);
    counts.reasoning_tokens = current
        .reasoning_tokens
        .saturating_add(next.reasoning_tokens);
}

/// Group `messages` into atomic trim units: an assistant message whose
/// content is entirely (or partly) `ToolUse` blocks, immediately followed by
/// a user message whose content is entirely `ToolResult` blocks, is one unit
/// — every other message is its own unit. Every provider rejects a
/// `tool_result` with no matching `tool_use` in the same request (and vice
/// versa), so [`cap_input_bytes`] must never keep one half of such a pair.
fn tool_pair_units(
    messages: &[lingxi_core::types::ConversationMessage],
) -> Vec<&[lingxi_core::types::ConversationMessage]> {
    let mut units = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let is_tool_use_turn = matches!(
            &messages[i],
            lingxi_core::types::ConversationMessage::Assistant { content, .. }
                if content.iter().any(|b| matches!(b, lingxi_core::types::ContentBlock::ToolUse { .. }))
        );
        if is_tool_use_turn && i + 1 < messages.len() {
            let next_is_all_tool_result = matches!(
                &messages[i + 1],
                lingxi_core::types::ConversationMessage::User { content, .. }
                    if !content.is_empty()
                        && content.iter().all(|b| matches!(b, lingxi_core::types::ContentBlock::ToolResult { .. }))
            );
            if next_is_all_tool_result {
                units.push(&messages[i..=i + 1]);
                i += 2;
                continue;
            }
        }
        units.push(&messages[i..=i]);
        i += 1;
    }
    units
}

/// Cached measurements for the exact compact JSON representation of one
/// borrowed trimming unit. `payload_bytes` excludes the unit's `[` and `]`;
/// compact serde_json sequences can then be joined with one comma without
/// cloning or serializing a growing candidate on every fit check.
struct SerializedInputUnit {
    serialized_bytes: u64,
    payload_bytes: Option<u64>,
}

#[derive(Default)]
struct SerializedByteCounter {
    bytes: u64,
}

impl std::io::Write for SerializedByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let bytes = u64::try_from(buf.len())
            .map_err(|_| std::io::Error::other("serialized input length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| std::io::Error::other("serialized input length overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
std::thread_local! {
    /// Test-only observation point for unit serializations and bytes visited.
    /// Thread-local state keeps parallel module tests independent and compiles
    /// entirely out of production builds.
    static CAP_INPUT_MEASUREMENTS: std::cell::Cell<(usize, u64)> = const {
        std::cell::Cell::new((0, 0))
    };
}

#[cfg(test)]
fn reset_cap_input_measurements() {
    CAP_INPUT_MEASUREMENTS.set((0, 0));
}

#[cfg(test)]
fn cap_input_measurements() -> (usize, u64) {
    CAP_INPUT_MEASUREMENTS.get()
}

fn measure_serialized_input_unit(
    messages: &[lingxi_core::types::ConversationMessage],
) -> SerializedInputUnit {
    // A slice serializes as the same compact JSON array as the old flattened
    // Vec. Count the borrowed serialization directly instead of allocating a
    // temporary byte buffer. Keep the u64::MAX sentinel used by the old helper
    // for either a serde failure or a length overflow.
    let mut counter = SerializedByteCounter::default();
    let serialized_bytes = serde_json::to_writer(&mut counter, messages)
        .ok()
        .map(|()| counter.bytes);
    #[cfg(test)]
    CAP_INPUT_MEASUREMENTS.set({
        let (units, bytes) = CAP_INPUT_MEASUREMENTS.get();
        (
            units.saturating_add(1),
            bytes.saturating_add(serialized_bytes.unwrap_or_default()),
        )
    });
    let payload_bytes = serialized_bytes.and_then(|bytes| bytes.checked_sub(2));
    SerializedInputUnit {
        serialized_bytes: serialized_bytes.unwrap_or(u64::MAX),
        payload_bytes,
    }
}

/// Append one already-measured non-empty JSON array fragment to another
/// compact JSON array. The result is exactly the byte count of the flattened
/// candidate: the fragment contributes its contents and one inter-unit comma.
fn append_serialized_input_unit(current_bytes: u64, unit: &SerializedInputUnit) -> u64 {
    let Some(payload_bytes) = unit.payload_bytes else {
        return u64::MAX;
    };
    current_bytes
        .checked_add(payload_bytes)
        .and_then(|bytes| bytes.checked_add(1))
        .unwrap_or(u64::MAX)
}

fn cap_input_bytes(
    messages: &[lingxi_core::types::ConversationMessage],
    max_bytes: Option<u64>,
) -> Result<Vec<lingxi_core::types::ConversationMessage>, String> {
    let Some(max) = max_bytes else {
        return Ok(messages.to_vec());
    };
    let units = tool_pair_units(messages);
    if units.is_empty() {
        return Ok(Vec::new());
    };

    // The first seeded task/fork unit is mandatory: Fusion puts its entire
    // task text here and never re-injects it on later turns. Sending it whole
    // when over-cap violated the cap; dropping it silently violated task
    // semantics. Measure it first and reject without serializing any optional
    // history when it cannot fit.
    let head = units[0];
    let head_measurement = measure_serialized_input_unit(head);
    let head_bytes = head_measurement.serialized_bytes;
    if head_bytes > max {
        return Err(format!(
            "mandatory initial prompt exceeds max_input_bytes_per_turn ({head_bytes} > {max})"
        ));
    }

    // The newest unit is the current turn's continuation (usually a tool
    // result pair) and is also mandatory. Reject when preserving it together
    // with the seed would exceed the cap rather than silently sending stale,
    // incomplete history. Older units may be dropped as whole units.
    let mut selected_tail: Vec<&[lingxi_core::types::ConversationMessage]> = Vec::new();
    let mut selected_bytes = head_bytes;
    if let Some(newest) = units.get(1..).and_then(|tail| tail.last()).copied() {
        let newest_measurement = measure_serialized_input_unit(newest);
        let head_and_newest_bytes =
            append_serialized_input_unit(selected_bytes, &newest_measurement);
        if head_and_newest_bytes > max {
            let newest_bytes = newest_measurement.serialized_bytes;
            return Err(format!(
                "mandatory latest tool/message unit exceeds max_input_bytes_per_turn when combined with the initial prompt ({head_bytes} + {newest_bytes} > {max})"
            ));
        }
        selected_tail.push(newest);
        selected_bytes = head_and_newest_bytes;
    }

    // Fill from the newest older unit backwards, keeping each tool_use /
    // tool_result pair atomic. A unit that does not fit is dropped and the
    // search continues; no over-cap fallback is permitted. Each optional unit
    // is measured only when reached, once, in newest-to-oldest order.
    if units.len() > 2 {
        for unit in units[1..units.len() - 1].iter().rev().copied() {
            let measurement = measure_serialized_input_unit(unit);
            let candidate_bytes = append_serialized_input_unit(selected_bytes, &measurement);
            if candidate_bytes <= max {
                selected_tail.push(unit);
                selected_bytes = candidate_bytes;
            }
        }
    }

    selected_tail.reverse();
    let mut out =
        Vec::with_capacity(head.len() + selected_tail.iter().map(|unit| unit.len()).sum::<usize>());
    out.extend(head.iter().cloned());
    for unit in selected_tail {
        out.extend(unit.iter().cloned());
    }
    debug_assert!(
        selected_bytes <= max,
        "cap_input_bytes must never return an over-cap request"
    );
    Ok(out)
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod runner_test;

/// The typed `model_refusal_fallback` system message for a subagent hop.
///
/// `convert_messages` drops every `System` before the wire, so this rides in
/// the run's history and its transcript without becoming model context — which
/// is exactly where claude-code's `ICe` looks for it when the agent finalizes.
///
/// `scope` is `"local"`, not the main thread's `"session"`: a subagent's swap
/// lasts for this run only and does not touch the session model.
fn refusal_fallback_frame(
    id: MessageId,
    banner: &lingxi_core::host::refusal_notice::RefusalNotice,
) -> ConversationMessage {
    ConversationMessage::System {
        id,
        content: format!(
            "This model's safeguards flagged this message. Switched to {}.",
            banner.serving_model
        ),
        subtype: Some("model_refusal_fallback".to_string()),
        compact_metadata: None,
        refusal_fallback: Some(lingxi_core::types::RefusalFallbackMetadata {
            trigger: "refusal".to_string(),
            direction: "retry".to_string(),
            scope: Some("local".to_string()),
            original_model: banner.origin_model.clone(),
            fallback_model: banner.serving_model.clone(),
            request_id: banner.request_id.clone(),
            api_refusal_category: banner.api_refusal_category.clone(),
            retracted_message_uuids: banner.retracted_message_uuids.clone(),
            refused_user_message_uuid: banner.refused_user_message_uuid.clone(),
        }),
    }
}

/// The uuid prefix length `PZo` compares on (claude `D4n = 24`).
const RETRACTED_UUID_PREFIX: usize = 24;

/// `PZo` — drop the messages a refusal notice retracted.
///
/// A cascade that supersedes an earlier hop names the messages that hop
/// produced; replaying them would show the user work the session has already
/// moved past. System messages always survive: the notices themselves are how
/// the retraction is expressed.
fn drop_retracted(history: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let retracted: std::collections::HashSet<String> = history
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::System {
                subtype: Some(subtype),
                refusal_fallback: Some(meta),
                ..
            } if subtype == "model_refusal_fallback" => Some(&meta.retracted_message_uuids),
            _ => None,
        })
        .flatten()
        .map(|u| u.chars().take(RETRACTED_UUID_PREFIX).collect())
        .collect();
    if retracted.is_empty() {
        return history.to_vec();
    }
    let live: Vec<ConversationMessage> = history
        .iter()
        .filter(|m| {
            matches!(m, ConversationMessage::System { .. })
                || !retracted.contains(
                    &m.id()
                        .as_uuid()
                        .to_string()
                        .chars()
                        .take(RETRACTED_UUID_PREFIX)
                        .collect::<String>(),
                )
        })
        .cloned()
        .collect();
    if live.len() != history.len() {
        tracing::info!(
            event = "tengu_resume_retracted_dropped",
            dropped = history.len() - live.len(),
            chain_length = history.len(),
        );
    }
    live
}

/// `ICe`'s notice half — the `scope: "local"` refusal frame that explains the
/// model which actually produced this run's answer.
///
/// Upstream finds the last non-error assistant message with real text, reads
/// its `model`, and matches a frame whose `fallbackModel` equals it. This
/// port's `Assistant` carries no model, but the runner knows the serving model
/// outright — and that IS the model that produced the answer, because every hop
/// retries the turn. So the match is on the same value, not an approximation.
fn local_refusal_notice(live: &[ConversationMessage], serving_model: &str) -> Option<String> {
    live.iter().rev().find_map(|m| match m {
        ConversationMessage::System {
            subtype: Some(subtype),
            refusal_fallback: Some(meta),
            content,
            ..
        } if subtype == "model_refusal_fallback"
            && meta.scope.as_deref() == Some("local")
            && meta.fallback_model == serving_model =>
        {
            Some(content.clone())
        }
        _ => None,
    })
}
