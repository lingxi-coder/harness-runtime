//! Faithful port of claude-code's `StreamingToolExecutor`
//! (`services/tools/StreamingToolExecutor.ts`). Schedules tool execution as
//! `tool_use` blocks stream in, under concurrency control, buffering results
//! for emission in *received* order. Single-task: all tool futures borrow
//! `&ConversationOrchestrator` and are polled on one `FuturesUnordered`, so no
//! `'static`/spawn is required.

use crate::conversation::ConversationOrchestrator;
use futures::{stream::FuturesUnordered, StreamExt};
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use tool_api::tool_trait::ToolStaticContext;
use tool_api::ContextModifier;

/// claude-code `REJECT_MESSAGE` (utils/messages.ts:212). The user-interrupted
/// synthetic result is this BARE text (NOT `<tool_use_error>`-wrapped, unlike
/// sibling_error/streaming_fallback). The optional memoryCorrectionHint is gated
/// off by default in claude-code, so it is not appended.
const REJECT_MESSAGE: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";

/// Claude Code 2.1.246 `J6`: MCP calls aborted before a response must not be
/// normalized into the generic empty-result sentinel.
const MCP_INTERRUPTED_MESSAGE: &str = "The tool call was interrupted before a result was received. It may or may not have completed on the server — verify before assuming it succeeded, and retry if needed.";

/// Result of one `dispatch_tool_uses_tracked` call routed through the executor:
/// the single result block + the tool's injected messages + context modifiers.
type DispatchOutcome = Result<
    (
        ContentBlock,
        bool,
        Vec<(ConversationMessage, ToolUseId)>,
        Vec<ContextModifier>,
        Vec<hooks::events::PostToolBatchCall>,
    ),
    crate::error::OrchestratorError,
>;

/// Why a tracked tool is being cancelled (TS `getAbortReason`). v2.1.183's
/// `getAbortReason` returns exactly `"streaming_fallback"` (discarded) or
/// `"user_interrupted"` — there is NO sibling/parallel-error abort reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbortReason {
    /// Emitted when the user interrupts (ESC / new message) an in-flight or
    /// queued tool whose `interrupt_behavior()==Cancel` (TS `getAbortReason`
    /// 'user_interrupted'). The synthetic result is the bare `REJECT_MESSAGE`.
    UserInterrupted,
    StreamingFallback,
}

/// The `toolUseResult` claude persists alongside a synthetic abort block.
///
/// These do NOT mirror the block's model-facing content — claude uses two
/// SHORTER literals on the transcript line:
/// * `user_interrupted` → `vld` (2.1.220 BIN off **238097321**), used at the
///   synthetic site BIN off **232972524** — `"User rejected tool use"`, while
///   the model block carries the long `<tool_use_error>`-wrapped rejection.
/// * `streaming_fallback` → BIN off **232973166** —
///   `"Streaming fallback - tool execution discarded"`, i.e. the model block's
///   text with the `<tool_use_error>Error: ` wrapper stripped.
pub(crate) fn synthetic_tool_use_result(reason: AbortReason) -> serde_json::Value {
    serde_json::Value::String(
        match reason {
            AbortReason::UserInterrupted => "User rejected tool use",
            AbortReason::StreamingFallback => "Streaming fallback - tool execution discarded",
        }
        .to_string(),
    )
}

pub(crate) fn synthetic_tool_use_result_for_tool(
    reason: AbortReason,
    is_mcp: bool,
) -> serde_json::Value {
    if reason == AbortReason::UserInterrupted && is_mcp {
        return serde_json::Value::String(format!("Error: {MCP_INTERRUPTED_MESSAGE}"));
    }
    synthetic_tool_use_result(reason)
}

/// Build the synthetic `tool_result` for a cancelled tool (TS
/// `createSyntheticErrorMessage`). `provider_tool_use_id` is left `None` —
/// the caller copies the tracked tool's `provider_id` in before persisting.
#[cfg(test)]
pub(crate) fn synthetic_error_block(tool_use_id: ToolUseId, reason: AbortReason) -> ContentBlock {
    synthetic_error_block_for_tool(tool_use_id, reason, false)
}

fn synthetic_error_block_for_tool(
    tool_use_id: ToolUseId,
    reason: AbortReason,
    is_mcp: bool,
) -> ContentBlock {
    let content = match reason {
        AbortReason::StreamingFallback => {
            "<tool_use_error>Error: Streaming fallback - tool execution discarded</tool_use_error>"
                .to_string()
        }
        // claude-code (StreamingToolExecutor.ts:160-172) uses the BARE REJECT_MESSAGE
        // here — NOT `<tool_use_error>`-wrapped — with is_error: true. This is the
        // faithful text; the `UserInterrupted` reason itself is only produced once
        // the user-ESC / per-tool cancellation path is wired in a later sub-task.
        AbortReason::UserInterrupted if is_mcp => format!("Error: {MCP_INTERRUPTED_MESSAGE}"),
        AbortReason::UserInterrupted => REJECT_MESSAGE.to_string(),
    };
    ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error: true,
        provider_tool_use_id: None,
        content_blocks: None,
    }
}

/// Lifecycle of one tracked tool, mirroring TS `ToolStatus`.
#[allow(dead_code)] // variants wired in Tasks 5-11
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Queued,
    Executing,
    Completed,
    Yielded,
}

/// One `tool_use` block under management. `assistant_id` is the id of the
/// assistant message that requested this call — it becomes the JSONL
/// `parentUuid` of the result (TS `sourceToolAssistantUUID`).
#[allow(dead_code)] // fields wired in Tasks 5-11
pub(crate) struct TrackedTool {
    pub(crate) id: ToolUseId,
    pub(crate) name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) provider_id: Option<String>,
    pub(crate) assistant_id: MessageId,
    pub(crate) status: ToolStatus,
    pub(crate) is_concurrency_safe: bool,
    /// The result block once `Completed` (the unknown-tool case fills it
    /// synchronously at `add_tool` time).
    pub(crate) result: Option<ContentBlock>,
    /// A per-tool Pre/PostToolUse hook requested that the loop stop.
    pub(crate) prevent_continuation: bool,
    /// Tool-injected follow-up messages (SKILLEXEC.3) + context modifiers,
    /// threaded through unchanged from `dispatch_tool_uses_tracked`.
    pub(crate) injected: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) modifiers: Vec<ContextModifier>,
    /// This tool's resolved-call entry, deferred so the streaming driver can
    /// fire one `PostToolBatch` for the complete model-response batch.
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
}

// ============================================================================
// Task 7: concurrency gate + ordered process_queue
// ============================================================================

/// TS `canExecuteTool` (line 129-135): a tool may start if nothing is
/// executing, OR if both the candidate and every executing tool are
/// concurrency-safe.
///
/// `executing_safe_flags` is a slice of the `is_concurrency_safe` flags for
/// every tool currently in the `Executing` state.
pub(crate) fn can_execute(executing_safe_flags: &[bool], candidate_safe: bool) -> bool {
    executing_safe_flags.is_empty() || (candidate_safe && executing_safe_flags.iter().all(|&s| s))
}

/// Default maximum number of concurrency-safe tools to execute simultaneously.
/// Byte-locked to the v2.1.183 binary's `r1p` getter:
/// ```js
/// function r1p() {
///   let e = parseInt(process.env.LINGXI_MAX_TOOL_USE_CONCURRENCY || "", 10);
///   return e > 0 ? e : 10;
/// }
/// ```
pub(crate) const DEFAULT_MAX_TOOL_USE_CONCURRENCY: usize = 10;

/// Resolve the maximum number of concurrency-safe tools to run at once,
/// mirroring the binary's `r1p()`. Reads `LINGXI_MAX_TOOL_USE_CONCURRENCY`
/// and uses it only when it parses to a value `> 0`; otherwise the default of
/// [`DEFAULT_MAX_TOOL_USE_CONCURRENCY`] (10).
///
/// Injectable form for tests: [`max_tool_use_concurrency_from`].
pub(crate) fn max_tool_use_concurrency() -> usize {
    max_tool_use_concurrency_from(
        std::env::var("LINGXI_MAX_TOOL_USE_CONCURRENCY")
            .ok()
            .as_deref(),
    )
}

/// Pure resolver for [`max_tool_use_concurrency`] — `parseInt(v, 10) > 0 ? v : 10`.
/// `parseInt` semantics: leading numeric prefix is parsed (e.g. `"5x"` → 5),
/// non-numeric / absent / `<= 0` → the default.
pub(crate) fn max_tool_use_concurrency_from(raw: Option<&str>) -> usize {
    // Mirror JS `parseInt(s, 10)`: take the leading (optionally signed) integer
    // prefix. Anything else (NaN) falls through to the default.
    let parsed: Option<i64> = raw.and_then(|s| {
        let t = s.trim_start();
        let bytes = t.as_bytes();
        let mut end = 0;
        if matches!(bytes.first(), Some(b'+' | b'-')) {
            end = 1;
        }
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        // Need at least one digit after the optional sign.
        let has_digit = bytes[..end].iter().any(u8::is_ascii_digit);
        if has_digit {
            t[..end].parse::<i64>().ok()
        } else {
            None
        }
    });
    match parsed {
        Some(n) if n > 0 => n as usize,
        _ => DEFAULT_MAX_TOOL_USE_CONCURRENCY,
    }
}

// ============================================================================
// StreamingToolExecutor — Task 6: struct + new() + add_tool()
// ============================================================================

/// Manages the lifecycle of all `tool_use` blocks from one streaming assistant
/// turn. Mirrors TS `StreamingToolExecutor` (`StreamingToolExecutor.ts:76-124`
/// for `addTool`).
///
/// All fields are now read by the Task 8 dispatch/abort logic.
pub(crate) struct StreamingToolExecutor<'a> {
    orch: &'a ConversationOrchestrator,
    pub(crate) tools: Vec<TrackedTool>,
    /// Set when the turn is discarded (streaming fallback); all queued tools
    /// are cancelled with `AbortReason::StreamingFallback` (Task 8).
    discarded: bool,
    /// In-flight tool futures keyed by their index in `tools`. Polled on the
    /// current task (no spawn); each borrows `&'a orch`. `+ Send` so the whole
    /// executor (held across `.await` in the live streaming turn) stays `Send`,
    /// matching the `Send` turn future required by the handle traits.
    inflight: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = (usize, DispatchOutcome)> + Send + 'a>>,
    >,
    /// Parent cancellation token carrying cancellation into in-flight tools.
    /// Each dispatched tool receives a `child_token()` of this, threaded into
    /// its `ToolUseContext::cancel`. It fires when the turn is **discarded**
    /// (streaming fallback) and — being a child of `user_cancel` when one is
    /// supplied — when the **user interrupts**. On either, an in-flight tool
    /// observing the token returns early (a Bash SIGKILLs its subprocess via
    /// `kill_on_drop` and returns `Aborted`), whose real outcome `drain_one`
    /// then substitutes with the synthetic abort block.
    tool_abort: tokio_util::sync::CancellationToken,
    /// DEFERRED-3: the turn's USER-interrupt token (ESC / new message), mirroring
    /// claude-code's `toolUseContext.abortController` with reason 'interrupt'.
    /// `None` outside the live streaming turn (executor unit tests + the test-only
    /// `run_to_completion` path), so those are byte-identical to before. When
    /// `Some` and fired, `abort_reason_for` substitutes `UserInterrupted` for
    /// every tool whose `interrupt_behavior()==Cancel` (queued via
    /// `apply_abort_to_pending`, in-flight via `drain_one`). To also deliver the
    /// token into each in-flight tool's `ctx.cancel` (so a Cancel-behavior tool
    /// observes it and returns early), `tool_abort` is parented to this token
    /// in `new_with_user_cancel` — a child token fires when its parent fires.
    user_cancel: Option<tokio_util::sync::CancellationToken>,
}

impl<'a> StreamingToolExecutor<'a> {
    /// Construct a fresh executor borrowing the given orchestrator for the
    /// duration of the streaming turn.
    pub(crate) fn new(orch: &'a ConversationOrchestrator) -> Self {
        Self {
            orch,
            tools: Vec::new(),
            discarded: false,
            inflight: FuturesUnordered::new(),
            tool_abort: tokio_util::sync::CancellationToken::new(),
            user_cancel: None,
        }
    }

    /// DEFERRED-3: construct with the turn's USER-interrupt token (ESC / new
    /// message). `tool_abort` is made a CHILD of `user_cancel`, so firing the
    /// user token also cancels every per-tool child (an in-flight Cancel-behavior
    /// tool observes its `ctx.cancel` and returns early); `abort_reason_for` then
    /// substitutes the bare `REJECT_MESSAGE` for Cancel-behavior tools.
    /// Mirrors claude-code's `createChildAbortController` where the user abort
    /// lives on the parent `toolUseContext.abortController`.
    pub(crate) fn new_with_user_cancel(
        orch: &'a ConversationOrchestrator,
        user_cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        let tool_abort = user_cancel.child_token();
        Self {
            orch,
            tools: Vec::new(),
            discarded: false,
            inflight: FuturesUnordered::new(),
            tool_abort,
            user_cancel: Some(user_cancel),
        }
    }

    /// Register one `tool_use` block received from the stream.
    ///
    /// - If the tool is **not in the registry**, a `TrackedTool` already
    ///   `Completed` is pushed with an unknown-tool error block (short-circuit,
    ///   mirrors TS `addTool` lines 76-85).
    /// - If the tool **is known**, classify `is_concurrency_safe` via the tool's
    ///   own method and push a `Queued` entry (mirrors TS lines 86-124).
    pub(crate) fn add_tool(
        &mut self,
        id: ToolUseId,
        name: String,
        input: serde_json::Value,
        provider_id: Option<String>,
        assistant_id: MessageId,
    ) {
        match self.orch.find_tool_for_dispatch(&name) {
            None => {
                let suffix = unknown_tool_suffix_for(&name, self.orch);
                let block = synthetic_unknown_tool(id.clone(), &name, provider_id.clone(), &suffix);
                let post_tool_batch_calls =
                    vec![post_tool_batch_call_for_result(&id, &name, &input, &block)];
                self.tools.push(TrackedTool {
                    id,
                    name,
                    input,
                    provider_id,
                    assistant_id,
                    status: ToolStatus::Completed,
                    is_concurrency_safe: true,
                    result: Some(block),
                    prevent_continuation: false,
                    injected: Vec::new(),
                    modifiers: Vec::new(),
                    post_tool_batch_calls,
                });
            }
            Some(tool) => {
                let safe = crate::schema_validation::validate_tool_schema(tool.as_ref(), &input)
                    .is_ok()
                    && tool.is_concurrency_safe(&input);
                self.tools.push(TrackedTool {
                    id,
                    name,
                    input,
                    provider_id,
                    assistant_id,
                    status: ToolStatus::Queued,
                    is_concurrency_safe: safe,
                    result: None,
                    prevent_continuation: false,
                    injected: Vec::new(),
                    modifiers: Vec::new(),
                    post_tool_batch_calls: Vec::new(),
                });
            }
        }
    }

    /// TS `processQueue` (line 140-151): walk the queue IN ORDER; start each
    /// queued tool whose concurrency conditions are met; STOP at the first
    /// queued non-concurrency-safe tool that cannot start yet (preserves
    /// exclusive-tool ordering). After each start the executing set changes, so
    /// we re-evaluate from scratch.
    ///
    /// ## Concurrency cap (parity binary `r1p` / `i1p`)
    ///
    /// The v2.1.183 binary runs a contiguous concurrency-safe group through
    /// `i1p`, which merges the per-tool generators with a bounded window of
    /// `r1p()` (`LINGXI_MAX_TOOL_USE_CONCURRENCY`, default 10). So no more
    /// than N concurrency-safe tools execute simultaneously; the rest of the
    /// group waits for a slot. We enforce the same bound here: a queued safe
    /// tool may only start when the number of currently-`Executing`
    /// concurrency-safe tools is below the cap. When the cap is reached no
    /// further safe tool starts this pass (and an unsafe tool is barriered by
    /// the executing safe tools), so the queue stalls until a completion frees
    /// a slot — at which point the streaming loop re-invokes `process_queue`.
    // Index loop + per-pass rebuild are forced by the borrow checker: `start_tool`
    // takes `&mut self`, so we can't hold an iterator borrow over `self.tools`
    // across a start. N is small (tools per turn), so the rebuild is negligible.
    #[allow(clippy::needless_range_loop)]
    pub(crate) fn process_queue(&mut self) {
        // Resolve the cap once per call (env-driven; `r1p()`).
        let max_safe = max_tool_use_concurrency();
        loop {
            let executing_flags: Vec<bool> = self
                .tools
                .iter()
                .filter(|t| t.status == ToolStatus::Executing)
                .map(|t| t.is_concurrency_safe)
                .collect();
            // Count concurrency-safe tools already in flight (all executing
            // tools are safe whenever a safe candidate could start, but count
            // explicitly so the bound is correct regardless).
            let executing_safe_count = executing_flags.iter().filter(|&&s| s).count();

            let mut started_any = false;
            for i in 0..self.tools.len() {
                if self.tools[i].status != ToolStatus::Queued {
                    continue;
                }
                let safe = self.tools[i].is_concurrency_safe;
                // Concurrency cap: do not start an (N+1)th simultaneous safe
                // tool — keep it queued. An unsafe tool is unbounded (it runs
                // alone behind the barrier), so the cap applies only to safe.
                if safe && executing_safe_count >= max_safe {
                    // At the safe-concurrency ceiling: this safe tool waits.
                    // Keep scanning in case a later unsafe tool barriers, but it
                    // can't start either while safe tools execute — so the pass
                    // ends without starting anything once we hit the cap.
                    continue;
                }
                if can_execute(&executing_flags, safe) {
                    self.start_tool(i);
                    started_any = true;
                    break; // re-evaluate the executing set after each start
                } else if !safe {
                    // An exclusive (non-safe) tool can't start yet → barrier.
                    return;
                }
                // A safe tool that can't start (unsafe tool executing) —
                // keep scanning; TS continues the loop in this case.
            }
            if !started_any {
                return;
            }
        }
    }

    /// Dispatch tool `i` as an in-flight future on `self.inflight`. The future
    /// borrows `&'a orch` and is polled on the current task (no spawn). It
    /// routes through the existing per-tool pipeline
    /// [`crate::turn_loop::dispatch_tool_uses_tracked`] (one tool at a time) so
    /// the hook→permission→registry ordering stays byte-locked.
    fn start_tool(&mut self, i: usize) {
        self.tools[i].status = ToolStatus::Executing;
        let id = self.tools[i].id.clone();
        let name = self.tools[i].name.clone();
        let input = self.tools[i].input.clone();
        let provider_id = self.tools[i].provider_id.clone();
        let assistant_id = self.tools[i].assistant_id;
        let orch = self.orch;
        // Hand this tool a child of the executor's `tool_abort` token. When the
        // turn is discarded (or the user interrupts, via the parented
        // `user_cancel`), `tool_abort` fires and this child fires too — an
        // in-flight Bash kills its subprocess.
        let child = self.tool_abort.child_token();
        let fut = async move {
            let single = vec![(id, name, input, provider_id)];
            let outcome: DispatchOutcome =
                match crate::turn_loop::dispatch_tool_uses_tracked_deferred(
                    orch,
                    &single,
                    Some(child),
                    Some(assistant_id),
                )
                .await
                {
                    // `single` has one element, so `pop()` == the only result.
                    Ok(mut dispatched) => dispatched
                        .results
                        .pop()
                        .map(|block| {
                            (
                                block,
                                dispatched.prevent_continuation,
                                dispatched.injected_messages,
                                dispatched.context_modifiers,
                                dispatched.post_tool_batch_calls,
                            )
                        })
                        .ok_or_else(|| {
                            crate::error::OrchestratorError::StreamingProtocol(format!(
                                "dispatch returned empty for tool index {i}"
                            ))
                        }),
                    Err(e) => Err(e),
                };
            (i, outcome)
        };
        self.inflight.push(Box::pin(fut));
    }

    /// `true` when no tool futures are currently in flight. Accessor for the
    /// live loop (which cannot reach the private `inflight` field across module
    /// boundaries).
    pub(crate) fn inflight_is_empty(&self) -> bool {
        self.inflight.is_empty()
    }

    /// TS `getAbortReason` (StreamingToolExecutor.ts:210-231): why tool `i` should
    /// be cancelled, computed from PRIOR state. Precedence is faithful:
    /// `discarded` → `StreamingFallback`, then a fired USER-interrupt token →
    /// `UserInterrupted` ONLY when this tool's `interrupt_behavior()==Cancel`
    /// (Block-behavior tools are NOT interrupted; TS returns `null` for them).
    /// Unlike `discarded` — which applies to the whole batch — the user-interrupt
    /// branch is PER-TOOL, so it must be evaluated by index here rather than
    /// precomputed once.
    fn abort_reason_for(&self, i: usize) -> Option<AbortReason> {
        if self.discarded {
            return Some(AbortReason::StreamingFallback);
        }
        if let Some(token) = &self.user_cancel {
            if token.is_cancelled() {
                // TS `getToolInterruptBehavior`: default Block when the tool is not
                // in the registry; only `Cancel` tools are user-interrupted.
                let is_cancel = matches!(
                    self.orch
                        .tools
                        .find_by_name(&self.tools[i].name)
                        .map(|t| t.interrupt_behavior(&self.tools[i].input)),
                    Some(tool_api::tool_trait::InterruptBehavior::Cancel)
                );
                if is_cancel {
                    return Some(AbortReason::UserInterrupted);
                }
            }
        }
        None
    }

    /// Await one in-flight tool future and record its result. Returns the
    /// completed tool index, or `None` if no futures are in flight.
    /// (TS `executeTool`/`collectResults` completion path.)
    pub(crate) async fn drain_one(&mut self) -> Option<usize> {
        let (i, outcome) = self.inflight.next().await?;
        // Compute the abort reason from PRIOR state (discard / user-interrupt). A
        // cancelled in-flight tool's real outcome is discarded for the synthetic.
        let abort_reason = self.abort_reason_for(i);
        // Cancelled in-flight tool: discard its real outcome for the synthetic.
        if let Some(reason) = abort_reason {
            // A completed real MCP result may already have recorded `_meta`
            // and an end-turn request before the executor notices the abort.
            // The synthetic error is the result that survives, so neither may
            // leak onto its transcript line or terminate the turn.
            self.orch
                .clear_discarded_tool_result_metadata(&self.tools[i].id)
                .await;
            // Denial-kind housekeeping: the dispatch-site catch may already have
            // recorded a kind (`interrupted` for a `ToolError::Aborted`, or
            // `cancelled` from the pre-cancel guard) for the block we are about
            // to THROW AWAY. `take_tool_denial_kind` removes on read, so a stale
            // entry would both mis-stamp the synthetic and leak. claude-code's
            // `createSyntheticErrorMessage` (2.1.220 @232972360) attaches
            // `toolDenialKind:"user-rejected"` to the `user_interrupted`
            // synthetic (@232972524) and NO kind to `streaming_fallback` /
            // `conversation_ended`, so follow the block that actually survives.
            let is_mcp = self
                .orch
                .tools
                .find_by_name(&self.tools[i].name)
                .is_some_and(|tool| tool.is_mcp());
            match reason {
                AbortReason::UserInterrupted => {
                    self.orch
                        .record_tool_denial_kind(
                            &self.tools[i].id,
                            if is_mcp {
                                "interrupted"
                            } else {
                                "user-rejected"
                            },
                        )
                        .await;
                }
                AbortReason::StreamingFallback => {
                    self.orch.remove_tool_denial_kind(&self.tools[i].id).await;
                }
            }
            // O1: the discarded real outcome may already have recorded a
            // `toolUseResult`; the synthetic that survives carries claude's own
            // literal instead. `record_tool_use_result` overwrites, so this both
            // corrects the value and prevents a stale entry.
            self.orch
                .record_tool_use_result(
                    &self.tools[i].id,
                    synthetic_tool_use_result_for_tool(reason, is_mcp),
                )
                .await;
            let mut block =
                synthetic_error_block_for_tool(self.tools[i].id.clone(), reason, is_mcp);
            set_provider_id(&mut block, self.tools[i].provider_id.clone());
            let post_tool_batch_call = post_tool_batch_call_for_result(
                &self.tools[i].id,
                &self.tools[i].name,
                &self.tools[i].input,
                &block,
            );
            self.tools[i].post_tool_batch_calls = vec![post_tool_batch_call];
            self.tools[i].result = Some(block);
            // A cancelled tool yields ONLY the synthetic — its injected msgs/modifiers are dropped.
            self.tools[i].status = ToolStatus::Completed;
            return Some(i);
        }
        // Otherwise record the real outcome (existing handling).
        match outcome {
            Ok((mut block, prevent, injected, modifiers, post_tool_batch_calls)) => {
                // Copy the provider id onto the result for egress replay.
                set_provider_id(&mut block, self.tools[i].provider_id.clone());
                self.tools[i].result = Some(block);
                self.tools[i].prevent_continuation = prevent;
                self.tools[i].injected = injected;
                self.tools[i].modifiers = modifiers;
                self.tools[i].post_tool_batch_calls = post_tool_batch_calls;
                self.tools[i].status = ToolStatus::Completed;
            }
            Err(e) => {
                // A hard orchestrator error → surface as an errored result,
                // matching claude-code's outer plumbing catch
                // (toolExecution.ts:471-480): `Error calling tool (<name>): <msg>`
                // wrapped in `<tool_use_error>`.
                let name = &self.tools[i].name;
                let block = ContentBlock::ToolResult {
                    tool_use_id: self.tools[i].id.clone(),
                    content: format!(
                        "<tool_use_error>Error calling tool ({name}): {e}</tool_use_error>"
                    ),
                    is_error: true,
                    provider_tool_use_id: self.tools[i].provider_id.clone(),
                    content_blocks: None,
                };
                let post_tool_batch_call = post_tool_batch_call_for_result(
                    &self.tools[i].id,
                    &self.tools[i].name,
                    &self.tools[i].input,
                    &block,
                );
                self.tools[i].post_tool_batch_calls = vec![post_tool_batch_call];
                self.tools[i].result = Some(block);
                self.tools[i].status = ToolStatus::Completed;
            }
        }
        Some(i)
    }

    /// Convert still-`Queued` tools to a synthetic-cancel result once
    /// `discarded` is set / the user interrupts (TS `getAbortReason` on next
    /// poll). This handles ONLY the `Queued` siblings; in-flight (`Executing`)
    /// siblings are substituted with the synthetic in [`Self::drain_one`] on
    /// their next completion (mirroring `collectResults` 335-345). The two are
    /// complementary: queued tools never enter `inflight`, so `drain_one` never
    /// sees them, and an executing tool is never `Queued` here.
    ///
    /// RETURNS every substituted id WITH its reason, so the async caller can
    /// stamp the transcript side-tables:
    /// * [`AbortReason::UserInterrupted`] ⇒ `toolDenialKind:"user-rejected"` —
    ///   the synthetic claude-code builds here (`createSyntheticErrorMessage`,
    ///   2.1.220 @232972360) carries it (@232972524) exactly as the in-flight
    ///   substitution in [`Self::drain_one`] does. `StreamingFallback` carries
    ///   NO kind, and a queued tool never reached dispatch, so there is no
    ///   prior entry to clear.
    /// * BOTH reasons ⇒ a `toolUseResult` literal
    ///   ([`synthetic_tool_use_result`]).
    ///
    /// Recording cannot happen inline because this method is sync and the
    /// recorders take the orchestrator's async mutexes.
    ///
    /// NOT `#[must_use]`: `streaming_executor_test.rs` calls this as a bare
    /// statement to assert only the synthetic block, and that file is owned by
    /// a concurrent session.
    pub(crate) fn apply_abort_to_pending(&mut self) -> Vec<(ToolUseId, AbortReason, bool)> {
        let mut substituted = Vec::new();
        for i in 0..self.tools.len() {
            if !matches!(self.tools[i].status, ToolStatus::Queued) || self.tools[i].result.is_some()
            {
                continue;
            }
            // Per-tool: the user-interrupt branch in `abort_reason_for` gates on
            // `interrupt_behavior()`, so a Queued Block-behavior tool under a pure
            // user-interrupt gets `None` here and still runs (faithful: Block tools
            // are not interrupted). `discarded` applies to all.
            let Some(reason) = self.abort_reason_for(i) else {
                continue;
            };
            let is_mcp = self
                .orch
                .tools
                .find_by_name(&self.tools[i].name)
                .is_some_and(|tool| tool.is_mcp());
            substituted.push((self.tools[i].id.clone(), reason, is_mcp));
            let mut block =
                synthetic_error_block_for_tool(self.tools[i].id.clone(), reason, is_mcp);
            set_provider_id(&mut block, self.tools[i].provider_id.clone());
            let post_tool_batch_call = post_tool_batch_call_for_result(
                &self.tools[i].id,
                &self.tools[i].name,
                &self.tools[i].input,
                &block,
            );
            self.tools[i].post_tool_batch_calls = vec![post_tool_batch_call];
            self.tools[i].result = Some(block);
            self.tools[i].status = ToolStatus::Completed;
        }
        substituted
    }

    /// Mark the turn discarded (streaming fallback). Pending tools get a
    /// `StreamingFallback` synthetic result on the next `apply_abort_to_pending`.
    // Still only exercised by the Task-11 fallback test path / Phase 2; the live
    // loop does not yet discard.
    #[allow(dead_code)]
    fn discard(&mut self) {
        self.discarded = true;
        // Streaming-fallback also aborts in-flight work — fire `tool_abort` so
        // any in-flight Bash kills its subprocess.
        self.tool_abort.cancel();
    }

    #[cfg(test)]
    pub(crate) async fn run_to_completion(
        &mut self,
    ) -> Result<Vec<ContentBlock>, crate::error::OrchestratorError> {
        loop {
            for (id, reason, is_mcp) in self.apply_abort_to_pending() {
                if reason == AbortReason::UserInterrupted {
                    self.orch
                        .record_tool_denial_kind(
                            &id,
                            if is_mcp {
                                "interrupted"
                            } else {
                                "user-rejected"
                            },
                        )
                        .await;
                }
                self.orch
                    .record_tool_use_result(&id, synthetic_tool_use_result_for_tool(reason, is_mcp))
                    .await;
            }
            self.process_queue();
            if self.inflight.is_empty() {
                break;
            }
            self.drain_one().await;
        }
        Ok(self
            .tools
            .iter()
            .map(|t| t.result.clone().expect("completed"))
            .collect())
    }
}

/// Short `Name(arg…)` description for a tool (TS `getToolDescription`,
/// StreamingToolExecutor.ts:243-252). No longer consumed in production now that
/// the sibling-error cascade is gone, but kept as the faithful API twin.
#[allow(dead_code)]
fn tool_description(t: &TrackedTool) -> String {
    let summary = t
        .input
        .get("command")
        .or_else(|| t.input.get("file_path"))
        .or_else(|| t.input.get("pattern"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if summary.is_empty() {
        t.name.clone()
    } else {
        let truncated = if summary.chars().count() > 40 {
            format!("{}\u{2026}", summary.chars().take(40).collect::<String>())
        } else {
            summary.to_owned()
        };
        format!("{}({})", t.name, truncated)
    }
}

/// Copy a provider-issued tool-call id onto a `ToolResult` block's
/// `provider_tool_use_id` for egress replay; no-op for non-`ToolResult` blocks.
fn set_provider_id(block: &mut ContentBlock, provider_id: Option<String>) {
    if let ContentBlock::ToolResult {
        provider_tool_use_id,
        ..
    } = block
    {
        *provider_tool_use_id = provider_id;
    }
}

fn post_tool_batch_call_for_result(
    id: &ToolUseId,
    name: &str,
    input: &serde_json::Value,
    block: &ContentBlock,
) -> hooks::events::PostToolBatchCall {
    let tool_response = match block {
        ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } => Some(content_blocks.as_ref().map_or_else(
            || serde_json::Value::String(content.clone()),
            |blocks| {
                serde_json::to_value(blocks)
                    .unwrap_or_else(|_| serde_json::Value::String(content.clone()))
            },
        )),
        _ => None,
    };
    hooks::events::PostToolBatchCall {
        tool_name: name.to_string(),
        tool_input: input.clone(),
        tool_use_id: id.clone(),
        tool_response,
    }
}

// ============================================================================
// Task 9: ordered result drain (TS getCompletedResults / hasUnfinishedTools)
// ============================================================================

/// One drained result ready for the live loop to persist, carrying everything
/// needed to build the per-result user message (TS `getCompletedResults` yields
/// one message per result). The live loop parents every result to the single
/// per-turn assistant via that assistant's captured JSONL uuid (TS
/// `sourceToolAssistantUUID`), so no per-result assistant id is carried here.
pub(crate) struct DrainedResult {
    pub(crate) tool: String,
    pub(crate) block: ContentBlock,
    pub(crate) prevent_continuation: bool,
    pub(crate) injected: Vec<(ConversationMessage, ToolUseId)>,
    pub(crate) modifiers: Vec<ContextModifier>,
    pub(crate) post_tool_batch_calls: Vec<hooks::events::PostToolBatchCall>,
}

impl<'a> StreamingToolExecutor<'a> {
    /// TS `getCompletedResults`: walk tools in order, yield each newly-`Completed`
    /// tool's result (marking it `Yielded`), and STOP at an `Executing`
    /// non-concurrency-safe tool (don't emit past an unfinished exclusive
    /// barrier). Returns results in RECEIVED order.
    pub(crate) fn take_newly_completed(&mut self) -> Vec<DrainedResult> {
        let mut out = Vec::new();
        for t in &mut self.tools {
            match t.status {
                ToolStatus::Completed => {
                    t.status = ToolStatus::Yielded;
                    out.push(DrainedResult {
                        tool: t.name.clone(),
                        block: t.result.clone().expect("completed tool has result"),
                        prevent_continuation: t.prevent_continuation,
                        injected: std::mem::take(&mut t.injected),
                        modifiers: std::mem::take(&mut t.modifiers),
                        post_tool_batch_calls: std::mem::take(&mut t.post_tool_batch_calls),
                    });
                }
                ToolStatus::Yielded => continue,
                ToolStatus::Executing if !t.is_concurrency_safe => break,
                _ => {}
            }
        }
        out
    }

    /// TS `hasUnfinishedTools`: any tool not yet `Yielded`. The live loop drives
    /// on `inflight_is_empty` instead (guaranteed-progress shape), so this is
    /// exercised by the executor's tests; kept as the faithful API twin.
    #[allow(dead_code)]
    pub(crate) fn has_unfinished(&self) -> bool {
        self.tools.iter().any(|t| t.status != ToolStatus::Yielded)
    }
}

/// Build the synthetic `tool_result` for an unknown tool (claude-code `addTool`
/// line 78-84 / `toolExecution.ts:401`). Shared by the streaming executor and
/// the batched dispatch (`turn_loop`) so this parity-critical string lives in
/// exactly one place.
pub(crate) fn synthetic_unknown_tool(
    id: ToolUseId,
    name: &str,
    provider_id: Option<String>,
    suffix: &str,
) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id,
        content: format!(
            "<tool_use_error>Error: No such tool available: {name}{suffix}</tool_use_error>"
        ),
        is_error: true,
        provider_tool_use_id: provider_id,
        content_blocks: None,
    }
}

/// Claude Code 2.1.263 `Ldt` suffix after `No such tool available: ${name}`.
/// Genuinely-unknown tools stay empty. Mapped arms that have substrate here:
/// subagent-restricted `d1e`/`ct("external")`, coordinator `Y7e`, catalog
/// disabled, Glob/Grep-via-shell (`Nte` + `qe`), pending-MCP `l5o`, MCP
/// disconnected (`a5o` `disconnected`).
///
/// WebFetch→Artifact is a carve-out. The WebFetch→`web-fetch` agent redirect
/// needs the live roster (`pq`); that agent is gated off by default (`xgi()`),
/// so a catalog-hidden WebFetch falls through to the disabled arm.
///
/// `d1e` names resolved from 2.1.263 `src_160256736.js` `ct("external")`.
/// `ltr` (spread into `ct`) is not fully named here; the listed tools are
/// the resolved scalars. LingXi is the external user type, so `Workflow`
/// is included.
const SUBAGENT_RESTRICTED_TOOLS: &[&str] = &[
    "TaskOutput",
    "ExitPlanMode",
    "EnterPlanMode",
    "AskUserQuestion",
    "Poll",
    "ConnectGitHub",
    "propose_skills",
    "WaitForMcpServers",
    "RefreshMcpTools",
    "Workflow",
    "ScheduleWakeup",
    "ReadNotifications",
    "ProposeGoal",
    "EndConversation",
];

/// 2.1.263 `qbt`: base coordinator tools AND worker-redirect exclusions.
const COORDINATOR_REDIRECT_EXCLUSIONS: &[&str] = &[
    "Agent",
    "TaskStop",
    "SendMessage",
    "StructuredOutput",
    "Skill",
    "ReadNotifications",
    "ListAgents",
    "Workflow",
];

#[must_use]
pub(crate) fn is_coordinator_redirect_excluded(name: &str) -> bool {
    COORDINATOR_REDIRECT_EXCLUSIONS.contains(&name)
}

#[must_use]
pub(crate) fn unknown_tool_suffix_for(name: &str, orch: &ConversationOrchestrator) -> String {
    let src = crate::config::sanitize_query_source(&orch.config.query_source);
    let is_subagent = src.starts_with("agent") || src == "subagent";
    let may_redirect = orch.is_coordinator_session()
        && orch.find_dispatchable_tool("Agent").is_some()
        && orch
            .tools
            .find_registered(name)
            .is_some_and(|tool| !orch.is_tool_pool_denied(tool.as_ref()));
    unknown_tool_suffix(name, &orch.tools, is_subagent, may_redirect)
}

#[must_use]
pub(crate) fn unknown_tool_suffix(
    name: &str,
    tools: &tool_api::registry::ToolRegistry,
    is_subagent: bool,
    is_coordinator: bool,
) -> String {
    let registered = tools.find_registered(name);
    let canonical = registered.as_ref().map(|tool| tool.name()).unwrap_or(name);
    if is_subagent && SUBAGENT_RESTRICTED_TOOLS.contains(&canonical) {
        return format!(
            ". {name} is not available inside subagents. Complete the task with the tools provided and return findings to the orchestrator."
        );
    }
    if registered.is_some() && canonical == "SendUserMessage" {
        return format!(". {name} is not enabled in this session \u{2014} write your message as normal assistant text instead.");
    }
    let in_catalog = registered.is_some();
    // 2.1.263 `dt("external")` / Y7e. A catalog entry is not necessarily
    // available to a worker (notably AskUserQuestion and ExitPlanMode).
    const WORKER_TOOLS: &[&str] = &[
        "Read",
        "WebSearch",
        "TodoWrite",
        "Grep",
        "WebFetch",
        "Glob",
        "Bash",
        "PowerShell",
        "Edit",
        "Write",
        "NotebookEdit",
        "Skill",
        "StructuredOutput",
        "ToolSearch",
        "EnterWorktree",
        "ExitWorktree",
        "REPL",
        "Monitor",
        "TaskStop",
        "GetTask",
        "SendMessage",
        "Artifact",
        "SearchPlugins",
        "SearchSkills",
        "ListPlugins",
        "ListSkills",
    ];
    let worker_available = tools.find_by_name(name).is_some_and(|tool| {
        tool.is_enabled(&tool_api::tool_trait::ToolStaticContext {
            main_loop_model: tools.main_loop_model(),
            ..Default::default()
        }) && std::iter::once(tool.name())
            .chain(tool.underlying_v1_tool_name())
            .chain(tool.family_parent_tool_name())
            .any(|name| WORKER_TOOLS.contains(&name))
    });
    if is_coordinator
        && !is_subagent
        && in_catalog
        && tools.find_by_name("Agent").is_some()
        && !is_coordinator_redirect_excluded(canonical)
        && worker_available
    {
        return format!(
            ". {name} is not available to you as the coordinator \u{2014} run it from a worker via the Agent tool instead."
        );
    }
    if in_catalog {
        return format!(". {name} is disabled for this session, in subagents as well as here.");
    }
    let shell = if tools.find_by_name("Bash").is_some() {
        "Bash"
    } else if tools.find_by_name("Shell").is_some() {
        "Shell"
    } else {
        ""
    };
    if name.eq_ignore_ascii_case("Glob") || name.eq_ignore_ascii_case("Grep") {
        if shell.is_empty() {
            return format!(". {name} is disabled for this session.");
        }
        return if name.eq_ignore_ascii_case("Glob") {
            format!(
                ". {name} is not available in this session \u{2014} find files with `find` via the {shell} tool instead."
            )
        } else {
            format!(
                ". {name} is not available in this session \u{2014} search file contents with `grep` via the {shell} tool instead."
            )
        };
    }
    if let Some(server) = mcp_server_from_tool_name(name) {
        // `l5o` (non-subagent): WaitForMcpServers is advertised only while an
        // MCP client is `pending`. `is_enabled` is that same pending mirror.
        if !is_subagent
            && tools
                .find_by_name("WaitForMcpServers")
                .is_some_and(|t| t.is_enabled(&ToolStaticContext::default()))
        {
            return format!(
                ". The MCP server '{server}' is still connecting. Call WaitForMcpServers to wait for it, then try again."
            );
        }
        // `a5o` `disconnected`. Subagent copy is "not available in this
        // context" (reconnecting uses "not connected").
        return if is_subagent {
            format!(
                ". Its MCP server '{server}' is not available in this context. Continue without this tool."
            )
        } else {
            format!(
                ". Its MCP server '{server}' has disconnected. Continue without this tool; it becomes callable again only if the server reconnects."
            )
        };
    }
    String::new()
}

fn mcp_server_from_tool_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("mcp__")?;
    let server = rest.split("__").next()?;
    (!server.is_empty()).then_some(server)
}

#[cfg(test)]
#[path = "streaming_executor_test.rs"]
mod streaming_executor_test;

/// O4-A step 3: denial-kind housekeeping across the executor's synthetic
/// substitution.
///
/// The dispatch-site catch (`turn_loop.rs`) records `"interrupted"` for a tool
/// that returned `ToolError::Aborted`. When the executor then DISCARDS that
/// real outcome for a synthetic (claude-code `createSyntheticErrorMessage`,
/// 2.1.220 @232972360), the persisted block is no longer the interrupted one,
/// so the recorded kind must follow the block that actually survives:
///   * `user_interrupted` ⇒ `toolDenialKind:"user-rejected"` (@232972524)
///   * `streaming_fallback` / `conversation_ended` ⇒ NO `toolDenialKind`
/// Leaving the dispatch-site entry in place would both stamp the wrong kind on
/// the synthetic and orphan the map entry (`take_tool_denial_kind` removes on
/// read, and the interrupted block never reaches persistence).
#[cfg(test)]
mod synthetic_denial_kind_tests {
    use super::*;
    use crate::conversation::ConversationOrchestrator;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use crate::OrchestratorConfig;
    use async_trait::async_trait;
    use lingxi_core::types::MessageId;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::registry::ToolRegistry;
    use tool_api::tool_trait::{
        DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
        ToolStaticContext, ValidationError,
    };

    /// Cancel-behavior tool that returns `ToolError::Aborted` the moment its
    /// `ctx.cancel` fires — the same shape as the real Bash/MCP abort seams.
    struct CancelTool;

    #[async_trait]
    impl Tool for CancelTool {
        fn name(&self) -> &str {
            "CancelTool"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
            InterruptBehavior::Cancel
        }
        async fn validate_input(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "cancel-tool".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            let token = ctx.cancel.clone();
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    Ok(ToolCallResult {
                        data: json!({ "content": "ran-to-end" }),
                        model_content: None,
                        new_messages: vec![],
                        context_modifier: None,
                        is_error: false,
                        mcp_meta: None,
                    })
                }
                () = async { match token { Some(t) => t.cancelled().await, None => std::future::pending().await } } => {
                    Err(ToolError::Aborted)
                }
            }
        }
    }

    fn orch() -> ConversationOrchestrator {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancelTool) as Arc<dyn Tool>);
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
    }

    #[tokio::test]
    async fn user_interrupt_substitution_rewrites_the_kind_to_user_rejected() {
        let orch = orch();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let id = ToolUseId::new();
        let mut exec = StreamingToolExecutor::new_with_user_cancel(&orch, user_cancel.clone());
        exec.add_tool(id.clone(), "CancelTool".into(), json!({}), None, a);
        exec.process_queue();
        user_cancel.cancel();
        let results = exec.run_to_completion().await.unwrap();
        let ContentBlock::ToolResult { content, .. } = &results[0] else {
            panic!("expected a tool_result")
        };
        assert_eq!(content, REJECT_MESSAGE, "synthetic must survive");
        assert_eq!(
            orch.transcript
                .tool_denial_kinds
                .lock()
                .await
                .get(&id.to_string())
                .map(String::as_str),
            Some("user-rejected"),
            "the substituted synthetic carries `user-rejected`, not the \
             dispatch-site `interrupted`"
        );
    }

    #[tokio::test]
    async fn streaming_fallback_substitution_clears_the_kind() {
        let orch = orch();
        let a = MessageId::new();
        let id = ToolUseId::new();
        let mut exec = StreamingToolExecutor::new(&orch);
        exec.add_tool(id.clone(), "CancelTool".into(), json!({}), None, a);
        exec.process_queue();
        exec.discard();
        let _ = exec.run_to_completion().await.unwrap();
        assert!(
            !orch
                .transcript
                .tool_denial_kinds
                .lock()
                .await
                .contains_key(&id.to_string()),
            "a streaming-fallback synthetic carries NO toolDenialKind"
        );
    }

    /// A tool cancelled while still QUEUED never reaches dispatch, so nothing
    /// records a kind there — but `apply_abort_to_pending` still persists the
    /// very same `user_interrupted` synthetic that `drain_one` produces, and
    /// claude-code's `createSyntheticErrorMessage` (2.1.220 @232972360) stamps
    /// `toolDenialKind:"user-rejected"` (@232972524) on that message regardless
    /// of whether the tool had started. Without this the persisted JSONL line
    /// silently loses the stamp on the queued path while keeping it on the
    /// in-flight path.
    #[tokio::test]
    async fn queued_tool_cancelled_before_dispatch_is_stamped_user_rejected() {
        let orch = orch();
        let user_cancel = tokio_util::sync::CancellationToken::new();
        let a = MessageId::new();
        let id = ToolUseId::new();
        let mut exec = StreamingToolExecutor::new_with_user_cancel(&orch, user_cancel.clone());
        exec.add_tool(id.clone(), "CancelTool".into(), json!({}), None, a);
        // Cancel while the tool is still Queued — `process_queue` never runs it,
        // so `apply_abort_to_pending` is the path that produces the synthetic.
        user_cancel.cancel();
        let results = exec.run_to_completion().await.unwrap();
        let ContentBlock::ToolResult { content, .. } = &results[0] else {
            panic!("expected a tool_result")
        };
        assert_eq!(content, REJECT_MESSAGE, "queued tool gets the synthetic");
        assert_eq!(
            orch.transcript
                .tool_denial_kinds
                .lock()
                .await
                .get(&id.to_string())
                .map(String::as_str),
            Some("user-rejected"),
            "the queued-path synthetic must carry the same kind as the \
             in-flight-path synthetic"
        );
    }
}
