//! Shared Linux lifecycle coordination. Every product runtime adapter is wrapped
//! once; chat, PTYs, app processes and native maintenance use the same gate.
use mobile_linux_api::*;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

static GATES: LazyLock<Mutex<HashMap<PathBuf, Weak<Gate>>>> = LazyLock::new(Mutex::default);
type RuntimeRegistration = (Weak<dyn MobileLinuxRuntime>, Weak<CoordinatedRuntime>);
static WRAPPERS: LazyLock<Mutex<Vec<RuntimeRegistration>>> = LazyLock::new(Mutex::default);
static EXECUTOR: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("linux-lifecycle")
        .build()
        .expect("Linux lifecycle executor")
});

struct Gate {
    lock: Arc<RwLock<()>>,
    apps: Mutex<BTreeMap<String, serde_json::Value>>,
    cleanup: Mutex<BTreeMap<String, String>>,
}
impl Gate {
    fn busy(&self) -> MobileLinuxError {
        MobileLinuxError::InvalidRequest(format!(
            "runtimeBusy: stop shared Linux tasks before maintenance; {}",
            serde_json::json!({"apps": self.apps.lock().unwrap().values().collect::<Vec<_>>(),"pendingCleanup":*self.cleanup.lock().unwrap()})
        ))
    }
    fn read(self: &Arc<Self>) -> Result<OwnedRwLockReadGuard<()>, MobileLinuxError> {
        self.lock.clone().try_read_owned().map_err(|_| self.busy())
    }
}

/// A run retains this lease until all owned processes are successfully reaped.
/// It blocks global mutation even between install, build and service startup.
pub struct AppExecutionLease {
    _read: OwnedRwLockReadGuard<()>,
    gate: Arc<Gate>,
    run_id: String,
}
impl Drop for AppExecutionLease {
    fn drop(&mut self) {
        self.gate.apps.lock().unwrap().remove(&self.run_id);
    }
}
pub fn app_execution_lease(
    runtime: &Arc<dyn MobileLinuxRuntime>,
    app_id: &str,
    run_id: &str,
) -> Result<Option<AppExecutionLease>, MobileLinuxError> {
    let registrations = WRAPPERS.lock().unwrap();
    let gate = registrations.iter().find_map(|(registered, wrapper)| {
        registered
            .upgrade()
            .filter(|value| Arc::ptr_eq(value, runtime))
            .and_then(|_| wrapper.upgrade())
            .map(|wrapper| wrapper.gate.clone())
    });
    drop(registrations);
    let Some(gate) = gate else {
        return Ok(None);
    }; // isolated host test adapters
    let read = gate.read()?;
    gate.apps.lock().unwrap().insert(
        run_id.into(),
        serde_json::json!({"appId":app_id,"runId":run_id}),
    );
    Ok(Some(AppExecutionLease {
        _read: read,
        gate,
        run_id: run_id.into(),
    }))
}

/// `managed_root` identifies the existing Linux user space, never a project or
/// Firebase account. Wrappers for different workspaces share its lifecycle gate.
pub fn coordinate_runtime(
    managed_root: &Path,
    runtime: Arc<dyn MobileLinuxRuntime>,
) -> Arc<dyn MobileLinuxRuntime> {
    fn canonical_key(path: &Path) -> PathBuf {
        if let Ok(path) = path.canonicalize() {
            return path;
        }
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => canonical_key(parent).join(name),
            _ => path.to_path_buf(),
        }
    }
    let key = canonical_key(managed_root);
    let mut gates = GATES.lock().unwrap();
    let gate = gates.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
        let gate = Arc::new(Gate {
            lock: Arc::new(RwLock::new(())),
            apps: Mutex::new(BTreeMap::new()),
            cleanup: Mutex::new(BTreeMap::new()),
        });
        gates.insert(key, Arc::downgrade(&gate));
        gate
    });
    drop(gates);
    let coordinated = Arc::new(CoordinatedRuntime {
        inner: runtime,
        gate,
    });
    let wrapper: Arc<dyn MobileLinuxRuntime> = coordinated.clone();
    let mut wrappers = WRAPPERS.lock().unwrap();
    wrappers.retain(|(runtime, _)| runtime.strong_count() > 0);
    wrappers.push((Arc::downgrade(&wrapper), Arc::downgrade(&coordinated)));
    wrapper
}

/// Keep host archive staging and SDK activation in one global maintenance
/// operation. The independent owner retains the lease if a native UI caller
/// disappears while its foreign staging callback is still writing files.
pub async fn repair_rootfs_with_preparation<F, Fut>(
    runtime: Arc<dyn MobileLinuxRuntime>,
    prepare: F,
) -> Result<RootfsStatus, MobileLinuxError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), MobileLinuxError>> + Send + 'static,
{
    let wrapper = WRAPPERS
        .lock()
        .unwrap()
        .iter()
        .find_map(|(registered, wrapper)| {
            registered
                .upgrade()
                .filter(|value| Arc::ptr_eq(value, &runtime))
                .and_then(|_| wrapper.upgrade())
        })
        .ok_or_else(|| {
            MobileLinuxError::InvalidRequest(
                "runtime is not coordinated for native rootfs maintenance".into(),
            )
        })?;
    EXECUTOR
        .spawn(async move {
            let _write = wrapper.maintain().await?;
            // status() may fall back to shell presence if its manifest is
            // missing. Only full verification can skip immutable restaging.
            if matches!(
                wrapper.inner.verify_rootfs().await,
                Ok(RootfsStatus {
                    state: RootfsState::Ready,
                    ..
                })
            ) {
                return wrapper.inner.repair_rootfs().await;
            }
            prepare().await?;
            wrapper.inner.repair_rootfs().await
        })
        .await
        .map_err(join_error)?
}

struct CoordinatedRuntime {
    inner: Arc<dyn MobileLinuxRuntime>,
    gate: Arc<Gate>,
}
impl CoordinatedRuntime {
    async fn maintain(&self) -> Result<OwnedRwLockWriteGuard<()>, MobileLinuxError> {
        let write = self
            .gate
            .lock
            .clone()
            .try_write_owned()
            .map_err(|_| self.gate.busy())?;
        let tasks: Vec<_> = self
            .inner
            .list_tasks()
            .await?
            .into_iter()
            .filter(|task| {
                matches!(
                    task.status,
                    MobileLinuxTaskStatus::Queued
                        | MobileLinuxTaskStatus::Running
                        | MobileLinuxTaskStatus::Backgrounded
                )
            })
            .collect();
        if !tasks.is_empty() {
            return Err(MobileLinuxError::InvalidRequest(format!(
                "runtimeBusy: stop shared Linux tasks before maintenance; {}",
                serde_json::json!({"tasks":tasks})
            )));
        }
        Ok(write)
    }
}

// A canceled caller must not release the gate while a native SDK spawn is
// still in flight. If its result is abandoned, reap that exact returned handle.
struct Handoff<T: Send + 'static> {
    value: Option<T>,
    read: Option<OwnedRwLockReadGuard<()>>,
    cleanup: Option<
        Box<
            dyn FnOnce(T) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
        >,
    >,
}
impl<T: Send + 'static> Handoff<T> {
    fn take(mut self) -> T {
        self.value.take().unwrap()
    }
}
impl<T: Send + 'static> Drop for Handoff<T> {
    fn drop(&mut self) {
        if let (Some(value), Some(cleanup)) = (self.value.take(), self.cleanup.take()) {
            let read = self.read.take();
            EXECUTOR.spawn(async move {
                cleanup(value).await;
                drop(read);
            });
        }
    }
}
fn join_error(error: tokio::task::JoinError) -> MobileLinuxError {
    MobileLinuxError::Io(error.to_string())
}

// A failed reap retains its exact handle and lease, rather than losing the
// only cleanup owner. Pending handles are included in maintenance diagnostics.
async fn retry_reap<F, Fut>(gate: Arc<Gate>, id: String, mut reap: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), MobileLinuxError>>,
{
    loop {
        match reap().await {
            Ok(()) => {
                gate.cleanup.lock().unwrap().remove(&id);
                return;
            }
            Err(error) => {
                gate.cleanup
                    .lock()
                    .unwrap()
                    .insert(id.clone(), error.to_string());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[async_trait::async_trait]
impl MobileLinuxRuntime for CoordinatedRuntime {
    fn backend(&self) -> SandboxBackend {
        self.inner.backend()
    }
    fn mode(&self) -> MobileLinuxRuntimeMode {
        self.inner.mode()
    }
    async fn probe_capability(&self) -> MobileLinuxCapability {
        self.inner.probe_capability().await
    }
    async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let _read = self.gate.read()?;
        self.inner.boot().await
    }
    async fn shutdown(&self) -> Result<(), MobileLinuxError> {
        let _write = self.maintain().await?;
        self.inner.shutdown().await
    }
    async fn run(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        let read = self.gate.read()?;
        let inner = self.inner.clone();
        EXECUTOR
            .spawn(async move {
                let _read = read;
                inner.run(request).await
            })
            .await
            .map_err(join_error)?
    }
    async fn run_streaming(
        &self,
        request: LinuxCommandRequest,
        sink: Arc<dyn ProcessStreamSink>,
    ) -> Result<LinuxCommandResult, MobileLinuxError> {
        let read = self.gate.read()?;
        let inner = self.inner.clone();
        EXECUTOR
            .spawn(async move {
                let _read = read;
                inner.run_streaming(request, sink).await
            })
            .await
            .map_err(join_error)?
    }
    async fn spawn_background(
        &self,
        request: LinuxCommandRequest,
    ) -> Result<LinuxProcessHandle, MobileLinuxError> {
        let read = self.gate.read()?;
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        let handoff = EXECUTOR
            .spawn(async move {
                let value = inner.spawn_background(request).await?;
                Ok::<_, MobileLinuxError>(Handoff {
                    value: Some(value),
                    read: Some(read),
                    cleanup: Some(Box::new(move |handle| {
                        Box::pin(async move {
                            retry_reap(gate, handle.id.clone(), || {
                                let inner = inner.clone();
                                let handle = handle.clone();
                                async move { inner.kill(&handle).await }
                            })
                            .await;
                        })
                    })),
                })
            })
            .await
            .map_err(join_error)??;
        Ok(handoff.take())
    }
    async fn kill(&self, handle: &LinuxProcessHandle) -> Result<(), MobileLinuxError> {
        self.inner.kill(handle).await
    }
    async fn open_pty(
        &self,
        request: PtyOpenRequest,
    ) -> Result<PtySessionHandle, MobileLinuxError> {
        let read = self.gate.read()?;
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        let handoff = EXECUTOR
            .spawn(async move {
                let value = inner.open_pty(request).await?;
                Ok::<_, MobileLinuxError>(Handoff {
                    value: Some(value),
                    read: Some(read),
                    cleanup: Some(Box::new(move |handle| {
                        Box::pin(async move {
                            retry_reap(gate, handle.id.clone(), || {
                                let inner = inner.clone();
                                let handle = handle.clone();
                                async move { inner.close_pty(&handle).await }
                            })
                            .await;
                        })
                    })),
                })
            })
            .await
            .map_err(join_error)??;
        Ok(handoff.take())
    }
    async fn write_pty(
        &self,
        handle: &PtySessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        self.inner.write_pty(handle, input).await
    }
    async fn resize_pty(
        &self,
        handle: &PtySessionHandle,
        size: PtySize,
    ) -> Result<(), MobileLinuxError> {
        self.inner.resize_pty(handle, size).await
    }
    async fn close_pty(&self, handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
        self.inner.close_pty(handle).await
    }
    async fn open_raw_stdio(
        &self,
        request: RawStdioOpenRequest,
    ) -> Result<RawStdioSessionHandle, MobileLinuxError> {
        let read = self.gate.read()?;
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        let handoff = EXECUTOR
            .spawn(async move {
                let value = inner.open_raw_stdio(request).await?;
                Ok::<_, MobileLinuxError>(Handoff {
                    value: Some(value),
                    read: Some(read),
                    cleanup: Some(Box::new(move |handle| {
                        Box::pin(async move {
                            retry_reap(gate, handle.id.clone(), || {
                                let inner = inner.clone();
                                let handle = handle.clone();
                                async move { inner.close_raw_stdio(&handle).await }
                            })
                            .await;
                        })
                    })),
                })
            })
            .await
            .map_err(join_error)??;
        Ok(handoff.take())
    }
    async fn write_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        input: Vec<u8>,
    ) -> Result<(), MobileLinuxError> {
        self.inner.write_raw_stdio(handle, input).await
    }
    async fn read_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
        max_bytes: usize,
    ) -> Result<RawStdioReadResult, MobileLinuxError> {
        self.inner.read_raw_stdio(handle, max_bytes).await
    }
    async fn close_raw_stdio(
        &self,
        handle: &RawStdioSessionHandle,
    ) -> Result<(), MobileLinuxError> {
        self.inner.close_raw_stdio(handle).await
    }
    async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
        self.inner.rootfs_status().await
    }
    async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let _read = self.gate.read()?;
        self.inner.verify_rootfs().await
    }
    async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let _write = self.maintain().await?;
        self.inner.repair_rootfs().await
    }
    async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
        let _write = self.maintain().await?;
        self.inner.reset_rootfs().await
    }
    async fn configure_mounts(&self, mounts: Vec<MountSpec>) -> Result<(), MobileLinuxError> {
        if self.inner.current_mounts() == mounts {
            return Ok(());
        }
        let _write = self.maintain().await?;
        self.inner.configure_mounts(mounts).await
    }
    fn current_mounts(&self) -> Vec<MountSpec> {
        self.inner.current_mounts()
    }
    async fn read_events(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<MobileLinuxEvent>, MobileLinuxError> {
        self.inner.read_events(after, limit).await
    }
    async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        self.inner.list_tasks().await
    }
    async fn task_status(
        &self,
        task_id: &str,
    ) -> Result<Option<MobileLinuxTaskSnapshot>, MobileLinuxError> {
        self.inner.task_status(task_id).await
    }
}
