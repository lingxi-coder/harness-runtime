//! Teammate mailbox + router for coordinator-mode messaging.
//!
//! Each registered worker has a `TeammateMailbox`; the `MailboxRouter`
//! resolves an [`AgentId`] to its mailbox and delivers messages. Tokio
//! `Notify` is used for the async wake-up path; the inbox itself sits
//! behind a `std::sync::Mutex` because we only ever hold it across
//! synchronous boundaries.

#![allow(clippy::unwrap_used)] // std mutex poisoning is fatal for us anyway

use lingxi_core::types::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Notify;

/// One message routed between a coordinator and a teammate worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeammateMessage {
    /// Who sent the message.
    pub from: MessageSender,
    /// Model-visible sender name used by the teammate envelope. This preserves
    /// display names instead of degrading teammate senders to UUIDs.
    #[serde(default)]
    pub from_name: String,
    /// Message content.
    pub content: String,
    /// Optional concise summary rendered as the envelope's `summary=`
    /// attribute. Ordinary string `SendMessage` calls require and preserve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Caller-assigned message ID for idempotency.
    pub message_id: String,
    /// Wall-clock send time.
    pub timestamp: std::time::SystemTime,
    /// Optional swarm-protocol request id for messages that participate in a
    /// request/response handshake (shutdown / plan-approval). Minted by
    /// `SendMessage` for a `shutdown_request` (`generateRequestId`,
    /// `SendMessageTool.ts:276`) and echoed by the corresponding response.
    /// `None` for ordinary teammate messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Originator of a [`TeammateMessage`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageSender {
    /// The coordinator agent.
    Coordinator,
    /// A specific teammate worker.
    Teammate(AgentId),
    /// A user-initiated message.
    User,
    /// A system-generated message (e.g. injected notifications).
    System,
}

/// Errors produced by mailbox operations.
#[derive(Debug, Clone, Error)]
pub enum MailboxError {
    /// The mailbox has been closed.
    #[error("mailbox closed")]
    Closed,
    /// The mailbox is at capacity.
    #[error("mailbox full")]
    Full,
    /// No mailbox is registered for the recipient ID.
    #[error("recipient not found: {0}")]
    NotFound(AgentId),
}

/// Per-teammate inbox with bounded capacity and async wake-up.
pub struct TeammateMailbox {
    /// The owning teammate's agent ID.
    pub agent_id: AgentId,
    inbox: Mutex<VecDeque<TeammateMessage>>,
    max_size: usize,
    waker: Arc<Notify>,
    closed: AtomicBool,
}

impl TeammateMailbox {
    /// Construct a new mailbox owned by `agent_id`.
    #[must_use]
    pub fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            inbox: Mutex::new(VecDeque::new()),
            max_size: 100,
            waker: Arc::new(Notify::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Deliver `msg` to the inbox. Returns an error if the mailbox is full
    /// or closed.
    pub fn deliver(&self, msg: TeammateMessage) -> Result<(), MailboxError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(MailboxError::Closed);
        }
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.len() >= self.max_size {
            return Err(MailboxError::Full);
        }
        inbox.push_back(msg);
        self.waker.notify_one();
        Ok(())
    }

    /// Drain all queued messages.
    pub fn drain(&self) -> Vec<TeammateMessage> {
        self.inbox.lock().unwrap().drain(..).collect()
    }

    /// Wait up to `timeout` for the next message.
    pub async fn wait_for_message(&self, timeout: Duration) -> Option<TeammateMessage> {
        tokio::select! {
            () = self.waker.notified() => self.inbox.lock().unwrap().pop_front(),
            () = tokio::time::sleep(timeout) => None,
        }
    }
}

/// Routes a [`TeammateMessage`] to the mailbox registered for an [`AgentId`].
pub struct MailboxRouter {
    colors: tokio::sync::RwLock<std::collections::HashMap<AgentId, String>>,
    mailboxes: tokio::sync::RwLock<std::collections::HashMap<AgentId, Arc<TeammateMailbox>>>,
    /// Display-name → [`AgentId`] index (keys lower-cased) so a message can be
    /// addressed by a teammate's NAME, not just its id. claude-code's mailbox is
    /// keyed by agent name (`teammateMailbox.ts`); the `TaskUpdate` owner-change
    /// notification and `getAgentStatuses` both address recipients by NAME. The
    /// registry populates this on `spawn_worker` and clears it on
    /// `delete_worker`. Separate from `mailboxes` so the id-keyed delivery path
    /// stays unchanged.
    names: tokio::sync::RwLock<std::collections::HashMap<String, AgentId>>,
    /// Additional address → [`AgentId`] index for identities the model is handed
    /// but that are not display names — today the background agent's TASK id,
    /// which is what a completion `<task-notification>` carries and what the
    /// coordinator prompt tells the model to send to.
    ///
    /// Deliberately separate from [`Self::names`]: that index feeds
    /// [`Self::named_recipients`], which drives `ListAgents` rows and `to:"*"`
    /// broadcast, and neither dedupes by [`AgentId`]. Putting a second address
    /// for the same agent in there would show a duplicate agent and deliver
    /// every broadcast to it twice.
    aliases: tokio::sync::RwLock<std::collections::HashMap<String, AgentId>>,
}

impl MailboxRouter {
    /// Construct an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            colors: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            mailboxes: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            names: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            aliases: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Store a palette color against the resolved identity so aliases agree.
    pub async fn set_color(&self, name: &str, color: &str) {
        if let Some(id) = self
            .resolve_name(name)
            .await
            .or(self.resolve_alias(name).await)
        {
            self.colors.write().await.insert(id, color.to_owned());
        }
    }

    pub async fn teammate_color(&self, name: &str) -> Option<String> {
        let id = self
            .resolve_name(name)
            .await
            .or(self.resolve_alias(name).await)
            .or_else(|| uuid::Uuid::parse_str(name).ok().map(AgentId::from_uuid))?;
        self.colors.read().await.get(&id).cloned()
    }

    /// Register `mailbox` for `agent_id`. Existing entries are overwritten.
    pub async fn register(&self, agent_id: AgentId, mailbox: Arc<TeammateMailbox>) {
        self.mailboxes.write().await.insert(agent_id, mailbox);
    }

    /// Index `name` → `agent_id` so the worker can later be addressed by its
    /// display name (case-insensitively). Empty names are ignored.
    pub async fn register_name(&self, name: &str, agent_id: AgentId) {
        if name.is_empty() {
            return;
        }
        self.names
            .write()
            .await
            .insert(name.to_ascii_lowercase(), agent_id);
    }

    /// Resolve a teammate display `name` to its [`AgentId`] (case-insensitive),
    /// if one is registered.
    pub async fn resolve_name(&self, name: &str) -> Option<AgentId> {
        self.names
            .read()
            .await
            .get(&name.to_ascii_lowercase())
            .copied()
    }

    /// Index an additional `alias` → `agent_id` address (case-insensitively).
    ///
    /// Unlike [`Self::register_name`] this does NOT make the agent appear under
    /// a second name in `ListAgents` or receive broadcasts twice; it only makes
    /// the alias resolvable when a message names it directly. Empty aliases are
    /// ignored.
    pub async fn register_alias(&self, alias: &str, agent_id: AgentId) {
        if alias.is_empty() {
            return;
        }
        self.aliases
            .write()
            .await
            .insert(alias.to_ascii_lowercase(), agent_id);
    }

    /// Resolve an additional address to its [`AgentId`], if one is registered.
    pub async fn resolve_alias(&self, alias: &str) -> Option<AgentId> {
        self.aliases
            .read()
            .await
            .get(&alias.to_ascii_lowercase())
            .copied()
    }

    /// Snapshot every display-name/address pair currently registered. Used by
    /// the narrow mailbox trait's broadcast operation; the snapshot drops the
    /// name-index lock before any mailbox delivery awaits.
    pub async fn named_recipients(&self) -> Vec<(String, AgentId)> {
        self.names
            .read()
            .await
            .iter()
            .map(|(name, id)| (name.clone(), *id))
            .collect()
    }

    /// Look up the mailbox registered for `agent_id`, if any.
    ///
    /// Returns a clone of the `Arc<TeammateMailbox>` so the caller (the
    /// mailbox→runner pump) can park on it independently of the router lock.
    /// `None` when no mailbox is registered (the worker was never spawned or was
    /// already unregistered).
    pub async fn get(&self, agent_id: &AgentId) -> Option<Arc<TeammateMailbox>> {
        self.mailboxes.read().await.get(agent_id).cloned()
    }

    /// Route `msg` to `to`.
    pub async fn route(&self, to: &AgentId, msg: TeammateMessage) -> Result<(), MailboxError> {
        let mailboxes = self.mailboxes.read().await;
        let mb = mailboxes.get(to).ok_or(MailboxError::NotFound(*to))?;
        mb.deliver(msg)
    }

    /// Drop the mailbox registration for `agent` (and any name index pointing at
    /// it).
    pub async fn unregister(&self, agent: &AgentId) {
        self.mailboxes.write().await.remove(agent);
        self.colors.write().await.remove(agent);
        self.names.write().await.retain(|_, id| id != agent);
        self.aliases.write().await.retain(|_, id| id != agent);
    }
}

impl Default for MailboxRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn palette_color_resolves_aliases_and_is_removed_with_identity() {
        let router = MailboxRouter::new();
        let id = AgentId::new();
        router.register_name("Alice", id).await;
        router.register_alias("Alice@session", id).await;
        router.set_color("alice", "blue").await;
        assert_eq!(
            router.teammate_color("ALICE").await.as_deref(),
            Some("blue")
        );
        assert_eq!(
            router.teammate_color("Alice@session").await.as_deref(),
            Some("blue")
        );
        router.unregister(&id).await;
        assert_eq!(router.teammate_color(&id.as_uuid().to_string()).await, None);
    }

    #[tokio::test]
    async fn get_returns_registered_mailbox() {
        let router = MailboxRouter::new();
        let agent = AgentId::new();
        // Unknown id before registration.
        assert!(router.get(&agent).await.is_none());

        let mailbox = Arc::new(TeammateMailbox::new(agent));
        router.register(agent, mailbox.clone()).await;

        let got = router
            .get(&agent)
            .await
            .expect("registered mailbox resolves");
        assert!(Arc::ptr_eq(&got, &mailbox), "get returns the same Arc");

        // After unregister it is gone again.
        router.unregister(&agent).await;
        assert!(router.get(&agent).await.is_none());
    }

    #[tokio::test]
    async fn an_alias_routes_without_becoming_a_second_name() {
        // The model is handed a background agent's TASK id in its completion
        // notification and told to continue that agent by sending to it. That
        // id is not a display name, so it must resolve for a direct send while
        // staying out of the name index that drives ListAgents rows and `to:"*"`
        // broadcast — otherwise the agent shows up twice and receives every
        // broadcast twice.
        let router = MailboxRouter::new();
        let agent = AgentId::new();
        router
            .register(agent, Arc::new(TeammateMailbox::new(agent)))
            .await;
        router.register_name("reviewer", agent).await;
        router.register_alias("a1b2c3d4e", agent).await;

        assert_eq!(router.resolve_alias("a1b2c3d4e").await, Some(agent));
        assert_eq!(router.resolve_alias("A1B2C3D4E").await, Some(agent));
        // The alias is NOT a name, and the name is NOT an alias.
        assert_eq!(router.resolve_name("a1b2c3d4e").await, None);
        assert_eq!(router.resolve_alias("reviewer").await, None);
        assert_eq!(
            router.named_recipients().await,
            vec![("reviewer".to_string(), agent)],
            "an alias must not add a broadcast recipient or a ListAgents row",
        );

        // Unregistering the agent drops both indexes.
        router.unregister(&agent).await;
        assert_eq!(router.resolve_alias("a1b2c3d4e").await, None);
        assert_eq!(router.resolve_name("reviewer").await, None);
    }
}
