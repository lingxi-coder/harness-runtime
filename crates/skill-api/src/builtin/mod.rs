//! Compiled-in builtin skill templates plus the `register_desktop` /
//! `register_mobile` entry points the composition roots
//! (`apps/engine-{desktop,mobile}`) call to assemble their skill set.
//!
//! Mobile Local App skills are file-backed Plugin components. The mobile
//! entry point remains as an empty compatibility seam so hosts can assemble
//! the shared registry without a second bundled source of truth.

mod bundled;

use crate::model::{LoadedFrom, SkillSource};
use crate::{parse_skill_markdown, Skill, SkillRegistry};
use bundled::{BundledSkill, BUILTIN_DESKTOP, BUILTIN_MOBILE};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// Register the desktop builtin skill set into `reg`.
pub fn register_desktop(reg: &mut SkillRegistry) {
    register_slice(reg, BUILTIN_DESKTOP);
}

/// Register mobile bundled skills. Local App skills are intentionally not
/// bundled; the mobile composition root loads them from the verified Plugin
/// package through `PluginManager`.
pub fn register_mobile(reg: &mut SkillRegistry) {
    register_slice(reg, BUILTIN_MOBILE);
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
        assert_eq!(names, vec!["claude-api", "plugin-authoring"]);
        let skill = r.get("claude-api").expect("registered bundled skill");
        assert_eq!(skill.source, SkillSource::Bundled);
        assert_eq!(skill.loaded_from, LoadedFrom::Bundled);
        assert_eq!(skill.frontmatter.name, "claude-api");
        assert!(skill
            .description
            .string_at("")
            .is_some_and(|description| description.contains("Anthropic SDKs")));
        assert!(skill
            .content
            .string_at("")
            .is_some_and(|content| content.contains("/claude-api upgrade python")));
        assert!(skill
            .content
            .string_at("")
            .is_some_and(|content| content.contains("anthropic.Timeout")));
    }

    #[test]
    fn mobile_registry_contains_no_bundled_local_app_skills() {
        let mut r = SkillRegistry::new();
        register_mobile(&mut r);
        assert!(
            r.is_empty(),
            "Local App skills must come from the Plugin registry"
        );
        assert!(BUILTIN_MOBILE.is_empty());
    }

    #[test]
    fn claude_api_desktop_skill_is_discoverable_for_python_upgrade() {
        let mut r = SkillRegistry::new();
        register_desktop(&mut r);
        assert!(
            r.discover("/claude-api upgrade python")
                .iter()
                .any(|skill| skill.name == "claude-api"),
            "claude-api should be discoverable from its builtin command surface"
        );
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
