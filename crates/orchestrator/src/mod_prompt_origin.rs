//! Host-stamped provenance for the prompt currently entering a model turn.

use std::future::Future;

use serde_json::{json, Value};

tokio::task_local! {
    static PROMPT_ORIGIN: Value;
}

/// Scope a host-authenticated prompt origin to one turn dispatch. The value
/// crosses no persisted boundary; the prompt admission site reads it before
/// the model turn starts.
pub async fn with_origin<F: Future>(origin: Value, future: F) -> F::Output {
    PROMPT_ORIGIN.scope(origin, future).await
}

/// Read the current host origin, using `unclassified` for callers that have
/// not yet stamped their ingress.
#[must_use]
pub fn current() -> Value {
    PROMPT_ORIGIN
        .try_with(Clone::clone)
        .unwrap_or_else(|_| json!({"kind":"unclassified"}))
}
