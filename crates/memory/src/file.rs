//! Memory file representation and frontmatter parsing.
//!
//! A memory file is a markdown document with an optional YAML
//! frontmatter block. The frontmatter carries metadata used by the
//! selector (spec §6.2).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;
use thiserror::Error;

/// One memory file loaded from disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFile {
    /// On-disk path the file was loaded from.
    pub path: PathBuf,
    /// Last-modified timestamp at the time of load.
    pub mtime: SystemTime,
    /// Parsed YAML frontmatter; defaults are used when missing.
    pub frontmatter: MemoryFrontmatter,
    /// Markdown body (frontmatter stripped, leading whitespace trimmed).
    pub content: String,
}

/// YAML frontmatter fields recognised by the engine.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MemoryFrontmatter {
    /// Free-form category (e.g. `tool_usage`, `convention`).
    pub memory_type: String,
    /// One-line summary shown to the selector LLM.
    pub description: String,
    /// Optional natural-language trigger for the selector.
    pub when_to_use: Option<String>,
    /// Topical tags used by the selector for ranking.
    pub tags: Vec<String>,
    /// Tool full-names this memory is relevant to.
    pub related_tools: Vec<String>,
}

/// Failure modes for memory loading and parsing.
#[derive(Debug, Clone, Error)]
pub enum MemoryError {
    /// YAML or markdown parse failure.
    #[error("parse failed: {0}")]
    Parse(String),
    /// I/O failure while reading a memory file.
    #[error("io failed: {0}")]
    Io(String),
    /// Selector LLM call could not be issued.
    #[error("selector unavailable: {0}")]
    SelectorUnavailable(String),
}

/// Maximum number of lines an entrypoint memory file may have.
pub const MAX_ENTRYPOINT_LINES: usize = 200;
/// Maximum size in bytes for an entrypoint memory file.
pub const MAX_ENTRYPOINT_BYTES: usize = 25_000;

/// Parse a `---\n<yaml>\n---\n<body>` markdown file.
///
/// Returns `(default_frontmatter, original_input)` when the file does not
/// start with a frontmatter delimiter.
pub fn parse_markdown_with_frontmatter(
    input: &str,
) -> Result<(MemoryFrontmatter, String), MemoryError> {
    if !input.starts_with("---") {
        return Ok((MemoryFrontmatter::default(), input.to_string()));
    }
    let rest = &input[3..];
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| MemoryError::Parse("unterminated frontmatter".into()))?;
    let yaml = &rest[..end];
    let body = &rest[end + 5..];
    let fm: MemoryFrontmatter =
        serde_yaml::from_str(yaml).map_err(|e| MemoryError::Parse(e.to_string()))?;
    Ok((fm, body.trim_start().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_frontmatter() {
        let raw = "---\nmemory_type: tool_usage\ndescription: bash tips\n---\nuse fd not find\n";
        let (fm, body) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(fm.memory_type, "tool_usage");
        assert_eq!(fm.description, "bash tips");
        assert!(body.contains("fd"));
    }

    #[test]
    fn no_frontmatter_returns_default() {
        let raw = "just markdown\n";
        let (fm, body) = parse_markdown_with_frontmatter(raw).unwrap();
        assert!(fm.description.is_empty());
        assert_eq!(body, raw);
    }
}
