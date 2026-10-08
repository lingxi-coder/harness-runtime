//! Current raw nested-memory replay preserves payloads and exact projection.

use super::*;
use crate::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider, noop_hook_executor,
};
use lingxi_core::host::{OrchestratorHandle, ResumeRuntimeSnapshot};
use platform_posix::fs::PosixFileSystem;
use serde_json::json;

fn native_attachment() -> Value {
    json!({
        "type": "nested_memory",
        "path": "/repo/folder/LINGXI.md",
        "displayPath": "folder/LINGXI.md",
        "content": {
            "path": "/repo/folder/LINGXI.md",
            "type": "Project",
            "content": " \nraw instruction body\r\n\t",
            "globs": ["src/**"],
            "contentDiffersFromDisk": true,
            "rawContent": "---\npaths: src/**\n---\n \nraw instruction body\r\n\t",
            "parent": "/repo/LINGXI.md"
        }
    })
}

fn raw_row(session: Uuid, id: Uuid, attachment: Value) -> JsonlMessage {
    serde_json::from_value(json!({
        "type": "attachment",
        "uuid": id.to_string(),
        "parentUuid": null,
        "sessionId": session.to_string(),
        "timestamp": "2026-10-01T00:00:00.000Z",
        "cwd": "/repo",
        "version": "2.1.287",
        "isSidechain": false,
        "message": null,
        "attachment": attachment
    }))
    .unwrap()
}

#[test]
fn nested_memory_replay_preserves_uuid_full_file_fields_and_untrimmed_rendering() {
    let session = Uuid::new_v4();
    let uuid = Uuid::new_v4();
    let attachment = native_attachment();
    let row = raw_row(session, uuid, attachment.clone());
    let state = state_from_messages(session, std::slice::from_ref(&row));
    let history = state.model_context_history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id(), MessageId::from_uuid(uuid));
    assert_eq!(
        history[0].text_content(),
        "<system-reminder>\nContents of /repo/folder/LINGXI.md:\n\n \nraw instruction body\r\n\t\n</system-reminder>"
    );
    let sidecars = model_reminder_attachments_from_messages(std::slice::from_ref(&row));
    let restored = &sidecars[&MessageId::from_uuid(uuid)];
    assert_eq!(restored, &attachment);
    assert_eq!(
        serde_json::to_vec(restored).unwrap(),
        serde_json::to_vec(&attachment).unwrap()
    );
    assert!(row.message.is_null());
}

#[test]
fn raw_sidecars_keep_opaque_attachment_values_without_synthesizing_projections() {
    let session = Uuid::new_v4();
    let object_uuid = Uuid::new_v4();
    let array_uuid = Uuid::new_v4();
    let null_uuid = Uuid::new_v4();
    let opaque = json!({"type": "opaque-test-attachment", "payload": {"unrecognized": [1, null]}});
    let array = json!(["raw", {"kind": "opaque"}]);
    let mut invalid = raw_row(session, Uuid::new_v4(), native_attachment());
    invalid.uuid = "invalid-current-row-uuid".to_owned();
    let rows = vec![
        raw_row(session, object_uuid, opaque.clone()),
        raw_row(session, array_uuid, array.clone()),
        raw_row(session, null_uuid, Value::Null),
        invalid,
    ];
    let sidecars = model_reminder_attachments_from_messages(&rows);
    assert_eq!(sidecars.len(), 3);
    assert_eq!(sidecars[&MessageId::from_uuid(object_uuid)], opaque);
    assert_eq!(sidecars[&MessageId::from_uuid(array_uuid)], array);
    assert_eq!(sidecars[&MessageId::from_uuid(null_uuid)], Value::Null);
    assert!(hook_attachment_message_for_api(&rows[0], object_uuid).is_none());
    assert!(hook_attachment_message_for_api(&rows[1], array_uuid).is_none());
    assert!(hook_attachment_message_for_api(&rows[2], null_uuid).is_none());
}

#[test]
fn rendered_reminder_prose_does_not_create_a_raw_nested_memory_sidecar() {
    let session = Uuid::new_v4();
    let uuid = Uuid::new_v4();
    let row: JsonlMessage = serde_json::from_value(json!({
        "type": "user", "uuid": uuid.to_string(), "parentUuid": null,
        "sessionId": session.to_string(), "timestamp": "2026-10-01T00:00:00.000Z",
        "cwd": "/repo", "version": "2.1.287", "isSidechain": false,
        "message": {"role": "user", "content": "<system-reminder>\nContents of /repo/folder/LINGXI.md:\n\nraw instruction body\n</system-reminder>"}
    })).unwrap();
    assert!(model_reminder_attachments_from_messages(std::slice::from_ref(&row)).is_empty());
    assert!(hook_attachment_message_for_api(&row, uuid).is_none());
}

#[tokio::test]
async fn persisted_nested_memory_has_identical_hot_and_cold_payload_and_projection() {
    let directory = tempfile::tempdir().unwrap();
    let session_uuid = Uuid::new_v4();
    let session_id = SessionId::from_uuid(session_uuid);
    let id = MessageId::new();
    let path = directory.path().join("nested-memory.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(directory.path().to_owned()));
    let writer = Arc::new(JsonlWriter::new(path.clone(), fs));
    let root = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            directory.path().to_owned(),
        )
        .with_session_id(session_id)
        .with_jsonl_writer(writer),
    );
    root.attach_owned_session_switches();
    let attachment = native_attachment();
    let projection = ConversationOrchestrator::nested_memory_attachment_projection(id, &attachment)
        .expect("current native nested memory projects to one message");
    let hot = root
        .persist_model_reminder(projection.clone(), attachment.clone())
        .await;
    assert_eq!(hot, projection);
    let bytes = tokio::fs::read(&path).await.unwrap();
    let row: JsonlMessage = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(row.message_type, "attachment");
    assert_eq!(row.uuid, id.as_uuid().to_string());
    assert!(row.message.is_null());
    assert_eq!(row.extra["attachment"], attachment);
    let cold = state_from_messages(session_uuid, std::slice::from_ref(&row));
    assert_eq!(cold.model_context_history(), vec![projection.clone()]);
    let raw_sidecars = model_reminder_attachments_from_messages(std::slice::from_ref(&row));
    root.resume_session(
        session_id,
        cold.history,
        Some(row.uuid),
        None,
        ResumeRuntimeSnapshot {
            model_reminder_attachments: raw_sidecars.into_iter().collect(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        root.session.lock().await.model_context_history(),
        vec![projection]
    );
    let restored = root.transcript.model_reminder_attachments.lock().unwrap();
    assert_eq!(restored[&id], attachment);
    assert_eq!(
        serde_json::to_vec(&restored[&id]).unwrap(),
        serde_json::to_vec(&row.extra["attachment"]).unwrap()
    );
}
