//! Source diagnostics: what a host's language tooling reports about an app's
//! workspace before it is built.
//!
//! The service asks for them and blocks a build on errors; where they come from
//! (a language server, a compiler invocation, nothing at all) is the host's
//! concern.

use std::path::PathBuf;

/// How serious one diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Must be fixed; blocks a build.
    Error,
    /// Worth fixing; reported to the builder without blocking.
    Warning,
    /// Informational.
    Information,
    /// A suggestion.
    Hint,
}

/// One problem in one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `None` when the tooling did not say; treated as not blocking.
    pub severity: Option<DiagnosticSeverity>,
    /// The tooling's own code for the problem (for example a TypeScript error
    /// number), if it has one.
    pub code: Option<String>,
    /// Zero-based line of the start of the problem.
    pub line: u32,
    /// Zero-based column of the start of the problem.
    pub character: u32,
    /// What is wrong, as the tooling worded it.
    pub message: String,
}

/// The diagnostics currently held for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiagnostics {
    /// The file, as a path on the host.
    pub path: PathBuf,
    /// Whether the diagnostics describe the file's current contents. Stale ones
    /// are advisory: they never block a build.
    pub fresh: bool,
    /// The problems, in the order the tooling reported them.
    pub diagnostics: Vec<Diagnostic>,
}

/// Whether the tooling has caught up with the files it tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsSettleState {
    /// The tooling tracks no file under the workspace, so there is nothing to
    /// wait for and nothing to report.
    NoTrackedDocuments,
    /// Every tracked file has current diagnostics.
    Settled,
    /// The deadline passed first; what is held may be stale.
    TimedOut,
}

/// The answer to "have the diagnostics under this workspace settled?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticsSettleStatus {
    /// Whether they have.
    pub state: DiagnosticsSettleState,
    /// How many files the tooling tracks under the workspace.
    pub tracked_documents: usize,
}
