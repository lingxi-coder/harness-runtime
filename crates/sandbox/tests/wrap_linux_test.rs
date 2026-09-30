//! Regression tests for wrap linux test.

use sandbox::runtime_config::{
    FilesystemRestrictionConfig, NetworkRestrictionConfig, Platform, SandboxRuntimeConfig,
};
use sandbox::wrap::wrap_with_sandbox;

fn cfg(allow_write: Vec<&str>, allowed_domains: Vec<&str>) -> SandboxRuntimeConfig {
    SandboxRuntimeConfig {
        enabled: true,
        filesystem: FilesystemRestrictionConfig {
            allow_write: allow_write.into_iter().map(String::from).collect(),
            ..Default::default()
        },
        network: NetworkRestrictionConfig {
            allowed_domains: allowed_domains.into_iter().map(String::from).collect(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn linux_wrap_starts_with_bwrap_and_ends_with_command() {
    let wrapped =
        wrap_with_sandbox("ls -la", &cfg(vec!["/tmp"], vec![]), Platform::Linux).expect("wrap ok");
    assert!(wrapped.starts_with("bwrap "), "got: {wrapped}");
    assert!(
        wrapped.contains("-- /bin/sh -c"),
        "expected shell suffix, got: {wrapped}"
    );
    // The wrapped command should appear at the very end, quoted.
    assert!(wrapped.contains("ls -la"), "command missing: {wrapped}");
}

#[test]
fn linux_wrap_includes_ro_bind_root() {
    let wrapped = wrap_with_sandbox("ls", &cfg(vec![], vec![]), Platform::Linux).expect("wrap ok");
    assert!(wrapped.contains("--ro-bind / /"), "got: {wrapped}");
}

#[test]
fn linux_wrap_uses_rw_root_when_filesystem_disabled() {
    let mut cfg = cfg(vec![], vec![]);
    cfg.filesystem.disabled = true;
    let wrapped = wrap_with_sandbox("ls", &cfg, Platform::Linux).expect("wrap ok");
    assert!(wrapped.contains("--bind / /"), "got: {wrapped}");
    assert!(!wrapped.contains("--ro-bind / /"), "got: {wrapped}");
}

#[test]
fn linux_wrap_includes_allowwrite_bindings() {
    let wrapped =
        wrap_with_sandbox("ls", &cfg(vec!["/tmp/work"], vec![]), Platform::Linux).expect("wrap ok");
    assert!(
        wrapped.contains("--bind /tmp/work /tmp/work"),
        "got: {wrapped}"
    );
}

#[test]
fn linux_wrap_unshare_net_when_no_domains() {
    let wrapped = wrap_with_sandbox("ls", &cfg(vec![], vec![]), Platform::Linux).expect("wrap ok");
    assert!(wrapped.contains("--unshare-net"), "got: {wrapped}");
    assert!(!wrapped.contains("--share-net"), "got: {wrapped}");
}

#[test]
fn linux_wrap_share_net_when_domains_listed() {
    let wrapped = wrap_with_sandbox(
        "curl",
        &cfg(vec![], vec!["api.example.com"]),
        Platform::Linux,
    )
    .expect("wrap ok");
    assert!(wrapped.contains("--share-net"), "got: {wrapped}");
}

#[test]
fn wsl2_uses_same_bwrap_path() {
    let wrapped = wrap_with_sandbox("ls", &cfg(vec![], vec![]), Platform::Wsl).expect("wrap ok");
    assert!(wrapped.starts_with("bwrap "), "got: {wrapped}");
}

#[test]
fn quoted_command_with_inner_single_quote_round_trips() {
    let wrapped =
        wrap_with_sandbox("echo it's fine", &cfg(vec![], vec![]), Platform::Linux).expect("wrap");
    // The wrapped command should preserve the inner apostrophe via shell escaping.
    assert!(
        wrapped.contains(r"'it'\''s fine'") || wrapped.contains(r"'echo it'\''s fine'"),
        "shell-escape missing for apostrophe: {wrapped}"
    );
}
