//! Side effects emitted by the reducer.
//!
//! Each variant is a request to do something with the outside world.
//! `EffectHandler` (in `core::host`) processes these. See spec §5.3.
//!
//! M1.1 ships a subset: API, render, persistence. Later plans (Tools, Hooks,
//! Memory, MCP, Agent, etc.) extend this enum.

use crate::types::ids::{RequestId, SessionId, ToolUseId};
use crate::types::messages::ConversationMessage;
use crate::types::secret::{RedactableContent, SecureStorageData};
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

/// Provider-neutral model input emitted by the pure reducer.
///
/// This is an application-level intent, not a provider request body. The
/// host/runtime adapter maps it to the SDK's semantic request types; provider
/// codecs remain owned by `llm-client`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Model identifier selected by the session.
    pub model: String,
    /// Optional host-side provider profile used to resolve the model route.
    pub profile: Option<String>,
    /// Conversation history after the new user message has been appended.
    pub messages: Vec<ConversationMessage>,
    /// Current Core request output limit.
    pub max_output_tokens: u32,
}

/// A side effect requested by the reducer.
///
/// `Effect` deliberately does NOT implement `PartialEq`/`Eq`: some variants
/// carry secret material (`StoreCredential.data`, `ScanForSecrets.content`)
/// whose contents must never be compared by value (timing leaks; accidental
/// disclosure in panic messages). Tests that need to assert structural
/// equality should round-trip through JSON and compare strings instead.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    // — API —
    /// Request a model response. Resulting events arrive as `Event::ApiStream*`.
    RequestModel {
        /// Correlation ID used to match streaming events back to this request.
        request_id: RequestId,
        /// Provider-neutral model and typed conversation data.
        request: ModelRequest,
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

/// Retain tagged-effect members as JSON source until the selected variant is
/// known. Serde's internally-tagged enum buffering recursively materializes
/// members, which cannot carry raw source fields used by exact conversation
/// message deserialization.
struct RawEffectObject(HashMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for RawEffectObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawEffectObjectVisitor;

        impl<'de> Visitor<'de> for RawEffectObjectVisitor {
            type Value = RawEffectObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object with raw effect members")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    fields.insert(key, map.next_value::<Box<RawValue>>()?);
                }
                Ok(RawEffectObject(fields))
            }
        }

        deserializer.deserialize_map(RawEffectObjectVisitor)
    }
}

fn raw_effect_required<T, E>(
    fields: &mut HashMap<String, Box<RawValue>>,
    name: &'static str,
) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    let raw = fields.remove(name).ok_or_else(|| E::missing_field(name))?;
    serde_json::from_str(raw.get()).map_err(E::custom)
}

impl<'de> Deserialize<'de> for Effect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let RawEffectObject(mut fields) = RawEffectObject::deserialize(deserializer)?;
        let kind = raw_effect_required::<String, D::Error>(&mut fields, "kind")?;
        macro_rules! field {
            ($name:literal) => {
                raw_effect_required(&mut fields, $name)?
            };
        }

        match kind.as_str() {
            "request_model" => Ok(Self::RequestModel {
                request_id: field!("request_id"),
                request: field!("request"),
            }),
            "render_stream_delta" => Ok(Self::RenderStreamDelta {
                text: field!("text"),
            }),
            "render_error" => Ok(Self::RenderError {
                error: field!("error"),
            }),
            "render_token_usage_update" => Ok(Self::RenderTokenUsageUpdate {
                input_tokens: field!("input_tokens"),
                output_tokens: field!("output_tokens"),
            }),
            "persist_session_snapshot" => Ok(Self::PersistSessionSnapshot {
                session_id: field!("session_id"),
                snapshot: field!("snapshot"),
            }),
            "load_session" => Ok(Self::LoadSession {
                session_id: field!("session_id"),
            }),
            "terminate" => Ok(Self::Terminate {
                reason: field!("reason"),
            }),
            "record_unexpected_event" => Ok(Self::RecordUnexpectedEvent {
                state_name: field!("state_name"),
                event_name: field!("event_name"),
            }),
            "persist_cost_state" => Ok(Self::PersistCostState {
                state_json: field!("state_json"),
            }),
            "display_cost_update" => Ok(Self::DisplayCostUpdate {
                snapshot_json: field!("snapshot_json"),
            }),
            "enforce_budget" => Ok(Self::EnforceBudget {
                estimated_cost_nano_usd: field!("estimated_cost_nano_usd"),
            }),
            "fire_budget_warning" => Ok(Self::FireBudgetWarning {
                pct: field!("pct"),
                current: field!("current"),
                limit: field!("limit"),
            }),
            "halt_on_budget" => Ok(Self::HaltOnBudget),
            "evaluate_permission" => Ok(Self::EvaluatePermission {
                tool_use_id: field!("tool_use_id"),
                tool_name: field!("tool_name"),
                input: field!("input"),
            }),
            "persist_permission_update" => Ok(Self::PersistPermissionUpdate {
                update_json: field!("update_json"),
            }),
            "detect_shadowed_rules" => Ok(Self::DetectShadowedRules {
                rule_json: field!("rule_json"),
            }),
            "store_credential" => Ok(Self::StoreCredential {
                kind: field!("credential_kind"),
                data: field!("data"),
            }),
            "retrieve_credential" => Ok(Self::RetrieveCredential {
                kind: field!("credential_kind"),
            }),
            "delete_credential" => Ok(Self::DeleteCredential {
                kind: field!("credential_kind"),
            }),
            "scan_for_secrets" => Ok(Self::ScanForSecrets {
                boundary: field!("boundary"),
                content: field!("content"),
            }),
            _ => Err(serde::de::Error::custom(format!(
                "unknown effect kind {kind:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, MessageId};

    #[test]
    fn request_model_roundtrip() {
        // `Effect` no longer implements `PartialEq` (secrets must not be
        // compared by value). Verify the roundtrip by re-serializing and
        // comparing the JSON strings instead.
        let e = Effect::RequestModel {
            request_id: RequestId::nil(),
            request: ModelRequest {
                model: "claude-opus-4-6".into(),
                profile: Some("anthropic".into()),
                messages: Vec::new(),
                max_output_tokens: 8192,
            },
        };
        let s = serde_json::to_string(&e).unwrap();
        let e2: Effect = serde_json::from_str(&s).unwrap();
        let s2 = serde_json::to_string(&e2).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn request_model_roundtrip_preserves_exact_utf16_message() {
        let message = crate::types::ConversationMessage::User {
            id: MessageId::nil(),
            content: vec![ContentBlock::TextJsUtf16 {
                text: "x�".into(),
                utf16_code_units: vec![u16::from(b'x'), 0xD800],
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
            api_message_override: None,
        };
        let effect = Effect::RequestModel {
            request_id: RequestId::nil(),
            request: ModelRequest {
                model: "claude-opus-4-6".into(),
                profile: None,
                messages: vec![message],
                max_output_tokens: 8192,
            },
        };
        let wire = serde_json::to_string(&effect).unwrap();
        let decoded: Effect = serde_json::from_str(&wire).unwrap();
        let Effect::RequestModel { request, .. } = decoded else {
            panic!("request model effect");
        };
        let ConversationMessage::User { content, .. } = &request.messages[0] else {
            panic!("request retains the typed user message");
        };
        assert!(matches!(
            content.as_slice(),
            [ContentBlock::TextJsUtf16 { utf16_code_units, .. }]
                if utf16_code_units == &[u16::from(b'x'), 0xD800]
        ));
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
