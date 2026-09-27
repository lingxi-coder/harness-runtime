//! The context-hint controller is reachable from the turn loop — and inert.
//!
//! Two gates stand between an ordinary turn and this negotiation:
//! `OrchestratorConfig::include_first_party_betas` (a custom Anthropic-wire
//! gateway must never inherit a private first-party beta) and the controller's
//! own env gate, which is off because the oracle's server-delivered
//! `tengu_hazel_osprey` is false. Both default closed, so no request changes
//! shape unless a host opts in twice.

use compaction::context_hint::create_context_hint_controller;

/// The default state: no controller at all, so `build_request_params` is never
/// even reached and the turn loop takes its ordinary seam.
#[test]
fn no_controller_without_the_first_party_gate() {
    assert!(
        create_context_hint_controller(false, "repl_main_thread").is_none(),
        "a non-first-party route must not negotiate"
    );
}

/// Only the MAIN turn negotiates. Subagents and side queries have their own API
/// paths and never reach `call_api_with_ptl_recovery`; this pins the oracle's
/// `querySource.startsWith("repl_main_thread")` half of the same rule.
#[test]
fn only_the_main_thread_negotiates() {
    assert!(create_context_hint_controller(true, "repl_main_thread").is_some());
    assert!(create_context_hint_controller(true, "sdk").is_none());
    assert!(create_context_hint_controller(true, "agent:reviewer").is_none());
}

/// With the route gate open but the env gate shut — the shipping default — a
/// controller exists but contributes NOTHING to the request, so the body and
/// headers are byte-identical to a turn that never had one.
#[test]
fn a_controller_on_a_first_party_route_is_still_inert_by_default() {
    let mut c = create_context_hint_controller(true, "repl_main_thread")
        .expect("first-party main thread gets a controller");
    assert!(
        !c.is_active(),
        "LINGXI_CONTEXT_HINT is unset in the test env, matching the shipping default"
    );
    assert!(
        c.build_request_params(&[]).is_none(),
        "an inactive controller adds no beta and no body"
    );
}

/// The turn loop must pass the CONFIG gate, not a constant.
///
/// Source-level, and deliberately so: the two gates are read inside
/// `call_api_with_ptl_recovery`, which no test here can drive without a live
/// API. Hardcoding `true` at that call site compiles, passes every other test,
/// and would silently send a private first-party beta to a custom
/// Anthropic-wire gateway — which is the one outcome this gate exists to
/// prevent.
#[test]
fn the_turn_loop_passes_the_config_gate_not_a_constant() {
    const BATCHED: &str = include_str!("../src/turn_loop.rs");
    let call = BATCHED
        .split_once("create_context_hint_controller(")
        .expect("the turn loop must construct the controller")
        .1;
    let args = &call[..call.find(')').unwrap_or(call.len())];
    assert!(
        args.contains("orch.config.include_first_party_betas"),
        "the first-party gate must come from config; got: {args}"
    );
    assert!(
        args.contains("\"repl_main_thread\""),
        "the query source must be the main thread; got: {args}"
    );
}
