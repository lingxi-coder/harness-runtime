//! Product auto-memory gate shared by prompt injection and memory prefetch.
//!
//! `LINGXI_DISABLE_AUTO_MEMORY` and `LINGXI_SIMPLE` disable memory when true.
//! False flags leave the `autoMemoryEnabled` setting in control. With no flag
//! or setting the feature is enabled.
//!
//! Environment is captured once at the composition root. The decision remains
//! a pure function so ordinary tests do not mutate process-global variables.

/// Product environment inputs captured once at the composition root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoMemoryEnv {
    /// `LINGXI_DISABLE_AUTO_MEMORY`, as written. `None` = unset.
    /// `1`, `true`, `yes`, or `on` disable memory.
    /// A false flag leaves the simple-mode and settings gates in control.
    pub disable_auto_memory: Option<String>,
    /// Whether `LINGXI_SIMPLE` is truthy.
    pub simple_mode: bool,
}

impl AutoMemoryEnv {
    /// Read the gate's env inputs from the process environment.
    ///
    /// Call this ONCE, at the composition root, and pass the result down.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            disable_auto_memory: lookup("LINGXI_DISABLE_AUTO_MEMORY")
                .filter(|value| !value.trim().is_empty()),
            simple_mode: lingxi_core::host::env::is_env_truthy(lookup("LINGXI_SIMPLE").as_deref()),
        }
    }
}

/// Is auto-memory active? Product flags use the same semantics as agent tools.
///
/// `settings_enabled` is the `autoMemoryEnabled` settings key (`None` = unset).
///
/// Precedence, highest first:
/// 1. `disable_auto_memory` truthy  → OFF
/// 2. `simple_mode`                 → OFF
/// 3. `settings_enabled`            → whatever it says
/// 4. otherwise                     → **ON**
#[must_use]
pub fn auto_memory_enabled(env: &AutoMemoryEnv, settings_enabled: Option<bool>) -> bool {
    if let Some(raw) = env.disable_auto_memory.as_deref() {
        if lingxi_core::host::env::is_env_truthy(Some(raw)) {
            return false;
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

    /// Nothing set defaults to enabled.
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

    #[test]
    fn false_disable_flag_leaves_other_gates_in_control() {
        for raw in ["0", "false", "OFF"] {
            assert!(!auto_memory_enabled(&env(Some(raw), false), Some(false)));
            assert!(!auto_memory_enabled(&env(Some(raw), true), Some(true)));
            assert!(auto_memory_enabled(&env(Some(raw), false), None));
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

    /// Unrecognized values are not truthy and leave settings in control.
    #[test]
    fn an_unrecognised_killswitch_value_falls_through() {
        assert!(auto_memory_enabled(&env(Some("maybe"), false), None));
        assert!(!auto_memory_enabled(
            &env(Some("maybe"), false),
            Some(false)
        ));
    }

    #[test]
    fn process_env_uses_only_current_product_flags() {
        const EXPECTED: &str = "LINGXI_TEST_MEMORY_GATE_EXPECTED";
        const SETTING: &str = "LINGXI_TEST_MEMORY_GATE_SETTING";
        if let Ok(expected) = std::env::var(EXPECTED) {
            let setting = std::env::var(SETTING).ok().map(|value| value == "true");
            assert_eq!(
                auto_memory_enabled(&AutoMemoryEnv::from_process_env(), setting),
                expected == "true"
            );
            return;
        }

        // Each case runs in a fresh process so unrelated parallel tests never
        // observe these environment changes.
        let cases = [
            (None, None, None, true),
            (Some("false"), None, Some(false), false),
            (Some("false"), Some("true"), Some(true), false),
            (None, Some("false"), None, true),
            (Some("yes"), Some("false"), Some(true), false),
        ];
        for (disable, simple, setting, expected) in cases {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "gate::tests::process_env_uses_only_current_product_flags",
                ])
                .env(EXPECTED, expected.to_string())
                .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "true")
                .env("CLAUDE_CODE_SIMPLE", "true")
                .env_remove("LINGXI_DISABLE_AUTO_MEMORY")
                .env_remove("LINGXI_SIMPLE")
                .env_remove(SETTING);
            if let Some(value) = disable {
                command.env("LINGXI_DISABLE_AUTO_MEMORY", value);
            }
            if let Some(value) = simple {
                command.env("LINGXI_SIMPLE", value);
            }
            if let Some(value) = setting {
                command.env(SETTING, value.to_string());
            }
            let output = command.output().expect("run isolated memory gate test");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "gate case {disable:?}/{simple:?}/{setting:?} failed: {} {}",
                stdout,
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
}
