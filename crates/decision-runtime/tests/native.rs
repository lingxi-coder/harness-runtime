use async_trait::async_trait;
use decision_api::*;
use decision_runtime::*;
use futures::StreamExt;
use lingxi_llm_client::{
    protocol::LlmError,
    transport::{HttpRequest, StreamResponse, Transport},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
struct Mock {
    response: Value,
    calls: Mutex<Vec<HttpRequest>>,
    stalled: bool,
}
#[async_trait]
impl Transport for Mock {
    async fn send(&self, request: HttpRequest) -> Result<StreamResponse, LlmError> {
        self.calls.lock().unwrap().push(request);
        if self.stalled {
            futures::future::pending::<()>().await;
        }
        let bytes = serde_json::to_vec(&self.response).unwrap().into();
        Ok(StreamResponse {
            status: 200,
            headers: vec![],
            body: futures::stream::once(async move { Ok(bytes) }).boxed(),
        })
    }
}
fn request() -> Request {
    Request {
        model: ModelRef {
            backend: "native".into(),
            account: "test".into(),
            model: "test-model".into(),
        },
        images: Vec::new(),
        state_revision: 7,
        state: json!({"ticket":"refund"}),
        questions: BTreeMap::from([
            (
                "binary".into(),
                Question::Binary {
                    instructions: "Refund requested?".into(),
                },
            ),
            (
                "choice".into(),
                Question::Choice {
                    instructions: "Choose department".into(),
                    options: BTreeMap::from([
                        ("a".into(), "Billing".into()),
                        ("b".into(), "Other".into()),
                    ]),
                },
            ),
            (
                "score".into(),
                Question::Score {
                    instructions: "Urgency".into(),
                    levels: vec!["Low".into(), "High".into()],
                },
            ),
        ]),
    }
}
fn response() -> Value {
    json!({"model":"actual","usage":{"input_tokens":12},"answers":[
 {"name":"binary","type":"predicate","probability":0.8},
 {"name":"choice","type":"choice","choice":"a","confidence":0.7,"probabilities":[{"value":"b","probability":0.2},{"value":"a","probability":0.8}]},
 {"name":"score","type":"score","score":0.4,"probabilities":[{"value":1,"label":"High","probability":0.4},{"value":0,"label":"Low","probability":0.6}]}]})
}
#[tokio::test]
async fn native_wire_and_probabilities_are_preserved() {
    let mock = Arc::new(Mock {
        response: response(),
        calls: Mutex::new(vec![]),
        stalled: false,
    });
    let service = RemoteDecisionService {
        transport: mock.clone(),
        protocol: Protocol::OpenaiDecisions,
        endpoint: "http://localhost:8000/v1/decisions".into(),
        credential: None,
    };
    let result = service
        .evaluate(
            request(),
            InvocationContext {
                timeout: Duration::from_secs(1),
                cancellation: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.actual_model.as_deref(), Some("actual"));
    assert_eq!(result.usage.output_tokens, None);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].url.ends_with("/v1/decisions"));
    let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert_eq!(body["questions"][2]["levels"][0]["label"], "Low");
    assert!(body.get("response_format").is_none());
}
#[test]
fn invalid_or_extra_answers_and_wrong_types_are_rejected() {
    for change in 0..4 {
        let mut r = response();
        match change {
            0 => r["answers"][0]["probability"] = json!(1.2),
            1 => r["answers"][0]["type"] = json!("choice"),
            2 => r["answers"][1]["choice"] = json!("invented"),
            _ => {
                let duplicate = r["answers"][0].clone();
                r["answers"].as_array_mut().unwrap().push(duplicate)
            }
        };
        assert!(decode_response(&request(), Protocol::OpenaiDecisions, r, 0).is_err());
    }
}
#[test]
fn refusals_do_not_replace_other_answers() {
    let mut r = response();
    r["answers"][0] = json!({"name":"binary","type":"refusal"});
    let r = decode_response(&request(), Protocol::OpenaiDecisions, r, 0).unwrap();
    assert!(matches!(r.answers["binary"], Answer::Refused { .. }));
    assert!(matches!(r.answers["choice"], Answer::Choice { .. }));
}
#[test]
fn typesafe_uses_named_questions_and_noul() {
    let q = encode_questions(&request(), Protocol::TypesafeSystemone);
    assert_eq!(q["binary"]["type"], "noul");
    assert_eq!(q["choice"]["criteria"]["a"], "Billing");
    let r = json!({"model":"jev-test","answers":{"binary":{"type":"noul","noul":0.8},"choice":{"type":"choice","choice":"a","probabilities":{"a":0.7,"b":0.3}},"score":{"type":"score","score":0.5,"probabilities":{"0":0.5,"1":0.5}}}});
    assert!(decode_response(&request(), Protocol::TypesafeSystemone, r, 0).is_ok());
}
#[tokio::test]
async fn cancellation_and_deadline_do_not_replay() {
    for cancel in [true, false] {
        let mock = Arc::new(Mock {
            response: response(),
            calls: Mutex::new(vec![]),
            stalled: true,
        });
        let token = tokio_util::sync::CancellationToken::new();
        if cancel {
            token.cancel();
        }
        let service = RemoteDecisionService {
            transport: mock.clone(),
            protocol: Protocol::OpenaiDecisions,
            endpoint: "http://localhost/v1/decisions".into(),
            credential: None,
        };
        let result = service
            .evaluate(
                request(),
                InvocationContext {
                    timeout: Duration::from_millis(10),
                    cancellation: token,
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(Error::Cancelled) | Err(Error::Timeout)
        ));
        assert!(mock.calls.lock().unwrap().len() <= 1);
    }
}

#[test]
fn images_use_provider_native_shapes() {
    let mut request = request();
    request.images.push(Image {
        data_url: "data:image/png;base64,aGVsbG8=".into(),
    });
    let input = openai_input(&request);
    assert_eq!(input[0]["content"][1]["type"], "input_image");
    let (state, assets) = typesafe_input(&request);
    let reference = state["images"][0].as_str().unwrap();
    assert_eq!(assets[reference]["data_base64"], "aGVsbG8=");
}
