//! `migrateBypassPermissionsAcceptedToSettings.ts` — move
//! `bypassPermissionsModeAccepted` from global config to
//! `settings.json skipDangerousModePermissionPrompt`. The written key has no
//! Rust consumer yet (the `--dangerously-skip-permissions` posture is the
//! separately-confirmed remainder item B) — the file-level move is the
//! faithful contract.
//!
//! `hasSkipDangerousModePermissionPrompt` (`settings.ts:882-889`) checks
//! user/local/flag/policy sources; this port checks user+local (the flag and
//! policy sources have no Rust substrate — documented).

use crate::context::{js_truthy, MigrationEnv};
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use serde_json::json;

/// Run the migration.
pub async fn run(env: &MigrationEnv) -> bool {
    let cfg = match global_config::read_map(&env.global_config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "migrate_bypass_permissions: config read failed");
            return true;
        }
    };
    if !cfg
        .get("bypassPermissionsModeAccepted")
        .is_some_and(js_truthy)
    {
        return true;
    }

    let has_skip = [WritableScope::User, WritableScope::Local].iter().any(|s| {
        let p = settings_path(*s, &env.lingxi_config_home, &env.project_dir);
        read_settings_map(&p)
            .ok()
            .and_then(|m| m.get("skipDangerousModePermissionPrompt").map(js_truthy))
            .unwrap_or(false)
    });
    if !has_skip {
        let sp = settings_path(
            WritableScope::User,
            &env.lingxi_config_home,
            &env.project_dir,
        );
        if let Err(e) = update_settings(
            &sp,
            vec![(
                "skipDangerousModePermissionPrompt".into(),
                Some(json!(true)),
            )],
        ) {
            tracing::warn!(error = %e, "migrate_bypass_permissions: settings write failed");
            return false;
        }
    }

    env.emit(
        telemetry::tengu::migration::MIGRATE_BYPASS_PERMISSIONS_ACCEPTED,
        std::collections::HashMap::new(),
    )
    .await;

    if let Err(e) = global_config::save_map(&env.global_config_path, |mut m| {
        m.remove("bypassPermissionsModeAccepted");
        m
    }) {
        tracing::warn!(error = %e, "migrate_bypass_permissions: config cleanup failed");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_update::{
        force_rename_failure_for_test, read_settings_map, settings_path, WritableScope,
    };
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
    async fn moves_flag_to_user_settings_and_removes_config_key() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"bypassPermissionsModeAccepted": true}"#).unwrap();
        run(&test_env(&t)).await;
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        let s = read_settings_map(&sp).unwrap();
        assert_eq!(s["skipDangerousModePermissionPrompt"], json!(true));
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("bypassPermissionsModeAccepted").is_none());
    }

    #[tokio::test]
    async fn existing_skip_flag_in_local_settings_is_not_overwritten() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"bypassPermissionsModeAccepted": true}"#).unwrap();
        let lp = settings_path(WritableScope::Local, &t.home, &t.project);
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        std::fs::write(&lp, r#"{"skipDangerousModePermissionPrompt": true}"#).unwrap();
        run(&test_env(&t)).await;
        // userSettings must NOT gain the key (TS: hasSkip… short-circuits)
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        let s = read_settings_map(&sp).unwrap();
        assert!(s.get("skipDangerousModePermissionPrompt").is_none());
        // config key still removed
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("bypassPermissionsModeAccepted").is_none());
    }

    /// In 2.1.245 a failing settings write aborts the migration and preserves
    /// the legacy config key so the runner can retry later.
    #[cfg(unix)]
    #[tokio::test]
    async fn settings_write_failure_preserves_config_key() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"bypassPermissionsModeAccepted": true}"#).unwrap();
        let sp = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
        std::fs::write(&sp, "{}").unwrap();
        let _failure = force_rename_failure_for_test(&sp);
        run(&test_env(&t)).await;
        // write failed → settings unchanged…
        assert!(read_settings_map(&sp)
            .unwrap()
            .get("skipDangerousModePermissionPrompt")
            .is_none());
        // …and the config key is preserved for a retry.
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["bypassPermissionsModeAccepted"], json!(true));
    }

    #[tokio::test]
    async fn absent_or_falsy_config_flag_is_noop() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"bypassPermissionsModeAccepted": false}"#).unwrap();
        run(&test_env(&t)).await;
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["bypassPermissionsModeAccepted"], json!(false)); // untouched
    }
}
