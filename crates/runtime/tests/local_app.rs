//! Host wiring for local apps: the `/local-app` skill follows the `LocalApp`
//! tool, one library per config home, and the mobile UniFFI host lists,
//! serves and deletes what the agent publishes.

use std::path::Path;
use std::sync::Arc;

use command_api::{CommandRegistry, SlashCommandKind};
use harness_runtime::local_apps::{register_skill, shared_store};
use harness_runtime::mobile::local_app_host::LocalAppHost;
use lingxi_core::host::FileSystem;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn body(registry: &CommandRegistry) -> Option<Utf16JsonProjection> {
    let name = Utf16JsonProjection::from(local_app::skill::SKILL_NAME);
    let command = registry.resolve(&name)?;
    let SlashCommandKind::Bundled {
        prompt_fn: Some(prompt),
        frontmatter,
    } = &command.kind
    else {
        panic!("local-app must be a bundled prompt");
    };
    assert!(
        frontmatter.allowed_tools.is_none(),
        "the skill grants nothing"
    );
    Some(prompt.build(&Utf16JsonProjection::from("")))
}

#[test]
fn skill_follows_the_tool_and_names_the_host_shell() {
    let mut registry = CommandRegistry::new();
    register_skill(&mut registry, &names(&["Read", "Bash"]), "Bash");
    assert!(body(&registry).is_none(), "no tool, no skill");

    register_skill(&mut registry, &names(&["LocalApp", "Bash"]), "Bash");
    assert!(body(&registry)
        .unwrap()
        .string_at("")
        .unwrap()
        .contains("with the `Bash` tool"));

    register_skill(&mut registry, &names(&["LocalApp"]), "Shell");
    assert!(!body(&registry)
        .unwrap()
        .string_at("")
        .unwrap()
        .contains("file_path"));
    assert_eq!(
        registry
            .list_all()
            .iter()
            .filter(|command| { command.name.string_at("") == Some(local_app::skill::SKILL_NAME) })
            .count(),
        1,
        "re-registration replaces instead of stacking"
    );

    register_skill(&mut registry, &names(&["Read"]), "Shell");
    assert!(body(&registry).is_none(), "the skill leaves with the tool");
}

#[test]
fn the_mobile_host_lists_serves_and_deletes_the_shared_library() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
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

    let host = LocalAppHost::for_config_home(fs, home.path(), runtime.handle().clone());
    assert!(host.list().is_empty());
    let published = runtime
        .block_on(store.publish(
            None,
            Some("Water"),
            Some("Log glasses"),
            "<!doctype html><title>Water</title>",
            7,
        ))
        .unwrap();
    let id = published.id.as_str().to_string();

    let listed = host.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            listed[0].id.as_str(),
            listed[0].name.as_str(),
            listed[0].revision
        ),
        (id.as_str(), "Water", 1)
    );
    assert_eq!(host.get(id.clone()).unwrap().summary, "Log glasses");

    let document = host.serve(id.clone(), "/".into());
    assert_eq!(
        (document.status, document.mime_type.as_str()),
        (200, "text/html")
    );
    assert_eq!(document.body, b"<!doctype html><title>Water</title>");
    assert!(document
        .headers
        .iter()
        .any(|header| header.name == "Content-Security-Policy"
            && header.value.starts_with("sandbox allow-scripts")));
    assert_eq!(host.serve(id.clone(), "/app.js".into()).status, 404);
    assert_eq!(host.serve("NOT-AN-ID".into(), "/".into()).status, 404);

    assert!(host.delete(id.clone()));
    assert!(!host.delete(id.clone()));
    assert!(host.get(id.clone()).is_none());
    assert_eq!(host.serve(id, "/index.html".into()).status, 404);
    assert!(host.list().is_empty());
}
