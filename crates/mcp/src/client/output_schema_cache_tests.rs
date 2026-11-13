mod output_schema_cache {
    use super::*;
    use lingxi_core::host::json_schema_validator::{JsonSchemaValidatorSession, SchemaWorkerError};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Validator {
        fail: AtomicBool,
        alive: Arc<AtomicUsize>,
    }
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct Session {
        lease: Arc<Lease>,
        fail: bool,
    }
    struct Plan(Arc<Lease>);
    impl JsonSchemaValidator for Validator {
        fn new_session(&self, _: SchemaProfile) -> Arc<dyn JsonSchemaValidatorSession> {
            self.alive.fetch_add(1, Ordering::SeqCst);
            Arc::new(Session {
                lease: Arc::new(Lease(self.alive.clone())),
                fail: self.fail.swap(false, Ordering::SeqCst),
            })
        }
    }
    #[async_trait::async_trait]
    impl JsonSchemaValidatorSession for Session {
        async fn compile(
            &self,
            _: &Utf16JsonProjection,
        ) -> Result<Arc<dyn CompiledJsonSchema>, SchemaCompileError> {
            if self.fail {
                Err(SchemaCompileError::Worker(
                    "transient fixture failure".into(),
                ))
            } else {
                Ok(Arc::new(Plan(self.lease.clone())))
            }
        }
    }
    #[async_trait::async_trait]
    impl CompiledJsonSchema for Plan {
        async fn validate(
            &self,
            input: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            let _lease = &self.0;
            if input.at_pointer("/ok").and_then(|value| value.as_bool()) == Some(true) {
                Ok(JsonSchemaValidation::Valid)
            } else {
                Ok(JsonSchemaValidation::Invalid {
                    detail: "required ok".into(),
                    keywords: vec!["required".into()],
                })
            }
        }
    }
    fn tool() -> McpToolDto {
        McpToolDto {
            full_name: "mcp__fixture__worker".into(),
            server_name: "fixture".into(),
            tool_name: "worker".into(),
            wire_tool_name: None,
            description: "fixture".into(),
            input_schema: Utf16JsonProjection::parse("{}").unwrap(),
            output_schema: Some(
                Utf16JsonProjection::parse(r#"{"type":"object","required":["ok"]}"#).unwrap(),
            ),
            annotations: None,
            icons: Vec::new(),
            meta: None,
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        }
    }
    #[tokio::test]
    async fn worker_failure_does_not_poison_recovery_and_catalog_sessions_retire() {
        let (connection, _peer_tx, _peer_rx) = paired_connection();
        let validator = Arc::new(Validator::default());
        validator.fail.store(true, Ordering::SeqCst);
        let client = McpClient::new("fixture", PathBuf::from("/tmp"), connection)
            .await
            .with_json_schema_validator(validator.clone());
        let tools = vec![tool()];
        assert!(client.install_output_schemas(&tools).await.is_err());
        assert_eq!(validator.alive.load(Ordering::SeqCst), 0);
        client.install_output_schemas(&tools).await.unwrap();
        assert!(client
            .parse_tool_output(
                &tools[0].full_name,
                &Utf16JsonProjection::parse("{}").unwrap()
            )
            .await
            .is_err());
        assert!(client
            .parse_tool_output(
                &tools[0].full_name,
                &Utf16JsonProjection::parse(r#"{"ok":true}"#).unwrap()
            )
            .await
            .is_ok());
        let in_flight = client
            .output_schema_snapshot(&tools[0].full_name)
            .await
            .plan
            .unwrap()
            .unwrap();
        for _ in 0..16 {
            client.install_output_schemas(&tools).await.unwrap();
            assert_eq!(validator.alive.load(Ordering::SeqCst), 2);
        }
        assert!(matches!(
            in_flight
                .validate(&Utf16JsonProjection::parse(r#"{"ok":true}"#).unwrap())
                .await
                .unwrap(),
            JsonSchemaValidation::Valid
        ));
        drop(in_flight);
        assert_eq!(validator.alive.load(Ordering::SeqCst), 1);
        drop(client);
        assert_eq!(validator.alive.load(Ordering::SeqCst), 0);
    }
}
