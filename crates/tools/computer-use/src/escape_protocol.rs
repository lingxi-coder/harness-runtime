//! Foreground-only Escape lifetime, matching the recovered CLI host wrapper.
use super::*;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct EscapeCalls {
    next_id: u64,
    calls: HashMap<u64, (String, Option<CancellationToken>)>,
}

pub(super) struct CallFlight {
    count: Arc<AtomicUsize>,
    calls: Arc<Mutex<EscapeCalls>>,
    id: u64,
}
impl CallFlight {
    pub(super) fn begin(
        count: &Arc<AtomicUsize>,
        calls: &Arc<Mutex<EscapeCalls>>,
        owner: String,
        cancel: Option<CancellationToken>,
    ) -> Self {
        let mut state = calls.lock().unwrap_or_else(|error| error.into_inner());
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;
        state.calls.insert(id, (owner, cancel));
        count.fetch_add(1, Ordering::SeqCst);
        Self {
            count: count.clone(),
            calls: calls.clone(),
            id,
        }
    }
}
impl Drop for CallFlight {
    fn drop(&mut self) {
        let mut state = self.calls.lock().unwrap_or_else(|error| error.into_inner());
        state.calls.remove(&self.id);
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ComputerTool {
    pub(super) async fn ensure_escape_registration(
        &self,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<(), ToolError> {
        let owner = Self::owner(ctx).await;
        // Backend registration is synchronous. Serialize it with removal so
        // cleanup cannot unregister just before a pending install takes effect.
        let _lifecycle = self
            .escape_lifecycle
            .lock()
            .map_err(|_| ToolError::Internal("computer Escape lifecycle poisoned".into()))?;
        {
            let mut protocol = self
                .protocol
                .lock()
                .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?;
            if protocol.escape_active {
                return Ok(());
            }
            protocol.escape_active = true;
            protocol.escape_owner = Some(owner.clone());
            protocol.exit_progress = ctx.tool_use_id.clone().map(|id| (id, tx.clone()));
        }
        let calls = self.calls_in_flight.clone();
        let active_calls = self.escape_calls.clone();
        let aborted = self.escape_aborted.clone();
        let generation = self.escape_generation.clone();
        let installed_generation = generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let registered = self.ctx.computer_control.as_ref().is_some_and(|backend| {
            backend.register_escape_abort(Arc::new(move || {
                if generation.load(Ordering::SeqCst) != installed_generation
                    || calls.load(Ordering::SeqCst) == 0
                {
                    return;
                }
                let state = active_calls
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if generation.load(Ordering::SeqCst) != installed_generation {
                    return;
                }
                let owned = state
                    .calls
                    .values()
                    .filter(|(call_owner, _)| call_owner == &owner)
                    .map(|(_, token)| token.clone())
                    .collect::<Vec<_>>();
                if owned.is_empty() {
                    return;
                }
                aborted.store(true, Ordering::SeqCst);
                drop(state);
                for token in owned.into_iter().flatten() {
                    token.cancel();
                }
            }))
        });
        if let Some(id) = &ctx.tool_use_id {
            let message = if registered {
                NOTIFY_ENTER_ESC
            } else {
                NOTIFY_ENTER_CTRL_C
            };
            let _ = tx.try_send(tool_api::progress::ToolProgress {
                tool_use_id: id.clone(),
                data: tool_api::tool_trait::generated_builtin_result(&json!({
                    "type":"computer_use_enter", "message":message.replacen("Claude", branding::PRODUCT_NAME, 1)
                })),
            });
        }
        Ok(())
    }

    pub(super) async fn stop_escape_registration(
        &self,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let owner = Self::owner(ctx).await;
        let _lifecycle = self
            .escape_lifecycle
            .lock()
            .map_err(|_| ToolError::Internal("computer Escape lifecycle poisoned".into()))?;
        let (active, progress) = {
            let mut protocol = self
                .protocol
                .lock()
                .map_err(|_| ToolError::Internal("computer protocol state poisoned".into()))?;
            if protocol.escape_owner.as_ref() != Some(&owner) {
                return Ok(());
            }
            let active = protocol.escape_active;
            protocol.escape_active = false;
            protocol.escape_owner = None;
            (active, protocol.exit_progress.take())
        };
        // Retired callbacks can already be queued by the OS event source.
        // Invalidate them before removal, under the same call registry lock
        // used by callbacks, so they cannot relatch aborted after cleanup.
        {
            let _calls = self
                .escape_calls
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.escape_generation.fetch_add(1, Ordering::SeqCst);
            self.escape_aborted.store(false, Ordering::SeqCst);
        }
        if active {
            if let Some(backend) = &self.ctx.computer_control {
                backend.unregister_escape_abort();
            }
        }
        if let Some((id, tx)) = progress {
            let _ = tx.try_send(tool_api::progress::ToolProgress {
                tool_use_id: id,
                data: tool_api::tool_trait::generated_builtin_result(&json!({
                    "type":"computer_use_exit", "message":NOTIFY_EXIT.replacen("Claude", branding::PRODUCT_NAME, 1)
                })),
            });
        }
        Ok(())
    }
}
