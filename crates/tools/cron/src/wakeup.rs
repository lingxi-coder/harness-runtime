//! `ScheduleWakeup` tool — `/loop` dynamic (self-pace) mode.
//!
//! PARITY: Claude Code 2.1.270 — definition `ryr`, schemas `qvs` / `Vvs`,
//! prompt `bgr`, description `wgr`; runtime `hpr` (schedule), `ypr`
//! (keepalive), `_pr` (stop), `tXt` (user abort). The executable oracle
//! in `tests/oracle` locks the native artifact hash and observable outputs.
//! Dynamic mode has no feature gate: model calls schedule (or age out),
//! and `stop: true` ends the loop.
//!
//! Runtime layering: this tool lives in a LOW crate (`tool-cron`) and cannot
//! reach the per-connection message queue (owned at the bridge composition
//! root). Firing is mediated by the [`WakeupScheduler`] seam: an injected
//! `Arc<dyn WakeupScheduler>` whose real impl lives at the composition root and
//! does `RuntimeSpawner::sleep(delay) → resolve sentinel → enqueue`, and which
//! can cancel its pending wakeups (the binary's `kind:"loop"` cron registry).
//! When no scheduler is wired the tool reports the zero triple (the binary's
//! `aKi === null` branch) instead of promising a wakeup that cannot fire.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{COMPLETED, FAILED, STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    CoercedInput, DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult,
    ToolError, ToolStaticContext, ValidationError,
};

use crate::autonomous_loop::{self as al, DynamicLoopRecord};

/// Tool name byte-lock (binary `Xi`).
pub const SCHEDULE_WAKEUP_TOOL_NAME: &str = "ScheduleWakeup";

/// Lower clamp bound for `delaySeconds` (binary `_=60`).
pub const MIN_DELAY_SECONDS: i64 = 60;
/// Upper clamp bound for `delaySeconds` (binary `b=3600`).
pub const MAX_DELAY_SECONDS: i64 = 3600;

/// Sentinel the model passes as `prompt` for an autonomous `/loop` (no user
/// prompt). ScheduleWakeup ALWAYS uses the `-dynamic` variant; the sibling
/// CronCreate-mode sentinel `<<autonomous-loop>>` is distinct.
pub use crate::autonomous_loop::AUTONOMOUS_LOOP_DYNAMIC_SENTINEL;

/// One-shot self-wakeup scheduling seam (twin of the orchestrator's
/// `MidTurnInputSource`). The real impl lives at the bridge composition root,
/// where the per-connection `MessageQueueManager` + `RuntimeSpawner` are owned.
///
/// Implementations schedule a single delayed re-injection of `prompt` after
/// `delay`, applying [`resolve_wakeup_prompt`] to the prompt first (so the
/// `<<autonomous-loop-dynamic>>` sentinel expands at fire time). `reason` is
/// carried for telemetry / user surfacing.
#[async_trait]
pub trait WakeupScheduler: Send + Sync {
    /// Schedule a one-shot self-wakeup. `delay` is already clamped to
    /// `[MIN_DELAY_SECONDS, MAX_DELAY_SECONDS]` and minute-aligned by the tool.
    async fn schedule(&self, delay: Duration, prompt: String, reason: String);

    /// Cancel every wakeup still pending (the binary's `HL(loopCronIds)`),
    /// returning the PROMPT of each wakeup that was cancelled.
    ///
    /// The prompts — not just the count — are what `ZXn` / `t3t` feed to
    /// `Ort(prompt)` to forget those loops' chain-start records. Hosts that
    /// cannot cancel return an empty vec.
    async fn cancel_pending(&self) -> Vec<String> {
        Vec::new()
    }

    /// Return the session-scoped loop state owned by this scheduler's host.
    /// Hosts without a session object keep the legacy process-local fallback.
    fn loop_runtime(&self) -> Option<Arc<al::LoopRuntime>> {
        None
    }
}

/// Shared, set-once handle to the live [`WakeupScheduler`].
///
/// The `ScheduleWakeupTool` is constructed deep inside `harness_runtime::desktop::build`
/// (via `tool_cron::register_all_with_auth`), BEFORE the per-connection
/// `MessageQueueManager` + `RuntimeSpawner` exist at `boot::assemble`. So the
/// tool holds an empty cell whose clone is surfaced on `DesktopRuntime`; the
/// bridge composition root fills it (`cell.set(scheduler)`) once those inputs
/// are available. Hosts that own no per-connection queue leave it empty.
pub type WakeupSchedulerCell = Arc<std::sync::OnceLock<Arc<dyn WakeupScheduler>>>;

/// JS `Math.round` (half rounds toward +∞), saturating into `i64`.
fn js_round(raw: f64) -> i64 {
    (raw + 0.5).floor() as i64
}

/// Binary `F(e)` rounding step: `NaN → 60`, `+∞ → 3600`, `−∞ → 60`, else
/// `Math.round(e)`. Not yet clamped.
fn requested_delay_seconds(raw: f64) -> i64 {
    if raw.is_nan() {
        MIN_DELAY_SECONDS
    } else if raw == f64::INFINITY {
        MAX_DELAY_SECONDS
    } else if raw == f64::NEG_INFINITY {
        MIN_DELAY_SECONDS
    } else {
        js_round(raw)
    }
}

/// Clamp `delaySeconds` to `[MIN_DELAY_SECONDS, MAX_DELAY_SECONDS]` after the
/// binary's rounding step (`F(e)`: `Math.max(_, Math.min(b, o))`).
#[must_use]
pub fn clamp_delay_seconds(raw: f64) -> i64 {
    requested_delay_seconds(raw).clamp(MIN_DELAY_SECONDS, MAX_DELAY_SECONDS)
}

/// Binary `F(e)`: `wasClamped = !Number.isFinite(e) || o !== t`.
#[must_use]
pub fn delay_was_clamped(raw: f64) -> bool {
    !raw.is_finite() || requested_delay_seconds(raw) != clamp_delay_seconds(raw)
}

/// The scheduler's five-minute cache window (`Xbt = 300000`).
const CACHE_TTL_MS: i64 = 300_000;
/// `mN.cacheLeadMs` — how far ahead of the cache cliff the wakeup is pulled.
const CACHE_LEAD_MS: i64 = 15_000;
/// `mN.recurringMaxAgeMs` — a dynamic loop ends 7 days after its first wakeup.
const LOOP_MAX_AGE_MS: i64 = 604_800_000;
/// Binary `S` restart check: a loop whose last wakeup was due more than an hour
/// ago is a NEW loop for aging purposes (`r > d.lastScheduledFor + b*1000`).
const LOOP_RESTART_GAP_MS: i64 = MAX_DELAY_SECONDS * 1000;

/// Binary `F(e)` — the resolved wakeup timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeupTarget {
    /// `clamped` — seconds after clamping.
    pub clamped_delay_seconds: i64,
    /// `wasClamped`.
    pub was_clamped: bool,
    /// `targetMs` — the whole-minute epoch ms the wakeup is scheduled for.
    pub target_ms: i64,
}

/// Binary `P(e)`: round an epoch-ms instant UP to the next whole minute.
fn ceil_to_minute_ms(ms: i64) -> i64 {
    let minute = 60_000;
    let rem = ms.rem_euclid(minute);
    if rem == 0 {
        ms
    } else {
        ms - rem + minute
    }
}

/// Binary `F(e)`: clamp, then place the wakeup on a whole minute at or after
/// `now + clamped`. For delays inside the five-minute cache window the target
/// is pulled back by whole minutes until it lands at least `cacheLeadMs` before
/// the cache cliff (while staying ≥ one minute out).
#[must_use]
pub fn wakeup_target(raw: f64, now_ms: i64) -> WakeupTarget {
    let clamped = clamp_delay_seconds(raw);
    let was_clamped = delay_was_clamped(raw);
    let requested_ms = now_ms + clamped * 1000;
    let mut target_ms = ceil_to_minute_ms(requested_ms);
    if CACHE_LEAD_MS > 0 && clamped * 1000 <= CACHE_TTL_MS {
        let limit = CACHE_TTL_MS - CACHE_LEAD_MS;
        while target_ms - now_ms > limit && target_ms - 60_000 >= now_ms + MIN_DELAY_SECONDS * 1000
        {
            target_ms -= 60_000;
        }
    }
    WakeupTarget {
        clamped_delay_seconds: clamped,
        was_clamped,
        target_ms,
    }
}

// ── Keepalive fallback (binary `QXn`, budget `D=1`, delay `O=1200`) ───────────

/// `O` — the keepalive fallback delay (seconds): one quiet heartbeat at 1200s
/// if the model did not reschedule.
const KEEPALIVE_DELAY_SECONDS: i64 = 1200;
/// `D` — the consecutive-keepalive budget: after this many back-to-back
/// keepalives with no model reschedule, the loop ends.
const KEEPALIVE_BUDGET: u32 = 1;

/// Outcome of an [`arm_keepalive`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveOutcome {
    /// A fallback wakeup was scheduled (binary `E(…, {viaKeepalive:true})`).
    Armed,
    /// The consecutive-keepalive budget was exhausted → loop ended
    /// (`tengu_loop_ended{model_stopped, via_keepalive:true}`).
    BudgetExhausted,
    /// The loop reached its maximum age → loop ended (`aged_out`).
    AgedOut,
}

/// What a successful [`schedule_dynamic_wakeup`] produced (binary `E` return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledWakeup {
    /// Epoch ms the wakeup fires.
    pub scheduled_for_ms: i64,
    /// Seconds after clamping.
    pub clamped_delay_seconds: i64,
    /// Whether the request was clamped.
    pub was_clamped: bool,
}

/// Session-or-process loop state accessor: the scheduler's [`al::LoopRuntime`]
/// when it owns one, else the process-global fallback.
struct LoopState(Option<Arc<al::LoopRuntime>>);

impl LoopState {
    fn record(&self, prompt: &str) -> Option<DynamicLoopRecord> {
        match &self.0 {
            Some(rt) => rt.dynamic_loop_record(prompt),
            None => al::dynamic_loop_record(prompt),
        }
    }
    fn set_record(&self, prompt: &str, record: DynamicLoopRecord) {
        match &self.0 {
            Some(rt) => rt.set_dynamic_loop_record(prompt, record),
            None => al::set_dynamic_loop_record(prompt, record),
        }
    }
    fn keepalives(&self) -> u32 {
        match &self.0 {
            Some(rt) => rt.consecutive_keepalives(),
            None => al::loop_consecutive_keepalives(),
        }
    }
    fn set_keepalives(&self, n: u32) {
        match &self.0 {
            Some(rt) => rt.set_consecutive_keepalives(n),
            None => al::set_loop_consecutive_keepalives(n),
        }
    }
    fn mark_rescheduled(&self) {
        match &self.0 {
            Some(rt) => rt.mark_rescheduled(),
            None => al::mark_loop_rescheduled(),
        }
    }
    fn take_in_flight(&self) -> Option<String> {
        match &self.0 {
            Some(rt) => rt.take_in_flight_prompt(),
            None => al::take_loop_tick_in_flight_prompt(),
        }
    }
    /// `Ort(prompt)` — drop the per-prompt dynamic-loop record.
    fn forget(&self, prompt: &str) {
        match &self.0 {
            Some(rt) => rt.forget_dynamic_loop(prompt),
            None => al::forget_dynamic_loop(prompt),
        }
    }
    fn loop_ended(&self) -> bool {
        match &self.0 {
            Some(rt) => rt.loop_ended(),
            None => al::loop_ended(),
        }
    }
    fn set_loop_ended(&self, ended: bool) {
        match &self.0 {
            Some(rt) => rt.set_loop_ended(ended),
            None => al::set_loop_ended(ended),
        }
    }
    /// Record this turn's `ScheduleWakeup({noop})` for the no-op fold.
    ///
    /// Fold bookkeeping is session-scoped ONLY. The process-global fallback
    /// exists for hosts with no session object, and such a host also has no
    /// message queue to deliver a wakeup into (`WakeupSchedulerCell` stays
    /// empty), so it has no tick to fold and nothing to render a streak on.
    fn mark_noop(&self, noop: bool) {
        if let Some(rt) = &self.0 {
            rt.mark_noop_reported(noop);
        }
    }
}

/// JS `pluralize(n, word)`: the bare word at exactly 1, else `word + "s"`.
fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// PARITY `D(transcript, shouldFold, task, uuid)` — the two lines a firing
/// `/loop` wakeup announces itself with.
///
/// Always `Claude resuming /loop wakeup (Sep 7 3:04pm)`; when the ticks before
/// it were quiet, the fold suffix `<MIDDLE DOT> N no-op tick(s) since <when>`
/// and the companion meta line `K(n)`.
///
/// The oracle appends these to the transcript array and hangs `foldedUuids` on
/// the fire record so its renderer can collapse the span. LingXi has no
/// transcript array at this seam and no renderer that honours `foldedUuids`, so
/// the lines are emitted as system notices: the streak is surfaced and counted,
/// but the earlier quiet turns stay on screen instead of collapsing.
#[must_use]
pub fn loop_wakeup_lines(
    now_ms: u64,
    streak: Option<(u32, std::time::SystemTime)>,
) -> (String, Option<String>) {
    let head = format!(
        "Claude resuming /loop wakeup ({})",
        cron::short_local_timestamp(now_ms)
    );
    let Some((streak, since)) = streak else {
        return (head, None);
    };
    let since_ms = since
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(now_ms, |d| u64::try_from(d.as_millis()).unwrap_or(now_ms));
    (
        format!(
            "{head} \u{b7} {streak} no-op {} since {}",
            plural(streak, "tick"),
            cron::short_local_timestamp(since_ms)
        ),
        Some(format!(
            "[{streak} prior /loop {} found nothing actionable; loop is healthy.]",
            plural(streak, "wakeup")
        )),
    )
}

/// PARITY `v()` + `D()`: settle the turn that just ended into the `/loop` no-op
/// streak and emit the `loop_noop_fold` counter.
///
/// Call at EVERY turn-completion edge, BEFORE [`maybe_arm_keepalive`] and
/// [`cancel_dynamic_loop_on_user_abort`] — both consume the in-flight tick
/// marker this reads. Returns `None` for turns that were not loop ticks.
///
/// The caller marks the disturbances only it can see (a user abort, a `Now`
/// command) with `LoopRuntime::veto_tick` first; the remaining veto — the model
/// not ending the tick with `noop: true` — is decided here.
pub fn settle_loop_tick(
    runtime: &al::LoopRuntime,
    span: al::LoopSpanCounts,
) -> Option<al::LoopFoldOutcome> {
    let outcome = runtime.settle_tick(std::time::SystemTime::now(), span)?;
    match outcome {
        al::LoopFoldOutcome::Folded {
            streak,
            duration_secs,
            span,
            ..
        } => telemetry::emit_loop_noop_fold(streak, span.span_len, span.tool_uses, duration_secs),
        al::LoopFoldOutcome::Vetoed { reason } => {
            telemetry::emit_loop_noop_fold_veto(reason.reason());
        }
    }
    Some(outcome)
}

/// Binary `L(reason, extras)`: emit `tengu_loop_ended` and mark the loop ended.
fn end_loop(state: &LoopState, reason: &str, via_keepalive: Option<bool>) {
    telemetry::emit_loop_ended(reason, via_keepalive);
    state.set_loop_ended(true);
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Binary `E(delay, prompt, {viaKeepalive, reason})` — the single scheduling
/// path shared by the model's call and the keepalive fallback.
///
/// 1. A model call resets the consecutive-keepalive counter.
/// 2. Pending wakeups for this session are cancelled (superseded).
/// 3. The loop's age is measured from its first wakeup (a loop idle for more
///    than an hour restarts); at `recurringMaxAgeMs` (7 days) the loop ends
///    with `aged_out` and nothing is scheduled.
/// 4. Otherwise the wakeup is minute-aligned via [`wakeup_target`] and armed.
pub async fn schedule_dynamic_wakeup(
    scheduler: &Arc<dyn WakeupScheduler>,
    raw_delay_seconds: f64,
    prompt: &str,
    reason: Option<&str>,
    via_keepalive: bool,
) -> Option<ScheduledWakeup> {
    let state = LoopState(scheduler.loop_runtime());
    if !via_keepalive {
        state.set_keepalives(0);
    }
    // PARITY `E`: `let m = x()` — superseding cancels the pending wakeups and
    // reports the count. It does NOT call `Ort`; only `ZXn` / `t3t` forget.
    let superseded = scheduler.cancel_pending().await.len();
    let now = now_epoch_ms();
    let record = state.record(prompt);
    let restarted = record.is_some_and(|d| now > d.last_scheduled_for_ms + LOOP_RESTART_GAP_MS);
    let started_at = match record {
        Some(d) if !restarted => d.started_at_ms,
        _ => now,
    };
    if LOOP_MAX_AGE_MS > 0 && now - started_at >= LOOP_MAX_AGE_MS {
        if !record.is_some_and(|d| d.aged_out) {
            state.set_record(
                prompt,
                DynamicLoopRecord {
                    started_at_ms: started_at,
                    last_scheduled_for_ms: now - (MAX_DELAY_SECONDS - MIN_DELAY_SECONDS) * 1000,
                    aged_out: true,
                },
            );
            telemetry::emit_loop_dynamic_wakeup_aged_out(
                (now - started_at).max(0) as u64,
                LOOP_MAX_AGE_MS as u64,
            );
            end_loop(&state, "aged_out", Some(via_keepalive));
        }
        return None;
    }
    let target = wakeup_target(raw_delay_seconds, now);
    let delay = Duration::from_millis((target.target_ms - now).max(0) as u64);
    scheduler
        .schedule(
            delay,
            prompt.to_string(),
            reason.map_or_else(|| "loop keepalive fallback".to_string(), str::to_string),
        )
        .await;
    state.set_record(
        prompt,
        DynamicLoopRecord {
            started_at_ms: started_at,
            last_scheduled_for_ms: target.target_ms,
            aged_out: false,
        },
    );
    state.set_loop_ended(false);
    state.mark_rescheduled();
    let scheduled = ScheduledWakeup {
        scheduled_for_ms: target.target_ms,
        clamped_delay_seconds: target.clamped_delay_seconds,
        was_clamped: target.was_clamped,
    };
    if via_keepalive {
        state.set_keepalives(state.keepalives() + 1);
        tracing::info!(
            "[loop] keepalive armed (model did not reschedule): {}s fallback",
            target.clamped_delay_seconds
        );
        telemetry::emit_loop_keepalive_fired(
            target.clamped_delay_seconds as u64,
            al::is_loop_default_sentinel(prompt),
        );
        return Some(scheduled);
    }
    let clamped_note = if target.was_clamped {
        format!(" (clamped from {raw_delay_seconds}s)")
    } else {
        String::new()
    };
    let reason_note = reason.map_or_else(String::new, |r| format!(" — {r}"));
    tracing::info!(
        "[loop] dynamic wakeup scheduled: {}s{clamped_note}{reason_note}",
        target.clamped_delay_seconds
    );
    telemetry::emit_loop_dynamic_wakeup_scheduled(
        if raw_delay_seconds.is_finite() {
            raw_delay_seconds
        } else {
            0.0
        },
        target.clamped_delay_seconds as u64,
        target.was_clamped,
        reason.map_or(0, |r| r.encode_utf16().count()),
        superseded as u64,
    );
    Some(scheduled)
}

/// Binary `QXn(prompt)` — arm the keepalive fallback when a dynamic /loop tick
/// completes without the model rescheduling: budget check, then the shared
/// scheduling path with `viaKeepalive: true`.
pub async fn arm_keepalive(scheduler: &Arc<dyn WakeupScheduler>, prompt: &str) -> KeepaliveOutcome {
    let state = LoopState(scheduler.loop_runtime());
    if state.keepalives() >= KEEPALIVE_BUDGET {
        tracing::info!(
            "[loop] keepalive budget exhausted (model declined to reschedule twice) — ending loop"
        );
        end_loop(&state, "model_stopped", Some(true));
        return KeepaliveOutcome::BudgetExhausted;
    }
    match schedule_dynamic_wakeup(
        scheduler,
        KEEPALIVE_DELAY_SECONDS as f64,
        prompt,
        None,
        true,
    )
    .await
    {
        Some(_) => KeepaliveOutcome::Armed,
        None => KeepaliveOutcome::AgedOut,
    }
}

/// Session-scoped variant of [`arm_keepalive`] (the runtime is the scheduler's).
pub async fn arm_keepalive_with_runtime(
    scheduler: &Arc<dyn WakeupScheduler>,
    prompt: &str,
    _runtime: &al::LoopRuntime,
) -> KeepaliveOutcome {
    arm_keepalive(scheduler, prompt).await
}

/// The turn-completion keepalive trigger (binary loading→idle effect:
/// `let l=tAt();if(l!==null){I7e(null);if(YXn()&&!BY())QXn(l)}`).
///
/// Call once at every turn's completion edge. Returns `None` when the just-ended
/// turn was NOT a loop tick (no in-flight prompt), when the keepalive gate is
/// off, or when the model rescheduled this turn; otherwise runs
/// [`arm_keepalive`] and returns its outcome.
pub async fn maybe_arm_keepalive(scheduler: &Arc<dyn WakeupScheduler>) -> Option<KeepaliveOutcome> {
    let prompt = al::take_loop_tick_in_flight_prompt()?;
    let rescheduled = al::take_loop_rescheduled();
    if !al::is_loop_keepalive_enabled() || rescheduled {
        return None;
    }
    Some(arm_keepalive(scheduler, &prompt).await)
}

/// Session-scoped variant of [`maybe_arm_keepalive`].
pub async fn maybe_arm_keepalive_with_runtime(
    scheduler: &Arc<dyn WakeupScheduler>,
    runtime: &al::LoopRuntime,
) -> Option<KeepaliveOutcome> {
    let prompt = runtime.take_in_flight_prompt()?;
    let rescheduled = runtime.take_rescheduled();
    if !al::is_loop_keepalive_enabled() || rescheduled {
        return None;
    }
    Some(arm_keepalive_with_runtime(scheduler, &prompt, runtime).await)
}

/// Binary `ZXn()` — the model called `ScheduleWakeup({stop:true})`: cancel every
/// pending wakeup, drop the in-flight tick, reset the keepalive counter and end
/// the loop (terminal event suppressed when the loop already ended). Returns
/// how many pending wakeups were cancelled.
pub async fn stop_dynamic_loop(scheduler: Option<&Arc<dyn WakeupScheduler>>) -> usize {
    let state = LoopState(scheduler.and_then(|s| s.loop_runtime()));
    let already_ended = state.loop_ended();
    let in_flight = state.take_in_flight();
    state.set_keepalives(0);
    let cancelled_prompts = match scheduler {
        Some(s) => s.cancel_pending().await,
        None => Vec::new(),
    };
    // PARITY `ZXn`: `for(let l of o) Ort(l.prompt); if(t!==null) Ort(t)` — every
    // cancelled wakeup's prompt and the in-flight tick's prompt lose their
    // chain-start record, so a later `/loop` on the same prompt is a NEW loop.
    let cancelled = cancelled_prompts.len();
    for prompt in &cancelled_prompts {
        state.forget(prompt);
    }
    if let Some(prompt) = &in_flight {
        state.forget(prompt);
    }
    if already_ended {
        tracing::info!(
            "[loop] ScheduleWakeup({{stop:true}}) after loop already ended — cleanup only, terminal event suppressed"
        );
        return cancelled;
    }
    tracing::info!(
        "[loop] model called ScheduleWakeup({{stop:true}}) — ending loop ({cancelled} pending wakeup(s) cancelled{})",
        if in_flight.is_some() { ", tick in flight" } else { "" }
    );
    end_loop(&state, "model_stopped", Some(false));
    cancelled
}

/// Binary `t3t()` — user abort: cancel every pending wakeup and the in-flight
/// tick, emit `tengu_loop_ended{user_abort, loops_cancelled}`. Returns the count.
pub async fn cancel_dynamic_loop_on_user_abort(scheduler: &Arc<dyn WakeupScheduler>) -> usize {
    let state = LoopState(scheduler.loop_runtime());
    let in_flight = state.take_in_flight();
    state.set_keepalives(0);
    let cancelled_prompts = scheduler.cancel_pending().await;
    let cancelled = cancelled_prompts.len();
    if cancelled == 0 && in_flight.is_none() {
        return 0;
    }
    // PARITY `t3t`: same `Ort` sweep as `ZXn`.
    for prompt in &cancelled_prompts {
        state.forget(prompt);
    }
    if let Some(prompt) = &in_flight {
        state.forget(prompt);
    }
    tracing::info!(
        "[loop/dynamic] cancelled {cancelled} pending loop wakeup(s) on user abort{}",
        if in_flight.is_some() {
            " (tick in flight)"
        } else {
            ""
        }
    );
    telemetry::emit_loop_ended("user_abort", None);
    state.set_loop_ended(true);
    cancelled
}

/// Binary `K_n()` — `/loop` was invoked: clear the loop-ended marker.
///
/// The process-global arm is [`al::note_loop_invoked`], which the `/loop` skill
/// itself calls (it can reach `cron` but not this tool crate). This overload
/// exists for hosts that own a session-scoped runtime.
pub fn note_loop_invoked(runtime: Option<&al::LoopRuntime>) {
    match runtime {
        Some(rt) => rt.set_loop_ended(false),
        None => al::note_loop_invoked(),
    }
}

/// Format an epoch-ms timestamp as local `HH:MM:SS`
/// (binary `new Date(e).toTimeString().slice(0,8)`).
#[must_use]
fn local_hhmmss(epoch_ms: i64) -> String {
    let epoch_secs = epoch_ms.div_euclid(1000);
    let local_secs = epoch_secs + cron::schedule::local_offset_seconds(epoch_secs);
    let tod = local_secs.rem_euclid(86_400);
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Resolve the `prompt` argument at fire time: the autonomous / loop.md
/// sentinels expand to the real autonomous-loop tick prompt (full preamble on
/// first delivery, short reminder afterward); any other prompt passes through
/// verbatim (binary `resolveLoopDefaultFire`).
#[must_use]
pub fn resolve_wakeup_prompt(prompt: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    resolve_wakeup_prompt_at(prompt, &cwd)
}

/// Resolve a wakeup against its owning session's working directory.
#[must_use]
pub fn resolve_wakeup_prompt_at(prompt: &str, cwd: &std::path::Path) -> String {
    // Upstream sentinel predicates compare exact strings; padded sentinel-like
    // prompts are ordinary user text and must not activate the autonomous loop.
    if al::is_loop_default_sentinel(prompt) {
        return al::resolve_loop_default_fire(prompt, cwd);
    }
    prompt.to_string()
}

/// Model-facing SHORT description (binary `aZn`).
const DESCRIPTION: &str = "Schedule when to resume work in /loop dynamic mode (always pass the `prompt` arg unless stopping). Call before ending the turn to keep the loop alive; call with `stop: true` to end the loop immediately.";

/// Binary `searchHint`.
const SEARCH_HINT: &str = "self-pace the dynamic /loop: pick a delay before the next tick, or stop/end/cancel the dynamic loop with stop:true (a fixed-interval /loop is a recurring cron — cancel it with CronDelete)";

/// Binary `t` — the prompt's intro paragraphs.
const PROMPT_INTRO: &str = "Schedule when to resume work in /loop dynamic mode — the user invoked /loop without an interval, asking you to self-pace iterations of a specific task.

Do NOT schedule a short-interval wakeup to poll for background work you started — when harness-tracked work finishes, you are re-invoked automatically, so polling is wasted. Instead schedule a long fallback (1200s+) so the loop survives if the work hangs or never notifies. The exception is external work the harness cannot track (a CI run, a deploy, a remote queue) — there, pick a delay matched to how fast that state actually changes.

Pass the same /loop prompt back via `prompt` each turn so the next firing repeats the task. For an autonomous /loop (no user prompt), pass the literal sentinel `<<autonomous-loop-dynamic>>` as `prompt` instead — the runtime resolves it back to the autonomous-loop instructions at fire time. (There is a similar `<<autonomous-loop>>` sentinel for CronCreate-based autonomous loops; do not confuse the two — ScheduleWakeup always uses the `-dynamic` variant.) To end the loop, call this tool with `stop: true` (omit every other field) — the loop ends immediately and no further wakeups fire.";

/// Binary `iZn` noop paragraph.
const PROMPT_NOOP: &str = "Set `noop: true` if nothing changed — you checked and there's nothing to report (\"no change\", \"still waiting\", \"quiet hold\"). Set `noop: false` if something happened worth keeping — you edited a file, posted a message, advanced state, or surfaced a finding. Consecutive `noop: true` ticks are collapsed in the user's terminal view and tracked as a streak, so long quiet holds stay legible to the user without scrolling. Omit `noop` when stopping (`stop: true`).";

/// Binary `iZn(true)` — the 1-hour prompt-cache TTL variant.
const PROMPT_DELAY_ONE_HOUR_TTL: &str = "## Picking delaySeconds

This session's requests use a 1-hour Anthropic prompt-cache TTL, so effectively every allowed delay (the runtime clamps to [60, 3600]) wakes up with your conversation context still cached. There is no cache cliff inside that range to pace around, and scheduling extra wakeups just to keep the cache warm is pure waste — never do that. (If the session enters usage overage, later requests drop to the 5-minute TTL; don't try to track or preempt that — the guidance here stays the same.)

Match the delay to what you're actually waiting for:

- **Actively polling external state the harness can't notify you about** (a CI run, a deploy, a remote queue): pick the delay from how fast that state actually changes. A CI run that takes ~8 minutes deserves one ~480s check, not eight 60s ones.
- **The long fallback heartbeat** (something else — a Monitor, a task notification — is the primary wake signal): 1200s+, so quiet wakeups stay rare.
- **Idle ticks with no specific signal to watch**: default to **1200s–1800s** (20–30 min). The loop still checks back regularly, and the user can always interrupt if they need you sooner.

Don't think in cache windows — think about what you're actually waiting for.";

/// Binary `iZn(false)` — the 5-minute prompt-cache TTL variant.
const PROMPT_DELAY_FIVE_MINUTE_TTL: &str = "## Picking delaySeconds

This session's requests use the default 5-minute Anthropic prompt-cache TTL. Sleeping past 300 seconds means the next wake-up reads your full conversation context uncached — slower and more expensive. So the natural breakpoints:

- **Under 5 minutes (60s–270s)**: cache stays warm. Right for actively polling external state the harness can't notify you about — a CI run, a deploy, a remote queue.
- **5 minutes to 1 hour (300s–3600s)**: pay the cache miss. Right when there's no point checking sooner — waiting on something that takes minutes to change, genuinely idle, or as the long fallback heartbeat when something else is the primary wake signal.

**Don't pick 300s.** It's the worst-of-both: you pay the cache miss without amortizing it. If you're tempted to \"wait 5 minutes,\" either drop to 270s (stay in cache) or commit to 1200s+ (one cache miss buys a much longer wait). Don't think in round-number minutes — think in cache windows.

For idle ticks with no specific signal to watch, default to **1200s–1800s** (20–30 min). The loop checks back, you don't burn cache 12× per hour for nothing, and the user can always interrupt if they need you sooner.

Think about what you're actually waiting for, not just \"how long should I sleep.\" If you're polling a CI run that takes ~8 minutes, sleeping 60s burns the cache 8 times before it finishes — sleep ~270s twice instead.

The runtime clamps to [60, 3600], so you don't need to clamp yourself.";

/// Binary `iZn(undefined)` — TTL not uniformly known (the variant the binary
/// emits when the main-thread and sdk TTL signals disagree).
const PROMPT_DELAY_UNKNOWN_TTL: &str = "## Picking delaySeconds

The Anthropic prompt cache decides how expensive a wake-up is: waking inside the cache TTL re-reads your conversation context cached (fast, cheap); waking past it re-reads everything uncached. The TTL depends on how the session is billed: Claude subscriber sessions get a 1-hour TTL (dropping to 5 minutes during usage overage), while API-key, Bedrock, and Vertex sessions default to 5 minutes.

In either regime: never schedule extra wakeups just to keep the cache warm — they cost more than the cache miss they avoid. Match the delay to what you're actually waiting for: when actively polling external state the harness can't notify you about (a CI run, a deploy, a remote queue), pick the delay from how fast that state actually changes; for idle ticks with no specific signal to watch, default to **1200s–1800s** (20–30 min) — the user can always interrupt if they need you sooner.

On a 5-minute TTL only, two refinements: under 300s (60s–270s) the cache stays warm, so prefer 270s over 300s when actively polling (300s is the worst-of-both — you pay the miss without amortizing it); and commit to 1200s+ rather than repeated ~300s waits, so one cache miss buys a long wait.

The runtime clamps to [60, 3600], so you don't need to clamp yourself.";

/// Binary `iZn` reason paragraph.
const PROMPT_REASON: &str = "## The reason field

One short sentence on what you chose and why. Goes to telemetry and is shown back to the user. \"watching CI run\" beats \"waiting.\" The user reads this to understand what you're doing without having to predict your cadence in advance — make it specific.";

/// Which prompt-cache TTL the session's requests use (binary `rN(...)` pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCacheTtl {
    /// Both signals agree on a 1-hour TTL.
    OneHour,
    /// Both signals agree on the default 5-minute TTL.
    FiveMinutes,
    /// The signals disagree or the TTL is not known.
    Unknown,
}

/// Binary `iZn(e)`: intro, noop paragraph, the TTL-specific delay guidance and
/// the reason paragraph, each separated by a blank line, trailing newline.
#[must_use]
pub fn build_prompt(ttl: PromptCacheTtl) -> String {
    let delay = match ttl {
        PromptCacheTtl::OneHour => PROMPT_DELAY_ONE_HOUR_TTL,
        PromptCacheTtl::FiveMinutes => PROMPT_DELAY_FIVE_MINUTE_TTL,
        PromptCacheTtl::Unknown => PROMPT_DELAY_UNKNOWN_TTL,
    };
    format!("{PROMPT_INTRO}\n\n{PROMPT_NOOP}\n\n{delay}\n\n{PROMPT_REASON}\n")
}

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    // PARITY: binary `Uqo` — `strictObject({delaySeconds: DM(number).optional(),
    // reason: string.optional(), prompt: string.optional(), stop:
    // boolean.optional(), noop: boolean.optional()})`. Nothing is required at
    // the schema level; `call` enforces the "required unless stop" rule.
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "delaySeconds": {
                "type": "number",
                "description": "Seconds from now to wake up. Clamped to [60, 3600] by the runtime. Required unless `stop` is true."
            },
            "reason": {
                "type": "string",
                "description": "One short sentence explaining the chosen delay. Goes to telemetry and is shown to the user. Be specific. Required unless `stop` is true."
            },
            "prompt": {
                "type": "string",
                "description": "The /loop input to fire on wake-up. Pass the same /loop input verbatim each turn so the next firing re-enters the skill and continues the loop. For autonomous /loop (no user prompt), pass the literal sentinel `<<autonomous-loop-dynamic>>` instead (the dynamic-pacing variant, not the CronCreate-mode `<<autonomous-loop>>`). Required unless `stop` is true."
            },
            "stop": {
                "type": "boolean",
                "description": "Set to true to end the dynamic loop immediately instead of scheduling another wakeup. When true, all other fields are ignored and no further wakeups fire."
            },
            "noop": {
                "type": "boolean",
                "description": "true = nothing changed (you checked and there is nothing to report). false = something happened worth keeping (edited a file, posted a message, advanced state, surfaced a finding). Consecutive noop:true ticks are collapsed in the user's terminal view and tracked as a streak. Required unless `stop` is true."
            }
        }
    })
});

/// Tool RESULT schema (binary `Hqo`).
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "required": ["scheduledFor", "clampedDelaySeconds", "wasClamped"],
        "properties": {
            "scheduledFor": {
                "type": "number",
                "description": "Epoch ms timestamp when the next wakeup will fire"
            },
            "clampedDelaySeconds": {
                "type": "number",
                "description": "Actual delay used after clamping to runtime bounds"
            },
            "wasClamped": {
                "type": "boolean",
                "description": "True if the requested delaySeconds was outside [60, 3600]"
            },
            "stopped": {
                "type": "boolean",
                "description": "True when the model ended the loop via `stop: true`"
            },
            "cancelledWakeups": {
                "type": "number",
                "description": "How many pending dynamic-loop wakeups stop:true cancelled. 0 means nothing was pending — a recurring /loop cron is not cancelled by stop:true."
            }
        }
    })
});

/// `ScheduleWakeup` — `/loop` dynamic-mode one-shot self-wakeup.
pub struct ScheduleWakeupTool {
    ctx: tool_api::BuiltinToolContext,
    /// Set-once wakeup seam (see [`WakeupSchedulerCell`]). Empty until a host
    /// fills it via the clone returned by [`Self::wakeup_cell`]; while empty the
    /// tool reports the zero triple (no wakeup can fire).
    wakeup: WakeupSchedulerCell,
    /// Raised when a call actually ARMS a wakeup, so the turn loop can end the
    /// turn on a lone `ScheduleWakeup` (see [`Self::loop_wakeup_armed_slot`]).
    armed: Arc<std::sync::atomic::AtomicBool>,
}

impl ScheduleWakeupTool {
    /// Construct with an empty set-once scheduler cell. The host fills it later
    /// via the clone from [`Self::wakeup_cell`].
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self {
            ctx,
            wakeup: Arc::new(std::sync::OnceLock::new()),
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// A clone of the flag this tool raises when a call arms a wakeup.
    ///
    /// PARITY the oracle's `kg().some(o => o.kind === "loop" && o.prompt === jr)`
    /// — the turn loop's test for "that `ScheduleWakeup` really did arm a loop
    /// cron". The oracle re-reads its session cron registry; LingXi has no such
    /// registry reachable from the orchestrator (which does not depend on
    /// `tool-cron`), so the tool reports the same fact through a shared flag,
    /// the way `EndConversationSlot` already does.
    #[must_use]
    pub fn loop_wakeup_armed_slot(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.armed.clone()
    }

    /// A clone of the set-once cell, for the composition root to fill once the
    /// per-connection queue + spawner exist (`cell.set(scheduler)`).
    #[must_use]
    pub fn wakeup_cell(&self) -> WakeupSchedulerCell {
        self.wakeup.clone()
    }

    /// Construct with a live [`WakeupScheduler`] already wired.
    #[must_use]
    pub fn with_scheduler(
        ctx: tool_api::BuiltinToolContext,
        wakeup: Arc<dyn WakeupScheduler>,
    ) -> Self {
        let cell: WakeupSchedulerCell = Arc::new(std::sync::OnceLock::new());
        let _ = cell.set(wakeup);
        Self {
            ctx,
            wakeup: cell,
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

fn pii_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("tool_name".into(), verified_str(SCHEDULE_WAKEUP_TOOL_NAME));
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(FAILED, md).await;
}

async fn emit_completed(bus: &Arc<AnalyticsBus>, duration_ms: u64, scheduled: bool) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("tool_name".into(), verified_str(SCHEDULE_WAKEUP_TOOL_NAME));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    md.insert("scheduled".into(), AnalyticsValue::Bool(scheduled));
    bus.log_event(COMPLETED, md).await;
}

fn result(data: Value) -> ToolCallResult {
    ToolCallResult {
        data,
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// Binary `mapToolResultToToolResultBlockParam` for `scheduledFor === 0`.
const NOT_SCHEDULED_TEXT: &str = "Wakeup not scheduled. The loop reached its maximum duration — the loop has ended; do not re-issue.";

/// PARITY: `{scheduledFor:0, clampedDelaySeconds:0, wasClamped:false}` with the
/// `e===0` model text — the aged-out / no-scheduler branches.
fn zero_triple_result(reason: &str) -> ToolCallResult {
    result(json!({
        "scheduledFor": 0,
        "clampedDelaySeconds": 0,
        "wasClamped": false,
        "model_content": NOT_SCHEDULED_TEXT,
        "reason": reason,
    }))
}

/// Binary `mapToolResultToToolResultBlockParam` for `stopped === true`.
fn stopped_result(cancelled: usize) -> ToolCallResult {
    let tail = "If you armed a Monitor for this loop, TaskStop it now; otherwise nothing more to do this turn.";
    let content = if cancelled == 0 {
        format!("Loop stopped — any dynamic loop in this session is ended; there was no pending wakeup to cancel. If you are running a fixed-interval /loop (a recurring cron), it is NOT stopped by this call — cancel it with CronDelete. {tail}")
    } else {
        format!("Loop stopped — cancelled {cancelled} pending wakeup(s); no further dynamic-loop wakeups scheduled. {tail}")
    };
    result(json!({
        "scheduledFor": 0,
        "clampedDelaySeconds": 0,
        "wasClamped": false,
        "stopped": true,
        "cancelledWakeups": cancelled,
        "model_content": content,
    }))
}

/// Render with an explicit clock so the full model-visible result is byte-tested.
fn scheduled_wakeup_text(scheduled: ScheduledWakeup, now_ms: i64) -> String {
    let hhmmss = local_hhmmss(scheduled.scheduled_for_ms);
    let secs = ((scheduled.scheduled_for_ms - now_ms) as f64 / 1000.0)
        .round()
        .max(0.0) as i64;
    let clamped_suffix = if scheduled.was_clamped {
        format!(
            " (clamped to {}s from your requested value)",
            scheduled.clamped_delay_seconds
        )
    } else {
        String::new()
    };
    format!(
            "Next wakeup scheduled for {hhmmss} (in {secs}s){clamped_suffix}. Nothing more to do this turn — the harness re-invokes you when the wakeup fires or a task-notification arrives."
        )
}

/// Binary `DM(number)` — accept a JSON number or a numeric string.
fn coerce_delay(value: Option<&Value>) -> Result<Option<f64>, ValidationError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) => {
            // Upstream TH only coerces signed decimal strings and finite values.
            // Rust's float parser also accepts exponents, infinities and `.5`.
            let trimmed = s.trim_matches(|c: char| {
                // ECMAScript trim includes BOM but excludes U+0085.
                matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}'
                    | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}'
                    | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
            });
            let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
            let mut parts = unsigned.split('.');
            let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            let decimal = parts.next().is_some_and(digits)
                && parts.next().is_none_or(digits)
                && parts.next().is_none();
            if decimal {
                if let Ok(value) = trimmed.parse::<f64>() {
                    if value.is_finite() {
                        return Ok(Some(value));
                    }
                }
            }
            Err(ValidationError(
                "ScheduleWakeup: `delaySeconds` must be a number".into(),
            ))
        }
        Some(_) => Err(ValidationError(
            "ScheduleWakeup: `delaySeconds` must be a number".into(),
        )),
    }
}

/// The binary's `ScheduleWakeupInputError` checks (run in `call` there; here in
/// `validate_input`, before the turn loop records a tool failure).
fn validate_wakeup_input(input: &Value) -> Result<(), ValidationError> {
    if input.get("stop").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    for (key, ty) in [("stop", "boolean"), ("noop", "boolean")] {
        if input
            .get(key)
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err(ValidationError(format!(
                "ScheduleWakeup: `{key}` must be a {ty}"
            )));
        }
    }
    let delay = coerce_delay(input.get("delaySeconds"))?;
    let reason = input.get("reason").and_then(Value::as_str);
    if delay.is_none() || reason.is_none() {
        return Err(ValidationError(
            "`delaySeconds` and `reason` are required when `stop` is not true.".into(),
        ));
    }
    if input.get("prompt").and_then(Value::as_str).is_none() {
        return Err(ValidationError(
            "`prompt` is required when `stop` is not true.".into(),
        ));
    }
    if input.get("noop").and_then(Value::as_bool).is_none() {
        return Err(ValidationError(
            "`noop` is required when `stop` is not true.".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn name(&self) -> &str {
        SCHEDULE_WAKEUP_TOOL_NAME
    }
    fn native_input_validation(&self) -> bool {
        true
    }
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }
    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }
    fn coerce_input(&self, input: &Value) -> Option<CoercedInput> {
        // Zod's TH preprocessing runs before type validation upstream. Move the
        // same accepted string rewrite ahead of our exported JSON-schema gate.
        if !input.get("delaySeconds")?.is_string() {
            return None;
        }
        let delay = coerce_delay(input.get("delaySeconds")).ok()??;
        let mut normalized = input.clone();
        normalized["delaySeconds"] = json!(delay);
        Some(CoercedInput {
            input: normalized,
            shape_class: "delaySeconds".into(),
        })
    }
    fn search_hint(&self) -> Option<&str> {
        Some(SEARCH_HINT)
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // Binary: no `isEnabled` — always available where registered (main
        // session only; denylisted for subagents).
        true
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    /// Upstream `.270` returns tool-local `passthrough` in auto mode so the
    /// outer classifier can review the request; otherwise it returns `allow`.
    /// Our Allow already has passthrough semantics: both turn_loop.rs and
    /// tool_invoker_impl.rs preserve the outer policy result for tool-local
    /// Allow. Returning Ask here would incorrectly force another user prompt.
    /// PermissionPolicy carries this tool's non-Auto allow baseline after
    /// explicit rules. In Auto, PolicyPermissionGate invokes the existing
    /// classifier, which returns Pass and falls back to the prompt transport.
    /// Matching Anthropic's LLM classification requires that shared classifier,
    /// not a tool-local bypass or fabricated deterministic approval.
    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "ScheduleWakeup schedules a one-shot self-wakeup for /loop dynamic mode"
                    .into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        // LingXi is multi-provider and has no uniform Anthropic prompt-cache TTL
        // signal, so it emits the variant the binary uses when its two TTL
        // signals disagree.
        build_prompt(PromptCacheTtl::Unknown)
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // Required-unless-stop checks are thrown from call upstream, producing
        // Error: ..., rather than the validateInput <tool_use_error> wrapper.
        let _ = input;
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        // PARITY `v()`'s `if(i.name===Xi) p = i.input?.noop===!0`: the LAST
        // `ScheduleWakeup` of the tick decides whether it was quiet. Recorded
        // for EVERY call — including a `stop: true` one, which carries no
        // `noop` and therefore vetoes, exactly as `p !== true` does there.
        LoopState(self.wakeup.get().and_then(|w| w.loop_runtime()))
            .mark_noop(input.get("noop").and_then(Value::as_bool) == Some(true));

        // PARITY: `if(p===!0) return {…stopped:!0, cancelledWakeups: ZXn()}`.
        if input.get("stop").and_then(Value::as_bool) == Some(true) {
            let cancelled = stop_dynamic_loop(self.wakeup.get()).await;
            emit_completed(&bus, started.elapsed().as_millis() as u64, false).await;
            return Ok(stopped_result(cancelled));
        }

        if let Err(ValidationError(message)) = validate_wakeup_input(&input) {
            emit_failed(&bus, "invalid_input", started.elapsed().as_millis() as u64).await;
            return Err(ToolError::InvalidInput(message));
        }
        let raw_delay = coerce_delay(input.get("delaySeconds"))
            .ok()
            .flatten()
            .unwrap_or(f64::NAN);
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let mut md: LogEventMetadata = HashMap::new();
        md.insert("tool_name".into(), verified_str(SCHEDULE_WAKEUP_TOOL_NAME));
        md.insert(
            "delay_seconds".into(),
            AnalyticsValue::Int(clamp_delay_seconds(raw_delay)),
        );
        md.insert("_PROTO_reason".into(), pii_str(&reason));
        bus.log_event(STARTED, md).await;

        // A host with no scheduler cell owns no message queue to fire into: the
        // binary's `aKi(...) === null` stand-in — zero triple, no loop telemetry.
        let Some(wakeup) = self.wakeup.get() else {
            emit_completed(&bus, started.elapsed().as_millis() as u64, false).await;
            return Ok(zero_triple_result(&reason));
        };

        let Some(scheduled) =
            schedule_dynamic_wakeup(wakeup, raw_delay, &prompt, Some(&reason), false).await
        else {
            emit_completed(&bus, started.elapsed().as_millis() as u64, false).await;
            return Ok(zero_triple_result(&reason));
        };
        emit_completed(&bus, started.elapsed().as_millis() as u64, true).await;
        // A wakeup is now armed: the turn loop may end the turn if this was the
        // round's only tool call (PARITY the `kg()` lookup).
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);

        // PARITY: `mapToolResultToToolResultBlockParam` — local HH:MM:SS,
        // `Math.max(0, Math.round((e - Date.now())/1000))`, clamp suffix.
        let now_ms = now_epoch_ms();
        let model_content = scheduled_wakeup_text(scheduled, now_ms);

        Ok(result(json!({
            "scheduledFor": scheduled.scheduled_for_ms,
            "clampedDelaySeconds": scheduled.clamped_delay_seconds,
            "wasClamped": scheduled.was_clamped,
            "model_content": model_content,
            "reason": reason,
        })))
    }
}

#[cfg(test)]
mod fold_line_tests {
    use super::loop_wakeup_lines;
    use std::time::{Duration, SystemTime};

    const NOW_MS: u64 = 1_788_793_449_000;
    const SINCE_MS: u64 = 1_788_790_449_000;

    fn since() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(SINCE_MS)
    }

    /// With no streak the wakeup announces itself with the bare resume line and
    /// no companion — the oracle's `e.kind !== "fold"` arm.
    #[test]
    fn a_wakeup_after_a_working_tick_has_no_streak_suffix() {
        let (head, companion) = loop_wakeup_lines(NOW_MS, None);
        assert_eq!(
            head,
            format!(
                "Claude resuming /loop wakeup ({})",
                cron::short_local_timestamp(NOW_MS)
            )
        );
        assert_eq!(companion, None);
    }

    /// PARITY the fold arm: the `\u{b7}` suffix and the `K(n)` companion, with the
    /// oracle's singular/plural at exactly 1.
    #[test]
    fn a_folded_wakeup_carries_the_streak_and_its_companion() {
        let (head, companion) = loop_wakeup_lines(NOW_MS, Some((1, since())));
        assert_eq!(
            head,
            format!(
                "Claude resuming /loop wakeup ({}) \u{b7} 1 no-op tick since {}",
                cron::short_local_timestamp(NOW_MS),
                cron::short_local_timestamp(SINCE_MS)
            )
        );
        assert_eq!(
            companion.as_deref(),
            Some("[1 prior /loop wakeup found nothing actionable; loop is healthy.]")
        );

        let (head, companion) = loop_wakeup_lines(NOW_MS, Some((4, since())));
        assert!(head.ends_with(&format!(
            "\u{b7} 4 no-op ticks since {}",
            cron::short_local_timestamp(SINCE_MS)
        )));
        assert_eq!(
            companion.as_deref(),
            Some("[4 prior /loop wakeups found nothing actionable; loop is healthy.]")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use std::sync::Mutex;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx_in};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Serialize tests that touch the process-global loop state, and start each
    /// from a clean slate.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        let g = al::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        al::reset_loop_runtime_state();
        al::reset_autonomous_loop_delivered();
        std::env::remove_var("CLAUDE_CODE_LOOP_KEEPALIVE");
        telemetry::test_clear_flag("tengu_kairos_loop_keepalive");
        g
    }

    /// A scheduler that records calls and can cancel what it armed.
    struct Rec {
        calls: Mutex<Vec<(Duration, String, String)>>,
        /// Prompts of the wakeups still pending (what `cancel_pending` returns).
        pending: Mutex<Vec<String>>,
    }
    impl Rec {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                pending: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl WakeupScheduler for Rec {
        async fn schedule(&self, delay: Duration, prompt: String, reason: String) {
            self.calls
                .lock()
                .unwrap()
                .push((delay, prompt.clone(), reason));
            self.pending.lock().unwrap().push(prompt);
        }
        async fn cancel_pending(&self) -> Vec<String> {
            std::mem::take(&mut *self.pending.lock().unwrap())
        }
    }

    fn tool_with(rec: Arc<Rec>) -> ScheduleWakeupTool {
        let tmp = tempfile::tempdir().unwrap();
        ScheduleWakeupTool::with_scheduler(
            shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()),
            rec,
        )
    }

    #[test]
    fn constants_locked() {
        assert_eq!(SCHEDULE_WAKEUP_TOOL_NAME, "ScheduleWakeup");
        assert_eq!(MIN_DELAY_SECONDS, 60);
        assert_eq!(MAX_DELAY_SECONDS, 3600);
        assert_eq!(
            AUTONOMOUS_LOOP_DYNAMIC_SENTINEL,
            "<<autonomous-loop-dynamic>>"
        );
        assert_eq!(KEEPALIVE_DELAY_SECONDS, 1200);
        assert_eq!(KEEPALIVE_BUDGET, 1);
        assert_eq!(LOOP_MAX_AGE_MS, 7 * 24 * 60 * 60 * 1000);
        assert_eq!(CACHE_TTL_MS, 300_000);
        assert_eq!(CACHE_LEAD_MS, 15_000);
    }

    // PARITY 2.1.263 `F(e)`: NaN→60, +∞→3600, −∞→60, else Math.round; then
    // clamp; wasClamped = !isFinite || rounded !== clamped.
    #[test]
    fn clamp_and_round_match_the_binary() {
        assert_eq!(clamp_delay_seconds(f64::NAN), 60);
        assert_eq!(clamp_delay_seconds(f64::INFINITY), 3600);
        assert_eq!(clamp_delay_seconds(f64::NEG_INFINITY), 60);
        assert_eq!(clamp_delay_seconds(600.9), 601, "Math.round, not floor");
        assert_eq!(clamp_delay_seconds(600.5), 601, "JS rounds half up");
        assert_eq!(clamp_delay_seconds(59.4), 60);
        assert_eq!(clamp_delay_seconds(-5.0), 60);
        assert_eq!(clamp_delay_seconds(4000.0), 3600);
        assert_eq!(clamp_delay_seconds(1e300), 3600);
        assert!(!delay_was_clamped(600.9));
        assert!(!delay_was_clamped(60.0));
        assert!(!delay_was_clamped(3600.0));
        assert!(delay_was_clamped(59.4));
        assert!(delay_was_clamped(3600.6));
        assert!(delay_was_clamped(f64::NAN));
        assert!(delay_was_clamped(f64::INFINITY));
    }

    // PARITY 2.1.263 `F(e)` + `P(m)`: the wakeup lands on a whole minute at or
    // after `now + delay`; inside the 5-minute cache window it is pulled back by
    // whole minutes to stay ≥ 15 s ahead of the cache cliff.
    #[test]
    fn wakeup_target_is_minute_aligned_with_cache_lead() {
        let now = 1_700_000_010_000; // 1700000010 s ≡ 30 s past the minute
        let t = wakeup_target(1200.0, now);
        assert_eq!(t.clamped_delay_seconds, 1200);
        assert!(!t.was_clamped);
        assert_eq!(t.target_ms % 60_000, 0);
        assert_eq!(t.target_ms, now + 1230 * 1000, "ceil to the next minute");
        // 300 s requested from hh:mm:30 → hh:mm+6:00 (330 s) is past the
        // 285 s cache lead → pulled back to hh:mm+5:00 (270 s).
        let t = wakeup_target(300.0, now);
        assert_eq!(t.target_ms, now + 270 * 1000);
        // 60 s from hh:mm:30 → next minute is only 30 s out and the pull-back
        // floor (≥ 60 s) forbids going earlier, so it stays at hh:mm+1:00.
        let t = wakeup_target(60.0, now);
        assert_eq!(t.target_ms, now + 90 * 1000);
        // Already on a minute boundary (1700000040 s ≡ :00): no rounding.
        let t = wakeup_target(120.0, 1_700_000_040_000);
        assert_eq!(t.target_ms, 1_700_000_040_000 + 120_000);
    }

    #[test]
    fn schema_matches_uqo() {
        let props = SCHEMA["properties"].as_object().unwrap();
        assert_eq!(
            props.keys().cloned().collect::<Vec<_>>(),
            vec!["delaySeconds", "reason", "prompt", "stop", "noop"]
        );
        assert!(
            SCHEMA.get("required").is_none(),
            "nothing is required at the schema level"
        );
        assert_eq!(SCHEMA["additionalProperties"], json!(false));
        assert_eq!(
            props["delaySeconds"]["description"],
            json!("Seconds from now to wake up. Clamped to [60, 3600] by the runtime. Required unless `stop` is true.")
        );
        assert_eq!(
            props["reason"]["description"],
            json!("One short sentence explaining the chosen delay. Goes to telemetry and is shown to the user. Be specific. Required unless `stop` is true.")
        );
        assert_eq!(
            props["prompt"]["description"],
            json!("The /loop input to fire on wake-up. Pass the same /loop input verbatim each turn so the next firing re-enters the skill and continues the loop. For autonomous /loop (no user prompt), pass the literal sentinel `<<autonomous-loop-dynamic>>` instead (the dynamic-pacing variant, not the CronCreate-mode `<<autonomous-loop>>`). Required unless `stop` is true.")
        );
        assert_eq!(
            props["stop"]["description"],
            json!("Set to true to end the dynamic loop immediately instead of scheduling another wakeup. When true, all other fields are ignored and no further wakeups fire.")
        );
        assert_eq!(
            props["noop"]["description"],
            json!("true = nothing changed (you checked and there is nothing to report). false = something happened worth keeping (edited a file, posted a message, advanced state, surfaced a finding). Consecutive noop:true ticks are collapsed in the user's terminal view and tracked as a streak. Required unless `stop` is true.")
        );
        let out = OUTPUT_SCHEMA["properties"].as_object().unwrap();
        assert_eq!(
            out.keys().cloned().collect::<Vec<_>>(),
            vec![
                "scheduledFor",
                "clampedDelaySeconds",
                "wasClamped",
                "stopped",
                "cancelledWakeups"
            ]
        );
        assert_eq!(
            out["cancelledWakeups"]["description"],
            json!("How many pending dynamic-loop wakeups stop:true cancelled. 0 means nothing was pending — a recurring /loop cron is not cancelled by stop:true.")
        );
    }

    #[tokio::test]
    async fn actual_cron_tools_and_nested_dispatch_match_native_zod_corpus() {
        use lingxi_core::host::tool_invoker::{
            SubagentInvocationContext, ToolExecutionPolicy, ToolInvoker,
        };
        fn invocation_context() -> SubagentInvocationContext {
            SubagentInvocationContext {
                permission_pause_observer: None,
                parent_agent_id: None,
                origin_session_id: None,
                tool_execution_policy: ToolExecutionPolicy::Ordinary,
                agent_name: None,
                team_name: None,
                is_async: false,
                is_non_interactive_session: true,
                can_show_permission_prompts: false,
                cwd: None,
                tool_use_id: None,
                assistant_message_id: None,
                depth: 0,
                observer: None,
                parent_model: None,
                parent_model_profile: None,
                mode_override: None,
                request_source: None,
                frozen_command_denies: Vec::new(),
            }
        }
        fn schema_shape(mut value: Value) -> Value {
            if let Some(object) = value.as_object_mut() {
                object.remove("description");
                if object.get("required").is_some_and(|v| v == &json!([])) {
                    object.remove("required");
                }
                for child in object.values_mut() {
                    *child = schema_shape(child.take());
                }
            }
            value
        }
        let tmp = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf());
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ScheduleWakeupTool::new(ctx.clone())),
            Arc::new(crate::CronCreateTool::new(ctx.clone())),
            Arc::new(crate::CronDeleteTool::new(ctx.clone())),
            Arc::new(crate::CronListTool::new(ctx)),
        ];
        let mut registry = tool_api::ToolRegistry::new();
        for tool in &tools {
            registry.register_builtin(tool.clone());
        }
        let invoker = tool_api::RegistryToolInvoker::new(Arc::new(registry));
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/malformed_input_2_1_270.json"
        ))
        .unwrap();
        for tool in &tools {
            assert!(tool.native_input_validation());
            assert_eq!(
                schema_shape(tool.input_schema().clone()),
                schema_shape(oracle["schemas"][tool.name()].clone()),
                "schema {}",
                tool.name()
            );
        }
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            let name = case["tool"].as_str().unwrap();
            let tool = tools.iter().find(|tool| tool.name() == name).unwrap();
            let actual = tool
                .coerce_input(&case["input"])
                .map_or_else(|| case["input"].clone(), |c| c.input);
            assert_eq!(
                tool_api::native_schema::js_json(&actual, false),
                tool_api::native_schema::js_json(&case["coerced"], false),
                "coercion {index}"
            );
            if case["success"] == true {
                let normalized = tool_api::native_schema::normalize_flat_input(&actual)
                    .unwrap_or_else(|| actual.clone());
                let expected = case["normalized"].as_object().unwrap();
                assert_eq!(
                    normalized.as_object().unwrap().len(),
                    expected.len(),
                    "parsed keys {index}"
                );
                for (key, value) in expected {
                    assert_eq!(
                        tool_api::native_schema::js_json(&normalized[key], false),
                        tool_api::native_schema::js_json(value, false),
                        "parsed {index} {key}"
                    );
                }
            }
            if case["success"] == false {
                let error = invoker
                    .invoke(name, case["input"].clone(), invocation_context())
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.model_tool_result_content(),
                    format!(
                        "<tool_use_error>InputValidationError: {}</tool_use_error>",
                        case["display"].as_str().unwrap()
                    ),
                    "nested case {index}"
                );
            } else if let Some(message) = case["callError"].as_str() {
                assert!(
                    tool.validate_input(&actual, &fresh_ctx()).await.is_ok(),
                    "native missing-field checks run in call"
                );
                let error = invoker
                    .invoke(name, case["input"].clone(), invocation_context())
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.model_tool_result_content(),
                    format!("Error: {message}"),
                    "call case {index}"
                );
            }
        }
        let error = invoker
            .invoke("CronDelete", json!({"id":"missing"}), invocation_context())
            .await
            .unwrap_err();
        assert_eq!(
            error.model_tool_result_content(),
            "<tool_use_error>No scheduled job with id 'missing'</tool_use_error>"
        );
        let error = invoker
            .invoke(
                "CronCreate",
                json!({"cron":"invalid","prompt":"p"}),
                invocation_context(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.model_tool_result_content(), "<tool_use_error>Invalid cron expression 'invalid'. Expected 5 fields: M H DoM Mon DoW.</tool_use_error>");
    }

    #[test]
    fn latest_executable_oracle_schema_prompt_timing_and_coercion() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/schedule_wakeup_2_1_270.json"
        ))
        .unwrap();
        assert_eq!(*SCHEMA, oracle["inputSchema"]);
        assert_eq!(*OUTPUT_SCHEMA, oracle["outputSchema"]);
        for (name, ttl) in [
            ("oneHour", PromptCacheTtl::OneHour),
            ("fiveMinutes", PromptCacheTtl::FiveMinutes),
            ("unknown", PromptCacheTtl::Unknown),
        ] {
            assert_eq!(
                build_prompt(ttl).as_bytes(),
                oracle["prompts"][name].as_str().unwrap().as_bytes()
            );
        }
        for case in oracle["timing"].as_array().unwrap() {
            let raw = case["raw"].as_str().unwrap().parse::<f64>().unwrap();
            let actual = wakeup_target(raw, case["nowMs"].as_i64().unwrap());
            assert_eq!(
                actual.clamped_delay_seconds,
                case["clamped"].as_i64().unwrap(),
                "{case}"
            );
            assert_eq!(
                actual.was_clamped,
                case["wasClamped"].as_bool().unwrap(),
                "{case}"
            );
            assert_eq!(
                actual.target_ms,
                case["targetMs"].as_i64().unwrap(),
                "{case}"
            );
        }
        for case in oracle["coercion"].as_array().unwrap() {
            let actual = coerce_delay(Some(&case["input"]));
            assert_eq!(
                actual.is_ok(),
                case["accepted"].as_bool().unwrap(),
                "{case}"
            );
            if let Ok(Some(value)) = actual {
                assert_eq!(value, case["value"].as_f64().unwrap());
            }
        }
        for case in oracle["errors"].as_array().unwrap() {
            assert_eq!(
                validate_wakeup_input(&case["input"]).unwrap_err().0,
                case["message"].as_str().unwrap()
            );
        }
        for case in oracle["results"].as_array().unwrap().iter().take(3) {
            let result = if case["input"]["stopped"] == true {
                stopped_result(case["input"]["cancelledWakeups"].as_u64().unwrap() as usize)
            } else {
                zero_triple_result("")
            };
            assert_eq!(result.data["model_content"], case["content"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn latest_local_schedule_result_bytes() {
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/schedule_wakeup_2_1_270.json"
        ))
        .unwrap();
        if let Ok(zone) = std::env::var("LINGXI_WAKEUP_ORACLE_TZ") {
            for case in oracle["localResults"][&zone].as_array().unwrap() {
                let scheduled = ScheduledWakeup {
                    scheduled_for_ms: case["input"]["scheduledFor"].as_i64().unwrap(),
                    clamped_delay_seconds: case["input"]["clampedDelaySeconds"].as_i64().unwrap(),
                    was_clamped: case["input"]["wasClamped"].as_bool().unwrap(),
                };
                assert_eq!(
                    scheduled_wakeup_text(scheduled, case["nowMs"].as_i64().unwrap()),
                    case["content"].as_str().unwrap(),
                    "zone {zone} {case}"
                );
            }
            return;
        }
        // Each timezone runs in its own process: no mutation races with other tests.
        for zone in oracle["localResults"].as_object().unwrap().keys() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "wakeup::tests::latest_local_schedule_result_bytes",
                    "--nocapture",
                ])
                .env("LINGXI_WAKEUP_ORACLE_TZ", zone)
                .env("TZ", zone)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{zone}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn numeric_string_coercion_runs_before_schema_validation() {
        let tool = tool_with(Rec::new());
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/schedule_wakeup_2_1_270.json"
        ))
        .unwrap();
        for case in oracle["coercion"].as_array().unwrap() {
            let original =
                json!({"delaySeconds": case["input"], "reason":"r", "prompt":"p", "noop":false});
            let normalized = tool.coerce_input(&original);
            assert_eq!(
                normalized.is_some(),
                case["accepted"].as_bool().unwrap(),
                "{case}"
            );
            if let Some(normalized) = normalized {
                assert_eq!(
                    normalized.input["delaySeconds"].as_f64(),
                    case["value"].as_f64()
                );
                assert_eq!(normalized.input["reason"], original["reason"]);
                assert_eq!(normalized.input["prompt"], original["prompt"]);
                assert_eq!(normalized.input["noop"], original["noop"]);
                assert_eq!(normalized.shape_class, "delaySeconds");
                assert!(normalized.input["delaySeconds"].is_number());
            }
            assert!(
                original["delaySeconds"].is_string(),
                "input remains unchanged"
            );
        }
        assert!(tool.coerce_input(&json!({"delaySeconds":120})).is_none());
        assert!(tool.coerce_input(&json!({"stop":true})).is_none());
    }

    #[tokio::test]
    async fn description_search_hint_and_prompt_surface() {
        let tool = tool_with(Rec::new());
        assert_eq!(
            tool.description(&json!({}), &DescriptionOptions { is_non_interactive_session: false }).await,
            "Schedule when to resume work in /loop dynamic mode (always pass the `prompt` arg unless stopping). Call before ending the turn to keep the loop alive; call with `stop: true` to end the loop immediately."
        );
        assert_eq!(
            tool.search_hint(),
            Some("self-pace the dynamic /loop: pick a delay before the next tick, or stop/end/cancel the dynamic loop with stop:true (a fixed-interval /loop is a recurring cron — cancel it with CronDelete)")
        );
        assert_eq!(tool.max_result_size_chars(), 1000);
        assert!(tool.should_defer());
        let prompt = tool.prompt(&PromptOptions::default()).await;
        // Structure of `iZn`: intro, noop, delay guidance, reason — blank-line
        // separated, trailing newline. Lengths are the oracle-derived sizes of
        // the three variants (chars / bytes: 3148/3180, 3396/3433, 3197/3227).
        assert_eq!(prompt, build_prompt(PromptCacheTtl::Unknown));
        assert_eq!(prompt.chars().count(), 3197);
        assert_eq!(prompt.len(), 3227);
        assert_eq!(build_prompt(PromptCacheTtl::OneHour).len(), 3180);
        assert_eq!(build_prompt(PromptCacheTtl::FiveMinutes).len(), 3433);
        assert!(prompt.starts_with("Schedule when to resume work in /loop dynamic mode — the user invoked /loop without an interval"));
        assert!(prompt.contains("no further wakeups fire.\n\nSet `noop: true` if nothing changed"));
        assert!(prompt.contains("Omit `noop` when stopping (`stop: true`).\n\n## Picking delaySeconds\n\nThe Anthropic prompt cache decides how expensive a wake-up is"));
        assert!(prompt.ends_with("so you don't need to clamp yourself.\n\n## The reason field\n\nOne short sentence on what you chose and why. Goes to telemetry and is shown back to the user. \"watching CI run\" beats \"waiting.\" The user reads this to understand what you're doing without having to predict your cadence in advance — make it specific.\n"));
        assert!(
            build_prompt(PromptCacheTtl::FiveMinutes).contains("you don't burn cache 12× per hour")
        );
    }

    // PARITY 2.1.263 `ScheduleWakeupInputError` messages.
    #[tokio::test]
    async fn required_unless_stop() {
        let tool = tool_with(Rec::new());
        let tool = &tool;
        let err = |v: Value| async move {
            let _ = tool;
            validate_wakeup_input(&v).err().map(|e| e.0)
        };
        assert_eq!(
            err(json!({"reason": "r", "prompt": "p", "noop": true}))
                .await
                .as_deref(),
            Some("`delaySeconds` and `reason` are required when `stop` is not true.")
        );
        assert_eq!(
            err(json!({"delaySeconds": 120, "prompt": "p", "noop": true}))
                .await
                .as_deref(),
            Some("`delaySeconds` and `reason` are required when `stop` is not true.")
        );
        assert_eq!(
            err(json!({"delaySeconds": 120, "reason": "r", "noop": true}))
                .await
                .as_deref(),
            Some("`prompt` is required when `stop` is not true.")
        );
        assert_eq!(
            err(json!({"delaySeconds": 120, "reason": "r", "prompt": "p"}))
                .await
                .as_deref(),
            Some("`noop` is required when `stop` is not true.")
        );
        assert_eq!(
            err(json!({"delaySeconds": 120, "reason": "r", "prompt": "p", "noop": false})).await,
            None
        );
        // `DM(number)` coerces a numeric string; `stop: true` needs nothing else.
        assert_eq!(
            err(json!({"delaySeconds": "120", "reason": "r", "prompt": "p", "noop": true})).await,
            None
        );
        assert_eq!(err(json!({"stop": true})).await, None);
        assert!(
            err(json!({"delaySeconds": "soon", "reason": "r", "prompt": "p", "noop": true}))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn call_schedules_minute_aligned_wakeup_and_resets_keepalives() {
        let _s = serial();
        let rec = Rec::new();
        let tool = tool_with(rec.clone());
        al::set_loop_consecutive_keepalives(1);
        let before = now_epoch_ms();
        let out = tool
            .call(
                json!({"delaySeconds": 9999, "reason": "idle tick", "prompt": "5m /x", "noop": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        let scheduled_for = out.data["scheduledFor"].as_i64().unwrap();
        assert_eq!(scheduled_for % 60_000, 0, "whole-minute target");
        assert!(scheduled_for >= before + 3600 * 1000);
        assert_eq!(out.data["clampedDelaySeconds"], json!(3600));
        assert_eq!(out.data["wasClamped"], json!(true));
        let mc = out.data["model_content"].as_str().unwrap();
        assert!(mc.starts_with("Next wakeup scheduled for "));
        assert!(mc.contains("(clamped to 3600s from your requested value)"));
        assert!(mc.ends_with(". Nothing more to do this turn — the harness re-invokes you when the wakeup fires or a task-notification arrives."));
        let calls = rec.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0 >= Duration::from_secs(3600) && calls[0].0 <= Duration::from_secs(3660));
        assert_eq!(calls[0].1, "5m /x");
        assert_eq!(calls[0].2, "idle tick");
        drop(calls);
        assert_eq!(
            al::loop_consecutive_keepalives(),
            0,
            "a model call resets the keepalive budget"
        );
        assert!(al::take_loop_rescheduled());
        assert!(!al::loop_ended());
        let record = al::dynamic_loop_record("5m /x").unwrap();
        assert_eq!(record.last_scheduled_for_ms, scheduled_for);
        assert!(!record.aged_out);
    }

    #[tokio::test]
    async fn call_supersedes_pending_wakeups() {
        let _s = serial();
        let rec = Rec::new();
        let tool = tool_with(rec.clone());
        for _ in 0..2 {
            tool.call(
                json!({"delaySeconds": 120, "reason": "r", "prompt": "p", "noop": false}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        }
        // Each schedule first cancels what was pending (binary `x()`), so at
        // most one wakeup is ever armed.
        assert_eq!(rec.pending.lock().unwrap().len(), 1);
        assert_eq!(rec.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn stop_true_cancels_pending_and_ends_loop() {
        let _s = serial();
        let rec = Rec::new();
        let tool = tool_with(rec.clone());
        tool.call(
            json!({"delaySeconds": 120, "reason": "r", "prompt": "p", "noop": false}),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("ok");
        al::begin_loop_tick("p".into());
        al::set_loop_consecutive_keepalives(1);
        let out = tool
            .call(json!({"stop": true}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["stopped"], json!(true));
        assert_eq!(out.data["cancelledWakeups"], json!(1));
        assert_eq!(out.data["scheduledFor"], json!(0));
        assert_eq!(
            out.data["model_content"],
            json!("Loop stopped — cancelled 1 pending wakeup(s); no further dynamic-loop wakeups scheduled. If you armed a Monitor for this loop, TaskStop it now; otherwise nothing more to do this turn.")
        );
        assert!(al::loop_ended());
        assert_eq!(al::loop_consecutive_keepalives(), 0);
        assert!(
            al::loop_tick_in_flight_prompt().is_none(),
            "in-flight tick dropped"
        );
        // Nothing pending: the zero-count wording names the recurring-cron caveat.
        let again = tool
            .call(
                json!({"stop": true, "delaySeconds": 5}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(again.data["cancelledWakeups"], json!(0));
        assert_eq!(
            again.data["model_content"],
            json!("Loop stopped — any dynamic loop in this session is ended; there was no pending wakeup to cancel. If you are running a fixed-interval /loop (a recurring cron), it is NOT stopped by this call — cancel it with CronDelete. If you armed a Monitor for this loop, TaskStop it now; otherwise nothing more to do this turn.")
        );
        // Stopping without a wired scheduler still succeeds.
        let tmp = tempfile::tempdir().unwrap();
        let unwired =
            ScheduleWakeupTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = unwired
            .call(json!({"stop": true}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["cancelledWakeups"], json!(0));
    }

    /// PARITY `ZXn` → `Ort(prompt)` (`sessionCron.forgetChainStart`): stopping a
    /// loop drops the per-prompt chain-start record, so a later `/loop` on the
    /// same prompt is a NEW loop rather than one that inherits the stopped
    /// loop's `startedAt` (and would age out on its first wakeup).
    #[tokio::test]
    async fn stop_forgets_the_cancelled_loops_chain_start() {
        let _s = serial();
        let rec = Rec::new();
        let tool = tool_with(rec.clone());
        tool.call(
            json!({"delaySeconds": 120, "reason": "r", "prompt": "p", "noop": false}),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("ok");
        // Age the record past the 7-day cap while keeping its last wakeup recent,
        // so the restart-gap escape hatch (`S`) does NOT apply.
        let now = now_epoch_ms();
        al::set_dynamic_loop_record(
            "p",
            DynamicLoopRecord {
                started_at_ms: now - LOOP_MAX_AGE_MS - 1000,
                last_scheduled_for_ms: now,
                aged_out: false,
            },
        );

        let out = tool
            .call(json!({"stop": true}), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["cancelledWakeups"], json!(1));
        assert!(
            al::dynamic_loop_record("p").is_none(),
            "stop:true must forget the cancelled wakeup's chain start"
        );

        // Re-arming the same prompt schedules. Without the `Ort` sweep the stale
        // record survives, the loop reads as 7 days old and this returns the
        // aged-out zero triple instead.
        let out = tool
            .call(
                json!({"delaySeconds": 120, "reason": "r", "prompt": "p", "noop": false}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_ne!(
            out.data["scheduledFor"],
            json!(0),
            "a restarted loop must not inherit the stopped loop's age"
        );
    }

    #[tokio::test]
    async fn no_scheduler_returns_zero_triple() {
        let _s = serial();
        let tmp = tempfile::tempdir().unwrap();
        let tool =
            ScheduleWakeupTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(
                json!({"delaySeconds": 120, "reason": "r", "prompt": "p", "noop": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["scheduledFor"], json!(0));
        assert_eq!(out.data["clampedDelaySeconds"], json!(0));
        assert_eq!(out.data["wasClamped"], json!(false));
        assert_eq!(
            out.data["model_content"],
            json!("Wakeup not scheduled. The loop reached its maximum duration — the loop has ended; do not re-issue.")
        );
    }

    // PARITY 2.1.263 `E(...)`: a loop older than `recurringMaxAgeMs` (7 days)
    // since its first wakeup ends with `aged_out` and schedules nothing; a
    // loop whose last wakeup is more than an hour stale restarts its clock.
    #[tokio::test]
    async fn aged_out_loop_schedules_nothing_until_it_restarts() {
        let _s = serial();
        let rec = Rec::new();
        let tool = tool_with(rec.clone());
        let now = now_epoch_ms();
        al::set_dynamic_loop_record(
            "old",
            DynamicLoopRecord {
                started_at_ms: now - LOOP_MAX_AGE_MS - 1000,
                last_scheduled_for_ms: now - 10 * 60 * 1000,
                aged_out: false,
            },
        );
        let out = tool
            .call(
                json!({"delaySeconds": 120, "reason": "r", "prompt": "old", "noop": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["scheduledFor"], json!(0));
        assert!(rec.calls.lock().unwrap().is_empty());
        assert!(al::dynamic_loop_record("old").unwrap().aged_out);
        assert!(al::loop_ended());
        // A second call after aging out is silent (no double terminal event) and
        // still schedules nothing.
        let out = tool
            .call(
                json!({"delaySeconds": 120, "reason": "r", "prompt": "old", "noop": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["scheduledFor"], json!(0));
        assert!(rec.calls.lock().unwrap().is_empty());
        // Stale by more than an hour → treated as a fresh loop → schedules.
        al::set_dynamic_loop_record(
            "old",
            DynamicLoopRecord {
                started_at_ms: now - LOOP_MAX_AGE_MS - 1000,
                last_scheduled_for_ms: now - 2 * 60 * 60 * 1000,
                aged_out: true,
            },
        );
        let out = tool
            .call(
                json!({"delaySeconds": 120, "reason": "r", "prompt": "old", "noop": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_ne!(out.data["scheduledFor"], json!(0));
        assert_eq!(rec.calls.lock().unwrap().len(), 1);
        let record = al::dynamic_loop_record("old").unwrap();
        assert!(!record.aged_out);
        assert!(record.started_at_ms >= now);
    }

    #[tokio::test]
    async fn keepalive_arms_once_then_exhausts_budget() {
        let _s = serial();
        let rec = Rec::new();
        let sched: Arc<dyn WakeupScheduler> = rec.clone();
        assert_eq!(
            arm_keepalive(&sched, "<<autonomous-loop-dynamic>>").await,
            KeepaliveOutcome::Armed
        );
        {
            let calls = rec.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(
                calls[0].0 >= Duration::from_secs(1200) && calls[0].0 <= Duration::from_secs(1260)
            );
            assert_eq!(
                calls[0].1, "<<autonomous-loop-dynamic>>",
                "the keepalive re-arms the ORIGINAL sentinel"
            );
        }
        assert_eq!(al::loop_consecutive_keepalives(), 1);
        assert_eq!(
            arm_keepalive(&sched, "<<autonomous-loop-dynamic>>").await,
            KeepaliveOutcome::BudgetExhausted
        );
        assert_eq!(rec.calls.lock().unwrap().len(), 1);
        assert!(al::loop_ended());
    }

    #[tokio::test]
    async fn turn_end_keepalive_defaults_on_and_yields_to_a_model_reschedule() {
        let _s = serial();
        let rec = Rec::new();
        let sched: Arc<dyn WakeupScheduler> = rec.clone();
        // Not a loop tick → nothing.
        assert_eq!(maybe_arm_keepalive(&sched).await, None);
        // Loop tick, model silent → keepalive armed (gate default TRUE in 2.1.263).
        al::begin_loop_tick("5m /x".into());
        assert_eq!(
            maybe_arm_keepalive(&sched).await,
            Some(KeepaliveOutcome::Armed)
        );
        // Loop tick, model rescheduled → no keepalive.
        al::begin_loop_tick("5m /x".into());
        al::mark_loop_rescheduled();
        assert_eq!(maybe_arm_keepalive(&sched).await, None);
        // Gate off via the env var → no keepalive.
        al::begin_loop_tick("5m /x".into());
        std::env::set_var("CLAUDE_CODE_LOOP_KEEPALIVE", "");
        assert_eq!(maybe_arm_keepalive(&sched).await, None);
        std::env::remove_var("CLAUDE_CODE_LOOP_KEEPALIVE");
        assert_eq!(rec.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn user_abort_cancels_pending_and_ends_loop() {
        let _s = serial();
        let rec = Rec::new();
        let sched: Arc<dyn WakeupScheduler> = rec.clone();
        assert_eq!(cancel_dynamic_loop_on_user_abort(&sched).await, 0);
        assert!(!al::loop_ended(), "nothing to cancel → no terminal event");
        sched
            .schedule(Duration::from_secs(60), "p".into(), "r".into())
            .await;
        al::begin_loop_tick("p".into());
        assert_eq!(cancel_dynamic_loop_on_user_abort(&sched).await, 1);
        assert!(al::loop_ended());
        assert!(al::loop_tick_in_flight_prompt().is_none());
        note_loop_invoked(None);
        assert!(!al::loop_ended(), "`/loop` clears the ended marker");
    }

    #[test]
    fn sentinel_resolution() {
        let _s = serial();
        let resolved = resolve_wakeup_prompt("<<autonomous-loop-dynamic>>");
        assert!(resolved.starts_with("# Autonomous loop check\n\n"));
        assert!(resolved.contains("\n\n---\n\n# Autonomous loop tick (dynamic pacing)\n\n"));
        assert_eq!(resolve_wakeup_prompt("5m /babysit-prs"), "5m /babysit-prs");
        for text in [
            " <<autonomous-loop-dynamic>> ",
            "\u{feff}<<loop.md-dynamic>>",
            "<<loop.md-dynamic>>\n",
        ] {
            assert_eq!(
                resolve_wakeup_prompt(text),
                text,
                "sentinel matching is exact, not trimmed"
            );
        }
        assert_eq!(
            resolve_wakeup_prompt("  5m /x  "),
            "  5m /x  ",
            "real prompts are never trimmed"
        );
    }

    #[test]
    fn local_hhmmss_is_within_a_day() {
        let s = local_hhmmss(1_700_000_000_000);
        let parts: Vec<i64> = s.split(':').map(|p| p.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0] < 24 && parts[1] < 60 && parts[2] < 60);
    }
}
