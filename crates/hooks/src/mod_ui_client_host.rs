//! Session-owned Host execution for Native Mods Client UI controls.
//!
//! Module source is prepared from the loaded plugin and kept on the host. The
//! renderer can request a manifest or a drawing operation, but it cannot
//! supply executable source or choose Client props outside the latest
//! validated parent `ui_render` tree.

use super::client_source::PreparedClientModules;
use super::{
    core_projection_from_mod, ModError, ModHost, ModSessionContext, ModUiInvalidationUpdate,
    ModUiRenderPace, ModUtf16DispatchScope,
};
use crate::mod_ui_fault::{
    dispatch_client_ui_fault, ClientUiComponent, ClientUiFaultHost, ClientUiFaultRegistry,
    ClientUiIdentity, ClientUiRenderGeneration, ClientUiRenderSite, UiClientFaultRequest,
    UiFaultHookEvent,
};
use lingxi_core::host::orchestrator::ModUiControlOutcome;
use lingxi_core::types::utf16_json::{Utf16JsonProjection, Utf16JsonString};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, PoisonError};

const MAX_CONTROL_UTF16: usize = 256;
const MAX_PARENT_CLIENT_UTF16: usize = 10_000;
const MAX_CLIENT_MESSAGE_UTF16: usize = 100_000;
const MAX_CLIENT_MESSAGE_VALUES: usize = 2_000;
const MAX_CLIENT_MESSAGE_DEPTH: usize = 32;
const MAX_PRESS_VALUE_UTF16: usize = 16_384;
const MAX_JS_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
pub(super) struct ClientUiHostState {
    data: StdMutex<ClientUiHostData>,
    fault_registry: ClientUiFaultRegistry,
    next_revision: AtomicU64,
    next_client_environment_epoch: AtomicU64,
    next_runtime_id: AtomicU64,
    next_invalidation_id: AtomicU64,
}

#[derive(Default)]
struct ClientUiHostData {
    modules: HashMap<String, PreparedClientModules>,
    client_runtime_epochs: HashMap<String, u64>,
    plugin_by_storage: HashMap<String, String>,
    parents: HashMap<ClientUiRenderSite, ParentRender>,
    latest_started: HashMap<ClientUiRenderSite, u64>,
    parent_control_latest: HashMap<ParentControlSite, u64>,
    parent_control_renders: HashMap<ParentControlSite, ParentControlRender>,
    pending_tokens: HashMap<(ClientUiRenderSite, u64), ClientUiRenderGeneration>,
    instances: HashMap<String, ClientUiInstance>,
    instance_by_identity: HashMap<ClientUiInstanceKey, String>,
}

struct ParentRender {
    revision: u64,
    token: Option<ClientUiRenderGeneration>,
    committed_generation: Option<u64>,
    clients: HashMap<ClientUiIdentity, Utf16JsonProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ParentControlSite {
    surface: String,
    component: ClientUiComponent,
    request_id: String,
}

struct ParentControlRender {
    revision: u64,
    press_actions: Vec<ParentUiPressAction>,
}

#[derive(Clone)]
struct ParentUiPressAction {
    site: ParentControlSite,
    surface: String,
    plugin: String,
    handle: u64,
    element: Option<String>,
    kind: String,
    pressable_links: Option<Vec<String>>,
    select_values: HashSet<String>,
}

enum ClientWorkerOperationError {
    Fault { phase: String, reason: String },
    Host(ModError),
}

impl From<ModError> for ClientWorkerOperationError {
    fn from(error: ModError) -> Self {
        Self::Host(error)
    }
}

impl From<ClientWorkerOperationError> for ModError {
    fn from(error: ClientWorkerOperationError) -> Self {
        match error {
            ClientWorkerOperationError::Fault { phase, reason } => {
                ModError::Hook(format!("Client surface {phase} failed: {reason}"))
            }
            ClientWorkerOperationError::Host(error) => error,
        }
    }
}

fn worker_fault_response(
    runtime_id: Option<&str>,
    revision: u64,
    plugin: &str,
    module: &str,
    phase: &str,
    reason: &str,
) -> Value {
    let mut response = json!({"handled":false,"renderRevision":revision});
    if let Some(runtime_id) = runtime_id {
        response["runtimeId"] = Value::String(runtime_id.to_owned());
    }
    response["fault"] = json!({
        "phase":phase,
        "reason":crate::mod_ui_fault::normalize_client_fault_reason(plugin, module, reason),
        "source":"worker",
    });
    response
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ClientUiInstanceKey {
    site: ClientUiRenderSite,
    identity: ClientUiIdentity,
}

#[derive(Clone)]
struct ClientUiInstance {
    key: ClientUiInstanceKey,
    runtime_id: String,
    revision: u64,
    worker_generation: Option<u64>,
    pending_events: Vec<Value>,
    draining_pending_events: bool,
    props: Utf16JsonProjection,
    mounted: bool,
    worker_fault: Option<ClientUiWorkerFault>,
    worker_fault_snapshot_sent: bool,
    worker_fault_event_dispatched: bool,
}

#[derive(Clone)]
struct ClientUiWorkerFault {
    phase: String,
    reason: String,
}

fn remove_instance_if_current(
    data: &mut ClientUiHostData,
    runtime_id: &str,
    key: &ClientUiInstanceKey,
) {
    data.instances.remove(runtime_id);
    if data.instance_by_identity.get(key).map(String::as_str) == Some(runtime_id) {
        data.instance_by_identity.remove(key);
    }
}

impl ClientUiHostState {
    pub(super) fn install_loaded_plugin(
        &self,
        plugin: &str,
        storage_id: &str,
        prepared: Option<PreparedClientModules>,
    ) {
        let mut data = lock(&self.data);
        data.plugin_by_storage
            .insert(storage_id.to_owned(), plugin.to_owned());
        if let Some(prepared) = prepared {
            data.modules.insert(plugin.to_owned(), prepared);
            let epoch = self
                .next_client_environment_epoch
                .fetch_add(1, Ordering::AcqRel)
                + 1;
            assert!(
                epoch > 0 && epoch <= MAX_JS_SAFE_INTEGER,
                "Client environment epoch space exhausted"
            );
            data.client_runtime_epochs.insert(plugin.to_owned(), epoch);
        } else {
            data.modules.remove(plugin);
            data.client_runtime_epochs.remove(plugin);
        }
    }

    pub(super) fn plugin_for_storage(&self, storage_id: &str) -> Option<String> {
        lock(&self.data).plugin_by_storage.get(storage_id).cloned()
    }

    pub(super) fn unload_storage(&self, storage_id: &str) -> Vec<ClientUiRenderSite> {
        let (removed_plugin, affected_sites) = {
            let mut data = lock(&self.data);
            if let Some(plugin) = data.plugin_by_storage.remove(storage_id) {
                let still_loaded = data
                    .plugin_by_storage
                    .values()
                    .any(|loaded| loaded == &plugin);
                if !still_loaded {
                    data.modules.remove(&plugin);
                    data.client_runtime_epochs.remove(&plugin);
                    data.instances
                        .retain(|_, instance| instance.key.identity.plugin() != plugin);
                    data.instance_by_identity
                        .retain(|key, _| key.identity.plugin() != plugin);
                    let mut affected_sites = HashSet::new();
                    for (site, parent) in data.parents.iter_mut() {
                        if parent
                            .clients
                            .keys()
                            .any(|identity| identity.plugin() == plugin)
                        {
                            affected_sites.insert(site.clone());
                        }
                        parent
                            .clients
                            .retain(|identity, _| identity.plugin() != plugin);
                    }
                    for (site, parent) in data.parent_control_renders.iter_mut() {
                        if site.surface == "desktop"
                            && parent
                                .press_actions
                                .iter()
                                .any(|action| action.plugin == plugin)
                        {
                            if let Ok(site) =
                                ClientUiRenderSite::new(site.component, site.request_id.clone())
                            {
                                affected_sites.insert(site);
                            }
                        }
                        parent
                            .press_actions
                            .retain(|action| action.plugin != plugin);
                    }
                    (Some(plugin), affected_sites.into_iter().collect())
                } else {
                    // Unloading any storage disposes the worker's plugin
                    // environment. If another storage still owns the plugin,
                    // the prepared modules remain available for the next
                    // mount but that runtime needs a fresh local epoch.
                    let epoch = self
                        .next_client_environment_epoch
                        .fetch_add(1, Ordering::AcqRel)
                        + 1;
                    assert!(
                        epoch > 0 && epoch <= MAX_JS_SAFE_INTEGER,
                        "Client environment epoch space exhausted"
                    );
                    data.client_runtime_epochs.insert(plugin.clone(), epoch);
                    (None, Vec::new())
                }
            } else {
                (None, Vec::new())
            }
        };
        if let Some(plugin) = removed_plugin {
            self.fault_registry.remove_plugin_clients(&plugin);
        }
        affected_sites
    }

    fn next_revision(&self) -> Result<u64, ModError> {
        let revision = self.next_revision.fetch_add(1, Ordering::AcqRel) + 1;
        if revision == 0 || revision > MAX_JS_SAFE_INTEGER {
            return Err(ModError::Unavailable(
                "Mod UI render revision space exhausted".into(),
            ));
        }
        Ok(revision)
    }

    fn next_runtime_id(&self, host_id: u64) -> String {
        let counter = self.next_runtime_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("mod-client-{host_id}-{counter}")
    }
}

/// Validate a canonical Native Mods Client UI request without requiring a
/// live session. Core hosts use this before projecting a no-Mod fallback.
pub(super) fn normalize_control_request(
    request: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ModError> {
    request
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let object = request
        .value
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI control request must be an object".into()))?;
    let subtype = string_field(object, "subtype")?;
    let normalized = match subtype {
        "ui_render" => {
            let mut result = pick_fields(
                object,
                &[
                    "subtype",
                    "surface",
                    "component",
                    "instance_id",
                    "props",
                    "client_id",
                    "content_rows",
                ],
            );
            if let Some(value) = object.get("viewport") {
                result.insert(
                    "viewport".into(),
                    normalize_object_fields(value, &["columns", "rows", "isFullscreen"])?,
                );
            }
            if let Some(value) = object.get("on_screen") {
                result.insert(
                    "on_screen".into(),
                    if value.is_null() {
                        Value::Null
                    } else {
                        normalize_object_fields(value, &["first", "last", "of"])?
                    },
                );
            }
            if let Some(Value::Array(rows)) = object.get("keyed") {
                let rows = rows
                    .iter()
                    .map(|row| normalize_object_fields(row, &["plugin", "key", "top", "bottom"]))
                    .collect::<Result<Vec<_>, _>>()?;
                result.insert("keyed".into(), Value::Array(rows));
            } else if let Some(value) = object.get("keyed") {
                result.insert("keyed".into(), value.clone());
            }
            if let Some(value) = object.get("bench") {
                result.insert(
                    "bench".into(),
                    normalize_object_fields(value, &["seq", "t0"])?,
                );
            }
            Value::Object(result)
        }
        "ui_client_module" => Value::Object(pick_fields(object, &["subtype", "plugin"])),
        "ui_message" => {
            let mut result = pick_fields(
                object,
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                ],
            );
            if let Some(data) = object.get("data") {
                result.insert("data".into(), data.clone());
            }
            Value::Object(result)
        }
        "ui_client_fault" => Value::Object(pick_fields(
            object,
            &[
                "subtype",
                "plugin",
                "component",
                "instance_id",
                "client",
                "module",
                "phase",
                "reason",
            ],
        )),
        "ui_client_press" => {
            let mut result = pick_fields(
                object,
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "element",
                ],
            );
            let event = object
                .get("event")
                .and_then(Value::as_object)
                .ok_or_else(|| ModError::Hook("ui_client_press event must be an object".into()))?;
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| ModError::Hook("ui_client_press event needs a type".into()))?;
            let canonical_event = match event_type {
                "press" => Value::Object(pick_fields(event, &["type"])),
                "input" => Value::Object(pick_fields(event, &["type", "kind", "value"])),
                "select" => Value::Object(pick_fields(event, &["type", "value"])),
                _ => {
                    return Err(ModError::Hook(
                        "ui_client_press event type is invalid".into(),
                    ));
                }
            };
            result.insert("event".into(), canonical_event);
            Value::Object(result)
        }
        "ui_press" => {
            let mut result = pick_fields(
                object,
                &[
                    "subtype",
                    "plugin",
                    "handle",
                    "key",
                    "surface",
                    "href",
                    "client_id",
                ],
            );
            result
                .entry("surface")
                .or_insert_with(|| Value::String("desktop".into()));
            Value::Object(result)
        }
        "ui_input" => {
            let mut result = pick_fields(
                object,
                &[
                    "subtype",
                    "plugin",
                    "handle",
                    "kind",
                    "value",
                    "key",
                    "component",
                    "instance_id",
                    "surface",
                    "client_id",
                ],
            );
            result
                .entry("surface")
                .or_insert_with(|| Value::String("desktop".into()));
            Value::Object(result)
        }
        "ui_select" => {
            let mut result = pick_fields(
                object,
                &[
                    "subtype",
                    "plugin",
                    "handle",
                    "value",
                    "key",
                    "component",
                    "instance_id",
                    "surface",
                    "client_id",
                ],
            );
            result
                .entry("surface")
                .or_insert_with(|| Value::String("desktop".into()));
            Value::Object(result)
        }
        _ => return Err(ModError::Hook("unknown Mod UI control subtype".into())),
    };
    let normalized = retain_matching_projection(request, normalized)?;
    validate_control_request(&normalized)?;
    Ok(normalized)
}

fn retain_matching_projection(
    source: &Utf16JsonProjection,
    value: Value,
) -> Result<Utf16JsonProjection, ModError> {
    let mut projection = Utf16JsonProjection::plain(value);
    projection.strings = source
        .strings
        .iter()
        .filter(|sidecar| {
            projection
                .value
                .pointer(&sidecar.pointer)
                .and_then(Value::as_str)
                .is_some_and(|text| text == String::from_utf16_lossy(&sidecar.code_units))
        })
        .cloned()
        .collect();
    projection.keys = source
        .keys
        .iter()
        .filter(|sidecar| {
            projection
                .value
                .pointer(&sidecar.pointer)
                .and_then(Value::as_object)
                .is_some_and(|object| object.contains_key(&sidecar.placeholder))
        })
        .cloned()
        .collect();
    projection
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    Ok(projection)
}

fn pick_fields(object: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|field| {
            object
                .get(*field)
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect()
}

fn normalize_object_fields(value: &Value, fields: &[&str]) -> Result<Value, ModError> {
    let object = value
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI nested request value must be an object".into()))?;
    Ok(Value::Object(pick_fields(object, fields)))
}

pub(super) fn validate_control_request(request: &Utf16JsonProjection) -> Result<(), ModError> {
    request
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let object = request
        .value
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI control request must be an object".into()))?;
    let subtype = string_field(object, "subtype")?;
    match subtype {
        "ui_render" => validate_ui_render(request),
        "ui_client_module" => {
            exact_keys(object, &["subtype", "plugin"])?;
            bounded_string(object, "plugin", MAX_CONTROL_UTF16)?;
            Ok(())
        }
        "ui_message" => {
            exact_keys(
                object,
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "data",
                ],
            )?;
            validate_client_address(request)?;
            validate_plain_json_projection(
                request,
                "/data",
                MAX_CLIENT_MESSAGE_DEPTH,
                MAX_CLIENT_MESSAGE_VALUES,
                MAX_CLIENT_MESSAGE_UTF16,
            )
        }
        "ui_client_press" => {
            exact_keys(
                object,
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "element",
                    "event",
                ],
            )?;
            validate_client_address(request)?;
            bounded_string(object, "element", MAX_CONTROL_UTF16)?;
            let event = object
                .get("event")
                .and_then(Value::as_object)
                .ok_or_else(|| ModError::Hook("ui_client_press event must be an object".into()))?;
            match event.get("type").and_then(Value::as_str) {
                Some("press") => exact_keys(event, &["type"]),
                Some("input") => {
                    exact_keys(event, &["type", "kind", "value"])?;
                    if !matches!(
                        event.get("kind").and_then(Value::as_str),
                        Some("change" | "submit")
                    ) {
                        return Err(ModError::Hook(
                            "ui_client_press input kind is invalid".into(),
                        ));
                    }
                    bounded_string_allow_empty(event, "value", MAX_PRESS_VALUE_UTF16)?;
                    Ok(())
                }
                Some("select") => {
                    exact_keys(event, &["type", "value"])?;
                    bounded_string_allow_empty(event, "value", MAX_PRESS_VALUE_UTF16)?;
                    Ok(())
                }
                _ => Err(ModError::Hook(
                    "ui_client_press event type is invalid".into(),
                )),
            }
        }
        "ui_press" => validate_parent_ui_press(object),
        "ui_input" => validate_parent_ui_input(object),
        "ui_select" => validate_parent_ui_select(object),
        "ui_client_fault" => {
            exact_keys(
                object,
                &[
                    "subtype",
                    "plugin",
                    "component",
                    "instance_id",
                    "client",
                    "module",
                    "phase",
                    "reason",
                ],
            )?;
            validate_client_address(request)?;
            UiClientFaultRequest::try_from_internal(request)
                .map(|_| ())
                .map_err(|error| ModError::Hook(error.to_string()))
        }
        _ => Err(ModError::Hook("unknown Mod UI control subtype".into())),
    }
}

fn validate_ui_render(request: &Utf16JsonProjection) -> Result<(), ModError> {
    let object = request
        .value
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI control request must be an object".into()))?;
    exact_keys(
        object,
        &[
            "subtype",
            "surface",
            "component",
            "instance_id",
            "props",
            "client_id",
            "viewport",
            "on_screen",
            "content_rows",
            "keyed",
            "bench",
        ],
    )?;
    if !matches!(
        object.get("surface").and_then(Value::as_str),
        Some("desktop" | "mobile" | "vscode")
    ) {
        return Err(ModError::Hook("ui_render surface is invalid".into()));
    }
    component_field(object, "component")?;
    bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?;
    let props = object
        .get("props")
        .and_then(Value::as_object)
        .ok_or_else(|| ModError::Hook("ui_render props must be an object".into()))?;
    validate_plain_json_projection(request, "/props", 32, 20_000, 100_000)?;
    if object.contains_key("client_id") {
        let client_id = bounded_string(object, "client_id", 64)?;
        if !client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ModError::Hook("ui_render client_id is invalid".into()));
        }
    }
    if let Some(viewport) = object.get("viewport") {
        let viewport = viewport
            .as_object()
            .ok_or_else(|| ModError::Hook("ui_render viewport must be an object".into()))?;
        exact_keys(viewport, &["columns", "rows", "isFullscreen"])?;
        positive_integer(viewport, "columns")?;
        positive_integer(viewport, "rows")?;
        if viewport
            .get("isFullscreen")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(ModError::Hook(
                "ui_render viewport isFullscreen must be boolean".into(),
            ));
        }
    }
    if let Some(on_screen) = object.get("on_screen") {
        if !on_screen.is_null() {
            let on_screen = on_screen.as_object().ok_or_else(|| {
                ModError::Hook("ui_render on_screen must be an object or null".into())
            })?;
            exact_keys(on_screen, &["first", "last", "of"])?;
            let first = nonnegative_integer(on_screen, "first")?;
            let last = nonnegative_integer(on_screen, "last")?;
            let of = positive_integer(on_screen, "of")?;
            if first > last || last >= of {
                return Err(ModError::Hook(
                    "ui_render on_screen range is invalid".into(),
                ));
            }
        }
    }
    if let Some(value) = object.get("content_rows") {
        nonnegative_value(value, "content_rows")?;
    }
    if let Some(keyed) = object.get("keyed") {
        let keyed = keyed
            .as_array()
            .filter(|rows| rows.len() <= 512)
            .ok_or_else(|| {
                ModError::Hook("ui_render keyed must contain at most 512 rows".into())
            })?;
        for row in keyed {
            let row = row
                .as_object()
                .ok_or_else(|| ModError::Hook("ui_render keyed row must be an object".into()))?;
            exact_keys(row, &["plugin", "key", "top", "bottom"])?;
            bounded_string(row, "plugin", MAX_CONTROL_UTF16)?;
            bounded_string(row, "key", MAX_CONTROL_UTF16)?;
            let top = nonnegative_integer(row, "top")?;
            let bottom = nonnegative_integer(row, "bottom")?;
            if bottom < top {
                return Err(ModError::Hook(
                    "ui_render keyed row range is invalid".into(),
                ));
            }
        }
    }
    if let Some(bench) = object.get("bench") {
        let bench = bench
            .as_object()
            .ok_or_else(|| ModError::Hook("ui_render bench must be an object".into()))?;
        exact_keys(bench, &["seq", "t0"])?;
        nonnegative_integer(bench, "seq")?;
        if !bench
            .get("t0")
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
        {
            return Err(ModError::Hook("ui_render bench t0 must be finite".into()));
        }
    }
    Ok(())
}

fn validate_client_address(request: &Utf16JsonProjection) -> Result<(), ModError> {
    let object = request
        .value
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI request must be an object".into()))?;
    bounded_string(object, "plugin", MAX_CONTROL_UTF16)?;
    component_field(object, "component")?;
    bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?;
    let key = request
        .string_units("/client")
        .ok_or_else(|| ModError::Hook("Mod UI request needs string field client".into()))?;
    if key.is_empty() || key.len() > MAX_CONTROL_UTF16 {
        return Err(ModError::Hook(
            "Mod UI field client exceeds its UTF-16 limit".into(),
        ));
    }
    bounded_string(object, "module", MAX_CONTROL_UTF16)?;
    Ok(())
}

fn validate_parent_ui_press(object: &Map<String, Value>) -> Result<(), ModError> {
    exact_keys(
        object,
        &[
            "subtype",
            "plugin",
            "handle",
            "key",
            "surface",
            "href",
            "client_id",
        ],
    )?;
    let plugin = string_field(object, "plugin")?;
    if plugin.is_empty() {
        return Err(ModError::Hook("ui_press plugin must not be empty".into()));
    }
    integer_value(object.get("handle"), "handle")?;
    if let Some(value) = object.get("key") {
        value
            .as_str()
            .ok_or_else(|| ModError::Hook("ui_press key must be a string".into()))?;
    }
    parent_surface(object)?;
    if let Some(href) = object.get("href") {
        let href = href
            .as_str()
            .ok_or_else(|| ModError::Hook("ui_press href must be a string".into()))?;
        if href.encode_utf16().count() > 2_048 {
            return Err(ModError::Hook("ui_press href exceeds its limit".into()));
        }
    }
    validate_optional_client_id(object)
}

fn validate_parent_ui_input(object: &Map<String, Value>) -> Result<(), ModError> {
    exact_keys(
        object,
        &[
            "subtype",
            "plugin",
            "handle",
            "kind",
            "value",
            "key",
            "component",
            "instance_id",
            "surface",
            "client_id",
        ],
    )?;
    bounded_string(object, "plugin", MAX_CONTROL_UTF16)?;
    integer_value(object.get("handle"), "handle")?;
    if !matches!(
        object.get("kind").and_then(Value::as_str),
        Some("change" | "submit")
    ) {
        return Err(ModError::Hook(
            "ui_input kind must be change or submit".into(),
        ));
    }
    bounded_string_allow_empty(object, "value", MAX_PRESS_VALUE_UTF16)?;
    validate_optional_parent_address(object)?;
    parent_surface(object)?;
    validate_optional_client_id(object)
}

fn validate_parent_ui_select(object: &Map<String, Value>) -> Result<(), ModError> {
    exact_keys(
        object,
        &[
            "subtype",
            "plugin",
            "handle",
            "value",
            "key",
            "component",
            "instance_id",
            "surface",
            "client_id",
        ],
    )?;
    bounded_string(object, "plugin", MAX_CONTROL_UTF16)?;
    integer_value(object.get("handle"), "handle")?;
    bounded_string_allow_empty(object, "value", MAX_PRESS_VALUE_UTF16)?;
    validate_optional_parent_address(object)?;
    parent_surface(object)?;
    validate_optional_client_id(object)
}

fn validate_optional_parent_address(object: &Map<String, Value>) -> Result<(), ModError> {
    if let Some(key) = object.get("key") {
        key.as_str()
            .ok_or_else(|| ModError::Hook("parent UI key must be a string".into()))?;
    }
    if object.contains_key("component") {
        component_field(object, "component")?;
    }
    if let Some(instance_id) = object.get("instance_id") {
        let instance_id = instance_id
            .as_str()
            .ok_or_else(|| ModError::Hook("instance_id must be a string".into()))?;
        if instance_id.encode_utf16().count() > MAX_CONTROL_UTF16 {
            return Err(ModError::Hook("instance_id exceeds its limit".into()));
        }
    }
    Ok(())
}

fn parent_surface(object: &Map<String, Value>) -> Result<&str, ModError> {
    match object.get("surface").and_then(Value::as_str) {
        Some(surface @ ("desktop" | "mobile" | "vscode")) => Ok(surface),
        _ => Err(ModError::Hook("parent UI surface is invalid".into())),
    }
}

fn integer_value(value: Option<&Value>, name: &str) -> Result<(), ModError> {
    let Some(value) = value else {
        return Err(ModError::Hook(format!("{name} must be an integer")));
    };
    let integer = value.as_i64().is_some() || value.as_u64().is_some();
    let integral_float = value
        .as_f64()
        .is_some_and(|number| number.is_finite() && number.fract() == 0.0);
    if integer || integral_float {
        Ok(())
    } else {
        Err(ModError::Hook(format!("{name} must be an integer")))
    }
}

fn parent_action_handle(value: Option<&Value>) -> Option<u64> {
    let value = value?.as_f64()?;
    (value.is_finite()
        && value.fract() == 0.0
        && value >= 1.0
        && value <= MAX_JS_SAFE_INTEGER as f64)
        .then_some(value as u64)
}

fn validate_optional_client_id(object: &Map<String, Value>) -> Result<(), ModError> {
    if let Some(client_id) = object.get("client_id") {
        let client_id = client_id
            .as_str()
            .ok_or_else(|| ModError::Hook("client_id must be a string".into()))?;
        if client_id.is_empty()
            || client_id.encode_utf16().count() > 64
            || !client_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ModError::Hook("client_id is invalid".into()));
        }
    }
    Ok(())
}

fn validate_client_operation(operation: &Utf16JsonProjection) -> Result<(), ModError> {
    operation
        .validate()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let object = operation
        .value
        .as_object()
        .ok_or_else(|| ModError::Hook("UI Client operation must be an object".into()))?;
    let kind = string_field(object, "type")?;
    match kind {
        "mount" => {
            exact_keys(
                object,
                &[
                    "type",
                    "surface",
                    "component",
                    "instance_id",
                    "plugin",
                    "client",
                    "module",
                    "render_revision",
                    "columns",
                    "rows",
                ],
            )?;
            if object.get("surface").and_then(Value::as_str) != Some("desktop") {
                return Err(ModError::Hook(
                    "Client mount surface must be desktop".into(),
                ));
            }
            component_field(object, "component")?;
            bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?;
            bounded_string(object, "plugin", MAX_CONTROL_UTF16)?;
            let client = operation
                .string_units("/client")
                .ok_or_else(|| ModError::Hook("mount client must be a string".into()))?;
            if client.is_empty() || client.len() > MAX_PARENT_CLIENT_UTF16 {
                return Err(ModError::Hook(
                    "mount client exceeds its UTF-16 limit".into(),
                ));
            }
            bounded_string(object, "module", MAX_PARENT_CLIENT_UTF16)?;
            positive_integer(object, "render_revision")?;
            positive_integer(object, "columns")?;
            positive_integer(object, "rows")?;
        }
        "render" | "unmount" => {
            exact_keys(object, &["type", "runtimeId", "render_revision"])?;
            bounded_string(object, "runtimeId", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
        }
        "setProps" => {
            exact_keys(object, &["type", "runtimeId", "render_revision", "props"])?;
            bounded_string(object, "runtimeId", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
            if !object.get("props").is_some_and(Value::is_object) {
                return Err(ModError::Hook("Client props must be an object".into()));
            }
            validate_plain_json_projection(operation, "/props", 32, 20_000, 100_000)?;
        }
        "resize" => {
            exact_keys(
                object,
                &["type", "runtimeId", "render_revision", "columns", "rows"],
            )?;
            bounded_string(object, "runtimeId", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
            positive_integer(object, "columns")?;
            positive_integer(object, "rows")?;
        }
        "pointer" | "key" => {
            exact_keys(object, &["type", "runtimeId", "render_revision", "event"])?;
            bounded_string(object, "runtimeId", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
            validate_plain_json_projection(operation, "/event", 32, 20_000, 100_000)?;
        }
        "runHeld" => {
            exact_keys(
                object,
                &["type", "runtimeId", "render_revision", "event", "handle"],
            )?;
            bounded_string(object, "runtimeId", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
            positive_integer(object, "handle")?;
            if let Some(event) = object.get("event") {
                validate_plain_json_projection(operation, "/event", 32, 20_000, 100_000)?;
            }
        }
        "draw_commit" => {
            exact_keys(
                object,
                &[
                    "type",
                    "surface",
                    "component",
                    "instance_id",
                    "render_revision",
                    "clients",
                ],
            )?;
            if object.get("surface").and_then(Value::as_str) != Some("desktop") {
                return Err(ModError::Hook(
                    "Client draw_commit surface must be desktop".into(),
                ));
            }
            component_field(object, "component")?;
            bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
            let clients = object
                .get("clients")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ModError::Hook("Client draw_commit clients must be an array".into())
                })?;
            if clients.len() > 20_000 {
                return Err(ModError::Hook(
                    "Client draw_commit exceeds its item limit".into(),
                ));
            }
            for (index, raw_client) in clients.iter().enumerate() {
                let client = raw_client.as_object().ok_or_else(|| {
                    ModError::Hook("drawn Client identity must be an object".into())
                })?;
                exact_keys(client, &["plugin", "key", "module"])?;
                bounded_string(client, "plugin", MAX_CONTROL_UTF16)?;
                let key = operation
                    .string_units(&format!("/clients/{index}/key"))
                    .ok_or_else(|| ModError::Hook("drawn Client key must be a string".into()))?;
                if key.is_empty() || key.len() > MAX_PARENT_CLIENT_UTF16 {
                    return Err(ModError::Hook(
                        "drawn Client key exceeds its UTF-16 limit".into(),
                    ));
                }
                bounded_string(client, "module", MAX_PARENT_CLIENT_UTF16)?;
            }
        }
        "draw_unmount" => {
            exact_keys(
                object,
                &[
                    "type",
                    "surface",
                    "component",
                    "instance_id",
                    "render_revision",
                ],
            )?;
            if object.get("surface").and_then(Value::as_str) != Some("desktop") {
                return Err(ModError::Hook(
                    "Client draw_unmount surface must be desktop".into(),
                ));
            }
            component_field(object, "component")?;
            bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?;
            positive_integer(object, "render_revision")?;
        }
        _ => return Err(ModError::Hook("unknown Client operation type".into())),
    }
    Ok(())
}

fn component_field(object: &Map<String, Value>, name: &str) -> Result<ClientUiComponent, ModError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .and_then(ClientUiComponent::parse)
        .ok_or_else(|| ModError::Hook(format!("{name} is not a supported UI component")))
}

fn exact_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ModError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ModError::Hook("Mod UI request has unknown fields".into()));
    }
    Ok(())
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, ModError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ModError::Hook(format!("Mod UI request needs string field {name}")))
}

fn bounded_string<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
    maximum: usize,
) -> Result<&'a str, ModError> {
    let value = string_field(object, name)?;
    if value.is_empty() || value.encode_utf16().count() > maximum {
        return Err(ModError::Hook(format!(
            "Mod UI field {name} exceeds its UTF-16 limit"
        )));
    }
    Ok(value)
}

fn bounded_string_allow_empty<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
    maximum: usize,
) -> Result<&'a str, ModError> {
    let value = string_field(object, name)?;
    if value.encode_utf16().count() > maximum {
        return Err(ModError::Hook(format!(
            "Mod UI field {name} exceeds its UTF-16 limit"
        )));
    }
    Ok(value)
}

fn nonnegative_value(value: &Value, name: &str) -> Result<u64, ModError> {
    value
        .as_u64()
        .filter(|value| *value <= MAX_JS_SAFE_INTEGER)
        .ok_or_else(|| ModError::Hook(format!("{name} must be a non-negative safe integer")))
}

fn nonnegative_integer(object: &Map<String, Value>, name: &'static str) -> Result<u64, ModError> {
    nonnegative_value(object.get(name).unwrap_or(&Value::Null), name)
}

fn positive_integer(object: &Map<String, Value>, name: &'static str) -> Result<u64, ModError> {
    nonnegative_integer(object, name).and_then(|value| {
        (value > 0)
            .then_some(value)
            .ok_or_else(|| ModError::Hook(format!("{name} must be positive")))
    })
}

fn validate_plain_json_projection(
    projection: &Utf16JsonProjection,
    pointer: &str,
    maximum_depth: usize,
    maximum_values: usize,
    maximum_utf16: usize,
) -> Result<(), ModError> {
    let value = if pointer.is_empty() {
        projection.clone()
    } else {
        projection
            .subprojection(pointer)
            .map_err(|error| ModError::Protocol(error.to_string()))?
    };
    validate_plain_json(&value.value, maximum_depth, maximum_values, usize::MAX)?;
    let encoded = value
        .to_json_string()
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    if encoded.encode_utf16().count() > maximum_utf16 {
        return Err(ModError::Hook(
            "Mod UI data exceeds its serialized character limit".into(),
        ));
    }
    Ok(())
}

fn validate_plain_json(
    value: &Value,
    maximum_depth: usize,
    maximum_values: usize,
    maximum_utf16: usize,
) -> Result<(), ModError> {
    fn walk(
        value: &Value,
        depth: usize,
        maximum_depth: usize,
        maximum_values: usize,
        count: &mut usize,
    ) -> Result<(), ModError> {
        if depth > maximum_depth {
            return Err(ModError::Hook(
                "Mod UI data exceeds its nesting limit".into(),
            ));
        }
        *count = count.saturating_add(1);
        if *count > maximum_values {
            return Err(ModError::Hook("Mod UI data exceeds its value limit".into()));
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    walk(item, depth + 1, maximum_depth, maximum_values, count)?;
                }
            }
            Value::Object(items) => {
                for item in items.values() {
                    walk(item, depth + 1, maximum_depth, maximum_values, count)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut count = 0;
    walk(value, 0, maximum_depth, maximum_values, &mut count)?;
    let encoded =
        serde_json::to_string(value).map_err(|error| ModError::Protocol(error.to_string()))?;
    if encoded.encode_utf16().count() > maximum_utf16 {
        return Err(ModError::Hook(
            "Mod UI data exceeds its serialized character limit".into(),
        ));
    }
    if count > maximum_values {
        return Err(ModError::Hook("Mod UI data exceeds its value limit".into()));
    }
    Ok(())
}

fn client_site_from_input(input: &Value) -> Result<ClientUiRenderSite, ModError> {
    let object = input
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI request must be an object".into()))?;
    let component = component_field(object, "component")?;
    let request_id = bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?;
    ClientUiRenderSite::new(component, request_id)
        .map_err(|error| ModError::Hook(error.to_string()))
}

fn parent_control_site_from_input(input: &Value) -> Result<ParentControlSite, ModError> {
    let object = input
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI request must be an object".into()))?;
    Ok(ParentControlSite {
        surface: parent_surface(object)?.to_owned(),
        component: component_field(object, "component")?,
        request_id: bounded_string(object, "instance_id", MAX_CONTROL_UTF16)?.to_owned(),
    })
}

fn client_identity_from_control(input: &Utf16JsonProjection) -> Result<ClientUiIdentity, ModError> {
    let object = input
        .value
        .as_object()
        .ok_or_else(|| ModError::Hook("Mod UI request must be an object".into()))?;
    let key = input
        .string_units("/client")
        .ok_or_else(|| ModError::Hook("Mod UI request needs string field client".into()))?;
    ClientUiIdentity::new(
        bounded_string(object, "plugin", MAX_CONTROL_UTF16)?,
        bounded_string(object, "module", MAX_CONTROL_UTF16)?,
        key,
    )
    .map_err(|error| ModError::Hook(error.to_string()))
}

fn pointer_child(parent: &str, token: &str) -> String {
    let token = token.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{token}")
}

fn exact_string_projection(units: Vec<u16>) -> Utf16JsonProjection {
    let (value, exact) = match String::from_utf16(&units) {
        Ok(value) => (value, false),
        Err(_) => (String::from_utf16_lossy(&units), true),
    };
    let mut projection = Utf16JsonProjection::plain(Value::String(value));
    if exact {
        projection.strings.push(Utf16JsonString {
            pointer: String::new(),
            code_units: units,
        });
    }
    projection
}

fn set_exact_string_field(
    projection: &mut Utf16JsonProjection,
    field: &str,
    units: Vec<u16>,
) -> Result<(), ModError> {
    projection
        .set_field(field, exact_string_projection(units))
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn collect_client_parents(
    projection: &Utf16JsonProjection,
    clients: &mut HashMap<ClientUiIdentity, Utf16JsonProjection>,
    seen_plugin_keys: &mut HashSet<(String, Vec<u16>)>,
) -> Result<(), ModError> {
    fn visit(
        projection: &Utf16JsonProjection,
        node: &Value,
        pointer: &str,
        clients: &mut HashMap<ClientUiIdentity, Utf16JsonProjection>,
        seen_plugin_keys: &mut HashSet<(String, Vec<u16>)>,
    ) -> Result<(), ModError> {
        let Some(object) = node.as_object() else {
            if let Some(children) = node.as_array() {
                for (index, child) in children.iter().enumerate() {
                    visit(
                        projection,
                        child,
                        &pointer_child(pointer, &index.to_string()),
                        clients,
                        seen_plugin_keys,
                    )?;
                }
            }
            return Ok(());
        };
        if object.get("type").and_then(Value::as_str) == Some("Client") {
            super::ui::validate_client_parent_element(node).map_err(ModError::Hook)?;
            let props = object
                .get("props")
                .and_then(Value::as_object)
                .expect("Client parent validator checked props");
            let identity_object = object
                .get("client")
                .and_then(Value::as_object)
                .ok_or_else(|| ModError::Hook("Client parent has no trusted owner".into()))?;
            let plugin = identity_object
                .get("plugin")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ModError::Hook("Client parent has no trusted owner".into()))?;
            let props_pointer = pointer_child(pointer, "props");
            let key_pointer = pointer_child(&props_pointer, "key");
            let key = projection
                .string_units(&key_pointer)
                .ok_or_else(|| ModError::Hook("Client parent key is not a string".into()))?;
            let module = props
                .get("module")
                .and_then(Value::as_str)
                .expect("Client parent validator checked module");
            if !seen_plugin_keys.insert((plugin.to_owned(), key.clone())) {
                return Err(ModError::Hook(
                    "ui.render draws a Client key more than once for one plugin".into(),
                ));
            }
            let identity = ClientUiIdentity::new(plugin, module, key)
                .map_err(|error| ModError::Hook(error.to_string()))?;
            let client_props = if props.contains_key("props") {
                projection
                    .subprojection(&pointer_child(&props_pointer, "props"))
                    .map_err(|error| ModError::Protocol(error.to_string()))?
            } else {
                Utf16JsonProjection::plain(Value::Object(Map::new()))
            };
            validate_plain_json_projection(&client_props, "", 32, 20_000, 100_000)?;
            clients.insert(identity, client_props);
            return Ok(());
        }
        if let Some(children) = object.get("children") {
            visit(
                projection,
                children,
                &pointer_child(pointer, "children"),
                clients,
                seen_plugin_keys,
            )?;
        }
        Ok(())
    }

    visit(projection, &projection.value, "", clients, seen_plugin_keys)
}

fn collect_parent_press_actions(
    tree: &Value,
    site: &ParentControlSite,
) -> Result<Vec<ParentUiPressAction>, ModError> {
    fn visit(
        node: &Value,
        site: &ParentControlSite,
        actions: &mut HashMap<(String, u64), ParentUiPressAction>,
    ) -> Result<(), ModError> {
        if let Some(nodes) = node.as_array() {
            for child in nodes {
                visit(child, site, actions)?;
            }
            return Ok(());
        }
        let Some(object) = node.as_object() else {
            return Ok(());
        };
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "Button" | "Input" | "Select" | "Markdown") {
            if let Some(token) = object.get("press") {
                let token = token.as_object().ok_or_else(|| {
                    ModError::Protocol("parent UI press token is not an object".into())
                })?;
                if token
                    .keys()
                    .any(|key| !matches!(key.as_str(), "plugin" | "handle"))
                {
                    return Err(ModError::Protocol(
                        "parent UI press token has unknown fields".into(),
                    ));
                }
                let plugin = token
                    .get("plugin")
                    .and_then(Value::as_str)
                    .filter(|plugin| !plugin.is_empty())
                    .ok_or_else(|| {
                        ModError::Protocol("parent UI press token has no plugin".into())
                    })?
                    .to_owned();
                let handle = token
                    .get("handle")
                    .and_then(Value::as_u64)
                    .filter(|handle| *handle > 0 && *handle <= MAX_JS_SAFE_INTEGER)
                    .ok_or_else(|| {
                        ModError::Protocol("parent UI press token has an invalid handle".into())
                    })?;
                let props = object
                    .get("props")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ModError::Protocol("pressable parent UI element has no props".into())
                    })?;
                let element = props.get("key").and_then(Value::as_str).map(str::to_owned);
                if element.as_deref().is_none_or(str::is_empty) {
                    return Err(ModError::Protocol(
                        "parent held-action element has no key".into(),
                    ));
                }
                let select_values = if kind == "Select" {
                    props
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|option| {
                                    option
                                        .get("value")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    HashSet::new()
                };
                let pressable_links = if kind == "Markdown" {
                    props
                        .get("pressableLinks")
                        .and_then(Value::as_array)
                        .map(|links| {
                            links
                                .iter()
                                .map(|link| {
                                    link.as_str().map(str::to_owned).ok_or_else(|| {
                                        ModError::Protocol(
                                            "Markdown pressable link is not a string".into(),
                                        )
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .transpose()?
                } else {
                    None
                };
                let action = ParentUiPressAction {
                    site: site.clone(),
                    surface: site.surface.clone(),
                    plugin: plugin.clone(),
                    handle,
                    element,
                    kind: kind.to_owned(),
                    pressable_links,
                    select_values,
                };
                if let Some(previous) = actions.insert((plugin, handle), action.clone()) {
                    if previous.site != action.site
                        || previous.element != action.element
                        || previous.kind != action.kind
                        || previous.pressable_links != action.pressable_links
                        || previous.select_values != action.select_values
                    {
                        return Err(ModError::Protocol(
                            "parent UI tree repeats a press handle for different elements".into(),
                        ));
                    }
                }
            }
        }
        if let Some(children) = object.get("children") {
            visit(children, site, actions)?;
        }
        Ok(())
    }

    let mut actions = HashMap::new();
    visit(tree, site, &mut actions)?;
    Ok(actions.into_values().collect())
}

impl ModHost {
    pub(super) async fn invalidate_unloaded_client_sites(
        &self,
        sites: &[ClientUiRenderSite],
        session: &dyn ModSessionContext,
    ) {
        if sites.is_empty() {
            return;
        }
        let sequence = self
            .client_ui
            .next_invalidation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let uuid = format!("mod-ui-{}-{sequence}", self.instance_id);
        let instances = sites
            .iter()
            .map(|site| {
                json!({
                    "surface":"desktop",
                    "component":site.component().as_str(),
                    "instance_id":site.request_id(),
                })
            })
            .collect::<Vec<_>>();
        let session_id = session.id().await;
        session
            .emit_mod_ui_invalidate(
                Some(&Value::Array(instances).to_string()),
                &uuid,
                &session_id,
            )
            .await;
    }

    /// Dispatch one canonical Native `ui_*` control request for this session.
    pub async fn dispatch_client_ui_control(
        &self,
        request: Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<ModUiControlOutcome, ModError> {
        let request = normalize_control_request(&request)?;
        let subtype = request.value["subtype"]
            .as_str()
            .expect("validated subtype");
        let (response, render_revision, client_runtime_epochs, client_state_token) = match subtype {
            "ui_client_module" => (
                Utf16JsonProjection::plain(self.client_module_manifest(&request.value)?),
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_render" => {
                let outcome = self.dispatch_parent_ui_render(&request, session).await?;
                (
                    outcome.response,
                    outcome.render_revision,
                    outcome.client_runtime_epochs,
                    outcome.client_state_token,
                )
            }
            "ui_message" => (
                self.dispatch_client_message(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_client_press" => (
                self.dispatch_client_press(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_client_fault" => (
                Utf16JsonProjection::plain(self.dispatch_client_fault(&request, session).await?),
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_press" => (
                self.dispatch_parent_ui_press(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_input" => (
                self.dispatch_parent_ui_input(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            "ui_select" => (
                self.dispatch_parent_ui_select(&request, session).await?,
                None,
                BTreeMap::new(),
                None,
            ),
            _ => unreachable!("canonical request validator accepted an unknown subtype"),
        };
        Ok(ModUiControlOutcome {
            response: response,
            render_revision,
            client_runtime_epochs,
            client_state_token,
        })
    }

    /// Execute a host-local Client VM operation. Renderer data never supplies
    /// module source, a graph, a manifest, or the props used for first mount.
    pub async fn dispatch_client_ui_operation(
        &self,
        operation: Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        validate_client_operation(&operation)?;
        let kind = operation.value["type"].as_str().expect("validated type");
        let revision = operation.value["render_revision"]
            .as_u64()
            .expect("validated render revision");
        match kind {
            "draw_commit" => Ok(Utf16JsonProjection::plain(
                self.commit_client_draw(&operation, revision, session)
                    .await?,
            )),
            "draw_unmount" => Ok(Utf16JsonProjection::plain(
                self.unmount_client_site(&operation.value, revision).await?,
            )),
            "mount" => {
                self.mount_client_from_parent(&operation, revision, session)
                    .await
            }
            _ => self.dispatch_runtime_operation(&operation, revision).await,
        }
    }

    fn client_module_manifest(&self, request: &Value) -> Result<Value, ModError> {
        let plugin = request["plugin"]
            .as_str()
            .expect("validated ui_client_module plugin");
        let prepared = lock(&self.client_ui.data).modules.get(plugin).cloned();
        match prepared {
            Some(prepared) => prepared
                .for_plugin(plugin)
                .map_err(|error| ModError::Protocol(error.to_string())),
            None => Ok(Value::Null),
        }
    }

    async fn dispatch_parent_ui_render(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<ModUiControlOutcome, ModError> {
        let surface = request.value["surface"]
            .as_str()
            .expect("validated surface");
        let component = request.value["component"]
            .as_str()
            .expect("validated component");
        let instance_id = request.value["instance_id"]
            .as_str()
            .expect("validated instance_id");
        let props = request
            .subprojection("/props")
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let on_screen = request
            .value
            .get("on_screen")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let render_pace = ModUiRenderPace::for_site(surface, component, on_screen);
        let revision = self.client_ui.next_revision()?;
        let control_site = parent_control_site_from_input(&request.value)?;
        {
            let mut data = lock(&self.client_ui.data);
            data.parent_control_latest
                .insert(control_site.clone(), revision);
        }
        let site = if surface == "desktop" {
            Some(client_site_from_input(&request.value)?)
        } else {
            None
        };
        let token = site.as_ref().map(|site| {
            let state_version = self.current_client_ui_render_state_version(
                "desktop",
                site.component().as_str(),
                site.request_id(),
            );
            self.client_ui
                .fault_registry
                .begin_render(site.clone(), state_version)
        });
        if let (Some(site), Some(token)) = (site.as_ref(), token) {
            let mut data = lock(&self.client_ui.data);
            data.latest_started.insert(site.clone(), revision);
            data.pending_tokens.insert((site.clone(), revision), token);
        }

        let mut event = json!({
            "surface": surface,
            "component": component,
            "requestId": instance_id,
            "props": props.value.clone(),
        });
        let event_object = event.as_object_mut().expect("event is an object");
        for (wire, hook) in [
            ("client_id", "clientId"),
            ("on_screen", "onScreen"),
            ("content_rows", "contentRows"),
        ] {
            if let Some(value) = request.value.get(wire) {
                event_object.insert(hook.into(), value.clone());
            }
        }
        for field in ["viewport", "keyed", "bench"] {
            if let Some(value) = request.value.get(field) {
                event_object.insert(field.into(), value.clone());
            }
        }
        let event = super::ui::normalize_ui_render_event(&event).map_err(ModError::Hook)?;
        let mut event_projection = Utf16JsonProjection::plain(event);
        event_projection
            .set_field("props", props.clone())
            .map_err(|error| ModError::Protocol(error.to_string()))?;

        let engine = Utf16JsonProjection::plain(json!({"type":"engine","ref":revision}));
        let engine_for_core = engine.clone();
        let final_props = std::sync::Arc::new(StdMutex::new(props.clone()));
        let captured_props = final_props.clone();
        let dispatch_result = self
            .dispatch_ui_render_with_state_scope_at_session(
                event_projection,
                revision,
                session,
                move |forwarded| {
                    *lock(&captured_props) = forwarded
                        .subprojection("/props")
                        .unwrap_or_else(|_| Utf16JsonProjection::plain(Value::Null));
                    let result = engine_for_core.clone();
                    async move { Ok(result) }
                },
                |plugin, text| async move { session.emit_mod_log(&plugin, &text).await },
                |plugin, text, timeout_ms| async move {
                    session.emit_mod_toast(&plugin, &text, timeout_ms).await
                },
                |plugin, text| async move {
                    session.emit_mod_status(&plugin, text.as_deref()).await
                },
            )
            .await;
        let (tree, render_plugins) = match dispatch_result {
            Ok(outcome) => (
                core_projection_from_mod(super::ModUtf16ValueProjection {
                    value: outcome.result,
                    strings: outcome.result_utf16_strings,
                    keys: outcome.result_utf16_keys,
                })?,
                outcome.ui_render_plugins,
            ),
            Err(error) => {
                if let Some(site) = site.as_ref() {
                    lock(&self.client_ui.data)
                        .pending_tokens
                        .remove(&(site.clone(), revision));
                }
                self.discard_client_ui_render_state(surface, component, instance_id, revision);
                return Err(error);
            }
        };
        let normalized_tree_is_invalid = tree.validate().is_err()
            || (surface == "desktop"
                && tree.value.get("type").and_then(Value::as_str) != Some("engine")
                && super::ui::validate_desktop_parent_tree(&tree.value).is_err());
        let tree = if normalized_tree_is_invalid {
            engine.clone()
        } else {
            tree
        };
        let resolved_props = if normalized_tree_is_invalid {
            props.clone()
        } else {
            lock(&final_props).clone()
        };

        let mut clients = HashMap::new();
        let mut client_modules = Map::new();
        let mut client_runtime_epochs = BTreeMap::new();
        let press_actions = collect_parent_press_actions(&tree.value, &control_site)?;
        if surface == "desktop" {
            collect_client_parents(&tree, &mut clients, &mut HashSet::new())?;
            let data = lock(&self.client_ui.data);
            for identity in clients.keys() {
                if let Some(prepared) = data.modules.get(identity.plugin()) {
                    if prepared
                        .manifest
                        .modules
                        .iter()
                        .any(|module| module.module == identity.module())
                    {
                        client_modules.insert(
                            identity.plugin().to_owned(),
                            Value::String(prepared.manifest.hash.clone()),
                        );
                        if let Some(epoch) = data.client_runtime_epochs.get(identity.plugin()) {
                            client_runtime_epochs.insert(identity.plugin().to_owned(), *epoch);
                        }
                    }
                }
            }
        }

        let mut client_state_token = None;
        if let Some(site) = site {
            let has_client_modules = !client_modules.is_empty();
            let has_clients = !clients.is_empty() && has_client_modules;
            let (is_latest, invalidation_update) = {
                let mut data = lock(&self.client_ui.data);
                let is_latest = data.latest_started.get(&site) == Some(&revision);
                data.pending_tokens.remove(&(site.clone(), revision));
                if is_latest {
                    let (state_token, update) = self.finish_client_ui_render_state(
                        surface,
                        component,
                        instance_id,
                        revision,
                        render_plugins,
                        has_clients,
                        render_pace,
                    );
                    client_state_token = state_token.clone();
                    let version = state_token
                        .as_deref()
                        .and_then(|token| token.parse::<u64>().ok())
                        .unwrap_or(0);
                    let token = self
                        .client_ui
                        .fault_registry
                        .begin_render(site.clone(), version);
                    data.parents.insert(
                        site.clone(),
                        ParentRender {
                            revision,
                            token: Some(token),
                            committed_generation: None,
                            clients,
                        },
                    );
                    (true, update)
                } else {
                    (false, ModUiInvalidationUpdate::default())
                }
            };
            if is_latest {
                self.apply_ui_invalidation_update(invalidation_update, Some(session))
                    .await;
            } else {
                self.discard_client_ui_render_state(surface, component, instance_id, revision);
            }
        }
        {
            let mut data = lock(&self.client_ui.data);
            if data.parent_control_latest.get(&control_site) == Some(&revision) {
                data.parent_control_renders.insert(
                    control_site,
                    ParentControlRender {
                        revision,
                        press_actions,
                    },
                );
            }
        }

        let rewritten = tree != engine || resolved_props != props;
        let mut response = Utf16JsonProjection::plain(json!({
            "rewritten": rewritten,
            "hooked": self.has_event("ui.render"),
        }));
        response
            .set_field("tree", tree)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        response
            .set_field("props", resolved_props)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        if !client_modules.is_empty() {
            response.value["client_modules"] = Value::Object(client_modules);
        }
        if let Some(bench) = request.value.get("bench") {
            response.value["bench"] = bench.clone();
        }
        Ok(ModUiControlOutcome {
            response,
            render_revision: Some(revision),
            client_runtime_epochs,
            client_state_token,
        })
    }

    async fn dispatch_client_press(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_input(&request.value)?;
        let identity = client_identity_from_control(request)?;
        if !self.client_is_currently_drawn(&site, &identity) {
            return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
        }
        let plugin = identity.plugin().to_owned();
        let surface = "desktop";
        let component = site.component().as_str();
        let request_id = site.request_id();
        let event = &request.value["event"];
        let (hook_event, mut input, include_value) = match event["type"].as_str() {
            Some("press") => (
                "ui.press",
                json!({"plugin":plugin,"element":"","component":component,"requestId":request_id,"surface":surface}),
                false,
            ),
            Some("input") => (
                "ui.input",
                json!({"plugin":plugin,"element":"","component":component,"requestId":request_id,"surface":surface,
                    "kind":event["kind"],"value":event["value"]}),
                true,
            ),
            Some("select") => (
                "ui.select",
                json!({"plugin":plugin,"element":"","component":component,"requestId":request_id,"surface":surface,
                    "value":event["value"]}),
                true,
            ),
            _ => unreachable!("press request was validated"),
        };
        let mut input = Utf16JsonProjection::plain(input);
        set_exact_string_field(&mut input, "element", identity.key_units().to_vec())?;
        if include_value {
            input
                .set_field(
                    "value",
                    request
                        .subprojection("/event/value")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        let reached = std::sync::Arc::new(StdMutex::new(None::<Utf16JsonProjection>));
        let reached_core = reached.clone();
        let plugin_for_dispatch = plugin.clone();
        let result = self
            .dispatch_plugin_scoped_ui_event(
                hook_event,
                &plugin_for_dispatch,
                input,
                session,
                None,
                move |forwarded| {
                    *lock(&reached_core) = Some(forwarded.clone());
                    let mut response = Utf16JsonProjection::plain(json!({"handled":true}));
                    let mut response = response;
                    if let Ok(element) = forwarded.subprojection("/element") {
                        response
                            .set_field("element", element)
                            .expect("element field belongs to object");
                    }
                    if include_value {
                        let value = forwarded
                            .subprojection("/value")
                            .expect("validated input has a value");
                        response
                            .set_field("value", value)
                            .expect("value field belongs to object");
                    }
                    async move { Ok(response) }
                },
            )
            .await?;
        let handled = result
            .value
            .get("handled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut response = Utf16JsonProjection::plain(json!({"handled":handled}));
        if let Some(reached) = lock(&reached).as_ref() {
            let mut value = Utf16JsonProjection::plain(json!({}));
            if let Ok(element) = reached.subprojection("/element") {
                value
                    .set_field("element", element)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
            if include_value {
                if let Ok(field_value) = reached.subprojection("/value") {
                    value
                        .set_field("value", field_value)
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
            }
            response
                .set_field("reached", value)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        Ok(response)
    }

    async fn dispatch_parent_ui_press(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let href = request.value.get("href").and_then(Value::as_str);
        let expected_kind = if href.is_some() { "Markdown" } else { "Button" };
        self.dispatch_parent_held_action(request, "ui.press", Some(expected_kind), href, session)
            .await
    }

    async fn dispatch_parent_ui_input(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        self.dispatch_parent_held_action(request, "ui.input", Some("Input"), None, session)
            .await
    }

    async fn dispatch_parent_ui_select(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        self.dispatch_parent_held_action(request, "ui.select", Some("Select"), None, session)
            .await
    }

    async fn dispatch_parent_held_action(
        &self,
        request: &Utf16JsonProjection,
        event_name: &'static str,
        expected_kind: Option<&str>,
        href: Option<&str>,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let plugin = request.value["plugin"].as_str().expect("validated plugin");
        let surface = request.value["surface"]
            .as_str()
            .expect("defaulted surface");
        let handle = parent_action_handle(request.value.get("handle"));
        let action = handle.and_then(|handle| {
            self.current_parent_press_action(
                plugin,
                handle,
                surface,
                request.value.get("key").and_then(Value::as_str),
                request
                    .value
                    .get("component")
                    .and_then(Value::as_str)
                    .and_then(ClientUiComponent::parse),
                request.value.get("instance_id").and_then(Value::as_str),
                expected_kind,
            )
        });
        let Some(action) = action else {
            return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
        };
        if event_name == "ui.press" && action.surface != "desktop" {
            return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
        }
        let mut press_href_admitted = false;
        if event_name == "ui.press" {
            if let Some(href) = href {
                if let Some(allowed) = action.pressable_links.as_ref() {
                    if !self
                        .preflight_parent_press_href(href, allowed, &session.cwd())
                        .await?
                    {
                        return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
                    }
                }
                // The capability stays in the private Host→worker dispatch
                // envelope. It is stripped before the plugin sees the event.
                press_href_admitted = true;
            }
            if href.is_some() && action.kind != "Markdown" {
                return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
            }
        }
        let value = if event_name == "ui.press" {
            None
        } else {
            Some(request.value["value"].as_str().expect("validated value"))
        };
        if event_name == "ui.select"
            && !action
                .select_values
                .contains(value.expect("select has value"))
        {
            return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
        }

        let mut input = json!({
            "plugin":action.plugin.as_str(),
            "component":action.site.component.as_str(),
            "requestId":action.site.request_id.as_str(),
            "surface":action.surface.as_str(),
        });
        if let Some(element) = action.element.as_ref() {
            input["element"] = Value::String(element.clone());
        }
        if event_name == "ui.input" {
            input["kind"] = request.value["kind"].clone();
        }
        if let Some(href) = href {
            input["link"] = json!({"href":href});
        }
        if let Some(value) = value {
            input["value"] = Value::String(value.to_owned());
        }
        let mut input = Utf16JsonProjection::plain(input);
        if let Some(value) = value {
            input
                .set_field(
                    "value",
                    request
                        .subprojection("/value")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        let mut press_token = json!({"plugin":action.plugin.as_str(),"handle":action.handle});
        if press_href_admitted {
            press_token["hostPressHrefAdmitted"] = Value::Bool(true);
        }
        let reached = std::sync::Arc::new(StdMutex::new(None::<Utf16JsonProjection>));
        let reached_core = reached.clone();
        let include_value = event_name != "ui.press";
        let plugin_for_dispatch = action.plugin.clone();
        let result = self
            .dispatch_plugin_scoped_ui_event(
                event_name,
                &plugin_for_dispatch,
                input,
                session,
                Some(press_token),
                move |forwarded| {
                    *lock(&reached_core) = Some(forwarded.clone());
                    let mut response = Utf16JsonProjection::plain(json!({"handled":true}));
                    if let Ok(element) = forwarded.subprojection("/element") {
                        response
                            .set_field("element", element)
                            .expect("element belongs to response object");
                    }
                    if include_value {
                        let value = forwarded
                            .subprojection("/value")
                            .expect("validated input has value");
                        response
                            .set_field("value", value)
                            .expect("value belongs to response object");
                    }
                    async move { Ok(response) }
                },
            )
            .await?;
        let handled = result
            .value
            .get("handled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut response = Utf16JsonProjection::plain(json!({"handled":handled}));
        if handled {
            if let Some(forwarded) = lock(&reached).as_ref() {
                if let Ok(element) = forwarded.subprojection("/element") {
                    response
                        .set_field("element", element)
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
                if include_value {
                    if let Ok(value) = forwarded.subprojection("/value") {
                        response
                            .set_field("value", value)
                            .map_err(|error| ModError::Protocol(error.to_string()))?;
                    }
                }
            }
        }
        Ok(response)
    }

    async fn preflight_parent_press_href(
        &self,
        href: &str,
        pressable_links: &[String],
        cwd: &Path,
    ) -> Result<bool, ModError> {
        let cwd = cwd
            .to_str()
            .ok_or_else(|| ModError::Protocol("session cwd is not valid UTF-8".into()))?;
        let mut request = self
            .request(json!({
                "kind":"ui.press.preflight",
                "href":href,
                "pressableLinks":pressable_links,
                "cwd":cwd,
            }))
            .await?;
        let reply = self.receive(&mut request).await?;
        if reply.get("kind").and_then(Value::as_str) != Some("ui.press.preflight.result") {
            return Err(ModError::Protocol(format!(
                "unexpected UI press preflight reply: {reply}"
            )));
        }
        reply
            .get("admitted")
            .and_then(Value::as_bool)
            .ok_or_else(|| ModError::Protocol("UI press preflight has no admission result".into()))
    }

    async fn dispatch_client_message(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_input(&request.value)?;
        let identity = client_identity_from_control(request)?;
        let Some(instance) = self.current_instance(&site, &identity) else {
            return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
        };
        if !self.client_is_currently_drawn(&site, &identity) {
            return Ok(Utf16JsonProjection::plain(json!({"handled":false})));
        }
        let mut input = Utf16JsonProjection::plain(json!({
            "surface":"desktop",
            "component":site.component().as_str(),
            "requestId":site.request_id(),
            "element":String::from_utf16_lossy(identity.key_units()),
            "module":identity.module(),
            "data":request.value["data"],
        }));
        set_exact_string_field(&mut input, "element", identity.key_units().to_vec())?;
        input
            .set_field(
                "data",
                request
                    .subprojection("/data")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            )
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let plugin = identity.plugin().to_owned();
        let result = self
            .dispatch_plugin_scoped_ui_event(
                "ui.message",
                &plugin,
                input,
                session,
                None,
                |_| async { Ok(Utf16JsonProjection::plain(json!({"handled":true}))) },
            )
            .await?;
        let handled = result
            .value
            .get("handled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let mut response = Utf16JsonProjection::plain(json!({"handled":handled}));
        if result.value.get("props").is_some_and(Value::is_object) {
            let props = result
                .subprojection("/props")
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            let mut operation = Utf16JsonProjection::plain(json!({
                "type":"setProps",
                "runtimeId":instance.runtime_id.clone(),
                "generation":instance.worker_generation,
                "props":props.value.clone(),
            }));
            operation
                .set_field("props", props.clone())
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            let frame = self.run_client_worker_operation(operation).await?;
            self.update_instance_props(&instance.runtime_id, props.clone());
            if let Some(frame) =
                self.frame_for_worker_result(&instance.runtime_id, instance.revision, &frame)
            {
                self.emit_client_frame(session, &instance.runtime_id, instance.revision, &frame)
                    .await;
            }
            response
                .set_field("props", props)
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        Ok(response)
    }

    async fn dispatch_client_fault(
        &self,
        request: &Utf16JsonProjection,
        session: &dyn ModSessionContext,
    ) -> Result<Value, ModError> {
        let fault = UiClientFaultRequest::try_from_internal(request)
            .map_err(|error| ModError::Hook(error.to_string()))?;
        let session_id = session.id().await;
        let adapter = ClientFaultHostAdapter {
            host: self,
            session,
            session_id,
        };
        let outcome =
            dispatch_client_ui_fault(&self.client_ui.fault_registry, &fault, &adapter).await?;
        Ok(
            json!({"handled":!matches!(outcome, crate::mod_ui_fault::ClientUiFaultOutcome::NotDrawn)}),
        )
    }

    async fn dispatch_mount_client_fault(
        &self,
        site: &ClientUiRenderSite,
        identity: &ClientUiIdentity,
        phase: &str,
        reason: &str,
        session: &dyn ModSessionContext,
    ) {
        let reason = crate::mod_ui_fault::normalize_client_fault_reason(
            identity.plugin(),
            identity.module(),
            reason,
        );
        let mut request = Utf16JsonProjection::plain(json!({
            "subtype":"ui_client_fault",
            "plugin":identity.plugin(),
            "component":site.component().as_str(),
            "instance_id":site.request_id(),
            "client":String::from_utf16_lossy(identity.key_units()),
            "module":identity.module(),
            "phase":phase,
            "reason":reason,
        }));
        if set_exact_string_field(&mut request, "client", identity.key_units().to_vec()).is_err() {
            return;
        }
        // The operation response carries the authoritative worker failure.
        // A hook failure while reporting the worker's async fault must not
        // replace that result with a second, transport-shaped failure.
        let _ = self.dispatch_client_fault(&request, session).await;
    }

    fn client_is_currently_drawn(
        &self,
        site: &ClientUiRenderSite,
        identity: &ClientUiIdentity,
    ) -> bool {
        let address =
            crate::mod_ui_fault::ClientUiFaultAddress::new(site.clone(), identity.clone());
        self.client_ui.fault_registry.is_drawn(&address)
    }

    fn current_parent_press_action(
        &self,
        plugin: &str,
        handle: u64,
        surface: &str,
        key: Option<&str>,
        component: Option<ClientUiComponent>,
        request_id: Option<&str>,
        expected_kind: Option<&str>,
    ) -> Option<ParentUiPressAction> {
        let data = lock(&self.client_ui.data);
        let mut matched = None;
        for (site, render) in &data.parent_control_renders {
            if data.parent_control_latest.get(site) != Some(&render.revision) {
                continue;
            }
            for action in &render.press_actions {
                if action.plugin != plugin
                    || action.handle != handle
                    || action.surface != surface
                    || key.is_some_and(|key| action.element.as_deref() != Some(key))
                    || component.is_some_and(|component| action.site.component != component)
                    || request_id.is_some_and(|request_id| action.site.request_id != request_id)
                    || expected_kind.is_some_and(|kind| action.kind != kind)
                {
                    continue;
                }
                if matched.is_some() {
                    // Parent handles are worker-issued and normally unique.
                    // Refuse an ambiguous match instead of choosing a site.
                    return None;
                }
                matched = Some(action.clone());
            }
        }
        matched
    }

    fn current_instance(
        &self,
        site: &ClientUiRenderSite,
        identity: &ClientUiIdentity,
    ) -> Option<ClientUiInstance> {
        let data = lock(&self.client_ui.data);
        let key = ClientUiInstanceKey {
            site: site.clone(),
            identity: identity.clone(),
        };
        let runtime_id = data.instance_by_identity.get(&key)?;
        let instance = data.instances.get(runtime_id)?;
        (instance.mounted
            && instance.worker_fault.is_none()
            && instance.revision == data.parents.get(site)?.revision
            && instance.key.identity == *identity)
            .then(|| instance.clone())
    }

    fn update_instance_props(&self, runtime_id: &str, props: Utf16JsonProjection) {
        if let Some(instance) = lock(&self.client_ui.data).instances.get_mut(runtime_id) {
            instance.props = props;
        }
    }

    fn record_client_worker_fault(&self, runtime_id: &str, phase: &str, reason: &str) -> bool {
        let mut data = lock(&self.client_ui.data);
        let Some(instance) = data.instances.get_mut(runtime_id) else {
            return false;
        };
        if instance.worker_fault.is_some() {
            return false;
        }
        instance.worker_fault = Some(ClientUiWorkerFault {
            phase: phase.to_owned(),
            reason: reason.to_owned(),
        });
        true
    }

    fn claim_client_worker_fault_dispatch(&self, runtime_id: &str) -> bool {
        let mut data = lock(&self.client_ui.data);
        let Some(instance) = data.instances.get_mut(runtime_id) else {
            return false;
        };
        if instance.worker_fault_event_dispatched {
            return false;
        }
        instance.worker_fault_event_dispatched = true;
        true
    }

    fn claim_client_worker_fault_snapshot(&self, runtime_id: &str) -> bool {
        let mut data = lock(&self.client_ui.data);
        let Some(instance) = data.instances.get_mut(runtime_id) else {
            return false;
        };
        if instance.worker_fault_snapshot_sent {
            return false;
        }
        instance.worker_fault_snapshot_sent = true;
        true
    }

    fn client_worker_fault(&self, runtime_id: &str) -> Option<ClientUiWorkerFault> {
        lock(&self.client_ui.data)
            .instances
            .get(runtime_id)?
            .worker_fault
            .clone()
    }

    async fn dispatch_plugin_scoped_ui_event<F, Fut>(
        &self,
        event: &str,
        plugin: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        press_token: Option<Value>,
        core: F,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Utf16JsonProjection) -> Fut,
        Fut: std::future::Future<Output = Result<Utf16JsonProjection, ModError>>,
    {
        let cwd = session.cwd();
        let scope = ModUtf16DispatchScope {
            plugin_scope: Some(plugin.to_owned()),
            press_token,
            ui_render_host_revision: None,
        };
        let outcome = self
            .dispatch_with_utf16_at_context_scope(
                event,
                input,
                &cwd,
                Some(session),
                scope,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                core,
                |plugin, text| async move { session.emit_mod_log(&plugin, &text).await },
                |plugin, text, timeout_ms| async move {
                    session.emit_mod_toast(&plugin, &text, timeout_ms).await
                },
                |plugin, text| async move { session.emit_mod_status(&plugin, text.as_deref()).await },
            )
            .await?;
        core_projection_from_mod(super::ModUtf16ValueProjection {
            value: outcome.result,
            strings: outcome.result_utf16_strings,
            keys: outcome.result_utf16_keys,
        })
    }

    async fn has_matching_ui_listener(
        &self,
        plugin: &str,
        event: &str,
        input: &Utf16JsonProjection,
    ) -> Result<bool, ModError> {
        let projection = super::mod_projection_from_core(input.clone())?;
        let mut envelope = json!({
            "kind":"ui.listener",
            "plugin":plugin,
            "event":event,
            "input":projection.value,
        });
        super::attach_mod_utf16_sidecars(&mut envelope, "/input", &projection.strings);
        super::attach_mod_utf16_key_sidecars(&mut envelope, "/input", &projection.keys);
        let mut request = self.request(envelope).await?;
        let reply = self.receive(&mut request).await?;
        match reply.get("kind").and_then(Value::as_str) {
            Some("ui.listener.result") => Ok(reply
                .get("matched")
                .and_then(Value::as_bool)
                .unwrap_or(false)),
            Some("error") => Err(ModError::Hook(
                reply
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("UI listener matcher failed")
                    .into(),
            )),
            _ => Err(ModError::Protocol(format!(
                "unexpected UI listener reply: {reply}"
            ))),
        }
    }

    async fn run_client_worker_operation(
        &self,
        operation: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ClientWorkerOperationError> {
        let operation =
            super::mod_projection_from_core(operation).map_err(ClientWorkerOperationError::Host)?;
        let mut envelope = json!({
            "kind":"ui.client.operation",
            "operation":operation.value,
        });
        super::attach_mod_utf16_sidecars(&mut envelope, "/operation", &operation.strings);
        super::attach_mod_utf16_key_sidecars(&mut envelope, "/operation", &operation.keys);
        let mut request = self.request(envelope).await?;
        let reply = self.receive(&mut request).await?;
        match reply.get("kind").and_then(Value::as_str) {
            Some("ui.client.result") => {
                if let Some(error) = reply.get("error") {
                    let phase = error
                        .get("phase")
                        .and_then(Value::as_str)
                        .filter(|phase| matches!(*phase, "load" | "render" | "run"))
                        .ok_or_else(|| {
                            ClientWorkerOperationError::Host(ModError::Protocol(
                                "Client worker fault has an invalid phase".into(),
                            ))
                        })?;
                    let reason = error.get("reason").and_then(Value::as_str).ok_or_else(|| {
                        ClientWorkerOperationError::Host(ModError::Protocol(
                            "Client worker fault has no reason".into(),
                        ))
                    })?;
                    return Err(ClientWorkerOperationError::Fault {
                        phase: phase.to_owned(),
                        reason: reason.to_owned(),
                    });
                }
                let value = reply.get("result").cloned().ok_or_else(|| {
                    ClientWorkerOperationError::Host(ModError::Protocol(
                        "Client operation reply has no result".into(),
                    ))
                })?;
                let strings = super::worker_utf16_sidecars(&reply, "/result")
                    .map_err(|error| ClientWorkerOperationError::Host(ModError::Protocol(error)))?;
                let keys = super::worker_utf16_key_sidecars(&reply, "/result")
                    .map_err(|error| ClientWorkerOperationError::Host(ModError::Protocol(error)))?;
                core_projection_from_mod(super::ModUtf16ValueProjection {
                    value,
                    strings,
                    keys,
                })
                .map_err(ClientWorkerOperationError::Host)
            }
            Some("error") => Err(ClientWorkerOperationError::Host(ModError::Hook(
                reply
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Client operation failed")
                    .into(),
            ))),
            _ => Err(ClientWorkerOperationError::Host(ModError::Protocol(
                format!("unexpected Client operation reply: {reply}"),
            ))),
        }
    }

    fn frame_for_worker_result(
        &self,
        runtime_id: &str,
        revision: u64,
        result: &Utf16JsonProjection,
    ) -> Option<Utf16JsonProjection> {
        if result.value.get("stale").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let frame_sequence = result
            .value
            .get("frameSequence")
            .and_then(Value::as_u64)
            .filter(|sequence| *sequence > 0 && *sequence <= MAX_JS_SAFE_INTEGER)?;
        let mut frame = Utf16JsonProjection::plain(json!({
            "runtimeId":runtime_id,
            "renderRevision":revision,
            "frameSequence":frame_sequence,
            "hasPointerListener":result.value.get("hasPointerListener")?.as_bool()?,
            "hasKeyListener":result.value.get("hasKeyListener")?.as_bool()?,
        }));
        frame
            .set_field("tree", result.subprojection("/tree").ok()?)
            .ok()?;
        Some(frame)
    }

    async fn emit_client_frame(
        &self,
        session: &dyn ModSessionContext,
        runtime_id: &str,
        revision: u64,
        frame: &Utf16JsonProjection,
    ) {
        let mut public = Utf16JsonProjection::plain(json!({
            "renderRevision":revision,
            "frameSequence":frame.value.get("frameSequence"),
            "hasPointerListener":frame.value.get("hasPointerListener"),
            "hasKeyListener":frame.value.get("hasKeyListener"),
        }));
        let Ok(tree) = frame.subprojection("/tree") else {
            return;
        };
        if public.set_field("tree", tree).is_err() {
            return;
        }
        if let Ok(text) = public.to_json_string() {
            session.emit_mod_ui_client_frame(runtime_id, &text).await;
        }
    }

    async fn mount_client_from_parent(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
        session: &dyn ModSessionContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let site = client_site_from_input(&operation.value)?;
        let plugin = operation.value["plugin"]
            .as_str()
            .expect("validated plugin")
            .to_owned();
        let key_units = operation.string_units("/client").expect("validated client");
        let key = String::from_utf16_lossy(&key_units);
        let module = operation.value["module"]
            .as_str()
            .expect("validated module")
            .to_owned();
        let identity = ClientUiIdentity::new(plugin.clone(), module.clone(), key_units.clone())
            .map_err(|error| ModError::Hook(error.to_string()))?;
        let identity_key = ClientUiInstanceKey {
            site: site.clone(),
            identity: identity.clone(),
        };
        let (trusted_props, prepared) = {
            let data = lock(&self.client_ui.data);
            let Some(parent) = data
                .parents
                .get(&site)
                .filter(|parent| parent.revision == revision)
            else {
                return Ok(Utf16JsonProjection::plain(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            let Some(props) = parent.clients.get(&identity).cloned() else {
                return Err(ModError::Hook(
                    "Client mount is not in the latest validated parent tree".into(),
                ));
            };
            let Some(prepared) = data.modules.get(&plugin).cloned() else {
                return Err(ModError::Unavailable(format!(
                    "{plugin}: Client module source is unavailable"
                )));
            };
            if !prepared
                .manifest
                .modules
                .iter()
                .any(|candidate| candidate.module == module)
            {
                return Err(ModError::Hook(format!(
                    "{plugin}: Client module is not in its prepared manifest"
                )));
            }
            (props, prepared)
        };
        let module_graph = prepared
            .worker_module_graph()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let runtime_id = self.client_ui.next_runtime_id(self.instance_id);
        let old_instance = {
            let mut data = lock(&self.client_ui.data);
            if data.latest_started.get(&site) != Some(&revision) {
                return Ok(Utf16JsonProjection::plain(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            }
            let old_runtime = data.instance_by_identity.remove(&identity_key);
            if let Some(old_runtime) = old_runtime.as_ref() {
                data.instances.remove(old_runtime)
            } else {
                None
            }
        };
        if let Some(old_instance) = old_instance {
            let mut unmount = json!({"type":"unmount","runtimeId":old_instance.runtime_id});
            if let Some(generation) = old_instance.worker_generation {
                unmount["generation"] = json!(generation);
            }
            let _ = self
                .run_client_worker_operation(Utf16JsonProjection::plain(unmount))
                .await;
        }

        let manifest_hash = prepared.manifest.hash.clone();
        let instance = ClientUiInstance {
            key: identity_key.clone(),
            runtime_id: runtime_id.clone(),
            revision,
            worker_generation: None,
            pending_events: Vec::new(),
            draining_pending_events: false,
            props: trusted_props.clone(),
            mounted: false,
            worker_fault: None,
            worker_fault_snapshot_sent: false,
            worker_fault_event_dispatched: false,
        };
        {
            let mut data = lock(&self.client_ui.data);
            data.instances.insert(runtime_id.clone(), instance);
            data.instance_by_identity
                .insert(identity_key, runtime_id.clone());
        }

        let mut child_request_id_units = site.request_id().encode_utf16().collect::<Vec<_>>();
        child_request_id_units.extend([
            0,
            u16::from(b'c'),
            u16::from(b'l'),
            u16::from(b'i'),
            u16::from(b'e'),
            u16::from(b'n'),
            u16::from(b't'),
            0,
        ]);
        child_request_id_units.extend_from_slice(&key_units);
        let mut parent = Utf16JsonProjection::plain(json!({
            "surface":"desktop",
            "component":site.component().as_str(),
            "requestId":String::from_utf16_lossy(&child_request_id_units),
        }));
        set_exact_string_field(&mut parent, "requestId", child_request_id_units)?;
        let mut client = Utf16JsonProjection::plain(json!({"key":key,"module":module}));
        set_exact_string_field(&mut client, "key", key_units)?;
        let mut worker_operation = Utf16JsonProjection::plain(json!({
            "type":"mount",
            "plugin":plugin,
            "environmentId":manifest_hash,
            "manifestHash":manifest_hash,
            "runtimeId":runtime_id,
            "parent":parent.value.clone(),
            "client":client.value.clone(),
            "moduleGraph":module_graph,
            "props":trusted_props.value.clone(),
            "columns":operation.value["columns"],
            "rows":operation.value["rows"],
        }));
        worker_operation
            .set_field("parent", parent)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        worker_operation
            .set_field("client", client)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        worker_operation
            .set_field("props", trusted_props.clone())
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let result = match self.run_client_worker_operation(worker_operation).await {
            Ok(result) => result,
            Err(error) => {
                let instance_key = ClientUiInstanceKey {
                    site: site.clone(),
                    identity: identity.clone(),
                };
                {
                    let mut data = lock(&self.client_ui.data);
                    remove_instance_if_current(&mut data, &runtime_id, &instance_key);
                }
                match error {
                    ClientWorkerOperationError::Fault { phase, reason } => {
                        self.dispatch_mount_client_fault(
                            &site, &identity, &phase, &reason, session,
                        )
                        .await;
                        return Ok(Utf16JsonProjection::plain(worker_fault_response(
                            Some(&runtime_id),
                            revision,
                            identity.plugin(),
                            identity.module(),
                            &phase,
                            &reason,
                        )));
                    }
                    error @ ClientWorkerOperationError::Host(_) => return Err(error.into()),
                }
            }
        };
        let worker_generation = result
            .value
            .get("generation")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ModError::Protocol("Client mount result has no worker generation".into())
            })?;
        let still_current = {
            let mut data = lock(&self.client_ui.data);
            if data.latest_started.get(&site) != Some(&revision)
                || data
                    .parents
                    .get(&site)
                    .is_none_or(|parent| parent.revision != revision)
            {
                if let Some(instance) = data.instances.get(&runtime_id) {
                    let key = instance.key.clone();
                    remove_instance_if_current(&mut data, &runtime_id, &key);
                }
                false
            } else if let Some(instance) = data.instances.get_mut(&runtime_id) {
                instance.worker_generation = Some(worker_generation);
                instance.mounted = true;
                true
            } else {
                false
            }
        };
        if !still_current {
            let _ = self
                .run_client_worker_operation(Utf16JsonProjection::plain(
                    json!({"type":"unmount","runtimeId":runtime_id,"generation":worker_generation}),
                ))
                .await;
            return Ok(Utf16JsonProjection::plain(
                json!({"handled":false,"renderRevision":revision}),
            ));
        }
        self.frame_for_worker_result(&runtime_id, revision, &result)
            .ok_or_else(|| ModError::Protocol("Client mount result has no render frame".into()))
    }

    async fn dispatch_runtime_operation(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
    ) -> Result<Utf16JsonProjection, ModError> {
        let kind = operation.value["type"].as_str().expect("validated type");
        let runtime_id = operation.value["runtimeId"]
            .as_str()
            .expect("validated runtime id");
        let instance = {
            let mut data = lock(&self.client_ui.data);
            let Some(mut instance) = data.instances.get(runtime_id).cloned() else {
                return Ok(Utf16JsonProjection::plain(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            };
            let parent_state = data.parents.get(&instance.key.site).map(|parent| {
                (
                    parent.revision,
                    parent.clients.contains_key(&instance.key.identity),
                    parent.clients.get(&instance.key.identity).cloned(),
                )
            });
            if kind != "unmount" {
                let Some((parent_revision, true, Some(parent_props))) = parent_state else {
                    return Ok(Utf16JsonProjection::plain(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                };
                if parent_revision != revision {
                    return Ok(Utf16JsonProjection::plain(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
                if !instance.mounted || instance.worker_fault.is_some() {
                    return Ok(Utf16JsonProjection::plain(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
                instance.revision = revision;
                instance.props = parent_props;
                if let Some(stored) = data.instances.get_mut(runtime_id) {
                    stored.revision = revision;
                    stored.props = instance.props.clone();
                }
            } else {
                let same_identity_is_current =
                    parent_state.is_some_and(|(parent_revision, present, _)| {
                        present && (parent_revision != revision || instance.revision != revision)
                    });
                if same_identity_is_current {
                    return Ok(Utf16JsonProjection::plain(
                        json!({"handled":false,"renderRevision":revision}),
                    ));
                }
            }
            instance
        };
        if kind == "setProps" {
            let submitted = operation
                .subprojection("/props")
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            let data = lock(&self.client_ui.data);
            if data
                .parents
                .get(&instance.key.site)
                .and_then(|parent| parent.clients.get(&instance.key.identity))
                != Some(&submitted)
            {
                return Err(ModError::Hook(
                    "Client props must come from the latest validated parent tree".into(),
                ));
            }
        }
        let mut worker_operation = Utf16JsonProjection::plain(json!({
            "type":kind,
            "runtimeId":runtime_id,
        }));
        if let Some(generation) = instance.worker_generation {
            worker_operation.value["generation"] = json!(generation);
        }
        match kind {
            "setProps" => worker_operation
                .set_field(
                    "props",
                    operation
                        .subprojection("/props")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?,
            "resize" => {
                worker_operation.value["columns"] = operation.value["columns"].clone();
                worker_operation.value["rows"] = operation.value["rows"].clone();
            }
            "pointer" | "key" => worker_operation
                .set_field(
                    "payload",
                    operation
                        .subprojection("/event")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?,
            "runHeld" => {
                worker_operation.value["handle"] = operation.value["handle"].clone();
                if operation.value.get("event").is_some() {
                    worker_operation
                        .set_field(
                            "payload",
                            operation
                                .subprojection("/event")
                                .map_err(|error| ModError::Protocol(error.to_string()))?,
                        )
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                }
            }
            "render" | "unmount" => {}
            _ => unreachable!("operation was validated"),
        }
        let result = match self.run_client_worker_operation(worker_operation).await {
            Ok(result) => result,
            Err(ClientWorkerOperationError::Fault { phase, reason }) if kind != "unmount" => {
                self.record_client_worker_fault(runtime_id, &phase, &reason);
                return Ok(Utf16JsonProjection::plain(worker_fault_response(
                    Some(runtime_id),
                    revision,
                    instance.key.identity.plugin(),
                    instance.key.identity.module(),
                    &phase,
                    &reason,
                )));
            }
            Err(error) => return Err(error.into()),
        };
        if kind == "unmount" {
            let handled = result.value.as_str() == Some(runtime_id);
            let mut data = lock(&self.client_ui.data);
            if data
                .instance_by_identity
                .get(&instance.key)
                .map(String::as_str)
                == Some(runtime_id)
            {
                data.instance_by_identity.remove(&instance.key);
            }
            return Ok(Utf16JsonProjection::plain(
                json!({"handled":handled,"renderRevision":revision}),
            ));
        }
        if result.value.get("stale").and_then(Value::as_bool) == Some(true) {
            return Ok(Utf16JsonProjection::plain(
                json!({"handled":false,"renderRevision":revision}),
            ));
        }
        if kind == "pointer" || kind == "key" || kind == "runHeld" {
            let rendered = match self
                .run_client_worker_operation(Utf16JsonProjection::plain(json!({
                    "type":"render",
                    "runtimeId":runtime_id,
                    "generation":instance.worker_generation,
                })))
                .await
            {
                Ok(rendered) => rendered,
                Err(ClientWorkerOperationError::Fault { phase, reason }) => {
                    return Ok(Utf16JsonProjection::plain(worker_fault_response(
                        Some(runtime_id),
                        revision,
                        instance.key.identity.plugin(),
                        instance.key.identity.module(),
                        &phase,
                        &reason,
                    )));
                }
                Err(error) => return Err(error.into()),
            };
            if rendered.value.get("stale").and_then(Value::as_bool) == Some(true) {
                return Ok(Utf16JsonProjection::plain(
                    json!({"handled":false,"renderRevision":revision}),
                ));
            }
            return self
                .frame_for_worker_result(runtime_id, revision, &rendered)
                .ok_or_else(|| {
                    ModError::Protocol("Client operation produced no render frame".into())
                });
        }
        if kind == "setProps" {
            self.update_instance_props(
                runtime_id,
                operation
                    .subprojection("/props")
                    .map_err(|error| ModError::Protocol(error.to_string()))?,
            );
        }
        self.frame_for_worker_result(runtime_id, revision, &result)
            .ok_or_else(|| ModError::Protocol("Client operation produced no render frame".into()))
    }

    async fn commit_client_draw(
        &self,
        operation: &Utf16JsonProjection,
        revision: u64,
        session: &dyn ModSessionContext,
    ) -> Result<Value, ModError> {
        let site = client_site_from_input(&operation.value)?;
        let raw_clients = operation.value["clients"]
            .as_array()
            .expect("validated clients");
        let mut identities = Vec::with_capacity(raw_clients.len());
        let mut seen = HashSet::new();
        for (index, raw) in raw_clients.iter().enumerate() {
            let object = raw.as_object().expect("validated client identity");
            let key = operation
                .string_units(&format!("/clients/{index}/key"))
                .expect("validated key");
            let identity = ClientUiIdentity::new(
                object["plugin"].as_str().expect("plugin"),
                object["module"].as_str().expect("module"),
                key,
            )
            .map_err(|error| ModError::Hook(error.to_string()))?;
            if !seen.insert(identity.clone()) {
                return Err(ModError::Hook(
                    "draw_commit contains a duplicate Client identity".into(),
                ));
            }
            identities.push(identity);
        }
        let identities_set = identities.iter().cloned().collect::<HashSet<_>>();
        let (handled, stale_runtimes, pending_event_runtimes) = {
            let mut data = lock(&self.client_ui.data);
            let Some(parent) = data
                .parents
                .get(&site)
                .filter(|parent| parent.revision == revision)
            else {
                return Ok(json!({"handled":false,"renderRevision":revision}));
            };
            if identities
                .iter()
                .any(|identity| !parent.clients.contains_key(identity))
            {
                return Err(ModError::Hook(
                    "draw_commit contains a Client not present in the latest validated parent tree"
                        .into(),
                ));
            }
            let Some(token) = parent.token.as_ref() else {
                return Ok(json!({"handled":false,"renderRevision":revision}));
            };
            let generation = token.generation();
            let handled = self
                .client_ui
                .fault_registry
                .finish_render(token, identities);
            if handled {
                if let Some(parent) = data.parents.get_mut(&site) {
                    parent.committed_generation = Some(generation);
                }
            }
            let stale_runtimes = data
                .instances
                .iter()
                .filter(|(_, instance)| {
                    instance.key.site == site && !identities_set.contains(&instance.key.identity)
                })
                .map(|(runtime_id, instance)| (runtime_id.clone(), instance.worker_generation))
                .collect::<Vec<_>>();
            for (runtime_id, _) in &stale_runtimes {
                if let Some(instance) = data.instances.remove(runtime_id) {
                    if data
                        .instance_by_identity
                        .get(&instance.key)
                        .map(String::as_str)
                        == Some(runtime_id)
                    {
                        data.instance_by_identity.remove(&instance.key);
                    }
                }
            }
            let pending_event_runtimes = if handled {
                data.instances
                    .iter_mut()
                    .filter_map(|(runtime_id, instance)| {
                        if instance.key.site == site
                            && instance.revision == revision
                            && identities_set.contains(&instance.key.identity)
                            && instance.mounted
                            && !instance.pending_events.is_empty()
                        {
                            instance.draining_pending_events = true;
                            Some(runtime_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            (handled, stale_runtimes, pending_event_runtimes)
        };
        for (runtime_id, generation) in stale_runtimes {
            let mut unmount = json!({"type":"unmount","runtimeId":runtime_id});
            if let Some(generation) = generation {
                unmount["generation"] = json!(generation);
            }
            let _ = self
                .run_client_worker_operation(Utf16JsonProjection::plain(unmount))
                .await;
        }
        for runtime_id in pending_event_runtimes {
            self.drain_client_ui_pending_events(&runtime_id, session)
                .await?;
        }
        Ok(json!({"handled":handled,"renderRevision":revision}))
    }

    async fn unmount_client_site(
        &self,
        operation: &Value,
        revision: u64,
    ) -> Result<Value, ModError> {
        let site = client_site_from_input(operation)?;
        let instances = {
            let mut data = lock(&self.client_ui.data);
            let Some(parent) = data
                .parents
                .get(&site)
                .filter(|parent| parent.revision == revision)
            else {
                return Ok(json!({"handled":false,"renderRevision":revision}));
            };
            if let Some(generation) = parent.committed_generation {
                self.client_ui
                    .fault_registry
                    .unmount_site(&site, generation);
            }
            let runtimes = data
                .instances
                .iter()
                .filter(|(_, instance)| instance.key.site == site)
                .map(|(runtime_id, _)| runtime_id.clone())
                .collect::<Vec<_>>();
            for runtime_id in &runtimes {
                if let Some(instance) = data.instances.remove(runtime_id) {
                    data.instance_by_identity.remove(&instance.key);
                }
            }
            data.parents.remove(&site);
            if data.latest_started.get(&site) == Some(&revision) {
                data.latest_started.remove(&site);
            }
            data.pending_tokens.remove(&(site.clone(), revision));
            let control_site = ParentControlSite {
                surface: "desktop".into(),
                component: site.component(),
                request_id: site.request_id().to_owned(),
            };
            if data.parent_control_latest.get(&control_site) == Some(&revision) {
                data.parent_control_latest.remove(&control_site);
                data.parent_control_renders.remove(&control_site);
            } else if data
                .parent_control_renders
                .get(&control_site)
                .is_some_and(|render| render.revision == revision)
            {
                data.parent_control_renders.remove(&control_site);
            }
            runtimes
        };
        self.remove_client_ui_render_state("desktop", site.component().as_str(), site.request_id());
        for runtime_id in instances {
            let _ = self
                .run_client_worker_operation(Utf16JsonProjection::plain(
                    json!({"type":"unmount","runtimeId":runtime_id}),
                ))
                .await;
        }
        Ok(json!({"handled":true,"renderRevision":revision}))
    }

    async fn drain_client_ui_pending_events(
        &self,
        runtime_id: &str,
        session: &dyn ModSessionContext,
    ) -> Result<(), ModError> {
        loop {
            let event = {
                let mut data = lock(&self.client_ui.data);
                let Some(instance) = data.instances.get_mut(runtime_id) else {
                    return Ok(());
                };
                if instance.pending_events.is_empty() {
                    instance.draining_pending_events = false;
                    return Ok(());
                }
                instance.pending_events.remove(0)
            };
            self.handle_client_ui_background_event_inner(&event, session, true)
                .await?;
        }
    }

    pub(super) async fn handle_client_ui_background_event(
        &self,
        event: &Value,
        session: &dyn ModSessionContext,
    ) -> Result<(), ModError> {
        self.handle_client_ui_background_event_inner(event, session, false)
            .await
    }

    async fn handle_client_ui_background_event_inner(
        &self,
        event: &Value,
        session: &dyn ModSessionContext,
        from_pending_queue: bool,
    ) -> Result<(), ModError> {
        if event.get("kind").and_then(Value::as_str) != Some("ui.client.event") {
            return Ok(());
        }
        let exact_event = core_projection_from_mod(
            super::worker_utf16_projection(event, "").map_err(ModError::Protocol)?,
        )?;
        let runtime_id = event
            .get("runtimeId")
            .and_then(Value::as_str)
            .ok_or_else(|| ModError::Protocol("Client event has no runtime id".into()))?;
        let action = event.get("action").and_then(Value::as_str).unwrap_or("");
        let instance = {
            let mut data = lock(&self.client_ui.data);
            let Some(instance) = data.instances.get(runtime_id).cloned() else {
                return Ok(());
            };
            if event.get("plugin").and_then(Value::as_str) != Some(instance.key.identity.plugin())
                || exact_event.string_units("/element").as_deref()
                    != Some(instance.key.identity.key_units())
                || event.get("module").and_then(Value::as_str)
                    != Some(instance.key.identity.module())
            {
                return Ok(());
            }
            if let Some(generation) = instance.worker_generation {
                if event.get("generation").and_then(Value::as_u64) != Some(generation) {
                    return Ok(());
                }
            } else {
                if let Some(instance) = data.instances.get_mut(runtime_id) {
                    instance.pending_events.push(event.clone());
                }
                return Ok(());
            }
            if !data.parents.get(&instance.key.site).is_some_and(|parent| {
                parent.revision == instance.revision
                    && parent.clients.contains_key(&instance.key.identity)
            }) {
                return Ok(());
            }
            let drawn = self.client_ui.fault_registry.is_drawn(
                &crate::mod_ui_fault::ClientUiFaultAddress::new(
                    instance.key.site.clone(),
                    instance.key.identity.clone(),
                ),
            );
            if from_pending_queue && !drawn {
                return Ok(());
            }
            if !drawn || (!from_pending_queue && instance.draining_pending_events) {
                if let Some(instance) = data.instances.get_mut(runtime_id) {
                    instance.pending_events.push(event.clone());
                }
                return Ok(());
            }
            instance
        };
        if instance.worker_fault.is_some() && action != "fault" {
            return Ok(());
        }
        match action {
            "schedule" => {
                let result = self
                    .run_client_worker_operation(Utf16JsonProjection::plain(json!({
                        "type":"render","runtimeId":runtime_id,
                        "generation":instance.worker_generation,
                    })))
                    .await?;
                if let Some(frame) =
                    self.frame_for_worker_result(runtime_id, instance.revision, &result)
                {
                    if self
                        .current_instance(&instance.key.site, &instance.key.identity)
                        .is_some()
                    {
                        self.emit_client_frame(session, runtime_id, instance.revision, &frame)
                            .await;
                    }
                }
            }
            "post" => {
                let text = event.get("json").and_then(Value::as_str).ok_or_else(|| {
                    ModError::Protocol("Client post event has invalid JSON".into())
                })?;
                let data = Utf16JsonProjection::parse(text)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let mut request = Utf16JsonProjection::plain(json!({
                    "subtype":"ui_message",
                    "plugin":instance.key.identity.plugin(),
                    "component":instance.key.site.component().as_str(),
                    "instance_id":instance.key.site.request_id(),
                    "client":String::from_utf16_lossy(instance.key.identity.key_units()),
                    "module":instance.key.identity.module(),
                    "data":data.value.clone(),
                }));
                set_exact_string_field(
                    &mut request,
                    "client",
                    instance.key.identity.key_units().to_vec(),
                )?;
                request
                    .set_field("data", data)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let _ = self.dispatch_client_message(&request, session).await?;
            }
            "fault" => {
                let event_phase = event
                    .get("phase")
                    .and_then(Value::as_str)
                    .filter(|phase| matches!(*phase, "load" | "render" | "run"))
                    .unwrap_or("run");
                let event_reason = event
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Client surface failed");
                self.record_client_worker_fault(runtime_id, event_phase, event_reason);
                let Some(fault) = self.client_worker_fault(runtime_id) else {
                    return Ok(());
                };
                let phase = fault.phase.as_str();
                let reason = fault.reason.as_str();
                if self.claim_client_worker_fault_snapshot(runtime_id) {
                    let normalized_reason = crate::mod_ui_fault::normalize_client_fault_reason(
                        instance.key.identity.plugin(),
                        instance.key.identity.module(),
                        reason,
                    );
                    let failure = json!({
                        "renderRevision":instance.revision,
                        "fault":{
                            "phase":phase,
                            "reason":normalized_reason,
                            "source":"worker",
                        },
                    });
                    // This is a Host-local leaf failure snapshot for async
                    // worker faults (timers/listeners), not a successful tree
                    // frame and not a Native control response.
                    session
                        .emit_mod_ui_client_frame(runtime_id, &failure.to_string())
                        .await;
                }
                if !self.claim_client_worker_fault_dispatch(runtime_id) {
                    return Ok(());
                }
                let mut request = Utf16JsonProjection::plain(json!({
                    "subtype":"ui_client_fault",
                    "plugin":instance.key.identity.plugin(),
                    "component":instance.key.site.component().as_str(),
                    "instance_id":instance.key.site.request_id(),
                    "client":String::from_utf16_lossy(instance.key.identity.key_units()),
                    "module":instance.key.identity.module(),
                    "phase":phase,
                    "reason":reason,
                }));
                set_exact_string_field(
                    &mut request,
                    "client",
                    instance.key.identity.key_units().to_vec(),
                )?;
                let _ = self.dispatch_client_fault(&request, session).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

struct ClientFaultHostAdapter<'a> {
    host: &'a ModHost,
    session: &'a dyn ModSessionContext,
    session_id: String,
}

#[async_trait::async_trait]
impl ClientUiFaultHost for ClientFaultHostAdapter<'_> {
    type Error = ModError;

    async fn dispatch_plugin_ui_fault(
        &self,
        plugin: &str,
        event: &UiFaultHookEvent,
    ) -> Result<(), Self::Error> {
        self.host
            .dispatch_plugin_scoped_ui_event(
                "ui.fault",
                plugin,
                event.to_projection(),
                self.session,
                None,
                |_| async { Ok(Utf16JsonProjection::plain(json!({"handled":true}))) },
            )
            .await
            .map(|_| ())
    }

    async fn has_matching_ui_fault_listener(&self, plugin: &str, event: &UiFaultHookEvent) -> bool {
        self.host
            .has_matching_ui_listener(plugin, "ui.fault", &event.to_projection())
            .await
            // Native treats a matcher exception as a match; fail open only
            // for the invalidation calculation after the hook has run.
            .unwrap_or(true)
    }

    fn current_ui_render_version(&self, site: &ClientUiRenderSite) -> u64 {
        self.host.current_client_ui_render_state_version(
            "desktop",
            site.component().as_str(),
            site.request_id(),
        )
    }

    async fn invalidate_ui_render_site(&self, site: &ClientUiRenderSite) {
        let sequence = self
            .host
            .client_ui
            .next_invalidation_id
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let uuid = format!("mod-ui-{}-{sequence}", self.host.instance_id);
        let instances = json!([{
            "surface":"desktop",
            "component":site.component().as_str(),
            "instance_id":site.request_id(),
        }]);
        self.session
            .emit_mod_ui_invalidate(Some(&instances.to_string()), &uuid, &self.session_id)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct UiLifecycleSession {
        root: std::path::PathBuf,
        frames: StdMutex<Vec<(String, Value)>>,
        invalidations: StdMutex<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for UiLifecycleSession {
        fn cwd(&self) -> std::path::PathBuf {
            self.root.clone()
        }

        fn root(&self) -> std::path::PathBuf {
            self.root.clone()
        }

        async fn model(&self) -> String {
            "test-model".into()
        }

        async fn id(&self) -> String {
            "ui-lifecycle-session".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn emit_mod_ui_client_frame(&self, runtime_id: &str, frame_json: &str) {
            if let Ok(frame) = serde_json::from_str(frame_json) {
                lock(&self.frames).push((runtime_id.to_owned(), frame));
            }
        }

        async fn emit_mod_ui_invalidate(
            &self,
            instances_json: Option<&str>,
            uuid: &str,
            session_id: &str,
        ) {
            let instances = instances_json
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or(Value::Null);
            lock(&self.invalidations).push(json!({
                "instances":instances,
                "uuid":uuid,
                "session_id":session_id,
            }));
        }
    }

    fn tree_contains_text(tree: &Value, expected: &str) -> bool {
        if tree.as_str() == Some(expected) {
            return true;
        }
        tree.get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| {
                children
                    .iter()
                    .any(|child| tree_contains_text(child, expected))
            })
    }

    fn tree_first_held(tree: &Value) -> Option<u64> {
        tree.get("held").and_then(Value::as_u64).or_else(|| {
            tree.get("children")
                .and_then(Value::as_array)
                .and_then(|children| children.iter().find_map(tree_first_held))
        })
    }

    async fn wait_for_client_frame(
        session: &UiLifecycleSession,
        runtime_id: &str,
        stage: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frame = {
                    let frames = lock(&session.frames);
                    frames
                        .iter()
                        .rev()
                        .find(|(seen_runtime, frame)| {
                            seen_runtime == runtime_id && predicate(frame)
                        })
                        .map(|(_, frame)| frame.clone())
                };
                if let Some(frame) = frame {
                    return frame;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let captured = lock(&session.frames)
                .iter()
                .filter(|(seen_runtime, _)| seen_runtime == runtime_id)
                .map(|(_, frame)| frame.clone())
                .collect::<Vec<_>>();
            panic!("Client UI frame did not arrive during {stage}; captured frames: {captured:?}")
        })
    }

    #[tokio::test]
    async fn client_state_token_tracks_only_committed_native_render_dependencies() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(
            &child,
            r#"
              import { Box, Text, h } from 'claude:surface-runtime';
              export function Card(props: { label: string }) {
                return h(Box, { children: [h(Text, { children: props.label })] });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, async ($, event) => {
                  const current = await $.state.get({ plugin: 'stateful', key: 'screen' });
                  const { Client, h } = $.ui.resolve(event);
                  return h(Client, {
                    key: 'card', module: './card.tsx',
                    props: { label: current.value?.label ?? 'initial' },
                  });
                });
                on('ui.render', { surface: 'terminal', component: 'AbovePrompt' }, async ($, event) => {
                  const current = await $.state.get({ plugin: 'stateful', key: 'screen' });
                  const { Text } = $.ui.resolve(event);
                  return Text({ children: [current.value?.label ?? 'initial'] });
                });
                on('turn.start', async ($, event, next) => {
                  await $.state.set({ plugin: 'stateful', key: 'screen' }, { label: 'updated' });
                  return next(event);
                });
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "stateful",
            "stateful@user",
            directory.path(),
            &entry,
            json!({}),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let render = || {
            json!({
                "subtype":"ui_render",
                "surface":"desktop",
                "component":"Pane",
                "instance_id":"pane-state",
                "props":{},
                "viewport":{"columns":80,"rows":24},
            })
        };
        let initial = host
            .dispatch_client_ui_control(Utf16JsonProjection::plain(render()), session.as_ref())
            .await
            .unwrap();
        let initial_revision = initial
            .render_revision
            .expect("successful ui.render carries a host revision");
        let initial_site = client_site_from_input(&render()).unwrap();
        assert_eq!(
            initial.response.value["tree"]["type"], "Client",
            "{:#?}",
            initial.response
        );
        assert!(
            initial.response.value["client_modules"]["stateful"].is_string(),
            "loaded Client manifest is missing: {:#?}",
            initial.response
        );
        {
            let data = lock(&host.client_ui.data);
            assert_eq!(
                data.latest_started.get(&initial_site),
                Some(&initial_revision)
            );
            assert!(data.parents.contains_key(&initial_site));
        }
        let initial_token = initial.client_state_token.as_deref().unwrap_or_else(|| {
            panic!(
                "validated Client tree carries its state token; response={:#?}",
                initial.response
            )
        });
        assert!(initial_token.bytes().all(|byte| byte.is_ascii_digit()));
        let module = initial.response.value["tree"]["props"]["module"]
            .as_str()
            .unwrap();
        assert_eq!(module, "card.tsx");
        assert_eq!(
            initial.response.value["tree"]["props"]["props"]["label"],
            "initial"
        );
        let committed = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":initial_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);

        let unchanged = host
            .dispatch_client_ui_control(Utf16JsonProjection::plain(render()), session.as_ref())
            .await
            .unwrap();
        assert_eq!(unchanged.client_state_token.as_deref(), Some(initial_token));
        let unchanged_revision = unchanged.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":unchanged_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);

        let mut wider_viewport_render = render();
        wider_viewport_render["viewport"]["columns"] = json!(120);
        let wider_viewport = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(wider_viewport_render),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            wider_viewport.client_state_token.as_deref(),
            Some(initial_token),
            "viewport is a separate invalidation input and is excluded from Rh's render signature"
        );
        let wider_viewport_revision = wider_viewport.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":wider_viewport_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);

        let fault = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(json!({
                    "subtype":"ui_client_fault",
                    "plugin":"stateful",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "client":"card",
                    "module":"card.tsx",
                    "phase":"run",
                    "reason":"test fault",
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(fault.response.value["handled"], true);
        assert_eq!(fault.client_state_token, None);

        let after_fault = host
            .dispatch_client_ui_control(Utf16JsonProjection::plain(render()), session.as_ref())
            .await
            .unwrap();
        assert_eq!(
            after_fault.client_state_token.as_deref(),
            Some(initial_token)
        );
        let after_fault_revision = after_fault.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":after_fault_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);

        let above_prompt = host
            .dispatch_ui_render_with_state_scope_at_session(
                Utf16JsonProjection::plain(json!({
                    "surface":"terminal",
                    "component":"AbovePrompt",
                    "requestId":"above-prompt-state",
                    "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,
                        "bodyColumns":80,"scroll":{"offset":0,"bodyRows":2},"view":{}},
                    "viewport":{"columns":80,"rows":24,"isFullscreen":false},
                })),
                1,
                session.as_ref(),
                |event| async move { Ok(event) },
                |_: String, _: String| async {},
                |_: String, _: String, _: u64| async {},
                |_: String, _: Option<String>| async {},
            )
            .await
            .unwrap();
        assert_eq!(above_prompt.result["children"][0], "initial");

        // Seed the two Native pace folds as pending with a fake monotonic
        // clock. The following `state.set` still traverses the real Worker/API
        // path and must flush both pending folds before that API reply.
        let state_key = crate::mods::ModStateKey::test("stateful", "screen");
        let now_ms = host.ui_pacing_now_ms();
        {
            let mut state = lock(&host.state);
            state.ui_render_state_folds.0.last_at_ms = Some(now_ms.saturating_add(1_000));
            state.ui_render_state_folds.1.last_at_ms = Some(now_ms.saturating_add(1_000));
            let pending = state.schedule_ui_render_write(&state_key, now_ms);
            assert!(pending.sites.is_empty());
            assert_eq!(pending.timers.len(), 2);
            assert_eq!(
                pending
                    .timers
                    .iter()
                    .map(|timer| timer.deadline_ms - now_ms)
                    .collect::<HashSet<_>>(),
                HashSet::from([34, 100])
            );
        }
        let invalidation_start = lock(&session_impl.invalidations).len();
        host.dispatch_with_log_at_session(
            "turn.start",
            json!({"turnId":"state-change"}),
            session.as_ref(),
            |event| async move { Ok(event) },
            |_, _| async {},
        )
        .await
        .unwrap();
        let invalidations = lock(&session_impl.invalidations);
        let state_invalidation = invalidations[invalidation_start..]
            .iter()
            .find(|entry| {
                entry["instances"].as_array().is_some_and(|instances| {
                    instances.iter().any(|instance| {
                        instance["component"] == "Pane" && instance["instance_id"] == "pane-state"
                    }) && instances.iter().any(|instance| {
                        instance["component"] == "AbovePrompt"
                            && instance["instance_id"] == "above-prompt-state"
                    })
                })
            })
            .expect("state.set flushes both pending pace folds before replying");
        let pane = state_invalidation["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|instance| instance["component"] == "Pane")
            .unwrap();
        assert_eq!(pane["surface"], "desktop");
        drop(invalidations);

        let updated = host
            .dispatch_client_ui_control(Utf16JsonProjection::plain(render()), session.as_ref())
            .await
            .unwrap();
        let updated_token = updated.client_state_token.as_deref().unwrap();
        assert_ne!(updated_token, initial_token);
        assert_eq!(
            updated.response.value["tree"]["props"]["props"]["label"],
            "updated"
        );
        let updated_revision = updated.render_revision.unwrap();
        let committed = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-state",
                    "render_revision":updated_revision,
                    "clients":[{"plugin":"stateful","key":"card","module":"card.tsx"}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);
    }

    #[tokio::test]
    async fn unmatched_plugin_set_change_advances_each_failed_client_site_token() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let owner_entry = directory.path().join("owner.tsx");
        let unrelated_entry = directory.path().join("unrelated.js");
        std::fs::write(
            &child,
            r#"
              import { Box, Text, h } from 'claude:surface-runtime';
              export function Card(props: { label: string }) {
                return h(Box, { children: [h(Text, { children: props.label })] });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &owner_entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, async ($, event) => {
                  const { Client, h } = $.ui.resolve(event);
                  return h(Client, {
                    key: 'card', module: './card.tsx', props: { label: event.requestId },
                  });
                });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &unrelated_entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'AbovePrompt' }, async () => null);
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));
        let prepared = host
            .prepare_module(directory.path(), &owner_entry)
            .await
            .unwrap();
        host.load_with_tier_order_storage_prepared(
            "owner",
            "owner@user",
            directory.path(),
            &owner_entry,
            json!({}),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        async fn render_and_commit(
            host: &ModHost,
            session: &dyn ModSessionContext,
            request_id: &str,
        ) -> String {
            let response = host
                .dispatch_client_ui_control(
                    Utf16JsonProjection::plain(json!({
                        "subtype":"ui_render",
                        "surface":"desktop",
                        "component":"Pane",
                        "instance_id":request_id,
                        "props":{},
                        "viewport":{"columns":80,"rows":24},
                    })),
                    session,
                )
                .await
                .unwrap();
            assert_eq!(response.response.value["tree"]["type"], "Client");
            let token = response
                .client_state_token
                .clone()
                .expect("validated Client tree carries a state token");
            let revision = response.render_revision.unwrap();
            let committed = host
                .dispatch_client_ui_operation(
                    Utf16JsonProjection::plain(json!({
                        "type":"draw_commit",
                        "surface":"desktop",
                        "component":"Pane",
                        "instance_id":request_id,
                        "render_revision":revision,
                        "clients":[{"plugin":"owner","key":"card","module":"card.tsx"}],
                    })),
                    session,
                )
                .await
                .unwrap();
            assert_eq!(committed.value["handled"], true);
            token
        }

        let first_site = "pane-global-a";
        let second_site = "pane-global-b";
        let first_token = render_and_commit(&host, session.as_ref(), first_site).await;
        let second_token = render_and_commit(&host, session.as_ref(), second_site).await;
        assert_eq!(
            render_and_commit(&host, session.as_ref(), first_site).await,
            first_token,
            "an unchanged render must preserve its token"
        );
        assert_eq!(
            render_and_commit(&host, session.as_ref(), second_site).await,
            second_token,
            "a second site's unchanged render must preserve its token"
        );

        let fault = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(json!({
                    "subtype":"ui_client_fault",
                    "plugin":"owner",
                    "component":"Pane",
                    "instance_id":first_site,
                    "client":"card",
                    "module":"card.tsx",
                    "phase":"run",
                    "reason":"test fault",
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(fault.response.value["handled"], true);
        assert_eq!(
            render_and_commit(&host, session.as_ref(), first_site).await,
            first_token,
            "same-token rerender must keep the existing Client fault identity"
        );
        assert_eq!(
            render_and_commit(&host, session.as_ref(), second_site).await,
            second_token,
            "a fault at one site must not change another site's token"
        );

        let invalidation_start = lock(&session_impl.invalidations).len();
        host.load("unrelated", directory.path(), &unrelated_entry, json!({}))
            .await
            .unwrap();
        let lifecycle_invalidations = lock(&session_impl.invalidations)
            .iter()
            .skip(invalidation_start)
            .cloned()
            .collect::<Vec<_>>();
        let invalidated_sites = lifecycle_invalidations
            .iter()
            .flat_map(|entry| entry["instances"].as_array().into_iter().flatten())
            .map(|site| site["instance_id"].as_str().unwrap_or_default().to_owned())
            .collect::<HashSet<_>>();
        assert!(invalidated_sites.contains(first_site));
        assert!(invalidated_sites.contains(second_site));

        let first_after_global = render_and_commit(&host, session.as_ref(), first_site).await;
        let second_after_global = render_and_commit(&host, session.as_ref(), second_site).await;
        assert_ne!(first_after_global, first_token);
        assert_ne!(second_after_global, second_token);
        assert_eq!(
            render_and_commit(&host, session.as_ref(), first_site).await,
            first_after_global,
            "the new global generation remains stable without another change"
        );
    }

    #[test]
    fn parent_held_control_normalization_defaults_surface_and_strips_unknowns() {
        let normalized = normalize_control_request(&Utf16JsonProjection::plain(json!({
            "subtype":"ui_input",
            "plugin":"review",
            "handle":7,
            "kind":"change",
            "value":"hello",
            "key":"search",
            "component":"Pane",
            "instance_id":"pane-1",
            "client_id":"desktop-1",
            "unrecognized":"discarded"
        })))
        .unwrap();

        assert_eq!(
            normalized.value,
            json!({
                "subtype":"ui_input",
                "plugin":"review",
                "handle":7,
                "kind":"change",
                "value":"hello",
                "key":"search",
                "component":"Pane",
                "instance_id":"pane-1",
                "client_id":"desktop-1",
                "surface":"desktop"
            })
        );
    }

    #[test]
    fn parent_control_normalization_strips_nested_unknowns_but_keeps_user_props() {
        let normalized = normalize_control_request(&Utf16JsonProjection::plain(json!({
            "subtype":"ui_render",
            "surface":"desktop",
            "component":"Pane",
            "instance_id":"pane-1",
            "props":{"custom":{"kept":true}},
            "viewport":{"columns":80,"rows":24,"ignored":true},
            "bench":{"seq":2,"t0":3.5,"ignored":true},
            "ignored":true
        })))
        .unwrap();

        assert_eq!(
            normalized.value,
            json!({
                "subtype":"ui_render",
                "surface":"desktop",
                "component":"Pane",
                "instance_id":"pane-1",
                "props":{"custom":{"kept":true}},
                "viewport":{"columns":80,"rows":24},
                "bench":{"seq":2,"t0":3.5}
            })
        );
    }

    #[test]
    fn ui_message_projection_preserves_lone_surrogate_client_and_nested_data() {
        let raw = r#"{"subtype":"ui_message","plugin":"review","component":"Pane","instance_id":"pane-1","client":"\ud800","module":"surface.js","data":{"\ud800":"\udfff"}}"#;
        let parsed = Utf16JsonProjection::parse(raw).unwrap();
        let normalized = normalize_control_request(&parsed).unwrap();
        validate_control_request(&normalized).unwrap();

        assert_eq!(normalized.string_units("/client"), Some(vec![0xd800]));
        let data = normalized.subprojection("/data").unwrap();
        let key = data.value.as_object().unwrap().keys().next().unwrap();
        assert_eq!(data.key_units("", key), vec![0xd800]);
        assert_eq!(
            data.string_units(&pointer_child("", key)),
            Some(vec![0xdfff])
        );
        let identity = client_identity_from_control(&normalized).unwrap();
        assert_eq!(identity.key_units(), &[0xd800]);
        assert!(normalized
            .to_json_string()
            .unwrap()
            .contains(r#""client":"\ud800""#));
    }

    #[tokio::test]
    async fn native_client_event_keeps_exact_identity_and_nested_post_data() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(
            &child,
            r#"
              import { Text, h } from 'claude:surface-runtime';
              export function Card(props: { valid: boolean }, surface: any) {
                if (!surface.state.sent) {
                  surface.setState({ sent: true });
                  surface.post(JSON.parse('{"\\ud800":"\\udfff"}'));
                }
                return h(Text, { children: props.valid ? 'exact-utf16' : 'waiting' });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
                  const { Client, h } = $.ui.resolve(event);
                  return h(Client, {
                    key: '\ud800', module: './card.tsx', props: { valid: false },
                  });
                });
                on('ui.message', (_$, event) => {
                  const key = Object.keys(event.data)[0];
                  return { props: { valid: key === '\ud800' && event.data[key] === '\udfff' } };
                });
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));
        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "utf16",
            "utf16@user",
            directory.path(),
            &entry,
            json!({}),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let rendered = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(json!({
                    "subtype":"ui_render",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"utf16-pane",
                    "props":{},
                    "viewport":{"columns":80,"rows":24},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let revision = rendered.render_revision.unwrap();
        let wire_tree = rendered.response.to_json_string().unwrap();
        assert!(wire_tree.contains(r#""key":"\ud800""#));
        let site = client_site_from_input(&json!({"component":"Pane","instance_id":"utf16-pane"}))
            .unwrap();
        let identity = lock(&host.client_ui.data)
            .parents
            .get(&site)
            .and_then(|parent| parent.clients.keys().next())
            .cloned()
            .expect("the parent render records its Client identity");
        assert_eq!(identity.key_units(), &[0xd800]);

        let mut mount = Utf16JsonProjection::plain(json!({
            "type":"mount",
            "surface":"desktop",
            "component":"Pane",
            "instance_id":"utf16-pane",
            "plugin":"utf16",
            "client":"�",
            "module":"card.tsx",
            "render_revision":revision,
            "columns":80,
            "rows":24,
        }));
        set_exact_string_field(&mut mount, "client", vec![0xd800]).unwrap();
        let mounted = host
            .dispatch_client_ui_operation(mount, session.as_ref())
            .await
            .unwrap();
        let runtime_id = mounted.value["runtimeId"].as_str().unwrap().to_owned();

        let mut commit = Utf16JsonProjection::plain(json!({
            "type":"draw_commit",
            "surface":"desktop",
            "component":"Pane",
            "instance_id":"utf16-pane",
            "render_revision":revision,
            "clients":[{"plugin":"utf16","key":"�","module":"card.tsx"}],
        }));
        commit.strings.push(Utf16JsonString {
            pointer: "/clients/0/key".into(),
            code_units: vec![0xd800],
        });
        commit.validate().unwrap();
        let committed = host
            .dispatch_client_ui_operation(commit, session.as_ref())
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);

        let frame =
            wait_for_client_frame(&session_impl, &runtime_id, "UTF-16 ui.message", |frame| {
                tree_contains_text(&frame["tree"], "exact-utf16")
            })
            .await;
        assert!(tree_contains_text(&frame["tree"], "exact-utf16"));
    }

    #[test]
    fn parent_press_actions_are_recovered_only_from_validated_render_tree() {
        let site = ParentControlSite {
            surface: "desktop".into(),
            component: ClientUiComponent::Pane,
            request_id: "pane-1".into(),
        };
        let actions = collect_parent_press_actions(
            &json!({
                "type":"Box",
                "props":{},
                "children":[{
                    "type":"Select",
                    "props":{"key":"choice","options":[{"value":"a"},{"value":"b"}]},
                    "press":{"plugin":"review","handle":7},
                    "children":[]
                }]
            }),
            &site,
        )
        .unwrap();

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].site, site);
        assert_eq!(actions[0].plugin, "review");
        assert_eq!(actions[0].handle, 7);
        assert_eq!(actions[0].element.as_deref(), Some("choice"));
        assert_eq!(actions[0].kind, "Select");
        assert!(actions[0].select_values.contains("a"));
        assert!(actions[0].select_values.contains("b"));
    }

    #[test]
    fn markdown_press_action_keeps_the_parent_links_for_native_preflight() {
        let site = ParentControlSite {
            surface: "desktop".into(),
            component: ClientUiComponent::Pane,
            request_id: "pane-1".into(),
        };
        let actions = collect_parent_press_actions(
            &json!({
                "type":"Markdown",
                "props":{"text":"link","key":"doc","pressableLinks":["https://example.com/a"]},
                "press":{"plugin":"review","handle":8},
            }),
            &site,
        )
        .unwrap();

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, "Markdown");
        let expected_links = vec!["https://example.com/a".to_owned()];
        assert_eq!(
            actions[0].pressable_links.as_deref(),
            Some(expected_links.as_slice())
        );
    }

    #[test]
    fn worker_fault_result_is_a_local_fault_union_with_native_reason_cap() {
        let response = worker_fault_response(
            Some("runtime-1"),
            4,
            "plugin",
            "view.ts",
            "render",
            &"😀".repeat(120),
        );
        assert_eq!(response["handled"], false);
        assert_eq!(response["renderRevision"], 4);
        assert_eq!(response["runtimeId"], "runtime-1");
        assert_eq!(response["fault"]["phase"], "render");
        assert_eq!(response["fault"]["source"], "worker");
        assert!(
            response["fault"]["reason"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count()
                <= 200
        );
    }

    #[tokio::test]
    async fn worker_frames_preserve_only_positive_safe_frame_sequences() {
        let host = ModHost::start(None).await.unwrap();
        let result = json!({
            "tree":{"type":"Text","props":{},"children":["ready"]},
            "hasPointerListener":false,
            "hasKeyListener":false,
            "frameSequence":3,
        });
        let frame = host
            .frame_for_worker_result("runtime-1", 4, &Utf16JsonProjection::plain(result))
            .unwrap();
        assert_eq!(frame.value["frameSequence"], 3);
        assert_eq!(frame.value["renderRevision"], 4);
        assert!(host
            .frame_for_worker_result(
                "runtime-1",
                4,
                &Utf16JsonProjection::plain(json!({
                    "tree":{"type":"Text","props":{},"children":["stale"]},
                    "hasPointerListener":false,
                    "hasKeyListener":false,
                    "frameSequence":0,
                })),
            )
            .is_none());
        assert!(host
            .frame_for_worker_result(
                "runtime-1",
                4,
                &Utf16JsonProjection::plain(json!({
                    "tree":{"type":"Text","props":{},"children":["missing"]},
                    "hasPointerListener":false,
                    "hasKeyListener":false,
                })),
            )
            .is_none());
    }

    #[tokio::test]
    async fn prepared_tsx_client_runs_the_host_render_mount_draw_post_and_unload_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("card.tsx");
        let entry = directory.path().join("register.tsx");
        std::fs::write(
            &child,
            r#"
              import { Box, Text, Button, h } from 'claude:surface-runtime';
              type CardProps = { title: string; fail?: boolean; failRun?: boolean; timerFault?: boolean };
              export function Card(props: CardProps, surface: any) {
                if (props.fail) throw new Error('client render exploded');
                if (props.timerFault && !surface.state.timerStarted) {
                  surface.setState({ timerStarted: true });
                  surface.every(250, () => { throw new Error('client timer exploded'); });
                }
                if (!surface.state.posted) {
                  surface.setState({ posted: true });
                  if (!props.failRun && !props.timerFault) surface.post({ title: 'posted' });
                }
                return h(Box, { flexDirection: 'column', children: [
                  h(Text, { children: props.title }),
                  h(Button, { key: 'submit', label: 'Send',
                    onPress: () => {
                      if (props.failRun) throw new Error('client press exploded');
                      surface.post({ title: 'pressed' });
                    } }),
                ] });
              }
            "#,
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
              export function register(on) {
                on('ui.render', { surface: 'desktop', component: 'Pane' }, ($, event) => {
                  const { Client, h } = $.ui.resolve(event);
                  const clientProps = event.props.invalid
                    ? { values: Array.from({ length: 20001 }, () => 0) }
                    : event.props.fail
                      ? { title: 'will fail', fail: true }
                    : event.props.failRun
                      ? { title: 'ready to fail', failRun: true }
                      : event.props.timerFault
                        ? { title: 'timer waiting', timerFault: true }
                      : { title: 'initial' };
                  const client = h(Client, {
                    key: 'card', module: './card.tsx', props: clientProps,
                  });
                  return client;
                });
                on('ui.message', (_$, event) => ({
                  handled: true,
                  props: { title: event.data.title },
                }));
              }
            "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let session_impl = std::sync::Arc::new(UiLifecycleSession {
            root: directory.path().to_path_buf(),
            ..UiLifecycleSession::default()
        });
        let session: std::sync::Arc<dyn ModSessionContext> = session_impl.clone();
        host.attach_background_context(std::sync::Arc::downgrade(&session));

        let prepared = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "review",
            "review@user",
            directory.path(),
            &entry,
            json!({}),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let render = |props: Value| {
            json!({
                "subtype":"ui_render",
                "surface":"desktop",
                "component":"Pane",
                "instance_id":"pane-1",
                "props":props,
                "viewport":{"columns":80,"rows":24},
            })
        };

        // The Rust Host's stricter bounded parent-tree check is downstream of
        // the worker's generic tree check. An oversized Client payload must
        // restore the original engine answer and original input props without
        // entering the Client mount/fault lifecycle.
        let original_props = json!({"invalid":true,"preserve":"original"});
        let invalid = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(original_props.clone())),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.response.value["tree"]["type"], "engine");
        assert_eq!(
            invalid.response.value["tree"]["ref"],
            invalid.render_revision.unwrap()
        );
        assert_eq!(invalid.response.value["props"], original_props);
        assert_eq!(invalid.response.value["rewritten"], false);
        assert!(lock(&session_impl.invalidations).is_empty());
        assert_eq!(host.ui_render_generation(), 0);

        let initial_render = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let initial_epoch = initial_render.client_runtime_epochs["review"];
        assert!(initial_epoch > 0);
        assert!(initial_render
            .response
            .value
            .get("clientRuntimeEpochs")
            .is_none());
        let first_manifest_hash = initial_render.response.value["client_modules"]["review"]
            .as_str()
            .unwrap()
            .to_owned();

        // Reinstalling the same exact plugin source/hash is still a real
        // environment reload and receives a new sidecar epoch.
        let reloaded = host.prepare_module(directory.path(), &entry).await.unwrap();
        host.load_with_tier_order_storage_prepared(
            "review",
            "review@user",
            directory.path(),
            &entry,
            json!({}),
            "user",
            None,
            Some(&reloaded),
        )
        .await
        .unwrap();
        let reloaded_render = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let reloaded_epoch = reloaded_render.client_runtime_epochs["review"];
        assert!(reloaded_epoch > initial_epoch);
        assert_eq!(
            reloaded_render.response.value["client_modules"]["review"],
            first_manifest_hash
        );

        // A normal redraw advances renderRevision but keeps the environment
        // epoch stable.
        let rendered = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(rendered.client_runtime_epochs["review"], reloaded_epoch);
        let revision = rendered.render_revision.unwrap();
        let client = &rendered.response.value["tree"];
        let client_module = client["props"]["module"].as_str().unwrap().to_owned();
        assert_eq!(client["type"], "Client");
        assert_eq!(client["client"]["plugin"], "review");
        assert_eq!(client_module, "card.tsx");
        assert!(std::fs::read_to_string(&entry)
            .unwrap()
            .contains("module: './card.tsx'"));
        let manifest = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(json!({"subtype":"ui_client_module","plugin":"review"})),
                session.as_ref(),
            )
            .await
            .unwrap()
            .response;
        assert_eq!(manifest.value["modules"][0]["module"], "card.tsx");

        let mount = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let runtime_id = mount.value["runtimeId"].as_str().unwrap().to_owned();
        assert_eq!(mount.value["renderRevision"], revision);
        assert_eq!(mount.value["frameSequence"], 1);
        assert!(tree_contains_text(&mount.value["tree"], "initial"));

        let committed = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed.value["handled"], true);

        // `surface.post` fires during the first Client render, before mount
        // returns its runtime generation. The Host must queue and replay it.
        let posted =
            wait_for_client_frame(&session_impl, &runtime_id, "initial mount post", |frame| {
                tree_contains_text(&frame["tree"], "posted")
            })
            .await;
        assert!(
            posted["frameSequence"].as_u64().unwrap()
                > mount.value["frameSequence"].as_u64().unwrap()
        );

        let reset = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"setProps",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "props":{"title":"initial"},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(tree_contains_text(&reset.value["tree"], "initial"));
        let held =
            tree_first_held(&reset.value["tree"]).expect("rendered Button has a held action");
        let run = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"runHeld",
                    "runtimeId":runtime_id,
                    "render_revision":revision,
                    "handle":held,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert!(run.value.get("tree").is_some());
        let pressed =
            wait_for_client_frame(&session_impl, &runtime_id, "held Button post", |frame| {
                tree_contains_text(&frame["tree"], "pressed")
            })
            .await;
        assert!(
            pressed["frameSequence"].as_u64().unwrap()
                > reset.value["frameSequence"].as_u64().unwrap()
        );

        // A run failure reports through one authoritative Host ui.fault
        // dispatch. The worker keeps a failed snapshot: later operations on
        // that runtime cannot produce a success frame, and a fresh mount is
        // the recovery boundary.
        let failed_run_parent = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({"failRun":true}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_run_revision = failed_run_parent.render_revision.unwrap();
        let failed_run_mount = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":failed_run_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_run_runtime = failed_run_mount.value["runtimeId"]
            .as_str()
            .unwrap()
            .to_owned();
        let failed_run_commit = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":failed_run_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(failed_run_commit.value["handled"], true);
        let ready_to_fail = wait_for_client_frame(
            &session_impl,
            &failed_run_runtime,
            "run-held setup",
            |frame| tree_contains_text(&frame["tree"], "ready to fail"),
        )
        .await;
        let failed_run_handle = tree_first_held(&ready_to_fail["tree"]).unwrap();
        let run_fault = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"runHeld",
                    "runtimeId":failed_run_runtime,
                    "render_revision":failed_run_revision,
                    "handle":failed_run_handle,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(run_fault.value["fault"]["phase"], "run");
        assert_eq!(run_fault.value["fault"]["source"], "worker");
        let async_run_fault = wait_for_client_frame(
            &session_impl,
            &failed_run_runtime,
            "run fault snapshot",
            |frame| frame.get("fault").is_some(),
        )
        .await;
        assert_eq!(async_run_fault["fault"]["phase"], "run");
        assert_eq!(async_run_fault["fault"]["source"], "worker");
        assert_eq!(async_run_fault["renderRevision"], failed_run_revision);
        assert!(async_run_fault.get("frameSequence").is_none());
        assert!(async_run_fault.get("tree").is_none());
        assert!(
            lock(&host.client_ui.data).instances[&failed_run_runtime].worker_fault_event_dispatched
        );
        let stale_after_run_fault = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"setProps",
                    "runtimeId":failed_run_runtime,
                    "render_revision":failed_run_revision,
                    "props":{"title":"ready to fail","failRun":true},
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(stale_after_run_fault.value["handled"], false);
        assert!(stale_after_run_fault.value.get("tree").is_none());

        let recovered_parent = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let recovered_revision = recovered_parent.render_revision.unwrap();
        let recovered_mount = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":recovered_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let recovered_runtime = recovered_mount.value["runtimeId"].as_str().unwrap();
        assert_ne!(recovered_runtime, failed_run_runtime);
        assert!(tree_contains_text(
            &recovered_mount.value["tree"],
            "initial"
        ));
        let recovered_commit = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":recovered_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(recovered_commit.value["handled"], true);

        // Timer failures have no operation result. The Host emits a local
        // fault snapshot immediately, even though this plugin has no
        // ui.fault listener to trigger an invalidation.
        let timer_parent = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({"timerFault":true}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let timer_revision = timer_parent.render_revision.unwrap();
        let timer_mount = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":timer_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let timer_runtime = timer_mount.value["runtimeId"].as_str().unwrap().to_owned();
        let timer_commit = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":timer_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(timer_commit.value["handled"], true);
        let timer_fault = wait_for_client_frame(
            &session_impl,
            &timer_runtime,
            "timer fault snapshot",
            |frame| frame.get("fault").is_some(),
        )
        .await;
        assert_eq!(timer_fault["fault"]["phase"], "run");
        assert_eq!(timer_fault["fault"]["source"], "worker");
        assert_eq!(timer_fault["renderRevision"], timer_revision);
        assert!(timer_fault.get("frameSequence").is_none());
        assert!(lock(&host.client_ui.data).instances[&timer_runtime].worker_fault_event_dispatched);
        {
            let invalidations = lock(&session_impl.invalidations);
            assert_eq!(
                invalidations.len(),
                1,
                "the successful same-plugin reload invalidates the already-rendered site once"
            );
            let event = &invalidations[0];
            assert_eq!(event["session_id"], "ui-lifecycle-session");
            assert!(event["uuid"].as_str().unwrap().starts_with("mod-ui-state-"));
            assert_eq!(
                event["instances"],
                json!([{
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                }]),
                "the reload invalidation is scoped to the rendered parent site"
            );
        }
        let timer_stale = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"resize",
                    "runtimeId":timer_runtime,
                    "render_revision":timer_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(timer_stale.value["handled"], false);
        assert!(timer_stale.value.get("tree").is_none());
        let timer_generation = lock(&host.client_ui.data).instances[&timer_runtime]
            .worker_generation
            .unwrap();
        let duplicate_timer_fault = json!({
            "kind":"ui.client.event",
            "runtimeId":timer_runtime,
            "generation":timer_generation,
            "plugin":"review",
            "element":"card",
            "module":client_module,
            "action":"fault",
            "phase":"run",
            "reason":"client timer exploded",
        });
        host.handle_client_ui_background_event(&duplicate_timer_fault, session.as_ref())
            .await
            .unwrap();
        assert_eq!(
            lock(&session_impl.frames)
                .iter()
                .filter(|(runtime, frame)| {
                    runtime == &timer_runtime && frame.get("fault").is_some()
                })
                .count(),
            1,
            "a repeated worker fault does not emit a second local failure snapshot"
        );

        // Mount-time render failure uses the same one-shot fault path and
        // removes the incomplete runtime mapping before returning its union.
        let failed_parent = host
            .dispatch_client_ui_control(
                Utf16JsonProjection::plain(render(json!({"fail":true}))),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_revision = failed_parent.render_revision.unwrap();
        let failed_mount = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"mount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "plugin":"review",
                    "client":"card",
                    "module":client_module,
                    "render_revision":failed_revision,
                    "columns":80,
                    "rows":24,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        let failed_mount_runtime = failed_mount.value["runtimeId"].as_str().unwrap().to_owned();
        assert_eq!(failed_mount.value["handled"], false);
        assert_eq!(failed_mount.value["fault"]["phase"], "render");
        assert_eq!(failed_mount.value["fault"]["source"], "worker");
        assert!(!lock(&session_impl.frames).iter().any(|(runtime, frame)| {
            runtime == &failed_mount_runtime && frame.get("fault").is_some()
        }));
        assert!(!lock(&host.client_ui.data)
            .instances
            .contains_key(&failed_mount_runtime));
        let after_mount_fault = host.ui_render_generation();
        let stale_after_mount_fault = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"render",
                    "runtimeId":failed_mount_runtime,
                    "render_revision":failed_revision,
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(stale_after_mount_fault.value["handled"], false);
        assert_eq!(host.ui_render_generation(), after_mount_fault);

        let committed_failure = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_commit",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":failed_revision,
                    "clients":[{"plugin":"review","key":"card","module":client_module}],
                })),
                session.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(committed_failure.value["handled"], true);

        let unload_invalidation_start = lock(&session_impl.invalidations).len();
        host.unload("review@user").await.unwrap();
        let invalidations = lock(&session_impl.invalidations);
        let unload_invalidations = &invalidations[unload_invalidation_start..];
        assert_eq!(
            unload_invalidations.len(),
            2,
            "Native's unload path invalidates at removal start and after table resolution"
        );
        for event in unload_invalidations {
            assert_eq!(event["session_id"], "ui-lifecycle-session");
            assert!(event["uuid"].as_str().unwrap().starts_with("mod-ui-state-"));
            assert_eq!(
                event["instances"],
                json!([{
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                }]),
                "each unload invalidation targets only the tracked parent site"
            );
        }
        assert_ne!(
            unload_invalidations[0]["uuid"],
            unload_invalidations[1]["uuid"]
        );
        assert!(host.client_ui.data.lock().unwrap().instances.is_empty());
    }

    #[test]
    fn stale_mount_cleanup_does_not_remove_a_newer_runtime_mapping() {
        let site = ClientUiRenderSite::new(ClientUiComponent::Pane, "pane-1").unwrap();
        let identity =
            ClientUiIdentity::new("plugin", "view.ts", "card".encode_utf16().collect()).unwrap();
        let key = ClientUiInstanceKey { site, identity };
        let mut data = ClientUiHostData::default();
        data.instance_by_identity
            .insert(key.clone(), "new-runtime".into());

        remove_instance_if_current(&mut data, "old-runtime", &key);

        assert_eq!(
            data.instance_by_identity.get(&key).map(String::as_str),
            Some("new-runtime")
        );
    }

    #[test]
    fn plugin_unload_returns_sites_only_when_the_last_storage_is_removed() {
        let state = ClientUiHostState::default();
        state.install_loaded_plugin("plugin", "plugin@user", None);
        state.install_loaded_plugin("plugin", "plugin@project", None);
        let site = ClientUiRenderSite::new(ClientUiComponent::Pane, "pane-1").unwrap();
        let identity =
            ClientUiIdentity::new("plugin", "view.ts", "card".encode_utf16().collect()).unwrap();
        lock(&state.data).parents.insert(
            site.clone(),
            ParentRender {
                revision: 1,
                token: None,
                committed_generation: None,
                clients: HashMap::from([(identity, Utf16JsonProjection::plain(json!({})))]),
            },
        );

        assert!(state.unload_storage("plugin@user").is_empty());
        let affected = state.unload_storage("plugin@project");
        assert_eq!(affected, vec![site]);
    }

    #[tokio::test]
    async fn stale_draw_unmount_preserves_a_newer_in_flight_parent_render() {
        let host = ModHost::start(None).await.unwrap();
        let session = UiLifecycleSession::default();
        let site = ClientUiRenderSite::new(ClientUiComponent::Pane, "pane-1").unwrap();
        let new_token = host.client_ui.fault_registry.begin_render(site.clone(), 2);
        {
            let mut data = lock(&host.client_ui.data);
            data.parents.insert(
                site.clone(),
                ParentRender {
                    revision: 1,
                    token: None,
                    committed_generation: None,
                    clients: HashMap::new(),
                },
            );
            data.latest_started.insert(site.clone(), 2);
            data.pending_tokens.insert((site.clone(), 2), new_token);
        }

        let result = host
            .dispatch_client_ui_operation(
                Utf16JsonProjection::plain(json!({
                    "type":"draw_unmount",
                    "surface":"desktop",
                    "component":"Pane",
                    "instance_id":"pane-1",
                    "render_revision":1,
                })),
                &session,
            )
            .await
            .unwrap();

        assert_eq!(result.value["handled"], true);
        let data = lock(&host.client_ui.data);
        assert_eq!(data.latest_started.get(&site), Some(&2));
        assert!(data.pending_tokens.contains_key(&(site.clone(), 2)));
        assert!(!data.parents.contains_key(&site));
    }

    #[tokio::test]
    async fn parent_press_preflight_uses_worker_url_and_native_file_path_rules() {
        let host = ModHost::start(None).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();

        assert!(host
            .preflight_parent_press_href(
                "www.Example.com:443/a/../b",
                &["http://www.example.com:443/b".into()],
                cwd,
            )
            .await
            .unwrap());
        assert!(!host
            .preflight_parent_press_href(
                "https://example.com/a",
                &["https://example.com/b".into()],
                cwd,
            )
            .await
            .unwrap());
        assert!(host
            .preflight_parent_press_href("/tmp/a.md", &["file:///tmp/a.md".into()], cwd,)
            .await
            .unwrap());
    }

    #[test]
    fn parent_action_handle_accepts_only_positive_safe_integers() {
        assert_eq!(parent_action_handle(Some(&json!(1))), Some(1));
        assert_eq!(parent_action_handle(Some(&json!(0))), None);
        assert_eq!(parent_action_handle(Some(&json!(-1))), None);
        assert_eq!(parent_action_handle(Some(&json!(1.5))), None);
        assert_eq!(
            parent_action_handle(Some(&json!(MAX_JS_SAFE_INTEGER + 1))),
            None
        );
    }
}
