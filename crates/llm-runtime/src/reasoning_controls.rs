use crate::{LlmError, LlmRequest, ProtocolFamily, ReasoningConfig};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningSelection {
    Automatic,
    Disabled,
    Enabled,
    Level(String),
    TokenBudget(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudgetRange {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReasoningControlSpec {
    pub levels: Vec<String>,
    pub token_budget: Option<TokenBudgetRange>,
    pub can_disable: bool,
    pub can_enable: bool,
    pub mandatory_selection: Option<ReasoningSelection>,
}

impl ReasoningControlSpec {
    #[must_use]
    pub fn automatic_only() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn supports(&self, selection: &ReasoningSelection) -> bool {
        // Automatic never sends an override; provider defaults remain
        // authoritative even for models that mandate internal reasoning.
        if matches!(selection, ReasoningSelection::Automatic) {
            return true;
        }
        if let Some(mandatory) = &self.mandatory_selection {
            return mandatory == selection;
        }
        match selection {
            ReasoningSelection::Automatic => true,
            ReasoningSelection::Disabled => self.can_disable,
            ReasoningSelection::Enabled => self.can_enable,
            ReasoningSelection::Level(level) => self.levels.iter().any(|item| item == level),
            ReasoningSelection::TokenBudget(tokens) => self
                .token_budget
                .is_some_and(|range| *tokens >= range.min && *tokens <= range.max),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReasoningTarget<'a> {
    pub profile_name: Option<&'a str>,
    pub protocol: &'a ProtocolFamily,
    pub base_url: &'a str,
    pub model: &'a str,
}

pub fn apply_reasoning_selection(
    request: &mut LlmRequest,
    target: ReasoningTarget<'_>,
    selection: ReasoningSelection,
) -> Result<(), LlmError> {
    let spec = reasoning_control_spec(target);
    if !spec.supports(&selection) {
        return Err(LlmError::InvalidRequest {
            message: format!(
                "reasoning selection {selection:?} is unsupported for model {}",
                target.model
            ),
        });
    }

    request.reasoning = None;
    request.effort = None;
    match selection {
        ReasoningSelection::Automatic => {}
        ReasoningSelection::Disabled => {
            request.effort = Some(Value::String("disabled".to_string()));
        }
        ReasoningSelection::Enabled => {
            request.effort = Some(Value::String("enabled".to_string()));
        }
        ReasoningSelection::Level(level) => {
            request.effort = Some(Value::String(level));
        }
        ReasoningSelection::TokenBudget(tokens) => {
            request.effort = Some(Value::from(tokens));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestReasoningIntent {
    Automatic,
    LegacyAdaptive,
    LegacyBudget(u32),
    Disabled,
    Enabled,
    Level(String),
    EffortBudget(u32),
}

pub(crate) fn request_reasoning_intent(request: &LlmRequest) -> RequestReasoningIntent {
    if let Some(effort) = &request.effort {
        if let Some(level) = effort.as_str() {
            return match level.to_ascii_lowercase().as_str() {
                "auto" => RequestReasoningIntent::Automatic,
                "disabled" | "off" | "none" => RequestReasoningIntent::Disabled,
                "enabled" | "on" => RequestReasoningIntent::Enabled,
                _ => RequestReasoningIntent::Level(level.to_string()),
            };
        }
        if let Some(tokens) = effort.as_u64().and_then(|value| u32::try_from(value).ok()) {
            return RequestReasoningIntent::EffortBudget(tokens);
        }
    }

    match request.reasoning {
        Some(ReasoningConfig::Adaptive) => RequestReasoningIntent::LegacyAdaptive,
        Some(ReasoningConfig::Enabled { budget_tokens }) => {
            RequestReasoningIntent::LegacyBudget(budget_tokens)
        }
        None => RequestReasoningIntent::Automatic,
    }
}

pub fn reasoning_control_spec(target: ReasoningTarget<'_>) -> ReasoningControlSpec {
    if let Some(spec) = match target.profile_name {
        Some("zai") => Some(catalog_control_spec(zai_catalog(), target.model)),
        Some("glm-coding") => Some(catalog_control_spec(glm_coding_catalog(), target.model)),
        Some("github-copilot") => {
            Some(catalog_control_spec(github_copilot_catalog(), target.model))
        }
        _ => None,
    } {
        return spec;
    }
    match target.protocol {
        ProtocolFamily::AnthropicMessages
        | ProtocolFamily::BedrockClaude
        | ProtocolFamily::FoundryClaude
        | ProtocolFamily::VertexClaude => anthropic_spec(target.model),
        ProtocolFamily::OpenAiResponses => openai_responses_spec(target.base_url, target.model),
        ProtocolFamily::GeminiGenerateContent | ProtocolFamily::VertexGemini => {
            gemini_spec(target.model)
        }
        ProtocolFamily::OpenAiChat => {
            openai_chat_spec(target.profile_name, target.base_url, target.model)
        }
        ProtocolFamily::AzureOpenAi => ReasoningControlSpec::automatic_only(),
    }
}

fn catalog_control_spec(
    catalog: &HashMap<String, CatalogReasoningSpec>,
    model: &str,
) -> ReasoningControlSpec {
    let Some(spec) = catalog.get(model) else {
        return ReasoningControlSpec::automatic_only();
    };
    if !spec.reasoning {
        return ReasoningControlSpec::automatic_only();
    }
    let can_disable = spec.toggle || spec.levels.iter().any(|level| level == "none");
    ReasoningControlSpec {
        levels: spec
            .levels
            .iter()
            .filter(|level| level.as_str() != "none")
            .cloned()
            .collect(),
        token_budget: spec.token_budget,
        can_disable,
        can_enable: spec.toggle && spec.levels.is_empty() && spec.token_budget.is_none(),
        mandatory_selection: None,
    }
}

#[derive(Debug, Clone)]
struct CatalogReasoningSpec {
    reasoning: bool,
    levels: Vec<String>,
    token_budget: Option<TokenBudgetRange>,
    toggle: bool,
}

fn anthropic_spec(model: &str) -> ReasoningControlSpec {
    use platform_api::model_capabilities::{has_capability, ModelCapability};

    if !has_capability(model, ModelCapability::Effort) {
        return ReasoningControlSpec::automatic_only();
    }

    let mut levels = vec!["low".to_string(), "medium".to_string(), "high".to_string()];
    if has_capability(model, ModelCapability::XHighEffort) {
        levels.push("xhigh".to_string());
    }
    if has_capability(model, ModelCapability::MaxEffort) {
        levels.push("max".to_string());
    }

    ReasoningControlSpec {
        levels,
        token_budget: None,
        can_disable: false,
        can_enable: false,
        mandatory_selection: None,
    }
}

fn openai_responses_spec(base_url: &str, model: &str) -> ReasoningControlSpec {
    if base_url.contains("chatgpt.com/backend-api/codex") {
        return match model {
            "gpt-5-codex" | "gpt-5.3-codex" => ReasoningControlSpec {
                levels: vec![
                    "minimal".to_string(),
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                ],
                token_budget: None,
                can_disable: false,
                can_enable: false,
                mandatory_selection: None,
            },
            _ => openai_catalog().get(model).map_or_else(
                ReasoningControlSpec::automatic_only,
                catalog_spec_for_openai,
            ),
        };
    }

    if let Some(spec) = openai_catalog().get(model) {
        return catalog_spec_for_openai(spec);
    }

    if base_url.contains("aliyuncs.com") && model.starts_with("qwen") {
        return ReasoningControlSpec {
            levels: vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ],
            token_budget: None,
            can_disable: true,
            can_enable: false,
            mandatory_selection: None,
        };
    }

    ReasoningControlSpec::automatic_only()
}

fn catalog_spec_for_openai(spec: &CatalogReasoningSpec) -> ReasoningControlSpec {
    if !spec.reasoning || spec.levels.is_empty() {
        return ReasoningControlSpec::automatic_only();
    }
    let can_disable = spec.levels.iter().any(|level| level == "none");
    let levels = spec
        .levels
        .iter()
        .filter(|level| level.as_str() != "none")
        .cloned()
        .collect();
    ReasoningControlSpec {
        levels,
        token_budget: None,
        can_disable,
        can_enable: false,
        mandatory_selection: None,
    }
}

fn gemini_spec(model: &str) -> ReasoningControlSpec {
    let Some(spec) = gemini_catalog().get(model) else {
        return ReasoningControlSpec::automatic_only();
    };
    if !spec.reasoning {
        return ReasoningControlSpec::automatic_only();
    }
    ReasoningControlSpec {
        levels: spec.levels.clone(),
        token_budget: spec.token_budget,
        can_disable: spec.toggle && spec.token_budget.is_some(),
        can_enable: spec.toggle && spec.levels.is_empty() && spec.token_budget.is_none(),
        mandatory_selection: None,
    }
}

fn openai_chat_spec(
    profile_name: Option<&str>,
    base_url: &str,
    model: &str,
) -> ReasoningControlSpec {
    if is_openrouter_profile(profile_name, base_url) {
        return openrouter_spec(model);
    }
    if is_deepseek_profile(profile_name, base_url) {
        return deepseek_spec(model);
    }
    if is_kimi_profile(profile_name) {
        return kimi_spec(model);
    }
    ReasoningControlSpec::automatic_only()
}

fn openrouter_spec(model: &str) -> ReasoningControlSpec {
    let Some(spec) = openrouter_catalog().get(model) else {
        return ReasoningControlSpec::automatic_only();
    };
    if !spec.reasoning {
        return ReasoningControlSpec::automatic_only();
    }
    if spec.levels.is_empty() && spec.token_budget.is_none() && !spec.toggle {
        return ReasoningControlSpec::automatic_only();
    }
    let can_disable = spec.toggle || spec.levels.iter().any(|level| level == "none");
    let levels = spec
        .levels
        .iter()
        .filter(|level| level.as_str() != "none")
        .cloned()
        .collect();
    ReasoningControlSpec {
        levels,
        token_budget: spec.token_budget,
        can_disable,
        can_enable: spec.toggle && spec.levels.is_empty() && spec.token_budget.is_none(),
        mandatory_selection: None,
    }
}

fn deepseek_spec(model: &str) -> ReasoningControlSpec {
    match model {
        "deepseek-reasoner" => ReasoningControlSpec {
            levels: Vec::new(),
            token_budget: None,
            can_disable: false,
            can_enable: false,
            mandatory_selection: Some(ReasoningSelection::Enabled),
        },
        _ => {
            let Some(spec) = deepseek_catalog().get(model) else {
                return ReasoningControlSpec::automatic_only();
            };
            if !spec.reasoning {
                return ReasoningControlSpec::automatic_only();
            }
            ReasoningControlSpec {
                levels: spec.levels.clone(),
                token_budget: None,
                can_disable: spec.toggle,
                can_enable: false,
                mandatory_selection: None,
            }
        }
    }
}

fn kimi_spec(model: &str) -> ReasoningControlSpec {
    match model {
        "kimi-k2-thinking"
        | "k2-thinking"
        | "kimi-k2-thinking-preview"
        | "kimi-k2.7-code"
        | "kimi-for-coding"
        | "kimi-for-coding-highspeed" => ReasoningControlSpec {
            levels: Vec::new(),
            token_budget: None,
            can_disable: false,
            can_enable: false,
            mandatory_selection: Some(ReasoningSelection::Enabled),
        },
        _ => {
            let spec = kimi_catalog()
                .get(model)
                .or_else(|| kimi_code_catalog().get(model));
            let Some(spec) = spec else {
                return ReasoningControlSpec::automatic_only();
            };
            if !spec.reasoning {
                return ReasoningControlSpec::automatic_only();
            }
            let can_enable = spec.toggle && spec.levels.is_empty() && spec.token_budget.is_none();
            // K3 exposes discrete effort levels, not a separate off switch.
            // Only toggle-only K2.x entries may advertise Disable.
            let can_disable = spec.toggle && spec.levels.is_empty() && spec.token_budget.is_none();
            ReasoningControlSpec {
                levels: spec.levels.clone(),
                token_budget: None,
                can_disable,
                can_enable,
                mandatory_selection: None,
            }
        }
    }
}

pub(crate) fn is_openrouter_profile(profile_name: Option<&str>, base_url: &str) -> bool {
    profile_name == Some("openrouter") || base_url.contains("openrouter.ai")
}

pub(crate) fn is_deepseek_profile(profile_name: Option<&str>, base_url: &str) -> bool {
    profile_name == Some("deepseek")
        || matches!(
            base_url.trim_end_matches('/'),
            "https://api.deepseek.com" | "https://api.deepseek.com/v1"
        )
}

pub(crate) fn is_kimi_profile(profile_name: Option<&str>) -> bool {
    matches!(profile_name, Some("kimi" | "kimi-code"))
}

fn openai_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static OPENAI: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    OPENAI.get_or_init(|| upstream_catalog("openai"))
}

fn gemini_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static GEMINI: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    GEMINI.get_or_init(|| upstream_catalog("gemini"))
}

fn deepseek_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static DEEPSEEK: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    DEEPSEEK.get_or_init(|| upstream_catalog("deepseek"))
}

fn kimi_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static KIMI: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    KIMI.get_or_init(|| upstream_catalog("kimi"))
}

fn kimi_code_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static KIMI_CODE: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    KIMI_CODE.get_or_init(|| upstream_catalog("kimi-code"))
}

fn openrouter_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static OPENROUTER: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    OPENROUTER.get_or_init(|| upstream_catalog("openrouter"))
}

fn zai_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static CATALOG: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    CATALOG.get_or_init(|| upstream_catalog("zai"))
}

fn glm_coding_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static CATALOG: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    CATALOG.get_or_init(|| upstream_catalog("glm-coding"))
}

fn github_copilot_catalog() -> &'static HashMap<String, CatalogReasoningSpec> {
    static CATALOG: OnceLock<HashMap<String, CatalogReasoningSpec>> = OnceLock::new();
    CATALOG.get_or_init(|| upstream_catalog("github-copilot"))
}

fn upstream_catalog(profile_name: &str) -> HashMap<String, CatalogReasoningSpec> {
    use lingxi_llm_client::protocol::{CapabilitySupport, ThinkingMode};
    let Some(profile) = lingxi_llm_client::builtin_providers()
        .expect("pinned catalog parses")
        .into_iter()
        .find(|p| p.profile_name == profile_name)
    else {
        return HashMap::new();
    };
    profile
        .models
        .iter()
        .map(|model| {
            let features = model.info.features.on_connection(&profile.info.features);
            let levels = features
                .effort
                .levels
                .unwrap_or_default()
                .into_iter()
                .filter_map(|effort| {
                    serde_json::to_value(effort)
                        .ok()?
                        .as_str()
                        .map(str::to_owned)
                })
                .collect();
            let token_budget = match (features.budget.min_tokens, features.budget.max_tokens) {
                (Some(min), Some(max))
                    if features.budget.support == CapabilitySupport::Supported =>
                {
                    Some(TokenBudgetRange { min, max })
                }
                _ => None,
            };
            let toggle = features.modes.as_ref().is_some_and(|m| {
                m.contains(&ThinkingMode::Enabled) && m.contains(&ThinkingMode::Disabled)
            });
            (
                model.request_model.clone(),
                CatalogReasoningSpec {
                    reasoning: features.thinking == CapabilitySupport::Supported,
                    levels,
                    token_budget,
                    toggle,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_capability_levels_follow_model_registry() {
        let spec = anthropic_spec("claude-opus-5");
        assert_eq!(spec.levels, vec!["low", "medium", "high", "xhigh", "max"]);
        assert!(!spec.can_disable);

        let unknown = anthropic_spec("claude-unknown");
        assert_eq!(unknown, ReasoningControlSpec::automatic_only());
    }

    #[test]
    fn openai_and_chatgpt_codex_specs_are_provider_aware() {
        let openai = openai_responses_spec("https://api.openai.com/v1", "gpt-5");
        assert_eq!(openai.levels, vec!["minimal", "low", "medium", "high"]);
        assert!(!openai.can_disable);

        let chatgpt = openai_responses_spec("https://chatgpt.com/backend-api/codex", "gpt-5-codex");
        assert_eq!(chatgpt.levels, vec!["minimal", "low", "medium", "high"]);
    }

    #[test]
    fn gemini_spec_distinguishes_budget_and_level_models() {
        let budget = gemini_spec("gemini-2.5-flash");
        assert_eq!(
            budget.token_budget,
            Some(TokenBudgetRange {
                min: 1,
                max: 24_576
            })
        );
        assert!(budget.can_disable);
        assert!(budget.levels.is_empty());

        let levels = gemini_spec("gemini-3.1-pro-preview");
        assert_eq!(levels.levels, vec!["low", "medium", "high"]);
        assert!(!levels.can_disable);
    }

    #[test]
    fn deepseek_and_kimi_specs_only_expose_verified_controls() {
        let deepseek = deepseek_spec("deepseek-flash");
        assert_eq!(deepseek.levels, vec!["low", "high", "max"]);
        assert!(deepseek.can_disable);
        assert!(!deepseek.can_enable);

        let kimi_k3 = kimi_spec("kimi-k3");
        assert_eq!(kimi_k3.levels, vec!["low", "high", "max"]);
        assert!(!kimi_k3.can_disable);

        let kimi_k27 = kimi_spec("kimi-k2.7-code");
        assert_eq!(
            kimi_k27.mandatory_selection,
            Some(ReasoningSelection::Enabled)
        );
    }

    #[test]
    fn openrouter_spec_uses_catalog_reasoning_options() {
        let spec = openrouter_spec("google/gemini-3.5-flash");
        assert!(
            spec.levels.is_empty(),
            "upstream does not publish effort levels for this route"
        );
        assert!(!spec.can_disable);

        let unknown = openrouter_spec("openrouter/auto");
        assert_eq!(unknown, ReasoningControlSpec::automatic_only());
    }

    #[test]
    fn subscription_and_glm_profiles_use_their_route_catalog() {
        let protocol = ProtocolFamily::AnthropicMessages;
        let glm = reasoning_control_spec(ReasoningTarget {
            profile_name: Some("glm-coding"),
            protocol: &protocol,
            base_url: "https://open.bigmodel.cn/api/anthropic",
            model: "glm-5.3",
        });
        assert_eq!(glm.levels, vec!["low", "high", "max"]);

        let chat = ProtocolFamily::OpenAiChat;
        let copilot = reasoning_control_spec(ReasoningTarget {
            profile_name: Some("github-copilot"),
            protocol: &chat,
            base_url: "https://api.githubcopilot.com",
            model: "gpt-5.6-sol",
        });
        assert_eq!(
            copilot,
            ReasoningControlSpec::automatic_only(),
            "unknown upstream controls must not be inferred from the model name"
        );
    }

    #[test]
    fn apply_selection_reuses_existing_request_fields() {
        let target = ReasoningTarget {
            profile_name: None,
            protocol: &ProtocolFamily::OpenAiResponses,
            base_url: "https://api.openai.com/v1",
            model: "gpt-5",
        };
        let mut request = LlmRequest::new("gpt-5");
        apply_reasoning_selection(
            &mut request,
            target,
            ReasoningSelection::Level("high".to_string()),
        )
        .expect("selection");
        assert_eq!(request.effort, Some(Value::String("high".to_string())));
        assert!(request.reasoning.is_none());
    }
}
