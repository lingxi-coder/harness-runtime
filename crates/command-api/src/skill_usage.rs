//! Persistent usage counters for user/project slash skills.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One entry in `<config-home>/skill_usage.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SkillUsageRecord {
    /// Number of successful expansions of this skill.
    #[serde(default)]
    pub count: u64,
    /// Unix timestamp, in milliseconds, of the most recent invocation.
    #[serde(default)]
    pub last_used_at_ms: Option<i64>,
}

const USAGE_WRITE_INTERVAL: Duration = Duration::from_secs(60);
const MILLIS_PER_DAY: f64 = 86_400_000.0;
const MILLIS_PER_WEEK: f64 = MILLIS_PER_DAY * 7.0;

/// Immutable lifetime-usage view for one skill-listing build. All candidates
/// share the same records and timestamp, so priority comparisons are made
/// against one coherent snapshot.
#[derive(Debug, Clone)]
pub struct SkillUsageSnapshot {
    usage: HashMap<Vec<u16>, SkillUsageRecord>,
    now_unix_ms: i64,
}

impl SkillUsageSnapshot {
    /// Read the global usage map and capture the current epoch-millisecond time
    /// once. A listing producer should reuse this value for every row.
    pub fn read(config_home: &Path) -> Result<Self, String> {
        Ok(Self {
            usage: read_skill_usage(config_home)?,
            now_unix_ms: current_unix_ms(),
        })
    }

    /// Native `xwt(command.name)` priority using the shared listing timestamp.
    #[must_use]
    pub fn priority_for_command(&self, name: &Utf16JsonProjection) -> f64 {
        skill_usage_priority(&self.usage, name, self.now_unix_ms)
    }
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Session-local throttle paired with the persistent lifetime usage map.
/// Native keeps its 60-second per-session/name debounce separately from the
/// global-config counters; composition roots should create one instance for a
/// session and share clones across slash and Skill-tool invocation paths.
#[derive(Clone)]
pub struct SkillUsageSession {
    config_home: PathBuf,
    last_recorded: Arc<Mutex<HashMap<Vec<u16>, Instant>>>,
}

impl SkillUsageSession {
    /// Create a session-local usage throttle that writes under `config_home`.
    /// Clones share the same 60-second per-name throttle.
    #[must_use]
    pub fn new(config_home: PathBuf) -> Self {
        Self {
            config_home,
            last_recorded: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record a canonical skill name unless this session recorded the same
    /// name less than 60 seconds ago. Returns true only when the persistent
    /// lifetime counter was updated.
    pub fn record(&self, name: &Utf16JsonProjection) -> Result<bool, String> {
        self.record_at(name, Instant::now())
    }

    fn record_at(&self, name: &Utf16JsonProjection, now: Instant) -> Result<bool, String> {
        let units = name
            .string_units("")
            .ok_or_else(|| "skill usage name must be a string".to_string())?;
        let mut last_recorded = self
            .last_recorded
            .lock()
            .map_err(|_| "skill usage session lock poisoned".to_string())?;
        if last_recorded
            .get(&units)
            .is_some_and(|last| now.duration_since(*last) < USAGE_WRITE_INTERVAL)
        {
            return Ok(false);
        }

        // Native marks the per-session debounce before dispatching the async
        // global-config update, so a failed storage write still consumes this
        // invocation's 60-second slot.
        last_recorded.insert(units, now);
        record_skill_usage(&self.config_home, name)?;
        Ok(true)
    }
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

/// Native `xwt(name)` priority for a canonical command name. The input map is
/// read once by the listing producer, and `now_unix_ms` is shared across all
/// entries in that snapshot. Missing names score zero; a present record with
/// no last-used timestamp yields NaN, matching the source arithmetic.
#[must_use]
pub fn skill_usage_priority(
    usage: &HashMap<Vec<u16>, SkillUsageRecord>,
    name: &Utf16JsonProjection,
    now_unix_ms: i64,
) -> f64 {
    let Some(name_units) = name.string_units("") else {
        return 0.0;
    };
    let Some(record) = usage.get(&name_units) else {
        return 0.0;
    };
    let Some(last_used_at_ms) = record.last_used_at_ms else {
        return f64::NAN;
    };

    let elapsed_ms = now_unix_ms as f64 - last_used_at_ms as f64;
    let decay = 0.5_f64.powf(elapsed_ms / MILLIS_PER_WEEK);
    let native_max = if decay.is_nan() {
        f64::NAN
    } else if decay < 0.1 {
        0.1
    } else {
        decay
    };
    record.count as f64 * native_max
}

/// Record one invocation using its exact canonical command name. Session-level
/// 60-second throttling is provided by [`SkillUsageSession`]. The update is
/// written through a same-directory create-new temporary file and atomic rename.
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
    let now_unix_ms = current_unix_ms();
    let entry = map.entry(name_units).or_default();
    entry.count = entry.count.saturating_add(1);
    entry.last_used_at_ms = Some(now_unix_ms);

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
            .last_used_at_ms
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
            r#"{"p\ud800":{"count":2,"last_used_at_ms":10000},"ordinary":{"count":4,"last_used_at_ms":20000}}"#,
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
        assert!(bytes.contains(r#"p\ud800"#));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn priority_matches_native_lifetime_count_and_seven_day_half_life() {
        let name: Utf16JsonProjection = "review".into();
        let units = name.string_units("").unwrap();
        let now_ms = 1_800_000_000_000_i64;
        let mut usage = HashMap::from([(
            units,
            SkillUsageRecord {
                count: 16,
                last_used_at_ms: Some(now_ms - 7 * 86_400_000),
            },
        )]);
        assert!((skill_usage_priority(&usage, &name, now_ms) - 8.0).abs() < f64::EPSILON);

        usage
            .get_mut(&name.string_units("").unwrap())
            .unwrap()
            .last_used_at_ms = Some(now_ms - 28 * 86_400_000);
        assert!((skill_usage_priority(&usage, &name, now_ms) - 1.6).abs() < f64::EPSILON);
        assert_eq!(skill_usage_priority(&usage, &"other".into(), now_ms), 0.0);
    }

    #[test]
    fn listing_snapshot_reuses_one_timestamp_for_all_candidates() {
        let now_ms = 1_800_000_000_000_i64;
        let snapshot = SkillUsageSnapshot {
            usage: HashMap::from([
                (
                    "recent".encode_utf16().collect(),
                    SkillUsageRecord {
                        count: 4,
                        last_used_at_ms: Some(now_ms),
                    },
                ),
                (
                    "older".encode_utf16().collect(),
                    SkillUsageRecord {
                        count: 8,
                        last_used_at_ms: Some(now_ms - 7 * 86_400_000),
                    },
                ),
            ]),
            now_unix_ms: now_ms,
        };

        assert_eq!(snapshot.priority_for_command(&"recent".into()), 4.0);
        assert_eq!(snapshot.priority_for_command(&"older".into()), 4.0);
        assert_eq!(snapshot.priority_for_command(&"missing".into()), 0.0);
    }

    #[test]
    fn session_throttles_same_name_for_sixty_seconds_but_not_other_sessions() {
        let root = temp_dir("session-throttle");
        let first = SkillUsageSession::new(root.clone());
        let second = SkillUsageSession::new(root.clone());
        let name: Utf16JsonProjection = "review".into();
        let start = Instant::now();

        assert!(first.record_at(&name, start).unwrap());
        assert!(!first
            .record_at(&name, start + Duration::from_secs(59))
            .unwrap());
        assert!(second
            .record_at(&name, start + Duration::from_secs(59))
            .unwrap());
        assert!(first
            .record_at(&name, start + Duration::from_secs(60))
            .unwrap());
        let usage = read_skill_usage(&root).unwrap();
        assert_eq!(usage.get(&name.string_units("").unwrap()).unwrap().count, 3);
        std::fs::remove_dir_all(root).ok();
    }
}
