//! Physical SDK dispatch through host projection with the server lane unarmed.
use async_trait::async_trait;
use futures::StreamExt;
use lingxi_llm_client::{HttpRequest, HttpResponse, StreamResponse};
use llm_runtime::history::ContentBlock;
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    ApiService, ClientConfig, LlmRequest, ModelRuntime, NonStreamingRequestClass,
    NonStreamingRetryOptions, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
const MODEL: &str = "claude-opus-4-7";
struct Capture {
    malformed: bool,
    requests: Mutex<Vec<Value>>,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: HttpRequest,
    ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        self.requests.lock().unwrap().push(body.clone());
        let block = if self.malformed {
            json!({"type":"fallback","from":{"model":MODEL},"to":{"model":""}})
        } else {
            json!({"type":"fallback","from":{"model":MODEL},"to":{"model":"served-target"},"trigger":{"type":"refusal","category":"cyber"}})
        };
        let usage = json!({"input_tokens":2,"output_tokens":1,"iterations":[{"type":"fallback_message","model":"first","input_tokens":1.5},{"type":"fallback_message","model":"","output_tokens":-1},{"type":"message","model":"unrelated","input_tokens":7}]});
        let bytes = if body["stream"] == true {
            [json!({"type":"message_start","message":{"id":"msg_control","model":MODEL,"usage":{"input_tokens":2,"output_tokens":0}}}),json!({"type":"content_block_start","index":-0.5,"content_block":block}),json!({"type":"content_block_delta","index":-0.5,"delta":{"type":"input_json_delta","partial_json":"bad"}}),json!({"type":"content_block_stop","index":-0.5}),json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),json!({"type":"content_block_stop","index":1}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage}),json!({"type":"message_stop"})].iter().map(|frame|format!("data: {frame}\n\n")).collect::<String>().into_bytes()
        } else {
            serde_json::to_vec(&json!({"id":"msg_control","model":MODEL,"content":[block,{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":usage})).unwrap()
        };
        if body["stream"] == true {
            let chunks = String::from_utf8(bytes)
                .unwrap()
                .split("\n\n")
                .filter(|frame| !frame.is_empty())
                .map(|frame| Ok(bytes::Bytes::from(format!("{frame}\n\n"))))
                .collect::<Vec<_>>();
            return Ok(StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::iter(chunks).boxed(),
            });
        }
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: bytes.into(),
        }
        .into())
    }
}
fn service(capture: Arc<Capture>, custom: bool) -> ApiService {
    let provider = if custom {
        json!({"custom":{"name":"gateway"}})
    } else {
        json!("anthropic_first_party")
    };
    let cfg:ClientConfig=serde_json::from_value(json!({"providers":[{"provider_id":provider,"profile_name":"direct","base_url":if custom {"https://gateway.invalid"}else{"https://api.anthropic.com"},"protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,"capabilities":{"streaming":true,"tools":true,"reasoning":true,"vision":false,"documents":false,"structured_output":false}}]}]})).unwrap();
    ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(cfg).unwrap()),
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "fixture",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
}
#[tokio::test]
async fn fallback_controls_reach_host_metadata_without_replay_or_model_adoption() {
    for custom in [false, true] {
        for malformed in [false, true] {
            for streaming in [false, true] {
                let capture = Arc::new(Capture {
                    malformed,
                    requests: Mutex::new(vec![]),
                });
                let api = service(capture.clone(), custom);
                let request = LlmRequest::new(MODEL).with_user_text("fixture");
                let response = if streaming {
                    llm_runtime::stream_accumulator::accumulate_stream_salvaging(
                        api.stream_request(request).await.unwrap(),
                    )
                    .await
                    .unwrap()
                } else {
                    api.execute_non_stream_request(
                        request,
                        NonStreamingRequestClass::Main,
                        NonStreamingRetryOptions::default(),
                    )
                    .await
                    .unwrap()
                };
                assert_eq!(capture.requests.lock().unwrap().len(), 1);
                assert_eq!(
                    response.model, MODEL,
                    "observation does not authorize a swap"
                );
                assert_eq!(response.content.len(), 1);
                assert!(
                    matches!(&response.content[0],ContentBlock::Text {text,..} if text=="answer")
                );
                let fallback = &response.provider_metadata["llm_client"]["anthropic_fallback"];
                assert_eq!(fallback["malformedBlocks"], usize::from(malformed));
                assert_eq!(
                    fallback["hops"].as_array().unwrap().len(),
                    usize::from(!malformed)
                );
                if !malformed {
                    assert_eq!(fallback["hops"][0]["model"], "served-target");
                    assert_eq!(fallback["hops"][0]["category"], "cyber");
                }
                assert_eq!(fallback["iterations"]["servedFallbackModel"], "");
                assert_eq!(fallback["iterations"]["entries"][0]["inputTokens"], 1.5);
                assert_eq!(
                    fallback["iterations"]["entries"][1]["outputTokens"].as_f64(),
                    Some(0.0)
                );
            }
        }
    }
}
