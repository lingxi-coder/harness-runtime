use crate::headless::sdk_prompt_suggestions::{
    AfterResult, SdkPromptSuggestionState, SDK_SUGGESTION_SETTLE_TIMEOUT_MS,
};
use crate::headless::stream_json::StreamJsonStream;
use crate::headless::HeadlessRuntime as Runtime;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Session-owned counterpart of native `HS`. A new input cancels speculation;
/// the 30-second bound applies only when the SDK input stream settles.
pub(in crate::headless) struct SdkPromptSuggestions {
    state: Mutex<SdkSuggestionRuntimeState>,
}

struct SdkSuggestionRuntimeState {
    machine: SdkPromptSuggestionState,
    inflight: Option<(CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl SdkPromptSuggestions {
    pub(in crate::headless) fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SdkSuggestionRuntimeState {
                machine: SdkPromptSuggestionState::new(enabled),
                inflight: None,
            }),
        })
    }

    pub(super) fn initialize(&self, requested: Option<bool>, remote_cowork: bool) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .machine
            .initialize(requested, remote_cowork);
    }

    pub(super) fn set_paused(&self, paused: bool) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .machine
            .set_paused(paused);
    }

    pub(super) fn input_started(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .machine
            .input_started();
    }

    pub(super) async fn on_command(
        &self,
        runtime: &Runtime,
        should_query: bool,
        prompt_content: Option<&Value>,
    ) {
        if !should_query {
            return;
        }
        let feedback = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((cancel, _)) = state.inflight.as_ref() {
                cancel.cancel();
            }
            state
                .machine
                .on_command(should_query, prompt_content, now_ms())
        };
        if let Some(payload) = feedback {
            runtime
                .orchestrator
                .record_prompt_suggestion_event(payload)
                .await;
        }
    }

    pub(super) async fn after_result(
        self: &Arc<Self>,
        runtime: &Runtime,
        stream: &Arc<StreamJsonStream>,
        turn_origin: Option<&str>,
    ) {
        let environment_disabled =
            orchestrator::ConversationOrchestrator::prompt_suggestion_environment_override(
                runtime
                    .services
                    .environment_variable("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION")
                    .as_deref(),
            ) == Some(false);
        let latest_origin = runtime.orchestrator.latest_prompt_suggestion_turn_origin();
        let turn_origin = latest_origin.as_deref().or(turn_origin);
        let Some(action) = self.admit_after_result(
            runtime,
            stream,
            turn_origin,
            environment_disabled,
            telemetry::feature_flags::flag_bool("tengu_chomp_sable", false),
        ) else {
            return;
        };
        if action == AfterResult::Disabled {
            return;
        }
        if let AfterResult::Suppressed(reason) = action {
            runtime.orchestrator.record_prompt_suggestion_event(json!({"source":"sdk","outcome":"suppressed","reason":reason,"prompt_id":"user_intent"})).await;
        }
    }

    /// Atomically admit the synchronous suggestion transition with respect to
    /// terminal policy refusal. The lock order matches `place_generated`:
    /// suggestion state first, then refusal watch. No watch borrow crosses an
    /// await; a generation admitted here is owned work and is cancelled/joined
    /// by the runtime shutdown path.
    pub(super) fn admit_after_result(
        self: &Arc<Self>,
        runtime: &Runtime,
        stream: &Arc<StreamJsonStream>,
        turn_origin: Option<&str>,
        environment_disabled: bool,
        sable: bool,
    ) -> Option<AfterResult> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.policy_helper_refusal.with_not_refused(|| {
            let action = state.machine.after_result(
                true,
                runtime.shutdown.is_cancelled(),
                environment_disabled,
                sable,
                turn_origin,
            );
            if action == AfterResult::Disabled {
                return action;
            }
            if let Some((cancel, _)) = state.inflight.as_ref() {
                cancel.cancel();
            }
            if let AfterResult::Generate(generation) = action {
                let cancel = runtime.shutdown.child_token();
                let query_cancel = cancel.clone();
                let owner = Arc::clone(self);
                let orchestrator = Arc::clone(&runtime.orchestrator);
                let stream = Arc::clone(stream);
                let handle = tokio::spawn(async move {
                    let result = match orchestrator
                        .generate_prompt_suggestion_query(query_cancel.clone())
                        .await
                    {
                        Ok(Some(result)) => result,
                        Ok(None) => return,
                        Err(error) => {
                            tracing::debug!(%error, "SDK prompt suggestion generation failed");
                            return;
                        }
                    };
                    let session_id = stream.session_id_handle().lock().await.clone();
                    owner.place_generated(generation, result, &stream, &session_id, &query_cancel);
                });
                state.inflight = Some((cancel, handle));
            }
            action
        })
    }

    fn place_generated(
        &self,
        generation: u64,
        result: lingxi_core::host::PromptSuggestionResult,
        stream: &StreamJsonStream,
        session_id: &str,
        cancel: &CancellationToken,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cancel.is_cancelled() {
            return;
        }
        let frame = state.machine.generated(
            generation,
            result.suggestion,
            result.generation_request_id,
            &uuid::Uuid::new_v4().to_string(),
            session_id,
            now_ms(),
            stream.holds_print_results(),
        );
        // Placement and invalidation share one lock, like the native JS loop.
        if let Some(frame) = frame {
            stream.emit_prompt_suggestion_frame(&frame);
        }
    }

    pub(super) fn emit_pending(&self, stream: &StreamJsonStream) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(frame) = state.machine.emit_pending(now_ms()) {
            stream.emit_prompt_suggestion_frame(&frame);
        }
    }

    pub(super) fn discard(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cancel, _)) = state.inflight.as_ref() {
            cancel.cancel();
        }
        state.machine.discard();
    }

    #[cfg(test)]
    pub(super) fn has_inflight_for_test(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .inflight
            .is_some()
    }

    pub(super) async fn settle(&self) {
        let task = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .inflight
            .take();
        if let Some((cancel, mut handle)) = task {
            if tokio::time::timeout(
                std::time::Duration::from_millis(SDK_SUGGESTION_SETTLE_TIMEOUT_MS),
                &mut handle,
            )
            .await
            .is_err()
            {
                cancel.cancel();
                let _ = handle.await;
            }
            cancel.cancel();
        }
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod sdk_controller_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retained_refusal_blocks_prompt_suggestion_admission_after_result() {
        use super::super::tests::fixture_runtime;
        let root = tempfile::tempdir().unwrap();
        let stream = Arc::new(StreamJsonStream::new_placeholder(
            crate::headless::io::Output::new(tokio::io::sink()),
        ));
        let runtime = Arc::new(fixture_runtime(stream.clone(), root.path()).await);

        let control = SdkPromptSuggestions::new(true);
        let control_action = control.admit_after_result(&runtime, &stream, None, false, false);
        assert!(matches!(control_action, Some(AfterResult::Generate(_))));
        assert!(
            control.has_inflight_for_test(),
            "with refusal, environment, and suppression gates off, the fixture admits a query"
        );
        control.discard();
        control.settle().await;

        let suggestions = SdkPromptSuggestions::new(true);
        let suggestions_for_lock = suggestions.clone();
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let lock_holder = tokio::task::spawn_blocking(move || {
            let guard = suggestions_for_lock
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = locked_tx.send(());
            release_rx.recv().unwrap();
            drop(guard);
        });
        locked_rx.await.unwrap();

        let suggestions_for_task = suggestions.clone();
        let stream_for_task = stream.clone();
        let runtime_for_task = runtime.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let admission = tokio::spawn(async move {
            let _ = started_tx.send(());
            suggestions_for_task
                .after_result(&runtime_for_task, &stream_for_task, None)
                .await;
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        runtime.policy_helper_refusal.trigger_refusal_for_test();
        release_tx.send(()).unwrap();
        lock_holder.await.unwrap();
        admission.await.unwrap();
        assert!(
            !suggestions.has_inflight_for_test(),
            "a refusal retained while result admission is waiting for state must prevent query spawn"
        );
    }

    #[tokio::test]
    async fn sdk_suggestion_follows_held_result_after_live_task_notification_is_consumed() {
        use super::super::{tests::fixture_runtime, NativeQueryResultPublisher};
        use tokio::io::AsyncBufReadExt;
        let root = tempfile::tempdir().unwrap();
        let (output, reader) = tokio::io::duplex(65536);
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let stream = Arc::new(StreamJsonStream::new_placeholder(
            crate::headless::io::Output::new(output),
        ));
        let runtime = fixture_runtime(stream.clone(), root.path()).await;
        let task = runtime
            .task_registry
            .create(
                tasks::TaskType::LocalAgent,
                tasks::task_trait::TaskSpawnInput::LocalAgent {
                    agent_id: lingxi_core::types::AgentId::new(),
                    subagent_type: "test".into(),
                    prompt: "test".into(),
                    is_backgrounded: true,
                    tool_use_id: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                    spawn_request: None,
                    inheritance: None,
                },
                "held result fixture".into(),
            )
            .await
            .unwrap();
        let owner = SdkPromptSuggestions::new(false);
        let mut publisher = NativeQueryResultPublisher {
            runtime: &runtime,
            stream: &stream,
            prompt_suggestions: owner.clone(),
            suggestion_turn_origin: None,
            input_status: None,
            result_hold: Default::default(),
            max_budget_usd: None,
            fast_mode_state: "off",
            fast_mode_disabled_reason: None,
            betas: &[],
            next_index: 0,
            last_error_published: false,
            last_result_failed: false,
        };
        publisher
            .publish(&Ok(orchestrator::TurnOutcome::EndTurn), 0)
            .await;
        assert!(
            stream.holds_print_results(),
            "the real pending background task holds the SDK result"
        );
        // Supply the completed isolated provider result at the same placement
        // seam used by the real worker; model generation is independently
        // exercised by orchestrator/sidequery fixture transports.
        let generation = {
            let mut state = owner.state.lock().unwrap();
            state.machine.enabled = true;
            let AfterResult::Generate(generation) =
                state.machine.after_result(true, false, false, false, None)
            else {
                panic!("generation");
            };
            generation
        };
        owner.place_generated(
            generation,
            lingxi_core::host::PromptSuggestionResult {
                suggestion: "run the tests".into(),
                generation_request_id: Some("request-fixture".into()),
            },
            &stream,
            "session-fixture",
            &CancellationToken::new(),
        );
        stream.flush().await.unwrap();
        // The real registry publishes task_started independently of the held
        // query result. That lifecycle frame must remain visible to the SDK.
        let started: Value = serde_json::from_str(
            &tokio::time::timeout(std::time::Duration::from_secs(1), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(started["type"], "system");
        assert_eq!(started["subtype"], "task_started");
        assert_eq!(started["task_id"], task);
        let escaped =
            tokio::time::timeout(std::time::Duration::from_millis(10), lines.next_line()).await;
        assert!(
            escaped.is_err(),
            "neither the result nor its suggestion escapes the hold: {escaped:?}"
        );
        runtime
            .task_registry
            .set_status(&task, tasks::TaskStatus::Completed)
            .await
            .unwrap();
        assert!(
            publisher.should_hold_results().await,
            "terminal completion is held until notification consumption"
        );
        runtime.task_registry.mark_notified(&task).await.unwrap();
        assert!(!publisher.should_hold_results().await);
        publisher.release_held_results(false).await;
        let result: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let suggestion: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(suggestion["type"], "prompt_suggestion");
        assert_eq!(suggestion["suggestion"], "run the tests");
        assert!(suggestion.get("generationRequestId").is_none());
        assert!(!stream.holds_print_results());
        stream.finish().await.unwrap();
        assert!(
            runtime
                .session_lifecycle
                .shutdown_and_drain()
                .await
                .complete
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sdk_suggestion_timeout_starts_only_when_input_settles() {
        let owner = SdkPromptSuggestions::new(true);
        let cancel = CancellationToken::new();
        let query_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            query_cancel.cancelled().await;
        });
        owner.state.lock().unwrap().inflight = Some((cancel.clone(), handle));
        tokio::time::advance(std::time::Duration::from_secs(90)).await;
        assert!(
            !cancel.is_cancelled(),
            "background speculation has no per-query 30-second timer"
        );
        let settling = tokio::spawn(async move {
            owner.settle().await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_millis(29_999)).await;
        assert!(!cancel.is_cancelled());
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        settling.await.unwrap();
        assert!(cancel.is_cancelled());
    }
}

pub(super) const FILE_SUGGESTION_LIMIT: usize = 15;

pub(super) const FILE_SUGGESTION_MAX_INDEXED_PATHS: usize = 20_000;

pub(super) const FILE_SUGGESTION_INDEX_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Per-process file-index cache used by the stream-json `file_suggestions`
/// control request. Claude Code starts the tracked-file refresh in the
/// background and searches whatever portion of the index is already ready, so
/// the first non-trivial query may legitimately return an empty list.
#[derive(Clone, Default)]
pub(super) struct StreamFileSuggestionIndex {
    pub(super) paths: Arc<tokio::sync::RwLock<Vec<String>>>,
    pub(super) root: Arc<tokio::sync::RwLock<Option<PathBuf>>>,
    pub(super) generation: Arc<std::sync::atomic::AtomicU64>,
    pub(super) refresh_started: Arc<std::sync::atomic::AtomicBool>,
}

impl StreamFileSuggestionIndex {
    pub(super) async fn suggestions(&self, cwd: &std::path::Path, query: &str) -> Vec<String> {
        self.prepare_root(cwd).await;
        if matches!(query, "" | "." | "./") {
            self.start_refresh(cwd.to_path_buf());
            return list_cwd_suggestions(cwd).await;
        }

        self.start_refresh(cwd.to_path_buf());
        let expanded_query = expand_home_query(query);
        if std::path::Path::new(&expanded_query).is_absolute() {
            return absolute_file_suggestions(query, &expanded_query).await;
        }
        fuzzy_file_suggestions(
            &self.paths.read().await,
            &expanded_query,
            FILE_SUGGESTION_LIMIT,
        )
    }

    pub(super) async fn prepare_root(&self, cwd: &std::path::Path) {
        use std::sync::atomic::Ordering;

        let mut root = self.root.write().await;
        if root.as_deref() == Some(cwd) {
            return;
        }
        *root = Some(cwd.to_path_buf());
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.refresh_started.store(false, Ordering::Release);
        self.paths.write().await.clear();
    }

    pub(super) fn start_refresh(&self, cwd: PathBuf) {
        use std::sync::atomic::Ordering;

        if self
            .refresh_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let paths = Arc::clone(&self.paths);
        let generation = Arc::clone(&self.generation);
        let refresh_generation = generation.load(Ordering::Acquire);
        tokio::spawn(async move {
            let indexed = build_file_suggestion_index(&cwd).await;
            if generation.load(Ordering::Acquire) == refresh_generation {
                *paths.write().await = indexed;
            }
        });
    }
}

pub(super) async fn list_cwd_suggestions(cwd: &std::path::Path) -> Vec<String> {
    let Ok(mut entries) = tokio::fs::read_dir(cwd).await else {
        return Vec::new();
    };
    let mut suggestions = Vec::new();
    while suggestions.len() < FILE_SUGGESTION_LIMIT {
        let Ok(Some(entry)) = entries.next_entry().await else {
            break;
        };
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            name.push(std::path::MAIN_SEPARATOR);
        }
        suggestions.push(name);
    }
    suggestions
}

pub(super) fn expand_home_query(query: &str) -> String {
    let Some(rest) = query.strip_prefix('~') else {
        return query.to_string();
    };
    let Some(home) = dirs::home_dir() else {
        return query.to_string();
    };
    format!("{}{}", home.to_string_lossy(), rest)
}

pub(super) async fn absolute_file_suggestions(
    original_query: &str,
    expanded_query: &str,
) -> Vec<String> {
    let path = std::path::Path::new(expanded_query);
    let has_trailing_separator = expanded_query
        .chars()
        .last()
        .is_some_and(|character| matches!(character, '/' | '\\'));
    let (directory, needle) = if has_trailing_separator {
        (path, "")
    } else {
        (
            path.parent().unwrap_or_else(|| std::path::Path::new(".")),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
    };

    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return Vec::new();
    };
    let mut kinds = std::collections::HashMap::new();
    while kinds.len() < FILE_SUGGESTION_MAX_INDEXED_PATHS {
        let Ok(Some(entry)) = entries.next_entry().await else {
            break;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let is_dir = entry.file_type().await.is_ok_and(|kind| kind.is_dir());
        kinds.insert(name, is_dir);
    }

    let mut names: Vec<String> = kinds.keys().cloned().collect();
    names.sort();
    let selected = if needle.is_empty() {
        names.into_iter().take(FILE_SUGGESTION_LIMIT).collect()
    } else {
        fuzzy_file_suggestions(&names, needle, FILE_SUGGESTION_LIMIT)
    };
    let home = dirs::home_dir();
    selected
        .into_iter()
        .map(|name| {
            let full = directory.join(&name);
            let mut rendered = render_absolute_suggestion(original_query, &full, home.as_deref());
            if kinds.get(&name).copied().unwrap_or(false) {
                rendered.push(std::path::MAIN_SEPARATOR);
            }
            rendered
        })
        .collect()
}

pub(super) fn render_absolute_suggestion(
    original_query: &str,
    full: &std::path::Path,
    home: Option<&std::path::Path>,
) -> String {
    if original_query.starts_with('~') {
        if let Some(relative) = home.and_then(|home| full.strip_prefix(home).ok()) {
            let mut rendered = String::from("~");
            if !relative.as_os_str().is_empty() {
                rendered.push(std::path::MAIN_SEPARATOR);
                rendered.push_str(&relative.to_string_lossy());
            }
            return rendered;
        }
    }
    full.to_string_lossy().into_owned()
}

pub(super) async fn build_file_suggestion_index(cwd: &std::path::Path) -> Vec<String> {
    let mut git = tokio::process::Command::new("git");
    git.args([
        "-c",
        "core.quotepath=false",
        "ls-files",
        "--recurse-submodules",
        "-z",
    ])
    .current_dir(cwd);
    let mut files = match collect_command_items(
        git,
        FILE_SUGGESTION_MAX_INDEXED_PATHS,
        FILE_SUGGESTION_INDEX_TIMEOUT,
    )
    .await
    {
        Some(files) => files,
        None => {
            let mut rg = tokio::process::Command::new("rg");
            rg.args([
                "--files", "--null", "--follow", "--hidden", "--glob", "!.git/", "--glob",
                "!.svn/", "--glob", "!.hg/", "--glob", "!.jj/",
            ])
            .current_dir(cwd);
            collect_command_items(
                rg,
                FILE_SUGGESTION_MAX_INDEXED_PATHS,
                FILE_SUGGESTION_INDEX_TIMEOUT,
            )
            .await
            .unwrap_or_default()
        }
    };

    let mut directories = std::collections::BTreeSet::new();
    for file in &files {
        if directories.len() >= FILE_SUGGESTION_MAX_INDEXED_PATHS {
            break;
        }
        let mut parent = std::path::Path::new(file).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() || path == std::path::Path::new(".") {
                break;
            }
            directories.insert(format!(
                "{}{}",
                path.to_string_lossy(),
                std::path::MAIN_SEPARATOR
            ));
            parent = path.parent();
        }
    }
    let mut indexed: Vec<String> = directories
        .into_iter()
        .take(FILE_SUGGESTION_MAX_INDEXED_PATHS)
        .collect();
    let remaining = FILE_SUGGESTION_MAX_INDEXED_PATHS.saturating_sub(indexed.len());
    indexed.extend(files.drain(..files.len().min(remaining)));
    indexed
}

pub(super) async fn collect_command_items(
    mut command: tokio::process::Command,
    limit: usize,
    timeout: std::time::Duration,
) -> Option<Vec<String>> {
    use tokio::io::AsyncBufReadExt as _;

    if limit == 0 {
        return Some(Vec::new());
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut items = Vec::new();
    let read = tokio::time::timeout(timeout, async {
        loop {
            let mut bytes = Vec::new();
            let read = reader.read_until(0, &mut bytes).await?;
            if read == 0 {
                return Ok::<bool, std::io::Error>(false);
            }
            if bytes.last() == Some(&0) {
                bytes.pop();
            }
            if bytes.is_empty() {
                continue;
            }
            items.push(String::from_utf8_lossy(&bytes).into_owned());
            if items.len() >= limit {
                return Ok(true);
            }
        }
    })
    .await;

    match read {
        Ok(Ok(hit_limit)) => {
            if hit_limit {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Some(items)
            } else {
                child
                    .wait()
                    .await
                    .ok()
                    .filter(std::process::ExitStatus::success)
                    .map(|_| items)
            }
        }
        Ok(Err(_)) | Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    }
}

pub(super) fn fuzzy_file_suggestions(paths: &[String], query: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    if query.is_empty() {
        let mut top_level = std::collections::BTreeSet::new();
        for path in paths {
            let component = path
                .split(['/', '\\'])
                .next()
                .filter(|part| !part.is_empty());
            if let Some(component) = component {
                top_level.insert(component.to_string());
            }
        }
        return top_level.into_iter().take(limit).collect();
    }

    let case_sensitive = query != query.to_lowercase();
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let needle_chars: Vec<char> = needle.chars().take(64).collect();
    if needle_chars.is_empty() {
        return Vec::new();
    }

    let mut ranked = Vec::new();
    for path in paths {
        let candidate = if case_sensitive {
            path.clone()
        } else {
            path.to_lowercase()
        };
        let chars: Vec<char> = candidate.chars().collect();
        let mut positions = Vec::with_capacity(needle_chars.len());
        let mut cursor = 0;
        let mut matched = true;
        for needle in &needle_chars {
            let Some(relative) = chars[cursor..].iter().position(|ch| ch == needle) else {
                matched = false;
                break;
            };
            let position = cursor + relative;
            positions.push(position);
            cursor = position + 1;
        }
        if !matched {
            continue;
        }

        let mut adjacency = 0_i64;
        let mut gap_cost = 0_i64;
        for pair in positions.windows(2) {
            let gap = pair[1].saturating_sub(pair[0] + 1);
            if gap == 0 {
                adjacency += 4;
            } else {
                gap_cost += 3 + gap as i64;
            }
        }
        let mut score = needle_chars.len() as i64 * 16 + adjacency - gap_cost;
        for (index, position) in positions.iter().enumerate() {
            if *position == 0 {
                if index == 0 {
                    score += 8;
                }
                continue;
            }
            let previous = chars[*position - 1];
            if matches!(previous, '/' | '\\' | '-' | '_' | '.' | ' ') {
                score += 8;
            } else if previous.is_ascii_lowercase() && chars[*position].is_ascii_uppercase() {
                score += 6;
            }
        }
        score += 32_i64.saturating_sub((chars.len() / 4) as i64);
        ranked.push((score, path));
    }
    ranked.sort_by(|(left_score, left_path), (right_score, right_path)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_path.cmp(right_path))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, path)| path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_suggestions_use_subsequence_ranking_and_limit() {
        let paths = vec![
            "src/main.rs".to_string(),
            "src/manager.rs".to_string(),
            "tests/main_test.rs".to_string(),
            "README.md".to_string(),
        ];
        let suggestions = fuzzy_file_suggestions(&paths, "smr", 2);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0], "src/main.rs");
        assert!(suggestions.iter().all(|path| path.contains(".rs")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_item_collection_stops_at_the_index_bound() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "printf 'a\\0b\\0c\\0'"]);
        assert_eq!(
            collect_command_items(command, 2, std::time::Duration::from_secs(1)).await,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_item_collection_times_out_and_reaps_the_child() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 1; printf 'late\\0'"]);
        assert_eq!(
            collect_command_items(command, 2, std::time::Duration::from_millis(20)).await,
            None
        );
    }

    #[tokio::test]
    async fn absolute_file_suggestions_use_the_filesystem_coordinate_space() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("alpha.txt"), "").expect("write alpha");
        std::fs::write(dir.path().join("beta.txt"), "").expect("write beta");
        std::fs::create_dir(dir.path().join("archive")).expect("create archive");

        let query = dir.path().join("al");
        let suggestions =
            absolute_file_suggestions(&query.to_string_lossy(), &query.to_string_lossy()).await;
        assert_eq!(
            suggestions,
            vec![dir.path().join("alpha.txt").to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn home_relative_suggestions_preserve_the_tilde_prefix() {
        let home = std::path::Path::new("/home/tester");
        assert_eq!(
            render_absolute_suggestion(
                "~/Doc",
                &home.join("Documents").join("notes.md"),
                Some(home)
            ),
            format!(
                "~{}Documents{}notes.md",
                std::path::MAIN_SEPARATOR,
                std::path::MAIN_SEPARATOR
            )
        );
    }

    #[tokio::test]
    async fn file_suggestion_cache_is_invalidated_when_session_cwd_changes() {
        use std::sync::atomic::Ordering;

        let first = tempfile::tempdir().expect("first tempdir");
        let second = tempfile::tempdir().expect("second tempdir");
        let index = StreamFileSuggestionIndex::default();

        index.prepare_root(first.path()).await;
        index.paths.write().await.push("first-only.rs".to_string());
        index.refresh_started.store(true, Ordering::Release);

        index.prepare_root(first.path()).await;
        assert_eq!(&*index.paths.read().await, &["first-only.rs".to_string()]);
        assert!(index.refresh_started.load(Ordering::Acquire));

        index.prepare_root(second.path()).await;
        assert!(index.paths.read().await.is_empty());
        assert!(!index.refresh_started.load(Ordering::Acquire));
    }
}
