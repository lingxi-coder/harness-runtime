# R10 review brief — completed targeted re-audit of the unreviewed delta

**Artifact under review:** `docs/mcp-plugin-byte-alignment-2.1.251-2026-08-28.md`
(v9 at review start; incorporated as v10, §0–§27).
**Oracle:** `~/.local/share/claude/versions/2.1.251`, build
`37534ac596d80cefb02d272f036adba4ba055d2c`.
**Reviewer:** Codex. **Author of the items below:** Claude, round 9.

**Status:** complete. The detailed verdict and evidence ledger are in
`docs/mcp-plugin-audit-R10-final-review-2026-08-29.md`. Of the ten targeted
items, eight were upheld, §26b was upheld with a narrower 401 precondition, and
the broad §27c claim was retracted. The residual `mcp add` redaction delta kept
at sign-off was itself withdrawn on 2026-08-29 (final review §6.1): the
`<redacted>` strings are telemetry labels, not user copy.

---

## Why this round is scoped, and why it is probably the last one that pays

Nine rounds, alternating. The yield split cleanly:

| round | who | new code gaps | corrections to the document |
| --- | --- | --- | --- |
| 1 | Claude (initial) | ~12 | — |
| 2 | Codex | ~3 | 6 |
| 3 | Claude (connect path) | ~5 | — |
| 4 | Codex | ~0 | 8 |
| 5 | Claude (self-review) | 2 | 0 (self-reported) |
| 6 | Claude (26 files) | 4 | — |
| 7 | Claude (14 files) | 4 | — |
| 8 | Claude (3 files) | 2 | — |
| 9 | Codex | 1 | 6 |

Two facts follow, and they set this brief's scope.

1. **The breadth sweep was complete.** Rounds 6–8 found things because there was
   unswept code; every file in `mcp/` and `plugin/` has now been compared. R10
   therefore targeted the unreviewed delta rather than assuming that file
   coverage alone proved behavioural completeness.
2. **The author's self-review does not catch his own errors.** Round 5 reported
   "no new retractions" — and §23a, introduced *in that round*, was inverted and
   only caught in round 9. Same for §25a (round 7) and §26b (round 8). The
   observed error rate on newly written claims is roughly **1 in 3**, and every
   one of those errors was a *direction* or *layer* inversion that reads
   perfectly well on its own.

So the value left is not breadth. It is **the ten claims no second reader has
seen**, at a prior of ~2–3 wrong among them.

---

## Scope: exactly ten items. Everything else was out of scope for R10.

### Group A — written from scratch in round 9 (never reviewed)

| # | claim | evidence behind it | attack this |
| --- | --- | --- | --- |
| **§27a** | `--mcp-config` entries are tagged `ConfigScope::Project` (`apps/cli/src/init.rs:398`) and therefore enter the project approval gate (`mcp/src/server_gate.rs:119-133`); the oracle stamps `scope:"dynamic"` | Binary @166780019: `let Tl={...ws,scope:"dynamic"}` inside the block referencing `["--mcp-config"]`; Rust line read directly | The port side is a one-line read, so attack the *consequence*: does `server_gate::decide` actually gate a `Project` entry that arrived from the flag, or does some earlier filter exempt it? |
| **§27b** | `requiresUserInteraction` is never parsed, so the permission prompt still offers allow-always | **UPHELD.** Binary @182519150–182520945 directly links `_meta["anthropic/requiresUserInteraction"]` to `requiresUserInteraction()`, `suppressesAlwaysAllowRule()`, and an ask result with `suppressAlwaysAllowRule: true`. | Port lacks the metadata/DTO/tool/UI propagation chain. |
| **§27c** | MCP server names reach CLI output unsanitised | **BROAD CLAIM RETRACTED.** Binary `CYt` / `Unt` and their real callers also render raw names. | ~~A narrower delta survives: Oracle redacts invalid/reserved `mcp add` names~~ — withdrawn 2026-08-29: the `<redacted>` strings are `ft(Error(raw), label)` telemetry labels; the thrown message and Rust's copy both carry the raw name. |
| **§27d** | `additionalDirectories` null-byte skip is out of scope for this document | **UPHELD.** | The document covers `mcp/` + `plugin/`, not the whole changelog. |

### Group B — rewritten in round 9 (the text is new; only the retraction was reviewed)

| # | claim as it now stands | attack this |
| --- | --- | --- |
| **§19** | Three gaps: (1) `inject_bearer` (`registry.rs:2402-2411`) `insert`s over a static `headers.Authorization`; (2) `has_headers_helper` keys on helper *presence*, not on it minting `Authorization`; (3) helper resolved before `resolve_oauth_spec`, so a bearer can clobber a minted header | (1) is now binary-and-source confirmed. (2) and (3) are read from source — check whether any *caller* orders them differently, and whether the oracle's `hasUserAuthHeader` really precedes its OAuth path rather than running alongside it. |
| **§23a** | The port is **unconditionally managed-only** for `allowedMcpServers` (`enterprise_policy.rs:133-139`), i.e. over-strict, not a hole | The inversion is fixed, but is "over-strict" the whole story? Check whether some other reader merges an ordinary-tier allowlist elsewhere, which would make the port merely *inconsistent* rather than strict. |
| **§23b** | The `--mcp-config` FIFO example is retracted (`init.rs:381` has `Path::is_file()`); the unguarded read is ambient `.mcp.json` / global config, and **no path** has a size limit or typed diagnostics | Confirm the ambient path really is unguarded end-to-end, and that the oracle's byte limit applies to ambient reads too — I only saw the limit in the diagnostic string. |
| **§24b** | The whole per-subagent inline `mcpServers` chain is unimplemented; `AgentToolResolver::resolve`'s one production caller passes a literal `&[]` (`tool_resolver.rs:360`); **no leak can be claimed** because no connection is created | Verify there is no *second* path that connects subagent MCP servers. If one exists, "unimplemented" is wrong and the leak question reopens. |
| **§25a** | **Retracted.** The oracle migrates `_v2.json` → `installed_plugins.json` (binary `MBt`: `renameSync(u,o)` + `Renamed installed_plugins_v2.json to installed_plugins.json`); production Rust already writes the right name and `installedAt`/`lastUpdated`/`installPath` | This one agrees with your round-9 finding — but your citation was `claude-code/src/*.ts`, which is a **stale tree**. Re-derive from the binary before signing off, so the retraction rests on the right evidence. |
| **§26b** | The port *does* silently re-exchange (`registry.rs:1152-1157`). Real gaps: no 300 s proactive window, no single-flight, and `reauth_oauth_spec` (`:1216-1250`) has no `xaa` branch so a 401 contradicts the port's own `:1148-1151` comment | Check the 401 claim hardest: is `reauth_oauth_spec` genuinely reachable for an `oauth.xaa` server, or does an earlier arm intercept it? |

---

## Rules for this round

1. **Provenance.** `~/Projects/LingXi-Next/claude-code/src/**` is a **stale
   leaked tree** and is not the oracle. Every oracle claim — including a
   refutation of mine — must cite the 2.1.251 binary (offset or literal count).
   Round 9's §25a refutation was correct but rested on that stale tree.
2. **The changelog is navigation, not evidence.** §27c is in this brief
   precisely because a changelog line was accepted as an oracle fact.
3. **Controls on negatives.** Any `<none>` you assert or overturn must come from
   a batch that also contains a known source-code hit. Use
   `mcp/src/config_diagnostics.rs` and `agent/src/mcp_servers.rs` as the positive
   control for `"reserved MCP server name"`; do not rely on a repo-wide count,
   because these audit documents also contain the phrase. Round 3 shipped an
   entire batch of false zeros from the wrong working directory.
4. **Name-based greps have a positional blind spot.** §24b was first written as
   "no caller supplies it" because `git grep agent_mcp_tools` cannot see a
   positional `&[]`. Check call sites by arity, not just by name.
5. **Hunt inversions specifically.** All four of the author's worst errors were
   the same shape: right components, wrong direction — migration direction,
   security consequence, schema layer, fallback presence. For each claim of the
   form "A causes B", reconstruct the data flow before agreeing.
6. **Do not re-file anything in §0's three retraction tables** (20 + 14 + 6
   entries), and do not re-audit §26c / §25e / §24f, which record verified-clean
   results deliberately so later rounds skip them.

## What is already settled — leave it alone

Confirmed by two or more independent readers and not in scope here: §1, §2
(per-tool permission policy and the org ceiling), §25c (confusable-URL guard),
§26a (resource templates), §24a (unwired blocklist), §25b (superseded
`approval.rs`), §4, §17, §20c, and the §15 severity downgrade.

## After this round

R10 is complete and paper iteration is closed. The main document incorporates
the corrections, explicitly classifies the formerly-open §21 items, and freezes
an implementation order that separates reachable parity work from feature
completion and dead-code cleanup. Re-open review only when new runtime evidence
contradicts a recorded conclusion.
