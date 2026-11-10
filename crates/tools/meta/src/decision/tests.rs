use super::*;
use decision_api::{Answer, Response, Usage};
use std::sync::Mutex;
use tool_api::test_support::{fresh_ctx, fresh_tx};

#[derive(Default)]
struct Service {
    seen: Mutex<Vec<Input>>,
    cancel: Mutex<Option<tokio_util::sync::CancellationToken>>,
    hold: bool,
    invalid: bool,
    stale: bool,
}
#[async_trait]
impl DecisionService for Service {
    async fn evaluate(&self, input: Input, context: InvocationContext) -> Result<Response, Error> {
        self.seen.lock().unwrap().push(input);
        *self.cancel.lock().unwrap() = Some(context.cancellation);
        if self.hold {
            return std::future::pending().await;
        }
        Ok(Response {
            state_revision: if self.stale { 1 } else { 0 },
            answers: std::collections::BTreeMap::from([(
                "ok".into(),
                Answer::Binary {
                    probability: if self.invalid { 1.2 } else { 0.75 },
                },
            )]),
            usage: Usage::default(),
            actual_model: None,
            elapsed_ms: 1,
        })
    }
}
fn input() -> Utf16JsonProjection {
    generated_builtin_result(
        &json!({"state":{"evidence":"observed"},"questions":{"ok":{"type":"binary","instructions":"Does the evidence match?"}}}),
    )
}

#[tokio::test]
async fn returns_validated_predictions_without_inventing_statistics() {
    let service = Arc::new(Service::default());
    let tool = DecisionTool::new(service.clone());
    let result = tool.call(input(), fresh_ctx(), fresh_tx()).await.unwrap();
    let result: serde_json::Value =
        serde_json::from_str(&result.data.to_json_string().unwrap()).unwrap();
    assert_eq!(result["answers"]["ok"]["probability"], 0.75);
    assert!(result["usage"]["input_tokens"].is_null());
    assert!(result["actual_model"].is_null());
    assert_eq!(service.seen.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn rejects_backend_overrides_before_any_service_call() {
    let service = Arc::new(Service::default());
    let tool = DecisionTool::new(service.clone());
    let mut value: serde_json::Value =
        serde_json::from_str(&input().to_json_string().unwrap()).unwrap();
    value["endpoint"] = json!("https://other.invalid");
    assert!(matches!(
        tool.call(generated_builtin_result(&value), fresh_ctx(), fresh_tx())
            .await,
        Err(ToolError::InvalidInput(_))
    ));
    assert!(service.seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn cancellation_uses_the_invocation_token_and_never_restarts() {
    let service = Arc::new(Service {
        hold: true,
        ..Default::default()
    });
    let tool = DecisionTool::new(service.clone());
    let parent = tokio_util::sync::CancellationToken::new();
    let mut ctx = fresh_ctx();
    ctx.cancel = Some(parent.clone());
    let operation = tokio::spawn(async move { tool.call(input(), ctx, fresh_tx()).await });
    while service.seen.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    parent.cancel();
    assert!(matches!(operation.await.unwrap(), Err(ToolError::Aborted)));
    assert!(service
        .cancel
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled());
    assert_eq!(service.seen.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn already_cancelled_calls_do_not_dispatch() {
    let service = Arc::new(Service::default());
    let tool = DecisionTool::new(service.clone());
    let mut ctx = fresh_ctx();
    ctx.cancel = Some(tokio_util::sync::CancellationToken::new());
    ctx.cancel.as_ref().unwrap().cancel();
    assert!(matches!(
        tool.call(input(), ctx, fresh_tx()).await,
        Err(ToolError::Aborted)
    ));
    assert!(service.seen.lock().unwrap().is_empty());
}
#[tokio::test(start_paused = true)]
async fn total_deadline_cancels_work_without_cancelling_the_parent() {
    let service = Arc::new(Service {
        hold: true,
        ..Default::default()
    });
    let tool = DecisionTool::new(service.clone());
    let parent = tokio_util::sync::CancellationToken::new();
    let mut ctx = fresh_ctx();
    ctx.cancel = Some(parent.clone());
    assert!(matches!(
        tool.call(input(), ctx, fresh_tx()).await,
        Err(ToolError::Io(_))
    ));
    assert!(service
        .cancel
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled());
    assert!(!parent.is_cancelled());
    assert_eq!(service.seen.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn invalid_service_answers_are_not_presented_as_success() {
    let tool = DecisionTool::new(Arc::new(Service {
        invalid: true,
        ..Default::default()
    }));
    assert!(matches!(
        tool.call(input(), fresh_ctx(), fresh_tx()).await,
        Err(ToolError::Io(_))
    ));
}

#[tokio::test]
async fn moved_evidence_still_rejects_a_stale_state_revision() {
    let tool = DecisionTool::new(Arc::new(Service {
        stale: true,
        ..Default::default()
    }));
    assert!(matches!(
        tool.call(input(), fresh_ctx(), fresh_tx()).await,
        Err(ToolError::Io(_))
    ));
}

#[tokio::test]
async fn moved_evidence_still_rejects_missing_extra_and_wrong_answer_types() {
    struct InvalidService(Response);
    #[async_trait]
    impl DecisionService for InvalidService {
        async fn evaluate(&self, _: Input, _: InvocationContext) -> Result<Response, Error> {
            Ok(self.0.clone())
        }
    }
    for answers in [
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::from([
            ("ok".into(), Answer::Binary { probability: 0.75 }),
            ("extra".into(), Answer::Binary { probability: 0.75 }),
        ]),
        std::collections::BTreeMap::from([(
            "ok".into(),
            Answer::Score {
                value: 0.5,
                probabilities: None,
                confidence: None,
            },
        )]),
    ] {
        let tool = DecisionTool::new(Arc::new(InvalidService(Response {
            state_revision: 0,
            answers,
            usage: Usage::default(),
            actual_model: None,
            elapsed_ms: 1,
        })));
        assert!(matches!(
            tool.call(input(), fresh_ctx(), fresh_tx()).await,
            Err(ToolError::Io(_))
        ));
    }
}
