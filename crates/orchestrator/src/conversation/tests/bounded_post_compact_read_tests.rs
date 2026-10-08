use super::super::compaction_impl::read_restored_text_prefix;

async fn read_fixture(raw: &[u8], max_bytes: usize) -> (String, bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read.txt");
    std::fs::write(&path, raw).unwrap();
    read_restored_text_prefix(&path, max_bytes).await.unwrap()
}

#[tokio::test]
async fn post_compact_reader_bounds_bytes_and_detects_exact_fit() {
    let raw = vec![b'x'; 64 * 1024];
    let (content, truncated) = read_fixture(&raw, 1_023).await;
    assert_eq!(content, "x".repeat(1_023));
    assert!(truncated);
    assert_eq!(read_fixture(b"exact", 5).await, ("exact".into(), false));
}

#[tokio::test]
async fn post_compact_reader_lossily_decodes_invalid_and_split_utf8() {
    assert_eq!(
        read_fixture(b"a\xffz", 3).await,
        ("a\u{fffd}z".into(), false)
    );
    assert_eq!(
        read_fixture("aéz".as_bytes(), 2).await,
        ("a\u{fffd}".into(), true)
    );
}

#[tokio::test]
async fn post_compact_reader_normalizes_bom_crlf_and_final_cr_only() {
    let raw = "\u{feff}first\r\nsecond\rinterior\r".as_bytes();
    assert_eq!(
        read_fixture(raw, raw.len()).await,
        ("first\nsecond\rinterior".into(), false)
    );
}

#[tokio::test]
async fn post_compact_reader_preserves_unicode_for_native_token_budgeting() {
    let raw = format!("{}🦀", "界".repeat(20_000));
    let (content, truncated) = read_fixture(raw.as_bytes(), raw.len()).await;
    assert_eq!(content, raw);
    assert_eq!(content.encode_utf16().count(), 20_002);
    assert!(
        !truncated,
        "the producer applies its token budget after the bounded read"
    );
}
