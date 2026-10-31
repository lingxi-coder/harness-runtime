//! Per-run, integrity-checked Local App Skill scopes.
//!
//! A scope is published only after every selected release file is verified.
//! Its host-owned handle must outlive every model/tool/child that can use it.
use lingxi_core::host::rooted_fs::{self, AtomicWriteOptions, RootIdentity};
use local_app::v2::{AgentSkill, ReleaseFile};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};
use tokio_util::sync::CancellationToken;

const MAX_SKILLS: usize = 128;
const MAX_SKILL_FILES: usize = 256;
const MAX_SKILL_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SKILL_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_INSTRUCTION_BYTES: usize = 128 * 1024;
const MAX_UNCLEANED_COPY_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug)]
struct ScopeBudget {
    used: AtomicUsize,
    limit: usize,
}
impl ScopeBudget {
    fn reserve(&self, bytes: usize) -> Result<(), String> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| "skill_scope_limit_exceeded".into())
    }
    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}
fn scope_budget() -> Arc<ScopeBudget> {
    static BUDGET: OnceLock<Arc<ScopeBudget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| {
            Arc::new(ScopeBudget {
                used: AtomicUsize::new(0),
                limit: MAX_UNCLEANED_COPY_BYTES,
            })
        })
        .clone()
}

/// Verified instructions plus their host-owned, ephemeral filesystem scope.
#[derive(Debug)]
#[must_use = "retain the scope until tools and child processes have drained"]
pub struct MaterializedSkills {
    instructions: String,
    scope: SkillScope,
}

/// A materialization failure may still own a scope whose local cleanup failed.
/// Native callers must retain that handle in the run's cleanup fence before
/// surfacing the code. Converting this error to a string would lose custody.
#[derive(Debug)]
#[must_use = "retain any uncleaned scope in the native cleanup fence"]
pub struct MaterializeError {
    code: String,
    scope: Option<Box<SkillScope>>,
}
impl MaterializeError {
    /// Split the safe error code from any still-owned scope.
    pub fn into_parts(self) -> (String, Option<SkillScope>) {
        (self.code, self.scope.map(|scope| *scope))
    }
}
impl From<String> for MaterializeError {
    fn from(code: String) -> Self {
        Self { code, scope: None }
    }
}
impl From<&str> for MaterializeError {
    fn from(code: &str) -> Self {
        Self::from(code.to_owned())
    }
}
impl std::fmt::Display for MaterializeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.code)
    }
}
impl std::error::Error for MaterializeError {}
impl MaterializedSkills {
    /// Instructions referencing this run's fresh scope, never an earlier tree.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }
    /// Keep the returned scope in the native run's cleanup fence.
    pub fn into_parts(self) -> (String, SkillScope) {
        (self.instructions, self.scope)
    }
}

/// Native ownership of one exclusively created directory. This is deliberately
/// neither serializable nor cloneable. Drop closes the OS handle but NEVER
/// deletes files or releases the uncleaned-copy reservation.
///
/// The budget bounds copies retained by this process, not runtime output or
/// total disk use. Crash recovery/old-run ownership belongs to the host's task
/// record and cleanup fence; this module never scans or deletes old scopes.
#[derive(Debug)]
#[must_use = "explicitly clean only after every tool/child has drained"]
pub struct SkillScope {
    workspace: PathBuf,
    relative: PathBuf,
    path: PathBuf,
    workspace_identity: RootIdentity,
    directory: Option<fs::File>,
    copied_bytes: usize,
    budget: Arc<ScopeBudget>,
}
impl SkillScope {
    /// Absolute scope path inside the host-owned task or Chat workspace.
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Release file bytes reserved against the process's 128 MiB copy budget.
    pub fn copied_bytes(&self) -> usize {
        self.copied_bytes
    }
    /// Explicit host-only lifecycle transition. The caller MUST have confirmed
    /// all tools and subprocesses drained, including failure/cancellation paths.
    /// A timeout, final model message or revoked lease alone is not that proof.
    /// Await completion; failures retain this handle and its reservation.
    pub async fn cleanup_after_tools_drained(&mut self) -> Result<(), String> {
        let Some(directory) = self.directory.as_ref() else {
            return Ok(());
        };
        let directory = directory
            .try_clone()
            .map_err(|_| "skill_cleanup_incomplete")?;
        let workspace = self.workspace.clone();
        let relative = self.relative.clone();
        let identity = self.workspace_identity;
        tokio::task::spawn_blocking(move || {
            rooted_fs::remove_tree_pinned(&workspace, &relative, &identity, &directory)
        })
        .await
        .map_err(|_| "skill_cleanup_incomplete")?
        .map_err(|_| "skill_cleanup_incomplete")?;
        self.directory.take();
        self.budget.release(self.copied_bytes);
        Ok(())
    }
    fn cleanup_unpublished(&mut self) -> Result<(), String> {
        if let Some(directory) = self.directory.as_ref() {
            rooted_fs::remove_tree_pinned(
                &self.workspace,
                &self.relative,
                &self.workspace_identity,
                directory,
            )
            .map_err(|_| "skill_cleanup_incomplete")?;
            self.directory.take();
            self.budget.release(self.copied_bytes);
        }
        Ok(())
    }
}

struct VerifiedSkill {
    id: String,
    instruction: String,
}
struct VerifiedFile {
    path: String,
    bytes: Vec<u8>,
    executable: bool,
}
struct MaterializeControl<F> {
    cancellation: CancellationToken,
    budget: Arc<ScopeBudget>,
    after_write: F,
}
fn check_cancel(cancellation: &CancellationToken) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("cancelled".into())
    } else {
        Ok(())
    }
}

/// Materialize selected release files in a fresh, run-exclusive directory under
/// a host-owned task or Chat workspace. The caller first intersects IDs with the release
/// and native grants, and must retain the returned scope through tool drain.
/// No old tree, receipt, lock or Python-cache special case is used.
pub async fn materialize(
    source_root: &Path,
    workspace: &Path,
    declared: &[AgentSkill],
    release_files: &[ReleaseFile],
    selected: &[String],
    cancellation: CancellationToken,
) -> Result<Option<MaterializedSkills>, MaterializeError> {
    if selected.is_empty() {
        return Ok(None);
    }
    if selected.len() > MAX_SKILLS || !workspace.is_absolute() || !source_root.is_absolute() {
        return Err("skill_unavailable".into());
    }
    let source_root = source_root.to_path_buf();
    let workspace = workspace.to_path_buf();
    let declared = declared.to_vec();
    let release_files = release_files.to_vec();
    let selected = selected.to_vec();
    let cancellation = cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        materialize_sync(
            &source_root,
            &workspace,
            &declared,
            &release_files,
            &selected,
            &mut MaterializeControl {
                cancellation,
                budget: scope_budget(),
                after_write: |_| Ok(()),
            },
        )
    })
    .await
    .map_err(|_| "skill_unavailable")?
}

fn materialize_sync(
    source_root: &Path,
    workspace: &Path,
    declared: &[AgentSkill],
    release_files: &[ReleaseFile],
    selected: &[String],
    control: &mut MaterializeControl<impl FnMut(usize) -> Result<(), String>>,
) -> Result<Option<MaterializedSkills>, MaterializeError> {
    check_cancel(&control.cancellation)?;
    // Pin the supplied roots before canonicalization, so arbitrary root links
    // cannot silently redirect either reads or scope creation.
    let source_identity = rooted_fs::root_identity(source_root).map_err(|_| "skill_unavailable")?;
    let workspace_identity =
        rooted_fs::root_identity(workspace).map_err(|_| "task_storage_failed")?;
    let source = fs::canonicalize(source_root).map_err(|_| "skill_unavailable")?;
    let workspace = fs::canonicalize(workspace).map_err(|_| "task_storage_failed")?;
    let mut unique = HashSet::new();
    if selected
        .iter()
        .any(|id| !valid_skill_id(id) || !unique.insert(id))
    {
        return Err("skill_unavailable".into());
    }
    let mut skills = Vec::new();
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    for id in selected {
        let prefix = format!("skills/{id}/");
        let instruction_path = format!("{prefix}SKILL.md");
        if !declared
            .iter()
            .any(|skill| skill.id == *id && skill.path == instruction_path)
        {
            return Err("skill_not_declared".into());
        }
        let mut selected_files = release_files
            .iter()
            .filter(|file| file.path.starts_with(&prefix))
            .collect::<Vec<_>>();
        selected_files.sort_by(|left, right| left.path.cmp(&right.path));
        if selected_files.is_empty() || files.len() + selected_files.len() > MAX_SKILL_FILES {
            return Err("skill_unavailable".into());
        }
        let mut seen = HashSet::new();
        let mut instruction = None;
        for file in selected_files {
            check_cancel(&control.cancellation)?;
            if !valid_relative_path(&file.path)
                || !seen.insert(&file.path)
                || file.bytes > MAX_SKILL_FILE_BYTES as u64
            {
                return Err("skill_unavailable".into());
            }
            let bytes = read_file(
                &source,
                &source_identity,
                &file.path,
                MAX_SKILL_FILE_BYTES as u64,
            )?
            .0;
            if bytes.len() as u64 != file.bytes
                || format!("{:x}", Sha256::digest(&bytes)) != file.sha256
            {
                return Err("skill_integrity_failed".into());
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > MAX_SKILL_TOTAL_BYTES {
                return Err("skill_unavailable".into());
            }
            if file.path == instruction_path {
                if bytes.len() > MAX_INSTRUCTION_BYTES {
                    return Err("skill_unavailable".into());
                }
                instruction =
                    Some(String::from_utf8(bytes.clone()).map_err(|_| "skill_unavailable")?);
            }
            files.push(VerifiedFile {
                path: file.path.clone(),
                bytes,
                executable: file.executable,
            });
        }
        skills.push(VerifiedSkill {
            id: id.clone(),
            instruction: instruction.ok_or("skill_unavailable")?,
        });
    }
    check_cancel(&control.cancellation)?;
    control.budget.reserve(total_bytes)?;
    let relative = PathBuf::from(format!(
        ".local-app-skills-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let directory = match rooted_fs::create_directory_new_pinned(
        &workspace,
        &relative,
        0o700,
        &workspace_identity,
    ) {
        Ok(directory) => directory,
        Err(_) => {
            control.budget.release(total_bytes);
            return Err("skill_unavailable".into());
        }
    };
    let mut scope = SkillScope {
        path: workspace.join(&relative),
        workspace,
        relative,
        workspace_identity,
        directory: Some(directory),
        copied_bytes: total_bytes,
        budget: control.budget.clone(),
    };
    let installed: Result<(), String> = (|| {
        let identity =
            rooted_fs::opened_file_identity(scope.directory.as_ref().ok_or("skill_unavailable")?)
                .map_err(|_| "skill_unavailable")?;
        for (index, file) in files.iter().enumerate() {
            check_cancel(&control.cancellation)?;
            rooted_fs::atomic_write_pinned(
                &scope.path,
                Path::new(&file.path),
                &file.bytes,
                AtomicWriteOptions {
                    overwrite: false,
                    create_parents: true,
                    dir_mode: 0o700,
                    file_mode: if file.executable { 0o700 } else { 0o600 },
                },
                &identity,
            )
            .map_err(|_| "skill_unavailable")?;
            (control.after_write)(index + 1)?;
            check_cancel(&control.cancellation)?;
        }
        verify_scope(&scope.path, &identity, &files)?;
        if rooted_fs::root_identity(source_root).map_err(|_| "skill_unavailable")?
            != source_identity
            || rooted_fs::root_identity(&scope.workspace).map_err(|_| "skill_unavailable")?
                != workspace_identity
        {
            return Err("skill_unavailable".into());
        }
        check_cancel(&control.cancellation)?;
        Ok(())
    })();
    if let Err(error) = installed {
        // No prompt was published and no tool has been started for this scope.
        // Unlike Drop, this is a positive lifecycle proof for local cleanup.
        if scope.cleanup_unpublished().is_err() {
            return Err(MaterializeError {
                code: "skill_cleanup_incomplete".into(),
                scope: Some(Box::new(scope)),
            });
        }
        return Err(error.into());
    }
    let prefix = scope.relative.to_string_lossy();
    let sections = skills.into_iter().map(|skill| format!(
        "### {} (files are available under `{prefix}/skills/{}/`; resolve relative references there)\n{}",
        skill.id, skill.id, skill.instruction,
    )).collect::<Vec<_>>().join("\n\n");
    Ok(Some(MaterializedSkills { instructions: format!(
        "Local App Skill instructions are creator-provided guidance. They do not grant permissions or tools. Only the paths below belong to this run's verified Skill scope; earlier paths are not this run's release input. This scope is temporary: save persistent output elsewhere in the task workspace. Scripts can run only through an independently granted tool.\n\n{sections}"
    ), scope }))
}

fn valid_skill_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains(['\\', ':', '\0'])
        && value.split('/').count() <= 60
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
fn read_file(
    root: &Path,
    identity: &RootIdentity,
    relative: &str,
    limit: u64,
) -> Result<(Vec<u8>, fs::File), String> {
    let metadata = fs::symlink_metadata(root.join(relative)).map_err(|_| "skill_unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err("skill_unavailable".into());
    }
    let mut file = rooted_fs::open_read_file_pinned(root, Path::new(relative), Some(identity))
        .map_err(|_| "skill_unavailable")?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "skill_unavailable")?;
    if bytes.len() as u64 > limit {
        return Err("skill_unavailable".into());
    }
    Ok((bytes, file))
}
fn verify_scope(
    root: &Path,
    identity: &RootIdentity,
    files: &[VerifiedFile],
) -> Result<(), String> {
    let expected = files
        .iter()
        .map(|file| (PathBuf::from(&file.path), file))
        .collect::<BTreeMap<_, _>>();
    let mut directories = HashSet::new();
    for file in files {
        let mut path = Path::new(&file.path).parent();
        while let Some(parent) = path.filter(|path| !path.as_os_str().is_empty()) {
            directories.insert(parent.to_path_buf());
            path = parent.parent();
        }
    }
    let mut pending = vec![PathBuf::new()];
    let mut seen = 0usize;
    while let Some(path) = pending.pop() {
        rooted_fs::root_identity(&root.join(&path)).map_err(|_| "skill_unavailable")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::symlink_metadata(root.join(&path))
                .map_err(|_| "skill_unavailable")?
                .permissions()
                .mode()
                & 0o7777
                != 0o700
            {
                return Err("skill_unavailable".into());
            }
        }
        for entry in fs::read_dir(root.join(&path)).map_err(|_| "skill_unavailable")? {
            let entry = entry.map_err(|_| "skill_unavailable")?;
            let path = path.join(entry.file_name());
            if directories.contains(&path) {
                pending.push(path);
                continue;
            }
            let file = expected.get(&path).ok_or("skill_unavailable")?;
            let (bytes, opened) = read_file(root, identity, &file.path, file.bytes.len() as u64)?;
            if bytes != file.bytes {
                return Err("skill_integrity_failed".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let metadata = opened.metadata().map_err(|_| "skill_unavailable")?;
                if metadata.nlink() != 1
                    || metadata.permissions().mode() & 0o7777
                        != if file.executable { 0o700 } else { 0o600 }
                {
                    return Err("skill_unavailable".into());
                }
            }
            #[cfg(windows)]
            if rooted_fs::file_link_count(&opened).map_err(|_| "skill_unavailable")? != 1 {
                return Err("skill_unavailable".into());
            }
            seen += 1;
        }
    }
    if seen != files.len()
        || rooted_fs::root_identity(root).map_err(|_| "skill_unavailable")? != *identity
    {
        return Err("skill_unavailable".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Fixture {
        source: tempfile::TempDir,
        workspace: tempfile::TempDir,
        declared: Vec<AgentSkill>,
        files: Vec<ReleaseFile>,
    }
    impl Fixture {
        fn new() -> Self {
            let source = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let mut files = Vec::new();
            for (path, bytes, executable) in [
                ("skills/a/SKILL.md", "Run scripts/check.py", false),
                (
                    "skills/a/scripts/check.py",
                    "import helper\nprint(helper.VALUE)\n",
                    true,
                ),
                ("skills/a/scripts/helper.py", "VALUE = 'release'\n", false),
                ("skills/b/SKILL.md", "Unselected", false),
            ] {
                fs::create_dir_all(source.path().join(path).parent().unwrap()).unwrap();
                fs::write(source.path().join(path), bytes).unwrap();
                files.push(ReleaseFile {
                    path: path.into(),
                    sha256: format!("{:x}", Sha256::digest(bytes.as_bytes())),
                    bytes: bytes.len() as u64,
                    executable,
                });
            }
            Self {
                source,
                workspace,
                files,
                declared: vec![
                    AgentSkill {
                        id: "a".into(),
                        path: "skills/a/SKILL.md".into(),
                    },
                    AgentSkill {
                        id: "b".into(),
                        path: "skills/b/SKILL.md".into(),
                    },
                ],
            }
        }
        fn budget(&self) -> Arc<ScopeBudget> {
            Arc::new(ScopeBudget {
                used: AtomicUsize::new(0),
                limit: MAX_UNCLEANED_COPY_BYTES,
            })
        }
        fn run(
            &self,
            budget: Arc<ScopeBudget>,
            cancellation: CancellationToken,
            after_write: impl FnMut(usize) -> Result<(), String>,
        ) -> Result<MaterializedSkills, MaterializeError> {
            materialize_sync(
                self.source.path(),
                self.workspace.path(),
                &self.declared,
                &self.files,
                &["a".into()],
                &mut MaterializeControl {
                    budget,
                    cancellation,
                    after_write,
                },
            )
            .map(|result| result.unwrap())
        }
        fn normal(&self, budget: Arc<ScopeBudget>) -> MaterializedSkills {
            self.run(budget, CancellationToken::new(), |_| Ok(()))
                .unwrap()
        }
    }
    fn python(scope: &SkillScope) -> String {
        let output = Command::new("python3")
            // Apple's Python has a built-in global cache prefix. Keep this
            // fixture's genuine bytecode cache inside its own temporary scope.
            .args(["-X", "pycache_prefix="])
            .arg(scope.path().join("skills/a/scripts/check.py"))
            .env_remove("PYTHONPYCACHEPREFIX")
            .env_remove("PYTHONDONTWRITEBYTECODE")
            .output()
            .expect("python3 is required for the Python Skill regression test");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[tokio::test]
    async fn python_import_cache_and_forged_pyc_do_not_enter_later_scopes() {
        let fixture = Fixture::new();
        let budget = fixture.budget();
        let (prompt, mut first) = fixture.normal(budget.clone()).into_parts();
        assert!(prompt.contains(first.relative.to_str().unwrap()));
        assert!(!first.path().join("skills/b").exists());
        assert_eq!(python(&first), "release\n");
        let script = first.path().join("skills/a/scripts/helper.py");
        let cache = script.parent().unwrap().join("__pycache__");
        assert!(
            cache.is_dir(),
            "real Python imports must retain their normal cache semantics"
        );
        let poison = Command::new("python3").args(["-X", "pycache_prefix="]).arg("-c").arg(
            "import importlib.util,marshal,pathlib,struct,sys; p=pathlib.Path(sys.argv[1]); s=p.stat(); c=pathlib.Path(importlib.util.cache_from_source(str(p))); c.write_bytes(importlib.util.MAGIC_NUMBER+struct.pack('<III',0,int(s.st_mtime),s.st_size)+marshal.dumps(compile(\"VALUE='poison'\\n\",str(p),'exec')))"
        ).arg(&script).status().unwrap();
        assert!(poison.success());
        assert_eq!(
            python(&first),
            "poison\n",
            "the fixture must actually exercise malicious cached bytecode"
        );
        fs::write(
            first.path().join("skills/a/scripts/undeclared.py"),
            "poison",
        )
        .unwrap();
        let (_, mut second) = fixture.normal(budget.clone()).into_parts();
        assert_ne!(first.path(), second.path());
        assert!(!second.path().join("skills/a/scripts/__pycache__").exists());
        assert!(!second
            .path()
            .join("skills/a/scripts/undeclared.py")
            .exists());
        assert_eq!(python(&second), "release\n");
        first.cleanup_after_tools_drained().await.unwrap();
        second.cleanup_after_tools_drained().await.unwrap();
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn interrupted_publication_and_cancellation_clean_only_the_unpublished_scope() {
        let fixture = Fixture::new();
        let budget = fixture.budget();
        let existing = fixture.workspace.path().join("skills/a");
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join("SKILL.md"), "user data").unwrap();
        for cancel in [false, true] {
            let token = CancellationToken::new();
            let worker = token.clone();
            let result = fixture.run(budget.clone(), worker, |count| {
                assert_eq!(count, 1);
                if cancel {
                    token.cancel();
                    Ok(())
                } else {
                    Err("injected_interruption".into())
                }
            });
            assert_eq!(
                result.unwrap_err().code,
                if cancel {
                    "cancelled"
                } else {
                    "injected_interruption"
                }
            );
            assert_eq!(budget.used.load(Ordering::Acquire), 0);
            assert_eq!(fs::read_dir(fixture.workspace.path()).unwrap().count(), 1);
        }
        let (_, mut retry) = fixture.normal(budget.clone()).into_parts();
        assert_eq!(python(&retry), "release\n");
        retry.cleanup_after_tools_drained().await.unwrap();
        assert_eq!(fs::read(existing.join("SKILL.md")).unwrap(), b"user data");
    }

    #[tokio::test]
    async fn concurrent_calls_are_isolated_and_have_no_lock_wait() {
        let fixture = Fixture::new();
        let selected = ["a".into()];
        let owner_cancellation = CancellationToken::new();
        let call = || {
            materialize(
                fixture.source.path(),
                fixture.workspace.path(),
                &fixture.declared,
                &fixture.files,
                &selected,
                owner_cancellation.clone(),
            )
        };
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(call(), call())
        })
        .await
        .unwrap();
        let (_, mut first) = first.unwrap().unwrap().into_parts();
        let (_, mut second) = second.unwrap().unwrap().into_parts();
        assert_ne!(first.path(), second.path());
        assert!(
            !owner_cancellation.is_cancelled(),
            "scope cancellation must not cancel its owner"
        );
        first.cleanup_after_tools_drained().await.unwrap();
        second.cleanup_after_tools_drained().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_releases_the_budget_but_drop_and_failed_cleanup_do_not() {
        let fixture = Fixture::new();
        let bytes = fixture
            .files
            .iter()
            .filter(|file| file.path.starts_with("skills/a/"))
            .map(|file| file.bytes as usize)
            .sum();
        let budget = Arc::new(ScopeBudget {
            used: AtomicUsize::new(0),
            limit: bytes,
        });
        for _ in 0..3 {
            let (_, mut scope) = fixture.normal(budget.clone()).into_parts();
            assert_eq!(budget.used.load(Ordering::Acquire), bytes);
            assert_eq!(
                fixture
                    .run(budget.clone(), CancellationToken::new(), |_| Ok(()))
                    .unwrap_err()
                    .code,
                "skill_scope_limit_exceeded"
            );
            scope.cleanup_after_tools_drained().await.unwrap();
            scope.cleanup_after_tools_drained().await.unwrap();
            assert_eq!(budget.used.load(Ordering::Acquire), 0);
        }
        let (_, scope) = fixture.normal(budget.clone()).into_parts();
        // A retained native handle models the host cleanup fence. There is no
        // public Clone/serialization path for recreating ownership from disk.
        let mut fence = SkillScope {
            workspace: scope.workspace.clone(),
            relative: scope.relative.clone(),
            path: scope.path.clone(),
            workspace_identity: scope.workspace_identity,
            directory: Some(scope.directory.as_ref().unwrap().try_clone().unwrap()),
            copied_bytes: scope.copied_bytes,
            budget: budget.clone(),
        };
        drop(scope);
        assert!(fence.path().exists());
        assert_eq!(budget.used.load(Ordering::Acquire), bytes);
        let moved = fixture.workspace.path().join("retained");
        fs::rename(fence.path(), &moved).unwrap();
        fs::create_dir(fence.path()).unwrap();
        fs::write(fence.path().join("user.txt"), "preserve").unwrap();
        assert_eq!(
            fence.cleanup_after_tools_drained().await.unwrap_err(),
            "skill_cleanup_incomplete"
        );
        assert_eq!(budget.used.load(Ordering::Acquire), bytes);
        assert_eq!(
            fs::read(fence.path().join("user.txt")).unwrap(),
            b"preserve"
        );
        // Restore only this test's original owned directory, then prove the
        // same retained handle can finish cleanup and release exactly once.
        fs::rename(
            fence.path(),
            fixture.workspace.path().join("user-replacement"),
        )
        .unwrap();
        fs::rename(&moved, fence.path()).unwrap();
        fence.cleanup_after_tools_drained().await.unwrap();
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn integrity_failure_and_precancel_publish_nothing() {
        let mut fixture = Fixture::new();
        let budget = fixture.budget();
        fixture.files[0].sha256 = "bad".into();
        assert_eq!(
            fixture
                .run(budget.clone(), CancellationToken::new(), |_| Ok(()))
                .unwrap_err()
                .code,
            "skill_integrity_failed"
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert_eq!(
            fixture
                .run(budget.clone(), cancellation, |_| Ok(()))
                .unwrap_err()
                .code,
            "cancelled"
        );
        assert_eq!(fs::read_dir(fixture.workspace.path()).unwrap().count(), 0);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn undeclared_output_during_publication_is_not_published() {
        let fixture = Fixture::new();
        let budget = fixture.budget();
        let result = fixture.run(budget.clone(), CancellationToken::new(), |count| {
            if count == 1 {
                let directory = fs::read_dir(fixture.workspace.path())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::write(directory.join("unreleased.pyc"), "not release content").unwrap();
            }
            Ok(())
        });
        assert_eq!(result.unwrap_err().code, "skill_unavailable");
        assert_eq!(fs::read_dir(fixture.workspace.path()).unwrap().count(), 0);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn unpublished_cleanup_failure_returns_the_real_handle_to_the_host() {
        let fixture = Fixture::new();
        let budget = fixture.budget();
        let retained = fixture.workspace.path().join("retained-owned-scope");
        let result = fixture.run(budget.clone(), CancellationToken::new(), |count| {
            if count == 1 {
                let path = fs::read_dir(fixture.workspace.path())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                fs::rename(&path, &retained).unwrap();
                fs::create_dir(&path).unwrap();
                fs::write(path.join("business"), "preserve").unwrap();
            }
            Ok(())
        });
        let (code, scope) = result.unwrap_err().into_parts();
        assert_eq!(code, "skill_cleanup_incomplete");
        let mut scope =
            scope.expect("the native fence must receive ownership, not an inode receipt");
        assert!(budget.used.load(Ordering::Acquire) > 0);
        assert_eq!(
            fs::read(scope.path().join("business")).unwrap(),
            b"preserve"
        );
        fs::rename(
            scope.path(),
            fixture.workspace.path().join("user-replacement"),
        )
        .unwrap();
        fs::rename(&retained, scope.path()).unwrap();
        scope.cleanup_after_tools_drained().await.unwrap();
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_links_are_refused_and_scope_links_never_delete_the_target() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let budget = fixture.budget();
        let (_, mut scope) = fixture.normal(budget.clone()).into_parts();
        let outside = fixture.workspace.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("user.txt"), "preserve").unwrap();
        symlink(&outside, scope.path().join("cache-link")).unwrap();
        scope.cleanup_after_tools_drained().await.unwrap();
        assert_eq!(fs::read(outside.join("user.txt")).unwrap(), b"preserve");
        let source_file = fixture.source.path().join("skills/a/SKILL.md");
        fs::rename(&source_file, fixture.source.path().join("saved.md")).unwrap();
        symlink(fixture.source.path().join("saved.md"), &source_file).unwrap();
        assert_eq!(
            fixture
                .run(budget.clone(), CancellationToken::new(), |_| Ok(()))
                .unwrap_err()
                .code,
            "skill_unavailable"
        );
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }
}
