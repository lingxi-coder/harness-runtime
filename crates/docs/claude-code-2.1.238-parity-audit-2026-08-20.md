# Claude Code 2.1.238 byte-level parity audit — 2026-08-20

The port declared `platform_api::CLAUDE_CODE_VERSION = "2.1.220"`. The newest oracle is **2.1.238**,
so this audit covers **18 releases of drift**. The 2.1.220 native binary was pulled from npm
(`@anthropic-ai/claude-code-darwin-arm64@2.1.220`) so every finding could be classified as
NEW upstream drift versus a long-standing gap, rather than guessed.

## Scope, as confirmed by the user

**Excluded — all Anthropic-backend / remote surface:** Remote Control, cloud sessions,
`--teleport`, the self-hosted runner, `RemoteTrigger`, `ReadNotifications`, `Poll`, the
`Artifact` tool (publish/assets/comments/capabilities), Claude Design / `DesignSync`,
artifact watching, `SearchMcpRegistry`/`SuggestConnectors`/`ListConnectors`, the org-memory
`memory_*` tools, and Cowork onboarding.

**Excluded — LingXi-intentional divergences:** multi-LLM provider support, LingXi's own
WebSearch, crossreview, mobile (iOS/Android) surfaces, local-apps, `.lingxi`/`LINGXI_*`
branding, and the deliberately removed `claude-code-guide` / `claude` builtin agents.

**Included:** additions *and removals* — where 2.1.238 deleted something, the port should
delete it too unless it is a LingXi feature or an accepted divergence.

## Method

Twelve subsystem auditors read the oracle binary directly (a shared `oracle.sh` helper resolves a
literal to a byte offset and dumps readable context), compared it against the Rust source, and each
was then put through an adversarial verifier whose default verdict was *refuted*. Only findings that
survived refutation are listed as confirmed.

Two method rules were enforced because both have produced false findings in past sessions:
a zero-hit grep for an oracle symbol name is **not** evidence of a missing feature (grep the ported
*behaviour*), and oracle strings are frequently stored as interpolated fragments — `Claude-User
(claude-code/…)` returns zero hits in **both** binaries for exactly that reason, and is unchanged.

## Totals

| | count |
|---|---|
| confirmed findings | **140** |
| &nbsp;&nbsp;P1 | 47 |
| &nbsp;&nbsp;P2 | 61 |
| &nbsp;&nbsp;P3 | 32 |
| &nbsp;&nbsp;*missing-in-port* | 64 |
| &nbsp;&nbsp;*copy-drift* | 42 |
| &nbsp;&nbsp;*behavior-drift* | 19 |
| &nbsp;&nbsp;*schema-drift* | 11 |
| &nbsp;&nbsp;*port-extra* | 2 |
| &nbsp;&nbsp;*removed-upstream* | 2 |
| refuted by verification | 4 |

Plus 10 tool-registry findings (TR-01…TR-10) whose verifier died on a connection error; they are
listed separately below and are marked UNVERIFIED.

## Coverage by subsystem

| subsystem | raw | confirmed |
|---|---|---|
| Tool registry, names, order and enablement gating | 10 | 0 |
| Bash tool + BashOutput/KillShell contract | 19 | 19 |
| Read / Write / Edit / MultiEdit / NotebookEdit tool contracts | 10 | 10 |
| Glob / Grep tool contracts | 15 | 14 |
| Task/Agent tool + subagent framework | 15 | 15 |
| Main system prompt | 10 | 10 |
| system-reminder family and interstitials | 15 | 15 |
| Slash command registry | 15 | 13 |
| CLI argv surface | 17 | 17 |
| Settings schema + hooks | 7 | 7 |
| Permission engine | 9 | 8 |
| Session persistence, resume and compaction | 12 | 12 |

## Findings

### Bash tool + BashOutput/KillShell contract

#### `BASH-01` — CONCISE Bash prompt gates "Command output is displayed to you" on Opus5; 2.1.238 emits it unconditionally

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:670-676`
- **oracle** 2.1.238 binary @132246944: `- Command output is displayed to you, not reliably to the user.` emitted as a BARE array element in `hcT` (cc-238.js @231053238): `return["Executes a bash command and returns its output.",...u?["",u]:[],"",a,...s,"- Command output is displayed to you, not reliably to the user.",...c,...]`. 2.1.220 had it GATED: str-220.txt @6073966 `let c=KFc(t)?["- Command output is displayed to you, not reliably to the user."]:[]` with `function KFc(e){return SQt(Z.CLAUDE_CODE_MARL_CORMORANT,Xcg,e)}`. CLAUDE_CODE_MARL_CORMORANT is in DELTA-env-gone.txt:7 and `grep -c MARL_CORMORANT str-238.txt` = 0. The builder also lost its model parameter: `$ry(e,t)` (220) -> `hcT(e)` (238).
- **fix** In `simple_prompt_concise`, remove the `model.is_some_and(has_capability(Opus5PromptBundle))` guard and push "- Command output is displayed to you, not reliably to the user." unconditionally in the same position (after the IMPORTANT-avoid bullet, before the `timeout` bullet). Update the golden test at prompt.rs:880.
- **verifier** Confirmed at both oracles. cc-238.js @231053238 `hcT` emits "- Command output is displayed to you, not reliably to the user." as a bare array element between `...s` and `...c`; cc-bin-220 @235679500 `$ry(e,t)` had `let c=KFc(t)?[...]:[]` and took a model arg. MARL_CORMORANT: 1 hit in str-220, 0 in str-238. Port prompt.rs:670-676 gates on Opus5PromptBundle, and that gate is NOT vacuous — traits/src …[truncated; full text in the subsystem report]

#### `BASH-02` — VERBOSE Bash prompt ships a 5-line "When issuing multiple commands:" block that exists in no oracle build

- **severity** P1 &nbsp;·&nbsp; **kind** *port-extra* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:489-495 (the vec) and :527-528 (the two pushes)`
- **oracle** The oracle's verbose instruction list `u` (cc-238.js @231058108, identically 2.1.220 str-220.txt @23411877) is: [ls-check, quote-paths, maintain-cwd, timeout, ...backgroundNote, "For git commands:", i, "Avoid unnecessary `sleep` commands:", a, ...findAdvice] - no such item. Literal counts (220/238): `send a single message with two` 0/0; `call with '&&' to chain them together` 0/0; `DO NOT use newlines to separate commands (newlines are ok in quoted strings)` 0/0; `Use ';' only when you need to run commands sequentially` 0/0. The lone `When issuing multiple commands:` hit is 2.1.238 binary @131519896 and belongs to the POWERSHELL prompt with different wording: "  - When issuing multiple commands:\n    - ...chain them in a single <PS> call (see edition-specific chaining syntax above).\n    - Use `;` only when you need...\n    - DO NOT use newlines to separate commands (newlines are ok in quoted strings and here-strings)".
- **fix** Delete `multiple_commands_subitems` (prompt.rs:489-495) and the two `instruction_items.push(...)` calls at prompt.rs:527-528 so `For git commands:` follows the background-usage note directly.
- **verifier** Read the whole `Yhm` instruction array (cc-238.js @231055500-231058108): ls-check, quote-paths, maintain-cwd, timeout, background note, "For git commands:", git subitems, "Avoid unnecessary `sleep` commands:", sleep subitems, optional find advice. No multiple-commands item. `call with '&&' to chain them together` = 0 hits in BOTH 220 and 238. The 2 hits of `When issuing multiple commands:` in 220 …[truncated; full text in the subsystem report]

#### `BASH-03` — New 2.1.238 `backgroundEndsWithFinalResponse` field and its model-facing reaped-at-final-response sentence are missing

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/bash.rs:1627 and :1785 (both hardcode "...You will be notified when it completes. To check interim output, use Read on that file path."); `grep -rn backgroundEndsWithFinalResponse --include='*.rs'` over the workspace = 0 hits`
- **oracle** New output-schema field, 0 hits in str-220.txt / 6 in str-238.txt. Binary @247420288: `backgroundEndsWithFinalResponse:At(!0).optional().describe("True when this backgrounded command is owned by a synchronous subagent and is therefore terminated when that agent gives its final response; absent when the command survives (main loop, async subagents)")`. Set as `let Z=_.backgroundTaskId!==void 0&&wKo(t.agentContext)?!0:void 0` with `function wKo(e){return e!==void 0&&e.agentType==="subagent"&&e.isAsync===!1}` (cc-238.js @220303786). Threaded as `reapedAtFinalResponse` into `L0i` (cc-238.js @226484194), which at binary @289989687 emits: "If it exits while you are still working you will be notified, but it is terminated when you give your final response and no notification can follow that — so do not end your turn to wait for it; if you need its result, wait for it before giving your final response." in place of "You will be notified when it completes." (0 hits for that sentence in 2.1.220). `L0i` also added a trailing period to the manual-background sentence (238: `...Output is being wri …[truncated; full text in the subsystem report]
- **fix** Thread the agent context's (agentType==subagent && !isAsync) flag into BashTool::call; set `backgroundEndsWithFinalResponse: true` in `bash_result_data` when a backgroundTaskId is present and the caller is a synchronous subagent; factor the note into an L0i-shaped builder that picks the reaped sentence over "You will be notified when it completes." Add the missing trailing period on the manual-background variant while there.
- **verifier** Confirmed. `backgroundEndsWithFinalResponse` 6 hits str-238 / 0 str-220; describe text read at binary @247420288. `wKo` @220303786 and the set site at cc-238.js @231077600 match the quote. `L0i` @226484194 read verbatim including the reaped sentence and the new trailing period on the manual-background variant (220 @235697826 has `...written to: ${y}` with no period). Port has 0 hits for the field, …[truncated; full text in the subsystem report]

#### `BASH-04` — CONCISE prompt hardcodes the SHORT avoid-list while VERBOSE hardcodes the LONG one; the oracle reads one gate for both

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:660 (concise, SHORT list, commented "NOT config-gated") vs prompt.rs:487 (verbose, LONG list)`
- **oracle** 2.1.238 concise `hcT` (cc-238.js @231053238): ``let d=VH()?"`cat`, `head`, `tail`, `sed`, `awk`, or `echo`":"`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo`";``. 2.1.238 verbose `Yhm` (@231058108): ``let r=VH(), ... o=r?"`cat`, ...":"`find`, `grep`, `cat`, ..."``. Identical single gate in 2.1.220 (`aL()`; str-220.txt @6073966 and @23411877). Long-list literal at binary @132245312. `function VH(){if(!Un("true"))return!1;if(yNs())return!1;return V.CLAUDE_CODE_ENTRYPOINT!=="local-agent"}` (cc-238.js @224426328).
- **fix** The port ships Glob and Grep as real tools (test-harness/src/parity/fixtures/registry_42_tools.json), i.e. the non-embedded branch that the verbose prompt already takes. Change the concise `avoid_commands` at prompt.rs:660 to "`find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo`" and correct the comment claiming the list is not config-gated.
- **verifier** Confirmed. Concise `hcT` and verbose `Yhm` both read the SAME `VH()` gate for the avoid-list (cc-238.js @231053238 and @231058108; `aL()` identically in 220). `VH()` @224426328 also feeds `FNt()`→`new Set([Bm,Am])`, the Glob/Grep EXCLUSION set, so VH()==true means Glob/Grep are unregistered; LingXi registers both, hence the LONG list is correct for both builders. Port prompt.rs:660 hardcodes the S …[truncated; full text in the subsystem report]

#### `BASH-05` — Sandbox-section bullet says "This will prompt the user for permission" instead of the oracle's permission-gate wording

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:316`
- **oracle** 2.1.238 binary @132233600 (also @132234064), inside `Khm()` (cc-238.js @231049336) and byte-identical in 2.1.220: `This goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)`. `grep -c 'This will prompt the user for permission'` = 0 in both str-238.txt and str-220.txt.
- **fix** Replace the literal at prompt.rs:316 with "This goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)".
- **verifier** Confirmed. `This goes through the permission gate (a user prompt, or the auto-mode classifier when auto mode is active)` = 2 hits in BOTH 2.1.238 and 2.1.220; binary @132233600 shows it as the third sub-bullet of "When you see evidence of sandbox-caused failure:", exactly the slot the port fills at prompt.rs:316 with "This will prompt the user for permission" (0 oracle hits in either version). Lon …[truncated; full text in the subsystem report]

#### `BASH-06` — staleReadFileStateHint - the "[This command modified N files you've previously read: ... Call Read before editing.]" note is never emitted

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent - `grep -rn "Call Read before editing|This command modified" --include='*.rs'` over lingxi-code = 0 hits. Only the eviction half is ported: lingxi-code/tools/shell/src/bash.rs:860-877 `invalidate_written_read_state``
- **oracle** 2.1.238 binary @247789842 for `Call Read before editing.` (2 hits in both 220 and 238). Built in the Bash `call` (cc-238.js @231067780 region): ``te=`[This command modified ${re.length} ${Et(re.length,"file")} you've previously read: ${fe}${ne}. Call Read before editing.]` `` where `re=await OcT(e.command,t.readFileState,i)` (cc-238.js @231061224, mtime-bumped readFileState entries when the `PcT` WRITE_COMMAND_MARKERS regex matches), returned as `staleReadFileStateHint` and appended to the MODEL content in mapToolResultToToolResultBlockParam: `content:[h,g,y,p,f].filter(Boolean).join("\n")` with `p` = this hint. Output-schema description: "Model-facing note listing readFileState entries whose mtime bumped during this command (set when WRITE_COMMAND_MARKERS matches)".
- **fix** After a foreground, non-image, non-background Bash call, diff `ctx.read_file_state` mtimes against the run start; if any bumped, append `[This command modified {n} file(s) you've previously read: {first 5 cwd-relative paths, ", "-joined}{ and N more}. Call Read before editing.]` as an extra line of the model content, ordered after the background note and before the gh rate-limit hint.
- **verifier** Confirmed. cc-238.js @231077600 builds the hint verbatim (guarded by `!interrupted && !isImage && !backgroundTaskId`) from `OcT` (@231061224, WRITE_COMMAND_MARKERS `PcT` + mtime bump), returns it as `staleReadFileStateHint`, and joins it into the MODEL content at @231072400 (`[h,g,y,p,f].filter(Boolean).join("\n")`). Present in 220 too (2 hits both versions). Port has 0 hits for either literal; ba …[truncated; full text in the subsystem report]

#### `BASH-07` — ghRateLimitHint system-reminder is never emitted

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent - `grep -rn 'GitHub API rate limit exceeded' --include='*.rs'` over lingxi-code = 0 hits; no rate-limit detection anywhere in tools/shell/`
- **oracle** 2.1.238 binary @113553697 (2 hits in both 220 and 238), built by `ikf` (cc-238.js @226556032): ``return r.backoffUntil=Date.now()+Y_v,"<\system-reminder>GitHub API rate limit exceeded (5,000/hr shared across all tools and agents). Run `gh api rate_limit --jq .resources` and sleep until reset before further gh calls. If polling in a loop, use ScheduleWakeup instead of retrying.<\/system-reminder>"``. Wired as `let q=_.backgroundTaskId?void 0:ikf(e.command,S,t.toolState.get(y7a))` -> result field `ghRateLimitHint` -> appended to the model content as `f` in `[h,g,y,p,f].filter(Boolean).join("\n")`.
- **fix** Add a per-session backoff cell to the Bash tool state; when the command matches a gh invocation and the output matches a GitHub rate-limit error and `now >= backoff_until`, append the oracle system-reminder verbatim to the model content and set `backoff_until = now + Y_v`.
- **verifier** Confirmed. The system-reminder string is 2 hits in both 220 and 238; cc-238.js @231077600 wires `ghRateLimitHint` (suppressed for background launches) into the model-content join as `f`. Port: 0 hits for the string and no rate-limit detection in tools/shell. Note the reporter under-rated this — it reaches the model, so P1 would be defensible — but under-rating is not grounds for refutation.

#### `BASH-08` — Bash input-schema property order swaps `description` and `run_in_background`

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/bash.rs:1270-1298 - order is command, timeout, run_in_background, description, dangerouslyDisableSandbox`
- **oracle** `Qhm` (cc-238.js @231067780) declares, in order: command, timeout, description, run_in_background, dangerouslyDisableSandbox, _simulatedSedEdit. Confirmed by sdk-tools-238.d.ts:578-609 (BashInput: command; timeout; description; run_in_background; dangerouslyDisableSandbox). Identical order in 2.1.220 and sdk-tools-220.d.ts (sdk-tools-220-238.diff contains no BashInput hunk).
- **fix** Move the `"run_in_background"` entry after `"description"` in the json! literal at bash.rs:1284-1285. serde_json is built with `preserve_order` (lingxi-code/Cargo.toml:202), so insertion order is what the model receives. Field descriptions and types are already byte-correct; only the order changes.
- **verifier** Confirmed. `Qhm` read verbatim at cc-238.js @231064500: command, timeout, description, run_in_background, dangerouslyDisableSandbox, _simulatedSedEdit. sdk-tools-238.d.ts:578-609 agrees and the 220↔238 SDK diff has no BashInput hunk. Port bash.rs:1274-1292 emits command, timeout, run_in_background, description, dangerouslyDisableSandbox, and serde_json is built with `preserve_order` (lingxi-code/C …[truncated; full text in the subsystem report]

#### `BASH-09` — run_in_background is not stripped from the Bash schema when background tasks are disabled

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/bash.rs:1270 (`static INPUT_SCHEMA: Lazy<Value>`) and bash.rs:1313-1315 (`fn input_schema` returns it unconditionally)`
- **oracle** cc-238.js @231067780: `egm=we(()=>(WA()?Qhm().omit({run_in_background:!0,_simulatedSedEdit:!0}):Qhm().omit({_simulatedSedEdit:!0})).superRefine((e,t)=>{}))` with `function WA(){return wZe().backgroundTasksDisabled||V.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS}` (cc-238.js @222745633). `egm()` is the descriptor's `get inputSchema()`. Same shape in 2.1.220 (`gLd`/`DT()`, str-220.txt @7721691).
- **fix** Make `input_schema()` return a variant with `run_in_background` removed when the same disable switch that prompt.rs:105-118 `background_usage_note()` already reads is truthy, so the advertised schema and the prompt agree.
- **verifier** Confirmed. cc-238.js @231066716: `egm=we(()=>(WA()?Qhm().omit({run_in_background:!0,_simulatedSedEdit:!0}):Qhm().omit({_simulatedSedEdit:!0}))...)`. Port bash.rs:1274 is a `Lazy<Value>` static and `fn input_schema` returns it unconditionally, so `run_in_background` stays advertised even when the port's own `background_usage_note()` gate (LINGXI_DISABLE_BACKGROUND_TASKS, prompt.rs:107-110) has remo …[truncated; full text in the subsystem report]

#### `BASH-10` — dangerouslyDisableSandbox: true never downgrades to an ask with "Run outside of the sandbox"; SandboxOverride reason is never constructed

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/bash.rs:1357-1366 returns an unconditional `PermissionResult::Allow { reason: Other { reason: "allow-all-gate (M4-02 default)" } }`. `PermissionDecisionReason::SandboxOverride` (lingxi-code/permission/src/result.rs:112) is only matched (policy_gate.rs:513, :1754, :1781) and never constructed anywhere in the workspace; `grep -rn 'Run outside of the sandbox'` over lingxi-code = 0 hits`
- **oracle** 2.1.238 binary @114873408 for `Run outside of the sandbox` (4 hits in both 220 and 238). Bash descriptor (cc-238.js @231072502 region): `async checkPermissions(e,t){let r=await M8n(e,t);if(e.dangerouslyDisableSandbox&&r.behavior!=="deny"&&r.behavior!=="ask"&&!XXn(r.decisionReason)&&!BY(e)&&BY({...e,dangerouslyDisableSandbox:!1}))return{behavior:"ask",decisionReason:{type:"sandboxOverride",reason:"dangerouslyDisableSandbox"},message:"Run outside of the sandbox"};return r}`.
- **fix** In BashTool::check_permissions, run the real rule decision first; when dangerouslyDisableSandbox is set, the decision is neither deny nor ask, and should_use_sandbox would be true with the flag cleared but is false with it set, return an Ask carrying `PermissionDecisionReason::SandboxOverride { reason: "dangerouslyDisableSandbox" }` and message "Run outside of the sandbox".
- **verifier** Confirmed. cc-238.js @231071760 contains the exact `checkPermissions` downgrade to `{behavior:"ask",decisionReason:{type:"sandboxOverride",reason:"dangerouslyDisableSandbox"},message:"Run outside of the sandbox"}`. Port bash.rs:1357-1366 returns an unconditional Allow with reason "allow-all-gate (M4-02 default)"; `PermissionDecisionReason::SandboxOverride` is constructed only in permission/src/pol …[truncated; full text in the subsystem report]

#### `BASH-11` — Sandbox path lists in the prompt are never truncated at 50 with the "... and N more" marker

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:139-149 (`dedup`, no cap) used at prompt.rs:236-274; `grep -rn 'truncated for prompt size' --include='*.rs'` = 0 hits`
- **oracle** cc-238.js @231049197: ``function Phr(e){if(!e||e.length<=D_l)return e;let t=e.length-D_l;return[...e.slice(0,D_l),`... and ${t} more (truncated for prompt size)`]}`` with `D_l=50`, applied in `Khm()` to denyOnly, allowWithinDeny, allowOnly, denyWithinAllow, allowedHosts, deniedHosts and allowUnixSockets. Binary @132228439 for `truncated for prompt size` (2 hits in both 220 and 238; 220 uses `eFt`/`NFs`).
- **fix** Add a `truncate_for_prompt(list)` helper mirroring Phr (cap 50, append `format!("... and {} more (truncated for prompt size)", n - 50)`) and wrap every list in `sandbox_section`, applied AFTER dedup/normalization exactly as the oracle composes `Phr(gXr(x))`.
- **verifier** Confirmed. cc-238.js @231049197 defines `Phr` with `D_l=50` and the `... and ${t} more (truncated for prompt size)` marker, and `Khm()` wraps every one of the seven lists in it; cc-bin-220 @235675300 has the same shape under `eFt`/`NFs`. Port `sandbox_section` (prompt.rs:229-283) applies only `dedup` (prompt.rs:141-149); the truncation literal has 0 hits in the workspace.

#### `BASH-12` — $TMPDIR normalization checks one temp dir and dedups before substitution instead of after

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:207-215 - `dedup(paths).into_iter().map(|p| if p == tmp { "$TMPDIR" } else { p })`, with `tmp` from `lingxi_temp_dir()` only`
- **oracle** cc-238.js @231049336: `let s=new Set([_8(),s2r()]),a=Ojr(),l=(m)=>to(a?m.map((h)=>s.has(h)?"$TMPDIR":h):m.filter((h)=>!s.has(h)));` - TWO paths (`_8()` claudeTempDir @220851683, `s2r()` childProcessTmpDir @220851862) and `to()` (dedup, cc-238.js @217957738) runs AFTER the map. 2.1.220 identical two-element set: `s=new Set([Hie(),Z7r()]), a=(f)=>Co(f.map((m)=>s.has(m)?"$TMPDIR":m))` (str-220.txt @6073966 region).
- **fix** Build a two-element set { claude temp dir, child-process temp dir } and reorder to map-then-dedup so two distinct temp dirs collapse into a single "$TMPDIR" entry, matching `to(m.map(...))`.
- **verifier** Confirmed. cc-238.js @231049336: `s=new Set([_8(),s2r()])` and `l=(m)=>to(a?m.map(...):m.filter(...))` — two temp dirs, dedup AFTER the substitution. cc-bin-220 @235675300 has the same two-element set with map-then-dedup. Port `normalize_allow_only` (prompt.rs:202-215) uses a single `lingxi_temp_dir()` and dedups BEFORE mapping.

#### `BASH-13` — Verbose git section drops a trailing space after "when given direct instructions"

- **severity** P2 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:381 (`grep -c 'when given direct instructions $' tools/shell/src/prompt.rs` = 0)`
- **oracle** 2.1.238 `fcT` (cc-238.js @231042884), byte-identical in 2.1.220: `- NEVER run destructive git commands (push --force, reset --hard, checkout ., restore ., clean -f, branch -D) unless the user explicitly requests these actions. Taking unauthorized destructive actions is unhelpful and can result in lost work, so it's best to ONLY run these commands when given direct instructions ` - note the trailing U+0020 before the newline. `grep -c "when given direct instructions $"` = 1 in str-238.txt and = 1 in str-220.txt.
- **fix** Append a single trailing space to the line at prompt.rs:381. A full rendered diff of the port's verbose git section against the reconstructed oracle text shows this is the ONLY remaining byte difference in that section.
- **verifier** Confirmed by byte dump on both sides. `od -c` at cc-238.js @231043880 shows `instructions` + 0x20 + 0x0a; `grep -c 'when given direct instructions $'` = 1 in both str-238 and str-220. `od -c` of prompt.rs line 381 shows `instructions` immediately followed by 0x0a. I did not independently verify the reporter's stronger claim that this is the ONLY byte difference left in that section, but the cited …[truncated; full text in the subsystem report]

#### `BASH-14` — Commit/PR attribution has no insertion point in either Bash git section, though the oracle default is non-empty

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:34-42 and :586-592 document the assumption that attribution is empty; prompt.rs:399-401 renders "Create the commit with a message." and prompt.rs:606-611 `concise_git_section` emits only three fixed bullets. Attribution constants DO exist elsewhere: commands/core/src/commit.rs:107, commands/core/src/commit_push_pr.rs:15`
- **oracle** cc-238.js @231031496: ``function rcT(){...let n=`Co-Authored-By: ${t} <noreply@anthropic.com>`,r=Ohm(),o=Vo(),i=o.attribution;if(i!==void 0&&V8s(i))return{commit:i.commit??n,pr:i.pr??r};if(o.includeCoAuthoredBy===!1)return ...{commit:"",pr:""};return{commit:n,pr:r}}`` - the DEFAULT (no settings) branch returns NON-empty. `Ohm()` (@231030549) = `🤖 Generated with [Claude Code](${dNt})`. Binary @132242274 for `- End git commit messages with:` (present in 220 and 238). Consumed at five points: the concise section's `- End git commit messages with:\n${commit}` / `- End PR bodies with:\n${pr}` (mcT @231052587) and the verbose section's step-3 `Create the commit with a message ending with:\n   ${commit}`, the `git commit -m "$(cat <<'EOF'` example body, and the `gh pr create` body (fcT @231042884).
- **fix** Thread the existing attribution source (commands/core) into both `commit_and_pr_instructions` and `concise_git_section` so the five insertion points render when attribution is non-empty and vanish when a settings `includeCoAuthoredBy: false` / `attribution` override empties it. Only the wording is re-branded for LingXi; the missing piece is the mechanism.
- **verifier** Confirmed and NOT a branding-only exclusion. cc-238.js @231031796 `rcT()`'s default (no-settings) branch returns non-empty commit and PR attribution; only `includeCoAuthoredBy===false` or an explicit `attribution` override empties it. cc-238.js @231046030 shows `- Create the commit with a message${o?" ending with:\n   "+o:"."}` and `- End git commit messages with:` has 2 hits in 238. Port prompt.r …[truncated; full text in the subsystem report]

#### `BASH-15` — NEW 2.1.238 relaxed-filesystem-policy arm of the sandbox temp-dir bullet has no port surface

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/prompt.rs:330 emits only the $TMPDIR bullet; no filesystemPolicy/relaxed concept exists (`grep -rn 'mktemp -d' --include='*.rs'` = 0 hits)`
- **oracle** New string in 2.1.238 (0 hits in str-220.txt, 1 in str-238.txt), in `Khm()` (cc-238.js @231049336): ``Ojr()?"For temporary files, always use the `$TMPDIR` environment variable. ...":"For temporary files, create a scratch directory with `mktemp -d` and reference it by absolute path. Do NOT assume `$TMPDIR` is set — the sandbox does not export it in this configuration."``, with `function Ojr(){return Wt()==="windows"||Bze()!=="relaxed"}` and `Bze()` (cc-238.js @222550676) resolving a new `filesystemPolicy` setting (strict | relaxed | relaxedIfForced), default strict. The same `Ojr()` also switches allowOnly normalization from substitute-with-$TMPDIR to filter-out.
- **fix** Low priority: the default policy is strict, so the port matches today. If/when a relaxed filesystem policy is ported, add the alternate bullet verbatim and switch `normalize_allow_only` to the filter form under the same predicate.
- **verifier** Confirmed as genuinely new in 238: the `mktemp -d` sentence is 0 hits in str-220 and 1 in str-238, and cc-bin-220 @235677222 shows the `$TMPDIR` bullet emitted unconditionally while cc-238.js @231049336 wraps it in the `Ojr()` ternary. Default filesystemPolicy is strict so the port currently renders the correct arm — P3 is the right severity, not inflated.

#### `BASH-16` — "Commands are cheap to run and their errors are informative" bullet has no port surface

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent - no such literal or gate in lingxi-code/tools/shell/src/prompt.rs`
- **oracle** 2.1.238 binary @132246448 (2 hits in both 220 and 238): `- Commands are cheap to run and their errors are informative: run the straightforward command rather than perfecting it mentally first, and adjust from what it prints.` Emitted in `hcT` as `let c=JQd()?[...]:[]` where `function JQd(){return Q$r(V.CLAUDE_CODE_GORSE_PLOVER,DKb,void 0)}` (cc-238.js @221575238) - an env/statsig gate that is off by default.
- **fix** Inert at the default gate value, so no user-visible drift today. Record it (or add it behind an off-by-default flag placed between the "Command output is displayed" bullet and the timeout bullet) so a future enablement is not silently missed.
- **verifier** Confirmed. The bullet is 2 hits in both versions and is gated in `hcT` by `JQd()` = `Q$r(V.CLAUDE_CODE_GORSE_PLOVER,DKb,void 0)` (cc-238.js @221575238), an env/statsig gate that is off by default. Port has no such literal (`grep -rn 'cheap to run' --include='*.rs'` = 0). Inert today ⇒ P3 is correct.

#### `BASH-17` — Sleep-block validateInput gate is missing the !backgroundTasksDisabled conjunct

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/shell/src/bash.rs:1424 - `if sleep_block_enabled() && !run_bg {` (only two conjuncts; `sleep_block_enabled()` at bash.rs:118-125 covers HSe() only)`
- **oracle** cc-238.js @231072502 (identical in 2.1.220): ``async validateInput(e){if(HSe()&&!WA()&&!e.run_in_background){let t=xcT(e.command);if(t!==null)return{result:!1,message:`Blocked: ${t}. To wait for a condition, use Monitor with an until-loop ...`,errorCode:10}}return{result:!0}}`` - three conjuncts, including `!WA()` (`function WA(){return wZe().backgroundTasksDisabled||V.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS}`, cc-238.js @222745633).
- **fix** Add the third conjunct so the block is inert when background tasks are disabled (the block's own remedy tells the model to use run_in_background, which is unavailable in that configuration). The message text, the 25s threshold and the `standalone sleep N` / `sleep N followed by: ...` phrasing already match byte-for-byte.
- **verifier** Confirmed. cc-238.js @231071311: `async validateInput(e){if(HSe()&&!WA()&&!e.run_in_background){...}}` — three conjuncts. Port bash.rs:1424 has only `sleep_block_enabled() && !run_bg`, and `sleep_block_enabled()` (bash.rs:118-125) models only the `tengu_amber_sentinel` gate. Whole block defaults off in the port ⇒ P3 is correct, not inflated.

#### `BASH-18` — Bash coerceInput (timeout_ms -> timeout) is not ported

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent - `grep -rn 'fn coerce_input' --include='*.rs'` over lingxi-code = 0 hits; `timeout_ms` appears nowhere in lingxi-code/tools/shell/src/bash.rs`
- **oracle** cc-238.js @230980000 (present in 2.1.220 too), referenced by the descriptor as `coerceInput:Vmm`: `function Vmm(e){if(!ni(e))return null;let t={...e},r=[];if("timeout_ms"in t&&!("timeout"in t)){let n=t.timeout_ms;if(typeof n==="number"||typeof n==="string"&&/^\d+$/.test(n))t.timeout=n,r.push("timeout_ms");delete t.timeout_ms}return r.length?{input:t,shapeClass:r.join(",")}:null}`
- **fix** Add a coercion step before `resolve_timeout_ms` that rewrites a `timeout_ms` number-or-digit-string into `timeout` when `timeout` is absent, then removes `timeout_ms` - otherwise the port's `additionalProperties: false` schema plus the missing alias silently drops the model's requested timeout.
- **verifier** Confirmed. `Vmm` read verbatim at cc-238.js @230980000 and referenced as `coerceInput:Vmm` at @231070828; present in 2.1.220 too. Port has no `fn coerce_input` anywhere and no `timeout_ms` handling in tools/shell/src/bash.rs, while its schema is `additionalProperties:false`, so a `timeout_ms` input is rejected rather than coerced.

#### `BASH-19` — Bash userFacingName variants (SandboxedBash indicator, sed-edit rename) not implemented

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent - BashTool in lingxi-code/tools/shell/src/bash.rs implements no `user_facing_name`; `grep -rn 'SandboxedBash|BASH_SANDBOX_SHOW_INDICATOR' --include='*.rs'` = 0 hits`
- **oracle** cc-238.js @231069721 (identical in 2.1.220): `userFacingName(e){if(!e)return"Bash";if(e.command){let t=Ffr(e.command);if(t)return f0i({file_path:t.filePath,old_string:"x"})}return V.CLAUDE_CODE_BASH_SANDBOX_SHOW_INDICATOR&&BY(e)?"SandboxedBash":"Bash"}`. `SandboxedBash` = 2 hits and `CLAUDE_CODE_BASH_SANDBOX_SHOW_INDICATOR` = 3 hits in both str-220.txt and str-238.txt.
- **fix** Display-only. Add `user_facing_name` returning "SandboxedBash" when the indicator env var is set and the command would be sandboxed, otherwise "Bash" (and the sed-edit rename if the sed-preview path is ever ported).
- **verifier** Confirmed. `SandboxedBash` = 2 hits and `CLAUDE_CODE_BASH_SANDBOX_SHOW_INDICATOR` = 3 hits in BOTH str-220 and str-238; the `userFacingName` body sits in the Bash descriptor region at cc-238.js @231069721. The port's absence is not proven by a foreign-symbol grep alone: the trait method exists and is implemented elsewhere (tools/worktree/src/worktree.rs:817, tools/agent/src/agent.rs:1316, tools/ta …[truncated; full text in the subsystem report]

### Read / Write / Edit / MultiEdit / NotebookEdit tool contracts

#### `FT-01` — NotebookEdit/notebook-read invalid-notebook message missing the new "each with a string or string-array \"source\"" clause

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/notebook_read.rs:290 and /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/notebook_edit.rs:380 — both still emit "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects)."`
- **oracle** cc-238.js @226490219 (live JS): `$0i='Notebook file is not a valid Jupyter notebook (top-level "cells" must be an array of cell objects, each with a string or string-array "source").'` — 2.1.220 says `...an array of cell objects).` (cc-bin-220 @96596784 and readable @232562744 inside `Ctd`). Fixed-string counts on cc-bin-220: new form = 0 hits, old form = 2 hits.
- **fix** Hoist the literal into one shared const in tools/file/src/lib.rs and set it to the 238 bytes: `Notebook file is not a valid Jupyter notebook (top-level "cells" must be an array of cell objects, each with a string or string-array "source").` Update both call sites and the tests that assert the old text.
- **verifier** Independently confirmed. 2.1.238 string table @74017843 and live JS $0i carry 'Notebook file is not a valid Jupyter notebook (top-level "cells" must be an array of cell objects, each with a string or string-array "source").'; fixed-string counts: new form 238=2 / 220=0, old form 238=0 / 220=2. 2.1.220 @232562682 (Ctd) shows the old sentence. Port still emits the 220 form at notebook_read.rs:290 an …[truncated; full text in the subsystem report]

#### `FT-03` — Write has no Read-deny refusal; the port reuses the Edit wording ("cannot be edited") where 2.1.238 introduced "cannot be written"

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "cannot be written" --include='*.rs' lingxi-code` returns no tool hit. Instead /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/permission/src/policy.rs:726 routes every `FileToolKind::Editor` tool (and permission/src/filesystem.rs:157 classifies "Write" as Editor) into `ask_edit_read_deny_covered` (policy.rs:2954), which emits the Edit-flavoured "...cannot be edited."`
- **oracle** cc-238.js @220242876: `asa="File is covered by a Read deny rule in your permission settings and cannot be written."` — used only by Write: validateInput @226415322 `if(Tnr(n,gn(r)))return{result:!1,message:asa,errorCode:13};` (no `behavior:"ask"` => hard failure) and Write `call` @226410691 `throw new Q4e(asa)`. On cc-bin-220 the literal has 0 hits and 220's Write validateInput (readable @232489055) has no read-deny branch at all. The Edit-only sibling `ssa` ("...cannot be edited.") is at @220242784.
- **fix** Split the read-deny message by tool: keep "...cannot be edited." for Edit/MultiEdit/NotebookEdit and add "File is covered by a Read deny rule in your permission settings and cannot be written." for Write. Match the oracle's shape too: Write's is a hard validation error (no ask), Edit's is `behavior:"ask"`.
- **verifier** Confirmed at the oracle: 238 @283747794 defines ssa='...cannot be edited.' and asa='...cannot be written.'; asa has exactly two uses, both Write-only — validateInput @289920333 `if(Tnr(n,gn(r)))return{result:!1,message:asa,errorCode:13};` (no behavior:"ask") and call-phase `throw new Q4e(asa)` @289915702. Edit's twin @289911196 is `{result:!1,behavior:"ask",message:ssa,errorCode:13}`. 2.1.220 coun …[truncated; full text in the subsystem report]

#### `FT-05` — Notebook-too-large jq guidance still uses the 2.1.220 quoted-path form and a non-oracle shell-tool name

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/read.rs:2195 — "...Use a registered shell tool with jq to read specific portions:\n  cat \"{file_path}\" | jq '.cells[:20]' ..."`
- **oracle** cc-238.js @226486201 `function $Ka(){return Sh()?`Use ${Oi} with jq to read specific portions:\n  cat <notebook_path> | jq '.cells[:20]' # First 20 cells\n  cat <notebook_path> | jq '.cells[100:120]' # Cells 100-120\n  cat <notebook_path> | jq '.cells | length' # Count total cells\n  cat <notebook_path> | jq '.cells[] | select(.cell_type=="code") | .source' # All code sources`: ...}` with `Oi="Bash"` (@220239771). 2.1.220 (readable @235727511) built the same text inline as ``cat "${t}" | jq ...`` with the real path.
- **fix** Extract a `notebook_jq_guidance()` helper matching 238's `$Ka()` (literal `<notebook_path>` placeholder, `Use {BASH_TOOL_NAME} with jq to read specific portions:`) and use it both for the Read "Notebook content (...) exceeds maximum allowed size" message and for the new 100 MiB cap message from FT-04.
- **verifier** Confirmed. 238 $Ka() uses the literal placeholder `cat <notebook_path> | jq ...` (no quotes, no interpolated path); 2.1.220 @109111686 built the same lines as `cat "${t}" | jq ...` with the real path — so the placeholder form is genuine 220->238 drift, and read.rs:2195 still ships the 220 quoted-path form. Caveat that does not refute: the 'non-oracle shell-tool name' half is a port-WIDE substituti …[truncated; full text in the subsystem report]

#### `FT-06` — Edit's file-not-found error is a port-invented sentence instead of the oracle's cwd-note + "Did you mean" message

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/edit.rs:626-630 — `return Err(ToolError::InvalidInput(format!("File does not exist: {}", canon.display())));``
- **oracle** cc-238.js @226406753 (Edit validateInput; identical in 220 @232481628): ``let y=await aIt(s),_=await YDe(s),S=`File does not exist. ${U_e} ${er()}.`; if(_)S+=` Did you mean ${_}?`; else if(y)S+=` Did you mean ${y}?`; return{result:!1,behavior:"ask",message:S,errorCode:4};`` with `U_e="Note: your current working directory is"` (@218652…). Rendered: `File does not exist. Note: your current working directory is /abs/cwd.`
- **fix** Reuse the helper Read already has (read.rs:1300-1320, which builds `File does not exist. Note: your current working directory is {cwd}.` plus the cwd-suggestion / similar-filename `" Did you mean {x}?"` suffixes) from FileEditTool's nonexistent-file branch, and update edit_test.rs assertions.
- **verifier** Confirmed. 238 Edit validateInput @289911763: ``let y=await aIt(s),_=await YDe(s),S=`File does not exist. ${U_e} ${er()}.`; if(_)S+=` Did you mean ${_}?`; else if(y)S+=` Did you mean ${y}?`; return{result:!1,behavior:"ask",message:S,errorCode:4}``. Port edit.rs:626-630 returns `ToolError::InvalidInput(format!("File does not exist: {}", canon.display()))`. The correct builder already exists for Rea …[truncated; full text in the subsystem report]

#### `FT-07` — Stale-read refusal emits the oracle's rare call-phase message instead of the common validate-phase one

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/lib.rs:122 (`FILE_CONTENT_CHANGED_LINTER_MESSAGE`) returned by `check_read_before_write`, which Edit (edit.rs:645), Write and NotebookEdit (notebook_edit.rs:322) all call; the correct constant `FILE_UNEXPECTEDLY_MODIFIED_ERROR` (lib.rs:92) is defined but never emitted.`
- **oracle** Oracle validateInput bytes (the branch the model normally hits): "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it." — Edit errorCode 7 @226407883, Write errorCode 3 @226416137, NotebookEdit errorCode 10 @226495421 (all identical in 2.1.220). `WVo="File content has changed since it was last read. This commonly happens when a linter or formatter run via Bash rewrites the file. Call Read on this file to refresh, then retry the edit."` (@220242969) is thrown ONLY from the call-phase re-check `Ehv` (@226410…), i.e. the validate->call race.
- **fix** Make `check_read_before_write` return `FILE_UNEXPECTEDLY_MODIFIED_ERROR` for the ordinary stale case (mtime advanced, content differs) and keep `FILE_CONTENT_CHANGED_LINTER_MESSAGE` only for the post-validation re-check performed inside the write critical section, matching the oracle's validateInput-vs-`Ehv` split.
- **verifier** Confirmed and the port's own justification is factually wrong. 238 Edit validateInput @289912893 returns errorCode 7 with 'File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.'; WVo (the linter/formatter sentence) is thrown only from the call-phase re-check Ehv inside Chv's eke() critical section (@289915702 region). Port lib.rs:122 FIL …[truncated; full text in the subsystem report]

#### `FT-02` — Notebook shape validator only checks that `cells` is an array — per-cell object and `source` type checks missing

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/notebook_read.rs:285-291 (`notebook.get("cells").and_then(Value::as_array)`) and /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/notebook_edit.rs:374-381 — neither inspects individual cells.`
- **oracle** cc-238.js @226485491: `function X2t(e){let t=e?.cells;return Array.isArray(t)&&!t.some((r)=>r===null||typeof r!=="object"||typeof r.source!=="string"&&!(Array.isArray(r.source)&&r.source.every((n)=>typeof n==="string")))}`. 2.1.220 (readable @232562744) already had the weaker `if(!Array.isArray(i?.cells)||i.cells.some((a)=>a===null||typeof a!=="object"))` form.
- **fix** Add a shared `fn is_valid_notebook(nb: &Value) -> bool` that ports `X2t` exactly (cells is an array AND no cell is null/non-object AND every cell's `source` is a string or an array of strings) and gate both the read and edit paths on it before rendering/mutating cells.
- **verifier** Oracle side confirmed: 238 @289990631 `function X2t(e){let t=e?.cells;return Array.isArray(t)&&!t.some((r)=>r===null||typeof r!=="object"||typeof r.source!=="string"&&!(Array.isArray(r.source)&&r.source.every((n)=>typeof n==="string")))}`; 220 @232562682 had the weaker null/object form. Port side confirmed: notebook_read.rs:285-290 and notebook_edit.rs:374-381 only do `get("cells").as_array()`. Se …[truncated; full text in the subsystem report]

#### `FT-04` — Notebook reader missing 2.1.238's 100 MiB size cap and non-regular-file guard (two new model-facing errors)

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "Notebook file exceeds the maximum size\|Notebook path is not a regular file" --include='*.rs'` returns nothing; /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/notebook_read.rs:273 goes straight from raw text to `serde_json::from_str`.`
- **oracle** cc-238.js @226489330 / @226487111, inside `K0f`: `if(!n.isFile()&&!n.isDirectory())throw new Wur("Notebook path is not a regular file (device, FIFO, or socket)."); if(n.size>aV)throw new Wur(l8n()); let o=await Ar().readFileBytes(r,aV+1); if(o.length>aV)throw new Wur(l8n());` with `aV=104857600` and `function l8n(){return `Notebook file exceeds the maximum size this tool can read (${Ba(aV)}). ${$Ka()}`}` (@226487089). Both literals: 0 hits in cc-bin-220. `l8n()` is also NotebookEdit validateInput errorCode 13.
- **fix** Before parsing a notebook, stat the path: refuse non-file/non-dir with "Notebook path is not a regular file (device, FIFO, or socket).", and refuse size > 104_857_600 with `format!("Notebook file exceeds the maximum size this tool can read ({}). {}", format_file_size(104_857_600), jq_guidance())`. Re-read with a 100 MiB+1 cap and re-check. Surface the size message from NotebookEdit too.
- **verifier** Confirmed. 238 K0f @289994340: `if(!n.isFile()&&!n.isDirectory())throw new Wur("Notebook path is not a regular file (device, FIFO, or socket)."); if(n.size>aV)throw new Wur(l8n()); let o=await Ar().readFileBytes(r,aV+1); if(o.length>aV)throw new Wur(l8n());` with `var aV=104857600` and `l8n()` = 'Notebook file exceeds the maximum size this tool can read (${Ba(aV)}). ${$Ka()}'. Both literals: 2 hit …[truncated; full text in the subsystem report]

#### `FT-08` — Read's PARTIAL-view truncation banner omits the file path and is appended to the tool_result instead of being a separate notice

- **severity** P2 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/read.rs:258-270 builds the note without the `{fullFilePath}: ` segment; read.rs:2408-2412 appends it to `model_content` after `\n\n`. `read_truncation_notice` appears nowhere in the Rust tree.`
- **oracle** cc-238.js @231111059 (inside `Sgm`): ``R=!z&&k<_?gmt+`${r}: showing lines 1-${k} of ${_} total (${F.tokenCount} tokens, cap ${c}). Call ${Ns} with offset=${k+1} limit=${k} for the next page, or ${Am} to find a specific section. Do NOT answer from this page alone if the answer may be further in the file.]`:gmt+`${r}: showing the first ${J.length} of ${g.length} characters ...]``` with `gmt="[Truncated: PARTIAL view — "` and `r` = fullFilePath; 2.1.220 has the same `: showing lines 1-` fragment (@109114941). Delivery: `daf(L,R)` then the dispatcher @230795806 pushes `{type:"read_truncation_notice",banner,toolUseID}`, rendered @233231299 as `read_truncation_notice:(e)=>Zy([kn({content:pze(e.banner),isMeta:!0})])` — a separate meta user message, not part of the tool_result.
- **fix** Prefix both note branches with `{canon.display()}: ` so the bytes match `gmt + "{path}: showing ..."`. Separately (larger change) route the banner as its own meta message rather than concatenating it onto the tool_result, mirroring the oracle's `read_truncation_notice` attachment.
- **verifier** Confirmed. 238 @294616079 builds both branches as ``gmt+`${r}: showing lines 1-${k} of ${_} total ...`` / ``gmt+`${r}: showing the first ${J.length} of ${g.length} characters ...`` with gmt='[Truncated: PARTIAL view — ' (@285134337) and r = the full path; 2.1.220 @109114941 shows the same interpolated-variable prefix, so this is a long-standing port gap rather than new drift. Port read.rs:254-270 …[truncated; full text in the subsystem report]

#### `FT-09` — Image-read validation errors missing: no magic-byte check message, and the empty-image text differs

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/image_read.rs (76 lines, no error strings) and /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tool-api/src/util/image_budget.rs:79 ("Image file is empty (0 bytes)") / :82 ("failed to decode image: {e}").`
- **oracle** cc-238.js @231112380 (`q_l`, unchanged since 220 — the "download saved an error/login page" literal has 2 hits in cc-bin-220): ``if(i===0)throw new ht(`Image file is empty: ${e}`,"Image file is empty"); let s=Cle(o); if(s===null)throw new ht(`File has an image extension but its content is not a valid PNG/JPEG/GIF/WebP. Detected: ${Kzn(o)}. This usually means a download saved an error/login page instead of the image. Use \`file "${e}"\` to confirm, or read it as text with ${Oi} (e.g. \`head -c 500\`).`,"Image extension but invalid magic bytes");``
- **fix** In the FileRead image branch: emit `format!("Image file is empty: {file_path}")` for 0 bytes, and before decoding run a magic-byte sniff (PNG/JPEG/GIF/WebP); on failure emit the oracle's full sentence including the detected-type interpolation, the `file "<path>"` hint and the `Bash ... head -c 500` hint.
- **verifier** Confirmed. 238 string table @132623920 holds 'Image file is empty: ', 'File has an image extension but its content is not a valid PNG/JPEG/GIF/WebP. Detected: ... This usually means a download saved an error/login page instead of the image. Use `file "..."` to confirm, or read it as text with ... (e.g. `head -c 500`).' and the 'Image extension but invalid magic bytes' code. The magic-bytes literal …[truncated; full text in the subsystem report]

#### `FT-10` — MultiEdit is registered as a real builtin tool, but 2.1.238 registers no such tool

- **severity** P2 &nbsp;·&nbsp; **kind** *port-extra* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/tools/file/src/lib.rs:287 `reg.register_builtin(Arc::new(MultiEditTool::new(ctx.clone())));`, snapshotted at /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/engine-desktop/tests/snapshots/desktop_tool_list_snapshot__desktop_tool_list.snap:26. The port's own parity fixture test-harness/src/parity/fixtures/registry_42_tools.json omits it, and multi_edit.rs:13 states it is not a registered tool in the binary.`
- **oracle** `MultiEdit` does not appear in /private/tmp/.../scratchpad/sdk-tools-238.d.ts at all (0 hits), and its only three live-JS occurrences in cc-238.js are name lists / advice text: @218856888 (permission-rule suggestion mapping Write/NotebookEdit/MultiEdit -> Edit), @226841651 (a flat known-tool-name array), @243216151. There is no `es({name:"MultiEdit"...})` tool registration; 2.1.220 is the same (cat-toolname-220.txt has no MultiEdit).
- **fix** Stop registering MultiEdit as an advertised builtin (drop the `register_builtin` call and update the desktop/mobile tool-list snapshots) while keeping the name-routed dispatch shim so a model-emitted `MultiEdit` tool_use is still accepted — that is exactly the oracle's `V4l` behaviour. If it is meant to stay, record it in the accepted-divergences ledger.
- **verifier** Confirmed, and not proven by a bare negative grep: MultiEdit has 0 hits in both sdk-tools-238.d.ts and sdk-tools-220.d.ts, and all 12 binary occurrences in 238 are name lists or prose — @282361899 permission-rule suggestion mapping Write/NotebookEdit/MultiEdit to Edit, @290346662 the qTv known-tool-name array, @297388763 the activity map, @97640832 deny-circumvention advice. No tool descriptor exi …[truncated; full text in the subsystem report]

### Glob / Grep tool contracts

#### `ST-01` — Grep description says "through a registered shell tool" where the oracle names Bash

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/grep.rs:90 (long) and lingxi-code/tools/file/src/grep.rs:104 (short); the same substitution recurs at lingxi-code/orchestrator/src/prompt/body_sections.rs:243 where the oracle's `B9T` emits "`find` or `grep` via the Bash tool"`
- **oracle** 2.1.238 binary @97649891: `  - ALWAYS use ${Am} for search tasks. NEVER invoke \`grep\` or \`rg\` as a ${Oi} command. The ${Am} tool has been optimized for correct permissions and access.` with `var Am="Grep"` and `var Oi="Bash"` -> renders `... NEVER invoke \`grep\` or \`rg\` as a Bash command.`. Short variant, same function `Wka`: `Content search built on ripgrep. Prefer this over \`grep\`/\`rg\` via ${Oi} — results integrate with the permission UI and file links.` -> `... via Bash — results ...`. 2.1.220 (cc-bin-220 @229151688) is byte-identical.
- **fix** Restore the oracle bytes at these three sites: grep.rs:90 -> "NEVER invoke `grep` or `rg` as a Bash command."; grep.rs:104 -> "Prefer this over `grep`/`rg` via Bash — results integrate ..."; body_sections.rs:243 -> "`find` or `grep` via the Bash tool". Do NOT globally replace "registered shell tool" — the oracle genuinely uses that phrase in the Read tool description (verified), so read.rs:995 is correct as-is.
- **verifier** Verified at the oracle source text @286224735 (`Wka`) and @297069537 (`B9T`): `var Oi="Bash"` (@283744781), so the rendered bytes are "NEVER invoke `grep` or `rg` as a Bash command", "via Bash — results integrate...", and "`find` or `grep` via the Bash tool". Port grep.rs:90/:104 and body_sections.rs:243 say "registered shell tool". The port's own shell tool IS named "Bash" (tools/shell/src/bash.r …[truncated; full text in the subsystem report]

#### `ST-02` — Glob and Grep drop "(if available)" from the Agent-delegation guidance line

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/glob.rs:78 ("...use the Agent tool instead") and lingxi-code/tools/file/src/grep.rs:96 ("  - Use Agent tool for open-ended searches requiring multiple rounds")`
- **oracle** 2.1.238 binary @285270818: ``BJb=`${znp}\n- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the ${Ci} tool instead (if available)` `` with `var Ci="Agent"`. Grep's equivalent (function `Wka`): ``  - Use ${Ci} tool (if available) for open-ended searches requiring multiple rounds``. 2.1.220 identical (cc-bin-220 @229143230).
- **fix** Append " (if available)" to glob.rs:78 and insert "(if available) " after "Agent tool " in grep.rs:96 so they read exactly "...use the Agent tool instead (if available)" and "  - Use Agent tool (if available) for open-ended searches requiring multiple rounds".
- **verifier** Confirmed in both binaries: 2.1.238 `BJb` ends "...use the ${Ci} tool instead (if available)" and `Wka` has "  - Use ${Ci} tool (if available) for open-ended searches requiring multiple rounds"; 2.1.220 carries the same literals (cc-bin-220 @229143286 and @229152245). Port glob.rs:78 and grep.rs:96 omit "(if available)"; repo-wide grep for "(if available)" hits only coordinator/src/prompt.rs:193. …[truncated; full text in the subsystem report]

#### `ST-04` — Grep rejects a FILE path with "Path is not a directory"; the oracle accepts it (rg PATH)

- **severity** P1 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/grep.rs:510 calls `crate::dir_validate::validate_search_directory`, which returns `Path is not a directory: {path}` at lingxi-code/tools/file/src/dir_validate.rs:68-70. dir_validate.rs:3 claims the two tools' validateInput are "identical" (true only up to v2.1.183).`
- **oracle** 2.1.238 binary @289934948, Grep `validateInput`: `if(t){let a=Ar(),l=Zi(t); if(aU(l))return{result:!0}; try{await a.stat(l)}catch(c){if(ur(c)){ ... `Path does not exist: ${t}. ${U_e} ${er()}.` ...}} } return{result:!0}` — there is NO `isDirectory()` branch. Only Glob has `if(!i.isDirectory())return{result:!1,message:`Path is not a directory: ${t}`,errorCode:2}`. Grep's own schema description reads "File or directory to search in (rg PATH). Defaults to current working directory." 2.1.220 identical (cc-bin-220 @232504978 region).
- **fix** Give Grep its own validator: stat the resolved path, return Ok on success regardless of file-vs-directory, and on ENOENT emit `Path does not exist: {path}. Note: your current working directory is {cwd}.` (+ the " Did you mean {sibling}?" suffix). Leave `validate_search_directory` for Glob only.
- **verifier** Read the FULL Grep tool object at @289934948 (238) and @232505234 (220): validateInput stats the path and returns {result:!0} on success with no isDirectory() branch; the isDirectory branch exists only on the Glob object (@289927138). Port grep.rs:510 routes into dir_validate::validate_search_directory, which errors `Path is not a directory: {path}` (dir_validate.rs:66-70). The module header's cla …[truncated; full text in the subsystem report]

#### `ST-05` — Grep's missing-path error says "Directory does not exist" instead of "Path does not exist"

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/dir_validate.rs:74-77 emits `Directory does not exist: {path}. Note: your current working directory is {cwd}.` for BOTH tools, reached from grep.rs:510`
- **oracle** 2.1.238 binary @289934948: Grep builds ``let d=`Path does not exist: ${t}. ${U_e} ${er()}.`;if(u)d+=` Did you mean ${u}?`;``. Glob (separate site) builds ``let l=`Directory does not exist: ${t}. ${U_e} ${er()}.`;``. `U_e` = "Note: your current working directory is" (2 hits in 2.1.238). 2.1.220 identical.
- **fix** Parameterize the ENOENT prefix (or add a Grep-specific path) so Grep emits `Path does not exist: {path}. ...` while Glob keeps `Directory does not exist: {path}. ...`.
- **verifier** Oracle Grep builds `Path does not exist: ${t}. ${U_e} ${er()}.` while Glob builds `Directory does not exist: ${t}. ...` (both quoted from the full tool objects; identical in 2.1.220). Port dir_validate.rs:74-77 emits the Glob wording for both tools, and a unit test (nonexistent_directory_message_is_byte_exact) pins the wrong wording for Grep.

#### `ST-03` — Glob/Grep descriptions never drop the Agent line under a non-default subagent steer

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/glob.rs:227-241 and lingxi-code/tools/file/src/grep.rs:526-541 — `description()`/`prompt()` return the constant unconditionally; neither calls `platform_api::live_sessions::subagent_steer_is_default()`, which exists at lingxi-code/platform-api/src/live_sessions.rs:1405 and is used only by orchestrator/src/prompt/body_sections.rs:234`
- **oracle** 2.1.238 `function ISa(e){if(qk(e))return '<short>'; return DZ()==="default"?BJb:znp}` — `znp` is the four-bullet Glob text WITHOUT the Agent bullet, `BJb` is `znp` + the Agent bullet. Grep's `Wka` wraps its Agent line in `${DZ()==="default"?`  - Use ${Ci} tool (if available) for open-ended searches requiring multiple rounds\n`:""}`. `DZ()` = the latched subagent steer (binary @285... `function DZ(){let e=SJo();...}`). 2.1.220 identical (`Cq()==="default"?BOg:Iou`).
- **fix** Split each constant into a base (Glob: the four bullets; Grep: the text minus the Agent line) plus the Agent line, and select on `platform_api::live_sessions::subagent_steer_is_default()` in both `description()` and the LONG arm of `prompt()`, mirroring `DZ()==="default"?BJb:znp`.
- **verifier** Oracle source read verbatim: `function ISa(e){if(qk(e))return '<short>';return DZ()==="default"?BJb:znp}` and Grep's `${DZ()==="default"?`  - Use ${Ci} tool (if available)...\n`:""}`. Port glob.rs/grep.rs `description()` and `prompt()` return the constants unconditionally; `platform_api::live_sessions::subagent_steer_is_default()` (platform-api/src/live_sessions.rs:1405) has exactly one caller, body_sections. …[truncated; full text in the subsystem report]

#### `ST-06` — No null-byte validation on Glob or Grep inputs

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn 'null bytes' --include='*.rs'` over the whole lingxi-code workspace (excluding target/) returns 0 hits; grep.rs:504-513 and glob.rs:205-214 only call `validate_search_directory``
- **oracle** 2.1.238 binary @289924033: `function h0i(e,t){let r=t.find(([,n])=>n?.includes("\x00"));if(r)return{result:!1,message:`${e} ${r[0]} cannot contain null bytes (\\0). Remove the null byte and try again.`,errorCode:2};return null}`, invoked as `h0i(Am,[["pattern",e],["path",t],["glob",r],["type",n]])` for Grep and `h0i(Bm,[["pattern",e],["path",t]])` for Glob — i.e. "Grep pattern cannot contain null bytes (\0). Remove the null byte and try again." 2.1.220 identical (cc-bin-220 @232496445).
- **fix** Add a shared helper in tools/file/src/dir_validate.rs that scans (pattern, path, glob, type) for '\0' and returns `{TOOL_NAME} {field} cannot contain null bytes (\0). Remove the null byte and try again.`, called first in both tools' `validate_input`.
- **verifier** Oracle @289924033 defines h0i exactly as quoted, and both tool objects call it first (`h0i(Am,[["pattern",e],["path",t],["glob",r],["type",n]])`, `h0i(Bm,[["pattern",e],["path",t]])`); present in 2.1.220 as HTo. Port: workspace grep for "null byte" hits only sandbox-runtime/src/host.rs:72 (an unrelated test), and grep.rs/glob.rs validate_input call only validate_search_directory. Genuinely absent.

#### `ST-07` — Grep does not validate head_limit / offset as non-negative integers

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — lingxi-code/tools/file/src/grep.rs:504-513 validates only `path`; the values are later silently coerced by the `semanticNumber`-ish helper at grep.rs:312`
- **oracle** 2.1.238 binary @289934948 region: `for(let[a,l]of[["head_limit",o],["offset",i]])if(l!==void 0&&(!Number.isInteger(l)||l<0))return{result:!1,message:`${a} must be a whole number of 0 or more, got ${l}.${a==="head_limit"?" Pass 0 for unlimited.":""}`,errorCode:2}`. Literal `must be a whole number of 0 or more, got ` present in 2.1.238 and in 2.1.220 (cc-bin-220 @232504978).
- **fix** In Grep's `validate_input`, reject non-integer or negative `head_limit`/`offset` with `{field} must be a whole number of 0 or more, got {value}.` plus the trailing " Pass 0 for unlimited." for head_limit only.
- **verifier** The head_limit/offset integer loop with message `${a} must be a whole number of 0 or more, got ${l}.` (+ " Pass 0 for unlimited." for head_limit) is present verbatim in the 2.1.238 and 2.1.220 Grep validateInput. Port grep.rs:504-513 validates only `path`; the numbers are coerced later by value_as_usize. Absence confirmed by reading the port function, not by a symbol grep.

#### `ST-08` — Grep input_schema property order diverges and injects `default` keys the oracle does not emit

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/grep.rs:449-469 — order is pattern, path, glob, TYPE, output_mode, -A, -B, -C, context, -n, -i, -o, head_limit, offset, multiline, and it adds "default": "files_with_matches" / true / false / false / 0 / false on six fields. serde_json is built with `preserve_order` (lingxi-code/Cargo.toml:202), so insertion order is the wire order.`
- **oracle** 2.1.238 `Dhv=we(()=>ci({pattern, path, glob, output_mode, "-B", "-A", "-C", context, "-n", "-i", "-o", type, head_limit, offset, multiline}))` — that exact order, and every numeric/boolean field is `ece(Xe().optional())` / `xq(Bt().optional())` with NO `.default(...)` (contrast Edit's `replace_all:xq(Bt().default(!1).optional())`, which does carry one). 2.1.220 identical.
- **fix** Reorder the `json!` properties to pattern, path, glob, output_mode, -B, -A, -C, context, -n, -i, -o, type, head_limit, offset, multiline and drop the six `"default"` keys (keep the defaults in the Rust parsing code, which already applies them).
- **verifier** Read the full `Dhv` schema factory: order is pattern, path, glob, output_mode, -B, -A, -C, context, -n, -i, -o, type, head_limit, offset, multiline, and no field carries `.default(...)`. Port grep.rs:449-469 reorders (type moved up, -A before -B) and injects six `"default"` keys; serde_json is built with preserve_order (Cargo.toml:202) so insertion order is wire order. Arguably P1 since the schema …[truncated; full text in the subsystem report]

#### `ST-09` — files_with_matches omits `totalFiles` and never emits "No entries at this offset" for empty pages (count mode too)

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/grep.rs:944-971 (files_with_matches: `let model = if num_files == 0 { "No files found" ... }` and the data map has no `totalFiles`) and lingxi-code/tools/file/src/grep.rs:889-893 (count: `if limited.is_empty() { "No matches found" }`). Content mode DOES implement the pattern correctly at grep.rs:826-843 via `totalLines`.`
- **oracle** 2.1.238 binary @289930790: `return{data:{mode:"files_with_matches",filenames:I,numFiles:I.length,totalFiles:A.length, ...x!==void 0&&{appliedLimit:x},...f>0&&{appliedOffset:f}}}`. @289936064: `if(t===0)return{...content:c&&(s??0)>0?`No entries at this offset. [Showing results with pagination = ${d}]`:"No files found"}` (t=numFiles, c=appliedOffset, s=totalFiles). Count branch: `g=n||(m>0?"No entries at this offset":"No matches found")` (m=numMatches). `totalFiles` is also a declared output field in sdk-tools-238.d.ts:3277.
- **fix** Add `totalFiles` (pre-pagination sorted-file count) to the files_with_matches data map, and mirror the content-mode branch: when the page is empty and `appliedOffset` is set and totalFiles>0, emit `No entries at this offset. [Showing results with pagination = {limit_info}]`; in count mode, when the page is empty and numMatches>0, emit `No entries at this offset` before appending the Found-N summary.
- **verifier** Oracle files_with_matches data includes `totalFiles:A.length`, and mapToolResultToToolResultBlockParam emits `No entries at this offset. [Showing results with pagination = ${d}]` when numFiles===0 && appliedOffset && totalFiles>0; the count branch uses `n||(m>0?"No entries at this offset":"No matches found")`. Port grep.rs:944-971 omits totalFiles and only ever says "No files found"; grep.rs:889-8 …[truncated; full text in the subsystem report]

#### `ST-10` — maxResultSizeChars wrong for both tools (oracle: Glob 100000, Grep 20000; port: 30000 for both)

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/glob.rs:191-193 and the identical method in lingxi-code/tools/file/src/grep.rs both return `tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH`, which is `30_000` (lingxi-code/tool-api/src/util/output_truncation.rs:22)`
- **oracle** 2.1.238 binary @289926104: `lhe=es({name:Bm,searchHint:"find files by name pattern or wildcard",maxResultSizeChars:1e5,...})`. @289933868: `Uve=es({name:Am,searchHint:"search file contents with regex (ripgrep)",maxResultSizeChars:20000,strict:!0,...})`. 2.1.220 identical (cc-bin-220 @232498562 `...wildcard",maxResultSizeChars:1e5` and @232504064 `...(ripgrep)",maxResultSizeChars:20000`).
- **fix** Return 100_000 from GlobTool::max_result_size_chars and 20_000 from GrepTool::max_result_size_chars as named per-tool constants, and add the two tools to the exemption/override list in test-harness/tests/parity_output_truncation.rs so the blanket 30_000 lock does not re-impose the wrong value.
- **verifier** Read both tool factory calls: Glob `es({name:Bm,searchHint:"find files by name pattern or wildcard",maxResultSizeChars:1e5,...})` @289926104 and Grep `es({name:Am,...,maxResultSizeChars:20000,strict:!0,...})` @289933868; 2.1.220 identical. Port glob.rs:191-193 and the matching grep.rs method both return MAX_TOOL_OUTPUT_LENGTH = 30_000 (tool-api/src/util/output_truncation.rs:22).

#### `ST-12` — Lean-prompt gate checks the model-name list before the lean_prompt capability (oracle checks capability first)

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tool-api/src/model_prompt_gate.rs:60-92 — `uwu_standard_model` runs the `claude-3-`/`haiku`/`sonnet`/`opus-4-x` name list at :66-76 and only then the `LeanPrompt` capability / `claude-mythos-5` check at :85-90`
- **oracle** 2.1.238 binary @285080767: `function BKb(e){if(imt(e))return!1;let t=Fo(e);if(F2(t,"lean_prompt")||t==="claude-mythos-5")return!1;if(t.includes("claude-3-")||t.includes("haiku")||t.includes("sonnet")||t==="claude-opus-4-0"||...||t==="claude-opus-4-7")return!0;return!v_()}` — capability check is FIRST. 2.1.220 `oug` (cc-bin-220 @228079500) has the same order.
- **fix** Move the `has_capability(&t, LeanPrompt) || t == "claude-mythos-5" => return false` block above the name-list block so the ordering matches `BKb`. Latent today (no registry model with lean_prompt contains haiku/sonnet/claude-3-), but the port comment claims 1:1 and it is not.
- **verifier** Oracle @285080767 read verbatim: `BKb(e){if(imt(e))return!1;let t=Fo(e);if(F2(t,"lean_prompt")||t==="claude-mythos-5")return!1;if(t.includes("claude-3-")||...)return!0;return!v_()}` - capability first. Port tool-api/src/model_prompt_gate.rs:60-92 runs the name list first. Confirmed latent: the capability table (platform-api/src/model_capabilities.rs:100-160) gives lean_prompt only to claude-opus-4-8, cl …[truncated; full text in the subsystem report]

#### `ST-13` — --max-columns 500 is hard-truncation instead of ripgrep's omission marker

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** likely
- **port** `lingxi-code/tools/file/src/grep.rs:298-310 (`decode_line`) truncates the line to 500 chars; the module header at grep.rs:23-24 documents this as an approximation ("`rg` prints an omission marker instead")`
- **oracle** 2.1.238 Grep arg builder (`Nhv`): `_.push("--max-columns","500")` with no `--max-columns-preview`, so ripgrep replaces an over-long matching line with `[Omitted long line with N matches]` rather than emitting the first 500 characters. 2.1.220 identical.
- **fix** When a decoded line exceeds MAX_COLUMNS, emit `[Omitted long line with {n} matches]` (n from `only_matching_spans(&matcher, text).len()`) in place of the content, keeping the `relpath:line:` prefix, rather than slicing to 500 chars.
- **verifier** Oracle arg builder Nhv confirmed at @289928728: `_.push("--max-columns","500")` with no --max-columns-preview; port grep.rs:298-310 hard-slices to 500 chars and the module header admits the approximation. CORRECTION to the finding: I ran the ripgrep the oracle ships (ARGV0=rg ~/.local/bin/claude, ripgrep 14.1.1) and the marker is `[Omitted long matching line]`, NOT `[Omitted long line with N match …[truncated; full text in the subsystem report]

#### `ST-14` — Grep suppresses -A/-B/-C context whenever -o is set; the oracle passes both to ripgrep

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** likely
- **port** `lingxi-code/tools/file/src/grep.rs:706-718 — `let (ctx_before, ctx_after) = if content_mode && !only_matching { ... } else { (None, None) }`, i.e. context is dropped whenever `-o` is true`
- **oracle** 2.1.238 Grep arg builder (`Nhv`): `if(c&&o==="content")_.push("-n");if(d&&o==="content")_.push("-o");if(o==="content")if(l!==void 0)_.push("-C",l.toString());else if(a!==void 0)_.push("-C",a.toString());else{if(i!==void 0)_.push("-B",i.toString());if(s!==void 0)_.push("-A",s.toString())}` — `-o` and the context flags are pushed together, unconditionally of each other. 2.1.220 identical.
- **fix** Drop the `!only_matching` condition so context is computed from `content_mode` alone, and emit context lines alongside the per-match `-o` output as ripgrep does. If the port's Sink cannot faithfully interleave them, keep the current behaviour but move the note from a passive comment into an explicit documented divergence.
- **verifier** Oracle Nhv pushes `-o` and the -C/-B/-A context flags in independent branches (read verbatim). Port grep.rs:706-718 zeroes context whenever only_matching is set, justified by the comment "rg -o ignores context entirely". I disproved that comment empirically against the oracle's own bundled rg: `ARGV0=rg claude -n -o -C 1 MATCH f.txt` prints `2-bbb / 3:MATCH / 4-ccc`. Confidence upgraded from likel …[truncated; full text in the subsystem report]

#### `ST-15` — Glob applies read-deny globs without the rooted/unrooted `!**/` prefixing the oracle uses

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/file/src/glob.rs:323-325 adds `format!("!{p}")` for every entry, with a comment asserting the reference "prefixes a bare `!` to every entry". Grep does implement the split correctly at lingxi-code/tools/file/src/grep.rs:668-675.`
- **oracle** 2.1.238 Glob arg builder (`async function PEf`): `for(let w of d)m.push("--glob",w.startsWith("/")?`!${w}`:`!**/${w}`)` — same split as the Grep builder. 2.1.220 identical (cc-bin-220 @232494294 region: `for(let C of d)m.push("--glob",C.startsWith("/")?`!${C}`:`!**/${C}`)`).
- **fix** Change glob.rs:323-325 to mirror grep.rs:668-675 — `if p.starts_with('/') { format!("!{p}") } else { format!("!**/{p}") }` — and correct the comment. Only observable for a multi-segment relative deny pattern (e.g. `secrets/*.key`), where `!P` anchors at the root but `!**/P` matches at any depth.
- **verifier** Oracle `async function PEf` @289923175 read verbatim: `for(let w of d)m.push("--glob",w.startsWith("/")?`!${w}`:`!**/${w}`)`. Port glob.rs:320-322 uses `format!("!{p}")` unconditionally with a comment asserting the reference prefixes a bare `!`; grep.rs:668-675 does implement the split, which makes the intra-repo inconsistency plain. Correctly scoped as P3 (only multi-segment relative deny pattern …[truncated; full text in the subsystem report]

### Task/Agent tool + subagent framework (Agent tool description & schema, built-in agent roster, SendMessage/ListAgents companions, agent_listing_delta scaffolding)

#### `AGT-01` — Agent tool `run_in_background` schema description is the 2.1.220 text

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:277 (schema) — "Agents run in the background by default; you will be notified when one completes. Set to false to run this agent synchronously when you need its result before continuing."; stale assertion locked at lingxi-code/tools/agent/src/agent_test.rs:687`
- **oracle** 2.1.238 @292882355: `Agents run in the background by default; you will be notified when one completes. Set to false only when your very next action depends on this agent's result and nothing else could usefully happen while it runs — otherwise leave it in the background so the user can hand you other work.`  Authoritative SDK diff sdk-tools-220-238.diff:103-104 shows the -/+ pair. `oracle.sh count` of the OLD sentence: 1 in 2.1.220, 0 in 2.1.238.
- **fix** Replace the `run_in_background` description literal in AGENT_INPUT_SCHEMA with the 2.1.238 bytes (em dash U+2014 between "while it runs" and "otherwise"), and update the agent_test.rs:687 expectation to match.
- **verifier** Verified at both ends. sdk-tools-220-238.diff carries the exact -/+ pair for run_in_background; oracle.sh count of the old sentence = 2 in 2.1.220, 0 in 2.1.238, and the new clause 'otherwise leave it in the background so the user can hand you other work' = 1 in 2.1.238. Port still emits the 2.1.220 bytes at lingxi-code/tools/agent/src/agent.rs:277, pinned by agent_test.rs:687. Model-visible schem …[truncated; full text in the subsystem report]

#### `AGT-02` — Agent tool prompt: background bullet still carries the 2.1.220 wording

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:790 (and duplicated fixture copy at lingxi-code/tools/agent/src/agent.rs:2464)`
- **oracle** 2.1.238 @292443853: `"\n- Subagents run in the background by default; you'll be notified when one completes. Pass `run_in_background: false` only when your very next action depends on the result and nothing else could usefully happen while it runs — otherwise background it so the user can interject. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running."`. 2.1.220 @234657xxx has `... Pass `run_in_background: false` for a synchronous run when you need the result before continuing. ...`
- **fix** Swap the `background_bullet` literal for the 2.1.238 sentence; update the duplicated expected-prompt fixture at agent.rs:2464.
- **verifier** Read 2.1.238 @292443770: the lean-branch bullet is '... Pass `run_in_background: false` only when your very next action depends on the result and nothing else could usefully happen while it runs — otherwise background it so the user can interject. ...'. Port agent.rs:790 (dup fixture agent.rs:2464, test agent_test.rs:2026) still says '... for a synchronous run when you need the result before conti …[truncated; full text in the subsystem report]

#### `AGT-04` — The entire LONG (non-lean) Agent tool prompt body is never rendered

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:1397 — `async fn prompt(&self, _: &PromptOptions)` discards the model; `build_prompt_with_async_agents` (agent.rs:633-800) only ever emits the SHORT form. absent: `rg '## Usage notes|## When not to use|Foreground vs background' lingxi-code/tools/agent` -> 0 hits`
- **oracle** 2.1.238 `CGf` @~292443000-292445600: `let m=qk(e); ... if(m){ <SHORT form> } return `${_}\n${v}\n## Usage notes\n\n- Always include a short description summarizing what the agent will do\n- ...\n- Trust but verify: ...${u&&!o?`\n- Agents run in the background by default. ... do NOT sleep, poll, or proactively check on its progress. ...\n- **Foreground vs background**: Pass `run_in_background: false` only when your very next action depends on the agent's result ...`:""}...`` plus `v` = `\n## When not to use\n\nIf the target is already known, use the direct tool: ${Ns} for a known path, ${S} for a specific symbol or string. ...` and the trailing `Example usage:` <example> blocks. `qk(e)=R_a().leanPrompt(e)=UKb(e)` @285081019/@285081366 is false for sonnet/haiku/claude-3-*/opus-4-0..4-7.
- **fix** Thread `PromptOptions.model` into `build_prompt` and branch on the already-ported `tool_api::dh_simple_system_prompt(model)` (tool-api/src/model_prompt_gate.rs:119) exactly as Read/Bash/TodoWrite do; add the LONG arm (`## When not to use`, `## Usage notes` bullets incl. the `u&&!o` background pair and the `u&&!i` Don't-race bullet, the `g`-gated proactive/parallel bullets, the worktree bullet, and the `Example usage:` examples).
- **verifier** Not a negative-grep claim: I read the oracle LONG arm at @292445000 ('## Usage notes', '**Foreground vs background**', "**Don't race**") and the lean gate UKb/BKb @285081019, which is FALSE (=> LONG) for sonnet/haiku/claude-3-*/opus-4-0..4-7. Port agent.rs:1397 discards PromptOptions.model and only ever builds the SHORT form; '## Usage notes' appears in the port solely inside comments (agent.rs:57 …[truncated; full text in the subsystem report]

#### `AGT-06` — NEW in 2.1.238: general-purpose-unavailable prompt arm and `subagent_type is required` spawn error

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:1580-1602 (omitted subagent_type unconditionally becomes GENERAL_PURPOSE_AGENT_TYPE) and lingxi-code/tools/agent/src/agent.rs:264-267 (unconditional "If omitted, the general-purpose agent is used."); `rg 'subagent_type is required|subagent_type_missing'` over lingxi-code -> 0 hits`
- **oracle** 2.1.238 @285270080: `Gri="subagent_type is required: the general-purpose agent is not available in this session"` (0 hits in 2.1.220). Prompt use @292433588: `... to select which agent type to use. ${n?"If omitted, the general-purpose agent is used.":s}` with `s=`${Gri}, so choose ${i?'`"fork"` or ':""}one of the listed agent types.``. Spawn use @292889305: `if(t===void 0&&!p7f(k,A)){N("tengu_subagent_type_miss",{requestedNormalized:xe("OMITTED"),availableCount:vt.length}),de("subagent_launch","subagent_type_missing");let Ot=x&&f7f(B)===null?[GAe,...Xt]:Xt;throw new ISt(`${Gri}. Available agents: ${YLi(Ot)}`)}`. 2.1.220's equivalent @234673007 goes straight to `let lt=t??$Fe.agentType` with no such check.
- **fix** Add a general-purpose-availability probe (port of `p7f`: normalized match of the general-purpose agentType against the active listing, honoring allowedAgentTypes); when it fails, make the prompt's subagent_type sentence use the `Gri, so choose ... one of the listed agent types.` form and make an omitted subagent_type throw `subagent_type is required: the general-purpose agent is not available in this session. Available agents: <list|none>`.
- **verifier** Oracle string Gri verified @285270085 with count 2 in 2.1.238 vs 0 in 2.1.220, and the throw site read verbatim @292889305 (tengu_subagent_type_miss / subagent_launch:subagent_type_missing / 'Available agents: ${YLi(Ot)}'). Port agent.rs:1580-1602 unconditionally substitutes GENERAL_PURPOSE_AGENT_TYPE with no availability probe, and the prompt sentence at agent.rs:693 is unconditional. P1 stands ( …[truncated; full text in the subsystem report]

#### `AGT-11` — SendMessage prompt: `"*"` broadcast row replaces the oracle's `"main"` row, and the name/agentId paragraph is a paraphrase

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/ui/src/send_message.rs:859 (`| `"*"` | Broadcast to all teammates — expensive (linear in team size), use only when everyone genuinely needs it |`) and lingxi-code/tools/ui/src/send_message.rs:884 (`Refer to teammates by name, never by UUID.`)`
- **oracle** 2.1.238 @293561470+ and 2.1.220 @106263265 both render: `| `to` | |\n|---|---|\n| `"researcher"` | Teammate by name |\n| `"main"` | The main conversation (background subagents only) |` and `... Refer to agents by name — names keep working after an agent completes (a send resumes it from its transcript). Use the raw `agentId` (format `a...-...`) from its spawn result only when the agent has no name, or when a newer agent took the name (latest wins). When relaying, don't quote the original — it's already rendered to the user.` (2.1.238's template even leaves a dead `${""}` slot and an unused `r=""` where a removed row was).
- **fix** Replace the `"*"` row with `| `"main"` | The main conversation (background subagents only) |` and restore the full three-clause agents-by-name / raw-agentId paragraph verbatim.
- **verifier** Read the oracle template verbatim (2.1.238 @293559100/@293561470 and the 2.1.220 UTF-16 copy @106263280): the row is `"main"` | The main conversation (background subagents only), plus the full agents-by-name/raw-agentId paragraph. count 'Broadcast to all teammates' and 'Refer to teammates by name, never by UUID' are 0 in BOTH versions. Port send_message.rs:861/884 diverges — and worse, send_messag …[truncated; full text in the subsystem report]

#### `AGT-12` — SendMessage "Protocol responses (legacy)" closing sentence changed in 2.1.238

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/ui/src/send_message.rs:897 — `... Don't send structured JSON status messages — use TaskUpdate.`, emitted unconditionally`
- **oracle** 2.1.238 @293561364: `... Don't send structured JSON status messages — report progress through your task tools if you have them, otherwise in plain prose.` (0 hits in 2.1.220, whose text is `... Don't send structured JSON status messages — use TaskUpdate.`, read at 2.1.220 @106263265+). The oracle also gates the whole section on the builder's boolean argument (`${e?'\n\n## Protocol responses (legacy)...':""}`).
- **fix** Update the closing sentence to the 2.1.238 bytes and gate the whole `## Protocol responses (legacy)` block on the same condition the oracle uses (swarm/teammate context) rather than emitting it always.
- **verifier** Verified both sides: 2.1.220 @235084295 ends '... Don\'t send structured JSON status messages — use TaskUpdate.' (count 1; 0 in 2.1.238), and 2.1.238 ends '... report progress through your task tools if you have them, otherwise in plain prose.' Port send_message.rs:897 carries the 2.1.220 sentence and emits the block unconditionally where the oracle gates it on `${e?...:""}`. P1 stands.

#### `AGT-03` — Agent tool prompt: fork addendum lost the two sentences 2.1.238 appended

- **severity** P2 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:742`
- **oracle** 2.1.238 @292444519: `A=i?`\n\nA fork runs in the background and keeps its tool output out of your context. If you are the fork, execute directly — don't re-delegate. Subagents run in the background; you'll be notified when one completes. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running.`:""`. 2.1.220 @234658240: same string ENDING at `don't re-delegate.`
- **fix** Append the two new sentences to the `fork_addendum` literal so it matches 2.1.238 (only observable when LINGXI_FORK_SUBAGENT is on, but it is model-visible then).
- **verifier** Oracle 2.1.238 @292444519 appends two sentences to the fork addendum; 2.1.220 @234658240 ends at "don't re-delegate." — both read directly. Port agent.rs:742 matches 2.1.220. Real drift, but the addendum only renders under LINGXI_FORK_SUBAGENT (default OFF), so no wrong bytes reach the model in a default session: severity corrected P1 -> P2.

#### `AGT-05` — `## When to use` ignores the subagent-steer gate (`DZ()==="default"`)

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:729-735 — `when_to_use` is unconditionally the long "Reach for this when..." arm; the steer seam `platform_api::live_sessions::subagent_steer_is_default()` exists and is already used at lingxi-code/orchestrator/src/prompt/body_sections.rs:234 but is not consulted here`
- **oracle** 2.1.238 @292441984 `g=DZ()==="default"` and the return template `${y?"":g?`\n\n## When to use\n\nReach for this when the task matches an available agent type, ... not the file dumps. ${R}`:`\n\n## When to use\n\n${R}`}` where R=`For a single-fact lookup where you already know the file, symbol, or value, search directly. Once you've delegated a search, don't also run it yourself — wait for the result.`; same structure at 2.1.220 @234658608.
- **fix** Gate the "Reach for this when the task matches...file dumps. " sentence on `platform_api::live_sessions::subagent_steer_is_default()`, falling back to the heading plus the single-fact-lookup sentence alone.
- **verifier** Confirmed: 2.1.238 @292441984 has g=DZ()==="default" selecting between the long and short '## When to use' arms (DZ @284211296 is the steer latch), same shape in 2.1.220. Port agent.rs:729-735 gates only on the pro block and always emits the long arm, while the steer seam platform_api::live_sessions::subagent_steer_is_default() (platform-api/src/live_sessions.rs:1405) is already used at orchestrator/src/promp …[truncated; full text in the subsystem report]

#### `AGT-07` — NEW in 2.1.238: built-in `web-fetch` agent missing from the roster

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/agent/src/builtins.rs:452-511 — roster is general-purpose, statusline-setup, Explore, Plan, workflow-subagent; `rg 'web-fetch' lingxi-code/agent lingxi-code/tools/agent` -> only an unrelated comment at tools/agent/src/agent.rs:2275`
- **oracle** 2.1.238 @287981417 `function vyt(){...let t=[hRe];if(!Ad())t.push(iKp);if(!AFr()){...}if(z5r())t.push(JQe,wgi);if(xgi())t.push(Hlr);...}`; `KH="web-fetch"` @287971650; `Hlr={agentType:KH,whenToUse:`Use this to fetch and read web pages / URLs when you do not have a direct ${cm} tool of your own (if you do, just call it). ...`,...}` with system prompt `You are a web-reading specialist for Claude Code, Anthropic's official CLI for Claude. ...`; gate `xgi()` @287975693 -> `Rgi()` @287975578 = `V.CLAUDE_CODE_WEB_FETCH_AGENT ?? it("tengu_clever_orbit",!1)`. Also new copy `[web-fetch agent] isolation:'<x>' ignored; the built-in web-fetch agent always runs as a local agent.` `oracle.sh count 'web-fetch agent'`: 0 in 2.1.220, 5 in 2.1.238.
- **fix** Add the `web-fetch` AgentDefinition (whenToUse + full system prompt + tools policy) behind an env/flag gate mirroring `CLAUDE_CODE_WEB_FETCH_AGENT`, default OFF, and port the isolation-override notice.
- **verifier** Verified: count 'web-fetch agent' = 0 in 2.1.220, 5 in 2.1.238; roster vyt() @287981417 pushes Hlr under xgi() @287975693 -> Rgi() @287975578 = CLAUDE_CODE_WEB_FETCH_AGENT ?? tengu_clever_orbit(false); isolation-override copy at @124755473. Port roster lingxi-code/agent/src/builtins.rs:452-511 lacks it. Not a HARD EXCLUSION — the exclusion names LingXi's own WebSearch (Tavily), not the built-in we …[truncated; full text in the subsystem report]

#### `AGT-08` — NEW in 2.1.238: "every tool denied" agent-type rejection not ported

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:1576-1690 — handles only agent_type_denied / agent_type_ambiguous / agent_type_not_found; `rg 'every tool it may use is denied' lingxi-code` -> 0 hits`
- **oracle** 2.1.238 @290291941: `function mdr(e,t){return NJa([e],t).length===0}function hdr(e){return`Agent type '${e}' is unavailable because every tool it may use is denied by the current permission settings.`}`; thrown at @292890415 with `de("subagent_launch","subagent_type_tools_denied")`. `oracle.sh count 'is unavailable because every tool it may use is denied'`: 0 in 2.1.220, present in 2.1.238.
- **fix** After the normalized single-match resolution, when the matched agent's tool set is fully denied by the permission gate, emit `subagent_type_tools_denied` and return `Agent type '<x>' is unavailable because every tool it may use is denied by the current permission settings.`
- **verifier** Read mdr/hdr @290291941 and the throw with de('subagent_launch','subagent_type_tools_denied') @292890415; count of the message = 2 in 2.1.238, 0 in 2.1.220 (genuine new drift). Port agent.rs:1576-1690 covers only agent_type_denied / ambiguous / not_found. P2 appropriate.

#### `AGT-10` — Advertised input_schema drops `run_in_background` on the Pro plan instead of on the fork flag

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:535-540 — `let background_available = !is_env_truthy(LINGXI_DISABLE_BACKGROUND_TASKS) && !platform_api::subscription::is_pro_plan();` consumed at agent.rs:1342-1349 (input_schema) and agent.rs:1893 (async dispatch)`
- **oracle** 2.1.238 @292883815 (identical at 2.1.220 @234666822): `nul=we(()=>{let e=e9v().omit({cwd:!0});return WA()||z1e()?e.omit({run_in_background:!0}):e})`, where `WA()` @286250643 = `wZe().backgroundTasksDisabled||V.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` and `z1e()` is the fork feature flag. All four `Cc()==="pro"` sites in 2.1.238 (284189548, 285093692, 292442005, 302317223) are the plan predicate, a model-picker suffix, the Agent-prompt discouragement block, and a statusline hint — none gates background agents.
- **fix** Replace `!platform_api::subscription::is_pro_plan()` with the fork-feature-flag term so the gate reads `disable_background_tasks || fork_feature_enabled`, matching `WA()||z1e()`; keep `is_pro_plan()` only for the prompt's discouragement block, which is where the oracle uses it.
- **verifier** Verified @292883815 `WA()||z1e()`, WA @286250643 (backgroundTasksDisabled || CLAUDE_CODE_DISABLE_BACKGROUND_TASKS) and z1e @286314232 (Obp()!=='disabled', the fork flag); enumerated all four Cc()==='pro' sites (284189548, 285093692, 292442005, 302317223) and none gates background. Port agent.rs:535-540 substitutes !platform_api::subscription::is_pro_plan(), consumed by input_schema (agent.rs:1342-1349) …[truncated; full text in the subsystem report]

#### `AGT-14` — ListAgents description tells the model in-process subagents are NOT listed; the oracle says they are

- **severity** P2 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/ui/src/list_agents.rs:25-31 — `Lists other local Claude sessions on this machine you can SendMessage to. In-process teammates are addressed by the name they were spawned with, not this list. ...``
- **oracle** 2.1.238 `YmS` @~286283398: `Lists agents you can ${Zm} to — in-process subagents you spawned, other local Claude sessions on this machine, ...`; 2.1.220 `Kdw` @230695038: `Lists agents you can ${mf} to — in-process subagents you spawned, other local Claude sessions on this machine, ...` — both open with the in-process-subagent clause.
- **fix** Restore the oracle's opening clause (`Lists agents you can SendMessage to — in-process subagents you spawned, other local Claude sessions on this machine, ...`), keeping only the cloud / Remote-Control clauses trimmed per the accepted divergence; drop the contradictory "not this list" sentence.
- **verifier** Read YmS @286282922 in 2.1.238 and confirmed 'in-process subagents you spawned' also counts 1 in 2.1.220 — so both oracle versions say in-process subagents ARE listed, while the port (list_agents.rs:25-31) tells the model they are NOT. Long-standing gap rather than new drift, and the port's call (list_agents.rs:194-209) genuinely only renders local sessions, so the copy tracks a real behavior gap; …[truncated; full text in the subsystem report]

#### `AGT-15` — `agent_listing_delta`: removal branch and the concurrency note are never emitted

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11887-11894 documents the omission ("DOCUMENTED DEFERRAL vs TS: the `removedTypes` branch ... and the subscription-conditioned 'launch multiple agents concurrently' note ... are omitted"); render at conversation.rs:11941-11954 emits header+lines only. `rg 'launch multiple agents for independent work' lingxi-code` -> 0 hits`
- **oracle** 2.1.238 @296704484: `case"agent_listing_delta":{...if(i.length>0)s.push(`The following agent types are no longer available:\n${i.map((a)=>`- ${a}`).join("\n")}`),s.push($io);if(n.length>0&&o.length>0&&e.isInitial&&e.showConcurrencyNote)s.push("When you launch multiple agents for independent work, send them in a single message with multiple tool uses so they run concurrently.");...}` with `$io` @296730196 = `This is ambient context — do not narrate it to the user unless they ask or it is directly relevant to their request.`; producer @296530704 sets `showConcurrencyNote:Cc()!=="pro"&&DZ()==="default"` and sorts added entries by `localeCompare`. Both literals also count 2 in 2.1.220.
- **fix** Emit the concurrency note on the initial listing when the steer is default and the plan is not Pro (both seams already exist: `platform_api::live_sessions::subagent_steer_is_default()` and `platform_api::subscription::is_pro_plan()`), and add the `removedTypes` branch plus the ambient-context trailer (already a constant at tool-api/src/defer.rs:443).
- **verifier** Read the oracle case @296704484 (removedTypes branch + $io ambient trailer + the concurrency note gated on isInitial && showConcurrencyNote) and the producer @296530704 (showConcurrencyNote: Cc()!=='pro' && DZ()==='default'). Both literals also exist in 2.1.220, so this is an old gap, not new drift — the finding says as much. Port conversation.rs:11887-11894 documents the deferral and the render a …[truncated; full text in the subsystem report]

#### `AGT-09` — NEW in 2.1.238: empty available-agents list must render as `none`

- **severity** P3 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/agent/src/agent.rs:1650 and lingxi-code/tools/agent/src/agent.rs:1685 — `available().join(", ")` with no empty fallback`
- **oracle** 2.1.238 @292880950: `function YLi(e){return e.join(", ")||"none"}`, used by the ambiguous, not-found and new missing-type messages. 2.1.220 @234670299 / @234674018 used bare `${et.join(", ")}` / `${Ft.join(", ")}`.
- **fix** Introduce a helper equivalent to `YLi` (`if list.is_empty() { "none" } else { list.join(", ") }`) and use it at every `Available agents: ` interpolation.
- **verifier** Verified YLi @292880950 in 2.1.238 (`join(", ")||"none"`) against the bare joins in 2.1.220 at @234670299 and @234674018, and confirmed the port uses available().join(", ") with no fallback at agent.rs:1650 and agent.rs:1685. Only observable on an empty listing => P3 correct.

#### `AGT-13` — SendMessage cross-session paragraph: busy/idle clause diverges

- **severity** P3 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tools/ui/src/send_message.rs:876 — `A listed peer is alive and will process your message — no "busy" state; messages enqueue and drain at the receiver's next tool round.``
- **oracle** 2.1.238 (inside `Xnm`'s `n` block, @~293562xxx): `A listed peer is alive and will process your message; messages enqueue and drain at the receiver's next tool round (its `ListAgents` row says whether it is busy or idle right now).` — `## Cross-session` does not exist at all in 2.1.220 (`oracle.sh show '## Cross-session'` -> not found).
- **fix** Align the sentence to the 2.1.238 bytes (drop the `— no "busy" state` claim, add the parenthetical about the ListAgents busy/idle column). The rest of the 2.1.238 delta in this block (notify_when_idle, cross-machine/cloud) is excluded scope.
- **verifier** Confirmed @293558417 in 2.1.238 (count 1) with the parenthetical about the ListAgents busy/idle column; 'A listed peer is alive' and '## Cross-session' are both 0-count in 2.1.220, so the whole section is new. Port send_message.rs:876 asserts 'no "busy" state'. Gated on cross-session messaging, so P3 is fine.

### Main system prompt

#### `SP-1` — `# Communicating with the user`: final-message sentence re-punctuated in 2.1.238

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:160 — `"\n\nText you write between tool calls may not be shown to the user. Everything the user needs from this turn \u{2014} answers, summaries, findings, conclusions, deliverables \u{2014} must be in the final text message of your turn, ..."``
- **oracle** 2.1.238 @ byte 297049247 (fn T9T): "Text you write between tool calls may not be shown to the user. Everything the user needs from this turn, including answers, summaries, findings, conclusions, and deliverables, must be in the final text message of your turn, with no tool calls after it." — byte count of "conclusions, and deliverables, must be in the final": 238=2, 220=0; count of "conclusions, deliverables — must be in the final": 238=0, 220=1.
- **fix** In `communicating_with_the_user_section`'s `final_message_paragraph`, replace `from this turn \u{2014} answers, summaries, findings, conclusions, deliverables \u{2014} must be` with `from this turn, including answers, summaries, findings, conclusions, and deliverables, must be`.
- **verifier** Confirmed at oracle 2.1.238 fn T9T (ctx @297049247): 'Everything the user needs from this turn, including answers, summaries, findings, conclusions, and deliverables, must be in the final text message of your turn'. 2.1.220 @237463547 shows the em-dash form '— answers, summaries, findings, conclusions, deliverables —'. Port /lingxi-code/orchestrator/src/prompt/body_sections.rs:160 still emits the …[truncated; full text in the subsystem report]

#### `SP-2` — `# Communicating with the user`: "what did you find" separator changed from em dash to colon

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:165 — `... should answer \"what happened\" or \"what did you find\" \u{2014} the thing the user would ask for ...``
- **oracle** 2.1.238 @ byte 297049689: 'Lead with the outcome. Your first sentence after finishing should answer "what happened" or "what did you find": the thing the user would ask for if they said "just give me the TLDR."' — count of 'what did you find": the thing the user': 238=1, 220=0.
- **fix** Replace `\"what did you find\" \u{2014} the thing` with `\"what did you find\": the thing` in the format! literal.
- **verifier** Confirmed: 238 T9T reads 'should answer "what happened" or "what did you find": the thing the user would ask for'; 220 @237463547 reads '"what did you find" — the thing'. Port body_sections.rs:165 carries the em-dash form. Real drift.

#### `SP-3` — `# Communicating with the user`: "Calibrate to the user" separator changed from em dash to colon

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:165 — `... Calibrate to the user \u{2014} a bit tighter for an expert, more explanatory for someone newer.``
- **oracle** 2.1.238 @ byte 297050700: "... Calibrate to the user: a bit tighter for an expert, more explanatory for someone newer." — count of "Calibrate to the user: a bit tighter": 238=1, 220=0; count of "Calibrate to the user — a bit tighter": 238=0, 220=1.
- **fix** Replace `Calibrate to the user \u{2014} a bit tighter` with `Calibrate to the user: a bit tighter`.
- **verifier** Confirmed: 238 T9T reads 'Calibrate to the user: a bit tighter for an expert'; 220 reads 'Calibrate to the user — a bit tighter'. Port body_sections.rs:165 keeps the em dash. Real drift.

#### `SP-4` — `# Communicating with the user`: code-comment sentence changed twice ("can't show, never" and "the change merges")

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:165 — `Only write a code comment to state a constraint the code itself can't show \u{2014} never to say ... and it's noise the moment the PR merges.``
- **oracle** 2.1.238 @ byte 297050932: "Only write a code comment to state a constraint the code itself can't show, never to say where it came from, what the next line does, or why your change is correct; that's you talking to the reviewer, not the next reader, and it's noise the moment the change merges." — counts: "the code itself can't show, never to say" 238=1 / 220=0; "noise the moment the change merges" 238=1 / 220=0; "noise the moment the PR merges" 238=1 / 220=2 (the surviving 238 hit is at 305633198, inside a Markdown model-migration doc, not the prompt).
- **fix** Replace `can't show \u{2014} never to say` with `can't show, never to say`, and `noise the moment the PR merges.` with `noise the moment the change merges.`
- **verifier** Confirmed both edits in one sentence: 238 reads 'the code itself can't show, never to say where it came from ... noise the moment the change merges.'; 220 reads 'can't show — never to say ... noise the moment the PR merges.'. Port body_sections.rs:165 has the 220 wording and body_sections.rs:1164 asserts '...noise the moment the PR merges.'. Real drift.

#### `SP-5` — `action_caution` (lean, non-Opus-5): "look at the target — if what you find" became its own sentence

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:313 — `" \u{2014} if what you find contradicts how it was described, or you didn't create it, surface that instead of proceeding"``
- **oracle** 2.1.238 @ byte 297052981 (fn w9T): "Before deleting or overwriting, look at the target${YQd(e)?\"\":\". If what you find contradicts how it was described, or you didn't create it, surface that instead of proceeding\"}. Report outcomes faithfully: ..." — the clause now starts with ". If". 2.1.220 @ 113625834/237-region carried " — if what you find contradicts ..." (cmp.py: present in 220, absent in 238).
- **fix** Change the `extra` literal in `action_caution_section` to `". If what you find contradicts how it was described, or you didn't create it, surface that instead of proceeding"` (leading period+space, capital If, no leading space).
- **verifier** Confirmed at 238 fn w9T (ctx @297052900): 'look at the target${YQd(e)?"":". If what you find contradicts how it was described, or you didn't create it, surface that instead of proceeding"}'. 2.1.220 @113625834 shows the same clause introduced by ' — if what you find'. Port action_caution_section extra literal at body_sections.rs:313 is the 220 em-dash/lowercase form. Real drift.

#### `SP-6` — Fable/Mythos autonomy tail: "system state — restarts, deletes, config edits —" became a parenthetical

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:353 — `FABLE_MYTHOS_MITIGATIONS` final paragraph still reads `... changes system state — restarts, deletes, config edits — check that ...``
- **oracle** 2.1.238 @ byte 297055870 (fn I9T): "Before running a command that changes system state (such as restarts, deletes, or config edits), check that the evidence actually supports that specific action." vs 2.1.220 @ 237470252: "Before running a command that changes system state — restarts, deletes, config edits — check that the evidence actually supports that specific action." Counts for the 238 form: 238=1, 220=0.
- **fix** Replace `changes system state — restarts, deletes, config edits — check` with `changes system state (such as restarts, deletes, or config edits), check` in FABLE_MYTHOS_MITIGATIONS.
- **verifier** Confirmed at 238 @297055870: 'Before running a command that changes system state (such as restarts, deletes, or config edits), check that the evidence actually supports that specific action.'; count of that exact clause is 238=1 / 220=0. Port FABLE_MYTHOS_MITIGATIONS at body_sections.rs:353 still reads 'system state — restarts, deletes, config edits — check'. Real drift.

#### `SP-7` — Fork-delegation guidance: upstream deleted "(or omitting it)"

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/body_sections.rs:233 — `... Other subagent_type values (or omitting it) start fresh agents with no context. ...``
- **oracle** 2.1.238 @ byte 297068165 (fn $9T): "... Other subagent_type values start fresh agents with no context. **If you ARE the fork** — execute directly; do not re-delegate." Counts: "Other subagent_type values start fresh agents with no context" 238=1 / 220=0; "Other subagent_type values (or omitting it) start fresh agents with no context" 238=0 / 220=1.
- **fix** Delete ` (or omitting it)` from the fork bullet in `session_guidance` (and from the matching assertion in the file's test module around body_sections.rs:1268 if it pins the phrase).
- **verifier** Confirmed at 238 fn $9T (@297068192): 'Other subagent_type values start fresh agents with no context.'; literal 'Other subagent_type values (or omitting it) start fresh agents' counts 238=0 / 220=1. Port body_sections.rs:233 emits the parenthetical. Fork bullet is reachable (fork_mode_enabled defaults on when interactive). Real drift.

#### `SP-8` — `# Environment` fast-mode line still advertises Opus 4.7, which 2.1.238 dropped

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/orchestrator/src/prompt/env_block.rs:176-178 (literal ends `available on Opus 5/4.8/4.7.`); pinned by env_block.rs:228 and orchestrator/tests/prompt_orchestrator_wiring_test.rs:58; stale citation in orchestrator/src/provider_adapter.rs:1348`
- **oracle** 2.1.238 @ bytes 297075805 and 297076453 (fns K9T and Y9T): "Fast mode for Claude Code uses Claude Opus with faster output (it does not downgrade to a smaller model). It can be toggled with /fast and is available on Opus 5/4.8." Counts: "It can be toggled with /fast and is available on Opus 5/4.8." 238=4 / 220=0; "... on Opus 5/4.8/4.7." 238=0 / 220=4.
- **fix** Change the env-block literal to `... is available on Opus 5/4.8.` and update the two assertions (env_block.rs:228, prompt_orchestrator_wiring_test.rs:58); refresh the oracle citation comment at provider_adapter.rs:1348.
- **verifier** Confirmed: 'is available on Opus 5/4.8.' counts 238=4 / 220=0; 'is available on Opus 5/4.8/4.7.' counts 238=0 / 220=4, and the 238 env string @141889072 ends '/fast and is available on Opus 5/4.8.'. Port env_block.rs:178 still ends '/4.8/4.7.'. Finding under-lists the pins (also orchestrator/tests/prompt_env_block_test.rs:53 and platform-api/src/model_capabilities.rs:111 comment) but that strengthens ra …[truncated; full text in the subsystem report]

#### `SP-9` — New-in-238 `turn_updates` communication variant (v9T) has no port equivalent

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg "turn_updates|Before you start, say in a line what you're about to do" --type rust` over lingxi-code returns 0 hits; orchestrator/src/prompt/body_sections.rs:121 `anti_verbosity_section` has only the three 2.1.220 arms (communicating / one-liner / # Text output).`
- **oracle** 2.1.238 @ byte 297082655: v9T = "Before you start, say in a line what you're about to do; brief updates while you work help the user follow along. Close with a short recap that stands on its own — what you found, what you did, and what's next — so a reader who only sees the last message has the full picture." Selected first in T9T via `if(JJr("turn_updates",V.CLAUDE_CODE_TURN_UPDATES,t))return v9T;`. Counts: 238=1, 220=0.
- **fix** Add a fourth, first-checked arm to `anti_verbosity_section` returning the v9T literal when `CLAUDE_CODE_TURN_UPDATES`/`LINGXI_TURN_UPDATES` is truthy. Low urgency: `turn_updates` appears only twice in the whole 2.1.238 binary (V8 string table + the call site), so no model in the capability table declares it and the section is unreachable without the env var.
- **verifier** Confirmed: v9T exists at 238 @297082655 with exactly the quoted text and is selected first in T9T via JJr("turn_updates", V.CLAUDE_CODE_TURN_UPDATES, t); 'turn_updates' occurs 2x in 238 and 0x in 220. Port grep for both the env name and the literal returns 0 hits, and anti_verbosity_section (body_sections.rs:121) has only the three 220 arms. Severity P3 is correct and already self-limited: JJr req …[truncated; full text in the subsystem report]

#### `SP-10` — `tool_param_json` paragraph (k9T) never ported

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** likely
- **port** `absent — `rg "Object and array parameter values must be a single JSON value" --type rust` over lingxi-code returns 0 hits; no `tool_param_json` slot exists in orchestrator/src/prompt/body_sections.rs `format()` (body_sections.rs:536-600).`
- **oracle** 2.1.238 @ byte 297083649 (also 2.1.220 @ 237498803): k9T = "Object and array parameter values must be a single JSON value — never write parameter-tag markup inside a JSON value.", registered as slot `aB("tool_param_json", () => z4d()||(($Xe(i)||Vpe(t))&&it("tengu_silent_harbor",!1)) ? k9T : null)`.
- **fix** Port the literal behind a slot mirroring `z4d()` (`g1n().toolParamStrictness`) plus the `tengu_silent_harbor` arm. Not 238 drift — the text is identical in 2.1.220 — and both gates are off in a default install, so this is a long-standing, unreachable-by-default gap.
- **verifier** Confirmed: k9T at 238 @297083649 with the quoted text, registered at @297072456 as aB("tool_param_json",()=>z4d()||(($Xe(i)||Vpe(t))&&it("tengu_silent_harbor",!1))?k9T:null); z4d()=g1n().toolParamStrictness (bracken_spool gate, default false). Text count is 1 in both 220 and 238, so it is a long-standing gap rather than 238 drift, exactly as the finding states. Port grep for the literal and for a …[truncated; full text in the subsystem report]

### system-reminder family and interstitials

#### `REM-01` — date_change reminder still ships the 2.1.220 sentence ("DO NOT mention this…") instead of 2.1.238's rewritten copy

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11786-11790 (literal), pinned by lingxi-code/orchestrator/src/conversation_test.rs:2447`
- **oracle** 2.1.238 @296739637 (string-table copy @256746832): `date_change:(e)=>Zy([kn({content:`The date has changed. Today's date is now ${e.newDate}. No need to announce the new date — the user's own clock shows it.`,isMeta:!0})]),`  —  2.1.220 same renderer: `…Today's date is now ${e.newDate}. DO NOT mention this to the user explicitly because they are already aware.`
- **fix** Replace the tail of the format! literal with `No need to announce the new date \u{2014} the user's own clock shows it.` (U+2014 em dash) keeping the `<\system-reminder>\n…\n<\/system-reminder>` wrapper, and update the expected string in conversation_test.rs:2447.
- **verifier** Reproduced on both sides. Oracle 2.1.238 @296739603: date_change:(e)=>Zy([kn({content:`The date has changed. Today's date is now ${e.newDate}. No need to announce the new date — the user's own clock shows it.`,isMeta:!0})]) — UTF-16 table @256746832 decodes to the same sentence with a real em dash. Oracle 2.1.220 @208887424 has the old `. DO NOT mention this to the user explicitly because they are …[truncated; full text in the subsystem report]

#### `REM-02` — Plan-mode reminders are injected without the <\system-reminder> wrapper (and as non-meta user messages)

- **severity** P1 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11488-11490 — `let content = crate::prompt::plan_reminder::render_plan_mode_reminder(&params); Some(ConversationMessage::user(MessageId::new(), content))`; renderer returns the bare body (orchestrator/src/prompt/plan_reminder.rs:147,230) and the unwrapped form is pinned by conversation_test.rs:3468/3517`
- **oracle** 2.1.238 `M5T` full-variant tail @296685746: `return Zy([kn({content:a,isMeta:!0})])`; `L5T` (sparse) and `H5T` (subagent) identical; dispatch `I5T` @296675801. `Zy` @296675470 maps `NT` over the content, and `NT` @296673554 is `function NT(e){return`<\system-reminder>\n${e}\n<\/system-reminder>`}`. 2.1.220 identical via `pm`/`Ww` @238048739.
- **fix** Wrap the rendered body: `format!("<\system-reminder>\n{content}\n<\/system-reminder>")` and emit with `ConversationMessage::user_meta` (as output_style/skill_listing/agent_listing already do); update conversation_test.rs:3468/3517.
- **verifier** Verified NT @296673554 (`return `<\system-reminder>\n${e}\n<\/system-reminder>``) and Zy @296675470 mapping NT over message text; M5T tail @296685746 is `return Zy([kn({content:a,isMeta:!0})])`; 220 identical via pm @238048739. Port: plan_reminder.rs render_full/render_sparse/render_subagent return bare bodies (the file does not appear in `rg -l '<\system-reminder>' -g '*.rs'`) and conversation.rs …[truncated; full text in the subsystem report]

#### `REM-03` — Todo/Task reminders are injected without the <\system-reminder> wrapper

- **severity** P1 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11724 and :11746 — `Some(ConversationMessage::user(MessageId::new(), content))` with the raw body; the port's doc claims the opposite (orchestrator/src/prompt/todo_reminder.rs:23-25) and conversation_test.rs:7650 pins it with the comment "// RAW body — NOT wrapped in <\system-reminder>."`
- **oracle** 2.1.238 `case"todo_reminder"` @296690005 ends `return Zy([kn({content:o,isMeta:!0})])`; `case"task_reminder"` @296690634 ends `return Zy([kn({content:o,isMeta:!0})])`. 2.1.220 identical at @238062271 (`pm([zr({…,isMeta:!0})])`). Wrapper `NT` @296673554.
- **fix** Wrap both V1 and V2 bodies in `<\system-reminder>\n…\n<\/system-reminder>`, emit via `user_meta`, fix the module doc in prompt/todo_reminder.rs, and update the byte-exact tests in conversation_test.rs (~7650 and the V2 twin).
- **verifier** Oracle 238 @296690005 case"todo_reminder" and the adjacent case"task_reminder" both end `return Zy([kn({content:o,isMeta:!0})])`; 220 @238062271 identical with pm([zr(...)]). Port tools/task/src/reminder.rs:131-161 returns raw bodies and its own doc (line 36) asserts the opposite about the oracle; conversation.rs:11724/11746 emit ConversationMessage::user. Confirmed.

#### `REM-04` — New silent-turn reminder (CLAUDE_CODE_SILENT_TURN_REMINDER) is entirely absent

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n 'silent_turn|SILENT_TURN|hushed_lark|hasn.t heard from you' -i` over lingxi-code/ returns 0 hits`
- **oracle** 2.1.238 text constant `a3m` @296477528: `"The user hasn't heard from you in a while. As you continue, keep them updated when there's something to tell — a finding, a change of plan."`; producer `K4T` @296525255 (`if(r>=l3m||t<d3m())return[]`, l3m=3, default turns s3m=5); config `c3m`/`u3m`/`d3m` @296476775-296477561 reading CLAUDE_CODE_SILENT_TURN_REMINDER{,_TEXT,_TURNS}; renderer `silent_turn_reminder:(e)=>[kn({content:NT(e.text),isMeta:!0})]` in `Cqm`; gate in the fan-out @296520120. Absent from 2.1.220 (attachment registry diff `unE` @301006047 vs `Gjb` @241605813).
- **fix** Port `Ezm` (count assistant turns since the last 'speaking' turn / reminders in the stretch, where speaking = non-empty text block or a tool_use in {AskUserQuestion, Brief}) and `K4T`; emit `<\system-reminder>`-wrapped meta text with the three env overrides and defaults turns=5, max-per-stretch=3, gated to the main agent on non-user-prompt turns.
- **verifier** Oracle 238 @296477528 has a3m with the exact sentence, s3m=5, l3m=3, and CLAUDE_CODE_SILENT_TURN_REMINDER{,_TURNS}; producer K4T and counter Ezm verified near 296525255 (`if(r>=l3m||t<d3m())return[]`); silent_turn_reminder occurs 15x. 2.1.220 count of the sentence = 0, so it is new upstream. Port: rg for silent_turn/SILENT_TURN/'heard from you' returns 0 hits — absence proven by behaviour text, no …[truncated; full text in the subsystem report]

#### `REM-05` — The per-turn changed-files reminder (edited_text_file) does not exist, and upstream rewrote its copy in 2.1.238

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n "either by the user or by a linter|changed on disk since you last read it"` matches only the Write/Edit staleness error at lingxi-code/tools/file/src/lib.rs:93; no `changed_files`/`edited_text_file` producer exists in orchestrator/ or tool-api/ though tool_api::read_file_state already stores mtime_ms`
- **oracle** 2.1.238 renderer @296733495: `Note: ${Kae(e.filename)} changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself — otherwise no need to call it out.` (+ ` Here are the relevant changes (shown with line numbers):\n${e.snippet}` / snippet-budget variant); producer `Izm` @296537358 (mtime-vs-readFileState scan, budget m3T=16384). 2.1.220 @238102310 said `Note: ${e.filename} was modified, either by the user or by a linter. …`
- **fix** Add a per-turn producer that walks `read_file_state` (skipping offset/limit entries), re-reads files whose mtime exceeds the recorded timestamp, diffs them, and emits the 2.1.238 wording wrapped in `<\system-reminder>`, honouring the 16384-char cumulative snippet budget (over-budget entries render the 'diff is omitted here' variant).
- **verifier** Oracle 238 @296733495 quotes verbatim, including the omitted-diff variant; oracle 220 @238102310 has the older `was modified, either by the user or by a linter … Don't tell the user this`. Port has no producer: rg for edited_text_file / 'snippet budget' / 'shown with line numbers' = 0 hits, and the only 220-phrase match is the Edit/Write staleness error in tools/file/src/lib.rs (a different surfac …[truncated; full text in the subsystem report]

#### `REM-06` — total_tokens_reminder (`<total_tokens>N tokens left</total_tokens>`) is not emitted at all

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n '<total_tokens>'` in lingxi-code matches only the task-result footer `<usage><total_tokens>…</total_tokens></usage>` (tasks/src/handlers/local_agent.rs:475, tasks/src/handlers/dream.rs:374); no CLAUDE_CODE_TOTAL_TOKENS_REMINDER* handling anywhere`
- **oracle** 2.1.238 `function dOi(e,t){return`<total_tokens>${e==="infinite"?"Infinite":e==="fixed"?zBv:Math.max(0,t)} tokens left</total_tokens>`}` @292022017 (zBv=5000000, budget default X3f=15000000); mode resolver `srt()`/`qBv()` @292020823 defaults to `"padded-countdown"` (on); producer `D3T` @296556375; renderer `total_tokens_reminder:(e)=>[kn({content:NT(e.text),isMeta:!0})]`. Identical in 2.1.220 (`Qdo` @230540538).
- **fix** Port `srt()`/`uOi()`/`Q3f()` (env → settings → default `padded-countdown` / budget 15_000_000) and `dOi`, emitting the `<\system-reminder>`-wrapped `<total_tokens>…</total_tokens>` block after each tool-result batch and, when the after-user-turn flag is on, after each regular user prompt.
- **verifier** Oracle 238 @292022017 has dOi exactly as quoted; qBv() resolves to 'padded-countdown' by default (`let o=it(Lil,"padded-countdown");return cOi(o)?o:"padded-countdown"`), zBv=5000000, X3f=15000000, three CLAUDE_CODE_TOTAL_TOKENS_REMINDER* env vars; the 238 typedoc @86329153 confirms it is emitted in the system prompt, after each tool result, and after user prompts. Present in 220 too. Port: only th …[truncated; full text in the subsystem report]

#### `REM-07` — New bash_output_audience_note reminder missing

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n "Only you see"` over lingxi-code returns 0 hits`
- **oracle** 2.1.238 renderer @296736428: `bash_output_audience_note:()=>Zy([kn({content:"Only you see that command's output — the user's terminal shows at most a few lines of it. If the user needs to read any of it, put it in your reply.",isMeta:!0})]),`; gate `kpm(e,t,r)` @294267076 (tool is Bash, long string stdout, interactive, model capability `bash_output_audience_note` / env CLAUDE_CODE_BASH_OUTPUT_AUDIENCE_NOTE). Not present in 2.1.220's attachment registry `Gjb` @241605813.
- **fix** After a Bash tool_result whose stdout exceeds the oracle's length predicate, append the quoted note as a `<\system-reminder>` meta message, gated on interactive sessions and the model-capability/env flag.
- **verifier** Oracle 238 @296736428 renderer reads verbatim; 220 count = 0, so it is new. Port: rg 'Only you see|bash_output_audience' = 0 hits. P2 is defensible given the interactive + model-capability/env gate (P1 would also be arguable since it is model-visible copy), so no severity correction.

#### `REM-08` — Plan-mode reminder cadence and full/sparse rotation diverge (port fires every turn, full only once)

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11473-11476 — `let sparse = s.plan_reminder_shown; s.plan_reminder_shown = true;` with no turn-gap gate; callers push it on every turn (turn_loop.rs:430, conversation.rs:8659)`
- **oracle** 2.1.238 producer `X4T` @296525982: `if(t&&t.length>0){let{turnCount:y,foundPlanModeAttachment:_}=ixl(t);if(_&&y<txl.TURNS_BETWEEN_ATTACHMENTS)return[]}` and `let c=Y4T(t??[])+1, … p = … || c%txl.FULL_REMINDER_EVERY_N_ATTACHMENTS===1?"full":"sparse"` with `txl={TURNS_BETWEEN_ATTACHMENTS:5,FULL_REMINDER_EVERY_N_ATTACHMENTS:5}` — at most one reminder per 5 non-meta user turns, full on the 1st/6th/11th.
- **fix** Count non-meta user turns since the last plan_mode reminder and skip when < 5; track the plan_mode attachment count since the last plan_mode_exit and render "full" when count % 5 == 1, "sparse" otherwise, replacing the boolean `plan_reminder_shown`.
- **verifier** Oracle txl={TURNS_BETWEEN_ATTACHMENTS:5,FULL_REMINDER_EVERY_N_ATTACHMENTS:5} @296558044; X4T @296525982 early-returns on `_&&y<5` and selects full via `c%5===1` with c=Y4T(history)+1; ixl @296524028 counts only non-meta user turns. Port conversation.rs:11473-11476 uses a one-shot boolean plan_reminder_shown with no gap gate or rotation, and turn_loop.rs:429-431 pushes it every turn. Confirmed; 220 …[truncated; full text in the subsystem report]

#### `REM-09` — Goal check-in interstitial (new in 2.1.238) is missing

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `ActiveGoalState` exists (lingxi-code/orchestrator/src/handle_impl.rs:216) but `rg -n "Goal check-in|deferredSince|deferred_since|GOAL_CHECKIN" -i` returns 0 hits`
- **oracle** 2.1.238 `Szf(e,t,r)` @292038578 builds `Goal check-in: \xAB${goal}\xBB is still active. Its evaluation was deferred for ${n} min while background work ran, and that work is no longer running (it finished or was stopped without reporting back). Continue toward the goal.` and the running-work variant `Goal check-in: \xAB${goal}\xBB is still active, and evaluation has been deferred for ${n} min because background work is still running:\n${lines}\nCheck on their progress …`; interval `Zil()` @292038365 (`CLAUDE_CODE_GOAL_CHECKIN_MINUTES`, default 30 min); injected at turn end as `kn({content:checkinText,isMeta:!0})` @292176475. `CC_VER=2.1.220 oracle.sh count 'Goal check-in'` = 0.
- **fix** Add goal-evaluation deferral state (deferredSince / checkinCount / lastDeferralPassAt) and inject the quoted check-in text as a meta user message when the goal's Stop-hook evaluation is deferred by running background work for longer than the configured interval.
- **verifier** Oracle 238 string table @120864400 carries every quoted fragment of both the 'no longer running' and 'still running' variants; 2.1.220 count of 'Goal check-in' = 0, so it is new in 238. Port has the goal feature (core/src/session.rs:63 ActiveGoalState, handle_impl.rs:216/320, resume.rs:478) but zero hits for the check-in text or any deferral state, so this is a real absence, not a rename. Not in …[truncated; full text in the subsystem report]

#### `REM-10` — tool_search_usage_reminder is missing (pre-existing gap; text unchanged 220→238)

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n "gentle reminder" -l` over lingxi-code matches only tools/task/src/reminder.rs and its tests; the port's deferred-tool announcement (tool-api/src/defer.rs:489) is the separate `deferred_tools_delta` attachment`
- **oracle** 2.1.238 @139328720 / @296691448: `Some available tools' schemas are not loaded in this conversation yet: ${i}. Before concluding a capability is missing or building a workaround, use ${y0} to find and load relevant tools — keywords to search, or query "select:<name>[,<name>...]" for specific tools. Calling a tool before its schema is loaded will fail. This is just a gentle reminder - ignore if not applicable to the current work.`; producer `Uzm` (turn-count `R3T`, everyNTurns/maxNames from `Lda()`). Same string present in 2.1.220.
- **fix** Emit the quoted reminder every N turns while undiscovered deferred tools remain (skipping the turn a todo/task reminder already fired), listing at most `maxNames` names with a `(+K more)` suffix.
- **verifier** Oracle 238 UTF-16 table @139328720 plus the case"tool_search_usage_reminder" branch right after task_reminder; the same string is in 220 (count 2), so it is a pre-existing gap as the finding states. Port: 0 hits for the reminder text; tool-api/src/defer.rs render_reminder is the distinct deferred_tools_delta announcement with different copy and one-shot-per-delta semantics, so it is not the same b …[truncated; full text in the subsystem report]

#### `REM-11` — output_style reminder ignores the style's turnReminder override (and lacks 2.1.238's 256-char suppression / escaping)

- **severity** P2 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11405-11412 hardcodes "Remember to follow the specific guidelines for this style."; `rg -n "turn_reminder|turnReminder" -i` finds no per-style field anywhere in the port`
- **oracle** 2.1.238 `Cqm.output_style`: `if(e.style.length>gFn)return T(`Output style name exceeds ${gFn} characters …`),[] ; return Zy([kn({content:`${pze(e.style)} output style is active. ${e.turnReminder??"Remember to follow the specific guidelines for this style."}`,isMeta:!0})])` with `gFn=256` @285128933; producer `s3T()` passes `turnReminder:r.turnReminder`, and built-in styles carry one (`{…,turnReminder:j3S}` / `q3S`). 2.1.220 already had the `??` override.
- **fix** Add a `turn_reminder` field to the resolved output style (built-in table + frontmatter) and use it in place of the fallback sentence; add the >256-char suppression and route the style name through the `pze` escaping.
- **verifier** Oracle 238 @296737673 has both the gFn=256 suppression (gFn defined @285128585) and `${e.turnReminder??"Remember to follow the specific guidelines for this style."}`; built-ins really do override (@287919803 turnReminder:j3S Proactive, @287920185 turnReminder:q3S Concise, with the two literals resolved). 220 @238106296 had the ?? override but not the length gate. Port conversation.rs:11405-11412 h …[truncated; full text in the subsystem report]

#### `REM-12` — 2.1.238 entity-escapes `<\/system-reminder>` inside injected content; the port does not (envelope can be closed early by tool output)

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/prompt/task_notification.rs:288 — `prefix_non_user_provenance(&format!("<\system-reminder>\n{body}\n<\/system-reminder>"))`: header placed OUTSIDE the wrapper and no escaping; `rg -n 'lt;/system-reminder'` over lingxi-code returns 0 hits`
- **oracle** 2.1.238 @285068292: `function Xei(e){return e.replaceAll(/<\s*\/\s*system-reminder\s*>/gi,"&lt;/system-reminder&gt;")}` and `function b_a(e){…return`<\system-reminder>\n${nFn(Xei(e))}\n<\/system-reminder>`}`, applied to every task-notification-origin user message at API-build time (@296655062). `CC_VER=2.1.220 oracle.sh count '&lt;/system-reminder&gt;'` = 0 (new in 238).
- **fix** Add an `escape_reminder_close(s)` helper replacing `/<\s*/\s*system-reminder\s*>/gi` with `&lt;/system-reminder&gt;`, apply it to task-notification / queued-command bodies before wrapping, and move the provenance header inside the `<\system-reminder>` wrapper to match `b_a`.
- **verifier** Oracle 238 @285068292/@285068367 has Xei and b_a exactly as quoted, and the API-build switch @296655062 applies b_a to every origin.kind==='task-notification' user message; 220 count of &lt;/system-reminder&gt; = 0, so the escaping is new in 238. Port task_notification.rs:284-295 wraps without escaping and places the provenance header outside the wrapper, and rg 'lt;/system-reminder' = 0 hits. The …[truncated; full text in the subsystem report]

#### `REM-13` — 2.1.238 HTML-escapes filenames/paths interpolated into reminder bodies (Kae); the port interpolates raw, and the file-truncation notice copy also drifted

- **severity** P3 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — no equivalent sanitizer in lingxi-code (`rg -n 'lt;|&amp;' orchestrator/src/prompt/` finds only task_notification.rs:81's XML-attr escaping for <summary>/<result>), and neither truncation wording exists in the port`
- **oracle** 2.1.238 @285128585: `function pze(e){return ktp(e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;"))}` / `function Kae(e){return ktp(uLt(String(e??"")))}` (ktp entity-escapes C0/C1, U+2028/9). 238 wraps `e.filename` in `Kae(...)` across edited_text_file, compact_file_reference, audio_transcript, pdf_reference, selected_lines_in_ide/diff, opened_file_in_ide, memory_update and the file-truncation notice; 220 interpolated raw. Same notice also changed: 220 `… Don't tell the user about this truncation.` → 238 `Note: The file ${Kae(e.filename)} was too large and has been truncated to the first ${bBr} lines. No need to mention the truncation. Use ${mC.name} to read more of the file if you need.` (@139326232 / @296688988).
- **fix** Add `Kae`/`pze` equivalents in the reminder-rendering layer and route every interpolated path / style name through them; when the file-truncation notice is ported, use the 2.1.238 wording.
- **verifier** pze/ktp/uLt/Kae verified verbatim @285128585, and the truncation notice really did change (238 @139326145 'No need to mention the truncation.' vs 220 @117358120 "Don't tell the user about this truncation."). Port has no Kae/pze analogue in the reminder layer and neither truncation wording. The 'copy-drift' kind label is imprecise — the notice is absent rather than mis-worded, and most Kae call sit …[truncated; full text in the subsystem report]

#### `REM-14` — memory_update reminder is missing

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n "memory_update|pendingMemoryUpdates|pending_memory_updates" -i` over lingxi-code returns 0 hits`
- **oracle** 2.1.238 @139344881 / @296705761: `${F5T[e.source]} updated your memory directory: ${e.summary}` + `Files changed: ${i.map(Kae).join(", ")}` + `Your loaded copy of ${s.map(Kae).join(", ")} is now stale relative to disk — Read it again if you need current contents.` (`F5T={dream:"Background memory consolidation"}`); producer `jzm` drains `getAppState().pendingMemoryUpdates`. Present in 2.1.220 too.
- **fix** When a background memory consolidation writes the memory directory, queue a pending update and emit the quoted `<\system-reminder>` on the next turn, adding the staleness line only for paths already loaded into context.
- **verifier** Oracle 238 @139344881 carries ' updated your memory directory: ', 'Files changed: ' and the 'is now stale relative to disk' line; also present in 220 (count 2), i.e. a pre-existing gap as stated. Port: 0 hits for memory_update / pendingMemoryUpdates / the copy, while the dream consolidation that would queue it exists (tasks/src/handlers/dream.rs). Confirmed at P3.

#### `REM-15` — Several reminder messages are built with is_meta = false where the oracle sets isMeta:!0

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:11489, 11613, 11653, 11725, 11747, 11954, 12000, 12071, 12226, 12570 all use `ConversationMessage::user(...)` (is_meta:false, protocol/src/messages.rs:358) while 11373/11412/11791 correctly use `user_meta``
- **oracle** Every attachment renderer in 2.1.238 constructs `kn({content:…,isMeta:!0})` — e.g. `case"todo_reminder"` @296690005 `Zy([kn({content:o,isMeta:!0})])`, `M5T` @296685746, skill_listing/agent_listing/diagnostics entries in `Cqm` @296733172ff.
- **fix** Switch these reminder constructors to `ConversationMessage::user_meta` so the meta flag matches the oracle (it gates transcript visibility and, upstream, the non-meta turn counters used by the plan-mode and silent-turn gates).
- **verifier** Line-by-line check confirms the split: conversation.rs 11489/11613/11653/11725/11747/11954/12000/12071/12226/12570 use ConversationMessage::user (is_meta:false per protocol/src/messages.rs:355-365) while 11373/11412/11791 use user_meta; oracle sets isMeta:!0 on every attachment renderer. Impact is low because these messages are per-turn transients never persisted to history/JSONL (the port's own d …[truncated; full text in the subsystem report]

### Slash command registry

#### `SLASH-01` — /review was deleted upstream in 2.1.238 but the port still registers and advertises it

- **severity** P1 &nbsp;·&nbsp; **kind** *removed-upstream* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs:647 (`"review" => "Review a pull request",`) + "review" in BUILTIN_COMMAND_NAMES; lingxi-code/commands/core/src/register.rs:234 (`reg.register_builtin_handler(Arc::new(ReviewHandler::new()));`); lingxi-code/tui/src/command.rs (`/review`, hint `[pr number]`, advertised: true); lingxi-code/test-harness/src/parity/fixtures/parity_help_screen.txt:60`
- **oracle** 2.1.220 @ 237206060: `dDy={type:"prompt",name:"review",description:"Review a GitHub pull request; for your working diff use /code-review",argumentHint:"[pr number]",progressMessage:"reviewing pull request",contentLength:0,source:"builtin",async getPromptForCommand(e){…}}`. In 2.1.238: `grep -acoF 'name:"review"'` → 0 and `grep -acoF 'Review a GitHub pull request'` → 0. The PR-review surface moved into the bundled `code-review` skill.
- **fix** Remove `review` from BUILTIN_COMMAND_NAMES and core_description, drop ReviewHandler from register_core_batch_3, remove the /review entry from tui/src/command.rs, and re-lock parity_help_screen.txt (79 visible commands). If the PR-review capability is to be kept, move it to the bundled code-review skill surface rather than a slash command.
- **verifier** Verified at the oracle: `name:"review"` 220=1 / 238=0, `Review a GitHub pull request` 220=2 / 238=0, `reviewing pull request` 238=0, and the cmdrec3 name-diff lists `review` as gone in 238. Port still ships it on BOTH surfaces: names.rs BUILTIN_COMMAND_NAMES entry "review", core_description `"review" => "Review a pull request"`, register.rs:235 ReviewHandler, tui/src/command.rs:435 (advertised:tru …[truncated; full text in the subsystem report]

#### `SLASH-02` — /memory description changed in 2.1.238; port still ships the 2.1.220 string

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs:593 (`"memory" => "Open a memory file in your editor",`); lingxi-code/tui/src/command.rs:274; lingxi-code/commands/core/src/memory.rs:135 (test asserts the stale string); lingxi-code/test-harness/src/parity/fixtures/parity_help_screen.txt:44`
- **oracle** 2.1.238 @ 295075140: `cyT={type:"local-jsx",name:"memory",description:"Edit CLAUDE.md files and memory settings"}`. 2.1.220 @ 236130045: `fcy={type:"local-jsx",name:"memory",description:"Open a memory file in your editor"}`.
- **fix** Change all four sites to "Edit CLAUDE.md files and memory settings" (LingXi branding would make it "Edit LINGXI.md files and memory settings" if the memory-file noun is branded elsewhere; the structure of the sentence must change either way) and re-lock parity_help_screen.txt.
- **verifier** Oracle 238 `{type:"local-jsx",name:"memory",description:"Edit CLAUDE.md files and memory settings"}` vs 220 "Open a memory file in your editor" — confirmed as a 220→238 change in the record diff. Port carries the stale string at all four cited sites (names.rs:593, tui/src/command.rs:274, memory.rs:135, parity_help_screen.txt:44); because the TUI row is stale too, an interactive user sees the wrong …[truncated; full text in the subsystem report]

#### `SLASH-05` — /fork advertises the agent-view-DISABLED variant while the port registers the ENABLED one

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/commands/core/src/register.rs:386-392 correctly selects ForkBackgroundHandler when agent view is enabled, but the advertising surfaces are hardcoded to the other variant: lingxi-code/command-api/src/builtin_support/names.rs:670 (`"fork" => "Spawn a background agent that inherits the full conversation"`), lingxi-code/tui/src/command.rs:619 (same string, hint "<directive>"), fixture parity_help_screen.txt:29`
- **oracle** 2.1.238 @ 296246436, two /fork objects: `y$m={type:"local-jsx",name:"fork",description:"Spawn a background agent that inherits the full conversation",argumentHint:"<directive>",isEnabled:()=>!sv()}` and `b$m={type:"local-jsx",name:"fork",description:"Copy this conversation into a new background session and keep working here",argumentHint:"[prompt]",isEnabled:()=>!sv()}`. With agent view on (the default) the registered command is the second one.
- **fix** Make core_description("fork") and the TUI row track the same agent_view::is_enabled_with_setting() branch that register_core_batch_8 already uses: enabled → "Copy this conversation into a new background session and keep working here" with hint "[prompt]"; disabled → the current string with hint "<directive>". Re-lock the help fixture.
- **verifier** Both /fork objects re-read @296246436. The selector is `fleetFork:{open:()=>S3()&&!Un(V.IS_DEMO),whenOpen:[b$m,w$m],whenClosed:[y$m]}` inside the command table `ijT()`; `S3()=!AFr()` and `AFr()` is false unless CLAUDE_CODE_DISABLE_AGENT_VIEW/disableAgentView is set, so the DEFAULT registered /fork is b$m ("Copy this conversation into a new background session and keep working here", hint [prompt]). …[truncated; full text in the subsystem report]

#### `SLASH-06` — /bug is missing entirely; the port mismodels it as an alias of /feedback and invents a `share` stub

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs:463 (`("feedback", &["bug"]),`) and names.rs:145/:268 where "share" is listed as a builtin annotated `("share", "compiled stub in claude-code")`; `rg 'Report a bug or share your conversation'` over lingxi-code → 0 hits; "bug" is absent from BUILTIN_COMMAND_NAMES and from tui/src/command.rs`
- **oracle** 2.1.238 @ 294965131, two SEPARATE commands, neither carrying a `bug` alias: `bhT={type:"local-jsx",name:"feedback",description:"Send feedback to Anthropic or report a bug",argumentHint:"[report]",immediate:!0,requires:{ink:!0}}` and `ShT={aliases:["share"],type:"local-jsx",name:"bug",description:"Report a bug or share your conversation",argumentHint:"[report]",immediate:!0,requires:{ink:!0}}`. Byte-identical in 2.1.220 @ 236041664. `name:"share"` occurs 0 times in either binary.
- **fix** Add `bug` as a first-class builtin with description "Report a bug or share your conversation", argumentHint "[report]" and aliases ["share"]; remove the `("feedback", &["bug"])` alias and the phantom `share` stub entry; fix core_description("feedback") to "Send feedback to Anthropic or report a bug" and give it the `[report]` hint. Re-lock parity_help_screen.txt.
- **verifier** Oracle @294965337 re-read verbatim: `feedback` ("Send feedback to Anthropic or report a bug") and a SEPARATE `bug` command `{aliases:["share"],type:"local-jsx",name:"bug",description:"Report a bug or share your conversation",argumentHint:"[report]"}`; its binding `_Sl` is present in the builtin command table `ijT()`, so it really is registered and ungated. Port: names.rs:463 `("feedback", &["bug"] …[truncated; full text in the subsystem report]

#### `SLASH-08` — Six /help one-liners carry text that matches no oracle command object (2.1.220 and 2.1.238 identical)

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs:588 ("Manage hooks"), :635 ("Open or create your keybindings configuration file"), :655 ("List and manage background tasks"), :615 ("Claude in Chrome (Beta) settings"), :603 ("Configure the advisor model"), :661 ("Configure usage credits to keep working when you hit a limit"); rendered into parity_help_screen.txt lines 32, 40, 71, 10`
- **oracle** 2.1.238: `hooks` = "View hook configurations for tool events"; `keybindings` @ 296309682 region = "Open your keyboard shortcuts file"; `tasks` = "View and manage everything running in the background"; `chrome` = "Open Claude in Chrome settings"; `advisor` = "Let Claude consult a stronger model at key moments"; `usage-credits` = "Configure usage credits or request them from your admin when you hit a limit". All six byte-identical in 2.1.220 (cmdrec3 diff shows no change).
- **fix** Replace each with the verbatim oracle string (tui/src/command.rs already has the right text for hooks, keybindings and tasks — copy from there) and re-lock parity_help_screen.txt.
- **verifier** All six oracle strings re-read and byte-identical in 220 and 238 (absent from the record diff), so no twin/surface ambiguity applies — the port's strings match NO oracle object anywhere: names.rs:588 "Manage hooks" vs "View hook configurations for tool events"; :635 vs "Open your keyboard shortcuts file"; :655 vs "View and manage everything running in the background"; :615 vs "Open Claude in Chrom …[truncated; full text in the subsystem report]

#### `SLASH-03` — /auto-mode-setup description rewritten in 2.1.238; port keeps the 2.1.220 wording

- **severity** P2 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs:580; lingxi-code/commands/core/src/auto_mode_setup.rs:380 (doc comment at :7); lingxi-code/test-harness/src/parity/fixtures/parity_slash_commands_102.json:56`
- **oracle** 2.1.238 @ 294963678, both twins: `mSl={type:"local",name:"auto-mode-setup",supportsNonInteractive:!0,description:"Teach auto mode about your environment, plus optional rule tweaks",argumentHint:"[--request-id <uuid>] (--wizard posture=… scope=… depth=… --propose | --expect-sha256 <64-hex> --apply-file <path>)",…}` and `hhT={type:"local-jsx",name:"auto-mode-setup",description:"Teach auto mode about your environment, plus optional rule tweaks",…}`. `oracle.sh count 'Teach auto mode about your environment, plus optional rule tweaks'` → 2. 2.1.220 had "Set up and customise auto mode — environment context, plus optional rule tweaks".
- **fix** Replace the string at all three sites with "Teach auto mode about your environment, plus optional rule tweaks" (no em dash any more) and update the fixture.
- **verifier** Oracle bytes re-read @294963678: both `auto-mode-setup` twins now say "Teach auto mode about your environment, plus optional rule tweaks"; 220 said "Set up and customise auto mode — environment context…". Port stale at auto_mode_setup.rs:7/:380, names.rs:580, parity_slash_commands_102.json:56. Severity corrected: `auto-mode-setup` is in HIDDEN_PALETTE_COMMANDS, has no TUI row and is absent from pa …[truncated; full text in the subsystem report]

#### `SLASH-04` — NEW in 2.1.238: /goal auto-clears its Stop hook on an unrecoverable turn error; port has no such path

- **severity** P2 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg 'Goal cleared after|cleared_auth|cleared_billing|cleared_context_limit|cleared_model_unavailable|quartz_pipit'` over lingxi-code returns 0 hits; lingxi-code/commands/core/src/goal.rs implements only set/status/clear/too-long (CLEAR_TOKENS, NO_GOAL_SET, too_long_message, directive_for) with no fatal-error teardown`
- **oracle** 2.1.238 @ 292182815: `function*Cqf(e,t,r,n){try{if(!it("tengu_quartz_pipit",!0)||!e||t.agentId||t.abortController.signal.aborted||PH(r)!=="main")return; let o=w4v(n);if(o===null)return;let{label:i,errorCode:s}=v4v[o]; t.sessionHooksRegistry.remove(zt(),"Stop",{type:"prompt",prompt:e.condition}), cFe(e,o==="context_limit"?"context_limit":"api_error"),de("goal_met",s), yield{type:"active_goal",value:void 0},yield bOi(!0,e.condition), yield jBt(`Goal cleared after an unrecoverable error (${i}): \"${Yl(e.condition,T4v,!0)}\". Run /goal again to continue.`,"warning")}catch(o){Ce(o)}}`. Label map @ 292183854: `v4v={auth:{label:"authentication failed",errorCode:"cleared_auth"},billing:{label:"credit balance too low",errorCode:"cleared_billing"},context_limit:{label:"context limit reached",errorCode:"cleared_context_limit"},model_unavailable:{label:"model unavailable",errorCode:"cleared_model_unavailable"}}` with `T4v=80`. `CC_VER=2.1.220 oracle.sh count '. Run /goal again to continue.'` → 0, so this is new in this window. Statsig default is true (`it("tengu_quartz_pipit",!0)`).
- **fix** On main-agent (non-subagent, non-aborted) turn termination, map the terminal reason through the oracle's w4v buckets (blocking_limit|prompt_too_long|rapid_refill_breaker → context_limit; non-transient api_error with errorKind authentication_failed|oauth_org_not_allowed (when not remote) or account_on_hold → auth; billing_error → billing; model_not_found → model_unavailable; everything else → no clear), remove the session-scoped Stop prompt hook, clear the active goal, and emit a system informational message at level "warning" reading exactly `Goal cleared after an unrecoverable error ({label}): "{condition truncated to 80}". Run /goal again to continue.`
- **verifier** `Cqf` read verbatim @292182815 and matches the quoted body exactly, including `w4v` reason mapping, `v4v={auth|billing|context_limit|model_unavailable}` labels and `it("tengu_quartz_pipit",!0)` default-true. The literal is 0 hits in 2.1.220 → genuinely new drift. Port absence confirmed by behaviour grep (not symbol grep): `Goal cleared after` = 0 hits; goal.rs implements only set/status/clear/too_ …[truncated; full text in the subsystem report]

#### `SLASH-09` — /subtask has a real handler but is not in BUILTIN_COMMAND_NAMES, so it never appears in /help or the palette

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/commands/core/src/subtask.rs exists and lingxi-code/commands/core/src/register.rs:389 registers SubtaskHandler when agent view is on, but `grep -c '^subtask$'` over the extracted BUILTIN_COMMAND_NAMES list → 0, and /subtask is absent from lingxi-code/tui/src/command.rs. lingxi-code/command-api/src/builtin_support/help_render.rs:39-44 iterates BUILTIN_COMMAND_NAMES only.`
- **oracle** 2.1.238 @ 296247354: `w$m={type:"local-jsx",name:"subtask",description:"Send a subagent off with your full context; its result comes back here",argumentHint:"<task>",isEnabled:()=>!sv(),load:…}` — `sv()` is "is a coordinator session", so /subtask is visible by default. Present identically in 2.1.220.
- **fix** Add "subtask" to BUILTIN_COMMAND_NAMES (bumping the locked total) with core_description "Send a subagent off with your full context; its result comes back here", gate its palette visibility on the same agent-view branch that register_core_batch_8 uses, add a /subtask row with hint "<task>" to tui/src/command.rs, and re-lock the fixtures.
- **verifier** Oracle `w$m={type:"local-jsx",name:"subtask",description:"Send a subagent off with your full context; its result comes back here",argumentHint:"<task>",isEnabled:()=>!sv()}` @296247354, registered through `...yio("fleetFork")` whenOpen — and agent view (S3()) is on by default, so /subtask is visible by default. Port: SubtaskHandler exists and registers (register.rs:389, tests at :1117-1210) but `s …[truncated; full text in the subsystem report]

#### `SLASH-10` — /rewind is missing the `undo` alias in the headless registry

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs:468 — `("rewind", &["checkpoint"]),` (tui/src/command.rs correctly carries `"/checkpoint", "/undo"`)`
- **oracle** 2.1.238 @ 296261668 (byte-identical to 2.1.220 @ 237294063): `S$T={description:"Restore the code and/or conversation to a previous point",name:"rewind",aliases:["checkpoint","undo"],argumentHint:"",type:"local",supportsNonInteractive:!1,…}`
- **fix** Change names.rs:468 to `("rewind", &["checkpoint", "undo"]),` so /undo resolves on the headless/bridge dispatcher as it does in the TUI.
- **verifier** Oracle `rewind` carries `aliases:["checkpoint","undo"]` in both 220 and 238 (unchanged in the record diff). Port names.rs:468 is `("rewind", &["checkpoint"])` while tui/src/command.rs:197-198 correctly lists `&["/checkpoint", "/undo"]`, so /undo resolves in the TUI but not on the headless/bridge dispatcher.

#### `SLASH-11` — TUI registry collapses /focus into /tui with the wrong description and no argument hint

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tui/src/command.rs — single `/tui` entry with `aliases: &["/focus"]`, `description: "Toggle full-screen mode"`, `hint: ""`; /focus has no row of its own (command-api/src/builtin_support/names.rs does carry both names with the correct strings)`
- **oracle** 2.1.238 has two distinct ungated commands: `{type:"local-jsx",name:"tui",description:"Set the terminal UI renderer (default | fullscreen)",argumentHint:"[default|fullscreen]"}` and `{type:"local-jsx",name:"focus",description:"Toggle focus view: just your prompt, summary, and response"}`. Neither carries the other as an alias. Identical in 2.1.220.
- **fix** Split them in tui/src/command.rs: /tui with description "Set the terminal UI renderer (default | fullscreen)" and hint "[default|fullscreen]", and a separate /focus row with "Toggle focus view: just your prompt, summary, and response"; drop the /focus alias from /tui.
- **verifier** Oracle 238 ships two independent ungated local-jsx objects, `tui` ("Set the terminal UI renderer (default | fullscreen)", argumentHint "[default|fullscreen]") and `focus` ("Toggle focus view: just your prompt, summary, and response"), neither aliasing the other. Port tui/src/command.rs:381-390 collapses them into one `/tui` row with `aliases:&["/focus"]`, `description:"Toggle full-screen mode"`, ` …[truncated; full text in the subsystem report]

#### `SLASH-15` — terminalOriented:!0 added to /color, /exit, /reload-plugins and /statusline in 2.1.238

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — lingxi-code/command-api/src/builtin_support/names.rs has no terminal-oriented field or table; `rg 'terminal_oriented|terminalOriented' lingxi-code` → 0 hits`
- **oracle** 2.1.238 @ 296279264: `B$T={type:"local-jsx",name:"exit",aliases:["quit"],get description(){return KBm()},immediate:!0,requires:{ink:!0},terminalOriented:!0,fleetHostCall:async({exit:e})=>e()}` and `YBm={type:"local",name:"exit",terminalOriented:!0,supportsNonInteractive:!0,…}`; also present on color, reload-plugins and statusline (2.1.238 @ 296309682 for statusline). Zero commands carry `terminalOriented:!0` in 2.1.220 (flag scan over cmdrec3-220.json returned an empty set).
- **fix** Add a `TERMINAL_ORIENTED_COMMANDS` table (color, exit, reload-plugins, statusline) alongside COMMAND_ALIASES so thin/bridge clients can route or suppress these four the way the oracle does; no user-visible copy changes.
- **verifier** Confirmed new in 238: the flag is on color, exit, reload-plugins and statusline objects in 238 and on zero objects in 220. Not merely an internal routing hint — the init emitter at 298686200 builds `n=e.commands.filter(i=>i.userInvocable!==!1&&i.terminalOriented===!0).map(i=>i.name)` and emits `...n.length>0&&{terminal_slash_commands:n}` into the stream-json `system/init` payload (`terminal_slash_ …[truncated; full text in the subsystem report]

#### `SLASH-13` — Upstream-visible commands /bug, /powerup, /daemon and /scroll-speed have no registry rows in the port

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — none of powerup, daemon, scroll-speed, bug appear in lingxi-code/command-api/src/builtin_support/names.rs BUILTIN_COMMAND_NAMES (verified against the extracted 107-name list) or in lingxi-code/tui/src/command.rs`
- **oracle** 2.1.238: `{type:"local-jsx",name:"powerup",description:"Discover Claude Code features through quick interactive lessons",requires:{ink:!0}}` (ungated); `{type:"local-jsx",name:"daemon",description:"Manage background services and routines",immediate:!0,requires:{ink:!0}}` (ungated); `{…name:"scroll-speed",description:"Adjust mouse wheel scroll speed",isEnabled:()=>{if(!Ws())return!1;…}}` (terminal-conditional); /bug per SLASH-06. By contrast pause-memory, loops and wellbeing all carry `isEnabled:()=>!1`, radio is statsig tengu_velvet_static default false and update is `isEnabled:()=>!1,isHidden:!0`, so their absence from the port correctly matches the default surface.
- **fix** Add rows for the ungated ones the port can support (/bug is the important one — see SLASH-06); for /powerup, /daemon and /scroll-speed either implement or register them as explicit hidden stubs with the verbatim oracle descriptions so the name surface is complete and the divergence is documented rather than silent.
- **verifier** Mostly confirmed but the oracle claim is partly false. Confirmed: `Rkl` (powerup, ungated local-jsx) and `IFm` (scroll-speed, terminal-conditional) and `_Sl` (bug) are all members of the builtin command table `ijT()` and all four names are absent from the port's 107-entry BUILTIN_COMMAND_NAMES and from tui/src/command.rs. REFUTED WITHIN: /daemon is NOT ungated — it enters only via `daemon:{open:() …[truncated; full text in the subsystem report]

#### `SLASH-14` — /version is advertised in /help with a made-up description although both oracle objects are isEnabled:()=>!1

- **severity** P3 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/command-api/src/builtin_support/names.rs (`"version" => "Print version information"`) and lingxi-code/test-harness/src/parity/fixtures/parity_help_screen.txt:78 — `  /version              Print version information``
- **oracle** 2.1.238 @ 296268759: `C$T={type:"local-jsx",name:"version",description:"Show this session's version (autoupdate may have a newer one)",isEnabled:()=>!1,immediate:!0,requires:{ink:!0}}` and `gAl={type:"local",name:"version",description:"Print the version this session is running (not what autoupdate downloaded)",isEnabled:()=>!1,get isHidden(){return!Dn()},supportsNonInteractive:!0,…}`. Both disabled; identical in 2.1.220 @ 237301143.
- **fix** Either drop /version from the visible palette (add it to is_palette_hidden, matching the upstream `isEnabled:()=>!1`) or, if LingXi intends to keep it enabled, use the oracle's non-interactive string "Print the version this session is running (not what autoupdate downloaded)" instead of the invented "Print version information".
- **verifier** Oracle @296268759 re-read verbatim: `C$T` (local-jsx, "Show this session's version (autoupdate may have a newer one)") and `gAl` (local, "Print the version this session is running (not what autoupdate downloaded)") BOTH carry `isEnabled:()=>!1`, identical in 220. Port names.rs advertises `"version" => "Print version information"` and parity_help_screen.txt:78 renders it; that string matches neithe …[truncated; full text in the subsystem report]

### CLI argv surface

#### `CLI-02` — `mcp get` / `mcp list` help text gained ' unless disabled for this project' in 2.1.238

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/mcp.rs:126-133 — both doc comments end `…approved servers are\n    /// health-checked.``
- **oracle** 2.1.238 @298368697 (list) and @298369123 (get): `r.command("list").description("List configured MCP servers. Unapproved .mcp.json servers are shown as ⏸ Pending approval and not connected to; approved servers are health-checked unless disabled for this project.")`. The 2.1.220 form `approved servers are health-checked.` has 1 hit in 2.1.220 and 0 hits in 2.1.238; the new form has 0 hits in 2.1.220 and 2 hits in 2.1.238.
- **fix** In apps/cli/src/commands/mcp.rs, change both the `Get(GetArgs)` and `List` doc comments to end `…approved servers are health-checked unless disabled for this project.`
- **verifier** Confirmed. Two distinct 2.1.238 offsets carry the new sentence — @298368718 `r.command("list").description("List configured MCP servers. ... approved servers are health-checked unless disabled for this project.")` and @298369144 the identical tail on `r.command("get <name>")`. Counts: new form 0 in 2.1.220 / present twice in 2.1.238; old form `approved servers are health-checked.` present in 2.1.2 …[truncated; full text in the subsystem report]

#### `CLI-03` — `plugin validate` description changed in 2.1.238 (now covers directories of skills/agents/commands)

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin.rs:79 — `/// Validate a plugin or marketplace manifest``
- **oracle** 2.1.238 @298409179 (table `Qj`, cc-238.js @234903950): `validate:{usage:"plugin validate <path>",description:"Validate a plugin or marketplace manifest, or the skills, agents, and commands in a directory"}`. 2.1.220 @124171952: `Validate a plugin or marketplace manifest`; the longer 238 form has 0 hits in 2.1.220.
- **fix** Update the doc comment to `Validate a plugin or marketplace manifest, or the skills, agents, and commands in a directory`, and confirm the validate handler actually accepts a bare directory of skills/agents/commands (the new copy advertises that capability).
- **verifier** Confirmed. cc-238.js @234903950 (binary @298409179): `validate:{usage:"plugin validate <path>",description:"Validate a plugin or marketplace manifest, or the skills, agents, and commands in a directory"}`. Long form = 2 hits in 2.1.238, 0 in 2.1.220; 2.1.220 @124171952 shows the bare short form. Port plugin.rs:79 `/// Validate a plugin or marketplace manifest`. The fix's second clause (verify the …[truncated; full text in the subsystem report]

#### `CLI-06` — Five `plugin eval` option help strings were rewritten in 2.1.238; the port still ships the 2.1.220 text

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin_eval.rs:43-46 (ablation), :89-91 (output-dir), :94 (publish-report), :98 (report), :118 (verbose) — all still the 2.1.220 sentences`
- **oracle** 2.1.238: `--ablation <mode>` @298657037 `Run a no-plugin baseline arm and report the score delta (none | with-without; default: with-without whenever a plugin resolves — by name, or from the target path — and none when nothing does; under with-without, graders marked with-only, incl. `tool_used: Skill`, are a plugin-fired indicator rather than part of the score)`; `--output-dir <dir>` @298656058 `Directory for aggregate-result.json (default: ./<eval dir>/results/<timestamp>/)`; `--publish-report` @298657698 `Also require publishing the report to claude.ai (already the default when your account supports it); explains why if unavailable`; `--report <path>` @298657560 `Write the self-contained HTML report (scores, prompts, grader verdicts) to <path> instead of the results dir`; `--verbose` @298657454 `Log per-message trace events to the debug log (use --debug-file to read them)`. Each 2.1.220 counterpart is in cc-bin-220 at 239390242 / 239389557 / 239390667 / 239390559 / 239390509 respectively.
- **fix** Replace the five doc comments with the 2.1.238 strings quoted above. Note --publish-report also changed semantics (publishing is now the default when the account supports it; the flag only *requires* it), so revisit the hard error at plugin_eval.rs:544.
- **verifier** Confirmed all five by reading the full 2.1.238 option chain at binary @298656000 against the full 2.1.220 chain at @239389557 and against plugin_eval.rs. --ablation (238 `...whenever a plugin resolves — by name, or from the target path — and none when nothing does; under with-without, graders marked with-only, incl. `tool_used: Skill`...` vs port :43-46 `...when targeting a plugin by name, none fo …[truncated; full text in the subsystem report]

#### `CLI-01` — `--autocompact <auto|tokens>` is a new visible root flag in 2.1.238; the port rejects it

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/argv.rs declares no `autocompact` option (the complete long-flag inventory from `grep -o 'long = "[a-zA-Z0-9-]*"' src/argv.rs` contains no `autocompact`). The only autocompact identifiers in the workspace are the compaction subsystem: lingxi-code/compaction/src/autocompact.rs and compaction/src/orchestrator.rs:99 `autocompact_threshold` — no CLI binding. `lingxi-cli --autocompact 500k` fails with clap 'unexpected argument'.`
- **oracle** 2.1.238 @307413819 (spec) / @307413849 (desc): `t.addOption(new bp("--autocompact <auto|tokens>","Auto-compact window size (auto, or 100k–1M tokens)").argParser((l)=>{let c=DUn(l);if(c===void 0)throw new j3t("It must be 'auto', or between 100k and 1M (e.g. 500k, 200000, or 200 as shorthand)");return c}))` — no .hideHelp(), so it renders in `claude --help`. Parser DUn (cc-238.js @222905882): `auto` sentinel; `m`→*1e6, `k`→*1000, bare 100..1000→*1000; out-of-window returns undefined. Version proof: literal `--autocompact <auto|tokens>` has 0 hits in 2.1.220 and 2 hits in 2.1.238.
- **fix** Add `--autocompact <auto|tokens>` to `Argv` with a value_parser mirroring DUn (trim+lowercase; `auto` → sentinel; `m` suffix ×1e6; `k` suffix ×1000; bare integer in 100..=1000 treated as thousands; reject anything outside the 100k–1M window with the exact copy `It must be 'auto', or between 100k and 1M (e.g. 500k, 200000, or 200 as shorthand)`), help text `Auto-compact window size (auto, or 100k–1M tokens)`, and thread the resolved value into CompactionOrchestrator::autocompact_threshold so it overrides the autoCompactWindow setting.
- **verifier** Confirmed both sides. cc-238.js @243908809: `t.addOption(new bp("--autocompact <auto|tokens>","Auto-compact window size (auto, or 100k–1M tokens)").argParser(...))` — and unlike its neighbours `--advisor` / `--enable-auto-mode` it carries NO .hideHelp(), so it renders in `claude --help`. Literal `--autocompact <auto|tokens>`: 2 hits in 2.1.238, 0 in 2.1.220 → genuine new drift. Port: `grep -rn aut …[truncated; full text in the subsystem report]

#### `CLI-04` — `plugin eval` description drift plus two flags added in 2.1.238 (`--eval-dir`, `--no-publish`)

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin_eval.rs:33-124 (struct Cli: ablation, allow-tools, case, json, judge-model, keep-temp, max-cost-usd, model, no-scaffold, output-dir, publish-report, report, runs, scaffold, tag, threshold, verbose — no eval-dir, no no-publish; `rg -e 'no-publish' -e 'eval_dir' apps/cli/src/commands/plugin_eval.rs` → 0 hits) and apps/cli/src/commands/plugin.rs:44 (long_about still the 2.1.220 sentence)`
- **oracle** Description 2.1.238 @298409561: `Run eval cases (<eval dir>/**/case.yaml or prompt.md + graders/*.md; the eval dir is evals/ unless --eval-dir or the manifest says otherwise) against a plugin and report scored results. Target is a path, a plugin name, or a `plugin@marketplace` id — installed and skills-dir plugins both resolve (and add a no-plugin baseline arm)`; the 2.1.220 form `Run eval cases (evals/**/case.yaml or evals/**/prompt.md + graders/*.md) against a plugin and report scored results. ` has 0 hits in 2.1.238. New flags: `--eval-dir <dir>` @298656168 `Directory name (below the plugin) that holds the eval cases; results go to <plugin>/<dir>/results/ — for an installed-plugin target, ./<dir>/results/ with this flag, else ./evals/results/ (default dir: the manifest's experimental.evals value, else evals/)` (0 hits in 2.1.220); `--no-publish` @298657852 `Keep the HTML report local only; skip publishing it to claude.ai` (0 hits in 2.1.220).
- **fix** Add `--eval-dir <dir>` (resolving the eval directory from the flag, else the manifest's experimental.evals, else evals/, and rooting results at <plugin>/<dir>/results/) and `--no-publish` to plugin_eval::Cli with the 2.1.238 help strings, and replace the `plugin eval` description in plugin.rs:44 and plugin_eval.rs:33 with the 2.1.238 wording.
- **verifier** Confirmed. Description at @298409561 matches the quote verbatim. `--eval-dir <dir>` = 4 hits in 2.1.238 / 0 in 2.1.220; `Keep the HTML report local only; skip publishing it to claude.ai` = 2 / 0. Re-read the option chain at binary @298656000 — both `.option("--eval-dir <dir>",...)` and `.option("--no-publish",...)` are registered on `plugin eval`. Port plugin_eval.rs:33-124 struct Cli has neither …[truncated; full text in the subsystem report]

#### `CLI-05` — `plugin eval init` description drift plus two flags added in 2.1.238 (`-i/--interactive`, `--eval-dir`)

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin_eval.rs:127-145 — `EvalSub::Init` doc comment is the 2.1.220 text and `InitArgs` declares only `--bare` and the `[name]` positional`
- **oracle** 2.1.238 @298409982: `evalInit:{usage:"plugin eval init [name]",description:"Author an eval suite under the eval dir (evals/ unless --eval-dir or the manifest says otherwise) via an interview that sources inputs and designs graders. Use --bare <name> for a blank single-case template.",earlyAccess:"pluginEval"}`; the 2.1.220 form `Author an eval suite under evals/ via an interview…` has 0 hits in 2.1.238. New flags: `-i, --interactive` @298658574 `Run the authoring interview (already the default in a terminal); requires an interactive terminal`; `--eval-dir <dir>` (cc-238.js @235153733) `Directory (below the current directory) to write cases into (default: experimental.evals from the plugin.json in the current directory, else evals/)`.
- **fix** Update the EvalSub::Init doc comment to the 2.1.238 wording and add `-i, --interactive` and `--eval-dir <dir>` to InitArgs with the quoted help strings.
- **verifier** Confirmed. Qj.evalInit @298409982 matches the quote; the 2.1.220 form `Author an eval suite under evals/ via an interview` is present in 220 and 0 in 238. Registration cc-238.js @~235153500 shows `.option("-i, --interactive","Run the authoring interview (already the default in a terminal); requires an interactive terminal")` and `.option("--eval-dir <dir>","Directory (below the current directory) …[truncated; full text in the subsystem report]

#### `CLI-07` — `plugin install -y/--yes` is new in 2.1.238; the port hard-errors on it

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin.rs:149-163 — `pub struct InstallArgs { config, scope, plugin }`, no `yes` field`
- **oracle** cc-238.js @235156585 (binary @298664358): `n.command("install <plugin>")…​.option("-y, --yes","Accept the displayed marketplace-declared command without the confirmation prompt — a plugin installed by running a command, or one whose archive is fetched through a headersHelper command (required when stdin or stdout is not a TTY)")`. In 2.1.220 the `plugin install` registration (cc-bin-220 @239394095 / @239394178) carries only `-s, --scope <scope>` and `--config <key=value>`.
- **fix** Add `#[arg(short = 'y', long)] pub yes: bool` to InstallArgs with the 2.1.238 help string, and use it to bypass the marketplace-declared-command confirmation (required when stdin or stdout is not a TTY).
- **verifier** Confirmed. cc-238.js @235156585 re-read: the `install <plugin>` chain ends `.option("-y, --yes","Accept the displayed marketplace-declared command without the confirmation prompt — a plugin installed by running a command, or one whose archive is fetched through a headersHelper command (required when stdin or stdout is not a TTY)")`. `Accept the displayed marketplace-declared command` = 0 hits in 2 …[truncated; full text in the subsystem report]

#### `CLI-08` — `plugin update -y/--yes` is new in 2.1.238; the port hard-errors on it

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin.rs:251-259 — `pub struct UpdateArgs { scope, plugin }``
- **oracle** cc-238.js @235159327 (binary @298661616): `n.command("update <plugin>").description(Qj.update.description).option("-s, --scope <scope>",`Installation scope: ${oSr.join(", ")} (default: user)`).option("-y, --yes","Accept the displayed marketplace-declared command without the confirmation prompt — a changed install command, or the headersHelper command that fetches its archive (required when stdin or stdout is not a TTY)")`. Absent from 2.1.220's plugin update registration.
- **fix** Add `#[arg(short = 'y', long)] pub yes: bool` to UpdateArgs with the 2.1.238 help string and wire it to skip the changed-install-command / headersHelper confirmation.
- **verifier** Confirmed. cc-238.js @235159327: `n.command("update <plugin>").description(Qj.update.description).option("-s, --scope <scope>",...).option("-y, --yes","Accept the displayed marketplace-declared command without the confirmation prompt — a changed install command, or the headersHelper command that fetches its archive (required when stdin or stdout is not a TTY)")`. 0 hits for that copy in 2.1.220. P …[truncated; full text in the subsystem report]

#### `CLI-13` — Truncating-resume flags missing: `--resume-session-at` never ported, and 2.1.238 added `--resume-drops-turn` and reworded `--resume-session-at`

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -e 'resume-session-at' -e 'resume_session_at' -e 'resume-drops-turn' --glob '*.rs'` across lingxi-code returns 0 hits; apps/cli/src/argv.rs has neither option`
- **oracle** `--resume-session-at <message id>` @307408593 — 2.1.220 desc: `When resuming, only messages up to and including the assistant message with <message.id> (use with --resume in print mode)`; 2.1.238 desc: `When resuming, only messages up to and including the chain entry with <message.id> — any chain-entry UUID, typically the kept turn's last entry (use with --resume in print mode)`. `--resume-drops-turn <message id>` @307408861 — NEW (0 hits in 2.1.220, 10 in 2.1.238): `With --resume-session-at in print mode: declare the prompt uuid of the turn the truncating resume intends to discard; the resume is refused if the discarded range contains anything not attributable to that turn (absorbed queued messages, task notifications, content from other turns). Ignored outside print mode, like --resume-session-at.` Both are consumed by runHeadless (`…resumeSessionAt:t.resumeSessionAt||void 0,resumeDropsTurn:t.resumeDropsTurn,…`).
- **fix** Add hidden `--resume-session-at <message id>` and `--resume-drops-turn <message id>` to Argv (both String, print-mode only), and implement the truncating resume in the headless path: load the transcript only up to and including the chain entry with that UUID, and refuse the resume when the discarded range contains anything not attributable to the declared dropped turn.
- **verifier** Confirmed. Binary @307408593 (2.1.238) reads `--resume-session-at <message id>` → `...only messages up to and including the chain entry with <message.id> — any chain-entry UUID, typically the kept turn's last entry...`, immediately followed by the new `--resume-drops-turn <message id>` with the quoted body; 2.1.220 @150368496 shows the older `...the assistant message with <message.id>...` and jump …[truncated; full text in the subsystem report]

#### `CLI-14` — Top-level `import` and `import-conversations` subcommands absent from the port (and `import --yes` changed to `--yes=<digest>` in 2.1.238)

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -e 'import-conversations' -e '"import"' --glob '*.rs'` across lingxi-code finds only lingxi-code/tui-core/src/terminal_setup.rs:452 (an unrelated `.arg("import")`); apps/cli/src/commands/mod.rs `enum Commands` has no Import variant`
- **oracle** cc-238.js @~245142681 (strings @308647783): `r.command("import").argument("[source]","Which agent to import from (codex, gemini)").option("--dry-run","Show what would be imported without writing anything").option("--yes","Skip the interactive picker. On headless surfaces, pass --yes=<digest> from the `/import` preview.").description("Import config from another AI coding agent into Claude Code")` with usage `Usage: claude import [codex|gemini] [--dry-run] [--yes[=<digest>]]`; and `r.command("import-conversations <exportPath>",{hidden:!0}).option("--cwd <dir>","Archive directory the imported sessions anchor to").option("--dry-run","Parse and verify manifest without writing files")`. 2.1.220 @152320496 had `Usage: claude import [codex|gemini] [--dry-run] [--yes]` and `--yes` → `Import everything without the interactive picker (headless surfaces)`.
- **fix** Add `import [source]` (with `--dry-run` and `--yes[=<digest>]`) and the hidden `import-conversations <exportPath>` (with `--cwd <dir>`, `--dry-run`) to Commands, matching the 2.1.238 help strings and the digest-bound `--yes` contract.
- **verifier** Confirmed. cc-238.js @245142681 re-read verbatim — `r.command("import").argument("[source]",...).option("--dry-run",...).option("--yes","Skip the interactive picker. On headless surfaces, pass --yes=<digest> from the `/import` preview.")` with usage `Usage: claude import [codex|gemini] [--dry-run] [--yes[=<digest>]]`, plus `r.command("import-conversations <exportPath>",{hidden:!0})`. 2.1.220 @1522 …[truncated; full text in the subsystem report]

#### `CLI-15` — `--append-subagent-system-prompt <prompt>` never ported — operators cannot inject a subagent prompt suffix

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -e 'append-subagent-system-prompt' -e 'append_subagent' --glob '*.rs'` across lingxi-code returns 0 hits; apps/cli/src/argv.rs has `--append-system-prompt` and `--append-system-prompt-file` but no subagent variant`
- **oracle** 2.1.238 @307405786 (also present in 2.1.220, so not new drift): `new bp("--append-subagent-system-prompt <prompt>","Append a system prompt to every Task-tool subagent's system prompt, propagated to nested subagents (only works with --print). Implies CLAUDE_CODE_ENABLE_APPEND_SUBAGENT_PROMPT=1.").argParser(String).hideHelp()`, consumed in the headless launch as `appendSubagentSystemPrompt:t.appendSubagentSystemPrompt`.
- **fix** Add the hidden `--append-subagent-system-prompt <prompt>` option to Argv and append its value to every Task-tool subagent's system prompt (propagating to nested subagents) in the print/headless path, gated the same way the oracle gates it via CLAUDE_CODE_ENABLE_APPEND_SUBAGENT_PROMPT.
- **verifier** Confirmed. Binary @307405786 / mainopts-238.txt: `new bp("--append-subagent-system-prompt <prompt>","Append a system prompt to every Task-tool subagent's system prompt, propagated to nested subagents (only works with --print). Implies CLAUDE_CODE_ENABLE_APPEND_SUBAGENT_PROMPT=1.").argParser(String).hideHelp()` — sitting directly between `--append-system-prompt-file` and `--plan-mode-instructions`, …[truncated; full text in the subsystem report]

#### `CLI-16` — `--rewind-files <user-message-id>` never wired although the port already implements file rewind

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent at the argv seam — apps/cli/src/argv.rs declares no `rewind-files` option and `rg 'rewind' apps/cli/src` returns 0 hits, even though lingxi-code/session/src/file_history.rs:289 implements `FileHistory::rewind_files(message_id)` and lingxi-code/session/src/rewind.rs implements the transcript rewind`
- **oracle** 2.1.238 @307409495 (present in 2.1.220 too): `new bp("--rewind-files <user-message-id>","Restore files to state at the specified user message and exit (requires --resume)").hideHelp()`
- **fix** Add the hidden `--rewind-files <user-message-id>` option to Argv, require `--resume`, and route it straight to `FileHistory::rewind_files` followed by process exit — the machinery already exists, only the CLI entry point is missing.
- **verifier** Confirmed. Binary @307409495: `new bp("--rewind-files <user-message-id>","Restore files to state at the specified user message and exit (requires --resume)").hideHelp()`; present in 2.1.220 too. Port: no `rewind` under apps/cli/src except apps/cli/src/init.rs:87 (a doc-comment mention), while session/src/file_history.rs:289 `pub async fn rewind_files(&self, message_id: Uuid) -> Result<Vec<String>, …[truncated; full text in the subsystem report]

#### `CLI-09` — `plugin uninstall` gained the `rm` alias in 2.1.238

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/plugin.rs:73 — `#[command(name = "uninstall", visible_alias = "remove")]``
- **oracle** 2.1.238 @298411033: `uninstall:{usage:"plugin uninstall <plugin>",aliases:["remove","rm"],description:"Uninstall an installed plugin"}`. 2.1.220 @124178720 shows the uninstall entry with only `remove` in the alias slot.
- **fix** Add a second alias: `#[command(name = "uninstall", visible_alias = "remove", visible_alias = "rm")]` (or `visible_aliases = ["remove", "rm"]`).
- **verifier** Confirmed, and I checked the alias is actually WIRED rather than merely sitting in a table: cc-238.js @234905978 `uninstall:{usage:"plugin uninstall <plugin>",aliases:["remove","rm"],...}` and @235157072 `n.command("uninstall <plugin>").aliases(Qj.uninstall.aliases)`. 2.1.220 @124178720 shows only `remove` in the alias slot. Port plugin.rs:73 `#[command(name = "uninstall", visible_alias = "remove" …[truncated; full text in the subsystem report]

#### `CLI-10` — New hidden top-level command family `claude sandbox install|status` (Windows sandbox)

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/mod.rs `enum Commands` (Mcp, Auth, AutoMode, AutoModeSetup, Doctor, Gateway, Install, Plugin, Project, RemoteControl, SetupToken, Agents, Attach, Rm, Ultrareview, Update, Daemon, BgRun, BgPtySession) has no Sandbox variant`
- **oracle** cc-238.js @~245141000: `let c=r.command("sandbox",{hidden:!0}); c.command("install").description('Install the Windows sandbox user and network filters. Self-elevates (one UAC prompt). Prints a JSON {status, message} result and exits 0 only when status is "ok".') … c.command("status").description("Print Windows sandbox availability and install state as JSON {available, installed, policyLocked, reasons}.")` (help strings @308646876). A diff of every `.command("name")` literal between the two binaries yields exactly two new names: `sandbox` and its child `install`.
- **fix** Windows-only feature with no counterpart in lingxi-code/sandbox-runtime. If Windows support is in scope, add a hidden `sandbox` group with `install` and `status` children emitting the same JSON shapes; otherwise record as an accepted divergence so future audits stop re-finding it.
- **verifier** Confirmed. cc-238.js @245141200: `let c=r.command("sandbox",{hidden:!0}); return c.command("install").description('Install the Windows sandbox user and network filters. Self-elevates (one UAC prompt). Prints a JSON {status, message} result and exits 0 only when status is "ok".')..., c.command("status").description("Print Windows sandbox availability and install state as JSON {available, installed, …[truncated; full text in the subsystem report]

#### `CLI-11` — `ultrareview --post` / `--no-post` are new in 2.1.238; the port parses neither

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `/Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/commands/ultrareview.rs:20-32 — `pub struct Cli { target, json, timeout }`; the module header at :1-13 states the port "parses its surface ([target], --json, --timeout) byte-faithfully"`
- **oracle** 2.1.238 @308643854 region: `--post` → `Post the finished review's findings to the PR as you (PR targets only; one plain comment, not a review)`; `--no-post` → `Do not post the findings to the PR (the default; accepted for parity with the /ultrareview and /code-review ultra flags)`. Neither string appears in the 2.1.220 ultrareview block (token-level diff of the eagerly-registered command string table between the two binaries shows exactly these two flags added between `--timeout <minutes>` and the `disabled` token).
- **fix** Add `--post` and `--no-post` (mutually exclusive, `--no-post` the default) to ultrareview::Cli with the 2.1.238 help strings so scripted invocations stop hard-erroring; the stub run() can keep returning NOT_IMPLEMENTED.
- **verifier** Confirmed. Binary @308643854: the `ultrareview [target]` chain carries `.option("--post","Post the finished review's findings to the PR as you (PR targets only; one plain comment, not a review)").option("--no-post","Do not post the findings to the PR (the default; accepted for parity with the /ultrareview and /code-review ultra flags)")`; that copy = 0 hits in 2.1.220. Port ultrareview.rs:20-32 `C …[truncated; full text in the subsystem report]

#### `CLI-12` — `--messaging-socket-path <path>` is new in 2.1.238 and absent from the port

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -e 'messaging-socket-path' --glob '*.rs'` across lingxi-code returns 0 hits; apps/cli/src/argv.rs declares no such option`
- **oracle** 2.1.238 @307414302: `{let l=new bp("--messaging-socket-path <path>","Cross-session messaging server path: a Unix domain socket on Mac/Linux, a \\\\.\\pipe\\ name on Windows (defaults to an auto-generated path)");l.hideHelp(),t.addOption(l)}`. Literal `--messaging-socket-path` has 0 hits in 2.1.220 and 8 in 2.1.238.
- **fix** Add the hidden `--messaging-socket-path <path>` option to Argv and thread it into the cross-session messaging server bootstrap (msgqueue crate), defaulting to an auto-generated socket path when unset.
- **verifier** Confirmed. Binary @307414302: `new bp("--messaging-socket-path <path>","Cross-session messaging server path: a Unix domain socket on Mac/Linux, a \\\\.\\pipe\\ name on Windows (defaults to an auto-generated path)")` + .hideHelp(). Raw `grep -acoF -- '--messaging-socket-path'`: 0 in cc-bin-220, 8 in 2.1.238. Port: absent from argv.rs. I checked the BEHAVIOUR not just the name — the port does have t …[truncated; full text in the subsystem report]

#### `CLI-17` — Nineteen hidden root flags from 2.1.238 are absent from the port's argv parser

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — the complete long-flag inventory of /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code/apps/cli/src/argv.rs (68 long options, extracted with `grep -o 'long = "[a-zA-Z0-9-]*"'`) contains none of these names. Note the `agents`-subcommand copy of --plugin-dir-no-mcp IS ported (apps/cli/src/commands/agents.rs:108); only the root copy is missing.`
- **oracle** All extracted from the 2.1.238 root registration block (scratchpad/mainopts-238.txt, binary region ~307405000-307415000; each also present in 2.1.220, so none are new drift): `-d2e, --debug-to-stderr` (deprecated), `--init` ("Run Setup hooks with init trigger, then continue"), `--init-only` ("Run Setup and SessionStart:startup hooks, then exit"), `--maintenance` ("Run Setup hooks with maintenance trigger, then continue"), `--session-mirror`, `--task-budget <tokens>`, `--enable-auth-status`, `--workload <tag>`, `--managed-settings <json>`, `--plugin-dir-no-mcp <path>` (root copy), `--advisor <model>`, `--channels <servers...>`, `--dangerously-load-development-channels <servers...>`, `--sdk-url <url>`, `--prefill <text>`, `--prefill-b64 <b64>`, `--deep-link-origin`, `--deep-link-repo <slug>`, `--deep-link-last-fetch <ms>`, `--deep-link-cwd-b64 <b64>`, `--reply-on-resume`, `--enable-auto-mode` (deprecated).
- **fix** Triage as a batch: `--init`/`--init-only`/`--maintenance` are the only ones with a live counterpart in the port (lingxi-code/hooks/src/registry.rs already understands the `init` and `maintenance` Setup triggers), so wire those three first; declare the rest as hidden parse-and-carry options so SDK/automation callers stop hard-erroring, or record them as accepted divergences.
- **verifier** Substance confirmed; one count error that does not refute. I read the registration blocks directly: mainopts-238.txt (root chain) carries, each .hideHelp(): -d2e/--debug-to-stderr (.implies({debug:!0})), --init, --init-only, --maintenance, --session-mirror, --task-budget <tokens>, --enable-auth-status, --workload <tag>, --managed-settings <json>, --plugin-dir-no-mcp <path>, --prefill, --prefill-b6 …[truncated; full text in the subsystem report]

### Settings schema + hooks (settings.json keys/defaults/validation across all scopes; hook events, matcher semantics, payload fields, timeouts, hook-related flags/env)

#### `SH-01` — PostToolUse hook output field `classifierContext` (new in 2.1.238) is entirely unimplemented — hook-supplied context never reaches the auto-mode permission classifier

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n 'classifierContext|classifier_context' -g '*.rs'` over /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238/lingxi-code returns 0 hits. The port's PostToolUse hook-output model is lingxi-code/hooks/src/hook_payload.rs:1239 (`updated_mcp_tool_output`) plus `additional_context`; there is no classifier_context field, no 2000-unit cap, no paired-rewrite state. The auto-mode classifier at lingxi-code/permission/src/classifier.rs has no host-context input channel.`
- **oracle** 2.1.238 @ byte 296466460, in the `hookSpecificOutput` union arm for PostToolUse: `be({hookEventName:At("PostToolUse"),additionalContext:H().optional(),classifierContext:H().describe("Host-asserted context shown to the auto-mode permission classifier alongside this tool call's result. In the live session the classifier may weigh a user statement relayed here as user intent ... Capped at 2000 UTF-16 code units, a budget shared across all hooks that contribute to one call (surrogate-pair-safe; emoji and other astral characters count as two). Honored on synchronous hook responses only ...").optional(),updatedToolOutput:Fn()...,updatedMCPToolOutput:Fn()...})`. Consumption site: `if(z.classifierContext){let U=wo(z.classifierContext,Pfr); T(`Hook ${f} (${dJ(z.hook)}) provided classifierContext (${U.length} chars after cap)`), M.classifierContextChars+=U.length, G(z.hook,"classifierContextChars",U.length), yield{pairedRewrite: z.updatedToolOutput!==void 0?"direct":z.updatedMCPToolOutput!==void 0?"legacy_mcp":z.legacyMcpRewriteSuppressed?"suppressed":"none", classifierContexts:[{value:U,hostP …[truncated; full text in the subsystem report]
- **fix** Add `classifier_context: Option<String>` to the PostToolUse arm of the port's hook-response `hookSpecificOutput` model (hooks/src/hook_payload.rs / hooks/src/response.rs), cap it at 2000 UTF-16 code units surrogate-pair-safe (oracle `Pfr=2000`, `wo(value, 2000)`), honor it only on synchronous hook responses (drop it on async late responses), carry the `paired_rewrite` state (direct / legacy_mcp / suppressed / none) derived from whether the same hook result also set updated_tool_output / updated_mcp_tool_output, aggregate `classifier_context_chars` per hook and per plugin for telemetry, and thread the accepted values into permission/src/classifier.rs as `host_context` lines bound to the tool_ …[truncated; full text in the subsystem report]
- **verifier** Oracle confirmed: at 2.1.238 byte 296466460 the PostToolUse arm of the hook-output union carries `classifierContext:H().describe("Host-asserted context shown to the auto-mode permission classifier alongside this tool call's result...Capped at 2000 UTF-16 code units...Honored on synchronous hook responses only...").optional()`; `Pfr=2000`, `Zal="host_context"`, `Qal="host_context_live"`, `JDi()=it( …[truncated; full text in the subsystem report]

#### `SH-02` — New 2.1.238 permission-prompt Notification hook (6s delayed, `permission_prompt`) and its `CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS` gate are missing

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `rg -n 'permission_prompt' -g '*.rs'` finds only --permission-prompt-tool plumbing (apps/cli/src/argv.rs:595) and TUI test names. lingxi-code/orchestrator/src/conversation.rs:7870 `fire_notification(message, notification_type)` has exactly two call sites: lingxi-code/apps/cli/src/idle_notify.rs:109 ("idle_prompt") and lingxi-code/apps/cli/src/agents_notify.rs:174,188 ("agent_needs_input"/"agent_completed"). No permission-prompt notification, no 6s timer, no disable env var.`
- **oracle** 2.1.238 @ byte 178307264 (message) and @ 81377040 (env name): `function Cou(e){if(V.CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS)return()=>{};let t=setTimeout((r)=>{NY({id:zt(),project:{originalCwd:xn(),projectRoot:ul()}},{message:`Claude needs your permission to use ${r}`,notificationType:"permission_prompt"}).catch(()=>{})},A8n,e);t.unref();return()=>clearTimeout(t)}` with `A8n=6000` (2.1.238 @ byte 290068323: `var q_=600000,C7a=30000,A8n=6000`). `Cou(KNe(t.name))` wraps the `can_use_tool` control request and the sandbox network-ask callback; the returned disposer cancels the timer when the prompt resolves. Fixed-string counts 238 vs 220: CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS 3/0, "Claude needs your permission to use " 2/0. 2.1.220 has the `permission_prompt` token in the notification-type enum but zero construction sites, so nothing fired it.
- **fix** At the port's permission-prompt seam (the can_use_tool / permission-prompt-tool request path), arm a 6000 ms timer that calls `ConversationOrchestrator::fire_notification(&format!("Claude needs your permission to use {tool_display_name}"), "permission_prompt")` and cancel it when the prompt resolves; gate the whole thing on the env var (`CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS`, plus the LINGXI_* alias per branding policy) returning a no-op disposer when set.
- **verifier** Confirmed at the oracle and the port. 2.1.238 @ 307013340 (JS, not the V8-snapshot copy at 178307264): `function Cou(e){if(V.CLAUDE_CODE_DISABLE_PERMISSION_PROMPT_NOTIFY_HOOKS)return()=>{};let t=setTimeout((r)=>{NY({id:zt(),project:{originalCwd:xn(),projectRoot:ul()}},{message:`Claude needs your permission to use ${r}`,notificationType:"permission_prompt"}).catch(()=>{})},A8n,e);return t.unref(),( …[truncated; full text in the subsystem report]

#### `SH-03` — `prompt_id` is missing from the shared hook-input base, so no hook payload carries it on any of the 31 events

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/hooks/src/hook_payload.rs:1852-1858 — `struct BaseHookFields { session_id, transcript_path, cwd, permission_mode, agent_id, agent_type, effort }`; no prompt_id. Every concrete payload mirrors it, e.g. lingxi-code/hooks/src/hook_payload.rs:419-436 `UserPromptSubmitPayload`. `rg -n 'prompt_id' lingxi-code/hooks/` returns 0 hits (the identifier exists only in session/src/jsonl/schema.rs:159 for transcript records).`
- **oracle** 2.1.238 @ byte 306828246: `XO=we(()=>be({session_id:H(),transcript_path:H(),cwd:H(),prompt_id:H().optional().describe("UUID correlating a user prompt with all subsequent events until the next prompt. Same value emitted on OpenTelemetry events as the `prompt.id` attribute, so hook output can be joined to OTel events at prompt grain. Absent until the first user input of the process lifetime."),permission_mode:H().optional(),agent_id:H().optional()...,agent_type:H().optional()...,effort:be({level:H()...}).optional()...}))`. The same field with the identical describe string exists in 2.1.220 (@ byte 222583568), so this is a long-standing gap rather than new drift.
- **fix** Add `#[serde(skip_serializing_if = "Option::is_none", default)] pub prompt_id: Option<String>` to `BaseHookFields` and to every payload struct in hooks/src/hook_payload.rs, positioned after `cwd` and before `permission_mode` to preserve key order; source it from the per-prompt UUID the orchestrator already mints for the session JSONL `prompt_id` (session/src/jsonl/schema.rs:159) and leave it None until the first user turn of the process.
- **verifier** Confirmed, and strengthened beyond the finding's own evidence. The finding cited hook_payload.rs:1852 for BaseHookFields but that file is only 1391 lines — the struct is actually at hooks/src/executor.rs:1852-1868 (session_id, transcript_path, cwd, permission_mode, agent_id, agent_type, effort, session_title; no prompt_id), so the file attribution is wrong while the substance holds. Crucially, the …[truncated; full text in the subsystem report]

#### `SH-04` — `mcp_tool` hook type is not implemented — such settings entries are silently dropped

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/hooks/src/loader.rs:426-470 — `fn build_executor(entry: &HookEntry)` matches only `Some("command")`, `Some("http")`, `Some("agent")`, `Some("prompt")`; anything else returns None and the caller skips the entry. The port documents the omission at lingxi-code/hooks/src/attachment.rs:549: "The port has no `mcp_tool` / `callback` / `function` executor arms".`
- **oracle** 2.1.238 @ byte 282295711: `r=be({type:At("mcp_tool").describe("MCP tool hook type"),server:H().describe("Name of an already-configured MCP server to invoke"),tool:H().describe("Name of the tool on that server to call"),input:lo(H(),Fn()).optional().describe('Arguments passed to the MCP tool. String values support ${path} interpolation from the hook input JSON (e.g. "${tool_input.file_path}").'),if:Pxn(),timeout:Xe().positive().optional().describe("Timeout in seconds for this specific tool call"),statusMessage:H().optional()...,once:Bt().optional()...})` and it is one of the five members of the discriminated union `z0("type",[e,t,r,n,o])`. Byte-identical in 2.1.220, so pre-existing rather than new drift.
- **fix** Add `server`, `tool`, and `input: Option<BTreeMap<String, Value>>` to `HookEntry` (hooks/src/loader.rs), add a `HookExecutor::McpTool { server, tool, input }` variant, and implement the arm in hooks/src/executor.rs by resolving the already-connected MCP client for `server` and calling `tool` with `${path}` values interpolated from the hook input JSON. Until then, at minimum emit the oracle-style invalid-entry diagnostic instead of dropping the hook silently.
- **verifier** Oracle confirmed verbatim at 2.1.238 @282295711: `r=be({type:At("mcp_tool").describe("MCP tool hook type"),server:H().describe("Name of an already-configured MCP server to invoke"),tool:H()...,input:lo(H(),Fn()).optional().describe('Arguments passed to the MCP tool. String values support ${path} interpolation from the hook input JSON (e.g. "${tool_input.file_path}").'),if:Pxn(),timeout:...,statusM …[truncated; full text in the subsystem report]

#### `SH-05` — Nine settings keys added in 2.1.238 are absent from the port, including `disableCommandPluginSources` whose default is defined in terms of the hook-security setting `allowManagedHooksOnly`

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — from /Users/luolingfeng/Projects/LingXi-Next/.worktrees/parity-cc-2.1.238, `rg -c 'additionalMarketplaces|allowedMarketplaces|autoContinueAtUsageLimit|disableCommandPluginSources|keybindingFlavor|modelProposedGoals|policyHelpers|spellcheck|syncClaudeAiSkills' -g '!target' .` returns no matching files at all. The typed schema lingxi-code/core/src/settings/schema.rs:115-622 carries dialogExpiry (:188) and crossSessionInbound (:194) but none of the other nine.`
- **oracle** Brace-walked top-level key diff of the settings zod object (2.1.220 factory `ALi` @ byte 226813009 = 146 keys; 2.1.238 factory `gZt` @ ~byte 282366697 = 157 keys; 0 removed). The 11 additions, with 2.1.238 byte offsets: policyHelpers @282368479, syncClaudeAiSkills @282372041, additionalMarketplaces @282386972, allowedMarketplaces @282388068, disableCommandPluginSources @282388922, spellcheck @282394048, dialogExpiry @282397538, modelProposedGoals @282398334, keybindingFlavor @282406907, autoContinueAtUsageLimit @282408150, crossSessionInbound @282409688. The hook-coupled one reads: `disableCommandPluginSources:Bt().optional().describe("Controls the `command` plugin source, whose plugin directory is produced by running a marketplace-declared command on this machine. true: command-sourced plugins are never installed, updated, or re-resolved (the command never runs). false: explicitly allowed. Unset: follows allowManagedHooksOnly — an org that restricts hook execution to managed settings gets command sources disabled too. Only honored from managed settings.")`. Two of the eleven (crossS …[truncated; full text in the subsystem report]
- **fix** Prioritise `disableCommandPluginSources` (managed-only bool whose unset default must be read from the already-implemented `allowManagedHooksOnly` at core/src/settings/schema.rs:337 / hooks/src/loader.rs:163) and `policyHelpers` (per-OS managed-settings helper chain with defaultSettings fallback), since both sit in this subsystem. Add the remaining seven as typed Option fields on SettingsJson with their zod-equivalent enums (spellcheck object, keybindingFlavor "classic"|"readline", modelProposedGoals "auto"|"alwaysAsk"|"disabled", autoContinueAtUsageLimit bool, syncClaudeAiSkills bool, and the two marketplace aliases with their both-set-ignored-with-warning rule) and wire each to its consum …[truncated; full text in the subsystem report]
- **verifier** Spot-checked rather than trusting the brace walk, and it holds. `disableCommandPluginSources` read at 2.1.238 @282388922 matches the quoted describe string byte-for-byte including the hook coupling `Unset: follows allowManagedHooksOnly — an org that restricts hook execution to managed settings gets command sources disabled too. Only honored from managed settings.`; `spellcheck:be({enabled...,check …[truncated; full text in the subsystem report]

#### `SH-06` — Command-hook `shell` selector and `rewakeSummary` are dropped by the settings loader

- **severity** P3 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/hooks/src/loader.rs:216-296 — `struct HookEntry` declares kind, command, args, url, headers, allowedEnvVars, prompt, model, continueOnBlock, timeout, once, async, asyncRewake, asyncTimeout, rewakeMessage, statusMessage, if — but no `shell` and no `rewakeSummary`. Confirmed by `rg -n 'rewakeSummary|rewake_summary' lingxi-code/hooks lingxi-code/orchestrator/src` (0 hits) and `rg -n '"shell"|powershell' lingxi-code/hooks/src/` (only unrelated background_tasks type:"shell" labels). lingxi-code/hooks/src/definition.rs:17-66 `HookDefinition` likewise has rewake_message but no summary field.`
- **oracle** 2.1.238 @ byte 282293705: `shell:Mr(w$u).optional().describe("Shell interpreter. 'bash' uses your $SHELL (bash/zsh/sh); 'powershell' uses pwsh. Defaults to bash (powershell on Windows without Git Bash).")` where `w$u=["bash","powershell"]`; it drives the two spawn branches and the user-facing errors `Hook "<cmd>" has shell: 'powershell' but no PowerShell executable (pwsh or powershell) was found on PATH. Install PowerShell, or remove "shell": "powershell" to use bash.` and `Hook "<cmd>" requires bash but Git Bash was not found. Install Git for Windows (https://git-scm.com/downloads/win), or add "shell": "powershell" to this hook's config.`. 2.1.238 @ byte 282294595: `rewakeSummary:H().min(1).optional().describe('@internal One-line summary shown to the user in the terminal when an asyncRewake hook exits with code 2. Defaults to "Stop hook feedback".')`, threaded at registration as `LWm({... asyncRewake:e.asyncRewake, rewakeMessage:e.rewakeMessage, rewakeSummary:e.rewakeSummary, pluginId:d})`. Both fields are byte-identical in 2.1.220 — pre-existing gaps, not new drift.
- **fix** Add `#[serde(default)] shell: Option<String>` (validated against ["bash","powershell"]) and `#[serde(default, rename = "rewakeSummary")] rewake_summary: Option<String>` to `HookEntry`; carry `shell` onto `HookExecutor::Command` so the Windows path can select pwsh vs Git Bash and reproduce the two error strings verbatim, and carry `rewake_summary` onto `HookDefinition` next to `rewake_message`, defaulting the terminal one-liner to "Stop hook feedback" when absent.
- **verifier** Both oracle fields read directly. @282293705: `shell:Mr(w$u).optional().describe("Shell interpreter. 'bash' uses your $SHELL (bash/zsh/sh); 'powershell' uses pwsh. Defaults to bash (powershell on Windows without Git Bash).")`, immediately after the exec-form `args` describe and before `timeout`. @282294595: `rewakeSummary:H().min(1).optional().describe('@internal One-line summary shown to the user …[truncated; full text in the subsystem report]

#### `SH-07` — `hook_progress` stream-json frames are never emitted (hook_started/hook_response are)

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/apps/cli/src/stream_json.rs — `emit_hook_started` at :1405 and `emit_hook_response` at :1432 both implement the correct `matches!(hook_event, "SessionStart" | "Setup")`-always-on gate, but there is no hook_progress emitter; `rg -n 'hook_progress' -g '*.rs'` over the workspace returns 0 hits.`
- **oracle** 2.1.238: `function tWi(e){if(!Q9i(e.hookEvent))return()=>{};let t="",r=setInterval(()=>{e.getOutput().then(({stdout:n,stderr:o,output:i})=>{if(i===t)return;t=i,EjT({hookId:e.hookId,hookName:e.hookName,hookEvent:e.hookEvent,stdout:n,stderr:o,output:i})})},e.intervalMs??1000);return r.unref(),()=>clearInterval(r)}` emitting `u0({type:"system",subtype:"hook_progress",hook_id,hook_name,hook_event,stdout,stderr,output})`, under the same gate `Q9i(e)` = `TjT.includes(e) || allHookEventsEnabled && n9.includes(e)` with `TjT=["SessionStart","Setup"]` (gate class at 2.1.238 @ byte 296462897). The identical mechanism exists in 2.1.220 (`Buo`/`S7g`, module flag `yRu`), so this is a pre-existing gap; fixed-string counts hook_progress 31/30.
- **fix** Add `emit_hook_progress(hook_id, hook_name, hook_event, stdout, stderr, output)` to StreamJsonStream next to the existing two, sharing the same always-stream gate, and drive it from the hook executor with a 1000 ms interval that fires only when the accumulated combined output changed since the last tick.
- **verifier** Substance confirmed despite one sloppy evidence claim. Oracle @296463289: `function EjT(e){if(!Q9i(e.hookEvent))return;u0({type:"system",subtype:"hook_progress",hook_id:...,stdout:...,stderr:...,output:...})}` and the 1000 ms poller `tWi` that suppresses unchanged output, under the same gate `Q9i` used by `eWi` (hook_started) and `Gq` (hook_response) — i.e. exactly the SessionStart/Setup-always-on …[truncated; full text in the subsystem report]

### Permission engine

#### `PERM-01` — Write blocked by a Read deny rule reports the Edit wording ("cannot be edited"); 2.1.238 added a Write-specific "cannot be written"

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/permission/src/policy.rs:2954-2957 (message constant) reached from lingxi-code/permission/src/policy.rs:726-730; Write is classified Editor at lingxi-code/permission/src/filesystem.rs:155-161. `grep -rn "cannot be written" lingxi-code --include=*.rs` → 0 hits.`
- **oracle** 2.1.238 binary @73827984 (and @283747891): `File is covered by a Read deny rule in your permission settings and cannot be written.` — defined in cc-238.js alongside the Edit variant:
  `ssa="File is covered by a Read deny rule in your permission settings and cannot be edited.",asa="File is covered by a Read deny rule in your permission settings and cannot be written."`
  Edit uses `ssa` (cc-238.js @226401366 `throw new Q4e(ssa)`; @226406178 `return{result:!1,behavior:"ask",message:ssa,errorCode:13}`); Write uses `asa` (@226410692 `throw new Q4e(asa)`; @226415323 `return{result:!1,message:asa,errorCode:13}` — note NO `behavior:"ask"` on the Write arm).
  2.1.220: `grep -c 'cannot be written.'` = 0 — the Write spelling is new upstream drift after the port's baseline.
- **fix** Split the constant by tool in `ask_edit_read_deny_covered`: `Write` → "File is covered by a Read deny rule in your permission settings and cannot be written.", Edit/MultiEdit/NotebookEdit keep "…cannot be edited.". Additionally make the Write arm a validation failure (errorCode 13, no `behavior:"ask"`) to match the oracle, and mirror the same message on the Write tool's own call path.
- **verifier** Independently confirmed. cc-238.js @220242789 defines both constants side by side (`ssa="…cannot be edited.",asa="…cannot be written."`); Edit consumes `ssa` (@226401366 throw, @226406186 `{result:!1,behavior:"ask",message:ssa,errorCode:13}`) and Write consumes `asa` (@226410692 throw, @226415323 `{result:!1,message:asa,errorCode:13}` with no `behavior` key). Binary counts: 'cannot be written.' 23 …[truncated; full text in the subsystem report]

#### `PERM-02` — TUI permission dialog copy is invented, and 2.1.238 replaced the oracle's fixed session-row labels with a composed row grammar the port lacks entirely

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/tui/src/bottom_pane/permission_view.rs:100-110 — `DialogView::new("Permission required", …, vec!["Yes, allow once", "Yes, allow always", "No, deny"])`. `grep -rn "Yes, and switch to\|Yes, and always allow access\|Yes, allow all edits" lingxi-code` → 0 hits.`
- **oracle** Generic dialog: oracle title is `Do you want to proceed?` (238=9, 220=9). `Yes, allow once` = 0 in BOTH 238 and 220; `Yes, allow always` = 0 in BOTH.
  NEW DRIFT 220→238: `Yes, allow all edits during this session` (220=2, 238=0), `Yes, during this session` (220=2, 238=0), `Yes, allow all edits in ` (220=2, 238=0) were removed and replaced by a composed row grammar (cc-238.js @239407909 `ms0`, @238606287 `WVe`/`_Ss`/`bSs`) driven by a NEW mode-description table `fzE` at binary @268663104 / @302114120 (all six entries absent from 220): `default (ask each time)`, `accept edits (auto-approve file edits and common file commands)`, `auto (no routine prompts; a reviewer model screens actions)`, `don't ask (auto-deny anything that would prompt)`, `plan mode (research and propose changes without making them)`, `BYPASS PERMISSIONS (no further prompts)`; row texts `Yes, and switch to <fzE[mode]> for this session` (binary @302111776), `Yes, and always allow access to <dirs> for this session`, `Yes, and don’t ask again for any <Tool> command`, `Yes, and don’t ask again for: <content>`.
- **fix** Replace the invented dialog strings with the oracle grammar: header `Do you want to proceed?`, options `Yes` / composed session row / `No`, with the inline placeholders `and tell Claude what to do next` and `and tell Claude what to do differently`. Add the `fzE` mode-description table and the `WVe` / `_Ss` / `bSs` row-minting helpers so the session row reads `Yes, and switch to <desc> for this session`, `Yes, and always allow access to <dirs> for this session`, `Yes, and don’t ask again for: <ruleContent>` (U+2019 apostrophe).
- **verifier** Both halves confirmed at the bytes. Oracle: 'Do you want to proceed?' 238=10, 220=10; 'Yes, allow once' and 'Yes, allow always' = 0 in BOTH binaries (so the port's strings are invented, not stale); 'Yes, allow all edits during this session' 220=2 → 238=0; the new mode-description table is present ('default (ask each time)' 238=2/220=0, 'BYPASS PERMISSIONS (no further prompts)' 238=2/220=0) and the …[truncated; full text in the subsystem report]

#### `PERM-03` — PowerShell quoted-path manual-approval gate missing: both quote-related verdicts of w$i are absent from the port

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** likely
- **port** `lingxi-code/permission/src/powershell_containment.rs:1364-1391 — `ps_path_reasons` carries every SIBLING reason from the same oracle function (TILDE_USER, BACKTICK, PROVIDER_QUALIFIED, UNC, VARIABLE_EXPANSION, TRAVERSAL, GLOB_WRITE, GLOB_READ) plus `drive_relative_reason`/`non_fs_provider_reason` at :1393-1405, but neither quote reason. `grep -rn "quote characters cannot be statically validated\|quote-stripping" lingxi-code` → 0 hits. The port's only quote guard is the coarser argument-level `is_unvalidatable_value` / `has_unvalidatable_path_arg` (powershell_containment.rs:1126-1140, used at :2119).`
- **oracle** 2.1.238 binary @131290688 (also @294352637): `Paths containing quote characters cannot be statically validated and require manual approval`; binary @131286770 (also @294356142): `resolves near a sensitive file under quote-stripping and cannot be statically validated; requires manual approval`. Both = 0 in 2.1.220, both already present in 2.1.232.
  cc-238.js @230846400 `function w$i(e,t,r,n){let o=wW(e),i=o!==e; …` — when the raw path contained quote chars it (a) re-checks deny rules against every quote-stripped / backtick-unescaped / `::`-tail spelling, (b) `if(i&&f.allowed)return s(d)` forcing manual approval on a path that would otherwise be ALLOWED, and (c) `if(i&&!f.allowed&&f.decisionReason?.type==="safetyCheck")` returns the quote-stripping safetyCheck with `classifierApprovable:!1`.
- **fix** Port `w$i`'s quote branch into `powershell_containment.rs`: add the quote-stripped/backtick/`::` candidate deny re-scan, add `QUOTE_CHARS` = "Paths containing quote characters cannot be statically validated and require manual approval" returned whenever a quoted path would otherwise resolve to allowed, and add the byte-exact `Path '<raw>' resolves near a sensitive file under quote-stripping and cannot be statically validated; requires manual approval` safetyCheck arm with classifier_approvable:false. Add a test asserting a quoted in-workspace path is NOT auto-allowed.
- **verifier** Confirmed by reading the whole of `w$i` at cc-238.js @230846400: `let o=wW(e),i=o!==e;` then the quote-stripped/backtick/`::` candidate deny rescan, `let s=(m)=>({allowed:!1,…reason:"Paths containing quote characters cannot be statically validated and require manual approval"})`, `if(i&&f.allowed)return s(d)`, and `if(i&&!f.allowed&&f.decisionReason?.type==="safetyCheck")` returning the `resolves …[truncated; full text in the subsystem report]

#### `PERM-05` — bashCommandClamp permission layer missing — four new model-visible deny messages and two new decision reasons unreachable

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rni "bashCommandClamp\|bash_command_clamp" lingxi-code` → 0 hits. The enclosing substrate is also missing: `grep -rn "permission_layers\|PermissionLayer" lingxi-code --include=*.rs` → 0 (permissionLayers already existed in 2.1.220), and lingxi-code/workflow/src contains only lib.rs with no agent() options.`
- **oracle** `bashCommandClamp` is entirely new in 2.1.238 (0 hits in 2.1.220). New layer kind at cc-238.js @223523969: `case"bash_command_clamp":t={...t,bashCommandClamps:[...t.bashCommandClamps??[],i.rules]};break;`. New denials: (1) cc-238.js @226641715 `Permission to use ${e} has been denied: this agent carries a per-spawn bashCommandClamp, which scopes shell execution to a fixed set of Bash command forms — this surface cannot match them. Use the clamped Bash forms instead.`; (2) @226642107 `The ${e} permission check crashed and this agent carries a per-spawn bashCommandClamp; denying rather than running an unverified command.`; (3) @226655818 `Permission to use ${Bash} with command ${cmd} has been denied: this agent's Bash use is clamped to a fixed set of command forms (per-spawn bashCommandClamp), and …` + telemetry `tengu_bash_command_clamp_denied`; (4) @230889754 the PowerShell variant. New reasons at binary @73808064 / @282288325: `bashCommandClamp: no clamp rule matches this command`, `bashCommandClamp fail-closed: permission check crashed`.
- **fix** First add the `permissionLayers` substrate to the tool-permission context (kinds allowed_tools / disallowed_tools / bash_command_clamp / avoid_prompts / permission_mode / working_directory), then implement the clamp: accumulate `bashCommandClamps`, gate Bash/PowerShell/file-surface permission checks on it, and emit the four byte-exact deny messages plus the two decision reasons `bashCommandClamp: no clamp rule matches this command` and `bashCommandClamp fail-closed: permission check crashed`.
- **verifier** Confirmed. 'bashCommandClamp' 238=42 / 220=0; the layer fold at cc-238.js @223523969 (`case"bash_command_clamp":t={...t,bashCommandClamps:[...t.bashCommandClamps??[],i.rules]}`); the four deny strings and telemetry `tengu_bash_command_clamp_denied` are present in the 238 binary; the two new decision reasons are declared at cc-238.js @218783315 (`sMr="bashCommandClamp: no clamp rule matches this co …[truncated; full text in the subsystem report]

#### `PERM-04` — Parked / unanswered-permission family absent — three new 2.1.238 env gates plus the whole interrupted-turn parked-permission resume

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "PARKED_PERMISSION" lingxi-code` → 0; `grep -rn "pending_action" lingxi-code --include=*.rs` → 0; `grep -rn "tengu_resume_parked_permission" lingxi-code` → 0. The only near-hit is `"LINGXI_RESUME_INTERRUPTED_TURN"` listed as a scrubbed env name at lingxi-code/platforms/posix/src/process/runner.rs:71 — a name in a scrub list, not an implementation.`
- **oracle** New in 2.1.238 (all 0 in 2.1.220): `CLAUDE_CODE_HOLD_UNANSWERED_PARKED_PERMISSION` (binary @81396112, @307230273), `CLAUDE_CODE_RETIRE_UNANSWERED_PARKED_PERMISSION` (@81398816, @307230322), `CLAUDE_CODE_ADOPT_UNDERIVABLE_PARKED_PERMISSION` (@81393664, @307230221). New classifier outcomes `underivable_no_superseded` / `underivable_leaf_only` / `underivable_excluded` / `underivable_adoptable` (all 0 in 220). cc-238.js @243725312:
  `function FRy(e,t,r,n,o){if(!V.CLAUDE_CODE_RESUME_INTERRUPTED_TURN)return; … if(V.CLAUDE_CODE_ADOPT_UNDERIVABLE_PARKED_PERMISSION&&(V.CLAUDE_CODE_HOLD_UNANSWERED_PARKED_PERMISSION||V.CLAUDE_CODE_RETIRE_UNANSWERED_PARKED_PERMISSION)&&r!==void 0&&Piu(e,r,n,o)==="underivable_adoptable")return{kind:"adopted",…}}`
  and the retire log at @243789955 ("…retiring it unanswered (CLAUDE_CODE_RETIRE_UNANSWERED_PARKED_PERMISSION); not re-running the interrupted turn"). The substrate (`CLAUDE_CODE_RESUME_INTERRUPTED_TURN`, `CLAUDE_CODE_PARKED_PERMISSION_WAIT_MS`, telemetry `tengu_resume_parked_permission`) already existed in 2.1.220.
- **fix** Implement the interrupted-turn parked-permission resume in the print/stream-json path first (persisted control_response lookup for `external.pending_action`, the WAIT_MS park, `tengu_resume_parked_permission` outcomes), then layer the three 2.1.238 gates: HOLD (keep the parked permission open instead of cancelling), RETIRE (on timeout, retire the parked permission unanswered and do not re-run the turn), and ADOPT_UNDERIVABLE (adopt a sidechain-parked permission when `Piu()` classifies it `underivable_adoptable`, retiring the interrupted turn's toolUseIDs). Port `Piu`'s five outcomes verbatim.
- **verifier** Facts hold: HOLD=7/RETIRE=6/ADOPT=5 occurrences in 2.1.238 vs 0 in 2.1.220, `underivable_adoptable` 238=4/220=0, and cc-238.js @243724955 `function FRy(e,t,r,n,o){if(!V.CLAUDE_CODE_RESUME_INTERRUPTED_TURN)return; …}` with the '[print.ts] … retiring it unanswered (CLAUDE_CODE_RETIRE_UNANSWERED_PARKED_PERMISSION)' log. It is NOT an excluded remote feature (it is the print/stream-json control_respons …[truncated; full text in the subsystem report]

#### `PERM-06` — Port still ships the Git-Bash/Cygwin `!<symlink>` cookie resolver that 2.1.238 deleted

- **severity** P3 &nbsp;·&nbsp; **kind** *removed-upstream* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/permission/src/filesystem.rs:68-69 (`WINDOWS_COOKIE_SYMLINK_PREFIX: &[u8] = b"!<symlink>"`, `WINDOWS_COOKIE_SYMLINK_READ_BYTES`), :348-358 (call site in the resolution loop), :372-410 (`resolve_windows_cookie_symlink_path`, `read_windows_cookie_symlink_bytes`, `decode_windows_cookie_symlink_target`), plus tests at :1135-1190. Landed by lingxi-code/docs/claude-code-2.1.232-permission-parity-closure-2026-08-14.md:32.`
- **oracle** 2.1.232 had the whole mechanism — cc-232.js @204351189: `Fnt=Buffer.from("!<symlink>","ascii"),avt=Buffer.from([76,0,0,0,1,20,2,0,0,0,0,0,192,0,0,0,0,0,0,70]),d8g=["lnk","exe"],p8g=[".lnk",".exe",".exe.lnk"] … g8g=Fnt.length+2+8192` and cc-232.js @208628688 `o8s="Path traverses a Cygwin-emulated symlink (Git Bash follows it, Node does not) — manual approval required"`.
  2.1.238: `!<symlink>` = 0 hits, `.exe.lnk` = 0, `76,0,0,0,1,20,2,0,0,0,0,0,192,0,0,0,0,0,0,70` = 0, `onCookieRemainder`/`scanCandidates`/`displayTarget` = 0, `Path traverses a Cygwin-emulated symlink` = 0. 2.1.238's file-path check `PBn` (cc-238.js @222614717) has no Windows cookie branch at all. (2.1.220 also had 0 — the feature lived only between 221 and 232.)
- **fix** Remove `WINDOWS_COOKIE_SYMLINK_PREFIX`, `WINDOWS_COOKIE_SYMLINK_READ_BYTES`, `WindowsCookieSymlinkTarget`, `resolve_windows_cookie_symlink_path`, `read_windows_cookie_symlink_bytes`, `decode_windows_cookie_symlink_target`, the `enable_windows_cookie_symlinks` call site, and their tests; update the 2.1.232 closure doc to record that upstream deleted the feature in 2.1.238. If the extra deny coverage is deliberately kept as a LingXi hardening, mark it as an accepted divergence rather than parity.
- **verifier** Confirmed as an upstream deletion, verified with data literals rather than a symbol-name grep: `!<symlink>` cc-232.js=2 → 2.1.238 binary=0 (and 220=0); 'Path traverses a Cygwin-emulated symlink' 232=1 → 238=0; '.exe.lnk' 238=0. The only remaining 'Cygwin' bytes in 238 are the tmux hint 'tmux is not natively available on Windows. Consider using WSL or Cygwin.' (cc-238.js @233506386) — so the mechan …[truncated; full text in the subsystem report]

#### `PERM-07` — 2.1.238 wraps the prompts-unavailable deny in new copy (`Permission for this tool use was denied: it requires interactive approval …`)

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "Permission for this tool use was denied" lingxi-code` → 0 hits. The enclosing `shouldAvoidPermissionPrompts` path is unimplemented and only documented at lingxi-code/platform-api/src/permission_gate.rs:133 and :171. The port's headless deny (lingxi-code/permission/src/headless_gate.rs:40) implements the OTHER oracle helper (`xxf`/GRu), which is unchanged in 238 and correct for its path.`
- **oracle** 2.1.220 (cc-bin-220 @237621668) forwarded the raw ask message: `function G8s(e){return{behavior:"deny",message:e,decisionReason:{type:"asyncAgent",reason:"Action requires interactive approval and permission prompts are not available in this context"}}}`.
  2.1.238 (cc-238.js @226788648 + @233122489; binary @296627596) wraps it: `function DJa(e){return{behavior:"deny",message:Ixf(e),decisionReason:{type:"asyncAgent",reason:"Action requires interactive approval and permission prompts are not available in this context"}}}` with `Ixf(e)=\`Permission for this tool use was denied: it requires interactive approval, and permission prompts are not available in this session. The action was NOT performed. Do not claim it succeeded, and do not retry it in this session — report the limitation to the user, or suggest an alternative. What was requested: ${e}\``. The literal is 0 in 2.1.220.
- **fix** When the `avoid_prompts` permission layer / `shouldAvoidPermissionPrompts` path is implemented, emit the byte-exact `Ixf` wrapper (em dash U+2014) with `decisionReason {type: asyncAgent, reason: "Action requires interactive approval and permission prompts are not available in this context"}`, keeping the original ask message appended after `What was requested: `.
- **verifier** Oracle claim holds after checking the trap: the 4 hits of 'Permission for this tool use was denied' in 2.1.220 are all the unrelated '…denied. The tool use was rejected (eg. if it was a file edit…)' family; the wrapped variant 'Permission for this tool use was denied: it requires interactive approval, and permission prompts are not available in this session…' appears exactly once in 2.1.238 and ze …[truncated; full text in the subsystem report]

#### `PERM-09` — New in 2.1.238: settings `defaultMode: bypassPermissions` needs explicit consent in a VS Code-owned session

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/permission/src/cli_mode.rs:196-207 — `if let Some(default_mode) = settings.default_mode { … ordered.push(default_mode) }` with no IDE-owned branch; `CliModeSettings` (cli_mode.rs:24-67) has no `allow_dangerously_skip_permissions` / IDE-entrypoint input. `grep -rn "allowDangerouslySkipPermissions" lingxi-code --include=*.rs` finds only the unrelated engine-desktop boot flag.`
- **oracle** cc-238.js @220919227, inside `initialPermissionModeFromCLI`, new branch guarded by `YCe()` (= `entrypoint==="claude-vscode" && !childSession && !claudecode`, cc-238.js @218472260):
  `else if(_==="bypassPermissions"){if(s||t.allowDangerouslySkipPermissions)p.push(_);else if(p.length===0){f='Permission mode bypassPermissions from settings was ignored — enable the "Claude Code: Allow Dangerously Skip Permissions" setting in VS Code to consent to it',T('settings defaultMode "bypassPermissions" ignored for a VS Code-owned session without the allow-bypass setting',{level:"warn"}),N("tengu_settings_bypass_unconsented_noninteractive_ignored",{}),process.stderr.write(`⚠ ${f}\n`),p.push("default")}}else if(_==="auto")if(!u)p.push(_);else T('settings defaultMode "auto" ignored for the IDE session — auto-mode circuit breaker is active',{level:"warn"})`
  All three literals and `tengu_settings_bypass_unconsented_noninteractive_ignored` are 0 in 2.1.220.
- **fix** Low priority — LingXi has no `claude-vscode` entrypoint, so the branch is structurally unreachable today. If an IDE-host entrypoint is ever added, port the branch with the byte-exact notice and stderr `⚠ ` prefix. Separately, update the now-stale comment at lingxi-code/permission/src/cli_mode.rs:10-13: the Statsig gate `tengu_disable_bypass_permissions_mode` and its notice `Bypass permissions mode was disabled by your organization policy` were REMOVED upstream in 2.1.238 (220 = 7/4 hits, 238 = 0/0), so the port's omission is now correct rather than a deferral.
- **verifier** Both halves confirmed. New in 238: `tengu_settings_bypass_unconsented_noninteractive_ignored` 238=2/220=0 and 'Claude Code: Allow Dangerously Skip Permissions' 238=1/220=0. Removed in 238: `tengu_disable_bypass_permissions_mode` 220=8→238=0 and 'Bypass permissions mode was disabled by your organization policy' 220=4→238=0, while 'Bypass permissions mode was disabled by settings' survives and the p …[truncated; full text in the subsystem report]

### Session persistence, resume and compaction

#### `SC-03` — /autocompact status block is missing the two new window-source labels added in 2.1.238

- **severity** P1 &nbsp;·&nbsp; **kind** *copy-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/commands/core/src/autocompact.rs:88-101 (status_block) — only emits `"Auto-compact window: auto"` or `"Auto-compact window: {n} tokens (from LINGXI_AUTO_COMPACT_WINDOW)"``
- **oracle** 2.1.238 string table @134660816 lists, in order: `"Auto-compact window: "`, `"auto"`, `"experiment"`, `"clientdata"`, `"auto ("`, `" tokens)"`, `"env"`, `" tokens (from CLAUDE_CODE_AUTO_COMPACT_WINDOW)"`, **`"unknown-model"`**, **`" tokens (default for an unrecognized model)"`**, **`"model-default"`**, **`" tokens (default for this model)"`**, `" tokens (from settings)"`, `"Auto-compact is currently disabled (see /config)"`, the two explanation lines, `"settings"`, `"Overriding auto may result in high token usage, especially when resuming long sessions."`. The 2.1.220 table @110856512 is byte-identical MINUS the four bolded entries.
- **fix** Extend `status_block` to render a resolved `source` and its label: add `" tokens (default for this model)"` for the `model-default` source and `" tokens (default for an unrecognized model)"` for the new `unknown-model` source, plus `"Auto-compact is currently disabled (see /config)"` when auto-compact is off. This needs `compaction::thresholds::effective_context_window_size` to return a `(window, configured, source)` triple instead of a bare `u64`.
- **verifier** Oracle confirmed by reading both renderers side by side. 2.1.238 sgT @294999853 has the two new ternary arms `o==="unknown-model"?`${oc(n)} tokens (default for an unrecognized model)${i}`:o==="model-default"?`${oc(n)} tokens (default for this model)${i}`'; 2.1.220 $ly @236076164 is the identical expression WITHOUT them. count ' tokens (default for this model)' = 2 (238) / 0 (220). The model-defaul …[truncated; full text in the subsystem report]

#### `SC-04` — New compaction-failure hint copy "Prompt is too long · automatic compaction failed: …" is missing

- **severity** P1 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "automatic compaction failed" --include='*.rs'` over lingxi-code/ returns 0 hits; the port has the `"Prompt is too long"` half at lingxi-code/orchestrator/src/api_error_copy.rs:455 and llm-runtime/src/model/prompt_too_long.rs:40`
- **oracle** 2.1.238 @291946200 `function Fol(e){if(e?.reason!=="error"||!e.detail)return;return `${_V} \xB7 automatic compaction failed: `+Yl(e.detail,z$v,!0)}` with `_V="Prompt is too long"` @296616294 and `z$v=300` @291946404 (`Yl(s,n,true)` = first line only, truncated to 300 chars). Raw 32-byte literal `" \xB7 automatic compaction failed: "` at @120117043. `grep -acF 'automatic compaction failed: '` = 2 in 2.1.238, 0 in 2.1.220.
- **fix** In the reactive/PTL compaction path (compaction/src/ptl_retry.rs / compaction/src/reactive.rs), when the precompute outcome is `failed` with a non-empty detail, set the spinner hint to `format!("{PROMPT_TOO_LONG} \u{00b7} automatic compaction failed: {}", truncate_first_line(detail, 300))`, matching `Fol`.
- **verifier** Oracle confirmed: count 'automatic compaction failed: ' = 2 (238) / 0 (220); Fol @291938542 verbatim `${_V} \xB7 automatic compaction failed: `+Yl(e.detail,z$v,!0); _V="Prompt is too long" @296616295; z$v=300 @291946404. Critically, I verified the call site @292226226 -- `ep({content:Fol(qn)??_V,error:"invalid_request",...})` -- so this string is the CONTENT of the assistant API-error message the …[truncated; full text in the subsystem report]

#### `SC-01` — usage.output_tokens_details.thinking_tokens is absent from every usage object the port emits

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/apps/cli/src/stream_json.rs:916-931 (build_usage_block) and :252-262; lingxi-code/llm-runtime/src/anthropic.rs:19`
- **oracle** 2.1.238 @283631657 canonical zero usage: `DR={output_tokens_details:{thinking_tokens:0},input_tokens:0,cache_creation_input_tokens:0,cache_read_input_tokens:0,output_tokens:0,server_tool_use:{web_search_requests:0,web_fetch_requests:0},service_tier:"standard",cache_creation:{ephemeral_1h_input_tokens:0,ephemeral_5m_input_tokens:0},inference_geo:"",iterations:[],speed:"standard"}` — `output_tokens_details` is the FIRST key. 2.1.220 @233167154 is the identical object WITHOUT it (`grep -acF output_tokens_details` = 10 in 2.1.238, 0 in 2.1.220). It is merged in `nTe` @297183459 (`output_tokens_details:{thinking_tokens:t.output_tokens_details?.thinking_tokens??e.output_tokens_details.thinking_tokens}`), in `bso` @297184034, summed in addUsage `NVr` @297184901, declared in the async-agent result schema @292431591 (`output_tokens_details:be({thinking_tokens:Xe().nullable().optional()}).nullable().optional()`), and spread into the SDK result frame @283648740 (`total_cost_usd:0,usage:{...DR},modelUsage:{}`).
- **fix** Add an `output_tokens_details: {thinking_tokens: u64}` slot to the usage DTO. (1) In `normalize_anthropic_usage` read `output_tokens_details.thinking_tokens` into `TokenUsage::reasoning_output` — the current key `reasoning_output_tokens` (anthropic.rs:19) has ZERO hits in both 2.1.238 and 2.1.220, so that bucket is dead for Anthropic. (2) Emit `"output_tokens_details": {"thinking_tokens": N}` as the FIRST key of `build_usage_block` (stream_json.rs:917) and of the assistant `message.usage` at :252, matching `DR`'s order. (3) Sum it component-wise in `cost::Usage::add`.
- **verifier** Oracle confirmed: count 'output_tokens_details' = 10 in 2.1.238 vs 0 in 2.1.220; @283631657 DR literal has output_tokens_details:{thinking_tokens:0} as the FIRST key; NVr @297184901 sums it component-wise. Port confirmed: apps/cli/src/stream_json.rs:916-931 build_usage_block and :252-262 both lack the key. Also verified the sub-claim: 'reasoning_output_tokens' = 0 hits in BOTH binaries, so llm-cli …[truncated; full text in the subsystem report]

#### `SC-02` — Rate-limit resume checkpoint (.claude/RESUME.md + refs/claude/checkpoint-*) is entirely missing

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "refs/claude|RESUME.md|performRateLimitCheckpoint|MAX_CHECKPOINT" --include='*.rs'` over lingxi-code/ returns only local-apps/src/checkpoints.rs:30 `const CHECKPOINT_REF_PREFIX: &str = "refs/lingxi/checkpoints";` (an unrelated local-apps feature)`
- **oracle** 2.1.238 @292197753 module `eal` exports `CHECKPOINT_REF_PREFIX, MAX_CHECKPOINT_FILE_COUNT, MAX_CHECKPOINT_TOTAL_BYTES, RESUME_MD_REPO_PATH, clearLastCheckpointResult, getLastCheckpointResult, performRateLimitCheckpoint, useRateLimitCheckpointResult`. Constants @292207306: `nDi=".claude/RESUME.md", oDi="refs/claude/checkpoint-", xxe=30000, v6f=25000, T6f=2147483648, b6f=500, j4v=1209600`. `K4v` @292206100 emits the user-visible RESUME.md: `"# Claude Code — resume checkpoint"`, `"Trigger: ${e.trigger===\"near_limit\"?\"near-limit\":\"rate-limited\"}"`, `"    claude --resume ${e.sessionId}"`, `"(or open Claude Code in this directory and run /resume)"`, `"No task list was active; see transcript via the resume command above."`, and the closing `"Don't want these changes? Resume this session (above), then run\n`/rewind` to roll back the turn's tool edits (bash-made changes\nexcluded). ${e.ref} holds a full snapshot until this session's\nnext checkpoint, or for up to ~2 weeks."`. `grep -acF performRateLimitCheckpoint` = 6 in 2.1.238, 0 in 2.1.220 (6 in cc-232.js, so it landed between 220 an …[truncated; full text in the subsystem report]
- **fix** Port `performRateLimitCheckpoint` as a new module under `session/`: the git plumbing sequence (`read-tree HEAD` under a private `GIT_INDEX_FILE`, `ls-files -z --cached` + `-z -o --exclude-standard`, per-path lstat with the 25000-file / 2 GiB caps, `hash-object -w --no-filters --stdin-paths`, `update-index --add --index-info`, `write-tree`, `commit-tree -p HEAD -m "WIP: Claude Code rate-limit checkpoint (<sid8>)"`, `update-ref --no-deref refs/lingxi/checkpoint-<sid8>`), the `info/exclude` append of `/.lingxi/RESUME.md`, the 14-day sibling-ref GC, the byte-exact RESUME.md renderer, and the 14 skip reasons (non_interactive, remote_workspace, policy, not_git, bare_repo, gitroot_uncontained, gitd …[truncated; full text in the subsystem report]
- **verifier** Oracle confirmed: performRateLimitCheckpoint = 6 hits in 2.1.238, 0 in 2.1.220; nDi='.claude/RESUME.md', oDi='refs/claude/checkpoint-', xxe=30000, v6f=25000, T6f=2147483648, b6f=500, j4v=1209600 all read at the tail of the K4v region; K4v @292206132 read in full and every quoted RESUME.md line ('# Claude Code — resume checkpoint', '    claude --resume ${e.sessionId}', '(or open Claude Code in this …[truncated; full text in the subsystem report]

#### `SC-05` — Synthetic/API-error assistant JSONL envelope is missing `diagnostics` (new in 2.1.238) and `usage` (present since 2.1.220)

- **severity** P2 &nbsp;·&nbsp; **kind** *schema-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/orchestrator/src/conversation.rs:5215-5256 (with the incorrect justification comment at :5207-5209)`
- **oracle** 2.1.238 `Mqm` @296632761: `message:{diagnostics:null,id,container:null,model:yD,role:"assistant",stop_details:null,stop_reason:"stop_sequence",stop_sequence:"",type:"message",usage:l,content:e,context_management:null}` where the default `usage:l={output_tokens_details:null,input_tokens:0,output_tokens:0,cache_creation_input_tokens:0,cache_read_input_tokens:0,server_tool_use:{web_search_requests:0,web_fetch_requests:0},service_tier:null,cache_creation:{ephemeral_1h_input_tokens:0,ephemeral_5m_input_tokens:0},inference_geo:null,iterations:null,speed:null}`. Its caller `ep(...)` @296633700 (createAssistantAPIErrorMessage) passes NO `usage` argument, so the JS default parameter substitutes and the object IS written. The 2.1.220 twin `plp` @238010511 is identical minus `diagnostics:null` and minus `output_tokens_details:null`. Corroborated at @283648740 where `Qkd` also builds `message:{diagnostics:null,id,container:null,model:yD,…}`.
- **fix** Insert `"diagnostics": null` as the FIRST key of the synthetic assistant `message`, and add the full zero-usage object (leading with `"output_tokens_details": null`) as the `usage` key between `"type"` and `"content"`. Delete the comment at conversation.rs:5207-5209 — it misreads JS default parameters: `usage:undefined` triggers the default, it does not drop the key.
- **verifier** Oracle confirmed: Mqm @296632761 read in full -- message:{diagnostics:null,id,container:null,model:yD,role:"assistant",stop_details:null,stop_reason:"stop_sequence",stop_sequence:"",type:"message",usage:l,content:e,context_management:null} with the JS DEFAULT PARAMETER `usage:l={output_tokens_details:null,input_tokens:0,...}`; ep() passes no usage key, so the default applies and the key IS written …[truncated; full text in the subsystem report]

#### `SC-06` — auto-compact window resolution has no `unknown-model` source and never emits its notice

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/compaction/src/thresholds.rs:154-166 (effective_context_window_size) — resolves `min(context_window_for_model, LINGXI_AUTO_COMPACT_WINDOW) - min(max_output, 20000)` with no source taxonomy; `grep -rn "unknown-model" --include='*.rs'` finds only unrelated cost/pricing strings`
- **oracle** 2.1.238 `N8` (resolveAutoCompactWindow) @~286417000 ends with a branch absent from the 2.1.220 twin `aY` @~230832000: `if(iO()&&!V.CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT&&!vyS(e,r)&&!C1r(e)&&!jpt(e,n))return{window:o,configured:o,source:"unknown-model"};`. New user-visible copy at @176869607-176877409, none present in 2.1.220: `" is not a model this version of Claude Code recognizes, so auto-compact will keep this session within "`, `" tokens (the context window it assumes). "`, `"map it in the modelOverrides setting or update Claude Code; CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT=1 restores the previous wait-for-the-API behavior"`, `"append [1m] to the model name for 1M"`, `"set CLAUDE_CODE_MAX_CONTEXT_TOKENS to its real window"`, `"To make it recognized, "`, `"unknown-model notice failed"`.
- **fix** Have the window resolver return `{window, configured, source}` with the 2.1.238 precedence (env → settings → clientdata → experiment → model-default → unknown-model → auto) and add the notice renderer. NOTE: the enforcement half interacts with LingXi's user-confirmed multi-provider divergence (many non-Claude models are legitimately 'unrecognized'); consider porting the source taxonomy + copy but gating the clamp behind a first-party-profile check rather than adopting it for every provider.
- **verifier** Oracle confirmed by reading N8 in full at ~286412900: the terminal branch `if(iO()&&!V.CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT&&!vyS(e,r)&&!C1r(e)&&!jpt(e,n))return{window:o,configured:o,source:"unknown-model"}' is verbatim and the full precedence chain (env/settings/clientdata/experiment/model-default x2/unknown-model/auto) matches. CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMEN …[truncated; full text in the subsystem report]

#### `SC-08` — Transcript-file compaction (performCompactTranscript) is missing while its counterpart, the metadata re-append that creates the garbage, is ported

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "Transcript compact|performCompactTranscript|bytesSinceCompact|backstopThreshold" --include='*.rs'` over lingxi-code/ returns 0 hits, while lingxi-code/session/src/jsonl/re_append.rs (METADATA_REAPPEND_BACKSTOP_BYTES = 32768) does implement the re-append that produces the duplicates`
- **oracle** 2.1.238 @296788068 `performCompactTranscript(e,t,r,n)` rewrites `<sid>.jsonl` via `<sid>.jsonl.compact.tmp.<8hex>`, dropping superseded sidecar records; guarded by three 4 KiB sample windows + inode + size re-checks, then `rename` and `reAppendSessionMetadataAsync(!1,!0)`. Constants @296896686: `I6m=5242880` (min size to bother), `Uyr=20971520` (backstop), `P6m=8*Uyr`, `O6m=0.1` (reclaim <10% ⇒ double the backstop up to P6m). Telemetry `tengu_transcript_compact{bytesBefore,bytesAfter}` and `tengu_transcript_compact_failed{reason: snapshot_mid_line|source_changed|rename_fallback|io}`; warn copy `"Transcript compact failed ("` / `"Transcript compact skipped ("`. Triggered from insertMessageChain on each compact boundary (@296794258 `this.backstopThresholdBytes=Uyr,this.requestCompact(this.sessionFile,a)`). Same constants in 2.1.220 @237933754 — pre-existing, not new drift.
- **fix** Port `performCompactTranscript` into `session/src/jsonl/` with the 5 MiB entry threshold, the 20 MiB backstop and its 10%-reclaim doubling up to 160 MiB, the tmp-file + inode/sample-window verification, the tail-append of bytes that arrived during the rewrite, and the post-rename `plan_re_append(skip_title_adopt=false, skip_dedup=true)`. Trigger it from the compact-boundary append path.
- **verifier** Oracle confirmed: performCompactTranscript = 8 hits in 2.1.238 and 4 in 2.1.220 (function def @296788068), 'Transcript compact failed (' = 4/2 -- i.e. pre-existing, exactly as the finding itself states rather than claiming new drift. Constants re-read @296896686: I6m=5242880, Uyr=20971520, O6m=0.1. Port confirmed: 0 hits for any of Transcript compact / performCompactTranscript / bytesSinceCompact …[truncated; full text in the subsystem report]

#### `SC-09` — New `--resume-drops-turn <message id>` flag and its refusal diagnostics are absent

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "resume-session-at|resume_session_at|resume-drops-turn" --include='*.rs'` over lingxi-code/ returns 0 hits (apps/cli/src/argv.rs has `--fork-session` but neither of these)`
- **oracle** 2.1.238 @181358016 flag spec `--resume-drops-turn <message id>` with description `"With --resume-session-at in print mode: declare the prompt uuid of the turn the truncating resume intends to discard; the resume is refused if the discarded range contains anything not attributable to that turn (absorbed queued messages, task notifications, content from other turns). Ignored outside print mode, like --resume-session-at."`; validation error `"Error: --resume-drops-turn requires --resume-session-at"`; refusal reasons @306800014-306800745 `"range does not start with the declared turn prompt; first discarded "`, `"declared turn id names a non-prompt user entry; "`, `"range contains a compaction summary; "`, `"range contains a non-furniture attachment; "`. `grep -acF '--resume-drops-turn'` = 10 in 2.1.238, 0 in 2.1.220 (`--resume-session-at` = 10 vs 9, i.e. pre-existing).
- **fix** Add `--resume-session-at <message id>` and `--resume-drops-turn <message id>` to apps/cli/src/argv.rs with the verbatim help text, the `Error: --resume-drops-turn requires --resume-session-at` guard, and the truncating-resume validator that refuses when the discarded range contains anything not attributable to the declared turn (emitting the four byte-exact reason prefixes). Overlaps with the CLI-flags subsystem — coordinate to avoid a duplicate port.
- **verifier** Oracle confirmed @181358016: the flag spec '--resume-drops-turn <message id>' and the full description string are byte-verbatim as quoted. count '--resume-drops-turn' = 10 (238) / 0 (220); count '--resume-session-at' = 10 (238) / 9 (220), matching the finding's own claim that --resume-session-at is pre-existing and only --resume-drops-turn is new. Port confirmed: grep for resume-session-at|resume_ …[truncated; full text in the subsystem report]

#### `SC-10` — New `--autocompact <auto|tokens>` CLI flag is absent

- **severity** P2 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `absent — `grep -rn "autocompact" lingxi-code/apps/cli/src/argv.rs` returns 0 hits`
- **oracle** 2.1.238 @181363296: flag spec `"--autocompact <auto|tokens>"` followed (UTF-16-encoded) by the description `"Auto-compact window size (auto, or 100k–1M tokens)"`. The flag spec string is absent from 2.1.220.
- **fix** Add `--autocompact <auto|tokens>` to apps/cli/src/argv.rs with the byte-exact description (note the U+2013 en dash in `100k–1M`), parsed by the same `auto` / `Nk` / `Nm` / bare-integer grammar as `DUn` (2.1.238 @286415xxx: `k`⇒×1000, `m`⇒×1e6, bare 100..1000 ⇒ ×1000, clamped to 100000..1000000), feeding the settings tier of the window resolver.
- **verifier** Oracle confirmed @181363296: '--autocompact <auto|tokens>' followed by the UTF-16 description 'Auto-compact window size (auto, or 100k-1M tokens)' (with U+2013). count = 2 in 2.1.238, 0 in 2.1.220 -> new. Port confirmed: grep 'autocompact|auto-compact' over apps/cli/src/argv.rs = 0 hits. P2 not inflated.

#### `SC-07` — `sessionKind` is never stamped on transcript lines, so the port's own daemon filter and `bg` picker badge are dead

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/session/src/jsonl/loader.rs:452-464 reads it; NO writer — `grep -rn "sessionKind" --include='*.rs'` over lingxi-code/ returns only that reader plus session/tests/session_gap_fixes_test.rs`
- **oracle** 2.1.238 @296794533 (and identically 2.1.220 @237863007) `insertMessageChain` builds every line as `{parentUuid, logicalParentUuid, isSidechain, teamName, agentName, promptId, agentId, ...entry, sessionKind:a3e(), userType, entrypoint, cwd, sessionId, version, gitBranch, slug}`; `a3e()` @283798463 = `CLAUDE_CODE_SESSION_KIND` when it is `"bg"|"daemon"|"daemon-worker"`. Consumed by the /resume picker filter and by the picker description `kMn` @283763615 (`...e.sessionKind==="bg"?["bg"]:[]`).
- **fix** Stamp `sessionKind` in the JSONL writer between `agentId`/entry-body and `userType`, sourced from `LINGXI_SESSION_KIND` when it is one of `bg|daemon|daemon-worker` (omit otherwise, matching TS `undefined`). Add the field to `session::JsonlMessage`'s recognized-extra ordering so it lands in claude's slot rather than the unrecognized tail.
- **verifier** The mechanical claim is confirmed: oracle @296794533 stamps sessionKind:a3e() into every chain entry (identically in 2.1.220, so this is an OLD gap, not new drift); a3e() @283798463 returns CLAUDE_CODE_SESSION_KIND when bg|daemon|daemon-worker. Port: session/src/jsonl/loader.rs:459 reads it, and the serializer at session/src/jsonl/schema.rs:420-437 emits the trailer userType/entrypoint/cwd/session …[truncated; full text in the subsystem report]

#### `SC-11` — New `atis-latch` transcript sidecar record type is neither written, re-appended, nor read

- **severity** P3 &nbsp;·&nbsp; **kind** *missing-in-port* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/session/src/jsonl/re_append.rs:68-126 (SessionMetadataState) has no `atis` field; `grep -rn "atis" --include='*.rs'` over lingxi-code/ returns 0 hits`
- **oracle** `grep -acF 'atis-latch'` = 15 in 2.1.238, 0 in 2.1.220. Written mid-chain by insertMessageChain @296794049 (`{type:"atis-latch",atis:g,sessionId:f}`, gated by `atisLatchStamp !== "${sessionFile}\n${atis}"`), by planReAppendSessionMetadata @296782335 (between `isolation-latch` and `worktree-state`), and by the branch/fork serializers @296240308 and @297497713. Read back with validation `typeof X.atis==="string" && /^[\x21-\x7e]*$/.test(X.atis)` at @296864709 and @297494618. Added to both policy tables @296898787: `"atis-latch":"always"` and `"atis-latch":"last-wins"`.
- **fix** Add `atis: Option<String>` to `SessionMetadataState` and emit `{type:"atis-latch",atis,sessionId}` between the `isolation-latch` and `worktree-state` entries in `plan_re_append`; add the reader (with the `^[\x21-\x7e]*$` guard) to the session-index scan and the fork/branch serializers; register the type as `always`/`last-wins`. Low priority: the value is a server-supplied opaque token forwarded as an Anthropic request header, so it is inert on non-Anthropic routes.
- **verifier** Oracle confirmed: count 'atis-latch' = 15 (238) / 0 (220). Read planReAppendSessionMetadata @296782335 directly -- atis-latch is pushed between isolation-latch and worktree-state exactly as claimed -- and the mid-chain write at @296794049 with the atisLatchStamp gate. Port confirmed: grep 'atis' over lingxi-code/*.rs yields only substring false positives (satisfies/satisfied/conservatism); no fiel …[truncated; full text in the subsystem report]

#### `SC-12` — Metadata re-append dedup comparator no longer matches: 2.1.238 also strips `ts` and special-cases `history-suppression`

- **severity** P3 &nbsp;·&nbsp; **kind** *behavior-drift* &nbsp;·&nbsp; **confidence** verified
- **port** `lingxi-code/session/src/jsonl/re_append.rs:627-640 (`fn without_timestamp`) — `if key != "timestamp"` only`
- **oracle** 2.1.238 @296784560: `let f=(m)=>{if(m.type==="history-suppression")return Ie({type:m.type,sessionId:m.sessionId});let{timestamp:h,ts:g,...y}=m;return Ie(y)};`. 2.1.220 @237855xxx (tail of planReAppendSessionMetadata): `let p=(f)=>{let{timestamp:m,...g}=f;return Ie(g)};` — strips `timestamp` only, with no per-type special case.
- **fix** Rename to `dedup_key` and (a) strip both `"timestamp"` and `"ts"`, (b) short-circuit `type == "history-suppression"` to `{"type":…,"sessionId":…}`. Update the doc comment at :622 and the `dedup_ignores_the_timestamp_field` test at :1141 to cover a `ts`-only difference.
- **verifier** Oracle confirmed on both sides. 2.1.238 @296784560: `let f=(m)=>{if(m.type==="history-suppression")return Ie({type:m.type,sessionId:m.sessionId});let{timestamp:h,ts:g,...y}=m;return Ie(y)};'. I located and read the 2.1.220 twin at the tail of planReAppendSessionMetadata (~237856700): `let p=(f)=>{let{timestamp:m,...g}=f;return Ie(g)};' -- timestamp only, no per-type case. history-suppression = 0 h …[truncated; full text in the subsystem report]

## Refuted during verification

- **ST-11** — Glob and Grep do not implement userFacingName "Search" or getActivityDescription

  REFUTED as stated: the port implements both hooks under different names in the display layer. tui-core/src/tool_display/header.rs:361-372 maps "Grep" and "Glob" to ToolVerb::Search, whose English label is "Search" (header.rs:97) - that is userFacingName(); header.rs:417 maps `"Grep" | "Glob" => "Searching"` for the in-flight activity label, pinned by tui/src/chat_widget.rs:8542-8543. The finding's evidence is also factually wrong - it asserts a repo-wide grep for "Searching for"/"Finding files" returns 0 hits, but tui-core/src/collapse/group.rs:250 contains "Searching for". This is the negative-grep-of-an-oracle-symbol failure mode. Only a much narrower residual survives (Glob should read "Finding ..."/"Finding files" rather than "Searching", and neither appends the truncated pattern), which is not what the finding claims.

- **SLASH-07** — core_description picks the non-interactive twin for /autocompact, /usage and /goal, so /help shows the hidden variant's text

  REFUTED — the port implements this correctly, split across two surfaces. `core_description` does NOT feed the interactive palette: the TUI /help is ChatWidget::cmd_help → ScreenView::help() → help_lines() → crate::command::advertised() (tui/src/bottom_pane/screen_view.rs:892-915), and tui/src/command.rs already carries the local-jsx strings for all three. `core_description`/render_help_screen feeds commands/core/src/help.rs::HelpHandler, the non-TUI surface, and the oracle's headless filter `R4m` (`type==="prompt"&&!disableNonInteractive || type==="local"&&supportsNonInteractive`) drops every local-jsx twin — so on that surface the oracle's visible objects are exactly the ones the port copied (HUi "Configure the auto-compact window size", Ykl "Show session cost, plan usage, and what's contributing to your limits", NBT "Set a goal — keep working until the condition is met", each with `get isHidden(){return!Dn()}` on the interactive side). The claim that "the port's two registries contradict each other" misreads a faithful twin split; no wrong byte reaching a user was demonstrated.

- **SLASH-12** — /list-agents is new in 2.1.238 and absent from the port registry

  REFUTED — flag-gated, not a gap. `pUT` carries `isEnabled:()=>eg()` and `function eg(){if(V.CLAUDE_CODE_HARBOR_KITE)return!0;if(Wt()==="windows"&&!it("tengu_harbor_kite_win",!1))return!1;return it("tengu_harbor_kite",!1)}` — statsig default FALSE. For an ordinary user the command is neither registered, dispatchable, nor listed in /help, so the port's omission is behaviorally identical to the oracle's default surface. The finding itself concedes "the default visible surface already matches". The feature is also cross-session/cloud messaging, adjacent to the excluded Remote-Control surface.

- **PERM-08** — New in 2.1.238: `permission_check_crashed` objection ask (crashIsObjection) has no port counterpart

  HARD EXCLUSION #1 (the Artifact tool: publish/assets/comments/capabilities). `crashIsObjection` has exactly four occurrences in cc-238.js: the bytecode string table (@51425470), the consumer inside `a6e` (@226791958), and the ONLY two call sites that ever pass it — @230258576 `weT` and @230275744 — both inside the artifact comment auto-react probe (`await a6e(n,o,{...i,toolUseId:a},{crashIsObjection:!0})` guarded by `artifact_comment_fixed_ack_on_probe_verdict` / `Se("artifact_comments_autoreact",…)` / `CLAUDE_CODE_ARTIFACT_COMMENT_FAST_ACK`). The option defaults undefined everywhere else, so `permission_check_crashed` is unreachable outside artifact comments. The finding presents it as a general pre-ask permission-engine arm, which the bytes do not support.

## Detailed evidence

Each auditor wrote a full report with byte offsets and side-by-side quotes:

- `agent-task-tool.md`
- `bash-tool-VERIFY.md`
- `bash-tool.md`
- `cli-surface.md`
- `file-tools-verification.md`
- `file-tools.md`
- `glob-grep-verify.md`
- `permissions.md`
- `reminders.md`
- `search-tools.md`
- `session-compaction-VERIFY.md`
- `sessions-compaction.md`
- `settings-hooks.md`
- `slash-command-registry-VERIFY.md`
- `slash-commands.md`
- `system-prompt.md`
- `system-reminder-family-VERIFY.md`
- `task-agent-subagent-VERIFY.md`
- `tool-registry.md`

Those live in the session scratchpad, not the repo, because they quote large spans of the
oracle binary verbatim.

## Addendum — tool registry (TR-01 … TR-10)

The tool-registry verifier died on a connection error during the first pass; these ten findings were
re-verified in a separate run, refute-by-default, and two were struck.

| id | verdict | sev | summary |
|---|---|---|---|
| TR-01 | CONFIRMED | P1 | `ReportFindings` is advertised every session by the oracle (`es()` defaults `isEnabled` to true) and does not exist in the port. |
| TR-02 | CONFIRMED | P1 | `ListMcpResourcesTool` / `ReadMcpResourceTool` are registered unconditionally; the oracle drops them by name and re-adds them only when the server declares `capabilities.resources`. |
| TR-03 | CONFIRMED | P2 | `ReadMcpResourceDirTool` is missing; the port already carries its name in alias tables with no implementation behind them. |
| TR-04 | CONFIRMED (claim corrected) | P2 | `WaitForMcpServers` is genuinely missing, but it is **not** new in 2.1.238 — 2.1.220 registers it too. Only the force-add leg is new. Pre-existing gap, not drift. |
| TR-05 | CONFIRMED | P2 | `MultiEdit` is a port-extra. All 12 oracle hits are name-strings (permission-rule alias map, display-verb map, trust dialog, docs, V8-snapshot copies); there is no tool object, no alias, and no dispatch shim. |
| TR-06 | CONFIRMED | P3 | `ProposeGoal` is new in 2.1.238 but sits behind a code-default-false gate, so it has **zero wire-byte impact** today. |
| TR-07 | **REFUTED** | — | Out of scope both ways: one half is gated on a remote/cloud environment, the other needs a first-party Anthropic backend that LingXi's multi-provider divergence rules out. |
| TR-08 | **REFUTED** | — | The new deny-rule leg is dead code *in the shipped oracle*: `entryFieldName` and `perEntryHookInputs` have no definitions, and no shipped tool satisfies the predicate. Porting it would change nothing observable. |
| TR-09 | CONFIRMED | P3 | The WebFetch-suppression path is real 220→238 drift, but it is triple-gated and also needs a `web-fetch` builtin agent the port lacks. Track as drift; no byte impact. |
| TR-10 | CONFIRMED | P2 | **Both tool-list snapshots are stale and the test is RED on `main`.** `ListAgentsTool` landed in `152710a07` (2026-08-18); the snapshots were last written by `b59db11e8` (2026-07-23) and contain no `ListAgents` row. |

TR-10 is the one to take seriously beyond its own fix: a snapshot test guarding the tool registry — the
exact artefact this audit relies on — sat red for four weeks without anyone noticing.

### Ordering constraint

TR-10's snapshot regeneration must be done **last**, after TR-01/02/03/04/05 land, so a single
regeneration covers every registry change instead of racing them.

## Correction — ST-01 was a FALSE finding, and it survived adversarial verification

`ST-01` ("Grep description says *through a registered shell tool* where the oracle names Bash") was
confirmed by the auditor, survived the refute-by-default verifier, and was applied in the first
implementation wave. **It is wrong, and it has been reverted.**

The oracle does not contain the literal `Bash` in that sentence. It stores an interpolation slot and
substitutes the shell tool's name at runtime — visible as non-printable bytes in the string table:

```
- ALWAYS use ·······(slot)······ for search tasks. NEVER invoke `grep` or `rg` as a ·······(slot)······
  command. The ·······(slot)······ tool has been optimized for correct permissions and access.
```

`Use ·······(slot)······ with jq to read specific portions:` (238 @113219062) is the same shape, and so
is the 2.1.220 copy (@109111686). LingXi renders that slot as **"a registered shell tool"** across the
whole repo — `tools/file/src/read.rs` (x3), `tools/web/src/web_fetch.rs`, `agent/src/builtins.rs` (x4),
`orchestrator/src/prompt/body_sections.rs` — because it registers more than one shell tool (Bash and
PowerShell). Two tests assert the convention: `grep.rs::description_is_byte_faithful` (`!d.contains("Bash")`)
and `agent/src/builtins.rs:722`. It is an intentional divergence, and `description_is_byte_faithful`
going red is what caught the mistake.

**Reverted at four sites** (Grep long description, `GREP_PROMPT_SHORT`, the `prompt()` literal, and the
notebook-too-large message). The rest of `FT-05` was kept: 2.1.238 genuinely de-parameterised the PATH
in that message — 2.1.220 interpolated `cat "<path>"` (quoted), 2.1.238 emits the literal
`cat <notebook_path>` (unquoted).

**The lesson for the next pass.** Both the auditor and the verifier compared a rendered oracle string
against a Rust literal without checking whether the oracle's bytes were a *template*. Any oracle string
whose neighbours contain non-printable bytes is interpolated, and a port that renders the slot
differently is making a choice, not drifting. Before filing a copy-drift finding, confirm the oracle
stores a LITERAL — and grep the port for the same phrasing elsewhere: a wording used at ten sites with
tests behind it is a convention, not a bug.

## Implementation status — 2026-08-20

Branch `parity/cc-2.1.238-byte-alignment`, worktree `.worktrees/parity-cc-2.1.238`, off `main@89b4f106a`.

### Wave A — model-visible copy and schema (45 applied, 1 later reverted)

AGT-01/02/03/09/11/12/14 · BASH-01/02/04/05/08/13 · CLI-02/03/06 · FT-01/03/05/06/07 · PERM-01 ·
REM-01/02/03 · SC-03/04 · SLASH-01/02/05/08/09/10 · SP-1…SP-8 · ST-02/04/05

`ST-01` was applied and then **reverted** — see the correction section above. It is the one finding in
this register that was wrong *and* survived adversarial verification.

### Wave B — structural (24 applied, 6 partial, 4 deferred, 0 rejected)

Applied: TR-01/02/04/05 · FT-02/04/09 · AGT-04/05/06 · REM-04/07 · CLI-01/04/05/07/08/09 ·
BASH-06/09/11 · SC-01 · PERM-03/07
Partial: TR-03 (tool ported; the paginated `resources/directory/read` path stays unported behind a
default-off gate) · FT-08 · REM-05 (copy + machinery landed, producer deferred) · REM-06 (ported but
left default-OFF where the oracle defaults ON) · BASH-03 · SC-06 (taxonomy applied, notice copy deferred)
Deferred with evidence: AGT-07 (the web-fetch agent's gate is NOT inert without the `allow_web_fetch`
entitlement, so registering it would change behaviour rather than match it) · AGT-08 · SC-05 ·
PERM-09.

### Tool registry after this pass

Removed `MultiEdit` (port-extra). Added `ReportFindings` and `WaitForMcpServers`. `ListMcpResourcesTool`,
`ReadMcpResourceTool` and the new `ReadMcpResourceDirTool` moved off the static list onto the dynamic
MCP partition, registered only when a connected server declares `capabilities.resources` — which is the
oracle's behaviour, and a real reduction in advertised surface for MCP users whose servers do not.
Both tool-list snapshots regenerated; `ListAgents` landed in them at the same time, closing the
four-week-old red snapshot test (TR-10).

### Verification

`cargo check --workspace --all-targets` → 0 errors. `cargo test --workspace --no-fail-fast` → **4
failures, all pre-existing on `main` and none touched by this branch**:
`reminder_twin_wiring_test`, `streaming_error_propagation_test`, `parity_registry::fixture_names_match_production_constants`
(a frozen 42-name fixture vs 43 live tools — identical before and after this work), and
`tool_task::task_tools_use_the_host_config_home_when_injected`.

The first two were proven pre-existing by reverse-applying only the orchestrator/compaction patch and
re-running: both still failed. All four correspond to files that already carried uncommitted fixes in
the main checkout, i.e. someone is repairing them there.

### Deliberately NOT done

`platform_api::CLAUDE_CODE_VERSION` is still **2.1.220**. It drives the `AI_AGENT` value and the WebFetch
user agent, so bumping it announces 2.1.238 to servers and child processes. Roughly 74 of the 148
confirmed findings are still open, so the honest state is "implementing parts of 2.1.238 while
advertising 2.1.220". Bump it when the register closes — and note the port has been burned by the
opposite error before (advertising 2.1.217 while implementing 2.1.220), which is why the three
identifiers derive from one constant.

### Repo defect found on the way (unrelated to parity)

`local-apps/src/permissions.rs:19` does `include_str!` on
`templates/vite-react-static-v1/.lingxi/settings.local.json`, but that template's own `.gitignore:5`
excludes exactly that file. It exists only as an untracked file in a developer's checkout, so **a fresh
clone or worktree cannot compile `local-apps`**. Fix by committing the file, removing the ignore line,
or generating it in a build script.

## Round 2 — 2026-08-21 (commit `ceae53506`)

| id | outcome |
|---|---|
| `REM-06` | **Closed.** Reminder defaults ON, matching the oracle. Confirmed from a LIVE 2.1.238 session emitting `<total_tokens>14999028 tokens left</total_tokens>` against the 15,000,000 budget — not inferred from the GrowthBook fallback. Divergence test now asserts `PORT_DEFAULT_MODE == ORACLE_DEFAULT_MODE`. |
| `PERM-02` | **Partial, deliberately.** Rows 1 and 3 byte-exact vs `@302829842`. Row 2 left alone: upstream it is composed from the permission result's `suggestions`, and the port has that field with no engine source (`hook_payload.rs:639`, `executor.rs:2184` pass `None`). Writing a label there would be inventing text — the ST-01 mistake. |
| `SLASH-06` | **Closed.** `bug` is its own command with alias `share` (`@248164336`); the port had `bug` as an alias of `feedback` and `share` as a standalone "compiled stub". `name:"share"` has ZERO hits in 2.1.220 and 2.1.238 — the stub classification came from the stale de-minified source. |

### A note on where wrong models come from

Two of this round's three findings trace to the same root cause as `ST-01`: a
claim taken from the **de-minified source tree** rather than the shipped binary.
`share`-as-a-stub-command survived in two classification tables, a fixture, and
four counter locks. When a port fact cites `claude-code/src/...`, treat it as a
hypothesis and re-check it against the binary.

### Verification note — a false pass that nearly shipped

One full-suite run exited 101 having executed **zero** tests: the disk filled up
(`No space left on device`) during compilation. The failure-name parser in use at
the time reported "0 failing tests", which read as success. It was not.

The worktree's `target/` had reached **55 GB**, 31 GB of it `debug/incremental`.
Two consequences worth carrying forward:

* An isolated worktree cannot share the main checkout's build cache, so it costs
  a second full target tree. Budget for it, and `cargo clean` the worktree when
  the branch merges.
* **Never read a non-zero exit with zero parsed failures as a pass.** Cross-check
  the exit code against how many test binaries actually reported, and against the
  compiler's own `error:` lines.

## Waves C and D — 2026-08-21 (`a90892a6b`, `3e8ff9edc`)

Wave C ran nine implementers over the remaining 62 P2/P3 findings; three lost
their connection mid-run, but their work was already on disk and compiled. Wave D
picked up the nine findings they never reached.

**Register closed out at ~108 of 148.** The rest are not "unfinished" — they are
adjudicated, each naming the substrate it needs:

| bucket | count | meaning |
|---|---|---|
| closed | ~108 | landed and verified |
| partial | 8 | renderer/half landed, producer named |
| rejected | 5 | finding wrong, or duplicate, or would ship dead code |
| deferred | 15 | verified at the oracle; blocked on a named missing mechanism |

### The deferrals are evidence, not fatigue

`PERM-04` and `PERM-05` are the clearest case. The permission agent edited **zero
files** and instead established the load-bearing fact: the port has no per-call
permission-layer substrate at all. `PermissionPolicy` is constructed once at boot,
is not `Clone`, and lives behind a shared `Arc`; upstream's `gn(toolUseContext)`
fold over `permissionLayers` has no counterpart. Both findings sit on top of that
layer, so landing them would have produced definitions no caller can reach.

The same test applies to `SH-01/02/05/06/07` and to `BASH-10/18/19` from Wave C:
`check_permissions`, `coerce_input` and `user_facing_name_for_input` have **zero
production call sites**, so implementing them inside the tool crate would ship
dead code that reads as parity in a diff.

## Two defects found while proving the deferrals — NEITHER IS FIXED

These are worth more than the findings that produced them.

### 1. POSIX command hooks never run through a shell

The oracle spawns a command hook as
`spawn(M,[],{env,cwd,shell:He,detached,windowsHide:!0})` — the whole command
string, shell-interpreted, with an **empty argv**. The port does a bare exec:
`Command::new(&inner.command).args(&inner.args)` at
`platforms/posix/src/process/runner.rs:196`.

So `{"type":"command","command":"./fmt.sh --all"}` — or any hook containing a
pipe, redirect or `&&` — fails with ENOENT on LingXi. **Existing fixtures are only
parsed, never executed**, so nothing in the suite catches it. This is a bug in a
shipped feature, not a parity nicety, and it should land before any `shell:`
selector work (`SH-06`).

### 2. `frozen_command_denies` — a security guarantee that does not exist

Constructed at `tools/skill/src/skill.rs:302`, threaded through
`platform-api/src/subagent_spawn.rs:266` into the scoping sidecar at
`apps/engine-desktop/src/background_agent.rs:99` — and **read by nothing**. No
consumer applies it as a command-deny set on resume.

Its tests (`tools/skill/src/fork.rs:458-481`, `skill_test.rs:1570-1605`) assert
only that the spawn request and sidecar field are populated, so they pass while
the guarantee the field exists for is absent. This is the "named, computed, never
WIRED" pattern with a security consequence.

## Scoreboard for the pattern this audit kept hitting

- **6 times** a GREEN test was pinning the bug, including one asserting a field
  must be ABSENT that the oracle always emits (`SC-05`), and one asserting text
  ripgrep never prints (`ST-13`).
- **3 defects** traced to a port "fact" read off the de-minified `claude-code/src`
  tree rather than the shipped binary (`ST-01`, `SLASH-06`, `SC-05`).
- **4 "named, computed, never wired"** cases found (`SC-07`, `CLI-16`,
  `frozen_command_denies`, and the `ReportFindings` name in a test's expected list
  with no implementing tool).
