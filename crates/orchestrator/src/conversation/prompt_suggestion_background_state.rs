//! Suggestion-facing subset of Native's classifier job state. The classifier
//! owns this state; composer context never supplies a guessed blocked flag.
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub(crate) struct BackgroundSuggestionState {
    pub session_id: String,
    pub epoch: u64,
    pub last_message_count: usize,
    pub previous_state: String,
    pub previous_state_since_ms: i64,
    pub last_result: Option<BackgroundSuggestionContext>,
    pub persisted: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BackgroundSuggestionContext {
    pub session_id: String,
    pub epoch: u64,
    pub needs: Option<String>,
}

impl BackgroundSuggestionState {
    pub fn reset(&mut self) {
        let epoch = self.epoch.wrapping_add(1);
        *self = Self {
            epoch,
            ..Self::default()
        };
    }

    pub fn mark_active(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.last_result = None;
        if let Some(state) = self.persisted.as_mut().and_then(Value::as_object_mut) {
            state.insert("tempo".into(), Value::String("active".into()));
            state.remove("needs");
            state.remove("block");
            state.remove("suggestedReply");
        }
    }

    pub fn eligible_context(&self) -> Option<BackgroundSuggestionContext> {
        self.last_result.clone()
    }

    pub fn record_reply(&mut self, text: &str, needs: Option<&str>) -> bool {
        let Some(state) = self.persisted.as_mut().and_then(Value::as_object_mut) else {
            return false;
        };
        if state.get("tempo").and_then(Value::as_str) != Some("blocked")
            || state.get("needs").and_then(Value::as_str) != needs
            || state.get("suggestedReply").and_then(Value::as_str) == Some(text)
        {
            return false;
        }
        state.insert("suggestedReply".into(), Value::String(text.into()));
        true
    }
}
