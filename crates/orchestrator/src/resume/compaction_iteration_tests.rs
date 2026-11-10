use super::*;
use serde_json::json;

fn boundary(session: Uuid) -> JsonlMessage {
    serde_json::from_value(json!({
        "type":"system", "subtype":"compact_boundary", "uuid":Uuid::new_v4(),
        "parentUuid":null, "sessionId":session, "message":{},
        "compactMetadata":{"preTokens":1000,"postTokens":400}
    }))
    .unwrap()
}

fn assistant(session: Uuid, response: Option<&str>, logical: Option<&str>) -> JsonlMessage {
    let mut row = json!({
        "type":"assistant", "uuid":Uuid::new_v4(), "parentUuid":null,
        "sessionId":session, "message":{"role":"assistant","content":"block"}
    });
    if let Some(response) = response {
        row["message"]["id"] = json!(response);
    }
    if let Some(logical) = logical {
        row["logicalMessageId"] = json!(logical);
    }
    serde_json::from_value(row).unwrap()
}

#[test]
fn block_rows_count_once_on_both_sides_of_compaction_boundaries() {
    for logical in [None, Some("host-logical-response")] {
        let session = Uuid::new_v4();
        let mut rows = vec![boundary(session)];
        rows.extend((0..3).map(|_| assistant(session, Some("response-before"), logical)));
        rows.push(boundary(session));
        rows.extend((0..3).map(|_| {
            assistant(
                session,
                Some("response-after"),
                logical.map(|_| "host-logical-after"),
            )
        }));
        let state = runtime_metadata_from_messages(&rows).compaction_tracking;
        assert_eq!(state.turn_counter, 1);
        assert_eq!(state.consecutive_rapid_refills, 1);
        assert_eq!(compaction::thresholds::rapid_refill_count(&state), 2);
    }
}

#[test]
fn different_responses_and_legacy_rows_keep_distinct_iterations() {
    let session = Uuid::new_v4();
    let mut rows = vec![boundary(session)];
    rows.extend([
        assistant(session, Some("first"), None),
        assistant(session, Some("second"), None),
        assistant(session, None, None),
    ]);
    rows.push(boundary(session));
    rows.extend([
        assistant(session, None, None),
        assistant(session, None, None),
    ]);
    let state = runtime_metadata_from_messages(&rows).compaction_tracking;
    assert_eq!(state.turn_counter, 2);
    assert_eq!(state.consecutive_rapid_refills, 0);
}

#[test]
fn partial_logical_metadata_still_groups_rows_by_the_shared_response() {
    let session = Uuid::new_v4();
    let rows = [
        assistant(session, Some("response"), Some("logical")),
        assistant(session, Some("response"), None),
        assistant(session, Some("response"), None),
    ];
    assert_eq!(assistant_iteration_count(&rows), 1);
}
