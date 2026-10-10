//! Locked exit codes for `lingxi-cli`.
//!
//! See plan M5-12 Task 0 step 2. Amended 2026-06-25 (CLI parity vs
//! claude-code 2.1.191): `ARGV_ERROR` was flipped 2 → 1 to match commander,
//! which exits 1 for EVERY input/usage error (unknown flag, invalid choice,
//! missing arg, and all hand-written cross-flag gates). claude-code collapses
//! usage and runtime onto exit 1; we now do the same so a script keying on
//! `$? == 1` for "bad usage" classifies lingxi the same as claude.

/// Conversation completed successfully (POSIX success).
pub const SUCCESS: i32 = 0;

/// Runtime error: orchestrator failure, API error, IO error, etc.
pub const RUNTIME_ERROR: i32 = 1;

/// Argv / usage error (clap rejected the input, or a cross-flag validation
/// gate failed). Byte-parity with claude-code/commander, which exits 1 for
/// every input error — NOT the BSD `EX_USAGE` 2. Kept as a distinct named
/// constant for call-site readability even though the value equals
/// `RUNTIME_ERROR`.
pub const ARGV_ERROR: i32 = 1;

/// Subcommand not yet implemented (REPL stub pre-M5-13; reserved future
/// stubs). Matches BSD `EX_USAGE` / sysexits.h convention.
pub const NOT_IMPLEMENTED: i32 = 64;

/// SIGINT (Ctrl+C) received during turn execution. POSIX convention
/// (128 + SIGINT=2).
pub const SIGINT: i32 = 130;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_values() {
        assert_eq!(SUCCESS, 0);
        assert_eq!(RUNTIME_ERROR, 1);
        // ARGV_ERROR == 1 (byte-parity with claude-code/commander, which exits
        // 1 for every usage error). Flipped from 2 on 2026-06-25.
        assert_eq!(ARGV_ERROR, 1);
        assert_eq!(NOT_IMPLEMENTED, 64);
        assert_eq!(SIGINT, 130);
    }
}
