//! Exercise exact JavaScript text through dispatched filesystem tools and disk I/O.

use std::{path::Path, sync::Arc};

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::json;
use tempfile::TempDir;
use tool_api::{
    test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs},
    tool_trait::{Tool, ToolCallResult},
    BuiltinToolContext,
};
use tool_file::{
    file_meta::{self, Encoding, LineEnding},
    FileEditTool, FileReadTool, FileWriteTool,
};

fn context(tmp: &TempDir) -> BuiltinToolContext {
    ctx_for_file_tools(
        make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![tmp.path().to_path_buf()],
    )
}

fn arguments(path: &Path, text: &[(&str, &[u16])]) -> Utf16JsonProjection {
    let mut fields = serde_json::Map::new();
    fields.insert("file_path".into(), json!(path));
    for (key, value) in text {
        fields.insert((*key).into(), json!(String::from_utf16_lossy(value)));
    }
    let mut projection =
        Utf16JsonProjection::parse(&serde_json::Value::Object(fields).to_string()).unwrap();
    for (key, value) in text {
        projection
            .set_string_units(&format!("/{key}"), value)
            .unwrap();
    }
    projection
}

async fn read(tool: &FileReadTool, path: &Path) -> ToolCallResult {
    tool.call(arguments(path, &[]), fresh_ctx(), fresh_tx())
        .await
        .unwrap()
}

fn write_utf16(path: &Path, units: &[u16]) {
    std::fs::write(
        path,
        file_meta::encode_units_with_metadata(units, Encoding::Utf16Le, LineEnding::Lf),
    )
    .unwrap();
}

fn includes(haystack: &[u16], needle: &[u16]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}

#[tokio::test]
async fn read_edit_write_preserves_invalid_surrogates_bom_endings_and_result_sources() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("malformed.txt");
    let original = [0xfeff, 0xd800, 97, 0xdc00, 13, 10, 0xe000, 0xfffd, 0xd801];
    write_utf16(&path, &original);
    let ctx = context(&tmp);
    let state = ctx.read_file_state.clone();
    let reader = FileReadTool::new(ctx.clone());
    let editor = FileEditTool::new(ctx.clone());
    let writer = FileWriteTool::new(ctx);
    let read_result = read(&reader, &path).await;
    let normalized = [0xd800, 97, 0xdc00, 10, 0xe000, 0xfffd, 0xd801];
    assert_eq!(
        read_result.data.string_units("/file/content"),
        Some(normalized.to_vec())
    );
    let old = [0xd800];
    let new = [0xdc01, 98];
    let edited = editor
        .call(
            arguments(&path, &[("old_string", &old), ("new_string", &new)]),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    let expected = [
        0xfeff, 0xdc01, 98, 97, 0xdc00, 13, 10, 0xe000, 0xfffd, 0xd801,
    ];
    assert_eq!(
        file_meta::decode_native_text_units(&std::fs::read(&path).unwrap()),
        expected
    );
    assert_eq!(edited.data.string_units("/oldString"), Some(old.to_vec()));
    assert_eq!(edited.data.string_units("/newString"), Some(new.to_vec()));
    assert_eq!(
        edited.data.string_units("/originalFile"),
        Some([0xfeff].into_iter().chain(normalized).collect())
    );
    let parsed = editor.parse_output(&edited.data).await.unwrap();
    assert_eq!(parsed.string_units("/oldString"), Some(old.to_vec()));
    let patch_lines = (0..edited.data.array_len("/structuredPatch/0/lines").unwrap())
        .map(|index| {
            edited
                .data
                .string_units(&format!("/structuredPatch/0/lines/{index}"))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(patch_lines
        .iter()
        .any(|line| line.first() == Some(&45) && line.contains(&0xd800)));
    assert!(patch_lines
        .iter()
        .any(|line| line.first() == Some(&43) && line.contains(&0xdc01)));

    // Post-edit exact state must permit a subsequent whole-file Write.
    let replacement = [0xd802, 120, 0xdc02, 10];
    let written = writer
        .call(
            arguments(&path, &[("content", &replacement)]),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    let final_units = [0xfeff, 0xd802, 120, 0xdc02, 10];
    assert_eq!(
        file_meta::decode_native_text_units(&std::fs::read(&path).unwrap()),
        final_units
    );
    assert_eq!(
        written.data.string_units("/content"),
        Some(final_units.to_vec())
    );
    let reread = read(&reader, &path).await;
    assert_eq!(
        reread.data.string_units("/file/content"),
        Some(replacement.to_vec())
    );
    assert_eq!(
        tool_api::read_file_state::get_content_units(&state, &path),
        Some(replacement.to_vec())
    );
}

#[tokio::test]
async fn surrogate_escape_swaps_keep_native_literal_and_escape_representations() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("escape.txt");
    write_utf16(&path, &[0xfeff, 0xd800]);
    let ctx = context(&tmp);
    let reader = FileReadTool::new(ctx.clone());
    let editor = FileEditTool::new(ctx);
    read(&reader, &path).await;
    let old: Vec<u16> = "\\ud800".encode_utf16().collect();
    let new: Vec<u16> = "\\udc00".encode_utf16().collect();
    let result = editor
        .call(
            arguments(&path, &[("old_string", &old), ("new_string", &new)]),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        file_meta::decode_native_text_units(&std::fs::read(&path).unwrap()),
        [0xfeff, 0xdc00]
    );
    assert_eq!(result.data.string_units("/oldString"), Some(old));
    assert_eq!(result.data.string_units("/newString"), Some(new));

    write_utf16(&path, &"\u{feff}\\uD800".encode_utf16().collect::<Vec<_>>());
    read(&reader, &path).await;
    editor
        .call(
            arguments(
                &path,
                &[("old_string", &[0xd800]), ("new_string", &[0xdc00])],
            ),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(
        file_meta::decode_native_text_units(&std::fs::read(&path).unwrap()),
        "\u{feff}\\uDC00".encode_utf16().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn stale_guard_distinguishes_surrogates_that_share_the_same_display_string() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("stale.txt");
    write_utf16(&path, &[0xfeff, 0xd800, 97]);
    let initial_time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&path, initial_time).unwrap();
    let ctx = context(&tmp);
    let reader = FileReadTool::new(ctx.clone());
    let writer = FileWriteTool::new(ctx.clone());
    let editor = FileEditTool::new(ctx);
    read(&reader, &path).await;
    write_utf16(&path, &[0xfeff, 0xd801, 97]);
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_700_000_010, 0)).unwrap();
    let changed = std::fs::read(&path).unwrap();
    let error = writer
        .call(
            arguments(&path, &[("content", &[98])]),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.model_facing_message().string_at(""),
        Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
    );
    assert_eq!(std::fs::read(&path).unwrap(), changed);
    let error = editor
        .call(
            arguments(&path, &[("old_string", &[97]), ("new_string", &[98])]),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.model_facing_message().string_at(""),
        Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
    );
    assert_eq!(std::fs::read(&path).unwrap(), changed);
}

#[tokio::test]
async fn missing_surrogate_old_string_is_preserved_in_error_source() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("missing.txt");
    write_utf16(&path, &[0xfeff, 0xd800]);
    let ctx = context(&tmp);
    let reader = FileReadTool::new(ctx.clone());
    let editor = FileEditTool::new(ctx);
    read(&reader, &path).await;
    let before = std::fs::read(&path).unwrap();
    let error = editor
        .call(
            arguments(
                &path,
                &[("old_string", &[0xd801]), ("new_string", &[0xdc00])],
            ),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap_err();
    let message = error.model_facing_message().string_units("").unwrap();
    assert!(includes(&message, &[0xd801]));
    assert!(!message.contains(&0xfffd));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn notebook_edits_select_exact_surrogate_cell_ids_and_preserve_error_sources() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("ids.ipynb");
    let original = r#"{"cells":[{"cell_type":"code","id":"\ud800","source":"first","metadata":{},"outputs":[],"execution_count":null},{"cell_type":"code","id":"\ud801","source":"second","metadata":{},"outputs":[],"execution_count":null}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;
    std::fs::write(&path, original).unwrap();
    let ctx = context(&tmp);
    let reader = FileReadTool::new(ctx.clone());
    let editor = tool_file::NotebookEditTool::new(ctx);
    read(&reader, &path).await;
    let mut input = Utf16JsonProjection::parse(
        &json!({"notebook_path":path,"cell_id":"display","new_source":"display"}).to_string(),
    )
    .unwrap();
    input.set_string_units("/cell_id", &[0xd801]).unwrap();
    input.set_string_units("/new_source", &[0xdc02]).unwrap();
    let result = editor
        .call(input.clone(), fresh_ctx(), fresh_tx())
        .await
        .unwrap();
    assert_eq!(result.data.string_units("/cell_id"), Some(vec![0xd801]));
    assert_eq!(result.data.string_units("/new_source"), Some(vec![0xdc02]));
    assert!(result
        .model_content
        .unwrap()
        .string_units("")
        .unwrap()
        .contains(&0xd801));
    let disk = Utf16JsonProjection::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(disk.string_at("/cells/0/source"), Some("first"));
    assert_eq!(disk.string_units("/cells/1/source"), Some(vec![0xdc02]));
    let before = std::fs::read(&path).unwrap();
    input.set_string_units("/cell_id", &[0xdc00]).unwrap();
    let error = editor
        .call(input, fresh_ctx(), fresh_tx())
        .await
        .unwrap_err();
    assert!(error
        .model_facing_message()
        .string_units("")
        .unwrap()
        .contains(&0xdc00));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
