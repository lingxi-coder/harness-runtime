//! Native background classification which precedes suggestion focus admission.
//! The host owns lifecycle and persistence; shipped JS owns classification,
//! Unicode truncation and secret redaction. Provider I/O stays in sidequery.
use super::*;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

#[path = "prompt_suggestion_background_state.rs"]
pub(super) mod state;
pub(crate) use state::BackgroundSuggestionContext;
use state::BackgroundSuggestionState;

#[path = "prompt_suggestion_background_js.rs"]
mod javascript;
#[path = "prompt_suggestion_background_model.rs"]
mod model_selection;
use javascript::evaluate;

fn null_projection() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

fn object_projection(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("classifier result fields form a JSON object")
}

fn projection_from_value(value: serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value)
        .expect("classifier control JSON is representable as a UTF-16 projection")
}

fn projection_member_or_default(
    value: Option<&Utf16JsonProjection>,
    pointer: &str,
    default: Utf16JsonProjection,
) -> Utf16JsonProjection {
    value
        .and_then(|value| value.subprojection(pointer).ok())
        .unwrap_or(default)
}

fn read_projection_bytes(bytes: &[u8]) -> Option<Utf16JsonProjection> {
    let json = std::str::from_utf8(bytes).ok()?;
    Utf16JsonProjection::parse(json).ok()
}

async fn read_projection_file(path: &std::path::Path) -> Option<Utf16JsonProjection> {
    let bytes = tokio::fs::read(path).await.ok()?;
    read_projection_bytes(&bytes)
}

fn read_projection_file_sync(path: &std::path::Path) -> Option<Utf16JsonProjection> {
    let bytes = std::fs::read(path).ok()?;
    read_projection_bytes(&bytes)
}

fn projection_options_match(
    left: Option<&Utf16JsonProjection>,
    right: Option<&Utf16JsonProjection>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.semantically_eq(right),
        _ => false,
    }
}

impl ConversationOrchestrator {
    fn background_suggestion_state(&self) -> std::sync::MutexGuard<'_, BackgroundSuggestionState> {
        self.prompt_runtime
            .suggestion_background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn reset_prompt_suggestion_background(&self) {
        self.background_suggestion_state().reset();
    }

    pub(crate) fn mark_prompt_suggestion_background_active(&self) {
        self.background_suggestion_state().mark_active();
    }

    fn background_suggestion_path(&self, session_id: &str) -> Option<std::path::PathBuf> {
        self.config_home.as_ref().map(|home| {
            home.join("prompt-suggestions")
                .join(format!("{session_id}.json"))
        })
    }

    /// Only a real classifier result may bypass background focus/throttling.
    /// In particular the input UI cannot supply a guessed `blocked` flag.
    pub(crate) async fn classify_prompt_suggestion_background(
        &self,
        cancel: &CancellationToken,
    ) -> Option<BackgroundSuggestionContext> {
        if std::env::var("CLAUDE_CODE_SESSION_KIND").ok().as_deref() != Some("bg") {
            return None;
        }
        let disabled_surfaces =
            telemetry::feature_flags::flag_value("tengu_classifier_disabled_surfaces", json!(""));
        if disabled_surfaces
            .as_str()
            .is_some_and(|value| value.split(',').any(|item| item.trim() == "bg"))
        {
            return None;
        }
        let heuristic_only = telemetry::feature_flags::flag_bool("tengu_cobalt_wren", false);
        let (session_id, history, model, profile) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                session.model_context_history(),
                session.model.clone(),
                session.model_profile.clone(),
            )
        };
        let saved = if let Some(path) = self.background_suggestion_path(&session_id) {
            read_projection_file(&path).await
        } else {
            None
        };
        let assistant: Vec<_> = history
            .iter()
            .filter(|message| matches!(message, ConversationMessage::Assistant { .. }))
            .collect();
        let count = assistant.len();
        let now = chrono::Utc::now().timestamp_millis();
        let (epoch, start, previous, mins) = {
            let mut state = self.background_suggestion_state();
            if state.session_id != session_id {
                state.reset();
                state.session_id = session_id.clone();
                state.previous_state_since_ms = saved
                    .as_ref()
                    .and_then(|value| value.string_at("/updatedAt"))
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.timestamp_millis())
                    .unwrap_or(now);
                state.previous_state = saved
                    .as_ref()
                    .and_then(|value| value.string_at("/state"))
                    .unwrap_or("working")
                    .into();
                state.persisted = saved.clone();
            }
            if let Some(saved) = &saved {
                if let Some(saved_state) = saved.string_at("/state") {
                    let terminal = matches!(saved_state, "done" | "failed" | "stopped");
                    if terminal || saved_state != state.previous_state {
                        state.previous_state = if terminal {
                            String::new()
                        } else {
                            saved_state.into()
                        };
                        state.previous_state_since_ms = saved
                            .string_at("/updatedAt")
                            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                            .map(|value| value.timestamp_millis())
                            .unwrap_or(now);
                    }
                }
            }
            (
                state.epoch,
                if state.last_message_count < count {
                    state.last_message_count
                } else {
                    0
                },
                if state.previous_state.is_empty() {
                    "working".into()
                } else {
                    state.previous_state.clone()
                },
                ((now - state.previous_state_since_ms) as f64 / 60000.0).round(),
            )
        };
        let messages = assistant.iter().map(|message| {
            let content = match message {ConversationMessage::Assistant {content,..} => content, _ => unreachable!()};
            json!({"type":"assistant","message":{"content":content},"isApiErrorMessage":self.is_prompt_suggestion_api_error(&message.id())})
        }).collect::<Vec<_>>();
        let assembled = evaluate("assemble", json!({"messages":messages,"lastCount":start}))?;
        let text = assembled.string_at("/text").unwrap_or_default();
        let tools = assembled.string_at("/tools").unwrap_or_default();
        let latest_ask = history
            .iter()
            .rev()
            .find_map(|message| match message {
                ConversationMessage::User {
                    content,
                    is_meta: false,
                    is_compact_summary: false,
                    ..
                } if content.len() == 1 => content.iter().find_map(|block| match block {
                    lingxi_core::types::ContentBlock::Text { text, .. } => {
                        Some(Utf16JsonProjection::from(text.clone()))
                    }
                    lingxi_core::types::ContentBlock::TextJsUtf16 {
                        utf16_code_units, ..
                    } => Utf16JsonProjection::root_string(
                        String::from_utf16_lossy(utf16_code_units),
                        utf16_code_units.clone(),
                    )
                    .ok(),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_else(|| Utf16JsonProjection::from(""));
        let prepared = javascript::evaluate_projection_result(
            "prepare",
            object_projection(vec![
                ("text".into(), Utf16JsonProjection::from(text)),
                (
                    "previous".into(),
                    Utf16JsonProjection::from(previous.as_str()),
                ),
                ("latestAsk".into(), latest_ask),
                ("tools".into(), Utf16JsonProjection::from(tools)),
                ("minutes".into(), projection_from_value(json!(mins))),
            ]),
        )
        .ok()?;
        let no_text_result = if prepared
            .at_pointer("/hasText")
            .and_then(|value| value.as_bool())
            == Some(false)
            && count > start
        {
            let previous_state = saved
                .as_ref()
                .and_then(|value| value.string_at("/state"))
                .unwrap_or(&previous)
                .to_owned();
            Some(object_projection(vec![
                (
                    "state".into(),
                    Utf16JsonProjection::from(previous_state.as_str()),
                ),
                (
                    "tempo".into(),
                    Utf16JsonProjection::from(if previous_state == "blocked" {
                        "blocked"
                    } else {
                        "idle"
                    }),
                ),
                (
                    "detail".into(),
                    projection_member_or_default(
                        saved.as_ref(),
                        "/detail",
                        Utf16JsonProjection::from(""),
                    ),
                ),
                (
                    "needs".into(),
                    projection_member_or_default(saved.as_ref(), "/needs", null_projection()),
                ),
                ("output".into(), projection_from_value(json!({}))),
            ]))
        } else {
            None
        };
        let mut result = no_text_result.or_else(|| {
            prepared
                .at_pointer("/result")
                .filter(|value| !value.is_null())
                .and_then(|_| prepared.subprojection("/result").ok())
        });
        if result.is_none() && heuristic_only {
            result = prepared.subprojection("/fallback").ok();
        }
        if result.is_none() {
            let config = telemetry::feature_flags::flag_value(
                "tengu_bg_classifier_config",
                json!({"useSmallFastModel":true,"disableThinking":true,"midTurnLlmDebounceMs":60000}),
            );
            let small = config
                .get("useSmallFastModel")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let disable_thinking = config
                .get("disableThinking")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (query_model, query_profile) = self
                .background_classifier_model(&model, profile.as_deref(), small)
                .await;
            let adaptive =
                llm_runtime::model::thinking::model_supports_adaptive_thinking(&query_model);
            let disable_thinking = disable_thinking && !adaptive;
            let mut system_blocks = vec![llm_runtime::SystemBlock {
                text: include_str!("prompt_suggestion_classifier_prompt.txt").into(),
                cache_control: Some(llm_runtime::CacheControl::Ephemeral),
            }];
            if telemetry::feature_flags::flag_bool("tengu_shimmering_firefly", false) {
                system_blocks.push(llm_runtime::SystemBlock {
                    text: include_str!("prompt_suggestion_classifier_leading_ask.txt").into(),
                    cache_control: Some(llm_runtime::CacheControl::Ephemeral),
                });
            }
            if let Some(runner) = &self.recap_runner {
                let base_prompt = prepared.string_units("/prompt").unwrap_or_default();
                for attempt in 0..2 {
                    let mut prompt_units = base_prompt.clone();
                    if attempt != 0 {
                        prompt_units.extend(
                            "\n\nPrevious response was not valid JSON. Respond with ONLY the JSON object, nothing else."
                                .encode_utf16(),
                        );
                    }
                    let mut message = ConversationMessage::user(
                        MessageId::new(),
                        String::from_utf16_lossy(&prompt_units),
                    );
                    if let ConversationMessage::User { content, .. } = &mut message {
                        content[0] = lingxi_core::types::ContentBlock::TextJsUtf16 {
                            text: String::from_utf16_lossy(&prompt_units),
                            utf16_code_units: prompt_units,
                            citations: None,
                        };
                    }
                    let request = sidequery::SideQueryRequest {
                        model_source: self
                            .main_model_source_for_request(&query_model, query_profile.as_deref())
                            .await,
                        model_attempt: None,
                        native_prompt_context: None,
                        model: query_model.clone(),
                        profile: query_profile.clone(),
                        system_prompt: None,
                        typed_attachment_ids: std::collections::HashSet::new(),
                        task_deliveries: std::collections::HashMap::new(),
                        system_prompt_input: None,
                        enable_prompt_caching: false,
                        messages: vec![message],
                        tools: Vec::new(),
                        tool_choice: None,
                        output_format: None,
                        max_tokens: Some(if disable_thinking { 1024 } else { 3072 }),
                        max_retries: 2,
                        temperature: None,
                        thinking: if disable_thinking {
                            Some(llm_runtime::model::thinking::ThinkingConfig::Disabled)
                        } else {
                            None
                        },
                        effort: None,
                        stop_sequences: Vec::new(),
                        query_source: sidequery::QuerySource::Custom("agent_classifier".into()),
                        skip_system_prompt_prefix: true,
                    };
                    let response = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return None,
                        response = runner.query_with_system_blocks(request, system_blocks.clone()) => response,
                    };
                    let Ok(response) = response else { break };
                    let Some(response_text) = response.text else {
                        continue;
                    };
                    result = javascript::evaluate_parse_projection(response_text, &previous)
                        .ok()
                        .filter(|value| value.at_pointer("").is_some_and(|root| !root.is_null()));
                    if result.is_some() {
                        break;
                    }
                }
            }
            if result.is_none() {
                result = prepared.subprojection("/fallback").ok();
            }
        }
        if cancel.is_cancelled() {
            return None;
        }
        let mut result = result?;
        if result.string_at("/state") == Some("working")
            && result.string_at("/tempo") == Some("active")
        {
            result
                .set_field("tempo", Utf16JsonProjection::from("idle"))
                .ok()?;
        }
        // Reload at placement boundary: a structured permission may have
        // superseded the classifier while its provider call was in flight.
        let current = if let Some(path) = self.background_suggestion_path(&session_id) {
            read_projection_file(&path).await
        } else {
            None
        };
        let terminal = matches!(
            result.string_at("/state"),
            Some("done" | "failed" | "stopped")
        );
        let preserve_block = !terminal
            && current.as_ref().is_some_and(|value| {
                value.string_at("/tempo") == Some("blocked")
                    && value.string_at("/updatedAt")
                        != saved
                            .as_ref()
                            .and_then(|value| value.string_at("/updatedAt"))
            });
        let context_result = result.clone();
        if preserve_block {
            if let Some(current) = &current {
                result
                    .set_field("tempo", Utf16JsonProjection::from("blocked"))
                    .ok()?;
                for key in ["needs", "block"] {
                    if current.get_property(key).is_some() {
                        let value = current.subprojection(&format!("/{key}")).ok()?;
                        result.set_field(key, value).ok()?;
                    }
                }
            }
        }
        result
            .set_field(
                "updatedAt",
                Utf16JsonProjection::from(chrono::Utc::now().to_rfc3339()),
            )
            .ok()?;
        let context = {
            let mut state = self.background_suggestion_state();
            if state.session_id != session_id || state.epoch != epoch {
                return None;
            }
            let new_state = result.string_at("/state").unwrap_or("working");
            if state.previous_state != new_state {
                state.previous_state = new_state.into();
                state.previous_state_since_ms = now;
            }
            state.last_message_count = count;
            state.last_result = BackgroundSuggestionContext::from_blocked_classifier(
                session_id.clone(),
                epoch,
                &context_result,
                &result,
            );
            state.persisted = Some(result.clone());
            state.eligible_context()
        };
        if !self
            .publish_background_suggestion_state(&session_id, epoch, &result, current.as_ref())
            .await
        {
            return None;
        }

        context
    }

    pub(crate) async fn record_background_suggested_reply(
        &self,
        text: &str,
        context: &BackgroundSuggestionContext,
    ) {
        if evaluate("safeReply", json!({"text":text}))
            .and_then(|value| value.at_pointer("").and_then(|root| root.as_bool()))
            != Some(true)
        {
            return;
        }
        let path = self.background_suggestion_path(&context.session_id);
        let disk_state = if let Some(path) = &path {
            read_projection_file(path).await
        } else {
            None
        };
        let persisted = {
            let mut state = self.background_suggestion_state();
            if state.epoch != context.epoch || state.session_id != context.session_id {
                return;
            }
            if let Some(disk_state) = &disk_state {
                state.persisted = Some(disk_state.clone());
            }
            if !state.record_reply(text, context.needs.as_deref()) {
                return;
            }
            state.persisted.clone()
        };
        if let Some(persisted) = persisted {
            self.publish_background_suggestion_state(
                &context.session_id,
                context.epoch,
                &persisted,
                disk_state.as_ref(),
            )
            .await;
        }
    }

    /// Stage outside the lifecycle lock, then validate the lease and publish
    /// with a single rename while reset/turn admission cannot change its epoch.
    async fn publish_background_suggestion_state(
        &self,
        session_id: &str,
        epoch: u64,
        value: &Utf16JsonProjection,
        expected: Option<&Utf16JsonProjection>,
    ) -> bool {
        let Some(path) = self.background_suggestion_path(session_id) else {
            return true;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        if tokio::fs::create_dir_all(parent).await.is_err() {
            return false;
        }
        let temporary = parent.join(format!(".{session_id}.{}.tmp", uuid::Uuid::new_v4()));
        let Ok(bytes) = value.to_json_string() else {
            return false;
        };
        if tokio::fs::write(&temporary, bytes.as_bytes())
            .await
            .is_err()
        {
            return false;
        }
        let published = {
            let state = self.background_suggestion_state();
            if state.epoch != epoch || state.session_id != session_id {
                false
            } else {
                let current = read_projection_file_sync(&path);
                projection_options_match(current.as_ref(), expected)
                    && std::fs::rename(&temporary, &path).is_ok()
            }
        };
        if !published {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        published
    }
}
