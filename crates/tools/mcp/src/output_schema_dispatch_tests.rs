mod output_schema_dispatch {
    use super::*;
    use lingxi_core::host::json_schema_validator::{
        CompiledJsonSchema, JsonSchemaValidation, JsonSchemaValidator, JsonSchemaValidatorSession,
        SchemaCompileError, SchemaProfile, SchemaWorkerError,
    };
    struct Validator;
    struct Session;
    struct Plan(String);
    impl JsonSchemaValidator for Validator {
        fn new_session(&self, _: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
            Arc::new(Session)
        }
    }
    #[async_trait]
    impl JsonSchemaValidatorSession for Session {
        async fn compile(
            &self,
            schema: &UJP,
        ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
            Ok(Arc::new(Plan(
                schema
                    .string_at("/required/0")
                    .expect("required fixture")
                    .to_owned(),
            )))
        }
    }
    #[async_trait]
    impl CompiledJsonSchema for Plan {
        async fn validate(&self, output: &UJP) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            Ok(if output.get_property(&self.0).is_some() {
                JsonSchemaValidation::Valid
            } else {
                JsonSchemaValidation::Invalid {
                    detail: format!("required {}", self.0).into(),
                    keywords: vec!["required".into()],
                }
            })
        }
    }
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    async fn request(rx: &mut mpsc::Receiver<Bytes>, method: &str) -> Value {
        loop {
            let bytes = tokio::time::timeout(WAIT, rx.recv())
                .await
                .unwrap()
                .unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            if request.get("id").is_none() {
                continue;
            }
            assert_eq!(request["method"], method);
            return request;
        }
    }
    async fn reply(tx: &mpsc::Sender<Bytes>, req: &Value, result: Value) {
        let response = json!({"jsonrpc":"2.0", "id":req["id"], "result":result});
        tx.send(Bytes::from(format!("{response}\n"))).await.unwrap();
    }
    async fn list(
        client: &Arc<mcp::McpClient>,
        tx: &mpsc::Sender<Bytes>,
        rx: &mut mpsc::Receiver<Bytes>,
        field: &str,
    ) {
        let task = {
            let client = client.clone();
            tokio::spawn(async move { client.list_tools().await })
        };
        let req = request(rx, "tools/list").await;
        reply(tx, &req, json!({"tools":[{"name":"slowtool", "inputSchema":{"type":"object"}, "outputSchema":{"required":[field]}}]})).await;
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn mcp_tool_hands_mod_the_physical_rpc_schema_instead_of_bound_or_refreshed_schema() {
        let (conn, tx, mut rx) = paired();
        let client = Arc::new(
            mcp::McpClient::new("slow", std::path::PathBuf::from("/tmp"), conn)
                .await
                .with_json_schema_validator(Arc::new(Validator)),
        );
        let registry = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
        registry.register_client("slow", client.clone()).await;
        list(&client, &tx, &mut rx, "live").await;
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![std::env::temp_dir()],
        );
        ctx.mcp_registry = Some(registry);
        let tool = Arc::new(MCPTool::new_for_tool(
            ctx,
            "mcp__slow__slowtool".into(),
            "fixture".into(),
            source_fixture(&json!({"type":"object"})),
            Some(source_fixture(&json!({"required":["cached"]}))),
            None,
            None,
            false,
            false,
        ));
        let capture = lingxi_core::host::tool_invoker::ToolOutputParserCapture::default();
        let task = {
            let tool = tool.clone();
            let capture = capture.clone();
            tokio::spawn(async move {
                tool.call_with_output_parser(
                    source_fixture(&json!({})),
                    tool_api::test_support::fresh_ctx(),
                    tool_api::test_support::fresh_tx(),
                    capture,
                )
                .await
            })
        };
        let req = request(&mut rx, "tools/call").await;
        list(&client, &tx, &mut rx, "refreshed").await;
        reply(
            &tx,
            &req,
            json!({"content":[], "structuredContent":{"live":true}}),
        )
        .await;
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .expect("ordinary RPC validates with live dispatch schema");
        let parser = capture
            .snapshot()
            .expect("physical call publishes an immutable parser");
        parser
            .parse_output(&source_fixture(&json!({"live":true})))
            .await
            .expect("Mod replacement retains physical call contract");
        assert!(parser
            .parse_output(&source_fixture(&json!({"cached":true})))
            .await
            .is_err());
        assert!(parser
            .parse_output(&source_fixture(&json!({"refreshed":true})))
            .await
            .is_err());
        assert!(
            tool.parse_output(&source_fixture(&json!({"live":true})))
                .await
                .is_err(),
            "current catalog has moved on"
        );
    }
}
