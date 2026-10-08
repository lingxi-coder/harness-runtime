//! Physical SDK dispatch: host authority stays outside provider input.
#[path = "common/thinking_display_fixture.rs"]
mod fixture;
use fixture::*;
use lingxi_llm_client::providers::anthropic::fallback_request::{
    LaneMode, RequestPolicy, ServerLane, DEFAULT_BETA, EXPLICIT_BETA,
};
use serde_json::{json, Value};
fn policy(mode: LaneMode) -> RequestPolicy {
    RequestPolicy {
        lane: Some(ServerLane {
            for_model: MODEL.into(),
            model: "Tar[1M]get[2m]".into(),
            mode,
        }),
        explicit_target_eligible: true,
        beta_transport_enabled: true,
        ..Default::default()
    }
}
fn betas(row: &(Value, Vec<(String, String)>)) -> Vec<&str> {
    row.1
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| value.split(',').collect())
        .unwrap_or_default()
}
#[tokio::test]
async fn native_server_request_parameters_and_sticky_betas_reach_physical_dispatches() {
    variable("CLAUDE_CODE_EXTRA_BODY", None);
    variable("CLAUDE_CODE_SIMULATE_PROXY_USAGE", None);
    let mut calls = 0;
    for custom in [false, true] {
        for streaming in [false, true] {
            for mode in [LaneMode::Explicit, LaneMode::Default] {
                for silent in [false, true] {
                    for threaded in [false, true] {
                        let capture = captures(vec![Action::Success]);
                        let api = service(capture.clone(), !custom, custom, MODEL);
                        let scope = Default::default();
                        let mut req = request(&scope, MODEL, false);
                        let mut p = policy(mode);
                        p.silent_arm = silent;
                        p.threaded_request = threaded;
                        req.execution.server_fallback = Some(p);
                        assert!(run(&api, req, streaming).await);
                        let rows = capture.requests.lock().unwrap();
                        assert_eq!(rows.len(), 1);
                        calls += rows.len();
                        let admitted = !custom && !silent && !threaded;
                        assert_eq!(
                            rows[0].0.get("fallbacks"),
                            admitted
                                .then(|| if mode == LaneMode::Default {
                                    json!("default")
                                } else {
                                    json!([{"model":"Target"}])
                                })
                                .as_ref()
                        );
                        assert_eq!(
                            betas(&rows[0]).contains(&if mode == LaneMode::Default {
                                DEFAULT_BETA
                            } else {
                                EXPLICIT_BETA
                            }),
                            admitted
                        );
                    }
                }
            }
        }
    }
    let capture = captures(vec![Action::Success; 5]);
    let api = service(capture.clone(), true, false, MODEL);
    let scope: llm_runtime::thinking_scope::ThinkingRecoveryScope = Default::default();
    let mut req = request(&scope, MODEL, false);
    req.execution.server_fallback = Some(policy(LaneMode::Explicit));
    assert!(run(&api, req, true).await);
    assert!(run(&api, request(&scope, MODEL, false), false).await);
    let mut req = request(&scope, MODEL, false);
    req.execution.server_fallback = Some(policy(LaneMode::Default));
    assert!(run(&api, req, false).await);
    let gateway_capture = captures(vec![Action::Success]);
    let gateway = service(gateway_capture.clone(), false, true, MODEL);
    assert!(run(&gateway, request(&scope, MODEL, false), true).await);
    let gateway_rows = gateway_capture.requests.lock().unwrap();
    calls += gateway_rows.len();
    assert_eq!(gateway_rows.len(), 1);
    assert!(gateway_rows[0].0.get("fallbacks").is_none());
    assert!(!betas(&gateway_rows[0]).contains(&EXPLICIT_BETA));
    assert!(!betas(&gateway_rows[0]).contains(&DEFAULT_BETA));
    drop(gateway_rows);
    let old = scope.server_fallback_betas(
        &llm_runtime::ProviderId::AnthropicFirstParty,
        "direct",
        llm_runtime::ProtocolFamily::AnthropicMessages,
    );
    old.reject(LaneMode::Explicit);
    let mut req = request(&scope, MODEL, false);
    req.execution.server_fallback = Some(policy(LaneMode::Default));
    assert!(run(&api, req, true).await);
    scope.reset_beta_rejections();
    assert!(run(&api, request(&scope, MODEL, false), true).await);
    let rows = capture.requests.lock().unwrap();
    assert_eq!(rows.len(), 5);
    calls += rows.len();
    assert!(rows[1].0.get("fallbacks").is_none());
    assert!(betas(&rows[1]).contains(&EXPLICIT_BETA));
    let shared = betas(&rows[2]);
    assert!(
        shared.iter().position(|&b| b == EXPLICIT_BETA)
            < shared.iter().position(|&b| b == DEFAULT_BETA)
    );
    assert!(rows[3].0.get("fallbacks").is_none());
    assert!(!betas(&rows[3]).contains(&EXPLICIT_BETA));
    assert!(betas(&rows[3]).contains(&DEFAULT_BETA));
    assert!(rows[4].0.get("fallbacks").is_none());
    assert!(!betas(&rows[4]).contains(&EXPLICIT_BETA));
    assert!(!betas(&rows[4]).contains(&DEFAULT_BETA));
    assert!(old.snapshot().explicit_rejected);
    assert!(
        !scope
            .server_fallback_betas(
                &llm_runtime::ProviderId::AnthropicFirstParty,
                "direct",
                llm_runtime::ProtocolFamily::AnthropicMessages
            )
            .snapshot()
            .explicit_rejected
    );
    drop(rows);
    let capture = captures(vec![Action::Success]);
    let api = service(capture.clone(), true, false, MODEL);
    let scope = Default::default();
    let mut req = request(&scope, MODEL, true);
    req.execution.server_fallback = Some(policy(LaneMode::Default));
    assert!(run(&api, req, true).await);
    let rows = capture.requests.lock().unwrap();
    calls += rows.len();
    assert!(rows[0].0.get("fallbacks").is_none());
    assert!(!betas(&rows[0]).contains(&DEFAULT_BETA));
    drop(rows);
    for streaming in [false, true] {
        variable(
            "CLAUDE_CODE_EXTRA_BODY",
            Some("{\"fallbacks\":\"manual\",\"model\":\"extra-model\"}"),
        );
        let capture = captures(vec![Action::Success]);
        let api = service(capture.clone(), true, false, MODEL);
        let scope = Default::default();
        let mut req = request(&scope, MODEL, false);
        req.execution.server_fallback = Some(policy(LaneMode::Default));
        assert!(run(&api, req, streaming).await);
        let rows = capture.requests.lock().unwrap();
        calls += rows.len();
        assert_eq!(rows[0].0["fallbacks"], "manual");
        assert_eq!(rows[0].0["model"], "extra-model");
        assert!(betas(&rows[0]).contains(&DEFAULT_BETA));
    }
    variable("CLAUDE_CODE_EXTRA_BODY", None);
    assert_eq!(calls, 41);
}
