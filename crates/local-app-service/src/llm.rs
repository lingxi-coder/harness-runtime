//! The model the service calls on an app's behalf: free-text chat and a
//! bounded stream, with nothing but a request in and an answer out.
//!
//! The host owns auth, routing, retry and timeout; the service owns what an app
//! may ask (see [`ChatRequest`]). A host with no model simply provides none and
//! the bridge answers `llm_unavailable`.
//!
//! `AppError::LlmUnavailable` means the model could not be reached (offline,
//! auth failure, timeout); `AppError::LlmOutputRejected` means it answered and
//! the answer was unusable. Clients render them differently and offer different
//! actions, so they are never interchangeable.

use async_trait::async_trait;
use futures_core::Stream;
use local_apps::AppError;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// What the host reports of a streamed model call. The service lowers only
/// text to the page; reasoning, tool use and provider metadata stay inside the
/// host, so these are the only two facts a stream carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatStreamEvent {
    /// More of the answer's text.
    TextDelta(String),
    /// The provider's stop reason. When several are reported the last wins.
    StopReason(String),
}

/// Pull-based events of one streamed model call.
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, AppError>> + Send>>;

/// Who wrote one turn of an app-initiated chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    /// The app's own user-side prompt.
    User,
    /// A previous answer the app is replaying for context.
    Assistant,
}

/// One piece of a chat turn. Media arrives already decoded and
/// size-checked by the bridge; this layer only shapes it for the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatPart {
    /// Plain text.
    Text(String),
    /// An image the model should look at (vision).
    Image {
        /// MIME type, e.g. `image/jpeg`.
        media_type: String,
        /// Base64-encoded bytes, no `data:` prefix.
        base64: String,
    },
    /// A document (PDF) the model should read.
    Document {
        /// MIME type, e.g. `application/pdf`.
        media_type: String,
        /// Base64-encoded bytes, no `data:` prefix.
        base64: String,
    },
}

/// One turn of an app-initiated chat.
///
/// Multi-part so a photo the app just captured can be asked about directly.
/// There is deliberately no audio part: the conversation protocol has no
/// audio content block and no provider on this stack accepts raw audio in a
/// messages call, so an app that wants speech input transcribes it first
/// (`device.transcribeSpeech`) and sends text — a silently dropped audio
/// attachment would be far worse than a typed refusal.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Who wrote it.
    pub role: ChatRole,
    /// Ordered parts. Apps send no tool results.
    pub content: Vec<ChatPart>,
}

/// A free-text model call a RUNNING app asked for (`window.lingxi.v2.llm`).
///
/// Deliberately smaller than the provider surface: the model and profile are
/// NOT part of it — an app always rides whatever the user currently has
/// selected (`ApiServiceModel`'s live selection), so an app can neither pin
/// an expensive model nor route around the user's `/model` choice. Tools are
/// absent for the same reason a running app cannot reach the orchestrator: a
/// page's prompt is untrusted input, and giving it tool calls would hand
/// prompt-injected text an execution surface.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Optional system prompt written by the app's own code.
    pub system: Option<String>,
    /// Conversation so far, oldest first.
    pub messages: Vec<ChatMessage>,
    /// Output budget. An app-initiated call spends the USER's quota on the
    /// app's behalf, so it always carries an explicit cap.
    pub max_tokens: u32,
    /// Optional sampling temperature.
    pub temperature: Option<f32>,
    /// Internal cache scope for delegated media analysis reuse.
    pub cache_scope: Option<String>,
}

/// What a [`ChatRequest`] produced.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    /// The answer's text blocks, concatenated (thinking excluded).
    pub text: String,
    /// Provider stop reason, when reported.
    pub stop_reason: Option<String>,
}

/// The injectable model seam. The implementation owns auth, routing, retry
/// and timeout — callers only see a request in, an answer or a typed error
/// out.
#[async_trait]
pub trait LocalAppsModel: Send + Sync {
    /// One free-text call on behalf of a RUNNING app. No tool, no schema —
    /// see [`ChatRequest`] for what an app may and may not control.
    ///
    /// Required rather than defaulted: a double that silently answered with
    /// canned text would make a broken wiring look green, so every
    /// implementation states its behaviour.
    async fn chat(&self, request: ChatRequest) -> Result<ChatOutcome, AppError>;

    /// Open a bounded streaming call on behalf of a running app. Test doubles
    /// that only cover the legacy request/response path keep the explicit
    /// unavailable default; the production `ApiServiceModel` overrides it.
    async fn stream(&self, _request: ChatRequest) -> Result<ModelStream, AppError> {
        Err(AppError::LlmUnavailable(
            "streaming is unavailable for this model adapter".into(),
        ))
    }

    /// Update the default model/profile future calls route through —
    /// `ClientCommand::SetModel` calls this so app-initiated `llm.chat`
    /// follows a `/model` switch instead of staying pinned to whatever was
    /// live at engine build time. Default is a no-op: only
    /// [`ApiServiceModel`] (the production implementation) has a live
    /// selection to update; test doubles ignore it.
    fn set_model(&self, _model: String, _profile: Option<String>) {}
}

/// The handle over the injected model — what the broker's
/// `llm.chat` bridge operation calls through (`SharedLlm` holds one).
pub struct LocalAppsLlm {
    model: Arc<dyn LocalAppsModel>,
}

impl LocalAppsLlm {
    #[must_use]
    pub fn new(model: Arc<dyn LocalAppsModel>) -> Self {
        Self { model }
    }

    /// Follow a live `/model` switch: see [`LocalAppsModel::set_model`].
    pub fn set_model(&self, model: String, profile: Option<String>) {
        self.model.set_model(model, profile);
    }

    /// One free-text call on behalf of a running app — see [`ChatRequest`].
    ///
    /// A passthrough: there is no prompt to compose and no validator to run,
    /// because the answer is prose the app renders itself. Everything
    /// policy-shaped (declared capability, budget clamp, concurrency,
    /// truncation) lives at the bridge, where the app id is known.
    pub async fn chat(&self, request: ChatRequest) -> Result<ChatOutcome, AppError> {
        self.model.chat(request).await
    }

    /// Open a streaming side-query through the injected model adapter.
    pub async fn stream(&self, request: ChatRequest) -> Result<ModelStream, AppError> {
        self.model.stream(request).await
    }
}

/// Swappable holder for a profile's [`LocalAppsLlm`].
///
/// `ProfileApps` is cached process-wide (see `profile_apps`'s `OnceCell`),
/// but its `llm` is NOT — every consumer that reaches the model (the
/// broker's `llm.chat` bridge operation) reads through this cell instead of
/// holding its own `Arc<LocalAppsLlm>`. `clock` / `mobile_linux` stay
/// genuinely pinned to whichever connection first loaded the profile (their
/// doc above explains why); the model is different: it carries auth, and a
/// RECONNECT (a fresh `MobileEngineHandle` — possibly rotated credentials,
/// possibly a different `ApiService`) hits this exact cache-hit path with a
/// brand-new `LocalAppsLlm`. Pinning that silently would mean app-initiated
/// calls keep authenticating as a rotated-out credential with no error and
/// no log — so `profile_apps` refreshes this cell on every call, cached
/// hit or not.
///
/// This does NOT cover a live `/model` switch — `ClientCommand::SetModel`
/// never rebuilds the engine, so it never reaches `profile_apps` at all.
/// That path is fixed separately and more narrowly: `ApiServiceModel`
/// (in the runtime) holds its own model/profile behind a lock and
/// `SetModel`'s handler mutates it in place via
/// [`LocalAppsModel::set_model`] — the SAME
/// `Arc<LocalAppsLlm>` this cell holds for the connection's lifetime, no
/// swap needed.
pub struct SharedLlm(RwLock<Arc<LocalAppsLlm>>);

impl SharedLlm {
    pub fn new(llm: Arc<LocalAppsLlm>) -> Self {
        Self(RwLock::new(llm))
    }

    /// The current model. Read fresh on every use (not cached by the
    /// caller) so a swap takes effect for the very next LLM call, including
    /// one already in flight when the swap lands but that has not yet
    /// reached the model.
    pub fn current(&self) -> Arc<LocalAppsLlm> {
        self.0.read().expect("shared llm poisoned").clone()
    }

    /// Swap in a new model — called by `profile_apps` with the calling
    /// connection's own `ApiService`-backed model.
    pub fn replace(&self, llm: Arc<LocalAppsLlm>) {
        *self.0.write().expect("shared llm poisoned") = llm;
    }
}
