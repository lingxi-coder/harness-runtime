//! Parity baseline vs Claude Code 2.1.198 (M0 of
//! `docs/superpowers/specs/2026-07-02-cc2.1.198-alignment-plan.md`).
//!
//! Two jobs:
//! 1. Pin the real-binary CLI surface as golden fixtures (captured from the
//!    local `claude` 2.1.198 binary on 2026-07-02).
//! 2. Hold a structured checklist of every 2.1.196–2.1.198 changelog entry.
//!    Each entry must carry an explicit disposition — `Implemented`,
//!    `Mission("M<n>")`, or `Divergence(reason)`. The drift test fails on any
//!    `Unknown`, so a new checklist entry can never be silently ignored.
//!
//! As missions land they flip their entries to `Implemented`; M14 asserts no
//! `Mission` dispositions remain.

const ROOT_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_198_root_help.txt");
const AGENTS_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_198_agents_help.txt");
const MCP_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_198_mcp_help.txt");
const GATEWAY_HELP: &str = include_str!("../src/parity/fixtures/cc_2_1_198_gateway_help.txt");

/// Where a changelog entry stands in LingXi.
// `Implemented` and `Unknown` are unconstructed until missions land / drift
// appears — both are part of the checklist contract, not dead code.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
enum Disposition {
    /// Behavior verified present in lingxi-code.
    Implemented,
    /// Tracked by a mission in the alignment plan; not yet landed.
    Mission(&'static str),
    /// Deliberately not ported; reason recorded.
    Divergence(&'static str),
    /// Not yet triaged — the drift test rejects these.
    Unknown,
}

struct Entry {
    version: &'static str,
    item: &'static str,
    disposition: Disposition,
}

use Disposition::{Divergence, Mission};

/// Every entry of the official changelog for 2.1.196–2.1.198
/// (snapshot in the alignment plan doc, fetched 2026-07-02).
const CHECKLIST: &[Entry] = &[
    // ── 2.1.198 ────────────────────────────────────────────────────────────
    Entry { version: "2.1.198", item: "Claude in Chrome generally available", disposition: Divergence("browser extension + Anthropic cloud service; not portable") },
    // M8 landed: `cli::agents_notify` ports the binary's FleetView pipeline —
    // `$1f` band diff (@222691698: blocked→`agent_needs_input` with `needs`
    // clamped at 120 UTF-16 units, completed→`agent_completed` "finished"/
    // "failed", stopped/self-driving silent, `i2 = "send a prompt to start"`
    // fresh-session sentinel keeps the prev band), `Fhe` band derivation and
    // the `Pon` label — all unit-locked. The agents view (commands/agents.rs
    // `NotificationWatcher`) loads settings hooks standalone and fires
    // `HookEvent::Notification { message, kind }` (= binary `TQ` @219455460
    // payload `{message, title?, notification_type}`, matcher on
    // `notification_type`) on a 1s registry poll; the interactive session
    // writes its own `sessions/<pid>.json` status transitions
    // (`SessionRegistration::update_status` = binary `mvn`: busy/idle/waiting
    // + `waitingFor: "permission prompt"`, statusUpdatedAt on change).
    Entry { version: "2.1.198", item: "Notification hook agent_needs_input/agent_completed for claude agents sessions", disposition: Disposition::Implemented },
    // M13 landed: `/dataviz` is registered as an unconditional, user-invocable
    // bundled prompt with the oracle's menu label and optional user-request
    // suffix; the provider-neutral body and reference assets are embedded in
    // command-core.
    Entry { version: "2.1.198", item: "/dataviz bundled skill", disposition: Disposition::Implemented },
    Entry { version: "2.1.198", item: "Gateway: anthropicAws upstream provider", disposition: Divergence("no enterprise gateway runtime in lingxi; client-side model fallback (tengu_model_fallback_triggered) already ported") },
    Entry { version: "2.1.198", item: "Gateway: model-not-found advances failover chain", disposition: Divergence("no enterprise gateway runtime in lingxi; client-side model fallback (tengu_model_fallback_triggered) already ported") },
    // M8 landed: the binary implements this PROMPT-DRIVEN — `_ff()`
    // (@219583413) appends a `# Background Session` system-prompt section to
    // bg jobs whose shipping paragraph directs the model to commit, push and
    // `gh pr create --draft` on worktree completion (no programmatic git
    // flow exists in the binary; the no-remote case is likewise instruction:
    // "Skip the PR only if … there's no remote to push to (then commit and
    // say where the work is)"). Ported byte-faithfully (lingxi env names +
    // `.lingxi/worktrees/`) as `orchestrator::prompt::bg_session`, gated on
    // `LINGXI_SESSION_KIND=bg` + `LINGXI_JOB_DIR`, spliced after
    // output_style before `# Context management` (binary `cx()` order);
    // byte-locked incl. the isolation-none no-shipping branch. The live
    // `--bg` dispatcher now sets those envs and retains the prompt through the
    // detached daemon launch; no separate programmatic git flow is implied.
    Entry { version: "2.1.198", item: "Background agents auto commit/push/draft PR on worktree completion", disposition: Disposition::Implemented },
    // M10 landed: 1:1 port of `GAe`/`obm`/`dPn` (binary: `qme` Explore def is
    // `model:"inherit"`; `GAe` returns `obm(sessionModel) ? "opus" : "inherit"`
    // for built-in Explore only; `obm` = firstParty && session model names none
    // of Kyl=["haiku","sonnet","opus"]). Ported as
    // `agent::model_resolution::resolve_builtin_explore_model`, applied in
    // `PoolSubagentSpawner::{resolve_definition,resolve_selection}`; the
    // built-in Explore def's model flipped Alias("haiku")→Inherit to match
    // qme. LingXi multi-provider (accepted divergence): a session default
    // model routed to a NON-Anthropic provider profile behaves like the TS
    // non-firstParty branch (inherit, never the opus cap) via
    // `with_session_provider_first_party(false)` wired at engine-desktop boot.
    // Locked by agent::model_resolution (explore_on_* table) +
    // agent::handle (resolve_definition_explore_*) tests.
    Entry { version: "2.1.198", item: "Explore agent inherits session model capped at opus", disposition: Disposition::Implemented },
    // M10 landed, both halves. SUBAGENTS: inherit BY CONSTRUCTION — the
    // subagent seam (`ProviderApiAdapter: agent::SubagentApiClient`) delegates
    // to the SAME `ApiService` whose `build_request` applies the session
    // `self.thinking` to every request (binary: child options carry
    // `thinkingConfig: sDi(n.options.thinkingConfig,…)` @215628753); locked by
    // llm_runtime service_test::subagent_entry_point_inherits_session_thinking_
    // config. COMPACTION: was a real gap — the fork-summarizer path
    // (`ForkedAgentRunner`→`ProviderSideQueryClient`) DROPPED thinking. Now the
    // session `ThinkingConfig` threads `with_session_thinking` (engine-desktop
    // compaction runner) → `SideQueryRequest.thinking` → the SAME
    // `model::thinking::reasoning_for_request` rules as the main loop (binary:
    // summarizer passes `thinkingConfig: mXt(r)` = session options.thinkingConfig
    // @216945141/@216926189; other sEt callers stay explicitly disabled →
    // utility side queries keep `thinking: None`). Locked by
    // sidequery forked_agent::forked_call_carries_the_session_thinking_config +
    // provider_side_query::{session_thinking_config_rides_on_the_wire,
    // no_thinking_config_keeps_legacy_wire}.
    Entry { version: "2.1.198", item: "Subagents + compaction inherit extended thinking config", disposition: Disposition::Implemented },
    // M12 landed: the orchestrator streaming turn loop re-opens + re-pumps the
    // streaming request on a transient network drop (ECONNRESET / connection
    // closed / reset → `LlmError::Transport`, or a watchdog idle-timeout) with
    // exponential+jitter backoff (binary `sle`), gated on `!real_content_started`
    // (binary `!Hr`): because a `tool_use` block STARTING flips that flag, the
    // guard also guarantees a non-idempotent tool is never re-run. Cause-aware
    // caps (stale-connection `An=2`, idle-timeout `ao=1`,
    // orchestrator/src/streaming_loop.rs). `pump_stream_with_executor_tracked`
    // returns the `PumpFailure { error, real_content_started }`; locked by
    // orchestrator streaming_transient_retry_test (thinking-only reset retries
    // & succeeds; a started tool forbids the retry; non-transient never retries).
    Entry { version: "2.1.198", item: "Mid-response transient network errors retry with backoff (ECONNRESET etc.)", disposition: Disposition::Implemented },
    // M11 landed: the oracle's `leafCerts`/`secureContexts` caches are
    // get→build→set maps keyed by host. LingXi mirrors those maps with
    // short-held Mutex-protected get/build/insert caches in the MITM CA; a
    // first-miss thread race can mint an improvement-only duplicate, but the
    // oracle's own map has the same get/build/set race and no stronger
    // coalescing contract. Desktop network asks are also routed through the
    // live session PermissionGate.
    Entry { version: "2.1.198", item: "Sandbox classifier: dedupe repeated same-host requests", disposition: Disposition::Implemented },
    // M8 landed: REAL lingxi bug found + fixed — `register_self_contained_
    // handlers` registered `LocalBashHandler` with its default
    // `NoopStatusSink`, so a finished background bash task's registry status
    // stayed `Running` forever (the stuck panel). Now a deferred
    // `RegistryStatusSink` (bash terminal status + new `set_exit_code`
    // write-through) is threaded at registration and bound at engine-desktop
    // (5.46f); locked by tasks::registry_test::
    // `finished_background_bash_task_does_not_stay_running`. Resume half:
    // lingxi's task registry is in-process with no disk persistence — a
    // resumed session builds a FRESH registry, so a stale Running row cannot
    // survive resume by construction.
    Entry { version: "2.1.198", item: "Task panels: no stuck Running after finish/resume", disposition: Disposition::Implemented },
    // M9 landed, both halves. FAILED-TO-LEAD: the binary's in-process runner
    // catch sends a failed idle notification to the leader
    // (`{idleReason:"failed", completedStatus:"failed", failureReason}`
    // @216293689; `failureReason` capped at 200 by `zTt`/`RXn=200`
    // @215144889/@215149471). lingxi already flipped the lead-visible status
    // (teammate worker Failed → CoordinatorStatusSink → WorkerStatus::Failed);
    // now the REAL error reason rides along via the new defaulted
    // `TaskStatusSink::set_failed` seam (worker passes
    // `SubagentEvent::Failed.error`; CoordinatorStatusSink overrides with the
    // 200-char cap). Locked by tasks::…::failed_turn_set_reports_error_reason_
    // through_set_failed + coordinator::status_sink::set_failed_reports_reason_
    // to_lead_capped_at_200. WAKE-ON-MESSAGE: binary SendMessage emits the
    // recipient task's `retryWake` after the mailbox write (`TDo` @215134403:
    // `r.retryWake?.emit()`; called @216646768/@216647191), threaded as
    // `subscribeRetryWake: V.subscribe` into the API-retry loop (@216289770)
    // so a teammate stuck in retry backoff re-issues NOW with the queued
    // message. lingxi analog: a `UserMessage` racing the in-flight round-trip
    // (llm-runtime retries live inside that future) now drops it, appends the
    // message to history, and re-issues immediately — previously the text was
    // silently DISCARDED. Locked by agent::runner_test::persist_mode_message_
    // wakes_stuck_round_trip_and_carries_the_text.
    Entry { version: "2.1.198", item: "Teammate API-error reports failed to lead; stuck teammate wake-retries on message", disposition: Disposition::Implemented },
    // M13 landed: `/diff` opens a scrollable list/detail-style overlay backed
    // by `git --no-optional-locks diff HEAD`. Its cached snapshot refreshes on
    // the owner tick after external branch, commit, or edit changes, with a
    // debounce so render/height paths never spawn git processes.
    Entry { version: "2.1.198", item: "/diff panel refreshes on external branch switch/commit", disposition: Disposition::Implemented },
    // M6 landed: tui_core::render::markdown_table vertical-format clamp
    // (long labels hard-broken, over-long words hard-wrapped, all lines ≤
    // frame − SAFETY_MARGIN); locked by overflow tests in markdown_table.rs +
    // tui-rata message.rs `wide_markdown_table_never_overflows_narrow_frame`.
    Entry { version: "2.1.198", item: "Markdown tables no longer overflow right border in fullscreen", disposition: Disposition::Implemented },
    // M2 landed: llm_runtime::aws_auth (ZBd/gIn/t2d port: trust gate + STS
    // probe + QBd=30s cooldown + 3-min timeout) + the V_c/G_c/s_f drive-loop
    // trigger (Ygf=2) in ApiService; settings keys awsAuthRefresh /
    // awsCredentialExport / gcpAuthRefresh in engine SettingsJson.
    Entry { version: "2.1.198", item: "awsAuthRefresh runs automatically on STS expiry (anthropicAws/Mantle)", disposition: Disposition::Implemented },
    Entry { version: "2.1.198", item: "macOS Local Network entitlements for background agent sessions", disposition: Divergence("macOS app packaging/entitlements, not core runtime") },
    // M13 N/A-with-evidence: `/desktop` ("Continue the current session in
    // Claude Desktop") is a pass-1 UnimplementedCommandHandler stub in lingxi
    // (command-api builtin_support names.rs; only the `app` alias is wired) —
    // there is no Claude Desktop handoff whose cwd could go stale after a
    // worktree exit.
    Entry { version: "2.1.198", item: "/desktop cwd after entering+exiting a worktree", disposition: Divergence("no Claude Desktop handoff in lingxi: /desktop is an unimplemented stub, so there is no cwd to fix") },
    // M7 N/A-with-evidence: the cc fix was in the remote-session WS transport
    // (`useRemoteSession`/`updateReconnectingStatus`, binary strings
    // @206609/221616 regions). lingxi's agents view
    // (apps/cli/commands/agents.rs + tui-rata agents_screen) reads the LOCAL
    // sessions/jobs registry and attaches by respawning `lingxi-cli --resume`
    // — no socket, no timed reconnect loop. The only "Reconnecting" strings
    // in lingxi live in the MCP client (mcp/src/{registry,connection}.rs), a
    // different subsystem.
    Entry { version: "2.1.198", item: "Agents view: no Reconnecting spam every ~52s", disposition: Divergence("no remote-session transport in lingxi's agents view; the ~52s reconnect log loop cannot exist (attach = local respawn, registry = local files)") },
    // M3 landed: `Argv::validate_background_args` rejects --bg/--background ×
    // --print/-p up front in `run_cli` (byte-locked `pof` message @218854391,
    // stderr + exit 1); e2e-locked in apps/cli/tests/cli_argv_errors.rs.
    Entry { version: "2.1.198", item: "--bg + --print/-p rejected up front", disposition: Disposition::Implemented },
    // M9 verified: the cc bug's surface (a bounded workflowProgress row list
    // whose overflow trim dropped the earliest workflow_agent rows) does not
    // exist in lingxi — progress rows stream unbounded (RunOutcome.progress
    // Vec + task-output spool), so earliest agents are kept by construction.
    // The binary fix (`updateWorkflowProgressBatch`/`GCo` @213640399) keys
    // agent/phase rows on `${type}:${index}` (updated in place, never
    // dropped) and trims ONLY `workflow_log` rows from the front when the
    // list exceeds `xVa*2` (xVa=500 @213645694) — agent rows and the phase
    // counter survive. Locked by tasks::…::local_workflow_test::
    // workflow_progress_keeps_earliest_agents_through_log_flood (>1000-line
    // log flood; earliest agent + phase rows retained, indices correct).
    Entry { version: "2.1.198", item: "Workflow progress view keeps earliest agents", disposition: Disposition::Implemented },
    // M11: conditional-rule glob matching gains a realpath symlink fallback,
    // 1:1 with the binary filter `pqt` (claudemd.ts): after the lexical
    // `relative(base, touched)`, if `touched` is absolute AND the lexical
    // relative failed (empty / `..`-escape / absolute), it resolves
    // `realpathSync(dirname(touched))` (`jd`) and, only when a symlink was
    // resolved (`c!==l`), recomputes the relative path from the canonical dir —
    // so a file reached through a symlinked path that resolves back under the
    // rule base still matches. Ported in orchestrator conditional_rules::
    // relative_path_for_match (only the touched dir is realpath'd; base is
    // assumed canonical, as production's realpath'd getOriginalCwd is). Locked by
    // `symlinked_touched_file_matches_via_realpath_fallback` (matches) +
    // `symlink_resolving_outside_base_still_does_not_match` (does not over-match).
    Entry { version: "2.1.198", item: ".claude/rules conditional rules load via symlinked paths (realpath)", disposition: Disposition::Implemented },
    // M6 landed: tui_core::render::osc8 ports the binary's OSC 8 emitters
    // (`Bpl` hyperlink bytes, `jx()` support gate, URL wrapping incl. scheme)
    // with byte-locked tests. Native scrollback and the ratatui
    // alternate-screen/fullscreen renderer emit gated URL links, including
    // whole-URL targets for Warp Cmd-click/double-click behavior.
    Entry { version: "2.1.198", item: "Cmd+click opens URLs in fullscreen in Warp; double-click selects whole URL", disposition: Disposition::Implemented },
    // M11: session-start (boot) plan mode auto-allows read-only tools. The
    // CLI `--permission-mode plan` (resolve_permission_mode) threads through
    // DesktopConfig into `PermissionPolicy::from_rules(Plan, rules)`
    // (engine-desktop lib.rs:3435). At the first tool call `session.plan_mode`
    // is still false (only the runtime EnterPlanMode tool sets it), so the
    // orchestrator uses the normal `check` → `authorize` under the boot mode
    // Plan → the mutation backstop: `hmr`-plan-safe read-only tools fall
    // through to the read-only auto-allow (no prompt), mutating tools trip the
    // backstop → Ask → prompt. The binary session-start branch
    // `if(permissionMode==="plan"){i=_nn(n);...}` is a no-op here (`_nn`/`Smr`
    // is the auto/transcript-classifier gate, OFF in external builds).
    // Locked by permission policy_gate_test `plan_mode_read_only_tool_auto_allows`
    // (boot Plan + Read → Allow, 0 prompts) and `plan_mode_mutating_tool_
    // delegates_to_inner` (boot Plan + Edit → prompt), plus policy_test
    // `plan_mode_does_not_backstop_plan_safe_read` / `plan_mode_asks_on_mutating_tool`.
    Entry { version: "2.1.198", item: "Plan mode auto-allows read-only tools when session starts in plan mode", disposition: Disposition::Implemented },
    // M12 landed: `session::jsonl::title::derive_fork_name` is a 1:1 port of the
    // binary `I2l`/`deriveFirstPrompt` (@217273303) — it reuses the existing
    // `first_meaningful_user_text` (`n9e`) extractor, which SKIPS `isMeta` and
    // `isCompactSummary` user messages, so a session whose history begins with a
    // compaction summary is named from the first REAL prompt; then the
    // `/branch`-specific `.replace(/\s+/g," ").trim().slice(0,100).trimEnd() ||
    // "Branched conversation"` tail (100-cap + fork fallback, vs the title
    // path's 200-cap + "(session)"). Locked by session fork_name_test. The
    // interactive `/branch` fork+resume flow now mounts the new transcript
    // through the CLI/TUI switch path and uses this derived title.
    Entry { version: "2.1.198", item: "/branch default fork name from first real prompt, not compaction summary", disposition: Disposition::Implemented },
    // M10 partial: the `/focus` command is now advertised, but its handler
    // only toggles the fullscreen renderer. LingXi still has no focus-specific
    // transcript/activity filtering, focus-mode prompt section, or folded
    // completion notification surface from the oracle.
    Entry { version: "2.1.198", item: "Focus mode: subagents in activity summary; completed notifications fold to one count", disposition: Divergence("LingXi advertises /focus but only toggles fullscreen; focus-specific transcript/activity filtering and folded completion notifications remain unimplemented") },
    Entry { version: "2.1.198", item: "Syntax highlighting upgraded to highlight.js 11", disposition: Divergence("lingxi renders via syntect; visual-equivalence accepted, highlight.js is a JS-runtime dependency") },
    // M6 landed: tui_core::key_hint ports the binary's `Pct()` probe (local
    // macOS, or LC_TERMINAL=iTerm2 / TERM_PROGRAM=Apple_Terminal|iTerm.app
    // forwarded over SSH) + the `nop` modifier table (Opt/Alt, Cmd/Super);
    // wired into the tui-rata footer and locked by key_hint + app.rs tests.
    Entry { version: "2.1.198", item: "opt/cmd hints instead of alt/super for Mac over SSH", disposition: Disposition::Implemented },
    // M12 landed (logic + render): `tui_core::retry_ux` ports the binary's
    // retry-status gating (`pHo`/spinner `Te` @214952343/@214957169): the
    // concrete error reason is hidden behind a generic "API error" until
    // `attempt >= min(3, max_retries)` (after the 2nd attempt), and an
    // overloaded error (`status==529` or text contains "overload") at that point
    // shows the status-page link `https://status.claude.com` (binary `zha`).
    // Wired into the tui-rata `SystemApiError` scrollback renderer; locked by
    // tui_core::retry_ux tests + tui-rata message render tests. DIVERGENCE (grep
    // evidence): the LIVE per-attempt retry-status EVENT surface from llm-runtime
    // (binary `onRetryStatus`) is not wired — retries are internal to
    // `llm_runtime::ApiService`'s drive loop and the only `SystemApiError`
    // producer is a demo fixture (tui/src/state.rs:1770); the tui-rata spinner
    // has no "tip" surface to replace, so the status-page link renders in the
    // scrollback api-error line rather than the spinner tip.
    Entry { version: "2.1.198", item: "Retry UX: error reason after 2nd attempt; status page link when overloaded", disposition: Disposition::Implemented },
    // M7 seam: the standalone agents view (tui-rata `agents_screen`) has no
    // mountable sign-in dialog yet — the OAuth `/connect`/login flow lives in
    // the full TUI runtime. Seam = a `/login` key route in
    // `AgentsScreenState::on_key` once an auth dialog is mountable from the
    // thin view loop.
    Entry { version: "2.1.198", item: "/login opens sign-in dialog from claude agents view", disposition: Divergence("LingXi's agents/tasks views do not expose the oracle's in-view sign-in dialog; login remains a separate command/auth flow") },
    // M10 verify+lock (no code change needed): LingXi structurally separates
    // the two channels — permission approval reaches a pending prompt only
    // through the permission gate below the `ToolInvoker` seam (keyed
    // `can_use_tool`/dialog), while a launcher/lead message arrives as
    // `lingxi_core::Event::UserMessage` on the runner's event channel, where the
    // M9 wake arm appends it to history as a plain user message (task
    // direction). A message delivered while a permission prompt is pending
    // cannot resolve the prompt or re-run the tool. Locked by
    // agent::runner_test::launcher_message_is_direction_not_approval_of_
    // pending_permission.
    Entry { version: "2.1.198", item: "Launcher-agent messages are task direction, never user approval", disposition: Disposition::Implemented },
    // M4 landed: `/agents` now returns the binary's removed-wizard guidance
    // (`Otf` text, `.lingxi`-branded paths) with the verbatim `(removed) …`
    // description (`commands/core/src/agents.rs`, `core_description`, /help
    // golden re-locked). M4 also wired --agents/--agent/--plugin-dir/
    // --from-pr/--prompt-suggestions/--effort + the `gateway` subcommand
    // surface and locked `ultrareview`'s unsupported exit.
    Entry { version: "2.1.198", item: "Removed /agents wizard", disposition: Disposition::Implemented },
    // ── 2.1.197 ────────────────────────────────────────────────────────────
    // M1+M1b landed. DEFAULT: sonnet alias → claude-sonnet-5 (registry
    // `aliases.sonnet.default` @207774{6xx}; lingxi: agent/skill sonnet-family
    // default flip + engine-desktop boot default, M1 3a730b442). NATIVE 1M:
    // registry `context:{window:1e6,native_1m:!0,native_1m_3p:{bedrock,vertex,
    // foundry}}`; ported as llm-runtime `model_native_1m` (binary `Hx`
    // @208698511), locked by context_window sonnet_5_is_natively_1m_and_64k_
    // output + opus_4_7_opus_4_8_fable_5_are_natively_1m (M1b extends native
    // 1M to opus-4-7/opus-4-8/fable-5/mythos-5 per the same registry blob).
    // PRICING — binary-probed: the client cost table (`S3e` @207923~,
    // keyed via `wa.sonnet5`) maps claude-sonnet-5 to the STANDARD sonnet
    // rate object `mne = {inputTokens:3, outputTokens:15,
    // promptCacheWriteTokens:3.75, promptCacheWrite1hTokens:6,
    // promptCacheReadTokens:0.3}`; the binary contains NO `inputTokens:2` /
    // `outputTokens:10` rate and NO date logic — the $2/$10-through-2026-08-31
    // promo appears ONLY in embedded doc prose as billing-side intro pricing
    // ("Per-token pricing is unchanged at the $3/$15 sticker (introductory
    // $2/$10 per MTok applies through 2026-08-31)" @222439766; pricing table
    // "$3.00 ($2.00 intro through 2026-08-31)" @222047026). lingxi matches
    // the binary exactly: cost builtin_reference claude-sonnet-5 =
    // 3_000/15_000/3_750/300 nano-USD, locked by cost
    // builtin_has_sonnet_5_standard_3_15.
    Entry { version: "2.1.197", item: "Sonnet 5 default model, native 1M context, promo $2/$10 per Mtok through 2026-08-31", disposition: Disposition::Implemented },
    // ── 2.1.196 ────────────────────────────────────────────────────────────
    // M1b binary-probed: "Org default" is sourced from the claude.ai OAuth
    // bootstrap — `fetchBootstrapData` persists `orgModelDefaultCache`
    // {name, updated_at, data_source, override_user_selection, orgUuid} into
    // local config (@214604765), validated against
    // `oauthAccount.organizationUuid` in `getOrgModelDefaultCache`/`b6r`
    // (@208724792, firstParty-only via `Zle`), cleared on oauth_logout
    // (@214599714); the /model picker suffixes " · Org default" (`rha`
    // @210923280) or " · Set by your organization" (managed model setting,
    // `zRn`) on the Default row (`YRn` @207950852). "Role default" does NOT
    // exist in the 2.1.198 binary (0 string hits). lingxi has neither source:
    // no claude.ai bootstrap/client-data cache (grep orgModelDefault|
    // modelAccessCache|clientDataCache → only a doc comment in
    // tool-api/src/model_prompt_gate.rs) and no managed-settings `model` key
    // (PolicySettings is permission-rule provenance only,
    // permission/src/rule.rs; engine-desktop settings_watch fires ConfigChange
    // hooks, parses no model). No org-policy seam exists to source the label.
    Entry { version: "2.1.196", item: "Org default models (Org default/Role default in /model)", disposition: Divergence("LingXi now reads managed availableModels/modelOverrides policy, but has no claude.ai orgModelDefaultCache bootstrap or Org default/Role default entries in /model") },
    // M13 N/A-with-evidence: in the 2.1.198 binary the ONLY
    // default-name-at-creation generator is `ast()` = `${adjective}-${noun}`
    // (crypto-random picks from the `_pi`/`ypi` word lists, @207957524), and
    // its call sites are (a) Remote Control bridge session titles
    // `${lJt()}-${ast()}` — `claude remote-control` @219124566 and the
    // in-session bridge repl auto-start @220332899, where `lJt()` =
    // CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX || sanitized hostname ||
    // "remote-control" — and (b) plan-file slugs (`fCe` @219392307). Local
    // sessions still title via customTitle / AI rename
    // (`rename_generate_name`) / first-prompt extraction. So this changelog
    // entry is the Remote Control bridge default title, and LingXi's local
    // session names remain outside that unavailable bridge surface.
    Entry { version: "2.1.196", item: "Readable default session names at start", disposition: Divergence("binary-verified: the readable default name (adjective-noun ast()) is minted only for Remote Control bridge sessions + plan slugs; lingxi has no Remote Control bridge") },
    // M6 landed: tui_core::render::osc8::file_link ports the binary's `t2()`
    // (file:// OSC 8 target, plain-path display) with byte-locked tests and
    // both native-scrollback and alternate-screen attachment wiring.
    // Finder/Explorer reveal remains the host terminal's responsibility.
    Entry { version: "2.1.196", item: "Clickable file attachments (Cmd/Ctrl-click reveals in Finder)", disposition: Disposition::Implemented },
    // M11: `mcp list`/`mcp get` surface unapproved (repo-self-approved) project
    // `.mcp.json` servers as the byte-exact binary status `SSc` = "\u23F8 Pending
    // approval (run `claude` to approve)" and NEVER spawn/health-check them —
    // 1:1 with the binary list `$Tf` (`status: n.has(i) ? SSc : (await
    // ySc(i,a)).status`) and get `qTf` (`i==="pending" ? {status:SSc} : … : await
    // ySc(t,s)`), where the pending branch SKIPS `ySc` (the connect/spawn probe).
    // lingxi's list/get were already probe-free (client-side), so the spawn-guard
    // held trivially; the gap was DISPLAY. apps/cli commands/mcp.rs now flags
    // pending project servers (is_pending_project_server + project_server_is_approved,
    // the scope guard keeps a same-named user/local server fully shown). Locked by
    // pending_approval_tests (byte-exact string, pending vs approved, user/local
    // neighbor never mislabelled, trust-reset reverts approved→pending).
    Entry { version: "2.1.196", item: "mcp list/get do not spawn repo-self-approved servers; Pending approval shown", disposition: Disposition::Implemented },
    // M8 equivalent: the cc fix quarantines an unreadable wake transcript
    // instead of deleting it. LingXi's attach/respawn paths preserve the
    // durable launch transcript across Fresh→Resume, and the session crate +
    // resume loader contain no transcript unlink/remove_file path. The exact
    // oracle quarantine filename is not observable in this architecture, but
    // the safety contract (never delete the transcript) is closed.
    Entry { version: "2.1.196", item: "Waking a background job never deletes its transcript (set aside instead)", disposition: Disposition::Implemented },
    // M12 landed (flicker) + divergence (telemetry): the FLICKER fix ports the
    // binary `Bha`/`Nha` monotonic guard (@210953352) into
    // `ApiService::record_rate_limit_from_headers_at` / `_from_429_at` — a
    // response whose record timestamp is OLDER than the last recorded one is
    // dropped, so an out-of-order (parallel) response can never flip the warning
    // off. Combined with the orchestrator's existing full-value
    // `emit_rate_limit_if_changed` change-gate (stricter than the binary's
    // status+overage `kqt` gate), the warning neither flickers nor re-emits an
    // unchanged state. Locked by llm-runtime
    // stale_parallel_response_does_not_flip_rate_limit_warning_off +
    // equal_or_increasing_timestamps_always_record. The over-counted-telemetry
    // half is N/A: lingxi never ported `tengu_claudeai_limits_status_changed`
    // (grep: 0 hits in telemetry/llm-runtime/orchestrator), so there is no
    // shared limits-status counter to over-count.
    Entry { version: "2.1.196", item: "Rate-limit warning flicker + over-counted telemetry with parallel requests", disposition: Disposition::Implemented },
    // M9 verified: cannot reproduce in lingxi's architecture. The workflow
    // subagent's result surface is the runner's single terminal
    // `Completed.result` (agent/src/runner.rs: `structured_result` is set
    // ONLY by a schema-VALID StructuredOutput input; a rejected input feeds
    // back an `is_error` ToolResult and the model retries), and
    // `PoolSubagentSpawner::spawn` ignores Message events — so a
    // schema-rejected attempt has no rendered recap to duplicate beside its
    // retry. Locked by agent::runner_test::
    // schema_rejected_attempt_is_not_surfaced_beside_its_retry (exactly one
    // Completed; payload is the retry's; rejected sentinel absent).
    Entry { version: "2.1.196", item: "No duplicate recap after schema-rejected StructuredOutput retry", disposition: Disposition::Implemented },
    // M13 landed: 1:1 port of the binary's PowerShell command semantics
    // (`Wja`/`W6p`/`JEo`/`G6p` @213326500-213329000) as
    // tool-shell `powershell_semantics` — PowerShell-aware last-segment split
    // (quoted `|` is NOT a separator; `'...'` literal, `"..."` with backtick
    // escapes, `#` comments, `&&`/lone-`&` statement breaks), call-operator +
    // quoted-.exe-path base extraction, and the `j6p` map
    // (grep/rg/egrep/fgrep/findstr exit 1 = "No matches found",
    // robocopy 0-7 succeed) plus `git grep`/`git diff` (`isError:
    // code!==0&&code!==1`, byte-locked `Nht`/`q6p`). `PowerShellTool` result
    // data routes `is_error` through the interpreter and carries
    // `returnCodeInterpretation` (the binary result field). The Bash-side
    // `interpretCommandResult` gained the same 2.1.196 additions from the
    // 2.1.198 oracle (`cLp`/`uLp`/`lLp` @212637913): egrep/fgrep + git
    // diff/grep with `-C`/`-c` value-skip (exit>=2 semantics). Locked by
    // tool-shell powershell_semantics::tests, powershell::tests::
    // exit_one_search_commands_are_not_failures, and command_semantics tests
    // (egrep_fgrep/git_diff_and_grep/quoted_pipe).
    Entry { version: "2.1.196", item: "PowerShell git diff/grep, egrep/fgrep, quoted | patterns: exit 1 is not failure", disposition: Disposition::Implemented },
    // M7 seam: this is the IN-APP side panel (task panel inside the running
    // TUI), not the standalone `claude agents` view. LingXi now has a generic
    // `/tasks` picker and a type-aware one-line running-agent summary, but it
    // does not yet expose the oracle's focus/subagent-type/running-status
    // behavior in a live side panel with the same refresh contract.
    Entry { version: "2.1.196", item: "Agents side panel: focus, subagent types, running status fixes", disposition: Divergence("LingXi now exposes `/tasks` and type-aware running-agent summaries, but the oracle's live in-app side-panel focus/subagent-type/status surface is still incomplete") },
    // M7 landed: `agents::run` gates the interactive view on the bypass
    // request (`Cli::bypass_requested` = binary `nis`), runs the root
    // refusal (`permission::enforce_bypass_safety`, byte-locked message =
    // binary `refuseBypassUnderRoot` @223855350) and mounts the byte-locked
    // `tui::startup_bypass` disclaimer unless
    // `skipDangerousModePermissionPrompt` is already set
    // (`ensureAgentsBypassConsent` parity); the attach respawn forwards
    // `--dangerously-skip-permissions`/`--permission-mode` to the dispatched
    // session (locked by `commands::agents::tests::
    // attach_forwards_bypass_to_dispatched_session`).
    Entry { version: "2.1.196", item: "claude agents --dangerously-skip-permissions shows disclaimer, applies bypass", disposition: Disposition::Implemented },
    Entry { version: "2.1.196", item: "Remote sessions auto-resume after server restart", disposition: Divergence("Anthropic cloud/remote infra; LingXi has no remote-session backend") },
    // M13 landed: `/cd` validates and confirms the target in the TUI, swaps
    // the shared SessionCwd, atomically retargets the transcript writer, and
    // emits a JSONL `relocated` marker. Resume discovery consumes the marker
    // and the moved transcript as one session, so sanitized-directory
    // collisions no longer resurrect the old location.
    Entry { version: "2.1.196", item: "/cd moved sessions don't reappear in old dir's resume list (special chars)", disposition: Disposition::Implemented },
    // M13 landed: `plugin validate` walks local `.` plugins and marketplace
    // manifests, validates declared/discovered skills, agents, commands,
    // output styles, hooks, and MCP payloads, and aggregates every error
    // class instead of stopping at the first shallow manifest failure.
    Entry { version: "2.1.196", item: "plugin validate: local '.' plugins included; all error classes reported", disposition: Disposition::Implemented },
    // M6 partial: tui-rata now has the real `/rewind` checkpoint subsystem —
    // persisted file-history snapshots, transcript truncation, and an
    // interactive restore-scope picker — but the live idle Esc path still
    // follows the layered view/completion/interrupt/quit policy. It does not
    // route a double-Esc gesture into that picker, so the exact changelog
    // regression remains open even though `/rewind` itself is implemented.
    Entry { version: "2.1.196", item: "Esc Esc at idle prompt opens rewind menu (regression fix)", disposition: Divergence("LingXi now has persisted /rewind checkpoints and a restore-scope picker, but idle Esc still follows the layered quit policy and does not open that picker on double-Esc") },
    Entry { version: "2.1.196", item: "MCP OAuth: no-scope request must not ask for full scopes_supported catalog", disposition: Disposition::Implemented },
    // M13 landed as a regression LOCK: lingxi's `/context`
    // (commands/core/src/context.rs) renders
    // `OrchestratorHandle::context_window_usage`, which sums the session's
    // CUMULATIVE usage (`SessionState::usage`, orchestrator/src/
    // handle_impl.rs) with no model-id/provider lookup — the failure mode the
    // binary fixed (a per-model usage lookup coming up empty under
    // Bedrock-style model ids) cannot occur by construction. Locked by
    // orchestrator handle_impl::tests::
    // context_window_usage_is_model_id_agnostic_bedrock_regression (Bedrock
    // inference-profile id + non-zero session usage → real token counts).
    Entry { version: "2.1.196", item: "/context shows real token counts on Bedrock", disposition: Disposition::Implemented },
    // M13 landed: the manual-only `/deep-research` launcher and immutable
    // built-in workflow enforce independent verification votes and omit claims
    // without enough support, so verifier failures cannot become an all-refuted
    // answer.
    Entry { version: "2.1.196", item: "/deep-research verifier failures reported as unverified, not all-refuted", disposition: Disposition::Implemented },
    // M13 landed: marketplace entries now carry dependency requirements and
    // the installer resolves them across local-folder catalogs, enforcing
    // every accumulated semver range and any allowed cross-marketplace pin.
    Entry { version: "2.1.196", item: "Plugin dependency pins honored for local-folder git-backed marketplaces", disposition: Disposition::Implemented },
    // M7 landed: `cli::agents_registry::merged_state` ports the binary's
    // `mGf` (@223855350) — a terminal outcome beats a stale `blocked` tempo,
    // so a row can never flip Done ↔ Needs-input (locked by
    // `merged_state_terminal_beats_blocked_tempo_no_done_needs_input_flip`);
    // tui-rata `agents_screen` bands/labels are byte-locked to `NZo`/`MFc`
    // (@222792125: "Ready for review"/"Needs input"/"Working"/"Completed")
    // and result rows referencing a PR show `PR #N` via the `Hon` token-scan
    // port (`extract_pr_number`, locked by `lines_show_pr_reference_on_row`).
    Entry { version: "2.1.196", item: "Agents view status: no Done/Needs-input flip; Needs attention; PR link", disposition: Disposition::Implemented },
    Entry { version: "2.1.196", item: "Voice dictation: no swallowed spaces / spurious recording on fast typing", disposition: Divergence("platform voice input; LingXi voice stack differs (sherpa)") },
    // M8 partial/deferred: LingXi now has the durable `--bg` dispatch,
    // `jobs/<short>/state.json`, and a detached daemon/`__bg-run` worker that
    // a restarted supervisor can adopt. The supervisor deliberately fails a
    // vanished worker closed rather than replaying its prompt, so full oracle
    // survival across worker loss/update and the Windows handoff contract are
    // still not proven.
    Entry { version: "2.1.196", item: "Background sessions survive process stop/restart/update (incl. Windows handoff)", disposition: Divergence("LingXi now persists --bg jobs and adopts detached daemon workers across supervisor restarts, but vanished-worker continuation, binary-update handoff, and Windows end-to-end survival remain unproven") },
    // M8 remains a partial divergence: auto-resume = the FleetView respawn
    // (`needsRespawn`/`ees`: outcome failure|stopped && terminal && !exec,
    // then `kon`→`vRt` relaunch) over daemon-backed jobs. LingXi now has a
    // durable daemon worker store and manual attach/respawn, but opening the
    // agents view does not yet discover and relaunch a worker killed by a
    // daemon restart.
    Entry { version: "2.1.196", item: "Workers killed by daemon restart auto-resume when agents view opens", disposition: Divergence("LingXi now has durable daemon workers and manual attach/respawn, but agents-view open still does not auto-resume a worker killed by daemon restart") },
    Entry { version: "2.1.196", item: "/code-review workflow: five cleanup finders merged into one (-25% tokens)", disposition: Disposition::Implemented },
    Entry { version: "2.1.196", item: "Per-frame rendering skips no-op subtree walks during streaming", disposition: Disposition::Implemented },
    // M12 landed: `llm_runtime::model::stream_watchdog` ports the binary's
    // default-ON idle watchdog (`jo = CLAUDE_ENABLE_STREAM_WATCHDOG ?? !0`,
    // `fzr()` = `max(env, 300000)` @208691984). `drive_stream` wraps each
    // blocking frame read in a `tokio::time::timeout` (deadline reset per event)
    // and aborts with a detectable `StreamInterrupted` idle-timeout error.
    // Disabled via `LINGXI_ENABLE_STREAM_WATCHDOG=0` (+ `CLAUDE_` alias);
    // timeout raised via `LINGXI_STREAM_IDLE_TIMEOUT_MS` (+ `CLAUDE_` alias,
    // 5-min floor). Locked by stream_watchdog unit tests +
    // service_test::streaming_idle_watchdog_aborts_hung_stream.
    Entry { version: "2.1.196", item: "Streaming idle watchdog on by default (5 min, env kill-switch)", disposition: Disposition::Implemented },
    // M13 N/A-with-evidence: lingxi has no Remote Control runtime to gate;
    // only compatibility surfaces remain (the `remoteControlAtStartup` settings-key migration,
    // PushNotification's "Remote Control inactive" strings, the `bridge`
    // command-name stub, and SendMessage's documented-unported `bridge:`
    // peer). With no bridge to start, the non-Anthropic-base-URL disable gate
    // has nothing to disable.
    Entry { version: "2.1.196", item: "Remote Control disabled when ANTHROPIC_BASE_URL is non-Anthropic", disposition: Divergence("no Remote Control runtime in lingxi; the base-URL gate has no surface") },
    // M7 landed: the foreground TUI's empty-composer Left key opens the
    // agents/tasks view through the same path as `defaultToAgentsView`, with
    // the editing guard and attached-session confirmation covered by
    // bottom-pane and chat-widget tests.
    Entry { version: "2.1.196", item: "Agents view opens with single ← from foreground sessions", disposition: Disposition::Implemented },
];

/// Valid mission ids from the alignment plan.
const MISSIONS: &[&str] = &[
    "M1", "M2", "M3", "M4", "M5", "M6", "M7", "M8", "M9", "M10", "M11", "M12", "M13",
];

#[test]
fn every_changelog_entry_is_triaged() {
    for e in CHECKLIST {
        assert!(
            e.disposition != Disposition::Unknown,
            "untriaged changelog entry ({} — {})",
            e.version,
            e.item
        );
        if let Mission(m) = e.disposition {
            assert!(
                MISSIONS.contains(&m),
                "entry '{}' references unknown mission {m}",
                e.item
            );
        }
        if let Divergence(reason) = e.disposition {
            assert!(
                !reason.is_empty(),
                "divergence for '{}' must state a reason",
                e.item
            );
        }
    }
}

/// M14 closure gate: once every mission (M0–M13, M1b) has landed, no checklist
/// entry may still be parked under a `Mission(...)` disposition — each must have
/// resolved to `Implemented` (with a locking test) or `Divergence(reason)`.
#[test]
fn no_mission_dispositions_remain_after_m14_closure() {
    let parked: Vec<&str> = CHECKLIST
        .iter()
        .filter_map(|e| match e.disposition {
            Mission(_) => Some(e.item),
            _ => None,
        })
        .collect();
    assert!(
        parked.is_empty(),
        "M14 closure requires every entry resolved to Implemented/Divergence; still parked under Mission(...): {parked:?}"
    );
}

#[test]
fn current_surface_audit_keeps_corrected_dispositions() {
    let entry = |item: &str| {
        CHECKLIST
            .iter()
            .find(|entry| entry.item == item)
            .unwrap_or_else(|| panic!("missing checklist item: {item}"))
    };

    // These were stale Divergence rows: each corresponding subsystem now has
    // a live implementation and its own lower-level regression coverage.
    for item in [
        "Sandbox classifier: dedupe repeated same-host requests",
        "/deep-research verifier failures reported as unverified, not all-refuted",
        "Plugin dependency pins honored for local-folder git-backed marketplaces",
        "/code-review workflow: five cleanup finders merged into one (-25% tokens)",
        "Agents view opens with single ← from foreground sessions",
        "Waking a background job never deletes its transcript (set aside instead)",
        "/diff panel refreshes on external branch switch/commit",
        "Cmd+click opens URLs in fullscreen in Warp; double-click selects whole URL",
        "Clickable file attachments (Cmd/Ctrl-click reveals in Finder)",
        "/cd moved sessions don't reappear in old dir's resume list (special chars)",
        "plugin validate: local '.' plugins included; all error classes reported",
    ] {
        assert_eq!(
            entry(item).disposition,
            Disposition::Implemented,
            "{item} must stay implemented after the cross-version audit"
        );
    }

    // These remain real gaps, but their reasons must describe the current
    // partial surfaces rather than the pre-implementation stubs.
    let current_gap_reasons = [
        (
            "Agents side panel: focus, subagent types, running status fixes",
            "`/tasks`",
        ),
        (
            "Esc Esc at idle prompt opens rewind menu (regression fix)",
            "persisted /rewind checkpoints",
        ),
        (
            "Background sessions survive process stop/restart/update (incl. Windows handoff)",
            "daemon workers",
        ),
        (
            "Workers killed by daemon restart auto-resume when agents view opens",
            "daemon workers",
        ),
        (
            "Org default models (Org default/Role default in /model)",
            "managed availableModels/modelOverrides",
        ),
        (
            "Focus mode: subagents in activity summary; completed notifications fold to one count",
            "advertises /focus",
        ),
    ];
    for (item, marker) in current_gap_reasons {
        let Disposition::Divergence(reason) = entry(item).disposition else {
            panic!("{item} must remain a documented divergence");
        };
        assert!(
            reason.contains(marker),
            "{item} divergence reason no longer records current surface marker {marker:?}: {reason}"
        );
    }
}

#[test]
fn checklist_is_complete_snapshot() {
    // Locked counts from the official changelog snapshot (2026-07-02):
    // 31 entries in 2.1.198, 1 in 2.1.197, 27 in 2.1.196.
    let count = |v: &str| CHECKLIST.iter().filter(|e| e.version == v).count();
    assert_eq!(count("2.1.198"), 31, "2.1.198 entry count drifted");
    assert_eq!(count("2.1.197"), 1, "2.1.197 entry count drifted");
    assert_eq!(count("2.1.196"), 27, "2.1.196 entry count drifted");
}

// ── Fixture sanity: the golden captures really are the 2.1.198 surface ──────

#[test]
fn root_help_fixture_pins_flags_missions_will_wire() {
    for flag in [
        "--safe-mode",
        "--bare",
        "--agents <json>",
        "--agent <agent>",
        "--plugin-dir <path>",
        "--from-pr",
        "--prompt-suggestions",
        "--effort <level>",
        "--no-session-persistence",
        "--bg, --background",
    ] {
        assert!(
            ROOT_HELP.contains(flag),
            "expected `{flag}` in captured 2.1.198 root help"
        );
    }
}

#[test]
fn gateway_subcommand_surface_is_pinned() {
    assert!(GATEWAY_HELP.contains("Usage: claude gateway [options]"));
    assert!(GATEWAY_HELP.contains("--config <path>"));
    assert!(GATEWAY_HELP.contains("Run the enterprise auth/telemetry gateway"));
}

#[test]
fn agents_and_mcp_help_fixtures_nonempty() {
    assert!(
        AGENTS_HELP.contains("agents"),
        "agents help fixture looks wrong"
    );
    assert!(MCP_HELP.contains("mcp"), "mcp help fixture looks wrong");
}
