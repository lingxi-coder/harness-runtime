use super::*;
use lingxi_core::types::MessageId;
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
        protocol_history_digest: None,
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
fn successful_chat_history_lookup_survives_release_change_without_widening_task_reads() {
    let (_directory, store) = store();
    let current_owner = owner();
    let chat_id = uuid::Uuid::new_v4().to_string();
    let input_value = json!({
        "threadId":chat_id,
        "runId":"chat-history-cross-release",
        "protocolVersion":"1.0",
        "messages":[{"id":"cross-release-user","role":"user","content":"hello"}]
    });
    let normalized = protocol::normalize_run_input(&input_value, &[]).unwrap();
    let admitted = store
        .admit_scoped(
            &current_owner,
            &normalized.input,
            &[],
            &[],
            Some(&chat_id),
            true,
        )
        .unwrap();
    store
        .update(
            &current_owner,
            &admitted.task.run_id,
            State::Running,
            None,
            None,
            None,
        )
        .unwrap();
    store
        .append_event(
            &current_owner,
            &admitted.task.run_id,
            json!({
                "type":"RUN_STARTED",
                "protocolVersion":"1.0",
                "threadId":chat_id,
                "runId":"chat-history-cross-release"
            }),
        )
        .unwrap();
    store
        .record_chat_transcript_checkpoint(
            &current_owner,
            &admitted.task.run_id,
            &chat_id,
            ChatTranscriptCheckpoint {
                byte_len: 14,
                sha256: "a".repeat(64),
            },
        )
        .unwrap();
    let assistant_id = MessageId::new().to_string();
    commit_protocol_terminal_for_test(
        &store,
        &current_owner,
        &admitted.task.run_id,
        State::Succeeded,
        Some(TaskResult {
            text: "hello back".into(),
            output: None,
            attachments: Vec::new(),
            chat_id: Some(chat_id.clone()),
            message_id: Some(assistant_id),
        }),
        None,
    )
    .unwrap();

    let next_release_owner = Owner {
        release_digest: "3".repeat(64),
        invocation_id: "next-release".into(),
        ..current_owner.clone()
    };
    assert!(store
        .get(&next_release_owner, &admitted.task.run_id)
        .is_err());
    let (history_task, checkpoint) = store
        .successful_chat_history_task(&current_owner.app_id, &chat_id, &admitted.task.run_id)
        .unwrap();
    assert_eq!(history_task.state, State::Succeeded);
    assert_eq!(checkpoint.byte_len, 14);
    assert_eq!(checkpoint.sha256, "a".repeat(64));
    assert_eq!(
        history_task.result.as_ref().unwrap().chat_id.as_deref(),
        Some(chat_id.as_str())
    );
    assert!(store
        .successful_chat_history_task(
            &current_owner.app_id,
            &uuid::Uuid::new_v4().to_string(),
            &admitted.task.run_id,
        )
        .is_err());
    assert!(store
        .successful_chat_history_task(
            "a00000000000000000000000000000000",
            &chat_id,
            &admitted.task.run_id,
        )
        .is_err());
}

#[test]
fn latest_chat_checkpoint_uses_longest_capture_across_release_not_task_sequence() {
    let (_directory, store) = store();
    let first_owner = owner();
    let chat_id = uuid::Uuid::new_v4().to_string();

    let finish_turn = |owner: &Owner,
                       request_id: &str,
                       checkpoint: ChatTranscriptCheckpoint,
                       advance_phase: bool| {
        let raw = json!({
            "threadId":chat_id,
            "runId":request_id,
            "protocolVersion":"1.0",
            "messages":[{"id":format!("user-{request_id}"),"role":"user","content":"question"}]
        });
        let normalized = protocol::normalize_run_input(&raw, &[]).unwrap();
        let admitted = store
            .admit_scoped(owner, &normalized.input, &[], &[], Some(&chat_id), true)
            .unwrap();
        let task_id = admitted.task.run_id;
        store
            .update(owner, &task_id, State::Running, None, None, None)
            .unwrap();
        store
            .append_event(
                owner,
                &task_id,
                json!({
                    "type":"RUN_STARTED",
                    "protocolVersion":"1.0",
                    "threadId":chat_id,
                    "runId":request_id
                }),
            )
            .unwrap();
        if advance_phase {
            store
                .set_phase(owner, &task_id, Some("waiting_for_answer"))
                .unwrap();
            store.set_phase(owner, &task_id, None).unwrap();
        }
        store
            .record_chat_transcript_checkpoint(owner, &task_id, &chat_id, checkpoint)
            .unwrap();
        let task = commit_protocol_terminal_for_test(
            &store,
            owner,
            &task_id,
            State::Succeeded,
            Some(TaskResult {
                text: format!("answer-{request_id}"),
                output: None,
                attachments: Vec::new(),
                chat_id: Some(chat_id.clone()),
                message_id: Some(MessageId::new().to_string()),
            }),
            None,
        )
        .unwrap();
        (task_id, task)
    };

    let (first_id, first_task) = finish_turn(
        &first_owner,
        "older-complex-turn",
        ChatTranscriptCheckpoint {
            byte_len: 1_000,
            sha256: "a".repeat(64),
        },
        true,
    );
    let next_owner = Owner {
        release_digest: "3".repeat(64),
        invocation_id: "next-release".into(),
        ..first_owner.clone()
    };
    store
        .configure(next_owner.clone(), Some(settings()))
        .unwrap();
    let (second_id, second_task) = finish_turn(
        &next_owner,
        "newer-simple-turn",
        ChatTranscriptCheckpoint {
            byte_len: 2_000,
            sha256: "b".repeat(64),
        },
        false,
    );

    assert!(
        first_task.sequence > second_task.sequence,
        "the older run fixture must have more per-task phase transitions"
    );
    assert!(store.get(&next_owner, &first_id).is_err());
    assert_eq!(
        store
            .successful_chat_history_task(&first_owner.app_id, &chat_id, &first_id)
            .unwrap()
            .0
            .state,
        State::Succeeded,
        "same-App Chat recovery remains scoped across the release update"
    );

    // This is the checkpoint a third turn must open and verify against the
    // exact Chat file. A smaller, older capture cannot win because its task
    // happened to accumulate more state transitions.
    let (_, third_turn_checkpoint) = store
        .latest_chat_transcript_checkpoint(&next_owner.app_id, &chat_id, "third-turn-id")
        .unwrap()
        .unwrap();
    assert_eq!(third_turn_checkpoint.byte_len, 2_000);
    assert_eq!(third_turn_checkpoint.sha256, "b".repeat(64));

    // Simulate conflicting durable evidence that could survive an older
    // implementation: same length, different digest must not select either.
    let first_conflicting_id = first_id.clone();
    let second_conflicting_id = second_id.clone();
    store
        .change(|ledger| {
            ledger
                .runs
                .get_mut(&first_conflicting_id)
                .expect("first successful Chat task exists")
                .chat_transcript_checkpoint = Some(ChatTranscriptCheckpoint {
                byte_len: 2_000,
                sha256: "a".repeat(64),
            });
            ledger
                .runs
                .get_mut(&second_conflicting_id)
                .expect("second successful Chat task exists")
                .chat_transcript_checkpoint = Some(ChatTranscriptCheckpoint {
                byte_len: 2_000,
                sha256: "c".repeat(64),
            });
            Ok(())
        })
        .unwrap();
    assert!(store
        .latest_chat_transcript_checkpoint(&next_owner.app_id, &chat_id, "third-turn-id")
        .is_err());
}

#[test]
fn duplicate_admission_skips_stale_history_check_after_first_commit() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    };

    let (_directory, store) = store();
    let store = Arc::new(store);
    let chat_store = Arc::new(ChatStore::open(&_directory.path().join("chat-index")).unwrap());
    let chat = chat_store
        .create(
            &uuid::Uuid::new_v4().to_string(),
            None,
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
    let mut request = input();
    request.request_id = "shared-history-race".into();
    request.protocol_thread_id = Some(chat.chat_id.clone());
    request.protocol_history_digest = Some(protocol::history_fingerprint(&[]));

    let barrier = Arc::new(Barrier::new(3));
    let checks = Arc::new(AtomicUsize::new(0));
    let chat_id = chat.chat_id.clone();
    let spawn = |store: Arc<AgentStore>,
                 chat_store: Arc<ChatStore>,
                 barrier: Arc<Barrier>,
                 checks: Arc<AtomicUsize>,
                 request: Input| {
        let chat_id = chat_id.clone();
        let persisted_chat_id = chat_id.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store.admit_scoped_with_check(
                &owner(),
                &request,
                &[],
                &[],
                Some(&chat_id),
                true,
                move || {
                    checks.fetch_add(1, Ordering::SeqCst);
                    chat_store.record_protocol_message(
                        &persisted_chat_id,
                        "accepted-run",
                        &serde_json::json!({
                            "id":"accepted-user","role":"user","content":"question"
                        }),
                    )
                },
            )
        })
    };
    let first = spawn(
        store.clone(),
        chat_store.clone(),
        barrier.clone(),
        checks.clone(),
        request.clone(),
    );
    let second = spawn(
        store.clone(),
        chat_store.clone(),
        barrier.clone(),
        checks.clone(),
        request,
    );
    barrier.wait();
    let first = first.join().unwrap().unwrap();
    let second = second.join().unwrap().unwrap();

    assert_ne!(first.start, second.start);
    assert_eq!(first.task.run_id, second.task.run_id);
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(
        chat_store
            .protocol_history_page(&chat.chat_id, None, 10)
            .unwrap()
            .messages
            .len(),
        1,
        "the first admission must finish its Chat write before the duplicate resolves"
    );
}

#[test]
fn failed_cancelled_and_interrupted_protocol_turns_block_chat_followups() {
    let cases = [
        ("failed", State::Failed, "model_error"),
        ("cancelled", State::Cancelled, "cancelled"),
        ("interrupted", State::Interrupted, "ios_background_expired"),
    ];

    for (suffix, terminal_state, error_code) in cases {
        let (_directory, store) = store();
        let chat_id = format!("thread-{suffix}");
        let mut request = input();
        request.request_id = format!("run-{suffix}");
        request.protocol_thread_id = Some(chat_id.clone());
        let admission = store
            .admit_scoped(&owner(), &request, &[], &[], Some(&chat_id), true)
            .unwrap();
        store
            .append_event(
                &owner(),
                &admission.task.run_id,
                serde_json::json!({
                    "type":"RUN_STARTED", "protocolVersion":"1.0",
                    "threadId":chat_id,"runId":request.request_id
                }),
            )
            .unwrap();
        let terminal = commit_protocol_terminal_for_test(
            &store,
            &owner(),
            &admission.task.run_id,
            terminal_state.clone(),
            None,
            Some(error_code.into()),
        )
        .unwrap();
        assert_eq!(terminal.state, terminal_state);

        let mut followup = input();
        followup.request_id = format!("next-{suffix}");
        followup.protocol_thread_id = Some(chat_id.clone());
        let error = match store.admit_scoped(&owner(), &followup, &[], &[], Some(&chat_id), true) {
            Err(error) => error,
            Ok(_) => panic!("an unverified Chat turn must block follow-up admission"),
        };
        assert!(matches!(
            error,
            AppError::Invalid(message) if message == "Chat has an incomplete or unverified turn"
        ));
        assert!(matches!(
            store.get_by_request(&owner(), &followup.request_id),
            Err(AppError::NotFound(_))
        ));
    }
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
fn omitted_native_skill_selection_does_not_grant_skills() {
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
fn current_task_results_preserve_optional_output_and_reject_obsolete_fields() {
    let text: TaskResult = serde_json::from_value(serde_json::json!({"text":"done"})).unwrap();
    assert_eq!(text.text, "done");
    let structured: TaskResult = serde_json::from_value(serde_json::json!({
        "output": {"ok": true}
    }))
    .unwrap();
    assert_eq!(structured.text, "");
    assert_eq!(structured.output, Some(serde_json::json!({"ok": true})));
    assert!(serde_json::from_value::<TaskResult>(serde_json::json!({
        "output": {"ok": true},
        "usage": {"inputTokens": 2}
    }))
    .is_err());
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
fn incomplete_current_ledger_is_rejected_without_inventing_a_recovery_frontier() {
    let (dir, store) = store();
    drop(store);
    let ledger = dir.path().join("agent-tasks.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
    assert!(value
        .as_object_mut()
        .unwrap()
        .remove("chatRecoveries")
        .is_some());
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(&ledger, &bytes).unwrap();
    assert!(matches!(
        AgentStore::open(dir.path()),
        Err(AppError::Json(_))
    ));
    assert_eq!(fs::read(&ledger).unwrap(), bytes);
    assert!(fs::read_dir(dir.path().join("protocol-events"))
        .unwrap()
        .next()
        .is_none());
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
fn task_list_pages_keep_public_request_cursors_and_app_release_scope() {
    let (_dir, store) = store();
    let owner = owner();
    let mut expected = Vec::new();
    for index in 0..5 {
        let mut request = input();
        request.request_id = format!("page-request-{index}");
        let admission = store.admit(&owner, &request, &[]).unwrap();
        expected.push(admission.task.clone());
        store
            .update(
                &owner,
                &admission.task.run_id,
                State::Succeeded,
                None,
                None,
                None,
            )
            .unwrap();
    }

    for other_owner in [
        Owner {
            app_id: format!("a{}", "3".repeat(32)),
            ..owner.clone()
        },
        Owner {
            release_digest: "4".repeat(64),
            ..owner.clone()
        },
    ] {
        store
            .configure(other_owner.clone(), Some(settings()))
            .unwrap();
        let mut request = input();
        request.request_id = format!("other-{}", other_owner.app_id);
        store.admit(&other_owner, &request, &[]).unwrap();
    }

    let all = store.list(&owner, None, 100).unwrap();
    assert_eq!(all.len(), expected.len());
    let first = store.list_page(&owner, None, 2).unwrap();
    assert_eq!(
        first
            .tasks
            .iter()
            .map(|task| task.run_id.as_str())
            .collect::<Vec<_>>(),
        all[..2]
            .iter()
            .map(|task| task.run_id.as_str())
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        first.next_cursor.as_deref(),
        Some(all[1].request_id.as_str())
    );

    let second = store
        .list_page(&owner, first.next_cursor.as_deref(), 2)
        .unwrap();
    assert_eq!(
        second
            .tasks
            .iter()
            .map(|task| task.run_id.as_str())
            .collect::<Vec<_>>(),
        all[2..4]
            .iter()
            .map(|task| task.run_id.as_str())
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        second.next_cursor.as_deref(),
        Some(all[3].request_id.as_str())
    );

    let last = store
        .list_page(&owner, second.next_cursor.as_deref(), 2)
        .unwrap();
    assert_eq!(
        last.tasks
            .iter()
            .map(|task| task.run_id.as_str())
            .collect::<Vec<_>>(),
        all[4..]
            .iter()
            .map(|task| task.run_id.as_str())
            .collect::<Vec<_>>(),
    );
    assert!(last.next_cursor.is_none());

    let mut paged = first.tasks;
    paged.extend(second.tasks);
    paged.extend(last.tasks);
    assert_eq!(paged.len(), all.len());
    assert_eq!(
        paged.iter().map(|task| &task.run_id).collect::<Vec<_>>(),
        all.iter().map(|task| &task.run_id).collect::<Vec<_>>()
    );
    assert!(store.list_page(&owner, Some("unknown-request"), 2).is_err());
    assert!(store.list_page(&owner, None, 0).is_err());
    assert!(store.list_page(&owner, None, 101).is_err());
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
fn current_chat_and_skill_failures_keep_actionable_messages_in_task_and_agui_replay() {
    let cases = [
        ("chat_context_unavailable", "Reopen the Chat"),
        (
            "chat_context_compaction_unsupported",
            "Reduce the new input",
        ),
        ("chat_context_limit_exceeded", "smaller request"),
        ("chat_context_budget_unknown", "model configuration"),
        ("chat_context_authority_revoked", "tool effects"),
        ("chat_tool_unsupported", "another tool"),
        ("chat_transcript_external_result_unmanaged", "saved history"),
        ("local_app_chat_inline_result_too_large", "smaller result"),
        ("skill_scope_limit_exceeded", "fewer or smaller Skills"),
    ];
    for (index, (code, remedy)) in cases.into_iter().enumerate() {
        let (dir, store) = store();
        let chat_id = uuid::Uuid::new_v4().to_string();
        let run_id = format!("failure-{index}");
        let normalized = protocol::normalize_run_input(
            &json!({
                "threadId":chat_id, "runId":run_id,
                "messages":[{"id":format!("user-{index}"),"role":"user",
                    "content":"PRIVATE_ERROR_CANARY /private/host in-memory-test-secret"}]
            }),
            &[],
        )
        .unwrap();
        let admitted = store
            .admit_scoped(&owner(), &normalized.input, &[], &[], Some(&chat_id), true)
            .unwrap();
        store
            .update(
                &owner(),
                &admitted.task.run_id,
                State::Running,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .append_event(
                &owner(),
                &admitted.task.run_id,
                json!({"type":"RUN_STARTED","threadId":chat_id,"runId":run_id}),
            )
            .unwrap();
        let task = commit_protocol_terminal_for_test(
            &store,
            &owner(),
            &admitted.task.run_id,
            State::Failed,
            None,
            Some(code.into()),
        )
        .unwrap();
        assert_eq!(task.state, State::Failed);
        assert!(task.result.is_none());
        let error = task.error.unwrap();
        assert_eq!(error.code, code);
        assert!(error.message.contains(remedy), "{code}: {}", error.message);
        assert_ne!(error.message, error_message("unknown-task-failure"));
        for detail in [
            "PRIVATE_ERROR_CANARY",
            "/private/",
            "in-memory-test-secret",
            "rolled back",
        ] {
            assert!(!error.message.contains(detail), "{code}: {detail}");
        }
        let events = store.protocol_events(&owner(), &run_id, 0).unwrap();
        assert_eq!(events.events.len(), 2);
        assert_eq!(events.events[1]["type"], "RUN_ERROR");
        assert_eq!(events.events[1]["code"], code);
        assert_eq!(events.events[1]["message"], error.message);
        drop(store);
        let reopened = AgentStore::open(dir.path()).unwrap();
        let restored = reopened.get_by_request(&owner(), &run_id).unwrap();
        assert_eq!(restored.state, State::Failed);
        assert!(restored.result.is_none());
        assert_eq!(restored.error.unwrap().message, error.message);
        assert_eq!(
            reopened
                .protocol_events(&owner(), &run_id, 0)
                .unwrap()
                .events,
            events.events
        );
    }
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
    assert!(store
        .set_phase(&owner(), &run.run_id, Some("approve_any_tool"))
        .is_err());
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
    assert!(store
        .protocol_events(
            &Owner {
                app_id: format!("a{}", "3".repeat(32)),
                ..owner()
            },
            "request-1",
            0,
        )
        .is_err());
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

#[test]
fn native_output_is_private_bounded_owner_scoped_and_durable() {
    let (dir, store) = store();
    let mut run_input = input();
    run_input.protocol_thread_id = Some("thread-native".into());
    let task = store.admit(&owner(), &run_input, &[]).unwrap().task;
    store
        .update(&owner(), &task.run_id, State::Running, None, None, None)
        .unwrap();
    store
        .append_event(
            &owner(),
            &task.run_id,
            serde_json::json!({
                "type":"RUN_STARTED","threadId":"thread-native","runId":"request-1"
            }),
        )
        .unwrap();
    let task_before = store.get(&owner(), &task.run_id).unwrap();
    let page_before = store.protocol_events(&owner(), "request-1", 0).unwrap();
    assert_eq!(
        store
            .native_output_bytes_for_request(&owner(), "request-1", 7)
            .unwrap(),
        None
    );
    assert!(store
        .record_native_output_bytes(&owner(), &task.run_id, 7, 0)
        .is_err());
    assert!(store
        .record_native_output_bytes(&owner(), &task.run_id, 7, MAX_NATIVE_OUTPUT_BYTES + 1)
        .is_err());
    store
        .record_native_output_bytes(&owner(), &task.run_id, 7, 17)
        .unwrap();
    assert_eq!(
        store
            .native_output_bytes_for_request(&owner(), "request-1", 7)
            .unwrap(),
        Some(17)
    );
    assert_eq!(
        store
            .native_output_bytes_for_request(&owner(), "request-1", 8)
            .unwrap(),
        None
    );
    let task_after = store.get(&owner(), &task.run_id).unwrap();
    let page_after = store.protocol_events(&owner(), "request-1", 0).unwrap();
    assert_eq!(task_before.sequence, task_after.sequence);
    assert_eq!(
        serde_json::to_value(task_before).unwrap(),
        serde_json::to_value(task_after).unwrap()
    );
    assert_eq!(page_before, page_after);
    assert!(serde_json::to_value(&page_after)
        .unwrap()
        .get("nativeOutputBytes")
        .is_none());
    let other_release = Owner {
        release_digest: "3".repeat(64),
        ..owner()
    };
    assert!(store
        .native_output_bytes_for_request(&other_release, "request-1", 7)
        .is_err());
    assert!(store
        .record_native_output_bytes(&other_release, &task.run_id, 7, 3)
        .is_err());
    commit_protocol_terminal_for_test(
        &store,
        &owner(),
        &task.run_id,
        State::Cancelled,
        None,
        Some("cancelled".into()),
    )
    .unwrap();
    assert!(store
        .record_native_output_bytes(&owner(), &task.run_id, 7, 18)
        .is_err());
    let reopened = AgentStore::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .native_output_bytes_for_request(&owner(), "request-1", 7)
            .unwrap(),
        Some(17)
    );
    {
        let mut ledger = reopened.ledger.lock().unwrap();
        ledger.runs.get_mut(&task.run_id).unwrap().native_output = Some(NativeOutputObservation {
            revision: 7,
            bytes: u64::MAX,
        });
        reopened.persist(&ledger).unwrap();
    }
    let reopened_invalid = AgentStore::open(dir.path()).unwrap();
    assert_eq!(
        reopened_invalid
            .native_output_bytes_for_request(&owner(), "request-1", 7)
            .unwrap(),
        Some(MAX_NATIVE_OUTPUT_BYTES)
    );
}

#[test]
fn protocol_finalization_blocks_readers_until_terminal_event_is_durable() {
    use std::{
        sync::{mpsc, Arc},
        thread,
        time::Duration,
    };
    let (_dir, store) = store();
    let store = Arc::new(store);
    let mut input = input();
    input.protocol_thread_id = Some("barrier-thread".into());
    let run = store.admit(&owner(), &input, &[]).unwrap().task;
    store
        .update(&owner(), &run.run_id, State::Running, None, None, None)
        .unwrap();
    store
        .append_event(
            &owner(),
            &run.run_id,
            serde_json::json!({
                "type":"RUN_STARTED","threadId":"barrier-thread","runId":"request-1"
            }),
        )
        .unwrap();
    let (updated_tx, updated_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer_store = store.clone();
    let run_id = run.run_id.clone();
    let writer = thread::spawn(move || {
        writer_store.with_protocol_commit_permit(|permit| {
            writer_store
                .prepare_protocol_terminal(
                    permit,
                    &owner(),
                    &run_id,
                    State::Cancelled,
                    None,
                    None,
                    Some("cancelled".into()),
                )
                .unwrap();
            updated_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            writer_store.append_protocol_commit_event(permit, &owner(), &run_id, serde_json::json!({
                "type":"RUN_FINISHED","threadId":"barrier-thread","runId":"request-1","outcome":{"type":"cancelled"}
            })).unwrap();
            writer_store
                .finalize_protocol_terminal(permit, &owner(), &run_id)
                .unwrap();
        }).unwrap();
    });
    updated_rx.recv().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let reader_store = store.clone();
    let reader_id = run.run_id.clone();
    let reader = thread::spawn(move || {
        ready_tx.send(()).unwrap();
        let status = reader_store.get(&owner(), &reader_id).unwrap();
        let page = reader_store
            .protocol_events(&owner(), "request-1", 0)
            .unwrap();
        observed_tx.send((status, page)).unwrap();
    });
    ready_rx.recv().unwrap();
    assert!(
        matches!(
            observed_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "readers must not observe terminal status before its event is published"
    );
    release_tx.send(()).unwrap();
    let (status, page) = observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(status.state, State::Cancelled);
    assert_eq!(page.events.last().unwrap()["type"], "RUN_FINISHED");
    writer.join().unwrap();
    reader.join().unwrap();
}

fn failed_recovery_turn(store: &AgentStore, chat_id: &str, run_id: &str) -> (Input, Task) {
    let normalized = protocol::normalize_run_input(
        &json!({
            "threadId":chat_id,"runId":run_id,"protocolVersion":"1.0",
            "messages":[{"id":format!("user-{run_id}"),"role":"user","content":"retained input"}]
        }),
        &[],
    )
    .unwrap();
    let admission = store
        .admit_scoped(&owner(), &normalized.input, &[], &[], Some(chat_id), true)
        .unwrap();
    store
        .append_event(
            &owner(),
            &admission.task.run_id,
            json!({
                "type":"RUN_STARTED","protocolVersion":"1.0","threadId":chat_id,"runId":run_id
            }),
        )
        .unwrap();
    let task = commit_protocol_terminal_for_test(
        store,
        &owner(),
        &admission.task.run_id,
        State::Failed,
        None,
        Some("model_error".into()),
    )
    .unwrap();
    (normalized.input, task)
}

#[test]
fn prepared_chat_recovery_survives_restart_and_only_committed_authority_allows_new_run() {
    let (directory, store) = store();
    let chat_id = uuid::Uuid::new_v4().to_string();
    let (old_input, old_task) = failed_recovery_turn(&store, &chat_id, "failed-before-recovery");
    let event_path = store.event_path(&old_task.run_id).unwrap();
    let event_evidence = fs::read(&event_path).unwrap();
    let plan = store.prepare_chat_recovery(&owner(), &chat_id).unwrap();
    assert!(store
        .committed_chat_recoveries(&owner().app_id)
        .unwrap()
        .is_empty());
    drop(store);
    let store = AgentStore::open(directory.path()).unwrap();
    assert_eq!(
        store.prepare_chat_recovery(&owner(), &chat_id).unwrap(),
        plan
    );
    let mut next = old_input.clone();
    next.request_id = "after-recovery".into();
    assert!(store
        .admit_scoped(&owner(), &next, &[], &[], Some(&chat_id), true)
        .is_err());
    // This is the ledger-only contract: physical staging is independently
    // tested by Session, and production commits only after its proof checks.
    store.commit_chat_recovery(&plan).unwrap();
    store.commit_chat_recovery(&plan).unwrap();
    assert!(
        store
            .admit_scoped(&owner(), &next, &[], &[], Some(&chat_id), true)
            .unwrap()
            .start
    );
    let duplicate = store
        .admit_scoped(&owner(), &old_input, &[], &[], Some(&chat_id), true)
        .unwrap();
    assert!(!duplicate.start);
    assert_eq!(duplicate.task.state, State::Failed);
    assert_eq!(duplicate.task.run_id, old_task.run_id);
    assert_eq!(fs::read(event_path).unwrap(), event_evidence);
}

#[test]
fn recovery_commit_rename_failure_requires_reopen_without_rewriting_attempt_outcome() {
    let (directory, store) = store();
    let chat_id = uuid::Uuid::new_v4().to_string();
    let (_, original) = failed_recovery_turn(&store, &chat_id, "rename-fault");
    let plan = store.prepare_chat_recovery(&owner(), &chat_id).unwrap();
    store
        .fail_next_persist_after_rename
        .store(true, Ordering::Release);
    assert!(store.commit_chat_recovery(&plan).is_err());
    assert!(store.committed_chat_recoveries(&owner().app_id).is_err());
    drop(store);
    let store = AgentStore::open(directory.path()).unwrap();
    assert_eq!(
        store
            .committed_chat_recoveries(&owner().app_id)
            .unwrap()
            .get(&chat_id),
        Some(&plan)
    );
    assert_eq!(
        store.get(&owner(), &original.run_id).unwrap().state,
        State::Failed
    );
}

#[test]
fn recovery_does_not_restore_revoked_grants_or_allow_old_run_ids_on_a_new_release() {
    let (_directory, store) = store();
    let chat_id = uuid::Uuid::new_v4().to_string();
    let (old_input, _) = failed_recovery_turn(&store, &chat_id, "old-public-run");
    let plan = store.prepare_chat_recovery(&owner(), &chat_id).unwrap();
    store.commit_chat_recovery(&plan).unwrap();
    let next_owner = Owner {
        release_digest: "3".repeat(64),
        ..owner()
    };
    assert!(store.prepare_chat_recovery(&next_owner, &chat_id).is_err());
    store
        .configure(next_owner.clone(), Some(settings()))
        .unwrap();
    assert!(store
        .admit_scoped(&next_owner, &old_input, &[], &[], Some(&chat_id), true)
        .is_err());
    store.configure(next_owner.clone(), None).unwrap();
    let mut next = old_input;
    next.request_id = "fresh-after-revoke".into();
    assert!(store
        .admit_scoped(&next_owner, &next, &[], &[], Some(&chat_id), true)
        .is_err());
    let foreign_owner = Owner {
        app_id: format!("a{}", "9".repeat(32)),
        ..next_owner
    };
    assert!(store
        .archived_chat_attempts(&foreign_owner, &chat_id)
        .unwrap()
        .is_empty());
}
