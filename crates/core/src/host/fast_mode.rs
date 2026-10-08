//! Current native Fast admission (SF/a8). Facts are supplied by the host.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Default)]
struct RejectedModels {
    blocked: BTreeSet<String>,
    ever: BTreeSet<String>,
}

/// Native Q4o/mQe/ttr host state. Keys are resolved identities supplied by the
/// caller, independently of model input and provider wire parsing.
#[derive(Debug, Clone, Default)]
pub struct ModelRejections(Arc<Mutex<RejectedModels>>);
impl ModelRejections {
    pub fn for_process() -> Self {
        static MODELS: OnceLock<ModelRejections> = OnceLock::new();
        MODELS.get_or_init(Self::default).clone()
    }
    pub fn reject(&self, identity: &str) -> bool {
        let mut models = self.0.lock().unwrap_or_else(|e| e.into_inner());
        models.ever.insert(identity.into());
        models.blocked.insert(identity.into())
    }
    pub fn blocked(&self, identity: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blocked
            .contains(identity)
    }
    pub fn ever_rejected(&self, identity: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ever
            .contains(identity)
    }
    pub fn known_rejected(&self, identity: &str) -> bool {
        let models = self.0.lock().unwrap_or_else(|e| e.into_inner());
        models.blocked.contains(identity) || models.ever.contains(identity)
    }
    /// Explicit fast rearming preserves IR's rejection history.
    pub fn rearm(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blocked
            .clear();
    }
    /// A full host reset clears both sets.
    pub fn reset(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = RejectedModels::default();
    }
}

/// Native zp(): profile capability comes from token scopes, not plan tier.
pub fn oauth_profile_scope(scopes: &[String]) -> bool {
    scopes.iter().any(|scope| scope == "user:profile")
}

/// Native qu()/YBe(): session tokens additionally admit CCR inference scopes.
pub fn oauth_status_allowed(
    scopes: &[String],
    session_access_token: bool,
    no_user_account: bool,
) -> bool {
    oauth_profile_scope(scopes)
        || (session_access_token
            && !no_user_account
            && scopes.iter().any(|scope| scope == "user:ccr_inference"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decline {
    NotFirstParty,
    DisabledByEnv,
    ModelNotAllowed,
    SdkOptInRequired,
    Pending,
    Unknown,
    Free,
    Preference,
    ExtraUsageDisabled,
    NetworkError,
}
impl Decline {
    pub fn org_reason(value: Option<&str>) -> Self {
        match value {
            None => Self::Preference,
            Some("free") => Self::Free,
            Some("preference") => Self::Preference,
            Some("extra_usage_disabled") => Self::ExtraUsageDisabled,
            Some("network_error") => Self::NetworkError,
            _ => Self::Unknown,
        }
    }
    pub fn transient(self) -> bool {
        matches!(self, Self::NetworkError | Self::Unknown)
    }
    pub fn message(self, inputs: &Inputs) -> String {
        match self {
            Self::NotFirstParty => {
                "Fast mode is only available when using the Anthropic API directly".into()
            }
            Self::DisabledByEnv => "Fast mode is not available".into(),
            Self::ModelNotAllowed => format!(
                "{} is not in your organization's allowed models",
                inputs.default_model_label
            ),
            Self::SdkOptInRequired => "Fast mode is not available in the Agent SDK".into(),
            Self::Pending => "Checking fast mode availability".into(),
            Self::Unknown => inputs
                .kill_switch
                .clone()
                .unwrap_or_else(|| "Fast mode is currently unavailable".into()),
            Self::Free if inputs.oauth => "Fast mode requires a paid subscription".into(),
            Self::Free => {
                "Fast mode unavailable during evaluation. Please purchase credits.".into()
            }
            Self::Preference => "Fast mode has been disabled by your organization".into(),
            Self::ExtraUsageDisabled => inputs.credits_message.clone(),
            Self::NetworkError => "Fast mode unavailable due to network connectivity issues".into(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSource {
    Server,
    Guess,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OrgStatus {
    #[default]
    Pending,
    Enabled,
    Disabled {
        reason: Decline,
        source: StatusSource,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inputs {
    pub first_party: bool,
    pub disabled: bool,
    pub kill_switch: Option<String>,
    pub default_model_allowed: bool,
    pub remote: bool,
    pub model_fast: bool,
    pub model_allowed: bool,
    pub flag_fast: bool,
    pub policy_fast: Option<bool>,
    pub policy_session_opt_in: Option<bool>,
    pub non_interactive: bool,
    pub agent_sdk: bool,
    pub session_opt_in: bool,
    pub session_only: bool,
    pub agent_owned_remote: bool,
    pub skip_org: bool,
    pub skip_network: bool,
    pub org: OrgStatus,
    pub oauth: bool,
    pub default_model_label: String,
    pub credits_message: String,
}
impl Default for Inputs {
    fn default() -> Self {
        Self {
            first_party: false,
            disabled: false,
            kill_switch: None,
            default_model_allowed: true,
            remote: false,
            model_fast: false,
            model_allowed: true,
            flag_fast: false,
            policy_fast: None,
            policy_session_opt_in: None,
            non_interactive: false,
            agent_sdk: false,
            session_opt_in: false,
            session_only: false,
            agent_owned_remote: false,
            skip_org: false,
            skip_network: false,
            org: OrgStatus::Pending,
            oauth: false,
            default_model_label: "Opus".into(),
            credits_message: "Fast mode requires usage credits".into(),
        }
    }
}
/// Native branch order, including server decisions that explicit opt-in cannot bypass.
pub fn decline(input: &Inputs) -> Option<Decline> {
    if !input.first_party {
        return Some(Decline::NotFirstParty);
    }
    if input.disabled {
        return Some(Decline::DisabledByEnv);
    }
    if input.kill_switch.is_some() {
        return Some(Decline::Unknown);
    }
    if !input.default_model_allowed && (input.remote || !input.model_fast || !input.model_allowed) {
        return Some(Decline::ModelNotAllowed);
    }
    let opted_in = input.session_opt_in || input.flag_fast;
    if input.policy_fast == Some(false) {
        return Some(Decline::Preference);
    }
    if (input.session_opt_in || input.session_only) && input.policy_session_opt_in == Some(true) {
        return Some(Decline::Preference);
    }
    if input.non_interactive && input.agent_sdk && !opted_in {
        return Some(Decline::SdkOptInRequired);
    }
    let bypass = opted_in && !input.agent_owned_remote;
    let skip_org = input.skip_org && !input.agent_owned_remote;
    match input.org {
        OrgStatus::Pending if !skip_org && !bypass => Some(Decline::Pending),
        OrgStatus::Disabled { reason, source } if !skip_org || source == StatusSource::Server => {
            if reason.transient() && ((input.skip_network && !input.agent_owned_remote) || bypass) {
                None
            } else {
                Some(reason)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_native_oauth_scope_cases() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/fast_oauth_2_1_287.json"))
                .unwrap();
        for row in fixture["cases"].as_array().unwrap() {
            let scopes: Vec<String> =
                serde_json::from_value(row["input"]["scopes"].clone()).unwrap();
            assert_eq!(
                oauth_profile_scope(&scopes),
                row["expected"]["profile"].as_bool().unwrap()
            );
            assert_eq!(
                oauth_status_allowed(
                    &scopes,
                    row["input"]["session_token"].as_bool().unwrap(),
                    row["input"]["no_user_account"].as_bool().unwrap()
                ),
                row["expected"]["eligible"].as_bool().unwrap()
            );
        }
    }
    #[test]
    fn current_native_admission_cases() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fast_admission_2_1_287.json"
        ))
        .unwrap();
        for row in fixture["cases"].as_array().unwrap() {
            let input: Inputs = serde_json::from_value(row["input"].clone()).unwrap();
            let reason = decline(&input);
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                row["reason"],
                "{row}"
            );
            assert_eq!(
                serde_json::to_value(reason.map(|r| r.message(&input))).unwrap(),
                row["message"],
                "{row}"
            );
        }
    }
}

#[cfg(test)]
mod model_rejection_tests {
    use super::*;
    #[test]
    fn rejection_rearm_and_host_reset_match_actual_native_history() {
        let f: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/fast_refusal_2_1_288.json"
        ))
        .unwrap();
        let cases = f["states"].as_array().unwrap();
        assert_eq!(cases.len(), 12);
        for case in cases {
            let models = ModelRejections::default();
            {
                let mut state = models.0.lock().unwrap();
                if case["blocked"].as_bool().unwrap() {
                    state.blocked.insert("model".into());
                }
                if case["ever"].as_bool().unwrap() {
                    state.ever.insert("model".into());
                }
            }
            match case["operation"].as_str().unwrap() {
                "reject" => {
                    models.reject("model");
                }
                "rearm" => models.rearm(),
                "reset" => models.reset(),
                _ => unreachable!(),
            }
            assert_eq!(
                models.blocked("model"),
                case["expected"]["blocked"].as_bool().unwrap()
            );
            assert_eq!(
                models.ever_rejected("model"),
                case["expected"]["ever"].as_bool().unwrap()
            );
            assert_eq!(
                models.known_rejected("model"),
                case["expected"]["recognized"].as_bool().unwrap()
            );
            assert!(!models.known_rejected("other"));
        }
    }
}
