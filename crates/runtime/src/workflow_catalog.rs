//! Synchronize validated Workflow descriptors into the live slash registry.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use command_api::CommandRegistry;

use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tool_workflow::WorkflowTool;

/// Serialized owner for the current Workflow descriptor and slash-command
/// snapshots. Registry consumers call the provider before reading; the current
/// working directory is the Native cache key.
pub(crate) struct WorkflowCommandSnapshot {
    registry: Arc<RwLock<CommandRegistry>>,
    workflow_tool: Arc<RwLock<Option<Arc<WorkflowTool>>>>,
    current_cwd: Arc<Mutex<PathBuf>>,
    refresh_lock: AsyncMutex<()>,
    state: Mutex<WorkflowCommandSnapshotState>,
}

#[derive(Default)]
struct WorkflowCommandSnapshotState {
    published_cwd: Option<PathBuf>,
    generation: u64,
}

impl WorkflowCommandSnapshot {
    pub(crate) fn new(
        registry: Arc<RwLock<CommandRegistry>>,
        workflow_tool: Arc<RwLock<Option<Arc<WorkflowTool>>>>,
        current_cwd: Arc<Mutex<PathBuf>>,
    ) -> Self {
        Self {
            registry,
            workflow_tool,
            current_cwd,
            refresh_lock: AsyncMutex::new(()),
            state: Mutex::new(WorkflowCommandSnapshotState::default()),
        }
    }

    pub(crate) fn provider(self: &Arc<Self>) -> command_api::CommandSnapshotProvider {
        let snapshot = self.clone();
        Arc::new(move || {
            let snapshot = snapshot.clone();
            Box::pin(async move {
                snapshot.refresh_if_cwd_changed().await;
            })
        })
    }

    /// Force-refresh for startup and explicit reloads, even when cwd is
    /// unchanged. The same serialized path is used by lazy cwd-key refreshes.
    pub(crate) async fn refresh(&self) {
        self.refresh_inner(true).await;
    }

    /// Refresh only when the live cwd differs from the cwd whose workflow
    /// rows are currently published.
    pub(crate) async fn refresh_if_cwd_changed(&self) {
        self.refresh_inner(false).await;
    }

    async fn refresh_inner(&self, force: bool) {
        let _refresh = self.refresh_lock.lock().await;
        loop {
            let Some(workflow_tool) = self.workflow_tool.read().await.clone() else {
                return;
            };
            let cwd = self.current_cwd();
            let generation = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !force && state.published_cwd.as_ref() == Some(&cwd) {
                    return;
                }
                state.generation
            };

            let descriptors = workflow_tool.refresh_workflow_catalog(&cwd);
            let mut registry = self.registry.write().await;
            let current_cwd = self.current_cwd();
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if generation != state.generation || cwd != current_cwd {
                drop(state);
                drop(registry);
                continue;
            }

            registry.replace_native_workflow_commands(
                crate::workflow_slash::workflow_slash_commands(&descriptors),
            );
            state.published_cwd = Some(cwd);
            state.generation = state.generation.wrapping_add(1);
            return;
        }
    }

    /// Build the synchronous /reload-skills finalizer without making the
    /// registry-owned handler keep this snapshot alive in a reference cycle.
    pub(crate) fn locked_post_reload_finalizer(
        self: &Arc<Self>,
        workflow_tool: Arc<WorkflowTool>,
    ) -> Arc<dyn Fn(&mut CommandRegistry) + Send + Sync> {
        let snapshot: Weak<Self> = Arc::downgrade(self);
        Arc::new(move |registry| {
            if let Some(snapshot) = snapshot.upgrade() {
                snapshot.replace_locked(registry, workflow_tool.as_ref());
            }
        })
    }

    /// Synchronous /reload-skills finalizer. The caller already owns the
    /// registry write lock, so this updates the same generation fence without
    /// attempting to acquire the async lock recursively.
    pub(crate) fn replace_locked(
        &self,
        registry: &mut CommandRegistry,
        workflow_tool: &WorkflowTool,
    ) {
        let cwd = self.current_cwd();
        let descriptors = workflow_tool.refresh_workflow_catalog(&cwd);
        registry.replace_native_workflow_commands(crate::workflow_slash::workflow_slash_commands(
            &descriptors,
        ));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.published_cwd = Some(cwd);
        // Advance only after the registry replacement is complete. A lazy
        // refresh may start while this caller owns the registry write lock;
        // this fence makes it retry instead of publishing descriptors it
        // computed before this explicit reload.
        state.generation = state.generation.wrapping_add(1);
    }

    fn current_cwd(&self) -> PathBuf {
        self.current_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::RegistrySlashDispatcher;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tool_api::SessionCwd;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn registry_owned_reload_finalizer_does_not_keep_snapshot_or_registry_alive() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let weak_registry = Arc::downgrade(&registry);
        let current_cwd = Arc::new(Mutex::new(PathBuf::from(".")));
        let snapshot = Arc::new(WorkflowCommandSnapshot::new(
            registry.clone(),
            Arc::new(RwLock::new(None::<Arc<WorkflowTool>>)),
            current_cwd,
        ));
        let weak_snapshot = Arc::downgrade(&snapshot);
        let finalizer = snapshot.locked_post_reload_finalizer(Arc::new(WorkflowTool::new(None)));
        let handler = Arc::new(
            command_api::builtins::ReloadSkillsHandler::new(registry.clone())
                .with_locked_post_reload_finalizer(false, finalizer),
        );
        registry.write().await.register_builtin_handler(handler);

        drop(snapshot);
        drop(registry);
        assert!(weak_snapshot.upgrade().is_none());
        assert!(weak_registry.upgrade().is_none());
    }

    fn write_workflow(cwd: &std::path::Path, name: &str) {
        let directory = cwd.join(branding::DOT_DIR).join("workflows");
        std::fs::create_dir_all(&directory).expect("create workflow directory");
        std::fs::write(
            directory.join(format!("{name}.js")),
            format!(
                "export const meta = {{ name: {name:?}, description: {name:?} }};\nawait agent('run');\n"
            ),
        )
        .expect("write workflow fixture");
    }

    #[tokio::test]
    async fn dispatcher_refreshes_live_workflow_snapshot_for_a_to_b_to_a_and_waits_for_registry() {
        let fixture = tempfile::tempdir().expect("temporary cwd fixture");
        let first_cwd = fixture.path().join("first");
        let second_cwd = fixture.path().join("second");
        std::fs::create_dir_all(&first_cwd).expect("first cwd");
        std::fs::create_dir_all(&second_cwd).expect("second cwd");
        let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let first_name = format!("cwd-first-{suffix}");
        let second_name = format!("cwd-second-{suffix}");
        write_workflow(&first_cwd, &first_name);
        write_workflow(&second_cwd, &second_name);

        let current_cwd = Arc::new(Mutex::new(first_cwd.clone()));
        let session_cwd = SessionCwd::new(first_cwd.clone(), vec![first_cwd.clone()]);
        session_cwd.link_live_cwd(current_cwd.clone());
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let workflow_tool = Arc::new(
            WorkflowTool::new(None)
                .with_current_cwd(current_cwd.clone())
                .with_allowed_builtin_workflow_names(Vec::new()),
        );
        let workflow_tool_slot = Arc::new(RwLock::new(Some(workflow_tool)));
        let snapshot = Arc::new(WorkflowCommandSnapshot::new(
            registry.clone(),
            workflow_tool_slot,
            current_cwd,
        ));
        snapshot.refresh().await;
        let dispatcher = RegistrySlashDispatcher::new(registry.clone())
            .with_command_snapshot_provider(snapshot.provider());

        let first = dispatcher.list_commands().await;
        assert!(first
            .iter()
            .any(|command| { command.name.string_at("") == Some(first_name.as_str()) }));
        assert!(!first
            .iter()
            .any(|command| { command.name.string_at("") == Some(second_name.as_str()) }));

        session_cwd.change_cwd(second_cwd.clone());
        let second = dispatcher.headless_command_catalog().await;
        assert!(second
            .prompt_command_names
            .iter()
            .any(|name| name.string_at("") == Some(second_name.as_str())));
        assert!(!second
            .prompt_command_names
            .iter()
            .any(|name| name.string_at("") == Some(first_name.as_str())));
        let old_command = dispatcher
            .dispatch_without_mod_hooks(
                &format!("/{first_name}").into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            old_command,
            lingxi_core::host::SlashDispatchResult::Unknown { .. }
        ));
        let current_command = dispatcher
            .dispatch_without_mod_hooks(
                &format!("/{second_name}").into(),
                lingxi_core::host::CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            current_command,
            lingxi_core::host::SlashDispatchResult::RunSkillAsTurn { command, .. }
                if command.string_at("").is_some_and(|text| text.contains(second_name.as_str()))
        ));

        session_cwd.change_cwd(first_cwd.clone());
        let first_again = dispatcher.list_palette_commands().await;
        assert!(first_again
            .iter()
            .any(|command| { command.name.string_at("") == Some(first_name.as_str()) }));
        assert!(!first_again
            .iter()
            .any(|command| { command.name.string_at("") == Some(second_name.as_str()) }));

        session_cwd.change_cwd(second_cwd);
        let held_registry = registry.write().await;
        let dispatch = dispatcher.clone_shared();
        let wait_for_snapshot = tokio::spawn(async move { dispatch.list_commands().await });
        tokio::task::yield_now().await;
        assert!(
            !wait_for_snapshot.is_finished(),
            "consumer must await the registry writer rather than return its old cwd snapshot"
        );
        drop(held_registry);
        let after_unlock = wait_for_snapshot.await.expect("list task completes");
        assert!(after_unlock
            .iter()
            .any(|command| { command.name.string_at("") == Some(second_name.as_str()) }));
        assert!(!after_unlock
            .iter()
            .any(|command| { command.name.string_at("") == Some(first_name.as_str()) }));
    }
}
