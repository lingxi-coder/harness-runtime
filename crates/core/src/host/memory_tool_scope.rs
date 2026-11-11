//! Host-only, exact-call permission scope for background memory workers.
//!
//! The memory worker validates tool kind, shell syntax and its memory-root
//! boundary before entering this scope. The ordinary invoker still evaluates
//! configured deny/ask rules and tool-owned safety checks. A JSON field, tool
//! name, query label or nested different call cannot mint this authority.

use crate::host::{rooted_fs, FsError};
use crate::types::utf16_json::Utf16JsonProjection;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

/// Actual execution owned by one background memory pass. The outer worker
/// holds a lease before scheduling; pool runners and blocking inventory jobs
/// acquire their own leases before they can outlive that worker.
#[derive(Default)]
pub struct MemoryForkSettlement {
    outstanding: AtomicUsize,
    changed: tokio::sync::Notify,
}

impl MemoryForkSettlement {
    pub fn enter(self: &Arc<Self>) -> MemoryForkLease {
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        MemoryForkLease(self.clone())
    }

    pub fn is_settled(&self) -> bool {
        self.outstanding.load(Ordering::Acquire) == 0
    }

    pub async fn wait(&self) {
        self.wait_for_at_most(0).await;
    }

    /// Called only by the outer worker while it still owns its own lease.
    pub async fn wait_for_children(&self) {
        self.wait_for_at_most(1).await;
    }

    async fn wait_for_at_most(&self, remaining: usize) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.outstanding.load(Ordering::Acquire) <= remaining {
                return;
            }
            changed.await;
        }
    }
}

pub struct MemoryForkLease(Arc<MemoryForkSettlement>);

impl Drop for MemoryForkLease {
    fn drop(&mut self) {
        self.0.outstanding.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

tokio::task_local! {
    static MEMORY_FORK_SETTLEMENT: Arc<MemoryForkSettlement>;
}

/// Bind settlement ownership explicitly; spawned work must retain a lease.
pub async fn with_memory_settlement<F: Future>(
    settlement: Arc<MemoryForkSettlement>,
    future: F,
) -> F::Output {
    MEMORY_FORK_SETTLEMENT.scope(settlement, future).await
}

pub fn current_memory_settlement() -> Option<Arc<MemoryForkSettlement>> {
    MEMORY_FORK_SETTLEMENT.try_with(Arc::clone).ok()
}

/// An accepted blocking effect keeps running when its async waiter is aborted.
/// Retain its own execution lease inside the blocking closure, until it exits.
pub async fn run_memory_blocking<F, T>(work: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let lease = current_memory_settlement().map(|owner| owner.enter());
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        work()
    })
    .await
}

/// Worker-owned root identity, retained across all calls in the pass.
#[derive(Clone, Debug)]
pub struct MemoryMutationRoot {
    directory: PathBuf,
    identity: rooted_fs::RootIdentity,
    targets: BTreeMap<PathBuf, MemoryMutationTarget>,
}

#[derive(Clone, Debug)]
struct MemoryMutationTarget {
    relative: PathBuf,
    parent: PathBuf,
    parent_identity: rooted_fs::RootIdentity,
    missing_parents: Vec<std::ffi::OsString>,
    file_identity: rooted_fs::FileIdentity,
}

impl MemoryMutationRoot {
    pub fn capture(directory: &Path) -> Result<Self, FsError> {
        let directory =
            std::fs::canonicalize(directory).map_err(|error| FsError::Io(error.to_string()))?;
        let identity = rooted_fs::root_identity(&directory)?;
        Ok(Self {
            directory,
            identity,
            targets: BTreeMap::new(),
        })
    }

    pub fn verify(&self) -> Result<(), FsError> {
        if rooted_fs::root_identity(&self.directory)? != self.identity {
            return Err(mutation_denied());
        }
        Ok(())
    }

    fn relative_target(&self, target: &Path) -> Result<PathBuf, FsError> {
        let relative = target
            .strip_prefix(&self.directory)
            .map_err(|_| mutation_denied())?;
        if relative.as_os_str().is_empty()
            || !relative.to_str().is_some_and(|path| path.ends_with(".md"))
            || relative.components().any(|part| match part {
                Component::Normal(name) => {
                    memory_protected_component(name) || name.to_string_lossy().contains(':')
                }
                _ => true,
            })
        {
            return Err(mutation_denied());
        }
        Ok(relative.to_owned())
    }

    /// Finish validation once, before permission evaluation. Existing parent
    /// and leaf identities are retained; absent parents may only be created.
    pub fn admit_targets(&mut self, paths: &[PathBuf]) -> Result<(), FsError> {
        self.verify()?;
        for path in paths {
            let relative = self.relative_target(path)?;
            let mut parent = path.parent().ok_or_else(mutation_denied)?.to_owned();
            let mut missing_parents = Vec::new();
            loop {
                match std::fs::symlink_metadata(&parent) {
                    Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => break,
                    Ok(_) => return Err(mutation_denied()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing_parents
                            .push(parent.file_name().ok_or_else(mutation_denied)?.to_owned());
                        parent = parent.parent().ok_or_else(mutation_denied)?.to_owned();
                    }
                    Err(error) => return Err(FsError::Io(error.to_string())),
                }
            }
            if !parent.starts_with(&self.directory) {
                return Err(mutation_denied());
            }
            let parent_identity = rooted_fs::root_identity(&parent)?;
            missing_parents.reverse();
            let file_identity = if missing_parents.is_empty() {
                rooted_fs::file_identity_pinned(
                    &parent,
                    Path::new(path.file_name().ok_or_else(mutation_denied)?),
                    &parent_identity,
                )?
            } else {
                rooted_fs::FileIdentity::Missing
            };
            self.verify()?;
            self.targets.insert(
                path.clone(),
                MemoryMutationTarget {
                    relative,
                    parent,
                    parent_identity,
                    missing_parents,
                    file_identity,
                },
            );
        }
        Ok(())
    }
}

fn mutation_denied() -> FsError {
    FsError::OutsideWorkspace("the memory mutation scope changed or is no longer active".into())
}

/// Native protected names, shared by admission and final relative targets.
pub fn memory_protected_component(value: &std::ffi::OsStr) -> bool {
    let folded = value
        .to_string_lossy()
        .to_lowercase()
        .replace('ı', "i")
        .replace('ſ', "s");
    let folded = folded.chars().filter(|ch| !matches!(*ch, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')).collect::<String>();
    let folded = folded
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ' ']);
    folded == branding::DOT_DIR
        || matches!(
            folded,
            ".git"
                | "hooks"
                | ".husky"
                | ".githooks"
                | "node_modules"
                | ".vscode"
                | ".idea"
                | "head"
                | "config"
                | "objects"
                | "refs"
                | ".claude"
                | "skills"
                | "commands"
                | "agents"
                | ".cargo"
                | ".devcontainer"
                | ".yarn"
                | ".mvn"
                | ".consolidate-lock"
        )
}

/// Host-only authority for exact targets admitted before permission awaits.
#[derive(Clone)]
pub struct MemoryMutation {
    root: MemoryMutationRoot,
    targets: Vec<MemoryMutationTarget>,
    ignore_missing: bool,
    active: Arc<AtomicBool>,
}

impl MemoryMutation {
    pub fn directory(&self) -> &Path {
        &self.root.directory
    }
    pub fn target(&self) -> PathBuf {
        self.root.directory.join(&self.targets[0].relative)
    }
    fn check_active(&self) -> Result<(), FsError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(mutation_denied());
        }
        self.root.verify()
    }

    fn parent(
        &self,
        target: &MemoryMutationTarget,
        create: bool,
    ) -> Result<(PathBuf, rooted_fs::RootIdentity), FsError> {
        self.check_active()?;
        if !create && !target.missing_parents.is_empty() {
            return Err(FsError::NotFound(target.relative.display().to_string()));
        }
        let mut parent = target.parent.clone();
        let mut pin = target.parent_identity;
        for component in &target.missing_parents {
            self.check_active()?;
            let opened =
                rooted_fs::create_directory_new_pinned(&parent, Path::new(component), 0o777, &pin)?;
            pin = rooted_fs::opened_file_identity(&opened)?;
            parent.push(component);
        }
        Ok((parent, pin))
    }

    pub fn read_file(&self) -> Result<rooted_fs::RootedFileSnapshot, rooted_fs::RootedFsError> {
        let target = &self.targets[0];
        let (parent, pin) = self
            .parent(target, false)
            .map_err(rooted_fs::RootedFsError::Fs)?;
        let path = self.target();
        rooted_fs::read_file_after_permission_pinned(
            &parent,
            Path::new(target.relative.file_name().expect("admitted leaf")),
            &path,
            &path,
            &rooted_fs::FileMutationIdentity {
                root: pin,
                file: target.file_identity,
            },
        )
    }

    pub fn write_file(
        &self,
        bytes: &[u8],
    ) -> Result<rooted_fs::RootedWriteResult, rooted_fs::RootedFsError> {
        let target = &self.targets[0];
        let (parent, pin) = self
            .parent(target, true)
            .map_err(rooted_fs::RootedFsError::Fs)?;
        let path = self.target();
        self.check_active().map_err(rooted_fs::RootedFsError::Fs)?;
        rooted_fs::write_file_after_permission_pinned(
            &parent,
            Path::new(target.relative.file_name().expect("admitted leaf")),
            &path,
            &path,
            bytes,
            &rooted_fs::FileMutationIdentity {
                root: pin,
                file: target.file_identity,
            },
        )
    }

    pub fn write_file_checked(
        &self,
        bytes: &[u8],
        expectation: &rooted_fs::RootedWriteExpectation,
    ) -> Result<rooted_fs::RootedWriteResult, rooted_fs::RootedFsError> {
        let target = &self.targets[0];
        let (parent, pin) = self
            .parent(target, true)
            .map_err(rooted_fs::RootedFsError::Fs)?;
        let path = self.target();
        self.check_active().map_err(rooted_fs::RootedFsError::Fs)?;
        rooted_fs::write_file_after_permission_pinned_checked(
            &parent,
            Path::new(target.relative.file_name().expect("admitted leaf")),
            &path,
            &path,
            bytes,
            &rooted_fs::FileMutationIdentity {
                root: pin,
                file: target.file_identity,
            },
            expectation,
        )
    }

    pub fn remove_files(&self, is_cancelled: impl Fn() -> bool) -> Result<(), FsError> {
        for target in &self.targets {
            self.check_active()?;
            if is_cancelled() {
                return Err(mutation_denied());
            }
            if target.file_identity == rooted_fs::FileIdentity::Missing {
                if self.ignore_missing {
                    continue;
                }
                return Err(FsError::NotFound(target.relative.display().to_string()));
            }
            let (parent, pin) = self.parent(target, false)?;
            let leaf = Path::new(target.relative.file_name().expect("admitted leaf"));
            let current = rooted_fs::file_identity_pinned(&parent, leaf, &pin)?;
            if current == rooted_fs::FileIdentity::Missing && self.ignore_missing {
                continue;
            }
            if current != target.file_identity {
                return Err(mutation_denied());
            }
            if is_cancelled() {
                return Err(mutation_denied());
            }
            rooted_fs::remove_file_pinned(&parent, leaf, &pin)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct MemoryToolScope {
    name: String,
    input: Utf16JsonProjection,
    directory: PathBuf,
    mutation_root: Option<MemoryMutationRoot>,
    read_directories: Vec<PathBuf>,
    removal_paths: Vec<PathBuf>,
    ignore_missing_removals: bool,
    active: Arc<AtomicBool>,
}

tokio::task_local! {
    static MEMORY_TOOL_SCOPE: MemoryToolScope;
}

/// Run one already-validated memory-worker tool call under a scoped allowance.
pub async fn with_memory_tool_scope<F: Future>(
    name: String,
    input: Utf16JsonProjection,
    directory: PathBuf,
    active: Arc<AtomicBool>,
    future: F,
) -> F::Output {
    with_memory_tool_scope_and_read_roots(name, input, directory, Vec::new(), active, future).await
}

/// Admit additional host-owned transcript roots for read-only file tools.
/// Write/Edit retain only the validated memory directory.
pub async fn with_memory_tool_scope_and_read_roots<F: Future>(
    name: String,
    input: Utf16JsonProjection,
    directory: PathBuf,
    read_directories: Vec<PathBuf>,
    active: Arc<AtomicBool>,
    future: F,
) -> F::Output {
    with_memory_tool_scope_and_removals(
        name,
        input,
        directory,
        None,
        read_directories,
        Vec::new(),
        false,
        active,
        future,
    )
    .await
}

/// Carry already-validated literal removals to a host file operation. Mobile
/// shells execute in a guest whose mounts need not expose the memory store.
pub async fn with_memory_tool_scope_and_removals<F: Future>(
    name: String,
    input: Utf16JsonProjection,
    directory: PathBuf,
    mutation_root: Option<MemoryMutationRoot>,
    read_directories: Vec<PathBuf>,
    removal_paths: Vec<PathBuf>,
    ignore_missing_removals: bool,
    active: Arc<AtomicBool>,
    future: F,
) -> F::Output {
    MEMORY_TOOL_SCOPE
        .scope(
            MemoryToolScope {
                name,
                input,
                directory,
                mutation_root,
                read_directories,
                removal_paths,
                ignore_missing_removals,
                active,
            },
            future,
        )
        .await
}

/// Resolve a file mutation without confusing a revoked scope with an
/// ordinary foreground call. Normalization must preserve its exact target.
pub fn memory_file_mutation(
    name: &str,
    input: &Utf16JsonProjection,
) -> Result<Option<MemoryMutation>, FsError> {
    MEMORY_TOOL_SCOPE
        .try_with(|scope| {
            if !matches!(name, "Write" | "Edit") || !scope_matches(scope, name, input) {
                return Err(mutation_denied());
            }
            let root = scope.mutation_root.clone().ok_or_else(mutation_denied)?;
            let target = input.string_at("/file_path").ok_or_else(mutation_denied)?;
            let target = root
                .targets
                .get(Path::new(target))
                .cloned()
                .ok_or_else(mutation_denied)?;
            Ok(Some(MemoryMutation {
                root,
                targets: vec![target],
                ignore_missing: false,
                active: scope.active.clone(),
            }))
        })
        .unwrap_or(Ok(None))
}

fn scope_matches(scope: &MemoryToolScope, name: &str, input: &Utf16JsonProjection) -> bool {
    scope.name == name && scope.input == *input && scope.active.load(Ordering::Acquire)
}

/// All advertised shell identities consume the same pre-permission host
/// authority for literal pruning. An invalid existing scope fails closed.
#[must_use]
pub fn memory_tool_scope_removals(
    name: &str,
    input: &Utf16JsonProjection,
) -> Result<Option<MemoryMutation>, FsError> {
    if !matches!(name, "Bash" | "PowerShell" | "Shell") {
        return Ok(None);
    }
    MEMORY_TOOL_SCOPE
        .try_with(|scope| {
            if !scope_matches(scope, name, input) {
                return Err(mutation_denied());
            }
            if scope.removal_paths.is_empty() {
                return Ok(None);
            }
            let root = scope.mutation_root.clone().ok_or_else(mutation_denied)?;
            let targets = scope
                .removal_paths
                .iter()
                .map(|path| root.targets.get(path).cloned().ok_or_else(mutation_denied))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(MemoryMutation {
                root,
                targets,
                ignore_missing: scope.ignore_missing_removals,
                active: scope.active.clone(),
            }))
        })
        .unwrap_or(Ok(None))
}

/// Bind trusted schema normalization to the exact tool body after permission
/// checks. Revocation, another call, or a changed target never follows it.
pub async fn with_normalized_memory_tool_scope<F: Future>(
    name: &str,
    original: &Utf16JsonProjection,
    normalized: Utf16JsonProjection,
    future: F,
) -> Option<F::Output> {
    let scope = MEMORY_TOOL_SCOPE
        .try_with(|scope| {
            (scope.active.load(Ordering::Acquire)
                && scope.name == name
                && scope.input == *original
                && ["/file_path", "/path", "/command"]
                    .into_iter()
                    .all(|field| original.string_units(field) == normalized.string_units(field)))
            .then(|| {
                let mut scope = scope.clone();
                scope.input = normalized;
                scope
            })
        })
        .ok()
        .flatten()?;
    Some(MEMORY_TOOL_SCOPE.scope(scope, future).await)
}

/// File-tool containment roots for this exact active call. They are never
/// added to the session's global trusted-directory set or inherited by a
/// different nested invocation.
#[must_use]
pub fn memory_tool_scope_file_roots(name: &str, input: &Utf16JsonProjection) -> Vec<PathBuf> {
    MEMORY_TOOL_SCOPE
        .try_with(|scope| {
            if !scope.active.load(Ordering::Acquire)
                || scope.name != name
                || scope.input != *input
                || !matches!(name, "Read" | "Grep" | "Glob" | "Write" | "Edit")
            {
                return Vec::new();
            }
            let mut roots = vec![scope.directory.clone()];
            if matches!(name, "Read" | "Grep" | "Glob") {
                roots.extend(scope.read_directories.iter().cloned());
            }
            roots
        })
        .unwrap_or_default()
}

/// Resolve the memory root only for the exact, still-active host-owned call.
#[must_use]
pub fn memory_tool_scope_directory(name: &str, input: &Utf16JsonProjection) -> Option<PathBuf> {
    MEMORY_TOOL_SCOPE
        .try_with(|scope| {
            (scope.active.load(Ordering::Acquire) && scope.name == name && scope.input == *input)
                .then(|| scope.directory.clone())
        })
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_settlement_joins_blocking_effect_after_async_waiter_is_aborted() {
        let owner = Arc::new(MemoryForkSettlement::default());
        let outer = owner.enter();
        let (finish, release) = std::sync::mpsc::channel();
        let started = Arc::new(tokio::sync::Notify::new());
        let entered = started.clone();
        let completed = Arc::new(AtomicBool::new(false));
        let effect_completed = completed.clone();
        let worker = tokio::spawn(with_memory_settlement(owner.clone(), async move {
            let _outer = outer;
            run_memory_blocking(move || {
                entered.notify_one();
                release.recv().unwrap();
                effect_completed.store(true, Ordering::Release);
            })
            .await
            .unwrap();
        }));
        started.notified().await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(!completed.load(Ordering::Acquire));
        let done = owner.wait();
        tokio::pin!(done);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(done.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        finish.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), done)
            .await
            .unwrap();
        assert!(completed.load(Ordering::Acquire));
        assert!(current_memory_settlement().is_none());
    }

    #[tokio::test]
    async fn memory_mutation_revocation_does_not_become_an_ordinary_file_call() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("topic.md");
        let input = Utf16JsonProjection::from_serde_value(&serde_json::json!({
            "file_path":target,"content":"memory"
        }))
        .unwrap();
        let active = Arc::new(AtomicBool::new(true));
        let mut pin = MemoryMutationRoot::capture(&root).unwrap();
        pin.admit_targets(&[target.clone()]).unwrap();
        assert!(memory_file_mutation("Write", &input).unwrap().is_none());
        with_memory_tool_scope_and_removals(
            "Write".into(),
            input.clone(),
            root,
            Some(pin),
            vec![],
            vec![],
            false,
            active.clone(),
            async {
                let admitted = memory_file_mutation("Write", &input).unwrap().unwrap();
                active.store(false, Ordering::Release);
                assert!(memory_file_mutation("Write", &input).is_err());
                assert!(admitted.write_file(b"must not write").is_err());
            },
        )
        .await;
        assert!(!target.exists());
        assert!(memory_file_mutation("Write", &input).unwrap().is_none());
    }

    #[tokio::test]
    async fn grant_is_exact_call_scoped_and_revocable() {
        let input = Utf16JsonProjection::parse(r#"{"file_path":"/memory/a.md"}"#).unwrap();
        let other = Utf16JsonProjection::parse(r#"{"file_path":"/outside/a.md"}"#).unwrap();
        let active = Arc::new(AtomicBool::new(true));
        assert!(memory_tool_scope_directory("Write", &input).is_none());
        with_memory_tool_scope(
            "Write".into(),
            input.clone(),
            "/memory".into(),
            active.clone(),
            async {
                assert_eq!(
                    memory_tool_scope_directory("Write", &input),
                    Some("/memory".into())
                );
                assert!(memory_tool_scope_directory("Edit", &input).is_none());
                assert!(memory_tool_scope_directory("Write", &other).is_none());
                active.store(false, Ordering::Release);
                assert!(memory_tool_scope_directory("Write", &input).is_none());
            },
        )
        .await;
        assert!(memory_tool_scope_directory("Write", &input).is_none());
    }

    #[tokio::test]
    async fn memory_file_roots_follow_only_exact_normalized_calls_and_read_only_transcripts() {
        let original =
            Utf16JsonProjection::parse(r#"{"file_path":"/memory/a.md","inert":true}"#).unwrap();
        let normalized = Utf16JsonProjection::parse(r#"{"file_path":"/memory/a.md"}"#).unwrap();
        let changed =
            Utf16JsonProjection::parse(r#"{"file_path":"/transcripts/session.jsonl"}"#).unwrap();
        let active = Arc::new(AtomicBool::new(true));
        with_memory_tool_scope_and_read_roots(
            "Read".into(),
            original.clone(),
            "/memory".into(),
            vec!["/transcripts".into()],
            active.clone(),
            async {
                assert!(memory_tool_scope_file_roots("Read", &normalized).is_empty());
                let result = with_normalized_memory_tool_scope(
                    "Read",
                    &original,
                    normalized.clone(),
                    async {
                        assert_eq!(
                            memory_tool_scope_file_roots("Read", &normalized),
                            vec![PathBuf::from("/memory"), PathBuf::from("/transcripts")]
                        );
                        assert!(memory_tool_scope_file_roots("Write", &normalized).is_empty());
                        assert!(memory_tool_scope_file_roots("Read", &changed).is_empty());
                        tokio::spawn(async {
                            assert!(MEMORY_TOOL_SCOPE.try_with(|_| ()).is_err());
                        })
                        .await
                        .unwrap();
                    },
                )
                .await;
                assert_eq!(result, Some(()));
                assert!(with_normalized_memory_tool_scope(
                    "Read",
                    &original,
                    changed.clone(),
                    async { panic!("a rewritten memory target must never execute") }
                )
                .await
                .is_none());
                active.store(false, Ordering::Release);
                assert!(memory_tool_scope_file_roots("Read", &original).is_empty());
            },
        )
        .await;
        assert!(memory_tool_scope_file_roots("Read", &original).is_empty());
        with_memory_tool_scope_and_read_roots(
            "Write".into(),
            normalized.clone(),
            "/memory".into(),
            vec!["/transcripts".into()],
            Arc::new(AtomicBool::new(true)),
            async {
                assert_eq!(
                    memory_tool_scope_file_roots("Write", &normalized),
                    vec![PathBuf::from("/memory")]
                );
            },
        )
        .await;
    }
}
