//! Schema-level checks: `deny_unknown_fields` rejects extras across categories;
//! a couple of representative payload structs round-trip via serde JSON.

use telemetry::{tengu, Verified};

#[test]
fn api_request_started_rejects_unknown_field() {
    let bad = r#"{"model":"m","provider":"p","endpoint":"/v1","request_id":"r","is_stream":false,"input_tokens_estimate":null,"surprise":true}"#;
    let r: Result<tengu::api::RequestStartedPayload, _> = serde_json::from_str(bad);
    assert!(r.is_err(), "deny_unknown_fields must reject 'surprise'");
}

#[test]
fn agent_started_round_trips() {
    let p = tengu::agent::StartedPayload {
        agent_id: Verified::assert_safe("a1".into()),
        agent_kind: tengu::agent::AgentKind::Main,
        parent_agent_id: None,
        session_id: Verified::assert_safe("s1".into()),
    };
    let j = serde_json::to_string(&p).unwrap();
    let _: tengu::agent::StartedPayload = serde_json::from_str(&j).unwrap();
}

#[test]
fn cost_budget_warning_round_trips() {
    // (Was `cost_recorded_round_trips`; the port-only `tengu_cost_recorded`
    // schema was dropped under strict parity — this exercises a representative
    // surviving cost-category payload.)
    let p = tengu::cost::BudgetWarningPayload {
        limit_usd: 1_000_000_000,
        current_usd: 800_000_000,
        percent_bps: 8000,
    };
    let j = serde_json::to_string(&p).unwrap();
    let back: tengu::cost::BudgetWarningPayload = serde_json::from_str(&j).unwrap();
    assert_eq!(back.percent_bps, 8000);
}

#[test]
fn memory_case_mismatch_preserves_proto_path() {
    use telemetry::pii::PiiTagged;
    let p = tengu::memory::CaseMismatchPayload {
        actual_name: Verified::assert_safe("claude.md".into()),
        path: PiiTagged::assert_pii_tagged_column("/x/y".into()),
    };
    let j = serde_json::to_string(&p).unwrap();
    // The PiiTagged inner is serialized as a string; emitter would store it
    // under a _PROTO_ key in the LogEventMetadata at emit time, but the
    // payload struct itself just holds the wrapper.
    assert!(j.contains("/x/y"));
}
