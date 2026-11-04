//! Process-global Skill registry, shared with the agent publication path.

pub use lingxi_core::host::invoked_skills::*;

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{utf16_json::Utf16JsonProjection, MessageId};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use crate::post_compact::{
        restore_post_compact_skills, AttachedSkillContent, SKILL_TRUNCATION_MARKER,
    };

    // The registry is a process-global; serialize the tests that mutate it so
    // they don't race, and `reset_for_test()` at the top of each for a clean
    // view.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        g
    }

    fn source(text: &str) -> Utf16JsonProjection {
        text.into()
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn text(value: &Utf16JsonProjection) -> &str {
        value.string_at("").expect("root string")
    }

    #[test]
    fn register_sets_key_and_filter_by_agent() {
        let _g = guard();
        register(
            &source("deploy"),
            Path::new("/skills/deploy"),
            &source("deploy body"),
            None,
        );
        register(
            &source("build"),
            Path::new("/skills/build"),
            &source("build body"),
            Some("agent:x"),
        );

        // Main-thread filter (None) sees only the `:deploy` row.
        let main = filter_for_agent(None);
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].key, units(":deploy"));
        assert_eq!(text(&main[0].name), "deploy");
        assert_eq!(text(&main[0].content), "deploy body");
        assert_eq!(main[0].path, PathBuf::from("/skills/deploy"));

        // Agent filter sees only its own row.
        let agent = filter_for_agent(Some("agent:x"));
        assert_eq!(agent.len(), 1);
        assert_eq!(agent[0].key, units("agent:x:build"));
    }

    #[test]
    fn equal_timestamps_restore_in_registry_insertion_order() {
        let _g = guard();
        let expected = (0..12).map(|i| format!("skill-{i}")).collect::<Vec<_>>();
        for name in &expected {
            register(&source(name), Path::new("/s"), &source("body"), None);
        }
        // Force the millisecond collision that the native Map + stable sort
        // handles without relying on wall-clock scheduling in this test.
        set_invocation_times_for_test(42);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(
            restored
                .iter()
                .map(|skill| text(&skill.name))
                .collect::<Vec<_>>(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overwriting_a_skill_preserves_its_map_position_on_timestamp_ties() {
        let _g = guard();
        for name in ["first", "second", "third"] {
            register(&source(name), Path::new("/s"), &source("body"), None);
        }
        register(
            &source("first"),
            Path::new("/updated"),
            &source("updated"),
            None,
        );
        write_back(&units(":second"), &source("persisted"));
        set_invocation_times_for_test(42);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(
            restored
                .iter()
                .map(|skill| text(&skill.name))
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(text(&restored[0].content), "updated");
        assert_eq!(text(&restored[1].content), "persisted");
    }

    #[test]
    fn register_overwrites_same_key() {
        let _g = guard();
        register(&source("s"), Path::new("/s"), &source("first"), None);
        register(&source("s"), Path::new("/s"), &source("second"), None);
        let rows = filter_for_agent(None);
        assert_eq!(rows.len(), 1, "same key overwrites, not appends");
        assert_eq!(text(&rows[0].content), "second");
    }

    #[test]
    fn write_back_overwrites_and_clears() {
        let _g = guard();
        register(&source("s"), Path::new("/s"), &source("body"), None);
        let exact =
            Utf16JsonProjection::root_string("t�".to_string(), vec![0x0074, 0xD83D]).unwrap();
        write_back(&units(":s"), &exact);
        assert_eq!(content_for_test(&units(":s")), Some(exact));
        write_back(&units(":s"), &source(""));
        assert_eq!(text(&content_for_test(&units(":s")).unwrap()), "");
    }

    #[test]
    fn write_back_absent_key_is_noop() {
        let _g = guard();
        write_back(&units(":missing"), &source("x"));
        assert!(content_for_test(&units(":missing")).is_none());
    }

    #[test]
    fn registry_key_uses_empty_prefix_for_main_thread() {
        assert_eq!(
            registry_key(InvokedSkillScopeRef::new(None, None), &source("foo")),
            units(":foo")
        );
        assert_eq!(
            registry_key(
                InvokedSkillScopeRef::new(None, Some("agent:a")),
                &source("foo")
            ),
            units("agent:a:foo")
        );
    }

    #[test]
    fn scoped_rows_do_not_collide_across_session_or_agent() {
        let _g = guard();
        let session_a_main = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let other_session_main = InvokedSkillScopeRef::new(Some("sess:b"), None);
        let session_a_agent = InvokedSkillScopeRef::new(Some("sess:a"), Some("agent:x"));

        register_scoped(
            &source("build"),
            Path::new("/sa"),
            &source("main a"),
            session_a_main,
        );
        register_scoped(
            &source("build"),
            Path::new("/sb"),
            &source("main b"),
            other_session_main,
        );
        register_scoped(
            &source("build"),
            Path::new("/sx"),
            &source("agent a"),
            session_a_agent,
        );

        let rows = filter_for_scope(session_a_main);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "main a");
        assert_eq!(
            rows[0].key,
            registry_key(session_a_main, &source("build")),
            "scoped key must be stable for write-back"
        );

        let rows = filter_for_scope(other_session_main);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "main b");

        let rows = filter_for_scope(session_a_agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "agent a");
    }

    #[test]
    fn mode_change_retires_only_the_sessions_main_thread() {
        let _g = guard();
        let main_a = InvokedSkillScopeRef::new(Some("a"), None);
        let agent_a = InvokedSkillScopeRef::new(Some("a"), Some("child"));
        let main_b = InvokedSkillScopeRef::new(Some("b"), None);
        let legacy = InvokedSkillScopeRef::new(None, None);
        let ids = [
            MessageId::new(),
            MessageId::new(),
            MessageId::new(),
            MessageId::new(),
        ];
        for (scope, id) in [
            (main_a, ids[0]),
            (agent_a, ids[1]),
            (main_b, ids[2]),
            (legacy, ids[3]),
        ] {
            register_scoped_for_message(
                &source("same"),
                Path::new("/s"),
                &source("body"),
                scope,
                id,
            );
        }
        let before = filter_for_scope(main_a);

        expire_for_mode_change("a");
        expire_for_mode_change("a"); // retirement is idempotent

        assert!(filter_for_scope(main_a).is_empty());
        assert!(restore_post_compact_skills(before, &[]).is_empty());
        for scope in [agent_a, main_b, legacy] {
            assert_eq!(filter_for_scope(scope).len(), 1);
            assert!(expired_message_ids_for_scope(scope).is_empty());
        }
        assert_eq!(
            expired_message_ids_for_scope(main_a),
            HashSet::from([ids[0]])
        );
        let history = activation_history_for_scope(main_a);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].kind, SkillLifecycleEventKind::Activated);
        assert_eq!(
            history[1].kind,
            SkillLifecycleEventKind::Expired(SkillExpirationReason::ModeChange)
        );
        assert_eq!(history[1].message_id, Some(ids[0]));
    }

    #[test]
    fn context_epochs_are_scoped_and_cleared_with_the_session() {
        let _g = guard();
        let main_a = InvokedSkillScopeRef::new(Some("a"), None);
        let agent_a = InvokedSkillScopeRef::new(Some("a"), Some("child"));
        let main_b = InvokedSkillScopeRef::new(Some("b"), None);
        assert_eq!(context_epoch(main_a), 0);
        set_context_epoch(main_a, 7);
        set_context_epoch(agent_a, 8);
        set_context_epoch(main_b, 9);
        assert_eq!(context_epoch(main_a), 7);
        assert_eq!(context_epoch(agent_a), 8);
        assert_eq!(context_epoch(main_b), 9);
        clear_session("a");
        assert_eq!(context_epoch(main_a), 0);
        assert_eq!(context_epoch(agent_a), 0);
        assert_eq!(context_epoch(main_b), 9);
        set_context_epoch(main_b, 0);
        assert_eq!(context_epoch(main_b), 0);
    }

    #[test]
    fn reinvocation_reactivates_and_versions_original_exact_content() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("session"), None);
        let name = source("build");
        let full_body = source(&"x".repeat(40_000));
        let first_id = MessageId::new();
        let second_id = MessageId::new();
        let third_id = MessageId::new();
        register_scoped_for_message(&name, Path::new("/s"), &full_body, scope, first_id);
        let old_snapshot = filter_for_scope(scope);
        let old_activation = old_snapshot[0].activation_id.unwrap();
        // A budget write-back changes the stored restoration body, not the
        // version of the content originally produced by the Skill tool.
        assert_eq!(
            restore_post_compact_skills(old_snapshot.clone(), &[]).len(),
            1
        );
        expire_for_mode_change("session");
        register_scoped_for_message(&name, Path::new("/s"), &full_body, scope, second_id);
        let current = filter_for_scope(scope);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].content, full_body);
        assert_ne!(current[0].activation_id, Some(old_activation));
        assert!(restore_post_compact_skills(old_snapshot, &[]).is_empty());
        assert!(!write_back_if_current(
            &current[0].key,
            old_activation,
            &source("stale")
        ));
        assert_eq!(filter_for_scope(scope)[0].content, full_body);

        let changed = Utf16JsonProjection::root_string("�".into(), vec![0xD800]).unwrap();
        register_scoped_for_message(&name, Path::new("/s"), &changed, scope, third_id);
        let history = activation_history_for_scope(scope);
        let activations: Vec<_> = history
            .iter()
            .filter(|event| event.kind == SkillLifecycleEventKind::Activated)
            .collect();
        assert_eq!(
            activations
                .iter()
                .map(|event| event.content_version)
                .collect::<Vec<_>>(),
            [1, 1, 2]
        );
        assert_eq!(filter_for_scope(scope)[0].content, changed);
        assert_eq!(
            expired_message_ids_for_scope(scope),
            HashSet::from([first_id, second_id])
        );
    }

    #[test]
    fn content_hash_uses_exact_utf16_and_is_stable_across_scopes() {
        let _g = guard();
        let a = InvokedSkillScopeRef::new(Some("a"), None);
        let b = InvokedSkillScopeRef::new(Some("b"), None);
        let first = Utf16JsonProjection::root_string("�".into(), vec![0xD800]).unwrap();
        let second = Utf16JsonProjection::root_string("�".into(), vec![0xD801]).unwrap();
        register_scoped(&source("s"), Path::new("/s"), &first, a);
        register_scoped(&source("s"), Path::new("/s"), &first, b);
        let first_hash = activation_history_for_scope(a)[0].content_sha256;
        assert_eq!(
            first_hash,
            activation_history_for_scope(b)[0].content_sha256
        );
        let hex = first_hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "205022e3428b7c8276cf247b36e4e512db5651e5cb3472c253d9ee893a8ac750"
        );
        register_scoped(&source("s"), Path::new("/s"), &second, a);
        let history = activation_history_for_scope(a);
        let second_activation = history.last().unwrap();
        assert_ne!(first_hash, second_activation.content_sha256);
        assert_eq!(second_activation.content_version, 2);
    }

    #[test]
    fn definition_and_expanded_content_have_independent_versions() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("session"), None);
        let name = source("s");
        let definition = source("Run with $ARGUMENTS");
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &source("Run with first"),
            Some(&definition),
            scope,
            MessageId::new(),
        );
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &source("Run with second"),
            Some(&definition),
            scope,
            MessageId::new(),
        );
        let rows = activation_metadata_for_scope(scope);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].effective);
        assert_eq!(rows[0].content_version, 2);
        assert_eq!(rows[0].definition_version, 1);
        assert_eq!(rows[0].definition_sha256, content_sha256_for_test(&definition));
        assert_ne!(rows[0].content_sha256, rows[0].definition_sha256);

        let revised = source("Changed source body");
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &source("Run with second"),
            Some(&revised),
            scope,
            MessageId::new(),
        );
        let row = activation_metadata_for_scope(scope).remove(0);
        assert_eq!(row.content_version, 2);
        assert_eq!(row.definition_version, 2);
        assert_eq!(row.definition_sha256, content_sha256_for_test(&revised));
        expire_for_mode_change("session");
        assert!(!activation_metadata_for_scope(scope)[0].effective);
    }

    #[test]
    fn passive_restore_of_the_same_message_is_idempotent() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("session"), None);
        let name = source("s");
        let body = source("expanded value");
        let definition = source("template $ARGUMENTS");
        let id = MessageId::new();
        register_activation_with_message(
            &name,
            Path::new("/s"),
            &body,
            Some(&definition),
            scope,
            id,
        );
        let before = activation_metadata_for_scope(scope);
        register_activation_with_message(&name, Path::new("/s"), &body, None, scope, id);
        assert_eq!(activation_metadata_for_scope(scope), before);
        assert_eq!(activation_history_for_scope(scope).len(), 1);
        assert!(expired_message_ids_for_scope(scope).is_empty());
        assert_eq!(filter_for_scope(scope).len(), 1);
    }

    #[test]
    fn expired_entries_drop_bodies_and_are_bounded_without_losing_message_ids() {
        let _g = guard();
        let main = InvokedSkillScopeRef::new(Some("session"), None);
        let child = InvokedSkillScopeRef::new(Some("session"), Some("child"));
        let mut ids = Vec::new();
        for i in 0..(MAX_EXPIRED_ENTRIES + 4) {
            let id = MessageId::new();
            ids.push(id);
            register_scoped_for_message(
                &source(&format!("skill-{i}")),
                Path::new("/s"),
                &source("body"),
                main,
                id,
            );
        }
        register_scoped(
            &source("child"),
            Path::new("/s"),
            &source("still effective"),
            child,
        );
        expire_for_mode_change("session");
        let (expired_count, bodies_cleared) = expired_entries_for_test();
        assert_eq!(expired_count, MAX_EXPIRED_ENTRIES);
        assert!(bodies_cleared);
        assert_eq!(filter_for_scope(child).len(), 1);
        let retired = expired_message_ids_for_scope(main);
        assert_eq!(retired.len(), ids.len());
        assert!(
            retired.contains(&ids[0]),
            "entry eviction must not revive retained rows"
        );
        register_scoped(&source("skill-0"), Path::new("/s"), &source("body"), main);
        let reactivated = activation_history_for_scope(main).last().unwrap().clone();
        assert_eq!(
            reactivated.content_version, 1,
            "a local version restarts after eviction"
        );
        assert_eq!(reactivated.content_sha256, content_sha256_for_test(&source("body")));
    }

    #[test]
    fn completed_task_retires_exact_scope_and_history_is_bounded() {
        let _g = guard();
        let main = InvokedSkillScopeRef::new(Some("session"), None);
        let child = InvokedSkillScopeRef::new(Some("session"), Some("child"));
        let mut ids = Vec::new();
        for _ in 0..(MAX_LIFECYCLE_EVENTS + 2) {
            let id = MessageId::new();
            ids.push(id);
            register_scoped_for_message(&source("s"), Path::new("/s"), &source("same"), main, id);
        }
        register_scoped(&source("s"), Path::new("/s"), &source("child"), child);
        complete_task(main);
        assert!(filter_for_scope(main).is_empty());
        assert_eq!(filter_for_scope(child).len(), 1);
        assert_eq!(history_len_for_test(), MAX_LIFECYCLE_EVENTS);
        let retired = expired_message_ids_for_scope(main);
        assert_eq!(retired.len(), ids.len());
        assert!(
            retired.contains(&ids[0]),
            "audit eviction must not revive a retained message"
        );
        assert_eq!(
            activation_history_for_scope(main).last().unwrap().kind,
            SkillLifecycleEventKind::Expired(SkillExpirationReason::TaskCompleted)
        );

        prune_retired_message_ids(main, &HashSet::from([ids[0]]));
        assert_eq!(expired_message_ids_for_scope(main), HashSet::from([ids[0]]));
        clear_session("session");
        assert!(activation_history_for_scope(main).is_empty());
        assert!(expired_message_ids_for_scope(main).is_empty());
    }

    #[test]
    fn clearing_one_session_preserves_other_scopes() {
        let _g = guard();
        let session_a = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let session_b = InvokedSkillScopeRef::new(Some("sess:b"), None);
        register_scoped(&source("build"), Path::new("/a"), &source("a"), session_a);
        register_scoped(&source("build"), Path::new("/b"), &source("b"), session_b);

        clear_session("sess:a");

        assert!(filter_for_scope(session_a).is_empty());
        assert_eq!(filter_for_scope(session_b).len(), 1);
    }

    #[test]
    fn session_guard_clears_on_replace_and_drop() {
        let _g = guard();
        let session_a = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let session_b = InvokedSkillScopeRef::new(Some("sess:b"), None);
        register_scoped(&source("build"), Path::new("/a"), &source("a"), session_a);
        register_scoped(&source("build"), Path::new("/b"), &source("b"), session_b);

        let owner = InvokedSkillSessionGuard::new("sess:a".to_string());
        owner.replace("sess:b".to_string());
        assert!(filter_for_scope(session_a).is_empty());
        assert_eq!(filter_for_scope(session_b).len(), 1);

        drop(owner);
        assert!(filter_for_scope(session_b).is_empty());
    }

    // --- rRg write-back semantics (n_n) via restore_post_compact_skills ----- //

    #[test]
    fn staged_skill_compaction_only_writes_current_unchanged_main_activation() {
        let _g = guard();
        let main = InvokedSkillScopeRef::new(Some("session"), None);
        let child = InvokedSkillScopeRef::new(Some("session"), Some("child"));
        let original = source(&"x".repeat(40_000));
        register_scoped(&source("skill"), Path::new("/s"), &original, main);
        register_scoped(&source("skill"), Path::new("/s"), &source("child"), child);
        let (restored, writes) = crate::post_compact::stage_post_compact_skills(filter_for_scope(main), &[]);
        assert_eq!(restored.len(), 1);
        assert_eq!(writes.len(), 1);
        let write = &writes[0];
        assert!(filter_for_scope(main)[0].content.semantically_eq(&original));
        let concurrent = source("concurrent change");
        assert!(write_back_if_current(&write.key, write.activation_id, &concurrent));
        assert!(!write_back_if_unchanged(&write.key, write.activation_id, &write.original, &write.replacement));
        assert!(filter_for_scope(main)[0].content.semantically_eq(&concurrent));
        assert!(write_back_if_current(&write.key, write.activation_id, &original));
        assert!(write_back_if_unchanged(&write.key, write.activation_id, &write.original, &write.replacement));
        assert!(filter_for_scope(main)[0].content.semantically_eq(&restored[0].content));
        assert_eq!(text(&filter_for_scope(child)[0].content), "child");
    }

    #[test]
    fn restore_writes_back_truncated_content() {
        // A kept-and-truncated candidate persists the truncated content back to
        // the registry (`if(!c&&u!==a.content)n_n(s,u)`).
        let _g = guard();
        let big = "x".repeat(40_000); // caps to ~5_000 tokens
        register(&source("trunc"), Path::new("/t"), &source(&big), None);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 1);
        let stored = content_for_test(&units(":trunc")).expect("row present");
        assert!(
            text(&stored).ends_with(SKILL_TRUNCATION_MARKER),
            "truncated content persisted to the registry"
        );
        assert_eq!(stored, restored[0].content);
        assert_ne!(
            text(&stored),
            big,
            "the full body no longer occupies the row"
        );
    }

    #[test]
    fn restore_does_not_write_back_short_content() {
        // Short content is returned verbatim (`u===a.content`) → no write-back.
        let _g = guard();
        register(
            &source("short"),
            Path::new("/s"),
            &source("tiny body"),
            None,
        );
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 1);
        assert_eq!(text(&restored[0].content), "tiny body");
        assert_eq!(
            text(&content_for_test(&units(":short")).unwrap()),
            "tiny body"
        );
    }

    #[test]
    fn restore_body_match_counts_but_does_not_write_back() {
        // A body match (`c=true`) restores + counts toward the budget but never
        // writes back — the registry keeps the FULL (untruncated) body.
        let _g = guard();
        let big = "x".repeat(40_000);
        register(&source("bm"), Path::new("/b"), &source(&big), None);
        let already = [AttachedSkillContent::Body(source(&big))];
        let restored = restore_post_compact_skills(filter_for_agent(None), &already);
        assert_eq!(restored.len(), 1);
        assert!(text(&restored[0].content).ends_with(SKILL_TRUNCATION_MARKER));
        assert_eq!(
            text(&content_for_test(&units(":bm")).unwrap()),
            big.as_str(),
            "body match → no write-back; registry unchanged"
        );
    }

    #[test]
    fn restore_clears_registry_on_budget_overflow() {
        // Six equal ~5_000-token skills: exactly 5 fit (25_000) and 1 overflows;
        // the overflowed row's content is CLEARED (`n_n(s,"")`).
        let _g = guard();
        for i in 0..6 {
            register(
                &source(&format!("s{i}")),
                Path::new("/s"),
                &source(&"x".repeat(40_000)),
                None,
            );
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 5, "5 fit, 6th overflows");
        let cleared: Vec<String> = (0..6)
            .map(|i| format!(":s{i}"))
            .filter(|k| content_for_test(&units(k)).is_some_and(|value| text(&value).is_empty()))
            .collect();
        assert_eq!(cleared.len(), 1, "exactly the overflowed row is cleared");
    }

    #[test]
    fn restore_sorts_by_invoked_at_desc() {
        // DESC by invokedAt: the most recently invoked skill leads the result.
        let _g = guard();
        register(&source("old"), Path::new("/o"), &source("old body"), None);
        std::thread::sleep(std::time::Duration::from_millis(3));
        register(&source("new"), Path::new("/n"), &source("new body"), None);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 2);
        assert_eq!(text(&restored[0].name), "new", "most recent first");
        assert_eq!(text(&restored[1].name), "old");
    }

    #[test]
    fn restore_preserves_exact_utf16_across_repeated_compactions() {
        let _g = guard();
        let big = format!("{}😀{}", "x".repeat(19_899), "y".repeat(200));
        register(&source("repeat"), Path::new("/r"), &source(&big), None);

        let first = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(first.len(), 1);
        let first_exact = first[0].content.string_units("").unwrap();
        assert!(
            first_exact
                .windows(2)
                .any(|w| w == [0xD83D, 0x000A] || w == [0xD83D, 0x005B]),
            "wire image must retain the split surrogate at the truncation boundary"
        );

        let second = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content.string_units("").unwrap(), first_exact);
    }

    #[test]
    fn skill_name_and_body_keep_unpaired_units_and_distinct_keys() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("sess"), None);
        let replacement =
            Utf16JsonProjection::root_string("�a".into(), vec![0xD800, 0x0061]).unwrap();
        let other = Utf16JsonProjection::root_string("�a".into(), vec![0xD801, 0x0061]).unwrap();
        let body =
            Utf16JsonProjection::root_string("x�y".into(), vec![0x0078, 0xDFFF, 0x0079]).unwrap();
        register_scoped(&replacement, Path::new("/one"), &body, scope);
        register_scoped(&other, Path::new("/two"), &source("other"), scope);
        let restored = restore_post_compact_skills(filter_for_scope(scope), &[]);
        assert_eq!(restored.len(), 2);
        let first = restored
            .iter()
            .find(|skill| skill.name.string_units("").unwrap() == vec![0xD800, 0x0061])
            .expect("first exact key survives");
        let second = restored
            .iter()
            .find(|skill| skill.name.string_units("").unwrap() == vec![0xD801, 0x0061])
            .expect("second exact key survives");
        assert_eq!(first.name.string_units("").unwrap(), vec![0xD800, 0x0061]);
        assert_eq!(
            first.content.string_units("").unwrap(),
            vec![0x0078, 0xDFFF, 0x0079]
        );
        assert_eq!(second.name.string_units("").unwrap(), vec![0xD801, 0x0061]);
    }
}
