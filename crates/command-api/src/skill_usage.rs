//! Persistent usage counters for user/project slash skills.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One entry in `<config-home>/skill_usage.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SkillUsageRecord {
    /// Number of successful expansions of this skill.
    #[serde(default)]
    pub count: u64,
    /// Unix timestamp of the most recent successful expansion.
    #[serde(default)]
    pub last_used_unix: Option<i64>,
}

fn usage_log_path(config_home: &Path) -> PathBuf {
    config_home.join("skill_usage.json")
}

/// Read the persistent skill usage map with exact UTF-16 command-name keys.
/// Missing and empty files are empty maps; malformed JSON is surfaced so
/// `/skill-doctor` can report it truthfully.
pub fn read_skill_usage(config_home: &Path) -> Result<HashMap<Vec<u16>, SkillUsageRecord>, String> {
    let path = usage_log_path(config_home);
    match lingxi_core::host::rooted_fs::read_to_string(config_home, Path::new("skill_usage.json")) {
        Ok(content) if content.trim().is_empty() => Ok(HashMap::new()),
        Ok(content) => {
            let source = Utf16JsonProjection::parse(&content)
                .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
            let entries = source
                .object_entries()
                .ok_or_else(|| format!("invalid JSON in {}: expected an object", path.display()))?;
            let mut usage = HashMap::with_capacity(entries.len());
            for entry in entries {
                let value = source
                    .subprojection_property_units(entry.code_units)
                    .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
                let value_json = value
                    .to_json_string()
                    .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
                let record = serde_json::from_str(&value_json)
                    .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
                usage.insert(entry.code_units.to_vec(), record);
            }
            Ok(usage)
        }
        Err(lingxi_core::host::FsError::NotFound(_)) => Ok(HashMap::new()),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
    }
}

/// Record a successful invocation of a markdown/MCP command using its exact
/// source name. The update is written through a same-directory create-new
/// temporary file and atomic rename.
pub fn record_skill_usage(config_home: &Path, name: &Utf16JsonProjection) -> Result<(), String> {
    let name_units = name
        .string_units("")
        .ok_or_else(|| "skill usage name must be a string".to_string())?;
    let path = usage_log_path(config_home);
    std::fs::create_dir_all(config_home)
        .map_err(|e| format!("failed to create {}: {e}", config_home.display()))?;
    let _lock = lingxi_core::host::rooted_fs::lock_exclusive(
        config_home,
        Path::new(".skill_usage.lock"),
        lingxi_core::host::rooted_fs::PRIVATE_DIR_MODE,
        lingxi_core::host::rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|e| format!("failed to lock {}: {e}", path.display()))?;
    let mut map = read_skill_usage(config_home)?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let entry = map.entry(name_units).or_default();
    entry.count = entry.count.saturating_add(1);
    entry.last_used_unix = Some(now_unix);

    let fields = map
        .into_iter()
        .map(|(key, record)| {
            let json = serde_json::to_string(&record)
                .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
            let value = Utf16JsonProjection::parse(&json)
                .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
            Ok((key, value))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut serialized = Utf16JsonProjection::object_units(fields)
        .and_then(|projection| projection.to_pretty_json_string())
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?
        .into_bytes();
    serialized.push(b'\n');
    lingxi_core::host::rooted_fs::atomic_write(
        config_home,
        Path::new("skill_usage.json"),
        &serialized,
        lingxi_core::host::rooted_fs::AtomicWriteOptions::default(),
    )
    .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "command-api-skill-usage-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn record_is_incremental_and_never_leaves_a_temp_file() {
        let root = temp_dir("incremental");
        std::fs::create_dir_all(&root).unwrap();
        record_skill_usage(&root, &"review".into()).unwrap();
        record_skill_usage(&root, &"review".into()).unwrap();
        let usage = read_skill_usage(&root).unwrap();
        assert_eq!(
            usage
                .get(&"review".encode_utf16().collect::<Vec<_>>())
                .map(|row| row.count),
            Some(2)
        );
        assert!(usage
            .get(&"review".encode_utf16().collect::<Vec<_>>())
            .unwrap()
            .last_used_unix
            .is_some());
        assert!(!std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn lone_surrogate_name_survives_usage_file_update() {
        let root = temp_dir("lone");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("skill_usage.json"),
            r#"{"p\ud800":{"count":2,"last_used_unix":10},"ordinary":{"count":4,"last_used_unix":20}}"#,
        )
        .unwrap();
        let name =
            Utf16JsonProjection::root_string("p�".into(), vec![b'p' as u16, 0xd800]).unwrap();
        record_skill_usage(&root, &name).unwrap();
        let usage = read_skill_usage(&root).unwrap();
        assert_eq!(
            usage.get(&vec![b'p' as u16, 0xd800]).map(|row| row.count),
            Some(3)
        );
        assert_eq!(
            usage
                .get(&"ordinary".encode_utf16().collect::<Vec<_>>())
                .map(|row| row.count),
            Some(4)
        );
        let bytes = std::fs::read_to_string(root.join("skill_usage.json")).unwrap();
        assert!(bytes.contains(r#"p\\ud800"#));
        std::fs::remove_dir_all(root).ok();
    }
}
