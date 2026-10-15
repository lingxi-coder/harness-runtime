//! Small shared composition pieces for Desktop and native child factories.
use command_api::{CommandRegistry, SlashCommandKind};
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
    let query = Arc::new(sidequery::ProviderSideQueryClient::from_service(
        api.clone(),
    ));
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(query, model.to_string())
            .with_session_thinking(api.thinking()),
    );
    let compactor = Arc::new(
        compaction::CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            compaction::thresholds::auto_compact_threshold(model, &[]),
        )
        .with_model_derived_threshold(),
    );
    (compactor, slot)
}

/// Expose only eligible inline skills from the child registry and read state.
pub fn skill_listing(
    commands: Arc<RwLock<CommandRegistry>>,
    read_state: tool_api::read_file_state::ReadFileStateMap,
    cwd: PathBuf,
) -> Arc<dyn orchestrator::prompt::skill_listing::SkillListingProvider> {
    let conditional = Arc::new(std::sync::Mutex::new(skill_api::ConditionalSkills::new()));
    Arc::new(
        orchestrator::prompt::skill_listing::LazySkillListingProvider::new(move || {
            let commands = commands.clone();
            let read_state = read_state.clone();
            let conditional = conditional.clone();
            let cwd = cwd.clone();
            async move {
                let registry = commands.read().await;
                let touched = read_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .keys();
                registry
                    .model_invocable_commands()
                    .into_iter()
                    .filter(|command| registry.execution_allowed(command))
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
                    .map(
                        |command| orchestrator::prompt::skill_listing::SkillListingEntry {
                            name: command.name.clone(),
                            description: command.description.clone(),
                            when_to_use: command.when_to_use.clone(),
                            is_bundled: command.loaded_from.as_deref() == Some("bundled"),
                        },
                    )
                    .collect()
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
