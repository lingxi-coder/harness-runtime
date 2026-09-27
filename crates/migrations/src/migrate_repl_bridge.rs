//! `migrateReplBridgeEnabledToRemoteControlAtStartup.ts` — copy
//! `replBridgeEnabled` to `remoteControlAtStartup` (JS-truthy-coerced) and
//! drop the old key. Idempotent: only acts when the old key exists and the
//! new one doesn't.

use crate::context::{js_truthy, MigrationEnv};
use crate::global_config;
use serde_json::Value;

/// Run the migration. Errors are swallowed with a `tracing::warn!` —
/// migrations never abort startup.
// async for the uniform migration-runner API; this one emits no telemetry,
// so it has no await point.
#[allow(clippy::unused_async)]
pub async fn run(env: &MigrationEnv) {
    let result = global_config::save_map(&env.global_config_path, |mut cfg| {
        let Some(old) = cfg.get("replBridgeEnabled").cloned() else {
            return cfg;
        };
        if cfg.get("remoteControlAtStartup").is_some() {
            return cfg;
        }
        cfg.insert(
            "remoteControlAtStartup".into(),
            Value::Bool(js_truthy(&old)),
        );
        cfg.remove("replBridgeEnabled");
        cfg
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "migrate_repl_bridge failed");
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
    async fn renames_old_key_when_new_absent() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"replBridgeEnabled": 1}"#).unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["remoteControlAtStartup"], json!(true)); // Boolean(1)
        assert!(m.get("replBridgeEnabled").is_none());
    }

    #[tokio::test]
    async fn no_old_key_is_noop_even_for_null() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"other": 1}"#).unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("remoteControlAtStartup").is_none());
        // JSON null is NOT undefined: TS `oldValue === undefined` only skips
        // a MISSING key — null proceeds and Boolean(null)=false.
        std::fs::write(&t.global, r#"{"replBridgeEnabled": null}"#).unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["remoteControlAtStartup"], json!(false));
        assert!(m.get("replBridgeEnabled").is_none());
    }

    #[tokio::test]
    async fn existing_new_key_blocks_migration() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"replBridgeEnabled": true, "remoteControlAtStartup": false}"#,
        )
        .unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["remoteControlAtStartup"], json!(false));
        assert_eq!(m["replBridgeEnabled"], json!(true)); // untouched
    }
}
