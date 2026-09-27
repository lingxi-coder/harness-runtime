//! Persistent usage counters for user/project slash skills.

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

/// Read the persistent skill usage map. Missing and empty files are empty maps;
/// malformed JSON is surfaced so `/skill-doctor` can report it truthfully.
pub fn read_skill_usage(config_home: &Path) -> Result<HashMap<String, SkillUsageRecord>, String> {
    let path = usage_log_path(config_home);
    match platform_api::rooted_fs::read_to_string(config_home, Path::new("skill_usage.json")) {
        Ok(content) if content.trim().is_empty() => Ok(HashMap::new()),
        Ok(content) => serde_json::from_str(&content)
            .map_err(|e| format!("invalid JSON in {}: {e}", path.display())),
        Err(platform_api::FsError::NotFound(_)) => Ok(HashMap::new()),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
    }
}

/// Record a successful invocation of a user/project markdown command.
///
/// The update is written through a same-directory create-new temporary file and
/// atomic rename, so a crash never leaves a partially-written JSON document.
pub fn record_skill_usage(config_home: &Path, name: &str) -> Result<(), String> {
    let path = usage_log_path(config_home);
    std::fs::create_dir_all(config_home)
        .map_err(|e| format!("failed to create {}: {e}", config_home.display()))?;
    let _lock = platform_api::rooted_fs::lock_exclusive(
        config_home,
        Path::new(".skill_usage.lock"),
        platform_api::rooted_fs::PRIVATE_DIR_MODE,
        platform_api::rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|e| format!("failed to lock {}: {e}", path.display()))?;
    let mut map = read_skill_usage(config_home)?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let entry = map.entry(name.to_string()).or_default();
    entry.count = entry.count.saturating_add(1);
    entry.last_used_unix = Some(now_unix);
    let mut serialized = serde_json::to_vec_pretty(&map)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    serialized.push(b'\n');
    platform_api::rooted_fs::atomic_write(
        config_home,
        Path::new("skill_usage.json"),
        &serialized,
        platform_api::rooted_fs::AtomicWriteOptions::default(),
    )
    .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_incremental_and_never_leaves_a_temp_file() {
        let root = std::env::temp_dir().join(format!(
            "command-api-skill-usage-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        record_skill_usage(&root, "review").unwrap();
        record_skill_usage(&root, "review").unwrap();
        let usage = read_skill_usage(&root).unwrap();
        assert_eq!(usage.get("review").map(|row| row.count), Some(2));
        assert!(usage.get("review").unwrap().last_used_unix.is_some());
        assert!(!std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp")));
        std::fs::remove_dir_all(root).ok();
    }
}
