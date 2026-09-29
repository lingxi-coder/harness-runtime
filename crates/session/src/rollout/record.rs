//! On-disk rollout record format — faithful port of codex's `RolloutLine`,
//! `RolloutItem`, `SessionMeta`, and `SessionMetaLine`.
//!
//! The wire shape is byte-compatible with codex so existing rollout files
//! parse and resume. Each line is a [`RolloutLine`]: a `timestamp` field plus
//! the flattened [`RolloutItem`], which serializes as
//! `{ "type": <variant>, "payload": <body> }` (serde `tag`/`content`).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use uuid::Uuid;

/// Plain-UUID thread identifier.
///
/// Unlike `lingxi_core::types::SessionId` (which serializes with a `"sess:"` prefix),
/// `ThreadId` serializes as a bare UUID string — matching codex's on-disk
/// rollout bytes and the `rollout-<ts>-<uuid>.jsonl` filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(Uuid);

impl ThreadId {
    /// Generate a fresh random thread id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct from a raw UUID.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// Parse a bare-UUID string into a thread id.
    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(s).map(Self)
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ThreadId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_string(s)
    }
}

/// Session-id alias used in [`SessionMeta`]. Codex distinguishes the two but
/// they share the same wire representation (bare UUID); LingXi keeps the
/// distinction at the type level by re-using [`ThreadId`].
pub type SessionId = ThreadId;

/// How a session was initiated. Faithful subset of codex's `SessionSource`
/// (the codex-internal/sub-agent arms collapse into [`SessionSource::Custom`]
/// / [`SessionSource::Unknown`] which is all the recorder needs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    Cli,
    #[default]
    VSCode,
    Exec,
    Mcp,
    Custom(String),
    #[serde(other)]
    Unknown,
}

/// Git provenance recorded on the session-meta line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GitInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
}

/// Session-level metadata that does not belong to any single turn.
///
/// Field set and serde attributes mirror codex's `SessionMeta` so existing
/// rollout headers deserialize unchanged. Codex-only structured fields
/// (`base_instructions`, `dynamic_tools`, `context_window`, …) are carried as
/// opaque [`Value`]s so the recorder neither loses nor reinterprets them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<ThreadId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    pub timestamp: String,
    pub cwd: PathBuf,
    pub originator: String,
    pub cli_version: String,
    #[serde(default)]
    pub source: SessionSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_nickname: Option<String>,
    #[serde(default, alias = "agent_type", skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mode: Option<String>,
    /// Initial context-window identity. Codex models this as a struct; here it
    /// is the opaque object so the `window_id` round-trips byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<Value>,
    /// Any other codex-side `SessionMeta` fields (`base_instructions`,
    /// `dynamic_tools`, `thread_source`, `multi_agent_version`, …) preserved
    /// verbatim across read→write.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Default for SessionMeta {
    fn default() -> Self {
        let id = ThreadId::default();
        Self {
            session_id: id,
            id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: String::new(),
            cwd: PathBuf::new(),
            originator: String::new(),
            cli_version: String::new(),
            source: SessionSource::default(),
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            model_provider: None,
            memory_mode: None,
            context_window: None,
            extra: serde_json::Map::new(),
        }
    }
}

/// A [`SessionMeta`] plus optional git provenance — the first line of every
/// rollout. The `Deserialize` impl defaults a missing `session_id` from `id`
/// so legacy headers (codex pre-`session_id`) still load (matches codex).
#[derive(Debug, Clone, Serialize)]
pub struct SessionMetaLine {
    #[serde(flatten)]
    pub meta: SessionMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
}

impl<'de> Deserialize<'de> for SessionMetaLine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;

        #[derive(Deserialize)]
        struct Fields {
            #[serde(flatten)]
            meta: SessionMeta,
            git: Option<GitInfo>,
        }

        let mut value = Value::deserialize(deserializer)?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("session metadata must be an object"))?;
        if !fields.contains_key("session_id") {
            let thread_id = fields
                .get("id")
                .cloned()
                .ok_or_else(|| D::Error::missing_field("id"))?;
            fields.insert("session_id".to_string(), thread_id);
        }
        // `git` is captured by both the explicit field and the `extra` flatten
        // map; drop it from the value before re-deserializing so it does not
        // also land in `meta.extra`.
        let git_value = fields.remove("git");
        let Fields { mut meta, .. } = serde_json::from_value(value).map_err(D::Error::custom)?;
        meta.extra.remove("git");
        let git = match git_value {
            Some(Value::Null) | None => None,
            Some(other) => Some(serde_json::from_value(other).map_err(D::Error::custom)?),
        };
        Ok(Self { meta, git })
    }
}

/// One canonical rollout item. The tag/content envelope
/// (`{ "type": …, "payload": … }`) is byte-compatible with codex.
///
/// `ResponseItem`, `TurnContext`, `EventMsg`, and `InterAgentCommunication`
/// carry opaque [`Value`] payloads (see module docs) — the recorder persists
/// and replays them verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum RolloutItem {
    SessionMeta(SessionMetaLine),
    ResponseItem(Value),
    InterAgentCommunication(Value),
    Compacted(CompactedItem),
    TurnContext(Value),
    EventMsg(Value),
}

/// Compaction marker — keeps the structured `replacement_history` so the
/// legacy ghost-snapshot stripping in the loader can prune it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactedItem {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_history: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// One physical JSONL line: `timestamp` + flattened [`RolloutItem`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutLine {
    pub timestamp: String,
    #[serde(flatten)]
    pub item: RolloutItem,
}
