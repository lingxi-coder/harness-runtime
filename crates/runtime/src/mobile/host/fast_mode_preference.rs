//! Device-owned last explicit Fast mode choice.
use std::path::Path;

pub(super) fn load(home: &Path) -> Option<bool> {
    if home.as_os_str().is_empty() {
        return None;
    }
    let map = migrations::global_config::read_map(&home.join("last-fast-mode.json")).ok()?;
    map.get("enabled")?.as_bool()
}

pub(super) fn save(home: &Path, enabled: bool) -> Result<(), String> {
    if home.as_os_str().is_empty() {
        return Err("invalid Fast mode preference path".into());
    }
    std::fs::create_dir_all(home).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&serde_json::json!({ "enabled": enabled }))
        .map_err(|error| error.to_string())?;
    lingxi_core::host::rooted_fs::atomic_write(
        home,
        Path::new("last-fast-mode.json"),
        &bytes,
        lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_fast_mode_choice_survives_reopening_without_touching_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"reasoning":{"defaultSelection":{"type":"level","id":"high"}}}"#,
        )
        .unwrap();

        for enabled in [true, false] {
            save(dir.path(), enabled).unwrap();
            assert_eq!(load(dir.path()), Some(enabled));
        }
        assert!(std::fs::read_to_string(dir.path().join("settings.json"))
            .unwrap()
            .contains("defaultSelection"));
    }

    #[test]
    fn invalid_fast_mode_preferences_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
        std::fs::write(dir.path().join("last-fast-mode.json"), "{\"enabled\":true}").unwrap();
        assert_eq!(load(dir.path()), Some(true));
        std::fs::write(
            dir.path().join("last-fast-mode.json"),
            "{\"enabled\":\"yes\"}",
        )
        .unwrap();
        assert_eq!(load(dir.path()), None);
        std::fs::write(dir.path().join("last-fast-mode.json"), "broken").unwrap();
        save(dir.path(), false).unwrap();
        assert_eq!(load(dir.path()), Some(false));
    }

    #[test]
    fn persistence_errors_are_not_reported_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("not-a-directory");
        std::fs::write(&home, "occupied").unwrap();
        assert!(save(&home, true).is_err());
        assert_eq!(std::fs::read_to_string(&home).unwrap(), "occupied");
    }
}
