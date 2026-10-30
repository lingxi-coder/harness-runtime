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
    fn skip_shell_expansion(&self) -> bool {
        matches!(self.name, "lingxi-api" | "document-authoring")
    }

    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        skill::desktop_builtin_skill_prompt(self.name, args)
            .expect("slash builtin is registered in the desktop SkillRegistry")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_bodies_share_the_registered_skill_source() {
        for (name, expected_marker) in [
            (
                "document-authoring",
                format!("Document authoring in {}", branding::PRODUCT_NAME),
            ),
            (
                "lingxi-api",
                format!("{} API guide", branding::PRODUCT_NAME),
            ),
            (
                "plugin-authoring",
                ".lingxi-plugin/\n    plugin.json".to_owned(),
            ),
        ] {
            let prompt = DesktopBuiltinSkillPromptFn::new(name).build(&"".into());
            let skill = skill::desktop_builtin_skill(name).expect("desktop builtin skill");

            assert_eq!(prompt, skill.content);
            assert!(
                prompt
                    .string_at("")
                    .is_some_and(|body| body.contains(expected_marker.as_str())),
                "the shared body for {name} contains its expected content"
            );
        }
    }
}
