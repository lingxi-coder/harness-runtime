//! Tests for `wrap_command_for_cwd_tracking` and `task_output_path`.

use platform_posix::process::{task_output_path, wrap_command_for_cwd_tracking};
use std::path::Path;

#[test]
fn bash_wrap_includes_extglob_disable_and_pwd_tail() {
    let cwd_file = Path::new("/tmp/cwd.txt");
    let wrapped = wrap_command_for_cwd_tracking("ls -la", cwd_file, "/bin/bash", false);
    assert!(
        wrapped.contains("shopt -u extglob 2>/dev/null || true"),
        "missing bash extglob disable: {wrapped}"
    );
    assert!(
        wrapped.contains(r"pwd -P >| '/tmp/cwd.txt'"),
        "missing pwd -P tail: {wrapped}"
    );
    assert!(wrapped.contains("ls -la"), "user command must be present");
}

#[test]
fn zsh_wrap_uses_zsh_idiom() {
    let cwd_file = Path::new("/tmp/cwd.txt");
    let wrapped = wrap_command_for_cwd_tracking("echo hi", cwd_file, "/bin/zsh", false);
    assert!(
        wrapped.contains("setopt NO_EXTENDED_GLOB 2>/dev/null || true"),
        "missing zsh extglob disable: {wrapped}"
    );
}

#[test]
fn shell_prefix_uses_combined_idiom() {
    let cwd_file = Path::new("/tmp/cwd.txt");
    let wrapped = wrap_command_for_cwd_tracking("echo hi", cwd_file, "/bin/bash", true);
    assert!(
        wrapped
            .contains("{ shopt -u extglob || setopt NO_EXTENDED_GLOB; } >/dev/null 2>&1 || true"),
        "missing combined extglob disable: {wrapped}"
    );
}

#[test]
fn cwd_file_path_with_single_quote_is_escaped() {
    let cwd_file = Path::new("/tmp/it's a cwd.txt");
    let wrapped = wrap_command_for_cwd_tracking("ls", cwd_file, "/bin/bash", false);
    // Bash single-quote escape: replace ' with '\''
    assert!(
        wrapped.contains(r"pwd -P >| '/tmp/it'\''s a cwd.txt'"),
        "single-quote not shell-escaped: {wrapped}"
    );
}

#[test]
fn task_output_path_uses_unique_task_id() {
    let p = task_output_path("abc123");
    let s = p.to_string_lossy();
    assert!(s.contains("lingxi-task-output"));
    assert!(s.ends_with("abc123.out"));
}
