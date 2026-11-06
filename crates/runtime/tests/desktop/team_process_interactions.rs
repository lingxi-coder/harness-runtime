//! Major 3 reference and broker contracts. No legacy interaction decoder remains.
use harness_runtime::desktop::team_process::{
    TeamChatQuery, TeamChatReadRequest, TeamChatReadResult, TeamDataRef,
};
use serde_json::json;
use sha2::{Digest, Sha256};
#[test]
fn data_references_bind_full_bytes_and_refuse_links_or_cross_root_reads() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let bytes = serde_json::to_vec(&json!({"full": "x".repeat(450*1024)})).unwrap();
    let path = root.join("batch.json");
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let reference = TeamDataRef {
        path: path.clone(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes: bytes.len() as u64,
    };
    let value: serde_json::Value = reference.read(&root).unwrap();
    assert_eq!(value["full"].as_str().unwrap().len(), 450 * 1024);
    let outside = tempfile::tempdir().unwrap();
    assert!(reference.read::<serde_json::Value>(outside.path()).is_err());
    let link = root.join("linked.json");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let linked = TeamDataRef {
        path: link,
        ..reference.clone()
    };
    assert!(linked.read::<serde_json::Value>(&root).is_err());
    std::fs::write(path, b"tampered").unwrap();
    assert!(reference.read::<serde_json::Value>(&root).is_err());
}
#[test]
fn history_contract_refuses_cross_chat_authority_and_incomplete_ranges() {
    for invalid in [
        json!({"kind":"before","before_seq":3,"limit":200,"conversationId":"other"}),
        json!({"kind":"startBot"}),
        json!({"kind":"range","from_seq_exclusive":0,"to_seq_inclusive":201}),
    ] {
        match serde_json::from_value::<TeamChatQuery>(invalid) {
            Err(_) => {}
            Ok(query) => assert!(TeamChatReadRequest {
                id: "query-1".into(),
                query
            }
            .validate()
            .is_err()),
        }
    }
    let result = TeamChatReadResult {
        messages: vec![],
        complete: false,
        next_before_seq: None,
    };
    assert!(result
        .validate(&TeamChatQuery::Range {
            from_seq_exclusive: 0,
            to_seq_inclusive: 2
        })
        .is_err());
}
#[test]
fn old_claim_or_requester_identity_is_not_the_new_chat_scope() {
    assert!(serde_json::from_value::<harness_runtime::desktop::session_state::BotChatScope>(json!({"teamId":"team_0001","sharedBotId":"sharedbot_01","conversationId":"chat_0001","sessionEpoch":1,"originHumanUid":"alice"})).is_err());
}
#[test]
fn history_queries_cannot_advance_beyond_the_current_task_cutoff() {
    assert!(TeamChatQuery::Before {
        before_seq: 51,
        limit: 20
    }
    .validate_upper_bound(50)
    .is_ok());
    assert!(TeamChatQuery::Before {
        before_seq: 52,
        limit: 20
    }
    .validate_upper_bound(50)
    .is_err());
    assert!(TeamChatQuery::Range {
        from_seq_exclusive: 40,
        to_seq_inclusive: 51
    }
    .validate_upper_bound(50)
    .is_err());
}
