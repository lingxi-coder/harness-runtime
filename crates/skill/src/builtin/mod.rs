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
        "create-bot" | "document-authoring" | "artifact-design" | "plan-artifact" => {
            with_user_request(body, args)
        }
        _ => body,
    })
}

/// Append a non-blank slash argument as text. Blank arguments keep the body.
fn with_user_request(body: Utf16JsonProjection, args: &Utf16JsonProjection) -> Utf16JsonProjection {
    let request = args.string_units("").unwrap_or_default();
    if request
        .iter()
        .all(|unit| char::from_u32(u32::from(*unit)).is_some_and(char::is_whitespace))
    {
        return body;
    }
    let mut units = body.string_units("").expect("builtin prompt is text");
    units.extend("\n\n## User Request\n\n".encode_utf16());
    units.extend(request);
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("builtin prompt preserves request source")
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
    let rendered = if entry.name == "lingxi-api" {
        branding::render_namespace_template(entry.raw)
    } else {
        entry.raw.to_owned()
    };
    let mut skill = parse_skill_markdown(
        &rendered,
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
        assert_eq!(
            names,
            vec![
                "artifact-design",
                "create-bot",
                "document-authoring",
                "lingxi-api",
                "plan-artifact",
                "plugin-authoring"
            ]
        );
        assert!(r.get("docs").is_none());
        assert!(r.get("claude-api").is_none());
        let skill = r.get("lingxi-api").expect("registered bundled skill");
        assert_eq!(skill.source, SkillSource::Bundled);
        assert_eq!(skill.loaded_from, LoadedFrom::Bundled);
        assert_eq!(skill.frontmatter.name, "lingxi-api");
        assert!(skill
            .description
            .string_at("")
            .is_some_and(|description| description.contains(&format!(
                "{} ({})",
                branding::PRODUCT_NAME,
                branding::PRODUCT_NAME_ZH
            ))));
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
            "/lingxi-api media".to_owned(),
            "Configure LingXi MCP".to_owned(),
            format!("{}的 Bot 如何使用", branding::PRODUCT_NAME_ZH),
        ] {
            assert!(r
                .discover(&query)
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
    fn bot_skill_is_discoverable_and_preserves_the_request() {
        let mut registry = SkillRegistry::new();
        register_desktop(&mut registry);
        for query in ["/create-bot", "create a bot", "创建 bot"] {
            assert!(registry
                .discover(query)
                .iter()
                .any(|skill| skill.name == "create-bot"));
        }
        let request = "帮我创建一个写作助手，保留 !`literal text`。";
        let prompt = desktop_builtin_skill_prompt("create-bot", &request.into()).unwrap();
        assert!(prompt.string_at("").unwrap().ends_with(request));
        assert_eq!(
            desktop_builtin_skill_prompt("create-bot", &"".into()),
            desktop_builtin_skill_content("create-bot")
        );
    }

    #[test]
    fn document_skill_preserves_literal_request_and_has_no_connector_dependency() {
        let root = desktop_builtin_skill_content("document-authoring").unwrap();
        assert_eq!(
            desktop_builtin_skill_prompt("document-authoring", &"".into()).unwrap(),
            root
        );
        let request = "Update the existing report; keep !`literal example` as text.";
        let prompt = desktop_builtin_skill_prompt("document-authoring", &request.into()).unwrap();
        assert!(prompt.string_at("").unwrap().ends_with(request));
        let mut r = SkillRegistry::new();
        register_desktop(&mut r);
        for query in ["/document-authoring", "create a document", "创建文档"] {
            assert!(r
                .discover(query)
                .iter()
                .any(|skill| skill.name == "document-authoring"));
        }
        assert!(!r
            .discover("Explain OpenAI API docs")
            .iter()
            .any(|skill| skill.name == "document-authoring"));
        let text = root.string_at("").unwrap();
        assert!(!text.contains("Artifact"));
        assert!(text.contains("`Visualization` tool"));
        assert!(text.contains("tool is registered"));
        assert!(text.contains("stays in this conversation"));
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

    #[test]
    fn hosted_page_skills_point_at_page_and_leave_visualization_alone() {
        let mut registry = SkillRegistry::new();
        register_desktop(&mut registry);
        for (name, query) in [
            ("artifact-design", "/artifact-design"),
            ("artifact-design", "publish an artifact"),
            ("artifact-design", "hosted web page"),
            ("artifact-design", "发布成网页"),
            ("plan-artifact", "/plan-artifact"),
            ("plan-artifact", "plan as an artifact"),
            ("plan-artifact", "publish the plan as a page"),
            ("plan-artifact", "把计划发布成页面"),
        ] {
            assert!(
                registry
                    .discover(query)
                    .iter()
                    .any(|skill| skill.name == name),
                "{query} discovers {name}"
            );
        }
        for query in [
            "Explain OpenAI API docs",
            "write a report",
            "show an inline chart",
            "create a document",
        ] {
            let names: Vec<_> = registry
                .discover(query)
                .iter()
                .map(|skill| skill.name.as_str())
                .collect();
            assert!(
                !names.contains(&"artifact-design") && !names.contains(&"plan-artifact"),
                "{query} discovered {names:?}"
            );
        }
        for name in ["artifact-design", "plan-artifact"] {
            let body = desktop_builtin_skill_content(name).unwrap();
            let request = "Publish this; keep !`literal example` as text.";
            assert_eq!(
                desktop_builtin_skill_prompt(name, &"".into()).unwrap(),
                body
            );
            let prompt = desktop_builtin_skill_prompt(name, &request.into()).unwrap();
            assert!(prompt.string_at("").unwrap().ends_with(request));
            let text = body.string_at("").unwrap();
            assert!(text.contains("Publish a private page on this computer with the `Page` tool"));
            assert!(text.contains("There is no public URL."));
            assert!(text.contains(
                "If `Page` is not among the tools, say this host cannot open a private page."
            ));
            assert!(!text.contains("Visualization"));
            assert!(!text.contains("visualize"));
            assert!(!text.contains("LocalApp"));
            assert!(!text.contains("local-app"));
            assert!(!text.contains("`Artifact`"));
            assert!(!text.to_ascii_lowercase().contains("claude"));
            assert!(!text.contains("{{"));
            assert!(!text.contains("!`"));
            assert!(!text.contains("```!"));
            let description = registry
                .get(name)
                .unwrap()
                .description
                .string_at("")
                .unwrap();
            assert!(description.starts_with("Use when "));
            assert!(description.chars().count() <= 180, "{description}");
        }
        let plan_body = desktop_builtin_skill_content("plan-artifact").unwrap();
        let plan = plan_body.string_at("").unwrap();
        assert!(plan.contains("ExitPlanMode"));
        assert!(plan.contains("no plan-share command"));
        assert!(!plan.contains("/plan share"));
    }
}
