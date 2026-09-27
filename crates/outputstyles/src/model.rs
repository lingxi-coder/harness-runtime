//! Output-style data model: how the engine formats assistant output (markdown,
//! plain text, JSON stream, etc.) and how style descriptors flow in from
//! configuration files or plugins.
//!
//! See spec §21.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A registered output style.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputStyle {
    /// Canonical style name (also used as the registry key).
    pub name: String,
    /// Short user-facing description.
    pub description: String,
    /// Provenance classification.
    pub source: OutputStyleSource,
    /// Parsed YAML frontmatter from the originating file (if any).
    pub frontmatter: OutputStyleFrontmatter,
    /// Text appended to the system prompt when this style is active.
    pub system_prompt_addendum: String,
    /// Filesystem path of the originating definition, if loaded from disk.
    pub source_path: Option<PathBuf>,
}

/// YAML frontmatter shape for output-style definition files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputStyleFrontmatter {
    /// Style name (must match the filename slug in practice).
    pub name: String,
    /// Short description.
    pub description: String,
    /// When true this style is treated as the default if multiple match.
    pub default: bool,
    /// Wire-level output format this style renders to.
    pub format: OutputFormat,
    /// Preserve the standard coding instructions when this style is active.
    /// The oracle canonicalizes author key variants (LXc: strip `-`/`_` +
    /// lowercase), so kebab (`keep-coding-instructions`, the schema's canonical
    /// key) and snake forms are accepted alongside camelCase.
    #[serde(
        rename = "keepCodingInstructions",
        alias = "keep-coding-instructions",
        alias = "keep_coding_instructions"
    )]
    pub keep_coding_instructions: bool,
    /// `force-for-plugin` (oracle schema `Oit()` = a BOOLEAN): when true on a
    /// plugin-owned style, the style activates automatically while its plugin is
    /// enabled. It is `@internal` — only meaningful for plugin-bundled styles.
    #[serde(rename = "force-for-plugin", default)]
    pub force_for_plugin: bool,
}

impl Default for OutputStyleFrontmatter {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            default: false,
            format: OutputFormat::default(),
            keep_coding_instructions: true,
            force_for_plugin: false,
        }
    }
}

/// Wire-level output formats supported by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputFormat {
    /// Markdown with code fences and headings (default).
    #[default]
    Markdown,
    /// Plain text, no markup.
    Plain,
    /// JSON-encoded events streamed line-by-line.
    JsonStream,
    /// Markdown styled for terse, minimal answers.
    Concise,
    /// Markdown styled for in-depth, pedagogical answers.
    Explanatory,
}

/// Where the style came from.
///
/// The settings rungs are the shared [`protocol::SettingsScope`]; `Builtin` and
/// `Plugin` are this subsystem's own producers and stay here, because they are
/// exactly what an output style can come from and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStyleSource {
    /// Compiled-in default.
    Builtin,
    /// Read from a settings tier's `output-styles/` directory.
    Settings(protocol::SettingsScope),
    /// Loaded by an installed plugin.
    Plugin,
}
