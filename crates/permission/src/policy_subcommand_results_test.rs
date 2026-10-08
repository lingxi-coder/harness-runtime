use super::*;
use crate::filesystem::FsRoots;
use crate::rule::PermissionRuleValue;
use serde_json::json;
use std::path::PathBuf;

fn roots() -> FsRoots {
    FsRoots {
        cwd: PathBuf::from("/proj/work"),
        home: Some(PathBuf::from("/home/u")),
        lingxi_home: PathBuf::from("/home/u/.lingxi"),
    }
}

fn bash_rule(
    content: &str,
    behavior: PermissionBehavior,
    source: PermissionRuleSource,
) -> PermissionRule {
    PermissionRule {
        value: PermissionRuleValue::from_rule_string(&format!("Bash({content})")),
        behavior,
        source,
    }
}

fn bash_tool_wide_rule(
    behavior: PermissionBehavior,
    source: PermissionRuleSource,
) -> PermissionRule {
    PermissionRule {
        value: PermissionRuleValue::from_rule_string("Bash"),
        behavior,
        source,
    }
}

fn bare_result(behavior: PermissionBehavior) -> PermissionResult {
    let reason = PermissionDecisionReason::Other {
        reason: "test child".into(),
    };
    match behavior {
        PermissionBehavior::Allow => PermissionResult::Allow {
            reason,
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        },
        PermissionBehavior::Deny => PermissionResult::Deny {
            reason,
            explanation: None,
            metadata: Default::default(),
        },
        PermissionBehavior::Ask => PermissionResult::Ask {
            reason,
            prompt: crate::result::PermissionPrompt {
                title: "Allow?".into(),
                message: "approval required".into(),
                options: Vec::new(),
            },
            pending_classifier_check: None,
            metadata: Default::default(),
        },
    }
}

fn child_rule(result: &PermissionResult) -> Option<&PermissionRule> {
    match result_reason(result) {
        PermissionDecisionReason::MatchedRule { rule } => Some(rule),
        _ => None,
    }
}

#[test]
fn managed_nested_deny_keeps_the_real_ordered_child_results() {
    let managed = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule("rm:*", PermissionBehavior::Deny, managed)],
    )
    .with_roots(roots());

    let result = policy.authorize("Bash", &json!({"command":"echo ok | rm -rf build"}));
    let PermissionResult::Deny { reason, .. } = &result else {
        panic!("nested managed deny must remain a deny: {result:?}");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("nested rule decision should carry per-command results: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["echo ok", "rm -rf build"]
    );
    assert_eq!(
        result_behavior(&reasons["rm -rf build"]),
        PermissionBehavior::Deny
    );
    let rule = child_rule(&reasons["rm -rf build"]).expect("child cites the matched rule");
    assert_eq!(rule.value.to_rule_string(), "Bash(rm:*)");
    assert_eq!(rule.source, managed);
}

#[test]
fn sole_preliminary_ask_returns_its_child_rule_directly() {
    let managed = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule("rm:*", PermissionBehavior::Ask, managed)],
    )
    .with_roots(roots());

    let result = policy.authorize("Bash", &json!({"command":"echo ok && rm -rf build"}));
    let PermissionResult::Ask { reason, .. } = &result else {
        panic!("nested ask must remain an ask: {result:?}");
    };
    let PermissionDecisionReason::MatchedRule { rule } = reason else {
        panic!("Native a6t returns its sole preliminary Ask directly: {reason:?}");
    };
    assert_eq!(
        rule.value.to_rule_string(),
        "Bash(rm:*)"
    );
}

#[test]
fn all_allowed_compound_has_actual_child_results() {
    let user = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule("echo:*", PermissionBehavior::Allow, user),
            bash_rule("pwd:*", PermissionBehavior::Allow, user),
        ],
    )
    .with_roots(roots());

    let result = policy.authorize("Bash", &json!({"command":"echo ok && pwd"}));
    let PermissionResult::Allow { reason, .. } = &result else {
        panic!("every allowed part should keep the compound Allow: {result:?}");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("compound allow should replace its aggregate reason with child results: {reason:?}");
    };
    assert_eq!(reasons.len(), 2);
    assert!(reasons
        .values()
        .all(|result| result_behavior(result) == PermissionBehavior::Allow));
}

#[test]
fn direct_single_command_deny_keeps_its_matched_rule_reason() {
    let managed = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule("rm:*", PermissionBehavior::Deny, managed)],
    )
    .with_roots(roots());

    let result = policy.authorize("Bash", &json!({"command":"rm -rf build"}));
    let PermissionResult::Deny { reason, .. } = result else {
        panic!("single-command deny must remain a deny: {result:?}");
    };
    let PermissionDecisionReason::MatchedRule { rule } = reason else {
        panic!("single-command rule attribution must remain direct: {reason:?}");
    };
    assert_eq!(rule.value.to_rule_string(), "Bash(rm:*)");
    assert_eq!(rule.source, managed);
}

#[test]
fn compound_tool_wide_deny_returns_before_the_child_producer() {
    let managed = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_tool_wide_rule(PermissionBehavior::Deny, managed)],
    )
    .with_roots(roots());

    let result = policy.authorize("Bash", &json!({"command":"echo ok && rm -rf build"}));
    let PermissionResult::Deny { reason, .. } = &result else {
        panic!("tool-wide deny must remain a deny: {result:?}");
    };
    let PermissionDecisionReason::MatchedRule { rule } = reason else {
        panic!("tool-wide denial returns before Native's children: {reason:?}");
    };
    assert_eq!(
        rule.value.to_rule_string(),
        "Bash"
    );
}

#[test]
fn unmatched_sibling_keeps_the_preliminary_ask_in_a_compound_tree() {
    let source = PermissionRuleSource::FlagSettings;
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule("rm:*", PermissionBehavior::Ask, source)],
    ).with_roots(roots());
    let result = policy.authorize("Bash", &json!({"command":"rm -rf build && curl https://x"}));
    let PermissionResult::Ask { reason, prompt, .. } = &result else {
        panic!("the unmatched sibling also requires approval: {result:?}");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("Native retains a tree when another Ot part is passthrough: {reason:?}");
    };
    assert_eq!(reasons.len(), 2);
    assert_eq!(prompt.message, "This Bash command contains multiple operations. The following parts require approval: rm -rf build, curl https://x");
    assert_eq!(first_subcommand_rule_for_behavior(reasons, PermissionBehavior::Ask).unwrap().value.to_rule_string(), "Bash(rm:*)");
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_keeps_original_pipeline_segments_in_policy_results() {
    let command = "if true; then echo ok | cat";
    let (parsed, segments) =
        crate::bash_ast_security::parse_for_security_with_pipeline_segments(command);
    let crate::bash_ast_security::ParseForSecurityResult::Simple { commands } = parsed else {
        panic!("a terminal open if with a valid pipeline body should be structurally recoverable");
    };
    assert_eq!(
        commands
            .iter()
            .map(|command| command.text.as_str())
            .collect::<Vec<_>>(),
        ["true", "echo ok", "cat"]
    );
    assert_eq!(
        segments,
        Some(vec!["if true; then echo ok".to_string(), "cat".to_string()])
    );

    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule(
            "cat:*",
            PermissionBehavior::Ask,
            PermissionRuleSource::FlagSettings,
        )],
    )
    .with_roots(roots());
    let PermissionResult::Ask { reason, .. } =
        policy.authorize("Bash", &json!({"command":command}))
    else {
        panic!("the child ask must remain an ask");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("the recovered pipeline must retain its child results: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["if true; then echo ok", "cat"]
    );
    assert!(reasons.keys().all(|command| !command.contains("\nfi")));
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_child_deny_is_not_downgraded_to_parent_ask() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule(
            "echo:*",
            PermissionBehavior::Deny,
            PermissionRuleSource::FlagSettings,
        )],
    )
    .with_roots(roots());
    let PermissionResult::Deny {
        reason,
        explanation,
        ..
    } = policy.authorize("Bash", &json!({"command":"if true; then echo x"}))
    else {
        panic!("the Native AST walker returns a child deny before the mode fallback");
    };
    assert_eq!(
        explanation.as_deref(),
        Some("Permission to use Bash with command if true; then echo x has been denied.")
    );
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("Native retains the AST children for the compound denial: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["true", "echo x"]
    );
    assert!(matches!(
        reasons.get("echo x").map(|child| child.as_ref()),
        Some(PermissionResult::Deny { .. })
    ));
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_child_deny_outranks_a_parent_content_ask() {
    // `if:*` matches the current Host's first text segment and establishes a
    // content-rule Ask before the recovered AST child walk.
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule(
                "if:*",
                PermissionBehavior::Ask,
                PermissionRuleSource::FlagSettings,
            ),
            bash_rule(
                "echo:*",
                PermissionBehavior::Deny,
                PermissionRuleSource::FlagSettings,
            ),
        ],
    )
    .with_roots(roots());
    let PermissionResult::Deny { reason, .. } =
        policy.authorize("Bash", &json!({"command":"if true; then echo x"}))
    else {
        panic!("a denied AST child outranks the matched content Ask");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("the AST denial retains ordered child outcomes: {reason:?}");
    };
    assert!(matches!(
        reasons.get("echo x").map(|child| child.as_ref()),
        Some(PermissionResult::Deny { .. })
    ));
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_can_allow_when_every_ast_child_is_allowed() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule(
                "true:*",
                PermissionBehavior::Allow,
                PermissionRuleSource::FlagSettings,
            ),
            bash_rule(
                "echo:*",
                PermissionBehavior::Allow,
                PermissionRuleSource::FlagSettings,
            ),
        ],
    )
    .with_roots(roots());
    // The exact current 2.1.291 `sye` → `Znn` → `Dnn` replay returns null
    // for this no-rm command with the default feature enabled. The Host reaches
    // this mode-fallback closure only after its own Bash safety gates pass.
    let PermissionResult::Allow {
        reason,
        updated_input,
        ..
    } = policy.authorize("Bash", &json!({"command":"if true; then echo x"}))
    else {
        panic!("all AST children have explicit content-rule allows");
    };
    assert_eq!(
        updated_input,
        Some(json!({"command":"if true; then echo x"}))
    );
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("Native exposes the ordered AST child results: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["true", "echo x"]
    );
    assert!(reasons.values().all(|child| matches!(
        child.as_ref(),
        PermissionResult::Allow {
            reason: PermissionDecisionReason::MatchedRule { rule },
            ..
        } if rule.value.rule_content.is_some()
    )));
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_parent_content_ask_yields_to_guarded_all_child_allows() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule(
                "if:*",
                PermissionBehavior::Ask,
                PermissionRuleSource::FlagSettings,
            ),
            bash_rule(
                "true:*",
                PermissionBehavior::Allow,
                PermissionRuleSource::FlagSettings,
            ),
            bash_rule(
                "echo:*",
                PermissionBehavior::Allow,
                PermissionRuleSource::FlagSettings,
            ),
        ],
    )
    .with_roots(roots());
    let PermissionResult::Allow { reason, .. } =
        policy.authorize("Bash", &json!({"command":"if true; then echo x"}))
    else {
        panic!("the guarded all-child Allow replaces the parent content Ask");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("the aggregate keeps its child reasons: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["true", "echo x"]
    );
    assert!(reasons
        .values()
        .all(|child| safe_ast_child_allow_result(child)));
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_parent_ask_guard_probe_rejects_dangerous_paths_and_inline_rm() {
    let policy = PermissionPolicy::from_rules(PermissionMode::Default, []).with_roots(roots());
    for command in [
        "if true; then rm -rf /",
        "if true; then cat /etc/passwd",
        "if true; then printf x > /etc/passwd",
        "if true; then echo $(rm -rf /)",
        "if true; then bash -c 'echo x'",
        "if true; then bash -o posix -c 'echo x'",
    ] {
        let input = json!({"command": command});
        let analysis = analyze_shell_command("Bash", &input);
        assert!(
            !policy.shell_content_ask_compound_guards_pass(
                "Bash",
                &input,
                PermissionMode::Default,
                None,
                &analysis,
            ),
            "guard probe must reject {command:?}"
        );
    }
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_can_compose_a_rule_allow_with_a_read_only_child() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule(
            "true:*",
            PermissionBehavior::Allow,
            PermissionRuleSource::FlagSettings,
        )],
    )
    .with_roots(roots());
    let PermissionResult::Allow { reason, .. } =
        policy.authorize("Bash", &json!({"command":"if true; then echo x"}))
    else {
        panic!("both children are allowed by explicit rule or read-only policy");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("the mixed Allow records its child reasons: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["true", "echo x"]
    );
    assert!(matches!(
        reasons.get("true").map(|child| child.as_ref()),
        Some(PermissionResult::Allow {
            reason: PermissionDecisionReason::MatchedRule { .. },
            ..
        })
    ));
    assert!(matches!(
        reasons.get("echo x").map(|child| child.as_ref()),
        Some(PermissionResult::Allow {
            reason: PermissionDecisionReason::Other { reason },
            ..
        }) if reason == "Read-only command is allowed"
    ));
}

#[cfg(feature = "bash-ast")]
#[test]
fn recovered_open_if_keeps_an_explicit_child_ask() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule(
                "true:*",
                PermissionBehavior::Allow,
                PermissionRuleSource::FlagSettings,
            ),
            bash_rule(
                "echo:*",
                PermissionBehavior::Ask,
                PermissionRuleSource::FlagSettings,
            ),
        ],
    )
    .with_roots(roots());
    assert!(matches!(
        policy.authorize("Bash", &json!({"command":"if true; then echo x"})),
        PermissionResult::Ask { .. }
    ));
}

#[cfg(feature = "bash-ast")]
#[test]
fn pipeline_keeps_the_single_preliminary_ask_in_its_tree() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule("rm:*", PermissionBehavior::Ask, PermissionRuleSource::FlagSettings)],
    ).with_roots(roots());
    let PermissionResult::Ask { reason, prompt, .. } = policy.authorize("Bash", &json!({"command":"echo ok | rm -rf build"})) else {
        panic!("the pipeline must ask");
    };
    assert!(matches!(reason, PermissionDecisionReason::SubcommandResults { .. }));
    assert_eq!(prompt.message, "This Bash command contains multiple operations. The following part requires approval: rm -rf build");
}

#[test]
fn ask_parent_cannot_hide_a_deny_child_in_its_reason_tree() {
    let reasons = indexmap::IndexMap::from([
        (
            "rm -rf build".into(),
            Box::new(bare_result(PermissionBehavior::Deny)),
        ),
        (
            "bazel test".into(),
            Box::new(bare_result(PermissionBehavior::Ask)),
        ),
    ]);
    assert!(
        !tree_matches_parent(PermissionBehavior::Ask, &reasons),
        "a child deny outranks ask and must not be projected under an Ask parent"
    );
}

#[test]
fn duplicate_child_reduction_matches_the_native_producer_branch() {
    let ask = bare_result(PermissionBehavior::Ask);
    let allow = bare_result(PermissionBehavior::Allow);
    let deny = bare_result(PermissionBehavior::Deny);
    let pipeline = ShellReasonTree::Pipeline(vec!["same".into(), "same".into()]);
    let ast = ShellReasonTree::Ast(vec!["same".into(), "same".into()]);

    // m6o always uses Map.set, so the later duplicate replaces the earlier
    // value even when its behavior rank is lower.
    assert!(replace_duplicate_child(
        &pipeline,
        PermissionBehavior::Ask,
        &ask,
        &allow
    ));

    // a6t's early Deny / all-Allow maps retain the last duplicate; only the
    // general Ask path uses deny > ask > allow ranking.
    assert!(replace_duplicate_child(
        &ast,
        PermissionBehavior::Deny,
        &deny,
        &ask
    ));
    assert!(!replace_duplicate_child(
        &ast,
        PermissionBehavior::Ask,
        &ask,
        &allow
    ));
    assert!(replace_duplicate_child(
        &ast,
        PermissionBehavior::Ask,
        &ask,
        &deny
    ));
}

#[cfg(feature = "bash-ast")]
#[test]
fn nested_mod_projection_uses_first_child_rule_when_parent_walk_picks_another() {
    let managed = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule("rm:*", PermissionBehavior::Deny, managed),
            bash_rule("echo:*", PermissionBehavior::Deny, managed),
        ],
    )
    .with_roots(roots());
    let input = json!({"command":"echo ok | rm -rf build"});

    // The current Harness whole-input content walk chooses the first matching
    // rule bucket entry (rm), while Native m6o evaluates each pipeline segment
    // and Pst projects the first same-behavior child (echo).
    let whole_input_rule = policy
        .first_match(
            &policy.deny_rules,
            &SOURCES_BY_PRIORITY,
            "Bash",
            &input,
            true,
            &[],
        )
        .expect("the whole command matches the earlier rm rule");
    assert_eq!(whole_input_rule.value.to_rule_string(), "Bash(rm:*)");

    let result = policy.authorize("Bash", &input);
    let PermissionResult::Deny { reason, .. } = &result else {
        panic!("both child rules deny the pipeline: {result:?}");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("Native's pipeline producer should retain the child tree: {reason:?}");
    };
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["echo ok", "rm -rf build"]
    );
    assert_eq!(
        first_subcommand_rule_for_behavior(reasons, PermissionBehavior::Deny)
            .expect("the first denied child projects its rule")
            .value
            .to_rule_string(),
        "Bash(echo:*)"
    );
}

#[cfg(feature = "bash-ast")]
#[test]
fn ast_pipeline_uses_parser_command_spans_for_the_tree() {
    let managed = PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [bash_rule("rm:*", PermissionBehavior::Deny, managed)],
    )
    .with_roots(roots());

    let result = policy.authorize(
        "Bash",
        &json!({"command":"TZ=\"$HOME\" echo ok | rm -rf build"}),
    );
    let PermissionResult::Deny { reason, .. } = result else {
        panic!("AST nested deny must remain a deny: {result:?}");
    };
    let PermissionDecisionReason::SubcommandResults { reasons } = reason else {
        panic!("AST pipeline should carry parser-derived child spans: {reason:?}");
    };
    assert_eq!(reasons.len(), 2);
    assert_eq!(
        reasons.keys().map(String::as_str).collect::<Vec<_>>(),
        ["TZ=\"$HOME\" echo ok", "rm -rf build"]
    );
}

#[cfg(feature = "bash-ast")]
#[test]
fn native_pipeline_deny_copies_child_message_but_ast_keeps_parent_message() {
    let source = PermissionRuleSource::FlagSettings;
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule("rm:*", PermissionBehavior::Deny, source),
            bash_rule("echo:*", PermissionBehavior::Deny, source),
        ],
    ).with_roots(roots());
    for (command, denied_command) in [
        ("echo ok | rm -rf build", "echo ok"),
        ("rm -rf build | echo ok", "rm -rf build"),
        ("echo Ω | rm -rf build", "echo Ω"),
        ("echo 😀 | rm -rf build", "echo 😀"),
        ("echo ok |& rm -rf build", "echo ok"),
        ("echo ok && rm -rf build", "echo ok && rm -rf build"),
    ] {
        let result = policy.authorize("Bash", &json!({"command":command}));
        let PermissionResult::Deny { explanation, .. } = result else {
            panic!("Native policy fixture must deny {command}");
        };
        assert_eq!(explanation.as_deref(), Some(format!(
            "Permission to use Bash with command {denied_command} has been denied."
        ).as_str()), "{command}");
    }
}

#[cfg(feature = "bash-ast")]
#[test]
fn native_compound_allow_retains_original_input() {
    let source = PermissionRuleSource::FlagSettings;
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [
            bash_rule("echo:*", PermissionBehavior::Allow, source),
            bash_rule("pwd:*", PermissionBehavior::Allow, source),
        ],
    ).with_roots(roots());
    for command in ["echo ok | pwd", "echo ok && pwd"] {
        let input = json!({"command":command,"timeout":1000});
        let result = policy.authorize("Bash", &input);
        let PermissionResult::Allow { updated_input, .. } = result else {
            panic!("Native allowed compound must stay allowed: {command}");
        };
        assert_eq!(updated_input, Some(input));
    }
}

#[test]
fn native_ast_ask_duplicate_prefers_safety_but_not_a_rule() {
    let ast = ShellReasonTree::Ast(vec!["same".into(), "same".into()]);
    let ordinary = bare_result(PermissionBehavior::Ask);
    let mut safety = bare_result(PermissionBehavior::Ask);
    let PermissionResult::Ask { reason, .. } = &mut safety else { unreachable!() };
    *reason = PermissionDecisionReason::SafetyCheck {
        reason: "safety".into(),
        classifier_approvable: false,
        circuit_breaker: None,
    };
    assert!(replace_duplicate_child(&ast, PermissionBehavior::Ask, &ordinary, &safety));
    assert!(!replace_duplicate_child(&ast, PermissionBehavior::Ask, &safety, &ordinary));
    let mut rule = bare_result(PermissionBehavior::Ask);
    let PermissionResult::Ask { reason, .. } = &mut rule else { unreachable!() };
    *reason = PermissionDecisionReason::MatchedRule { rule: bash_rule(
        "rm:*", PermissionBehavior::Ask, PermissionRuleSource::FlagSettings,
    ) };
    assert!(!replace_duplicate_child(&ast, PermissionBehavior::Ask, &ordinary, &rule));
}

#[test]
fn global_stdin_guard_precedes_child_read_path_denial() {
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [PermissionRule {
            value: PermissionRuleValue::from_rule_string("Read(secret.env)"),
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::FlagSettings,
        }],
    ).with_roots(roots()).with_block_reads_outside_working_directories(true);
    let result = policy.authorize("Bash", &json!({"command":"cat secret.env | python"}));
    let PermissionResult::Ask { reason, prompt, .. } = result else {
        panic!("Native a6t returns the complete-input stdin guard first");
    };
    assert!(crate::read_block::is_outside_reads_blocked(&reason));
    assert_eq!(prompt.message, "code on stdin cannot be checked against the read block; under the read block (permissions.blockReadsOutsideWorkingDirectories) a command the shell parser cannot analyze asks the person");
}
