//! The "agent view" enablement gate — the boot-time flag that decides which
//! `/fork` variant (and whether `/subtask`) the command list registers.
//!
//! Port of claude-code 2.1.212 `vO()` and its helper chain (binary parity
//! G05/G19). The command list `Blr` selects the fork/subtask surface with:
//!
//! ```text
//! ...vO() && !Gt(Z.IS_DEMO) ? [vAd, RAd] : [SAd]
//! vO()  = !Hzt()
//! Hzt() = I2i() !== null
//! I2i() = if Gt(process.env.CLAUDE_CODE_DISABLE_AGENT_VIEW) -> "is disabled by CLAUDE_CODE_DISABLE_AGENT_VIEW"
//!         if settings.disableAgentView === true          -> "is disabled by the 'disableAgentView' setting"
//!         else                                            -> null
//! ```
//!
//! So agent view is **enabled** (the default) unless the
//! `CLAUDE_CODE_DISABLE_AGENT_VIEW` env var is truthy OR the `disableAgentView`
//! setting is `true`. When enabled, the command list registers the redefined
//! background-session-copy `/fork` (`vAd`) plus `/subtask` (`RAd`); when
//! disabled, it falls back to the legacy subagent-spawn `/fork` (`SAd`) and no
//! `/subtask`.
//!
//! **Wired here:** the `CLAUDE_CODE_DISABLE_AGENT_VIEW` env half of `I2i()`.
//! The `settings.disableAgentView === true` half is a documented seam — the
//! schema key round-trips via the engine settings model; composition roots
//! that resolve it at boot pass the combined result to
//! `command_core::register_core_batch_8` (mirroring the `enableArtifact`
//! Stage-2 settings seam in `tool-api::artifact_gate`).

use crate::env::is_env_truthy;

/// `CLAUDE_CODE_DISABLE_AGENT_VIEW` — the env override half of `I2i()`.
pub const DISABLE_AGENT_VIEW_ENV: &str = "CLAUDE_CODE_DISABLE_AGENT_VIEW";

/// `vO()` — is agent view enabled? Wires the env half of `I2i()`; a caller that
/// has resolved the `disableAgentView` setting should AND its own result via
/// [`is_enabled_with_setting`].
///
/// Returns `true` (the default) unless `CLAUDE_CODE_DISABLE_AGENT_VIEW` is
/// truthy.
#[must_use]
pub fn is_enabled() -> bool {
    !disabled_by_env()
}

/// `vO()` combining the env half with a resolved `disableAgentView` setting
/// value (`settings.disableAgentView === true`). Composition roots that read
/// the setting call this; `disable_agent_view_setting == true` disables agent
/// view exactly like the env var.
#[must_use]
pub fn is_enabled_with_setting(disable_agent_view_setting: bool) -> bool {
    !(disabled_by_env() || disable_agent_view_setting)
}

/// `Gt(process.env.CLAUDE_CODE_DISABLE_AGENT_VIEW)` — the env half of `I2i()`.
fn disabled_by_env() -> bool {
    is_env_truthy(std::env::var(DISABLE_AGENT_VIEW_ENV).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env mutation across the gate tests (shared process env).
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DISABLE_AGENT_VIEW_ENV);
        g
    }

    #[test]
    fn enabled_by_default() {
        let _g = guard();
        assert!(is_enabled());
        assert!(is_enabled_with_setting(false));
    }

    #[test]
    fn env_disable_wins() {
        let _g = guard();
        std::env::set_var(DISABLE_AGENT_VIEW_ENV, "1");
        assert!(!is_enabled());
        // env disable dominates even when the setting is false.
        assert!(!is_enabled_with_setting(false));
    }

    #[test]
    fn setting_disable_wins() {
        let _g = guard();
        assert!(!is_enabled_with_setting(true));
    }

    #[test]
    fn empty_env_is_not_truthy() {
        let _g = guard();
        std::env::set_var(DISABLE_AGENT_VIEW_ENV, "");
        assert!(is_enabled());
    }
}
