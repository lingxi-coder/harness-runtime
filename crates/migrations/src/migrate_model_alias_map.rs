//! `migrateModelAliases.ts` — optional model-alias map migration.
//!
//! The 2.1.245 external runner passes `undefined`, so the startup path is an
//! exact no-op. Keep the helper shape so the runner order matches the oracle.

use crate::context::MigrationEnv;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use serde_json::json;
use std::collections::HashMap;

/// Run the migration. Returns `false` only if a provided alias map needed to
/// write `userSettings.model` and that write failed.
pub async fn run(alias_map: Option<&HashMap<String, String>>, env: &MigrationEnv) -> bool {
    let Some(alias_map) = alias_map else {
        return true;
    };

    let sp = settings_path(
        WritableScope::User,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let Some(model) = read_settings_map(&sp).ok().and_then(|m| {
        m.get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }) else {
        return true;
    };
    let Some(target) = alias_map.get(&model) else {
        return true;
    };
    if target.is_empty() {
        return true;
    }

    let value = if model.ends_with("[1m]") && model != target.as_str() {
        format!("{target}[1m]")
    } else {
        target.clone()
    };
    let Ok(()) = update_settings(&sp, vec![("model".into(), Some(json!(value)))]) else {
        tracing::warn!("migrate_model_alias_map: settings write failed");
        return false;
    };
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
    async fn none_map_is_a_noop_success() {
        let t = temp_config();
        assert!(run(None, &test_env(&t)).await);
    }
}
