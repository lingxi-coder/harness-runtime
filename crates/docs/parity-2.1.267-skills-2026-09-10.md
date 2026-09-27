# Skills vs Claude Code 2.1.267

First alignment audit of this subsystem. There has never been one: no prior doc
covers skills, and `grep -rl '2\.1\.26[0-9]' docs/` finds nothing for it.

Oracle: `~/.local/share/claude/versions/2.1.267`, sha256
`a681f3008f0050029aeebcab3af51bb6a55ddeb625a3af3141a4416d43cd2558`, extracted to
`~/.claude/oracle-chunks/2.1.267/` (1657 chunks). Tree at `2d081b3dd`.

🚨 **Every claim below is checked against the executable.** An early pass of
this audit used `claude-code/src/skills/*.ts`, and that mirror is stale enough to
produce wrong findings — it lists `skillify` and `lorem-ipsum` as bundled skills
and both are **0 hits in the 2.1.267 binary**. Skill names survive minification
as string literals, so that is decisive: they do not exist upstream and must not
be ported. Anything sourced from the mirror was re-derived or dropped.

---

## 1. Shape: there are two parsers and two scanners, and the obvious one is not live

| | parses | reaches the model? |
|---|---|---|
| `skill-api/src/{model,frontmatter}.rs` | the FULL key set (21 keys) | **no** — feeds bundled/plugin registration and the `/skills` listing |
| `command-api/src/markdown_loader.rs` | a subset | **yes** — this is what turns `SKILL.md` into an invocable `SlashCommand` |

A skill reaches the model as a `SlashCommand`, through
`load_skill_markdown_files_with_roots` → `load_skill_dir` → `build_skill_command`.
Reading `skill-api` and concluding a key is supported is the trap this subsystem
sets; two of the three defects below were exactly that.

## 2. Source tiers

| tier | upstream | port |
|---|---|---|
| managed / policy | ✅ | ✅ |
| user (`~/<DOT_DIR>/skills`) | ✅ | ✅ |
| project, walking UP to the repo root | ✅ | ✅ `project_dirs_up_to_home` |
| `--add-dir` roots | ✅ | ✅ **fixed in `e63f84d11`** — see §4.2 |
| bundled | ✅ 21 | ⚠️ 11 + 1, see §3 |
| plugin | ✅ | ✅ |
| MCP-derived | ✅ | partial — resolves as `Other` and is rejected by the tool |
| legacy `commands/`-as-skills | ✅ verified (§2.3) | ✅ **present** — via `command-api`, not `skill-api` |
| conditional / `paths:`-activated | ✅ verified (§2.1) | ✅ **ported** (`4c7fb33bd` + `f790dc179`) |

### 2.1 ✅ Conditional (`paths:`) skills — verified, 2026-09-10

The needle the old note asked for, found without relying on symbol names:
`"skill_paths"` is a real pattern-source tag (`Yf` in `src_161574736.js`, beside
`claudemd_rule_globs` / `permission_rules`), and the activator is `lhr`
(`src_163219561.js` @4573642):

```js
function lhr(e,n){ if((g_e()?.conditionalSkills.size??0)===0) return [];
  let r=[];
  for(let[o,d] of PR().conditionalSkills){
    if(d.type!=="prompt"||!d.paths||d.paths.length===0) continue;
    let p=uhr.default().add(y8(d.paths,"skill_paths"));      // gitignore-style
    for(let y of e){ let v=sCt(y)?dhr(n,y):y;
      if(!v||v.startsWith("..")||sCt(v)) continue;
      if(p.ignores(v)){ PR().dynamicSkills.set(MGe(d),d);
        PR().conditionalSkills.delete(o);
        PR().activatedConditionalSkillNames.add(o);
        r.push(o), t(`[skills] Activated conditional skill '${o}' (matched path: ${v})`);
        break } } }
  if(r.length>0) i("tengu_dynamic_skills_changed",{source:_("conditional_paths"),…}) }
```

So the shape is:

* a skill whose frontmatter carries `paths:` is loaded into **`conditionalSkills`**,
  NOT into the listed set — it is invisible until activated;
* when the session touches a file matching any of its patterns
  (**gitignore semantics**, the `ignore` library, same compiler as
  `claudemd_rule_globs`), it moves into `dynamicSkills` and becomes available;
* the move is **one-way and once** (`conditionalSkills.delete`, and the name is
  recorded in `activatedConditionalSkillNames`);
* relative paths escaping the root (`v.startsWith("..")`) are skipped;
* it emits `tengu_dynamic_skills_changed` with `source: "conditional_paths"`, and
  logs `[skills] Activated conditional skill '{name}' (matched path: {path})`.

`paths` is also in the recognized-frontmatter-key union `O` (`src_161416353.js`)
alongside `when_to_use` / `hooks` / `context`, and in the command-metadata shape
next to `whenToUse` — consistent with skills surfacing as commands.

### 2.2 Mechanism landed (`4c7fb33bd`), listing filter still to wire

`skill_api::ConditionalSkills` implements the state machine:
`is_conditional` (non-empty `paths`), `is_available`, and `activate_for_paths`,
which matches gitignore-style through the `ignore` crate — the same crate the
`.worktreeinclude` matcher uses, and reporting through the already-declared
`SITE_SKILL_PATHS` telemetry site. Activation is one-way and once, and a path
that escapes the root is skipped.

⚠️ **Two bugs worth knowing about, both caught by the tests:** `docs/` matched
nothing until the matcher moved to `matched_path_or_any_parents` (plain `matched`
only tests the final component), and a path outside the workspace activated a
repo-scoped skill because the first version fell back to the absolute path when
it could not be made relative.

**Wired end to end (`f790dc179`).** `paths` travels from skill frontmatter onto
the command record (upstream carries it on the command metadata too, beside
`whenToUse`), and both composition roots drop un-activated conditional skills
from the model-facing listing.

🚨 **Activation is remembered, not recomputed.** `read_file_state` is an LRU
capped at 100 entries, so the file that revealed a skill can age OUT of the
touched set. Recomputing availability per turn would make a skill the model has
already been shown disappear again; the state lives with the listing provider for
the session, and a test evicts the matching path and asserts the skill stays.

⚠️ The read-state map is session-owned, not a module global — the first attempt
reached for an accessor that does not exist. It has to be threaded from the root
that creates it (`read_state_map`). ⚠️ The sibling
`tengu_dynamic_skills_changed` `source: "file_operation"` belongs to DYNAMIC skill
discovery (`Dynamically discovered {n} skills from {m} directories`) — a
different mechanism that is also absent; do not conflate them.

### 2.3 ✅ `commands/`-as-skills — verified present, and the old row was misleading

Upstream's loader is at `src_163219561.js` @4565355: it walks the commands dirs
and builds skills with `loadedFrom: "commands_DEPRECATED"`, `paths: void 0`
(so a legacy command is never conditional) and the default description
`"Custom command"`, reporting through `skill_load_commands_dir` /
`skill_load_commands_parse_failed`.

**This port already does it.** `command-api/src/markdown_loader.rs` scans
`.lingxi/commands/**.md` (`SUBDIR = "commands"`, plus the managed dir) and tags
every command `loaded_from: Some("commands_DEPRECATED")`, and both listing
closures admit that tag alongside `bundled` / `skills`.

🚨 The old row said "`LoadedFrom::CommandsDeprecated` declared, never
constructed" and marked the tier absent. The declaration it named is
`skill_api::LoadedFrom`'s variant — a DEAD variant in a parallel type, while the
tier itself lives on `command_api`'s string-valued `loaded_from`. Reading one
type's unused variant as the feature's absence is the same mistake as reading a
0-hit grep for a foreign symbol as proof: the behaviour was never checked. The
dead variant is worth removing on its own, but it is not this feature. Their upstream identifiers
(`loadSkillsFromCommandsDir`, `activateConditionalSkillsForPaths`,
`getDynamicSkills`) are **0 hits in the binary — which proves nothing**, because
minification erases source-level function names. They must be established by a
behaviour needle or a string literal before anyone builds them.

**The single-level `read_dir` is CORRECT — do not "fix" it.** Upstream's scan is
also one level (`skill-name/SKILL.md` only). This is *not* the shape of hole the
agent audit found in the agent-catalog loader, and the two should not be
conflated.

## 3. Bundled skills — the name set

Upstream 2.1.267 registers **21** (`uo({name:…})`, variable names resolved):
`artifact-components`, `batch`, `claude-api`, `claude-in-chrome`, `code-review`,
`dataviz`, `debug`, `design-sync`, `doctor`, `explain-usage`,
`fewer-permission-prompts`, `keybindings-help`, `loop`, `memory-types`, `run`,
`run-skill-generator`, `setup-claude`, `update-config`, `whiteboard`,
`workflow-authoring`, `workshop`.

The port registers **17** through `register_bundled_skills` (11 + `update-config`,
`keybindings-help`, `explain-usage`, `workflow-authoring` and `checkup`, added 2026-09-10), plus `claude-api` as the `skill-api`
compiled-in builtin.

| in both (14) | LingXi-only (4) | upstream-only (7) |
|---|---|---|
| batch, claude-api, code-review, dataviz, debug, doctor (as `checkup`), explain-usage, fewer-permission-prompts, keybindings-help, loop, run, run-skill-generator, update-config, workflow-authoring | cron, deep-research, simplify, verify | artifact-components, claude-in-chrome, design-sync, memory-types, setup-claude, whiteboard, workshop |

### The remaining nine, adjudicated one by one (2026-09-10)

Every row below was checked at the binary. **Two were misclassified** by the
blanket "they ride surfaces this port does not have":

| skill | verdict |
|---|---|
| `memory-types` | 🔒 **DORMANT upstream** — `var Jxn="memory-types"` sits next to `function Qxn(){return H("tengu_ochre_finch",!1)}`, its `isEnabled`. Flag defaults **false**, so absence here is ALIGNMENT, not a gap. Same shape as `melodic_wolf` / `lively_waffle`. |
| `workflow-authoring` | ✅ **LANDED 2026-09-10** — see §2.4. The three reasons previously recorded for not landing it were all artefacts of a truncating extractor; 27 of 31 paragraphs survive byte-identical and both divergence anchors survive verbatim. |
| `debug` | ✅ **LANDED 2026-09-10.** 原阻塞成立但**可以修**：`memory::retention` 一直在清扫 `<config-home>/debug/` 并保留 `latest`，却从来没人写过那里。补上文件 sink（`apps/cli/src/logging.rs`）后技能就有真东西可读。⚠️ 两处适配：端口的 subscriber 在启动时装好，**不能中途开启**日志，所以提示直说「本次没开 `--debug`」而不是谎称刚刚启用（⛔ 否则模型会去找永远不会出现的条目）；上游第 3 步建议调 `claude-code-guide` 子代理，那是已记录的 divergence，整步删掉。 |
| `setup-claude` | ⛔ `isEnabled:()=>a.CLAUDE_CODE_ENTRYPOINT==="remote_cowork"`，且正文是另一个 chunk 的 `SETUP_COWORK_PROMPT`。**2026-09-10 复核仍成立**：`remote_cowork` 在端口全树 0 命中，移植它等于注册一个**永远关着**的 cowork 引导流程。 |
| `artifact-components` | 🔒 **2026-09-10 三次复核，结论变了：这是「缺席即对齐」，不是缺口。** 门是 `tengu_cobalt_plinth`，**上游和端口都默认 `false`**（端口 `tool_api::artifact_gate::is_enabled` → `telemetry::flag_bool(COBALT_PLINTH_FLAG, false)`）。⇒ 上游用户也看不到这个技能，端口不注册它**对可见行为零影响**——和 [[memory-types]] 同一个形状。⚠️ 真要打开还差两层，且**第二层比第一层大得多**：(1) bundled 技能带附件文件的机制（端口只支持 `include_str!` 单体）；(2) Artifact 工具本身是 register-but-disabled 骨架，`call()` 返回如实的「not wired」，publish/list 管线是 Stage-2。⇒ 先动技能是本末倒置。 |
| `whiteboard`, `workshop` | ⛔ same Artifact family — they sit in one name block with `artifact-design` / `artifact-diagramming` / `artifact-capabilities` / `prototype`. |
| `design-sync` | ⛔ pushes a design system to claude.ai/design; `isEnabled:MF` plus a `policyGate`. No Design surface here, and the destination is a claude.ai service. |
| `claude-in-chrome` | ⛔ needs the Chrome extension. |

So of the original 13: **6 ported** (`update-config`, `keybindings-help`,
`explain-usage`, `workflow-authoring` §2.4, `doctor`→`checkup` §2.5, `debug`),
1 dormant upstream (`memory-types`), 1 alignment rather than a gap
(`artifact-components` — its gate defaults false in BOTH builds), and
**5 ACCEPTED DIVERGENCES, not backlog** (below). 6+1+1+5 = 13.

### ⛔ The last five are Anthropic-service divergences, not unbuilt features

2026-09-10, user decision. These were carried as "blocked on a product surface",
which reads as work owed. It is the wrong classification: every one of them
terminates at an **Anthropic-operated service**, and LingXi is a multi-provider
product whose service is not Anthropic. There is no version of "build the
surface" that does not mean wiring this port into Anthropic's account system.

| skill | terminates at |
|---|---|
| `whiteboard`, `workshop` | the **claude.ai** publish backend (`tools/ui/src/artifact.rs`: "a default-private claude.ai web page") |
| `design-sync` | **claude.ai/design** — upstream's own menuDescription is "Push your design system components to claude.ai/design" |
| `claude-in-chrome` | the **Claude** Chrome extension |
| `setup-claude` | `CLAUDE_CODE_ENTRYPOINT === "remote_cowork"` — Anthropic's cowork product |

This is the same standing decision already recorded for the Anthropic
backend/remote surface generally. ⛔ Do not re-open these as parity gaps; they
belong in `lingxi-accepted-divergences`, and a future audit that lists them as
"missing" has mis-scoped, not found something.

`artifact-components` is *not* one of the five — it is settled one row up by a
gate that is false in both builds, which is a stronger disposition and does not
depend on this decision. It shares the claude.ai backend all the same, so if the
gate ever flips, it lands here.

⚠️ **Superseded (kept for the trail).** This section used to close by saying
the 13 "are not a backlog" and naming `update-config`, `keybindings-help`,
`explain-usage` and `doctor` as the ones worth triaging first. All four have
since landed, and the rest are adjudicated above — so the sentence is no longer
a to-do list. ⛔ Do not read it as one.

### ✅ `update-config` (`29a8e64a4`) and `keybindings-help` (`f7fb8840a`) — ported

Both were recorded as needing "dynamic-prompt plumbing plus a branding decision".
**Neither blocker was real.** `SlashCommandKind::Bundled` already carries a
`prompt_fn`, and the `branding` crate already fixes the product name, config dir
and env prefix — there was nothing left to decide.

What makes both worth having is the same property: their prompts carry LIVE data
rather than prose.

* `update-config` injects the settings schema generated from the very
  `SettingsJson` the loader parses (`schemars::schema_for!`), so it cannot
  describe a key the loader would reject. Both prompt shapes are ported,
  including the `[hooks-only]` one that swaps the entire prompt and carries no
  schema.
* `keybindings-help` builds its contexts / actions / reserved tables from
  `command_core::keybindings` — the same tables the validator uses. The action
  column INVERTS the default-binding table, so a moved default shows its new key
  with no prose to update. `userInvocable:!1` is kept verbatim: model-invocable
  only, the user route stays `/keybindings`.

⚠️ Both bodies are the binary's, rebranded (`Claude Code`→`LingXi`,
`.claude/`→`.lingxi/`, `claude --debug`→`lingxi-cli --debug`), with a test per
skill asserting none of those strings survive.

🚨 The name-set lock caught BOTH additions, and caught `keybindings-help` going
in out of alphabetical order (the list is compared sorted). Update it
deliberately; do not re-bless it.

### ✅ `explain-usage` (`724b9e52b`) — ported

A single-prompt skill, so the only judgement in it is what NOT to rebrand:
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}` becomes the LingXi pair, but
`mcp__claude-in-chrome__` stays **verbatim** — that is an MCP server id on the
wire, not a product reference, and rewriting it would point the skill at a
server that does not exist. A test pins each direction.

Also pinned: the line telling the model to treat transcript contents as data
rather than instructions. It is the skill's only defence against a transcript
that contains instruction-shaped text.

### ✅ `doctor` — RESOLVED 2026-09-10: landed as `/checkup` (see §2.5)

Upstream's `doctor` is a bundled SKILL (`uo({name:"doctor",aliases:["checkup"],
survivesBundledKillSwitch:!0,requires:{workspace:!0},terminalOriented:!0,…})`) —
an LLM-driven health check that reads local data and proposes fixes.

**This port already ships `/doctor`**, but as a deterministic builtin command
handler (`commands/core/src/doctor.rs`) rendering a LOCKED report, plus the
`lingxi-cli doctor` subcommand. Registering a bundled skill under the same name
would collide with that builtin registration.

The open question this section posed — "the deterministic report, the
model-driven one, or both under distinct names" — was **decided by the user on
2026-09-10: both, under distinct names.** Upstream's skill now ships as
`/checkup`, which is upstream's OWN alias for it, so the port neither collides
with the builtin `doctor` nor invents a name. Full write-up, including the two
Anthropic-distribution checks that were cut, in §2.5.

⛔ The original warning still stands and is why this landed as `checkup`: do not
register a second `doctor`. Same shape as the `commands/`-as-skills row above —
a feature judged absent because one mechanism was missing, when another already
covers part of it.

⚠️ **A near miss worth recording.** `stuck` was reported out of this audit as a
fourth portable name. It is not a bundled skill — it has ~50 occurrences in the
2.1.267 binary and **no `uo({name:"stuck"` registration**; they are the English
word. The list above never contained it, because it was built from
registrations; the claim came from reading a substring count as if it were one.
🚨 A bundled name is only a bundled name when a registration says so — literal
`uo({name:"…"})` or a resolved variable. `skillify` and `lorem-ipsum` fail the
same test from the other direction (present in the stale TS mirror, absent from
the binary).

**Now locked.** `the_bundled_skill_name_set_is_locked`
(`commands/core/src/bundled/mod.rs`) pins the set in BOTH directions — it
enumerates `list_all()` rather than filtering a hardcoded list, so an addition
fails as loudly as a removal — and separately asserts that five
surface-dependent upstream names stay absent. Nothing pinned this before: the
per-skill tests each check one skill, so gaining or losing a whole skill changed
no assertion. Assert names, never a count.

## 4. Defects found and fixed

### 4.1 `user-invocable: false` was a no-op — `e6d65a403`

`build_skill_command` hardcoded `user_invocable: Some(true)`, and the production
frontmatter reader never parsed the key at all. Upstream:
`let dt=v["user-invocable"], en=dt===void 0?!0:htt(dt)` with
`htt(e)=c1(e)??!1` — absent means invocable, and a value that coerces to neither
boolean set means `false`. Load-bearing on two surfaces that filter on it
(`CommandRegistry`'s listing and the TUI slash menu), so a skill asking to be
hidden now is.

### 4.2 The `--add-dir` tier was plumbed and never populated — `e63f84d11`

All three desktop registration sites passed `Vec::new()`. Wiring it naively
would still have found nothing: upstream joins each root with `.claude/skills`
(`for(let e of Up()){ let S=P.join(e,".claude","skills"); … }`,
`src_172414592.js` @5180) while the port's loader takes already-resolved skill
dirs. This repo already agreed — the repo-root reload path has always built
`root.join(branding::DOT_DIR).join("skills")`. Only initial registration was
empty. Desktop-only; engine-mobile has no `add_dir` concept.

### 4.3 A skill's declared `effort` never reached its fork — `2d081b3dd`

`ForkedSkillScoping.effort` exists, validates against a faithful port of
`union([enum(low|medium|high|xhigh|max), int().min(1).max(1000)])`, is persisted
beside the fork's transcript and replayed on resume — and was **always `None`**,
because nothing above it parsed the key. Upstream's generic skill→command
builder carries it (`wXe({… effort: De …})`, `src_163219561.js` @4555866) and
the scoping spreads it in conditionally.

The raw carrier is an untagged enum because `effort: high` arrives as YAML text
and `effort: 500` as an integer. Conversion is deliberately lenient, mirroring
`Gx`: outside the union yields `None`, so a typo means "declared no effort" and
the fork still launches.

## 5. ⛔ Seven keys that are NOT worth porting the way they look

`version`, `hooks`, `paths`, `metadata`, `created_by`, `improved_by`,
`hide-from-slash-command-tool` are absent from the production reader — and
**already parsed, with tests, in `skill-api`, where nothing reads any of them**
(`skill-api/src/model.rs:39` labels one "P2 gap" in so many words).

Adding them to `CommandFrontmatter` would produce a second parsed-and-unread
copy: the exact "named, computed, never wired" shape this audit exists to find,
doubled. **Build the consumer first, then the plumbing.** `effort` was the one
member of this group with a live consumer, which is why it is in §4 and these
are not.

## 5b. The two portable bundled skills — sized

`update-config` and `keybindings-help` are the two upstream-only names with no
substrate blocker, and both are genuinely LIVE upstream:

| | gate | user-invocable |
|---|---|---|
| `update-config` | none — always on | yes |
| `keybindings-help` | `OF(){return H("tengu_keybinding_customization_release",!0)}` — default **true** | **no** (model-only) |

⚠️ **Neither is a quick win, and an earlier note here implied otherwise.** Both
carry a DYNAMIC prompt: `getPromptForCommand(e)` assembled from live host state,
not a static body. `update-config` additionally branches on a `[hooks-only]`
prefix; `keybindings-help` composes roughly nine sections plus the session's
actual keybindings. The port has the substrate for this (`dynamic_body` /
`BundledPromptFn` on `SkillDescriptor`), so it is a port rather than an
invention — but it is a feature each, not an afternoon.

They also both name Claude Code and `~/.claude/` paths in copy the model reads,
so porting them lands on the established branding divergence and needs the
LingXi rebrand rather than a byte-exact copy. That is a decision to take
deliberately, not incidentally.

## 6. Gates

- `scripts/check_skill_frontmatter.py` is a real fail-closed gate (name == dir,
  description ≤ 180 display columns, frontmatter parsable) but `SKILL_ROOTS`
  covers only `plugins/lingxi-local-app/skills` — not the bundled skills and not
  `<DOT_DIR>/skills`. Widening it is cheap and unclaimed.
- No fixture anywhere pins bundled skill BODIES against the oracle. The
  `*_body.md` files are `include_str!`-ed and never diffed.

## 7. Open, with blockers

| item | blocker |
|---|---|
| conditional / `paths:`-activated skills | ⛔ establish the behaviour at the binary first; the symbol-name greps prove nothing |
| legacy `commands/`-as-skills tier | same |
| the 13 upstream-only bundled skills | most need a surface this port does not ship; triage `update-config` / `keybindings-help` / `explain-usage` / `doctor` first |
| MCP-derived skills | resolve as `Other` and are rejected by the tool |
| the seven unread keys (§5) | need a consumer before they need a parser |


## 2.4 `workflow-authoring` — LANDED 2026-09-10

Commits: `64ca3fc2e` (the split + machinery), `8568a51a9` (the skill),
plus the reachability gate.

**What shipped.** The tool description is now 2.1.267's 3,031-byte head; the
17,141-byte authoring reference moved to `workflow_authoring_skill.txt`, served
by the `workflow-authoring` bundled skill. `assemble_description` is upstream's
`Epn`: it emits the head plus a one-line pointer when the skill is loadable, and
the head plus the whole reference when it is not.

### The three reasons recorded here for NOT landing it were all wrong

They are left in place, because each was a plausible reading of bad evidence and
the way each failed is the reusable part. **All three traced to one cause: the
extractor scanned for a closing backtick and truncated at 3,500 chars.**

**1. "It is a REWRITE — only 2 of 31 paragraphs survive."** The comparison was
against the 3 KB HEAD alone. Against both halves, **27 of 31 survive
byte-identical**. The four real deltas are three cross-references the split
itself required (`"below"` → `"in the workflow authoring reference"`,
`"above"` → `"in the Workflow tool description"`, `"(example below)"` →
`"(the review-changes example)"`) and one added sentence about schemas.
🚨 A fifth apparent delta, `×` vs `\xD7`, was the extractor: it unescaped
`\uXXXX` but not `\xNN`, which the binary also uses. The byte-lock now rejects
both spellings.

**2. "The fusion anchor occurs 0 times in .267."** It occurs **exactly once** —
past the truncation point. Both registered divergences survive verbatim, each in
the correct half (`local-app-create-handoff` in the description, the fusion hook
in the reference), and **neither needed re-anchoring**. The recommendation this
section made — move `fusion()` inline into the trimmed description — would have
separated it from the hook list it belongs to for no reason.

The reachability invariant is real, but the fix is upstream's own: `Epn` keeps
an inline branch, so a build that cannot load the skill still gets every hook.
That is now asserted directly
(`the_inline_branch_documents_every_script_body_hook`) rather than implied.

**3. "The skill body is not a static string."** True, and it was the one finding
that held — but it is three `${e?"":"…"}` fragments on
`CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, which this port already reads. Stored as the
unforced text plus a subtraction list, each fragment required to match exactly
once so a stale fragment cannot silently subtract nothing.

⚠️ The generalisable error: **the conclusion "this is a rewrite" and the
conclusion "the anchor is gone" were both produced by an instrument that had
silently stopped reading.** Neither was re-checked against a second method. A
truncating parser does not announce itself — it returns a shorter string that
looks complete. See [[an-exit-code-is-not-evidence]]: the extractor exited 0.

### How the gate is expressed

Upstream's `nre(tools)` is six session conditions plus a per-call check that the
`Skill` tool is advertised. This port has no substrate for three of them
(`disableBundledSkills`, `skillOverrides`, the session skill allowlist), so it
answers the question those conditions ask rather than reproducing them: the
registrar publishes that the skill exists, the wire-schema build publishes
whether this request advertises `Skill`, and both must hold. The
wire-schema cache is keyed on the result, so an entry built before the registrar
ran cannot outlive it.

⚠️ Divergence: upstream gates registration on `isEnabled: () => qc()`.
`register_bundled_skills` never receives the managed workflow-disable setting,
and gating on a predicate the registration site cannot see risks the one state
the invariant forbids — the skill absent while the description claims it is
loadable. Registered unconditionally instead; a reference readable while
workflows are off is inert.

## 2.5 `doctor` — LANDED 2026-09-10 as `/checkup`

Commit `e8da94ffa`. Upstream registers `doctor` with `aliases:["checkup"]`,
`terminalOriented:!0`, `disableModelInvocation:!0`. This port already ships a
`/doctor` that is a **different thing**: a deterministic command whose
`DoctorReport` DTO the GUI clients render as a screen (`doctor_report_parity`
in `client-adapter`). Both wanted the name; neither subsumes the other. User
decision 2026-09-10: keep `/doctor`, land upstream's under its own alias.

**Two of ten checks cut**, both Anthropic-DISTRIBUTION diagnostics already
excluded by the accepted divergences:

| cut | why |
|---|---|
| Check 7 (version currency), whole | `npm view @anthropic-ai/claude-code`, `downloads.claude.ai/claude-code-releases`, `claude-code` Homebrew casks, `claude update` — LingXi ships through none of them |
| Check 0's first two bullets | enumerate `~/.local/bin/claude`, npm-global `@anthropic-ai/claude-code`, `installMethod` |

Check 0's other three bullets (unparseable settings, broken/colliding agent
definitions, malformed skill frontmatter) map exactly and are kept, as are
checks 1-6 and 8-9.

🚨 **Cutting a numbered check moves every cross-reference to it** — the report
format's check list, the "checks 0 and 7" command note, the consolidated-cleanup
gate, and the data-sources header, which advertised check 7 as the one permitted
network call. Prose has no compiler, so
`the_check_numbering_is_self_consistent` pins the heading set, the absence of
any "check 7" reference, and the actionable-check list together.

⛔ `mcp__claude_ai_<connector>__` is deliberately NOT rebranded — it is the wire
prefix for claude.ai connectors. Rebranding it would stop the model matching
real transcript entries **while every branding assertion still passed**, so it
has its own test, red-proofed separately.

**Substrate verified rather than rebranded on faith:** skill usage is
`~/.lingxi/skill_usage.json` (a file, not a key in `~/.claude.json`); transcripts
`~/.lingxi/projects/<cwd>[-<djb2>]/*.jsonl`; `MAX_MEMORY_CHARACTER_COUNT` in
`memory/src/lib.rs`; `lingxi-cli plugin validate` / `mcp remove`.
⚠️ `pluginUsage` has NO counterpart here — plugin guidance rests on transcript
evidence, which is upstream's own fallback for zero-count plugins.
⚠️ Upstream's `progressMessage:"running checkup"` is unwired: `SlashCommand`
carries no such field.

