use super::ConversationOrchestrator;

impl ConversationOrchestrator {
    /// Read authority at the API call boundary. A host-provided observation
    /// retains its provenance; otherwise use the actual env and session MCP
    /// state. An unavailable feature resolver stays unknown.
    pub(crate) async fn resolved_mod_projects_consent_facts(
        &self,
    ) -> hooks::mods::ProjectsConsentFacts {
        let mut facts = self.config.mod_projects_consent.clone();
        if facts.default_host_sticky_latch.is_none() {
            facts.default_host_sticky_latch = self
                .mcp_registry
                .as_ref()
                .and_then(|registry| registry.projects_session_default_host_enabled());
        }
        facts.projects_env.get_or_insert_with(|| {
            mcp::projects_session::parse_env_bool(
                std::env::var("CLAUDE_CODE_PROJECTS_SESSION")
                    .ok()
                    .as_deref(),
            )
        });
        if facts.session_mcp_signal.is_none() {
            facts.session_mcp_signal = match &self.mcp_registry {
                Some(registry) => registry.projects_session_signal().await,
                None => Some(false),
            };
        }
        // Native 2.1.290 reads GrowthBook's live `hasUsedNonDefaultHost()`.
        // Preserve `None` when this host has no equivalent trusted producer;
        // the LLM provider route is not that fact.
        facts
    }
}
