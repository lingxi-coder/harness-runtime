//! Serde-friendly client configuration.

use crate::ProviderId;
use serde::{Deserialize, Serialize};

/// Raw client configuration supplied by hosts, files, or tests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Configured provider profiles.
    #[serde(default)]
    pub providers: Vec<ProviderProfile>,
}

/// AWS `SigV4` signing region + service for a provider profile.
///
/// Required when [`AuthStrategy::AwsSigV4`] is used. The region and service
/// are needed to build the credential scope string in the `Authorization`
/// header: `<date>/<region>/<service>/aws4_request`.
///
/// Example for Amazon Bedrock in us-east-1:
/// ```json
/// { "region": "us-east-1", "service": "bedrock" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningConfig {
    /// AWS region (e.g. `"us-east-1"`).
    pub region: String,
    /// AWS service name (e.g. `"bedrock"`, `"execute-api"`).
    pub service: String,
}

/// Azure `OpenAI` API-version configuration.
///
/// Required when [`ProtocolFamily::AzureOpenAi`] is used. The API version is
/// appended as a query parameter (`?api-version=<api_version>`) per the Azure
/// `OpenAI` REST specification:
/// <https://learn.microsoft.com/en-us/azure/ai-services/openai/reference>
///
/// Example:
/// ```json
/// { "apiVersion": "2024-02-01" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureConfig {
    /// Azure `OpenAI` API version string (e.g. `"2024-02-01"`).
    #[serde(rename = "apiVersion")]
    pub api_version: String,
}

/// Which failures move a request to the next CONNECTION of the same provider.
///
/// Every field defaults to `false`, so a profile that never opts in behaves
/// exactly as it did before connections existed: the drive loop's retry/fallback
/// decisions are reached untouched. Only a provider that actually declares
/// `connections` or `credentialIds` gets [`Self::DEFAULT`].
///
/// Deliberately excluded: `ModelUnavailable` (it conflates a local registry miss
/// with a provider 404, so a config typo would masquerade as a dead endpoint and
/// burn the whole chain), and every request-shaped error — `InvalidRequest`,
/// `ContextOverflow`, `RequestTooLarge`, `UnsupportedCapability` — which another
/// endpoint would reject identically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverTriggers {
    /// 429 from this connection.
    #[serde(default)]
    pub rate_limit: bool,
    /// 529 overloaded.
    #[serde(default)]
    pub overloaded: bool,
    /// 5xx / provider-internal.
    #[serde(default)]
    pub server_error: bool,
    /// Transport failure or timeout reaching this endpoint.
    #[serde(default)]
    pub network: bool,
    /// 401/403 — the usual reason to rotate to the next key.
    #[serde(default)]
    pub auth: bool,
}

impl FailoverTriggers {
    /// What a provider gets when it declares connections without naming
    /// triggers: everything that another endpoint or key could plausibly answer.
    pub const DEFAULT: Self = Self {
        rate_limit: true,
        overloaded: true,
        server_error: true,
        network: true,
        auth: true,
    };

    /// No trigger set — never fail over.
    pub const NONE: Self = Self {
        rate_limit: false,
        overloaded: false,
        server_error: false,
        network: false,
        auth: false,
    };

    /// Whether no trigger is set, i.e. failover is off for this profile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }

    /// Turn one settings token into the trigger it enables.
    ///
    /// Returns `None` for an unknown token so the caller can warn by name
    /// instead of silently ignoring a typo.
    #[must_use]
    pub fn apply_name(&mut self, name: &str) -> Option<()> {
        match name {
            "rate_limit" | "rateLimit" => self.rate_limit = true,
            "overloaded" => self.overloaded = true,
            "server_error" | "serverError" => self.server_error = true,
            "network" => self.network = true,
            "auth" => self.auth = true,
            _ => return None,
        }
        Some(())
    }

    /// Whether `error` should move the request to the next connection.
    #[must_use]
    pub fn matches(self, error: &crate::LlmError) -> bool {
        use crate::LlmError;
        match error {
            LlmError::RateLimited { .. } | LlmError::QuotaExceeded => self.rate_limit,
            LlmError::Overloaded { .. } => self.overloaded,
            LlmError::ProviderInternal => self.server_error,
            LlmError::Transport { .. } | LlmError::TransportTimeout { .. } => self.network,
            LlmError::Authentication { .. }
            | LlmError::OAuthRefreshDead
            | LlmError::PermissionDenied { .. } => self.auth,
            _ => false,
        }
    }
}

/// Which provider GROUP a profile belongs to, and where it sits in that group's
/// ordered connection list.
///
/// A "connection" is one reachable way to talk to a provider: its own base URL,
/// wire protocol, auth strategy and credential. A provider that publishes both a
/// domestic and an international host, or that accepts several API keys, is one
/// group with several connections — not several providers.
///
/// The default is the historical shape: a profile is its own one-connection
/// group, so a `settings.providers` entry written before `connections` existed
/// keeps behaving exactly as it did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSpec {
    /// Group this connection belongs to. `None` = the profile stands alone, and
    /// [`ProviderProfile::group`] reports `profile_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Connection id within the group, unique per group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Position within the group; lower is tried first. Ties break on
    /// `profile_name` so ordering is total and stable.
    #[serde(default)]
    pub order: u32,
    /// Never offered in a model picker. Set on the second and later key slots of
    /// one connection, which exist only to be failed over onto.
    #[serde(default)]
    pub hidden: bool,
    /// Which failures move to the next connection of this group. Shared by every
    /// connection in the group, because it is the provider's policy, not the
    /// endpoint's.
    #[serde(default, skip_serializing_if = "FailoverTriggers::is_empty")]
    pub failover: FailoverTriggers,
}

impl ConnectionSpec {
    /// Whether this is the default (standalone, visible, first) identity.
    /// Used by `skip_serializing_if` so untouched profiles serialize unchanged.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Provider profile used to build one or more routes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Immutable upstream profile facts retained during host configuration projection.
    #[serde(skip)]
    pub wire_profile: Option<lingxi_llm_client::protocol::ProviderProfile>,
    /// Product usage regions in which this connection may execute.
    #[serde(default = "lingxi_llm_client::protocol::Region::all")]
    pub regions: Vec<lingxi_llm_client::protocol::Region>,
    /// Explicit provider identity used after route resolution.
    pub provider_id: ProviderId,
    /// Human/config profile name.
    pub profile_name: String,
    /// Base URL for this provider profile.
    ///
    /// The expected shape differs by protocol family: `OpenAiChat` includes
    /// the version segment (`https://api.openai.com/v1`), `AnthropicMessages`
    /// is the bare origin (`https://api.anthropic.com`), and
    /// `GeminiGenerateContent` is the versioned root
    /// (`https://generativelanguage.googleapis.com/v1beta`).
    ///
    /// For `AzureOpenAi` the base URL should be the resource endpoint without
    /// the deployment segment, e.g.
    /// `https://<resource>.openai.azure.com`.  The codec appends
    /// `/openai/deployments/{model}/chat/completions?api-version=...`.
    pub base_url: String,
    /// Wire protocol family used by this route.
    pub protocol: ProtocolFamily,
    /// Auth application strategy.
    pub auth: AuthStrategy,
    /// Credential lookup reference.
    pub credential: CredentialConfig,
    /// Supported models for this profile.
    #[serde(default)]
    pub models: Vec<ModelProfile>,
    /// Pricing behavior for this profile.
    #[serde(default)]
    pub pricing: PricingConfig,
    /// AWS `SigV4` signing region + service.
    ///
    /// Required when `auth = AwsSigV4`.  Missing → `InvalidRequest` at auth
    /// time (naming the profile and field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<SigningConfig>,
    /// Azure `OpenAI` API-version configuration.
    ///
    /// Required when `protocol = AzureOpenAi`.  Missing → `InvalidRequest` at
    /// codec-build time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure: Option<AzureConfig>,
    /// Whether this `OpenAI` Responses provider supports the Responses WebSocket
    /// transport. Defaults to false so existing profiles keep HTTP SSE
    /// streaming unless they opt in explicitly.
    #[serde(default)]
    pub supports_websockets: bool,
    /// Whether this provider profile can negotiate WebSocket
    /// permessage-deflate for Responses WebSocket transport.
    ///
    /// Defaults to false. The current platform transport keeps this disabled
    /// until the underlying tungstenite dependency exposes a stable compression
    /// configuration surface.
    #[serde(default)]
    pub supports_websocket_compression: bool,
    /// Optional WebSocket connect timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Optional same-profile vision delegate model used when the selected main
    /// model cannot accept image input.
    #[serde(
        default,
        rename = "visionDelegate",
        skip_serializing_if = "Option::is_none"
    )]
    pub vision_delegate: Option<String>,
    /// Group + ordering identity. Defaults to "this profile is its own
    /// one-connection group", which is how every pre-`connections` profile and
    /// every built-in preset behaves.
    #[serde(default, skip_serializing_if = "ConnectionSpec::is_default")]
    pub connection: ConnectionSpec,
}

impl ProviderProfile {
    /// The provider group this profile belongs to.
    ///
    /// Falls back to `profile_name`, so a standalone profile is a group of one
    /// and every caller can reason in groups without special-casing.
    #[must_use]
    pub fn group(&self) -> &str {
        self.connection
            .group
            .as_deref()
            .unwrap_or(&self.profile_name)
    }

    /// This profile's connection id within its group (`"default"` when it is a
    /// standalone profile).
    #[must_use]
    pub fn connection_id(&self) -> &str {
        self.connection
            .connection_id
            .as_deref()
            .unwrap_or("default")
    }

    /// Sort key giving a total, stable order over one group's connections.
    #[must_use]
    pub fn connection_sort_key(&self) -> (u32, &str) {
        (self.connection.order, self.profile_name.as_str())
    }
}

pub use lingxi_llm_client::protocol::{AuthStrategy, ProtocolFamily};

/// Serializable credential reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum CredentialConfig {
    /// Load secret material from an environment variable.
    Env {
        /// Environment variable name.
        var: String,
    },
    /// Load static host-supplied secret by id.
    Static {
        /// Host-defined static credential id.
        id: String,
    },
    /// Ask the host-managed secret store for this id.
    HostManaged {
        /// Host-managed secret id.
        id: String,
    },
    /// No credential is loaded; requests for this profile are sent without
    /// client-applied authentication.
    None,
}

/// Model profile declared within a provider profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProfile {
    /// Human-facing model label.
    pub display_model: String,
    /// Provider-local model value sent on the wire.
    pub request_model: String,
    /// Model id used by pricing lookup.
    pub billing_model: String,
    /// Alternate names accepted by registry resolution.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Optional one-line human description, surfaced as a dimmed sub-line in the
    /// `/model` picker. Sourced from the catalog (models.dev `description`) when
    /// present; `None` otherwise.
    #[serde(default)]
    pub description: Option<String>,
    /// Provider-published display metadata. This is informational and never
    /// participates in model routing.
    #[serde(default)]
    pub metadata: platform_api::ModelMetadata,
    /// Model capabilities used for preflight validation.
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// Capabilities advertised by a configured model route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    /// Whether streaming is supported.
    pub streaming: bool,
    /// Whether native tool calls are supported.
    pub tools: bool,
    /// Whether image input is supported.
    pub vision: bool,
    /// Whether document input is supported.
    pub documents: bool,
    /// Whether reasoning controls are supported.
    pub reasoning: bool,
    /// Whether structured-output controls are supported.
    pub structured_output: bool,
}

impl Capabilities {
    /// Whether this route accepts a non-text input that this client can
    /// represent as image or document media.
    ///
    /// Audio/video modalities remain provider-specific and are not currently
    /// admitted by the conversation protocol, so they are intentionally not
    /// folded into this marker.
    #[must_use]
    pub const fn supports_multimodal(self) -> bool {
        self.vision || self.documents
    }
}

/// Pricing resolution behavior for a profile.
///
/// ## Per-model overrides
///
/// `overrides` is a list of `(model_id, TokenPricing)` pairs, where `model_id`
/// is the **display model** (the `id` key from the `models` array in settings).
/// At the host build step, each override is applied onto the `PricingCatalog`
/// keyed by the model's **billing model** (resolved via the profile's
/// [`ModelProfile`] table).
///
/// Serde round-trips the field; absent → empty vec (existing configs unaffected).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingConfig {
    /// Whether this provider charges per token, via subscription, or is
    /// explicitly free. Unknown is distinct from free.
    #[serde(default, rename = "billingMode")]
    pub billing_mode: platform_api::ModelBillingMode,
    /// Whether missing pricing must fail instead of returning unestimated cost.
    #[serde(default)]
    pub require_priced: bool,
    /// Per-model price overrides declared in the `providers.<name>.pricing` object.
    ///
    /// Each entry is `(display_model_id, TokenPricing)`.  The billing-model
    /// resolution and catalog insertion happen at the host build step, not at
    /// parse time.  Absent → empty (no overrides).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<(String, crate::cost::TokenPricing)>,
}
