//! Session-owned native model-selection latch and resume decisions.
use crate::types::{ConversationMessage, RefusalFallbackMetadata};
use serde_json::Value;

/// `None`, `Some(None)` and `Some(Some(model))` retain native undefined/null/string.
pub type ModelSlot = Option<Option<String>>;

/// Native `Mn` removes both context tags wherever they occur, preserving case.
pub fn wire_identity(model: &str) -> String {
    let bytes = model.as_bytes();
    let mut result = String::with_capacity(model.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes.get(index..index + 4).is_some_and(|tag| {
            tag[0] == b'['
                && matches!(tag[1], b'1' | b'2')
                && matches!(tag[2], b'm' | b'M')
                && tag[3] == b']'
        }) {
            index += 4;
        } else {
            let ch = model[index..].chars().next().unwrap();
            result.push(ch);
            index += ch.len_utf8();
        }
    }
    result
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelLatch {
    pub fallback_model: String,
    pub previous_override: ModelSlot,
    pub previous_app_state_model: ModelSlot,
    pub previous_model_for_session: ModelSlot,
    pub carried_effort: Option<Value>,
    /// Harness's authorized provider routing companion to the model snapshot.
    pub previous_profile: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelSelection {
    pub override_model: ModelSlot,
    pub latch: Option<ModelLatch>,
    pub refusal_occurred: bool,
    pub header_armed: bool,
    pub origin_request_id: Option<String>,
    pub carried_effort_end: Option<String>,
}

impl ModelSelection {
    pub fn live_latch(&self) -> Option<&ModelLatch> {
        self.latch.as_ref().filter(|latch| {
            self.override_model.as_ref().and_then(Option::as_deref)
                == Some(latch.fallback_model.as_str())
        })
    }

    /// Native `vvt`: another owned hop retains the original restoration snapshot.
    pub fn latch(&mut self, mut next: ModelLatch) {
        if let Some(previous) = self.live_latch() {
            next.previous_override
                .clone_from(&previous.previous_override);
            next.previous_app_state_model
                .clone_from(&previous.previous_app_state_model);
            next.previous_model_for_session
                .clone_from(&previous.previous_model_for_session);
            next.previous_profile.clone_from(&previous.previous_profile);
            if next.carried_effort.as_ref().is_none_or(Value::is_null) {
                next.carried_effort.clone_from(&previous.carried_effort);
            }
        }
        self.replace_latch(Some(next), "superseded");
    }

    fn replace_latch(&mut self, next: Option<ModelLatch>, reason: &str) {
        if self
            .latch
            .as_ref()
            .is_some_and(|latch| latch.carried_effort.is_some())
            && next
                .as_ref()
                .is_none_or(|latch| latch.carried_effort.is_none())
        {
            self.carried_effort_end = Some(reason.into());
        }
        self.latch = next;
    }

    pub fn model_pick(&mut self, model: &str) {
        self.replace_latch(None, "model_pick");
        self.override_model = Some(Some(model.into()));
    }

    pub fn take_carried_effort_end(&mut self) -> Option<String> {
        self.carried_effort_end.take()
    }

    pub fn session_transition(&mut self) -> Option<ModelLatch> {
        let restore = self.live_latch().cloned();
        self.replace_latch(None, "session_transition");
        if let Some(latch) = &restore {
            self.override_model.clone_from(&latch.previous_override);
        }
        self.refusal_occurred = false;
        self.header_armed = false;
        self.origin_request_id = None;
        restore
    }

    pub fn arm_header(&mut self, request_id: Option<String>) {
        self.header_armed = true;
        if self.origin_request_id.is_none() {
            self.origin_request_id = request_id;
        }
    }

    pub fn restore_headers(&mut self, frames: &[RefusalFallbackMetadata], fork: bool) {
        if fork {
            return;
        }
        let cyber: Vec<_> = frames
            .iter()
            .filter(|frame| {
                frame.neutralized_by_fork != Some(true)
                    && (frame.api_refusal_category.as_deref() == Some("cyber")
                        || frame.saw_cyber_refusal == Some(true))
            })
            .collect();
        if !cyber.is_empty() {
            let request_id = cyber.iter().find_map(|frame| {
                (frame.api_refusal_category.as_deref() == Some("cyber"))
                    .then_some(frame.request_id.as_ref())
                    .flatten()
                    .cloned()
            });
            self.arm_header(request_id);
        }
    }

    /// Native `cCe`/`dCe`: model restoration and cyber header restoration are
    /// independent. The latest notice controls the former, all eligible notices
    /// control the latter. Full model-identity acquisition belongs to the host.
    pub fn restore(
        &mut self,
        requested_model: &str,
        frames: &[RefusalFallbackMetadata],
        fork: bool,
        initial_model: ModelSlot,
        equivalent: impl Fn(&str, &str) -> bool,
        same_identity: impl Fn(&str, &str) -> bool,
    ) -> bool {
        self.restore_headers(frames, fork);
        let last = frames.last();
        let is_fallback =
            last.is_some_and(|frame| equivalent(&frame.fallback_model, requested_model));
        if is_fallback
            && (fork
                || last.is_some_and(|frame| {
                    frame.neutralized_by_fork == Some(true)
                        && same_identity(&frame.fallback_model, requested_model)
                }))
        {
            return false;
        }
        self.override_model = Some(Some(requested_model.into()));
        if is_fallback {
            self.latch(ModelLatch {
                fallback_model: requested_model.into(),
                previous_app_state_model: initial_model.or(Some(None)),
                previous_model_for_session: Some(None),
                ..Default::default()
            });
        }
        true
    }
}

pub fn frames(history: &[ConversationMessage]) -> Vec<RefusalFallbackMetadata> {
    history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::System {
                subtype,
                refusal_fallback: Some(metadata),
                ..
            } if subtype.as_deref() == Some("model_refusal_fallback") => Some(metadata.clone()),
            _ => None,
        })
        .collect()
}

pub fn neutralize_fork(history: &mut [ConversationMessage]) {
    for message in history {
        if let ConversationMessage::System {
            subtype,
            refusal_fallback: Some(metadata),
            ..
        } = message
        {
            if subtype.as_deref() == Some("model_refusal_fallback") {
                metadata.neutralized_by_fork = Some(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/refusal_state_2_1_288.json"
        ))
        .unwrap()
    }
    fn slot(value: &Value, key: &str) -> ModelSlot {
        value
            .get(key)
            .map(|value| value.as_str().map(str::to_owned))
    }
    fn put_slot(map: &mut Map<String, Value>, key: &str, value: &ModelSlot) {
        if let Some(model) = value {
            map.insert(key.into(), json!(model));
        }
    }
    fn latch_from(value: &Value) -> ModelLatch {
        ModelLatch {
            fallback_model: value["fallbackModel"].as_str().unwrap().into(),
            previous_override: slot(value, "previousOverride"),
            previous_app_state_model: slot(value, "previousAppStateModel"),
            previous_model_for_session: slot(value, "previousModelForSession"),
            carried_effort: value.get("carriedEffort").cloned(),
            ..Default::default()
        }
    }
    fn latch_json(latch: Option<&ModelLatch>) -> Value {
        let Some(latch) = latch else {
            return Value::Null;
        };
        let mut map = Map::new();
        map.insert("fallbackModel".into(), json!(latch.fallback_model));
        put_slot(&mut map, "previousOverride", &latch.previous_override);
        put_slot(
            &mut map,
            "previousAppStateModel",
            &latch.previous_app_state_model,
        );
        put_slot(
            &mut map,
            "previousModelForSession",
            &latch.previous_model_for_session,
        );
        if let Some(effort) = &latch.carried_effort {
            map.insert("carriedEffort".into(), effort.clone());
        }
        Value::Object(map)
    }
    #[test]
    fn native_latch_retarget_and_session_transition_match_all_slots() {
        let fixture = fixture();
        for case in fixture["operations"].as_array().unwrap() {
            let mut state = ModelSelection {
                override_model: slot(case, "override"),
                latch: Some(latch_from(&case["initial"])),
                ..Default::default()
            };
            let actual = if case["operation"] == "latch" {
                state.latch(latch_from(&case["next"]));
                json!({"latch":latch_json(state.latch.as_ref()),"end":state.take_carried_effort_end()})
            } else {
                let restored = state.session_transition();
                let restore = if let Some(latch) = restored {
                    let mut map = Map::new();
                    put_slot(&mut map, "appStateModel", &latch.previous_app_state_model);
                    put_slot(
                        &mut map,
                        "forSessionValue",
                        &latch.previous_model_for_session,
                    );
                    put_slot(&mut map, "overrideValue", &latch.previous_override);
                    map.insert(
                        "restoredToExplicitOverride".into(),
                        json!(latch.previous_override.is_some()),
                    );
                    map.insert("fallbackModel".into(), json!(latch.fallback_model));
                    Value::Object(map)
                } else {
                    Value::Null
                };
                let mut map = Map::new();
                map.insert("latch".into(), latch_json(state.latch.as_ref()));
                put_slot(&mut map, "override", &state.override_model);
                map.insert("restoration".into(), restore);
                map.insert("end".into(), json!(state.take_carried_effort_end()));
                Value::Object(map)
            };
            assert_eq!(actual, case["expected"], "{case}");
        }
    }
    #[test]
    fn native_resume_model_latch_and_cyber_header_decisions_match() {
        let fixture = fixture();
        for case in fixture["resumeCases"].as_array().unwrap() {
            let frames: Vec<RefusalFallbackMetadata> = case["frames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|frame| {
                    let mut value = frame.clone();
                    value["trigger"] = json!("refusal");
                    value["direction"] = json!("retry");
                    value["originalModel"] = json!("origin");
                    serde_json::from_value(value).unwrap()
                })
                .collect();
            let mut state = ModelSelection {
                override_model: Some(Some("before".into())),
                ..Default::default()
            };
            let key = |model: &str, field: &str| {
                fixture["identities"][model][field]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let accepted = state.restore(
                case["requestedModel"].as_str().unwrap(),
                &frames,
                case["fork"].as_bool().unwrap(),
                Some(Some("launch".into())),
                |a, b| key(a, "wire") == key(b, "wire") || key(a, "family") == key(b, "family"),
                |a, b| key(a, "wire") == key(b, "wire"),
            );
            let actual = json!({"accepted":accepted,"override":state.override_model.flatten(),"latch":latch_json(state.latch.as_ref()),"armed":state.header_armed,"origin":state.origin_request_id});
            assert_eq!(actual, case["expected"], "{case}");
        }
        for (model, identity) in fixture["identities"].as_object().unwrap() {
            assert_eq!(wire_identity(model), identity["wire"].as_str().unwrap());
        }
    }
}
