//! Host-owned persistence for Local App Agent sessions and prompt profiles.
//!
//! The page is never the authority for these records.  A generated app can
//! request a session operation, but the host owns the JSON files, validates
//! app ownership, and applies profile revisions only after user approval.

use crate::error::AppError;
use crate::manifest::AppLayout;
use crate::runtime_v2::{AgentSessionRecord, AppAgentProfile, RUNTIME_CONTRACT_SCHEMA_VERSION};
use lingxi_core::host::rooted_fs::{self, AtomicWriteOptions};
use lingxi_core::host::FsError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum persisted Agent session catalog size per app.
pub const MAX_AGENT_SESSION_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum persisted App Agent Profile size.
pub const MAX_AGENT_PROFILE_BYTES: u64 = 64 * 1024;
/// Maximum persisted transcript size for one app Agent session.
pub const MAX_AGENT_SESSION_HISTORY_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum number of messages restored into one app Agent session.
pub const MAX_AGENT_SESSION_HISTORY_MESSAGES: usize = 1024;

pub(crate) const AGENT_SESSION_CATALOG_FILE: &str = "agent-sessions.json";
pub(crate) const AGENT_PROFILE_FILE: &str = "agent-profile.json";
pub(crate) const AGENT_SESSION_HISTORY_DIR: &str = "agent-session-history";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionHistory {
    schema_version: u32,
    messages: Vec<serde_json::Value>,
}

/// Validate a session id before using it as a filename component.
pub(crate) fn validate_agent_session_id(session_id: &str) -> Result<(), AppError> {
    let bytes = session_id.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 128
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(AppError::InvalidRequest(format!(
            "invalid Agent session id {session_id:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionCatalog {
    schema_version: u32,
    sessions: Vec<AgentSessionRecord>,
}

/// Load the host-owned session catalog, returning an empty catalog on first
/// use.  Records are rejected if they do not belong to `layout.app_id`.
pub fn load_sessions(layout: &AppLayout) -> Result<Vec<AgentSessionRecord>, AppError> {
    let path = layout.agent_sessions_rel();
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &path,
        MAX_AGENT_SESSION_CATALOG_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(AppError::from_fs("read Agent session catalog", &error)),
    };
    let catalog: AgentSessionCatalog = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("Agent session catalog: {error}")))?;
    if catalog.schema_version != RUNTIME_CONTRACT_SCHEMA_VERSION {
        return Err(AppError::StorageCorrupt(format!(
            "Agent session catalog schemaVersion {} is unsupported",
            catalog.schema_version
        )));
    }
    validate_sessions(layout, &catalog.sessions)?;
    Ok(catalog.sessions)
}

/// Atomically persist the app's Agent session catalog.
pub fn save_sessions(layout: &AppLayout, sessions: &[AgentSessionRecord]) -> Result<(), AppError> {
    validate_sessions(layout, sessions)?;
    let mut body = serde_json::to_vec_pretty(&AgentSessionCatalog {
        schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
        sessions: sessions.to_vec(),
    })
    .map_err(|error| AppError::Io(format!("serialize Agent session catalog: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_AGENT_SESSION_CATALOG_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "Agent session catalog exceeds {MAX_AGENT_SESSION_CATALOG_BYTES} bytes"
        )));
    }
    layout.initialize()?;
    rooted_fs::atomic_write(
        layout.root(),
        &layout.agent_sessions_rel(),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write Agent session catalog", &error))
}

/// Load the durable conversation history for one app Agent session.
pub fn load_agent_history(
    layout: &AppLayout,
    session_id: &str,
) -> Result<Vec<serde_json::Value>, AppError> {
    let path = layout.agent_session_history_rel(session_id)?;
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &path,
        MAX_AGENT_SESSION_HISTORY_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(AppError::from_fs("read Agent session history", &error)),
    };
    let history: AgentSessionHistory = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("Agent session history: {error}")))?;
    if history.schema_version != RUNTIME_CONTRACT_SCHEMA_VERSION {
        return Err(AppError::StorageCorrupt(format!(
            "Agent session history schemaVersion {} is unsupported",
            history.schema_version
        )));
    }
    validate_agent_history(&history.messages)?;
    Ok(history.messages)
}

/// Atomically persist one app Agent session's bounded conversation history.
pub fn save_agent_history(
    layout: &AppLayout,
    session_id: &str,
    messages: &[serde_json::Value],
) -> Result<(), AppError> {
    validate_agent_history(messages)?;
    let mut body = serde_json::to_vec_pretty(&AgentSessionHistory {
        schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
        messages: messages.to_vec(),
    })
    .map_err(|error| AppError::Io(format!("serialize Agent session history: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_AGENT_SESSION_HISTORY_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "Agent session history exceeds {MAX_AGENT_SESSION_HISTORY_BYTES} bytes"
        )));
    }
    layout.initialize()?;
    let path = layout.agent_session_history_rel(session_id)?;
    rooted_fs::atomic_write(layout.root(), &path, &body, AtomicWriteOptions::default())
        .map_err(|error| AppError::from_fs("write Agent session history", &error))
}

/// Insert or replace one host-owned session by id.
pub fn upsert_session(layout: &AppLayout, session: AgentSessionRecord) -> Result<(), AppError> {
    if session.app_id != layout.app_id() {
        return Err(AppError::InvalidRequest(
            "Agent session app ownership does not match the app layout".into(),
        ));
    }
    let mut sessions = load_sessions(layout)?;
    if let Some(existing) = sessions
        .iter_mut()
        .find(|existing| existing.session_id == session.session_id)
    {
        *existing = session;
    } else {
        sessions.push(session);
    }
    sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    save_sessions(layout, &sessions)
}

/// Load an app's approved profile.  A missing profile is the empty revision 0
/// profile, not an implicit instruction source.
pub fn load_profile(layout: &AppLayout) -> Result<AppAgentProfile, AppError> {
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &layout.agent_profile_rel(),
        MAX_AGENT_PROFILE_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(AppAgentProfile::empty(layout.app_id(), 0)),
        Err(error) => return Err(AppError::from_fs("read App Agent Profile", &error)),
    };
    let profile: AppAgentProfile = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("App Agent Profile: {error}")))?;
    profile
        .validate()
        .map_err(|error| AppError::StorageCorrupt(error.to_string()))?;
    if profile.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(
            "App Agent Profile app ownership does not match the app layout".into(),
        ));
    }
    Ok(profile)
}

/// Atomically persist an approved App Agent Profile.
pub fn save_profile(layout: &AppLayout, profile: &AppAgentProfile) -> Result<(), AppError> {
    if profile.app_id != layout.app_id() {
        return Err(AppError::InvalidRequest(
            "App Agent Profile app ownership does not match the app layout".into(),
        ));
    }
    profile
        .validate()
        .map_err(|error| AppError::InvalidRequest(error.to_string()))?;
    let mut body = serde_json::to_vec_pretty(profile)
        .map_err(|error| AppError::Io(format!("serialize App Agent Profile: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_AGENT_PROFILE_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "App Agent Profile exceeds {MAX_AGENT_PROFILE_BYTES} bytes"
        )));
    }
    layout.initialize()?;
    rooted_fs::atomic_write(
        layout.root(),
        &layout.agent_profile_rel(),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write App Agent Profile", &error))
}

fn validate_sessions(layout: &AppLayout, sessions: &[AgentSessionRecord]) -> Result<(), AppError> {
    let mut ids = BTreeMap::new();
    for session in sessions {
        session
            .validate()
            .map_err(|error| AppError::StorageCorrupt(error.to_string()))?;
        if session.app_id != layout.app_id() {
            return Err(AppError::StorageCorrupt(
                "Agent session app ownership does not match the app layout".into(),
            ));
        }
        if ids.insert(session.session_id.as_str(), ()).is_some() {
            return Err(AppError::StorageCorrupt(
                "Agent session catalog contains duplicate session ids".into(),
            ));
        }
    }
    Ok(())
}

fn validate_agent_history(messages: &[serde_json::Value]) -> Result<(), AppError> {
    if messages.len() > MAX_AGENT_SESSION_HISTORY_MESSAGES {
        return Err(AppError::StorageCorrupt(format!(
            "Agent session history contains too many messages ({} > {})",
            messages.len(),
            MAX_AGENT_SESSION_HISTORY_MESSAGES
        )));
    }
    if messages.iter().any(|message| !message.is_object()) {
        return Err(AppError::StorageCorrupt(
            "Agent session history contains a non-object message".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FixedClock;
    use crate::{AppService, NoopAppEventObserver};
    use tempfile::tempdir;

    #[tokio::test]
    async fn sessions_and_approved_profile_round_trip_under_app_layout() {
        let root = tempdir().unwrap();
        let service = AppService::load(
            root.path(),
            std::sync::Arc::new(FixedClock::new(1)),
            std::sync::Arc::new(NoopAppEventObserver),
        )
        .await
        .unwrap();
        let record = service
            .create_app(Some("Notes"), "notes", None)
            .await
            .unwrap();
        let layout = AppLayout::new(root.path(), record.id.clone()).unwrap();
        let session = AgentSessionRecord {
            schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
            session_id: "session-1".into(),
            app_id: record.id.clone(),
            app_instance_id: "instance-1".into(),
            status: crate::AgentSessionStatus::Active,
            prompt_profile_revision: 0,
            budget: crate::AgentBudget::default(),
            turn_count: 0,
            output_tokens_used: 0,
            bridge_calls_used: 0,
            mcp_calls_used: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        upsert_session(&layout, session.clone()).unwrap();
        assert_eq!(load_sessions(&layout).unwrap(), vec![session]);
        let profile = AppAgentProfile::empty(record.id, 1);
        save_profile(&layout, &profile).unwrap();
        assert_eq!(load_profile(&layout).unwrap(), profile);

        let history = vec![
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "hi"}]}),
        ];
        save_agent_history(&layout, "session-1", &history).unwrap();
        assert_eq!(load_agent_history(&layout, "session-1").unwrap(), history);
    }

    #[tokio::test]
    async fn foreign_session_is_rejected_before_persistence() {
        let root = tempdir().unwrap();
        let service = AppService::load(
            root.path(),
            std::sync::Arc::new(FixedClock::new(1)),
            std::sync::Arc::new(NoopAppEventObserver),
        )
        .await
        .unwrap();
        let record = service
            .create_app(Some("Notes"), "notes", None)
            .await
            .unwrap();
        let layout = AppLayout::new(root.path(), record.id).unwrap();
        let mut session = AgentSessionRecord {
            schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
            session_id: "session-1".into(),
            app_id: "other123".into(),
            app_instance_id: "instance-1".into(),
            status: crate::AgentSessionStatus::Active,
            prompt_profile_revision: 0,
            budget: crate::AgentBudget::default(),
            turn_count: 0,
            output_tokens_used: 0,
            bridge_calls_used: 0,
            mcp_calls_used: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        assert!(upsert_session(&layout, session.clone()).is_err());
        session.app_id = layout.app_id().into();
        session.budget.max_recursion_depth = 0;
        assert!(upsert_session(&layout, session).is_err());
    }
}
