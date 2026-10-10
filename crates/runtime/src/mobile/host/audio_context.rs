use super::MobileEngineHandle;

impl MobileEngineHandle {
    /// Exact effective runtime profiles and host credential manager. This is a
    /// Rust-only boundary; no credentials or authentication headers cross FFI.
    pub fn audio_provider_services(
        &self,
    ) -> (
        llm_runtime::services::ProviderServices,
        std::collections::BTreeMap<String, String>,
        std::sync::Arc<secret::CredentialManager>,
    ) {
        (
            self.inner.audio_services.clone(),
            self.inner.audio_credential_ids.clone(),
            self.inner.credentials.clone(),
        )
    }
    /// Rust-only access for the app's realtime adapter; tools and history remain
    /// owned by this exact engine rather than a separately constructed agent.
    pub fn audio_orchestrator(&self) -> std::sync::Arc<orchestrator::ConversationOrchestrator> {
        self.inner.orchestrator.clone()
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileEngineHandle {
    /// Non-secret, authoritative binding for a native host's audio operation.
    /// An unresolved profile stays unavailable rather than selecting a vendor
    /// from the model spelling or reusing another session's account.
    pub async fn audio_session_context(&self) -> String {
        match self.inner.orchestrator.current_audio_binding().await {
            Ok((session_id, profile)) => serde_json::json!({
                "sessionId":session_id,
                "profileId":profile,
                "accountScope":format!("profile:{profile}"),
                "region":self.inner.provider_region,
            }).to_string(),
            Err(_) => serde_json::json!({"error":{
                "kind":"needs_configuration", "message":"audio requires an exact session provider profile"
            }}).to_string(),
        }
    }
}
