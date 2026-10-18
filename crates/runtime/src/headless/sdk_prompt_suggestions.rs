//! SDK suggestion state. The wire frame is deliberately
//! separate from its private generation request ID and acceptance timestamp.
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use serde_json::{json, Map, Value};

pub(crate) const SDK_SUGGESTION_SETTLE_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug)]
struct EmittedSuggestion {
    text: Utf16JsonProjection,
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
    pending: Option<(Utf16JsonProjection, EmittedSuggestion)>,
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
        suggestion: Utf16JsonProjection,
        generation_request_id: Option<String>,
        uuid: &str,
        session_id: &str,
        now: u64,
        holds_results: bool,
    ) -> Option<Utf16JsonProjection> {
        if generation != self.generation {
            return None;
        }
        assert!(
            suggestion.string_units("").is_some(),
            "prompt suggestion must be a root JSON string"
        );
        let frame = Utf16JsonProjection::object(vec![
            (
                "type".into(),
                Utf16JsonProjection::from("prompt_suggestion"),
            ),
            ("suggestion".into(), suggestion.clone()),
            ("uuid".into(), Utf16JsonProjection::from(uuid)),
            ("session_id".into(), Utf16JsonProjection::from(session_id)),
        ])
        .expect("prompt suggestion frame is an object");
        let entry = EmittedSuggestion {
            text: suggestion,
            generation_request_id,
            emitted_at: now,
        };
        if holds_results {
            self.pending = Some((frame, entry));
            None
        } else {
            self.last_emitted = Some(entry);
            Some(frame)
        }
    }

    pub(crate) fn emit_pending(&mut self, now: u64) -> Option<Utf16JsonProjection> {
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
    let last_units = last
        .text
        .string_units("")
        .expect("stored suggestion is a root JSON string");
    let input_units = input.encode_utf16().collect::<Vec<_>>();
    let accepted = lowercase_utf16(&input_units) == lowercase_utf16(&last_units);
    let denominator = last_units.len().max(1) as f64;
    let similarity = (input_units.len() as f64 / denominator * 100.0).round() / 100.0;
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

fn lowercase_utf16(units: &[u16]) -> Vec<u16> {
    // Keep Rust's existing Unicode case mapping for every well-formed run,
    // while copying an isolated surrogate through unchanged as a JavaScript
    // string unit. This lets feedback state retain the same source identity as
    // the suggestion frame without converting the carrier through display.
    let mut output = Vec::with_capacity(units.len());
    let mut run = String::new();
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok(character) => run.push(character),
            Err(error) => {
                output.extend(run.to_lowercase().encode_utf16());
                run.clear();
                output.push(error.unpaired_surrogate());
            }
        }
    }
    output.extend(run.to_lowercase().encode_utf16());
    output
}

/// Exact first Zod issue used by the native control handler.
pub(crate) fn parse_paused(value: Option<JsonValueView<'_>>) -> Result<bool, String> {
    if let Some(paused) = value.and_then(JsonValueView::as_bool) {
        return Ok(paused);
    }
    let received = match value {
        None => "undefined",
        Some(value) if value.is_null() => "null",
        Some(value) if value.code_units().is_some() => "string",
        Some(value) if value.as_number().is_some() => "number",
        Some(value) if value.array_len().is_some() => "array",
        Some(value) if value.object_entries().is_some() => "object",
        Some(_) => unreachable!("boolean values returned above"),
    };
    Err(format!(
        "Invalid set_prompt_suggestions_paused: Invalid input: expected boolean, received {received}"
    ))
}

#[cfg(test)]
#[path = "sdk_prompt_suggestions_tests.rs"]
mod tests;
