use super::*;

use lingxi_llm_client::protocol::{
    ContentBlock, NativeAnthropicErrorResponse, NativeAnthropicModelProviderFamily,
    NativeHrn15ErrorEvidence, ProtocolFamily,
};

fn native_facts() -> NativeAnthropicErrorFacts {
    NativeAnthropicErrorFacts {
        provider_id: "anthropic".into(),
        protocol: ProtocolFamily::AnthropicMessages,
        request_authority: None,
        thinking_resumption_refused: false,
        thinking_resumption_beta_requested: None,
        thinking_resumption_beta_refused: false,
        native_thinking_type_rejection: None,
        native_thinking_binding_controls_beta_requested: None,
        native_thinking_binding_controls_error_evidence: None,
        native_hq_model_source: None,
        native_hq_model_provider_family: Some(NativeAnthropicModelProviderFamily::FirstParty),
        native_hq_known_error_evidence: None,
        native_hq_hrn15_evidence: Some(NativeHrn15ErrorEvidence {
            original_status: 400,
            prefix_lock_message_match: true,
            prompt_too_long: false,
            context_overflow: false,
            credit_balance_too_low: false,
            organization_disabled: false,
            fast_mode_not_enabled: false,
            advisor_model_unavailable: false,
            dlp_request_denied: false,
            policy_blocked: false,
            output_blocked_by_content_filter: false,
            native_hie_claim: false,
            native_su_claim: false,
            mid_conversation_system_rejected: false,
            encrypted_search_payload_rejected: false,
            encrypted_code_execution_payload_rejected: false,
        }),
        response: Some(NativeAnthropicErrorResponse {
            body: None,
            thinking_prefix_mismatch: None,
            api_error_name: None,
            amzn_error_type: None,
            unified_representative_claim: None,
            unified_overage_disabled_reason: None,
        }),
    }
}

fn mismatch(message_index: u32, block_index: u32) -> NativeThinkingPrefixMismatch {
    NativeThinkingPrefixMismatch {
        block: Some(NativeThinkingPrefixBlock {
            message_index,
            block_index,
        }),
        kind: Some("signature".into()),
    }
}

fn request_with_target(signature: &str) -> ChatRequest {
    let mut request = ChatRequest::new("claude-sonnet-4-5");
    request
        .messages
        .push(lingxi_llm_client::protocol::ConversationMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "prefix".into(),
                    signature: Some("prefix-signature".into()),
                },
                ContentBlock::Thinking {
                    text: "target".into(),
                    signature: Some(signature.into()),
                },
                ContentBlock::Text {
                    text: "keep after target".into(),
                    thought_signature: None,
                    citations: None,
                },
            ],
            native_options: Vec::new(),
        });
    request
}

fn body_with_target(signature: &str) -> JsonPayload {
    JsonPayload::parse(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"prefix","signature":"prefix-signature"}},{{"type":"thinking","thinking":"target","signature":"{signature}"}},{{"type":"text","text":"keep after target"}}]}}]}}"#
    ))
    .unwrap()
}

fn sources_for(id: MessageId, blocks: usize) -> Vec<Vec<HashSet<MessageId>>> {
    vec![vec![HashSet::from([id]); blocks]]
}

fn source_history(id: MessageId, signature: &str) -> Vec<SourceThinkingMessage> {
    vec![SourceThinkingMessage {
        message_id: id,
        blocks: vec![
            Some(ThinkingPrefixBlockKey::Thinking("prefix-signature".into())),
            Some(ThinkingPrefixBlockKey::Thinking(signature.into())),
        ],
    }]
}

fn plan(
    request: &ChatRequest,
    body: &JsonPayload,
    sources: &[Vec<HashSet<MessageId>>],
    history: &[SourceThinkingMessage],
    ids: &[MessageId],
    attempt: u32,
) -> Option<PrefixLockPlan> {
    plan_prefix_lock_recovery(
        400,
        &native_facts(),
        Some(&mismatch(0, 1)),
        request,
        body,
        sources,
        history,
        ids,
        &HashMap::new(),
        attempt,
        false,
        false,
    )
}

#[test]
fn unique_typed_body_and_source_coordinate_selects_one_original_thinking_ordinal() {
    let id = MessageId::new();
    let request = request_with_target("target-signature");
    let selected = plan(
        &request,
        &body_with_target("target-signature"),
        &sources_for(id, 3),
        &source_history(id, "target-signature"),
        &[id],
        1,
    );
    assert_eq!(
        selected,
        Some(PrefixLockPlan::Partial(PrefixLockTarget {
            request_message_index: 0,
            request_block_index: 1,
            source_message_id: id,
            source_thinking_index: 1,
        }))
    );
}

#[test]
fn partial_marker_records_target_ordinal_then_later_source_rows_from_zero() {
    let target_id = MessageId::new();
    let later_id = MessageId::new();
    let target = PrefixLockTarget {
        request_message_index: 0,
        request_block_index: 1,
        source_message_id: target_id,
        source_thinking_index: 2,
    };
    assert_eq!(
        partial_source_ranges(target, &[target_id, later_id, later_id]),
        Some(HashMap::from([(target_id, 2), (later_id, 0)]))
    );
}

#[test]
fn duplicate_source_signature_or_multisource_block_falls_back_instead_of_guessing() {
    let id = MessageId::new();
    let request = request_with_target("target-signature");
    let duplicate_history = vec![
        SourceThinkingMessage {
            message_id: id,
            blocks: vec![Some(ThinkingPrefixBlockKey::Thinking(
                "target-signature".into(),
            ))],
        },
        SourceThinkingMessage {
            message_id: MessageId::new(),
            blocks: vec![Some(ThinkingPrefixBlockKey::Thinking(
                "target-signature".into(),
            ))],
        },
    ];
    assert_eq!(
        plan(
            &request,
            &body_with_target("target-signature"),
            &sources_for(id, 3),
            &duplicate_history,
            &[id, duplicate_history[1].message_id],
            1,
        ),
        Some(PrefixLockPlan::All)
    );

    let mut ambiguous_sources = sources_for(id, 3);
    ambiguous_sources[0][1].insert(MessageId::new());
    assert_eq!(
        plan(
            &request,
            &body_with_target("target-signature"),
            &ambiguous_sources,
            &source_history(id, "target-signature"),
            &[id],
            1,
        ),
        Some(PrefixLockPlan::All)
    );
}

#[test]
fn missing_or_disagreeing_body_coordinate_and_later_attempt_use_all_fallback() {
    let id = MessageId::new();
    let request = request_with_target("target-signature");
    let valid_body = body_with_target("target-signature");
    let cases = [
        (Some(&mismatch(9, 1)), &valid_body, 1),
        (
            Some(&mismatch(0, 1)),
            &body_with_target("different-signature"),
            1,
        ),
        (Some(&mismatch(0, 1)), &valid_body, 3),
        (None, &valid_body, 1),
    ];
    for (coordinate, body, attempt) in cases {
        let selected = plan_prefix_lock_recovery(
            400,
            &native_facts(),
            coordinate,
            &request,
            body,
            &sources_for(id, 3),
            &source_history(id, "target-signature"),
            &[id],
            &HashMap::new(),
            attempt,
            false,
            false,
        );
        assert_eq!(selected, Some(PrefixLockPlan::All));
    }
}

#[test]
fn duplicate_final_wire_body_key_falls_back_even_when_request_input_is_unique() {
    let id = MessageId::new();
    let request = request_with_target("target-signature");
    let body = JsonPayload::parse(
        r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"target","signature":"target-signature"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"duplicate","signature":"target-signature"}]}]}"#,
    )
    .unwrap();
    assert_eq!(
        plan(
            &request,
            &body,
            &sources_for(id, 3),
            &source_history(id, "target-signature"),
            &[id],
            1,
        ),
        Some(PrefixLockPlan::All)
    );
}

#[test]
fn history_projection_drop_keeps_the_surviving_block_source_at_its_new_coordinate() {
    let dropped_id = MessageId::new();
    let target_id = MessageId::new();
    let messages = vec![crate::Message {
        api_output_config: None,
        role: "assistant".into(),
        content: vec![
            crate::ContentBlock::ProviderContent {
                protocol: "open_ai_responses".into(),
                value: JsonPayload::parse(r#"{"type":"computer_call"}"#).unwrap(),
            },
            crate::ContentBlock::Reasoning {
                text: "target".into(),
                signature: Some("target-signature".into()),
            },
        ],
    }];
    let incoming_sources = vec![vec![
        HashSet::from([dropped_id]),
        HashSet::from([target_id]),
    ]];
    let (request, _, projected_sources) =
        crate::convert::input_projection::history_input_with_sources(
            "claude-sonnet-4-5",
            &messages,
            &incoming_sources,
            &[],
            &[],
            ProtocolFamily::AnthropicMessages,
        )
        .unwrap();
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].content.len(), 1);
    assert_eq!(
        projected_sources,
        vec![vec![HashSet::from([target_id])]],
        "the old source index 1 must follow its block to new index 0"
    );

    let prepared_body = JsonPayload::parse(
        r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"target","signature":"target-signature"}]}]}"#,
    )
    .unwrap();
    let source_history = vec![SourceThinkingMessage {
        message_id: target_id,
        blocks: vec![Some(ThinkingPrefixBlockKey::Thinking(
            "target-signature".into(),
        ))],
    }];
    assert_eq!(
        plan_prefix_lock_recovery(
            400,
            &native_facts(),
            Some(&mismatch(0, 0)),
            &request,
            &prepared_body,
            &projected_sources,
            &source_history,
            &[target_id],
            &HashMap::new(),
            1,
            false,
            false,
        ),
        Some(PrefixLockPlan::Partial(PrefixLockTarget {
            request_message_index: 0,
            request_block_index: 0,
            source_message_id: target_id,
            source_thinking_index: 0,
        }))
    );
}

#[test]
fn missing_extra_or_reordered_source_rows_never_authorize_a_guessed_target() {
    let id = MessageId::new();
    let request = request_with_target("target-signature");
    let body = body_with_target("target-signature");
    assert_eq!(
        plan(
            &request,
            &body,
            &[],
            &source_history(id, "target-signature"),
            &[id],
            1,
        ),
        Some(PrefixLockPlan::All),
        "request.extra.messages or an absent source projection cannot be inferred"
    );

    let prefix_id = MessageId::new();
    let text_id = MessageId::new();
    let reordered_sources = vec![vec![
        HashSet::from([prefix_id]),
        HashSet::from([prefix_id]),
        HashSet::from([text_id]),
    ]];
    let ordered_history = vec![
        SourceThinkingMessage {
            message_id: prefix_id,
            blocks: vec![Some(ThinkingPrefixBlockKey::Thinking(
                "prefix-signature".into(),
            ))],
        },
        SourceThinkingMessage {
            message_id: id,
            blocks: vec![Some(ThinkingPrefixBlockKey::Thinking(
                "target-signature".into(),
            ))],
        },
    ];
    assert_eq!(
        plan_prefix_lock_recovery(
            400,
            &native_facts(),
            Some(&mismatch(0, 1)),
            &request,
            &body,
            &reordered_sources,
            &ordered_history,
            &[prefix_id, id],
            &HashMap::new(),
            1,
            false,
            false,
        ),
        Some(PrefixLockPlan::All),
        "a source vector with a different content order cannot authorize partial stripping"
    );
}

#[test]
fn incomplete_resume_suppresses_mutation_and_marks_only_this_unhealed_error() {
    let id = MessageId::new();
    let request = request_with_target("target-signature");
    let selected = plan_prefix_lock_recovery(
        400,
        &native_facts(),
        Some(&mismatch(0, 1)),
        &request,
        &body_with_target("target-signature"),
        &sources_for(id, 3),
        &source_history(id, "target-signature"),
        &[id],
        &HashMap::new(),
        1,
        false,
        true,
    );
    assert_eq!(
        selected,
        Some(PrefixLockPlan::Declined {
            mark_same_error: true
        })
    );
    assert_eq!(
        plan_prefix_lock_recovery(
            400,
            &native_facts(),
            Some(&mismatch(0, 1)),
            &request,
            &body_with_target("target-signature"),
            &sources_for(id, 3),
            &source_history(id, "target-signature"),
            &[id],
            &HashMap::new(),
            2,
            true,
            true,
        ),
        Some(PrefixLockPlan::Declined {
            mark_same_error: false
        })
    );
}

#[test]
fn only_native_anthropic_http_400_with_hrn15_prefix_fact_is_eligible() {
    let facts = native_facts();
    assert!(is_native_prefix_lock_400(400, &facts));
    assert!(!is_native_prefix_lock_400(422, &facts));
    let mut compatible = facts.clone();
    compatible.provider_id = "custom-proxy".into();
    assert!(!is_native_prefix_lock_400(400, &compatible));
    let mut other_protocol = facts;
    other_protocol.protocol = ProtocolFamily::OpenAiChat;
    assert!(!is_native_prefix_lock_400(400, &other_protocol));
}

#[test]
fn partial_request_strip_preserves_prefix_and_reindexes_cache_string_and_source_sidecars() {
    let id = MessageId::new();
    let mut request = crate::LlmRequest::new("claude-sonnet-4-5");
    request
        .input
        .messages
        .push(lingxi_llm_client::protocol::ConversationMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "lead".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Thinking {
                    text: "preserve".into(),
                    signature: Some("prefix-signature".into()),
                },
                ContentBlock::Text {
                    text: "  ".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Thinking {
                    text: "strip".into(),
                    signature: Some("target-signature".into()),
                },
                ContentBlock::Text {
                    text: "keep".into(),
                    thought_signature: None,
                    citations: None,
                },
            ],
            native_options: Vec::new(),
        });
    request.execution.request_content_source_ids = vec![vec![HashSet::from([id]); 5]];
    request
        .execution
        .message_json_string_overrides
        .insert("/messages/0/content/4/text".into(), vec![0xd800]);
    request
        .input
        .prompt_cache
        .breakpoints
        .push(lingxi_llm_client::protocol::CacheBreakpoint {
            position: lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 4 },
            scope: None,
            ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
        });

    assert!(strip_partial_request(
        &mut request,
        PrefixLockTarget {
            request_message_index: 0,
            request_block_index: 3,
            source_message_id: id,
            source_thinking_index: 1,
        },
    ));
    assert!(matches!(
        request.input.messages[0].content.as_slice(),
        [
            ContentBlock::Text { text: lead, .. },
            ContentBlock::Thinking { signature: Some(prefix), .. },
            ContentBlock::Text { text: blank, .. },
            ContentBlock::Text { text: keep, .. },
        ] if lead == "lead" && prefix == "prefix-signature" && blank == "  " && keep == "keep"
    ));
    assert_eq!(request.execution.request_content_source_ids[0].len(), 4);
    assert_eq!(
        request
            .execution
            .message_json_string_overrides
            .get("/messages/0/content/3/text"),
        Some(&vec![0xd800])
    );
    assert_eq!(
        request.input.prompt_cache.breakpoints[0].position,
        lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 3 }
    );
}

#[test]
fn partial_first_thinking_strip_removes_blank_text_before_and_after_target() {
    let id = MessageId::new();
    let mut request = crate::LlmRequest::new("claude-sonnet-4-5");
    request
        .input
        .messages
        .push(lingxi_llm_client::protocol::ConversationMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "lead".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Text {
                    text: "  ".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Thinking {
                    text: "target".into(),
                    signature: Some("signature".into()),
                },
                ContentBlock::Text {
                    text: "\n".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Text {
                    text: "keep".into(),
                    thought_signature: None,
                    citations: None,
                },
            ],
            native_options: Vec::new(),
        });
    request.execution.request_content_source_ids = vec![vec![HashSet::from([id]); 5]];
    request
        .execution
        .message_json_string_overrides
        .insert("/messages/0/content/4/text".into(), vec![0xd800]);
    request
        .input
        .prompt_cache
        .breakpoints
        .push(lingxi_llm_client::protocol::CacheBreakpoint {
            position: lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 4 },
            scope: None,
            ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
        });
    assert!(strip_partial_request(
        &mut request,
        PrefixLockTarget {
            request_message_index: 0,
            request_block_index: 2,
            source_message_id: id,
            source_thinking_index: 0,
        }
    ));
    assert!(matches!(request.input.messages[0].content.as_slice(), [
        ContentBlock::Text { text: lead, .. },
        ContentBlock::Text { text: keep, .. },
    ] if lead == "lead" && keep == "keep"));
    assert_eq!(request.execution.request_content_source_ids[0].len(), 2);
    assert_eq!(
        request
            .execution
            .message_json_string_overrides
            .get("/messages/0/content/1/text"),
        Some(&vec![0xd800])
    );
    assert_eq!(
        request.input.prompt_cache.breakpoints[0].position,
        lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 1 }
    );
}

#[test]
fn partial_and_all_request_strips_use_native_js_trim_and_reindex_source_sidecars() {
    let target_id = MessageId::new();
    let after_thinking_id = MessageId::new();
    let after_nel_id = MessageId::new();
    let keep_id = MessageId::new();
    let make_request = || {
        let mut request = crate::LlmRequest::new("claude-sonnet-4-5");
        request
            .input
            .messages
            .push(lingxi_llm_client::protocol::ConversationMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "\u{feff}".into(),
                        thought_signature: None,
                        citations: None,
                    },
                    ContentBlock::Text {
                        text: "\u{0085}".into(),
                        thought_signature: None,
                        citations: None,
                    },
                    ContentBlock::Thinking {
                        text: "target".into(),
                        signature: Some("target-signature".into()),
                    },
                    ContentBlock::Text {
                        text: "\u{feff}".into(),
                        thought_signature: None,
                        citations: None,
                    },
                    ContentBlock::Text {
                        text: "\u{0085}".into(),
                        thought_signature: None,
                        citations: None,
                    },
                    ContentBlock::Text {
                        text: "keep".into(),
                        thought_signature: None,
                        citations: None,
                    },
                ],
                native_options: Vec::new(),
            });
        request.execution.request_content_source_ids = vec![vec![
            HashSet::from([target_id]),
            HashSet::from([target_id]),
            HashSet::from([target_id]),
            HashSet::from([after_thinking_id]),
            HashSet::from([after_nel_id]),
            HashSet::from([keep_id]),
        ]];
        for (block, units) in [
            (0, vec![0xfeff]),
            (1, vec![0x0085]),
            (3, vec![0xfeff]),
            (4, vec![0x0085]),
            (5, vec![0xd800]),
        ] {
            request
                .execution
                .message_json_string_overrides
                .insert(format!("/messages/0/content/{block}/text"), units);
            request.input.prompt_cache.breakpoints.push(
                lingxi_llm_client::protocol::CacheBreakpoint {
                    position: lingxi_llm_client::protocol::CachePosition::Message {
                        index: 0,
                        block,
                    },
                    scope: None,
                    ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
                },
            );
        }
        request
    };

    for partial in [true, false] {
        let mut request = make_request();
        if partial {
            assert!(strip_partial_request(
                &mut request,
                PrefixLockTarget {
                    request_message_index: 0,
                    request_block_index: 2,
                    source_message_id: target_id,
                    source_thinking_index: 0,
                },
            ));
        } else {
            assert!(strip_all_request_thinking(&mut request));
        }

        assert!(matches!(request.input.messages[0].content.as_slice(), [
            ContentBlock::Text { text: first_nel, .. },
            ContentBlock::Text { text: second_nel, .. },
            ContentBlock::Text { text: keep, .. },
        ] if first_nel == "\u{0085}" && second_nel == "\u{0085}" && keep == "keep"));
        assert_eq!(
            request.execution.request_content_source_ids[0],
            vec![
                HashSet::from([target_id]),
                HashSet::from([after_nel_id]),
                HashSet::from([keep_id]),
            ]
        );
        assert_eq!(
            request.execution.message_json_string_overrides,
            std::collections::BTreeMap::from([
                ("/messages/0/content/0/text".into(), vec![0x0085]),
                ("/messages/0/content/1/text".into(), vec![0x0085]),
                ("/messages/0/content/2/text".into(), vec![0xd800]),
            ])
        );
        assert_eq!(
            request
                .input
                .prompt_cache
                .breakpoints
                .iter()
                .map(|breakpoint| breakpoint.position)
                .collect::<Vec<_>>(),
            vec![
                lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 0 },
                lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 1 },
                lingxi_llm_client::protocol::CachePosition::Message { index: 0, block: 2 },
            ]
        );
    }
}

#[test]
fn partial_first_source_thinking_preserves_earlier_merged_source_prefix() {
    let previous = MessageId::new();
    let target = MessageId::new();
    let mut request = crate::LlmRequest::new("claude-sonnet-4-5");
    request
        .input
        .messages
        .push(lingxi_llm_client::protocol::ConversationMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "earlier row".into(),
                    signature: Some("previous".into()),
                },
                ContentBlock::Text {
                    text: "  ".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Text {
                    text: "\t".into(),
                    thought_signature: None,
                    citations: None,
                },
                ContentBlock::Thinking {
                    text: "target row".into(),
                    signature: Some("target".into()),
                },
                ContentBlock::Text {
                    text: "keep".into(),
                    thought_signature: None,
                    citations: None,
                },
            ],
            native_options: Vec::new(),
        });
    request.execution.request_content_source_ids = vec![vec![
        HashSet::from([previous]),
        HashSet::from([previous]),
        HashSet::from([target]),
        HashSet::from([target]),
        HashSet::from([target]),
    ]];
    assert!(strip_partial_request(
        &mut request,
        PrefixLockTarget {
            request_message_index: 0,
            request_block_index: 3,
            source_message_id: target,
            source_thinking_index: 0,
        }
    ));
    assert!(matches!(request.input.messages[0].content.as_slice(), [
        ContentBlock::Thinking { signature: Some(signature), .. },
        ContentBlock::Text { text: prefix_blank, .. },
        ContentBlock::Text { text: keep, .. },
    ] if signature == "previous" && prefix_blank == "  " && keep == "keep"));
    assert_eq!(
        request.execution.request_content_source_ids[0],
        vec![
            HashSet::from([previous]),
            HashSet::from([previous]),
            HashSet::from([target]),
        ]
    );
}
