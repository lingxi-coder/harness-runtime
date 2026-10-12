//! Claude Code 2.1.296 post-turn suggestions and their session-owned lifecycle.
use super::*;
use lingxi_core::host::{PromptSuggestionContext, PromptSuggestionResult};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

#[path = "prompt_suggestion_state.rs"]
mod controls;
#[path = "prompt_suggestion_filter.rs"]
mod filter;
#[cfg(test)]
pub(super) use filter::parse_prompt_suggestion_response;

pub(super) struct SuggestionRuntime {
    context: PromptSuggestionContext,
    enabled: Option<bool>,
    presentation: controls::Presentation,
    uuid: Option<String>,
    unused: controls::UnusedState,
    fallback_streak: f64,
    turn_origin: Option<String>,
    api_error_ids: std::collections::HashSet<MessageId>,
    first_keystroke_at: Option<f64>,
    focused_when_shown: bool,
    first_start_initialized: bool,
    active_generation: Option<uuid::Uuid>,
    submission_gate: Arc<tokio::sync::Mutex<()>>,
}
impl Default for SuggestionRuntime {
    fn default() -> Self {
        Self {
            context: PromptSuggestionContext {
                session_id: String::new(),
                focus: "unknown".into(),
                input_empty: true,
                pending_permission: false,
                elicitation_active: false,
            },
            enabled: None,
            presentation: Default::default(),
            uuid: None,
            unused: Default::default(),
            fallback_streak: 0.0,
            turn_origin: None,
            api_error_ids: Default::default(),
            first_keystroke_at: None,
            focused_when_shown: true,
            first_start_initialized: false,
            active_generation: None,
            submission_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

struct SuggestionQuery {
    runner: Arc<sidequery::ForkedAgentRunner>,
    request: sidequery::ForkedAgentRequest,
    session_id: SessionId,
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

impl ConversationOrchestrator {
    pub(crate) const PROMPT_SUGGESTION_PROMPT: &str = include_str!("prompt_suggestion_prompt.txt");

    pub fn prompt_suggestion_environment_override(value: Option<&str>) -> Option<bool> {
        controls::parse_env(value)
    }

    fn suggestion_state(&self) -> std::sync::MutexGuard<'_, SuggestionRuntime> {
        self.prompt_runtime
            .suggestion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn cancel_prompt_suggestion(&self) {
        if let Some(cancel) = self
            .prompt_runtime
            .suggestion_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            cancel.cancel();
        }
    }

    pub(crate) fn reset_prompt_suggestion_session(&self) {
        self.cancel_prompt_suggestion();
        self.reset_prompt_suggestion_background();
        let mut state = self.suggestion_state();
        let enabled = state.enabled;
        let streak = state.fallback_streak;
        let first_start_initialized = state.first_start_initialized;
        let submission_gate = state.submission_gate.clone();
        *state = SuggestionRuntime::default();
        state.enabled = enabled;
        state.fallback_streak = streak;
        state.first_start_initialized = first_start_initialized;
        state.submission_gate = submission_gate;
    }

    pub(crate) fn mark_prompt_suggestion_api_error(&self, id: MessageId) {
        self.suggestion_state().api_error_ids.insert(id);
    }

    pub(crate) fn is_prompt_suggestion_api_error(&self, id: &MessageId) -> bool {
        self.suggestion_state().api_error_ids.contains(id)
    }

    pub(crate) fn restore_prompt_suggestion_api_errors(
        &self,
        ids: impl IntoIterator<Item = MessageId>,
    ) {
        self.suggestion_state().api_error_ids = ids.into_iter().collect();
    }

    pub(crate) async fn apply_prompt_suggestion_context(&self, context: PromptSuggestionContext) {
        let session = self.session.lock().await;
        if context.session_id != session.session_id.as_uuid().to_string() {
            return;
        }
        if !context.input_empty {
            self.cancel_prompt_suggestion();
        }
        {
            let mut state = self.suggestion_state();
            if !context.input_empty
                && state.presentation.is_shown()
                && state.first_keystroke_at.is_none()
            {
                state.first_keystroke_at = Some(now_ms());
            }
            state.context = context;
        }
        drop(session);
        self.initialize_prompt_suggestions().await;
    }

    pub(crate) async fn apply_prompt_suggestion_feedback(
        &self,
        session_id: &str,
        uuid: &str,
        action: &str,
    ) {
        let session = self.session.lock().await;
        if session_id != session.session_id.as_uuid().to_string() {
            return;
        }
        let mut state = self.suggestion_state();
        if state.uuid.as_deref() != Some(uuid) {
            return;
        }
        match action {
            "shown" => {
                if matches!(state.presentation, controls::Presentation::Generated { .. }) {
                    state.focused_when_shown = state.context.focus != "blurred";
                    state.first_keystroke_at = None;
                }
                state.presentation.mark_shown(now_ms());
            }
            "accepted" => state.presentation.mark_accepted(now_ms()),
            // Invalidating a surface is not an ignored submission.
            "dismissed" => {
                state.presentation = Default::default();
                state.uuid = None;
            }
            _ => {}
        }
    }

    pub(crate) async fn apply_prompt_suggestion_setting(&self, enabled: bool) {
        self.initialize_prompt_suggestions().await;
        let (streak, backing_off) = self.suggestion_backoff().await;
        let reset = {
            let mut state = self.suggestion_state();
            let was_enabled = state.enabled.unwrap_or(false);
            state.enabled = Some(enabled);
            if enabled && !was_enabled {
                state.unused.reenable(streak)
            } else {
                None
            }
        };
        // Native config toggles affect future generation; an in-flight query
        // or an already shown preview keeps its existing lifecycle.
        self.output
            .emit_prompt_suggestion_state(enabled, None)
            .await;
        if reset.is_some() {
            self.reset_suggestion_backoff(streak, backing_off, true)
                .await;
        }
    }

    pub fn latest_prompt_suggestion_turn_origin(&self) -> Option<String> {
        self.suggestion_state().turn_origin.clone()
    }

    pub(crate) async fn begin_prompt_suggestion_turn(
        &self,
        prompt: &str,
        human: bool,
        origin: Option<String>,
    ) {
        self.cancel_prompt_suggestion();
        self.mark_prompt_suggestion_background_active();
        self.suggestion_state().turn_origin = origin;
        if !human {
            return;
        }
        self.record_prompt_suggestion_submission_inner(prompt).await;
        // Local/local-JSX submissions are handled before model admission. Their
        // outcome is recorded through the host entry point, but they never claim
        // the native back-off notice. This driver is the admitted prompt lane.
        if !self.initialize_prompt_suggestions().await
            || matches!(
                prompt.trim_matches(js_whitespace),
                "exit" | "quit" | ":q" | ":q!" | ":wq" | ":wq!"
            )
            || (Self::suggestion_background_mode()
                && self.suggestion_state().context.focus != "focused")
        {
            return;
        }
        let (streak, backing_off) = self.suggestion_backoff().await;
        if let Some(claimed) = controls::claim_notice(streak, backing_off) {
            self.persist_suggestion_streak(claimed).await;
            self.suggestion_state().unused.ask_on_next_turn();
            self.record_suggestion_feature(
                "tengu_feature_ok",
                "prompt_suggestion_back_off_notice",
                None,
                json!({}),
            )
            .await;
            self.output
                .emit_prompt_suggestion_state(
                    true,
                    Some(&format!(
                        "{}{}",
                        controls::BACKOFF_NOTICE,
                        controls::BACKOFF_NOTICE_DIM
                    )),
                )
                .await;
        }
    }

    /// Called once at real composer submission, before local command dispatch.
    /// The driver may call it again: taking the shown presentation makes outcome
    /// accounting idempotent without relying on text equality or wall clocks.
    pub(crate) async fn record_prompt_suggestion_submission_inner(&self, prompt: &str) {
        self.cancel_prompt_suggestion();
        let gate = self.suggestion_state().submission_gate.clone();
        let _submission = gate.lock().await;
        let (session_id, outcome, first_keystroke, shown_at, was_focused) = {
            let session = self.session.lock().await;
            let mut state = self.suggestion_state();
            let shown_at = match &state.presentation {
                controls::Presentation::Shown { shown_at, .. }
                | controls::Presentation::Accepted { shown_at, .. } => *shown_at,
                _ => 0.0,
            };
            let outcome = state.presentation.submission(
                prompt.trim_end_matches(js_whitespace),
                now_ms(),
                false,
            );
            let first_keystroke = state.first_keystroke_at.take();
            let was_focused = state.focused_when_shown;
            state.presentation = Default::default();
            state.uuid = None;
            state.context.input_empty = true;
            (
                session.session_id,
                outcome,
                first_keystroke,
                shown_at,
                was_focused,
            )
        };
        let Some(outcome) = outcome else {
            return;
        };
        let (streak, backing_off) = self.suggestion_backoff().await;
        let write = {
            let session = self.session.lock().await;
            if session.session_id != session_id {
                return;
            }
            self.suggestion_state()
                .unused
                .record_submission(outcome.accepted, streak)
        };
        if let Some(value) = write {
            if outcome.accepted {
                self.reset_suggestion_backoff(streak, backing_off, false)
                    .await;
            } else {
                self.persist_suggestion_streak(value).await;
            }
        }
        let mut event = json!({"source":"cli", "outcome":if outcome.accepted {"accepted"} else {"ignored"},
            "prompt_id":"user_intent", "similarity":outcome.similarity, "wasFocusedWhenShown":was_focused});
        event[if outcome.accepted {
            "timeToAcceptMs"
        } else {
            "timeToIgnoreMs"
        }] = json!(outcome.elapsed_ms);
        if let Some(at) = first_keystroke {
            event["timeToFirstKeystrokeMs"] = json!(at - shown_at);
        }
        if let Some(method) = outcome.accept_method {
            event["acceptMethod"] = json!(method);
        }
        if let Some(id) = outcome.generation_request_id {
            event["generationRequestId"] = json!(id);
        }
        self.record_prompt_suggestion_event(event).await;
    }

    async fn initialize_prompt_suggestions(&self) -> bool {
        if let Some(enabled) = self.suggestion_state().enabled {
            return enabled;
        }
        let settings = self.suggestion_settings().await;
        let setting = settings
            .at_pointer("/promptSuggestionEnabled")
            .and_then(|value| value.as_bool());
        let env = Self::suggestion_env();
        let (enabled, initialization) = {
            let mut state = self.suggestion_state();
            if let Some(enabled) = state.enabled {
                (enabled, None)
            } else {
                let init = controls::initial_enablement(
                    env,
                    telemetry::feature_flags::flag_bool("tengu_chomp_inflection", false),
                    !self.config.interactive_session && state.context.session_id.is_empty(),
                    self.config.query_source.starts_with("agent:"),
                    setting,
                );
                state.enabled = Some(init.enabled);
                (init.enabled, Some(init))
            }
        };
        if let Some(init) = initialization {
            self.record_named_suggestion_event(
                "tengu_prompt_suggestion_init",
                json!({"enabled":init.enabled,"source":init.source}),
            )
            .await;
            self.output
                .emit_prompt_suggestion_state(enabled, None)
                .await;
        }
        enabled
    }

    async fn suggestion_settings(&self) -> Utf16JsonProjection {
        match &self.mod_settings_reader {
            Some(reader) => reader
                .read(Utf16JsonProjection::parse("{}").expect("empty settings request is JSON"))
                .await
                .unwrap_or_else(|_| {
                    Utf16JsonProjection::parse("{}").expect("empty settings fallback is JSON")
                }),
            None => Utf16JsonProjection::parse("{}").expect("empty settings are JSON"),
        }
    }

    fn suggestion_env() -> Option<bool> {
        controls::parse_env(
            std::env::var("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION")
                .ok()
                .as_deref(),
        )
    }

    fn suggestion_background_mode() -> bool {
        std::env::var("CLAUDE_CODE_SESSION_KIND").ok().as_deref() == Some("bg")
    }

    async fn reset_suggestion_backoff(
        &self,
        streak: f64,
        backing_off: bool,
        via_config_toggle: bool,
    ) {
        let confirmed = self.persist_suggestion_streak(0.0).await;
        if backing_off {
            self.record_suggestion_feature(if confirmed {"tengu_feature_ok"} else {"tengu_feature_sad"},
                "prompt_suggestion_back_off_end", (!confirmed).then_some("reset_unconfirmed"),
                json!({"was_notice_claimed":streak == controls::NOTICE_CLAIMED,"via_config_toggle":via_config_toggle})).await;
        }
    }

    async fn record_named_suggestion_event(&self, name: &str, metadata: Value) {
        let Some(bus) = &self.model_runtime.analytics_bus else {
            return;
        };
        let Ok(metadata) = serde_json::from_value::<telemetry::LogEventMetadata>(metadata) else {
            return;
        };
        bus.log_event(name, metadata).await;
    }

    async fn record_suggestion_feature(
        &self,
        name: &str,
        feature: &str,
        error: Option<&str>,
        mut metadata: Value,
    ) {
        metadata["feature_name"] = json!(feature);
        if let Some(error) = error {
            metadata["error_code"] = json!(error);
        }
        self.record_named_suggestion_event(name, metadata).await;
    }

    /// Shared native analytics schema. Text remains local; only lengths/timing/ids are recorded.
    pub async fn record_prompt_suggestion_event(&self, metadata: Value) {
        self.record_named_suggestion_event("tengu_prompt_suggestion", metadata)
            .await;
    }

    async fn suppress_suggestion(&self, source: &str, reason: &str, cache_cold_by: Option<&str>) {
        let mut event = json!({"source":source,"outcome":"suppressed","reason":reason,"prompt_id":"user_intent"});
        if let Some(by) = cache_cold_by {
            event["cacheColdBy"] = json!(by);
        }
        self.record_prompt_suggestion_event(event).await;
    }

    fn suggestion_global_config_path(&self) -> Option<std::path::PathBuf> {
        let home = self.config_home.as_ref()?;
        if migrations::global_config::lingxi_config_home().as_ref() == Some(home) {
            migrations::global_config::global_config_path()
        } else {
            // A custom/mobile config home must never consult the desktop user's ambient config.
            Some(home.join(branding::LEGACY_GLOBAL_CONFIG_FILE))
        }
    }

    async fn suggestion_backoff(&self) -> (f64, bool) {
        let path = self.suggestion_global_config_path();
        if let Some(path) = path
            .clone()
            .filter(|_| !self.suggestion_state().first_start_initialized)
        {
            let result = tokio::task::spawn_blocking(move || {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                migrations::global_config::ensure_first_start_metadata(
                    &path,
                    &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    lingxi_core::host::CLAUDE_CODE_VERSION,
                )
                .map_err(|error| std::io::Error::other(error.to_string()))
            })
            .await;
            if matches!(result, Ok(Ok(_))) {
                self.suggestion_state().first_start_initialized = true;
            }
        }
        let config = tokio::task::spawn_blocking(move || {
            path.and_then(|p| migrations::global_config::read_map(&p).ok())
        })
        .await
        .ok()
        .flatten();
        let streak = match &config {
            Some(map) => map
                .get("promptSuggestionUnusedStreak")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            None => self.suggestion_state().fallback_streak,
        };
        let first_start = config
            .as_ref()
            .and_then(|m| m.get("firstStartTime"))
            .and_then(Value::as_str)
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|d| d.timestamp_millis() as f64)
                    .or_else(|_| {
                        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| {
                            d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis() as f64
                        })
                    })
                    .unwrap_or(f64::NAN)
            });
        let env = controls::parse_env(
            std::env::var("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION")
                .ok()
                .as_deref(),
        );
        (
            streak,
            controls::BackoffInputs {
                persisted_streak: streak,
                now_ms: now_ms(),
                first_start_ms: first_start,
                env,
            }
            .backing_off(),
        )
    }

    async fn persist_suggestion_streak(&self, value: f64) -> bool {
        self.suggestion_state().fallback_streak = value;
        let Some(path) = self.suggestion_global_config_path() else {
            return true;
        };
        let result = tokio::task::spawn_blocking(move || {
            migrations::global_config::save_map(&path, |mut map| {
                let old = map
                    .get("promptSuggestionUnusedStreak")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                // The native threshold/notice writes recheck the shared config inside the update.
                if value == 0.0
                    || (value == controls::UNUSED_THRESHOLD && old < controls::UNUSED_THRESHOLD)
                    || (value == controls::NOTICE_CLAIMED
                        && old >= controls::UNUSED_THRESHOLD
                        && old != controls::NOTICE_CLAIMED)
                {
                    map.insert("promptSuggestionUnusedStreak".into(), json!(value as i64));
                }
                map
            })
        })
        .await;
        if !matches!(result, Ok(Ok(_))) {
            tracing::debug!("could not persist prompt suggestion back-off");
        }
        matches!(result, Ok(Ok(_)))
    }

    async fn prepare_prompt_suggestion(
        &self,
        sdk: bool,
        bypass_throttle: bool,
        cancel: &CancellationToken,
    ) -> Option<SuggestionQuery> {
        let source = if sdk { "sdk" } else { "cli" };
        let env = Self::suggestion_env();
        let enabled = if sdk {
            self.initialize_prompt_suggestions().await;
            controls::explicit_enablement(
                env,
                self.suggestion_settings()
                    .await
                    .get("promptSuggestionEnabled")
                    .and_then(Value::as_bool),
            )
        } else {
            self.initialize_prompt_suggestions().await
        };
        let (session_id, history) = {
            let session = self.session.lock().await;
            (session.session_id, session.model_context_history())
        };
        let context = self.suggestion_state().context.clone();
        let pending_permission =
            context.pending_permission || self.perms.has_pending_request().await;
        let elicitation_active = context.elicitation_active
            || if let Some(registry) = &self.mcp_registry {
                registry.has_pending_elicitation().await
            } else {
                false
            };
        let limit = self.api.last_rate_limit_full();
        let gates =
            controls::GenerationGates {
                aborted: cancel.is_cancelled(),
                assistant_count: history
                    .iter()
                    .filter(|m| matches!(m, ConversationMessage::Assistant { .. }))
                    .count(),
                last_response_api_error: history
                    .iter()
                    .rev()
                    .find(|m| matches!(m, ConversationMessage::Assistant { .. }))
                    .is_some_and(|m| self.suggestion_state().api_error_ids.contains(&m.id())),
                enabled,
                pending_permission,
                elicitation_active,
                plan_mode: self.permission_mode().as_deref() == Some("plan"),
                rate_limit_status: limit
                    .as_ref()
                    .and_then(|l| l.status.as_deref())
                    .unwrap_or("allowed"),
                env,
                usage: self.snapshot_cost_real().await.current_usage.map(|u| {
                    controls::CacheUsage {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                    }
                }),
                cache_thaw: telemetry::feature_flags::flag_bool("tengu_chomp_thaw", false),
            };
        if let Some(reason) = gates.suppression() {
            self.suppress_suggestion(source, reason.reason(), reason.cache_cold_by())
                .await;
            return None;
        }
        if !sdk && !bypass_throttle {
            let (_, backing_off) = self.suggestion_backoff().await;
            let eligible = self.suggestion_state().unused.eligible(backing_off);
            if !eligible {
                self.suppress_suggestion(source, "unused_streak", None)
                    .await;
                return None;
            }
        }
        let runner = self.recap_runner.clone()?;
        let mut params = self
            .model_runtime
            .cache_safe_slot
            .as_ref()?
            .get_last()
            .await?;
        extend_session_memory_fork_context(&mut params.fork_context_messages, &history);
        Some(SuggestionQuery {
            runner,
            session_id,
            request: sidequery::ForkedAgentRequest {
                prompt_messages: vec![ConversationMessage::user(
                    MessageId::new(),
                    Self::PROMPT_SUGGESTION_PROMPT.into(),
                )],
                cache_safe_params: params,
                fork_label: "prompt_suggestion".into(),
                query_source: sidequery::QuerySource::PromptSuggestion,
                max_output_tokens: None,
            },
        })
    }

    async fn suggestion_result(
        &self,
        reply: sidequery::ForkedAgentResult,
        source: &str,
    ) -> Option<PromptSuggestionResult> {
        self.record_suggestion_feature(
            "tengu_feature_ok",
            "prompt_suggestion_generate",
            None,
            json!({}),
        )
        .await;
        let suggestion = reply
            .candidates
            .iter()
            .map(|s| filter::normalize_prompt_suggestion_response(s))
            .find(|s| !s.is_empty());
        let Some(suggestion) = suggestion else {
            self.suppress_suggestion(source, "empty", None).await;
            return None;
        };
        if let Some(reason) = filter::prompt_suggestion_suppression_reason(&suggestion) {
            self.suppress_suggestion(source, reason, None).await;
            return None;
        }
        Some(PromptSuggestionResult {
            suggestion,
            generation_request_id: reply.generation_request_id,
        })
    }

    /// The SDK opts in explicitly and never participates in the CLI unused streak.
    pub async fn generate_prompt_suggestion_query(
        &self,
        cancel: CancellationToken,
    ) -> Result<Option<PromptSuggestionResult>, lingxi_core::host::HandleError> {
        if self.model_runtime.cache_safe_slot.as_ref().is_none()
            || match &self.model_runtime.cache_safe_slot {
                Some(slot) => slot.get_last().await.is_none(),
                None => true,
            }
        {
            self.suppress_suggestion("sdk", "sdk_no_params", None).await;
            return Ok(None);
        }
        let Some(query) = self.prepare_prompt_suggestion(true, false, &cancel).await else {
            return Ok(None);
        };
        let observer = self.model_safety_observer().await;
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                self.suppress_suggestion("sdk", "aborted", None).await;
                Ok(None)
            },
            result = lingxi_core::host::model_safety::bind_model_safety(observer, query.runner.run(query.request)) => {
                let reply = result.map_err(|e| lingxi_core::host::HandleError::ActionFailed(e.to_string()))?;
                if cancel.is_cancelled() {
                    self.suppress_suggestion("sdk", "aborted", None).await;
                    return Ok(None);
                }
                Ok(self.suggestion_result(reply, "sdk").await)
            }
        }
    }

    pub(super) async fn spawn_prompt_suggestion(&self) {
        if !self.output.wants_prompt_suggestions() {
            return;
        }
        let attached = !self.suggestion_state().context.session_id.is_empty();
        let main_thread = self.config.query_source.starts_with("repl_main_thread");
        let composer =
            self.config.query_source == "sdk" && (self.config.interactive_session || attached);
        if !(main_thread || composer)
            || self.config.bare
            || controls::parse_env(std::env::var("CLAUDE_CODE_SIMPLE").ok().as_deref())
                == Some(true)
            || controls::parse_env(std::env::var(branding::SIMPLE_ENV).ok().as_deref())
                == Some(true)
            || self
                .suggestion_settings()
                .await
                .get("disableAllHooks")
                .and_then(Value::as_bool)
                == Some(true)
        {
            return;
        }
        if controls::parse_env(
            std::env::var("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION")
                .ok()
                .as_deref(),
        ) == Some(false)
        {
            return;
        }
        self.cancel_prompt_suggestion();
        let cancel = CancellationToken::new();
        let generation = uuid::Uuid::new_v4();
        self.suggestion_state().active_generation = Some(generation);
        *self
            .prompt_runtime
            .suggestion_cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel.clone());
        let background = self.classify_prompt_suggestion_background(&cancel).await;
        let focus = self.suggestion_state().context.focus.clone();
        if let Some(reason) = controls::cli_focus_suppression(
            &focus,
            Self::suggestion_background_mode(),
            background.is_some(),
        ) {
            self.suppress_suggestion("cli", reason, None).await;
            self.cancel_prompt_suggestion();
            return;
        }
        let Some(query) = self
            .prepare_prompt_suggestion(false, background.is_some(), &cancel)
            .await
        else {
            self.cancel_prompt_suggestion();
            return;
        };
        let Some(owner) = self.upgrade_streaming_tool_dispatch_owner() else {
            self.cancel_prompt_suggestion();
            return;
        };
        let observer = self.model_safety_observer().await;
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => { owner.suppress_suggestion("cli", "aborted", None).await; }
                result = lingxi_core::host::model_safety::bind_model_safety(observer, query.runner.run(query.request)) => {
                    match result {
                        Ok(reply) => {
                            if cancel.is_cancelled() { owner.suppress_suggestion("cli", "aborted", None).await; }
                            else if let Some(result) = owner.suggestion_result(reply, "cli").await {
                                owner.place_prompt_suggestion(result, query.session_id, &cancel, background).await;
                            }
                        }
                        Err(_) => { owner.record_suggestion_feature("tengu_feature_bad", "prompt_suggestion_generate", Some("api_error"), json!({})).await; }
                    }
                }
            }
            let mut state = owner.suggestion_state();
            if state.active_generation == Some(generation) {
                state.active_generation = None;
                owner
                    .prompt_runtime
                    .suggestion_cancel
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
            }
        });
    }

    async fn place_prompt_suggestion(
        &self,
        result: PromptSuggestionResult,
        session_id: SessionId,
        cancel: &CancellationToken,
        background: Option<super::prompt_suggestion_background::BackgroundSuggestionContext>,
    ) {
        let generation_request_id = result.generation_request_id.clone();
        let input = Utf16JsonProjection::from_serde_value(&json!({
            "text": result.suggestion,
            "origin": {"kind": "suggestion"}
        }))
        .expect("generated prompt suggestion input is JSON");
        let placed = std::sync::atomic::AtomicBool::new(false);
        let place = |input: Option<Utf16JsonProjection>| {
            placed.store(true, std::sync::atomic::Ordering::Relaxed);
            let generation_request_id = generation_request_id.clone();
            let background = background.clone();
            async move {
                let input = input.ok_or_else(|| {
                    hooks::mods::ModError::Hook("prompt.suggest requires an event payload".into())
                })?;
                let text_units = input.string_units("/text").ok_or_else(|| {
                    hooks::mods::ModError::Hook("prompt.suggest text must be a string".into())
                })?;
                let text = String::from_utf16(&text_units).map_err(|_| {
                    hooks::mods::ModError::Hook(
                        "prompt.suggest text must contain valid Unicode scalar values".into(),
                    )
                })?;
                // Delivery and turn admission share one owner: a delayed output
                // send cannot overtake TurnStarted or a committed session swap.
                let _turn = tokio::select! { biased;
                    () = cancel.cancelled() => {
                        return Utf16JsonProjection::from_serde_value(&json!({"isShown":false}))
                            .map(Some)
                            .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()));
                    },
                    guard = self.turn_gate.lock() => guard,
                };
                let session = self.session.lock().await;
                let current = session.session_id == session_id;
                drop(session);
                let uuid = uuid::Uuid::new_v4().to_string();
                let shown = {
                    let mut state = self.suggestion_state();
                    if !current
                        || cancel.is_cancelled()
                        || !state.context.input_empty
                        || text.trim_matches(js_whitespace).is_empty()
                    {
                        false
                    } else {
                        state.uuid = Some(uuid.clone());
                        state.first_keystroke_at = None;
                        state.presentation = controls::Presentation::Generated {
                            text: text.clone(),
                            generation_request_id,
                        };
                        true
                    }
                };
                if shown {
                    if let Some(context) = &background {
                        self.record_background_suggested_reply(&text, context).await;
                    }
                    self.output
                        .emit_prompt_suggestion(&session_id.as_uuid().to_string(), &uuid, &text)
                        .await;
                }
                Utf16JsonProjection::from_serde_value(&json!({"isShown":shown}))
                    .map(Some)
                    .map_err(|error| hooks::mods::ModError::Protocol(error.to_string()))
            }
        };
        let host = if let Some(registry) = &self.lifecycle_runtime.hook_registry {
            registry.read().await.mod_host()
        } else {
            None
        };
        let outcome = if let Some(host) = host {
            let output = self.output.clone();
            tokio::select! { biased;
                () = cancel.cancelled() => {
                    self.suppress_suggestion("cli", "aborted", None).await;
                    return;
                },
                result = host.dispatch_with_log_at_session("prompt.suggest", input, self, place, move |plugin,text| {
                    let output = output.clone(); async move { output.emit_mod_log(&plugin,&text).await }
                }) => result,
            }
        } else {
            place(Some(input)).await.and_then(|result| {
                result.ok_or_else(|| {
                    hooks::mods::ModError::Hook(
                        "prompt.suggest did not produce a placement result".into(),
                    )
                })
            })
        };
        if cancel.is_cancelled() {
            self.suppress_suggestion("cli", "aborted", None).await;
            return;
        }
        if let Ok(outcome) = outcome {
            if !outcome
                .at_pointer("/isShown")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                && !cancel.is_cancelled()
                && placed.load(std::sync::atomic::Ordering::Relaxed)
            {
                self.suppress_suggestion("cli", "timing", None).await;
            }
        } else {
            self.record_suggestion_feature(
                "tengu_feature_bad",
                "prompt_suggestion_generate",
                Some("api_error"),
                json!({}),
            )
            .await;
        }
    }
}
