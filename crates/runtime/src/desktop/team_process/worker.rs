use super::interaction::ChildInteraction;
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
        branding::CONFIG_DIR_ENV,
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
    if bootstrap.version != 3 {
        return Err(TeamProcessError::Protocol);
    }
    let model = bootstrap.model()?;
    let limits = bootstrap.limits()?;
    let batch: crate::desktop::session_state::team_chat::ChatInputBatch =
        bootstrap.input_batch_ref.read(&bootstrap.storage_root)?;
    let mut profile = TeamInvocationProfile::prepare(
        &bootstrap.storage_root,
        bootstrap.scope,
        bootstrap.invocation_id,
        bootstrap.instructions,
        limits,
    )?
    .with_selected_model(model.clone());
    batch
        .validate(profile.session_id())
        .map_err(|_| TeamProcessError::Protocol)?;
    let knowledge: Vec<super::TeamKnowledgeDocument> =
        bootstrap.knowledge_ref.read(&bootstrap.storage_root)?;
    super::read_tools::validate_knowledge(&knowledge)?;
    for (key, value) in environment(profile.scope_root()) {
        if std::env::var_os(key).as_ref() != Some(&value) {
            return Err(TeamProcessError::Environment);
        }
    }
    if std::env::current_dir()? != profile.scope_root().join("workspace") {
        return Err(TeamProcessError::Environment);
    }
    let cancel = CancellationToken::new();
    let bridge = ChildInteraction::start(
        cancel.clone(),
        bootstrap.storage_root.clone(),
        batch.to_seq_inclusive,
    )?;
    let authorize = bridge.clone();
    profile
        .output_quota()
        .set_dispatch_authorization(Arc::new(move || {
            let bridge = authorize.clone();
            Box::pin(async move {
                bridge
                    .authorize()
                    .await
                    .map_err(|_| llm_runtime::LlmError::InvalidRequest {
                        message: "Shared Bot execution authorization changed".into(),
                    })
            })
        }))
        .map_err(|_| TeamProcessError::Protocol)?;
    profile = profile.with_read_tools(Arc::new(super::read_tools::TeamReadTools {
        bridge: bridge.clone(),
        knowledge,
    }));
    let mut cfg = DesktopConfig::default();
    cfg.default_model = format!("{}/{}", model.route().model_provider, model.model_id);
    profile.apply_to(&mut cfg)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(crate::RUNTIME_THREAD_STACK_SIZE * 2)
        .enable_all()
        .build()?;
    // The process main stack is fixed by the OS. Poll the deep desktop/model
    // future on an explicitly sized runtime thread; block_on only awaits its
    // small join/deadline future. The parent still owns the hard process lease.
    let task_profile = profile.clone();
    let task_cancel = cancel.child_token();
    let result = runtime.block_on(async {
        let mut task=tokio::spawn(async move {run(cfg,&task_profile,batch,bootstrap.task_id,limits.physical_dispatches.get(),task_cancel,bridge,bootstrap.credentials).await});
        tokio::select! {
            biased;
            _=tokio::time::sleep_until(deadline.instant.into())=>{task.abort();Err(TeamProcessError::Deadline)},
            _=cancel.cancelled()=>{task.abort();Err(TeamProcessError::Cancelled)},
            result=&mut task=>result.map_err(|_|TeamProcessError::Worker)?,
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
    let frame = protocol::encode(&protocol::WorkerFrame::Result { result })?;
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
    mut cfg: DesktopConfig,
    profile: &TeamInvocationProfile,
    batch: crate::desktop::session_state::team_chat::ChatInputBatch,
    task_id: String,
    dispatch_limit: u32,
    cancel: CancellationToken,
    interaction: Arc<ChildInteraction>,
    envelope: super::TeamAccountEnvelope,
) -> Result<TeamProcessResult, TeamProcessError> {
    interaction.authorize().await?;
    let path = session::jsonl::session_path(
        &cfg.lingxi_home,
        &cfg.cwd.to_string_lossy(),
        &profile.session_id().as_uuid().to_string(),
    );
    let start = if path.exists() {
        crate::headless::session::SessionStart::Resume {
            session_id: profile.session_id().as_uuid(),
            resume_session_at: None,
            resume_drops_turn: None,
            fork: false,
        }
    } else {
        crate::headless::session::SessionStart::New
    };
    let prepared = crate::headless::session::prepare(&mut cfg, &start, false)
        .await
        .map_err(|_| TeamProcessError::Worker)?;
    let mounted_cfg = cfg.clone();
    let output = Arc::new(BoundedOutput {
        received_bytes: AtomicUsize::new(0),
        failed: AtomicBool::new(false),
        overflow: AtomicBool::new(false),
        compaction_failed: AtomicBool::new(false),
        request_retried: AtomicBool::new(false),
        cancel: cancel.clone(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PermissionRequest>(8);
    let rt = {
        let interaction = interaction.clone();
        let mut shared = crate::desktop::build_shared_credential_stack_for_config(&cfg)
            .await
            .map_err(|_| TeamProcessError::Worker)?;
        envelope.seed(&shared.credentials).await?;
        shared.provider_accounts = Some(envelope.provider_accounts.clone());
        shared.provider_account_home = Some(envelope.provider_account_home.clone());
        shared
            .credentials
            .set_account_credential_refresher(Arc::new(super::accounts::WorkerAccountRefresher {
                credentials: Arc::downgrade(&shared.credentials),
                bridge: interaction,
                registry: shared.catalog_registry.clone(),
                home: envelope.provider_account_home,
                scope: envelope.provider_accounts.store_scope,
                profiles: envelope.provider_account_profiles.clone(),
                locks: Default::default(),
            }));
        crate::desktop::build_with_credential_stack(
            cfg,
            output.clone(),
            Arc::new(PermissionSink(tx)),
            shared,
        )
        .await
        .map_err(|_| TeamProcessError::Worker)?
    };
    if crate::headless::session::mount_prepared(&rt, &mounted_cfg, prepared)
        .await
        .is_err()
    {
        let _ = rt.shutdown().await;
        return Err(TeamProcessError::Worker);
    }
    let gate = rt
        .permission_gate
        .as_ref()
        .ok_or(TeamProcessError::Worker)?
        .clone();
    let denied = Arc::new(AtomicU32::new(0));
    let count = denied.clone();
    let pump = AbortOnDrop(tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            count.fetch_add(1, Ordering::Relaxed);
            let _ = gate
                .resolve(request.request_id, PermissionResponseDto::Deny, "")
                .await;
        }
    }));
    if let Err(error) = super::chat_session::repair_projection(&rt).await {
        let _ = rt.shutdown().await;
        return Err(error);
    }
    let usage_before = rt.cost().await;
    let current_message_id = batch.current_message_id.clone();
    if let Err(error) = super::chat_session::admit(&rt, profile, batch, cancel.clone()).await {
        let _ = rt.shutdown().await;
        return Err(error);
    }
    let input=format!("Respond to shared Chat message {current_message_id}. The original messages above include complete author identity and original timestamps. Read older Chat history before asking anyone to repeat missing earlier context. Use only published knowledge and Chat history tools. Produce a useful Markdown answer; this result will be saved as the task's document.");
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
    // reply uses the final accepted assistant of this task in the restored session.
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
    let cursor = rt.session_state.bot_chat_cursor();
    let finished_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| TeamProcessError::Deadline)?
        .as_millis() as u64;
    if cancel.is_cancelled() && !output.overflow.load(Ordering::Acquire) {
        let _ = rt.shutdown().await;
        return Err(TeamProcessError::Cancelled);
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
    let usage_after = rt.cost().await;
    let dispatched_requests = dispatch_limit - profile.output_quota().remaining_dispatches();
    let settled = output.usage_settled(
        outcome,
        dispatched_requests,
        usage_after.api_calls.saturating_sub(usage_before.api_calls),
    );
    let result = TeamProcessResult {
        version: 3,
        outcome,
        document: if outcome == TeamProcessOutcome::Completed {
            Some(super::chat_session::document(&task_id, &text))
        } else {
            None
        },
        text: if text.len() <= 8192 {
            text
        } else {
            "Result saved as a shared document.".into()
        },
        finished_at,
        cursor,
        usage: super::TeamUsage::from_snapshots(&usage_before, &usage_after, settled),
        dispatched_requests,
        denied_tools: denied.load(Ordering::Relaxed),
    };
    let saved = super::chat_session::record_result(&rt, &task_id, &result).await;
    let drain = rt.shutdown().await;
    drop(pump);
    saved?;
    if !drain.complete {
        return Err(TeamProcessError::Worker);
    }
    Ok(result)
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
    compaction_failed: AtomicBool,
    request_retried: AtomicBool,
    cancel: CancellationToken,
}
impl BoundedOutput {
    /// Whether the task's token accounting can be called complete. Every
    /// physical provider dispatch (retries, fallbacks, failovers and the
    /// compaction summary all spend the shared dispatch quota, never refunded)
    /// must have produced exactly one recorded cost response, and the task must
    /// have ended normally. A failed or aborted request, a failed compaction
    /// summary, or a retried/fallen-back request may have consumed tokens that
    /// never reached the cost tracker, so those stay Partial.
    fn usage_settled(
        &self,
        outcome: TeamProcessOutcome,
        dispatched_requests: u32,
        recorded_responses: u32,
    ) -> bool {
        matches!(
            outcome,
            TeamProcessOutcome::Completed | TeamProcessOutcome::MaxTurns
        ) && dispatched_requests == recorded_responses
            && !self.compaction_failed.load(Ordering::Acquire)
            && !self.request_retried.load(Ordering::Acquire)
    }
}
#[async_trait::async_trait]
impl OutputStream for BoundedOutput {
    fn wants_forwarded_subagent_messages(&self) -> bool {
        false
    }

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
    // Retry backoff, model fallback and abandoned fallback rows all mean an
    // earlier request of this task was re-dispatched; its own usage may be
    // missing or cut short.
    async fn emit_api_retry(&self, _: &str, _: u32, _: u32, _: u64) {
        self.request_retried.store(true, Ordering::Release);
    }
    async fn emit_model_fallback(
        &self,
        _: &lingxi_core::types::MessageId,
        _: &lingxi_core::types::SessionId,
        _: &Json,
        _: &lingxi_core::types::ModelFallbackMetadata,
    ) {
        self.request_retried.store(true, Ordering::Release);
    }
    async fn emit_server_fallback_tombstone(
        &self,
        _: &lingxi_core::host::ServerFallbackTombstoneMessage,
        _: bool,
    ) {
        self.request_retried.store(true, Ordering::Release);
    }
    async fn emit_compaction_finished(&self, error: Option<&str>) {
        if error.is_some() {
            self.compaction_failed.store(true, Ordering::Release);
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

#[cfg(all(test, not(target_os = "macos")))]
mod unsupported_platform_tests {
    #[test]
    fn worker_rejects_unsupported_platform_before_reading_bootstrap() {
        assert!(matches!(
            super::main(),
            Err(super::TeamProcessError::Environment)
        ));
    }
}

#[cfg(test)]
mod usage_settlement_tests {
    use super::*;

    fn output() -> BoundedOutput {
        BoundedOutput {
            received_bytes: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            overflow: AtomicBool::new(false),
            compaction_failed: AtomicBool::new(false),
            request_retried: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn only_a_clean_fully_recorded_normal_end_is_settled() {
        let clean = output();
        assert!(clean.usage_settled(TeamProcessOutcome::Completed, 2, 2));
        assert!(clean.usage_settled(TeamProcessOutcome::MaxTurns, 1, 1));
        assert!(!clean.usage_settled(TeamProcessOutcome::ModelFailed, 1, 1));
        assert!(!clean.usage_settled(TeamProcessOutcome::OutputLimit, 1, 1));
        // A dispatch whose response never reached the cost tracker.
        assert!(!clean.usage_settled(TeamProcessOutcome::Completed, 2, 1));
    }

    #[tokio::test]
    async fn retry_fallback_and_failed_compaction_unsettle_usage() {
        let retried = output();
        retried.emit_api_retry("overloaded", 1, 10, 500).await;
        assert!(!retried.usage_settled(TeamProcessOutcome::Completed, 1, 1));

        let failed_compaction = output();
        failed_compaction
            .emit_compaction_finished(Some("summary failed"))
            .await;
        assert!(!failed_compaction.usage_settled(TeamProcessOutcome::Completed, 1, 1));

        let succeeded_compaction = output();
        succeeded_compaction.emit_compaction_finished(None).await;
        assert!(succeeded_compaction.usage_settled(TeamProcessOutcome::Completed, 1, 1));
    }
}
