//! A transport-injected print and bidirectional SDK service over the desktop
//! composition root and its existing execution, permission and session owners.
mod cleanup;
pub mod config;
pub mod control_plane;
pub mod exit_codes;
pub mod host;
pub mod io;
pub mod output;
pub mod output_adapter;
pub mod permission_prompt_notify;
pub mod queued_commands;
pub mod remote_ui;
pub mod resume_truncation;
mod run;
mod sdk_mcp_transport;
pub mod session;
mod stdio;
pub mod stream_json;
pub mod stream_json_input;
pub mod structured_output;

pub use cleanup::{HeadlessCleanup, HeadlessCleanupStatus};
pub use config::{
    HeadlessConfig, HeadlessOptions, ImageSource, InputFormat, OutputFormat, PermissionMode,
    SessionStart, Utf16JsonProjection,
};
pub use host::{HeadlessHostServices, OAuthDescriptorCredential};
pub use io::HeadlessIo;
use lingxi_core::host::OutputStream;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The frozen native distribution documented in headless-native-baseline.json.
/// This identifies the protocol target, not a claim of completed parity.
pub const CLAUDE_CODE_REFERENCE_VERSION: &str = "2.1.293";

/// Notifications supplied by the embedding host. No process signal handlers
/// are installed by the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessSignal {
    Interrupt,
    Shutdown,
    /// The injected output transport has disconnected.
    OutputClosed,
    Terminate {
        signal: i32,
    },
}

pub struct HeadlessSignals {
    receiver: tokio::sync::mpsc::UnboundedReceiver<HeadlessSignal>,
}
impl HeadlessSignals {
    pub fn new(receiver: tokio::sync::mpsc::UnboundedReceiver<HeadlessSignal>) -> Self {
        Self { receiver }
    }
    pub fn channel() -> (tokio::sync::mpsc::UnboundedSender<HeadlessSignal>, Self) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (sender, Self::new(receiver))
    }
}
impl Default for HeadlessSignals {
    fn default() -> Self {
        Self::channel().1
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadlessExitReason {
    Completed,
    InvalidInput,
    RuntimeError,
    Interrupted,
    Terminated { signal: i32 },
    OutputError,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadlessExecutionStatus {
    NotStarted,
    Succeeded,
    Failed,
    Interrupted,
    Terminated { signal: i32 },
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadlessShutdownStatus {
    NotStarted,
    Complete,
    Incomplete,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadlessDeliveryStatus {
    Complete,
    Failed,
}

/// Internal service outcome; this does not add fields to the native wire.
#[derive(Clone, Debug)]
pub struct HeadlessExit {
    pub code: i32,
    pub reason: HeadlessExitReason,
    pub session_id: Option<String>,
    pub errors: Vec<String>,
    pub execution: HeadlessExecutionStatus,
    pub shutdown: HeadlessShutdownStatus,
    pub delivery: HeadlessDeliveryStatus,
    pub shutdown_errors: Vec<String>,
    pub delivery_errors: Vec<String>,
    /// Present when the initial drain was incomplete. The retained supervisor
    /// continues cleanup even if this diagnostic handle is dropped.
    pub cleanup: Option<HeadlessCleanup>,
}

pub(crate) struct HeadlessRuntime {
    inner: crate::desktop::DesktopRuntime,
    pub(crate) services: Arc<dyn HeadlessHostServices>,
    pub(crate) output: io::Output,
    pub(crate) stdout: io::Output,
    pub(crate) shutdown: CancellationToken,
    active_turn: Mutex<CancellationToken>,
    failures: Mutex<Vec<String>>,
    execution_errors: Arc<Mutex<Vec<String>>>,
    max_turns: Option<u32>,
    execution_code: std::sync::atomic::AtomicI32,
    execution_interrupted: std::sync::atomic::AtomicBool,
    shutdown_complete: std::sync::atomic::AtomicBool,
}
impl std::ops::Deref for HeadlessRuntime {
    type Target = crate::desktop::DesktopRuntime;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl HeadlessRuntime {
    fn record_execution_error(&self, error: impl ToString) {
        self.execution_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(error.to_string());
    }

    pub(crate) fn turn_cancel(&self) -> CancellationToken {
        let token = self.shutdown.child_token();
        *self
            .active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = token.clone();
        token
    }
    fn interrupt(&self) {
        self.active_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }
}

/// Execute an accepted headless session. The task owns execution and durable
/// cleanup, so dropping this waiter does not drop a running tool or its receipt.
pub async fn run(
    config: HeadlessConfig,
    services: Arc<dyn HeadlessHostServices>,
    io: HeadlessIo,
    signals: HeadlessSignals,
) -> HeadlessExit {
    let waiter_cancel = CancellationToken::new();
    let _waiter_guard = CancelWaiterOnDrop(waiter_cancel.clone());
    match tokio::spawn(run_owned(config, services, io, signals, waiter_cancel)).await {
        Ok(exit) => exit,
        Err(error) => HeadlessExit {
            code: exit_codes::RUNTIME_ERROR,
            reason: HeadlessExitReason::RuntimeError,
            session_id: None,
            errors: vec![error.to_string()],
            execution: HeadlessExecutionStatus::Failed,
            shutdown: HeadlessShutdownStatus::Incomplete,
            delivery: HeadlessDeliveryStatus::Complete,
            shutdown_errors: vec![error.to_string()],
            delivery_errors: Vec::new(),
            cleanup: None,
        },
    }
}

struct CancelWaiterOnDrop(CancellationToken);
impl Drop for CancelWaiterOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct SignalWatcher(Option<tokio::task::JoinHandle<()>>);
impl Drop for SignalWatcher {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
impl SignalWatcher {
    async fn stop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

struct HeadlessPermissionRequestSink;
struct HeadlessDesktopDiagnostics(io::Output);
#[async_trait::async_trait]
impl crate::desktop::DesktopDiagnosticSink for HeadlessDesktopDiagnostics {
    async fn stderr_line(&self, line: &str) {
        let _ = self.0.write_line(line).await;
    }
}

#[async_trait::async_trait]
impl client::adapter::PermissionRequestSink for HeadlessPermissionRequestSink {
    async fn emit_request(&self, _request: client::protocol::permission::PermissionRequest) {
        // Headless always installs either its control gate or deny-on-ask.
        debug_assert!(false, "headless permission transport was not installed");
    }
}

async fn run_owned(
    mut config: HeadlessConfig,
    services: Arc<dyn HeadlessHostServices>,
    mut io: HeadlessIo,
    mut signals: HeadlessSignals,
    waiter_cancel: CancellationToken,
) -> HeadlessExit {
    let mut exit = HeadlessExit {
        code: 0,
        reason: HeadlessExitReason::Completed,
        session_id: None,
        errors: Vec::new(),
        execution: HeadlessExecutionStatus::NotStarted,
        shutdown: HeadlessShutdownStatus::NotStarted,
        delivery: HeadlessDeliveryStatus::Complete,
        shutdown_errors: Vec::new(),
        delivery_errors: Vec::new(),
        cleanup: None,
    };
    let input_format = config.options.input_format;
    let max_turns = config.desktop.max_turns;
    let last_signal = Arc::new(Mutex::new(None));
    let runtime_slot: Arc<Mutex<Option<Arc<HeadlessRuntime>>>> = Arc::new(Mutex::new(None));
    let plane_slot: Arc<Mutex<Option<Arc<control_plane::StdioControlPlane>>>> =
        Arc::new(Mutex::new(None));
    let startup_cancel = CancellationToken::new();
    let signal_runtime = runtime_slot.clone();
    let signal_plane = plane_slot.clone();
    let signal_result = last_signal.clone();
    let signal_startup = startup_cancel.clone();
    let signal_stdout = io.stdout.clone();
    let signal_stderr = io.stderr.clone();
    let mut watcher = SignalWatcher(Some(tokio::spawn(async move {
        let mut signals_open = true;
        let mut print_interrupt_deadline = None;
        loop {
            let signal = tokio::select! {
                _ = async {
                    match print_interrupt_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // Native 2.1.293 externally clocks the remaining stdout
                    // drain for two seconds after print SIGINT, then exits 0.
                    signal_stdout.abort_delivery("Headless print interrupt drain deadline reached");
                    signal_stderr.abort_delivery("Headless print interrupt drain deadline reached");
                    break;
                },
                signal = signals.receiver.recv(), if signals_open => {
                    if let Some(signal) = signal { signal } else {
                        signals_open = false;
                        continue;
                    }
                },
                _ = waiter_cancel.cancelled() => HeadlessSignal::Shutdown,
                _ = signal_stdout.delivery_cancelled() => HeadlessSignal::OutputClosed,
                _ = signal_stderr.delivery_cancelled() => HeadlessSignal::OutputClosed,
            };
            if signal == HeadlessSignal::OutputClosed && print_interrupt_deadline.is_some() {
                // A transport failure during the signal's drain cannot
                // replace the print handler's already selected exit code 0.
                signal_stdout.abort_delivery("Headless print interrupt delivery stopped");
                signal_stderr.abort_delivery("Headless print interrupt delivery stopped");
                break;
            }
            *signal_result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(signal);
            let print_interrupt =
                signal == HeadlessSignal::Interrupt && input_format != InputFormat::StreamJson;
            if print_interrupt {
                print_interrupt_deadline.get_or_insert_with(|| {
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2)
                });
            }
            if matches!(
                signal,
                HeadlessSignal::Shutdown
                    | HeadlessSignal::OutputClosed
                    | HeadlessSignal::Terminate { .. }
            ) {
                signal_stdout.abort_delivery("Headless output delivery cancelled by shutdown");
                signal_stderr.abort_delivery("Headless diagnostic delivery cancelled by shutdown");
            }
            if input_format != InputFormat::StreamJson
                || !matches!(signal, HeadlessSignal::Interrupt)
            {
                signal_startup.cancel();
            }
            let runtime = signal_runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let plane = signal_plane
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(runtime) = runtime {
                runtime.interrupt();
                if let Some(plane) = &plane {
                    plane.cancel_active_turn().await;
                }
                if signal_startup.is_cancelled() {
                    runtime.shutdown.cancel();
                    // The print owner continues polling execution and owns
                    // task settlement. Keep this watcher free to bound I/O.
                    if !print_interrupt {
                        run::shutdown_tasks(&runtime).await;
                    }
                }
            }
            if !matches!(signal, HeadlessSignal::Interrupt) {
                if let Some(plane) = &plane {
                    plane.shutdown("Session terminated").await;
                }
                break;
            }
        }
    })));
    let options = &mut config.options;
    let invalid = options.validation_error();
    if let Some(message) = invalid {
        let _ = io.stderr.write_line(message).await;
        exit.code = exit_codes::ARGV_ERROR;
        exit.reason = HeadlessExitReason::InvalidInput;
        exit.errors.push(message.into());
        return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
    }
    if options.rewind_files.is_none()
        && options.input_format == InputFormat::Text
        && !io.input_is_terminal
    {
        use tokio::io::AsyncReadExt;
        let mut prompt = String::new();
        let read = tokio::select! {
            outcome = io.input.read_to_string(&mut prompt) => Some(outcome),
            _ = startup_cancel.cancelled() => None,
        };
        let Some(read) = read else {
            let signal = *last_signal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match signal {
                Some(HeadlessSignal::Shutdown) => {
                    exit.code = exit_codes::SIGINT;
                    exit.reason = HeadlessExitReason::Cancelled;
                    exit.execution = HeadlessExecutionStatus::Cancelled;
                }
                Some(HeadlessSignal::Terminate { signal }) => {
                    exit.code = 128 + signal;
                    exit.reason = HeadlessExitReason::Terminated { signal };
                    exit.execution = HeadlessExecutionStatus::Terminated { signal };
                }
                Some(HeadlessSignal::OutputClosed) => {
                    exit.code = exit_codes::RUNTIME_ERROR;
                    exit.reason = HeadlessExitReason::OutputError;
                }
                _ => {
                    exit.code = exit_codes::SIGINT;
                    exit.reason = HeadlessExitReason::Interrupted;
                    exit.execution = HeadlessExecutionStatus::Interrupted;
                }
            }
            return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
        };
        match read {
            Ok(_) => {
                if !prompt.is_empty() {
                    options.prompt = Some(
                        match options.prompt.take().filter(|text| !text.is_empty()) {
                            Some(argument) => format!("{argument}\n{prompt}"),
                            None => prompt,
                        },
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                let _ = io.stderr.write_line(&message).await;
                exit.code = exit_codes::RUNTIME_ERROR;
                exit.reason = HeadlessExitReason::RuntimeError;
                exit.errors.push(message);
                return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format)
                    .await;
            }
        }
    }
    host::configure_desktop_request_state(&mut config.desktop, services.as_ref(), options);
    let environment_services = services.clone();
    let descriptor_services = services.clone();
    config.desktop.oauth_environment_lookup = Some(
        llm_runtime::auth::anthropic::environment::EnvironmentLookup::new(move |name| {
            environment_services.environment_variable(name)
        })
        .with_descriptor_lookup(move || descriptor_services.oauth_descriptor_credential()),
    );
    config.desktop.use_noop_permission_gate = true;
    config.desktop.deny_unresolved_ask = true;
    config.desktop.max_budget_usd = options.max_budget_usd;
    config.desktop.json_schema = options.json_schema.clone();
    config.desktop.max_structured_output_retries = options.max_structured_output_retries;
    if options.permission_prompts_none {
        config.desktop.injected_permission_gate = None;
    }
    let prepared = match session::prepare(&mut config.desktop, &config.session_start).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = io.stderr.write_line(&format!("lingxi-cli: {error}")).await;
            exit.code = exit_codes::RUNTIME_ERROR;
            exit.reason = HeadlessExitReason::RuntimeError;
            exit.errors.push(error);
            return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
        }
    };
    if let Some(message_id) = &options.rewind_files {
        match session::rewind_files(&config.desktop, prepared.as_ref(), message_id).await {
            Ok(outcome) => {
                if outcome.skipped_links > 0 {
                    let noun = if outcome.skipped_links == 1 {
                        "path was"
                    } else {
                        "paths were"
                    };
                    let _ = io.stderr.write_line(&format!("Warning: {} tracked {noun} skipped: the tracked path is (or became) a link or other non-regular file, its directory changed since the checkpoint, or its backup could not be safely read. Run with --debug for the paths.", outcome.skipped_links)).await;
                }
                let _ = io
                    .stdout
                    .write_line(&format!("Files rewound to state at message {message_id}."))
                    .await;
            }
            Err(error) => {
                let detail = if error.is_empty() {
                    "Unexpected error"
                } else {
                    &error
                };
                let _ = io.stderr.write_line(&format!("Error: {detail}")).await;
                exit.code = exit_codes::RUNTIME_ERROR;
                exit.reason = HeadlessExitReason::RuntimeError;
                exit.errors.push(error);
            }
        }
        return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
    }
    let permission_mode = config.desktop.permission_mode;
    let local_text_command = options.output_format == OutputFormat::Text
        && options.prompt.as_deref().is_some_and(|prompt| {
            prompt.starts_with('/') && prompt.split_whitespace().next() != Some("/compact")
        });
    let stream = match options.output_format {
        OutputFormat::Text if !local_text_command => Some(Arc::new(
            stream_json::StreamJsonStream::new_text_mode_placeholder(
                io.stdout.clone(),
                max_turns,
                options.max_budget_usd,
            ),
        )),
        OutputFormat::StreamJson => Some(Arc::new(stream_json::StreamJsonStream::new_placeholder(
            io.stdout.clone(),
        ))),
        OutputFormat::Json => Some(Arc::new(if options.verbose {
            stream_json::StreamJsonStream::new_verbose_json_mode_placeholder(io.stdout.clone())
        } else {
            stream_json::StreamJsonStream::new_json_mode_placeholder(io.stdout.clone())
        })),
        _ => None,
    };
    let sink: Option<Arc<dyn output::OutputSink>> = match options.output_format {
        OutputFormat::Text if local_text_command => Some(Arc::new(output::PlainSink::new(
            io.stdout.clone(),
            io.stderr.clone(),
        ))),
        OutputFormat::Ndjson => Some(Arc::new(output::JsonSink::new(
            lingxi_core::types::SessionId::new(),
            io.stdout.clone(),
        ))),
        _ => None,
    };
    let adapter: Arc<dyn OutputStream> = if let Some(stream) = &stream {
        stream.set_flags(
            options.include_partial_messages,
            options.include_hook_events,
        );
        stream.set_forward_subagent_text(options.forward_subagent_text);
        stream.set_thinking_display(options.thinking_display.as_deref());
        stream.clone()
    } else {
        Arc::new(output_adapter::SinkAdapter::new(
            sink.as_ref().expect("plain output sink").clone(),
        ))
    };
    let plane = if options.input_format == InputFormat::StreamJson {
        let stream = stream.as_ref().expect("validated stream-json output");
        let plane = control_plane::StdioControlPlane::new(stream.outbound_tx());
        plane.set_session_id(stream.session_id_handle());
        Some(plane)
    } else {
        None
    };
    let gate = if let Some(plane) = &plane {
        if !options.permission_prompts_none {
            let gate = Arc::new(
                control_plane::StdioControlPermissionGate::new(plane.clone()).with_persist(
                    permission::PermissionPaths {
                        lingxi_home: config.desktop.lingxi_home.clone(),
                        cwd: config.desktop.cwd.clone(),
                    },
                ),
            );
            config.desktop.injected_permission_gate = Some(gate.clone());
            Some(gate)
        } else {
            let _ = io.stderr.write_line("--permission-prompts none: permission prompts are answered with a local deny; the SDK host is not consulted").await;
            config.desktop.injected_permission_gate = None;
            None
        }
    } else {
        None
    };
    let mut prepared_stdio = if let (Some(stream), Some(plane)) = (&stream, &plane) {
        let session_id = config
            .desktop
            .session_id_override
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        let delegate_factory = config.desktop.mcp_services_factory.take();
        let transport_plane = plane.clone();
        let transport_cwd = config.desktop.cwd.clone();
        config.desktop.mcp_services_factory = Some(Arc::new(move |base| {
            let base = match &delegate_factory {
                Some(factory) => factory(base),
                None => base,
            };
            let transport = sdk_mcp_transport::SdkMcpTransport::new(
                base,
                transport_plane.clone(),
                transport_cwd.clone(),
            );
            crate::desktop::DesktopMcpServices {
                transport: transport.clone(),
                raw_connections: transport,
            }
        }));
        *plane_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(plane.clone());
        Some(
            stdio::PreparedStdio::start(
                std::mem::replace(&mut io.input, Box::pin(tokio::io::empty())),
                io.stderr.clone(),
                options.replay_user_messages,
                session_id,
                stream.clone(),
                plane.clone(),
                startup_cancel.clone(),
            )
            .await,
        )
    } else {
        None
    };
    config.desktop.diagnostics = Some(Arc::new(HeadlessDesktopDiagnostics(io.stderr.clone())));
    let inner = match session::build_prepared(
        config.desktop,
        prepared,
        adapter,
        Arc::new(HeadlessPermissionRequestSink),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let message = error.to_string();
            let _ = io
                .stderr
                .write_line(&format!("lingxi-cli: {message}"))
                .await;
            exit.code = exit_codes::RUNTIME_ERROR;
            exit.reason = HeadlessExitReason::RuntimeError;
            exit.errors.push(message);
            exit.cleanup = error.cleanup;
            exit.shutdown = error.shutdown;
            close_prepared_stdio(&mut prepared_stdio, stream.as_deref(), &mut exit).await;
            return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
        }
    };
    let runtime = Arc::new(HeadlessRuntime {
        inner,
        services,
        output: io.stderr.clone(),
        stdout: io.stdout.clone(),
        shutdown: CancellationToken::new(),
        active_turn: Mutex::new(CancellationToken::new()),
        failures: Mutex::new(Vec::new()),
        execution_errors: Arc::new(Mutex::new(Vec::new())),
        max_turns,
        execution_code: std::sync::atomic::AtomicI32::new(exit_codes::SUCCESS),
        execution_interrupted: std::sync::atomic::AtomicBool::new(false),
        shutdown_complete: std::sync::atomic::AtomicBool::new(false),
    });
    exit.session_id = Some(
        runtime
            .orchestrator
            .session()
            .lock()
            .await
            .session_id
            .to_string(),
    );
    if let Some(stream) = &stream {
        stream.share_permission_denials(runtime.orchestrator.permission_denials_handle());
    }
    if let Some(plane) = &plane {
        runtime
            .orchestrator
            .set_remote_ui_host(remote_ui::HeadlessRemoteUiHost::new(plane.clone()));
    }
    if let Some(gate) = gate {
        gate.set_prompt_notifier(Arc::new(
            permission_prompt_notify::OrchestratorPermissionPromptNotifier::new(
                runtime.orchestrator.clone(),
                runtime.services.clone(),
            ),
        ));
    }
    if let Err(error) = runtime.services.runtime_ready(&runtime.inner).await {
        let _ = runtime.output.write_line(&error).await;
        exit.code = exit_codes::RUNTIME_ERROR;
        exit.reason = HeadlessExitReason::RuntimeError;
        exit.errors.push(error);
        let report = runtime.session_lifecycle.shutdown_and_drain().await;
        exit.shutdown = if report.complete {
            HeadlessShutdownStatus::Complete
        } else {
            HeadlessShutdownStatus::Incomplete
        };
        exit.shutdown_errors = report.errors.clone();
        if !report.complete {
            exit.cleanup = Some(cleanup::retain_cleanup(runtime.clone(), report));
        }
        exit.errors.extend(exit.shutdown_errors.iter().cloned());
        close_prepared_stdio(&mut prepared_stdio, stream.as_deref(), &mut exit).await;
        return finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
    }
    *runtime_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime.clone());
    *plane_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = plane.clone();
    if startup_cancel.is_cancelled() {
        runtime.shutdown.cancel();
        runtime.interrupt();
    }
    exit.code = match (stream.as_ref(), plane) {
        (Some(stream), Some(plane)) => {
            run::run_stream_json_input_loop(
                options,
                &runtime,
                stream.clone(),
                permission_mode,
                plane,
                prepared_stdio
                    .take()
                    .expect("bidirectional transport prepared before build"),
            )
            .await
        }
        (Some(stream), None) => {
            run::run_stream_json_print(options, &runtime, stream.clone(), permission_mode).await
        }
        (None, _) => {
            run::run_oneshot(
                options,
                &runtime,
                sink.as_ref().expect("plain sink").as_ref(),
            )
            .await
        }
    };
    let signal = *last_signal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    exit.execution = if runtime
        .execution_interrupted
        .load(std::sync::atomic::Ordering::Acquire)
    {
        HeadlessExecutionStatus::Interrupted
    } else if runtime
        .execution_code
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
    {
        HeadlessExecutionStatus::Succeeded
    } else {
        HeadlessExecutionStatus::Failed
    };
    exit.shutdown = if runtime
        .shutdown_complete
        .load(std::sync::atomic::Ordering::Acquire)
    {
        HeadlessShutdownStatus::Complete
    } else {
        HeadlessShutdownStatus::Incomplete
    };
    exit.reason = if exit.code == 0 {
        HeadlessExitReason::Completed
    } else {
        HeadlessExitReason::RuntimeError
    };
    match signal {
        Some(HeadlessSignal::Shutdown) => {
            exit.code = exit_codes::SIGINT;
            exit.reason = HeadlessExitReason::Cancelled;
            exit.execution = HeadlessExecutionStatus::Cancelled;
        }
        Some(HeadlessSignal::Terminate { signal }) => {
            exit.code = 128 + signal;
            exit.reason = HeadlessExitReason::Terminated { signal };
            exit.execution = HeadlessExecutionStatus::Terminated { signal };
        }
        Some(HeadlessSignal::OutputClosed) => {
            exit.code = exit_codes::RUNTIME_ERROR;
            exit.reason = HeadlessExitReason::OutputError;
        }
        Some(HeadlessSignal::Interrupt) if options.input_format != InputFormat::StreamJson => {
            exit.code = exit_codes::SUCCESS;
            exit.reason = HeadlessExitReason::Interrupted;
            if exit.execution != HeadlessExecutionStatus::Succeeded {
                exit.execution = HeadlessExecutionStatus::Interrupted;
            }
        }
        _ => {}
    }
    exit.errors.extend(
        runtime
            .execution_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    exit.shutdown_errors.extend(
        runtime
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    exit.errors.extend(exit.shutdown_errors.iter().cloned());
    if exit.shutdown == HeadlessShutdownStatus::Incomplete {
        exit.cleanup = Some(cleanup::retain_cleanup(
            runtime.clone(),
            crate::desktop::DesktopSessionShutdownReport {
                complete: false,
                errors: exit.shutdown_errors.clone(),
                ..Default::default()
            },
        ));
    }
    if let Some(stream) = stream {
        if let Err(error) = stream.finish().await {
            exit.delivery_errors.push(error.to_string());
        }
    }
    let exit = finish_delivery(exit, &io.stdout, &io.stderr, &last_signal, input_format).await;
    watcher.stop().await;
    exit
}

async fn close_prepared_stdio(
    prepared: &mut Option<stdio::PreparedStdio>,
    stream: Option<&stream_json::StreamJsonStream>,
    exit: &mut HeadlessExit,
) {
    if let Some(prepared) = prepared.as_mut() {
        if let Err(error) = prepared.shutdown("Headless startup failed").await {
            exit.errors.push(error.to_string());
        }
    }
    if let Some(stream) = stream {
        if let Err(error) = stream.finish().await {
            exit.delivery_errors.push(error.to_string());
        }
    }
}

async fn finish_delivery(
    mut exit: HeadlessExit,
    stdout: &io::Output,
    stderr: &io::Output,
    last_signal: &Mutex<Option<HeadlessSignal>>,
    input_format: InputFormat,
) -> HeadlessExit {
    for output in [stdout, stderr] {
        let result = output.flush().await;
        if let Some(error) = output.error() {
            exit.delivery_errors.push(error.message);
        } else if let Err(error) = result {
            exit.delivery_errors.push(error.to_string());
        }
    }
    exit.delivery_errors.sort();
    exit.delivery_errors.dedup();
    exit.errors.extend(exit.delivery_errors.iter().cloned());
    if !exit.delivery_errors.is_empty() {
        exit.delivery = HeadlessDeliveryStatus::Failed;
        if !matches!(
            exit.reason,
            HeadlessExitReason::Terminated { .. } | HeadlessExitReason::Cancelled
        ) {
            exit.code = exit_codes::RUNTIME_ERROR;
            exit.reason = HeadlessExitReason::OutputError;
        }
    }
    // Signals can arrive while any diagnostic or final frame is draining.
    match *last_signal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        Some(HeadlessSignal::Terminate { signal }) => {
            exit.code = 128 + signal;
            exit.reason = HeadlessExitReason::Terminated { signal };
            exit.execution = HeadlessExecutionStatus::Terminated { signal };
        }
        Some(HeadlessSignal::Shutdown) => {
            exit.code = exit_codes::SIGINT;
            exit.reason = HeadlessExitReason::Cancelled;
            exit.execution = HeadlessExecutionStatus::Cancelled;
        }
        Some(HeadlessSignal::Interrupt) if input_format != InputFormat::StreamJson => {
            // Delivery may be incomplete after the native two-second grace;
            // that diagnostic is independent of the print signal exit code.
            exit.code = exit_codes::SUCCESS;
            exit.reason = HeadlessExitReason::Interrupted;
            if !matches!(
                exit.execution,
                HeadlessExecutionStatus::Succeeded | HeadlessExecutionStatus::NotStarted
            ) {
                exit.execution = HeadlessExecutionStatus::Interrupted;
            }
        }
        _ => {}
    }
    exit
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Host;
    #[async_trait::async_trait]
    impl HeadlessHostServices for Host {
        fn environment_variable(&self, _: &str) -> Option<String> {
            None
        }
        fn update_environment_variables(
            &self,
            _: std::collections::HashMap<String, String>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn embedded_argument_diagnostics_match_frozen_native_process_bytes() {
        use tokio::io::AsyncReadExt;
        let captures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/headless-fixtures/native-2.1.293-parser-probes.json"
        ))
        .unwrap();
        for (case, options) in [
            (
                "probe-parser-partial-json",
                HeadlessOptions {
                    output_format: OutputFormat::Json,
                    include_partial_messages: true,
                    ..Default::default()
                },
            ),
            (
                "probe-parser-replay-text",
                HeadlessOptions {
                    replay_user_messages: true,
                    ..Default::default()
                },
            ),
            (
                "probe-parser-sdk-json",
                HeadlessOptions {
                    input_format: InputFormat::StreamJson,
                    output_format: OutputFormat::Json,
                    ..Default::default()
                },
            ),
        ] {
            let (stdout, mut stdout_reader) = tokio::io::duplex(1024);
            let (stderr, mut stderr_reader) = tokio::io::duplex(1024);
            let exit = run(
                HeadlessConfig {
                    desktop: crate::desktop::DesktopConfig::default(),
                    options,
                    session_start: SessionStart::New,
                },
                Arc::new(Host),
                HeadlessIo::new(tokio::io::empty(), stdout, stderr),
                HeadlessSignals::default(),
            )
            .await;
            let mut actual_stdout = Vec::new();
            let mut actual_stderr = Vec::new();
            stdout_reader.read_to_end(&mut actual_stdout).await.unwrap();
            stderr_reader.read_to_end(&mut actual_stderr).await.unwrap();
            for native in captures["cases"][case].as_array().unwrap() {
                assert_eq!(
                    actual_stdout,
                    native["stdout"].as_str().unwrap().as_bytes(),
                    "{case}"
                );
                assert_eq!(
                    actual_stderr,
                    native["stderr"].as_str().unwrap().as_bytes(),
                    "{case}"
                );
                assert_eq!(
                    exit.code as i64,
                    native["observation"]["exit"]["code"].as_i64().unwrap()
                );
            }
            assert_eq!(exit.execution, HeadlessExecutionStatus::NotStarted);
        }
    }

    #[tokio::test]
    async fn terminate_unblocks_text_input_before_runtime_build() {
        let (peer, input) = tokio::io::duplex(1024);
        let (sender, signals) = HeadlessSignals::channel();
        sender
            .send(HeadlessSignal::Terminate { signal: 15 })
            .unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run(
                HeadlessConfig {
                    desktop: crate::desktop::DesktopConfig::default(),
                    options: HeadlessOptions::default(),
                    session_start: SessionStart::New,
                },
                Arc::new(Host),
                HeadlessIo {
                    input: Box::pin(input),
                    input_is_terminal: false,
                    stdout: io::Output::new(tokio::io::sink()),
                    stderr: io::Output::new(tokio::io::sink()),
                },
                signals,
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome.code, 143);
        assert_eq!(
            outcome.reason,
            HeadlessExitReason::Terminated { signal: 15 }
        );
        assert_eq!(outcome.shutdown, HeadlessShutdownStatus::NotStarted);
        drop(peer);
    }

    #[test]
    fn dropping_the_waiter_requests_owned_shutdown() {
        let token = CancellationToken::new();
        let guard = CancelWaiterOnDrop(token.clone());
        assert!(!token.is_cancelled());
        drop(guard);
        assert!(token.is_cancelled());
    }

    struct BrokenWriter;
    impl tokio::io::AsyncWrite for BrokenWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "diagnostic receiver closed",
            )))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn diagnostic_delivery_failure_is_distinct_from_execution() {
        let outcome = run(
            HeadlessConfig {
                desktop: crate::desktop::DesktopConfig::default(),
                options: HeadlessOptions {
                    output_format: OutputFormat::StreamJson,
                    ..Default::default()
                },
                session_start: SessionStart::New,
            },
            Arc::new(Host),
            HeadlessIo {
                input: Box::pin(tokio::io::empty()),
                input_is_terminal: false,
                stdout: io::Output::new(tokio::io::sink()),
                stderr: io::Output::new(BrokenWriter),
            },
            HeadlessSignals::default(),
        )
        .await;
        assert_eq!(outcome.code, 1);
        assert_eq!(outcome.execution, HeadlessExecutionStatus::NotStarted);
        assert_eq!(outcome.shutdown, HeadlessShutdownStatus::NotStarted);
        assert_eq!(outcome.delivery, HeadlessDeliveryStatus::Failed);
        assert_eq!(outcome.delivery_errors, vec!["diagnostic receiver closed"]);
    }

    #[tokio::test]
    async fn desktop_diagnostic_adapter_uses_injected_bytes_and_retains_write_failures() {
        use tokio::io::AsyncReadExt;
        let (writer, mut reader) = tokio::io::duplex(128);
        let diagnostic = HeadlessDesktopDiagnostics(io::Output::new(writer));
        crate::desktop::DesktopDiagnosticSink::stderr_line(&diagnostic, "startup diagnostic").await;
        let mut bytes = vec![0; b"startup diagnostic\n".len()];
        reader.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"startup diagnostic\n");
        let output = io::Output::new(BrokenWriter);
        crate::desktop::DesktopDiagnosticSink::stderr_line(
            &HeadlessDesktopDiagnostics(output.clone()),
            "startup diagnostic",
        )
        .await;
        assert_eq!(
            output.error().unwrap().message,
            "diagnostic receiver closed"
        );
    }

    #[tokio::test]
    async fn terminate_unblocks_a_full_diagnostic_pipe() {
        let (stderr, mut peer) = tokio::io::duplex(1);
        let (sender, signals) = HeadlessSignals::channel();
        let task = tokio::spawn(run(
            HeadlessConfig {
                desktop: crate::desktop::DesktopConfig::default(),
                options: HeadlessOptions {
                    output_format: OutputFormat::StreamJson,
                    ..Default::default()
                },
                session_start: SessionStart::New,
            },
            Arc::new(Host),
            HeadlessIo::new(tokio::io::empty(), tokio::io::sink(), stderr),
            signals,
        ));
        use tokio::io::AsyncReadExt;
        peer.read_u8().await.unwrap();
        sender
            .send(HeadlessSignal::Terminate { signal: 15 })
            .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.code, 143);
        assert_eq!(
            outcome.reason,
            HeadlessExitReason::Terminated { signal: 15 }
        );
        assert_eq!(outcome.delivery, HeadlessDeliveryStatus::Failed);
        assert_eq!(outcome.shutdown, HeadlessShutdownStatus::NotStarted);
    }

    #[tokio::test(start_paused = true)]
    async fn print_interrupt_bounds_blocked_delivery_without_changing_native_exit_zero() {
        let (stderr, mut peer) = tokio::io::duplex(1);
        let (sender, signals) = HeadlessSignals::channel();
        let task = tokio::spawn(run(
            HeadlessConfig {
                desktop: crate::desktop::DesktopConfig::default(),
                options: HeadlessOptions {
                    output_format: OutputFormat::StreamJson,
                    ..Default::default()
                },
                session_start: SessionStart::New,
            },
            Arc::new(Host),
            HeadlessIo::new(tokio::io::empty(), tokio::io::sink(), stderr),
            signals,
        ));
        use tokio::io::AsyncReadExt;
        peer.read_u8().await.unwrap();
        sender.send(HeadlessSignal::Interrupt).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(1999)).await;
        assert!(
            !task.is_finished(),
            "print interruption permits the native two-second drain"
        );
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        let outcome = task.await.unwrap();
        assert_eq!(outcome.code, 0);
        assert_eq!(outcome.reason, HeadlessExitReason::Interrupted);
        assert_eq!(outcome.execution, HeadlessExecutionStatus::NotStarted);
        assert_eq!(outcome.delivery, HeadlessDeliveryStatus::Failed);
        assert_eq!(outcome.shutdown, HeadlessShutdownStatus::NotStarted);
    }
}
