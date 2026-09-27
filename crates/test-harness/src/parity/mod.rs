//! Parity fixtures derived from the claude-code 2026-03-31 reference. Each
//! fixture captures a known input + expected output for a behavior we
//! committed to in
//! `docs/superpowers/specs/2026-05-23-m2-claude-code-parity-design.md`.
//!
//! Phase B of plan M2-07 lands seven fixtures + driver tests under
//! `crates/test-harness/tests/parity_*.rs`. The drivers load a JSON fixture
//! through [`load_fixture`], run the relevant production code, and assert
//! the output matches the fixture on the load-bearing slice (wire bytes,
//! file layout, error string, ...).

use serde::de::DeserializeOwned;
use std::path::PathBuf;

/// Placeholder retained for backwards-compatibility with the M1 scaffold.
///
/// Future expansion may use this as the deserialized shape for streaming
/// scenario fixtures (events + expected effect summary).
#[derive(Debug, Default)]
pub struct ParityFixture;

/// Locate the `fixtures/` directory beside this source file.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("parity")
        .join("fixtures")
}

/// Load a fixture by stem (e.g. `"mcp_initialize_request"`).
///
/// # Panics
///
/// Panics if the fixture file is missing or its JSON does not deserialize as
/// `T`; tests rely on this to fail loudly.
#[must_use]
pub fn load_fixture<T: DeserializeOwned>(stem: &str) -> T {
    let path = fixtures_dir().join(format!("{stem}.json"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {path:?}: {e}"));
    serde_json::from_slice::<T>(&bytes)
        .unwrap_or_else(|e| panic!("deserialize fixture {stem}: {e}"))
}
