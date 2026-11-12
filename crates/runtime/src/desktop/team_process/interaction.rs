//! One correlated private exchange at a time. EOF revokes the process lease.
use super::protocol::{self, ParentFrame, WorkerFrame, MAX_FRAME};
use super::{
    TeamChatQuery, TeamChatReadRequest, TeamChatReadResult, TeamDataRef, TeamProcessError,
};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;
enum Pending {
    Authorize {
        id: String,
        reply: oneshot::Sender<()>,
    },
    Account {
        id: String,
        reply: oneshot::Sender<super::TeamAccountEnvelope>,
    },
    Chat {
        id: String,
        reply: oneshot::Sender<TeamDataRef>,
    },
    Operation {
        id: String,
        reply: oneshot::Sender<super::TeamOperationResult>,
    },
}
pub(super) struct ChildInteraction {
    next: AtomicU64,
    serial: AsyncMutex<()>,
    pending: Mutex<Option<Pending>>,
    pub cancel: CancellationToken,
    root: PathBuf,
    upper_seq: u64,
}
impl ChildInteraction {
    pub fn start(
        cancel: CancellationToken,
        root: PathBuf,
        upper_seq: u64,
    ) -> Result<Arc<Self>, TeamProcessError> {
        let bridge = Arc::new(Self {
            next: AtomicU64::new(1),
            serial: AsyncMutex::new(()),
            pending: Mutex::new(None),
            cancel,
            root,
            upper_seq,
        });
        let reader = bridge.clone();
        std::thread::Builder::new()
            .name("team-parent-lease".into())
            .spawn(move || {
                if reader.read_replies().is_err() {
                    reader.cancel.cancel();
                }
            })?;
        Ok(bridge)
    }
    fn id(&self) -> Result<String, TeamProcessError> {
        let n = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| TeamProcessError::Protocol)?;
        Ok(format!("interaction-{n}"))
    }
    fn read_replies(&self) -> Result<(), TeamProcessError> {
        let mut input = std::io::stdin();
        loop {
            let mut prefix = [0; 4];
            input.read_exact(&mut prefix)?;
            let n = u32::from_be_bytes(prefix) as usize;
            if n == 0 || n > MAX_FRAME {
                return Err(TeamProcessError::Protocol);
            }
            let mut bytes = vec![0; n];
            input.read_exact(&mut bytes)?;
            let response: ParentFrame =
                serde_json::from_slice(&bytes).map_err(|_| TeamProcessError::Protocol)?;
            let pending = self
                .pending
                .lock()
                .map_err(|_| TeamProcessError::Protocol)?
                .take()
                .ok_or(TeamProcessError::Protocol)?;
            match (pending, response) {
                (Pending::Authorize { id, reply }, ParentFrame::Authorized { id: got })
                    if id == got =>
                {
                    reply.send(()).map_err(|_| TeamProcessError::Protocol)?
                }
                (
                    Pending::Account { id, reply },
                    ParentFrame::ProviderAccountsUpdated {
                        id: got,
                        credentials,
                    },
                ) if id == got => reply
                    .send(credentials)
                    .map_err(|_| TeamProcessError::Protocol)?,
                (Pending::Chat { id, reply }, ParentFrame::ChatRead { id: got, data })
                    if id == got =>
                {
                    reply.send(data).map_err(|_| TeamProcessError::Protocol)?
                }
                (Pending::Operation { id, reply }, ParentFrame::Operated { id: got, result })
                    if id == got =>
                {
                    result.validate()?;
                    reply.send(result).map_err(|_| TeamProcessError::Protocol)?
                }
                _ => return Err(TeamProcessError::Protocol),
            }
        }
    }
    fn emit(&self, pending: Pending, frame: WorkerFrame) -> Result<(), TeamProcessError> {
        if self.cancel.is_cancelled() {
            return Err(TeamProcessError::Cancelled);
        }
        let bytes = protocol::encode(&frame)?;
        let mut slot = self
            .pending
            .lock()
            .map_err(|_| TeamProcessError::Protocol)?;
        if slot.is_some() {
            return Err(TeamProcessError::Protocol);
        }
        *slot = Some(pending);
        let mut out = std::io::stdout().lock();
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(())
    }
    pub async fn refresh_provider_account(
        &self,
        account: &lingxi_core::host::provider_accounts::AccountMetadata,
        revision: u64,
    ) -> Result<super::TeamAccountEnvelope, TeamProcessError> {
        let _serial = self.serial.lock().await;
        let id = self.id()?;
        let (reply, response) = oneshot::channel();
        self.emit(
            Pending::Account {
                id: id.clone(),
                reply,
            },
            WorkerFrame::ProviderAccountRefresh {
                request: super::TeamProviderAccountRefresh {
                    id,
                    account_id: account.account_id.clone(),
                    identity_generation: account.identity_generation,
                    credential_revision: revision,
                },
            },
        )?;
        tokio::select! {biased;_=self.cancel.cancelled()=>Err(TeamProcessError::Cancelled),response=response=>response.map_err(|_|TeamProcessError::Protocol)}
    }
    pub async fn authorize(&self) -> Result<(), TeamProcessError> {
        let _serial = self.serial.lock().await;
        let id = self.id()?;
        let (reply, response) = oneshot::channel();
        self.emit(
            Pending::Authorize {
                id: id.clone(),
                reply,
            },
            WorkerFrame::Authorize { id },
        )?;
        tokio::select! {biased;_=self.cancel.cancelled()=>Err(TeamProcessError::Cancelled),response=response=>response.map_err(|_|TeamProcessError::Protocol)}
    }
    /// One request to the host's shared-list service. An error means the
    /// outcome is not established and the parent is ending this process.
    pub async fn operate(
        &self,
        tool: super::TeamOperationTool,
        logical_step_id: Option<String>,
        arguments: serde_json::Value,
    ) -> Result<super::TeamOperationResult, TeamProcessError> {
        let _serial = self.serial.lock().await;
        let id = self.id()?;
        let (reply, response) = oneshot::channel();
        self.emit(
            Pending::Operation {
                id: id.clone(),
                reply,
            },
            WorkerFrame::Operate {
                request: super::TeamOperationRequest {
                    id,
                    tool,
                    logical_step_id,
                    arguments,
                },
            },
        )?;
        tokio::select! {biased;_=self.cancel.cancelled()=>Err(TeamProcessError::Cancelled),response=response=>response.map_err(|_|TeamProcessError::Protocol)}
    }
    pub async fn read_chat(
        &self,
        query: TeamChatQuery,
    ) -> Result<TeamChatReadResult, TeamProcessError> {
        query.validate_upper_bound(self.upper_seq)?;
        let _serial = self.serial.lock().await;
        let id = self.id()?;
        let request = TeamChatReadRequest {
            id: id.clone(),
            query: query.clone(),
        };
        request.validate()?;
        let (reply, response) = oneshot::channel();
        self.emit(
            Pending::Chat { id, reply },
            WorkerFrame::ReadChat { request },
        )?;
        let data = tokio::select! {biased;_=self.cancel.cancelled()=>return Err(TeamProcessError::Cancelled),response=response=>response.map_err(|_|TeamProcessError::Protocol)?};
        let result: TeamChatReadResult = data.read(&self.root)?;
        result.validate(&query)?;
        if result
            .messages
            .iter()
            .any(|message| message.seq > self.upper_seq)
        {
            return Err(TeamProcessError::Protocol);
        }
        Ok(result)
    }
}
