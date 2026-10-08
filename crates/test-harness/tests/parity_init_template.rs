//! Parity: `/init` template byte-locked.
//!
//! M5-10 Task 10.

use command_api::builtins::INIT_PROMPT;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("../src/parity/fixtures/parity_init_template.json");

#[derive(Debug, Deserialize)]
struct Fixture {
    byte_length: usize,
    line_count: usize,
    sha256: String,
    first_sentence: String,
    must_contain_substrings: Vec<String>,
}

fn load() -> Fixture {
    serde_json::from_str(FIXTURE).expect("fixture parse")
}

/// Project the current product identity into the captured oracle vocabulary.
/// The raw source fixture retains its historical URL and checksum.
fn oracle_projection() -> String {
    INIT_PROMPT.replacen(
        " when working with code in this repository.",
        " (claude.ai/code) when working with code in this repository.",
        1,
    )
}

#[test]
fn byte_length_matches_fixture() {
    let f = load();
    assert_eq!(oracle_projection().len(), f.byte_length);
}

#[test]
fn line_count_matches_fixture() {
    let f = load();
    assert_eq!(INIT_PROMPT.lines().count(), f.line_count);
}

#[test]
fn sha256_matches_fixture() {
    let f = load();
    let mut hasher = Sha256::new();
    hasher.update(oracle_projection().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    assert_eq!(digest, f.sha256);
}

#[test]
fn first_sentence_matches_fixture() {
    let f = load();
    assert!(INIT_PROMPT.starts_with(&f.first_sentence));
}

#[test]
fn contains_all_required_substrings() {
    let f = load();
    for s in &f.must_contain_substrings {
        assert!(oracle_projection().contains(s), "missing substring: {s}");
    }
}
