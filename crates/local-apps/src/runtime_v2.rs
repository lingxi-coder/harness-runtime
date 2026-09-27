//! LingXi Local Apps Runtime OS v2 contracts.
//!
//! This module is the schema-first kernel for the v2 runtime.  It deliberately
//! contains no platform implementation: native bridges, the MCP hub, and
//! background schedulers consume these contracts and remain the only places
//! allowed to perform privileged work.
//!
//! The important boundary is that an app supplies data and intent, while the
//! host supplies the capability, principal, grant, and call-chain metadata.
//! An app page cannot manufacture an [`InvocationContext`] that the host would
//! trust merely by sending matching JSON.

#![allow(missing_docs)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use thiserror::Error;

/// The only bridge API major accepted by a v2 runtime.
pub const RUNTIME_API_MAJOR: u16 = 2;
/// Human-readable runtime API version advertised to generated apps.
pub const RUNTIME_API_VERSION: &str = "2.0.0";
/// Persisted contract version for runtime-owned records.
pub const RUNTIME_CONTRACT_SCHEMA_VERSION: u32 = 1;
/// Maximum number of nested app/MCP calls in one execution chain.
pub const MAX_CALL_CHAIN_DEPTH: usize = 16;
/// Maximum number of bytes in an app Agent Profile.
pub const MAX_PROFILE_INSTRUCTION_BYTES: usize = 32 * 1024;
/// Maximum number of bytes in one stream data frame.
pub const MAX_STREAM_DATA_BYTES: usize = 256 * 1024;
/// Default bounded number of pending stream frames per app request.
pub const MAX_STREAM_BUFFER_FRAMES: usize = 64;
/// Hard host ceiling for one app Agent session's cumulative output tokens.
pub const MAX_AGENT_MAX_TOKENS: u32 = 128_000;
/// Hard host ceiling for one Agent turn wall-clock budget.
pub const MAX_AGENT_MAX_WALL_MS: u64 = 15 * 60 * 1_000;
/// Hard host ceiling for one app Agent session's turn count.
pub const MAX_AGENT_MAX_TURNS: u32 = 16;
/// Hard host ceiling for bridge calls in one Agent session.
pub const MAX_AGENT_MAX_BRIDGE_CALLS: u32 = 256;
/// Hard host ceiling for MCP calls in one Agent session.
pub const MAX_AGENT_MAX_MCP_CALLS: u32 = 256;

/// Stable capability identifiers.  This enum is the source catalog used by
/// generated SDKs and MCP tool descriptions; handlers are selected by the
/// host, never by an app payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum CapabilityId {
    DataQuery,
    DataMutate,
    NetworkRequest,
    RuntimeStatus,
    FilesRead,
    FilesWrite,
    Share,
    Clipboard,
    Calendar,
    Contacts,
    Media,
    DeviceStatus,
    Haptics,
    DeepLink,
    Camera,
    PhotoLibrary,
    Microphone,
    SpeechToText,
    TextToSpeech,
    Location,
    Notifications,
    LlmComplete,
    LlmStream,
    AgentSessionCreate,
    AgentSessionList,
    AgentSessionResume,
    AgentSessionClose,
    AgentSend,
    AgentStream,
    AgentCancel,
    AgentEmit,
    AgentProfilePropose,
    FlowExecute,
    BackgroundSchedule,
}

impl CapabilityId {
    /// Stable dot-separated identifier used by native bridge and MCP.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DataQuery => "data.query",
            Self::DataMutate => "data.mutate",
            Self::NetworkRequest => "network.request",
            Self::RuntimeStatus => "runtime.status",
            Self::FilesRead => "files.read",
            Self::FilesWrite => "files.write",
            Self::Share => "share.open",
            Self::Clipboard => "clipboard.access",
            Self::Calendar => "calendar.access",
            Self::Contacts => "contacts.access",
            Self::Media => "media.access",
            Self::DeviceStatus => "device.status",
            Self::Haptics => "device.haptics",
            Self::DeepLink => "device.deep-link",
            Self::Camera => "device.camera",
            Self::PhotoLibrary => "device.photo-library",
            Self::Microphone => "device.microphone",
            Self::SpeechToText => "device.speech-to-text",
            Self::TextToSpeech => "device.text-to-speech",
            Self::Location => "device.location",
            Self::Notifications => "device.notifications",
            Self::LlmComplete => "llm.complete",
            Self::LlmStream => "llm.stream",
            Self::AgentSessionCreate => "agent.sessions.create",
            Self::AgentSessionList => "agent.sessions.list",
            Self::AgentSessionResume => "agent.sessions.resume",
            Self::AgentSessionClose => "agent.sessions.close",
            Self::AgentSend => "agent.send",
            Self::AgentStream => "agent.stream",
            Self::AgentCancel => "agent.cancel",
            Self::AgentEmit => "agent.emit",
            Self::AgentProfilePropose => "agent.profiles.propose-update",
            Self::FlowExecute => "flow.execute",
            Self::BackgroundSchedule => "background.schedule",
        }
    }

    /// All catalog entries in deterministic identifier order.
    pub const ALL: [Self; 34] = [
        Self::DataQuery,
        Self::DataMutate,
        Self::NetworkRequest,
        Self::RuntimeStatus,
        Self::FilesRead,
        Self::FilesWrite,
        Self::Share,
        Self::Clipboard,
        Self::Calendar,
        Self::Contacts,
        Self::Media,
        Self::DeviceStatus,
        Self::Haptics,
        Self::DeepLink,
        Self::Camera,
        Self::PhotoLibrary,
        Self::Microphone,
        Self::SpeechToText,
        Self::TextToSpeech,
        Self::Location,
        Self::Notifications,
        Self::LlmComplete,
        Self::LlmStream,
        Self::AgentSessionCreate,
        Self::AgentSessionList,
        Self::AgentSessionResume,
        Self::AgentSessionClose,
        Self::AgentSend,
        Self::AgentStream,
        Self::AgentCancel,
        Self::AgentEmit,
        Self::AgentProfilePropose,
        Self::FlowExecute,
        Self::BackgroundSchedule,
    ];
}

/// Physical transport used by the host for one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTransport {
    NativeBridge,
    AppMcp,
    SystemBackground,
}

/// Principal that initiated an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOrigin {
    PageForeground,
    ConversationAgent,
    AppRuntimeHeadless,
    SystemScheduler,
}

/// Whether a capability needs a foreground interaction or user prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityScope {
    App,
    Turn,
    Session,
    Profile,
    System,
}

/// Schema entry exposed to generated SDK/docs and the MCP catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub transport: CapabilityTransport,
    pub scope: CapabilityScope,
    pub requires_user_approval: bool,
    pub interactive: bool,
    pub supports_stream: bool,
    pub description: &'static str,
}

/// The deterministic host-owned capability registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRegistry {
    descriptors: BTreeMap<&'static str, CapabilityDescriptor>,
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        let mut descriptors = BTreeMap::new();
        for id in CapabilityId::ALL {
            let descriptor = descriptor_for(id);
            descriptors.insert(id.as_str(), descriptor);
        }
        Self { descriptors }
    }
}

impl Serialize for CapabilityId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CapabilityId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::ALL
            .into_iter()
            .find(|id| id.as_str() == value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown capability {value:?}")))
    }
}

impl CapabilityRegistry {
    /// Return the stable descriptor for one capability.
    #[must_use]
    pub fn get(&self, id: CapabilityId) -> Option<&CapabilityDescriptor> {
        self.descriptors.get(id.as_str())
    }

    /// Return every descriptor in stable identifier order.
    #[must_use]
    pub fn descriptors(&self) -> impl Iterator<Item = &CapabilityDescriptor> {
        self.descriptors.values()
    }

    /// Resolve a wire identifier without permitting an app to select a
    /// handler or arbitrary namespace.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<&CapabilityDescriptor> {
        self.descriptors.get(id)
    }

    /// Serialize the generated catalog deterministically for SDK/doc checks.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.descriptors.values().collect::<Vec<_>>())
    }
}

fn descriptor_for(id: CapabilityId) -> CapabilityDescriptor {
    let (transport, scope, requires_user_approval, interactive, supports_stream) = match id {
        CapabilityId::DataQuery | CapabilityId::RuntimeStatus | CapabilityId::DeviceStatus => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::App,
            false,
            false,
            false,
        ),
        CapabilityId::DataMutate
        | CapabilityId::FilesWrite
        | CapabilityId::Clipboard
        | CapabilityId::Notifications => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::App,
            true,
            false,
            false,
        ),
        CapabilityId::Calendar
        | CapabilityId::Contacts
        | CapabilityId::Share
        | CapabilityId::DeepLink => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::App,
            true,
            true,
            false,
        ),
        CapabilityId::NetworkRequest | CapabilityId::FilesRead | CapabilityId::Haptics => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::Turn,
            true,
            false,
            false,
        ),
        CapabilityId::Media
        | CapabilityId::Camera
        | CapabilityId::PhotoLibrary
        | CapabilityId::Microphone
        | CapabilityId::SpeechToText
        | CapabilityId::Location => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::Turn,
            true,
            true,
            false,
        ),
        CapabilityId::TextToSpeech => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::Turn,
            true,
            false,
            false,
        ),
        CapabilityId::LlmComplete => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::Turn,
            true,
            false,
            false,
        ),
        CapabilityId::LlmStream => (
            CapabilityTransport::NativeBridge,
            CapabilityScope::Turn,
            true,
            false,
            true,
        ),
        CapabilityId::AgentSessionCreate
        | CapabilityId::AgentSessionList
        | CapabilityId::AgentSessionResume
        | CapabilityId::AgentSessionClose
        | CapabilityId::AgentSend
        | CapabilityId::AgentStream
        | CapabilityId::AgentCancel
        | CapabilityId::AgentEmit => (
            CapabilityTransport::AppMcp,
            CapabilityScope::Session,
            true,
            false,
            id == CapabilityId::AgentStream,
        ),
        CapabilityId::AgentProfilePropose => (
            CapabilityTransport::AppMcp,
            CapabilityScope::Profile,
            false,
            false,
            false,
        ),
        CapabilityId::FlowExecute => (
            CapabilityTransport::AppMcp,
            CapabilityScope::Turn,
            true,
            false,
            false,
        ),
        CapabilityId::BackgroundSchedule => (
            CapabilityTransport::SystemBackground,
            CapabilityScope::App,
            true,
            false,
            false,
        ),
    };
    CapabilityDescriptor {
        id,
        transport,
        scope,
        requires_user_approval,
        interactive,
        supports_stream,
        description: description_for(id),
    }
}

const fn description_for(id: CapabilityId) -> &'static str {
    match id {
        CapabilityId::DataQuery => "Query host-owned app records",
        CapabilityId::DataMutate => "Create, update, or delete app records",
        CapabilityId::NetworkRequest => "Make an allow-listed HTTPS request",
        CapabilityId::RuntimeStatus => "Inspect local app runtime status",
        CapabilityId::FilesRead => "Read an app-scoped file handle",
        CapabilityId::FilesWrite => "Write an app-scoped file handle",
        CapabilityId::Share => "Open the system share sheet",
        CapabilityId::Clipboard => "Read or write the app clipboard contract",
        CapabilityId::Calendar => "Read bounded calendar event projections",
        CapabilityId::Contacts => "Search bounded contact projections",
        CapabilityId::Media => "Read an app-owned retained media handle",
        CapabilityId::DeviceStatus => "Inspect non-sensitive device status",
        CapabilityId::Haptics => "Request bounded device haptics",
        CapabilityId::DeepLink => "Open an approved external deep link",
        CapabilityId::Camera => "Capture a camera image handle",
        CapabilityId::PhotoLibrary => "Pick a photo handle",
        CapabilityId::Microphone => "Record bounded microphone audio",
        CapabilityId::SpeechToText => "Listen once and transcribe speech from the microphone",
        CapabilityId::TextToSpeech => "Speak text through the host voice service",
        CapabilityId::Location => "Read a one-shot location fix",
        CapabilityId::Notifications => "Post a local notification",
        CapabilityId::LlmComplete => "Run a bounded side-call without tools",
        CapabilityId::LlmStream => "Stream a bounded side-call without tools",
        CapabilityId::AgentSessionCreate => "Create a persistent app Agent session",
        CapabilityId::AgentSessionList => "List app-owned Agent sessions",
        CapabilityId::AgentSessionResume => "Resume an app-owned Agent session",
        CapabilityId::AgentSessionClose => "Close an app-owned Agent session",
        CapabilityId::AgentSend => "Send a turn to an app-owned Agent session",
        CapabilityId::AgentStream => "Stream an app-owned Agent turn",
        CapabilityId::AgentCancel => "Cancel an app-owned Agent turn",
        CapabilityId::AgentEmit => {
            "Emit a structured event to the conversation or an app-owned Agent session"
        }
        CapabilityId::AgentProfilePropose => "Propose a future App Agent Profile revision",
        CapabilityId::FlowExecute => "Execute a bounded declarative app flow",
        CapabilityId::BackgroundSchedule => "Schedule an authorized app background flow",
    }
}

/// Host-created authorization and attribution metadata for every call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvocationContext {
    pub app_id: String,
    pub app_instance_id: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub origin: InvocationOrigin,
    pub grant_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_chain: Vec<InvocationFrame>,
}

/// One parent edge in a nested app/Agent/MCP call chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvocationFrame {
    pub app_id: String,
    pub capability: CapabilityId,
    pub input_hash: String,
}

impl InvocationContext {
    /// Validate host-generated metadata before routing a privileged call.
    pub fn validate(&self) -> Result<(), RuntimeContractError> {
        crate::ids::validate_app_id(&self.app_id)
            .map_err(|error| RuntimeContractError::InvalidContext(error.to_string()))?;
        bounded_identifier("appInstanceId", &self.app_instance_id, 128)?;
        bounded_identifier("requestId", &self.request_id, 128)?;
        if let Some(turn_id) = &self.turn_id {
            bounded_identifier("turnId", turn_id, 128)?;
        }
        if self.grant_epoch == 0 {
            return Err(RuntimeContractError::InvalidContext(
                "grantEpoch must be non-zero".into(),
            ));
        }
        if self.call_chain.len() > MAX_CALL_CHAIN_DEPTH {
            return Err(RuntimeContractError::CallChainTooDeep);
        }
        for frame in &self.call_chain {
            crate::ids::validate_app_id(&frame.app_id)
                .map_err(|error| RuntimeContractError::InvalidContext(error.to_string()))?;
            if frame.input_hash.len() != 64
                || !frame.input_hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(RuntimeContractError::InvalidContext(
                    "call-chain inputHash must be a SHA-256 hex digest".into(),
                ));
            }
        }
        Ok(())
    }

    /// Return a stable recursion/replay key for one capability and payload.
    #[must_use]
    pub fn call_key(&self, capability: CapabilityId, input_json: &str) -> String {
        let input_hash = normalized_input_hash(input_json);
        format!("{}:{}:{}", self.app_id, capability.as_str(), input_hash)
    }
}

/// Hash JSON after parsing and re-serializing so equivalent object key order
/// cannot bypass recursion or replay limits.
#[must_use]
pub fn normalized_input_hash(input_json: &str) -> String {
    let normalized = serde_json::from_str::<serde_json::Value>(input_json)
        .ok()
        .map(canonicalize_json)
        .and_then(|value| serde_json::to_vec(&value).ok())
        .unwrap_or_else(|| input_json.as_bytes().to_vec());
    format!("{:x}", Sha256::digest(normalized))
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut normalized = serde_json::Map::new();
            for (key, value) in entries {
                normalized.insert(key, canonicalize_json(value));
            }
            serde_json::Value::Object(normalized)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        other => other,
    }
}

/// A per-execution replay guard.  The host should create one for each turn or
/// background run and never reuse it across independent authorizations.
#[derive(Debug, Default)]
pub struct InvocationReplayGuard {
    seen: BTreeSet<String>,
}

impl InvocationReplayGuard {
    /// Admit a key once; repeated calls are typed as replay/recursion.
    pub fn admit(&mut self, key: impl Into<String>) -> Result<(), RuntimeContractError> {
        if self.seen.insert(key.into()) {
            Ok(())
        } else {
            Err(RuntimeContractError::ReplayDetected)
        }
    }
}

/// Ordered stream frames used by `llm.stream` and `agent.stream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamFrame {
    Started {
        stream_id: String,
        request_id: String,
    },
    Data {
        seq: u64,
        data_json: String,
    },
    Completed {
        seq: u64,
    },
    Error {
        seq: u64,
        code: String,
        message: String,
    },
    Cancelled {
        seq: u64,
        reason: String,
    },
}

/// Host-side stream protocol validator.
#[derive(Debug, Default)]
pub struct StreamValidator {
    started: bool,
    terminal: bool,
    next_seq: u64,
}

impl StreamValidator {
    /// Accept one frame and reject gaps, duplicate terminals, or oversized
    /// deltas before forwarding to a WebView/client.
    pub fn accept(&mut self, frame: &StreamFrame) -> Result<(), RuntimeContractError> {
        if self.terminal {
            return Err(RuntimeContractError::StreamClosed);
        }
        match frame {
            StreamFrame::Started {
                stream_id,
                request_id,
            } => {
                if self.started || stream_id.is_empty() || request_id.is_empty() {
                    return Err(RuntimeContractError::StreamOrder);
                }
                self.started = true;
            }
            StreamFrame::Data { seq, data_json } => {
                if !self.started || *seq != self.next_seq {
                    return Err(RuntimeContractError::StreamOrder);
                }
                if data_json.len() > MAX_STREAM_DATA_BYTES {
                    return Err(RuntimeContractError::StreamDataTooLarge);
                }
                self.next_seq = self.next_seq.saturating_add(1);
            }
            StreamFrame::Completed { seq }
            | StreamFrame::Error { seq, .. }
            | StreamFrame::Cancelled { seq, .. } => {
                if !self.started || *seq != self.next_seq {
                    return Err(RuntimeContractError::StreamOrder);
                }
                self.terminal = true;
            }
        }
        Ok(())
    }
}

/// Bounded producer/consumer buffer for native bridge stream adapters.
///
/// The producer must stop or cancel when [`Self::push`] returns
/// [`RuntimeContractError::StreamBackpressure`]; unbounded buffering is not a
/// valid fallback for a mobile WebView that has gone backgrounded.
#[derive(Debug)]
pub struct StreamBuffer {
    capacity: usize,
    frames: VecDeque<StreamFrame>,
    cancelled: bool,
}

impl StreamBuffer {
    /// Create a bounded buffer. Capacities above the runtime limit are capped.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_STREAM_BUFFER_FRAMES),
            frames: VecDeque::new(),
            cancelled: false,
        }
    }

    /// Enqueue one frame or report backpressure/cancellation.
    pub fn push(&mut self, frame: StreamFrame) -> Result<(), RuntimeContractError> {
        if self.cancelled {
            return Err(RuntimeContractError::StreamCancelled);
        }
        if self.frames.len() >= self.capacity {
            return Err(RuntimeContractError::StreamBackpressure);
        }
        self.frames.push_back(frame);
        Ok(())
    }

    /// Consume the oldest pending frame.
    pub fn pop(&mut self) -> Option<StreamFrame> {
        self.frames.pop_front()
    }

    /// Cancel the stream and discard queued frames.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.frames.clear();
    }

    /// Number of pending frames.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// True when no frames are pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Persisted app Agent Profile.  It is intentionally separate from the
/// platform system prompt and can only be installed through approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppAgentProfile {
    pub schema_version: u32,
    pub app_id: String,
    pub revision: u64,
    pub instructions: String,
    pub updated_at_ms: u64,
}

impl AppAgentProfile {
    /// Create the initial empty profile for a generated app.
    #[must_use]
    pub fn empty(app_id: impl Into<String>, now_ms: u64) -> Self {
        Self {
            schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
            app_id: app_id.into(),
            revision: 0,
            instructions: String::new(),
            updated_at_ms: now_ms,
        }
    }

    /// Validate the profile without interpreting its text as host policy.
    pub fn validate(&self) -> Result<(), RuntimeContractError> {
        if self.schema_version != RUNTIME_CONTRACT_SCHEMA_VERSION {
            return Err(RuntimeContractError::SchemaVersion {
                found: self.schema_version,
                expected: RUNTIME_CONTRACT_SCHEMA_VERSION,
            });
        }
        crate::ids::validate_app_id(&self.app_id)
            .map_err(|error| RuntimeContractError::InvalidContext(error.to_string()))?;
        if self.instructions.len() > MAX_PROFILE_INSTRUCTION_BYTES {
            return Err(RuntimeContractError::ProfileTooLarge);
        }
        Ok(())
    }
}

/// App-proposed profile revision.  A proposal never changes the active
/// profile; only a separate user-approved apply operation can do that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppAgentProfileProposal {
    pub app_id: String,
    pub base_revision: u64,
    pub instructions: String,
    pub reason: String,
}

/// Apply a proposal after the client has recorded explicit user approval.
pub fn apply_approved_profile(
    current: &AppAgentProfile,
    proposal: &AppAgentProfileProposal,
    user_approved: bool,
    now_ms: u64,
) -> Result<AppAgentProfile, RuntimeContractError> {
    current.validate()?;
    if !user_approved {
        return Err(RuntimeContractError::ApprovalRequired);
    }
    if proposal.app_id != current.app_id || proposal.base_revision != current.revision {
        return Err(RuntimeContractError::ProfileRevisionConflict);
    }
    let next = AppAgentProfile {
        schema_version: current.schema_version,
        app_id: current.app_id.clone(),
        revision: current.revision.saturating_add(1),
        instructions: proposal.instructions.clone(),
        updated_at_ms: now_ms,
    };
    next.validate()?;
    Ok(next)
}

/// Trusted/untrusted prompt layer kinds.  Core and runtime policy are supplied
/// by the host; app/session/event text cannot replace them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptLayerKind {
    PlatformCore,
    RuntimeSecurityPolicy,
    AppAgentProfile,
    SessionGoal,
    TurnContext,
}

/// One assembled prompt layer, with trust metadata retained for auditing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptLayer {
    pub kind: PromptLayerKind,
    pub text: String,
    pub trusted: bool,
    pub profile_revision: Option<u64>,
}

/// Compose the only valid layer order for an app Agent turn.
#[must_use]
pub fn compose_prompt_layers(
    platform_core: impl Into<String>,
    runtime_policy: impl Into<String>,
    profile: &AppAgentProfile,
    session_goal: Option<String>,
    turn_context: Option<String>,
) -> Vec<PromptLayer> {
    let mut layers = vec![
        PromptLayer {
            kind: PromptLayerKind::PlatformCore,
            text: platform_core.into(),
            trusted: true,
            profile_revision: None,
        },
        PromptLayer {
            kind: PromptLayerKind::RuntimeSecurityPolicy,
            text: runtime_policy.into(),
            trusted: true,
            profile_revision: None,
        },
        PromptLayer {
            kind: PromptLayerKind::AppAgentProfile,
            text: profile.instructions.clone(),
            trusted: true,
            profile_revision: Some(profile.revision),
        },
    ];
    if let Some(text) = session_goal {
        layers.push(PromptLayer {
            kind: PromptLayerKind::SessionGoal,
            text,
            trusted: false,
            profile_revision: None,
        });
    }
    if let Some(text) = turn_context {
        layers.push(PromptLayer {
            kind: PromptLayerKind::TurnContext,
            text,
            trusted: false,
            profile_revision: None,
        });
    }
    layers
}

/// Bounded budgets applied to both foreground and headless Agent sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBudget {
    pub max_tokens: u32,
    pub max_wall_ms: u64,
    pub max_turns: u32,
    pub max_bridge_calls: u32,
    pub max_mcp_calls: u32,
    pub max_recursion_depth: u16,
}

impl Default for AgentBudget {
    fn default() -> Self {
        Self {
            max_tokens: 32_000,
            max_wall_ms: 120_000,
            max_turns: 8,
            max_bridge_calls: 64,
            max_mcp_calls: 64,
            max_recursion_depth: MAX_CALL_CHAIN_DEPTH as u16,
        }
    }
}

impl AgentBudget {
    /// Clamp an app-requested budget to host-owned resource ceilings.
    ///
    /// Apps may request a smaller budget, but never enlarge the platform
    /// limits by serializing a larger number into the create request.
    pub fn clamp_to_host_limits(&mut self) {
        self.max_tokens = self.max_tokens.clamp(1, MAX_AGENT_MAX_TOKENS);
        self.max_wall_ms = self.max_wall_ms.clamp(1, MAX_AGENT_MAX_WALL_MS);
        self.max_turns = self.max_turns.clamp(1, MAX_AGENT_MAX_TURNS);
        self.max_bridge_calls = self.max_bridge_calls.clamp(1, MAX_AGENT_MAX_BRIDGE_CALLS);
        self.max_mcp_calls = self.max_mcp_calls.clamp(1, MAX_AGENT_MAX_MCP_CALLS);
        self.max_recursion_depth = self
            .max_recursion_depth
            .clamp(1, MAX_CALL_CHAIN_DEPTH as u16);
    }
}

/// Persistent Agent session state owned by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRecord {
    pub schema_version: u32,
    pub session_id: String,
    pub app_id: String,
    pub app_instance_id: String,
    pub status: AgentSessionStatus,
    pub prompt_profile_revision: u64,
    pub budget: AgentBudget,
    pub turn_count: u32,
    /// Cumulative output-token estimate consumed by this session.
    #[serde(default)]
    pub output_tokens_used: u64,
    /// Cumulative host bridge calls consumed by this session.
    #[serde(default)]
    pub bridge_calls_used: u32,
    /// Cumulative MCP calls consumed by this session.
    #[serde(default)]
    pub mcp_calls_used: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Lifecycle state of a persistent app Agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    Active,
    Paused,
    Closed,
}

impl AgentSessionRecord {
    /// Validate ownership and budget invariants before a resume/send.
    pub fn validate(&self) -> Result<(), RuntimeContractError> {
        if self.schema_version != RUNTIME_CONTRACT_SCHEMA_VERSION {
            return Err(RuntimeContractError::SchemaVersion {
                found: self.schema_version,
                expected: RUNTIME_CONTRACT_SCHEMA_VERSION,
            });
        }
        crate::ids::validate_app_id(&self.app_id)
            .map_err(|error| RuntimeContractError::InvalidContext(error.to_string()))?;
        bounded_identifier("sessionId", &self.session_id, 128)?;
        bounded_identifier("appInstanceId", &self.app_instance_id, 128)?;
        if self.budget.max_recursion_depth == 0
            || usize::from(self.budget.max_recursion_depth) > MAX_CALL_CHAIN_DEPTH
            || self.budget.max_tokens == 0
            || self.budget.max_wall_ms == 0
            || self.budget.max_turns == 0
            || self.budget.max_bridge_calls == 0
            || self.budget.max_mcp_calls == 0
            || self.budget.max_tokens > MAX_AGENT_MAX_TOKENS
            || self.budget.max_wall_ms > MAX_AGENT_MAX_WALL_MS
            || self.budget.max_turns > MAX_AGENT_MAX_TURNS
            || self.budget.max_bridge_calls > MAX_AGENT_MAX_BRIDGE_CALLS
            || self.budget.max_mcp_calls > MAX_AGENT_MAX_MCP_CALLS
            || self.output_tokens_used > u64::from(self.budget.max_tokens)
            || self.bridge_calls_used > self.budget.max_bridge_calls
            || self.mcp_calls_used > self.budget.max_mcp_calls
        {
            return Err(RuntimeContractError::BudgetInvalid);
        }
        Ok(())
    }
}

/// A declarative, acyclic app flow.  There is no arbitrary JavaScript in this
/// contract; each step resolves to a registered capability or fixed CRUD op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowDefinition {
    pub flow_id: String,
    pub version: u64,
    pub steps: Vec<FlowStep>,
}

/// One declarative flow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowStep {
    pub step_id: String,
    pub capability: CapabilityId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    pub input_json: String,
}

impl FlowDefinition {
    /// Validate ids, registered capabilities, dependency references, and
    /// acyclicity before a scheduler or MCP tool accepts the flow.
    pub fn validate(&self, registry: &CapabilityRegistry) -> Result<(), RuntimeContractError> {
        bounded_identifier("flowId", &self.flow_id, 128)?;
        if self.steps.is_empty() || self.steps.len() > 128 {
            return Err(RuntimeContractError::FlowInvalid(
                "flow must contain 1..=128 steps".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            bounded_identifier("stepId", &step.step_id, 128)?;
            if !ids.insert(step.step_id.as_str()) {
                return Err(RuntimeContractError::FlowInvalid(
                    "flow contains duplicate step ids".into(),
                ));
            }
            if registry.get(step.capability).is_none() {
                return Err(RuntimeContractError::FlowInvalid(format!(
                    "capability {} is not registered",
                    step.capability.as_str()
                )));
            }
            if step.input_json.len() > MAX_STREAM_DATA_BYTES {
                return Err(RuntimeContractError::FlowInvalid(
                    "flow step input exceeds the bounded payload limit".into(),
                ));
            }
            if serde_json::from_str::<serde_json::Value>(&step.input_json).is_err() {
                return Err(RuntimeContractError::FlowInvalid(
                    "flow step input must be valid JSON".into(),
                ));
            }
            if step
                .depends_on
                .iter()
                .any(|dependency| dependency == &step.step_id)
            {
                return Err(RuntimeContractError::FlowCycle);
            }
            if step
                .depends_on
                .iter()
                .any(|dependency| !ids.contains(dependency.as_str()))
            {
                return Err(RuntimeContractError::FlowInvalid(
                    "flow dependencies must reference an earlier step".into(),
                ));
            }
        }
        Ok(())
    }
}

/// System-background trigger supported by the host scheduler adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackgroundTrigger {
    Event { topic: String },
    Schedule { interval_ms: u64 },
}

/// Persisted background task definition and resumable status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskRecord {
    pub schema_version: u32,
    pub task_id: String,
    pub app_id: String,
    pub flow_id: String,
    pub flow: FlowDefinition,
    pub trigger: BackgroundTrigger,
    pub status: BackgroundTaskStatus,
    pub updated_at_ms: u64,
}

/// Background task status.  `Running` must always have a journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Scheduled,
    Running,
    WaitingForSystem,
    Succeeded,
    Failed,
    Cancelled,
}

/// Journal entry allowing Android/iOS task expiration to resume safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundJournalEntry {
    pub task_id: String,
    pub flow_id: String,
    pub next_step_id: Option<String>,
    /// Earliest time a scheduler wake-up may claim this entry. Older journal
    /// files omit this field and are treated as immediately runnable.
    #[serde(default)]
    pub next_run_at_ms: Option<u64>,
    /// Bounded result of the most recent completed run, retained across
    /// process death so native schedulers do not lose background output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result_json: Option<String>,
    pub attempt: u32,
    pub last_error: Option<String>,
    pub updated_at_ms: u64,
}

/// A headless invocation is never allowed to start a foreground interaction.
#[must_use]
pub fn allowed_for_origin(origin: InvocationOrigin, descriptor: &CapabilityDescriptor) -> bool {
    !(matches!(
        origin,
        InvocationOrigin::AppRuntimeHeadless | InvocationOrigin::SystemScheduler
    ) && descriptor.interactive)
}

/// Synchronous `flow.execute` may not start UI-bound or unpaired lifecycle
/// capabilities. Background scheduling already uses [`allowed_for_origin`].
#[must_use]
pub fn allowed_for_synchronous_flow(capability: CapabilityId) -> bool {
    !matches!(
        capability,
        CapabilityId::FlowExecute
            | CapabilityId::BackgroundSchedule
            | CapabilityId::LlmStream
            | CapabilityId::AgentStream
            | CapabilityId::AgentCancel
            | CapabilityId::Camera
            | CapabilityId::PhotoLibrary
            | CapabilityId::Microphone
            | CapabilityId::SpeechToText
            | CapabilityId::Share
    )
}

fn bounded_identifier(
    name: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), RuntimeContractError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(RuntimeContractError::InvalidContext(format!(
            "{name} must be non-empty and at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

/// Contract failures are stable enough for host/client error mapping.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RuntimeContractError {
    #[error("invalid invocation context: {0}")]
    InvalidContext(String),
    #[error("call chain is too deep")]
    CallChainTooDeep,
    #[error("replayed or recursive invocation")]
    ReplayDetected,
    #[error("stream frame order is invalid")]
    StreamOrder,
    #[error("stream is already closed")]
    StreamClosed,
    #[error("stream frame is too large")]
    StreamDataTooLarge,
    #[error("stream requires user approval")]
    ApprovalRequired,
    #[error("profile revision conflicts with the active profile")]
    ProfileRevisionConflict,
    #[error("profile instructions exceed the bounded size")]
    ProfileTooLarge,
    #[error("invalid runtime budget")]
    BudgetInvalid,
    #[error("flow contains a cycle")]
    FlowCycle,
    #[error("invalid flow: {0}")]
    FlowInvalid(String),
    #[error("stream producer is ahead of the bounded consumer")]
    StreamBackpressure,
    #[error("stream was cancelled")]
    StreamCancelled,
    #[error("schema version {found} is unsupported; expected {expected}")]
    SchemaVersion { found: u32, expected: u32 },
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> InvocationContext {
        InvocationContext {
            app_id: "abc12345".into(),
            app_instance_id: "instance-1".into(),
            request_id: "request-1".into(),
            turn_id: Some("turn-1".into()),
            origin: InvocationOrigin::ConversationAgent,
            grant_epoch: 1,
            capability_instance: None,
            call_chain: vec![],
        }
    }

    #[test]
    fn registry_is_complete_and_deterministic() {
        let registry = CapabilityRegistry::default();
        let ids: Vec<_> = registry
            .descriptors()
            .map(|entry| entry.id.as_str())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        assert_eq!(ids.len(), CapabilityId::ALL.len());
        assert!(registry.resolve("llm.stream").unwrap().supports_stream);
        assert!(registry
            .to_json()
            .unwrap()
            .contains("agent.sessions.create"));
        assert!(registry
            .get(CapabilityId::SpeechToText)
            .expect("speech capability")
            .description
            .contains("microphone"));
        assert!(registry.resolve("background.resume").is_none());
    }

    #[test]
    fn headless_origins_cannot_start_foreground_capabilities() {
        let registry = CapabilityRegistry::default();
        for capability in [
            CapabilityId::Calendar,
            CapabilityId::Contacts,
            CapabilityId::Share,
            CapabilityId::DeepLink,
            CapabilityId::Media,
            CapabilityId::Camera,
            CapabilityId::PhotoLibrary,
            CapabilityId::Microphone,
            CapabilityId::SpeechToText,
            CapabilityId::Location,
        ] {
            assert!(!allowed_for_origin(
                InvocationOrigin::SystemScheduler,
                registry.get(capability).expect("registered capability")
            ));
        }
        assert!(allowed_for_origin(
            InvocationOrigin::SystemScheduler,
            registry
                .get(CapabilityId::TextToSpeech)
                .expect("registered capability")
        ));
    }

    #[test]
    fn synchronous_flow_rejects_ui_and_unpaired_lifecycle_capabilities() {
        for capability in [
            CapabilityId::Camera,
            CapabilityId::PhotoLibrary,
            CapabilityId::Microphone,
            CapabilityId::SpeechToText,
            CapabilityId::Share,
            CapabilityId::FlowExecute,
            CapabilityId::BackgroundSchedule,
            CapabilityId::LlmStream,
            CapabilityId::AgentStream,
            CapabilityId::AgentCancel,
        ] {
            assert!(
                !allowed_for_synchronous_flow(capability),
                "{capability:?} must stay out of a synchronous flow"
            );
        }
        assert!(allowed_for_synchronous_flow(CapabilityId::DataQuery));
        assert!(allowed_for_synchronous_flow(CapabilityId::Calendar));
        assert!(allowed_for_synchronous_flow(CapabilityId::Haptics));
    }

    #[test]
    fn context_rejects_missing_grant_and_deep_chain() {
        let mut value = context();
        value.grant_epoch = 0;
        assert!(matches!(
            value.validate(),
            Err(RuntimeContractError::InvalidContext(_))
        ));
        value.grant_epoch = 1;
        value.call_chain = (0..=MAX_CALL_CHAIN_DEPTH)
            .map(|_| InvocationFrame {
                app_id: "abc12345".into(),
                capability: CapabilityId::AgentSend,
                input_hash: "0".repeat(64),
            })
            .collect();
        assert_eq!(
            value.validate(),
            Err(RuntimeContractError::CallChainTooDeep)
        );
    }

    #[test]
    fn agent_budget_is_clamped_to_host_limits() {
        let mut budget = AgentBudget {
            max_tokens: u32::MAX,
            max_wall_ms: u64::MAX,
            max_turns: u32::MAX,
            max_bridge_calls: u32::MAX,
            max_mcp_calls: u32::MAX,
            max_recursion_depth: u16::MAX,
        };
        budget.clamp_to_host_limits();
        assert_eq!(budget.max_tokens, MAX_AGENT_MAX_TOKENS);
        assert_eq!(budget.max_wall_ms, MAX_AGENT_MAX_WALL_MS);
        assert_eq!(budget.max_turns, MAX_AGENT_MAX_TURNS);
        assert_eq!(budget.max_bridge_calls, MAX_AGENT_MAX_BRIDGE_CALLS);
        assert_eq!(budget.max_mcp_calls, MAX_AGENT_MAX_MCP_CALLS);
        assert_eq!(budget.max_recursion_depth, MAX_CALL_CHAIN_DEPTH as u16);
        assert!(budget.max_tokens > 0);
    }

    #[test]
    fn normalized_hash_and_replay_guard_close_equivalent_replays() {
        assert_eq!(
            normalized_input_hash(r#"{"b":2,"a":1}"#),
            normalized_input_hash(r#"{"a":1,"b":2}"#)
        );
        let mut guard = InvocationReplayGuard::default();
        guard.admit("abc").unwrap();
        assert_eq!(
            guard.admit("abc"),
            Err(RuntimeContractError::ReplayDetected)
        );
    }

    #[test]
    fn stream_validator_enforces_order_and_terminal_state() {
        let mut validator = StreamValidator::default();
        assert!(validator
            .accept(&StreamFrame::Data {
                seq: 0,
                data_json: "{}".into()
            })
            .is_err());
        validator
            .accept(&StreamFrame::Started {
                stream_id: "s".into(),
                request_id: "r".into(),
            })
            .unwrap();
        validator
            .accept(&StreamFrame::Data {
                seq: 0,
                data_json: "{}".into(),
            })
            .unwrap();
        validator
            .accept(&StreamFrame::Completed { seq: 1 })
            .unwrap();
        assert_eq!(
            validator.accept(&StreamFrame::Completed { seq: 1 }),
            Err(RuntimeContractError::StreamClosed)
        );
    }

    #[test]
    fn stream_buffer_is_bounded_and_cancellable() {
        let mut buffer = StreamBuffer::new(1);
        buffer
            .push(StreamFrame::Started {
                stream_id: "s".into(),
                request_id: "r".into(),
            })
            .unwrap();
        assert_eq!(
            buffer.push(StreamFrame::Data {
                seq: 0,
                data_json: "{}".into(),
            }),
            Err(RuntimeContractError::StreamBackpressure)
        );
        assert!(buffer.pop().is_some());
        buffer.cancel();
        assert_eq!(buffer.len(), 0);
        assert_eq!(
            buffer.push(StreamFrame::Completed { seq: 0 }),
            Err(RuntimeContractError::StreamCancelled)
        );
    }

    #[test]
    fn profile_requires_approval_and_applies_next_revision() {
        let profile = AppAgentProfile::empty("abc12345", 1);
        let proposal = AppAgentProfileProposal {
            app_id: "abc12345".into(),
            base_revision: 0,
            instructions: "Use the app's domain vocabulary".into(),
            reason: "specialized app behavior".into(),
        };
        assert_eq!(
            apply_approved_profile(&profile, &proposal, false, 2),
            Err(RuntimeContractError::ApprovalRequired)
        );
        let updated = apply_approved_profile(&profile, &proposal, true, 2).unwrap();
        assert_eq!(updated.revision, 1);
        let layers = compose_prompt_layers(
            "core",
            "policy",
            &updated,
            Some("goal".into()),
            Some("event".into()),
        );
        assert_eq!(layers[0].kind, PromptLayerKind::PlatformCore);
        assert_eq!(layers[2].profile_revision, Some(1));
        assert!(!layers[4].trusted);
    }

    #[test]
    fn flow_validation_is_declarative_and_acyclic() {
        let registry = CapabilityRegistry::default();
        let valid = FlowDefinition {
            flow_id: "sync".into(),
            version: 1,
            steps: vec![
                FlowStep {
                    step_id: "read".into(),
                    capability: CapabilityId::DataQuery,
                    depends_on: vec![],
                    input_json: "{}".into(),
                },
                FlowStep {
                    step_id: "send".into(),
                    capability: CapabilityId::AgentEmit,
                    depends_on: vec!["read".into()],
                    input_json: "{}".into(),
                },
            ],
        };
        valid.validate(&registry).unwrap();
        let cycle = FlowDefinition {
            steps: vec![FlowStep {
                step_id: "a".into(),
                capability: CapabilityId::DataQuery,
                depends_on: vec!["a".into()],
                input_json: "{}".into(),
            }],
            ..valid
        };
        assert_eq!(
            cycle.validate(&registry),
            Err(RuntimeContractError::FlowCycle)
        );
    }
}
