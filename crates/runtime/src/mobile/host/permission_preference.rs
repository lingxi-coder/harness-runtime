//! Device-owned last explicit permission choice. Session snapshots are separate.
use permission::PermissionMode;
use std::path::Path;

fn parse(mode: &str) -> Option<PermissionMode> {
    match mode {
        "default" => Some(PermissionMode::Default),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "plan" => Some(PermissionMode::Plan),
        "auto" => Some(PermissionMode::Auto),
        "dontAsk" => Some(PermissionMode::DontAsk),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        _ => None,
    }
}

pub(super) fn load(home: &Path) -> Option<PermissionMode> {
    if home.as_os_str().is_empty() {
        return None;
    }
    let map = migrations::global_config::read_map(&home.join("last-permission-mode.json")).ok()?;
    parse(map.get("mode")?.as_str()?)
}

pub(super) fn save(home: &Path, mode: &str) -> Result<(), String> {
    if home.as_os_str().is_empty() || parse(mode).is_none() {
        return Err("invalid permission preference path or mode".into());
    }
    std::fs::create_dir_all(home).map_err(|error| error.to_string())?;
    // This file contains only the last selection, so no read/modify/write is
    // needed. An atomic replacement also repairs malformed preference JSON.
    let bytes = serde_json::to_vec(&serde_json::json!({ "mode": mode }))
        .map_err(|error| error.to_string())?;
    platform_api::rooted_fs::atomic_write(
        home,
        Path::new("last-permission-mode.json"),
        &bytes,
        platform_api::rooted_fs::AtomicWriteOptions::default(),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_permission_choice_survives_reopening_without_mutating_default_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            "{\"permissions\":{\"defaultMode\":\"default\"}}",
        )
        .unwrap();
        for mode in [
            "plan",
            "acceptEdits",
            "default",
            "auto",
            "dontAsk",
            "bypassPermissions",
        ] {
            save(dir.path(), mode).unwrap();
            assert_eq!(load(dir.path()).unwrap().wire_str(), mode);
        }
        assert!(std::fs::read_to_string(dir.path().join("settings.json"))
            .unwrap()
            .contains("defaultMode\":\"default"));
    }
    #[test]
    fn invalid_preferences_do_not_become_permission_grants() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_none());
        save(dir.path(), "plan").unwrap();
        assert!(save(dir.path(), "unknown").is_err());
        assert_eq!(load(dir.path()), Some(PermissionMode::Plan));
        std::fs::write(
            dir.path().join("last-permission-mode.json"),
            "{\"mode\":true}",
        )
        .unwrap();
        assert!(load(dir.path()).is_none());
        std::fs::write(dir.path().join("last-permission-mode.json"), "broken").unwrap();
        save(dir.path(), "acceptEdits").unwrap();
        assert_eq!(load(dir.path()), Some(PermissionMode::AcceptEdits));
    }
    #[test]
    fn persistence_errors_are_not_reported_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("not-a-directory");
        std::fs::write(&home, "occupied").unwrap();
        assert!(save(&home, "plan").is_err());
        assert_eq!(std::fs::read_to_string(&home).unwrap(), "occupied");
    }
}
