//! `InteractivePromptingGate` ↔ `PermissionGate` upcast integration test.
//!
//! `ConversationOrchestrator` (M5-02 / M5-04) calls
//! `Arc<dyn PermissionGate>::check(name, input)` per tool dispatch. M5-05
//! ships the interactive gate as a `PermissionGate` impl that delegates to
//! the `PromptingGate::prompt_user` round-trip. This test exercises that
//! upcast end-to-end: the gate is invoked via the parent trait method, and
//! we verify the user's scripted "y" / "n" answers map to
//! `PermissionDecision::Allow` / `PermissionDecision::Deny`.

use std::sync::Arc;

use permission::{InteractivePromptingGate, PermissionDecision, PermissionGate};
use serde_json::json;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

#[tokio::test]
async fn check_typing_y_returns_allow() {
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, mut stderr_reader) = duplex(1024);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"y\n").await.unwrap();
        drop(w);
    });

    let decision = gate.check("Bash", &json!({"command": "ls"})).await;

    assert_eq!(decision, PermissionDecision::Allow);

    writer_handle.await.unwrap();

    // The upcast must consult the tool-default table — Bash is DenyByDefault,
    // so the bracket suffix is `[y/N]`.
    let expected = b"Claude needs your permission to use Bash\n[y/N] ";
    let mut printed = vec![0u8; expected.len()];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    assert_eq!(printed.as_slice(), expected.as_slice());
}

#[tokio::test]
async fn check_typing_n_returns_deny() {
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, _stderr_reader) = duplex(1024);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"n\n").await.unwrap();
        drop(w);
    });

    let decision = gate.check("Read", &json!({"file_path": "/tmp/x"})).await;

    match decision {
        PermissionDecision::Deny { reason } => {
            assert!(reason.contains("user typed 'n'"), "got reason: {reason}");
        }
        PermissionDecision::Allow => panic!("expected Deny, got Allow"),
    }

    writer_handle.await.unwrap();
}

#[tokio::test]
async fn check_unknown_tool_falls_back_to_deny_default() {
    // Tool name not in the defaults_per_tool table — `tool_default` returns
    // DenyByDefault. Empty input (bare Enter) takes the default => Deny.
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, mut stderr_reader) = duplex(1024);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"\n").await.unwrap();
        drop(w);
    });

    let decision = gate.check("DoesNotExist", &json!({})).await;
    match decision {
        PermissionDecision::Deny { .. } => {}
        PermissionDecision::Allow => panic!("expected Deny for unknown tool, got Allow"),
    }

    writer_handle.await.unwrap();

    let expected = b"Claude needs your permission to use DoesNotExist\n[y/N] ";
    let mut printed = vec![0u8; expected.len()];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    assert_eq!(printed.as_slice(), expected.as_slice());
}

#[tokio::test]
async fn check_after_3_invalid_inputs_returns_deny() {
    // Upcast must absorb PromptError::InvalidInput into a Deny decision —
    // the orchestrator never sees the error tier.
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, _stderr_reader) = duplex(2048);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"foo\nbar\nbaz\n").await.unwrap();
        drop(w);
    });

    let decision = gate.check("Bash", &json!({})).await;
    match decision {
        PermissionDecision::Deny { reason } => {
            assert!(
                reason.contains("invalid permission input after 3 attempts"),
                "got reason: {reason}"
            );
        }
        PermissionDecision::Allow => panic!("expected Deny after retries, got Allow"),
    }

    writer_handle.await.unwrap();
}
