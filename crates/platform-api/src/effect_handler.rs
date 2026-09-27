//! Side-effect dispatch abstraction.
//!
//! The reducer is pure and emits [`Effect`] values; an `EffectHandler` turns
//! each Effect into real I/O (API call, render, persistence, …). See spec §5.3.

use async_trait::async_trait;
use protocol::{Effect, EffectError, EffectResult};

/// Processes Effects emitted by the reducer.
///
/// Implementations are typically a router that dispatches each variant to a
/// dedicated subsystem (api-client, persistence, renderer, …).
#[async_trait]
pub trait EffectHandler: Send + Sync {
    /// Process one Effect emitted by the reducer.
    async fn handle(&self, effect: Effect) -> Result<EffectResult, EffectError>;
}
