//! A direct-call wrapper, passed to one dispatcher invocation rather than
//! registered globally. Permission policy and hooks see the original tool.
use super::*;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolStaticContext,
};
use tool_api::{ToolProgressSender, ToolUseContext};

pub(super) struct ControlMcpTool {
    pub base: Arc<dyn Tool>,
    pub owner: Arc<ConversationOrchestrator>,
    pub client: Arc<mcp::McpClient>,
    pub dispatch_name: String,
    pub server: String,
    pub diagnostic_server: String,
    pub raw_tool: String,
    pub wire_name: Utf16JsonProjection,
    pub cancellation_reason: Arc<std::sync::Mutex<Utf16JsonProjection>>,
    pub program_result: bool,
    pub expected_id: ToolUseId,
    pub result: Arc<tokio::sync::Mutex<Option<lingxi_core::host::McpToolResultDto>>>,
    pub failure: Arc<tokio::sync::Mutex<Option<Utf16JsonProjection>>>,
}

#[async_trait::async_trait]
impl Tool for ControlMcpTool {
    fn name(&self) -> &str {
        self.base.name()
    }
    fn aliases(&self) -> &[&str] {
        self.base.aliases()
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        self.base.input_schema()
    }
    fn input_validation_schema(&self) -> &Utf16JsonProjection {
        self.base.input_validation_schema()
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        self.base.input_repair_metadata()
    }
    fn input_schema_snapshot(&self) -> Option<Utf16JsonProjection> {
        self.base.input_schema_snapshot()
    }
    fn input_schema_revision(&self) -> Option<String> {
        self.base.input_schema_revision()
    }
    fn native_input_validation(&self) -> bool {
        self.base.native_input_validation()
    }
    fn input_validation_issues(&self, input: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
        self.base.input_validation_issues(input)
    }
    fn parse_native_input(
        &self,
        input: &Utf16JsonProjection,
    ) -> Option<Result<Utf16JsonProjection, tool_api::native_schema::NativeSchemaError>> {
        self.base.parse_native_input(input)
    }
    fn mcp_tool_identity(&self) -> Option<&tool_api::McpToolIdentity> {
        self.base.mcp_tool_identity()
    }
    fn mcp_deny_aliases(&self) -> &[String] {
        self.base.mcp_deny_aliases()
    }
    fn mcp_server_provenance(&self) -> Option<&mcp::McpServerProvenance> {
        self.base.mcp_server_provenance()
    }
    fn bind_session_tool_lease(&self, lease: tool_api::SessionToolGrantLease) {
        self.base.bind_session_tool_lease(lease)
    }
    fn mcp_role(&self) -> Option<&str> {
        self.base.mcp_role()
    }
    fn is_mcp(&self) -> bool {
        true
    }
    fn is_enabled(&self, context: &ToolStaticContext) -> bool {
        self.base.is_enabled(context)
    }
    fn supports_persistent_chat(&self) -> bool {
        self.base.supports_persistent_chat()
    }
    fn is_concurrency_safe(&self, input: &Utf16JsonProjection) -> bool {
        self.base.is_concurrency_safe(input)
    }
    fn is_read_only(&self, input: &Utf16JsonProjection) -> bool {
        self.base.is_read_only(input)
    }
    fn is_destructive(&self, input: &Utf16JsonProjection) -> bool {
        self.base.is_destructive(input)
    }
    fn is_open_world(&self, input: &Utf16JsonProjection) -> bool {
        self.base.is_open_world(input)
    }
    fn requires_user_interaction(&self) -> bool {
        self.base.requires_user_interaction()
    }
    fn suppresses_always_allow_rule(&self) -> bool {
        true
    }
    fn suppresses_all_permission_updates(&self) -> bool {
        true
    }
    fn requires_user_interaction_for_context(&self, context: &ToolUseContext) -> bool {
        self.base.requires_user_interaction_for_context(context)
    }
    fn max_result_size_chars(&self) -> usize {
        usize::MAX
    }
    fn persistence_threshold(&self) -> Option<usize> {
        None
    }
    fn interrupt_behavior(&self, input: &Utf16JsonProjection) -> InterruptBehavior {
        self.base.interrupt_behavior(input)
    }
    async fn tool_check_permission_ceiling(
        &self,
        input: &Utf16JsonProjection,
    ) -> Option<lingxi_core::host::McpPermissionCeiling> {
        self.base.tool_check_permission_ceiling(input).await
    }
    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        context: &ToolUseContext,
    ) -> permission::PermissionResult {
        self.base.check_permissions(input, context).await
    }
    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        context: &ToolUseContext,
    ) -> Result<(), tool_api::tool_trait::ValidationError> {
        self.base.validate_input(input, context).await
    }
    async fn description(
        &self,
        input: &Utf16JsonProjection,
        options: &DescriptionOptions,
    ) -> String {
        self.base.description(input, options).await
    }
    async fn prompt(&self, options: &PromptOptions) -> Utf16JsonProjection {
        self.base.prompt(options).await
    }
    fn map_result_text(&self, _: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        Some("The result went to the host that asked for this call.".into())
    }
    async fn call(
        &self,
        mut input: Utf16JsonProjection,
        context: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if context.tool_use_id.as_ref() != Some(&self.expected_id) {
            return Err(ToolError::InvalidInput(
                "this tool answers only the call the host asked for.".into(),
            ));
        }
        if input.object_entries().is_some() {
            let _ = input.remove_field("__consentNonce");
        }
        let cancel = context.cancel.unwrap_or_default();
        let result = tokio::select! {
            _=cancel.cancelled()=>Err(format!("mcp_call cancelled by client: {}",self.diagnostic_server).into()),
            result=self.client.call_control_tool_with_wire_name(&self.dispatch_name,self.wire_name.clone(),input,self.cancellation_reason.clone(),None)=>match result {
                Ok(result)=>self.owner.finish_mcp_control_result(result,&self.server,&self.raw_tool,self.program_result).await,
                Err(error)=>Err(super::control_rpc_error(&error)),
            },
        };
        let result = match result {
            Ok(result) => result,
            Err(detail) => {
                *self.failure.lock().await = Some(detail.clone());
                return Err(ToolError::McpFailure {
                    server: self.server.clone().into(),
                    tool: self.raw_tool.clone().into(),
                    detail: detail.into(),
                });
            }
        };
        *self.result.lock().await = Some(result.clone());
        let mut value = ToolCallResult::from_data(result.content);
        value.model_content = self.map_result_text(&value.data);
        Ok(value)
    }
}
