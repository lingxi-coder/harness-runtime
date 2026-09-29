//! POSIX swarm backends: tmux (real), `iTerm` (`it2`), `InProcess` (no-pane).
//!
//! Module layout mirrors claude-code `src/utils/swarm/backends/`:
//! - `detection`  — env + `which` probes
//! - `tmux`       — real `tmux` shell-out backend
//! - `iterm`      — native `it2` backend (Task 13)
//! - `inprocess`  — no-pane fallback that's always available (Task 14)
//! - `registry`   — auto-detect + construct the appropriate `Box<dyn SwarmBackend>` (Task 15)
//!
//! See spec §6.5 and the M2-05 plan for parity details.

pub mod detection;
pub mod inprocess;
pub mod iterm;
pub mod registry;
pub mod tmux;

pub use inprocess::InProcessSwarmBackend;
pub use iterm::ITermSwarmBackend;
pub use registry::SwarmRegistry;
pub use tmux::TmuxBackend;

/// Reject Unicode Cc characters before dispatching commands to a terminal.
/// Source: Claude Code 2.1.263 src_160690378.js, cCe.
pub(crate) fn validate_pane_command(command: &str) -> Result<(), lingxi_core::host::SwarmError> {
    if let Some(character) = command.chars().find(|character| character.is_control()) {
        return Err(lingxi_core::host::SwarmError::Tmux(format!(
            "Refusing to send command containing control character U+{:04X} to terminal pane",
            u32::from(character)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod command_tests {
    use super::validate_pane_command;

    #[test]
    fn control_character_error_matches_oracle() {
        for (command, code) in [
            ("echo\nnext", "000A"),
            ("echo\u{85}", "0085"),
            ("echo\t", "0009"),
        ] {
            let error = validate_pane_command(command).unwrap_err();
            let lingxi_core::host::SwarmError::Tmux(message) = error else {
                panic!("wrong error kind")
            };
            assert_eq!(message, format!("Refusing to send command containing control character U+{code} to terminal pane"));
        }
        assert!(validate_pane_command("echo 'hello world'").is_ok());
    }
}
