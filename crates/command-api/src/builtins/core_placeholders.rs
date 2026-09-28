//! Per-name placeholder handler stubs for the M5-09 19-core-command surface.
//!
//! M5-10 removed the 6 batch-1 placeholders; their real handlers ship under
//! `builtin::{clear, compact, exit, help, init, memory}`.
//!
//! M5-11 removed the remaining batch-2 placeholders; their real handlers
//! ship under `builtin::{agents, config, doctor, hooks, login, logout,
//! mcp, model, permissions, status, version}`.
//!
//! Until [`crate::registry::register_core_batch_1`] /
//! [`crate::registry::register_core_batch_2`] are called, the 19 core names
//! resolve to the shared [`super::unimplemented::UnimplementedCommandHandler`]
//! that returns the locked literal `"{name}: not implemented in v0.6.0 (M5)"`.
//!
//! See plans M5-09 T4, M5-10 T13, M5-11 T13.

use crate::registry::CommandRegistry;

/// No-op shim retained for back-compat with the M5-09 entry-point sequence
/// (`register_all_builtin_commands` calls this after the pass-1 loop).
///
/// Post-M5-11 there are zero per-name placeholders to install; this function
/// exists solely so the call site in
/// [`crate::registry::register_all_builtin_commands`] keeps compiling
/// without churn while M5-12 boots the CLI binary.
pub fn register_core_placeholders(_reg: &mut CommandRegistry) {
    // Intentionally empty: M5-11 replaced all 12 remaining placeholders
    // with their real handlers. The CLI binary (M5-12) wires the real
    // handlers via `register_core_batch_1` + `register_core_batch_2`.
}

#[cfg(test)]
mod tests {
    use crate::builtin_support::names::BUILTIN_CORE_NAMES;
    use crate::model::CommandResult;
    use crate::parser::ParsedSlashCommand;

    #[test]
    fn all_19_core_names_resolve_after_register_all() {
        let mut reg = crate::CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut reg);
        for name in BUILTIN_CORE_NAMES {
            assert!(reg.resolve(name).is_some(), "core /{name} disappeared");
            assert!(
                reg.get_handler(name).is_some(),
                "core /{name} has no handler"
            );
        }
    }

    #[tokio::test]
    async fn core_name_returns_m5_09_stub_before_batch_overwrites() {
        // Without batch_1/batch_2 wiring, every core name still resolves
        // through the shared UnimplementedCommandHandler.
        let mut reg = crate::CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut reg);
        let h = reg.get_handler("clear").expect("clear handler missing");
        let args = ParsedSlashCommand {
            name: "clear".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        };
        match h.handle(&args).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "clear: not implemented in v0.6.0 (M5)");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn core_name_carries_real_description_after_register_all() {
        let mut reg = crate::CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut reg);
        let h = reg.get_handler("help").expect("help handler missing");
        assert_eq!(h.description(), "Show help and available commands");
    }

    #[tokio::test]
    async fn all_19_core_names_return_locked_literal_before_batch_overwrites() {
        let mut reg = crate::CommandRegistry::new();
        crate::builtins::register_all_builtin_commands(&mut reg);
        for name in BUILTIN_CORE_NAMES {
            // `usage` is a core command that `register_all_builtin_commands`
            // itself overwrites with the headless InteractiveOnlyHandler
            // fallback (`register_interactive_only_commands`), so before the
            // batch-5 UsageHandler wiring it returns the interactive-only
            // notice — not the M5 stub literal. Exempt it here.
            if *name == "usage" {
                continue;
            }
            // Same shape as `usage` above: `register_all_builtin_commands`
            // itself wires the real `/auto-mode-setup` handler (Pass 4b — it
            // is handle-free), so it never shows the M5 stub literal. Its
            // no-args output is the oracle's usage block.
            if *name == "auto-mode-setup" {
                continue;
            }
            let h = reg
                .get_handler(name)
                .unwrap_or_else(|| panic!("no handler /{name}"));
            let args = ParsedSlashCommand {
                name: (*name).to_string(),
                raw_args: String::new(),
                positional_args: vec![],
            };
            match h.handle(&args).await {
                CommandResult::Done { display: Some(s) } => {
                    assert_eq!(s, format!("{name}: not implemented in v0.6.0 (M5)"));
                }
                other => panic!("/{name} did not return Done, got {other:?}"),
            }
        }
    }
}
