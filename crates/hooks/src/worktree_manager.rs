//! Worktree lifecycle ownership shared by tools, CLI launch and subagents.

use crate::{
    ExactHookText, HookContext, HookDefinition, HookEvent, HookEventType, HookExecutor,
    HookExecutorImpl, HookOutcome, HookResult, HookSource,
};
use async_trait::async_trait;
use lingxi_core::host::worktree::{
    SnapshotLimits, WorkspaceBase, WorktreeChangeSummary, WorktreeError, WorktreeHandle,
    WorktreeInfo, WorktreeManager, WorktreePatch,
};
use lingxi_core::types::HookId;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Classic lifecycle result before path selection. Source identity belongs to
/// the registered hook, never to a field supplied by its response.
#[derive(Clone, Debug)]
pub struct WorktreeHookResult {
    pub hook_id: HookId,
    pub source: HookSource,
    pub command: ExactHookText,
    pub succeeded: bool,
    pub output: ExactHookText,
    pub exit_code: Option<i32>,
    /// Native callbacks are host-owned and never personal answers.
    pub can_be_personal: bool,
}

impl WorktreeHookResult {
    pub(crate) fn from_result(hook: &HookDefinition, result: &HookResult) -> Self {
        let succeeded = result.outcome == HookOutcome::Success;
        // Native cU keeps command stdout verbatim. HTTP and callback arms use
        // only the structured WorktreeCreate path, never their JSON body.
        let output = if !succeeded {
            if matches!(hook.executor, HookExecutor::Command { .. }) || !result.stderr.is_empty() {
                result.stderr.clone()
            } else {
                result.stdout.clone()
            }
        } else if matches!(hook.executor, HookExecutor::Command { .. }) {
            result.stdout.clone()
        } else if matches!(
            hook.executor,
            HookExecutor::Http { .. } | HookExecutor::McpTool { .. }
        ) && result
            .response
            .as_ref()
            .is_some_and(|response| response.decision == Some(crate::HookDecision::Block))
        {
            result
                .response
                .as_ref()
                .and_then(|response| response.reason.clone())
                .unwrap_or_default()
        } else if matches!(hook.executor, HookExecutor::McpTool { .. }) {
            result.stdout.clone()
        } else {
            result
                .response
                .as_ref()
                .and_then(|response| response.worktree_path.clone())
                .unwrap_or_default()
        };
        Self {
            hook_id: hook.id,
            source: hook.source,
            command: match &hook.executor {
                HookExecutor::Builtin { .. } => "callback".into(),
                HookExecutor::Function { .. } => "function".into(),
                _ => {
                    let mut raw = hook.clone();
                    raw.status_message = None;
                    crate::attachment::attachment_command(&raw)
                }
            },
            succeeded,
            output,
            exit_code: result.exit_code,
            can_be_personal: !matches!(hook.executor, HookExecutor::Builtin { .. }),
        }
    }

    /// Native VRo/uK: strip ANSI, choose the last nonempty trimmed line, then
    /// choose the first successful hook containing such a line.
    pub fn selected_path(&self) -> Option<ExactHookText> {
        let text = strip_ansi_units(&self.output);
        text.utf16_code_units
            .split(|unit| *unit == u16::from(b'\n'))
            .map(|line| ExactHookText::from_utf16(line.to_vec()).trim_js())
            .rfind(|line| !line.is_empty())
    }
}

/// Native strips ANSI before interpreting stdout, without normalizing its
/// remaining UTF-16. Keep lone surrogates available to the response API.
fn strip_ansi_units(text: &ExactHookText) -> ExactHookText {
    // Bun.stripANSI uses terminal control-string state, including DCS/SOS/PM/
    // APC and cancellation. A regex or a CSI-only stripper leaves log payloads
    // in the folder name. The fixture covers the intrinsic boundary directly.
    const PLAIN: u8 = 0;
    const ESCAPE: u8 = 1;
    const CSI: u8 = 2;
    const OSC: u8 = 3;
    const CONTROL_STRING: u8 = 4;
    const OSC_ESCAPE: u8 = 5;
    const STRING_ESCAPE: u8 = 6;
    const INTERMEDIATE: u8 = 7;
    let mut state = PLAIN;
    let mut out = Vec::with_capacity(text.utf16_code_units.len());
    for &unit in &text.utf16_code_units {
        match state {
            PLAIN => match unit {
                0x1b => state = ESCAPE,
                0x9b => state = CSI,
                0x9d => state = OSC,
                0x90 | 0x98 | 0x9e | 0x9f => state = CONTROL_STRING,
                _ => out.push(unit),
            },
            ESCAPE => match unit {
                0x1b => {}
                0x18 | 0x1a => state = PLAIN,
                0x5b | 0x9b => state = CSI,
                0x5d | 0x9d => state = OSC,
                0x90 | 0x98 | 0x9e | 0x9f => state = CONTROL_STRING,
                0x50 | 0x58 | 0x5e | 0x5f => state = CONTROL_STRING,
                0x20..=0x2f => state = INTERMEDIATE,
                0x30..=0x7e => state = PLAIN,
                _ => {
                    state = PLAIN;
                    out.push(unit);
                }
            },
            INTERMEDIATE => match unit {
                0x1b => state = ESCAPE,
                0x18 | 0x1a => state = PLAIN,
                0x20..=0x2f => {}
                0x30..=0x7e => state = PLAIN,
                _ => {
                    state = PLAIN;
                    out.push(unit);
                }
            },
            CSI => match unit {
                0x1b => state = ESCAPE,
                0x18 | 0x1a | 0x40..=0x7e => state = PLAIN,
                _ => {}
            },
            OSC | CONTROL_STRING => {
                if matches!(unit, 0x18 | 0x1a | 0x9c) || (state == OSC && unit == 7) {
                    state = PLAIN;
                } else if unit == 0x1b {
                    state = if state == OSC {
                        OSC_ESCAPE
                    } else {
                        STRING_ESCAPE
                    };
                }
            }
            OSC_ESCAPE | STRING_ESCAPE => {
                if matches!(unit, 0x18 | 0x1a | 0x5c | 0x9c) || (state == OSC_ESCAPE && unit == 7) {
                    state = PLAIN;
                } else if unit != 0x1b {
                    state = if state == OSC_ESCAPE {
                        OSC
                    } else {
                        CONTROL_STRING
                    };
                }
            }
            _ => unreachable!(),
        }
    }
    ExactHookText::from_utf16(out)
}

pub type PersonalHookClassifier = dyn Fn(HookId, HookSource) -> bool + Send + Sync;

/// Adapts a platform manager once at composition, so every create path shares
/// hook selection and every hook-owned folder retains its cleanup owner.
pub struct HookedWorktreeManager {
    inner: Arc<dyn WorktreeManager>,
    hooks: Arc<HookExecutorImpl>,
    original_cwd: PathBuf,
    context: Arc<dyn Fn() -> HookContext + Send + Sync>,
    owned: Mutex<HashMap<PathBuf, (WorktreeHandle, SystemTime)>>,
    restrict_personal_config: bool,
    remote_original_base: bool,
    personal_hook: Arc<PersonalHookClassifier>,
    workspace_trusted: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl HookedWorktreeManager {
    pub fn new(
        inner: Arc<dyn WorktreeManager>,
        hooks: Arc<HookExecutorImpl>,
        original_cwd: PathBuf,
        context: Arc<dyn Fn() -> HookContext + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            hooks,
            original_cwd,
            context,
            owned: Mutex::new(HashMap::new()),
            restrict_personal_config: false,
            remote_original_base: false,
            personal_hook: Arc::new(|_, source| source.personal_config_origin().unwrap_or(true)),
            workspace_trusted: Arc::new(|| true),
        }
    }

    /// Restriction is independent of workspace trust and permission mode.
    pub fn with_restrict_personal_config(mut self, restrict: bool) -> Self {
        self.restrict_personal_config = restrict;
        self
    }

    /// Remote hook inputs can contain the peer's cwd. Resolve their relative
    /// output against this host's original session folder, matching qUe.
    pub fn with_remote_original_base(mut self, remote: bool) -> Self {
        self.remote_original_base = remote;
        self
    }

    /// Hosts with user agent/skill/plugin origin information can retain that
    /// exact fact without treating SDK callbacks or managed plugins as personal.
    pub fn with_personal_hook_classifier(
        mut self,
        classifier: Arc<PersonalHookClassifier>,
    ) -> Self {
        self.personal_hook = classifier;
        self
    }

    /// Consult live loader provenance so hooks registered after startup retain
    /// their origin. Lock contention and unknown bundle scope hold a personal
    /// restricted answer; ordinary unrestricted execution is unchanged.
    pub fn with_personal_hook_registry(
        mut self,
        registry: Arc<tokio::sync::RwLock<crate::HookRegistry>>,
    ) -> Self {
        self.personal_hook = Arc::new(move |id, source| {
            registry.try_read().map_or_else(
                |_| source.personal_config_origin().unwrap_or(true),
                |registry| registry.personal_hook_origin(id, source).unwrap_or(true),
            )
        });
        self
    }

    pub fn with_workspace_trust_provider(
        mut self,
        trusted: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        self.workspace_trusted = trusted;
        self
    }

    fn held(&self, result: &WorktreeHookResult) -> bool {
        self.restrict_personal_config
            && result.can_be_personal
            && (self.personal_hook)(result.hook_id, result.source)
    }

    fn remember(&self, handle: WorktreeHandle) {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle.path.clone(), (handle, SystemTime::now()));
    }
}

fn native_diagnostic(text: &ExactHookText, phase: &str) -> Result<ExactHookText, WorktreeError> {
    let input = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&format!(
        "{{\"phase\":\"{}\",\"text\":{}}}",
        phase,
        text.json_projection()
            .to_json_string()
            .map_err(|error| WorktreeError::Hook(error.to_string()))?,
    ))
    .map_err(|error| WorktreeError::Hook(error.to_string()))?;
    let result = crate::function_hook::Sandbox::default()
        .eval(include_str!("worktree_manager/native_display.js"), &input)
        .map_err(|error| {
            WorktreeError::Hook(format!(
                "Worktree hook diagnostic could not be formatted: {error}"
            ))
        })?;
    ExactHookText::from_json_projection(&result, "").ok_or_else(|| {
        WorktreeError::Hook("Worktree hook diagnostic formatter returned a non-string".into())
    })
}

const CONFIGURED_NO_CREATE: &str = "WorktreeCreate hook failed: hook is configured but did not run (workspace not trusted or matcher mismatch)";
const NO_CREATE_PATH: &str = "WorktreeCreate hook failed: hook succeeded but returned no worktree path (command: echo the path to stdout; http/callback: return hookSpecificOutput.worktreePath)";
const PERSONAL_CREATE_PATH: &str = "A WorktreeCreate hook from your own LingXi files ran, but this session doesn't let your own hooks choose the worktree folder (LINGXI_RESTRICT_PERSONAL_CONFIG), so LingXi didn't use any folder the hook made (remove that folder if you don't need it). Remove that hook, or turn off the skill, agent or plugin that adds it, to create worktrees in this session.";

fn absolute_path(base: &Path, selected: &str) -> PathBuf {
    let path = Path::new(selected);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let mut resolved = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

fn screen_hook_path(path: &Path, original_cwd: &Path) -> Result<(), WorktreeError> {
    // Absolute output retains its spelling until the native ancestry screen.
    if path
        .to_string_lossy()
        .split(std::path::MAIN_SEPARATOR)
        .any(|part| matches!(part, "." | ".."))
    {
        return Err(WorktreeError::Hook(format!(
            "Cannot use the WorktreeCreate hook's worktree: the hook emitted a path with dot segments ({}). The symlink screen cannot verify a dotted spelling — have the hook emit a normalized (dot-free) absolute path and retry.", path.display()
        )));
    }
    // Hook outputs may intentionally live outside the repository. Only inspect
    // ancestry below a known checkout, preserving that supported native case.
    let mut roots = vec![original_cwd.to_path_buf()];
    for cwd in
        std::iter::once(original_cwd.to_path_buf()).chain(std::fs::canonicalize(original_cwd).ok())
    {
        if let Some(root) = cwd
            .ancestors()
            .find(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok())
        {
            if !roots.iter().any(|known| known == root) {
                roots.push(root.to_path_buf());
            }
        }
    }
    for root in roots {
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        let mut component_path = root;
        for component in relative.components() {
            component_path.push(component.as_os_str());
            let metadata = match std::fs::symlink_metadata(&component_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(WorktreeError::Hook(format!(
                    "Cannot use the WorktreeCreate hook's worktree: {} could not be verified. A repository-committed symlink below the checkout root could redirect the worktree outside the repository. Remove the symlink (or emit a path outside the repository) and retry.", component_path.display()
                ))),
            };
            if metadata.file_type().is_symlink() {
                return Err(WorktreeError::Hook(format!(
                    "Cannot use the WorktreeCreate hook's worktree: {} is a symlink. A repository-committed symlink below the checkout root could redirect the worktree outside the repository. Remove the symlink (or emit a path outside the repository) and retry.", component_path.display()
                )));
            }
        }
    }
    Ok(())
}

#[async_trait]
impl WorktreeManager for HookedWorktreeManager {
    async fn create_worktree(
        &self,
        slug: &str,
        base_branch: Option<&str>,
        copy_includes: &[PathBuf],
    ) -> Result<WorktreeHandle, WorktreeError> {
        if !self
            .hooks
            .has_hooks_for(&HookEventType::WorktreeCreate)
            .await
        {
            return self
                .inner
                .create_worktree(slug, base_branch, copy_includes)
                .await;
        }
        if !(self.workspace_trusted)() {
            return Err(WorktreeError::Hook(CONFIGURED_NO_CREATE.into()));
        }
        let context = (self.context)();
        let relative_base = if self.remote_original_base || context.cwd.as_os_str().is_empty() {
            self.original_cwd.clone()
        } else {
            context.cwd.clone()
        };
        let aggregate = self
            .hooks
            .execute(
                HookEvent::WorktreeCreate {
                    name: slug.to_owned(),
                    path: PathBuf::new(),
                    branch: String::new(),
                },
                context,
            )
            .await;
        let results = &aggregate.worktree_results;
        if let Some(selected) = results
            .iter()
            .filter(|result| result.succeeded && !self.held(result))
            .find_map(WorktreeHookResult::selected_path)
        {
            let path = absolute_path(&relative_base, &selected.display);
            screen_hook_path(&path, &self.original_cwd)?;
            self.inner.validate_hook_worktree(&path).await?;
            let handle = WorktreeHandle {
                path,
                branch_name: String::new(),
                base_commit: None,
            };
            self.remember(handle.clone());
            return Ok(handle);
        }
        if results.is_empty() {
            return Err(WorktreeError::Hook(CONFIGURED_NO_CREATE.into()));
        }
        let failures: Vec<_> = results
            .iter()
            .filter(|result| !result.succeeded)
            .map(|result| {
                let output = native_diagnostic(&result.output, "create")?;
                Ok(format!(
                    "{}: {}",
                    result.command.display,
                    if output.is_empty() {
                        "no output"
                    } else {
                        &output.display
                    },
                ))
            })
            .collect::<Result<Vec<_>, WorktreeError>>()?;
        if !failures.is_empty() {
            return Err(WorktreeError::Hook(format!(
                "WorktreeCreate hook failed: {}",
                failures.join("; ")
            )));
        }
        Err(WorktreeError::Hook(
            if results.iter().any(|result| self.held(result)) {
                PERSONAL_CREATE_PATH
            } else {
                NO_CREATE_PATH
            }
            .into(),
        ))
    }

    async fn remove_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError> {
        if !self.is_hook_based(handle) {
            return self.inner.remove_worktree(handle).await;
        }
        if !self
            .hooks
            .has_hooks_for(&HookEventType::WorktreeRemove)
            .await
        {
            return Err(WorktreeError::Hook(
                "No WorktreeRemove hook configured; kept hook-owned worktree".into(),
            ));
        }
        let aggregate = self
            .hooks
            .execute(
                HookEvent::WorktreeRemove {
                    path: handle.path.clone(),
                },
                (self.context)(),
            )
            .await;
        let results = &aggregate.worktree_results;
        if results.is_empty() {
            return Err(WorktreeError::Hook("WorktreeRemove hook is configured but did not run (workspace not trusted, or no matching hook entries)".into()));
        }
        if let Some(failed) = results.iter().find(|result| !result.succeeded) {
            // Native treats a failed hook as removed when nothing is left.
            if std::fs::symlink_metadata(&handle.path).is_ok() {
                let output = native_diagnostic(&failed.output, "remove")?;
                let detail = if output.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", output.display)
                };
                let summary = failed.exit_code.map_or_else(
                    || {
                        format!(
                            "failed{}",
                            if detail.is_empty() {
                                " with no output"
                            } else {
                                &detail
                            }
                        )
                    },
                    |code| format!("exited {code}{detail}"),
                );
                return Err(WorktreeError::Hook(format!(
                    "WorktreeRemove hook {summary}; kept worktree at: {}",
                    handle.path.display()
                )));
            }
        }
        if !(self.workspace_trusted)() && std::fs::symlink_metadata(&handle.path).is_ok() {
            return Err(WorktreeError::Hook("WorktreeRemove hook left the tree in place (workspace not trusted, so the folder's own hooks did not run)".into()));
        }
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle.path);
        Ok(())
    }

    fn is_hook_based(&self, handle: &WorktreeHandle) -> bool {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&handle.path)
            || self.inner.is_hook_based(handle)
    }

    fn restore_hook_worktree(&self, handle: &WorktreeHandle) {
        self.remember(handle.clone());
    }

    async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let mut all = self.inner.list_worktrees().await?;
        for (handle, created_at) in self
            .owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            if !all.iter().any(|entry| entry.path == handle.path) {
                all.push(WorktreeInfo {
                    path: handle.path.clone(),
                    branch: handle.branch_name.clone(),
                    created_at: *created_at,
                });
            }
        }
        Ok(all)
    }

    async fn cleanup_stale(&self, max_age: Duration) -> Result<Vec<PathBuf>, WorktreeError> {
        self.inner.cleanup_stale(max_age).await
    }
    fn is_supported(&self) -> bool {
        self.inner.is_supported()
            || self
                .hooks
                .may_have_hooks_for(&HookEventType::WorktreeCreate)
    }
    async fn worktree_change_summary(
        &self,
        handle: &WorktreeHandle,
    ) -> Result<Option<WorktreeChangeSummary>, WorktreeError> {
        self.inner.worktree_change_summary(handle).await
    }
    async fn validate_hook_worktree(&self, path: &Path) -> Result<(), WorktreeError> {
        screen_hook_path(path, &self.original_cwd)?;
        self.inner.validate_hook_worktree(path).await
    }
    async fn enter_existing(&self, path: &Path) -> Result<WorktreeHandle, WorktreeError> {
        self.inner.enter_existing(path).await
    }
    async fn snapshot_base(&self, limits: SnapshotLimits) -> Result<WorkspaceBase, WorktreeError> {
        self.inner.snapshot_base(limits).await
    }
    async fn worktree_patch(
        &self,
        handle: &WorktreeHandle,
        base: &str,
    ) -> Result<WorktreePatch, WorktreeError> {
        self.inner.worktree_patch(handle, base).await
    }
    async fn discard_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError> {
        if self.is_hook_based(handle) {
            self.remove_worktree(handle).await
        } else {
            self.inner.discard_worktree(handle).await
        }
    }
}

#[cfg(test)]
mod tests;
