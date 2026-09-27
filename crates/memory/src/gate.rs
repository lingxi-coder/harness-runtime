//! The auto-memory feature gate — port of claude-code `ra()` / `dLt()`
//! (2.1.270 `src_166572870.js`).
//!
//! ```js
//! function ra(){ if(Zy())return!1; return dLt() }
//! function dLt(){
//!   if(Ar())return!1;
//!   if(FA())return!1;
//!   let e=process.env.CLAUDE_CODE_DISABLE_AUTO_MEMORY;
//!   if(Ie(e))return!1;           // truthy  -> disabled
//!   if(fo(e))return!0;           // falsy   -> force ENABLED
//!   if(a.CLAUDE_CODE_SIMPLE)return!1;
//!   if(a.CLAUDE_CODE_REMOTE && !CLAUDE_CODE_REMOTE_MEMORY_DIR && !CLAUDE_COWORK_MEMORY_PATH_OVERRIDE)return!1;
//!   if(EIe())return!1;
//!   let n=Ge();
//!   if(n.autoMemoryEnabled!==void 0)return n.autoMemoryEnabled;
//!   return!0                     // DEFAULT ON
//! }
//! ```
//!
//! 🚨 The port previously gated the `# Memory` prompt section and the memdir
//! prefetch on `LINGXI_MEMDIR_PREFETCH` and attributed that to the flag
//! `tengu_moth_copse`. That attribution was WRONG: at the oracle
//! `tengu_moth_copse` (`X$()`) guards `CLAUDE_MEMORY_STORES` — the memory-stores
//! feature — and has nothing to do with auto-memory. The real gate is the one
//! above, whose last statement is `return!0`. So the port shipped the feature
//! OFF by default on a mis-mapped flag.
//!
//! Env is read at the EDGE ([`auto_memory_env`]) and the decision itself is a
//! pure function of its inputs, so tests never need `set_var` — an env-reading
//! gate plus `set_var` makes the parallel suite flake, and the failure looks
//! like another session's fault.

/// The env inputs `dLt()` consults, captured once at the composition root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoMemoryEnv {
    /// `CLAUDE_CODE_DISABLE_AUTO_MEMORY` / `LINGXI_DISABLE_AUTO_MEMORY`, as
    /// written. `None` = unset.
    ///
    /// Three-valued at the oracle: truthy DISABLES, **explicitly falsy FORCES
    /// ON** (overriding the settings key below), unset falls through.
    pub disable_auto_memory: Option<String>,
    /// `CLAUDE_CODE_SIMPLE` / `LINGXI_SIMPLE` set to anything non-empty.
    pub simple_mode: bool,
}

impl AutoMemoryEnv {
    /// Read the gate's env inputs from the process environment.
    ///
    /// Call this ONCE, at the composition root, and pass the result down.
    #[must_use]
    pub fn from_process_env() -> Self {
        let first_set = |names: [&str; 2]| -> Option<String> {
            names
                .into_iter()
                .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
        };
        Self {
            disable_auto_memory: first_set([
                "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
                "LINGXI_DISABLE_AUTO_MEMORY",
            ]),
            simple_mode: first_set(["CLAUDE_CODE_SIMPLE", "LINGXI_SIMPLE"]).is_some(),
        }
    }
}

fn is_truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

fn is_falsy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    matches!(v.as_str(), "0" | "false" | "no" | "off")
}

/// Is auto-memory active? Port of `dLt()`.
///
/// `settings_enabled` is the `autoMemoryEnabled` settings key (`None` = unset).
///
/// Precedence, highest first:
/// 1. `disable_auto_memory` truthy  → OFF
/// 2. `disable_auto_memory` falsy   → ON (beats the settings key, as upstream)
/// 3. `simple_mode`                 → OFF
/// 4. `settings_enabled`            → whatever it says
/// 5. otherwise                     → **ON**
#[must_use]
pub fn auto_memory_enabled(env: &AutoMemoryEnv, settings_enabled: Option<bool>) -> bool {
    if let Some(raw) = env.disable_auto_memory.as_deref() {
        if is_truthy(raw) {
            return false;
        }
        if is_falsy(raw) {
            return true;
        }
    }
    if env.simple_mode {
        return false;
    }
    settings_enabled.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(disable: Option<&str>, simple: bool) -> AutoMemoryEnv {
        AutoMemoryEnv {
            disable_auto_memory: disable.map(ToOwned::to_owned),
            simple_mode: simple,
        }
    }

    /// The whole point of the fix: nothing set ⇒ ON. `dLt()` ends `return!0`.
    #[test]
    fn a_clean_environment_enables_auto_memory() {
        assert!(auto_memory_enabled(&env(None, false), None));
    }

    #[test]
    fn the_killswitch_disables_it() {
        for raw in ["1", "true", "YES", " on "] {
            assert!(
                !auto_memory_enabled(&env(Some(raw), false), None),
                "{raw:?} must disable"
            );
        }
    }

    /// `if(fo(e))return!0` — an explicitly FALSY killswitch forces the feature
    /// on, ahead of the settings key. Dropping this arm would make
    /// `DISABLE_AUTO_MEMORY=0` plus `autoMemoryEnabled:false` resolve OFF, where
    /// upstream resolves ON.
    #[test]
    fn an_explicitly_falsy_killswitch_forces_it_on_over_the_setting() {
        for raw in ["0", "false", "OFF"] {
            assert!(
                auto_memory_enabled(&env(Some(raw), false), Some(false)),
                "{raw:?} must force ON"
            );
        }
    }

    #[test]
    fn simple_mode_disables_it() {
        assert!(!auto_memory_enabled(&env(None, true), None));
        assert!(!auto_memory_enabled(&env(None, true), Some(true)));
    }

    #[test]
    fn the_settings_key_decides_when_no_env_applies() {
        assert!(!auto_memory_enabled(&env(None, false), Some(false)));
        assert!(auto_memory_enabled(&env(None, false), Some(true)));
    }

    /// An unrecognised killswitch value is neither truthy nor falsy, so it falls
    /// through rather than being read as "disable" — matching `Ie`/`fo`.
    #[test]
    fn an_unrecognised_killswitch_value_falls_through() {
        assert!(auto_memory_enabled(&env(Some("maybe"), false), None));
        assert!(!auto_memory_enabled(
            &env(Some("maybe"), false),
            Some(false)
        ));
    }
}
