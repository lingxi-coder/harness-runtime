//! Forked prompt suggestions preserve isolated tool-denial retries and candidate order.

use async_trait::async_trait;
use lingxi_core::host::SourceToolWireDefinition;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use serde_json::json;
use sidequery::{
    CacheSafeParams, ForkedAgentRequest, ForkedAgentRunner, QuerySource, SideQueryClient,
    SideQueryError, SideQueryRequest, SideQueryResponse,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

struct Client {
    seen: Mutex<Vec<SideQueryRequest>>,
    responses: Mutex<VecDeque<Result<SideQueryResponse, SideQueryError>>>,
}
#[async_trait]
impl SideQueryClient for Client {
    async fn query(&self, request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        self.seen.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("fixture exhausted")
    }
}
fn response(text: Option<&str>, tool: bool) -> SideQueryResponse {
    SideQueryResponse {
        text: text.map(Utf16JsonProjection::from),
        structured: None,
        tool_calls: if tool {
            vec![Utf16JsonProjection::parse(
                r#"{"id":"toolu_requested","name":"Read","input":{"file_path":"/tmp/file"}}"#,
            )
            .unwrap()]
        } else {
            vec![]
        },
        usage: cost::Usage {
            tokens: cost::TokenUsage {
                input: 3,
                output: 2,
                ..Default::default()
            },
            ..Default::default()
        },
        stop_reason: Some(if tool { "tool_use" } else { "end_turn" }.into()),
        retry_count: 0,
    }
}
fn request() -> ForkedAgentRequest {
    ForkedAgentRequest {
        api_attempts_left: None,
        prompt_messages: vec![ConversationMessage::user(
            MessageId::new(),
            "predict my next prompt".into(),
        )],
        cache_safe_params: CacheSafeParams {
            model_source: None,
            system_prompt: Arc::new(Utf16JsonProjection::from("parent system")),
            system_prompt_input: None,
            user_context: HashMap::new(),
            system_context: HashMap::new(),
            user_context_message: None,
            tool_use_options: tool_api::context::ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: "parent-model".into(),
                model_profile: Some("parent-profile".into()),
                max_budget_nano_usd: None,
                mcp_clients: vec![],
                is_non_interactive_session: false,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            tools: vec![SourceToolWireDefinition {
                name: "Read".into(),
                description: "".into(),
                input_schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
                strict: false,
                defer_loading: false,
            }],
            effort: Some(json!("high")),
            fork_context_messages: vec![ConversationMessage::user(
                MessageId::new(),
                "parent history".into(),
            )],
            typed_attachment_ids: Default::default(),
            task_deliveries: Default::default(),
            transcript_path: None,
            generation: 1,
        },
        fork_label: "prompt_suggestion".into(),
        query_source: QuerySource::PromptSuggestion,
        max_output_tokens: Some(128),
    }
}
fn client(responses: Vec<Result<SideQueryResponse, SideQueryError>>) -> Arc<Client> {
    Arc::new(Client {
        seen: Mutex::new(vec![]),
        responses: Mutex::new(responses.into()),
    })
}
#[tokio::test]
async fn denied_tools_continue_in_isolation_and_keep_candidates_in_order() {
    let client = client(vec![
        Ok(response(Some("first candidate"), true)),
        Ok(response(Some("second candidate"), false)),
    ]);
    let runner = ForkedAgentRunner::new().with_side_query_client(client.clone(), "unused".into());
    let result = runner.run(request()).await.unwrap();
    assert_eq!(result.candidates, ["first candidate", "second candidate"]);
    assert_eq!(result.usage.tokens.input, 6);
    let seen = client.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].messages.len(), seen[0].messages.len() + 2);
    let ConversationMessage::User { content, .. } = seen[1].messages.last().unwrap() else {
        panic!()
    };
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
        ..
    } = &content[0]
    else {
        panic!()
    };
    let native: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/prompt_suggestion_query_2_1_296.json"
    ))
    .unwrap();
    assert_eq!(
        content.to_json_string().unwrap(),
        serde_json::to_string(&native["denial"]["message"]).unwrap()
    );
    assert_eq!(tool_use_id.as_str(), "toolu_requested");
    assert_eq!(*is_error, Some(true));
    assert_eq!(seen[1].model, "parent-model");
    assert_eq!(seen[1].profile.as_deref(), Some("parent-profile"));
    assert_eq!(seen[1].tools, seen[0].tools);
    assert_eq!(seen[1].effort, Some(json!("high")));
}
#[tokio::test]
async fn later_api_failure_does_not_discard_earlier_candidate() {
    let client = client(vec![
        Ok(response(Some("run tests"), true)),
        Err(SideQueryError::InvalidResponse("API error".into())),
    ]);
    let result = ForkedAgentRunner::new()
        .with_side_query_client(client, "parent".into())
        .run(request())
        .await
        .unwrap();
    assert_eq!(result.candidates, ["run tests"]);
}
#[tokio::test]
async fn repeated_denials_stop_at_native_fifty_turn_limit() {
    let native: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/prompt_suggestion_query_2_1_296.json"
    ))
    .unwrap();
    let max = native["maxTurns"].as_u64().unwrap() as usize;
    let client = client((0..max).map(|_| Ok(response(None, true))).collect());
    let result = ForkedAgentRunner::new()
        .with_side_query_client(client.clone(), "parent".into())
        .run(request())
        .await
        .unwrap();
    assert_eq!(client.seen.lock().unwrap().len(), max);
    assert!(result.candidates.is_empty());
}
