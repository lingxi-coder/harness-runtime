//! Native IR is physical-call policy even when this request is not fast.
#[path = "common/thinking_display_fixture.rs"]
mod fixture;
use fixture::*;
use lingxi_core::host::fast_mode::ModelRejections;
use lingxi_core::host::refusal_driver::FallbackTargetContext;
fn target_context(model: &str) -> FallbackTargetContext {
    FallbackTargetContext {
        user_model: "origin".into(),
        turn_override: Some(model.into()),
        ..Default::default()
    }
}
use lingxi_llm_client::providers::anthropic::{
    beta_repair::Beta, thinking_display::DisplayProbeBudget,
};

#[tokio::test(start_paused = true)]
async fn full_ir_routes_target_history_and_model_match_before_display_repair() {
    for name in [
        branding::MAX_RETRIES_ENV,
        branding::THINKING_DISPLAY_UPDATES_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
    ] {
        variable(name, None);
    }
    let history = ModelRejections::for_process();
    let budget = DisplayProbeBudget::for_process();
    for streaming in [false, true] {
        for target in [false, true] {
            for rejected in [false, true] {
                history.reset();
                if rejected {
                    history.reject(MODEL);
                    history.rearm();
                }
                let recognized = target || rejected;
                let mut actions = vec![Action::Rejection(
                    "'claude-opus-4-7-20260901' does not support the `speed` parameter",
                )];
                if !recognized {
                    actions.push(Action::Success);
                }
                let capture = captures(actions);
                let api = service(capture.clone(), false, false, MODEL);
                let scope = Default::default();
                let mut req = request(&scope, MODEL, false);
                if target {
                    req.execution.refusal_fallback_context = Some(target_context(MODEL));
                }
                assert_eq!(run(&api, req, streaming).await, !recognized);
                assert_eq!(
                    capture.requests.lock().unwrap().len(),
                    if recognized { 1 } else { 2 }
                );
                assert_eq!(
                    scope
                        .beta_rejections()
                        .rejected(Beta::ThinkingDisplayUpdates),
                    !recognized
                );
                assert_eq!(budget.failures(), 0);
            }
        }
        history.reset();
        history.reject(MODEL);
        let capture = captures(vec![
            Action::Rejection("'claude-sonnet-4-6' does not support the `speed` parameter"),
            Action::Success,
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        let mut req = request(&scope, MODEL, false);
        req.execution.refusal_fallback_context = Some(target_context(MODEL));
        assert!(run(&api, req, streaming).await);
        assert_eq!(capture.requests.lock().unwrap().len(), 2);
    }
    history.reset();
    // Target ownership follows the query facts, rather than a matching model
    // string alone. Raw equality governs whether a session latch is still live.
    let contexts = [
        (
            FallbackTargetContext {
                user_model: MODEL.into(),
                turn_override: Some(MODEL.into()),
                ..Default::default()
            },
            false,
        ),
        (
            FallbackTargetContext {
                user_model: "other".into(),
                chain_step_model: Some(MODEL.into()),
                ..Default::default()
            },
            true,
        ),
        (
            FallbackTargetContext {
                user_model: MODEL.into(),
                chain_step_model: Some(MODEL.into()),
                ..Default::default()
            },
            false,
        ),
        (
            FallbackTargetContext {
                user_model: "other".into(),
                chain_step_model: Some("another".into()),
                ..Default::default()
            },
            false,
        ),
        (
            FallbackTargetContext {
                user_model: MODEL.into(),
                latched_model: Some(MODEL.into()),
                main_loop_override: Some(MODEL.into()),
                ..Default::default()
            },
            true,
        ),
        (
            FallbackTargetContext {
                user_model: MODEL.into(),
                latched_model: Some(MODEL.into()),
                main_loop_override: Some(format!("{MODEL}[1m]")),
                ..Default::default()
            },
            false,
        ),
        (
            FallbackTargetContext {
                user_model: "other".into(),
                latched_model: Some(MODEL.into()),
                main_loop_override: Some(MODEL.into()),
                ..Default::default()
            },
            true,
        ),
        (target_context(MODEL), true),
    ];
    for streaming in [false, true] {
        for (context, recognized) in &contexts {
            let mut actions = vec![Action::Rejection(
                "'claude-opus-4-7' does not support the `speed` parameter",
            )];
            if !recognized {
                actions.push(Action::Success);
            }
            let capture = captures(actions);
            let api = service(capture.clone(), false, false, MODEL);
            let scope = Default::default();
            let mut req = request(&scope, MODEL, false);
            req.execution.refusal_fallback_context = Some(context.clone());
            assert_eq!(run(&api, req, streaming).await, !recognized, "{context:?}");
            assert_eq!(
                capture.requests.lock().unwrap().len(),
                if *recognized { 1 } else { 2 }
            );
            assert_eq!(
                scope
                    .beta_rejections()
                    .rejected(Beta::ThinkingDisplayUpdates),
                !recognized
            );
            assert_eq!(budget.failures(), 0);
        }
    }
    // The ordinary high-level API captures the same host task context as main
    // and child adapters. It does not put the authority field into the body.
    let capture = captures(vec![Action::Rejection(
        "'claude-opus-4-7' does not support the `speed` parameter",
    )]);
    let api = service(capture.clone(), false, false, MODEL);
    let result = lingxi_core::host::refusal_driver::scope_fallback_target(
        target_context(MODEL),
        api.stream(MODEL, None, None, vec![], vec![], None, None),
    )
    .await;
    assert!(result.is_err());
    let rows = capture.requests.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].0.get("refusal_fallback_context").is_none());
    assert!(has_beta(&rows[0].1));
    assert_eq!(budget.failures(), 0);
    drop(rows);
    for streaming in [false, true] {
        const FAST_MODEL: &str = "claude-opus-4-8";
        history.reset();
        let capture = captures(vec![
            Action::Rejection("'claude-opus-4-8' does not support the `speed` parameter"),
            Action::Success,
            Action::Success,
            Action::Rejection("'claude-opus-4-8' does not support the `speed` parameter"),
            Action::Success,
        ]);
        let api = service(capture.clone(), true, false, FAST_MODEL).with_fast_policy_source(
            std::sync::Arc::new(|| llm_runtime::model::fast_admission::Policy {
                flag_fast: true,
                cached_org_enabled: true,
                remote: true,
                agent_owned_remote: false,
                ..Default::default()
            }),
        );
        let scope = Default::default();
        let fast_request = |target: bool| {
            let mut req = request(&scope, FAST_MODEL, false);
            req.input.service_tier = Some(lingxi_llm_client::protocol::ServiceTier::Fast);
            req.execution.refusal_fallback_context = target.then(|| target_context(FAST_MODEL));
            req
        };
        assert!(
            run(&api, fast_request(true), streaming).await,
            "{:?}",
            capture.requests.lock().unwrap()
        );
        assert!(history.blocked(FAST_MODEL));
        assert!(run(&api, fast_request(false), streaming).await);
        history.rearm();
        assert!(!history.blocked(FAST_MODEL));
        assert!(history.known_rejected(FAST_MODEL));
        assert!(run(&api, fast_request(false), streaming).await);
        let rows = capture.requests.lock().unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows.iter()
                .map(|(body, _)| body["speed"] == "fast")
                .collect::<Vec<_>>(),
            [true, false, false, true, false]
        );
        assert!(rows.iter().all(|(_, headers)| has_beta(headers)));
        assert_eq!(rows[0].0["messages"], rows[1].0["messages"]);
        assert_eq!(budget.failures(), 0);
    }
    history.reset();
}
