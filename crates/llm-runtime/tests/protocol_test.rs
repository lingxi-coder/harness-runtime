//! Regression tests for protocol test.

use lingxi_llm_client::protocol::{
    ContentBlock, ConversationMessage, ImageSource, MessageRole, ToolSpec,
};
use llm_runtime::{validate_capabilities, Capabilities, LlmRequest};

#[test]
fn tool_declarations_require_tools_capability() {
    let mut request = LlmRequest::new("text-only").with_user_text("hi");
    request.input.tools = vec![ToolSpec {
        input_schema_json: None,
        name: "Read".to_string(),
        description: "d".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        strict: false,
        defer_loading: false,
        native_options: vec![],
        tool_type: None,
        extra: serde_json::Value::Null,
    }];
    let capabilities = Capabilities {
        streaming: true,
        tools: false,
        ..Default::default()
    };

    let error = validate_capabilities(&request, capabilities).expect_err("tools should fail");

    assert!(matches!(
        error,
        llm_runtime::LlmError::UnsupportedCapability { capability } if capability == "tools"
    ));
}

#[test]
fn with_image_attaches_to_last_user_message_or_starts_one() {
    let mut request = LlmRequest::new("vision-model").with_user_text("look at this");
    request.input.messages.push(ConversationMessage {
        role: MessageRole::Assistant,
        native_options: vec![],
        content: vec![ContentBlock::Text {
            text: "ok".to_string(),
            thought_signature: None,
            citations: None,
        }],
    });

    let request = request.with_image("image/png", vec![1, 2, 3]);

    let last = request.input.messages.last().expect("messages");
    assert_eq!(last.role, MessageRole::User);
    assert!(matches!(
        last.content.as_slice(),
        [ContentBlock::Image { .. }]
    ));
}

#[test]
fn unsupported_capabilities_fail_before_transport() {
    let request = LlmRequest::new("text-only")
        .with_user_text("describe")
        .with_image("image/png", vec![1, 2, 3]);
    let capabilities = Capabilities {
        streaming: true,
        tools: true,
        vision: false,
        documents: false,
        reasoning: false,
        structured_output: false,
    };

    let error = validate_capabilities(&request, capabilities).expect_err("vision should fail");

    assert!(matches!(
        error,
        llm_runtime::LlmError::UnsupportedCapability { capability } if capability == "vision"
    ));
}

#[test]
fn image_url_block_requires_vision_capability() {
    let mut request = LlmRequest::new("m");
    request.input.messages.push(ConversationMessage {
        role: MessageRole::User,
        native_options: vec![],
        content: vec![ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://x/y.png".to_string(),
            },
        }],
    });
    let capabilities = Capabilities {
        streaming: true,
        tools: true,
        vision: false,
        ..Default::default()
    };

    let error = validate_capabilities(&request, capabilities).expect_err("vision should fail");

    assert!(matches!(
        error,
        llm_runtime::LlmError::UnsupportedCapability { capability } if capability == "vision"
    ));
}

#[test]
fn reasoning_config_requires_reasoning_capability() {
    let mut request = LlmRequest::new("m").with_user_text("hi");
    request.set_reasoning(Some(llm_runtime::ReasoningConfig::Enabled {
        budget_tokens: 1024,
    }));
    let capabilities = Capabilities {
        streaming: true,
        tools: true,
        reasoning: false,
        ..Default::default()
    };

    let error = validate_capabilities(&request, capabilities).expect_err("reasoning should fail");

    assert!(matches!(
        error,
        llm_runtime::LlmError::UnsupportedCapability { capability } if capability == "reasoning"
    ));
}
