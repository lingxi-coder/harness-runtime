//! `migrateNotificationDismissals.ts` — populate `seenNotifications` from the
//! legacy `subscriptionNoticeCount` field when the new map is still absent.

use crate::context::MigrationEnv;
use crate::global_config;
use serde_json::{json, Map, Value};

const SUBSCRIPTION_SWITCH: &str = "subscription-switch";
const LEGACY_KEY: &str = "subscriptionNoticeCount";

/// Run the migration.
pub async fn run(env: &MigrationEnv) {
    let result = global_config::save_map(&env.global_config_path, |mut cfg| {
        if cfg.get("seenNotifications").is_some() {
            return cfg;
        }

        let mut seen = Map::new();
        match cfg.get(LEGACY_KEY) {
            Some(Value::Number(n)) if n.as_i64().is_some_and(|count| count > 0) => {
                seen.insert(SUBSCRIPTION_SWITCH.to_string(), Value::Number(n.clone()));
            }
            Some(Value::Bool(true)) => {
                seen.insert(SUBSCRIPTION_SWITCH.to_string(), json!(1));
            }
            _ => {}
        }
        cfg.insert("seenNotifications".to_string(), Value::Object(seen));
        cfg
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "migrate_notification_dismissals failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_config;
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
    async fn number_count_migrates_to_seen_notifications() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"subscriptionNoticeCount": 3}"#).unwrap();
        run(&test_env(&t)).await;
        let cfg = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(cfg["seenNotifications"], json!({"subscription-switch": 3}));
    }

    #[tokio::test]
    async fn true_count_becomes_one() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"subscriptionNoticeCount": true}"#).unwrap();
        run(&test_env(&t)).await;
        let cfg = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(cfg["seenNotifications"], json!({"subscription-switch": 1}));
    }

    #[tokio::test]
    async fn existing_seen_notifications_wins() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"subscriptionNoticeCount": 2, "seenNotifications": {"x": 1}}"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(&t.global).unwrap();
        run(&test_env(&t)).await;
        assert_eq!(std::fs::read_to_string(&t.global).unwrap(), before);
    }
}
