use std::sync::Arc;

use llm_runtime::LlmRequest;
use platform_api::{ModelAttemptRun, ModelAttemptStage};

#[test]
fn registered_capability_is_in_memory_only_and_cannot_be_forged_by_json() {
    let run = ModelAttemptRun::new(Arc::new(()));
    let mut request = LlmRequest::new("test-model").with_user_text("request");
    request.query_source = Some("fusion_panel".into());
    assert!(request.model_attempt.is_none());
    let ordinary = serde_json::to_value(&request).unwrap();
    request.model_attempt = Some(run.context(ModelAttemptStage::Panel, Some(0)).unwrap());
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
    let decoded: LlmRequest = serde_json::from_value(forged).unwrap();
    assert!(decoded.model_attempt.is_none());
    assert!(decoded.query_source.is_none());
}
