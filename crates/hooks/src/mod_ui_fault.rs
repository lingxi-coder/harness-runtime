//! Host-side projection of the Native Mods Client UI fault lifecycle.
//!
//! This module is a typed seam for a real UI renderer. The renderer must
//! register the Client identities that its current parent render actually
//! draws; session attach/detach state is not a substitute. A caller should
//! wire [`dispatch_client_ui_fault`] to the session-owned ModHost and to a
//! renderer callback that invalidates one parent render site.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;

use lingxi_core::types::utf16_json::{Utf16JsonProjection, Utf16JsonString};
use serde_json::{Value, json};
use thiserror::Error;

const MAX_ADDRESS_UTF16: usize = 256;
const MAX_REASON_UTF16: usize = 200;
const DRAWN_SITE_LIMIT: usize = 1024;
const FAULT_RUN_LIMIT: usize = 4096;
const REASON_PRE_TRUNCATION_UTF16: usize = 500;

/// One Native UI site that owns one or more Client elements.
///
/// `request_id` is the parent `ui_render` instance id. It is deliberately not
/// the derived nested Client drawing id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientUiRenderSite {
    component: ClientUiComponent,
    request_id: String,
}

impl ClientUiRenderSite {
    /// Construct a parent render site using Native's component and string
    /// bounds.
    pub fn new(
        component: ClientUiComponent,
        request_id: impl Into<String>,
    ) -> Result<Self, ClientUiFaultRequestError> {
        let request_id = request_id.into();
        validate_utf16_len("instance_id", &request_id, MAX_ADDRESS_UTF16)?;
        Ok(Self {
            component,
            request_id,
        })
    }

    /// The parent UI component.
    pub fn component(&self) -> ClientUiComponent {
        self.component
    }

    /// The parent `ui_render` instance id.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

/// Component names accepted by the Native `ui_client_fault` control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientUiComponent {
    /// Ask-user-question UI.
    AskUserQuestion,
    /// User transcript row.
    UserMessage,
    /// Assistant transcript row.
    AssistantMessage,
    /// Tool-use row.
    ToolUse,
    /// Tool-result row.
    ToolResult,
    /// Grouped tool row.
    ToolGroup,
    /// Tool-progress row.
    ToolProgress,
    /// Command output row.
    CommandOutput,
    /// Spinner UI.
    Spinner,
    /// Turn-duration UI.
    TurnDuration,
    /// Informational notice UI.
    InfoNotice,
    /// Session-mode UI.
    SessionMode,
    /// Prompt-hint UI.
    PromptHint,
    /// Above-prompt UI.
    AbovePrompt,
    /// Pane UI.
    Pane,
}

impl ClientUiComponent {
    /// Return the exact Native component spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AskUserQuestion => "AskUserQuestion",
            Self::UserMessage => "UserMessage",
            Self::AssistantMessage => "AssistantMessage",
            Self::ToolUse => "ToolUse",
            Self::ToolResult => "ToolResult",
            Self::ToolGroup => "ToolGroup",
            Self::ToolProgress => "ToolProgress",
            Self::CommandOutput => "CommandOutput",
            Self::Spinner => "Spinner",
            Self::TurnDuration => "TurnDuration",
            Self::InfoNotice => "InfoNotice",
            Self::SessionMode => "SessionMode",
            Self::PromptHint => "PromptHint",
            Self::AbovePrompt => "AbovePrompt",
            Self::Pane => "Pane",
        }
    }

    /// Parse a Native render-site component name.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "AskUserQuestion" => Self::AskUserQuestion,
            "UserMessage" => Self::UserMessage,
            "AssistantMessage" => Self::AssistantMessage,
            "ToolUse" => Self::ToolUse,
            "ToolResult" => Self::ToolResult,
            "ToolGroup" => Self::ToolGroup,
            "ToolProgress" => Self::ToolProgress,
            "CommandOutput" => Self::CommandOutput,
            "Spinner" => Self::Spinner,
            "TurnDuration" => Self::TurnDuration,
            "InfoNotice" => Self::InfoNotice,
            "SessionMode" => Self::SessionMode,
            "PromptHint" => Self::PromptHint,
            "AbovePrompt" => Self::AbovePrompt,
            "Pane" => Self::Pane,
            _ => return None,
        })
    }
}

/// Native Client lifecycle phase that produced a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientUiFaultPhase {
    /// Client module mount or load failed.
    Load,
    /// Client tree creation or validation failed.
    Render,
    /// A held listener or timer callback failed.
    Run,
}

impl ClientUiFaultPhase {
    /// Return the exact Native phase spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Render => "render",
            Self::Run => "run",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "load" => Some(Self::Load),
            "render" => Some(Self::Render),
            "run" => Some(Self::Run),
            _ => None,
        }
    }
}

/// Exact identity of one currently drawn Client element.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientUiIdentity {
    plugin: String,
    module: String,
    key: Vec<u16>,
}

impl ClientUiIdentity {
    /// Construct an identity using Native's address string bounds.
    pub fn new(
        plugin: impl Into<String>,
        module: impl Into<String>,
        key: Vec<u16>,
    ) -> Result<Self, ClientUiFaultRequestError> {
        let plugin = plugin.into();
        let module = module.into();
        // Parent Client descriptors permit larger keys/modules than the
        // address fields of the internal `ui_client_fault` control request.
        validate_utf16_len("plugin", &plugin, 10_000)?;
        validate_utf16_len("module", &module, 10_000)?;
        validate_utf16_units_len("client", &key, 10_000)?;
        Ok(Self {
            plugin,
            module,
            key,
        })
    }

    /// The plugin that owns the Client.
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// The path of the plugin surface module.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The Client element key.
    pub fn key_units(&self) -> &[u16] {
        &self.key
    }
}

/// Exact composite address used by the Native drawn-Client registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientUiFaultAddress {
    site: ClientUiRenderSite,
    client: ClientUiIdentity,
}

impl ClientUiFaultAddress {
    /// Construct an address from a parent render site and Client identity.
    pub fn new(site: ClientUiRenderSite, client: ClientUiIdentity) -> Self {
        Self { site, client }
    }

    /// The parent render site.
    pub fn site(&self) -> &ClientUiRenderSite {
        &self.site
    }

    /// The Client identity.
    pub fn client(&self) -> &ClientUiIdentity {
        &self.client
    }
}

/// A parsed Native internal `ui_client_fault` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiClientFaultRequest {
    address: ClientUiFaultAddress,
    phase: ClientUiFaultPhase,
    reason: String,
}

impl UiClientFaultRequest {
    /// Parse the internal control request shape used by Native `ui_client_fault`.
    ///
    /// This parser intentionally does not accept an ordinary `ui.render`
    /// result. Invalid ordinary render trees keep their engine-row fallback
    /// behavior and do not become Client lifecycle faults.
    pub fn try_from_internal(
        value: &Utf16JsonProjection,
    ) -> Result<Self, ClientUiFaultRequestError> {
        value
            .validate()
            .map_err(|_| ClientUiFaultRequestError::NotObject)?;
        let object = value
            .value
            .as_object()
            .ok_or(ClientUiFaultRequestError::NotObject)?;
        let subtype = required_string(object, "subtype")?;
        if subtype != "ui_client_fault" {
            return Err(ClientUiFaultRequestError::WrongSubtype);
        }
        let plugin = required_string(object, "plugin")?.to_owned();
        let component = ClientUiComponent::parse(required_string(object, "component")?)
            .ok_or(ClientUiFaultRequestError::InvalidComponent)?;
        let request_id = required_string(object, "instance_id")?.to_owned();
        let client = value
            .string_units("/client")
            .ok_or(ClientUiFaultRequestError::MissingString("client"))?;
        let module = required_string(object, "module")?.to_owned();
        let phase = ClientUiFaultPhase::parse(required_string(object, "phase")?)
            .ok_or(ClientUiFaultRequestError::InvalidPhase)?;
        let reason = required_string(object, "reason")?.to_owned();
        validate_utf16_len("plugin", &plugin, MAX_ADDRESS_UTF16)?;
        validate_utf16_len("module", &module, MAX_ADDRESS_UTF16)?;
        validate_utf16_units_len("client", &client, MAX_ADDRESS_UTF16)?;
        let site = ClientUiRenderSite::new(component, request_id)?;
        let client = ClientUiIdentity::new(plugin, module, client)?;
        validate_utf16_len("reason", &reason, MAX_REASON_UTF16)?;
        Ok(Self {
            address: ClientUiFaultAddress::new(site, client),
            phase,
            reason,
        })
    }

    /// The exact drawn Client address.
    pub fn address(&self) -> &ClientUiFaultAddress {
        &self.address
    }

    /// Fault phase.
    pub fn phase(&self) -> ClientUiFaultPhase {
        self.phase
    }

    /// Raw reason accepted by the internal request schema.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Build the public plugin-hook event. Native maps the internal
    /// `instance_id` to the parent `requestId`, not the derived Client drawing
    /// id, and fixes the surface to `desktop`.
    pub fn hook_event(&self) -> UiFaultHookEvent {
        UiFaultHookEvent {
            surface: "desktop",
            component: self.address.site.component.as_str(),
            request_id: self.address.site.request_id.clone(),
            element: self.address.client.key.clone(),
            module: self.address.client.module.clone(),
            phase: self.phase,
            reason: normalize_client_fault_reason(
                &self.address.client.plugin,
                &self.address.client.module,
                &self.reason,
            ),
        }
    }
}

/// Snapshot of the Client's first drawn version and whether this is a repeat.
/// Repeated reports use the live site version after the hook settles, as
/// Native's lazy `bornAt` supplier does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientUiFaultMark {
    first_born_at: u64,
    repeated: bool,
}

impl ClientUiFaultMark {
    fn born_at_after_dispatch(self, current_version: u64) -> u64 {
        if self.repeated {
            current_version
        } else {
            self.first_born_at
        }
    }
}

/// The seven-field, pinned public `ui.fault` hook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiFaultHookEvent {
    surface: &'static str,
    component: &'static str,
    request_id: String,
    element: Vec<u16>,
    module: String,
    phase: ClientUiFaultPhase,
    reason: String,
}

impl UiFaultHookEvent {
    /// Return the exact hook argument object.
    pub fn to_projection(&self) -> Utf16JsonProjection {
        let mut projection = Utf16JsonProjection::plain(json!({
            "surface": self.surface,
            "component": self.component,
            "requestId": self.request_id,
            "element": String::from_utf16_lossy(&self.element),
            "module": self.module,
            "phase": self.phase.as_str(),
            "reason": self.reason,
        }));
        if String::from_utf16(&self.element).is_err() {
            projection.strings.push(Utf16JsonString {
                pointer: "/element".into(),
                code_units: self.element.clone(),
            });
        }
        projection
    }

    /// The parent render-site component.
    pub fn component(&self) -> &'static str {
        self.component
    }

    /// The parent render-site request id.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The Client key.
    pub fn element_units(&self) -> &[u16] {
        &self.element
    }

    /// The plugin surface-module path.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// Native lifecycle phase.
    pub fn phase(&self) -> ClientUiFaultPhase {
        self.phase
    }

    /// Normalized, UTF-16-limited reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A host-issued generation for one parent render operation.
///
/// The previous drawn identities remain current while a replacement render is
/// in flight. Only the latest generation can commit its drawn Client list.
#[derive(Debug)]
pub struct ClientUiRenderGeneration {
    site: ClientUiRenderSite,
    generation: u64,
    version: u64,
}

impl ClientUiRenderGeneration {
    /// Parent site associated with this render.
    pub fn site(&self) -> &ClientUiRenderSite {
        &self.site
    }

    /// Monotonic render generation used to reject stale completions.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Default)]
struct DrawnSite {
    generation: u64,
    drawn: HashMap<ClientUiIdentity, u64>,
    failed: HashMap<ClientUiIdentity, u64>,
}

#[derive(Default)]
struct RegistryState {
    next_generation: u64,
    drawn_sites: HashMap<ClientUiRenderSite, DrawnSite>,
    drawn_site_order: VecDeque<ClientUiRenderSite>,
    fault_runs: HashMap<ClientUiRenderSite, HashSet<u64>>,
    fault_run_order: VecDeque<ClientUiRenderSite>,
}

/// Session-owned registry of the Clients actually present in current UI trees.
///
/// Keep one instance with the session's `ModHost`; do not derive it from
/// `session.attach`/`session.detach`. The lock is always dropped before plugin
/// dispatch or renderer invalidation callbacks.
#[derive(Default)]
pub struct ClientUiFaultRegistry {
    inner: Mutex<RegistryState>,
}

impl ClientUiFaultRegistry {
    /// Start replacing a parent site's Client tree.
    ///
    /// Existing identities remain registered until [`Self::finish_render`]
    /// commits the replacement, matching Native's begin/record split.
    pub fn begin_render(
        &self,
        site: ClientUiRenderSite,
        site_version: u64,
    ) -> ClientUiRenderGeneration {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.next_generation = inner
            .next_generation
            .checked_add(1)
            .expect("Client UI render generation space exhausted");
        let generation = inner.next_generation;
        let previous = inner.drawn_sites.get(&site).cloned().unwrap_or_default();
        inner.drawn_sites.insert(
            site.clone(),
            DrawnSite {
                generation,
                drawn: previous.drawn,
                failed: previous.failed,
            },
        );
        touch_lru(&mut inner.drawn_site_order, &site);
        evict_drawn_sites(&mut inner);
        ClientUiRenderGeneration {
            site,
            generation,
            version: site_version,
        }
    }

    /// Commit the Clients present in a completed parent render.
    ///
    /// A stale render is ignored. An empty list marks the site undrawn. Each
    /// identity includes plugin, module, and key; matching only by Client key
    /// would let a replacement module inherit a stale fault address.
    pub fn finish_render<I>(&self, token: &ClientUiRenderGeneration, clients: I) -> bool
    where
        I: IntoIterator<Item = ClientUiIdentity>,
    {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(previous) = inner.drawn_sites.get(&token.site) else {
            return false;
        };
        if previous.generation != token.generation {
            return false;
        }
        let previous = previous.drawn.clone();
        let failed_before = inner
            .drawn_sites
            .get(&token.site)
            .map(|state| state.failed.clone())
            .unwrap_or_default();
        let mut drawn = HashMap::new();
        let mut failed = HashMap::new();
        for client in clients {
            let born_at = if failed_before.contains_key(&client) {
                token.version
            } else {
                previous.get(&client).copied().unwrap_or(token.version)
            };
            drawn.insert(client.clone(), born_at);
            if failed_before.get(&client) == Some(&token.version) {
                failed.insert(client, token.version);
            }
        }
        if drawn.is_empty() {
            inner.drawn_sites.remove(&token.site);
            inner.drawn_site_order.retain(|site| site != &token.site);
            return true;
        }
        inner.drawn_sites.insert(
            token.site.clone(),
            DrawnSite {
                generation: token.generation,
                drawn,
                failed,
            },
        );
        touch_lru(&mut inner.drawn_site_order, &token.site);
        evict_drawn_sites(&mut inner);
        true
    }

    /// Mark a site unmounted if the caller still owns its latest render
    /// generation. A delayed unmount from an older render cannot erase a newer
    /// drawn tree.
    pub fn unmount_site(&self, site: &ClientUiRenderSite, generation: u64) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner
            .drawn_sites
            .get(site)
            .is_none_or(|drawn| drawn.generation != generation)
        {
            return false;
        }
        inner.drawn_sites.remove(site);
        inner.drawn_site_order.retain(|candidate| candidate != site);
        true
    }

    /// Revoke the exact Client identities owned by a plugin after that plugin
    /// unloads. The parent site remains current for its other plugins.
    pub fn remove_plugin_clients(&self, plugin: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut empty_sites = Vec::new();
        for (site, state) in &mut inner.drawn_sites {
            state
                .drawn
                .retain(|identity, _| identity.plugin() != plugin);
            state
                .failed
                .retain(|identity, _| identity.plugin() != plugin);
            if state.drawn.is_empty() {
                empty_sites.push(site.clone());
            }
        }
        for site in empty_sites {
            inner.drawn_sites.remove(&site);
            inner.drawn_site_order.retain(|current| current != &site);
        }
    }

    /// Record a lifecycle failure only when the exact Client is currently
    /// drawn. The mark preserves the first born version and whether Native's
    /// repeated-report path should read the current site version after hooks.
    pub fn mark_fault(
        &self,
        address: &ClientUiFaultAddress,
        current_site_version: u64,
    ) -> Option<ClientUiFaultMark> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = inner.drawn_sites.get_mut(&address.site)?;
        let original_born_at = *state.drawn.get(&address.client)?;
        let repeated = state.failed.contains_key(&address.client);
        state
            .failed
            .insert(address.client.clone(), current_site_version);
        touch_lru(&mut inner.drawn_site_order, &address.site);
        Some(ClientUiFaultMark {
            first_born_at: original_born_at,
            repeated,
        })
    }

    /// Check the exact currently drawn identity after an async hook dispatch.
    pub fn is_drawn(&self, address: &ClientUiFaultAddress) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let drawn = inner
            .drawn_sites
            .get(&address.site)
            .is_some_and(|state| state.drawn.contains_key(&address.client));
        if drawn {
            touch_lru(&mut inner.drawn_site_order, &address.site);
        }
        drawn
    }

    fn should_invalidate(
        &self,
        plugin: &str,
        site: &ClientUiRenderSite,
        born_at: u64,
        current_version: u64,
    ) -> bool {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = plugin;
        let duplicate = inner.fault_runs.get(site).is_some_and(|versions| {
            versions.contains(&born_at) && versions.contains(&current_version)
        });
        if inner.fault_runs.contains_key(site) {
            touch_lru(&mut inner.fault_run_order, site);
        }
        !duplicate
    }

    fn record_invalidation(
        &self,
        plugin: &str,
        site: &ClientUiRenderSite,
        version_before: u64,
        version_after: u64,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = plugin;
        let mut versions = inner
            .fault_runs
            .get(site)
            .filter(|versions| versions.contains(&version_before))
            .cloned()
            .unwrap_or_default();
        versions.insert(version_after);
        inner.fault_runs.insert(site.clone(), versions);
        touch_lru(&mut inner.fault_run_order, site);
        evict_fault_runs(&mut inner);
    }
}

/// Host effects needed to connect fault projection to one real renderer and
/// one session ModHost.
#[async_trait::async_trait]
pub trait ClientUiFaultHost: Send + Sync {
    /// Error returned by the plugin hook dispatch.
    type Error: Send + Sync + 'static;

    /// Dispatch this event to the named plugin's `ui.fault` chain plus the
    /// Native core handler. Plugin scope comes from the internal request and
    /// is deliberately not included in the public event object.
    async fn dispatch_plugin_ui_fault(
        &self,
        plugin: &str,
        event: &UiFaultHookEvent,
    ) -> Result<(), Self::Error>;

    /// Check whether the plugin has a matching `ui.fault` listener after the
    /// chain settles. Native treats a matcher exception as a match.
    async fn has_matching_ui_fault_listener(&self, plugin: &str, event: &UiFaultHookEvent) -> bool;

    /// Read the parent UI render site's current version.
    fn current_ui_render_version(&self, site: &ClientUiRenderSite) -> u64;

    /// Invalidate exactly this parent render site. This must not advance a
    /// global/full-tree render generation.
    async fn invalidate_ui_render_site(&self, site: &ClientUiRenderSite);
}

/// Outcome of processing one internal Client lifecycle fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientUiFaultOutcome {
    /// The exact Client was no longer drawn before the producer ran, so Native
    /// drops the report without dispatching it.
    NotDrawn,
    /// The fault was dispatched. Invalidation is gated by the post-dispatch
    /// listener, drawn-instance, and duplicate-version checks.
    Dispatched { invalidated: bool },
}

/// Dispatch a fault for a currently drawn Client and perform Native's
/// post-dispatch invalidation checks.
///
/// No registry lock is held while awaiting hooks. If dispatch fails, the
/// `finally`-equivalent invalidation path still runs before the dispatch error
/// is returned. Hook matcher errors should be converted to `true` by the host
/// adapter, as in Native's `iO` matcher.
pub async fn dispatch_client_ui_fault<H: ClientUiFaultHost>(
    registry: &ClientUiFaultRegistry,
    request: &UiClientFaultRequest,
    host: &H,
) -> Result<ClientUiFaultOutcome, H::Error> {
    let address = &request.address;
    let site = &address.site;
    let plugin = &address.client.plugin;
    let event = request.hook_event();
    if !registry.is_drawn(address) {
        return Ok(ClientUiFaultOutcome::NotDrawn);
    }
    let version_at_report = host.current_ui_render_version(site);
    let Some(fault_mark) = registry.mark_fault(address, version_at_report) else {
        return Ok(ClientUiFaultOutcome::NotDrawn);
    };

    let dispatch_result = host.dispatch_plugin_ui_fault(plugin, &event).await;
    let mut invalidated = false;
    if !site.request_id.is_empty()
        && host.has_matching_ui_fault_listener(plugin, &event).await
        && registry.is_drawn(address)
    {
        let version_before = host.current_ui_render_version(site);
        let born_at = fault_mark.born_at_after_dispatch(version_before);
        if registry.should_invalidate(plugin, site, born_at, version_before) {
            host.invalidate_ui_render_site(site).await;
            let version_after = host.current_ui_render_version(site);
            registry.record_invalidation(plugin, site, version_before, version_after);
            invalidated = true;
        }
    }

    dispatch_result?;
    Ok(ClientUiFaultOutcome::Dispatched { invalidated })
}

/// Normalize a Client fault reason like Native's `mO` projection.
///
/// The caller passes the lifecycle error text after Native's Error/rejection
/// wrapper has selected its text. This function scrubs control/sentinel
/// characters, removes one plugin and one Client-module prefix, substitutes
/// the empty-reason fallback, replaces Unicode line separators, and applies
/// Native's UTF-16 truncation bounds.
pub fn normalize_client_fault_reason(plugin: &str, module: &str, reason: &str) -> String {
    let scrubbed: String = reason
        .chars()
        .map(|character| {
            if character.is_control() || character == '\u{10eeee}' {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut normalized = if scrubbed.encode_utf16().count() > REASON_PRE_TRUNCATION_UTF16 {
        slice_utf16(&scrubbed, REASON_PRE_TRUNCATION_UTF16) + "…"
    } else {
        scrubbed
    };
    if let Some(without_plugin) = normalized.strip_prefix(&format!("{plugin}: ")) {
        normalized = without_plugin.to_owned();
    }
    let module_prefix = format!("Client {module}: ");
    if let Some(without_module) = normalized.strip_prefix(&module_prefix) {
        normalized = without_module.to_owned();
    }
    if normalized.trim().is_empty() {
        return "the module failed without a message".to_owned();
    }
    normalized = normalized.replace(['\u{2028}', '\u{2029}'], " ");
    if normalized.encode_utf16().count() > MAX_REASON_UTF16 {
        slice_utf16(&normalized, MAX_REASON_UTF16 - 1) + "…"
    } else {
        normalized
    }
}

fn slice_utf16(value: &str, limit: usize) -> String {
    let mut output = String::new();
    let mut units = 0;
    for character in value.chars() {
        let width = character.len_utf16();
        if units + width > limit {
            break;
        }
        output.push(character);
        units += width;
    }
    output
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, ClientUiFaultRequestError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or(ClientUiFaultRequestError::MissingString(name))
}

fn validate_utf16_units_len(
    field: &'static str,
    value: &[u16],
    maximum: usize,
) -> Result<(), ClientUiFaultRequestError> {
    if value.len() > maximum {
        return Err(ClientUiFaultRequestError::TooLong(field));
    }
    Ok(())
}

fn validate_utf16_len(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ClientUiFaultRequestError> {
    if value.encode_utf16().count() > max {
        return Err(ClientUiFaultRequestError::TooLong(field));
    }
    Ok(())
}

fn touch_lru<T: Eq + Hash + Clone>(order: &mut VecDeque<T>, value: &T) {
    order.retain(|existing| existing != value);
    order.push_back(value.clone());
}

fn evict_drawn_sites(inner: &mut RegistryState) {
    while inner.drawn_sites.len() > DRAWN_SITE_LIMIT {
        let Some(oldest) = inner.drawn_site_order.pop_front() else {
            break;
        };
        inner.drawn_sites.remove(&oldest);
    }
}

fn evict_fault_runs(inner: &mut RegistryState) {
    while inner.fault_runs.len() > FAULT_RUN_LIMIT {
        let Some(oldest) = inner.fault_run_order.pop_front() else {
            break;
        };
        inner.fault_runs.remove(&oldest);
        if let Some(drawn) = inner.drawn_sites.get_mut(&oldest) {
            drawn.failed.clear();
        }
    }
}

/// Parse/validation failures for the Native internal control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClientUiFaultRequestError {
    /// The control request was not a JSON object.
    #[error("ui_client_fault request must be an object")]
    NotObject,
    /// Required control fields must be strings.
    #[error("ui_client_fault request needs string field {0}")]
    MissingString(&'static str),
    /// Another internal control request was supplied.
    #[error("request subtype is not ui_client_fault")]
    WrongSubtype,
    /// Component is not one of Native's 15 UI render components.
    #[error("ui_client_fault component is unsupported")]
    InvalidComponent,
    /// Phase is not load, render, or run.
    #[error("ui_client_fault phase is unsupported")]
    InvalidPhase,
    /// A string exceeded its Native UTF-16 maximum.
    #[error("ui_client_fault field {0} exceeds its UTF-16 maximum")]
    TooLong(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn site(request_id: &str) -> ClientUiRenderSite {
        ClientUiRenderSite::new(ClientUiComponent::AbovePrompt, request_id).unwrap()
    }

    fn identity(plugin: &str, module: &str, key: &str) -> ClientUiIdentity {
        ClientUiIdentity::new(plugin, module, key.encode_utf16().collect()).unwrap()
    }

    fn fault_request(
        plugin: &str,
        request_id: &str,
        module: &str,
        client: &str,
        phase: &str,
        reason: &str,
    ) -> UiClientFaultRequest {
        UiClientFaultRequest::try_from_internal(&Utf16JsonProjection::plain(json!({
            "subtype":"ui_client_fault",
            "plugin":plugin,
            "component":"AbovePrompt",
            "instance_id":request_id,
            "client":client,
            "module":module,
            "phase":phase,
            "reason":reason,
        })))
        .unwrap()
    }

    struct FakeHost {
        registry: Arc<ClientUiFaultRegistry>,
        versions: Mutex<HashMap<ClientUiRenderSite, u64>>,
        listener: AtomicBool,
        fail_dispatch: AtomicBool,
        dispatches: AtomicUsize,
        events: Mutex<Vec<(String, UiFaultHookEvent)>>,
        invalidations: Mutex<Vec<ClientUiRenderSite>>,
        unmount_on_dispatch: Mutex<Option<(ClientUiRenderSite, u64)>>,
    }

    impl FakeHost {
        fn new(registry: Arc<ClientUiFaultRegistry>) -> Self {
            Self {
                registry,
                versions: Mutex::new(HashMap::new()),
                listener: AtomicBool::new(true),
                fail_dispatch: AtomicBool::new(false),
                dispatches: AtomicUsize::new(0),
                events: Mutex::new(Vec::new()),
                invalidations: Mutex::new(Vec::new()),
                unmount_on_dispatch: Mutex::new(None),
            }
        }

        fn set_version(&self, site: ClientUiRenderSite, version: u64) {
            self.versions.lock().unwrap().insert(site, version);
        }
    }

    #[async_trait::async_trait]
    impl ClientUiFaultHost for FakeHost {
        type Error = String;

        async fn dispatch_plugin_ui_fault(
            &self,
            plugin: &str,
            event: &UiFaultHookEvent,
        ) -> Result<(), Self::Error> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap()
                .push((plugin.to_owned(), event.clone()));
            if let Some((site, generation)) = self.unmount_on_dispatch.lock().unwrap().take() {
                assert!(self.registry.unmount_site(&site, generation));
            }
            if self.fail_dispatch.load(Ordering::SeqCst) {
                Err("hook dispatch failed".to_owned())
            } else {
                Ok(())
            }
        }

        async fn has_matching_ui_fault_listener(
            &self,
            _plugin: &str,
            _event: &UiFaultHookEvent,
        ) -> bool {
            self.listener.load(Ordering::SeqCst)
        }

        fn current_ui_render_version(&self, site: &ClientUiRenderSite) -> u64 {
            *self.versions.lock().unwrap().get(site).unwrap_or(&0)
        }

        async fn invalidate_ui_render_site(&self, site: &ClientUiRenderSite) {
            self.invalidations.lock().unwrap().push(site.clone());
            let mut versions = self.versions.lock().unwrap();
            *versions.entry(site.clone()).or_default() += 1;
        }
    }

    fn draw(
        registry: &ClientUiFaultRegistry,
        site: ClientUiRenderSite,
        version: u64,
        clients: impl IntoIterator<Item = ClientUiIdentity>,
    ) -> u64 {
        let token = registry.begin_render(site, version);
        let generation = token.generation();
        assert!(registry.finish_render(&token, clients));
        generation
    }

    #[test]
    fn parser_maps_native_internal_request_to_the_pinned_hook_envelope() {
        let request = fault_request(
            "sample-plugin",
            "parent-instance",
            "ui/panel.tsx",
            "client-key",
            "render",
            "sample-plugin: Client ui/panel.tsx: broken tree",
        );
        assert_eq!(request.address().site().request_id(), "parent-instance");
        assert_eq!(request.phase(), ClientUiFaultPhase::Render);
        assert_eq!(
            request.hook_event().to_projection().value,
            json!({
                "surface":"desktop",
                "component":"AbovePrompt",
                "requestId":"parent-instance",
                "element":"client-key",
                "module":"ui/panel.tsx",
                "phase":"render",
                "reason":"broken tree",
            })
        );
    }

    #[test]
    fn ordinary_ui_render_failure_is_not_parsed_as_a_client_fault() {
        let ordinary_render_failure = json!({
            "surface":"desktop",
            "component":"AbovePrompt",
            "requestId":"parent-instance",
            "result":{"type":"invalid-tree"},
        });
        assert_eq!(
            UiClientFaultRequest::try_from_internal(&Utf16JsonProjection::plain(
                ordinary_render_failure
            )),
            Err(ClientUiFaultRequestError::MissingString("subtype"))
        );
    }

    #[test]
    fn generation_rejects_stale_render_and_empty_commit_marks_undrawn() {
        let registry = ClientUiFaultRegistry::default();
        let site = site("parent-instance");
        let stale = registry.begin_render(site.clone(), 1);
        let current = registry.begin_render(site.clone(), 1);
        assert!(!registry.finish_render(&stale, [identity("plug", "ui.ts", "key")]));
        assert!(registry.finish_render(&current, [identity("plug", "ui.ts", "key")]));

        let next = registry.begin_render(site.clone(), 2);
        assert!(registry.finish_render(&next, std::iter::empty()));
        let address = ClientUiFaultAddress::new(site, identity("plug", "ui.ts", "key"));
        assert!(!registry.is_drawn(&address));
        assert_eq!(registry.mark_fault(&address, 2), None);
    }

    #[tokio::test]
    async fn drawn_client_dispatches_then_invalidates_only_its_parent_site() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("parent-instance");
        host.set_version(site.clone(), 1);
        draw(
            &registry,
            site.clone(),
            1,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        let request = fault_request(
            "sample-plugin",
            "parent-instance",
            "ui/panel.tsx",
            "client-key",
            "load",
            "module failed",
        );

        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: true }
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(*host.invalidations.lock().unwrap(), [site]);
    }

    #[tokio::test]
    async fn no_drawn_client_drops_before_plugin_dispatch() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let request = fault_request(
            "sample-plugin",
            "not-drawn",
            "ui/panel.tsx",
            "client-key",
            "run",
            "callback failed",
        );
        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::NotDrawn
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn draw_admission_matches_component_parent_id_plugin_module_and_key() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("parent-instance");
        host.set_version(site.clone(), 3);
        draw(
            &registry,
            site,
            3,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        let requests = [
            fault_request(
                "other-plugin",
                "parent-instance",
                "ui/panel.tsx",
                "client-key",
                "render",
                "wrong plugin",
            ),
            fault_request(
                "sample-plugin",
                "parent-instance",
                "ui/other.tsx",
                "client-key",
                "render",
                "wrong module",
            ),
            fault_request(
                "sample-plugin",
                "parent-instance",
                "ui/panel.tsx",
                "other-key",
                "render",
                "wrong key",
            ),
            fault_request(
                "sample-plugin",
                "other-parent",
                "ui/panel.tsx",
                "client-key",
                "render",
                "wrong request id",
            ),
            UiClientFaultRequest::try_from_internal(&Utf16JsonProjection::plain(json!({
                "subtype":"ui_client_fault",
                "plugin":"sample-plugin",
                "component":"Pane",
                "instance_id":"parent-instance",
                "client":"client-key",
                "module":"ui/panel.tsx",
                "phase":"render",
                "reason":"wrong component",
            })))
            .unwrap(),
        ];

        for request in requests {
            assert_eq!(
                dispatch_client_ui_fault(&registry, &request, &host)
                    .await
                    .unwrap(),
                ClientUiFaultOutcome::NotDrawn
            );
        }
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unmount_during_await_keeps_dispatch_but_skips_invalidation() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("parent-instance");
        host.set_version(site.clone(), 4);
        let generation = draw(
            &registry,
            site.clone(),
            4,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        *host.unmount_on_dispatch.lock().unwrap() = Some((site, generation));
        let request = fault_request(
            "sample-plugin",
            "parent-instance",
            "ui/panel.tsx",
            "client-key",
            "run",
            "listener failed",
        );

        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: false }
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert!(host.invalidations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_parent_request_id_dispatches_without_site_invalidation() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("");
        host.set_version(site.clone(), 5);
        draw(
            &registry,
            site,
            5,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        let request = fault_request(
            "sample-plugin",
            "",
            "ui/panel.tsx",
            "client-key",
            "render",
            "invalid Client tree",
        );

        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: false }
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert!(host.invalidations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_error_still_runs_finally_invalidation() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("parent-instance");
        host.set_version(site.clone(), 7);
        draw(
            &registry,
            site.clone(),
            7,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        host.fail_dispatch.store(true, Ordering::SeqCst);
        let request = fault_request(
            "sample-plugin",
            "parent-instance",
            "ui/panel.tsx",
            "client-key",
            "render",
            "invalid Client tree",
        );

        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host).await,
            Err("hook dispatch failed".to_owned())
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(*host.invalidations.lock().unwrap(), [site]);
    }

    #[tokio::test]
    async fn absent_listener_still_dispatches_but_does_not_invalidate() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("parent-instance");
        host.set_version(site.clone(), 2);
        draw(
            &registry,
            site,
            2,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        host.listener.store(false, Ordering::SeqCst);
        let request = fault_request(
            "sample-plugin",
            "parent-instance",
            "ui/panel.tsx",
            "client-key",
            "render",
            "invalid Client tree",
        );

        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: false }
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert!(host.invalidations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_suppression_requires_both_native_versions_and_is_site_scoped() {
        let registry = Arc::new(ClientUiFaultRegistry::default());
        let host = FakeHost::new(registry.clone());
        let site = site("parent-instance");
        host.set_version(site.clone(), 10);
        draw(
            &registry,
            site.clone(),
            10,
            [identity("sample-plugin", "ui/panel.tsx", "client-key")],
        );
        let request = fault_request(
            "sample-plugin",
            "parent-instance",
            "ui/panel.tsx",
            "client-key",
            "render",
            "invalid Client tree",
        );

        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: true }
        );
        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: false }
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 2);
        assert_eq!(host.invalidations.lock().unwrap().len(), 1);

        host.set_version(site.clone(), 12);
        assert_eq!(
            dispatch_client_ui_fault(&registry, &request, &host)
                .await
                .unwrap(),
            ClientUiFaultOutcome::Dispatched { invalidated: true }
        );
        assert_eq!(host.invalidations.lock().unwrap().len(), 2);
    }

    #[test]
    fn reason_normalization_uses_native_prefix_scrub_fallback_and_utf16_cap() {
        assert_eq!(
            normalize_client_fault_reason("plug", "ui.ts", "plug: Client ui.ts:  bad\nreason"),
            " bad reason"
        );
        assert_eq!(
            normalize_client_fault_reason("plug", "ui.ts", "\u{2028}"),
            "the module failed without a message"
        );
        let long = normalize_client_fault_reason("plug", "ui.ts", &"😀".repeat(110));
        assert!(long.encode_utf16().count() <= MAX_REASON_UTF16);
        assert!(long.ends_with('\u{2026}'));
    }

    #[test]
    fn internal_schema_uses_utf16_limits_and_requires_known_phase() {
        let too_long_client = json!({
            "subtype":"ui_client_fault",
            "plugin":"plug",
            "component":"AbovePrompt",
            "instance_id":"parent",
            "client":"😀".repeat(129),
            "module":"ui.ts",
            "phase":"render",
            "reason":"bad",
        });
        assert_eq!(
            UiClientFaultRequest::try_from_internal(&Utf16JsonProjection::plain(too_long_client)),
            Err(ClientUiFaultRequestError::TooLong("client"))
        );
        let invalid_phase = json!({
            "subtype":"ui_client_fault",
            "plugin":"plug",
            "component":"AbovePrompt",
            "instance_id":"parent",
            "client":"key",
            "module":"ui.ts",
            "phase":"mount",
            "reason":"bad",
        });
        assert_eq!(
            UiClientFaultRequest::try_from_internal(&Utf16JsonProjection::plain(invalid_phase)),
            Err(ClientUiFaultRequestError::InvalidPhase)
        );
    }
}
