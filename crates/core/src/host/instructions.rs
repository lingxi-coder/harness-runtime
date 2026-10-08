//! Host-owned instruction loading shared by the main loop and child runners.

use crate::types::{AgentId, SessionId};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files eligible for an instruction-context load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstructionScope {
    Full,
    ManagedOnly,
}

/// A context identity inside one explicitly owned instruction root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstructionContextKey {
    /// Main conversation owning this context.
    pub session_id: SessionId,
    /// A child context has a separate identity under the same root.
    pub agent_id: Option<AgentId>,
}

/// Native causes which invalidate an existing user-context Promise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionRefreshReason {
    #[default]
    SessionStart,
    Compaction,
    PolicyRefresh,
    DirectoryAdded,
    SettingsSync,
    AccountChange,
    HooksInvalidate,
    PolicyVerdict,
    MemoryPaused,
    MemoryResumed,
    AutoMemoryOff,
    AutoMemoryBackOn,
}

impl InstructionRefreshReason {
    /// Exact attachment reason bytes emitted by the current native contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::Compaction => "compaction",
            Self::PolicyRefresh => "policy_refresh",
            Self::DirectoryAdded => "directory_added",
            Self::SettingsSync => "settings_sync",
            Self::AccountChange => "account_change",
            Self::HooksInvalidate => "hooks_invalidate",
            Self::PolicyVerdict => "policy_verdict",
            Self::MemoryPaused => "memory_paused",
            Self::MemoryResumed => "memory_resumed",
            Self::AutoMemoryOff => "auto_memory_off",
            Self::AutoMemoryBackOn => "auto_memory_back_on",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionFileType {
    Managed,
    User,
    Project,
    Local,
    #[serde(alias = "AutoMemPinned")]
    AutoMem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionFile {
    pub path: String,
    #[serde(
        rename = "type",
        default = "user_instruction_type",
        deserialize_with = "instruction_type"
    )]
    pub kind: InstructionFileType,
    pub content: String,
}

fn user_instruction_type() -> InstructionFileType {
    InstructionFileType::User
}

fn instruction_type<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<InstructionFileType, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str() {
        Some("Managed") => InstructionFileType::Managed,
        Some("Project") => InstructionFileType::Project,
        Some("Local") => InstructionFileType::Local,
        Some("AutoMem" | "AutoMemPinned") => InstructionFileType::AutoMem,
        _ => InstructionFileType::User,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstructionRendering {
    #[default]
    Announced,
    Inline,
}

/// Frozen user context and lazy-instruction cursor inherited by forks.
/// This is host metadata, never parsed from model-authored tool input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionContext {
    /// None means descriptors are unavailable; Some(empty) is a successful
    /// load with no eligible instruction files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_instructions: Option<Vec<InstructionFile>>,
    /// Live routing from the parent prompt snapshot; never a Gv disk cache.
    #[serde(skip)]
    pub rendering: InstructionRendering,
    /// Actual typed attachment payloads in visible history order. Cold restore
    /// recovers these from existing attachment rows, never from reminder prose.
    #[serde(skip)]
    pub announcement_history: Vec<serde_json::Value>,
    #[serde(default)]
    pub user_context: BTreeMap<String, String>,
    /// JavaScript insertion order of the inherited userContext object.
    #[serde(default)]
    pub user_context_order: Vec<String>,
    #[serde(default)]
    pub managed_instructions_only: bool,
    #[serde(default)]
    pub sent_paths: HashSet<PathBuf>,
    #[serde(default)]
    pub project_instruction_bodies: HashSet<String>,
    #[serde(default)]
    pub instructions_root: Option<PathBuf>,
}

impl InstructionContext {
    /// Inline runQuery userContext envelope; normal context uses attachments.
    #[must_use]
    pub fn reminder(&self) -> Option<String> {
        let mut keys = self.user_context_order.clone();
        for key in ["Environment", "instructions", "userEmail", "currentDate"] {
            if !keys.iter().any(|existing| existing == key) {
                keys.push(key.into());
            }
        }
        for key in self.user_context.keys() {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        let entries: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                self.user_context
                    .get(key)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("# {key}\n{value}"))
            })
            .collect();
        (!entries.is_empty()).then(|| {
            let body = entries.join("\n");
            format!("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n{body}\n\n      {} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.\n</system-reminder>\n", branding::PRODUCT_NAME)
        })
    }
}

/// Distinct instruction producers attached to a successful Read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstructionReadContext {
    /// Legacy nested-memory messages, already rendered for the model.
    pub legacy_reminders: Vec<String>,
    /// AGENTS plugin frames rendered as one tool.call hook context message.
    pub agents_context: Vec<String>,
}

impl InstructionReadContext {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.legacy_reminders.is_empty() && self.agents_context.is_empty()
    }
}

/// Narrow engine boundary: implementations belong to the composition layer.
#[async_trait]
pub trait InstructionContextProvider: Send + Sync {
    /// Failure differs from an empty successful load: omission keeps the
    /// original context when managed-policy retrieval fails.
    async fn load(&self, cwd: &Path, scope: InstructionScope)
        -> Result<InstructionContext, String>;

    /// Attach instructions only after a successful Read. `is_partial` means
    /// offset or limit was explicitly supplied, matching the native self-read
    /// cursor even when the tool truncated an otherwise omitted-range read.
    async fn after_read(
        &self,
        _cwd: &Path,
        _path: &Path,
        _is_partial: bool,
        _context: &mut InstructionContext,
    ) -> InstructionReadContext {
        InstructionReadContext::default()
    }
}

/// Adapt guest-visible tool paths to the host's instruction filesystem.
#[must_use]
pub fn with_path_resolver(
    provider: Arc<dyn InstructionContextProvider>,
    resolver: Arc<dyn Fn(&Path) -> PathBuf + Send + Sync>,
) -> Arc<dyn InstructionContextProvider> {
    Arc::new(MappedInstructionProvider { provider, resolver })
}

struct MappedInstructionProvider {
    provider: Arc<dyn InstructionContextProvider>,
    resolver: Arc<dyn Fn(&Path) -> PathBuf + Send + Sync>,
}

#[async_trait]
impl InstructionContextProvider for MappedInstructionProvider {
    async fn load(
        &self,
        cwd: &Path,
        scope: InstructionScope,
    ) -> Result<InstructionContext, String> {
        self.provider.load(&(self.resolver)(cwd), scope).await
    }

    async fn after_read(
        &self,
        cwd: &Path,
        path: &Path,
        is_partial: bool,
        context: &mut InstructionContext,
    ) -> InstructionReadContext {
        self.provider
            .after_read(
                &(self.resolver)(cwd),
                &(self.resolver)(path),
                is_partial,
                context,
            )
            .await
    }
}
