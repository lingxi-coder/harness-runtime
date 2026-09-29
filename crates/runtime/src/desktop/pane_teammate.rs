//! Session-owned transport for persistent teammates in terminal panes.
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use coordinator::{CoordinatorStatusSink, SendMessageTool, TeamRegistry};
use lingxi_core::host::runtime::RuntimeSpawner;
use lingxi_core::host::subagent_spawn::{SubagentInheritance, SubagentSpawnRequest};
use lingxi_core::host::swarm::{PaneId, PanePosition, SwarmBackend};
use lingxi_core::host::team_spawn::{PaneLaunchMetadata, TeamSpawnError, TeamSpawnSeam};
use lingxi_core::host::teammate_worker::{PaneTeammateManifest, ParentToWorker, WorkerToParent};
use lingxi_core::host::OutputStream;
use lingxi_core::types::{AgentId, SessionId};
use tasks::handlers::TaskStatusSink;
use tasks::registry::TaskRegistry;
use tasks::{TaskSpawnInput, TaskStatus, TaskType};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tool_api::context::ToolUseContext;
use tool_api::tool_trait::Tool;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const BACKEND_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EARLY_FRAMES: usize = 128;
const MAX_EARLY_BYTES: usize = 4 * 1024 * 1024;

/// Keep partial line bytes across select cancellation and cap every frame
/// before allocating an unbounded JSON line from the worker socket.
struct WorkerReader<R> {
    reader: R,
    partial: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> WorkerReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            partial: Vec::new(),
        }
    }

    async fn next_frame(&mut self) -> Result<Option<WorkerToParent>, TeamSpawnError> {
        let remaining = MAX_FRAME_BYTES.saturating_sub(self.partial.len()) + 1;
        let read = (&mut self.reader)
            .take(remaining as u64)
            .read_until(b'\n', &mut self.partial)
            .await
            .map_err(internal)?;
        if self.partial.len() > MAX_FRAME_BYTES {
            return Err(internal("Teammate frame exceeds 1 MiB"));
        }
        if read == 0 && self.partial.is_empty() {
            return Ok(None);
        }
        if !self.partial.ends_with(b"\n") {
            return Err(internal("Incomplete teammate frame"));
        }
        let frame = serde_json::from_slice(&self.partial).map_err(internal)?;
        self.partial.clear();
        Ok(Some(frame))
    }
}

async fn authenticate_until_ready<R: AsyncBufRead + Unpin>(
    reader: &mut WorkerReader<R>,
    token: &str,
) -> Result<VecDeque<WorkerToParent>, TeamSpawnError> {
    match reader.next_frame().await? {
        Some(WorkerToParent::Hello { token: received }) if received == token => {}
        _ => return Err(internal("Teammate authentication failed")),
    }
    let mut early = VecDeque::new();
    let mut bytes = 0;
    loop {
        let message = reader
            .next_frame()
            .await?
            .ok_or_else(|| internal("Teammate closed before Ready"))?;
        match message {
            WorkerToParent::Ready { .. } => return Ok(early),
            WorkerToParent::Hello { .. } => {
                return Err(internal("Repeated teammate authentication"))
            }
            WorkerToParent::State {
                ref status,
                ref error,
                ..
            } if matches!(status.as_str(), "failed" | "killed") => {
                return Err(internal(format!(
                    "Teammate terminated before Ready ({status}): {}",
                    error.as_deref().unwrap_or("no failure detail")
                )));
            }
            _ => {
                bytes += serde_json::to_vec(&message).map_err(internal)?.len();
                if early.len() >= MAX_EARLY_FRAMES || bytes > MAX_EARLY_BYTES {
                    return Err(internal("Teammate exceeded pre-Ready message buffer"));
                }
                early.push_back(message);
            }
        }
    }
}

struct ControlWriter {
    stream: Mutex<OwnedWriteHalf>,
    poisoned: AtomicBool,
}
impl ControlWriter {
    fn new(stream: OwnedWriteHalf) -> Self {
        Self {
            stream: Mutex::new(stream),
            poisoned: AtomicBool::new(false),
        }
    }
}

struct FrameWriteGuard<'a> {
    poisoned: &'a AtomicBool,
    complete: bool,
}
impl Drop for FrameWriteGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone)]
struct PaneTask {
    writer: Option<Arc<ControlWriter>>,
    teardown: Arc<Mutex<()>>,
    terminated: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    pane: PaneId,
    directory: PathBuf,
    metadata: Option<PaneLaunchMetadata>,
    backend: Arc<dyn SwarmBackend>,
}

/// Live backend selection resolved for one new teammate launch.
pub(crate) struct PaneBackendSelection {
    pub(crate) backend: Option<Arc<dyn SwarmBackend>>,
    pub(crate) explicit: bool,
    pub(crate) error: Option<String>,
}

/// Routes launches to a terminal backend, retaining the in-process registry as
/// the automatic-mode fallback and lifecycle owner of ordinary teammate tasks.
#[derive(Clone)]
pub(crate) struct PaneTeammateSpawner {
    registry: Arc<TaskRegistry>,
    team: Arc<TeamRegistry>,
    runtime: Arc<dyn RuntimeSpawner>,
    output: Arc<dyn OutputStream>,
    session_id: SessionId,
    private_dir: PathBuf,
    backend: Option<Arc<dyn SwarmBackend>>,
    explicit: bool,
    backend_error: Option<String>,
    fallback_warned: Arc<AtomicBool>,
    backend_selector: Option<Arc<dyn Fn() -> PaneBackendSelection + Send + Sync>>,
    executable: PathBuf,
    tasks: Arc<Mutex<HashMap<String, PaneTask>>>,
}

impl PaneTeammateSpawner {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        registry: Arc<TaskRegistry>,
        team: Arc<TeamRegistry>,
        runtime: Arc<dyn RuntimeSpawner>,
        output: Arc<dyn OutputStream>,
        session_id: SessionId,
        private_dir: PathBuf,
        backend: Option<Arc<dyn SwarmBackend>>,
        explicit: bool,
        worker_executable: PathBuf,
    ) -> Self {
        Self {
            registry,
            team,
            runtime,
            output,
            session_id,
            private_dir,
            backend,
            explicit,
            backend_error: None,
            fallback_warned: Arc::new(AtomicBool::new(false)),
            backend_selector: None,
            executable: worker_executable,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn with_backend_selector(
        mut self,
        selector: Arc<dyn Fn() -> PaneBackendSelection + Send + Sync>,
    ) -> Self {
        self.backend_selector = Some(selector);
        self
    }

    async fn write(writer: &ControlWriter, value: ParentToWorker) -> Result<(), TeamSpawnError> {
        if writer.poisoned.load(Ordering::Acquire) {
            return Err(TeamSpawnError::Terminated);
        }
        let mut bytes = serde_json::to_vec(&value).map_err(internal)?;
        bytes.push(b'\n');
        let deadline = tokio::time::Instant::now() + CONTROL_WRITE_TIMEOUT;
        // A lock timeout has written no bytes and is safe for the pump to retry.
        let mut stream = tokio::time::timeout_at(deadline, writer.stream.lock())
            .await
            .map_err(|_| internal("Teammate control writer lock timed out"))?;
        if writer.poisoned.load(Ordering::Acquire) {
            return Err(TeamSpawnError::Terminated);
        }
        // Cancellation or timeout after this point may leave a partial JSON
        // frame. Poison the channel before releasing the lock on every such exit.
        let mut attempt = FrameWriteGuard {
            poisoned: &writer.poisoned,
            complete: false,
        };
        match tokio::time::timeout_at(deadline, stream.write_all(&bytes)).await {
            Ok(Ok(())) => {
                attempt.complete = true;
                Ok(())
            }
            _ => Err(TeamSpawnError::Terminated),
        }
    }

    async fn cleanup(&self, task_id: &str) -> Result<(), TeamSpawnError> {
        let Some(task) = self.tasks.lock().await.get(task_id).cloned() else {
            return Ok(());
        };
        let _teardown = task.teardown.lock().await;
        if !self.tasks.lock().await.contains_key(task_id) {
            return Ok(());
        }
        let killed = if task.terminated.load(Ordering::Acquire) {
            Ok(())
        } else {
            match tokio::time::timeout(BACKEND_CLEANUP_TIMEOUT, task.backend.kill_pane(&task.pane))
                .await
            {
                Ok(Ok(())) => {
                    task.terminated.store(true, Ordering::Release);
                    Ok(())
                }
                Ok(Err(error)) => Err(internal(format!(
                    "Teammate pane termination failed: {error}"
                ))),
                Err(_) => Err(internal("Teammate pane termination timed out")),
            }
        };
        // Scrub private launch files even on kill failure, but retain the pane
        // identity so a subsequent Stop can retry real backend termination.
        let removed = match tokio::fs::remove_dir_all(&task.directory).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(internal(error)),
        };
        killed?;
        removed?;
        self.team
            .complete_approved_departure(task_id)
            .await
            .map_err(internal)?;
        self.registry
            .unregister_external_teammate_task(task_id)
            .await;
        self.tasks.lock().await.remove(task_id);
        Ok(())
    }

    async fn finish_pane(
        &self,
        task_id: &str,
        terminal: Option<(TaskStatus, Option<String>)>,
    ) -> Result<(), TeamSpawnError> {
        // A terminal worker report does not confirm external pane teardown.
        // Keep the task Running and stoppable until cleanup actually succeeds.
        self.cleanup(task_id).await?;
        if let Some((status, error)) = terminal {
            let actual = self
                .registry
                .set_status(task_id, status)
                .await
                .map_err(internal)?
                .base()
                .status;
            let sink = CoordinatorStatusSink::new(self.team.clone(), self.output.clone());
            if let Some(error) = error.filter(|_| status == TaskStatus::Failed && actual == status)
            {
                sink.set_failed(task_id, &error).await;
            } else {
                sink.set_status(task_id, actual).await;
            }
        }
        Ok(())
    }

    async fn stop_pane(&self, task_id: &str) -> Result<(), TeamSpawnError> {
        let task = self.tasks.lock().await.get(task_id).cloned();
        if let Some(task) = task {
            task.stopping.store(true, Ordering::Release);
            if let Some(writer) = &task.writer {
                let _ = Self::write(writer, ParentToWorker::Shutdown).await;
            }
        }
        self.finish_pane(task_id, Some((TaskStatus::Killed, None)))
            .await
    }

    async fn launch_owned(
        &self,
        agent_id: AgentId,
        name: String,
        team_name: String,
        request: SubagentSpawnRequest,
    ) -> Result<String, TeamSpawnError> {
        // Keep the bounded handshake alive if its calling tool is interrupted:
        // it must still reap the pane and private files on every failure.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let owner = self.clone();
        self.runtime
            .spawn(
                "pane-teammate-launch",
                Box::pin(async move {
                    let result = owner.launch_pane(agent_id, name, team_name, request).await;
                    if let Err(Ok(task_id)) = sender.send(result) {
                        let _ = TeamSpawnSeam::kill(&owner, &task_id).await;
                    }
                }),
            )
            .await
            .map_err(internal)?;
        receiver.await.map_err(internal)?
    }

    async fn launch_pane(
        &self,
        agent_id: AgentId,
        name: String,
        team_name: String,
        request: SubagentSpawnRequest,
    ) -> Result<String, TeamSpawnError> {
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| internal("No terminal backend available"))?;
        if !self.executable.is_file() {
            return Err(internal(
                "LingXi CLI executable is unavailable for a terminal teammate",
            ));
        }
        // Each directory has a random identity and is created owner-only before
        // the manifest or socket exists. An existing path is never reused.
        let directory = self
            .private_dir
            .join(format!("lingxi-tm-{}", AgentId::new()));
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(internal)?;
        let result = async {
            let socket_path = directory.join("ipc");
            let listener = UnixListener::bind(&socket_path).map_err(internal)?;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).map_err(internal)?;
            let token = format!("{}{}", AgentId::new(), AgentId::new());
            let manifest_path = directory.join("launch.json");
            let manifest = PaneTeammateManifest { socket_path, token: token.clone(), agent_id,
                name: name.clone(), team_name: team_name.clone(), parent_session_id: self.session_id,
                request: request.clone() };
            {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new().write(true).create_new(true)
                    .mode(0o600).open(&manifest_path).map_err(internal)?;
                file.write_all(&serde_json::to_vec(&manifest).map_err(internal)?).map_err(internal)?;
            }
                let task_id = self.registry.create(TaskType::InProcessTeammate,
                    TaskSpawnInput::InProcessTeammate { agent_id, name: name.clone(), team_name: team_name.clone(),
                        description: request.prompt.clone(), spawn_request: Some(request.clone()), inheritance: None },
                    request.description.clone().unwrap_or_default()).await.map_err(internal)?;
            let pane = match backend.create_teammate_pane(&agent_id, PanePosition::Right).await {
                Ok(pane) => pane,
                Err(error) => {
                    let _ = self.registry.set_status(&task_id, TaskStatus::Failed).await;
                    return Err(internal(error));
                }
            };
            let mut registered = false;
            let launched = async {
                let metadata = backend.pane_metadata(&pane).await.map_err(internal)?;
                let mut command = format!("{} --teammate-launch-file {}", shell_quote(&self.executable), shell_quote(&manifest_path));
                let color = request.teammate_color.as_deref().unwrap_or("blue");
                for (flag, value) in [
                    ("--agent-id", format!("{name}@{team_name}")),
                    ("--agent-name", name.clone()),
                    ("--team-name", team_name.clone()),
                    ("--agent-color", color.to_owned()),
                    ("--parent-session-id", self.session_id.to_string()),
                    ("--agent-type", request.subagent_type.clone()),
                ] {
                    command.push(' '); command.push_str(flag); command.push(' ');
                    command.push_str(&shell_quote(Path::new(&value)));
                }
                if request.mode.as_deref() == Some("plan") { command.push_str(" --plan-mode-required"); }
                if let Some(mode) = request.mode.as_deref() {
                    command.push_str(" --permission-mode ");
                    command.push_str(&shell_quote(Path::new(mode)));
                }
                backend.send_command_to_pane(&pane, &command).await.map_err(internal)?;
                let handshake = async {
                    let (stream, _) = listener.accept().await.map_err(internal)?;
                    let (read, write) = stream.into_split();
                    let mut reader = WorkerReader::new(BufReader::new(read));
                    let early = authenticate_until_ready(&mut reader, &token).await?;
                    Ok((reader, write, early))
                };
                let (mut reader, write, mut early) = tokio::time::timeout(Duration::from_secs(30), handshake)
                    .await.map_err(|_| internal("Teammate did not become ready within 30 seconds"))??;
                let writer = Arc::new(ControlWriter::new(write));
                let stopping = Arc::new(AtomicBool::new(false));
                self.tasks.lock().await.insert(task_id.clone(), PaneTask {
                    writer: Some(writer.clone()), teardown: Arc::new(Mutex::new(())), terminated: Arc::new(AtomicBool::new(false)), stopping: stopping.clone(), pane: pane.clone(), directory: directory.clone(), metadata: Some(metadata), backend: backend.clone(),
                });
                self.registry.register_external_teammate_task(&task_id).await;
                registered = true;
                let _ = self.registry.set_status(&task_id, TaskStatus::Running).await;
                let owner = self.clone();
                let watched_id = task_id.clone();
                let result = self.runtime.spawn("pane-teammate-reader", Box::pin(async move {
                    let sink = CoordinatorStatusSink::new(owner.team.clone(), owner.output.clone());
                    sink.set_status(&watched_id, TaskStatus::Running).await;
                    let tool = SendMessageTool::new(owner.team.clone(), tool_ui::send_message::truncate_preview).with_spawn_seam(Arc::new(owner.clone()));
                    let mut ctx = ToolUseContext::model_seed(request.model.clone().unwrap_or_default());
                    ctx.agent_id = Some(agent_id); ctx.agent_name = Some(name);
                    ctx.team_name = Some(team_name); ctx.origin_session_id = Some(owner.session_id);
                    let mut ended = false;
                    let mut terminal_report = None;
                    // Keep this deadline across frames: continuously arriving
                    // output must not defer terminal-state cleanup forever.
                    let mut status_poll = tokio::time::interval(Duration::from_millis(200));
                    status_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! {
                            frame = async {
                                if let Some(message) = early.pop_front() { Ok(Some(message)) }
                                else { reader.next_frame().await }
                            } => {
                                let message = match frame {
                                    Ok(Some(message)) => message,
                                    _ => break,
                                };
                                match message {
                                    WorkerToParent::Output { text } => {
                                        if let Ok(path) = owner.registry.output_manager.path_for(&watched_id) {
                                            let _ = owner.registry.output_manager.append(&path, &text).await;
                                        }
                                    }
                                    WorkerToParent::State { status, error, awaiting_plan_approval } => {
                                        let _ = owner.registry.set_awaiting_plan_approval(&watched_id, awaiting_plan_approval).await;
                                        sink.set_awaiting_plan_approval(&watched_id, awaiting_plan_approval).await;
                                        let status = match status.as_str() {
                                            "running" => TaskStatus::Running,
                                            "completed" => TaskStatus::Completed,
                                            "failed" => TaskStatus::Failed,
                                            "killed" => TaskStatus::Killed,
                                            "idle" => {
                                                sink.set_teammate_idle(&watched_id).await;
                                                continue;
                                            }
                                            _ => continue,
                                        };
                                        if status.is_terminal() {
                                            terminal_report = Some((status, error));
                                            ended = true;
                                            break;
                                        }
                                        let _ = owner.registry.set_status(&watched_id, status).await;
                                        sink.set_status(&watched_id, status).await;
                                    }
                                    WorkerToParent::CoordinatorMessage { message } => {
                                        if let Ok(mut message) = serde_json::from_value::<coordinator::mailbox::TeammateMessage>(message) {
                                            // Attribute the authenticated channel to its member,
                                            // never to the untrusted payload's supplied sender.
                                            message.from = coordinator::mailbox::MessageSender::Teammate(agent_id);
                                            message.from_name = ctx.agent_name.clone().unwrap_or_default();
                                            let _ = owner.team.mailbox_router.route(&owner.team.coordinator_id, message).await;
                                        }
                                    }
                                    WorkerToParent::SendMessage { id, input } => {
                                        let (tx, rx) = tokio::sync::mpsc::channel(16);
                                        drop(rx);
                                        let (result, is_error) = match tool.call(input, ctx.clone(), tx).await {
                                            Ok(result) => (result.data, result.is_error),
                                            Err(error) => (serde_json::json!({"error":error.to_string()}), true),
                                        };
                                        if Self::write(&writer, ParentToWorker::SendMessageResult { id, result, is_error }).await.is_err() { break; }
                                    }
                                    _ => break,
                                }
                            }
                            _ = status_poll.tick() => {
                                if writer.poisoned.load(Ordering::Acquire) { break; }
                                if owner.registry.get(&watched_id).await.is_none_or(|state| state.base().status.is_terminal()) {
                                    let _ = Self::write(&writer, ParentToWorker::Shutdown).await;
                                    ended = true;
                                    break;
                                }
                            }
                        }
                    }
                    if terminal_report.is_none() && !ended {
                        terminal_report = Some(if stopping.load(Ordering::Acquire) {
                            (TaskStatus::Killed, None)
                        } else {
                            (TaskStatus::Failed, Some("Teammate transport closed".to_owned()))
                        });
                    }
                    match owner.finish_pane(&watched_id, terminal_report).await {
                        Ok(()) => {}
                        Err(error) => {
                            // No terminal child report + unconfirmed backend kill
                            // means the task must stay stoppable through TaskStop.
                            tracing::warn!(task_id = %watched_id, %error, "Pane cleanup remains retryable");
                            owner.output.emit_system_notice(&format!("Teammate transport closed, but its pane could not be terminated: {error}. Stop the task again to retry."), true).await;
                        }
                    }
                })).await;
                if let Err(error) = result {
                    match self.cleanup(&task_id).await {
                        Ok(()) => {
                            let _ = self.registry.set_status(&task_id, TaskStatus::Failed).await;
                            return Err(internal(error));
                        }
                        Err(cleanup_error) => {
                            let message = format!("Teammate reader failed to start for task {task_id}: {error}. Pane termination could not be confirmed: {cleanup_error}. Stop task {task_id} to retry.");
                            self.output.emit_system_notice(&message, true).await;
                            return Err(internal(message));
                        }
                    }
                }
                Ok(task_id.clone())
            }.await;
            if let Err(startup_error) = &launched {
                if !registered {
                    let cleanup_error = match tokio::time::timeout(BACKEND_CLEANUP_TIMEOUT, backend.kill_pane(&pane)).await {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("Pane termination timed out".to_owned()),
                    };
                    if let Some(cleanup_error) = cleanup_error {
                        self.tasks.lock().await.insert(task_id.clone(), PaneTask {
                            writer: None, teardown: Arc::new(Mutex::new(())), terminated: Arc::new(AtomicBool::new(false)),
                            stopping: Arc::new(AtomicBool::new(true)), pane: pane.clone(), directory: directory.clone(),
                            metadata: None, backend: backend.clone(),
                        });
                        self.registry.register_external_teammate_task(&task_id).await;
                        let _ = self.registry.set_status(&task_id, TaskStatus::Running).await;
                        let message = format!("Teammate startup failed: {startup_error}. Pane termination could not be confirmed: {cleanup_error}. Stop task {task_id} to retry.");
                        self.output.emit_system_notice(&message, true).await;
                        return Err(internal(message));
                    }
                    let _ = self.registry.set_status(&task_id, TaskStatus::Failed).await;
                }
            }
            launched
        }.await;
        if result.is_err() {
            let _ = tokio::fs::remove_dir_all(&directory).await;
        }
        result
    }
}

fn internal(error: impl std::fmt::Display) -> TeamSpawnError {
    TeamSpawnError::Internal(error.to_string())
}
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[async_trait]
impl TeamSpawnSeam for PaneTeammateSpawner {
    async fn spawn_teammate(
        &self,
        agent_id: AgentId,
        name: String,
        team_name: String,
        description: String,
    ) -> Result<String, TeamSpawnError> {
        self.registry
            .spawn_teammate(agent_id, name, team_name, description)
            .await
    }
    async fn spawn_teammate_request(
        &self,
        agent_id: AgentId,
        name: String,
        team_name: String,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<String, TeamSpawnError> {
        if let Some(selector) = &self.backend_selector {
            let selection = selector();
            let mut selected = self.clone();
            selected.backend = selection.backend;
            selected.explicit = selection.explicit;
            selected.backend_error = selection.error;
            selected.backend_selector = None;
            return selected
                .spawn_teammate_request(agent_id, name, team_name, request, inherit)
                .await;
        }
        if let Some(error) = &self.backend_error {
            if self.explicit {
                return Err(internal(error));
            }
            tracing::warn!(
                "[handleSpawn] No pane backend available, falling back to in-process: {error}"
            );
            if !self.fallback_warned.swap(true, Ordering::SeqCst) {
                let hint = if std::env::var("TERM_PROGRAM").as_deref() == Ok("iTerm.app") {
                    "To force iTerm2 panes, set teammateMode: \"iterm2\" in settings and enable the iTerm2 Python API (Preferences > General > Magic)."
                } else {
                    "To use terminal panes, set teammateMode: \"tmux\" in settings."
                };
                self.output
                    .emit_system_notice(
                        &format!(
                            "Couldn't open a teammate pane — running in-process instead. {hint}"
                        ),
                        false,
                    )
                    .await;
            }
        } else if self.backend.is_some() {
            return self.launch_owned(agent_id, name, team_name, request).await;
        } else if self.explicit {
            return Err(internal("No terminal backend available"));
        }
        self.registry
            .spawn_teammate_request(agent_id, name, team_name, request, inherit)
            .await
    }
    async fn pane_metadata(&self, task_id: &str) -> Option<PaneLaunchMetadata> {
        self.tasks
            .lock()
            .await
            .get(task_id)
            .and_then(|task| task.metadata.clone())
    }
    async fn send_message(&self, task_id: &str, message: String) -> Result<(), TeamSpawnError> {
        let writer = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .map(|task| task.writer.clone());
        if let Some(writer) = writer {
            let writer = writer.ok_or(TeamSpawnError::Terminated)?;
            Self::write(&writer, ParentToWorker::Message { text: message }).await
        } else {
            self.registry.send_message(task_id, message).await
        }
    }
    async fn apply_plan_approval(
        &self,
        task_id: &str,
        response: lingxi_core::host::teammate_plan::PlanApprovalResponse,
    ) -> Result<(), TeamSpawnError> {
        let writer = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .map(|task| task.writer.clone());
        if let Some(writer) = writer {
            let writer = writer.ok_or(TeamSpawnError::Terminated)?;
            Self::write(&writer, ParentToWorker::PlanApprovalResponse { response }).await
        } else {
            self.registry.apply_plan_approval(task_id, response).await
        }
    }
    async fn kill(&self, task_id: &str) -> Result<(), TeamSpawnError> {
        if self.tasks.lock().await.contains_key(task_id) {
            // Teardown must survive cancellation of the requesting tool call.
            let owner = self.clone();
            let task_id = task_id.to_owned();
            let (send, receive) = tokio::sync::oneshot::channel();
            self.runtime
                .spawn(
                    "pane-teammate-stop",
                    Box::pin(async move {
                        let _ = send.send(owner.stop_pane(&task_id).await);
                    }),
                )
                .await
                .map_err(internal)?;
            receive.await.map_err(internal)?
        } else {
            self.registry.kill(task_id).await.map_err(internal)
        }
    }
    async fn is_alive(&self, task_id: &str) -> bool {
        TeamSpawnSeam::is_alive(self.registry.as_ref(), task_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::swarm::{SwarmError, SwarmHandle, SwarmLayout};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn transcript(frames: Vec<WorkerToParent>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for frame in frames {
            bytes.extend(serde_json::to_vec(&frame).unwrap());
            bytes.push(b'\n');
        }
        bytes
    }

    #[tokio::test]
    async fn ready_handshake_preserves_early_rpc_and_output_order() {
        let bytes = transcript(vec![
            WorkerToParent::Hello {
                token: "secret".into(),
            },
            WorkerToParent::SendMessage {
                id: 7,
                input: serde_json::json!({"to":"main","message":"early"}),
            },
            WorkerToParent::Output {
                text: "first output".into(),
            },
            WorkerToParent::Ready {
                task_id: "child-id".into(),
            },
            WorkerToParent::State {
                status: "running".into(),
                error: None,
                awaiting_plan_approval: false,
            },
        ]);
        let mut reader = WorkerReader::new(BufReader::new(bytes.as_slice()));
        let mut early = authenticate_until_ready(&mut reader, "secret")
            .await
            .unwrap();
        assert!(
            matches!(early.pop_front(), Some(WorkerToParent::SendMessage { id:7, input }) if input["message"] == "early")
        );
        assert!(
            matches!(early.pop_front(), Some(WorkerToParent::Output { text }) if text == "first output")
        );
        assert!(early.is_empty());
        assert!(
            matches!(reader.next_frame().await.unwrap(), Some(WorkerToParent::State { status, .. }) if status == "running")
        );
    }

    #[tokio::test]
    async fn worker_reader_keeps_partial_frame_when_status_poll_cancels_read() {
        let (mut writer, stream) = tokio::io::duplex(1024);
        let mut reader = WorkerReader::new(BufReader::new(stream));
        writer
            .write_all(b"{\"type\":\"output\",\"text\":\"part")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.next_frame())
                .await
                .is_err()
        );
        writer.write_all(b"ial\"}\n").await.unwrap();
        assert!(
            matches!(reader.next_frame().await.unwrap(), Some(WorkerToParent::Output { text }) if text == "partial")
        );
    }

    #[tokio::test]
    async fn handshake_reports_early_failure_and_bounds_buffer() {
        let failure = transcript(vec![
            WorkerToParent::Hello {
                token: "secret".into(),
            },
            WorkerToParent::State {
                status: "failed".into(),
                error: Some("startup failure".into()),
                awaiting_plan_approval: false,
            },
        ]);
        let mut reader = WorkerReader::new(BufReader::new(failure.as_slice()));
        assert!(authenticate_until_ready(&mut reader, "secret")
            .await
            .unwrap_err()
            .to_string()
            .contains("Teammate terminated before Ready (failed): startup failure"));
        let mut frames = vec![WorkerToParent::Hello {
            token: "secret".into(),
        }];
        frames.extend((0..=MAX_EARLY_FRAMES).map(|_| WorkerToParent::Output {
            text: "early".into(),
        }));
        let bytes = transcript(frames);
        let mut reader = WorkerReader::new(BufReader::new(bytes.as_slice()));
        assert!(authenticate_until_ready(&mut reader, "secret")
            .await
            .unwrap_err()
            .to_string()
            .contains("pre-Ready message buffer"));
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = WorkerReader::new(BufReader::new(oversized.as_slice()));
        assert!(reader
            .next_frame()
            .await
            .unwrap_err()
            .to_string()
            .contains("frame exceeds 1 MiB"));
    }

    struct QuietOutput;
    #[async_trait]
    impl OutputStream for QuietOutput {
        async fn emit_text(&self, _: &str) {}
        async fn emit_tool_call(
            &self,
            _: &lingxi_core::types::ToolUseId,
            _: &str,
            _: &serde_json::Value,
        ) {
        }
        async fn emit_tool_result(
            &self,
            _: &lingxi_core::types::ToolUseId,
            _: &str,
            _: &str,
            _: &serde_json::Value,
        ) {
        }
        async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
    }

    struct FailingBackend {
        created: AtomicUsize,
        killed: AtomicUsize,
        kill_failures: AtomicUsize,
        root: PathBuf,
        invalid_auth: bool,
    }
    #[async_trait]
    impl SwarmBackend for FailingBackend {
        async fn start_swarm(&self, _: SwarmLayout) -> Result<SwarmHandle, SwarmError> {
            Err(SwarmError::Unsupported)
        }
        async fn create_teammate_pane(
            &self,
            _: &AgentId,
            _: PanePosition,
        ) -> Result<PaneId, SwarmError> {
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(PaneId { raw: "%9".into() })
        }
        async fn send_command_to_pane(&self, _: &PaneId, command: &str) -> Result<(), SwarmError> {
            use std::os::unix::fs::PermissionsExt;
            let directory = std::fs::read_dir(&self.root)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let manifest = directory.join("launch.json");
            assert_eq!(
                std::fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(!command.contains("private prompt"));
            assert!(command.contains("--teammate-launch-file"));
            if self.invalid_auth {
                let manifest: PaneTeammateManifest =
                    serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
                let mut stream = tokio::net::UnixStream::connect(&manifest.socket_path)
                    .await
                    .unwrap();
                stream
                    .write_all(b"{\"type\":\"hello\",\"token\":\"wrong\"}\n")
                    .await
                    .unwrap();
                Ok(())
            } else {
                Err(SwarmError::Tmux("injected dispatch failure".into()))
            }
        }
        async fn pane_metadata(&self, pane: &PaneId) -> Result<PaneLaunchMetadata, SwarmError> {
            Ok(PaneLaunchMetadata {
                session_name: "current".into(),
                window_name: "current".into(),
                pane_id: pane.raw.clone(),
                backend_type: "tmux".into(),
            })
        }
        async fn kill_pane(&self, _: &PaneId) -> Result<(), SwarmError> {
            self.killed.fetch_add(1, Ordering::SeqCst);
            if self
                .kill_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(SwarmError::Tmux("injected kill failure".into()));
            }
            Ok(())
        }
        async fn destroy_swarm(&self, _: SwarmHandle) -> Result<(), SwarmError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    struct EarlyBackend {
        root: PathBuf,
        runtime: Arc<dyn RuntimeSpawner>,
        reply: Arc<Mutex<Option<ParentToWorker>>>,
    }
    #[async_trait]
    impl SwarmBackend for EarlyBackend {
        async fn start_swarm(&self, _: SwarmLayout) -> Result<SwarmHandle, SwarmError> {
            Err(SwarmError::Unsupported)
        }
        async fn create_teammate_pane(
            &self,
            _: &AgentId,
            _: PanePosition,
        ) -> Result<PaneId, SwarmError> {
            Ok(PaneId { raw: "%12".into() })
        }
        async fn pane_metadata(&self, pane: &PaneId) -> Result<PaneLaunchMetadata, SwarmError> {
            Ok(PaneLaunchMetadata {
                backend_type: "tmux".into(),
                session_name: "current".into(),
                window_name: "current".into(),
                pane_id: pane.raw.clone(),
            })
        }
        async fn send_command_to_pane(&self, _: &PaneId, _: &str) -> Result<(), SwarmError> {
            let directory = std::fs::read_dir(&self.root)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let manifest: PaneTeammateManifest =
                serde_json::from_slice(&std::fs::read(directory.join("launch.json")).unwrap())
                    .unwrap();
            let mut stream = tokio::net::UnixStream::connect(manifest.socket_path)
                .await
                .unwrap();
            stream
                .write_all(&transcript(vec![
                    WorkerToParent::Hello {
                        token: manifest.token,
                    },
                    WorkerToParent::Output {
                        text: "before Ready\n".into(),
                    },
                    WorkerToParent::SendMessage {
                        id: 7,
                        input: serde_json::json!({"to":"main","message":"early RPC"}),
                    },
                    WorkerToParent::Ready {
                        task_id: "child-task".into(),
                    },
                ]))
                .await
                .unwrap();
            let reply = self.reply.clone();
            self.runtime
                .spawn(
                    "fake-pane-worker",
                    Box::pin(async move {
                        let mut lines = BufReader::new(stream).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let value: ParentToWorker = serde_json::from_str(&line).unwrap();
                            if matches!(value, ParentToWorker::Shutdown) {
                                break;
                            }
                            *reply.lock().await = Some(value);
                        }
                    }),
                )
                .await
                .unwrap();
            Ok(())
        }
        async fn kill_pane(&self, _: &PaneId) -> Result<(), SwarmError> {
            Ok(())
        }
        async fn destroy_swarm(&self, _: SwarmHandle) -> Result<(), SwarmError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn unix_worker_early_rpc_is_replied_to_after_ready() {
        let root = tempfile::Builder::new()
            .prefix("lxt-")
            .tempdir_in("/tmp")
            .unwrap();
        let spool = tempfile::tempdir().unwrap();
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let fs = Arc::new(platform_posix::PosixFileSystem::new(
            spool.path().to_owned(),
        ));
        let registry = Arc::new(TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                spool.path().to_owned(),
                fs,
            )),
        ));
        let team = Arc::new(TeamRegistry::new(AgentId::new()));
        team.mailbox_router
            .register(
                team.coordinator_id,
                Arc::new(coordinator::mailbox::TeammateMailbox::new(
                    team.coordinator_id,
                )),
            )
            .await;
        let reply = Arc::new(Mutex::new(None));
        let backend = Arc::new(EarlyBackend {
            root: root.path().to_owned(),
            runtime: runtime.clone(),
            reply: reply.clone(),
        });
        let spawner = PaneTeammateSpawner::new(
            registry.clone(),
            team,
            runtime.clone(),
            Arc::new(QuietOutput),
            SessionId::new(),
            root.path().to_owned(),
            Some(backend),
            true,
            std::env::current_exe().unwrap(),
        );
        let task_id = spawner
            .launch_pane(
                AgentId::new(),
                "scout".into(),
                "session".into(),
                SubagentSpawnRequest::default(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if reply.lock().await.is_some() {
                    break;
                }
                runtime.sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("early RPC must be replayed and receive a response");
        assert!(matches!(
            reply.lock().await.as_ref(),
            Some(ParentToWorker::SendMessageResult { id: 7, .. })
        ));
        let text = tokio::fs::read_to_string(registry.output_manager.path_for(&task_id).unwrap())
            .await
            .unwrap();
        assert!(text.contains("before Ready"));
        TeamSpawnSeam::kill(&spawner, &task_id).await.unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn stop_reaps_pane_and_private_material_when_writer_lock_is_stuck() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private-launch");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("launch.json"), "private token").unwrap();
        let spool = tempfile::tempdir().unwrap();
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let fs = Arc::new(platform_posix::PosixFileSystem::new(
            spool.path().to_owned(),
        ));
        let registry = Arc::new(TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                spool.path().to_owned(),
                fs,
            )),
        ));
        let agent_id = AgentId::new();
        let task_id = registry
            .create(
                TaskType::InProcessTeammate,
                TaskSpawnInput::InProcessTeammate {
                    agent_id,
                    name: "scout".into(),
                    team_name: "session".into(),
                    description: "work".into(),
                    spawn_request: None,
                    inheritance: None,
                },
                "work".into(),
            )
            .await
            .unwrap();
        let backend = Arc::new(FailingBackend {
            created: AtomicUsize::new(0),
            killed: AtomicUsize::new(0),
            kill_failures: AtomicUsize::new(0),
            root: root.path().to_owned(),
            invalid_auth: false,
        });
        let spawner = PaneTeammateSpawner::new(
            registry.clone(),
            Arc::new(TeamRegistry::new(AgentId::new())),
            runtime,
            Arc::new(QuietOutput),
            SessionId::new(),
            root.path().to_owned(),
            Some(backend.clone()),
            true,
            std::env::current_exe().unwrap(),
        );
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = stream.into_split();
        let writer = Arc::new(ControlWriter::new(write));
        let _held = writer.stream.lock().await;
        spawner.tasks.lock().await.insert(
            task_id.clone(),
            PaneTask {
                writer: Some(writer.clone()),
                teardown: Arc::new(Mutex::new(())),
                terminated: Arc::new(AtomicBool::new(false)),
                stopping: Arc::new(AtomicBool::new(false)),
                pane: PaneId { raw: "%9".into() },
                directory: directory.clone(),
                metadata: Some(PaneLaunchMetadata {
                    session_name: "current".into(),
                    window_name: "current".into(),
                    pane_id: "%9".into(),
                    backend_type: "tmux".into(),
                }),
                backend: backend.clone(),
            },
        );
        tokio::time::timeout(
            CONTROL_WRITE_TIMEOUT + Duration::from_secs(2),
            TeamSpawnSeam::kill(&spawner, &task_id),
        )
        .await
        .expect("stop must not wait for the held writer lock")
        .unwrap();
        assert_eq!(backend.killed.load(Ordering::SeqCst), 1);
        assert!(!directory.exists());
        assert!(spawner.tasks.lock().await.is_empty());
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Killed
        );
    }

    #[tokio::test]
    async fn failed_backend_kill_retains_generic_taskstop_retry_ownership() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private-launch");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("launch.json"), "private token").unwrap();
        let spool = tempfile::tempdir().unwrap();
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let fs = Arc::new(platform_posix::PosixFileSystem::new(
            spool.path().to_owned(),
        ));
        let registry = Arc::new(TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                spool.path().to_owned(),
                fs,
            )),
        ));
        let agent_id = AgentId::new();
        let task_id = registry
            .create(
                TaskType::InProcessTeammate,
                TaskSpawnInput::InProcessTeammate {
                    agent_id,
                    name: "scout".into(),
                    team_name: "session".into(),
                    description: "work".into(),
                    spawn_request: None,
                    inheritance: None,
                },
                "work".into(),
            )
            .await
            .unwrap();
        let backend = Arc::new(FailingBackend {
            created: AtomicUsize::new(0),
            killed: AtomicUsize::new(0),
            kill_failures: AtomicUsize::new(2),
            root: root.path().to_owned(),
            invalid_auth: false,
        });
        let spawner = PaneTeammateSpawner::new(
            registry.clone(),
            Arc::new(TeamRegistry::new(AgentId::new())),
            runtime,
            Arc::new(QuietOutput),
            SessionId::new(),
            root.path().to_owned(),
            Some(backend.clone()),
            true,
            std::env::current_exe().unwrap(),
        );
        let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = stream.into_split();
        let writer = Arc::new(ControlWriter::new(write));
        spawner.tasks.lock().await.insert(
            task_id.clone(),
            PaneTask {
                writer: Some(writer.clone()),
                teardown: Arc::new(Mutex::new(())),
                terminated: Arc::new(AtomicBool::new(false)),
                stopping: Arc::new(AtomicBool::new(false)),
                pane: PaneId { raw: "%9".into() },
                directory: directory.clone(),
                metadata: Some(PaneLaunchMetadata {
                    session_name: "current".into(),
                    window_name: "current".into(),
                    pane_id: "%9".into(),
                    backend_type: "tmux".into(),
                }),
                backend: backend.clone(),
            },
        );
        let spawner = Arc::new(spawner);
        let controller: Arc<dyn TeamSpawnSeam> = spawner.clone();
        registry
            .set_external_teammate_controller(Arc::downgrade(&controller))
            .await;
        registry.register_external_teammate_task(&task_id).await;
        registry
            .set_status(&task_id, TaskStatus::Running)
            .await
            .unwrap();
        let error = spawner
            .finish_pane(&task_id, Some((TaskStatus::Completed, None)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected kill failure"));
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Running
        );
        assert!(spawner.tasks.lock().await.contains_key(&task_id));
        assert!(
            !directory.exists(),
            "private material must be scrubbed on failure"
        );
        assert!(registry.kill(&task_id).await.is_err());
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Running
        );
        registry.kill(&task_id).await.unwrap();
        assert_eq!(backend.killed.load(Ordering::SeqCst), 3);
        assert!(!spawner.tasks.lock().await.contains_key(&task_id));
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Killed
        );
    }

    #[tokio::test]
    async fn approved_departure_finishes_once_after_public_taskstop_retry() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private-launch");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("launch.json"), "private token").unwrap();
        let spool = tempfile::tempdir().unwrap();
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let fs = Arc::new(platform_posix::PosixFileSystem::new(
            spool.path().to_owned(),
        ));
        let registry = Arc::new(TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                spool.path().to_owned(),
                fs,
            )),
        ));
        let agent_id = AgentId::new();
        let task_id = registry
            .create(
                TaskType::InProcessTeammate,
                TaskSpawnInput::InProcessTeammate {
                    agent_id,
                    name: "scout".into(),
                    team_name: "session".into(),
                    description: "work".into(),
                    spawn_request: None,
                    inheritance: None,
                },
                "work".into(),
            )
            .await
            .unwrap();
        let backend = Arc::new(FailingBackend {
            created: AtomicUsize::new(0),
            killed: AtomicUsize::new(0),
            kill_failures: AtomicUsize::new(1),
            root: root.path().to_owned(),
            invalid_auth: false,
        });
        let team =
            Arc::new(TeamRegistry::new(AgentId::new()).with_config_home(root.path().to_owned()));
        team.set_team_name(Some("session".into())).await;
        team.register_worker(agent_id, "explorer".into(), "scout".into(), task_id.clone())
            .await
            .unwrap();
        let mailbox = Arc::new(coordinator::mailbox::TeammateMailbox::new(
            team.coordinator_id,
        ));
        team.mailbox_router
            .register(team.coordinator_id, mailbox.clone())
            .await;
        let config_path = coordinator::team_file::team_file_path(root.path(), "session");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let config = serde_json::json!({
            "name":"session", "createdAt":0, "leadAgentId":"team-lead@session",
            "members":[{"agentId":"team-lead@session", "name":"team-lead"},
                       {"agentId":agent_id.to_string(), "name":"scout"}]
        });
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        // Seed the public persisted task format without adding a task-store dependency.
        let list_id = std::env::var("LINGXI_TASK_LIST_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "session".into());
        let list_path: String = list_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let shared_task_path = root.path().join("tasks").join(list_path).join("1.json");
        std::fs::create_dir_all(shared_task_path.parent().unwrap()).unwrap();
        let shared_task = serde_json::json!({"id":"1", "subject":"Fix parser", "description":"work",
            "owner":"scout", "status":"in_progress", "blocks":[], "blockedBy":[]});
        std::fs::write(&shared_task_path, serde_json::to_vec(&shared_task).unwrap()).unwrap();
        let config_before = std::fs::read(&config_path).unwrap();
        let task_before = std::fs::read(&shared_task_path).unwrap();
        let spawner = PaneTeammateSpawner::new(
            registry.clone(),
            team.clone(),
            runtime,
            Arc::new(QuietOutput),
            SessionId::new(),
            root.path().to_owned(),
            Some(backend.clone()),
            true,
            std::env::current_exe().unwrap(),
        );
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = stream.into_split();
        let writer = Arc::new(ControlWriter::new(write));
        spawner.tasks.lock().await.insert(
            task_id.clone(),
            PaneTask {
                writer: Some(writer.clone()),
                teardown: Arc::new(Mutex::new(())),
                terminated: Arc::new(AtomicBool::new(false)),
                stopping: Arc::new(AtomicBool::new(false)),
                pane: PaneId { raw: "%9".into() },
                directory: directory.clone(),
                metadata: Some(PaneLaunchMetadata {
                    session_name: "current".into(),
                    window_name: "current".into(),
                    pane_id: "%9".into(),
                    backend_type: "tmux".into(),
                }),
                backend: backend.clone(),
            },
        );
        let spawner = Arc::new(spawner);
        let controller: Arc<dyn TeamSpawnSeam> = spawner.clone();
        registry
            .set_external_teammate_controller(Arc::downgrade(&controller))
            .await;
        registry.register_external_teammate_task(&task_id).await;
        registry
            .set_status(&task_id, TaskStatus::Running)
            .await
            .unwrap();
        let tool = SendMessageTool::new(team.clone(), tool_ui::send_message::truncate_preview)
            .with_spawn_seam(spawner.clone());
        let mut context = ToolUseContext::model_seed("test-model".into());
        context.agent_id = Some(agent_id);
        let (progress, _events) = tool_api::progress::progress_channel();
        let error = tool
            .call(
                serde_json::json!({"to":"team-lead", "message":{
                    "type":"shutdown_response", "request_id":"approved-stop", "approve":true
                }}),
                context,
                progress,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected kill failure"));
        let mut child = BufReader::new(peer);
        let mut frame = String::new();
        tokio::time::timeout(Duration::from_secs(1), child.read_line(&mut frame))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            serde_json::from_str::<ParentToWorker>(&frame).unwrap(),
            ParentToWorker::Shutdown
        ));
        drop(child); // The approval reached the child; it has now exited.
        assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
        assert_eq!(std::fs::read(&shared_task_path).unwrap(), task_before);
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Running
        );
        assert!(spawner.tasks.lock().await.contains_key(&task_id));
        let approval = mailbox.drain();
        assert_eq!(approval.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&approval[0].content).unwrap()["type"],
            "shutdown_approved"
        );

        // Exercise the public host handle used by TaskStop, not the pane seam directly.
        let public: &dyn lingxi_core::host::TaskRegistryHandle = registry.as_ref();
        let held_config = tool_task::proper_lockfile::lock(&config_path)
            .await
            .unwrap();
        assert!(public.kill(&task_id).await.is_err());
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Running
        );
        assert!(spawner
            .tasks
            .lock()
            .await
            .get(&task_id)
            .unwrap()
            .terminated
            .load(Ordering::Acquire));
        assert_eq!(backend.killed.load(Ordering::SeqCst), 2);
        assert!(mailbox.drain().is_empty());
        assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
        assert_eq!(std::fs::read(&shared_task_path).unwrap(), task_before);
        drop(held_config);
        public.kill(&task_id).await.unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(config["members"].as_array().unwrap().len(), 1);
        assert_eq!(config["members"][0]["name"], "team-lead");
        let task: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&shared_task_path).unwrap()).unwrap();
        assert!(task.get("owner").is_none());
        assert_eq!(task["status"], "pending");
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Killed
        );
        assert!(!spawner.tasks.lock().await.contains_key(&task_id));
        assert_eq!(backend.killed.load(Ordering::SeqCst), 2);
        let terminated = mailbox.drain();
        assert_eq!(terminated.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&terminated[0].content).unwrap()["type"],
            "teammate_terminated"
        );

        let (repeated_stop, reader_finish, other_finish) = tokio::join!(
            public.kill(&task_id),
            spawner.finish_pane(&task_id, Some((TaskStatus::Killed, None))),
            spawner.finish_pane(&task_id, Some((TaskStatus::Completed, None))),
        );
        repeated_stop.unwrap();
        reader_finish.unwrap();
        other_finish.unwrap();
        assert!(
            mailbox.drain().is_empty(),
            "departure notification must be emitted exactly once"
        );
        assert_eq!(backend.killed.load(Ordering::SeqCst), 2);
        assert_eq!(
            registry.get(&task_id).await.unwrap().base().status,
            TaskStatus::Killed
        );
    }

    #[tokio::test]
    async fn partial_write_timeout_poisoning_prevents_retry_frame_concatenation() {
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = stream.into_split();
        let writer = ControlWriter::new(write);
        let error = PaneTeammateSpawner::write(
            &writer,
            ParentToWorker::Message {
                text: "x".repeat(16 * 1024 * 1024),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, TeamSpawnError::Terminated));
        assert!(writer.poisoned.load(Ordering::Acquire));
        peer.readable().await.unwrap();
        let mut buffer = [0_u8; 64 * 1024];
        let mut written = 0;
        loop {
            match peer.try_read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    written += read;
                    assert!(!buffer[..read].contains(&b'\n'));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("read partial frame: {error}"),
            }
        }
        assert!(written > 0, "test must actually interrupt a partial frame");
        assert!(matches!(
            PaneTeammateSpawner::write(&writer, ParentToWorker::Shutdown).await,
            Err(TeamSpawnError::Terminated)
        ));
        assert!(
            matches!(peer.try_read(&mut buffer), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[tokio::test]
    async fn cancelled_partial_write_also_poisoned_the_parent_transport() {
        let (stream, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = stream.into_split();
        let writer = Arc::new(ControlWriter::new(write));
        let writing = writer.clone();
        let pending = tokio::spawn(async move {
            PaneTeammateSpawner::write(
                &writing,
                ParentToWorker::Message {
                    text: "x".repeat(16 * 1024 * 1024),
                },
            )
            .await
        });
        let mut first = [0_u8; 16];
        peer.read_exact(&mut first).await.unwrap();
        pending.abort();
        let _ = pending.await;
        assert!(writer.poisoned.load(Ordering::Acquire));
        assert!(matches!(
            PaneTeammateSpawner::write(&writer, ParentToWorker::Shutdown).await,
            Err(TeamSpawnError::Terminated)
        ));
    }

    #[tokio::test]
    async fn terminal_publication_uses_registry_winner_for_late_finish_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let fs = Arc::new(platform_posix::PosixFileSystem::new(root.path().to_owned()));
        let registry = Arc::new(TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                root.path().to_owned(),
                fs,
            )),
        ));
        let team = Arc::new(TeamRegistry::new(AgentId::new()));
        let spawner = PaneTeammateSpawner::new(
            registry.clone(),
            team.clone(),
            runtime,
            Arc::new(QuietOutput),
            SessionId::new(),
            root.path().to_owned(),
            None,
            false,
            std::env::current_exe().unwrap(),
        );
        for (index, winner) in [TaskStatus::Killed, TaskStatus::Completed]
            .into_iter()
            .enumerate()
        {
            let name = format!("worker-{index}");
            let agent_id = team
                .spawn_worker("explorer".into(), name.clone(), String::new())
                .await
                .unwrap();
            let task_id = registry
                .create(
                    TaskType::InProcessTeammate,
                    TaskSpawnInput::InProcessTeammate {
                        agent_id,
                        name,
                        team_name: "session".into(),
                        description: String::new(),
                        spawn_request: None,
                        inheritance: None,
                    },
                    String::new(),
                )
                .await
                .unwrap();
            team.set_task_id(&agent_id, task_id.clone()).await;
            registry.set_status(&task_id, winner).await.unwrap();
            if winner == TaskStatus::Killed {
                spawner
                    .finish_pane(
                        &task_id,
                        Some((TaskStatus::Failed, Some("late failure".into()))),
                    )
                    .await
                    .unwrap();
            } else {
                spawner.stop_pane(&task_id).await.unwrap();
            }
            assert_eq!(registry.get(&task_id).await.unwrap().base().status, winner);
            let status = team.find_by_agent_id(&agent_id).await.unwrap().status;
            if winner == TaskStatus::Killed {
                assert_eq!(status, coordinator::team_registry::WorkerStatus::Killed);
            } else {
                assert_eq!(status, coordinator::team_registry::WorkerStatus::Completed);
            }
        }
    }

    #[tokio::test]
    async fn output_allocation_failure_never_creates_an_external_pane() {
        let root = tempfile::Builder::new()
            .prefix("lxt-")
            .tempdir_in("/tmp")
            .unwrap();
        let spool = tempfile::tempdir().unwrap();
        let blocked = spool.path().join("not-a-directory");
        std::fs::write(&blocked, "file").unwrap();
        let runtime = Arc::new(platform_posix::PosixRuntime::new());
        let fs = Arc::new(platform_posix::PosixFileSystem::new(
            spool.path().to_owned(),
        ));
        let registry = Arc::new(TaskRegistry::new(
            runtime.clone(),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(blocked, fs)),
        ));
        let backend = Arc::new(FailingBackend {
            created: AtomicUsize::new(0),
            killed: AtomicUsize::new(0),
            kill_failures: AtomicUsize::new(0),
            root: root.path().to_owned(),
            invalid_auth: false,
        });
        let spawner = PaneTeammateSpawner::new(
            registry,
            Arc::new(TeamRegistry::new(AgentId::new())),
            runtime,
            Arc::new(QuietOutput),
            SessionId::new(),
            root.path().to_owned(),
            Some(backend.clone()),
            true,
            std::env::current_exe().unwrap(),
        );
        assert!(spawner
            .launch_pane(
                AgentId::new(),
                "scout".into(),
                "session".into(),
                SubagentSpawnRequest::default()
            )
            .await
            .is_err());
        assert_eq!(backend.created.load(Ordering::SeqCst), 0);
        assert_eq!(backend.killed.load(Ordering::SeqCst), 0);
        assert!(spawner.tasks.lock().await.is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn dispatch_or_authentication_failure_reaps_pane_and_private_launch_material() {
        for (invalid_auth, kill_failures) in [(false, 0), (true, 0), (false, 1), (true, 1)] {
            let root = tempfile::Builder::new()
                .prefix("lxt-")
                .tempdir_in("/tmp")
                .unwrap();
            let spool = tempfile::tempdir().unwrap();
            let runtime = Arc::new(platform_posix::PosixRuntime::new());
            let fs = Arc::new(platform_posix::PosixFileSystem::new(
                spool.path().to_owned(),
            ));
            let registry = Arc::new(TaskRegistry::new(
                runtime.clone(),
                fs.clone(),
                Arc::new(tasks::output_manager::TaskOutputManager::new(
                    spool.path().to_owned(),
                    fs,
                )),
            ));
            let backend = Arc::new(FailingBackend {
                created: AtomicUsize::new(0),
                killed: AtomicUsize::new(0),
                kill_failures: AtomicUsize::new(kill_failures),
                root: root.path().to_owned(),
                invalid_auth,
            });
            let spawner = Arc::new(PaneTeammateSpawner::new(
                registry.clone(),
                Arc::new(TeamRegistry::new(AgentId::new())),
                runtime,
                Arc::new(QuietOutput),
                SessionId::new(),
                root.path().to_owned(),
                Some(backend.clone()),
                true,
                std::env::current_exe().unwrap(),
            ));
            let controller: Arc<dyn TeamSpawnSeam> = spawner.clone();
            registry
                .set_external_teammate_controller(Arc::downgrade(&controller))
                .await;
            let error = spawner
                .launch_pane(
                    AgentId::new(),
                    "scout".into(),
                    "session".into(),
                    SubagentSpawnRequest {
                        prompt: "private prompt".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains(if invalid_auth {
                "authentication failed"
            } else {
                "injected dispatch failure"
            }));
            assert_eq!(backend.killed.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            if kill_failures == 0 {
                assert!(spawner.tasks.lock().await.is_empty());
            } else {
                let task_id = spawner.tasks.lock().await.keys().next().unwrap().clone();
                assert!(error.to_string().contains(&format!("Stop task {task_id}")));
                assert_eq!(
                    registry.get(&task_id).await.unwrap().base().status,
                    TaskStatus::Running
                );
                assert!(matches!(
                    spawner.send_message(&task_id, "unavailable".into()).await,
                    Err(TeamSpawnError::Terminated)
                ));
                registry.kill(&task_id).await.unwrap();
                assert_eq!(backend.killed.load(Ordering::SeqCst), 2);
                assert!(spawner.tasks.lock().await.is_empty());
                assert_eq!(
                    registry.get(&task_id).await.unwrap().base().status,
                    TaskStatus::Killed
                );
            }
        }
    }
}
