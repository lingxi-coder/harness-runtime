//! SH-02 — the 6-second-delayed `permission_prompt` `Notification` hook.
//!
//! NEW in claude-code 2.1.238 (`Cou`, oracle @ 178307264 / 307013328; the
//! literal `Claude needs your permission to use` and the env name both count 0
//! in 2.1.220, so this is upstream drift, not an old gap):
//!
//! ```js
//! function Cou(e){
//!   if(V.CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS)return()=>{};
//!   let t=setTimeout((r)=>{
//!     NY({id:zt(),project:{originalCwd:xn(),projectRoot:ul()}},
//!        {message:`Claude needs your permission to use ${r}`,
//!         notificationType:"permission_prompt"}).catch(()=>{})
//!   },A8n,e);
//!   t.unref();return()=>clearTimeout(t)}
//! ```
//!
//! with `A8n=6000` (oracle @ 290068323: `var q_=600000,C7a=30000,A8n=6000`).
//!
//! Both upstream call sites wrap a `can_use_tool` control request:
//! `A=Cou(KNe(t.name))` around the tool-permission round trip (oracle
//! @ 307031502) and `s=Cou(KNe(yvt))` around the sandbox network-ask callback
//! (@ 307035915) — in each case the returned disposer runs in a `finally`, so a
//! prompt answered inside 6 s fires nothing at all.
//!
//! The port's equivalent seam is `StdioControlPermissionGate::decide_outcome`
//! (`control_plane.rs`), which is the one place that emits a `can_use_tool`
//! request and blocks on the host's `control_response`.

use futures::future::BoxFuture;
use std::sync::Arc;
use std::time::Duration;

/// Byte-faithful `notification_type` discriminator (`notificationType:
/// "permission_prompt"`). 2.1.220 already carried this token in its
/// notification-type union, but had NO construction site — 2.1.238 is the first
/// build that can actually fire it.
pub const PERMISSION_PROMPT_NOTIFICATION_TYPE: &str = "permission_prompt";

/// `A8n = 6000` — how long a permission prompt must stay unanswered before the
/// `Notification` hook fires (oracle 2.1.238 @ 290068323).
pub const PERMISSION_PROMPT_NOTIFY_DELAY_MS: u64 = 6_000;

/// Env kill-switch (`V.CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS`).
///
/// Upstream reads it with RAW JS truthiness — `if (V.NAME)` — so ANY non-empty
/// value disables the notification, including `"0"` and `"false"`. This is NOT
/// the strict `isEnvTruthy` predicate the port uses elsewhere, and using the
/// strict one here would leave the switch on for a user who set it to `0`.
/// Both spellings are honoured, `LINGXI_` first then the upstream
/// `CLAUDE_CODE_` name — the same convention `tools::shell::bash`'s
/// `LINGXI_GIT_BASH_PATH` / `CLAUDE_CODE_GIT_BASH_PATH` override uses.
pub const DISABLE_ENV_VARS: [&str; 2] = [
    "LINGXI_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS",
    "CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS",
];

/// Build the byte-faithful message: `` `Claude needs your permission to use ${r}` ``
/// where `r` is `KNe(tool_name)` — the same display name the `can_use_tool`
/// request carries in its `display_name` field.
#[must_use]
pub fn permission_prompt_message(display_name: &str) -> String {
    format!("Claude needs your permission to use {display_name}")
}

/// `if (V.CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS) return () => {}`.
#[must_use]
pub fn notifications_disabled_by_env(
    host: &dyn crate::headless::host::HeadlessHostServices,
) -> bool {
    DISABLE_ENV_VARS.iter().any(|var| {
        host.environment_variable(var)
            .is_some_and(|v| !v.is_empty())
    })
}

/// Injectable seam so the gate can be unit-tested without a real orchestrator
/// or a real 6-second sleep.
pub trait PermissionPromptNotifier: Send + Sync {
    /// Whether this connection disabled the delayed notification hook.
    fn disabled(&self) -> bool {
        false
    }

    /// Fire the `Notification` hook for a still-pending permission prompt.
    /// Best-effort: it must never affect the permission decision.
    fn fire(&self, display_name: String) -> BoxFuture<'static, ()>;
}

/// Production notifier: fires
/// [`orchestrator::ConversationOrchestrator::fire_notification`] with the
/// byte-faithful message + `permission_prompt` type.
pub struct OrchestratorPermissionPromptNotifier {
    orch: Arc<orchestrator::ConversationOrchestrator>,
    host: Arc<dyn crate::headless::host::HeadlessHostServices>,
}

impl OrchestratorPermissionPromptNotifier {
    /// Build a notifier over the concrete orchestrator.
    #[must_use]
    pub fn new(
        orch: Arc<orchestrator::ConversationOrchestrator>,
        host: Arc<dyn crate::headless::host::HeadlessHostServices>,
    ) -> Self {
        Self { orch, host }
    }
}

impl PermissionPromptNotifier for OrchestratorPermissionPromptNotifier {
    fn disabled(&self) -> bool {
        notifications_disabled_by_env(self.host.as_ref())
    }

    fn fire(&self, display_name: String) -> BoxFuture<'static, ()> {
        let orch = self.orch.clone();
        Box::pin(async move {
            orch.fire_notification(
                &permission_prompt_message(&display_name),
                PERMISSION_PROMPT_NOTIFICATION_TYPE,
            )
            .await;
        })
    }
}

/// The armed timer `Cou` returns. Dropping it (or calling
/// [`Self::cancel`]) is `clearTimeout` — the notification never fires.
pub struct PermissionPromptNotifyGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl PermissionPromptNotifyGuard {
    /// `Cou(display_name)` — arm the 6 s timer, or return an inert guard when
    /// the env kill-switch is set or no notifier is wired.
    ///
    /// The timer task is detached (upstream's `t.unref()`), so it can never keep
    /// the process alive; the guard's `Drop` aborts it, which is the disposer
    /// upstream runs in its `finally`.
    #[must_use]
    pub fn arm(notifier: Option<&Arc<dyn PermissionPromptNotifier>>, display_name: &str) -> Self {
        let Some(notifier) = notifier else {
            return Self { handle: None };
        };
        if notifier.disabled() {
            return Self { handle: None };
        }
        let notifier = notifier.clone();
        let display_name = display_name.to_string();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(PERMISSION_PROMPT_NOTIFY_DELAY_MS)).await;
            notifier.fire(display_name).await;
        });
        Self {
            handle: Some(handle),
        }
    }

    /// Explicit `clearTimeout`. `Drop` does the same, so this exists only to
    /// make the `finally` position readable at the call site.
    pub fn cancel(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Drop for PermissionPromptNotifyGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message is byte-faithful to the oracle template
    /// `` `Claude needs your permission to use ${r}` ``.
    #[test]
    fn message_is_byte_faithful() {
        assert_eq!(
            permission_prompt_message("Bash"),
            "Claude needs your permission to use Bash"
        );
    }

    /// `A8n = 6000`.
    #[test]
    fn delay_matches_the_oracle_constant() {
        assert_eq!(PERMISSION_PROMPT_NOTIFY_DELAY_MS, 6_000);
    }

    /// An unwired notifier arms nothing — a host with no orchestrator seam must
    /// not pay for a task per permission prompt.
    #[test]
    fn arm_without_a_notifier_is_inert() {
        let guard = PermissionPromptNotifyGuard::arm(None, "Bash");
        assert!(guard.handle.is_none());
    }
}
