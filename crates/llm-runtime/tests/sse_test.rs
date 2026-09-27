use llm_runtime::SseFrameSplitter;

#[test]
fn splits_simple_data_events() {
    let mut splitter = SseFrameSplitter::default();

    let frames = splitter
        .push(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n")
        .unwrap();

    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], b"{\"a\":1}");
    assert_eq!(frames[1], b"{\"b\":2}");
}

#[test]
fn buffers_events_across_chunk_boundaries() {
    let mut splitter = SseFrameSplitter::default();

    assert!(splitter.push(b"data: {\"a\"").unwrap().is_empty());
    assert!(splitter.push(b":1}\n").unwrap().is_empty());
    let frames = splitter.push(b"\ndata: x").unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], b"{\"a\":1}");
    let trailing = splitter
        .finish()
        .unwrap()
        .expect("trailing unterminated event");
    assert_eq!(trailing, b"x");
}

#[test]
fn joins_multiple_data_lines_with_newline() {
    let mut splitter = SseFrameSplitter::default();

    let frames = splitter.push(b"data: line1\ndata: line2\n\n").unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], b"line1\nline2");
}

#[test]
fn handles_crlf_delimiters() {
    let mut splitter = SseFrameSplitter::default();

    let frames = splitter
        .push(b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n\r\n")
        .unwrap();

    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], b"{\"a\":1}");
    assert_eq!(frames[1], b"{\"b\":2}");
}

#[test]
fn ignores_comments_and_non_data_fields() {
    let mut splitter = SseFrameSplitter::default();

    let frames = splitter
        .push(b": keep-alive\nevent: content_block_delta\nid: 7\nretry: 100\ndata: payload\n\n")
        .unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], b"payload");
}

#[test]
fn event_without_data_produces_no_frame() {
    let mut splitter = SseFrameSplitter::default();

    assert!(splitter.push(b"event: ping\n\n").unwrap().is_empty());
    assert!(splitter.finish().unwrap().is_none());
}

#[test]
fn passes_done_sentinel_through() {
    let mut splitter = SseFrameSplitter::default();

    let frames = splitter.push(b"data: [DONE]\n\n").unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], b"[DONE]");
}

#[test]
fn data_without_space_after_colon_is_kept() {
    let mut splitter = SseFrameSplitter::default();

    let frames = splitter.push(b"data:{\"a\":1}\n\n").unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], b"{\"a\":1}");
}
