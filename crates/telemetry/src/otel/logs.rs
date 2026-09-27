//! OTEL log signal — the `claude_code.events` logger CC 2.1.207 uses to emit
//! structured events (user prompts, tool calls/results, assistant responses,
//! API bodies), each gated by an `OTEL_LOG_*` opt-in.
//!
//! The log-emit egress (`LoggerProvider` + OTLP log exporter) is the documented
//! remainder; this module owns the signal names and the opt-in gate helpers so
//! emit sites read a single authoritative predicate.

use super::config::{bool_env, env_truthy};

/// Log signal / logger name (`claude_code.events`). Kept verbatim (Monitoring
/// schema).
pub const EVENTS_SIGNAL: &str = "claude_code.events";

/// Trace signal / tracer name (`claude_code.tracing`). Kept verbatim.
pub const TRACING_SIGNAL: &str = "claude_code.tracing";

/// Whether assistant response bodies should be logged
/// (`OTEL_LOG_ASSISTANT_RESPONSES`). Byte-faithful truthy parse (binary `ct`),
/// so `1`/`true`/`yes`/`on` (case-insensitive, trimmed) all enable it. Default
/// OFF ⇒ byte-noop.
///
/// This is the single gate the orchestrator's turn loop consults before it
/// emits the opt-in `assistant_response` log line.
#[must_use]
pub fn assistant_responses_enabled() -> bool {
    env_gate("OTEL_LOG_ASSISTANT_RESPONSES")
}

/// Whether user prompt text should be logged (`OTEL_LOG_USER_PROMPTS`).
#[must_use]
pub fn user_prompts_enabled() -> bool {
    env_gate("OTEL_LOG_USER_PROMPTS")
}

/// Whether tool call parameters should be logged (`OTEL_LOG_TOOL_DETAILS`).
#[must_use]
pub fn tool_details_enabled() -> bool {
    env_gate("OTEL_LOG_TOOL_DETAILS")
}

/// Whether tool result content should be logged (`OTEL_LOG_TOOL_CONTENT`).
#[must_use]
pub fn tool_content_enabled() -> bool {
    env_gate("OTEL_LOG_TOOL_CONTENT")
}

/// Whether raw API request/response bodies should be logged
/// (`OTEL_LOG_RAW_API_BODIES`).
#[must_use]
pub fn raw_api_bodies_enabled() -> bool {
    env_gate("OTEL_LOG_RAW_API_BODIES")
}

/// Read an `OTEL_LOG_*` gate from the process env with the binary `ct`/`hNr`
/// (default `false`) semantics.
fn env_gate(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => env_truthy(&v),
        Err(_) => bool_env(None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_are_verbatim() {
        assert_eq!(EVENTS_SIGNAL, "claude_code.events");
        assert_eq!(TRACING_SIGNAL, "claude_code.tracing");
    }

    #[test]
    fn assistant_responses_default_off() {
        // The var is not set in the test process ⇒ byte-noop default.
        // (Do not mutate process env here — other tests run in parallel.)
        if std::env::var("OTEL_LOG_ASSISTANT_RESPONSES").is_err() {
            assert!(!assistant_responses_enabled());
        }
    }
}
