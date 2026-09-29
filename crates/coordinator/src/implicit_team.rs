//! Session-owned persistent teammate creation, invoked by Agent.
use crate::{TeamRegistry, WorkerStatus};
use lingxi_core::host::subagent_spawn::{
    SubagentInheritance, SubagentSpawnError, SubagentSpawnRequest,
};
use lingxi_core::host::team_spawn::{TeamSpawnSeam, TeammateLaunch};
use lingxi_core::host::{OutputStream, RuntimeSpawner};
use std::sync::Arc;

/// Coordinates identity reservation, task startup and mailbox delivery.
#[derive(Clone)]
pub struct ImplicitTeammateSpawner {
    team: Arc<TeamRegistry>,
    seam: Arc<dyn TeamSpawnSeam>,
    runtime: Arc<dyn RuntimeSpawner>,
    output: Arc<dyn OutputStream>,
    session_id: String,
    reservation: Arc<tokio::sync::Mutex<()>>,
    home: Option<std::path::PathBuf>,
}
struct StartedTeammate {
    launch: TeammateLaunch,
    agent_id: lingxi_core::types::AgentId,
    task_id: String,
}

impl ImplicitTeammateSpawner {
    pub fn new(
        team: Arc<TeamRegistry>,
        seam: Arc<dyn TeamSpawnSeam>,
        runtime: Arc<dyn RuntimeSpawner>,
        output: Arc<dyn OutputStream>,
        session_id: String,
    ) -> Self {
        let home = team.config_home().map(std::path::Path::to_path_buf);
        Self {
            team,
            seam,
            runtime,
            output,
            session_id,
            reservation: Arc::new(tokio::sync::Mutex::new(())),
            home,
        }
    }
    /// Initialize once at session startup, before any Agent or task operation.
    pub async fn initialize(&self) {
        let team_name = self.team.team_name().await.unwrap_or_else(|| {
            format!(
                "session-{}",
                self.session_id
                    .strip_prefix("sess:")
                    .unwrap_or(&self.session_id)
                    .chars()
                    .take(8)
                    .collect::<String>()
            )
        });
        if let Some(home) = &self.home {
            // Plan review persists the teammate-owned plan before notifying the lead.
            if let Err(error) = std::fs::create_dir_all(home.join("plans")) {
                tracing::warn!(%error, "failed to initialize teammate plans directory");
            }
            let lead = format!("team-lead@{team_name}");
            let now = crate::team_file::now_unix_millis();
            let value = serde_json::json!({"name":team_name,"createdAt":now,"leadAgentId":lead,"leadSessionId":self.session_id,"members":[{"agentId":lead,"name":"team-lead","agentType":"team-lead","joinedAt":now,"tmuxPaneId":"leader","cwd":std::env::current_dir().unwrap_or_default(),"subscriptions":[],"backendType":"in-process"}]});
            if let Err(error) = crate::team_file::update_team_file(home, &team_name, |current| {
                current.get_or_insert(value);
                Ok(())
            })
            .await
            {
                tracing::warn!(%error, "failed to initialize session team file");
            }
            if team_name != self.session_id {
                let _ = std::fs::rename(
                    crate::team_file::task_dir(home, &self.session_id),
                    crate::team_file::task_dir(home, &team_name),
                );
            }
            let _ = std::fs::create_dir_all(crate::team_file::task_dir(home, &team_name));
        }
        self.team.set_team_name(Some(team_name.clone())).await;
        lingxi_core::host::team_registry::set_leader_team_name_for_session(
            &self.session_id,
            Some(&team_name),
        );
        if self
            .team
            .mailbox_router
            .get(&self.team.coordinator_id)
            .await
            .is_none()
        {
            self.team
                .mailbox_router
                .register(
                    self.team.coordinator_id,
                    Arc::new(crate::TeammateMailbox::new(self.team.coordinator_id)),
                )
                .await;
        }
        self.team
            .mailbox_router
            .register_alias(&format!("team-lead@{team_name}"), self.team.coordinator_id)
            .await;
        self.team
            .mailbox_router
            .register_alias("main", self.team.coordinator_id)
            .await;
        self.team
            .mailbox_router
            .register_alias("team-lead", self.team.coordinator_id)
            .await;
        self.team.mailbox_router.set_color("team-lead", "red").await;
    }
    async fn rollback(&self, team_name: &str, name: &str, agent_id: &lingxi_core::types::AgentId) {
        self.team.delete_worker(agent_id).await;
        if let Some(home) = &self.home {
            let _ = crate::team_file::remove_team_member(
                home,
                team_name,
                &format!("{name}@{team_name}"),
                name,
            )
            .await;
        }
    }
    /// Run the transaction independently of the caller's cancellation lifetime.
    pub async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<TeammateLaunch, SubagentSpawnError> {
        let service = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        self.runtime
            .spawn(
                "teammate-launch",
                Box::pin(async move {
                    if send.is_closed() {
                        return;
                    }
                    let result = service.spawn_inner(request, inherit).await;
                    let owned_task = result.as_ref().ok().map(|started| {
                        (
                            started.agent_id,
                            started.task_id.clone(),
                            started.launch.name.clone(),
                            started.launch.team_name.clone(),
                        )
                    });
                    let _ = send.send(result.map(|started| started.launch));
                    if acknowledged.await.is_err() {
                        if let Some((agent_id, task_id, name, team_name)) = owned_task {
                            let _reservation = service.reservation.lock().await;
                            let _ = service.seam.kill(&task_id).await;
                            if service.team.find_by_agent_id(&agent_id).await.is_some() {
                                service.rollback(&team_name, &name, &agent_id).await;
                            }
                            service
                                .output
                                .emit_coordinator_status(
                                    service.team.active_worker_count().await,
                                    Some(&team_name),
                                )
                                .await;
                        }
                    }
                }),
            )
            .await
            .map_err(|error| SubagentSpawnError::Runtime(error.to_string()))?;
        let result = receive
            .await
            .map_err(|error| SubagentSpawnError::Runtime(error.to_string()))?;
        let _ = acknowledge.send(());
        result
    }

    async fn spawn_inner(
        &self,
        mut request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<StartedTeammate, SubagentSpawnError> {
        let name = request.name.as_deref().unwrap_or("");
        if name.is_empty() || request.prompt.is_empty() {
            return Err(error("name and prompt are required for spawn operation"));
        }
        let team_name = self.team.team_name().await.ok_or_else(|| error("Internal error: session team not initialized. This should have happened at startup when agent swarms are enabled."))?;
        if name.chars().any(char::is_control) {
            return Err(error(
                "Invalid name: control characters are not allowed in agent or team names",
            ));
        }
        let _reservation = self.reservation.lock().await;
        let workers = self.team.list().await;
        let mut reserved_names: Vec<String> =
            workers.iter().map(|worker| worker.name.clone()).collect();
        if let Some(home) = &self.home {
            if let Ok(file) = crate::team_file::read_team_file(home, &team_name) {
                reserved_names.extend(file.members.into_iter().map(|member| member.name));
            }
        }
        let name = unique_name(name, reserved_names.iter().map(String::as_str))?;
        let agent_id = self
            .team
            .spawn_worker(request.subagent_type.clone(), name.clone(), String::new())
            .await
            .map_err(|e| error(&e.to_string()))?;
        let advertised_id = format!("{name}@{team_name}");
        let color = [
            "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
        ][(workers.len() + 1) % 8];
        self.team.mailbox_router.set_color(&name, color).await;
        request.teammate_color = Some(color.into());
        if let Some(home) = &self.home {
            let member = serde_json::json!({"agentId":advertised_id,"name":name,"agentType":request.subagent_type,"joinedAt":crate::team_file::now_unix_millis(),"tmuxPaneId":"in-process","cwd":request.cwd.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default().display().to_string()),"subscriptions":[],"backendType":"in-process","model":request.model,"color":color,"prompt":request.prompt,"planModeRequired":request.mode.as_deref()==Some("plan")});
            let result = crate::team_file::update_team_file(home, &team_name, |current| {
                let members = current
                    .as_mut()
                    .and_then(|value| value.get_mut("members"))
                    .and_then(serde_json::Value::as_array_mut)
                    .ok_or_else(|| {
                        std::io::Error::other(
                            "reserveTeammateIdentity: updateTeamFile returned undefined",
                        )
                    })?;
                members.push(member);
                Ok(())
            })
            .await;
            if let Err(error) = result {
                self.team.delete_worker(&agent_id).await;
                return Err(SubagentSpawnError::Runtime(error.to_string()));
            }
        }
        let task_id = match self
            .seam
            .spawn_teammate_request(
                agent_id,
                name.clone(),
                team_name.clone(),
                request.clone(),
                inherit,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                self.rollback(&team_name, &name, &agent_id).await;
                return Err(match e {
                    lingxi_core::host::team_spawn::TeamSpawnError::Internal(message) => {
                        error(&message)
                    }
                    other => error(&other.to_string()),
                });
            }
        };
        self.team.set_task_id(&agent_id, task_id.clone()).await;
        self.team
            .mailbox_router
            .register_alias(&advertised_id, agent_id)
            .await;
        self.team
            .mailbox_router
            .register_alias(&task_id, agent_id)
            .await;
        let mailbox = self
            .team
            .mailbox_router
            .get(&agent_id)
            .await
            .expect("reserved teammate mailbox");
        let seam = self.seam.clone();
        let router = self.team.mailbox_router.clone();
        let pump_task_id = task_id.clone();
        if let Err(e) = self
            .runtime
            .spawn(
                &format!("teammate-pump:{task_id}"),
                Box::pin(async move {
                    crate::run_teammate_pump(mailbox, pump_task_id, seam).await;
                    router.unregister(&agent_id).await;
                }),
            )
            .await
        {
            let _ = self.seam.kill(&task_id).await;
            self.rollback(&team_name, &name, &agent_id).await;
            return Err(error(&format!(
                "failed to start teammate mailbox pump: {e}"
            )));
        }
        let initial_status = if self.seam.is_alive(&task_id).await {
            WorkerStatus::Working {
                activity: "running".into(),
            }
        } else {
            WorkerStatus::Failed {
                error: "teammate terminated during startup".into(),
            }
        };
        self.team
            .update_status_if_nonterminal(&agent_id, initial_status)
            .await;
        if let Some(worker) = self.team.find_by_agent_id(&agent_id).await {
            self.output
                .emit_coordinator_worker(&crate::handle::worker_info(worker))
                .await;
        }
        self.output
            .emit_coordinator_status(self.team.active_worker_count().await, Some(&team_name))
            .await;
        let pane = self.seam.pane_metadata(&task_id).await;
        if let (Some(pane), Some(home)) = (&pane, &self.home) {
            if let Err(error) = crate::team_file::update_team_file(home, &team_name, |current| {
                if let Some(member) = current
                    .as_mut()
                    .and_then(|value| value.get_mut("members"))
                    .and_then(serde_json::Value::as_array_mut)
                    .and_then(|members| {
                        members
                            .iter_mut()
                            .find(|member| member["agentId"].as_str() == Some(&advertised_id))
                    })
                {
                    member["tmuxPaneId"] = pane.pane_id.clone().into();
                    member["backendType"] = pane.backend_type.clone().into();
                }
                Ok(())
            })
            .await
            {
                tracing::warn!(%error, "failed to update running teammate pane metadata");
            }
        }
        Ok(StartedTeammate {
            agent_id,
            task_id,
            launch: TeammateLaunch {
                teammate_id: advertised_id.clone(),
                agent_id: advertised_id,
                agent_type: request.subagent_type,
                model: request.model.unwrap_or_default(),
                name,
                color: color.into(),
                tmux_session_name: pane
                    .as_ref()
                    .map_or_else(|| "in-process".into(), |p| p.session_name.clone()),
                tmux_window_name: pane
                    .as_ref()
                    .map_or_else(|| "in-process".into(), |p| p.window_name.clone()),
                tmux_pane_id: pane
                    .as_ref()
                    .map_or_else(|| "in-process".into(), |p| p.pane_id.clone()),
                team_name,
                is_splitpane: pane.is_some(),
                plan_mode_required: request.mode.as_deref() == Some("plan"),
            },
        })
    }
}
fn error(message: &str) -> SubagentSpawnError {
    SubagentSpawnError::Runtime(message.into())
}
fn unique_name<'a>(
    name: &str,
    names: impl Iterator<Item = &'a str>,
) -> Result<String, SubagentSpawnError> {
    let name = name.replace('@', "-");
    if name == "main" {
        return Err(error("\"main\" is a reserved recipient name (SendMessage routes it to the main conversation) — choose another teammate name."));
    }
    let normalized = agent::catalog::normalize_teammate_recipient(&name);
    if normalized == "main" || normalized == "team-lead" || is_reserved_agent_id(&normalized) {
        return Err(error("That teammate name is a reserved recipient (\"main\" or \"team-lead\", in any spelling) or has the shape of an agent id, which already addresses an agent directly — choose another teammate name."));
    }
    let names: Vec<_> = names.map(str::to_lowercase).collect();
    if !names.contains(&name.to_lowercase()) {
        return Ok(name);
    }
    let mut suffix = 2;
    while names.contains(&format!("{name}-{suffix}").to_lowercase()) {
        suffix += 1;
    }
    Ok(format!("{name}-{suffix}"))
}

fn is_reserved_agent_id(name: &str) -> bool {
    let Some(body) = name.strip_prefix('a') else {
        return false;
    };
    if body.len() < 16 {
        return false;
    }
    if !body.is_ascii() {
        return false;
    }
    let (prefix, hex) = body.split_at(body.len() - 16);
    hex.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && (prefix.is_empty()
            || prefix.strip_suffix('-').is_some_and(|p| {
                !p.is_empty()
                    && p.len() <= 63
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::team_spawn::TeamSpawnError;
    use lingxi_core::types::AgentId;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Harness {
        failed_spawn: AtomicBool,
        failed_pump: AtomicBool,
        spawns: AtomicUsize,
        killed: AtomicUsize,
        block_spawn: AtomicBool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        alive: AtomicBool,
        colors: std::sync::Mutex<Vec<Option<String>>>,
        report_idle_before_return: std::sync::Mutex<Option<Arc<TeamRegistry>>>,
    }
    impl Harness {
        fn new() -> Self {
            Self {
                failed_spawn: AtomicBool::new(false),
                failed_pump: AtomicBool::new(false),
                spawns: AtomicUsize::new(0),
                killed: AtomicUsize::new(0),
                block_spawn: AtomicBool::new(false),
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                alive: AtomicBool::new(true),
                colors: std::sync::Mutex::new(Vec::new()),
                report_idle_before_return: std::sync::Mutex::new(None),
            }
        }
    }
    #[async_trait]
    impl TeamSpawnSeam for Harness {
        async fn spawn_teammate(
            &self,
            _: AgentId,
            _: String,
            _: String,
            _: String,
        ) -> Result<String, TeamSpawnError> {
            if self.block_spawn.load(Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            if self.failed_spawn.load(Ordering::SeqCst) {
                return Err(TeamSpawnError::Internal("failed".into()));
            }
            Ok(format!(
                "task-{}",
                self.spawns.fetch_add(1, Ordering::SeqCst)
            ))
        }
        async fn spawn_teammate_request(
            &self,
            agent_id: AgentId,
            name: String,
            team_name: String,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<String, TeamSpawnError> {
            self.colors
                .lock()
                .unwrap()
                .push(request.teammate_color.clone());
            let task_id = self
                .spawn_teammate(agent_id, name, team_name, request.prompt)
                .await?;
            let team = self.report_idle_before_return.lock().unwrap().clone();
            if let Some(team) = team {
                team.update_status_from_handler_by_task_id(&task_id, WorkerStatus::Idle)
                    .await;
            }
            Ok(task_id)
        }
        async fn kill(&self, _: &str) -> Result<(), TeamSpawnError> {
            self.killed.fetch_add(1, Ordering::SeqCst);
            self.alive.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn is_alive(&self, _: &str) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl RuntimeSpawner for Harness {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            if name.starts_with("teammate-pump:") && self.failed_pump.load(Ordering::SeqCst) {
                return Err(lingxi_core::host::RuntimeError::ShuttingDown);
            }
            tokio::spawn(task);
            Ok(lingxi_core::host::BackgroundTaskHandle {
                task_name: name.into(),
                task_id: 0,
            })
        }
        async fn sleep(&self, d: std::time::Duration) {
            tokio::time::sleep(d).await
        }
        async fn cancel(
            &self,
            _: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }
    #[async_trait]
    impl OutputStream for Harness {
        async fn emit_text(&self, _: &str) {}
        async fn emit_tool_call(
            &self,
            _: &lingxi_core::types::ToolUseId,
            _: &str,
            _: &serde_json::Value,
        ) {
        }
        async fn emit_tool_result(
            &self,
            _: &lingxi_core::types::ToolUseId,
            _: &str,
            _: &str,
            _: &serde_json::Value,
        ) {
        }
        async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
    }
    #[async_trait]
    impl lingxi_core::host::ToolInvoker for Harness {
        async fn invoke(
            &self,
            _: &str,
            _: serde_json::Value,
            _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
            Ok(serde_json::Value::Null)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    #[async_trait]
    impl lingxi_core::host::budget::BudgetEnforcerHandle for Harness {
        async fn check_and_charge(
            &self,
            _: u64,
        ) -> Result<(), lingxi_core::host::budget::BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    fn request() -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            name: Some("researcher".into()),
            prompt: "inspect the tree".into(),
            subagent_type: "general-purpose".into(),
            ..Default::default()
        }
    }
    fn inherit(h: &Arc<Harness>) -> SubagentInheritance {
        SubagentInheritance {
            tool_invoker: h.clone(),
            budget: h.clone(),
        }
    }
    fn setup() -> (
        ImplicitTeammateSpawner,
        Arc<TeamRegistry>,
        Arc<Harness>,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let team = Arc::new(TeamRegistry::new(AgentId::new()).with_config_home(tmp.path().into()));
        let h = Arc::new(Harness::new());
        let spawner = ImplicitTeammateSpawner::new(
            team.clone(),
            h.clone(),
            h.clone(),
            h.clone(),
            "12345678-abcd".into(),
        );
        (spawner, team, h, tmp)
    }
    #[test]
    fn names_preserve_bytes_except_at_and_reserve_case_insensitively() {
        assert_eq!(unique_name("A B@C", [].into_iter()).unwrap(), "A B-C");
        assert_eq!(
            unique_name("Scout", ["scout", "SCOUT-2"].into_iter()).unwrap(),
            "Scout-3"
        );
        assert!(unique_name("MAIN", [].into_iter()).is_err());
    }
    #[tokio::test]
    async fn spawn_preserves_idle_reported_before_task_link() {
        let (service, team, harness, _tmp) = setup();
        service.initialize().await;
        *harness.report_idle_before_return.lock().unwrap() = Some(team.clone());
        let launched = service.spawn(request(), inherit(&harness)).await.unwrap();
        let worker = team.find_by_name(&launched.name).await.unwrap();
        assert!(harness.is_alive(&worker.task_id).await);
        assert_eq!(worker.status, WorkerStatus::Idle);
    }

    #[tokio::test]
    async fn implicit_team_supports_multiple_named_members_and_ignores_input_team() {
        let (service, team, h, tmp) = setup();
        service.initialize().await;
        let mut req = request();
        req.team_name = Some("ignored-old-team".into());
        let first = service.spawn(req, inherit(&h)).await.unwrap();
        let second = service.spawn(request(), inherit(&h)).await.unwrap();
        assert_eq!(first.team_name, "session-12345678");
        assert_eq!(first.teammate_id, "researcher@session-12345678");
        assert_eq!(second.name, "researcher-2");
        assert_eq!(first.color, "blue");
        assert_eq!(second.color, "green");
        assert_eq!(
            *h.colors.lock().unwrap(),
            vec![Some("blue".into()), Some("green".into())]
        );
        assert_eq!(team.list().await.len(), 2);
        let file = crate::team_file::read_team_file(tmp.path(), &first.team_name).unwrap();
        assert_eq!(file.members.len(), 3);
        assert!(team
            .mailbox_router
            .resolve_alias(&first.teammate_id)
            .await
            .is_some());
    }
    #[tokio::test]
    async fn teammate_message_to_main_reaches_leader_inbox() {
        use lingxi_core::host::mailbox::MailboxRouterHandle;
        let (service, team, h, _tmp) = setup();
        service.initialize().await;
        let launched = service.spawn(request(), inherit(&h)).await.unwrap();
        let message = lingxi_core::host::mailbox::MailboxMessage {
            content: "finished the inspection".into(),
            color: None,
            message_id: "message-1".into(),
            timestamp: std::time::SystemTime::now(),
        };
        MailboxRouterHandle::route(
            team.mailbox_router.as_ref(),
            &launched.name,
            "main",
            message,
        )
        .await
        .unwrap();
        let inbox = team.mailbox_router.get(&team.coordinator_id).await.unwrap();
        let messages = inbox.drain();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from_name, "researcher");
        assert_eq!(messages[0].content, "finished the inspection");
    }

    #[tokio::test]
    async fn task_failure_rolls_back_member_and_route() {
        let (service, team, h, tmp) = setup();
        service.initialize().await;
        h.failed_spawn.store(true, Ordering::SeqCst);
        assert!(service.spawn(request(), inherit(&h)).await.is_err());
        assert!(team.list().await.is_empty());
        assert!(team
            .mailbox_router
            .resolve_name("researcher")
            .await
            .is_none());
        assert_eq!(
            crate::team_file::read_team_file(tmp.path(), "session-12345678")
                .unwrap()
                .members
                .len(),
            1
        );
    }
    #[tokio::test]
    async fn pump_failure_kills_started_task_and_removes_identity() {
        let (service, team, h, _tmp) = setup();
        service.initialize().await;
        h.failed_pump.store(true, Ordering::SeqCst);
        assert!(service.spawn(request(), inherit(&h)).await.is_err());
        assert!(team.list().await.is_empty());
        assert_eq!(h.killed.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn cancelled_launch_cleans_reserved_member_and_backing_task() {
        let (service, team, h, tmp) = setup();
        service.initialize().await;
        h.block_spawn.store(true, Ordering::SeqCst);
        let caller_service = service.clone();
        let caller_h = h.clone();
        let caller =
            tokio::spawn(async move { caller_service.spawn(request(), inherit(&caller_h)).await });
        h.started.notified().await;
        caller.abort();
        let _ = caller.await;
        h.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if h.killed.load(Ordering::SeqCst) == 1 && team.list().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(team
            .mailbox_router
            .resolve_name("researcher")
            .await
            .is_none());
        assert_eq!(
            crate::team_file::read_team_file(tmp.path(), "session-12345678")
                .unwrap()
                .members
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn immediate_terminal_task_is_not_resurrected_as_working() {
        let (service, team, h, _tmp) = setup();
        service.initialize().await;
        h.alive.store(false, Ordering::SeqCst);
        service.spawn(request(), inherit(&h)).await.unwrap();
        assert!(matches!(
            team.list().await[0].status,
            WorkerStatus::Failed { .. }
        ));
        assert_eq!(team.active_worker_count().await, 0);
    }

    #[tokio::test]
    async fn uninitialized_session_refuses_spawn() {
        let (service, _, h, _tmp) = setup();
        assert!(service
            .spawn(request(), inherit(&h))
            .await
            .unwrap_err()
            .to_string()
            .contains("session team not initialized"));
    }
}
