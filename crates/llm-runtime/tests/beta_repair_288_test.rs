//! Current named beta failures retire immediately through actual SDK dispatch.
#[path = "common/thinking_display_fixture.rs"]
mod fixture;
use fixture::*;
use lingxi_llm_client::providers::anthropic::{
    beta_repair::Beta, thinking_display::DisplayProbeBudget,
};

#[tokio::test(start_paused = true)]
async fn named_rejection_lifetime_priority_and_model_isolation_reach_both_drivers() {
    for name in [
        branding::MAX_RETRIES_ENV,
        branding::THINKING_DISPLAY_UPDATES_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
    ] {
        variable(name, None);
    }
    let initial_failures = DisplayProbeBudget::for_process().failures();
    for streaming in [false, true] {
        for (index,message,strong) in [
            (0,"Unrecognized thinking-display-updates-2026-08-18 in anthropic-beta",false),
            (1,"Unexpected value(s) `thinking-display-updates-2026-08-18` for the `anthropic-beta` header",true),
            (2,"thinking.adaptive.display: Input should be omitted",true),
            (3,"capability_rejected: beta_header:thinking-display-updates-2026-08-18",true),
        ] {
            let model=format!("claude-opus-4-7-repair-{streaming}-{index}");
            let capture=captures(vec![Action::Rejection(message),Action::Error(418,"api_error"),Action::Success,Action::Success]);
            let api=service(capture.clone(),true,false,&model);
            let scope=Default::default();
            assert!(!run(&api,request(&scope,&model,false),streaming).await);
            assert!(scope.beta_rejections().rejected(Beta::ThinkingDisplayUpdates));
            // Retirement precedes trial success; failure must not restore it.
            assert!(run(&api,request(&scope,&model,false),streaming).await);
            let isolated=Default::default();
            assert!(run(&api,request(&isolated,&model,false),streaming).await);
            let rows=capture.requests.lock().unwrap();assert_eq!(rows.len(),4);
            for (row,expected) in rows.iter().zip([true,false,false,!strong]) {
                assert_eq!(has_beta(&row.1),expected,"{message}");
                assert_eq!(row.0["thinking"]["display"]=="updates",expected);
                assert_eq!(row.0["messages"],rows[0].0["messages"]);
                assert_eq!(row.1.iter().any(|(name,value)|name.eq_ignore_ascii_case("anthropic-beta")&&value.split(',').any(|v|v=="redact-thinking-2026-02-12")), !expected);
            }
        }
        // E3e first removes token-count for this conversation. The named
        // updates refusal then retires updates without spending ordinary retries.
        let model = format!("claude-opus-4-7-token-first-{streaming}");
        let capture = captures(vec![
            Action::Rejection(
                "invalid beta flag: thinking-display-updates-2026-08-18 in anthropic-beta",
            ),
            Action::Rejection("thinking.adaptive.display: Input should be omitted"),
            Action::Success,
        ]);
        let api = service(capture.clone(), true, false, &model);
        let scope = Default::default();
        assert!(run(&api, request(&scope, &model, false), streaming).await);
        let rows = capture.requests.lock().unwrap();
        assert_eq!(rows.len(), 3);
        for (index, row) in rows.iter().enumerate() {
            let betas = row
                .1
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .unwrap()
                .1
                .split(',')
                .collect::<Vec<_>>();
            assert_eq!(
                betas.contains(&Beta::ThinkingTokenCount.header()),
                index == 0
            );
            assert_eq!(has_beta(&row.1), index < 2);
        }
    }
    // Explicit beta replacement is outside the computed kw snapshot. A
    // rejected caller override must not become an infinite automatic repair.
    variable("CLAUDE_CODE_EXTRA_BODY",Some("{\"betas\":[\"thinking-display-updates-2026-08-18\"],\"thinking\":{\"type\":\"adaptive\",\"display\":\"updates\"}}"));
    let model = "claude-opus-4-7-explicit-beta";
    let capture = captures(vec![Action::Rejection(
        "thinking.adaptive.display: Input should be omitted",
    )]);
    let api = service(capture.clone(), true, false, model);
    let scope = Default::default();
    assert!(!run(&api, request(&scope, model, false), true).await);
    assert_eq!(capture.requests.lock().unwrap().len(), 1);
    assert!(!scope
        .beta_rejections()
        .rejected(Beta::ThinkingDisplayUpdates));
    variable("CLAUDE_CODE_EXTRA_BODY", None);
    assert_eq!(
        DisplayProbeBudget::for_process().failures(),
        initial_failures
    );
}
