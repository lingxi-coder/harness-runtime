//! from an on-disk JSONL so the next live turn's append chains correctly.

use lingxi_core::session::{ActiveGoalState, GoalOrigin};
use orchestrator::{
    replay_session_state, runtime_metadata_from_messages, state_from_messages, ResumeError,
};
use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use protocol::ConversationMessage;
use serde_json::json;
use session::jsonl::project_dir_name;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

async fn setup_two_turn_jsonl() -> (
    TempDir,
    std::path::PathBuf,
    String,
    Uuid,
    Uuid,
    Arc<dyn FileSystem>,
) {
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();

    let sid = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let m2 = Uuid::new_v4();
    let body = format!(
        "{}\n{}\n",
        serde_json::to_string(&json!({
            "type": "user",
            "uuid": m1.to_string(),
            "parentUuid": null,
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": cwd,
            "version": "0.6.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": "hi"}
        }))
        .unwrap(),
        serde_json::to_string(&json!({
            "type": "assistant",
            "uuid": m2.to_string(),
            "parentUuid": m1.to_string(),
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:01.000Z",
            "cwd": cwd,
            "version": "0.6.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "assistant", "content": "hello"}
        }))
        .unwrap(),
    );
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    (temp, lingxi_home, cwd, sid, m2, fs)
}

#[tokio::test]
async fn cold_resume_consumes_physical_retry_removal_without_breaking_chain() {
    let (_temp, home, cwd, sid, discarded, fs) = setup_two_turn_jsonl().await;
    let path = home
        .join("projects")
        .join(project_dir_name(&cwd))
        .join(format!("{sid}.jsonl"));
    let writer = session::jsonl::writer::JsonlWriter::new(path.clone(), fs.clone());
    let parent = writer
        .remove_retry_attempt(&discarded.to_string())
        .await
        .unwrap();
    let clean = Uuid::new_v4();
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await
        .unwrap();
    for row in [json!({
        "type":"assistant","uuid":clean.to_string(),"parentUuid":parent,
        "sessionId":sid.to_string(),"timestamp":"2026-05-25T12:00:02.000Z",
        "cwd":cwd,"version":"0.6.0","isSidechain":false,
        "message":{"role":"assistant","content":"clean retry"}
    })] {
        file.write_all(format!("{row}\n").as_bytes()).await.unwrap();
    }
    drop(file);
    let replayed = replay_session_state(&home, &cwd, sid, fs).await.unwrap();
    assert_eq!(replayed.last_message_uuid, Some(clean));
    let visible: Vec<_> = replayed
        .state
        .history
        .iter()
        .filter(|message| {
            !replayed
                .state
                .model_context_excluded_messages
                .contains(&message.id())
        })
        .collect();
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].text_content(), "hi");
    assert_eq!(visible[1].text_content(), "clean retry");
    assert!(!replayed
        .state
        .history
        .iter()
        .any(|message| message.id() == protocol::MessageId::from_uuid(discarded)));
}

#[tokio::test]
async fn cold_resume_restores_trailing_thinking_marker_scope() {
    let (_temp, home, cwd, sid, previous, fs) = setup_two_turn_jsonl().await;
    let path = home
        .join("projects")
        .join(project_dir_name(&cwd))
        .join(format!("{sid}.jsonl"));
    let thinking_id = Uuid::new_v4();
    let marker_id = Uuid::new_v4();
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await
        .unwrap();
    for row in [
        json!({
            "type":"assistant","uuid":thinking_id.to_string(),"parentUuid":previous.to_string(),
            "sessionId":sid.to_string(),"timestamp":"2026-05-25T12:00:02.000Z",
            "cwd":cwd,"version":"0.6.0","isSidechain":false,
            "message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret","signature":"sig"}]}
        }),
        json!({
            "type":"attachment","uuid":marker_id.to_string(),"parentUuid":thinking_id.to_string(),
            "sessionId":sid.to_string(),"timestamp":"2026-05-25T12:00:03.000Z",
            "cwd":cwd,"version":"0.6.0","isSidechain":false,
            "attachment":{"type":"thinking_stripped","scope":"all"}
        }),
    ] {
        file.write_all(format!("{row}\n").as_bytes()).await.unwrap();
    }
    drop(file);
    let replayed = replay_session_state(&home, &cwd, sid, fs).await.unwrap();
    assert_eq!(replayed.last_message_uuid, Some(marker_id));
    assert_eq!(
        replayed
            .state
            .thinking_stripped_messages
            .get(&protocol::MessageId::from_uuid(thinking_id)),
        Some(&0)
    );
    assert_eq!(replayed.state.history.len(), 3);
}

#[tokio::test]
async fn replay_returns_state_with_last_uuid_set() {
    let (_temp, lingxi_home, cwd, sid, last_uuid, fs) = setup_two_turn_jsonl().await;
    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    assert_eq!(replayed.messages.len(), 2);
    assert_eq!(replayed.last_message_uuid, Some(last_uuid));
    assert_eq!(replayed.state.session_id.as_uuid(), sid);
    assert_eq!(replayed.state.history.len(), 2);
    match &replayed.state.history[0] {
        ConversationMessage::User { .. } => {}
        other => panic!("expected User first, got {other:?}"),
    }
    match &replayed.state.history[1] {
        ConversationMessage::Assistant { .. } => {}
        other => panic!("expected Assistant second, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_recovers_off_chain_prompt_snapshot_from_full_entries() {
    // The resumable chain is intentionally narrower than the routed entry
    // stream: an attachment can be a sibling of the active user/assistant
    // branch and therefore be absent from `replayed.messages`.  Cold resume
    // must still recover the latest prompt snapshot from all transcript
    // entries, otherwise the next turn silently rebuilds a live prompt.
    let (_temp, lingxi_home, cwd, sid, last_uuid, fs) = setup_two_turn_jsonl().await;
    let transcript_path = session::jsonl::session_path(&lingxi_home, &cwd, &sid.to_string());
    let branch_user = Uuid::new_v4();
    let branch_assistant = Uuid::new_v4();
    let snapshot_uuid = Uuid::new_v4();
    let lines = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&json!({
            "type": "attachment",
            "uuid": snapshot_uuid.to_string(),
            "parentUuid": last_uuid.to_string(),
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:01.500Z",
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": false,
            "message": {},
            "attachment": {
                "type": "prompt_snapshot",
                "systemPrompt": ["frozen from an off-chain attachment"],
                "tools": [{"name": "Read", "description": "frozen Read"}]
            }
        }))
        .unwrap(),
        serde_json::to_string(&json!({
            "type": "user",
            "uuid": branch_user.to_string(),
            "parentUuid": last_uuid.to_string(),
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:02.000Z",
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": "continue"}
        }))
        .unwrap(),
        serde_json::to_string(&json!({
            "type": "assistant",
            "uuid": branch_assistant.to_string(),
            "parentUuid": branch_user.to_string(),
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:03.000Z",
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "assistant", "content": "ready"}
        }))
        .unwrap(),
    );
    tokio::fs::OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .await
        .unwrap()
        .write_all(lines.as_bytes())
        .await
        .unwrap();

    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    assert_eq!(
        replayed.last_message_uuid,
        Some(branch_assistant),
        "the active chain still resumes from the latest assistant line"
    );
    assert!(
        replayed
            .messages
            .iter()
            .all(|message| message.uuid != snapshot_uuid.to_string()),
        "the sibling attachment must remain outside the selected chain"
    );
    let snapshot = replayed
        .runtime_metadata
        .prompt_snapshot
        .expect("full-entry restore must recover the off-chain snapshot");
    assert_eq!(
        snapshot.system_prompt,
        vec!["frozen from an off-chain attachment".to_string()]
    );
    assert_eq!(snapshot.tools[0].name, "Read");
}

#[tokio::test]
async fn replay_carries_integrity_checked_main_agent_snapshot_for_hot_resume() {
    let (_temp, lingxi_home, cwd, sid, _last_uuid, fs) = setup_two_turn_jsonl().await;
    let transcript_path = session::jsonl::session_path(&lingxi_home, &cwd, &sid.to_string());
    let definition = json!({
        "agent_type": "reviewer",
        "system_prompt": "frozen prompt",
        "tools": {"Explicit": ["Read"]}
    });
    session::jsonl::JsonlWriter::new(transcript_path, fs.clone())
        .append_agent_setting_snapshot(&sid.to_string(), "reviewer", &definition)
        .await
        .expect("append agent snapshot");

    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    let runtime = replayed.handle_runtime_snapshot();
    assert_eq!(runtime.main_thread_agent_type.as_deref(), Some("reviewer"));
    assert_eq!(runtime.main_thread_agent_definition, Some(definition));
}

/// 🔒 AG-5: the snapshot's integrity check must REFUSE a definition that was
/// edited after it was recorded.
///
/// The sibling test above proves a good snapshot round-trips, which is the easy
/// half — it passes just as well with the hash check deleted. This is the half
/// the guarantee actually rests on: upstream's 2.1.268 teammate-respawn fix
/// exists because a respawn could pick up an untrusted agent file sharing the
/// resumed agent's name, and this port is immune only for as long as a
/// mismatched hash means "no definition".
///
/// The agent TYPE still restores from the sibling `agentSetting` field, which
/// is the correct split: the name is not the thing that carries a system prompt
/// and a tool list.
#[tokio::test]
async fn a_tampered_agent_snapshot_is_refused_while_the_type_still_restores() {
    let (_temp, lingxi_home, cwd, sid, _last_uuid, fs) = setup_two_turn_jsonl().await;
    let transcript_path = session::jsonl::session_path(&lingxi_home, &cwd, &sid.to_string());
    let definition = json!({
        "agent_type": "reviewer",
        "system_prompt": "frozen prompt",
        "tools": {"Explicit": ["Read"]}
    });
    session::jsonl::JsonlWriter::new(transcript_path.clone(), fs.clone())
        .append_agent_setting_snapshot(&sid.to_string(), "reviewer", &definition)
        .await
        .expect("append agent snapshot");

    // Substitute a different definition under the RECORDED hash — exactly what
    // an attacker-supplied same-named agent file would amount to.
    let raw = std::fs::read_to_string(&transcript_path).expect("read transcript");
    assert!(
        raw.contains("frozen prompt"),
        "fixture must contain the definition it is about to tamper with"
    );
    let tampered = raw.replace("frozen prompt", "attacker prompt");
    assert_ne!(tampered, raw, "the tamper must actually change the file");
    std::fs::write(&transcript_path, &tampered).expect("write tampered transcript");

    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    let runtime = replayed.handle_runtime_snapshot();
    assert_eq!(
        runtime.main_thread_agent_definition, None,
        "a definition whose sha256 no longer matches must not be restored"
    );
    assert_eq!(
        runtime.main_thread_agent_type.as_deref(),
        Some("reviewer"),
        "the agent NAME is a separate field and still restores"
    );
}

#[tokio::test]
async fn state_from_messages_matches_disk_replay() {
    // (M5-13) The CLI resume mount seeds the orchestrator session from the
    // transcript ALREADY in hand (no second disk read). `state_from_messages`
    // over `replayed.messages` must reproduce the SAME `SessionState.history` +
    // `session_id` the on-disk `replay_session_state` produced.
    let (_temp, lingxi_home, cwd, sid, _last_uuid, fs) = setup_two_turn_jsonl().await;
    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");

    let from_hand = state_from_messages(sid, &replayed.messages);
    assert_eq!(from_hand.session_id, replayed.state.session_id);
    assert_eq!(from_hand.history.len(), replayed.state.history.len());
    assert_eq!(from_hand.history, replayed.state.history);
}

#[test]
fn resume_restores_effort_compaction_counters_and_rapid_refill_tracking() {
    let sid = Uuid::new_v4();
    let line = |value: serde_json::Value| serde_json::from_value(value).unwrap();
    let messages = vec![
        line(json!({
            "type":"system", "subtype":"compact_boundary",
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{}, "compactMetadata":{"preTokens":1000,"postTokens":400,"cumulativeDroppedTokens":600}
        })),
        line(json!({
            "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:01.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":"one"}, "effort":"high"
        })),
        line(json!({
            "type":"system", "subtype":"compact_boundary",
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:02.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{}, "compactMetadata":{"preTokens":900,"postTokens":300,"cumulativeDroppedTokens":1200}
        })),
        line(json!({
            "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:03.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":"two"}, "effort":"xhigh"
        })),
    ];

    let metadata = runtime_metadata_from_messages(&messages);
    assert_eq!(metadata.effort.as_deref(), Some("xhigh"));
    assert_eq!(metadata.cumulative_dropped_tokens, 1200);
    assert!(metadata.compaction_tracking.compacted);
    assert_eq!(metadata.compaction_tracking.turn_counter, 1);
    assert_eq!(metadata.compaction_tracking.consecutive_rapid_refills, 1);
}

#[test]
fn resume_detects_an_open_plan_segment_from_the_latest_user_permission_mode() {
    let sid = Uuid::new_v4();
    let messages = vec![serde_json::from_value(json!({
        "type":"user", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-08-25T00:00:00.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
        "permissionMode":"plan",
        "message":{"role":"user","content":"keep planning"}
    }))
    .unwrap()];

    let state = state_from_messages(sid, &messages);
    assert!(state.plan_mode);
}

#[test]
fn resume_restores_model_exclusion_for_assistant_transcript_messages() {
    let sid = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let messages = vec![serde_json::from_value(json!({
        "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-09-04T00:00:00.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
        "isModelContextExcluded":true,
        "message":{
            "id":message_id.to_string(),
            "role":"assistant",
            "content":"No scheduled prompts."
        }
    }))
    .unwrap()];

    let state = state_from_messages(sid, &messages);
    assert!(
        state
            .model_context_excluded_messages
            .contains(&protocol::MessageId::from_uuid(message_id)),
        "a persisted slash result must remain excluded from model input after resume",
    );
}

#[test]
fn resume_does_not_reenter_a_plan_segment_after_a_successful_exit() {
    let sid = Uuid::new_v4();
    let tool_use_id = "toolu_exit_plan";
    let messages = vec![
        serde_json::from_value(json!({
            "type":"user", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-08-25T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "permissionMode":"plan",
            "message":{"role":"user","content":"finish the plan"}
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-08-25T00:00:01.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":[{
                "type":"tool_use","id":tool_use_id,"name":"ExitPlanMode","input":{}
            }]}
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type":"user", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-08-25T00:00:02.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"user","content":[{
                "type":"tool_result","tool_use_id":tool_use_id,"content":"Plan approved","is_error":false
            }]}
        }))
        .unwrap(),
    ];

    let state = state_from_messages(sid, &messages);
    assert!(!state.plan_mode);
}

#[test]
fn resume_recovers_active_goal_from_compact_metadata_and_later_updates() {
    let sid = Uuid::new_v4();
    let line = |value: serde_json::Value| serde_json::from_value(value).unwrap();
    let set_at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let compact_goal = ActiveGoalState {
        condition: "ship it".to_string(),
        set_at,
        last_reason: Some("initial".to_string()),
        iterations: 1,
        tokens_at_start: 10,
        // `origin` is `serde(skip)`, so this never reaches the transcript — the
        // fold re-derives it. Left as the default so the assertion below can
        // only pass if the fold actually stamped it.
        origin: GoalOrigin::User,
    };
    let updated_goal = ActiveGoalState {
        condition: "ship it".to_string(),
        set_at,
        last_reason: Some("still working".to_string()),
        iterations: 2,
        tokens_at_start: 10,
        origin: GoalOrigin::User,
    };
    let messages = vec![
        line(json!({
            "type":"system", "subtype":"compact_boundary",
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{}, "compactMetadata":{"preTokens":1000,"postTokens":400,"activeGoal":serde_json::to_value(&compact_goal).unwrap()}
        })),
        line(json!({
            "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:01.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":"one"}
        })),
        line(json!({
            "type":"system", "subtype":"thread_goal_updated",
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:02.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":null, "goalState":serde_json::to_value(&updated_goal).unwrap()
        })),
    ];

    let state = state_from_messages(sid, &messages);
    let restored = state.active_goal.expect("goal restored");
    assert_eq!(restored.condition, "ship it");
    assert_eq!(restored.last_reason.as_deref(), Some("still working"));
    assert_eq!(
        restored.origin,
        GoalOrigin::Restored,
        "`mon` stamps `origin:\"restored\"` on whatever it recovers, so the \
         `tengu_goal_*` metrics do not report a resumed goal as freshly set"
    );
}

#[test]
fn resume_prefers_typed_goal_status_attachment_and_honors_achieved() {
    let sid = Uuid::new_v4();
    let set_at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let snapshot = platform_api::ActiveGoalSnapshot {
        condition: "ship it".to_string(),
        set_at,
        last_reason: Some("tests pending".to_string()),
        iterations: 2,
        tokens_at_start: 500,
    };
    let attachment = |status, goal_state: Option<platform_api::ActiveGoalSnapshot>| match status {
        platform_api::GoalStatusKind::Set => {
            platform_api::GoalStatusAttachment::sentinel_set("ship it".to_string(), goal_state)
        }
        platform_api::GoalStatusKind::Cleared => {
            platform_api::GoalStatusAttachment::sentinel_cleared("ship it".to_string())
        }
        platform_api::GoalStatusKind::Achieved => platform_api::GoalStatusAttachment::achieved(
            "ship it".to_string(),
            Some("tests pending".to_string()),
            2,
            1000,
            200,
        ),
        platform_api::GoalStatusKind::Failed => platform_api::GoalStatusAttachment::failed(
            "ship it".to_string(),
            Some("tests pending".to_string()),
            2,
            1000,
            200,
        ),
        platform_api::GoalStatusKind::NotMet => platform_api::GoalStatusAttachment::not_met(
            "ship it".to_string(),
            Some("tests pending".to_string()),
            goal_state,
        ),
    };
    let line = |payload: platform_api::GoalStatusAttachment| {
        serde_json::from_value(json!({
            "type":"attachment", "attachment":payload,
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
        }))
        .unwrap()
    };
    let active = line(attachment(
        platform_api::GoalStatusKind::Set,
        Some(snapshot.clone()),
    ));
    let state = state_from_messages(sid, &[active]);
    let goal = state
        .active_goal
        .expect("typed set attachment restores goal");
    assert_eq!(goal.iterations, 2);
    assert_eq!(goal.tokens_at_start, 500);
    assert_eq!(goal.origin, GoalOrigin::Restored);

    for status in [
        platform_api::GoalStatusKind::Achieved,
        platform_api::GoalStatusKind::Failed,
        platform_api::GoalStatusKind::Cleared,
    ] {
        let terminal = line(attachment(status, None));
        assert!(state_from_messages(sid, &[terminal]).active_goal.is_none());
    }

    // A not-met turn is NOT terminal: the goal keeps running, refreshed from the
    // snapshot the record carries.
    let not_met = line(attachment(
        platform_api::GoalStatusKind::NotMet,
        Some(snapshot.clone()),
    ));
    let still_running = state_from_messages(sid, &[not_met])
        .active_goal
        .expect("a not-met record leaves the goal active");
    assert_eq!(still_running.iterations, 2);
}

#[test]
fn resume_normalizes_stopped_hook_attachment_into_one_meta_message() {
    let sid = Uuid::new_v4();
    let message_uuid = Uuid::new_v4();
    let attachment = serde_json::from_value(json!({
        "type":"attachment",
        "attachment":{
            "type":"hook_stopped_continuation",
            "message":"STOP-NOW",
            "hookName":"PostToolUseFailure:Write",
            "toolUseID":"tool-1",
            "hookEvent":"PostToolUseFailure"
        },
        "uuid":message_uuid.to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
    }))
    .unwrap();

    let state = state_from_messages(sid, &[attachment]);
    assert_eq!(state.history.len(), 1);
    assert!(state.history[0].is_meta());
    assert_eq!(
        state.history[0].text_content(),
        "<system-reminder>\nPostToolUseFailure:Write hook stopped continuation: STOP-NOW\n</system-reminder>"
    );
    assert_eq!(state.history[0].id().as_uuid(), message_uuid);
}

#[test]
fn resume_normalizes_hook_additional_context_into_one_meta_message() {
    let sid = Uuid::new_v4();
    let message_uuid = Uuid::new_v4();
    let attachment = serde_json::from_value(json!({
        "type":"attachment",
        "attachment":{
            "type":"hook_additional_context",
            "content":["line-a","line-b"],
            "hookName":"PostToolUse:Write",
            "toolUseID":"tool-1",
            "hookEvent":"PostToolUse"
        },
        "uuid":message_uuid.to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
    }))
    .unwrap();

    let state = state_from_messages(sid, &[attachment]);
    assert_eq!(state.history.len(), 1);
    assert!(state.history[0].is_meta());
    assert_eq!(
        state.history[0].text_content(),
        "<system-reminder>\nPostToolUse:Write hook additional context: line-a\nline-b\n</system-reminder>"
    );
    assert_eq!(state.history[0].id().as_uuid(), message_uuid);
}

#[test]
fn resume_rejects_malformed_hook_context_without_partial_replay() {
    let sid = Uuid::new_v4();
    for content in [
        json!(["valid", null]),
        json!([42]),
        json!([{}]),
        json!("text"),
    ] {
        let attachment = serde_json::from_value(json!({
            "type":"attachment",
            "attachment":{
                "type":"hook_additional_context",
                "content":content,
                "hookName":"PostToolUse:Write"
            },
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-09-08T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
        }))
        .unwrap();
        assert!(state_from_messages(sid, &[attachment]).history.is_empty());
    }
}

#[test]
fn resume_drops_malformed_text_blocks_without_losing_valid_siblings() {
    let sid = Uuid::new_v4();
    for role in ["user", "assistant"] {
        let message = serde_json::from_value(json!({
            "type":role,
            "message":{
                "role":role,
                "content":[
                    {"type":"text","text":"before"},
                    {"type":"text","text":null},
                    {"type":"text"},
                    {"type":"text","text":123},
                    {"type":"text","text":"after"}
                ]
            },
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-09-08T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
        }))
        .unwrap();
        let state = state_from_messages(sid, &[message]);
        assert_eq!(state.history.len(), 1);
        let content = match &state.history[0] {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content,
            other => panic!("unexpected message: {other:?}"),
        };
        assert_eq!(
            content,
            &vec![
                protocol::ContentBlock::Text {
                    text: "before".into()
                },
                protocol::ContentBlock::Text {
                    text: "after".into()
                },
            ]
        );
    }
}

#[test]
fn resume_skips_empty_hook_additional_context() {
    let sid = Uuid::new_v4();
    let attachment = serde_json::from_value(json!({
        "type":"attachment",
        "attachment":{
            "type":"hook_additional_context",
            "content":[],
            "hookName":"PostToolUse:Write",
            "toolUseID":"tool-1",
            "hookEvent":"PostToolUse"
        },
        "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
    }))
    .unwrap();

    let state = state_from_messages(sid, &[attachment]);
    assert!(
        state.history.is_empty(),
        "empty hook_additional_context must not enter the resumed request"
    );
}

#[test]
fn resume_interleaves_hook_additional_context_with_parallel_tool_results() {
    let sid = Uuid::new_v4();
    let assistant_uuid = Uuid::new_v4();
    let result_a = Uuid::new_v4();
    let hook_uuid = Uuid::new_v4();
    let result_b = Uuid::new_v4();
    let messages = vec![
        serde_json::from_value(json!({
            "type":"assistant", "uuid":assistant_uuid.to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:01.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":[
                {"type":"tool_use","id":"a","name":"Echo","input":{}},
                {"type":"tool_use","id":"b","name":"Echo","input":{}}
            ]}
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type":"user", "uuid":result_a.to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:02.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"a","content":"A","is_error":false}
            ]}
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type":"attachment",
            "attachment":{
                "type":"hook_additional_context",
                "content":["HOOK-CTX"],
                "hookName":"PostToolUse:Echo",
                "toolUseID":"a",
                "hookEvent":"PostToolUse"
            },
            "uuid":hook_uuid.to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:02.100Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type":"user", "uuid":result_b.to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:03.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"b","content":"B","is_error":false}
            ]}
        }))
        .unwrap(),
    ];

    let state = state_from_messages(sid, &messages);
    assert_eq!(
        state.history.len(),
        4,
        "assistant + result A + hook ctx + result B"
    );
    assert!(matches!(
        state.history[0],
        ConversationMessage::Assistant { .. }
    ));
    assert_eq!(state.history[1].id().as_uuid(), result_a);
    assert!(state.history[2].is_meta());
    assert_eq!(
        state.history[2].text_content(),
        "<system-reminder>\nPostToolUse:Echo hook additional context: HOOK-CTX\n</system-reminder>"
    );
    assert_eq!(state.history[3].id().as_uuid(), result_b);
}

#[test]
fn resume_thinking_markers_are_scoped_to_preceding_history() {
    let sid = Uuid::new_v4();
    let old = Uuid::new_v4();
    let fresh = Uuid::new_v4();
    let row = |kind: &str, message: serde_json::Value, attachment: serde_json::Value| {
        serde_json::from_value(json!({
            "type":kind, "message":message, "attachment":attachment,
            "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-09-08T00:00:00.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
        }))
        .unwrap()
    };
    let thinking = json!({"type":"thinking","thinking":"same","signature":"sig"});
    for (scope, from, expected) in [
        ("all", json!(null), 0),
        (
            "partial",
            json!({"messageId":old.to_string(),"thinkingIndex":1}),
            1,
        ),
        (
            "partial",
            json!({"messageId":"missing","thinkingIndex":0}),
            0,
        ),
    ] {
        let messages = vec![
            row(
                "assistant",
                json!({"id":old.to_string(),"role":"assistant","content":[thinking.clone()]}),
                json!(null),
            ),
            row(
                "assistant",
                json!({"id":old.to_string(),"role":"assistant","content":[thinking.clone()]}),
                json!(null),
            ),
            row(
                "attachment",
                json!(null),
                json!({"type":"thinking_stripped","scope":scope,"from":from}),
            ),
            row(
                "assistant",
                json!({"id":fresh.to_string(),"role":"assistant","content":[thinking.clone()]}),
                json!(null),
            ),
        ];
        let state = state_from_messages(sid, &messages);
        assert_eq!(
            state
                .thinking_stripped_messages
                .get(&protocol::MessageId::from_uuid(old)),
            Some(&expected)
        );
        assert!(!state
            .thinking_stripped_messages
            .contains_key(&protocol::MessageId::from_uuid(fresh)));
        assert_eq!(
            state.history.len(),
            2,
            "markers must not enter model history"
        );
    }
}

#[test]
fn resume_restores_thinking_stripped_latch_from_attachment() {
    let sid = Uuid::new_v4();
    let attachment = serde_json::from_value(json!({
        "type":"attachment",
        "attachment":{"type":"thinking_stripped","scope":"all"},
        "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:00.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false
    }))
    .unwrap();

    let state = state_from_messages(sid, &[attachment]);
    assert!(
        state.thinking_signature_stripped,
        "thinking_stripped attachment must restore the session latch"
    );
    assert!(
        state.history.is_empty(),
        "thinking_stripped is a latch marker, not a model-facing message"
    );
}

#[test]
fn resume_without_goal_metadata_defaults_to_none() {
    let sid = Uuid::new_v4();
    let messages = vec![serde_json::from_value(json!({
        "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
        "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:01.000Z",
        "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
        "message":{"role":"assistant","content":"one"}
    }))
    .unwrap()];

    assert!(
        state_from_messages(sid, &messages).active_goal.is_none(),
        "legacy sessions without goal metadata must resume with no active goal"
    );
}

#[test]
fn resume_restores_last_assistant_timestamp_sidecar() {
    let sid = Uuid::new_v4();
    let messages = vec![
        serde_json::from_value(json!({
            "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:01.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":"one"}
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type":"assistant", "uuid":Uuid::new_v4().to_string(), "parentUuid":null,
            "sessionId":sid.to_string(), "timestamp":"2026-07-19T00:00:03.000Z",
            "cwd":"/tmp", "version":"0.12.0", "isSidechain":false,
            "message":{"role":"assistant","content":"two"}
        }))
        .unwrap(),
    ];

    let state = state_from_messages(sid, &messages);
    let restored = state
        .message_timing
        .last_assistant_at
        .expect("assistant timestamp restored");
    let expected: std::time::SystemTime =
        chrono::DateTime::parse_from_rfc3339("2026-07-19T00:00:03.000Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .into();
    assert_eq!(restored, expected);
}

#[tokio::test]
async fn resume_recovers_the_saved_model_from_the_last_assistant_line() {
    // Regression (reported): a resumed session showed the launch-default model
    // instead of the model it was saved on. `build_state_from_jsonl` seeds
    // DEFAULT_MODEL, then the last assistant line's `message.model` overrides it.
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    let sid = Uuid::new_v4();
    let (m1, m2) = (Uuid::new_v4(), Uuid::new_v4());
    let body = format!(
        "{}\n{}\n",
        serde_json::to_string(&json!({
            "type": "user", "uuid": m1.to_string(), "parentUuid": null,
            "sessionId": sid.to_string(), "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": cwd, "version": "0.6.0", "isSidechain": false, "userType": "external",
            "message": {"role": "user", "content": "hi"}
        }))
        .unwrap(),
        serde_json::to_string(&json!({
            "type": "assistant", "uuid": m2.to_string(), "parentUuid": m1.to_string(),
            "sessionId": sid.to_string(), "timestamp": "2026-05-25T12:00:01.000Z",
            "cwd": cwd, "version": "0.6.0", "isSidechain": false, "userType": "external",
            "modelProfile": "deepseek",
            "message": {"role": "assistant", "content": "hi there", "model": "deepseek-v4-pro"}
        }))
        .unwrap(),
    );
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    assert_eq!(
        replayed.state.model, "deepseek-v4-pro",
        "resume recovers the saved model from the last assistant line"
    );
    assert_eq!(replayed.state.model_profile.as_deref(), Some("deepseek"));
    // The in-hand path (`state_from_messages`, used by the CLI resume mount)
    // recovers the same model.
    let in_hand = state_from_messages(sid, &replayed.messages);
    assert_eq!(in_hand.model, "deepseek-v4-pro");
    assert_eq!(in_hand.model_profile.as_deref(), Some("deepseek"));
}

#[tokio::test]
async fn resume_skips_synthetic_model_marker() {
    // Regression (reported "model unavailable" after resume): when the LAST
    // assistant line is a SYNTHETIC error/system message (model "<synthetic>",
    // e.g. a request-rejection notice), resume must NOT adopt "<synthetic>" as
    // the active model — that would fail `resolve_in` → ModelUnavailable on the
    // first turn. It keeps the last REAL model instead.
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();
    let sid = Uuid::new_v4();
    let (m1, m2, m3) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let line = |uuid: Uuid, parent: Option<Uuid>, role: &str, model: Option<&str>, text: &str| {
        let mut msg = json!({"role": role, "content": text});
        if let Some(m) = model {
            msg["model"] = json!(m);
        }
        serde_json::to_string(&json!({
            "type": role, "uuid": uuid.to_string(),
            "parentUuid": parent.map(|p| p.to_string()),
            "sessionId": sid.to_string(), "timestamp": "2026-05-25T12:00:00.000Z",
            "cwd": cwd, "version": "0.6.0", "isSidechain": false, "userType": "external",
            "message": msg,
        }))
        .unwrap()
    };
    let body = format!(
        "{}\n{}\n{}\n",
        line(m1, None, "user", None, "hi"),
        line(m2, Some(m1), "assistant", Some("gpt-5.5"), "hi there"),
        // A synthetic error message closed the session.
        line(
            m3,
            Some(m2),
            "assistant",
            Some("<synthetic>"),
            "invalid request: ..."
        ),
    );
    tokio::fs::write(subdir.join(format!("{sid}.jsonl")), body)
        .await
        .unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    assert_eq!(
        replayed.state.model, "gpt-5.5",
        "synthetic marker is skipped; the last REAL model is kept"
    );
    assert_eq!(
        state_from_messages(sid, &replayed.messages).model,
        "gpt-5.5"
    );
}

#[tokio::test]
async fn resume_without_a_model_field_keeps_the_default() {
    // A transcript with no `message.model` (or no assistant lines) keeps the
    // DEFAULT_MODEL seed — the fallback stays correct.
    let (_temp, lingxi_home, cwd, sid, _last, fs) = setup_two_turn_jsonl().await;
    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    assert_eq!(replayed.state.model, orchestrator::config::DEFAULT_MODEL);
}

#[tokio::test]
async fn resume_marks_pre_compact_discovered_tools_loaded() {
    // P2-10 (parity 2.1.208): a transcript whose compact boundary carries
    // `preCompactDiscoveredTools` re-marks those tools loaded on the session's
    // DeferralState at cold resume (claude's `Age()` boundary scan), so a tool
    // the model had loaded via ToolSearch before the compaction stays
    // NON-deferred across `--resume`.
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
    use tool_api::registry::ToolRegistry;
    use tool_api::{DeferralState, ToolSearchMode};

    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
    tokio::fs::create_dir_all(&subdir).await.unwrap();

    let sid = Uuid::new_v4();
    let (b, s, a) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    // Boundary (chain-reset root) → summary → post-compact assistant tip.
    let boundary = serde_json::to_string(&json!({
        "type": "system", "subtype": "compact_boundary",
        "content": "Conversation compacted", "level": "info",
        "uuid": b.to_string(), "parentUuid": null, "logicalParentUuid": null,
        "sessionId": sid.to_string(), "timestamp": "2026-07-13T10:00:00.000Z",
        "cwd": cwd, "version": "0.6.0", "isSidechain": false, "userType": "external",
        "compactMetadata": {
            "trigger": "auto", "preTokens": 1234, "messagesSummarized": 2,
            "preCompactDiscoveredTools": ["Task", "WebFetch"]
        }
    }))
    .unwrap();
    let summary = serde_json::to_string(&json!({
        "type": "user", "uuid": s.to_string(), "parentUuid": b.to_string(),
        "sessionId": sid.to_string(), "timestamp": "2026-07-13T10:00:01.000Z",
        "cwd": cwd, "version": "0.6.0", "isSidechain": false, "userType": "external",
        "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
        "message": {"role": "user", "content": "Summary:\nS"}
    }))
    .unwrap();
    let assistant = serde_json::to_string(&json!({
        "type": "assistant", "uuid": a.to_string(), "parentUuid": s.to_string(),
        "sessionId": sid.to_string(), "timestamp": "2026-07-13T10:00:02.000Z",
        "cwd": cwd, "version": "0.6.0", "isSidechain": false, "userType": "external",
        "message": {"role": "assistant", "content": "post-compact reply"}
    }))
    .unwrap();
    tokio::fs::write(
        subdir.join(format!("{sid}.jsonl")),
        format!("{boundary}\n{summary}\n{assistant}\n"),
    )
    .await
    .unwrap();

    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));

    // Tool-Search-enabled registry: the two carried tools start deferred.
    let mut registry = ToolRegistry::new();
    registry.set_deferral(Arc::new(DeferralState::new(ToolSearchMode::Enabled, false)));
    let tools = Arc::new(registry);
    assert!(
        tools.deferral().should_defer("WebFetch", true),
        "deferred before resume"
    );

    let _orch = ConversationOrchestrator::with_resume(
        OrchestratorConfig::default(),
        sid,
        lingxi_home,
        cwd.clone(),
        fs.clone(),
        Arc::new(MockApiClient::new(vec![])),
        tools.clone(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from(&cwd),
        None,
    )
    .await
    .expect("resume ok");

    assert!(
        tools.deferral().is_loaded("WebFetch"),
        "WebFetch re-marked loaded at resume"
    );
    assert!(
        tools.deferral().is_loaded("Task"),
        "Task re-marked loaded at resume"
    );
    assert!(
        !tools.deferral().should_defer("WebFetch", true),
        "loaded ⇒ no longer deferred after resume"
    );
}

#[tokio::test]
async fn replay_propagates_loader_error() {
    let temp = TempDir::new().unwrap();
    let cwd_path = temp.path().join("proj");
    tokio::fs::create_dir(&cwd_path).await.unwrap();
    let cwd = cwd_path.to_string_lossy().into_owned();
    let lingxi_home = temp.path().join("home");
    let sid = Uuid::new_v4();
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let res = replay_session_state(&lingxi_home, &cwd, sid, fs).await;
    assert!(matches!(res, Err(ResumeError::Loader(_))));
}

#[test]
fn resume_merges_per_block_assistant_rows_sharing_one_inner_message_id() {
    // Regression: write-side per-block persistence
    // (`ConversationOrchestrator::persist_assistant_per_block`) splits one
    // assistant turn `[thinking, tool_use]` into two single-block "assistant"
    // JSONL rows that share one inner `message.id`, with distinct top-level
    // `uuid`s. Before this fix, `build_state_from_jsonl` pushed one
    // `ConversationMessage::Assistant` per ROW instead of per TURN, so the
    // reasoning block and the tool_use block ended up in two separate history
    // turns. On the next DeepSeek-thinking request that split resurfaces as
    // two separate provider-wire messages — the tool_calls message carries no
    // reasoning_content, and DeepSeek 400s: "The reasoning_content in the
    // thinking mode must be passed back to the API."
    let sid = Uuid::new_v4();
    let inner_id = Uuid::new_v4().to_string();
    let messages: Vec<session::jsonl::JsonlMessage> = vec![
        serde_json::from_value(json!({
            "type": "assistant", "uuid": Uuid::new_v4().to_string(), "parentUuid": null,
            "sessionId": sid.to_string(), "timestamp": "2026-08-18T00:00:00.000Z",
            "cwd": "/tmp", "version": "0.12.0", "isSidechain": false,
            "message": {
                "id": inner_id, "role": "assistant", "model": "deepseek-flash",
                "content": [{"type": "thinking", "thinking": "let me think", "signature": null}]
            }
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "type": "assistant", "uuid": Uuid::new_v4().to_string(), "parentUuid": null,
            "sessionId": sid.to_string(), "timestamp": "2026-08-18T00:00:01.000Z",
            "cwd": "/tmp", "version": "0.12.0", "isSidechain": false,
            "message": {
                "id": inner_id, "role": "assistant", "model": "deepseek-flash",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}]
            }
        }))
        .unwrap(),
    ];

    let state = state_from_messages(sid, &messages);
    assert_eq!(
        state.history.len(),
        1,
        "the two per-block rows must merge back into ONE assistant turn"
    );
    match &state.history[0] {
        ConversationMessage::Assistant { content, .. } => {
            assert_eq!(content.len(), 2, "merged turn keeps both blocks, in order");
            assert!(matches!(
                &content[0],
                protocol::ContentBlock::Thinking { thinking, .. } if thinking == "let me think"
            ));
            assert!(matches!(
                &content[1],
                protocol::ContentBlock::ToolUse { .. }
            ));
        }
        other => panic!("expected a merged Assistant turn, got {other:?}"),
    }
}

#[cfg(test)]
mod deferred_tool_resume_tests {
    use super::*;
    use async_trait::async_trait;
    use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
    use hooks::events::{HookEvent, HookEventType};
    use hooks::executor::BuiltinHookHandler;
    use hooks::registry::{HookContext, HookRegistry};
    use hooks::response::{HookOutcome, HookResponse, HookResult};
    use hooks::HookExecutorImpl;
    use orchestrator::test_support::{
        MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
    };
    use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
    use protocol::{ContentBlock, HookId};
    use serde_json::Value;
    use session::jsonl::JsonlWriter;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::registry::ToolRegistry;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
        ValidationError,
    };

    struct CountingEchoTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingEchoTool {
        fn name(&self) -> &str {
            "Echo"
        }

        fn input_schema(&self) -> &Value {
            static SCHEMA: once_cell::sync::Lazy<Value> =
                once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
            &SCHEMA
        }

        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }

        fn max_result_size_chars(&self) -> usize {
            1024 * 1024
        }

        fn is_concurrency_safe(&self, _input: &Value) -> bool {
            true
        }

        fn is_read_only(&self, _input: &Value) -> bool {
            true
        }

        async fn validate_input(
            &self,
            _input: &Value,
            _ctx: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn check_permissions(
            &self,
            _input: &Value,
            _ctx: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }

        async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
            "echo".into()
        }

        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }

        async fn call(
            &self,
            _input: Value,
            _ctx: ToolUseContext,
            _tx: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolCallResult {
                data: json!({ "out": "resumed" }),
                model_content: Some("resumed".into()),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    struct CaptureTraceHook {
        seen: Arc<Mutex<Vec<Option<telemetry::otel::SerializedTraceContext>>>>,
    }

    #[async_trait]
    impl BuiltinHookHandler for CaptureTraceHook {
        async fn handle(&self, _event: &HookEvent, ctx: &HookContext) -> HookResult {
            self.seen.lock().unwrap().push(ctx.trace_context.clone());
            HookResult {
                outcome: HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                response: Some(HookResponse::default()),
            }
        }

        fn id(&self) -> &str {
            "capture-trace"
        }
    }

    struct UnusedHttp;
    #[async_trait]
    impl platform_api::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }

        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
    }

    struct UnusedRuntime;
    #[async_trait]
    impl platform_api::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<platform_api::BackgroundTaskHandle, platform_api::RuntimeError> {
            Err(platform_api::RuntimeError::Internal("unused".into()))
        }

        async fn sleep(&self, _d: std::time::Duration) {}

        async fn cancel(
            &self,
            _h: &platform_api::BackgroundTaskHandle,
        ) -> Result<(), platform_api::RuntimeError> {
            Ok(())
        }
    }

    fn tracing_pre_hook_executor(
        seen: Arc<Mutex<Vec<Option<telemetry::otel::SerializedTraceContext>>>>,
    ) -> Arc<HookExecutorImpl> {
        let hook = HookDefinition {
            id: HookId::new(),
            name: "capture-trace".into(),
            events: vec![HookEventType::PreToolUse],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "capture-trace".into(),
            },
            source: HookSource::Session,
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let mut exec = HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        );
        exec.register_builtin(Arc::new(CaptureTraceHook { seen }));
        Arc::new(exec)
    }

    async fn write_deferred_transcript(
        root: &TempDir,
        traceparent: &str,
    ) -> (PathBuf, String, Uuid, Arc<dyn FileSystem>) {
        let cwd_path = root.path().join("proj");
        tokio::fs::create_dir(&cwd_path).await.unwrap();
        let cwd = cwd_path.to_string_lossy().into_owned();
        let lingxi_home = root.path().join("home");
        let subdir = lingxi_home.join("projects").join(project_dir_name(&cwd));
        tokio::fs::create_dir_all(&subdir).await.unwrap();

        let sid = Uuid::new_v4();
        let user_uuid = Uuid::new_v4();
        let assistant_uuid = Uuid::new_v4();
        let attachment_uuid = Uuid::new_v4();
        let tool_use_id = "toolu_resume_deferred";
        let body = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&json!({
                "type":"user",
                "uuid":user_uuid.to_string(),
                "parentUuid":null,
                "sessionId":sid.to_string(),
                "timestamp":"2026-08-23T01:00:00.000Z",
                "cwd":cwd,
                "version":"0.12.0",
                "isSidechain":false,
                "userType":"external",
                "message":{"role":"user","content":"resume this"}
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "type":"assistant",
                "uuid":assistant_uuid.to_string(),
                "parentUuid":user_uuid.to_string(),
                "sessionId":sid.to_string(),
                "timestamp":"2026-08-23T01:00:01.000Z",
                "cwd":cwd,
                "version":"0.12.0",
                "isSidechain":false,
                "userType":"external",
                "message":{
                    "role":"assistant",
                    "content":[{"type":"tool_use","id":tool_use_id,"name":"Echo","input":{}}]
                }
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "type":"attachment",
                "uuid":attachment_uuid.to_string(),
                "parentUuid":assistant_uuid.to_string(),
                "sessionId":sid.to_string(),
                "timestamp":"2026-08-23T01:00:02.000Z",
                "cwd":cwd,
                "version":"0.12.0",
                "isSidechain":false,
                "attachment":{
                    "type":"hook_deferred_tool",
                    "toolUseID":tool_use_id,
                    "toolName":"Echo",
                    "toolInput":{},
                    "hookName":"PreToolUse:Echo",
                    "hookEvent":"PreToolUse",
                    "permissionMode":"default",
                    "traceparent":traceparent
                }
            }))
            .unwrap(),
        );
        let transcript_path = subdir.join(format!("{sid}.jsonl"));
        tokio::fs::write(&transcript_path, body).await.unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(root.path().to_path_buf()));
        (lingxi_home, cwd, sid, fs)
    }

    fn tool_result_count(history: &[ConversationMessage]) -> usize {
        history
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::User { content, .. } => Some(content),
                _ => None,
            })
            .flat_map(|content| content.iter())
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .count()
    }

    #[tokio::test]
    async fn cold_resume_replays_unresolved_deferred_tool_once_and_restores_trace_context() {
        let temp = TempDir::new().unwrap();
        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (lingxi_home, cwd, sid, fs) = write_deferred_transcript(&temp, traceparent).await;
        let transcript_path = session::jsonl::session_path(&lingxi_home, &cwd, &sid.to_string());
        let writer = Arc::new(JsonlWriter::new(transcript_path.clone(), fs.clone()));

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CountingEchoTool {
            calls: calls.clone(),
        }) as Arc<dyn Tool>);
        let tools = Arc::new(registry);
        let hooks = tracing_pre_hook_executor(seen.clone());

        let orch = ConversationOrchestrator::with_resume(
            OrchestratorConfig::default(),
            sid,
            lingxi_home.clone(),
            cwd.clone(),
            fs.clone(),
            Arc::new(MockApiClient::new(vec![])),
            tools.clone(),
            hooks.clone(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from(&cwd),
            Some(writer),
        )
        .await
        .expect("resume ok");

        assert_eq!(calls.load(Ordering::SeqCst), 1, "first resume replays once");
        let captured = seen.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "one PreToolUse replay");
        let trace = captured[0].clone().expect("trace context restored");
        assert_eq!(trace.traceparent, traceparent);
        assert!(trace.tracestate.is_none());
        assert_eq!(
            tool_result_count(&orch.session().lock().await.history),
            1,
            "replay appends exactly one tool_result"
        );

        let raw = tokio::fs::read_to_string(&transcript_path)
            .await
            .expect("read transcript");
        assert_eq!(
            raw.matches(r#""tool_use_id":"toolu_resume_deferred""#)
                .count(),
            1,
            "one persisted tool_result line after first resume"
        );

        let _orch2 = ConversationOrchestrator::with_resume(
            OrchestratorConfig::default(),
            sid,
            lingxi_home,
            cwd.clone(),
            fs,
            Arc::new(MockApiClient::new(vec![])),
            tools,
            hooks,
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from(&cwd),
            None,
        )
        .await
        .expect("second resume ok");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "resolved deferred tool must not replay twice"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no second PreToolUse replay after the tool_result exists"
        );
    }
}

#[tokio::test]
async fn scheduled_actual_settings_do_not_replace_persisted_human_defaults() {
    for with_human_turn in [true, false] {
        let temp = TempDir::new().unwrap();
        let cwd_path = temp.path().join("project");
        tokio::fs::create_dir(&cwd_path).await.unwrap();
        let cwd = cwd_path.to_string_lossy().into_owned();
        let home = temp.path().join("home");
        let subdir = home.join("projects").join(project_dir_name(&cwd));
        tokio::fs::create_dir_all(&subdir).await.unwrap();
        let sid = Uuid::new_v4();
        let mut parent = None;
        let mut rows = Vec::new();
        let turns = if with_human_turn {
            vec![
                (false, "claude-sonnet-4-6", "low"),
                (true, "claude-opus-4-6", "high"),
            ]
        } else {
            vec![(true, "claude-opus-4-6", "high")]
        };
        for (scheduled, model, level) in turns {
            for role in ["user", "assistant"] {
                let id = Uuid::new_v4().to_string();
                let mut row = json!({"type":role,"uuid":id,"parentUuid":parent,"sessionId":sid.to_string(),"cwd":cwd,"timestamp":"2026-09-12T12:00:00.000Z","version":"0.6.0","isSidechain":false,"userType":"external","message":{"role":role,"content":"retained text"}});
                if role == "assistant" {
                    row["message"]["model"] = json!(model);
                    row["modelProfile"] = json!("anthropic");
                    row["effort"] = json!(level);
                    row["reasoningSelection"] = json!({"type":"level","id":level});
                    if scheduled {
                        row["perTurnSettings"] = json!(true);
                    }
                }
                rows.push(row.to_string());
                parent = Some(id);
            }
        }
        tokio::fs::write(subdir.join(format!("{sid}.jsonl")), rows.join("\n"))
            .await
            .unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
        let replayed = replay_session_state(&home, &cwd, sid, fs).await.unwrap();
        let snapshot = replayed.handle_runtime_snapshot();
        if with_human_turn {
            assert_eq!(snapshot.model, "claude-sonnet-4-6");
            assert_eq!(snapshot.model_profile.as_deref(), Some("anthropic"));
            assert_eq!(snapshot.effort.as_deref(), Some("low"));
            assert_eq!(
                snapshot.reasoning_selection,
                Some(platform_api::ReasoningSelection::Level { id: "low".into() })
            );
        } else {
            assert!(
                snapshot.model.is_empty(),
                "scheduled-only history must preserve the boot default"
            );
            assert!(snapshot.reasoning_selection.is_none());
            assert!(snapshot.effort.is_none());
        }
        let actual = replayed
            .messages
            .iter()
            .rev()
            .find(|row| row.message_type == "assistant")
            .unwrap();
        assert_eq!(actual.message["model"], "claude-opus-4-6");
        assert_eq!(actual.extra["effort"], "high");
        assert_eq!(actual.extra["perTurnSettings"], true);
        assert_eq!(
            replayed.state.history.len(),
            if with_human_turn { 4 } else { 2 }
        );
    }
}

/// REGRESSION: a resumed transcript dropped every tool's STRUCTURED result, so
/// after a restart the desktop could no longer tell which call had spawned
/// which subagent and stacked all of their cards at the bottom of the
/// transcript.
///
/// `ContentBlock::ToolResult` can only hold the model-facing TEXT, which is
/// why the turn loop stamps the raw structured result on the persisted
/// `tool_result` line as `toolUseResult` in the first place. Replay has to hand
/// that back, or the sole structural link from a subagent to its creation site
/// (the Agent tool's `data.agentId`) exists only for the life of one process.
#[tokio::test]
async fn replay_recovers_the_structured_client_state_results_the_transcript_stamped() {
    let (_temp, lingxi_home, cwd, sid, last_uuid, fs) = setup_two_turn_jsonl().await;
    let transcript_path = session::jsonl::session_path(&lingxi_home, &cwd, &sid.to_string());
    let spawn_uuid = Uuid::new_v4();
    let result_uuid = Uuid::new_v4();
    let agent_id = "7a1c9e0e-0000-4000-8000-00000000abcd";
    let lines = format!(
        "{}\n{}\n",
        serde_json::to_string(&json!({
            "type": "assistant",
            "uuid": spawn_uuid.to_string(),
            "parentUuid": last_uuid.to_string(),
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:02.000Z",
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_spawn", "name": "Agent",
                 "input": {"description": "review", "subagent_type": "Explore"}}
            ]}
        }))
        .unwrap(),
        serde_json::to_string(&json!({
            "type": "user",
            "uuid": result_uuid.to_string(),
            "parentUuid": spawn_uuid.to_string(),
            "sessionId": sid.to_string(),
            "timestamp": "2026-05-25T12:00:03.000Z",
            "cwd": cwd,
            "version": "0.12.0",
            "isSidechain": false,
            "userType": "external",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_spawn",
                 "content": "Async agent launched successfully.", "is_error": false}
            ]},
            // The model-facing text above never names the id in a form a client
            // may read; this sibling is the only structured copy.
            "toolUseResult": {"status": "async_launched", "agentId": agent_id}
        }))
        .unwrap(),
    );
    tokio::fs::OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .await
        .unwrap()
        .write_all(lines.as_bytes())
        .await
        .unwrap();

    let replayed = replay_session_state(&lingxi_home, &cwd, sid, fs)
        .await
        .expect("replay ok");
    assert_eq!(
        replayed.client_state_tool_results.get("toolu_spawn"),
        Some(&json!({"status": "async_launched", "agentId": agent_id})),
        "the structured result is keyed by the tool_use_id it answers",
    );
}

/// The map is deliberately NARROWER than "every persisted `toolUseResult`",
/// on both axes, and neither bound is visible from a call site.
///
/// * A tool outside `CLIENT_STATE_TOOLS` stays out. Its `data` is the raw
///   uncapped result — `Read` ships an image's base64 where the model-facing
///   text is the 41-byte "[Image content provided in tool result.]", `Edit`
///   ships `originalFile` — and none of it renders. Carrying all of them
///   measured 2.0x-34.5x growth on real transcripts for a payload that crosses
///   as ONE `SessionResumed` frame. The gate is the paired tool NAME, which the
///   result line does not carry — hence the call fixtures below.
/// * A multi-block line stays out, mirroring the writer: `take_tool_use_result`
///   stamps the field only for a line holding exactly one `tool_result`
///   (`sole_tool_result_id`). Fanning one payload across several blocks would
///   give tool B tool A's `agentId` — a card anchored to the WRONG call.
#[test]
fn client_state_recovery_skips_other_tools_and_multi_block_lines() {
    let line = |content: serde_json::Value, result: serde_json::Value| {
        serde_json::from_value::<session::jsonl::JsonlMessage>(json!({
            "type": "user",
            "uuid": Uuid::new_v4().to_string(),
            "parentUuid": null,
            "sessionId": Uuid::new_v4().to_string(),
            "timestamp": "2026-05-25T12:00:00.000Z",
            "message": {"role": "user", "content": content},
            "toolUseResult": result,
        }))
        .expect("a well-formed jsonl line")
    };
    // The result line names only the id it answers, so the tool NAME has to
    // come from the paired call in an earlier message.
    let call = |id: &str, tool: &str| {
        serde_json::from_value::<session::jsonl::JsonlMessage>(json!({
            "type": "assistant",
            "uuid": Uuid::new_v4().to_string(),
            "parentUuid": null,
            "sessionId": Uuid::new_v4().to_string(),
            "timestamp": "2026-05-25T12:00:00.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": tool, "input": {}}
            ]},
        }))
        .expect("a well-formed jsonl line")
    };
    let block = |id: &str| json!({"type": "tool_result", "tool_use_id": id, "content": "x", "is_error": false});
    let spawn =
        json!({"status": "async_launched", "agentId": "7a1c9e0e-0000-4000-8000-00000000abcd"});

    let plan = json!({"plan": "1. Ship it", "model_content": "User has approved your plan."});
    let messages = vec![
        call("toolu_spawn", "Agent"),
        line(json!([block("toolu_spawn")]), spawn.clone()),
        call("toolu_plan", "ExitPlanMode"),
        line(json!([block("toolu_plan")]), plan.clone()),
        // A Bash payload — structured, large, and not a client-state tool.
        call("toolu_bash", "Bash"),
        line(
            json!([block("toolu_bash")]),
            json!({"stdout": "...", "exit_code": 0}),
        ),
        // Two results on one line: the writer would not have stamped this.
        call("toolu_a", "Agent"),
        call("toolu_b", "Agent"),
        line(json!([block("toolu_a"), block("toolu_b")]), spawn.clone()),
    ];

    let recovered = orchestrator::client_state_tool_results_from_messages(&messages);
    assert_eq!(
        recovered,
        std::collections::HashMap::from([
            ("toolu_spawn".to_string(), spawn),
            ("toolu_plan".to_string(), plan),
        ]),
        "only single-block lines answering a client-state tool are recovered",
    );
}

#[tokio::test]
async fn resume_restores_latest_api_usage_from_disk_without_summing_split_rows() {
    let (_temp, home, cwd, sid, _last, fs) = setup_two_turn_jsonl().await;
    let path = home
        .join("projects")
        .join(project_dir_name(&cwd))
        .join(format!("{sid}.jsonl"));
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut rows: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    rows[1]["message"]["model"] = json!("claude-sonnet-4-5");
    rows[1]["message"]["usage"] = json!({"input_tokens":1200,"output_tokens":80,"cache_read_input_tokens":3000,"cache_creation_input_tokens":500});
    std::fs::write(
        &path,
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let replayed = replay_session_state(&home, &cwd, sid, fs).await.unwrap();
    let usage = replayed.handle_runtime_snapshot().current_usage.unwrap();
    assert_eq!(
        usage,
        platform_api::CurrentUsageSnapshot {
            input_tokens: 1200,
            output_tokens: 80,
            cache_read_input_tokens: 3000,
            cache_creation_input_tokens: 500,
        }
    );
    let mut messages = replayed.messages;
    messages.push(messages.last().unwrap().clone());
    assert_eq!(
        runtime_metadata_from_messages(&messages).current_usage,
        Some(usage)
    );
    let mut synthetic = messages.last().unwrap().clone();
    synthetic.message["model"] = json!("<synthetic>");
    synthetic.message["usage"] = json!({"input_tokens":0,"output_tokens":0});
    messages.push(synthetic);
    assert_eq!(
        runtime_metadata_from_messages(&messages).current_usage,
        Some(usage)
    );
    let mut sentinel = messages.last().unwrap().clone();
    sentinel.message["model"] = json!("claude-sonnet-4-5");
    sentinel.message["content"] = json!([{"type":"text","text":"[Request interrupted by user]"}]);
    messages.push(sentinel);
    assert_eq!(
        runtime_metadata_from_messages(&messages).current_usage,
        Some(usage)
    );
    let mut boundary = messages.last().unwrap().clone();
    boundary.message_type = "system".into();
    boundary
        .extra
        .insert("subtype".into(), json!("compact_boundary"));
    messages.push(boundary);
    assert_eq!(
        runtime_metadata_from_messages(&messages).current_usage,
        None
    );
    let mut fresh = messages[1].clone();
    fresh.message["usage"] = json!({"input_tokens":0,"output_tokens":0});
    messages.push(fresh);
    assert_eq!(
        runtime_metadata_from_messages(&messages).current_usage,
        Some(Default::default())
    );
    messages.last_mut().unwrap().message["usage"] = json!({"input_tokens":50,"output_tokens":2});
    messages.last_mut().unwrap().message["usage"]["cache_read_input_tokens"] =
        serde_json::Value::Null;
    messages.last_mut().unwrap().message["usage"]["cache_creation_input_tokens"] =
        serde_json::Value::Null;
    assert_eq!(
        runtime_metadata_from_messages(&messages).current_usage,
        Some(platform_api::CurrentUsageSnapshot {
            input_tokens: 50,
            output_tokens: 2,
            ..Default::default()
        })
    );
}
