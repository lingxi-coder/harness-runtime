//! Parity driver for `shell_tools.json` — asserts every fixture value is
//! byte-for-byte present in production constants. Any drift here means
//! the parity contract with claude-code has broken.

#![allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::used_underscore_binding
)]

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    _source: String,
    _note: String,
    tool_names: Vec<String>,
    bash: BashF,
    powershell: PowerShellF,
    repl: ReplF,
    sleep: SleepF,
    ansi_strip: AnsiF,
    output_truncation: OutputTruncF,
    sandbox_refusal_windows_literal: String,
}
#[derive(Deserialize)]
struct BashF {
    default_timeout_ms: u64,
    max_timeout_ms: u64,
    shell_paths: ShellPaths,
    timeout_error_template: String,
    background_response_keys: Vec<String>,
    telemetry_events: Vec<String>,
}
#[derive(Deserialize)]
struct ShellPaths {
    linux: String,
    macos: String,
}
#[derive(Deserialize)]
struct PowerShellF {
    default_timeout_ms: u64,
    max_timeout_ms: u64,
    executables: PsExe,
    missing_pwsh_error_substring: String,
    telemetry_events: Vec<String>,
}
#[derive(Deserialize)]
struct PsExe {
    windows: String,
    unix: String,
}
#[derive(Deserialize)]
struct ReplF {
    default_timeout_ms: u64,
    supported_languages: Vec<String>,
    python_exec_unix: String,
    python_exec_windows: String,
    node_exec: String,
    ruby_exec: String,
    telemetry_events: Vec<String>,
}
#[derive(Deserialize)]
struct SleepF {
    max_duration_ms: u64,
    telemetry_events: Vec<String>,
}
#[derive(Deserialize)]
struct AnsiF {
    regex_literal: String,
}
#[derive(Deserialize)]
struct OutputTruncF {
    max_length: usize,
    suffix: String,
}

#[test]
fn shell_tools_parity() {
    let f: Fixture = load_fixture("shell_tools");
    assert!(!f._source.is_empty(), "_source key required");
    assert!(!f._note.is_empty(), "_note key required");

    // Tool names.
    assert_eq!(f.tool_names, vec!["Bash", "PowerShell", "REPL", "Sleep"]);

    // Bash constants.
    use tool_shell::bash;
    assert_eq!(f.bash.default_timeout_ms, bash::BASH_DEFAULT_TIMEOUT_MS);
    assert_eq!(f.bash.max_timeout_ms, bash::BASH_MAX_TIMEOUT_MS);
    assert_eq!(f.bash.shell_paths.linux, bash::BASH_SHELL_LINUX);
    assert_eq!(f.bash.shell_paths.macos, bash::BASH_SHELL_MACOS);
    assert_eq!(
        f.bash.timeout_error_template,
        bash::BASH_TIMEOUT_ERROR_TEMPLATE
    );
    assert_eq!(
        f.bash.background_response_keys,
        vec![
            "pid".to_string(),
            "task_id".to_string(),
            "task_output_path".to_string(),
        ],
    );
    assert_eq!(
        f.bash.telemetry_events,
        vec![
            telemetry::tengu::tool::BASH_STARTED.to_string(),
            telemetry::tengu::tool::BASH_COMPLETED.into(),
            telemetry::tengu::tool::BASH_FAILED.into(),
            telemetry::tengu::tool::BASH_TIMEOUT.into(),
        ],
    );

    // PowerShell.
    use tool_shell::powershell;
    assert_eq!(
        f.powershell.default_timeout_ms,
        powershell::POWERSHELL_DEFAULT_TIMEOUT_MS
    );
    assert_eq!(
        f.powershell.max_timeout_ms,
        powershell::POWERSHELL_MAX_TIMEOUT_MS
    );
    assert_eq!(
        f.powershell.executables.windows,
        powershell::POWERSHELL_BIN_WINDOWS
    );
    assert_eq!(
        f.powershell.executables.unix,
        powershell::POWERSHELL_BIN_UNIX
    );
    assert!(
        f.powershell
            .missing_pwsh_error_substring
            .contains("pwsh not found"),
        "fixture missing_pwsh_error_substring should be a human-readable diagnostic",
    );
    assert_eq!(
        f.powershell.telemetry_events,
        vec![
            telemetry::tengu::tool::POWERSHELL_STARTED.to_string(),
            telemetry::tengu::tool::POWERSHELL_COMPLETED.into(),
            telemetry::tengu::tool::POWERSHELL_FAILED.into(),
        ],
    );

    // REPL.
    use tool_shell::repl;
    assert_eq!(f.repl.default_timeout_ms, repl::REPL_DEFAULT_TIMEOUT_MS);
    assert_eq!(f.repl.supported_languages, vec!["python", "node", "ruby"]);
    let python_expected = if cfg!(target_os = "windows") {
        &f.repl.python_exec_windows[..]
    } else {
        &f.repl.python_exec_unix[..]
    };
    assert_eq!(repl::lang_exec("python", "x").unwrap().0, python_expected);
    assert_eq!(repl::lang_exec("node", "x").unwrap().0, f.repl.node_exec);
    assert_eq!(repl::lang_exec("ruby", "x").unwrap().0, f.repl.ruby_exec);
    assert_eq!(
        f.repl.telemetry_events,
        vec![
            telemetry::tengu::tool::REPL_STARTED.to_string(),
            telemetry::tengu::tool::REPL_COMPLETED.into(),
            telemetry::tengu::tool::REPL_FAILED.into(),
        ],
    );

    // Sleep.
    use tool_ui::sleep as sleep_tool;
    assert_eq!(f.sleep.max_duration_ms, sleep_tool::SLEEP_MAX_DURATION_MS);
    assert_eq!(
        f.sleep.telemetry_events,
        vec![
            telemetry::tengu::tool::SLEEP_STARTED.to_string(),
            telemetry::tengu::tool::SLEEP_COMPLETED.into(),
            telemetry::tengu::tool::SLEEP_FAILED.into(),
        ],
    );

    // ANSI strip regex literal.
    assert_eq!(
        f.ansi_strip.regex_literal,
        tool_shell::shared::ANSI_ESCAPE_REGEX_LITERAL,
    );

    // Output truncation (cross-check with M4-01 lock).
    assert_eq!(
        f.output_truncation.max_length,
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH,
    );
    assert_eq!(
        f.output_truncation.suffix,
        tool_api::util::output_truncation::SHELL_TRUNCATION_SUFFIX_TEMPLATE,
    );

    // Sandbox refusal literal — sourced from M2-04 lock at
    // SandboxError::Unsupported display.
    let err = platform_api::sandbox::SandboxError::Unsupported;
    assert_eq!(f.sandbox_refusal_windows_literal, err.to_string());
}
