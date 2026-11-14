//! Strict anonymous-pipe bridge from trusted Desktop main into the isolated worker.
use crate::desktop::team_invocation::{BotChatScope, TeamInvocationLimits};
use crate::desktop::team_process::{
    self, TeamAccountEnvelope, TeamChatReadRequest, TeamChatReadResult, TeamContinuation,
    TeamDataRef, TeamKnowledgeDocument, TeamModelRoute, TeamProcessBroker, TeamProcessDeadline,
    TeamProcessError, TeamProcessModel, TeamProcessOutcome, TeamProcessRequest, TeamProcessStart,
    TeamProviderAccountRefresh,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
const MAX_LINE: usize = 4 * 1024 * 1024;
type Error = TeamProcessError;
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Start {
    version: u32,
    #[serde(rename = "type")]
    kind: String,
    storage_root: PathBuf,
    worker_executable: PathBuf,
    scope: BotChatScope,
    task_id: Option<String>,
    invocation_id: Option<uuid::Uuid>,
    instructions: Option<String>,
    input_batch_ref: Option<TeamDataRef>,
    knowledge: Option<Vec<TeamKnowledgeDocument>>,
    deadline_at: Option<u64>,
    model: Option<Model>,
    credentials: Option<TeamAccountEnvelope>,
    limits: Option<Limits>,
    continuation: Option<TeamContinuation>,
    ask_requester: Option<bool>,
    shared_list: Option<team_process::TeamSharedList>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Model {
    model_id: String,
    model_provider: String,
    model_protocol: lingxi_llm_client::protocol::ProtocolFamily,
    model_profile: lingxi_llm_client::protocol::ProviderProfile,
    model_endpoint: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    max_turns: u32,
    context_window_tokens: u32,
    output_tokens_per_request: u32,
    physical_dispatches: u32,
}
impl Start {
    fn validate(&self) -> Result<(), Error> {
        if self.version != 3
            || !matches!(
                self.kind.as_str(),
                "inspect_scope" | "inspect_result" | "inspect_task" | "run" | "continue"
            )
        {
            return Err(Error::Protocol);
        }
        self.scope.session_id().map_err(|_| Error::Protocol)?;
        private_dir(&self.storage_root)?;
        let any = self.invocation_id.is_some()
            || self.instructions.is_some()
            || self.input_batch_ref.is_some()
            || self.knowledge.is_some()
            || self.deadline_at.is_some()
            || self.model.is_some()
            || self.credentials.is_some()
            || self.limits.is_some()
            || self.continuation.is_some()
            || self.ask_requester.is_some()
            || self.shared_list.is_some();
        let all = self.task_id.is_some()
            && self.invocation_id.is_some()
            && self.instructions.is_some()
            && self.input_batch_ref.is_some()
            && self.knowledge.is_some()
            && self.deadline_at.is_some()
            && self.model.is_some()
            && self.credentials.is_some()
            && self.limits.is_some();
        if self.kind == "inspect_scope" && (any || self.task_id.is_some())
            || self.kind == "inspect_result" && (any || self.task_id.is_none())
            || self.kind == "run" && (!all || self.continuation.is_some())
            || self.kind == "continue" && (!all || self.continuation.is_none())
        {
            return Err(Error::Protocol);
        }
        if self.kind == "inspect_task" {
            // Read-only: the route is needed only to say whether continuation
            // is enabled for it. No credentials, endpoint use or model call.
            let only_task_and_model = self.task_id.is_some()
                && self.model.is_some()
                && self.invocation_id.is_none()
                && self.instructions.is_none()
                && self.input_batch_ref.is_none()
                && self.knowledge.is_none()
                && self.deadline_at.is_none()
                && self.credentials.is_none()
                && self.limits.is_none()
                && self.continuation.is_none()
                && self.ask_requester.is_none()
                && self.shared_list.is_none();
            if !only_task_and_model {
                return Err(Error::Protocol);
            }
        }
        if let Some(continuation) = &self.continuation {
            if self.kind != "continue" {
                return Err(Error::Protocol);
            }
            // Re-checked by the runtime; refuse malformed epochs before any I/O.
            if continuation.epoch < 2 || continuation.expected_checkpoint_digest.len() != 64 {
                return Err(Error::Protocol);
            }
        }
        Ok(())
    }
    fn request(self) -> Result<TeamProcessRequest, Error> {
        let model = self.model.ok_or(Error::Protocol)?;
        let limits = self.limits.ok_or(Error::Protocol)?;
        let nonzero = |n| NonZeroU32::new(n).ok_or(Error::Protocol);
        Ok(TeamProcessRequest {
            scope: self.scope,
            task_id: self.task_id.ok_or(Error::Protocol)?,
            invocation_id: self.invocation_id.ok_or(Error::Protocol)?,
            instructions: self.instructions.ok_or(Error::Protocol)?,
            input_batch_ref: self.input_batch_ref.ok_or(Error::Protocol)?,
            knowledge: self.knowledge.ok_or(Error::Protocol)?,
            limits: TeamInvocationLimits {
                max_turns: nonzero(limits.max_turns)?,
                context_window_tokens: nonzero(limits.context_window_tokens)?,
                output_tokens_per_request: nonzero(limits.output_tokens_per_request)?,
                physical_dispatches: nonzero(limits.physical_dispatches)?,
            },
            model: TeamProcessModel::selected_account_pool(
                TeamModelRoute {
                    model_provider: model.model_provider,
                    model_protocol: model.model_protocol,
                    model_id: model.model_id,
                    model_endpoint: model.model_endpoint,
                    model_profile: model.model_profile,
                    credential_id: team_process::model::PRIVATE_CREDENTIAL.into(),
                },
                self.credentials.ok_or(Error::Protocol)?,
            )?,
            deadline: TeamProcessDeadline::from_backend_unix_millis(
                self.deadline_at.ok_or(Error::Protocol)?,
            )?,
            ask_requester: self.ask_requester.unwrap_or(false),
            shared_list: self.shared_list,
        })
    }
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    ProviderAccountsUpdated {
        version: u32,
        id: String,
        credentials: TeamAccountEnvelope,
    },
    ChatRead {
        version: u32,
        id: String,
        reference: TeamDataRef,
    },
    Authorized {
        version: u32,
        id: String,
    },
    /// `established` is false when Desktop could not learn what the list
    /// service did: the worker then ends with the call unfinished.
    OperationResult {
        version: u32,
        id: String,
        established: bool,
        succeeded: bool,
        result: Value,
    },
}
impl Response {
    fn correlation(&self) -> (u32, &str, &'static str) {
        match self {
            Self::ProviderAccountsUpdated { version, id, .. } => {
                (*version, id, "provider_account_refresh")
            }
            Self::ChatRead { version, id, .. } => (*version, id, "read_chat"),
            Self::Authorized { version, id } => (*version, id, "authorize"),
            Self::OperationResult { version, id, .. } => (*version, id, "operation"),
        }
    }
}
struct Pending {
    kind: &'static str,
    sender: oneshot::Sender<Response>,
}
struct Bridge {
    root: PathBuf,
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
        let mut out = self.writer.lock().map_err(|_| Error::Protocol)?;
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(())
    }
    async fn request(
        &self,
        kind: &'static str,
        mut value: Value,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        if self.cancel.is_cancelled() || cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let (sender, receiver) = oneshot::channel();
        let id = {
            let mut out = self.writer.lock().map_err(|_| Error::Protocol)?;
            let number = self
                .counter
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| Error::Protocol)?;
            let id = format!("{kind}-{number}");
            value["version"] = json!(3);
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
            pending.insert(id.clone(), Pending { kind, sender });
            drop(pending);
            out.write_all(&bytes)?;
            out.flush()?;
            id
        };
        let result = tokio::select! {biased;_=self.cancel.cancelled()=>Err(Error::Cancelled),_=cancel.cancelled()=>Err(Error::Cancelled),value=receiver=>value.map_err(|_|Error::Protocol)};
        self.pending
            .lock()
            .map_err(|_| Error::Protocol)?
            .remove(&id);
        result
    }
    fn accept(&self, response: Response) -> Result<(), Error> {
        let (version, id, kind) = response.correlation();
        if version != 3 {
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
impl TeamProcessBroker for Bridge {
    async fn authorize(&self, cancel: CancellationToken) -> Result<(), Error> {
        match self.request("authorize", json!({}), &cancel).await? {
            Response::Authorized { .. } => Ok(()),
            _ => Err(Error::Protocol),
        }
    }
    async fn refresh_provider_account(
        &self,
        request: TeamProviderAccountRefresh,
        cancel: CancellationToken,
    ) -> Result<TeamAccountEnvelope, Error> {
        match self.request("provider_account_refresh",json!({"account_id":request.account_id,"identity_generation":request.identity_generation,"credential_revision":request.credential_revision}),&cancel).await?{Response::ProviderAccountsUpdated{credentials,..}=>Ok(credentials),_=>Err(Error::Protocol)}
    }
    async fn operate(
        &self,
        request: team_process::TeamOperationRequest,
        cancel: CancellationToken,
    ) -> Result<team_process::TeamOperationResult, Error> {
        let mut value = json!({"tool":request.tool,"arguments":request.arguments});
        if let Some(step) = request.logical_step_id {
            value["logicalStepId"] = json!(step);
        }
        if let Some(wait) = request.approval_ref {
            value["approvalRef"] = json!(wait);
        }
        match self.request("operation", value, &cancel).await? {
            Response::OperationResult {
                established: true,
                succeeded,
                result,
                ..
            } => Ok(team_process::TeamOperationResult { succeeded, result }),
            // Not a protocol fault: the outcome is unknown and must not be replayed blindly.
            Response::OperationResult { .. } => Err(Error::Worker),
            _ => Err(Error::Protocol),
        }
    }
    async fn read_chat(
        &self,
        request: TeamChatReadRequest,
        cancel: CancellationToken,
    ) -> Result<TeamChatReadResult, Error> {
        request.validate()?;
        match self
            .request("read_chat", json!({"query":request.query}), &cancel)
            .await?
        {
            Response::ChatRead { reference, .. } => {
                let result: TeamChatReadResult = reference.read(&self.root)?;
                result.validate(&request.query)?;
                Ok(result)
            }
            _ => Err(Error::Protocol),
        }
    }
}
fn private_dir(path: &Path) -> Result<(), Error> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || path.canonicalize()? != path {
        return Err(Error::Environment);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(Error::Environment);
        }
    }
    Ok(())
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
        return Err(Error::Environment);
    }
    let keys = [
        "HOME",
        branding::CONFIG_DIR_ENV,
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
        return Err(Error::Environment);
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or(Error::Environment)?);
    let tmp = PathBuf::from(std::env::var_os("TMPDIR").ok_or(Error::Environment)?);
    private_dir(&home)?;
    private_dir(&tmp)?;
    if std::env::var_os(branding::CONFIG_DIR_ENV) != Some(home.clone().into_os_string())
        || std::env::var_os("CFFIXED_USER_HOME") != Some(home.into_os_string())
        || std::env::var("PATH").ok().as_deref() != Some("/usr/bin:/bin:/usr/sbin:/sbin")
        || std::env::var("LANG").ok().as_deref() != Some("C")
        || std::env::var("LC_ALL").ok().as_deref() != Some("C")
        || std::env::var("__CF_USER_TEXT_ENCODING").ok()
            != Some(format!(
                "0x{:X}:0x8000100:0x0",
                nix::unistd::geteuid().as_raw()
            ))
    {
        return Err(Error::Environment);
    }
    // Single-threaded preflight closes every inherited descriptor other than the private pipe.
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
            Err(_) => return Err(Error::Environment),
        }
    }
    let mut reader = std::io::BufReader::new(std::io::stdin());
    let boot: Start = serde_json::from_slice(&line(&mut reader)?.ok_or(Error::Protocol)?)
        .map_err(|_| Error::Protocol)?;
    boot.validate()?;
    let bridge = Arc::new(Bridge {
        root: boot.storage_root.clone(),
        cancel: CancellationToken::new(),
        counter: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        writer: Mutex::new(std::io::stdout()),
    });
    let port = bridge.clone();
    std::thread::Builder::new()
        .name("shared-bot-parent".into())
        .spawn(move || {
            let mut operation = || -> Result<(), Error> {
                while let Some(bytes) = line(&mut reader)? {
                    port.accept(serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?)?;
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
        if boot.kind == "inspect_scope" {
            let cursor = team_process::inspect_scope(&boot.storage_root, &boot.scope)?;
            return bridge
                .emit(json!({"version":3,"type":"result","status":"inspected","cursor":cursor}));
        }
        if boot.kind == "inspect_result" {
            let result = team_process::recover_result(
                &boot.storage_root,
                &boot.scope,
                boot.task_id.as_deref().ok_or(Error::Protocol)?,
            )?;
            return match result {
                Some(result) => bridge.emit(result_value(result)),
                None => {
                    let cursor = team_process::inspect_scope(&boot.storage_root, &boot.scope)?;
                    bridge.emit(
                        json!({"version":3,"type":"result","status":"not_found","cursor":cursor}),
                    )
                }
            };
        }
        if boot.kind == "inspect_task" {
            let model = boot.model.as_ref().ok_or(Error::Protocol)?;
            let checkpoint = team_process::inspect_task(
                &boot.storage_root,
                &boot.scope,
                boot.task_id.as_deref().ok_or(Error::Protocol)?,
                model.model_protocol,
            )?;
            let mut value = serde_json::to_value(&checkpoint).map_err(|_| Error::Protocol)?;
            value["type"] = json!("result");
            value["status"] = json!("task_checkpoint");
            return bridge.emit(value);
        }
        let stop = bridge.cancel.clone();
        let watcher = tokio::spawn(async move {
            if let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
                stop.cancel();
            }
        });
        let root = boot.storage_root.clone();
        let worker = boot.worker_executable.clone();
        let continuation = boot.continuation.clone();
        let scope = boot.scope.clone();
        let protocol = boot.model.as_ref().ok_or(Error::Protocol)?.model_protocol;
        let request = boot.request()?;
        let task_id = request.task_id.clone();
        bridge.authorize(bridge.cancel.child_token()).await?;
        let start = match continuation {
            Some(continuation) => {
                TeamProcessStart::prepare_continuation(&root, request, continuation)?
            }
            None => TeamProcessStart::prepare(&root, request)?,
        };
        let result = start
            .run_with_broker(&worker, bridge.cancel.child_token(), bridge.clone())
            .await;
        watcher.abort();
        let result = result?;
        if result.outcome == TeamProcessOutcome::Waiting {
            // The worker is gone; nothing can append any more. The wait and the
            // digest come from the durable checkpoint, the one authority a
            // later inspection and the continuing worker will also read.
            let checkpoint = team_process::inspect_task(&root, &scope, &task_id, protocol)?;
            let wait = checkpoint
                .wait
                .filter(|_| checkpoint.continuable)
                .ok_or(Error::Worker)?;
            return bridge.emit(json!({"version":3,"type":"result","status":"waiting","cursor":result.cursor,
                "wait":wait,"checkpointDigest":checkpoint.checkpoint_digest,"pausedAt":result.finished_at,
                "dispatchedRequests":result.dispatched_requests,"attemptDispatchedRequests":result.attempt_dispatched_requests,
                "deniedTools":result.denied_tools,"usage":result.usage}));
        }
        let value = result_value(result);
        save_result(&root, &task_id, &value)?;
        bridge.emit(value)
    });
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}
fn result_value(result: team_process::TeamProcessResult) -> Value {
    let mut value = json!({"version":3,"type":"result","status":if result.outcome==TeamProcessOutcome::Completed{"completed"}else{"failed"},
        "text":result.text,"cursor":result.cursor,"finishedAt":result.finished_at,"dispatchedRequests":result.dispatched_requests,"attemptDispatchedRequests":result.attempt_dispatched_requests,"deniedTools":result.denied_tools,"usage":result.usage});
    if let Some(document) = result.document {
        value["document"] = json!(document);
    }
    value
}
fn save_result(root: &Path, task_id: &str, value: &Value) -> Result<(), Error> {
    if !(8..=64).contains(&task_id.len())
        || !task_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::Protocol);
    }
    let directory = root.join("results");
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    private_dir(&directory)?;
    let path = directory.join(format!("{task_id}.json"));
    let bytes = serde_json::to_vec(value).map_err(|_| Error::Protocol)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        // A continuation that returns the already recorded terminal result may
        // find the host's earlier copy; it must be byte-identical.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return if std::fs::read(&path)? == bytes {
                Ok(())
            } else {
                Err(Error::Protocol)
            };
        }
        Err(e) => return Err(e.into()),
    };
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::File::open(&directory)?.sync_all()?;
    Ok(())
}
pub fn host_main() -> i32 {
    match entry() {
        Ok(()) => 0,
        Err(error) => {
            let code = match error {
                Error::Cancelled => "CANCELLED",
                Error::Model => "MODEL_UNAVAILABLE",
                Error::Deadline => "TASK_EXPIRED",
                Error::AlreadyAttempted => "TASK_INTERRUPTED",
                Error::CheckpointMismatch => "CHECKPOINT_MISMATCH",
                Error::NotContinuable => "NOT_CONTINUABLE",
                Error::RouteUnsupported => "ROUTE_UNSUPPORTED",
                Error::Environment => "ENVIRONMENT",
                Error::Worker => "WORKER",
                Error::Capacity => "CHAT_CONTEXT_CAPACITY",
                Error::ResourceLimit => "CHAT_RESOURCE_LIMIT",
                Error::Profile(_) => "PROFILE",
                Error::Executable => "EXECUTABLE",
                Error::Io(_) => "STORAGE",
                Error::Protocol => "PROTOCOL",
            };
            let _ = writeln!(
                std::io::stdout(),
                "{}",
                json!({"version":3,"type":"error","code":code})
            );
            70
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retired_protocol_is_not_decoded() {
        assert!(serde_json::from_value::<Start>(
            json!({"version":1,"type":"start","mode":"run","target":{}})
        )
        .is_err());
    }
    #[test]
    fn an_operation_result_says_whether_its_outcome_is_established() {
        let refused: Response = serde_json::from_value(json!({"version":3,"type":"operation_result","id":"operation-1",
            "established":true,"succeeded":false,"result":{"error":"DATA_REVISION_CONFLICT","dataRevision":4}}))
        .unwrap();
        assert_eq!(refused.correlation(), (3, "operation-1", "operation"));
        for incomplete in [
            json!({"version":3,"type":"operation_result","id":"operation-1","succeeded":true,"result":{}}),
            json!({"version":3,"type":"operation_result","id":"operation-1","established":true,"result":{}}),
            json!({"version":3,"type":"operation_result","id":"operation-1","established":true,"succeeded":true,"result":{},"retry":true}),
        ] {
            assert!(serde_json::from_value::<Response>(incomplete).is_err());
        }
    }
    #[test]
    fn stale_response_versions_are_rejected() {
        let response: Response =
            serde_json::from_value(json!({"version":1,"type":"authorized","id":"authorize-1"}))
                .unwrap();
        let bridge = Bridge {
            root: PathBuf::new(),
            cancel: CancellationToken::new(),
            counter: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            writer: Mutex::new(std::io::stdout()),
        };
        assert!(bridge.accept(response).is_err());
    }
}
