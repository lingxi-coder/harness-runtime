//! Regression tests for protocol events test.

use llm_runtime::{
    ContentBlock, ExecutionUsage, HistoryContentDelta, HistoryEvent, HistoryMessageDelta,
    HistoryResponse,
};

fn sample_response() -> HistoryResponse {
    HistoryResponse {
        id: "resp_1".to_string(),
        model: "model-a".to_string(),
        content: vec![],
        stop_reason: None,
        stop_details: None,
        usage: ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::json!({}),
    }
}

#[test]
fn stream_events_support_block_lifecycle_and_terminal_delta() {
    let start = HistoryEvent::MessageStart {
        response: Box::new(sample_response()),
    };
    let block_start = HistoryEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::Text {
            text: String::new(),
            cache_control: None,
            citations: None,
        },
    };
    let delta = HistoryEvent::ContentBlockDelta {
        index: 0,
        delta: HistoryContentDelta::TextDelta {
            text: "hi".to_string(),
        },
    };
    let stop = HistoryEvent::ContentBlockStop { index: 0 };
    let terminal = HistoryEvent::MessageDelta {
        delta: HistoryMessageDelta {
            stop_reason: Some("end_turn".to_string()),
            stop_details: None,
        },
        usage: None,
    };

    assert!(matches!(start, HistoryEvent::MessageStart { .. }));
    assert!(matches!(
        block_start,
        HistoryEvent::ContentBlockStart { .. }
    ));
    assert!(matches!(delta, HistoryEvent::ContentBlockDelta { .. }));
    assert!(matches!(stop, HistoryEvent::ContentBlockStop { .. }));
    assert!(matches!(terminal, HistoryEvent::MessageDelta { .. }));
}

#[test]
fn stream_event_json_has_expected_shape_and_round_trips() {
    let start = HistoryEvent::MessageStart {
        response: Box::new(sample_response()),
    };
    let block_start = HistoryEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::Text {
            text: String::new(),
            cache_control: None,
            citations: None,
        },
    };
    let delta = HistoryEvent::ContentBlockDelta {
        index: 0,
        delta: HistoryContentDelta::InputJsonDelta {
            partial_json: "{\"a\":1".to_string(),
        },
    };
    let stop = HistoryEvent::ContentBlockStop { index: 0 };
    let terminal = HistoryEvent::MessageDelta {
        delta: HistoryMessageDelta {
            stop_reason: Some("end_turn".to_string()),
            stop_details: None,
        },
        usage: Some(ExecutionUsage::default()),
    };

    let start_json = serde_json::json!({
        "type": "message_start",
        "response": {
            "id": "resp_1",
            "model": "model-a",
            "content": [],
            "usage": ExecutionUsage::default(),
            "cost": null,
            "provider_metadata": {},
        }
    });
    let block_start_json = serde_json::json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "text", "text": ""}
    });
    let delta_json = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": "{\"a\":1"}
    });
    let stop_json = serde_json::json!({
        "type": "content_block_stop",
        "index": 0
    });
    let terminal_json = serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": ExecutionUsage::default(),
    });

    assert_eq!(serde_json::to_value(&start).unwrap(), start_json);
    assert_eq!(
        serde_json::to_value(&block_start).unwrap(),
        block_start_json
    );
    assert_eq!(serde_json::to_value(&delta).unwrap(), delta_json);
    assert_eq!(serde_json::to_value(&stop).unwrap(), stop_json);
    assert_eq!(serde_json::to_value(&terminal).unwrap(), terminal_json);

    let round_trip: HistoryEvent = serde_json::from_value(terminal_json).unwrap();
    assert!(matches!(round_trip, HistoryEvent::MessageDelta { .. }));
}
