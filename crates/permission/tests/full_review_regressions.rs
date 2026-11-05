//! Public-boundary regressions from the complete permission review.
//! Commands are inspected only; these tests never execute them.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use lingxi_core::host::permission_gate::{
    ModToolCheckDecision, PermissionCheckContext, PermissionOutcome,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use permission::classifier::{AutoModeClassifierVerdict, LoopPermissionClassifier};
use permission::rule::{
    PermissionBehavior as Behavior, PermissionRule, PermissionRuleSource, PermissionRuleValue,
};
use permission::{
    DenyOnAskGate, FsRoots, ModelPathOutcome, ModelPathTranslator, PermissionGate, PermissionMode,
    PermissionPolicy, PermissionResult, PolicyPermissionGate,
};

fn input(value: serde_json::Value) -> Json {
    Json::from_serde_value(&value).unwrap()
}
fn rule(text: &str, behavior: Behavior) -> PermissionRule {
    PermissionRule {
        value: PermissionRuleValue::from_rule_string(text),
        behavior,
        source: PermissionRuleSource::Session,
    }
}
fn roots(cwd: &str) -> FsRoots {
    FsRoots {
        cwd: cwd.into(),
        home: Some("/home/user".into()),
        lingxi_home: "/home/user/.lingxi".into(),
    }
}
fn policy(mode: PermissionMode, rules: Vec<PermissionRule>) -> PermissionPolicy {
    PermissionPolicy::from_rules(mode, rules).with_roots(roots("/proj"))
}

#[test]
fn read_field_denies_survive_the_filesystem_path_matcher() {
    for (field, value) in [
        ("offset", serde_json::json!(1)),
        ("limit", serde_json::json!(10)),
        ("allow_large", serde_json::json!(true)),
    ] {
        let policy = policy(
            PermissionMode::Default,
            vec![
                rule(&format!("Read({field}:*)"), Behavior::Deny),
                rule("Read", Behavior::Allow),
            ],
        );
        let mut value_input = serde_json::json!({"file_path":"/proj/public.txt"});
        value_input[field] = value;
        assert!(
            matches!(
                policy.authorize("Read", &input(value_input)),
                PermissionResult::Deny { .. }
            ),
            "{field}"
        );
        assert!(matches!(
            policy.authorize(
                "Read",
                &input(serde_json::json!({"file_path":"/proj/public.txt"}))
            ),
            PermissionResult::Allow { .. }
        ));
    }
}

#[test]
fn known_cd_targets_keep_read_denies_in_every_mode() {
    for mode in [
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::BypassPermissions,
    ] {
        let policy = policy(mode, vec![rule("Read(sub/secret.txt)", Behavior::Deny)]);
        for command in [
            "cat sub/secret.txt",
            "cd sub && cat secret.txt",
            "cd sub; cat secret.txt",
            "cd sub && cat /proj/sub/secret.txt",
        ] {
            let result = policy.authorize("Bash", &input(serde_json::json!({"command":command})));
            assert!(
                matches!(result, PermissionResult::Deny { .. }),
                "{mode:?} {command}: {result:?}"
            );
        }
    }
}

#[test]
fn unresolved_compound_relative_reads_keep_the_native_guard() {
    let policy = policy(
        PermissionMode::Default,
        vec![rule("Read(other/secret.txt)", Behavior::Deny)],
    );
    let result = policy.authorize(
        "Bash",
        &input(serde_json::json!({"command":"cd sub && cat public.txt"})),
    );
    assert!(matches!(result, PermissionResult::Ask { .. }), "{result:?}");
}

#[test]
fn wrapper_candidates_cannot_erase_explicit_restrictions() {
    for behavior in [Behavior::Deny, Behavior::Ask] {
        let policy = policy(
            PermissionMode::Default,
            vec![rule("Bash(rm:*)", behavior), rule("Bash", Behavior::Allow)],
        );
        for command in [
            "rm file",
            "env rm file",
            "env -i rm file",
            "env -u HOME rm file",
            "env -S 'rm file'",
            "command rm file",
            "command -- rm file",
            "sudo -u user rm file",
            "exec -a alias rm file",
            "nice -n 1 env rm file",
            "timeout --signal TERM 5 env rm file",
            "flock lock rm file",
            "flock -c 'rm file' lock",
            "/usr/bin/env /bin/rm file",
        ] {
            let result = policy.authorize("Bash", &input(serde_json::json!({"command":command})));
            assert!(
                match behavior {
                    Behavior::Deny => matches!(result, PermissionResult::Deny { .. }),
                    _ => matches!(result, PermissionResult::Ask { .. }),
                },
                "{behavior:?} {command}: {result:?}"
            );
        }
        assert!(matches!(
            policy.authorize(
                "Bash",
                &input(serde_json::json!({"command":"command -v rm"}))
            ),
            PermissionResult::Allow { .. }
        ));
    }
}

#[test]
fn ordinary_class_escapes_and_set_endpoints_keep_native_policy_effects() {
    for (pattern, denied, outside) in [
        (r"x[\x80]y", "x\u{80}y", "x0y"),
        (r"x[\u0080]y", "x\u{80}y", "x0y"),
        (r"x[\s\x80]y", "x\u{80}y", "x8y"),
        (r"x[\d-!]y", "x-y", "xfy"),
        (r"x[\d--\d]y", "x-y", "xfy"),
    ] {
        let allow = policy(
            PermissionMode::Default,
            vec![rule(&format!("Edit({pattern})"), Behavior::Allow)],
        );
        for (name, expected) in [(denied, true), (outside, false)] {
            let result = allow.authorize("Edit", &input(serde_json::json!({"file_path":format!("/proj/{name}"),"old_string":"a","new_string":"b"})));
            assert_eq!(
                matches!(result, PermissionResult::Allow { .. }),
                expected,
                "{pattern} {name}: {result:?}"
            );
        }
        for behavior in [Behavior::Deny, Behavior::Ask] {
            let restricted = policy(
                PermissionMode::Default,
                vec![
                    rule(&format!("Read({pattern})"), behavior),
                    rule("Read", Behavior::Allow),
                ],
            );
            let result = restricted.authorize(
                "Read",
                &input(serde_json::json!({"file_path":format!("/proj/{denied}")})),
            );
            assert!(
                match behavior {
                    Behavior::Deny => matches!(result, PermissionResult::Deny { .. }),
                    _ => matches!(result, PermissionResult::Ask { .. }),
                },
                "{pattern}: {result:?}"
            );
        }
    }
}

#[test]
fn legal_unicode_and_literal_brackets_do_not_crash_authorization() {
    let policy =
        policy(PermissionMode::Default, vec![]).with_block_reads_outside_working_directories(true);
    for command in [
        "rm -rf /工作/四个字",
        "rm -rf /private/😀",
        "cat '['",
        "cat '[!'",
    ] {
        let _ = policy.authorize("Bash", &input(serde_json::json!({"command":command})));
    }
    let mut policy = policy;
    policy.roots = Some(FsRoots {
        cwd: "/İ".into(),
        home: Some("/i\u{307}/sub".into()),
        lingxi_home: "/home/user/.lingxi".into(),
    });
    let policy = PermissionPolicy::from_rules(
        PermissionMode::Default,
        [rule("Read(~/secret)", Behavior::Deny)],
    )
    .with_roots(policy.roots.unwrap());
    assert_eq!(
        permission::read_deny_exclude_globs(&policy, std::path::Path::new("/İ")),
        vec!["/\u{307}/sub/secret"]
    );
}

struct Translator;
impl ModelPathTranslator for Translator {
    fn translate(&self, path: &str, _: bool) -> ModelPathOutcome {
        path.strip_prefix("/workspace/ws")
            .map_or(ModelPathOutcome::NotGuest, |rest| {
                ModelPathOutcome::Host(format!("/proj{rest}"))
            })
    }
}

#[tokio::test]
async fn all_contextual_phases_enforce_host_coordinate_rules_and_live_cwd() {
    let input = input(serde_json::json!({"file_path":"/workspace/ws/secret.txt"}));
    let gate = PolicyPermissionGate::new(
        Arc::new(policy(
            PermissionMode::Default,
            vec![rule("Read(./secret.txt)", Behavior::Deny)],
        )),
        Arc::new(DenyOnAskGate),
    )
    .with_path_translator(Arc::new(Translator));
    for ctx in [
        PermissionCheckContext::default(),
        PermissionCheckContext {
            is_non_interactive_session: true,
            ..Default::default()
        },
    ] {
        assert_eq!(
            gate.check_mod_query("Read", &input, &ctx)
                .await
                .unwrap()
                .verdict
                .decision,
            ModToolCheckDecision::Deny
        );
        assert!(matches!(
            gate.check_with_context_or_abort("Read", &input, &ctx)
                .await
                .unwrap(),
            PermissionOutcome::Deny { .. }
        ));
    }
    let gate = PolicyPermissionGate::new(
        Arc::new(policy(
            PermissionMode::Default,
            vec![rule("Read(./secret.txt)", Behavior::Deny)],
        )),
        Arc::new(DenyOnAskGate),
    )
    .with_cwd_provider(Arc::new(|| "/new".into()));
    let input = Json::parse(r#"{"file_path":"/new/secret.txt"}"#).unwrap();
    for ctx in [
        PermissionCheckContext::default(),
        PermissionCheckContext {
            is_non_interactive_session: true,
            ..Default::default()
        },
    ] {
        assert_eq!(
            gate.check_mod_query("Read", &input, &ctx)
                .await
                .unwrap()
                .verdict
                .decision,
            ModToolCheckDecision::Deny
        );
        assert!(matches!(
            gate.check_with_context_or_abort("Read", &input, &ctx)
                .await
                .unwrap(),
            PermissionOutcome::Deny { .. }
        ));
    }
}

struct DenyClassifier(AtomicUsize);
#[async_trait::async_trait]
impl LoopPermissionClassifier for DenyClassifier {
    async fn classify(
        &self,
        _: &str,
        _: &Json,
        _: &[permission::host_context::HostContextRecord],
        _: &[Json],
    ) -> AutoModeClassifierVerdict {
        self.0.fetch_add(1, Ordering::SeqCst);
        AutoModeClassifierVerdict::Deny {
            score: 1.0,
            reason: "fixture rejects the complete operation".into(),
            hard: false,
        }
    }
}

#[tokio::test]
async fn local_test_prefix_never_approves_the_next_operation() {
    for separator in ["\n", " & "] {
        let classifier = Arc::new(DenyClassifier(AtomicUsize::new(0)));
        let gate = PolicyPermissionGate::new(
            Arc::new(
                policy(
                    PermissionMode::Auto,
                    vec![rule("Edit(blocked.txt)", Behavior::Deny)],
                )
                .with_classify_all_shell(true),
            ),
            Arc::new(DenyOnAskGate),
        );
        gate.loop_classifier_handle()
            .set(classifier.clone())
            .ok()
            .unwrap();
        let result = gate.check_with_context_or_abort("Bash", &input(serde_json::json!({"command":format!("npm test{separator}custom-mutator /proj/blocked.txt")})), &PermissionCheckContext::default()).await;
        assert!(
            !matches!(result, Ok(PermissionOutcome::Allow { .. })),
            "{separator:?}: {result:?}"
        );
        if separator == "\n" {
            assert_eq!(classifier.0.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(feature = "bash-ast")]
#[test]
fn deeply_nested_lists_fit_the_supported_host_stack() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let command = vec!["echo ok"; 900].join(" && ");
            let policy = policy(PermissionMode::Default, vec![])
                .with_block_reads_outside_working_directories(true);
            assert!(matches!(
                policy.authorize("Bash", &input(serde_json::json!({"command":command}))),
                PermissionResult::Ask { .. }
            ));
        })
        .unwrap()
        .join()
        .unwrap();
}

struct RefusalThenOverflow(AtomicUsize);
#[async_trait::async_trait]
impl permission::loop_llm::Transport for RefusalThenOverflow {
    fn model(&self) -> &str {
        "fixture"
    }
    async fn query(
        &self,
        _: permission::loop_llm::Query,
    ) -> Result<permission::loop_llm::Reply, permission::loop_llm::QueryError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(permission::loop_llm::Reply {
                text: String::new(),
                stop_reason: "refusal".into(),
            })
        } else {
            Err(permission::loop_llm::QueryError::TranscriptTooLong)
        }
    }
}

#[tokio::test]
async fn safeguard_refusal_is_not_replaced_by_stage_two_overflow() {
    let result = permission::loop_llm::classify_detailed(
        &RefusalThenOverflow(AtomicUsize::new(0)),
        "Agent",
        vec![],
    )
    .await;
    assert_eq!(
        result.failure,
        Some(permission::loop_llm::ClassifierFailure::Refused)
    );
    assert!(matches!(
        result.verdict,
        AutoModeClassifierVerdict::NoVerdict { .. }
    ));
}

#[test]
fn trusted_classifier_settings_preserve_exact_units_and_defaults() {
    let mut settings =
        lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings::default();
    settings.hard_deny = vec![Json::parse(r#""managed-\ud800-boundary""#).unwrap()];
    settings.allow = vec![
        Json::from("$defaults"),
        Json::from("$defaults"),
        Json::from("trusted operation"),
    ];
    let prompt = permission::loop_llm::system_prompt_with_trusted_settings(&settings, &[]);
    let units = prompt.string_units("").unwrap();
    assert!(units.contains(&0xd800));
    assert!(
        prompt
            .string_at("")
            .unwrap_or_default()
            .contains("trusted operation")
            || String::from_utf16_lossy(&units).contains("trusted operation")
    );
    assert!(
        !units.contains(&0xfffd),
        "policy units must not be replaced by display text"
    );
}

#[test]
fn alternative_directory_paths_never_erase_an_existing_read_denial() {
    for command in [
        "true || cd sub; cat secret.txt",
        "false && cd sub; cat secret.txt",
        "echo ok | cd sub; cat secret.txt",
        "cd sub & cat secret.txt",
    ] {
        let policy = policy(
            PermissionMode::BypassPermissions,
            vec![rule("Read(secret.txt)", Behavior::Deny)],
        );
        assert!(
            matches!(
                policy.authorize("Bash", &input(serde_json::json!({"command":command}))),
                PermissionResult::Deny { .. }
            ),
            "{command}"
        );
    }
    for command in [
        "command cd sub && cat secret.txt",
        "builtin cd sub && cat secret.txt",
        "cd $DIRECTORY && cat secret.txt",
        "cd a; cd sub; cat secret.txt",
    ] {
        let policy = policy(
            PermissionMode::BypassPermissions,
            vec![rule("Read(sub/secret.txt)", Behavior::Deny)],
        );
        assert!(
            matches!(
                policy.authorize("Bash", &input(serde_json::json!({"command":command}))),
                PermissionResult::Deny { .. }
            ),
            "{command}"
        );
    }
}

#[tokio::test]
async fn transient_skill_checks_share_the_live_directory_and_coordinates() {
    let gate = PolicyPermissionGate::new(
        Arc::new(policy(
            PermissionMode::Default,
            vec![rule("Read(./secret.txt)", Behavior::Deny)],
        )),
        Arc::new(DenyOnAskGate),
    )
    .with_cwd_provider(Arc::new(|| "/new".into()));
    assert!(matches!(
        gate.check_noninteractive_with_allow_rules(
            "Bash",
            &input(serde_json::json!({"command":"cat /new/secret.txt"})),
            &["Bash".into()]
        ),
        Some(lingxi_core::host::permission_gate::NonInteractivePermissionDecision::Deny { .. })
    ));
}

#[test]
fn wrapper_argv_is_not_reparsed_after_losing_quoting() {
    let policy = policy(
        PermissionMode::Default,
        vec![
            rule("Bash(rm:*)", Behavior::Deny),
            rule("Bash", Behavior::Allow),
        ],
    );
    assert!(!matches!(
        policy.authorize(
            "Bash",
            &input(serde_json::json!({"command":"env 'sudo -u bob' rm file"}))
        ),
        PermissionResult::Deny { .. }
    ));
}

#[test]
fn raw_search_rules_keep_astral_character_classes() {
    let policy = policy(
        PermissionMode::Default,
        vec![rule("Read(x[😀][😀]y)", Behavior::Deny)],
    );
    assert_eq!(
        permission::read_deny_exclude_globs(&policy, std::path::Path::new("/proj")),
        vec!["x[😀][😀]y"]
    );
}

#[test]
fn a_plus_prefixed_directory_is_a_static_path_not_a_directory_stack_index() {
    let policy = policy(
        PermissionMode::BypassPermissions,
        vec![rule("Read(other/secret.txt)", Behavior::Deny)],
    );
    assert!(matches!(
        policy.authorize(
            "Bash",
            &input(serde_json::json!({"command":"cd +foo && cat secret.txt"}))
        ),
        PermissionResult::Allow { .. }
    ));
}

struct ProtectedPrompt(std::sync::Mutex<Vec<PermissionCheckContext>>);
#[async_trait::async_trait]
impl PermissionGate for ProtectedPrompt {
    async fn check(&self, _: &str, _: &Json) -> lingxi_core::host::PermissionDecision {
        panic!("protected calls carry context")
    }
    async fn check_with_context(
        &self,
        _: &str,
        _: &Json,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.0.lock().unwrap().push(ctx.clone());
        PermissionOutcome::Allow { updated_input: None, decision_classification: None,
            permission_updates: vec![Json::parse(r#"{"type":"addRules","rules":[{"toolName":"Write"}],"behavior":"allow","destination":"session"}"#).unwrap()] }
    }
}
struct AllowClassifier(AtomicUsize);
#[async_trait::async_trait]
impl LoopPermissionClassifier for AllowClassifier {
    async fn classify(
        &self,
        _: &str,
        _: &Json,
        _: &[permission::host_context::HostContextRecord],
        _: &[Json],
    ) -> AutoModeClassifierVerdict {
        self.0.fetch_add(1, Ordering::SeqCst);
        AutoModeClassifierVerdict::Allow {
            score: 1.0,
            reason: "fixture".into(),
        }
    }
}
#[tokio::test]
async fn protected_restricted_writes_keep_one_time_human_approval_on_all_entry_points() {
    for legacy in [false, true] {
        let mut policy = policy(PermissionMode::Auto, vec![]);
        policy.restricted = true;
        let input =
            input(serde_json::json!({"file_path":"/proj/.lingxi/settings.json","content":"{}"}));
        assert!(policy.is_restricted_protected_mutation("Write", &input));
        let inner = Arc::new(ProtectedPrompt(std::sync::Mutex::new(Vec::new())));
        let classifier = Arc::new(AllowClassifier(AtomicUsize::new(0)));
        let gate = PolicyPermissionGate::new(Arc::new(policy), inner.clone());
        gate.loop_classifier_handle()
            .set(classifier.clone())
            .ok()
            .unwrap();
        if legacy {
            let _ = gate.check("Write", &input).await;
        } else {
            let _ = gate
                .check_with_context("Write", &input, &PermissionCheckContext::default())
                .await;
        }
        assert_eq!(classifier.0.load(Ordering::SeqCst), 0);
        let contexts = inner.0.lock().unwrap();
        assert_eq!(contexts.len(), 1);
        assert!(contexts[0].suppress_always_allow_rule);
        assert!(gate
            .permission_policy_snapshot()
            .unwrap()
            .allow_rules
            .is_empty());
    }
}

struct SafeguardClassifier;
#[async_trait::async_trait]
impl LoopPermissionClassifier for SafeguardClassifier {
    async fn classify(
        &self,
        name: &str,
        _: &Json,
        _: &[permission::host_context::HostContextRecord],
        _: &[Json],
    ) -> AutoModeClassifierVerdict {
        permission::loop_llm::classify(&RefusalThenOverflow(AtomicUsize::new(0)), name, vec![])
            .await
    }
}
#[tokio::test]
async fn agent_gate_does_not_allow_after_a_safeguard_and_context_overflow() {
    let gate = PolicyPermissionGate::new(
        Arc::new(policy(PermissionMode::Auto, vec![])),
        Arc::new(DenyOnAskGate),
    );
    gate.loop_classifier_handle()
        .set(Arc::new(SafeguardClassifier))
        .ok()
        .unwrap();
    let result = gate
        .check_with_context_or_abort(
            "Agent",
            &input(serde_json::json!({"prompt":"fixture","description":"fixture"})),
            &PermissionCheckContext::default(),
        )
        .await;
    assert!(
        !matches!(result, Ok(PermissionOutcome::Allow { .. })),
        "{result:?}"
    );
}
