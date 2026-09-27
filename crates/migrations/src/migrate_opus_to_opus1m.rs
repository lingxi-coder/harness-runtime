//! `migrateOpusToOpus1m.ts` — rewrite a pinned `opus` to `opus[1m]` for
//! merge-eligible users. `isOpus1mMergeEnabled` (`model.ts:314-332`) fails
//! closed on unknown tier — structurally always false in this port today
//! (tier `None`) ⇒ dormant; the body is ported for when tier persistence
//! lands.
//!
//! DOCUMENTED DIVERGENCE (live, conservative): TS fails closed only for
//! claude.ai SUBSCRIBERS with an unknown tier (`isClaudeAISubscriber() &&
//! getSubscriptionType() === null`, model.ts:328-330). A first-party
//! API-key/external-token user has `isClaudeAISubscriber() === false`, skips
//! that guard, and TS WOULD migrate their pinned `opus` → `opus[1m]`. This
//! port's `subscription_type: None` cannot distinguish "API-key user" from
//! "subscriber, tier unknown", so `None → false` skips both — a no-op in the
//! conservative direction, matching the TS stated intent ("Max/Team Premium
//! on 1P"). Revisit when tier/auth-kind persistence lands.
//!
//! Stand-ins for unported helpers (doc'd, dormant path only):
//! - `getDefaultMainLoopModelSetting` (`model.ts:178-200`): Max/Team(≈Team
//!   Premium — `rateLimitTier` has no Rust substrate) → `opus[1m]` under
//!   merge (getDefaultOpusModel alias level), else the sonnet default —
//!   represented as the literal setting strings `"opus[1m]"` / `"sonnet"`.
//! - `parseUserSpecifiedModel` comparison: direct setting-string equality
//!   (alias-level), sufficient for the `opus[1m]` vs default comparison.

use crate::context::{env_var_truthy, MigrationEnv};
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use llm_runtime::oauth::anthropic::limits::SubscriptionType;
use serde_json::{json, Value};

/// `isOpus1mMergeEnabled` port (`model.ts:314-332`): false when 1M disabled
/// by env (`is1mContextDisabled`, context.ts:31-33), on Pro, off first-party,
/// or tier unknown (fail closed, model.ts:322-330).
fn is_opus1m_merge_enabled(env: &MigrationEnv) -> bool {
    if env_var_truthy("CLAUDE_CODE_DISABLE_1M_CONTEXT") {
        return false;
    }
    match env.ctx.subscription_type {
        // `None` fails closed — see the module-doc DOCUMENTED DIVERGENCE
        // (TS's fail-closed guard at model.ts:328-330 applies only to
        // claude.ai subscribers; API-key users would pass in TS). Pro keeps
        // separate Opus / Opus 1M options (model.ts:317).
        None | Some(SubscriptionType::Pro) => false,
        Some(_) => env.ctx.first_party,
    }
}

/// `getDefaultMainLoopModelSetting` stand-in (dormant path; see module doc).
/// Only called after `is_opus1m_merge_enabled` passed, so the merge-enabled
/// `[1m]` suffix is unconditional here (model.ts:188-196).
fn default_main_loop_model_setting(env: &MigrationEnv) -> &'static str {
    match env.ctx.subscription_type {
        Some(SubscriptionType::Max | SubscriptionType::Team) => "opus[1m]",
        _ => "sonnet",
    }
}

/// Run the migration.
pub async fn run(env: &MigrationEnv) -> bool {
    if !is_opus1m_merge_enabled(env) {
        return true;
    }
    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let model = read_settings_map(&sp)
        .ok()
        .and_then(|m| m.get("model").and_then(Value::as_str).map(String::from));
    if model.as_deref() != Some("opus") {
        return true;
    }

    // modelToSet: undefined (delete) when opus[1m] IS the default, else set.
    let migrated = "opus[1m]";
    let update = if migrated == default_main_loop_model_setting(env) {
        ("model".to_string(), None)
    } else {
        ("model".to_string(), Some(json!(migrated)))
    };
    if let Err(e) = update_settings(&sp, vec![update]) {
        tracing::warn!(error = %e, "migrate_opus_to_opus1m: settings write failed");
        return false;
    }
    env.emit(
        telemetry::tengu::migration::OPUS_TO_OPUS1M_MIGRATION,
        std::collections::HashMap::new(),
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_update::{read_settings_map, settings_path, WritableScope};
    use crate::test_support::{env_lock, temp_config};
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

    // env_lock is a std Mutex held across `.await` on purpose: it serializes
    // whole test bodies against parallel test threads, and each #[tokio::test]
    // runs its own current-thread runtime, so no executor task can deadlock on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn tier_none_fails_closed() {
        let _g = env_lock();
        std::env::remove_var("CLAUDE_CODE_DISABLE_1M_CONTEXT");
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "opus"}"#).unwrap();
        run(&test_env(&t)).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("opus")
        );
    }

    // See tier_none_fails_closed for the lock rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn max_tier_deletes_pinned_opus_when_default_matches() {
        let _g = env_lock();
        std::env::remove_var("CLAUDE_CODE_DISABLE_1M_CONTEXT");
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "opus", "keep": 1}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        let s = read_settings_map(&sp).unwrap();
        assert!(s.get("model").is_none()); // modelToSet === undefined → delete
        assert_eq!(s["keep"], serde_json::json!(1));
    }

    // See tier_none_fails_closed for the lock rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn enterprise_tier_writes_opus1m() {
        let _g = env_lock();
        std::env::remove_var("CLAUDE_CODE_DISABLE_1M_CONTEXT");
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "opus"}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Enterprise);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("opus[1m]")
        );
    }

    /// Gate coverage: not first-party is a noop even with an eligible tier
    /// (`is_opus1m_merge_enabled` requires `first_party`, model.ts:331).
    // See tier_none_fails_closed for the lock rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn not_first_party_max_tier_noop() {
        let _g = env_lock();
        std::env::remove_var("CLAUDE_CODE_DISABLE_1M_CONTEXT");
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "opus"}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.first_party = false;
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("opus")
        );
    }

    // See tier_none_fails_closed for the lock rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn env_disable_or_pro_or_other_model_noop() {
        let _g = env_lock();
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "opus"}"#).unwrap();

        // 1M context disabled by env
        std::env::set_var("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1");
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("opus")
        );
        std::env::remove_var("CLAUDE_CODE_DISABLE_1M_CONTEXT");

        // Pro subscribers keep separate Opus / Opus 1M options
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Pro);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("opus")
        );

        // non-'opus' pin untouched
        std::fs::write(&sp, r#"{"model": "opus[1m]"}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Max);
        run(&env).await;
        assert_eq!(
            read_settings_map(&sp).unwrap()["model"],
            serde_json::json!("opus[1m]")
        );
    }
}
