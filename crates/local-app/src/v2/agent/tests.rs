use super::*;
fn owner() -> Owner {
    Owner {
        app_id: format!("a{}", "1".repeat(32)),
        release_digest: "2".repeat(64),
        invocation_id: "invocation-1".into(),
    }
}

#[test]
fn structured_output_route_failure_has_distinct_safe_message_from_tool_failure() {
    assert_ne!(
        error_message("model_structured_output_unsupported"),
        error_message("model_tools_unsupported")
    );
    assert!(
        error_message("model_structured_output_unsupported").contains("structured result schema")
    );
}

fn settings() -> Settings {
    Settings {
        model: Model {
            provider_id: "provider".into(),
            model_id: "vision".into(),
        },
        tools: BTreeMap::new(),
        skills: vec![],
        background: false,
        max_turns: 20,
        timeout_ms: 600_000,
    }
}
fn input() -> Input {
    Input {
        request_id: "request-1".into(),
        protocol_thread_id: None,
        protocol_parent_run_id: None,
        protocol_input_digest: None,
        protocol_message_sha256: None,
        prompt: "private photo description".into(),
        attachments: vec![Attachment {
            name: "photo.txt".into(),
            media_type: "text/plain".into(),
            base64: "cHJpdmF0ZQ==".into(),
        }],
        content_parts: Vec::new(),
        output_schema: None,
        tools: vec![],
        skills: vec![],
        max_turns: None,
        timeout_ms: None,
    }
}
fn store() -> (tempfile::TempDir, AgentStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = AgentStore::open(dir.path()).unwrap();
    s.configure(owner(), Some(settings())).unwrap();
    (dir, s)
}

#[test]
fn deduplication_does_not_admit_another_task() {
    let (_dir, s) = store();
    let a = s.admit(&owner(), &input(), &[]).unwrap();
    let b = s.admit(&owner(), &input(), &[]).unwrap();
    assert!(a.start);
    assert!(!b.start);
    assert_eq!(a.task.run_id, b.task.run_id);
    let mut changed = input();
    changed.prompt = "changed".into();
    assert!(s.admit(&owner(), &changed, &[]).is_err());
}

#[test]
fn unstarted_protocol_admission_can_be_rolled_back_for_safe_retry() {
    let (_dir, store) = store();
    let mut input = input();
    input.request_id = "retry-run".into();
    input.protocol_thread_id = Some("thread-retry".into());
    input.protocol_input_digest = Some("a".repeat(64));
    let admitted = store.admit(&owner(), &input, &[]).unwrap();
    assert!(admitted.start);

    store
        .rollback_queued_admission(&owner(), &admitted.task.run_id)
        .unwrap();
    assert!(store.get_by_request(&owner(), "retry-run").is_err());

    let retried = store.admit(&owner(), &input, &[]).unwrap();
    assert!(retried.start);
    assert_ne!(retried.task.run_id, admitted.task.run_id);
}

#[test]
fn consumed_wake_retry_is_idempotent_and_chat_scoped() {
    let (_dir, store) = store();
    let chat_id = uuid::Uuid::new_v4().to_string();
    let first = store
        .admit_scoped(&owner(), &input(), &[], &[], Some(&chat_id), true)
        .unwrap();
    let retry = store
        .admit_scoped(&owner(), &input(), &[], &[], Some(&chat_id), false)
        .unwrap();
    assert!(first.start);
    assert!(!retry.start);
    assert_eq!(retry.task.run_id, first.task.run_id);

    let other_chat = uuid::Uuid::new_v4().to_string();
    let conflict = store.admit_scoped(&owner(), &input(), &[], &[], Some(&other_chat), false);
    assert_eq!(
        admission_error_code(&conflict.err().unwrap()),
        "request_conflict"
    );
}

#[test]
fn previous_settings_without_skills_migrate_to_an_empty_approved_set() {
    let value = serde_json::json!({
        "model": {"providerId":"provider","modelId":"vision"},
        "tools": {},
        "background": false,
        "maxTurns": 20,
        "timeoutMs": 600000
    });
    let settings: Settings = serde_json::from_value(value).unwrap();
    assert!(settings.skills.is_empty());
}

#[test]
fn previous_task_results_load_without_losing_text_or_structured_output() {
    let text: TaskResult = serde_json::from_value(serde_json::json!({"text":"done"})).unwrap();
    assert_eq!(text.text, "done");
    let structured: TaskResult = serde_json::from_value(serde_json::json!({
        "output": {"ok": true},
        "usage": {"inputTokens": 2}
    }))
    .unwrap();
    assert_eq!(structured.text, "");
    assert_eq!(structured.output, Some(serde_json::json!({"ok": true})));
}

#[test]
fn normalized_attachment_input_is_bounded_and_path_free() {
    let mut request = input();
    assert!(validate_input(&request).is_ok());
    request.attachments[0].name = "../outside.txt".into();
    assert!(validate_input(&request).is_err());
    request.attachments[0].name = "valid.txt".into();
    request.attachments[0].base64 = "not base64".into();
    assert!(validate_input(&request).is_err());
}
#[test]
fn ledger_never_persists_inputs() {
    let (dir, s) = store();
    s.admit(&owner(), &input(), &[]).unwrap();
    let disk = fs::read_to_string(dir.path().join("agent-tasks.json")).unwrap();
    assert!(!disk.contains("private photo"));
    assert!(!disk.contains("cHJpdmF0ZQ=="));
}
#[test]
fn restart_interrupts_without_replay() {
    let (dir, s) = store();
    let a = s.admit(&owner(), &input(), &[]).unwrap();
    drop(s);
    let s = AgentStore::open(dir.path()).unwrap();
    let b = s.admit(&owner(), &input(), &[]).unwrap();
    assert!(!b.start);
    assert_eq!(b.task.state, State::Interrupted);
    assert_eq!(b.task.run_id, a.task.run_id);
}
#[test]
fn reads_are_app_release_scoped_while_execution_leases_are_invocation_scoped() {
    let (_dir, s) = store();
    let a = s.admit(&owner(), &input(), &[]).unwrap();
    for o in [
        Owner {
            app_id: format!("a{}", "3".repeat(32)),
            ..owner()
        },
        Owner {
            release_digest: "4".repeat(64),
            ..owner()
        },
        Owner {
            invocation_id: "another".into(),
            ..owner()
        },
    ] {
        if o.app_id == owner().app_id && o.release_digest == owner().release_digest {
            assert_eq!(s.get(&o, &a.task.run_id).unwrap().run_id, a.task.run_id);
            assert_eq!(s.list(&o, None, 10).unwrap().len(), 1);
        } else {
            assert!(s.get(&o, &a.task.run_id).is_err());
            assert!(s.list(&o, None, 10).unwrap().is_empty());
        }
        assert!(!s.lease_valid(&o, &a.task.run_id));
    }
}
#[test]
fn revocation_and_regrant_cannot_revive_an_old_lease() {
    let (_dir, s) = store();
    let a = s.admit(&owner(), &input(), &[]).unwrap();
    assert!(s.lease_valid(&owner(), &a.task.run_id));
    assert_eq!(
        s.configure(owner(), None).unwrap(),
        vec![a.task.run_id.clone()]
    );
    assert!(!s.lease_valid(&owner(), &a.task.run_id));
    s.configure(owner(), Some(settings())).unwrap();
    assert!(!s.lease_valid(&owner(), &a.task.run_id));
}
#[test]
fn concurrency_is_held_until_executor_finishes() {
    let (_dir, s) = store();
    let a = s.admit(&owner(), &input(), &[]).unwrap();
    s.configure(owner(), None).unwrap();
    s.configure(owner(), Some(settings())).unwrap();
    let mut next = input();
    next.request_id = "next".into();
    assert!(matches!(
        s.admit(&owner(), &next, &[]),
        Err(AppError::Busy(_))
    ));
    s.update(&owner(), &a.task.run_id, State::Cancelled, None, None, None)
        .unwrap();
    assert!(s.admit(&owner(), &next, &[]).unwrap().start);
}
#[test]
fn exact_mcp_grants_and_manifest_are_both_required() {
    let (_dir, s) = store();
    let mut grant = settings();
    let tool = ToolGrant::Mcp {
        server_config_id: "stable-host-id".into(),
        config_revision: "rev".into(),
        raw_tool_name: "raw.tool".into(),
        definition_digest: "5".repeat(64),
    };
    grant.tools.insert("opaque-id".into(), tool.clone());
    s.configure(owner(), Some(grant)).unwrap();
    let mut i = input();
    i.tools = vec!["opaque-id".into()];
    assert!(s.admit(&owner(), &i, &[]).is_err());
    let a = s.admit(&owner(), &i, &["opaque-id".into()]).unwrap();
    assert_eq!(a.tools["opaque-id"], tool);
}
#[test]
fn limits_cannot_expand_host_settings() {
    let (_dir, s) = store();
    for (turns, time) in [
        (Some(0), None),
        (Some(21), None),
        (None, Some(600_001)),
        (None, Some(0)),
    ] {
        let mut i = input();
        i.max_turns = turns;
        i.timeout_ms = time;
        assert!(s.admit(&owner(), &i, &[]).is_err());
    }
    let mut i = input();
    i.max_turns = Some(2);
    i.timeout_ms = Some(10_000);
    assert_eq!(s.admit(&owner(), &i, &[]).unwrap().task.limits.max_turns, 2);
}
#[test]
fn terminal_result_is_immutable() {
    let (_dir, s) = store();
    let a = s.admit(&owner(), &input(), &[]).unwrap();
    let result = TaskResult {
        text: String::new(),
        output: Some(serde_json::json!({"food":"apple"})),
        attachments: vec![],
        chat_id: None,
        message_id: None,
    };
    let t = s
        .update(
            &owner(),
            &a.task.run_id,
            State::Succeeded,
            Some(result.clone()),
            None,
            None,
        )
        .unwrap();
    let cancelled = s
        .update(&owner(), &a.task.run_id, State::Cancelled, None, None, None)
        .unwrap();
    assert_eq!(t.sequence, cancelled.sequence);
    assert_eq!(cancelled.result, Some(result));
}
#[test]
fn release_changes_require_fresh_authorization() {
    let (_dir, s) = store();
    let other = Owner {
        release_digest: "a".repeat(64),
        ..owner()
    };
    assert!(s.settings(&other).unwrap().is_none());
    assert!(s.admit(&other, &input(), &[]).is_err());
}
#[test]
fn malformed_storage_is_preserved_and_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("agent-tasks.json");
    fs::write(&p, "{broken").unwrap();
    assert!(AgentStore::open(dir.path()).is_err());
    assert_eq!(fs::read_to_string(p).unwrap(), "{broken");
}
#[test]
fn page_cannot_supply_host_fields() {
    let mut value = serde_json::to_value(input()).unwrap();
    value["appId"] = serde_json::json!(owner().app_id);
    assert!(serde_json::from_value::<Input>(value).is_err());
    let mut o = owner();
    o.app_id = "../escape".into();
    assert!(o.validate().is_err());
}

#[test]
fn terminal_result_survives_a_new_page_invocation_without_reexecution() {
    let (_dir, store) = store();
    let admitted = store.admit(&owner(), &input(), &[]).unwrap();
    store
        .update(
            &owner(),
            &admitted.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "done".into(),
                output: None,
                attachments: vec![],
                chat_id: None,
                message_id: None,
            }),
            None,
            None,
        )
        .unwrap();
    let next = Owner {
        invocation_id: "new-page".into(),
        ..owner()
    };
    assert_eq!(
        store.get(&next, &admitted.task.run_id).unwrap().state,
        State::Succeeded
    );
    let repeated = store.admit(&next, &input(), &[]).unwrap();
    assert!(!repeated.start);
    assert_eq!(repeated.task.run_id, admitted.task.run_id);
}

#[test]
fn revocation_wins_a_racing_success_commit() {
    let (_dir, store) = store();
    let admitted = store.admit(&owner(), &input(), &[]).unwrap();
    store.configure(owner(), None).unwrap();
    let result = store
        .update(
            &owner(),
            &admitted.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "late".into(),
                output: None,
                attachments: vec![],
                chat_id: None,
                message_id: None,
            }),
            None,
            None,
        )
        .unwrap();
    assert_eq!(result.state, State::Cancelled);
    assert!(result.result.is_none());
    assert_eq!(result.error.unwrap().code, "authorization_revoked");
}

#[test]
fn cancellation_intent_blocks_late_success_without_releasing_concurrency() {
    let (_dir, store) = store();
    let admitted = store.admit(&owner(), &input(), &[]).unwrap();
    store
        .request_cancel(&owner(), &admitted.task.run_id)
        .unwrap();
    assert!(!store.lease_valid(&owner(), &admitted.task.run_id));
    let mut next = input();
    next.request_id = "next".into();
    assert!(matches!(
        store.admit(&owner(), &next, &[]),
        Err(AppError::Busy(_))
    ));
    let task = store
        .update(
            &owner(),
            &admitted.task.run_id,
            State::Succeeded,
            Some(TaskResult {
                text: "late".into(),
                output: None,
                attachments: vec![],
                chat_id: None,
                message_id: None,
            }),
            None,
            None,
        )
        .unwrap();
    assert_eq!(task.state, State::Cancelled);
    assert!(task.result.is_none());
    assert!(store.admit(&owner(), &next, &[]).unwrap().start);
}

#[test]
fn admission_failures_expose_stable_authorization_codes() {
    let (_dir, store) = store();
    let mut request = input();
    request.tools.push("builtin:Read".into());
    assert_eq!(
        admission_error_code(
            &store
                .admit(&owner(), &request, &["builtin:Read".into()])
                .err()
                .unwrap()
        ),
        "tool_not_authorized"
    );
    store.configure(owner(), None).unwrap();
    assert_eq!(
        admission_error_code(&store.admit(&owner(), &input(), &[]).err().unwrap()),
        "agent_not_authorized"
    );
}
#[test]
fn busy_and_request_conflicts_have_distinct_codes() {
    let (_dir, store) = store();
    store.admit(&owner(), &input(), &[]).unwrap();
    let mut changed = input();
    changed.prompt = "different".into();
    assert_eq!(
        admission_error_code(&store.admit(&owner(), &changed, &[]).err().unwrap()),
        "request_conflict"
    );
    changed.request_id = "another".into();
    assert_eq!(
        admission_error_code(&store.admit(&owner(), &changed, &[]).err().unwrap()),
        "busy"
    );
}
#[test]
fn unsupported_image_model_result_explains_the_remedy() {
    let (_dir, store) = store();
    let task = store.admit(&owner(), &input(), &[]).unwrap().task;
    let task = store
        .update(
            &owner(),
            &task.run_id,
            State::Failed,
            None,
            None,
            Some("model_vision_unsupported".into()),
        )
        .unwrap();
    let error = task.error.unwrap();
    assert_eq!(error.code, "model_vision_unsupported");
    assert!(error.message.contains("compatible model"));
}

#[test]
fn object_key_order_does_not_create_a_request_conflict() {
    let (_dir, store) = store();
    let mut first = input();
    first.output_schema = Some(serde_json::from_str(r#"{"type":"object","properties":{"food":{"type":"string"},"grams":{"minimum":0,"type":"number"}}}"#).unwrap());
    let accepted = store.admit(&owner(), &first, &[]).unwrap();
    let mut retry = input();
    retry.output_schema = Some(serde_json::from_str(r#"{"properties":{"grams":{"type":"number","minimum":0},"food":{"type":"string"}},"type":"object"}"#).unwrap());
    let repeated = store.admit(&owner(), &retry, &[]).unwrap();
    assert!(!repeated.start);
    assert_eq!(repeated.task.run_id, accepted.task.run_id);
}

#[test]
fn native_interaction_phases_are_durable_sequenced_and_cleared_on_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let store = AgentStore::open(dir.path()).unwrap();
    store.configure(owner(), Some(settings())).unwrap();
    let run = store.admit(&owner(), &input(), &[]).unwrap().task;
    store
        .set_phase(&owner(), &run.run_id, Some("waiting_for_answer"))
        .unwrap();
    let pending = store.get(&owner(), &run.run_id).unwrap();
    assert_eq!(pending.phase.as_deref(), Some("waiting_for_answer"));
    assert!(pending.sequence > run.sequence);
    store
        .set_phase(&owner(), &run.run_id, Some("waiting_for_answer"))
        .unwrap();
    assert_eq!(
        store.get(&owner(), &run.run_id).unwrap().sequence,
        pending.sequence
    );
    assert!(
        store
            .set_phase(&owner(), &run.run_id, Some("approve_any_tool"))
            .is_err()
    );
    let finished = store
        .update(&owner(), &run.run_id, State::Cancelled, None, None, None)
        .unwrap();
    assert!(finished.phase.is_none());
    store
        .set_phase(&owner(), &run.run_id, Some("waiting_for_permission"))
        .unwrap();
    assert!(store.get(&owner(), &run.run_id).unwrap().phase.is_none());
}

#[test]
fn protocol_events_are_schema_valid_durable_ordered_and_owner_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = AgentStore::open(dir.path()).unwrap();
    store.configure(owner(), Some(settings())).unwrap();
    let mut run_input = input();
    run_input.protocol_thread_id = Some("thread-1".into());
    let task = store.admit(&owner(), &run_input, &[]).unwrap().task;

    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"RUN_STARTED","protocolVersion":"1.0","threadId":"thread-1","runId":"request-1"}),
        )
        .unwrap();
    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"TEXT_MESSAGE_START","messageId":"message-1"}),
        )
        .unwrap();
    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"TEXT_MESSAGE_CONTENT","messageId":"message-1","delta":"hello"}),
        )
        .unwrap();
    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"TEXT_MESSAGE_END","messageId":"message-1"}),
        )
        .unwrap();
    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"RUN_FINISHED","threadId":"thread-1","runId":"request-1","result":{"food":"soup"},"outcome":{"type":"success"}}),
        )
        .unwrap();

    let first = store.protocol_events(&owner(), "request-1", 0).unwrap();
    assert_eq!(first.events.len(), 5);
    assert_eq!(first.sequence, 5);
    assert_eq!(
        store.protocol_events(&owner(), "request-1", 3).unwrap(),
        ProtocolEventPage {
            events: first.events[3..].to_vec(),
            sequence: 5,
        }
    );
    assert!(store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"RUN_FINISHED","threadId":"thread-1","runId":"request-1"}),
        )
        .is_err());
    assert!(
        store
            .protocol_events(
                &Owner {
                    app_id: format!("a{}", "3".repeat(32)),
                    ..owner()
                },
                "request-1",
                0,
            )
            .is_err()
    );
}

#[test]
fn host_restart_ends_protocol_stream_as_error_not_success() {
    let dir = tempfile::tempdir().unwrap();
    let store = AgentStore::open(dir.path()).unwrap();
    store.configure(owner(), Some(settings())).unwrap();
    let mut run_input = input();
    run_input.protocol_thread_id = Some("thread-restart".into());
    let task = store.admit(&owner(), &run_input, &[]).unwrap().task;
    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({"type":"RUN_STARTED","threadId":"thread-restart","runId":"request-1"}),
        )
        .unwrap();
    drop(store);

    let recovered = AgentStore::open(dir.path()).unwrap();
    let page = recovered.protocol_events(&owner(), "request-1", 0).unwrap();
    assert_eq!(page.events.last().unwrap()["type"], "RUN_ERROR");
    assert_eq!(page.events.last().unwrap()["code"], "host_restarted");
    assert_eq!(
        recovered
            .get_by_request(&owner(), "request-1")
            .unwrap()
            .state,
        State::Interrupted
    );
}
