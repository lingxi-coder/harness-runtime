//! Host wiring for local apps: the `/local-app` skill follows the `LocalApp`
//! tool, one library per config home, and the mobile UniFFI host lists,
//! serves and deletes what the agent publishes.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use client::adapter::PermissionRequestSink;
use command_api::{CommandRegistry, SlashCommandKind};
use harness_runtime::local_apps::{register_skill, shared_store};
use harness_runtime::mobile::local_app_host::LocalAppHost;
use lingxi_core::host::{FileSystem, OrchestratorHandle, Platform};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ToolUseId};

const VISUALIZATION_FRAGMENT: &str = "<p>assembled-create-app-marker</p>";

struct NoopPermissionSink;

#[async_trait]
impl PermissionRequestSink for NoopPermissionSink {
    async fn emit_request(&self, _request: client::protocol::permission::PermissionRequest) {}
}

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

async fn assert_assembled_tool_saves_current_visualization(
    orchestrator: &orchestrator::ConversationOrchestrator,
    fs: Arc<dyn FileSystem>,
    config_home: &Path,
    desktop: bool,
) {
    let session = orchestrator.current_session_id().await;
    let visualizations =
        harness_runtime::inline_visualization::shared_store(fs.clone(), config_home);
    let reference = visualizations
        .publish(
            &visualization::Publisher {
                root_session: session.as_uuid(),
                agent_id: None,
            },
            None,
            "Assembled visualization",
            VISUALIZATION_FRAGMENT,
            0,
        )
        .await
        .expect("publish visualization into the host's shared store")
        .reference;
    let input = Utf16JsonProjection::object(vec![(
        "visualization_id".to_owned(),
        Utf16JsonProjection::from(reference.id.as_str()),
    )])
    .expect("visualization reference is valid LocalApp input");

    let desktop_request = if desktop {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "a".repeat(64);
        std::fs::write(
            config_home.join("desktop-local-app-host.json"),
            serde_json::json!({"port":port,"token":token}).to_string(),
        )
        .unwrap();
        Some(tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let (header_end, length) = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(end) = buffer.windows(4).position(|v| v == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                    assert!(header.starts_with("post /local-app "));
                    assert!(header.contains(&format!("authorization: bearer {token}")));
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while buffer.len() < header_end + length {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                buffer.extend_from_slice(&chunk[..n]);
            }
            let input: serde_json::Value =
                serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap();
            let body = r#"{"action":"publish","id":"a00000000000000000000","revision":1,"name":"Assembled visualization"}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            input
        }))
    } else {
        None
    };

    // Exercise the assembled tool through the same dispatcher path as a host
    // tool request, including the real schema, validation, and permission gates.
    let result = orchestrator
        .call_tool_from_host(ToolUseId::new(), "LocalApp".to_owned(), input, None)
        .await
        .expect("dispatch LocalApp through the assembled orchestrator")
        .expect("the local-app tool is registered by this host");
    match result {
        ContentBlock::ToolResult {
            is_error, content, ..
        } => assert_ne!(
            is_error,
            Some(true),
            "saving the current visualization should succeed; full tool-result content: {}",
            content
                .to_json_string()
                .unwrap_or_else(|error| format!("<projection serialization failed: {error}>"))
        ),
        _ => panic!("LocalApp dispatch should return a tool result"),
    }

    if let Some(request) = desktop_request {
        let request = tokio::time::timeout(std::time::Duration::from_secs(10), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request["action"], "publish");
        assert_eq!(request["name"], "Assembled visualization");
        assert!(request["html"]
            .as_str()
            .unwrap()
            .contains(VISUALIZATION_FRAGMENT));
        assert!(request["source_session"]["sessionId"].is_string());
        return;
    }

    let host_runtime = tokio::runtime::Handle::current();
    let host_config_home = config_home.to_path_buf();
    let (apps, served) = tokio::task::spawn_blocking(move || {
        let host = LocalAppHost::for_config_home(fs, &host_config_home, host_runtime);
        let apps = host.list();
        let served = apps
            .first()
            .map(|app| host.serve(app.id.clone(), "/".to_owned()));
        (apps, served)
    })
    .await
    .expect("run synchronous LocalAppHost methods outside the runtime");
    assert_eq!(apps.len(), 1, "the host should observe the newly saved app");
    assert_eq!(apps[0].name, "Assembled visualization");
    let served = served.expect("serve the listed local app");
    assert_eq!(served.status, 200);
    assert!(
        String::from_utf8(served.body)
            .unwrap()
            .contains(VISUALIZATION_FRAGMENT),
        "the published app should contain the source visualization fragment"
    );
}

fn configure_local_app_test_permission(config_home: &Path) {
    // The assembly test is about store wiring, so avoid the Auto-mode safety
    // classifier (which needs a provider) while retaining the real policy gate
    // and an explicit rule for this tool.
    std::fs::create_dir_all(config_home).expect("create isolated host settings directory");
    std::fs::write(
        config_home.join("settings.local.json"),
        r#"{"permissions":{"defaultMode":"default","allow":["LocalApp"]}}"#,
    )
    .expect("configure an explicit non-auto LocalApp allow rule");
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

#[tokio::test]
async fn desktop_assembly_passes_shared_visualizations_to_local_app() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().to_path_buf();
    let config_home = cwd.join(".lingxi");
    configure_local_app_test_permission(&config_home);
    let mut cfg = harness_runtime::desktop::DesktopConfig::default();
    cfg.cwd = cwd.clone();
    cfg.lingxi_home = config_home.clone();
    cfg.local_apps = true;
    cfg.inline_visualization = true;
    cfg.isolated_credential_storage = true;
    cfg.session_persistence = false;
    cfg.use_noop_permission_gate = true;

    let output: Arc<dyn lingxi_core::host::OutputStream> =
        Arc::new(orchestrator::test_support::MockOutputStream::new());
    let permission_sink: Arc<dyn PermissionRequestSink> = Arc::new(NoopPermissionSink);
    let runtime = Box::pin(harness_runtime::desktop::build(
        cfg,
        output,
        permission_sink,
    ))
    .await
    .expect("desktop runtime should assemble with local apps and visualizations");
    assert!(runtime
        .registered_tool_names()
        .iter()
        .any(|name| name == "LocalApp"));

    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(cwd));
    assert_assembled_tool_saves_current_visualization(
        &runtime.orchestrator,
        fs,
        &config_home,
        true,
    )
    .await;
}

#[tokio::test]
async fn mobile_assembly_passes_shared_visualizations_to_local_app() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().to_path_buf();
    let config_home = cwd.join(".lingxi");
    configure_local_app_test_permission(&config_home);
    let mut cfg = harness_runtime::mobile::test_support::test_config(&cwd);
    cfg.local_apps = true;
    cfg.inline_visualization = true;
    cfg.api_key = "test-key".into();
    assert_eq!(cfg.lingxi_home, config_home);
    let platform = Arc::new(harness_runtime::mobile::test_support::HostFakePlatform::new(cwd));
    let fs = platform.filesystem();
    let runtime = harness_runtime::mobile::build_mobile(
        cfg,
        platform,
        Arc::new(harness_runtime::mobile::test_support::FakeListener::default()),
        Arc::new(harness_runtime::mobile::test_support::CollectingPermissionSink::default()),
    )
    .await
    .expect("mobile runtime should assemble with local apps and visualizations");

    assert_assembled_tool_saves_current_visualization(
        &runtime.orchestrator,
        fs,
        &config_home,
        false,
    )
    .await;
}
