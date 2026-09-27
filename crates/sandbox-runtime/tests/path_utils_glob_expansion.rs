//! Integration test for the filesystem-touching `expand_glob_pattern`
//! (sandbox-utils.js:429-471) against a real tempdir.

use std::fs;

use sandbox_runtime::path_utils::expand_glob_pattern;

#[test]
fn expand_glob_pattern_matches_top_level_ts_only() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let base = dir.path();

    // Lay out: foo.ts, bar.js, sub/baz.ts
    fs::write(base.join("foo.ts"), b"").unwrap();
    fs::write(base.join("bar.js"), b"").unwrap();
    fs::create_dir(base.join("sub")).unwrap();
    fs::write(base.join("sub").join("baz.ts"), b"").unwrap();

    // `expand_glob_pattern` normalizes the base dir via realpath, which on
    // macOS maps /var/... -> /private/var/... — canonicalize the expected
    // base the same way so the comparison is platform-stable.
    let canonical_base = fs::canonicalize(base).expect("canonicalize base");
    let pattern = format!("{}/*.ts", canonical_base.display());

    let mut matches = expand_glob_pattern(&pattern);
    matches.sort();

    let expected = vec![canonical_base.join("foo.ts").to_string_lossy().into_owned()];
    // `*` does not cross `/`, so sub/baz.ts must NOT match; bar.js is excluded.
    assert_eq!(matches, expected);
}

#[test]
fn expand_glob_pattern_globstar_matches_nested() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let base = dir.path();

    fs::write(base.join("top.ts"), b"").unwrap();
    fs::create_dir(base.join("sub")).unwrap();
    fs::write(base.join("sub").join("deep.ts"), b"").unwrap();
    fs::write(base.join("sub").join("note.txt"), b"").unwrap();

    let canonical_base = fs::canonicalize(base).expect("canonicalize base");
    let pattern = format!("{}/**/*.ts", canonical_base.display());

    let mut matches = expand_glob_pattern(&pattern);
    matches.sort();

    let mut expected = vec![
        canonical_base.join("top.ts").to_string_lossy().into_owned(),
        canonical_base
            .join("sub")
            .join("deep.ts")
            .to_string_lossy()
            .into_owned(),
    ];
    expected.sort();
    assert_eq!(matches, expected);
}

#[test]
fn expand_glob_pattern_too_broad_returns_empty() {
    // A pattern whose static prefix is "/" is rejected as too broad.
    assert!(expand_glob_pattern("/*").is_empty());
}
