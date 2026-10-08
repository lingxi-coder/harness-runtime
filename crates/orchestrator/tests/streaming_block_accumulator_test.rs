//!
//! Each test asserts an invariant of the per-block state machine.
//! These tests fail to compile until Task 5 lands the implementation.

use lingxi_core::types::ToolUseId;
use orchestrator::sse::accumulator::{BlockAccumulator, BlockKind, CompletedBlock};
use orchestrator::sse::StreamingError;
use serde_json::json;

#[test]
fn text_block_round_trip() {
    let mut acc = BlockAccumulator::new();
    acc.start_block(
        0,
        BlockKind::Text {
            citations: None,
            utf16_code_units: None,
            initial_text: String::new(),
        },
    )
    .expect("start");
    acc.append_text(0, "hel").expect("append1");
    acc.append_text(0, "lo").expect("append2");
    let completed = acc.stop_block(0).expect("stop");
    match completed {
        CompletedBlock::Text { text, .. } => assert_eq!(text, "hello"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn tool_use_partial_json_reassembles() {
    let mut acc = BlockAccumulator::new();
    let tu_id = ToolUseId::new();
    acc.start_block(
        1,
        BlockKind::ToolUse { input_projection: None,
            id: tu_id.clone(),
            name: "Read".into(),
            provider_id: Some("toolu_01ACC".into()),
        },
    )
    .expect("start");
    acc.append_json(1, "{\"file").expect("append1");
    acc.append_json(1, "_path\":\"foo.rs\"}").expect("append2");
    let completed = acc.stop_block(1).expect("stop");
    match completed {
        CompletedBlock::ToolUse {
            id,
            name,
            input,
            provider_id,
         .. } => {
            assert_eq!(id, tu_id);
            assert_eq!(name, "Read");
            assert_eq!(input, json!({"file_path": "foo.rs"}));
            // P0: the verbatim provider id survives accumulation.
            assert_eq!(provider_id.as_deref(), Some("toolu_01ACC"));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn empty_tool_use_input_parses_as_empty_object() {
    // Some tools have schema { "type": "object", "properties": {} } and
    // the API streams ZERO input_json_delta events. claude-code
    // synthesizes `{}` in that case. Mirror.
    let mut acc = BlockAccumulator::new();
    acc.start_block(
        0,
        BlockKind::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "NoArgs".into(),
            provider_id: None,
        },
    )
    .expect("start");
    let completed = acc.stop_block(0).expect("stop");
    match completed {
        CompletedBlock::ToolUse { input, .. } => assert_eq!(input, json!({})),
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn delta_without_start_errors() {
    let mut acc = BlockAccumulator::new();
    let err = acc.append_text(0, "x").expect_err("no start");
    assert!(matches!(err, StreamingError::BlockNotFound { index: 0 }));
}

#[test]
fn double_stop_errors() {
    let mut acc = BlockAccumulator::new();
    acc.start_block(
        0,
        BlockKind::Text {
            citations: None,
            utf16_code_units: None,
            initial_text: String::new(),
        },
    )
    .expect("start");
    acc.stop_block(0).expect("first stop");
    let err = acc.stop_block(0).expect_err("second stop");
    assert!(matches!(err, StreamingError::DoubleStop { index: 0 }));
}

#[test]
fn type_mismatch_errors() {
    let mut acc = BlockAccumulator::new();
    acc.start_block(
        0,
        BlockKind::Text {
            citations: None,
            utf16_code_units: None,
            initial_text: String::new(),
        },
    )
    .expect("start");
    let err = acc.append_json(0, "{}").expect_err("mismatch");
    assert!(matches!(
        err,
        StreamingError::TypeMismatch {
            index: 0,
            expected: "text",
            got: "input_json_delta"
        }
    ));
}

#[test]
fn malformed_tool_use_json_errors_at_stop() {
    let mut acc = BlockAccumulator::new();
    acc.start_block(
        0,
        BlockKind::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "Bad".into(),
            provider_id: None,
        },
    )
    .expect("start");
    acc.append_json(0, "{not json").expect("append");
    let err = acc.stop_block(0).expect_err("parse fail");
    assert!(matches!(
        err,
        StreamingError::ToolUseJsonParse { index: 0, .. }
    ));
}
