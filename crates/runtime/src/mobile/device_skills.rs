//! Native mobile workflows exposed through the same live registry as `Skill`.
//!
//! Only tools provided by this host appear in a skill's discovery description
//! and prompt. These prompts grant no additional tool permissions and have no
//! dependency on any separately installed plugin.

use std::path::PathBuf;
use std::sync::Arc;

use command_api::{BundledPromptFn, CommandRegistry, CommandSource, SlashCommandKind};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

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
    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        let args = args
            .string_units("")
            .expect("device skill arguments are text");
        if args
            .iter()
            .all(|unit| char::from_u32(u32::from(*unit)).is_some_and(char::is_whitespace))
        {
            return self.0.clone().into();
        }
        let mut units = self.0.encode_utf16().collect::<Vec<_>>();
        units.extend("\n\n## User Request\n\n".encode_utf16());
        units.extend(args);
        source_from_units(units)
    }
}

fn source_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("device skill source text")
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
    register_mobile_device_skills_with_tools(registry, available_tool_names, None);
}

/// Register mobile device skills while retaining the live media tool catalog
/// for `/media` prompt filtering.
pub(crate) fn register_mobile_device_skills_with_tools(
    registry: &mut CommandRegistry,
    available_tool_names: &[String],
    media_tools: Option<Arc<tool_api::ToolRegistry>>,
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
        let SlashCommandKind::Markdown { frontmatter, .. } = command.kind else {
            unreachable!("embedded device skill is a Markdown prompt")
        };
        let workflows = available
            .iter()
            .map(|(_, description)| *description)
            .collect::<Vec<_>>()
            .join("; ");
        let mut description = command
            .description
            .string_units("")
            .expect("device skill description");
        description.extend(format!(" Available workflows: {workflows}.").encode_utf16());
        command.description = source_from_units(description);
        command.kind = SlashCommandKind::Bundled {
            frontmatter,
            prompt_fn: Some(Arc::new(DevicePrompt(supported_sections(
                &parsed.content,
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
    // `/local-app` follows the `LocalApp` tool the same way.
    crate::local_apps::register_skill(
        registry,
        available_tool_names,
        Some(tool_shell_mobile::TOOL_NAME),
    );
    if let Some(media_tools) = media_tools {
        crate::media_skill::register_media_skill_for_tools(registry, media_tools);
    } else {
        crate::media_skill::register_media_skill(registry, available_tool_names);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile::skill_loader::MobileDiskSkillLoader;
    use session::jsonl::SessionMode;
    use tokio::sync::RwLock;
    use tool_skill::skill::SkillLoader;

    #[test]
    fn device_prompt_preserves_non_scalar_user_request_units() {
        let prompt = DevicePrompt("Instructions".into());
        let args = Utf16JsonProjection::parse(r#""\ud800 request""#).unwrap();
        let output = prompt.build(&args);
        let mut expected = "Instructions\n\n## User Request\n\n"
            .encode_utf16()
            .collect::<Vec<_>>();
        expected.extend(args.string_units("").unwrap());
        assert_eq!(output.string_units(""), Some(expected));
    }

    const ALL_TOOLS: &[&str] = &[
        "media_sources",
        "media_status",
        "media_capture",
        "media_record",
        "media_transcribe",
        "media_synthesize",
        "media_play",
        "media_speak",
        "media_session_start",
        "media_session_update",
        "media_observe",
        "media_wait",
        "media_stop",
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
        assert!(registry.resolve(&"visualize".into()).is_none());
        register_mobile_device_skills(&mut registry, &tool_names(&["Visualization", "Shell"]));
        let registry = Arc::new(RwLock::new(registry));
        for mode in [SessionMode::Chat, SessionMode::Code] {
            let loader = MobileDiskSkillLoader::for_mode(registry.clone(), mode);
            assert_eq!(loader.list_names().await, ["visualize".into()]);
            let descriptor = loader.load(&"visualize".into()).await.unwrap().unwrap();
            assert!(descriptor.allowed_tools.is_empty());
            assert!(descriptor.shell.is_none() && descriptor.context.is_none());
            assert_eq!(descriptor.skip_shell_expansion, mode == SessionMode::Chat);
            assert!(descriptor
                .dynamic_body
                .unwrap()
                .build(&"".into())
                .string_at("")
                .is_some_and(|text| text.contains("with the `Shell` tool")));
        }
    }

    #[tokio::test]
    async fn local_app_skill_follows_the_local_app_tool_in_both_modes() {
        let mut registry = CommandRegistry::new();
        register_mobile_device_skills(&mut registry, &tool_names(&["Read"]));
        assert!(registry.resolve(&"local-app".into()).is_none());
        register_mobile_device_skills(&mut registry, &tool_names(&["LocalApp", "Shell"]));
        let registry = Arc::new(RwLock::new(registry));
        for mode in [SessionMode::Chat, SessionMode::Code] {
            let loader = MobileDiskSkillLoader::for_mode(registry.clone(), mode);
            assert_eq!(loader.list_names().await, ["local-app".into()]);
            let descriptor = loader.load(&"local-app".into()).await.unwrap().unwrap();
            assert!(descriptor.allowed_tools.is_empty());
            assert!(descriptor.shell.is_none() && descriptor.context.is_none());
            assert!(descriptor
                .dynamic_body
                .unwrap()
                .build(&"".into())
                .string_at("")
                .is_some_and(|text| text.contains("with the `Shell` tool")));
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
            names.sort_by_key(|name| name.string_units("").unwrap());
            assert_eq!(
                names
                    .iter()
                    .map(|name| name.string_at("").unwrap())
                    .collect::<Vec<_>>(),
                ["media", "mobile-device", "mobile-personal-context"]
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
                let body = prompt.build(&"".into()).string_at("").unwrap().to_owned();
                assert!(body.starts_with("# "));
                assert!(prompt
                    .build(&"Read my device".into())
                    .string_at("")
                    .unwrap()
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
        assert!(loader.load(&"media".into()).await.unwrap().is_none());
        let device = loader.load(&"mobile-device".into()).await.unwrap().unwrap();
        let body = device
            .dynamic_body
            .unwrap()
            .build(&"".into())
            .string_at("")
            .unwrap()
            .to_owned();
        assert!(device
            .description
            .string_at("")
            .unwrap()
            .contains("clipboard"));
        assert!(!device.description.string_at("").unwrap().contains("URL"));
        assert!(body.contains("## clipboard\n"));
        assert!(!body.contains("## open_url\n"));
        assert!(!body.contains("## notification\n"));
        let personal = loader
            .load(&"mobile-personal-context".into())
            .await
            .unwrap()
            .unwrap();
        let body = personal
            .dynamic_body
            .unwrap()
            .build(&"".into())
            .string_at("")
            .unwrap()
            .to_owned();
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
        let available_tool_names = tool_names(&["calendar"])
            .into_iter()
            .map(|name| lingxi_core::types::utf16_json::Utf16JsonProjection::from(name.as_str()))
            .collect::<Vec<_>>();
        let loader =
            MobileDiskSkillLoader::for_mode(Arc::new(RwLock::new(registry)), SessionMode::Chat);
        let loaded = loader
            .resolve_and_load(
                "mobile-personal-context",
                "general-purpose",
                None,
                None,
                &available_tool_names,
                lingxi_core::host::CancellationToken::new(),
            )
            .await
            .unwrap()
            .unwrap();
        let [ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            ..
        }] = loaded.content.as_slice()
        else {
            panic!("device skill should preload one exact-text instruction block");
        };
        assert_eq!(
            String::from_utf16(utf16_code_units)
                .expect("bundled guidance is well-formed UTF-16")
                .as_str(),
            text.as_str(),
        );
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
        let mut expected = ALL_TOOLS
            .iter()
            .copied()
            .filter(|name| !name.starts_with("media_"))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        assert_eq!(documented, expected);
    }
}
