//! Anthropic profile protocol is implemented by the SDK; tier policy stays here.
use crate::oauth::anthropic::limits::SubscriptionType;
pub use lingxi_llm_client::auth::oauth::anthropic::{
    fetch_profile_from_api_key, fetch_profile_from_oauth_token, fetch_user_roles, OAuthAccount,
    OAuthOrganization, OAuthProfileResponse, UserRolesResponse, BASE_API_URL, OAUTH_BETA_HEADER,
    ROLES_URL_PATH,
};

pub fn subscription_type(profile: &OAuthProfileResponse) -> Option<SubscriptionType> {
    match profile
        .organization
        .as_ref()
        .and_then(|org| org.organization_type.as_deref())
    {
        Some("claude_max") => Some(SubscriptionType::Max),
        Some("claude_pro") => Some(SubscriptionType::Pro),
        Some("claude_enterprise") => Some(SubscriptionType::Enterprise),
        Some("claude_team") => Some(SubscriptionType::Team),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::anthropic::testsupport::{Canned, MockHttp};
    use lingxi_llm_client::transport::Transport;
    use protocol::HttpMethod;
    use std::sync::Arc;
    use std::time::Duration;

    fn transport(status: u16, body: &str) -> Arc<dyn Transport> {
        MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status,
                body: body.into(),
            },
        )]) as Arc<dyn Transport>
    }

    #[tokio::test]
    async fn oauth_token_profile_parses_max_tier() {
        let body = r#"{"organization":{"organization_type":"claude_max","uuid":"org-1"},
                       "account":{"display_name":"Ada"}}"#;
        let t = transport(200, body);
        let profile = fetch_profile_from_oauth_token("tok-abc", t.as_ref())
            .await
            .expect("200 → Some(profile)");
        assert_eq!(subscription_type(&profile), Some(SubscriptionType::Max));
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
        let arc = mock.clone() as Arc<dyn Transport>;
        let profile = fetch_profile_from_oauth_token("tok-xyz", arc.as_ref()).await;
        assert_eq!(
            subscription_type(&profile.unwrap()),
            Some(SubscriptionType::Pro)
        );
        let req = mock.last_request().expect("a request was sent");
        assert!(req.url.ends_with("/api/oauth/profile"), "url = {}", req.url);
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer tok-xyz"));
        assert!(req.timeout.is_some_and(
            |timeout| timeout > Duration::from_secs(9) && timeout <= Duration::from_secs(10)
        ));
    }

    #[tokio::test]
    async fn non_200_is_swallowed_to_none() {
        let t = transport(403, r#"{"error":"forbidden"}"#);
        assert!(fetch_profile_from_oauth_token("tok", t.as_ref())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn malformed_body_is_swallowed_to_none() {
        let t = transport(200, "not json at all");
        // serde tolerates unknown fields but not invalid JSON → None.
        assert!(fetch_profile_from_oauth_token("tok", t.as_ref())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn unknown_org_type_resolves_to_none_tier() {
        let t = transport(
            200,
            r#"{"organization":{"organization_type":"claude_galaxy"}}"#,
        );
        let profile = fetch_profile_from_oauth_token("tok", t.as_ref())
            .await
            .unwrap();
        assert_eq!(subscription_type(&profile), None);
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
        let arc = mock.clone() as Arc<dyn Transport>;
        let profile = fetch_profile_from_api_key("acct-77", "sk-key", arc.as_ref()).await;
        assert_eq!(
            subscription_type(&profile.unwrap()),
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
        assert!(req.timeout.is_some_and(
            |timeout| timeout > Duration::from_secs(9) && timeout <= Duration::from_secs(10)
        ));
    }

    #[tokio::test]
    async fn fetch_user_roles_parses_role_fields() {
        let t = transport(
            200,
            r#"{"organization_role":"admin","workspace_role":"workspace_developer",
                "organization_name":"Acme"}"#,
        );
        let roles = fetch_user_roles("tok", t.as_ref())
            .await
            .expect("200 → Some(roles)");
        assert_eq!(roles.organization_role.as_deref(), Some("admin"));
        assert_eq!(roles.workspace_role.as_deref(), Some("workspace_developer"));
        assert_eq!(roles.organization_name.as_deref(), Some("Acme"));
    }

    #[tokio::test]
    async fn fetch_user_roles_swallows_non_200_and_transport_errors() {
        let t = transport(403, r#"{"error":"forbidden"}"#);
        assert!(fetch_user_roles("tok", t.as_ref()).await.is_none());
        // No routes → MockHttp returns Err (transport failure) → swallowed.
        let failing = MockHttp::new(vec![]) as Arc<dyn Transport>;
        assert!(fetch_user_roles("tok", failing.as_ref()).await.is_none());
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
        let arc = mock.clone() as Arc<dyn Transport>;
        let roles = fetch_user_roles("test-token", arc.as_ref()).await.unwrap();
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
        assert!(req.timeout.is_some_and(
            |timeout| timeout > Duration::from_secs(9) && timeout <= Duration::from_secs(10)
        ));
    }

    #[tokio::test]
    async fn fetch_user_roles_tolerates_unknown_and_missing_fields() {
        let t = transport(200, r#"{"organization_role":"member","unknown_field":1}"#);
        let roles = fetch_user_roles("tok", t.as_ref()).await.unwrap();
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
        let arc = mock.clone() as Arc<dyn Transport>;
        assert!(fetch_profile_from_api_key("", "sk-key", arc.as_ref())
            .await
            .is_none());
        assert!(fetch_profile_from_api_key("acct", "", arc.as_ref())
            .await
            .is_none());
        // No request should have been issued for the early-return cases.
        assert_eq!(mock.call_count(), 0);
    }
}
