use super::{MobileEngineError, MobileEngineHandle};

/// Trusted provider/account routing context for app-owned media services.
/// This stays in the runtime host API rather than the generic client protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSessionContext {
    /// Current conversation session ID.
    pub session_id: String,
    /// Exact selected provider profile.
    pub profile_id: String,
    /// Stable account namespace selected by the host.
    pub account_scope: String,
    /// Host-selected product region.
    pub region: String,
}

impl MobileEngineHandle {
    /// Return the trusted provider/account binding used by this handle's
    /// current conversation session. The app uses this snapshot to register
    /// owner-scoped media routes without inspecting model names or credentials.
    pub async fn current_media_context(&self) -> Result<MediaSessionContext, MobileEngineError> {
        let (session_id, profile_id) = self
            .inner
            .orchestrator
            .current_audio_binding()
            .await
            .map_err(|error| MobileEngineError::Internal(error.to_string()))?;
        let region = match &self.inner.provider_region {
            llm_runtime::Region::ChinaMainland => "china_mainland",
            llm_runtime::Region::International => "international",
        };
        Ok(MediaSessionContext {
            session_id,
            account_scope: format!("profile:{profile_id}"),
            profile_id,
            region: region.to_owned(),
        })
    }

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
