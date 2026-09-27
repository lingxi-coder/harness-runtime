//! `tengu_*` event schemas — the single authoritative source.
//!
//! Spec §7 line 752-792. Eight sub-modules, one per category. Every payload
//! struct uses `#[serde(deny_unknown_fields)]`; every payload enum uses
//! `#[non_exhaustive]`; every user-derived string field uses [`Verified`]
//! or [`PiiTagged`] (not bare `String`).
//!
//! [`Verified`]: crate::pii::Verified
//! [`PiiTagged`]: crate::pii::PiiTagged

pub mod agent;
pub mod api;
pub mod command;
pub mod coordinator;
pub mod cost;
/// Fusion telemetry event names (NOT in `ALL_EVENT_NAMES` — separate from the
/// count-locked event set; kept here for string-lock testing only).
pub mod fusion;
/// `tengu_uncompilable_ignore_pattern` event name + its `site` values (NOT in
/// `ALL_EVENT_NAMES` — string-lock only, mirroring `workflow`).
pub mod ignore_pattern;
/// `/loop` (Kairos) autonomous-loop telemetry event names (NOT in
/// `ALL_EVENT_NAMES` — separate from the count-locked event set; kept here for
/// string-lock testing only, mirroring `workflow`).
pub mod kairos;
/// `tengu_mcp_*` analytics-event schemas (2.1.252 parity audit). The registry
/// now pins 44 of the oracle's 55 provider-neutral MCP analytics events; see
/// the module doc for the remaining 11 and for the still-separate Statsig
/// flag-name family that shares the `tengu_mcp_*` prefix.
pub mod mcp;
pub mod memory;
pub mod migration;
pub mod oauth;
pub mod orchestrator;
pub mod permission;
/// `tengu_plugin_*` event schemas (2.1.251 §20c — the port's telemetry
/// catalogue had no plugin module at all before this).
pub mod plugin;
/// Queue-operation telemetry event names (NOT in `ALL_EVENT_NAMES` — these are
/// LingXi-native `lingxi_queue_*` observability events, kept apart from the
/// count-locked `tengu_*` set; mirrors `workflow`).
pub mod queue;
pub mod release;
pub mod session;
pub mod settings;
pub mod tool;
pub mod tui;
/// Workflow telemetry event names (NOT in `ALL_EVENT_NAMES` — separate from
/// the count-locked event set; kept here for string-lock testing only).
pub mod workflow;

/// Flat list of every `tengu_*` event name in registration order:
/// api → agent → session → tool → cost → oauth → memory → settings →
/// orchestrator → release.
///
/// Tasks 2-9 each append their category in registration order. The list is
/// re-exported at crate root as `telemetry::ALL_EVENT_NAMES` (no — kept
/// inside the `tengu` module so the path `telemetry::tengu::ALL_EVENT_NAMES`
/// remains the single source of truth; see `parity_tengu_events.rs`).
pub const ALL_EVENT_NAMES: &[&str] = {
    // post-M5-06: orchestrator block grew from 7 to 15 (+ 6 hook lifecycle
    // events + hook_http_skipped_ssrf + hook_timeout).
    // M5-07: session block grew 15 -> 18 (+3 session_appended/rotated/corrupted).
    // M5-08: session block grows 18 -> 20 (+2 session_resume_started/completed).
    // M5-10: + 18 command events (6 commands × 3 phases) -> 276 total.
    // M5-11: command block grows 18 -> 54 (+36 new events for 12 batch-2 commands)
    //        -> 312 total.
    // M5-13: orchestrator block grows 15 -> 17 (+2 REPL session started/ended)
    //        -> 314 total. (Note: baseline observed at M6-01 start was 315
    //        entries; the comments above understate by 1 — see audit log
    //        in the M6-01 plan execution.)
    // M6-01: TUI lifecycle events (+4 -> 319).
    // M6-03: +2 streaming render events (streaming_render_{started,ended})
    //        → tui block grows 4 → 6 → 321 total.
    // M6-05: +2 permission dialog events (permission_dialog_{shown,resolved})
    //        → tui block grows 6 → 8 → 323 total.
    // M6-09: release block grows 2 → 3 (+lingxi_core_v0_7_0_released);
    //        tui block grows 8 → 10 (+scroll_started/scroll_ended). The
    //        specced tengu_tui_key_pressed is deferred to M7 (no emit site),
    //        so M6-09 adds 3 (not the spec's stale §2.6 estimate of ~15/330).
    //        → 326 total.
    // M7-16: release block grows 3 → 4 (+lingxi_core_v0_8_0_released); tui
    //        block grows 10 → 13 (+screen_opened/screen_closed/search_opened —
    //        the §2.7 candidates with REAL emit sites). M7-01..M7-15 added 0
    //        events (every candidate was deferred to this audit). The deferred
    //        command_palette_opened / vim_mode_entered / key_pressed stay OUT
    //        (no clean/aggregated emit site → no dead names — the M6 lesson).
    //        → 330 total. Cumulative across releases:
    //          v0.4.0 (M3-06): 196 · v0.5.0 (M4-09): 238 ·
    //          v0.6.0 (M5-14): 315 · v0.7.0 (M6-09): 326 · v0.8.0 (M7-16): 330.
    // CronDelete/CronList: tool block grows 128 → 134 (+6, 2 tools × 3 stages)
    //        → 330 total. (Grep/Glob emit no telemetry, matching claude-code
    //        v2.1.183 — 6 fewer tengu_tool_* events than the old 140-name block.)
    // FileReadTool analytics: +4 appended at the GLOBAL TAIL (tengu_file_read_dedup,
    //        tengu_session_file_read, tengu_file_read_limits_override,
    //        tengu_file_read_reread [#13]) — NOT in the tool concat block (they are
    //        not tengu_tool_*) → 334 total. The `+ 4` below (between `13` and `9`)
    //        is `tool::FILE_READ_ANALYTICS_NAMES.len()`.
    // Config migrations: +9 (migration::NAMES, runMigrations port) → 343 total.
    // Bypass-permissions dialog: +1 (permission::NAMES) → 344 total.
    // Coordinator swarm events: +3 (coordinator::NAMES — tengu_team_created,
    //        tengu_team_deleted, tengu_coordinator_mode_switched) appended as
    //        their own GLOBAL-TAIL block after the permission block → 347 total.
    // Strict-parity removed events (D1 tengu_tool_todo_write_*, D2 port-only
    // tengu_cost_recorded — both absent in claude-code 2.1.195) shrank their
    // blocks. Rather than hand-maintain a brittle running sum (which drifted and
    // left empty `""` slots → duplicate/bad-prefix failures), derive TOTAL from
    // the actual per-block `NAMES.len()` so the array size ALWAYS equals the
    // number of names concat_all appends, in the SAME block order.
    // 2.1.251 byte-alignment B8 (telemetry-modules): two new GLOBAL-TAIL
    // blocks, appended after oauth::AWS_AUTH_NAMES.
    //   - mcp::NAMES (tengu_mcp_server_config_invalid,
    //     tengu_mcp_tools_listed, tengu_mcp_degraded,
    //     tengu_mcp_discovery_source, tengu_mcp_tool_auto_backgrounded).
    //     ⚠️ An earlier revision of this comment claimed these were "the only
    //     two confirmed real `tengu_mcp_*` ANALYTICS events at the oracle —
    //     everything else with that prefix is a Statsig feature-flag name".
    //     THAT CLAIM WAS FALSE and is retracted: scanning 2.1.251 for the
    //     analytics-bus call shape `s("tengu_mcp_<name>"` returns 55 distinct
    //     event names (tengu_mcp_server_connection_succeeded/_failed,
    //     _list_changed, _listen_reopen, _tripwire, _sdk_generation, the four
    //     _oauth_flow_* , _registry_fetch, _elicitation_shown/_response,
    //     _discovery_source, _first_party_auto_auth, …), several of them
    //     corroborated by the oracle's own event allowlist at @156122853.
    //     This block now ports 44 of 55; the other 11 are an OPEN gap. It IS true
    //     that many `tengu_mcp_*` STRINGS are Statsig flags, but that does
    //     not make the event set two. See mcp.rs's module doc.
    //     The auto-background event is emitted by the tool dispatcher but
    //     belongs here because its wire prefix is `tengu_mcp_`.
    //   - plugin::NAMES (+14 initially — tengu_plugin_enabled_for_session and
    //     its 13 siblings; the port had NO plugin telemetry module before
    //     this. The block has since grown to 24 names with CLI entry-point
    //     and prune/state-file observability.)
    //     ⚠️ SUBSTRATE ONLY: none of the 24 has a production emit site, so
    //     §20c is OPEN. See plugin.rs's module doc.
    //     350 + 24 = 374 after the later expansion.
    const TOTAL: usize = api::NAMES.len()
        + agent::NAMES.len()
        + session::NAMES.len()
        + tool::NAMES.len()
        + cost::NAMES.len()
        + oauth::NAMES.len()
        + memory::NAMES.len()
        + settings::NAMES.len()
        + orchestrator::NAMES.len()
        + release::NAMES.len()
        + command::NAMES.len()
        + tui::NAMES.len()
        + tool::FILE_READ_ANALYTICS_NAMES.len()
        + migration::NAMES.len()
        + permission::NAMES.len()
        + coordinator::NAMES.len()
        + oauth::AWS_AUTH_NAMES.len()
        + mcp::NAMES.len()
        + plugin::NAMES.len();
    const fn concat_all() -> [&'static str; TOTAL] {
        let mut out: [&'static str; TOTAL] = [""; TOTAL];
        let mut idx = 0;
        let mut i = 0;
        while i < api::NAMES.len() {
            out[idx] = api::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < agent::NAMES.len() {
            out[idx] = agent::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < session::NAMES.len() {
            out[idx] = session::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < tool::NAMES.len() {
            out[idx] = tool::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < cost::NAMES.len() {
            out[idx] = cost::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < oauth::NAMES.len() {
            out[idx] = oauth::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < memory::NAMES.len() {
            out[idx] = memory::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < settings::NAMES.len() {
            out[idx] = settings::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < orchestrator::NAMES.len() {
            out[idx] = orchestrator::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < release::NAMES.len() {
            out[idx] = release::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < command::NAMES.len() {
            out[idx] = command::NAMES[i];
            idx += 1;
            i += 1;
        }
        let mut i = 0;
        while i < tui::NAMES.len() {
            out[idx] = tui::NAMES[i];
            idx += 1;
            i += 1;
        }
        // FileReadTool analytics names (`tengu_file_read_*` / `tengu_session_file_read`)
        // are appended at the GLOBAL TAIL — they are not `tengu_tool_*`, so keeping
        // them out of the tool concat block preserves every per-block prefix slice
        // in event_name_completeness_test::category_ordering_preserved. Positions
        // 330/331/332/333 (tengu_events.json fixture).
        let mut i = 0;
        while i < tool::FILE_READ_ANALYTICS_NAMES.len() {
            out[idx] = tool::FILE_READ_ANALYTICS_NAMES[i];
            idx += 1;
            i += 1;
        }
        // Config-migration block (tengu_migrate_* / model-migration markers) —
        // appended after the FileRead global tail. Positions 334..343.
        let mut i = 0;
        while i < migration::NAMES.len() {
            out[idx] = migration::NAMES[i];
            idx += 1;
            i += 1;
        }
        // Permission-flow block (bypass dialog accept) — appended after the
        // config-migration block. Position 343.
        let mut i = 0;
        while i < permission::NAMES.len() {
            out[idx] = permission::NAMES[i];
            idx += 1;
            i += 1;
        }
        // Coordinator swarm block (tengu_team_created/_deleted/
        // coordinator_mode_switched) — appended after the permission block.
        // Positions 344..347.
        let mut i = 0;
        while i < coordinator::NAMES.len() {
            out[idx] = coordinator::NAMES[i];
            idx += 1;
            i += 1;
        }
        // AWS auth-refresh trust-gate block (2.1.198:
        // tengu_awsAuthRefresh_missing_trust /
        // tengu_awsCredentialExport_missing_trust) — appended after the
        // coordinator block. Positions 341..343.
        let mut i = 0;
        while i < oauth::AWS_AUTH_NAMES.len() {
            out[idx] = oauth::AWS_AUTH_NAMES[i];
            idx += 1;
            i += 1;
        }
        // 2.1.251 byte-alignment B8: MCP analytics-event block (2 events —
        // tengu_mcp_server_config_invalid, tengu_mcp_tools_listed) appended
        // after the AWS auth-refresh block.
        let mut i = 0;
        while i < mcp::NAMES.len() {
            out[idx] = mcp::NAMES[i];
            idx += 1;
            i += 1;
        }
        // 2.1.251 byte-alignment B8: plugin event block (now 24 events —
        // tengu_plugin_enabled_for_session, its original siblings, plus the
        // later CLI/prune/state-file additions) appended after the MCP block.
        let mut i = 0;
        while i < plugin::NAMES.len() {
            out[idx] = plugin::NAMES[i];
            idx += 1;
            i += 1;
        }
        out
    }
    &concat_all()
};

// Audit-macro invocation: Task 12 replaces the stub body with the real walker.
telemetry_macros::tengu_event_audit!();
