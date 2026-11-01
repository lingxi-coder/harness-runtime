//! Secret-free identities and host-local provider account configuration.
use serde::{Deserialize, Serialize};

/// Select an account at the first resource-creating operation; never replaces a concrete resource scope.
pub const UNBOUND_ACCOUNT_SCOPE: &str = "unbound";

/// Allowed credential form; provider capability policy still controls login entry points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountAuthKind {
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

/// An exact opaque slot in one credential storage domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRef {
    pub store_scope: String,
    pub secret_ref: String,
}

/// Stable account identity independent of upstream email, token, or display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMetadata {
    pub account_id: String,
    pub provider_id: String,
    pub display_name: String,
    pub auth_kind: AccountAuthKind,
    pub execution_binding_id: String,
    pub credential_ref: CredentialRef,
    pub enabled: bool,
    pub user_priority: u32,
    pub config_revision: u64,
    pub identity_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountHealthStatus {
    Ready,
    CoolingDown,
    NeedsAuth,
    QuotaExhausted,
    StorageUnavailable,
    Disabled,
}

/// Host-derived health never changes user priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountHealth {
    pub account_id: String,
    pub identity_generation: u64,
    pub status: AccountHealthStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until_ms: Option<u64>,
    pub health_revision: u64,
}

/// Account-specific inference authorization observations. Discovery is unverified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountModelEntitlementStatus {
    Unverified,
    Confirmed,
    Forbidden,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountModelEntitlement {
    pub account_id: String,
    pub identity_generation: u64,
    pub model_id: String,
    pub status: AccountModelEntitlementStatus,
    pub observed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Shared picker projection; eligibility follows host account routing policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountModelAvailability {
    pub provider_id: String,
    pub execution_binding_id: String,
    pub model_id: String,
    pub eligible_account_ids: Vec<String>,
    pub confirmed_account_ids: Vec<String>,
    pub unverified_account_ids: Vec<String>,
    pub forbidden_account_ids: Vec<String>,
}

/// Temporary per-model and per-endpoint health used by the shared picker projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountHealthScopeKind {
    Model,
    Endpoint,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountScopedHealth {
    pub account_id: String,
    pub identity_generation: u64,
    pub scope_kind: AccountHealthScopeKind,
    pub scope_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_until_ms: Option<u64>,
    pub reason: String,
}

/// A complete reconnect snapshot. Contains no authentication material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    #[serde(default)]
    pub provider_policies: std::collections::BTreeMap<String, ProviderAccountPolicy>,
    pub store_scope: String,
    pub config_revision: u64,
    pub accounts: Vec<AccountMetadata>,
    #[serde(default)]
    pub health: Vec<AccountHealth>,
    #[serde(default)]
    pub model_entitlements: Vec<AccountModelEntitlement>,
    #[serde(default)]
    pub model_availability: Vec<AccountModelAvailability>,
    #[serde(default)]
    pub scoped_health: Vec<AccountScopedHealth>,
}

/// Revision-fenced command. Each action has a typed payload and secrets travel separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountCommand {
    pub expected_revision: u64,
    #[serde(default)]
    pub operation_id: Option<u64>,
    #[serde(flatten)]
    pub action: ProviderAccountAction,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ProviderAccountAction {
    List,
    SetPolicy {
        provider_id: String,
        allow_subscription_to_metered: bool,
    },
    Add {
        provider_id: String,
        display_name: String,
        execution_binding_id: String,
        auth_kind: AccountAuthKind,
        #[serde(default)]
        account_id: Option<String>,
    },
    Update {
        account_id: String,
        display_name: String,
    },
    Reorder {
        provider_id: String,
        account_ids: Vec<String>,
    },
    SetEnabled {
        account_id: String,
        enabled: bool,
    },
    Remove {
        account_id: String,
    },
    Logout {
        account_id: String,
    },
    ClearCredential {
        account_id: String,
    },
    SetCredential {
        account_id: String,
    },
    Test {
        account_id: String,
    },
    BeginOauth {
        account_id: String,
    },
    PollOauth {
        account_id: String,
        flow_id: String,
    },
    CompleteOauth {
        account_id: String,
        flow_id: String,
        callback_url: String,
    },
    CancelOauth {
        account_id: String,
        flow_id: String,
    },
}

/// Live registry shared by host credentials and request scheduling.
pub type SharedAccountDirectory = std::sync::Arc<std::sync::RwLock<AccountSnapshot>>;

/// Explicit consent for crossing the subscription-to-metered billing boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAccountPolicy {
    #[serde(default)]
    pub allow_subscription_to_metered: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oauth_wire_and_typed_action_are_stable() {
        assert_eq!(
            serde_json::to_string(&AccountAuthKind::OAuth).unwrap(),
            "\"oauth\""
        );
        let command: ProviderAccountCommand = serde_json::from_str(
            r#"{"action":"begin_oauth","accountId":"a","expectedRevision":2}"#,
        )
        .unwrap();
        assert!(
            matches!(command.action,ProviderAccountAction::BeginOauth{account_id} if account_id=="a")
        );
        assert!(serde_json::from_str::<ProviderAccountCommand>(
            r#"{"action":"begin_oauth","accountId":"a","expectedRevision":2,"credential":"secret"}"#
        )
        .is_err());
    }
}

impl std::fmt::Debug for ProviderAccountAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self {
            Self::List => "List",
            Self::SetPolicy { .. } => "SetPolicy",
            Self::Add { .. } => "Add",
            Self::Update { .. } => "Update",
            Self::Reorder { .. } => "Reorder",
            Self::SetEnabled { .. } => "SetEnabled",
            Self::Remove { .. } => "Remove",
            Self::Logout { .. } => "Logout",
            Self::ClearCredential { .. } => "ClearCredential",
            Self::SetCredential { .. } => "SetCredential",
            Self::Test { .. } => "Test",
            Self::BeginOauth { .. } => "BeginOauth",
            Self::PollOauth { .. } => "PollOauth",
            Self::CompleteOauth { .. } => "CompleteOauth",
            Self::CancelOauth { .. } => "CancelOauth",
        };
        f.debug_tuple(action).field(&"<redacted>").finish()
    }
}

/// Sanitized failure facts for one candidate; never includes provider error bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountFailure {
    pub account_id: String,
    pub reason_code: String,
    pub scope_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    pub retryable: bool,
    pub recovery_at_ms: Option<u64>,
}
/// Ordered selection and aggregate failure observation for one logical call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountRoutingEvent {
    pub logical_call_id: String,
    pub model: String,
    pub attempt: u32,
    pub status: String,
    pub selected_account_id: Option<String>,
    pub profile: Option<String>,
    pub identity_generation: Option<u64>,
    pub config_revision: Option<u64>,
    pub failed_accounts: Vec<ProviderAccountFailure>,
    pub earliest_recovery_ms: Option<u64>,
}
