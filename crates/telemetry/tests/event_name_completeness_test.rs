//! `ALL_EVENT_NAMES` matches the parity fixture byte-for-byte (positive case
//! is in test-harness's `parity_tengu_events.rs`; this test guards the count
//! and uniqueness without crossing the workspace boundary).

use telemetry::tengu::ALL_EVENT_NAMES;

#[test]
fn registry_is_exactly_410_entries() {
    // M4-05 added 24 events (8 agent/task tools × 3 lifecycle stages),
    // M4-06 added 6 (2 team tools × 3 lifecycle stages),
    // M4-07 added 13 (1 MCP_STARTED + 4 new tools × 3 lifecycle stages),
    // M4-08 added 24 (8 system tools × 3 lifecycle stages),
    // M4-09 added 1 (release marker `lingxi_core_v0_5_0_released`),
    // M5-02 added 3 (orchestrator conversation lifecycle: started/completed/failed),
    // M5-03 added 0,
    // M5-04 added 2 streaming events (turn_streaming_started/completed),
    // M5-05 added 2 permission events (permission_prompted/answered),
    // M5-06 added 8 hook events (pre/post started/completed/failed,
    //   http_skipped_ssrf, timeout),
    // M5-07 added 3 session-jsonl events (appended/rotated/corrupted),
    // M5-08 added 2 session-resume events (resume_started/completed),
    // M5-10 added 18 command events (6 batch-1 commands × 3 phases),
    // M5-11 added 36 command events (12 batch-2 commands × 3 phases):
    // 276 (post-M5-10) + 36 (M5-11) = 312.
    // M5-13 added 2 REPL session lifecycle events (repl_session_started/ended):
    // 312 + 2 = 314.
    // M5-14 added 1 release marker `lingxi_core_v0_6_0_released`:
    // 314 + 1 = 315.
    // M6-01 added 4 TUI lifecycle events (session_started/ended,
    // first_render, resize): 315 + 4 = 319.
    // M6-03 added 2 TUI streaming render events (streaming_render_started/ended):
    // 319 + 2 = 321.
    // M6-05 added 2 TUI permission-dialog events
    // (permission_dialog_shown/resolved): 321 + 2 = 323.
    // M6-06/07/08 added 0 (engine wiring reused existing M3/M4/M5 events).
    // M6-09 added 3: lingxi_core_v0_7_0_released (release marker) +
    // tengu_tui_scroll_started/scroll_ended (real emit sites in
    // app::scroll_with_viewport). tengu_tui_key_pressed deferred to M7
    // (no aggregator infra). 323 + 3 = 326.
    // M7-01..M7-15 added 0 (every TUI event candidate was deferred to the
    // M7-16 audit; the renderer/screen/vim/palette sub-plans registered none).
    // M7-16 added 4: lingxi_core_v0_8_0_released (release marker) +
    // tengu_tui_screen_opened/screen_closed/search_opened (real emit sites in
    // AppState::open_*/close_screen + MessageSelectorState::open/open_export).
    // tengu_tui_command_palette_opened/vim_mode_entered/key_pressed stay
    // deferred to M8 (no clean/aggregated emit site). 326 + 4 = 330.
    // CronDelete/CronList added 6 (2 tools × 3 lifecycle stages): 330 + 6 = 336.
    // FileReadTool analytics added 4 at the global tail (tengu_file_read_dedup,
    // tengu_session_file_read, tengu_file_read_limits_override,
    // tengu_file_read_reread [#13]): 336 + 4 = 340.
    // Config migrations added 9 as their own tail block (migration::NAMES,
    // runMigrations port): 340 + 9 = 349.
    // Permission flow added 1 as its own tail block (permission::NAMES,
    // bypass dialog accept): 349 + 1 = 350.
    // Coordinator swarm added 3 as its own global-tail block
    // (coordinator::NAMES — tengu_team_created, tengu_team_deleted,
    // tengu_coordinator_mode_switched): 344 + 3 = 347.
    // Grep/Glob telemetry removed: claude-code v2.1.183 emits NO
    // tengu_tool_grep_* / tengu_tool_glob_* events, so the 6 fabricated
    // names were dropped from the tool block (tool block 140 → 134),
    // shifting the total from 353 → 347.
    // Strict-parity (2.1.195) removed three classes of port/fabricated events,
    // reconciled here against the ACTUAL per-block NAMES lengths:
    //   - D1: tengu_tool_todo_write_{started,completed,failed} (tool 134 → 131)
    //   - D2: port-only tengu_cost_recorded (cost 10 → 9)
    //   - D3: session-resume consolidation (session 20 → 18)
    // 347 - 3 - 1 - 2 = 341.
    // 2.1.198 M2: AWS auth-refresh trust-gate events added as their own
    // global-tail block (oauth::AWS_AUTH_NAMES —
    // tengu_awsAuthRefresh_missing_trust,
    // tengu_awsCredentialExport_missing_trust): 341 + 2 = 343.
    // Worktree 2.1.206 parity added 2 byte-exact single-success events to the
    // tool block (WORKTREE_CREATED = tengu_worktree_created,
    // WORKTREE_ENTERED_EXISTING = tengu_worktree_entered_existing — the
    // `EnterWorktree` byte-exact events ADDITIONAL to the port's own
    // started/completed/failed lifecycle triad): tool block 131 → 133,
    // 343 + 2 = 345.
    // ExitWorktree 2.1.206 parity added 2 more byte-exact single-success
    // events to the tool block (WORKTREE_KEPT = tengu_worktree_kept,
    // WORKTREE_REMOVED = tengu_worktree_removed): tool block 133 → 135,
    // 345 + 2 = 347.
    // 2.1.251 byte-alignment B8 (telemetry-modules) added two new
    // GLOBAL-TAIL blocks, appended after oauth::AWS_AUTH_NAMES:
    //   - mcp::NAMES (+2 — tengu_mcp_server_config_invalid,
    //     tengu_mcp_tools_listed): 347 + 2 = 349.
    //     ⚠️ RETRACTED CLAIM: this comment used to say these were "the ONLY
    //     two confirmed real `tengu_mcp_*` analytics events at the oracle —
    //     the ~100 other `tengu_mcp_*` binary strings are Statsig
    //     feature-flag names, not events". The first half is FALSE. The
    //     analytics-bus call shape `s("tengu_mcp_<name>"` matches 55 DISTINCT
    //     event names in 2.1.251. Many `tengu_mcp_*` strings really are
    //     Statsig flags — that is the true half — but the event set is 55,
    //     not 2. The registry now pins 44 of them, leaving 11 OPEN gaps.
    //     Reading this count as "MCP telemetry is complete" is
    //     exactly the error the retracted wording invited.
    //   - plugin::NAMES (+14 — tengu_plugin_enabled_for_session and its 13
    //     siblings; the port had NO plugin telemetry module before this):
    //     349 + 14 = 363.
    //     ⚠️ SUBSTRATE ONLY: none of the 14 has a production emit site
    //     anywhere in the repo, so §20c is OPEN. See plugin.rs's module doc.
    // 2.1.251 §20a/§20b wiring: mcp::NAMES +1 (tengu_mcp_degraded — traced
    // to `yn`'s per-server tool-schema-classification tail and `qr`'s
    // process-global validator-unavailable fallback; this closes the
    // `normalizedCount`/`keptCount` open question the mcp.rs module doc
    // flagged when tengu_mcp_tools_listed was first wired): 363 + 1 = 364.
    // §11 discovery-cache Stage 1 added mcp::NAMES's 4th entry
    // (tengu_mcp_discovery_source, DISCOVERY_SOURCE — see mcp.rs's module
    // doc for the two oracle call sites, `Ko`-gated on the miss side): mcp
    // block 3 → 4, 364 + 1 = 365. Registering the already-emitted
    // tengu_mcp_tool_auto_backgrounded in the MCP global-tail block adds one:
    // 365 + 1 = 366.
    // 2.1.252 registry/catalog/connect slice adds 4 more MCP analytics names
    // to the same MCP tail block:
    //   - tengu_mcp_server_connection_succeeded
    //   - tengu_mcp_server_connection_failed
    //   - tengu_mcp_list_changed
    //   - tengu_mcp_resource_templates_fetched
    // MCP block 5 → 9, 366 + 4 = 370. `mcp serve` startup parity then appends
    // one more MCP analytics name (`tengu_mcp_start`) at the same tail block's
    // end, preserving append-only order: 370 + 1 = 371.
    // Modern `subscriptions/listen` recovery plus `reset_mcpjson_choices`
    // added 2 more MCP analytics names, and the provider-neutral OAuth/auth
    // family adds 12 more (authenticate, clear, browser_open, flow
    // start/success/error, refresh success/failure, token_persist_failed,
    // issuer_echo_mismatch, server_needs_auth, tool_call_auth_error):
    // 371 + 14 = 385.
    // The plugin tail block later grew from 14 to 24 names (additional CLI
    // entry points plus prune/state-file observability), so the compiled
    // registry is now 385 + 10 = 395.
    // 2.1.252 session persistence-failure parity appends one more
    // `tengu_session_*` event, growing the registry to 396.
    // Provider-neutral, non-deferred MCP analytics parity then appends 20 more
    // MCP tail names (`add`, `delete`, `get`, `list`, `login`, `logout`,
    // `command_inline`, `elicitation_shown`, `elicitation_response`,
    // `input_missing_required`, `large_result_handled`, `pending_call`,
    // `servers`, `tool_result_ended_turn`, `tools_commands_loaded`,
    // `tools_refreshed_mid_turn`, `oauth_flow_failure`, `session_expired`,
    // `list_paginated`, `reconcile`): 396 + 20 = 416.
    //
    // Re-counted by hand against telemetry/src/tengu/mcp.rs::NAMES.len() (44)
    // and telemetry/src/tengu/plugin.rs::NAMES.len() (24), not pasted from a
    // failing assertion.
    // Current tool block has 129 names, six fewer than the historical
    // running total above. Recounted all 19 blocks; fixture also locks 410.
    assert_eq!(ALL_EVENT_NAMES.len(), 410);
}

#[test]
fn registry_entries_are_unique() {
    use std::collections::HashSet;
    let set: HashSet<&&str> = ALL_EVENT_NAMES.iter().collect();
    assert_eq!(
        set.len(),
        ALL_EVENT_NAMES.len(),
        "duplicate name in ALL_EVENT_NAMES"
    );
}

#[test]
fn registry_entries_all_use_tengu_or_release_prefix() {
    // All events use the `tengu_` prefix EXCEPT for release-marker events,
    // which use the engine-scoped `lingxi_core_` prefix (the release marker
    // is a one-shot init event, not a subsystem event).
    for n in ALL_EVENT_NAMES {
        assert!(
            n.starts_with("tengu_") || n.starts_with("lingxi_core_"),
            "{n} must start with `tengu_` or `lingxi_core_`"
        );
    }
}

#[test]
fn lingxi_core_v0_5_0_released_is_registered() {
    assert!(
        ALL_EVENT_NAMES.contains(&"lingxi_core_v0_5_0_released"),
        "v0.5.0 release-marker event must be present in ALL_EVENT_NAMES"
    );
}

#[test]
fn lingxi_core_v0_6_0_released_is_registered() {
    assert!(
        ALL_EVENT_NAMES.contains(&"lingxi_core_v0_6_0_released"),
        "v0.6.0 release-marker event must be present in ALL_EVENT_NAMES"
    );
}

#[test]
fn lingxi_core_v0_7_0_released_is_registered() {
    assert!(
        ALL_EVENT_NAMES.contains(&"lingxi_core_v0_7_0_released"),
        "v0.7.0 release-marker event must be present in ALL_EVENT_NAMES"
    );
}

#[test]
fn lingxi_core_v0_8_0_released_is_registered() {
    assert!(
        ALL_EVENT_NAMES.contains(&"lingxi_core_v0_8_0_released"),
        "v0.8.0 release-marker event must be present in ALL_EVENT_NAMES"
    );
}

#[test]
fn m7_16_screen_opened_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_screen_opened"));
}

#[test]
fn m7_16_screen_closed_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_screen_closed"));
}

#[test]
fn m7_16_search_opened_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_search_opened"));
}

/// The three §2.7 candidates that stayed deferred (no clean/aggregated emit
/// site) MUST NOT be registered — registering a name with no call site mints a
/// dead name (the M6 "330-vs-326" lesson).
#[test]
fn m7_16_deferred_candidates_not_registered() {
    assert!(!ALL_EVENT_NAMES.contains(&"tengu_tui_command_palette_opened"));
    assert!(!ALL_EVENT_NAMES.contains(&"tengu_tui_vim_mode_entered"));
    assert!(!ALL_EVENT_NAMES.contains(&"tengu_tui_key_pressed"));
}

#[test]
fn release_marker_constant_matches() {
    assert_eq!(
        telemetry::tengu::release::LINGXI_CORE_V0_5_0_RELEASED,
        "lingxi_core_v0_5_0_released"
    );
    assert_eq!(
        telemetry::tengu::release::LINGXI_CORE_V0_6_0_RELEASED,
        "lingxi_core_v0_6_0_released"
    );
    assert_eq!(
        telemetry::tengu::release::LINGXI_CORE_V0_7_0_RELEASED,
        "lingxi_core_v0_7_0_released"
    );
    assert_eq!(
        telemetry::tengu::release::LINGXI_CORE_V0_8_0_RELEASED,
        "lingxi_core_v0_8_0_released"
    );
}

#[test]
fn category_ordering_preserved() {
    // First 25 are api_; next 30 are agent_; etc.
    for n in &ALL_EVENT_NAMES[0..25] {
        assert!(n.starts_with("tengu_api_"), "api block: {n}");
    }
    for n in &ALL_EVENT_NAMES[25..55] {
        assert!(n.starts_with("tengu_agent_"), "agent block: {n}");
    }
    // M5-07 grew the session block by +3 (appended/rotated/corrupted): 15 -> 18.
    // M5-08 grew it by +2 (resume_started/completed): 18 -> 20.
    // D3 strict-parity consolidated session-resume telemetry: 20 -> 18.
    for n in &ALL_EVENT_NAMES[55..74] {
        assert!(n.starts_with("tengu_session_"), "session block: {n}");
    }
    // M4-05 grew the tool block by +24 (67 → 91); M4-06 grew it by +6 (91 → 97);
    // M4-07 grew it by +13 (97 → 110); M4-08 grew it by +24 (110 → 134),
    // shifting downstream offsets by +24.
    // M5-07 shifts all post-session offsets by +3.
    // M5-08 shifts all post-session offsets by another +2.
    // CronDelete/CronList grow the tool block by +6 (128 → 134), shifting all
    // downstream offsets by +6.
    // Grep/Glob telemetry removed (claude-code emits none): the 6 fabricated
    // tengu_tool_grep_* / tengu_tool_glob_* names are dropped, so the tool
    // block is 134 (not 140) and every downstream offset shifts back by -6.
    // D1 strict-parity dropped tengu_tool_todo_write_* (3): tool block 134 -> 131.
    // Worktree 2.1.206 parity added 2 byte-exact events (tengu_worktree_created,
    // tengu_worktree_entered_existing) to the tool block: 131 -> 133, shifting
    // every downstream offset by +2.
    // ExitWorktree 2.1.206 parity added 2 more byte-exact events
    // (tengu_worktree_kept, tengu_worktree_removed) to the tool block:
    // 133 -> 135, shifting every downstream offset by another +2.
    for n in &ALL_EVENT_NAMES[74..203] {
        assert!(
            n.starts_with("tengu_tool_") || n.starts_with("tengu_worktree_"),
            "tool block: {n}"
        );
    }
    // D2 strict-parity dropped tengu_cost_recorded: cost block 10 -> 9.
    for n in &ALL_EVENT_NAMES[203..212] {
        assert!(n.starts_with("tengu_cost_"), "cost block: {n}");
    }
    for n in &ALL_EVENT_NAMES[212..220] {
        assert!(n.starts_with("tengu_oauth_"), "oauth block: {n}");
    }
    for n in &ALL_EVENT_NAMES[220..232] {
        assert!(n.starts_with("tengu_memory_"), "memory block: {n}");
    }
    for n in &ALL_EVENT_NAMES[232..235] {
        assert!(n.starts_with("tengu_settings_"), "settings block: {n}");
    }
    // M5-02 grew the orchestrator block by +3 (conversation lifecycle).
    // M5-04 grew it by +2 (streaming). M5-05 grew it by +2 (permission).
    // M5-06 grew it by +8 (hook pre/post + http_skipped_ssrf + timeout).
    // M5-13 grew it by +2 (REPL session started/ended).
    // Block size is now 17; release marker still trails. Walk order is
    // fixed by tengu::mod.rs's concat_all (settings → orchestrator →
    // release).
    for n in &ALL_EVENT_NAMES[235..252] {
        assert!(
            n.starts_with("tengu_orchestrator_") || n.starts_with("tengu_repl_"),
            "orchestrator block: {n}"
        );
    }
    // M5-14 grew the release block from 1 to 2 (+lingxi_core_v0_6_0_released).
    // M6-09 grew it from 2 to 3 (+lingxi_core_v0_7_0_released).
    // M7-16 grew it from 3 to 4 (+lingxi_core_v0_8_0_released).
    // Grep/Glob telemetry removed shifts the start back by -6 (tool block
    // 140→134): 259..263.
    for n in &ALL_EVENT_NAMES[252..256] {
        assert!(n.starts_with("lingxi_core_"), "release block: {n}");
    }
    // M5-10/M5-11: command block (54 events: 18 batch-1 + 36 batch-2) follows
    // the release markers. Walk order (per tengu::mod.rs concat_all):
    // … → release → command. Grep/Glob telemetry removed shifts it back by -6
    // (tool block 140→134): 263..317.
    for n in &ALL_EVENT_NAMES[256..310] {
        assert!(n.starts_with("tengu_command_"), "command block: {n}");
    }
    // M6-01: tui block (4 events) trails command.
    // M6-03: tui block grows to 6 events (+streaming_render_{started,ended}).
    // M6-05: tui block grows to 8 events (+permission_dialog_{shown,resolved}).
    // M6-09: tui block grows to 10 events (+scroll_started/scroll_ended).
    // M7-16: tui block grows to 13 events
    //        (+screen_opened/screen_closed/search_opened). Block shifted by +2
    //        total vs M6-09 (release 2→3→4). Grep/Glob telemetry removed shifts
    //        it back by -6 (tool block 140→134): 317..330.
    for n in &ALL_EVENT_NAMES[310..323] {
        assert!(n.starts_with("tengu_tui_"), "tui block: {n}");
    }
    // FileReadTool analytics block (4 events, #13 added the 4th) appended at the
    // GLOBAL TAIL — these are `tengu_file_read_*` / `tengu_session_file_read`
    // (NOT `tengu_tool_*`), kept after the tui block so every per-block prefix
    // slice above stays valid.
    assert_eq!(
        &ALL_EVENT_NAMES[323..327],
        &[
            "tengu_file_read_dedup",
            "tengu_session_file_read",
            "tengu_file_read_limits_override",
            "tengu_file_read_reread",
        ],
        "FileReadTool analytics tail block",
    );
    // Config-migration block (9 events) appended after the FileRead block —
    // order matches TS runMigrations execution order (main.tsx:328-336).
    // Positions 328..337 (shifted -6 by the Grep/Glob telemetry removal, +2 by
    // the worktree-206-parity tool-block growth, +2 by the
    // exit-worktree-206-parity tool-block growth).
    assert_eq!(
        &ALL_EVENT_NAMES[327..336],
        &telemetry::tengu::migration::NAMES,
        "config-migration tail block",
    );
    // Permission-flow block (1 event) appended after the config-migration
    // block — bypass dialog accept (BypassPermissionsModeDialog.tsx).
    // Position 341..342.
    assert_eq!(
        &ALL_EVENT_NAMES[336..337],
        &telemetry::tengu::permission::NAMES,
        "permission-flow tail block",
    );
    // Coordinator swarm block (3 events) appended after the permission block —
    // tengu_team_created/_deleted/coordinator_mode_switched. Positions 336..339.
    assert_eq!(
        &ALL_EVENT_NAMES[337..340],
        &telemetry::tengu::coordinator::NAMES,
        "coordinator swarm tail block",
    );
    // AWS auth-refresh trust-gate block (2 events, 2.1.198 M2) appended after
    // the coordinator block — tengu_awsAuthRefresh_missing_trust /
    // tengu_awsCredentialExport_missing_trust. Positions 339..341.
    assert_eq!(
        &ALL_EVENT_NAMES[340..342],
        telemetry::tengu::oauth::AWS_AUTH_NAMES,
        "AWS auth-refresh trust-gate tail block",
    );
    // MCP analytics-event block (44 events, 2.1.252 byte-alignment
    // B8/§20a/§20b/§11 plus registry/catalog/connect, `mcp serve`, listen
    // recovery, provider-neutral OAuth/auth, and the non-deferred command /
    // elicitation / pending / large-result / reconcile family) appended after
    // the AWS auth-refresh block. Positions 342..386.
    assert_eq!(
        &ALL_EVENT_NAMES[342..386],
        telemetry::tengu::mcp::NAMES,
        "MCP analytics-event tail block",
    );
    // Plugin event block (24 events, 2.1.251 byte-alignment B8 plus CLI
    // entry-point and prune/state-file observability) appended after the MCP
    // block. Positions 386..410.
    assert_eq!(
        &ALL_EVENT_NAMES[386..410],
        telemetry::tengu::plugin::NAMES,
        "plugin event tail block",
    );
}

#[test]
fn mcp_events_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_server_config_invalid"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_server_connection_succeeded"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_server_connection_failed"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_tools_listed"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_degraded"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_discovery_source"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_list_changed"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_resource_templates_fetched"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_tool_auto_backgrounded"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_start"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_add"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_delete"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_get"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_list"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_login"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_logout"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_command_inline"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_elicitation_shown"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_elicitation_response"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_input_missing_required"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_large_result_handled"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_pending_call"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_servers"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_tool_result_ended_turn"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_tools_commands_loaded"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_tools_refreshed_mid_turn"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_oauth_flow_failure"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_session_expired"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_list_paginated"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_mcp_reconcile"));
}

#[test]
fn plugin_events_registered() {
    for n in telemetry::tengu::plugin::NAMES {
        assert!(
            ALL_EVENT_NAMES.contains(n),
            "{n} must be registered in ALL_EVENT_NAMES"
        );
    }
    assert_eq!(telemetry::tengu::plugin::NAMES.len(), 24);
}

#[test]
fn file_read_analytics_events_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_file_read_dedup"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_session_file_read"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_file_read_limits_override"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_file_read_reread"));
}

#[test]
fn m6_03_streaming_render_events_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_streaming_render_started"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_streaming_render_ended"));
}

#[test]
fn m6_05_permission_dialog_events_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_permission_dialog_shown"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_permission_dialog_resolved"));
}

#[test]
fn m6_09_scroll_events_registered() {
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_scroll_started"));
    assert!(ALL_EVENT_NAMES.contains(&"tengu_tui_scroll_ended"));
}
