//! Model-free lifecycle coverage through the public tools and real temporary
//! files. These are runtime integration checks, not native CLI byte comparisons.

use std::path::Path;
use std::sync::Arc;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
use tempfile::TempDir;
use tool_api::test_support::{
    ctx_for_file_tools, fresh_ctx, fresh_ctx_cancelled, fresh_tx, make_dummy_fs,
};
use tool_api::tool_trait::{Tool, ToolCallResult, ToolError};
use tool_api::BuiltinToolContext;
use tool_file::{FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool, NotebookEditTool};

fn context(root: &Path) -> BuiltinToolContext {
    ctx_for_file_tools(
        make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![root.to_path_buf()],
    )
}

fn input(value: Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).unwrap()
}

async fn call(tool: &dyn Tool, value: Value) -> ToolCallResult {
    tool.call(input(value), fresh_ctx(), fresh_tx())
        .await
        .unwrap_or_else(|error| panic!("{} failed: {error:?}", tool.name()))
}

async fn six_tool_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(tmp.path());
    assert_ne!(ctx.cwd(), std::env::current_dir().unwrap());
    let text = tmp.path().join("doc.txt");

    call(
        &FileWriteTool::new(ctx.clone()),
        json!({"file_path": "doc.txt", "content": "marker-0\n"}),
    )
    .await;
    // Recreate handles at each step: knowledge belongs to the context, rather
    // than an accidental field retained by one Read/Edit object.
    for version in 1..=16 {
        call(
            &FileReadTool::new(ctx.clone()),
            json!({"file_path": "doc.txt"}),
        )
        .await;
        let before = format!("marker-{}", version - 1);
        let after = format!("marker-{version}");
        call(
            &FileEditTool::new(ctx.clone()),
            json!({"file_path": "doc.txt", "old_string": before, "new_string": after}),
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(&text).unwrap(),
            format!("{after}\n")
        );
        let key = tool_file::normalize_model_file_path("doc.txt", &ctx.cwd());
        let entry = tool_api::read_file_state::get(&ctx.read_file_state, &key).unwrap();
        assert_eq!(entry.content, format!("{after}\n"));
        assert!(
            !entry.from_read,
            "the snapshot must represent the completed edit"
        );
    }

    let glob = call(&GlobTool::new(ctx.clone()), json!({"pattern": "*.txt"})).await;
    assert_eq!(
        glob.data
            .at_pointer("/numFiles")
            .and_then(|v| v.as_number()),
        Some(1.0)
    );
    assert!(glob
        .data
        .string_at("/filenames/0")
        .unwrap()
        .ends_with("doc.txt"));
    let grep = call(
        &GrepTool::new(ctx.clone()),
        json!({"pattern": "marker-16", "path": "doc.txt", "output_mode": "content"}),
    )
    .await;
    assert!(grep
        .data
        .string_at("/content")
        .unwrap()
        .contains("marker-16"));

    let notebook = json!({
        "cells": [{"cell_type": "code", "id": "c1", "execution_count": null,
                   "metadata": {}, "outputs": [], "source": ["print('before')\n"]}],
        "metadata": {"language_info": {"name": "python"}},
        "nbformat": 4, "nbformat_minor": 5
    });
    call(
        &FileWriteTool::new(ctx.clone()),
        json!({"file_path": "notes.ipynb", "content": notebook.to_string()}),
    )
    .await;
    call(
        &FileReadTool::new(ctx.clone()),
        json!({"file_path": "notes.ipynb"}),
    )
    .await;
    call(
        &NotebookEditTool::new(ctx.clone()),
        json!({"notebook_path": "notes.ipynb", "cell_id": "c1", "new_source": "print('after')"}),
    )
    .await;
    let updated: Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("notes.ipynb")).unwrap()).unwrap();
    assert_eq!(updated["cells"][0]["source"], json!("print('after')"));
}

#[tokio::test(flavor = "current_thread")]
async fn six_tools_share_session_state_on_current_thread_localset() {
    tokio::task::LocalSet::new()
        .run_until(six_tool_lifecycle())
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn six_tools_share_session_state_on_multithread_localset() {
    tokio::task::LocalSet::new()
        .run_until(six_tool_lifecycle())
        .await;
}

#[tokio::test]
async fn a_new_context_requires_its_own_observed_file_snapshot() {
    let tmp = TempDir::new().unwrap();
    let first = context(tmp.path());
    call(
        &FileWriteTool::new(first),
        json!({"file_path": "doc.txt", "content": "observed by first context\n"}),
    )
    .await;
    let second = context(tmp.path());
    let change = json!({"file_path": "doc.txt", "old_string": "first", "new_string": "second"});
    let error = FileEditTool::new(second.clone())
        .call(input(change.clone()), fresh_ctx(), fresh_tx())
        .await
        .expect_err("a new session may not inherit another session's file knowledge");
    assert!(matches!(error, ToolError::InvalidInput(message)
        if message.string_at("") == Some(tool_file::FILE_NOT_READ_ERROR)));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("doc.txt")).unwrap(),
        "observed by first context\n"
    );
    call(
        &FileReadTool::new(second.clone()),
        json!({"file_path": "doc.txt"}),
    )
    .await;
    call(&FileEditTool::new(second), change).await;
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("doc.txt")).unwrap(),
        "observed by second context\n"
    );
}

#[tokio::test]
async fn cancelled_mutations_preserve_disk_bytes_and_observed_state() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(tmp.path());
    std::fs::write(tmp.path().join("doc.txt"), "before\n").unwrap();
    std::fs::write(tmp.path().join("notes.ipynb"), r#"{"cells":[{"cell_type":"code","id":"c1","execution_count":null,"metadata":{},"outputs":[],"source":["before"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#).unwrap();
    for name in ["doc.txt", "notes.ipynb"] {
        call(&FileReadTool::new(ctx.clone()), json!({"file_path": name})).await;
    }
    let cases: Vec<(Box<dyn Tool>, Value, &str)> = vec![
        (
            Box::new(FileEditTool::new(ctx.clone())),
            json!({"file_path":"doc.txt","old_string":"before","new_string":"after"}),
            "doc.txt",
        ),
        (
            Box::new(FileWriteTool::new(ctx.clone())),
            json!({"file_path":"doc.txt","content":"after"}),
            "doc.txt",
        ),
        (
            Box::new(NotebookEditTool::new(ctx.clone())),
            json!({"notebook_path":"notes.ipynb","cell_id":"c1","new_source":"after"}),
            "notes.ipynb",
        ),
    ];
    for (tool, value, name) in cases {
        let path = tmp.path().join(name);
        let bytes = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let key = tool_file::normalize_model_file_path(name, &ctx.cwd());
        let state = tool_api::read_file_state::get(&ctx.read_file_state, &key);
        let result = tool
            .call(input(value), fresh_ctx_cancelled(), fresh_tx())
            .await;
        assert!(
            matches!(result, Err(ToolError::Aborted)),
            "{} must observe cancellation before mutation",
            tool.name()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{} modified cancelled bytes",
            tool.name()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(
            tool_api::read_file_state::get(&ctx.read_file_state, &key),
            state
        );
    }
    let result = FileWriteTool::new(ctx)
        .call(
            input(json!({"file_path":"new/parents/doc.txt","content":"after"})),
            fresh_ctx_cancelled(),
            fresh_tx(),
        )
        .await;
    assert!(matches!(result, Err(ToolError::Aborted)));
    assert!(
        !tmp.path().join("new").exists(),
        "cancelled creation must not materialize parent directories"
    );
}

// These assertions run only on a real Windows target. Cross-checking Node's
// win32 paths on macOS would not exercise Rust's Windows Path components.
#[cfg(windows)]
#[test]
fn windows_drive_and_unc_roots_survive_lexical_normalization() {
    use std::path::PathBuf;
    let cwd = Path::new(r"C:\workspace\nested");
    for (raw, expected) in [
        (r"C:\workspace\one\..\two", r"C:\workspace\two"),
        (r"C:\..\..\file.txt", r"C:\file.txt"),
        (r"..\doc.txt", r"C:\workspace\doc.txt"),
        (r"\rooted\doc.txt", r"C:\rooted\doc.txt"),
        (
            r"\\server\share\one\..\file.txt",
            r"\\server\share\file.txt",
        ),
        (r"\\server\share\..\..\file.txt", r"\\server\share\file.txt"),
    ] {
        let actual = tool_file::normalize_model_file_path(raw, cwd);
        assert_eq!(actual, PathBuf::from(expected), "{raw}");
        assert!(actual.is_absolute(), "a drive/UNC root must remain rooted");
    }
}
