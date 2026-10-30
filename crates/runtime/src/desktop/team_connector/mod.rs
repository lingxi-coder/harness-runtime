//! Disabled trusted-host Team executor. Backend authentication stays in this
//! parent; only the selected model key reaches the dedicated all-tools-denied
//! worker. No renderer/RC/owner Bot launch or public IPC registration exists.

mod http;
mod journal;

use super::team_invocation::{TeamInvocationIdentity, TeamInvocationLimits};
use super::team_process::{
    TeamProcessDeadline, TeamProcessModel, TeamProcessOutcome, TeamProcessRequest, TeamProcessStart,
};
use http::Client;
pub use http::{TeamConnectorAccount, TeamConnectorCredential};
use journal::Journal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Host-selected binding and backend queue identity. This metadata grants no
/// execution authority: only a fresh authenticated start response can do so.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TeamBotTarget {
    pub owner_uid: String,
    pub owner_generation: u64,
    pub host_id: String,
    pub bot_id: String,
    pub bot_revision: u64,
    pub team_id: String,
    pub conversation_id: String,
    pub bot_key: String,
    pub trigger_id: String,
}
use TeamBotTarget as Target;

impl Target {
    fn validate(&self) -> Result<(), Error> {
        if !identifier(&self.owner_uid, 1, 128)
            || !identifier(&self.host_id, 22, 22)
            || !identifier(&self.bot_id, 22, 22)
            || !identifier(&self.team_id, 8, 64)
            || !identifier(&self.conversation_id, 8, 64)
            || !identifier(&self.trigger_id, 8, 64)
            || !positive(self.owner_generation)
            || !positive(self.bot_revision)
            || self.bot_key != format!("bot:{}:{}:{}", self.owner_uid, self.host_id, self.bot_id)
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }

    fn command(&self, action: &str, attempt_id: &str) -> serde_json::Value {
        json!({ "action": action, "teamId": self.team_id, "generation": self.owner_generation,
            "botKey": self.bot_key, "conversationId": self.conversation_id,
            "triggerId": self.trigger_id, "attemptId": attempt_id })
    }
}

pub(super) fn identifier(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn positive(value: u64) -> bool {
    value > 0 && value <= MAX_SAFE_INTEGER
}
fn human_uid(value: &str) -> bool {
    !value.is_empty()
        && value.encode_utf16().count() <= 128
        && !value
            .chars()
            .any(|c| c == '/' || c <= '\u{1f}' || c == '\u{7f}')
}

/// Already frozen by the host, independent of ordinary owner Bot settings.
/// Original human text and principal are taken solely from the backend permit.
pub struct TeamBotSnapshot {
    pub instructions: String,
    pub limits: TeamInvocationLimits,
    pub model: TeamProcessModel,
}
impl TeamBotSnapshot {
    fn validate(&self) -> Result<(), Error> {
        if self.instructions.trim().is_empty()
            || self.instructions.len() > 65536
            || self.instructions.contains('\0')
            || self.limits.max_turns.get() > 20
            || self.limits.max_input_bytes.get() > 32768
            || self.limits.output_tokens_per_request.get() > 4096
            || self.limits.physical_dispatches.get() > 32
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }
}

/// Fixed diagnostics never contain an ID token, provider key, prompt or body.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid trusted Team connector configuration")]
    Configuration,
    #[error("Team owner account changed or signed out")]
    Account,
    #[error("Team connector cancelled")]
    Cancelled,
    #[error("Team backend transport unavailable; no execution replay is permitted")]
    Transport,
    #[error("Team backend HTTP status {0}")]
    Http(u16),
    #[error("invalid Team backend protocol")]
    Protocol,
    #[error("Team connector journal is invalid; owner recovery is required")]
    Storage,
    #[error("Team invocation is already owned by a local connector")]
    Busy,
    #[error("Team connector storage operation failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamBotState {
    Claimed,
    Running,
    Busy,
    Recovery,
    Completed,
    Failed,
    Missed,
    Cancelled,
    Recovered,
    Revoked,
}
impl TeamBotState {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Missed
                | Self::Cancelled
                | Self::Recovered
                | Self::Revoked
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TeamConnectorStatus {
    Settled {
        state: TeamBotState,
        reply_id: Option<String>,
    },
    Busy,
    /// A start may have happened. No new claim, worker or quota is minted.
    RecoveryRequired,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    uid: String,
    generation: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Execution {
    version: u32,
    team_id: String,
    conversation_id: String,
    bot_key: String,
    trigger_id: String,
    attempt_id: String,
    owner_uid: String,
    owner_generation: u64,
    host_id: String,
    bot_id: String,
    bot_revision: u64,
    origin: Origin,
    message_id: String,
    seq: u64,
    text: String,
}
impl Execution {
    fn validate(&self, journal: &Journal) -> Result<(), Error> {
        let t = &journal.intent.target;
        let trigger =
            serde_json::to_vec(&(&self.message_id, &t.bot_key)).map_err(|_| Error::Protocol)?;
        if self.version != 1
            || self.team_id != t.team_id
            || self.conversation_id != t.conversation_id
            || self.bot_key != t.bot_key
            || self.trigger_id != t.trigger_id
            || self.attempt_id != journal.intent.attempt_id
            || self.owner_uid != t.owner_uid
            || self.owner_generation != t.owner_generation
            || self.host_id != t.host_id
            || self.bot_id != t.bot_id
            || self.bot_revision != t.bot_revision
            || !human_uid(&self.origin.uid)
            || !positive(self.origin.generation)
            || !identifier(&self.message_id, 8, 64)
            || !positive(self.seq)
            || format!("{:x}", Sha256::digest(trigger)) != t.trigger_id
            || self.text.is_empty()
            || self.text.len() > 32768
            || self.text.contains('\0')
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

/// Different actions share terminal receipts; only start's true+snapshot grants
/// authority. Unknown response fields fail closed for this first-release wire.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Reply {
    state: TeamBotState,
    execute: Option<bool>,
    lease_until: Option<u64>,
    execution: Option<Execution>,
    reply_id: Option<String>,
    origin: Option<Origin>,
    text: Option<String>,
    message_id: Option<String>,
    seq: Option<u64>,
    question_id: Option<String>,
    answers: Option<std::collections::HashMap<String, String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PermitRecord {
    version: u32,
    lease_until: u64,
    execution: Execution,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settlement {
    version: u32,
    outcome: String,
    text: String,
}
impl Settlement {
    fn validate(&self) -> Result<(), Error> {
        if self.version != 1
            || !matches!(self.outcome.as_str(), "completed" | "failed")
            || self.text.trim().is_empty()
            || self.text.len() > 8192
            || self.text.contains('\0')
        {
            return Err(Error::Storage);
        }
        Ok(())
    }
}

/// Host-only driver. Construction does not start a listener/poller/worker.
/// A stable private journal and runtime root must survive application restarts.
pub struct TeamConnector {
    client: Client,
    journal_root: PathBuf,
    runtime_root: PathBuf,
    worker_executable: PathBuf,
}
impl TeamConnector {
    pub fn new(
        endpoint: &str,
        account: Arc<dyn TeamConnectorAccount>,
        owner_uid: String,
        account_epoch: u64,
        session_cancel: CancellationToken,
        journal_root: PathBuf,
        runtime_root: PathBuf,
        worker_executable: PathBuf,
    ) -> Result<Self, Error> {
        journal::private_dir(&journal_root)?;
        journal::private_dir(&runtime_root)?;
        if journal_root == runtime_root {
            return Err(Error::Configuration);
        }
        let client = Client::new(endpoint, account, owner_uid, account_epoch, session_cancel)?;
        Ok(Self {
            client,
            journal_root,
            runtime_root,
            worker_executable,
        })
    }

    fn open(&self, target: Target) -> Result<Journal, Error> {
        target.validate()?;
        self.client.check(&target)?;
        Journal::open(&self.journal_root, &self.client.endpoint, target)
    }

    /// Drive one backend queue identity. Claim alone never supplies worker
    /// input. Persist start intent BEFORE sending start; any ambiguity refuses
    /// execution after restart. A fresh true permit is consumed in this call.
    pub async fn run(
        &self,
        target: Target,
        snapshot: TeamBotSnapshot,
        cancel: CancellationToken,
    ) -> Result<TeamConnectorStatus, Error> {
        snapshot.validate()?;
        let journal = self.open(target)?;
        if journal.read::<Settlement>("result.json")?.is_some() {
            return self.finish_saved(&journal, &cancel).await;
        }
        if journal.read::<bool>("start-requested.json")?.is_some() {
            return self.observe_abandoned(&journal, &cancel).await;
        }
        let t = &journal.intent.target;
        let claim: Reply = self
            .client
            .call(
                t,
                t.command("claimBot", &journal.intent.attempt_id),
                &cancel,
            )
            .await?;
        if claim.state != TeamBotState::Claimed {
            return receipt(t, claim);
        }
        // Even failure to send the request now is deliberately conservative:
        // the immutable marker is never automatically deleted or overwritten.
        journal.write("start-requested.json", &true)?;
        let started: Reply = self
            .client
            .call(
                t,
                t.command("startBot", &journal.intent.attempt_id),
                &cancel,
            )
            .await?;
        if started.state != TeamBotState::Running {
            return receipt(t, started);
        }
        if started.execute != Some(true) {
            return Ok(TeamConnectorStatus::RecoveryRequired);
        }
        let lease_until = started.lease_until.ok_or(Error::Protocol)?;
        let deadline = TeamProcessDeadline::from_backend_unix_millis(lease_until)
            .map_err(|_| Error::Protocol)?;
        let execution = started.execution.ok_or(Error::Protocol)?;
        execution.validate(&journal)?;
        journal.write(
            "permit.json",
            &PermitRecord {
                version: 1,
                lease_until,
                execution,
            },
        )?;
        // Read the synced snapshot back before granting the worker authority.
        let permit = saved_permit(&journal)?;
        self.client.check(t)?;
        let e = permit.execution;
        let start = TeamProcessStart::prepare(
            &self.runtime_root,
            TeamProcessRequest {
                identity: TeamInvocationIdentity {
                    owner_uid: e.owner_uid,
                    bot_key: e.bot_key,
                    team_id: e.team_id,
                    conversation_id: e.conversation_id,
                    origin_human_uid: e.origin.uid,
                    membership_generation: e.origin.generation,
                },
                invocation_id: journal.intent.invocation_id,
                instructions: snapshot.instructions,
                input: e.text,
                limits: snapshot.limits,
                model: snapshot.model,
                deadline,
            },
        )
        .map_err(|_| Error::Configuration)?;
        let worker_cancel = self.client.session_cancel.child_token();
        let worker = start.run(&self.worker_executable, worker_cancel.clone());
        tokio::pin!(worker);
        let monitor = self.monitor(&journal, lease_until, deadline, &cancel);
        tokio::pin!(monitor);
        let result = tokio::select! {
            biased;
            _ = self.client.session_cancel.cancelled() => {
                worker_cancel.cancel(); let _ = worker.await; return Err(Error::Account);
            },
            _ = cancel.cancelled() => {
                worker_cancel.cancel(); let _ = worker.await; return Err(Error::Cancelled);
            },
            status = &mut monitor => {
                worker_cancel.cancel(); let _ = worker.await; return status;
            },
            result = &mut worker => result,
        };
        let result = match result {
            Ok(result) => result,
            Err(_) => return Ok(TeamConnectorStatus::RecoveryRequired),
        };
        self.client.check(t)?;
        let settlement = if result.outcome == TeamProcessOutcome::Completed {
            Settlement {
                version: 1,
                outcome: "completed".into(),
                text: result.text,
            }
        } else {
            Settlement { version: 1, outcome: "failed".into(),
                text: "The Bot could not complete this request within its execution limits. Please send a new request.".into() }
        };
        settlement.validate()?;
        journal.write("result.json", &settlement)?;
        self.finish_saved(&journal, &cancel).await
    }

    /// Retry ONLY the exact durable result. No snapshot/model credential is
    /// required, no claim/start is sent and no worker can be prepared here.
    pub async fn retry_result(
        &self,
        target: Target,
        cancel: CancellationToken,
    ) -> Result<TeamConnectorStatus, Error> {
        let journal = self.open(target)?;
        self.finish_saved(&journal, &cancel).await
    }

    async fn finish_saved(
        &self,
        journal: &Journal,
        cancel: &CancellationToken,
    ) -> Result<TeamConnectorStatus, Error> {
        saved_permit(journal)?;
        let result: Settlement = journal.read("result.json")?.ok_or(Error::Storage)?;
        result.validate()?;
        let t = &journal.intent.target;
        let mut command = t.command("finishBot", &journal.intent.attempt_id);
        command["text"] = json!(result.text);
        command["outcome"] = json!(result.outcome);
        let reply: Reply = self.client.call(t, command, cancel).await?;
        receipt(t, reply)
    }

    async fn observe_abandoned(
        &self,
        journal: &Journal,
        cancel: &CancellationToken,
    ) -> Result<TeamConnectorStatus, Error> {
        let t = &journal.intent.target;
        let reply: Reply = match self
            .client
            .call(t, t.command("botPoll", &journal.intent.attempt_id), cancel)
            .await
        {
            Ok(reply) => reply,
            // In particular an expired claimed lease after a start intent
            // that never reached the server. Only explicit owner recovery
            // may settle it; a 409 can never authorize another start.
            Err(Error::Http(409)) => return Ok(TeamConnectorStatus::RecoveryRequired),
            Err(error) => return Err(error),
        };
        receipt(t, reply)
    }

    async fn monitor(
        &self,
        journal: &Journal,
        lease: u64,
        deadline: TeamProcessDeadline,
        cancel: &CancellationToken,
    ) -> Result<TeamConnectorStatus, Error> {
        let t = &journal.intent.target;
        loop {
            let reply: Reply = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline.instant().into()) => return Ok(TeamConnectorStatus::RecoveryRequired),
                value = self.client.call(t, t.command("botPoll", &journal.intent.attempt_id), cancel) => value?,
            };
            if reply.state != TeamBotState::Running {
                return receipt(t, reply);
            }
            if reply.lease_until != Some(lease)
                || reply.execute.is_some()
                || reply.execution.is_some()
            {
                return Err(Error::Protocol);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

fn saved_permit(journal: &Journal) -> Result<PermitRecord, Error> {
    if journal.read::<bool>("start-requested.json")? != Some(true) {
        return Err(Error::Storage);
    }
    let permit: PermitRecord = journal.read("permit.json")?.ok_or(Error::Storage)?;
    if permit.version != 1 || !positive(permit.lease_until) {
        return Err(Error::Storage);
    }
    permit.execution.validate(journal)?;
    Ok(permit)
}

fn receipt(t: &Target, reply: Reply) -> Result<TeamConnectorStatus, Error> {
    if reply.state.terminal() {
        let expected = format!(
            "bot_{}",
            &format!("{:x}", Sha256::digest(format!("{}:reply", t.trigger_id)))[..60]
        );
        if reply.state != TeamBotState::Revoked && reply.reply_id.as_deref() != Some(&expected)
            || reply.reply_id.as_ref().is_some_and(|id| id != &expected)
        {
            return Err(Error::Protocol);
        }
        return Ok(TeamConnectorStatus::Settled {
            state: reply.state,
            reply_id: reply.reply_id,
        });
    }
    match reply.state {
        TeamBotState::Busy => Ok(TeamConnectorStatus::Busy),
        TeamBotState::Recovery | TeamBotState::Running => Ok(TeamConnectorStatus::RecoveryRequired),
        _ => Err(Error::Protocol),
    }
}
