//! Correlated queued-turn query wire and native binding contract.

use client::protocol::commands::ClientCommand;
use client::protocol::events::{ClientEvent, QueuedTurnStatusDto, TurnRecoveryStateDto};

#[test]
fn query_preserves_all_three_correlators_and_requires_them() {
    let command = ClientCommand::QueryQueuedTurn {
        session_id: "session-a".into(),
        turn_id: 11,
        request_id: "request-a".into(),
    };
    let json = serde_json::to_string(&command).unwrap();
    assert_eq!(
        json,
        r#"{"type":"query_queued_turn","session_id":"session-a","turn_id":11,"request_id":"request-a"}"#
    );
    assert_eq!(
        serde_json::from_str::<ClientCommand>(&json).unwrap(),
        command
    );
    for raw in [
        r#"{"type":"query_queued_turn","turn_id":11,"request_id":"request-a"}"#,
        r#"{"type":"query_queued_turn","session_id":"session-a","request_id":"request-a"}"#,
        r#"{"type":"query_queued_turn","session_id":"session-a","turn_id":11}"#,
    ] {
        assert!(serde_json::from_str::<ClientCommand>(raw).is_err(), "{raw}");
    }
}

#[test]
fn every_disposition_round_trips_without_inventing_a_parent_turn() {
    let statuses = [
        QueuedTurnStatusDto::Pending,
        QueuedTurnStatusDto::ClaimedUncertain,
        QueuedTurnStatusDto::Delivered {
            batch_owner_turn_id: 11,
            member_turn_ids: vec![11, 12],
            transcript_uuid: "row-12".into(),
        },
        QueuedTurnStatusDto::Cancelled,
        QueuedTurnStatusDto::Running,
        QueuedTurnStatusDto::Terminal {
            state: TurnRecoveryStateDto::Failed,
        },
    ];
    for status in statuses {
        let event = ClientEvent::QueuedTurnStatus {
            session_id: "session-a".into(),
            turn_id: 12,
            request_id: "request-a".into(),
            status,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<ClientEvent>(&json).unwrap(), event);
    }
    let delivered = serde_json::to_value(QueuedTurnStatusDto::Delivered {
        batch_owner_turn_id: 11,
        member_turn_ids: vec![11, 12],
        transcript_uuid: "row-12".into(),
    })
    .unwrap();
    assert_eq!(
        delivered,
        serde_json::json!({
            "type": "delivered", "batch_owner_turn_id": 11,
            "member_turn_ids": [11, 12], "transcript_uuid": "row-12"
        })
    );
    assert_eq!(
        serde_json::to_value(QueuedTurnStatusDto::Terminal {
            state: TurnRecoveryStateDto::Completed
        })
        .unwrap(),
        serde_json::json!({"type": "terminal", "state": {"type": "completed"}})
    );
    for raw in [
        r#"{"type":"delivered","batch_owner_turn_id":11,"member_turn_ids":[11]}"#,
        r#"{"type":"delivered","batch_owner_turn_id":11,"transcript_uuid":"row"}"#,
        r#"{"type":"terminal"}"#,
        r#"{"type":"future_unknown"}"#,
    ] {
        assert!(
            serde_json::from_str::<QueuedTurnStatusDto>(raw).is_err(),
            "{raw}"
        );
    }
}

#[cfg(feature = "uniffi")]
#[test]
fn native_metadata_appends_query_event_and_all_payloads() {
    use client::protocol::commands::UNIFFI_META_CLIENT_ENUM_CLIENTCOMMAND;
    use client::protocol::events::{
        UNIFFI_META_CLIENT_ENUM_CLIENTEVENT, UNIFFI_META_CLIENT_ENUM_QUEUEDTURNSTATUSDTO,
    };
    use uniffi_meta::{Metadata, Type};

    let Metadata::Enum(commands) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_CLIENTCOMMAND).unwrap()
    else {
        panic!("command metadata");
    };
    let query = commands.variants.last().unwrap();
    assert_eq!(query.name, "QueryQueuedTurn");
    assert_eq!(
        query
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["session_id", "turn_id", "request_id"]
    );
    assert!(matches!(&query.fields[0].ty, Type::String));
    assert!(matches!(&query.fields[1].ty, Type::UInt64));
    assert!(matches!(&query.fields[2].ty, Type::String));

    let Metadata::Enum(events) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_CLIENTEVENT).unwrap()
    else {
        panic!("event metadata");
    };
    let event = events.variants.last().unwrap();
    assert_eq!(event.name, "QueuedTurnStatus");
    assert_eq!(
        event
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["session_id", "turn_id", "request_id", "status"]
    );
    assert!(
        matches!(&event.fields[3].ty, Type::Enum { name, .. } if name == "QueuedTurnStatusDto")
    );

    let Metadata::Enum(statuses) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_QUEUEDTURNSTATUSDTO).unwrap()
    else {
        panic!("status metadata");
    };
    assert_eq!(
        statuses
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Pending",
            "ClaimedUncertain",
            "Delivered",
            "Cancelled",
            "Running",
            "Terminal"
        ]
    );
    let delivered = &statuses.variants[2];
    assert_eq!(
        delivered
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["batch_owner_turn_id", "member_turn_ids", "transcript_uuid"]
    );
    assert!(matches!(&delivered.fields[0].ty, Type::UInt64));
    assert!(
        matches!(&delivered.fields[1].ty, Type::Sequence { inner_type } if matches!(inner_type.as_ref(), Type::UInt64))
    );
    assert!(matches!(&delivered.fields[2].ty, Type::String));
    let terminal = &statuses.variants[5];
    assert_eq!(terminal.fields.len(), 1);
    assert!(
        matches!(&terminal.fields[0].ty, Type::Enum { name, .. } if name == "TurnRecoveryStateDto")
    );
}
