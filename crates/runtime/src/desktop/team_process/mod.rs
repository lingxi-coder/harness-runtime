//! One clean-environment process per trusted Team task. M1 exposes only the
//! current Chat and published knowledge through a revocable host broker. The
//! canonical Bot×Chat session survives completed rounds and process restarts.

pub(crate) mod accounts;
mod ask_tool;
mod chat_session;
mod checkpoint;
mod data;
mod data_tools;
mod decline_tool;
mod delegate_tool;
mod interaction;
pub(crate) mod list_tools;
mod local_app_tools;
pub(crate) mod read_tools;
use crate::desktop::session_state::team_chat::{BotChatCursor, BotChatScope, ChatInputBatch};
pub use chat_session::{inspect_scope, inspect_task, recover_result};
pub use checkpoint::{
    route_supports_continuation, TeamCheckpointCursor, TeamPendingToolCall, TeamTaskCheckpoint,
    TeamTaskPhase, TeamTaskWait, TeamWaitKind,
};
pub use data::TeamDataRef;
pub use data_tools::TeamDatasetOffer;
pub use delegate_tool::TeamDelegateOffer;
pub use list_tools::{
    TeamListApproval, TeamOperationRequest, TeamOperationResult, TeamOperationTool, TeamSharedList,
};
pub use local_app_tools::TeamLocalAppOffer;
pub use read_tools::{
    TeamChatQuery, TeamChatReadRequest, TeamChatReadResult, TeamKnowledgeDocument,
};
pub(super) mod model;
pub use accounts::TeamAccountEnvelope;
pub use model::TeamModelRoute;
mod protocol;
mod visualization;
mod worker;

use super::team_invocation::{TeamInvocationError, TeamInvocationLimits, TeamInvocationProfile};
use protocol::Bootstrap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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

    /// Call when receiving the authenticated `startTask execute:true` response.
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
    route: TeamModelRoute,
    credentials: Arc<TeamAccountEnvelope>,
}

impl std::fmt::Debug for TeamProcessModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamProcessModel")
            .field("model_id", &self.model_id)
            .finish_non_exhaustive()
    }
}

/// Explicit host input; intentionally has no serde or settings constructor.
/// This does not authenticate a permit. The product must validate/consume its
/// backend permit before preparing this request and use stable invocation IDs.
pub struct TeamProcessRequest {
    pub scope: BotChatScope,
    pub invocation_id: uuid::Uuid,
    pub instructions: String,
    pub task_id: String,
    pub input_batch_ref: TeamDataRef,
    pub knowledge: Vec<TeamKnowledgeDocument>,
    pub limits: TeamInvocationLimits,
    pub model: TeamProcessModel,
    pub deadline: TeamProcessDeadline,
    /// Offer the Bot the tool that asks the task's requester one question and
    /// pauses the task. Ignored on routes without continuation support, since
    /// a paused task could never be continued there.
    pub ask_requester: bool,
    /// Offer the Bot the published shared list: one tool reads it and one
    /// changes it through the Team operation ledger. Ignored on routes without
    /// continuation support, where an interrupted write could not be finished.
    pub shared_list: Option<TeamSharedList>,
    pub shared_data: Vec<TeamDatasetOffer>,
    /// Offer the Bot the Local Apps the host lets this run use: one tool lists
    /// the tools of an offered app and one calls them, through the Team
    /// operation ledger. At most 16 apps. Ignored on routes without
    /// continuation support, where an interrupted call could not be finished.
    pub local_apps: Vec<TeamLocalAppOffer>,
    /// Offer the Bot a tool to hand part of the request to one of these other
    /// Bots of the Chat, with or without waiting for its reply. At most 20
    /// Bots. Ignored on routes without continuation support, where an
    /// interrupted delegation could not be finished.
    pub delegates: Vec<TeamDelegateOffer>,
    pub visualizations: bool,
    /// Nobody asked the Bot for this task: offer it the tool that leaves the
    /// message unanswered. The task then ends as `Declined` and posts nothing.
    pub may_decline: bool,
    /// With `may_decline` only: the member wrote this message to the Bot
    /// directly (a Bot DM), so the Bot answers unless it plainly needs no answer.
    pub addressed: bool,
}

/// The requester's reply to what a task is waiting on: the answer to its
/// question, or their decision on the change it proposed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeamWaitAnswer {
    pub wait_id: String,
    pub message_id: String,
    pub text: String,
    /// Present exactly for the reply to an approval request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
}

/// Continuation of a task whose earlier worker process died. The epoch must
/// exceed every recorded epoch (the first `run` is epoch 1) and the digest must
/// still match the durable checkpoint the host inspected.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeamContinuation {
    pub epoch: u32,
    pub expected_checkpoint_digest: String,
    /// Required exactly when the checkpoint reports a wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<TeamWaitAnswer>,
}
/// What the answer of a continuation may hold, in bytes: a person's answer or
/// another Bot's reply to a delegation.
pub(super) const MAX_ANSWER_BYTES: usize = 8192;
impl TeamContinuation {
    pub(crate) fn validate(&self) -> Result<(), TeamProcessError> {
        if self.epoch < 2 || !checkpoint::valid_digest(&self.expected_checkpoint_digest) {
            return Err(TeamProcessError::Protocol);
        }
        if let Some(answer) = &self.answer {
            if !protocol::valid_id(&answer.wait_id)
                || !protocol::valid_id(&answer.message_id)
                || answer.text.trim().is_empty()
                || answer.text.len() > MAX_ANSWER_BYTES
            {
                return Err(TeamProcessError::Protocol);
            }
        }
        Ok(())
    }
}

/// A single-use launch. A durable create-new tombstone refuses another launch
/// of the same invocation, including after spawn/pipe/ACK failure. It is never
/// automatically removed: ambiguous starts require product owner recovery.
pub struct TeamProcessStart {
    profile: TeamInvocationProfile,
    bootstrap: Bootstrap,
    deadline: TeamProcessDeadline,
    batch_id: String,
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
    #[error("complete private data object exceeds its 64 MiB transport resource ceiling")]
    ResourceLimit,
    #[error("complete Chat input exceeds the selected model context capacity")]
    Capacity,
    #[error("Team continuation checkpoint changed since it was inspected")]
    CheckpointMismatch,
    #[error("Team task is not continuable from its durable checkpoint")]
    NotContinuable,
    #[error("Team continuation is not enabled for this provider route")]
    RouteUnsupported,
    #[error("Team worker failed; execution must not be replayed")]
    Worker,
    #[error("Team worker process or storage operation failed")]
    Io(#[from] std::io::Error),
}

/// Trusted host boundary. Recheck current membership, published grants, source
/// watermark, account epoch and task lease before every provider dispatch/read.
/// Futures are supervised inline and never detached.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamProviderAccountRefresh {
    pub id: String,
    pub account_id: String,
    pub identity_generation: u64,
    pub credential_revision: u64,
}

#[async_trait::async_trait]
pub trait TeamProcessBroker: Send + Sync {
    async fn authorize(&self, cancel: CancellationToken) -> Result<(), TeamProcessError>;
    async fn refresh_provider_account(
        &self,
        _request: TeamProviderAccountRefresh,
        _cancel: CancellationToken,
    ) -> Result<TeamAccountEnvelope, TeamProcessError> {
        Err(TeamProcessError::Model)
    }
    async fn read_chat(
        &self,
        request: TeamChatReadRequest,
        cancel: CancellationToken,
    ) -> Result<TeamChatReadResult, TeamProcessError>;
    /// Run one shared-list request. For a write the host records the operation
    /// in the Team ledger before the list service sees it and settles it after.
    /// Return an error whenever the outcome is not established: the process
    /// ends with the call unfinished and nothing is reported to the model.
    async fn operate(
        &self,
        _request: TeamOperationRequest,
        _cancel: CancellationToken,
    ) -> Result<TeamOperationResult, TeamProcessError> {
        Err(TeamProcessError::Protocol)
    }
}

/// Worker outcomes carry no provider error bodies or arbitrary diagnostic text.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamProcessOutcome {
    Completed,
    MaxTurns,
    ModelFailed,
    OutputLimit,
    /// The Bot asked its requester a question. The task is not finished: no
    /// result row exists and the durable checkpoint reports the wait.
    Waiting,
    /// The Bot chose to leave an unasked message unanswered. The task is
    /// finished and has no text and no document.
    Declined,
}

/// Bounded terminal text for the trusted host to settle through `completeTask`.
/// Transport failure/timeout is ambiguous and never yields a completed result.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamProcessResult {
    version: u32,
    pub outcome: TeamProcessOutcome,
    pub text: String,
    /// Provider requests authorized for the whole task, across every epoch.
    pub dispatched_requests: u32,
    /// Provider requests authorized by the process that produced this result.
    pub attempt_dispatched_requests: u32,
    pub denied_tools: u32,
    pub finished_at: u64,
    pub cursor: BotChatCursor,
    pub document: Option<TeamDocumentResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visualization: Option<visualization::Artifact>,
    pub usage: TeamUsage,
}

/// Token/cost accounting for one Team task. `Complete` is a promise that every
/// model request of the task finished and was recorded by the session cost
/// tracker at a known price; anything weaker is `Partial`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TeamUsage {
    Missing,
    Partial {
        #[serde(rename = "inputTokens")]
        input_tokens: u64,
        #[serde(rename = "outputTokens")]
        output_tokens: u64,
        #[serde(rename = "costMicros", skip_serializing_if = "Option::is_none")]
        cost_micros: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        currency: Option<String>,
    },
    Complete {
        #[serde(rename = "inputTokens")]
        input_tokens: u64,
        #[serde(rename = "outputTokens")]
        output_tokens: u64,
        #[serde(rename = "costMicros")]
        cost_micros: u64,
        currency: String,
    },
}
impl TeamUsage {
    /// `settled` is true only when the task ended normally and no request whose
    /// usage may be unrecorded (an aborted stream, a failed compaction summary)
    /// can have happened. Complete additionally needs a known price for every
    /// model the session has used and a monotonic cost total.
    pub(super) fn from_snapshots(
        before: &lingxi_core::host::CostSnapshot,
        after: &lingxi_core::host::CostSnapshot,
        settled: bool,
    ) -> Self {
        let input = after.input_tokens.saturating_sub(before.input_tokens);
        let output = after.output_tokens.saturating_sub(before.output_tokens);
        if after.api_calls <= before.api_calls || (input == 0 && output == 0) {
            return Self::Missing;
        }
        let priced = !after.unknown_models && after.total_nano_usd >= before.total_nano_usd;
        let cost_micros = priced.then(|| (after.total_nano_usd - before.total_nano_usd) / 1000);
        match cost_micros {
            Some(cost_micros) if settled => Self::Complete {
                input_tokens: input,
                output_tokens: output,
                cost_micros,
                currency: "USD".into(),
            },
            _ => Self::Partial {
                input_tokens: input,
                output_tokens: output,
                cost_micros,
                currency: priced.then(|| "USD".into()),
            },
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TeamDocumentResult {
    pub operation_id: String,
    pub delivery_id: String,
    pub title: String,
    pub markdown: String,
    pub sha256: String,
}
struct DenyInteractions;
#[async_trait::async_trait]
impl TeamProcessBroker for DenyInteractions {
    async fn authorize(&self, _: CancellationToken) -> Result<(), TeamProcessError> {
        Err(TeamProcessError::Protocol)
    }
    async fn read_chat(
        &self,
        _: TeamChatReadRequest,
        _: CancellationToken,
    ) -> Result<TeamChatReadResult, TeamProcessError> {
        Err(TeamProcessError::Protocol)
    }
}

impl TeamProcessStart {
    pub fn prepare(
        storage_root: &Path,
        request: TeamProcessRequest,
    ) -> Result<Self, TeamProcessError> {
        Self::prepare_inner(storage_root, request, None, false)
    }

    /// Prepare a task-owned workspace for a tool-free private Data invocation.
    pub fn prepare_private(
        storage_root: &Path,
        request: TeamProcessRequest,
    ) -> Result<Self, TeamProcessError> {
        if !protocol::private_data_scope(storage_root, &request.scope, &request.task_id)
            || request.ask_requester
            || request.shared_list.is_some()
            || !request.shared_data.is_empty()
            || !request.local_apps.is_empty()
            || !request.delegates.is_empty()
            || request.visualizations
            || request.may_decline
            || !request.knowledge.is_empty()
        {
            return Err(TeamProcessError::Protocol);
        }
        Self::prepare_inner(storage_root, request, None, true)
    }

    /// Prepare the continuation of an earlier epoch. The batch is never
    /// admitted again; the worker verifies the checkpoint digest under the
    /// session writer lease before any provider dispatch.
    pub fn prepare_continuation(
        storage_root: &Path,
        request: TeamProcessRequest,
        continuation: TeamContinuation,
    ) -> Result<Self, TeamProcessError> {
        continuation.validate()?;
        if !checkpoint::route_supports_continuation(request.model.route.model_protocol) {
            return Err(TeamProcessError::RouteUnsupported);
        }
        Self::prepare_inner(storage_root, request, Some(continuation), false)
    }

    fn prepare_inner(
        storage_root: &Path,
        request: TeamProcessRequest,
        continuation: Option<TeamContinuation>,
        private_data: bool,
    ) -> Result<Self, TeamProcessError> {
        request.deadline.check()?;
        if !private_data
            && storage_root
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some("data-private")
        {
            return Err(TeamProcessError::Protocol);
        }
        let profile = TeamInvocationProfile::prepare(
            storage_root,
            request.scope,
            request.invocation_id,
            request.instructions.clone(),
            request.limits,
        )?
        .with_selected_model(request.model.clone());
        let requester = if private_data {
            Some(protocol::private_data_requester(
                storage_root,
                profile.scope(),
                &request.task_id,
            )?)
        } else {
            None
        };
        let batch: ChatInputBatch = request.input_batch_ref.read(storage_root)?;
        if let Some(requester) = requester {
            protocol::validate_private_data_input(&batch, &requester, &request.task_id)?;
        }
        batch
            .validate(profile.session_id())
            .map_err(|_| TeamProcessError::Protocol)?;
        if !protocol::valid_id(&request.task_id) {
            return Err(TeamProcessError::Protocol);
        }
        read_tools::validate_knowledge(&request.knowledge)?;
        // How the message reached the Bot is said only for a task that may stay silent.
        if request.addressed && !request.may_decline {
            return Err(TeamProcessError::Protocol);
        }
        let batch_id = batch.batch_id.clone();
        let protocol = request.model.route.model_protocol;
        let bootstrap = Bootstrap {
            version: 3,
            storage_root: storage_root.to_owned(),
            scope: profile.scope().clone(),
            invocation_id: request.invocation_id,
            task_id: request.task_id,
            instructions: request.instructions,
            input_batch_ref: request.input_batch_ref,
            knowledge_ref: TeamDataRef::write(
                &profile.scope_root().join("tmp"),
                &request.knowledge,
            )?,
            max_turns: request.limits.max_turns.get(),
            context_window_tokens: request.limits.context_window_tokens.get(),
            output_tokens_per_request: request.limits.output_tokens_per_request.get(),
            physical_dispatches: request.limits.physical_dispatches.get(),
            deadline_unix_ms: request.deadline.unix_ms,
            model_provider: request.model.route.model_provider.clone(),
            model_protocol: request.model.route.model_protocol,
            model_profile: request.model.route.model_profile.clone(),
            model_id: request.model.model_id,
            endpoint: request.model.endpoint,
            credentials: request
                .model
                .credentials
                .for_worker(&profile.scope_root().join("home"))?,
            continuation,
            private_data,
            ask_requester: request.ask_requester
                && checkpoint::route_supports_continuation(protocol),
            shared_list: match request.shared_list {
                Some(list) if checkpoint::route_supports_continuation(protocol) => {
                    list.validate()?;
                    // An approval is a wait: it needs the same durable pause as a question.
                    if list.approval == TeamListApproval::Requester && !request.ask_requester {
                        return Err(TeamProcessError::Protocol);
                    }
                    Some(list)
                }
                _ => None,
            },
            shared_data: {
                data_tools::validate_offers(&request.shared_data)?;
                request
                    .shared_data
                    .into_iter()
                    .map(|mut offer| {
                        offer.write &= checkpoint::route_supports_continuation(protocol);
                        if !offer.write {
                            offer.handoff_targets.clear();
                        }
                        offer
                    })
                    .collect()
            },
            local_apps: {
                local_app_tools::validate_offers(&request.local_apps)?;
                if checkpoint::route_supports_continuation(protocol) {
                    request.local_apps
                } else {
                    Vec::new()
                }
            },
            delegates: {
                delegate_tool::validate_offers(&request.delegates)?;
                if checkpoint::route_supports_continuation(protocol) {
                    request.delegates
                } else {
                    Vec::new()
                }
            },
            visualizations: request.visualizations,
            may_decline: request.may_decline,
            addressed: request.addressed,
        };
        Ok(Self {
            profile,
            bootstrap,
            deadline: request.deadline,
            batch_id,
        })
    }

    /// Run the dedicated binary with no inherited environment/stdio or CLI
    /// options. Cancellation/drop/deadline kill the worker process group.
    /// Without a host authorization broker, execution fails closed.
    pub async fn run(
        self,
        worker_executable: &Path,
        cancel: CancellationToken,
    ) -> Result<TeamProcessResult, TeamProcessError> {
        self.run_inner(worker_executable, cancel, None).await
    }

    /// Run with the authenticated host broker for dispatch authorization,
    /// current-Chat history and the existing selected-account refresh flow.
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
        self,
        worker_executable: &Path,
        cancel: CancellationToken,
        broker: Option<Arc<dyn TeamProcessBroker>>,
    ) -> Result<TeamProcessResult, TeamProcessError> {
        self.deadline.check()?;
        if cancel.is_cancelled() {
            return Err(TeamProcessError::Cancelled);
        }
        check_executable(worker_executable)?;
        let _: ChatInputBatch = self
            .bootstrap
            .input_batch_ref
            .read(&self.bootstrap.storage_root)?;
        if self.bootstrap.continuation.is_some() {
            // A recorded terminal result is returned as is: no epoch is
            // consumed, no worker is spawned and no model is called.
            if let Some(result) = chat_session::recover_result(
                &self.bootstrap.storage_root,
                &self.bootstrap.scope,
                &self.bootstrap.task_id,
            )? {
                return Ok(result);
            }
        }
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

        let frame = protocol::encode(&self.bootstrap)?;
        // Persist before spawning, even if spawn itself fails. Never reset
        // quota authority or replay ambiguous work after a lost response.
        checkpoint::record_start(
            &self.profile.scope_root().join("home"),
            &self.bootstrap.task_id,
            &self.batch_id,
            self.bootstrap.continuation.as_ref().map_or(1, |c| c.epoch),
        )?;
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
            // its entire build/run/drain. Major 3 admits only correlated
            // authorization, history and credential responses after this single bootstrap.
            input.flush().await?;
            let broker = broker.unwrap_or_else(|| Arc::new(DenyInteractions));
            let result = broker_exchange(
                &mut output,
                &mut input,
                broker,
                broker_cancel,
                &self.profile.scope_root().join("tmp"),
                &self.bootstrap.task_id,
                self.bootstrap.private_data,
                &self.bootstrap.shared_data,
                &self.bootstrap.local_apps,
                &self.bootstrap.delegates,
                self.bootstrap
                    .shared_list
                    .as_ref()
                    .map_or(TeamListApproval::None, |list| list.approval),
            )
            .await?;
            let status = child.wait().await?;
            guard.0 = None; // reaped: never signal a potentially reused PID
            if !status.success() {
                return Err(TeamProcessError::Worker);
            }
            if result.attempt_dispatched_requests > self.bootstrap.physical_dispatches
                || result.attempt_dispatched_requests > result.dispatched_requests
            {
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
    data_root: &Path,
    task_id: &str,
    private_data: bool,
    shared_data: &[TeamDatasetOffer],
    local_apps: &[TeamLocalAppOffer],
    delegates: &[TeamDelegateOffer],
    approval: TeamListApproval,
) -> Result<TeamProcessResult, TeamProcessError> {
    use protocol::{ParentFrame, WorkerFrame};
    let mut seen = BTreeSet::new();
    // Serialized interactions make a second request, result or any byte while
    // awaiting the broker an invalid out-of-order frame. Read one byte here so
    // a partial malicious frame cannot hide behind a successful authorization.
    loop {
        let request: WorkerFrame = protocol::read_frame(output).await?;
        let mut unexpected = [0u8; 1];
        let response = match request {
            WorkerFrame::Failed { code } => {
                return Err(match code {
                    protocol::WorkerFailure::Capacity => TeamProcessError::Capacity,
                    protocol::WorkerFailure::ResourceLimit => TeamProcessError::ResourceLimit,
                    protocol::WorkerFailure::Protocol => TeamProcessError::Protocol,
                    protocol::WorkerFailure::Model => TeamProcessError::Model,
                    protocol::WorkerFailure::CheckpointMismatch => {
                        TeamProcessError::CheckpointMismatch
                    }
                    protocol::WorkerFailure::NotContinuable => TeamProcessError::NotContinuable,
                    protocol::WorkerFailure::RouteUnsupported => TeamProcessError::RouteUnsupported,
                    _ => TeamProcessError::Worker,
                });
            }
            WorkerFrame::Authorize { id } => {
                if !protocol::valid_id(&id) || !seen.insert(id.clone()) {
                    return Err(TeamProcessError::Protocol);
                }
                tokio::select! {biased;_=output.read(&mut unexpected)=>return Err(TeamProcessError::Protocol),result=broker.authorize(cancel.child_token())=>result?};
                ParentFrame::Authorized { id }
            }

            WorkerFrame::ReadChat { request } => {
                if private_data {
                    return Err(TeamProcessError::Protocol);
                }
                request.validate()?;
                if !seen.insert(request.id.clone()) {
                    return Err(TeamProcessError::Protocol);
                }
                let id = request.id.clone();
                broker.authorize(cancel.child_token()).await?;
                let result = tokio::select! {biased;_=output.read(&mut unexpected)=>return Err(TeamProcessError::Protocol),result=broker.read_chat(request.clone(),cancel.child_token())=>result?};
                result.validate(&request.query)?;
                ParentFrame::ChatRead {
                    id,
                    data: TeamDataRef::write(data_root, &result)?,
                }
            }
            WorkerFrame::Operate { request } => {
                if private_data {
                    return Err(TeamProcessError::Protocol);
                }
                request.validate(task_id, approval)?;
                if matches!(
                    request.tool,
                    TeamOperationTool::DataRead
                        | TeamOperationTool::DataWrite
                        | TeamOperationTool::DataHandoff
                ) && !data_tools::permitted(shared_data, request.tool, &request.arguments)
                {
                    return Err(TeamProcessError::Protocol);
                }
                if matches!(
                    request.tool,
                    TeamOperationTool::LocalAppTools | TeamOperationTool::LocalAppCall
                ) && !local_app_tools::permitted(local_apps, request.tool, &request.arguments)
                {
                    return Err(TeamProcessError::Protocol);
                }
                if request.tool == TeamOperationTool::Delegate
                    && !delegate_tool::permitted(delegates, &request.arguments)
                {
                    return Err(TeamProcessError::Protocol);
                }
                if !seen.insert(request.id.clone()) {
                    return Err(TeamProcessError::Protocol);
                }
                let id = request.id.clone();
                broker.authorize(cancel.child_token()).await?;
                let result = tokio::select! {biased;_=output.read(&mut unexpected)=>return Err(TeamProcessError::Protocol),result=broker.operate(request,cancel.child_token())=>result?};
                result.validate()?;
                ParentFrame::Operated { id, result }
            }
            WorkerFrame::ProviderAccountRefresh { request } => {
                if !protocol::valid_id(&request.id)
                    || uuid::Uuid::parse_str(&request.account_id).is_err()
                    || !seen.insert(request.id.clone())
                {
                    return Err(TeamProcessError::Protocol);
                }
                let id = request.id.clone();
                let credentials = tokio::select! {biased;_ = output.read(&mut unexpected)=>return Err(TeamProcessError::Protocol),result=broker.refresh_provider_account(request,cancel.child_token())=>result?};
                ParentFrame::ProviderAccountsUpdated {
                    id,
                    credentials: credentials.for_worker(
                        &data_root
                            .parent()
                            .ok_or(TeamProcessError::Protocol)?
                            .join("home"),
                    )?,
                }
            }
            WorkerFrame::Result { result } => {
                protocol::validate_result(&result, private_data)?;
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
            branding::CONFIG_DIR_ENV.into(),
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
        Err(error) => {
            let (code, status) = match error {
                TeamProcessError::Capacity => (protocol::WorkerFailure::Capacity, 65),
                TeamProcessError::ResourceLimit => (protocol::WorkerFailure::ResourceLimit, 65),
                TeamProcessError::Environment => (protocol::WorkerFailure::Environment, 78),
                TeamProcessError::Profile(_) => (protocol::WorkerFailure::Profile, 77),
                TeamProcessError::Protocol => (protocol::WorkerFailure::Protocol, 65),
                TeamProcessError::Model => (protocol::WorkerFailure::Model, 70),
                TeamProcessError::CheckpointMismatch => {
                    (protocol::WorkerFailure::CheckpointMismatch, 65)
                }
                TeamProcessError::NotContinuable => (protocol::WorkerFailure::NotContinuable, 65),
                TeamProcessError::RouteUnsupported => {
                    (protocol::WorkerFailure::RouteUnsupported, 65)
                }
                TeamProcessError::Io(_) => (protocol::WorkerFailure::Storage, 74),
                _ => (protocol::WorkerFailure::Worker, 70),
            };
            if let Ok(frame) = protocol::encode(&protocol::WorkerFrame::Failed { code }) {
                let _ = std::io::stdout().write_all(&frame);
                let _ = std::io::stdout().flush();
            }
            status
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::TeamUsage;
    use lingxi_core::host::CostSnapshot;

    fn snapshot(calls: u32, input: u64, output: u64, nano: u64, unknown: bool) -> CostSnapshot {
        CostSnapshot {
            api_calls: calls,
            input_tokens: input,
            output_tokens: output,
            total_nano_usd: nano,
            unknown_models: unknown,
            ..CostSnapshot::default()
        }
    }

    fn json(usage: &TeamUsage) -> serde_json::Value {
        serde_json::to_value(usage).unwrap()
    }

    #[test]
    fn no_new_model_call_or_tokens_is_missing() {
        let before = snapshot(2, 10, 5, 4_000, false);
        for settled in [true, false] {
            assert!(matches!(
                TeamUsage::from_snapshots(&before, &before, settled),
                TeamUsage::Missing
            ));
            // A call that reported no tokens is not observed usage.
            assert!(matches!(
                TeamUsage::from_snapshots(&before, &snapshot(3, 10, 5, 4_000, false), settled),
                TeamUsage::Missing
            ));
        }
    }

    #[test]
    fn settled_priced_task_is_complete_with_required_cost() {
        let before = snapshot(1, 10, 5, 4_000, false);
        let after = snapshot(2, 110, 25, 4_000 + 12_345_678, false);
        let usage = TeamUsage::from_snapshots(&before, &after, true);
        assert_eq!(
            json(&usage),
            serde_json::json!({
                "status": "complete",
                "inputTokens": 100,
                "outputTokens": 20,
                "costMicros": 12_345,
                "currency": "USD"
            })
        );
    }

    #[test]
    fn unsettled_task_stays_partial_but_keeps_known_cost() {
        let before = snapshot(0, 0, 0, 0, false);
        let after = snapshot(1, 7, 3, 9_000, false);
        assert_eq!(
            json(&TeamUsage::from_snapshots(&before, &after, false)),
            serde_json::json!({
                "status": "partial",
                "inputTokens": 7,
                "outputTokens": 3,
                "costMicros": 9,
                "currency": "USD"
            })
        );
    }

    #[test]
    fn unknown_model_or_backwards_cost_is_partial_without_cost() {
        let before = snapshot(0, 0, 0, 5_000, false);
        for after in [
            snapshot(1, 7, 3, 9_000, true),
            snapshot(1, 7, 3, 4_000, false),
        ] {
            assert_eq!(
                json(&TeamUsage::from_snapshots(&before, &after, true)),
                serde_json::json!({
                    "status": "partial",
                    "inputTokens": 7,
                    "outputTokens": 3
                })
            );
        }
    }

    #[test]
    fn complete_round_trips_and_rejects_missing_cost() {
        let usage = TeamUsage::from_snapshots(
            &snapshot(0, 0, 0, 0, false),
            &snapshot(1, 7, 3, 9_000, false),
            true,
        );
        let encoded = serde_json::to_string(&usage).unwrap();
        let decoded: TeamUsage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(json(&decoded), json(&usage));
        assert!(serde_json::from_str::<TeamUsage>(
            r#"{"status":"complete","inputTokens":1,"outputTokens":1}"#
        )
        .is_err());
    }
}

#[cfg(test)]
mod local_app_broker_tests {
    use super::protocol::{ParentFrame, WorkerFrame};
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        operations: Mutex<Vec<TeamOperationRequest>>,
    }
    #[async_trait::async_trait]
    impl TeamProcessBroker for Recorder {
        async fn authorize(&self, _: CancellationToken) -> Result<(), TeamProcessError> {
            Ok(())
        }
        async fn read_chat(
            &self,
            _: TeamChatReadRequest,
            _: CancellationToken,
        ) -> Result<TeamChatReadResult, TeamProcessError> {
            Err(TeamProcessError::Protocol)
        }
        async fn operate(
            &self,
            request: TeamOperationRequest,
            _: CancellationToken,
        ) -> Result<TeamOperationResult, TeamProcessError> {
            self.operations.lock().unwrap().push(request);
            Ok(TeamOperationResult {
                succeeded: true,
                result: json!({"content":[{"type":"text","text":"3 notes"}]}),
            })
        }
    }

    fn offers() -> Vec<TeamLocalAppOffer> {
        vec![TeamLocalAppOffer {
            id: "notes".into(),
            name: "Notes".into(),
            description: None,
        }]
    }
    fn listing(app: &str) -> TeamOperationRequest {
        TeamOperationRequest {
            id: "interaction-1".into(),
            tool: TeamOperationTool::LocalAppTools,
            logical_step_id: None,
            approval_ref: None,
            arguments: json!({"app":app}),
        }
    }
    fn call(app: &str, step: &str) -> TeamOperationRequest {
        TeamOperationRequest {
            id: "interaction-1".into(),
            tool: TeamOperationTool::LocalAppCall,
            logical_step_id: Some(step.into()),
            approval_ref: None,
            arguments: json!({"app":app,"tool":"search","arguments":{"q":"budget"}}),
        }
    }

    /// What the parent does with one request of the worker: the error it ends with
    /// (`None` when it answered) and every request that reached the host.
    async fn exchange(
        request: TeamOperationRequest,
        apps: &[TeamLocalAppOffer],
        private_data: bool,
    ) -> (Option<TeamProcessError>, Vec<TeamOperationRequest>) {
        exchange_with(request, apps, &[], private_data).await
    }
    async fn exchange_with(
        request: TeamOperationRequest,
        apps: &[TeamLocalAppOffer],
        delegates: &[TeamDelegateOffer],
        private_data: bool,
    ) -> (Option<TeamProcessError>, Vec<TeamOperationRequest>) {
        let broker = Arc::new(Recorder::default());
        // What the worker writes is what the parent reads, and the other way round.
        let (mut worker_out, mut parent_reads) = tokio::io::duplex(1 << 20);
        let (mut parent_writes, mut worker_reads) = tokio::io::duplex(1 << 20);
        let mut parent = std::pin::pin!(broker_exchange(
            &mut parent_reads,
            &mut parent_writes,
            broker.clone(),
            CancellationToken::new(),
            Path::new("/unused"),
            "task_1",
            private_data,
            &[],
            apps,
            delegates,
            TeamListApproval::None,
        ));
        let worker = async {
            worker_out
                .write_all(&protocol::encode(&WorkerFrame::Operate { request }).unwrap())
                .await
                .unwrap();
            worker_out.flush().await.unwrap();
            protocol::read_frame::<ParentFrame>(&mut worker_reads).await
        };
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                // The parent ended the exchange without answering.
                result = &mut parent => (result.err(), None),
                answered = worker => (None, Some(answered)),
            }
        })
        .await
        .expect("the exchange ends");
        let seen = broker.operations.lock().unwrap().clone();
        match outcome {
            (error, None) => (error, seen),
            (_, Some(answered)) => {
                let Ok(ParentFrame::Operated { id, result }) = answered else {
                    panic!("only an operation is answered");
                };
                assert_eq!(id, "interaction-1");
                assert!(result.succeeded);
                // The worker leaving without a result is the only way this exchange ends.
                drop(worker_out);
                let ended = tokio::time::timeout(Duration::from_secs(10), parent)
                    .await
                    .expect("the exchange ends");
                assert!(matches!(ended, Err(TeamProcessError::Io(_))), "{ended:?}");
                (None, seen)
            }
        }
    }

    #[tokio::test]
    async fn the_host_is_asked_only_for_offered_apps() {
        // An offered app: the request reaches the host exactly as the worker sent it.
        let (error, seen) = exchange(listing("notes"), &offers(), false).await;
        assert!(error.is_none());
        assert_eq!(seen, [listing("notes")]);
        let (error, seen) = exchange(call("notes", "task_1/t0/c1"), &offers(), false).await;
        assert!(error.is_none());
        assert_eq!(seen, [call("notes", "task_1/t0/c1")]);

        // An app that was not offered never reaches the host: the worker is ended.
        for request in [
            listing("calendar"),
            call("calendar", "task_1/t0/c1"),
            listing("Notes"),
        ] {
            let (error, seen) = exchange(request.clone(), &offers(), false).await;
            assert!(
                matches!(error, Some(TeamProcessError::Protocol)),
                "{request:?}"
            );
            assert!(seen.is_empty(), "{request:?}");
        }
        // Nothing offered, nothing reaches the host, however well formed the request is.
        for request in [listing("notes"), call("notes", "task_1/t0/c1")] {
            let (error, seen) = exchange(request.clone(), &[], false).await;
            assert!(
                matches!(error, Some(TeamProcessError::Protocol)),
                "{request:?}"
            );
            assert!(seen.is_empty());
        }
        // A malformed request, a call without its durable step or for another task, never does either.
        let mut stepless = call("notes", "task_1/t0/c1");
        stepless.logical_step_id = None;
        let foreign = call("notes", "task_2/t0/c1");
        let mut stepped_listing = listing("notes");
        stepped_listing.logical_step_id = Some("task_1/t0/c1".into());
        for request in [stepless, foreign, stepped_listing] {
            let (error, seen) = exchange(request.clone(), &offers(), false).await;
            assert!(
                matches!(error, Some(TeamProcessError::Protocol)),
                "{request:?}"
            );
            assert!(seen.is_empty(), "{request:?}");
        }
        // A private Data workspace has no operations at all.
        let (error, seen) = exchange(listing("notes"), &offers(), true).await;
        assert!(matches!(error, Some(TeamProcessError::Protocol)));
        assert!(seen.is_empty());
    }
    fn delegates() -> Vec<TeamDelegateOffer> {
        vec![TeamDelegateOffer {
            id: "sharedbot_research".into(),
            name: "Research".into(),
            description: None,
        }]
    }
    fn delegation(target: &str, step: Option<&str>) -> TeamOperationRequest {
        TeamOperationRequest {
            id: "interaction-1".into(),
            tool: TeamOperationTool::Delegate,
            logical_step_id: step.map(str::to_owned),
            approval_ref: None,
            arguments: json!({"target":target,"request":"Find the 2026 figures."}),
        }
    }

    #[tokio::test]
    async fn the_host_is_asked_only_to_delegate_to_offered_bots() {
        let ask = delegation("sharedbot_research", Some("task_1/t0/c1"));
        let (error, seen) = exchange_with(ask.clone(), &[], &delegates(), false).await;
        assert!(error.is_none());
        assert_eq!(seen, [ask]);

        // A Bot that was not offered never reaches the host, nor does anything when no Bot is offered.
        for (request, offers) in [
            (
                delegation("sharedbot_other", Some("task_1/t0/c1")),
                delegates(),
            ),
            (
                delegation("sharedbot_research", Some("task_1/t0/c1")),
                vec![],
            ),
        ] {
            let (error, seen) = exchange_with(request.clone(), &[], &offers, false).await;
            assert!(
                matches!(error, Some(TeamProcessError::Protocol)),
                "{request:?}"
            );
            assert!(seen.is_empty(), "{request:?}");
        }
        // A delegation without its durable step, for another task, with a flag of its own or in a private workspace never does.
        let mut flagged = delegation("sharedbot_research", Some("task_1/t0/c1"));
        flagged.arguments["expectReply"] = json!(false);
        for request in [
            delegation("sharedbot_research", None),
            delegation("sharedbot_research", Some("task_2/t0/c1")),
            flagged,
        ] {
            let (error, seen) = exchange_with(request.clone(), &[], &delegates(), false).await;
            assert!(
                matches!(error, Some(TeamProcessError::Protocol)),
                "{request:?}"
            );
            assert!(seen.is_empty(), "{request:?}");
        }
        let (error, seen) = exchange_with(
            delegation("sharedbot_research", Some("task_1/t0/c1")),
            &[],
            &delegates(),
            true,
        )
        .await;
        assert!(matches!(error, Some(TeamProcessError::Protocol)));
        assert!(seen.is_empty());
    }
}
