use super::*;
use lingxi_core::host::TaskDeliveryHistoryEnvelope;
use lingxi_core::types::SessionId;
use serde_json::json;
use tool_api::read_file_state::{get, get_content_units, new_read_file_state_map};

const FILE: &str = "/workspace/file.txt";

fn source(value: serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).unwrap()
}

fn text(units: &[u16]) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec()).unwrap()
}

fn call(session: &mut SessionState, name: &str, input: Utf16JsonProjection) -> ToolUseId {
    let id = ToolUseId::new();
    session.history.push(ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::ToolUse {
            id: id.clone(),
            name: name.into(),
            input,
            provider_id: None,
        }],
        stop_reason: None,
        resumable: None,
        per_turn_effort: None,
    });
    id
}

fn result(
    session: &mut SessionState,
    tool_use_id: ToolUseId,
    data: Option<Utf16JsonProjection>,
    error: bool,
) -> MessageId {
    let id = MessageId::new();
    let mut message = ConversationMessage::user(id, String::new());
    if let ConversationMessage::User { content, .. } = &mut message {
        content.push(ContentBlock::ToolResult {
            tool_use_id,
            content: "tool result".into(),
            is_error: Some(error),
            provider_tool_use_id: None,
            content_blocks: None,
        });
    }
    session.history.push(message);
    if let Some(data) = data {
        session
            .native_request_history
            .task_delivery_envelopes
            .insert(
                id,
                TaskDeliveryHistoryEnvelope {
                    tool_use_result: Some(data),
                    ..Default::default()
                },
            );
    }
    id
}

fn read_data(path: &str, units: &[u16], start: u64, total: u64) -> Utf16JsonProjection {
    let mut data = source(json!({"type":"text","file":{
        "filePath":path,"content":"","startLine":start,
        "numLines":units.split_inclusive(|unit| *unit == 10).count(),"totalLines":total
    }}));
    data.set_string_units("/file/content", units).unwrap();
    data
}

fn native_read_data(path: &str, units: &[u16], start: u64, total: u64) -> Utf16JsonProjection {
    let mut data = source(json!({"type":"text","file":{
        "filePath":path,"content":"","startLine":start,
        "numLines":units.split(|unit| *unit == 10).count(),"totalLines":total
    }}));
    data.set_string_units("/file/content", units).unwrap();
    data
}

fn append_read(session: &mut SessionState, path: &str, units: &[u16]) {
    let id = call(session, "Read", source(json!({"file_path":path})));
    result(
        session,
        id,
        Some(read_data(path, units, 1, total_lines_units(units))),
        false,
    );
}

fn restore(session: &SessionState) -> ReadFileStateMap {
    let map = new_read_file_state_map();
    restore_file_state(session, Path::new("/workspace"), &map, &HashMap::new());
    map
}

fn session() -> SessionState {
    SessionState::empty(SessionId::new(), "test-model".into())
}

#[test]
fn resume_native_read_counts_restore_empty_and_terminal_empty_lines_exactly() {
    for units in [&[][..], &[10][..], &[0xd800, 10][..], &[97, 10, 10][..]] {
        let mut session = session();
        let id = call(&mut session, "Read", source(json!({"file_path":FILE})));
        let total = units.split(|unit| *unit == 10).count() as u64;
        result(
            &mut session,
            id,
            Some(native_read_data(FILE, units, 1, total)),
            false,
        );
        let map = restore(&session);
        assert_eq!(
            get_content_units(&map, Path::new(FILE)),
            Some(units.to_vec())
        );
    }
}

#[test]
fn resume_native_partial_read_requires_exact_prior_view_and_eof_evidence() {
    let before = [0xd800, 10, 0xdc00, 10];
    for (has_prior, start, limit, units, total, expected) in [
        (false, 1, 2, &[0xd800, 10][..], 3, false),
        (false, 1, 1, &[0xd800, 10][..], 2, false),
        (false, 2, 1, &[0xdc00][..], 3, false),
        (true, 2, 1, &[0xdc00][..], 3, true),
        (true, 2, 1, &[0xd800][..], 3, false),
        (true, 3, 1, &[][..], 3, true),
        (true, 3, 1, &[0xdc00][..], 3, false),
        (true, 2, 1, &[0xdc00][..], 4, false),
    ] {
        let mut session = session();
        if has_prior {
            let id = call(&mut session, "Read", source(json!({"file_path":FILE})));
            result(
                &mut session,
                id,
                Some(native_read_data(FILE, &before, 1, 3)),
                false,
            );
        }
        let id = call(
            &mut session,
            "Read",
            source(json!({"file_path":FILE,"offset":start,"limit":limit})),
        );
        result(
            &mut session,
            id,
            Some(native_read_data(FILE, units, start, total)),
            false,
        );
        assert_eq!(
            get_content_units(&restore(&session), Path::new(FILE)),
            expected.then(|| before.to_vec()),
            "prior={has_prior}, start={start}, limit={limit}, units={units:?}, total={total}"
        );
    }
}

#[test]
fn resume_reads_keep_exact_surrogates_and_compare_partial_views_without_display_repair() {
    let before = [97, 0xd800, 10, 0xdc00, 98, 10];
    for matching in [true, false] {
        let mut session = session();
        append_read(&mut session, FILE, &before);
        let id = call(
            &mut session,
            "Read",
            source(json!({"file_path":FILE,"offset":2,"limit":1})),
        );
        let slice = [if matching { 0xdc00 } else { 0xd800 }, 98, 10];
        result(&mut session, id, Some(read_data(FILE, &slice, 2, 3)), false);
        let map = restore(&session);
        if matching {
            assert_eq!(
                get_content_units(&map, Path::new(FILE)),
                Some(before.to_vec())
            );
            assert_eq!(get(&map, Path::new(FILE)).unwrap().mtime_ms, i64::MIN);
        } else {
            assert!(
                get(&map, Path::new(FILE)).is_none(),
                "different surrogates with equal U+FFFD display cannot confirm a prior view"
            );
        }
    }
}

#[test]
fn resume_write_sidecars_and_writerless_inputs_preserve_exact_source_and_bom() {
    let units = [0xfeff, 0xd800, 97, 0xdc00, 10];
    for raw_sidecar in [false, true] {
        let mut session = session();
        append_read(&mut session, FILE, &[0xd801, 10]);
        let mut input = source(json!({"file_path":FILE,"content":"","note":""}));
        input.set_string_units("/content", &units).unwrap();
        input.set_string_units("/note", &[0xdc01]).unwrap();
        let id = call(&mut session, "Write", input);
        let data = raw_sidecar.then(|| {
            let mut data = source(json!({"type":"update","filePath":FILE,"content":""}));
            data.set_string_units("/content", &units).unwrap();
            data
        });
        result(&mut session, id, data, false);
        let map = restore(&session);
        assert_eq!(
            get_content_units(&map, Path::new(FILE)),
            Some(units[1..].to_vec())
        );
        assert!(!get(&map, Path::new(FILE)).unwrap().from_read);
    }
}

#[test]
fn resume_edits_apply_exact_hunks_and_preserve_proven_line_endings() {
    let before = [97, 0xd800, 13, 10, 0xdc00, 98];
    let normalized = [97, 0xd800, 10, 0xdc00, 98];
    let expected = [65, 0xdc00, 13, 10, 0xd800, 98];
    for matching in [true, false] {
        let mut session = session();
        append_read(&mut session, FILE, &before);
        let id = call(&mut session, "Edit", Utf16JsonProjection::parse(r#"{"file_path":"/workspace/file.txt","old_string":"\ud800","new_string":"wrong if input were replayed"}"#).unwrap());
        let mut data = Utf16JsonProjection::parse(
            r#"{
            "filePath":"/workspace/file.txt","originalFile":"",
            "structuredPatch":[{"oldStart":1,"newStart":1,"oldLines":2,"newLines":2,"lines":[
                "-a\ud800","-\udc00b","\\ No newline at end of file",
                "+A\udc00","+\ud800b","\\ No newline at end of file"
            ]}]
        }"#,
        )
        .unwrap();
        data.set_string_units("/originalFile", &normalized).unwrap();
        if !matching {
            data.set_string_units("/structuredPatch/0/lines/0", &[45, 97, 0xdc00])
                .unwrap();
        }
        result(&mut session, id, Some(data), false);
        let map = restore(&session);
        assert_eq!(
            get_content_units(&map, Path::new(FILE)),
            matching.then(|| expected.to_vec())
        );
    }
}

#[test]
fn resume_notebook_edits_retain_complete_source_even_with_literal_surrogates() {
    let mut original = r#"{"cells":[{"cell_type":"code","source":""#
        .encode_utf16()
        .collect::<Vec<_>>();
    original.push(0xd800);
    original.extend(r#"","outputs":[]}]}"#.encode_utf16());
    let mut updated = original.clone();
    *updated.iter_mut().find(|unit| **unit == 0xd800).unwrap() = 0xdc00;
    for valid in [true, false] {
        let mut session = session();
        append_read(&mut session, FILE, &original);
        let mut input = source(json!({"notebook_path":FILE,"new_source":""}));
        input.set_field("new_source", text(&[0xdc00])).unwrap();
        let id = call(&mut session, "NotebookEdit", input);
        let mut data = source(json!({
            "notebook_path":FILE,"new_source":"","cell_type":"code","language":"python",
            "edit_mode":"replace","error":"","original_file":"","updated_file":""
        }));
        data.set_field("new_source", text(&[0xdc00])).unwrap();
        data.set_field("original_file", text(&original)).unwrap();
        data.set_field(
            "updated_file",
            text(if valid { &updated } else { &[0xd800] }),
        )
        .unwrap();
        result(&mut session, id, Some(data), false);
        let map = restore(&session);
        assert_eq!(
            get_content_units(&map, Path::new(FILE)),
            valid.then(|| updated.clone())
        );
    }
}

#[test]
fn resume_file_attachments_retain_exact_source_without_reading_current_disk() {
    let units = [0xd800, 97, 10, 0xdc00];
    let mut session = session();
    let id = MessageId::new();
    let mut attachment = source(
        json!({"type":"file","filename":FILE,"content":{"type":"text","file":{
            "filePath":FILE,"content":"","startLine":1,"numLines":2,"totalLines":2
        }}}),
    );
    attachment
        .set_string_units("/content/file/content", &units)
        .unwrap();
    let message =
        crate::conversation::ConversationOrchestrator::file_attachment_projection(id, &attachment)
            .unwrap();
    session.history.push(message);
    let attachments = HashMap::from([(id, attachment)]);
    let map = new_read_file_state_map();
    restore_file_state(&session, Path::new("/workspace"), &map, &attachments);
    assert_eq!(
        get_content_units(&map, Path::new(FILE)),
        Some(units.to_vec())
    );
}

#[test]
fn resume_invalid_paths_ambiguous_results_and_errors_remain_fail_closed() {
    let units = [0xd800, 10];
    for case in ["duplicate", "malformed", "failed", "path"] {
        let mut session = session();
        append_read(&mut session, FILE, &units);
        let mut input = source(json!({"file_path":FILE,"content":""}));
        input.set_field("content", text(&[0xdc00, 10])).unwrap();
        if case == "path" {
            input
                .set_string_units(
                    "/file_path",
                    &"/workspace/"
                        .encode_utf16()
                        .chain([0xd800])
                        .chain(".txt".encode_utf16())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
        }
        let id = call(&mut session, "Write", input);
        let mut data = source(json!({"type":"update","filePath":FILE,"content":""}));
        data.set_field("content", text(&[0xdc00, 10])).unwrap();
        result(
            &mut session,
            id.clone(),
            Some(if case == "malformed" {
                source(json!(null))
            } else {
                data.clone()
            }),
            case == "failed",
        );
        if case == "duplicate" {
            result(&mut session, id, Some(data), false);
        }
        let map = restore(&session);
        let expected = matches!(case, "failed" | "path").then(|| units.to_vec());
        assert_eq!(get_content_units(&map, Path::new(FILE)), expected, "{case}");
        assert!(
            map.lock()
                .unwrap()
                .keys()
                .iter()
                .all(|path| path == Path::new(FILE)),
            "invalid UTF-16 paths cannot manufacture repaired names"
        );
    }
}

#[tokio::test]
async fn resumed_exact_view_rejects_changed_surrogate_with_identical_display() {
    use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};
    use tool_api::Tool;
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("utf16.txt");
    let before = [97, 0xd800, 10];
    let changed = [97, 0xdc00, 10];
    assert_eq!(
        String::from_utf16_lossy(&before),
        String::from_utf16_lossy(&changed)
    );
    let encode = |units: &[u16]| {
        [0xfeff]
            .into_iter()
            .chain(units.iter().copied())
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    std::fs::write(&path, encode(&changed)).unwrap();
    let mut session = session();
    append_read(&mut session, path.to_str().unwrap(), &before);
    let map = restore(&session);
    let mut context = ctx_for_file_tools(
        make_dummy_fs(),
        std::sync::Arc::new(telemetry::AnalyticsBus::new()),
        vec![root],
    );
    context.read_file_state = map;
    let tool = tool_file::FileWriteTool::new(context);
    let input = source(json!({"file_path":path,"content":"replacement"}));
    let error = tool
        .call(input.clone(), fresh_ctx(), fresh_tx())
        .await
        .unwrap_err();
    let projection = error.model_facing_message();
    assert_eq!(
        projection.string_at("").unwrap(),
        tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR
    );
    assert_eq!(std::fs::read(&path).unwrap(), encode(&changed));
    std::fs::write(&path, encode(&before)).unwrap();
    tool.call(input, fresh_ctx(), fresh_tx()).await.unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        encode(&"replacement".encode_utf16().collect::<Vec<_>>())
    );
}
