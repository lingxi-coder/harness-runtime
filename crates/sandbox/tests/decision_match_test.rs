//! Regression tests for decision match test.

use sandbox::decision::should_use_sandbox_for_command;
use sandbox::runtime_config::SandboxRuntimeConfig;

fn cfg_with_excluded(excluded: &[&str]) -> SandboxRuntimeConfig {
    SandboxRuntimeConfig {
        enabled: true,
        excluded_commands: excluded.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn excluded_command_bare_prefix() {
    // A bare `bazel` rule is a STRICT Exact match (faithful to
    // claude-code's `parsePermissionRule` → `ShellRule::Exact`): it excludes
    // ONLY the literal `bazel`, NOT `bazel build //...` (the old first-token
    // over-match is gone). Use `bazel:*` to exclude arg-bearing invocations.
    let cfg = cfg_with_excluded(&["bazel"]);
    assert!(!should_use_sandbox_for_command("bazel", &cfg));
    assert!(should_use_sandbox_for_command("bazel build //...", &cfg));
    assert!(should_use_sandbox_for_command("cargo build", &cfg));

    let cfg_prefix = cfg_with_excluded(&["bazel:*"]);
    assert!(!should_use_sandbox_for_command(
        "bazel build //...",
        &cfg_prefix
    ));
}

#[test]
fn excluded_command_prefix_with_colon_star() {
    let cfg = cfg_with_excluded(&["docker:*"]);
    assert!(!should_use_sandbox_for_command("docker ps", &cfg));
    assert!(!should_use_sandbox_for_command("docker compose up", &cfg));
    assert!(should_use_sandbox_for_command("dockerd --foo", &cfg));
}

#[test]
fn excluded_after_compound_split() {
    // `curl:*` is a Prefix rule that excludes `curl <args>`; the compound split
    // isolates the `curl evil.com` subcommand → entire command unsandboxed.
    // (A bare `curl` Exact rule would NOT match `curl evil.com` — strict.)
    let cfg = cfg_with_excluded(&["curl:*"]);
    assert!(!should_use_sandbox_for_command(
        "docker ps && curl evil.com",
        &cfg
    ));
}

#[test]
fn excluded_after_env_var_strip() {
    // Faithful claude-code stripAllLeadingEnvVars(_, BINARY_HIJACK_VARS):
    let cfg = cfg_with_excluded(&["bazel:*"]);
    // A NON-hijack env prefix (FOO) is stripped → recognized excluded → NOT sandboxed.
    assert!(!should_use_sandbox_for_command(
        "FOO=bar bazel build //...",
        &cfg
    ));
    // PATH matches BINARY_HIJACK_VARS → stripping BREAKS on it → `bazel build`
    // is never exposed → the command stays SANDBOXED (this is the corrected
    // faithful behavior; the prior assertion encoded the inverted bug).
    assert!(should_use_sandbox_for_command(
        "PATH=/usr/local/bin bazel build //...",
        &cfg
    ));
}

#[test]
fn excluded_after_wrapper_strip() {
    let cfg = cfg_with_excluded(&["bazel:*"]);
    assert!(!should_use_sandbox_for_command(
        "timeout 30 bazel build //...",
        &cfg
    ));
}

#[test]
fn no_match_for_unrelated_command() {
    let cfg = cfg_with_excluded(&["bazel:*", "docker:*"]);
    assert!(should_use_sandbox_for_command("rm -rf /tmp/x", &cfg));
}

#[test]
fn empty_excluded_list_always_sandboxes() {
    let cfg = SandboxRuntimeConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(should_use_sandbox_for_command("anything", &cfg));
}
