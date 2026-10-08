use super::js_to_fixed_2;
use crate::headless::control_plane::StdioControlPlane;
use crate::headless::exit_codes;
use crate::headless::output::OutputSink;
use crate::headless::HeadlessRuntime as Runtime;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;

/// Claude Code 2.1.217 print-loop budget cleanup (`Wam` + `rcr`). After every
/// main turn, compare the cumulative cost directly with `--max-budget-usd` and
/// stop every running background local agent/workflow once the ceiling is
/// reached. This deliberately does not depend on the main turn returning
/// `error_max_budget_usd`: a natural `end_turn` can itself push the cumulative
/// cost over the ceiling.
pub(super) async fn stop_background_agents_at_budget(
    max_budget_usd: Option<f64>,
    orchestrator: &dyn OrchestratorHandle,
    task_registry: &dyn lingxi_core::host::task_registry::TaskRegistryHandle,
    diagnostics: &crate::headless::io::Output,
) -> usize {
    let Some(max_budget_usd) = max_budget_usd else {
        return 0;
    };
    let cost = orchestrator.snapshot_cost().await;
    if !budget_reached(max_budget_usd, cost.total_nano_usd) {
        return 0;
    }
    let notice = budget_halt_notice(cost.total_usd, max_budget_usd);
    let announced = std::sync::atomic::AtomicBool::new(false);
    let announce = || {
        announced.store(true, std::sync::atomic::Ordering::Relaxed);
    };
    let Ok(stopped) = task_registry
        .stop_background_agents_for_budget(&announce)
        .await
    else {
        return 0;
    };
    if announced.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = diagnostics.write_line(&notice).await;
    }
    stopped
}

/// Print mode remains alive for delegated work and shells, then tears down
/// connection-owned monitors/parked workers before returning to its caller.
pub(super) async fn wind_down_print_tasks(
    runtime: &Runtime,
    max_budget_usd: Option<f64>,
    shutdown: tokio_util::sync::CancellationToken,
    control_plane: Option<&Arc<StdioControlPlane>>,
    mut publisher: Option<&mut super::NativeQueryResultPublisher<'_>>,
) -> Result<(), orchestrator::OrchestratorError> {
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let cost = runtime.orchestrator.snapshot_cost().await;
        if max_budget_usd.is_some_and(|limit| budget_reached(limit, cost.total_nano_usd)) {
            break;
        }
        let records = lingxi_core::host::task_registry::TaskRegistryHandle::list(
            runtime.task_registry.as_ref(),
            lingxi_core::host::task_registry::TaskListFilter::default(),
        )
        .await
        .unwrap_or_default();
        let has_work = records.iter().any(print_task_keeps_session_alive);
        // Monitors are subscriptions, not jobs that can finish naturally. Stop
        // them before draining their final batch once no finite work remains.
        if !has_work {
            stop_print_subscriptions(runtime).await;
        }
        if runtime
            .task_registry
            .has_pending_task_notifications_for(None)
            .await
        {
            let _operation = match control_plane {
                Some(plane) => Some(plane.lock_operation().await),
                None => None,
            };
            if let Some(publisher) = publisher.as_deref_mut() {
                publisher.begin_notification_query().await;
            }
            let cancel = shutdown.child_token();
            if let Some(plane) = control_plane {
                plane.set_active_turn(cancel.clone()).await;
            }
            let result = runtime
                .orchestrator
                .run_task_notification_rewake(runtime.task_registry.as_ref(), cancel)
                .await;
            runtime.execution_interrupted.store(matches!(&result, Ok(orchestrator::TurnOutcome::Cancelled)), std::sync::atomic::Ordering::Release);
            if let Some(plane) = control_plane {
                plane.clear_active_turn().await;
            }
            if let Some(publisher) = publisher.as_deref_mut() {
                publisher.publish(&result, 0).await;
            }
            match result {
                Err(error) => {
                    stop_print_tasks(runtime).await;
                    return Err(error);
                }
                Ok(orchestrator::conversation::TurnOutcome::Cancelled) => break,
                Ok(_) => {}
            }
            continue;
        }
        if !has_work {
            break;
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
    }
    stop_print_tasks(runtime).await;
    Ok(())
}

pub(super) fn print_task_keeps_session_alive(
    task: &lingxi_core::host::task_registry::TaskRecord,
) -> bool {
    lingxi_core::host::task_activity::is_active_delegated_task(task)
        || (lingxi_core::host::task_activity::is_live_shell_task(task)
            && task.kind.as_deref() != Some("monitor"))
}

pub(super) async fn stop_print_subscriptions(runtime: &Runtime) {
    stop_print_tasks_matching(runtime, true).await;
}

pub(super) async fn stop_print_tasks(runtime: &Runtime) {
    stop_print_tasks_matching(runtime, false).await;
}

pub(super) async fn stop_print_tasks_matching(runtime: &Runtime, subscriptions_only: bool) {
    if let Ok(records) = lingxi_core::host::task_registry::TaskRegistryHandle::list(
        runtime.task_registry.as_ref(),
        lingxi_core::host::task_registry::TaskListFilter::default(),
    )
    .await
    {
        for task in records.into_iter().filter(|task| {
            (matches!(
                task.status.as_str(),
                "running" | "pending" | "paused" | "queued"
            ) || task.is_parked)
                && (!subscriptions_only
                    || task.kind.as_deref() == Some("monitor")
                    || matches!(task.task_type.as_str(), "monitor_mcp" | "monitor_ws"))
        }) {
            let _ = runtime.task_registry.mark_notified(&task.task_id).await;
            let _ = runtime
                .task_registry
                .kill_with_reason(&task.task_id, "system")
                .await;
        }
    }
}

pub(super) fn budget_reached(max_budget_usd: f64, total_nano_usd: u64) -> bool {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let budget_nano_usd = (max_budget_usd.max(0.0) * 1_000_000_000.0) as u64;
    total_nano_usd >= budget_nano_usd
}

pub(super) fn budget_halt_notice(total_usd: f64, max_budget_usd: f64) -> String {
    let total_usd = js_to_fixed_2(total_usd);
    format!("Budget limit reached (${total_usd} of ${max_budget_usd}); stopping background agents.")
}

pub(super) async fn run_print_owned<F>(runtime: &Runtime, operation: F) -> i32
where
    F: std::future::Future<Output = i32>,
{
    run_print_owned_with_cleanup(runtime, operation, futures::future::ready(())).await
}

pub(super) async fn run_print_owned_with_cleanup<F, C>(
    runtime: &Runtime,
    operation: F,
    cleanup: C,
) -> i32
where
    F: std::future::Future<Output = i32>,
    C: std::future::Future<Output = ()>,
{
    // The owning service injects cancellation into the existing turn driver.
    // Continue polling accepted execution until tools and receipts unwind.
    let code = operation.await;
    runtime
        .execution_code
        .store(code, std::sync::atomic::Ordering::Release);
    cleanup.await;
    finish_oneshot_lifecycle(runtime, code).await
}

#[derive(Default)]
pub(crate) struct PrintAuxTaskGroup {
    pub(super) tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl PrintAuxTaskGroup {
    pub(crate) fn push(&self, task: tokio::task::JoinHandle<()>) {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
    }

    pub(crate) async fn abort_and_join(&self) {
        loop {
            let tasks = std::mem::take(
                &mut *self
                    .tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if tasks.is_empty() { break; }
            for task in &tasks {
                task.abort();
            }
            for task in tasks {
                let _ = task.await;
            }
            // An admitted task can register an owned child while this batch
            // is being joined. Drain that batch before releasing the group.
        }
    }

    pub(crate) async fn join(&self) {
        loop {
            let tasks = std::mem::take(
                &mut *self
                    .tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if tasks.is_empty() { break; }
            for task in tasks {
                let _ = task.await;
            }
        }
    }
}

/// Every branch that can invoke tools owns the same print shutdown boundary.
pub(super) async fn finish_print_branch(
    runtime: &Runtime,
    budget: Option<f64>,
    sink: &dyn OutputSink,
    code: i32,
) -> i32 {
    if code != exit_codes::SUCCESS {
        stop_print_tasks(runtime).await;
        return code;
    }
    match wind_down_print_tasks(runtime, budget, runtime.shutdown.child_token(), None, None).await {
        Ok(()) => code,
        Err(error) => {
            runtime.record_execution_error(&error);
            sink.error("runtime", &error.to_string()).await;
            exit_codes::RUNTIME_ERROR
        }
    }
}

pub(super) async fn finish_oneshot_lifecycle(runtime: &Runtime, code: i32) -> i32 {
    let report = runtime.session_lifecycle.shutdown_and_drain().await;
    runtime
        .shutdown_complete
        .store(report.complete, std::sync::atomic::Ordering::Release);
    runtime
        .failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(report.errors.iter().cloned());
    if !report.complete && report.errors.is_empty() {
        runtime
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push("Session shutdown drain was incomplete".into());
    }
    for error in &report.errors {
        let _ = runtime
            .output
            .write_line(&format!(
                "lingxi-cli: session shutdown persistence failed: {error}"
            ))
            .await;
    }
    if code == exit_codes::SUCCESS && !report.errors.is_empty() {
        exit_codes::RUNTIME_ERROR
    } else {
        code
    }
}
