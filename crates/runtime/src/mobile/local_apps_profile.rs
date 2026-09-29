//! Process-wide, profile-keyed local-app service registry.

use crate::mobile::local_apps_host::LocalAppsHostBroker;
use crate::mobile::local_apps_llm::LocalAppsLlm;
use async_trait::async_trait;
use client::adapter::ClientEventSink;
use client::protocol::events::ClientEvent;
use lingxi_core::host::{Clock, MobileLinuxRuntime};
use local_apps::{AppError, AppEventFanout, AppService};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use tokio::sync::OnceCell;

type ProfileCell = Arc<OnceCell<Arc<ProfileApps>>>;

const MAX_PROFILE_CACHE_ENTRIES: usize = 8;

struct RegistryEntry {
    cell: ProfileCell,
    last_used: u64,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, RegistryEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, RegistryEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_registry_stamp() -> u64 {
    static STAMP: AtomicU64 = AtomicU64::new(1);
    STAMP.fetch_add(1, Ordering::Relaxed)
}

/// The long-lived runtime every `ProfileApps`-owned task runs on.
///
/// `ProfileApps` is cached process-wide, but the tasks it owns are `tokio::spawn`ed
/// onto the AMBIENT runtime — the static server and the full-runtime exit
/// watch in the broker. For the first engine that ambient runtime is the
/// `MobileEngineHandle`-owned one, which the next reconnect or project switch
/// drops while the cached profile survives: a `Running` entry would outlive
/// the socket it describes. Anchoring the load here anchors them all.
pub(crate) fn worker_runtime() -> &'static tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("lingxi-local-apps")
                .build()
                .expect("build the local-app worker runtime")
        })
        .handle()
}

pub(crate) struct ClientEventFanout {
    next_id: AtomicU64,
    sinks: Mutex<HashMap<u64, Weak<dyn ClientEventSink>>>,
}

impl ClientEventFanout {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sinks: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn subscribe(&self, sink: Arc<dyn ClientEventSink>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sinks
            .lock()
            .expect("local-app client fanout poisoned")
            .insert(id, Arc::downgrade(&sink));
        id
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        self.sinks
            .lock()
            .expect("local-app client fanout poisoned")
            .remove(&id);
    }

    fn subscriber_count(&self) -> usize {
        let mut sinks = self.sinks.lock().expect("local-app client fanout poisoned");
        sinks.retain(|_, sink| sink.strong_count() > 0);
        sinks.len()
    }
}

#[async_trait]
impl ClientEventSink for ClientEventFanout {
    async fn emit(&self, event: ClientEvent) {
        let sinks = {
            let mut registrations = self.sinks.lock().expect("local-app client fanout poisoned");
            let mut live = Vec::with_capacity(registrations.len());
            registrations.retain(|_, sink| match sink.upgrade() {
                Some(sink) => {
                    live.push(sink);
                    true
                }
                None => false,
            });
            live
        };
        for sink in sinks {
            sink.emit(event.clone()).await;
        }
    }
}

/// Swappable holder for a profile's [`LocalAppsLlm`].
///
/// `ProfileApps` is cached process-wide (see [`profile_apps`]'s `OnceCell`),
/// but its `llm` is NOT — every consumer that reaches the model (the
/// broker's `llm.chat` bridge operation) reads through this cell instead of
/// holding its own `Arc<LocalAppsLlm>`. `clock` / `mobile_linux` stay
/// genuinely pinned to whichever connection first loaded the profile (their
/// doc above explains why); the model is different: it carries auth, and a
/// RECONNECT (a fresh `MobileEngineHandle` — possibly rotated credentials,
/// possibly a different `ApiService`) hits this exact cache-hit path with a
/// brand-new `LocalAppsLlm`. Pinning that silently would mean app-initiated
/// calls keep authenticating as a rotated-out credential with no error and
/// no log — so [`profile_apps`] refreshes this cell on every call, cached
/// hit or not.
///
/// This does NOT cover a live `/model` switch — `ClientCommand::SetModel`
/// never rebuilds the engine, so it never reaches `profile_apps` at all.
/// That path is fixed separately and more narrowly: `ApiServiceModel`
/// (`local_apps_llm.rs`) holds its own model/profile behind a lock and
/// `SetModel`'s handler mutates it in place via
/// [`crate::mobile::local_apps_llm::LocalAppsModel::set_model`] — the SAME
/// `Arc<LocalAppsLlm>` this cell holds for the connection's lifetime, no
/// swap needed.
pub(crate) struct SharedLlm(RwLock<Arc<LocalAppsLlm>>);

impl SharedLlm {
    pub(crate) fn new(llm: Arc<LocalAppsLlm>) -> Self {
        Self(RwLock::new(llm))
    }

    /// The current model. Read fresh on every use (not cached by the
    /// caller) so a swap takes effect for the very next LLM call, including
    /// one already in flight when the swap lands but that has not yet
    /// reached the model.
    pub(crate) fn current(&self) -> Arc<LocalAppsLlm> {
        self.0.read().expect("shared llm poisoned").clone()
    }

    /// Swap in a new model — called by [`profile_apps`] with the calling
    /// connection's own `ApiService`-backed model.
    pub(crate) fn replace(&self, llm: Arc<LocalAppsLlm>) {
        *self.0.write().expect("shared llm poisoned") = llm;
    }
}

pub(crate) struct ProfileApps {
    pub(crate) service: Arc<AppService>,
    pub(crate) host: Arc<LocalAppsHostBroker>,
    pub(crate) domain_events: Arc<AppEventFanout>,
    pub(crate) client_events: Arc<ClientEventFanout>,
    pub(crate) llm: Arc<SharedLlm>,
    /// Refreshed on every [`profile_apps`] call for the same reason as
    /// `llm`: the handles are one connection's Swift/Kotlin objects.
    pub(crate) device: Arc<crate::mobile::local_apps_device::SharedDeviceCapabilities>,
}

impl ProfileApps {
    async fn load(
        root: PathBuf,
        clock: Arc<dyn Clock>,
        mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
        full_runtime: bool,
        runtime_root: Option<PathBuf>,
        physical_memory_bytes: u64,
        llm: Arc<LocalAppsLlm>,
        devices: crate::mobile::local_apps_device::DeviceCapabilities,
    ) -> Result<Arc<Self>, AppError> {
        let llm = Arc::new(SharedLlm::new(llm));
        let device =
            Arc::new(crate::mobile::local_apps_device::SharedDeviceCapabilities::new(devices));
        let client_events = Arc::new(ClientEventFanout::new());
        // Dependency updates publish a durable journal before touching the
        // manifest, dependency record or promoted build. Recovery must finish
        // before AppService loads those files into memory; continuing after a
        // failed restore would make a torn transaction look authoritative.
        let root_is_real_directory = match std::fs::symlink_metadata(&root) {
            Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(AppError::Io(format!(
                    "inspect Local App profile root {}: {error}",
                    root.display()
                )))
            }
        };
        if root_is_real_directory {
            LocalAppsHostBroker::recover_dependency_updates_on_boot(&root).map_err(|error| {
                AppError::StorageCorrupt(format!(
                    "recover interrupted Local App dependency update: {error}"
                ))
            })?;
        }
        let host = LocalAppsHostBroker::new_with_physical_memory(
            root.clone(),
            client_events.clone(),
            mobile_linux.clone(),
            full_runtime,
            runtime_root,
            physical_memory_bytes,
        );
        let domain_events = Arc::new(AppEventFanout::new());
        let service = Arc::new(AppService::load(root, clock, domain_events.clone()).await?);
        host.attach_service(service.clone())
            .map_err(|_| AppError::Io("local-app host was already attached".into()))?;
        host.attach_llm(llm.clone())
            .map_err(|_| AppError::Io("local-app host llm was already attached".into()))?;
        host.attach_device(device.clone())
            .map_err(|_| AppError::Io("local-app host device was already attached".into()))?;
        Ok(Arc::new(Self {
            service,
            host,
            domain_events,
            client_events,
            llm,
            device,
        }))
    }
}

pub(crate) async fn profile_apps(
    root: PathBuf,
    clock: Arc<dyn Clock>,
    mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    full_runtime: bool,
    runtime_root: Option<PathBuf>,
    physical_memory_bytes: u64,
    llm: Arc<LocalAppsLlm>,
    devices: crate::mobile::local_apps_device::DeviceCapabilities,
) -> Result<Arc<ProfileApps>, AppError> {
    let cell = {
        let mut profiles = registry()
            .lock()
            .expect("local-app profile registry poisoned");
        let stamp = next_registry_stamp();
        let cell = {
            let entry = profiles
                .entry(root.clone())
                .or_insert_with(|| RegistryEntry {
                    cell: Arc::new(OnceCell::new()),
                    last_used: stamp,
                });
            entry.last_used = stamp;
            entry.cell.clone()
        };
        cell
    };
    // Clone every connection/build-scoped input BEFORE the (maybe-never-run)
    // init closure consumes its copy. A cached profile retains the durable
    // service and runtime table, but must not retain stale client handles or
    // runtime capability metadata across engine reconnects.
    let refresh_llm = llm.clone();
    let refresh_devices = devices.clone();
    let refresh_mobile_linux = mobile_linux.clone();
    let refresh_runtime_root = runtime_root.clone();
    let load_root = root.clone();
    let profile = match cell
        .get_or_try_init(|| async move {
            worker_runtime()
                .spawn(ProfileApps::load(
                    load_root,
                    clock,
                    mobile_linux,
                    full_runtime,
                    runtime_root,
                    physical_memory_bytes,
                    llm,
                    devices,
                ))
                .await
                .map_err(|error| AppError::Io(format!("local-app profile load failed: {error}")))?
        })
        .await
    {
        Ok(profile) => profile.clone(),
        Err(error) => {
            let mut profiles = registry()
                .lock()
                .expect("local-app profile registry poisoned");
            if profiles
                .get(&root)
                .is_some_and(|entry| Arc::ptr_eq(&entry.cell, &cell))
            {
                profiles.remove(&root);
            }
            // Preserve typed load errors (notably `StorageCorrupt`) so the
            // engine can degrade without turning a recoverable app-store
            // diagnosis into a generic I/O failure. Add context only to
            // untyped I/O failures raised while opening the profile.
            let error = match error {
                AppError::Io(message) if message.starts_with("local-app profile load failed:") => {
                    AppError::Io(message)
                }
                AppError::Io(message) => {
                    AppError::Io(format!("local-app profile load failed: {message}"))
                }
                other => other,
            };
            return Err(error);
        }
    };
    profile.host.refresh_runtime_configuration(
        refresh_mobile_linux,
        refresh_runtime_root,
        physical_memory_bytes,
    );
    profile.llm.replace(refresh_llm);
    profile.device.replace(refresh_devices);
    evict_idle_profiles(&root).await;
    Ok(profile)
}

async fn evict_idle_profiles(current_root: &PathBuf) {
    loop {
        let candidates = {
            let profiles = registry()
                .lock()
                .expect("local-app profile registry poisoned");
            if profiles.len() <= MAX_PROFILE_CACHE_ENTRIES {
                return;
            }
            let mut candidates: Vec<_> = profiles
                .iter()
                .filter(|(path, entry)| {
                    path.as_path() != current_root.as_path()
                        && entry.cell.get().is_some_and(|profile| {
                            Arc::strong_count(profile) == 1
                                && profile.client_events.subscriber_count() == 0
                                && profile.domain_events.subscriber_count() == 0
                        })
                })
                .filter_map(|(path, entry)| {
                    entry
                        .cell
                        .get()
                        .map(|profile| (path.clone(), Arc::clone(profile), entry.last_used))
                })
                .collect();
            candidates.sort_by_key(|(_, _, last_used)| *last_used);
            candidates
        };
        if candidates.is_empty() {
            return;
        }
        let mut removed = false;
        for (path, profile, _) in candidates {
            if profile.host.has_active_runtimes().await {
                continue;
            }
            let mut profiles = registry()
                .lock()
                .expect("local-app profile registry poisoned");
            let should_remove = profiles.get(&path).is_some_and(|entry| {
                entry.cell.get().is_some_and(|current| {
                    Arc::ptr_eq(current, &profile)
                            // `profile` is the candidate snapshot held by this
                            // eviction pass; a count of two means the registry
                            // and this pass are the only owners. Re-checking
                            // under the registry lock prevents a reconnect
                            // that acquired the cached Arc between the first
                            // snapshot and removal from being evicted.
                            && Arc::strong_count(current) == 2
                            && current.client_events.subscriber_count() == 0
                            && current.domain_events.subscriber_count() == 0
                })
            });
            if should_remove {
                profiles.remove(&path);
                removed = true;
                break;
            }
        }
        if !removed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile::local_apps_llm::test_support::ScriptedModel;
    use local_apps::test_support::FixedClock;

    /// The test below never reaches the LLM, so an empty script is enough —
    /// any call would fail loudly with "ran out of responses" rather than
    /// silently returning something plausible.
    fn no_op_llm() -> Arc<LocalAppsLlm> {
        Arc::new(LocalAppsLlm::new(ScriptedModel::new()))
    }

    #[tokio::test]
    async fn same_profile_root_reuses_one_app_service() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("profile");
        let first = profile_apps(
            root.clone(),
            Arc::new(FixedClock::new(1_000)),
            None,
            false,
            None,
            0,
            no_op_llm(),
            crate::mobile::local_apps_device::DeviceCapabilities::default(),
        )
        .await
        .expect("first profile");
        let second = profile_apps(
            root,
            Arc::new(FixedClock::new(2_000)),
            None,
            false,
            None,
            0,
            no_op_llm(),
            crate::mobile::local_apps_device::DeviceCapabilities::default(),
        )
        .await
        .expect("second profile");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first.service, &second.service));
    }

    #[tokio::test]
    async fn failed_profile_load_does_not_leave_a_registry_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("not-a-directory");
        std::fs::write(&root, b"file").expect("file root");
        let error = match profile_apps(
            root.clone(),
            Arc::new(FixedClock::new(1_000)),
            None,
            false,
            None,
            0,
            no_op_llm(),
            crate::mobile::local_apps_device::DeviceCapabilities::default(),
        )
        .await
        {
            Ok(_) => panic!("file root must fail to load"),
            Err(error) => error,
        };
        assert_eq!(error.code(), local_apps::AppErrorCode::Io);
        assert!(!registry().lock().expect("registry").contains_key(&root));
    }

    #[tokio::test]
    async fn idle_profile_registry_stays_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..(MAX_PROFILE_CACHE_ENTRIES + 2) {
            let profile = profile_apps(
                temp.path().join(format!("profile-{index}")),
                Arc::new(FixedClock::new(1_000)),
                None,
                false,
                None,
                0,
                no_op_llm(),
                crate::mobile::local_apps_device::DeviceCapabilities::default(),
            )
            .await
            .expect("profile");
            drop(profile);
        }
        assert!(registry().lock().expect("registry").len() <= MAX_PROFILE_CACHE_ENTRIES);
    }

    #[tokio::test]
    async fn cached_profile_refreshes_runtime_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("profile");
        let runtime_root = temp.path().join("runtime");
        std::fs::create_dir_all(runtime_root.join("node_modules/vite/bin"))
            .expect("create Vite runtime fixture");
        std::fs::write(
            runtime_root.join("node_modules/vite/bin/vite.js"),
            "fixture",
        )
        .expect("write Vite fixture");

        let first = profile_apps(
            root.clone(),
            Arc::new(FixedClock::new(1_000)),
            None,
            false,
            None,
            128,
            no_op_llm(),
            crate::mobile::local_apps_device::DeviceCapabilities::default(),
        )
        .await
        .expect("first profile");
        let second = profile_apps(
            root,
            Arc::new(FixedClock::new(2_000)),
            None,
            true,
            Some(runtime_root.clone()),
            256,
            no_op_llm(),
            crate::mobile::local_apps_device::DeviceCapabilities::default(),
        )
        .await
        .expect("second profile");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.host.physical_memory_bytes(), 256);
        assert_eq!(
            second
                .host
                .await_fixed_runtime_root(std::time::Duration::ZERO)
                .await
                .expect("refreshed runtime root"),
            runtime_root
        );
    }
}
