//! Host wiring for inline visualizations: the `/visualize` skill follows the
//! `Visualization` tool, one store per config home, and follow-up prompts.

use std::path::Path;
use std::sync::Arc;

use command_api::{CommandRegistry, SlashCommandKind};
use harness_runtime::inline_visualization::{followup_text, register_skill, shared_store};
use lingxi_core::host::FileSystem;

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn body(registry: &CommandRegistry) -> Option<String> {
    let command = registry.resolve(visualization::skill::SKILL_NAME)?;
    let SlashCommandKind::Bundled {
        prompt_fn: Some(prompt),
        frontmatter,
    } = &command.kind
    else {
        panic!("visualize must be a bundled prompt");
    };
    assert!(
        frontmatter.allowed_tools.is_none(),
        "the skill grants nothing"
    );
    Some(prompt.build(""))
}

#[test]
fn skill_follows_the_tool_and_names_the_host_shell() {
    let mut registry = CommandRegistry::new();
    register_skill(&mut registry, &names(&["Read", "Bash"]), "Bash");
    assert!(body(&registry).is_none(), "no tool, no skill");

    register_skill(&mut registry, &names(&["Visualization", "Bash"]), "Bash");
    assert!(body(&registry).unwrap().contains("with the `Bash` tool"));

    register_skill(&mut registry, &names(&["Visualization", "Shell"]), "Shell");
    assert!(body(&registry).unwrap().contains("with the `Shell` tool"));

    register_skill(&mut registry, &names(&["Visualization"]), "Shell");
    assert!(!body(&registry).unwrap().contains("file_path"));
    assert_eq!(
        registry
            .list_all()
            .iter()
            .filter(|command| command.name == visualization::skill::SKILL_NAME)
            .count(),
        1,
        "re-registration replaces instead of stacking"
    );
}

#[tokio::test]
async fn stores_are_shared_per_config_home_and_follow_ups_snapshot_state() {
    let home = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
        home.path().to_path_buf(),
    ));
    let store = shared_store(fs.clone(), home.path());
    assert!(Arc::ptr_eq(&store, &shared_store(fs.clone(), home.path())));
    assert!(!Arc::ptr_eq(
        &store,
        &shared_store(fs.clone(), Path::new("/tmp/other-home"))
    ));

    let session = lingxi_core::types::SessionId::new();
    let reference = store
        .publish(
            &visualization::Publisher {
                root_session: session.as_uuid(),
                agent_id: None,
            },
            None,
            "Sales",
            "<p>x</p>",
            0,
        )
        .await
        .unwrap()
        .reference;
    store
        .write_state(
            session.as_uuid(),
            &reference,
            0,
            "{\"region\":\"EU\"}",
            "null",
        )
        .await
        .unwrap();
    let dto = client::protocol::message::VisualizationRefDto {
        id: reference.id.as_str().to_string(),
        revision: reference.revision,
    };
    let text = followup_text(
        fs.clone(),
        home.path(),
        &session.to_string(),
        Some(&dto),
        "Why?".to_string(),
    )
    .await;
    let (attachment, typed) = visualization::context::split_context(&text).unwrap();
    assert_eq!((attachment.title.as_str(), typed), ("Sales", "Why?"));
    assert!(text.contains("{\"region\":\"EU\"}"));
    assert_eq!(
        followup_text(
            fs.clone(),
            home.path(),
            &session.to_string(),
            None,
            "plain".into()
        )
        .await,
        "plain"
    );
    let bare = session.as_uuid().to_string();
    assert_ne!(
        followup_text(fs, home.path(), &bare, Some(&dto), "x".into()).await,
        "x",
        "a bare session uuid resolves too"
    );
}
