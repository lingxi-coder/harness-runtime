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
pub use sdk::{files, providers, realtime};

/// Shared service client using an explicitly selected usage region and transport.
#[derive(Clone)]
pub struct ProviderServices {
    client: sdk::LlmClient,
    transport: Arc<dyn sdk::Transport>,
}

impl ProviderServices {
    /// Build services over the host's HTTP transport without creating a second
    /// HTTP client. Streaming uploads require [`crate::Transport::send_stream_raw`].
    pub fn with_host_transport(
        profiles: &[sdk::protocol::ProviderProfile],
        region: sdk::protocol::Region,
        transport: Arc<dyn crate::Transport>,
    ) -> Result<Self, sdk::BuildError> {
        Self::with_transport(
            profiles,
            region,
            Arc::new(crate::execution::HostTransport(transport)),
        )
    }

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
        })
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
