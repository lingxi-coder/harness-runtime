//! Core-owned result carrier for Native-compatible JavaScript RegExp checks.
//!
//! Runtime performs the constructor probe in the pinned Bun engine. Core
//! consumes only the typed result and never starts JavaScript itself. Pattern
//! keys and result strings are UTF-16 code units so isolated surrogates remain
//! distinguishable until the source-compatible validation message is built.

use std::fmt;

/// One RegExp constructor result, keyed by the exact UTF-16 input pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyRegexValidationRow {
    pub pattern_units: Vec<u16>,
    pub outcome: NativePolicyRegexValidationOutcome,
}

impl NativePolicyRegexValidationRow {
    pub fn matches_pattern(&self, pattern_units: &[u16]) -> bool {
        self.pattern_units.as_slice() == pattern_units
    }
}

/// Result of `new RegExp(pattern)` and the capture-group probe used by the
/// fixed Native validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyRegexValidationOutcome {
    /// The expression compiled. `source_units` is JavaScript `RegExp.source`;
    /// capture_count is the number of capturing groups observed by Native's
    /// `new RegExp(source + "|").exec("").length - 1` probe.
    Valid {
        source_units: Vec<u16>,
        capture_count: usize,
    },
    /// The Bun RegExp constructor rejected this pattern. The message is the
    /// caught JavaScript Error.message returned by Native `l(error)`.
    Invalid { error_message_units: Vec<u16> },
}

/// A worker/capability failure. These failures are not per-pattern invalid
/// results and must prevent Core from treating an unvalidated pattern as
/// valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyRegexWorkerError {
    ExecutableUnavailable { message: String },
    VersionMismatch { expected: String, actual: String },
    Startup { message: String },
    Protocol { message: String },
    Exited { status: Option<i32> },
    TimedOut,
}

impl fmt::Display for NativePolicyRegexWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutableUnavailable { message } => {
                write!(f, "Bun RegExp worker executable is unavailable: {message}")
            }
            Self::VersionMismatch { expected, actual } => {
                write!(f, "Bun RegExp worker requires {expected}, found {actual}")
            }
            Self::Startup { message } => write!(f, "Bun RegExp worker failed to start: {message}"),
            Self::Protocol { message } => write!(f, "Bun RegExp worker protocol failed: {message}"),
            Self::Exited { status } => {
                write!(
                    f,
                    "Bun RegExp worker exited before replying (status {status:?})"
                )
            }
            Self::TimedOut => f.write_str("Bun RegExp worker timed out"),
        }
    }
}

impl std::error::Error for NativePolicyRegexWorkerError {}
