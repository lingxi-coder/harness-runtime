//! Claude.ai subscription-tier snapshot shared between the composition roots
//! (which resolve it from the OAuth profile/roles endpoints) and UI layers
//! (which branch rate-limit copy on it).
//!
//! Mirrors the TS subscription data sources: `getSubscriptionType()` /
//! `getRateLimitTier()` (`utils/auth.ts:1662-1712`, keychain token fields),
//! and `getOauthAccountInfo()` (`hasExtraUsageEnabled` / `billingType` /
//! `organizationRole`, written at login from the profile + roles endpoints).
//! Tier strings use the TS union values verbatim: `"pro" | "max" | "team" |
//! "enterprise"`. Absent / unknown tier ⇒ every predicate is conservative
//! `false`, reproducing the pre-plumbing scope-guard behaviour byte-for-byte.

use std::sync::{Arc, RwLock};

/// Point-in-time view of the signed-in user's subscription. `Default` is the
/// "unknown subscription" state (all predicates `false`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionSnapshot {
    /// `isClaudeAISubscriber()` (`auth.ts:1564`) — pre-computed at the
    /// composition root from token scopes + auth-source precedence.
    pub is_subscriber: bool,
    /// `getSubscriptionType()`: `"pro" | "max" | "team" | "enterprise"`.
    pub subscription_type: Option<String>,
    /// `getRateLimitTier()`: e.g. `"default_claude_max_20x"`.
    pub rate_limit_tier: Option<String>,
    /// `oauthAccount.hasExtraUsageEnabled === true`.
    pub has_extra_usage_enabled: bool,
    /// `oauthAccount.billingType`, e.g. `"stripe_subscription"`.
    pub billing_type: Option<String>,
    /// `oauthAccount.organizationRole`, e.g. `"admin"`.
    pub organization_role: Option<String>,
}

impl SubscriptionSnapshot {
    /// `subscriptionType === 'team' || subscriptionType === 'enterprise'`.
    #[must_use]
    pub fn is_team_or_enterprise(&self) -> bool {
        matches!(
            self.subscription_type.as_deref(),
            Some("team" | "enterprise")
        )
    }

    /// `subscriptionType === 'pro' || subscriptionType === 'enterprise'`
    /// (the `seven_day_sonnet` naming gate, `rateLimitMessages.ts:176-181`).
    #[must_use]
    pub fn is_pro_or_enterprise(&self) -> bool {
        matches!(
            self.subscription_type.as_deref(),
            Some("pro" | "enterprise")
        )
    }

    /// 2.1.206 `Tdo`'s `isMax20x` (binary @221158587): `oly =
    /// subscriptionType==="max" && rateLimitTier==="default_claude_max_20x"`
    /// — a TWO-conjunct check (pre-206 `RateLimitMessage.tsx:75` checked the
    /// tier alone; 206 additionally requires the `"max"` subscription type).
    #[must_use]
    pub fn is_max20x(&self) -> bool {
        self.subscription_type.as_deref() == Some("max")
            && self.rate_limit_tier.as_deref() == Some("default_claude_max_20x")
    }

    /// Port of `isOverageProvisioningAllowed` (`auth.ts:1623-1643`): must be a
    /// subscriber with a billing type that can purchase extra usage (Stripe or
    /// mobile billing).
    ///
    /// Also serves as `QJe()` in the 2.1.206 rate-limit messages: the four
    /// arms below (`stripe_subscription` / `stripe_subscription_contracted` /
    /// `apple_subscription` / `google_play_subscription`) are exactly `QJe`'s
    /// four billing types.
    #[must_use]
    pub fn is_overage_provisioning_allowed(&self) -> bool {
        if !self.is_subscriber {
            return false;
        }
        matches!(
            self.billing_type.as_deref(),
            Some(
                "stripe_subscription"
                    | "stripe_subscription_contracted"
                    | "apple_subscription"
                    | "google_play_subscription"
            )
        )
    }

    /// Port of `hasClaudeAiBillingAccess` (`billing.ts:53-78`; the
    /// `/mock-limits` override is not ported). Consumer plans always have
    /// billing access; Team/Enterprise gate on the org role.
    ///
    /// Also serves as `tC()` / `hasBillingAccess` in the 2.1.206
    /// `getUpsellMessage` (`Bo()&&(max|pro || orgRole∈…)`); byte-equivalent
    /// given `Bo()≈is_subscriber`.
    #[must_use]
    pub fn has_claude_ai_billing_access(&self) -> bool {
        if !self.is_subscriber {
            return false;
        }
        match self.subscription_type.as_deref() {
            Some("max" | "pro") => true,
            _ => matches!(
                self.organization_role.as_deref(),
                Some("admin" | "billing" | "owner" | "primary_owner")
            ),
        }
    }

    /// Port of `extraUsage.isEnabled()` for the interactive session arm
    /// (`commands/extra-usage/index.ts:6-17`):
    /// `!isEnvTruthy(DISABLE_EXTRA_USAGE_COMMAND) && isOverageProvisioningAllowed()`.
    /// The env read stays at the caller (keeps this type pure).
    #[must_use]
    pub fn is_extra_usage_command_enabled(&self, disable_env_truthy: bool) -> bool {
        !disable_env_truthy && self.is_overage_provisioning_allowed()
    }

    /// Port of `bnr()` (2.1.207 binary @221947558):
    /// `function bnr(){if(be.DISABLE_EXTRA_USAGE_COMMAND)return!1;
    /// if(S1()!==null)return!0;return NXe()}` — the shared `isEnabled` gate for
    /// BOTH `/usage-credits` command objects (interactive `local-jsx` and the
    /// non-interactive `local` supportsNonInteractive variant) and the
    /// deprecated hidden `/extra-usage` alias. In words:
    /// `!isEnvTruthy(DISABLE_EXTRA_USAGE_COMMAND) && (rateLimitStatus !== null ||
    /// isOverageProvisioningAllowed())`. The 2.1.207 addition over
    /// [`Self::is_extra_usage_command_enabled`] is the middle short-circuit:
    /// when a live rate-limit status is present the command is enabled even
    /// without overage provisioning (so the "you hit a limit → run
    /// /usage-credits" flow always resolves).
    ///
    /// `S1()` = the current rate-limit status (`getRateLimitStatus()`, non-null
    /// once the account has a resolved usage window); `NXe()` = `Self`'s
    /// [`Self::is_overage_provisioning_allowed`]. Both the env read and the
    /// rate-limit-status presence stay at the caller (keeps this type pure).
    ///
    /// The interactive object is additionally `&&!isNonInteractiveSession()`
    /// and the non-interactive object `&&isNonInteractiveSession()` (with
    /// `isHidden=!isNonInteractiveSession()`); that interactive split is applied
    /// by the caller, not modeled here.
    #[must_use]
    pub fn is_usage_credits_command_enabled(
        &self,
        disable_env_truthy: bool,
        has_rate_limit_status: bool,
    ) -> bool {
        if disable_env_truthy {
            return false;
        }
        has_rate_limit_status || self.is_overage_provisioning_allowed()
    }

    /// Port of `A5()` (`Uc()?.billingType==="usage_based"`) — selects "usage limit"
    /// vs "usage credit limit" wording in the 2.1.206 rate-limit messages.
    #[must_use]
    pub fn is_usage_based_billing(&self) -> bool {
        self.billing_type.as_deref() == Some("usage_based")
    }

    /// Port of `DBe()` (2.1.206 binary @214254095):
    /// `Fs()==="enterprise" && Mhc()==="enterprise_usage_based"`, where
    /// `Fs()` = `getSubscriptionType()` and `Mhc()` = `Uc()?.seatTier`
    /// (binary @214254332). One leaf of [`Self::is_saffron_credits_only`]
    /// (`B5()`).
    ///
    // 206 DBe: the `seatTier==="enterprise_usage_based"` conjunct defaults to
    // `false` because the port has no `oauthAccount.seatTier` source — it is a
    // claude.ai-only field never surfaced to a third-party port and not carried
    // on `SubscriptionSnapshot`. `DBe()` is therefore `false` for every
    // populated snapshot; the `subscription_type` conjunct is retained for
    // faithfulness but cannot flip the result true on its own.
    #[must_use]
    pub fn is_enterprise_usage_based_seat(&self) -> bool {
        // `seat_tier` has no port data source (see note) ⇒ the conjunction is
        // always false regardless of `subscription_type`.
        const SEAT_TIER_IS_ENTERPRISE_USAGE_BASED: bool = false;
        self.subscription_type.as_deref() == Some("enterprise")
            && SEAT_TIER_IS_ENTERPRISE_USAGE_BASED
    }

    /// Port of `B5()` (2.1.206 binary @213282281):
    /// `Rn()!=="firstParty" || !Bo() || DBe() || x5()==="default_claude_zero"`
    /// — the saffron "credits-only tier" gate.
    ///
    /// Leaf mapping:
    /// - `Rn()!=="firstParty"` → injected `deployment_first_party` (see the
    ///   `DEPLOYMENT_FIRST_PARTY` note in `tui::rate_limit_messages`; the port
    ///   has no first-party deployment discriminator, so the composer passes
    ///   the documented default `false` ⇒ this disjunct is `true`);
    /// - `!Bo()` → `!self.is_subscriber` (`Bo()` = `bS()&&GW(scopes)`, the
    ///   claude.ai-subscriber check, binary @214252997);
    /// - `DBe()` → [`Self::is_enterprise_usage_based_seat`];
    /// - `x5()==="default_claude_zero"` → `rate_limit_tier` (`x5()` =
    ///   `getRateLimitTier()`, binary @214254206).
    ///
    /// Within THIS task's rate-limit-message scope, `B5()` feeds `Ucg`'s first
    /// guard `!(ZA(t)&&WBe()&&!B5())`; the port's `WBe()` is a documented
    /// `false` (see `tui::rate_limit_messages::overage_consent_required`), so
    /// for that guard the `!B5()` suppression term cannot take effect. Pinned
    /// faithfully so the branch stays correct if `WBe()` ever gains a data
    /// source. NOTE: `B5()` has many other 2.1.206 consumers
    /// (`Gcg`/`Qfi`/`Y6l`/`x7m`/`Eid`/`WCt`, ~10 sites) that are OUT OF SCOPE
    /// here and where the documented defaults are NOT no-ops — each must be
    /// analyzed independently before relying on this predicate elsewhere.
    #[must_use]
    pub fn is_saffron_credits_only(&self, deployment_first_party: bool) -> bool {
        !deployment_first_party
            || !self.is_subscriber
            || self.is_enterprise_usage_based_seat()
            || self.rate_limit_tier.as_deref() == Some("default_claude_zero")
    }
}

/// Shared slot the composition root fills asynchronously (a background
/// profile/roles fetch) and UI layers read at compose time. `None` until the
/// fetch lands; readers treat `None` as `SubscriptionSnapshot::default()`.
pub type SharedSubscription = Arc<RwLock<Option<SubscriptionSnapshot>>>;

/// Process-global current-subscription cache — the port's analog of claude-code's
/// module-level `getSubscriptionType()` / `getOauthAccountInfo()` (`vi()` reads a
/// cached global, not a threaded value). `None` until a composition root resolves
/// the OAuth profile and calls [`set_current_subscription`]; readers that need a
/// process-wide tier (e.g. the `AgentTool` pro-plan prompt gate) consult it
/// without taking a dependency on any per-instance [`SharedSubscription`] slot.
static CURRENT_SUBSCRIPTION: RwLock<Option<SubscriptionSnapshot>> = RwLock::new(None);

/// Set (or clear) the process-global subscription snapshot. Called by the
/// composition root after it resolves the signed-in user's plan; clears to
/// `None` on logout. Mirrors claude-code caching the resolved subscription in
/// module state for later `vi()` reads.
pub fn set_current_subscription(snapshot: Option<SubscriptionSnapshot>) {
    if let Ok(mut slot) = CURRENT_SUBSCRIPTION.write() {
        *slot = snapshot;
    }
}

/// Process-global `getSubscriptionType()` (`vi()`): the current subscription type
/// string (`"pro" | "max" | "team" | "enterprise"`), or `None` when unknown / not
/// yet resolved. A poisoned lock degrades to `None` (conservative, matching the
/// "unknown subscription ⇒ every predicate false" contract).
#[must_use]
pub fn current_subscription_type() -> Option<String> {
    CURRENT_SUBSCRIPTION
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().and_then(|s| s.subscription_type.clone()))
}

/// Whether the signed-in user is on the `"pro"` plan specifically (binary
/// `vi()==="pro"`). `false` when the tier is unknown or any non-`pro` value.
#[must_use]
pub fn is_pro_plan() -> bool {
    current_subscription_type().as_deref() == Some("pro")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team_snapshot() -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            is_subscriber: true,
            subscription_type: Some("team".to_string()),
            billing_type: Some("stripe_subscription".to_string()),
            ..SubscriptionSnapshot::default()
        }
    }

    #[test]
    fn default_snapshot_resolves_all_predicates_false() {
        let snap = SubscriptionSnapshot::default();
        assert!(!snap.is_team_or_enterprise());
        assert!(!snap.is_pro_or_enterprise());
        assert!(!snap.is_max20x());
        assert!(!snap.is_overage_provisioning_allowed());
        assert!(!snap.has_claude_ai_billing_access());
        assert!(!snap.is_extra_usage_command_enabled(false));
    }

    #[test]
    fn team_and_enterprise_predicates() {
        let team = team_snapshot();
        assert!(team.is_team_or_enterprise());
        assert!(!team.is_pro_or_enterprise());

        let enterprise = SubscriptionSnapshot {
            subscription_type: Some("enterprise".to_string()),
            ..team_snapshot()
        };
        assert!(enterprise.is_team_or_enterprise());
        assert!(enterprise.is_pro_or_enterprise());

        let pro = SubscriptionSnapshot {
            subscription_type: Some("pro".to_string()),
            ..team_snapshot()
        };
        assert!(!pro.is_team_or_enterprise());
        assert!(pro.is_pro_or_enterprise());
    }

    #[test]
    fn max20x_is_exact_tier_match() {
        let max20 = SubscriptionSnapshot {
            subscription_type: Some("max".to_string()),
            rate_limit_tier: Some("default_claude_max_20x".to_string()),
            ..SubscriptionSnapshot::default()
        };
        assert!(max20.is_max20x());

        let max5 = SubscriptionSnapshot {
            subscription_type: Some("max".to_string()),
            rate_limit_tier: Some("default_claude_max_5x".to_string()),
            ..SubscriptionSnapshot::default()
        };
        assert!(!max5.is_max20x());
    }

    #[test]
    fn max20x_requires_max_subscription_type_206_tdo() {
        // 206 `Tdo`'s `isMax20x` is a two-conjunct check
        // (`subscriptionType==="max" && rateLimitTier==="default_claude_max_20x"`).
        // A snapshot with the tier but a non-"max" subscription type (or no
        // subscription type at all) must NOT satisfy it.
        let tier_only = SubscriptionSnapshot {
            rate_limit_tier: Some("default_claude_max_20x".to_string()),
            ..SubscriptionSnapshot::default()
        };
        assert!(!tier_only.is_max20x());

        let pro_with_tier = SubscriptionSnapshot {
            subscription_type: Some("pro".to_string()),
            rate_limit_tier: Some("default_claude_max_20x".to_string()),
            ..SubscriptionSnapshot::default()
        };
        assert!(!pro_with_tier.is_max20x());
    }

    #[test]
    fn overage_provisioning_requires_subscriber_and_billing_type() {
        for billing in [
            "stripe_subscription",
            "stripe_subscription_contracted",
            "apple_subscription",
            "google_play_subscription",
        ] {
            let snap = SubscriptionSnapshot {
                billing_type: Some(billing.to_string()),
                ..team_snapshot()
            };
            assert!(
                snap.is_overage_provisioning_allowed(),
                "billing type {billing} should allow overage provisioning"
            );
        }

        let marketplace = SubscriptionSnapshot {
            billing_type: Some("aws_marketplace".to_string()),
            ..team_snapshot()
        };
        assert!(!marketplace.is_overage_provisioning_allowed());

        let no_billing = SubscriptionSnapshot {
            billing_type: None,
            ..team_snapshot()
        };
        assert!(!no_billing.is_overage_provisioning_allowed());

        let non_subscriber = SubscriptionSnapshot {
            is_subscriber: false,
            ..team_snapshot()
        };
        assert!(!non_subscriber.is_overage_provisioning_allowed());
    }

    #[test]
    fn billing_access_pro_max_always_team_by_role() {
        for role in ["admin", "billing", "owner", "primary_owner"] {
            let snap = SubscriptionSnapshot {
                organization_role: Some(role.to_string()),
                ..team_snapshot()
            };
            assert!(
                snap.has_claude_ai_billing_access(),
                "team org role {role} should grant billing access"
            );
        }

        let member = SubscriptionSnapshot {
            organization_role: Some("member".to_string()),
            ..team_snapshot()
        };
        assert!(!member.has_claude_ai_billing_access());

        for consumer in ["max", "pro"] {
            let snap = SubscriptionSnapshot {
                subscription_type: Some(consumer.to_string()),
                organization_role: None,
                ..team_snapshot()
            };
            assert!(
                snap.has_claude_ai_billing_access(),
                "{consumer} should always have billing access"
            );
        }

        let non_subscriber = SubscriptionSnapshot {
            is_subscriber: false,
            subscription_type: Some("max".to_string()),
            ..team_snapshot()
        };
        assert!(!non_subscriber.has_claude_ai_billing_access());
    }

    #[test]
    fn extra_usage_command_gate() {
        let team = team_snapshot();
        assert!(!team.is_extra_usage_command_enabled(true));

        let no_billing = SubscriptionSnapshot {
            billing_type: None,
            ..team_snapshot()
        };
        assert!(!no_billing.is_extra_usage_command_enabled(false));

        assert!(team.is_extra_usage_command_enabled(false));
    }

    #[test]
    fn usage_credits_command_gate_bnr() {
        // H-BIN-11 (cc2.1.207) `bnr()` gate matrix.
        let team = team_snapshot(); // overage-provisioning allowed

        // DISABLE_EXTRA_USAGE_COMMAND truthy → always disabled, even with a
        // live rate-limit status and overage provisioning.
        assert!(!team.is_usage_credits_command_enabled(true, true));
        assert!(!team.is_usage_credits_command_enabled(true, false));

        // Not disabled: a live rate-limit status short-circuits to enabled
        // regardless of overage provisioning (the 2.1.207 addition).
        let no_billing = SubscriptionSnapshot {
            billing_type: None,
            ..team_snapshot()
        };
        assert!(!no_billing.is_overage_provisioning_allowed());
        assert!(no_billing.is_usage_credits_command_enabled(false, true));

        // No rate-limit status: falls back to overage provisioning.
        assert!(team.is_usage_credits_command_enabled(false, false));
        assert!(!no_billing.is_usage_credits_command_enabled(false, false));

        // Default (unknown) snapshot with neither signal → disabled.
        let unknown = SubscriptionSnapshot::default();
        assert!(!unknown.is_usage_credits_command_enabled(false, false));
        assert!(unknown.is_usage_credits_command_enabled(false, true));
    }

    #[test]
    fn is_usage_based_billing_matches_a5() {
        let mut s = SubscriptionSnapshot::default();
        assert!(!s.is_usage_based_billing());
        s.billing_type = Some("usage_based".into());
        assert!(s.is_usage_based_billing());
        s.billing_type = Some("stripe_subscription".into());
        assert!(!s.is_usage_based_billing());
    }

    #[test]
    fn dbe_enterprise_usage_based_seat_has_no_port_source() {
        // 206 DBe: `seatTier` has no port source ⇒ always false, even for an
        // enterprise snapshot (the only `subscription_type` that could satisfy
        // the first conjunct).
        let enterprise = SubscriptionSnapshot {
            is_subscriber: true,
            subscription_type: Some("enterprise".into()),
            ..SubscriptionSnapshot::default()
        };
        assert!(!enterprise.is_enterprise_usage_based_seat());

        let team = SubscriptionSnapshot {
            subscription_type: Some("team".into()),
            ..enterprise.clone()
        };
        assert!(!team.is_enterprise_usage_based_seat());

        assert!(!SubscriptionSnapshot::default().is_enterprise_usage_based_seat());
    }

    #[test]
    fn b5_saffron_credits_only_truth_table() {
        // Fully "not credits-only": first-party deployment, subscriber, no
        // enterprise-usage-based seat, non-zero tier ⇒ every disjunct false.
        let clean = SubscriptionSnapshot {
            is_subscriber: true,
            subscription_type: Some("pro".into()),
            rate_limit_tier: Some("default_claude_max_20x".into()),
            ..SubscriptionSnapshot::default()
        };
        assert!(!clean.is_saffron_credits_only(true));

        // `Rn()!=="firstParty"` disjunct: non-first-party deployment ⇒ true.
        assert!(clean.is_saffron_credits_only(false));

        // `!Bo()` disjunct: not a subscriber ⇒ true (even first-party).
        let anon = SubscriptionSnapshot {
            is_subscriber: false,
            ..clean.clone()
        };
        assert!(anon.is_saffron_credits_only(true));

        // `x5()==="default_claude_zero"` disjunct ⇒ true (first-party sub).
        let zero = SubscriptionSnapshot {
            rate_limit_tier: Some("default_claude_zero".into()),
            ..clean.clone()
        };
        assert!(zero.is_saffron_credits_only(true));

        // A different tier does NOT trip the zero-tier disjunct.
        let non_zero = SubscriptionSnapshot {
            rate_limit_tier: Some("default_claude_max_5x".into()),
            ..clean.clone()
        };
        assert!(!non_zero.is_saffron_credits_only(true));
    }
}
