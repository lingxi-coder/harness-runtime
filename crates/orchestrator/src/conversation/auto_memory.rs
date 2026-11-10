//! Native local-file post-turn extraction: one tool-executing fork and a
//! coalesced trailing snapshot, owned by the conversation's runtime lifetime.

use agent::memory_fork::{MemoryForkLease, MemoryForkSettlement};
use lingxi_core::host::{
    BackgroundTaskHandle, BudgetEnforcerHandle, RuntimeSpawner, SubagentInheritance,
    SubagentResult, SubagentSpawnRequest, SubagentSpawner, ToolInvoker,
};
use lingxi_core::types::{ConversationMessage, MessageId};
use memory::agent_tools::{resolve_memory_markdown_path, MemoryAgentShell, MemoryAgentToolInvoker};
use memory::auto_memory_extract::{
    extraction_write_summary, file_extraction_message_count, has_file_extraction_user_prose,
    main_wrote_memory_since, parse_file_inventory_head, render_existing_memory_file,
    render_file_extraction_prompt_with_shell, FileExtractionPrompt, FileInventoryMetadata,
    FILE_EXTRACTION_EVERY_N_TURNS, FILE_EXTRACTION_MAX_TURNS, FILE_EXTRACTION_QUERY_SOURCE,
    FILE_INVENTORY_HEAD_BYTES, FILE_INVENTORY_HEAD_LINES,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex, RwLock, Weak,
};

/// Main-turn snapshot. The caller captures `generation()` before assembling
/// it, so clear/resume invalidates snapshots still being prepared.
#[derive(Clone)]
pub struct AutoMemorySnapshot {
    /// Current model/profile, exact parent prompt/prefix and host policy.
    /// The extraction directive is appended by the handle, never by the caller.
    pub request: SubagentSpawnRequest,
    /// Complete main-conversation model history for cursor/prose/write gates.
    pub history: Vec<ConversationMessage>,
    /// Handle epoch captured before snapshot assembly.
    pub generation: u64,
    /// Exact inherited provider source sections, tools and typed attachments.
    pub wire: Option<Arc<agent::memory_fork::MemoryForkProjection>>,
}

/// A notice backed by actual successful memory Write/Edit calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMemoryUpdate {
    /// Source-derived saved-memory/index summary.
    pub summary: String,
    /// Exact successfully written paths, in first-write order.
    pub paths: Vec<PathBuf>,
}

#[derive(Default)]
struct State {
    generation: u64,
    next_run: u64,
    active_run: Option<u64>,
    worker: Option<BackgroundTaskHandle>,
    settlement: Option<Arc<MemoryForkSettlement>>,
    retiring: Vec<tokio::sync::watch::Receiver<bool>>,
    invoker: Option<Arc<MemoryAgentToolInvoker>>,
    cursor: Option<MessageId>,
    turns_since_pass: u32,
    pending: Option<AutoMemorySnapshot>,
    updates: Vec<AutoMemoryUpdate>,
}

/// Per-main-conversation extraction lifetime. Separate instances keep the
/// source cursor, cadence, cancellation and notices out of unrelated sessions.
pub struct AutoMemoryHandle {
    memory_dir: RwLock<PathBuf>,
    spawner: Arc<dyn SubagentSpawner>,
    invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    runtime: Arc<dyn RuntimeSpawner>,
    every_n_turns: AtomicU32,
    state: Mutex<State>,
}

impl AutoMemoryHandle {
    /// Assemble extraction with the active branded/project memory directory
    /// and the host's actual provider-neutral subagent/tool/budget runtime.
    #[must_use]
    pub fn new(
        memory_dir: PathBuf,
        spawner: Arc<dyn SubagentSpawner>,
        invoker: Arc<dyn ToolInvoker>,
        budget: Arc<dyn BudgetEnforcerHandle>,
        runtime: Arc<dyn RuntimeSpawner>,
    ) -> Self {
        Self {
            memory_dir: RwLock::new(memory_dir),
            spawner,
            invoker,
            budget,
            runtime,
            every_n_turns: AtomicU32::new(FILE_EXTRACTION_EVERY_N_TURNS),
            state: Mutex::new(State::default()),
        }
    }

    /// Apply a validated positive native cadence override.
    #[must_use]
    pub fn with_every_n_turns(self, every_n_turns: u32) -> Self {
        self.set_every_n_turns(every_n_turns);
        self
    }

    /// Refresh the live, validated native cadence before submitting a turn.
    pub fn set_every_n_turns(&self, every_n_turns: u32) {
        self.every_n_turns
            .store(every_n_turns.max(1), Ordering::Release);
    }

    /// Active directory; no fallback to another brand/config root is inferred.
    #[must_use]
    pub fn memory_dir(&self) -> PathBuf {
        self.memory_dir
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Epoch that clear/resume/cancel increments before revoking the worker.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub(crate) fn snapshot_tool_invoker(&self) -> Option<Arc<dyn ToolInvoker>> {
        self.invoker.fork_for_memory(true)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Submit a completed main turn without waiting on its model work. If a
    /// pass is running, retain only the latest complete snapshot for a trailing
    /// pass; one model await never holds the state or conversation lock.
    pub async fn submit(self: &Arc<Self>, snapshot: AutoMemorySnapshot) {
        let (generation, run, settlement, lease) = loop {
            let (retiring, previous) = {
                let mut state = self.lock();
                if snapshot.generation != state.generation {
                    return;
                }
                state.retiring.retain(|done| !*done.borrow());
                let retiring = state.retiring.clone();
                if retiring.is_empty() && state.active_run.is_some() {
                    state.pending = Some(snapshot);
                    return;
                }
                let previous = state
                    .settlement
                    .as_ref()
                    .filter(|owner| !owner.is_settled())
                    .cloned();
                if retiring.is_empty() && previous.is_none() {
                    state.next_run = state.next_run.wrapping_add(1);
                    let run = state.next_run;
                    state.active_run = Some(run);
                    let settlement = Arc::new(MemoryForkSettlement::default());
                    let lease = settlement.enter();
                    state.settlement = Some(settlement.clone());
                    break (state.generation, run, settlement, lease);
                }
                (retiring, previous)
            };
            Self::join_retiring(retiring).await;
            if let Some(previous) = previous {
                previous.wait().await;
            }
        };
        let weak = Arc::downgrade(self);
        let guard = ActiveWorker {
            handle: weak.clone(),
            generation,
            run,
            _lease: lease,
        };
        let spawned = self
            .runtime
            .spawn(
                FILE_EXTRACTION_QUERY_SOURCE,
                Box::pin(agent::memory_fork::with_settlement(
                    settlement,
                    async move {
                        let _guard = guard;
                        if let Some(handle) = weak.upgrade() {
                            handle.run_snapshots(snapshot, generation, run).await;
                        }
                    },
                )),
            )
            .await;
        match spawned {
            Ok(worker) => {
                let still_owned = {
                    let mut state = self.lock();
                    if state.generation == generation && state.active_run == Some(run) {
                        state.worker = Some(worker.clone());
                        true
                    } else {
                        false
                    }
                };
                if !still_owned {
                    let _ = self.runtime.cancel(&worker).await;
                }
            }
            Err(error) => {
                let mut state = self.lock();
                if state.generation == generation && state.active_run == Some(run) {
                    state.active_run = None;
                    state.pending = None;
                }
                tracing::warn!(%error, "could not start background memory extraction");
            }
        }
    }

    async fn run_snapshots(
        self: Arc<Self>,
        mut snapshot: AutoMemorySnapshot,
        generation: u64,
        run: u64,
    ) {
        loop {
            if !self.is_current(generation, run) {
                return;
            }
            self.run_one(&mut snapshot, generation, run).await;
            let next = {
                let mut state = self.lock();
                if state.generation != generation || state.active_run != Some(run) {
                    return;
                }
                state
                    .invoker
                    .take()
                    .inspect(|invoker| invoker.set_active(false));
                // Native runs a trailing pass only for cadence <= 1. For a
                // larger cadence, keep all messages uncovered for the next
                // ordinary turn, rather than inventing an extra trigger.
                let next = if self.every_n_turns.load(Ordering::Acquire) <= 1 {
                    state.pending.take()
                } else {
                    state.pending = None;
                    None
                };
                if next.is_none() {
                    // Release ownership in the same critical section that
                    // observes no trailing work. A concurrently submitted turn
                    // must either join this queue or acquire a new worker.
                    state.active_run = None;
                    state.worker = None;
                }
                next
            };
            let Some(next) = next else {
                return;
            };
            snapshot = next;
        }
    }

    fn is_current(&self, generation: u64, run: u64) -> bool {
        let state = self.lock();
        state.generation == generation && state.active_run == Some(run)
    }

    async fn run_one(&self, snapshot: &mut AutoMemorySnapshot, generation: u64, run: u64) {
        // A pass owns one directory identity. A setting update replaces the
        // root and advances the epoch together, revoking this pass's invoker.
        let memory_dir = self.memory_dir();
        let cursor = self.lock().cursor;
        let tail = snapshot.history.last().map(ConversationMessage::id);
        let cwd = snapshot.request.cwd.as_deref().map(Path::new);
        let already_written = main_wrote_memory_since(&snapshot.history, cursor.as_ref(), |path| {
            resolve_memory_markdown_path(&memory_dir, path, cwd).is_some()
        });
        let has_prose = has_file_extraction_user_prose(&snapshot.history, cursor.as_ref());
        let should_run = {
            let mut state = self.lock();
            if state.generation != generation || state.active_run != Some(run) {
                return;
            }
            if already_written || !has_prose {
                state.cursor = tail;
                false
            } else {
                state.turns_since_pass = state.turns_since_pass.saturating_add(1);
                if state.turns_since_pass >= self.every_n_turns.load(Ordering::Acquire) {
                    state.turns_since_pass = 0;
                    true
                } else {
                    false
                }
            }
        };
        if !should_run {
            return;
        }
        if let Err(error) = tokio::fs::create_dir_all(&memory_dir).await {
            tracing::warn!(%error, "could not prepare background memory directory");
            return;
        }
        let inventory = existing_memory_files(&memory_dir).await;
        if !self.is_current(generation, run) {
            return;
        }
        let shell = snapshot
            .wire
            .as_ref()
            .map_or_else(MemoryAgentShell::default, |wire| {
                MemoryAgentShell::from_tools(&wire.tools)
            });
        let prompt = render_file_extraction_prompt_with_shell(
            FileExtractionPrompt {
                message_count: file_extraction_message_count(&snapshot.history, cursor.as_ref()),
                existing_memory_files: &inventory,
                has_scope_guidance: false,
                project_skill_upkeep: false,
                shared_memory_tools_unavailable: false,
                posix: shell.is_posix(),
            },
            shell.name(),
        );
        let request = &mut snapshot.request;
        request.prompt = prompt.clone().into();
        // A fork's prompt_messages are empty in the agent runner. The native
        // directive therefore belongs in the inherited conversation carrier.
        let mut prefix = request
            .fork_context_messages
            .take()
            .unwrap_or_else(|| snapshot.history.clone());
        prefix.push(ConversationMessage::user(MessageId::new(), prompt));
        request.fork_context_messages = Some(prefix);
        request.query_source_label = Some(FILE_EXTRACTION_QUERY_SOURCE.to_owned());
        request.max_turns_override = Some(FILE_EXTRACTION_MAX_TURNS);
        request.run_in_background = false;
        request.observer = None;
        request.name = None;
        request.team_name = None;
        request.handback_opt_in = false;
        // Tool definitions/prefix remain those of the parent's normal fork;
        // the invoker enforces Native's narrow allow/deny boundary at call time.
        let invoker = Arc::new(
            MemoryAgentToolInvoker::new(
                snapshot
                    .wire
                    .as_ref()
                    .and_then(|wire| wire.tool_invoker.clone())
                    .unwrap_or_else(|| self.invoker.clone()),
                memory_dir,
            )
            .with_shell(shell),
        );
        {
            let mut state = self.lock();
            if state.generation != generation || state.active_run != Some(run) {
                invoker.set_active(false);
                return;
            }
            state.invoker = Some(invoker.clone());
        }
        let spawn = self.spawner.spawn(
            request.clone(),
            SubagentInheritance {
                tool_invoker: invoker.clone(),
                budget: self.budget.clone(),
            },
        );
        let outcome = if let Some(wire) = snapshot.wire.clone() {
            agent::memory_fork::with_wire_snapshot(wire, spawn).await
        } else {
            spawn.await
        };
        if let Some(owner) = agent::memory_fork::current_settlement() {
            owner.wait_for_children().await;
        }
        invoker.set_active(false);
        let writes = invoker.completed_write_paths();
        let successful_pass = matches!(&outcome, Ok(SubagentResult::Completed { .. }));
        let mut state = self.lock();
        if state.generation != generation || state.active_run != Some(run) {
            return;
        }
        if successful_pass {
            state.cursor = tail;
        }
        if !writes.is_empty() {
            state.updates.push(AutoMemoryUpdate {
                summary: extraction_write_summary(&writes),
                paths: writes,
            });
        }
        if let Err(error) = outcome {
            tracing::warn!(%error, "background memory extraction failed");
        }
    }

    /// Consume successful-write notices once. This never fabricates a notice
    /// from the final assistant text or a scan of filesystem modification times.
    #[must_use]
    pub fn drain_updates(&self) -> Vec<AutoMemoryUpdate> {
        std::mem::take(&mut self.lock().updates)
    }

    /// Revoke pending and future tool calls before requesting worker teardown.
    pub async fn cancel(&self) {
        self.invalidate(false);
        self.wait_for_retiring().await;
    }

    fn invalidate(&self, reset: bool) {
        self.invalidate_root(reset, None);
    }

    fn invalidate_root(&self, reset: bool, root: Option<PathBuf>) {
        let mut state = self.lock();
        if let Some(root) = root {
            let mut active_root = self
                .memory_dir
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *active_root == root {
                return;
            }
            *active_root = root;
        }
        state.generation = state.generation.wrapping_add(1);
        state.active_run = None;
        state.pending = None;
        if reset {
            state.cursor = None;
            state.turns_since_pass = 0;
            state.updates.clear();
        }
        if let Some(invoker) = state.invoker.take() {
            invoker.set_active(false);
        }
        let worker = state.worker.take();
        if let Some(settlement) = state.settlement.take() {
            let runtime = self.runtime.clone();
            let (settled, done) = tokio::sync::watch::channel(false);
            state.retiring.push(done);
            // Publish the receipt and its independent owner without an await.
            // Dropping a reset waiter cannot discard cancellation or allow a
            // new pass to overlap the old pool child and its in-flight tools.
            tokio::spawn(async move {
                if let Some(worker) = worker {
                    runtime.cancel(&worker).await.expect("cancel memory worker");
                }
                settlement.wait().await;
                settled.send_replace(true);
            });
        }
    }

    async fn wait_for_retiring(&self) {
        let receipts = {
            let mut state = self.lock();
            state.retiring.retain(|done| !*done.borrow());
            state.retiring.clone()
        };
        Self::join_retiring(receipts).await;
    }

    async fn join_retiring(receipts: Vec<tokio::sync::watch::Receiver<bool>>) {
        for mut done in receipts {
            while !*done.borrow_and_update() {
                done.changed()
                    .await
                    .expect("memory teardown owner ended before settlement");
            }
        }
    }

    /// Clear extraction coverage and notices at a conversation boundary.
    pub async fn reset(&self) {
        self.invalidate(true);
        self.wait_for_retiring().await;
    }

    /// Follow an admitted live directory setting without letting a previous
    /// snapshot write under its new identity. Existing memory files stay put.
    pub async fn retarget(&self, memory_dir: PathBuf) {
        self.invalidate_root(true, Some(memory_dir));
        self.wait_for_retiring().await;
    }
}

struct ActiveWorker {
    handle: Weak<AutoMemoryHandle>,
    generation: u64,
    run: u64,
    _lease: MemoryForkLease,
}
impl Drop for ActiveWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.upgrade() {
            let mut state = handle.lock();
            if state.generation == self.generation && state.active_run == Some(self.run) {
                state.active_run = None;
                state.worker = None;
                state.pending = None;
                if let Some(invoker) = state.invoker.take() {
                    invoker.set_active(false);
                }
            }
        }
    }
}

async fn existing_memory_files(root: &Path) -> String {
    let Ok(root) = std::fs::canonicalize(root) else {
        return String::new();
    };
    let Ok(identity) = lingxi_core::host::rooted_fs::root_identity(&root) else {
        return String::new();
    };
    // Blocking filesystem work retains its own lease: aborting the async
    // wrapper neither interrupts this job nor establishes its completion.
    lingxi_core::host::memory_tool_scope::run_memory_blocking(move || {
        scan_memory_inventory(&root, &identity)
    })
    .await
    .unwrap_or_default()
}

fn read_inventory_file(
    root: &Path,
    relative: &Path,
    identity: &lingxi_core::host::rooted_fs::RootIdentity,
) -> Option<(std::time::SystemTime, FileInventoryMetadata)> {
    let file =
        lingxi_core::host::rooted_fs::open_read_file_pinned(root, relative, Some(identity)).ok()?;
    #[cfg(unix)]
    let links = {
        use std::os::unix::fs::MetadataExt;
        file.metadata().ok()?.nlink()
    };
    #[cfg(windows)]
    let links = u64::from(lingxi_core::host::rooted_fs::file_link_count(&file).ok()?);
    if links != 1 {
        return None;
    }
    let modified = file.metadata().ok()?.modified().ok()?;
    let mut bytes = Vec::new();
    file.take(FILE_INVENTORY_HEAD_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    if lingxi_core::host::rooted_fs::root_identity(root).ok()? != *identity {
        return None;
    }
    let content = String::from_utf8_lossy(&bytes);
    let head = content
        .lines()
        .take(FILE_INVENTORY_HEAD_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    Some((modified, parse_file_inventory_head(&head)))
}

fn scan_memory_inventory(
    root: &Path,
    identity: &lingxi_core::host::rooted_fs::RootIdentity,
) -> String {
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    // Like the native scanner, inspect candidates before selecting the newest
    // 200; an early traversal-order cap would silently omit newer memories.
    while let Some(directory) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if resolve_memory_markdown_path(
                    root,
                    path.join("probe.md").to_str().unwrap_or_default(),
                    None,
                )
                .is_some()
                {
                    directories.push(path);
                }
                continue;
            }
            if !kind.is_file() || entry.file_name() == "MEMORY.md" {
                continue;
            }
            if resolve_memory_markdown_path(root, path.to_str().unwrap_or_default(), None).is_none()
            {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Some((modified, frontmatter)) = read_inventory_file(root, relative, identity)
            else {
                continue;
            };
            let filename = relative.to_string_lossy().replace('\\', "/");
            files.push((modified, filename, frontmatter));
        }
    }
    files.sort_by(|left, right| right.0.cmp(&left.0));
    files
        .into_iter()
        .take(200)
        .map(|(mtime, filename, metadata)| {
            let millis = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_millis())
                .unwrap_or_default();
            let date = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                i64::try_from(millis).unwrap_or(i64::MAX),
            )
            .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default();
            render_existing_memory_file(
                &filename,
                &date,
                metadata.memory_type.as_deref(),
                metadata.description.as_deref(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolExecutionPolicy, ToolInvokerError,
    };
    use lingxi_core::host::{BudgetError, RuntimeError, SubagentSpawnError};
    use lingxi_core::types::{utf16_json::Utf16JsonProjection, AgentId};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    type Work = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
    #[derive(Default)]
    struct ManualRuntime {
        next: AtomicU64,
        work: Mutex<BTreeMap<u64, Work>>,
    }
    #[async_trait]
    impl RuntimeSpawner for ManualRuntime {
        async fn spawn(&self, _: &str, future: Work) -> Result<BackgroundTaskHandle, RuntimeError> {
            let id = self.next.fetch_add(1, Ordering::SeqCst);
            self.work.lock().unwrap().insert(id, future);
            Ok(BackgroundTaskHandle {
                task_name: FILE_EXTRACTION_QUERY_SOURCE.into(),
                task_id: id,
            })
        }
        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            // Drop the future after releasing this runtime's lock: its guard
            // legitimately acquires the memory handle's independent lock.
            let work = self.work.lock().unwrap().remove(&handle.task_id);
            drop(work);
            Ok(())
        }
        async fn sleep(&self, _: Duration) {
            unreachable!()
        }
    }
    impl ManualRuntime {
        fn take(&self) -> Work {
            self.work
                .lock()
                .unwrap()
                .pop_first()
                .expect("scheduled worker")
                .1
        }
    }

    struct Budget;
    #[async_trait]
    impl BudgetEnforcerHandle for Budget {
        async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    struct FileInvoker;
    #[async_trait]
    impl ToolInvoker for FileInvoker {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn invoke(
            &self,
            name: &str,
            input: Utf16JsonProjection,
            context: SubagentInvocationContext,
        ) -> Result<Utf16JsonProjection, ToolInvokerError> {
            assert_eq!(name, "Write");
            assert!(context.is_non_interactive_session);
            assert!(!context.can_show_permission_prompts);
            assert!(
                lingxi_core::host::memory_tool_scope::memory_tool_scope_directory(name, &input)
                    .is_some()
            );
            std::fs::write(
                input.string_at("/file_path").unwrap(),
                input.string_at("/content").unwrap(),
            )
            .unwrap();
            Ok(Utf16JsonProjection::parse(r#"{"written":true}"#).unwrap())
        }
    }
    fn invocation_context() -> SubagentInvocationContext {
        SubagentInvocationContext {
            available_tool_names: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
            origin_session_id: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: true,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            current_history: Vec::new(),
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }
    fn completed() -> SubagentResult {
        SubagentResult::Completed {
            handback: None,
            agent_id: AgentId::new(),
            content: "saved imaginary notes".into(),
            usage: Default::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            total_tokens: 0,
            assistant_message_count: 1,
            response_char_count: 1,
            last_request_id: None,
            cumulative_usage: Default::default(),
            usage_complete: true,
        }
    }

    struct RecordingSpawner {
        requests: Mutex<Vec<SubagentSpawnRequest>>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
        block_first: bool,
        write_path: Mutex<Option<PathBuf>>,
        fail_after_write: bool,
    }
    impl RecordingSpawner {
        fn new(block_first: bool, write_path: Option<PathBuf>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
                block_first,
                write_path: Mutex::new(write_path),
                fail_after_write: false,
            }
        }
    }
    #[async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            let first = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request);
                requests.len() == 1
            };
            let write_path = self.write_path.lock().unwrap().clone();
            self.entered.notify_one();
            if self.block_first && first {
                self.release.acquire().await.unwrap().forget();
            }
            if let Some(path) = &write_path {
                let input = Utf16JsonProjection::from_serde_value(
                    &serde_json::json!({"file_path":path,"content":"Remember this preference"}),
                )
                .unwrap();
                inherit
                    .tool_invoker
                    .invoke("Write", input, invocation_context())
                    .await
                    .map_err(|error| SubagentSpawnError::Runtime(error.to_string()))?;
            }
            if self.fail_after_write {
                return Err(SubagentSpawnError::Runtime(
                    "provider failed after a completed write".into(),
                ));
            }
            Ok(completed())
        }
    }
    fn handle(
        root: &Path,
        spawner: Arc<RecordingSpawner>,
        runtime: Arc<ManualRuntime>,
    ) -> Arc<AutoMemoryHandle> {
        Arc::new(AutoMemoryHandle::new(
            root.to_path_buf(),
            spawner,
            Arc::new(FileInvoker),
            Arc::new(Budget),
            runtime,
        ))
    }
    fn snapshot(
        handle: &AutoMemoryHandle,
        history: Vec<ConversationMessage>,
    ) -> AutoMemorySnapshot {
        AutoMemorySnapshot {
            generation: handle.generation(),
            wire: None,
            request: SubagentSpawnRequest {
                fork_context_messages: Some(history.clone()),
                parent_model_override: Some("provider-native-model".into()),
                parent_model_profile_override: Some("selected-provider".into()),
                fork_parent_system_prompt: Some("parent prefix".into()),
                ..Default::default()
            },
            history,
        }
    }
    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.into())
    }

    #[tokio::test]
    async fn coalesces_to_latest_snapshot_preserves_provider_and_publishes_no_fictional_writes() {
        let directory = tempfile::tempdir().unwrap();
        let spawner = Arc::new(RecordingSpawner::new(true, None));
        let runtime = Arc::new(ManualRuntime::default());
        let handle = handle(directory.path(), spawner.clone(), runtime.clone());
        let first = vec![user("Remember these three words")];
        handle.submit(snapshot(&handle, first.clone())).await;
        let running = tokio::spawn(runtime.take());
        spawner.entered.notified().await;
        let mut second = first.clone();
        second.push(user("Second preference to remember"));
        handle.submit(snapshot(&handle, second.clone())).await;
        let mut latest = second;
        latest.push(user("Third preference to remember"));
        handle.submit(snapshot(&handle, latest.clone())).await;
        spawner.release.add_permits(1);
        running.await.unwrap();
        let requests = spawner.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].parent_model_override.as_deref(),
            Some("provider-native-model")
        );
        assert_eq!(
            requests[1].parent_model_profile_override.as_deref(),
            Some("selected-provider")
        );
        assert_eq!(
            requests[1]
                .fork_parent_system_prompt
                .as_ref()
                .and_then(|prompt| prompt.string_at("")),
            Some("parent prefix")
        );
        assert_eq!(
            requests[1].fork_context_messages.as_ref().unwrap().len(),
            latest.len() + 1
        );
        assert!(!requests[1]
            .fork_context_messages
            .as_ref()
            .unwrap()
            .last()
            .unwrap()
            .is_meta());
        assert!(requests[1]
            .prompt
            .string_at("")
            .unwrap()
            .contains("most recent ~2 messages"));
        assert_eq!(requests[1].max_turns_override, Some(5));
        assert_eq!(
            requests[1].query_source_label.as_deref(),
            Some("extract_memories")
        );
        assert!(handle.drain_updates().is_empty());
    }

    #[tokio::test]
    async fn reports_exact_successful_paths_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("feedback.md");
        let spawner = Arc::new(RecordingSpawner::new(false, Some(path.clone())));
        let runtime = Arc::new(ManualRuntime::default());
        let handle = handle(directory.path(), spawner, runtime.clone());
        handle
            .submit(snapshot(
                &handle,
                vec![user("Remember my testing preference")],
            ))
            .await;
        runtime.take().await;
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Remember this preference"
        );
        assert_eq!(
            handle.drain_updates(),
            vec![AutoMemoryUpdate {
                summary: "saved 1 memory".into(),
                paths: vec![path]
            }]
        );
        assert!(handle.drain_updates().is_empty());
    }

    #[tokio::test]
    async fn failure_after_a_successful_write_reports_the_path_and_keeps_cursor_for_retry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("partial.md");
        let mut spawner = RecordingSpawner::new(false, Some(path.clone()));
        spawner.fail_after_write = true;
        let runtime = Arc::new(ManualRuntime::default());
        let handle = handle(directory.path(), Arc::new(spawner), runtime.clone());
        handle
            .submit(snapshot(
                &handle,
                vec![user("Remember the proven preference")],
            ))
            .await;
        runtime.take().await;
        assert_eq!(
            handle.drain_updates(),
            vec![AutoMemoryUpdate {
                summary: "saved 1 memory".into(),
                paths: vec![path]
            }]
        );
        assert!(handle.lock().cursor.is_none());
    }

    #[tokio::test]
    async fn skips_no_prose_and_main_memory_writes_without_starting_a_model() {
        let directory = tempfile::tempdir().unwrap();
        let spawner = Arc::new(RecordingSpawner::new(false, None));
        let runtime = Arc::new(ManualRuntime::default());
        let handle = handle(directory.path(), spawner.clone(), runtime.clone());
        handle
            .submit(snapshot(&handle, vec![user("two words")]))
            .await;
        runtime.take().await;
        let mut history = vec![user("Remember my test preference")];
        history.push(ConversationMessage::Assistant {
            id: MessageId::new(), content: vec![lingxi_core::types::ContentBlock::ToolUse {
                id: lingxi_core::types::ToolUseId::new(), name: "Write".into(),
                input: Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path":directory.path().join("direct.md"),"content":"Already saved"})).unwrap(), provider_id: None,
            }], stop_reason: Some("tool_use".into()), resumable: None, per_turn_effort: None,
        });
        // Reset makes the new range start without the preceding cursor.
        handle.reset().await;
        handle.submit(snapshot(&handle, history)).await;
        runtime.take().await;
        assert!(spawner.requests.lock().unwrap().is_empty());
        assert!(handle.drain_updates().is_empty());
    }

    #[tokio::test]
    async fn reset_revokes_blocked_worker_and_rejects_prepared_old_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale.md");
        let spawner = Arc::new(RecordingSpawner::new(true, Some(path.clone())));
        let runtime = Arc::new(ManualRuntime::default());
        let handle = handle(directory.path(), spawner.clone(), runtime.clone());
        let stale = snapshot(&handle, vec![user("Remember this old session")]);
        handle.submit(stale.clone()).await;
        let running = tokio::spawn(runtime.take());
        spawner.entered.notified().await;
        let reset = handle.reset();
        tokio::pin!(reset);
        assert!(futures::poll!(&mut reset).is_pending());
        handle.submit(stale).await;
        spawner.release.add_permits(1);
        reset.await;
        running.await.unwrap();
        assert!(!path.exists());
        assert!(handle.drain_updates().is_empty());
        assert!(runtime.work.lock().unwrap().is_empty());
        assert!(handle.lock().cursor.is_none());
    }

    #[tokio::test]
    async fn retarget_revokes_old_snapshot_preserves_old_files_and_uses_new_directory() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old");
        let new = directory.path().join("new");
        std::fs::create_dir_all(&old).unwrap();
        let retained = old.join("existing.md");
        std::fs::write(&retained, "Keep the original notes.").unwrap();
        let old_write = old.join("stale.md");
        let new_write = new.join("new.md");
        let spawner = Arc::new(RecordingSpawner::new(true, Some(old_write.clone())));
        let runtime = Arc::new(ManualRuntime::default());
        let handle = handle(&old, spawner.clone(), runtime.clone());
        let stale = snapshot(&handle, vec![user("Remember this old preference")]);
        handle.submit(stale.clone()).await;
        let old_run = tokio::spawn(runtime.take());
        spawner.entered.notified().await;
        let retarget = handle.retarget(new.clone());
        tokio::pin!(retarget);
        assert!(futures::poll!(&mut retarget).is_pending());
        assert_eq!(handle.memory_dir(), new);
        handle.submit(stale).await;
        spawner.release.add_permits(1);
        retarget.await;
        old_run.await.unwrap();
        *spawner.write_path.lock().unwrap() = Some(new_write.clone());
        handle
            .submit(snapshot(&handle, vec![user("Remember the new preference")]))
            .await;
        runtime.take().await;
        assert!(!old_write.exists());
        assert_eq!(
            std::fs::read_to_string(&retained).unwrap(),
            "Keep the original notes."
        );
        assert!(new_write.exists());
        let updates = handle.drain_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].paths, vec![new_write.canonicalize().unwrap()]);
    }

    #[tokio::test]
    async fn memory_reset_retains_child_effect_settlement_after_its_waiter_is_cancelled() {
        struct AbortOnlyRuntime {
            next: AtomicU64,
            tasks: Mutex<BTreeMap<u64, tokio::task::JoinHandle<()>>>,
            cancelled: tokio::sync::Semaphore,
        }
        #[async_trait]
        impl RuntimeSpawner for AbortOnlyRuntime {
            async fn spawn(
                &self,
                name: &str,
                work: Work,
            ) -> Result<BackgroundTaskHandle, RuntimeError> {
                let id = self.next.fetch_add(1, Ordering::SeqCst);
                self.tasks.lock().unwrap().insert(id, tokio::spawn(work));
                Ok(BackgroundTaskHandle {
                    task_name: name.into(),
                    task_id: id,
                })
            }
            async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
                if let Some(task) = self.tasks.lock().unwrap().remove(&handle.task_id) {
                    task.abort();
                }
                self.cancelled.add_permits(1);
                Ok(())
            }
            async fn sleep(&self, _: Duration) {
                unreachable!()
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("admitted.md");
        let spawner = Arc::new(RecordingSpawner::new(true, None));
        let runtime = Arc::new(AbortOnlyRuntime {
            next: AtomicU64::new(0),
            tasks: Mutex::new(BTreeMap::new()),
            cancelled: tokio::sync::Semaphore::new(0),
        });
        let handle = Arc::new(AutoMemoryHandle::new(
            directory.path().to_owned(),
            spawner.clone(),
            Arc::new(FileInvoker),
            Arc::new(Budget),
            runtime.clone(),
        ));
        handle
            .submit(snapshot(&handle, vec![user("Remember the old preference")]))
            .await;
        spawner.entered.notified().await;
        let child = handle.lock().settlement.as_ref().unwrap().enter();
        let (finish_effect, wait_effect) = std::sync::mpsc::channel();
        let changed = path.clone();
        // Model an already-admitted synchronous effect independently retained
        // by the pool child. Revocation cannot interrupt its in-flight syscall.
        let effect = std::thread::spawn(move || {
            let _child = child;
            wait_effect.recv().unwrap();
            std::fs::write(changed, "completed before teardown returned").unwrap();
        });
        let reset_handle = handle.clone();
        let first = tokio::spawn(async move { reset_handle.reset().await });
        runtime.cancelled.acquire().await.unwrap().forget();
        assert!(!first.is_finished());
        first.abort();
        let _ = first.await;
        let retry = handle.reset();
        tokio::pin!(retry);
        assert!(futures::poll!(&mut retry).is_pending());
        let submit = handle.submit(snapshot(&handle, vec![user("Remember the new preference")]));
        tokio::pin!(submit);
        assert!(
            futures::poll!(&mut submit).is_pending(),
            "new admission must join the old effect"
        );
        assert!(!path.exists());
        finish_effect.send(()).unwrap();
        effect.join().unwrap();
        tokio::time::timeout(Duration::from_secs(2), retry)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), submit)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "completed before teardown returned"
        );
        assert!(handle.drain_updates().is_empty());
        handle.cancel().await;
    }

    #[cfg(unix)]
    #[test]
    fn memory_inventory_rejects_leaf_parent_and_root_replacement_after_validation() {
        for change in ["leaf", "parent", "root"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("memory");
            let parent = root.join("topics");
            let outside = directory.path().join("other-project");
            std::fs::create_dir_all(&parent).unwrap();
            std::fs::create_dir_all(outside.join("topics")).unwrap();
            let target = parent.join("candidate.md");
            std::fs::write(&target, "---\ndescription: admitted description\n---\n").unwrap();
            let external = outside.join("topics/candidate.md");
            std::fs::write(
                &external,
                "---\ndescription: EXTERNAL_DESCRIPTION\ntype: project\n---\n",
            )
            .unwrap();
            let root = root.canonicalize().unwrap();
            let identity = lingxi_core::host::rooted_fs::root_identity(&root).unwrap();
            let target = root.join("topics/candidate.md");
            assert!(resolve_memory_markdown_path(&root, target.to_str().unwrap(), None).is_some());
            match change {
                "leaf" => {
                    std::fs::remove_file(&target).unwrap();
                    std::os::unix::fs::symlink(&external, &target).unwrap();
                }
                "parent" => {
                    std::fs::rename(root.join("topics"), root.join("old-topics")).unwrap();
                    std::os::unix::fs::symlink(outside.join("topics"), root.join("topics"))
                        .unwrap();
                }
                "root" => {
                    std::fs::rename(&root, directory.path().join("old-memory")).unwrap();
                    std::os::unix::fs::symlink(&outside, &root).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                read_inventory_file(&root, Path::new("topics/candidate.md"), &identity).is_none(),
                "{change}"
            );
            assert!(
                !scan_memory_inventory(&root, &identity).contains("EXTERNAL_DESCRIPTION"),
                "{change}"
            );
        }
    }

    #[tokio::test]
    async fn inventory_uses_native_topic_shape_and_excludes_index_and_protected_subtrees() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("MEMORY.md"), "index").unwrap();
        std::fs::write(directory.path().join("feedback.md"), "---\nname: checks\ndescription: Prefer focused checks\nmetadata:\n  type: feedback\n---\nAvoid broad reruns.\n").unwrap();
        std::fs::create_dir(directory.path().join("agents")).unwrap();
        std::fs::write(directory.path().join("agents/private.md"), "private").unwrap();
        let inventory = existing_memory_files(directory.path()).await;
        assert!(
            inventory.starts_with("- [feedback] feedback.md ("),
            "{inventory}"
        );
        assert!(inventory.ends_with(": Prefer focused checks"));
        assert!(!inventory.contains("MEMORY.md"));
        assert!(!inventory.contains("private.md"));
    }

    struct PausingMemoryGate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    impl Default for PausingMemoryGate {
        fn default() -> Self {
            Self {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            }
        }
    }

    #[async_trait]
    impl lingxi_core::host::permission_gate::PermissionGate for PausingMemoryGate {
        async fn check(
            &self,
            name: &str,
            _: &Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            if name != "Read" {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
            lingxi_core::host::permission_gate::PermissionDecision::Allow
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_file_mutations_remain_bound_across_permission_waits() {
        for operation in ["Write", "Edit"] {
            for change in [
                "parent",
                "protected",
                "directory",
                "leaf",
                "root",
                "hard_link",
                "revoked",
            ] {
                let temp = tempfile::tempdir().unwrap();
                let workspace = temp.path().join("workspace");
                let root = temp.path().join("memory");
                let parent = root.join("topics");
                std::fs::create_dir_all(&workspace).unwrap();
                std::fs::create_dir_all(&parent).unwrap();
                let root = root.canonicalize().unwrap();
                let parent = root.join("topics");
                let path = parent.join("topic.md");
                let outside = workspace.join("topic.md");
                let protected = root.join("agents");
                std::fs::create_dir(&protected).unwrap();
                for file in [&path, &outside, &protected.join("topic.md")] {
                    std::fs::write(file, "original").unwrap();
                }
                let builtins = tool_api::test_support::ctx_for_file_tools(
                    tool_api::test_support::make_dummy_fs(),
                    Arc::new(telemetry::AnalyticsBus::new()),
                    vec![workspace.clone()],
                );
                let mut registry = tool_api::ToolRegistry::new();
                registry.register_builtin(Arc::new(tool_file::FileReadTool::new(builtins.clone())));
                registry
                    .register_builtin(Arc::new(tool_file::FileWriteTool::new(builtins.clone())));
                registry.register_builtin(Arc::new(tool_file::FileEditTool::new(builtins.clone())));
                let gate = Arc::new(PausingMemoryGate::default());
                let inner = Arc::new(
                    tool_api::RegistryToolInvoker::new(Arc::new(registry))
                        .with_gate(gate.clone())
                        .with_read_file_state(builtins.read_file_state.clone()),
                );
                let wrapper = Arc::new(MemoryAgentToolInvoker::new(inner, root.clone()));
                wrapper
                    .invoke_detailed(
                        "Read",
                        Utf16JsonProjection::from_serde_value(
                            &serde_json::json!({"file_path":path}),
                        )
                        .unwrap(),
                        invocation_context(),
                    )
                    .await
                    .unwrap();
                let input = if operation == "Write" {
                    serde_json::json!({"file_path":path,"content":"changed"})
                } else {
                    serde_json::json!({"file_path":path,"old_string":"original","new_string":"changed"})
                };
                let worker = wrapper.clone();
                let call = tokio::spawn(async move {
                    worker
                        .invoke_detailed(
                            operation,
                            Utf16JsonProjection::from_serde_value(&input).unwrap(),
                            invocation_context(),
                        )
                        .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), gate.entered.notified())
                    .await
                    .unwrap();
                match change {
                    "parent" | "protected" => {
                        std::fs::rename(&parent, root.join("old-topics")).unwrap();
                        std::os::unix::fs::symlink(
                            if change == "parent" {
                                &workspace
                            } else {
                                &protected
                            },
                            &parent,
                        )
                        .unwrap();
                    }
                    "root" => {
                        std::fs::rename(&root, temp.path().join("old-memory")).unwrap();
                        std::fs::create_dir_all(&parent).unwrap();
                        std::fs::write(&path, "replacement").unwrap();
                    }
                    "directory" => {
                        std::fs::rename(&parent, root.join("old-topics")).unwrap();
                        std::fs::rename(&protected, &parent).unwrap();
                    }
                    "leaf" => {
                        std::fs::rename(&path, root.join("old-topic.md")).unwrap();
                        std::fs::rename(protected.join("topic.md"), &path).unwrap();
                    }
                    "hard_link" => {
                        std::fs::remove_file(&path).unwrap();
                        std::fs::hard_link(&outside, &path).unwrap();
                    }
                    "revoked" => wrapper.set_active(false),
                    _ => unreachable!(),
                }
                gate.release.add_permits(1);
                let result = call.await.unwrap();
                assert!(
                    result.is_err() || result.unwrap().is_error,
                    "{operation}: {change}"
                );
                assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
                if change == "directory" {
                    std::fs::rename(&parent, &protected).unwrap();
                    std::fs::rename(root.join("old-topics"), &parent).unwrap();
                }
                if change == "leaf" {
                    std::fs::rename(&path, protected.join("topic.md")).unwrap();
                }
                let protected_after = if change == "root" {
                    temp.path().join("old-memory/agents/topic.md")
                } else {
                    protected.join("topic.md")
                };
                assert_eq!(
                    std::fs::read_to_string(protected_after).unwrap(),
                    "original"
                );
                if change == "root" {
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
                }
                assert!(wrapper.completed_write_paths().is_empty());
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_new_writes_cannot_adopt_changed_or_previously_absent_parents() {
        for existing_parent in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let parent = root.join("topics");
            let protected = root.join("agents");
            std::fs::create_dir(&protected).unwrap();
            std::fs::write(protected.join("preserve.md"), "original").unwrap();
            if existing_parent {
                std::fs::create_dir(&parent).unwrap();
            }
            let builtins = tool_api::test_support::ctx_for_file_tools(
                tool_api::test_support::make_dummy_fs(),
                Arc::new(telemetry::AnalyticsBus::new()),
                vec![root.clone()],
            );
            let mut registry = tool_api::ToolRegistry::new();
            registry.register_builtin(Arc::new(tool_file::FileWriteTool::new(builtins)));
            let gate = Arc::new(PausingMemoryGate::default());
            let wrapper = Arc::new(MemoryAgentToolInvoker::new(
                Arc::new(
                    tool_api::RegistryToolInvoker::new(Arc::new(registry)).with_gate(gate.clone()),
                ),
                root.clone(),
            ));
            let path = parent.join("topic.md");
            let worker = wrapper.clone();
            let target = path.clone();
            let call = tokio::spawn(async move {
                worker
                    .invoke_detailed(
                        "Write",
                        Utf16JsonProjection::from_serde_value(
                            &serde_json::json!({"file_path":target,"content":"new"}),
                        )
                        .unwrap(),
                        invocation_context(),
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), gate.entered.notified())
                .await
                .unwrap();
            if existing_parent {
                std::fs::rename(&parent, root.join("old-topics")).unwrap();
            }
            std::fs::rename(&protected, &parent).unwrap();
            gate.release.add_permits(1);
            let result = call.await.unwrap();
            assert!(result.is_err() || result.unwrap().is_error);
            assert!(!path.exists());
            assert_eq!(
                std::fs::read_to_string(parent.join("preserve.md")).unwrap(),
                "original"
            );
            assert!(wrapper.completed_write_paths().is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_worker_cannot_admit_a_substituted_root_on_a_later_call() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("memory");
        let outside = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = root.canonicalize().unwrap();
        let builtins = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![outside.clone()],
        );
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(tool_file::FileWriteTool::new(builtins)));
        let gate = Arc::new(PausingMemoryGate::default());
        gate.release.add_permits(1);
        let wrapper = MemoryAgentToolInvoker::new(
            Arc::new(tool_api::RegistryToolInvoker::new(Arc::new(registry)).with_gate(gate)),
            root.clone(),
        );
        std::fs::rename(&root, temp.path().join("old-memory")).unwrap();
        std::os::unix::fs::symlink(&outside, &root).unwrap();
        let result = wrapper.invoke_detailed("Write", Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"file_path":root.join("new.md"),"content":"must not write"})).unwrap(), invocation_context()).await;
        assert!(result.is_err());
        assert!(!outside.join("new.md").exists());
        assert!(wrapper.completed_write_paths().is_empty());
    }

    struct PruneBackendProbe(&'static str);
    #[async_trait]
    impl tool_api::Tool for PruneBackendProbe {
        fn name(&self) -> &str {
            self.0
        }
        fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
            tool_api::tool_trait::ToolInputRepairMetadata::Known {
                schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
                schema_provenance:
                    tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
                exempt_top_level_keys: None,
            }
        }
        fn input_schema(&self) -> &Utf16JsonProjection {
            static SCHEMA: once_cell::sync::Lazy<Utf16JsonProjection> = once_cell::sync::Lazy::new(
                || {
                    tool_api::tool_trait::generated_builtin_schema(
                        &serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":true}),
                    )
                },
            );
            &SCHEMA
        }
        fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
            true
        }
        fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
            true
        }
        fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
            false
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        async fn description(
            &self,
            _: &Utf16JsonProjection,
            _: &tool_api::tool_trait::DescriptionOptions,
        ) -> String {
            "prune backend probe".into()
        }
        async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> Utf16JsonProjection {
            "".into()
        }
        async fn validate_input(
            &self,
            _: &Utf16JsonProjection,
            _: &tool_api::context::ToolUseContext,
        ) -> Result<(), tool_api::ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &Utf16JsonProjection,
            _: &tool_api::context::ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "probe".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: Default::default(),
            }
        }
        async fn call(
            &self,
            _: Utf16JsonProjection,
            _: tool_api::context::ToolUseContext,
            _: tool_api::progress::ToolProgressSender,
        ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
            panic!("memory pruning must not execute a shell backend")
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_pruning_uses_the_preadmitted_root_for_every_shell_identity() {
        for shell in [
            MemoryAgentShell::Bash,
            MemoryAgentShell::PowerShell,
            MemoryAgentShell::Mobile,
        ] {
            for change in ["control", "parent", "directory", "root", "cancelled"] {
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path().join("memory");
                let parent = root.join("topics");
                let outside = temp.path().join("workspace");
                std::fs::create_dir_all(&parent).unwrap();
                std::fs::create_dir(&outside).unwrap();
                let root = root.canonicalize().unwrap();
                let parent = root.join("topics");
                let protected = root.join("agents");
                std::fs::create_dir(&protected).unwrap();
                let paths = [parent.join("note one.md"), parent.join("note two.md")];
                for path in &paths {
                    std::fs::write(path, "memory").unwrap();
                    std::fs::write(outside.join(path.file_name().unwrap()), "preserve").unwrap();
                    std::fs::write(protected.join(path.file_name().unwrap()), "protected").unwrap();
                }
                let mut registry = tool_api::ToolRegistry::new();
                registry.register_builtin(Arc::new(PruneBackendProbe(shell.name())));
                let gate = Arc::new(PausingMemoryGate::default());
                let wrapper = Arc::new(
                    MemoryAgentToolInvoker::new(
                        Arc::new(
                            tool_api::RegistryToolInvoker::new(Arc::new(registry))
                                .with_gate(gate.clone()),
                        ),
                        root.clone(),
                    )
                    .with_shell(shell),
                );
                let command = if shell.is_posix() {
                    format!(
                        "rm -f '{}' '{}' '{}' '{}'",
                        paths[0].display(),
                        paths[1].display(),
                        paths[0].display(),
                        parent.join("missing.md").display()
                    )
                } else {
                    format!(
                        "Remove-Item '{}' '{}'",
                        paths[0].display(),
                        paths[1].display()
                    )
                };
                let worker = wrapper.clone();
                let context = invocation_context();
                if change == "cancelled" {
                    context.cancellation_token.cancel();
                }
                let call = tokio::spawn(async move {
                    worker
                        .invoke_detailed(
                            shell.name(),
                            Utf16JsonProjection::from_serde_value(
                                &serde_json::json!({"command":command}),
                            )
                            .unwrap(),
                            context,
                        )
                        .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), gate.entered.notified())
                    .await
                    .unwrap();
                match change {
                    "parent" => {
                        std::fs::rename(&parent, root.join("old-topics")).unwrap();
                        std::os::unix::fs::symlink(&outside, &parent).unwrap();
                    }
                    "root" => {
                        std::fs::rename(&root, temp.path().join("old-memory")).unwrap();
                        std::fs::create_dir_all(&parent).unwrap();
                        for path in &paths {
                            std::fs::write(path, "replacement").unwrap();
                        }
                    }
                    "directory" => {
                        std::fs::rename(&parent, root.join("old-topics")).unwrap();
                        std::fs::rename(&protected, &parent).unwrap();
                    }
                    _ => {}
                }
                gate.release.add_permits(1);
                let result = call.await.unwrap();
                if change == "control" {
                    let result = result.unwrap();
                    assert!(!result.is_error);
                    assert!(!paths[0].exists() && !paths[1].exists());
                    if shell == MemoryAgentShell::Bash {
                        assert_eq!(
                            result.data.get_property("interrupted").unwrap().as_bool(),
                            Some(false)
                        );
                        assert!(!result.data.contains_property("exit_code"));
                    } else {
                        assert_eq!(result.data.number_at("/exit_code"), Some(0.0));
                    }
                    assert!(!wrapper.completed_touched_paths().is_empty());
                } else {
                    assert!(
                        result.is_err() || result.unwrap().is_error,
                        "{}: {change}",
                        shell.name()
                    );
                    assert!(wrapper.completed_touched_paths().is_empty());
                }
                for path in &paths {
                    assert_eq!(
                        std::fs::read_to_string(outside.join(path.file_name().unwrap())).unwrap(),
                        "preserve"
                    );
                }
                if change == "directory" {
                    for path in &paths {
                        assert_eq!(std::fs::read_to_string(path).unwrap(), "protected");
                    }
                }
                if change == "cancelled" {
                    for path in &paths {
                        assert_eq!(std::fs::read_to_string(path).unwrap(), "memory");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn mobile_memory_prunes_host_files_through_the_real_shell_and_permission_gate() {
        use permission::{
            PermissionBehavior, PermissionMode, PermissionPolicy, PermissionRule,
            PermissionRuleSource, PermissionRuleValue,
        };
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let root = temp.path().join("host-private-memory");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("obsolete.md");
        let outside = temp.path().join("outside.md");
        std::fs::write(&outside, "preserve outside").unwrap();
        for behavior in [
            Some(PermissionBehavior::Deny),
            Some(PermissionBehavior::Ask),
            None,
        ] {
            std::fs::write(&path, "old memory").unwrap();
            let mut builtins = tool_api::test_support::ctx_for_file_tools(
                tool_api::test_support::make_dummy_fs(),
                Arc::new(telemetry::AnalyticsBus::new()),
                vec![workspace.clone()],
            );
            // There is no host-memory mount in this guest. A successful prune
            // must use the exact host authority, not run a host path in Linux.
            builtins.mobile_shell = Some(tool_api::MobileShellToolCtx::mobile_linux_guest(
                true,
                vec!["sh".into()],
                None,
            ));
            let mut registry = tool_api::ToolRegistry::new();
            tool_shell_mobile::register_all(&mut registry, builtins);
            assert!(registry.find_by_name("Shell").is_some());
            assert!(registry.find_by_name("Bash").is_none());
            let rules = behavior.into_iter().map(|behavior| PermissionRule {
                value: PermissionRuleValue::from_rule_string("Shell"),
                source: PermissionRuleSource::Command,
                behavior,
            });
            let policy = Arc::new(PermissionPolicy::from_rules_confined(
                PermissionMode::Default,
                rules,
                false,
            ));
            let gate = Arc::new(permission::PolicyPermissionGate::new(
                policy,
                Arc::new(permission::DenyOnAskGate),
            ));
            let inner =
                Arc::new(tool_api::RegistryToolInvoker::new(Arc::new(registry)).with_gate(gate));
            let wrapper = MemoryAgentToolInvoker::new(inner, root.clone())
                .with_shell(MemoryAgentShell::Mobile);
            let call = |path: &Path| {
                Utf16JsonProjection::from_serde_value(&serde_json::json!({
                    "command": format!("rm -f '{}'", path.display())
                }))
                .unwrap()
            };
            let result = wrapper
                .invoke_detailed("Shell", call(&path), invocation_context())
                .await;
            assert_eq!(result.is_ok(), behavior.is_none());
            assert_eq!(path.exists(), behavior.is_some());
            assert!(wrapper
                .invoke_detailed("Shell", call(&outside), invocation_context())
                .await
                .is_err());
            assert_eq!(
                std::fs::read_to_string(&outside).unwrap(),
                "preserve outside"
            );
            assert_eq!(
                wrapper.completed_touched_paths(),
                if behavior.is_none() {
                    vec![path.clone()]
                } else {
                    vec![]
                }
            );
            assert!(wrapper.completed_write_paths().is_empty());
            wrapper.set_active(false);
            assert!(wrapper
                .invoke_detailed("Shell", call(&path), invocation_context())
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn scoped_memory_allowance_executes_the_real_file_write_schema_and_tool() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let root = directory.path().join("memory");
        let transcripts = directory.path().join("transcripts");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&transcripts).unwrap();
        let root = root.canonicalize().unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let transcripts = transcripts.canonicalize().unwrap();
        let transcript = transcripts.join("session.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        let path = root.join("feedback.md");
        let builtins = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![workspace.clone()],
        );
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(tool_file::FileWriteTool::new(builtins.clone())));
        registry.register_builtin(Arc::new(tool_file::FileReadTool::new(builtins.clone())));
        registry.register_builtin(Arc::new(tool_file::FileEditTool::new(builtins.clone())));
        let policy = Arc::new(permission::PermissionPolicy::new(
            permission::PermissionMode::Default,
        ));
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            policy,
            Arc::new(permission::DenyOnAskGate),
        ));
        let inner = Arc::new(
            tool_api::RegistryToolInvoker::new(Arc::new(registry))
                .with_gate(gate)
                .with_read_file_state(builtins.read_file_state.clone()),
        );
        let input = Utf16JsonProjection::from_serde_value(&serde_json::json!({
            "file_path":path,"content":"---\nname: tests\ndescription: Preferred verification\nmetadata:\n  type: feedback\n---\nUse focused checks.\n"
        })).unwrap();
        let wrapper = MemoryAgentToolInvoker::new(inner.clone(), root.clone())
            .with_read_directory(transcripts.clone());
        let result = wrapper
            .invoke_detailed("Write", input, invocation_context())
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .ends_with("Use focused checks.\n"));
        let read =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path":path})).unwrap();
        assert!(
            !wrapper
                .invoke_detailed("Read", read, invocation_context())
                .await
                .unwrap()
                .is_error
        );
        let edit = Utf16JsonProjection::from_serde_value(&serde_json::json!({
            "file_path":path,"old_string":"Use focused checks.","new_string":"Use relevant checks."
        }))
        .unwrap();
        assert!(
            !wrapper
                .invoke_detailed("Edit", edit, invocation_context())
                .await
                .unwrap()
                .is_error
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .ends_with("Use relevant checks.\n"));
        let read_transcript =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path":transcript}))
                .unwrap();
        assert!(
            !wrapper
                .invoke_detailed("Read", read_transcript, invocation_context())
                .await
                .unwrap()
                .is_error
        );
        let forbidden = transcripts.join("forbidden.md");
        let input = Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"file_path":forbidden,"content":"not a memory"}),
        )
        .unwrap();
        assert!(wrapper
            .invoke_detailed("Write", input, invocation_context())
            .await
            .is_err());
        assert!(!forbidden.exists());
        assert_eq!(builtins.trusted_dirs(), vec![workspace.clone()]);
        assert_eq!(wrapper.completed_write_paths(), vec![path.clone()]);

        let unscoped = root.join("unscoped.md");
        let input = Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"file_path":unscoped,"content":"no grant"}),
        )
        .unwrap();
        assert!(inner
            .invoke_detailed("Write", input, invocation_context())
            .await
            .is_err());
        assert!(!unscoped.exists());

        let replacement = directory.path().join("replacement");
        std::fs::create_dir_all(&replacement).unwrap();
        let replacement = replacement.canonicalize().unwrap();
        wrapper.set_active(false);
        let next = MemoryAgentToolInvoker::new(inner, replacement.clone());
        let next_path = replacement.join("feedback.md");
        let input = Utf16JsonProjection::from_serde_value(&serde_json::json!({
            "file_path":next_path,"content":"The replacement store owns this memory.\n"
        }))
        .unwrap();
        assert!(
            !next
                .invoke_detailed("Write", input, invocation_context())
                .await
                .unwrap()
                .is_error
        );
        let old_read =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path":path})).unwrap();
        assert!(next
            .invoke_detailed("Read", old_read, invocation_context())
            .await
            .is_err());
        assert!(path.exists());
        assert!(next_path.exists());
        assert_eq!(builtins.trusted_dirs(), vec![workspace]);
    }

    #[tokio::test]
    async fn memory_forks_do_not_hide_private_reads_or_authorize_stale_foreground_writes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        let root = workspace.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        let private_path = root.join("private.md");
        let queued_path = root.join("queued.md");
        let path = root.join("topic.md");
        std::fs::write(&private_path, "Only the first child read these bytes.").unwrap();
        std::fs::write(&queued_path, "Read after the fork history was frozen.").unwrap();
        std::fs::write(&path, "Version A.").unwrap();
        let builtins = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![workspace],
        );
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(tool_file::FileReadTool::new(builtins.clone())));
        registry.register_builtin(Arc::new(tool_file::FileWriteTool::new(builtins.clone())));
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(
                permission::PermissionPolicy::new(permission::PermissionMode::AcceptEdits)
                    .with_roots(permission::FsRoots {
                        cwd: builtins.cwd(),
                        home: None,
                        lingxi_home: builtins.cwd().join(branding::DOT_DIR),
                    }),
            ),
            Arc::new(permission::DenyOnAskGate),
        ));
        let inner = Arc::new(
            tool_api::RegistryToolInvoker::new(Arc::new(registry))
                .with_gate(gate)
                .with_read_file_state(builtins.read_file_state.clone()),
        );
        let read = |path: &Path| {
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"file_path":path})).unwrap()
        };
        let write = |content: &str| {
            Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"file_path":path,"content":content}),
            )
            .unwrap()
        };
        let invocation_context = || {
            let mut context = invocation_context();
            context.cwd = Some(builtins.cwd());
            context
        };
        let frozen = inner.fork_for_memory(true).unwrap();
        // A subsequent foreground turn reads a file absent from the frozen
        // prefix. Both workers sharing that prefix still need its full bytes.
        inner
            .invoke_detailed("Read", read(&queued_path), invocation_context())
            .await
            .unwrap();
        for _ in 0..2 {
            let worker = MemoryAgentToolInvoker::new(frozen.clone(), root.clone());
            let result = worker
                .invoke_detailed("Read", read(&queued_path), invocation_context())
                .await
                .unwrap();
            assert_ne!(result.data.string_at("/type"), Some("file_unchanged"));
        }
        let first = MemoryAgentToolInvoker::new(inner.clone(), root.clone());
        let result = first
            .invoke_detailed("Read", read(&private_path), invocation_context())
            .await
            .unwrap();
        assert_ne!(result.data.string_at("/type"), Some("file_unchanged"));
        assert!(tool_api::read_file_state::get(&builtins.read_file_state, &private_path).is_none());
        let second = MemoryAgentToolInvoker::new(inner.clone(), root.clone());
        let result = second
            .invoke_detailed("Read", read(&private_path), invocation_context())
            .await
            .unwrap();
        assert_ne!(result.data.string_at("/type"), Some("file_unchanged"));
        let result = inner
            .invoke_detailed("Read", read(&private_path), invocation_context())
            .await
            .unwrap();
        assert_ne!(result.data.string_at("/type"), Some("file_unchanged"));

        inner
            .invoke_detailed("Read", read(&path), invocation_context())
            .await
            .unwrap();
        let writer = MemoryAgentToolInvoker::new(inner.clone(), root);
        writer
            .invoke_detailed("Read", read(&path), invocation_context())
            .await
            .unwrap();
        writer
            .invoke_detailed(
                "Write",
                write("Version B from the background."),
                invocation_context(),
            )
            .await
            .unwrap();
        assert_eq!(
            tool_api::read_file_state::get(&builtins.read_file_state, &path)
                .unwrap()
                .content,
            "Version A."
        );
        let stale = inner
            .invoke_detailed(
                "Write",
                write("Stale overwrite based on A."),
                invocation_context(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            stale.model_facing_message().string_at(""),
            Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Version B from the background."
        );
        let result = inner
            .invoke_detailed("Read", read(&path), invocation_context())
            .await
            .unwrap();
        assert_ne!(result.data.string_at("/type"), Some("file_unchanged"));
        inner
            .invoke_detailed(
                "Write",
                write("Version C after a foreground read."),
                invocation_context(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "Version C after a foreground read."
        );
    }
}
