//! Per-teammate plan review dispatch, independent of the leader's permission state.
use crate::types::AgentId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanApprovalResponse {
    pub request_id: String,
    pub approved: bool,
    #[serde(default)]
    pub feedback: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
}

#[async_trait]
pub trait TeammatePlanRequester: Send + Sync {
    /// Engine-selected plan file, never supplied by a model tool argument.
    fn writable_plan_path(&self) -> Option<&str> {
        None
    }

    async fn submit(&self, input: Value) -> Result<Value, String>;
}

type Requesters = HashMap<AgentId, Weak<dyn TeammatePlanRequester>>;
fn requesters() -> &'static Mutex<Requesters> {
    static REQUESTERS: OnceLock<Mutex<Requesters>> = OnceLock::new();
    REQUESTERS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub fn register(agent_id: AgentId, requester: &Arc<dyn TeammatePlanRequester>) {
    let mut entries = requesters().lock().unwrap_or_else(|e| e.into_inner());
    entries.retain(|_, value| value.strong_count() > 0);
    entries.insert(agent_id, Arc::downgrade(requester));
}
pub fn requester(agent_id: &AgentId) -> Option<Arc<dyn TeammatePlanRequester>> {
    requesters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(agent_id)
        .and_then(Weak::upgrade)
}

/// Call-local root for exactly the registered teammate plan file. Symlinks,
/// nonregular files, and hard links never receive this allowance.
pub fn own_plan_file_root(
    agent_id: Option<&AgentId>,
    path: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let requester = requester(agent_id?)?;
    let trusted = std::path::Path::new(requester.writable_plan_path()?);
    if !path.is_absolute() || path != trusted {
        return None;
    }
    let parent = trusted.parent()?;
    if std::fs::canonicalize(parent).ok()?.as_path() != parent {
        return None;
    }
    match std::fs::symlink_metadata(trusted) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return None;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    Some(parent.to_path_buf())
}

#[cfg(test)]
mod file_scope_tests {
    use super::*;
    struct Requester(String);
    #[async_trait]
    impl TeammatePlanRequester for Requester {
        fn writable_plan_path(&self) -> Option<&str> {
            Some(&self.0)
        }
        async fn submit(&self, _: Value) -> Result<Value, String> {
            unreachable!()
        }
    }
    #[test]
    fn own_plan_file_scope_rejects_other_agents_siblings_and_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("plan.md");
        let id = AgentId::new();
        let owner: Arc<dyn TeammatePlanRequester> =
            Arc::new(Requester(path.to_string_lossy().into_owned()));
        register(id, &owner);
        assert_eq!(own_plan_file_root(Some(&id), &path), Some(root.clone()));
        assert!(own_plan_file_root(Some(&AgentId::new()), &path).is_none());
        assert!(own_plan_file_root(Some(&id), &root.join("other.md")).is_none());
        #[cfg(unix)]
        {
            let outside = root.join("outside.md");
            std::fs::write(&outside, "protected").unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            assert!(own_plan_file_root(Some(&id), &path).is_none());
            std::fs::remove_file(&path).unwrap();
            std::fs::hard_link(&outside, &path).unwrap();
            assert!(own_plan_file_root(Some(&id), &path).is_none());
            assert_eq!(std::fs::read_to_string(outside).unwrap(), "protected");
        }
    }
}
