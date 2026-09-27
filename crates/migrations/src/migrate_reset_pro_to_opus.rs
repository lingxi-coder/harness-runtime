//! `resetProToOpusDefault.ts` — one-shot flag/timestamp bookkeeping for the
//! Pro→Opus default switch. Run-once via `opusProMigrationComplete`.
//!
//! Tier is structurally `None` in this port (no keychain subscriptionType) ⇒
//! the not-Pro branch runs: mark complete + `skipped: true` event — which is
//! TS's own behavior for non-Pro/non-firstParty users, not a stub.

use crate::context::{js_truthy, MigrationEnv};
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, WritableScope};
use llm_runtime::oauth::anthropic::limits::SubscriptionType;
use serde_json::{json, Value};
use telemetry::sink::AnalyticsValue;

/// Run the migration.
pub async fn run(env: &MigrationEnv) {
    let cfg = match global_config::read_map(&env.global_config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "migrate_reset_pro_to_opus: config read failed");
            return;
        }
    };
    if cfg.get("opusProMigrationComplete").is_some_and(js_truthy) {
        return;
    }

    let is_pro = env.ctx.subscription_type == Some(SubscriptionType::Pro);
    if !env.ctx.first_party || !is_pro {
        if let Err(e) = global_config::save_map(&env.global_config_path, |mut m| {
            m.insert("opusProMigrationComplete".into(), Value::Bool(true));
            m
        }) {
            // TS `saveGlobalConfig` throws here, before logEvent (no catch in
            // this migration) — mirror that by skipping the emit.
            tracing::warn!(error = %e, "migrate_reset_pro_to_opus: skip-branch flag write failed");
            return;
        }
        env.emit(
            telemetry::tengu::migration::RESET_PRO_TO_OPUS_DEFAULT,
            std::collections::HashMap::from([("skipped".to_string(), AnalyticsValue::Bool(true))]),
        )
        .await;
        return;
    }

    // DORMANT until tier persistence lands. TS reads getSettings_DEPRECATED
    // (merged settings); the user-settings model is the in-port stand-in
    // (doc'd: the merged read has no substrate at this pre-boot point).
    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let has_custom_model = read_settings_map(&sp)
        .ok()
        .is_some_and(|m| m.get("model").is_some());
    if let Err(e) = global_config::save_map(&env.global_config_path, |mut m| {
        m.insert("opusProMigrationComplete".into(), Value::Bool(true));
        if !has_custom_model {
            m.insert(
                "opusProMigrationTimestamp".into(),
                json!(MigrationEnv::now_ms()),
            );
        }
        m
    }) {
        // TS throws here, before logEvent — mirror that by skipping the emit.
        tracing::warn!(error = %e, "migrate_reset_pro_to_opus: eligible-branch flag write failed");
        return;
    }
    env.emit(
        telemetry::tengu::migration::RESET_PRO_TO_OPUS_DEFAULT,
        std::collections::HashMap::from([
            ("skipped".to_string(), AnalyticsValue::Bool(false)),
            (
                "had_custom_model".to_string(),
                AnalyticsValue::Bool(has_custom_model),
            ),
        ]),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_config;
    use llm_runtime::oauth::anthropic::limits::SubscriptionType;
    use serde_json::json;

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
    async fn tier_none_marks_complete_and_skips() {
        let t = temp_config();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["opusProMigrationComplete"], json!(true));
        assert!(m.get("opusProMigrationTimestamp").is_none());
    }

    /// Emission contract: the tier-`None` (not-Pro) branch still logs
    /// `tengu_reset_pro_to_opus_default` with `skipped: true`
    /// (`resetProToOpusDefault.ts` non-eligible path).
    #[tokio::test]
    async fn tier_none_emits_skipped_true() {
        let t = temp_config();
        let (bus, events) = crate::test_support::capture_bus().await;
        let mut env = test_env(&t);
        env.bus = Some(bus);
        run(&env).await;
        let ev = events.lock().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0].0,
            telemetry::tengu::migration::RESET_PRO_TO_OPUS_DEFAULT
        );
        assert_eq!(ev[0].1, json!({"skipped": true}));
    }

    #[tokio::test]
    async fn complete_flag_short_circuits() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"opusProMigrationComplete": true}"#).unwrap();
        let before = std::fs::read_to_string(&t.global).unwrap();
        run(&test_env(&t)).await;
        assert_eq!(std::fs::read_to_string(&t.global).unwrap(), before);
    }

    #[tokio::test]
    async fn pro_first_party_default_model_stamps_timestamp() {
        let t = temp_config();
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Pro);
        run(&env).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["opusProMigrationComplete"], json!(true));
        assert!(m["opusProMigrationTimestamp"].is_i64());
    }

    #[tokio::test]
    async fn pro_with_custom_model_marks_complete_without_timestamp() {
        let t = temp_config();
        let sp = crate::settings_update::settings_path(
            crate::settings_update::WritableScope::User,
            &t.home,
            &t.project,
        );
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"model": "sonnet"}"#).unwrap();
        let mut env = test_env(&t);
        env.ctx.subscription_type = Some(SubscriptionType::Pro);
        run(&env).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["opusProMigrationComplete"], json!(true));
        assert!(m.get("opusProMigrationTimestamp").is_none());
    }
}
