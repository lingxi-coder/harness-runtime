//! On-disk team-file helpers — 1:1 port of the path/IO surface of
//! `claude-code/src/utils/swarm/teamHelpers.ts` used by the coordinator
//! implicit session-team lifecycle.
//!
//! Layout (claude-code):
//! - team dir:  `~/.lingxi/teams/{sanitize(name)}/`
//! - team file: `~/.lingxi/teams/{sanitize(name)}/config.json`
//! - task dir:  `~/.lingxi/tasks/{sanitize(name)}/`
//!
//! NOTE: this is the claude-code *coordinator* team-file subsystem
//! (`~/.lingxi/teams/`), DISTINCT from the LingXi-internal `team-mem`
//! subsystem in `tools/team` (`~/.lingxi/team-mem/`). They do not share a
//! directory or a schema.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// `TeamFile` — 1:1 with the TS `TeamFile` type (`teamHelpers.ts:64-90`),
/// typed projection used by readers. Mutations preserve unmodeled metadata
/// by updating the JSON object directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamFile {
    /// Team name (the un-sanitized display name).
    pub name: String,
    /// Optional free-form description/purpose.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// Creation timestamp (Unix milliseconds, mirroring TS `Date.now()`).
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// Deterministic lead agent id (`team-lead@{name}` analog).
    #[serde(rename = "leadAgentId")]
    pub lead_agent_id: String,
    /// Actual session id of the leader (for team discovery).
    #[serde(
        rename = "leadSessionId",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub lead_session_id: Option<String>,
    /// Team members. The coordinator writes exactly one: the lead.
    pub members: Vec<TeamMember>,
}

/// A single team member — 1:1 with the TS member object the coordinator writes
/// in the implicit session-team configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    /// Member agent id.
    #[serde(rename = "agentId")]
    pub agent_id: String,
    /// Member name (the lead is `team-lead`).
    pub name: String,
    /// Agent type/role.
    #[serde(rename = "agentType", skip_serializing_if = "Option::is_none", default)]
    pub agent_type: Option<String>,
    /// Model id resolved for the member.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// Configured provider profile serving the resolved member model.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_profile: Option<String>,
    /// Join timestamp (Unix milliseconds).
    #[serde(rename = "joinedAt")]
    pub joined_at: u64,
    /// tmux pane id (empty for in-process teammates).
    #[serde(rename = "tmuxPaneId")]
    pub tmux_pane_id: String,
    /// Working directory.
    pub cwd: String,
    /// PR-activity subscriptions (empty on creation).
    pub subscriptions: Vec<String>,
}

/// `sanitizeName` (`teamHelpers.ts:100-102`): replace every non-ASCII
/// alphanumeric UTF-16 code unit with `-`, then lowercase ASCII letters.
#[must_use]
pub fn sanitize_name(name: &str) -> String {
    name.encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte).to_ascii_lowercase(),
            _ => '-',
        })
        .collect()
}

/// Resolve the config-home root: `$LINGXI_CONFIG_DIR` when set wins (claude-code
/// `tr()` `??`: an empty value is honored verbatim → cwd-relative), else
/// `$HOME/.claude` (tests redirect `$HOME` to a tempdir). Returns `None` when
/// neither resolves.
#[must_use]
pub fn lingxi_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(branding::DOT_DIR))
}

/// `getTeamDir` (`teamHelpers.ts:115-117`): `<lingxi_home>/teams/{sanitize}`.
#[must_use]
pub fn team_dir(home: &Path, name: &str) -> PathBuf {
    normalize_nfc_path(home)
        .join("teams")
        .join(sanitize_name(name))
}

/// Native `oze` NFC-normalizes its resolved config-home before addressing the
/// teams directory (2.1.289 source bytes `[179346876,179347230)`, SHA-256
/// `b8b17cee2f781f08b5041a50342e8ec5380c18a4f0acad783a15d239c2cd5b90`).
/// Preserve non-UTF-8 host paths unchanged; Native config-home values are strings.
fn normalize_nfc_path(path: &Path) -> PathBuf {
    path.to_str()
        .map(|value| PathBuf::from(value.nfc().collect::<String>()))
        .unwrap_or_else(|| path.to_path_buf())
}

/// `getTeamFilePath` (`teamHelpers.ts:122-124`):
/// `<lingxi_home>/teams/{sanitize}/config.json`.
#[must_use]
pub fn team_file_path(home: &Path, name: &str) -> PathBuf {
    team_dir(home, name).join("config.json")
}

/// Host adapter for actual teammate query/stop/failure lifecycle boundaries.
/// The config root is captured at construction so later process-environment
/// changes cannot redirect activity updates to another team's storage.
#[derive(Debug, Clone)]
pub struct TeamFileMemberActivity {
    home: PathBuf,
}

impl TeamFileMemberActivity {
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::team_registry::TeamMemberActivityHandle for TeamFileMemberActivity {
    async fn set_active(
        &self,
        team_name: &str,
        member_name: &str,
        active: bool,
    ) -> Result<(), String> {
        set_team_member_active(&self.home, team_name, member_name, active)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// `getTasksDir` analog: `<lingxi_home>/tasks/{sanitize}`.
#[must_use]
pub fn task_dir(home: &Path, name: &str) -> PathBuf {
    home.join("tasks").join(sanitize_name(name))
}

/// `readTeamFile` existence probe (`teamHelpers.ts:131-142`): true iff a team
/// config file exists for `name`. Used by `generateUniqueTeamName`.
#[must_use]
pub fn team_file_exists(home: &Path, name: &str) -> bool {
    team_file_path(home, name).is_file()
}

/// `writeTeamFileAsync` (`teamHelpers.ts:175-182`): create the team dir and
/// write `config.json` as pretty JSON.
///
/// # Errors
/// Returns the underlying `std::io::Error` on a mkdir / write / serialize
/// failure.
pub async fn write_team_file(home: &Path, name: &str, file: &TeamFile) -> std::io::Result<()> {
    let value = serde_json::to_value(file).map_err(std::io::Error::other)?;
    update_team_file(home, name, |current| {
        *current = Some(value);
        Ok(())
    })
    .await
}

/// Mutate a team's complete JSON under its cross-process lock. Missing files
/// are `None`; malformed files fail instead of being silently overwritten.
/// All lifecycle writers use this transaction so member additions, removals
/// and metadata updates cannot lose each other's changes.
pub async fn update_team_file<R>(
    home: &Path,
    name: &str,
    mutate: impl FnOnce(&mut Option<serde_json::Value>) -> std::io::Result<R>,
) -> std::io::Result<R> {
    let path = team_file_path(home, name);
    std::fs::create_dir_all(team_dir(home, name))?;
    let _lock = task_store::proper_lockfile::lock(&path).await?;
    let mut current = match std::fs::read(&path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(std::io::Error::other)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let original = current.clone();
    let result = mutate(&mut current)?;
    if current != original {
        if let Some(value) = current {
            atomic_write(&path, &value)?;
        }
    }
    Ok(result)
}

fn atomic_write(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_file_name(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Read + parse a team's `config.json` (`readTeamFile`, `teamHelpers.ts`).
///
/// # Errors
/// Returns the underlying `std::io::Error` on a read failure, or an
/// `InvalidData` error when the file is not valid `TeamFile` JSON.
pub fn read_team_file(home: &Path, name: &str) -> std::io::Result<TeamFile> {
    let content = std::fs::read_to_string(team_file_path(home, name))?;
    serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Remove one member from a team's `config.json`, matching by agent id OR
/// display name — the port of the oracle's `jqt(teamName, {agentId, name})`
/// (2.1.223 shutdown paths: `Urv` @261175890 and the print.ts
/// `shutdown_approved` handler @262044791, both of which remove the member
/// from the team file BEFORE unassigning its tasks).
///
/// Returns `true` when a member was removed and the file written back.
///
/// # Errors
/// Propagates read/parse/write failures from the underlying file IO.
pub async fn remove_team_member(
    home: &Path,
    team_name: &str,
    agent_id: &str,
    member_name: &str,
) -> std::io::Result<bool> {
    update_team_file(home, team_name, |current| {
        let Some(members) = current
            .as_mut()
            .and_then(|value| value.get_mut("members"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            return Ok(false);
        };
        let before = members.len();
        members.retain(|member| {
            member["agentId"].as_str() != Some(agent_id)
                && member["name"].as_str() != Some(member_name)
        });
        Ok(members.len() != before)
    })
    .await
}

/// Persist the Native team member `isActive` transition for the first member
/// whose display `name` matches exactly. The complete config is changed under
/// the same cross-process lock used by membership writers, and the JSON-level
/// update preserves every field this crate does not model.
///
/// Returns `true` only when a matching member's value changed. A missing team,
/// malformed member row, missing member, or an already-equal value is a no-op.
pub async fn set_team_member_active(
    home: &Path,
    team_name: &str,
    member_name: &str,
    active: bool,
) -> std::io::Result<bool> {
    // Unlike team creation, an activity notification must never materialize a
    // missing team directory or config file.
    match std::fs::metadata(team_file_path(home, team_name)) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    update_team_file(home, team_name, |current| {
        let Some(members) = current
            .as_mut()
            .and_then(|value| value.get_mut("members"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            return Ok(false);
        };
        let Some(member) = members.iter_mut().find(|member| {
            member.get("name").and_then(serde_json::Value::as_str) == Some(member_name)
        }) else {
            return Ok(false);
        };
        let Some(object) = member.as_object_mut() else {
            return Ok(false);
        };
        if object.get("isActive").and_then(serde_json::Value::as_bool) == Some(active) {
            return Ok(false);
        }
        object.insert("isActive".into(), active.into());
        Ok(true)
    })
    .await
}

/// Unix-millisecond timestamp (TS `Date.now()`).
#[must_use]
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::team_registry::TeamMemberActivityHandle as _;

    #[test]
    fn sanitize_matches_ts() {
        assert_eq!(sanitize_name("Alpha Team"), "alpha-team");
        assert_eq!(sanitize_name("My_Team!"), "my-team-");
        assert_eq!(sanitize_name("ABC123"), "abc123");
        assert_eq!(sanitize_name("a/b\\c"), "a-b-c");
        // JavaScript's global regex replaces UTF-16 code units, so an astral
        // character contributes two replacements rather than one scalar.
        assert_eq!(sanitize_name("x😀y"), "x--y");
    }

    #[test]
    fn paths_compose() {
        let home = PathBuf::from("/home/u/.lingxi");
        assert_eq!(
            team_file_path(&home, "Alpha Team"),
            PathBuf::from("/home/u/.lingxi/teams/alpha-team/config.json")
        );
        assert_eq!(
            task_dir(&home, "Alpha Team"),
            PathBuf::from("/home/u/.lingxi/tasks/alpha-team")
        );
    }

    #[tokio::test]
    async fn team_file_write_normalizes_decomposed_home_and_supplementary_team_name() {
        let tmp = tempfile::tempdir().unwrap();
        let decomposed_home = tmp.path().join("config-cafe\u{301}");
        let composed_home = tmp.path().join("config-café");
        let team_name = "Team😀";
        let expected_path = composed_home
            .join("teams")
            .join("team--")
            .join("config.json");

        assert_eq!(team_file_path(&decomposed_home, team_name), expected_path);
        assert_eq!(team_file_path(&composed_home, team_name), expected_path);

        let file = TeamFile {
            name: team_name.into(),
            description: None,
            created_at: 1,
            lead_agent_id: "team-lead@team".into(),
            lead_session_id: None,
            members: Vec::new(),
        };
        write_team_file(&decomposed_home, team_name, &file)
            .await
            .unwrap();

        assert!(expected_path.is_file());
        assert_eq!(
            read_team_file(&composed_home, team_name).unwrap().name,
            team_name
        );
    }

    #[tokio::test]
    async fn write_persists_team_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".lingxi");
        assert!(!team_file_exists(&home, "alpha"));

        let file = TeamFile {
            name: "alpha".into(),
            description: Some("do work".into()),
            created_at: 123,
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: Some("sess-1".into()),
            members: vec![TeamMember {
                agent_id: "team-lead@alpha".into(),
                name: "team-lead".into(),
                agent_type: Some("team-lead".into()),
                model: Some("claude-x".into()),
                model_profile: None,
                joined_at: 123,
                tmux_pane_id: String::new(),
                cwd: "/work".into(),
                subscriptions: vec![],
            }],
        };
        write_team_file(&home, "alpha", &file).await.unwrap();
        assert!(team_file_exists(&home, "alpha"));

        // The written JSON uses the TS field names + shape.
        let raw = std::fs::read_to_string(team_file_path(&home, "alpha")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["name"], "alpha");
        assert_eq!(v["createdAt"], 123);
        assert_eq!(v["leadAgentId"], "team-lead@alpha");
        assert_eq!(v["leadSessionId"], "sess-1");
        assert_eq!(v["members"][0]["agentId"], "team-lead@alpha");
        assert_eq!(v["members"][0]["name"], "team-lead");
        assert_eq!(v["members"][0]["agentType"], "team-lead");
        assert_eq!(v["members"][0]["joinedAt"], 123);
        assert_eq!(v["members"][0]["subscriptions"], serde_json::json!([]));
    }
    #[tokio::test]
    async fn removing_a_member_preserves_current_backend_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let path = team_file_path(tmp.path(), "session-12345678");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({"name":"session-12345678","members":[{"agentId":"team-lead@session-12345678","name":"team-lead","backendType":"in-process","color":"red"},{"agentId":"worker@session-12345678","name":"worker","backendType":"tmux"}]});
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(remove_team_member(
            tmp.path(),
            "session-12345678",
            "worker@session-12345678",
            "worker"
        )
        .await
        .unwrap());
        let remaining: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(remaining["members"].as_array().unwrap().len(), 1);
        assert_eq!(remaining["members"][0]["backendType"], "in-process");
        assert_eq!(remaining["members"][0]["color"], "red");
    }

    #[tokio::test]
    async fn active_transition_updates_exact_named_row_and_preserves_unmodeled_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = team_file_path(tmp.path(), "session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "name": "session",
                "futureTeamField": {"preserve": true},
                "members": [
                    {"agentId":"first@session", "name":"first", "isActive":true, "futureMemberField":[1,2]},
                    {"agentId":"second@session", "name":"second", "futureMemberField":"untouched"},
                    {"agentId":"duplicate@session", "name":"second", "isActive":true}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        TeamFileMemberActivity::new(tmp.path())
            .set_active("session", "second", false)
            .await
            .unwrap();
        assert!(
            !set_team_member_active(tmp.path(), "session", "second", false)
                .await
                .unwrap()
        );
        assert!(
            !set_team_member_active(tmp.path(), "session", "missing", true)
                .await
                .unwrap()
        );

        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["futureTeamField"]["preserve"], true);
        assert_eq!(value["members"][0]["isActive"], true);
        assert_eq!(
            value["members"][0]["futureMemberField"],
            serde_json::json!([1, 2])
        );
        assert_eq!(value["members"][1]["isActive"], false);
        assert_eq!(value["members"][1]["futureMemberField"], "untouched");
        assert_eq!(value["members"][2]["isActive"], true);
    }

    #[tokio::test]
    async fn activity_update_does_not_create_missing_team_storage() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!team_dir(tmp.path(), "missing").exists());
        assert!(
            !set_team_member_active(tmp.path(), "missing", "member", true)
                .await
                .unwrap()
        );
        assert!(!team_dir(tmp.path(), "missing").exists());
    }
    #[tokio::test]
    async fn transaction_locks_before_reading_the_latest_config() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        update_team_file(&home, "team", |value| {
            *value = Some(serde_json::json!({"members":[],"generation":1}));
            Ok(())
        })
        .await
        .unwrap();
        let path = team_file_path(&home, "team");
        let guard = task_store::proper_lockfile::lock(&path).await.unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let observed = entered.clone();
        let worker_home = home.clone();
        let writer = tokio::spawn(async move {
            update_team_file(&worker_home, "team", |value| {
                observed.store(true, Ordering::SeqCst);
                value.as_mut().unwrap()["mutated"] = true.into();
                Ok(())
            })
            .await
            .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!entered.load(Ordering::SeqCst));
        // Simulate the current lock owner publishing a newer config.
        atomic_write(&path, &serde_json::json!({"members":[],"generation":2})).unwrap();
        drop(guard);
        writer.await.unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["generation"], 2);
        assert_eq!(value["mutated"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_departures_and_spawns_preserve_every_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let mut members =
            vec![serde_json::json!({"agentId":"lead","name":"lead","backendType":"in-process"})];
        for index in 0..12 {
            members.push(
                serde_json::json!({"agentId":format!("old-{index}"),"name":format!("old-{index}")}),
            );
        }
        update_team_file(&home, "team", |value| {
            *value =
                Some(serde_json::json!({"members":members,"futureMetadata":{"preserve":true}}));
            Ok(())
        })
        .await
        .unwrap();
        let mut jobs = Vec::new();
        for index in 0..12 {
            let remove_home = home.clone();
            jobs.push(tokio::spawn(async move {
                let name = format!("old-{index}");
                assert!(remove_team_member(&remove_home, "team", &name, &name)
                    .await
                    .unwrap());
            }));
            let spawn_home = home.clone();
            jobs.push(tokio::spawn(async move {
                update_team_file(&spawn_home, "team", |value| {
                    value.as_mut().unwrap()["members"].as_array_mut().unwrap()
                        .push(serde_json::json!({"agentId":format!("new-{index}"),"name":format!("new-{index}"),"backendType":"tmux"}));
                    Ok(())
                }).await.unwrap();
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }
        let path = team_file_path(&home, "team");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let members = value["members"].as_array().unwrap();
        assert_eq!(members.len(), 13);
        assert_eq!(value["futureMetadata"]["preserve"], true);
        for index in 0..12 {
            assert!(members
                .iter()
                .any(|member| member["name"] == format!("new-{index}")));
            assert!(!members
                .iter()
                .any(|member| member["name"] == format!("old-{index}")));
        }
        let files: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("config.json")]);
    }
}
