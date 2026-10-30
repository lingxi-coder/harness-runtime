use super::interaction::ChildInteraction;
use super::protocol::{self, Bootstrap, MAX_FRAME, MAX_RESULT_TEXT};
use super::{
    environment, TeamProcessDeadline, TeamProcessError, TeamProcessOutcome, TeamProcessResult,
};
use crate::api::{LifecycleService, RunInput, SessionService};
use crate::desktop::team_invocation::TeamInvocationProfile;
use crate::desktop::DesktopConfig;
use client::adapter::PermissionRequestSink;
use client::protocol::permission::{PermissionKindDto, PermissionRequest, PermissionResponseDto};
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
    if !matches!(bootstrap.version, 1 | 2) {
        return Err(TeamProcessError::Protocol);
    }
    let mut profile = TeamInvocationProfile::prepare(
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
    let cancel = CancellationToken::new();
    if bootstrap.version == 2 {
        // Only this fresh, validated broker-enabled worker can route private
        // Bash through a crash-safe per-command helper. Its lease closes even
        // after abrupt worker death; the helper receives no model credential.
        platform_posix::process::team_guardian::enable_team_guardian(
            std::env::current_exe()?,
            profile.scope_root().to_owned(),
            bootstrap.deadline_unix_ms,
            cancel.clone(),
        )
        .map_err(|_| TeamProcessError::Worker)?;
    }
    let (interaction, questions) = if bootstrap.version == 2 {
        let bridge = ChildInteraction::start(cancel.clone())?;
        let (sender, questions) = tokio::sync::mpsc::channel(4);
        profile = profile.with_original_human_question_sender(sender);
        (Some(bridge), Some(questions))
    } else {
        let parent_gone = cancel.clone();
        // Version one preserves the bootstrap/result-only explicit-deny route.
        std::thread::Builder::new()
            .name("team-parent-lease".into())
            .spawn(move || {
                let _ = std::io::stdin().read(&mut [0u8; 1]);
                parent_gone.cancel();
            })?;
        (None, None)
    };
    let mut cfg = DesktopConfig::default();
    cfg.default_model = format!("anthropic/{}", model.model_id);
    cfg.api_base = model.endpoint;
    cfg.api_key = model.api_key;
    profile.apply_to(&mut cfg)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline.instant.into()) => Err(TeamProcessError::Deadline),
            _ = cancel.cancelled() => Err(TeamProcessError::Cancelled),
            result = run(cfg, &profile, bootstrap.input, limits.physical_dispatches.get(), cancel.child_token(), interaction, questions) => result,
        }
    });
    // Runtime tasks (including an in-flight model request) cannot outlive this
    // worker. Parent independently kills the process on its monotonic deadline.
    profile.revoke();
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    let mut result = result?;
    if cancel.is_cancelled() {
        return Err(TeamProcessError::Cancelled);
    }
    result.version = bootstrap.version;
    let frame = if bootstrap.version == 2 {
        protocol::encode(&protocol::WorkerFrame::Result { result })?
    } else {
        protocol::encode(&result)?
    };
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

struct PermissionSink(tokio::sync::mpsc::Sender<PermissionRequest>);
#[async_trait::async_trait]
impl PermissionRequestSink for PermissionSink {
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
    interaction: Option<Arc<ChildInteraction>>,
    questions: Option<
        tokio::sync::mpsc::Receiver<tool_api::ask_user_question::AskUserQuestionExchange>,
    >,
) -> Result<TeamProcessResult, TeamProcessError> {
    let output = Arc::new(BoundedOutput {
        received_bytes: AtomicUsize::new(0),
        failed: AtomicBool::new(false),
        overflow: AtomicBool::new(false),
        cancel: cancel.clone(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PermissionRequest>(8);
    let rt = crate::desktop::build(cfg, output.clone(), Arc::new(PermissionSink(tx)))
        .await
        .map_err(|_| TeamProcessError::Worker)?;
    let gate = rt
        .permission_gate
        .as_ref()
        .ok_or(TeamProcessError::Worker)?
        .clone();
    let denied = Arc::new(AtomicU32::new(0));
    let count = denied.clone();
    let permission_bridge = interaction.clone();
    let permission_cancel = cancel.clone();
    let pump = AbortOnDrop(tokio::spawn(async move {
        while let Some(request) = tokio::select! {
            biased;
            _ = permission_cancel.cancelled() => None,
            request = rx.recv() => request,
        } {
            let approved = if let Some(bridge) = &permission_bridge {
                match request.kind {
                    PermissionKindDto::ToolUseConfirm {
                        tool_name,
                        tool_input_json,
                        ..
                    } if tool_name == "Bash" => match serde_json::from_str(&tool_input_json) {
                        Ok(input) => bridge.approve(tool_name, input).await,
                        Err(_) => Err(TeamProcessError::Protocol),
                    },
                    _ => Ok(false),
                }
            } else {
                Ok(false)
            };
            let allow = match approved {
                Ok(allow) if !permission_cancel.is_cancelled() => allow,
                _ => {
                    permission_cancel.cancel();
                    break;
                }
            };
            let response = if allow {
                PermissionResponseDto::AllowOnce
            } else {
                PermissionResponseDto::Deny
            };
            if !gate.resolve(request.request_id, response, "").await {
                permission_cancel.cancel();
                break;
            }
            if !allow {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }));
    let question_pump = match (interaction, questions) {
        (Some(bridge), Some(mut questions)) => Some(AbortOnDrop(tokio::spawn(async move {
            while let Some(question) = tokio::select! {
                biased;
                _ = bridge.cancel.cancelled() => None,
                question = questions.recv() => question,
            } {
                if bridge.ask(question).await.is_err() {
                    bridge.cancel.cancel();
                    break;
                }
            }
        }))),
        _ => None,
    };
    let outcome = rt
        .run(
            RunInput {
                prompt: input,
                images: Vec::new(),
            },
            cancel.clone(),
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
    drop(pump);
    drop(question_pump);
    if cancel.is_cancelled() && !output.overflow.load(Ordering::Acquire) {
        return Err(TeamProcessError::Cancelled);
    }
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

// Dropping a run future also drops its callbacks; no detached broker wait
// survives runtime cancellation or a terminal turn.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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
