//! Host recovery policy. Provider codecs never decide to repeat side effects.

use crate::conversation::{NativeThinkingResumptionHostFacts, OrchestratorApiClient};
pub(crate) const INCOMPLETE_TOOL_NUDGE: &str = "A tool call was interrupted before its arguments were complete. Keep completed tool results. Generate a complete call only for the unfinished work, using smaller chunks if necessary. Do not repeat tools that already completed.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputRecoveryAction {
    ContinueText,
    RegenerateIncompleteTool,
    KeepToolResultsAndRepair,
    Stop,
}

/// Terminal facts only: a transport failure or parser diagnosis alone never
/// establishes that generation reached a token limit.
pub(crate) struct CompletionFacts<'a> {
    pub stop_reason: Option<&'a str>,
    pub malformed_tool: bool,
    pub accepted_tools: bool,
    pub actions_used: u32,
}
/// Decide whether one Native max-output response may resume its signed thinking
/// block. All policy gates must be explicit, including the SDK's response-local
/// context headroom decision. Missing SDK evidence remains Unknown.
pub(crate) fn native_thinking_resumption_eligible(
    host: &NativeThinkingResumptionHostFacts,
    stop_reason: Option<&str>,
    successful_assistant_messages: u32,
    has_api_error: bool,
    content_block_count: usize,
    has_resumable_thinking_block: bool,
    resumable: Option<bool>,
    native_resume_context_exhausted: Option<bool>,
) -> bool {
    if !host.gates_admitted() {
        return false;
    }
    if stop_reason != Some("max_tokens")
        || successful_assistant_messages != 1
        || has_api_error
        || content_block_count != 1
        || !has_resumable_thinking_block
        || resumable != Some(true)
        || native_resume_context_exhausted != Some(false)
    {
        return false;
    }
    true
}

/// Determine the locally observable Native provider-family admission for the
/// selected request. Remote feature gates remain separate Host facts.
pub(crate) fn native_route_supports_resumption(
    api: &dyn OrchestratorApiClient,
    model: &str,
    profile: Option<&str>,
) -> bool {
    match api.protocol_for_model(model, profile) {
        Some(llm_runtime::ProtocolFamily::AnthropicMessages) => {
            api.is_first_party_route(model, profile)
        }
        Some(llm_runtime::ProtocolFamily::FoundryClaude) => true,
        _ => false,
    }
}

/// Native OUn's baked `interleaved_thinking` result for routes represented by
/// Harness. The SDK-backed ApiService lookup supplies QTe's model-option
/// override first; this helper covers the subsequent route/model default.
pub(crate) fn native_interleaved_thinking_default(
    api: &dyn OrchestratorApiClient,
    model: &str,
    profile: Option<&str>,
) -> Option<bool> {
    let protocol = api.protocol_for_model(model, profile)?;
    let canonical =
        lingxi_core::host::model_capabilities::normalize_model_id(model).to_ascii_lowercase();
    match protocol {
        llm_runtime::ProtocolFamily::FoundryClaude => Some(true),
        llm_runtime::ProtocolFamily::AnthropicMessages
            if api.is_first_party_route(model, profile) =>
        {
            Some(!canonical.contains("claude-3-"))
        }
        // Harness's Bedrock and Vertex route identities do not establish
        // Native's anthropicAws or anthropicGoogle provider identities.
        llm_runtime::ProtocolFamily::BedrockClaude | llm_runtime::ProtocolFamily::VertexClaude => {
            Some(false)
        }
        _ => None,
    }
}

pub(crate) fn json_payload_is_true(payload: &lingxi_llm_client::protocol::JsonPayload) -> bool {
    matches!(
        serde_json::from_str::<bool>(payload.as_json_source()),
        Ok(true)
    )
}

pub(crate) fn history_has_resumable_thinking_block(blocks: &[llm_runtime::ContentBlock]) -> bool {
    blocks.len() == 1
        && (matches!(
            blocks.first(),
            Some(llm_runtime::ContentBlock::Reasoning {
                signature: Some(signature),
                ..
            }) if !signature.is_empty()
        ) || matches!(
            blocks.first(),
            Some(llm_runtime::ContentBlock::RedactedThinking { .. })
        ))
}

pub(crate) fn conversation_has_resumable_thinking_block(
    blocks: &[lingxi_core::types::ContentBlock],
) -> bool {
    blocks.len() == 1
        && (matches!(
            blocks.first(),
            Some(lingxi_core::types::ContentBlock::Thinking {
                signature: Some(signature),
                ..
            }) if !signature.is_empty()
        ) || matches!(
            blocks.first(),
            Some(lingxi_core::types::ContentBlock::RedactedThinking { .. })
        ))
}

pub(crate) fn utf16_projection_is_true(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> bool {
    projection
        .at_pointer("")
        .is_some_and(|value| value.as_bool() == Some(true))
}

pub(crate) fn decide_output_recovery(facts: CompletionFacts<'_>) -> OutputRecoveryAction {
    if facts.actions_used >= 3 {
        return OutputRecoveryAction::Stop;
    }
    if facts.malformed_tool
        && matches!(
            facts.stop_reason,
            Some("max_tokens" | "tool_use" | "end_turn" | "stop_sequence")
        )
    {
        return if facts.accepted_tools {
            OutputRecoveryAction::KeepToolResultsAndRepair
        } else {
            OutputRecoveryAction::RegenerateIncompleteTool
        };
    }
    if facts.stop_reason == Some("max_tokens") {
        OutputRecoveryAction::ContinueText
    } else {
        OutputRecoveryAction::Stop
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockApiClient;

    fn fully_known_resumption_facts() -> NativeThinkingResumptionHostFacts {
        NativeThinkingResumptionHostFacts {
            feature_gate_enabled: Some(true),
            request_gate_enabled: Some(true),
            provider_route_allowed: Some(true),
            model_capability_allowed: Some(true),
            disable_experimental_betas: Some(false),
            hipaa_mode: Some(false),
            thinking_resumption_refused: Some(false),
        }
    }

    #[test]
    fn native_resume_requires_all_typed_host_gates_and_exact_response_shape() {
        let host = fully_known_resumption_facts();
        let eligible = |host: &NativeThinkingResumptionHostFacts,
                        response_count,
                        has_api_error,
                        block_count,
                        signed,
                        resumable,
                        context_exhausted| {
            native_thinking_resumption_eligible(
                host,
                Some("max_tokens"),
                response_count,
                has_api_error,
                block_count,
                signed,
                resumable,
                context_exhausted,
            )
        };
        assert!(eligible(&host, 1, false, 1, true, Some(true), Some(false)));
        assert!(!eligible(&host, 1, false, 1, true, Some(true), None));
        assert!(!eligible(&host, 1, false, 1, true, Some(true), Some(true)));
        assert!(!eligible(
            &NativeThinkingResumptionHostFacts::default(),
            1,
            false,
            1,
            true,
            Some(true),
            Some(false),
        ));
        assert!(!eligible(&host, 2, false, 1, true, Some(true), Some(false)));
        assert!(!eligible(&host, 1, true, 1, true, Some(true), Some(false)));
        assert!(!eligible(&host, 1, false, 2, true, Some(true), Some(false)));
        assert!(!eligible(
            &host,
            1,
            false,
            1,
            false,
            Some(true),
            Some(false)
        ));
        assert!(!eligible(
            &host,
            1,
            false,
            1,
            true,
            Some(false),
            Some(false)
        ));
    }

    #[test]
    fn native_resume_u6_admits_only_first_party_messages_and_foundry() {
        let first_party = MockApiClient::new(Vec::new());
        first_party
            .set_native_request_route(Some(llm_runtime::ProtocolFamily::AnthropicMessages), true);
        assert!(native_route_supports_resumption(
            &first_party,
            "claude-sonnet-4-6",
            None
        ));

        let compatible = MockApiClient::new(Vec::new());
        compatible
            .set_native_request_route(Some(llm_runtime::ProtocolFamily::AnthropicMessages), false);
        assert!(!native_route_supports_resumption(
            &compatible,
            "claude-sonnet-4-6",
            None
        ));

        for protocol in [
            llm_runtime::ProtocolFamily::BedrockClaude,
            llm_runtime::ProtocolFamily::VertexClaude,
        ] {
            let api = MockApiClient::new(Vec::new());
            api.set_native_request_route(Some(protocol), false);
            assert!(!native_route_supports_resumption(
                &api,
                "claude-sonnet-4-6",
                None
            ));
        }

        let foundry = MockApiClient::new(Vec::new());
        foundry.set_native_request_route(Some(llm_runtime::ProtocolFamily::FoundryClaude), false);
        assert!(native_route_supports_resumption(
            &foundry,
            "claude-sonnet-4-6",
            None
        ));
    }

    #[test]
    fn native_interleaved_thinking_defaults_match_supported_local_routes() {
        let first_party = MockApiClient::new(Vec::new());
        first_party
            .set_native_request_route(Some(llm_runtime::ProtocolFamily::AnthropicMessages), true);
        assert_eq!(
            native_interleaved_thinking_default(&first_party, "claude-sonnet-4-6", None),
            Some(true)
        );
        assert_eq!(
            native_interleaved_thinking_default(&first_party, "claude-3-7-sonnet", None),
            Some(false)
        );
        assert_eq!(
            native_interleaved_thinking_default(&first_party, "claude-haiku-4-5", None),
            Some(true),
            "first-party belongs to Native DW before the generic Haiku rule"
        );

        let foundry = MockApiClient::new(Vec::new());
        foundry.set_native_request_route(Some(llm_runtime::ProtocolFamily::FoundryClaude), false);
        assert_eq!(
            native_interleaved_thinking_default(&foundry, "claude-3-7-sonnet", None),
            Some(true),
            "Foundry's explicit default precedes the DW model check"
        );
    }

    #[test]
    fn resumable_json_payload_requires_root_true_boolean() {
        let payload = |source: &str| {
            lingxi_llm_client::protocol::JsonPayload::parse(source).expect("fixture is valid JSON")
        };
        assert!(json_payload_is_true(&payload("true")));
        assert!(!json_payload_is_true(&payload("false")));
        assert!(!json_payload_is_true(&payload("1")));
        assert!(!json_payload_is_true(&payload("\"true\"")));
    }

    #[test]
    fn native_resume_requires_signed_thinking_or_redacted_thinking() {
        assert!(history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::Reasoning {
                text: String::new(),
                signature: Some("sig".into()),
            }
        ]));
        assert!(history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::RedactedThinking {
                data: "ciphertext".into(),
            }
        ]));
        assert!(!history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::Reasoning {
                text: String::new(),
                signature: Some(String::new()),
            }
        ]));
        assert!(history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::RedactedThinking {
                data: String::new(),
            }
        ]));
        assert!(!history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::Reasoning {
                text: "unfinished".into(),
                signature: None,
            }
        ]));
        assert!(!history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None,
            }
        ]));
        assert!(!history_has_resumable_thinking_block(&[
            llm_runtime::ContentBlock::Reasoning {
                text: String::new(),
                signature: Some("sig".into())
            },
            llm_runtime::ContentBlock::Text {
                text: String::new(),
                cache_control: None,
                citations: None
            },
        ]));
    }

    #[test]
    fn core_signed_thinking_requires_nonempty_signature_but_redacted_keeps_presence_semantics() {
        assert!(!conversation_has_resumable_thinking_block(&[
            lingxi_core::types::ContentBlock::Thinking {
                thinking: String::new(),
                signature: Some(String::new()),
            }
        ]));
        assert!(conversation_has_resumable_thinking_block(&[
            lingxi_core::types::ContentBlock::RedactedThinking {
                data: String::new(),
            }
        ]));
    }

    #[test]
    fn terminal_reason_and_side_effects_choose_recovery() {
        for reason in [None, Some("aborted"), Some("insufficient_system_resource")] {
            assert_eq!(
                decide_output_recovery(CompletionFacts {
                    stop_reason: reason,
                    malformed_tool: true,
                    accepted_tools: false,
                    actions_used: 0
                }),
                OutputRecoveryAction::Stop
            );
        }
        assert_eq!(
            decide_output_recovery(CompletionFacts {
                stop_reason: Some("max_tokens"),
                malformed_tool: true,
                accepted_tools: false,
                actions_used: 0
            }),
            OutputRecoveryAction::RegenerateIncompleteTool
        );
        assert_eq!(
            decide_output_recovery(CompletionFacts {
                stop_reason: Some("max_tokens"),
                malformed_tool: true,
                accepted_tools: true,
                actions_used: 0
            }),
            OutputRecoveryAction::KeepToolResultsAndRepair
        );
        assert_eq!(
            decide_output_recovery(CompletionFacts {
                stop_reason: Some("max_tokens"),
                malformed_tool: false,
                accepted_tools: false,
                actions_used: 3
            }),
            OutputRecoveryAction::Stop
        );
    }
}

#[derive(Clone)]
pub(crate) struct FrozenGeneration {
    pub model: String,
    pub profile: Option<String>,
    pub system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    pub tools: Vec<lingxi_core::host::source_tool_wire::SourceToolWireDefinition>,
    pub available_tool_names: Option<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
    pub skip_global_cache: bool,
    pub thinking_resumption_host_facts: NativeThinkingResumptionHostFacts,
}

impl std::fmt::Debug for FrozenGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrozenGeneration")
            .field("model", &self.model)
            .field("profile", &self.profile)
            .field("message_count", &self.messages.len())
            .field("tool_count", &self.tools.len())
            .finish_non_exhaustive()
    }
}
