//! SC-08 — the transcript-rewrite CALL SITE.
//!
//! `session/src/jsonl/transcript_compact.rs` unit-tests the plan builder, the
//! applier and the rewrite driver. This binary tests the only thing those
//! cannot: that something actually CALLS them. The trigger is
//! `JsonlWriter::maybe_compact_transcript`, fired from `JsonlWriter::append`
//! the moment a `compact_boundary` line is persisted — the port's seam for the
//! oracle's `insertMessageChain` arm (cc-238.js @296794903:
//! `this.backstopThresholdBytes=Uyr, this.requestCompact(this.sessionFile,a)`).
//!
//! It lives in its own test binary because it mutates
//! `LINGXI_TRANSCRIPT_LOCAL_GC`, a process-global every `append` reads, and it
//! writes a >5 MiB fixture (the oracle's `I6m` floor is 5 MiB, and testing the
//! wiring below that floor would prove nothing — the driver would return
//! `Skipped` whether or not it was reached).

use lingxi_core::host::FileSystem;
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Map};
use session::jsonl::schema::JsonlMessage;
use session::jsonl::writer::JsonlWriter;
use session::jsonl::MIN_COMPACT_FILE_BYTES;
use session::TRANSCRIPT_LOCAL_GC_ENV;
use std::sync::Arc;
use tempfile::TempDir;

const SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

/// A transcript big enough to cross `I6m`, made of exactly the two things the
/// rewrite exists to reclaim: pre-boundary conversation, and the superseded
/// `custom-title` records the metadata backstop manufactures.
fn seed_transcript(path: &std::path::Path) -> usize {
    let mut buf = String::with_capacity(MIN_COMPACT_FILE_BYTES as usize + 65_536);
    let filler = "x".repeat(160);
    let mut i = 0usize;
    while (buf.len() as u64) < MIN_COMPACT_FILE_BYTES + 65_536 {
        buf.push_str(&format!(
            r#"{{"type":"user","uuid":"msg-{i}","parentUuid":null,"sessionId":"{SESSION_ID}","message":"{filler}"}}
"#
        ));
        buf.push_str(&format!(
            r#"{{"type":"custom-title","customTitle":"title-{i}","sessionId":"{SESSION_ID}"}}
"#
        ));
        i += 1;
    }
    std::fs::write(path, &buf).expect("seed transcript");
    i
}

fn boundary_line() -> JsonlMessage {
    let mut extra = Map::new();
    extra.insert("subtype".into(), json!("compact_boundary"));
    extra.insert("content".into(), json!("Conversation compacted"));
    extra.insert("level".into(), json!("info"));
    extra.insert(
        "compactMetadata".into(),
        json!({"trigger":"auto","preTokens":1000}),
    );
    JsonlMessage {
        message_type: "system".into(),
        uuid: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
        parent_uuid: None,
        session_id: SESSION_ID.into(),
        timestamp: "2026-08-20T15:08:27.000Z".into(),
        cwd: "/tmp/proj".into(),
        version: "0.12.0".into(),
        message: serde_json::Value::Null,
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

fn new_writer(dir: &TempDir) -> (std::path::PathBuf, JsonlWriter) {
    let path = dir.path().join(format!("{SESSION_ID}.jsonl"));
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    (path.clone(), JsonlWriter::new(path, fs))
}

/// One sequential function: both halves mutate the same process-global gate.
#[tokio::test]
async fn writing_a_compact_boundary_reclaims_the_transcript() {
    // ── gate OFF (the default install) ⇒ nothing is reclaimed ───────────────
    {
        std::env::remove_var(TRANSCRIPT_LOCAL_GC_ENV);
        let dir = TempDir::new().expect("tempdir");
        let (path, writer) = new_writer(&dir);
        seed_transcript(&path);
        let before = std::fs::metadata(&path).expect("stat").len();

        writer.append(&boundary_line()).await.expect("append");

        let after = std::fs::metadata(&path).expect("stat").len();
        assert!(
            after > before,
            "with the gate off the transcript only ever grows: {before} -> {after}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("title-0"),
            "the superseded title survives when local GC is off"
        );
    }

    // ── gate ON ⇒ the boundary arms the rewrite, in the same append ─────────
    {
        let dir = TempDir::new().expect("tempdir");
        let (path, writer) = new_writer(&dir);
        let pairs = seed_transcript(&path);
        let before = std::fs::metadata(&path).expect("stat").len();
        assert!(before >= MIN_COMPACT_FILE_BYTES, "fixture must cross I6m");

        std::env::set_var(TRANSCRIPT_LOCAL_GC_ENV, "1");
        let appended = writer.append(&boundary_line()).await;
        std::env::remove_var(TRANSCRIPT_LOCAL_GC_ENV);
        appended.expect("append");

        let after_text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            (after_text.len() as u64) < before / 100,
            "the rewrite must reclaim ~everything before the boundary: \
             {before} -> {}",
            after_text.len()
        );
        assert!(
            after_text.contains(r#""subtype":"compact_boundary""#),
            "the boundary itself survives: {after_text}"
        );
        assert!(
            after_text.contains(&format!("title-{}", pairs - 1)),
            "the surviving custom-title is hoisted to the boundary: {after_text}"
        );
        assert!(
            !after_text.contains("title-0"),
            "every superseded title is gone: {after_text}"
        );
        assert!(
            !after_text.contains(r#""uuid":"msg-0""#),
            "pre-boundary conversation with no preserved segment is reclaimed"
        );

        // The backstop bookkeeping the next rewrite depends on.
        //
        // The rewrite zeroes `bytesSinceCompact` — but it then re-states the
        // surviving metadata at the tail (the oracle's
        // `await this.reAppendSessionMetadataAsync(!1,!0)`, because the rewrite
        // just deleted every superseded metadata record), and THAT append runs
        // through `appendToFile`, which bumps the same counter
        // (`bytesSinceCompact += o`, @296775839). So the honest post-condition
        // is "the transcript's bytes are gone, only the fresh metadata tail is
        // counted" — not a bare zero. Asserting `== 0` here demanded behaviour
        // the oracle does not have.
        let left = writer.bytes_since_compact();
        assert!(
            left < before / 100,
            "the rewrite must zero the counter, leaving only the metadata \
             re-append it performs: {before} -> {left}"
        );
        assert_eq!(
            writer.compact_backstop_bytes(),
            session::COMPACT_BACKSTOP_BYTES,
            "a rewrite that reclaimed >10% resets the threshold to Uyr"
        );

        // No temp file survives a successful publish.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".compact.tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }
}
