use super::{gateway, now_ms, ActiveRun, MobileLocalAppServiceHandle, ServiceError};
use local_app::v2::{AppService, Entry, Process, RunPhase, RunPreparation, Step};
use mobile_linux_api::{
    LinuxCommandRequest, MobileLinuxEventKind, MobileLinuxTaskStatus, NetworkPolicy, ResourceLimits,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

impl MobileLocalAppServiceHandle {
    pub(super) async fn prepare_and_launch(
        self: &Arc<Self>,
        preparation: &RunPreparation,
        run: Arc<ActiveRun>,
    ) -> Result<Value, ServiceError> {
        let manifest = &preparation.release.manifest;
        let origin = format!("http://127.0.0.1:{}", preparation.record.origin_port);
        let run_id = preparation.run.run_id.clone();
        let prefix = format!("/__run/{run_id}/");
        // Reserve the persisted browser origin before running any install code.
        // Failure is explicit; changing it would strand origin-scoped data.
        let replacing_run = run.previous.lock().unwrap().is_some();
        let listener = if replacing_run {
            None
        } else {
            Some(Arc::new(
                TcpListener::bind((
                    std::net::Ipv4Addr::LOCALHOST,
                    preparation.record.origin_port,
                ))
                .await
                .map_err(|e| {
                    ServiceError::new(
                        "originPortUnavailable",
                        format!("{}: {e}", preparation.record.origin_port),
                    )
                })?,
            ))
        };
        let mut ports = BTreeMap::new();
        if manifest.requires_execution() {
            let library = self.library.clone();
            let id = preparation.record.app_id.clone();
            let revision = preparation.release.revision;
            tokio::task::spawn_blocking(move || library.require_execution_grant(&id, revision))
                .await
                .map_err(super::join_error)??;
            let runtime = self.runtime()?;
            *run.execution_lease.lock().unwrap() =
                super::super::linux_maintenance::app_execution_lease(
                    &runtime,
                    &preparation.record.app_id,
                    &run_id,
                )?;
            let capability = runtime.probe_capability().await;
            if !capability.available || !capability.background_processes {
                return Err(ServiceError::new(
                    "runtimeUnavailable",
                    capability
                        .reason
                        .unwrap_or_else(|| "Linux background process support required".into()),
                ));
            }
            runtime.boot().await?;
            // Run output only ever includes this app's registered process IDs.
            let owner = self.clone();
            let logging = run.clone();
            *run.log_task.lock().await = Some(tokio::spawn(async move {
                owner.collect_logs(logging).await;
            }));
            for step in manifest
                .install
                .iter()
                .chain(&manifest.build)
                .filter(|_| !preparation.environment_prepared)
            {
                self.step(preparation, &run, step, None, &ports).await?;
            }
            let mut started = BTreeSet::new();
            while started.len() < manifest.processes.len() {
                for process in &manifest.processes {
                    if started.contains(&process.name)
                        || !process.depends_on.iter().all(|name| started.contains(name))
                    {
                        continue;
                    }
                    if run.cancel.is_cancelled() {
                        return Err(ServiceError::new(
                            "cancelled",
                            "application startup cancelled",
                        ));
                    }
                    let reservation = TcpListener::bind("127.0.0.1:0").await?;
                    let port = reservation.local_addr()?.port();
                    ports.insert(process.name.clone(), port);
                    drop(reservation);
                    let step = Step {
                        command: process.command.clone(),
                        cwd: process.cwd.clone(),
                        env: process.env.clone(),
                        venv: process.venv.clone(),
                        python: process.python.clone(),
                    };
                    self.ensure_venv(preparation, &run, &step, &ports).await?;
                    let request = self.command(preparation, &step, Some(port), &ports)?;
                    let handle = self.spawn_owned(&run, request).await?;
                    run.service_tasks.lock().unwrap().push(handle.id.clone());
                    self.ready(&run, process, port, &handle.id).await?;
                    started.insert(process.name.clone());
                }
            }
        }
        if let Entry::Server { process, .. } = &manifest.entry {
            let port = *ports.get(process).ok_or_else(|| {
                ServiceError::new("serviceUnavailable", "entry service was not registered")
            })?;
            if TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .is_err()
            {
                return Err(ServiceError::new(
                    "serviceUnavailable",
                    "entry process did not listen on its allocated loopback port",
                ));
            }
        }
        if run.cancel.is_cancelled() {
            return Err(ServiceError::new(
                "cancelled",
                "application startup cancelled",
            ));
        }
        if run.cancel.is_cancelled() {
            return Err(ServiceError::new(
                "cancelled",
                "application startup cancelled",
            ));
        }
        let (entry, entry_service) = match &manifest.entry {
            Entry::Static { path } => {
                let root = Path::new(&preparation.work_path).canonicalize()?;
                let file = root.join(path).canonicalize()?;
                if !file.starts_with(&root) || !file.is_file() {
                    return Err(ServiceError::new(
                        "entryUnavailable",
                        "static entry is not a contained file after building",
                    ));
                }
                (path.clone(), None)
            }
            Entry::Server { process, path } => (
                path.clone()
                    .unwrap_or_else(|| "/".into())
                    .trim_start_matches('/')
                    .to_string(),
                Some(process.clone()),
            ),
        };
        let previous = run.previous.lock().unwrap().clone();
        let listener = match listener {
            Some(listener) => listener,
            None => previous
                .as_ref()
                .expect("replacement owner")
                .listener
                .lock()
                .await
                .clone()
                .ok_or_else(|| {
                    ServiceError::new(
                        "originPortUnavailable",
                        "previous application gateway is unavailable",
                    )
                })?,
        };
        let url = format!("{origin}{prefix}{entry}");
        let route = gateway::Route {
            host: Some(gateway::BrowserHost {
                owner: Arc::downgrade(self),
                app_id: preparation.record.app_id.clone(),
                name: preparation.record.name.clone(),
                revision: preparation.release.revision,
                platform: self.target.lock().unwrap().platform.clone(),
            }),
            run_id: run_id.clone(),
            root: PathBuf::from(&preparation.work_path),
            entry,
            entry_service,
            services: ports.clone(),
            port: preparation.record.origin_port,
            capabilities: manifest.capabilities.clone(),
            cancel: run.cancel.clone(),
        };
        let mut record = run.record.lock().unwrap().clone();
        record.phase = RunPhase::Running;
        record.services = ports
            .iter()
            .map(|(name, port)| AppService {
                name: name.clone(),
                port: *port,
            })
            .collect();
        record.updated_at_ms = now_ms();
        let owner = self.clone();
        let monitor = !manifest.processes.is_empty();
        // Once the durable switch begins, its owner finishes the route handoff
        // even if the UI/tool stops waiting. Explicit Stop still cancels the run.
        tokio::spawn(async move {
            let _lifecycle = run.lifecycle.lock().await;
            if run.cancel.is_cancelled() {
                return Err(ServiceError::new(
                    "cancelled",
                    "application switch cancelled",
                ));
            }
            let library = owner.library.clone();
            // Freeze old-page admissions and synchronously revoke captured
            // tool handles before the release/data switch can commit.
            if let Some(previous) = &previous {
                previous.agent_access.lock().unwrap().cancel();
                if let Ok(agent) = owner.agent_host() {
                    let previous_id = previous.record.lock().unwrap().run_id.clone();
                    agent
                        .stop_invocation(&record.app_id, &previous_id)
                        .await
                        .map_err(|message| ServiceError::new("agentCleanupIncomplete", message))?;
                }
            }
            let saved = record.clone();
            let app_id = record.app_id.clone();
            let revision = record.revision;
            tokio::task::spawn_blocking(move || {
                library.update_run(saved)?;
                library.activate_release(&app_id, revision)
            })
            .await
            .map_err(super::join_error)??;
            run.committed.store(true, Ordering::Release);
            *run.listener.lock().await = Some(listener.clone());
            if let Some(previous) = &previous {
                previous.cancel.cancel();
                if let Some(task) = previous.gateway.lock().await.take() {
                    let _ = task.await;
                }
                previous.listener.lock().await.take();
            }
            *run.gateway.lock().await = Some(gateway::spawn(listener, route));
            *run.url.lock().unwrap() = url.clone();
            *run.record.lock().unwrap() = record.clone();
            if let Some(previous) = previous {
                match owner
                    .stop_chain(&record.app_id, previous, RunPhase::Stopped, true)
                    .await
                {
                    Ok(_) => {
                        run.previous.lock().unwrap().take();
                    }
                    Err(error) => {
                        let warning = format!("Previous application cleanup needs retry: {error}");
                        record.error = Some(warning.clone());
                        *run.record.lock().unwrap() = record.clone();
                        run.output.lock().unwrap().insert("host".into(), warning);
                    }
                }
            }
            if monitor {
                let monitoring_owner = owner.clone();
                let monitoring = run.clone();
                tokio::spawn(async move {
                    monitoring_owner.monitor_processes(monitoring).await;
                });
            }
            Ok(json!({"run":record,"url":url,"origin":origin}))
        })
        .await
        .map_err(super::join_error)?
    }

    pub(super) fn command(
        &self,
        preparation: &RunPreparation,
        step: &Step,
        port: Option<u16>,
        ports: &BTreeMap<String, u16>,
    ) -> Result<LinuxCommandRequest, ServiceError> {
        let work = Path::new(&preparation.work_path);
        let cwd = work.join(step.cwd.as_deref().unwrap_or("."));
        let root = work.canonicalize()?;
        let cwd = cwd.canonicalize()?;
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err(ServiceError::new(
                "invalidPath",
                "command cwd escapes prepared application",
            ));
        }
        let guest_work = self.guest_path(work)?;
        // Preparation gets disposable scratch data. Live application data is
        // exposed only to the ready-to-start service phase, never install/build.
        let data_path = if port.is_some() {
            &preparation.data_path
        } else {
            &preparation.preparation_data_path
        };
        let guest_data = self.guest_path(Path::new(data_path))?;
        let mut env = step.env.clone();
        env.insert("LOCAL_APP_ID".into(), preparation.record.app_id.clone());
        env.insert("LOCAL_APP_RUN_ID".into(), preparation.run.run_id.clone());
        env.insert("LOCAL_APP_DATA_DIR".into(), guest_data);
        env.insert(
            "LOCAL_APP_BASE_PATH".into(),
            // Prepared assets outlive a run epoch. Root-absolute asset requests
            // are admitted only with the gateway's current document Referer.
            if port.is_some() {
                format!("/__run/{}/", preparation.run.run_id)
            } else {
                "/".into()
            },
        );
        env.insert("HOST".into(), "127.0.0.1".into());
        env.insert(
            "npm_config_prefix".into(),
            format!("{guest_work}/.local-prefix"),
        );
        env.insert("PIP_CACHE_DIR".into(), format!("{guest_work}/.pip-cache"));
        if let Some(port) = port {
            env.insert("PORT".into(), port.to_string());
        }
        for (name, port) in ports {
            let name = name.to_uppercase().replace(['-', '.'], "_");
            env.insert(format!("LOCAL_APP_{name}_PORT"), port.to_string());
            env.insert(
                format!("LOCAL_APP_{name}_URL"),
                format!("http://127.0.0.1:{port}"),
            );
        }
        if let Some(venv) = &step.venv {
            local_app::v2::relative_path(venv)?;
            let venv = format!("{guest_work}/{venv}");
            env.insert("VIRTUAL_ENV".into(), venv.clone());
            env.insert(
                "PATH".into(),
                format!("{venv}/bin:/usr/local/bin:/usr/bin:/bin"),
            );
        }
        Ok(LinuxCommandRequest {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), step.command.clone()],
            cwd: Some(self.guest_path(&cwd)?),
            env,
            stdin: None,
            timeout_ms: port.is_none().then_some(600_000),
            network: NetworkPolicy::Allowed,
            resource_limits: ResourceLimits::default(),
            mounts: Vec::new(),
        })
    }

    async fn step(
        &self,
        preparation: &RunPreparation,
        run: &Arc<ActiveRun>,
        step: &Step,
        port: Option<u16>,
        ports: &BTreeMap<String, u16>,
    ) -> Result<(), ServiceError> {
        self.ensure_venv(preparation, run, step, ports).await?;
        self.wait_step(run, self.command(preparation, step, port, ports)?)
            .await
    }

    async fn ensure_venv(
        &self,
        preparation: &RunPreparation,
        run: &Arc<ActiveRun>,
        step: &Step,
        ports: &BTreeMap<String, u16>,
    ) -> Result<(), ServiceError> {
        if let Some(venv) = &step.venv {
            local_app::v2::relative_path(venv)?;
            if !Path::new(&preparation.work_path)
                .join(venv)
                .join("bin/python")
                .is_file()
            {
                let bootstrap = Step {
                    command: venv_command(step.python.as_deref(), venv),
                    cwd: None,
                    env: BTreeMap::new(),
                    venv: None,
                    python: None,
                };
                self.wait_step(run, self.command(preparation, &bootstrap, None, ports)?)
                    .await?;
            }
        }
        Ok(())
    }
    /// SDK spawn is not cancellation-safe on every backend. Keep its owner
    /// alive until the returned handle is registered, even if the caller drops.
    /// Stop waits on the same registry lock and then reaps that exact handle.
    async fn spawn_owned(
        &self,
        run: &Arc<ActiveRun>,
        request: LinuxCommandRequest,
    ) -> Result<mobile_linux_api::LinuxProcessHandle, ServiceError> {
        let runtime = self.runtime()?;
        let run = run.clone();
        super::EXECUTOR
            .get()
            .expect("app executor")
            .spawn(async move {
                let mut handles = run.handles.lock().await;
                if run.cancel.is_cancelled() {
                    return Err(ServiceError::new(
                        "cancelled",
                        "application startup cancelled",
                    ));
                }
                let handle = runtime.spawn_background(request).await?;
                run.log_task_ids.lock().unwrap().insert(handle.id.clone());
                handles.push(handle.clone());
                Ok(handle)
            })
            .await
            .map_err(super::join_error)?
    }

    async fn wait_step(
        &self,
        run: &Arc<ActiveRun>,
        request: LinuxCommandRequest,
    ) -> Result<(), ServiceError> {
        let runtime = self.runtime()?;
        let handle = self.spawn_owned(run, request).await?;
        loop {
            tokio::select! {_ = run.cancel.cancelled()=>return Err(ServiceError::new("cancelled","application preparation cancelled")),_ = tokio::time::sleep(Duration::from_millis(75))=>{}}
            let status = runtime.task_status(&handle.id).await?.ok_or_else(|| {
                ServiceError::new("runtimeFailure", "process backend lost a prepared task")
            })?;
            match status.status {
                MobileLinuxTaskStatus::Completed if status.exit_code == Some(0) => return Ok(()),
                MobileLinuxTaskStatus::Completed
                | MobileLinuxTaskStatus::Failed
                | MobileLinuxTaskStatus::Cancelled
                | MobileLinuxTaskStatus::TimedOut => {
                    return Err(ServiceError::new(
                        "preparationFailed",
                        format!(
                            "{} (exit {:?})",
                            status
                                .detail
                                .unwrap_or_else(|| "install/build command failed".into()),
                            status.exit_code
                        ),
                    ))
                }
                _ => {}
            }
        }
    }
    async fn ready(
        &self,
        run: &Arc<ActiveRun>,
        process: &Process,
        port: u16,
        task_id: &str,
    ) -> Result<(), ServiceError> {
        let runtime = self.runtime()?;
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(
                process
                    .ready
                    .as_ref()
                    .and_then(|r| r.timeout_ms)
                    .unwrap_or(30_000),
            );
        loop {
            if run.cancel.is_cancelled() {
                return Err(ServiceError::new(
                    "cancelled",
                    "application startup cancelled",
                ));
            }
            if let Some(status) = runtime.task_status(task_id).await? {
                if !matches!(
                    status.status,
                    MobileLinuxTaskStatus::Queued
                        | MobileLinuxTaskStatus::Running
                        | MobileLinuxTaskStatus::Backgrounded
                ) {
                    return Err(ServiceError::new(
                        "serviceFailed",
                        format!(
                            "{} exited before readiness (exit {:?})",
                            process.name, status.exit_code
                        ),
                    ));
                }
            }
            let output_ready =
                if let Some(pattern) = process.ready.as_ref().and_then(|r| r.output.as_ref()) {
                    run.output
                        .lock()
                        .unwrap()
                        .get(task_id)
                        .is_some_and(|text| text.contains(pattern))
                } else {
                    true
                };
            let path = process.ready.as_ref().and_then(|r| r.path.as_deref());
            let network_ready = if let Some(path) = path {
                http_ready(port, path).await
            } else if process.ready.as_ref().is_some_and(|r| r.output.is_some()) {
                true
            } else {
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                    .await
                    .is_ok()
            };
            if output_ready && network_ready {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ServiceError::new(
                    "readinessTimeout",
                    format!(
                        "{} did not become ready on its allocated port",
                        process.name
                    ),
                ));
            }
            tokio::select! {_=run.cancel.cancelled()=>{},_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        }
    }
    async fn collect_logs(&self, run: Arc<ActiveRun>) {
        let Ok(runtime) = self.runtime() else {
            return;
        };
        let mut cursor = None;
        let mut final_batches = 0;
        loop {
            let finishing = run.log_stop.is_cancelled();
            let handles = run.handles.lock().await;
            let ids = run.log_task_ids.lock().unwrap().clone();
            let events = runtime.read_events(cursor, 512).await;
            let batch_len = events.as_ref().map_or(0, Vec::len);
            drop(handles);
            if let Ok(events) = events {
                for event in events {
                    cursor = Some(event.sequence);
                    if !event.task_id.as_ref().is_some_and(|id| ids.contains(id)) {
                        continue;
                    }
                    let text = match event.kind {
                        MobileLinuxEventKind::StdoutLine { line } => format!("{line}\n"),
                        MobileLinuxEventKind::StderrChunk { chunk } => {
                            String::from_utf8_lossy(&chunk).into_owned()
                        }
                        _ => continue,
                    };
                    if let Some(id) = event.task_id {
                        let mut output = run.output.lock().unwrap();
                        let current = output.entry(id).or_default();
                        current.push_str(&text);
                        if current.len() > 65536 {
                            let mut at = current.len() - 32768;
                            while !current.is_char_boundary(at) {
                                at += 1;
                            }
                            current.drain(..at);
                        }
                    }
                    let record = run.record.lock().unwrap().clone();
                    let library = self.library.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        library.append_log(&record.app_id, &record.run_id, &text)
                    })
                    .await;
                }
            }
            if finishing {
                final_batches += 1;
                // Drain multiple SDK batches after the last process is reaped.
                // A different app flooding the shared journal must not keep
                // this app's Stop alive indefinitely.
                if batch_len < 512 || final_batches >= 16 {
                    break;
                }
                continue;
            }
            tokio::select! {_=run.log_stop.cancelled()=>{},_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        }
    }
    pub(super) async fn monitor_processes(self: Arc<Self>, run: Arc<ActiveRun>) {
        let Ok(runtime) = self.runtime() else {
            return;
        };
        loop {
            tokio::select! {_=run.cancel.cancelled()=>return,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
            // Completed install/build tasks are excluded: only registered service
            // process IDs are monitored after readiness.
            let ids = run.service_tasks.lock().unwrap().clone();
            let mut failure = None;
            for id in ids {
                match runtime.task_status(&id).await {
                    Ok(Some(s))
                        if matches!(
                            s.status,
                            MobileLinuxTaskStatus::Queued
                                | MobileLinuxTaskStatus::Running
                                | MobileLinuxTaskStatus::Backgrounded
                        ) => {}
                    Ok(Some(s)) => {
                        failure = Some(format!(
                            "application service exited (exit {:?})",
                            s.exit_code
                        ));
                        break;
                    }
                    Ok(None) => {
                        failure = Some("application service disappeared from runtime".into());
                        break;
                    }
                    Err(e) => {
                        failure = Some(e.to_string());
                        break;
                    }
                }
            }
            if let Some(error) = failure {
                let mut record = run.record.lock().unwrap().clone();
                if self
                    .stop_chain(&record.app_id, run.clone(), RunPhase::Failed, false)
                    .await
                    .is_err()
                {
                    return;
                }
                record.phase = RunPhase::Failed;
                record.services.clear();
                record.error = Some(error);
                record.updated_at_ms = now_ms();
                let library = self.library.clone();
                let _ = tokio::task::spawn_blocking(move || library.update_run(record)).await;
                return;
            }
        }
    }
}
fn venv_command(python: Option<&str>, path: &str) -> String {
    match python {
        Some(selector) => format!(
            "uv venv --python {} {}",
            shell_quote(selector),
            shell_quote(path)
        ),
        None => format!("python3 -m venv {}", shell_quote(path)),
    }
}
#[cfg(test)]
mod command_tests {
    #[test]
    fn python_version_selector_matches_desktop_venv_semantics() {
        assert_eq!(
            super::venv_command(Some("3.12"), ".venv"),
            "uv venv --python '3.12' '.venv'"
        );
        assert_eq!(
            super::venv_command(None, ".venv"),
            "python3 -m venv '.venv'"
        );
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
async fn http_ready(port: u16, path: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .ok()?;
        socket
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .ok()?;
        let mut buf = [0; 128];
        let n = socket.read(&mut buf).await.ok()?;
        let response = std::str::from_utf8(&buf[..n]).ok()?;
        let code = response.split_whitespace().nth(1)?.parse::<u16>().ok()?;
        Some((200..400).contains(&code))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}
