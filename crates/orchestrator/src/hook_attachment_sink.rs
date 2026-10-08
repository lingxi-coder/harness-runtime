//! Transcript sink for per-hook-run `attachment` records.
//!
//! claude-code persists exactly ONE `type:"attachment"` transcript line for
//! every hook run. The hooks crate builds the payload
//! ([`hooks::attachment`]); this adapter is the persistence half — it forwards
//! each payload to
//! [`ConversationOrchestrator::persist_hook_attachment_to_jsonl`].
//!
//! # Why a late-bound cell
//!
//! The composition root builds the [`hooks::HookExecutorImpl`] BEFORE the
//! orchestrator exists (the executor is one of the orchestrator's
//! constructor arguments), so the sink cannot be handed a live orchestrator at
//! construction time. It is created empty, wired onto the executor, and
//! [`JsonlHookAttachmentSink::attach`]ed once the orchestrator is built — the
//! same "fill the cell afterwards" shape the subagent spawner's
//! hook-executor handle already uses.
//!
//! The cell holds a [`Weak`] on purpose: the orchestrator owns the hook
//! executor, which owns this sink, so a strong handle back would close a
//! reference cycle and leak the orchestrator for the process's lifetime.

use crate::conversation::ConversationOrchestrator;
use async_trait::async_trait;
use hooks::attachment::PersistedHookOutput;
use hooks::ExactHookText;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::{Arc, OnceLock, Weak};

/// Persists hook-run attachments to the session transcript.
#[derive(Debug, Default)]
pub struct JsonlHookAttachmentSink {
    orch: OnceLock<Weak<ConversationOrchestrator>>,
}

impl JsonlHookAttachmentSink {
    /// Build an unattached sink. Recording is an inert no-op until
    /// [`Self::attach`] runs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            orch: OnceLock::new(),
        }
    }

    /// Bind the sink to the orchestrator that owns the transcript writer.
    ///
    /// First call wins; later calls are ignored, so a re-composed engine
    /// cannot silently retarget an in-flight sink.
    pub fn attach(&self, orch: &Arc<ConversationOrchestrator>) {
        let _ = self.orch.set(Arc::downgrade(orch));
    }
}

#[async_trait]
impl hooks::HookAttachmentSink for JsonlHookAttachmentSink {
    async fn record(&self, attachment: Utf16JsonProjection) {
        // Unattached, or the orchestrator has been dropped (engine shutdown):
        // nothing to persist to.
        let Some(orch) = self.orch.get().and_then(Weak::upgrade) else {
            return;
        };
        if !attachment.keys.is_empty() || attachment.validate().is_err() {
            tracing::warn!("discarding invalid exact hook attachment projection");
            return;
        }
        let utf16_overrides = attachment
            .strings
            .iter()
            .map(|sidecar| {
                (
                    format!("/attachment{}", sidecar.pointer),
                    sidecar.code_units.clone(),
                )
            })
            .collect();
        orch.persist_hook_attachment_to_jsonl(attachment.value, utf16_overrides)
            .await;
    }

    async fn persist_large_output(
        &self,
        text: &ExactHookText,
    ) -> Result<PersistedHookOutput, String> {
        let orch = self
            .orch
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "tool result was not saved".to_string())?;
        orch.persist_large_hook_output(text).await
    }
}
