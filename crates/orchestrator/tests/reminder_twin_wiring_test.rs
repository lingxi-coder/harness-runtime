//! Anti-drift: every per-turn reminder must reach BOTH turn drivers.
//!
//! claude-code has ONE main loop. LingXi has two — the batched `turn_loop.rs`
//! and the streaming driver under `conversation/drivers` — and a reminder wired
//! into only one of them is invisible in exactly one mode. That failure is
//! silent: the feature works when you test it, and does nothing in production
//! if production runs the other driver.
//!
//! The two drivers now share ONE collector (`conversation/drivers/prepare.rs`),
//! so the shape of the check moved with it. Instead of counting the same
//! producer in two hand-maintained fan-outs, it pins the two halves that
//! replaced them:
//!
//! 1. the shared collector invokes every producer exactly once, and
//! 2. each driver routes through that collector exactly once, and reaches NO
//!    producer behind its back.
//!
//! Half 2 is what keeps half 1 meaningful: a shared collector nothing calls is
//! the same silent regression in a new costume, and a reminder open-coded into
//! one driver instead of the collector is the original bug exactly.
//!
//! This is a SOURCE-level check, and it is honest about being weak: it proves
//! each producer is reached, not that the message is pushed in the right
//! position or at all. `tests/streaming_vs_batched_equivalence_test.rs` drives
//! both real entries and pins the ORDER of the producers that fire there; note
//! that its byte-equality assertion cannot see a reorder INSIDE the collector,
//! because one function feeds both paths. Order is pinned by that file's
//! `assert_shared_reminder_order`, not by cross-path equality.

const BATCHED: &str = concat!(
    include_str!("../src/turn_loop.rs"),
    include_str!("../src/turn_loop/api_recovery.rs"),
);
const STREAMING: &str = concat!(
    include_str!("../src/conversation/drivers/mod.rs"),
    include_str!("../src/conversation/drivers/streaming.rs"),
);
const COLLECTOR: &str = include_str!("../src/conversation/drivers/prepare.rs");
const PROMPT_PIPELINE: &str = include_str!("../src/conversation/prompt.rs");

/// The collector's own file, excluded from the driver scan below.
///
/// Unlike the driver set, this is pinned to ONE file. If the collector is ever
/// split, the failures land in the safe direction — the moved producers read as
/// missing from the collector AND as direct driver calls, so two checks go red
/// rather than one going quietly green — but the second message will blame "the
/// driver source". Update this constant and [`COLLECTOR`] together.
const COLLECTOR_FILE: &str = "prepare.rs";

/// Every file a turn driver can live in, as (label, source).
///
/// Enumerated from disk rather than `include_str!`ed by name on purpose. The
/// unified-driver plan adds `loop_state.rs` and `disposition.rs` under
/// `drivers/`, and a producer open-coded into a file this list did not happen
/// to name would be invisible to every check below — the exact regression they
/// exist to catch, arriving through the door they do not watch.
/// Every `.rs` file under `dir`, at any depth.
fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn driver_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = vec![(
        "turn_loop.rs".to_string(),
        std::fs::read_to_string(root.join("src/turn_loop.rs")).expect("read src/turn_loop.rs"),
    )];

    // Recursive: a driver split into `drivers/streaming/mod.rs` must not slip
    // past the way a flat listing would let it.
    let drivers_dir = root.join("src/conversation/drivers");
    let mut driver_files = Vec::new();
    collect_rs_files(&drivers_dir, &mut driver_files);
    driver_files.sort();
    for path in driver_files {
        if path.file_name().is_some_and(|name| name == COLLECTOR_FILE) {
            continue;
        }
        let label = format!(
            "drivers/{}",
            path.strip_prefix(&drivers_dir)
                .expect("driver path is under drivers/")
                .to_string_lossy()
        );
        let source = std::fs::read_to_string(&path).expect("read driver source");
        sources.push((label, source));
    }

    // Coverage self-check. A scan that silently found nothing — wrong working
    // directory, renamed module — would report every check below as clean,
    // which is worse than no check at all.
    let found: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        found.contains(&"turn_loop.rs") && found.contains(&"drivers/mod.rs"),
        "the driver scan must cover at least the two known drivers; it found {found:?}"
    );
    sources
}

/// Every producer the shared collector must invoke, in no particular order —
/// position is not what this file checks. Add a row when you add a reminder:
/// forgetting to wire it into the collector fails
/// `shared_collector_invokes_every_per_turn_reminder`, and wiring it into one
/// driver instead fails `neither_driver_reaches_a_reminder_behind_the_collector`.
const TWIN_REMINDERS: &[&str] = &[
    "brief_mode_reminder_message",
    "output_style_reminder_message",
    "plan_mode_turn_messages",
    "plan_mode_exit_message",
    "skill_listing_reminder_message",
    "conditional_rules_reminder_messages",
    "nested_memory_reminder_messages",
    "new_diagnostics_reminder_message",
    "agent_listing_reminder_message",
    "changed_files_reminder_messages",
    "todo_reminder_message",
    "tool_search_usage_reminder_message",
    "async_hook_response_reminder_message",
    "task_notification_reminder_messages_in_turn",
    "memory_update_reminder_messages",
    "relevant_memory_reminder_messages",
    "skill_discovery_reminder_message",
    "silent_turn_reminder_message",
    "total_tokens_reminder_message",
    // Moved into the collector with `prepare_turn_step` (PR 2). They are
    // PREPENDED rather than appended to `transient`, but they are per-turn
    // reminders both drivers must get, and they are computed once per step for
    // the same reason as the rest.
    "deferred_tools_reminder_message",
    "date_change_reminder_message",
];

/// Count CALL sites of `name`, wherever rustfmt put the receiver.
///
/// The leading dot is what makes this a call rather than a mention: a doc link
/// spells the same method `Self::name` or `` `name` `` with no parenthesis, and
/// a wrapped call (`self\n    .name(arg)`) still carries the dot.
///
/// The previous matcher was `receiver.name().await`, which scores zero for any
/// call rustfmt wrapped or that takes an argument — two of the producers here
/// are both. A matcher that under-counts is the dangerous direction for a
/// presence check, because the failure looks like a missing call.
fn call_sites(src: &str, name: &str) -> usize {
    src.matches(&format!(".{name}(")).count()
}

/// A row-count canary for [`TWIN_REMINDERS`].
///
/// The list above is hand-maintained, so on its own it cannot notice a producer
/// ADDED to the collector and not listed — the very omission it exists to
/// catch, just one level up. Counting the producer-shaped calls in the
/// collector and comparing against the list closes that: a new reminder makes
/// the numbers disagree until its row is added.
///
/// Counting leans on the naming convention: every producer is named
/// `*_message`, `*_messages` or `*_messages_in_turn` (`persist_message_to_jsonl`
/// does not match, because the `(` must follow). A future producer named
/// outside that convention would not be counted here and would need its row
/// added by hand — the same hazard one level further out, and the reason the
/// per-name checks below exist as well rather than this canary alone.
#[test]
fn twin_reminder_list_covers_every_producer_in_the_collector() {
    let producer_calls = COLLECTOR.matches("_message(").count()
        + COLLECTOR.matches("_messages(").count()
        + COLLECTOR.matches("_messages_in_turn(").count();
    assert_eq!(
        producer_calls,
        TWIN_REMINDERS.len(),
        "the collector makes {producer_calls} producer calls but TWIN_REMINDERS lists \
         {}. Add the new reminder's row above — an unlisted producer is invisible to \
         every other check in this file.",
        TWIN_REMINDERS.len()
    );
}

#[test]
fn shared_collector_invokes_every_per_turn_reminder() {
    for name in TWIN_REMINDERS {
        assert_eq!(
            call_sites(COLLECTOR, name),
            1,
            "{name} must be invoked exactly once by the shared collector \
             (conversation/drivers/prepare.rs). Both turn drivers read their \
             per-turn reminders from there; a producer missing here is missing \
             from BOTH paths."
        );
    }
}

#[test]
fn neither_driver_reaches_a_reminder_behind_the_collector() {
    let sources = driver_sources();

    // Exactly one route in, per driver: the batched one in turn_loop.rs, the
    // streaming one somewhere under drivers/ (which file is not pinned — the
    // plan moves that code — but the COUNT is).
    let batched_entries = sources
        .iter()
        .filter(|(name, _)| name == "turn_loop.rs")
        .map(|(_, src)| call_sites(src, "prepare_turn_step"))
        .sum::<usize>();
    assert_eq!(
        batched_entries, 1,
        "the BATCHED driver (turn_loop.rs) must call prepare_turn_step exactly once; \
         the shared preparation only protects paths that go through it"
    );
    let streaming_entries = sources
        .iter()
        .filter(|(name, _)| name.starts_with("drivers/"))
        .map(|(_, src)| call_sites(src, "prepare_turn_step"))
        .sum::<usize>();
    assert_eq!(
        streaming_entries, 1,
        "the STREAMING driver (somewhere under conversation/drivers/) must call \
         prepare_turn_step exactly once across all its files"
    );

    // And nothing reaches the reminder collector around it: `collect_turn_reminders`
    // is `prepare_turn_step`'s to call, so a driver calling it directly has
    // prepared twice.
    for (file, source) in &sources {
        assert_eq!(
            call_sites(source, "collect_turn_reminders"),
            0,
            "{file} calls collect_turn_reminders directly. It belongs to \
             prepare_turn_step; a second call drains this step's consume-once \
             sources again, and the second drain returns None."
        );
    }

    // And no way around it. A producer called straight from a driver is the
    // single-driver wiring bug this file was written for; adding it to the
    // collector instead is the fix, not adding it to both drivers.
    for (file, source) in &sources {
        for name in TWIN_REMINDERS {
            assert_eq!(
                call_sites(source, name),
                0,
                "{name} is invoked directly by the driver source {file}. Per-turn \
                 reminders belong in the shared collector \
                 (conversation/drivers/{COLLECTOR_FILE}) so BOTH drivers get them; a \
                 direct call here is live on one path only."
            );
        }
    }
}

/// Every request assembly must carry THIS step's per-turn reminders.
///
/// Computing a reminder advances session state — sent-sets, delta trackers,
/// consume-once drains — so a retry can never recompute one: it comes back
/// `None` and the reminder is lost for the rest of the session. Only
/// `deferred_tools` and `date_change` used to be reused this way; every other
/// reminder was silently dropped whenever a request was rebuilt from raw
/// `session.history` (context-overflow recovery, the 529 fallback, the
/// non-streaming fallback, the PTL truncation retry, the post-compact retry).
///
/// Structural, like the check above, and for the same reason: there is no
/// harness that can drive a real recovery and inspect the retried request. It
/// pins the invariant "wherever a snapshot is rebuilt, the reminders go back
/// on", which is exactly what was violated.
/// Both drivers now route every REBUILD through `reattach_outgoing_context`,
/// which prepends the leading context and re-appends this step's reminders in
/// one place. That is strictly stronger than the previous shape (N open-coded
/// assemblies that each had to remember), so this check moved with it: instead
/// of counting duplicated `insert`/`extend` lines, it pins that each driver has
/// exactly ONE hand-rolled assembly and that every other assembly goes through
/// the shared helper.
///
/// The tripwire property is unchanged, and so is the rule for reading a
/// failure: when it fires, the question is "does the new assembly re-append?"
/// A new site that routes through `reattach_outgoing_context` is a legitimate
/// bump; a new site that hand-rolls its own snapshot is the bug this test
/// exists to catch, and bumping the number to silence it defeats the check.
#[test]
fn every_rebuilt_request_snapshot_re_appends_the_turn_reminders() {
    const EXTEND: &str = "extend(turn_reminders.iter().cloned())";

    // ONE hand-rolled assembly in total now, and it is the shared preparation's.
    // Before PR 2 there was one per driver; a driver that grows its own again
    // has stopped going through `prepare_turn_step`.
    assert_eq!(
        COLLECTOR
            .matches("self.prepend_leading_context(&mut snapshot).await")
            .count(),
        1,
        "the shared preparation must hand-roll exactly one assembly (the main \
         path); any other must go through reattach_outgoing_context"
    );
    for (file, source) in &driver_sources() {
        assert_eq!(
            source.matches("prepend_leading_context(").count(),
            0,
            "{file} assembles its own leading context. Since PR 2 that is \
             prepare_turn_step's job, and a second assembly is a second preparation."
        );
    }

    // Every rebuild-from-raw-history path routes through the shared helper:
    // the 529 fallback, PTL recovery and the non-streaming fallback in
    // streaming; context hint, context collapse and reactive compaction in
    // batched. The destructive main-request head-truncation retry is gone.
    assert_eq!(
        STREAMING.matches("orch.reattach_outgoing_context(").count(),
        3,
        "STREAMING rebuilds (529 fallback, PTL retry, non-streaming fallback) \
         must each reattach; a rebuild that does not silently drops reminders"
    );
    assert_eq!(
        BATCHED.matches("orch.reattach_outgoing_context(").count(),
        3,
        "BATCHED recovery and context-collapse rebuilds must each reattach"
    );

    // Routing through the helper only proves anything while the helper still
    // re-appends. The streaming main path lives in the driver; the shared
    // `reattach_outgoing_context` helper lives in the prompt pipeline.
    assert_eq!(
        COLLECTOR.matches(EXTEND).count(),
        1,
        "the shared preparation's main path must append this step's reminders"
    );
    assert_eq!(
        PROMPT_PIPELINE.matches(EXTEND).count(),
        1,
        "the shared prompt pipeline must re-append reminders on every rebuild"
    );
    assert_eq!(
        BATCHED.matches(EXTEND).count(),
        0,
        "the batched driver no longer appends reminders itself; preparation does, \
         and its rebuilds go through reattach_outgoing_context"
    );
}
