//! Anti-drift guard for the clear/resume session-boundary reset sequence.

const HANDLE_IMPL: &str = include_str!("../src/handle_impl.rs");

#[test]
fn session_reset_stages_remain_in_the_pre_split_order() {
    let reset_start = HANDLE_IMPL
        .find("async fn reset_session_scoped_runtime")
        .expect("session reset coordinator exists");
    let reset_tail = &HANDLE_IMPL[reset_start..];
    let reset_end = reset_tail
        .find("\n}\n\n#[async_trait]")
        .expect("session reset coordinator impl ends before OrchestratorHandle");
    let reset_body = &reset_tail[..reset_end];

    let ordered_calls = [
        "reset_context_collapse_and_session_memory",
        "reset_cost_and_api_accounting",
        "reset_token_accounting",
        "reset_refusal_fallback",
        "replace_loaded",
        "reset_read_state",
        "self.transcript.reset_session_scoped().await",
        "orphan_forced_decisions.lock().await.clear()",
        "self.prompt_runtime.reset_session_scoped().await",
    ];

    let positions = ordered_calls.map(|call| {
        reset_body
            .find(call)
            .unwrap_or_else(|| panic!("missing session-reset stage: {call}"))
    });

    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "session reset stages must retain the pre-split execution order: {positions:?}"
    );
}
