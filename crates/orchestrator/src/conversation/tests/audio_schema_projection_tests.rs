use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::{DeferralState, ToolSearchMode};

struct LiveAudioSchemaTool {
    supported_actions: Arc<RwLock<Option<Vec<String>>>>,
    schema_revision: Arc<AtomicU64>,
}

impl LiveAudioSchemaTool {
    fn current_actions(&self) -> Vec<String> {
        self.supported_actions
            .read()
            .expect("audio support lock")
            .clone()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Tool for LiveAudioSchemaTool {
    fn name(&self) -> &str {
        "speech"
    }

    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<lingxi_core::types::utf16_json::Utf16JsonProjection> =
            std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            tool_api::tool_trait::generated_builtin_result(
                &json!({ "type": "object", "properties": {} }),
            )
        })
    }

    fn input_schema_snapshot(&self) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        Some(tool_api::tool_trait::generated_builtin_result(&json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": self.current_actions() }
            },
            "required": ["action"]
        })))
    }

    fn input_schema_revision(&self) -> Option<String> {
        Some(format!(
            "native:{}",
            self.schema_revision.load(Ordering::Acquire)
        ))
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        !self.current_actions().is_empty()
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> bool {
        false
    }

    fn is_read_only(&self, _input: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "schema projection fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Live speech recognition and playback".into()
    }

    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        "Live speech recognition and playback".into()
    }

    async fn call(
        &self,
        _input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({ "content": "ok" })),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

struct ToolSearchMarker;

#[async_trait]
impl Tool for ToolSearchMarker {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<lingxi_core::types::utf16_json::Utf16JsonProjection> =
            std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            tool_api::tool_trait::generated_builtin_result(
                &json!({ "type": "object", "properties": {} }),
            )
        })
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> bool {
        false
    }

    fn is_read_only(&self, _input: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn check_permissions(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "schema projection fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "Search deferred tools".into()
    }

    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        "Search deferred tools".into()
    }

    async fn call(
        &self,
        _input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({ "content": "ok" })),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

fn live_audio_orchestrator() -> (
    ConversationOrchestrator,
    Arc<ToolRegistry>,
    Arc<RwLock<Option<Vec<String>>>>,
    Arc<AtomicU64>,
) {
    let supported_actions = Arc::new(RwLock::new(None));
    let schema_revision = Arc::new(AtomicU64::new(0));
    let mut registry = ToolRegistry::new();
    registry.set_deferral(Arc::new(DeferralState::new(ToolSearchMode::Enabled, false)));
    registry.register_builtin(Arc::new(ToolSearchMarker));
    registry.register_builtin(Arc::new(LiveAudioSchemaTool {
        supported_actions: supported_actions.clone(),
        schema_revision: schema_revision.clone(),
    }));
    let registry = Arc::new(registry);
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        registry.clone(),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(Vec::new())),
        PathBuf::from("/work/repo"),
    );
    (orchestrator, registry, supported_actions, schema_revision)
}

fn wire_tool<'a>(
    wire: &'a [lingxi_core::host::SourceToolWireDefinition],
    name: &str,
) -> Option<&'a lingxi_core::host::SourceToolWireDefinition> {
    wire.iter().find(|tool| tool.name == name)
}

fn action_enum(tool: &lingxi_core::host::SourceToolWireDefinition) -> String {
    tool.input_schema
        .subprojection("/properties/action/enum")
        .expect("speech action enum projection")
        .to_json_string()
        .expect("speech action enum JSON")
}

#[tokio::test]
async fn live_audio_support_updates_wire_cache_and_tool_search_together() {
    let (orchestrator, registry, supported_actions, schema_revision) = live_audio_orchestrator();
    let search_view = registry.tool_search_view();

    let unknown = orchestrator.build_wire_tools().await.unwrap().0;
    assert!(wire_tool(&unknown, "speech").is_none());
    assert!(search_view.entries().is_empty());

    *supported_actions.write().expect("support lock") =
        Some(vec!["transcribe".into(), "speak".into()]);
    schema_revision.store(1, Ordering::Release);
    // Model the successful ToolSearch selection: its shared deferral state
    // makes this discovered schema visible in the next request.
    registry.deferral().mark_loaded(["speech"]);
    let known = orchestrator.build_wire_tools().await.unwrap().0;
    assert_eq!(
        action_enum(wire_tool(&known, "speech").unwrap()),
        r#"["transcribe","speak"]"#
    );
    let known_cache = orchestrator
        .prompt_runtime
        .wire_tool_schema_cache
        .lock()
        .await
        .clone()
        .expect("known capabilities should populate the wire cache");
    assert!(known_cache
        .key
        .dynamic_schema_revisions
        .contains(&("speech".into(), "native:1".into())));
    let known_search = search_view.entries();
    assert_eq!(known_search.len(), 1);
    assert_eq!(known_search[0].name, "speech");
    assert_eq!(
        known_search[0].description.string_at(""),
        Some("Live speech recognition and playback")
    );

    *supported_actions.write().expect("support lock") = Some(vec!["transcribe".into()]);
    schema_revision.store(2, Ordering::Release);
    let changed = orchestrator.build_wire_tools().await.unwrap().0;
    assert_eq!(
        action_enum(wire_tool(&changed, "speech").unwrap()),
        r#"["transcribe"]"#
    );
    let changed_cache = orchestrator
        .prompt_runtime
        .wire_tool_schema_cache
        .lock()
        .await
        .clone()
        .expect("changed capabilities should refresh the wire cache");
    assert!(changed_cache
        .key
        .dynamic_schema_revisions
        .contains(&("speech".into(), "native:2".into())));
    assert_ne!(known_cache.key, changed_cache.key);
    assert_eq!(search_view.entries()[0].name, "speech");

    *supported_actions.write().expect("support lock") = None;
    schema_revision.store(3, Ordering::Release);
    let disconnected = orchestrator.build_wire_tools().await.unwrap().0;
    assert!(wire_tool(&disconnected, "speech").is_none());
    assert!(search_view.entries().is_empty());
    let disconnected_cache = orchestrator
        .prompt_runtime
        .wire_tool_schema_cache
        .lock()
        .await
        .clone()
        .expect("disconnect should refresh the wire cache");
    assert!(!disconnected_cache
        .key
        .tool_names
        .iter()
        .any(|name| name == "speech"));
}
