//! Implement mode: every panel changes the code in its own git worktree, and
//! the host collects each worktree's patch and verifies it.
//!
//! The pieces, in run order:
//!
//! - [`Workspaces::prepare`] checks the host can confine the panels, snapshots
//!   the user's workspace (uncommitted work included) and creates one worktree
//!   per panel from that snapshot, before anything is spent.
//! - Each panel's tool calls go through a [`WorktreeScopedInvoker`]: file
//!   tools may only touch the panel's own worktree, nothing can prompt the
//!   user, and Bash cannot opt out of the sandbox (which phase 3 roots at the
//!   worktree).
//! - [`Workspaces::collect_patches`] and [`Workspaces::verify`] record what
//!   each panel changed and how the configured verification commands came
//!   out. A panel's own account of either is never trusted.
//! - [`Workspaces::finish`] discards the worktrees without changes and keeps
//!   the rest for the parent model to read. Dropping [`Workspaces`] without
//!   finishing it (a cancel, or the run future being dropped) discards all
//!   of them.

use crate::config::FusionRuntimeConfig;
use crate::panel::PanelInternal;
use async_trait::async_trait;
use lingxi_core::host::tool_invoker::{
    SubagentInvocationContext, ToolInvocationResult, ToolInvoker, ToolInvokerError,
};
use lingxi_core::host::{
    panel_never_dispatched, truncate_at_char_boundary, FusionError, FusionImplementHost,
    PanelPatch, PanelVerification, SnapshotLimits, VerificationOutcome, VerificationRun,
    WorkspaceBase, WorktreeError, WorktreeHandle, FUSION_MATERIAL_DIFF_BYTE_CAP,
    FUSION_MATERIAL_MAX_PATCH_FILES,
};
use serde_json::Value;
use std::any::Any;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Worktree directory names start with this, so a sweep only ever touches
/// Fusion's own worktrees.
const WORKTREE_PREFIX: &str = "fusion-";

/// What the host learned about one panel's worktree, stage by stage.
#[derive(Debug, Clone, Default)]
pub struct PanelImplementState {
    /// The panel's worktree; `None` in analysis mode or when it could not be
    /// created.
    pub worktree: Option<WorktreeHandle>,
    /// What the panel changed relative to the run's base.
    pub patch: Option<PanelPatch>,
    /// Why the patch could not be collected.
    pub patch_error: Option<String>,
    /// The verification commands' results.
    pub verification: Option<PanelVerification>,
}

impl PanelImplementState {
    /// `true` when the panel left changes in its worktree.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.patch.as_ref().is_some_and(|patch| !patch.is_empty())
    }
}

/// One panel's worktree as handed to its spawn.
#[derive(Clone)]
pub(crate) struct PanelWorkspace {
    pub(crate) handle: WorktreeHandle,
    /// The parent's invoker, confined to [`Self::handle`].
    pub(crate) invoker: Arc<dyn ToolInvoker>,
}

/// The run's worktrees, indexed by panel spawn index.
pub(crate) struct Workspaces {
    host: Arc<dyn FusionImplementHost>,
    pub(crate) base: WorkspaceBase,
    /// `None` where the worktree could not be created; that panel is never
    /// dispatched.
    pub(crate) panels: Vec<Option<PanelWorkspace>>,
    armed: bool,
}

impl Workspaces {
    /// Check the host, snapshot the workspace and create one worktree per
    /// panel, named after the anonymous id the panel will report under
    /// (`anon_rank[i]` is spawn index `i`'s rank). Nothing is spent here, so
    /// every refusal is [`FusionError::ImplementUnavailable`].
    pub(crate) async fn prepare(
        host: Arc<dyn FusionImplementHost>,
        config: &FusionRuntimeConfig,
        run_id: &str,
        anon_rank: &[usize],
        parent_invoker: &Arc<dyn ToolInvoker>,
    ) -> Result<Self, FusionError> {
        host.preflight(config.implement.min_free_disk_bytes)
            .await
            .map_err(FusionError::ImplementUnavailable)?;
        let worktrees = host.worktrees();
        if !worktrees.is_supported() {
            return Err(FusionError::ImplementUnavailable(
                "git worktrees are not supported on this host".into(),
            ));
        }
        let limits = SnapshotLimits {
            max_untracked_files: config.implement.max_untracked_files,
            max_untracked_bytes: config.implement.max_untracked_bytes,
        };
        let base = worktrees
            .snapshot_base(limits)
            .await
            .map_err(|error| match error {
                WorktreeError::SnapshotRefused(message) => {
                    FusionError::ImplementUnavailable(message)
                }
                WorktreeError::Unsupported => FusionError::ImplementUnavailable(
                    "the workspace is not a git repository".into(),
                ),
                other => FusionError::ImplementUnavailable(format!(
                    "could not snapshot the workspace: {other}"
                )),
            })?;
        let short = short_run_id(run_id);
        let mut this = Self {
            host,
            base,
            panels: Vec::with_capacity(anon_rank.len()),
            armed: true,
        };
        for rank in anon_rank {
            let slug = format!("{WORKTREE_PREFIX}{short}-p{}", rank + 1);
            match worktrees
                .create_worktree(&slug, Some(&this.base.commit), &[])
                .await
            {
                Ok(handle) => this.panels.push(Some(PanelWorkspace {
                    invoker: Arc::new(WorktreeScopedInvoker::new(
                        Arc::clone(parent_invoker),
                        &handle.path,
                    )),
                    handle,
                })),
                Err(error) => {
                    tracing::warn!(%error, slug, "fusion: panel worktree could not be created");
                    this.panels.push(None);
                }
            }
        }
        let Some(created) = this.panels.iter().flatten().next() else {
            return Err(FusionError::ImplementUnavailable(
                "no panel worktree could be created".into(),
            ));
        };
        if let Some(dir) = created.handle.path.parent() {
            let current: Vec<PathBuf> = this
                .panels
                .iter()
                .flatten()
                .map(|panel| panel.handle.path.clone())
                .collect();
            sweep_stale(
                this.host.as_ref(),
                dir,
                &current,
                Duration::from_secs(u64::from(config.implement.retain_hours) * 3600),
            )
            .await;
        }
        Ok(this)
    }

    /// The worktree of spawn index `index`.
    pub(crate) fn panel(&self, index: usize) -> Option<&PanelWorkspace> {
        self.panels.get(index).and_then(Option::as_ref)
    }

    /// Record, for every dispatched panel, its worktree and what it changed
    /// relative to the base. The full patch is written next to the worktree
    /// (`<worktree>.patch`); the material keeps a capped copy.
    pub(crate) async fn collect_patches(&self, panels: &mut [PanelInternal]) {
        let worktrees = self.host.worktrees();
        for panel in panels.iter_mut() {
            let Some(workspace) = self.panel(panel.index) else {
                continue;
            };
            panel.implement.worktree = Some(workspace.handle.clone());
            if panel_never_dispatched(panel.error_category.as_deref()) {
                continue;
            }
            match worktrees
                .worktree_patch(&workspace.handle, &self.base.commit)
                .await
            {
                Ok(patch) => {
                    panel.implement.patch =
                        Some(material_patch(&workspace.handle, &self.base, patch).await);
                }
                Err(error) => {
                    panel.implement.patch_error =
                        Some(truncate_at_char_boundary(&error.to_string(), 512));
                }
            }
        }
    }

    /// Run `commands` in every worktree that has changes, `concurrency`
    /// worktrees at a time, each command capped at `per_command` and at
    /// `deadline`. With no commands, those panels are marked
    /// [`PanelVerification::NotConfigured`].
    pub(crate) async fn verify(
        &self,
        panels: &mut [PanelInternal],
        commands: &[String],
        per_command: Duration,
        concurrency: u8,
        deadline: Instant,
        cancel: &CancellationToken,
    ) {
        if commands.is_empty() {
            for panel in panels.iter_mut().filter(|p| p.implement.has_changes()) {
                panel.implement.verification = Some(PanelVerification::NotConfigured);
            }
            return;
        }
        let permits = Arc::new(Semaphore::new(usize::from(concurrency.max(1))));
        let mut tasks = JoinSet::new();
        for (slot, panel) in panels.iter().enumerate() {
            let Some(worktree) = panel
                .implement
                .worktree
                .as_ref()
                .filter(|_| panel.implement.has_changes())
            else {
                continue;
            };
            let host = Arc::clone(&self.host);
            let path = worktree.path.clone();
            let commands = commands.to_vec();
            let permits = Arc::clone(&permits);
            let cancel = cancel.child_token();
            tasks.spawn(async move {
                let _permit = permits.acquire_owned().await;
                let mut runs = Vec::with_capacity(commands.len());
                for command in commands {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() || cancel.is_cancelled() {
                        runs.push(VerificationRun {
                            command,
                            outcome: VerificationOutcome::Error {
                                message: "not run: the run's deadline was reached".into(),
                            },
                            duration_ms: 0,
                            output_tail: String::new(),
                        });
                        continue;
                    }
                    runs.push(
                        host.verify(&path, &command, per_command.min(remaining), cancel.clone())
                            .await,
                    );
                }
                (slot, runs)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            if let Ok((slot, runs)) = joined {
                panels[slot].implement.verification = Some(PanelVerification::Runs(runs));
            }
        }
    }

    /// The run is over: discard the worktrees without changes and keep the
    /// rest (with their patch files) for the parent model.
    pub(crate) async fn finish(mut self, panels: &[PanelInternal]) {
        self.armed = false;
        let worktrees = self.host.worktrees();
        for (index, workspace) in self.panels.iter().enumerate() {
            let Some(workspace) = workspace else { continue };
            let keep = panels
                .iter()
                .any(|panel| panel.index == index && panel.implement.has_changes());
            if !keep {
                let _ = worktrees.discard_worktree(&workspace.handle).await;
                remove_patch_file(&workspace.handle.path).await;
            }
        }
    }
}

impl Drop for Workspaces {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let handles: Vec<WorktreeHandle> = self
            .panels
            .iter()
            .flatten()
            .map(|panel| panel.handle.clone())
            .collect();
        if handles.is_empty() {
            return;
        }
        let worktrees = self.host.worktrees();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    for handle in handles {
                        let _ = worktrees.discard_worktree(&handle).await;
                        remove_patch_file(&handle.path).await;
                    }
                });
            }
            Err(_) => tracing::warn!(
                count = handles.len(),
                "fusion: no runtime to discard panel worktrees; the retention sweep will"
            ),
        }
    }
}

/// The ulid's random tail, lowercase: short enough for a directory name and
/// still unique per run.
fn short_run_id(run_id: &str) -> String {
    let id: String = run_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    id[id.len().saturating_sub(10)..].to_string()
}

fn patch_file_path(worktree: &Path) -> PathBuf {
    worktree.with_extension("patch")
}

async fn remove_patch_file(worktree: &Path) {
    let _ = tokio::fs::remove_file(patch_file_path(worktree)).await;
}

async fn material_patch(
    handle: &WorktreeHandle,
    base: &WorkspaceBase,
    patch: lingxi_core::host::WorktreePatch,
) -> PanelPatch {
    let insertions = patch.files.iter().map(|f| u64::from(f.insertions)).sum();
    let deletions = patch.files.iter().map(|f| u64::from(f.deletions)).sum();
    let patch_file = if patch.diff.is_empty() {
        None
    } else {
        let path = patch_file_path(&handle.path);
        match tokio::fs::write(&path, patch.diff.as_bytes()).await {
            Ok(()) => Some(path.to_string_lossy().into_owned()),
            Err(error) => {
                tracing::warn!(%error, "fusion: could not write the panel patch file");
                None
            }
        }
    };
    let files_omitted = patch
        .files
        .len()
        .saturating_sub(FUSION_MATERIAL_MAX_PATCH_FILES);
    let mut files = patch.files;
    files.truncate(FUSION_MATERIAL_MAX_PATCH_FILES);
    let diff = truncate_at_char_boundary(&patch.diff, FUSION_MATERIAL_DIFF_BYTE_CAP);
    PanelPatch {
        worktree: handle.path.to_string_lossy().into_owned(),
        branch: handle.branch_name.clone(),
        base_commit: base.commit.clone(),
        patch_file,
        files,
        files_omitted,
        insertions,
        deletions,
        diff_truncated: diff.len() != patch.diff.len(),
        diff,
    }
}

/// Whether `name` is one of the directory names [`Workspaces::prepare`]
/// mints (`fusion-<run id>-p<n>`); nothing else is ever swept or cleaned, so a
/// worktree the user made themselves is safe even if it starts with `fusion-`.
fn is_fusion_worktree_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(WORKTREE_PREFIX) else {
        return false;
    };
    let Some((run, panel)) = rest.rsplit_once("-p") else {
        return false;
    };
    (1..=16).contains(&run.len())
        && run
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && (1..=2).contains(&panel.len())
        && panel.bytes().all(|b| b.is_ascii_digit())
}

/// Discard every Fusion worktree and its patch file, whatever its age
/// (`/fusion clean`). Returns how many worktrees were removed. Best effort,
/// like the retention sweep; the caller makes sure no run is using them.
pub async fn clean_worktrees(host: &dyn FusionImplementHost) -> usize {
    let worktrees = host.worktrees();
    let Ok(listed) = worktrees.list_worktrees().await else {
        return 0;
    };
    let mut removed = 0;
    for info in listed {
        let is_ours = info
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_fusion_worktree_name);
        if !is_ours {
            continue;
        }
        let handle = WorktreeHandle {
            branch_name: info.branch.clone(),
            path: info.path.clone(),
            base_commit: None,
        };
        if worktrees.discard_worktree(&handle).await.is_ok() {
            removed += 1;
        }
        remove_patch_file(&info.path).await;
    }
    removed
}

/// Discard Fusion worktrees in `dir` older than `retain` (and their patch
/// files), except `current`. Best effort: this is housekeeping.
async fn sweep_stale(
    host: &dyn FusionImplementHost,
    dir: &Path,
    current: &[PathBuf],
    retain: Duration,
) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let worktrees = host.worktrees();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !is_fusion_worktree_name(&name) || current.contains(&path) {
            continue;
        }
        let stale = entry
            .metadata()
            .await
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > retain);
        if stale {
            let handle = WorktreeHandle {
                branch_name: format!("worktree-{name}"),
                path: path.clone(),
                base_commit: None,
            };
            let _ = worktrees.discard_worktree(&handle).await;
            remove_patch_file(&path).await;
        }
    }
}

/// File tools and the input field holding the path they touch.
const PATH_ARGS: [(&str, &str); 6] = [
    ("Read", "file_path"),
    ("Edit", "file_path"),
    ("Write", "file_path"),
    ("NotebookEdit", "notebook_path"),
    ("Grep", "path"),
    ("Glob", "path"),
];

/// The parent's tool invoker, confined to one panel's worktree.
///
/// - A file tool whose target resolves (symlinks included) outside the
///   worktree is refused before the parent's gate ever sees it, so the panel
///   can neither write the user's workspace nor read another panel's
///   worktree.
/// - Calls run as a non-interactive session that cannot prompt: anything the
///   parent's rules would ask about is denied instead of bubbling N prompts
///   to the user.
/// - Edits run under `acceptEdits`, so edits inside the worktree (which lies
///   inside the session's working directory) need no approval, while the
///   parent's deny rules and protected paths still apply.
/// - Bash cannot opt out of the sandbox, which roots its writes at the
///   worktree (phase 3).
pub(crate) struct WorktreeScopedInvoker {
    inner: Arc<dyn ToolInvoker>,
    root: PathBuf,
}

impl WorktreeScopedInvoker {
    pub(crate) fn new(inner: Arc<dyn ToolInvoker>, root: &Path) -> Self {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| lexical_normalize(root));
        Self { inner, root }
    }

    fn scope(
        &self,
        name: &str,
        mut input: Value,
        mut ctx: SubagentInvocationContext,
    ) -> Result<(Value, SubagentInvocationContext), ToolInvokerError> {
        if let Some((_, key)) = PATH_ARGS.iter().find(|(tool, _)| *tool == name) {
            if let Some(raw) = input.get(*key).and_then(Value::as_str) {
                if !self.contains(raw) {
                    return Err(self.outside(raw));
                }
            }
        }
        if name == "Glob" {
            if let Some(pattern) = input.get("pattern").and_then(Value::as_str) {
                if !self.glob_stays_inside(pattern) {
                    return Err(self.outside(pattern));
                }
            }
        }
        if name == "Bash" {
            if let Some(fields) = input.as_object_mut() {
                fields.remove("dangerouslyDisableSandbox");
            }
        }
        ctx.cwd = Some(self.root.clone());
        ctx.is_non_interactive_session = true;
        ctx.can_show_permission_prompts = false;
        ctx.mode_override = Some("acceptEdits".into());
        Ok((input, ctx))
    }

    /// Whether `raw` (absolute, or relative to the worktree) resolves inside
    /// the worktree once `..` and every existing symlink are resolved.
    fn contains(&self, raw: &str) -> bool {
        if raw.starts_with('~') {
            return false;
        }
        let path = Path::new(raw);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        resolve_existing_prefix(&lexical_normalize(&joined)).starts_with(&self.root)
    }

    /// A Glob pattern may itself name a directory; its literal prefix (up to
    /// the first wildcard) must stay inside, and it may not climb with `..`.
    fn glob_stays_inside(&self, pattern: &str) -> bool {
        if pattern.split('/').any(|segment| segment == "..") {
            return false;
        }
        if !pattern.starts_with('/') && !pattern.starts_with('~') {
            return true;
        }
        let literal: Vec<&str> = pattern
            .split('/')
            .take_while(|segment| !segment.contains(['*', '?', '[', '{']))
            .collect();
        self.contains(&literal.join("/"))
    }

    fn outside(&self, raw: &str) -> ToolInvokerError {
        ToolInvokerError::Internal(format!(
            "Permission denied: {raw} is outside this panel's working copy {}. \
             Work only inside the current directory.",
            self.root.display()
        ))
    }
}

#[async_trait]
impl ToolInvoker for WorktreeScopedInvoker {
    async fn invoke(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        let (input, ctx) = self.scope(name, input, ctx)?;
        self.inner.invoke(name, input, ctx).await
    }

    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<Value, ToolInvokerError> {
        let (input, ctx) = self.scope(name, input, ctx)?;
        self.inner
            .invoke_with_workspace_lease(name, input, ctx, workspace_lease_token)
            .await
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        let (input, ctx) = self.scope(name, input, ctx)?;
        self.inner
            .invoke_detailed(name, input, ctx, workspace_lease_token)
            .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
    async fn cleanup_computer_inputs(
        &self,
        agent_id: lingxi_core::types::AgentId,
        origin_session_id: Option<lingxi_core::types::SessionId>,
    ) -> Result<(), lingxi_core::host::tool_invoker::ToolInvokerError> {
        self.inner
            .cleanup_computer_inputs(agent_id, origin_session_id)
            .await
    }
}

/// `path` with `.` and `..` resolved without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with its longest existing ancestor canonicalized (following
/// symlinks) and the not-yet-existing rest appended, which cannot contain a
/// symlink.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut resolved = canonical;
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::tool_invoker::ToolExecutionPolicy;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<(String, Value, SubagentInvocationContext)>>,
    }

    #[async_trait]
    impl ToolInvoker for Recorder {
        async fn invoke(
            &self,
            name: &str,
            input: Value,
            ctx: SubagentInvocationContext,
        ) -> Result<Value, ToolInvokerError> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), input, ctx));
            Ok(Value::Null)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn ctx() -> SubagentInvocationContext {
        SubagentInvocationContext {
            permission_pause_observer: None,
            parent_agent_id: None,
            origin_session_id: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: true,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 1,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        worktree: PathBuf,
        recorder: Arc<Recorder>,
        invoker: WorktreeScopedInvoker,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        let worktree = repo.join(".wt/fusion-x-p1");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(repo.join("secret.rs"), "x").unwrap();
        let recorder = Arc::new(Recorder::default());
        let invoker = WorktreeScopedInvoker::new(recorder.clone(), &worktree);
        Fixture {
            _dir: dir,
            repo,
            worktree,
            recorder,
            invoker,
        }
    }

    #[tokio::test]
    async fn file_tools_stay_inside_the_worktree() {
        let f = fixture();
        let inside = f.worktree.join("src/a.rs");
        let new_file = f.worktree.join("src/new/b.rs");
        for (tool, key, path) in [
            ("Read", "file_path", inside.to_str().unwrap()),
            ("Write", "file_path", new_file.to_str().unwrap()),
            ("Grep", "path", "src"),
        ] {
            f.invoker
                .invoke(tool, serde_json::json!({ key: path }), ctx())
                .await
                .unwrap();
        }
        let escapes = [
            ("Read", "file_path", f.repo.join("secret.rs")),
            ("Edit", "file_path", f.worktree.join("../../secret.rs")),
            ("Grep", "path", f.repo.join(".wt/fusion-x-p2")),
            ("Write", "file_path", PathBuf::from("~/.bashrc")),
        ];
        for (tool, key, path) in escapes {
            let err = f
                .invoker
                .invoke(tool, serde_json::json!({ key: path }), ctx())
                .await
                .unwrap_err();
            assert!(
                matches!(&err, ToolInvokerError::Internal(msg) if msg.contains("outside this panel's working copy")),
                "{tool} {path:?}: {err:?}"
            );
        }
        assert_eq!(f.recorder.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_worktree_is_refused() {
        let f = fixture();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&f.repo, f.worktree.join("escape")).unwrap();
        let target = f.worktree.join("escape/secret.rs");
        let err = f
            .invoker
            .invoke("Edit", serde_json::json!({ "file_path": target }), ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolInvokerError::Internal(_)));
    }

    #[tokio::test]
    async fn globs_cannot_climb_or_point_elsewhere() {
        let f = fixture();
        f.invoker
            .invoke(
                "Glob",
                serde_json::json!({ "pattern": "src/**/*.rs" }),
                ctx(),
            )
            .await
            .unwrap();
        let inside = format!("{}/src/**/*.rs", f.worktree.display());
        f.invoker
            .invoke("Glob", serde_json::json!({ "pattern": inside }), ctx())
            .await
            .unwrap();
        for pattern in [
            "../**/*.rs".to_string(),
            format!("{}/*.rs", f.repo.display()),
        ] {
            assert!(f
                .invoker
                .invoke("Glob", serde_json::json!({ "pattern": pattern }), ctx())
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn calls_are_non_interactive_accept_edits_in_the_worktree_and_bash_stays_sandboxed() {
        let f = fixture();
        f.invoker
            .invoke(
                "Bash",
                serde_json::json!({ "command": "cargo test", "dangerouslyDisableSandbox": true }),
                ctx(),
            )
            .await
            .unwrap();
        let calls = f.recorder.calls.lock().unwrap();
        let (name, input, ctx) = &calls[0];
        assert_eq!(name, "Bash");
        assert_eq!(input, &serde_json::json!({ "command": "cargo test" }));
        assert!(ctx.is_non_interactive_session);
        assert!(!ctx.can_show_permission_prompts);
        assert_eq!(ctx.mode_override.as_deref(), Some("acceptEdits"));
        assert_eq!(ctx.cwd.as_deref(), Some(f.worktree.as_path()));
    }

    #[test]
    fn only_names_the_run_mints_count_as_fusion_worktrees() {
        for ok in ["fusion-k2abcdef12-p1", "fusion-ab-p12"] {
            assert!(is_fusion_worktree_name(ok), "{ok}");
        }
        for bad in [
            "fusion-k2abcdef12-p1.patch",
            "fusion-x",
            "fusion--p1",
            "fusion-UPPER-p1",
            "fusion-k2abcdef12-p123",
            "agent-abc",
            "my-fusion-a-p1",
        ] {
            assert!(!is_fusion_worktree_name(bad), "{bad}");
        }
    }

    #[test]
    fn short_run_ids_are_slug_safe() {
        assert_eq!(short_run_id("fu_01J8ZQ4XK2ABCDEF12"), "k2abcdef12");
        assert_eq!(short_run_id("ab"), "ab");
    }
}
