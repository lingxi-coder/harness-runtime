//! `BashRunner` — the host seam that runs a TUI `!` bash-mode command.
//!
//! When the user submits `!<cmd>` in the interactive composer, claude-code does
//! NOT send the line to the model: it RUNS the command (through the SAME
//! sandboxed Bash tool the model's `Bash` tool uses) and renders its
//! stdout/stderr inline, with no LLM turn. This trait is that seam.
//!
//! This module defines only the abstract trait + output struct; it never
//! spawns a process itself. The desktop composition root
//! (`harness_runtime::desktop::build`) implements it over the session's
//! `BuiltinToolContext` so the command goes through `tool_shell::BashTool` —
//! i.e. the exact sandbox + permission path of the model's Bash tool. Mounts
//! with no runner wired (resume picker / smoke gates) leave it `None`, and the
//! `!` line is simply echoed without running (inert, not raw-spawned).

use async_trait::async_trait;

/// The captured output of a `!` bash-mode command.
#[derive(Debug, Clone, Default)]
pub struct BashRunOutput {
    /// Standard output captured from the command (may carry ANSI SGR codes;
    /// the `UserBashOutput` renderer parses them at render time).
    pub stdout: String,
    /// Standard error captured from the command.
    pub stderr: String,
    /// The command's exit status.
    ///
    /// The stream-json `bash_command` frame reports it as
    /// `<bash-exit-code>N</bash-exit-code>`, which is how an SDK peer learns a
    /// between-turns command FAILED — stdout/stderr alone cannot distinguish a
    /// command that printed a diagnostic and succeeded from one that died.
    pub exit_code: i32,
}

/// Runs a single `!` bash-mode command through the host's sandboxed Bash tool.
///
/// Implementations MUST route through the same sandboxed executor the model's
/// `Bash` tool uses (`tool_shell::BashTool`), never a raw `std::process`. The
/// pump (the client bash pump) calls [`run`](BashRunner::run) outside the
/// `AppState` lock and folds the result into a `UserBashOutput` scrollback row.
#[async_trait]
pub trait BashRunner: Send + Sync {
    /// Execute `command` and return its captured stdout/stderr.
    async fn run(&self, command: &str) -> BashRunOutput;

    /// Run typed host input with cancellation that stops and joins its owned
    /// process before returning. Implementations must keep sandbox execution,
    /// never transfer cancelled input into model/background work. The default
    /// refuses execution because dropping an arbitrary run future cannot prove
    /// that its subprocess stopped.
    async fn run_with_cancel(
        &self,
        _command: &str,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> BashRunOutput {
        BashRunOutput {
            stdout: String::new(),
            stderr: "Cancellable Bash execution is unavailable".into(),
            exit_code: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Legacy(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl BashRunner for Legacy {
        async fn run(&self, _: &str) -> BashRunOutput {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            BashRunOutput::default()
        }
    }
    #[tokio::test]
    async fn cancellable_default_refuses_to_launch_legacy_runner() {
        let runner = Legacy(std::sync::atomic::AtomicUsize::new(0));
        let result = runner
            .run_with_cancel("ignored", tokio_util::sync::CancellationToken::new())
            .await;
        assert_ne!(result.exit_code, 0);
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
