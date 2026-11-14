use super::*;
use std::collections::HashMap;

fn source(value: Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).unwrap()
}

#[test]
fn deferred_record_index_deduplicates_links_and_keeps_map_traversal_order() {
    let first_delta = MessageId::new().to_string();
    let second_delta = MessageId::new().to_string();
    let record_a = MessageId::new();
    let record_b = MessageId::new();
    let attachments = HashMap::from([
        (
            record_a,
            source(serde_json::json!({
                "type":"deferred_tools_record",
                "nameOnlyAnnouncements":[first_delta, second_delta, first_delta, null, 42]
            })),
        ),
        (
            record_b,
            source(serde_json::json!({
                "type":"deferred_tools_record",
                "nameOnlyAnnouncements":[first_delta, first_delta]
            })),
        ),
        (
            MessageId::new(),
            source(serde_json::json!({
                "type":"unrelated",
                "nameOnlyAnnouncements":[first_delta]
            })),
        ),
        (
            MessageId::new(),
            source(serde_json::json!({"type":"deferred_tools_record"})),
        ),
    ]);
    let expected_first = attachments
        .keys()
        .copied()
        .filter(|id| *id == record_a || *id == record_b)
        .collect::<Vec<_>>();
    let index = deferred_records_by_delta(&attachments);
    assert_eq!(index.len(), 2);
    assert_eq!(index[first_delta.as_str()], expected_first);
    assert_eq!(index[second_delta.as_str()], vec![record_a]);
}

#[test]
fn deferred_hot_replay_latches_persistence_only_multi_links_before_visible_deltas() {
    let first = MessageId::new();
    let second = MessageId::new();
    let final_delta = MessageId::new();
    let prior_record = MessageId::new();
    let future_record = MessageId::new();
    let attachments = HashMap::from([
        (
            first,
            source(serde_json::json!({
                "type":"deferred_tools_delta",
                "addedNames":["Alpha"], "addedLines":["Alpha"],
                "surfacedNames":["Alpha"],
                "pendingMcpServers":["z-pending","a-pending"],
                "needsAuthMcpServers":["auth"],
                "failedMcpServers":[{"name":"broken"}]
            })),
        ),
        (
            second,
            source(serde_json::json!({
                "type":"deferred_tools_delta",
                "addedNames":["Beta"], "addedLines":["Beta"],
                "removedNames":["Alpha"], "surfacedNames":["Beta"],
                "pendingMcpServers":[], "failedMcpServers":[]
            })),
        ),
        (
            final_delta,
            source(serde_json::json!({
                "type":"deferred_tools_delta",
                "addedNames":["Alpha","Gamma"], "addedLines":["Gamma"],
                "readdedNames":["Alpha"], "surfacedNames":["Gamma"],
                "pendingMcpServers":["last-pending"]
            })),
        ),
        (
            prior_record,
            source(serde_json::json!({
                "type":"deferred_tools_record",
                "nameOnlyAnnouncements":[first.to_string(),first.to_string(),second.to_string()]
            })),
        ),
        (
            future_record,
            source(serde_json::json!({
                "type":"deferred_tools_record",
                "nameOnlyAnnouncements":[first.to_string(),second.to_string(),second.to_string()]
            })),
        ),
    ]);

    // Cold replay includes persistence rows in transcript order. Hot history
    // omits them; only the captured sidecar map can establish their latches.
    // Exercise an already-replayed record as well as a record reached through
    // two deltas and later encountered at its ordinary transcript position.
    let ordered_ids = [prior_record, first, second, final_delta, future_record];
    let session_id = Uuid::new_v4();
    let cold_rows = ordered_ids
        .iter()
        .map(|id| {
            hook_attachment_row(
                session_id,
                id.as_uuid(),
                serde_json::from_str(&attachments[id].to_json_string().unwrap()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let hot_history = [first, second, final_delta]
        .into_iter()
        .map(|id| ConversationMessage::user(id, "rendered deferred state".into()))
        .collect::<Vec<_>>();

    for cold in [true, false] {
        for request_supported in [true, false] {
            let state = tool_api::DeferralState::new(tool_api::ToolSearchMode::Enabled, false);
            state.set_request_supported(request_supported);
            if cold {
                restore_deferred_tool_state_from_messages(&state, &cold_rows);
            } else {
                restore_deferred_tool_state_from_history(&state, &hot_history, &attachments);
            }
            let current = ["Alpha", "Beta", "Gamma"].map(str::to_owned);
            let status = tool_api::DeferredMcpServerStatus {
                pending: Some(vec!["last-pending".into()]),
                needs_auth: Some(vec!["auth".into()]),
                failed: Some(Vec::new()),
            };
            assert!(
                state
                    .compute_deferred_delta_with_mcp_status(&current, status.clone())
                    .is_empty(),
                "ordered removal/readd and the last represented MCP arrays must be restored"
            );
            let next = state.compute_deferred_delta_with_native_state(
                &current,
                status,
                tool_api::DeferredToolNativeContext {
                    current_tool_names: current.to_vec(),
                    declared_pool_available: true,
                    tool_search_absent: true,
                    ..Default::default()
                },
            );
            assert_eq!(next.added_lines, vec!["Alpha", "Beta"]);
            assert_eq!(
                next.surfaced_names,
                Some(vec!["Alpha".into(), "Beta".into()])
            );
            assert!(next.readded.is_empty());
            assert!(next.removed.is_empty());
            assert_eq!(state.declared_tool_names(), None);
            assert_eq!(state.request_supported(), request_supported);
        }
    }
}
