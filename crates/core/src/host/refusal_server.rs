//! Native HVn decisions over a host-resolved model/feature snapshot.
//! Catalog, preference and dialog acquisition remain owned by the embedding host.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub candidate_model: Option<String>,
    pub enabled: bool,
    pub switch_models_on_flag: bool,
    pub server_allowed: bool,
    pub explicit_beta_rejected: bool,
    pub default_beta_rejected: bool,
    pub already_used: bool,
    pub declined: bool,
    pub candidate_same_model: bool,
    pub default_allowed: bool,
    pub explicit_target_eligible: bool,
    pub threaded_request: bool,
    pub request_dialog: bool,
    pub consumer_lacks_dialog_capability: bool,
    pub preference_question_pending: bool,
    pub silent_arm: bool,
    pub beta_transport_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Explicit,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lane {
    pub for_model: String,
    pub model: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_lane: Option<Lane>,
    pub should_log_suppression: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryPolicy {
    pub policy: Policy,
    pub model: String,
    pub query: Query,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub already_used: bool,
    pub declined: bool,
    pub in_cascade_episode: bool,
    pub cascade_next: Option<String>,
    pub is_main_thread: bool,
    pub suppression_already_logged: bool,
}

pub fn disabled_by_environment(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let value=value.trim_matches(|c|matches!(c,'\u{0009}'..='\u{000D}'|'\u{0020}'|'\u{00A0}'|'\u{1680}'|'\u{2000}'..='\u{200A}'|'\u{2028}'|'\u{2029}'|'\u{202F}'|'\u{205F}'|'\u{3000}'|'\u{FEFF}'));
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

impl Policy {
    pub fn decide(&self, current_model: &str, query: &Query) -> Decision {
        let Query {
            already_used,
            declined,
            in_cascade_episode,
            cascade_next,
            is_main_thread,
            suppression_already_logged,
        } = query.clone();
        let suppressed = is_main_thread
            && (!self.request_dialog || self.consumer_lacks_dialog_capability)
            && !self.switch_models_on_flag;
        let candidate = if in_cascade_episode {
            cascade_next.as_deref()
        } else {
            self.candidate_model.as_deref()
        };
        let visible = (!already_used && !declined && self.enabled && !suppressed)
            .then_some(candidate)
            .flatten();
        let server = visible.is_some()
            && !in_cascade_episode
            && !self.threaded_request
            && !self.preference_question_pending
            && self.enabled
            && self.switch_models_on_flag
            && self.server_allowed;
        let server = server && !self.explicit_beta_rejected;
        let mode = if server && self.default_allowed && !self.default_beta_rejected {
            Mode::Default
        } else {
            Mode::Explicit
        };
        let lane =
            (server && (mode == Mode::Default || !self.candidate_same_model)).then(|| Lane {
                for_model: current_model.into(),
                model: visible.expect("server candidate").into(),
                mode,
            });
        Decision {
            visible_model: visible.map(str::to_owned),
            server_lane: lane,
            should_log_suppression: !suppression_already_logged
                && self.enabled
                && suppressed
                && candidate.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_native_query_admission_and_dialog_suppression_match() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fallback_admission_2_1_288.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let policy: Policy = serde_json::from_value(case["policy"].clone()).unwrap();
            let q = &case["query"];
            let result = policy.decide(
                q["model"].as_str().unwrap(),
                &Query {
                    already_used: q["alreadyUsed"].as_bool().unwrap(),
                    declined: q["declined"].as_bool().unwrap(),
                    in_cascade_episode: q["inCascade"].as_bool().unwrap(),
                    cascade_next: q["next"].as_str().map(str::to_owned),
                    is_main_thread: q["main"].as_bool().unwrap(),
                    suppression_already_logged: q["logged"].as_bool().unwrap(),
                },
            );
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                case["expected"],
                "{case}"
            );
        }
        for case in fixture["envCases"].as_array().unwrap() {
            assert_eq!(
                disabled_by_environment(case["input"].as_str()),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
