//! Android PRoot implementation of the shared mobile-Linux runtime seam.
//!
//! PRoot is a compatibility layer, not a security boundary. Admission remains
//! the responsibility of `MobileLinuxSandbox`; this module owns process and
//! rootfs lifecycle and fails closed when its native/runtime payload is absent.

use async_trait::async_trait;
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use platform_api::mobile_linux::LinuxEnforcementReceipt;
use platform_api::{
    LinuxCommandRequest, LinuxCommandResult, LinuxProcessHandle, MobileLinuxCapability,
    MobileLinuxError, MobileLinuxEvent, MobileLinuxEventKind, MobileLinuxRuntime,
    MobileLinuxRuntimeMode, MobileLinuxTaskSnapshot, MobileLinuxTaskStatus, MountPurpose,
    MountSpec, NetworkPolicy, ProcessStreamSink, PtyOpenRequest, PtySessionHandle, PtySize,
    RawStdioOpenRequest, RawStdioReadResult, RawStdioSessionHandle, RootfsState, RootfsStatus,
    SandboxBackend,
};
use platform_common::{RootfsManifest, RootfsStore, RootfsStoreError};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

const MAX_EVENTS: usize = 4096;
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_STDOUT_FRAGMENT_BYTES: usize = 16 * 1024;
const MAX_RAW_STDIO_BUFFER_BYTES: usize = 512 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const REAP_BUDGET: Duration = Duration::from_secs(2);
const ENFORCEMENT_RECEIPT_TIMEOUT: Duration = Duration::from_secs(3);
const MEMORY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOCAL_APP_BUILD_STATE_ROOT: &str = ".lingxi-build-state";

#[derive(Debug, Clone)]
/// Paths and immutable identity expected by one managed Android PRoot runtime.
pub struct AndroidProotRuntimeConfig {
    /// App-private directory containing `active`, `staged`, `bin`, and `tmp`.
    pub managed_root: PathBuf,
    /// Canonical app sandbox root (`Context.filesDir`) that owns app build roots.
    pub app_sandbox_root: PathBuf,
    /// Guest ABI label (`arm64-v8a` or `x86_64`).
    pub abi: String,
    /// Version directory selected below `staged`.
    pub rootfs_version: String,
    /// Expected source archive digest surfaced in status/diagnostics.
    pub archive_sha256: Option<String>,
}

impl AndroidProotRuntimeConfig {
    fn active_root(&self) -> PathBuf {
        self.managed_root.join("active")
    }

    fn staged_root(&self) -> PathBuf {
        self.managed_root.join("staged").join(&self.rootfs_version)
    }
}

#[derive(Debug)]
struct TaskControl {
    snapshot: Mutex<MobileLinuxTaskSnapshot>,
    pid: AtomicU64,
    cancel_requested: AtomicBool,
    terminal_emitted: AtomicBool,
}

impl TaskControl {
    fn new(task_id: String, command: String, status: MobileLinuxTaskStatus) -> Self {
        Self {
            snapshot: Mutex::new(MobileLinuxTaskSnapshot {
                task_id,
                status,
                command,
                started_at_ms: Some(now_ms()),
                finished_at_ms: None,
                exit_code: None,
                detail: None,
            }),
            pid: AtomicU64::new(0),
            cancel_requested: AtomicBool::new(false),
            terminal_emitted: AtomicBool::new(false),
        }
    }
}

struct PtyControl {
    task: Arc<TaskControl>,
    process: Arc<platform_pty::ProcessHandle>,
}

#[derive(Default)]
struct RawStdioBuffers {
    stdout: VecDeque<u8>,
    stderr: VecDeque<u8>,
    closed: bool,
    exit_code: Option<i32>,
    overflowed: bool,
}

struct RawStdioControl {
    task: Arc<TaskControl>,
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    buffers: Arc<Mutex<RawStdioBuffers>>,
    snapshot_roots: Vec<PathBuf>,
}

struct SpawnedChild {
    child: Child,
    enforcement: LinuxEnforcementReceipt,
    memory_limit_bytes: Option<u64>,
}

fn copy_raw_stdio_snapshot(source: &Path, destination: &Path) -> Result<(), MobileLinuxError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| {
        MobileLinuxError::Io(format!(
            "inspect read-only LSP workspace {}: {error}",
            source.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source).map_err(|error| {
            MobileLinuxError::Io(format!(
                "read LSP workspace symlink {}: {error}",
                source.display()
            ))
        })?;
        std::os::unix::fs::symlink(target, destination).map_err(|error| {
            MobileLinuxError::Io(format!(
                "copy LSP workspace symlink {}: {error}",
                source.display()
            ))
        })?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination).map_err(|error| {
            MobileLinuxError::Io(format!(
                "create LSP workspace snapshot {}: {error}",
                destination.display()
            ))
        })?;
        for entry in fs::read_dir(source).map_err(|error| {
            MobileLinuxError::Io(format!("read LSP workspace {}: {error}", source.display()))
        })? {
            let entry = entry.map_err(|error| MobileLinuxError::Io(error.to_string()))?;
            copy_raw_stdio_snapshot(&entry.path(), &destination.join(entry.file_name()))?;
        }
        fs::set_permissions(destination, metadata.permissions()).map_err(|error| {
            MobileLinuxError::Io(format!("preserve LSP snapshot permissions: {error}"))
        })?;
        return Ok(());
    }
    if metadata.is_file() {
        fs::copy(source, destination).map_err(|error| {
            MobileLinuxError::Io(format!(
                "copy LSP workspace file {}: {error}",
                source.display()
            ))
        })?;
        fs::set_permissions(destination, metadata.permissions()).map_err(|error| {
            MobileLinuxError::Io(format!("preserve LSP snapshot permissions: {error}"))
        })?;
        return Ok(());
    }
    Err(MobileLinuxError::InvalidRequest(format!(
        "unsupported special file in LSP workspace snapshot: {}",
        source.display()
    )))
}

fn snapshot_read_only_mounts(
    mounts: Vec<MountSpec>,
    snapshot_root: &Path,
) -> Result<(Vec<MountSpec>, Vec<PathBuf>), MobileLinuxError> {
    if snapshot_root.exists() {
        fs::remove_dir_all(snapshot_root).map_err(|error| {
            MobileLinuxError::Io(format!("clear stale LSP workspace snapshot: {error}"))
        })?;
    }
    let mut prepared = Vec::with_capacity(mounts.len());
    let mut roots = Vec::new();
    for (index, mut mount) in mounts.into_iter().enumerate() {
        if mount.read_only {
            let destination = snapshot_root.join(index.to_string());
            let node_modules = mount.host_path.join("node_modules");
            if mount.host_path.is_dir() {
                fs::create_dir_all(&destination).map_err(|error| {
                    MobileLinuxError::Io(format!(
                        "create LSP workspace snapshot {}: {error}",
                        destination.display()
                    ))
                })?;
                for entry in fs::read_dir(&mount.host_path).map_err(|error| {
                    MobileLinuxError::Io(format!(
                        "read LSP workspace {}: {error}",
                        mount.host_path.display()
                    ))
                })? {
                    let entry = entry.map_err(|error| MobileLinuxError::Io(error.to_string()))?;
                    if entry.file_name() == "node_modules" {
                        continue;
                    }
                    copy_raw_stdio_snapshot(&entry.path(), &destination.join(entry.file_name()))?;
                }
            } else {
                copy_raw_stdio_snapshot(&mount.host_path, &destination)?;
            }
            let guest_root = mount.guest_path.clone();
            mount.host_path = destination;
            mount.read_only = false;
            roots.push(snapshot_root.to_path_buf());
            prepared.push(mount);
            if node_modules.is_dir() {
                prepared.push(MountSpec {
                    host_path: node_modules,
                    guest_path: format!("{guest_root}/node_modules"),
                    read_only: false,
                    purpose: MountPurpose::External,
                });
            }
            continue;
        }
        prepared.push(mount);
    }
    roots.sort();
    roots.dedup();
    Ok((prepared, roots))
}

#[derive(Clone, Copy)]
enum ForegroundMountMode {
    Merged,
    RequestOnly,
    ExplicitOnly,
}

struct RuntimeState {
    config: AndroidProotRuntimeConfig,
    mounts: RwLock<Vec<MountSpec>>,
    tasks: Mutex<HashMap<String, Arc<TaskControl>>>,
    ptys: Mutex<HashMap<String, Arc<PtyControl>>>,
    raw_stdio: Mutex<HashMap<String, Arc<RawStdioControl>>>,
    events: Mutex<VecDeque<MobileLinuxEvent>>,
    next_id: AtomicU64,
    next_sequence: AtomicU64,
    booted: AtomicBool,
}

#[derive(Clone)]
/// Process, PTY, mount, event, and rootfs lifecycle owner for Android PRoot.
pub struct AndroidProotRuntime {
    state: Arc<RuntimeState>,
}

impl std::fmt::Debug for AndroidProotRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AndroidProotRuntime")
            .field("config", &self.state.config)
            .field("booted", &self.state.booted.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl AndroidProotRuntime {
    /// Construct a stopped runtime. No filesystem state is mutated until boot.
    #[must_use]
    pub fn new(config: AndroidProotRuntimeConfig) -> Self {
        Self {
            state: Arc::new(RuntimeState {
                config,
                mounts: RwLock::new(Vec::new()),
                tasks: Mutex::new(HashMap::new()),
                ptys: Mutex::new(HashMap::new()),
                raw_stdio: Mutex::new(HashMap::new()),
                events: Mutex::new(VecDeque::new()),
                next_id: AtomicU64::new(1),
                next_sequence: AtomicU64::new(1),
                booted: AtomicBool::new(false),
            }),
        }
    }

    fn next_id(&self, prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            self.state.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn emit(&self, task_id: Option<String>, kind: MobileLinuxEventKind) {
        let mut events = self.state.events.lock().expect("mobile-linux events mutex");
        events.push_back(MobileLinuxEvent {
            sequence: self.state.next_sequence.fetch_add(1, Ordering::Relaxed),
            task_id,
            kind,
        });
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
    }

    fn proot_binary(&self) -> Result<PathBuf, MobileLinuxError> {
        let root = &self.state.config.managed_root;
        let mut candidates = vec![
            root.join("bin/libproot.so"),
            root.join("libproot.so"),
            root.join("bin/proot"),
        ];
        if let Some(native_lib_dir) = android_native_library_dir() {
            candidates.insert(0, native_lib_dir.join("libproot.so"));
        }
        candidates
            .into_iter()
            .find(|candidate| executable_regular_file(candidate))
            .ok_or_else(|| {
                MobileLinuxError::Unavailable(format!(
                    "PRoot executable is missing under {}",
                    root.display()
                ))
            })
    }

    fn policy_launcher(&self) -> Result<PathBuf, MobileLinuxError> {
        let mut candidates = vec![
            self.state
                .config
                .managed_root
                .join("bin/libmobile_linux_policy_launcher.so"),
            self.state
                .config
                .managed_root
                .join("libmobile_linux_policy_launcher.so"),
        ];
        if let Some(native_lib_dir) = android_native_library_dir() {
            candidates.insert(0, native_lib_dir.join("libmobile_linux_policy_launcher.so"));
        }
        candidates
            .into_iter()
            .find(|candidate| executable_regular_file(candidate))
            .ok_or_else(|| {
                MobileLinuxError::NetworkPolicyUnavailable(
                    "Android network policy launcher is not packaged or executable".to_string(),
                )
            })
    }

    fn checked_active_root(&self) -> Result<PathBuf, MobileLinuxError> {
        let root = self.state.config.active_root();
        let metadata = fs::symlink_metadata(&root).map_err(|error| {
            MobileLinuxError::Unavailable(format!(
                "active rootfs is missing at {}: {error}",
                root.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(MobileLinuxError::Integrity(
                "active rootfs must be a real directory".to_string(),
            ));
        }
        let shell = root.join("bin/sh");
        if !executable_regular_file(&shell) {
            return Err(MobileLinuxError::Integrity(format!(
                "rootfs shell is missing or not executable: {}",
                shell.display()
            )));
        }
        Ok(root)
    }

    fn readiness(&self) -> Result<(PathBuf, PathBuf), MobileLinuxError> {
        Ok((self.proot_binary()?, self.checked_active_root()?))
    }

    fn rootfs_store(&self) -> RootfsStore {
        RootfsStore::new(
            self.state.config.managed_root.clone(),
            SandboxBackend::AndroidProot,
            MobileLinuxRuntimeMode::MobileLinux,
            "android",
            self.state.config.abi.clone(),
        )
    }

    fn rootfs_manifest_paths(&self) -> [PathBuf; 2] {
        [
            self.state.config.managed_root.join("rootfs-manifest.json"),
            self.state.config.active_root().join("rootfs-manifest.json"),
        ]
    }

    fn load_rootfs_manifest(&self) -> Result<RootfsManifest, MobileLinuxError> {
        let paths = self.rootfs_manifest_paths();
        for path in &paths {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(MobileLinuxError::Io(format!(
                        "read rootfs manifest metadata {}: {error}",
                        path.display()
                    )))
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(MobileLinuxError::Integrity(format!(
                    "rootfs manifest must be a regular file: {}",
                    path.display()
                )));
            }
            let bytes = fs::read(path).map_err(|error| {
                MobileLinuxError::Io(format!("read rootfs manifest {}: {error}", path.display()))
            })?;
            let manifest = serde_json::from_slice::<RootfsManifest>(&bytes).map_err(|error| {
                MobileLinuxError::Integrity(format!(
                    "parse rootfs manifest {}: {error}",
                    path.display()
                ))
            })?;
            return Ok(manifest);
        }
        Err(MobileLinuxError::Unavailable(format!(
            "rootfs manifest is missing (looked for {} and {})",
            paths[0].display(),
            paths[1].display()
        )))
    }

    fn execution_mounts(
        &self,
        request_mounts: &[MountSpec],
        mode: ForegroundMountMode,
    ) -> Result<Vec<MountSpec>, MobileLinuxError> {
        if matches!(mode, ForegroundMountMode::RequestOnly) {
            validate_isolated_local_app_mounts(
                request_mounts,
                &self.state.config.managed_root,
                &self.state.config.app_sandbox_root,
            )?;
        }
        let mut mounts = match mode {
            ForegroundMountMode::Merged => self
                .state
                .mounts
                .read()
                .expect("mobile-linux mounts rwlock")
                .clone(),
            ForegroundMountMode::RequestOnly | ForegroundMountMode::ExplicitOnly => {
                Vec::with_capacity(request_mounts.len())
            }
        };
        for mount in request_mounts {
            validate_mount(mount, &self.state.config.managed_root)?;
            mounts.retain(|existing| existing.guest_path != mount.guest_path);
            mounts.push(mount.clone());
        }
        Ok(mounts)
    }

    async fn spawn_child_with_mounts(
        &self,
        request: &LinuxCommandRequest,
        mounts: &[MountSpec],
        isolated_local_app_build_mounts: Option<&[MountSpec]>,
    ) -> Result<SpawnedChild, MobileLinuxError> {
        validate_request(request, isolated_local_app_build_mounts)?;
        let memory_limit_bytes = requested_memory_limit_bytes(request)?;
        if memory_limit_bytes.is_some() {
            ensure_process_group_rss_available()?;
        }
        let receipt_policy = enforced_network_policy_name(request.network);
        let receipt_path = if receipt_policy.is_some() {
            let path = self.state.config.managed_root.join("tmp").join(format!(
                "network-policy-receipt-{}",
                self.state.next_id.fetch_add(1, Ordering::Relaxed)
            ));
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| {
                    MobileLinuxError::Io(format!(
                        "create network enforcement receipt {}: {error}",
                        path.display()
                    ))
                })?;
            Some(path)
        } else {
            None
        };
        let mut command = match self.build_command(
            request,
            request.cwd.as_deref(),
            &request.env,
            mounts,
            receipt_path.as_deref(),
        ) {
            Ok(command) => command,
            Err(error) => {
                if let Some(path) = receipt_path.as_deref() {
                    let _ = fs::remove_file(path);
                }
                return Err(error);
            }
        };
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(path) = receipt_path.as_deref() {
                    let _ = fs::remove_file(path);
                }
                return Err(MobileLinuxError::Io(format!("spawn PRoot: {error}")));
            }
        };
        let network_policy_enforced = if let Some(path) = receipt_path.as_deref() {
            match wait_for_network_policy_receipt(
                &mut child,
                path,
                receipt_policy.expect("restricted policy has receipt name"),
            )
            .await
            {
                Ok(()) => true,
                Err(error) => {
                    if let Some(pid) = child.id() {
                        terminate_group(pid, Signal::SIGKILL);
                    }
                    let _ = child.wait().await;
                    let _ = fs::remove_file(path);
                    return Err(error);
                }
            }
        } else {
            false
        };
        if let Some(path) = receipt_path {
            let _ = fs::remove_file(path);
        }
        if let Some(input) = &request.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(input.as_bytes())
                    .await
                    .map_err(|error| MobileLinuxError::Io(format!("write stdin: {error}")))?;
            }
        }
        Ok(SpawnedChild {
            child,
            enforcement: LinuxEnforcementReceipt {
                network_policy_enforced,
                memory_limit_enforced: memory_limit_bytes.is_some(),
            },
            memory_limit_bytes,
        })
    }

    fn build_command(
        &self,
        request: &LinuxCommandRequest,
        cwd: Option<&str>,
        env: &BTreeMap<String, String>,
        mounts: &[MountSpec],
        receipt_path: Option<&Path>,
    ) -> Result<Command, MobileLinuxError> {
        let (proot, rootfs) = self.readiness()?;
        let native_lib_dir = proot.parent().map(Path::to_path_buf);
        let mut proot_args = vec![
            "-0".to_string(),
            "--link2symlink".to_string(),
            "-r".to_string(),
            rootfs.display().to_string(),
            "-b".to_string(),
            "/dev".to_string(),
            "-b".to_string(),
            "/proc".to_string(),
            "-b".to_string(),
            "/sys".to_string(),
            "-w".to_string(),
            cwd.unwrap_or("/root").to_string(),
        ];
        if matches!(request.network, NetworkPolicy::LoopbackOnly) {
            // This enables LingXi's sockaddr-aware extension in the pinned
            // PRoot build. The extension, not the outer launcher, publishes
            // the LoopbackOnly enforcement receipt after initialization.
            proot_args.push("-p".to_string());
        }
        for mount in mounts {
            proot_args.push("-b".to_string());
            proot_args.push(format!(
                "{}:{}",
                mount.host_path.display(),
                mount.guest_path
            ));
        }
        proot_args.push(request.command.clone());
        proot_args.extend(request.args.iter().cloned());

        let mut command = match enforced_network_policy_name(request.network) {
            None => Command::new(&proot),
            Some(policy) => {
                let launcher = self.policy_launcher()?;
                let mut command = Command::new(launcher);
                command.arg(policy).arg(&proot);
                command
            }
        };
        command
            .args(proot_args)
            .env_clear()
            .env("PROOT_TMP_DIR", self.state.config.managed_root.join("tmp"))
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/bin",
            )
            .env("HOME", "/root")
            .envs(env);
        if let Some(receipt_path) = receipt_path {
            command.env("LINGXI_ENFORCEMENT_RECEIPT_PATH", receipt_path);
        }
        if let Some(native_lib_dir) = native_lib_dir {
            command.env("LD_LIBRARY_PATH", &native_lib_dir);
            let loader = native_lib_dir.join("libproot-loader.so");
            if executable_regular_file(&loader) {
                command.env("PROOT_LOADER", loader);
            }
            let loader32 = native_lib_dir.join("libproot-loader32.so");
            if executable_regular_file(&loader32) {
                command.env("PROOT_LOADER_32", loader32);
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        Ok(command)
    }

    fn build_pty_invocation(
        &self,
        executable: &str,
        executable_args: &[String],
        cwd: Option<&str>,
        env: &BTreeMap<String, String>,
        mounts: &[MountSpec],
    ) -> Result<(String, Vec<String>, HashMap<String, String>), MobileLinuxError> {
        let (proot, rootfs) = self.readiness()?;
        let mut args = vec![
            "-0".to_string(),
            "--link2symlink".to_string(),
            "-r".to_string(),
            rootfs.display().to_string(),
            "-b".to_string(),
            "/dev".to_string(),
            "-b".to_string(),
            "/proc".to_string(),
            "-b".to_string(),
            "/sys".to_string(),
            "-w".to_string(),
            cwd.unwrap_or("/root").to_string(),
        ];
        for mount in mounts {
            args.push("-b".to_string());
            args.push(format!(
                "{}:{}",
                mount.host_path.display(),
                mount.guest_path
            ));
        }
        args.push(executable.to_string());
        args.extend(executable_args.iter().cloned());
        let mut child_env = HashMap::from([
            (
                "PROOT_TMP_DIR".to_string(),
                self.state
                    .config
                    .managed_root
                    .join("tmp")
                    .display()
                    .to_string(),
            ),
            (
                "PATH".to_string(),
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/bin".to_string(),
            ),
            ("HOME".to_string(), "/root".to_string()),
        ]);
        child_env.extend(env.iter().map(|(key, value)| (key.clone(), value.clone())));
        if let Some(native_lib_dir) = proot.parent() {
            child_env.insert(
                "LD_LIBRARY_PATH".to_string(),
                native_lib_dir.display().to_string(),
            );
            let loader = native_lib_dir.join("libproot-loader.so");
            if executable_regular_file(&loader) {
                child_env.insert("PROOT_LOADER".to_string(), loader.display().to_string());
            }
            let loader32 = native_lib_dir.join("libproot-loader32.so");
            if executable_regular_file(&loader32) {
                child_env.insert(
                    "PROOT_LOADER_32".to_string(),
                    loader32.display().to_string(),
                );
            }
        }
        Ok((proot.display().to_string(), args, child_env))
    }

    fn create_task(
        &self,
        prefix: &str,
        command: String,
        status: MobileLinuxTaskStatus,
    ) -> (String, Arc<TaskControl>) {
        let id = self.next_id(prefix);
        let task = Arc::new(TaskControl::new(id.clone(), command, status));
        self.state
            .tasks
            .lock()
            .expect("mobile-linux tasks mutex")
            .insert(id.clone(), task.clone());
        self.emit(
            Some(id.clone()),
            MobileLinuxEventKind::TaskStatusChanged {
                status,
                exit_code: None,
                detail: None,
            },
        );
        (id, task)
    }

    fn finish_task(
        &self,
        id: &str,
        task: &TaskControl,
        status: MobileLinuxTaskStatus,
        exit_code: Option<i32>,
        detail: Option<String>,
    ) {
        if task.terminal_emitted.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut snapshot = task.snapshot.lock().expect("task snapshot mutex");
        snapshot.status = status;
        snapshot.finished_at_ms = Some(now_ms());
        snapshot.exit_code = exit_code;
        snapshot.detail.clone_from(&detail);
        drop(snapshot);
        self.emit(
            Some(id.to_string()),
            MobileLinuxEventKind::TaskStatusChanged {
                status,
                exit_code,
                detail,
            },
        );
    }

    async fn spawn_child(
        &self,
        request: &LinuxCommandRequest,
    ) -> Result<SpawnedChild, MobileLinuxError> {
        let mounts = self.execution_mounts(&request.mounts, ForegroundMountMode::Merged)?;
        self.spawn_child_with_mounts(request, &mounts, None).await
    }

    async fn run_inner(
        &self,
        request: LinuxCommandRequest,
        sink: Option<Arc<dyn ProcessStreamSink>>,
        mount_mode: ForegroundMountMode,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.boot().await?;
        let mounts = self.execution_mounts(&request.mounts, mount_mode)?;
        let (id, task) = self.create_task(
            "task",
            display_command(&request.command, &request.args),
            MobileLinuxTaskStatus::Running,
        );
        let isolated_local_app_build_mounts =
            matches!(mount_mode, ForegroundMountMode::RequestOnly).then_some(mounts.as_slice());
        let spawned = match self
            .spawn_child_with_mounts(&request, &mounts, isolated_local_app_build_mounts)
            .await
        {
            Ok(spawned) => spawned,
            Err(error) => {
                self.finish_task(
                    &id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        };
        let SpawnedChild {
            mut child,
            enforcement,
            memory_limit_bytes,
        } = spawned;
        let pid = child
            .id()
            .ok_or_else(|| MobileLinuxError::Io("PRoot child has no pid".to_string()))?;
        task.pid.store(u64::from(pid), Ordering::Release);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MobileLinuxError::Io("PRoot stdout unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| MobileLinuxError::Io("PRoot stderr unavailable".to_string()))?;
        let stdout_runtime = self.clone();
        let stdout_id = id.clone();
        let stdout_sink = sink.clone();
        let stdout_task = tokio::spawn(async move {
            read_stdout(stdout, move |line| {
                stdout_runtime.emit(
                    Some(stdout_id.clone()),
                    MobileLinuxEventKind::StdoutLine { line: line.clone() },
                );
                let sink = stdout_sink.clone();
                async move {
                    if let Some(sink) = sink {
                        sink.stdout_line(line)
                            .await
                            .map_err(MobileLinuxError::from)?;
                    }
                    Ok(())
                }
            })
            .await
        });
        let stderr_runtime = self.clone();
        let stderr_id = id.clone();
        let stderr_task = tokio::spawn(async move {
            read_stderr(stderr, move |chunk| {
                stderr_runtime.emit(
                    Some(stderr_id.clone()),
                    MobileLinuxEventKind::StderrChunk {
                        chunk: chunk.clone(),
                    },
                );
                let sink = sink.clone();
                async move {
                    if let Some(sink) = sink {
                        sink.stderr_chunk(chunk)
                            .await
                            .map_err(MobileLinuxError::from)?;
                    }
                    Ok(())
                }
            })
            .await
        });
        let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let waited = wait_for_child(&mut child, pid, Some(timeout), memory_limit_bytes).await;
        let (exit_code, timed_out, memory_limit_exceeded) = match waited {
            Ok(ChildWaitOutcome::Exited(status)) => (status.code().unwrap_or(-1), false, None),
            Ok(ChildWaitOutcome::TimedOut) => (-1, true, None),
            Ok(ChildWaitOutcome::MemoryLimitExceeded(diagnostic)) => (-1, false, Some(diagnostic)),
            Err(error) => {
                terminate_group(pid, Signal::SIGKILL);
                let _ = child.wait().await;
                self.finish_task(
                    &id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        };
        let stdout = join_reader(stdout_task, "stdout").await?;
        let stderr = join_reader(stderr_task, "stderr").await?;
        let cancelled = task.cancel_requested.load(Ordering::Acquire);
        let status = if memory_limit_exceeded.is_some() {
            MobileLinuxTaskStatus::Failed
        } else if cancelled {
            MobileLinuxTaskStatus::Cancelled
        } else if timed_out {
            MobileLinuxTaskStatus::TimedOut
        } else if exit_code == 0 {
            MobileLinuxTaskStatus::Completed
        } else {
            MobileLinuxTaskStatus::Failed
        };
        self.finish_task(
            &id,
            &task,
            status,
            (!timed_out && !cancelled).then_some(exit_code),
            if let Some(diagnostic) = &memory_limit_exceeded {
                Some(diagnostic.detail())
            } else if cancelled {
                Some("command cancelled".to_string())
            } else {
                timed_out.then(|| "command timed out".to_string())
            },
        );
        if let Some(diagnostic) = memory_limit_exceeded {
            return Err(MobileLinuxError::ResourceLimitExceeded(
                diagnostic.summary(),
            ));
        }
        Ok(LinuxCommandResult {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code,
            timed_out,
            cancelled,
            enforcement,
        })
    }

    fn append_raw_stdio_bytes(queue: &mut VecDeque<u8>, chunk: &[u8]) -> bool {
        if queue.len().saturating_add(chunk.len()) > MAX_RAW_STDIO_BUFFER_BYTES {
            return false;
        }
        queue.extend(chunk.iter().copied());
        true
    }

    fn drain_raw_stdio_bytes(queue: &mut VecDeque<u8>, max_bytes: usize) -> Vec<u8> {
        queue.drain(..queue.len().min(max_bytes)).collect()
    }

    fn rootfs_snapshot(&self) -> RootfsStatus {
        if let Ok(manifest) = self.load_rootfs_manifest() {
            return self.rootfs_store().status(&manifest);
        }
        let active = self.state.config.active_root();
        let staged = self.state.config.staged_root();
        let root = self.checked_active_root();
        let proot = self.proot_binary();
        let (state, last_error) = match (&root, &proot) {
            (Ok(_), Ok(_)) => (RootfsState::Ready, None),
            (Err(error), _) if path_present(&active) => {
                (RootfsState::Corrupt, Some(error.to_string()))
            }
            (Err(error), _) if path_present(&staged) => {
                (RootfsState::Installing, Some(error.to_string()))
            }
            (Err(error), _) => (RootfsState::Missing, Some(error.to_string())),
            (_, Err(error)) => (RootfsState::Unsupported, Some(error.to_string())),
        };
        RootfsStatus {
            state,
            backend: SandboxBackend::AndroidProot,
            mode: MobileLinuxRuntimeMode::MobileLinux,
            platform: "android".to_string(),
            abi: self.state.config.abi.clone(),
            version: Some(self.state.config.rootfs_version.clone()),
            managed_root: Some(self.state.config.managed_root.clone()),
            active_root: path_present(&active).then_some(active.clone()),
            staged_root: path_present(&staged).then_some(staged),
            archive_sha256: self.state.config.archive_sha256.clone(),
            installed_size_bytes: directory_size(&active).ok(),
            writable_guest_paths: vec![
                "/root".to_string(),
                "/tmp".to_string(),
                "/var/tmp".to_string(),
            ],
            last_error,
        }
    }
}

#[async_trait]
impl MobileLinuxRuntime for AndroidProotRuntime {
    fn backend(&self) -> SandboxBackend {
        SandboxBackend::AndroidProot
    }

    fn mode(&self) -> MobileLinuxRuntimeMode {
        MobileLinuxRuntimeMode::MobileLinux
    }

    async fn probe_capability(&self) -> MobileLinuxCapability {
        let readiness = self.readiness();
        MobileLinuxCapability {
            available: readiness.is_ok(),
            backend: SandboxBackend::AndroidProot,
            mode: MobileLinuxRuntimeMode::MobileLinux,
            reason: readiness.err().map(|error| error.to_string()),
            streaming_output: true,
            background_processes: true,
            pty: true,
            bind_mounts: true,
            rootfs_integrity: self.load_rootfs_manifest().is_ok(),
        }
    }

    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.readiness()?;
        fs::create_dir_all(self.state.config.managed_root.join("tmp"))
            .map_err(|error| MobileLinuxError::Io(format!("create PRoot tmp: {error}")))?;
        self.state.booted.store(true, Ordering::Release);
        Ok(self.rootfs_snapshot())
    }

    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        let pty_ids: HashSet<_> = self
            .state
            .ptys
            .lock()
            .expect("mobile-linux PTY mutex")
            .keys()
            .cloned()
            .collect();
        let raw_stdio_ids: HashSet<_> = self
            .state
            .raw_stdio
            .lock()
            .expect("mobile-linux raw stdio mutex")
            .keys()
            .cloned()
            .collect();
        let task_ids: Vec<_> = self
            .state
            .tasks
            .lock()
            .expect("mobile-linux tasks mutex")
            .keys()
            .filter(|id| !pty_ids.contains(*id) && !raw_stdio_ids.contains(*id))
            .cloned()
            .collect();
        let mut errors = Vec::new();
        for id in pty_ids {
            if let Err(error) = self.close_pty(&PtySessionHandle { id }).await {
                errors.push(error.to_string());
            }
        }
        for id in raw_stdio_ids {
            if let Err(error) = self
                .close_raw_stdio(&RawStdioSessionHandle {
                    id,
                    enforcement: LinuxEnforcementReceipt::default(),
                })
                .await
            {
                errors.push(error.to_string());
            }
        }
        for id in task_ids {
            if let Err(error) = self
                .kill(&LinuxProcessHandle {
                    id,
                    enforcement: LinuxEnforcementReceipt::default(),
                })
                .await
            {
                errors.push(error.to_string());
            }
        }
        self.state.booted.store(false, Ordering::Release);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(MobileLinuxError::Io(format!(
                "shutdown reaping failed: {}",
                errors.join("; ")
            )))
        }
    }

    async fn run(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.run_inner(request, None, ForegroundMountMode::Merged)
            .await
    }

    async fn run_isolated(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.run_inner(request, None, ForegroundMountMode::RequestOnly)
            .await
    }

    async fn run_streaming(
        &self,
        request: LinuxCommandRequest,
        sink: Arc<dyn ProcessStreamSink>,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        self.run_inner(request, Some(sink), ForegroundMountMode::Merged)
            .await
    }

    async fn spawn_background(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        self.boot().await?;
        let (id, task) = self.create_task(
            "bg",
            display_command(&request.command, &request.args),
            MobileLinuxTaskStatus::Backgrounded,
        );
        let spawned = match self.spawn_child(&request).await {
            Ok(spawned) => spawned,
            Err(error) => {
                self.finish_task(
                    &id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        };
        let SpawnedChild {
            mut child,
            enforcement,
            memory_limit_bytes,
        } = spawned;
        let pid = child
            .id()
            .ok_or_else(|| MobileLinuxError::Io("PRoot child has no pid".to_string()))?;
        task.pid.store(u64::from(pid), Ordering::Release);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let runtime = self.clone();
        let reaper_id = id.clone();
        tokio::spawn(async move {
            let stdout_task = stdout.map(|stdout| {
                let rt = runtime.clone();
                let stream_id = reaper_id.clone();
                tokio::spawn(async move {
                    read_stdout(stdout, move |line| {
                        rt.emit(
                            Some(stream_id.clone()),
                            MobileLinuxEventKind::StdoutLine { line },
                        );
                        async { Ok(()) }
                    })
                    .await
                })
            });
            let stderr_task = stderr.map(|stderr| {
                let rt = runtime.clone();
                let stream_id = reaper_id.clone();
                tokio::spawn(async move {
                    read_stderr(stderr, move |chunk| {
                        rt.emit(
                            Some(stream_id.clone()),
                            MobileLinuxEventKind::StderrChunk { chunk },
                        );
                        async { Ok(()) }
                    })
                    .await
                })
            });
            let result = wait_for_child(
                &mut child,
                pid,
                request.timeout_ms.map(Duration::from_millis),
                memory_limit_bytes,
            )
            .await;
            let stdout_result = match stdout_task {
                Some(task) => join_reader(task, "stdout").await,
                None => Ok(Vec::new()),
            };
            let stderr_result = match stderr_task {
                Some(task) => join_reader(task, "stderr").await,
                None => Ok(Vec::new()),
            };
            let reader_error = stdout_result.err().or_else(|| stderr_result.err());
            let cancelled = task.cancel_requested.load(Ordering::Acquire);
            match (result, reader_error) {
                (_, Some(error)) => runtime.finish_task(
                    &reaper_id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                ),
                (Ok(ChildWaitOutcome::Exited(status)), None) => {
                    let code = status.code().unwrap_or(-1);
                    runtime.finish_task(
                        &reaper_id,
                        &task,
                        if cancelled {
                            MobileLinuxTaskStatus::Cancelled
                        } else if code == 0 {
                            MobileLinuxTaskStatus::Completed
                        } else {
                            MobileLinuxTaskStatus::Failed
                        },
                        Some(code),
                        cancelled.then(|| "task cancelled".to_string()),
                    );
                }
                (Ok(ChildWaitOutcome::TimedOut), None) => runtime.finish_task(
                    &reaper_id,
                    &task,
                    MobileLinuxTaskStatus::TimedOut,
                    None,
                    Some("command timed out".to_string()),
                ),
                (Ok(ChildWaitOutcome::MemoryLimitExceeded(diagnostic)), None) => {
                    runtime.finish_task(
                        &reaper_id,
                        &task,
                        MobileLinuxTaskStatus::Failed,
                        None,
                        Some(diagnostic.detail()),
                    );
                }
                (Err(error), None) => runtime.finish_task(
                    &reaper_id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                ),
            }
        });
        Ok(LinuxProcessHandle { id, enforcement })
    }

    async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        let task = self
            .state
            .tasks
            .lock()
            .expect("mobile-linux tasks mutex")
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| MobileLinuxError::InvalidRequest("unknown task handle".to_string()))?;
        if task.terminal_emitted.load(Ordering::Acquire) {
            return Ok(());
        }
        task.cancel_requested.store(true, Ordering::Release);
        let pid = u32::try_from(task.pid.load(Ordering::Acquire))
            .map_err(|_| MobileLinuxError::Io("invalid task pid".to_string()))?;
        if pid == 0 {
            return Err(MobileLinuxError::Io(
                "task has not published its pid".to_string(),
            ));
        }
        terminate_group(pid, Signal::SIGTERM);
        let deadline = tokio::time::Instant::now() + REAP_BUDGET;
        let hard_kill_at = tokio::time::Instant::now() + REAP_BUDGET / 2;
        let mut sent_sigkill = false;
        while !task.terminal_emitted.load(Ordering::Acquire) {
            if !sent_sigkill && tokio::time::Instant::now() >= hard_kill_at {
                terminate_group(pid, Signal::SIGKILL);
                sent_sigkill = true;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(MobileLinuxError::Io(format!(
                    "task {} did not reap within 2 seconds",
                    handle.id
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    async fn open_pty(
        &self,
        request: PtyOpenRequest,
    ) -> Result<PtySessionHandle, MobileLinuxError> {
        self.boot().await?;
        validate_pty_request(&request)?;
        let mounts = self.execution_mounts(&request.mounts, ForegroundMountMode::Merged)?;
        let (program, args, env) = self.build_pty_invocation(
            &request.command,
            &request.args,
            request.cwd.as_deref(),
            &request.env,
            &mounts,
        )?;
        let (id, task) = self.create_task(
            "pty",
            display_command(&request.command, &request.args),
            MobileLinuxTaskStatus::Running,
        );
        let spawned = platform_pty::spawn_pty_process(
            &program,
            &args,
            &self.state.config.managed_root,
            &env,
            &None,
            platform_pty::TerminalSize {
                cols: request.size.cols,
                rows: request.size.rows,
            },
            &[],
        )
        .await;
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                let error = MobileLinuxError::Io(format!("spawn PRoot PTY: {error}"));
                self.finish_task(
                    &id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        };
        let process = Arc::new(spawned.session);
        if let Some(pid) = process.process_id() {
            task.pid.store(u64::from(pid), Ordering::Release);
        }
        let control = Arc::new(PtyControl {
            task: task.clone(),
            process: process.clone(),
        });
        self.state
            .ptys
            .lock()
            .expect("mobile-linux PTY mutex")
            .insert(id.clone(), control);
        let mut stdout = spawned.stdout_rx;
        let exit = spawned.exit_rx;
        let runtime = self.clone();
        let reaper_id = id.clone();
        tokio::spawn(async move {
            let output_runtime = runtime.clone();
            let output_id = reaper_id.clone();
            let output_task = tokio::spawn(async move {
                while let Some(data) = stdout.recv().await {
                    output_runtime.emit(
                        Some(output_id.clone()),
                        MobileLinuxEventKind::PtyOutput {
                            session_id: output_id.clone(),
                            data,
                        },
                    );
                }
            });
            let result = exit.await;
            let _ = output_task.await;
            let cancelled = task.cancel_requested.load(Ordering::Acquire);
            let (status, code, detail) = match result {
                Ok(code) => (
                    if cancelled {
                        MobileLinuxTaskStatus::Cancelled
                    } else if code == 0 {
                        MobileLinuxTaskStatus::Completed
                    } else {
                        MobileLinuxTaskStatus::Failed
                    },
                    Some(code),
                    cancelled.then(|| "PTY closed".to_string()),
                ),
                Err(error) => (
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(format!("PTY exit channel closed: {error}")),
                ),
            };
            runtime.finish_task(&reaper_id, &task, status, code, detail.clone());
            runtime.emit(
                Some(reaper_id.clone()),
                MobileLinuxEventKind::PtyClosed {
                    session_id: reaper_id.clone(),
                    exit_code: code,
                    detail,
                },
            );
            runtime
                .state
                .ptys
                .lock()
                .expect("mobile-linux PTY mutex")
                .remove(&reaper_id);
        });
        Ok(PtySessionHandle { id })
    }

    async fn open_raw_stdio(
        &self,
        request: RawStdioOpenRequest,
    ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
        self.boot().await?;
        let (id, task) = self.create_task(
            "stdio",
            display_command(&request.command, &request.args),
            MobileLinuxTaskStatus::Running,
        );
        let snapshot_root = self
            .state
            .config
            .app_sandbox_root
            .join("cache/lingxi-raw-stdio-snapshots")
            .join(&id);
        let snapshot_root_for_copy = snapshot_root.clone();
        let request_mounts = request.mounts.clone();
        let snapshot_result = tokio::task::spawn_blocking(move || {
            snapshot_read_only_mounts(request_mounts, &snapshot_root_for_copy)
        })
        .await
        .map_err(|error| MobileLinuxError::Io(format!("join LSP workspace snapshot: {error}")))?;
        let (prepared_mounts, snapshot_roots) = match snapshot_result {
            Ok(value) => value,
            Err(error) => {
                let _ = fs::remove_dir_all(&snapshot_root);
                self.finish_task(
                    &id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        };
        let mounts =
            match self.execution_mounts(&prepared_mounts, ForegroundMountMode::ExplicitOnly) {
                Ok(mounts) => mounts,
                Err(error) => {
                    for root in &snapshot_roots {
                        let _ = fs::remove_dir_all(root);
                    }
                    self.finish_task(
                        &id,
                        &task,
                        MobileLinuxTaskStatus::Failed,
                        None,
                        Some(error.to_string()),
                    );
                    return Err(error);
                }
            };
        let command_request = LinuxCommandRequest {
            command: request.command,
            args: request.args,
            cwd: request.cwd,
            env: request.env,
            stdin: None,
            timeout_ms: None,
            network: request.network,
            resource_limits: request.resource_limits,
            mounts: mounts.clone(),
        };
        let spawned = match self
            .spawn_child_with_mounts(&command_request, &mounts, None)
            .await
        {
            Ok(spawned) => spawned,
            Err(error) => {
                for root in &snapshot_roots {
                    let _ = fs::remove_dir_all(root);
                }
                self.finish_task(
                    &id,
                    &task,
                    MobileLinuxTaskStatus::Failed,
                    None,
                    Some(error.to_string()),
                );
                return Err(error);
            }
        };
        let SpawnedChild {
            mut child,
            enforcement,
            ..
        } = spawned;
        let pid = child
            .id()
            .ok_or_else(|| MobileLinuxError::Io("PRoot child has no pid".to_string()))?;
        task.pid.store(u64::from(pid), Ordering::Release);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| MobileLinuxError::Io("PRoot stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MobileLinuxError::Io("PRoot stdout unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| MobileLinuxError::Io("PRoot stderr unavailable".to_string()))?;
        let buffers = Arc::new(Mutex::new(RawStdioBuffers::default()));
        let control = Arc::new(RawStdioControl {
            task: task.clone(),
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            buffers: buffers.clone(),
            snapshot_roots,
        });
        self.state
            .raw_stdio
            .lock()
            .expect("mobile-linux raw stdio mutex")
            .insert(id.clone(), control);
        tokio::spawn(read_raw_stream(stdout, {
            let buffers = buffers.clone();
            move |chunk| {
                let buffers = buffers.clone();
                async move {
                    let mut guard = buffers.lock().expect("raw stdio stdout mutex");
                    if !Self::append_raw_stdio_bytes(&mut guard.stdout, &chunk) {
                        guard.overflowed = true;
                    }
                    Ok(())
                }
            }
        }));
        tokio::spawn(read_raw_stream(stderr, move |chunk| {
            let buffers = buffers.clone();
            async move {
                let mut guard = buffers.lock().expect("raw stdio stderr mutex");
                if !Self::append_raw_stdio_bytes(&mut guard.stderr, &chunk) {
                    guard.overflowed = true;
                }
                Ok(())
            }
        }));
        Ok(RawStdioSessionHandle { id, enforcement })
    }

    async fn write_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        let control = self
            .state
            .raw_stdio
            .lock()
            .expect("mobile-linux raw stdio mutex")
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| {
                MobileLinuxError::InvalidRequest("unknown raw stdio session".to_string())
            })?;
        let mut stdin_guard = control.stdin.lock().await;
        let stdin = stdin_guard.as_mut().ok_or_else(|| {
            MobileLinuxError::InvalidRequest("raw stdio stdin is closed".to_string())
        })?;
        stdin
            .write_all(&input)
            .await
            .map_err(|error| MobileLinuxError::Io(format!("write raw stdio stdin: {error}")))?;
        Ok(())
    }

    async fn read_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        max_bytes: usize,
    ) -> Result<RawStdioReadResult, MobileLinuxError> {
        let control = self
            .state
            .raw_stdio
            .lock()
            .expect("mobile-linux raw stdio mutex")
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| {
                MobileLinuxError::InvalidRequest("unknown raw stdio session".to_string())
            })?;
        {
            let mut child = control.child.lock().await;
            if let Some(status) = child
                .try_wait()
                .map_err(|error| MobileLinuxError::Io(format!("poll raw stdio child: {error}")))?
            {
                let mut guard = control.buffers.lock().expect("raw stdio buffers mutex");
                guard.closed = true;
                guard.exit_code = status.code().or(Some(-1));
                self.finish_task(
                    &handle.id,
                    &control.task,
                    if status.success() {
                        MobileLinuxTaskStatus::Completed
                    } else {
                        MobileLinuxTaskStatus::Failed
                    },
                    guard.exit_code,
                    None,
                );
            }
        }
        let mut guard = control.buffers.lock().expect("raw stdio buffers mutex");
        if guard.overflowed {
            return Err(MobileLinuxError::Io(
                "raw stdio unread output exceeded 512 KiB".to_string(),
            ));
        }
        let stdout = Self::drain_raw_stdio_bytes(&mut guard.stdout, max_bytes);
        let stderr =
            Self::drain_raw_stdio_bytes(&mut guard.stderr, max_bytes.saturating_sub(stdout.len()));
        Ok(RawStdioReadResult {
            stdout,
            stderr,
            closed: guard.closed && guard.stdout.is_empty() && guard.stderr.is_empty(),
            exit_code: guard.exit_code,
        })
    }

    async fn close_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
    ) -> Result<(), MobileLinuxError> {
        let control = self
            .state
            .raw_stdio
            .lock()
            .expect("mobile-linux raw stdio mutex")
            .remove(&handle.id)
            .ok_or_else(|| {
                MobileLinuxError::InvalidRequest("unknown raw stdio session".to_string())
            })?;
        drop(control.stdin.lock().await.take());
        let mut child = control.child.lock().await;
        let pid = child.id().ok_or_else(|| {
            MobileLinuxError::InvalidRequest("raw stdio child has no pid".to_string())
        })?;
        terminate_group(pid, Signal::SIGTERM);
        let status = match tokio::time::timeout(REAP_BUDGET, child.wait()).await {
            Ok(result) => result
                .map_err(|error| MobileLinuxError::Io(format!("wait raw stdio child: {error}")))?,
            Err(_) => {
                terminate_group(pid, Signal::SIGKILL);
                tokio::time::timeout(REAP_BUDGET, child.wait())
                    .await
                    .map_err(|_| {
                        MobileLinuxError::Io("raw stdio process tree survived SIGKILL".to_string())
                    })?
                    .map_err(|error| {
                        MobileLinuxError::Io(format!("reap raw stdio child: {error}"))
                    })?
            }
        };
        let mut guard = control.buffers.lock().expect("raw stdio buffers mutex");
        guard.closed = true;
        guard.exit_code = status.code().or(Some(-1));
        self.finish_task(
            &handle.id,
            &control.task,
            if status.success() {
                MobileLinuxTaskStatus::Completed
            } else {
                MobileLinuxTaskStatus::Cancelled
            },
            guard.exit_code,
            Some("raw stdio session closed".to_string()),
        );
        for root in &control.snapshot_roots {
            let _ = fs::remove_dir_all(root);
        }
        Ok(())
    }

    async fn write_pty(
        &self,
        handle: &PtySessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        let control = self
            .state
            .ptys
            .lock()
            .expect("mobile-linux PTY mutex")
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| MobileLinuxError::InvalidRequest("stale PTY handle".to_string()))?;
        if input == [3] {
            return control
                .process
                .signal(platform_pty::ProcessSignal::Interrupt)
                .map_err(|error| MobileLinuxError::Io(format!("interrupt PTY: {error}")));
        }
        control
            .process
            .write(input)
            .await
            .map_err(|error| MobileLinuxError::Io(format!("write PTY: {error}")))
    }

    async fn resize_pty(
        &self,
        handle: &PtySessionHandle,
        size: PtySize,
    ) -> Result<(), MobileLinuxError> {
        if size.cols == 0 || size.rows == 0 {
            return Err(MobileLinuxError::InvalidRequest(
                "PTY dimensions must be non-zero".to_string(),
            ));
        }
        let control = self
            .state
            .ptys
            .lock()
            .expect("mobile-linux PTY mutex")
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| MobileLinuxError::InvalidRequest("stale PTY handle".to_string()))?;
        control
            .process
            .resize(platform_pty::TerminalSize {
                cols: size.cols,
                rows: size.rows,
            })
            .map_err(|error| MobileLinuxError::Io(format!("resize PTY: {error}")))
    }

    async fn close_pty(&self, handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        let control = self
            .state
            .ptys
            .lock()
            .expect("mobile-linux PTY mutex")
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| MobileLinuxError::InvalidRequest("stale PTY handle".to_string()))?;
        control.task.cancel_requested.store(true, Ordering::Release);
        control.process.close_stdin();
        let _ = control
            .process
            .signal(platform_pty::ProcessSignal::Terminate);
        let deadline = tokio::time::Instant::now() + REAP_BUDGET;
        let hard_kill_at = tokio::time::Instant::now() + REAP_BUDGET / 2;
        let mut forced = false;
        while !control.task.terminal_emitted.load(Ordering::Acquire) {
            if !forced && tokio::time::Instant::now() >= hard_kill_at {
                control.process.terminate();
                forced = true;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(MobileLinuxError::Io(format!(
                    "PTY {} did not reap within 2 seconds",
                    handle.id
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        Ok(self.rootfs_snapshot())
    }

    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let manifest = self.load_rootfs_manifest()?;
        Ok(self.rootfs_store().status(&manifest))
    }

    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let manifest = self.load_rootfs_manifest()?;
        let store = self.rootfs_store();
        let status = store
            .recover_interrupted_activation(&manifest)
            .map_err(rootfs_store_error)?;
        if matches!(status.state, RootfsState::Ready) || status.staged_root.is_none() {
            return Ok(status);
        }
        store
            .activate_staged_rootfs(&manifest)
            .map_err(rootfs_store_error)
    }

    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let manifest = self.load_rootfs_manifest()?;
        self.shutdown().await?;
        let store = self.rootfs_store();
        store
            .reset_writable_state(&manifest)
            .map_err(rootfs_store_error)?;
        Ok(store.status(&manifest))
    }

    async fn configure_mounts(&self, mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        for mount in &mounts {
            validate_mount(mount, &self.state.config.managed_root)?;
        }
        *self
            .state
            .mounts
            .write()
            .expect("mobile-linux mounts rwlock") = mounts;
        Ok(())
    }

    fn current_mounts(&self) -> Vec<MountSpec> {
        self.state
            .mounts
            .read()
            .expect("mobile-linux mounts rwlock")
            .clone()
    }

    async fn read_events(
        &self,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<MobileLinuxEvent>, MobileLinuxError> {
        let after = after_sequence.unwrap_or(0);
        Ok(self
            .state
            .events
            .lock()
            .expect("mobile-linux events mutex")
            .iter()
            .filter(|event| event.sequence > after)
            .take(limit.min(platform_api::mobile_linux::MAX_MOBILE_LINUX_EVENT_BATCH))
            .cloned()
            .collect())
    }

    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        let mut tasks: Vec<_> = self
            .state
            .tasks
            .lock()
            .expect("mobile-linux tasks mutex")
            .values()
            .map(|task| task.snapshot.lock().expect("task snapshot mutex").clone())
            .collect();
        tasks.sort_by_key(|task| task.started_at_ms);
        Ok(tasks)
    }

    async fn task_status(
        &self,
        task_id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        Ok(self
            .state
            .tasks
            .lock()
            .expect("mobile-linux tasks mutex")
            .get(task_id)
            .map(|task| task.snapshot.lock().expect("task snapshot mutex").clone()))
    }
}

fn validate_request(
    request: &LinuxCommandRequest,
    isolated_local_app_build_mounts: Option<&[MountSpec]>,
) -> Result<(), MobileLinuxError> {
    if request.command.trim().is_empty() || request.command.as_bytes().contains(&0) {
        return Err(MobileLinuxError::InvalidRequest(
            "command must not be empty or contain NUL".to_string(),
        ));
    }
    if matches!(request.timeout_ms, Some(0)) {
        return Err(MobileLinuxError::InvalidRequest(
            "timeout must be greater than zero".to_string(),
        ));
    }
    let _ = requested_memory_limit_bytes(request)?;
    validate_guest_path(request.cwd.as_deref().unwrap_or("/root"))?;
    for value in &request.args {
        if value.as_bytes().contains(&0) {
            return Err(MobileLinuxError::InvalidRequest(
                "argument contains NUL".to_string(),
            ));
        }
    }
    validate_env_map(&request.env, isolated_local_app_build_mounts)
}

fn validate_pty_request(request: &PtyOpenRequest) -> Result<(), MobileLinuxError> {
    if request.command.trim().is_empty()
        || request.command.as_bytes().contains(&0)
        || request.size.cols == 0
        || request.size.rows == 0
    {
        return Err(MobileLinuxError::InvalidRequest(
            "PTY command and dimensions must be valid".to_string(),
        ));
    }
    for value in &request.args {
        if value.as_bytes().contains(&0) {
            return Err(MobileLinuxError::InvalidRequest(
                "PTY argument contains NUL".to_string(),
            ));
        }
    }
    validate_guest_path(request.cwd.as_deref().unwrap_or("/root"))?;
    validate_env_map(&request.env, None)
}

fn validate_mount(mount: &MountSpec, managed_root: &Path) -> Result<(), MobileLinuxError> {
    if !mount.host_path.is_absolute() {
        return Err(MobileLinuxError::InvalidRequest(
            "mount host path must be absolute".to_string(),
        ));
    }
    if mount.read_only {
        return Err(MobileLinuxError::InvalidRequest(
            "Android PRoot does not enforce read-only bind mounts".to_string(),
        ));
    }
    validate_guest_path(&mount.guest_path)?;
    let host = mount.host_path.canonicalize().map_err(|error| {
        MobileLinuxError::InvalidRequest(format!(
            "mount host path is unavailable ({}): {error}",
            mount.host_path.display()
        ))
    })?;
    let managed = managed_root
        .canonicalize()
        .unwrap_or_else(|_| managed_root.to_path_buf());
    if host.starts_with(&managed) || managed.starts_with(&host) {
        return Err(MobileLinuxError::InvalidRequest(
            "mount must not expose the managed rootfs".to_string(),
        ));
    }
    Ok(())
}

fn validate_isolated_local_app_mounts(
    mounts: &[MountSpec],
    managed_root: &Path,
    app_sandbox_root: &Path,
) -> Result<(), MobileLinuxError> {
    let build_mounts: Vec<_> = mounts
        .iter()
        .filter(|mount| matches!(mount.purpose, MountPurpose::LocalAppBuild))
        .collect();
    let store_mounts: Vec<_> = mounts
        .iter()
        .filter(|mount| {
            matches!(mount.purpose, MountPurpose::Shared)
                && mount.guest_path
                    == platform_api::mobile_linux::guest_paths::LOCAL_APP_DEPENDENCY_STORE
        })
        .collect();
    if build_mounts.len() != 1 || mounts.len() != 1 + store_mounts.len() || store_mounts.len() > 1 {
        return Err(MobileLinuxError::InvalidRequest(
            "isolated local-app execution requires exactly one LocalAppBuild mount and at most one validated dependency store mount".to_string(),
        ));
    }
    for mount in &store_mounts {
        let host = mount.host_path.canonicalize().map_err(|error| {
            MobileLinuxError::InvalidRequest(format!(
                "dependency store mount is unavailable ({}): {error}",
                mount.host_path.display()
            ))
        })?;
        if !host.starts_with(app_sandbox_root) {
            return Err(MobileLinuxError::InvalidRequest(
                "dependency store mount must remain inside the app sandbox".to_string(),
            ));
        }
    }
    let mount = build_mounts[0];
    let host_path = mount.host_path.canonicalize().map_err(|error| {
        MobileLinuxError::InvalidRequest(format!(
            "mount host path is unavailable ({}): {error}",
            mount.host_path.display()
        ))
    })?;
    let managed_root = managed_root
        .canonicalize()
        .unwrap_or_else(|_| managed_root.to_path_buf());
    if host_path.starts_with(&managed_root) || managed_root.starts_with(&host_path) {
        return Err(MobileLinuxError::InvalidRequest(
            "mount must not expose the managed rootfs".to_string(),
        ));
    }
    let (app_id, channel) = parse_local_app_build_guest_path(&mount.guest_path)?;
    let sandbox_root = app_sandbox_root
        .canonicalize()
        .unwrap_or_else(|_| app_sandbox_root.to_path_buf());
    let expected = sandbox_root
        .join("apps")
        .join(app_id)
        .join("build")
        .join(channel);
    let workspace = sandbox_root.join("apps").join(app_id).join("workspace");
    if host_path != workspace
        && host_path != expected
        && !local_app_build_host_path_matches(&host_path, &expected, channel)
    {
        return Err(MobileLinuxError::InvalidRequest(format!(
            "local-app build mount host_path must match {} or workspace {} or its .{channel}.staging-<numeric nonce> sibling (got {})",
            expected.display(), workspace.display(),
            host_path.display()
        )));
    }
    Ok(())
}

fn local_app_build_host_path_matches(
    host_path: &Path,
    expected_host_path: &Path,
    channel: &str,
) -> bool {
    if host_path == expected_host_path {
        return true;
    }
    if host_path.parent() != expected_host_path.parent() {
        return false;
    }
    let staging_prefix = format!(".{channel}.staging-");
    let Some(nonce) = host_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(staging_prefix.as_str()))
    else {
        return false;
    };
    !nonce.is_empty() && nonce.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_local_app_build_guest_path(path: &str) -> Result<(&str, &str), MobileLinuxError> {
    let relative = path
        .strip_prefix(platform_api::mobile_linux::guest_paths::LOCAL_APP_BUILD_ROOT)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .ok_or_else(|| {
            MobileLinuxError::InvalidRequest(format!(
                "local-app build guest_path must be {}/<app-id>/<channel>/project",
                platform_api::mobile_linux::guest_paths::LOCAL_APP_BUILD_ROOT
            ))
        })?;
    let mut segments = relative.split('/');
    let app_id = segments.next().unwrap_or_default();
    let channel = segments.next().unwrap_or_default();
    let project = segments.next().unwrap_or_default();
    if segments.next().is_some()
        || !is_valid_local_app_id(app_id)
        || !matches!(channel, "store" | "full")
        || project != platform_api::mobile_linux::guest_paths::LOCAL_APP_BUILD_PROJECT_DIR
    {
        return Err(MobileLinuxError::InvalidRequest(format!(
            "local-app build guest_path must be {}/<app-id>/<store|full>/project",
            platform_api::mobile_linux::guest_paths::LOCAL_APP_BUILD_ROOT
        )));
    }
    Ok((app_id, channel))
}

fn is_valid_local_app_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn validate_env_map(
    env: &BTreeMap<String, String>,
    isolated_local_app_build_mounts: Option<&[MountSpec]>,
) -> Result<(), MobileLinuxError> {
    let fixed_local_app_build_env = isolated_local_app_build_mounts
        .map(expected_local_app_build_env)
        .transpose()?;
    if let Some(expected_env) = fixed_local_app_build_env.as_ref() {
        for (key, expected_value) in expected_env {
            match env.get(key) {
                Some(value) if value == expected_value => {}
                Some(_) => {
                    return Err(MobileLinuxError::InvalidRequest(format!(
                        "isolated local-app build environment variable {key} must equal {expected_value}"
                    )))
                }
                None => {
                    return Err(MobileLinuxError::InvalidRequest(format!(
                        "isolated local-app build requires environment variable {key}"
                    )))
                }
            }
        }
    }
    for (key, value) in env {
        if key.is_empty() || key.contains('=') || key.as_bytes().contains(&0) {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "invalid environment variable name: {key}"
            )));
        }
        if value.as_bytes().contains(&0) {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "environment variable value contains NUL: {key}"
            )));
        }
        if let Some(expected_value) = fixed_local_app_build_env
            .as_ref()
            .and_then(|expected| expected.get(key))
        {
            debug_assert_eq!(value, expected_value);
            continue;
        }
        if is_host_reserved_env_var(key) {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "host-reserved environment variable: {key}"
            )));
        }
    }
    Ok(())
}

fn expected_local_app_build_env(
    mounts: &[MountSpec],
) -> Result<BTreeMap<String, String>, MobileLinuxError> {
    let mount = mounts
        .iter()
        .find(|mount| matches!(mount.purpose, MountPurpose::LocalAppBuild))
        .ok_or_else(|| {
            MobileLinuxError::InvalidRequest("missing LocalAppBuild mount".to_string())
        })?;
    parse_local_app_build_guest_path(&mount.guest_path)?;
    let build_state_root = format!("{}/{LOCAL_APP_BUILD_STATE_ROOT}", mount.guest_path);
    Ok(BTreeMap::from([
        ("HOME".into(), format!("{build_state_root}/home")),
        ("TMPDIR".into(), format!("{build_state_root}/tmp")),
        ("TMP".into(), format!("{build_state_root}/tmp")),
        ("TEMP".into(), format!("{build_state_root}/tmp")),
        (
            "XDG_CACHE_HOME".into(),
            format!("{build_state_root}/xdg-cache"),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            format!("{build_state_root}/xdg-config"),
        ),
        (
            "XDG_DATA_HOME".into(),
            format!("{build_state_root}/xdg-data"),
        ),
    ]))
}

fn is_host_reserved_env_var(key: &str) -> bool {
    matches!(
        key,
        "HOME"
            | "PATH"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "PROOT_LOADER"
            | "PROOT_LOADER_32"
            | "PROOT_TMP_DIR"
    )
}

fn validate_guest_path(path: &str) -> Result<(), MobileLinuxError> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(MobileLinuxError::InvalidRequest(
            "guest path must be normalized and absolute".to_string(),
        ));
    }
    Ok(())
}

fn executable_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o111 != 0
    })
}

fn path_present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.file_type().is_symlink())
}

fn android_native_library_dir() -> Option<PathBuf> {
    let maps = fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines().find_map(|line| {
        let path = line.split_whitespace().last()?;
        if !path.ends_with("/libandroid_aar.so") {
            return None;
        }
        Path::new(path).parent().map(Path::to_path_buf)
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn display_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn terminate_group(pid: u32, signal: Signal) {
    if let Ok(raw) = i32::try_from(pid) {
        let pid = Pid::from_raw(raw);
        if killpg(pid, signal).is_err() {
            let _ = kill(pid, signal);
        }
    }
}

enum ChildWaitOutcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    MemoryLimitExceeded(MemoryLimitExceededDiagnostic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemoryLimitExceededDiagnostic {
    observed_rss: u64,
    peak_rss: u64,
    limit: u64,
}

impl MemoryLimitExceededDiagnostic {
    fn summary(self) -> String {
        format!(
            "Android PRoot process group exceeded resident-memory limit: observed_rss_bytes={} peak_rss_bytes={} limit_bytes={}",
            self.observed_rss, self.peak_rss, self.limit
        )
    }

    fn detail(self) -> String {
        format!("resource_limit_exceeded: {}", self.summary())
    }
}

#[derive(Debug)]
struct MemoryWatchdog {
    limit: u64,
    peak_rss: u64,
}

impl MemoryWatchdog {
    fn new(limit_bytes: u64) -> Self {
        Self {
            limit: limit_bytes,
            peak_rss: 0,
        }
    }

    fn observe(&mut self, observed_rss_bytes: u64) -> Option<MemoryLimitExceededDiagnostic> {
        self.peak_rss = self.peak_rss.max(observed_rss_bytes);
        (observed_rss_bytes > self.limit).then_some(MemoryLimitExceededDiagnostic {
            observed_rss: observed_rss_bytes,
            peak_rss: self.peak_rss,
            limit: self.limit,
        })
    }
}

async fn wait_for_network_policy_receipt(
    child: &mut Child,
    path: &Path,
    expected_policy: &str,
) -> Result<(), MobileLinuxError> {
    let expected = format!("{expected_policy}\n");
    let deadline = tokio::time::Instant::now() + ENFORCEMENT_RECEIPT_TIMEOUT;
    loop {
        match fs::read_to_string(path) {
            Ok(value) if value == expected => return Ok(()),
            Ok(value) if !value.is_empty() => {
                return Err(MobileLinuxError::NetworkPolicyUnavailable(format!(
                    "Android policy launcher returned an invalid enforcement receipt: {value:?}"
                )));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(MobileLinuxError::NetworkPolicyUnavailable(format!(
                    "Android policy launcher receipt became unreadable: {error}"
                )));
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| MobileLinuxError::Io(format!("poll policy launcher: {error}")))?
        {
            return Err(MobileLinuxError::NetworkPolicyUnavailable(format!(
                "Android policy launcher exited before enforcement (status {status})"
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MobileLinuxError::NetworkPolicyUnavailable(
                "Android policy launcher did not prove enforcement before spawn timeout"
                    .to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn enforced_network_policy_name(policy: NetworkPolicy) -> Option<&'static str> {
    match policy {
        NetworkPolicy::Disabled => Some("disabled"),
        NetworkPolicy::LoopbackOnly => Some("loopback_only"),
        NetworkPolicy::Allowed => None,
    }
}

async fn wait_for_child(
    child: &mut Child,
    pid: u32,
    timeout: Option<Duration>,
    memory_limit_bytes: Option<u64>,
) -> Result<ChildWaitOutcome, MobileLinuxError> {
    let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
    let mut memory_watchdog = memory_limit_bytes.map(MemoryWatchdog::new);
    let mut memory_poll = tokio::time::interval(MEMORY_POLL_INTERVAL);
    memory_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = child.wait() => {
                return result
                    .map(ChildWaitOutcome::Exited)
                    .map_err(|error| MobileLinuxError::Io(format!("wait for PRoot: {error}")));
            }
            _ = memory_poll.tick(), if memory_watchdog.is_some() => {
                let resident = match process_group_rss_bytes(pid) {
                    Ok(resident) => resident,
                    Err(error) => {
                        terminate_group(pid, Signal::SIGKILL);
                        let _ = child.wait().await;
                        return Err(error);
                    }
                };
                let watchdog = memory_watchdog.as_mut().expect("guarded memory watchdog");
                if let Some(diagnostic) = watchdog.observe(resident) {
                    terminate_group(pid, Signal::SIGKILL);
                    let _ = child.wait().await;
                    return Ok(ChildWaitOutcome::MemoryLimitExceeded(diagnostic));
                }
            }
            () = async {
                if let Some(deadline) = deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                terminate_group(pid, Signal::SIGKILL);
                let _ = child.wait().await;
                return Ok(ChildWaitOutcome::TimedOut);
            }
        }
    }
}

fn ensure_process_group_rss_available() -> Result<(), MobileLinuxError> {
    if Path::new("/proc/self/stat").is_file() && Path::new("/proc/self/status").is_file() {
        Ok(())
    } else {
        Err(MobileLinuxError::ResourceLimitExceeded(
            "Android process-group RSS accounting is unavailable".to_string(),
        ))
    }
}

fn process_group_rss_bytes(process_group: u32) -> Result<u64, MobileLinuxError> {
    let entries = fs::read_dir("/proc").map_err(|error| {
        MobileLinuxError::ResourceLimitExceeded(format!(
            "read Android process table for memory watchdog: {error}"
        ))
    })?;
    let mut resident_bytes = 0_u64;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let process_dir = entry.path();
        let Ok(stat) = fs::read_to_string(process_dir.join("stat")) else {
            continue;
        };
        let Some(after_name) = stat.rsplit_once(')').map(|(_, tail)| tail.trim()) else {
            continue;
        };
        let mut fields = after_name.split_whitespace();
        let _state = fields.next();
        let _parent_pid = fields.next();
        let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        if group != process_group {
            continue;
        }
        let Ok(status) = fs::read_to_string(process_dir.join("status")) else {
            continue;
        };
        let resident_kib = status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        });
        if let Some(resident_kib) = resident_kib {
            resident_bytes = resident_bytes.saturating_add(resident_kib.saturating_mul(1024));
        } else if pid == process_group {
            return Err(MobileLinuxError::ResourceLimitExceeded(
                "Android memory watchdog could not read root process RSS".to_string(),
            ));
        }
    }
    Ok(resident_bytes)
}

fn requested_memory_limit_bytes(
    request: &LinuxCommandRequest,
) -> Result<Option<u64>, MobileLinuxError> {
    let limits = request.resource_limits;
    if limits.max_cpu_seconds.is_some()
        || limits.max_processes.is_some()
        || limits.max_open_files.is_some()
    {
        return Err(MobileLinuxError::ResourceLimitExceeded(
            "Android PRoot currently enforces only max_memory_mb for local-app commands"
                .to_string(),
        ));
    }
    match limits.max_memory_mb {
        Some(0) => Err(MobileLinuxError::InvalidRequest(
            "max_memory_mb must be greater than zero".to_string(),
        )),
        Some(megabytes) => Ok(Some(u64::from(megabytes).saturating_mul(1024 * 1024))),
        None => Ok(None),
    }
}

async fn read_stdout<R, F, Fut>(reader: R, mut on_line: F) -> Result<Vec<u8>, MobileLinuxError>
where
    R: AsyncRead + Unpin,
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), MobileLinuxError>>,
{
    let mut captured = Vec::new();
    let mut pending = Vec::new();
    let mut reader = BufReader::new(reader);
    let mut buffer = vec![0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| MobileLinuxError::Io(format!("read stdout: {error}")))?;
        if read == 0 {
            break;
        }
        append_capped(&mut captured, &buffer[..read]);
        pending.extend_from_slice(&buffer[..read]);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line = pending.drain(..=newline).collect::<Vec<_>>();
            trim_line_endings(&mut line);
            on_line(String::from_utf8_lossy(&line).into_owned()).await?;
        }
        if pending.len() >= MAX_STDOUT_FRAGMENT_BYTES {
            let fragment = std::mem::take(&mut pending);
            on_line(String::from_utf8_lossy(&fragment).into_owned()).await?;
        }
    }
    trim_line_endings(&mut pending);
    if !pending.is_empty() {
        on_line(String::from_utf8_lossy(&pending).into_owned()).await?;
    }
    Ok(captured)
}

async fn read_stderr<R, F, Fut>(mut reader: R, mut on_chunk: F) -> Result<Vec<u8>, MobileLinuxError>
where
    R: AsyncRead + Unpin,
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), MobileLinuxError>>,
{
    let mut captured = Vec::new();
    let mut buffer = vec![0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| MobileLinuxError::Io(format!("read stderr: {error}")))?;
        if read == 0 {
            break;
        }
        let chunk = buffer[..read].to_vec();
        append_capped(&mut captured, &chunk);
        on_chunk(chunk).await?;
    }
    Ok(captured)
}

async fn read_raw_stream<R, F, Fut>(mut reader: R, mut on_chunk: F) -> Result<(), MobileLinuxError>
where
    R: AsyncRead + Unpin,
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), MobileLinuxError>>,
{
    let mut buffer = vec![0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| MobileLinuxError::Io(format!("read raw stdio: {error}")))?;
        if read == 0 {
            return Ok(());
        }
        on_chunk(buffer[..read].to_vec()).await?;
    }
}

async fn join_reader(
    task: tokio::task::JoinHandle<Result<Vec<u8>, MobileLinuxError>>,
    stream: &str,
) -> Result<Vec<u8>, MobileLinuxError> {
    task.await
        .map_err(|error| MobileLinuxError::Io(format!("{stream} reader failed: {error}")))?
}

fn directory_size(path: &Path) -> std::io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut size = 0_u64;
    for entry in fs::read_dir(path)? {
        size = size.saturating_add(directory_size(&entry?.path())?);
    }
    Ok(size)
}

fn append_capped(captured: &mut Vec<u8>, chunk: &[u8]) {
    let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
    if remaining == 0 {
        return;
    }
    captured.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
}

fn trim_line_endings(line: &mut Vec<u8>) {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
}

fn rootfs_store_error(error: RootfsStoreError) -> MobileLinuxError {
    match error {
        RootfsStoreError::Manifest(error) => MobileLinuxError::Integrity(error.to_string()),
        RootfsStoreError::Io { path, message } => {
            MobileLinuxError::Io(format!("{}: {message}", path.display()))
        }
        RootfsStoreError::ArchiveSizeMismatch { expected, actual } => MobileLinuxError::Integrity(
            format!("archive size mismatch: expected {expected}, got {actual}"),
        ),
        RootfsStoreError::ArchiveHashMismatch { expected, actual } => MobileLinuxError::Integrity(
            format!("archive hash mismatch: expected {expected}, got {actual}"),
        ),
        RootfsStoreError::TargetMismatch { store, manifest } => MobileLinuxError::Integrity(
            format!("rootfs manifest target {manifest} does not match store target {store}"),
        ),
        RootfsStoreError::UnsafeResetPath(path) | RootfsStoreError::UnsafeManagedPath(path) => {
            MobileLinuxError::Integrity(format!("unsafe rootfs path: {}", path.display()))
        }
        RootfsStoreError::Integrity(message) => MobileLinuxError::Integrity(message),
        RootfsStoreError::ExecutionDenied { path, reason } => {
            MobileLinuxError::Integrity(format!("{path}: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::MountPurpose;
    use tempfile::TempDir;

    fn runtime() -> (TempDir, AndroidProotRuntime) {
        let temp = tempfile::tempdir().expect("temp");
        let managed = temp.path().join("runtime");
        fs::create_dir_all(managed.join("active/bin")).expect("active");
        fs::create_dir_all(managed.join("bin")).expect("bin");
        fs::write(managed.join("active/bin/sh"), b"#!/bin/sh\n").expect("shell");
        fs::write(
            managed.join("bin/libproot.so"),
            b"#!/bin/sh\n\
              while [ \"$#\" -gt 0 ]; do\n\
                case \"$1\" in\n\
                  -0|--link2symlink) shift ;;\n\
                  -r|-b|-w) shift 2 ;;\n\
                  *) break ;;\n\
                esac\n\
              done\n\
              exec \"$@\"\n",
        )
        .expect("proot");
        for path in [
            managed.join("active/bin/sh"),
            managed.join("bin/libproot.so"),
        ] {
            let mut permissions = fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("permissions");
        }
        let runtime = AndroidProotRuntime::new(AndroidProotRuntimeConfig {
            managed_root: managed,
            app_sandbox_root: temp.path().join("sandbox"),
            abi: "x86_64".to_string(),
            rootfs_version: "test".to_string(),
            archive_sha256: None,
        });
        (temp, runtime)
    }

    fn request() -> LinuxCommandRequest {
        LinuxCommandRequest {
            command: "/bin/true".to_string(),
            args: vec![],
            cwd: Some("/root".to_string()),
            env: BTreeMap::new(),
            stdin: None,
            // Generous on purpose. These commands are `printf`/`true`, so the
            // only thing this bound can catch is the machine being busy -- and
            // at 1000ms it did: under a full `--workspace` run the spawn alone
            // could exceed it, the runtime SIGKILLed the group as instructed,
            // and the test failed with an empty stdout that looked like a drain
            // race rather than a timeout. The one test that actually exercises
            // the timeout sets its own `timeout_ms` (50ms), so raising this
            // weakens no assertion.
            timeout_ms: Some(30_000),
            network: NetworkPolicy::Allowed,
            resource_limits: Default::default(),
            mounts: vec![],
        }
    }

    fn local_app_build_host(temp: &TempDir, app_id: &str, channel: &str) -> PathBuf {
        let path = temp
            .path()
            .join("sandbox")
            .join("apps")
            .join(app_id)
            .join("build")
            .join(channel);
        fs::create_dir_all(&path).expect("local-app build host");
        path
    }

    fn fixed_local_app_build_env(
        app_id: &str,
        channel: &str,
    ) -> (String, BTreeMap<String, String>) {
        let project_guest_path = format!("/var/lingxi/local-app-build/{app_id}/{channel}/project");
        let build_state_root = format!("{project_guest_path}/.lingxi-build-state");
        let mut env = BTreeMap::new();
        env.insert("HOME".into(), format!("{build_state_root}/home"));
        env.insert("TMPDIR".into(), format!("{build_state_root}/tmp"));
        env.insert("TMP".into(), format!("{build_state_root}/tmp"));
        env.insert("TEMP".into(), format!("{build_state_root}/tmp"));
        env.insert(
            "XDG_CACHE_HOME".into(),
            format!("{build_state_root}/xdg-cache"),
        );
        env.insert(
            "XDG_CONFIG_HOME".into(),
            format!("{build_state_root}/xdg-config"),
        );
        env.insert(
            "XDG_DATA_HOME".into(),
            format!("{build_state_root}/xdg-data"),
        );
        (project_guest_path, env)
    }

    #[test]
    fn memory_watchdog_reports_trigger_sample_peak_and_limit() {
        assert_eq!(MEMORY_POLL_INTERVAL, Duration::from_millis(250));
        let mut watchdog = MemoryWatchdog::new(10 * 1024 * 1024);
        assert!(watchdog.observe(4 * 1024 * 1024).is_none());
        assert!(watchdog.observe(9 * 1024 * 1024).is_none());
        assert_eq!(watchdog.peak_rss, 9 * 1024 * 1024);

        let exceeded = watchdog
            .observe(11 * 1024 * 1024)
            .expect("triggering sample exceeds the limit");
        assert_eq!(exceeded.observed_rss, 11 * 1024 * 1024);
        assert_eq!(exceeded.peak_rss, 11 * 1024 * 1024);
        assert_eq!(exceeded.limit, 10 * 1024 * 1024);
        let detail = exceeded.detail();
        assert!(detail.starts_with("resource_limit_exceeded:"));
        assert!(detail.contains("observed_rss_bytes=11534336"));
        assert!(detail.contains("peak_rss_bytes=11534336"));
        assert!(detail.contains("limit_bytes=10485760"));
        let error = MobileLinuxError::ResourceLimitExceeded(exceeded.summary()).to_string();
        assert_eq!(error.matches("resource_limit_exceeded:").count(), 1);
        assert!(error.contains("observed_rss_bytes=11534336"));
        assert!(error.contains("peak_rss_bytes=11534336"));
        assert!(error.contains("limit_bytes=10485760"));
    }

    #[test]
    fn every_positive_max_memory_mb_value_is_supported() {
        let mut request = request();
        for megabytes in [1_u32, 17, u32::MAX] {
            request.resource_limits.max_memory_mb = Some(megabytes);
            assert_eq!(
                requested_memory_limit_bytes(&request).expect("positive memory limit"),
                Some(u64::from(megabytes) * 1024 * 1024),
            );
        }
    }

    #[tokio::test]
    async fn missing_payload_fails_closed_without_legacy_fallback() {
        let temp = tempfile::tempdir().expect("temp");
        let runtime = AndroidProotRuntime::new(AndroidProotRuntimeConfig {
            managed_root: temp.path().join("missing"),
            app_sandbox_root: temp.path().join("sandbox"),
            abi: "x86_64".to_string(),
            rootfs_version: "test".to_string(),
            archive_sha256: None,
        });
        let capability = runtime.probe_capability().await;
        assert!(!capability.available);
        assert_eq!(capability.backend, SandboxBackend::AndroidProot);
        assert!(matches!(
            runtime.run(request()).await,
            Err(MobileLinuxError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn mount_validation_rejects_managed_root_and_traversal() {
        let (_temp, runtime) = runtime();
        let result = runtime
            .configure_mounts(vec![MountSpec {
                host_path: runtime.state.config.managed_root.clone(),
                guest_path: "/workspace/../root".to_string(),
                read_only: false,
                purpose: MountPurpose::Workspace,
            }])
            .await;
        assert!(matches!(result, Err(MobileLinuxError::InvalidRequest(_))));
    }

    #[test]
    fn merged_execution_mounts_keep_configured_binds() {
        let (temp, runtime) = runtime();
        let workspace_host = temp.path().join("workspace");
        let build_host = local_app_build_host(&temp, "app", "store");
        fs::create_dir_all(&workspace_host).expect("workspace host");
        runtime
            .state
            .mounts
            .write()
            .expect("mounts rwlock")
            .push(MountSpec {
                host_path: workspace_host,
                guest_path: "/workspace/default".to_string(),
                read_only: false,
                purpose: MountPurpose::Workspace,
            });
        let mounts = runtime
            .execution_mounts(
                &[MountSpec {
                    host_path: build_host,
                    guest_path: "/var/lingxi/local-app-build/app/store/project".to_string(),
                    read_only: false,
                    purpose: MountPurpose::LocalAppBuild,
                }],
                ForegroundMountMode::Merged,
            )
            .expect("merged mounts");
        assert_eq!(mounts.len(), 2);
        assert!(mounts
            .iter()
            .any(|mount| mount.guest_path == "/workspace/default"));
        assert!(mounts.iter().any(|mount| {
            mount.guest_path == "/var/lingxi/local-app-build/app/store/project"
                && matches!(mount.purpose, MountPurpose::LocalAppBuild)
        }));
    }

    #[test]
    fn isolated_execution_mounts_drop_configured_binds_but_keep_request_mounts() {
        let (temp, runtime) = runtime();
        let workspace_host = temp.path().join("workspace");
        let build_host = local_app_build_host(&temp, "app", "store");
        fs::create_dir_all(&workspace_host).expect("workspace host");
        runtime
            .state
            .mounts
            .write()
            .expect("mounts rwlock")
            .push(MountSpec {
                host_path: workspace_host,
                guest_path: "/workspace/default".to_string(),
                read_only: false,
                purpose: MountPurpose::Workspace,
            });
        let mounts = runtime
            .execution_mounts(
                &[MountSpec {
                    host_path: build_host,
                    guest_path: "/var/lingxi/local-app-build/app/store/project".to_string(),
                    read_only: false,
                    purpose: MountPurpose::LocalAppBuild,
                }],
                ForegroundMountMode::RequestOnly,
            )
            .expect("isolated mounts");
        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].guest_path,
            "/var/lingxi/local-app-build/app/store/project"
        );
        assert!(matches!(mounts[0].purpose, MountPurpose::LocalAppBuild));
    }

    #[test]
    fn isolated_execution_mounts_require_exactly_one_local_app_build_mount() {
        let (temp, runtime) = runtime();
        let build_host = local_app_build_host(&temp, "app", "store");
        let extra_host = temp.path().join("workspace");
        fs::create_dir_all(&extra_host).expect("workspace host");

        let error = runtime
            .execution_mounts(
                &[
                    MountSpec {
                        host_path: build_host.clone(),
                        guest_path: "/var/lingxi/local-app-build/app/store/project".to_string(),
                        read_only: false,
                        purpose: MountPurpose::LocalAppBuild,
                    },
                    MountSpec {
                        host_path: extra_host,
                        guest_path: "/workspace/default".to_string(),
                        read_only: false,
                        purpose: MountPurpose::Workspace,
                    },
                ],
                ForegroundMountMode::RequestOnly,
            )
            .expect_err("extra mounts must be rejected");
        assert!(error
            .to_string()
            .contains("exactly one LocalAppBuild mount"));
    }

    #[test]
    fn isolated_execution_mounts_reject_wrong_host_or_guest_shape() {
        let (temp, runtime) = runtime();
        let wrong_guest_host = local_app_build_host(&temp, "app", "store");
        let error = runtime
            .execution_mounts(
                &[MountSpec {
                    host_path: wrong_guest_host,
                    guest_path: "/workspace/default".to_string(),
                    read_only: false,
                    purpose: MountPurpose::LocalAppBuild,
                }],
                ForegroundMountMode::RequestOnly,
            )
            .expect_err("wrong guest path must be rejected");
        assert!(error.to_string().contains("guest_path must be"));

        let wrong_host = temp
            .path()
            .join("sandbox")
            .join("apps")
            .join("other")
            .join("build")
            .join("store");
        fs::create_dir_all(&wrong_host).expect("wrong host");
        let error = runtime
            .execution_mounts(
                &[MountSpec {
                    host_path: wrong_host,
                    guest_path: "/var/lingxi/local-app-build/app/store/project".to_string(),
                    read_only: false,
                    purpose: MountPurpose::LocalAppBuild,
                }],
                ForegroundMountMode::RequestOnly,
            )
            .expect_err("wrong host app id must be rejected");
        assert!(error.to_string().contains(&format!(
            "{}/apps/app/build/store",
            runtime.state.config.app_sandbox_root.display()
        )));
    }

    #[test]
    fn isolated_execution_mounts_reject_same_suffix_outside_app_sandbox_root() {
        let (temp, runtime) = runtime();
        let wrong_host = temp
            .path()
            .join("other-root")
            .join("apps")
            .join("app")
            .join("build")
            .join("store");
        fs::create_dir_all(&wrong_host).expect("wrong host");

        let error = runtime
            .execution_mounts(
                &[MountSpec {
                    host_path: wrong_host,
                    guest_path: "/var/lingxi/local-app-build/app/store/project".to_string(),
                    read_only: false,
                    purpose: MountPurpose::LocalAppBuild,
                }],
                ForegroundMountMode::RequestOnly,
            )
            .expect_err("same suffix outside sandbox root must be rejected");
        assert!(error.to_string().contains(&format!(
            "{}/apps/app/build/store",
            runtime.state.config.app_sandbox_root.display()
        )));
    }

    #[test]
    fn raw_stdio_read_only_mounts_use_isolated_snapshots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("workspace");
        let snapshot = temp.path().join("snapshots/raw-1");
        fs::create_dir_all(source.join("src")).expect("source");
        fs::create_dir_all(source.join("node_modules/react")).expect("node_modules");
        fs::write(
            source.join("src/app.jsx"),
            b"// @ts-check\nconst value = 1;\n",
        )
        .expect("source file");
        fs::write(source.join("node_modules/react/package.json"), b"{}").expect("dependency file");
        let (mounts, roots) = snapshot_read_only_mounts(
            vec![MountSpec {
                host_path: source.clone(),
                guest_path: "/workspace/lingxi-lsp-test".to_string(),
                read_only: true,
                purpose: MountPurpose::External,
            }],
            &snapshot,
        )
        .expect("snapshot mount");

        assert!(!mounts[0].read_only);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[1].host_path, source.join("node_modules"));
        assert_eq!(
            mounts[1].guest_path,
            "/workspace/lingxi-lsp-test/node_modules"
        );
        assert_eq!(roots, vec![snapshot.clone()]);
        fs::write(mounts[0].host_path.join("src/app.jsx"), b"changed in guest")
            .expect("mutate snapshot");
        assert_eq!(
            fs::read(source.join("src/app.jsx")).expect("read source"),
            b"// @ts-check\nconst value = 1;\n"
        );
    }

    #[tokio::test]
    async fn event_reads_are_ordered_and_exclusive() {
        let (_temp, runtime) = runtime();
        runtime.emit(
            None,
            MobileLinuxEventKind::RuntimeError { detail: "a".into() },
        );
        runtime.emit(
            None,
            MobileLinuxEventKind::RuntimeError { detail: "b".into() },
        );
        let first = runtime.read_events(None, 1).await.expect("events");
        let rest = runtime
            .read_events(Some(first[0].sequence), 10)
            .await
            .expect("events");
        assert_eq!(first.len(), 1);
        assert_eq!(rest.len(), 1);
    }

    #[tokio::test]
    async fn run_keeps_stdout_and_stderr_separate_and_emits_one_terminal_state() {
        let (_temp, runtime) = runtime();
        let mut request = request();
        request.command = "/bin/sh".to_string();
        request.args = vec![
            "-c".to_string(),
            "printf out; printf err >&2; exit 7".to_string(),
        ];
        let result = runtime.run(request).await.expect("run");
        assert_eq!(result.stdout, "out");
        assert_eq!(result.stderr, "err");
        assert_eq!(result.exit_code, 7);
        assert_eq!(result.enforcement, LinuxEnforcementReceipt::default());
        let tasks = runtime.list_tasks().await.expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, MobileLinuxTaskStatus::Failed);
        let terminal_events = runtime
            .read_events(None, 100)
            .await
            .expect("events")
            .into_iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    MobileLinuxEventKind::TaskStatusChanged {
                        status: MobileLinuxTaskStatus::Completed
                            | MobileLinuxTaskStatus::Failed
                            | MobileLinuxTaskStatus::Cancelled
                            | MobileLinuxTaskStatus::TimedOut,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(terminal_events, 1);
    }

    #[tokio::test]
    async fn disabled_network_fails_before_guest_spawn_without_policy_launcher() {
        let (_temp, runtime) = runtime();
        let mut request = request();
        request.network = NetworkPolicy::Disabled;
        let error = runtime
            .run(request)
            .await
            .expect_err("launcher is required");
        assert!(matches!(
            error,
            MobileLinuxError::NetworkPolicyUnavailable(_)
        ));
        let tasks = runtime.list_tasks().await.expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, MobileLinuxTaskStatus::Failed);
    }

    #[tokio::test]
    async fn loopback_network_fails_before_guest_spawn_without_policy_launcher() {
        let (_temp, runtime) = runtime();
        let mut request = request();
        request.network = NetworkPolicy::LoopbackOnly;
        let error = runtime
            .run(request)
            .await
            .expect_err("launcher and PRoot extension are required");
        assert!(matches!(
            error,
            MobileLinuxError::NetworkPolicyUnavailable(_)
        ));
        let tasks = runtime.list_tasks().await.expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, MobileLinuxTaskStatus::Failed);
    }

    #[test]
    fn loopback_only_is_admitted_for_sockaddr_aware_proot_enforcement() {
        let mut request = request();
        request.network = NetworkPolicy::LoopbackOnly;
        validate_request(&request, None).expect("LoopbackOnly is supported");
        assert_eq!(
            enforced_network_policy_name(request.network),
            Some("loopback_only")
        );
    }

    #[test]
    fn ordinary_requests_reject_build_state_env_overrides() {
        let mut request = request();
        request.env.insert(
            "HOME".into(),
            "/var/lingxi/local-app-build/app/store/project/.lingxi-build-state/home".into(),
        );
        let error =
            validate_request(&request, None).expect_err("ordinary requests must reject HOME");
        assert!(error
            .to_string()
            .contains("host-reserved environment variable"));
    }

    #[tokio::test]
    async fn isolated_local_app_build_accepts_fixed_build_env() {
        let (temp, runtime) = runtime();
        let build_host = local_app_build_host(&temp, "app", "store");
        let (project_guest_path, env) = fixed_local_app_build_env("app", "store");
        let mut request = request();
        request.command = "/bin/sh".into();
        request.args = vec![
            "-c".into(),
            "printf '%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n%s' \
$HOME \"$TMPDIR\" \"$TMP\" \"$TEMP\" \"$XDG_CACHE_HOME\" \"$XDG_CONFIG_HOME\" \
\"$XDG_DATA_HOME\""
                .into(),
        ];
        request.cwd = Some(project_guest_path.clone());
        request.env = env.clone();
        request.mounts = vec![MountSpec {
            host_path: build_host,
            guest_path: project_guest_path,
            read_only: false,
            purpose: MountPurpose::LocalAppBuild,
        }];

        let result = runtime.run_isolated(request).await.expect("isolated run");
        let stdout_lines: Vec<_> = result.stdout.lines().collect();
        assert_eq!(stdout_lines.len(), env.len());
        let expected = [
            "HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ]
        .into_iter()
        .map(|key| {
            env.get(key)
                .expect("fixed local-app build env key")
                .as_str()
        })
        .collect::<Vec<_>>();
        assert_eq!(stdout_lines, expected);
    }

    #[test]
    fn isolated_local_app_build_rejects_incomplete_fixed_build_env() {
        let (project_guest_path, fixed_env) = fixed_local_app_build_env("app", "store");
        for missing_key in fixed_env.keys() {
            let mut request = request();
            request.cwd = Some(project_guest_path.clone());
            request.env = fixed_env.clone();
            request.env.remove(missing_key);
            request.mounts = vec![MountSpec {
                host_path: PathBuf::from("/tmp/lingxi-local-app-build"),
                guest_path: project_guest_path.clone(),
                read_only: false,
                purpose: MountPurpose::LocalAppBuild,
            }];

            let error = validate_request(&request, Some(&request.mounts))
                .expect_err("isolated builds require the complete fixed environment");
            assert!(error.to_string().contains(&format!(
                "isolated local-app build requires environment variable {missing_key}"
            )));
        }
    }

    #[tokio::test]
    async fn timeout_reaps_group_and_next_command_runs() {
        let (_temp, runtime) = runtime();
        let mut slow = request();
        slow.command = "/bin/sh".to_string();
        slow.args = vec!["-c".to_string(), "sleep 30".to_string()];
        slow.timeout_ms = Some(50);
        let timed_out = runtime.run(slow).await.expect("timeout result");
        assert!(timed_out.timed_out);

        let mut next = request();
        next.command = "/bin/sh".to_string();
        next.args = vec!["-c".to_string(), "printf ready".to_string()];
        let next = runtime.run(next).await.expect("next run");
        assert_eq!(next.stdout, "ready");
        assert_eq!(next.exit_code, 0);
    }

    #[tokio::test]
    async fn background_kill_is_cancelled_and_idempotent() {
        let (_temp, runtime) = runtime();
        let mut request = request();
        request.command = "/bin/sh".to_string();
        request.args = vec!["-c".to_string(), "sleep 30".to_string()];
        let handle = runtime.spawn_background(request).await.expect("background");
        runtime.kill(&handle).await.expect("kill");
        runtime.kill(&handle).await.expect("idempotent kill");
        let task = runtime
            .task_status(&handle.id)
            .await
            .expect("status")
            .expect("task");
        assert_eq!(task.status, MobileLinuxTaskStatus::Cancelled);
    }

    #[test]
    fn terminal_transition_is_emitted_once() {
        let (_temp, runtime) = runtime();
        let (id, task) = runtime.create_task("task", "true".into(), MobileLinuxTaskStatus::Running);
        runtime.finish_task(&id, &task, MobileLinuxTaskStatus::Completed, Some(0), None);
        runtime.finish_task(&id, &task, MobileLinuxTaskStatus::Cancelled, None, None);
        let terminal = runtime
            .state
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    MobileLinuxEventKind::TaskStatusChanged {
                        status: MobileLinuxTaskStatus::Completed
                            | MobileLinuxTaskStatus::Failed
                            | MobileLinuxTaskStatus::Cancelled
                            | MobileLinuxTaskStatus::TimedOut,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(terminal, 1);
    }
}
