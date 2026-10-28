//! Markdown frontmatter parsing for skill files.
//!
//! Accepts files starting with a `---` YAML block followed by a blank line and
//! the markdown body. Missing frontmatter falls back to default values.

use crate::model::{LoadedFrom, Skill, SkillFrontmatter, SkillSource};
use std::path::PathBuf;
use thiserror::Error;

const UTF8_BOM: char = '\u{feff}';

/// Errors produced while loading a skill from disk.
#[derive(Debug, Clone, Error)]
pub enum SkillLoadError {
    /// Frontmatter could not be parsed (malformed YAML or missing terminator).
    #[error("parse failed: {0}")]
    Parse(String),
}

/// Parse a raw skill markdown string into a [`Skill`].
///
/// `raw` may optionally start with a `---`-delimited YAML frontmatter block
/// followed by `\n---\n` and the markdown body. When no frontmatter is present
/// a default [`SkillFrontmatter`] is used and the whole input is treated as
/// content.
pub fn parse_skill_markdown(
    raw: &str,
    file_path: PathBuf,
    source: SkillSource,
    loaded_from: LoadedFrom,
) -> Result<Skill, SkillLoadError> {
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw);
    let (fm, body): (SkillFrontmatter, String) = if let Some(rest) = raw.strip_prefix("---") {
        let end = rest
            .find("\n---\n")
            .ok_or_else(|| SkillLoadError::Parse("unterminated".into()))?;
        let yaml = &rest[..end];
        let body = &rest[end + 5..];
        let fm = parse_frontmatter_yaml(yaml).map_err(|e| SkillLoadError::Parse(e.to_string()))?;
        (fm, body.trim_start().to_string())
    } else {
        (SkillFrontmatter::default(), raw.to_string())
    };

    Ok(Skill {
        name: fm.name.clone(),
        description: fm.description.clone(),
        frontmatter: fm,
        content: body.into(),
        source,
        loaded_from,
        plugin_id: None,
        file_path,
    })
}

fn parse_frontmatter_yaml(yaml: &str) -> Result<SkillFrontmatter, yaml_serde::Error> {
    // The reusable SkillFrontmatter DTO uses raw JSON serde for its source
    // description. YAML skill files are UTF-8 text, so extract that one scalar
    // at the file boundary, deserialize the remaining YAML fields normally,
    // and install the scalar directly into the source carrier.
    let mut value = yaml_serde::from_str::<yaml_serde::Value>(yaml)?;
    let description = match &mut value {
        yaml_serde::Value::Mapping(mapping) => mapping
            .remove(yaml_serde::Value::String("description".to_owned()))
            .map(yaml_serde::from_value::<String>)
            .transpose()?,
        _ => None,
    };
    let mut frontmatter: SkillFrontmatter = yaml_serde::from_value(value)?;
    if let Some(description) = description {
        frontmatter.description = description.into();
    }
    Ok(frontmatter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SkillSource;

    fn parse(yaml: &str, body: &str) -> Skill {
        let raw = format!("---\n{yaml}\n---\n{body}");
        parse_skill_markdown(
            raw.as_str(),
            "/tmp/x.md".into(),
            SkillSource::Settings(lingxi_core::types::SettingsScope::User),
            LoadedFrom::Skills,
        )
        .expect("parse ok")
    }

    #[test]
    fn yaml_frontmatter_preserves_block_text_and_resolves_aliases() {
        let skill = parse(
            "name: unicode\ndescription: |-\n  检查输入\n  Keep the second line.\nshared-tools: &readers [Read, Grep]\nallowed-tools: *readers",
            "body",
        );

        assert_eq!(
            skill.frontmatter.description.string_at(""),
            Some("检查输入\nKeep the second line.")
        );
        assert_eq!(
            skill.frontmatter.allowed_tools.as_deref(),
            Some(&["Read".to_string(), "Grep".to_string()][..])
        );
        assert_eq!(skill.content.string_at(""), Some("body"));
    }

    #[test]
    fn yaml_frontmatter_rejects_unresolved_aliases() {
        let result = parse_skill_markdown(
            "---\nname: x\nallowed-tools: *missing\n---\nbody",
            "/tmp/x.md".into(),
            SkillSource::Settings(lingxi_core::types::SettingsScope::User),
            LoadedFrom::Skills,
        );
        assert!(matches!(result, Err(SkillLoadError::Parse(_))));
    }

    // ---- disable-model-invocation (P1 gap #2) --------------------------------

    #[test]
    fn disable_model_invocation_parsed_true() {
        let s = parse("disable-model-invocation: true\nname: locked", "body");
        assert!(
            s.frontmatter.disable_model_invocation,
            "disable-model-invocation: true must parse"
        );
    }

    #[test]
    fn disable_model_invocation_defaults_false() {
        let s = parse("name: unlocked", "body");
        assert!(
            !s.frontmatter.disable_model_invocation,
            "disable-model-invocation defaults to false"
        );
    }

    // ---- user-invocable (P1 gap #3) ------------------------------------------

    #[test]
    fn user_invocable_parsed_false() {
        let s = parse("user-invocable: false\nname: model-only", "body");
        assert_eq!(
            s.frontmatter.user_invocable,
            Some(false),
            "user-invocable: false must parse to Some(false)"
        );
    }

    #[test]
    fn user_invocable_defaults_none() {
        let s = parse("name: x", "body");
        assert_eq!(
            s.frontmatter.user_invocable, None,
            "user-invocable absent → None (unset, not false)"
        );
    }

    // ---- cc 2.1.218 boolean coercion (yes/no/on/off/1/0) ---------------------

    #[test]
    fn boolean_coercion_truthy_spellings() {
        // 2.1.218 changelog: yes/no/on/off/1/0 (case-insensitive) join
        // true/false for skill frontmatter booleans. `yes` is a STRING in
        // YAML 1.2, so pre-218 this was a whole-skill parse error.
        for raw in [
            "disable-model-invocation: yes",
            "disable-model-invocation: \"On\"",
            "disable-model-invocation: 1",
            "disable-model-invocation: \"1\"",
            "disable-model-invocation: TRUE",
        ] {
            let s = parse(&format!("{raw}\nname: x"), "body");
            assert!(s.frontmatter.disable_model_invocation, "raw: {raw}");
        }
    }

    #[test]
    fn boolean_coercion_falsy_and_garbage() {
        for raw in [
            "disable-model-invocation: no",
            "disable-model-invocation: \"OFF\"",
            "disable-model-invocation: 0",
            // rtr = Kde ?? false: declared garbage lands on false, not error.
            "disable-model-invocation: sometimes",
        ] {
            let s = parse(&format!("{raw}\nname: x"), "body");
            assert!(!s.frontmatter.disable_model_invocation, "raw: {raw}");
        }
    }

    #[test]
    fn user_invocable_coerces_and_declares_on_garbage() {
        // Present key always declares (`U === void 0 ? !0 : rtr(U)`): the
        // truthy/falsy spellings map, garbage lands on Some(false).
        let s = parse("user-invocable: off\nname: x", "body");
        assert_eq!(s.frontmatter.user_invocable, Some(false));
        let s = parse("user-invocable: yes\nname: x", "body");
        assert_eq!(s.frontmatter.user_invocable, Some(true));
        let s = parse("user-invocable: whatever\nname: x", "body");
        assert_eq!(s.frontmatter.user_invocable, Some(false));
    }

    #[test]
    fn background_kde_garbage_is_undeclared() {
        // Bare `Kde`: `background ?? true` must survive a typo — garbage is
        // UNDECLARED (None), never false.
        let s = parse("background: yes\nname: x", "body");
        assert_eq!(s.frontmatter.background, Some(true));
        let s = parse("background: \"0\"\nname: x", "body");
        assert_eq!(s.frontmatter.background, Some(false));
        let s = parse("background: maybe\nname: x", "body");
        assert_eq!(s.frontmatter.background, None);
    }

    // ---- disallowed-tools (P1 gap #4) ----------------------------------------

    #[test]
    fn disallowed_tools_parsed_list() {
        let s = parse("disallowed-tools:\n  - Bash\n  - Edit\nname: safe", "body");
        assert_eq!(
            s.frontmatter.disallowed_tools.as_deref(),
            Some(&["Bash".to_string(), "Edit".to_string()][..]),
            "disallowed-tools YAML list must parse"
        );
    }

    #[test]
    fn disallowed_tools_alias_disallowedtools() {
        // Binary bytes 94993840: also accepts `disallowedTools` camelCase alias.
        let s = parse("disallowedTools:\n  - Write\nname: safe", "body");
        assert_eq!(
            s.frontmatter.disallowed_tools.as_deref(),
            Some(&["Write".to_string()][..]),
            "disallowedTools alias must parse"
        );
    }

    #[test]
    fn disallowed_tools_defaults_none() {
        let s = parse("name: x", "body");
        assert!(s.frontmatter.disallowed_tools.is_none());
    }

    // ---- argument-hint (P1 gap #5) -------------------------------------------

    #[test]
    fn argument_hint_parsed() {
        let s = parse("argument-hint: \"<ticket-id>\"\nname: x", "body");
        assert_eq!(
            s.frontmatter.argument_hint.as_deref(),
            Some("<ticket-id>"),
            "argument-hint must parse"
        );
    }

    #[test]
    fn argument_hint_alias_arguments() {
        let s = parse("arguments: \"<path>\"\nname: x", "body");
        assert_eq!(
            s.frontmatter.argument_hint.as_deref(),
            Some("<path>"),
            "arguments alias for argument-hint must parse"
        );
    }

    // ---- effort / version / shell (P1 gap #6) --------------------------------

    #[test]
    fn effort_version_shell_parsed() {
        let s = parse(
            "effort: high\nversion: \"1.2.3\"\nshell: zsh\nname: x",
            "body",
        );
        assert_eq!(s.frontmatter.effort.as_deref(), Some("high"));
        assert_eq!(s.frontmatter.version.as_deref(), Some("1.2.3"));
        assert_eq!(s.frontmatter.shell.as_deref(), Some("zsh"));
    }

    // ---- context / agent (P1 gap #7) -----------------------------------------

    #[test]
    fn context_and_agent_parsed() {
        let s = parse("context: fork\nagent: claude\nname: x", "body");
        assert_eq!(s.frontmatter.context.as_deref(), Some("fork"));
        assert_eq!(s.frontmatter.agent.as_deref(), Some("claude"));
    }

    // ---- paths (P1 gap #8) ---------------------------------------------------

    #[test]
    fn paths_parsed() {
        let s = parse("paths:\n  - \"src/**\"\n  - \"tests/**\"\nname: x", "body");
        assert_eq!(
            s.frontmatter.paths.as_deref(),
            Some(&["src/**".to_string(), "tests/**".to_string()][..])
        );
    }

    // ---- hide-from-slash-command-tool (P2 gap) --------------------------------

    #[test]
    fn hide_from_slash_command_tool_parsed_true() {
        let s = parse("hide-from-slash-command-tool: true\nname: x", "body");
        assert!(s.frontmatter.hide_from_slash_command_tool);
    }

    #[test]
    fn hide_from_slash_command_tool_defaults_false() {
        let s = parse("name: x", "body");
        assert!(!s.frontmatter.hide_from_slash_command_tool);
    }

    // ---- created_by / improved_by (P2 gap) -----------------------------------

    #[test]
    fn created_by_improved_by_parsed() {
        let s = parse("created_by: alice\nimproved_by: bob\nname: x", "body");
        assert_eq!(s.frontmatter.created_by.as_deref(), Some("alice"));
        assert_eq!(s.frontmatter.improved_by.as_deref(), Some("bob"));
    }

    // ---- model (also in SkillFrontmatter) ------------------------------------

    #[test]
    fn model_parsed() {
        let s = parse("model: claude-opus-4-6\nname: x", "body");
        assert_eq!(s.frontmatter.model.as_deref(), Some("claude-opus-4-6"));
    }

    // ---- body trimming -------------------------------------------------------

    #[test]
    fn body_trim_start() {
        let s = parse("name: x", "\n\nhello world");
        assert_eq!(s.content.string_at(""), Some("hello world"));
    }

    #[test]
    fn utf8_bom_before_frontmatter_is_ignored() {
        let raw = "\u{feff}---\nname: x\ndescription: demo\n---\nbody";
        let s = parse_skill_markdown(
            raw,
            "/tmp/x.md".into(),
            SkillSource::Settings(lingxi_core::types::SettingsScope::User),
            LoadedFrom::Skills,
        )
        .expect("ok");
        assert_eq!(s.frontmatter.name, "x");
        assert_eq!(s.frontmatter.description.string_at(""), Some("demo"));
        assert_eq!(s.content.string_at(""), Some("body"));
    }

    // ---- no frontmatter fallback ---------------------------------------------

    #[test]
    fn no_frontmatter_falls_back_to_defaults() {
        let raw = "just body text";
        let s = parse_skill_markdown(
            raw,
            "/tmp/x.md".into(),
            SkillSource::Settings(lingxi_core::types::SettingsScope::User),
            LoadedFrom::Skills,
        )
        .expect("ok");
        assert!(!s.frontmatter.disable_model_invocation);
        assert!(s.frontmatter.user_invocable.is_none());
        assert!(s.frontmatter.disallowed_tools.is_none());
    }
}
