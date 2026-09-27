use llm_runtime::{validate_capabilities, Capabilities, ContentBlock, LlmRequest, Message};

#[test]
fn tool_declarations_require_tools_capability() {
    let mut request = LlmRequest::new("text-only").with_user_text("hi");
    request.tools = vec![llm_runtime::ToolDeclaration {
        name: "Read".to_string(),
        description: "d".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        ..Default::default()
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
    request.messages.push(llm_runtime::Message {
        role: "assistant".to_string(),
        content: vec![ContentBlock::Text {
            text: "ok".to_string(),
            cache_control: None,
        }],
    });

    let request = request.with_image("image/png", vec![1, 2, 3]);

    let last = request.messages.last().expect("messages");
    assert_eq!(last.role, "user");
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
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ImageUrl {
            url: "https://x/y.png".to_string(),
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
    request.reasoning = Some(llm_runtime::ReasoningConfig::Enabled {
        budget_tokens: 1024,
    });
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
