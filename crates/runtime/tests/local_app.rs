//! Host wiring for local apps: the `/local-app` skill follows the `LocalApp`
//! tool, and the independent mobile app service observes what the ordinary
//! development session publishes without owning that session.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use client::adapter::PermissionRequestSink;
use command_api::{CommandRegistry, SlashCommandKind};
use harness_runtime::local_apps::register_skill;
use harness_runtime::mobile::unified_local_apps::MobileLocalAppServiceHandle;
use lingxi_core::host::{FileSystem, OrchestratorHandle, Platform};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ToolUseId};
use serde_json::json;

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

fn canonical_body(shell_tool: Option<&str>) -> String {
    let markdown = local_app::skill::skill_markdown(shell_tool);
    markdown
        .split_once("\n---\n")
        .map(|(_, body)| body.trim_start().to_owned())
        .expect("canonical Local App skill has frontmatter")
}

/// Two scripted model turns: one `LocalApp` tool use saving `visualization_id`,
/// then the reply after its result. Since 46fb13b3 a publication must
/// originate from a durable assistant tool use, so the model, not the host,
/// calls the assembled tool.
fn local_app_turns(visualization_id: &str) -> [Vec<llm_runtime::HistoryEvent>; 2] {
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_start_tool_use, content_block_stop,
        input_json_delta, message_delta_stop, message_start, message_stop, text_delta,
    };
    let input = serde_json::json!({ "visualization_id": visualization_id }).to_string();
    [
        vec![
            message_start("msg_local_app", "claude-sonnet-4-20250514"),
            content_block_start_tool_use(0, ToolUseId::from("toolu_local_app"), "LocalApp"),
            input_json_delta(0, &input),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ],
        vec![
            message_start("msg_local_app_done", "claude-sonnet-4-20250514"),
            content_block_start_text(0),
            text_delta(0, "saved"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
    ]
}

/// The tool result the model received after its `LocalApp` call succeeded.
async fn assert_local_app_result_succeeded(
    streaming: &orchestrator::test_support_stream::MockStreamingApiClient,
) {
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2, "the tool result returns to the model once");
    let (is_error, content) = calls[1]
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            lingxi_core::types::ConversationMessage::User { content, .. } => {
                content.iter().find_map(|block| match block {
                    ContentBlock::ToolResult {
                        is_error, content, ..
                    } => Some((*is_error, content.clone())),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("the LocalApp tool result reaches the model");
    assert_ne!(
        is_error,
        Some(true),
        "saving the current visualization should succeed; full tool-result content: {}",
        content
            .to_json_string()
            .unwrap_or_else(|error| format!("<projection serialization failed: {error}>"))
    );
}

/// The native app service reads the same durable library without borrowing
/// an engine runtime or requiring Linux for this static release.
async fn assert_mobile_library_has_published_app(config_home: &Path) {
    let service =
        MobileLocalAppServiceHandle::new(config_home.into(), None, "android".into()).unwrap();
    let apps = service.request_value(json!({"op":"list"})).await.unwrap();
    let apps = apps.as_array().unwrap();
    assert_eq!(
        apps.len(),
        1,
        "the standalone service sees the published app"
    );
    assert_eq!(apps[0]["name"], "Assembled visualization");
    let app_id = apps[0]["appId"].as_str().unwrap();
    let release = service
        .request_value(json!({"op":"getRelease","appId":app_id,"revision":1}))
        .await
        .unwrap();
    assert_eq!(release["manifest"]["schema"], 2);
    let layout = service
        .request_value(json!({"op":"layout","appId":app_id}))
        .await
        .unwrap();
    let source = Path::new(layout["sourcePath"].as_str().unwrap()).join("index.html");
    assert!(std::fs::read_to_string(source)
        .unwrap()
        .contains(VISUALIZATION_FRAGMENT));
    assert!(
        !config_home.join("local-apps/index.json").exists(),
        "new publication must not populate the archived chat-document registry"
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
    register_skill(&mut registry, &names(&["Read", "Bash"]), Some("Bash"));
    assert!(body(&registry).is_none(), "no tool, no skill");

    register_skill(&mut registry, &names(&["LocalApp", "Bash"]), Some("Bash"));
    let bash_body = body(&registry).unwrap().string_at("").unwrap().to_owned();
    assert_eq!(bash_body, canonical_body(Some("Bash")));

    register_skill(&mut registry, &names(&["LocalApp", "Shell"]), Some("Shell"));
    let mobile_body = body(&registry).unwrap().string_at("").unwrap().to_owned();
    assert_eq!(mobile_body, canonical_body(Some("Shell")));
    assert_eq!(
        registry
            .list_all()
            .iter()
            .filter(|command| { command.name.string_at("") == Some(local_app::skill::SKILL_NAME) })
            .count(),
        1,
        "re-registration replaces instead of stacking"
    );

    register_skill(&mut registry, &names(&["Read"]), Some("Shell"));
    assert!(body(&registry).is_none(), "the skill leaves with the tool");

    register_skill(&mut registry, &names(&["LocalApp"]), None);
    let no_shell_body = body(&registry).unwrap().string_at("").unwrap().to_owned();
    assert_eq!(no_shell_body, canonical_body(None));
    register_skill(&mut registry, &names(&["LocalApp"]), Some("Shell"));
    assert_eq!(
        body(&registry).unwrap().string_at("").unwrap(),
        canonical_body(None)
    );
    let command = registry
        .resolve(&Utf16JsonProjection::from(local_app::skill::SKILL_NAME))
        .unwrap();
    assert_eq!(command.loaded_from.as_deref(), Some("local-app"));
    assert!(command.skill_root.is_none());
}

#[tokio::test]
async fn desktop_assembly_passes_shared_visualizations_to_local_app() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().canonicalize().unwrap();
    let config_home = cwd.join(".lingxi");
    configure_local_app_test_permission(&config_home);
    let streaming = Arc::new(orchestrator::test_support_stream::MockStreamingApiClient::empty());
    let mut cfg = harness_runtime::desktop::DesktopConfig::default();
    cfg.cwd = cwd.clone();
    cfg.lingxi_home = config_home.clone();
    cfg.local_apps = true;
    cfg.inline_visualization = true;
    cfg.isolated_credential_storage = true;
    // The publication origin is read back from the durable transcript.
    cfg.session_persistence = true;
    cfg.use_noop_permission_gate = true;
    cfg.streaming_override = Some(streaming.clone());

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
    let session = runtime.orchestrator.current_session_id().await;
    let reference = harness_runtime::inline_visualization::shared_store(fs, &config_home)
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

    let desktop_request = {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "a".repeat(64);
        std::fs::write(
            config_home.join("desktop-local-app-host.json"),
            serde_json::json!({"port":port,"token":token}).to_string(),
        )
        .unwrap();
        tokio::spawn(async move {
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
            let body = r#"{"action":"publish","id":"a00000000000000000000000000000000","revision":1,"name":"Assembled visualization"}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            input
        })
    };
    for turn in local_app_turns(reference.id.as_str()) {
        streaming.push_turn(turn).await;
    }
    runtime
        .orchestrator
        .run_turn_streaming("save this visualization as an app")
        .await
        .expect("the LocalApp turn completes");
    assert_local_app_result_succeeded(&streaming).await;

    let request = tokio::time::timeout(std::time::Duration::from_secs(10), desktop_request)
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
}

#[test]
fn mobile_assembly_passes_shared_visualizations_to_local_app() {
    use client::protocol::commands::ClientCommand;
    use client::protocol::events::ClientEvent;
    use client::protocol::text::ClientTextDto;
    use orchestrator::test_support_stream::MockStreamingApiClient;

    let home = tempfile::tempdir().unwrap();
    // Mobile config lives under a brand user's Linux home. A random .tmp*
    // directory is not a valid Linux username.
    let cwd = home.path().canonicalize().unwrap().join("test-brand-user");
    std::fs::create_dir(&cwd).unwrap();
    let config_home = cwd.join(".lingxi");
    configure_local_app_test_permission(&config_home);
    let mut cfg = harness_runtime::mobile::test_support::test_config(&cwd);
    cfg.local_apps = true;
    cfg.inline_visualization = true;
    assert_eq!(cfg.lingxi_home, config_home);
    let platform = Arc::new(harness_runtime::mobile::test_support::HostFakePlatform::new(cwd));
    let fs = platform.filesystem();
    let streaming = Arc::new(MockStreamingApiClient::empty());
    let listener = Arc::new(harness_runtime::mobile::test_support::FakeListener::default());
    let handle = harness_runtime::mobile::test_support::new_engine_with_streaming(
        cfg,
        platform,
        listener.clone(),
        Arc::new(harness_runtime::mobile::test_support::CollectingPermissionSink::default()),
        Some(streaming.clone()),
    )
    .expect("mobile runtime should assemble with local apps and visualizations");

    handle.runtime().block_on(async {
        handle
            .submit(ClientCommand::NewSession {
                mode: client::protocol::listings::SessionModeDto::Code,
                cwd: None,
                model: None,
            })
            .await
            .expect("start a Code session");
        let session_id = listener
            .received
            .lock()
            .await
            .iter()
            .rev()
            .find_map(|event| match event {
                ClientEvent::SessionStarted { session_id, .. } => Some(session_id.clone()),
                _ => None,
            })
            .expect("SessionStarted carries the durable id");
        let visualizations =
            harness_runtime::inline_visualization::shared_store(fs.clone(), &config_home);
        let reference = visualizations
            .publish(
                &visualization::Publisher {
                    root_session: uuid::Uuid::parse_str(&session_id).unwrap(),
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

        for turn in local_app_turns(reference.id.as_str()) {
            streaming.push_turn(turn).await;
        }
        handle
            .submit(ClientCommand::SendPrompt {
                text: ClientTextDto::from_utf8("save this visualization as an app"),
                prompt_mode: None,
                images: Vec::new(),
                turn_id: Some(1),
                visualization_context: None,
            })
            .await
            .expect("send prompt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !listener
            .received
            .lock()
            .await
            .iter()
            .any(|event| matches!(event, ClientEvent::TurnEnded { .. }))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the LocalApp turn ended"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert_local_app_result_succeeded(&streaming).await;
        assert_mobile_library_has_published_app(&config_home).await;
    });
}
