//! Integration test for the REAL REPL seam: an `InteractivePromptingGate`
//! wrapped by the default-on `PolicyPermissionGate`, driven over a SHARED
//! `BufReader`.
//!
//! This mirrors the TUI `policy_gate_*` tests but for the stdio gate the
//! `--no-tui`-on-TTY REPL injects:
//!   - a mutating tool (`Write`) with no allow rule reaches the inner gate as
//!     an unresolved `Ask` → the `y/n` prompt; `"y\n"` → Allow, `"n\n"` → Deny.
//!   - a read-only tool (`Read`) auto-allows IN THE POLICY → the inner gate is
//!     never reached, so NO stdin byte is consumed.

use std::sync::Arc;

use permission::{
    InteractivePromptingGate, PermissionDecision, PermissionGate, PermissionMode, PermissionPolicy,
    PolicyPermissionGate,
};
use tokio::io::{duplex, AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Empty-rules, Default-mode policy: read-only tools auto-allow, mutating
/// tools fall through to the injected inner gate as an unresolved `Ask`.
fn default_policy() -> Arc<PermissionPolicy> {
    Arc::new(PermissionPolicy::from_rules(
        PermissionMode::Default,
        Vec::new(),
    ))
}

fn make_gate(shared: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>>) -> PolicyPermissionGate {
    // `tokio::io::sink()` is an always-open `AsyncWrite` — the prompt bytes are
    // discarded but the write never breaks (no dropped reader half to race).
    let inner = Arc::new(InteractivePromptingGate::new(
        shared,
        Arc::new(Mutex::new(tokio::io::sink())),
    ));
    PolicyPermissionGate::new(default_policy(), inner)
}

#[tokio::test]
async fn unresolved_write_ask_with_y_allows() {
    let (mut writer, client) = duplex(1024);
    writer.write_all(b"y\n").await.unwrap();
    drop(writer);
    let shared: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>> =
        Arc::new(Mutex::new(BufReader::new(client)));
    let gate = make_gate(shared);

    let decision = gate
        .check("Write", &serde_json::json!({"file_path": "/tmp/x"}))
        .await;
    assert_eq!(
        decision,
        PermissionDecision::Allow,
        "y on a Write ask → Allow"
    );
}

#[tokio::test]
async fn unresolved_write_ask_with_n_denies() {
    let (mut writer, client) = duplex(1024);
    writer.write_all(b"n\n").await.unwrap();
    drop(writer);
    let shared: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>> =
        Arc::new(Mutex::new(BufReader::new(client)));
    let gate = make_gate(shared);

    match gate
        .check("Write", &serde_json::json!({"file_path": "/tmp/x"}))
        .await
    {
        PermissionDecision::Deny { .. } => {}
        PermissionDecision::Allow => panic!("expected Deny on `n`, got Allow"),
    }
}

#[tokio::test]
async fn read_only_tool_auto_allows_consuming_no_byte() {
    // Queue a byte the gate would consume IF it were (wrongly) reached.
    let (mut writer, client) = duplex(1024);
    writer.write_all(b"y\n").await.unwrap();
    let shared: Arc<Mutex<dyn AsyncBufRead + Send + Unpin>> =
        Arc::new(Mutex::new(BufReader::new(client)));
    let gate = make_gate(shared.clone());

    let decision = gate
        .check("Read", &serde_json::json!({"file_path": "/tmp/x"}))
        .await;
    assert_eq!(
        decision,
        PermissionDecision::Allow,
        "Read auto-allows in the policy"
    );

    // The queued `"y\n"` was NOT consumed — the gate never prompted.
    let mut buf = String::new();
    let mut guard = shared.lock().await;
    let n = guard.read_line(&mut buf).await.unwrap();
    assert_eq!(
        n,
        "y\n".len(),
        "read-only auto-allow must consume NO stdin byte"
    );
    assert_eq!(buf, "y\n");
    // keep `writer` alive until after the read so EOF doesn't race the assert
    drop(writer);
}
