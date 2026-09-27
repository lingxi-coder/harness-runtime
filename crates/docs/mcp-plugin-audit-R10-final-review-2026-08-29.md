# R10 final review — Rust MCP/plugin alignment vs Claude Code 2.1.251

**Date:** 2026-08-29
**Status:** APPROVED FOR IMPLEMENTATION (with the two post-sign-off corrections in §6.1)
**Oracle:** `~/.local/share/claude/versions/2.1.251`
**Oracle build:** `37534ac596d80cefb02d272f036adba4ba055d2c`
**Primary artifact:** `docs/mcp-plugin-byte-alignment-2.1.251-2026-08-28.md`
**Review brief:** `docs/mcp-plugin-audit-R10-review-brief.md`

---

## 1. Executive verdict

R10 was a targeted second-reader audit of the ten claims introduced or rewritten
in round 9. It did not repeat the repository-wide sweep.

Final outcome:

- **8 claims upheld without a material change**;
- **§26b upheld with a narrower 401 precondition** and, post-sign-off, the
  oracle's `!refreshToken` gate on the proactive window plus a fourth delta
  (§4.10, §6.1);
- **§27c retracted entirely** (post-sign-off correction: the surviving "narrow
  redaction delta" was a misread of a telemetry label — see §4.3 and §6.1);
- **no new breadth audit is required**;
- **paper iteration is closed** after applying the corrections recorded below.

The main audit is now suitable to drive implementation. “Approved for
implementation” means the documented conclusions are internally consistent and
evidence-backed; it does not mean the Rust gaps have been fixed.

---

## 2. Scope and provenance rules

### 2.1 In scope

R10 reviewed exactly these items:

1. §27a — `--mcp-config` scope and project approval;
2. §27b — MCP `requiresUserInteraction` propagation;
3. §27c — MCP server-name output handling;
4. §27d — `additionalDirectories` scope decision;
5. §19 — static/helper authorization versus OAuth;
6. §23a — `allowManagedMcpServersOnly` direction;
7. §23b — MCP config-file shape/size guards;
8. §24b — per-subagent inline `mcpServers`;
9. §25a — installed-plugins migration retraction;
10. §26b — XAA refresh and 401 recovery.

The formerly-open §21 items were then classified so that the document could
stop carrying ambiguous work.

### 2.2 Oracle rule

All Oracle conclusions in this report come from the exact 2.1.251 Mach-O named
above. `claude-code/src/**` was treated as stale and was not used as authority.

The published changelog was used only as navigation. A changelog sentence was
not accepted as proof when the binary contradicted or failed to demonstrate the
claimed runtime behaviour.

### 2.3 Negative-search controls

Rust negative greps were paired with known source-code hits. The stable positive
control is `"reserved MCP server name"` in:

- `mcp/src/config_diagnostics.rs`;
- `agent/src/mcp_servers.rs`.

Repo-wide hit counts are deliberately not used: the audit documents themselves
now contain that phrase and would contaminate the count.

---

## 3. R10 verdict matrix

| Item | Final verdict | Required document action |
| --- | --- | --- |
| §27a | **UPHELD** | Keep as confirmed behavioural gap. |
| §27b | **UPHELD** | Correct the wording: Rust reads some Anthropic `_meta` keys, but not `anthropic/requiresUserInteraction`. |
| §27c | **RETRACTED ENTIRELY** | Remove the `mcp add` redaction delta too; Rust's user-facing copy already matches. Residue is a telemetry label under §20b. |
| §27d | **UPHELD** | Keep `additionalDirectories` null-byte handling explicitly out of scope. |
| §19 | **UPHELD** | Keep all three authorization/helper/OAuth findings. |
| §23a | **UPHELD** | Keep “unconditionally managed-only / over-strict”; remove every remnant of the old permissive-security claim. |
| §23b | **UPHELD** | Keep ambient-read exposure; Oracle's limit is confirmed on non-dynamic scoped reads. |
| §24b | **UPHELD** | Keep “unimplemented end to end”; do not claim a live leak. |
| §25a | **RETRACTION UPHELD** | Keep `_v2.json → installed_plugins.json`; retain dead `added` shape only under duplicate install-path cleanup. |
| §26b | **UPHELD, NARROWED** | State that the 401 misroute requires a locally unexpired but server-rejected cached token; gate the proactive window on `!refreshToken`; add the unused-refresh-token delta. |

---

## 4. Detailed evidence

### 4.1 §27a — `--mcp-config` is incorrectly project-scoped

**Verdict:** confirmed gap.

Rust flow:

1. `apps/cli/src/init.rs:398` parses every explicit `--mcp-config` entry with
   `ConfigScope::Project`.
2. `apps/engine-desktop/src/lib.rs:7114-7119` merges those entries into the
   runtime MCP list.
3. `apps/engine-desktop/src/lib.rs:7128` calls
   `apply_project_server_gate` after that merge.
4. `mcp/src/server_gate.rs:119-133` blocks any unapproved `Project` entry as
   `ProjectPendingApproval`.

There is no earlier exemption for flag-supplied entries. The composition-root
comment even states that explicitly supplied servers are gated.

Oracle evidence:

- the `--mcp-config` handler stamps `let Tl={...ws,scope:"dynamic"}` at
  approximately @166780013.

**Failure mode:** an explicitly supplied server can be disabled as a pending
project server even though the normal `.mcp.json` approval UI does not own that
flag entry.

**Implementation direction:** represent CLI entries as `Dynamic`, then verify
every scope switch and approval helper rather than changing only the parser.

### 4.2 §27b — MCP `requiresUserInteraction` is dropped

**Verdict:** confirmed gap.

Oracle evidence from one MCP-tool factory:

- `_meta["anthropic/requiresUserInteraction"] === true`: @182519150;
- `requiresUserInteraction(){return Ee}`: @182520425;
- `suppressesAlwaysAllowRule:()=>Ee||...`: @182520462;
- an `ask` result with `suppressAlwaysAllowRule: true`: @182520945.

This is a direct chain, not an inference from the changelog.

Rust flow:

- `mcp/src/client.rs:1559-1567` parses `anthropic/searchHint` and
  `anthropic/alwaysLoad`, but has no `anthropic/requiresUserInteraction` field;
- `platform-api/src/mcp.rs:217-239` has no DTO slot for the bit;
- `tools/mcp/src/mcp_tool.rs:780+` does not override the `Tool` trait's default
  `requires_user_interaction() -> false`;
- no `suppress_always_allow_rule` channel exists;
- `tui/src/bottom_pane/permission_view.rs:125-129` renders
  `"Yes, allow always"` unconditionally.

**Failure mode:** the UI can persist an allow rule for a tool that requires
fresh interaction and will ignore that persistent grant.

### 4.3 §27c — CLI sanitisation claim retracted in full

**Verdict:** broad parity claim false; the narrow delta kept at sign-off was
also false (corrected 2026-08-29, §6.1).

Oracle counter-evidence:

- `CYt` / `Unt` directly interpolate `No MCP server named "${r}"` at
  @179943206;
- production remove/get/login/logout paths call those helpers or render the raw
  name at @180019468, @180023358, @179944649, @179958365 and @179960984.

Therefore Rust's general raw-name CLI output is not, by itself, a 2.1.251 parity
gap.

Why the "narrow surviving difference" was withdrawn:

- The sign-off read the `<redacted>` constants (`phr` / `fhr`, @160896747) as
  public copy. The binary's `addMcpServer` (`XL`, @160897062) actually throws
  `ft(Error("Invalid name ${e}. Names can only contain …"), phr)` and
  `ft(Error("Cannot add MCP server \"${e}\": this name is reserved."), fhr)`:
  the **raw name is in the thrown `Error`**, and the redacted string is the
  wrapper's second argument.
- Every sibling `ft(Error(detailed), "short label")` site in the binary pairs a
  detailed message with a stable PII-free label (@159245817, @159485640,
  @160707905) — the shape of a telemetry/error-grouping tag, not user copy.
  `ft`'s definition is chunk-local and was not recovered; the classification
  rests on the call-site shape plus the unambiguous raw-name template.
- Rust's `validate_mcp_server_name` (`apps/cli/src/commands/mcp.rs:1519-1531`)
  already emits the same two raw-name templates. Redacting them would create a
  divergence.

**Classification:** no user-facing gap. The absent sanitized telemetry label is
tracked with §20b.

### 4.4 §27d — `additionalDirectories` null byte is out of scope

**Verdict:** scope decision upheld.

The Rust behaviour is a real 2.1.251-wide gap: `permission/src/loader.rs`
converts raw entries to `PathBuf` without skipping null bytes. It is nevertheless
owned by the permission/settings ingestion subsystem, not `mcp/` or `plugin/`.

It remains recorded as an explicit non-goal so it is not repeatedly rediscovered
inside this audit.

### 4.5 §19 — static/helper authorization versus OAuth

**Verdict:** all three gaps upheld.

Oracle flow near @182284000–@182295907:

- `hasUserAuthHeader` is derived before transport OAuth setup;
- `helperMintsAuthHeader` is true only when the helper output actually contains
  Authorization;
- either condition suppresses construction of the OAuth provider;
- `bo(...)` classifies rejection as `AUTH_HEADER_REJECTED` or
  `HEADERS_HELPER_AUTH_REJECTED` before generic OAuth recovery.

Rust differences:

1. `mcp/src/registry.rs:2402-2426` inserts a bearer over an existing static
   `Authorization` value.
2. `mcp/src/headers_helper.rs:15-17` tests helper presence, not whether the
   helper minted Authorization.
3. `mcp/src/registry.rs:901-915` runs the helper first and OAuth resolution
   second, so OAuth can overwrite the helper result.
4. The OAuth error arm precedes the helper retry arm at
   `mcp/src/registry.rs:929-953`.

### 4.6 §23a — the allowlist is unconditionally managed-only

**Verdict:** confirmed over-strict compatibility gap, not an authorization hole.

Oracle evidence:

- `ghr()` reads the managed `allowManagedMcpServersOnly` bit;
- `uAn()` returns policy-only settings when true and merged effective settings
  otherwise, around @160892000–@160894000.

Rust evidence:

- `mcp/src/enterprise_policy.rs:135-179` merges denies from ordinary and managed
  tiers but reads allows only from managed tiers;
- its unit test explicitly asserts that an ordinary-tier allow is ignored;
- no second ordinary-tier allowlist consumer was found.

**Correct consequence:** when managed-only mode is absent/false, LingXi can
reject an ordinary-tier allowlist that Claude Code would honour.

### 4.7 §23b — ambient MCP config reads lack Oracle guards

**Verdict:** confirmed gap.

Oracle `Iqe` at approximately @160910600:

- defines a 2 MiB limit;
- uses the regular-file/limited reader for every non-`dynamic` scope;
- emits `mcp_config_shape_gate`, missing/read/JSON diagnostics and structured
  suggestions.

Rust evidence:

- `mcp/src/json_config.rs:510` reads the global config directly;
- `mcp/src/json_config.rs:529` reads project `.mcp.json` directly;
- `mcp/src/config_diagnostics.rs:460-461` also uses unbounded
  `read_to_string` before parsing;
- `--mcp-config` itself does call `Path::is_file()`, so the earlier FIFO example
  on that flag remains correctly retracted.

**Failure mode:** an ambient path can block on a FIFO/device, consume excessive
memory, or fail without Oracle-equivalent typed diagnostics.

### 4.8 §24b — per-subagent inline MCP is unimplemented

**Verdict:** confirmed feature-completeness gap; no current leak demonstrated.

Evidence:

- `AgentToolResolver::resolve` accepts agent-specific MCP tools but the shared
  production wrapper passes a literal empty slice in
  `agent/src/tool_resolver.rs:360`;
- main-agent frontmatter is merged only at startup in
  `apps/engine-desktop/src/lib.rs:4679-4710` and `:7302+`;
- subagent and teammate production paths share `resolve_subagent_tools`;
- `mcp/src/agent_scope.rs` and the registry's `agent_scoped` field have no live
  ownership path.

No second connect/inject route was found. Since no per-subagent connection is
created, a connection leak cannot be claimed; the missing work is the complete
connect → tool injection → teardown ownership chain.

### 4.9 §25a — installed-plugins retraction is correct

**Verdict:** retraction upheld from the binary.

Oracle evidence near @162709040:

- `uK()` returns `installed_plugins.json`;
- `Kor()` returns `installed_plugins_v2.json`;
- `MBt` executes `renameSync(Kor(), uK())`;
- the log says `Renamed installed_plugins_v2.json to installed_plugins.json`;
- V1 data in `installed_plugins.json` is converted to V2 in place.

Rust production installation already writes `installed_plugins.json` with
`installPath`, `installedAt` and `lastUpdated` in
`apps/cli/src/commands/plugin_install.rs`.

The dead alternate `plugin/src/installed.rs` still uses `{version, added}`; that
is duplicate-path cleanup under §25d, not a live cross-compatibility finding.

### 4.10 §26b — XAA refresh and rejected cached tokens

**Verdict:** confirmed with a narrowed 401 condition.

What Rust already does:

- every XAA resolve enters `resolve_xaa_token`;
- a missing or expired token is silently re-exchanged;
- optional refresh tokens are persisted.

Confirmed gaps:

1. Oracle re-exchanges within a 300-second expiry window **only when no refresh
   token is stored** (`!n?.refreshToken && (!n?.accessToken ||
   (n.expiresAt-Date.now())/1000<=300)`, @182213696); Rust waits for actual
   expiry. The `!refreshToken` gate was missing from the sign-off wording and
   must be part of the implementation.
2. Oracle shares `_refreshInProgress`; Rust has no XAA single-flight guard.
3. If a locally unexpired cached token is rejected by the server, Rust's 401 arm
   calls generic `reauth_oauth_spec`, which has no XAA branch and can enter
   ordinary refresh/interactive consent.

Key evidence:

- Oracle proactive/single-flight region: approximately @154915370;
- Rust XAA selection: `mcp/src/registry.rs:1152-1157`;
- Rust cached-token check: `mcp/src/registry.rs:1332-1337`;
- Rust generic 401 retry: `mcp/src/registry.rs:929-939` and `:1216-1261`.

4. (Added 2026-08-29.) Rust never uses a persisted XAA refresh token:
   `resolve_xaa_token` (`mcp/src/registry.rs:1332-1337`) reuses an unexpired
   token and otherwise re-runs the full exchange, whereas the oracle's
   `!refreshToken` gate routes a stored refresh token through ordinary refresh
   first.

No dedicated regression test currently exercises the XAA-specific 401
misroute.

---

## 5. Final disposition of §21

| Item | Final classification | Decision |
| --- | --- | --- |
| Startup `N MCP servers need authentication` | **Explicitly deferred** | Exact sentence found only in embedded changelog; no runtime producer recovered. Not counted. |
| `listMcpResources` | **Closed — not a gap** | Implemented and registered. |
| JSON-Schema normalization | **Closed — confirmed gap** | Tracked in §20a. |
| Failed-server ToolSearch note | **Closed — confirmed partial-port gap** | Oracle flag defaults true; Rust implementation defaults false. |
| strict config / requires interaction | **Closed — confirmed gaps** | Tracked in §27a/§27b. |
| claude.ai connector heading/scope | **Closed — accepted divergence** | Covered by the §0 exclusion. |
| Managed-settings startup-mode approval exemptions | **Explicitly deferred / out of scope** | Exact env family not recovered; owner is managed-settings approval. |
| MCPB / `.dxt` | **Closed — confirmed gap** | Tracked in §14. |
| Versionless update cache replacement | **Closed — confirmed gap** | Rust deletes the shared `unknown` directory; Oracle defers when live. |

### 5.1 Failed-server note evidence

- Oracle: `function VKe(){return I("tengu_surface_failed_mcp_servers",!0)}` at
  @156292983; ToolSearch supplies failed servers when enabled at @158539909.
- Rust: `tools/meta/src/tool_search.rs:655-660` uses
  `telemetry::flag_bool(..., false)`.

### 5.2 Versionless cache evidence

- Rust: `apps/cli/src/commands/plugin_install.rs:1840-1851` skips the fast path
  for `unknown` and calls `remove_dir_all(dest)`.
- Oracle: the `ice` materializer at @162795186 checks live use and logs
  `deferring overwrite until it exits` instead of deleting.

---

## 6. Corrections applied during finalisation

The primary audit document was updated to:

- add a v10 final-signoff header;
- replace the contaminated repo-wide grep-control count with source-scoped
  controls;
- classify every §21 item;
- narrow the XAA 401 precondition;
- document the full `requiresUserInteraction → suppressAlwaysAllowRule` chain;
- retract broad §27c and retain only `mcp add` redaction (superseded by §6.1);
- remove the reintroduced §23a direction inversion;
- move dead local scaffolding out of the top remediation slots;
- split directly reachable work from feature completion and cleanup;
- remove duplicated tail wording.

### 6.1 Post-sign-off corrections (second binary read, 2026-08-29)

Two claims in the signed-off text were re-derived from the Mach-O and
corrected in both this report and the primary audit (v10.1):

1. **§27c — retracted entirely.** The `mcp add` "redaction delta" mistook the
   second argument of the oracle's `ft(Error(raw), label)` wrapper for public
   copy; the thrown message carries the raw name, exactly as Rust does. Phase C
   item 23 is now a no-op for user-facing copy.
2. **§26b — precondition and fourth delta added.** The proactive 300-second
   window is gated on `!n?.refreshToken` (@182213696); Rust additionally never
   uses a persisted XAA refresh token (`registry.rs:1332-1337`).

The R10 brief was updated to:

- remove its stale line count;
- say that non-target items were out of scope, not falsely “closed”;
- record the completed verdict;
- fix its grep-control instructions;
- point to this final evidence ledger.

---

## 7. Frozen implementation sequence

### Phase A — authorization and directly reachable behaviour

1. §1 + §2 — per-tool policy and administrative ceiling.
2. §19 — static/helper Authorization precedence.
3. §27b — `requiresUserInteraction` and persistent-allow suppression.
4. §25c — confusable URL guard.
5. §27a — dynamic scope for explicit MCP configs.
6. §23b — ambient config shape/size guards.
7. §21.9 — live-safe versionless cache replacement.
8. §24c — RFC 9728 `resource_metadata`.
9. §20a — tool-schema normalization.
10. §5 + §6 — BOM and manifest command shapes.

### Phase B — protocol and feature completeness

11. §26a + §26b — resource templates and XAA refresh/retry (proactive window
    gated on `!refreshToken`; refresh-first when a token is stored).
12. §15 — archive host/IP/digest checks.
13. §7 + §8 — policy/name validation.
14. §12 + §9–§11 — parser/transports/discovery cache.
15. §3 + §4 — host-owned and skipped plugin MCP discovery.
16. §13 + §25d — canonical install path and source unions.
17. §17 + §18 — negotiation and connection error classification.
18. §14 + §16 + §24d — manifest/MCPB and command-source completion.
19. §21.4 — default-on failed-server ToolSearch note.
20. §24b — per-subagent inline MCP ownership.

### Phase C — compatibility polish and cleanup

21. §23a — optional managed-only allowlist semantics.
22. §24a + §25b — dead local blocklist/approval cleanup.
23. §27c — no user-facing change (retracted §6.1); sanitized telemetry labels
    ship with item 24 / §20b.
24. §20b + §20c + §22 — telemetry and exact copy.

---

## 8. Verification performed

The finalisation pass verifies:

- the priority list is continuous and has no duplicate numbers;
- every `§N` cross-reference resolves to an existing numbered section or item;
- the version history is ordered newest to oldest;
- no active text repeats the retracted §23a security consequence;
- broad §27c language no longer appears as an implementation requirement;
- the R10 brief no longer carries a stale line count or contaminated exact
  positive-control count;
- Markdown structure has no malformed table or heading sequence;
- only documentation files changed.

No Rust build or runtime test is claimed: this delivery changes documentation,
and its behavioural evidence comes from static Rust call-chain inspection and
the pinned 2.1.251 Mach-O.

---

## 9. Residual risks and stop rule

Two questions are deliberately deferred:

1. the generic startup authentication-warning producer;
2. the exact managed-settings MCP startup-mode environment-variable family.

Neither is counted as a confirmed implementation item. Reopen them only with a
runtime reproduction or a binary producer/control-flow region—not with a
changelog sentence alone.

The audit should otherwise remain closed. A new review round is justified only
if:

- implementation work exposes contradictory runtime behaviour;
- the Oracle binary/version changes;
- a new source layer or production caller invalidates the recorded call graph;
- a deferred item receives direct evidence.

Absent one of those triggers, the next step is implementation and regression
testing, not another paper audit.
