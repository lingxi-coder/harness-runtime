use sandbox::runtime_config::{
    FilesystemRestrictionConfig, NetworkRestrictionConfig, Platform, RipgrepConfig,
    SandboxRuntimeConfig,
};
use std::collections::HashMap;

#[test]
fn default_config_serializes_with_camelcase_keys() {
    let cfg = SandboxRuntimeConfig::default();
    let v = serde_json::to_value(&cfg).expect("serialize default");
    // Every field name from claude-code's zod SandboxSettingsSchema must appear.
    for key in [
        "enabled",
        "failIfUnavailable",
        "enabledPlatforms",
        "autoAllowBashIfSandboxed",
        "allowUnsandboxedCommands",
        "network",
        "filesystem",
        "ignoreViolations",
        "enableWeakerNestedSandbox",
        "enableWeakerNetworkIsolation",
        "excludedCommands",
        "ripgrep",
    ] {
        assert!(
            v.as_object().unwrap().contains_key(key),
            "missing zod key {key} from default config: {v}"
        );
    }
}

#[test]
fn network_subkeys_match_zod_schema() {
    let cfg = SandboxRuntimeConfig::default();
    let v = serde_json::to_value(&cfg).unwrap();
    let net = &v["network"];
    for key in [
        "allowedDomains",
        "allowManagedDomainsOnly",
        "allowUnixSockets",
        "allowAllUnixSockets",
        "allowLocalBinding",
        "httpProxyPort",
        "socksProxyPort",
    ] {
        assert!(
            net.as_object().unwrap().contains_key(key),
            "missing network key {key}: {net}"
        );
    }
}

#[test]
fn filesystem_subkeys_match_zod_schema() {
    let cfg = SandboxRuntimeConfig::default();
    let v = serde_json::to_value(&cfg).unwrap();
    let fs = &v["filesystem"];
    for key in [
        "allowWrite",
        "denyWrite",
        "denyRead",
        "allowRead",
        "allowManagedReadPathsOnly",
    ] {
        assert!(
            fs.as_object().unwrap().contains_key(key),
            "missing filesystem key {key}: {fs}"
        );
    }
}

#[test]
fn deserializes_realistic_claude_code_settings_fragment() {
    let json = serde_json::json!({
        "enabled": true,
        "failIfUnavailable": false,
        "enabledPlatforms": ["macos", "linux"],
        "autoAllowBashIfSandboxed": true,
        "allowUnsandboxedCommands": true,
        "network": {
            "allowedDomains": ["github.com", "*.anthropic.com"],
            "allowManagedDomainsOnly": false,
            "allowUnixSockets": ["/var/run/docker.sock"],
            "allowAllUnixSockets": false,
            "allowLocalBinding": true,
            "httpProxyPort": 8080,
            "socksProxyPort": 1080
        },
        "filesystem": {
            "allowWrite": ["./build", "./target"],
            "denyWrite": ["/etc"],
            "denyRead": ["/private/etc"],
            "allowRead": ["~/.cargo/registry"],
            "allowManagedReadPathsOnly": false
        },
        "ignoreViolations": { "fs.read": ["~/.cache"] },
        "enableWeakerNestedSandbox": false,
        "enableWeakerNetworkIsolation": false,
        "excludedCommands": ["bazel", "make"],
        "ripgrep": { "command": "/usr/bin/rg", "args": ["--no-config"] }
    });
    let cfg: SandboxRuntimeConfig =
        serde_json::from_value(json).expect("parse SandboxRuntimeConfig");
    assert!(cfg.enabled);
    assert_eq!(
        cfg.enabled_platforms.as_deref(),
        Some(&[Platform::Mac, Platform::Linux][..])
    );
    assert!(cfg.allow_unsandboxed_commands);
    assert_eq!(
        cfg.network.allowed_domains,
        vec!["github.com".to_string(), "*.anthropic.com".to_string()]
    );
    assert_eq!(cfg.network.http_proxy_port, Some(8080));
    assert_eq!(cfg.filesystem.allow_write, vec!["./build", "./target"]);
    assert_eq!(cfg.excluded_commands, vec!["bazel", "make"]);
    assert_eq!(cfg.ripgrep.command, "/usr/bin/rg");
    assert_eq!(cfg.ripgrep.args, vec!["--no-config"]);
    let _: &HashMap<String, Vec<String>> = &cfg.ignore_violations;
}

#[test]
fn platform_enum_serializes_with_macos_spelling() {
    assert_eq!(
        serde_json::to_value(Platform::Mac).unwrap(),
        serde_json::json!("macos")
    );
    assert_eq!(
        serde_json::to_value(Platform::Linux).unwrap(),
        serde_json::json!("linux")
    );
    assert_eq!(
        serde_json::to_value(Platform::Wsl).unwrap(),
        serde_json::json!("wsl")
    );
    // back-compat: the older "mac" spelling still deserializes.
    assert_eq!(
        serde_json::from_value::<Platform>(serde_json::json!("mac")).unwrap(),
        Platform::Mac
    );
    assert_eq!(
        serde_json::from_value::<Platform>(serde_json::json!("macos")).unwrap(),
        Platform::Mac
    );
}

#[test]
fn unknown_platform_in_enabled_list_is_dropped_not_fatal() {
    // claude-code reads enabledPlatforms untyped (sandbox-adapter.ts:505) — an
    // unknown entry must NOT abort the whole SettingsJson parse, else the
    // desktop tier loader's `if let Ok(..) else continue` drops the entire tier.
    let cfg: SandboxRuntimeConfig = serde_json::from_value(serde_json::json!({
        "enabledPlatforms": ["macos", "windows", "linux"]
    }))
    .expect("unknown platform must not fail the parse");
    assert_eq!(
        cfg.enabled_platforms.as_deref(),
        Some(&[Platform::Mac, Platform::Linux][..])
    );
}

#[test]
fn settings_json_with_unknown_platform_still_parses_other_fields() {
    use sandbox::runtime_config::SettingsJson;
    let s: SettingsJson = serde_json::from_value(serde_json::json!({
        "permissions": { "allow": ["Edit(./src/**)"] },
        "sandbox": { "enabled": true, "enabledPlatforms": ["windows"] }
    }))
    .expect("tier must not be dropped over an unknown platform");
    assert!(s.permissions.is_some());
    assert_eq!(
        s.sandbox.unwrap().enabled_platforms.as_deref(),
        Some(&[][..])
    );
}

#[test]
fn unused_helpers_are_referenced() {
    // Touch the optional sub-structs to keep them in scope and ensure they're
    // constructible without arguments.
    let _: NetworkRestrictionConfig = NetworkRestrictionConfig::default();
    let _: FilesystemRestrictionConfig = FilesystemRestrictionConfig::default();
    let _: RipgrepConfig = RipgrepConfig::default();
}

#[test]
fn allow_unsandboxed_commands_defaults_true_when_absent() {
    // claude-code: `allowUnsandboxedCommands` defaults to `true` (sandboxTypes.ts:119).
    let cfg: SandboxRuntimeConfig =
        serde_json::from_value(serde_json::json!({})).expect("parse empty");
    assert!(cfg.allow_unsandboxed_commands);
    assert!(SandboxRuntimeConfig::default().allow_unsandboxed_commands);
    assert!(SandboxRuntimeConfig::default().are_unsandboxed_commands_allowed());
}

#[test]
fn allow_unsandboxed_commands_roundtrips_bool() {
    let off: SandboxRuntimeConfig =
        serde_json::from_value(serde_json::json!({ "allowUnsandboxedCommands": false }))
            .expect("parse false");
    assert!(!off.allow_unsandboxed_commands);
    assert!(!off.are_unsandboxed_commands_allowed());

    let on: SandboxRuntimeConfig =
        serde_json::from_value(serde_json::json!({ "allowUnsandboxedCommands": true }))
            .expect("parse true");
    assert!(on.allow_unsandboxed_commands);

    // Serialized default is the boolean `true`, NOT an array.
    let v = serde_json::to_value(SandboxRuntimeConfig::default()).unwrap();
    assert_eq!(v["allowUnsandboxedCommands"], serde_json::json!(true));
}

#[test]
fn network_emits_denied_domains_camelcase() {
    let v = serde_json::to_value(SandboxRuntimeConfig::default()).unwrap();
    assert!(
        v["network"]
            .as_object()
            .unwrap()
            .contains_key("deniedDomains"),
        "network must serialize deniedDomains: {}",
        v["network"]
    );
    assert!(v["network"]["deniedDomains"].is_array());

    let cfg: SandboxRuntimeConfig =
        serde_json::from_value(serde_json::json!({ "network": { "deniedDomains": ["evil.com"] } }))
            .expect("parse deniedDomains");
    assert_eq!(cfg.network.denied_domains, vec!["evil.com".to_string()]);
}

#[test]
fn ripgrep_argv0_roundtrips() {
    let cfg: SandboxRuntimeConfig = serde_json::from_value(
        serde_json::json!({ "ripgrep": { "command": "rg", "argv0": "rg" } }),
    )
    .expect("parse ripgrep argv0");
    assert_eq!(cfg.ripgrep.argv0, Some("rg".to_string()));

    // `argv0: None` is skipped on serialize (skip_serializing_if).
    let v = serde_json::to_value(SandboxRuntimeConfig::default()).unwrap();
    assert!(
        !v["ripgrep"].as_object().unwrap().contains_key("argv0"),
        "argv0 None must be omitted: {}",
        v["ripgrep"]
    );
}
