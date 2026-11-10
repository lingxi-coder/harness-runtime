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
#[serde(from = "MemoryFrontmatterSource")]
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
    /// Native memory slug, when the file supplies one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct MemoryFrontmatterSource {
    memory_type: Option<String>,
    description: String,
    when_to_use: Option<String>,
    tags: Vec<String>,
    related_tools: Vec<String>,
    name: Option<yaml_serde::Value>,
    #[serde(rename = "type")]
    native_type: Option<yaml_serde::Value>,
    metadata: Option<yaml_serde::Value>,
}

impl From<MemoryFrontmatterSource> for MemoryFrontmatter {
    fn from(source: MemoryFrontmatterSource) -> Self {
        let metadata_type = source.metadata.as_ref().and_then(|metadata| {
            metadata.as_mapping().and_then(|mapping| {
                mapping
                    .get(yaml_serde::Value::String("type".to_owned()))
                    .and_then(yaml_serde::Value::as_str)
            })
        });
        let native_type = source
            .native_type
            .as_ref()
            .and_then(yaml_serde::Value::as_str);
        let memory_type = source
            .memory_type
            .unwrap_or_else(|| metadata_type.or(native_type).unwrap_or_default().to_owned());
        Self {
            memory_type,
            description: source.description,
            when_to_use: source.when_to_use,
            tags: source.tags,
            related_tools: source.related_tools,
            name: source
                .name
                .as_ref()
                .and_then(yaml_serde::Value::as_str)
                .map(str::to_owned),
        }
    }
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
        yaml_serde::from_str(yaml).map_err(|e| MemoryError::Parse(e.to_string()))?;
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

    #[test]
    fn native_memory_frontmatter_preserves_slug_description_and_nested_type() {
        let raw = "---\nname: user-role\ndescription: Senior engineer learning the frontend\nmetadata:\n  type: user\n---\n\nDeep Go experience; explain React through backend analogies.\n";
        let (frontmatter, body) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(frontmatter.name.as_deref(), Some("user-role"));
        assert_eq!(frontmatter.memory_type, "user");
        assert_eq!(
            frontmatter.description,
            "Senior engineer learning the frontend"
        );
        assert_eq!(
            body,
            "Deep Go experience; explain React through backend analogies.\n"
        );
    }

    #[test]
    fn native_memory_type_aliases_have_explicit_legacy_precedence() {
        for (fields, expected) in [
            ("type: feedback", "feedback"),
            ("type: feedback\nmetadata:\n  type: project", "project"),
            (
                "memory_type: tool_usage\ntype: feedback\nmetadata:\n  type: project",
                "tool_usage",
            ),
            ("memory_type: ''\nmetadata:\n  type: reference", ""),
        ] {
            let raw = format!("---\n{fields}\n---\nFact\n");
            assert_eq!(
                parse_markdown_with_frontmatter(&raw).unwrap().0.memory_type,
                expected
            );
        }
    }

    #[test]
    fn legacy_frontmatter_fields_and_serialized_shape_remain_compatible() {
        let raw = "---\nmemory_type: tool_usage\ndescription: Run the narrow check\nwhen_to_use: Before committing\ntags: [testing, rust]\nrelated_tools: [Bash]\n---\nRun cargo test.\n";
        let (frontmatter, _) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(
            frontmatter.when_to_use.as_deref(),
            Some("Before committing")
        );
        assert_eq!(frontmatter.tags, ["testing", "rust"]);
        assert_eq!(frontmatter.related_tools, ["Bash"]);
        assert!(frontmatter.name.is_none());
        let encoded = serde_json::to_value(&frontmatter).unwrap();
        assert!(encoded.get("name").is_none());
        assert_eq!(encoded["memory_type"], "tool_usage");
        let decoded: MemoryFrontmatter = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.memory_type, frontmatter.memory_type);
        assert_eq!(decoded.description, frontmatter.description);
        assert_eq!(decoded.tags, frontmatter.tags);
    }

    #[test]
    fn native_frontmatter_type_survives_the_existing_writer_format() {
        let raw = "---\nname: test-guidance\ndescription: Focused verification\nmetadata:\n  type: feedback\n---\nUse the relevant test target.\n";
        let (frontmatter, body) = parse_markdown_with_frontmatter(raw).unwrap();
        let yaml = yaml_serde::to_string(&frontmatter).unwrap();
        let written = format!("---\n{yaml}---\n{body}");
        let (reloaded, reloaded_body) = parse_markdown_with_frontmatter(&written).unwrap();
        assert_eq!(reloaded.name.as_deref(), Some("test-guidance"));
        assert_eq!(reloaded.memory_type, "feedback");
        assert_eq!(reloaded.description, "Focused verification");
        assert_eq!(reloaded_body, body);
    }

    #[test]
    fn unrelated_or_malformed_native_extensions_do_not_break_legacy_notes() {
        for extensions in [
            "name: [unexpected]\ntype: 7\nmetadata: not-a-map",
            "metadata:\n  type: [unexpected]\n  other: true",
        ] {
            let raw = format!("---\nmemory_type: convention\n{extensions}\n---\nKeep this fact.\n");
            let (frontmatter, body) = parse_markdown_with_frontmatter(&raw).unwrap();
            assert_eq!(frontmatter.memory_type, "convention");
            assert!(frontmatter.name.is_none());
            assert_eq!(body, "Keep this fact.\n");
        }
    }
}
