//! Replay the independently executed native codec and driver fixture.
use async_trait::async_trait;
use lingxi_core::host::mcp_result::*;
use serde_json::{json, Map, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/input_required_2_1_286.json")).unwrap()
}
fn decoded(value: Result<DecodedMcpResult, McpSdkError>) -> Value {
    match value {
        Ok(DecodedMcpResult::Complete(result)) => json!({"kind":"complete","result":result}),
        Ok(DecodedMcpResult::Task(result)) => json!({"kind":"task","result":result}),
        Ok(DecodedMcpResult::InputRequired {
            input_requests,
            request_state,
        }) => {
            let mut out = json!({"kind":"input_required","inputRequests":input_requests});
            if let Some(state) = request_state {
                out["requestState"] = json!(state);
            }
            out
        }
        Err(error) => {
            let mut out = json!({"kind":"invalid","error":{"name":"SdkError","code":error.code,"message":error.message}});
            if let Some(data) = error.data {
                out["error"]["data"] = data;
            }
            out
        }
    }
}
#[test]
fn native_codec_fixture_matches_result_and_error_data() {
    for (index, case) in fixture()["decode_cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let payload = case
            .get("raw_json")
            .and_then(Value::as_str)
            .map(|raw| serde_json::from_str(raw).unwrap())
            .unwrap_or_else(|| case["payload"].clone());
        let mut actual = decoded(decode_modern_result(
            case["method"].as_str().unwrap(),
            payload,
        ));
        let mut expected = case["expected"].clone();
        if index >= 18 && actual["kind"] == "invalid" {
            // The existing codec cases lock exact SDK strings; these additional
            // native schema cases lock acceptance, code and structured data.
            actual["error"].as_object_mut().unwrap().remove("message");
            expected["error"].as_object_mut().unwrap().remove("message");
        }
        assert_eq!(actual, expected, "{}", case["name"]);
    }
}
#[test]
fn native_input_request_schema_and_trusted_ids() {
    for case in fixture()["schema_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["schema"] == "InputRequestSchema")
    {
        assert_eq!(
            validate_input_request_schema(&case["payload"]),
            case["expected"]["valid"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
    let req = validate_embedded_request(
        "trusted-key",
        &json!({"jsonrpc":"bad","id":"forged","method":"roots/list"}),
    )
    .unwrap();
    assert_eq!(req.id, jsonrpc::Id::String("trusted-key".into()));
    assert_eq!(req.jsonrpc, "2.0");
}
#[test]
fn native_result_and_response_union_schemas() {
    for case in fixture()["schema_cases"].as_array().unwrap() {
        let valid = match case["schema"].as_str().unwrap() {
            "InputRequiredResultSchema" => validate_input_required_schema(&case["payload"]),
            "InputResponseSchema" => validate_input_response_schema(&case["payload"]),
            _ => continue,
        };
        assert_eq!(
            valid,
            case["expected"]["valid"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
}
#[test]
fn native_integer_object_keys_and_original_params() {
    for name in ["integer-index-input-key-order", "non-index-input-key-order"] {
        let f = fixture();
        let case = f["flow_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let keys = javascript_object_keys(
            case["server_results"][0]["inputRequests"]
                .as_object()
                .unwrap(),
        );
        let expected: Vec<String> = case["expected"]["handler_events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"] == "start")
            .map(|e| e["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(keys, expected);
    }
    assert_eq!(
        rebuild_input_params(
            &json!({"inputResponses":{"old":1},"requestState":"initial"}),
            Map::new(),
            None
        ),
        json!({"inputResponses":{"old":1},"requestState":"initial"})
    );
}
struct ReplayIo {
    delay: Duration,
    frames: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
}
#[async_trait]
impl McpInputRequiredIo for ReplayIo {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _options: &McpInputRequiredOptions,
    ) -> Result<Value, McpResultError> {
        self.calls.lock().unwrap().push((method.to_owned(), params));
        Ok(self
            .frames
            .lock()
            .unwrap()
            .pop_front()
            .expect("fixture reply"))
    }
    async fn dispatch_local(
        &self,
        key: &str,
        request: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, McpResultError> {
        validate_embedded_request(key, &request)?;
        tokio::time::sleep(self.delay).await;
        Ok(json!({"roots":[]}))
    }
}
#[tokio::test(start_paused = true)]
async fn native_multi_round_request_replay_preserves_method_and_replaces_answers() {
    for name in [
        "state-only-250ms",
        "resources-read-method-preserved",
        "two-round-answer-replacement",
        "round-state-is-not-accumulated",
        "default-ten-round-limit",
        "configured-two-round-limit",
        "auto-fulfilment-disabled",
        "state-only-total-timeout",
        "handler-time-consumes-total-budget",
        "task-result-remains-disabled",
    ] {
        let f = fixture();
        let case = f["flow_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let io = ReplayIo {
            delay: Duration::from_millis(
                case["handlers"]["roots/list"]["delay"]
                    .as_u64()
                    .unwrap_or(0),
            ),
            frames: Mutex::new(
                case["server_results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
        };
        let progress = Arc::new(Mutex::new(Vec::new()));
        let p = progress.clone();
        let options = McpInputRequiredOptions {
            on_progress: case["progress_enabled"]
                .as_bool()
                .unwrap_or(false)
                .then(|| {
                    Arc::new(move |event: McpResultProgress| {
                        let mut value =
                            json!({"progress":event.progress as u64,"message":event.message});
                        if let Some(total) = event.total {
                            value["total"] = json!(total);
                        }
                        p.lock().unwrap().push(value);
                    }) as McpResultProgressCallback
                }),
            auto_fulfill: case["input_required_config"]["autoFulfill"]
                .as_bool()
                .unwrap_or(true),
            max_rounds: case["input_required_config"]["maxRounds"]
                .as_u64()
                .unwrap_or(10) as usize,
            max_total_timeout: case["options"]["maxTotalTimeout"]
                .as_u64()
                .map(Duration::from_millis),
            ..Default::default()
        };
        let start = tokio::time::Instant::now();
        let outcome = drive_modern_request(
            &io,
            case["original_request"]["method"].as_str().unwrap(),
            case["original_request"]["params"].clone(),
            options,
        )
        .await;
        if let Some(expected) = case["expected"].get("result") {
            assert_eq!(outcome.unwrap(), *expected, "{name}");
        } else {
            let McpResultError::Sdk(error) = outcome.unwrap_err() else {
                panic!("SDK error expected")
            };
            assert_eq!(
                error.code,
                case["expected"]["error"]["code"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                error.data,
                case["expected"]["error"].get("data").cloned(),
                "{name}"
            );
        }
        assert_eq!(
            *progress.lock().unwrap(),
            case["expected"]["progress"].as_array().unwrap().clone(),
            "{name}"
        );
        if name.starts_with("state-only") || name == "handler-time-consumes-total-budget" {
            assert_eq!(
                start.elapsed().as_millis(),
                case["expected"]["elapsed_ms"].as_u64().unwrap() as u128
            );
        }
        let expected = case["expected"]["requests"].as_array().unwrap();
        let actual = io.calls.lock().unwrap();
        assert_eq!(actual.len(), expected.len(), "{name}");
        for ((method, params), expected) in actual.iter().zip(expected) {
            assert_eq!(method, expected["request"]["method"].as_str().unwrap());
            let mut expected_params = expected["request"]["params"].clone();
            if let Some(meta) = expected_params
                .get_mut("_meta")
                .and_then(Value::as_object_mut)
            {
                meta.remove("progressToken");
                for key in [
                    "io.modelcontextprotocol/protocolVersion",
                    "io.modelcontextprotocol/clientInfo",
                    "io.modelcontextprotocol/clientCapabilities",
                ] {
                    meta.remove(key);
                }
            }
            if expected_params
                .get("_meta")
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty)
                && case["original_request"]["params"].get("_meta").is_none()
            {
                expected_params.as_object_mut().unwrap().remove("_meta");
            }
            assert_eq!(*params, expected_params, "{name}");
        }
    }
}

fn connection() -> (
    Arc<jsonrpc::Connection>,
    tokio::sync::mpsc::Sender<bytes::Bytes>,
    tokio::sync::mpsc::Receiver<bytes::Bytes>,
) {
    let (input, rx) = tokio::sync::mpsc::channel(32);
    let (tx, output) = tokio::sync::mpsc::channel(32);
    (
        Arc::new(jsonrpc::Connection::new_streams(
            rx,
            tx,
            jsonrpc::Mode::Lines,
        )),
        input,
        output,
    )
}
struct RecordingHandler {
    ids: Arc<Mutex<Vec<String>>>,
    delay: Duration,
    result: Value,
    pending: Arc<std::sync::atomic::AtomicUsize>,
}
struct Pending(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
#[async_trait]
impl jsonrpc::InboundHandler for RecordingHandler {
    async fn handle(&self, req: jsonrpc::Request) -> jsonrpc::Response {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _pending = Pending(self.pending.clone());
        self.ids.lock().unwrap().push(match &req.id {
            jsonrpc::Id::String(s) => s.clone(),
            _ => panic!("embedded original key"),
        });
        tokio::time::sleep(self.delay).await;
        if self.result.get("fail").is_some() {
            jsonrpc::Response::error(
                Some(req.id),
                jsonrpc::ResponseError {
                    code: -32000,
                    message: "handler failed".into(),
                    data: None,
                },
            )
        } else {
            jsonrpc::Response::success(req.id, self.result.clone())
        }
    }
}
#[tokio::test(start_paused = true)]
async fn actual_local_registry_parallel_failure_cancels_siblings_without_wire_calls() {
    let (conn, _input, mut output) = connection();
    let ids = Arc::new(Mutex::new(Vec::new()));
    let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for (method, delay, result) in [
        ("roots/list", Duration::from_millis(5), json!({"fail":true})),
        (
            "sampling/createMessage",
            Duration::from_millis(100),
            json!({"role":"assistant","model":"m","content":{"type":"text","text":"done"}}),
        ),
    ] {
        conn.register_handler(
            method,
            Arc::new(RecordingHandler {
                ids: ids.clone(),
                delay,
                result,
                pending: pending.clone(),
            }),
        )
        .await;
    }
    let io = JsonrpcMcpResultIo {
        connection: conn,
        client_capabilities: json!({"roots":{},"sampling":{}}),
    };
    let scope = CancellationToken::new();
    let keys = [
        ("fail", json!({"method":"roots/list"})),
        (
            "wait",
            json!({"method":"sampling/createMessage","params":{"messages":[],"maxTokens":1}}),
        ),
    ];
    let started = tokio::time::Instant::now();
    let futures = keys.map(|(key, value)| io.dispatch_local(key, value, scope.clone()));
    let joined = async { futures_util::future::try_join_all(futures).await };
    assert!(matches!(joined.await, Err(McpResultError::Local(_))));
    scope.cancel();
    assert_eq!(started.elapsed(), Duration::from_millis(5));
    assert_eq!(ids.lock().unwrap().len(), 2);
    assert_eq!(pending.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(output.try_recv().is_err());
}
#[tokio::test(start_paused = true)]
async fn actual_wire_progress_resets_timeout_and_timeout_cancels_assigned_id() {
    for respond in [true, false] {
        let (conn, input, mut output) = connection();
        let peer = tokio::spawn(async move {
            let frame = output.recv().await.unwrap();
            let request: Value = serde_json::from_slice(&frame).unwrap();
            assert_eq!(request["params"]["_meta"]["progressToken"], request["id"]);
            tokio::time::sleep(Duration::from_millis(30)).await;
            input.send(bytes::Bytes::from(format!("{}\n",json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":request["id"],"progress":1}})))).await.unwrap();
            if respond {
                tokio::time::sleep(Duration::from_millis(40)).await;
                input.send(bytes::Bytes::from(format!("{}\n",json!({"jsonrpc":"2.0","id":request["id"],"result":{"resultType":"complete","content":[]}})))).await.unwrap();
            } else {
                let frame = output.recv().await.unwrap();
                let cancel: Value = serde_json::from_slice(&frame).unwrap();
                assert_eq!(cancel["method"], "notifications/cancelled");
                assert_eq!(cancel["params"]["requestId"], request["id"]);
            }
        });
        let io = JsonrpcMcpResultIo {
            connection: conn,
            client_capabilities: json!({}),
        };
        let progress = Arc::new(Mutex::new(Vec::new()));
        let p = progress.clone();
        let options = McpInputRequiredOptions {
            per_request_timeout: Duration::from_millis(50),
            reset_timeout_on_progress: true,
            on_progress: Some(Arc::new(move |event| p.lock().unwrap().push(event))),
            ..Default::default()
        };
        let outcome = drive_modern_request(&io, "tools/call", json!({"name":"w"}), options).await;
        assert_eq!(outcome.is_ok(), respond);
        assert_eq!(progress.lock().unwrap().len(), 1);
        peer.await.unwrap();
    }
}

#[test]
fn actual_value_serialization_matches_js_safe_integral_number_bytes() {
    for case in fixture()["safe_integral_number_wire_cases"]
        .as_array()
        .unwrap()
    {
        let mut value: Value = serde_json::from_str(case["raw_json"].as_str().unwrap()).unwrap();
        normalize_mcp_safe_integral_doubles(&mut value);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            case["canonical_json"].as_str().unwrap()
        );
    }
}

#[tokio::test]
async fn actual_connection_retry_and_result_canonicalize_safe_integral_double_bytes() {
    let (connection, input, mut output) = connection();
    connection
        .register_handler(
            "roots/list",
            Arc::new(RecordingHandler {
                ids: Arc::new(Mutex::new(Vec::new())),
                delay: Duration::ZERO,
                result: serde_json::from_str("{\"roots\":[],\"number\":3.0}").unwrap(),
                pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        )
        .await;
    let peer = tokio::spawn(async move {
        for round in 0..2 {
            let frame = output.recv().await.unwrap();
            let wire = std::str::from_utf8(&frame).unwrap();
            let request: Value = serde_json::from_slice(&frame).unwrap();
            assert!(wire.contains("\"arguments\":{\"number\":2}"), "{wire}");
            assert!(!wire.contains("\"number\":2.0"), "{wire}");
            if round == 1 {
                assert!(
                    wire.contains("\"inputResponses\":{\"r\":{\"roots\":[],\"number\":3}}"),
                    "{wire}"
                );
                assert!(!wire.contains("\"number\":3.0"), "{wire}");
            }
            let result = if round == 0 {
                "{\"resultType\":\"input_required\",\"inputRequests\":{\"r\":{\"method\":\"roots/list\"}}}"
            } else {
                "{\"resultType\":\"complete\",\"content\":[],\"structuredContent\":{\"number\":2.0},\"_meta\":{\"number\":10.0}}"
            };
            input
                .send(bytes::Bytes::from(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{result}}}\n",
                    request["id"]
                )))
                .await
                .unwrap();
        }
    });
    let io = JsonrpcMcpResultIo {
        connection,
        client_capabilities: json!({"roots":{}}),
    };
    let original: Value =
        serde_json::from_str("{\"name\":\"worker\",\"arguments\":{\"number\":2.0}}").unwrap();
    let result = drive_modern_request(
        &io,
        "tools/call",
        original,
        McpInputRequiredOptions::default(),
    )
    .await
    .unwrap();
    let f = fixture();
    let native = f["flow_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "complete-numeric-public-field-order")
        .unwrap();
    assert_eq!(
        serde_json::to_string(&result).unwrap(),
        native["expected_result_json"].as_str().unwrap()
    );
    let native_decode = f["decode_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "complete-numeric-decode-field-order")
        .unwrap();
    let DecodedMcpResult::Complete(decoded) = decode_modern_result(
        "tools/call",
        serde_json::from_str(native_decode["raw_json"].as_str().unwrap()).unwrap(),
    )
    .unwrap() else {
        panic!("complete expected")
    };
    assert_eq!(
        serde_json::to_string(&decoded).unwrap(),
        native_decode["expected_result_json"].as_str().unwrap()
    );
    peer.await.unwrap();
}

struct ProjectionHandler {
    seen: Arc<Mutex<Vec<jsonrpc::Request>>>,
    result: Value,
}
#[async_trait]
impl jsonrpc::InboundHandler for ProjectionHandler {
    async fn handle(&self, request: jsonrpc::Request) -> jsonrpc::Response {
        self.seen.lock().unwrap().push(request.clone());
        jsonrpc::Response::success(request.id, self.result.clone())
    }
}
#[tokio::test]
async fn actual_registered_handler_params_and_result_match_native_schema_projection_bytes() {
    for name in [
        "roots-handler-schema-projection",
        "roots-handler-retains-common-wire-params",
        "elicitation-handler-schema-projection",
        "sampling-handler-and-result-schema-projection",
    ] {
        let f = fixture();
        let case = f["flow_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let (connection, _input, mut output) = connection();
        let requests = case["server_results"][0]["inputRequests"]
            .as_object()
            .unwrap();
        let (key, request) = requests.iter().next().unwrap();
        let method = request["method"].as_str().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        connection
            .register_handler(
                method,
                Arc::new(ProjectionHandler {
                    seen: seen.clone(),
                    result: case["handlers"][method]["result"].clone(),
                }),
            )
            .await;
        let io = JsonrpcMcpResultIo {
            connection,
            client_capabilities: case["capabilities"].clone(),
        };
        let result = io
            .dispatch_local(key, request.clone(), CancellationToken::new())
            .await
            .unwrap();
        let events = case["expected"]["handler_events"].as_array().unwrap();
        let expected = events
            .iter()
            .find(|event| event["event"] == "start")
            .unwrap();
        let actual = seen.lock().unwrap();
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].id, jsonrpc::Id::String(key.clone()));
        assert_eq!(actual[0].method, method);
        assert_eq!(
            serde_json::to_string(actual[0].params.as_ref().unwrap()).unwrap(),
            serde_json::to_string(&expected["params"]).unwrap(),
            "{name}"
        );
        let expected_result =
            &case["expected"]["requests"][1]["request"]["params"]["inputResponses"][key];
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            serde_json::to_string(expected_result).unwrap(),
            "{name}"
        );
        assert!(output.try_recv().is_err());
    }
}
