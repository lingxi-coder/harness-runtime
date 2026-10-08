//! Shell stdout event monitor (`monitor_ws`).

use crate::handlers::{NoopStatusSink, TaskStatusSink};
use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use async_trait::async_trait;
use lingxi_core::host::{
    BackgroundTaskHandle, ProcessCommand, ProcessRunner, RuntimeSpawner, Sandbox,
};
use mobile_linux_api::{ProcessError, ProcessStreamSink};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

const HANDLER_NAME: &str = "monitor_ws";
const BYPASS_REASON: &str = "monitor_task";
/// Audit reason when the command arrived ALREADY wrapped by the tool's
/// `shouldUseSandbox` decision — the construction is a bypass, the command is
/// not. Distinguishing the two is what keeps the audit log honest.
const CONFINED_REASON: &str = "monitor_task_sandbox_wrapped";
const BATCH_WINDOW: Duration = Duration::from_millis(200);
const TOKEN_CAPACITY: f64 = 10.0;
const TOKEN_REFILL_SECS: f64 = 2.0;
const HIGH_VOLUME_STOP: Duration = Duration::from_secs(30);
/// Oracle `aEe*3` — how long since the LAST SUPPRESSED event the high-volume
/// window is forgiven. Keyed on the last suppression, not the last batch: a
/// monitor that keeps emitting at a sustainable rate must not have its window
/// reset by its own well-behaved traffic.
const HIGH_VOLUME_FORGIVE: Duration = Duration::from_secs(6);
/// Upper bound on how long the worker waits for `TaskRegistry::spawn` to publish
/// its registry row before giving up. Registration normally lands microseconds
/// after `spawn` returns; a wait this long means the spawn future was dropped
/// (turn/session teardown), so the worker exits instead of busy-polling forever.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Oracle `cFe` — per-line cap, measured AFTER trimming.
const MAX_EVENT_CHARS: usize = 500;
/// Oracle `SHn` — cap on the joined batch.
const MAX_BATCH_CHARS: usize = 3000;
/// Oracle `Umn`'s per-line suffix. The batch suffix is NOT the same string —
/// it carries a leading newline (`\n...(truncated)`), because it is appended
/// after a `\n`-joined block rather than mid-line.
const TRUNCATION_SUFFIX: &str = "...(truncated)";
/// Oracle marker delivered on a monitor's timeout, before the kill. The dash is
/// U+2014 EM DASH with one ASCII space either side.
const TIMEOUT_MARKER: &str = "[Monitor timed out \u{2014} re-arm if needed.]";

/// 2.1.286 `l7()`: bounded Monitor rollout, enabled by default.
pub fn bounded_monitors_enabled() -> bool {
    telemetry::flag_bool("tengu_breezy_crescent", true)
}

/// 2.1.270 `Vye()`: ten minutes for single-shot print, thirty otherwise.
pub fn bounded_monitor_timeout_ms() -> u64 {
    if lingxi_core::host::session_flags::is_single_shot_print_session() {
        600_000
    } else {
        1_800_000
    }
}

/// Upstream `Lt(ms, {hideTrailingZeros:true})` formatting.
pub fn monitor_duration(ms: u64) -> String {
    let text = lingxi_core::host::shell_support::format_duration_ms(ms);
    let mut parts: Vec<_> = text.split(' ').collect();
    while parts.len() > 1 && parts.last().is_some_and(|part| part.starts_with('0')) {
        parts.pop();
    }
    parts.join(" ")
}

fn timeout_notice(timeout_ms: u64, events: usize, bounded: bool) -> String {
    if !bounded {
        return TIMEOUT_MARKER.into();
    }
    let duration = monitor_duration(timeout_ms);
    if events == 0 {
        format!("[Monitor expired after {duration} with no events delivered. Re-arm it if you still need the watch — and widen the filter if silence was unexpected.]")
    } else {
        let noun = if events == 1 { "event" } else { "events" };
        format!("[Monitor expired after {duration} with {events} {noun} delivered. Re-arm it if you still need the watch.]")
    }
}

/// Truncate to `limit` UTF-16 code units, returning `None` when nothing needed
/// cutting.
///
/// The oracle measures with JS `String.length` and cuts with `slice`, both of
/// which count UTF-16 code units — an emoji is 2. Counting `char`s instead
/// would let a line of astral text through at twice the intended size. The cut
/// lands on a whole `char`, which is also what the oracle's slice achieves for
/// any well-formed input.
fn truncate_utf16(value: &str, limit: usize) -> Option<String> {
    let total: usize = value.chars().map(char::len_utf16).sum();
    if total <= limit {
        return None;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in value.chars() {
        let width = c.len_utf16();
        if used + width > limit {
            break;
        }
        used += width;
        out.push(c);
    }
    Some(out)
}

struct WorkerCancel {
    handle: BackgroundTaskHandle,
    flush_handle: Option<BackgroundTaskHandle>,
    runtime: Arc<dyn RuntimeSpawner>,
}

struct BatchState {
    pending: Vec<String>,
    flush_scheduled: bool,
    tokens: f64,
    last_refill: Instant,
    /// Oracle `E` — when an event was last SUPPRESSED. Drives the high-volume
    /// window's forgiveness.
    last_suppressed: Option<Instant>,
    high_volume_since: Option<Instant>,
    suppressed: usize,
    delivered: usize,
    stop_reason: Option<String>,
}

impl Default for BatchState {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            flush_scheduled: false,
            tokens: TOKEN_CAPACITY,
            last_refill: Instant::now(),
            last_suppressed: None,
            high_volume_since: None,
            suppressed: 0,
            delivered: 0,
            stop_reason: None,
        }
    }
}

/// What one flush tick decided to do with the batched output — the PURE result
/// of the token-bucket / suppression / high-volume-stop logic (no I/O), so it is
/// unit-testable in isolation.
#[derive(Debug, PartialEq, Eq)]
enum FlushOutcome {
    /// Deliver this event to the model via `notify_monitor_event`.
    ///
    /// `notice` is the housekeeping suppression line. The oracle sends it as
    /// its OWN `GM` call BEFORE the batch (`R6t`: two calls, notice first), not
    /// merged into the batch string — the two carry different
    /// `isHousekeeping` flags, so merging them would attach the
    /// PushNotification nudge to a housekeeping line.
    Deliver {
        notice: Option<String>,
        event: String,
    },
    /// Deliver this stop message to the model, THEN cancel the task (the oracle
    /// `cvo` delivers via `dY(...)` before `killTask()` → terminal status Killed).
    Stop(String),
    /// Rate-limited with nothing to report this tick.
    Nothing,
}

impl BatchState {
    /// Pure token-bucket + suppression decision for one flush tick (mirrors the
    /// oracle `cvo` onData body). Refills the bucket, drains `pending`, and
    /// returns what to deliver. Caller has already confirmed `pending` is
    /// non-empty and the task is not cancelled.
    fn flush_decision(&mut self, now: Instant) -> FlushOutcome {
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed / TOKEN_REFILL_SECS).min(TOKEN_CAPACITY);
        self.last_refill = now;
        // Oracle `C=o.join("\n"); if(C.length>SHn) C=C.slice(0,SHn)+"\n...(truncated)"`.
        let batch = truncate_utf16(&self.pending.join("\n"), MAX_BATCH_CHARS).map_or_else(
            || self.pending.join("\n"),
            |cut| format!("{cut}\n{TRUNCATION_SUFFIX}"),
        );
        self.pending.clear();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            self.delivered += 1;
            // Oracle `if(p>0){ if(GM(notice), p=0, E!==void 0 && now-E > aEe*3) _=void 0 }`.
            // That head is a COMMA EXPRESSION: the notice is emitted and the
            // count zeroed UNCONDITIONALLY, and only the window reset is
            // guarded — by time since the last SUPPRESSION, not since the last
            // batch. Reading it as three statements inverts the reset.
            let notice = if self.suppressed > 0 {
                let suppressed = std::mem::take(&mut self.suppressed);
                if self
                    .last_suppressed
                    .is_some_and(|last| now.duration_since(last) > HIGH_VOLUME_FORGIVE)
                {
                    self.high_volume_since = None;
                }
                Some(format!(
                    "[{suppressed} events suppressed — output rate too high. Consider using TaskStop to restart this monitor with a more selective filter.]"
                ))
            } else {
                None
            };
            FlushOutcome::Deliver {
                notice,
                event: batch,
            }
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            self.last_suppressed = Some(now);
            let started = *self.high_volume_since.get_or_insert(now);
            let window = now.duration_since(started);
            if window > HIGH_VOLUME_STOP {
                // Oracle stop message, DELIVERED before the kill. `${s}` is
                // `Math.round((now - windowStart) / 1000)`.
                let secs = window.as_secs_f64().round() as u64;
                let stop = format!(
                    "[Monitor stopped — too much output ({} events suppressed over {secs}s). Restart with a more selective source.]",
                    self.suppressed
                );
                self.stop_reason = Some(stop.clone());
                FlushOutcome::Stop(stop)
            } else {
                FlushOutcome::Nothing
            }
        }
    }
}

#[derive(Clone)]
struct MonitorStreamSink {
    task_id: String,
    output_file: PathBuf,
    output_manager: Arc<TaskOutputManager>,
    status_sink: Arc<dyn TaskStatusSink>,
    cancel: CancellationToken,
    batch: Arc<Mutex<BatchState>>,
    flush_notify: Arc<Notify>,
    /// Bytes seen on STDOUT (claude-code `pipedStdoutBytes`). Counted from the
    /// SPOOLED form — line plus its newline — because that is the raw chunk
    /// upstream measures; counting the stripped line would read a stream of
    /// bare newlines as no output at all. Stderr is deliberately not counted.
    stdout_bytes: Arc<std::sync::atomic::AtomicU64>,
}

impl MonitorStreamSink {
    /// Oracle `if(I.length>cFe) I=I.slice(0,cFe)+"...(truncated)"`, applied to
    /// the ALREADY-TRIMMED line.
    fn truncate_line(line: &str) -> String {
        truncate_utf16(line, MAX_EVENT_CHARS).map_or_else(
            || line.to_string(),
            |cut| format!("{cut}{TRUNCATION_SUFFIX}"),
        )
    }

    async fn flush(&self) {
        let now = Instant::now();
        let outcome = {
            let mut state = self.batch.lock().await;
            state.flush_scheduled = false;
            if state.pending.is_empty() || self.cancel.is_cancelled() {
                state.pending.clear();
                return;
            }
            state.flush_decision(now)
        };
        match outcome {
            FlushOutcome::Deliver { notice, event } => {
                // Two separate notifications, housekeeping notice FIRST.
                if let Some(notice) = notice {
                    self.status_sink
                        .notify_monitor_event(&self.task_id, &notice, true)
                        .await;
                }
                self.status_sink
                    .notify_monitor_event(&self.task_id, &event, false)
                    .await;
            }
            // High-volume auto-stop: deliver the stop message to the model FIRST,
            // then cancel — the terminal status becomes Killed (epilogue).
            FlushOutcome::Stop(event) => {
                self.status_sink
                    .notify_monitor_event(&self.task_id, &event, false)
                    .await;
                self.cancel.cancel();
            }
            FlushOutcome::Nothing => {}
        }
    }

    async fn stop_reason(&self) -> Option<String> {
        self.batch.lock().await.stop_reason.clone()
    }
}

#[async_trait]
impl ProcessStreamSink for MonitorStreamSink {
    async fn stdout_line(&self, line: String) -> Result<(), ProcessError> {
        let mut spool = line.clone();
        spool.push('\n');
        self.stdout_bytes
            .fetch_add(spool.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.output_manager
            .append(&self.output_file, &spool)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        // Oracle `let I=r.slice(0,C).trim(); … if(I){…o.push(I)}` — TRIM
        // FIRST, then drop an empty line, then measure. Trimming after the
        // measurement would truncate a short payload padded with trailing
        // whitespace.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Still spooled to the output file above; just not an event.
            return Ok(());
        }
        let mut state = self.batch.lock().await;
        // No pending cap: the oracle bounds the batch by CHARACTERS (`SHn`), not
        // by line count, and counting an over-cap line as "suppressed" fed the
        // high-volume auto-stop with events the model was never rate-limited
        // out of.
        state
            .pending
            .push(MonitorStreamSink::truncate_line(trimmed));
        // Oracle `if(o.length>0&&!d) d=t(p)` — schedule only once something is
        // actually queued.
        let notify = !state.flush_scheduled;
        state.flush_scheduled = true;
        drop(state);
        if notify {
            self.flush_notify.notify_one();
        }
        Ok(())
    }

    async fn stderr_chunk(&self, chunk: Vec<u8>) -> Result<(), ProcessError> {
        self.output_manager
            .append(&self.output_file, &String::from_utf8_lossy(&chunk))
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))
    }
}
/// Real handler for the public Monitor tool.
pub struct MonitorHandler {
    process: Arc<dyn ProcessRunner>,
    sandbox: Arc<dyn Sandbox>,
    output_manager: Arc<TaskOutputManager>,
    status_sink: Arc<dyn TaskStatusSink>,
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
}

impl MonitorHandler {
    /// Construct a monitor handler.
    #[must_use]
    pub fn new(
        process: Arc<dyn ProcessRunner>,
        sandbox: Arc<dyn Sandbox>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            process,
            sandbox,
            output_manager,
            status_sink: Arc::new(NoopStatusSink),
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach the registry-backed lifecycle sink.
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    fn classify(result: &Result<mobile_linux_api::ProcessOutput, ProcessError>) -> TaskStatus {
        match result {
            Ok(output) if output.exit_code == 0 => TaskStatus::Completed,
            Ok(_) => TaskStatus::Failed,
            Err(ProcessError::Timeout) => TaskStatus::Killed,
            Err(_) => TaskStatus::Failed,
        }
    }

    /// Terminal status for a finished monitor worker. A cancellation — whether an
    /// explicit `kill()` or the high-volume auto-stop — goes through the oracle's
    /// `killTask()`, which sets the status to Killed (NOT Failed, even though the
    /// cancelled `run_streaming` resolves to an `Err`). Only a natural end is
    /// classified from the process result.
    /// Whether the timeout marker is owed (claude-code's timeout handler:
    /// `if(A.isKilled())return; GM(marker); JF(...)`).
    ///
    /// Three terms, each load-bearing:
    /// - `had_deadline`: a PERSISTENT monitor arms no timer upstream, so it can
    ///   never produce this marker. It still meets the process runner's own
    ///   default deadline here, which is why the flag is needed rather than
    ///   just looking at the error.
    /// - the error really is a timeout, not a failure or a stop.
    /// - not already cancelled — the oracle's `isKilled()` early return. A
    ///   monitor stopped by the high-volume rule already said why.
    fn timeout_marker_due(
        had_deadline: bool,
        result: &Result<mobile_linux_api::ProcessOutput, ProcessError>,

        cancelled: bool,
    ) -> bool {
        had_deadline && matches!(result, Err(ProcessError::Timeout)) && !cancelled
    }

    fn terminal_status(
        cancelled: bool,
        result: &Result<mobile_linux_api::ProcessOutput, ProcessError>,
    ) -> TaskStatus {
        if cancelled {
            TaskStatus::Killed
        } else {
            Self::classify(result)
        }
    }
}

impl MonitorHandler {
    async fn spawn_websocket(
        &self,
        source: crate::task_trait::WebSocketMonitorInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        use lingxi_core::host::http::MonitorWebSocketFrame;
        let task_id = crate::id::generate_task_id(TaskType::Monitor);
        let output_file = self
            .output_manager
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        let cancel = CancellationToken::new();
        let sink = Arc::new(MonitorStreamSink {
            task_id: task_id.clone(),
            output_file,
            output_manager: self.output_manager.clone(),
            status_sink: self.status_sink.clone(),
            cancel: cancel.clone(),
            batch: Arc::new(Mutex::new(BatchState::default())),
            flush_notify: Arc::new(Notify::new()),
            stdout_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        let worker_id = task_id.clone();
        let worker_cancel = cancel.clone();
        let workers = self.workers.clone();
        let status = self.status_sink.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let future = async move {
            if ready_rx.await.is_err() {
                workers.lock().await.remove(&worker_id);
                return;
            }
            let reg = source.registration;
            let bounded = bounded_monitors_enabled();
            let timed = !reg.task.persistent && reg.task.timeout_ms != 0;
            let deadline = tokio::time::sleep(Duration::from_millis(if timed {
                reg.task.timeout_ms
            } else {
                3_600_000
            }));
            tokio::pin!(deadline);
            let result = tokio::select! {
                biased;
                _ = worker_cancel.cancelled() => None,
                _ = &mut deadline, if timed => {
                    if bounded { sink.flush().await; }
                    // Read the counter first: a guard created inside the argument
                    // list lives until the end of the statement, i.e. across the
                    // `notify_monitor_event` await, blocking the flush worker.
                    let delivered = sink.batch.lock().await.delivered;
                    status.notify_monitor_event(&worker_id, &timeout_notice(reg.task.timeout_ms, delivered, bounded), true).await;
                    None
                },
                result = source.http.monitor_websocket(reg.url, reg.protocols) => Some(result),
            };
            if let Some(result) = result {
                match result {
                    Err(error) => {
                        let event = match error {
                            lingxi_core::host::HttpError::Status { status, .. } => {
                                format!("[WebSocket upgrade rejected: HTTP {status}]")
                            }
                            other => format!("[WebSocket error: {other}]"),
                        };
                        status.notify_monitor_event(&worker_id, &event, true).await;
                        status
                            .notify_monitor_event(&worker_id, "[WebSocket closed: 1006]", true)
                            .await;
                    }
                    Ok(mut socket) => {
                        let mut flush = tokio::time::interval(BATCH_WINDOW);
                        loop {
                            tokio::select! {
                                biased;
                                _ = worker_cancel.cancelled() => break,
                                _ = &mut deadline, if timed => {
                                    if bounded { sink.flush().await; }
                                    if worker_cancel.is_cancelled() { break; }
                                    let delivered = sink.batch.lock().await.delivered;
                                    status.notify_monitor_event(&worker_id, &timeout_notice(reg.task.timeout_ms, delivered, bounded), true).await;
                                    break;
                                },
                                _ = flush.tick() => sink.flush().await,
                                frame = socket.recv() => match frame {
                                    Some(Ok(MonitorWebSocketFrame::Text(text))) => {
                                        let _ = sink.stdout_line(text).await;
                                    },
                                    Some(Ok(MonitorWebSocketFrame::Oversized(size))) => {
                                        status.notify_monitor_event(&worker_id, &format!("[Dropped {size}-byte frame (exceeds 1048576); closing]"), true).await;
                                        break;
                                    },
                                    Some(Ok(MonitorWebSocketFrame::Binary(size))) => {
                                        let _ = sink.stdout_line(format!("[binary frame, {size} bytes]")).await;
                                    },
                                    Some(Ok(MonitorWebSocketFrame::Closed(code, reason))) => {
                                        sink.flush().await;
                                        let reason = if reason.is_empty() { reason } else { format!(" {reason}") };
                                        status.notify_monitor_event(&worker_id, &format!("[WebSocket closed: {code}{reason}]"), true).await;
                                        break;
                                    },
                                    Some(Err(error)) => {
                                        status.notify_monitor_event(&worker_id, &format!("[WebSocket error: {error}]"), true).await;
                                        status.notify_monitor_event(&worker_id, "[WebSocket closed: 1006]", true).await;
                                        break;
                                    },
                                    None => break,
                                },
                            }
                        }
                    }
                }
            }
            sink.flush().await;
            // The close/timeout event is the whole notification; no second generic completion.
            if let Some(registry) = status.task_registry() {
                let _ = registry.publish_task_stopped(&worker_id).await;
            } else {
                status.set_status(&worker_id, TaskStatus::Killed).await;
            }
            workers.lock().await.remove(&worker_id);
        };
        let mut workers = self.workers.lock().await;
        let handle = ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:ws:{task_id}"), Box::pin(future))
            .await
            .map_err(|error| TaskError::Internal(error.to_string()))?;
        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle,
                flush_handle: None,
                runtime: ctx.runtime,
            },
        );
        Ok(
            TaskHandle::new(task_id, Some(Arc::new(move || cancel.cancel()))).with_activation(
                move || {
                    let _ = ready_tx.send(());
                },
            ),
        )
    }
}

#[async_trait]
impl Task for MonitorHandler {
    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::Monitor
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        if let TaskSpawnInput::MonitorWs(source) = input {
            return self.spawn_websocket(source, ctx).await;
        }
        let TaskSpawnInput::Monitor {
            command,
            spawn_command,
            timeout,
            cwd,
            tool_use_id: _,
            creator_teammate_name: _,
            creator_team_name: _,
            creator_agent_id: _,
        } = input
        else {
            return Err(TaskError::Internal(
                "monitor handler received a non-Monitor input".into(),
            ));
        };
        // 2.1.263 command monitors are local_bash tasks; Monitor remains
        // the internal handler key so TaskStop cancels the streaming worker.
        let task_id = crate::id::generate_task_id(TaskType::LocalBash);
        let output_file = self
            .output_manager
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        // A persistent monitor arrives with no deadline; the posix runner still
        // applies one of its own, so remember which kind this is before the
        // value is moved into the command.
        let had_deadline = timeout.is_some();
        let timeout_ms = timeout.map_or(0, |duration| duration.as_millis() as u64);
        let bounded = bounded_monitors_enabled();
        // MON-09: the Monitor TOOL made the `shouldUseSandbox` decision and, when
        // it said confine, handed us the wrapped command. The bypass below is
        // then only about how the `SandboxedCommand` is CONSTRUCTED — the
        // confinement already lives in the string, exactly as it does on the
        // Bash tool's sandboxed path.
        let confined = spawn_command.is_some();
        let to_spawn = spawn_command.unwrap_or(command);
        let sandboxed = self.sandbox.bypass_with_audit(
            ProcessCommand {
                command: "bash".into(),
                args: vec!["-c".into(), to_spawn],
                cwd,
                env: HashMap::new(),
                timeout,
                stdin: None,
            },
            if confined {
                CONFINED_REASON
            } else {
                BYPASS_REASON
            },
        );
        let cancel = CancellationToken::new();
        let sink = Arc::new(MonitorStreamSink {
            task_id: task_id.clone(),
            output_file,
            output_manager: self.output_manager.clone(),
            status_sink: self.status_sink.clone(),
            cancel: cancel.clone(),
            batch: Arc::new(Mutex::new(BatchState::default())),
            flush_notify: Arc::new(Notify::new()),
            stdout_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        // One batching worker per monitor. A per-batch RuntimeSpawner task would
        // retain a completed JoinHandle every 200ms for the lifetime of the
        // session; this single worker is cancelled on every exit path.
        let flush_sink = sink.clone();
        let flush_runtime = ctx.runtime.clone();
        let flush_cancel = CancellationToken::new();
        let flush_worker_cancel = flush_cancel.clone();
        let flush_handle = ctx
            .runtime
            .spawn(
                &format!("{HANDLER_NAME}:flush:{task_id}"),
                Box::pin(async move {
                    loop {
                        tokio::select! {
                            () = flush_worker_cancel.cancelled() => break,
                            () = flush_sink.flush_notify.notified() => {
                                flush_runtime.sleep(BATCH_WINDOW).await;
                                flush_sink.flush().await;
                            }
                        }
                    }
                }),
            )
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;
        let process = self.process.clone();
        let status_sink = self.status_sink.clone();
        let workers = self.workers.clone();
        let worker_id = task_id.clone();
        let worker_sink = sink.clone();
        let worker_cancel = cancel.clone();
        let runtime = ctx.runtime.clone();
        let worker_runtime = ctx.runtime.clone();
        let worker_flush_handle = flush_handle.clone();
        let worker = Box::pin(async move {
            // `TaskRegistry::spawn` can only insert the handler-generated id
            // after this method returns. Wait for that publication so an
            // `echo ready` monitor cannot finish before its event/status has a
            // registry row to update — but BOUND the wait and honor cancellation
            // so a dropped `TaskRegistry::spawn` future (turn/session teardown)
            // cannot leave this detached worker busy-polling the registry every
            // 1ms forever.
            let registered = {
                let wait = async {
                    while !status_sink.is_registered(&worker_id).await {
                        runtime.sleep(Duration::from_millis(1)).await;
                    }
                };
                tokio::select! {
                    () = wait => true,
                    () = worker_cancel.cancelled() => false,
                    () = runtime.sleep(REGISTRATION_TIMEOUT) => false,
                }
            };
            if !registered {
                // Never published (cancelled or timed out). Tear down the paired
                // flush worker, drop our map entry, and exit. The monitored
                // command was never started, so nothing runs unwatched.
                flush_cancel.cancel();
                let _ = worker_runtime.cancel(&worker_flush_handle).await;
                workers.lock().await.remove(&worker_id);
                return;
            }
            status_sink
                .set_status(&worker_id, TaskStatus::Running)
                .await;
            let stream_sink: Arc<dyn ProcessStreamSink> = worker_sink.clone();
            let result = tokio::select! {
                result = process.run_streaming(&sandboxed, stream_sink) => result,
                () = worker_cancel.cancelled() => {
                    Err(ProcessError::Io(worker_sink.stop_reason().await.unwrap_or_else(|| "monitor cancelled".into())))
                }
            };
            worker_sink.flush().await;
            // claude-code's timeout handler is `if(A.isKilled())return;
            // GM(…,"[Monitor timed out — re-arm if needed.]",…); JF(taskId,…)`
            // — say so, THEN kill. Without this the monitor just goes terminal
            // and the model is left with no reason and no hint that re-arming
            // is the move.
            //
            // Gated on the monitor having had a real deadline: a PERSISTENT
            // monitor arms no timer upstream, but here it still meets the posix
            // runner's own default deadline, so `Err(Timeout)` alone would fire
            // the marker exactly where the oracle is silent.
            if MonitorHandler::timeout_marker_due(
                had_deadline,
                &result,
                worker_cancel.is_cancelled(),
            ) {
                let delivered = worker_sink.batch.lock().await.delivered;
                status_sink
                    .notify_monitor_event(
                        &worker_id,
                        &timeout_notice(timeout_ms, delivered, bounded),
                        true,
                    )
                    .await;
                worker_cancel.cancel();
            }
            // Before the terminal status, so the drain that reads the row can
            // already see it (same ordering rule as the exit code).
            status_sink
                .set_monitor_stdout_bytes(
                    &worker_id,
                    worker_sink
                        .stdout_bytes
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
                .await;
            if let Ok(output) = &result {
                status_sink
                    .set_exit_code(&worker_id, output.exit_code)
                    .await;
            }
            let status = MonitorHandler::terminal_status(worker_cancel.is_cancelled(), &result);
            status_sink.set_status(&worker_id, status).await;
            flush_cancel.cancel();
            let _ = worker_runtime.cancel(&worker_flush_handle).await;
            workers.lock().await.remove(&worker_id);
        });
        let mut workers = self.workers.lock().await;
        let handle = match ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:{task_id}"), worker)
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                let _ = ctx.runtime.cancel(&flush_handle).await;
                return Err(TaskError::Internal(error.to_string()));
            }
        };
        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle,
                flush_handle: Some(flush_handle),
                runtime: ctx.runtime.clone(),
            },
        );
        drop(workers);

        let cleanup_cancel = cancel;
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || cleanup_cancel.cancel());
        Ok(TaskHandle::new(task_id, Some(cleanup)))
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        // Drop the map guard before awaiting cancellation. The main worker's
        // epilogue removes this same entry, so retaining the guard across
        // `RuntimeSpawner::cancel` can deadlock while cancel waits for the
        // worker future to finish.
        let worker = { self.workers.lock().await.remove(task_id) };
        if let Some(worker) = worker {
            let main_result = worker.runtime.cancel(&worker.handle).await;
            let flush_result = if let Some(flush_handle) = worker.flush_handle {
                worker.runtime.cancel(&flush_handle).await
            } else {
                Ok(())
            };
            if let Err(error) = main_result.or(flush_result) {
                return Err(TaskError::Io(error.to_string()));
            }
        }
        if !self.status_sink.is_terminal(task_id).await {
            self.status_sink
                .set_status(task_id, TaskStatus::Killed)
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
    use lingxi_core::host::sandbox::{SandboxCapability, SandboxedTag};
    use lingxi_core::host::{
        ProcessCommand, ProcessHandle, SandboxError, SandboxPolicy, SandboxedCommand,
    };
    use mobile_linux_api::ProcessOutput;
    use mobile_linux_api::SandboxBackend;
    use std::collections::HashMap as StdHashMap;
    use std::sync::Mutex as StdMutex;
    use test_harness::mocks::MockRuntimeSpawner;
    use tokio::sync::Mutex as TokioMutex;

    // ── Pure token-bucket / suppression / high-volume-stop logic ────────────

    /// A `BatchState` at `now` with a given token count and suppressed count,
    /// `last_refill`/`last_suppressed` pinned to `now` (no incidental refill,
    /// and the high-volume window is NOT forgiven unless a test moves
    /// `last_suppressed` back).
    fn state(now: Instant, tokens: f64, suppressed: usize) -> BatchState {
        let mut s = BatchState::default();
        s.tokens = tokens;
        s.last_refill = now;
        s.last_suppressed = Some(now);
        s.suppressed = suppressed;
        s
    }

    #[test]
    fn flush_decision_batches_pending_into_one_event() {
        let now = Instant::now();
        let mut s = state(now, 10.0, 0);
        s.pending = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(
            s.flush_decision(now),
            FlushOutcome::Deliver {
                notice: None,
                event: "a\nb\nc".to_string()
            }
        );
        assert!(s.pending.is_empty(), "pending is drained");
    }

    #[test]
    fn flush_decision_reports_suppression_notice_byte_exact() {
        let now = Instant::now();
        let mut s = state(now, 5.0, 5);
        s.pending = vec!["out".into()];
        // The notice is its OWN event, delivered BEFORE the batch — not spliced
        // onto the end of it. Upstream sends two `GM` calls with different
        // `isHousekeeping` flags, so merging them would attach the
        // PushNotification nudge to a housekeeping line.
        assert_eq!(
            s.flush_decision(now),
            FlushOutcome::Deliver {
                notice: Some(
                    "[5 events suppressed \u{2014} output rate too high. Consider using TaskStop to restart this monitor with a more selective filter.]"
                        .to_string()
                ),
                event: "out".to_string(),
            }
        );
        assert_eq!(s.suppressed, 0, "count is zeroed ONLY when reported");
    }

    #[test]
    fn flush_decision_rate_limited_suppresses_and_starts_window() {
        let now = Instant::now();
        let mut s = state(now, 0.0, 0);
        s.high_volume_since = None;
        s.pending = vec!["x".into()];
        assert_eq!(s.flush_decision(now), FlushOutcome::Nothing);
        assert_eq!(s.suppressed, 1);
        assert!(s.high_volume_since.is_some(), "high-volume window started");
        assert_eq!(
            s.last_suppressed,
            Some(now),
            "and the suppression is stamped"
        );
    }

    #[test]
    fn flush_decision_high_volume_stop_delivers_byte_exact_message() {
        // `now` is 100s after `base`; the window opened at base+69s → 31s wide.
        let base = Instant::now();
        let now = base + Duration::from_secs(100);
        let mut s = state(now, 0.0, 42);
        s.high_volume_since = Some(base + Duration::from_secs(69));
        s.pending = vec!["x".into()];
        assert_eq!(
            s.flush_decision(now),
            FlushOutcome::Stop(
                "[Monitor stopped \u{2014} too much output (43 events suppressed over 31s). Restart with a more selective source.]"
                    .to_string()
            )
        );
        assert!(s.stop_reason.is_some());
    }

    /// The high-volume window is forgiven ONLY on a tick that actually reports
    /// a suppression notice, and only when >6s (`aEe*3`) have passed since the
    /// LAST SUPPRESSION.
    ///
    /// Upstream writes this as a comma expression —
    /// `if(GM(notice), p=0, E!==void 0 && now-E > aEe*3) _=void 0` — where only
    /// the reset is guarded. Reading it as three statements, or keying the
    /// forgiveness on the last BATCH, resets the window from a monitor's own
    /// well-behaved traffic and defers the auto-stop indefinitely. An earlier
    /// version of this test pinned exactly that.
    #[test]
    fn the_high_volume_window_is_forgiven_only_by_a_gap_in_suppression() {
        let base = Instant::now();
        let now = base + Duration::from_secs(100);

        // Reporting a notice after a 7s suppression gap forgives the window.
        let mut s = state(now, 5.0, 3);
        s.last_suppressed = Some(base + Duration::from_secs(93));
        s.high_volume_since = Some(base + Duration::from_secs(50));
        s.pending = vec!["x".into()];
        assert!(matches!(
            s.flush_decision(now),
            FlushOutcome::Deliver {
                notice: Some(_),
                ..
            }
        ));
        assert_eq!(s.high_volume_since, None, "forgiven after a 7s quiet gap");

        // The same report with only a 3s gap does NOT forgive it.
        let mut s = state(now, 5.0, 3);
        s.last_suppressed = Some(base + Duration::from_secs(97));
        let opened = base + Duration::from_secs(50);
        s.high_volume_since = Some(opened);
        s.pending = vec!["x".into()];
        let _ = s.flush_decision(now);
        assert_eq!(s.high_volume_since, Some(opened), "3s is not enough");

        // And a delivery with NOTHING suppressed never touches the window,
        // however long the gap — this is the half that keyed on the last batch.
        let mut s = state(now, 5.0, 0);
        s.last_suppressed = Some(base);
        s.high_volume_since = Some(opened);
        s.pending = vec!["x".into()];
        assert_eq!(
            s.flush_decision(now),
            FlushOutcome::Deliver {
                notice: None,
                event: "x".to_string()
            }
        );
        assert_eq!(
            s.high_volume_since,
            Some(opened),
            "a clean batch must not forgive the window",
        );
    }

    /// `Umn`'s two truncation suffixes are DIFFERENT strings: the per-line one
    /// has no newline, the batch one does.
    #[test]
    fn the_two_truncation_suffixes_are_not_the_same_string() {
        let long_line = "x".repeat(MAX_EVENT_CHARS + 10);
        let truncated = MonitorStreamSink::truncate_line(&long_line);
        assert_eq!(
            truncated,
            format!("{}...(truncated)", "x".repeat(MAX_EVENT_CHARS))
        );
        assert!(!truncated.contains('\u{2026}'), "no ellipsis character");

        // A line exactly at the cap is untouched.
        let exact = "y".repeat(MAX_EVENT_CHARS);
        assert_eq!(MonitorStreamSink::truncate_line(&exact), exact);

        // The batch cap carries a LEADING newline.
        let now = Instant::now();
        let mut s = state(now, 10.0, 0);
        s.pending = vec!["z".repeat(MAX_BATCH_CHARS + 50)];
        let FlushOutcome::Deliver { event, .. } = s.flush_decision(now) else {
            panic!("expected a delivery")
        };
        assert_eq!(
            event,
            format!("{}\n...(truncated)", "z".repeat(MAX_BATCH_CHARS))
        );
    }

    /// The cap counts UTF-16 code units, as JS `String.length` does — an emoji
    /// is 2. Counting chars would let a line through at twice the size.
    #[test]
    fn truncation_counts_utf16_code_units() {
        // 300 astral chars = 600 code units ⇒ over the 500 cap.
        let astral = "\u{1F600}".repeat(300);
        let out = MonitorStreamSink::truncate_line(&astral);
        assert!(out.ends_with("...(truncated)"));
        let kept: usize = out
            .trim_end_matches("...(truncated)")
            .chars()
            .map(char::len_utf16)
            .sum();
        assert_eq!(kept, MAX_EVENT_CHARS);
    }

    #[test]
    fn truncate_line_caps_at_max_event_chars() {
        let long = "x".repeat(MAX_EVENT_CHARS + 50);
        let t = MonitorStreamSink::truncate_line(&long);
        // The suffix is the oracle's literal, not a single ellipsis character.
        assert_eq!(t.chars().count(), MAX_EVENT_CHARS + TRUNCATION_SUFFIX.len());
        assert!(t.ends_with(TRUNCATION_SUFFIX));
        assert_eq!(MonitorStreamSink::truncate_line("hello"), "hello");
    }

    /// The stdout byte count that selects the completion summary. Counted from
    /// the SPOOLED form — line plus newline — because that is the raw chunk
    /// upstream measures: a script emitting bare newlines HAS produced output,
    /// and counting stripped lines would report zero. Stderr is not counted.
    #[tokio::test]
    async fn stdout_bytes_are_counted_from_the_spooled_form() {
        let sink = Arc::new(RecordingSink::default());
        let status_sink: Arc<dyn TaskStatusSink> = sink.clone();
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let stream = MonitorStreamSink {
            task_id: "s1".into(),
            output_file: std::path::PathBuf::from("/spool/s1.output"),
            output_manager: Arc::new(TaskOutputManager::new(
                std::path::PathBuf::from("/spool"),
                fs,
            )),
            status_sink,
            cancel: CancellationToken::new(),
            batch: Arc::new(Mutex::new(BatchState::default())),
            flush_notify: Arc::new(Notify::new()),
            stdout_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let read = || {
            stream
                .stdout_bytes
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        assert_eq!(read(), 0, "a monitor that never emits stays at zero");

        stream.stdout_line("abc".into()).await.unwrap();
        assert_eq!(read(), 4, "three bytes plus the newline");

        // An EMPTY line is dropped as an event but is still output.
        stream.stdout_line(String::new()).await.unwrap();
        assert_eq!(read(), 5, "a bare newline still counts");

        // Stderr is not stdout.
        stream.stderr_chunk(b"boom".to_vec()).await.unwrap();
        assert_eq!(read(), 5, "stderr must not be counted");
    }

    /// claude-code says why the monitor died, THEN kills it. Persistent
    /// monitors arm no timer upstream, so they must never produce the marker
    /// even though they do meet the process runner's own default deadline.
    #[test]
    fn the_timeout_marker_is_owed_only_by_a_real_deadline() {
        let timeout: Result<mobile_linux_api::ProcessOutput, ProcessError> =
            Err(ProcessError::Timeout);
        assert!(MonitorHandler::timeout_marker_due(true, &timeout, false));

        // Persistent monitor: the runner still times it out, the oracle is silent.
        assert!(!MonitorHandler::timeout_marker_due(false, &timeout, false));
        // Already stopped (high-volume rule) — it already said why.
        assert!(!MonitorHandler::timeout_marker_due(true, &timeout, true));
        // Any other ending is not a timeout.
        assert!(!MonitorHandler::timeout_marker_due(
            true,
            &Err(ProcessError::Io("boom".into())),
            false
        ));
    }

    #[test]
    fn the_timeout_marker_is_byte_exact() {
        assert_eq!(
            TIMEOUT_MARKER,
            "[Monitor timed out \u{2014} re-arm if needed.]"
        );
        // An em dash, not a hyphen — the two look alike in a diff.
        assert!(TIMEOUT_MARKER.contains('\u{2014}'));
        assert!(!TIMEOUT_MARKER.contains(" - "));
    }

    /// The suppression notice reaches the model as its OWN notification,
    /// BEFORE the batch. Asserting the delivered CALLS, not just the decision:
    /// dropping the notice from `flush` leaves the decision test green.
    #[tokio::test]
    async fn flush_delivers_the_suppression_notice_before_the_batch() {
        let sink = Arc::new(RecordingSink::default());
        let status_sink: Arc<dyn TaskStatusSink> = sink.clone();
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let now = Instant::now();
        let mut batch = BatchState::default();
        batch.tokens = 5.0;
        batch.last_refill = now;
        batch.last_suppressed = Some(now);
        batch.suppressed = 4;
        batch.pending = vec!["line".into()];

        let stream = MonitorStreamSink {
            task_id: "s1".into(),
            output_file: std::path::PathBuf::from("/tmp/s1.output"),
            output_manager: Arc::new(TaskOutputManager::new(
                std::path::PathBuf::from("/spool"),
                fs,
            )),
            status_sink,
            cancel: CancellationToken::new(),
            batch: Arc::new(Mutex::new(batch)),
            flush_notify: Arc::new(Notify::new()),
            stdout_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        stream.flush().await;

        let events = sink.events();
        assert_eq!(events.len(), 2, "two notifications, got: {events:?}");
        assert!(
            events[0].starts_with("[4 events suppressed"),
            "the housekeeping notice comes FIRST, got: {:?}",
            events[0]
        );
        assert_eq!(events[1], "line", "then the batch, unmerged");
    }

    #[test]
    fn terminal_status_treats_any_cancellation_as_killed() {
        // The high-volume auto-stop cancels with an Err result — it must be
        // Killed (NOT Failed, the pre-fix bug).
        assert_eq!(
            MonitorHandler::terminal_status(true, &Err(ProcessError::Io("stopped".into()))),
            TaskStatus::Killed
        );
        // A natural end is classified from the exit code.
        let ok0 = Ok(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        assert_eq!(
            MonitorHandler::terminal_status(false, &ok0),
            TaskStatus::Completed
        );
        let ok1 = Ok(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 1,
            timed_out: false,
        });
        assert_eq!(
            MonitorHandler::terminal_status(false, &ok1),
            TaskStatus::Failed
        );
    }

    // ── Handler-level (spawn → run → terminal status) ───────────────────────

    struct MockRunner {
        output: ProcessOutput,
    }
    impl MockRunner {
        fn new(stdout: &str, exit_code: i32) -> Arc<Self> {
            Arc::new(Self {
                output: ProcessOutput {
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                    exit_code,
                    timed_out: false,
                },
            })
        }
    }
    #[async_trait]
    impl ProcessRunner for MockRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            Ok(self.output.clone())
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// A `run()` that parks forever — stands in for a long-running monitored
    /// command until the future is cancelled.
    struct BlockingRunner;
    impl BlockingRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self)
        }
    }
    #[async_trait]
    impl ProcessRunner for BlockingRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            std::future::pending::<()>().await;
            unreachable!()
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    struct StubSandbox;
    #[async_trait]
    impl Sandbox for StubSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::None
        }
        fn prepare(
            &self,
            cmd: ProcessCommand,
            _policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, SandboxError> {
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: "test".into(),
                },
            ))
        }
        fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: reason.into(),
                },
            )
        }
        async fn probe_capability(&self) -> SandboxCapability {
            SandboxCapability {
                available: true,
                reason: None,
                features: lingxi_core::host::SandboxFeatures::default(),
            }
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        statuses: StdMutex<Vec<TaskStatus>>,
        events: StdMutex<Vec<String>>,
    }
    #[async_trait]
    impl TaskStatusSink for RecordingSink {
        async fn set_status(&self, _task_id: &str, status: TaskStatus) {
            self.statuses.lock().unwrap().push(status);
        }
        async fn notify_monitor_event(&self, _task_id: &str, event: &str, _housekeeping: bool) {
            self.events.lock().unwrap().push(event.to_string());
        }
    }
    impl RecordingSink {
        fn last_status(&self) -> Option<TaskStatus> {
            self.statuses.lock().unwrap().last().copied()
        }
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    // Minimal in-memory FileSystem (the spool target); output_manager only
    // exercises create/append here.
    struct InMemoryFs {
        files: TokioMutex<StdHashMap<String, String>>,
    }
    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: TokioMutex::new(StdHashMap::new()),
            }
        }
    }
    #[async_trait]
    impl FileSystem for InMemoryFs {
        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            let map = self.files.lock().await;
            let content = map.get(path).cloned().unwrap_or_default();
            let total_lines = content.lines().count() as u64;
            Ok(FileContent {
                content,
                truncated: false,
                total_lines,
            })
        }
        async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .insert(path.to_string(), body.to_string());
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError>
        {
            Err(FsError::Io("not supported".into()))
        }
        async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .entry(path.to_string())
                .or_default()
                .push_str(body);
            Ok(())
        }
        async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
            Ok(())
        }
        async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            let map = self.files.lock().await;
            Ok(map.get(path).map_or(0, |s| s.len() as u64))
        }
        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.files.lock().await.remove(path);
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
            Ok(())
        }
        async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("not supported".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), FsError> {
            Ok(())
        }
    }

    /// A status sink whose task row is NEVER published — exercises the worker's
    /// bounded/cancellable registration wait.
    #[derive(Default)]
    struct NeverRegisteredSink;
    #[async_trait]
    impl TaskStatusSink for NeverRegisteredSink {
        async fn set_status(&self, _task_id: &str, _status: TaskStatus) {}
        async fn is_registered(&self, _task_id: &str) -> bool {
            false
        }
    }

    fn make_handler(
        process: Arc<dyn ProcessRunner>,
        sink: Arc<dyn TaskStatusSink>,
    ) -> (MonitorHandler, TaskContext) {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let mgr = Arc::new(TaskOutputManager::new(PathBuf::from("/spool"), fs.clone()));
        let handler =
            MonitorHandler::new(process, Arc::new(StubSandbox), mgr).with_status_sink(sink);
        let ctx = TaskContext {
            fs,
            runtime: Arc::new(MockRuntimeSpawner::default()),
        };
        (handler, ctx)
    }

    fn monitor_input() -> TaskSpawnInput {
        TaskSpawnInput::Monitor {
            command: "echo hi".into(),
            spawn_command: None,
            timeout: None,
            cwd: None,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        }
    }

    async fn await_terminal(sink: &Arc<RecordingSink>) -> TaskStatus {
        for _ in 0..5000 {
            if let Some(s) = sink.last_status() {
                if s.is_terminal() {
                    return s;
                }
            }
            tokio::task::yield_now().await;
        }
        panic!("worker never reported a terminal status");
    }

    #[tokio::test]
    async fn natural_completion_reports_completed_and_delivers_events() {
        let sink = Arc::new(RecordingSink::default());
        let (handler, ctx) = make_handler(MockRunner::new("hello\nworld\n", 0), sink.clone());
        let handle = handler.spawn(monitor_input(), ctx).await.unwrap();
        assert!(
            handle.task_id.starts_with('b'),
            "command monitor must use bash id: {}",
            handle.task_id
        );
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| e.contains("hello") && e.contains("world")),
            "batched output must reach the model; got {events:?}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_reports_failed() {
        let sink = Arc::new(RecordingSink::default());
        let (handler, ctx) = make_handler(MockRunner::new("", 1), sink.clone());
        let _handle = handler.spawn(monitor_input(), ctx).await.unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn cancellation_reports_killed_not_failed() {
        let sink = Arc::new(RecordingSink::default());
        let (handler, ctx) = make_handler(BlockingRunner::new(), sink.clone());
        let handle = handler.spawn(monitor_input(), ctx).await.unwrap();
        // Let the worker register, mark Running, and park in run_streaming.
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        // The cleanup closure cancels the token; the worker's cancelled arm runs
        // its epilogue → Killed (the pre-fix code reported Failed once a
        // stop_reason was set).
        (handle.cleanup.expect("cleanup closure"))();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn worker_exits_when_registration_never_occurs() {
        // The registry row is never published, so the worker's registration wait
        // would busy-poll forever (the #47 bug). With the bounded/cancellable
        // wait it must observe the cancellation and EXIT — proven by the worker
        // dropping its own entry from the handler's `workers` map.
        let (handler, ctx) = make_handler(BlockingRunner::new(), Arc::new(NeverRegisteredSink));
        let handle = handler.spawn(monitor_input(), ctx).await.unwrap();
        assert_eq!(
            handler.workers.lock().await.len(),
            1,
            "the worker is registered in the handler map while it waits"
        );
        // Let the worker reach its registration wait, then cancel it.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        (handle.cleanup.expect("cleanup closure"))();
        for _ in 0..5000 {
            if handler.workers.lock().await.is_empty() {
                return; // the worker exited the wait and tore itself down
            }
            tokio::task::yield_now().await;
        }
        panic!("worker never exited the registration wait after cancellation");
    }
    struct SocketHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for SocketHttp {
        async fn request(
            &self,
            _: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            unreachable!()
        }
        async fn stream_sse(
            &self,
            _: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            unreachable!()
        }
        async fn monitor_websocket(
            &self,
            url: String,
            protocols: Vec<String>,
        ) -> Result<lingxi_core::host::http::MonitorWebSocketReceiver, lingxi_core::host::HttpError>
        {
            assert_eq!(url, "wss://events.example.com/feed");
            assert_eq!(protocols, vec!["v1"]);
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tx.send(Ok(lingxi_core::host::http::MonitorWebSocketFrame::Text(
                "one\ntwo".into(),
            )))
            .await
            .unwrap();
            tx.send(Ok(lingxi_core::host::http::MonitorWebSocketFrame::Binary(
                17,
            )))
            .await
            .unwrap();
            tx.send(Ok(lingxi_core::host::http::MonitorWebSocketFrame::Closed(
                1000,
                "done".into(),
            )))
            .await
            .unwrap();
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn websocket_handler_streams_frames_and_reports_close_after_events() {
        let sink = Arc::new(RecordingSink::default());
        let (handler, ctx) = make_handler(BlockingRunner::new(), sink.clone());
        let mut handle = handler
            .spawn(
                TaskSpawnInput::MonitorWs(crate::task_trait::WebSocketMonitorInput {
                    registration: lingxi_core::host::task_registry::WebSocketMonitorRegistration {
                        url: "wss://events.example.com/feed".into(),
                        protocols: vec!["v1".into()],
                        task: lingxi_core::host::task_registry::MonitorRegistration {
                            persistent: true,
                            ..Default::default()
                        },
                    },
                    http: Arc::new(SocketHttp),
                }),
                ctx,
            )
            .await
            .unwrap();
        assert!(handle.task_id.starts_with('s'));
        assert!(
            sink.events().is_empty(),
            "socket cannot open before registry activation"
        );
        handle.activate();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Killed);
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|event| event.contains("one\ntwo")
                    && event.contains("[binary frame, 17 bytes]")),
            "{events:?}"
        );
        assert_eq!(
            events.last().map(String::as_str),
            Some("[WebSocket closed: 1000 done]")
        );
    }
    #[test]
    fn bounded_expiry_notices_match_270_bytes() {
        assert_eq!(timeout_notice(300_000, 0, false), TIMEOUT_MARKER);
        assert_eq!(timeout_notice(1_800_000, 0, true), "[Monitor expired after 30m with no events delivered. Re-arm it if you still need the watch — and widen the filter if silence was unexpected.]");
        assert_eq!(timeout_notice(600_000, 1, true), "[Monitor expired after 10m with 1 event delivered. Re-arm it if you still need the watch.]");
        assert_eq!(timeout_notice(61_000, 2, true), "[Monitor expired after 1m 1s with 2 events delivered. Re-arm it if you still need the watch.]");
        let mut batch = BatchState::default();
        batch.pending = vec!["one".into(), "two".into()];
        assert!(matches!(
            batch.flush_decision(Instant::now()),
            FlushOutcome::Deliver { .. }
        ));
        assert_eq!(
            batch.delivered, 1,
            "count delivered notifications, not stdout lines"
        );
    }
    struct TimeoutRunner;
    #[async_trait]
    impl ProcessRunner for TimeoutRunner {
        async fn run(&self, _: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            Err(ProcessError::Timeout)
        }
        async fn spawn_background(
            &self,
            _: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn deadline_emits_notice_before_killed_status() {
        let sink = Arc::new(RecordingSink::default());
        let (handler, ctx) = make_handler(Arc::new(TimeoutRunner), sink.clone());
        let mut input = monitor_input();
        if let TaskSpawnInput::Monitor { timeout, .. } = &mut input {
            *timeout = Some(Duration::from_secs(300));
        }
        handler.spawn(input, ctx).await.unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Killed);
        assert_eq!(sink.events(), vec![timeout_notice(300_000, 0, true)]);
    }
    #[test]
    fn high_volume_stop_requires_more_than_thirty_seconds() {
        let now = Instant::now();
        let mut batch = state(now, 0.0, 42);
        batch.high_volume_since = Some(now - Duration::from_secs(30));
        batch.pending = vec!["at boundary".into()];
        assert_eq!(batch.flush_decision(now), FlushOutcome::Nothing);
        batch.pending = vec!["past boundary".into()];
        assert!(matches!(
            batch.flush_decision(now + Duration::from_millis(1)),
            FlushOutcome::Stop(_)
        ));
    }
}
