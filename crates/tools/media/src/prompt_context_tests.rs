use super::*;
use std::sync::{Arc, Mutex};
use tool_api::context::{MediaAgentCapabilityContext, MediaAgentHandoffRequest};
use tool_api::tool_trait::Tool;

struct SessionBoundHandoff(Mutex<String>);

impl SessionBoundHandoff {
    fn accepts(&self, context: &MediaAgentCapabilityContext) -> bool {
        self.0.lock().unwrap().as_str() == context.session_id.as_str()
            && context.model == "shared-model"
            && context.profile_name.as_deref() == Some("trusted-profile")
            && context.account_scope.as_deref() == Some("profile:trusted-profile")
    }
}

impl MediaAgentHandoffHandle for SessionBoundHandoff {
    fn supported_modes(&self) -> Vec<MediaSessionMode> {
        vec![MediaSessionMode::Audio]
    }

    fn supported_modes_for_context(
        &self,
        context: &MediaAgentCapabilityContext,
    ) -> Vec<MediaSessionMode> {
        self.accepts(context)
            .then_some(MediaSessionMode::Audio)
            .into_iter()
            .collect()
    }

    fn supports_mode(&self, context: &MediaAgentCapabilityContext, mode: MediaSessionMode) -> bool {
        mode == MediaSessionMode::Audio && self.accepts(context)
    }

    fn request(
        &self,
        _invocation: &ToolUseContext,
        _context: &MediaAgentCapabilityContext,
        _request: MediaAgentHandoffRequest,
    ) -> Result<(), String> {
        Err("schema fixture does not dispatch handoffs".into())
    }
}

fn request_options(
    session_id: lingxi_core::types::SessionId,
    profile: &str,
    is_scheduled_turn: bool,
) -> PromptOptions {
    PromptOptions {
        session_id: Some(session_id),
        is_scheduled_turn,
        model: Some("shared-model".into()),
        model_profile: Some(profile.into()),
        ..PromptOptions::default()
    }
}

async fn advertised_schema(tool: &MediaTool, options: &PromptOptions) -> Value {
    let definition = tool_api::wire::tool_to_wire(tool, options).await;
    serde_json::from_str(&definition.input_schema.to_json_string().unwrap()).unwrap()
}

fn advertises_conversation(schema: &Value) -> bool {
    schema["properties"]["intent"]["enum"]
        .as_array()
        .is_some_and(|intents| intents.iter().any(|intent| intent == "conversation"))
}

#[tokio::test]
async fn model_wire_schema_tracks_current_session_route_and_scheduled_origin() {
    let first_session = lingxi_core::types::SessionId::new();
    let retargeted_session = lingxi_core::types::SessionId::new();
    let handoff = Arc::new(SessionBoundHandoff(Mutex::new(
        first_session.as_uuid().to_string(),
    )));
    let mut context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    // Mobile intentionally has no session identity baked into the static tool
    // context. The provider request supplies the current identity per build.
    assert!(context.session_id.is_none());
    context.media_agent_handoff = Some(handoff.clone());
    let tool = MediaTool::new(
        context,
        MediaOperationKind::SessionStart,
        "media_session_start",
    );

    let first = advertised_schema(
        &tool,
        &request_options(first_session, "trusted-profile", false),
    )
    .await;
    assert!(advertises_conversation(&first));
    assert_eq!(
        first["allOf"][0]["then"]["properties"]["mode"]["enum"],
        json!(["audio"])
    );

    *handoff.0.lock().unwrap() = retargeted_session.as_uuid().to_string();
    let stale_owner = advertised_schema(
        &tool,
        &request_options(first_session, "trusted-profile", false),
    )
    .await;
    assert!(!advertises_conversation(&stale_owner));

    let current_owner = advertised_schema(
        &tool,
        &request_options(retargeted_session, "trusted-profile", false),
    )
    .await;
    assert!(advertises_conversation(&current_owner));

    let scheduled = advertised_schema(
        &tool,
        &request_options(retargeted_session, "trusted-profile", true),
    )
    .await;
    assert!(!advertises_conversation(&scheduled));

    let unpinned_account =
        advertised_schema(&tool, &request_options(retargeted_session, "", false)).await;
    assert!(!advertises_conversation(&unpinned_account));
}

#[tokio::test]
async fn scheduled_conversation_invocation_is_rejected_before_media_service_dispatch() {
    let session_id = lingxi_core::types::SessionId::new();
    let handoff = Arc::new(SessionBoundHandoff(Mutex::new(
        session_id.as_uuid().to_string(),
    )));
    let mut context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    context.media_agent_handoff = Some(handoff);
    let tool = MediaTool::new(
        context,
        MediaOperationKind::SessionStart,
        "media_session_start",
    );
    let mut invocation = tool_api::test_support::fresh_ctx();
    invocation.is_scheduled_turn = true;
    let result = tool
        .call(
            generated_projection(&json!({
                "intent":"conversation",
                "mode":"audio",
                "audio_source_ref":"microphone-1"
            })),
            invocation,
            tool_api::test_support::fresh_tx(),
        )
        .await;
    assert!(matches!(
        result,
        Err(ToolError::InteractionRequired(message))
            if message.contains("interactive main Agent")
    ));
}
