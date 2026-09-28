//! `/connect <provider>` — interactive credential setup (engine-driven, tui-rendered).
//!
//! API-key providers read+store a secret via the [`ConnectCredentialWriter`] seam.
//! `/connect github-copilot` drives the GitHub device-flow through the
//! [`CopilotConnectDriver`] seam (begin → display → poll → store). Both seams are
//! defined HERE (not in the frozen `platform-api` crate) and implemented by the engine.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

/// Static one-liner for `/help` + the palette (`/connect` is not a locked builtin
/// name, so it carries its own description).
pub const CONNECT_DESCRIPTION: &str =
    "Connect a model provider (store an API key, or sign in to GitHub Copilot)";

/// Failure modes surfaced by the `/connect` seams.
#[derive(Debug, Clone, Error)]
pub enum ConnectError {
    /// The user cancelled the secure prompt or device-flow.
    #[error("connect cancelled")]
    Cancelled,
    /// Keychain / secure-storage write failed.
    #[error("could not store credential: {0}")]
    Storage(String),
    /// Network / device-flow transport error.
    #[error("network error: {0}")]
    Network(String),
    /// GitHub reported a terminal device-flow error (e.g. `access_denied`).
    #[error("device authorization failed: {0}")]
    DeviceFailed(String),
    /// A managed login policy (`forceLoginOrgUUID` org pin) rejected the
    /// completed sign-in (parity 2.1.207 H-BIN-09). Carries the byte-exact
    /// admin/validation message, surfaced VERBATIM (no wrapper) — unlike a
    /// network failure, an API key cannot satisfy the org pin either.
    #[error("{0}")]
    LoginPolicyDenied(String),
}

/// Engine seam: prompt for a secret (tui renders a masked input) and persist it
/// under a credential id. One call does prompt + store so the raw secret never
/// crosses back through the command layer.
#[async_trait]
pub trait ConnectCredentialWriter: Send + Sync {
    /// Prompt for the provider's API key and store it under `credential_id`.
    async fn prompt_and_store_key(&self, credential_id: &str) -> Result<(), ConnectError>;
}

/// Engine seam for `/connect chatgpt`: runs the OpenAI ChatGPT-account OAuth
/// login (browser PKCE, device-code fallback) and persists tokens. Returns a
/// human-facing display message on success.
#[async_trait]
pub trait ChatGptConnectDriver: Send + Sync {
    /// Drive the login to completion, returning a display message.
    async fn connect(&self) -> Result<String, ConnectError>;
}

/// Engine seam for the TUI `/connect` OAuth flow (Anthropic Pro/Max, OpenAI
/// ChatGPT). Unlike Copilot's device-flow (begin → display code → poll), these are
/// a single browser/PKCE call that blocks until the redirect completes, so one
/// method drives the whole flow. The engine dispatches per `provider_id` to the
/// real backend (`AuthHandle::login` for Anthropic; [`ChatGptConnectDriver`] for
/// OpenAI ChatGPT) and persists tokens. Returns a human-facing display message.
#[async_trait]
pub trait OAuthConnectDriver: Send + Sync {
    /// Drive the browser OAuth sign-in for `provider_id` to completion.
    async fn login(&self, provider_id: &str) -> Result<String, ConnectError>;
}

/// One step of the Copilot device-flow, surfaced so the tui renders the code.
#[derive(Debug, Clone)]
pub struct CopilotConnectStep {
    /// Code the user types at `verification_uri`.
    pub user_code: String,
    /// URL the user opens to authorize.
    pub verification_uri: String,
}

/// Engine seam: drive the GitHub Copilot device-flow end-to-end. `begin` returns
/// the code to display; `poll_to_completion` runs the poll loop and stores the
/// token on success.
#[async_trait]
pub trait CopilotConnectDriver: Send + Sync {
    /// Request a device code against `domain` (`None` = `github.com` public;
    /// `Some("company.ghe.com")` = a GitHub Enterprise host). The caller displays
    /// the code then polls.
    async fn begin(&self, domain: Option<&str>) -> Result<CopilotConnectStep, ConnectError>;
    /// Poll until authorized (or terminal), storing the token on success.
    async fn poll_to_completion(&self, step: &CopilotConnectStep) -> Result<(), ConnectError>;
}

/// `/connect` handler over the engine-supplied seams.
pub struct ConnectHandler {
    writer: Arc<dyn ConnectCredentialWriter>,
    copilot: Arc<dyn CopilotConnectDriver>,
    chatgpt: Arc<dyn ChatGptConnectDriver>,
}

impl ConnectHandler {
    /// Construct over the writer + Copilot + ChatGPT seams.
    #[must_use]
    pub fn new(
        writer: Arc<dyn ConnectCredentialWriter>,
        copilot: Arc<dyn CopilotConnectDriver>,
        chatgpt: Arc<dyn ChatGptConnectDriver>,
    ) -> Self {
        Self {
            writer,
            copilot,
            chatgpt,
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ConnectHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let provider = args.raw_args.trim();
        if provider.is_empty() {
            return CommandResult::Done {
                display: Some(
                    "Usage: /connect <provider>  (e.g. openrouter, deepseek, glm-coding, zai, chatgpt, github-copilot)".to_string(),
                ),
            };
        }
        if provider == "github-copilot" {
            // The text `/connect github-copilot` command path uses the public
            // deployment; the TUI's deployment-type popup drives Enterprise.
            let step = match self.copilot.begin(None).await {
                Ok(s) => s,
                Err(e) => {
                    return CommandResult::Done {
                        display: Some(format!("Could not start Copilot sign-in: {e}")),
                    }
                }
            };
            let intro = format!(
                "To sign in to GitHub Copilot, open {} and enter code {}",
                step.verification_uri, step.user_code
            );
            match self.copilot.poll_to_completion(&step).await {
                Ok(()) => CommandResult::Done {
                    display: Some(format!("{intro}\nConnected github-copilot.")),
                },
                Err(e) => CommandResult::Done {
                    display: Some(format!("{intro}\nCould not connect github-copilot: {e}")),
                },
            }
        } else if provider == "chatgpt" || provider == "openai-chatgpt" {
            match self.chatgpt.connect().await {
                Ok(msg) => CommandResult::Done { display: Some(msg) },
                Err(e) => CommandResult::Done {
                    display: Some(format!("Could not connect chatgpt: {e}")),
                },
            }
        } else {
            match self.writer.prompt_and_store_key(provider).await {
                Ok(()) => CommandResult::Done {
                    display: Some(format!("Connected {provider}.")),
                },
                Err(e) => CommandResult::Done {
                    display: Some(format!("Could not connect {provider}: {e}")),
                },
            }
        }
    }
    fn name(&self) -> &str {
        "connect"
    }
    fn description(&self) -> &str {
        CONNECT_DESCRIPTION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "connect".to_string(),
            raw_args: raw.to_string(),
            positional_args: raw.split_whitespace().map(str::to_string).collect(),
        }
    }

    struct MockWriter {
        last_id: StdMutex<Option<String>>,
        result: StdMutex<Result<(), ConnectError>>,
    }
    impl MockWriter {
        fn ok() -> Self {
            Self {
                last_id: StdMutex::new(None),
                result: StdMutex::new(Ok(())),
            }
        }
        fn err(e: ConnectError) -> Self {
            Self {
                last_id: StdMutex::new(None),
                result: StdMutex::new(Err(e)),
            }
        }
    }
    #[async_trait]
    impl ConnectCredentialWriter for MockWriter {
        async fn prompt_and_store_key(&self, credential_id: &str) -> Result<(), ConnectError> {
            *self.last_id.lock().unwrap() = Some(credential_id.to_string());
            self.result.lock().unwrap().clone()
        }
    }

    struct PanicCopilot;
    #[async_trait]
    impl CopilotConnectDriver for PanicCopilot {
        async fn begin(&self, _domain: Option<&str>) -> Result<CopilotConnectStep, ConnectError> {
            panic!("api-key path must not call the copilot driver");
        }
        async fn poll_to_completion(&self, _s: &CopilotConnectStep) -> Result<(), ConnectError> {
            panic!("api-key path must not call the copilot driver");
        }
    }

    struct PanicChatGpt;
    #[async_trait]
    impl ChatGptConnectDriver for PanicChatGpt {
        async fn connect(&self) -> Result<String, ConnectError> {
            panic!("api-key path must not call the chatgpt driver");
        }
    }

    struct MockChatGpt {
        result: Result<String, ConnectError>,
    }
    impl MockChatGpt {
        fn ok(msg: impl Into<String>) -> Self {
            Self {
                result: Ok(msg.into()),
            }
        }
        fn err(e: ConnectError) -> Self {
            Self { result: Err(e) }
        }
    }
    #[async_trait]
    impl ChatGptConnectDriver for MockChatGpt {
        async fn connect(&self) -> Result<String, ConnectError> {
            self.result.clone()
        }
    }

    fn handler_with_panic_chatgpt(writer: Arc<dyn ConnectCredentialWriter>) -> ConnectHandler {
        ConnectHandler::new(writer, Arc::new(PanicCopilot), Arc::new(PanicChatGpt))
    }

    #[tokio::test]
    async fn no_arg_shows_usage() {
        let h = ConnectHandler::new(
            Arc::new(MockWriter::ok()),
            Arc::new(PanicCopilot),
            Arc::new(PanicChatGpt),
        );
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("")).await {
            assert!(s.starts_with("Usage: /connect <provider>"));
            assert!(s.contains("chatgpt"));
        } else {
            panic!("expected Done");
        }
    }

    #[tokio::test]
    async fn api_key_provider_stores_under_its_id() {
        let writer = Arc::new(MockWriter::ok());
        let h = handler_with_panic_chatgpt(writer.clone());
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("openrouter")).await {
            assert_eq!(s, "Connected openrouter.");
        } else {
            panic!("expected Done");
        }
        assert_eq!(
            writer.last_id.lock().unwrap().as_deref(),
            Some("openrouter")
        );
    }

    #[tokio::test]
    async fn api_key_store_failure_is_surfaced() {
        let writer = Arc::new(MockWriter::err(ConnectError::Storage(
            "keychain locked".into(),
        )));
        let h = handler_with_panic_chatgpt(writer);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("deepseek")).await {
            assert_eq!(
                s,
                "Could not connect deepseek: could not store credential: keychain locked"
            );
        } else {
            panic!("expected Done");
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let h = ConnectHandler::new(
            Arc::new(MockWriter::ok()),
            Arc::new(PanicCopilot),
            Arc::new(PanicChatGpt),
        );
        assert_eq!(h.name(), "connect");
        assert_eq!(h.description(), CONNECT_DESCRIPTION);
    }

    #[tokio::test]
    async fn chatgpt_provider_calls_driver_and_returns_message() {
        let h = ConnectHandler::new(
            Arc::new(MockWriter::ok()),
            Arc::new(PanicCopilot),
            Arc::new(MockChatGpt::ok("Connected chatgpt (account acct_test).")),
        );
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("chatgpt")).await {
            assert_eq!(s, "Connected chatgpt (account acct_test).");
        } else {
            panic!("expected Done");
        }
    }

    #[tokio::test]
    async fn openai_chatgpt_alias_also_routes_to_driver() {
        let h = ConnectHandler::new(
            Arc::new(MockWriter::ok()),
            Arc::new(PanicCopilot),
            Arc::new(MockChatGpt::ok("Connected chatgpt (account acct_test).")),
        );
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("openai-chatgpt")).await {
            assert_eq!(s, "Connected chatgpt (account acct_test).");
        } else {
            panic!("expected Done");
        }
    }

    #[tokio::test]
    async fn chatgpt_driver_error_is_surfaced() {
        let h = ConnectHandler::new(
            Arc::new(MockWriter::ok()),
            Arc::new(PanicCopilot),
            Arc::new(MockChatGpt::err(ConnectError::Network(
                "browser timed out".into(),
            ))),
        );
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("chatgpt")).await {
            assert!(s.starts_with("Could not connect chatgpt:"), "got: {s}");
        } else {
            panic!("expected Done");
        }
    }
}
