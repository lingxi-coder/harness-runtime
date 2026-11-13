mod output_schema_dispatch {
    use super::*;
    use lingxi_core::host::json_schema_validator::{JsonSchemaValidatorSession, SchemaWorkerError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct RequiredValidator {
        alive: Arc<AtomicUsize>,
        compiles: Arc<AtomicUsize>,
    }
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct Session {
        lease: Arc<Lease>,
        compiles: Arc<AtomicUsize>,
    }
    struct Plan {
        _lease: Arc<Lease>,
        required: String,
    }
    impl JsonSchemaValidator for RequiredValidator {
        fn new_session(&self, _: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
            self.alive.fetch_add(1, Ordering::SeqCst);
            Arc::new(Session {
                lease: Arc::new(Lease(self.alive.clone())),
                compiles: self.compiles.clone(),
            })
        }
    }
    #[async_trait::async_trait]
    impl JsonSchemaValidatorSession for Session {
        async fn compile(
            &self,
            schema: &Utf16JsonProjection,
        ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
            self.compiles.fetch_add(1, Ordering::SeqCst);
            let Some(required) = schema.string_at("/required/0") else {
                return Err(SchemaCompileError::InvalidSchema(
                    "fixture requires an array".into(),
                ));
            };
            Ok(Arc::new(Plan {
                _lease: self.lease.clone(),
                required: required.to_owned(),
            }))
        }
    }
    #[async_trait::async_trait]
    impl CompiledJsonSchema for Plan {
        async fn validate(
            &self,
            input: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            Ok(if input.get_property(&self.required).is_some() {
                JsonSchemaValidation::Valid
            } else {
                JsonSchemaValidation::Invalid {
                    detail: format!("required {}", self.required).into(),
                    keywords: vec!["required".into()],
                }
            })
        }
    }
    const NAME: &str = "mcp__fixture__worker";
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    fn source(value: serde_json::Value) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(&value.to_string()).unwrap()
    }
    async fn request(
        rx: &mut tokio::sync::mpsc::Receiver<Bytes>,
        method: &str,
    ) -> serde_json::Value {
        loop {
            let bytes = tokio::time::timeout(WAIT, rx.recv())
                .await
                .unwrap()
                .unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if request.get("id").is_none() {
                continue;
            }
            assert_eq!(request["method"], method);
            return request;
        }
    }
    async fn reply(
        tx: &tokio::sync::mpsc::Sender<Bytes>,
        req: &serde_json::Value,
        mut result: serde_json::Value,
        modern: bool,
    ) {
        if modern {
            result["resultType"] = "complete".into();
            match req["method"].as_str().expect("fixture RPC method") {
                "tools/list" => {
                    result["ttlMs"] = serde_json::json!(0);
                    result["cacheScope"] = "private".into();
                }
                "tools/call" => {
                    result["isError"] = serde_json::json!(false);
                }
                other => panic!("unsupported modern fixture method: {other}"),
            }
        }
        let response = serde_json::json!({"jsonrpc":"2.0", "id":req["id"], "result":result});
        tx.send(Bytes::from(format!("{response}\n"))).await.unwrap();
    }
    async fn list(
        client: &Arc<McpClient>,
        tx: &tokio::sync::mpsc::Sender<Bytes>,
        rx: &mut tokio::sync::mpsc::Receiver<Bytes>,
        schema: Option<&str>,
        modern: bool,
    ) {
        let task = {
            let client = client.clone();
            tokio::spawn(async move { client.list_tools().await })
        };
        let req = request(rx, "tools/list").await;
        let mut tool = serde_json::json!({"name":"worker", "inputSchema":{"type":"object"}});
        if let Some(schema) = schema {
            tool["outputSchema"] = serde_json::from_str(schema).unwrap();
        }
        reply(tx, &req, serde_json::json!({"tools":[tool]}), modern).await;
        assert_eq!(
            tokio::time::timeout(WAIT, task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .len(),
            1
        );
    }
    async fn client(
        modern: bool,
    ) -> (
        Arc<McpClient>,
        Arc<RequiredValidator>,
        tokio::sync::mpsc::Sender<Bytes>,
        tokio::sync::mpsc::Receiver<Bytes>,
    ) {
        let (conn, tx, rx) = paired_connection();
        let validator = Arc::new(RequiredValidator::default());
        let mut client = McpClient::new("fixture", PathBuf::from("/tmp"), conn)
            .await
            .with_json_schema_validator(validator.clone());
        if modern {
            client = client.with_negotiated_protocol(McpNegotiatedProtocol {
                era: McpProtocolEra::Modern,
                version: "2026-07-28".into(),
            });
        }
        (Arc::new(client), validator, tx, rx)
    }

    #[tokio::test]
    async fn real_rpc_pins_replaced_removed_invalid_and_absent_schemas_in_both_protocols() {
        for modern in [false, true] {
            for (initial, refreshed, output, error_kind) in [
                (
                    Some(r#"{"required":["old"]}"#),
                    Some(r#"{"required":["new"]}"#),
                    serde_json::json!({"old":true}),
                    0,
                ),
                (
                    Some(r#"{"required":["old"]}"#),
                    None,
                    serde_json::json!({}),
                    1,
                ),
                (
                    None,
                    Some(r#"{"required":["new"]}"#),
                    serde_json::json!({}),
                    0,
                ),
                (
                    Some(r#"{"required":false}"#),
                    None,
                    serde_json::json!({}),
                    2,
                ),
            ] {
                let (client, validator, tx, mut rx) = client(modern).await;
                list(&client, &tx, &mut rx, initial, modern).await;
                let captured = Arc::new(StdMutex::new(None));
                let capture = captured.clone();
                let callback: McpOutputSchemaCallback =
                    Arc::new(move |snapshot| *capture.lock().unwrap() = Some(snapshot));
                let call = {
                    let client = client.clone();
                    tokio::spawn(async move {
                        client
                            .call_tool_with_output_schema_capture(
                                NAME,
                                source(serde_json::json!({})),
                                None,
                                None,
                                None,
                                Some(callback),
                            )
                            .await
                    })
                };
                let req = request(&mut rx, "tools/call").await;
                list(&client, &tx, &mut rx, refreshed, modern).await;
                if initial.is_some_and(|value| value.contains("old")) {
                    assert!(
                        validator.alive.load(Ordering::SeqCst) >= 1,
                        "pending RPC retains the retired catalog session"
                    );
                }
                reply(
                    &tx,
                    &req,
                    serde_json::json!({"content":[], "structuredContent":output}),
                    modern,
                )
                .await;
                let result = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
                match error_kind {
                    0 => {
                        result.expect("dispatch-time schema accepts the result");
                    }
                    1 => assert!(matches!(
                        result,
                        Err(McpClientError::OutputSchemaInvalid { .. })
                    )),
                    2 => assert!(matches!(
                        result,
                        Err(McpClientError::OutputSchemaCompileInvalid { .. })
                    )),
                    _ => unreachable!(),
                }
                let parser = captured
                    .lock()
                    .unwrap()
                    .take()
                    .expect("actual RPC supplied its parser");
                let parsed = parser.parse_output(&source(output)).await;
                assert_eq!(
                    parsed.is_err(),
                    error_kind != 0,
                    "Mod replacement must use the same dispatch contract"
                );
                drop(parser);
                drop(client);
                assert_eq!(
                    validator.alive.load(Ordering::SeqCst),
                    0,
                    "settled calls release all retired catalog sessions"
                );
            }
        }
    }

    #[tokio::test]
    async fn concurrent_real_calls_keep_independent_catalog_leases() {
        for modern in [false, true] {
            let (client, validator, tx, mut rx) = client(modern).await;
            list(
                &client,
                &tx,
                &mut rx,
                Some(r#"{"required":["old"]}"#),
                modern,
            )
            .await;
            let first = {
                let client = client.clone();
                tokio::spawn(
                    async move { client.call_tool(NAME, source(serde_json::json!({}))).await },
                )
            };
            let old = request(&mut rx, "tools/call").await;
            list(
                &client,
                &tx,
                &mut rx,
                Some(r#"{"required":["new"]}"#),
                modern,
            )
            .await;
            assert_eq!(validator.alive.load(Ordering::SeqCst), 2);
            let second = {
                let client = client.clone();
                tokio::spawn(
                    async move { client.call_tool(NAME, source(serde_json::json!({}))).await },
                )
            };
            let new = request(&mut rx, "tools/call").await;
            reply(
                &tx,
                &new,
                serde_json::json!({"content":[],"structuredContent":{"new":true}}),
                modern,
            )
            .await;
            second.await.unwrap().expect("new call uses new schema");
            reply(
                &tx,
                &old,
                serde_json::json!({"content":[],"structuredContent":{"old":true}}),
                modern,
            )
            .await;
            first
                .await
                .unwrap()
                .expect("old call uses its retained schema");
            assert_eq!(validator.alive.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn deterministic_invalid_schema_is_negative_cached_until_refresh() {
        let (client, validator, tx, mut rx) = client(false).await;
        list(&client, &tx, &mut rx, Some(r#"{"required":false}"#), false).await;
        for _ in 0..3 {
            assert!(matches!(
                client
                    .parse_tool_output(NAME, &source(serde_json::json!({})))
                    .await,
                Err(McpClientError::OutputSchemaCompileInvalid { .. })
            ));
        }
        assert_eq!(validator.compiles.load(Ordering::SeqCst), 1);
        list(
            &client,
            &tx,
            &mut rx,
            Some(r#"{"required":["new"]}"#),
            false,
        )
        .await;
        assert!(client
            .parse_tool_output(NAME, &source(serde_json::json!({"new":true})))
            .await
            .is_ok());
        assert_eq!(validator.compiles.load(Ordering::SeqCst), 2);
    }
}
