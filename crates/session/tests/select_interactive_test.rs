//! T7 tests — stdio picker behavior using `tokio::io::duplex`.

use session::jsonl::{select_session_interactive, LoaderError, SessionMetadata};
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

fn rows(n: usize) -> Vec<SessionMetadata> {
    (0..n)
        .map(|i| SessionMetadata {
            uuid: Uuid::from_bytes([u8::try_from(i + 1).unwrap_or(0); 16]),
            title: format!("title-{i}"),
            modified: UNIX_EPOCH
                + Duration::from_secs(1_700_000_000 + u64::try_from(i).unwrap_or(0)),
            created: UNIX_EPOCH
                + Duration::from_secs(1_700_000_000 + u64::try_from(i).unwrap_or(0)),
            message_count: 3,
            mode: session::jsonl::SessionMode::Code,
            path: PathBuf::from(format!("s{i}.jsonl")),
            pr_number: None,
            custom_or_ai_title: Some(format!("title-{i}")),
            resume_model: None,
            resume_model_profile: None,
        })
        .collect()
}

#[tokio::test]
async fn valid_first_selection_returns_first_uuid() {
    let sessions = rows(3);
    let (mut user_w, server_r) = tokio::io::duplex(64);
    let (server_w, mut user_r) = tokio::io::duplex(1024);
    user_w.write_all(b"1\n").await.unwrap();
    user_w.shutdown().await.unwrap();

    let expected_uuid = sessions[0].uuid;
    let task = {
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let mut stdin = BufReader::new(server_r);
            let mut stdout = server_w;
            select_session_interactive(&sessions, &mut stdin, &mut stdout).await
        })
    };
    let chosen = task.await.unwrap().unwrap();
    assert_eq!(chosen, Some(expected_uuid));

    let mut buf = String::new();
    user_r.read_to_string(&mut buf).await.unwrap();
    assert!(buf.starts_with("Resume which session?\n"), "{buf:?}");
    assert!(buf.contains("  1. title-0 [2023-11-14"), "{buf:?}");
    assert!(buf.contains("> "), "{buf:?}");
}

#[tokio::test]
async fn empty_input_returns_none_cancel() {
    let sessions = rows(2);
    let (mut user_w, server_r) = tokio::io::duplex(64);
    let (server_w, _user_r) = tokio::io::duplex(1024);
    user_w.write_all(b"\n").await.unwrap();
    user_w.shutdown().await.unwrap();

    let mut stdin = BufReader::new(server_r);
    let mut stdout = server_w;
    let res = select_session_interactive(&sessions, &mut stdin, &mut stdout).await;
    assert!(matches!(res, Ok(None)));
}

#[tokio::test]
async fn retry_then_valid_succeeds() {
    let sessions = rows(3);
    let (mut user_w, server_r) = tokio::io::duplex(64);
    let (server_w, mut user_r) = tokio::io::duplex(1024);
    // attempt 1 = "9" (out of range) -> retry prompt
    // attempt 2 = "2" (valid)
    user_w.write_all(b"9\n2\n").await.unwrap();
    user_w.shutdown().await.unwrap();

    let expected_uuid = sessions[1].uuid;
    // Drain output concurrently so the picker's writes never block when the
    // duplex buffer would otherwise fill up.
    let drain = tokio::spawn(async move {
        let mut buf = String::new();
        user_r.read_to_string(&mut buf).await.unwrap();
        buf
    });
    let mut stdin = BufReader::new(server_r);
    let mut stdout = server_w;
    let chosen = select_session_interactive(&sessions, &mut stdin, &mut stdout)
        .await
        .unwrap();
    assert_eq!(chosen, Some(expected_uuid));
    drop(stdout); // close the write half so the drain task sees EOF.

    let buf = drain.await.unwrap();
    assert!(
        buf.contains("Please enter a number from 1 to 3, or empty to cancel."),
        "retry feedback missing: {buf:?}"
    );
}

#[tokio::test]
async fn three_invalid_inputs_return_invalid_selection() {
    let sessions = rows(2);
    let (mut user_w, server_r) = tokio::io::duplex(64);
    let (server_w, _user_r) = tokio::io::duplex(1024);
    user_w.write_all(b"abc\nxyz\n99\n").await.unwrap();
    user_w.shutdown().await.unwrap();

    let mut stdin = BufReader::new(server_r);
    let mut stdout = server_w;
    let res = select_session_interactive(&sessions, &mut stdin, &mut stdout).await;
    assert!(matches!(res, Err(LoaderError::InvalidSelection)));
}

#[tokio::test]
async fn empty_sessions_list_returns_empty_directory() {
    let sessions: Vec<SessionMetadata> = vec![];
    let (_user_w, server_r) = tokio::io::duplex(64);
    let (server_w, _user_r) = tokio::io::duplex(1024);
    let mut stdin = BufReader::new(server_r);
    let mut stdout = server_w;
    let res = select_session_interactive(&sessions, &mut stdin, &mut stdout).await;
    assert!(matches!(res, Err(LoaderError::EmptyDirectory)));
}
