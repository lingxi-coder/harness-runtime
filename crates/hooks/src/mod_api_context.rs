//! Host-owned provenance for Mod worker API calls.
//!
//! A worker API request must never supply its own origin. The host puts an
//! opaque ticket in each dispatch, the worker closes over that ticket, and the
//! host resolves it back to this immutable snapshot when `$.tool.call` arrives.
//! The separate caller registry is populated from the host's successful Mod
//! load handshake and prevents a ticket from being paired with an invented
//! plugin/hook identity.

use lingxi_core::host::task_registry::FieldPresence;
use lingxi_core::types::HookId;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Opaque host-issued origin ticket captured by one worker API closure.
///
/// The inner token is intentionally available only for the worker protocol;
/// debug output never reveals it and it does not serialize as application
/// data.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ModApiContextTicket(String);

impl ModApiContextTicket {
    /// The opaque token to place on the internal worker request.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ModApiContextTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModApiContextTicket(<opaque>)")
    }
}

/// Worker-reported key used only to locate a host-loaded callback registration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModApiCallerKey {
    /// Worker storage identity matched against the host's load table.
    pub storage_id: String,
    /// Worker callback id, verified against the host's successful load reply.
    pub hook_id: u64,
}

/// A caller identity derived from the successful host load, never from a tool
/// input or a worker-provided origin value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModApiCallerRegistration {
    /// Plugin identity supplied by the host when the module was loaded.
    pub plugin: String,
    /// Host-owned module storage identity.
    pub storage_id: String,
    /// Worker callback id accepted from the successful load handshake.
    pub hook_id: u64,
    /// Callback event accepted from the successful load handshake.
    pub event: String,
}

/// Resolved trusted origin and caller for one worker API request.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModApiContext {
    /// Caller identity recovered from the host registration table.
    pub caller: ModApiCallerRegistration,
    /// Exact origin snapshot issued by the host.
    pub hook_origin: FieldPresence<Value>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModApiContextError {
    /// No host-issued ticket matches this token.
    #[error("Mod API origin ticket is not registered by this host")]
    UnknownTicket,
    /// The ticket exists, but caller storage and callback id are not registered.
    #[error("Mod API caller is not a registered hook")]
    UnknownCaller,
    /// The worker's claimed plugin differs from its registered plugin.
    #[error("Mod API caller plugin does not match its host registration")]
    CallerPluginMismatch,
    /// Load supplied a duplicate callback id or an empty event name.
    #[error("Mod API load returned an invalid or duplicate hook registration")]
    InvalidRegistration,
    /// A host origin value could not be serialized into the deduplication key.
    #[error("origin snapshot could not be encoded for its host ticket")]
    OriginEncoding,
}

#[derive(Default)]
struct OriginContextState {
    /// Canonical FieldPresence JSON identity -> ticket.
    tickets_by_origin: HashMap<String, ModApiContextTicket>,
    /// Ticket -> exact, unnormalized host origin snapshot.
    origins_by_ticket: HashMap<ModApiContextTicket, FieldPresence<Value>>,
    /// `(storage_id, hook_id)` -> the plugin/event captured by successful load.
    callers: HashMap<ModApiCallerKey, ModApiCallerRegistration>,
}

/// Registry shared by the host dispatch paths for one Mod worker.
///
/// Origin snapshots are immutable and tickets are deduplicated by their full
/// FieldPresence value, so long-lived API/timer closures can continue to use a
/// ticket after the originating dispatch ends without allocating one ticket per
/// event. Caller registrations are independently removed on unload/reload.
#[derive(Clone, Default)]
pub struct ModApiOriginContexts {
    state: Arc<Mutex<OriginContextState>>,
}

impl ModApiOriginContexts {
    /// Issue or reuse an opaque ticket for this host-captured origin snapshot.
    pub fn issue(
        &self,
        origin: FieldPresence<Value>,
    ) -> Result<ModApiContextTicket, ModApiContextError> {
        let key = origin_key(&origin)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ticket) = state.tickets_by_origin.get(&key) {
            return Ok(ticket.clone());
        }

        let ticket = ModApiContextTicket(HookId::new().as_uuid().to_string());
        state.tickets_by_origin.insert(key, ticket.clone());
        state.origins_by_ticket.insert(ticket.clone(), origin);
        Ok(ticket)
    }

    /// Replace all registered hook callers for one successfully loaded Mod.
    /// The host supplies the plugin and storage identity; the worker handshake
    /// contributes only the callback ids and event names it actually registered.
    pub fn register_storage(
        &self,
        plugin: &str,
        storage_id: &str,
        api_callers: impl IntoIterator<Item = (u64, String)>,
    ) -> Result<(), ModApiContextError> {
        let mut registrations = Vec::new();
        let mut seen_hook_ids = std::collections::HashSet::new();
        for (hook_id, event) in api_callers {
            if event.is_empty() || !seen_hook_ids.insert(hook_id) {
                return Err(ModApiContextError::InvalidRegistration);
            }
            registrations.push(ModApiCallerRegistration {
                plugin: plugin.to_owned(),
                storage_id: storage_id.to_owned(),
                hook_id,
                event,
            });
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.callers.retain(|key, _| key.storage_id != storage_id);
        for registration in registrations {
            let key = ModApiCallerKey {
                storage_id: registration.storage_id.clone(),
                hook_id: registration.hook_id,
            };
            state.callers.insert(key, registration);
        }
        Ok(())
    }

    /// Remove all caller registrations owned by a disabled or replaced Mod.
    pub fn unregister_storage(&self, storage_id: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .callers
            .retain(|key, _| key.storage_id != storage_id);
    }

    /// Resolve a worker's opaque ticket and registration key to host facts.
    /// `claimed_plugin`, when present in the worker protocol, is checked against
    /// the host registration and is never used as the returned caller identity.
    pub fn resolve(
        &self,
        ticket: &str,
        caller_key: &ModApiCallerKey,
        claimed_plugin: Option<&str>,
    ) -> Result<ResolvedModApiContext, ModApiContextError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ticket = ModApiContextTicket(ticket.to_owned());
        let hook_origin = state
            .origins_by_ticket
            .get(&ticket)
            .cloned()
            .ok_or(ModApiContextError::UnknownTicket)?;
        let caller = state
            .callers
            .get(caller_key)
            .cloned()
            .ok_or(ModApiContextError::UnknownCaller)?;
        if claimed_plugin.is_some_and(|plugin| plugin != caller.plugin) {
            return Err(ModApiContextError::CallerPluginMismatch);
        }
        Ok(ResolvedModApiContext {
            caller,
            hook_origin,
        })
    }
}

/// Apply Native's `n.origin ?? [plugin]` at the direct `$.tool.call` boundary.
/// Value origins are copied without projection; missing and JSON-null origins
/// take the registered plugin fallback.
#[must_use]
pub fn native_tool_call_origin(origin: FieldPresence<Value>, plugin: &str) -> FieldPresence<Value> {
    match origin {
        FieldPresence::Value(Value::Null) => {
            FieldPresence::Value(Value::Array(vec![Value::String(plugin.to_owned())]))
        }
        FieldPresence::Value(origin) => FieldPresence::Value(origin),
        FieldPresence::Missing | FieldPresence::Null => {
            FieldPresence::Value(Value::Array(vec![Value::String(plugin.to_owned())]))
        }
    }
}

fn origin_key(origin: &FieldPresence<Value>) -> Result<String, ModApiContextError> {
    let (tag, value) = match origin {
        FieldPresence::Missing => ("missing", None),
        FieldPresence::Null => ("null", None),
        FieldPresence::Value(value) => ("value", Some(value)),
    };
    let encoded = value
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| ModApiContextError::OriginEncoding)?
        .unwrap_or_default();
    Ok(format!("{tag}:{encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn register_one(contexts: &ModApiOriginContexts) -> ModApiCallerKey {
        contexts
            .register_storage(
                "trusted-plugin",
                "trusted-storage",
                [(17, "tool.call".to_owned())],
            )
            .unwrap();
        ModApiCallerKey {
            storage_id: "trusted-storage".into(),
            hook_id: 17,
        }
    }

    #[test]
    fn tickets_preserve_presence_and_reuse_only_equal_snapshots() {
        let contexts = ModApiOriginContexts::default();
        let missing = contexts.issue(FieldPresence::Missing).unwrap();
        let missing_again = contexts.issue(FieldPresence::Missing).unwrap();
        let null = contexts.issue(FieldPresence::Null).unwrap();
        let value = contexts
            .issue(FieldPresence::Value(
                json!(["parent", {"source": "worker"}]),
            ))
            .unwrap();

        assert_eq!(missing, missing_again);
        assert_ne!(missing, null);
        assert_ne!(null, value);
        assert!(!format!("{missing:?}").contains(missing.as_str()));
        assert_eq!(missing.as_str().len(), 36);
    }

    #[test]
    fn ticket_resolution_derives_caller_from_loaded_registration() {
        let contexts = ModApiOriginContexts::default();
        let caller_key = register_one(&contexts);
        let ticket = contexts
            .issue(FieldPresence::Value(json!(["parent-mod", "child-agent"])))
            .unwrap();

        let resolved = contexts
            .resolve(ticket.as_str(), &caller_key, Some("trusted-plugin"))
            .unwrap();
        assert_eq!(resolved.caller.plugin, "trusted-plugin");
        assert_eq!(resolved.caller.event, "tool.call");
        assert_eq!(
            resolved.hook_origin,
            FieldPresence::Value(json!(["parent-mod", "child-agent"]))
        );

        assert_eq!(
            contexts.resolve(ticket.as_str(), &caller_key, Some("forged-plugin")),
            Err(ModApiContextError::CallerPluginMismatch)
        );
        assert_eq!(
            contexts.resolve(
                ticket.as_str(),
                &ModApiCallerKey {
                    storage_id: "other-storage".into(),
                    hook_id: 17,
                },
                None,
            ),
            Err(ModApiContextError::UnknownCaller)
        );
    }

    #[test]
    fn immutable_tickets_keep_concurrent_agent_origins_isolated() {
        let contexts = ModApiOriginContexts::default();
        let caller_key = register_one(&contexts);
        let parent_a = contexts
            .issue(FieldPresence::Value(json!(["root", "agent-a"])))
            .unwrap();
        let parent_b = contexts
            .issue(FieldPresence::Value(json!(["root", "agent-b"])))
            .unwrap();

        let a = contexts
            .resolve(parent_a.as_str(), &caller_key, Some("trusted-plugin"))
            .unwrap();
        let b = contexts
            .resolve(parent_b.as_str(), &caller_key, Some("trusted-plugin"))
            .unwrap();
        assert_eq!(
            a.hook_origin,
            FieldPresence::Value(json!(["root", "agent-a"]))
        );
        assert_eq!(
            b.hook_origin,
            FieldPresence::Value(json!(["root", "agent-b"]))
        );
    }

    #[test]
    fn nullish_origin_uses_native_plugin_fallback_but_value_is_unchanged() {
        let expected = FieldPresence::Value(json!(["trusted-plugin"]));
        assert_eq!(
            native_tool_call_origin(FieldPresence::Missing, "trusted-plugin"),
            expected
        );
        assert_eq!(
            native_tool_call_origin(FieldPresence::Null, "trusted-plugin"),
            expected
        );
        assert_eq!(
            native_tool_call_origin(FieldPresence::Value(Value::Null), "trusted-plugin"),
            expected
        );
        assert_eq!(
            native_tool_call_origin(
                FieldPresence::Value(json!({"kind":"parent"})),
                "trusted-plugin"
            ),
            FieldPresence::Value(json!({"kind":"parent"}))
        );
    }

    #[test]
    fn unload_removes_registered_callers_without_rewriting_ticket_origin() {
        let contexts = ModApiOriginContexts::default();
        let caller_key = register_one(&contexts);
        let ticket = contexts.issue(FieldPresence::Null).unwrap();
        contexts.unregister_storage("trusted-storage");

        assert_eq!(
            contexts.resolve(ticket.as_str(), &caller_key, None),
            Err(ModApiContextError::UnknownCaller)
        );
    }
}
