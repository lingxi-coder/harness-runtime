//! Independent provider services for host applications.
//!
//! Keep one [`ProviderServices`] per configuration/transport lifetime. Its SDK
//! client shares HTTP resources and immutable configuration snapshots across
//! audio, image, embedding, batch and retrieval calls. Credentials, account
//! scopes and request deadlines are supplied on each operation.
//!
//! These operations do not enter [`crate::ApiService`] or its agent-turn retry,
//! permission and durable cost-accounting hooks. Hosts own authorization,
//! cancellation, storage and accounting for independent service operations.
//! Audio capture/playback remain host capabilities. Realtime sessions require
//! a separate [`realtime::RealtimeTransport`]; Responses WebSocket continuation
//! is not a bidirectional audio transport.

use std::sync::Arc;

/// The exact shared SDK version used by this runtime, including provider-specific
/// services and their request, response, error and resource-scope types.
pub use lingxi_llm_client as sdk;
pub use sdk::ClientSnapshot as ProviderServiceSnapshot;
pub use sdk::{audio, files, providers, realtime};

/// Shared service client using an explicitly selected usage region and transport.
#[derive(Clone)]
pub struct ProviderServices {
    client: sdk::LlmClient,
    transport: Arc<dyn sdk::Transport>,
    credential_source: Option<crate::ModelRuntime>,
}

impl ProviderServices {
    /// Build services over a native SDK transport. This preserves streamed
    /// request bodies and raw binary responses when the transport supports them.
    pub fn with_transport(
        profiles: &[sdk::protocol::ProviderProfile],
        region: sdk::protocol::Region,
        transport: Arc<dyn sdk::Transport>,
    ) -> Result<Self, sdk::BuildError> {
        Self::with_configured_transport(profiles, region, transport, |_| {})
    }

    /// Build services with host authenticators, clocks, attachment resolution or
    /// account sources registered on the same shared SDK builder.
    pub fn with_configured_transport(
        profiles: &[sdk::protocol::ProviderProfile],
        region: sdk::protocol::Region,
        transport: Arc<dyn sdk::Transport>,
        configure: impl FnOnce(&mut sdk::LlmClientBuilder),
    ) -> Result<Self, sdk::BuildError> {
        let mut builder =
            sdk::LlmClientBuilder::with_transport(transport.clone(), profiles).with_region(region);
        configure(&mut builder);
        Ok(Self {
            client: builder.build()?,
            transport,
            credential_source: None,
        })
    }

    pub(crate) fn with_credential_source(mut self, source: crate::ModelRuntime) -> Self {
        self.credential_source = Some(source);
        self
    }

    /// Resolve the exact configured key/token for an independent operation.
    /// This stays in Rust and uses the same source as inference, including
    /// authorized environment and host-managed sources. It never guesses an ID.
    pub async fn service_credential(
        &self,
        profile: &str,
    ) -> Result<sdk::protocol::Secret<String>, crate::LlmError> {
        let source = self.credential_source.as_ref().ok_or_else(|| {
            crate::LlmError::UnsupportedCapability {
                capability: "independent services have no configured host credential source".into(),
            }
        })?;
        source.service_credential(profile).await
    }

    /// Access all SDK services without duplicating their provider-specific APIs.
    /// Bind an exact profile using `provider::<providers::OpenAiClient>(name)`
    /// (or another provider client), then call its `audio()`, `files()` or other
    /// SDK resource. Credentials, account scopes and deadlines travel in each
    /// operation's `RequestOptions`.
    pub fn client(&self) -> &sdk::LlmClient {
        &self.client
    }

    /// Reuse the same transport for explicit-scope services such as Anthropic
    /// Skills or provider-specific realtime setup over HTTP.
    pub fn transport(&self) -> &dyn sdk::Transport {
        self.transport.as_ref()
    }

    /// Pin one configuration revision for related resource and model operations.
    pub fn snapshot(&self) -> ProviderServiceSnapshot {
        self.client.snapshot()
    }
}
