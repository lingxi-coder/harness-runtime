//! Prompt adapter for a Skill API desktop builtin.

use crate::model::BundledPromptFn;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// A slash registration that shares its prompt body and metadata with the
/// desktop `SkillRegistry` entry of the same name.
pub struct DesktopBuiltinSkillPromptFn {
    name: &'static str,
}

impl DesktopBuiltinSkillPromptFn {
    /// Construct a prompt adapter for a registered desktop skill name.
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

impl BundledPromptFn for DesktopBuiltinSkillPromptFn {
    fn build(&self, _args: &Utf16JsonProjection) -> Utf16JsonProjection {
        skill_api::desktop_builtin_skill_content(self.name)
            .expect("slash builtin is registered in the desktop SkillRegistry")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_bodies_share_the_registered_skill_source() {
        for (name, expected_marker) in [
            ("claude-api", "Anthropic 1.x SDK surface"),
            ("plugin-authoring", ".lingxi-plugin/\n    plugin.json"),
        ] {
            let prompt = DesktopBuiltinSkillPromptFn::new(name).build(&"".into());
            let skill = skill_api::desktop_builtin_skill(name).expect("desktop builtin skill");

            assert_eq!(prompt, skill.content);
            assert!(
                prompt
                    .string_at("")
                    .is_some_and(|body| body.contains(expected_marker)),
                "the shared body for {name} contains its expected content"
            );
        }
    }
}
