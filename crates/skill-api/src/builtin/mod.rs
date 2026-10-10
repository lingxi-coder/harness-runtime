//! Compiled-in builtin skill templates plus the `register_desktop` /
//! `register_mobile` entry points the composition roots
//! (`apps/engine-{desktop,mobile}`) call to assemble their skill set.
//!
//! Mobile product skills are file-backed Plugin components. The mobile
//! entry point remains as an empty compatibility seam so hosts can assemble
//! the shared registry without a second bundled source of truth.

mod bundled;

use crate::model::{LoadedFrom, SkillSource};
use crate::{parse_skill_markdown, Skill, SkillRegistry};
use bundled::{BundledSkill, BUILTIN_DESKTOP, BUILTIN_MOBILE};

/// Register the desktop builtin skill set into `reg`.
pub fn register_desktop(reg: &mut SkillRegistry) {
    register_slice(reg, BUILTIN_DESKTOP);
}

/// Register mobile bundled skills. Product skills are intentionally not
/// bundled; the mobile composition root loads them from the verified Plugin
/// package through `PluginManager`.
pub fn register_mobile(reg: &mut SkillRegistry) {
    register_slice(reg, BUILTIN_MOBILE);
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
        skill.content.push_str("\n\n---\n\n");
        skill
            .content
            .push_str(&format!("## Bundled resource: `{}`\n\n", reference.path));
        skill.content.push_str(reference.content);
    }
    skill
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
        assert_eq!(names, vec!["claude-api"]);
        let skill = r.get("claude-api").expect("registered bundled skill");
        assert_eq!(skill.source, SkillSource::Bundled);
        assert_eq!(skill.loaded_from, LoadedFrom::Bundled);
        assert_eq!(skill.frontmatter.name, "claude-api");
        assert!(skill.description.contains("Anthropic SDKs"));
        assert!(skill.content.contains("/claude-api upgrade python"));
        assert!(skill.content.contains("anthropic.Timeout"));
    }

    #[test]
    fn mobile_registry_contains_no_bundled_plugin_skills() {
        let mut r = SkillRegistry::new();
        register_mobile(&mut r);
        assert!(
            r.is_empty(),
            "product skills must come from the Plugin registry"
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
}
