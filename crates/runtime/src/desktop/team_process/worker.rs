use super::protocol::{self, Bootstrap, MAX_FRAME, MAX_RESULT_TEXT};
use super::{
    environment, TeamProcessDeadline, TeamProcessError, TeamProcessOutcome, TeamProcessResult,
};
use crate::api::{LifecycleService, RunInput, SessionService};
use crate::desktop::team_invocation::TeamInvocationProfile;
use crate::desktop::DesktopConfig;
use client::adapter::PermissionRequestSink;
use client::protocol::permission::{PermissionRequest, PermissionResponseDto};
use lingxi_core::host::{CostSnapshot, OutputStream, TurnOutcome};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use lingxi_core::types::{ConversationMessage, ToolUseId};
use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn main() -> Result<(), TeamProcessError> {
    if !cfg!(target_os = "macos")
        || std::env::args_os().len() != 1
        || std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
    {
        return Err(TeamProcessError::Environment);
    }
    // Check names before even parsing a bootstrap or constructing runtime
    // defaults; never "sanitize" the already-running owner's process.
    let allowed = [
        "HOME",
        "LINGXI_CONFIG_DIR",
        "TMPDIR",
        "PATH",
        "LANG",
        "LC_ALL",
        "CFFIXED_USER_HOME",
        "__CF_USER_TEXT_ENCODING",
    ];
    if std::env::vars_os().count() != allowed.len()
        || std::env::vars_os().any(|(key, _)| !allowed.iter().any(|n| key == *n))
    {
        return Err(TeamProcessError::Environment);
    }
    close_inherited_descriptors()?;
    let bootstrap = read_bootstrap()?;
    let deadline = TeamProcessDeadline::from_backend_unix_millis(bootstrap.deadline_unix_ms)?;
    let model = bootstrap.model()?;
    let limits = bootstrap.limits()?;
    if bootstrap.version != 1 {
        return Err(TeamProcessError::Protocol);
    }
    let profile = TeamInvocationProfile::prepare(
        &bootstrap.storage_root,
        bootstrap.identity(),
        bootstrap.invocation_id,
        bootstrap.instructions,
        limits,
    )?;
    profile.validate_input(&bootstrap.input)?;
    for (key, value) in environment(profile.scope_root()) {
        if std::env::var_os(key).as_ref() != Some(&value) {
            return Err(TeamProcessError::Environment);
        }
    }
    if std::env::current_dir()? != profile.scope_root().join("workspace") {
        return Err(TeamProcessError::Environment);
    }
    let mut cfg = DesktopConfig::default();
    cfg.default_model = format!("anthropic/{}", model.model_id);
    cfg.api_base = model.endpoint;
    cfg.api_key = model.api_key;
    profile.apply_to(&mut cfg)?;

    let cancel = CancellationToken::new();
    let parent_gone = cancel.clone();
    // Tokio stdin uses an uncancellable blocking task, which can deadlock
    // runtime shutdown. A dedicated detached reader observes EOF/unexpected
    // bytes without joining or keeping the runtime alive.
    std::thread::Builder::new()
        .name("team-parent-lease".into())
        .spawn(move || {
            let _ = std::io::stdin().read(&mut [0u8; 1]);
            parent_gone.cancel();
        })?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline.instant.into()) => Err(TeamProcessError::Deadline),
            _ = cancel.cancelled() => Err(TeamProcessError::Cancelled),
            result = run(cfg, &profile, bootstrap.input, limits.physical_dispatches.get(), cancel.child_token()) => result,
        }
    });
    // Runtime tasks (including an in-flight model request) cannot outlive this
    // worker. Parent independently kills the process on its monotonic deadline.
    profile.revoke();
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    let frame = protocol::encode(&result?)?;
    std::io::stdout().write_all(&frame)?;
    std::io::stdout().flush()?;
    Ok(())
}

/// Called only by the fresh binary's single-threaded entry point, before Tokio
/// or any host runtime is built. Standard process spawning alone does not close
/// a host descriptor that lacks CLOEXEC. Enumerate actual macOS /dev/fd entries
/// instead of assuming every existing descriptor is below the current rlimit.
fn close_inherited_descriptors() -> Result<(), TeamProcessError> {
    #[cfg(unix)]
    {
        let mut descriptors = Vec::new();
        for entry in std::fs::read_dir("/dev/fd")? {
            let entry = entry?;
            let fd = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
                .ok_or(TeamProcessError::Environment)?;
            if fd > 2 {
                descriptors.push(fd);
            }
        }
        // read_dir's own descriptor is closed before this loop; EBADF is fine.
        for fd in descriptors {
            match nix::unistd::close(fd) {
                Ok(()) | Err(nix::errno::Errno::EBADF) => {}
                Err(_) => return Err(TeamProcessError::Environment),
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    Err(TeamProcessError::Environment)
}

fn read_bootstrap() -> Result<Bootstrap, TeamProcessError> {
    let mut input = std::io::stdin();
    let mut prefix = [0u8; 4];
    input.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(TeamProcessError::Protocol);
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|_| TeamProcessError::Protocol)
}

struct DenySink(tokio::sync::mpsc::Sender<PermissionRequest>);
#[async_trait::async_trait]
impl PermissionRequestSink for DenySink {
    async fn emit_request(&self, request: PermissionRequest) {
        let _ = self.0.send(request).await;
    }
}

async fn run(
    cfg: DesktopConfig,
    profile: &TeamInvocationProfile,
    input: String,
    dispatch_limit: u32,
    cancel: CancellationToken,
) -> Result<TeamProcessResult, TeamProcessError> {
    let output = Arc::new(BoundedOutput {
        received_bytes: AtomicUsize::new(0),
        failed: AtomicBool::new(false),
        overflow: AtomicBool::new(false),
        cancel: cancel.clone(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PermissionRequest>(8);
    let rt = crate::desktop::build(cfg, output.clone(), Arc::new(DenySink(tx)))
        .await
        .map_err(|_| TeamProcessError::Worker)?;
    let gate = rt
        .permission_gate
        .as_ref()
        .ok_or(TeamProcessError::Worker)?
        .clone();
    let denied = Arc::new(AtomicU32::new(0));
    let count = denied.clone();
    let pump = tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            if gate
                .resolve(request.request_id, PermissionResponseDto::Deny, "")
                .await
            {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    let outcome = rt
        .run(
            RunInput {
                prompt: input,
                images: Vec::new(),
            },
            cancel,
        )
        .await;
    let metrics = rt.orchestrator.completed_turn_metrics();
    // The sink also sees rejected attempts and error presentation. A Bot
    // reply uses only the last accepted assistant in this fresh session.
    let text = rt
        .transcript()
        .await
        .into_iter()
        .rev()
        .find_map(|message| match message {
            assistant @ ConversationMessage::Assistant { .. } => Some(assistant.text_content()),
            _ => None,
        })
        .unwrap_or_default();
    let drain = rt.shutdown().await;
    pump.abort();
    if !drain.complete {
        return Err(TeamProcessError::Worker);
    }
    let (outcome, text) = if output.overflow.load(Ordering::Acquire) {
        (TeamProcessOutcome::OutputLimit, String::new())
    } else if output.failed.load(Ordering::Acquire)
        || outcome.is_err()
        || (matches!(&outcome, Ok(TurnOutcome::EndTurn)) && text.trim().is_empty())
        || metrics.as_ref().is_none_or(|m| {
            m.api_error_status.is_some()
                || m.api_error_terminal_reason.is_some()
                || m.api_error_stop_reason.is_some()
                || (matches!(&outcome, Ok(TurnOutcome::EndTurn))
                    && !matches!(
                        m.model_stop_reason.as_deref(),
                        Some("end_turn" | "stop_sequence")
                    ))
        })
    {
        (TeamProcessOutcome::ModelFailed, String::new())
    } else {
        let outcome = match outcome.unwrap() {
            TurnOutcome::EndTurn => TeamProcessOutcome::Completed,
            TurnOutcome::MaxTurns => TeamProcessOutcome::MaxTurns,
            TurnOutcome::Cancelled => return Err(TeamProcessError::Cancelled),
        };
        if text.len() > MAX_RESULT_TEXT {
            return Err(TeamProcessError::Protocol);
        }
        (outcome, text)
    };
    Ok(TeamProcessResult {
        version: 1,
        outcome,
        text,
        dispatched_requests: dispatch_limit - profile.output_quota().remaining_dispatches(),
        denied_tools: denied.load(Ordering::Relaxed),
    })
}

struct BoundedOutput {
    received_bytes: AtomicUsize,
    failed: AtomicBool,
    overflow: AtomicBool,
    cancel: CancellationToken,
}
#[async_trait::async_trait]
impl OutputStream for BoundedOutput {
    fn note_model_error_kind(&self, _kind: &'static str) {
        self.failed.store(true, Ordering::Release);
    }
    async fn emit_text(&self, text: &Json) {
        if let Some(delta) = text.string_at("") {
            let previous = self.received_bytes.fetch_add(delta.len(), Ordering::AcqRel);
            if previous.saturating_add(delta.len()) > MAX_RESULT_TEXT {
                self.overflow.store(true, Ordering::Release);
                self.cancel.cancel();
            }
        }
    }
    async fn emit_terminal_api_error(&self, _: Option<u16>, _: &str, _: &str) {
        self.failed.store(true, Ordering::Release);
    }
    async fn emit_tool_call(&self, _: &ToolUseId, _: &str, _: &Json) {}
    async fn emit_tool_result(&self, _: &ToolUseId, _: &str, _: &Json, _: &Json, _: Option<&Json>) {
    }
    async fn emit_end_turn(&self, _: &str, _: &CostSnapshot) {}
}
