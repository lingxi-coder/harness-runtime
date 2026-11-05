//! Differential fixtures from the hash-pinned official 2.1.318 artifact.
use permission::filesystem::{test_rule_pattern, FsRoots, RulePatternMatch};
use permission::rule::{PermissionBehavior, PermissionRuleSource, PermissionRuleValue};
use permission::shell_rule_matching::{
    match_wildcard_pattern_ex, match_wildcard_pattern_utf16, permission_rule_extract_prefix,
};
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/permission_rules_2_1_318.json")).unwrap()
}

#[test]
fn rule_parser_and_public_aliases_match_official_318() {
    let fixture = fixture();
    for case in fixture["parses"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let parsed = PermissionRuleValue::from_rule_string(input);
        let mut actual = json!({"toolName": parsed.tool_name});
        if let Some(content) = parsed.rule_content_text() {
            actual["ruleContent"] = json!(content);
        }
        assert_eq!(actual, case["expected"], "parse {input:?}");
    }
    for case in fixture["serializations"].as_array().unwrap() {
        let value = PermissionRuleValue {
            tool_name: case["input"]["toolName"].as_str().unwrap().to_owned(),
            rule_content: Some(case["input"]["ruleContent"].as_str().unwrap().into()),
            original_rule: None,
        };
        assert_eq!(json!(value.to_rule_string()), case["expected"]);
    }
}

#[test]
fn shell_pattern_guards_and_whitespace_match_official_318() {
    let fixture = fixture();
    for case in fixture["wildcards"].as_array().unwrap() {
        let args = case["input"].as_array().unwrap();
        let pattern = args[0].as_str().unwrap();
        let input = args[1].as_str().unwrap();
        let insensitive = args.get(2).and_then(Value::as_bool).unwrap_or(false);
        let spaces = args.get(3).and_then(Value::as_bool).unwrap_or(false);
        let expected = case["expected"].as_bool().unwrap();
        assert_eq!(
            match_wildcard_pattern_ex(pattern, input, insensitive, spaces),
            expected,
            "{case}"
        );
        if !insensitive && !spaces {
            assert_eq!(
                match_wildcard_pattern_utf16(
                    &pattern.encode_utf16().collect::<Vec<_>>(),
                    &input.encode_utf16().collect::<Vec<_>>()
                ),
                expected,
                "UTF16 {case}"
            );
        }
    }
    for case in fixture["prefixes"].as_array().unwrap() {
        assert_eq!(
            json!(permission_rule_extract_prefix(
                case["input"].as_str().unwrap()
            )),
            case["expected"]
        );
    }
}

#[test]
fn path_glob_case_and_unusable_patterns_match_official_318() {
    let roots = FsRoots {
        cwd: "/proj".into(),
        home: None,
        lingxi_home: "/home/.lingxi".into(),
    };
    for case in fixture()["fileMatches"].as_array().unwrap() {
        let expected = case["expected"].as_bool().unwrap();
        let actual = test_rule_pattern(
            case["input"].as_str().unwrap(),
            case["pattern"].as_str().unwrap(),
            PermissionRuleSource::Session,
            PermissionBehavior::Deny,
            &roots,
        );
        assert_eq!(actual == RulePatternMatch::Match, expected, "{case}");
    }
}

#[test]
fn invalid_character_ranges_do_not_grant_file_access() {
    use permission::rule::PermissionRule;
    use permission::{PermissionMode, PermissionPolicy, PermissionResult};
    for pattern in ["[a--c]", "[z-a]", "[z-ab]"] {
        let policy = PermissionPolicy::from_rules(
            PermissionMode::Default,
            [PermissionRule {
                value: PermissionRuleValue::from_rule_string(&format!("Edit({pattern})")),
                behavior: PermissionBehavior::Allow,
                source: PermissionRuleSource::Session,
            }],
        )
        .with_roots(FsRoots {
            cwd: "/proj".into(),
            home: None,
            lingxi_home: "/home/.lingxi".into(),
        });
        let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
            &json!({"file_path": "/proj/c", "content": "changed"}),
        )
        .unwrap();
        for tool in ["Edit", "Write"] {
            assert!(
                !matches!(
                    policy.authorize(tool, &input),
                    PermissionResult::Allow { .. }
                ),
                "{tool}({pattern})"
            );
        }
    }
}

#[test]
fn escaped_classes_preserve_allow_scope_and_deny_precedence() {
    use permission::rule::PermissionRule;
    use permission::{PermissionMode, PermissionPolicy, PermissionResult};
    let roots = FsRoots {
        cwd: "/proj".into(),
        home: None,
        lingxi_home: "/proj/.lingxi".into(),
    };
    let empty_allow = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [PermissionRule {
            value: PermissionRuleValue::from_rule_string(r"Edit([^a\])"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Session,
        }],
    )
    .with_roots(roots.clone());
    let edit = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
        &json!({"file_path":"/proj/a","old_string":"old","new_string":"new"}),
    )
    .unwrap();
    assert!(!matches!(
        empty_allow.authorize("Edit", &edit),
        PermissionResult::Allow { .. }
    ));
    for (pattern, filename) in [(r"[\c]", "\\"), (r"[\ca-z]", "b"), (r"[\c0-z]", "b")] {
        for behavior in [PermissionBehavior::Deny, PermissionBehavior::Ask] {
            let policy = PermissionPolicy::from_rules(
                PermissionMode::BypassPermissions,
                [
                    PermissionRule {
                        value: PermissionRuleValue::from_rule_string(&format!("Read({pattern})")),
                        behavior,
                        source: PermissionRuleSource::Session,
                    },
                    PermissionRule {
                        value: PermissionRuleValue::from_rule_string("Read"),
                        behavior: PermissionBehavior::Allow,
                        source: PermissionRuleSource::Session,
                    },
                ],
            )
            .with_roots(roots.clone());
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"file_path":format!("/proj/{filename}")}),
            )
            .unwrap();
            let result = policy.authorize("Read", &input);
            match behavior {
                PermissionBehavior::Deny => {
                    assert!(matches!(result, PermissionResult::Deny { .. }), "{pattern}")
                }
                PermissionBehavior::Ask => {
                    assert!(matches!(result, PermissionResult::Ask { .. }), "{pattern}")
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn class_set_escapes_cannot_expand_grants_or_erase_restrictions() {
    use permission::rule::PermissionRule;
    use permission::{PermissionMode, PermissionPolicy, PermissionResult};
    let roots = FsRoots {
        cwd: "/proj".into(),
        home: None,
        lingxi_home: "/proj/.lingxi".into(),
    };
    for (pattern, outside, inside) in [
        (r"x[\d-z]y", "xfy", "x-y"),
        (r"x[\s-z]y", "xty", "x-y"),
        (r"x[\W-z]y", "xfy", "x-y"),
        (r"x[!-\d]y", "xfy", "x0y"),
    ] {
        let rule = |tool, behavior| PermissionRule {
            value: PermissionRuleValue::from_rule_string(&format!("{tool}({pattern})")),
            behavior,
            source: PermissionRuleSource::Session,
        };
        let allow = PermissionPolicy::from_rules(
            PermissionMode::Default,
            [rule("Edit", PermissionBehavior::Allow)],
        )
        .with_roots(roots.clone());
        for (filename, allowed) in [(outside, false), (inside, true)] {
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"file_path":format!("/proj/{filename}"),"old_string":"old","new_string":"new"}),
            )
            .unwrap();
            assert_eq!(
                matches!(
                    allow.authorize("Edit", &input),
                    PermissionResult::Allow { .. }
                ),
                allowed,
                "Edit({pattern}) {filename}"
            );
        }
        for behavior in [PermissionBehavior::Deny, PermissionBehavior::Ask] {
            let policy = PermissionPolicy::from_rules(
                PermissionMode::BypassPermissions,
                [
                    rule("Read", behavior),
                    PermissionRule {
                        value: PermissionRuleValue::from_rule_string("Read"),
                        behavior: PermissionBehavior::Allow,
                        source: PermissionRuleSource::Session,
                    },
                ],
            )
            .with_roots(roots.clone());
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"file_path":format!("/proj/{inside}")}),
            )
            .unwrap();
            match (behavior, policy.authorize("Read", &input)) {
                (PermissionBehavior::Deny, PermissionResult::Deny { .. })
                | (PermissionBehavior::Ask, PermissionResult::Ask { .. }) => {}
                (_, result) => panic!("Read({pattern}) {inside}: {result:?}"),
            }
        }
    }
}

#[test]
fn class_wildcard_rewrites_preserve_grants_and_restrictions() {
    use permission::rule::PermissionRule;
    use permission::{PermissionMode, PermissionPolicy, PermissionResult};
    let roots = FsRoots {
        cwd: "/proj".into(),
        home: None,
        lingxi_home: "/proj/.lingxi".into(),
    };
    for (pattern, outside, inside) in [
        (r"x[\d?]y", "x0y", "x0]y"),
        (r"x[\D?]y", "xfy", "xf]y"),
        (r"x[\d*]y", "x0y", "x]y"),
        (r"x[\d\?]y", "x0y", "x0]y"),
        (r"x[?\d]y", "x0y", "x[0]y"),
        (r"x[\d\-?]y", "x0y", "x0]y"),
        (r"x[\d**]y", "xf]y", "x0]y"),
        (r"x[\x80?]y", "x0]y", "x\u{80}]y"),
        (r"x[?\-?]y", "x^]y", "x^-a]y"),
    ] {
        let rule = |tool, behavior| PermissionRule {
            value: PermissionRuleValue::from_rule_string(&format!("{tool}({pattern})")),
            behavior,
            source: PermissionRuleSource::Session,
        };
        let allow = PermissionPolicy::from_rules(
            PermissionMode::Default,
            [rule("Edit", PermissionBehavior::Allow)],
        )
        .with_roots(roots.clone());
        for (filename, expected) in [(outside, false), (inside, true)] {
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"file_path":format!("/proj/{filename}"),"old_string":"old","new_string":"new"}),
            ).unwrap();
            assert_eq!(
                matches!(
                    allow.authorize("Edit", &input),
                    PermissionResult::Allow { .. }
                ),
                expected,
                "{pattern} {filename}"
            );
        }
        for behavior in [PermissionBehavior::Deny, PermissionBehavior::Ask] {
            let policy = PermissionPolicy::from_rules(
                PermissionMode::BypassPermissions,
                [
                    rule("Read", behavior),
                    PermissionRule {
                        value: PermissionRuleValue::from_rule_string("Read"),
                        behavior: PermissionBehavior::Allow,
                        source: PermissionRuleSource::Session,
                    },
                ],
            )
            .with_roots(roots.clone());
            let input = lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &json!({"file_path":format!("/proj/{inside}")}),
            )
            .unwrap();
            match (behavior, policy.authorize("Read", &input)) {
                (PermissionBehavior::Deny, PermissionResult::Deny { .. })
                | (PermissionBehavior::Ask, PermissionResult::Ask { .. }) => {}
                (_, result) => panic!("Read({pattern}) {inside}: {result:?}"),
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn rooted_restrictions_follow_symlinks_and_remember_previous_landings() {
    use permission::rule::PermissionRule;
    use permission::{PermissionMode, PermissionPolicy, PermissionResult};
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(first.join("secret"), "one").unwrap();
    std::fs::write(second.join("secret"), "two").unwrap();
    let link = project.join("secrets");
    symlink(&first, &link).unwrap();
    let roots = FsRoots {
        cwd: project.clone(),
        home: None,
        lingxi_home: project.join(".lingxi"),
    };
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [PermissionRule {
            value: PermissionRuleValue::from_rule_string("Read(/secrets/**)"),
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Session,
        }],
    )
    .with_roots(roots.clone());
    let check = |policy: &PermissionPolicy, path: &std::path::Path| {
        let input = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
            &json!({"file_path": path}).to_string(),
        )
        .unwrap();
        policy.authorize("Read", &input)
    };
    assert!(matches!(
        check(&policy, &first.join("secret")),
        PermissionResult::Deny { .. }
    ));
    std::fs::remove_file(&link).unwrap();
    symlink(&second, &link).unwrap();
    assert!(matches!(
        check(&policy, &second.join("secret")),
        PermissionResult::Deny { .. }
    ));
    assert!(matches!(
        check(&policy, &first.join("secret")),
        PermissionResult::Deny { .. }
    ));
    let exclusions = permission::read_deny_globs::read_deny_exclude_globs(&policy, &first);
    assert!(
        exclusions.iter().any(|pattern| pattern == "/**"),
        "{exclusions:?}"
    );
    let independent = policy
        .fork_independent(PermissionMode::Default)
        .with_roots(roots);
    assert!(!matches!(
        check(&independent, &first.join("secret")),
        PermissionResult::Deny { .. }
    ));
}

#[test]
fn injected_windows_wsl_guards_match_official_318() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/permission_guards_2_1_318.json")).unwrap();
    for case in fixture["windowsPatterns"].as_array().unwrap() {
        assert_eq!(
            permission::auto_edit_safety::has_suspicious_windows_path_pattern_for_platform(
                case["input"].as_str().unwrap(),
                case["platform"].as_str().unwrap()
            ),
            case["expected"].as_bool().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn removal_tilde_expansion_matches_official_318() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/permission_guards_2_1_318.json")).unwrap();
    for case in fixture["removals"].as_array().unwrap() {
        let result = permission::check_dangerous_removal(
            &format!("rm {}", case["target"].as_str().unwrap()),
            std::path::Path::new(case["cwd"].as_str().unwrap()),
            case["home"].as_str(),
        );
        if case["expected"]["behavior"] == "ask" {
            assert_eq!(
                result.unwrap().message,
                case["expected"]["message"].as_str().unwrap(),
                "{case}"
            );
        } else {
            assert!(result.is_none(), "{case}: {result:?}");
        }
    }
}

#[cfg(unix)]
#[test]
fn auto_edit_checks_sensitive_symlink_landings_and_removal_checks_glob_bases() {
    use permission::auto_edit_safety::{check_path_safety_for_auto_edit, AutoEditSafety};
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let protected = project.join(".git/config");
    std::fs::write(&protected, "sensitive").unwrap();
    let innocent = project.join("public.txt");
    symlink(&protected, &innocent).unwrap();
    let roots = FsRoots {
        cwd: project.clone(),
        home: Some(home.clone()),
        lingxi_home: home.join(".lingxi"),
    };
    assert!(matches!(
        check_path_safety_for_auto_edit(innocent.to_str().unwrap(), &roots),
        AutoEditSafety::Unsafe { .. }
    ));
    let link = project.join("home-link");
    symlink(&home, &link).unwrap();
    let home = home.to_str().unwrap();
    assert!(permission::check_dangerous_removal(
        &format!("rm {}/*", link.display()),
        &project,
        Some(home)
    )
    .is_some());
    assert!(permission::check_dangerous_removal(
        &format!("rm {}", link.display()),
        &project,
        Some(home)
    )
    .is_none());
}

#[test]
fn env_values_and_later_compound_prefixes_keep_read_block_checks() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/permission_guards_2_1_318.json")).unwrap();
    for case in fixture["wrappers"].as_array().unwrap() {
        assert_eq!(
            permission::shell_command::strip_safe_wrappers(case["command"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "{case}"
        );
    }
    let roots = FsRoots {
        cwd: "/proj".into(),
        home: None,
        lingxi_home: "/home/.lingxi".into(),
    };
    for command in [
        "echo ok; SECRET=1 cat safe.txt",
        "LANG=/tmp/locale cat safe.txt",
        "env LANG=/tmp/locale cat safe.txt",
    ] {
        assert!(
            permission::command_path_containment::check_command_path_containment(
                command,
                &roots,
                &[],
                Some(&[roots.cwd.clone()])
            )
            .is_some(),
            "{command}"
        );
    }
}

#[test]
fn glob_removal_of_additional_working_directory_matches_official_318() {
    use permission::rule::PermissionRule;
    use permission::working_dirs::AdditionalWorkingDirs;
    use permission::{PermissionMode, PermissionPolicy, PermissionResult};
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/permission_guards_2_1_318.json")).unwrap();
    for case in fixture["removalsWithWorkingDirs"].as_array().unwrap() {
        let dirs = case["workingDirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| std::path::PathBuf::from(value.as_str().unwrap()))
            .collect::<Vec<_>>();
        let cwd = std::path::Path::new(case["cwd"].as_str().unwrap());
        let command = format!("rm {}", case["target"].as_str().unwrap());
        let result = permission::check_dangerous_removal_with_working_dirs(
            &command,
            cwd,
            case["home"].as_str(),
            &dirs,
        );
        if case["expected"]["behavior"] == "ask" {
            assert_eq!(
                result.unwrap().message,
                case["expected"]["message"].as_str().unwrap(),
                "{case}"
            );
        } else {
            assert!(result.is_none(), "{case}: {result:?}");
        }
    }
    let roots = FsRoots {
        cwd: "/proj".into(),
        home: Some("/home/user".into()),
        lingxi_home: "/home/user/.lingxi".into(),
    };
    let mut directories = AdditionalWorkingDirs::new();
    directories.insert("/other/deep", PermissionRuleSource::Session);
    let policy = PermissionPolicy::from_rules(
        PermissionMode::AcceptEdits,
        [PermissionRule {
            value: PermissionRuleValue::from_rule_string("Bash"),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Session,
        }],
    )
    .with_roots(roots)
    .with_working_dirs(directories);
    let glob = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
        r#"{"command":"rm /other/deep/*"}"#,
    )
    .unwrap();
    assert!(matches!(
        policy.authorize("Bash", &glob),
        PermissionResult::Ask { .. }
    ));
    let bare = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
        r#"{"command":"rm /other/deep"}"#,
    )
    .unwrap();
    assert!(matches!(
        policy.authorize("Bash", &bare),
        PermissionResult::Allow { .. }
    ));
}
