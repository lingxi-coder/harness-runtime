//! Physical request retry and query-state projection for the native July
//! server-fallback beta rejection repair.

#[path = "common/thinking_display_fixture.rs"]
mod fixture;

use fixture::{captures, request, run, variable, Action, Capture, MODEL};
use lingxi_llm_client::providers::anthropic::fallback_request::{
    LaneMode, RequestPolicy, ServerLane, DEFAULT_BETA, EXPLICIT_BETA,
};
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState};
use serde_json::{json, Value};
use std::sync::Arc;

const CATEGORY_REJECTION: &str =
    "Invalid beta header `server-side-fallback-2026-07-01` for anthropic-beta";
const DEFAULT_UNCONFIGURED: &str = "has no default fallback configuration";

fn policy(mode: LaneMode) -> RequestPolicy {
    RequestPolicy {
        lane: Some(ServerLane {
            for_model: MODEL.into(),
            model: "server-fallback-target".into(),
            mode,
        }),
        explicit_target_eligible: true,
        beta_transport_enabled: true,
        ..Default::default()
    }
}

fn with_server_fallback(
    mut request: llm_runtime::LlmRequest,
    mode: LaneMode,
) -> llm_runtime::LlmRequest {
    request.execution.server_fallback = Some(policy(mode));
    request
}

fn profile_service(capture: Arc<Capture>) -> ApiService {
    let profile = |provider_id: Value, profile_name: &str, base_url: &str| {
        json!({
            "provider_id":provider_id,
            "profile_name":profile_name,
            "base_url":base_url,
            "protocol":"anthropic_messages",
            "auth":"none",
            "credential":{"type":"none"},
            "models":[{
                "display_model":MODEL,
                "request_model":MODEL,
                "billing_model":MODEL,
                "capabilities":{
                    "streaming":true,"tools":true,"reasoning":true,
                    "vision":false,"documents":false,"structured_output":false
                }
            }]
        })
    };
    let config: ClientConfig = serde_json::from_value(json!({"providers":[
        profile(json!("anthropic_first_party"), "direct", "https://api.anthropic.com"),
        profile(json!("anthropic_first_party"), "alternate", "https://api.anthropic.com"),
        profile(json!({"custom":{"name":"gateway"}}), "gateway", "https://gateway.fixture.invalid")
    ]}))
    .unwrap();
    ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(config).unwrap()),
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "fixture",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
}

fn has_beta(headers: &[(String, String)], beta: &str) -> bool {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .is_some_and(|(_, value)| value.split(',').any(|candidate| candidate == beta))
}

fn rows(capture: &Capture) -> Vec<(Value, Vec<(String, String)>)> {
    capture.requests.lock().unwrap().clone()
}

#[tokio::test]
async fn default_beta_repairs_stream_and_nonstream_and_preserves_native_scope_guards() {
    variable("CLAUDE_CODE_EXTRA_BODY", None);
    variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", None);

    // The specialized repair precedes x-should-retry=false and retries once
    // with the explicit lane after the default beta has been rejected.
    for streaming in [false, true] {
        let capture = captures(vec![
            Action::Rejection(DEFAULT_UNCONFIGURED),
            Action::Success,
        ]);
        let api = profile_service(capture.clone());
        let scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
        let request = with_server_fallback(request(&scope, MODEL, false), LaneMode::Default)
            .with_profile("direct");
        assert!(run(&api, request, streaming).await);

        let sent = rows(&capture);
        assert_eq!(sent.len(), 2, "streaming={streaming}");
        assert_eq!(sent[0].0["fallbacks"], "default");
        assert_eq!(
            sent[1].0["fallbacks"],
            json!([{"model":"server-fallback-target"}])
        );
        assert!(has_beta(&sent[0].1, DEFAULT_BETA));
        assert!(!has_beta(&sent[1].1, DEFAULT_BETA));
        assert!(has_beta(&sent[1].1, EXPLICIT_BETA));
        let state = scope.server_fallback_betas(
            &llm_runtime::ProviderId::AnthropicFirstParty,
            "direct",
            llm_runtime::ProtocolFamily::AnthropicMessages,
        );
        let snapshot = state.snapshot();
        assert!(snapshot.default_rejected);
        assert!(!snapshot.default_active);
    }

    // The native category guard checks the registered July descriptor, not
    // whether this particular request emitted that header or had a lane.
    let unarmed_capture = captures(vec![Action::Rejection(CATEGORY_REJECTION), Action::Success]);
    let unarmed_api = profile_service(unarmed_capture.clone());
    let unarmed_scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    assert!(
        run(
            &unarmed_api,
            request(&unarmed_scope, MODEL, false).with_profile("direct"),
            false,
        )
        .await
    );
    let unarmed_rows = rows(&unarmed_capture);
    assert_eq!(unarmed_rows.len(), 2);
    assert!(unarmed_rows.iter().all(|(body, headers)| {
        body.get("fallbacks").is_none() && !has_beta(headers, DEFAULT_BETA)
    }));
    assert!(
        unarmed_scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages,
            )
            .snapshot()
            .default_rejected
    );

    let threaded_capture = captures(vec![Action::Rejection(CATEGORY_REJECTION), Action::Success]);
    let threaded_api = profile_service(threaded_capture.clone());
    let threaded_scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    let mut threaded_policy = policy(LaneMode::Default);
    threaded_policy.threaded_request = true;
    let mut threaded_request = request(&threaded_scope, MODEL, false).with_profile("direct");
    threaded_request.execution.server_fallback = Some(threaded_policy);
    assert!(run(&threaded_api, threaded_request, false).await);
    let threaded_rows = rows(&threaded_capture);
    assert_eq!(threaded_rows.len(), 2);
    assert!(threaded_rows.iter().all(|(body, headers)| {
        body.get("fallbacks").is_none() && !has_beta(headers, DEFAULT_BETA)
    }));
    assert!(
        threaded_scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages,
            )
            .snapshot()
            .default_rejected
    );

    let side_capture = captures(vec![Action::Rejection(CATEGORY_REJECTION), Action::Success]);
    let side_api = profile_service(side_capture.clone());
    let side_scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    side_api
        .execute_side_query_request(request(&side_scope, MODEL, true).with_profile("direct"))
        .await
        .expect("side query shares the native Anthropic bad-request repair catch");
    let side_rows = rows(&side_capture);
    assert_eq!(side_rows.len(), 2);
    assert!(side_rows.iter().all(|(body, headers)| {
        body.get("fallbacks").is_none() && !has_beta(headers, DEFAULT_BETA)
    }));
    assert!(
        side_scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages,
            )
            .snapshot()
            .default_rejected
    );

    // A sticky July beta can be rejected on a later main query with no current
    // fallback parameter. `category_beta_header` does not require `a1` or a
    // current lane, but the resulting rejection stays in this provider/profile.
    let capture = captures(vec![
        Action::Success,
        Action::Rejection(CATEGORY_REJECTION),
        Action::Success,
        Action::Success,
        Action::Success,
        Action::Rejection(CATEGORY_REJECTION),
        Action::Success,
        Action::Success,
    ]);
    let api = profile_service(capture.clone());
    let scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    assert!(
        run(
            &api,
            with_server_fallback(request(&scope, MODEL, false), LaneMode::Default)
                .with_profile("direct"),
            false,
        )
        .await
    );
    let before_sticky_rejection = scope.server_fallback_betas(
        &llm_runtime::ProviderId::AnthropicFirstParty,
        "direct",
        llm_runtime::ProtocolFamily::AnthropicMessages,
    );
    assert!(before_sticky_rejection.snapshot().default_active);

    assert!(
        run(
            &api,
            request(&scope, MODEL, false).with_profile("direct"),
            false
        )
        .await
    );
    let sent = rows(&capture);
    assert_eq!(sent.len(), 3);
    assert!(sent[1].0.get("fallbacks").is_none());
    assert!(sent[2].0.get("fallbacks").is_none());
    assert!(has_beta(&sent[1].1, DEFAULT_BETA));
    assert!(!has_beta(&sent[2].1, DEFAULT_BETA));
    assert!(before_sticky_rejection.snapshot().default_rejected);

    // The rejection persists into later direct-profile requests; a different
    // profile gets its own default-beta state. An Anthropic-compatible custom
    // profile uses the same native category catch, with an independent latch.
    assert!(
        run(
            &api,
            with_server_fallback(request(&scope, MODEL, false), LaneMode::Default)
                .with_profile("direct"),
            false,
        )
        .await
    );
    assert!(
        run(
            &api,
            with_server_fallback(request(&scope, MODEL, false), LaneMode::Default)
                .with_profile("alternate"),
            false,
        )
        .await
    );
    assert!(
        run(
            &api,
            with_server_fallback(request(&scope, MODEL, false), LaneMode::Default)
                .with_profile("gateway"),
            false,
        )
        .await
    );
    let sent = rows(&capture);
    assert_eq!(sent.len(), 7);
    assert!(!has_beta(&sent[3].1, DEFAULT_BETA));
    assert!(has_beta(&sent[3].1, EXPLICIT_BETA));
    assert_eq!(sent[4].0["fallbacks"], "default");
    assert!(has_beta(&sent[4].1, DEFAULT_BETA));
    assert!(sent[5].0.get("fallbacks").is_none());
    assert!(sent[6].0.get("fallbacks").is_none());
    assert!(!has_beta(&sent[5].1, DEFAULT_BETA));
    assert!(!has_beta(&sent[6].1, DEFAULT_BETA));
    let gateway_state = scope.server_fallback_betas(
        &llm_runtime::ProviderId::Custom {
            name: "gateway".into(),
        },
        "gateway",
        llm_runtime::ProtocolFamily::AnthropicMessages,
    );
    assert!(gateway_state.snapshot().default_rejected);
    assert!(before_sticky_rejection.snapshot().default_rejected);

    scope.reset_beta_rejections();
    assert!(before_sticky_rejection.snapshot().default_rejected);
    assert!(
        run(
            &api,
            with_server_fallback(request(&scope, MODEL, false), LaneMode::Default)
                .with_profile("direct"),
            false,
        )
        .await
    );
    let sent = rows(&capture);
    assert_eq!(sent.len(), 8);
    assert_eq!(sent[7].0["fallbacks"], "default");
    assert!(has_beta(&sent[7].1, DEFAULT_BETA));
    assert!(
        !scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages,
            )
            .snapshot()
            .default_rejected
    );
    assert!(capture.actions.lock().unwrap().is_empty());

    // The native one-shot guard is query-local even though the category
    // classifier itself does not require the July header to be on the retry.
    let retry_capture = captures(vec![
        Action::Rejection(CATEGORY_REJECTION),
        Action::Rejection(CATEGORY_REJECTION),
        Action::Success,
    ]);
    let retry_api = profile_service(retry_capture.clone());
    let retry_scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    assert!(
        !run(
            &retry_api,
            with_server_fallback(request(&retry_scope, MODEL, false), LaneMode::Default)
                .with_profile("direct"),
            false,
        )
        .await
    );
    let retry_rows = rows(&retry_capture);
    assert_eq!(retry_rows.len(), 2);
    assert_eq!(retry_capture.actions.lock().unwrap().len(), 1);
    assert!(has_beta(&retry_rows[0].1, DEFAULT_BETA));
    assert!(!has_beta(&retry_rows[1].1, DEFAULT_BETA));
    assert!(
        retry_scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages,
            )
            .snapshot()
            .default_rejected
    );

    // `a1` is captured before the extra-body spread: an internally selected
    // fallback remains eligible even when the final body overrides it.
    variable("CLAUDE_CODE_EXTRA_BODY", Some("{\"fallbacks\":\"manual\"}"));
    let override_capture = captures(vec![
        Action::Rejection(DEFAULT_UNCONFIGURED),
        Action::Success,
    ]);
    let override_api = profile_service(override_capture.clone());
    let override_scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    assert!(
        run(
            &override_api,
            with_server_fallback(request(&override_scope, MODEL, false), LaneMode::Default)
                .with_profile("direct"),
            false,
        )
        .await
    );
    let sent = rows(&override_capture);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0["fallbacks"], "manual");
    assert_eq!(sent[1].0["fallbacks"], "manual");
    assert!(has_beta(&sent[0].1, DEFAULT_BETA));
    assert!(!has_beta(&sent[1].1, DEFAULT_BETA));
    variable("CLAUDE_CODE_EXTRA_BODY", None);

    // A user extra-body fallback does not set native `a1`. Here the final body
    // says "default" and sticky July is sent, but no host fallback descriptor
    // was selected, so default_unconfigured is terminal and does not latch.
    let no_descriptor_capture = captures(vec![
        Action::Success,
        Action::Rejection(DEFAULT_UNCONFIGURED),
    ]);
    let no_descriptor_api = profile_service(no_descriptor_capture.clone());
    let no_descriptor_scope = llm_runtime::thinking_scope::ThinkingRecoveryScope::default();
    assert!(
        run(
            &no_descriptor_api,
            with_server_fallback(
                request(&no_descriptor_scope, MODEL, false),
                LaneMode::Default,
            )
            .with_profile("direct"),
            false,
        )
        .await
    );
    variable(
        "CLAUDE_CODE_EXTRA_BODY",
        Some("{\"fallbacks\":\"default\"}"),
    );
    assert!(
        !run(
            &no_descriptor_api,
            request(&no_descriptor_scope, MODEL, false).with_profile("direct"),
            false,
        )
        .await
    );
    let sent = rows(&no_descriptor_capture);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].0["fallbacks"], "default");
    assert!(has_beta(&sent[1].1, DEFAULT_BETA));
    assert!(
        !no_descriptor_scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages,
            )
            .snapshot()
            .default_rejected
    );

    variable("CLAUDE_CODE_EXTRA_BODY", None);
    variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", None);
}
