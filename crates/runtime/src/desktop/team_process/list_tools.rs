//! M2 shared-list tools. The list is one published app instance with one data
//! identity; the worker holds no copy of it and every call goes through the
//! trusted host. A write is an operation of the Team ledger: it is named by
//! the durable logical step of its tool call, never by a provider call id, and
//! it leaves this process only after the response that holds the call is on
//! the transcript file, so a later process names the same operation again.
use super::checkpoint::{LIST_PROPOSE_TOOL, LIST_READ_TOOL, LIST_WRITE_TOOL};
use super::{interaction::ChildInteraction, TeamProcessError};
use crate::desktop::session_state::team_chat::BotChatScope;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tool_api::tool_trait::*;

pub(super) const MAX_OPERATION_JSON: usize = 64 * 1024;
const MAX_CHANGES: usize = 20;
const MAX_ITEM_TEXT: usize = 500;

/// Who must agree to a change before it is saved.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamListApproval {
    #[default]
    None,
    /// The member whose request the task serves approves each change.
    Requester,
}
impl TeamListApproval {
    fn is_none(&self) -> bool {
        *self == Self::None
    }
}

/// What the Bot is told about the list it may read and change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TeamSharedList {
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "TeamListApproval::is_none")]
    pub approval: TeamListApproval,
}
impl TeamSharedList {
    pub fn validate(&self) -> Result<(), TeamProcessError> {
        let ok = |text: &str, max: usize| {
            !text.trim().is_empty() && text.len() <= max && !text.contains('\0')
        };
        if ok(&self.title, 120) && ok(&self.description, 2000) {
            Ok(())
        } else {
            Err(TeamProcessError::Protocol)
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TeamOperationTool {
    ListRead,
    ListWrite,
}

/// One request to the host's shared-list service. `logical_step_id` is present
/// exactly for a write and is the identity the ledger derives its operation id from.
/// `approval_ref` is present exactly for a write on a list whose changes the
/// requester approves, and names the wait of that same step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TeamOperationRequest {
    pub id: String,
    pub tool: TeamOperationTool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_ref: Option<String>,
    pub arguments: serde_json::Value,
}
impl TeamOperationRequest {
    pub fn validate(
        &self,
        task_id: &str,
        approval: TeamListApproval,
    ) -> Result<(), TeamProcessError> {
        let step = match (self.tool, &self.logical_step_id) {
            (TeamOperationTool::ListRead, None) => {
                self.approval_ref.is_none() && self.arguments == serde_json::json!({})
            }
            (TeamOperationTool::ListWrite, Some(step)) => {
                step.len() <= 200
                    && match approval {
                        TeamListApproval::None => self.approval_ref.is_none(),
                        TeamListApproval::Requester => {
                            self.approval_ref.as_deref()
                                == Some(super::checkpoint::wait_id(step).as_str())
                        }
                    }
                    && step
                        .strip_prefix(task_id)
                        .and_then(|rest| rest.strip_prefix("/t"))
                        .and_then(|rest| rest.split_once("/c"))
                        .is_some_and(|(group, index)| {
                            group.parse::<u32>().is_ok() && index.parse::<u32>().is_ok()
                        })
                    && validate_write(&self.arguments).is_ok()
            }
            _ => false,
        };
        if step && super::protocol::valid_id(&self.id) {
            Ok(())
        } else {
            Err(TeamProcessError::Protocol)
        }
    }
}

/// The established outcome of a request. An outcome nobody could establish is
/// not a result: the host fails the process instead, and the call stays
/// unfinished on the transcript for the ledger to decide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TeamOperationResult {
    pub succeeded: bool,
    pub result: serde_json::Value,
}
impl TeamOperationResult {
    pub fn validate(&self) -> Result<(), TeamProcessError> {
        let size = serde_json::to_vec(&self.result)
            .map_err(|_| TeamProcessError::Protocol)?
            .len();
        if self.result.is_object() && size <= MAX_OPERATION_JSON {
            Ok(())
        } else {
            Err(TeamProcessError::Protocol)
        }
    }
}

/// A list change as the model may request it. Unknown keys are refused so a
/// typo is never silently dropped from a write.
pub(super) fn validate_write(arguments: &serde_json::Value) -> Result<(), &'static str> {
    let object = arguments.as_object().ok_or("The input must be an object")?;
    if object
        .keys()
        .any(|key| key != "expectedDataRevision" && key != "changes")
    {
        return Err("Only expectedDataRevision and changes are accepted");
    }
    if object
        .get("expectedDataRevision")
        .and_then(|v| v.as_u64())
        .is_none()
    {
        return Err("expectedDataRevision must be the dataRevision returned by the latest read");
    }
    let changes = object
        .get("changes")
        .and_then(|v| v.as_array())
        .filter(|changes| (1..=MAX_CHANGES).contains(&changes.len()))
        .ok_or("changes must hold 1 to 20 changes")?;
    let text = |v: Option<&serde_json::Value>| {
        v.and_then(|v| v.as_str())
            .is_some_and(|t| !t.trim().is_empty() && t.len() <= MAX_ITEM_TEXT && !t.contains('\0'))
    };
    let item = |v: Option<&serde_json::Value>| {
        v.and_then(|v| v.as_str())
            .is_some_and(super::protocol::valid_id)
    };
    for change in changes {
        let change = change.as_object().ok_or("Each change must be an object")?;
        let keys = |allowed: &[&str]| change.keys().all(|key| allowed.contains(&key.as_str()));
        let valid = match change.get("op").and_then(|v| v.as_str()) {
            Some("add") => keys(&["op", "text"]) && text(change.get("text")),
            Some("update") => {
                keys(&["op", "itemId", "text", "done"])
                    && item(change.get("itemId"))
                    && (change.contains_key("text") || change.contains_key("done"))
                    && change.get("text").is_none_or(|v| text(Some(v)))
                    && change.get("done").is_none_or(|v| v.is_boolean())
            }
            Some("remove") => keys(&["op", "itemId"]) && item(change.get("itemId")),
            _ => false,
        };
        if !valid {
            return Err("A change is add {text}, update {itemId, text?, done?} or remove {itemId}; text is at most 500 bytes");
        }
    }
    Ok(())
}

/// Where this process finds the durable row of a tool call.
pub(super) struct StepSource {
    pub transcript: PathBuf,
    pub scope: BotChatScope,
    pub batch_id: String,
}
pub(crate) struct TeamListTools {
    pub(super) bridge: Arc<ChildInteraction>,
    pub(super) list: TeamSharedList,
    pub(super) task_id: String,
    /// The wait state shared with the ask tool. Required when the requester approves changes.
    pub(super) ask: Option<Arc<super::ask_tool::AskRequester>>,
    /// Set once the session transcript of this process is known.
    pub(super) source: OnceLock<StepSource>,
}
impl TeamListTools {
    fn approved(&self) -> bool {
        self.list.approval == TeamListApproval::Requester
    }
    /// The name of the tool that changes the list. A list whose changes are
    /// approved offers a different tool, so a transcript never confuses a
    /// proposal with a direct write.
    pub(super) fn write_tool(&self) -> &'static str {
        if self.approved() {
            LIST_PROPOSE_TOOL
        } else {
            LIST_WRITE_TOOL
        }
    }
    /// The logical step of the unfinished call `tool_use_id`, once the response
    /// that holds it is on the transcript file. A call that never becomes
    /// durable is never dispatched.
    async fn durable_step(&self, tool_use_id: &str) -> Option<String> {
        let source = self.source.get()?;
        for _ in 0..200 {
            let transcript = std::fs::read_to_string(&source.transcript).unwrap_or_default();
            if let Some(step) = super::checkpoint::pending_step(
                &source.scope,
                &self.task_id,
                &source.batch_id,
                &transcript,
                tool_use_id,
            ) {
                return Some(step);
            }
            tokio::select! {
                biased;
                () = self.bridge.cancel.cancelled() => return None,
                () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
        None
    }
}

struct ListTool {
    tools: Arc<TeamListTools>,
    write: bool,
    schema: Json,
}
pub(super) fn register(registry: &mut tool_api::ToolRegistry, tools: Arc<TeamListTools>) {
    for write in [false, true] {
        let schema = if write {
            serde_json::json!({"type":"object","properties":{
                "expectedDataRevision":{"type":"integer","minimum":0,"description":"The dataRevision returned by the latest read of the list."},
                "changes":{"type":"array","minItems":1,"maxItems":MAX_CHANGES,"items":{"type":"object","properties":{
                    "op":{"type":"string","enum":["add","update","remove"]},
                    "itemId":{"type":"string","description":"For update and remove: the itemId returned by a read."},
                    "text":{"type":"string","description":"For add, and optionally for update: the item text."},
                    "done":{"type":"boolean","description":"Optionally for update: whether the item is done."}},
                    "required":["op"],"additionalProperties":false}}},
                "required":["expectedDataRevision","changes"],"additionalProperties":false})
        } else {
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
        };
        registry.register_builtin(Arc::new(ListTool {
            tools: tools.clone(),
            write,
            schema: generated_builtin_schema(&schema),
        }));
    }
}
fn done(value: &serde_json::Value, is_error: bool) -> ToolCallResult {
    ToolCallResult {
        data: generated_builtin_result(value),
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error,
        mcp_meta: None,
        subagent_hooks_fired: false,
    }
}
#[async_trait::async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        if self.write {
            self.tools.write_tool()
        } else {
            LIST_READ_TOOL
        }
    }
    fn input_schema(&self) -> &Json {
        &self.schema
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        MAX_OPERATION_JSON
    }
    fn is_concurrency_safe(&self, _: &Json) -> bool {
        !self.write
    }
    fn is_read_only(&self, _: &Json) -> bool {
        !self.write
    }
    async fn description(&self, _: &Json, _: &DescriptionOptions) -> String {
        let list = &self.tools.list;
        if self.write && self.tools.approved() {
            format!("Propose a change to the shared list \"{}\": add, update or remove items. The person who asked is shown the exact change and it is saved only if they approve it.", list.title)
        } else if self.write {
            format!("Change the shared list \"{}\": add, update or remove items. Everyone with access sees the same list.", list.title)
        } else {
            format!(
                "Read the shared list \"{}\": its items and its current dataRevision. {}",
                list.title, list.description
            )
        }
    }
    async fn prompt(&self, _: &PromptOptions) -> Json {
        if self.write && self.tools.approved() {
            "Read the list first and pass the dataRevision you received as expectedDataRevision. The person who asked is shown exactly this change and the task pauses until they reply, so propose everything you want changed in one call. If they decline, nothing is saved: do not propose the same change again unless they ask for it. If the list changed in the meantime the approved change is refused and nothing is saved: read the list again and, if the change is still needed, propose it again. Tell people a change was saved only after this tool reported success.".into()
        } else if self.write {
            "Read the list first and pass the dataRevision you received as expectedDataRevision. If the list changed in the meantime the write is refused and nothing is saved: read the list again, check whether your change is still needed, then write again. Tell people a change was saved only after this tool reported success.".into()
        } else {
            "The list is shared and other people change it. Read it again before relying on earlier contents.".into()
        }
    }
    async fn check_permissions(
        &self,
        _: &Json,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Published Team capability: the shared list".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn call(
        &self,
        input: Json,
        context: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let arguments: serde_json::Value = input
            .to_json_string()
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .ok_or_else(|| ToolError::Internal("Invalid list request".into()))?;
        let (tool, logical_step_id, approval_ref, arguments) = if self.write {
            validate_write(&arguments).map_err(|reason| ToolError::Internal(reason.into()))?;
            let id = context
                .tool_use_id
                .as_ref()
                .ok_or_else(|| ToolError::Internal("The list cannot be changed here".into()))?;
            let step = self.tools.durable_step(id.as_str()).await.ok_or_else(|| {
                ToolError::Internal(
                    "The change was not sent because the request could not be recorded first"
                        .into(),
                )
            })?;
            let approval_ref = if self.tools.approved() {
                let ask =
                    self.tools.ask.as_ref().ok_or_else(|| {
                        ToolError::Internal("The list cannot be changed here".into())
                    })?;
                // The proposal is durable, so the wait every process derives
                // from its step exists before this call can park on it.
                let wait_id = super::checkpoint::wait_id(&step);
                match ask.open(Some(&wait_id)) {
                    Err(reason) => return Err(ToolError::Internal(reason.into())),
                    Ok(None) => ask.park().await,
                    Ok(Some(reply)) if reply.approved != Some(true) => {
                        return Ok(done(
                            &serde_json::json!({"error":"DECLINED","message":"The person who asked declined this change. Nothing was saved."}),
                            true,
                        ))
                    }
                    Ok(Some(_)) => Some(wait_id),
                }
            } else {
                None
            };
            (
                TeamOperationTool::ListWrite,
                Some(step),
                approval_ref,
                arguments,
            )
        } else {
            (
                TeamOperationTool::ListRead,
                None,
                None,
                serde_json::json!({}),
            )
        };
        match self
            .tools
            .bridge
            .operate(tool, logical_step_id, approval_ref, arguments)
            .await
        {
            Ok(outcome) => Ok(done(&outcome.result, !outcome.succeeded)),
            // The outcome is not established. Returning anything would let the
            // model act on a guess; the parent is already ending this process.
            Err(_) if self.write => std::future::pending().await,
            Err(_) => Err(ToolError::Internal(
                "The shared list is unavailable or authorization changed".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_write_names_its_durable_step_and_a_read_carries_nothing() {
        let write = |step: Option<&str>, arguments| TeamOperationRequest {
            id: "interaction-1".into(),
            tool: TeamOperationTool::ListWrite,
            logical_step_id: step.map(str::to_owned),
            approval_ref: None,
            arguments,
        };
        let add = json!({"expectedDataRevision":3,"changes":[{"op":"add","text":"Milk"}]});
        assert!(write(Some("task_1/t0/c1"), add.clone())
            .validate("task_1", TeamListApproval::None)
            .is_ok());
        for step in [
            None,
            Some("task_2/t0/c1"),
            Some("task_1/t0"),
            Some("task_1/tx/c1"),
            Some("task_1/t0/c1/x"),
        ] {
            assert!(
                write(step, add.clone())
                    .validate("task_1", TeamListApproval::None)
                    .is_err(),
                "{step:?}"
            );
        }
        let read = |step: Option<&str>, arguments| TeamOperationRequest {
            id: "interaction-1".into(),
            tool: TeamOperationTool::ListRead,
            logical_step_id: step.map(str::to_owned),
            approval_ref: None,
            arguments,
        };
        assert!(read(None, json!({}))
            .validate("task_1", TeamListApproval::None)
            .is_ok());
        assert!(read(Some("task_1/t0/c0"), json!({}))
            .validate("task_1", TeamListApproval::None)
            .is_err());
        assert!(read(None, json!({"all":true}))
            .validate("task_1", TeamListApproval::None)
            .is_err());
    }

    #[test]
    fn an_approved_write_names_the_wait_of_its_own_step() {
        let write = |approval_ref: Option<String>| TeamOperationRequest {
            id: "interaction-1".into(),
            tool: TeamOperationTool::ListWrite,
            logical_step_id: Some("task_1/t0/c1".into()),
            approval_ref,
            arguments: json!({"expectedDataRevision":3,"changes":[{"op":"add","text":"Milk"}]}),
        };
        let own = super::super::checkpoint::wait_id("task_1/t0/c1");
        let other = super::super::checkpoint::wait_id("task_1/t0/c2");
        assert!(write(Some(own.clone()))
            .validate("task_1", TeamListApproval::Requester)
            .is_ok());
        for approval_ref in [None, Some(other)] {
            assert!(write(approval_ref)
                .validate("task_1", TeamListApproval::Requester)
                .is_err());
        }
        assert!(write(Some(own))
            .validate("task_1", TeamListApproval::None)
            .is_err());
        let list: TeamSharedList =
            serde_json::from_value(json!({"title":"Groceries","description":"What to buy"}))
                .unwrap();
        assert_eq!(list.approval, TeamListApproval::None);
        assert_eq!(
            serde_json::to_value(&list).unwrap(),
            json!({"title":"Groceries","description":"What to buy"})
        );
        let list: TeamSharedList = serde_json::from_value(
            json!({"title":"Groceries","description":"What to buy","approval":"requester"}),
        )
        .unwrap();
        assert_eq!(list.approval, TeamListApproval::Requester);
    }

    #[test]
    fn list_changes_are_strict() {
        let ok =
            |changes| validate_write(&json!({"expectedDataRevision":0,"changes":changes})).is_ok();
        assert!(ok(
            json!([{"op":"add","text":"Milk"},{"op":"update","itemId":"item_1","done":true},{"op":"remove","itemId":"item_2"}])
        ));
        assert!(ok(
            json!([{"op":"update","itemId":"item_1","text":"Oat milk","done":false}])
        ));
        for changes in [
            json!([]),
            json!([{"op":"add"}]),
            json!([{"op":"add","text":"  "}]),
            json!([{"op":"add","text":"x".repeat(501)}]),
            json!([{"op":"add","text":"Milk","done":true}]),
            json!([{"op":"update","itemId":"item_1"}]),
            json!([{"op":"update","itemId":"bad id","done":true}]),
            json!([{"op":"remove","itemId":"item_1","text":"x"}]),
            json!([{"op":"clear"}]),
            serde_json::Value::Array(vec![json!({"op":"add","text":"x"}); 21]),
        ] {
            assert!(!ok(changes.clone()), "{changes}");
        }
        assert!(validate_write(&json!({"changes":[{"op":"add","text":"Milk"}]})).is_err());
        assert!(validate_write(
            &json!({"expectedDataRevision":-1,"changes":[{"op":"add","text":"Milk"}]})
        )
        .is_err());
        assert!(validate_write(
            &json!({"expectedDataRevision":1,"changes":[{"op":"add","text":"Milk"}],"force":true})
        )
        .is_err());
    }

    #[test]
    fn results_are_bounded_objects() {
        assert!(TeamOperationResult {
            succeeded: true,
            result: json!({"dataRevision":4})
        }
        .validate()
        .is_ok());
        assert!(TeamOperationResult {
            succeeded: false,
            result: json!("conflict")
        }
        .validate()
        .is_err());
        assert!(TeamOperationResult {
            succeeded: true,
            result: json!({"text":"x".repeat(MAX_OPERATION_JSON)})
        }
        .validate()
        .is_err());
    }
}
