use super::tool_denial_kind;

#[test]
fn ask_behavior_is_user_rejected_and_outranks_the_classifier() {
    // The oracle tests `behavior === "ask"` FIRST and returns before it ever
    // looks at decisionReason, so an ask-behavior denial that also carries a
    // classifier reason is still `user-rejected`.
    assert_eq!(tool_denial_kind(true, None, None), "user-rejected");
    assert_eq!(
        tool_denial_kind(true, Some("classifier"), Some("Classifier unavailable")),
        "user-rejected"
    );
}

#[test]
fn classifier_unavailable_reason_is_automode_unavailable() {
    // `t.reason === TRt` — EXACT equality, not a prefix.
    assert_eq!(
        tool_denial_kind(false, Some("classifier"), Some("Classifier unavailable")),
        "automode-unavailable"
    );
    assert_eq!(
        tool_denial_kind(
            false,
            Some("classifier"),
            Some("Classifier unavailable later")
        ),
        "automode-blocked",
        "TRt is matched by equality, so a longer string is NOT unavailable"
    );
}

#[test]
fn safety_block_prefix_is_automode_parsing_error() {
    // `t.reason.startsWith(p2s)` — a PREFIX test, so the trailing detail the
    // oracle appends must still classify as a parsing error.
    let p2s = "Auto mode could not evaluate this action and is blocking it for safety";
    assert_eq!(
        tool_denial_kind(false, Some("classifier"), Some(p2s)),
        "automode-parsing-error"
    );
    assert_eq!(
        tool_denial_kind(false, Some("classifier"), Some(&format!("{p2s}: bad JSON"))),
        "automode-parsing-error"
    );
}

#[test]
fn other_classifier_reasons_are_automode_blocked() {
    assert_eq!(
        tool_denial_kind(
            false,
            Some("classifier"),
            Some("writes outside the workspace")
        ),
        "automode-blocked"
    );
    assert_eq!(
        tool_denial_kind(false, Some("classifier"), None),
        "automode-blocked",
        "a classifier denial with no reason still falls through to blocked"
    );
}

#[test]
fn non_classifier_denials_are_permission_rule() {
    for kind in [
        None,
        Some("rule"),
        Some("mode"),
        Some("hook"),
        Some("safetyCheck"),
    ] {
        assert_eq!(
            tool_denial_kind(false, kind, Some("Classifier unavailable")),
            "permission-rule",
            "{kind:?} is not a classifier decision, so the reason text is irrelevant"
        );
    }
}
