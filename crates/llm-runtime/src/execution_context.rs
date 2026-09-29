//! Host authority and recovery state for one logical model call.
//!
//! Kept outside model input so serialized requests cannot manufacture admission
//! authority or carry account identity across sessions. This context is never
//! forwarded to a provider; selected account scopes are passed as SDK options.

/// Host-owned execution state. Cloning preserves the logical attempt and its
/// recovery scope; it does not authorize another physical dispatch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionContext {
    /// Wire family used to prepare canonical history. Route changes adapt a
    /// clone through the SDK; absent for inputs authored directly for a route.
    pub input_protocol: Option<lingxi_llm_client::protocol::ProtocolFamily>,
    /// Exact UTF-16 strings indexed against the final SDK message sequence.
    /// Historical display strings remain valid Unicode; these overrides are
    /// applied only at the SDK JSON boundary.
    pub message_json_string_overrides: std::collections::BTreeMap<String, Vec<u16>>,
    /// Registered admission authority, never forwarded to the provider.
    pub model_attempt: Option<platform_api::ModelAttemptContext>,
    /// Historical identities used by signature recovery; never provider wire.
    pub thinking_source_message_ids: Vec<::protocol::MessageId>,
    /// Query ownership for retries and lazy streams; never provider wire.
    pub thinking_recovery_scope: Option<crate::thinking_scope::ThinkingRecoveryScope>,
    /// Trusted account identity for provider continuations and hosted resources.
    pub account_scope: Option<String>,
    /// Trusted host account identity for provider-owned file references.
    pub file_account_scope: Option<String>,
    /// Whether the host response should include per-call retry accounting.
    pub capture_retry_count: bool,
    /// Internal side-query purpose for telemetry; never sent to providers.
    pub query_source: Option<String>,
}
