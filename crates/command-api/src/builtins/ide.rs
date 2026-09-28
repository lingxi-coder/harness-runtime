//! `/ide` — inspect and control local IDE integrations.
//!
//! Discovery and credentials live behind [`platform_api::IdeHandle`]. This
//! handler only renders the secret-free status snapshot and delegates
//! connect/disconnect/open actions to the live host.

use crate::builtin_support::core_description;
use crate::builtin_support::list_render::render_list;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use platform_api::{IdeEndpointInfo, IdeStatus, IdeTransport, OrchestratorHandle};
use std::sync::Arc;

/// Handle for `/ide`, backed by the live orchestrator.
#[derive(Clone)]
pub struct IdeHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl IdeHandler {
    /// Construct an IDE handler for a live orchestrator.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for IdeHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let action = args
            .positional_args
            .first()
            .map(String::as_str)
            .unwrap_or("status");
        let result = match action {
            "status" => Ok(render_status(&self.handle.ide_status().await)),
            "open" => {
                let status = self.handle.ide_status().await;
                match status.endpoints.len() {
                    0 if status.selected.is_none() => {
                        Ok("No valid IDE endpoints found.".to_string())
                    }
                    1 if status.selected.is_none() => {
                        // Keep the common one-IDE path useful even when the
                        // user did not first run `/ide connect`: selecting
                        // this sole secure endpoint is deterministic, then
                        // the open operation uses the live session CWD.
                        let endpoint_id = status.endpoints[0].id.as_str();
                        match self.handle.ide_connect(endpoint_id).await {
                            Ok(_) => self.handle.ide_open().await,
                            Err(error) => Err(error),
                        }
                    }
                    _ if status.selected.is_none() => Ok(render_picker(&status)),
                    _ => self.handle.ide_open().await,
                }
            }
            "disconnect" => self
                .handle
                .ide_disconnect()
                .await
                .map(|status| render_status(&status)),
            "connect" => {
                let status = self.handle.ide_status().await;
                let endpoint_id = args
                    .positional_args
                    .get(1)
                    .map(String::as_str)
                    .map(|selector| endpoint_id_for_selector(&status, selector).unwrap_or(selector))
                    .or_else(|| single_endpoint_id(&status));
                match endpoint_id {
                    Some(id) => self
                        .handle
                        .ide_connect(id)
                        .await
                        .map(|status| render_status(&status)),
                    None if status.endpoints.is_empty() => {
                        Ok("No valid IDE endpoints found.".to_string())
                    }
                    None => Ok(render_picker(&status)),
                }
            }
            _ => Ok("Usage: /ide [status|open|connect [endpoint]|disconnect]".to_string()),
        };
        CommandResult::Done {
            display: Some(match result {
                Ok(display) => display,
                Err(error) => format!("Could not manage IDE integration: {error}"),
            }),
        }
    }

    fn name(&self) -> &str {
        "ide"
    }

    fn description(&self) -> &str {
        core_description("ide")
    }
}

fn single_endpoint_id(status: &IdeStatus) -> Option<&str> {
    (status.endpoints.len() == 1).then(|| status.endpoints[0].id.as_str())
}

fn render_picker(status: &IdeStatus) -> String {
    let rows = status.endpoints.iter().map(format_endpoint).collect();
    render_list("IDE endpoints", rows, "No valid IDE endpoints found.")
}

fn render_status(status: &IdeStatus) -> String {
    if status.endpoints.is_empty() {
        return "No valid IDE endpoints found.".to_string();
    }
    let mut rendered = render_picker(status);
    if let Some(selected) = status.selected.as_deref() {
        if let Some(endpoint) = status
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == selected)
        {
            rendered.push_str(&format!(
                "Connected endpoint: {} (port {})\n",
                endpoint.name, endpoint.port
            ));
        } else {
            // A host may report a selected id that disappeared between its
            // discovery and render snapshots. Do not print that opaque id or
            // a lockfile path into user-facing output.
            rendered.push_str("Connected endpoint: unavailable\n");
        }
    } else {
        rendered.push_str("Connected endpoint: none\n");
    }
    rendered
}

fn endpoint_id_for_selector<'a>(status: &'a IdeStatus, selector: &str) -> Option<&'a str> {
    status
        .endpoints
        .iter()
        .find(|endpoint| endpoint.id == selector)
        .or_else(|| {
            selector.parse::<u16>().ok().and_then(|port| {
                status
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.port == port)
            })
        })
        .map(|endpoint| endpoint.id.as_str())
}

fn format_endpoint(endpoint: &IdeEndpointInfo) -> String {
    let transport = match endpoint.transport {
        IdeTransport::Sse => "sse",
        IdeTransport::Ws => "ws",
    };
    let state = if endpoint.connected {
        "connected"
    } else {
        "available"
    };
    format!(
        "{}  {}  {}  port {}",
        endpoint.name, state, transport, endpoint.port
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args(raw: &str) -> ParsedSlashCommand {
        let positional_args = raw
            .split_whitespace()
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        ParsedSlashCommand {
            name: "ide".into(),
            raw_args: raw.into(),
            positional_args,
        }
    }

    #[tokio::test]
    async fn empty_status_is_secret_free() {
        let handler = IdeHandler::new(Arc::new(MockOrchestratorHandle::new()));
        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args("")).await
        else {
            panic!("expected display")
        };
        assert_eq!(display, "No valid IDE endpoints found.");
        assert!(!display.contains("auth"));
    }

    #[test]
    fn visible_port_resolves_to_the_opaque_endpoint_id() {
        let status = IdeStatus {
            endpoints: vec![IdeEndpointInfo {
                id: "/private/ide/43123.lock".into(),
                name: "VS Code".into(),
                transport: IdeTransport::Ws,
                port: 43123,
                workspace_folders: vec![],
                running_in_windows: false,
                connected: false,
            }],
            selected: None,
        };
        assert_eq!(
            endpoint_id_for_selector(&status, "43123"),
            Some("/private/ide/43123.lock")
        );
        assert_eq!(
            endpoint_id_for_selector(&status, "/private/ide/43123.lock"),
            Some("/private/ide/43123.lock")
        );
        assert_eq!(endpoint_id_for_selector(&status, "9999"), None);
    }

    #[test]
    fn status_never_renders_an_opaque_selected_id() {
        let status = IdeStatus {
            endpoints: vec![IdeEndpointInfo {
                id: "/private/ide/43124.lock".into(),
                name: "Cursor".into(),
                transport: IdeTransport::Sse,
                port: 43124,
                workspace_folders: vec![],
                running_in_windows: false,
                connected: true,
            }],
            selected: Some("/private/ide/43124.lock".into()),
        };
        let rendered = render_status(&status);
        assert!(rendered.contains("Connected endpoint: Cursor (port 43124)"));
        assert!(!rendered.contains("/private/ide/43124.lock"));
    }
}
