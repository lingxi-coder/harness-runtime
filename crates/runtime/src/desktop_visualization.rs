//! The bridge-server's `visualization` request: one correlated request whose
//! `op` selects mount, serve, state write, unmount, list or notices.
//!
//! The Electron main process forwards its `lingxi-viz://visualization` scheme
//! handler and the renderer's mount/state IPC here. Everything that decides —
//! authorization, document assembly, CSP headers, compare-and-swap state —
//! lives in [`VisualizationService`], the same service the mobile hosts drive
//! through UniFFI. This module only decodes and encodes JSON.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use visualization::document::{Theme, ThemeMode};
use visualization::{
    MountRequest, MountStateWrite, VisualizationId, VisualizationRef, VisualizationService,
};

/// The desktop's dedicated visualization origin.
pub const DESKTOP_ORIGIN: &str = "lingxi-viz://visualization";

#[derive(Debug, Deserialize)]
struct ThemeParams {
    dark: bool,
    #[serde(default)]
    tokens: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Mount {
        session_id: String,
        id: String,
        revision: u32,
        theme: ThemeParams,
        locale: String,
        expanded: bool,
    },
    Serve {
        path: String,
    },
    WriteState {
        token: String,
        generation: u64,
        base_version: u64,
        model_content: String,
        private_content: String,
    },
    Unmount {
        token: String,
    },
    UnmountSession {
        session_id: String,
    },
    List {
        session_id: String,
    },
    Notices,
}

/// Answers `visualization` requests for one bridge process.
pub struct DesktopVisualizationHost {
    service: VisualizationService,
}

/// A session id as the desktop sends it: bare or prefixed UUID.
fn session_uuid(session_id: &str) -> Option<uuid::Uuid> {
    lingxi_core::types::SessionId::parse_prefixed(session_id)
        .map(|id| id.as_uuid())
        .or_else(|| uuid::Uuid::parse_str(session_id).ok())
}

impl DesktopVisualizationHost {
    /// A host over the shared store of `config_home`.
    #[must_use]
    pub fn for_config_home(fs: Arc<dyn lingxi_core::host::FileSystem>, config_home: &Path) -> Self {
        let store = crate::inline_visualization::shared_store(fs, config_home);
        let service = VisualizationService::new(store, DESKTOP_ORIGIN)
            .expect("the desktop visualization origin is a valid scheme://host");
        Self { service }
    }

    /// Answer one request. `Ok(Value::Null)` is a valid reply (an
    /// unavailable mount, an unmount); `Err` is a malformed request.
    ///
    /// # Errors
    /// The params do not decode as a known `op`.
    pub async fn handle(&self, params: Value) -> Result<Value, String> {
        let request: Request = serde_json::from_value(params)
            .map_err(|_| "invalid visualization request".to_string())?;
        Ok(match request {
            Request::Mount {
                session_id,
                id,
                revision,
                theme,
                locale,
                expanded,
            } => {
                let (Some(root_session), Some(id)) =
                    (session_uuid(&session_id), VisualizationId::parse(&id))
                else {
                    return Ok(Value::Null);
                };
                let request = MountRequest {
                    root_session,
                    reference: VisualizationRef { id, revision },
                    theme: Theme {
                        mode: if theme.dark {
                            ThemeMode::Dark
                        } else {
                            ThemeMode::Light
                        },
                        tokens: theme.tokens.into_iter().collect(),
                    },
                    locale,
                    expanded,
                };
                match self.service.mount(request).await {
                    Ok(ticket) => json!({
                        "token": ticket.token,
                        "generation": ticket.generation,
                        "doc_url": ticket.doc_url,
                        "title": ticket.title,
                    }),
                    Err(_) => Value::Null,
                }
            }
            Request::Serve { path } => {
                let response = self.service.serve(&path).await;
                json!({
                    "status": response.status,
                    "headers": response.headers,
                    "body_base64": base64::engine::general_purpose::STANDARD.encode(response.body),
                })
            }
            Request::WriteState {
                token,
                generation,
                base_version,
                model_content,
                private_content,
            } => match self
                .service
                .write_state(
                    &token,
                    generation,
                    base_version,
                    &model_content,
                    &private_content,
                )
                .await
            {
                MountStateWrite::Saved { version } => json!({ "saved": true, "version": version }),
                MountStateWrite::Rejected { reason, current } => {
                    let mut reply = json!({
                        "saved": false,
                        "version": current.as_ref().map_or(0, |state| state.version),
                        "reason": reason,
                    });
                    if let Some(current) = current {
                        reply["current_state"] =
                            serde_json::to_value(current).unwrap_or(Value::Null);
                    }
                    reply
                }
            },
            Request::Unmount { token } => {
                self.service.unmount(&token);
                Value::Null
            }
            Request::UnmountSession { session_id } => {
                if let Some(root_session) = session_uuid(&session_id) {
                    self.service.unmount_session(root_session);
                }
                Value::Null
            }
            Request::List { session_id } => {
                let summaries = match session_uuid(&session_id) {
                    Some(root_session) => self.service.list(root_session).await.unwrap_or_default(),
                    None => Vec::new(),
                };
                Value::Array(
                    summaries
                        .into_iter()
                        .flat_map(|summary| {
                            let id = summary.id.as_str().to_string();
                            summary.revisions.into_iter().map(move |revision| {
                                json!({
                                    "id": id,
                                    "revision": revision.revision,
                                    "title": revision.title,
                                    "created_at_ms": revision.created_at_ms,
                                })
                            })
                        })
                        .collect(),
                )
            }
            Request::Notices => Value::String(visualization::assets::third_party_notices()),
        })
    }
}
