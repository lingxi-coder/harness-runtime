//! Parity fixture: `SettingsJson` → `SandboxRuntimeConfig` conversion.
//!
//! Locks the shape of `convert_settings_to_runtime_config` against
//! claude-code's reference (`src/utils/sandbox/sandbox-adapter.ts:38-103`).
//! `Edit(...)`, `Read(...)`, `WebFetch(domain:...)` rules from
//! `permissions.allow/deny` are folded into
//! `filesystem.{allowWrite,denyWrite,allowRead,denyRead}` and
//! `network.allowedDomains` in the resulting `SandboxRuntimeConfig`.
//! `Bash(...)` rules are intentionally ignored here — their decision lives
//! in `should_use_sandbox` (see `crates/sandbox/src/decision.rs`).

use sandbox::policy_convert::{convert_settings_to_runtime_config, SandboxConvertContext};
use sandbox::runtime_config::SettingsJson;
use serde::Deserialize;
use serde_json::Value;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    input_settings: Value,
    expected_runtime_config: Value,
}

#[test]
fn sandbox_config_conversion_matches_claude_code() {
    let fx: Fixture = load_fixture("sandbox_config_conversion");

    let settings: SettingsJson =
        serde_json::from_value(fx.input_settings).expect("input settings parse");
    let runtime = convert_settings_to_runtime_config(&settings, &SandboxConvertContext::default());
    let got = serde_json::to_value(&runtime).expect("runtime serializes");
    let want = fx.expected_runtime_config;

    // Compare on the fields the fixture exercises. Equality through
    // `serde_json::Value` avoids coupling to internal field order.
    for key in ["enabled", "failIfUnavailable"] {
        assert_eq!(
            got.get(key),
            want.get(key),
            "field {key} must match claude-code conversion exactly: got={:?} want={:?}",
            got.get(key),
            want.get(key)
        );
    }

    // Filesystem allow/deny lists: assert each path the fixture mentions is
    // present (extracted from `Edit(...)` / `Read(...)` rules). Path
    // ordering must match — claude-code emits rules in the order they
    // appear in `permissions.allow` / `permissions.deny`.
    for key in ["allowWrite", "denyWrite", "allowRead", "denyRead"] {
        let got_arr = got["filesystem"][key]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let want_arr = want["filesystem"][key]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            got_arr, want_arr,
            "filesystem.{key} must match claude-code conversion: got={got_arr:?} want={want_arr:?}",
        );
    }

    // Network allowed domains: ordered by `WebFetch(domain:...)` allow-rule
    // appearance. (Denied domains are not surfaced in `allowedDomains`.)
    let got_domains = got["network"]["allowedDomains"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let want_domains = want["network"]["allowedDomains"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        got_domains, want_domains,
        "network.allowedDomains must match claude-code conversion: got={got_domains:?} want={want_domains:?}",
    );
}

/// `allowAppleEvents` round-trip + source restriction. claude-code honors this
/// sandbox setting ONLY from user / managed-policy / CLI `--settings` sources —
/// project & local `.lingxi/settings*.json` are IGNORED (sandbox-adapter.ts
/// 2.1.207 @223928133: `allowAppleEvents:[...managedSources, flagSettings,
/// userSettings].map(z => z?.sandbox?.allowAppleEvents).find(z => z !==
/// undefined)`). In lingxi-core the per-source resolution happens at the
/// composition root and the effective value is threaded via
/// `SandboxConvertContext::allow_apple_events_override`; a `sandbox.allowAppleEvents`
/// in the MERGED settings blob is deliberately NOT applied by the converter, so
/// project/local can never enable Apple Events.
#[test]
fn allow_apple_events_source_restricted_round_trip() {
    let apple_on = r#"{"sandbox": {"allowAppleEvents": true}}"#;

    // (1) Merged-blob allowAppleEvents with default (no honored-source) context is
    //     IGNORED — models a project/local tier trying to enable it.
    let settings: SettingsJson = serde_json::from_str(apple_on).expect("settings parse");
    let cfg = convert_settings_to_runtime_config(&settings, &SandboxConvertContext::default());
    assert!(
        !cfg.allow_apple_events,
        "project/local (merged-blob) allowAppleEvents must be ignored"
    );

    // (2) A honored source (user/managed/flag) resolved to `Some(true)` at the
    //     composition root flows through as `allowAppleEvents: true` and survives
    //     the serde round-trip on the wire-shape config.
    let ctx = SandboxConvertContext {
        allow_apple_events_override: Some(true),
        ..Default::default()
    };
    let cfg = convert_settings_to_runtime_config(&SettingsJson::default(), &ctx);
    assert!(cfg.allow_apple_events);
    let round: Value = serde_json::to_value(&cfg).expect("runtime serializes");
    assert_eq!(
        round.get("allowAppleEvents"),
        Some(&Value::Bool(true)),
        "allowAppleEvents must serialize under its camelCase wire key"
    );
}
