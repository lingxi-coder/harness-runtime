//! End-to-end `InteractivePromptingGate::prompt_user` tests using
//! `tokio::io::duplex` to script stdin and capture stderr.
//!
//! Pattern: spawn a writer task that pushes the user's response onto the
//! stdin duplex and closes it. Read stderr with `read_exact` (NOT
//! `read_to_end`) sized to the expected prompt — `read_to_end` would
//! deadlock because the gate holds an `Arc<Mutex<stderr_writer>>` and
//! only drops it at end of test scope, so the reader never sees EOF
//! until after the assertions run.

use std::sync::Arc;

use permission::{
    InteractivePromptingGate, PermissionRequest, PromptDefault, PromptError, PromptingGate,
};
use serde_json::json;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

fn make_request(tool_name: &str, default: PromptDefault) -> PermissionRequest {
    PermissionRequest::ToolUseConfirm {
        tool_name: tool_name.to_string(),
        tool_input: json!({}),
        default_decision: default,
        suppress_always_allow_rule: false,
    }
}

#[tokio::test]
async fn typing_y_returns_allow() {
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, mut stderr_reader) = duplex(1024);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"y\n").await.unwrap();
        // Close so read_line returns even if more bytes were expected.
        drop(w);
    });

    let decision = gate
        .prompt_user(&make_request("Bash", PromptDefault::DenyByDefault))
        .await
        .expect("prompt_user should succeed on `y`");

    assert!(decision.allow, "y should map to allow=true");
    assert!(!decision.persist, "M5-05 always sets persist=false");

    writer_handle.await.unwrap();

    // Read exactly the expected prompt bytes — the gate flushed already
    // and the duplex buffer holds the bytes verbatim.
    let expected = b"Claude needs your permission to use Bash\n[y/N] ";
    let mut printed = vec![0u8; expected.len()];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    assert_eq!(printed.as_slice(), expected.as_slice());
}

#[tokio::test]
async fn empty_input_with_allow_default_returns_allow() {
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

    let decision = gate
        .prompt_user(&make_request("Read", PromptDefault::AllowByDefault))
        .await
        .expect("prompt_user should succeed on empty input with allow default");

    assert!(decision.allow);
    assert!(decision.reason.contains("default = allow"));

    writer_handle.await.unwrap();

    let expected = b"Claude needs your permission to use Read\n[Y/n] ";
    let mut printed = vec![0u8; expected.len()];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    assert_eq!(printed.as_slice(), expected.as_slice());
}

#[tokio::test]
async fn empty_input_with_deny_default_returns_deny() {
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

    let decision = gate
        .prompt_user(&make_request("Bash", PromptDefault::DenyByDefault))
        .await
        .expect("prompt_user should succeed on empty input with deny default");

    assert!(!decision.allow);
    assert!(decision.reason.contains("default = deny"));

    writer_handle.await.unwrap();

    let expected = b"Claude needs your permission to use Bash\n[y/N] ";
    let mut printed = vec![0u8; expected.len()];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    assert_eq!(printed.as_slice(), expected.as_slice());
}

#[tokio::test]
async fn three_invalid_inputs_returns_invalid_input_error() {
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, mut stderr_reader) = duplex(2048);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"foo\nbar\nbaz\n").await.unwrap();
        drop(w);
    });

    let err = gate
        .prompt_user(&make_request("Bash", PromptDefault::DenyByDefault))
        .await
        .expect_err("prompt_user should error after 3 invalid inputs");

    match err {
        PromptError::InvalidInput { attempts } => {
            assert_eq!(attempts, 3);
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }

    writer_handle.await.unwrap();

    // The gate writes the same prompt exactly 3 times before erroring.
    let one = b"Claude needs your permission to use Bash\n[y/N] ";
    let mut printed = vec![0u8; one.len() * 3];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    let occurrences = printed.windows(one.len()).filter(|w| *w == one).count();
    assert_eq!(
        occurrences, 3,
        "prompt must render exactly 3 times before erroring"
    );
}

#[tokio::test]
async fn second_attempt_valid_recovers() {
    let (stdin_writer, stdin_reader) = duplex(1024);
    let (stderr_writer, mut stderr_reader) = duplex(2048);

    let gate = InteractivePromptingGate::new(
        Arc::new(Mutex::new(BufReader::new(stdin_reader))),
        Arc::new(Mutex::new(stderr_writer)),
    );

    let writer_handle = tokio::spawn(async move {
        let mut w = stdin_writer;
        w.write_all(b"hmm\ny\n").await.unwrap();
        drop(w);
    });

    let decision = gate
        .prompt_user(&make_request("Read", PromptDefault::AllowByDefault))
        .await
        .expect("second-attempt valid input should succeed");
    assert!(decision.allow);

    writer_handle.await.unwrap();

    let one = b"Claude needs your permission to use Read\n[Y/n] ";
    let mut printed = vec![0u8; one.len() * 2];
    stderr_reader.read_exact(&mut printed).await.unwrap();
    let occurrences = printed.windows(one.len()).filter(|w| *w == one).count();
    assert_eq!(
        occurrences, 2,
        "prompt must render twice — once invalid, once recovered"
    );
}
