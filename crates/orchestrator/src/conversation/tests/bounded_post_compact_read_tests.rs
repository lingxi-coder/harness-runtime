use super::*;

#[tokio::test]
async fn post_compact_reader_never_loads_past_its_byte_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("large.txt");
    std::fs::write(&path, "x".repeat(64 * 1024)).expect("write fixture");

    let read = read_utf8_prefix(&path, 1_023, 1_023)
        .await
        .expect("bounded read");

    assert_eq!(read.content.len(), 1_023);
    assert!(read.content.bytes().all(|byte| byte == b'x'));
    assert!(read.truncated);
}

#[tokio::test]
async fn post_compact_reader_drops_only_a_split_utf8_tail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("utf8.txt");
    std::fs::write(&path, "aéz").expect("write fixture");

    let read = read_utf8_prefix(&path, 2, 2).await.expect("bounded read");

    assert_eq!(read.content, "a");
    assert!(read.truncated);
}

#[tokio::test]
async fn post_compact_reader_discards_utf8_padding_after_the_character_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("padding.txt");
    std::fs::write(&path, "x".repeat(60_003)).expect("write fixture");

    let read = read_utf8_prefix(&path, 60_003, 20_000)
        .await
        .expect("bounded read");

    assert_eq!(read.content.len(), 20_000);
    assert_eq!(read.content.chars().count(), 20_000);
    assert!(read.truncated);
}

#[tokio::test]
async fn post_compact_reader_handles_a_four_byte_scalar_split_in_the_padding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("padding-utf8.txt");
    std::fs::write(&path, format!("{}🦀", "x".repeat(20_000))).expect("write fixture");

    let read = read_utf8_prefix(&path, 60_003, 20_000)
        .await
        .expect("bounded read");

    assert_eq!(read.content, "x".repeat(20_000));
    assert!(read.truncated);
}

#[tokio::test]
async fn post_compact_reader_distinguishes_an_exact_fit_from_truncation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("exact.txt");
    std::fs::write(&path, "x".repeat(20_000)).expect("write fixture");

    let read = read_utf8_prefix(&path, 60_003, 20_000)
        .await
        .expect("bounded read");

    assert_eq!(read.content.len(), 20_000);
    assert!(!read.truncated);
}

#[tokio::test]
async fn post_compact_reader_counts_utf16_units_for_cjk_and_astral_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unicode.txt");
    let content = format!("{}🦀", "界".repeat(20_000));
    std::fs::write(&path, &content).expect("write fixture");

    let read = read_utf8_prefix(
        &path,
        compaction::thresholds::POST_COMPACT_MAX_BYTES_PER_FILE_READ,
        compaction::thresholds::POST_COMPACT_MAX_CHARS_PER_FILE_READ,
    )
    .await
    .expect("bounded read");

    assert_eq!(read.content, "界".repeat(20_000));
    assert!(
        read.truncated,
        "the trailing astral scalar exceeds the unit cap"
    );
    assert_eq!(read.content.encode_utf16().count(), 20_000);
}
