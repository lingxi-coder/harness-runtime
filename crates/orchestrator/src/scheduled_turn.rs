//! Immutable per-turn settings. Task-local scope cannot overwrite session defaults.
#[derive(Clone)]
pub(crate) struct ScheduledSettings {
    pub model: String,
    pub provider: String,
    pub reasoning: platform_api::ReasoningSelection,
    pub thinking: llm_runtime::model::thinking::ThinkingConfig,
    pub effort: Option<serde_json::Value>,
}
tokio::task_local! { pub(crate) static SETTINGS: ScheduledSettings; }
pub(crate) fn current() -> Option<ScheduledSettings> {
    SETTINGS.try_with(Clone::clone).ok()
}
