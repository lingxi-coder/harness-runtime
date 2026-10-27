use super::build_mobile_engine;
use crate::mobile::test_support::{
    test_config, CollectingPermissionSink, FakeListener, HostFakePlatform,
};
use client::adapter::{ClientEventListener, PermissionRequestSink};
use client::protocol::commands::{ClientCommand, ImageRefDto, ListingKindDto};
use client::protocol::events::ClientEvent;
use client::protocol::text::ClientTextDto;
use lingxi_core::host::agent_name_registry::AgentNameRegistry;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;
use tasks::{TaskSpawnInput, TaskType};

#[tokio::test]
async fn mobile_list_agents_catalog_and_peers_dispatch_use_live_mobile_stores() {
    let root = tempfile::tempdir().expect("mobile list-agents root");
    let listener = Arc::new(FakeListener::default());
    let listener_dyn: Arc<dyn ClientEventListener> = listener.clone();
    let permission_sink: Arc<dyn PermissionRequestSink> =
        Arc::new(CollectingPermissionSink::default());
    let platform: Arc<dyn lingxi_core::host::Platform> =
        Arc::new(HostFakePlatform::new(root.path().to_path_buf()));
    let handle = build_mobile_engine(
        test_config(root.path()),
        platform,
        listener_dyn,
        permission_sink,
    )
    .expect("compose the real mobile runtime and slash dispatcher");

    let agent_id = lingxi_core::types::AgentId::new();
    AgentNameRegistry::register(
        handle.inner().wired_agent_name_registry.as_ref(),
        "mobile-worker",
        agent_id,
    )
    .await;
    handle
        .inner()
        .task_registry
        .create(
            TaskType::LocalAgent,
            TaskSpawnInput::LocalAgent {
                agent_id,
                subagent_type: "general-purpose".into(),
                prompt: Utf16JsonProjection::from("mobile list-agents fixture"),
                is_backgrounded: true,
                tool_use_id: None,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
                spawn_request: None,
                inheritance: None,
            },
            Utf16JsonProjection::from("mobile list-agents fixture"),
        )
        .await
        .expect("seed the live task registry with a current local-agent row");

    handle
        .submit(ClientCommand::RefreshListings {
            which: vec![ListingKindDto::SlashCommands],
        })
        .await
        .expect("refresh through the mobile engine");
    let commands = listener
        .received
        .lock()
        .await
        .iter()
        .find_map(|event| match event {
            ClientEvent::SlashCommandCatalog { commands } => Some(commands.clone()),
            _ => None,
        })
        .expect("mobile engine publishes its live slash catalog");
    let command = commands
        .iter()
        .find(|command| command.name.display() == "list-agents")
        .expect("the enabled mobile factory is in the client catalog");
    assert!(
        command
            .aliases
            .iter()
            .any(|alias| alias.display() == "peers"),
        "the Native alias is advertised from the same command descriptor"
    );

    handle
        .submit(ClientCommand::RunSlashCommand {
            session_id: Some(handle.active_session_id()),
            raw: ClientTextDto::from_utf8("/peers"),
            images: Vec::<ImageRefDto>::new(),
            turn_id: Some(731),
        })
        .await
        .expect("dispatch the Native alias through the real mobile engine");
    let events = listener.received.lock().await;
    let result = events
        .iter()
        .find_map(|event| match event {
            ClientEvent::SlashCommandResult {
                turn_id: Some(731),
                display,
                is_error,
            } => Some((display, is_error)),
            _ => None,
        })
        .expect("mobile slash dispatch returns a correlated result");
    assert!(!result.1, "the registered alias must reach the handler");
    let display = result.0.display();
    assert!(display.contains("Subagents (1)"), "{display:?}");
    assert!(
        display.contains("mobile-worker"),
        "the wired AgentNameRegistry is read"
    );
    assert!(
        display.contains("general-purpose"),
        "the TaskRegistry facts are read"
    );
    assert!(
        display.contains("other local sessions may be missing"),
        "{display:?}"
    );
    assert!(
        !display.contains("Teammates ("),
        "mobile must not invent team state"
    );
}
