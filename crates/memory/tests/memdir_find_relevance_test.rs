//! M3-02 phase 10: full memdir scan + `find_relevant` ranking.
//!
//! Builds a small `~/.lingxi/memdir/` with three entries of varying age
//! and content, runs `scan_memdir_at` + `find_relevant`, and asserts the
//! top-`k` matches the deterministic expected ordering.

use memory::memdir::find::{find_relevant, RelevanceInputs};
use memory::memdir::paths::memdir_path;
use memory::memdir::scan::scan_memdir_at;
use std::fs;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn write_dated(path: &std::path::Path, body: &str, age_days: u64, now: SystemTime) {
    fs::write(path, body).unwrap();
    let mtime = now - Duration::from_secs(age_days * 86_400);
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(mtime)).unwrap();
}

#[test]
fn full_memdir_scan_then_find_relevant_returns_byte_identical_ordering() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let memdir = home
        .join(".lingxi")
        .join("projects")
        .join("-proj")
        .join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    let now = SystemTime::now();
    write_dated(&memdir.join("fresh-alpha.md"), "alpha beta gamma", 0, now);
    write_dated(&memdir.join("old-alpha.md"), "alpha beta", 270, now);
    write_dated(&memdir.join("fresh-delta.md"), "delta epsilon", 0, now);

    let roots = memdir_path(home, std::path::Path::new("/proj"), false);
    let snap = scan_memdir_at(&roots, now).unwrap();
    assert_eq!(snap.entries.len(), 3);

    let out = find_relevant(
        &snap.entries,
        &RelevanceInputs {
            prompt: "alpha beta",
            k: Some(3),
            team_boost_enabled: false,
        },
    );
    let names: Vec<_> = out
        .iter()
        .map(|e| {
            e.path
                .file_name()
                .and_then(|n| n.to_str().map(String::from))
        })
        .collect::<Option<Vec<_>>>()
        .unwrap();

    // fresh-alpha highest (jaccard = 2/3 = 6666, age = 10000, user = 6000)
    // old-alpha next     (jaccard = 2/2 = 10000, age = 1000 floor, user = 6000)
    // fresh-delta zero   (jaccard = 0)
    // 6666 * 10000 * 6000 = 399_960_000_000
    // 10000 * 1000 * 6000 = 60_000_000_000
    // 0 → 0
    assert_eq!(
        names,
        vec!["fresh-alpha.md", "old-alpha.md", "fresh-delta.md"]
    );
}

#[test]
fn ranking_is_deterministic_across_repeated_runs() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let memdir = home
        .join(".lingxi")
        .join("projects")
        .join("-proj")
        .join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    let now = SystemTime::now();
    write_dated(&memdir.join("a.md"), "x y z", 0, now);
    write_dated(&memdir.join("b.md"), "x y", 30, now);
    write_dated(&memdir.join("c.md"), "y z", 60, now);

    let roots = memdir_path(home, std::path::Path::new("/proj"), false);
    let snap = scan_memdir_at(&roots, now).unwrap();

    let r1 = find_relevant(
        &snap.entries,
        &RelevanceInputs {
            prompt: "x y z",
            k: Some(3),
            team_boost_enabled: false,
        },
    );
    let r2 = find_relevant(
        &snap.entries,
        &RelevanceInputs {
            prompt: "x y z",
            k: Some(3),
            team_boost_enabled: false,
        },
    );
    let names = |v: &[protocol::MemoryEntry]| -> Vec<String> {
        v.iter()
            .map(|e| e.path.to_string_lossy().to_string())
            .collect()
    };
    assert_eq!(names(&r1), names(&r2));
}

#[test]
fn entries_older_than_365_dropped_at_scan_not_in_results() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let memdir = home
        .join(".lingxi")
        .join("projects")
        .join("-proj")
        .join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    let now = SystemTime::now();
    write_dated(&memdir.join("ancient.md"), "alpha beta", 400, now);
    write_dated(&memdir.join("fresh.md"), "alpha beta", 0, now);

    let roots = memdir_path(home, std::path::Path::new("/proj"), false);
    let snap = scan_memdir_at(&roots, now).unwrap();
    let names: Vec<_> = snap
        .entries
        .iter()
        .filter_map(|e| {
            e.path
                .file_name()
                .and_then(|n| n.to_str().map(String::from))
        })
        .collect();
    assert!(names.contains(&"fresh.md".into()));
    assert!(!names.contains(&"ancient.md".into()));
}
