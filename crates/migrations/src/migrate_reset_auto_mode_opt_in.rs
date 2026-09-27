//! `resetAutoModeOptInForDefaultOffer.ts` — once auto mode is on by default,
//! clear the old opt-in skip bit unless the user explicitly chose
//! `permissions.defaultMode = "auto"`.

use crate::context::{js_truthy, MigrationEnv};
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use serde_json::json;

const EVENT: &str = "tengu_migrate_reset_auto_opt_in_for_default_offer";

/// Run the migration. Returns `false` only when the settings/global write
/// needed by the oracle path failed.
pub async fn run(env: &MigrationEnv) -> bool {
    let cfg = match global_config::read_map(&env.global_config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "migrate_reset_auto_mode_opt_in: config read failed");
            return true;
        }
    };
    if cfg
        .get("hasResetAutoModeOptInForDefaultOffer")
        .is_some_and(js_truthy)
    {
        return true;
    }

    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let user = read_settings_map(&sp).unwrap_or_default();
    let should_clear = user.get("skipAutoPermissionPrompt").is_some_and(js_truthy)
        && user
            .get("permissions")
            .and_then(|v| v.get("defaultMode"))
            .and_then(serde_json::Value::as_str)
            != Some("auto");

    if should_clear
        && update_settings(&sp, vec![("skipAutoPermissionPrompt".into(), None)]).is_err()
    {
        tracing::warn!("migrate_reset_auto_mode_opt_in: settings write failed");
        return false;
    }
    if should_clear {
        env.emit(EVENT, std::collections::HashMap::new()).await;
    }

    let Ok(_) = global_config::save_map(&env.global_config_path, |mut map| {
        if map
            .get("hasResetAutoModeOptInForDefaultOffer")
            .is_some_and(js_truthy)
        {
            return map;
        }
        map.insert(
            "hasResetAutoModeOptInForDefaultOffer".to_string(),
            json!(true),
        );
        map
    }) else {
        tracing::warn!("migrate_reset_auto_mode_opt_in: flag write failed");
        return false;
    };
    true
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
    async fn clears_skip_bit_when_not_in_auto_mode() {
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(
            &sp,
            r#"{"skipAutoPermissionPrompt": true, "permissions": {"defaultMode": "plan"}}"#,
        )
        .unwrap();
        assert!(run(&test_env(&t)).await);
        let user = read_settings_map(&sp).unwrap();
        assert!(user.get("skipAutoPermissionPrompt").is_none());
        let cfg = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(cfg["hasResetAutoModeOptInForDefaultOffer"], json!(true));
    }

    #[tokio::test]
    async fn keeps_skip_bit_for_explicit_auto_mode() {
        let t = temp_config();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(
            &sp,
            r#"{"skipAutoPermissionPrompt": true, "permissions": {"defaultMode": "auto"}}"#,
        )
        .unwrap();
        assert!(run(&test_env(&t)).await);
        let user = read_settings_map(&sp).unwrap();
        assert_eq!(user["skipAutoPermissionPrompt"], json!(true));
        let cfg = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(cfg["hasResetAutoModeOptInForDefaultOffer"], json!(true));
    }
}
