use super::required_string;
use super::static_server::bind_stable_loopback;
use super::static_server::reconcile_static_runtime_exit;
use super::static_server::run_static_server;
use super::static_server::sibling_pinned_ports;
use super::LocalAppsHostBroker;
use super::RuntimeEntry;
use super::RuntimeEntryState;
use super::RuntimeHandle;
use super::RuntimePublicationCell;
use super::RuntimePublicationIdentity;
use super::RuntimeReservation;
use super::RuntimeStartStatus;
use local_apps::AppRuntimeMode;
use local_apps::AppRuntimeState;
use local_apps::AppService;
use serde_json::json;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::sync::watch;

impl LocalAppsHostBroker {
    pub(crate) async fn manage_runtime_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let action = required_string(&input, "action")?;
        match action {
            "start" | "open" | "resume" => self.start_runtime(&app_id).await,
            "stop" | "suspend" => self.stop_runtime(&app_id).await,
            "restart" => {
                self.stop_runtime(&app_id).await?;
                self.start_runtime(&app_id).await
            }
            _ => {
                Err("runtime action must be start, stop, restart, open, suspend, or resume".into())
            }
        }
    }
    pub(super) async fn start_runtime(&self, app_id: &str) -> Result<Value, String> {
        let service = self.service()?;
        service
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let requested_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start requires an active build".to_string())?;
        let publication_cell = self.runtime_publication_cell(app_id)?;
        let access_tick = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut wait_for_start = None;
        let mut return_running = false;
        let mut reserved_generation = None;
        {
            let mut runtimes = self.runtimes.lock().await;
            if let Some(entry) = runtimes.get_mut(app_id) {
                entry.last_used = access_tick;
                match &entry.state {
                    RuntimeEntryState::Starting { gate } => {
                        wait_for_start = Some(gate.subscribe());
                    }
                    RuntimeEntryState::Running { .. } => {
                        if entry.build_id != requested_build_id {
                            return Err(
                                "runtime_stale_build: running runtime does not serve the active build; stop and restart it"
                                    .into(),
                            );
                        }
                        return_running = true;
                    }
                }
            } else {
                let generation = self.next_request_id.fetch_add(1, Ordering::Relaxed);
                let (gate, _) = watch::channel(RuntimeStartStatus::Pending);
                *publication_cell
                    .write()
                    .map_err(|_| "runtime publication identity is poisoned".to_string())? = None;
                runtimes.insert(
                    app_id.to_string(),
                    RuntimeEntry {
                        state: RuntimeEntryState::Starting { gate },
                        last_used: access_tick,
                        generation,
                        build_id: requested_build_id.clone(),
                    },
                );
                reserved_generation = Some(generation);
            }
        }
        if let Some(receiver) = wait_for_start {
            return self.wait_for_runtime_start(app_id, receiver).await;
        }
        if return_running {
            let runtime = service
                .runtime_record(app_id)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(json!({
                "app_id": app_id,
                "state": runtime.state,
                "url": runtime.port.map(|port| format!("http://127.0.0.1:{port}")),
                "build_id": requested_build_id,
            }));
        }
        let Some(generation) = reserved_generation else {
            return Err("runtime start reservation disappeared before completion".into());
        };
        self.start_reserved_runtime(app_id, generation).await
    }
    pub(super) async fn wait_for_runtime_start(
        &self,
        app_id: &str,
        mut receiver: watch::Receiver<RuntimeStartStatus>,
    ) -> Result<Value, String> {
        loop {
            let status = receiver.borrow_and_update().clone();
            match status {
                RuntimeStartStatus::Pending => {
                    receiver
                        .changed()
                        .await
                        .map_err(|_| "runtime start was cancelled".to_string())?;
                }
                RuntimeStartStatus::Running => {
                    let runtime = self
                        .service()?
                        .runtime_record(app_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    let build_id =
                        crate::mobile::local_apps_build::active_build_id(&self.layout(app_id)?)
                            .map_err(|error| error.to_string())?;
                    return Ok(json!({
                        "app_id": app_id,
                        "state": runtime.state,
                        "url": runtime.port.map(|port| format!("http://127.0.0.1:{port}")),
                        "build_id": build_id,
                    }));
                }
                RuntimeStartStatus::Failed(detail) => return Err(detail),
            }
        }
    }
    pub(super) async fn start_reserved_runtime(
        &self,
        app_id: &str,
        generation: u64,
    ) -> Result<Value, String> {
        let _reservation = RuntimeReservation {
            runtimes: Arc::clone(&self.runtimes),
            app_id: app_id.to_string(),
            generation,
        };
        let service = self.service()?;
        let layout = self.layout(app_id)?;
        let expected_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start requires an active build".to_string())?;
        let reservation_matches = {
            let runtimes = self.runtimes.lock().await;
            runtimes.get(app_id).is_some_and(|entry| {
                entry.generation == generation && entry.build_id == expected_build_id
            })
        };
        if !reservation_matches {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    None,
                    "runtime start build identity changed before launch".into(),
                )
                .await;
        }
        let manifest = local_apps::load_manifest(&layout).map_err(|error| error.to_string())?;
        if !manifest.runtime_api_compatible() {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    None,
                    format!(
                        "runtime_api_incompatible: app manifest targets runtime API v{}; regenerate or rebuild this app for v{}",
                        manifest.runtime_api_version,
                        local_apps::RUNTIME_API_MAJOR
                    ),
                )
                .await;
        }
        if let Err(error) = crate::mobile::local_apps_build::validate_build_for_launch(&layout) {
            return self
                .fail_reserved_runtime_start(app_id, generation, None, error.to_string())
                .await;
        }
        let static_root = layout
            .root()
            .join(layout.build_rel(false))
            .join(crate::mobile::local_apps_build::VITE_OUTPUT_DIR);
        if !static_root.join("index.html").is_file() {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    None,
                    "static build output is missing index.html; generate the app first".into(),
                )
                .await;
        }
        let current = service
            .runtime_record(app_id)
            .await
            .map_err(|e| e.to_string())?;
        // The listener has to be registered with the SAME immortal I/O driver
        // that will serve it.  `start_reserved_runtime` runs on the AMBIENT
        // runtime — for an agent-driven `manage_runtime {action:"start"}` that
        // is the per-engine one — and tokio invalidates every resource
        // registered with a dropped runtime's driver, so a listener bound here
        // fails every later `accept()` forever behind an entry that still
        // reports `running`.
        let bound = {
            let app_id = app_id.to_string();
            let assigned = current.port;
            let leases = Arc::clone(&self.port_leases);
            let registry = Arc::clone(&service);
            // Everything from the pin read to the choice is one step against
            // this broker's other ALLOCATIONS — but only against those: a
            // sibling that is already past its own allocation still persists
            // its pin and releases its lease inside this window, which is why
            // the choice is re-checked against a fresh read below rather than
            // trusted because the gate is held.  See `port_allocation`.
            let _allocation = self.port_allocation.lock().await;
            // The FIRST read is taken on THIS runtime, before the hop: the
            // derivation has to know which ports stopped siblings own
            // permanently, which no bind probe on the worker runtime can
            // discover.  The re-read after the lease runs on the worker
            // runtime, where it is an ordinary `AppService` read — no runtime
            // affinity, nothing blocking.
            let sibling_pins = sibling_pinned_ports(&service, &app_id).await;
            crate::mobile::local_apps_profile::worker_runtime()
                .spawn(async move {
                    bind_stable_loopback(&app_id, assigned, &sibling_pins, &leases, &registry).await
                })
                .await
                .map_err(|error| format!("bind stable app port: {error}"))?
        };
        let (listener, port, port_lease) = match bound {
            Ok(bound) => bound,
            Err(error) => {
                return self
                    .fail_reserved_runtime_start(app_id, generation, current.port, error)
                    .await;
            }
        };
        let latest_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start lost its active build".to_string())?;
        if latest_build_id != expected_build_id {
            drop(port_lease);
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    "runtime start build identity changed while binding".into(),
                )
                .await;
        }
        if let Err(error) = service
            .set_runtime_mode(app_id, AppRuntimeMode::StaticExport)
            .await
        {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    format!("persist runtime mode: {error}"),
                )
                .await;
        }
        if let Err(error) = service
            .update_runtime_record(app_id, AppRuntimeState::Starting, Some(port), None, None)
            .await
        {
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    format!("persist starting runtime state: {error}"),
                )
                .await;
        }
        // The pin is durable HERE and not one line earlier: `with_app` has
        // already written the new record back under the state lock by the time
        // it returns, so from this point `sibling_pinned_ports` reports the
        // port for every later allocator and the lease has nothing left to
        // cover.  Every path above this line drops the guard instead, which
        // returns the port to the pool.
        //
        // That ORDER — persist, THEN release — is load-bearing beyond tidiness:
        // it is the whole premise of `bind_stable_loopback`'s post-lease pin
        // re-read.  A release moved above the persist would leave a port that
        // is in neither the records nor the leases, which is exactly the hole
        // both mechanisms exist to close.
        if let Some(lease) = port_lease {
            lease.commit();
        }

        let publication_cell = self.runtime_publication_cell(app_id)?;
        let (shutdown, receiver) = oneshot::channel();
        self.spawn_static_server(
            service.clone(),
            app_id.to_string(),
            generation,
            publication_cell.clone(),
            listener,
            static_root,
            receiver,
        );
        let handle = RuntimeHandle::Static { shutdown };
        if let Err(error) = service
            .update_runtime_record(app_id, AppRuntimeState::Running, Some(port), None, None)
            .await
            .map_err(|error| error.to_string())
        {
            self.cleanup_runtime_handle(handle).await;
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    format!("persist running runtime state: {error}"),
                )
                .await;
        }
        let latest_build_id = crate::mobile::local_apps_build::active_build_id(&layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "runtime start lost its active build".to_string())?;
        if latest_build_id != expected_build_id {
            self.cleanup_runtime_handle(handle).await;
            return self
                .fail_reserved_runtime_start(
                    app_id,
                    generation,
                    Some(port),
                    "runtime start build identity changed before commit".into(),
                )
                .await;
        }
        let gate = {
            let mut runtimes = self.runtimes.lock().await;
            let Some(entry) = runtimes.get_mut(app_id) else {
                self.cleanup_runtime_handle(handle).await;
                return Err("runtime start reservation disappeared before completion".into());
            };
            if entry.generation != generation {
                self.cleanup_runtime_handle(handle).await;
                return Err("runtime start reservation changed before completion".into());
            }
            let mut publication_identity = publication_cell
                .write()
                .map_err(|_| "runtime publication identity is poisoned".to_string())?;
            let previous =
                std::mem::replace(&mut entry.state, RuntimeEntryState::Running { handle });
            let gate = match previous {
                RuntimeEntryState::Starting { gate } => gate,
                RuntimeEntryState::Running { handle } => {
                    entry.state = RuntimeEntryState::Running { handle };
                    return Err("runtime start reservation was already resolved".into());
                }
            };
            *publication_identity = Some(RuntimePublicationIdentity {
                generation,
                build_id: expected_build_id.clone(),
            });
            gate
        };
        let _ = gate.send(RuntimeStartStatus::Running);
        Ok(json!({
            "app_id": app_id,
            "state": "running",
            "url": format!("http://127.0.0.1:{port}"),
            "build_id": expected_build_id,
            "runtime_generation": generation,
        }))
    }
    pub(super) async fn stop_runtime(&self, app_id: &str) -> Result<Value, String> {
        let service = self.service()?;
        service
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let runtime_generation = self
            .runtime_identity(app_id)
            .await?
            .map(|(generation, _)| generation);
        self.release_app_runtime_state(app_id, runtime_generation)
            .await;
        let publication_cell = self.runtime_publication_cell(app_id)?;
        // Classify and remove under ONE acquisition: a start woken in the gap
        // between a `remove` and its rollback `insert` finds no entry, kills the
        // runtime it just spawned and returns without resolving the gate,
        // leaving a reservation nothing can ever complete.
        let handle = {
            let mut runtimes = self.runtimes.lock().await;
            let mut publication_identity = publication_cell
                .write()
                .map_err(|_| "runtime publication identity is poisoned".to_string())?;
            match runtimes.get(app_id).map(|entry| &entry.state) {
                None => {
                    *publication_identity = None;
                    return Ok(json!({"app_id": app_id, "state": "stopped"}));
                }
                Some(RuntimeEntryState::Starting { .. }) => {
                    return Err("runtime is still starting; retry stop shortly".into());
                }
                Some(RuntimeEntryState::Running { .. }) => {}
            }
            match runtimes.remove(app_id).map(|entry| entry.state) {
                Some(RuntimeEntryState::Running { handle }) => {
                    *publication_identity = None;
                    handle
                }
                _ => return Ok(json!({"app_id": app_id, "state": "stopped"})),
            }
        };
        let runtime = service
            .runtime_record(app_id)
            .await
            .map_err(|e| e.to_string())?;
        service
            .update_runtime_record(
                app_id,
                AppRuntimeState::Stopping,
                runtime.port,
                runtime.pid,
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        match handle {
            RuntimeHandle::Static { shutdown } => {
                let _ = shutdown.send(());
            }
        }
        service
            .update_runtime_record(app_id, AppRuntimeState::Stopped, runtime.port, None, None)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({"app_id": app_id, "state": "stopped"}))
    }
    pub(super) async fn fail_reserved_runtime_start(
        &self,
        app_id: &str,
        generation: u64,
        port: Option<u16>,
        detail: String,
    ) -> Result<Value, String> {
        let gate = {
            let mut runtimes = self.runtimes.lock().await;
            let matches_generation = runtimes.get(app_id).is_some_and(|entry| {
                entry.generation == generation
                    && matches!(entry.state, RuntimeEntryState::Starting { .. })
            });
            if !matches_generation {
                None
            } else {
                match runtimes.remove(app_id) {
                    Some(RuntimeEntry {
                        state: RuntimeEntryState::Starting { gate },
                        ..
                    }) => Some(gate),
                    Some(entry) => {
                        runtimes.insert(app_id.to_string(), entry);
                        None
                    }
                    None => None,
                }
            }
        };
        if let Some(gate) = gate {
            let _ = gate.send(RuntimeStartStatus::Failed(detail.clone()));
        }
        // Same rule as `stop_runtime`'s failed kill: this bookkeeping write must
        // never mask the real failure, so its result stays discarded.
        //
        // Three callers reach here BEFORE the record leaves `stopped` (no
        // runtime mount, no static build, and the squatted permanent port).
        // `stopped -> failed` is now a legal edge (`local_apps::state::
        // runtime_transition_allowed`), added precisely so this write lands:
        // while it was rejected, the state, the `lastError`, and the
        // `RuntimeChanged` event were all discarded, leaving an app that could
        // not start and carried no recorded reason. The detail also reaches
        // every concurrent waiter through the gate above, and `failed ->
        // starting` keeps the record recoverable.
        if let Ok(service) = self.service() {
            let _ = service
                .update_runtime_record(
                    app_id,
                    AppRuntimeState::Failed,
                    port,
                    None,
                    Some(detail.clone()),
                )
                .await;
        }
        Err(detail)
    }
    pub(super) async fn cleanup_runtime_handle(&self, handle: RuntimeHandle) {
        match handle {
            RuntimeHandle::Static { shutdown } => {
                let _ = shutdown.send(());
            }
        }
    }
    /// Everything an app's runtime owned that must not outlive it.
    ///
    /// Called from EVERY way a runtime can end — the explicit stop, the Full
    /// handle's exit watch, and the static listener's reconciliation — not
    /// just the one the user drives. A crashed app used to keep the iOS
    /// audio-session lease open with nothing left able to release it, which
    /// takes FlowMode, hold-to-talk and transcribeSpeech down with it for the
    /// life of the process.
    ///
    /// Session grants go too: the user answered "allow for this session"
    /// while USING the app, and a grant that quietly survives the app's death
    /// behaves as "always allow" while staying invisible to permissions.json
    /// and unrevokable short of a full reset.
    pub(crate) async fn release_app_runtime_state(
        &self,
        app_id: &str,
        runtime_generation: Option<u64>,
    ) {
        if let Some(generation) = runtime_generation {
            self.cancel_local_app_audio_scope(app_id, generation);
            self.force_stop_recording(app_id, generation).await;
        }
        self.clear_media(app_id);
        self.session_permissions.lock().await.revoke_app(app_id);
    }
    /// Return the Host-owned runtime generation and the build provenance that
    /// was selected when it was started.  The stable loopback port is omitted
    /// deliberately: it preserves the app's IndexedDB origin and cannot prove
    /// which promoted build a native WebView currently displays.
    pub(crate) async fn runtime_identity(
        &self,
        app_id: &str,
    ) -> Result<Option<(u64, String)>, String> {
        let runtimes = self.runtimes.lock().await;
        Ok(runtimes.get(app_id).and_then(|entry| {
            matches!(entry.state, RuntimeEntryState::Running { .. })
                .then(|| (entry.generation, entry.build_id.clone()))
        }))
    }
    /// Serves the static export on the anchored runtime, and reconciles the
    /// record when the LISTENER dies the way the Full handle's exit watch does.
    /// A static handle has no process to poll, so without this a retired server
    /// leaves `Running { Static }` in the map and `start_runtime`'s
    /// `return_running` short-circuit keeps handing out a URL nothing answers.
    ///
    /// `service` is passed in rather than re-resolved: the only caller already
    /// holds it, and a bail-out here would drop the listener while the entry
    /// went on to report `running`.
    pub(super) fn spawn_static_server(
        &self,
        service: Arc<AppService>,
        app_id: String,
        generation: u64,
        publication_cell: RuntimePublicationCell,
        listener: TcpListener,
        root: PathBuf,
        shutdown: oneshot::Receiver<()>,
    ) {
        let runtimes = Arc::clone(&self.runtimes);
        let broker = self.weak_self();
        crate::mobile::local_apps_profile::worker_runtime().spawn(async move {
            let Some(detail) = run_static_server(listener, root, shutdown).await else {
                return;
            };
            reconcile_static_runtime_exit(
                runtimes,
                service,
                app_id,
                generation,
                publication_cell,
                detail,
                broker,
            )
            .await;
        });
    }
}
