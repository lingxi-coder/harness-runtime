//! Locked constant tables of the 86 builtin command names + the 19 core names.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md`
//! Task 0 step 2 (name list) + Task 0 step 3 (19 core list).
//!
//! NOTE: The 2026-05-28 addendum locked the total at **99** (originally `102`
//! in the plan prose). The 2026-06-20 slash-parity pass (findings #66/#67 vs
//! claude-code v2.1.183) re-locked the total to **94**: it removed the three
//! commands claude-code deleted upstream (`vim`, `pr-comments`, `output-style`
//! — 0 command objects in the v2.1.183 binary) and the two that became
//! `aliases:["cost","stats"]` of `/usage` rather than standalone commands
//! (`cost`, `stats` — 0 `name:"cost"`/`name:"stats"` command objects). The 18
//! core count is unchanged: `cost`'s core slot was reassigned to `usage` (now
//! the implemented command that absorbs cost/stats).
//!
//! The 2026-07-04 batch-8 slash-command pass added six newly-ported,
//! implemented commands (`fork`, `goal`, `recap`, `reload-skills`,
//! `skill-doctor`, `stop`), re-locking the total from 94 to **100**. The 18
//! core count is unchanged (the new commands are wired via
//! `command_core::register_core_batch_8`, not the core placeholder path).
//!
//! The follow-on batch-8 tail pass added the net-new headless `autocompact`
//! command (the real `type:"local",supportsNonInteractive:!0` auto-compact
//! window reporter — see `command_core::autocompact`), re-locking the total
//! from 100 to **101**. Its stub-bucket siblings from the same triage
//! (`powerup`, `scroll-speed` = interactive-only net-new; `install`,
//! `sandbox-toggle` = interactive/host-bound already-in-surface stubs) were
//! kept as stubs / not added, so only the total moved. (SLASH-13 later
//! reversed the `powerup` half of that call — see the 2026-08-20 note below.)
//! `/btw` is now wired
//! through batch 8 and the TUI retains its most recent side-question panel.
//!
//! The 2026-07-14 slash-parity pass (H-BIN-11 vs claude-code v2.1.207) added
//! FIVE `local-jsx` builtin command objects that had been missed by the
//! name-lock lineage — they exist byte-identically as far back as the local
//! 2.1.205 binary, so they are pre-existing misses, not post-lock drift:
//! `cd` (`Move this session to a new working directory`, ungated),
//! `background`/alias `bg` (`Send this session to the background and free the
//! terminal`, `isEnabled:()=>!0`), `focus` (`Toggle focus view: just your
//! prompt, summary, and response`, `requires:{ink}`), `tui` (`Set the terminal
//! UI renderer (default | fullscreen)`, ungated), and `usage-credits`
//! (`Configure usage credits or request them from your admin when you hit a
//! limit`, two objects
//! gated by `bnr()` = `!DISABLE_EXTRA_USAGE_COMMAND && (rateLimitStatus!==null
//! || isOverageProvisioningAllowed())`, split interactive/non-interactive on
//! `isNonInteractiveSession()`). That oracle total was **106**: 105 after
//! removing the stale `x402` entry, plus `auto-mode-setup` (2.1.220 WIZARD-06,
//! `{type:"local",name:"auto-mode-setup",supportsNonInteractive:!0}`).
//! `usage-credits` is hidden from the default palette / `/help` because `bnr()`
//! resolves `false` in a fresh session with no subscription or rate-limit
//! status (see [`is_palette_hidden`] + [`USAGE_CREDITS_BNR_GATED`]); the other
//! four are visible. The 2026-08-13 follow-up adds the pre-existing 2.1.205
//! `local-jsx` `/workflows` command already implemented by the TUI, re-locking
//! the shared registry surface at **107**.
//!
//! The 2026-08-20 byte-alignment pass vs claude-code **2.1.238** keeps the
//! total at **107** through a one-in / one-out swap: `review` was DELETED
//! upstream (`name:"review"` 2.1.220 = 1 hit, 2.1.238 = 0; the PR-review
//! surface moved into the bundled `code-review` skill), and `subtask`
//! (`{type:"local-jsx",name:"subtask",description:"Send a subagent off with
//! your full context; its result comes back here",argumentHint:"<task>"}`
//! @ 2.1.238 296247354) — long implemented by `command_core::subtask` but
//! never listed here, so never advertised in `/help` or the palette — takes
//! its slot.
//!
//! The same pass then re-locked 107 -> **108** for SLASH-13: `powerup`
//! (`Rkl={type:"local-jsx",name:"powerup",description:"Discover Claude Code
//! features through quick interactive lessons",requires:{ink:!0}}`
//! @ 2.1.238 296124285) is an **ungated** member of the builtin command table
//! `ijT()` — no `isEnabled`, no `isHidden` — so upstream lists it in `/help`
//! and the palette for every interactive session. The 2026-07-04 triage above
//! excluded it for having no LingXi body, but 39 of the port's other visible
//! `/help` rows are likewise unimplemented (the registry mirrors the oracle's
//! ADVERTISED surface; the body status is tracked separately in the parity
//! fixture), so the exclusion was inconsistent. It registers through
//! `register_all_builtin_commands`'s pass-1 stub loop like any other
//! body-less name.
//!
//! Its two SLASH-13 siblings stay out, both verified at the oracle:
//! * `scroll-speed` — `cFT={…name:"scroll-speed",…,isEnabled:()=>{if(!Ws())
//!   return!1;…}}` @296167417. `Ws()` @285037919 is the FULLSCREEN-renderer
//!   predicate; with no `tui` setting it falls through to the statsig gates
//!   `QVb()` (`tengu_amber_creek`, default `!1`) and `gbGateCached`, so the
//!   command is filtered out of a fresh default session — the port's omission
//!   already matches the default advertised surface.
//! * `daemon` — registered only via `daemon:{open:()=>nXe(),whenOpen:[mRl],
//!   whenClosed:[]}` @296430738 and `function nXe(){return!1}` @284247792, so
//!   it never reaches the command table at all.

/// Every built-in slash command's runtime name (without leading `/`),
/// ASCII-sorted. Locked at length **87** for the current 2.1.270 parity
/// surface. The latest oracle removed the stale internal command objects
/// listed in the slash audit, including the policy-gated `heapdump` object
/// which has no provider-neutral runtime analog.
///
/// Changing the count or membership requires bumping the parity fixture
/// `crates/test-harness/src/parity/fixtures/parity_slash_commands_102.json`
/// (fixture filename retained for git-history continuity; the counts inside
/// reflect the current membership lock).
pub const BUILTIN_COMMAND_NAMES: &[&str; 87] = &[
    "add-dir",
    "advisor",
    "agents",
    "auto-mode-setup",
    "autocompact",
    "autofix-pr",
    "background",
    "branch",
    "brief",
    "btw",
    // SLASH-06: `bug` is its OWN command upstream, not an alias of `feedback`.
    // Oracle 2.1.238 @248164336: `{aliases:["share"],type:"local-jsx",
    // name:"bug",description:"Report a bug or share your conversation",
    // argumentHint:"[report]",immediate:!0,requires:{ink:!0}}`.
    "bug",
    "cd",
    "chrome",
    "clear",
    "color",
    "commit",
    "commit-push-pr",
    "compact",
    "config",
    "context",
    "copy",
    "desktop",
    "diff",
    "doctor",
    "effort",
    "exit",
    "export",
    "extra-usage",
    "fast",
    "feedback",
    "files",
    "focus",
    "fork",
    "goal",
    "help",
    "hooks",
    "ide",
    "init",
    "init-verifiers",
    "insights",
    "install",
    "install-github-app",
    "install-slack-app",
    "keybindings",
    "login",
    "logout",
    "mcp",
    "memory",
    "mobile",
    "model",
    // SLASH (cc2.1.269): `/output-style` came BACK after its 2.1.183 removal.
    // The shipping object is `type:"local"` with `supportsNonInteractive:!0` —
    // a text command. Its `local-jsx` "moved to /config" sibling is gated on
    // `tengu_maple_sundial`, which defaults FALSE, so it is dormant.
    "output-style",
    "passes",
    "permissions",
    "plan",
    "plugin",
    // SLASH-13 (cc2.1.238 @296124285): ungated `local-jsx` member of the
    // `ijT()` command table — upstream advertises it in every interactive
    // session. The provider-neutral lesson handler is registered after the
    // pass-1 fallback, so it remains available to headless and TUI dispatch.
    "powerup",
    "privacy-settings",
    "rate-limit-options",
    "recap",
    "release-notes",
    "reload-plugins",
    "reload-skills",
    "remote-env",
    "rename",
    "resume",
    "rewind",
    "security-review",
    "session",
    // SLASH-06: `share` is NOT a command — it is `bug`'s alias (oracle
    // @248164336). `name:"share"` has ZERO hits in both the 2.1.220 and the
    // 2.1.238 binary; the old "compiled stub" annotation came from the stale
    // de-minified source, not from the shipped binary.
    "skill-doctor",
    "skills",
    "status",
    "statusline",
    "stickers",
    "stop",
    "subtask",
    "tasks",
    "teleport",
    "terminal-setup",
    "theme",
    "tui",
    "ultraplan",
    "upgrade",
    "usage",
    "usage-credits",
    "version",
    "voice",
    "workflows",
];

/// The 19 core commands that ship with real implementations in M5-10 / M5-11.
/// Subset of [`BUILTIN_COMMAND_NAMES`], ASCII-sorted.
///
/// In M5-09 each of these gets a per-name placeholder struct in
/// [`super::core_placeholders`]; the placeholders return the same locked stub
/// literal as the shared `UnimplementedCommandHandler` until the real bodies
/// land in M5-10 (batch 1: clear/compact/help/exit/memory/init) and M5-11
/// (batch 2: the remaining 12).
pub const BUILTIN_CORE_NAMES: &[&str; 19] = &[
    "agents",
    "auto-mode-setup",
    "clear",
    "compact",
    "config",
    "doctor",
    "exit",
    "help",
    "hooks",
    "init",
    "login",
    "logout",
    "mcp",
    "memory",
    "model",
    "permissions",
    "status",
    "usage",
    "version",
];

/// Bucket **(d)** — commands that **claude-code itself** disables, hides, or
/// gates behind an entitlement / `USER_TYPE` / statsig flag, so returning the
/// locked stub literal is **already behaviorally faithful** — these are *not*
/// a Rust parity gap.
///
/// In the leaked claude-code TypeScript reference each of these names either:
///
/// - compiles to a `name: 'stub'` command object in the shipped `index.js`
///   (the command exists in the palette but its body is a no-op stub), or
/// - is `USER_TYPE === 'ant'`-gated (Anthropic-internal only), or
/// - is statsig- / entitlement-gated (the body never runs for normal users).
///
/// Therefore the Rust port keeps every name here on the shared
/// [`super::UnimplementedCommandHandler`] **by design**. This table exists to
/// document *why* each is a non-goal and to prevent a future contributor from
/// "implementing" a command that claude-code itself ships as a stub.
///
/// Each entry is `(name, reason)`. Every `name` is a member of
/// [`BUILTIN_COMMAND_NAMES`] and is **disjoint** from the implemented core set
/// [`BUILTIN_CORE_NAMES`] (see the unit tests below). Cross-reference: SPECS.md
/// "slash-commands-stubs" bucket (d) + Batch 6.
///
/// NOTE: this is purely additive documentation — it changes **no** runtime
/// behavior and does **not** alter the locked command surface.
pub const INTENTIONALLY_DISABLED_COMMANDS: &[(&str, &str)] = &[
    (
        "advisor",
        "compiled stub in claude-code (gated advisor surface)",
    ),
    ("autofix-pr", "compiled stub in claude-code"),
    ("teleport", "compiled stub in claude-code (internal)"),
    // SLASH-06: `share` was listed here as a compiled stub. It is not a command
    // at all — it is the `bug` command's alias (oracle 2.1.238 @248164336), and
    // `name:"share"` has zero hits in the 2.1.220 and 2.1.238 binaries. The
    // entry came from the stale de-minified source tree.
];

// ============================================================================
// STUB.6 — faithful-stub audit: refine the bucket-(d) table into TWO disjoint
// partitions, verified against the claude-code TypeScript/compiled reference.
//
// The legacy `INTENTIONALLY_DISABLED_COMMANDS` table once conflated faithful
// stubs with host-bound gaps. The last host-bound entry (`btw`) now has a live
// handler and TUI panel, leaving only the correct-by-design partition.
//
// Evidence was taken directly from `claude-code/src/commands/<name>/`:
//   - compiled `index.js` literally `{ isEnabled: () => false, isHidden: true,
//     name: 'stub' }`  → the command never runs for ANYONE.
//   - `isEnabled: () => process.env.USER_TYPE === 'ant'`  → Anthropic-internal.
//   - `isEnabled: () => checkStatsigFeatureGate(...)` / `feature('KAIROS')`  →
//     feature-gated OFF in the external build.
//   - `isEnabled: () => canUserConfigureAdvisor()` + `isHidden` when not
//     configurable  → entitlement-gated OFF + hidden by default.
// ============================================================================

/// **Partition A — CORRECT-BY-DESIGN faithful stubs.** claude-code *itself*
/// disables, hides, feature-gates-OFF, or ships a literal no-op `name: 'stub'`
/// for each of these, so the Rust port returning the locked
/// `"{name}: not implemented in v0.6.0 (M5)"` literal is **behaviorally
/// faithful** — there is no command body to port. These are non-goals: a
/// future contributor must NOT "implement" them.
///
/// Each entry is `(name, why-claude-code-ships-nothing)`. Every `name` is a
/// member of [`BUILTIN_COMMAND_NAMES`], disjoint from the implemented core set
/// [`BUILTIN_CORE_NAMES`], and disjoint from [`HOST_BOUND_DEFERRED_GAPS`]
/// (asserted by the STUB.6 partition tests in the test-harness).
///
/// This is now the complete [`INTENTIONALLY_DISABLED_COMMANDS`] set.
pub const CORRECT_BY_DESIGN_STUBS: &[(&str, &str)] = &[
    // --- compiled `{ isEnabled:()=>false, isHidden:true, name:'stub' }` ---
    (
        "autofix-pr",
        "compiled `name:'stub'` (isEnabled:()=>false, isHidden) in claude-code",
    ),
    (
        "teleport",
        "compiled `name:'stub'` (isEnabled:()=>false, isHidden) in claude-code",
    ),
    // --- USER_TYPE==='ant' (Anthropic-internal only) ---
    // --- entitlement-gated OFF + hidden by default ---
    (
        "advisor",
        "isEnabled:()=>canUserConfigureAdvisor() (false by default) + isHidden",
    ),
];

/// Host/UI-bound command gaps. Empty after `/btw` gained both a handle-bound
/// command implementation and a reopenable TUI panel.
pub const HOST_BOUND_DEFERRED_GAPS: &[(&str, &str)] = &[];

/// **Statically-hidden named commands** — real, enabled (or conditionally
/// enabled) builtin command objects that claude-code ships with the literal
/// `isHidden:!0` flag, so they NEVER appear in the slash palette or `/help`
/// even though they remain dispatchable when typed in full.
///
/// These are NOT in [`CORRECT_BY_DESIGN_STUBS`] (they are not `name:'stub'`
/// no-ops, and several are conditionally enabled), so they must be filtered
/// separately. Verified against the shipped `claude.exe` v2.1.183 command
/// objects (each carries a literal `isHidden:!0` immediately after its
/// `name`/`description`):
///
/// ```text
/// name:"extra-usage",description:"Renamed to /usage-credits",isHidden:!0,isEnabled:()=>pct()&&!kr()
/// name:"rate-limit-options",description:"Show options when rate limit is reached",isEnabled:()=>Ro()||!1,isHidden:!0
/// ```
///
/// claude-code's palette / `/help` list builders all apply the filter
/// `commands.filter(c => !c.isHidden && !$te(c))` (3 confirmed sites:
/// `!ne.isHidden&&!$te(ne)`, `!S.isHidden&&!$te(S)`, `!Ur.isHidden&&!$te(Ur)`),
/// so any `isHidden:!0` command is dropped from both surfaces.
pub const HIDDEN_PALETTE_COMMANDS: &[&str] =
    &["auto-mode-setup", "extra-usage", "rate-limit-options"];

/// **`bnr()`-gated, hidden-by-default named commands** — real, conditionally
/// enabled builtin command objects whose `isEnabled` resolves to `false` in a
/// fresh session with no subscription and no rate-limit status, so they are
/// dropped from the default palette / `/help` even though they remain
/// dispatchable when typed in full.
///
/// H-BIN-11 (cc2.1.207): `/usage-credits` ships two objects, both gated by
/// `isEnabled:()=>bnr()&&…` where
/// `bnr()=!DISABLE_EXTRA_USAGE_COMMAND && (rateLimitStatus!==null ||
/// isOverageProvisioningAllowed())`. Because a default port session carries no
/// subscription snapshot and no live rate-limit status, `bnr()` is `false`, so
/// the command is `$te(c)`-filtered out of the default surface — matching a
/// fresh claude-code session. The DYNAMIC un-hide (when a subscription /
/// rate-limit status arrives) is applied by the palette builder via
/// `platform_api::subscription::SubscriptionSnapshot::is_usage_credits_command_enabled`;
/// only the default (off) state is modeled statically here.
pub const USAGE_CREDITS_BNR_GATED: &[&str] = &["usage-credits"];

/// **Statically DISABLED named commands** (SLASH-14) — real, implemented
/// builtin command objects whose every upstream twin carries the literal
/// `isEnabled:()=>!1`. They are *not* `name:'stub'` no-ops (so they do not
/// belong in [`CORRECT_BY_DESIGN_STUBS`], which must stay disjoint from the
/// implemented core set), and they are *not* `isHidden`-flagged (so they do not
/// belong in [`HIDDEN_PALETTE_COMMANDS`]) — but claude-code's
/// `commands.filter(c => !c.isHidden && !$te(c))` drops them all the same,
/// because `$te(c)` (isEnabled() === off) is true. They remain dispatchable
/// when typed in full, exactly like the other filtered buckets.
///
/// Verified byte-for-byte in the 2.1.238 oracle @296268759 — BOTH `/version`
/// objects are disabled (identical in 2.1.220, so this is a long-standing port
/// divergence rather than 2.1.238 drift):
///
/// ```text
/// C$T={type:"local-jsx",name:"version",description:"Show this session's version (autoupdate may have a newer one)",isEnabled:()=>!1,immediate:!0,requires:{ink:!0}}
/// gAl={type:"local",name:"version",description:"Print the version this session is running (not what autoupdate downloaded)",isEnabled:()=>!1,get isHidden(){return!Dn()},supportsNonInteractive:!0,…}
/// ```
///
/// `tui/src/command.rs` already carries `/version` with `advertised: false`;
/// this table makes the headless/bridge registry agree with it.
pub const STATICALLY_DISABLED_COMMANDS: &[&str] = &["version"];

/// **`terminalOriented:!0` commands** (SLASH-15) — the builtins claude-code
/// 2.1.238 marks as belonging to the terminal host, so a thin/remote client
/// knows to route them locally. NEW in 2.1.238: every 2.1.220 command object
/// carries no `terminalOriented` key at all.
///
/// Verified per-object in the 2.1.238 oracle. `terminalOriented` occurs at
/// **15** byte offsets there and at **0** in 2.1.220 (`grep -abo`; note
/// `grep -c` on this binary counts LINES, not occurrences, so it under-reports
/// — an earlier revision of this comment claimed "9 raw hits" on that basis):
/// six are the command objects below, four are the two bundled-command
/// normalizers (`terminalOriented:e.terminalOriented` @287816908 / @287821547,
/// each written twice), three are the consumers quoted further down
/// (@298685916, @307252016, @307252579), one is the bundled `doctor` SKILL
/// (see the note at the end), and one is a snapshot copy (@102087824).
///
/// ```text
/// @294987032 {type:"local-jsx",name:"color",…,terminalOriented:!0,…}
/// @294987135 {type:"local",name:"color",terminalOriented:!0,supportsNonInteractive:!0,…}
/// @296279356 {type:"local-jsx",name:"exit",aliases:["quit"],…,terminalOriented:!0,…}
/// @296279442 {type:"local",name:"exit",terminalOriented:!0,supportsNonInteractive:!0,…}
/// @296260416 {type:"local",name:"reload-plugins",…,terminalOriented:!0,thinClientDispatch:"control-request",…}
/// @296309820 {type:"prompt",name:"statusline",…,terminalOriented:!0,disableNonInteractive:!0,…}
/// ```
///
/// The flag is not purely internal: the stream-json `system`/`init` emitter
/// (`Fin`, @298685916) derives a payload key from it —
///
/// ```text
/// let n=e.commands.filter((i)=>i.userInvocable!==!1&&i.terminalOriented===!0).map((i)=>i.name);
/// …slash_commands:…, ...n.length>0&&{terminal_slash_commands:n}, apiKeySource:…
/// ```
///
/// — i.e. `terminal_slash_commands` is spread in directly AFTER `slash_commands`
/// and only when the list is non-empty (`terminal_slash_commands` is absent
/// from 2.1.220 entirely).
///
/// The bridge/SDK announcer is the mirror image — it SUBTRACTS the flagged
/// entries from what it announces (@307252016 / @307252579):
///
/// ```text
/// function _o(tt=l().mcp.commands){return Fi(tt).filter((wt)=>wt.terminalOriented!==!0)}
/// …writeSdkMessages([LSs({…,commands:_o(Ho.mcp.commands),…,loadedSkills:Ct.filter((Ri)=>Ri.terminalOriented!==!0),…})])
/// ```
///
/// NOTE 1 (CLOSED 2026-08-21): the emission half is wired. `apps/cli`'s
/// `build_init_params` (`apps/cli/src/stream_json.rs`) derives
/// `terminal_slash_commands` by running the advertised `slash_commands` list
/// through [`is_terminal_oriented`], and `build_init_frame` spreads it into the
/// `system`/`init` frame immediately after `slash_commands` and only when
/// non-empty, matching `Fin` (@298685916). The mirror-image consumer — the
/// bridge/SDK announcer's `_o()` SUBTRACTION (@307252579) — belongs to
/// `apps/bridge-server`, whose announcer still does no filtering; that is the
/// one remaining unconsumed use of this table.
///
/// NOTE 2 (open, for the skills owner): upstream also flags the BUNDLED SKILL
/// `doctor` — `wd({name:"doctor",aliases:["checkup"],…,terminalOriented:!0,…})`
/// @304067710 — and the emitter's `e.commands` includes bundled skills (they
/// pass through the normalizers above). The port models `doctor` as a builtin
/// COMMAND, not a bundled skill, so it is left out of this table: adding it
/// would encode a skill's flag in the command registry. Whoever wires the
/// emitter must decide which of the two models the payload should follow.
pub const TERMINAL_ORIENTED_COMMANDS: &[&str] = &["color", "exit", "reload-plugins", "statusline"];

/// Returns `true` when `name` carries claude-code 2.1.238's
/// `terminalOriented:!0` flag — see [`TERMINAL_ORIENTED_COMMANDS`].
#[must_use]
pub fn is_terminal_oriented(name: &str) -> bool {
    TERMINAL_ORIENTED_COMMANDS.contains(&name)
}

/// `(command, aliases)` for the builtins that ship an `aliases:` array in
/// claude-code (cp-03). The slash palette folds these into the fuzzy candidate
/// set (so typing `/cost` finds `/usage`) and, when a row matched via a typed
/// alias, shows ` (<alias>)` after the name (`createCommandSuggestionItem`).
/// Ported from each `commands/<name>/index.ts` `aliases` literal.
pub const COMMAND_ALIASES: &[(&str, &[&str])] = &[
    // H-BIN-11 cc2.1.207: `name:"background",aliases:["bg"]`.
    ("background", &["bg"]),
    // SLASH-06: upstream these are TWO commands, and the alias hangs off `bug`,
    // not `feedback` (oracle 2.1.238 @294965131 / @248164336):
    //   feedback: {type:"local-jsx",name:"feedback",…}          — no aliases
    //   bug:      {aliases:["share"],type:"local-jsx",name:"bug",…}
    ("bug", &["share"]),
    ("clear", &["reset", "new"]),
    ("config", &["settings"]),
    ("desktop", &["app"]),
    ("exit", &["quit"]),
    ("mobile", &["ios", "android"]),
    ("permissions", &["allowed-tools"]),
    ("plugin", &["plugins", "marketplace"]),
    ("resume", &["continue"]),
    ("rewind", &["checkpoint", "undo"]),
    ("session", &["remote"]),
    ("tasks", &["bashes"]),
    ("usage", &["cost", "stats"]),
];

/// The aliases for `name` (empty when it has none). See [`COMMAND_ALIASES`].
#[must_use]
pub fn command_aliases(name: &str) -> &'static [&'static str] {
    COMMAND_ALIASES
        .iter()
        .find(|(cmd, _)| *cmd == name)
        .map_or(&[], |(_, aliases)| *aliases)
}

/// Returns `true` if `name` is hidden or disabled in claude-code's default
/// external build and therefore must NOT appear in the slash palette or the
/// `/help` screen — mirroring claude-code's
/// `commands.filter(c => !c.isHidden && !$te(c))` (where `$te` is the
/// "isEnabled() resolves to off" gate). The set is the union of:
///
/// - [`CORRECT_BY_DESIGN_STUBS`] — the 3 commands claude-code disables /
///   hides / feature-gates-OFF / ships as a compiled `name:'stub'`
///   (`isEnabled:()=>!1,isHidden:!0`) for ordinary users, so `$te(c)` is true
///   (or, for `advisor`/`teleport`/`autofix-pr`, the entitlement /
///   statsig / remote gate is OFF by default, which also resolves `isHidden`
///   true and `isEnabled()` false); and
/// - [`HIDDEN_PALETTE_COMMANDS`] — the 3 enabled-but-hidden named commands
///   (`auto-mode-setup`, `extra-usage`, `rate-limit-options`); and
/// - [`USAGE_CREDITS_BNR_GATED`] — the 1 `bnr()`-gated command
///   (`usage-credits`), off-by-default in a fresh no-subscription session; and
/// - [`STATICALLY_DISABLED_COMMANDS`] — the 1 command (`version`) whose every
///   upstream twin is `isEnabled:()=>!1` (SLASH-14).
///
/// Total = 3 + 3 + 1 + 1 = **8** filtered names, leaving 86 − 8 = 78
/// visible in `/help`. The host-bound `btw` command and the implemented
/// `/reload-plugins` command remain visible. Likewise `install-slack-app`,
/// `mobile`, and `desktop` carry no
/// default-off hidden gate (`desktop`'s `Dsl()` returns `true`) and stay
/// visible; and `background`, `cd`, `focus`, `tui` (the other four H-BIN-11
/// additions) are ungated and stay visible.
#[must_use]
pub fn is_palette_hidden(name: &str) -> bool {
    HIDDEN_PALETTE_COMMANDS.contains(&name)
        || USAGE_CREDITS_BNR_GATED.contains(&name)
        || STATICALLY_DISABLED_COMMANDS.contains(&name)
        || CORRECT_BY_DESIGN_STUBS.iter().any(|(n, _)| *n == name)
}

/// `(command_name, DISABLE_*_COMMAND env var)` pairs whose command object in
/// claude-code v2.1.183 carries an `isEnabled:()=>!je.DISABLE_X_COMMAND`
/// truthiness gate (`je` is `process.env`). When the env var is set to any
/// non-empty value the command's `isEnabled()` resolves to `false`, so it is
/// dropped from the slash palette and `/help` (the same
/// `commands.filter(c => !c.isHidden && !$te(c))` path) **and** must not
/// resolve.
///
/// Verified verbatim against the shipped `claude.exe` v2.1.183 command objects:
///
/// ```text
/// name:"doctor",...,isEnabled:()=>!je.DISABLE_DOCTOR_COMMAND
/// name:"login",...,isEnabled:()=>!je.DISABLE_LOGIN_COMMAND
/// name:"logout",...,isEnabled:()=>!je.DISABLE_LOGOUT_COMMAND
/// name:"upgrade",...,isEnabled:()=>!kz()&&!je.DISABLE_UPGRADE_COMMAND&&sa()!=="enterprise"
/// name:"install-github-app",...,isEnabled:()=>!je.DISABLE_INSTALL_GITHUB_APP_COMMAND
/// ```
///
/// NOTE: claude-code also defines `DISABLE_BUG_COMMAND` and
/// `DISABLE_FEEDBACK_COMMAND`, but in v2.1.183 those gate the `/feedback`
/// **input handler** (`if(je.DISABLE_FEEDBACK_COMMAND)return{kind:"disabled"…}`),
/// not a command object's `isEnabled` — and there is no standalone `bug`
/// command — so they are intentionally NOT modeled here (LingXi's `feedback`
/// command object stays enabled, matching the binary). Likewise
/// `DISABLE_EXTRA_USAGE_COMMAND` does not appear on the `extra-usage` object
/// (`isEnabled:()=>pct()&&!kr()`), so it is excluded.
pub const ENV_DISABLE_GATED_COMMANDS: &[(&str, &str)] = &[
    ("doctor", "DISABLE_DOCTOR_COMMAND"),
    ("install-github-app", "DISABLE_INSTALL_GITHUB_APP_COMMAND"),
    ("login", "DISABLE_LOGIN_COMMAND"),
    ("logout", "DISABLE_LOGOUT_COMMAND"),
    ("upgrade", "DISABLE_UPGRADE_COMMAND"),
];

/// Returns `true` when `name`'s `DISABLE_*_COMMAND` env gate (see
/// [`ENV_DISABLE_GATED_COMMANDS`]) is tripped — i.e. the env var is present and
/// non-empty. Mirrors claude-code's `isEnabled:()=>!je.DISABLE_X` JS-truthiness
/// semantics: in JS `!process.env.X` is `false` (command disabled) for any
/// non-empty string value, including `"0"`/`"false"`, and `true` (enabled) only
/// when the var is unset or the empty string.
#[must_use]
pub fn is_command_env_disabled(name: &str) -> bool {
    ENV_DISABLE_GATED_COMMANDS
        .iter()
        .find(|(cmd, _)| *cmd == name)
        .is_some_and(|(_, var)| std::env::var(var).is_ok_and(|v| !v.is_empty()))
}

/// One-line descriptions for the builtin commands, used for slash-palette +
/// `/help` rendering. The first 19 are the M5-10 core set; the remainder
/// (cp-01) are the real per-command `description:` strings ported from each
/// claude-code `commands/<name>` object so no VISIBLE palette row shows the
/// `"(unimplemented in v0.6.0)"` placeholder. Commands that are hidden/disabled
/// (internal/dev/entitlement-gated) or LingXi-specific with no claude-code
/// analogue keep the placeholder fallback. Descriptions retain the "Claude
/// Code" product noun verbatim (1:1 with the oracle — the branding judgment
/// applies to screen titles, not command help strings).
#[must_use]
pub fn core_description(name: &str) -> &'static str {
    match name {
        // (M4 cc2.1.198) The /agents wizard was removed; the command now
        // returns static guidance. Description verbatim from the 2.1.198
        // binary command object (`name:"agents"`, description `(removed) …`).
        "agents" => "(removed) Ask Claude to create/manage subagents, or edit .lingxi/agents/",
        // WIZARD-06. SLASH-03: re-worded upstream between 2.1.220 and 2.1.238.
        // Byte-exact from BOTH 2.1.238 twins (oracle @294963678):
        //   mSl={type:"local",name:"auto-mode-setup",supportsNonInteractive:!0,
        //        description:"Teach auto mode about your environment, plus optional rule tweaks",…}
        //   hhT={type:"local-jsx",name:"auto-mode-setup",
        //        description:"Teach auto mode about your environment, plus optional rule tweaks",…}
        // `count` of the new string: 2.1.238 = 2, 2.1.220 = 0; the old
        // "Set up and customise auto mode — environment context, …" has 0 hits
        // in 2.1.238.
        "auto-mode-setup" => "Teach auto mode about your environment, plus optional rule tweaks",
        "clear" => "Start a new session with empty context; previous session stays on disk (resumable with /resume)",
        "compact" => "Free up context by summarizing the conversation so far",
        "config" => "Open settings",
        "doctor" => "Diagnose and verify your LingXi installation and settings",
        "exit" => "Exit the CLI",
        "help" => "Show help and available commands",
        "hooks" => "View hook configurations for tool events",
        "init" => "Initialize a new LINGXI.md file with codebase documentation",
        "login" => "Sign in with your Anthropic account",
        "logout" => "Sign out from your Anthropic account",
        "mcp" => "Manage MCP servers",
        "memory" => "Edit LINGXI.md files and memory settings",
        "model" => "Set the AI model for LingXi",
        // Byte-exact from the 2.1.270 command object `QNt`.
        "output-style" => "List output styles or switch to one",
        "permissions" => "Manage allow and deny tool permission rules",
        "status" => "Show LingXi status including version, model, account, API connectivity, and tool statuses",
        // claude-code v2.1.183 live `usage` command object
        // (`name:"usage",aliases:["cost","stats"],...`).
        "usage" => "Show session cost, plan usage, and what's contributing to your limits",
        // SLASH-14: `Print version information` matched NEITHER oracle object.
        // Both 2.1.238 twins @296268759 are `isEnabled:()=>!1`, so `/version`
        // is filtered out of `/help` and the palette entirely (see
        // [`STATICALLY_DISABLED_COMMANDS`]); it stays dispatchable, and the
        // interactive twin's string is what the TUI row already shows, so the
        // two port registries now agree.
        "version" => "Show this session's version (autoupdate may have a newer one)",
        // (cp-01) Remaining visible builtins — real claude-code descriptions.
        "add-dir" => "Add a new working directory",
        "advisor" => "Let Claude consult a stronger model at key moments",
        // Net-new headless auto-compact-window reporter (see
        // `command_core::autocompact`). Verbatim from the 2.1.198 binary's
        // headless `type:"local"` autocompact command object.
        "autocompact" => "Configure the auto-compact window size",
        // (H-BIN-11 cc2.1.207) `local-jsx` command objects, descriptions
        // verbatim from the 2.1.207 binary.
        "background" => "Send this session to the background and free the terminal",
        "branch" => "Create a branch of the current conversation at this point",
        "brief" => "Toggle brief-only mode",
        "cd" => "Move this session to a new working directory",
        "btw" => "Ask a quick side question without interrupting the main conversation",
        // SLASH-06: byte-exact from oracle 2.1.238 @248164336. No product name
        // appears in this string, so there is nothing to rebrand.
        "bug" => "Report a bug or share your conversation",
        "chrome" => "Open Claude in Chrome settings",
        "color" => "Set the prompt bar color for this session",
        "commit" => "Create a git commit",
        "commit-push-pr" => "Commit, push, and open a PR",
        "context" => "Visualize current context usage as a colored grid",
        "copy" => "Copy Claude's last response to clipboard (or /copy N for the Nth-latest)",
        "desktop" => "Continue the current session in Claude Desktop",
        "diff" => "View uncommitted changes and per-turn diffs",
        "effort" => "Set effort level for model usage",
        "export" => "Export the current conversation to a file or clipboard",
        "fast" => "Toggle fast mode",
        "feedback" => "Submit feedback about LingXi",
        "files" => "List all files currently in context",
        "focus" => "Toggle focus view: just your prompt, summary, and response",
        "ide" => "Manage IDE integrations and show status",
        "init-verifiers" => "Create verifier skill(s) for automated verification of code changes",
        "insights" => "Generate a report analyzing your LingXi sessions",
        "install" => "Install LingXi native build",
        "install-github-app" => "Set up Claude GitHub Actions for a repository",
        "install-slack-app" => "Install the Claude Slack app",
        "keybindings" => "Open your keyboard shortcuts file",
        "mobile" => "Show QR code to download the Claude mobile app",
        "passes" => "Share a free week of LingXi with friends and earn extra usage",
        "plan" => "Enable plan mode or view the current session plan",
        "plugin" => "Manage LingXi plugins",
        // SLASH-13. Oracle 2.1.238 @296124285:
        //   Rkl={type:"local-jsx",name:"powerup",description:"Discover Claude
        //        Code features through quick interactive lessons",
        //        requires:{ink:!0}}
        // Product-name substitution only, matching the rest of this table
        // ("Order Claude Code stickers" -> "Order LingXi stickers", "Generate a
        // report analyzing your Claude Code sessions" -> "... your LingXi
        // sessions"), both of which are 2 / 4 raw hits in the same binary.
        "powerup" => "Discover LingXi features through quick interactive lessons",
        "privacy-settings" => "View and update your privacy settings",
        "release-notes" => "View release notes",
        "reload-plugins" => "Activate pending plugin changes in the current session",
        "remote-env" => "Configure the default remote environment for teleport sessions",
        "rename" => "Rename the current conversation",
        "resume" => "Resume a previous conversation",
        "rewind" => "Restore the code and/or conversation to a previous point",
        "security-review" => "Complete a security review of the pending changes on the current branch",
        "session" => "Show remote session URL and QR code",
        "skills" => "List available skills",
        "statusline" => "Set up LingXi's status line UI",
        "stickers" => "Order LingXi stickers",
        "tasks" => "View and manage everything running in the background",
        "terminal-setup" => "Install Shift+Enter key binding for newlines",
        "theme" => "Change the theme",
        "tui" => "Set the terminal UI renderer (default | fullscreen)",
        "ultraplan" => "LingXi on the web drafts an advanced plan you can edit and approve",
        "upgrade" => "Upgrade to Max for higher rate limits and more Opus",
        "usage-credits" => {
            "Configure usage credits or request them from your admin when you hit a limit"
        }
        // Deprecated hidden alias of `/usage-credits`
        // (`name:"extra-usage",description:"Renamed to /usage-credits",isHidden:!0`).
        "extra-usage" => "Renamed to /usage-credits",
        "voice" => "Toggle voice mode",
        "workflows" => "Browse running and completed workflows",
        // Batch-8 implemented commands (real handlers in `command-core`); their
        // `description()` bodies carry the verbatim oracle strings, mirrored here
        // so the palette / `/help` rows never show the placeholder fallback.
        "fork" => "Copy this conversation into a new background session and keep working here",
        "goal" => "Set a goal — keep working until the condition is met",
        "recap" => "Generate a one-line session recap now",
        "reload-skills" => "Pick up skills added or changed on disk during this session",
        "skill-doctor" => "Show which loaded skills are unused and costing context",
        "stop" => "Stop this background session; transcript and worktree are kept",
        // cc2.1.238 `w$m` (`/subtask`, registered by `register_core_batch_8`
        // whenever agent view is on — the default). Verbatim command-object
        // description; without this row `/help` showed the placeholder.
        "subtask" => "Send a subagent off with your full context; its result comes back here",
        _ => "(unimplemented in v0.6.0)",
    }
}

/// Serializes every test that mutates or reads the `DISABLE_*_COMMAND` process
/// env: the env-gate tests below AND the sibling `help_render` render tests,
/// whose `render_help_screen()` consults those gates. Rust runs a crate's tests
/// in-process and in parallel, so without ONE shared lock a mutation here can
/// race a concurrent render in another module and intermittently drop a command
/// (e.g. `/login`) — the latent flake behind `every_visible_command_appears_once`
/// / `output_has_exactly_69_lines`. `pub(crate)` so help_render can lock it too.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_count_locked_at_87() {
        assert_eq!(BUILTIN_COMMAND_NAMES.len(), 87);
    }

    #[test]
    fn cc_2_1_252_removed_internal_names_stay_out_of_current_surface() {
        // These exact command objects existed in older local oracle builds but
        // have zero `name:"…"` hits in the current 2.1.252 binary. Keep the
        // removals locked in both the advertised list and every stub bucket.
        for name in [
            "ant-trace",
            "backfill-sessions",
            "break-cache",
            "bridge",
            "bughunter",
            "ctx-viz",
            "debug-tool-call",
            "env",
            "good-claude",
            "issue",
            "mock-limits",
            "oauth-refresh",
            "onboarding",
            "perf-issue",
            "remote-setup",
            "reset-limits",
            "sandbox-toggle",
            "summary",
            "tag",
            "thinkback",
            "thinkback-play",
        ] {
            assert!(
                !BUILTIN_COMMAND_NAMES.contains(&name),
                "stale /{name} remains"
            );
            assert!(
                !INTENTIONALLY_DISABLED_COMMANDS
                    .iter()
                    .any(|(candidate, _)| *candidate == name),
                "stale /{name} remains in the disabled bucket"
            );
            assert!(
                !CORRECT_BY_DESIGN_STUBS
                    .iter()
                    .any(|(candidate, _)| *candidate == name),
                "stale /{name} remains in the faithful-stub bucket"
            );
        }

        // `heapdump` is policy-gated and has no provider-neutral runtime
        // analog in LingXi, so it is removed from typed recognition as well
        // as the default advertised surface.
        assert!(!BUILTIN_COMMAND_NAMES.contains(&"heapdump"));
        assert!(!HIDDEN_PALETTE_COMMANDS.contains(&"heapdump"));
    }

    #[test]
    fn h_bin_11_names_present_and_gated() {
        // H-BIN-11 (cc2.1.207): five newly-registered names.
        for n in ["background", "cd", "focus", "tui", "usage-credits"] {
            assert!(
                BUILTIN_COMMAND_NAMES.contains(&n),
                "H-BIN-11 name '{n}' missing"
            );
        }
        // `/background` carries the `bg` alias.
        assert_eq!(command_aliases("background"), &["bg"]);
        // Four are visible; `/usage-credits` is bnr()-gated hidden-by-default.
        for visible in ["background", "cd", "focus", "tui"] {
            assert!(
                !is_palette_hidden(visible),
                "/{visible} must be visible in the default palette"
            );
        }
        assert!(
            is_palette_hidden("usage-credits"),
            "/usage-credits is bnr()-gated and hidden by default"
        );
        // Descriptions are the verbatim 2.1.207 command-object strings.
        assert_eq!(
            core_description("cd"),
            "Move this session to a new working directory"
        );
        assert_eq!(
            core_description("background"),
            "Send this session to the background and free the terminal"
        );
        assert_eq!(
            core_description("focus"),
            "Toggle focus view: just your prompt, summary, and response"
        );
        assert_eq!(
            core_description("tui"),
            "Set the terminal UI renderer (default | fullscreen)"
        );
        assert_eq!(
            core_description("usage-credits"),
            "Configure usage credits or request them from your admin when you hit a limit"
        );
    }

    /// cc2.1.238 byte-alignment: the one-in / one-out membership swap plus the
    /// six `/help` one-liners and the two changed descriptions, each verbatim
    /// from a 2.1.238 command object.
    #[test]
    fn cc_2_1_238_command_surface() {
        // `/review` was DELETED upstream (`name:"review"`: 220 = 1, 238 = 0);
        // `/subtask` took its slot in the then-107-name lock.
        assert!(
            !BUILTIN_COMMAND_NAMES.contains(&"review"),
            "/review was removed in claude-code 2.1.238"
        );
        assert!(
            BUILTIN_COMMAND_NAMES.contains(&"subtask"),
            "/subtask is registered by batch 8 and must be advertised"
        );
        assert!(!is_palette_hidden("subtask"));
        assert_eq!(
            core_description("subtask"),
            "Send a subagent off with your full context; its result comes back here"
        );

        // `/rewind` carries THREE names upstream:
        // `aliases:["checkpoint","undo"]` (238 and 220 alike).
        assert_eq!(command_aliases("rewind"), &["checkpoint", "undo"]);

        // SLASH-14: both `/version` objects are `isEnabled:()=>!1`, so the
        // command is filtered out of `/help` and the palette while staying
        // dispatchable and a member of the locked name list.
        assert!(BUILTIN_COMMAND_NAMES.contains(&"version"));
        assert!(
            is_palette_hidden("version"),
            "/version is isEnabled:()=>!1 upstream and must not be advertised"
        );
        assert_eq!(
            core_description("version"),
            "Show this session's version (autoupdate may have a newer one)"
        );

        // SLASH-13: `/powerup` is an UNGATED `local-jsx` object in the oracle's
        // `ijT()` table (no isEnabled, no isHidden), so upstream advertises it
        // in every interactive session. Its two siblings stay out on oracle
        // evidence: `scroll-speed` needs `Ws()` (the fullscreen renderer,
        // statsig-off by default) and `daemon` hangs off `open:()=>nXe()` with
        // `nXe(){return!1}`.
        assert!(BUILTIN_COMMAND_NAMES.contains(&"powerup"));
        assert!(!is_palette_hidden("powerup"));
        assert_eq!(
            core_description("powerup"),
            "Discover LingXi features through quick interactive lessons"
        );
        assert!(!BUILTIN_COMMAND_NAMES.contains(&"scroll-speed"));
        assert!(!BUILTIN_COMMAND_NAMES.contains(&"daemon"));

        // SLASH-03: `/auto-mode-setup` was re-worded between 220 and 238
        // (both twins). It stays palette-hidden, so this only shows up on the
        // registry/bridge listing, not in `/help`.
        assert_eq!(
            core_description("auto-mode-setup"),
            "Teach auto mode about your environment, plus optional rule tweaks"
        );

        // 220 -> 238 description changes (LingXi brands CLAUDE.md as LINGXI.md).
        assert_eq!(
            core_description("memory"),
            "Edit LINGXI.md files and memory settings"
        );
        // Agent view is on by default, so `/fork` advertises `b$m`, the twin
        // `register_core_batch_8` actually registers.
        assert_eq!(
            core_description("fork"),
            "Copy this conversation into a new background session and keep working here"
        );

        // Six one-liners that matched NO oracle command object on any surface.
        for (name, want) in [
            ("hooks", "View hook configurations for tool events"),
            ("keybindings", "Open your keyboard shortcuts file"),
            (
                "tasks",
                "View and manage everything running in the background",
            ),
            ("chrome", "Open Claude in Chrome settings"),
            (
                "advisor",
                "Let Claude consult a stronger model at key moments",
            ),
            (
                "usage-credits",
                "Configure usage credits or request them from your admin when you hit a limit",
            ),
        ] {
            assert_eq!(core_description(name), want, "/{name} description drift");
        }
    }

    #[test]
    fn terminal_oriented_set_is_the_four_2_1_238_objects() {
        // Byte-verified from the 2.1.238 command objects; 2.1.220 has the flag
        // on NO object at all, so this whole table is 2.1.238 drift.
        assert_eq!(
            TERMINAL_ORIENTED_COMMANDS,
            &["color", "exit", "reload-plugins", "statusline"]
        );
        let full: std::collections::HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
        for name in TERMINAL_ORIENTED_COMMANDS {
            assert!(full.contains(name), "'{name}' is not a real builtin");
            assert!(is_terminal_oriented(name));
            // The oracle's init filter is
            // `userInvocable!==!1 && terminalOriented===!0`, so a hidden or
            // disabled command could still qualify — but none of these four is
            // filtered out of the palette, which is worth pinning.
            assert!(!is_palette_hidden(name), "'{name}' unexpectedly hidden");
        }
        assert!(!is_terminal_oriented("help"));
    }

    #[test]
    fn statically_disabled_names_are_real_builtins_and_unique_to_that_bucket() {
        let full: std::collections::HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
        for name in STATICALLY_DISABLED_COMMANDS {
            assert!(full.contains(name), "'{name}' is not a real builtin");
            // The four filter buckets must stay disjoint so the 28-name total
            // (and therefore the /help line count) is unambiguous.
            assert!(!HIDDEN_PALETTE_COMMANDS.contains(name), "'{name}' twice");
            assert!(!USAGE_CREDITS_BNR_GATED.contains(name), "'{name}' twice");
            assert!(
                !CORRECT_BY_DESIGN_STUBS.iter().any(|(n, _)| n == name),
                "'{name}' is not a compiled stub — it has a real implementation"
            );
        }
    }

    // ── #63 DISABLE_*_COMMAND env gates ──────────────────────────────────────
    // These tests mutate process env, so they serialize through the module-level
    // [`ENV_LOCK`] (shared with help_render) to avoid racing each other or any
    // concurrent env-reading render test in this binary.

    #[test]
    fn env_disable_gates_are_the_five_command_object_gated_names() {
        // Verified verbatim against claude.exe v2.1.183 command objects.
        let pairs: Vec<(&str, &str)> = ENV_DISABLE_GATED_COMMANDS.to_vec();
        assert_eq!(
            pairs,
            vec![
                ("doctor", "DISABLE_DOCTOR_COMMAND"),
                ("install-github-app", "DISABLE_INSTALL_GITHUB_APP_COMMAND"),
                ("login", "DISABLE_LOGIN_COMMAND"),
                ("logout", "DISABLE_LOGOUT_COMMAND"),
                ("upgrade", "DISABLE_UPGRADE_COMMAND"),
            ]
        );
        // Every gated name is a real builtin.
        let full: std::collections::HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
        for (cmd, _) in ENV_DISABLE_GATED_COMMANDS {
            assert!(full.contains(cmd), "gated name '{cmd}' is not a builtin");
        }
    }

    #[test]
    fn ungated_command_is_never_env_disabled() {
        let _g = ENV_LOCK.lock().unwrap();
        // `clear` has no DISABLE gate, so the helper is always false for it.
        assert!(!is_command_env_disabled("clear"));
        assert!(!is_command_env_disabled("feedback"));
    }

    #[test]
    fn gated_command_disabled_only_when_env_set_nonempty() {
        let _g = ENV_LOCK.lock().unwrap();
        let var = "DISABLE_DOCTOR_COMMAND";
        // Default (unset): enabled.
        std::env::remove_var(var);
        assert!(!is_command_env_disabled("doctor"));
        // Empty string: still enabled (JS `!""` is true → command stays on).
        std::env::set_var(var, "");
        assert!(!is_command_env_disabled("doctor"));
        // Any non-empty value: disabled (JS truthiness — even "0"/"false").
        std::env::set_var(var, "1");
        assert!(is_command_env_disabled("doctor"));
        std::env::set_var(var, "0");
        assert!(is_command_env_disabled("doctor"));
        std::env::remove_var(var);
        assert!(!is_command_env_disabled("doctor"));
    }

    #[test]
    fn env_disabled_command_is_dropped_from_help_render() {
        use crate::builtin_support::help_render::render_help_screen;
        let _g = ENV_LOCK.lock().unwrap();
        let var = "DISABLE_LOGIN_COMMAND";
        std::env::remove_var(var);
        let before = render_help_screen();
        assert!(before.contains("  /login "), "login visible by default");
        std::env::set_var(var, "1");
        let after = render_help_screen();
        assert!(
            !after.contains("  /login "),
            "login must be dropped from /help when DISABLE_LOGIN_COMMAND is set"
        );
        assert_eq!(
            after.matches('\n').count(),
            before.matches('\n').count() - 1,
            "exactly one command (login) is removed"
        );
        std::env::remove_var(var);
    }

    #[test]
    fn core_count_locked_at_19() {
        assert_eq!(BUILTIN_CORE_NAMES.len(), 19);
    }

    #[test]
    fn names_are_sorted_ascii_ascending() {
        let mut sorted = BUILTIN_COMMAND_NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            BUILTIN_COMMAND_NAMES.as_slice(),
            sorted.as_slice(),
            "BUILTIN_COMMAND_NAMES must be ASCII-sorted to keep diffs reviewable"
        );
    }

    #[test]
    fn core_names_are_sorted_ascii_ascending() {
        let mut sorted = BUILTIN_CORE_NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(BUILTIN_CORE_NAMES.as_slice(), sorted.as_slice());
    }

    #[test]
    fn no_duplicate_names() {
        let mut set = std::collections::HashSet::new();
        for n in BUILTIN_COMMAND_NAMES {
            assert!(
                set.insert(*n),
                "duplicate name {n} in BUILTIN_COMMAND_NAMES"
            );
        }
    }

    #[test]
    fn every_core_name_is_in_the_full_list() {
        let full: std::collections::HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
        for c in BUILTIN_CORE_NAMES {
            assert!(
                full.contains(c),
                "core name {c} not in BUILTIN_COMMAND_NAMES"
            );
        }
    }

    #[test]
    fn all_names_are_lowercase_ascii_or_hyphen_or_digit() {
        for n in BUILTIN_COMMAND_NAMES {
            for c in n.chars() {
                assert!(
                    c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit(),
                    "name '{n}' has invalid char '{c}' (only lowercase ascii + '-' + digits allowed)"
                );
            }
        }
    }

    #[test]
    fn no_name_starts_or_ends_with_hyphen() {
        for n in BUILTIN_COMMAND_NAMES {
            assert!(!n.starts_with('-'), "name '{n}' starts with hyphen");
            assert!(!n.ends_with('-'), "name '{n}' ends with hyphen");
        }
    }

    #[test]
    fn no_double_hyphen() {
        for n in BUILTIN_COMMAND_NAMES {
            assert!(!n.contains("--"), "name '{n}' contains '--'");
        }
    }

    #[test]
    fn includes_known_canonical_names() {
        // Spot-check a few rare ones so a future name rename doesn't drift silently.
        assert!(!BUILTIN_COMMAND_NAMES.contains(&"x402"));
        assert!(BUILTIN_COMMAND_NAMES.contains(&"terminal-setup"));
        assert!(BUILTIN_COMMAND_NAMES.contains(&"powerup"));
    }

    // ========================================================================
    // Batch 6 — bucket (d) faithful-stub audit (additive; behavior-neutral).
    //
    // These tests lock the INTENTIONALLY_DISABLED_COMMANDS table as documented
    // non-goals: every name is a real builtin that claude-code itself ships as a
    // stub/gated, so it must remain on the UnimplementedCommandHandler by
    // design. The tests assert the *classification invariant* only — they make
    // no change to the locked command surface (`parity_slash_commands_102`).
    // ========================================================================

    #[test]
    fn intentionally_disabled_count_is_3() {
        assert_eq!(
            INTENTIONALLY_DISABLED_COMMANDS.len(),
            3,
            "only faithful disabled stubs remain after the 2.1.252 stale-name audit"
        );
    }

    #[test]
    fn every_intentionally_disabled_name_is_a_real_builtin() {
        let full: std::collections::HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
        for (name, _reason) in INTENTIONALLY_DISABLED_COMMANDS {
            assert!(
                full.contains(name),
                "intentionally-disabled name '{name}' is not in BUILTIN_COMMAND_NAMES"
            );
        }
    }

    #[test]
    fn no_duplicate_intentionally_disabled_names() {
        let mut set = std::collections::HashSet::new();
        for (name, _reason) in INTENTIONALLY_DISABLED_COMMANDS {
            assert!(
                set.insert(*name),
                "duplicate name '{name}' in INTENTIONALLY_DISABLED_COMMANDS"
            );
        }
    }

    #[test]
    fn every_intentionally_disabled_reason_is_documented() {
        // The whole point of the table is to record *why* each is a non-goal.
        for (name, reason) in INTENTIONALLY_DISABLED_COMMANDS {
            assert!(
                !reason.trim().is_empty(),
                "intentionally-disabled name '{name}' must document a reason"
            );
        }
    }

    #[test]
    fn intentionally_disabled_is_disjoint_from_core() {
        // Bucket (d) commands are NOT implemented; the 19 core names ARE wired
        // to real handlers. The two sets must never overlap, otherwise a name is
        // both "implemented" and "intentionally stubbed" — a contradiction.
        let core: std::collections::HashSet<&str> = BUILTIN_CORE_NAMES.iter().copied().collect();
        for (name, _reason) in INTENTIONALLY_DISABLED_COMMANDS {
            assert!(
                !core.contains(name),
                "name '{name}' is both core (implemented) and intentionally-disabled"
            );
        }
    }

    #[test]
    fn intentionally_disabled_set_is_a_documented_non_goal() {
        // The remaining names are compiled no-op stubs. `share` used to be
        // listed here; it is not a command at all (it is `bug`'s alias).
        let table: std::collections::HashMap<&str, &str> =
            INTENTIONALLY_DISABLED_COMMANDS.iter().copied().collect();
        for n in ["advisor", "autofix-pr", "teleport"] {
            assert!(table.contains_key(n), "expected compiled-stub name '{n}'");
        }
    }

    #[test]
    fn intentionally_disabled_does_not_change_the_locked_surface() {
        // Guard the no-op property: the additive bucket-(d) table is a strict
        // subset of the locked name list and therefore cannot change the
        // total count, membership, or ordering that the parity fixture locks.
        assert!(INTENTIONALLY_DISABLED_COMMANDS.len() < BUILTIN_COMMAND_NAMES.len());
        assert_eq!(BUILTIN_COMMAND_NAMES.len(), 87);
    }

    // ========================================================================
    // STUB.6 — the refined two-partition split (CORRECT_BY_DESIGN_STUBS vs
    // HOST_BOUND_DEFERRED_GAPS). These lock the *classification invariant* of
    // the constants; the end-to-end "still dispatches to the stub literal"
    // regression lives in test-harness (parity_slash_command_stub_partition).
    // ========================================================================

    fn name_set(table: &[(&'static str, &'static str)]) -> std::collections::HashSet<&'static str> {
        table.iter().map(|(n, _)| *n).collect()
    }

    #[test]
    fn correct_by_design_count_is_3() {
        assert_eq!(CORRECT_BY_DESIGN_STUBS.len(), 3);
    }

    #[test]
    fn host_bound_deferred_set_is_empty() {
        assert!(HOST_BOUND_DEFERRED_GAPS.is_empty());
    }

    #[test]
    fn every_partitioned_name_is_a_real_builtin() {
        let full: std::collections::HashSet<&str> = BUILTIN_COMMAND_NAMES.iter().copied().collect();
        for (name, _) in CORRECT_BY_DESIGN_STUBS
            .iter()
            .chain(HOST_BOUND_DEFERRED_GAPS)
        {
            assert!(
                full.contains(name),
                "partitioned name '{name}' is not a real builtin"
            );
        }
    }

    #[test]
    fn every_partitioned_reason_is_documented() {
        for (name, reason) in CORRECT_BY_DESIGN_STUBS
            .iter()
            .chain(HOST_BOUND_DEFERRED_GAPS)
        {
            assert!(!reason.trim().is_empty(), "'{name}' must document a reason");
        }
    }

    #[test]
    fn no_duplicate_names_within_each_partition() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in CORRECT_BY_DESIGN_STUBS
            .iter()
            .chain(HOST_BOUND_DEFERRED_GAPS)
        {
            assert!(seen.insert(*name), "duplicate partitioned name '{name}'");
        }
    }

    #[test]
    fn the_two_partitions_are_disjoint() {
        // The whole point of STUB.6: a name cannot be BOTH a faithful
        // correct-by-design stub AND a genuine deferred gap.
        let cbd = name_set(CORRECT_BY_DESIGN_STUBS);
        let gaps = name_set(HOST_BOUND_DEFERRED_GAPS);
        assert!(
            cbd.is_disjoint(&gaps),
            "correct-by-design and host-bound-deferred overlap"
        );
    }

    #[test]
    fn both_partitions_are_disjoint_from_implemented_core() {
        // Neither a faithful stub nor a deferred gap may also be a wired core
        // command — that would be a contradiction.
        let core: std::collections::HashSet<&str> = BUILTIN_CORE_NAMES.iter().copied().collect();
        for (name, _) in CORRECT_BY_DESIGN_STUBS
            .iter()
            .chain(HOST_BOUND_DEFERRED_GAPS)
        {
            assert!(
                !core.contains(name),
                "'{name}' is both core (implemented) and stubbed"
            );
        }
    }

    #[test]
    fn partitions_union_equals_legacy_intentionally_disabled_set() {
        // The refined split is exactly the legacy bucket-(d) table re-bucketed:
        // CORRECT_BY_DESIGN ∪ HOST_BOUND_DEFERRED == INTENTIONALLY_DISABLED.
        // This proves the refinement neither dropped nor invented a name; it
        // only moved the three host-bound names into their correct partition.
        let legacy = name_set(INTENTIONALLY_DISABLED_COMMANDS);
        let mut union = name_set(CORRECT_BY_DESIGN_STUBS);
        union.extend(name_set(HOST_BOUND_DEFERRED_GAPS));
        assert_eq!(
            union, legacy,
            "partition union must equal the legacy disabled set"
        );
        assert_eq!(
            CORRECT_BY_DESIGN_STUBS.len() + HOST_BOUND_DEFERRED_GAPS.len(),
            INTENTIONALLY_DISABLED_COMMANDS.len(),
            "all disabled commands are correct-by-design"
        );
        assert_eq!(BUILTIN_COMMAND_NAMES.len(), 87);
    }

    #[test]
    fn btw_is_not_classified_as_disabled_or_deferred() {
        let disabled = name_set(INTENTIONALLY_DISABLED_COMMANDS);
        let gaps = name_set(HOST_BOUND_DEFERRED_GAPS);
        assert!(!disabled.contains("btw"));
        assert!(!gaps.contains("btw"));
    }
}
