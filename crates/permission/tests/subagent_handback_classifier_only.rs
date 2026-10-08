//! Permission composition tests with scripted physical classifier replies.
//! These verify model-query dispatch and metadata, not live-provider accuracy.

use async_trait::async_trait;
use lingxi_core::host::handback::{handback_classifier_input, ReportReview};
use permission::classifier::{AutoModeClassifierVerdict, LoopPermissionClassifier};
use permission::gate::{
    ClassifierOnlyOnBlock, ClassifierOnlyPolicy, ClassifierOnlyReviewRequest,
    PermissionCheckContext, PermissionDecision, PermissionGate, PermissionOutcome,
};
use permission::loop_llm::{self, Query, QueryError, Reply, Transport};
use permission::{
    PermissionBehavior, PermissionMode, PermissionPolicy, PermissionRule, PermissionRuleSource,
    PermissionRuleValue, PolicyPermissionGate,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

struct NoPrompt;
#[async_trait]
impl PermissionGate for NoPrompt {
    async fn check(&self, _: &str, _: &Value) -> PermissionDecision {
        panic!("classifier-only tools must never reach a human transport")
    }
}

struct ScriptedQueries {
    replies: Mutex<VecDeque<Result<Reply, QueryError>>>,
    captured: Mutex<Vec<Query>>,
    reports: Mutex<Vec<ClassifierOnlyReviewRequest>>,
}

#[async_trait]
impl Transport for ScriptedQueries {
    async fn query(&self, query: Query) -> Result<Reply, QueryError> {
        self.captured.lock().unwrap().push(query);
        self.replies.lock().unwrap().pop_front().unwrap()
    }
    fn model(&self) -> &str {
        "scripted-classifier-model"
    }
}

#[async_trait]
impl LoopPermissionClassifier for ScriptedQueries {
    async fn classify(
        &self,
        _: &str,
        _: &Value,
        _: &[permission::host_context::HostContextRecord],
        _: &[String],
    ) -> AutoModeClassifierVerdict {
        panic!("report review must use child context rather than ordinary main-history review")
    }
    async fn classify_report(
        &self,
        request: &ClassifierOnlyReviewRequest,
        _: &[String],
    ) -> Option<ReportReview> {
        self.reports.lock().unwrap().push(request.clone());
        permission::handback_review::report_review(
            loop_llm::classify_detailed(
                self,
                "SubagentHandback",
                vec![
                    serde_json::to_string(&request.transcript).unwrap(),
                    request.action.clone(),
                ],
            )
            .await,
        )
    }
}

fn reply(text: &str) -> Result<Reply, QueryError> {
    Ok(Reply {
        text: text.into(),
        stop_reason: "end_turn".into(),
    })
}

fn fixture(
    rules: Vec<PermissionRule>,
    replies: Vec<Result<Reply, QueryError>>,
) -> (PolicyPermissionGate, Arc<ScriptedQueries>) {
    let gate = PolicyPermissionGate::new(
        Arc::new(PermissionPolicy::from_rules(PermissionMode::Auto, rules)),
        Arc::new(NoPrompt),
    );
    let classifier = Arc::new(ScriptedQueries {
        replies: Mutex::new(replies.into()),
        captured: Mutex::new(Vec::new()),
        reports: Mutex::new(Vec::new()),
    });
    assert!(gate
        .loop_classifier_handle()
        .set(classifier.clone())
        .is_ok());
    (gate, classifier)
}

fn rule(source: PermissionRuleSource, behavior: PermissionBehavior, name: &str) -> PermissionRule {
    PermissionRule {
        value: PermissionRuleValue::from_rule_string(name),
        behavior,
        source,
    }
}

fn request() -> ClassifierOnlyReviewRequest {
    ClassifierOnlyReviewRequest {
        transcript: vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "child task instructions".into(),
        )],
        action: handback_classifier_input("the child's complete report"),
    }
}

fn flag() -> ClassifierOnlyPolicy {
    ClassifierOnlyPolicy {
        on_block: ClassifierOnlyOnBlock::Flag,
    }
}

#[tokio::test]
async fn native_ignored_deny_sources_still_run_the_model_query() {
    use lingxi_core::types::SettingsScope;
    for source in [
        PermissionRuleSource::Settings(SettingsScope::User),
        PermissionRuleSource::Settings(SettingsScope::Project),
        PermissionRuleSource::Settings(SettingsScope::Local),
        PermissionRuleSource::Session,
    ] {
        let (gate, classifier) = fixture(
            vec![rule(source, PermissionBehavior::Deny, "SubagentHandback")],
            vec![reply("<block>no")],
        );
        let review_request = request();
        let result = gate
            .check_classifier_only_with_context_or_abort(
                "SubagentHandback",
                &json!({"message":"the child's complete report"}),
                &PermissionCheckContext::default(),
                flag(),
                &review_request,
            )
            .await
            .unwrap();
        assert!(matches!(result.permission, PermissionOutcome::Allow { .. }));
        assert_eq!(result.review, Some(ReportReview::Passed));
        assert_eq!(classifier.captured.lock().unwrap().len(), 1);
        assert_eq!(*classifier.reports.lock().unwrap(), [review_request]);
    }
}

#[tokio::test]
async fn every_other_deny_source_still_rejects_before_review() {
    use lingxi_core::types::SettingsScope;
    for source in [
        PermissionRuleSource::Settings(SettingsScope::Managed),
        PermissionRuleSource::Command,
        PermissionRuleSource::CliArg,
        PermissionRuleSource::FlagSettings,
        PermissionRuleSource::ToolsNarrowing,
        PermissionRuleSource::McpServerPolicy,
    ] {
        let (gate, classifier) = fixture(
            vec![rule(source, PermissionBehavior::Deny, "SubagentHandback")],
            Vec::new(),
        );
        let result = gate
            .check_classifier_only_with_context_or_abort(
                "SubagentHandback",
                &json!({"message":"report"}),
                &PermissionCheckContext::default(),
                flag(),
                &request(),
            )
            .await
            .unwrap();
        assert!(matches!(result.permission, PermissionOutcome::Deny { .. }));
        assert_eq!(result.review, None);
        assert!(classifier.captured.lock().unwrap().is_empty(), "{source:?}");
    }
}

#[tokio::test]
async fn saved_allow_and_safe_allow_paths_cannot_skip_classifier_only_review() {
    for name in ["SubagentHandback", "Read", "TodoWrite"] {
        let (gate, classifier) = fixture(
            vec![rule(
                PermissionRuleSource::Session,
                PermissionBehavior::Allow,
                name,
            )],
            vec![
                reply("<block>yes"),
                reply("<block>yes</block><reason>Unsafe report</reason>"),
            ],
        );
        let result = gate
            .check_classifier_only_with_context_or_abort(
                name,
                &json!({"message":"report"}),
                &PermissionCheckContext::default(),
                flag(),
                &request(),
            )
            .await
            .unwrap();
        assert!(matches!(result.permission, PermissionOutcome::Allow { .. }));
        assert_eq!(
            result.review,
            Some(ReportReview::Blocked {
                reason: "Unsafe report".into()
            })
        );
        assert_eq!(classifier.captured.lock().unwrap().len(), 2, "{name}");
    }
}

#[tokio::test]
async fn mode_ask_empty_action_and_missing_binding_never_prompt() {
    let (gate, classifier) = fixture(Vec::new(), Vec::new());
    let non_auto = PermissionCheckContext {
        mode_override: Some("plan".into()),
        ..Default::default()
    };
    let mut empty = request();
    empty.action.clear();
    for (ctx, review_request) in [
        (non_auto, request()),
        (PermissionCheckContext::default(), empty),
    ] {
        let result = gate
            .check_classifier_only_with_context_or_abort(
                "SubagentHandback",
                &json!({"message":"report"}),
                &ctx,
                flag(),
                &review_request,
            )
            .await
            .unwrap();
        assert!(matches!(result.permission, PermissionOutcome::Deny { .. }));
        assert_eq!(result.review, None);
    }
    assert!(classifier.captured.lock().unwrap().is_empty());
    let (asked, classifier) = fixture(
        vec![rule(
            PermissionRuleSource::Session,
            PermissionBehavior::Ask,
            "SubagentHandback",
        )],
        Vec::new(),
    );
    let result = asked
        .check_classifier_only_with_context_or_abort(
            "SubagentHandback",
            &json!({"message":"report"}),
            &PermissionCheckContext::default(),
            flag(),
            &request(),
        )
        .await
        .unwrap();
    assert!(matches!(result.permission, PermissionOutcome::Deny { .. }));
    assert!(classifier.captured.lock().unwrap().is_empty());
    let unbound = PolicyPermissionGate::new(
        Arc::new(PermissionPolicy::new(PermissionMode::Auto)),
        Arc::new(NoPrompt),
    );
    let result = unbound
        .check_classifier_only_with_context_or_abort(
            "SubagentHandback",
            &json!({"message":"report"}),
            &PermissionCheckContext::default(),
            flag(),
            &request(),
        )
        .await
        .unwrap();
    assert!(matches!(result.permission, PermissionOutcome::Deny { .. }));
    assert_eq!(result.review, None);
}

#[tokio::test]
async fn typed_query_failure_is_delivered_with_its_metadata_for_only_this_invocation() {
    let (gate, classifier) = fixture(
        Vec::new(),
        vec![
            Err(QueryError::UnavailableDetails {
                message: "not a refusal or a policy verdict".into(),
                http_status: Some(429),
                error_kind: Some("wall_clock_timeout".into()),
            }),
            reply("<block>no"),
        ],
    );
    let first = gate
        .check_classifier_only_with_context_or_abort(
            "SubagentHandback",
            &json!({"message":"report"}),
            &PermissionCheckContext::default(),
            flag(),
            &request(),
        )
        .await
        .unwrap();
    assert_eq!(
        first.review,
        Some(ReportReview::Unavailable {
            model: "scripted-classifier-model".into(),
            http_status: Some(429),
            error_kind: Some("wall_clock_timeout".into()),
            failure_kind: None,
        })
    );
    assert!(matches!(first.permission, PermissionOutcome::Allow { .. }));
    let second = gate
        .check_classifier_only_with_context_or_abort(
            "SubagentHandback",
            &json!({"message":"report"}),
            &PermissionCheckContext::default(),
            flag(),
            &request(),
        )
        .await
        .unwrap();
    assert_eq!(second.review, Some(ReportReview::Passed));
    assert_eq!(classifier.captured.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn flagging_delivery_preserves_native_denial_counter_distinctions() {
    let policy = Arc::new(PermissionPolicy::new(PermissionMode::Auto));
    policy.denial_tracking.lock().unwrap().record_auto_deny();
    let classifier = Arc::new(ScriptedQueries {
        replies: Mutex::new(
            vec![
                Err(QueryError::UnavailableDetails {
                    message: "offline".into(),
                    http_status: None,
                    error_kind: None,
                }),
                reply("<block>yes"),
                reply("<block>yes</block><reason>Actual block</reason>"),
                reply("<block>no"),
            ]
            .into(),
        ),
        captured: Mutex::new(Vec::new()),
        reports: Mutex::new(Vec::new()),
    });
    let gate = PolicyPermissionGate::new(policy.clone(), Arc::new(NoPrompt));
    assert!(gate.loop_classifier_handle().set(classifier).is_ok());
    for expected in [1, 2, 0] {
        let result = gate
            .check_classifier_only_with_context_or_abort(
                "SubagentHandback",
                &json!({"message":"report"}),
                &PermissionCheckContext::default(),
                flag(),
                &request(),
            )
            .await
            .unwrap();
        assert!(matches!(result.permission, PermissionOutcome::Allow { .. }));
        assert_eq!(
            policy.denial_tracking.lock().unwrap().consecutive_denials,
            expected
        );
    }
    assert_eq!(policy.denial_tracking.lock().unwrap().total_denials, 2);
}
