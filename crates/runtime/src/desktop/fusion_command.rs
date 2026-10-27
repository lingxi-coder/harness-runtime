//! Desktop `/fusion` slash handler and durable completion notices.
//!
//! Registered at composition time (not in `BUILTIN_COMMAND_NAMES`). `/fusion`
//! is explicit per-run and works even when `fusion.enabled` is false.

use async_trait::async_trait;
use command_api::builtins::{fusion_request_from_slash, parse_fusion_slash};
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use lingxi_core::host::{FusionExecutor, FusionRunId, FusionStatus, OrchestratorHandle};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::BTreeMap;
use std::sync::Arc;
use tasks::{TaskSpawnInput, TaskType};

const FUSION_ARGUMENT_HINT: &str = "[--strategy scope:id] [--quality|--fast] [--same-provider|--cross-provider] [--verify-claims] [--implement [--verify \"COMMAND\"]...] PROMPT | setup | clean | --retry-publication fu_RUN_ID";

/// What `/fusion setup` reports when it reaches THIS handler.
///
/// The TUI never gets here: `ChatWidget::cmd_fusion` opens the wizard in the
/// bottom pane and returns without dispatching. Every other host routes the
/// whole line to the command registry, and without this arm `setup` would be
/// taken as the PROMPT — a real multi-model deliberation, billed, on the word
/// "setup". Naming the surfaces that can actually configure it is the only
/// honest answer a handler with no UI of its own can give.
const FUSION_SETUP_ELSEWHERE: &str = "`/fusion setup` opens an interactive wizard, which this surface cannot show.\n     In the terminal UI run `/fusion setup`; on the desktop open Settings → Fusion 多模型合议.\n     Create a named strategy with Panel models and an Analyst, choose user/project/local scope, and set a default or select it with --strategy <scope>:<id>.";
const FUSION_PERSISTENCE_REQUIRED: &str = "durable session storage is disabled; /fusion requires session persistence (remove --no-session-persistence)";

/// Record a host-side preparation failure without resolving or running Fusion.
/// The selected strategy can change while credentials are loading, so replaying
/// the slash command to record its error could accidentally start a new run.
pub async fn record_fusion_command_failure(
    handle: &dyn OrchestratorHandle,
    raw: &str,
    error: &str,
) -> Result<String, String> {
    if !raw
        .split_whitespace()
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("/fusion"))
    {
        return Err("Only Fusion preparation failures can be recorded here".into());
    }
    if error.trim().is_empty() {
        return Err("Fusion preparation error is required".into());
    }
    let target = handle.current_session_id().await.to_string();
    let display = format!("fusion failed to start: {error}");
    let raw_source = Utf16JsonProjection::from(raw);
    let display_source = Utf16JsonProjection::from(display.clone());
    handle
        .append_fusion_command_message(&target, &raw_source, true)
        .await
        .map_err(|error| error.to_string())?;
    handle
        .append_fusion_command_message(&target, &display_source, false)
        .await
        .map_err(|error| error.to_string())?;
    Ok(display)
}

/// Short, content-free notice text for a finished Fusion run — no prompt,
/// panel text, or provider names. The material reaches the main model through
/// the task notification.
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
    let body = lingxi_core::types::SessionId::parse_prefixed(conversation_id).map_or_else(
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
            let body = lingxi_core::types::SessionId::parse_prefixed(conversation_id).map_or_else(
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
    implement_host: Option<Arc<dyn lingxi_core::host::FusionImplementHost>>,
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
        host: Option<Arc<dyn lingxi_core::host::FusionImplementHost>>,
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
        .enumerate()
        .any(|(index, _)| args.positional_arg_utf8(index) == Some("--retry-publication"));
    if !contains_retry {
        return Ok(None);
    }
    if args.positional_args.len() != 2 || args.positional_arg_utf8(0) != Some("--retry-publication")
    {
        return Err(format!(
            "--retry-publication cannot be combined with a prompt or Fusion flags\n{FUSION_ARGUMENT_HINT}"
        ));
    }
    let run_id = args.positional_arg_utf8(1).ok_or_else(|| {
        format!("--retry-publication requires a valid Unicode run ID\n{FUSION_ARGUMENT_HINT}")
    })?;
    FusionRunId::parse(run_id)
        .map(Some)
        .map_err(|error| format!("{error}\n{FUSION_ARGUMENT_HINT}"))
}

impl DesktopFusionCommandHandler {
    async fn execute(
        &self,
        args: &ParsedSlashCommand,
        session_id: lingxi_core::types::SessionId,
    ) -> CommandResult {
        let retry_run_id = match publication_retry_run_id(args) {
            Ok(retry) => retry,
            Err(message) => {
                return CommandResult::Done {
                    display: Some(message.into()),
                };
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
                lingxi_core::host::FusionPublicationStatus::Published => {
                    format!("Fusion publication {} is published.", run_id.as_str())
                }
                lingxi_core::host::FusionPublicationStatus::Queued => format!(
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
                display: Some(display.into()),
            };
        }
        if args
            .raw_args_utf8()
            .is_some_and(|raw| raw.trim().eq_ignore_ascii_case("setup"))
        {
            return CommandResult::Done {
                display: Some(FUSION_SETUP_ELSEWHERE.into()),
            };
        }
        if args
            .raw_args_utf8()
            .is_some_and(|raw| raw.trim().eq_ignore_ascii_case("clean"))
        {
            return CommandResult::Done {
                display: Some(self.clean_worktrees().await.into()),
            };
        }
        let parsed = match parse_fusion_slash(args) {
            Ok(parsed) => parsed,
            Err(msg) => {
                return CommandResult::Done {
                    display: Some(msg.into()),
                };
            }
        };
        let session_origin = match self.handle.capture_session_origin().await {
            Ok(Some(origin)) if origin.session_id() != session_id => {
                return CommandResult::Done {
                    display: Some("fusion failed to start: the originating session changed".into()),
                };
            }
            Ok(origin) => origin,
            Err(error) => {
                return CommandResult::Done {
                    display: Some(format!("fusion failed to start: {error}").into()),
                };
            }
        };
        let snapshot = self.handle.get_status_snapshot().await;
        let conversation_id = session_id.to_string();
        let strategy = match self.executor.capture_strategy(parsed.strategy.as_ref()) {
            Ok(strategy) => strategy,
            Err(error) => {
                return CommandResult::Done {
                    display: Some(format!("fusion failed to start: {error}").into()),
                }
            }
        };
        if parsed.strategy_revision.as_ref().is_some_and(|revision| {
            strategy
                .as_ref()
                .is_none_or(|selected| &selected.revision != revision)
        }) {
            return CommandResult::Done {
                display: Some("fusion failed to start: strategy changed since credential preparation; select the strategy again".into()),
            };
        }
        let surface = match self.executor.agent_surface_for_strategy(strategy.as_ref()) {
            Ok(surface) => surface,
            Err(error) => {
                return CommandResult::Done {
                    display: Some(format!("fusion failed to start: {error}").into()),
                }
            }
        };
        let preset = parsed.preset.unwrap_or(surface.default_preset);
        let cross = parsed
            .cross_provider
            .unwrap_or(surface.slash_cross_provider_default);
        let mut request = fusion_request_from_slash(
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
        request.capture_strategy(strategy);
        let preset_word = match preset {
            lingxi_core::host::FusionPreset::Quality => "quality",
            lingxi_core::host::FusionPreset::Fast => "fast",
        };
        let scope_word = if cross {
            "cross-provider"
        } else {
            "same-provider"
        };
        let preset_word = if request.mode == lingxi_core::host::FusionPanelMode::Implement {
            "implement"
        } else {
            preset_word
        };
        let mut description: Utf16JsonProjection =
            fusion_task_description(preset_word, scope_word, &request.prompt).into();
        if let Some(strategy) = request.strategy_metadata() {
            let prefix = format!("{} [{:?}] · ", strategy.name, strategy.reference.scope);
            description = prepend_generated_text(&prefix, &description);
        }
        let display = match self
            .registry
            .spawn(
                TaskType::LocalFusion,
                TaskSpawnInput::LocalFusion {
                    request,
                    conversation_id: conversation_id.clone(),
                    session_origin,
                },
                description,
            )
            .await
        {
            Ok(task_id) => format!("Fusion started: {task_id}  {preset_word}  {scope_word}"),
            Err(err) => format!("fusion failed to start: {err}"),
        };
        CommandResult::Done {
            display: Some(display.into()),
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
                display: Some(
                    format!("fusion failed to start: {FUSION_PERSISTENCE_REQUIRED}").into(),
                ),
            };
        }
        let session_id = self.handle.current_session_id().await;
        let conversation_id = session_id.to_string();
        let visible_args = fusion_visible_args(&args.raw_args);
        let visible_units = visible_args.string_units("").unwrap_or_default();
        let raw_request = if visible_units.is_empty() {
            Utf16JsonProjection::from("/fusion")
        } else {
            let mut units = "/fusion ".encode_utf16().collect::<Vec<_>>();
            units.extend_from_slice(&visible_units);
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                .expect("Fusion command source is a root string")
        };
        if let Err(error) = self
            .handle
            .append_fusion_command_message(&conversation_id, &raw_request, true)
            .await
        {
            return CommandResult::Done {
                display: Some(format!("fusion failed to start: {error}").into()),
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
                Err(error) => append_generated_text(
                    &display,
                    &format!("\nCould not save the Fusion reply: {error}"),
                ),
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

    fn supports_non_interactive(&self) -> bool {
        true
    }
}

fn append_generated_text(source: &Utf16JsonProjection, suffix: &str) -> Utf16JsonProjection {
    let mut units = source.string_units("").unwrap_or_default();
    units.extend(suffix.encode_utf16());
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("appended command display is a root string")
}

fn prepend_generated_text(prefix: &str, source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(source.string_units("").unwrap_or_default());
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("prepended command description is a root string")
}

/// The desktop prepends this internal fence after credentials are ready.
/// Remove only that prefix so the transcript retains the exact user input.
fn fusion_visible_args(raw: &Utf16JsonProjection) -> Utf16JsonProjection {
    let Some(raw_units) = raw.string_units("") else {
        return raw.clone();
    };
    let prefix = "--strategy-revision ".encode_utf16().collect::<Vec<_>>();
    let Some(revision_and_prompt) = raw_units.strip_prefix(prefix.as_slice()) else {
        return raw.clone();
    };
    let mut unit_offset = 0usize;
    for decoded in char::decode_utf16(revision_and_prompt.iter().copied()) {
        match decoded {
            Ok(character) => {
                let character_units = character.len_utf16();
                if lingxi_core::host::effort::javascript_whitespace(character) {
                    let suffix = revision_and_prompt[unit_offset + character_units..].to_vec();
                    return Utf16JsonProjection::root_string(
                        String::from_utf16_lossy(&suffix),
                        suffix,
                    )
                    .expect("visible Fusion arguments are a root string");
                }
                unit_offset += character_units;
            }
            Err(_) => unit_offset += 1,
        }
    }
    raw.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::parse_slash_command;

    #[test]
    fn hidden_revision_prefix_is_absent_from_the_transcript_without_losing_source_units() {
        let original = Utf16JsonProjection::from("--strategy project:code  explain  this\nexactly");
        let pinned = Utf16JsonProjection::from(format!(
            "--strategy-revision abc123 {}",
            original.string_at("").unwrap()
        ));
        assert_eq!(
            fusion_visible_args(&pinned).string_units("").unwrap(),
            original.string_units("").unwrap()
        );
        assert_eq!(
            fusion_visible_args(&original).string_units("").unwrap(),
            original.string_units("").unwrap()
        );
        for separator in [
            "\n", "\t", "\r\n", "\u{FEFF}", "\u{00A0}", "\u{2003}", "\u{2028}",
        ] {
            let pinned = Utf16JsonProjection::from(format!(
                "--strategy-revision abc123{separator}Analyze this\nexactly"
            ));
            let expected = if separator == "\r\n" {
                "\nAnalyze this\nexactly"
            } else {
                "Analyze this\nexactly"
            };
            assert_eq!(fusion_visible_args(&pinned).string_at(""), Some(expected));
        }
        let unrelated = Utf16JsonProjection::from("explain --strategy-revision abc123");
        assert_eq!(
            fusion_visible_args(&unrelated).string_units("").unwrap(),
            unrelated.string_units("").unwrap()
        );

        let mut lone_units = "--strategy-revision abc123 "
            .encode_utf16()
            .collect::<Vec<_>>();
        lone_units.extend([0xD800, b'x' as u16]);
        let lone_source =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&lone_units), lone_units)
                .unwrap();
        assert_eq!(
            fusion_visible_args(&lone_source).string_units("").unwrap(),
            vec![0xD800, b'x' as u16]
        );
    }

    #[tokio::test]
    async fn host_preparation_failures_are_durable_without_a_fusion_executor() {
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("session.jsonl");
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let handle = orchestrator::ConversationOrchestrator::new(
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
            fs,
        )));
        let raw = "/fusion --strategy project:removed  compare\nthese approaches";
        let display =
            record_fusion_command_failure(&handle, raw, "Selected strategy is unavailable")
                .await
                .unwrap();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&transcript_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["message"]["content"][0]["text"], raw);
        assert_eq!(rows[1]["message"]["content"][0]["text"], display);
        assert_eq!(handle.get_status_snapshot().await.n_messages, 2);
        assert!(
            record_fusion_command_failure(&handle, "/other run", "error")
                .await
                .is_err()
        );
        assert_eq!(handle.get_status_snapshot().await.n_messages, 2);
        // Storage errors remain visible; no strategy/executor is consulted.
        std::fs::remove_file(&transcript_path).unwrap();
        std::fs::create_dir(&transcript_path).unwrap();
        assert!(
            record_fusion_command_failure(&handle, raw, "OAuth unavailable")
                .await
                .is_err()
        );
        assert_eq!(handle.get_status_snapshot().await.n_messages, 2);
    }

    struct UnusedExecutor;

    impl FusionExecutor for UnusedExecutor {
        fn prepare(
            self: Arc<Self>,
            _: lingxi_core::host::FusionSubmission,
        ) -> Result<lingxi_core::host::PreparedFusionRun, lingxi_core::host::FusionError> {
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
        let command = Arc::new(DesktopFusionCommandHandler::new(
            registry,
            Arc::new(UnusedExecutor),
            handle.clone(),
            BTreeMap::new(),
        ));
        let mut slash_registry = command_api::CommandRegistry::new();
        let handler: Arc<dyn BuiltinCommandHandler> = command.clone();
        slash_registry.register_builtin_handler(handler);
        let fusion = slash_registry.resolve(&"fusion".into()).unwrap();
        assert_eq!(fusion.native_type, None);
        assert!(fusion.supports_non_interactive);
        assert!(slash_registry.headless_dispatch_admitted(fusion));
        assert!(!slash_registry.headless_command_admitted(fusion));
        assert!(!slash_registry
            .headless_init_commands()
            .iter()
            .any(|command| command.name.string_at("") == Some("fusion")));
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
            let result = command
                .handle(&parse_slash_command(&(*raw).into()).unwrap())
                .await;
            let CommandResult::Done {
                display: Some(display),
            } = result
            else {
                panic!("expected a command reply")
            };
            assert!(
                display
                    .string_at("")
                    .is_some_and(|text| text.starts_with(expected_prefix)),
                "{display:?}"
            );
            let text = std::fs::read_to_string(&transcript_path).unwrap();
            let rows: Vec<serde_json::Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), (index + 1) * 2);
            assert_eq!(rows[index * 2]["message"]["content"][0]["text"], *raw);
            assert_eq!(
                rows[index * 2 + 1]["message"]["content"][0]["text"].as_str(),
                display.string_at("")
            );
            assert_eq!(
                handle.get_status_snapshot().await.n_messages,
                u32::try_from((index + 1) * 2).unwrap()
            );
        }
        drop(slash_registry);
        let command = Arc::try_unwrap(command)
            .unwrap_or_else(|_| panic!("the test registry must release its handler"));
        let disabled = command.with_durable_publication_available(false);
        for (raw, _) in invocations {
            let CommandResult::Done {
                display: Some(display),
            } = disabled
                .handle(&parse_slash_command(&raw.into()).unwrap())
                .await
            else {
                panic!("expected a persistence failure")
            };
            assert!(
                display
                    .string_at("")
                    .is_some_and(|text| text.contains(FUSION_PERSISTENCE_REQUIRED)),
                "{display:?}"
            );
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
            } = unavailable
                .handle(&parse_slash_command(&raw.into()).unwrap())
                .await
            else {
                panic!("expected a storage failure")
            };
            assert!(
                display
                    .string_at("")
                    .is_some_and(|text| text.starts_with("fusion failed to start:")),
                "{display:?}"
            );
        }
        assert_eq!(handle.get_status_snapshot().await.n_messages, 10);
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
        let retry = parse_slash_command(&format!("/fusion --retry-publication {run_id}").into())
            .expect("slash");
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
            let parsed = parse_slash_command(&invocation.clone().into()).expect("slash");
            assert!(
                publication_retry_run_id(&parsed).is_err(),
                "retry must reject provider-bearing prompt/flag input: {invocation}"
            );
        }
    }
}
