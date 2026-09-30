//! Prompt file trees reflect workspace discovery.

use orchestrator::prompt::file_tree;
use std::fs;
use tempfile::TempDir;

#[test]
fn probe_returns_empty_for_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let t = file_tree::probe(tmp.path(), 2);
    assert!(t.entries.is_empty());
}

#[test]
fn probe_excludes_dotfiles_and_dot_git() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join(".env"), "x").unwrap();
    fs::write(tmp.path().join("README.md"), "x").unwrap();
    let t = file_tree::probe(tmp.path(), 2);
    assert_eq!(t.entries.len(), 1);
    assert_eq!(
        t.entries[0].path.file_name().unwrap().to_string_lossy(),
        "README.md"
    );
}

#[test]
fn probe_respects_depth_limit_2() {
    let tmp = TempDir::new().unwrap();
    let l0 = tmp.path();
    let l1 = l0.join("a");
    let l2 = l1.join("b");
    fs::create_dir_all(&l2).unwrap();
    fs::write(l0.join("root.rs"), "").unwrap();
    fs::write(l1.join("inner.rs"), "").unwrap();
    fs::write(l2.join("deep.rs"), "").unwrap();
    let t = file_tree::probe(l0, 2);
    let names: Vec<String> = t
        .entries
        .iter()
        .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // depth 0: a (dir), root.rs ; depth 1: inner.rs (under a) — NO `b/` or deep.rs.
    assert_eq!(names, vec!["a", "inner.rs", "root.rs"]);
}

#[test]
fn format_renders_dirs_with_trailing_slash() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("a")).unwrap();
    fs::write(tmp.path().join("a").join("b.rs"), "").unwrap();
    fs::write(tmp.path().join("c.rs"), "").unwrap();
    let t = file_tree::probe(tmp.path(), 2);
    let out = file_tree::format(&t);
    let expected = "a/\n  b.rs\nc.rs\n";
    assert_eq!(out, expected);
}

#[test]
fn dirs_sort_before_files_at_same_level() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("aaa.rs"), "").unwrap();
    fs::create_dir_all(tmp.path().join("zzz")).unwrap();
    let t = file_tree::probe(tmp.path(), 2);
    let out = file_tree::format(&t);
    assert_eq!(out, "zzz/\naaa.rs\n");
}
