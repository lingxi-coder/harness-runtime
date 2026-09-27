//! Unified message queue for user / notification / orphan / hook inputs
//! (spec §27).
//!
//! 1:1 parity port of claude-code's module-level command queue
//! (`src/utils/messageQueueManager.ts`). All inputs — user text, slash
//! commands, task notifications, orphaned permissions, hook injections —
//! flow through this ONE queue. Dequeue order is by priority
//! (`Now` > `Next` > `Later`); within the same priority commands are FIFO.
//!
//! ## Parity notes
//!
//! - Ordering is NOT enforced by insert-sorting. Like claude-code, items are
//!   appended in arrival order and the read/drain operations
//!   ([`MessageQueueManager::dequeue`], [`MessageQueueManager::peek`],
//!   [`MessageQueueManager::get_by_max_priority`]) compute the best priority
//!   over the whole queue, preserving FIFO within a level — the exact
//!   semantics of `getCommandsByMaxPriority` / `dequeue` in
//!   `messageQueueManager.ts`. (The previous insert-sorted `VecDeque`
//!   approximated this but had no max-priority threshold snapshot+remove
//!   pairing the mid-turn drain needs.)
//! - Every mutation emits a [`crate::QueueOperation`] to an optional recorder
//!   (twin of `logOperation` / `recordQueueOperation`), so the operation log
//!   that backs crash-recovery replay is actually produced.
//! - A `Now`-priority enqueue cancels the active turn's
//!   [`tokio_util::sync::CancellationToken`] (twin of
//!   `subscribeToCommandQueue` ⇒ `abortController.abort()` in print.ts).

use protocol::{AgentId, HookId, ToolUseId};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::operations::{QueueOperation, QueueOperationRecorder};

/// One queued unit of work waiting to be consumed by an Agent's run loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedCommand {
    /// Unique identifier for trace correlation.
    pub uuid: String,
    /// What the command represents (user text, notification, hook, etc).
    pub content: QueuedCommandContent,
    /// Dequeue priority.
    pub priority: QueuePriority,
    /// When the command entered the queue.
    pub queued_at: SystemTime,
    /// What subsystem produced the command.
    pub source: QueueSource,
    /// Agent that should receive this command. `None` = main thread.
    ///
    /// Subagents run in-process and share the one queue; the drain gate filters
    /// by this field so a subagent's background task notifications don't leak
    /// into the coordinator's context (twin of `QueuedCommand.agentId`, the
    /// isolation filter at query.ts:1574-1577 / print.ts:1924).
    #[serde(default)]
    pub agent_id: Option<AgentId>,
    /// When `true` the text is treated as plain text even if it starts with
    /// `/`. Set for remotely-received messages (bridge/CCR) whose text is meant
    /// for the model, not a local slash command (twin of `skipSlashCommands`).
    #[serde(default)]
    pub skip_slash_commands: bool,
    /// When `true` the resulting user message is `is_meta` — hidden in the
    /// transcript UI but visible to the model. Used by system-generated prompts
    /// routed through the queue (twin of `isMeta`).
    #[serde(default)]
    pub is_meta: bool,
    /// Exact scheduled-job identity, separate from the queue command UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_task_id: Option<String>,
    /// Exact scheduled-fire UUID, supplied by its producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_fire_id: Option<String>,
}

impl QueuedCommand {
    /// Whether this command targets the main thread (the coordinator), i.e. has
    /// no `agent_id`. Twin of the `cmd.agentId === undefined` filter the
    /// between-turn / SDK drains use.
    #[must_use]
    pub fn is_main_thread(&self) -> bool {
        self.agent_id.is_none()
    }

    /// Whether this command is a task notification scoped to `agent`. Twin of
    /// the subagent drain gate (`mode === 'task-notification' && agentId === currentAgentId`).
    ///
    /// **Nothing in production constructs a
    /// [`QueuedCommandContent::TaskNotification`] today**, so this predicate is
    /// unreachable outside tests and the green test below is coverage of the
    /// FILTER, not of any routing. The seam that actually decides who receives a
    /// completion is `tasks::registry::TaskRegistry::take_pending_task_notifications`,
    /// which derives notifications from task state (terminal AND not `notified`)
    /// and hands every one of them to the main session — the gap `AGT-05` /
    /// `TN-10` name. This half of the port is the shape a per-agent route would
    /// take if the drain ever grew one; it is not that route.
    #[must_use]
    pub fn is_task_notification_for(&self, agent: &AgentId) -> bool {
        matches!(self.content, QueuedCommandContent::TaskNotification { .. })
            && self.agent_id.as_ref() == Some(agent)
    }

    /// Whether this command is a slash command that should be routed through the
    /// slash-command processor rather than sent to the model as text. Excluded
    /// from the mid-turn drain (twin of `isSlashCommand`, query.ts:1573).
    ///
    /// A `SlashCommand` content variant always qualifies; a `UserInput` whose
    /// trimmed text starts with `/` qualifies UNLESS `skip_slash_commands`.
    #[must_use]
    pub fn is_slash_command(&self) -> bool {
        match &self.content {
            QueuedCommandContent::SlashCommand { .. } => true,
            QueuedCommandContent::UserInput { text } => {
                !self.skip_slash_commands && text.trim_start().starts_with('/')
            }
            _ => false,
        }
    }

    /// The user-facing text payload, if this command carries one. Used by the
    /// batching path (`join_prompt_values`) and to extract injected prompt text.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            QueuedCommandContent::UserInput { text } => Some(text),
            _ => None,
        }
    }
}

/// The payload of a [`QueuedCommand`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueuedCommandContent {
    /// Raw text from the human user.
    UserInput {
        /// User-supplied text.
        text: String,
    },
    /// A parsed slash-command invocation.
    SlashCommand {
        /// Slash-command payload, already validated.
        parsed_json: serde_json::Value,
    },
    /// Notification from a background subagent or scheduled task.
    ///
    /// Declared, never produced: the only constructor in the workspace is in
    /// this file's `mod tests`. See [`QueuedCommand::is_task_notification_for`]
    /// for where completions are actually routed.
    TaskNotification {
        /// Notification payload (serialized).
        value: String,
        /// How the notification should be surfaced.
        mode: NotificationMode,
    },
    /// Inter-agent message routed via `send_message`.
    TeammateMessage {
        /// Originating agent.
        from: AgentId,
        /// Body of the message.
        content: String,
    },
    /// A late `control_response` permission decision whose originating
    /// `can_use_tool` request was lost (process restart with resume, or a
    /// duplicate/late delivery) — the stdio control-plane could not match it to a
    /// pending request. Carries enough to RE-RUN the orphaned tool from the
    /// recovered transcript, 1:1 with claude-code's `mode:'orphaned-permission'`
    /// command (`print.ts` `handleOrphanedPermissionResponse` →
    /// `queryHelpers.ts` `handleOrphanedPermission`).
    OrphanedPermission {
        /// Tool use that was awaiting the decision (the `toolUseID` carried on the
        /// inner `control_response` payload).
        tool_use_id: ToolUseId,
        /// The raw inner `control_response.response` permission payload
        /// (`{behavior:"allow"|"deny", updatedInput?, message?, …}`). The consumer
        /// maps it to a `PermissionOutcome` to force the tool's decision —
        /// twin of `orphanedPermission.permissionResult`. Stored as raw JSON
        /// because `msgqueue` sits below the `permission`/`platform-api` crates and
        /// cannot name `PermissionOutcome`.
        #[serde(default)]
        permission_decision_json: serde_json::Value,
        /// Human-readable explanation (diagnostic only).
        #[serde(default)]
        reason: String,
    },
    /// Engine-injected content from a hook.
    HookInjected {
        /// Content to inject into the agent's next prompt.
        content: String,
        /// Hook that produced the injection.
        hook_id: HookId,
    },
}

/// How a [`QueuedCommandContent::TaskNotification`] should be surfaced.
///
/// Reachable only through that variant, which nothing in production builds, so
/// [`Self::Normal`] has never been constructed anywhere in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationMode {
    /// Normal notification.
    Normal,
    /// Notification associated with a task completion.
    TaskNotification,
}

/// Dequeue priority. `Ord` is derived so `Later < Next < Now`.
///
/// Ordering is consulted by the read/drain operations (not enforced at insert
/// time) so FIFO is preserved within a level, matching claude-code's
/// `PRIORITY_ORDER` lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum QueuePriority {
    /// Defer until after all higher-priority work (end-of-turn / between-turn drain).
    Later,
    /// Next available slot (mid-turn drain).
    Next,
    /// Interrupt and process immediately; aborts the in-flight turn.
    Now,
}

/// Where a queued command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueSource {
    /// Direct prompt input from the user.
    PromptInput,
    /// Subagent task completion notification.
    TaskCompletion,
    /// Inter-agent `send_message`.
    AgentSendMessage,
    /// Engine hook injection.
    Hook,
    /// Orphan permission cleanup.
    Orphan,
    /// Scheduled cron task.
    Cron,
}

/// The runtime queue itself. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct MessageQueueManager {
    queue: Arc<RwLock<VecDeque<QueuedCommand>>>,
    notify: Arc<Notify>,
    /// Optional crash-recovery operation recorder. `None` = log to nowhere
    /// (the default; tests and embedders that don't replay).
    recorder: Arc<RwLock<Option<Arc<dyn QueueOperationRecorder>>>>,
    /// The active turn's cancel token, if a turn is running. A `Now`-priority
    /// enqueue cancels it (twin of `subscribeToCommandQueue` ⇒ abort).
    active_turn: Arc<RwLock<Option<CancellationToken>>>,
    /// Optional hook run RIGHT BEFORE the active turn's token is fired by a
    /// `Now`-priority enqueue. The composition root uses this to record the
    /// abort REASON (so the turn loop can tell a `Now`-command abort from a user
    /// Ctrl+C) on a flag of its own type — keeping `msgqueue` free of any
    /// dependency on the orchestrator's reason enum. `None` ⇒ no reason
    /// bookkeeping (the default; the token still fires).
    on_now_abort: Arc<RwLock<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl MessageQueueManager {
    /// Enqueue a passive goal retry through the ordinary host turn lifecycle.
    pub async fn enqueue_goal_retry(&self, uuid: String, body: String, cancel: CancellationToken) {
        self.enqueue(QueuedCommand {
            uuid: uuid.clone(),
            content: QueuedCommandContent::UserInput { text: body },
            priority: QueuePriority::Later,
            queued_at: SystemTime::now(),
            source: QueueSource::Hook,
            agent_id: None,
            skip_slash_commands: true,
            is_meta: true,
            scheduled_task_id: None,
            scheduled_fire_id: None,
        })
        .await;
        let queue = self.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            queue.remove(&[uuid], "goal retry invalidated").await;
        });
    }

    /// Construct an empty queue with no operation recorder and no active turn.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: Arc::new(RwLock::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            recorder: Arc::new(RwLock::new(None)),
            active_turn: Arc::new(RwLock::new(None)),
            on_now_abort: Arc::new(RwLock::new(None)),
        }
    }

    /// Construct an empty queue with the operation recorder pre-installed
    /// synchronously at build time. Equivalent to [`Self::new`] followed by
    /// [`Self::set_recorder`], but usable from a synchronous composition root
    /// (e.g. `BridgeConnection::new`) where no async runtime is yet available to
    /// `.await set_recorder`. All other fields match [`Self::new`] exactly, so a
    /// queue built this way is byte-identical to a freshly-`new`'d one except for
    /// the recorder being present.
    #[must_use]
    pub fn with_recorder(recorder: Arc<dyn QueueOperationRecorder>) -> Self {
        Self {
            queue: Arc::new(RwLock::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            recorder: Arc::new(RwLock::new(Some(recorder))),
            active_turn: Arc::new(RwLock::new(None)),
            on_now_abort: Arc::new(RwLock::new(None)),
        }
    }

    /// Install the `Now`-abort reason hook: a callback run synchronously right
    /// before the active turn's token is fired by a `Now`-priority enqueue.
    /// The composition root captures its own abort-reason flag in this closure
    /// so the turn loop can distinguish a `Now`-command abort from a user
    /// interrupt — without `msgqueue` knowing the reason type. Idempotent.
    pub async fn set_now_abort_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.on_now_abort.write().await = Some(hook);
    }

    /// Install the operation recorder (twin of wiring `recordQueueOperation`).
    /// Every subsequent mutation emits a [`QueueOperation`] to it.
    pub async fn set_recorder(&self, recorder: Arc<dyn QueueOperationRecorder>) {
        *self.recorder.write().await = Some(recorder);
    }

    /// Register the active turn's cancellation token so a `Now`-priority enqueue
    /// can abort it. Call at turn start; pair with [`Self::clear_active_turn`]
    /// at turn end. Twin of the print.ts `abortController` the queue subscriber
    /// fires on `now`.
    pub async fn register_active_turn(&self, token: CancellationToken) {
        *self.active_turn.write().await = Some(token);
    }

    /// Forget the active turn's token (turn ended). A later `Now` enqueue then
    /// has nothing to abort — it simply waits to be drained.
    pub async fn clear_active_turn(&self) {
        *self.active_turn.write().await = None;
    }

    /// Whether a turn still owns the queue. Scheduled work waits for this owner
    /// to finish before announcing a wakeup or resolving its task file.
    pub async fn has_active_turn(&self) -> bool {
        self.active_turn.read().await.is_some()
    }

    /// Consume the next batch of human prompts that belongs inside the running
    /// main-thread turn.
    ///
    /// This is the canonical mid-turn queue operation used by every host. It
    /// accepts only `Next` priority, main-thread, non-slash user input. `Now`
    /// remains queued after cancelling the active token so the between-turn
    /// driver can run it as the interrupting turn; `Later` waits for the normal
    /// between-turn drain.
    pub async fn take_mid_turn_prompt(&self) -> Option<String> {
        let batch = self
            .get_by_max_priority(QueuePriority::Next, |command| {
                command.is_main_thread()
                    && !command.is_slash_command()
                    && command.priority == QueuePriority::Next
            })
            .await;
        let (joined, consumed) = join_prompt_values(&batch)?;
        self.consume(&consumed, "drained mid-turn into running turn")
            .await;
        Some(joined)
    }

    /// Append `cmd` to the queue; wakes one waiter and logs an `Enqueue`.
    ///
    /// If `cmd` is `Now`-priority, the active turn's cancellation token (if any)
    /// is fired so the in-flight turn aborts and the run loop drains the urgent
    /// command (twin of print.ts:1858-1863).
    pub async fn enqueue(&self, cmd: QueuedCommand) {
        let is_now = cmd.priority == QueuePriority::Now;
        let op = QueueOperation::Enqueue {
            uuid: cmd.uuid.clone(),
            priority: cmd.priority,
            source: cmd.source,
        };
        {
            let mut q = self.queue.write().await;
            q.push_back(cmd);
        }
        self.record(op).await;
        self.notify.notify_one();
        if is_now {
            if let Some(token) = self.active_turn.read().await.as_ref() {
                // Record the abort REASON before firing so the turn loop reads
                // `QueueNowCommand` (not a user interrupt) when it observes the
                // cancellation. No-op when no hook is wired.
                if let Some(hook) = self.on_now_abort.read().await.as_ref() {
                    hook();
                }
                token.cancel();
            }
        }
    }

    /// Pop the highest-priority queued command, or `None` if empty.
    ///
    /// Within the same priority level commands are dequeued FIFO. Optionally
    /// narrowed by `filter`: only commands for which the predicate returns
    /// `true` are considered; non-matching commands stay in the queue
    /// untouched. Twin of `dequeue(filter?)`.
    pub async fn dequeue_filtered(
        &self,
        filter: impl Fn(&QueuedCommand) -> bool,
    ) -> Option<QueuedCommand> {
        let mut q = self.queue.write().await;
        let idx = Self::best_index(&q, &filter)?;
        let cmd = q.remove(idx)?;
        let uuid = cmd.uuid.clone();
        drop(q);
        self.record(QueueOperation::Dequeue { uuid }).await;
        Some(cmd)
    }

    /// Pop the highest-priority queued command regardless of target, or `None`.
    pub async fn dequeue(&self) -> Option<QueuedCommand> {
        self.dequeue_filtered(|_| true).await
    }

    /// Pop the highest-priority MAIN-THREAD command (no `agent_id`), or `None`.
    /// The between-turn drain owner (the bridge run loop) uses this so subagent
    /// notifications stay scoped to their agent. Twin of
    /// `dequeue(c => c.agentId === undefined)`.
    pub async fn dequeue_main_thread(&self) -> Option<QueuedCommand> {
        self.dequeue_filtered(QueuedCommand::is_main_thread).await
    }

    /// Return (without removing) all commands at or above `threshold`,
    /// preserving priority+FIFO order, optionally narrowed by `filter`.
    ///
    /// Twin of `getCommandsByMaxPriority`: passing `Now` returns only `Now`
    /// items; `Later` returns everything (subject to the filter). The result
    /// holds clones; the caller passes the same `uuid`s back to [`Self::remove`]
    /// once consumed.
    pub async fn get_by_max_priority(
        &self,
        threshold: QueuePriority,
        filter: impl Fn(&QueuedCommand) -> bool,
    ) -> Vec<QueuedCommand> {
        let q = self.queue.read().await;
        let mut matched: Vec<(usize, QueuedCommand)> = Vec::new();
        for (i, c) in q.iter().enumerate() {
            if c.priority >= threshold && filter(c) {
                matched.push((i, c.clone()));
            }
        }
        // Highest priority first; FIFO (insertion index) within a level.
        matched.sort_by(|(ia, a), (ib, b)| b.priority.cmp(&a.priority).then(ia.cmp(ib)));
        matched.into_iter().map(|(_, c)| c).collect()
    }

    /// Return (without removing) the highest-priority command passing `filter`.
    /// Twin of `peek(filter?)`.
    pub async fn peek(&self, filter: impl Fn(&QueuedCommand) -> bool) -> Option<QueuedCommand> {
        let q = self.queue.read().await;
        let idx = Self::best_index(&q, &filter)?;
        q.get(idx).cloned()
    }

    /// Remove the commands with the given `uuids` from the queue, logging a
    /// `Remove` for each. Twin of `remove(commandsToRemove)` (which matches by
    /// reference identity; here we match by `uuid`). `reason` is recorded.
    ///
    /// Keep this for cancellation / prompt-restore. The 2.1.245 query loop
    /// folds absorbed commands via [`Self::consume`].
    pub async fn remove(&self, uuids: &[String], reason: &str) {
        if uuids.is_empty() {
            return;
        }
        {
            let mut q = self.queue.write().await;
            q.retain(|c| !uuids.contains(&c.uuid));
        }
        for uuid in uuids {
            self.record(QueueOperation::Remove {
                uuid: uuid.clone(),
                reason: reason.to_string(),
            })
            .await;
        }
    }

    /// Claude Code 2.1.245 `messageQueue.consume(commands)`: drop commands the
    /// query loop has already folded into the running / follow-up turn.
    ///
    /// Same splice and `"remove"` operation log as [`Self::remove`] — 2.1.245's
    /// shared helper still records `remove` for both. The JS split is the
    /// subscriber bus (`remove` → prompt-restore, `consume` → a separate bus
    /// so folded text is not bounced back into the TUI input). LingXi has no
    /// dual-bus subscriber yet; this is the named fold-into-turn path.
    pub async fn consume(&self, uuids: &[String], reason: &str) {
        self.remove(uuids, reason).await;
    }

    /// Pop EVERY queued command, logging a `Clear`. Twin of `dequeueAll` /
    /// `clearCommandQueue` (used by ESC cancellation to discard the queue).
    pub async fn clear(&self) -> Vec<QueuedCommand> {
        let drained: Vec<QueuedCommand> = {
            let mut q = self.queue.write().await;
            q.drain(..).collect()
        };
        if !drained.is_empty() {
            self.record(QueueOperation::Clear {
                count: drained.len(),
            })
            .await;
        }
        drained
    }

    /// Number of queued commands.
    pub async fn len(&self) -> usize {
        self.queue.read().await.len()
    }

    /// Whether the queue is empty.
    pub async fn is_empty(&self) -> bool {
        self.queue.read().await.is_empty()
    }

    /// Whether any MAIN-THREAD command is queued (drives the between-turn drain
    /// loop's continuation, twin of `getCommandQueue().some(...)`).
    pub async fn has_main_thread_commands(&self) -> bool {
        self.queue
            .read()
            .await
            .iter()
            .any(QueuedCommand::is_main_thread)
    }

    /// Snapshot the queue without consuming it. Useful for diagnostics.
    pub async fn snapshot(&self) -> Vec<QueuedCommand> {
        self.queue.read().await.iter().cloned().collect()
    }

    /// Wait up to `timeout` for an item, returning the best-priority one if any
    /// arrives.
    pub async fn wait_for_message(&self, timeout: std::time::Duration) -> Option<QueuedCommand> {
        tokio::select! {
            () = self.notify.notified() => self.dequeue().await,
            () = tokio::time::sleep(timeout) => None,
        }
    }

    /// Index of the highest-priority command passing `filter`, FIFO within a
    /// level. Shared by `dequeue_filtered` / `peek`.
    fn best_index(
        q: &VecDeque<QueuedCommand>,
        filter: &impl Fn(&QueuedCommand) -> bool,
    ) -> Option<usize> {
        let mut best: Option<(usize, QueuePriority)> = None;
        for (i, c) in q.iter().enumerate() {
            if !filter(c) {
                continue;
            }
            match best {
                // Strictly-greater keeps the FIRST item at the best level (FIFO).
                Some((_, p)) if c.priority <= p => {}
                _ => best = Some((i, c.priority)),
            }
        }
        best.map(|(i, _)| i)
    }

    async fn record(&self, op: QueueOperation) {
        // Clone the Arc and drop the read guard BEFORE awaiting so the lock is
        // not held across `record`'s await point (avoids blocking `set_recorder`
        // writers and prevents deadlock if a recorder ever becomes truly async).
        let rec_opt = {
            let r = self.recorder.read().await;
            r.as_ref().map(Arc::clone)
        };
        if let Some(rec) = rec_opt {
            rec.record(op).await;
        }
    }
}

/// Coalesce consecutive prompt-mode commands into ONE joined text payload, twin
/// of claude-code's `canBatchWith` / `joinPromptValues` (print.ts:1949-1961):
/// adjacent main-thread `UserInput` commands that are NOT slash commands are
/// merged into a single follow-up turn (their texts joined with `\n`).
///
/// Returns `(joined_text, consumed_uuids)`. `commands` must already be in
/// drain order. Stops at the first non-batchable command so a slash command or
/// a notification breaks the run (the caller handles those separately).
#[must_use]
pub fn join_prompt_values(commands: &[QueuedCommand]) -> Option<(String, Vec<String>)> {
    let mut texts = Vec::new();
    let mut uuids = Vec::new();
    for cmd in commands {
        if cmd.is_slash_command() {
            break;
        }
        match cmd.text() {
            Some(t) => {
                texts.push(t.to_string());
                uuids.push(cmd.uuid.clone());
            }
            None => break,
        }
    }
    if texts.is_empty() {
        None
    } else {
        Some((texts.join("\n"), uuids))
    }
}

impl Default for MessageQueueManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::VecRecorder;

    fn mk(priority: QueuePriority, text: &str) -> QueuedCommand {
        QueuedCommand {
            scheduled_task_id: None,
            scheduled_fire_id: None,
            uuid: text.into(),
            content: QueuedCommandContent::UserInput { text: text.into() },
            priority,
            queued_at: SystemTime::now(),
            source: QueueSource::PromptInput,
            agent_id: None,
            skip_slash_commands: false,
            is_meta: false,
        }
    }

    #[test]
    fn orphaned_permission_round_trips_through_serde() {
        // Lock the NEW (widened) OrphanedPermission shape: tool_use_id +
        // permission_decision_json + reason. Twin of claude-code's
        // `orphanedPermission.{permissionResult, …}` riding on the
        // `mode:'orphaned-permission'` command (print.ts:5291-5298).
        let cmd = QueuedCommand {
            scheduled_task_id: None,
            scheduled_fire_id: None,
            uuid: "u1".into(),
            content: QueuedCommandContent::OrphanedPermission {
                tool_use_id: ToolUseId::from("toolu_abc"),
                permission_decision_json: serde_json::json!({
                    "behavior": "allow",
                    "updatedInput": { "command": "ls -la" },
                }),
                reason: "orphaned control_response (no pending request)".into(),
            },
            priority: QueuePriority::Now,
            queued_at: SystemTime::now(),
            source: QueueSource::Orphan,
            agent_id: None,
            skip_slash_commands: false,
            is_meta: false,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: QueuedCommand = serde_json::from_str(&json).unwrap();
        match back.content {
            QueuedCommandContent::OrphanedPermission {
                tool_use_id,
                permission_decision_json,
                reason,
            } => {
                assert_eq!(tool_use_id.as_str(), "toolu_abc");
                assert_eq!(permission_decision_json["behavior"], "allow");
                assert_eq!(
                    permission_decision_json["updatedInput"]["command"],
                    "ls -la"
                );
                assert_eq!(reason, "orphaned control_response (no pending request)");
            }
            other => panic!("expected OrphanedPermission, got {other:?}"),
        }
        assert_eq!(back.source, QueueSource::Orphan);
    }

    #[test]
    fn orphaned_permission_serde_default_tolerates_missing_fields() {
        // The widened fields are `#[serde(default)]`, so a payload carrying only
        // `tool_use_id` still deserializes (forward/backward tolerance).
        let minimal = serde_json::json!({
            "uuid": "u2",
            "content": { "OrphanedPermission": { "tool_use_id": "toolu_min" } },
            "priority": "Now",
            "queued_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "source": "Orphan",
        });
        let back: QueuedCommand = serde_json::from_value(minimal).unwrap();
        match back.content {
            QueuedCommandContent::OrphanedPermission {
                tool_use_id,
                permission_decision_json,
                reason,
            } => {
                assert_eq!(tool_use_id.as_str(), "toolu_min");
                assert!(permission_decision_json.is_null());
                assert_eq!(reason, "");
            }
            other => panic!("expected OrphanedPermission, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn goal_retry_uses_host_queue_and_cancellation_removes_it() {
        let queue = MessageQueueManager::new();
        let cancel = CancellationToken::new();
        queue
            .enqueue_goal_retry("goal-retry-test".into(), "continue".into(), cancel.clone())
            .await;
        let queued = queue.snapshot().await;
        assert_eq!(queued.len(), 1);
        assert!(queued[0].is_meta);
        assert_eq!(queued[0].priority, QueuePriority::Later);
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !queue.snapshot().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn now_priority_dequeues_first() {
        let q = MessageQueueManager::new();
        q.enqueue(mk(QueuePriority::Later, "later")).await;
        q.enqueue(mk(QueuePriority::Now, "now")).await;
        q.enqueue(mk(QueuePriority::Next, "next")).await;
        assert_eq!(q.dequeue().await.unwrap().uuid, "now");
        assert_eq!(q.dequeue().await.unwrap().uuid, "next");
        assert_eq!(q.dequeue().await.unwrap().uuid, "later");
    }

    #[tokio::test]
    async fn fifo_within_priority_level() {
        // Parity with claude-code: append order preserved; best-priority FIFO.
        let q = MessageQueueManager::new();
        q.enqueue(mk(QueuePriority::Next, "a")).await;
        q.enqueue(mk(QueuePriority::Next, "b")).await;
        q.enqueue(mk(QueuePriority::Next, "c")).await;
        assert_eq!(q.dequeue().await.unwrap().uuid, "a");
        assert_eq!(q.dequeue().await.unwrap().uuid, "b");
        assert_eq!(q.dequeue().await.unwrap().uuid, "c");
    }

    #[tokio::test]
    async fn get_by_max_priority_threshold_and_order() {
        let q = MessageQueueManager::new();
        q.enqueue(mk(QueuePriority::Later, "l1")).await;
        q.enqueue(mk(QueuePriority::Now, "n1")).await;
        q.enqueue(mk(QueuePriority::Next, "x1")).await;
        q.enqueue(mk(QueuePriority::Now, "n2")).await;

        // threshold = Now ⇒ only the two Now items, FIFO.
        let now = q.get_by_max_priority(QueuePriority::Now, |_| true).await;
        assert_eq!(
            now.iter().map(|c| c.uuid.clone()).collect::<Vec<_>>(),
            vec!["n1", "n2"]
        );

        // threshold = Next ⇒ Now items then Next, never Later.
        let next = q.get_by_max_priority(QueuePriority::Next, |_| true).await;
        assert_eq!(
            next.iter().map(|c| c.uuid.clone()).collect::<Vec<_>>(),
            vec!["n1", "n2", "x1"]
        );

        // threshold = Later ⇒ everything, highest-first, FIFO within level.
        let all = q.get_by_max_priority(QueuePriority::Later, |_| true).await;
        assert_eq!(
            all.iter().map(|c| c.uuid.clone()).collect::<Vec<_>>(),
            vec!["n1", "n2", "x1", "l1"]
        );
    }

    #[tokio::test]
    async fn slash_commands_excluded_from_drain() {
        let q = MessageQueueManager::new();
        q.enqueue(mk(QueuePriority::Next, "/clear")).await;
        q.enqueue(mk(QueuePriority::Next, "real prompt")).await;
        // Filter out slash commands, matching the mid-turn drain gate.
        let drained = q
            .get_by_max_priority(QueuePriority::Next, |c| !c.is_slash_command())
            .await;
        assert_eq!(
            drained.iter().map(|c| c.uuid.clone()).collect::<Vec<_>>(),
            vec!["real prompt"]
        );
    }

    #[tokio::test]
    async fn skip_slash_commands_treats_slash_text_as_prompt() {
        let mut cmd = mk(QueuePriority::Next, "/not-a-command");
        cmd.skip_slash_commands = true;
        assert!(!cmd.is_slash_command());
    }

    /// Covers the main-thread FILTER, and nothing beyond it: the notification
    /// here is hand-built because no production code path constructs a
    /// [`QueuedCommandContent::TaskNotification`]. Read this as "the filter
    /// would scope correctly if something enqueued one", not as evidence that
    /// subagent notifications are routed — they are not (`AGT-05` / `TN-10`).
    #[tokio::test]
    async fn main_thread_filter_scopes_a_hand_built_subagent_notification() {
        let q = MessageQueueManager::new();
        let agent = AgentId::new();
        let sub = QueuedCommand {
            scheduled_task_id: None,
            scheduled_fire_id: None,
            uuid: "sub".into(),
            content: QueuedCommandContent::TaskNotification {
                value: "done".into(),
                mode: NotificationMode::TaskNotification,
            },
            priority: QueuePriority::Later,
            queued_at: SystemTime::now(),
            source: QueueSource::TaskCompletion,
            agent_id: Some(agent),
            skip_slash_commands: false,
            is_meta: true,
        };
        q.enqueue(sub).await;
        q.enqueue(mk(QueuePriority::Next, "main")).await;

        // Main-thread drain only sees the user input.
        assert_eq!(q.dequeue_main_thread().await.unwrap().uuid, "main");
        // The subagent notification is still queued, scoped to its agent.
        assert!(q.dequeue_main_thread().await.is_none());
        let snap = q.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert!(snap[0].is_task_notification_for(&agent));
    }

    #[tokio::test]
    async fn now_enqueue_aborts_active_turn() {
        let q = MessageQueueManager::new();
        let token = CancellationToken::new();
        q.register_active_turn(token.clone()).await;
        assert!(!token.is_cancelled());
        q.enqueue(mk(QueuePriority::Now, "urgent")).await;
        assert!(token.is_cancelled());

        // A non-Now enqueue after clearing the turn must NOT cancel.
        q.clear_active_turn().await;
        let token2 = CancellationToken::new();
        q.register_active_turn(token2.clone()).await;
        q.enqueue(mk(QueuePriority::Next, "calm")).await;
        assert!(!token2.is_cancelled());
    }

    #[tokio::test]
    async fn now_abort_hook_runs_before_token_fires() {
        use std::sync::atomic::{AtomicBool, Ordering as O};
        let q = MessageQueueManager::new();
        let token = CancellationToken::new();
        q.register_active_turn(token.clone()).await;
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = ran.clone();
        q.set_now_abort_hook(Arc::new(move || ran2.store(true, O::SeqCst)))
            .await;

        // A Next enqueue does NOT trigger the hook or the cancel.
        q.enqueue(mk(QueuePriority::Next, "calm")).await;
        assert!(!ran.load(O::SeqCst));
        assert!(!token.is_cancelled());

        // A Now enqueue runs the hook AND fires the token.
        q.enqueue(mk(QueuePriority::Now, "urgent")).await;
        assert!(ran.load(O::SeqCst), "hook must run on Now enqueue");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn operation_log_emitted_on_mutations() {
        let q = MessageQueueManager::new();
        let rec = Arc::new(VecRecorder::new());
        q.set_recorder(rec.clone()).await;

        q.enqueue(mk(QueuePriority::Next, "a")).await;
        q.dequeue().await;
        q.enqueue(mk(QueuePriority::Next, "b")).await;
        q.remove(&["b".into()], "test").await;

        let ops = rec.ops().await;
        assert!(matches!(ops[0], QueueOperation::Enqueue { .. }));
        assert!(matches!(ops[1], QueueOperation::Dequeue { .. }));
        assert!(matches!(ops[2], QueueOperation::Enqueue { .. }));
        assert!(matches!(ops[3], QueueOperation::Remove { .. }));
    }

    #[tokio::test]
    async fn consume_folds_commands_and_logs_remove() {
        // 2.1.245 `consume` shares the splice + `"remove"` operation log with
        // `remove`; only the subscriber bus differs in JS.
        let q = MessageQueueManager::new();
        let rec = Arc::new(VecRecorder::new());
        q.set_recorder(rec.clone()).await;

        q.enqueue(mk(QueuePriority::Next, "a")).await;
        q.enqueue(mk(QueuePriority::Next, "b")).await;
        q.consume(&["a".into()], "drained mid-turn into running turn")
            .await;

        assert_eq!(q.len().await, 1);
        let remaining = q.snapshot().await;
        assert_eq!(remaining[0].uuid, "b");
        let ops = rec.ops().await;
        match ops.last() {
            Some(QueueOperation::Remove { uuid, reason }) => {
                assert_eq!(uuid, "a");
                assert_eq!(reason, "drained mid-turn into running turn");
            }
            other => panic!("expected Remove, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_turn_prompt_consumes_only_next_main_thread_plain_input() {
        let q = MessageQueueManager::new();
        q.enqueue(mk(QueuePriority::Next, "first")).await;
        q.enqueue(mk(QueuePriority::Now, "urgent")).await;
        q.enqueue(mk(QueuePriority::Later, "later")).await;

        assert_eq!(q.take_mid_turn_prompt().await.as_deref(), Some("first"));
        let remaining = q.snapshot().await;
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].text(), Some("urgent"));
        assert_eq!(remaining[1].text(), Some("later"));
    }

    #[tokio::test]
    async fn join_prompt_values_batches_consecutive_prompts() {
        let cmds = vec![
            mk(QueuePriority::Next, "first"),
            mk(QueuePriority::Next, "second"),
        ];
        let (text, uuids) = join_prompt_values(&cmds).unwrap();
        assert_eq!(text, "first\nsecond");
        assert_eq!(uuids, vec!["first", "second"]);

        // A slash command breaks the batch.
        let cmds2 = vec![
            mk(QueuePriority::Next, "p"),
            mk(QueuePriority::Next, "/clear"),
        ];
        let (text2, uuids2) = join_prompt_values(&cmds2).unwrap();
        assert_eq!(text2, "p");
        assert_eq!(uuids2, vec!["p"]);
    }
}
