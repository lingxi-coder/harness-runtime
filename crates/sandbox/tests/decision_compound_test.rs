//! Regression tests for decision compound test.

// `split_compound_command` was removed from the `sandbox` crate; the canonical
// quote-aware splitter is `permission::shell_command::split_command` (which
// `should_use_sandbox` / `should_use_sandbox_for_command` now call internally).
// The env/wrapper fixed-point candidate builder is the SHARED
// `permission::shell_command::strip_env_and_wrappers_fixedpoint`. These cases
// verify the compound-splitting + faithful stripping the excludedCommands match
// relies on (1:1 with claude-code `containsExcludedCommand`).
use permission::shell_command::split_command;
use sandbox::decision::{should_use_sandbox, SandboxDecision};
use sandbox::runtime_config::SandboxRuntimeConfig;
use sandbox::strip_env_and_wrappers_fixedpoint;

#[test]
fn split_double_ampersand() {
    let r = split_command("docker ps && curl evil.com");
    assert_eq!(r, vec!["docker ps", "curl evil.com"]);
}

#[test]
fn split_double_pipe() {
    let r = split_command("ls || echo missing");
    assert_eq!(r, vec!["ls", "echo missing"]);
}

#[test]
fn split_semicolons() {
    let r = split_command("foo; bar; baz");
    assert_eq!(r, vec!["foo", "bar", "baz"]);
}

#[test]
fn split_mixed_operators() {
    let r = split_command("a && b ; c || d");
    assert_eq!(r, vec!["a", "b", "c", "d"]);
}

#[test]
fn no_split_for_single_command() {
    let r = split_command("ls -la /tmp");
    assert_eq!(r, vec!["ls -la /tmp"]);
}

#[test]
fn strip_leading_env_var() {
    // Faithful semantics: FOO is NOT a binary-hijack var, so stripAllLeadingEnvVars
    // strips it, yielding "bazel build //..." as a candidate.
    let candidates = strip_env_and_wrappers_fixedpoint("FOO=bar bazel build //...");
    assert!(
        candidates.iter().any(|c| c == "bazel build //..."),
        "FOO= should be stripped, got {candidates:?}"
    );
}

#[test]
fn strip_only_binary_hijack_vars() {
    // PATH matches BINARY_HIJACK_VARS → stripAllLeadingEnvVars BREAKS on it, so
    // "ls" is NEVER produced; the command stays "PATH=/usr/local/bin ls".
    let candidates = strip_env_and_wrappers_fixedpoint("PATH=/usr/local/bin ls");
    assert!(
        !candidates.iter().any(|c| c == "ls"),
        "PATH= must break stripping (not yield 'ls'), got {candidates:?}"
    );
    assert!(candidates.iter().any(|c| c == "PATH=/usr/local/bin ls"));
}

#[test]
fn strip_timeout_wrapper() {
    let candidates = strip_env_and_wrappers_fixedpoint("timeout 30 bazel build //...");
    assert!(candidates.iter().any(|c| c == "bazel build //..."));
}

#[test]
fn fixedpoint_handles_interleaved_timeout_and_env_var() {
    // timeout 300 (safe wrapper) then FOO=bar (non-hijack env) → both stripped to
    // a fixed point → surviving candidate set MUST contain "bazel run //app".
    let candidates = strip_env_and_wrappers_fixedpoint("timeout 300 FOO=bar bazel run //app");
    assert!(
        candidates.iter().any(|c| c == "bazel run //app"),
        "expected 'bazel run //app' in candidates, got {candidates:?}"
    );
}

#[test]
fn whitespace_only_command_is_sandboxed_not_skipped() {
    // claude-code `!input.command` is falsey only for "" — a whitespace-only
    // command ("   ") is truthy in JS and proceeds to sandbox.
    let cfg = SandboxRuntimeConfig {
        enabled: true,
        ..Default::default()
    };
    let decision = should_use_sandbox(
        "   ",
        /* sandbox_available */ true,
        /* dangerously_disable_sandbox */ false,
        /* unsandboxed_allowed */ true,
        &cfg,
        std::path::PathBuf::from("/work"),
    );
    assert!(matches!(decision, SandboxDecision::Sandbox { .. }));

    // The empty string still bails to NoSandbox.
    let decision_empty = should_use_sandbox(
        "",
        true,
        false,
        true,
        &cfg,
        std::path::PathBuf::from("/work"),
    );
    assert!(matches!(decision_empty, SandboxDecision::NoSandbox));
}
