//! Side effects emitted by the reducer.
//!
//! Each variant is a request to do something with the outside world.
//! `EffectHandler` (in `core::host`) processes these. See spec §5.3.
//!
//! M1.1 ships a subset: API, render, persistence. Later plans (Tools, Hooks,
//! Memory, MCP, Agent, etc.) extend this enum.

use crate::types::ids::{RequestId, SessionId, ToolUseId};
use crate::types::secret::{RedactableContent, SecureStorageData};
use crate::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A side effect requested by the reducer.
///
/// `Effect` deliberately does NOT implement `PartialEq`/`Eq`: some variants
/// carry secret material (`StoreCredential.data`, `ScanForSecrets.content`)
/// whose contents must never be compared by value (timing leaks; accidental
/// disclosure in panic messages). Tests that need to assert structural
/// equality should round-trip through JSON and compare strings instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    // — API —
    /// Send a fully-assembled request body. Reply arrives as `Event::ApiStream*`.
    SendApiRequest {
        /// Correlation ID used to match streaming events back to this request.
        request_id: RequestId,
        /// Provider-shape body with exact JavaScript string/key sidecars.
        /// Keep this projection typed until the host serializes the provider
        /// request with [`Utf16JsonProjection::to_json_string`].
        request_body: Utf16JsonProjection,
    },

    // — Render (UI / TUI / SDK consumers) —
    /// Emit an incremental text delta to attached UI consumers.
    RenderStreamDelta {
        /// The new text fragment to append to the current assistant turn.
        text: String,
    },
    /// Emit a user-visible error message to attached UI consumers.
    RenderError {
        /// Human-readable error description.
        error: String,
    },
    /// Update token usage counters surfaced in the UI.
    RenderTokenUsageUpdate {
        /// Cumulative input tokens for the current turn.
        input_tokens: u64,
        /// Cumulative output tokens for the current turn.
        output_tokens: u64,
    },

    // — Persistence (full session model lands in Plan 10) —
    /// Persist a snapshot of session state to durable storage.
    PersistSessionSnapshot {
        /// Session this snapshot belongs to.
        session_id: SessionId,
        /// Opaque snapshot payload (schema defined in Plan 10).
        snapshot: Value,
    },

    // — Lifecycle —
    /// Request the host to load a previously-persisted session.
    LoadSession {
        /// Session identifier to load.
        session_id: SessionId,
    },
    /// Terminate the run loop with a reason for logs.
    Terminate {
        /// Free-form reason text; surfaced in logs and telemetry.
        reason: String,
    },

    // — Diagnostic —
    /// Reducer hit a (state, event) pair it doesn't have a transition for.
    /// Emitted instead of `tracing::warn!` to keep the reducer pure.
    RecordUnexpectedEvent {
        /// Name of the state the reducer was in.
        state_name: String,
        /// Name of the event variant that had no handler.
        event_name: String,
    },

    // — Cost —
    /// Persist the current cost-tracking state to durable storage.
    PersistCostState {
        /// Opaque cost state payload (schema owned by the `lingxi-cost` crate).
        state_json: Value,
    },
    /// Push an updated cost snapshot to attached UI consumers for display.
    DisplayCostUpdate {
        /// Opaque cost snapshot payload (schema owned by the `lingxi-cost` crate).
        snapshot_json: Value,
    },
    /// Ask the host to enforce the active budget against an estimated cost.
    EnforceBudget {
        /// Estimated cost of the pending operation in nano USD (1e-9 USD).
        estimated_cost_nano_usd: u64,
    },
    /// Fire a budget-threshold warning at the given percentage of the limit.
    FireBudgetWarning {
        /// Threshold percentage that was crossed (e.g. `80` for 80%).
        pct: u32,
        /// Current spend in nano USD at the time of the warning.
        current: u64,
        /// Configured budget limit in nano USD.
        limit: u64,
    },
    /// Halt the run loop because the active budget has been exceeded.
    HaltOnBudget,

    // — Permission —
    /// Ask the permission subsystem to evaluate a tool call against active rules.
    EvaluatePermission {
        /// `ToolUseId` of the call being evaluated.
        tool_use_id: ToolUseId,
        /// Fully-qualified tool name (e.g. `"fs.read"`).
        tool_name: String,
        /// Tool input payload to evaluate against rule predicates.
        input: Value,
    },
    /// Persist a permission rule update (add/remove/modify) to durable storage.
    PersistPermissionUpdate {
        /// Opaque permission update payload (schema owned by `lingxi-permission`).
        update_json: Value,
    },
    /// Run shadowed-rule detection over a newly-added permission rule.
    DetectShadowedRules {
        /// Opaque rule payload to analyse for shadowing.
        rule_json: Value,
    },

    // — Secret —
    /// Store a credential of the given kind in the platform secret store.
    StoreCredential {
        /// Credential kind tag (e.g. `"api_key"`, `"oauth_token"`).
        ///
        /// Serialized as `credential_kind` on the wire to avoid colliding with
        /// the enum's `kind` discriminator tag.
        #[serde(rename = "credential_kind")]
        kind: String,
        /// Secure payload to persist (zeroized on drop).
        data: SecureStorageData,
    },
    /// Retrieve a credential of the given kind from the platform secret store.
    RetrieveCredential {
        /// Credential kind tag to look up.
        ///
        /// Serialized as `credential_kind` on the wire to avoid colliding with
        /// the enum's `kind` discriminator tag.
        #[serde(rename = "credential_kind")]
        kind: String,
    },
    /// Delete a credential of the given kind from the platform secret store.
    DeleteCredential {
        /// Credential kind tag to delete.
        ///
        /// Serialized as `credential_kind` on the wire to avoid colliding with
        /// the enum's `kind` discriminator tag.
        #[serde(rename = "credential_kind")]
        kind: String,
    },
    /// Scan content crossing a trust boundary for secret-like material.
    ScanForSecrets {
        /// Boundary label (e.g. `"prompt_to_model"`, `"tool_output_to_user"`).
        boundary: String,
        /// Redactable content to scan (the scanner may emit a redacted copy).
        content: RedactableContent,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_api_request_roundtrip() {
        // `Effect` no longer implements `PartialEq` (secrets must not be
        // compared by value). Verify the roundtrip by re-serializing and
        // comparing the JSON strings instead.
        let e = Effect::SendApiRequest {
            request_id: RequestId::nil(),
            request_body: Utf16JsonProjection::plain(
                serde_json::json!({"model": "claude-opus-4-6"}),
            ),
        };
        let s = serde_json::to_string(&e).unwrap();
        let e2: Effect = serde_json::from_str(&s).unwrap();
        let s2 = serde_json::to_string(&e2).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn send_api_request_roundtrip_preserves_exact_utf16_body() {
        let body = Utf16JsonProjection::parse(
            r#"{"messages":[{"role":"user","content":"x\ud800"}]}"#,
        )
        .unwrap();
        let encoded = body.to_json_string().unwrap();
        let effect = Effect::SendApiRequest {
            request_id: RequestId::nil(),
            request_body: body,
        };
        let wire = serde_json::to_string(&effect).unwrap();
        let Effect::SendApiRequest { request_body, .. } =
            serde_json::from_str(&wire).unwrap()
        else {
            panic!("send API effect");
        };
        assert_eq!(request_body.to_json_string().unwrap(), encoded);
        assert_eq!(
            request_body.string_units("/messages/0/content"),
            Some(vec![u16::from(b'x'), 0xD800])
        );
    }

    #[test]
    fn render_stream_delta_carries_text() {
        let e = Effect::RenderStreamDelta {
            text: "hello".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("hello"));
    }
}
