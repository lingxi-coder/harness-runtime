use telemetry::tengu::agent;

#[test]
fn all_30_agent_event_names_are_locked() {
    let names: &[&str] = &[
        agent::STARTED,
        agent::COMPLETED,
        agent::FAILED,
        agent::CANCELLED,
        agent::TURN_STARTED,
        agent::TURN_COMPLETED,
        agent::TURN_FAILED,
        agent::SUBAGENT_DISPATCHED,
        agent::SUBAGENT_COMPLETED,
        agent::SUBAGENT_FAILED,
        agent::MEMORY_LOADED,
        agent::SYSTEM_PROMPT_BUILT,
        agent::PERSONA_RESOLVED,
        agent::COMPACTION_TRIGGERED,
        agent::COMPACTION_COMPLETED,
        agent::COMPACTION_FAILED,
        agent::RESUME_STARTED,
        agent::RESUME_COMPLETED,
        agent::RESUME_FAILED,
        agent::FORK_STARTED,
        agent::FORK_COMPLETED,
        agent::FORK_FAILED,
        agent::STATE_PERSISTED,
        agent::STATE_LOADED,
        agent::STATE_LOAD_FAILED,
        agent::IDLE_TIMEOUT,
        agent::KILLSWITCH_ACTIVATED,
        agent::LOOP_ITERATION,
        agent::MESSAGE_ADDED,
        agent::MESSAGE_TRUNCATED,
    ];
    assert_eq!(
        names.len(),
        30,
        "agent category must declare exactly 30 events"
    );
    for n in names {
        assert!(
            n.starts_with("tengu_agent_"),
            "{n} must start with tengu_agent_"
        );
    }
    // Locked byte-for-byte against M3-02 plan reference.
    assert_eq!(agent::MEMORY_LOADED, "tengu_agent_memory_loaded");
}

#[test]
fn agent_started_payload_round_trips() {
    use telemetry::Verified;
    let p = agent::StartedPayload {
        agent_id: Verified::assert_safe("agent-001".into()),
        agent_kind: agent::AgentKind::Main,
        parent_agent_id: None,
        session_id: Verified::assert_safe("sess-uuid".into()),
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let _: agent::StartedPayload = serde_json::from_str(&json).expect("round-trip");
}

#[test]
fn agent_loop_iteration_carries_extra_value() {
    use telemetry::Verified;
    let p = agent::LoopIterationPayload {
        agent_id: Verified::assert_safe("agent-001".into()),
        iteration: 7,
        extra: serde_json::json!({"todo_count": 3}),
    };
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.contains("\"todo_count\":3"));
}
