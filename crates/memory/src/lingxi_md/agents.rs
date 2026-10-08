//! Claude Code 2.1.286 instruction-file plugin's filesystem contract.
//!
//! The option belongs to `pluginConfigs[plugin].options`, not project settings.
//! Hosts supply trusted user, flag and managed tiers in increasing precedence.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{loader, LingxiMdExcluder, LingxiMdTier};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Instruction file selection resolved from trusted builtin plugin options.
pub enum InstructionFilesMode {
    /// Load the existing branded instruction hierarchy.
    ClaudeMd,
    /// Use AGENTS.md when the project has no branded instruction files.
    #[default]
    ClaudeMdOrAgentsMd,
    /// Merge both instruction hierarchies by directory.
    ClaudeMdAndAgentsMd,
    /// Load enterprise managed instructions only.
    ManagedOnly,
}

impl InstructionFilesMode {
    #[must_use]
    /// Resolve current and legacy option spellings using builtin precedence.
    pub fn from_options(options: &Value) -> Self {
        let current = match options["instructionFiles"].as_str() {
            Some("claude-md") => Self::ClaudeMd,
            Some("claude-md-and-agents-md") => Self::ClaudeMdAndAgentsMd,
            Some("managed-only") => Self::ManagedOnly,
            _ => Self::ClaudeMdOrAgentsMd,
        };
        // Ye honours the old spelling only when the resolved new mode is the
        // default, including an explicitly selected default or invalid value.
        if current == Self::ClaudeMdOrAgentsMd && options.get("projectInstructions").is_some() {
            return match options["projectInstructions"].as_str() {
                Some("none") => Self::ManagedOnly,
                Some("agents-fallback") => Self::ClaudeMdOrAgentsMd,
                Some("both") => Self::ClaudeMdAndAgentsMd,
                _ => Self::ClaudeMd,
            };
        }
        current
    }
}

/// Alias first, canonical builtin ID last within each trusted settings tier.
#[must_use]
pub fn options_from_tiers<'a>(tiers: impl IntoIterator<Item = &'a Value>) -> Value {
    let mut result = serde_json::Map::new();
    for tier in tiers {
        for key in ["agents-md@builtin", "cc-plugin-agents-md@builtin"] {
            if let Some(options) = tier["pluginConfigs"][key]["options"].as_object() {
                result.extend(options.clone());
            }
        }
    }
    Value::Object(result)
}

#[derive(Debug)]
/// One ancestor's instruction roots together with their expanded imports.
pub struct AncestorGroup {
    /// Directory containing the root instruction file.
    pub dir: PathBuf,
    /// Root and imported entries in discovery order.
    pub files: Vec<loader::MemoryEntry>,
}

#[must_use]
/// Canonical identity used for sent-path and import deduplication.
pub fn identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `fs.ancestors`: root-to-leaf, excluding the filesystem root and `below`.
/// Imports retain their root file's group, even when they live elsewhere.
#[must_use]
pub fn ancestors(
    start: &Path,
    below: Option<&Path>,
    home: &Path,
    agents: bool,
    include_external: bool,
    excluder: Option<&LingxiMdExcluder>,
) -> Vec<AncestorGroup> {
    let start = identity(start);
    let below = below.map(identity);
    let mut directories = Vec::new();
    let mut cursor = start.as_path();
    while let Some(parent) = cursor.parent() {
        if below
            .as_ref()
            .is_some_and(|root| cursor == root || !cursor.starts_with(root))
        {
            break;
        }
        directories.push(cursor.to_path_buf());
        cursor = parent;
    }
    directories.reverse();
    let names = if agents {
        vec![
            PathBuf::from("AGENTS.md"),
            Path::new(branding::DOT_DIR).join("AGENTS.md"),
        ]
    } else {
        vec![
            PathBuf::from(branding::MEMORY_FILE),
            Path::new(branding::DOT_DIR).join(branding::MEMORY_FILE),
            PathBuf::from(branding::MEMORY_LOCAL_FILE),
        ]
    };
    let mut seen = HashSet::new();
    // The fs API seeds these identities so walking through HOME cannot turn a
    // user/managed instruction file into a project instruction.
    seen.insert(identity(
        &super::hierarchy::user_config_dir(home).join(branding::MEMORY_FILE),
    ));
    seen.insert(identity(
        &super::hierarchy::managed_path().join(branding::MEMORY_FILE),
    ));
    let mut groups = Vec::new();
    for dir in directories {
        for name in &names {
            let files = loader::expand_memory_file_with_excluder(
                &dir.join(name),
                &mut seen,
                include_external,
                below.as_deref().unwrap_or(&start),
                Some(home),
                0,
                LingxiMdTier::Project,
                excluder,
            );
            if !files.is_empty() {
                groups.push(AncestorGroup {
                    dir: dir.clone(),
                    files,
                });
            }
        }
    }
    groups
}

#[must_use]
/// Whether nested instruction attachments are enabled for this process.
pub fn attachments_enabled() -> bool {
    !["LINGXI_SIMPLE", "LINGXI_DISABLE_ATTACHMENTS"]
        .iter()
        .any(|key| {
            std::env::var(key).is_ok_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn option_precedence_and_legacy_default_match_plugin() {
        assert_eq!(
            InstructionFilesMode::from_options(&json!({})),
            InstructionFilesMode::ClaudeMdOrAgentsMd
        );
        assert_eq!(
            InstructionFilesMode::from_options(
                &json!({"instructionFiles":"claude-md-or-agents-md","projectInstructions":"both"})
            ),
            InstructionFilesMode::ClaudeMdAndAgentsMd
        );
        assert_eq!(
            InstructionFilesMode::from_options(
                &json!({"instructionFiles":"managed-only","projectInstructions":"both"})
            ),
            InstructionFilesMode::ManagedOnly
        );
        assert_eq!(
            InstructionFilesMode::from_options(&json!({"projectInstructions":false})),
            InstructionFilesMode::ClaudeMd
        );
        let user = json!({"pluginConfigs":{"agents-md@builtin":{"options":{"instructionFiles":"claude-md","x":1}},"cc-plugin-agents-md@builtin":{"options":{"x":2}}}});
        let policy = json!({"pluginConfigs":{"agents-md@builtin":{"options":{"instructionFiles":"managed-only"}}}});
        assert_eq!(
            options_from_tiers([&user, &policy]),
            json!({"instructionFiles":"managed-only","x":2})
        );
    }
}
