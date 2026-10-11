//! Claude Code 2.1.296 `HS` SDK suggestion state. The wire frame is deliberately
//! separate from its private generation request ID and acceptance timestamp.
use serde_json::{json, Map, Value};

pub(crate) const SDK_SUGGESTION_SETTLE_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug)]
struct EmittedSuggestion {
    text: String,
    generation_request_id: Option<String>,
    emitted_at: u64,
}

#[derive(Default, Debug)]
pub(crate) struct SdkPromptSuggestionState {
    pub(crate) enabled: bool,
    pub(crate) paused: bool,
    initialized: bool,
    generation: u64,
    last_emitted: Option<EmittedSuggestion>,
    pending: Option<(Value, EmittedSuggestion)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AfterResult {
    Disabled,
    Suppressed(&'static str),
    Generate(u64),
}

impl SdkPromptSuggestionState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    /// The first initialize may set either value. Reinitialize may only opt a
    /// remote-cowork worker in; ordinary reattach cannot change its preference.
    pub(crate) fn initialize(&mut self, requested: Option<bool>, remote_cowork: bool) {
        if self.initialized {
            if requested == Some(true) && remote_cowork {
                self.enabled = true;
            }
        } else {
            self.initialized = true;
            if let Some(enabled) = requested {
                self.enabled = enabled;
            }
        }
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        // Pausing affects the next result, and never aborts an in-flight query.
        self.paused = paused;
    }

    pub(crate) fn input_started(&mut self) {
        self.initialized = true;
    }

    pub(crate) fn after_result(
        &mut self,
        should_query: bool,
        shutting_down: bool,
        environment_disabled: bool,
        sable: bool,
        turn_origin: Option<&str>,
    ) -> AfterResult {
        if !self.enabled || !should_query || shutting_down || environment_disabled {
            return AfterResult::Disabled;
        }
        self.generation = self.generation.wrapping_add(1);
        if let Some(reason) = host_suppression(self.paused, turn_origin, sable) {
            return AfterResult::Suppressed(reason);
        }
        AfterResult::Generate(self.generation)
    }

    pub(crate) fn on_command(
        &mut self,
        should_query: bool,
        prompt_content: Option<&Value>,
        now: u64,
    ) -> Option<Value> {
        if !should_query {
            return None;
        }
        self.abort();
        self.pending = None;
        // A non-prompt command cancels generation but retains attribution for
        // the next prompt command, exactly like HS.onCommand.
        let content = prompt_content?;
        let last = self.last_emitted.take()?;
        let text = match content {
            Value::String(text) => Some(text.as_str()),
            Value::Array(blocks) => blocks
                .iter()
                .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str),
            _ => None,
        }?;
        Some(feedback(&last, text, now))
    }

    pub(crate) fn generated(
        &mut self,
        generation: u64,
        suggestion: String,
        generation_request_id: Option<String>,
        uuid: &str,
        session_id: &str,
        now: u64,
        holds_results: bool,
    ) -> Option<Value> {
        if generation != self.generation {
            return None;
        }
        let mut frame = Map::new();
        frame.insert("type".into(), json!("prompt_suggestion"));
        frame.insert("suggestion".into(), json!(suggestion));
        frame.insert("uuid".into(), json!(uuid));
        frame.insert("session_id".into(), json!(session_id));
        let entry = EmittedSuggestion {
            text: suggestion,
            generation_request_id,
            emitted_at: now,
        };
        if holds_results {
            self.pending = Some((Value::Object(frame), entry));
            None
        } else {
            self.last_emitted = Some(entry);
            Some(Value::Object(frame))
        }
    }

    pub(crate) fn emit_pending(&mut self, now: u64) -> Option<Value> {
        let (frame, mut entry) = self.pending.take()?;
        entry.emitted_at = now;
        self.last_emitted = Some(entry);
        Some(frame)
    }

    pub(crate) fn abort(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    #[cfg(test)]
    fn generation_is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    pub(crate) fn discard(&mut self) {
        self.abort();
        self.last_emitted = None;
        self.pending = None;
    }
}

pub(crate) fn host_suppression(
    paused: bool,
    turn_origin: Option<&str>,
    sable: bool,
) -> Option<&'static str> {
    if !sable {
        return None;
    }
    if paused {
        Some("host_paused")
    } else if matches!(
        turn_origin,
        Some("scheduled" | "task_notification" | "auto_continuation" | "host_synthetic" | "system")
    ) {
        Some("not_user_started")
    } else {
        None
    }
}

fn feedback(last: &EmittedSuggestion, input: &str, now: u64) -> Value {
    let accepted = input.to_lowercase() == last.text.to_lowercase();
    let denominator = last.text.encode_utf16().count().max(1) as f64;
    let similarity = (input.encode_utf16().count() as f64 / denominator * 100.0).round() / 100.0;
    let mut payload = Map::new();
    payload.insert("source".into(), json!("sdk"));
    payload.insert(
        "outcome".into(),
        json!(if accepted { "accepted" } else { "ignored" }),
    );
    payload.insert("prompt_id".into(), json!("user_intent"));
    if let Some(request_id) = last
        .generation_request_id
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        payload.insert("generationRequestId".into(), json!(request_id));
    }
    payload.insert(
        if accepted {
            "timeToAcceptMs"
        } else {
            "timeToIgnoreMs"
        }
        .into(),
        json!(now.saturating_sub(last.emitted_at)),
    );
    payload.insert(
        "similarity".into(),
        if similarity.fract() == 0.0 {
            json!(similarity as u64)
        } else {
            json!(similarity)
        },
    );
    Value::Object(payload)
}

/// Exact first Zod issue used by the native control handler.
pub(crate) fn parse_paused(value: Option<&Value>) -> Result<bool, String> {
    if let Some(Value::Bool(paused)) = value {
        return Ok(*paused);
    }
    let received = match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::String(_)) => "string",
        Some(Value::Number(_)) => "number",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
        Some(Value::Bool(_)) => unreachable!(),
    };
    Err(format!("Invalid set_prompt_suggestions_paused: Invalid input: expected boolean, received {received}"))
}

#[cfg(test)]
#[path = "sdk_prompt_suggestions_tests.rs"]
mod tests;
