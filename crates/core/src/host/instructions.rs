//! Host-owned instruction loading shared by the main loop and child runners.

use crate::types::{AgentId, SessionId};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
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
    /// Exact JavaScript path/name source. Only genuine filesystem operations
    /// may request a strict UTF-16-to-OS-string conversion.
    #[serde(
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize",
        deserialize_with = "deserialize_instruction_string"
    )]
    pub path: crate::types::utf16_json::Utf16JsonProjection,
    #[serde(
        rename = "type",
        default = "user_instruction_type",
        deserialize_with = "instruction_type"
    )]
    pub kind: InstructionFileType,
    /// Exact JavaScript string source. The serde adapter preserves the public
    /// JSON field as a string instead of exposing the projection's flat arena.
    #[serde(
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize",
        deserialize_with = "deserialize_instruction_string"
    )]
    pub content: crate::types::utf16_json::Utf16JsonProjection,
}

impl InstructionFile {
    /// Build one descriptor from ordinary Rust text or an exact source string.
    #[must_use]
    pub fn new(
        path: impl Into<crate::types::utf16_json::Utf16JsonProjection>,
        kind: InstructionFileType,
        content: impl Into<crate::types::utf16_json::Utf16JsonProjection>,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            content: content.into(),
        }
    }

    /// Return the descriptor's exact content code units.
    #[must_use]
    pub fn content_units(&self) -> Vec<u16> {
        self.content
            .string_units("")
            .expect("instruction file content is a root string")
    }

    /// Return the exact path/name code units.
    #[must_use]
    pub fn path_units(&self) -> Vec<u16> {
        self.path
            .string_units("")
            .expect("instruction file path is a root string")
    }

    /// Convert the path only when it is a valid Rust Unicode string. Callers
    /// must use this only when a text path is required.
    #[must_use]
    pub fn path_text(&self) -> Option<String> {
        String::from_utf16(&self.path_units()).ok()
    }

    /// Convert the source path to the host OS path representation without
    /// losing lone UTF-16 units on Windows. Call this only at a real file
    /// system boundary; virtual instruction names remain source projections.
    #[must_use]
    pub fn filesystem_path(&self) -> Option<PathBuf> {
        let units = self.path_units();
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            Some(PathBuf::from(std::ffi::OsString::from_wide(&units)))
        }
        #[cfg(not(windows))]
        {
            String::from_utf16(&units).ok().map(PathBuf::from)
        }
    }
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

fn deserialize_instruction_string<'de, D>(
    deserializer: D,
) -> Result<crate::types::utf16_json::Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = crate::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if value.string_units("").is_some() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "expected an instruction source string",
        ))
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub announcement_history: Vec<crate::types::utf16_json::Utf16JsonProjection>,
    /// Typed output-style/language rows retained by an independently resumed
    /// query. These remain separate from user-context instruction sources.
    #[serde(skip)]
    pub native_settings_attachments: Vec<(
        crate::types::MessageId,
        crate::types::utf16_json::Utf16JsonProjection,
    )>,
    /// The exact, insertion-ordered JavaScript `userContext` object. This is
    /// the authoritative source for prompt, fork, and Mod projections.
    #[serde(
        default = "empty_user_context",
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize",
        deserialize_with = "deserialize_user_context"
    )]
    pub user_context: crate::types::utf16_json::Utf16JsonProjection,
    #[serde(default)]
    pub managed_instructions_only: bool,
    #[serde(default)]
    pub sent_paths: HashSet<PathBuf>,
    #[serde(default)]
    pub project_instruction_bodies: HashSet<String>,
    #[serde(default)]
    pub instructions_root: Option<PathBuf>,
}

impl Default for InstructionContext {
    fn default() -> Self {
        Self {
            eager_instructions: None,
            rendering: InstructionRendering::default(),
            announcement_history: Vec::new(),
            native_settings_attachments: Vec::new(),
            user_context: empty_user_context(),
            managed_instructions_only: false,
            sent_paths: HashSet::new(),
            project_instruction_bodies: HashSet::new(),
            instructions_root: None,
        }
    }
}

fn empty_user_context() -> crate::types::utf16_json::Utf16JsonProjection {
    crate::types::utf16_json::Utf16JsonProjection::object(Vec::new())
        .expect("empty userContext is an object")
}

fn deserialize_user_context<'de, D>(
    deserializer: D,
) -> Result<crate::types::utf16_json::Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = crate::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if value.object_entries().is_some() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "expected userContext to be an object",
        ))
    }
}

impl InstructionContext {
    /// Set one member by exact UTF-16 key units, appending new keys and
    /// replacing existing values in place to preserve JavaScript object order.
    pub fn set_user_context_value(
        &mut self,
        key_units: &[u16],
        value: crate::types::utf16_json::Utf16JsonProjection,
    ) -> Result<(), crate::types::utf16_json::Utf16JsonProjectionError> {
        use crate::types::utf16_json::Utf16JsonProjection;

        let entries = self
            .user_context
            .object_entries()
            .ok_or(crate::types::utf16_json::Utf16JsonProjectionError::ExpectedObject)?;
        let mut replaced = false;
        let mut fields = Vec::with_capacity(entries.len() + 1);
        for entry in entries {
            if entry.code_units == key_units {
                fields.push((entry.code_units.to_vec(), value.clone()));
                replaced = true;
            } else {
                fields.push((
                    entry.code_units.to_vec(),
                    self.user_context
                        .subprojection_property_units(entry.code_units)?,
                ));
            }
        }
        if !replaced {
            fields.push((key_units.to_vec(), value));
        }
        self.user_context = Utf16JsonProjection::object_units(fields)?;
        Ok(())
    }

    /// Convenience setter for ordinary Rust keys and values.
    pub fn set_user_context_text(
        &mut self,
        key: &str,
        value: impl Into<crate::types::utf16_json::Utf16JsonProjection>,
    ) -> Result<(), crate::types::utf16_json::Utf16JsonProjectionError> {
        self.set_user_context_value(&key.encode_utf16().collect::<Vec<_>>(), value.into())
    }

    /// Read one member by exact UTF-16 key units.
    #[must_use]
    pub fn user_context_value(
        &self,
        key_units: &[u16],
    ) -> Option<crate::types::utf16_json::Utf16JsonProjection> {
        self.user_context
            .subprojection_property_units(key_units)
            .ok()
    }

    /// Read a member's string units by exact UTF-16 key units.
    #[must_use]
    pub fn user_context_string_units(&self, key_units: &[u16]) -> Option<Vec<u16>> {
        self.user_context_value(key_units)?.string_units("")
    }

    /// Read one ordinary Rust string key's string value without materializing
    /// unpaired source units as a lossy Rust string.
    #[must_use]
    pub fn user_context_text(&self, key: &str) -> Option<Vec<u16>> {
        self.user_context_string_units(&key.encode_utf16().collect::<Vec<_>>())
    }

    /// Materialize a source value as Rust text only when it contains valid
    /// Unicode scalar values. Callers that serialize or render source must use
    /// the projection/code-unit accessors instead.
    #[must_use]
    pub fn user_context_utf8(&self, key: &str) -> Option<String> {
        String::from_utf16(&self.user_context_text(key)?).ok()
    }

    /// Remove one member by exact UTF-16 key units.
    pub fn remove_user_context_value(
        &mut self,
        key_units: &[u16],
    ) -> Result<bool, crate::types::utf16_json::Utf16JsonProjectionError> {
        use crate::types::utf16_json::Utf16JsonProjection;

        let entries = self
            .user_context
            .object_entries()
            .ok_or(crate::types::utf16_json::Utf16JsonProjectionError::ExpectedObject)?;
        let mut removed = false;
        let mut fields = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.code_units == key_units {
                removed = true;
            } else {
                fields.push((
                    entry.code_units.to_vec(),
                    self.user_context
                        .subprojection_property_units(entry.code_units)?,
                ));
            }
        }
        if removed {
            self.user_context = Utf16JsonProjection::object_units(fields)?;
        }
        Ok(removed)
    }

    /// Remove one ordinary Rust string key.
    pub fn remove_user_context_text(
        &mut self,
        key: &str,
    ) -> Result<bool, crate::types::utf16_json::Utf16JsonProjectionError> {
        self.remove_user_context_value(&key.encode_utf16().collect::<Vec<_>>())
    }

    /// Clone members in ECMAScript `Object.entries` order, preserving exact
    /// UTF-16 keys and values. Canonical array-index keys precede other keys;
    /// the underlying source projection itself remains in insertion order.
    #[must_use]
    pub fn user_context_entries(
        &self,
    ) -> Vec<(Vec<u16>, crate::types::utf16_json::Utf16JsonProjection)> {
        crate::host::mcp_result::javascript_object_keys(&self.user_context)
            .into_iter()
            .map(|key| {
                (
                    key.clone(),
                    self.user_context
                        .subprojection_property_units(&key)
                        .expect("userContext entry remains addressable"),
                )
            })
            .collect()
    }

    /// Whether the source object has no properties.
    #[must_use]
    pub fn user_context_is_empty(&self) -> bool {
        self.user_context
            .object_entries()
            .is_some_and(|entries| entries.is_empty())
    }

    /// Inline runQuery userContext envelope; normal context uses attachments.
    #[must_use]
    pub fn reminder_source(&self) -> Option<crate::types::utf16_json::Utf16JsonProjection> {
        use crate::types::utf16_json::Utf16JsonProjection;

        let entries = self.user_context_entries();
        let mut keys = entries
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in ["Environment", "instructions", "userEmail", "currentDate"] {
            let units = key.encode_utf16().collect::<Vec<_>>();
            if !keys.iter().any(|existing| existing == &units) {
                keys.push(units);
            }
        }
        let entries: Vec<Vec<u16>> = keys
            .iter()
            .filter_map(|key| {
                let value = self.user_context_string_units(key)?;
                (!value.is_empty()).then(|| {
                    let mut section = "# ".encode_utf16().collect::<Vec<_>>();
                    section.extend_from_slice(key);
                    section.push(b'\n' as u16);
                    section.extend(value);
                    section
                })
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        let mut units = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n"
            .encode_utf16()
            .collect::<Vec<_>>();
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                units.push(b'\n' as u16);
            }
            units.extend(entry);
        }
        units.extend(
            format!(
                "\n\n      {} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.\n</system-reminder>\n",
                branding::PRODUCT_NAME
            )
            .encode_utf16(),
        );
        Some(
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                .expect("reminder display text matches exact UTF-16 units"),
        )
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
