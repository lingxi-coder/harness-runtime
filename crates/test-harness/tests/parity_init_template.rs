//! Parity: `/init` template byte-locked.
//!
//! M5-10 Task 10.

use command_api::builtins::OLD_INIT_PROMPT;
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

#[test]
fn byte_length_matches_fixture() {
    let f = load();
    assert_eq!(OLD_INIT_PROMPT.len(), f.byte_length);
}

#[test]
fn line_count_matches_fixture() {
    let f = load();
    assert_eq!(OLD_INIT_PROMPT.lines().count(), f.line_count);
}

#[test]
fn sha256_matches_fixture() {
    let f = load();
    let mut hasher = Sha256::new();
    hasher.update(OLD_INIT_PROMPT.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    assert_eq!(digest, f.sha256);
}

#[test]
fn first_sentence_matches_fixture() {
    let f = load();
    assert!(OLD_INIT_PROMPT.starts_with(&f.first_sentence));
}

#[test]
fn contains_all_required_substrings() {
    let f = load();
    for s in &f.must_contain_substrings {
        assert!(OLD_INIT_PROMPT.contains(s), "missing substring: {s}");
    }
}
