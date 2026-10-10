//! Shared source-preserving JSON projection API.
//!
//! The flat arena lives in the dependency-neutral `json-projection` crate so
//! protocol, host, and domain crates all use the same source carrier.
pub use json_projection::*;
