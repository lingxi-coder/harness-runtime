//! Single source of truth for which `SwarmBackend` the POSIX platform uses.
//!
//! Detection runs once at construction time. Re-detecting mid-session is
//! intentionally not supported — claude-code caches its choice for the
//! lifetime of the process (`registry.ts:26,140-145`) for the same reason:
//! the environment doesn't change while Claude is running.

use std::sync::OnceLock;

use platform_api::SwarmBackend;

use super::detection::{
    detect_terminal_env, pick_backend, select_backend, BackendChoice, TeammateMode, TerminalEnv,
};
use super::inprocess::InProcessSwarmBackend;
use super::iterm::ITermSwarmBackend;
use super::tmux::TmuxBackend;

/// Process-level cache for the detected backend choice. Lets tests use
/// `detect_and_construct_with()` to override.
static CHOICE_CACHE: OnceLock<BackendChoice> = OnceLock::new();

/// Construct the appropriate `Box<dyn SwarmBackend>` based on the host
/// environment. First call probes; subsequent calls reuse the cached choice.
pub struct SwarmRegistry;

impl SwarmRegistry {
    /// Probe env + tools and return a freshly-constructed backend. Caches the
    /// detection result for the process lifetime.
    #[must_use]
    pub fn detect_and_construct() -> Box<dyn SwarmBackend> {
        let choice = *CHOICE_CACHE.get_or_init(|| pick_backend(&detect_terminal_env()));
        Self::construct(choice)
    }

    /// Force a specific backend choice (test hook). Does not touch the cache.
    #[must_use]
    pub fn detect_and_construct_with(env: &TerminalEnv) -> Box<dyn SwarmBackend> {
        let choice = pick_backend(env);
        Self::construct(choice)
    }

    /// Resolve a caller's session settings without using the default-mode cache.
    pub fn construct_for_mode(
        env: &TerminalEnv,
        mode: TeammateMode,
        interactive: bool,
        prefer_tmux: bool,
    ) -> Result<Box<dyn SwarmBackend>, &'static str> {
        select_backend(env, mode, interactive, prefer_tmux).map(Self::construct)
    }

    fn construct(choice: BackendChoice) -> Box<dyn SwarmBackend> {
        match choice {
            BackendChoice::Tmux => Box::new(TmuxBackend::new()),
            BackendChoice::ITerm => Box::new(ITermSwarmBackend::new()),
            BackendChoice::InProcess => Box::new(InProcessSwarmBackend::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_with_inprocess_env() {
        let env = TerminalEnv {
            inside_tmux: false,
            iterm_app: false,
            tmux_available: false,
            it2_available: false,
        };
        let backend = SwarmRegistry::detect_and_construct_with(&env);
        assert!(backend.is_available()); // InProcess is always available
    }
}
