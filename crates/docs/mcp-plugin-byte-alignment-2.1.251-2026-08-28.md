# Rust `mcp/` + `plugin/` — byte-level alignment vs Claude Code 2.1.251

> **Round 10 is complete.** The scope and attack rules are preserved in
> `docs/mcp-plugin-audit-R10-review-brief.md`; the detailed verdict and evidence
> ledger are in `docs/mcp-plugin-audit-R10-final-review-2026-08-29.md`.
> Paper iteration is closed after this revision: future work should implement
> the settled findings, not start another breadth audit, unless new runtime
> evidence contradicts a recorded conclusion.

**v10.1 — two post-sign-off corrections from a second binary read (2026-08-29).**
(1) **§27c is now retracted entirely.** The `<redacted>` strings at @160896747
are the *second* argument to the oracle's `ft(Error(raw), label)` wrapper — the
thrown `Error` carries the raw name (`Invalid name ${e}. …`, @160897062), and
every sibling `ft(Error(…), "…")` site pairs a detailed message with a short
stable telemetry label. Rust's user-facing copy already matches byte for byte;
the only residue is the missing sanitized telemetry label, folded into §20b.
(2) **§26b item 1 gains the oracle's `!refreshToken` precondition** (the XAA
silent exchange fires only when no refresh token is stored, @182213696) and a
fourth delta: Rust re-runs the XAA exchange on expiry even when it persisted a
refresh token, where the oracle refreshes.

**v10 — final targeted Codex sign-off (APPROVED FOR IMPLEMENTATION).** The ten
round-9 claims were re-derived from the 2.1.251 Mach-O. Eight stand in substance,
§26b stands with a narrower 401 precondition, and broad §27c is retracted in
favour of one binary-proven `mcp add` redaction delta. §21 is no longer an open
bucket: two items are confirmed gaps, one is an accepted divergence, and two are
explicitly deferred. The implementation order now separates reachable parity
work from feature completion and dead-code cleanup.

**v9 — after a third Codex review (REQUEST CHANGES).** All eight of its findings
verified and applied: **§25a fully retracted** (I had the oracle's migration
direction backwards), **§23a inverted**, **§19 and §26b overstated**, **§23b's
example invalid**, **§24b understated**, three open questions closed as confirmed
gaps, and one new finding (§27). ⚠️ Codex's §25a refutation cited
`claude-code/src/*.ts` — that tree is **stale**; the retraction below is
re-derived from the 2.1.251 binary, which agrees with it.

**v8 — coverage is complete.** The final three thin spots (`xaa.rs` /
`xaa_idp.rs`, `oauth/callback.rs`, `mcp_output_storage.rs`) are compared. One
**new functional gap** — MCP resource templates are entirely unsupported — plus
a self-declared XAA residual promoted out of a module comment into a finding
(§26). Nothing in `mcp/` or `plugin/` is now unexamined.

**v7 — the last 14 surface-only files are now line-compared.** Result: **1
cross-compat divergence on a shared on-disk file, 1 more dead subsystem, 1
missing policy guard, 1 duplicate install path**, and a clean bill for the rest
(§25). No file in `mcp/` or `plugin/` remains uncompared.

**v6 — the 26 previously unswept files have now been swept.** Five read in
full, all public functions wiring-checked, the high-value ones content-compared,
and the OAuth/XAA cluster probed by feature. Result: **2 dead subsystems, 2 new
gaps, 1 correction** (§24), plus a list of what came back clean.

**v5 — after a Claude self-review pass.** No new retractions: every oracle
identifier asserted in this document (195 backticked tokens) was bulk-counted
against the binary, and the 27 zero-count tokens are all port-side Rust names or
items already marked retracted. Two call-site-only claims (§3, §1) were traced to
their callees and hold. **Two omissions found and added as §23.**

**v4 — after a second Codex review (REQUEST CHANGES).** Round 1 = Claude,
round 2 = Codex (6 retractions), round 3 = Claude (new connect-runtime cluster),
round 4 = Codex (**8 more corrections, all verified and applied**). Read §0
first: it states what is proven, what is assumed, and what has not been swept.

- Oracle: `~/.local/share/claude/versions/2.1.251`, build
  `37534ac596d80cefb02d272f036adba4ba055d2c`, 2026-08-28T14:51:38Z.
- Port: HEAD `5448e93e3`; last mcp/plugin alignment `24e91f6ab` (2.1.246).
- Byte offsets below are into the Mach-O read as latin-1.

---

## §0 Audit instructions and known limits

### How to attack this document

1. **Every `<none>` claim needs a control.** All negative greps in round 3 were
   re-run from `lingxi-code/` with a positive source-code control in the same
   batch. Use the known hits in `mcp/src/config_diagnostics.rs` and
   `agent/src/mcp_servers.rs`; do not use a repo-wide exact hit count, because
   the audit documents themselves now contain the control phrase. Round 1 and 2
   negatives were **not** all control-guarded. A batch whose source control is
   empty proves nothing — round 3 caught one such false-zero batch.
2. **Check the schema layer before checking the field.** The oracle has five
   layers that reuse field names and are not interchangeable: on-disk
   `.mcp.json`; host `mcp_set_servers`; plugin manifest; marketplace
   **registration** source; marketplace **plugin-entry** source. Round 1's
   biggest error was comparing (e) against (d).
3. **A grep hit is not a producer.** Round 1 called the startup auth warning
   "ported" on the strength of a hit that turned out to be a *tool description*
   string. For every "present" claim, confirm the hit is on the emission path.
4. **Bulk-count every oracle literal you assert.** Round 4 caught one
   fabricated errorCode (`NEEDS_AUTH`, count 0) that was inferred from its
   siblings rather than checked. Round 5 re-counted all 195 asserted tokens; do
   the same for anything you add.
5. **Confidence labels** — `[CODE]` = a Rust code path was read;
   `[SCHEMA]` = an oracle schema/control-flow region was extracted and compared;
   `[GREP]` = control-guarded string absence only, behaviour not traced.
   Downgrade anything you cannot reproduce.

### Coverage

Swept as of v6. `mcp/src/` and `plugin/src/` are fully accounted for; the
depth varies:

- **Read in full**: `capabilities.rs`, `raw_conn.rs`, `agent_scope.rs`,
  `trust.rs`, `blocklist.rs`, `strict_policy.rs`, plus the files cited in
  §1–§23.
- **Wiring-checked** (every `pub fn` searched for an external caller):
  all 26 formerly-unswept files. Two dead subsystems found — §24a, §24b.
- **Content-compared against the oracle**: `initialize_params.rs`,
  `identity.rs`, `strict_policy.rs`, `transform_result.rs`,
  `mcp/src/oauth.rs` + `xaa*.rs` (by feature probe, not line-by-line).
- **Line-compared in v7**: `env_expansion.rs`, `mcp_output_storage.rs`,
  `hook_dispatch.rs`, `approval.rs`, `oauth/callback.rs`,
  `plugin/src/{manager,loader,dependency,lifecycle,installed,git,agent_validation}.rs`,
  `tools/mcp/src/{auto_background,large_output}.rs` — see §25.

- **Compared in v8**: `xaa.rs` + `xaa_idp.rs`, `oauth/callback.rs`,
  `mcp_output_storage.rs` — see §26.

Coverage is complete: every file in `mcp/` and `plugin/` has been compared
against 2.1.251 at least at the contract level.

### Retracted round-1 claims (do not resurrect)

| claim | why it was wrong |
| --- | --- |
| Marketplace source fields compared against `MarketplaceExternalSource` | Wrong layer — see §0.2 and §13. |
| `keywords`/`license`/`repository` absent | Parsed at `plugin/src/discovery.rs:129-137`; only dropped later. |
| `$schema` missing | Oracle says "ignored at load time" — not a gap. |
| `evalsDir` / `monitorsPath` | Invented names; real fields are `experimental.evals` and `monitors`. |
| 3 missing `/plugin` strings | Present at `plugin_init.rs:421`, `plugin_install.rs:620-629`, `enterprise_policy.rs:667,1041`. |
| `role` on every transport | Not on `sdk`, not on `claudeai-proxy`. |
| `org_max_permission` an on-disk sibling field | Only on the `mcp_set_servers` control form. |
| Startup auth warning "ported" | The hit was a tool-description string. Re-opened, §21. |

Retracted in **round 4** (Codex review; each re-verified here):

| round-3 claim | why it was wrong |
| --- | --- |
| §15 "no scheme check" | **False.** `apps/cli/src/startup_resources.rs:100` rejects a non-`https` initial URL and `:73-74` rejects non-HTTPS redirects (`:75-84` also refuses cross-origin redirects for credentialed downloads). The `http://169.254.169.254/…` example is unreachable. §15 rewritten and downgraded. |
| §18 `NEEDS_AUTH` errorCode | **Fabricated.** Exact-literal count in the 2.1.251 binary is **0** (`CONNECT_TIMEOUT` 16, `AUTH_HEADER_REJECTED` 8, `HEADERS_HELPER_AUTH_REJECTED` 10, `FIRST_PARTY_AUTH_REJECTED` 8). Upstream returns `{type:"needs-auth"}`, which the port already models as `McpActionState::NeedsAuth`. Removed. |
| §18 implying the connect timeout is missing | **False.** `mcp/src/registry.rs:888-918,2219-2222` bounds connect+initialize by `MCP_TIMEOUT \|\| 30000`. Only the classification, gate/retry, and telemetry are missing. |
| §8 "none of these strings exist" | **False.** `Plugin name cannot be empty` and `… cannot contain spaces. Use kebab-case …` are at `apps/cli/src/commands/plugin_tag.rs:95,98`; the path-separator rejection at `plugin_init.rs:363`. My own round-3 probe had already printed `plugin_tag.rs` and I misread it — the same failure as round 1. §8 rewritten. |
| §20 heading vs body | Self-contradictory (heading asserted absence, body said "not traced"). Now **resolved**: normalization is confirmed absent, and the finding is promoted from telemetry to behaviour (§20a). |
| §21 item 2 `listMcpResources` | **False.** `ListMcpResourcesTool` is defined, implemented, and registered behind the `resources` capability (`tools/mcp/src/mcp_tool.rs:54`, `tools/mcp/src/lib.rs:30,38`), with a legacy alias at `llm-runtime/src/convert.rs:292`. Item deleted. |
| §21 item 9 "no in-repo caller" | **Literally false.** `copy_into_cache` has three callers (`plugin/src/manager.rs:301,332,375`). The accurate statement is that `PluginManager::install` has no production caller. Rewritten. |
| §19 "correct for the helper case" | **Overstated.** See §19 — the port keys on helper *presence*, and resolves the helper *before* OAuth bearer injection, so an OAuth bearer can overwrite a helper-minted `Authorization`. |

Retracted in **round 9** (Codex review; each re-derived from the binary here):

| round-7/8 claim | why it was wrong |
| --- | --- |
| §25a — the port writes V2 into the V1 filename | **Backwards.** The binary's `MBt` migration does `renameSync(installed_plugins_v2.json → installed_plugins.json)` and logs `Renamed installed_plugins_v2.json to installed_plugins.json`; `installed_plugins.json` is the **current** file holding the V2 shape, and `_v2.json` is the legacy interim name being retired. Production Rust (`apps/cli/src/commands/plugin_install.rs:12-14,46`) already writes `installed_plugins.json` with `{scope, installPath, version, installedAt, lastUpdated}` — correct name, correct fields. Only the **dead** `plugin/src/installed.rs` uses `added`; folded into §25d. Section deleted. |
| §23a — "a user-scope `allowedMcpServers` can widen an admin's allowlist" | **Inverted.** `mcp/src/enterprise_policy.rs:133-139` reads deny from `ordinary.chain(managed)` and accepts allow entries **only from managed policy**. The port is unconditionally managed-only — over-strict, not under-strict. Rewritten. |
| §19 — "a static `Authorization` with no `oauth` block still takes a fallback" | **False.** `resolve_oauth_spec` (`registry.rs:1140-1145`) returns the spec unchanged when there is no `oauth` block, so there is no fallback to take. The real issues survive; rewritten. |
| §26b — "no silent recovery at all" / "the chain yields no refresh token" | **Overstated.** `registry.rs:1152-1157` re-runs `resolve_xaa_token` on every resolve, so a missing or expired token *is* silently re-exchanged, and refresh tokens are optional and persisted. Rewritten around the real gaps. |
| §23b — the `--mcp-config` FIFO/device example | **Invalid.** `apps/cli/src/init.rs:381` gates that path on `Path::is_file()`. The unguarded reads are elsewhere; rewritten. |
| §24b — "dead code; possible connection leak" | **Understated.** The whole inline per-subagent `mcpServers` chain is unimplemented, so no connection is ever created and no leak can be claimed. Rewritten. |

### Accepted divergence — do not file

`claudeai-proxy` transport and the claude.ai connector surface (Anthropic
backend/remote, user-confirmed out of scope). `mcp/src/json_config.rs:333`
deliberately folds it onto `Http`.

---

## §1–§8 · P1 — permission, loading, policy

### 1. `tools[].permission_policy` is never derived `[CODE][SCHEMA]`

Oracle `LXe` @154729556 walks every **`dynamic`-scope** `http`/`sse` server,
expands `tools[]` to `mcp__<server>__<tool>`, resolves duplicates
**strictest-wins** (`always_allow` 0 < `always_ask` 1 < `always_deny` 2); `tir`
merges the three buckets into the session allow/deny/ask rules.

Port: no `tools` field on `McpJsonEntry` (`mcp/src/json_config.rs:71-114`); no
slot on `McpServerConfig` (`mcp/src/connection.rs:14-52`); the consumer rule
source is present but marked `No producer yet — latent`
(`permission/src/rule.rs:188-190`). An `always_deny` never reaches authorization.

### 2. `toolPermissions` (`allow`|`ask`|`blocked`) is dropped `[CODE][SCHEMA]`

On-disk `sse`/`http`: `toolPermissions: record(string, ["allow","ask","blocked"])`.
Oracle attaches the match as `mcpInfo.effectiveMaxPermission` at tool discovery
and warns `toolPermissions has N entries but none matched upstream tool names —
backend name drift?`. The host-control twin is `tools[].org_max_permission`
("Org admin's per-tool ceiling. Drives the auto-mode `isOrgAskCeiling` gate so an
admin 'ask' cap forces a user prompt even in auto mode"). Port: no parser,
storage, diagnostic, or authorization path.

### 3. `skipMcpDiscovery` not modelled `[CODE][SCHEMA]`

Oracle `OL` @160861000: `if (e.skipMcpDiscovery) return {}` — suppresses both
`.mcp.json` and manifest `mcpServers`, leaving other components enabled. Port
`detect_components` calls both loaders unconditionally
(`plugin/src/discovery.rs:737-782`); the bit exists in neither `RawManifest` nor
the load model. An SDK host opens the plugin's MCP connections twice.

### 4. `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` / `_EXCEPT` absent `[GREP][SCHEMA]`

Oracle order: `isBuiltin` returns first; skip-env suppresses discovery; `_EXCEPT`
is comma-split; an entry containing `@` matches `repository`, otherwise it
matches `name` **only for non-directory-loaded plugins**. None of it exists.

### 5. BOM-prefixed `plugin.json` drops the plugin `[CODE]`

`plugin/src/discovery.rs:627-642` passes the unmodified string to
`serde_json::from_str`; `serde_json` does not treat U+FEFF as whitespace, so the
plugin is skipped after `skipping plugin with malformed plugin.json`. Repo
convention already exists in `command-api/src/markdown_loader.rs`. (2.1.246 fix.)

### 6. An object-form `commands` map deletes the **whole plugin** `[CODE][SCHEMA]`

Oracle `Rs`: `commands: union([path, path[], record(string, Ds)])`,
`Ds = {source?, content?, description?, argumentHint?, model?, allowedTools?}`
refined by *"Command must have either `source` (file path) or `content` (inline
markdown), but not both"*. Port `PathDecl` is `untagged {One(String),
Many(Vec<String>)}` (`plugin/src/discovery.rs:144-158`); an object map fails
deserialization, and because `RawManifest` decodes as a whole,
`load_plugin_from_path` returns `None` — the plugin vanishes entirely.
Same class: mixed arrays in `hooks`/`mcpServers`/`lspServers` (oracle allows
`union([path, inlineObject])[]`); `load_declared_json_records` special-cases only
all-string arrays (`plugin/src/discovery.rs:1195-1200`).

### 7. Managed marketplace patterns: glob where the oracle uses regex `[CODE][SCHEMA]`

Oracle: `hostPattern`/`pathPattern` are JS `RegExp` (with `Invalid pathPattern
regex in policy settings strictKnownMarketplaces` diagnostics); `pathPattern`
applies to the `.path` of `file`/`directory` sources; `owner/*` is honoured only
inside `strictKnownMarketplaces`/`blockedMarketplaces` ("Everywhere else … a
wildcard is taken literally and fails to clone").
Port: a case-insensitive `*`/`?` glob (`apps/cli/src/commands/plugin_policy.rs:321-341`)
applied to every source family via `source.host_path()` (`:182-198`); `owner/*`
parses as an exact repo. The oracle's own example `^github\.mycompany\.com$`
matches nothing under a glob.

### 8. Name validation exists only on the authoring path `[CODE][GREP]`

**Corrected in round 4.** Part of the rejection gate *is* ported, but only in the
plugin-authoring commands:

- `apps/cli/src/commands/plugin_tag.rs:95,98` — `Plugin name cannot be empty`,
  `Plugin name cannot contain spaces. Use kebab-case (e.g., "my-plugin")`.
- `apps/cli/src/commands/plugin_init.rs:363` — path-separator rejection.

Still missing everywhere, including on those two paths:

- control- and bidirectional-formatting-character rejection (`Marketplace name
  cannot contain control or bidirectional-formatting characters` and the
  `Plugin name …` twin) — the 2.1.247 hardening;
- `Marketplace must have a name`, the `".." / "."` clause, and
  `Marketplace name impersonates an official Anthropic/Claude marketplace`.

And the structural problem is that **discovery, install, and marketplace
ingestion never call the gate at all** — the validators live in `plugin tag` /
`plugin init`, not on the path a third-party catalog entry travels.
`sanitize_segment` (`plugin/src/discovery.rs:211-235`) is a faithful port of the
cache-path sanitiser and is not a rejection gate. Separately, `plugin list` /
`plugin details` print manifest-controlled name/version/description/author
unescaped (`apps/cli/src/commands/plugin.rs:690-700,1059-1069`), missing the
2.1.247 escape-safe output hardening.

---

## §9–§12 · P2 — MCP transport / schema

### 9. `sdk` entries dropped while diagnostics bless them `[CODE][SCHEMA]`

Oracle `MAn`: `{type:"sdk", name, timeout, alwaysLoad}` — **no `url`**. Port
requires `command` or `url` (`mcp/src/json_config.rs:230-233,279`), so the
oracle-valid shape is skipped; the `Some("sdk")` arm (`:346`) is reachable only
with a URL and then misuses it as the control-channel id. `KNOWN_MCP_TYPES`
lists `sdk` (`mcp/src/config_diagnostics.rs:64-72`).

### 10. `sse-ide`/`ws-ide` diagnosed unknown, dialled as HTTP `[CODE][SCHEMA]`

Oracle requires `ideName` on both (`ws-ide` adds optional `authToken`), so a
missing `ideName` fails `safeParse` and skips the entry. Port: `KNOWN_MCP_TYPES`
omits both (warning fires) yet the wildcard arm turns both into `Http`
(`mcp/src/json_config.rs:320-359`); `ws-ide` has no transport variant.
`agent/src/mcp_servers.rs:74` correctly skips both for agent frontmatter only.

### 11. `discoveryCache` and `role:"comms"` unmodelled `[GREP][SCHEMA]`

`discoveryCache` (sse/http) gates the persisted discovery cache — the whole
vocabulary (`mcp-discovery-cache`, `MCP_DISCOVERY_CACHE`, `opt-out`,
`env-disabled`, `miss_disabled`/`miss_expired`/`miss_corrupt`/`miss_strike`/
`miss_no_fingerprint`, `connected_zero_tools`) is absent.
`role: literal("comms").optional().catch(undefined)` on
stdio/sse/sse-ide/ws-ide/http/ws — **not** `sdk`, **not** `claudeai-proxy`;
`.catch` means an invalid value is stripped, never fatal.

### 12. Parser is shape-driven, not schema-discriminated `[CODE][SCHEMA]`

Port deserializes one permissive aggregate and picks `command` first, `url`
second. Consequences: `{"url":…}` and `{"type":"bogus","url":…}` accepted as
HTTP; `{"type":"http","command":"x"}` accepted as stdio; empty stdio `command`
passes the loader despite `.min(1,"Command cannot be empty")`; non-oracle alias
`type:"websocket"` accepted; `request_timeout_ms` honoured for `ws` which the
oracle `ws` schema strips; `disabled` read and acted on though absent from the
recovered on-disk union.
OAuth child (`platform-api/src/mcp.rs:160-182`): oracle wants a **positive** integer
`callbackPort`, an `https://` `authServerMetadataUrl`, non-empty `scopes`; Rust
accepts port `0`, an arbitrary string, an empty scope, and rejects
schema-valid ports above `u16::MAX`.

---

## §13–§16 · P2 — marketplace, manifest, supply chain

### 13. Two different source unions `[CODE][SCHEMA]`

**(a) Registration source** (`extraKnownMarketplaces` / registry / policy),
union `dYe` @~154649600: `url {url, headers, headersHelper}` · `github {repo,
ref, path, sparsePaths, skipLfs}` · `git {url, ref, path, sparsePaths, skipLfs}`
· `npm` · `file` · `directory` · policy-only `skills-dir`, `hostPattern`,
`pathPattern`, inline `settings`.
Port gaps: `run_add` takes **`_sparse: &[String]` and discards it**
(`apps/cli/src/commands/plugin_marketplace.rs:541-563`) — `--sparse` is a lie and
the registry records nothing, so updates never sparse-checkout either; no
`skipLfs`/`GIT_LFS_SKIP_SMUDGE`; url registration cannot keep
`headers`/`headersHelper`; the writer covers only directory/github/git/url
(`:500-518`); `skills-dir` and inline-`settings` marketplaces unrepresented.

**(b) Plugin-entry source** (`plugins[].source`), union `ft` @~154654000 — a
**different** union: relative `./…` · `npm {package, version?, registry?}` ·
`url {url, ref?, sha?}` where **`url` is a git repository** · `github {repo,
ref?, sha?}` · `git-subdir {url, path, ref?, sha?}` (partial clone
`--filter=tree:0`) · `archive {https url, sha256?}` · `command {command,
timeout?, mode?: copy|link}` · a parse-time `unsupported` placeholder.
Port models github/git/url/npm/file/directory (`plugin/src/marketplace.rs:51-79`)
and its `Url` arm **downloads and unpacks an archive**
(`plugin_install.rs:346` → `:200` → `startup_resources.rs:135`) where the oracle's
`source:"url"` means a git checkout — same key, different code path. Missing:
`git-subdir`, `archive`, `command`/`link`, `sha` (40-hex) / `sha256` (64-hex)
pinning, `version`/`registry`, and the `unsupported` placeholder.
Failure boundary also differs: the oracle transforms each entry independently and
stubs a named-but-invalid entry as `source:"unsupported"` so delisting does not
read it as a removal; Rust decodes a typed `Vec<MarketplacePluginEntry>`, so one
unknown variant can fail the whole index. `MarketplacePluginEntry` lacks
entry-level `strict`, `headers`, `headersHelper`, `category`, `tags`,
`relevance`, inline-manifest fields; `MarketplaceIndex` lacks `owner`, version,
description, `forceRemoveDeletedPlugins`, `renames`, and keeps only `pluginRoot`
in `metadata` (`plugin/src/marketplace.rs:17-39`).

### 14. Plugin manifest — corrected status `[CODE][SCHEMA]`

| contract | port status |
| --- | --- |
| `keywords`, `license`, `repository` | Parsed (`discovery.rs:129-137`), dropped building `PluginManifest`. |
| `metadata` | Not preserved, though the oracle preserves it unread. |
| `$schema` | Ignored upstream by design — **not a gap**. |
| `experimental.evals` | String form works (`plugin_eval.rs:3693-3745`); oracle also accepts a list and takes its first element, Rust rejects the list and falls back. |
| `monitors` | Stable top-level field (relative JSON path or inline unique-name array; `always` / `on-skill-invoke:<skill>`; unsandboxed, hook trust tier). Monitor substrate exists; plugin discovery never loads or arms it. |
| `binaries` | Missing. ≤64 safe basenames, 64-hex SHA-256 pins, fetched into `bin/` at install. |
| `themes` | Missing — `union([path, path[]])`, suppresses the `themes/` auto-scan. |
| `workflows` | Missing — `union([path, path[]])` over dirs or `.js`. |
| `syntaxHighlighting.hljsLanguages` | Missing — ≤16 `.strict()` entries `{id: /^[a-z][a-z0-9_-]*$/, remote: npm:/github: form, integrity: SRI sha256\|384\|512}`. |
| hooks `modules` | `hooks.json` may declare one JS module exporting `register(on)` ("a second entry is refused"; "must have `hooks` or `modules`, or both"). Rust parses only settings-shaped hooks (`discovery.rs:1076-1109`). |
| `mcpServers` MCPB arm | Oracle accepts relative/URL `.mcpb` **and `.dxt`** inside `mcpServers`; `mcpb` is a union arm, not a top-level field. Rust treats strings as JSON paths, has no `.dxt`, never wires `plugin/src/mcpb.rs` to manifest discovery. |
| `channels[].displayName` | Dropped (`discovery.rs:173-181`). |
| `author.email` / `author.url` | Dropped — `RawAuthor` reduces to a name (`discovery.rs:160-190`). |
| `userConfig` strictness | Oracle: identifier keys, **required** `type`/`title`/`description`, fixed type enum. Rust: all optional/defaulted, arbitrary type strings (`plugin/src/manifest.rs:119-153`). |

### 15. Plugin archive fetch: no host denylist, no digest verification `[CODE][SCHEMA]`

**Corrected in round 4 — the transport half is already ported.**
`apps/cli/src/startup_resources.rs:100` refuses a non-`https` initial URL,
`:73-74` refuses a redirect to a non-HTTPS URL, and `:75-84` refuses a
cross-origin redirect on a credentialed download. So plain-HTTP fetches, and the
classic `http://169.254.169.254/…` example, are **not** reachable.

What is still missing from `download_plugin_urls`
(`apps/cli/src/startup_resources.rs:135-154` → `bounded_get` → unpack):

- **Host/IP denylist.** Oracle `archive` sources carry
  `.url().refine(Zqt)` — *"Archive URLs must use https:// and must not point at a
  loopback, link-local, or cloud-metadata host"* — backed by `PTt`:
  `127.0.0.0/8`, `169.254.0.0/16`, `0.0.0.0/8`, `100.100.100.200`, `localhost`
  and `*.localhost`, `::1`, `::`, `fd00:ec2::254`, `fe80::/10`, and IPv4-mapped
  forms. The port has no equivalent, so an `https://` URL pointing at a loopback
  or link-local service is still fetched and unpacked.
- **`sha256` verification.** Oracle: *"verified against every download and the
  install is refused on mismatch"*, and it doubles as the version identity when
  no `version` is declared. The port has no digest check on this path.

Residual severity is a same-origin-ish SSRF plus an unpinned supply chain, not
an arbitrary-scheme fetch. Priority lowered accordingly.

### 16. `command`-source consent guards absent `[SCHEMA]`

**Not an exploitable hole today** — these are the guards for a feature the port
does not implement at all (§13b). File them together with the `command` source
whenever it is built, not as a standing vulnerability.

Oracle `command` source: `.min(1).max(<consent-UI width>, "command must not be
longer than the install consent UI can display")` and *"command must be printable
ASCII (letters, digits, punctuation, single spaces) with no runs of 4 or more
spaces"* — anti-spoofing for the consent dialog, with the runtime refusal
`X is installed by running a command on this machine (…) that has not been
reviewed yet, so it was not run.` The port has no `command` source (§13b), so
neither the feature nor its guards exist. Also absent: the reserved/official-name
gate (`validateOfficialNameSource` accepts only `github`/`git` from
`anthropics/*` for reserved names).

---

## §17–§20 · MCP connection runtime and telemetry

§17–§20b were found in round 3 (no earlier round examined the connect path);
§20c is the plugin-telemetry section from round 2, restored after round 3 lost it
during renumbering. All negatives below were control-guarded.

### 17. MCP **protocol-era negotiation** is entirely absent `[GREP][SCHEMA]`

Oracle `co(e,t,o)` @~182283165:

- reads `MCP_PROTOCOL_NEGOTIATION`; accepts only `legacy` | `auto`; anything else
  logs `MCP_PROTOCOL_NEGOTIATION=<v> is invalid; expected 'legacy' or 'auto' —
  ignoring` at `warn` and is treated as unset;
- `legacy` ⇒ `{mode:"legacy"}` unconditionally;
- `auto` ⇒ only for `Kr = {"http","claudeai-proxy","ccr-proxy","stdio"}`;
  probe timeout `min(3000, floor(base/3))` for stdio, `min(5000, floor(base/3))`
  otherwise;
- unset ⇒ per-transport gates `tengu_mcp_protocol_negotiation_http` /
  `…_claudeai` / `…_ccr`; `stdio`, `sse`, `ws`, `ide`, `in-process`,
  `sdk-control` are always `legacy`;
- a server-denylist check (`tengu_mcp_negotiation_server_denylist`) downgrades to
  legacy and logs `MCP era negotiation denylist matched <host>; the legacy
  handshake applies`, where `<host>` is the URL hostname, else
  `a url-less server (the '*' entry)`, else `a server with an unparseable url`;
- transport→label mapping `Wr`: `http`→`http`|`ccr-proxy`, `sse-ide`/`ws-ide`→
  `ide`, `sdk`→`sdk-control`, `stdio`/`undefined`→`stdio`, in-process→`in-process`.

Downstream, capability gating keys off the negotiated revision:
`skills-capable`, `channel-capable`, `live-connection`, and the refusal
`… negotiated <revision>, a modern-era protocol revision, which has no delivery
path for unsolicited custom notifications`.

Port: `MCP_PROTOCOL_NEGOTIATION`, `era negotiation denylist`, `legacy handshake
applies`, `channel-capable` — all `<none>` with a passing control.

### 18. Connect-path error **classification** and telemetry absent `[CODE][SCHEMA]`

**Corrected in round 4 — the timeout behaviour itself is ported.**
`mcp/src/registry.rs:888-918` bounds the connect + initialize handshake by
`mcp_connection_timeout()`, which is `parseInt(MCP_TIMEOUT) || 30000`
(`:2219-2222`). What is missing is everything the oracle wraps around it.

Oracle emits `mcp_connect_starting` / `mcp_connect_skipped` and carries an
`errorCode` vocabulary. Present in Rust: `UNCONFIGURED`, `INVALID_CONFIG`
(`mcp/src/connection.rs`). Absent (binary literal counts in parentheses):
`CONNECT_TIMEOUT` (16) with its `tengu_mcp_connect_timeout_retry` gate and the
`MCP connection timeout` message, `AUTH_HEADER_REJECTED` (8),
`HEADERS_HELPER_AUTH_REJECTED` (10), `FIRST_PARTY_AUTH_REJECTED` (8).

> Round-3 also listed `NEEDS_AUTH`. That was **fabricated** — its literal count
> in the binary is 0; upstream returns `{type:"needs-auth"}`, which the port
> already models as `McpActionState::NeedsAuth`. Removed.

Also absent: `tengu_mcp_server_config_invalid` (fields `transportType`, `field`,
`source: loader|connect`), the invalid-URL config error `'url' is not a valid
URL. Update the server's config and reconnect.`, `lazy_dial_failed`,
`cached-row adopt subscriber threw:`, `cached-row dial-failed subscriber threw:`,
`mcp_reconnect_identity_changed` + `Reconnect cancelled: the account changed
while connecting. Choose Reconnect again.`, and `MCP: staging root unavailable,
omitted from roots/list:`.

### 19. **Static `headers.Authorization` must disable OAuth fallback** `[CODE][SCHEMA]`

This supersedes round 1's "low-confidence residual" on the 2.1.248 headersHelper
fix. Oracle `bo({…, hasUserAuthHeader, helperMintsAuthHeader, cliOwnedBearer,
useFirstPartyAuth, firstPartyBearer, …})` @~182269557 branches **before** any
OAuth path:

- `hasUserAuthHeader` ⇒ `AUTH_HEADER_REJECTED`, message `Server rejected the
  configured Authorization header (HTTP <n>). Check that the token is valid for
  this MCP endpoint — OAuth fallback is disabled when headers.Authorization is
  set.`
- `helperMintsAuthHeader` ⇒ `HEADERS_HELPER_AUTH_REJECTED`, message `Server
  rejected the Authorization header minted by the configured headersHelper (HTTP
  <n>). Check that the helper command retu…`
- a first-party family (`design_credential` / `none` / `login` /
  `design_scoped_login`) ⇒ `FIRST_PARTY_AUTH_REJECTED`.

Port (`mcp/src/registry.rs:895-953`) — **corrected in round 9.** There is no
"OAuth fallback on a plain static-header server": `resolve_oauth_spec`
(`:1140-1145`) returns the spec unchanged when the config has no `oauth` block,
so nothing to suppress. Three real gaps remain:

1. **A static `headers.Authorization` on a server that *also* has an `oauth`
   block is overwritten.** `inject_bearer` (`registry.rs:2402-2411`) does
   `headers.insert("Authorization", bearer)` on the `IndexMap`, which replaces
   any existing entry; the oracle instead treats `hasUserAuthHeader` as authoritative
   and reports `AUTH_HEADER_REJECTED` with *"OAuth fallback is disabled when
   headers.Authorization is set."*
2. **The helper branch keys on the wrong predicate.** `has_headers_helper`
   (`mcp/src/headers_helper.rs:15-17`) tests whether a helper is *configured*,
   not whether it actually minted `Authorization` — the oracle's
   `helperMintsAuthHeader`.
3. **Ordering.** The helper is resolved first (`:909-914`) and
   `resolve_oauth_spec` runs on the already-resolved config (`:915`), so an
   OAuth bearer can overwrite the `Authorization` the helper just minted.

None of the three error codes or their messages exist.

### 20a. MCP tool **JSON-Schema normalization** is absent `[CODE][SCHEMA]`

**Promoted in round 4 from "open question" to a confirmed behaviour gap.** Oracle
gates `tengu_mcp_normalize_root_combinators` and
`tengu_mcp_drop_invalid_tool_schemas`, and classifies every listed tool as one of
`tool_schema_normalized`, `tool_schema_normalize_gated`,
`tool_schema_unsupported`, `tool_schema_invalid`, `tool_property_key_invalid`,
`tool_schema_invalid_gated`, `tool_property_key_invalid_gated` — i.e. it
flattens root combinators, drops invalid schemas, and rejects invalid property
keys before the schema reaches the model.

Port: `mcp/src/normalization.rs` handles **server-name** normalization only. The
tool's `inputSchema` is forwarded verbatim (`mcp/src/client.rs:540`) and bound
straight onto the tool (`tools/mcp/src/mcp_tool.rs:451`, `bound_schema`). A
server that returns a root-combinator or otherwise unsupported schema therefore
reaches the provider unmodified, where the oracle would have normalized or
dropped it. This belongs with the behaviour work, not the telemetry tail.

### 20b. Tool-listing telemetry and error copy absent `[GREP][SCHEMA]`

`tengu_mcp_tools_listed` / `mcp_list_tools` with `normalizedCount`, `keptCount`,
`listDurationMs`, `alwaysLoadCount`; `_needs_auth`, `_claudeai_bearer_rejected`,
`mcperr_other`, `unregistered_mcp`; `No such tool available: mcp_tool`,
`is connected but does not offer this tool here`, `has since been authenticated
and its real tools are available`, `ignoring invalid timeout for SDK MCP server
'…'`, `Cached MCP server …: adoption prompts/list failed (…); keeping cached
prompts`, `URL elicitation required (open URL, then retry mcp_call): …`,
`"…" is not a deny wildcard outside mcp__ server specs`.

### 20c. Plugin telemetry is entirely absent `[GREP][SCHEMA]`

**Restored — round 3 dropped this section during renumbering.** Control-guarded
re-check: `tengu_plugin_enabled_for_session`, `plugin_id_hash`, `enabled_via`,
`host_owned_mcp`, `tengu_plugin_load_failed` are all `<none>`.

Oracle emits an OTel `plugin_loaded` metric **and** a
`tengu_plugin_enabled_for_session` event per loaded plugin, carrying
`plugin_id_hash`, `plugin_scope`, `plugin_name_redacted`,
`marketplace_name_redacted`, `is_official_plugin`, `enabled_via`
(`org-policy` / `auto_install` / **`admin-install`** / `seed-mount`),
`installation_preference`, `has_hooks`, `has_mcp` (`!skipMcpDiscovery &&
mcpServers!==undefined`), **`host_owned_mcp`** (`skipMcpDiscovery===true`),
`has_lsp`, `has_settings`, `skill_path_count`, `command_path_count`,
`agent_path_count`, `server_plugin_id`, `sessions_since_last_use`,
`days_since_last_use`, `settings_keys`, `safe_mode`, `version`.

Siblings, all absent: `tengu_plugin_name_collision`,
`tengu_plugin_folder_shadowed`, `tengu_plugin_renamed`,
`tengu_plugin_load_failed`, `plugin_load_monitors` /
`plugin_load_monitors_resolve_failed`, and the CLI family
`tengu_plugin_installed`, `…_installed_cli`, `…_uninstalled_cli`,
`…_enabled_cli`, `…_disabled_cli`, `…_disabled_all_cli`, `…_updated_cli`,
`…_command_failed`, `…_remote_fetch`. The port's telemetry catalogue has no
plugin module at all.

---

## §21 · Final disposition of formerly open questions

Nothing in this section remains ambiguously “open”. Items that could not be
proven from the specified Mach-O are explicitly deferred and are not part of the
implementation backlog.

1. **Startup `N MCP servers need authentication` warning — EXPLICITLY
   DEFERRED.** The exact sentence appears only in the embedded changelog at
   @165140187; no runtime producer was recovered. The only port hit remains a
   tool-description string (`tools/mcp/src/wait_for_mcp_servers.rs:58`). Do not
   count this as either a gap or a refutation without an end-to-end runtime probe.
2. **`listMcpResources` — CLOSED, NOT A GAP.** `ListMcpResourcesTool` is defined,
   implemented, and registered behind the `resources` capability
   (`tools/mcp/src/mcp_tool.rs:54`, `tools/mcp/src/lib.rs:30,38`), with a legacy
   alias at `llm-runtime/src/convert.rs:292`.
3. **JSON-Schema normalization — CLOSED, CONFIRMED GAP.** Promoted to §20a.
4. **Failed-server ToolSearch note — CLOSED, CONFIRMED PARTIAL-PORT GAP.** The
   oracle enables `tengu_surface_failed_mcp_servers` by default
   (`function VKe(){return I("tengu_surface_failed_mcp_servers",!0)}`
   @156292983) and supplies the failed list to ToolSearch @158539909. The port
   has the note body, but collection is guarded by
   `telemetry::flag_bool("tengu_surface_failed_mcp_servers", false)`
   (`tools/meta/src/tool_search.rs:655-660`), so the required provider/
   telemetry-disabled behaviour is off by default.
5. **`--strict-mcp-config` / `requiresUserInteraction` — CLOSED, CONFIRMED
   GAPS.** See §27a and §27b.
6. **`/mcp` claude.ai connector heading/scope — CLOSED, ACCEPTED DIVERGENCE.**
   This is part of the claude.ai connector surface excluded in §0.
7. **Managed-settings MCP startup-mode approval exemptions — EXPLICITLY
   DEFERRED / OUT OF SCOPE.** The exact environment-variable family was not
   recovered (`MCP_STARTUP` count 0), and the behaviour belongs to the managed
   settings approval subsystem rather than `mcp/` or `plugin/`.
8. **MCPB / `.dxt` — CLOSED, CONFIRMED GAP.** `.dxt` is accepted nowhere in the
   port; `MCPB content hash: …` and `No manifest.json found in MCPB file: …` are
   absent. Tracked under §14's `mcpServers` MCPB row.
9. **Versionless plugin update can replace a live shared cache — CLOSED,
   CONFIRMED GAP.** The Rust CLI skips the “already current” fast path when
   `new_version == "unknown"` and then unconditionally runs
   `remove_dir_all(dest)` (`apps/cli/src/commands/plugin_install.rs:1840-1851`).
   Every scope shares `cache/<marketplace>/<plugin>/unknown`, so an update can
   remove files used by another live scope/session. The oracle's `ice` cache
   materializer checks whether the path is live and, when another session is
   using it, logs `deferring overwrite until it exits` and returns without
   deleting (@162795186). This is distinct from the already-fixed second-scope
   *install* path and from the dead `PluginManager::install` duplicate.

### Settled refutations (do not re-file)

- Directory-loaded plugins escaping via a declared MCP JSON path — **false**;
  `resolve_declared_relative_path` (`plugin/src/discovery.rs:795-814`) requires
  `./` and rejects absolute/parent/root/prefix components. The port is *stricter*
  than the oracle here; the real delta is the missing MCPB-source skip and the
  two directory-loaded warnings.
- `/mcp` action parsing missing — **false**; `tui/src/chat_widget.rs`.
- MCP policy predicate-expansion warnings missing — **false**;
  `mcp/src/enterprise_policy.rs:667,1041`.

---

## §22 · `/plugin` copy — confirmed-missing set

Both plugin-name collision warnings (managed-locked and already-taken) with the
`.claude-plugin/plugin.json` remediation line · `N error(s) during load. Run
/plugin for details.` and its remote twin (the port routes the aggregate to
`/doctor`) · `Plugin archive from X contained no plugin files …` · `… does not
contain the component paths its marketplace entry declares …` · `Marketplace name
X is not a plain directory name; refusing to cache it` · `Clearing stale
case-variant marketplace directory X (unregistered) before publishing Y` ·
`Invalid known_marketplaces.json in zip cache: …` · `Skipping MCPB source "X" for
directory-loaded plugin "Y": …` · `Skipping out-of-directory MCP source "X" for
directory-loaded plugin "Y": …` · `/plugin configure <plugin> - Set userConfig
options` and the interactive configure route · the proactive `N auto-installed
dependencies no longer needed … claude plugin prune` notice · `A disabled setting
for <name> exists, so it won't load until you re-enable it in /plugin` ·
`Run /reload-plugins to activate` / `Configuration saved. Run /reload-plugins for
changes to take effect` (some CLI paths say `Restart to apply changes`) · the
four `Invalid owner-wildcard …` / `Invalid pathPattern regex …` policy
diagnostics (the §7 semantics gap).

## §23 · NEW in round 5 — managed settings and config ingestion

Found by tracing the oracle's *disable* mechanism (see §12) into the settings
layer. Both negatives are control-guarded.

### 23a. The allowlist is unconditionally managed-only `[CODE][SCHEMA]`

Oracle: `allowManagedMcpServersOnly` — *"When true (and set in managed
settings), `allowedMcpServers` is only read from managed settings.
`deniedMcpServers` still merges from all sources, so users can deny servers for
themselves."* Invalid values fail closed (`… treating it as true until it is
fixed.`). When the key is **absent or false**, `allowedMcpServers` merges from
every settings tier like any other list.

Port: the flag itself is `<none>`, and
`McpPolicy::from_effective_settings` (`mcp/src/enterprise_policy.rs:133-139`)
hard-codes the *true* branch — deny is read from `ordinary.chain(managed)`,
while "allow entries are accepted only from managed policy". A unit test asserts
the ordinary-tier allow is ignored.

So the divergence is **over-strictness, not a hole**: a user- or project-scope
`allowedMcpServers` that the oracle would honour (no managed
`allowManagedMcpServersOnly`) is silently dropped here. Round 7 stated the
opposite; that is retracted.

The invalid-value fallback copy family is **partial**: `"deniedMcpServers" was
present but invalid and was dropped; its entries cannot be enforced until it is
fixed.` exists in `enterprise_policy.rs`; the `… treating it as true until it is
fixed.` variants do not.

Out of scope (claude.ai surface): `allowAllClaudeAiMcps`, `syncClaudeAiPlugins`,
`disableClaudeAiConnectors`.

### 23b. MCP config-file ingestion guards and diagnostics `[CODE][SCHEMA]`

Before parsing an MCP config the oracle checks the file itself and emits a typed
diagnostic plus telemetry:

| condition | message / suggestion | telemetry |
| --- | --- | --- |
| not a regular file, or over the byte limit | `MCP config is not a regular file or exceeds <n> bytes: …` · *"Check that the path is a plain JSON file (not a device, FIFO, or symlink to one)"* | `mcp_config_shape_gate` |
| missing | `MCP config file not found: …` · *"Check that the file path is correct"* | — |
| unreadable | `MCP config read error for …` · *"Check file permissions and ensure the file exists"* | `mcp_config_read_failed` |
| malformed | `MCP config is not valid JSON: …, length=…, first100=…` · *"Fix the JSON syntax errors in the file"* | `mcp_config_invalid_json` |

Port: none of the four diagnostics or three telemetry ids exist, and
`mcp/src/config_diagnostics.rs` validates only the **parsed** object's shape.

**Corrected in round 9:** the `--mcp-config` path is *not* the exposure — it is
already gated on `Path::is_file()` (`apps/cli/src/init.rs:381`). What is
unguarded is the ambient `.mcp.json` / global-config read
(`plugin/src/discovery.rs:945`, `mcp/src/json_config.rs`), which goes straight to
`read_to_string`. And on **every** path there is no byte-size limit and no typed
diagnostic, regardless of file type.

## §24 · NEW in round 6 — sweeping the 26 unswept files

### 24a. `PluginBlocklist` is a gate that can never fire `[CODE]`

`plugin/src/blocklist.rs` holds `static_block: HashSet<PluginId>` +
`remote_block: RwLock<HashSet<PluginId>>`. But:

- `static_block` is written **once**, to `HashSet::new()` (`:32`), and there is
  no setter for it anywhere — the doc comment's claim that it "is populated by
  the host at construction time" is not true of any constructor that exists;
- `set_remote_blocklist` has **no caller**;
- the only construction site passes an empty URL:
  `PluginBlocklist::new(String::new())` (`apps/engine-desktop/src/lib.rs:10040`).

So `is_blocked` always returns `None`, yet it *is* called on the install path
(`plugin/src/manager.rs:481`) — which is why a code read makes it look live.

**Not a parity gap.** The oracle has no plugin blocklist: its `blocklist` hits
are the React compiler, the WebFetch domain blocklist, and `blockedMarketplaces`
(already covered in §7/§13a). This is a LingXi-original security gate that was
built and never wired. File it as a port defect, not as a 2.1.251 divergence.

### 24b. Per-subagent inline `mcpServers` is unimplemented end to end `[CODE]`

Round 6 filed this as "dead code, possible connection leak". **Corrected in
round 9 — the gap is larger and the leak is not real.**

- `AgentScopedConnections` (`mcp/src/agent_scope.rs`) has zero references outside
  its own file. (Beware: `git grep agent_scope` hits `execute_agent_scoped` /
  `agent_scoped_stop` in `hooks/` and `agent/`, an unrelated hook concept.)
- `AgentToolResolver::resolve` accepts an `agent_mcp_tools: &[Arc<dyn Tool>]`
  parameter (`agent/src/tool_resolver.rs:159`) and appends it (`:261`), and its
  one production caller passes a **literal empty slice**:
  `AgentToolResolver::resolve(agent_def, &parent_tools, &[], depth, false)`
  (`:360`). (Round 9 first said "no caller supplies it" — wrong; a name-based
  grep for `agent_mcp_tools` cannot see a positional `&[]`.)
- Agent-frontmatter `mcpServers` merging serves only the main agent selected at
  startup (`apps/engine-desktop/src/lib.rs`), not spawned subagents.

So the whole chain — connecting a subagent's inline `mcpServers`, injecting its
tools, and tearing the connections down on exit — is missing. Because no
per-subagent connection is ever created, **no leak can be claimed**; `agent_scope.rs`
is the unused teardown half of a feature whose other halves were never built.

### 24c. OAuth ignores `resource_metadata` in `WWW-Authenticate` `[CODE][SCHEMA]`

Oracle (@182022194 and @182113452) parses the `WWW-Authenticate` challenge into
`{resourceMetadataUrl, scope, error, errorDescription}` via
`<param>=(?:"([^"]+)"|([^\s,]+))`, and uses `resourceMetadataUrl` to fetch the
RFC 9728 document **at the URL the server names**. It also validates the
resource indicator: `Protected resource ${r.resource} does not match expected
${s} (or origin)`.

Port: `resource_metadata` is `<none>` in `mcp/`. `mcp/src/oauth.rs:194-200`
derives the PRM document solely from `/.well-known/oauth-protected-resource` on
the server origin, so a server whose PRM lives anywhere else fails discovery. The
`scope` / `error` / `error_description` challenge parameters are likewise not
extracted, and there is no resource-indicator validation.

Everything else in the OAuth cluster checks out: RFC 9728 → RFC 8414 discovery,
`/.well-known/openid-configuration` fallback, dynamic client registration (RFC
7591), PKCE `S256`, `refresh_token`, `revocation_endpoint`,
`token_endpoint_auth_method`, and the `insufficient_scope` step-up. Absent and
**correctly so**: `client_credentials` and `device_code` — in the oracle those
belong to the Claude apps gateway sign-in, not to MCP server OAuth.

### 24d. `strictPluginOnlyCustomization` accepts one non-oracle alias `[CODE][SCHEMA]`

Oracle: `dt([q(), H(ie($pe))])` with `$pe = ["skills","agents","hooks","mcp"]`,
and the array form is pre-filtered by `r.filter(c => $pe.includes(c))`.
Port `component_from_slot` (`plugin/src/strict_policy.rs:98-107`) accepts those
four **plus `"mcpServers"`**, which the oracle's enum would drop. So
`{"strictPluginOnlyCustomization":["mcpServers"]}` locks MCP here and locks
nothing upstream. Everything else in that file is faithful: `true` locks exactly
the four (`all_components`), unknown slots are dropped, and the scalar
last-tier-wins merge matches.

### 24e. Correction to §11 — `skills-capable` / `channel-capable` `[CODE][SCHEMA]`

Round 3 listed these next to the `miss_*` vocabulary without saying what they
are, which invited reading them as a missing "MCP skills/channels" feature. They
are **discovery-cache miss reasons**: oracle `rs(e)` turns a `fresh`/`stale`
cache entry into `{kind:"miss", reason:"skills-capable"}` (or
`"channel-capable"`) when the cached entry's `capabilities` match, i.e. a server
advertising those capabilities is never served from the discovery cache. They
fold into §11 and are not a separate finding.

Two related observations do stand:

- The port's `ServerCapabilitiesDto` (`platform-api/src/mcp.rs:200-211`) carries only
  `tools` / `resources` / `prompts` / `logging` / `experimental`, narrower than
  what the oracle inspects.
- The oracle keeps a **server identity epoch** (`identityBaseline`,
  `identityEpoch`, `persistedDiscoveryRounds`, `rawFetchedAtByResult`,
  `discoveryFetchErrors`) that drives cache invalidation and the
  `mcp_reconnect_identity_changed` path in §18. `mcp/src/identity.rs` is
  unrelated — it holds only the outgoing `clientInfo` constants.

### 24f. Came back clean

Recorded so a future round does not re-audit them: `initialize_params.rs`
(protocol version `2025-11-25` matches the oracle's bundled SDK `LATEST_PROTOCOL_VERSION`;
`{roots:{listChanged:true}, elicitation:{}}` capability shape matches) ·
`identity.rs` (deliberate LingXi rebrand of `clientInfo`, `websiteUrl`
binary-confirmed) · `transform_result.rs` (handles `text`, `audio`, `image`,
`resource` text/blob, and `resource_link`; `structuredContent` is handled in
`mcp/src/client.rs`) · `strict_policy.rs` apart from §24d · `capabilities.rs`,
`raw_conn.rs`, `trust.rs` (small, faithful, wired) · every other `pub fn` in the
26 files has an external caller.

## §25 · NEW in round 7 — the last 14 files, line-compared

### 25a. ~~Installed-plugins record~~ — **RETRACTED in round 9**

The claim was backwards; see the round-9 retraction table in §0. Production Rust
already writes `installed_plugins.json` with `installedAt` / `lastUpdated` /
`installPath`. The only residue is that the **dead** `plugin/src/installed.rs`
models the record as `{version, added}` — recorded under §25d, not as a
divergence.

### 25b. `mcp/src/approval.rs` is a superseded duplicate `[CODE]`

`McpApprovalPolicy` / `ApprovalStatus` have **zero external use**, and
`project_servers_require_approval` is written in `new()` and never read — not
even inside the file, so `is_approved` cannot be turned off.

**Not a missing feature.** Project `.mcp.json` approval *is* implemented, in
`mcp/src/server_gate.rs:102-135` (`decide`, driven by `enabledMcpjsonServers`,
`disabledMcpjsonServers`, and `enableAllProjectMcpServers`). This is the third
dead subsystem after §24a and §24b; delete it, or the next reader will wire the
wrong one.

### 25c. The confusable-URL guard (`ffe`/`pfe`) has no port counterpart `[CODE][SCHEMA]`

Oracle `ffe(t)` classifies a git-ish URL as suspicious when: `pfe(t)` finds a
backslash in the authority of an `http/https/ws/wss/ftp` URL (URL-parser
confusion, e.g. `https://evil.com\@good.com/`); or a non-http(s) scheme has a
hostname matching `/[%\x00-\x1f\x7f-\u{10FFFF}]/u`; or an scp-form
`user@host:path` has a `:` before the `@`.

It is applied at **four policy boundaries**:

| site | effect |
| --- | --- |
| `Is` (`validateOfficialNameSource`) | a suspicious URL can never claim a reserved/official marketplace name; also enforces the protocol allowlist `{https:, http:, git:, git+https:, git+http:, git+ssh:, ssh:}`, an `anthropics/*` owner check, and a `..` path-segment refusal |
| `M` (URL normalisation) | a suspicious URL is returned untouched instead of having credentials stripped |
| `D` (policy **allowlist** matcher) | `if (t.source==="git" && ffe(t.url)) return false` — a suspicious git URL **never matches an allowlist entry**, i.e. fails closed |
| `Xe` (marketplace classification) | returns `invalid_marketplace` |

Port: nothing equivalent. `plugin/src/git.rs:20-30` checks only the scheme
prefix (`https://`, `http://`, `file://`, `git@`, `ssh://`); there is no
backslash-authority check, no control-character hostname check, and no
fail-closed behaviour in `plugin_policy.rs`'s matcher (which §7 already shows is
a glob rather than a regex).

This also supplies the concrete implementations §8 was missing: the
impersonation test is
`/(?:anthropic|claude)[^a-z0-9]*official|^(?:anthropic|claude)[^a-z0-9]*(marketplace|plugins|official)/i`
plus a non-printable-ASCII test `/[^\u0020-\u007E]/`, and the enforcement copy
is `The name '<n>' is reserved for official Anthropic marketplaces and its
registered source is malformed.` · `"<n>" is registered from an untrusted
source: …` · ` To fix it, remove the marketplace and re-add it from the official
source.` · `Reserved marketplace name registered from untrusted source` — all
`<none>` in the port.

### 25d. `PluginManager::install` is a fully-built install path with no production caller `[CODE]`

`plugin/src/manager.rs`'s module doc says the network arms "return a typed,
capability-named error until the fetch + marketplace-policy machinery is
ported". That comment is **stale**: the git, marketplace, and `.mcpb` arms are
implemented (`:301`, `:332`, `:375`, each landing through `copy_into_cache`), and
the marketplace arm even carries a cache-escape check the CLI path does not
(`Marketplace name '<n>' resolves to a path outside the cache directory`).

But `.install(` has no non-test caller — production installs go through
`apps/cli/src/commands/plugin_install.rs`. So the repo carries **two** install
implementations with different guards, and the fixes in §13 and §15 would have
to be made twice, or one path deleted. Decide which is canonical before starting
that work.

### 25e. Came back clean

Recorded so a later round does not re-audit them:

- `env_expansion.rs` — the `bY` regex
  `/\$\{([A-Za-z_][A-Za-z0-9_]*(?::-[^}]*)?)\}/g` is **byte-identical in
  2.1.251**, so the "1:1 port of 2.1.220" header is still accurate.
- `tools/mcp/src/large_output.rs` — `12500`, `25000`, and
  `ENABLE_MCP_LARGE_OUTPUT_FILES` all still present in 2.1.251; the
  `[OUTPUT TRUNCATED …]` copy with the MCP pagination hint is ported.
- `tools/mcp/src/auto_background.rs` — `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`,
  `tengu_mcp_auto_background`, `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`,
  `CLAUDE_AUTO_BACKGROUND_TASKS`, and the `120000` default all confirmed.
- `hook_dispatch.rs` — properly wired end to end
  (`orchestrator/src/mcp_hook_dispatcher.rs:85` implements it,
  `apps/engine-desktop/src/lib.rs:7910` injects it via `with_hook_dispatcher`).
- `loader.rs` — `pluginSecrets` and `pluginConfigs` both exist in 2.1.251.
- `git.rs` — submodule recursion matches the oracle's `--recurse-submodules`;
  the missing `--shallow-submodules` is an explicitly documented git2 limitation
  that fetches more data, never less. `http://` is accepted by the oracle too
  (`ffe` returns false for http/https), so that is not a divergence.
- `lifecycle.rs`, `dependency.rs`, `agent_validation.rs` — LingXi-original
  models (`spec §15.2` / `D2`), wired, no oracle counterpart to diverge from.

## §26 · NEW in round 8 — XAA, the OAuth callback, and resource storage

Correction to an earlier assumption: **XAA is not the Anthropic-account
surface.** `mcp/src/xaa.rs` implements Cross-App Access / Enterprise Managed
Authorization (SEP-990) — RFC 8693 token exchange at the IdP (`id_token` →
ID-JAG) chained into an RFC 7523 JWT-bearer grant at the AS. It is a genuine
enterprise MCP auth feature and is **not** covered by the claude.ai accepted
divergence. It is also well wired: `XaaIdpConfigProvider`
(`apps/engine-desktop/src/lib.rs`), the `xaaIdp` settings, `mcpOAuthClientConfig`,
and a dedicated `apps/cli/src/commands/mcp_xaa.rs`.

### 26a. MCP **resource templates** are entirely unsupported `[CODE][SCHEMA]`

The oracle lists parameterized resources and models them throughout:
`resources/templates/list` (26), `uriTemplate` (28), `resourceTemplates` (63),
the `mcp-template::` reference form (4), and an `mcp_resource_template`
attachment/telemetry kind (6). Resource templates are part of the discovery
round — the discovery-cache entry carries a `templates` field alongside
`tools` / `commands` / `resources`.

Port: **`<none>`** for every one of those, control-guarded. The registry's
catalog fetch is `list_tools` / `list_resources` / `list_prompts` only
(`mcp/src/registry.rs:957-975`), and `ReadMcpResourceTool` /
`ListMcpResourcesTool` have no template counterpart. A server that exposes its
resources only as templates is invisible to LingXi.
(The two `uri_template` hits in the port are unrelated OAuth test fixtures.)

### 26b. XAA token refresh: no proactive window, no single-flight, and a rejected cached token leaves the XAA path `[CODE][SCHEMA]`

**Corrected in round 9.** Round 8 claimed there was "no silent recovery at all"
and that the exchange chain yields no refresh token. Both are too strong:
`resolve_oauth_spec` calls `resolve_xaa_token` on **every** resolve for an
`oauth.xaa` server (`mcp/src/registry.rs:1152-1157`), so a missing or expired
token is silently re-exchanged, and a refresh token — optional in this chain —
is persisted when the AS returns one.

Four real deltas against the oracle's `tokens()` (@182213877; the fourth added in v10.1):

1. **No proactive window — and it is gated on the absence of a refresh
   token.** The oracle's `tokens()` (@182213696) runs the XAA silent exchange
   only when
   `JI() && oauth?.xaa && !n?.refreshToken && (!n?.accessToken || (n.expiresAt-Date.now())/1000<=300)`
   — i.e. no stored refresh token *and* the access token is missing or expires
   within **300 seconds**. The port waits for actual expiry (or a failure).
   Implement the window **with** the `!refreshToken` guard; without it the port
   would re-exchange in a case where the oracle refreshes.
2. **No single-flight.** The oracle guards with `_refreshInProgress` so
   concurrent resolves share one exchange; the port has no such guard.
3. **A locally unexpired but server-rejected XAA token leaves the XAA path.**
   When the first connect used a cached token that Rust still considers valid,
   `registry.rs:927-953` routes a server 401 into `reauth_oauth_spec`.
   `reauth_oauth_spec` (`:1216-1250`) has **no `xaa` branch** and goes to
   refresh-or-interactive-consent. No earlier arm intercepts this case. That
   contradicts the port's own comment at `:1148-1151`: *"when `oauth.xaa` is
   set, XAA is the ONLY auth path — never fall through to the consent flow"*.
4. **A persisted refresh token is never used on the XAA path** (added v10.1).
   `resolve_xaa_token` (`:1332-1337`) reuses an unexpired stored token and
   otherwise re-runs the full IdP + AS exchange, even when the previous exchange
   returned a refresh token that the port itself persisted. The oracle's
   `!n?.refreshToken` guard means that once a refresh token exists it takes the
   ordinary refresh route and only falls back to the XAA exchange when none is
   stored.

Also absent: the three log strings (`XAA: no access_token yet, attempting silent
exchange` / `XAA: access_token expiring, attempting silent exchange` / `XAA
silent exchange failed: <e>`) and the two analytics events `xaa_idp_login` and
`xaa_failed`.

### 26c. Came back clean

- `mcp/src/oauth/callback.rs` — binds `127.0.0.1` only, ephemeral port via
  `listen(0)`, `/callback` route with a `404` for anything else, `state`
  CSRF validation, and `error` / `error_description` handling all match. The
  only deltas are the deliberate LingXi rebrand of the served page copy
  (`… try again from LingXi.`), which is an accepted divergence.
- `mcp/src/mcp_output_storage.rs` — the `mcp-resource-` filename prefix,
  `format_file_size`, the `Binary content could not be saved to disk: …`
  degradation path, and the mime→extension table (pdf/json/csv/txt/html/md/zip/
  docx/xlsx/pptx/doc/xls/mp3/wav/ogg/webm/png/…) all line up with the oracle's
  vocabulary.
- `mcp/src/xaa.rs` + `xaa_idp.rs` — the four Layer-2 operations and the
  Layer-3 orchestrator are present and wired; note that `xaa.rs`'s own residual
  list is **stale** (it says `getXaaIdpSettings`, `acquireIdpIdToken`,
  `discoverOidc` and the keychain `id_token` cache are unported — `xaa_idp.rs`
  ports all four). Only §26b's items remain.
- Checked and **not** gaps: `actor_token` in the oracle belongs to the bundled
  Google auth library's impersonation code, and
  `authorization_details_types_supported` is a field of the AS-metadata schema
  the client only ever reads — neither is an MCP XAA feature.

## §27 · NEW in round 9 — closed open questions and CLI name handling

### 27a. `--mcp-config` entries are tagged `Project` scope `[CODE]`

`apps/cli/src/init.rs:398` parses every `--mcp-config` entry with
`mcp::ConfigScope::Project`, so those servers enter the project approval gate in
`mcp/src/server_gate.rs:119-133` and can sit waiting for an approval the user
never sees a reason for. **Binary-confirmed (round 10 pre-check):** the oracle's `--mcp-config` handler
(@166780019, the block that literally references `["--mcp-config"]`) stamps
`let Tl = {...ws, scope:"dynamic"}` on every entry. So upstream these are
`dynamic`, never `project`, and never approval-gated. This is the surface the
2.1.246 fix — *"Fixed `--strict-mcp-config` sessions prompting to approve `.mcp.json`
servers they would never load, which left background sessions waiting at
startup"* — addresses. Promoted from §21.5.

### 27b. `requiresUserInteraction` is never parsed `[CODE][GREP]`

Control-guarded: `requiresUserInteraction` / `requires_user_interaction` are
`<none>` across `mcp/src` and `tools/mcp/src`. The MCP tool metadata decode
(`mcp/src/client.rs`) already reads `anthropic/searchHint` and
`anthropic/alwaysLoad`, but not `anthropic/requiresUserInteraction`; the surfaced
DTO has no slot for it, and `MCPTool` (`tools/mcp/src/mcp_tool.rs`) does not
override the trait's default `requires_user_interaction() -> false`.

The oracle chain is binary-confirmed in one MCP-tool factory: it reads
`v._meta?.["anthropic/requiresUserInteraction"] === true` (@182519150), returns
that bit from `requiresUserInteraction()` (@182520425), uses it in
`suppressesAlwaysAllowRule()` (@182520462), and returns an `ask` carrying
`suppressAlwaysAllowRule: true` (@182520945). The port has no equivalent
`suppress_always_allow_rule` channel, and
`tui/src/bottom_pane/permission_view.rs` offers "Yes, allow always"
unconditionally. This is the 2.1.246 fix *"Fixed MCP tools marked
`requiresUserInteraction` still offering 'Yes, and don't ask again' in their
permission prompt; the option wrote an allow rule the tool then ignored"*.
Promoted from §21.5.

### 27c. ~~`mcp add` validation errors do not redact invalid/reserved names~~ — **RETRACTED in v10.1**

**Narrowed in round 10, retracted entirely in v10.1.** Oracle 2.1.251 itself
interpolates raw server names in general CLI output: `CYt` / `Unt` build
`No MCP server named "${r}"` directly (@179943206), and production
`mcp remove`, `mcp get`, login and logout paths call those helpers or render the
name directly (@180019468, @180023358, @179944649, @179958365, @179960984).
Rust's equivalent raw rendering is therefore not a general parity gap.

The round-10 "narrow `mcp add` redaction delta" was a misreading of the binary.
The oracle's `addMcpServer` (`XL`, @160897062) throws

```js
throw ft(Error(`Invalid name ${e}. Names can only contain letters, numbers, hyphens, and underscores.`), phr);
throw ft(Error(`Cannot add MCP server "${e}": this name is reserved.`), fhr);
```

where `phr` / `fhr` are the `<redacted>` constants (@160896747). The **raw name
is in the thrown `Error`**; the redacted string is the second argument to the
`ft(Error(detailed), label)` wrapper, which every sibling call site uses to pair
a detailed message with a short PII-free label (e.g.
`ft(Error("Teleport events fetch failed: …"), "Teleport events fetch failed")`
@159245817, `"MCPB manifest: failed to generate MCP server configuration"`
@159485640, `"CreateSession response missing session id"` @160707905). That is
a telemetry/error-grouping label, not user copy. `ft`'s own definition is
chunk-local and was not recovered, so this rests on the call-site shape; but the
user-facing template is unambiguous.

Rust's `validate_mcp_server_name` (`apps/cli/src/commands/mcp.rs:1519-1531`)
already renders the same two raw-name templates byte for byte. **Do not redact
them** — that would introduce a divergence. The only residue is that the port
attaches no sanitized telemetry label to these failures, which belongs with
§20b's tool-listing telemetry, not here.

### 27d. Explicitly out of scope

The 2.1.251 `additionalDirectories` null-byte skip (`permission/src/loader.rs`
converts straight to `PathBuf`) is a permission-loader item, not MCP/plugin.
Recorded here so a future round does not re-discover it as a gap in this
document's scope.

## Suggested implementation order (frozen after round 10)

The order is by reachable boundary/user impact, not by discovery round. Dead
local scaffolding is deliberately separated from parity work.

### Phase A — enforced boundaries and directly reachable behaviour

1. §1 + §2 — per-tool permission policy and the administrative ceiling. These
   are the findings that leave an intended authorization boundary unenforced.
2. §19 — preserve static/helper-minted `Authorization`, classify the real auth
   source, and prevent OAuth from overwriting it.
3. §27b — parse and propagate `anthropic/requiresUserInteraction`, including the
   `suppressAlwaysAllowRule` decision that removes persistent approval.
4. §25c — add the confusable-URL guard, especially the fail-closed allowlist
   match.
5. §27a — tag `--mcp-config` entries as `Dynamic` so explicit servers do not
   enter the project `.mcp.json` approval gate.
6. §23b — add regular-file/size guards and typed diagnostics to ambient
   `.mcp.json` and global-config reads.
7. §21.9 — make versionless cache replacement live-use-aware; never delete the
   shared `unknown` directory while another session uses it.
8. §24c — honour `resource_metadata` in `WWW-Authenticate`.
9. §20a — normalize/reject MCP tool JSON Schema before it reaches the model.
10. §5 + §6 — BOM handling, object-form `commands`, and mixed-array helpers.

### Phase B — protocol and feature completeness

11. §26a + §26b — resource templates, proactive/single-flight XAA refresh
    (gated on `!refreshToken`, refresh-first when one is stored), and the
    XAA-specific 401 retry path.
12. §15 — archive host/IP denylist and `sha256` verification; the scheme half is
    already done.
13. §7 + §8 — policy regex semantics and ingestion-time name/control/bidi/
    impersonation validation.
14. §12 + §9–§11 — discriminated MCP parsing, OAuth child validation, SDK/IDE
    transport shapes, and discovery-cache/comms-role support.
15. §3 + §4 — host-owned and skip-plugin MCP discovery.
16. §13 + §25d — decide the canonical install path, then align both source
    unions and remove the duplicate path; resolve `--sparse` first.
17. §17 + §18 — protocol-era negotiation and connect-error classification.
18. §14 + §16 + §24d — manifest/MCPB completion, command-source guards when
    that source is built, and the `"mcpServers"` alias.
19. §21.4 — make the already-ported failed-server ToolSearch note default-on in
    the same sessions as the oracle.
20. §24b — implement the per-subagent inline `mcpServers` connect/inject/teardown
    ownership chain.

### Phase C — compatibility polish and cleanup

21. §23a — model `allowManagedMcpServersOnly`; today the port is over-strict and
    ignores ordinary-tier allowlists even when managed-only mode is absent.
22. §24a + §25b — delete or quarantine the non-oracle dead blocklist and delete
    superseded `approval.rs`; do not prefer wiring them over the live paths.
23. §27c — **no user-facing change** (retracted v10.1; the port's copy already
    matches). The sanitized `mcp add` failure labels are telemetry and ship with
    §20b in item 24.
24. §20b + §20c + §22 — telemetry and exact copy, including the two `xaa_*`
    events, after behaviour is aligned.
