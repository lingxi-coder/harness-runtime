//! OAuth profile fetch (subscription tier resolution).
//!
//! Ports claude-code `services/oauth/getOauthProfile.ts` (lines 7-53):
//! - [`fetch_profile_from_oauth_token`] → `GET {BASE_API_URL}/api/oauth/profile`
//!   with `Authorization: Bearer <token>`.
//! - [`fetch_profile_from_api_key`] → `GET {BASE_API_URL}/api/claude_cli_profile`
//!   with `x-api-key` + `anthropic-beta: oauth-2025-04-20` and an
//!   `account_uuid` query parameter.
//!
//! Both use a 10-second timeout and **swallow all errors → `None`**, matching
//! the TS `logError(error)` / `return undefined` behaviour (the caller treats a
//! missing profile as "tier unknown", never as a hard failure).
//!
//! The response shape ([`OAuthProfileResponse`]) mirrors the subset of the TS
//! `OAuthProfileResponse` we consume: the `organization.organization_type`
//! discriminant that drives subscription-type resolution
//! (`services/oauth/client.ts:366-387`). Unknown fields are ignored.

use crate::oauth::anthropic::limits::SubscriptionType;
use platform_api::HttpTransport;
use protocol::{HttpMethod, HttpRequest};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// `getOauthConfig().BASE_API_URL` — `constants/oauth.ts:85`. The profile
/// endpoints are always first-party; staging/custom bases are an ant-only
/// concern not wired here.
pub const BASE_API_URL: &str = "https://api.anthropic.com";

/// `OAUTH_BETA_HEADER` — `constants/oauth.ts:36`. Locked byte-for-byte.
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// Profile-fetch timeout. Matches the TS `timeout: 10000`.
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);

/// The user's organization record, as returned by the profile endpoints.
///
/// Only `organization_type` is consumed today (it maps 1:1 to
/// [`SubscriptionType`] per `client.ts:370-387`); the remaining optional
/// fields are carried for parity / future use and default to absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct OAuthOrganization {
    /// Organization tier discriminant, e.g. `"claude_max"`, `"claude_pro"`,
    /// `"claude_enterprise"`, `"claude_team"`. Unknown values resolve to
    /// `subscription_type == None`.
    #[serde(default)]
    pub organization_type: Option<String>,
    /// Organization UUID (used by `getOrganizationUUID` in TS).
    #[serde(default)]
    pub uuid: Option<String>,
    /// Rate-limit tier string (Max/Pro 5x etc.).
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    /// Billing type (`stripe_subscription`, `apple_subscription`, …).
    #[serde(default)]
    pub billing_type: Option<String>,
    /// Whether overage/extra usage is enabled for the org.
    #[serde(default)]
    pub has_extra_usage_enabled: Option<bool>,
    /// ISO8601 timestamp the subscription was created.
    #[serde(default)]
    pub subscription_created_at: Option<String>,
}

/// The user's account record (display name, created-at).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct OAuthAccount {
    /// Account UUID.
    #[serde(default)]
    pub uuid: Option<String>,
    /// Human-readable display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// ISO8601 timestamp the account was created.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Parsed profile response. Mirrors the TS `OAuthProfileResponse` subset we
/// read; unknown top-level fields are ignored (`serde` default-skips them).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct OAuthProfileResponse {
    /// Organization record (drives subscription-type resolution).
    #[serde(default)]
    pub organization: Option<OAuthOrganization>,
    /// Account record.
    #[serde(default)]
    pub account: Option<OAuthAccount>,
}

impl OAuthProfileResponse {
    /// Resolve the subscription tier from `organization.organization_type`,
    /// matching `services/oauth/client.ts:370-387`:
    ///
    /// | `organization_type` | tier |
    /// |---|---|
    /// | `claude_max` | [`SubscriptionType::Max`] |
    /// | `claude_pro` | [`SubscriptionType::Pro`] |
    /// | `claude_enterprise` | [`SubscriptionType::Enterprise`] |
    /// | `claude_team` | [`SubscriptionType::Team`] |
    /// | anything else / absent | `None` (conservatively unknown) |
    #[must_use]
    pub fn subscription_type(&self) -> Option<SubscriptionType> {
        match self
            .organization
            .as_ref()
            .and_then(|o| o.organization_type.as_deref())
        {
            Some("claude_max") => Some(SubscriptionType::Max),
            Some("claude_pro") => Some(SubscriptionType::Pro),
            Some("claude_enterprise") => Some(SubscriptionType::Enterprise),
            Some("claude_team") => Some(SubscriptionType::Team),
            // Unknown org type → return None (TS `default: subscriptionType = null`).
            _ => None,
        }
    }
}

/// Fetch the profile using an OAuth access token.
///
/// `GET {BASE_API_URL}/api/oauth/profile`, `Authorization: Bearer <token>`,
/// 10s timeout. Any transport/status/decode error → `None` (swallowed, per
/// `getOauthProfileFromOauthToken`). Non-200 statuses also yield `None`.
pub async fn fetch_profile_from_oauth_token(
    access_token: &str,
    transport: &Arc<dyn HttpTransport>,
) -> Option<OAuthProfileResponse> {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: format!("{BASE_API_URL}/api/oauth/profile"),
        headers: vec![
            ("Authorization".into(), format!("Bearer {access_token}")),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: None,
        body_bytes: None,
        timeout: Some(PROFILE_TIMEOUT),
    };
    let resp = transport.request(req).await.ok()?;
    if resp.status != 200 {
        return None;
    }
    serde_json::from_str::<OAuthProfileResponse>(&resp.body).ok()
}

/// Fetch the profile using an API key + account UUID.
///
/// `GET {BASE_API_URL}/api/claude_cli_profile`, `x-api-key` +
/// `anthropic-beta: oauth-2025-04-20`, with `account_uuid` as a query
/// parameter, 10s timeout. Any error → `None` (swallowed, per
/// `getOauthProfileFromApiKey`). The TS callsite also early-returns when
/// either `account_uuid` or `api_key` is empty; we reproduce that guard.
pub async fn fetch_profile_from_api_key(
    account_uuid: &str,
    api_key: &str,
    transport: &Arc<dyn HttpTransport>,
) -> Option<OAuthProfileResponse> {
    // TS: `if (!accountUuid || !apiKey) return`.
    if account_uuid.is_empty() || api_key.is_empty() {
        return None;
    }
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: format!(
            "{BASE_API_URL}/api/claude_cli_profile?account_uuid={}",
            urlencoding::encode(account_uuid)
        ),
        headers: vec![
            ("x-api-key".into(), api_key.to_string()),
            ("anthropic-beta".into(), OAUTH_BETA_HEADER.into()),
        ],
        body: None,
        body_bytes: None,
        timeout: Some(PROFILE_TIMEOUT),
    };
    let resp = transport.request(req).await.ok()?;
    if resp.status != 200 {
        return None;
    }
    serde_json::from_str::<OAuthProfileResponse>(&resp.body).ok()
}

/// Roles endpoint path — `constants/oauth.ts:93` (`ROLES_URL`).
pub const ROLES_URL_PATH: &str = "/api/oauth/claude_cli/roles";

/// Subset of the TS `UserRolesResponse` we consume
/// (`services/oauth/client.ts:283-301`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct UserRolesResponse {
    /// e.g. `"admin" | "billing" | "owner" | "primary_owner" | "member"`.
    #[serde(default)]
    pub organization_role: Option<String>,
    /// Workspace-scoped role.
    #[serde(default)]
    pub workspace_role: Option<String>,
    /// Human-readable organization name.
    #[serde(default)]
    pub organization_name: Option<String>,
}

/// Fetch the signed-in user's org/workspace roles.
///
/// `GET {BASE_API_URL}/api/oauth/claude_cli/roles` with `Authorization:
/// Bearer <token>`, 10s timeout. TS (`fetchAndStoreUserRoles`,
/// `client.ts:276-309`) THROWS on failure because the login flow wants the
/// error; this read-side port swallows everything → `None` (callers treat
/// missing roles as "role unknown", same stance as the profile fetch above).
pub async fn fetch_user_roles(
    access_token: &str,
    transport: &Arc<dyn HttpTransport>,
) -> Option<UserRolesResponse> {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: format!("{BASE_API_URL}{ROLES_URL_PATH}"),
        // Unlike the profile fetcher above, the TS sends ONLY the
        // Authorization header here (axios GET, no Content-Type) — mirrored.
        headers: vec![("Authorization".into(), format!("Bearer {access_token}"))],
        body: None,
        body_bytes: None,
        timeout: Some(PROFILE_TIMEOUT),
    };
    let resp = transport.request(req).await.ok()?;
    if resp.status != 200 {
        return None;
    }
    serde_json::from_str::<UserRolesResponse>(&resp.body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::anthropic::testsupport::{Canned, MockHttp};

    fn transport(status: u16, body: &str) -> Arc<dyn HttpTransport> {
        MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status,
                body: body.into(),
            },
        )]) as Arc<dyn HttpTransport>
    }

    #[tokio::test]
    async fn oauth_token_profile_parses_max_tier() {
        let body = r#"{"organization":{"organization_type":"claude_max","uuid":"org-1"},
                       "account":{"display_name":"Ada"}}"#;
        let t = transport(200, body);
        let profile = fetch_profile_from_oauth_token("tok-abc", &t)
            .await
            .expect("200 → Some(profile)");
        assert_eq!(profile.subscription_type(), Some(SubscriptionType::Max));
        assert_eq!(
            profile.organization.as_ref().unwrap().uuid.as_deref(),
            Some("org-1")
        );
    }

    #[tokio::test]
    async fn oauth_token_profile_sends_bearer_header() {
        let mock = MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status: 200,
                body: r#"{"organization":{"organization_type":"claude_pro"}}"#.into(),
            },
        )]);
        let arc = mock.clone() as Arc<dyn HttpTransport>;
        let profile = fetch_profile_from_oauth_token("tok-xyz", &arc).await;
        assert_eq!(
            profile.unwrap().subscription_type(),
            Some(SubscriptionType::Pro)
        );
        let req = mock.last_request().expect("a request was sent");
        assert!(req.url.ends_with("/api/oauth/profile"), "url = {}", req.url);
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer tok-xyz"));
        assert_eq!(req.timeout, Some(Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn non_200_is_swallowed_to_none() {
        let t = transport(403, r#"{"error":"forbidden"}"#);
        assert!(fetch_profile_from_oauth_token("tok", &t).await.is_none());
    }

    #[tokio::test]
    async fn malformed_body_is_swallowed_to_none() {
        let t = transport(200, "not json at all");
        // serde tolerates unknown fields but not invalid JSON → None.
        assert!(fetch_profile_from_oauth_token("tok", &t).await.is_none());
    }

    #[tokio::test]
    async fn unknown_org_type_resolves_to_none_tier() {
        let t = transport(
            200,
            r#"{"organization":{"organization_type":"claude_galaxy"}}"#,
        );
        let profile = fetch_profile_from_oauth_token("tok", &t).await.unwrap();
        assert_eq!(profile.subscription_type(), None);
    }

    #[tokio::test]
    async fn api_key_profile_sends_headers_and_query() {
        let mock = MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status: 200,
                body: r#"{"organization":{"organization_type":"claude_enterprise"}}"#.into(),
            },
        )]);
        let arc = mock.clone() as Arc<dyn HttpTransport>;
        let profile = fetch_profile_from_api_key("acct-77", "sk-key", &arc).await;
        assert_eq!(
            profile.unwrap().subscription_type(),
            Some(SubscriptionType::Enterprise)
        );
        let req = mock.last_request().unwrap();
        assert!(req
            .url
            .contains("/api/claude_cli_profile?account_uuid=acct-77"));
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "sk-key"));
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "anthropic-beta" && v == "oauth-2025-04-20"));
        assert_eq!(req.timeout, Some(Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn fetch_user_roles_parses_role_fields() {
        let t = transport(
            200,
            r#"{"organization_role":"admin","workspace_role":"workspace_developer",
                "organization_name":"Acme"}"#,
        );
        let roles = fetch_user_roles("tok", &t)
            .await
            .expect("200 → Some(roles)");
        assert_eq!(roles.organization_role.as_deref(), Some("admin"));
        assert_eq!(roles.workspace_role.as_deref(), Some("workspace_developer"));
        assert_eq!(roles.organization_name.as_deref(), Some("Acme"));
    }

    #[tokio::test]
    async fn fetch_user_roles_swallows_non_200_and_transport_errors() {
        let t = transport(403, r#"{"error":"forbidden"}"#);
        assert!(fetch_user_roles("tok", &t).await.is_none());
        // No routes → MockHttp returns Err (transport failure) → swallowed.
        let failing = MockHttp::new(vec![]) as Arc<dyn HttpTransport>;
        assert!(fetch_user_roles("tok", &failing).await.is_none());
    }

    #[tokio::test]
    async fn fetch_user_roles_requests_roles_url_with_bearer() {
        let mock = MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status: 200,
                body: r#"{"organization_role":"member"}"#.into(),
            },
        )]);
        let arc = mock.clone() as Arc<dyn HttpTransport>;
        let roles = fetch_user_roles("test-token", &arc).await.unwrap();
        assert_eq!(roles.organization_role.as_deref(), Some("member"));
        let req = mock.last_request().expect("a request was sent");
        assert_eq!(
            req.url,
            "https://api.anthropic.com/api/oauth/claude_cli/roles"
        );
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer test-token"));
        // TS sends ONLY the Authorization header on this endpoint.
        assert!(!req.headers.iter().any(|(k, _)| k == "Content-Type"));
        assert_eq!(req.timeout, Some(Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn fetch_user_roles_tolerates_unknown_and_missing_fields() {
        let t = transport(200, r#"{"organization_role":"member","unknown_field":1}"#);
        let roles = fetch_user_roles("tok", &t).await.unwrap();
        assert_eq!(roles.organization_role.as_deref(), Some("member"));
        assert_eq!(roles.workspace_role, None);
        assert_eq!(roles.organization_name, None);
    }

    #[tokio::test]
    async fn api_key_profile_requires_both_inputs() {
        let mock = MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status: 200,
                body: "{}".into(),
            },
        )]);
        let arc = mock.clone() as Arc<dyn HttpTransport>;
        assert!(fetch_profile_from_api_key("", "sk-key", &arc)
            .await
            .is_none());
        assert!(fetch_profile_from_api_key("acct", "", &arc).await.is_none());
        // No request should have been issued for the early-return cases.
        assert_eq!(mock.call_count(), 0);
    }
}
