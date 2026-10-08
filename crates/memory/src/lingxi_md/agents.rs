//! Builtin instruction-file plugin's filesystem contract.
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
    LingxiMd,
    /// Use AGENTS.md when the project has no branded instruction files.
    #[default]
    LingxiMdOrAgentsMd,
    /// Merge both instruction hierarchies by directory.
    LingxiMdAndAgentsMd,
    /// Load enterprise managed instructions only.
    ManagedOnly,
}

impl InstructionFilesMode {
    #[must_use]
    /// Resolve the current `instructionFiles` option.
    ///
    /// `lingxi-md` loads the LINGXI.md hierarchy, `lingxi-md-or-agents-md`
    /// falls back to AGENTS.md, and `lingxi-md-and-agents-md` loads both.
    /// Absent or unrecognized values use the fallback mode.
    pub fn from_options(options: &Value) -> Self {
        match options["instructionFiles"].as_str() {
            Some("lingxi-md") => Self::LingxiMd,
            Some("lingxi-md-and-agents-md") => Self::LingxiMdAndAgentsMd,
            Some("managed-only") => Self::ManagedOnly,
            _ => Self::LingxiMdOrAgentsMd,
        }
    }
}

/// Read the canonical builtin ID within each trusted settings tier.
#[must_use]
pub fn options_from_tiers<'a>(tiers: impl IntoIterator<Item = &'a Value>) -> Value {
    let mut result = serde_json::Map::new();
    for tier in tiers {
        if let Some(options) = tier["pluginConfigs"]["agents-md@builtin"]["options"].as_object() {
            result.extend(options.clone());
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
    fn current_instruction_modes_are_resolved() {
        assert_eq!(
            InstructionFilesMode::from_options(&json!({})),
            InstructionFilesMode::LingxiMdOrAgentsMd
        );
        assert_eq!(
            InstructionFilesMode::from_options(&json!({"instructionFiles":"lingxi-md"})),
            InstructionFilesMode::LingxiMd
        );
        assert_eq!(
            InstructionFilesMode::from_options(
                &json!({"instructionFiles":"lingxi-md-or-agents-md"})
            ),
            InstructionFilesMode::LingxiMdOrAgentsMd
        );
        assert_eq!(
            InstructionFilesMode::from_options(
                &json!({"instructionFiles":"lingxi-md-and-agents-md"})
            ),
            InstructionFilesMode::LingxiMdAndAgentsMd
        );
        assert_eq!(
            InstructionFilesMode::from_options(&json!({"instructionFiles":"managed-only"})),
            InstructionFilesMode::ManagedOnly
        );
    }

    #[test]
    fn obsolete_instruction_options_do_not_override_current_modes() {
        for obsolete in ["none", "both", "agents-fallback"] {
            assert_eq!(
                InstructionFilesMode::from_options(&json!({"projectInstructions":obsolete})),
                InstructionFilesMode::LingxiMdOrAgentsMd
            );
            assert_eq!(
                InstructionFilesMode::from_options(
                    &json!({"instructionFiles":"lingxi-md","projectInstructions":obsolete})
                ),
                InstructionFilesMode::LingxiMd
            );
        }
        for obsolete in ["claude-md", "claude-md-and-agents-md"] {
            assert_eq!(
                InstructionFilesMode::from_options(&json!({"instructionFiles":obsolete})),
                InstructionFilesMode::LingxiMdOrAgentsMd
            );
        }
    }

    #[test]
    fn canonical_plugin_options_follow_settings_precedence() {
        let user = json!({"pluginConfigs":{"agents-md@builtin":{"options":{"instructionFiles":"lingxi-md","x":1}},"cc-plugin-agents-md@builtin":{"options":{"x":2,"legacy":true}}}});
        let policy = json!({"pluginConfigs":{"agents-md@builtin":{"options":{"instructionFiles":"managed-only"}}}});
        assert_eq!(
            options_from_tiers([&user, &policy]),
            json!({"instructionFiles":"managed-only","x":1})
        );
        assert_eq!(
            options_from_tiers([
                &json!({"pluginConfigs":{"cc-plugin-agents-md@builtin":{"options":{"instructionFiles":"managed-only"}}}})
            ]),
            json!({})
        );
    }
}
