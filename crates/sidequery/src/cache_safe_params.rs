//! Shared snapshot of the parent agent's cache-safe prompt prefix.
//!
//! After every successful main-loop API call the host writes a
//! [`CacheSafeParams`] into the shared [`CacheSafeParamsSlot`]. Forked
//! agents read the latest snapshot and serialize their prompt with the same
//! byte layout, allowing Anthropic's prompt cache to hit on the prefix.
//!
//! `generation` is a monotonically-increasing tag the slot stamps on every
//! save. Forks capture the generation they read; producers that want to
//! avoid clobbering a newer snapshot use [`CacheSafeParamsSlot::save_if_generation_matches`]
//! (see spec §B10 anti-stale rule).

use lingxi_core::types::ConversationMessage;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Snapshot of every input the main loop must reproduce byte-for-byte for the
/// forked agent to share its prompt cache.
///
/// `Debug` is intentionally not derived: [`tool_api::ToolUseOptions`]
/// only implements `Clone` today, and tunneling debug formatting through
/// here would print MCP connection ids that may contain user secrets.
#[derive(Clone)]
pub struct CacheSafeParams {
    /// Frozen system prompt (`Arc<str>` so cheap to clone into a fork).
    pub system_prompt: Arc<str>,
    /// User-tier context bindings (env, mounted files, ...).
    pub user_context: HashMap<String, String>,
    /// Engine-tier context bindings (model id, working dir, ...).
    pub system_context: HashMap<String, String>,
    /// The transient `additionalContext` user message that Claude prepends to
    /// every model request. It is kept outside `fork_context_messages` so the
    /// cacheable prefix remains byte-identical, then replayed immediately
    /// before that prefix by forked-agent callers.
    pub user_context_message: Option<ConversationMessage>,
    /// Tool-set options the parent rendered in this turn.
    pub tool_use_options: tool_api::ToolUseOptions,
    /// Tool schemas sent with the parent's prompt, in their original order.
    /// Compaction replays these even though it never executes tool calls.
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Effective output effort used by the parent's model request.
    pub effort: Option<serde_json::Value>,
    /// Conversation prefix forks must replay verbatim.
    pub fork_context_messages: Vec<ConversationMessage>,
    /// Main-session transcript used by compact continuation messages to point
    /// the resumed model at the complete pre-compact record.
    pub transcript_path: Option<PathBuf>,
    /// Slot-assigned generation tag. Producers should treat this as
    /// read-only — [`CacheSafeParamsSlot::save`] overwrites it on insert.
    pub generation: u64,
}

/// Single-slot store for the latest [`CacheSafeParams`] snapshot.
///
/// Concurrency model: writers and readers contend on a single `RwLock`,
/// but readers clone-out so they don't hold the lock during the (potentially
/// long-running) fork call. The atomic generation counter is incremented
/// exactly once per successful save.
pub struct CacheSafeParamsSlot {
    last: RwLock<Option<CacheSafeParams>>,
    next_generation: AtomicU64,
}

impl CacheSafeParamsSlot {
    /// Empty slot. The first [`save`] will assign generation = 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: RwLock::new(None),
            next_generation: AtomicU64::new(1),
        }
    }

    /// Unconditional write. Assigns a fresh generation tag and stores.
    pub async fn save(&self, mut params: CacheSafeParams) {
        params.generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        *self.last.write().await = Some(params);
    }

    /// Returns a clone of the latest snapshot, or `None` if nothing has
    /// been saved yet.
    pub async fn get_last(&self) -> Option<CacheSafeParams> {
        self.last.read().await.clone()
    }

    /// Conditional save: only writes when the slot's current generation
    /// equals `expected`. Used by callers that read, transform, then write
    /// back, and want to abort on a concurrent newer write (spec §B10
    /// anti-stale rule).
    ///
    /// # Errors
    ///
    /// Returns `Err("stale save: generation mismatch")` when the slot's
    /// current generation no longer matches `expected`.
    pub async fn save_if_generation_matches(
        &self,
        expected: u64,
        mut params: CacheSafeParams,
    ) -> Result<(), &'static str> {
        let mut slot = self.last.write().await;
        if slot.as_ref().map(|p| p.generation) != Some(expected) {
            return Err("stale save: generation mismatch");
        }
        params.generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        *slot = Some(params);
        Ok(())
    }
}

impl Default for CacheSafeParamsSlot {
    fn default() -> Self {
        Self::new()
    }
}
