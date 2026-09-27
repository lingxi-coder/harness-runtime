//! File tree probe + formatter — produces a depth-bounded snapshot
//! of the cwd direct + grandchild entries, and renders it as an
//! indented list. Output is BYTE-STABLE across runs (entries sorted).
#![forbid(unsafe_code)]

use crate::prompt::{FileTree, FileTreeEntry};
use std::fs;
use std::path::Path;

/// Default max depth (cwd + 1 level deeper). Spec §4.3 locks to 2.
pub const DEFAULT_DEPTH_LIMIT: u8 = 2;

/// Probe the cwd for direct children (depth 0) and once-recursive
/// grandchildren (depth 1). Entries are sorted: directories first
/// at each level, alphabetic within.
///
/// Errors (unreadable dir, permission denied) yield an empty tree
/// — never panics. Hidden entries (names starting with `.`) are
/// excluded except for `.git` (skipped via name equality).
#[must_use]
pub fn probe(cwd: &Path, depth_limit: u8) -> FileTree {
    let mut entries = Vec::<FileTreeEntry>::new();
    collect_level(cwd, 0, depth_limit, &mut entries);
    FileTree { entries }
}

fn collect_level(dir: &Path, depth: u8, limit: u8, out: &mut Vec<FileTreeEntry>) {
    if depth >= limit {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let mut batch: Vec<(bool, std::path::PathBuf, String)> = Vec::new();
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name == ".git" || name.starts_with('.') {
            continue;
        }
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        batch.push((is_dir, ent.path(), name));
    }
    // Dirs first, alphabetic within.
    batch.sort_by(|a, b| match (a.0, b.0) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.2.cmp(&b.2),
    });
    for (is_dir, path, _name) in batch {
        // A directory is only emitted if we will recurse into it — leaf-level
        // directories (depth + 1 >= limit) are skipped so the tree never
        // shows dangling dir-only entries.
        if is_dir && depth.saturating_add(1) >= limit {
            continue;
        }
        out.push(FileTreeEntry {
            path: path.clone(),
            is_dir,
            depth,
        });
        if is_dir {
            collect_level(&path, depth.saturating_add(1), limit, out);
        }
    }
}

/// Render a [`FileTree`] as an indented list. Each level is 2 spaces.
/// Directory entries end with `/`. Format:
///
/// ```text
/// a/
///   b.rs
/// c.rs
/// ```
#[must_use]
pub fn format(tree: &FileTree) -> String {
    let mut s = String::with_capacity(256);
    for e in &tree.entries {
        for _ in 0..e.depth {
            s.push_str("  ");
        }
        let name = e
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        s.push_str(&name);
        if e.is_dir {
            s.push('/');
        }
        s.push('\n');
    }
    s
}
