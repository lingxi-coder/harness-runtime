//! On-demand topics for the compiled-in LingXi product guide.
//!
//! Uses the existing bundled prompt path: no filesystem, network, device
//! probes or privileged host dispatcher are needed to read a topic.

use lingxi_core::types::utf16_json::Utf16JsonProjection;

struct Topic {
    id: &'static str,
    path: &'static str,
    content: &'static str,
}

macro_rules! topic {
    ($id:literal) => {
        Topic {
            id: $id,
            path: concat!("references/", $id, ".md"),
            content: include_str!(concat!("lingxi-api/references/", $id, ".md")),
        }
    };
}

const TOPICS: &[Topic] = &[
    topic!("overview"),
    topic!("accounts"),
    topic!("teams"),
    topic!("configuration"),
    topic!("media"),
    topic!("webrtc"),
    topic!("local-app"),
    topic!("bots"),
    topic!("devices"),
    topic!("mobile-linux"),
    topic!("sessions"),
    topic!("automation"),
    topic!("agents"),
    topic!("models"),
    topic!("integrations"),
    topic!("troubleshooting"),
];

fn whitespace(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(char::is_whitespace)
}

pub(super) fn guide_context(body: Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = body.string_units("").expect("builtin guide is text");
    for topic in TOPICS {
        units.extend(
            format!(
                "\n\n---\n\n## Bundled resource: `{}`\n\n{}",
                topic.path,
                branding::render_namespace_template(topic.content)
            )
            .encode_utf16(),
        );
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("compiled reference context is text")
}

pub(super) fn render(body: Utf16JsonProjection, args: &Utf16JsonProjection) -> Utf16JsonProjection {
    let args = args.string_units("").unwrap_or_default();
    if args.is_empty() || args.iter().all(|unit| whitespace(*unit)) {
        return body;
    }
    let start = args.iter().position(|unit| !whitespace(*unit)).unwrap_or(0);
    let args = &args[start..];
    let end = args
        .iter()
        .position(|unit| whitespace(*unit))
        .unwrap_or(args.len());
    let selector = String::from_utf16(&args[..end]).ok();
    let topic = TOPICS.iter().find(|topic| {
        selector
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(topic.id))
    });
    let mut units = body.string_units("").expect("bundled guide body is text");
    let request = if let Some(topic) = topic {
        units.extend(
            format!(
                "\n\n---\n\n## Bundled resource: `{}`\n\n{}",
                topic.path,
                branding::render_namespace_template(topic.content)
            )
            .encode_utf16(),
        );
        &args[end..]
    } else {
        units.extend("\n\nNo bundled topic matched the first argument. Use the topic table above to select the relevant reference.\n".encode_utf16());
        args
    };
    if request.iter().any(|unit| !whitespace(*unit)) {
        units.extend("\n\n## User Request\n\n".encode_utf16());
        units.extend(request);
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("bundled prompt preserves source text")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Utf16JsonProjection {
        super::super::desktop_builtin_skill_content("lingxi-api").unwrap()
    }

    #[test]
    fn empty_arguments_load_only_the_entrypoint() {
        for args in ["", " \t\n", "\u{3000}"] {
            assert_eq!(render(body(), &args.into()), body());
        }
    }

    #[test]
    fn every_linked_topic_is_embedded_and_loads_alone() {
        let root = body();
        let root_text = root.string_at("").unwrap();
        for topic in TOPICS {
            assert!(
                root_text.contains(&format!("]({})", topic.path)),
                "{} is reachable",
                topic.id
            );
            let prompt = render(root.clone(), &topic.id.into());
            let text = prompt.string_at("").unwrap();
            assert!(
                text.contains(&branding::render_namespace_template(topic.content)),
                "{} loads offline",
                topic.id
            );
            assert_eq!(text.matches("## Bundled resource:").count(), 1);
            for other in TOPICS.iter().filter(|other| other.id != topic.id) {
                assert!(
                    !text.contains(&branding::render_namespace_template(other.content)),
                    "{} did not preload {}",
                    topic.id,
                    other.id
                );
            }
            assert!(
                text.chars().count() < 14_000,
                "{} remains bounded",
                topic.id
            );
        }
    }

    #[test]
    fn isolated_guide_agent_context_reuses_the_same_topic_sources() {
        let context = guide_context(body());
        let text = context.string_at("").unwrap();
        assert_eq!(text.matches("## Bundled resource:").count(), TOPICS.len());
        for topic in TOPICS {
            assert!(text.contains(&branding::render_namespace_template(topic.content)));
        }
        // The public handbook facts live in these same topics. Keep the
        // isolated guide under one bounded prompt.
        assert!(text.chars().count() < 56_000, "guide context grew past 56k");
    }

    #[test]
    fn every_resource_uses_the_injected_brand_namespace() {
        for topic in TOPICS {
            let text = branding::render_namespace_template(topic.content);
            assert!(!text.contains("{{"), "unresolved namespace in {}", topic.id);
            assert!(
                !topic.content.contains(".lingxi"),
                "hardcoded brand path in {}",
                topic.id
            );
        }
        let context = guide_context(body());
        assert!(!context.string_at("").unwrap().contains("{{"));
    }

    #[test]
    fn topic_selection_is_case_insensitive_and_preserves_question() {
        let prompt = render(body(), &"\u{3000}MEDIA\t请解释实时指导".into());
        let text = prompt.string_at("").unwrap();
        assert!(text.contains(
            TOPICS
                .iter()
                .find(|topic| topic.id == "media")
                .unwrap()
                .content
        ));
        assert!(text.ends_with("\t请解释实时指导"));
    }

    #[test]
    fn an_unknown_topic_cannot_read_a_path_or_url() {
        for request in [
            "../../settings.json",
            "https://example.com",
            "media/../bots",
        ] {
            let prompt = render(body(), &request.into());
            let text = prompt.string_at("").unwrap();
            assert!(!text.contains("## Bundled resource:"));
            assert!(text.ends_with(request));
        }
    }

    #[test]
    fn guide_answers_shipped_behavior_that_claude_priors_get_wrong() {
        let root = body();
        let root_text = root.string_at("").unwrap();
        assert!(
            root_text.contains("Read the matching topic before answering"),
            "entrypoint must route before answering from memory"
        );
        let context = guide_context(root.clone());
        let text = context.string_at("").unwrap();
        for fact in [
            "LINGXI_DISABLE_AGENT_VIEW",
            "runs only after the first assistant turn",
            "`/subtask` is not registered",
            "`lingxi agents --json` prints that listing",
            "add-from-claude-desktop",
            "--disable-slash-commands",
            "then `settings.local.json`, then `--settings`, then managed policy",
            "They cannot grant `auto` or `bypassPermissions`.",
            "User settings, `--settings`, and managed policy can.",
            "`--bare` and `--safe-mode` both disable settings hooks, plugins, skills, and custom agents.",
            "`--bare` skips instruction auto-discovery unless `--add-dir` is set",
            "still runs managed policy hooks",
            "ends at the git root when that root is below the home directory, and at the home directory when there is no such root",
        ] {
            assert!(text.contains(fact), "guide missing {fact}");
        }
        let agents = render(root.clone(), &"agents".into());
        let agents_text = agents.string_at("").unwrap();
        assert!(
            agents_text.contains("runs only after the first assistant turn"),
            "agents topic missing the /subtask first-turn gate"
        );
        assert!(
            agents_text.contains("`/subtask` is not registered"),
            "agents topic missing the agent-view-off /subtask absence"
        );
        assert!(
            agents_text.contains("`lingxi agents --json` prints that listing"),
            "agents topic missing the non-interactive agents listing"
        );
        assert!(
            !agents_text.contains("is not that command"),
            "agents topic still describes /subtask as some other command"
        );
        let configuration = render(root, &"configuration".into());
        let configuration_text = configuration.string_at("").unwrap();
        assert!(
            configuration_text.contains(
                "then `settings.local.json`, then `--settings`, then managed policy"
            ),
            "configuration topic missing the boot permission fold"
        );
        assert!(
            configuration_text.contains("They cannot grant `auto` or `bypassPermissions`."),
            "configuration topic missing the project and local grant limit"
        );
        assert!(
            configuration_text.contains("User settings, `--settings`, and managed policy can."),
            "configuration topic missing the trusted permission grantors"
        );
        assert!(
            configuration_text.contains(
                "`--bare` and `--safe-mode` both disable settings hooks, plugins, skills, and custom agents."
            ),
            "configuration topic missing the shared bare and safe-mode disables"
        );
        assert!(
            configuration_text.contains("still runs managed policy hooks"),
            "configuration topic missing the safe-mode policy-hook exception"
        );
        assert!(
            configuration_text.contains(
                "ends at the git root when that root is below the home directory, and at the home directory when there is no such root"
            ),
            "configuration topic missing the skill-walk bound"
        );
        assert!(
            !configuration_text.contains("permission-mode reader does not load"),
            "configuration topic still denies that settings.local.json can set the mode"
        );
        assert!(
            !configuration_text.contains("Only the user tier can grant"),
            "configuration topic still hides flag and managed grants"
        );
    }

    #[test]
    fn question_keeps_exact_utf16_units() {
        let mut question = "media ".encode_utf16().collect::<Vec<_>>();
        question.extend([0xd800, 0x0000, 0xdc00]);
        let args =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&question), question.clone())
                .unwrap();
        let prompt = render(body(), &args);
        assert!(prompt.string_units("").unwrap().ends_with(&question[5..]));
    }
}
