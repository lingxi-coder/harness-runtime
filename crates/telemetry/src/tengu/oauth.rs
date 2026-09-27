//! `tengu_oauth_*` event schemas — 8 events.
//!
//! Spec §7 line 712-721. The five M3-04-emitted events
//! (`tengu_oauth_refresh_started`, `_succeeded`, `_failed`,
//! `tengu_oauth_scope_upgraded`, `tengu_oauth_proactive_canceled`) byte-match
//! the wire shape emitted by `llm_runtime::oauth::anthropic::refresh` and
//! `scope_upgrade`. The three PKCE schemas are M4-staged but locked here so
//! the PKCE runner can emit without bumping the schema tree.

use crate::pii::Verified;
use serde::{Deserialize, Serialize};

/// `tengu_oauth_refresh_started` — proactive or reactive refresh began.
pub const REFRESH_STARTED: &str = "tengu_oauth_refresh_started";
/// `tengu_oauth_refresh_succeeded` — refresh produced a new bearer token.
pub const REFRESH_SUCCEEDED: &str = "tengu_oauth_refresh_succeeded";
/// `tengu_oauth_refresh_failed` — refresh hit a transport or upstream error.
pub const REFRESH_FAILED: &str = "tengu_oauth_refresh_failed";
/// `tengu_oauth_scope_upgraded` — scope upgrade succeeded (granted ⊇ required).
pub const SCOPE_UPGRADED: &str = "tengu_oauth_scope_upgraded";
/// `tengu_oauth_proactive_canceled` — proactive task aborted (shutdown, jitter).
pub const PROACTIVE_CANCELED: &str = "tengu_oauth_proactive_canceled";
/// `tengu_oauth_pkce_started` — PKCE authorization-code flow began.
pub const PKCE_STARTED: &str = "tengu_oauth_pkce_started";
/// `tengu_oauth_pkce_completed` — PKCE handshake yielded a token.
pub const PKCE_COMPLETED: &str = "tengu_oauth_pkce_completed";
/// `tengu_oauth_pkce_failed` — PKCE handshake errored before token exchange.
pub const PKCE_FAILED: &str = "tengu_oauth_pkce_failed";

pub(crate) const NAMES: &[&str] = &[
    REFRESH_STARTED,
    REFRESH_SUCCEEDED,
    REFRESH_FAILED,
    SCOPE_UPGRADED,
    PROACTIVE_CANCELED,
    PKCE_STARTED,
    PKCE_COMPLETED,
    PKCE_FAILED,
];

// ── AWS auth-refresh trust-gate events (2.1.198) ────────────────────────────
//
// Kept OUT of the `tengu_oauth_` prefix block above (the
// category_ordering_preserved test locks per-block prefixes); appended at the
// GLOBAL TAIL of `ALL_EVENT_NAMES` like `tool::FILE_READ_ANALYTICS_NAMES`.
// Both events carry an EMPTY payload — the binary emits
// `G("tengu_awsAuthRefresh_missing_trust",{})` — so no `*Payload` struct.

/// `tengu_awsAuthRefresh_missing_trust` — the `awsAuthRefresh` command resolved
/// from project/local settings before workspace trust was confirmed; execution
/// refused (2.1.198 `ZBd` security gate). Emitted by
/// `llm_runtime::aws_auth::AwsAuthRefresher::refresh`.
pub const AWS_AUTH_REFRESH_MISSING_TRUST: &str = "tengu_awsAuthRefresh_missing_trust";

/// `tengu_awsCredentialExport_missing_trust` — same trust gate for the
/// `awsCredentialExport` command (2.1.198 `t2d`). Emitted by
/// `llm_runtime::aws_auth::AwsAuthRefresher::export_credentials`.
pub const AWS_CREDENTIAL_EXPORT_MISSING_TRUST: &str = "tengu_awsCredentialExport_missing_trust";

/// Global-tail block for the two AWS auth trust-gate events. Public so the
/// completeness test can assert the tail slice against this list.
pub const AWS_AUTH_NAMES: &[&str] = &[
    AWS_AUTH_REFRESH_MISSING_TRUST,
    AWS_CREDENTIAL_EXPORT_MISSING_TRUST,
];

/// OAuth refresh trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefreshTrigger {
    /// Token expiry within `min(remaining/2, 5min)`.
    Proactive,
    /// API responded 401.
    Reactive,
}

/// Payload for [`REFRESH_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshStartedPayload {
    /// Whether the refresh was proactive (timer-driven) or reactive (401-driven).
    pub trigger: RefreshTrigger,
}

/// Payload for [`REFRESH_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshSucceededPayload {
    /// Trigger that initiated the refresh.
    pub trigger: RefreshTrigger,
    /// Seconds until the newly-issued token expires.
    pub new_expires_in_secs: u64,
}

/// Payload for [`REFRESH_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshFailedPayload {
    /// Trigger that initiated the refresh attempt.
    pub trigger: RefreshTrigger,
    /// Whitelisted error kind (no raw upstream body).
    pub error: Verified,
}

/// Payload for [`SCOPE_UPGRADED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeUpgradedPayload {
    /// Newly-granted scope identifiers (each whitelisted).
    pub added_scopes: Vec<Verified>,
}

/// Payload for [`PROACTIVE_CANCELED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProactiveCanceledPayload {
    /// Whitelisted reason (shutdown, jitter, etc.).
    pub reason: Verified,
}

/// Payload for [`PKCE_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PkceStartedPayload {
    /// Authorization-server hostname (provider-specific).
    pub auth_host: Verified,
}

/// Payload for [`PKCE_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PkceCompletedPayload {
    /// Authorization-server hostname.
    pub auth_host: Verified,
    /// Wall-clock duration of the handshake in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`PKCE_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PkceFailedPayload {
    /// Authorization-server hostname.
    pub auth_host: Verified,
    /// Whitelisted error kind.
    pub error: Verified,
}
