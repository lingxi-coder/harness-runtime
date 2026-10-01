//! Local App generated-MCP validation and ceiling derivation helpers.

use crate::{
    allowed_for_synchronous_flow, AppError, AppLayout, CapabilityId, CapabilityRegistry,
    CapabilityTransport, FlowDefinition, APPS_SCHEMA_VERSION,
};
use mcp_wire::{McpPermissionCeiling, McpToolDefinitionDto};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Maximum generated Local App tools allowed in one published catalog.
pub const MAX_GENERATED_MCP_TOOLS: usize = 16;
/// Maximum bytes in one generated tool schema.
pub const MAX_GENERATED_MCP_SCHEMA_BYTES: usize = 32 * 1024;
/// Maximum bytes in one generated structured result payload.
pub const MAX_GENERATED_MCP_STRUCTURED_RESULT_BYTES: usize = 64 * 1024;
/// Maximum UTF-16 units in one provider-visible generated tool definition.
pub const MAX_GENERATED_MCP_DEFINITION_UTF16: usize = 5_120;
/// Maximum estimated tokens in one provider-visible generated tool definition.
pub const MAX_GENERATED_MCP_DEFINITION_TOKENS: usize = 2_048;
/// Maximum UTF-16 units across the exposed generated-tool set.
pub const MAX_EXPOSED_MCP_DEFINITIONS_UTF16: usize = 40_960;
/// Maximum estimated tokens across the exposed generated-tool set.
pub const MAX_EXPOSED_MCP_DEFINITIONS_TOKENS: usize = 16_384;
const MAX_SCHEMA_DEPTH: usize = 8;
const MAX_TOOL_TITLE_CHARS: usize = 100;
const MAX_TOOL_DESCRIPTION_CHARS: usize = 1_000;
const MAX_COMBINATOR_VARIANTS: usize = 16;

/// Maximum number of steps in an MCP-bound flow (separate from the more
/// general background-flow limit).
pub const MAX_MCP_FLOW_STEPS: usize = 32;
/// Maximum UTF-8 bytes in a JSON pointer and maximum pointer segments.
pub const MAX_MCP_POINTER_BYTES: usize = 512;
pub const MAX_MCP_POINTER_SEGMENTS: usize = 32;
/// Maximum nesting depth accepted while resolving a binding.
pub const MAX_MCP_BINDING_DEPTH: usize = 8;
/// Maximum intermediate step result and complete call payload.
pub const MAX_MCP_STEP_RESULT_BYTES: usize = 64 * 1024;
pub const MAX_MCP_CALL_BYTES: usize = 256 * 1024;

/// A value supplied to an app MCP flow without executable interpolation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowValueBinding {
    Literal(Value),
    ToolInput {
        json_pointer: String,
    },
    StepOutput {
        step_id: String,
        json_pointer: String,
    },
}

/// Host-validated binding from a proposed MCP tool to a declarative flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppMcpFlowBinding {
    pub flow_id: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, FlowValueBinding>,
    pub result: FlowValueBinding,
}

/// Flow and schemas re-read by the Host when validating a binding. The
/// proposal carries only semantic references; source/path identity is never
/// trusted from the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppMcpFlowContext {
    pub app_id: String,
    pub source: FlowSource,
    pub flow: FlowDefinition,
    pub input_schema: Value,
    pub output_schema: Value,
    #[serde(default)]
    pub step_output_schemas: BTreeMap<String, Value>,
}

/// Whether the Host read a flow from the isolated create staging area or the
/// currently active app revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowSource {
    Staging,
    Active,
}

/// A tool proposal is intentionally smaller than an MCP wire definition.
/// Host-owned annotations, execution metadata, icons, permission ceilings,
/// server identity and catalog references cannot be supplied by an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppMcpToolProposal {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub semantic_flow_id: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, FlowValueBinding>,
    pub result: FlowValueBinding,
}

/// Agent-owned part of the Local App MCP authoring contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppMcpProposal {
    pub app_id: String,
    pub manifest_revision: u64,
    pub user_goal_sha256: String,
    pub summary: String,
    #[serde(default)]
    pub tools: Vec<AppMcpToolProposal>,
    #[serde(default)]
    pub required_flow_changes: Vec<String>,
    #[serde(default)]
    pub excluded_capabilities: Vec<String>,
}

/// Host-derived information for one validated proposal tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostValidatedMcpTool {
    pub definition: McpToolDefinitionDto,
    pub flow: AppMcpFlowBinding,
    pub ceiling: McpPermissionCeiling,
}

/// Proposal after Host schema/Flow validation and canonical identity
/// derivation. Only this representation may enter approval or candidate QA.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedAppMcpProposal {
    pub proposal: AppMcpProposal,
    pub tools: Vec<HostValidatedMcpTool>,
    pub proposal_sha256: String,
    pub tool_surface_sha256: String,
}

/// Durable stages of one MCP candidate transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthoringStage {
    Prepared,
    Approved,
    Built,
    SmokePassed,
    McpVerified,
    Promoted,
}

impl McpAuthoringStage {
    fn next(self) -> Option<Self> {
        Some(match self {
            Self::Prepared => Self::Approved,
            Self::Approved => Self::Built,
            Self::Built => Self::SmokePassed,
            Self::SmokePassed => Self::McpVerified,
            Self::McpVerified => Self::Promoted,
            Self::Promoted => return None,
        })
    }
}

/// Restart-safe candidate journal. `integrity_sha256` seals every field
/// except itself, so damaged or hand-edited state is rejected before resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCandidateJournal {
    pub schema_version: u32,
    pub app_id: String,
    pub workflow_run_id: String,
    pub stage: McpAuthoringStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_build_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_catalog_sha256: Option<String>,
    pub proposal_sha256: String,
    pub approval_contract_sha256: String,
    pub tool_surface_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_sha256: Option<String>,
    pub integrity_sha256: String,
}

/// Confirmation receipt shared by initial create and standalone MCP revise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfirmationReceipt {
    pub receipt_id: String,
    pub app_id: String,
    pub workflow_run_id: String,
    pub approval_contract_sha256: String,
    pub candidate_digest: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(default)]
    pub claimed: bool,
    #[serde(default)]
    pub consumed: bool,
    #[serde(default)]
    pub superseded: bool,
}

/// In-memory view of the Host's durable per-App receipt slots. Persisting the
/// candidate journal makes this book reconstructible after a restart; issuing
/// a new receipt always invalidates the previous slot before insertion.
#[derive(Debug, Default)]
pub struct McpReceiptBook {
    slots: BTreeMap<String, McpConfirmationReceipt>,
}

impl McpReceiptBook {
    /// Issue one receipt, superseding any outstanding receipt for this app.
    pub fn issue(&mut self, receipt: McpConfirmationReceipt) -> Result<(), GeneratedMcpIssue> {
        // `receipt.issued_at_ms` IS "now" — every caller mints it via
        // `McpConfirmationReceipt::new(..., now_ms())` immediately before
        // calling `issue` — so a claimed-but-never-released slot (a scaffold
        // whose future was dropped by a Stop, a panic, or the 529
        // non-streaming fallback, before `ReceiptClaim`'s `Drop` guard could
        // release it — or before that guard existed) self-clears once its TTL
        // has passed instead of wedging the app until the process restarts.
        //
        // r3-engine-core-5: `slots` had no eviction at all, so every receipt
        // ever issued — consumed, superseded, or simply abandoned — lived for
        // the process lifetime. `claim_candidate` and `consume` both already
        // refuse an expired receipt (`now_ms >= expires_at_ms`), so a slot
        // past its TTL can never again succeed at anything; only the
        // `receipt_duplicate` id-collision scan below still walks it. Drop
        // those slots here, before that scan runs, bounding the book to
        // receipts issued within the last TTL window instead of forever.
        self.slots
            .retain(|_, existing| existing.expires_at_ms > receipt.issued_at_ms);
        if self.slots.values().any(|existing| {
            existing.app_id == receipt.app_id
                && existing.claimed
                && !existing.consumed
                && receipt.issued_at_ms < existing.expires_at_ms
        }) {
            return Err(binding_issue(
                "receipt_in_use",
                "an MCP receipt for this app is already in use",
            )[0]
            .clone());
        }
        if self.slots.values().any(|existing| {
            existing.app_id == receipt.app_id && existing.receipt_id == receipt.receipt_id
        }) {
            return Err(
                binding_issue("receipt_duplicate", "receipt id is already present")[0].clone(),
            );
        }
        for existing in self.slots.values_mut() {
            if existing.app_id == receipt.app_id && !existing.consumed {
                existing.superseded = true;
            }
        }
        self.slots.insert(receipt.receipt_id.clone(), receipt);
        Ok(())
    }

    /// Consume a receipt by opaque id after all app/run/contract checks.
    pub fn consume(
        &mut self,
        receipt_id: &str,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        now_ms: u64,
    ) -> Result<(), GeneratedMcpIssue> {
        let receipt = self.slots.get_mut(receipt_id).ok_or_else(|| {
            binding_issue(
                "receipt_missing",
                "MCP receipt is unknown or was not issued",
            )[0]
            .clone()
        })?;
        receipt.consume(app_id, workflow_run_id, approval_digest, now_ms)
    }

    /// Promotion variant that verifies the candidate digest before consuming.
    pub fn consume_candidate(
        &mut self,
        receipt_id: &str,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        candidate_digest: &str,
        now_ms: u64,
    ) -> Result<(), GeneratedMcpIssue> {
        let receipt = self.slots.get_mut(receipt_id).ok_or_else(|| {
            binding_issue(
                "receipt_missing",
                "MCP receipt is unknown or was not issued",
            )[0]
            .clone()
        })?;
        receipt.consume_candidate(
            app_id,
            workflow_run_id,
            approval_digest,
            candidate_digest,
            now_ms,
        )
    }
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut ordered = BTreeMap::new();
            for (key, value) in object {
                ordered.insert(key, canonicalize(value));
            }
            Value::Object(ordered.into_iter().collect::<Map<_, _>>())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        value => value,
    }
}

fn digest_value(value: Value, context: &str) -> Result<String, GeneratedMcpIssue> {
    let bytes = serde_json::to_vec(&canonicalize(value)).map_err(|error| GeneratedMcpIssue {
        tool_name: None,
        code: "canonicalization_failed",
        message: format!("{context} cannot be canonicalized: {error}"),
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn pointer_segments(pointer: &str) -> Result<Vec<String>, GeneratedMcpIssue> {
    if pointer.len() > MAX_MCP_POINTER_BYTES || !pointer.is_char_boundary(pointer.len()) {
        return Err(GeneratedMcpIssue {
            tool_name: None,
            code: "pointer_too_large",
            message: "JSON pointer exceeds 512 UTF-8 bytes".into(),
        });
    }
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(GeneratedMcpIssue {
            tool_name: None,
            code: "pointer_invalid",
            message: "JSON pointer must be empty or start with /".into(),
        });
    }
    let segments = pointer
        .split('/')
        .skip(1)
        .map(|segment| {
            let mut out = String::with_capacity(segment.len());
            let mut chars = segment.chars();
            while let Some(ch) = chars.next() {
                if ch == '~' {
                    match chars.next() {
                        Some('0') => out.push('~'),
                        Some('1') => out.push('/'),
                        _ => return Err(()),
                    }
                } else {
                    out.push(ch);
                }
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| GeneratedMcpIssue {
            tool_name: None,
            code: "pointer_invalid",
            message: "JSON pointer contains an invalid escape".into(),
        })?;
    if segments.len() > MAX_MCP_POINTER_SEGMENTS
        || segments
            .iter()
            .any(|s| s.contains('{') || s.contains('}') || s == "*")
    {
        return Err(GeneratedMcpIssue {
            tool_name: None,
            code: "pointer_dynamic",
            message: "dynamic JSON pointers are forbidden".into(),
        });
    }
    if segments.len() > MAX_MCP_BINDING_DEPTH {
        return Err(GeneratedMcpIssue {
            tool_name: None,
            code: "pointer_depth_exceeded",
            message: "JSON pointer exceeds binding depth 8".into(),
        });
    }
    Ok(segments)
}

fn schema_at<'a>(schema: &'a Value, segments: &[String]) -> Option<&'a Value> {
    let mut node = schema;
    for segment in segments {
        if let Some(properties) = node.get("properties").and_then(Value::as_object) {
            node = properties.get(segment)?;
        } else if let Some(items) = node.get("items") {
            let index = segment.parse::<usize>().ok()?;
            node = items;
            if index > 0 {
                return None;
            }
        } else {
            return None;
        }
    }
    Some(node)
}

/// Check a bounded generated MCP value against its Host-validated JSON
/// Schema. This is intentionally the same small matcher used when validating
/// literal Flow results, so runtime calls do not acquire a second schema
/// implementation or dependency.
pub fn value_matches_schema(value: &Value, schema: &Value) -> bool {
    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        if !enum_values.iter().any(|candidate| candidate == value) {
            return false;
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(object) = value.as_object() else {
                return false;
            };
            if schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|key| key.as_str().is_none_or(|key| !object.contains_key(key)))
                })
            {
                return false;
            }
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                let properties = schema.get("properties").and_then(Value::as_object);
                if object
                    .keys()
                    .any(|key| properties.is_none_or(|properties| !properties.contains_key(key)))
                {
                    return false;
                }
            }
            schema
                .get("properties")
                .and_then(Value::as_object)
                .is_none_or(|properties| {
                    properties.iter().all(|(key, child)| {
                        object
                            .get(key)
                            .is_none_or(|value| value_matches_schema(value, child))
                    })
                })
        }
        Some("array") => value.as_array().is_some_and(|items| {
            schema.get("items").is_none_or(|schema| {
                items
                    .iter()
                    .all(|value| value_matches_schema(value, schema))
            })
        }),
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        Some(_) => false,
        None => true,
    }
}

fn binding_issue(code: &'static str, message: impl Into<String>) -> Vec<GeneratedMcpIssue> {
    vec![GeneratedMcpIssue {
        tool_name: None,
        code,
        message: message.into(),
    }]
}

fn validate_pointer(
    schema: &Value,
    pointer: &str,
    label: &str,
) -> Result<(), Vec<GeneratedMcpIssue>> {
    let segments = pointer_segments(pointer).map_err(|issue| vec![issue])?;
    if schema_at(schema, &segments).is_none() {
        return Err(binding_issue(
            "pointer_not_in_schema",
            format!("{label} pointer {pointer:?} is not present in its schema"),
        ));
    }
    Ok(())
}

fn forbidden_mcp_capability(capability: CapabilityId) -> bool {
    matches!(
        capability,
        CapabilityId::FlowExecute
            | CapabilityId::LlmComplete
            | CapabilityId::LlmStream
            | CapabilityId::AgentSessionCreate
            | CapabilityId::AgentSessionList
            | CapabilityId::AgentSessionResume
            | CapabilityId::AgentSessionClose
            | CapabilityId::AgentSend
            | CapabilityId::AgentStream
            | CapabilityId::AgentCancel
            | CapabilityId::AgentEmit
            | CapabilityId::AgentProfilePropose
            | CapabilityId::BackgroundSchedule
    )
}

/// Validate a semantic MCP binding against Host-reloaded flow and schemas.
pub fn validate_app_mcp_flow_binding(
    binding: &AppMcpFlowBinding,
    expected_app_id: &str,
    context: &AppMcpFlowContext,
    registry: &CapabilityRegistry,
) -> Result<(), Vec<GeneratedMcpIssue>> {
    if context.app_id != expected_app_id {
        return Err(binding_issue(
            "cross_app_flow",
            "MCP flow and binding must belong to the same app",
        ));
    }
    if binding.flow_id != context.flow.flow_id {
        return Err(binding_issue(
            "flow_not_found",
            "binding flow_id does not match the Host-reloaded flow",
        ));
    }
    if context.flow.steps.len() > MAX_MCP_FLOW_STEPS {
        return Err(binding_issue(
            "flow_step_limit",
            "MCP-bound flow exceeds 32 steps",
        ));
    }
    if context.flow.validate(registry).is_err() {
        return Err(binding_issue(
            "flow_invalid",
            "Host-reloaded flow failed declarative validation",
        ));
    }
    for step in &context.flow.steps {
        if forbidden_mcp_capability(step.capability) {
            return Err(binding_issue(
                "forbidden_capability",
                format!("{} cannot be used by an MCP flow", step.capability.as_str()),
            ));
        }
        if step.input_json.len() > MAX_MCP_STEP_RESULT_BYTES {
            return Err(binding_issue(
                "step_payload_limit",
                "flow step input exceeds 64 KiB",
            ));
        }
    }
    let mut input_bytes = 0usize;
    for value in binding.inputs.values() {
        match value {
            FlowValueBinding::Literal(value) => {
                input_bytes = input_bytes.saturating_add(
                    serde_json::to_vec(value)
                        .map_err(|_| {
                            binding_issue("literal_invalid", "literal is not serializable")
                        })?
                        .len(),
                );
            }
            FlowValueBinding::ToolInput { json_pointer } => {
                validate_pointer(&context.input_schema, json_pointer, "tool input")?
            }
            FlowValueBinding::StepOutput {
                step_id,
                json_pointer,
            } => {
                context
                    .flow
                    .steps
                    .iter()
                    .position(|step| step.step_id == *step_id)
                    .ok_or_else(|| {
                        binding_issue("step_not_found", "StepOutput references an unknown step")
                    })?;
                let schema = context.step_output_schemas.get(step_id).ok_or_else(|| {
                    binding_issue(
                        "step_schema_missing",
                        "Host did not provide the referenced step output schema",
                    )
                })?;
                validate_pointer(schema, json_pointer, "step output")?;
            }
        }
    }
    match &binding.result {
        FlowValueBinding::Literal(value) => {
            if serde_json::to_vec(value)
                .map_err(|_| binding_issue("result_invalid", "result literal is not serializable"))?
                .len()
                > MAX_MCP_STEP_RESULT_BYTES
            {
                return Err(binding_issue("step_payload_limit", "result exceeds 64 KiB"));
            }
            if !value_matches_schema(value, &context.output_schema) {
                return Err(binding_issue(
                    "output_schema_mismatch",
                    "result literal does not satisfy the tool output schema",
                ));
            }
        }
        FlowValueBinding::ToolInput { json_pointer } => {
            validate_pointer(&context.input_schema, json_pointer, "result tool input")?
        }
        FlowValueBinding::StepOutput {
            step_id,
            json_pointer,
        } => {
            context
                .flow
                .steps
                .iter()
                .position(|step| step.step_id == *step_id)
                .ok_or_else(|| {
                    binding_issue("step_not_found", "result references an unknown step")
                })?;
            let schema = context.step_output_schemas.get(step_id).ok_or_else(|| {
                binding_issue("step_schema_missing", "result output schema is unavailable")
            })?;
            validate_pointer(schema, json_pointer, "result")?;
            // `result` is the selected value, not the step's enclosing
            // object. The selected value is checked against the tool's
            // outputSchema after materialization; requiring the same JSON
            // pointer path in outputSchema would incorrectly reject the
            // common `{step: {result: ...}} -> {result: ...}` projection.
        }
    }
    if input_bytes > MAX_MCP_CALL_BYTES {
        return Err(binding_issue(
            "call_payload_limit",
            "MCP call payload exceeds 256 KiB",
        ));
    }
    Ok(())
}

/// Variant used by a Host scheduler while validating a binding for a specific
/// consumer step. It makes the “StepOutput must be prior” rule explicit even
/// though the compact persisted binding format only stores the semantic ref.
pub fn validate_app_mcp_flow_binding_for_consumer(
    binding: &AppMcpFlowBinding,
    consumer_step_id: &str,
    expected_app_id: &str,
    context: &AppMcpFlowContext,
    registry: &CapabilityRegistry,
) -> Result<(), Vec<GeneratedMcpIssue>> {
    validate_app_mcp_flow_binding(binding, expected_app_id, context, registry)?;
    let consumer_index = context
        .flow
        .steps
        .iter()
        .position(|step| step.step_id == consumer_step_id)
        .ok_or_else(|| {
            binding_issue(
                "step_not_found",
                "consumer step is not in the Host-reloaded flow",
            )
        })?;
    let check = |value: &FlowValueBinding| -> Result<(), Vec<GeneratedMcpIssue>> {
        if let FlowValueBinding::StepOutput { step_id, .. } = value {
            let source_index = context
                .flow
                .steps
                .iter()
                .position(|step| step.step_id == *step_id)
                .ok_or_else(|| {
                    binding_issue("step_not_found", "StepOutput references an unknown step")
                })?;
            if source_index >= consumer_index {
                return Err(binding_issue(
                    "forward_step_output",
                    "StepOutput must reference a prior step",
                ));
            }
        }
        Ok(())
    };
    for value in binding.inputs.values() {
        check(value)?;
    }
    check(&binding.result)
}

/// Resolve one Host-validated binding against the current tool input and the
/// outputs of already completed flow steps. The binding tree is deliberately
/// data-only: no interpolation, expression, or executable value can enter a
/// step through this adapter.
pub fn materialize_flow_value_binding(
    binding: &FlowValueBinding,
    tool_input: &Value,
    step_outputs: &BTreeMap<String, Value>,
) -> Result<Value, GeneratedMcpIssue> {
    let resolve = |value: &Value, pointer: &str, label: &str| {
        pointer_segments(pointer)?;
        value
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| GeneratedMcpIssue {
                tool_name: None,
                code: "binding_value_missing",
                message: format!("{label} pointer {pointer:?} did not resolve to a value"),
            })
    };
    match binding {
        FlowValueBinding::Literal(value) => Ok(value.clone()),
        FlowValueBinding::ToolInput { json_pointer } => {
            resolve(tool_input, json_pointer, "tool input")
        }
        FlowValueBinding::StepOutput {
            step_id,
            json_pointer,
        } => {
            let output = step_outputs.get(step_id).ok_or_else(|| GeneratedMcpIssue {
                tool_name: None,
                code: "step_output_missing",
                message: format!("StepOutput references incomplete step {step_id:?}"),
            })?;
            resolve(output, json_pointer, "step output")
        }
    }
}

impl AppMcpProposal {
    /// Validate agent-owned shape before Host derives any execution metadata.
    pub fn validate_shape(
        &self,
        expected_app_id: &str,
        expected_manifest_revision: u64,
    ) -> Result<(), Vec<GeneratedMcpIssue>> {
        if self.app_id != expected_app_id {
            return Err(binding_issue(
                "cross_app_proposal",
                "proposal app_id does not match Host app",
            ));
        }
        if self.manifest_revision != expected_manifest_revision {
            return Err(binding_issue(
                "manifest_revision_stale",
                "proposal manifest revision is stale",
            ));
        }
        if !valid_digest(&self.user_goal_sha256) {
            return Err(binding_issue(
                "user_goal_digest_invalid",
                "user_goal_sha256 must be lowercase SHA-256",
            ));
        }
        if self.summary.trim().is_empty() || self.summary.chars().count() > 2_000 {
            return Err(binding_issue(
                "summary_invalid",
                "proposal summary is empty or too large",
            ));
        }
        if self.tools.len() > MAX_GENERATED_MCP_TOOLS {
            return Err(binding_issue(
                "tool_count_exceeded",
                "proposal exceeds 16 tools",
            ));
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if !names.insert(tool.name.as_str()) {
                return Err(binding_issue(
                    "duplicate_tool_name",
                    "proposal contains duplicate tool names",
                ));
            }
            let candidate = McpToolDefinitionDto::new(tool.name.clone(), tool.input_schema.clone());
            let mut candidate = candidate;
            candidate.title = tool.title.clone();
            candidate.description = tool.description.clone();
            candidate.output_schema = tool.output_schema.clone();
            validate_generated_mcp_catalog(&[candidate])?;
        }
        Ok(())
    }
}

/// Host validation and derivation entry point. `contexts` is read from active
/// or isolated staging state and is never populated from proposal paths.
pub fn validate_app_mcp_proposal(
    proposal: AppMcpProposal,
    expected_app_id: &str,
    expected_manifest_revision: u64,
    contexts: &BTreeMap<String, AppMcpFlowContext>,
    registry: &CapabilityRegistry,
) -> Result<ValidatedAppMcpProposal, Vec<GeneratedMcpIssue>> {
    proposal.validate_shape(expected_app_id, expected_manifest_revision)?;
    let mut tools = Vec::with_capacity(proposal.tools.len());
    let mut definitions = Vec::with_capacity(proposal.tools.len());
    for tool in &proposal.tools {
        let context = contexts.get(&tool.semantic_flow_id).ok_or_else(|| {
            binding_issue(
                "flow_not_found",
                "Host could not resolve semantic flow reference",
            )
        })?;
        let flow = AppMcpFlowBinding {
            flow_id: context.flow.flow_id.clone(),
            inputs: tool.inputs.clone(),
            result: tool.result.clone(),
        };
        validate_app_mcp_flow_binding(&flow, expected_app_id, context, registry)?;
        let mut definition =
            McpToolDefinitionDto::new(tool.name.clone(), tool.input_schema.clone());
        definition.title = tool.title.clone();
        definition.description = tool.description.clone();
        definition.output_schema = tool.output_schema.clone();
        let context_flow = &context.flow;
        let ceiling = derive_local_app_mcp_ceiling(registry, context_flow);
        if ceiling == McpPermissionCeiling::Deny {
            return Err(binding_issue(
                "permission_ceiling",
                "Host-derived MCP permission ceiling denies this flow",
            ));
        }
        definitions.push(definition.clone());
        tools.push(HostValidatedMcpTool {
            definition,
            flow,
            ceiling,
        });
    }
    let proposal_value = serde_json::to_value(&proposal)
        .map_err(|_| binding_issue("proposal_invalid", "proposal cannot be serialized"))?;
    let proposal_sha256 = digest_value(proposal_value, "proposal").map_err(|issue| vec![issue])?;
    let definitions_value = serde_json::to_value(&definitions)
        .map_err(|_| binding_issue("proposal_invalid", "tool surface cannot be serialized"))?;
    let tool_surface_sha256 =
        digest_value(definitions_value, "tool surface").map_err(|issue| vec![issue])?;
    Ok(ValidatedAppMcpProposal {
        proposal,
        tools,
        proposal_sha256,
        tool_surface_sha256,
    })
}

/// Digest the user-visible approval contract. Source bytes and final build ids
/// are intentionally excluded so deterministic repair can reuse approval.
pub fn approval_contract_sha256(review_surface: Value) -> Result<String, GeneratedMcpIssue> {
    digest_value(review_surface, "approval contract")
}

/// Digest an immutable catalog including Host execution bindings and build id.
pub fn catalog_sha256(
    validated: &ValidatedAppMcpProposal,
    build_id: &str,
    execution_bindings: Value,
) -> Result<String, GeneratedMcpIssue> {
    digest_value(
        serde_json::json!({
            "appId": validated.proposal.app_id,
            "buildId": build_id,
            "proposal": validated.proposal,
            "tools": validated.tools.iter().map(|tool| serde_json::json!({
                "definition": tool.definition,
                "flow": tool.flow,
                "ceiling": tool.ceiling,
            })).collect::<Vec<_>>(),
            "execution": execution_bindings,
        }),
        "catalog",
    )
}

impl McpCandidateJournal {
    /// Seal a journal after each durable stage transition.
    pub fn seal(mut self) -> Result<Self, GeneratedMcpIssue> {
        if self.schema_version == 0
            || self.app_id.trim().is_empty()
            || self.workflow_run_id.trim().is_empty()
            || !valid_digest(&self.proposal_sha256)
            || !valid_digest(&self.approval_contract_sha256)
            || !valid_digest(&self.tool_surface_sha256)
            || self
                .catalog_sha256
                .as_deref()
                .is_some_and(|digest| !valid_digest(digest))
            || self
                .previous_catalog_sha256
                .as_deref()
                .is_some_and(|digest| !valid_digest(digest))
        {
            return Err(binding_issue(
                "journal_invalid",
                "candidate journal identity or digest is invalid",
            )[0]
            .clone());
        }
        self.integrity_sha256.clear();
        let bytes = serde_json::to_value(&self).map_err(|_| {
            binding_issue("journal_invalid", "journal is not serializable")[0].clone()
        })?;
        self.integrity_sha256 = digest_value(bytes, "journal")?;
        Ok(self)
    }

    pub fn verify(&self) -> Result<(), GeneratedMcpIssue> {
        if !valid_digest(&self.proposal_sha256)
            || !valid_digest(&self.approval_contract_sha256)
            || !valid_digest(&self.tool_surface_sha256)
        {
            return Err(
                binding_issue("journal_invalid", "candidate journal digest is invalid")[0].clone(),
            );
        }
        let mut unsigned = self.clone();
        unsigned.integrity_sha256.clear();
        let expected = digest_value(
            serde_json::to_value(unsigned).map_err(|_| {
                binding_issue("journal_invalid", "journal is not serializable")[0].clone()
            })?,
            "journal",
        )?;
        if expected != self.integrity_sha256 {
            return Err(binding_issue(
                "journal_tampered",
                "candidate journal integrity check failed",
            )[0]
            .clone());
        }
        Ok(())
    }

    /// Advance only one stage at a time, preserving durable crash boundaries.
    pub fn advance(mut self, stage: McpAuthoringStage) -> Result<Self, GeneratedMcpIssue> {
        self.verify()?;
        if self.stage.next() != Some(stage) {
            return Err(binding_issue(
                "journal_transition_invalid",
                "candidate journal stage transition is not sequential",
            )[0]
            .clone());
        }
        self.stage = stage;
        self.seal()
    }

    /// Approval remains valid across source/build-only repair when the
    /// user-visible contract is unchanged. Any review-surface digest change
    /// requires a new receipt.
    #[must_use]
    pub fn approval_reusable(&self, approval_contract_sha256: &str) -> bool {
        self.stage >= McpAuthoringStage::Approved
            && self.approval_contract_sha256 == approval_contract_sha256
    }
}

/// Atomically persist a sealed candidate journal in app-private Host data.
pub fn save_candidate_journal(
    layout: &AppLayout,
    journal: &McpCandidateJournal,
) -> Result<(), AppError> {
    journal
        .verify()
        .map_err(|issue| AppError::StorageCorrupt(issue.message))?;
    if journal.schema_version != APPS_SCHEMA_VERSION || journal.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(
            "candidate journal schema or app binding is invalid".into(),
        ));
    }
    layout.initialize()?;
    let mut body = serde_json::to_vec_pretty(journal)
        .map_err(|error| AppError::Io(format!("serialize candidate journal: {error}")))?;
    body.push(b'\n');
    lingxi_core::host::rooted_fs::atomic_write(
        layout.root(),
        &layout.mcp_authoring_journal_rel(),
        &body,
        lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write candidate journal", &error))
}

/// Load and authenticate the durable authoring journal before resume.
pub fn load_candidate_journal(layout: &AppLayout) -> Result<McpCandidateJournal, AppError> {
    let body = lingxi_core::host::rooted_fs::read_to_string_limited(
        layout.root(),
        &layout.mcp_authoring_journal_rel(),
        256 * 1024,
    )
    .map_err(|error| match error {
        lingxi_core::host::FsError::NotFound(_) => {
            AppError::StorageCorrupt("candidate journal is missing".into())
        }
        other => AppError::from_fs("read candidate journal", &other),
    })?;
    let journal: McpCandidateJournal = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("candidate journal: {error}")))?;
    if journal.schema_version != APPS_SCHEMA_VERSION || journal.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(
            "candidate journal schema or app binding is invalid".into(),
        ));
    }
    journal
        .verify()
        .map_err(|issue| AppError::StorageCorrupt(issue.message))?;
    Ok(journal)
}

/// Remove a completed create-only candidate journal. Missing files are a
/// successful no-op so crash-recovery cleanup is idempotent.
pub fn delete_candidate_journal(layout: &AppLayout) -> Result<(), AppError> {
    match lingxi_core::host::rooted_fs::remove_file(
        layout.root(),
        &layout.mcp_authoring_journal_rel(),
    ) {
        Ok(()) | Err(lingxi_core::host::FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(AppError::from_fs("delete candidate journal", &error)),
    }
}

impl McpConfirmationReceipt {
    pub const TTL_MS: u64 = 10 * 60 * 1_000;

    #[must_use]
    pub fn new(
        app_id: impl Into<String>,
        workflow_run_id: impl Into<String>,
        approval_contract_sha256: String,
        candidate_digest: String,
        issued_at_ms: u64,
    ) -> Self {
        Self {
            receipt_id: format!("mcp-{}", crate::ids::generate_interaction_id()),
            app_id: app_id.into(),
            workflow_run_id: workflow_run_id.into(),
            approval_contract_sha256,
            candidate_digest,
            issued_at_ms,
            expires_at_ms: issued_at_ms.saturating_add(Self::TTL_MS),
            claimed: false,
            consumed: false,
            superseded: false,
        }
    }

    pub fn claim_candidate(
        &mut self,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        candidate_digest: &str,
        now_ms: u64,
    ) -> Result<(), GeneratedMcpIssue> {
        if self.candidate_digest != candidate_digest {
            return Err(binding_issue(
                "receipt_candidate_mismatch",
                "MCP receipt is bound to another candidate",
            )[0]
            .clone());
        }
        if self.app_id != app_id || self.workflow_run_id != workflow_run_id {
            return Err(binding_issue(
                "receipt_scope",
                "MCP receipt is bound to another app or workflow run",
            )[0]
            .clone());
        }
        if self.consumed {
            return Err(
                binding_issue("receipt_replay", "MCP receipt has already been consumed")[0].clone(),
            );
        }
        if self.claimed {
            return Err(
                binding_issue("receipt_in_use", "MCP receipt is already in use")[0].clone(),
            );
        }
        if self.superseded {
            return Err(
                binding_issue("receipt_superseded", "MCP receipt has been superseded")[0].clone(),
            );
        }
        if now_ms >= self.expires_at_ms {
            return Err(binding_issue("receipt_expired", "MCP receipt has expired")[0].clone());
        }
        if self.approval_contract_sha256 != approval_digest {
            return Err(binding_issue(
                "receipt_contract_mismatch",
                "MCP receipt approval contract changed",
            )[0]
            .clone());
        }
        self.claimed = true;
        Ok(())
    }

    pub fn release_claim(&mut self) {
        if !self.consumed {
            self.claimed = false;
        }
    }

    pub fn commit_claimed_candidate(
        &mut self,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        candidate_digest: &str,
    ) -> Result<(), GeneratedMcpIssue> {
        if self.app_id != app_id || self.workflow_run_id != workflow_run_id {
            return Err(binding_issue(
                "receipt_scope",
                "MCP receipt is bound to another app or workflow run",
            )[0]
            .clone());
        }
        if self.consumed {
            return Err(
                binding_issue("receipt_replay", "MCP receipt has already been consumed")[0].clone(),
            );
        }
        if !self.claimed {
            return Err(binding_issue(
                "receipt_not_claimed",
                "MCP receipt must be claimed before commit",
            )[0]
            .clone());
        }
        if self.approval_contract_sha256 != approval_digest {
            return Err(binding_issue(
                "receipt_contract_mismatch",
                "MCP receipt approval contract changed",
            )[0]
            .clone());
        }
        if self.candidate_digest != candidate_digest {
            return Err(binding_issue(
                "receipt_candidate_mismatch",
                "MCP receipt is bound to another candidate",
            )[0]
            .clone());
        }
        self.claimed = false;
        self.consumed = true;
        Ok(())
    }

    pub fn consume(
        &mut self,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        now_ms: u64,
    ) -> Result<(), GeneratedMcpIssue> {
        if self.app_id != app_id || self.workflow_run_id != workflow_run_id {
            return Err(binding_issue(
                "receipt_scope",
                "MCP receipt is bound to another app or workflow run",
            )[0]
            .clone());
        }
        if self.consumed {
            return Err(
                binding_issue("receipt_replay", "MCP receipt has already been consumed")[0].clone(),
            );
        }
        if self.superseded {
            return Err(
                binding_issue("receipt_superseded", "MCP receipt has been superseded")[0].clone(),
            );
        }
        if now_ms >= self.expires_at_ms {
            return Err(binding_issue("receipt_expired", "MCP receipt has expired")[0].clone());
        }
        if self.approval_contract_sha256 != approval_digest {
            return Err(binding_issue(
                "receipt_contract_mismatch",
                "MCP receipt approval contract changed",
            )[0]
            .clone());
        }
        self.claimed = false;
        self.consumed = true;
        Ok(())
    }

    /// Consume while also binding the receipt to the exact candidate bytes
    /// selected by the Host. This is the path used by promotion; the shorter
    /// `consume` helper remains useful for callers that already looked up the
    /// candidate by its receipt slot.
    pub fn consume_candidate(
        &mut self,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        candidate_digest: &str,
        now_ms: u64,
    ) -> Result<(), GeneratedMcpIssue> {
        if self.candidate_digest != candidate_digest {
            return Err(binding_issue(
                "receipt_candidate_mismatch",
                "MCP receipt is bound to another candidate",
            )[0]
            .clone());
        }
        self.consume(app_id, workflow_run_id, approval_digest, now_ms)
    }
}

impl McpReceiptBook {
    pub fn claim_candidate(
        &mut self,
        receipt_id: &str,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        candidate_digest: &str,
        now_ms: u64,
    ) -> Result<(), GeneratedMcpIssue> {
        let receipt = self.slots.get_mut(receipt_id).ok_or_else(|| {
            binding_issue(
                "receipt_missing",
                "MCP receipt is unknown or was not issued",
            )[0]
            .clone()
        })?;
        receipt.claim_candidate(
            app_id,
            workflow_run_id,
            approval_digest,
            candidate_digest,
            now_ms,
        )
    }

    pub fn release_claim(&mut self, receipt_id: &str) {
        if let Some(receipt) = self.slots.get_mut(receipt_id) {
            receipt.release_claim();
        }
    }

    pub fn commit_claimed_candidate(
        &mut self,
        receipt_id: &str,
        app_id: &str,
        workflow_run_id: &str,
        approval_digest: &str,
        candidate_digest: &str,
    ) -> Result<(), GeneratedMcpIssue> {
        let receipt = self.slots.get_mut(receipt_id).ok_or_else(|| {
            binding_issue(
                "receipt_missing",
                "MCP receipt is unknown or was not issued",
            )[0]
            .clone()
        })?;
        receipt.commit_claimed_candidate(app_id, workflow_run_id, approval_digest, candidate_digest)
    }
}

/// One validation finding for a generated Local App MCP definition or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedMcpIssue {
    /// Tool name when known.
    pub tool_name: Option<String>,
    /// Stable short code for the failure class.
    pub code: &'static str,
    /// Human-readable bounded explanation.
    pub message: String,
}

/// Deterministic budget accounting for a generated Local App MCP catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedMcpBudget {
    /// UTF-16 units per tool under Claude's fallback accounting.
    pub per_tool_utf16: BTreeMap<String, usize>,
    /// Estimated tokens per tool (`utf16 / 2.5`, floored).
    pub per_tool_tokens: BTreeMap<String, usize>,
    /// Aggregate UTF-16 units across the exposed set.
    pub total_utf16: usize,
    /// Aggregate estimated tokens across the exposed set.
    pub total_tokens: usize,
}

fn utf16_units(value: &str) -> usize {
    value.encode_utf16().count()
}

fn estimated_tokens(utf16: usize) -> usize {
    utf16.saturating_mul(2) / 5
}

fn canonical_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut prev_underscore = false;
    for ch in chars {
        match ch {
            '_' if !prev_underscore => prev_underscore = true,
            '_' => return false,
            c if c.is_ascii_lowercase() || c.is_ascii_digit() => prev_underscore = false,
            _ => return false,
        }
    }
    !name.ends_with('_')
}

fn schema_bytes(schema: &Value) -> Result<usize, serde_json::Error> {
    serde_json::to_vec(schema).map(|bytes| bytes.len())
}

fn validate_schema_node(
    schema: &Value,
    depth: usize,
    at_root: bool,
    issues: &mut Vec<GeneratedMcpIssue>,
    tool_name: &str,
) {
    if depth > MAX_SCHEMA_DEPTH {
        issues.push(GeneratedMcpIssue {
            tool_name: Some(tool_name.into()),
            code: "schema_depth_exceeded",
            message: format!("tool {tool_name} schema exceeds depth {MAX_SCHEMA_DEPTH}"),
        });
        return;
    }
    let Some(object) = schema.as_object() else {
        issues.push(GeneratedMcpIssue {
            tool_name: Some(tool_name.into()),
            code: "schema_not_object",
            message: format!("tool {tool_name} schema nodes must be JSON objects"),
        });
        return;
    };
    if at_root {
        let closed = matches!(object.get("additionalProperties"), Some(Value::Bool(false)));
        let object_root = object.get("type").is_some_and(|value| {
            value == "object"
                || value
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item == "object"))
        });
        if !object_root || !closed {
            issues.push(GeneratedMcpIssue {
                tool_name: Some(tool_name.into()),
                code: "root_schema_open",
                message: format!(
                    "tool {tool_name} schema root must be a closed object with additionalProperties=false"
                ),
            });
        }
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if !reference.starts_with('#') {
            issues.push(GeneratedMcpIssue {
                tool_name: Some(tool_name.into()),
                code: "remote_ref_forbidden",
                message: format!(
                    "tool {tool_name} schema uses forbidden remote $ref {reference:?}"
                ),
            });
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(value) = object.get(key) {
            match value.as_array() {
                Some(items) if !items.is_empty() && items.len() <= MAX_COMBINATOR_VARIANTS => {
                    for item in items {
                        validate_schema_node(item, depth + 1, false, issues, tool_name);
                    }
                }
                _ => issues.push(GeneratedMcpIssue {
                    tool_name: Some(tool_name.into()),
                    code: "unbounded_combinator",
                    message: format!(
                        "tool {tool_name} schema {key} must contain 1..={MAX_COMBINATOR_VARIANTS} variants"
                    ),
                }),
            }
        }
    }
    for key in ["properties", "$defs", "definitions"] {
        if let Some(Value::Object(children)) = object.get(key) {
            for child in children.values() {
                validate_schema_node(child, depth + 1, false, issues, tool_name);
            }
        }
    }
    if let Some(items) = object.get("items") {
        validate_schema_node(items, depth + 1, false, issues, tool_name);
    }
}

/// Validate one generated Local App MCP catalog.
pub fn validate_generated_mcp_catalog(
    tools: &[McpToolDefinitionDto],
) -> Result<GeneratedMcpBudget, Vec<GeneratedMcpIssue>> {
    let mut issues = Vec::new();
    if tools.len() > MAX_GENERATED_MCP_TOOLS {
        issues.push(GeneratedMcpIssue {
            tool_name: None,
            code: "tool_count_exceeded",
            message: format!(
                "generated MCP catalog exposes {} tools (limit {MAX_GENERATED_MCP_TOOLS})",
                tools.len()
            ),
        });
    }

    let mut per_tool_utf16 = BTreeMap::new();
    let mut per_tool_tokens = BTreeMap::new();
    let mut total_utf16 = 0usize;

    for tool in tools {
        if tool.name.is_empty() || tool.name.len() > 64 || !canonical_tool_name(&tool.name) {
            issues.push(GeneratedMcpIssue {
                tool_name: Some(tool.name.clone()),
                code: "invalid_tool_name",
                message: format!(
                    "tool {:?} must be canonical lower snake case in 1..=64 bytes",
                    tool.name
                ),
            });
        }
        if tool
            .title
            .as_deref()
            .is_some_and(|title| title.chars().count() > MAX_TOOL_TITLE_CHARS)
        {
            issues.push(GeneratedMcpIssue {
                tool_name: Some(tool.name.clone()),
                code: "title_too_large",
                message: format!(
                    "tool {} title exceeds {MAX_TOOL_TITLE_CHARS} characters",
                    tool.name
                ),
            });
        }
        if tool
            .description
            .as_deref()
            .is_some_and(|description| description.chars().count() > MAX_TOOL_DESCRIPTION_CHARS)
        {
            issues.push(GeneratedMcpIssue {
                tool_name: Some(tool.name.clone()),
                code: "description_too_large",
                message: format!(
                    "tool {} description exceeds {MAX_TOOL_DESCRIPTION_CHARS} characters",
                    tool.name
                ),
            });
        }

        for (label, schema) in [
            ("inputSchema", &tool.input_schema),
            (
                "outputSchema",
                tool.output_schema.as_ref().unwrap_or(&Value::Null),
            ),
        ] {
            if matches!(schema, Value::Null) {
                continue;
            }
            match schema_bytes(schema) {
                Ok(bytes) if bytes <= MAX_GENERATED_MCP_SCHEMA_BYTES => {}
                Ok(bytes) => issues.push(GeneratedMcpIssue {
                    tool_name: Some(tool.name.clone()),
                    code: "schema_too_large",
                    message: format!(
                        "tool {} {label} is {bytes} bytes (limit {MAX_GENERATED_MCP_SCHEMA_BYTES})",
                        tool.name
                    ),
                }),
                Err(error) => issues.push(GeneratedMcpIssue {
                    tool_name: Some(tool.name.clone()),
                    code: "schema_not_serializable",
                    message: format!("tool {} {label} failed to serialize: {error}", tool.name),
                }),
            }
            validate_schema_node(schema, 1, true, &mut issues, &tool.name);
        }

        let schema_json = tool.input_schema.to_string();
        let budget_utf16 = utf16_units(&tool.name)
            + utf16_units(tool.description.as_deref().unwrap_or(""))
            + utf16_units(&schema_json);
        let budget_tokens = estimated_tokens(budget_utf16);
        per_tool_utf16.insert(tool.name.clone(), budget_utf16);
        per_tool_tokens.insert(tool.name.clone(), budget_tokens);
        total_utf16 = total_utf16.saturating_add(budget_utf16);
        if budget_utf16 > MAX_GENERATED_MCP_DEFINITION_UTF16
            || budget_tokens > MAX_GENERATED_MCP_DEFINITION_TOKENS
        {
            issues.push(GeneratedMcpIssue {
                tool_name: Some(tool.name.clone()),
                code: "definition_budget_exceeded",
                message: format!(
                    "tool {} definition budget is {budget_utf16} UTF-16 units / {budget_tokens} tokens (limits {MAX_GENERATED_MCP_DEFINITION_UTF16} / {MAX_GENERATED_MCP_DEFINITION_TOKENS})",
                    tool.name
                ),
            });
        }
    }

    let total_tokens = estimated_tokens(total_utf16);
    if total_utf16 > MAX_EXPOSED_MCP_DEFINITIONS_UTF16
        || total_tokens > MAX_EXPOSED_MCP_DEFINITIONS_TOKENS
    {
        issues.push(GeneratedMcpIssue {
            tool_name: None,
            code: "aggregate_definition_budget_exceeded",
            message: format!(
                "generated MCP catalog budget is {total_utf16} UTF-16 units / {total_tokens} tokens (limits {MAX_EXPOSED_MCP_DEFINITIONS_UTF16} / {MAX_EXPOSED_MCP_DEFINITIONS_TOKENS})"
            ),
        });
    }

    if issues.is_empty() {
        Ok(GeneratedMcpBudget {
            per_tool_utf16,
            per_tool_tokens,
            total_utf16,
            total_tokens,
        })
    } else {
        Err(issues)
    }
}

/// Validate one generated tool result payload size before returning it as
/// `structuredContent`.
pub fn validate_generated_structured_result(value: &Value) -> Result<(), GeneratedMcpIssue> {
    let bytes = serde_json::to_vec(value).map_err(|error| GeneratedMcpIssue {
        tool_name: None,
        code: "structured_result_not_serializable",
        message: format!("structured result failed to serialize: {error}"),
    })?;
    if bytes.len() > MAX_GENERATED_MCP_STRUCTURED_RESULT_BYTES {
        return Err(GeneratedMcpIssue {
            tool_name: None,
            code: "structured_result_too_large",
            message: format!(
                "structured result is {} bytes (limit {MAX_GENERATED_MCP_STRUCTURED_RESULT_BYTES})",
                bytes.len()
            ),
        });
    }
    Ok(())
}

/// Derive the tighten-only Local App MCP permission ceiling from one flow.
#[must_use]
pub fn derive_local_app_mcp_ceiling(
    registry: &CapabilityRegistry,
    flow: &FlowDefinition,
) -> McpPermissionCeiling {
    if flow.validate(registry).is_err() {
        return McpPermissionCeiling::Deny;
    }
    let mut ceiling = McpPermissionCeiling::Allow;
    for step in &flow.steps {
        if !allowed_for_synchronous_flow(step.capability) {
            return McpPermissionCeiling::Deny;
        }
        let Some(descriptor) = registry.get(step.capability) else {
            return McpPermissionCeiling::Deny;
        };
        let must_ask = descriptor.requires_user_approval
            || descriptor.interactive
            || descriptor.transport != CapabilityTransport::NativeBridge
            || matches!(
                step.capability,
                CapabilityId::NetworkRequest
                    | CapabilityId::FilesRead
                    | CapabilityId::FilesWrite
                    | CapabilityId::Share
                    | CapabilityId::Calendar
                    | CapabilityId::Contacts
                    | CapabilityId::Media
                    | CapabilityId::DeepLink
                    | CapabilityId::Camera
                    | CapabilityId::PhotoLibrary
                    | CapabilityId::Microphone
                    | CapabilityId::SpeechToText
                    | CapabilityId::TextToSpeech
                    | CapabilityId::Location
                    | CapabilityId::Notifications
            );
        if must_ask {
            ceiling = McpPermissionCeiling::Ask;
        }
    }
    ceiling
}

#[cfg(test)]
mod tests {
    use super::{
        approval_contract_sha256, derive_local_app_mcp_ceiling, load_candidate_journal,
        materialize_flow_value_binding, save_candidate_journal, validate_app_mcp_flow_binding,
        validate_app_mcp_flow_binding_for_consumer, validate_app_mcp_proposal,
        validate_generated_mcp_catalog, validate_generated_structured_result, AppMcpFlowBinding,
        AppMcpFlowContext, AppMcpProposal, FlowSource, FlowValueBinding, McpAuthoringStage,
        McpCandidateJournal, McpConfirmationReceipt, McpReceiptBook,
        MAX_GENERATED_MCP_STRUCTURED_RESULT_BYTES,
    };
    use crate::{
        AppLayout, CapabilityId, CapabilityRegistry, FlowDefinition, FlowStep, APPS_SCHEMA_VERSION,
    };
    use mcp_wire::{McpPermissionCeiling, McpToolDefinitionDto};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn zero_tool_catalog_is_valid() {
        let budget = validate_generated_mcp_catalog(&[]).expect("zero tools allowed");
        assert_eq!(budget.total_utf16, 0);
        assert_eq!(budget.total_tokens, 0);
    }

    #[test]
    fn invalid_generated_tool_name_is_rejected() {
        let tool = McpToolDefinitionDto::new(
            "Bad-Name",
            json!({"type":"object","additionalProperties":false}),
        );
        let issues = validate_generated_mcp_catalog(&[tool]).expect_err("invalid name");
        assert!(issues.iter().any(|issue| issue.code == "invalid_tool_name"));
    }

    #[test]
    fn open_root_and_remote_ref_are_rejected() {
        let mut tool = McpToolDefinitionDto::new(
            "good_name",
            json!({
                "type":"object",
                "properties":{"x":{"$ref":"https://example.com/schema.json"}},
                "additionalProperties":true
            }),
        );
        tool.output_schema = Some(json!({
            "type":"object",
            "properties":{"y":{"type":"string"}},
            "additionalProperties":false
        }));
        let issues = validate_generated_mcp_catalog(&[tool]).expect_err("invalid schema");
        assert!(issues.iter().any(|issue| issue.code == "root_schema_open"));
        assert!(issues
            .iter()
            .any(|issue| issue.code == "remote_ref_forbidden"));
    }

    #[test]
    fn closed_root_requires_additional_properties_false() {
        let valid = McpToolDefinitionDto::new(
            "good_name",
            json!({
                "type":"object",
                "properties":{"x":{"type":"string"}},
                "additionalProperties":false
            }),
        );
        validate_generated_mcp_catalog(&[valid]).expect("closed root schema should pass");

        let open_true = McpToolDefinitionDto::new(
            "good_name",
            json!({
                "type":"object",
                "properties":{"x":{"type":"string"}},
                "additionalProperties":true
            }),
        );
        let issues = validate_generated_mcp_catalog(&[open_true]).expect_err("true must fail");
        assert!(issues.iter().any(|issue| issue.code == "root_schema_open"));

        let missing_flag = McpToolDefinitionDto::new(
            "good_name",
            json!({
                "type":"object",
                "properties":{"x":{"type":"string"}}
            }),
        );
        let issues =
            validate_generated_mcp_catalog(&[missing_flag]).expect_err("missing flag must fail");
        assert!(issues.iter().any(|issue| issue.code == "root_schema_open"));
    }

    #[test]
    fn oversized_structured_result_is_rejected() {
        let payload = json!({
            "blob": "x".repeat(MAX_GENERATED_MCP_STRUCTURED_RESULT_BYTES + 1)
        });
        let issue =
            validate_generated_structured_result(&payload).expect_err("payload should exceed cap");
        assert_eq!(issue.code, "structured_result_too_large");
    }

    #[test]
    fn per_tool_definition_budget_is_enforced() {
        let mut tool = McpToolDefinitionDto::new(
            "budget_name",
            json!({
                "type":"object",
                "properties":{"x":{"type":"string","description":"y".repeat(6000)}},
                "additionalProperties":false
            }),
        );
        tool.description = Some("brief".into());
        let issues = validate_generated_mcp_catalog(&[tool]).expect_err("budget must fail");
        assert!(issues
            .iter()
            .any(|issue| issue.code == "definition_budget_exceeded"));
    }

    #[test]
    fn local_app_ceiling_allows_private_read_only_flow() {
        let registry = CapabilityRegistry::default();
        let flow = FlowDefinition {
            flow_id: "flow".into(),
            version: 1,
            steps: vec![FlowStep {
                step_id: "step1".into(),
                capability: crate::CapabilityId::DataQuery,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        assert_eq!(
            derive_local_app_mcp_ceiling(&registry, &flow),
            McpPermissionCeiling::Allow
        );
    }

    #[test]
    fn local_app_ceiling_asks_for_sensitive_or_open_world_flow() {
        let registry = CapabilityRegistry::default();
        let flow = FlowDefinition {
            flow_id: "flow".into(),
            version: 1,
            steps: vec![FlowStep {
                step_id: "step1".into(),
                capability: crate::CapabilityId::NetworkRequest,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        assert_eq!(
            derive_local_app_mcp_ceiling(&registry, &flow),
            McpPermissionCeiling::Ask
        );
    }

    #[test]
    fn local_app_ceiling_denies_forbidden_capabilities() {
        let registry = CapabilityRegistry::default();
        let flow = FlowDefinition {
            flow_id: "flow".into(),
            version: 1,
            steps: vec![FlowStep {
                step_id: "step1".into(),
                capability: crate::CapabilityId::FlowExecute,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        assert_eq!(
            derive_local_app_mcp_ceiling(&registry, &flow),
            McpPermissionCeiling::Deny
        );
    }

    fn flow_context(capability: CapabilityId) -> AppMcpFlowContext {
        let flow = FlowDefinition {
            flow_id: "flow".into(),
            version: 1,
            steps: vec![FlowStep {
                step_id: "step1".into(),
                capability,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        let mut step_output_schemas = BTreeMap::new();
        step_output_schemas.insert(
            "step1".into(),
            json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false}),
        );
        AppMcpFlowContext {
            app_id: "app".into(),
            source: FlowSource::Active,
            flow,
            input_schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}),
            output_schema: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}),
            step_output_schemas,
        }
    }

    fn binding(result: FlowValueBinding) -> AppMcpFlowBinding {
        AppMcpFlowBinding {
            flow_id: "flow".into(),
            inputs: BTreeMap::from([(
                "query".into(),
                FlowValueBinding::ToolInput {
                    json_pointer: "/query".into(),
                },
            )]),
            result,
        }
    }

    #[test]
    fn typed_binding_rejects_cross_app_and_forbidden_flow() {
        let registry = CapabilityRegistry::default();
        let context = flow_context(CapabilityId::FlowExecute);
        let cross_app_context = AppMcpFlowContext {
            app_id: "other".into(),
            ..context.clone()
        };
        let cross_app = binding(FlowValueBinding::Literal(json!(null)));
        let issue = validate_app_mcp_flow_binding(&cross_app, "app", &cross_app_context, &registry)
            .expect_err("cross app binding must fail");
        assert_eq!(issue[0].code, "cross_app_flow");
        let issue = validate_app_mcp_flow_binding(
            &binding(FlowValueBinding::Literal(json!(null))),
            "app",
            &context,
            &registry,
        )
        .expect_err("FlowExecute must fail");
        assert_eq!(issue[0].code, "forbidden_capability");
    }

    #[test]
    fn typed_binding_enforces_schema_pointer_and_depth_limits() {
        let registry = CapabilityRegistry::default();
        let context = flow_context(CapabilityId::DataQuery);
        let mut invalid = binding(FlowValueBinding::Literal(json!(null)));
        invalid.inputs.insert(
            "bad".into(),
            FlowValueBinding::ToolInput {
                json_pointer: "/missing".into(),
            },
        );
        let issue = validate_app_mcp_flow_binding(&invalid, "app", &context, &registry)
            .expect_err("unknown input pointer must fail");
        assert_eq!(issue[0].code, "pointer_not_in_schema");
        let mut too_deep = String::new();
        for _ in 0..9 {
            too_deep.push_str("/x");
        }
        invalid.inputs.insert(
            "bad".into(),
            FlowValueBinding::ToolInput {
                json_pointer: too_deep,
            },
        );
        let issue = validate_app_mcp_flow_binding(&invalid, "app", &context, &registry)
            .expect_err("deep pointer must fail");
        assert_eq!(issue[0].code, "pointer_depth_exceeded");
    }

    #[test]
    fn consumer_binding_rejects_forward_step_output_and_bad_result_schema() {
        let registry = CapabilityRegistry::default();
        let mut context = flow_context(CapabilityId::DataQuery);
        context.flow.steps.push(FlowStep {
            step_id: "step2".into(),
            capability: CapabilityId::DataQuery,
            depends_on: vec!["step1".into()],
            input_json: "{}".into(),
        });
        context.step_output_schemas.insert(
            "step2".into(),
            json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false}),
        );
        let forward = AppMcpFlowBinding {
            flow_id: "flow".into(),
            inputs: BTreeMap::new(),
            result: FlowValueBinding::StepOutput {
                step_id: "step2".into(),
                json_pointer: "/value".into(),
            },
        };
        let issue = validate_app_mcp_flow_binding_for_consumer(
            &forward, "step2", "app", &context, &registry,
        )
        .expect_err("a consumer cannot read its own/future output");
        assert_eq!(issue[0].code, "forward_step_output");
        let bad_literal = AppMcpFlowBinding {
            flow_id: "flow".into(),
            inputs: BTreeMap::new(),
            result: FlowValueBinding::Literal(json!(false)),
        };
        let issue = validate_app_mcp_flow_binding(&bad_literal, "app", &context, &registry)
            .expect_err("result must satisfy output schema");
        assert_eq!(issue[0].code, "output_schema_mismatch");
    }

    #[test]
    fn materialize_typed_binding_resolves_input_literal_and_prior_output() {
        let input = json!({"query": "hello"});
        let outputs = BTreeMap::from([("step1".into(), json!({"value": "world"}))]);
        assert_eq!(
            materialize_flow_value_binding(
                &FlowValueBinding::ToolInput {
                    json_pointer: "/query".into(),
                },
                &input,
                &outputs,
            )
            .unwrap(),
            json!("hello")
        );
        assert_eq!(
            materialize_flow_value_binding(&FlowValueBinding::Literal(json!(42)), &input, &outputs)
                .unwrap(),
            json!(42)
        );
        assert_eq!(
            materialize_flow_value_binding(
                &FlowValueBinding::StepOutput {
                    step_id: "step1".into(),
                    json_pointer: "/value".into(),
                },
                &input,
                &outputs,
            )
            .unwrap(),
            json!("world")
        );
    }

    #[test]
    fn materialize_typed_binding_rejects_missing_or_dynamic_values() {
        let input = json!({"query": "hello"});
        let outputs = BTreeMap::new();
        let missing = materialize_flow_value_binding(
            &FlowValueBinding::ToolInput {
                json_pointer: "/missing".into(),
            },
            &input,
            &outputs,
        )
        .expect_err("missing tool input must fail closed");
        assert_eq!(missing.code, "binding_value_missing");
        let dynamic = materialize_flow_value_binding(
            &FlowValueBinding::ToolInput {
                json_pointer: "/{dynamic}".into(),
            },
            &input,
            &outputs,
        )
        .expect_err("dynamic pointers must fail closed");
        assert_eq!(dynamic.code, "pointer_dynamic");
    }

    #[test]
    fn proposal_rejects_host_fields_and_digest_is_canonical() {
        let raw = r#"{"appId":"app","manifestRevision":1,"userGoalSha256":"0000000000000000000000000000000000000000000000000000000000000000","summary":"s","tools":[],"server":"forged"}"#;
        assert!(serde_json::from_str::<AppMcpProposal>(raw).is_err());
        let proposal = AppMcpProposal {
            app_id: "app".into(),
            manifest_revision: 1,
            user_goal_sha256: "0".repeat(64),
            summary: "s".into(),
            tools: Vec::new(),
            required_flow_changes: Vec::new(),
            excluded_capabilities: Vec::new(),
        };
        let validated = validate_app_mcp_proposal(
            proposal,
            "app",
            1,
            &BTreeMap::new(),
            &CapabilityRegistry::default(),
        )
        .expect("zero-tool proposal is a valid candidate");
        assert_eq!(validated.proposal_sha256.len(), 64);
        assert_eq!(validated.tool_surface_sha256.len(), 64);
        let mut a = serde_json::json!({"b":1,"a":{"d":2,"c":3}});
        let b = serde_json::json!({"a":{"c":3,"d":2},"b":1});
        assert_eq!(
            approval_contract_sha256(a.take()).unwrap(),
            approval_contract_sha256(b).unwrap()
        );
    }

    #[test]
    fn journal_transitions_are_sealed_and_tamper_evident() {
        let zero = "0".repeat(64);
        let journal = McpCandidateJournal {
            schema_version: APPS_SCHEMA_VERSION,
            app_id: "app".into(),
            workflow_run_id: "run".into(),
            stage: McpAuthoringStage::Prepared,
            previous_build_id: None,
            previous_catalog_sha256: None,
            proposal_sha256: zero.clone(),
            approval_contract_sha256: zero.clone(),
            tool_surface_sha256: zero,
            catalog_sha256: None,
            integrity_sha256: String::new(),
        }
        .seal()
        .unwrap();
        let approved = journal
            .clone()
            .advance(McpAuthoringStage::Approved)
            .unwrap();
        assert!(approved.verify().is_ok());
        assert!(approved.approval_reusable(&"0".repeat(64)));
        assert!(!approved.approval_reusable(&"1".repeat(64)));
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "app").unwrap();
        save_candidate_journal(&layout, &approved).unwrap();
        assert_eq!(load_candidate_journal(&layout).unwrap(), approved);
        let mut tampered = approved;
        tampered.app_id = "other".into();
        assert_eq!(tampered.verify().unwrap_err().code, "journal_tampered");
    }

    #[test]
    fn receipt_is_ten_minutes_scoped_single_use_and_supersedable() {
        let digest = "0".repeat(64);
        let mut receipt =
            McpConfirmationReceipt::new("app", "run", digest.clone(), digest.clone(), 100);
        assert_eq!(receipt.expires_at_ms, 100 + McpConfirmationReceipt::TTL_MS);
        assert!(receipt.consume("other", "run", &digest, 101).is_err());
        assert!(receipt.consume("app", "run", &digest, 101).is_ok());
        assert_eq!(
            receipt
                .consume("app", "run", &digest, 101)
                .unwrap_err()
                .code,
            "receipt_replay"
        );
        let mut expired = McpConfirmationReceipt::new("app", "run", digest.clone(), digest, 0);
        assert_eq!(
            expired
                .consume(
                    "app",
                    "run",
                    &"0".repeat(64),
                    McpConfirmationReceipt::TTL_MS + 1
                )
                .unwrap_err()
                .code,
            "receipt_expired"
        );
        let mut book = McpReceiptBook::default();
        let first =
            McpConfirmationReceipt::new("slot-app", "run-1", "0".repeat(64), "0".repeat(64), 0);
        let first_id = first.receipt_id.clone();
        book.issue(first).unwrap();
        let second =
            McpConfirmationReceipt::new("slot-app", "run-2", "0".repeat(64), "0".repeat(64), 0);
        book.issue(second).unwrap();
        assert_eq!(
            book.consume(&first_id, "slot-app", "run-1", &"0".repeat(64), 1)
                .unwrap_err()
                .code,
            "receipt_superseded"
        );
    }

    /// A claimed slot that is never released (a scaffold whose future was
    /// dropped by a Stop, a panic, or the 529 non-streaming fallback) must
    /// only block a NEW receipt for as long as its own TTL — never forever.
    ///
    /// REGRESSION: `issue`'s in-use predicate used to ignore
    /// `expires_at_ms` entirely, so a leaked claim wedged the app
    /// `receipt_in_use` for the life of the process; only an engine restart
    /// (which resets the in-memory `McpReceiptBook`) recovered it.
    #[test]
    fn issue_lets_an_expired_leaked_claim_self_clear() {
        let digest = "0".repeat(64);
        let mut book = McpReceiptBook::default();
        let leaked = McpConfirmationReceipt::new("app", "run-1", digest.clone(), digest.clone(), 0);
        let leaked_id = leaked.receipt_id.clone();
        book.issue(leaked).unwrap();
        // Claim it and never release — the shape of the leak this fix exists
        // to bound, independent of whatever guard now prevents it in
        // practice.
        book.claim_candidate(&leaked_id, "app", "run-1", &digest, &digest, 0)
            .unwrap();

        // Before the TTL elapses, the unreleased claim must still block —
        // this predicate is not simply disabled.
        let too_soon = McpConfirmationReceipt::new(
            "app",
            "run-2",
            digest.clone(),
            digest.clone(),
            McpConfirmationReceipt::TTL_MS - 1,
        );
        assert_eq!(
            book.issue(too_soon).unwrap_err().code,
            "receipt_in_use",
            "an unexpired claimed slot must still block a new receipt"
        );

        // Once the leaked slot's TTL has passed, a new receipt must be
        // issuable — the escape hatch this fix adds.
        let after_ttl = McpConfirmationReceipt::new(
            "app",
            "run-3",
            digest.clone(),
            digest,
            McpConfirmationReceipt::TTL_MS,
        );
        book.issue(after_ttl)
            .expect("an expired claimed slot must not block a new receipt from being issued");
    }

    /// r3-engine-core-5 REGRESSION: `slots` had no eviction at all — issued,
    /// consumed, and superseded receipts alike were retained for the process
    /// lifetime, so a long-running host's book only ever grew. Assert the
    /// map's actual size, not just that later calls still succeed.
    #[test]
    fn issue_prunes_expired_slots_from_every_app_not_just_the_caller() {
        let digest = "0".repeat(64);
        let mut book = McpReceiptBook::default();
        for i in 0..5 {
            book.issue(McpConfirmationReceipt::new(
                format!("app-{i}"),
                "run",
                digest.clone(),
                digest.clone(),
                0,
            ))
            .unwrap();
        }
        assert_eq!(book.slots.len(), 5, "all five receipts should be live");

        // Issuing one more receipt, after every prior receipt's TTL has
        // elapsed, must evict all five stale slots — not just the one for
        // the app being issued to.
        book.issue(McpConfirmationReceipt::new(
            "app-new",
            "run",
            digest.clone(),
            digest,
            McpConfirmationReceipt::TTL_MS,
        ))
        .unwrap();
        assert_eq!(
            book.slots.len(),
            1,
            "expired slots for every app must be pruned, leaving only the new one"
        );
    }
}
