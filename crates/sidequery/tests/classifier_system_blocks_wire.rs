//! The background classifier retains native system block/cache boundaries.
use llm_runtime::test_support::FixtureTransport;
use llm_runtime::{BoxFuture, LlmError, ProviderRequest, ProviderResponse, StreamingResponse};
use serde_json::{json, Value};
use sidequery::{
    ForkedSideQueryOptions, ProviderSideQueryClient, QuerySource, SideQueryClient, SideQueryRequest,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Transport(Mutex<Vec<String>>);
impl FixtureTransport for Transport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.0
            .lock()
            .unwrap()
            .push(request.body_json.as_json_source().to_owned());
        Box::pin(async {
            Ok(ProviderResponse::json(
                200,
                json!({
                    "id":"classifier", "type":"message", "role":"assistant", "model":"claude-haiku-4-5",
                    "content":[{"type":"text","text":"first classification"},{"type":"text","text":"must not join"}],
                    "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":2}
                }),
            ))
        })
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async { panic!("background classifier is a utility request") })
    }
}
llm_runtime::impl_fixture_transport!(Transport);

struct CacheEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl CacheEnvironment {
    fn capture() -> Self {
        let keys = [
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
            "FORCE_PROMPT_CACHING_5M",
        ];
        Self(
            keys.into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        )
    }
}
impl Drop for CacheEnvironment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[tokio::test]
async fn classifier_keeps_each_system_cache_control_without_implicit_prompt_rewrites() {
    let _environment = CacheEnvironment::capture();
    std::env::remove_var("FORCE_PROMPT_CACHING_5M");
    let config: llm_runtime::ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party", "profile_name":"classifier", "protocol":"anthropic_messages",
        "base_url":"https://api.anthropic.com", "auth":"none", "credential":{"type":"none"},
        "models":[{"display_model":"claude-haiku-4-5", "request_model":"claude-haiku-4-5", "billing_model":"claude-haiku-4-5",
        "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]
    }]})).unwrap();
    let transport = Arc::new(Transport::default());
    let service = Arc::new(llm_runtime::ApiService::new_with_routing(
        Arc::new(llm_runtime::ModelRuntime::from_config(config).unwrap()),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        Some(0),
    ));
    let client = ProviderSideQueryClient::from_service(service);
    for (ttl_1h, global, addendum) in [
        (false, false, false),
        (true, false, true),
        (false, true, true),
        (true, true, true),
    ] {
        std::env::set_var(
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
            if global { "0" } else { "1" },
        );
        std::env::set_var(
            "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
            if ttl_1h { "1h" } else { "5m" },
        );
        // Caller only supplies block shape. The live provider policy owns TTL/scope.
        let control = llm_runtime::CacheControl::Ephemeral;
        let mut system_blocks = vec![llm_runtime::SystemBlock {
            text: "classifier\n\nexact system".into(),
            cache_control: Some(control),
        }];
        if addendum {
            system_blocks.push(llm_runtime::SystemBlock {
                text: "leading ask addendum".into(),
                cache_control: Some(control),
            });
        }
        let response = client
            .query_fork(
                SideQueryRequest {
                    model_source: None,
                    enable_prompt_caching: false,
                    model_attempt: None,
                    native_prompt_context: None,
                    model: "claude-haiku-4-5".into(),
                    profile: Some("classifier".into()),
                    system_prompt: None,
                    system_prompt_input: None,
                    typed_attachment_ids: HashSet::new(),
                    task_deliveries: HashMap::new(),
                    messages: vec![lingxi_core::types::ConversationMessage::user(
                        lingxi_core::types::MessageId::new(),
                        "transcript".into(),
                    )],
                    tools: vec![],
                    tool_choice: None,
                    output_format: None,
                    max_tokens: Some(1024),
                    max_retries: 0,
                    temperature: None,
                    thinking: Some(llm_runtime::model::thinking::ThinkingConfig::Disabled),
                    effort: None,
                    stop_sequences: vec![],
                    query_source: QuerySource::Custom("agent_classifier".into()),
                    skip_system_prompt_prefix: true,
                },
                ForkedSideQueryOptions {
                    system_blocks: Some(system_blocks.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .response
                .text
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some("first classification".encode_utf16().collect())
        );
        let mut cache = json!({"type":"ephemeral"});
        if ttl_1h {
            cache["ttl"] = json!("1h");
        }
        if global {
            cache["scope"] = json!("global");
        }
        let expected: Vec<_> = system_blocks
            .iter()
            .map(|b| {
                let text = String::from_utf16(&b.text.string_units("").unwrap())
                    .expect("classifier fixture system text contains only Unicode scalars");
                json!({"type":"text","text":text,"cache_control":cache})
            })
            .collect();
        let seen = transport.0.lock().unwrap();
        let wire_source = seen.last().unwrap();
        let wire: Value = serde_json::from_str(wire_source)
            .expect("captured provider request source is valid fixture JSON");
        assert_eq!(
            serde_json::to_vec(&wire["system"]).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(wire["model"], "claude-haiku-4-5");
        assert_eq!(wire["max_tokens"], 1024);
        assert_eq!(
            wire["messages"],
            json!([{"role":"user","content":"transcript"}])
        );
        assert!(wire["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(wire["thinking"], json!({"type":"disabled"}));
        assert!(wire.get("tools").is_none());
        assert!(wire.get("tool_choice").is_none());
        assert!(wire.get("temperature").is_none());
        assert_ne!(wire.get("stream"), Some(&json!(true)));
    }
}
