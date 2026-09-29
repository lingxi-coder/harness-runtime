//! Desktop `/fusion` slash handler and completion sink.
//!
//! Registered at composition time (not in `BUILTIN_COMMAND_NAMES`). `/fusion`
//! is explicit per-run and works even when `fusion.enabled` is false.

use async_trait::async_trait;
use command_api::builtins::{fusion_request_from_slash, parse_fusion_slash};
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::{
    FusionCompletionSink, FusionExecutor, FusionPublicationReceipt, FusionResult, FusionRunId,
    FusionStatus, OrchestratorHandle,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use tasks::{TaskSpawnInput, TaskType};
use tokio::sync::Mutex;

const FUSION_ARGUMENT_HINT: &str = "[--quality|--fast] [--same-provider|--cross-provider] [--implement [--verify \"COMMAND\"]...] PROMPT | setup | clean | --retry-publication fu_RUN_ID";

/// What `/fusion setup` reports when it reaches THIS handler.
///
/// The TUI never gets here: `ChatWidget::cmd_fusion` opens the wizard in the
/// bottom pane and returns without dispatching. Every other host routes the
/// whole line to the command registry, and without this arm `setup` would be
/// taken as the PROMPT — a real multi-model deliberation, billed, on the word
/// "setup". Naming the surfaces that can actually configure it is the only
/// honest answer a handler with no UI of its own can give.
const FUSION_SETUP_ELSEWHERE: &str = "`/fusion setup` opens an interactive wizard, which this surface cannot show.\n     In the terminal UI run `/fusion setup`; on the desktop open Settings → Fusion 多模型合议.\n     Either way it writes fusion.panelModels / fusion.analystModel to settings.json, which you can also edit by hand.";
const FUSION_PERSISTENCE_REQUIRED: &str =
    "durable session storage is disabled; /fusion requires session persistence (remove --no-session-persistence)";

/// Idempotent parent-history sink for Fusion results.
pub struct DesktopFusionCompletionSink {
    handle: Arc<dyn OrchestratorHandle>,
    published: Mutex<HashSet<(String, String)>>,
    durable_storage_enabled: bool,
}

impl DesktopFusionCompletionSink {
    /// Bind to the live orchestrator.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self {
            handle,
            published: Mutex::new(HashSet::new()),
            durable_storage_enabled: true,
        }
    }

    /// Declare whether the composition root wired durable session storage.
    /// The default remains enabled for existing hosts and test doubles.
    #[must_use]
    pub fn with_durable_storage_enabled(mut self, enabled: bool) -> Self {
        self.durable_storage_enabled = enabled;
        self
    }
}

#[async_trait]
impl FusionCompletionSink for DesktopFusionCompletionSink {
    async fn publish(
        &self,
        conversation_id: &str,
        result: &FusionResult,
    ) -> FusionPublicationReceipt {
        if !self.durable_storage_enabled {
            return FusionPublicationReceipt::storage_failure(FUSION_PERSISTENCE_REQUIRED);
        }
        let key = (conversation_id.to_string(), result.run_id.clone());
        let mut seen = self.published.lock().await;
        if seen.contains(&key) {
            return FusionPublicationReceipt::published();
        }
        let xml = tasks::fusion_result_xml(result);
        if let Err(err) = self
            .handle
            .append_meta_user_message_to_session(conversation_id, &xml)
            .await
        {
            tracing::warn!(error = %err, "fusion completion sink failed; task status unchanged");
            // [Finding 13] Before this, the append failure left the run with
            // NO trace at all: `finalize_fusion_outcome` still calls
            // `mark_fusion_result_published` unconditionally after
            // `publish()` returns, so the durable `<fusion-result>` row is
            // missing here AND the one live signal (this F006 notice) was
            // skipped too — the user got nothing until the next turn's task
            // notification. Fire the notice anyway, worded so it does not
            // point at a durable record that does not exist.
            self.handle
                .emit_background_system_notice(&fusion_completion_notice_unrecorded())
                .await;
            return FusionPublicationReceipt::storage_failure(err.to_string());
        }
        // F006: before this, a finished background run's only trace was the
        // meta-message row above — nothing live ever told the user it had
        // finished. Fire a best-effort UI notice (whichever session/client is
        // currently connected); the durable record above is the source of
        // truth regardless of whether this reaches anyone live.
        //
        // [Round-5 finding 13] `emit_background_system_notice` has NO session
        // argument — it routes to whatever session is connected RIGHT NOW
        // (`handle_impl.rs`: `self.output.emit_system_notice(body, false)`).
        // The append above, by contrast, is session-TARGETED and returns
        // `Ok(())` for a target that is no longer current as long as the
        // durable write landed (it only pushes into live history inside its
        // `current == target` branch). So `Ok` does NOT mean "this
        // conversation": after a `/clear` or a hot-resume mid-run the result
        // is persisted to the OLD session's JSONL while this notice appears
        // in the NEW one. Ask which session is live and pick copy that is
        // true of the session the user is actually looking at.
        let appended_to_current =
            self.handle.current_session_id().await.to_string() == conversation_id;
        let body = if appended_to_current {
            fusion_completion_notice_for_status(result.status)
        } else {
            fusion_completion_notice_other_session_for_status(result.status, conversation_id)
        };
        self.handle.emit_background_system_notice(&body).await;
        seen.insert(key);
        FusionPublicationReceipt::published()
    }
}

/// Short, content-free notice text for [`DesktopFusionCompletionSink::publish`]
/// — no prompt, no panel text, no model/provider names, just the status. The
/// material itself reaches the main model through the task notification.
pub(crate) fn fusion_completion_notice_for_status(status: FusionStatus) -> String {
    match status {
        FusionStatus::Analyzed => {
            "Fusion run finished — the panels' answers and analysis were handed to the main model, which writes the final answer.".to_string()
        }
        FusionStatus::Unanalyzed => {
            "Fusion run finished without an analysis — the panels' answers were handed to the main model, which writes the final answer.".to_string()
        }
    }
}

/// [Round-5 finding 13] Notice text for the append-SUCCEEDED-ELSEWHERE path:
/// the durable `<fusion-result>` row exists, but it is in the session the run
/// was started in, not the one this notice is about to be rendered into.
/// Saying "appended to this conversation" there points the user at a
/// conversation that contains no fusion result anywhere and gives them no
/// pointer to where the (already paid-for) result actually went.
///
/// [Rework r1] The id is truncated to 8 characters OF ITS UUID BODY, not of
/// the string as it arrives. Every production caller feeds this
/// `SessionId::to_string()` (`fusion_command.rs`'s own handler and
/// `tasks/src/registry.rs`'s `LocalFusionTaskState.conversation_id`, both from
/// `current_session_id().await.to_string()`), and `protocol`'s
/// `id_newtype!(SessionId, "sess")` renders that as `"sess:<uuid>"` — so
/// truncating the raw string spent 5 of the 8 characters on the constant
/// prefix and printed `"sess:111"`, three hex digits of the real id. The
/// pointer has to be matchable against `/resume`'s listing, which keys rows by
/// the JSONL file stem, i.e. the BARE uuid (`session/src/jsonl/loader.rs`:
/// `let sid = stem.as_str()`), so the prefix is stripped rather than shown.
/// Eight uuid characters is short enough not to dominate a one-line notice.
pub(crate) fn fusion_completion_notice_other_session_for_status(
    status: FusionStatus,
    conversation_id: &str,
) -> String {
    // `parse_prefixed` accepts both the prefixed display form and a bare uuid,
    // so this stays correct if a caller ever hands over an unprefixed id; a
    // string that is neither falls back to being truncated as-is.
    let body = protocol::SessionId::parse_prefixed(conversation_id).map_or_else(
        || conversation_id.to_string(),
        |id| id.as_uuid().to_string(),
    );
    let short: String = body.chars().take(8).collect();
    let what = match status {
        FusionStatus::Analyzed => "its record",
        FusionStatus::Unanalyzed => "its record (without an analysis)",
    };
    format!(
        "Fusion run finished — {what} was saved to the conversation it was started in \
(session {short}…), not this one. Resume that session, or check the task list, to see it."
    )
}

pub(crate) fn fusion_persisted_notice(
    status: &str,
    conversation_id: &str,
    current_session: bool,
) -> String {
    match (status, current_session) {
        ("analyzed", true) => fusion_completion_notice_for_status(FusionStatus::Analyzed),
        ("unanalyzed", true) => fusion_completion_notice_for_status(FusionStatus::Unanalyzed),
        ("analyzed", false) => fusion_completion_notice_other_session_for_status(
            FusionStatus::Analyzed,
            conversation_id,
        ),
        ("unanalyzed", false) => fusion_completion_notice_other_session_for_status(
            FusionStatus::Unanalyzed,
            conversation_id,
        ),
        ("error", true) => {
            "Fusion run failed — see the recorded error in this conversation.".to_string()
        }
        ("error", false) => {
            let body = protocol::SessionId::parse_prefixed(conversation_id).map_or_else(
                || conversation_id.to_string(),
                |id| id.as_uuid().to_string(),
            );
            let short: String = body.chars().take(8).collect();
            format!(
                "Fusion run failed — its error was saved to the conversation it was started in \
(session {short}…), not this one."
            )
        }
        _ => "Fusion run finished — see its durable task record.".to_string(),
    }
}

/// [Finding 13] Notice text for the append-failure path: unlike
/// [`fusion_completion_notice`] this must NOT claim the result was appended
/// to the conversation — the whole reason it fires is that the append
/// failed, so pointing at a durable record that does not exist would be
/// actively misleading.
fn fusion_completion_notice_unrecorded() -> String {
    "Fusion run finished, but its result could not be recorded in this conversation \
     — it may have been cleared or moved. Check the task list for the result."
        .to_string()
}

/// Fill-later wrapper so the task handler can be registered before the
/// orchestrator exists.
pub struct DeferredFusionCompletionSink {
    state: Mutex<DeferredFusionCompletionState>,
}

#[derive(Default)]
struct DeferredFusionCompletionState {
    inner: Option<Arc<dyn FusionCompletionSink>>,
}

impl DeferredFusionCompletionSink {
    /// Empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(DeferredFusionCompletionState::default()),
        }
    }

    /// Bind the live sink. The production composition root completes this
    /// before its dispatcher escapes to a caller.
    pub async fn bind(&self, sink: Arc<dyn FusionCompletionSink>) {
        self.state.lock().await.inner.get_or_insert(sink);
    }
}

impl Default for DeferredFusionCompletionSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FusionCompletionSink for DeferredFusionCompletionSink {
    async fn publish(
        &self,
        conversation_id: &str,
        result: &FusionResult,
    ) -> FusionPublicationReceipt {
        let inner = self.state.lock().await.inner.clone();
        if let Some(sink) = inner {
            sink.publish(conversation_id, result).await
        } else {
            // There is deliberately no in-memory replay here: returning a
            // terminal failure and later appending behind the task registry's
            // back would make its publication state untruthful. PR-05 owns the
            // durable outbox/retry path.
            FusionPublicationReceipt::outbox_failed(
                "fusion completion sink is not bound; result was not durably queued",
            )
        }
    }
}

/// Build the task-row description: `Fusion {preset} {scope}: <first line,
/// first 80 chars>` instead of the raw, unbounded prompt (G012) — the prompt
/// otherwise duplicates verbatim into `TaskCreated` hook payloads, `list()`,
/// notifications, and the Electron detail pane / `RuntimeCenter` row title.
fn fusion_task_description(preset: &str, scope: &str, prompt: &str) -> String {
    const MAX_CHARS: usize = 80;
    let first_line = prompt.lines().next().unwrap_or("");
    let mut truncated: String = first_line.chars().take(MAX_CHARS).collect();
    if first_line.chars().count() > MAX_CHARS {
        truncated.push('…');
    }
    format!("Fusion {preset} {scope}: {truncated}")
}

/// Desktop `/fusion` command.
pub struct DesktopFusionCommandHandler {
    registry: Arc<tasks::registry::TaskRegistry>,
    executor: Arc<dyn FusionExecutor>,
    handle: Arc<dyn OrchestratorHandle>,
    parent_profiles: BTreeMap<String, String>,
    durable_publication_available: bool,
    publication_retrier: Option<Arc<crate::desktop::fusion_recorder::DesktopFusionRecorderFactory>>,
    implement_host: Option<Arc<dyn platform_api::FusionImplementHost>>,
}

impl DesktopFusionCommandHandler {
    /// Construct.
    #[must_use]
    pub fn new(
        registry: Arc<tasks::registry::TaskRegistry>,
        executor: Arc<dyn FusionExecutor>,
        handle: Arc<dyn OrchestratorHandle>,
        parent_profiles: BTreeMap<String, String>,
    ) -> Self {
        Self {
            registry,
            executor,
            handle,
            parent_profiles,
            durable_publication_available: true,
            publication_retrier: None,
            implement_host: None,
        }
    }

    /// The host `/fusion clean` discards implement-mode worktrees through.
    #[must_use]
    pub fn with_implement_host(
        mut self,
        host: Option<Arc<dyn platform_api::FusionImplementHost>>,
    ) -> Self {
        self.implement_host = host;
        self
    }

    /// `/fusion clean`: discard every implement-mode worktree and patch file.
    /// Refused while a Fusion run is going, whose worktrees are in use.
    async fn clean_worktrees(&self) -> String {
        let Some(host) = &self.implement_host else {
            return "This host has no implement-mode worktrees to clean.".into();
        };
        let running = self.registry.list().await.iter().any(|task| {
            task.base().task_type == TaskType::LocalFusion && !task.base().status.is_terminal()
        });
        if running {
            return "A Fusion run is still going; clean up after it finishes.".into();
        }
        match fusion::clean_worktrees(host.as_ref()).await {
            0 => "No Fusion worktrees to clean.".into(),
            1 => "Removed 1 Fusion worktree.".into(),
            n => format!("Removed {n} Fusion worktrees."),
        }
    }

    /// Declare whether completed Fusion answers can be durably published.
    /// Production sets this from the immutable session-persistence mode before
    /// the command handler is registered.
    #[must_use]
    pub fn with_durable_publication_available(mut self, available: bool) -> Self {
        self.durable_publication_available = available;
        self
    }

    /// Attach the durable outbox owner used by the explicit local-only retry
    /// form. This accepts only a run id; no executor or prompt reaches it.
    #[must_use]
    pub fn with_publication_retrier(
        mut self,
        retrier: Option<Arc<crate::desktop::fusion_recorder::DesktopFusionRecorderFactory>>,
    ) -> Self {
        self.publication_retrier = retrier;
        self
    }
}

fn publication_retry_run_id(args: &ParsedSlashCommand) -> Result<Option<FusionRunId>, String> {
    let contains_retry = args
        .positional_args
        .iter()
        .any(|token| token == "--retry-publication");
    if !contains_retry {
        return Ok(None);
    }
    if args.positional_args.len() != 2 || args.positional_args[0] != "--retry-publication" {
        return Err(format!(
            "--retry-publication cannot be combined with a prompt or Fusion flags\n{FUSION_ARGUMENT_HINT}"
        ));
    }
    FusionRunId::parse(args.positional_args[1].clone())
        .map(Some)
        .map_err(|error| format!("{error}\n{FUSION_ARGUMENT_HINT}"))
}

impl DesktopFusionCommandHandler {
    async fn execute(
        &self,
        args: &ParsedSlashCommand,
        session_id: protocol::SessionId,
    ) -> CommandResult {
        let retry_run_id = match publication_retry_run_id(args) {
            Ok(retry) => retry,
            Err(message) => {
                return CommandResult::Done {
                    display: Some(message),
                }
            }
        };
        if let Some(run_id) = retry_run_id {
            let Some(retrier) = self.publication_retrier.as_ref() else {
                return CommandResult::Done {
                    display: Some(
                        "fusion publication retry failed: durable publication is unavailable"
                            .into(),
                    ),
                };
            };
            let receipt = retrier.retry_publication(session_id, run_id.as_str()).await;
            let display = match receipt.status {
                platform_api::FusionPublicationStatus::Published => {
                    format!("Fusion publication {} is published.", run_id.as_str())
                }
                platform_api::FusionPublicationStatus::Queued => format!(
                    "Fusion publication {} is durably queued for retry.",
                    run_id.as_str()
                ),
                _ => format!(
                    "fusion publication retry failed: {}",
                    receipt
                        .error
                        .as_deref()
                        .unwrap_or("durable outbox did not accept the retry")
                ),
            };
            return CommandResult::Done {
                display: Some(display),
            };
        }
        if args.raw_args.trim().eq_ignore_ascii_case("setup") {
            return CommandResult::Done {
                display: Some(FUSION_SETUP_ELSEWHERE.to_string()),
            };
        }
        if args.raw_args.trim().eq_ignore_ascii_case("clean") {
            return CommandResult::Done {
                display: Some(self.clean_worktrees().await),
            };
        }
        let parsed = match parse_fusion_slash(args) {
            Ok(parsed) => parsed,
            Err(msg) => {
                return CommandResult::Done { display: Some(msg) };
            }
        };
        let snapshot = self.handle.get_status_snapshot().await;
        let conversation_id = session_id.to_string();
        let surface = self.executor.agent_surface();
        let preset = parsed.preset.unwrap_or(surface.default_preset);
        let cross = parsed
            .cross_provider
            .unwrap_or(surface.slash_cross_provider_default);
        let request = fusion_request_from_slash(
            parsed,
            snapshot.model_profile.clone().unwrap_or_else(|| {
                self.parent_profiles
                    .get(&snapshot.model)
                    .cloned()
                    .unwrap_or_default()
            }),
            snapshot.model.clone(),
            surface.slash_cross_provider_default,
            surface.default_preset,
            surface.default_partial_ok,
        );
        let preset_word = match preset {
            platform_api::FusionPreset::Quality => "quality",
            platform_api::FusionPreset::Fast => "fast",
        };
        let scope_word = if cross {
            "cross-provider"
        } else {
            "same-provider"
        };
        let preset_word = if request.mode == platform_api::FusionPanelMode::Implement {
            "implement"
        } else {
            preset_word
        };
        let description = fusion_task_description(preset_word, scope_word, &request.prompt);
        let display = match self
            .registry
            .spawn(
                TaskType::LocalFusion,
                TaskSpawnInput::LocalFusion {
                    request,
                    conversation_id: conversation_id.clone(),
                },
                description,
            )
            .await
        {
            Ok(task_id) => format!("Fusion started: {task_id}  {preset_word}  {scope_word}"),
            Err(err) => format!("fusion failed to start: {err}"),
        };
        CommandResult::Done {
            display: Some(display),
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for DesktopFusionCommandHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        // Persist every invocation before any publication retry or provider work.
        // Keep the same target even if the live conversation changes meanwhile.
        if !self.durable_publication_available {
            return CommandResult::Done {
                display: Some(format!(
                    "fusion failed to start: {FUSION_PERSISTENCE_REQUIRED}"
                )),
            };
        }
        let session_id = self.handle.current_session_id().await;
        let conversation_id = session_id.to_string();
        let raw_request = if args.raw_args.is_empty() {
            "/fusion".to_string()
        } else {
            format!("/fusion {}", args.raw_args)
        };
        if let Err(error) = self
            .handle
            .append_fusion_command_message(&conversation_id, &raw_request, true)
            .await
        {
            return CommandResult::Done {
                display: Some(format!("fusion failed to start: {error}")),
            };
        }
        let result = self.execute(args, session_id).await;
        if let CommandResult::Done {
            display: Some(display),
        } = result
        {
            let display = match self
                .handle
                .append_fusion_command_message(&conversation_id, &display, false)
                .await
            {
                Ok(()) => display,
                Err(error) => format!("{display}\nCould not save the Fusion reply: {error}"),
            };
            CommandResult::Done {
                display: Some(display),
            }
        } else {
            result
        }
    }

    fn name(&self) -> &str {
        "fusion"
    }

    fn description(&self) -> &str {
        "Run a multi-model Fusion deliberation (may add cost and cross-provider egress)"
    }

    fn argument_hint(&self) -> Option<&str> {
        Some(FUSION_ARGUMENT_HINT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::parse_slash_command;
    use platform_api::{FusionStatus, FusionTiming, FusionUsage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dummy_result(run_id: &str) -> FusionResult {
        FusionResult {
            schema_version: 2,
            run_id: run_id.into(),
            status: FusionStatus::Unanalyzed,
            analysis_failure: Some("timeout".into()),
            analysis: None,
            responses: vec![platform_api::PanelMaterial {
                panel_id: "P1".into(),
                summary: "summary".into(),
                candidate_answer: "the secret panel answer".into(),
                claims: vec![],
                evidence_checks: Default::default(),
                risks: vec![],
                unresolved_questions: vec![],
                ..Default::default()
            }],
            panels: vec![],
            usage: FusionUsage::default(),
            timing: FusionTiming::default(),
            egress_profiles: vec![],
            mode: Default::default(),
        }
    }

    struct UnusedExecutor;

    impl FusionExecutor for UnusedExecutor {
        fn prepare(
            self: Arc<Self>,
            _: platform_api::FusionSubmission,
        ) -> Result<platform_api::PreparedFusionRun, platform_api::FusionError> {
            panic!("the unregistered task must never dispatch a provider request")
        }
    }

    #[tokio::test]
    async fn every_command_branch_preserves_original_request_and_reply() {
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("session.jsonl");
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let handle = Arc::new(
            orchestrator::ConversationOrchestrator::new(
                orchestrator::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_jsonl_writer(Arc::new(session::jsonl::writer::JsonlWriter::new(
                transcript_path.clone(),
                fs.clone(),
            ))),
        );
        let registry = Arc::new(tasks::registry::TaskRegistry::new(
            Arc::new(platform_posix::runtime::PosixRuntime::new()),
            fs.clone(),
            Arc::new(tasks::output_manager::TaskOutputManager::new(
                dir.path().join("tasks"),
                fs,
            )),
        ));
        let command = DesktopFusionCommandHandler::new(
            registry,
            Arc::new(UnusedExecutor),
            handle.clone(),
            BTreeMap::new(),
        );
        let invocations = [
            (
                "/fusion --quality explain cursor behavior\nKeep this original detail",
                "fusion failed to start:",
            ),
            ("/fusion setup", FUSION_SETUP_ELSEWHERE),
            ("/fusion", ""),
            ("/fusion --retry-publication invalid", ""),
            (
                "/fusion --retry-publication fu_0123456789abcdef0123456789abcdef",
                "fusion publication retry failed:",
            ),
        ];
        for (index, (raw, expected_prefix)) in invocations.iter().enumerate() {
            let result = command.handle(&parse_slash_command(raw).unwrap()).await;
            let CommandResult::Done {
                display: Some(display),
            } = result
            else {
                panic!("expected a command reply")
            };
            assert!(display.starts_with(expected_prefix), "{display}");
            let text = std::fs::read_to_string(&transcript_path).unwrap();
            let rows: Vec<serde_json::Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), (index + 1) * 2);
            assert_eq!(rows[index * 2]["message"]["content"][0]["text"], *raw);
            assert_eq!(
                rows[index * 2 + 1]["message"]["content"][0]["text"],
                display
            );
            assert_eq!(
                handle.get_status_snapshot().await.n_messages,
                u32::try_from((index + 1) * 2).unwrap()
            );
        }
        let disabled = command.with_durable_publication_available(false);
        for (raw, _) in invocations {
            let CommandResult::Done {
                display: Some(display),
            } = disabled.handle(&parse_slash_command(raw).unwrap()).await
            else {
                panic!("expected a persistence failure")
            };
            assert!(display.contains(FUSION_PERSISTENCE_REQUIRED), "{display}");
        }
        assert_eq!(
            std::fs::read_to_string(&transcript_path)
                .unwrap()
                .lines()
                .count(),
            10
        );
        // A runtime storage failure must also prevent execution, even when
        // durable publication was enabled at composition time.
        std::fs::remove_file(&transcript_path).unwrap();
        std::fs::create_dir(&transcript_path).unwrap();
        let unavailable = disabled.with_durable_publication_available(true);
        for (raw, _) in invocations {
            let CommandResult::Done {
                display: Some(display),
            } = unavailable.handle(&parse_slash_command(raw).unwrap()).await
            else {
                panic!("expected a storage failure")
            };
            assert!(display.starts_with("fusion failed to start:"), "{display}");
        }
        assert_eq!(handle.get_status_snapshot().await.n_messages, 10);
    }

    struct CountingSink(AtomicUsize);

    #[async_trait]
    impl FusionCompletionSink for CountingSink {
        async fn publish(
            &self,
            _conversation_id: &str,
            _result: &FusionResult,
        ) -> FusionPublicationReceipt {
            self.0.fetch_add(1, Ordering::SeqCst);
            FusionPublicationReceipt::published()
        }
    }

    // ---- G012: `description` was `request.prompt.clone()` verbatim — an
    // unbounded, un-labeled string that leaked the full prompt into
    // `TaskCreated` hook payloads, `list()`, notifications, and RuntimeCenter
    // (which has no label for `local_fusion` and shows the raw string). ----

    #[test]
    fn description_is_labeled_with_preset_scope_and_truncated_first_line() {
        let desc = fusion_task_description(
            "quality",
            "cross-provider",
            "review this diff for races\nand also check the locking",
        );
        assert_eq!(
            desc,
            "Fusion quality cross-provider: review this diff for races"
        );
    }

    #[test]
    fn description_truncates_a_long_first_line_at_eighty_chars() {
        let long_line = "x".repeat(200);
        let desc = fusion_task_description("fast", "same-provider", &long_line);
        assert_eq!(
            desc,
            format!("Fusion fast same-provider: {}…", "x".repeat(80))
        );
    }

    #[test]
    fn description_does_not_truncate_a_short_prompt() {
        let desc = fusion_task_description("fast", "same-provider", "short");
        assert_eq!(desc, "Fusion fast same-provider: short");
    }

    #[test]
    fn publication_retry_is_strictly_local_and_mutex_with_run_arguments() {
        let run_id = "fu_0123456789abcdef0123456789abcdef";
        let retry =
            parse_slash_command(&format!("/fusion --retry-publication {run_id}")).expect("slash");
        assert_eq!(
            publication_retry_run_id(&retry)
                .expect("valid retry")
                .expect("retry mode")
                .as_str(),
            run_id
        );

        for invocation in [
            format!("/fusion --quality --retry-publication {run_id}"),
            format!("/fusion --retry-publication {run_id} prompt"),
            "/fusion --retry-publication not-a-run-id".to_string(),
        ] {
            let parsed = parse_slash_command(&invocation).expect("slash");
            assert!(
                publication_retry_run_id(&parsed).is_err(),
                "retry must reject provider-bearing prompt/flag input: {invocation}"
            );
        }
    }

    #[tokio::test]
    async fn deferred_sink_fails_closed_before_bind_and_does_not_replay() {
        let deferred = DeferredFusionCompletionSink::new();
        let result = dummy_result("fu_1");
        let first = deferred.publish("c", &result).await;
        let duplicate = deferred.publish("c", &result).await;
        assert_eq!(
            first.status,
            platform_api::FusionPublicationStatus::OutboxFailed
        );
        assert_eq!(duplicate, first);
        assert!(first
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not durably queued")));

        let counter = Arc::new(CountingSink(AtomicUsize::new(0)));
        deferred.bind(counter.clone()).await;
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "binding must not silently publish an item whose task already recorded OutboxFailed"
        );

        let published = deferred.publish("c", &result).await;
        assert_eq!(
            published.status,
            platform_api::FusionPublicationStatus::Published
        );
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    // ---- F006/WP6 item 3: before this, a finished background run's only
    // trace was the `<fusion-result>` meta row appended to history — nothing
    // live ever told the connected client the run had finished. ----------

    #[tokio::test]
    async fn publish_emits_exactly_one_content_free_background_notice() {
        let mock = Arc::new(orchestrator::test_support::MockOrchestratorHandle::new());
        let session_id = mock.current_session_id().await.to_string();
        let sink = DesktopFusionCompletionSink::new(mock.clone());
        let mut result = dummy_result("fu_1");
        result.status = FusionStatus::Analyzed;

        sink.publish(&session_id, &result).await;

        let notices = mock.background_notices();
        assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
        assert!(
            notices[0].contains("Fusion run finished"),
            "got: {notices:?}"
        );
        // Content-free: no panel text leaks into the live notice — the
        // material reaches the main model through the task notification.
        assert!(
            !notices[0].contains("the secret panel answer"),
            "notice must not leak panel text: {notices:?}"
        );

        // Idempotent publish (same run id) must not double-notify.
        sink.publish(&session_id, &result).await;
        assert_eq!(mock.background_notices().len(), 1);
    }

    #[tokio::test]
    async fn publish_notice_differs_for_unanalyzed_vs_analyzed() {
        let mock = Arc::new(orchestrator::test_support::MockOrchestratorHandle::new());
        let session_id = mock.current_session_id().await.to_string();
        let sink = DesktopFusionCompletionSink::new(mock.clone());
        let mut result = dummy_result("fu_unanalyzed");
        result.status = FusionStatus::Unanalyzed;

        sink.publish(&session_id, &result).await;

        let notices = mock.background_notices();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("without an analysis"),
            "got: {notices:?}"
        );
    }

    /// [Round-5 finding 13] The PRODUCTION `append_meta_user_message_to_session`
    /// (`orchestrator/src/handle_impl.rs`) returns `Ok(())` for a target
    /// session that is no longer current as long as the durable write landed
    /// — it only touches live history inside its `current == target` branch,
    /// and only errors when nothing was persisted. `MockOrchestratorHandle`
    /// uses the platform-api TRAIT DEFAULT instead, which fails closed on a
    /// session mismatch, so every existing test drives the opposite of
    /// production on exactly the path this finding is about. This double
    /// delegates everything to the mock except that one method, whose
    /// production semantics it reproduces.
    struct AppendsToAnySessionHandle {
        inner: Arc<orchestrator::test_support::MockOrchestratorHandle>,
        appended_to: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl OrchestratorHandle for AppendsToAnySessionHandle {
        async fn append_meta_user_message_to_session(
            &self,
            session_id: &str,
            _text: &str,
        ) -> Result<(), platform_api::HandleError> {
            self.appended_to
                .lock()
                .unwrap()
                .push(session_id.to_string());
            Ok(())
        }
        async fn emit_background_system_notice(&self, body: &str) {
            self.inner.emit_background_system_notice(body).await;
        }
        async fn current_session_id(&self) -> protocol::SessionId {
            self.inner.current_session_id().await
        }
        async fn clear_session(&self) -> Result<(), platform_api::HandleError> {
            self.inner.clear_session().await
        }
        async fn force_compact(
            &self,
        ) -> Result<platform_api::CompactionSummary, platform_api::HandleError> {
            self.inner.force_compact().await
        }
        async fn snapshot_cost(&self) -> platform_api::CostSnapshot {
            self.inner.snapshot_cost().await
        }
        async fn switch_model(
            &self,
            model: &str,
            profile: Option<&str>,
        ) -> Result<(), platform_api::HandleError> {
            self.inner.switch_model(model, profile).await
        }
        async fn request_exit(&self) {
            self.inner.request_exit().await;
        }
        async fn current_should_exit(&self) -> bool {
            self.inner.current_should_exit().await
        }
        async fn open_memory_editor(
            &self,
        ) -> Result<platform_api::MemoryEditorOutcome, platform_api::HandleError> {
            self.inner.open_memory_editor().await
        }
        async fn list_mcp_servers(&self) -> Vec<platform_api::McpServerInfo> {
            self.inner.list_mcp_servers().await
        }
        async fn list_skills(&self) -> Vec<platform_api::SkillInfo> {
            self.inner.list_skills().await
        }
        async fn list_hooks(&self) -> Vec<platform_api::HookInfo> {
            self.inner.list_hooks().await
        }
        async fn list_agents(&self) -> Vec<platform_api::AgentInfo> {
            self.inner.list_agents().await
        }
        async fn run_doctor_checks(&self) -> platform_api::DoctorReport {
            self.inner.run_doctor_checks().await
        }
        async fn get_status_snapshot(&self) -> platform_api::StatusSnapshot {
            self.inner.get_status_snapshot().await
        }
        async fn edit_config_file(
            &self,
        ) -> Result<platform_api::MemoryEditorOutcome, platform_api::HandleError> {
            self.inner.edit_config_file().await
        }
        async fn edit_permissions_file(
            &self,
        ) -> Result<platform_api::MemoryEditorOutcome, platform_api::HandleError> {
            self.inner.edit_permissions_file().await
        }
        async fn list_available_models(&self) -> Vec<String> {
            self.inner.list_available_models().await
        }
    }

    /// [Round-5 finding 13] `/fusion` started in session A, `/clear` before it
    /// finishes, result lands in A's durable transcript while the user is
    /// looking at session B. The append SUCCEEDS, so the
    /// `could not be recorded` copy never fires; the success copy must not
    /// claim the result is in "this conversation", which contains no fusion
    /// result at all.
    #[tokio::test]
    async fn publish_does_not_claim_this_conversation_when_the_append_landed_elsewhere() {
        let mock = Arc::new(orchestrator::test_support::MockOrchestratorHandle::new());
        let current = mock.current_session_id().await.to_string();
        // [Rework r1] Production NEVER hands this sink a bare uuid: both
        // producers feed it `current_session_id().await.to_string()`
        // (fusion_command.rs:256, tasks/src/registry.rs:2342), and
        // `SessionId`'s `Display` (protocol/src/ids.rs, `id_newtype!(SessionId,
        // "sess")`) renders `"sess:<uuid>"`. A bare-uuid fixture here silently
        // hid a truncation that prints `"sess:111"` in production.
        let started_in = "sess:11112222-3333-4444-5555-666677778888";
        assert_eq!(
            started_in,
            protocol::SessionId::parse_prefixed(started_in)
                .expect("fixture must parse as a real SessionId")
                .to_string(),
            "sanity: the fixture must be exactly what `SessionId::to_string()` produces"
        );
        assert_ne!(
            current, started_in,
            "sanity: the run's session must differ from the connected one"
        );
        let handle = Arc::new(AppendsToAnySessionHandle {
            inner: mock.clone(),
            appended_to: std::sync::Mutex::new(Vec::new()),
        });
        let sink = DesktopFusionCompletionSink::new(handle.clone());
        let mut result = dummy_result("fu_cleared");
        result.status = FusionStatus::Analyzed;

        sink.publish(started_in, &result).await;

        assert_eq!(
            handle.appended_to.lock().unwrap().as_slice(),
            [started_in.to_string()],
            "sanity: the durable append targeted the ORIGINATING session and succeeded"
        );
        let notices = mock.background_notices();
        assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
        assert!(
            !notices[0].contains("handed to the main model"),
            "the result is NOT in this conversation — that copy is false here: {notices:?}"
        );
        assert!(
            notices[0].contains("saved to the conversation it was started in"),
            "the notice must say where the result actually went: {notices:?}"
        );
        assert!(
            notices[0].contains("11112222"),
            "the notice must name the originating session so the user can find it: {notices:?}"
        );
        // The pointer must be usable against `/resume`'s listing, which keys
        // rows by the JSONL file stem — the BARE uuid (`session/src/jsonl/
        // loader.rs`: `let sid = stem.as_str()`). Printing the `sess:` prefix
        // inside an 8-character budget spends 5 of them on a constant and
        // leaves 3 hex digits of the real id.
        assert!(
            !notices[0].contains("sess:"),
            "the id prefix must be stripped, not truncated into: {notices:?}"
        );
        assert!(
            !notices[0].contains("could not be recorded"),
            "the append SUCCEEDED — the unrecorded copy would be wrong too: {notices:?}"
        );
    }

    /// The other half of the same branch: when the run's session IS the
    /// connected one, the original F006 copy must be unchanged.
    #[tokio::test]
    async fn publish_still_says_this_conversation_when_the_append_landed_here() {
        let mock = Arc::new(orchestrator::test_support::MockOrchestratorHandle::new());
        let current = mock.current_session_id().await.to_string();
        let handle = Arc::new(AppendsToAnySessionHandle {
            inner: mock.clone(),
            appended_to: std::sync::Mutex::new(Vec::new()),
        });
        let sink = DesktopFusionCompletionSink::new(handle.clone());
        let mut result = dummy_result("fu_same_session");
        result.status = FusionStatus::Analyzed;

        let receipt = sink.publish(&current, &result).await;
        assert_eq!(
            receipt.status,
            platform_api::FusionPublicationStatus::Published
        );

        let notices = mock.background_notices();
        assert_eq!(notices.len(), 1, "exactly one notice: {notices:?}");
        assert!(
            notices[0].contains("handed to the main model"),
            "got: {notices:?}"
        );
    }

    #[tokio::test]
    async fn disabled_session_persistence_never_claims_published() {
        let mock = Arc::new(orchestrator::test_support::MockOrchestratorHandle::new());
        let current = mock.current_session_id().await.to_string();
        let sink =
            DesktopFusionCompletionSink::new(mock.clone()).with_durable_storage_enabled(false);

        let receipt = sink.publish(&current, &dummy_result("fu_ephemeral")).await;

        assert_eq!(
            receipt.status,
            platform_api::FusionPublicationStatus::StorageFailure
        );
        assert!(receipt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("requires session persistence")));
        assert!(
            mock.background_notices().is_empty(),
            "the defensive guard returns before attempting append/notification work"
        );
    }

    // ---- [Finding 13]: a durable-append failure (e.g. the target session
    // is no longer current — the default `append_meta_user_message_to_session`
    // fails exactly this way when the id it is given no longer matches
    // `current_session_id`, which is what a `/clear` mid-run or a JSONL
    // write error looks like from this sink's point of view) used to leave
    // the run with NO trace at all: the F006 notice sat after the early
    // `return`, so it never fired on the one path where a live signal is
    // the user's only timely trace. ----

    #[tokio::test]
    async fn publish_still_emits_a_notice_when_the_durable_append_fails() {
        let mock = Arc::new(orchestrator::test_support::MockOrchestratorHandle::new());
        let sink = DesktopFusionCompletionSink::new(mock.clone());
        let mut result = dummy_result("fu_append_fails");
        result.status = FusionStatus::Analyzed;

        // A conversation id that does NOT match the mock's current session id
        // drives the trait's default `append_meta_user_message_to_session`
        // straight into its `Err(ActionFailed(...))` arm — the same shape a
        // real desktop handle returns once the target session is no longer
        // current (e.g. after `/clear`) or the JSONL append itself fails.
        let receipt = sink
            .publish("some-other-session-that-is-not-current", &result)
            .await;
        assert_eq!(
            receipt.status,
            platform_api::FusionPublicationStatus::StorageFailure,
            "an append error must never be reported as Published"
        );

        let notices = mock.background_notices();
        assert_eq!(
            notices.len(),
            1,
            "an append failure must still surface a live notice, not silence: {notices:?}"
        );
        assert!(
            notices[0].contains("could not be recorded"),
            "the failure notice must not claim the result was appended to \
             this conversation (it was not): {notices:?}"
        );
    }
}
