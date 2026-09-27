//! M3-02 phase 10: full LINGXI.md hierarchy load with mocked home dir.
//!
//! Exercises `lingxi_md::hierarchy::walk` + `lingxi_md::loader::load_file`
//! together. The hierarchy must surface entries in cwd-first order with
//! `LINGXI.local.md` shadowing `LINGXI.md` at the same depth.
//!
//! ⚠️ CORRECTED — this header claimed "GAP 4: the LINGXI.md loader has NO size
//! drop (parity with claude-code `readFile`), so an oversized file loads in
//! full rather than being skipped". The 2.1.220 binary drops it: `EG`
//! (@229022173) stats first and returns `null` for anything non-regular or over
//! `ELu = 4194304`. The walk still DISCOVERS such a file — the guard is in the
//! reader, not the scanner — which is why the oversize test asserts on both.

use memory::lingxi_md::hierarchy::walk;
use memory::lingxi_md::loader::load_file;
use memory::MAX_MEMORY_FILE_SIZE;
use std::fs;
use tempfile::TempDir;

fn touch(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
}

#[test]
fn full_hierarchy_walk_then_load_returns_innermost_first() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let user_lingxi = home.join(".lingxi");
    fs::create_dir_all(&user_lingxi).unwrap();
    touch(&user_lingxi.join("LINGXI.md"), "# user notes\n");

    let repo = tmp.path().join("repo");
    let pkg = repo.join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    touch(&repo.join("LINGXI.md"), "# repo notes\n");
    touch(&pkg.join("LINGXI.md"), "# pkg notes\n");
    touch(&pkg.join("LINGXI.local.md"), "# pkg local override\n");

    let h = walk(&pkg, &home, None);
    let loaded: Vec<_> = h
        .entries
        .iter()
        .map(|e| load_file(&e.path, None).unwrap())
        .collect();

    // Walk order: pkg/local, pkg/LINGXI.md, repo/LINGXI.md, home/LINGXI.md.
    let paths: Vec<_> = loaded.iter().map(|f| f.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            pkg.join("LINGXI.local.md"),
            pkg.join("LINGXI.md"),
            repo.join("LINGXI.md"),
            user_lingxi.join("LINGXI.md"),
        ]
    );
    assert!(loaded[0].body.contains("pkg local override"));
    assert!(loaded[3].body.contains("user notes"));
}

#[test]
fn oversized_file_is_skipped_while_other_files_still_load() {
    // RENAMED from `oversized_file_loads_whole_no_size_drop_other_files_also_load`.
    // "GAP 4: no size drop" was wrong — `EG` (@229022173) skips anything over
    // `ELu = 4194304`. One oversized file must not stop the rest loading.
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let user_lingxi = home.join(".lingxi");
    fs::create_dir_all(&user_lingxi).unwrap();
    touch(&user_lingxi.join("LINGXI.md"), "# small\n");

    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let big = vec![b'x'; MAX_MEMORY_FILE_SIZE + 1];
    fs::write(repo.join("LINGXI.md"), &big).unwrap();

    let h = walk(&repo, &home, None);
    // The walk still DISCOVERS both files — the size guard lives in the reader,
    // not the scanner, exactly as claude-code splits `Eds` (walk) from `EG`
    // (stat + read).
    assert_eq!(h.entries.len(), 2, "both files are discovered");

    let loaded: Vec<_> = h
        .entries
        .iter()
        .filter_map(|e| load_file(&e.path, None).ok())
        .collect();
    // CORRECTED from "both the small and oversized files load". The oversized
    // one is 10 MiB + 1, over `ELu = 4194304`, so claude-code skips it
    // (`EG`: `if(!o.isFile()||o.size>r) return n?.(o),null`). The old
    // expectation followed from a module doc that claimed no size check and
    // cited leaked TS; the binary disagrees.
    assert_eq!(loaded.len(), 1, "the oversized file is skipped, not loaded");
    assert_eq!(loaded[0].path, user_lingxi.join("LINGXI.md"));
    assert_eq!(loaded[0].body, "# small\n");

    match load_file(&repo.join("LINGXI.md"), None) {
        Err(memory::lingxi_md::loader::LoaderError::FileTooLarge { bytes, .. }) => {
            assert_eq!(bytes, (MAX_MEMORY_FILE_SIZE + 1) as u64);
        }
        other => panic!("expected FileTooLarge for the oversized file, got {other:?}"),
    }
}

#[test]
fn empty_dir_tree_yields_empty_hierarchy_no_error() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("empty");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    let h = walk(&cwd, &home, None);
    assert!(h.entries.is_empty());
}
