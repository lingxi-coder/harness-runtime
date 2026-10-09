//! F1-02 — `MessageDto` + `MessageBlockDto` shared block-schema tests.
//!
//! `MessageDto` is the first-class block container reproduced by both
//! `MessageComplete` (F1-03) and a resumed scrollback. It must carry the full
//! block set the TUI scrollback renders so those surfaces are reproducible
//! (plan F1-02). `input_json`/`result_json` are JSON **Strings** (§0.4); the
//! diff fields (`old_string`/`new_string`/`file_path`) mirror `UserToolResult`
//! at `tui/src/state.rs:73-89`.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate never depends on
//! `serde_json::Value`.

use client::protocol::message::{MessageBlockDto, MessageDto};

/// A full multi-block assistant message round-trips byte-stable, one of each
/// block kind.
#[test]
fn message_dto_round_trips() {
    let msg = MessageDto {
        loop_wakeup: None,
        visualization_context: None,
        role: "assistant".to_string(),
        blocks: vec![
            MessageBlockDto::Text {
                text: "thinking out loud".to_string(),
            },
            MessageBlockDto::Thinking {
                thinking: "internal".to_string(),
                signature: Some("sig".to_string()),
            },
            MessageBlockDto::RedactedThinking {
                data: "redacted-blob".to_string(),
            },
            MessageBlockDto::CompactBoundary {
                messages_before: 12,
                messages_after: 3,
                summary: "hidden summary".to_string(),
            },
            MessageBlockDto::ToolUse {
                id: "tu_01".to_string(),
                tool: "Edit".to_string(),
                input_json: r#"{"file_path":"/tmp/x"}"#.to_string(),
                header: None,
            },
            MessageBlockDto::ToolResult {
                id: "tu_01".to_string(),
                tool: "Edit".to_string(),
                result_json: r#"{"ok":true}"#.to_string(),
                is_error: false,
                old_string: Some("before".to_string()),
                new_string: Some("after".to_string()),
                file_path: Some("/tmp/x".to_string()),
                display: None,
            },
        ],
        images: Vec::new(),
    };

    let json = serde_json::to_value(&msg).expect("serialize MessageDto");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["blocks"][0]["type"], "text");
    assert_eq!(json["blocks"][1]["type"], "thinking");
    assert_eq!(json["blocks"][2]["type"], "redacted_thinking");
    assert_eq!(json["blocks"][3]["type"], "compact_boundary");
    assert_eq!(json["blocks"][4]["type"], "tool_use");
    assert_eq!(json["blocks"][5]["type"], "tool_result");
    // Tool payloads are JSON Strings, not nested objects.
    assert!(json["blocks"][4]["input_json"].is_string());
    assert!(json["blocks"][5]["result_json"].is_string());

    let back: MessageDto = serde_json::from_value(json).expect("deserialize MessageDto");
    assert_eq!(back, msg);
}

/// The block-kind set equals the kinds the TUI scrollback renders:
/// `Text | Thinking | RedactedThinking | CompactBoundary | ToolUse |
/// ToolResult`. Structural parity anchor (plan F1-02).
#[test]
fn message_block_set_matches_tui_scrollback() {
    // Construct one of every block kind and collect the snake_case wire tags.
    let blocks = [
        MessageBlockDto::Text {
            text: String::new(),
        },
        MessageBlockDto::Thinking {
            thinking: String::new(),
            signature: None,
        },
        MessageBlockDto::RedactedThinking {
            data: String::new(),
        },
        MessageBlockDto::CompactBoundary {
            messages_before: 0,
            messages_after: 0,
            summary: String::new(),
        },
        MessageBlockDto::ToolUse {
            id: String::new(),
            tool: String::new(),
            input_json: "null".to_string(),
            header: None,
        },
        MessageBlockDto::ToolResult {
            id: String::new(),
            tool: String::new(),
            result_json: "null".to_string(),
            is_error: false,
            old_string: None,
            new_string: None,
            file_path: None,
            display: None,
        },
    ];

    let tags: Vec<String> = blocks
        .iter()
        .map(|b| {
            serde_json::to_value(b).expect("serialize block")["type"]
                .as_str()
                .expect("tag is a string")
                .to_string()
        })
        .collect();

    assert_eq!(
        tags,
        vec![
            "text",
            "thinking",
            "redacted_thinking",
            "compact_boundary",
            "tool_use",
            "tool_result"
        ],
        "MessageBlockDto kind set must match the TUI scrollback block kinds"
    );
}

/// The diff fields on `ToolResult` are optional and skipped when `None`
/// (forward-compat convention; non-diff tools omit them).
#[test]
fn tool_result_diff_fields_skip_when_none() {
    let block = MessageBlockDto::ToolResult {
        id: "tu_01".to_string(),
        tool: "Read".to_string(),
        result_json: "null".to_string(),
        is_error: false,
        old_string: None,
        new_string: None,
        file_path: None,
        display: None,
    };
    let json = serde_json::to_value(&block).expect("serialize ToolResult");
    assert!(json.get("old_string").is_none());
    assert!(json.get("new_string").is_none());
    assert!(json.get("file_path").is_none());
    // is_error is NOT optional — it is always present.
    assert_eq!(json["is_error"], false);
    let back: MessageBlockDto = serde_json::from_value(json).expect("deserialize ToolResult");
    assert_eq!(back, block);
}

#[test]
fn loop_wakeup_metadata_is_additive_and_round_trips() {
    let old: MessageDto = serde_json::from_str(r#"{"role":"assistant","blocks":[]}"#).unwrap();
    assert!(old.loop_wakeup.is_none());
    let dto = MessageDto {
        role: "system".into(),
        blocks: vec![],
        images: vec![],
        loop_wakeup: Some(client::protocol::message::LoopWakeupDto {
            message: "fire".into(),
            companion: Some("healthy".into()),
            streak: 1,
            since_ms: 42,
        }),
        visualization_context: None,
    };
    let bytes = serde_json::to_string(&dto).unwrap();
    assert_eq!(serde_json::from_str::<MessageDto>(&bytes).unwrap(), dto);
    let golden = include_str!("../snapshots/message_loop_wakeup.json").trim();
    assert_eq!(serde_json::to_string_pretty(&dto).unwrap(), golden);
}

#[test]
fn visualization_block_and_context_are_additive_and_round_trip() {
    use client::protocol::message::{VisualizationContextDto, VisualizationRefDto};
    let block = MessageBlockDto::Visualization {
        reference: Some(VisualizationRefDto {
            id: "chart".into(),
            revision: 2,
        }),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"type": "visualization", "reference": {"id": "chart", "revision": 2}})
    );
    assert_eq!(
        serde_json::from_value::<MessageBlockDto>(json).unwrap(),
        block
    );
    let unavailable = MessageBlockDto::Visualization { reference: None };
    assert_eq!(
        serde_json::to_value(&unavailable).unwrap(),
        serde_json::json!({"type": "visualization"})
    );
    let old: MessageDto = serde_json::from_str(r#"{"role":"user","blocks":[]}"#).unwrap();
    assert!(old.visualization_context.is_none());
    let dto = MessageDto {
        role: "user".into(),
        blocks: vec![],
        images: vec![],
        loop_wakeup: None,
        visualization_context: Some(VisualizationContextDto {
            id: "chart".into(),
            revision: 2,
            title: "Sales".into(),
        }),
    };
    let bytes = serde_json::to_string(&dto).unwrap();
    assert!(bytes.contains(
        "\"visualization_context\":{\"id\":\"chart\",\"revision\":2,\"title\":\"Sales\"}"
    ));
    assert_eq!(serde_json::from_str::<MessageDto>(&bytes).unwrap(), dto);
}
