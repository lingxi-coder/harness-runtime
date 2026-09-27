//! Pattern lists for dangerous shell-tool allow-rule prefixes.
//!
//! An allow rule like `Bash(python:*)` or `PowerShell(node:*)` lets the model
//! run arbitrary code via that interpreter, bypassing the auto-mode classifier.
//! These lists feed the `is_dangerous_{bash,powershell}_permission` predicates
//! in [`crate::dangerous_perms`], which strip such rules at auto-mode entry.
//!
//! The matcher in each predicate handles the rule-shape variants (exact, `:*`,
//! trailing `*`, ` *`, ` -…*`). PS-specific cmdlet strings live in
//! [`POWERSHELL_DANGEROUS_PATTERNS`].
//!
//! 1:1 with claude-code `permissions/dangerousPatterns.ts:18-80`, with one
//! documented divergence: the `USER_TYPE === 'ant'` tail of
//! `DANGEROUS_BASH_PATTERNS` (`fa run`, `coo`, `gh`, `gh api`, `curl`, `wget`,
//! `git`, `kubectl`, `aws`, `gcloud`, `gsutil`) is OMITTED here. That tail is
//! an Anthropic-internal empirical-risk call gated behind `USER_TYPE === 'ant'`
//! in TS; for the external build TS itself omits it, so excluding it is the
//! faithful external behavior.

/// Cross-platform code-execution entry points present on both Unix and Windows.
/// Shared between the bash and `PowerShell` lists to prevent them drifting apart
/// on interpreter additions. 1:1 with `dangerousPatterns.ts:18-42`.
pub const CROSS_PLATFORM_CODE_EXEC: &[&str] = &[
    // Interpreters
    "python", "python3", "python2", "node", "deno", "tsx", "ruby", "perl", "php", "lua",
    // Package runners
    "npx", "bunx", "npm run", "yarn run", "pnpm run", "bun run",
    // Shells reachable from both (Git Bash / WSL on Windows, native on Unix)
    "bash", "sh", // Remote arbitrary-command wrapper (native OpenSSH on Win10+)
    "ssh",
];

/// The dangerous-bash pattern table (external build).
///
/// 1:1 with `dangerousPatterns.ts:44-80` MINUS the `USER_TYPE === 'ant'` tail
/// (see module doc). Returns a `Vec` because it concatenates
/// [`CROSS_PLATFORM_CODE_EXEC`] with the bash-only additions, matching the TS
/// spread `[...CROSS_PLATFORM_CODE_EXEC, 'zsh', ...]`.
#[must_use]
pub fn dangerous_bash_patterns() -> Vec<&'static str> {
    let mut patterns: Vec<&'static str> = CROSS_PLATFORM_CODE_EXEC.to_vec();
    patterns.extend_from_slice(&["zsh", "fish", "eval", "exec", "env", "xargs", "sudo"]);
    // The ant-only tail (`fa run`/`coo`/`gh`/…) is OMITTED — see module doc.
    patterns
}

/// PowerShell-specific dangerous cmdlet / process-spawner names.
///
/// 1:1 with the PS-only entries appended to [`CROSS_PLATFORM_CODE_EXEC`] in
/// `permissionSetup.ts:178-209` (`isDangerousPowerShellPermission`). Stored
/// here so the predicate composes `[...CROSS_PLATFORM_CODE_EXEC,
/// ...POWERSHELL_DANGEROUS_PATTERNS]`, mirroring the TS spread.
pub const POWERSHELL_DANGEROUS_PATTERNS: &[&str] = &[
    // Nested PS + shells launchable from PS
    "pwsh",
    "powershell",
    "cmd",
    "wsl",
    // String/scriptblock evaluators
    "iex",
    "invoke-expression",
    "icm",
    "invoke-command",
    // Process spawners
    "start-process",
    "saps",
    "start",
    "start-job",
    "sajb",
    "start-threadjob", // bundled PS 6.1+; takes -ScriptBlock like Start-Job
    // WMI/CIM process-creation escape hatches (2.1.211 Zqr additions)
    "invoke-wmimethod",
    "iwmi", // alias
    "invoke-cimmethod",
    "icim", // alias
    "wmic", // legacy CLI; the .exe variant is covered by the .exe suffix check
    // Event/session code exec
    "register-objectevent",
    "register-engineevent",
    "register-wmievent",
    "register-scheduledjob",
    "new-pssession",
    "nsn", // alias
    "enter-pssession",
    "etsn", // alias
    // .NET escape hatches
    "add-type",   // Add-Type -TypeDefinition '<C#>' → P/Invoke
    "new-object", // New-Object -ComObject WScript.Shell → .Run()
];
