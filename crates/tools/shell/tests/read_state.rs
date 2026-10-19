//! Drive real shell commands through Bash and then exercise the file tools'
//! read-before-write guards against the shared snapshot.

#![cfg(unix)]

use async_trait::async_trait;
use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
use lingxi_core::host::sandbox::SandboxedCommand;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use mobile_linux_api::{ProcessError, ProcessOutput};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use tool_api::read_file_state::{get, ReadFileEntry};
use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};
use tool_api::{BuiltinToolContext, Tool};
use tool_file::{FileEditTool, FileReadTool, FileWriteTool};
use tool_shell::BashTool;

fn source(value: serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).expect("valid test input")
}

/// Execute the actual BashTool command, including its cwd readback, without
/// loading the developer's login profile into these isolated tests.
struct RealShell;

#[async_trait]
impl ProcessRunner for RealShell {
    async fn run(&self, command: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        let command = command.inner();
        let mut process = std::process::Command::new("/bin/bash");
        process.args(["--noprofile", "--norc", "-c"]);
        process.arg(command.args.last().expect("BashTool shell command"));
        process.env_remove("BASH_ENV").envs(&command.env);
        if let Some(cwd) = &command.cwd {
            process.current_dir(cwd);
        }
        let output = process
            .output()
            .map_err(|error| ProcessError::Io(error.to_string()))?;
        Ok(ProcessOutput {
            stdout: String::from_utf8(output.stdout).expect("UTF-8 test output"),
            stderr: String::from_utf8(output.stderr).expect("UTF-8 test output"),
            exit_code: output.status.code().unwrap_or(1),
            timed_out: false,
        })
    }

    async fn spawn_background(&self, _: &SandboxedCommand) -> Result<ProcessHandle, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn kill(&self, _: &ProcessHandle) -> Result<(), ProcessError> {
        Err(ProcessError::Unsupported)
    }

    fn is_available(&self) -> bool {
        true
    }
}

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    ctx: BuiltinToolContext,
}

impl Fixture {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("target.txt");
        std::fs::write(&path, "original\n").unwrap();
        // Avoid clock-resolution sleeps: any subsequent real shell write is
        // newer than this fixture's known read snapshot.
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let mut ctx = ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![root],
        );
        ctx.process = Arc::new(RealShell);
        let fixture = Self {
            _dir: dir,
            path,
            ctx,
        };
        fixture.read().await;
        fixture
    }

    async fn read(&self) {
        self.read_with_limit(None).await;
    }

    async fn read_with_limit(&self, limit: Option<u64>) {
        let mut input = json!({ "file_path": self.path });
        if let Some(limit) = limit {
            input["limit"] = json!(limit);
        }
        FileReadTool::new(self.ctx.clone())
            .call(source(input), fresh_ctx(), fresh_tx())
            .await
            .expect("Read succeeds");
    }

    fn entry(&self) -> ReadFileEntry {
        get(&self.ctx.read_file_state, &self.path).expect("read snapshot must remain recorded")
    }

    async fn bash(&self, command: &str) -> tool_api::ToolCallResult {
        BashTool::new(self.ctx.clone())
            .call(
                source(json!({ "command": command })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Bash invocation succeeds")
    }

    async fn edit(
        &self,
        old: &str,
        new: &str,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        FileEditTool::new(self.ctx.clone())
            .call(
                source(json!({ "file_path": self.path, "old_string": old, "new_string": new })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
    }
}

#[tokio::test]
async fn non_writing_shell_syntax_preserves_read_state_and_allows_edit() {
    for command in [
        "echo 'example > target.txt'",
        "false && echo hi > target.txt",
        "printf side | tee side.txt && cat target.txt",
    ] {
        let fixture = Fixture::new().await;
        let before = fixture.entry();
        fixture.bash(command).await;
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::metadata(&fixture.path)
                .unwrap()
                .modified()
                .map(tool_api::read_file_state::mtime_ms_floor)
                .unwrap(),
            before.mtime_ms,
            "non-writing command changed mtime: {command}"
        );
        let after = fixture.entry();
        assert_eq!(after.content, before.content, "{command}");
        assert_eq!(after.mtime_ms, before.mtime_ms, "{command}");
        fixture.edit("original", "edited").await.expect(command);
        assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), "edited\n");
    }
}

#[tokio::test]
async fn actual_shell_writes_retain_stale_snapshot_and_block_edit_and_write_until_read() {
    // Failed commands can still mutate files before failing, and a writer
    // inside a nested shell does not expose its target as a top-level redirect.
    for command in [
        "printf 'shell changed\\n' > target.txt",
        "printf 'shell changed\\n' > target.txt; false",
        "sh -c 'printf \"shell changed\\n\" > \"$1\"' ignored target.txt",
    ] {
        let fixture = Fixture::new().await;
        let before = fixture.entry();
        fixture.bash(command).await;
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            "shell changed\n"
        );
        let after = fixture.entry();
        assert_eq!(
            after.content, before.content,
            "Bash must not refresh the snapshot"
        );
        assert!(
            after.mtime_ms <= before.mtime_ms,
            "Bash must not advance the observed mtime"
        );

        let edit_error = fixture
            .edit("shell changed", "edited")
            .await
            .expect_err("Edit must reject external changes");
        let write_error = FileWriteTool::new(fixture.ctx.clone())
            .call(
                source(json!({ "file_path": fixture.path, "content": "overwritten\n" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("Write must reject external changes");
        for error in [edit_error, write_error] {
            assert_eq!(
                error.model_facing_message().string_at(""),
                Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR),
                "must report stale content, not a missing Read: {command}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            "shell changed\n"
        );
        fixture.read().await;
        fixture
            .edit("shell changed", "edited")
            .await
            .expect("Read refreshes the snapshot");
        assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), "edited\n");
    }
}

#[tokio::test]
async fn same_mtime_shell_writes_are_stale_after_full_and_partial_reads() {
    for limit in [None, Some(1)] {
        for target in ["target.txt", "sub/../target.txt"] {
            let fixture = Fixture::new().await;
            std::fs::create_dir(fixture.path.parent().unwrap().join("sub")).unwrap();
            fixture.read_with_limit(limit).await;
            let before = fixture.entry();
            // Separate commands ensure the modified target is found from this
            // redirect, rather than accidentally from `touch`'s other arguments.
            fixture.bash("touch -r target.txt saved-time").await;
            fixture
                .bash(&format!(
                    "printf 'shell changed\\n' > {target} && touch -r saved-time {target}"
                ))
                .await;
            assert_eq!(
                std::fs::metadata(&fixture.path)
                    .unwrap()
                    .modified()
                    .map(tool_api::read_file_state::mtime_ms_floor)
                    .unwrap(),
                before.mtime_ms,
                "fixture must preserve mtime"
            );
            assert_eq!(fixture.entry().content, before.content);
            let edit_error = fixture
                .edit("shell changed", "edited")
                .await
                .expect_err("preserved mtime must not bypass the Edit guard");
            let write_error = FileWriteTool::new(fixture.ctx.clone())
                .call(
                    source(json!({ "file_path": fixture.path, "content": "overwritten\n" })),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .expect_err("preserved mtime must not bypass the Write guard");
            for error in [edit_error, write_error] {
                assert_eq!(
                    error.model_facing_message().string_at(""),
                    Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
                );
            }
            assert_eq!(
                std::fs::read_to_string(&fixture.path).unwrap(),
                "shell changed\n"
            );
            fixture.read().await;
            fixture
                .edit("shell changed", "edited")
                .await
                .expect("Read recovers from same-mtime changes");
        }
    }
}

#[tokio::test]
async fn metadata_only_shell_changes_do_not_invalidate_full_or_partial_snapshots() {
    for limit in [None, Some(1)] {
        let fixture = Fixture::new().await;
        fixture.read_with_limit(limit).await;
        let before = fixture.entry();
        fixture.bash("touch target.txt").await;
        assert_eq!(
            fixture.entry(),
            before,
            "unchanged bytes must retain their read snapshot"
        );
        if limit.is_none() {
            fixture
                .edit("original", "edited")
                .await
                .expect("unchanged full content is safe despite touched mtime");
        }
    }
}
