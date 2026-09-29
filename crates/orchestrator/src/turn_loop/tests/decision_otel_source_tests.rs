use super::rule_decision_otel_source;

#[test]
fn session_rule_is_temporary_on_allow_and_reject_on_deny() {
    assert_eq!(
        rule_decision_otel_source(Some("session"), true),
        "user_temporary"
    );
    assert_eq!(
        rule_decision_otel_source(Some("session"), false),
        "user_reject"
    );
}

#[test]
fn user_owned_settings_rules_are_permanent_on_allow() {
    for scope in ["localSettings", "userSettings"] {
        assert_eq!(
            rule_decision_otel_source(Some(scope), true),
            "user_permanent",
            "{scope}"
        );
        assert_eq!(
            rule_decision_otel_source(Some(scope), false),
            "user_reject",
            "{scope}"
        );
    }
}

#[test]
fn every_other_setting_source_falls_through_to_config() {
    // `ZX_`'s `default:` arm — projectSettings is deliberately NOT in the
    // user-owned set even though `U0s` (the interactive persistence check)
    // includes it.
    for scope in [
        "projectSettings",
        "policySettings",
        "flagSettings",
        "cliArg",
        "command",
        "toolsNarrowing",
        "mcpServerPolicy",
    ] {
        assert_eq!(
            rule_decision_otel_source(Some(scope), true),
            "config",
            "{scope}"
        );
        assert_eq!(
            rule_decision_otel_source(Some(scope), false),
            "config",
            "{scope}"
        );
    }
}

#[test]
fn no_matched_rule_is_config() {
    // `eQ_` reaches `ZX_` only for `decisionReason.type === "rule"`; a mode /
    // classifier / safety-check decision carries no rule and stays "config".
    assert_eq!(rule_decision_otel_source(None, true), "config");
    assert_eq!(rule_decision_otel_source(None, false), "config");
}

/// An Auto-mode approval is a USER allow-once, so 2.1.270's source
/// flattener (`case "user": return e.permanent ? "user_permanent" :
/// "user_temporary"`) renders it `user_temporary` — NOT `hook`.
///
/// The `AllowAuto` arm cannot write that label directly: the assignment
/// after its `match` is unconditional and overwrites every path, so a label
/// written there can never be observed (it was written, and never was). It
/// sets the CLASSIFICATION instead; this pins that the classification still
/// renders the oracle's string, so the repair did not quietly relabel the
/// telemetry it was restoring.
#[test]
fn an_auto_mode_approval_renders_as_a_temporary_user_allow() {
    use platform_api::permission_gate::ToolDecisionClassification;
    assert_eq!(
        ToolDecisionClassification::UserTemporary.as_str(),
        "user_temporary"
    );
    assert_ne!(ToolDecisionClassification::UserTemporary.as_str(), "hook");
}
