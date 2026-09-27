//! `migrateEnableAllProjectMcpServersToSettings.ts` — move the three MCP
//! approval fields from the project config (inside `~/.lingxi.json`
//! `projects[<key>]`) into `<project>/.lingxi/settings.local.json`.
//! No Rust reader consumes these settings keys yet; the file-level move is
//! the faithful contract.

use crate::context::MigrationEnv;
use crate::global_config;
use crate::settings_update::{read_settings_map, settings_path, update_settings, WritableScope};
use serde_json::{Map, Value};
use telemetry::sink::AnalyticsValue;

/// JS `[...new Set([...a, ...b])]`: a's order, then b's not-already-present.
fn union_preserving_order(existing: &[Value], incoming: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = existing.to_vec();
    for v in incoming {
        if !out.contains(v) {
            out.push(v.clone());
        }
    }
    out
}

/// Run the migration.
pub async fn run(env: &MigrationEnv) {
    let key = global_config::project_path_for_config(&env.project_dir);
    // `getCurrentProjectConfig()` (TS:18) sits OUTSIDE the try block — a
    // failure here is not the error-event path; warn and skip.
    let proj = match global_config::get_project_config(&env.global_config_path, &key) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "migrate_mcp_servers: config read failed");
            return;
        }
    };

    let has_enable_all = proj.get("enableAllProjectMcpServers").is_some();
    let enabled: Vec<Value> = proj
        .get("enabledMcpjsonServers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let disabled: Vec<Value> = proj
        .get("disabledMcpjsonServers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !has_enable_all && enabled.is_empty() && disabled.is_empty() {
        return;
    }

    // TS try block (TS:33-113): of everything inside it, only
    // `saveCurrentProjectConfig` can actually throw —
    // `getSettingsForSource('localSettings')` (settings.ts:309) reaches
    // `parseSettingsFileUncached` (settings.ts:201-231), which returns
    // `{settings: null}` on a broken file (the `{}` comes from the
    // migration's own `|| {}` at TS:34), and `updateSettingsForSource`
    // returns an ignored `{error}` (settings.ts:416-523). So only the
    // project-config save failure reaches
    // `tengu_migrate_mcp_approval_fields_error`.
    let lp = settings_path(
        WritableScope::Local,
        &env.lingxi_config_home,
        &env.project_dir,
    );
    let existing = read_settings_map(&lp).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "migrate_mcp_servers: settings read failed (treated as empty, TS parity)");
        Map::new()
    });

    let mut updates: Vec<(String, Option<Value>)> = Vec::new();
    let mut fields_to_remove = 0usize;

    if has_enable_all {
        if existing.get("enableAllProjectMcpServers").is_none() {
            updates.push((
                "enableAllProjectMcpServers".into(),
                proj.get("enableAllProjectMcpServers").cloned(),
            ));
        }
        // Already-migrated still counts for removal (TS:56-58).
        fields_to_remove += 1;
    }
    if !enabled.is_empty() {
        let merged = union_preserving_order(
            existing
                .get("enabledMcpjsonServers")
                .and_then(Value::as_array)
                .map_or(&[], Vec::as_slice),
            &enabled,
        );
        updates.push(("enabledMcpjsonServers".into(), Some(Value::Array(merged))));
        fields_to_remove += 1;
    }
    if !disabled.is_empty() {
        let merged = union_preserving_order(
            existing
                .get("disabledMcpjsonServers")
                .and_then(Value::as_array)
                .map_or(&[], Vec::as_slice),
            &disabled,
        );
        updates.push(("disabledMcpjsonServers".into(), Some(Value::Array(merged))));
        fields_to_remove += 1;
    }

    if !updates.is_empty() {
        if let Err(e) = update_settings(&lp, updates) {
            // TS discards the returned `{error}` (TS:90-92) — warn, continue.
            tracing::warn!(error = %e, "migrate_mcp_servers: settings write failed (ignored, TS parity)");
        }
    }

    // TS removes ALL THREE keys from the project config in one destructure
    // (TS:95-110); `saveCurrentProjectConfig` is the one throw-capable call
    // inside the TS try — a failure routes to the catch → error event.
    if let Err(e) = global_config::save_project_config(&env.global_config_path, &key, |mut p| {
        p.remove("enableAllProjectMcpServers");
        p.remove("enabledMcpjsonServers");
        p.remove("disabledMcpjsonServers");
        p
    }) {
        tracing::warn!(error = %e, "migrate_mcp_servers: project-config cleanup failed");
        env.emit(
            telemetry::tengu::migration::MIGRATE_MCP_APPROVAL_FIELDS_ERROR,
            std::collections::HashMap::new(),
        )
        .await;
        return;
    }

    env.emit(
        telemetry::tengu::migration::MIGRATE_MCP_APPROVAL_FIELDS_SUCCESS,
        std::collections::HashMap::from([(
            "migratedCount".to_string(),
            AnalyticsValue::Int(i64::try_from(fields_to_remove).unwrap_or(i64::MAX)),
        )]),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_update::{read_settings_map, settings_path, WritableScope};
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

    fn project_key(t: &crate::test_support::TempConfig) -> String {
        crate::global_config::project_path_for_config(&t.project)
    }

    #[tokio::test]
    async fn moves_all_three_fields_to_local_settings() {
        let t = temp_config();
        let key = project_key(&t);
        std::fs::write(
            &t.global,
            serde_json::to_string(&json!({"projects": {key.clone(): {
                "enableAllProjectMcpServers": true,
                "enabledMcpjsonServers": ["a", "b"],
                "disabledMcpjsonServers": ["c"],
                "other": 1
            }}}))
            .unwrap(),
        )
        .unwrap();
        run(&test_env(&t)).await;

        let lp = settings_path(WritableScope::Local, &t.home, &t.project);
        let s = read_settings_map(&lp).unwrap();
        assert_eq!(s["enableAllProjectMcpServers"], json!(true));
        assert_eq!(s["enabledMcpjsonServers"], json!(["a", "b"]));
        assert_eq!(s["disabledMcpjsonServers"], json!(["c"]));

        let proj = crate::global_config::get_project_config(&t.global, &key).unwrap();
        assert!(proj.get("enableAllProjectMcpServers").is_none());
        assert!(proj.get("enabledMcpjsonServers").is_none());
        assert!(proj.get("disabledMcpjsonServers").is_none());
        assert_eq!(proj["other"], json!(1));
    }

    #[tokio::test]
    async fn merges_server_lists_dedup_preserving_existing_order() {
        let t = temp_config();
        let key = project_key(&t);
        std::fs::write(
            &t.global,
            serde_json::to_string(&json!({"projects": {key: {
                "enabledMcpjsonServers": ["b", "c"]
            }}}))
            .unwrap(),
        )
        .unwrap();
        let lp = settings_path(WritableScope::Local, &t.home, &t.project);
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        std::fs::write(&lp, r#"{"enabledMcpjsonServers": ["a", "b"]}"#).unwrap();
        run(&test_env(&t)).await;
        let s = read_settings_map(&lp).unwrap();
        // [...new Set([...existing, ...incoming])] = a, b, c
        assert_eq!(s["enabledMcpjsonServers"], json!(["a", "b", "c"]));
    }

    #[tokio::test]
    async fn already_migrated_enable_all_is_removed_but_not_overwritten() {
        let t = temp_config();
        let key = project_key(&t);
        std::fs::write(
            &t.global,
            serde_json::to_string(&json!({"projects": {key.clone(): {
                "enableAllProjectMcpServers": true
            }}}))
            .unwrap(),
        )
        .unwrap();
        let lp = settings_path(WritableScope::Local, &t.home, &t.project);
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        std::fs::write(&lp, r#"{"enableAllProjectMcpServers": false}"#).unwrap();
        run(&test_env(&t)).await;
        let s = read_settings_map(&lp).unwrap();
        assert_eq!(s["enableAllProjectMcpServers"], json!(false)); // kept
        let proj = crate::global_config::get_project_config(&t.global, &key).unwrap();
        assert!(proj.get("enableAllProjectMcpServers").is_none()); // removed
    }

    #[tokio::test]
    async fn nothing_to_migrate_is_noop() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"projects": {}}"#).unwrap();
        let before = std::fs::read_to_string(&t.global).unwrap();
        run(&test_env(&t)).await;
        assert_eq!(std::fs::read_to_string(&t.global).unwrap(), before);
    }

    /// Convention check: a broken local settings file must NOT divert to the
    /// error event or block the project-config cleanup — TS
    /// `getSettingsForSource('localSettings')` (settings.ts:309) reaches
    /// `parseSettingsFileUncached` (settings.ts:201-231), which returns
    /// `{settings: null}` on a broken file (never throws; the migration's own
    /// `|| {}` at TS:34 makes it `{}`), and `updateSettingsForSource` returns
    /// an ignored `{error}` (settings.ts:416-523), so
    /// `saveCurrentProjectConfig` still runs and removes the fields.
    #[tokio::test]
    async fn broken_local_settings_still_removes_project_fields() {
        let t = temp_config();
        let key = project_key(&t);
        std::fs::write(
            &t.global,
            serde_json::to_string(&json!({"projects": {key.clone(): {
                "enabledMcpjsonServers": ["a"]
            }}}))
            .unwrap(),
        )
        .unwrap();
        let lp = settings_path(WritableScope::Local, &t.home, &t.project);
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        std::fs::write(&lp, "{ broken").unwrap();
        run(&test_env(&t)).await;
        // broken file untouched (update_settings bails without overwriting)…
        assert_eq!(std::fs::read_to_string(&lp).unwrap(), "{ broken");
        // …but the TS success path still removed the project-config fields.
        let proj = crate::global_config::get_project_config(&t.global, &key).unwrap();
        assert!(proj.get("enabledMcpjsonServers").is_none());
    }
}
