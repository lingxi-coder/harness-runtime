//! [`ForkResumeGate`] — the seam that lets a host refuse to resume a forked
//! skill whose permission scoping cannot be re-established.
//!
//! A `context: fork` skill runs as a background subagent under the SKILL's
//! allow/deny lists rather than the parent's. Those lists are not derivable
//! from the transcript, so they are persisted beside it at launch
//! (`session::forked_skill`). Resuming such an agent without them would run it
//! under the parent's permissions, which is strictly wider than what the fork
//! was granted — a silent privilege widening.
//!
//! The check therefore lives on the RESUME path, and every ambiguous state
//! fails closed. It is a seam rather than a direct call because the pieces it
//! needs — the session directory, the skill registry — live at the composition
//! root, while the resume itself happens down in the task handler.
//!
//! An unwired host has no gate, which is correct for a host that also has no
//! way to launch a forked skill.

use async_trait::async_trait;

/// Consulted before a parked background agent is resumed.
#[async_trait]
pub trait ForkResumeGate: Send + Sync {
    /// Decide whether the agent behind `agent_id` may be resumed.
    ///
    /// `task_forked_skill_name` is the fork identity the LIVE task record
    /// carries, when it has one — the "hot" corroboration path. `None` means
    /// the record names no skill, and the on-disk provenance marker becomes the
    /// only witness.
    ///
    /// # Errors
    /// The refusal message, ready to surface. Returning `Ok(())` permits the
    /// resume, which is also the right answer for an agent that never forked.
    async fn check_resume(
        &self,
        agent_id: protocol::AgentId,
        task_forked_skill_name: Option<&str>,
    ) -> Result<(), String>;
}
