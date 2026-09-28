#![cfg(feature = "builtin-handlers")]

//! Keep build-script regressions discoverable by normal Cargo integration tests.
#[allow(dead_code)]
#[path = "../../build-support/git_metadata.rs"]
mod git_metadata;
