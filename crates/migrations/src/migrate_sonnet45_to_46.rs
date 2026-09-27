//! `migrateSonnet45ToSonnet46.ts` — move Pro/Max/Team-Premium first-party
//! users off explicit Sonnet 4.5 strings to the `sonnet` alias. Tier `None`
//! ⇒ early-return (TS non-subscriber path). Rust `SubscriptionType::Team`
//! cannot distinguish Premium from Standard (TS gates on Team PREMIUM via
//! `rateLimitTier === 'default_claude_max_5x'`, auth.ts:1687-1692, which has
//! no Rust substrate) — `Team` is accepted, documented divergence in the
//! dormant path.

use crate::context::MigrationEnv;
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use llm_runtime::oauth::anthropic::limits::SubscriptionType;
use serde_json::{json, Value};
use telemetry::sink::AnalyticsValue;

/// The explicit Sonnet 4.5 strings (`migrateSonnet45ToSonnet46.ts:40-45`).
const SONNET45_MODELS: [&str; 4] = [
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-5-20250929[1m]",
    "sonnet-4-5-20250929",
    "sonnet-4-5-20250929[1m]",
];

/// Run the migration.
pub async fn run(env: &MigrationEnv) -> bool {
    if !env.ctx.first_party {
        return true;
    }
    // isProSubscriber || isMaxSubscriber || isTeamPremiumSubscriber; `None`
    // (unknown tier) ⇒ all three false in TS ⇒ early return (fail closed).
    let eligible = matches!(
        env.ctx.subscription_type,
        Some(SubscriptionType::Pro | SubscriptionType::Max | SubscriptionType::Team)
    );
    if !eligible {
        return true;
    }

    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let Some(model) = read_settings_map(&sp)
        .ok()
        .and_then(|m| m.get("model").and_then(Value::as_str).map(String::from))
    else {
        return true;
    };
    if !SONNET45_MODELS.contains(&model.as_str()) {
        return true;
    }

    let has_1m = model.ends_with("[1m]");
    let target = if has_1m { "sonnet[1m]" } else { "sonnet" };
    if let Err(e) = update_settings(&sp, vec![("model".into(), Some(json!(target)))]) {
        tracing::warn!(error = %e, "migrate_sonnet45_to_46: settings write failed");
        return false;
    }

    // Skip notification for brand-new users (numStartups <= 1).
    let num_startups = global_config::read_map(&env.global_config_path)
        .ok()
        .and_then(|m| m.get("numStartups").and_then(Value::as_u64))
        .unwrap_or(0);
    if num_startups > 1 {
        if let Err(e) = global_config::save_map(&env.global_config_path, |mut m| {
            m.insert(
                "sonnet45To46MigrationTimestamp".into(),
                json!(MigrationEnv::now_ms()),
            );
            m
        }) {
            // TS `saveGlobalConfig` throws here, before logEvent (no catch in
            // this migration) — mirror that by skipping the emit.
            tracing::warn!(error = %e, "migrate_sonnet45_to_46: timestamp write failed");
            return false;
        }
    }
    env.emit(
        telemetry::tengu::migration::SONNET45_TO_46_MIGRATION,
        std::collections::HashMap::from([
            ("from_model".to_string(), AnalyticsValue::String(model)),
            ("has_1m".to_string(), AnalyticsValue::Bool(has_1m)),
        ]),
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_update::{
        force_rename_failure_for_test, read_settings_map, settings_path, WritableScope,
    };
    use crate::test_support::temp_config;
    use llm_runtime::oauth::anthropic::limits::SubscriptionType;

    fn test_env(t: &crate::test_support::TempConfig) -> crate::context::MigrationEnv {
        crate::context::MigrationEnv {
            global_config_path: t.global.clone(),
            lingxi_config_home: t.home.clone(),
            project_dir: t.project.clone(),
            ctx: crate::context::MigrationContext {
                first_party: true,
                subscription_type: None,
            },
            bus: None,
        }
    }

    #[tokio::test]
    async fn tier_none_is_noop() {
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "claude-sonnet-4-5-20250929"}"#).unwrap();
        run(&test_env(&t)).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("claude-sonnet-4-5-20250929")
        );
    }

    #[tokio::test]
    async fn max_tier_rewrites_preserving_1m_and_gates_timestamp_on_startups() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"numStartups": 5}"#).unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "sonnet-4-5-20250929[1m]"}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("sonnet[1m]")
        );
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m["sonnet45To46MigrationTimestamp"].is_i64());

        // fresh user (numStartups missing → 0) gets no timestamp
        let t2 = temp_config();
        let sp2 = settings_path(WritableScope::User, &t2.home, &t2.project);
        std::fs::create_dir_all(sp2.parent().unwrap()).unwrap();
        std::fs::write(&sp2, r#"{"model": "claude-sonnet-4-5-20250929"}"#).unwrap();
        let mut env2 = test_env(&t2);
        env2.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env2).await;
        assert_eq!(
            read_settings_map(&sp2).unwrap()["model"],
            serde_json::json!("sonnet")
        );
        let m2 = crate::global_config::read_map(&t2.global).unwrap();
        assert!(m2.get("sonnet45To46MigrationTimestamp").is_none());
    }

    /// Emission contract: the Max-tier happy path logs
    /// `tengu_sonnet45_to_46_migration` with `from_model` = the ORIGINAL
    /// pinned string and `has_1m` (`migrateSonnet45ToSonnet46.ts:63`).
    #[tokio::test]
    async fn max_tier_happy_path_emits_from_model_and_has_1m() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"numStartups": 5}"#).unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "sonnet-4-5-20250929[1m]"}"#).unwrap();
        let (bus, events) = crate::test_support::capture_bus().await;
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        env.bus = Some(bus);
        run(&env).await;
        let ev = events.lock().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0].0,
            telemetry::tengu::migration::SONNET45_TO_46_MIGRATION
        );
        assert_eq!(
            ev[0].1,
            serde_json::json!({"from_model": "sonnet-4-5-20250929[1m]", "has_1m": true})
        );
    }

    /// Gate coverage: Pro is accepted, and Team is accepted — the module-doc
    /// divergence pin (Rust `Team` cannot distinguish Premium from Standard;
    /// TS gates on Team PREMIUM via `rateLimitTier`, auth.ts:1687-1692).
    #[tokio::test]
    async fn pro_and_team_tiers_are_accepted() {
        for tier in [SubscriptionType::Pro, SubscriptionType::Team] {
            let t = temp_config();
            let sp = settings_path(WritableScope::User, &t.home, &t.project);
            std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
            std::fs::write(&sp, r#"{"model": "claude-sonnet-4-5-20250929"}"#).unwrap();
            let mut env = test_env(&t);
            env.ctx.subscription_type = Some(tier);
            run(&env).await;
            assert_eq!(
                read_settings_map(&sp).unwrap()["model"],
                serde_json::json!("sonnet")
            );
        }
    }

    /// Gate coverage: not first-party is a noop even for an eligible tier.
    #[tokio::test]
    async fn not_first_party_is_noop() {
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "claude-sonnet-4-5-20250929"}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.first_party = false;
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("claude-sonnet-4-5-20250929")
        );
    }

    /// In 2.1.245 a failing settings write aborts before the timestamp/event.
    #[cfg(unix)]
    #[tokio::test]
    async fn settings_write_failure_skips_timestamp() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"numStartups": 5}"#).unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "claude-sonnet-4-5-20250929"}"#).unwrap();
        let _failure = force_rename_failure_for_test(&sp);
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        // write failed → model unchanged…
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("claude-sonnet-4-5-20250929")
        );
        // …and the timestamp is no longer stamped.
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("sonnet45To46MigrationTimestamp").is_none());
    }
}
