//! Subscription resolution from OAuth scopes and the profile response.
//!
//! Ports the two claude-code predicates the retry / 429 / beta batches consume:
//! - [`subscription_from_scopes`] — `auth.ts:1564-1571` `isClaudeAISubscriber`,
//!   which delegates to `services/oauth/client.ts:38-40`
//!   `shouldUseClaudeAIAuth(scopes) = Boolean(scopes?.includes('user:inference'))`.
//! - [`is_enterprise`] — `auth.ts:1694` `isEnterpriseSubscriber` =
//!   `getSubscriptionType() === 'enterprise'`.
//!
//! Plus [`apply_profile`], which folds an [`OAuthProfileResponse`] into a
//! [`ClaudeAiLimitsState`] by populating `subscription_type` from the org-type
//! discriminant (`client.ts:370-387`).
//!
//! **Divergence (documented):** TS `isClaudeAISubscriber` first checks
//! `isAnthropicAuthEnabled()` and returns `false` when Anthropic auth is off
//! (env API keys / third-party providers). That gate depends on global config
//! we do not see here, so callers must apply it; [`subscription_from_scopes`]
//! ports only the scope check. `isEnterpriseSubscriber` reads
//! `getSubscriptionType()`, which can come from a mock (ant-only), the stored
//! token's `subscriptionType`, or the profile org-type — we resolve from the
//! tier we *have* and conservatively treat an absent / ambiguous tier as
//! non-enterprise and non-subscriber.

use crate::oauth::anthropic::limits::{ClaudeAiLimitsState, SubscriptionType};
use lingxi_llm_client::auth::oauth::anthropic::{
    fetch_profile_from_oauth_token, fetch_user_roles, OAuthProfileResponse,
};
use lingxi_llm_client::auth::oauth::anthropic::{has_profile_scope, subscription_from_scopes};
use lingxi_llm_client::auth::oauth::anthropic::{paid_subscription_type, subscription_type};
use lingxi_llm_client::transport::Transport;
use std::sync::Arc;

/// Port of `isEnterpriseSubscriber()` (`auth.ts:1694`): the resolved tier is
/// Enterprise.
///
/// TS reads `getSubscriptionType() === 'enterprise'`. We read the tier already
/// resolved into `state.subscription_type`; absent / ambiguous → `false`
/// (conservative). Team-with-PAYG is NOT enterprise in TS (`isTeamPremiumSubscriber`
/// is a distinct predicate), so we do not fold it in here.
#[must_use]
pub fn is_enterprise(state: &ClaudeAiLimitsState) -> bool {
    state.subscription_type == Some(SubscriptionType::Enterprise)
}

/// Whether the resolved tier is a paid Claude.ai subscription
/// (`max`/`pro`/`team`/`enterprise`). Useful for the api-client batches that
/// gate on subscriber-ness when scopes are unavailable but a profile was
/// fetched. `Free`/`Unknown`/absent → `false`.
#[must_use]
pub fn is_subscriber_tier(state: &ClaudeAiLimitsState) -> bool {
    matches!(
        state.subscription_type,
        Some(
            SubscriptionType::Max
                | SubscriptionType::Pro
                | SubscriptionType::Team
                | SubscriptionType::Enterprise
        )
    )
}

/// Fold a fetched [`OAuthProfileResponse`] into a [`ClaudeAiLimitsState`],
/// populating `subscription_type` from the org-type discriminant
/// (`client.ts:370-387` via [`OAuthProfileResponse::subscription_type`]).
///
/// Conservative: an unknown / absent org type leaves `subscription_type`
/// untouched (we do NOT clobber a previously-known tier with `None`), matching
/// the TS coalescing `profileInfo?.subscriptionType ?? existing?.subscriptionType`.
pub fn apply_profile(state: &mut ClaudeAiLimitsState, profile: &OAuthProfileResponse) {
    if let Some(tier) = subscription_type(&profile) {
        state.subscription_type = Some(tier);
    }
}

/// Resolve the signed-in user's full subscription snapshot from the OAuth
/// profile + roles endpoints — the port's analog of claude-code's
/// `getOauthAccountInfo()` (`auth.ts`, written at login from the profile + roles
/// responses). Gated on the `user:profile` scope (`hasProfileScope`,
/// `auth.ts:1580-1584`): a token without it can't hit `/api/oauth/profile`, so
/// we skip and return `None`. Any profile-fetch failure → `None`; roles are
/// best-effort (missing roles ⇒ `organization_role: None`).
pub async fn resolve_subscription_snapshot(
    access_token: &str,
    scopes: &[String],
    transport: &Arc<dyn Transport>,
) -> Option<platform_api::subscription::SubscriptionSnapshot> {
    if !has_profile_scope(scopes) {
        return None;
    }
    let profile = fetch_profile_from_oauth_token(access_token, transport.as_ref()).await?;
    // Roles are a second, best-effort call (claude-code fetches them alongside
    // the profile for `organizationRole`); failure leaves the role unknown.
    let roles = fetch_user_roles(access_token, transport.as_ref()).await;
    let org = profile.organization.as_ref();
    Some(platform_api::subscription::SubscriptionSnapshot {
        // `isClaudeAISubscriber` ← the `user:inference` scope.
        is_subscriber: subscription_from_scopes(scopes),
        subscription_type: subscription_type(&profile)
            .and_then(paid_subscription_type)
            .map(str::to_owned),
        rate_limit_tier: org.and_then(|o| o.rate_limit_tier.clone()),
        has_extra_usage_enabled: org.and_then(|o| o.has_extra_usage_enabled).unwrap_or(false),
        billing_type: org.and_then(|o| o.billing_type.clone()),
        organization_role: roles.and_then(|r| r.organization_role),
    })
}

/// Resolve [`resolve_subscription_snapshot`] and publish it to the process-global
/// [`platform_api::subscription`] cache, so subscription-gated logic (e.g. the
/// `AgentTool` pro-plan prompt gate) reflects the signed-in user's plan.
/// Best-effort: a missing `user:profile` scope or a failed fetch leaves the
/// cache unchanged (subscription stays "unknown", matching the binary default).
pub async fn publish_subscription(
    access_token: &str,
    scopes: &[String],
    transport: &Arc<dyn Transport>,
) {
    if let Some(snapshot) = resolve_subscription_snapshot(access_token, scopes, transport).await {
        platform_api::subscription::set_current_subscription(Some(snapshot));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn inference_scope_makes_subscriber() {
        assert!(subscription_from_scopes(&scopes(&[
            "user:profile",
            "user:inference",
            "org:create_api_key"
        ])));
    }

    #[test]
    fn missing_inference_scope_is_not_subscriber() {
        assert!(!subscription_from_scopes(&scopes(&["user:profile"])));
        assert!(!subscription_from_scopes(&scopes(&[])));
    }

    #[test]
    fn inference_only_token_is_subscriber() {
        // Long-lived inference-only tokens hardcode ['user:inference'].
        assert!(subscription_from_scopes(&scopes(&["user:inference"])));
    }

    #[test]
    fn profile_scope_detection() {
        assert!(has_profile_scope(&scopes(&[
            "user:inference",
            "user:profile"
        ])));
        assert!(!has_profile_scope(&scopes(&["user:inference"])));
    }

    #[test]
    fn enterprise_tier_is_enterprise() {
        let state = ClaudeAiLimitsState {
            subscription_type: Some(SubscriptionType::Enterprise),
            ..Default::default()
        };
        assert!(is_enterprise(&state));
        assert!(is_subscriber_tier(&state));
    }

    #[test]
    fn non_enterprise_tiers_are_not_enterprise() {
        for tier in [
            SubscriptionType::Free,
            SubscriptionType::Pro,
            SubscriptionType::Max,
            SubscriptionType::Team,
            SubscriptionType::Unknown,
        ] {
            let state = ClaudeAiLimitsState {
                subscription_type: Some(tier),
                ..Default::default()
            };
            assert!(!is_enterprise(&state), "{tier:?} must not be enterprise");
        }
    }

    #[test]
    fn ambiguous_absent_tier_is_conservative() {
        let state = ClaudeAiLimitsState::default();
        assert!(!is_enterprise(&state));
        assert!(!is_subscriber_tier(&state));
    }

    #[test]
    fn subscriber_tier_excludes_free_and_unknown() {
        for tier in [SubscriptionType::Free, SubscriptionType::Unknown] {
            let state = ClaudeAiLimitsState {
                subscription_type: Some(tier),
                ..Default::default()
            };
            assert!(
                !is_subscriber_tier(&state),
                "{tier:?} must not be a subscriber"
            );
        }
        for tier in [
            SubscriptionType::Pro,
            SubscriptionType::Max,
            SubscriptionType::Team,
            SubscriptionType::Enterprise,
        ] {
            let state = ClaudeAiLimitsState {
                subscription_type: Some(tier),
                ..Default::default()
            };
            assert!(is_subscriber_tier(&state), "{tier:?} must be a subscriber");
        }
    }

    #[test]
    fn apply_profile_populates_tier() {
        let profile = OAuthProfileResponse {
            organization: Some(
                lingxi_llm_client::auth::oauth::anthropic::OAuthOrganization {
                    organization_type: Some("claude_team".into()),
                    ..Default::default()
                },
            ),
            account: None,
        };
        let mut state = ClaudeAiLimitsState::default();
        apply_profile(&mut state, &profile);
        assert_eq!(state.subscription_type, Some(SubscriptionType::Team));
        assert!(is_subscriber_tier(&state));
    }

    #[test]
    fn apply_profile_does_not_clobber_known_tier_with_unknown() {
        let mut state = ClaudeAiLimitsState {
            subscription_type: Some(SubscriptionType::Max),
            ..Default::default()
        };
        let unknown = OAuthProfileResponse {
            organization: Some(
                lingxi_llm_client::auth::oauth::anthropic::OAuthOrganization {
                    organization_type: Some("claude_galaxy".into()),
                    ..Default::default()
                },
            ),
            account: None,
        };
        apply_profile(&mut state, &unknown);
        // Known Max tier survives an unknown-org-type profile.
        assert_eq!(state.subscription_type, Some(SubscriptionType::Max));
    }

    #[tokio::test]
    async fn resolve_and_publish_pro_plan_to_global() {
        use crate::oauth::anthropic::testsupport::{Canned, MockHttp, SUBSCRIPTION_CACHE_LOCK};
        // Serialize: `publish_subscription` mutates the process-global cache.
        let _g = SUBSCRIPTION_CACHE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let body = r#"{"organization":{"organization_type":"claude_pro","uuid":"o1",
            "rate_limit_tier":"default_claude_pro","billing_type":"stripe_subscription",
            "has_extra_usage_enabled":true}}"#;
        let transport: Arc<dyn Transport> = MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status: 200,
                body: body.into(),
            },
        )]);
        let scopes = vec!["user:inference".to_string(), "user:profile".to_string()];

        let snap = resolve_subscription_snapshot("tok", &scopes, &transport)
            .await
            .expect("profile scope + 200 ⇒ Some");
        assert_eq!(snap.subscription_type.as_deref(), Some("pro"));
        assert!(snap.is_subscriber, "user:inference ⇒ subscriber");
        assert_eq!(snap.rate_limit_tier.as_deref(), Some("default_claude_pro"));
        assert_eq!(snap.billing_type.as_deref(), Some("stripe_subscription"));
        assert!(snap.has_extra_usage_enabled);

        // publish → the process-global cache reflects the pro plan.
        platform_api::subscription::set_current_subscription(None);
        publish_subscription("tok", &scopes, &transport).await;
        assert!(
            platform_api::subscription::is_pro_plan(),
            "global must report pro after publish (this is what activates the F3 gate)"
        );
        platform_api::subscription::set_current_subscription(None);
    }

    #[tokio::test]
    async fn resolve_skips_without_profile_scope() {
        use crate::oauth::anthropic::testsupport::{Canned, MockHttp};
        let body = r#"{"organization":{"organization_type":"claude_pro"}}"#;
        let transport: Arc<dyn Transport> = MockHttp::new(vec![(
            "anthropic.com",
            Canned {
                status: 200,
                body: body.into(),
            },
        )]);
        // No `user:profile` scope ⇒ `hasProfileScope` gate skips the fetch ⇒ None.
        let scopes = vec!["user:inference".to_string()];
        assert!(resolve_subscription_snapshot("tok", &scopes, &transport)
            .await
            .is_none());
    }
}
