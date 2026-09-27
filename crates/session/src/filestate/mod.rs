//! In-memory file-state cache and Read↔Edit verification.
//!
//! See spec §23 (`FileStateCache`, `verify_file_state`, `merge_caches`).
//!
//! Folded into the `session` crate (was the standalone `filestate` crate, whose
//! sole consumer was `session::resumer`).

pub mod cache;
pub mod merge;
pub mod verify;

pub use cache::{FileState, FileStateCache, MAX_BYTES, MAX_ENTRIES};
pub use merge::merge_caches;
pub use verify::{verify_file_state, FileStateVerification};
