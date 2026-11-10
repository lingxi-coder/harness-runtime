# Claude Code 2.1.319 tool prompt baseline

The reference is the official Darwin arm64 2.1.319 executable, SHA-256
`1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
`scripts/tests/tool_prompts_319_oracle.mjs` rejects any other executable before
evaluating the selected native JavaScript text builders in an isolated VM.
The fixture records the original byte ranges and their digests. It does not
execute the application, load credentials, or send a model request.
Static bodies and format templates for the sixteen verified descriptions,
their two file-tool rewrites, and the pure Task constants live under each
tool crate's `src/templates/claude-code-2.1.319/` directory. `include_str!`
preserves their exact leading/trailing bytes; dynamic renderers retain their
existing branches and interpolation values.

## Coverage

| Surface | Upstream producer | Production verification |
| --- | --- | --- |
| Glob, Read, Grep | `oks`, `E2i`, `iks` | Both default-style long/lean templates; Read/Grep temporal-hare rewrites |
| Write, Edit | `VDi`, `Cjs` | Both long/lean templates |
| TodoWrite | `mr` / `Fi` / `Bi` | Both long/lean templates |
| TaskCreate/Get/Update/List | `En`, `xn`, `zn`, `$n` | Six ordinary/Swarm templates; unchanged from the native descriptions |
| NotebookEdit, TaskStop | `Qso`, `jDi` | Complete templates; TaskStop no longer suggests the obsolete `name@team` spelling |
| TaskOutput | Native settings-schema deprecation | Removed upstream in 2.1.319; LingXi retains its implemented legacy tool as a product extension, without a template-equivalence claim |
| Monitor | `dms`, `cms`, `lms` | Four ordinary persistent-capability variants covering background disabled/enabled and push guidance absent/present; complete default text is unchanged |
| WebSearch | `oWi` | Both templates with a controlled month and absent citation override |
| WebFetch | `aBi` / `g` | Both templates, no artifact integration, default 900000 ms cache TTL |
| Agent | `rvi` / `$on` | Four native long/lean and fork/non-fork fixtures; delegation guidance checked from production |
| Bash | `Krn`, `LJo`, `DJo`, `Des`, `Vrn` | Twelve complete native variants; four complete production Git variants and complete long/lean descriptions have Rust assertions |
| CronCreate/Delete/List | `ems`, `nms`, `oms` | Six durable/session-only variants with explicit capability adaptations |
| Shared selector | `_t`, `ce`, `b0`, `Rw` | 216 cases across the native baked model catalog, unknown Claude models, capability overrides, and simple-prompt overrides |

Run the deterministic checks with:

```sh
node scripts/tests/tool_prompts_319_oracle.mjs /path/to/2.1.319 --check
python3 scripts/tests/tool_prompts_319_check.py
node scripts/tests/bash_prompt_319_oracle.mjs /path/to/2.1.319 --check
```

The Rust integration test `tool-file --test tool_prompts_319` exercises the
actual five file tools and both rewrite branches. Agent's
`build_prompt_matches_native_319_artifact` checks complete assembled descriptions;
the tool-api selector and Bash cwd tests use the same independent fixtures.
Those Rust tests are separate from the source-template comparison.
The complete Bash oracle additionally matched six unmodified native API request
captures. `--capture-root` can repeat that comparison without changing the
fixture. Complete production Bash comparisons normalize only the stated host
capability differences below.

## Request-scoped bundle integration

`PromptOptions` and `ToolUseOptions` carry the request's shared resolved policy.
The shared tool gate uses its canonical target rather than any stale legacy
model field. Generic bundles select the synchronized Lean bodies, suppress
Claude-only rewrites and the Claude subscription-tier restriction, and omit
Bash's default Claude attribution. Explicit user attribution remains intact.
Standalone consumers without a policy use the builtin model resolver; unknown
models receive generic Lean. Complete per-tool template replacements are
applied once in the common wire serializer, preserving source UTF-16 units
unchanged when no replacement exists.

Runtime tests cover conflicting legacy/resolved identities, forced rewrite
capabilities on generic routes, concurrent complete tool overrides, and nested
dispatch replacing the policy carried by an older tool-context snapshot.

## Explicit host adaptations

- Read describes image presentation to the selected model rather than assigning
  Claude identity to every model. Its new targeted-range guidance matches the
  native 2.1.319 producer.
- Grep names the registered shell, and its rewritten description uses LingXi's
  product name. The output formats and matching instructions are unchanged.
- WebSearch geographic availability and supported options belong to the actual
  selected hosted-search provider. The native US-only claims are not copied
  into multi-provider tools.
- WebFetch recommends an authenticated GitHub integration or an available shell
  CLI. Its hostname warning matches the existing URL validator. Cache wording
  describes the implemented fixed 15-minute TTL; no configurable cache TTL is
  introduced by the prompt synchronization.
- Agent uses `.lingxi/agents` and retains the existing Fusion catalog notice.
  Fixtures use an ordinary general-purpose catalog and the host's existing
  background/fork support. The 2.1.319 delegation-cost guidance is shared across
  steer variants; the long description still gates proactive-use guidance.
  The native non-fork fixture keeps the fork feature enabled while withholding
  fork availability/background. Its additional explicit-background-only
  paragraph is omitted for LingXi's feature-disabled control, where ordinary
  agents continue to run in the background by default; all other bytes remain
  compared against that fixed fixture.
- Bash now describes cwd carry-over only after successful main-session commands
  inside allowed working directories, or explicitly says every call starts in
  the start folder when project-directory maintenance is enabled. Its timeout
  wording distinguishes foreground and background limits. The upstream macOS
  Bash 3.2 advisory is not advertised for LingXi's default `/bin/zsh` execution.
  LingXi's existing background default remains 30 minutes; the upstream
  controlled fixture uses 10 minutes. Background completion notifications remain
  available even when Monitor is unavailable. Existing configured/inline
  attribution is preserved; native deferred-attribution fixtures remain separate.
  Both Git variants include the 2.1.319 GitHub notification/mention guidance.
- Cron descriptions retain the configured branded project store and advertise
  Monitor only when enabled. CronCreate now describes the fixed per-job offset.
  The upstream sentence promising actual next-run times in create/list results
  is omitted because the current output schemas do not expose that value.

## Remaining independent checks

The ordinary Bash oracle does not establish sandbox, macOS Bash 3.2, Windows,
REPL/embedded-search, or remote-machine behavior. Existing component tests for
those surfaces remain independent evidence. Agent/Bash temporal-hare rewrites
and optional remote branches are not covered by the default-runtime fixtures.
The fixture's controlled host capabilities are explicit; adding one of these
capabilities requires its own native fixture and production comparison.
