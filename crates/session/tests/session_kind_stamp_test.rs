//! SC-07 — the `sessionKind` writer half.
//!
//! Oracle `insertMessageChain` (cc-238.js @296794533) stamps
//! `sessionKind:a3e()` on every chain entry, immediately before `userType`;
//! `a3e()` (@283798463) whitelists `bg` / `daemon` / `daemon-worker` out of
//! `CLAUDE_CODE_SESSION_KIND` and returns `undefined` for anything else.
//!
//! The port has had the READER half since the SESSION.2 gap fix — the
//! `/resume` picker drops `daemon` / `daemon-worker` sessions — with no writer
//! behind it, so the filter could never fire on a LingXi-written transcript.
//!
//! This lives in its own test binary on purpose: it mutates a process-global
//! env var that [`session::jsonl::writer::JsonlWriter::append`] reads on every
//! line, so it must not run beside other `append` tests. Everything is one
//! sequential test function for the same reason.

use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Map};
use session::jsonl::schema::{JsonlMessage, SESSION_KIND_ENV};
use session::jsonl::writer::JsonlWriter;
use std::sync::Arc;
use tempfile::TempDir;

const SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

fn make_msg(uuid: &str, extra: Map<String, serde_json::Value>) -> JsonlMessage {
    JsonlMessage {
        message_type: "user".into(),
        uuid: uuid.into(),
        parent_uuid: None,
        session_id: SESSION_ID.into(),
        timestamp: "2026-05-25T14:30:00.000Z".into(),
        cwd: "/tmp/proj".into(),
        version: "0.6.0".into(),
        message: json!({"role":"user","content":"hi"}),
        is_sidechain: false,
        user_type: Some("external".into()),
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra,
    }
}

/// Append one line under `kind` and return it.
async fn append_one(kind: Option<&str>, extra: Map<String, serde_json::Value>) -> String {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join(format!("{SESSION_ID}.jsonl"));
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = JsonlWriter::new(path.clone(), fs);

    match kind {
        Some(value) => std::env::set_var(SESSION_KIND_ENV, value),
        None => std::env::remove_var(SESSION_KIND_ENV),
    }
    writer
        .append(&make_msg("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", extra))
        .await
        .expect("append");
    std::env::remove_var(SESSION_KIND_ENV);

    std::fs::read_to_string(&path).expect("read back")
}

#[tokio::test]
async fn session_kind_is_stamped_on_every_written_line() {
    // ── the three whitelisted kinds are stamped, in the trailer slot ────────
    for kind in ["bg", "daemon", "daemon-worker"] {
        let line = append_one(Some(kind), Map::default()).await;
        assert!(
            line.contains(&format!(
                r#""sessionKind":"{kind}","userType":"external","cwd":"/tmp/proj""#
            )),
            "`{kind}` must be stamped immediately before userType: {line}"
        );
    }

    // ── `a3e()` is a WHITELIST, not a passthrough ───────────────────────────
    // `LINGXI_SESSION_KIND=interactive` is a value the port's own /stop tests
    // set; upstream it yields `undefined`, i.e. no key at all.
    for kind in ["interactive", "", "BG", "worker"] {
        let line = append_one(Some(kind), Map::default()).await;
        assert!(
            !line.contains("sessionKind"),
            "`{kind}` is not whitelisted, so no key is emitted: {line}"
        );
    }

    // ── unset env: byte-unchanged, no key ───────────────────────────────────
    let line = append_one(None, Map::default()).await;
    assert!(!line.contains("sessionKind"), "{line}");

    // ── a line that already carries one keeps the ORIGINAL value ────────────
    // Round-tripping a foreign transcript must not relabel its lines with this
    // process's kind.
    let mut extra = Map::new();
    extra.insert("sessionKind".into(), json!("daemon"));
    let line = append_one(Some("bg"), extra).await;
    assert!(line.contains(r#""sessionKind":"daemon""#), "{line}");
    assert_eq!(line.matches("sessionKind").count(), 1, "{line}");
}
