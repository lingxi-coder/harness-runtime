//! One clean-environment process per trusted Team start. Default launches deny
//! all tools. A separately authenticated trusted host may install the narrow
//! owner/original-human broker; no backend permit decoder or public IPC lives here.

mod interaction;
pub(super) mod model;
pub use model::TeamModelRoute;
mod protocol;
mod worker;

use super::team_invocation::{
    TeamInvocationError, TeamInvocationIdentity, TeamInvocationLimits, TeamInvocationProfile,
};
use protocol::{Bootstrap, MAX_FRAME};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Trusted backend's absolute five-minute lease, captured once into a monotonic
/// deadline. Copying, waiting or reusing the request never extends that deadline.
#[derive(Clone, Copy, Debug)]
pub struct TeamProcessDeadline {
    unix_ms: u64,
    instant: Instant,
}

impl TeamProcessDeadline {
    pub(crate) fn instant(self) -> Instant {
        self.instant
    }

    /// Call when receiving the authenticated `startBot execute:true` response.
    /// Clock skew that places its deadline outside (now, now + 5 min] refuses.
    pub fn from_backend_unix_millis(unix_ms: u64) -> Result<Self, TeamProcessError> {
        let instant = Instant::now();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TeamProcessError::Deadline)?;
        let now_ms = u64::try_from(now.as_millis()).map_err(|_| TeamProcessError::Deadline)?;
        let remaining = unix_ms
            .checked_sub(now_ms)
            .filter(|n| *n > 0 && *n <= 300_000)
            .ok_or(TeamProcessError::Deadline)?;
        Ok(Self {
            unix_ms,
            instant: instant + Duration::from_millis(remaining),
        })
    }

    fn check(self) -> Result<(), TeamProcessError> {
        if Instant::now() >= self.instant {
            Err(TeamProcessError::Deadline)
        } else {
            Ok(())
        }
    }
}

/// One frozen registry-selected static route and key. The key crosses only
/// anonymous pipes, never argv/environment/settings or catalog projections.
#[derive(Clone)]
pub struct TeamProcessModel {
    model_id: String,
    endpoint: String,
    api_key: String,
    route: TeamModelRoute,
}

impl std::fmt::Debug for TeamProcessModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamProcessModel")
            .field("model_id", &self.model_id)
            .finish_non_exhaustive()
    }
}

impl TeamProcessModel {
    /// Endpoint is chosen by the trusted host, not by model text or a member.
    /// HTTPS is required except literal loopback HTTP for local model gateways
    /// and acceptance fixtures. Userinfo, queries and fragments are rejected.
    pub fn anthropic_api_key(
        model_id: String,
        endpoint: String,
        api_key: String,
    ) -> Result<Self, TeamProcessError> {
        if model_id.contains('/') {
            return Err(TeamProcessError::Model);
        }
        let mut profile = llm_runtime::anthropic_provider_profile(
            &endpoint,
            llm_runtime::AuthStrategy::ApiKey,
            llm_runtime::CredentialConfig::Static {
                id: model::PRIVATE_CREDENTIAL.into(),
            },
        );
        profile
            .models
            .retain(|entry| entry.request_model == model_id);
        let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
            providers: vec![profile],
        })
        .map_err(|_| TeamProcessError::Model)?;
        let profile = client
            .provider_service_profiles()
            .into_iter()
            .next()
            .ok_or(TeamProcessError::Model)?;
        let profile = model::sanitized(profile, &model_id)?;
        Self::selected_api_key(
            TeamModelRoute {
                model_provider: profile.profile_name.clone(),
                model_protocol: profile.protocol,
                model_id,
                model_endpoint: endpoint,
                credential_id: "anthropic-api-key".into(),
                model_profile: profile,
            },
            api_key,
        )
    }
}

/// Explicit host input; intentionally has no serde or settings constructor.
/// This does not authenticate a permit. The product must validate/consume its
/// backend permit before preparing this request and use stable invocation IDs.
pub struct TeamProcessRequest {
    pub identity: TeamInvocationIdentity,
    pub invocation_id: uuid::Uuid,
    pub instructions: String,
    pub input: String,
    pub limits: TeamInvocationLimits,
    pub model: TeamProcessModel,
    pub deadline: TeamProcessDeadline,
}

/// A single-use launch. A durable create-new tombstone refuses another launch
/// of the same invocation, including after spawn/pipe/ACK failure. It is never
/// automatically removed: ambiguous starts require product owner recovery.
pub struct TeamProcessStart {
    profile: TeamInvocationProfile,
    bootstrap: Bootstrap,
    deadline: TeamProcessDeadline,
}

impl std::fmt::Debug for TeamProcessStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamProcessStart")
            .field("profile", &self.profile)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TeamProcessError {
    #[error("invalid Team invocation profile")]
    Profile(#[from] TeamInvocationError),
    #[error("Team invocation deadline is expired or outside its five-minute ceiling")]
    Deadline,
    #[error("Team invocation was cancelled")]
    Cancelled,
    #[error("Team invocation already attempted; owner recovery is required")]
    AlreadyAttempted,
    #[error("Team worker must be a canonical private-owner executable")]
    Executable,
    #[error("Team worker model requires one supported frozen static route and safe endpoint")]
    Model,
    #[error("Team worker private protocol is invalid or exceeds its bound")]
    Protocol,
    #[error("Team worker environment is not the dedicated clean environment")]
    Environment,
    #[error("Team worker failed; execution must not be replayed")]
    Worker,
    #[error("Team worker process or storage operation failed")]
    Io(#[from] std::io::Error),
}

/// A permission forwarded exclusively to the authenticated Bot owner. This
/// admits only a single Bash call with its original, unedited JSON input.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamOwnerPermission {
    pub id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// Questions forwarded exclusively to the original human. Answer map keys are
/// the exact question texts; `text` adds bounded headers/options for display.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamHumanQuestion {
    pub id: String,
    pub questions: Vec<String>,
    pub text: String,
}

/// Trusted host boundary. Implementations must authenticate the owner/original
/// human and recheck membership, account epoch, revocation and the shared lease
/// after every wait. A generic TUI, renderer or remote-control resolver cannot
/// implement this authority. Futures are supervised inline and never detached.
#[async_trait::async_trait]
pub trait TeamProcessBroker: Send + Sync {
    async fn approve(
        &self,
        request: TeamOwnerPermission,
        cancel: CancellationToken,
    ) -> Result<bool, TeamProcessError>;
    async fn ask(
        &self,
        request: TeamHumanQuestion,
        cancel: CancellationToken,
    ) -> Result<Option<BTreeMap<String, String>>, TeamProcessError>;
}

/// Worker outcomes carry no provider error bodies or arbitrary diagnostic text.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamProcessOutcome {
    Completed,
    MaxTurns,
    ModelFailed,
    OutputLimit,
}

/// Bounded terminal text for the trusted host to settle through `finishBot`.
/// Transport failure/timeout is ambiguous and never yields a completed result.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamProcessResult {
    version: u32,
    pub outcome: TeamProcessOutcome,
    pub text: String,
    pub dispatched_requests: u32,
    pub denied_tools: u32,
}

impl TeamProcessStart {
    pub fn prepare(
        storage_root: &Path,
        request: TeamProcessRequest,
    ) -> Result<Self, TeamProcessError> {
        request.deadline.check()?;
        let profile = TeamInvocationProfile::prepare(
            storage_root,
            request.identity,
            request.invocation_id,
            request.instructions.clone(),
            request.limits,
        )?
        .with_selected_model(request.model.clone());
        profile.validate_input(&request.input)?;
        let identity = profile.identity();
        let bootstrap = Bootstrap {
            version: 1,
            storage_root: storage_root.to_owned(),
            owner_uid: identity.owner_uid.clone(),
            bot_key: identity.bot_key.clone(),
            team_id: identity.team_id.clone(),
            conversation_id: identity.conversation_id.clone(),
            origin_human_uid: identity.origin_human_uid.clone(),
            membership_generation: identity.membership_generation,
            invocation_id: request.invocation_id,
            instructions: request.instructions,
            input: request.input,
            max_turns: request.limits.max_turns.get(),
            max_input_bytes: request.limits.max_input_bytes.get(),
            output_tokens_per_request: request.limits.output_tokens_per_request.get(),
            physical_dispatches: request.limits.physical_dispatches.get(),
            deadline_unix_ms: request.deadline.unix_ms,
            model_provider: request.model.route.model_provider.clone(),
            model_protocol: request.model.route.model_protocol,
            model_profile: request.model.route.model_profile.clone(),
            model_id: request.model.model_id,
            endpoint: request.model.endpoint,
            api_key: request.model.api_key,
        };
        Ok(Self {
            profile,
            bootstrap,
            deadline: request.deadline,
        })
    }

    /// Run the dedicated binary with no inherited environment/stdio or CLI
    /// options. Cancellation/drop/deadline kill the worker process group.
    /// This default v1 route denies every tool and installs no question sender.
    pub async fn run(
        self,
        worker_executable: &Path,
        cancel: CancellationToken,
    ) -> Result<TeamProcessResult, TeamProcessError> {
        self.run_inner(worker_executable, cancel, None).await
    }

    /// Stream narrow owner approvals and original-human questions over the
    /// private worker pipe. `true` authorizes AllowOnce only; no policy mutation,
    /// edited tool input, command dispatch or credential refresh route exists.
    pub async fn run_with_broker(
        self,
        worker_executable: &Path,
        cancel: CancellationToken,
        broker: Arc<dyn TeamProcessBroker>,
    ) -> Result<TeamProcessResult, TeamProcessError> {
        self.run_inner(worker_executable, cancel, Some(broker))
            .await
    }

    async fn run_inner(
        mut self,
        worker_executable: &Path,
        cancel: CancellationToken,
        broker: Option<Arc<dyn TeamProcessBroker>>,
    ) -> Result<TeamProcessResult, TeamProcessError> {
        self.deadline.check()?;
        if cancel.is_cancelled() {
            return Err(TeamProcessError::Cancelled);
        }
        check_executable(worker_executable)?;
        self.profile.validate_input(&self.bootstrap.input)?;
        let mut cfg = super::DesktopConfig::default();
        cfg.default_model = format!(
            "{}/{}",
            self.profile
                .selected_model()
                .ok_or(TeamProcessError::Model)?
                .route
                .model_provider,
            self.bootstrap.model_id
        );
        self.profile.apply_to(&mut cfg)?;
        self.bootstrap.version = if broker.is_some() { 2 } else { 1 };
        let frame = protocol::encode(&self.bootstrap)?;
        // Persist before spawning, even if spawn itself fails. Never reset
        // quota authority or replay ambiguous work after a lost response.
        record_start(&self.profile)?;
        self.deadline.check()?;
        let mut command = tokio::process::Command::new(worker_executable);
        command
            .env_clear()
            .envs(environment(self.profile.scope_root()))
            .current_dir(self.profile.scope_root().join("workspace"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn()?;
        let mut guard = WorkerGroupGuard(child.id());
        let mut input = child.stdin.take().ok_or(TeamProcessError::Worker)?;
        let mut output = child.stdout.take().ok_or(TeamProcessError::Worker)?;
        let broker_cancel = cancel.child_token();
        let _broker_guard = CancelOnDrop(broker_cancel.clone());
        let exchange = async {
            input.write_all(&frame).await?;
            // Keep the pipe open as a parent-liveness lease. Worker EOF cancels
            // its entire build/run/drain. Version two admits only correlated
            // permission/question responses after this single bootstrap.
            input.flush().await?;
            let result = if let Some(broker) = broker {
                broker_exchange(&mut output, &mut input, broker, broker_cancel).await?
            } else {
                let mut bytes = Vec::new();
                output
                    .take((MAX_FRAME + 5) as u64)
                    .read_to_end(&mut bytes)
                    .await?;
                if bytes.len() > MAX_FRAME + 4 {
                    return Err(TeamProcessError::Protocol);
                }
                protocol::decode_result(&bytes)?
            };
            let status = child.wait().await?;
            guard.0 = None; // reaped: never signal a potentially reused PID
            if !status.success() {
                return Err(TeamProcessError::Worker);
            }
            if result.dispatched_requests > self.bootstrap.physical_dispatches {
                return Err(TeamProcessError::Protocol);
            }
            self.deadline.check()?;
            Ok(result)
        };
        let result = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(self.deadline.instant.into()) => Err(TeamProcessError::Deadline),
            _ = cancel.cancelled() => Err(TeamProcessError::Cancelled),
            result = exchange => result,
        };
        if guard.0.is_some() {
            guard.kill();
            let _ = child.start_kill();
            // Reaping is bounded separately; model work is already SIGKILLed.
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_ok()
            {
                guard.0 = None;
            }
        }
        result
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn broker_exchange(
    output: &mut (impl tokio::io::AsyncRead + Unpin),
    input: &mut (impl tokio::io::AsyncWrite + Unpin),
    broker: Arc<dyn TeamProcessBroker>,
    cancel: CancellationToken,
) -> Result<TeamProcessResult, TeamProcessError> {
    use protocol::{ParentFrame, WorkerFrame};
    let mut seen = BTreeSet::new();
    // Serialized interactions make a second request, result or any byte while
    // awaiting the broker an invalid out-of-order frame. Read one byte here so
    // a partial malicious frame cannot hide behind a successful approval.
    loop {
        let request: WorkerFrame = protocol::read_frame(output).await?;
        let mut unexpected = [0u8; 1];
        let response = match request {
            WorkerFrame::OwnerPermission { request } => {
                protocol::validate_permission(&request)?;
                if !seen.insert(request.id.clone()) {
                    return Err(TeamProcessError::Protocol);
                }
                let id = request.id.clone();
                let allow_once = tokio::select! {
                    biased;
                    _ = output.read(&mut unexpected) => return Err(TeamProcessError::Protocol),
                    result = broker.approve(request, cancel.child_token()) => result?,
                };
                ParentFrame::OwnerPermission { id, allow_once }
            }
            WorkerFrame::HumanQuestion { request } => {
                protocol::validate_question(&request)?;
                if !seen.insert(request.id.clone()) {
                    return Err(TeamProcessError::Protocol);
                }
                let answers = tokio::select! {
                    biased;
                    _ = output.read(&mut unexpected) => return Err(TeamProcessError::Protocol),
                    result = broker.ask(request.clone(), cancel.child_token()) => result?,
                };
                protocol::validate_answers(&request, &answers)?;
                ParentFrame::HumanQuestion {
                    id: request.id,
                    answers,
                }
            }
            WorkerFrame::Result { result } => {
                protocol::validate_result(&result, 2)?;
                // A terminal result is accepted only after clean stdout EOF.
                if output.read(&mut unexpected).await? != 0 {
                    return Err(TeamProcessError::Protocol);
                }
                return Ok(result);
            }
        };
        if cancel.is_cancelled() {
            return Err(TeamProcessError::Cancelled);
        }
        input.write_all(&protocol::encode(&response)?).await?;
        input.flush().await?;
    }
}

pub(super) fn environment(scope: &Path) -> [(String, std::ffi::OsString); 8] {
    [
        ("HOME".into(), scope.join("home").into_os_string()),
        (
            "LINGXI_CONFIG_DIR".into(),
            scope.join("home").into_os_string(),
        ),
        ("TMPDIR".into(), scope.join("tmp").into_os_string()),
        ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
        (
            "CFFIXED_USER_HOME".into(),
            scope.join("home").into_os_string(),
        ),
        (
            "__CF_USER_TEXT_ENCODING".into(),
            fixed_text_encoding().into(),
        ),
    ]
}

pub(super) fn fixed_text_encoding() -> String {
    // CoreFoundation initializes before main and may otherwise read the real
    // user's .CFUserTextEncoding and mutate the supposedly clean environment.
    #[cfg(unix)]
    let uid = nix::unistd::geteuid().as_raw();
    #[cfg(not(unix))]
    let uid = 0u32;
    format!("0x{uid:X}:0x8000100:0x0") // fixed UTF-8, not user preferences
}

fn record_start(profile: &TeamInvocationProfile) -> Result<(), TeamProcessError> {
    let path = profile.task_output_dir().join("process-started");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(TeamProcessError::AlreadyAttempted);
        }
        Err(e) => return Err(e.into()),
    };
    file.write_all(b"team-worker-start-v1\n")?;
    file.sync_all()?;
    std::fs::File::open(profile.task_output_dir())?.sync_all()?;
    Ok(())
}

fn check_executable(path: &Path) -> Result<(), TeamProcessError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|_| TeamProcessError::Executable)?;
        if metadata.is_file()
            && metadata.uid() == nix::unistd::geteuid().as_raw()
            && metadata.mode() & 0o022 == 0
            && metadata.mode() & 0o111 != 0
            && path.canonicalize().ok().as_deref() == Some(path)
        {
            return Ok(());
        }
    }
    Err(TeamProcessError::Executable)
}

struct WorkerGroupGuard(Option<u32>);
impl WorkerGroupGuard {
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            let _ = platform_posix::process::kill_tree::kill_tree_force(pid);
        }
    }
}
impl Drop for WorkerGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Entry point of the private binary. Errors return a fixed exit code; provider
/// bodies, credentials and arbitrary diagnostics are never written to stdio.
pub fn worker_main() -> i32 {
    match worker::main() {
        Ok(()) => 0,
        Err(TeamProcessError::Environment) => 78,
        Err(TeamProcessError::Profile(_)) => 77,
        Err(TeamProcessError::Protocol) => 65,
        Err(TeamProcessError::Deadline | TeamProcessError::Cancelled) => 75,
        Err(TeamProcessError::Io(_)) => 74,
        Err(_) => 70,
    }
}
