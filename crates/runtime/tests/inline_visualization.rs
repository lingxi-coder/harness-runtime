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

#[tokio::test]
async fn desktop_requests_mount_serve_and_write_by_op() {
    use harness_runtime::desktop_visualization::DesktopVisualizationHost;
    use serde_json::{json, Value};

    let home = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
        home.path().to_path_buf(),
    ));
    let session = lingxi_core::types::SessionId::new();
    let reference = shared_store(fs.clone(), home.path())
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
    let host = DesktopVisualizationHost::for_config_home(fs, home.path());
    let mount = |session_id: String, id: &str| {
        json!({
            "op": "mount", "session_id": session_id, "id": id, "revision": reference.revision,
            "theme": { "dark": true, "tokens": {} }, "locale": "en", "expanded": false,
        })
    };

    let ticket = host
        .handle(mount(session.as_uuid().to_string(), reference.id.as_str()))
        .await
        .unwrap();
    assert_eq!(ticket["title"], "Sales");
    let token = ticket["token"].as_str().unwrap().to_string();
    let doc_url = ticket["doc_url"].as_str().unwrap();
    let path = doc_url.strip_prefix("lingxi-viz://visualization").unwrap();
    assert_eq!(
        host.handle(mount(
            lingxi_core::types::SessionId::new().to_string(),
            reference.id.as_str()
        ))
        .await
        .unwrap(),
        Value::Null,
        "another conversation cannot mount this revision"
    );

    let served = host
        .handle(json!({ "op": "serve", "path": path }))
        .await
        .unwrap();
    assert_eq!(served["status"], 200);
    assert!(served["headers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|header| header[0] == "Content-Security-Policy"));
    let again = host
        .handle(json!({ "op": "serve", "path": path }))
        .await
        .unwrap();
    assert_eq!(again["status"], 404, "documents are single use");

    let write = |base: u64| {
        json!({
            "op": "write_state", "token": token, "generation": ticket["generation"], "base_version": base,
            "model_content": "{\"k\":1}", "private_content": "null",
        })
    };
    assert_eq!(
        host.handle(write(0)).await.unwrap(),
        json!({ "saved": true, "version": 1 })
    );
    let conflict = host.handle(write(0)).await.unwrap();
    assert_eq!(
        (conflict["reason"].as_str(), conflict["version"].as_u64()),
        (Some("conflict"), Some(1))
    );
    assert_eq!(conflict["current_state"]["modelContent"], json!({ "k": 1 }));

    let listed = host
        .handle(json!({ "op": "list", "session_id": session.to_string() }))
        .await
        .unwrap();
    assert_eq!(listed[0]["title"], "Sales");
    assert!(host
        .handle(json!({ "op": "notices" }))
        .await
        .unwrap()
        .is_string());

    host.handle(json!({ "op": "unmount", "token": token }))
        .await
        .unwrap();
    assert_eq!(
        host.handle(write(1)).await.unwrap()["reason"],
        "stale_mount"
    );
    assert!(host
        .handle(json!({ "op": "delete_everything" }))
        .await
        .is_err());
    assert!(host
        .handle(json!({ "op": "unmount", "token": "t", "extra": 1 }))
        .await
        .is_err());
}

#[test]
fn the_sweep_removes_only_old_directories_no_transcript_names() {
    use harness_runtime::inline_visualization::{
        catalog_sessions, sweep_orphans, ORPHAN_RETENTION,
    };
    let home = tempfile::tempdir().unwrap();
    let (live, archived, orphan) = (
        uuid::Uuid::from_u128(0x0001),
        uuid::Uuid::from_u128(0x0002),
        uuid::Uuid::from_u128(0x0003),
    );
    for session in [live, archived, orphan] {
        let dir = home.path().join("visualizations").join(session.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.json"), "{}").unwrap();
    }
    let catalog = home.path().join("projects").join("-work");
    std::fs::create_dir_all(&catalog).unwrap();
    std::fs::write(catalog.join(format!("{live}.jsonl")), "").unwrap();
    std::fs::write(catalog.join("notes.txt"), "").unwrap();
    let rollouts = home.path().join("archived_sessions");
    std::fs::create_dir_all(&rollouts).unwrap();
    std::fs::write(
        rollouts.join(format!("rollout-2026-01-01T00-00-00-{archived}.jsonl")),
        "",
    )
    .unwrap();

    assert_eq!(catalog_sessions(&catalog), vec![live]);

    let now = std::time::SystemTime::now();
    assert_eq!(
        sweep_orphans(home.path(), ORPHAN_RETENTION, now),
        0,
        "fresh orphans are kept"
    );
    let later = now + ORPHAN_RETENTION + std::time::Duration::from_secs(60);
    assert_eq!(sweep_orphans(home.path(), ORPHAN_RETENTION, later), 1);
    let root = home.path().join("visualizations");
    assert!(root.join(live.to_string()).exists());
    assert!(root.join(archived.to_string()).exists());
    assert!(!root.join(orphan.to_string()).exists());
}
