//! Swarm trait re-exports.
//!
//! The actual `SwarmBackend` trait lives in `lingxi-traits`; coordinator
//! code uses these re-exports so callers do not need to depend on the
//! traits crate directly.

pub use platform_api::{PaneId, PanePosition, SwarmBackend, SwarmError, SwarmHandle, SwarmLayout};
