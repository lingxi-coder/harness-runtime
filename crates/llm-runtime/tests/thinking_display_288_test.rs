//! Native display field and probe ownership reach actual SDK requests.
#[path = "common/thinking_display_fixture.rs"]
mod fixture;
use fixture::*;
use futures::StreamExt;

#[tokio::test(start_paused = true)]
async fn native_display_probe_heals_once_and_keeps_conversation_and_provider_ownership() {
    for name in [
        branding::MAX_RETRIES_ENV,
        branding::THINKING_DISPLAY_UPDATES_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
    ] {
        variable(name, None)
    }
    for streaming in [false, true] {
        let capture = captures(vec![
            Action::Error(400, "invalid_request_error"),
            Action::Success,
            Action::Success,
            Action::Success,
            Action::Success,
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
        assert!(
            run(&api, request(&scope, MODEL, false), streaming).await,
            "mode={streaming}; requests={:?}",
            capture.requests.lock().unwrap()
        );
        assert!(scope.beta_rejections().rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
        ));
        api.set_thinking_signature_stripped(false);
        assert!(
            run(&api, request(&scope, MODEL, false), streaming).await,
            "mode={streaming}; requests={:?}",
            capture.requests.lock().unwrap()
        );
        let isolated = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
        assert!(run(&api, request(&isolated, MODEL, false), streaming).await);
        scope.reset_beta_rejections();
        assert!(
            run(&api, request(&scope, MODEL, false), streaming).await,
            "mode={streaming}; requests={:?}",
            capture.requests.lock().unwrap()
        );
        let rows = capture.requests.lock().unwrap();
        assert_eq!(rows.len(), 5);
        for (index, (body, headers)) in rows.iter().enumerate() {
            let updates = matches!(index, 0 | 3 | 4);
            assert_eq!(body["thinking"]["display"] == "updates", updates);
            assert_eq!(has_beta(headers), updates);
            assert_eq!(body["messages"], rows[0].0["messages"]);
            assert_eq!(body["stream"] == true, streaming);
        }
        assert!(capture.actions.lock().unwrap().is_empty());
    }
    for (official, custom, side, model, flag) in [
        (true, false, false, MODEL, None),
        (false, true, false, MODEL, None),
        (false, false, true, MODEL, None),
        (false, false, false, "claude-haiku-4-5", None),
        (false, false, false, MODEL, Some("0")),
    ] {
        variable(branding::THINKING_DISPLAY_UPDATES_ENV, flag);
        let capture = captures(vec![Action::Error(400, "invalid_request_error")]);
        let api = service(capture.clone(), official, custom, model);
        let scope = Default::default();
        assert!(!run(&api, request(&scope, model, side), true).await);
        let rows = capture.requests.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(has_beta(&rows[0].1), official);
        assert!(!scope.beta_rejections().rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
        ));
    }
    variable(branding::THINKING_DISPLAY_UPDATES_ENV, None);
    for streaming in [false, true] {
        for message in [
            "signature in thinking block",
            "Fast mode is not enabled",
            "credit balance is too low",
            "organization has been disabled",
            "Output blocked by content filtering policy",
        ] {
            let capture = captures(vec![Action::Rejection(message)]);
            let api = service(capture.clone(), false, false, MODEL);
            let scope = Default::default();
            assert!(!run(&api, request(&scope, MODEL, false), streaming).await);
            assert_eq!(capture.requests.lock().unwrap().len(), 1, "{message}");
            assert!(!scope.beta_rejections().rejected(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
            ));
        }
    }
    // Native I4e commits when a stream controller opens, before any SSE frame.
    // A malformed complete nonstream response has not succeeded and cannot commit.
    for streaming in [false, true] {
        let capture = captures(vec![
            Action::Error(400, "invalid_request_error"),
            Action::EmptySuccess,
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        if streaming {
            let stream = api
                .stream_request(request(&scope, MODEL, false))
                .await
                .unwrap();
            assert!(scope.beta_rejections().rejected(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
            ));
            let _ = stream.collect::<Vec<_>>().await;
        } else {
            assert!(!run(&api, request(&scope, MODEL, false), false).await);
            assert!(!scope.beta_rejections().rejected(
                lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
            ));
        }
        assert_eq!(capture.requests.lock().unwrap().len(), 2);
    }
    // Explicit extra thinking and the proxy simulation flag precede automatic
    // updates; both remain unmodified and do not enter the unclaimed probe.
    for (extra, simulated) in [
        (
            Some("{\"thinking\":{\"type\":\"adaptive\",\"display\":\"omitted\"}}"),
            None,
        ),
        (None, Some("1")),
    ] {
        variable("CLAUDE_CODE_EXTRA_BODY", extra);
        variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", simulated);
        let capture = captures(vec![Action::Error(400, "invalid_request_error")]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        assert!(!run(&api, request(&scope, MODEL, false), true).await);
        let rows = capture.requests.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!has_beta(&rows[0].1));
        if extra.is_some() {
            assert_eq!(rows[0].0["thinking"]["display"], "omitted");
        }
    }
    variable("CLAUDE_CODE_EXTRA_BODY", None);
    variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", None);
    for flag in ["0", "false"] {
        variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", Some(flag));
        let capture = captures(vec![
            Action::Error(400, "invalid_request_error"),
            Action::Success,
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        assert!(run(&api, request(&scope, MODEL, false), true).await);
        assert!(has_beta(&capture.requests.lock().unwrap()[0].1));
        assert!(scope.beta_rejections().rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
        ));
    }
    variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", None);
    // Failed trial is counted once and does not commit the conversation latch.
    for failure in 0..2 {
        let capture = captures(vec![
            Action::Error(422, "invalid_request_error"),
            if failure == 0 {
                Action::Rejection("signature in thinking block")
            } else {
                Action::Error(422, "invalid_request_error")
            },
        ]);
        let api = service(capture.clone(), false, false, MODEL);
        let scope = Default::default();
        assert!(!run(&api, request(&scope, MODEL, false), true).await);
        assert_eq!(capture.requests.lock().unwrap().len(), 2);
        assert!(!scope.beta_rejections().rejected(
            lingxi_llm_client::providers::anthropic::beta_repair::Beta::ThinkingDisplayUpdates
        ));
        assert_eq!(lingxi_llm_client::providers::anthropic::thinking_display::DisplayProbeBudget::for_process().failures(),failure+1);
    }
    let capture = captures(vec![Action::Error(400, "invalid_request_error")]);
    let api = service(capture.clone(), false, false, MODEL);
    let scope = Default::default();
    assert!(!run(&api, request(&scope, MODEL, false), true).await);
    assert_eq!(capture.requests.lock().unwrap().len(), 1);
    assert!(
        has_beta(&capture.requests.lock().unwrap()[0].1),
        "process probe exhaustion does not itself disable the request feature"
    );
}
