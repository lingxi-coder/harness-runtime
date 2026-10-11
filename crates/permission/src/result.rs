//! `PermissionResult` — outcome of `PermissionPolicy::authorize`.
//!
//! Tagged enum with three variants — `Allow`, `Deny`, `Ask` — each carrying
//! a `PermissionDecisionReason` (why), an optional payload (per-variant),
//! and a shared `PermissionMetadata` (matched rule names for audit).

use crate::mode::PermissionMode;
use crate::rule::PermissionRule;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::RequestId;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::time::SystemTime;

/// The decision returned by `PermissionPolicy::authorize`.
///
/// Variants are tagged by `behavior` for JSON ergonomics.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub enum PermissionResult {
    /// The tool call is permitted.
    Allow {
        /// Why the call was allowed.
        reason: PermissionDecisionReason,
        /// Optional rewritten input the caller should use instead.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
        )]
        updated_input: Option<Utf16JsonProjection>,
        /// Optional destination for persisting a derived rule.
        update_destination: Option<PermissionUpdateDestination>,
        /// Audit metadata (matched rule names, etc.).
        metadata: PermissionMetadata,
    },
    /// The tool call is rejected.
    Deny {
        /// Why the call was denied.
        reason: PermissionDecisionReason,
        /// Optional human-readable explanation surfaced to the user.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
        )]
        explanation: Option<Utf16JsonProjection>,
        /// Audit metadata.
        metadata: PermissionMetadata,
    },
    /// The decision is deferred to the user.
    Ask {
        /// Why the user is being asked.
        reason: PermissionDecisionReason,
        /// Prompt the host should display.
        prompt: PermissionPrompt,
        /// Optional classifier check still in flight for this request.
        pending_classifier_check: Option<PendingClassifierCheck>,
        /// Audit metadata.
        metadata: PermissionMetadata,
    },
}

// Deserialize the public internally-tagged shape without asking Serde to buffer
// its map into `Content`. That buffering loses the raw member source required by
// `Utf16JsonProjection` fields (including escaped lone UTF-16 surrogates).
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PermissionResultWire {
    Allow {
        reason: PermissionDecisionReason,
        #[serde(
            default,
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
        )]
        updated_input: Option<Utf16JsonProjection>,
        update_destination: Option<PermissionUpdateDestination>,
        metadata: PermissionMetadata,
    },
    Deny {
        reason: PermissionDecisionReason,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
        )]
        explanation: Option<Utf16JsonProjection>,
        metadata: PermissionMetadata,
    },
    Ask {
        reason: PermissionDecisionReason,
        prompt: PermissionPrompt,
        pending_classifier_check: Option<PendingClassifierCheck>,
        metadata: PermissionMetadata,
    },
}

impl From<PermissionResultWire> for PermissionResult {
    fn from(wire: PermissionResultWire) -> Self {
        match wire {
            PermissionResultWire::Allow {
                reason,
                updated_input,
                update_destination,
                metadata,
            } => Self::Allow {
                reason,
                updated_input,
                update_destination,
                metadata,
            },
            PermissionResultWire::Deny {
                reason,
                explanation,
                metadata,
            } => Self::Deny {
                reason,
                explanation,
                metadata,
            },
            PermissionResultWire::Ask {
                reason,
                prompt,
                pending_classifier_check,
                metadata,
            } => Self::Ask {
                reason,
                prompt,
                pending_classifier_check,
                metadata,
            },
        }
    }
}

struct RawPermissionResultFields(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for RawPermissionResultFields {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RawObjectVisitor;

        impl<'de> serde::de::Visitor<'de> for RawObjectVisitor {
            type Value = RawPermissionResultFields;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a permission result object with raw member values")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut fields = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    fields.push((key, value));
                }
                Ok(RawPermissionResultFields(fields))
            }
        }

        deserializer.deserialize_map(RawObjectVisitor)
    }
}

fn permission_result_wire_json<E>(
    behavior: &str,
    fields: Vec<(String, Box<RawValue>)>,
) -> Result<String, E>
where
    E: serde::de::Error,
{
    let mut json = String::from("{");
    json.push_str(&serde_json::to_string(behavior).map_err(E::custom)?);
    json.push_str(":{");
    for (index, (key, value)) in fields.into_iter().enumerate() {
        if index != 0 {
            json.push(',');
        }
        json.push_str(&serde_json::to_string(&key).map_err(E::custom)?);
        json.push(':');
        json.push_str(value.get());
    }
    json.push_str("}}");
    Ok(json)
}

impl<'de> Deserialize<'de> for PermissionResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let RawPermissionResultFields(fields) =
            RawPermissionResultFields::deserialize(deserializer)?;
        let mut behavior = None;
        let mut payload = Vec::with_capacity(fields.len().saturating_sub(1));
        for (key, value) in fields {
            if key == "behavior" {
                if behavior.is_some() {
                    return Err(<D::Error as serde::de::Error>::duplicate_field("behavior"));
                }
                behavior = Some(
                    serde_json::from_str::<String>(value.get())
                        .map_err(<D::Error as serde::de::Error>::custom)?,
                );
            } else {
                payload.push((key, value));
            }
        }
        let behavior =
            behavior.ok_or_else(|| <D::Error as serde::de::Error>::missing_field("behavior"))?;
        let wire_json = permission_result_wire_json::<D::Error>(&behavior, payload)?;
        let wire: PermissionResultWire =
            serde_json::from_str(&wire_json).map_err(<D::Error as serde::de::Error>::custom)?;
        Ok(wire.into())
    }
}

/// claude-code `decisionReason.circuitBreaker` — the safety circuit that raised
/// a `safetyCheck`. The complete 2.1.263 value set (grep of the binary for
/// `circuitBreaker:"…"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SafetyCircuitBreaker {
    /// `backgroundOperator` — the `&` background-operator allow→ask downgrade.
    BackgroundOperator,
    /// `dangerousRemoval` — `Dangerous ${cmd} operation …` (oracle `HL`).
    DangerousRemoval,
    /// `isolatePeerMachines`.
    IsolatePeerMachines,
    /// `outsideReadsBlocked` — `permissions.blockReadsOutsideWorkingDirectories`.
    OutsideReadsBlocked,
    /// `restrictedMode`.
    RestrictedMode,
    /// `suspiciousWindowsPath`.
    SuspiciousWindowsPath,
}

/// Discriminated reason for a permission decision.
///
/// Carries enough detail for audit logs and for the UI to explain the choice
/// (matched rule, mode fallback, classifier verdict, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionDecisionReason {
    /// A rule explicitly matched the call.
    MatchedRule {
        /// The matching rule.
        rule: PermissionRule,
    },
    /// The active mode produced the decision (no rule matched).
    PermissionMode {
        /// The mode in force.
        mode: PermissionMode,
    },
    /// A composite call (e.g. piped bash) — per-subcommand reasons.
    SubcommandResults {
        /// Subcommand results in producer order. Claude Code walks the native
        /// `Map` in insertion order and reports the first rule whose behavior
        /// matches the enclosing decision, so this must not be a hash map.
        #[serde(with = "raw_subcommand_results")]
        reasons: Vec<(Utf16JsonProjection, Box<PermissionResult>)>,
    },
    /// A custom external permission-prompt tool returned the decision.
    PermissionPromptTool {
        /// Name of the prompt tool that ran.
        tool_name: String,
    },
    /// A classifier approved the call.
    ClassifierApproved {
        /// Which classifier.
        classifier: ClassifierKind,
        /// Classifier confidence in `[0.0, 1.0]`.
        score: f64,
    },
    /// A classifier rejected the call.
    ClassifierRejected {
        /// Which classifier.
        classifier: ClassifierKind,
        /// Classifier confidence in `[0.0, 1.0]`.
        score: f64,
        /// Human-readable classifier explanation, forwarded as the oracle's
        /// `decisionReason.reason` text.
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
        reason: Utf16JsonProjection,
    },
    /// A `PreToolUse` hook overrode the rule-based decision.
    HookOverride {
        /// Hook identifier.
        hook_id: String,
        /// Configuration source that registered the hook (optional).
        source: Option<String>,
        /// Free-form reason from the hook (optional).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
            deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
        )]
        reason: Option<Utf16JsonProjection>,
    },
    /// An async sub-agent returned the decision asynchronously.
    AsyncAgent {
        /// Free-form reason from the agent.
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
        reason: Utf16JsonProjection,
    },
    /// The sandbox layer overrode the decision.
    SandboxOverride {
        /// Why the sandbox intervened.
        reason: SandboxOverrideReason,
    },
    /// A working-directory check produced the decision.
    WorkingDirectory {
        /// Free-form reason (e.g. "path escapes project root").
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
        reason: Utf16JsonProjection,
    },
    /// A static safety check produced the decision.
    SafetyCheck {
        /// Free-form reason.
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
        reason: Utf16JsonProjection,
        /// Whether a classifier could still approve this call.
        classifier_approvable: bool,
        /// claude-code `decisionReason.circuitBreaker` — which safety circuit
        /// produced this check. `None` for a check the oracle emits without one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        circuit_breaker: Option<SafetyCircuitBreaker>,
    },
    /// Anything not covered above.
    Other {
        /// Free-form reason.
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
        reason: Utf16JsonProjection,
    },
    /// Consecutive denials exceeded the configured limit.
    DenialLimitExceeded,
    /// `Auto` mode fell back to the parent mode.
    AutoModeFallback,
    /// `BypassPermissions` mode allowed the call.
    BypassPermissions,
}

/// Which classifier produced (or is producing) a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassifierKind {
    /// "Yolo" / auto-accept classifier.
    Yolo,
    /// Bash-command-aware classifier.
    Bash,
    /// Transcript-aware classifier.
    Transcript,
}

/// Why the sandbox overrode the rule-based decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxOverrideReason {
    /// The command is on the sandbox exclude list.
    ExcludedCommand,
    /// The user passed `--dangerously-disable-sandbox`.
    DangerouslyDisableSandbox,
}

/// Audit metadata shared across all `PermissionResult` variants.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionMetadata {
    /// Names of rules that contributed to the decision.
    /// These are raw root-string projections because a rule may contain
    /// arbitrary JavaScript UTF-16 units.
    #[serde(
        default,
        serialize_with = "raw_text_array::serialize",
        deserialize_with = "raw_text_array::deserialize"
    )]
    pub matched_rules: Vec<Utf16JsonProjection>,
    /// Raw `PermissionUpdate[]` suggestions offered with an Ask decision.
    /// Keeping the wire-shaped value here lets the lower permission engine
    /// retain tool-specific suggestions without depending on the CLI control
    /// protocol.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub permission_suggestions: Option<Utf16JsonProjection>,
    /// Concrete filesystem path that caused a path-scoped Ask, when the path
    /// validator can report it without parsing a human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
}

/// Prompt payload shown to the user when the decision is `Ask`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionPrompt {
    /// Short title for the prompt.
    pub title: String,
    /// Longer message body.
    #[serde(
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize"
    )]
    pub message: Utf16JsonProjection,
    /// Action options offered to the user.
    pub options: Vec<String>,
}

/// Raw JSON-array adapters for exact root-string text projections.
mod raw_text_array {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(values: &[Utf16JsonProjection], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let array =
            Utf16JsonProjection::array(values.to_vec()).map_err(serde::ser::Error::custom)?;
        lingxi_core::types::utf16_json::raw_json_serde::serialize(&array, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Utf16JsonProjection>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let array = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
        let Some(count) = array.array_len("") else {
            return Err(serde::de::Error::custom("matched_rules must be an array"));
        };
        (0..count)
            .map(|index| {
                let item = array
                    .subprojection(&format!("/{index}"))
                    .map_err(serde::de::Error::custom)?;
                if item.string_units("").is_none() {
                    return Err(serde::de::Error::custom(
                        "matched_rules entries must be strings",
                    ));
                }
                Ok(item)
            })
            .collect()
    }
}

/// Serialize the ordered subcommand-result projection as the native JSON
/// object while retaining exact JavaScript property-name units.
mod raw_subcommand_results {
    use super::PermissionResult;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(
        reasons: &Vec<(Utf16JsonProjection, Box<PermissionResult>)>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = Vec::with_capacity(reasons.len());
        for (command, result) in reasons {
            let key = command
                .string_units("")
                .ok_or_else(|| serde::ser::Error::custom("subcommand key must be a string"))?;
            let json = serde_json::to_string(result).map_err(serde::ser::Error::custom)?;
            let value = Utf16JsonProjection::parse(&json).map_err(serde::ser::Error::custom)?;
            entries.push((key, value));
        }
        let object =
            Utf16JsonProjection::object_units(entries).map_err(serde::ser::Error::custom)?;
        lingxi_core::types::utf16_json::raw_json_serde::serialize(&object, serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Vec<(Utf16JsonProjection, Box<PermissionResult>)>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let object = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
        let entries = object
            .at_pointer("")
            .and_then(|root| root.object_entries())
            .ok_or_else(|| serde::de::Error::custom("subcommand results must be an object"))?;
        entries
            .into_iter()
            .map(|entry| {
                let key = Utf16JsonProjection::root_string(
                    String::from_utf16_lossy(entry.code_units),
                    entry.code_units.to_vec(),
                )
                .map_err(serde::de::Error::custom)?;
                let value = object
                    .subprojection_property_units(entry.code_units)
                    .map_err(serde::de::Error::custom)?;
                let json = value.to_json_string().map_err(serde::de::Error::custom)?;
                let result = serde_json::from_str(&json).map_err(serde::de::Error::custom)?;
                Ok((key, Box::new(result)))
            })
            .collect()
    }
}

/// A classifier check that is still running when the decision is returned.
///
/// The host watches `request_id` and resolves the prompt when the classifier
/// reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingClassifierCheck {
    /// Which classifier is running.
    pub classifier: ClassifierKind,
    /// The async request identifier.
    pub request_id: RequestId,
    /// When the classifier started.
    pub started_at: SystemTime,
}

/// Where to persist a permission update derived from this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionUpdateDestination {
    /// `~/.lingxi/settings.json`.
    UserSettings,
    /// `.lingxi/settings.json` (project-checked).
    ProjectSettings,
    /// `.lingxi/settings.local.json` (gitignored).
    LocalSettings,
    /// Session-only (lives until the agent exits).
    Session,
    /// Treat the rule as if it came from the CLI.
    CliArg,
}

#[cfg(test)]
mod source_tests {
    use super::PermissionMetadata;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    #[test]
    fn permission_result_raw_carrier_preserves_absence_null_and_public_tag_order() {
        let absent: super::PermissionResult = serde_json::from_str(
            r#"{"behavior":"allow","reason":{"Other":{"reason":"ordinary"}},"metadata":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            &absent,
            super::PermissionResult::Allow {
                updated_input: None,
                update_destination: None,
                ..
            }
        ));
        let absent_wire = serde_json::to_string(&absent).unwrap();
        assert_eq!(
            absent_wire,
            r#"{"behavior":"allow","reason":{"Other":{"reason":"ordinary"}},"update_destination":null,"metadata":{"matched_rules":[]}}"#
        );
        let absent_round_trip: super::PermissionResult =
            serde_json::from_str(&absent_wire).unwrap();
        assert!(matches!(
            absent_round_trip,
            super::PermissionResult::Allow {
                updated_input: None,
                ..
            }
        ));

        let explicit_null: super::PermissionResult = serde_json::from_str(
            r#"{"behavior":"allow","reason":{"Other":{"reason":"ordinary"}},"updated_input":null,"metadata":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            &explicit_null,
            super::PermissionResult::Allow {
                updated_input: Some(value),
                ..
            } if value.at_pointer("").is_some_and(|value| value.is_null())
        ));
        let explicit_null_wire = serde_json::to_string(&explicit_null).unwrap();
        assert_eq!(
            explicit_null_wire,
            r#"{"behavior":"allow","reason":{"Other":{"reason":"ordinary"}},"updated_input":null,"update_destination":null,"metadata":{"matched_rules":[]}}"#
        );
        let explicit_null_round_trip: super::PermissionResult =
            serde_json::from_str(&explicit_null_wire).unwrap();
        assert!(matches!(
            explicit_null_round_trip,
            super::PermissionResult::Allow {
                updated_input: Some(value),
                ..
            } if value.at_pointer("").is_some_and(|value| value.is_null())
        ));
    }

    #[test]
    fn permission_result_raw_carrier_does_not_overwrite_duplicate_fields() {
        let error = serde_json::from_str::<super::PermissionResult>(
            r#"{"behavior":"allow","reason":{"Other":{"reason":"ordinary"}},"metadata":{},"metadata":{}}"#,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("duplicate field `metadata`"),
            "{error}"
        );
    }

    #[test]
    fn permission_suggestions_serialize_as_the_original_json_tree() {
        let suggestions = Utf16JsonProjection::parse(r#"[{"kind":"ask","detail":"\ud800"}]"#)
            .expect("test source is valid JSON");
        let metadata = PermissionMetadata {
            permission_suggestions: Some(suggestions.clone()),
            ..PermissionMetadata::default()
        };
        let wire = serde_json::to_string(&metadata).unwrap();
        assert!(wire.starts_with(
            r#"{"matched_rules":[],"permission_suggestions":[{"kind":"ask","detail":"\ud800"}"#
        ));
        let decoded: PermissionMetadata = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded.permission_suggestions, Some(suggestions));
    }

    #[test]
    fn matched_rule_projection_round_trips_as_a_json_string_array() {
        let rule =
            Utf16JsonProjection::parse(r#""Bash(\ud800)""#).expect("test source is valid JSON");
        let metadata = PermissionMetadata {
            matched_rules: vec![rule.clone()],
            ..PermissionMetadata::default()
        };
        let wire = serde_json::to_string(&metadata).unwrap();
        assert!(wire.contains(r#""matched_rules":["Bash(\ud800)"]"#));
        let decoded: PermissionMetadata = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded.matched_rules, vec![rule]);
    }

    #[test]
    fn permission_prompt_message_is_serialized_as_a_json_string() {
        let message =
            Utf16JsonProjection::parse(r#""ask \ud800""#).expect("test source is valid JSON");
        let prompt = super::PermissionPrompt {
            title: "Ask".into(),
            message: message.clone(),
            options: vec!["Allow".into()],
        };
        let wire = serde_json::to_string(&prompt).unwrap();
        assert!(wire.contains(r#""message":"ask \ud800""#));
        let decoded: super::PermissionPrompt = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded.message, message);
    }

    #[test]
    fn denial_explanation_keeps_exact_source_text_units() {
        let explanation = Utf16JsonProjection::parse(r#""blocked \ud800 target""#)
            .expect("deny explanation is valid JSON source text");
        let result = super::PermissionResult::Deny {
            reason: super::PermissionDecisionReason::Other {
                reason: "blocked".into(),
            },
            explanation: Some(explanation.clone()),
            metadata: super::PermissionMetadata::default(),
        };
        let wire = serde_json::to_string(&result).unwrap();
        assert!(wire.contains(r#"blocked \ud800 target"#));
        let decoded: super::PermissionResult = serde_json::from_str(&wire).unwrap();
        let super::PermissionResult::Deny {
            explanation: Some(decoded),
            ..
        } = decoded
        else {
            panic!("deny explanation remains present");
        };
        assert_eq!(decoded, explanation);
    }

    #[test]
    fn ordered_subcommand_result_keys_keep_exact_utf16_units() {
        let key = Utf16JsonProjection::parse(r#""command-\ud800""#)
            .expect("subcommand key is valid JSON source text");
        let reason = super::PermissionDecisionReason::SubcommandResults {
            reasons: vec![(
                key.clone(),
                Box::new(super::PermissionResult::Allow {
                    reason: super::PermissionDecisionReason::Other {
                        reason: "Allowed".into(),
                    },
                    updated_input: Some(
                        Utf16JsonProjection::parse(r#"{"command":"\ud800"}"#)
                            .expect("nested rewritten input preserves raw UTF-16 JSON"),
                    ),
                    update_destination: None,
                    metadata: super::PermissionMetadata::default(),
                }),
            )],
        };
        let wire = serde_json::to_string(&reason).unwrap();
        assert!(wire.contains(r#"command-\ud800"#));
        let decoded: super::PermissionDecisionReason = serde_json::from_str(&wire).unwrap();
        let super::PermissionDecisionReason::SubcommandResults { reasons } = decoded else {
            panic!("subcommand reason retains its variant");
        };
        assert_eq!(reasons[0].0.string_units(""), key.string_units(""));
        let super::PermissionResult::Allow {
            updated_input: Some(input),
            ..
        } = reasons[0].1.as_ref()
        else {
            panic!("nested permission result keeps its raw rewritten input");
        };
        assert_eq!(input.string_units("/command"), Some(vec![0xd800]));
    }
}
