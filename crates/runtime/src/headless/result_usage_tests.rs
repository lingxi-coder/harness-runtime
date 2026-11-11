use super::*;
use lingxi_core::host::result_usage::{ResultModelUsage, ResultUsageSource, ResultUsageTotals};
use lingxi_core::types::SessionId;

#[derive(Default)]
struct Source(StdMutex<Vec<ResultModelUsage>>, AtomicUsize);

#[tokio::test]
async fn held_result_keeps_its_cost_snapshot_but_stamps_usage_at_publication() {
    use lingxi_core::host::result_usage::ResultCost;
    struct ChangingSource(StdMutex<(Vec<ResultModelUsage>, ResultUsageTotals)>);
    impl ResultUsageSource for ChangingSource {
        fn begin_query(&self, _: SessionId) {}
        fn query_usage(&self, _: SessionId, read: &mut dyn FnMut(&[ResultModelUsage])) {
            read(&self.0.lock().unwrap().0);
        }
        fn publish_result_usage(
            &self,
            _: SessionId,
            publish: &mut dyn FnMut(&[ResultModelUsage], &ResultUsageTotals) -> bool,
        ) {
            let mut state = self.0.lock().unwrap();
            if publish(&state.0, &state.1) {
                state.0.clear();
            }
        }
    }
    let bytes = Arc::new(StdMutex::new(Vec::new()));
    let id = SessionId::new().as_uuid().to_string();
    let stream = StreamJsonStream::new(
        super::tests::make_params(&id),
        Output::new(super::verbose_json_tests::CapturedWriter(bytes.clone())),
    );
    let source = Arc::new(ChangingSource(StdMutex::new((
        vec![row("firstParty", 3)],
        ResultUsageTotals {
            total: Some(ResultCost {
                nano_usd: 251400,
                cost_usd: 0.00025140000000000004,
            }),
            models: vec![],
        },
    ))));
    stream.set_result_usage_source(source.clone());
    stream.hold_sdk_results(true);
    stream
        .enqueue_result_frame(
            &json!({"type":"result","session_id":id,
        "result_index":0,"total_cost_usd":0.0002514}),
            None,
        )
        .await;
    {
        let mut pending = source.0.lock().unwrap();
        assert_eq!(pending.0.len(), 1);
        pending.0[0].input_tokens += 5;
        pending.1.total = Some(ResultCost {
            nano_usd: 335200,
            cost_usd: 0.0003352,
        });
    }
    stream.release_sdk_results(false);
    stream.finish().await.unwrap();
    let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(raw.contains(r#""total_cost_usd":0.00025140000000000004"#));
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap()["model_usage_since_last_result"]["shared"]
            ["inputTokens"],
        8
    );
    assert!(source.0.lock().unwrap().0.is_empty());
}

impl ResultUsageSource for Source {
    fn begin_query(&self, _: SessionId) {}

    fn query_usage(&self, _: SessionId, read: &mut dyn FnMut(&[ResultModelUsage])) {
        read(&self.0.lock().unwrap());
    }

    fn publish_result_usage(
        &self,
        _: SessionId,
        publish: &mut dyn FnMut(&[ResultModelUsage], &ResultUsageTotals) -> bool,
    ) {
        let mut rows = self.0.lock().unwrap();
        self.1.fetch_add(1, Ordering::Relaxed);
        if publish(&rows, &ResultUsageTotals::default()) {
            rows.clear();
        }
    }
}

fn row(provider: &str, input: i128) -> ResultModelUsage {
    ResultModelUsage {
        model: "shared".into(),
        provider: provider.into(),
        input_tokens: input,
        output_tokens: 2,
        nano_usd: 83800,
        cost_usd: 0.0000838,
        cache_creation_5m_input_tokens: 7,
        cache_creation_1h_input_tokens: 11,
        cache_creation_input_tokens: 18,
        ..Default::default()
    }
}

#[tokio::test]
async fn retry_exhaustion_reports_last_usage_and_keeps_cumulative_model_usage() {
    use lingxi_core::host::orchestrator::{
        CurrentUsageSnapshot, ModelUsageRow, ResponseUsageDetailsSnapshot,
    };
    let stream = StreamJsonStream::new(
        super::tests::make_params("session"),
        Output::new(tokio::io::sink()),
    );
    let cost = CostSnapshot {
        current_usage: Some(CurrentUsageSnapshot {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 18,
        }),
        current_usage_details: Some(ResponseUsageDetailsSnapshot {
            cache_creation_5m_input_tokens: 7,
            cache_creation_1h_input_tokens: 11,
            ..Default::default()
        }),
        by_model: vec![ModelUsageRow {
            model: "claude-sonnet-5-5".into(),
            provider: Some("firstParty".into()),
            total_nano_usd: 167600,
            input_tokens: 2,
            output_tokens: 4,
            cache_read_input_tokens: 6,
            cache_creation_input_tokens: 36,
            cache_creation_5m_input_tokens: 14,
            cache_creation_1h_input_tokens: 22,
            reasoning_tokens: 0,
            web_search_requests: 0,
        }],
        ..Default::default()
    };
    let frame = stream
        .build_result_error_frame(
            "error_max_structured_output_retries",
            vec!["fixture retry exhausted".into()],
            &cost,
            "off",
            Some("sdk_opt_in_required"),
            &[],
        )
        .await;
    assert_eq!(frame["usage"]["input_tokens"], 1);
    assert_eq!(frame["usage"]["output_tokens"], 2);
    assert_eq!(frame["usage"]["cache_creation_input_tokens"], 18);
    assert_eq!(
        frame["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
        11
    );
    assert_eq!(frame["modelUsage"]["claude-sonnet-5-5"]["inputTokens"], 2);
    assert_eq!(
        frame["modelUsage"]["claude-sonnet-5-5"]["cacheCreationInputTokens"],
        36
    );
}

#[tokio::test]
async fn held_results_consume_usage_at_release_and_each_result_resets_the_slice() {
    let bytes = Arc::new(StdMutex::new(Vec::new()));
    let id = SessionId::new().as_uuid().to_string();
    let stream = StreamJsonStream::new(
        super::tests::make_params(&id),
        Output::new(super::verbose_json_tests::CapturedWriter(bytes.clone())),
    );
    let source = Arc::new(Source::default());
    source.0.lock().unwrap().push(row("firstParty", 3));
    stream.set_result_usage_source(source.clone());
    stream.hold_sdk_results(true);
    stream
        .enqueue_result_frame(
            &json!({"type":"result", "session_id":id, "result_index":0, "account_routing":[]}),
            None,
        )
        .await;
    source.0.lock().unwrap()[0].input_tokens += 5;
    stream
        .enqueue_result_frame(
            &json!({"type":"result", "session_id":id, "result_index":1}),
            None,
        )
        .await;
    assert_eq!(source.1.load(Ordering::Relaxed), 0);
    stream.release_sdk_results(false);
    stream.finish().await.unwrap();
    let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let frames: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        frames[0]["model_usage_since_last_result"]["shared"]["inputTokens"],
        8
    );
    assert_eq!(
        frames[0]["model_usage_since_last_result"]["shared"]["cacheCreationEphemeral5mInputTokens"],
        7
    );
    assert_eq!(frames[1]["model_usage_since_last_result"], json!({}));
    let keys: Vec<_> = frames[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let index = keys.iter().position(|key| *key == "result_index").unwrap();
    assert_eq!(keys[index + 1], "model_usage_since_last_result");
    assert_eq!(keys[index + 2], "account_routing");
}

#[tokio::test]
async fn refusal_before_held_release_keeps_the_usage_pending() {
    let id = SessionId::new().as_uuid().to_string();
    let stream = StreamJsonStream::new(
        super::tests::make_params(&id),
        Output::new(tokio::io::sink()),
    );
    let source = Arc::new(Source::default());
    source.0.lock().unwrap().push(row("firstParty", 4));
    stream.set_result_usage_source(source.clone());
    let probe = crate::desktop::PolicyHelperRefusalProbe::test_probe();
    stream.set_policy_helper_refusal_probe(probe.clone());
    stream.hold_sdk_results(true);
    stream
        .enqueue_result_frame(
            &json!({"type":"result", "session_id":id, "result_index":0}),
            None,
        )
        .await;
    probe.trigger_refusal_for_test();
    stream.release_sdk_results(false);
    assert_eq!(source.1.load(Ordering::Relaxed), 0);
    assert_eq!(source.0.lock().unwrap()[0].input_tokens, 4);
}

#[tokio::test]
async fn disconnected_output_preserves_pending_result_usage() {
    let id = SessionId::new().as_uuid().to_string();
    let stream = StreamJsonStream::new(
        super::tests::make_params(&id),
        Output::new(tokio::io::sink()),
    );
    let source = Arc::new(Source::default());
    source.0.lock().unwrap().push(row("firstParty", 4));
    stream.set_result_usage_source(source.clone());
    drop(stream.drain_rx.lock().await.take());
    stream
        .enqueue_result_frame(
            &json!({"type":"result", "session_id":id, "result_index":0}),
            None,
        )
        .await;
    assert_eq!(source.0.lock().unwrap()[0].input_tokens, 4);
    assert!(stream.output.error().is_some());
}

#[tokio::test]
async fn json_output_does_not_consume_or_add_the_sdk_interval() {
    let id = SessionId::new().as_uuid().to_string();
    let stream = StreamJsonStream::new_json_mode_placeholder(Output::new(tokio::io::sink()));
    *stream.session_id_handle().lock().await = id.clone();
    let source = Arc::new(Source::default());
    source.0.lock().unwrap().push(row("firstParty", 4));
    stream.set_result_usage_source(source.clone());
    stream
        .enqueue_result_frame(
            &json!({"type":"result", "session_id":id, "result_index":0}),
            None,
        )
        .await;
    stream.finish().await.unwrap();
    assert_eq!(source.1.load(Ordering::Relaxed), 1);
    assert_eq!(source.0.lock().unwrap()[0].input_tokens, 4);
}

#[test]
fn query_usage_sums_ttl_counts_and_retains_native_observation_order() {
    let usage =
        StreamJsonStream::build_query_usage_block(&[row("firstParty", 3), row("openai", 5)]);
    assert_eq!(usage["input_tokens"], 8);
    assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 14);
    assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 22);
    let keys = usage
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        &keys[..5],
        &[
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "output_tokens",
            "output_tokens_details"
        ]
    );
    let empty = StreamJsonStream::build_query_usage_block(&[]);
    assert_eq!(
        empty.as_object().unwrap().keys().next().unwrap(),
        "output_tokens_details"
    );
}

#[test]
fn provider_collisions_and_signed_corrections_keep_their_values() {
    let usage =
        StreamJsonStream::build_result_usage_block(&[row("firstParty", -2), row("openai", 5)], &[]);
    assert_eq!(usage.len(), 2);
    assert_eq!(usage["firstParty/shared"]["inputTokens"], -2);
    assert_eq!(usage["openai/shared"]["inputTokens"], 5);
    assert_eq!(usage["firstParty/shared"]["costUSD"], 0.0000838);
}

#[test]
fn cumulative_cost_projection_keeps_native_float_and_restored_property_order() {
    use lingxi_core::host::result_usage::{ResultCost, ResultModelCost};

    let mut usage = row("firstParty", 3);
    usage.cost_usd = 0.0002514;
    let models = StreamJsonStream::build_result_usage_block(&[usage], &[]);
    let mut frame = project_json_value(&json!({
        "total_cost_usd": 0.0002514, "modelUsage": models, "result": ""
    }));
    frame
        .replace_pointer_projection(
            "/result",
            Utf16JsonProjection::parse(r#""\ud800""#).unwrap(),
        )
        .unwrap();
    StreamJsonStream::project_result_costs(
        &mut frame,
        &ResultUsageTotals {
            total: Some(ResultCost {
                nano_usd: 251400,
                cost_usd: 0.00025140000000000004,
            }),
            models: vec![ResultModelCost {
                model: "shared".into(),
                provider: "firstParty".into(),
                cost: ResultCost {
                    nano_usd: 251400,
                    cost_usd: 0.00025140000000000004,
                },
                restored: true,
            }],
        },
    )
    .unwrap();
    let raw = frame.to_json_string().unwrap();
    assert!(raw.contains(r#""total_cost_usd":0.00025140000000000004"#));
    assert!(raw.contains(r#""costUSD":0.00025140000000000004"#));
    assert!(raw.contains(r#""result":"\ud800""#));
    let model = frame.subprojection("/modelUsage/shared").unwrap();
    let keys = model
        .object_entries()
        .unwrap()
        .iter()
        .map(|entry| entry.display_key.to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        &keys[..4],
        &[
            "inputTokens",
            "outputTokens",
            "thinkingTokens",
            "cacheReadInputTokens"
        ]
    );
}
