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
const REMINDER_PRODUCERS: &str = include_str!("../src/conversation/reminders.rs");

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
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_tests.rs"))
        {
            // `prepare.rs` includes these only behind `#[cfg(test)]`; they
            // exercise the collector but cannot be reachable from a runtime
            // driver and must not inflate the production call-site count.
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

/// Every per-turn producer in the shared preparation path, with its expected
/// number of call sites. Conditional rules are produced inside
/// `nested_memory_reminder_messages`; they are not a separate call site. The
/// deferred-tools attachment is produced before reminder collection and has a
/// separate guard below. Task notifications have two mutually exclusive call
/// sites: the bare-mode early return and the ordinary reminder fan-out.
const TWIN_REMINDERS: &[(&str, usize)] = &[
    ("brief_mode_reminder_message", 1),
    ("output_style_reminder_message", 1),
    ("plan_mode_turn_messages", 1),
    ("plan_mode_exit_message", 1),
    ("skill_listing_reminder_message", 1),
    ("nested_memory_reminder_messages", 1),
    ("new_diagnostics_reminder_message", 1),
    ("agent_listing_reminder_message", 1),
    ("changed_files_reminder_messages", 1),
    ("todo_reminder_message", 1),
    ("tool_search_usage_reminder_message", 1),
    ("async_hook_response_mod_messages", 1),
    ("task_notification_reminder_messages_in_turn", 2),
    ("memory_update_reminder_messages", 1),
    ("relevant_memory_reminder_messages", 1),
    ("skill_discovery_reminder_message", 1),
    ("silent_turn_reminder_message", 1),
    ("total_tokens_reminder_message", 1),
    ("mcp_instructions_reminder_message", 1),
    // These model-facing rows are positioned separately from `transient`, but
    // are produced in the shared preparation path for both turn drivers.
    // The typed deferred-tools row is produced earlier in `prepare_turn_step`;
    // this API-system projection is the per-step row still produced here.
    ("native_tool_change_api_system_message", 1),
    ("date_change_reminder_message", 1),
];

/// Count method and qualified-function CALL sites of `name`.
///
/// Parentheses make this a call rather than a mention, and both forms tolerate
/// rustfmt wrapping: `self\n    .name(arg)` and
/// `module::\n    name(arg)`.
///
/// The previous matcher was `receiver.name().await`, which scores zero for any
/// call rustfmt wrapped or that takes an argument — two of the producers here
/// are both. A matcher that under-counts is the dangerous direction for a
/// presence check, because the failure looks like a missing call.
fn call_sites(src: &str, name: &str) -> usize {
    src.matches(&format!(".{name}(")).count() + src.matches(&format!("::{name}(")).count()
}

fn compact_whitespace(src: &str) -> String {
    src.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn source_between<'a>(src: &'a str, start_marker: &str, end_marker: &str) -> &'a str {
    let start = src
        .find(start_marker)
        .unwrap_or_else(|| panic!("missing source marker {start_marker:?}"));
    let tail = &src[start..];
    let end = tail
        .find(end_marker)
        .unwrap_or_else(|| panic!("missing source marker {end_marker:?} after {start_marker:?}"));
    &tail[..end]
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
fn twin_reminder_list_covers_every_producer_in_shared_preparation() {
    let message_shaped_calls = COLLECTOR.matches("_message(").count()
        + COLLECTOR.matches("_messages(").count()
        + COLLECTOR.matches("_messages_in_turn(").count();
    // These calls only render previously-created model-history rows; they do
    // not produce a per-turn reminder. Their suffix matches the producer
    // naming convention, so exclude the two explicit formatting call sites.
    let rendered_history_rows = COLLECTOR.matches("rendered_system_message(").count();
    assert_eq!(rendered_history_rows, 2);
    let producer_calls = message_shaped_calls - rendered_history_rows;
    let listed_calls = TWIN_REMINDERS
        .iter()
        .map(|(_, expected_calls)| expected_calls)
        .sum::<usize>();
    assert_eq!(
        producer_calls, listed_calls,
        "shared preparation makes {producer_calls} producer calls but TWIN_REMINDERS lists \
         {listed_calls} call sites. Add the new reminder's row above — an unlisted \
         producer is invisible to every other check in this file."
    );
}

#[test]
fn shared_preparation_invokes_every_per_turn_reminder() {
    for (name, expected_calls) in TWIN_REMINDERS {
        assert_eq!(
            call_sites(COLLECTOR, name),
            *expected_calls,
            "{name} must be invoked {expected_calls} time(s) by shared per-turn \
             preparation (conversation/drivers/prepare.rs). Both turn drivers read their \
             per-turn reminders from there; a producer missing here is missing \
             from BOTH paths."
        );
    }
}

#[test]
fn bare_and_full_paths_each_collect_task_notifications_once() {
    const TASK_NOTIFICATIONS: &str = "task_notification_reminder_messages_in_turn";
    let bare_path = source_between(
        COLLECTOR,
        "if self.config.bare {",
        "let mut transient = Vec::new();",
    );
    let full_path = &COLLECTOR[COLLECTOR
        .find("let mut transient = Vec::new();")
        .expect("the non-bare collector path starts with its transient list")..];

    assert_eq!(call_sites(bare_path, TASK_NOTIFICATIONS), 1);
    assert_eq!(call_sites(full_path, TASK_NOTIFICATIONS), 1);
}

#[test]
fn conditional_rules_remain_owned_by_the_nested_memory_producer() {
    let nested_memory = source_between(
        REMINDER_PRODUCERS,
        "pub async fn nested_memory_reminder_messages(",
        "pub(super) async fn discard_stale_prefetches(",
    );
    assert_eq!(call_sites(nested_memory, "load_conditional_rules"), 1);
    assert_eq!(
        nested_memory.matches("rule_matches_touched_file(").count(),
        1
    );
    assert!(compact_whitespace(nested_memory)
        .contains(".persist_nested_memory_files(rules,&trigger,&cwd,&prior)"));
}

#[test]
fn staged_prompt_ingress_is_consumed_without_duplicate_producer_calls() {
    let prepare_step = source_between(
        COLLECTOR,
        "pub(crate) async fn prepare_turn_step(",
        "pub(crate) fn collect_turn_reminders<'a>(",
    );
    assert_eq!(
        call_sites(prepare_step, "deferred_tools_prompt_ingress_attachment"),
        2,
        "deferred_tools_delta is produced once from the Native NAr cache-aware \
         branch and once from the legacy branch; the branches are mutually exclusive"
    );
    let compact_prepare_step = compact_whitespace(prepare_step);
    assert!(compact_prepare_step.contains(
        ".deferred_tools_prompt_ingress_attachment(&current_wire_tool_names,self.native_tool_pool_refresh_callback_available(),)"
    ));
    assert!(compact_prepare_step
        .contains("self.deferred_tools_prompt_ingress_attachment(&current_wire_tool_names,None)"));
    assert!(compact_prepare_step.contains(
        "cache_prompt_ingress_producer_rows_pending(self,\"deferred_tools_delta\",rows.clone(),)"
    ));

    let snapshot_ingress = source_between(
        COLLECTOR,
        "let mut snapshot = prepared_call.history_snapshot;",
        "let history = self.session.lock().await.model_context_history();",
    );
    assert_eq!(
        snapshot_ingress
            .matches("::staged_prompt_ingress_attachments(")
            .count(),
        1
    );
    assert!(snapshot_ingress.contains("snapshot.retain(|row| !ingress_ids.contains(&row.id()))"));

    let reminder_collector = source_between(
        COLLECTOR,
        "pub(crate) fn collect_turn_reminders<'a>(",
        "pub(crate) fn tool_result_reminder_suffixes_for_snapshot(",
    );
    assert_eq!(
        reminder_collector
            .matches("::staged_prompt_ingress_attachments(")
            .count(),
        2,
        "the bare path checks whether rows are staged before persisting task \
         notifications; the non-bare path snapshots them for consumption"
    );
    let bare_path = source_between(
        reminder_collector,
        "if self.config.bare {",
        "let mut transient = Vec::new();",
    );
    assert_eq!(
        bare_path
            .matches("::staged_prompt_ingress_attachments(")
            .count(),
        1,
        "bare mode performs a non-consuming presence check before it exits"
    );
    let full_path = &reminder_collector[reminder_collector
        .find("let mut transient = Vec::new();")
        .expect("the non-bare collector path starts with its transient list")..];
    assert_eq!(
        full_path
            .matches("::staged_prompt_ingress_attachments(")
            .count(),
        1,
        "the non-bare path snapshots the staged rows once for consumption"
    );

    let staged_settings_loop = source_between(
        reminder_collector,
        "for kind in [",
        "if let Some(reminder) = self.brief_mode_reminder_message()",
    );
    let staged_settings_kinds = source_between(reminder_collector, "for kind in [", "]")
        .strip_prefix("for kind in [")
        .expect("the settings-row loop starts with its kind list")
        .split(',')
        .map(|kind| kind.trim().trim_matches('"'))
        .filter(|kind| !kind.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        staged_settings_kinds,
        vec![
            "environment",
            "model",
            "output_style_instructions",
            "language"
        ],
        "the staged row projection preserves Native's environment/model rows, \
         then output-style/language deltas in source order"
    );
    let compact_settings_loop = compact_whitespace(staged_settings_loop);
    assert!(compact_settings_loop.contains("ifletSome(rows)=&staged_ingress"));
    assert!(compact_settings_loop
        .contains("rows.iter().filter(|row|row.payload.string_at(\"/type\")==Some(kind))"));
    assert!(compact_settings_loop.contains(".map(|row|row.message.clone())"));

    for kind in [
        "agent_listing_delta",
        "skill_listing",
        "total_tokens_reminder",
        "budget_usd",
    ] {
        assert!(
            COLLECTOR.contains(&format!("staged(\"{kind}\")")),
            "staged ingress type {kind} must be consumed as a row"
        );
    }
    assert!(COLLECTOR.contains("else if staged_ingress.is_none()"));
    assert!(COLLECTOR.contains("let total_tokens = if staged_ingress.is_none()"));
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
        for (name, _) in TWIN_REMINDERS {
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
        call_sites(COLLECTOR, "prepend_leading_context"),
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
