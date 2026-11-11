use super::*;
use crate::filesystem::FsRoots;
use core_permission::{ReadPathPolicyMatch, ReadSearchPolicyPreparation};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Transport;

#[async_trait]
impl PermissionGate for Transport {
    async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

struct LegacyGate {
    available: bool,
    projection_calls: AtomicUsize,
    snapshot_calls: AtomicUsize,
}

#[async_trait]
impl PermissionGate for LegacyGate {
    async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        PermissionDecision::Allow
    }

    fn read_deny_exclude_globs(&self, cwd: &Path) -> Option<Vec<String>> {
        assert_eq!(cwd, Path::new("/legacy"));
        self.projection_calls.fetch_add(1, Ordering::SeqCst);
        self.available.then(|| vec!["/protected/**".to_owned()])
    }

    fn read_path_policy_snapshot(&self) -> Arc<dyn core_permission::ReadPathPolicySnapshot> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        Arc::new(core_permission::NoPolicyReadPathSnapshot)
    }
}

#[test]
fn legacy_search_preparation_preserves_availability_and_single_projection() {
    for available in [false, true] {
        let gate = LegacyGate {
            available,
            projection_calls: AtomicUsize::new(0),
            snapshot_calls: AtomicUsize::new(0),
        };
        let dyn_gate: &dyn PermissionGate = &gate;
        let prepared = dyn_gate.prepare_read_search_policy(Path::new("/legacy"));
        assert_eq!(prepared.is_some(), available);
        if let Some(ReadSearchPolicyPreparation {
            exclude_globs,
            path_policy,
        }) = prepared
        {
            assert_eq!(exclude_globs, ["/protected/**"]);
            assert_eq!(
                path_policy.facts(),
                core_permission::ReadPathPolicyFacts::default()
            );
        }
        assert_eq!(gate.projection_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            gate.snapshot_calls.load(Ordering::SeqCst),
            usize::from(available)
        );
    }
    assert!(Transport
        .prepare_read_search_policy(Path::new("/legacy"))
        .is_none());
}

fn policy() -> Arc<PermissionPolicy> {
    Arc::new(
        PermissionPolicy::from_rules_confined(
            PermissionMode::Default,
            [PermissionRule {
                value: PermissionRuleValue {
                    tool_name: "Read".into(),
                    rule_content: Some("/protected/**".into()),
                    original_rule: None,
                },
                behavior: PermissionBehavior::Deny,
                source: PermissionRuleSource::Session,
            }],
            false,
        )
        .with_roots(FsRoots {
            cwd: "/first".into(),
            home: None,
            lingxi_home: "/__lingxi_read_search_home__".into(),
        }),
    )
}

#[test]
fn search_preparation_uses_one_live_cwd_and_retains_its_policy() {
    let cwd_calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&cwd_calls);
    let gate = PolicyPermissionGate::new(policy(), Arc::new(Transport)).with_cwd_provider(
        Arc::new(move || {
            if observed_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "/first".into()
            } else {
                "/second".into()
            }
        }),
    );
    let first = gate
        .prepare_read_search_policy(Path::new("/first"))
        .unwrap();
    assert_eq!(cwd_calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.exclude_globs, ["/protected/**"]);
    assert_eq!(
        first
            .path_policy
            .denied_search_path(Path::new("/first/protected/file")),
        ReadPathPolicyMatch::Match
    );
    assert_eq!(
        first
            .path_policy
            .denied_search_path(Path::new("/second/protected/file")),
        ReadPathPolicyMatch::NoMatch
    );

    gate.apply_permission_update_from_fixture(&serde_json::json!({
        "type": "addRules",
        "rules": [{"toolName": "Read", "ruleContent": "later"}],
        "behavior": "deny",
        "destination": "session"
    }));
    let second = gate
        .prepare_read_search_policy(Path::new("/second"))
        .unwrap();
    assert_eq!(cwd_calls.load(Ordering::SeqCst), 2);
    assert_eq!(second.exclude_globs, ["later", "/protected/**"]);
    assert_eq!(
        second
            .path_policy
            .denied_search_path(Path::new("/second/protected/file")),
        ReadPathPolicyMatch::Match
    );
    assert_eq!(
        second
            .path_policy
            .denied_search_path(Path::new("/second/later")),
        ReadPathPolicyMatch::Match
    );
    assert_eq!(
        first
            .path_policy
            .denied_search_path(Path::new("/first/later")),
        ReadPathPolicyMatch::NoMatch,
        "subsequent updates must not mutate a search already being walked"
    );
    assert_eq!(
        cwd_calls.load(Ordering::SeqCst),
        2,
        "exact checks use captured cwd"
    );
}

struct GuestTranslator;

impl crate::ModelPathTranslator for GuestTranslator {
    fn translate(&self, model_path: &str, write: bool) -> crate::ModelPathOutcome {
        assert!(!write);
        match model_path {
            "/guest" => crate::ModelPathOutcome::Host("/first".to_owned()),
            "/guest/fenced" => crate::ModelPathOutcome::Fenced,
            _ => crate::ModelPathOutcome::NotGuest,
        }
    }
}

#[test]
fn search_preparation_preserves_guest_translation_and_fenced_unavailability() {
    let cwd_calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&cwd_calls);
    let gate = PolicyPermissionGate::new(policy(), Arc::new(Transport))
        .with_path_translator(Arc::new(GuestTranslator))
        .with_cwd_provider(Arc::new(move || {
            observed_calls.fetch_add(1, Ordering::SeqCst);
            "/first".into()
        }));
    let host = gate
        .prepare_read_search_policy(Path::new("/first"))
        .unwrap();
    let guest = gate
        .prepare_read_search_policy(Path::new("/guest"))
        .unwrap();
    assert_eq!(guest.exclude_globs, host.exclude_globs);
    assert_eq!(
        guest
            .path_policy
            .denied_search_path(Path::new("/first/protected/file")),
        ReadPathPolicyMatch::Match
    );
    assert_eq!(cwd_calls.load(Ordering::SeqCst), 2);
    assert!(gate
        .prepare_read_search_policy(Path::new("/guest/fenced"))
        .is_none());
    assert_eq!(cwd_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn search_compilation_plan_preserves_source_order_and_negated_denies() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let mut patterns = (0..273)
        .map(|index| format!("unrelated-{index}/**"))
        .collect::<Vec<_>>();
    patterns.extend([
        "src/**".to_owned(),
        "!src/allowed/**".to_owned(),
        "src/allowed/private/**".to_owned(),
    ]);
    let rules = patterns.into_iter().map(|pattern| PermissionRule {
        value: PermissionRuleValue {
            tool_name: "Read".into(),
            rule_content: Some(pattern.into()),
            original_rule: None,
        },
        behavior: PermissionBehavior::Deny,
        source: PermissionRuleSource::Session,
    });
    let policy = Arc::new(
        PermissionPolicy::from_rules_confined(PermissionMode::Default, rules, false).with_roots(
            FsRoots {
                cwd: cwd.clone(),
                home: None,
                lingxi_home: cwd.join("config"),
            },
        ),
    );
    let snapshot = crate::read_deny_globs::read_deny_search_snapshot(Arc::clone(&policy), None);
    for (relative, expected) in [
        ("src/file", ReadPathPolicyMatch::Match),
        ("src/allowed/file", ReadPathPolicyMatch::NoMatch),
        ("src/allowed/private/file", ReadPathPolicyMatch::Match),
        ("other/file", ReadPathPolicyMatch::NoMatch),
        ("unrelated-272/file", ReadPathPolicyMatch::Match),
    ] {
        let path = cwd.join(relative);
        assert_eq!(snapshot.denied_search_path(&path), expected, "{relative}");
        assert_eq!(
            core_permission::ReadPathPolicySnapshot::denied_search_path(policy.as_ref(), &path),
            expected,
            "direct {relative}"
        );
    }
}

#[cfg(unix)]
#[test]
fn search_compilation_plan_keeps_live_and_historical_symlink_restrictions() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let first = cwd.join("first");
    let second = cwd.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let alias = cwd.join("alias");
    symlink(&first, &alias).unwrap();
    let policy = Arc::new(
        PermissionPolicy::from_rules_confined(
            PermissionMode::Default,
            [PermissionRule {
                value: PermissionRuleValue {
                    tool_name: "Read".into(),
                    // A single slash roots this rule at the session cwd.
                    // Native physical-twin history applies to rooted rules;
                    // `./alias/**` would be an unrooted lexical glob.
                    rule_content: Some("/alias/**".into()),
                    original_rule: None,
                },
                behavior: PermissionBehavior::Deny,
                source: PermissionRuleSource::Session,
            }],
            false,
        )
        .with_roots(FsRoots {
            cwd: cwd.clone(),
            home: None,
            lingxi_home: cwd.join("config"),
        }),
    );
    let snapshot = crate::read_deny_globs::read_deny_search_snapshot(Arc::clone(&policy), None);
    assert_eq!(
        snapshot.denied_search_path(&first.join("file")),
        ReadPathPolicyMatch::Match
    );
    std::fs::remove_file(&alias).unwrap();
    symlink(&second, &alias).unwrap();
    for path in [first.join("file"), second.join("file"), alias.join("file")] {
        assert_eq!(
            snapshot.denied_search_path(&path),
            ReadPathPolicyMatch::Match,
            "{}",
            path.display()
        );
    }
    assert_eq!(
        snapshot.denied_search_path(&cwd.join("public/file")),
        ReadPathPolicyMatch::NoMatch
    );
}
