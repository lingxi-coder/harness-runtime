//! The full native policy guard reaches both real SDK request modes.
#[path = "common/thinking_display_fixture.rs"]
mod fixture;
use fixture::*;
use lingxi_llm_client::providers::anthropic::{
    beta_repair::Beta, thinking_display::DisplayProbeBudget,
};

#[tokio::test(start_paused = true)]
async fn native_policy_refusals_do_not_start_display_trials_and_failed_trials_still_spend() {
    for name in [
        branding::MAX_RETRIES_ENV,
        branding::THINKING_DISPLAY_UPDATES_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
    ] {
        variable(name, None);
    }
    let budget = DisplayProbeBudget::for_process();
    assert_eq!(budget.failures(), 0);
    for streaming in [false, true] {
        for (status, message) in [
            (400, "could not process image"),
            (400, "messages.0.content.1.document.source: invalid"),
            (400, "Advisor tool result content could not be processed"),
            (400, "tools.5.model: invalid"),
            (
                400,
                "thinking.adaptive.block_binding: Extra inputs are not permitted",
            ),
            (400, "thinking.type: enabled is not supported"),
            (400, "tool_addition references an unknown tool"),
            (400, "tool definitions are not available on this platform"),
            (400, "cache_control: unknown field"),
            (400, "cache_control role\u{feff}system: unknown field"),
            (400, "output_config.foo: unknown field"),
            (400, "output_config.timing: unknown field"),
            (400, "effort parameter is not supported"),
            (400, "output_config.format: Extra inputs are not permitted"),
            (422, "Input tag 'tool_addition'"),
            (422, "Input tag 'tool_definition'"),
            (422, "Input tag 'advisor_20260301'"),
        ] {
            let capture = captures(vec![Action::RejectionStatus(status, message)]);
            let api = service(capture.clone(), false, false, MODEL);
            let scope = Default::default();
            assert!(
                !run(&api, request(&scope, MODEL, false), streaming).await,
                "{status} {message}"
            );
            let rows = capture.requests.lock().unwrap();
            assert_eq!(rows.len(), 1, "{status} {message}");
            assert!(has_beta(&rows[0].1));
            assert_eq!(rows[0].0["thinking"]["display"], "updates");
            assert!(!scope
                .beta_rejections()
                .rejected(Beta::ThinkingDisplayUpdates));
            assert_eq!(budget.failures(), 0);
        }
        // A near miss still admits the native unclaimed probe.
        for action in [
            Action::PlainRejection("could not process image"),
            Action::TopLevelRejection("tools.5.model: invalid"),
        ] {
            let capture = captures(vec![action]);
            let api = service(capture.clone(), false, false, MODEL);
            let scope = Default::default();
            assert!(!run(&api, request(&scope, MODEL, false), streaming).await);
            assert_eq!(capture.requests.lock().unwrap().len(), 1);
            assert_eq!(budget.failures(), 0);
            assert!(!scope
                .beta_rejections()
                .rejected(Beta::ThinkingDisplayUpdates));
        }
        let capture = captures(vec![
            Action::Rejection("tool_additions unsupported"),
            Action::Success,
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        assert!(run(&api, request(&scope, MODEL, false), streaming).await);
        let rows = capture.requests.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(has_beta(&rows[0].1));
        assert!(!has_beta(&rows[1].1));
        assert_eq!(rows[0].0["messages"], rows[1].0["messages"]);
        assert!(scope
            .beta_rejections()
            .rejected(Beta::ThinkingDisplayUpdates));
        assert_eq!(budget.failures(), 0);
        let capture = captures(vec![
            Action::PlainRejection("fixture unknown request rejection"),
            Action::Success,
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        assert!(run(&api, request(&scope, MODEL, false), streaming).await);
        assert_eq!(capture.requests.lock().unwrap().len(), 2);
        assert!(scope
            .beta_rejections()
            .rejected(Beta::ThinkingDisplayUpdates));
    }
    // A pF-only error on the trial request does spend one failure. It does not
    // become IR recognition or a healed conversation rejection.
    let capture = captures(vec![
        Action::Error(400, "invalid_request_error"),
        Action::Rejection("cache_control: unknown field"),
        Action::Error(400, "invalid_request_error"),
        Action::Success,
    ]);
    let api = service(capture.clone(), false, false, MODEL);
    let failed = Default::default();
    assert!(!run(&api, request(&failed, MODEL, false), true).await);
    assert_eq!(budget.failures(), 1);
    assert!(!failed
        .beta_rejections()
        .rejected(Beta::ThinkingDisplayUpdates));
    let fresh = Default::default();
    assert!(run(&api, request(&fresh, MODEL, false), true).await);
    assert_eq!(budget.failures(), 1);
    let rows = capture.requests.lock().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter()
            .map(|(_, headers)| has_beta(headers))
            .collect::<Vec<_>>(),
        [true, false, true, false]
    );
}
