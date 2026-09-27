//! `migrateUserIntentToSettings.ts` — move selected top-level user-preference
//! keys from the raw global config into `userSettings`, skipping only exact
//! default primitive values and keys already present in settings.

use crate::context::MigrationEnv;
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use serde_json::{json, Value};
use telemetry::sink::AnalyticsValue;

const EVENT: &str = "tengu_migrate_user_intent_to_settings";
const KEYS: [(&str, Option<fn() -> Value>); 15] = [
    ("theme", Some(|| json!("dark"))),
    ("verbose", Some(|| json!(false))),
    ("preferredNotifChannel", Some(|| json!("auto"))),
    ("editorMode", Some(|| json!("normal"))),
    ("autoCompactEnabled", Some(|| json!(true))),
    ("autoScrollEnabled", Some(|| json!(true))),
    ("showTurnDuration", Some(|| json!(true))),
    ("showMessageTimestamps", Some(|| json!(false))),
    ("todoFeatureEnabled", Some(|| json!(true))),
    ("fileCheckpointingEnabled", Some(|| json!(true))),
    ("terminalProgressBarEnabled", Some(|| json!(true))),
    ("inputNeededNotifEnabled", None),
    ("agentPushNotifEnabled", None),
    ("remoteControlAtStartup", None),
    ("autoUploadSessions", None),
];

/// Run the migration, returning `false` only when the settings write required
/// for a non-empty update failed.
pub async fn run(env: &MigrationEnv) -> bool {
    let Ok(cfg) = global_config::read_map(&env.global_config_path) else {
        return true;
    };

    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let user = read_settings_map(&sp).unwrap_or_default();
    let mut updates = Vec::new();
    for (key, default_fn) in KEYS {
        let Some(value) = cfg.get(key) else { continue };
        if user.get(key).is_some() {
            continue;
        }
        if let Some(default_fn) = default_fn {
            if value == &default_fn() {
                continue;
            }
        }
        updates.push((key.to_string(), Some(value.clone())));
    }
    if updates.is_empty() {
        return true;
    }
    let migrated_count = updates.len();
    if let Err(e) = update_settings(&sp, updates) {
        tracing::warn!(error = %e, "migrate_user_intent_to_settings: settings write failed");
        return false;
    }
    env.emit(
        EVENT,
        std::collections::HashMap::from([(
            "migrated_count".to_string(),
            AnalyticsValue::Int(i64::try_from(migrated_count).unwrap_or(i64::MAX)),
        )]),
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_config;

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
    async fn no_migration_needed_is_success() {
        let t = temp_config();
        assert!(run(&test_env(&t)).await);
    }

    #[tokio::test]
    async fn non_default_values_migrate_but_defaults_and_existing_keys_do_not() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"theme":"light","verbose":false,"inputNeededNotifEnabled":true}"#,
        )
        .unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, r#"{"theme":"solarized"}"#).unwrap();
        assert!(run(&test_env(&t)).await);
        let user = read_settings_map(&sp).unwrap();
        assert_eq!(user["theme"], json!("solarized"));
        assert!(user.get("verbose").is_none());
        assert_eq!(user["inputNeededNotifEnabled"], json!(true));
    }
}
