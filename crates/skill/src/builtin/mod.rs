//! Compiled-in desktop builtin skill templates and their registration entry point.

mod bundled;
mod lingxi_api;

use crate::model::{LoadedFrom, SkillSource};
use crate::{parse_skill_markdown, Skill, SkillRegistry};
use bundled::{BundledSkill, BUILTIN_DESKTOP};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// Register the desktop builtin skill set into `reg`.
pub fn register_desktop(reg: &mut SkillRegistry) {
    register_slice(reg, BUILTIN_DESKTOP);
}

/// Return a parsed desktop builtin skill by its registered name. Command
/// handlers use the same compiled source as the `Skill` tool so slash
/// invocation and tool discovery cannot drift apart.
#[must_use]
pub fn desktop_builtin_skill(name: &str) -> Option<Skill> {
    BUILTIN_DESKTOP
        .iter()
        .find(|entry| entry.name == name)
        .map(parse_builtin)
}

/// Return the exact prompt body for a desktop builtin skill by its registered
/// name.
#[must_use]
pub fn desktop_builtin_skill_content(name: &str) -> Option<Utf16JsonProjection> {
    desktop_builtin_skill(name).map(|skill| skill.content)
}

/// Expand a builtin prompt, loading only the requested product-guide topic.
/// Other builtins retain their existing body expansion behavior.
#[must_use]
pub fn desktop_builtin_skill_prompt(
    name: &str,
    args: &Utf16JsonProjection,
) -> Option<Utf16JsonProjection> {
    let body = desktop_builtin_skill_content(name)?;
    Some(match name {
        "lingxi-api" => lingxi_api::render(body, args),
        "docs" => {
            let request = args.string_units("").unwrap_or_default();
            if request
                .iter()
                .all(|unit| char::from_u32(u32::from(*unit)).is_some_and(char::is_whitespace))
            {
                return Some(body);
            }
            let mut units = body.string_units("").expect("builtin prompt is text");
            units.extend("\n\n## User Request\n\n".encode_utf16());
            units.extend(request);
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
                .expect("document prompt preserves request source")
        }
        _ => body,
    })
}

/// Offline reference context for the dedicated read-only product guide agent.
/// It uses the same sources as the on-demand API skill without requiring the
/// agent to receive an executable Skill or shell tool.
#[must_use]
pub fn lingxi_api_guide_context() -> Utf16JsonProjection {
    lingxi_api::guide_context(
        desktop_builtin_skill_content("lingxi-api").expect("builtin API guide"),
    )
}

fn register_slice(reg: &mut SkillRegistry, slice: &[BundledSkill]) {
    for entry in slice {
        reg.register(parse_builtin(entry));
    }
}

/// Parse a compiled-in skill template with bundled provenance.
fn parse_builtin(entry: &BundledSkill) -> Skill {
    let mut skill = parse_skill_markdown(
        entry.raw,
        std::path::PathBuf::from(format!("<bundled:{}>", entry.name)),
        SkillSource::Bundled,
        LoadedFrom::Bundled,
    )
    .unwrap_or_else(|e| panic!("bundled skill `{}` failed to parse: {e}", entry.name));
    skill.frontmatter.triggers = entry
        .triggers
        .iter()
        .map(|trigger| (*trigger).into())
        .collect();
    for reference in entry.references {
        append_utf8(&mut skill.content, "\n\n---\n\n");
        append_utf8(
            &mut skill.content,
            &format!("## Bundled resource: `{}`\n\n", reference.path),
        );
        append_utf8(&mut skill.content, reference.content);
    }
    skill
}

fn append_utf8(target: &mut Utf16JsonProjection, text: &str) {
    let mut units = target
        .string_units("")
        .expect("skill body is a root string");
    units.extend(text.encode_utf16());
    *target = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("UTF-8 resource text preserves the skill source string");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_registry_matches_bundled_len() {
        let mut r = SkillRegistry::new();
        register_desktop(&mut r);
        assert_eq!(r.names().len(), BUILTIN_DESKTOP.len());
        let mut names = r.names();
        names.sort_unstable();
        assert_eq!(names, vec!["docs", "lingxi-api", "plugin-authoring"]);
        assert!(r.get("claude-api").is_none());
        let skill = r.get("lingxi-api").expect("registered bundled skill");
        assert_eq!(skill.source, SkillSource::Bundled);
        assert_eq!(skill.loaded_from, LoadedFrom::Bundled);
        assert_eq!(skill.frontmatter.name, "lingxi-api");
        assert!(skill
            .description
            .string_at("")
            .is_some_and(|description| description.contains("LingXi (灵犀)")));
        assert!(skill
            .content
            .string_at("")
            .is_some_and(|content| content.contains("/lingxi-api <topic>")));
    }

    #[test]
    fn product_guide_discovery_stays_specific_to_lingxi() {
        let mut r = SkillRegistry::new();
        register_desktop(&mut r);
        for query in [
            "/lingxi-api media",
            "Configure LingXi MCP",
            "灵犀的 Bot 如何使用",
        ] {
            assert!(r
                .discover(query)
                .iter()
                .any(|skill| skill.name == "lingxi-api"));
        }
        for query in ["upgrade anthropic python", "write a report", "take a photo"] {
            assert!(!r
                .discover(query)
                .iter()
                .any(|skill| skill.name == "lingxi-api"));
        }
    }

    #[test]
    fn document_skill_preserves_literal_request_and_has_no_connector_dependency() {
        let root = desktop_builtin_skill_content("docs").unwrap();
        assert_eq!(
            desktop_builtin_skill_prompt("docs", &"".into()).unwrap(),
            root
        );
        let request = "Update the existing report; keep !`literal example` as text.";
        let prompt = desktop_builtin_skill_prompt("docs", &request.into()).unwrap();
        assert!(prompt.string_at("").unwrap().ends_with(request));
        let mut r = SkillRegistry::new();
        register_desktop(&mut r);
        for query in ["/docs", "create a document", "创建文档"] {
            assert!(r.discover(query).iter().any(|skill| skill.name == "docs"));
        }
        assert!(!r
            .discover("Explain OpenAI API docs")
            .iter()
            .any(|skill| skill.name == "docs"));
    }

    #[test]
    fn plugin_authoring_builtin_is_shared_with_command_surface() {
        let mut registry = SkillRegistry::new();
        register_desktop(&mut registry);

        let skill = registry
            .get("plugin-authoring")
            .expect("registered desktop plugin-authoring skill");
        let command_content = desktop_builtin_skill_content("plugin-authoring")
            .expect("bundled command uses the same source body");

        assert_eq!(skill.content, command_content);
        assert!(skill
            .content
            .string_at("")
            .is_some_and(|content| content.contains(".lingxi-plugin/\n    plugin.json")));
        assert!(registry
            .discover("write a LingXi plugin with a hooks module")
            .iter()
            .any(|discovered| discovered.name == "plugin-authoring"));
    }
}
