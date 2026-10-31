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
                topic.path, topic.content
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
                topic.path, topic.content
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
            assert!(text.contains(topic.content), "{} loads offline", topic.id);
            assert_eq!(text.matches("## Bundled resource:").count(), 1);
            for other in TOPICS.iter().filter(|other| other.id != topic.id) {
                assert!(
                    !text.contains(other.content),
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
            assert!(text.contains(topic.content));
        }
        assert!(text.chars().count() < 50_000);
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
