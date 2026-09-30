use super::FUSION_PANEL_QUERY_SOURCE;
use crate::definition::AgentSource;
use lingxi_core::host::subagent_spawn::{SubagentUsage, SubagentUsageRecorder};
use lingxi_core::types::ConversationMessage;
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn subagent_usage_from_llm_usage(usage: &llm_runtime::ExecutionUsage) -> SubagentUsage {
    let counts = usage.counts();
    let visible_output = counts.output_tokens.saturating_sub(counts.reasoning_tokens);
    SubagentUsage {
        total_tokens: counts
            .input_tokens
            .saturating_add(counts.cache_write_tokens)
            .saturating_add(counts.cache_read_tokens)
            .saturating_add(visible_output),
        input_tokens: counts.input_tokens,
        output_tokens: visible_output,
        cache_creation_input_tokens: counts.cache_write_tokens,
        cache_read_input_tokens: counts.cache_read_tokens,
        // Finding [1]: without this, a subagent's (including a Fusion
        // panel's) reasoning tokens were dropped at this seam — the caller
        // never saw them, no matter how the provider billed them.
        reasoning_output_tokens: counts.reasoning_tokens,
    }
}

pub(super) async fn record_subagent_usage(
    recorder: Option<&Arc<dyn SubagentUsageRecorder>>,
    query_source_label: Option<&str>,
    session_id: Option<lingxi_core::types::SessionId>,
    model: &str,
    model_profile: Option<&str>,
    usage: SubagentUsage,
    duration: Duration,
    usage_complete: bool,
) {
    // Fusion owns its own attempt reservation and settlement. Recording its
    // panel response here as a normal API response would charge it twice.
    if query_source_label == Some(FUSION_PANEL_QUERY_SOURCE) || usage.is_zero() {
        return;
    }
    if let Some(recorder) = recorder {
        recorder
            .record_subagent_usage(
                session_id,
                model,
                model_profile,
                usage,
                duration,
                usage_complete,
            )
            .await;
    }
}

pub(super) fn observer_initial_message_index(messages: Option<&[ConversationMessage]>) -> u64 {
    messages
        .unwrap_or_default()
        .iter()
        .filter(|message| match message {
            ConversationMessage::User { is_meta: true, .. } => false,
            ConversationMessage::User {
                is_compact_summary: true,
                ..
            } => false,
            ConversationMessage::User {
                is_visible_in_transcript_only: true,
                ..
            } => false,
            ConversationMessage::System {
                subtype: Some(subtype),
                ..
            } if subtype.starts_with("agent_") => false,
            _ => true,
        })
        .count() as u64
}

/// Map a LingXi [`AgentSource`] to claude-code's `selectedAgent.source` literal
/// (`SettingSource` ∪ `'built-in'` / `'plugin'`, loadAgentsDir.ts:137/156 +
/// Extract a one-line summary of each tool CALL in a serialized subagent
/// message (`SubagentEvent::Message`), for the nested-progress display. Searches
/// the message JSON recursively for `type:"tool_use"` content blocks (robust to
/// the message-envelope shape) and formats `Name(hint)`, where `hint` is the
/// first string field of the tool input (file path / pattern / command).
pub(super) fn subagent_tool_call_lines(message: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_tool_calls(message, &mut out);
    out
}

/// Encode a subagent ASSISTANT message as a sentinel-wrapped JSON line for the
/// `spawn_with_progress` `String` channel (`--forward-subagent-text`, 2.1.212).
///
/// Returns `None` for non-assistant messages (user/tool_result rides the
/// always-on activity path). The Agent tool decodes the returned line via
/// [`lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_MESSAGE_SENTINEL`] and forwards
/// the inner message to the stream-json sink, which re-emits its text/thinking
/// blocks with `parent_tool_use_id` set. The final text/thinking gate lives at
/// the sink, so this stays cheap and unconditional for assistant turns.
pub(super) fn forward_subagent_message_line(message: &serde_json::Value) -> Option<String> {
    if message.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    serde_json::to_string(&serde_json::json!({
        lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_MESSAGE_SENTINEL: message,
    }))
    .ok()
}

pub(super) fn collect_tool_calls(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(serde_json::Value::as_str) == Some("tool_use") {
                if let Some(name) = map.get("name").and_then(serde_json::Value::as_str) {
                    let hint = map.get("input").map(short_input_hint).unwrap_or_default();
                    out.push(if hint.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}({hint})")
                    });
                }
            }
            for v in map.values() {
                collect_tool_calls(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_tool_calls(v, out);
            }
        }
        _ => {}
    }
}

/// First string field of a tool `input` object (file path / pattern / command),
/// trimmed and char-truncated to a short hint. Empty when there is none.
pub(super) fn short_input_hint(input: &serde_json::Value) -> String {
    let Some(s) = input
        .as_object()
        .and_then(|o| o.values().find_map(serde_json::Value::as_str))
    else {
        return String::new();
    };
    let s = s.trim();
    if s.chars().count() > 40 {
        format!("{}\u{2026}", s.chars().take(40).collect::<String>())
    } else {
        s.to_string()
    }
}

/// settings/constants.ts:7-21). Used by [`PoolSubagentSpawner::resolve_selection`]
/// to emit `tengu_agent_tool_selected`'s `source` field byte-faithfully.
pub(crate) fn agent_source_to_claude_str(source: AgentSource) -> &'static str {
    match source {
        AgentSource::BuiltIn => "built-in",
        AgentSource::Plugin => "plugin",
        AgentSource::Settings(lingxi_core::types::SettingsScope::User) => "userSettings",
        AgentSource::Settings(lingxi_core::types::SettingsScope::Project) => "projectSettings",
        AgentSource::Settings(lingxi_core::types::SettingsScope::Managed) => "policySettings",
        // No loader produces a local-tier agent today. Named rather than caught
        // by `_` so that adding one is a decision here; the token is the
        // reference's own `SettingSource` spelling, already used by permission.
        AgentSource::Settings(lingxi_core::types::SettingsScope::Local) => "localSettings",
        AgentSource::Flag => "flagSettings",
        AgentSource::AdditionalDirectory => "additionalDirectory",
    }
}
