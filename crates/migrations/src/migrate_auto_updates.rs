//! `migrateAutoUpdatesToSettings.ts` — move a user-set `autoUpdates: false`
//! preference into `settings.json env.DISABLE_AUTOUPDATER = "1"`, set the
//! process env var so it takes effect immediately, and drop the old config
//! keys. The Rust port has no auto-updater consumer; the file/env effects
//! are the faithful contract.

use crate::context::{js_truthy, MigrationEnv};
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use serde_json::{json, Map, Value};
use telemetry::sink::AnalyticsValue;

/// Run the migration.
pub async fn run(env: &MigrationEnv) -> bool {
    let cfg = match global_config::read_map(&env.global_config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "migrate_auto_updates: config read failed");
            return true;
        }
    };
    // Only when autoUpdates was EXPLICITLY false and not native-protected
    // (`migrateAutoUpdatesToSettings.ts:19-24`).
    if cfg.get("autoUpdates") != Some(&Value::Bool(false))
        || cfg.get("autoUpdatesProtectedForNative") == Some(&Value::Bool(true))
    {
        return true;
    }

    // TS try block (TS:25-54): of everything inside it, only
    // `saveGlobalConfig` (the config-key cleanup) can actually throw —
    // `getSettingsForSource` returns null on a broken file (settings.ts:201-231)
    // and `updateSettingsForSource` returns an ignored `{error}`
    // (settings.ts:416-523). So only the cleanup failure reaches
    // `tengu_migrate_autoupdates_error`.
    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    // TS: `getSettingsForSource('userSettings') || {}` (TS:26) — a broken
    // settings file parses to null, never throws; proceed with an empty map.
    let user = read_settings_map(&sp).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "migrate_auto_updates: settings read failed (treated as empty, TS parity)");
        Map::new()
    });
    let already_had = user
        .get("env")
        .and_then(|e| e.get("DISABLE_AUTOUPDATER"))
        .is_some_and(js_truthy);
    let mut env_map: Map<String, Value> = user
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    env_map.insert("DISABLE_AUTOUPDATER".into(), json!("1"));
    if let Err(e) = update_settings(&sp, vec![("env".into(), Some(Value::Object(env_map)))]) {
        tracing::warn!(error = %e, "migrate_auto_updates: settings write failed");
        emit_error(env).await;
        return false;
    }

    env.emit(
        telemetry::tengu::migration::MIGRATE_AUTOUPDATES_TO_SETTINGS,
        std::collections::HashMap::from([
            (
                "was_user_preference".to_string(),
                AnalyticsValue::Bool(true),
            ),
            (
                "already_had_env_var".to_string(),
                AnalyticsValue::Bool(already_had),
            ),
        ]),
    )
    .await;
    // explicitly set, so this takes effect immediately (TS:44)
    std::env::set_var("DISABLE_AUTOUPDATER", "1");
    if let Err(e) = global_config::save_map(&env.global_config_path, |mut m| {
        m.remove("autoUpdates");
        m.remove("autoUpdatesProtectedForNative");
        m
    }) {
        // `saveGlobalConfig` (TS:47-54) is the one throw-capable call inside
        // the TS try: a failure here routes to the catch → error event.
        tracing::warn!(error = %e, "migrate_auto_updates: config cleanup failed");
        emit_error(env).await;
        return false;
    }
    true
}

/// `tengu_migrate_autoupdates_error` (TS catch path, `TS:55-60`).
async fn emit_error(env: &MigrationEnv) {
    env.emit(
        telemetry::tengu::migration::MIGRATE_AUTOUPDATES_ERROR,
        std::collections::HashMap::from([("has_error".to_string(), AnalyticsValue::Bool(true))]),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_update::{read_settings_map, settings_path, WritableScope};
    use crate::test_support::{env_lock, temp_config};
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

    // env_lock is a std Mutex held across `.await` on purpose: it serializes
    // whole test bodies against parallel test threads, and each #[tokio::test]
    // runs its own current-thread runtime, so no executor task can deadlock on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn migrates_explicit_false_to_settings_env() {
        let _g = env_lock(); // sets process env DISABLE_AUTOUPDATER
        std::env::remove_var("DISABLE_AUTOUPDATER");
        let t = temp_config();
        std::fs::write(&t.global, r#"{"autoUpdates": false, "keep": 1}"#).unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"env": {"EXISTING": "x"}}"#).unwrap();

        run(&test_env(&t)).await;

        let s = read_settings_map(&sp).unwrap();
        assert_eq!(s["env"]["DISABLE_AUTOUPDATER"], json!("1"));
        assert_eq!(s["env"]["EXISTING"], json!("x")); // spread-merge preserved
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("autoUpdates").is_none());
        assert!(m.get("autoUpdatesProtectedForNative").is_none());
        assert_eq!(m["keep"], json!(1));
        assert_eq!(std::env::var("DISABLE_AUTOUPDATER").unwrap(), "1");
        std::env::remove_var("DISABLE_AUTOUPDATER");
    }

    /// In 2.1.245 a settings write failure now aborts the migration and leaves
    /// the legacy global-config keys intact so the runner can retry later.
    // See migrates_explicit_false_to_settings_env for the lock rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn broken_settings_file_blocks_cleanup_and_env_set() {
        let _g = env_lock(); // sets process env DISABLE_AUTOUPDATER
        std::env::remove_var("DISABLE_AUTOUPDATER");
        let t = temp_config();
        std::fs::write(&t.global, r#"{"autoUpdates": false}"#).unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, "{ broken").unwrap();

        run(&test_env(&t)).await;

        // broken file untouched…
        assert_eq!(std::fs::read_to_string(&sp).unwrap(), "{ broken");
        // …and the migration now aborts before env/config cleanup.
        assert!(std::env::var("DISABLE_AUTOUPDATER").is_err());
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["autoUpdates"], json!(false));
    }

    // See migrates_explicit_false_to_settings_env for the lock rationale.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn skips_true_missing_or_protected() {
        let _g = env_lock();
        // autoUpdates true → skip
        let t = temp_config();
        std::fs::write(&t.global, r#"{"autoUpdates": true}"#).unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["autoUpdates"], json!(true));

        // protected → skip
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"autoUpdates": false, "autoUpdatesProtectedForNative": true}"#,
        )
        .unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["autoUpdates"], json!(false)); // untouched
    }
}
