//! On-demand topics for the compiled-in LingXi product guide.
//!
//! Uses the existing bundled prompt path: no filesystem, network, device
//! probes or privileged host dispatcher are needed to read a topic.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Workflow {
    topic: String,
    page: String,
    section: String,
    queries: Vec<String>,
    #[serde(default)]
    contexts: Vec<String>,
    #[serde(default)]
    refinements: Vec<Refinement>,
}

#[derive(Deserialize)]
struct Refinement {
    section: String,
    qualifiers: Vec<String>,
}

// The website imports this same index from its pinned Harness revision.
static WORKFLOWS: LazyLock<Vec<Workflow>> = LazyLock::new(|| {
    let mut workflows: Vec<Workflow> =
        serde_json::from_str(include_str!("lingxi-api/workflows.json")).expect("bundled workflows");
    for query in workflows
        .iter_mut()
        .flat_map(|workflow| &mut workflow.queries)
    {
        *query = query
            .replace("{{product_name}}", branding::PRODUCT_NAME)
            .replace("{{product_name_zh}}", branding::PRODUCT_NAME_ZH);
    }
    workflows
});

// Keep ASCII words whole and match Han characters with or without surrounding
// whitespace. Padding makes substring searches respect both word boundaries.
fn workflow_key(text: &str) -> String {
    let mut key = String::new();
    let mut in_ascii_word = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        let ascii_word = ch.is_ascii_alphanumeric();
        if ch.is_alphanumeric() || ch == '@' {
            if !(ascii_word && in_ascii_word) {
                key.push(' ');
            }
            key.push(ch);
        }
        in_ascii_word = ascii_word;
    }
    key.push(' ');
    key
}

static WORKFLOW_PHRASES: LazyLock<Vec<(String, &Workflow)>> = LazyLock::new(|| {
    let mut phrases: Vec<_> = WORKFLOWS
        .iter()
        .flat_map(|workflow| {
            workflow
                .queries
                .iter()
                .map(move |q| (workflow_key(q), workflow))
        })
        .collect();
    // Stable ties follow the shared index order on both surfaces.
    phrases.sort_by_cached_key(|(key, _)| std::cmp::Reverse(key.chars().count()));
    phrases
});

fn workflow_for_question(units: &[u16]) -> Option<&'static Workflow> {
    let question = String::from_utf16(units).ok()?;
    let first_word = question.split_whitespace().next()?;
    if first_word.starts_with(['/', '\\', '.'])
        || question.contains("://")
        || question.contains("/../")
    {
        return None;
    }
    let query = workflow_key(&question);
    let direct = WORKFLOW_PHRASES
        .iter()
        .find(|(phrase, _)| query.contains(phrase))
        .map(|(_, workflow)| *workflow);
    // A bare product name only provides context for a more specific action.
    let workflow = direct.or_else(|| {
        WORKFLOWS.iter().find(|workflow| {
            workflow
                .contexts
                .iter()
                .any(|context| query.contains(&workflow_key(context)))
        })
    })?;
    for refinement in &workflow.refinements {
        if refinement
            .qualifiers
            .iter()
            .any(|q| query.contains(&workflow_key(q)))
        {
            return WORKFLOWS
                .iter()
                .find(|item| item.page == workflow.page && item.section == refinement.section);
        }
    }
    direct
}

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
    topic!("friends"),
    topic!("datasets"),
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

pub(super) fn guide_context() -> Utf16JsonProjection {
    // The read-only guide has its own instructions and no Skill tool. Give it
    // the references directly, without the interactive Skill invocation menu.
    let mut units = Vec::new();
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
    let selector = selector.as_deref().unwrap_or("").to_ascii_lowercase();
    let canonical = match selector.as_str() {
        "team" | "channel" | "channels" | "频道" => "teams",
        "friend" | "好友" => "friends",
        "bot" => "bots",
        "dataset" | "数据集" => "datasets",
        "remote-control" => "webrtc",
        "local-apps" => "local-app",
        other => other,
    };
    let natural = workflow_for_question(args);
    // A canonical topic is explicit. A leading alias can be the first word
    // of a question, e.g. "bot dm" or "Bot 自主回复".
    let selected = TOPICS
        .iter()
        .find(|topic| topic.id == canonical)
        .filter(|_| {
            selector == canonical
                || matches!(
                    selector.as_str(),
                    "remote-control" | "local-apps" | "dataset" | "数据集" | "好友" | "频道"
                )
                || natural.is_none()
                || args[end..].iter().all(|unit| whitespace(*unit))
        });
    let workflow =
        natural.filter(|workflow| selected.is_none_or(|topic| topic.id == workflow.topic));
    let topic = selected.or_else(|| {
        let workflow = workflow?;
        TOPICS.iter().find(|topic| topic.id == workflow.topic)
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
        if selected.is_some() {
            &args[end..]
        } else {
            args
        }
    } else {
        units.extend("\n\nNo bundled topic matched the first argument. Use the topic table above to select the relevant reference.\n".encode_utf16());
        args
    };
    if let Some(workflow) = workflow {
        units.extend(
            format!(
                "\n\nHandbook section: `/docs/{}#{}`",
                workflow.page, workflow.section
            )
            .encode_utf16(),
        );
    }
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
    fn dataset_topic_loads_in_english_and_chinese() {
        for args in ["datasets", "dataset how do I write?", "数据集 如何撤权？"] {
            let result = render(body(), &args.into());
            let text = result.string_at("").unwrap();
            assert!(text.contains("# Datasets / 数据集"), "{args}");
            assert!(text.contains("fixed/latest/edit grants"), "{args}");
        }
    }

    #[test]
    fn friend_and_channel_selectors_load_their_topics() {
        for (args, topic, fact) in [
            ("friends", "friends", "**Let people find me by email**"),
            ("friend how do I add someone?", "friends", "My friend link"),
            ("好友 如何拉黑？", "friends", "Blocked people"),
            ("channel", "teams", "New channel"),
            ("频道 如何创建？", "teams", "Create channel"),
        ] {
            let result = render(body(), &args.into());
            let text = result.string_at("").unwrap();
            assert!(
                text.contains(&format!("## Bundled resource: `references/{topic}.md`")),
                "{args}"
            );
            assert_eq!(text.matches("## Bundled resource:").count(), 1, "{args}");
            assert!(text.contains(fact), "{args} missing {fact}");
        }
    }

    #[test]
    fn product_topics_state_the_shipped_controls() {
        for (topic, facts) in [
            (
                "teams",
                &[
                    "**Make owner**",
                    "1–50 uses",
                    "200 members",
                    "Invite friends",
                    "Recall",
                ][..],
            ),
            (
                "friends",
                &[
                    "without your approval",
                    "Unblocking does not restore the friendship",
                    "one Bot DM per",
                    "**Team invitations**",
                    "500",
                ][..],
            ),
            (
                "bots",
                &[
                    "**Publish a",
                    "**All current and future members**",
                    "UTC",
                    "**Take",
                    "**Bot Dataset permission**",
                ][..],
            ),
            (
                "datasets",
                &[
                    "**Use fixed version**",
                    "**See latest**",
                    "**Edit together**",
                    "`dataBindings`",
                    "**Data sources**",
                    "**Bind Dataset**",
                ][..],
            ),
        ] {
            let result = render(body(), &topic.into());
            let text = result.string_at("").unwrap();
            for fact in facts {
                assert!(text.contains(fact), "{topic} missing {fact}");
            }
        }
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
        let context = guide_context();
        let text = context.string_at("").unwrap();
        assert_eq!(text.matches("## Bundled resource:").count(), TOPICS.len());
        for topic in TOPICS {
            assert!(text.contains(&branding::render_namespace_template(topic.content)));
        }
        // The public handbook facts live in these same topics. Keep the
        // isolated guide under one bounded prompt (about 20k tokens).
        let chars = text.chars().count();
        assert!(chars < 76_000, "guide context grew to {chars} characters");
    }

    #[test]
    fn every_resource_uses_the_injected_brand_namespace() {
        for topic in TOPICS {
            let text = branding::render_namespace_template(topic.content);
            assert!(!text.contains("{{"), "unresolved namespace in {}", topic.id);
            assert!(
                !topic.content.contains(branding::DOT_DIR),
                "hardcoded brand path in {}",
                topic.id
            );
        }
        let context = guide_context();
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
    fn product_workflows_and_legacy_selectors_load_one_offline_topic() {
        for (question, topic) in [
            ("team", "teams"),
            ("BOT", "bots"),
            ("remote-control", "webrtc"),
            ("webrtc", "webrtc"),
            ("如何创建 team？", "teams"),
            ("如何开启 Bot 自主判断是否回复？", "teams"),
            ("不需要 @ 也能自动回复吗？", "teams"),
            ("how do I create a DM?", "teams"),
            ("如何发送 message？", "teams"),
            ("怎么把 Bot 分享给团队", "teams"),
            ("手机继续电脑上的会话", "webrtc"),
            ("怎么共享 Local App 给同事", "local-app"),
            ("How do I publish a local app?", "local-app"),
        ] {
            let prompt = render(body(), &question.into());
            let text = prompt.string_at("").unwrap();
            assert!(
                text.contains(&format!("## Bundled resource: `references/{topic}.md`")),
                "{question}"
            );
            assert_eq!(text.matches("## Bundled resource:").count(), 1);
            if question.contains('？') || question.ends_with('?') {
                assert!(text.ends_with(question));
            }
        }
        for workflow in WORKFLOWS.iter() {
            assert!(TOPICS.iter().any(|topic| topic.id == workflow.topic));
        }
        let decision = render(body(), &"不需要 @ 也能自主回复吗？".into());
        assert!(decision
            .string_at("")
            .unwrap()
            .contains("/docs/teams#auto-reply"));
    }

    #[test]
    fn bot_creation_workflows_use_both_product_names() {
        for question in [
            format!("How do I create a {} bot?", branding::PRODUCT_NAME),
            format!("如何创建{} Bot？", branding::PRODUCT_NAME_ZH),
        ] {
            let workflow = workflow_for_question(&question.encode_utf16().collect::<Vec<_>>())
                .expect("branded bot creation question");
            assert_eq!(
                (workflow.topic.as_str(), workflow.section.as_str()),
                ("bots", "create")
            );
        }
    }

    #[test]
    fn every_workflow_question_opens_its_registered_topic_and_section() {
        for workflow in WORKFLOWS.iter() {
            for question in &workflow.queries {
                let prompt =
                    crate::desktop_builtin_skill_prompt("lingxi-api", &question.as_str().into())
                        .expect("registered skill");
                let text = prompt.string_at("").unwrap();
                assert_eq!(
                    text.matches("## Bundled resource:").count(),
                    1,
                    "{question}"
                );
                assert!(
                    text.contains(&format!(
                        "## Bundled resource: `references/{}.md`",
                        workflow.topic
                    )),
                    "{question}"
                );
                assert!(
                    text.contains(&format!(
                        "Handbook section: `/docs/{}#{}`",
                        workflow.page, workflow.section
                    )),
                    "{question}"
                );
            }
        }
    }

    #[test]
    fn ordinary_questions_preserve_the_request_and_route_by_intent() {
        for (question, section) in [
            ("如何发送团队消息", "teams#send-message"),
            ("如何创建一个团队？", "teams#create-team"),
            ("Bot 自主回复", "teams#auto-reply"),
            (
                "如何配置团队里的 Bot 自主回复，让它在没有被 @ 的时候也能自己判断要不要回答？",
                "teams#auto-reply",
            ),
            ("Bot DM?", "teams#bot-chat"),
            ("创建群聊并发送消息", "teams#group-chat"),
            (
                "How do I publish a local app to the market?",
                "local-apps#market",
            ),
            (
                "Can I publish my local app directly to the app market?",
                "local-apps#market",
            ),
            ("如何把 Local App 发布到市场？", "local-apps#market"),
            ("如何发布 Local App 到应用市场？", "local-apps#market"),
            (
                "publish a local app on my phone",
                "local-apps#publish-local",
            ),
            ("怎么私下分享 Local App 给朋友？", "local-apps#source-zip"),
            (
                "如何把我做的 Local App 共享给一位同事？",
                "local-apps#source-zip",
            ),
            ("我只在 iPhone 上有这个 Local App，怎么只发给一个同事？如果后来决定让大家都能安装，要怎么操作？", "local-apps#source-zip"),
            ("我的手机通过远控打开电脑上的应用，我昨天记的数据这次还能继续用吗？", "local-apps#paired-devices"),
        ] {
            let prompt = render(body(), &question.into());
            let text = prompt.string_at("").unwrap();
            assert!(
                text.contains(&format!("Handbook section: `/docs/{section}`")),
                "{question}"
            );
            assert!(text.ends_with(question), "{question}");
        }
        for question in [
            "create admin account",
            "How do I create a chatbot?",
            "local app SDK protocol",
        ] {
            assert!(
                !render(body(), &question.into())
                    .string_at("")
                    .unwrap()
                    .contains("Handbook section:"),
                "{question}"
            );
        }
        let explicit = render(body(), &"bots 如何发送团队消息".into());
        let text = explicit.string_at("").unwrap();
        assert!(text.contains("## Bundled resource: `references/bots.md`"));
        assert!(!text.contains("Handbook section:"));
        assert!(text.ends_with("如何发送团队消息"));
    }

    #[test]
    fn local_app_guide_exposes_the_user_controls_for_each_sharing_route() {
        let prompt =
            crate::desktop_builtin_skill_prompt("lingxi-api", &"share a local app".into()).unwrap();
        let text = prompt.string_at("").unwrap();
        let hidden = text.find("starts hidden").unwrap();
        let public = text.find("**Make public**").unwrap();
        assert!(hidden < public);
        for control in [
            "iOS",
            "Android",
            "The **Export source ZIP** entry is available only on iOS and Android.",
            "source ZIP",
            "Import source",
            "localStorage",
            "IndexedDB",
        ] {
            assert!(text.contains(control), "missing {control}");
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
        let context = guide_context();
        let text = context.string_at("").unwrap();
        for fact in [
            branding::DISABLE_AGENT_VIEW_ENV,
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
            configuration_text
                .contains("then `settings.local.json`, then `--settings`, then managed policy"),
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
