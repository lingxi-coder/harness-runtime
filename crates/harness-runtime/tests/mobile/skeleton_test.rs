//! F3-06 — WALKING SKELETON: prove `submit()` + listener round-trip from a
//! host unit test.
//!
//! The mobile (`UniFFI`) sibling of the bridge-server walking skeleton (F2-05):
//! it proves the in-process FFI path WITHOUT a device. A host-only fake
//! `Platform` shim (`harness_runtime::mobile::test_support`) lets [`build_mobile_engine`]
//! run on CI; a host-fake [`ClientEventListener`] stands in for a Swift/Kotlin
//! listener; a scripted [`MockStreamingApiClient`] stands in for the network so
//! the streaming turn is deterministic.
//!
//! This is exactly the spec §8 "prove from a Swift/Kotlin unit test" smoke test,
//! runnable on the host: `submit(SendPrompt)` against a stubbed streaming client
//! drives the SAME `client-adapter` lowering pipeline both transports share, and
//! the registered listener receives `TextDelta`, `MessageComplete`, then
//! `TurnEnded` — the same message/turn boundaries shipped through the SAME
//! `client-protocol` DTOs.
//!
//! Requires `--features uniffi` (the FFI surface lives behind that feature).

#![cfg(feature = "mobile")]

use std::sync::Arc;
use std::time::Duration;

use client_protocol::commands::ClientCommand;
use client_protocol::events::{ClientEvent, TurnOutcomeDto};
use harness_runtime::mobile::test_support::{
    new_engine_with_streaming, CollectingPermissionSink, FakeListener, HostFakePlatform,
    MobileConfig,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::StreamingApiClient;

/// Submit `NewSession`, tolerating the transient mid-turn rejection.
///
/// `TurnEnded` reaching the listener does NOT mean the engine has cleared its
/// in-flight slot: `NewSession` is gated on `active_cancel`, which the turn task
/// drops as it unwinds — strictly AFTER the terminal event is emitted. So the
/// event these tests spin on is not the precondition they actually need, and
/// under load the gap between the two widens past the old bounded spin. Retry
/// against the real precondition instead of guessing a duration.
async fn start_new_session(handle: &std::sync::Arc<harness_runtime::mobile::MobileEngineHandle>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match handle
            .submit(ClientCommand::NewSession {
                cwd: None,
                model: None,
            })
            .await
        {
            Ok(()) => return,
            Err(error) => {
                let mid_turn = format!("{error:?}").contains("turn is in flight");
                assert!(
                    mid_turn && std::time::Instant::now() < deadline,
                    "NewSession never succeeded: {error:?}"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

/// F3-06: `submit(SendPrompt)` drives the streaming turn on the handle-owned
/// runtime and the registered listener receives `TextDelta`, `MessageComplete`,
/// then `TurnEnded`.
///
/// The walking skeleton: connect (build the engine) → register a listener →
/// `submit(SendPrompt)` against a stubbed `StreamingApiClient` → assert the
/// completed message lands between the streamed delta and terminal turn.
#[test]
fn submit_send_prompt_drives_listener_text_then_turn_ended() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // (1) A scripted streaming client: one assistant text block then `end_turn`.
    //     This stands in for the network so the turn is deterministic off-device.
    let scripted = vec![
        message_start("msg_skeleton", "claude-sonnet-4-20250514"),
        content_block_start_text(0),
        text_delta(0, "hello from the skeleton"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming: Arc<dyn StreamingApiClient> =
        Arc::new(MockStreamingApiClient::with_turns(vec![scripted]));

    // (2) Build the real engine host off-device: host-fake `Platform`, a
    //     recording listener (the Swift/Kotlin stand-in), the scripted stream.
    let platform = Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let perm_sink = Arc::new(CollectingPermissionSink::default());
    let cfg = MobileConfig {
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(".lingxi"),
        ..MobileConfig::default()
    };
    let handle =
        new_engine_with_streaming(cfg, platform, listener.clone(), perm_sink, Some(streaming))
            .expect("build engine with scripted streaming");

    // (3) `submit(SendPrompt)` — spawns the turn on the handle-owned runtime and
    //     returns promptly; the streamed events arrive on the listener.
    handle
        .runtime()
        .block_on(async {
            handle
                .submit(ClientCommand::SendPrompt {
                    text: "drive the skeleton".into(),
                    prompt_mode: None,
                    images: Vec::new(),
                    turn_id: Some(1),
                })
                .await
        })
        .expect("submit(SendPrompt) ok");

    // (4) The turn was SPAWNED — wait for the terminal `TurnEnded` to land on the
    //     listener (bounded spin, like the F3-05 gate tests). On a host fake the
    //     turn completes near-instantly but not synchronously inside `submit`.
    let events = handle.runtime().block_on(async {
        for _ in 0..2000 {
            let saw_turn_ended = listener
                .received
                .lock()
                .await
                .iter()
                .any(|e| matches!(e, ClientEvent::TurnEnded { .. }));
            if saw_turn_ended {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        listener.received.lock().await.clone()
    });

    // (5) Assert the message/turn boundary order.
    let text_idx = events
        .iter()
        .position(|e| matches!(e, ClientEvent::TextDelta { text } if text.contains("hello from the skeleton")))
        .unwrap_or_else(|| panic!("expected a TextDelta carrying the scripted text; got {events:?}"));

    let turn_ended_idx = events
        .iter()
        .position(|e| matches!(e, ClientEvent::TurnEnded { .. }))
        .unwrap_or_else(|| panic!("expected a terminal TurnEnded; got {events:?}"));

    let message_complete_idx = events
        .iter()
        .position(|e| matches!(e, ClientEvent::MessageComplete { .. }))
        .unwrap_or_else(|| panic!("expected MessageComplete; got {events:?}"));

    assert!(
        text_idx < message_complete_idx && message_complete_idx < turn_ended_idx,
        "TextDelta must precede MessageComplete, which must precede TurnEnded; events: {events:?}"
    );

    // The terminal `TurnEnded` carries the `end_turn` outcome (the same lowering
    // the bridge-server e2e asserts).
    match &events[turn_ended_idx] {
        ClientEvent::TurnEnded {
            outcome,
            stop_reason,
            ..
        } => {
            assert_eq!(outcome, &TurnOutcomeDto::EndTurn);
            assert_eq!(stop_reason.as_deref(), Some("end_turn"));
        }
        other => panic!("expected TurnEnded, got {other:?}"),
    }

    // A terminal turn must release the connection's in-flight slot. Session
    // control is the externally-observable contract here: before the fix the
    // stale, non-cancelled token made every post-turn NewSession look mid-turn
    // and therefore fail forever.
    handle.runtime().block_on(async {
        start_new_session(&handle).await;
    });
}

#[test]
fn completed_mobile_turns_are_persisted_and_listed_per_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let scripted_turn = |message_id: &str, text: &str| {
        vec![
            message_start(message_id, "claude-sonnet-4-20250514"),
            content_block_start_text(0),
            text_delta(0, text),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]
    };
    let streaming: Arc<dyn StreamingApiClient> =
        Arc::new(MockStreamingApiClient::with_turns(vec![
            scripted_turn("msg_persist_1", "first persisted reply"),
            scripted_turn("msg_persist_2", "second persisted reply"),
        ]));
    let platform = Arc::new(HostFakePlatform::new(tmp.path().to_path_buf()));
    let listener = Arc::new(FakeListener::default());
    let cfg = MobileConfig {
        cwd: tmp.path().to_path_buf(),
        lingxi_home: tmp.path().join(".lingxi"),
        ..MobileConfig::default()
    };
    let handle = new_engine_with_streaming(
        cfg,
        platform,
        listener.clone(),
        Arc::new(CollectingPermissionSink::default()),
        Some(streaming),
    )
    .expect("build engine with scripted streaming");

    handle.runtime().block_on(async {
        let mut session_ids = Vec::new();
        for (index, prompt) in ["first persisted prompt", "second persisted prompt"]
            .into_iter()
            .enumerate()
        {
            start_new_session(&handle).await;
            let session_id = listener
                .received
                .lock()
                .await
                .iter()
                .rev()
                .find_map(|event| match event {
                    ClientEvent::SessionStarted { session_id, .. } => Some(session_id.clone()),
                    _ => None,
                })
                .expect("SessionStarted carries the durable id");
            session_ids.push(session_id);

            handle
                .submit(ClientCommand::SendPrompt {
                    text: prompt.into(),
                    prompt_mode: None,
                    images: Vec::new(),
                    turn_id: Some(index as u64 + 1),
                })
                .await
                .expect("send prompt");
            for _ in 0..2000 {
                let completed = listener
                    .received
                    .lock()
                    .await
                    .iter()
                    .filter(|event| matches!(event, ClientEvent::TurnEnded { .. }))
                    .count();
                if completed > index {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }

        handle
            .submit(ClientCommand::ListSessions { limit: None })
            .await
            .expect("list persisted sessions");
        let listed = listener
            .received
            .lock()
            .await
            .iter()
            .rev()
            .find_map(|event| match event {
                ClientEvent::SessionList { sessions } => Some(sessions.clone()),
                _ => None,
            })
            .expect("SessionList event");
        let listed_ids: std::collections::HashSet<_> =
            listed.iter().map(|row| row.uuid.as_str()).collect();

        assert_eq!(session_ids.len(), 2);
        assert_ne!(session_ids[0], session_ids[1]);
        assert_eq!(listed.len(), 2, "each completed session must be listed");
        assert!(session_ids
            .iter()
            .all(|id| listed_ids.contains(id.as_str())));
        assert!(listed.iter().all(|row| row.message_count >= 2));
    });
}
