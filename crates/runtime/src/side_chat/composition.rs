//! Small shared composition pieces for Desktop and native child factories.
use command_api::{CommandRegistry, CommandSource, SlashCommandKind};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

/// Build independent RAM-only compaction and its cache-safe request slot.
pub fn compaction(
    api: Arc<llm_runtime::ApiService>,
    model: &str,
) -> (
    Arc<compaction::CompactionOrchestrator>,
    Arc<sidequery::CacheSafeParamsSlot>,
) {
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = compaction_runner(api, model);
    let compactor = Arc::new(
        compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            compaction::thresholds::auto_compact_threshold(model, &[]),
        )
        .with_model_derived_threshold(),
    );
    (compactor, slot)
}

fn compaction_runner(
    api: Arc<llm_runtime::ApiService>,
    model: &str,
) -> Arc<sidequery::ForkedAgentRunner> {
    let query = Arc::new(sidequery::ProviderSideQueryClient::from_service(
        api.clone(),
    ));
    Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(query, model.to_string())
            .with_session_thinking(api.thinking()),
    )
}

/// Expose only eligible inline skills from the child registry and read state.
pub fn skill_listing(
    commands: Arc<RwLock<CommandRegistry>>,
    read_state: tool_api::read_file_state::ReadFileStateMap,
    cwd: PathBuf,
) -> Arc<dyn orchestrator::prompt::skill_listing::SkillListingProvider> {
    let conditional = Arc::new(std::sync::Mutex::new(skill::ConditionalSkills::new()));
    Arc::new(
        orchestrator::prompt::skill_listing::LazySkillListingProvider::new(move || {
            let commands = commands.clone();
            let read_state = read_state.clone();
            let conditional = conditional.clone();
            let cwd = cwd.clone();
            async move {
                let registry = commands.read().await;
                let full_catalog = registry
                    .native_source_ordered_commands()
                    .into_iter()
                    .filter(|command| command.is_enabled)
                    .map(
                        |command| orchestrator::prompt::skill_listing::SkillListingCatalogName {
                            name: command.name.clone(),
                            user_facing_name: command.user_facing_name.clone(),
                            aliases: command.aliases.clone(),
                        },
                    )
                    .collect::<Vec<_>>();
                let touched = read_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .keys();
                let entries = registry
                    .model_invocable_commands()
                    .into_iter()
                    .filter(|command| command.is_enabled && registry.execution_allowed(command))
                    .filter(|command| registry.session_skill_allowed(command))
                    .filter(|command| {
                        matches!(
                            command.kind,
                            SlashCommandKind::Markdown { .. }
                                | SlashCommandKind::Plugin { .. }
                                | SlashCommandKind::Bundled { .. }
                        )
                    })
                    .filter(|command| {
                        command.paths.as_deref().is_none_or(|patterns| {
                            conditional
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .is_available_named(&command.name, patterns, &touched, &cwd)
                        })
                    })
                    .filter(|command| {
                        command.source == CommandSource::Builtin
                            || matches!(
                                command.loaded_from.as_deref(),
                                Some("bundled" | "skills" | "syncedSkills" | "commands_DEPRECATED")
                            )
                            || command.has_user_specified_description
                            || command.when_to_use.is_some()
                    })
                    .map(|command| {
                        let mut entry = orchestrator::prompt::skill_listing::SkillListingEntry {
                            name: command.name.clone(),
                            display_name: command.name.clone(),
                            user_facing_name: command.user_facing_name.clone(),
                            unqualified_name: command.unqualified_name.clone(),
                            is_synced_skill: command.loaded_from.as_deref() == Some("syncedSkills"),
                            bare_name_reserved: command.bare_name_reserved,
                            aliases: command.aliases.clone(),
                            source: command.native_source.clone(),
                            plugin_name: command.plugin_name.clone(),
                            description: command.description.clone(),
                            when_to_use: command.when_to_use.clone().map(Into::into),
                            is_bundled_prompt: command.source == CommandSource::Bundled,
                            is_name_only: false,
                            priority: 0.0,
                        };
                        entry.display_name =
                            orchestrator::prompt::skill_listing::native_display_name(
                                &entry,
                                &full_catalog,
                            );
                        entry
                    })
                    .collect();
                Ok(orchestrator::prompt::skill_listing::SkillListingSnapshot {
                    entries,
                    budget_settings: Default::default(),
                })
            }
        }),
    )
}

/// Resolve selectable references from the already-probed host availability map.
pub fn ready_model_refs(
    api: &llm_runtime::ApiService,
    availability: &std::collections::BTreeMap<String, bool>,
    snapshot: &orchestrator::SideChatParentSnapshot,
) -> Vec<String> {
    let mut models = vec![lingxi_core::host::qualified_model_ref(
        &snapshot.config.model,
        snapshot.model_profile.as_deref(),
    )];
    for listing in api.model_listings().into_iter().filter(|listing| {
        !listing.hidden
            && availability
                .get(&listing.profile_name)
                .copied()
                .unwrap_or(false)
    }) {
        models.push(lingxi_core::host::qualified_model_ref(
            &listing.display_model,
            Some(&listing.profile_name),
        ));
        models.push(lingxi_core::host::qualified_model_ref(
            &listing.request_model,
            Some(&listing.profile_name),
        ));
    }
    models.sort();
    models.dedup();
    models
}

/// Native hosts inspect only the existing redacted credential source; this
/// never starts OAuth or executes credential helpers to populate a picker.
pub async fn credential_ready_model_refs(
    api: &llm_runtime::ApiService,
    snapshot: &orchestrator::SideChatParentSnapshot,
) -> Vec<String> {
    let mut availability = std::collections::BTreeMap::new();
    for listing in api
        .model_listings()
        .into_iter()
        .filter(|listing| !listing.hidden)
    {
        if availability.contains_key(&listing.profile_name) {
            continue;
        }
        let ready = api
            .credential_source(&listing.request_model, Some(&listing.profile_name))
            .await
            .is_ok_and(|source| {
                !matches!(
                    source,
                    llm_runtime::CredentialSource::None | llm_runtime::CredentialSource::Unknown
                )
            });
        availability.insert(listing.profile_name, ready);
    }
    ready_model_refs(api, &availability, snapshot)
}

#[cfg(test)]
mod tests {
    use super::{compaction, compaction_runner};
    use llm_runtime::test_support::FixtureTransport;
    use llm_runtime::{
        ApiService, AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig,
        LlmError, ModelProfile, ModelRuntime, PricingConfig, ProviderProfile, ProviderRequest,
        ProviderResponse, Region, StreamingResponse,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[derive(Default)]
    struct CountingTransport(AtomicUsize);

    impl FixtureTransport for CountingTransport {
        fn execute<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(LlmError::UnsupportedCapability {
                    capability: "unexpected test transport dispatch".into(),
                })
            })
        }

        fn open_stream<'a>(
            &'a self,
            _: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(LlmError::UnsupportedCapability {
                    capability: "unexpected test stream dispatch".into(),
                })
            })
        }
    }

    llm_runtime::impl_fixture_transport!(CountingTransport);

    fn api() -> (Arc<ApiService>, Arc<CountingTransport>) {
        let profile = ProviderProfile {
            wire_profile: None,
            regions: Region::all(),
            provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".into(),
            base_url: "https://unused.invalid".into(),
            protocol: llm_runtime::ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::None,
            credential: CredentialConfig::None,
            models: vec![ModelProfile {
                display_model: "claude-opus-4-6".into(),
                request_model: "claude-opus-4-6".into(),
                billing_model: "claude-opus-4-6".into(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    reasoning: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        };
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile],
            })
            .expect("test provider config"),
        );
        let transport = Arc::new(CountingTransport::default());
        let api = Arc::new(ApiService::new(
            client,
            transport.clone(),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
        ));
        (api, transport)
    }

    fn fork_request() -> sidequery::ForkedAgentRequest {
        sidequery::ForkedAgentRequest {
            prompt_messages: Vec::new(),
            cache_safe_params: sidequery::CacheSafeParams {
                system_prompt: Arc::new(lingxi_core::types::utf16_json::Utf16JsonProjection::from(
                    "",
                )),
                system_prompt_input: None,
                user_context: Default::default(),
                system_context: Default::default(),
                user_context_message: None,
                tool_use_options: tool_api::ToolUseOptions {
                    debug: false,
                    verbose: false,
                    main_loop_model: "claude-opus-4-6".into(),
                    model_profile: None,
                    max_budget_nano_usd: None,
                    mcp_clients: Vec::new(),
                    is_non_interactive_session: false,
                    custom_system_prompt: None,
                    append_system_prompt: None,
                },
                model_source: None,
                tools: Vec::new(),
                effort: None,
                fork_context_messages: Vec::new(),
                transcript_path: None,
                typed_attachment_ids: Default::default(),
                task_deliveries: Default::default(),
                generation: 0,
            },
            fork_label: "test-fork".into(),
            query_source: sidequery::QuerySource::Compaction,
            max_output_tokens: None,
        }
    }

    #[tokio::test]
    async fn native_only_overlay_is_deferred_to_fork_and_stops_before_transport() {
        let (api, transport) = api();
        api.set_max_thinking_tokens_override(Some(
            llm_runtime::model::thinking_state::NativeIntegerNumber::new(f64::from(u32::MAX) + 1.0)
                .expect("valid Native safe integer outside generic u32 budget"),
        ));
        let expected = api
            .thinking()
            .expect_err("generic fork cannot represent the Native overlay");

        let _compaction = compaction(api.clone(), "claude-opus-4-6");
        let runner = compaction_runner(api, "claude-opus-4-6");
        let error = runner
            .run(fork_request())
            .await
            .expect_err("the deferred Native overlay error reaches the actual fork");
        let actual = match error {
            sidequery::ForkError::Api(sidequery::SideQueryError::Api(error)) => error,
            other => panic!("unexpected fork error: {other:?}"),
        };
        assert_eq!(actual, expected);
        assert_eq!(transport.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn compaction_accepts_generic_session_thinking() {
        let (api, _) = api();
        api.set_thinking(llm_runtime::model::thinking::ThinkingConfig::Enabled {
            budget_tokens: 4096,
        });

        let _compaction = compaction(api, "claude-opus-4-6");
    }
}
