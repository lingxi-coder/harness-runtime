//! Native background classification which precedes suggestion focus admission.
//! The host owns lifecycle and persistence; shipped JS owns classification,
//! Unicode truncation and secret redaction. Provider I/O stays in sidequery.
use super::*;
use serde_json::{Value, json};

#[path = "prompt_suggestion_background_state.rs"]
pub(super) mod state;
pub(crate) use state::BackgroundSuggestionContext;
use state::BackgroundSuggestionState;

#[path = "prompt_suggestion_background_js.rs"]
mod javascript;
#[path = "prompt_suggestion_background_model.rs"]
mod model_selection;
use javascript::evaluate;

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
            tokio::fs::read(path)
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
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
                    .and_then(|value| value.get("updatedAt"))
                    .and_then(Value::as_str)
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.timestamp_millis())
                    .unwrap_or(now);
                state.previous_state = saved
                    .as_ref()
                    .and_then(|value| value.get("state"))
                    .and_then(Value::as_str)
                    .unwrap_or("working")
                    .into();
                state.persisted = saved.clone();
            }
            if let Some(saved) = &saved {
                if let Some(saved_state) = saved.get("state").and_then(Value::as_str) {
                    let terminal = matches!(saved_state, "done" | "failed" | "stopped");
                    if terminal || saved_state != state.previous_state {
                        state.previous_state = if terminal {
                            String::new()
                        } else {
                            saved_state.into()
                        };
                        state.previous_state_since_ms = saved
                            .get("updatedAt")
                            .and_then(Value::as_str)
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
        let text = assembled
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tools = assembled
            .get("tools")
            .and_then(Value::as_str)
            .unwrap_or_default();
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
                    lingxi_core::types::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default();
        let prepared = evaluate(
            "prepare",
            json!({"text":text,"previous":previous,"latestAsk":latest_ask,"tools":tools,"minutes":mins}),
        )?;
        let no_text_result = if prepared.get("hasText").and_then(Value::as_bool) == Some(false)
            && count > start
        {
            let previous_state = saved
                .as_ref()
                .and_then(|value| value.get("state"))
                .and_then(Value::as_str)
                .unwrap_or(&previous);
            Some(
                json!({"state": previous_state, "tempo": if previous_state == "blocked" {"blocked"} else {"idle"}, "detail": saved.as_ref().and_then(|value| value.get("detail")).cloned().unwrap_or(json!("")), "needs": saved.as_ref().and_then(|value| value.get("needs")).cloned(), "output":{}}),
            )
        } else {
            None
        };
        let mut result = no_text_result.or_else(|| {
            prepared
                .get("result")
                .filter(|value| !value.is_null())
                .cloned()
        });
        if result.is_none() && heuristic_only {
            result = prepared.get("fallback").cloned();
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
                let base_prompt = prepared
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for attempt in 0..2 {
                    let prompt = if attempt == 0 {
                        base_prompt.to_owned()
                    } else {
                        format!(
                            "{base_prompt}\n\nPrevious response was not valid JSON. Respond with ONLY the JSON object, nothing else."
                        )
                    };
                    let request = sidequery::SideQueryRequest {
                        model_attempt: None,
                        model: query_model.clone(),
                        profile: query_profile.clone(),
                        system_prompt: None,
                        messages: vec![ConversationMessage::user(MessageId::new(), prompt)],
                        tools: Vec::new(),
                        tool_choice: None,
                        output_format: None,
                        max_tokens: if disable_thinking { 1024 } else { 3072 },
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
                    result = evaluate("parse", json!({"text":response_text,"previous":previous}))
                        .filter(|value| !value.is_null());
                    if result.is_some() {
                        break;
                    }
                }
            }
            if result.is_none() {
                result = prepared.get("fallback").cloned();
            }
        }
        if cancel.is_cancelled() {
            return None;
        }
        let mut result = result?;
        if result.get("state").and_then(Value::as_str) == Some("working")
            && result.get("tempo").and_then(Value::as_str) == Some("active")
        {
            result["tempo"] = json!("idle");
        }
        // Reload at placement boundary: a structured permission may have
        // superseded the classifier while its provider call was in flight.
        let current = if let Some(path) = self.background_suggestion_path(&session_id) {
            tokio::fs::read(path)
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        } else {
            None
        };
        let terminal = matches!(
            result.get("state").and_then(Value::as_str),
            Some("done" | "failed" | "stopped")
        );
        let preserve_block = !terminal
            && current.as_ref().is_some_and(|value| {
                value.get("tempo").and_then(Value::as_str) == Some("blocked")
                    && value.get("updatedAt")
                        != saved.as_ref().and_then(|value| value.get("updatedAt"))
            });
        let context_result = result.clone();
        if preserve_block {
            if let Some(current) = &current {
                result["tempo"] = json!("blocked");
                for key in ["needs", "block"] {
                    if let Some(value) = current.get(key) {
                        result[key] = value.clone();
                    }
                }
            }
        }
        result["updatedAt"] = json!(chrono::Utc::now().to_rfc3339());
        let context = {
            let mut state = self.background_suggestion_state();
            if state.session_id != session_id || state.epoch != epoch {
                return None;
            }
            let new_state = result
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("working");
            if state.previous_state != new_state {
                state.previous_state = new_state.into();
                state.previous_state_since_ms = now;
            }
            state.last_message_count = count;
            state.last_result = (context_result.get("tempo").and_then(Value::as_str)
                == Some("blocked")
                && result.get("block").is_none_or(Value::is_null))
            .then(|| BackgroundSuggestionContext {
                session_id: session_id.clone(),
                epoch,
                needs: context_result
                    .get("needs")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
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
        if evaluate("safeReply", json!({"text":text})).and_then(|value| value.as_bool())
            != Some(true)
        {
            return;
        }
        let path = self.background_suggestion_path(&context.session_id);
        let disk_state = if let Some(path) = &path {
            tokio::fs::read(path)
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
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
        value: &Value,
        expected: Option<&Value>,
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
        let Ok(bytes) = serde_json::to_vec(value) else {
            return false;
        };
        if tokio::fs::write(&temporary, bytes).await.is_err() {
            return false;
        }
        let published = {
            let state = self.background_suggestion_state();
            if state.epoch != epoch || state.session_id != session_id {
                false
            } else {
                let current = std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                current.as_ref() == expected && std::fs::rename(&temporary, &path).is_ok()
            }
        };
        if !published {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        published
    }
}
