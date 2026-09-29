use std::sync::Arc;

use llm_runtime::LlmRequest;
use platform_api::{ModelAttemptRun, ModelAttemptStage};

#[test]
fn registered_capability_is_in_memory_only_and_cannot_be_forged_by_json() {
    let run = ModelAttemptRun::new(Arc::new(()));
    let mut request = LlmRequest::new("test-model").with_user_text("request");
    request.execution.query_source = Some("fusion_panel".into());
    assert!(request.execution.model_attempt.is_none());
    let ordinary = serde_json::to_value(&request).unwrap();
    assert!(ordinary.get("execution").is_none());
    request.execution.model_attempt = Some(run.context(ModelAttemptStage::Panel, Some(0)).unwrap());
    assert_eq!(request.clone(), request);
    assert_eq!(serde_json::to_value(&request).unwrap(), ordinary);

    let mut forged = ordinary;
    forged["model_attempt"] = serde_json::json!({
        "registration_id": "forged",
        "stage": "panel",
        "panel_slot": 0,
        "logical_call_id": 1,
    });
    forged["query_source"] = "fusion_panel".into();
    forged["execution"] = serde_json::json!({
        "model_attempt": { "registration_id": "forged", "stage": "panel", "panel_slot": 0, "logical_call_id": 1 },
        "account_scope": "attacker-account",
        "file_account_scope": "attacker-files",
        "message_json_string_overrides": { "/messages/0/content/0/text": [65, 55296] },
        "query_source": "forged-query",
        "capture_retry_count": true,
        "thinking_source_message_ids": ["forged-message"],
        "thinking_recovery_scope": { "query_id": "forged" }
    });
    let decoded: LlmRequest = serde_json::from_value(forged).unwrap();
    assert_eq!(decoded.execution, llm_runtime::ExecutionContext::default());
    assert_eq!(decoded.input, request.input);
}
