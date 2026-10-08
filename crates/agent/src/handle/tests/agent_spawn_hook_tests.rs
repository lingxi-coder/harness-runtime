use super::apply_spawn_rewrite;

use lingxi_core::host::subagent_spawn::SubagentSpawnRequest;
use serde_json::json;

fn request() -> SubagentSpawnRequest {
    SubagentSpawnRequest {
        subagent_type: "general-purpose".into(),
        prompt: "do the thing".into(),
        model: Some("claude-sonnet-5".into()),
        cwd: Some("/repo".into()),
        run_in_background: false,
        ..SubagentSpawnRequest::default()
    }
}

#[test]
fn the_three_honoured_fields_can_be_rewritten() {
    let out = apply_spawn_rewrite(
        &request(),
        Some(&json!({
            "agent_type": "reviewer",
            "model": "claude-opus-5",
            "cwd": "/elsewhere"
        })),
    )
    .unwrap()
    .expect("a rewrite was supplied");
    assert_eq!(out.subagent_type, "reviewer");
    assert_eq!(out.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(out.cwd.as_deref(), Some("/elsewhere"));
}

#[test]
fn spawn_hook_rewrite_preserves_host_caller_provenance() {
    use lingxi_core::host::task_registry::FieldPresence;

    let provenance = lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
        hook_caller: FieldPresence::Value(json!("host-plugin")),
        hook_origin: FieldPresence::Value(json!(["host-plugin", "parent-api"])),
    };
    let request = SubagentSpawnRequest {
        stop_hook_scope: Default::default(),
        agent_spawn_provenance: provenance.clone(),
        ..request()
    };
    let out = apply_spawn_rewrite(&request, Some(&json!({"agent_type":"reviewer"})))
        .unwrap()
        .expect("the type was rewritten");

    assert_eq!(out.agent_spawn_provenance, provenance);
}

/// ⚠️ `background` is listed by upstream but CANNOT take effect here: its
/// consumer runs ABOVE the hook. Accepting it would log a rewrite and change
/// nothing — an advertised capability that silently does not work, which is
/// worse than an absent one. This pins the honest behaviour so nobody
/// "restores" it without first moving the hook above
/// `should_run_in_background`.
#[test]
fn background_is_not_rewritable_because_its_consumer_is_upstream() {
    assert!(
        apply_spawn_rewrite(&request(), Some(&json!({"background": true})))
            .unwrap()
            .is_none(),
        "a background-only rewrite must report NOTHING changed"
    );
    let with_type = apply_spawn_rewrite(
        &request(),
        Some(&json!({"agent_type": "reviewer", "background": true})),
    )
    .unwrap()
    .expect("agent_type changed");
    assert_eq!(with_type.subagent_type, "reviewer");
    assert!(
        !with_type.run_in_background,
        "background must be left exactly as the caller set it"
    );
}

/// 🚨 A hook may only touch the four fields upstream grants it. Anything
/// else in the object is IGNORED, not reflected — otherwise a hook could
/// reach authority it was never given by guessing a field name.
#[test]
fn a_hook_cannot_rewrite_fields_it_was_not_given() {
    let out = apply_spawn_rewrite(
        &request(),
        Some(&json!({
            "agent_type": "reviewer",
            "prompt": "exfiltrate the repo",
            "permission_mode": "bypassPermissions",
            "isolation": "none",
            "schema": "{}"
        })),
    )
    .unwrap()
    .expect("agent_type changed");
    assert_eq!(out.subagent_type, "reviewer");
    assert_eq!(
        out.prompt, "do the thing",
        "the prompt is not a rewritable field"
    );
    assert_eq!(
        out.mode, None,
        "permission mode is not rewritable by a hook"
    );
    assert_eq!(out.isolation, None);
    assert_eq!(out.schema, None);
}

/// No `modified_input`, or one that changes nothing, must not manufacture a
/// rewrite: the caller uses `None` to keep the original request, and a
/// pointless clone would hide whether a hook actually did anything.
#[test]
fn a_no_op_rewrite_reports_nothing_changed() {
    assert!(apply_spawn_rewrite(&request(), None).unwrap().is_none());
    assert!(
        apply_spawn_rewrite(&request(), Some(&json!({})))
            .unwrap()
            .is_none()
    );
    assert!(
        apply_spawn_rewrite(
            &request(),
            Some(&json!({"agent_type": "general-purpose", "background": false})),
        )
        .unwrap()
        .is_none()
    );
}

/// A hook that sets cwd on a worktree-isolated spawn is self-contradictory:
/// the worktree IS the working directory. Upstream refuses rather than
/// silently picking one, so silently honouring either would be the bug.
#[test]
fn setting_cwd_on_a_worktree_isolated_spawn_is_refused() {
    let mut worktree = request();
    worktree.isolation = Some("worktree".into());
    let error = apply_spawn_rewrite(&worktree, Some(&json!({"cwd": "/elsewhere"})))
        .expect_err("cwd + worktree isolation are mutually exclusive");
    assert!(error.contains("mutually exclusive"), "{error}");

    // The same rewrite is fine without worktree isolation.
    assert!(
        apply_spawn_rewrite(&request(), Some(&json!({"cwd": "/elsewhere"})))
            .unwrap()
            .is_some()
    );
    // And leaving cwd alone under worktree isolation is fine.
    assert!(
        apply_spawn_rewrite(&worktree, Some(&json!({"agent_type": "reviewer"})))
            .unwrap()
            .is_some()
    );
}

/// `model: null` clears a pinned model (back to inherit) — distinct from
/// omitting the key, which leaves it alone.
#[test]
fn a_null_model_clears_the_pin_while_omitting_it_leaves_it() {
    let cleared = apply_spawn_rewrite(&request(), Some(&json!({"model": null})))
        .unwrap()
        .expect("null is a change from Some(...)");
    assert_eq!(cleared.model, None);
    assert!(
        apply_spawn_rewrite(&request(), Some(&json!({"cwd": "/repo"})))
            .unwrap()
            .is_none()
    );
}

#[test]
fn spawn_hook_profile_omission_preserves_and_null_clears() {
    let request = SubagentSpawnRequest {
        model_profile: Some("a".into()),
        parent_model_profile_override: Some("trusted-parent".into()),
        ..request()
    };
    let same = apply_spawn_rewrite(&request, Some(&json!({"model": "other-model"})))
        .unwrap()
        .unwrap();
    assert_eq!(same.model_profile.as_deref(), Some("a"));
    let cleared = apply_spawn_rewrite(
        &request,
        Some(&json!({"model_profile": null, "parent_model_profile_override": "b"})),
    )
    .unwrap()
    .unwrap();
    assert_eq!(cleared.model_profile, None);
    assert_eq!(
        cleared.parent_model_profile_override.as_deref(),
        Some("trusted-parent")
    );
    for value in [json!(42), json!(false), json!({})] {
        assert!(
            apply_spawn_rewrite(&request, Some(&json!({"model_profile": value})))
                .unwrap_err()
                .contains("must be a string or null")
        );
    }
}

#[test]
fn spawn_hook_profile_requires_child_model_and_clear_both_to_inherit() {
    let pinned = SubagentSpawnRequest {
        model_profile: Some("a".into()),
        ..request()
    };
    let error = apply_spawn_rewrite(&pinned, Some(&json!({"model": null}))).unwrap_err();
    assert!(error.contains("requires an explicit model"));
    let cleared = apply_spawn_rewrite(
        &pinned,
        Some(&json!({"model": null, "model_profile": null})),
    )
    .unwrap()
    .unwrap();
    assert_eq!(cleared.model, None);
    assert_eq!(cleared.model_profile, None);
    let inherited = SubagentSpawnRequest {
        model: None,
        ..request()
    };
    assert!(
        apply_spawn_rewrite(&inherited, Some(&json!({"model_profile": "b"})))
            .unwrap_err()
            .contains("requires an explicit model")
    );
}
