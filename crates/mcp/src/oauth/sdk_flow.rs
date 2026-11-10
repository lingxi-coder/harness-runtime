//! SDK-owned OAuth continuation. It never invokes the host browser hook.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, watch};

pub(crate) const MISSING_CODE: &str = "Invalid callback URL: missing authorization code. Please paste the full redirect URL including the code parameter.";
pub(crate) const WRONG_CALLBACK: &str = "Callback URL not accepted: it belongs to a different sign-in attempt (OAuth state mismatch) or carries no authorization code. The sign-in in progress is still waiting; submit the redirect URL from its own browser page.";

#[derive(Clone)]
pub(crate) struct Ready {
    pub response: serde_json::Value,
    pub(crate) state: String,
}

pub(crate) struct FlowControl {
    pub ready: watch::Sender<Option<Result<Ready, String>>>,
    callbacks: mpsc::UnboundedSender<String>,
    callback_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
    submitted: AtomicBool,
    pub manual_callback: AtomicBool,
}

impl FlowControl {
    pub fn new() -> Self {
        let (callbacks, receiver) = mpsc::unbounded_channel();
        Self {
            ready: watch::channel(None).0,
            callbacks,
            callback_rx: tokio::sync::Mutex::new(receiver),
            submitted: AtomicBool::new(false),
            manual_callback: AtomicBool::new(false),
        }
    }

    pub fn publish(&self, auth_url: &str, port: u16, state: &str, custom: bool) {
        let mut response = serde_json::json!({"authUrl":auth_url,"requiresUserAction":true,"callbackExpected":true,"redirectScheme":if custom {"custom"} else {"localhost"},"state":state});
        if !custom {
            response["callbackPort"] = port.into();
        }
        self.ready.send_replace(Some(Ok(Ready {
            response,
            state: state.into(),
        })));
    }

    pub fn submit(&self, input: &str) -> Result<(), &'static str> {
        let parsed = url::Url::parse(input).ok();
        let pairs = parsed
            .as_ref()
            .map(|url| url.query_pairs().collect::<Vec<_>>());
        let first = |key: &str| {
            pairs.as_ref().and_then(|pairs| {
                pairs
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.as_ref())
            })
        };
        if first("code").is_none() && first("error").is_none() {
            return Err(MISSING_CODE);
        }
        let ready = self.ready.borrow().clone();
        let Some(Ok(ready)) = ready else {
            return Err(WRONG_CALLBACK);
        };
        if first("state") != Some(ready.state.as_str())
            || (!first("code").is_some_and(|value| !value.is_empty())
                && !first("error").is_some_and(|value| !value.is_empty()))
        {
            return Err(WRONG_CALLBACK);
        }
        if self.submitted.swap(true, Ordering::SeqCst) {
            return Err(WRONG_CALLBACK);
        }
        self.manual_callback.store(true, Ordering::SeqCst);
        self.callbacks
            .send(input.into())
            .map_err(|_| WRONG_CALLBACK)
    }

    pub async fn wait_for_callback(
        &self,
        listener: CallbackListener,
        state: &str,
        redirect_uri: &str,
        issuer: Option<&[u16]>,
        required: bool,
    ) -> Result<String, OAuthError> {
        let mut receiver = self.callback_rx.lock().await;
        let callback = listener.accept_with_issuer_units(state, redirect_uri, issuer, required);
        tokio::pin!(callback);
        tokio::select! {
            params = &mut callback => Ok(params?.code),
            _ = tokio::time::sleep(OAUTH_CALLBACK_TIMEOUT) => Err(OAuthError::Callback("Authentication timeout".into())),
            input = receiver.recv() => {
                let input=input.ok_or_else(||OAuthError::Callback("Authentication cancelled".into()))?;
                let issuer=issuer.map(String::from_utf16).transpose().map_err(|_|OAuthError::Callback("Authorization response issuer does not match authorization server".into()))?;
                try_manual_callback_url(&input,state,issuer.as_deref(),required).unwrap_or_else(||Err(OAuthError::Callback(WRONG_CALLBACK.into())))
            }
        }
    }
}

pub(super) struct Attempt<'a> {
    pub control: &'a FlowControl,
    pub redirect_uri: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn perform(
    http: &Arc<dyn HttpTransport>,
    clock: &Arc<dyn Clock>,
    oauth: &lingxi_core::host::McpOAuthConfigDto,
    server_name: &str,
    server_url: &str,
    storage: &Arc<dyn lingxi_core::host::SecureStorage>,
    key: &str,
    identity: &McpOAuthIdentity,
    control: &FlowControl,
    redirect_uri: Option<&str>,
    redirect_only: bool,
) -> Result<Tokens, OAuthError> {
    let no_browser: OnAuthorizationUrl = Arc::new(|_| {});
    let custom = redirect_uri.filter(|_| redirect_only || oauth.client_id.is_none());
    let run = |redirect_uri| {
        perform_oauth_flow_inner(
            http,
            clock,
            oauth,
            server_name,
            server_url,
            &no_browser,
            None,
            None,
            None,
            None,
            Some(OAuthFlowPersistence {
                storage,
                key,
                identity,
            }),
            Some(Attempt {
                control,
                redirect_uri,
            }),
        )
    };
    match run(custom).await {
        Err(_) if custom.is_some() && !redirect_only && control.ready.borrow().is_none() => {
            run(None).await
        }
        result => result,
    }
}

/// Native SDK control diagnostics remove terminal/markup delimiters and clamp
/// detail. Token descriptions are preferred to the internal typed error label.
pub(crate) fn error_detail(error: &OAuthError) -> String {
    let raw = match error {
        OAuthError::Token(message) => message
            .split_once(": ")
            .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
            .and_then(|value| {
                value
                    .get("error_description")
                    .and_then(|description| description.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| message.clone()),
        OAuthError::Callback(message) => message.clone(),
        OAuthError::Registration(message) => message
            .strip_prefix("status ")
            .and_then(|value| value.split_once(": "))
            .map(|(status, body)| {
                format!("Dynamic Client Registration rejected (HTTP {status}): {body}")
            })
            .unwrap_or_else(|| message.clone()),
        _ => error.to_string(),
    };
    crate::registry::sanitize_sdk_mcp_auth_text(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_callback_validation_keeps_attempt_alive_after_invalid_input() {
        let control = FlowControl::new();
        control.publish("http://localhost/authorize", 3210, "state", false);
        assert_eq!(control.submit("not-a-url"), Err(MISSING_CODE));
        assert_eq!(
            control.submit("lingxi-test://callback?code=x&state=wrong"),
            Err(WRONG_CALLBACK)
        );
        assert_eq!(
            control.submit("lingxi-test://callback?code=x&state=state"),
            Ok(())
        );
        assert!(control.manual_callback.load(Ordering::SeqCst));
        assert_eq!(
            control.submit("lingxi-test://callback?code=x&state=state"),
            Err(WRONG_CALLBACK)
        );
    }
}
