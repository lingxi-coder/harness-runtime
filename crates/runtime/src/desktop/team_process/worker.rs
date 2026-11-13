use super::interaction::ChildInteraction;
use super::protocol::{self, Bootstrap, MAX_FRAME, MAX_RESULT_TEXT};
use super::{
    environment, TeamProcessDeadline, TeamProcessError, TeamProcessOutcome, TeamProcessResult,
};
use crate::api::{LifecycleService, SessionService};
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
    if let Some(continuation) = &bootstrap.continuation {
        continuation.validate()?;
    }
    let ledger = Arc::new(super::chat_session::DispatchLedger::new(
        bootstrap.task_id.clone(),
        bootstrap.continuation.as_ref().map_or(1, |c| c.epoch),
        profile.session_id(),
    ));
    let receipts = ledger.clone();
    profile
        .output_quota()
        .set_dispatch_authorization(Arc::new(move || {
            let bridge = authorize.clone();
            let receipts = receipts.clone();
            Box::pin(async move {
                bridge
                    .authorize()
                    .await
                    .map_err(|_| llm_runtime::LlmError::InvalidRequest {
                        message: "Shared Bot execution authorization changed".into(),
                    })?;
                // The receipt is durable before the request may leave the
                // process, so a lost response can never be uncounted.
                receipts
                    .record()
                    .await
                    .map_err(|_| llm_runtime::LlmError::InvalidRequest {
                        message: "Shared Bot dispatch receipt could not be recorded".into(),
                    })
            })
        }))
        .map_err(|_| TeamProcessError::Protocol)?;
    let ask = bootstrap
        .ask_requester
        .then(|| Arc::new(super::ask_tool::AskRequester::new()));
    let list = match bootstrap.shared_list.clone() {
        Some(list) => {
            list.validate()?;
            if list.approval == super::TeamListApproval::Requester && ask.is_none() {
                return Err(TeamProcessError::Protocol);
            }
            Some(Arc::new(super::list_tools::TeamListTools {
                bridge: bridge.clone(),
                list,
                task_id: bootstrap.task_id.clone(),
                ask: ask.clone(),
                source: Default::default(),
            }))
        }
        None => None,
    };
    profile = profile.with_read_tools(Arc::new(super::read_tools::TeamReadTools {
        bridge: bridge.clone(),
        knowledge,
        ask: ask.clone(),
        list: list.clone(),
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
        let mut task=tokio::spawn(async move {run(cfg,&task_profile,batch,bootstrap.task_id,limits.physical_dispatches.get(),task_cancel,bridge,bootstrap.credentials,bootstrap.continuation,ledger,bootstrap.model_protocol,ask,list).await});
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
    continuation: Option<super::TeamContinuation>,
    ledger: Arc<super::chat_session::DispatchLedger>,
    protocol: lingxi_llm_client::protocol::ProtocolFamily,
    ask: Option<Arc<super::ask_tool::AskRequester>>,
    list: Option<Arc<super::list_tools::TeamListTools>>,
) -> Result<TeamProcessResult, TeamProcessError> {
    if continuation.is_some() && !super::checkpoint::route_supports_continuation(protocol) {
        return Err(TeamProcessError::RouteUnsupported);
    }
    interaction.authorize().await?;
    let path = session::jsonl::session_path(
        &cfg.lingxi_home,
        &cfg.cwd.to_string_lossy(),
        &profile.session_id().as_uuid().to_string(),
    );
    if continuation.is_some() && !path.exists() {
        return Err(TeamProcessError::NotContinuable);
    }
    if let Some(list) = &list {
        // A write is dispatched only once its call is on this file.
        let _ = list.source.set(super::list_tools::StepSource {
            transcript: path.clone(),
            scope: batch.scope(),
            batch_id: batch.batch_id.clone(),
        });
    }
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
    // Verified under the lease, from the captured pre-mount transcript and the
    // hydrated journal, before any repair, admission or provider dispatch.
    let resumed = match &continuation {
        Some(continuation) => {
            let captured = mounted_cfg
                .session_resume_snapshot
                .as_ref()
                .map(|snapshot| snapshot.content.clone());
            match verify_continuation(
                &rt,
                &batch,
                &task_id,
                protocol,
                continuation,
                captured.as_deref().unwrap_or_default(),
                ask.is_some(),
            ) {
                Ok(state) => Some(state),
                Err(error) => {
                    let _ = rt.shutdown().await;
                    return Err(error);
                }
            }
        }
        None => None,
    };
    if let Err(error) = ledger.bind(&rt) {
        let _ = rt.shutdown().await;
        return Err(error);
    }
    if let Err(error) = super::chat_session::repair_projection(&rt).await {
        let _ = rt.shutdown().await;
        return Err(error);
    }
    let usage_before = rt.cost().await;
    let current_message_id = batch.current_message_id.clone();
    let (scope, batch_id) = (batch.scope(), batch.batch_id.clone());
    if resumed.is_none() {
        if let Err(error) = super::chat_session::admit(&rt, profile, batch, cancel.clone()).await {
            let _ = rt.shutdown().await;
            return Err(error);
        }
    }
    let prior_dispatches = resumed
        .as_ref()
        .map_or(0, |state| state.checkpoint.dispatches_observed);
    let input=format!("Respond to shared Chat message {current_message_id}. The original messages above include complete author identity and original timestamps. Read older Chat history before asking anyone to repeat missing earlier context. Use only published knowledge and Chat history tools. Produce a useful Markdown answer; this result will be saved as the task's document.");
    let finish_only = resumed.as_ref().is_some_and(|state| state.final_answer.is_some());
    let turn = async {
        match &resumed {
            // The newest durable response already is the final answer: only its
            // terminal result row is missing. No model request may repeat it.
            Some(_) if finish_only => Ok(TurnOutcome::EndTurn),
            Some(state) => {
                let answer = continuation.as_ref().and_then(|c| c.answer.as_ref());
                run_continuation(&rt, state, ask.as_deref(), answer, &input, &task_id, cancel.clone())
                    .await
            }
            None => run_prompt(&rt, &input, &task_id, cancel.clone()).await,
        }
    };
    let outcome = match &ask {
        None => turn.await,
        Some(ask) => {
            tokio::pin!(turn);
            // A question pauses the task. The loop keeps driving the turn until
            // the response that holds the ask call is on the transcript file,
            // then ends this process exactly as a kill would: the call stays
            // unfinished, nothing is drained and no result row is written.
            let (mut asked, mut seen) = (false, false);
            loop {
                tokio::select! {
                    biased;
                    outcome = &mut turn => break outcome,
                    () = ask.asked(), if !asked => asked = true,
                    () = tokio::time::sleep(std::time::Duration::from_millis(50)), if asked => {
                        let transcript = std::fs::read_to_string(&path).unwrap_or_default();
                        let cursor = rt.session_state.bot_chat_cursor();
                        let waiting = super::checkpoint::evaluate(&super::checkpoint::Inputs {
                            scope: &scope,
                            task_id: &task_id,
                            batch_id: Some(&batch_id),
                            batch_admitted: true,
                            cursor: &cursor,
                            transcript: &transcript,
                            attempts: 0,
                            protocol,
                        })
                        .is_ok_and(|state| state.checkpoint.wait.is_some());
                        // Two sightings 50 ms apart let the response's own
                        // bookkeeping (cost journal) settle first.
                        if waiting && seen {
                            let usage_after = rt.cost().await;
                            let attempt = dispatch_limit - profile.output_quota().remaining_dispatches();
                            pause(TeamProcessResult {
                                version: 3,
                                outcome: TeamProcessOutcome::Waiting,
                                text: String::new(),
                                document: None,
                                finished_at: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map_err(|_| TeamProcessError::Deadline)?
                                    .as_millis() as u64,
                                cursor,
                                usage: super::TeamUsage::from_snapshots(&usage_before, &usage_after, false),
                                dispatched_requests: prior_dispatches.saturating_add(attempt),
                                attempt_dispatched_requests: attempt,
                                denied_tools: denied.load(Ordering::Relaxed),
                            }, &path);
                        }
                        seen = waiting;
                    }
                }
            }
        }
    };
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
        || (!finish_only && metrics.as_ref().is_none_or(|m| {
            m.api_error_status.is_some()
                || m.api_error_terminal_reason.is_some()
                || m.api_error_stop_reason.is_some()
                || (matches!(&outcome, Ok(TurnOutcome::EndTurn))
                    && !matches!(
                        m.model_stop_reason.as_deref(),
                        Some("end_turn" | "stop_sequence")
                    ))
        }))
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
    let attempt_dispatched_requests = dispatch_limit - profile.output_quota().remaining_dispatches();
    let dispatched_requests = prior_dispatches.saturating_add(attempt_dispatched_requests);
    // Usage is this process's cost-ledger delta. A continued task also spent
    // requests in an earlier process whose usage is not attributable here, so
    // it can never claim complete accounting.
    let settled = resumed.is_none()
        && output.usage_settled(
            outcome,
            attempt_dispatched_requests,
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
        attempt_dispatched_requests,
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

/// End this process on a durable question. The frame is the last thing the
/// parent reads; exiting here (no drain, no destructors) leaves the transcript
/// exactly as the checkpoint will be inspected.
fn pause(result: TeamProcessResult, transcript: &std::path::Path) -> ! {
    let _ = std::fs::File::open(transcript).and_then(|file| file.sync_all());
    let sent = protocol::encode(&protocol::WorkerFrame::Result { result })
        .ok()
        .and_then(|frame| {
            let mut out = std::io::stdout();
            out.write_all(&frame).and_then(|()| out.flush()).ok()
        });
    std::process::exit(if sent.is_some() { 0 } else { 70 })
}

/// The first epoch: admit the prompt with an identity derived from the task, so
/// a later continuation can recognise it among the durable rows.
async fn run_prompt(
    rt: &crate::desktop::DesktopRuntime,
    input: &str,
    task_id: &str,
    cancel: CancellationToken,
) -> Result<TurnOutcome, orchestrator::OrchestratorError> {
    let prompt = Json::from(input);
    let id = lingxi_core::types::MessageId::from_uuid(super::checkpoint::prompt_uuid(task_id));
    let outcome = orchestrator::native_user_message_source::with_sdk_source(
        rt.orchestrator
            .run_turn_streaming_with_cancel_images_and_message_id(prompt, &[], cancel, Some(id)),
    )
    .await?;
    Ok(host_outcome(outcome))
}

fn host_outcome(outcome: orchestrator::TurnOutcome) -> TurnOutcome {
    match outcome {
        orchestrator::TurnOutcome::EndTurn => TurnOutcome::EndTurn,
        orchestrator::TurnOutcome::MaxTurns => TurnOutcome::MaxTurns,
        orchestrator::TurnOutcome::Cancelled => TurnOutcome::Cancelled,
    }
}

/// Recompute the checkpoint from the captured pre-mount transcript and the
/// hydrated journal and refuse before any dispatch unless it still is the one
/// the host inspected and the task is continuable on this route.
fn verify_continuation(
    rt: &crate::desktop::DesktopRuntime,
    batch: &crate::desktop::session_state::team_chat::ChatInputBatch,
    task_id: &str,
    protocol: lingxi_llm_client::protocol::ProtocolFamily,
    continuation: &super::TeamContinuation,
    captured_transcript: &str,
    can_answer: bool,
) -> Result<super::checkpoint::TaskState, TeamProcessError> {
    let cursor = rt.session_state.bot_chat_cursor();
    let admitted = rt
        .session_state
        .chat_input_batches()
        .map_err(|_| TeamProcessError::Worker)?
        .iter()
        .any(|admitted| admitted.batch_id == batch.batch_id);
    let scope = batch.scope();
    let state = super::checkpoint::evaluate(&super::checkpoint::Inputs {
        scope: &scope,
        task_id,
        batch_id: Some(&batch.batch_id),
        batch_admitted: admitted,
        cursor: &cursor,
        transcript: captured_transcript,
        attempts: 0,
        protocol,
    })?;
    if state.checkpoint.checkpoint_digest != continuation.expected_checkpoint_digest {
        return Err(TeamProcessError::CheckpointMismatch);
    }
    if !super::checkpoint::route_supports_continuation(protocol) {
        return Err(TeamProcessError::RouteUnsupported);
    }
    if !state.checkpoint.continuable
        || !matches!(
            state.checkpoint.phase,
            super::TeamTaskPhase::Admitted | super::TeamTaskPhase::InFlight
        )
    {
        return Err(TeamProcessError::NotContinuable);
    }
    // A waiting task continues only with the reply to exactly that wait, of
    // the kind it asked for, in a process that offers the ask tool; any other
    // task takes no answer.
    match (&state.checkpoint.wait, &continuation.answer) {
        (Some(wait), Some(answer))
            if can_answer
                && wait.wait_id == answer.wait_id
                && (wait.kind == super::TeamWaitKind::ToolApproval)
                    == answer.approved.is_some() => {}
        (None, None) => {}
        _ => return Err(TeamProcessError::NotContinuable),
    }
    Ok(state)
}

/// Continue from the durable head. Persisted tool results are already in the
/// restored history and are never executed again; each unfinished read-only
/// Team tool runs once now under the current authorization and its result is
/// persisted before the next model request is built.
async fn run_continuation(
    rt: &crate::desktop::DesktopRuntime,
    state: &super::checkpoint::TaskState,
    ask: Option<&super::ask_tool::AskRequester>,
    answer: Option<&super::TeamWaitAnswer>,
    input: &str,
    task_id: &str,
    cancel: CancellationToken,
) -> Result<TurnOutcome, orchestrator::OrchestratorError> {
    use orchestrator::OrchestratorError;
    if matches!(state.checkpoint.phase, super::TeamTaskPhase::Admitted) && !state.prompt_persisted {
        return run_prompt(rt, input, task_id, cancel).await;
    }
    if !state.pending.is_empty() {
        let calls = state
            .pending
            .iter()
            .map(|call| {
                let row = uuid::Uuid::parse_str(&call.assistant_row_uuid)
                    .map_err(|_| OrchestratorError::Internal("invalid durable tool-call row".into()))?;
                Ok(lingxi_core::host::DeferredToolReplay {
                    tool_use_id: call.tool_use_id.clone(),
                    assistant_message_id: Some(lingxi_core::types::MessageId::from_uuid(row)),
                    tool_name: call.tool_name.clone(),
                    tool_input: Json::parse(&call.input_json).map_err(|_| {
                        OrchestratorError::Internal("invalid durable tool input".into())
                    })?,
                    permission_mode: None,
                    traceparent: None,
                })
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        if let Some(ask) = ask {
            ask.begin_replay(answer.map(|a| super::ask_tool::WaitReply {
                wait_id: a.wait_id.clone(),
                message_id: a.message_id.clone(),
                text: a.text.clone(),
                approved: a.approved,
            }));
        }
        let replayed =
            orchestrator::replay_unfinished_read_only_tool_calls(&rt.orchestrator, calls).await;
        if let Some(ask) = ask {
            ask.end_replay();
        }
        replayed?;
    }
    let outcome = rt
        .orchestrator
        .run_resumed_turn_streaming_with_cancel(cancel)
        .await?;
    Ok(host_outcome(outcome))
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
