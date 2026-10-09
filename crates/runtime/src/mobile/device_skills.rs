//! Native mobile workflows exposed through the same live registry as `Skill`.
//!
//! Only tools provided by this host appear in a skill's discovery description
//! and prompt. These prompts grant no additional tool permissions and have no
//! dependency on the separately enabled Local App plugin.

use std::path::PathBuf;
use std::sync::Arc;

use command_api::{BundledPromptFn, CommandRegistry, CommandSource, SlashCommandKind};

const LOADED_FROM: &str = "mobile-device-skills";

struct DeviceSkill {
    name: &'static str,
    markdown: &'static str,
    tools: &'static [(&'static str, &'static str)],
}

const SKILLS: &[DeviceSkill] = &[
    DeviceSkill {
        name: "mobile-device",
        markdown: include_str!("device_skills/mobile-device/SKILL.md"),
        tools: &[
            ("device_status", "battery and network status"),
            ("clipboard", "read or write clipboard text"),
            ("share", "open the native share sheet"),
            ("notification", "post a local notification"),
            ("haptics", "trigger haptic feedback"),
            ("open_url", "open an external URL"),
        ],
    },
    DeviceSkill {
        name: "mobile-media",
        markdown: include_str!("device_skills/mobile-media/SKILL.md"),
        tools: &[
            ("camera", "capture or select a photo"),
            ("voice", "record microphone audio"),
            ("speech", "live speech recognition or playback"),
        ],
    },
    DeviceSkill {
        name: "mobile-personal-context",
        markdown: include_str!("device_skills/mobile-personal-context/SKILL.md"),
        tools: &[
            ("location", "current location"),
            ("calendar", "read events in a bounded calendar interval"),
            ("contacts", "search contacts by name"),
        ],
    },
];

struct DevicePrompt(String);

impl BundledPromptFn for DevicePrompt {
    fn build(&self, args: &str) -> String {
        if args.trim().is_empty() {
            self.0.clone()
        } else {
            format!("{}\n\n## User Request\n\n{args}", self.0)
        }
    }
}

/// Keep the shared introduction and only the supported tool sections.
/// Each level-two heading in the embedded skills names exactly one tool.
fn supported_sections(body: &str, available_tool_names: &[String]) -> String {
    let mut include = true;
    let mut result = String::new();
    for line in body.split_inclusive('\n') {
        if let Some(tool_name) = line.strip_prefix("## ") {
            include = available_tool_names
                .iter()
                .any(|name| name == tool_name.trim());
        }
        if include {
            result.push_str(line);
        }
    }
    result
}

/// Register device workflows after the host has selected its available tools.
///
/// Reuse this on catalog reload: withdrawn backends remove their skill sections
/// and groups with no remaining tools disappear from the live registry. Tool
/// permissions stay with the runtime and OS; `allowed_tools` is left unset.
pub(crate) fn register_mobile_device_skills(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
) {
    registry.unregister_loaded_from(LOADED_FROM);
    for skill in SKILLS {
        let available: Vec<_> = skill
            .tools
            .iter()
            .filter(|(name, _)| {
                available_tool_names
                    .iter()
                    .any(|available| available.as_str() == *name)
            })
            .collect();
        if available.is_empty() {
            continue;
        }

        // Parsing the embedded asset keeps its name, description and session
        // policy in the same format used by ordinary file-backed skills.
        let skill_root = PathBuf::from(LOADED_FROM).join(skill.name);
        let parsed = command_api::parse_skill_command_markdown(
            skill.markdown,
            skill_root.join("SKILL.md"),
            skill_root,
            CommandSource::Bundled,
        );
        let mut command = command_api::build_skill_command(&parsed, CommandSource::Bundled);
        let SlashCommandKind::Markdown {
            frontmatter,
            prompt_template,
            ..
        } = command.kind
        else {
            unreachable!("embedded device skill is a Markdown prompt")
        };
        let workflows = available
            .iter()
            .map(|(_, description)| *description)
            .collect::<Vec<_>>()
            .join("; ");
        command.description = format!("{} Available workflows: {workflows}.", command.description);
        command.kind = SlashCommandKind::Bundled {
            frontmatter,
            prompt_fn: Some(Arc::new(DevicePrompt(supported_sections(
                &prompt_template,
                available_tool_names,
            )))),
        };
        command.loaded_from = Some(LOADED_FROM.into());
        command.skill_root = None;
        registry.register_command(command);
    }
    // `/visualize` follows the `Visualization` tool the same way the device
    // workflows follow theirs, on boot and on every catalog reload.
    crate::inline_visualization::register_skill(
        registry,
        available_tool_names,
        tool_shell_mobile::TOOL_NAME,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile::skill_loader::MobileDiskSkillLoader;
    use session::jsonl::SessionMode;
    use tokio::sync::RwLock;
    use tool_skill::skill::SkillLoader;

    const ALL_TOOLS: &[&str] = &[
        "camera",
        "voice",
        "speech",
        "device_status",
        "clipboard",
        "share",
        "notification",
        "haptics",
        "open_url",
        "location",
        "calendar",
        "contacts",
    ];

    fn tool_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).into()).collect()
    }

    #[tokio::test]
    async fn visualize_skill_follows_the_visualization_tool_in_both_modes() {
        let mut registry = CommandRegistry::new();
        register_mobile_device_skills(&mut registry, &tool_names(&["Read"]));
        assert!(registry.resolve("visualize").is_none());
        register_mobile_device_skills(&mut registry, &tool_names(&["Visualization", "Shell"]));
        let registry = Arc::new(RwLock::new(registry));
        for mode in [SessionMode::Chat, SessionMode::Code] {
            let loader = MobileDiskSkillLoader::for_mode(registry.clone(), mode);
            assert_eq!(loader.list_names().await, ["visualize"]);
            let descriptor = loader.load("visualize").await.unwrap().unwrap();
            assert!(descriptor.allowed_tools.is_empty());
            assert!(descriptor.shell.is_none() && descriptor.context.is_none());
            assert_eq!(descriptor.skip_shell_expansion, mode == SessionMode::Chat);
            assert!(descriptor
                .dynamic_body
                .unwrap()
                .build("")
                .contains("with the `Shell` tool"));
        }
    }

    #[tokio::test]
    async fn device_skills_load_in_both_modes_without_granting_tool_permissions() {
        let mut registry = CommandRegistry::new();
        register_mobile_device_skills(&mut registry, &tool_names(ALL_TOOLS));
        let registry = Arc::new(RwLock::new(registry));
        for mode in [SessionMode::Chat, SessionMode::Code] {
            let loader = MobileDiskSkillLoader::for_mode(registry.clone(), mode);
            let mut names = loader.list_names().await;
            names.sort();
            assert_eq!(
                names,
                ["mobile-device", "mobile-media", "mobile-personal-context"]
            );
            for name in names {
                let descriptor = loader.load(&name).await.unwrap().unwrap();
                assert!(!descriptor.disable_model_invocation);
                assert!(descriptor.allowed_tools.is_empty());
                assert!(descriptor.disallowed_tools.is_empty());
                assert!(descriptor.skill_root.is_none());
                assert!(descriptor.context.is_none());
                assert!(descriptor.shell.is_none());
                let prompt = descriptor.dynamic_body.unwrap();
                let body = prompt.build("");
                assert!(body.starts_with("# "));
                assert!(body.contains("No Local App"));
                assert!(prompt
                    .build("Read my device")
                    .ends_with("## User Request\n\nRead my device"));
            }
        }
    }

    #[tokio::test]
    async fn device_skill_reload_updates_live_loader_and_removes_missing_backends() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let loader = MobileDiskSkillLoader::for_mode(registry.clone(), SessionMode::Chat);
        register_mobile_device_skills(&mut *registry.write().await, &tool_names(ALL_TOOLS));
        assert_eq!(loader.list_names().await.len(), 3);

        register_mobile_device_skills(
            &mut *registry.write().await,
            &tool_names(&["clipboard", "contacts"]),
        );
        assert!(loader.load("mobile-media").await.unwrap().is_none());
        let device = loader.load("mobile-device").await.unwrap().unwrap();
        let body = device.dynamic_body.unwrap().build("");
        assert!(device.description.contains("clipboard"));
        assert!(!device.description.contains("URL"));
        assert!(body.contains("## clipboard\n"));
        assert!(!body.contains("## open_url\n"));
        assert!(!body.contains("## notification\n"));
        let personal = loader
            .load("mobile-personal-context")
            .await
            .unwrap()
            .unwrap();
        let body = personal.dynamic_body.unwrap().build("");
        assert!(body.contains("## contacts\n"));
        assert!(!body.contains("## calendar\n"));
        assert!(!body.contains("## location\n"));

        register_mobile_device_skills(&mut *registry.write().await, &[]);
        assert!(loader.list_names().await.is_empty());
    }

    #[tokio::test]
    async fn device_skill_preload_uses_the_same_available_tool_body() {
        use lingxi_core::host::skill_loader::SkillLoader as AgentSkillLoader;
        use lingxi_core::types::ContentBlock;

        let mut registry = CommandRegistry::new();
        register_mobile_device_skills(&mut registry, &tool_names(&["calendar"]));
        let loader =
            MobileDiskSkillLoader::for_mode(Arc::new(RwLock::new(registry)), SessionMode::Chat);
        let loaded = loader
            .resolve_and_load("mobile-personal-context", "general-purpose", None, None)
            .await
            .unwrap()
            .unwrap();
        let [ContentBlock::Text { text, .. }] = loaded.content.as_slice() else {
            panic!("device skill should preload one instruction block");
        };
        assert!(text.contains("## calendar\n"));
        assert!(!text.contains("## contacts\n"));
        assert!(!text.contains("## User Request\n"));
    }

    #[test]
    fn every_tool_has_one_embedded_guidance_section() {
        let mut documented = Vec::new();
        for skill in SKILLS {
            assert!(skill
                .markdown
                .contains(&format!("\nname: {}\n", skill.name)));
            let sections: Vec<_> = skill
                .markdown
                .lines()
                .filter_map(|line| line.strip_prefix("## "))
                .collect();
            let names: Vec<_> = skill.tools.iter().map(|(name, _)| *name).collect();
            assert_eq!(sections, names);
            documented.extend(names);
        }
        documented.sort_unstable();
        let mut expected = ALL_TOOLS.to_vec();
        expected.sort_unstable();
        assert_eq!(documented, expected);
    }
}
