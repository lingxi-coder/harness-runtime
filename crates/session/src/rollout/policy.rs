//! Which rollout items get persisted — faithful port of codex's `policy.rs`.
//!
//! Codex matches on the structured `ResponseItem` / `EventMsg` enums. LingXi
//! carries those as opaque [`Value`]s, so the persist decision inspects the
//! inner `"type"` discriminant — reproducing codex's allow/deny lists exactly.

use crate::rollout::record::RolloutItem;
use serde_json::Value;

/// Whether a rollout `item` should be persisted in rollout files.
pub fn is_persisted_rollout_item(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::ResponseItem(item) => should_persist_response_item(item),
        RolloutItem::InterAgentCommunication(_) => true,
        RolloutItem::EventMsg(ev) => should_persist_event_msg(ev),
        // Persist executive markers so flows (compaction, turns) stay analyzable.
        RolloutItem::Compacted(_) | RolloutItem::TurnContext(_) | RolloutItem::SessionMeta(_) => {
            true
        }
    }
}

/// Return the canonical rollout items that should be persisted for a live append.
pub fn persisted_rollout_items(items: &[RolloutItem]) -> Vec<RolloutItem> {
    items
        .iter()
        .filter(|item| is_persisted_rollout_item(item))
        .cloned()
        .collect()
}

/// Inner `"type"` of an opaque payload, if any.
fn payload_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

/// Whether a `ResponseItem` (opaque) should be persisted. Mirrors codex's
/// `should_persist_response_item` allow/deny lists keyed on the inner `type`.
#[inline]
pub fn should_persist_response_item(item: &Value) -> bool {
    match payload_type(item) {
        Some(
            "message"
            | "agent_message"
            | "reasoning"
            | "local_shell_call"
            | "function_call"
            | "tool_search_call"
            | "function_call_output"
            | "tool_search_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "web_search_call"
            | "image_generation_call"
            | "compaction"
            | "context_compaction",
        ) => true,
        Some("additional_tools" | "compaction_trigger" | "other") => false,
        // Unknown/legacy response items default to persisted so resume keeps them.
        _ => true,
    }
}

/// Whether an `EventMsg` (opaque) should be persisted. Mirrors codex's
/// `should_persist_event_msg` keyed on the inner `type`.
#[inline]
pub fn should_persist_event_msg(ev: &Value) -> bool {
    match payload_type(ev) {
        Some(
            "user_message"
            | "agent_message"
            | "agent_reasoning"
            | "agent_reasoning_raw_content"
            | "patch_apply_end"
            | "token_count"
            | "thread_goal_updated"
            | "context_compacted"
            | "entered_review_mode"
            | "exited_review_mode"
            | "mcp_tool_call_end"
            | "thread_rolled_back"
            | "turn_aborted"
            | "turn_started"
            | "turn_complete"
            | "web_search_end"
            | "image_generation_end"
            | "sub_agent_activity",
        ) => true,
        // ItemCompleted persists only for Plan / Sleep items.
        Some("item_completed") => matches!(
            ev.get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str),
            Some("plan" | "sleep")
        ),
        _ => false,
    }
}
