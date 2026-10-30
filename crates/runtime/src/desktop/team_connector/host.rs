//! Private product-main bridge. No public listener, owner tool authority from a
//! member, or human-answer message exists on this channel.
use super::{
    Error, TeamBotSnapshot, TeamBotTarget, TeamConnector, TeamConnectorAccount,
    TeamConnectorCredential, TeamConnectorStatus, TeamOwnerApproval,
};
use crate::desktop::team_invocation::TeamInvocationLimits;
use crate::desktop::team_process::{TeamOwnerPermission, TeamProcessModel};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const MAX_LINE: usize = 256 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    version: u32,
    #[serde(rename = "type")]
    kind: String,
    mode: String,
    owner_uid: String,
    account_epoch: u64,
    endpoint: String,
    journal_root: PathBuf,
    runtime_root: PathBuf,
    worker_executable: PathBuf,
    target: TeamBotTarget,
    snapshot: Option<Snapshot>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    instructions: String,
    model_id: String,
    model_endpoint: String,
    model_api_key: String,
    max_turns: u32,
    max_input_bytes: u32,
    output_tokens_per_request: u32,
    physical_dispatches: u32,
}
impl Snapshot {
    fn into_snapshot(self) -> Result<TeamBotSnapshot, Error> {
        let nonzero = |n| NonZeroU32::new(n).ok_or(Error::Configuration);
        Ok(TeamBotSnapshot {
            instructions: self.instructions,
            model: TeamProcessModel::anthropic_api_key(
                self.model_id,
                self.model_endpoint,
                self.model_api_key,
            )
            .map_err(|_| Error::Configuration)?,
            limits: TeamInvocationLimits {
                max_turns: nonzero(self.max_turns)?,
                max_input_bytes: nonzero(self.max_input_bytes)?,
                output_tokens_per_request: nonzero(self.output_tokens_per_request)?,
                physical_dispatches: nonzero(self.physical_dispatches)?,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Credential {
        version: u32,
        id: String,
        owner_uid: String,
        account_epoch: u64,
        id_token: String,
    },
    Permission {
        version: u32,
        id: String,
        allow: bool,
    },
}
impl Response {
    fn correlation(&self) -> (u32, &str, &'static str) {
        match self {
            Self::Credential { version, id, .. } => (*version, id, "credential"),
            Self::Permission { version, id, .. } => (*version, id, "permission"),
        }
    }
}
struct Pending {
    kind: &'static str,
    sender: oneshot::Sender<Response>,
}
struct Bridge {
    owner: String,
    epoch: u64,
    cancel: CancellationToken,
    counter: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    writer: Mutex<std::io::Stdout>,
}
impl Bridge {
    fn emit(&self, value: Value) -> Result<(), Error> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Error::Protocol)?;
        if bytes.len() > MAX_LINE {
            return Err(Error::Protocol);
        }
        bytes.push(b'\n');
        let mut output = self.writer.lock().map_err(|_| Error::Protocol)?;
        output.write_all(&bytes)?;
        output.flush()?;
        Ok(())
    }
    async fn request(
        &self,
        kind: &'static str,
        mut value: Value,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        if self.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(Error::Account);
        }
        let (tx, rx) = oneshot::channel();
        // Sequence assignment and write share one lock: concurrent credential
        // refresh and owner prompts cannot invert the stream's monotonic IDs.
        let id = {
            let mut output = self.writer.lock().map_err(|_| Error::Protocol)?;
            let sequence = self
                .counter
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| Error::Protocol)?;
            let id = format!("{kind}-{sequence}");
            value["version"] = json!(1);
            value["type"] = json!(kind);
            value["id"] = json!(id);
            let mut bytes = serde_json::to_vec(&value).map_err(|_| Error::Protocol)?;
            if bytes.len() > MAX_LINE {
                return Err(Error::Protocol);
            }
            bytes.push(b'\n');
            let mut pending = self.pending.lock().map_err(|_| Error::Protocol)?;
            if pending.len() >= 8 {
                return Err(Error::Protocol);
            }
            pending.insert(id.clone(), Pending { kind, sender: tx });
            drop(pending);
            if let Err(error) = output.write_all(&bytes).and_then(|_| output.flush()) {
                self.pending
                    .lock()
                    .map_err(|_| Error::Protocol)?
                    .remove(&id);
                return Err(error.into());
            }
            id
        };
        let result = tokio::select! {biased;
            _=self.cancel.cancelled()=>Err(Error::Account),
            _=cancel.cancelled()=>Err(Error::Cancelled),
            result=rx=>result.map_err(|_|Error::Protocol),
        };
        self.pending
            .lock()
            .map_err(|_| Error::Protocol)?
            .remove(&id);
        result
    }
    fn accept(&self, response: Response) -> Result<(), Error> {
        let (version, id, kind) = response.correlation();
        if version != 1 {
            return Err(Error::Protocol);
        }
        let pending = self
            .pending
            .lock()
            .map_err(|_| Error::Protocol)?
            .remove(id)
            .ok_or(Error::Protocol)?;
        if pending.kind != kind {
            return Err(Error::Protocol);
        }
        pending.sender.send(response).map_err(|_| Error::Protocol)
    }
}
#[async_trait]
impl TeamConnectorAccount for Bridge {
    fn is_current(&self, owner: &str, epoch: u64) -> bool {
        owner == self.owner && epoch == self.epoch && !self.cancel.is_cancelled()
    }
    async fn credential(&self) -> Result<TeamConnectorCredential, Error> {
        match self.request("credential", json!({}), &self.cancel).await? {
            Response::Credential {
                owner_uid,
                account_epoch,
                id_token,
                ..
            } => Ok(TeamConnectorCredential {
                owner_uid,
                epoch: account_epoch,
                id_token,
            }),
            _ => Err(Error::Protocol),
        }
    }
}
#[async_trait]
impl TeamOwnerApproval for Bridge {
    async fn approve(
        &self,
        request: TeamOwnerPermission,
        cancel: CancellationToken,
    ) -> Result<bool, Error> {
        if request.tool_name != "Bash" {
            return Ok(false);
        }
        match self
            .request(
                "permission",
                json!({"tool_name":request.tool_name,"input":request.input}),
                &cancel,
            )
            .await?
        {
            Response::Permission { allow, .. } => Ok(allow),
            _ => Err(Error::Protocol),
        }
    }
}

fn line<R: BufRead>(input: &mut R) -> Result<Option<Vec<u8>>, Error> {
    let mut bytes = Vec::new();
    let count = std::io::Read::take(input, (MAX_LINE + 2) as u64).read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if count > MAX_LINE + 1 || bytes.last() != Some(&b'\n') {
        return Err(Error::Protocol);
    }
    bytes.pop();
    if bytes.is_empty() {
        return Err(Error::Protocol);
    }
    Ok(Some(bytes))
}

fn entry() -> Result<(), Error> {
    if std::env::args_os().len() != 1
        || std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
    {
        return Err(Error::Configuration);
    }
    let keys = [
        "HOME",
        "LINGXI_CONFIG_DIR",
        "TMPDIR",
        "PATH",
        "LANG",
        "LC_ALL",
        "CFFIXED_USER_HOME",
        "__CF_USER_TEXT_ENCODING",
    ];
    if std::env::vars_os().count() != keys.len()
        || std::env::vars_os().any(|(k, _)| !keys.iter().any(|n| k == *n))
    {
        return Err(Error::Configuration);
    }
    // Fresh single-threaded entry: no inherited owner sockets/handles survive.
    let mut fds = Vec::new();
    for item in std::fs::read_dir("/dev/fd")? {
        if let Some(fd) = item?
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|fd| *fd > 2)
        {
            fds.push(fd);
        }
    }
    for fd in fds {
        match nix::unistd::close(fd) {
            Ok(()) | Err(nix::errno::Errno::EBADF) => (),
            Err(_) => return Err(Error::Configuration),
        }
    }
    let mut reader = std::io::BufReader::new(std::io::stdin());
    let boot: Start = serde_json::from_slice(&line(&mut reader)?.ok_or(Error::Protocol)?)
        .map_err(|_| Error::Protocol)?;
    if boot.version != 1
        || boot.kind != "start"
        || !matches!(boot.mode.as_str(), "run" | "retry_result")
        || boot.owner_uid != boot.target.owner_uid
        || boot.account_epoch > super::MAX_SAFE_INTEGER
    {
        return Err(Error::Configuration);
    }
    boot.target.validate()?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or(Error::Configuration)?);
    let tmp = PathBuf::from(std::env::var_os("TMPDIR").ok_or(Error::Configuration)?);
    super::journal::private_dir(&home)?;
    super::journal::private_dir(&tmp)?;
    if std::env::var_os("LINGXI_CONFIG_DIR") != Some(home.clone().into_os_string())
        || std::env::var_os("CFFIXED_USER_HOME") != Some(home.clone().into_os_string())
        || std::env::var("PATH").ok().as_deref() != Some("/usr/bin:/bin:/usr/sbin:/sbin")
        || std::env::var("LANG").ok().as_deref() != Some("C")
        || std::env::var("LC_ALL").ok().as_deref() != Some("C")
        || std::env::var("__CF_USER_TEXT_ENCODING").ok()
            != Some(format!(
                "0x{:X}:0x8000100:0x0",
                nix::unistd::geteuid().as_raw()
            ))
    {
        return Err(Error::Configuration);
    }
    let bridge = Arc::new(Bridge {
        owner: boot.owner_uid.clone(),
        epoch: boot.account_epoch,
        cancel: CancellationToken::new(),
        counter: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        writer: Mutex::new(std::io::stdout()),
    });
    let port = bridge.clone();
    std::thread::Builder::new()
        .name("team-host-parent".into())
        .spawn(move || {
            let mut operation = || -> Result<(), Error> {
                while let Some(bytes) = line(&mut reader)? {
                    let response = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
                    port.accept(response)?;
                }
                Ok(())
            };
            let _ = operation();
            port.cancel.cancel();
            if let Ok(mut pending) = port.pending.lock() {
                pending.clear();
            }
        })?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let stop = bridge.cancel.clone();
        let watcher = tokio::spawn(async move {
            if let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
                stop.cancel();
            }
        });
        let connector = TeamConnector::new(
            &boot.endpoint,
            bridge.clone(),
            boot.owner_uid,
            boot.account_epoch,
            bridge.cancel.clone(),
            boot.journal_root,
            boot.runtime_root,
            boot.worker_executable,
        )?
        .with_owner_approval(bridge.clone());
        let result = if boot.mode == "retry_result" {
            connector
                .observe_saved(boot.target, bridge.cancel.child_token())
                .await
        } else {
            let snapshot = boot.snapshot.map(Snapshot::into_snapshot).transpose()?;
            connector
                .run_optional(boot.target, snapshot, bridge.cancel.child_token())
                .await
        };
        watcher.abort();
        result
    });
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    match result {
        Ok(status) => {
            let mut value = match status {
                TeamConnectorStatus::Unstarted => json!({"status":"unstarted"}),
                TeamConnectorStatus::Busy => json!({"status":"busy"}),
                TeamConnectorStatus::RecoveryRequired => json!({"status":"recovery_required"}),
                TeamConnectorStatus::Settled { state, reply_id } => {
                    let mut v = json!({"status":"settled","state":state});
                    if let Some(id) = reply_id {
                        v["reply_id"] = json!(id);
                    }
                    v
                }
            };
            value["version"] = json!(1);
            value["type"] = json!("result");
            bridge.emit(value)
        }
        Err(error) => {
            let code = match error {
                Error::Account => "ACCOUNT_CHANGED".to_owned(),
                Error::ModelUnavailable => "MODEL_UNAVAILABLE".into(),
                Error::ResultPending => "RESULT_PENDING".into(),
                Error::Http(status) => format!("HTTP_{status}"),
                Error::Busy => "BUSY".into(),
                Error::Cancelled => "CANCELLED".into(),
                Error::Transport => "SERVICE_UNAVAILABLE".into(),
                Error::Protocol => "PROTOCOL".into(),
                Error::Storage | Error::Io(_) => "STORAGE".into(),
                Error::Configuration => "CONFIGURATION".into(),
            };
            bridge.emit(json!({"version":1,"type":"error","code":code}))?;
            Err(Error::Cancelled)
        }
    }
}

/// Entry of the dedicated host executable, never called in an existing app.
pub fn host_main() -> i32 {
    match entry() {
        Ok(()) => 0,
        Err(_) => 70,
    }
}
