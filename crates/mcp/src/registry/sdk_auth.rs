//! Session-owned SDK OAuth tasks. No task retains a strong registry reference.
use super::*;
use crate::oauth::sdk_flow::{self, FlowControl};
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Native SDK authentication error, including its optional stable wire code.
#[derive(Clone, Debug)]
pub struct SdkMcpAuthenticationError {
    /// Bounded display diagnostic for the control reply.
    pub message: String,
    /// Native `error_code` for redirect/registration startup failures.
    pub error_code: Option<String>,
}
impl SdkMcpAuthenticationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_code: None,
        }
    }
}

struct ActiveFlow {
    config: McpServerConfig,
    control: Arc<FlowControl>,
    cancellation: CancellationToken,
    result: watch::Sender<Option<Result<(), SdkMcpAuthenticationError>>>,
}
impl Drop for ActiveFlow {
    fn drop(&mut self) {
        if self.result.borrow().is_none() {
            self.cancellation.cancel();
            self.result
                .send_replace(Some(Err(SdkMcpAuthenticationError::new(
                    "Authentication cancelled",
                ))));
        }
        if self.control.ready.borrow().is_none() {
            self.control
                .ready
                .send_replace(Some(Err("Authentication cancelled".into())));
        }
    }
}

#[derive(Default)]
pub(super) struct SdkAuthFlows {
    active: StdMutex<HashMap<String, Arc<ActiveFlow>>>,
}

impl McpRegistry {
    /// Start an explicitly requested SDK OAuth flow, returning when its URL is
    /// ready. An HTTP/SSE server need not author an `oauth` configuration block.
    pub async fn start_sdk_authentication(
        self: &Arc<Self>,
        server: &[u16],
        redirect_uri: Option<String>,
        redirect_only: bool,
    ) -> Result<serde_json::Value, SdkMcpAuthenticationError> {
        let display = crate::normalization::sanitize_control_name_units(server);
        let config = self
            .connections
            .read()
            .await
            .values()
            .map(McpConnectionState::config)
            .find(|config| config.name_matches_utf16(server))
            .cloned()
            .ok_or_else(|| {
                SdkMcpAuthenticationError::new(format!("Server not found: {display}"))
            })?;
        if config.disabled {
            return Err(SdkMcpAuthenticationError::new(format!(
                "MCP server {display} is disabled — enable it (mcp_toggle) before authenticating"
            )));
        }
        if let Some(refusal) = self.connection_refusal(&config) {
            return Err(SdkMcpAuthenticationError::new(
                refusal.message.string_at("").unwrap_or_default(),
            ));
        }
        if self.action_states().await.iter().any(|(name, state)| {
            name == &config.name && *state == lingxi_core::host::McpActionState::NeedsApproval
        }) {
            return Err(SdkMcpAuthenticationError::new(format!("MCP server {display} is not approved for this project — approve it in /mcp before authenticating")));
        }
        if config.metadata.transport.as_deref() == Some("claudeai-proxy") {
            return Err(SdkMcpAuthenticationError::new(
                "Unable to build claude.ai connector auth URL (missing org or server id)",
            ));
        }
        let oauth = match &config.spec {
            McpTransportSpec::Http { oauth, .. } | McpTransportSpec::Sse { oauth, .. } => oauth
                .clone()
                .unwrap_or(lingxi_core::host::McpOAuthConfigDto {
                    client_id: None,
                    callback_port: None,
                    auth_server_metadata_url: None,
                    scopes: None,
                    xaa: None,
                }),
            other => {
                return Err(SdkMcpAuthenticationError::new(format!(
                    "Server type \"{}\" does not support OAuth authentication",
                    if matches!(other, McpTransportSpec::SdkControl { .. }) {
                        "sdk"
                    } else {
                        other.kind()
                    }
                )))
            }
        };
        let deps = self
            .oauth
            .as_ref()
            .ok_or_else(|| {
                SdkMcpAuthenticationError::new(
                    "MCP OAuth authentication is not configured for this session",
                )
            })?
            .clone();
        let storage = self.oauth_storage_for_config(&config, &deps.storage);
        let http = oauth::session::configured_http(&deps.http, &config.spec);
        let key = oauth::server_key(config.storage_name(), &config.spec);
        let control = Arc::new(FlowControl::new());
        let cancellation = CancellationToken::new();
        let (result, _) = watch::channel(None);
        let entry = Arc::new(ActiveFlow {
            config: config.clone(),
            control: control.clone(),
            cancellation: cancellation.clone(),
            result: result.clone(),
        });
        let mut ready = control.ready.subscribe();
        {
            let lifecycle = self.lifecycle_lock(&config.name);
            let _guard = lifecycle.lock().await;
            if !self
                .get_config_by_key(&config.name)
                .await
                .is_some_and(|current| Self::same_config_snapshot(&current, &config))
            {
                return Err(SdkMcpAuthenticationError::new(format!(
                    "Server not found: {display}"
                )));
            }
            if let Some(previous) = self
                .sdk_auth_flows
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(config.name.clone(), entry.clone())
            {
                previous.cancellation.cancel();
            }
        }
        let registry = Arc::downgrade(self);
        let identity = self.oauth_identity.clone();
        let task_control = control.clone();
        tokio::spawn(async move {
            let grant = tokio::select! {
                biased;
                _=cancellation.cancelled()=>Err(SdkMcpAuthenticationError::new("Authentication cancelled")),
                result=async {
                    // Native explicit auth clears the old grant first. This
                    // frozen storage/key pair cannot target a new config's row.
                    oauth::revoke_server_tokens(&storage,&http,&key,spec_url(&config.spec),&oauth).await;
                    let tokens=sdk_flow::perform(&http,&deps.clock,&oauth,config.storage_name(),spec_url(&config.spec),&storage,&key,&identity,&task_control,redirect_uri.as_deref(),redirect_only).await.map_err(|error| {
                        let startup=task_control.ready.borrow().is_none();
                        SdkMcpAuthenticationError {
                            message:if startup {format!("SDK auth failed: {}",sdk_flow::error_detail(&error))}else{sdk_flow::error_detail(&error)},
                            error_code:(startup && matches!(error,oauth::OAuthError::Registration(_))).then(||if redirect_only {"mcp_redirect_uri_unusable"}else{"mcp_client_registration_unavailable"}.into()),
                        }
                    })?;
                    Ok(tokens)
                }=>result,
            };
            let mut outcome = grant.as_ref().map(|_| ()).map_err(Clone::clone);
            let publish = |outcome: &Result<(), SdkMcpAuthenticationError>| {
                result.send_replace(Some(outcome.clone()));
                if task_control.ready.borrow().is_none() {
                    task_control.ready.send_replace(Some(outcome.clone().map(|_|sdk_flow::Ready {response:serde_json::json!({"requiresUserAction":false,"callbackExpected":false}),state:String::new()}).map_err(|error|error.message)));
                }
            };
            if let Some(registry) = registry.upgrade() {
                // Start/reconfiguration and grant publication share one
                // lifecycle authority. A superseded completion cannot close
                // the current attempt while redialling its old connection.
                let lifecycle = registry.lifecycle_lock(&config.name);
                let _guard = lifecycle.lock().await;
                let owns_flow = registry
                    .sdk_auth_flows
                    .active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&config.name)
                    .is_some_and(|active| Arc::ptr_eq(&active.control, &task_control));
                let current = owns_flow
                    && !cancellation.is_cancelled()
                    && registry
                        .get_config_by_key(&config.name)
                        .await
                        .is_some_and(|current| Self::same_config_snapshot(&current, &config));
                if !current {
                    outcome = Err(SdkMcpAuthenticationError::new("Authentication cancelled"));
                } else if let Ok(tokens) = grant {
                    outcome = tokio::select! {
                        biased;
                        _=cancellation.cancelled()=>Err(SdkMcpAuthenticationError::new("Authentication cancelled")),
                        saved=oauth::save_tokens(&storage,&deps.clock,&key,&tokens)=>saved.map_err(|error|SdkMcpAuthenticationError::new(sdk_flow::error_detail(&error))),
                    };
                }
                publish(&outcome);
                {
                    let mut active = registry
                        .sdk_auth_flows
                        .active
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if active
                        .get(&config.name)
                        .is_some_and(|active| Arc::ptr_eq(&active.control, &task_control))
                    {
                        active.remove(&config.name);
                    }
                }
                if current
                    && outcome.is_ok()
                    && !task_control.manual_callback.load(Ordering::SeqCst)
                {
                    tokio::select! {
                        biased;
                        _=cancellation.cancelled()=>{},
                        _=registry.reconnect_preserving_auth_locked(&config.name)=>{},
                    }
                }
            } else {
                publish(&Err(SdkMcpAuthenticationError::new(
                    "Authentication cancelled",
                )));
            }
        });
        loop {
            let answer = ready.borrow().clone();
            match answer {
                Some(Ok(ready)) => return Ok(ready.response),
                Some(Err(message)) => {
                    return Err(entry
                        .result
                        .borrow()
                        .clone()
                        .and_then(Result::err)
                        .unwrap_or_else(|| SdkMcpAuthenticationError::new(message)))
                }
                None => ready
                    .changed()
                    .await
                    .map_err(|_| SdkMcpAuthenticationError::new("Authentication cancelled"))?,
            }
        }
    }

    /// Submit a callback to the same source-identity flow and await its grant.
    /// Invalid callbacks leave that flow waiting for its own browser page.
    pub async fn submit_sdk_oauth_callback(
        &self,
        server: &[u16],
        callback: &str,
    ) -> Result<(), SdkMcpAuthenticationError> {
        let display = crate::normalization::sanitize_control_name_units(server);
        let entry = self
            .sdk_auth_flows
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|flow| flow.config.name_matches_utf16(server))
            .cloned()
            .ok_or_else(|| {
                SdkMcpAuthenticationError::new(format!(
                    "No active OAuth flow for server: {display}"
                ))
            })?;
        entry
            .control
            .submit(callback)
            .map_err(SdkMcpAuthenticationError::new)?;
        let mut result = entry.result.subscribe();
        loop {
            let completed = result.borrow().clone();
            if let Some(completed) = completed {
                return completed;
            }
            result
                .changed()
                .await
                .map_err(|_| SdkMcpAuthenticationError::new("Authentication cancelled"))?;
        }
    }

    /// Cancel session-owned SDK OAuth work, including bound callback sockets.
    pub fn cancel_sdk_authentications(&self) {
        let mut active = self
            .sdk_auth_flows
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for flow in active.values() {
            flow.cancellation.cancel();
        }
        active.clear();
    }
    pub(super) fn cancel_sdk_authentication(&self, name: &str) {
        if let Some(flow) = self
            .sdk_auth_flows
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name)
        {
            flow.cancellation.cancel();
        }
    }
    pub(super) fn cancel_sdk_auth_if_reconfigured(&self, config: &McpServerConfig) {
        let mut active = self
            .sdk_auth_flows
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .get(&config.name)
            .is_some_and(|flow| !Self::same_config_snapshot(&flow.config, config))
        {
            if let Some(flow) = active.remove(&config.name) {
                flow.cancellation.cancel();
            }
        }
    }
}
