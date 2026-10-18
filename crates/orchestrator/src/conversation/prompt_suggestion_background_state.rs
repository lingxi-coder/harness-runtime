//! Suggestion-facing subset of Native's classifier job state. The classifier
//! owns this state; composer context never supplies a guessed blocked flag.
use lingxi_core::types::utf16_json::Utf16JsonProjection;

#[derive(Debug, Clone, Default)]
pub(crate) struct BackgroundSuggestionState {
    pub session_id: String,
    pub epoch: u64,
    pub last_message_count: usize,
    pub previous_state: String,
    pub previous_state_since_ms: i64,
    pub last_result: Option<BackgroundSuggestionContext>,
    pub persisted: Option<Utf16JsonProjection>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BackgroundSuggestionContext {
    pub session_id: String,
    pub epoch: u64,
    pub needs: Option<Vec<u16>>,
}

impl BackgroundSuggestionContext {
    /// Admit only the current classifier's blocked state when no operation
    /// block is active, keeping `needs` in its source UTF-16 units.
    pub(super) fn from_blocked_classifier(
        session_id: String,
        epoch: u64,
        classifier_result: &Utf16JsonProjection,
        persisted_result: &Utf16JsonProjection,
    ) -> Option<Self> {
        if classifier_result.string_at("/tempo") != Some("blocked")
            || persisted_result
                .get_property("block")
                .is_some_and(|block| !block.is_null())
        {
            return None;
        }
        Some(Self {
            session_id,
            epoch,
            needs: classifier_result.string_units("/needs"),
        })
    }
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
        if let Some(state) = self.persisted.as_mut() {
            let _ = state.set_field("tempo", Utf16JsonProjection::from("active"));
            let _ = state.remove_field("needs");
            let _ = state.remove_field("block");
            let _ = state.remove_field("suggestedReply");
        }
    }

    pub fn eligible_context(&self) -> Option<BackgroundSuggestionContext> {
        self.last_result.clone()
    }

    pub fn record_reply(&mut self, text: &Utf16JsonProjection, needs: Option<&[u16]>) -> bool {
        let Some(state) = self.persisted.as_mut() else {
            return false;
        };
        if state.string_at("/tempo") != Some("blocked")
            || state.string_units("/needs").as_deref() != needs
            || state.string_units("/suggestedReply").as_deref() == text.string_units("").as_deref()
        {
            return false;
        }
        state.set_field("suggestedReply", text.clone()).is_ok()
    }
}
