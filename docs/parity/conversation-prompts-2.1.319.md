# Conversation prompt baseline and bundle migration

## Pinned upstream artifact

This synchronization uses the official Claude Code **2.1.319** release. The
`latest` npm dist tag and official release were checked at implementation start
on 2026-11-09. All extraction, source evaluation and native request captures use
one darwin-arm64 executable, never a moving latest installation.

- Release: <https://github.com/anthropics/claude-code/releases/tag/v2.1.319>
- Artifact size: `321995200` bytes.
- SHA-256: `1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
- Extracted Bun entry chunk: binary bytes `269336381..274727937`.
- Main composer: `FCr` at `271783865`; ordered section builder `j_n` at
  `271786452`; body constructor `NCr` at `271778774`.

The raw executable and extracted JavaScript are local evidence, not repository
assets. Checked-in fixtures record offsets, hashes, explicitly stubbed host
facts and evaluated expected output. The extractor rejects a different artifact.

## Inventory and verification

| Surface | Source and checked-in evidence | Verification |
| --- | --- | --- |
| Main Code system body, communication, safety, tool workflow, identity and tail | `scripts/tests/fixtures/main_body_319_sources.json`; `prompt/templates/claude-code-2.1.319/` | Source extraction and VM evaluation of 46 fragments, 10 guidance cases, composer order and rewrite/style branches. |
| Complete assembled main body | `crates/orchestrator/tests/fixtures/prompt_session_2_1_319.json` | Six **full body** comparisons against requests emitted by the unmodified native executable: Sonnet 4.6, Opus 5 and Sonnet 5.5, memory off/on. No whitespace normalization. |
| Model families and capability gates | `model_capabilities_319_oracle.mjs`, core baked capability tests | All 22 baked models; Lean 1296 cases, rewrite 216 cases, feature 972 cases, system-message placement 2484 cases. |
| Fork foreground/background eligibility | `fork_subagent_319_oracle.mjs`, core fork tests | 576 combinations of model, explicit overrides and foreground/background eligibility. |
| Memory protocols, auto memory, host environment, metadata, Skills listing | `scripts/tests/fixtures/context_prompts_319.json` and `context_prompts_319_oracle.mjs` | Source-derived text and model/rewrite gates; production template files checked for existence and exact bytes. |
| General, Explore, Plan and web-reading child agents; consent, notes, background and Skill guidance | [Agent/reminder ledger](agent-reminders-2.1.319.md) | Extracted native builders, exact renderers and static text; 20 source ranges plus compiled production helpers. |
| Child prefix and environment/model/date placement | `scripts/tests/fixtures/agent_child_prefix_319.json` | Three real child request captures; actual child assembly and typed environment projection tests. |
| Silent-turn narration and reminders, token budgets, changed-file/deferred-tool/task notifications | [Agent/reminder ledger](agent-reminders-2.1.319.md) | Protobuf signature classifier, model and elapsed/count gates, UTF-16 fixtures and clock restoration tests. |
| File/search, web, Agent, Skill, Bash, NotebookEdit, Task/Cron, Monitor, stop/output tool descriptions | [Tool ledger](tool-prompts-2.1.319.md) | Source-derived tool bodies and selection gates; actual Tool implementations and wire serializer tests. |

The two stages have separate acceptance evidence. The six full bodies and three
child captures establish the synchronized baseline before bundle selection is
introduced. Those same fixtures remain regression locks after the refactor.
The native binary is driven through an isolated configuration directory and
loopback mock API; captures do not require a real account or paid model call.

## Explicit differences and normalization allowlist

- Preserve LingXi branding, `LINGXI.md`, configured data directories, help paths
  and actual registered product extensions. Replace only the documented brand
  fields; do not perform a blanket text rewrite or whitespace folding.
- Private native billing attestation and vendor SDK identity are not forged.
  Full-body comparison isolates the native system body from those separate
  provider blocks. UTF-16 source vectors and provider cache boundaries remain
  owned by the existing provider projector.
- Live environment comparisons substitute only captured cwd, OS version, date
  and corresponding model context values. The environment is a message
  attachment, not an invented suffix inside the system body.
- Main Code uses the synchronized Code body. Claude Chat retains its existing
  product role and uses applicable synchronized shared fragments. Chat does not
  acquire the native Code role.
- Native Lean and `temporal_hare` rewrite are independent gates. Rewrite is not
  inferred from Lean. Native system-message guidance uses the actual route
  capability and native hard exclusions. Host capability overrides therefore
  take precedence over a baked model default.
- Only real tools and host features may be advertised. Shell naming reflects
  the configured shell; LingXi background timeout and notification behavior,
  WebFetch response format, Skill reference guidance and retained TaskOutput
  are documented in the component ledgers. Unsupported upstream transport
  behavior is not advertised merely because its text exists in the binary.
- Replace the complete native product-availability bullet (terminal, desktop,
  `claude.ai/code`, VS Code and JetBrains) with exactly: "LingXi runs in the
  current host client. Use the interfaces and capabilities explicitly exposed
  in this session." Upstream oracle bodies retain the original line; context
  and complete-body comparisons apply this one explicit host-capability
  substitution. No Claude web or IDE surface is implied for LingXi.
- The memory reader accepts native `name` and `metadata.type`/`type` frontmatter
  as well as existing `memory_type` files; existing legacy fields keep priority.
- New LingXi prompt snapshots retain optional typed `appendSystemPrompt` source
  independently of the rendered body. Bundle invalidation and cold resume keep
  its exact UTF-16 units, while explicit current append configuration wins.
  This is a host metadata extension, not a claim that native snapshots contain
  that field. Legacy snapshots with only opaque `systemPrompt` members cannot
  recover startup-only instructions whose provenance was never persisted.
  Their original transcript remains intact and current user/project/plugin
  sources are reloaded; no body-tail heuristic classifies old text as an append.

## Bundle architecture acceptance

The design and extension points are documented in
[Prompt bundles](../architecture/prompt-bundles.md). Checked-in coverage includes:

- All six selector priorities, mode defaults, registration errors and unknown
  model fallback; all baked Claude channel IDs in Chat and Code.
- Actual SDK route identity, including Kimi over Anthropic Messages, OpenRouter,
  Foundry underlying model IDs and custom cloud-provider identities.
- Immutable request policy shared by system, tools, memory and reminders;
  per-request resolution reuse, profile separation and concurrent request state.
- Preparation-time model rewriting and fallback, child/teammate selection,
  cross-target fork rebuilding, custom prompts and exact UTF-16 preservation.
- Bundle/target/version metadata in prompt snapshots and tool-cache keys;
  legacy, malformed or changed policy metadata causes prompt reconstruction
  without discarding conversation history or current additional instructions.
- Generic Chat/Code Lean templates and actual tool prompts, portable memory,
  removal of Claude-only identity/attribution/subscription assumptions, and
  complete component overrides reaching the real request pipeline.

Compaction, title generation, suggestions and other dedicated tasks keep their
independent prompts. No client setting or provider account integration is added.

## Reproduction

Run source checks with the exact pinned executable:

```sh
node scripts/tests/main_body_319_oracle.mjs /absolute/path/to/2.1.319 --check
node scripts/tests/model_capabilities_319_oracle.mjs /absolute/path/to/2.1.319 --check
node scripts/tests/context_prompts_319_oracle.mjs /absolute/path/to/2.1.319 --check
node scripts/tests/agent_reminders_319_oracle.mjs /absolute/path/to/2.1.319 --check
node scripts/tests/tool_prompts_319_oracle.mjs /absolute/path/to/2.1.319 --check
node scripts/tests/bash_prompt_319_oracle.mjs /absolute/path/to/2.1.319 --check
node scripts/tests/fork_subagent_319_oracle.mjs /absolute/path/to/2.1.319 --check
python3 scripts/tests/tool_prompts_319_check.py
python3 scripts/tests/agent_reminders_319_byte_check.py
```

`prompt_session_319_live_oracle.py` captures the main/child native requests.
`prompt_session_319_fixture.py /path/to/captures --check` and
`agent_child_prefix_319_oracle.py /path/to/captures/live-native-child --check`
validate their checked-in expectations.
`prompt_session_319_standalone.py` compiles the production assembler functions
and records their source digests, then compares all six complete bodies and
bundle renderer cases without requiring the entire workspace build.
It honors `CARGO_TARGET_DIR` when reusing compiled dependencies.

## Final acceptance, 2026-11-09

The unified `prompt` test filter passed across 13 packages: **651 passed,
0 failed**. This includes 437 orchestrator tests and the real batched/streaming
Mod model-rewrite paths. Native complete-body tests apply the single documented
host-platform line substitution; the six upstream-derived body hashes and raw
captures remain unchanged.

```sh
CARGO_INCREMENTAL=0 cargo test -p orchestrator -p agent -p core -p llm-runtime -p memory -p tool-api -p tool-file -p tool-shell -p tool-web -p tool-task -p tool-skill -p tool-agent -p tasks --lib prompt --locked --offline -j 2 --config profile.test.package.orchestrator.debug=0 -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p tool-file --test tool_prompts_319 --locked --offline -j 2 -- --test-threads=1
CARGO_INCREMENTAL=0 cargo check -p harness-runtime --features desktop,mobile --locked --offline -j 2
```

- The file-tool integration test passed its native standard/Lean/rewrite and
  generic Chat/Code/provider combinations using the real `Tool::prompt` methods.
- Additional targeted suites passed: model-call preparation (5), profile/session
  recovery (10), Mod physical requests (4), silent-turn parsing/timing/restoration
  (18), child streamed model rewriting (1), concurrent tool serialization (1),
  nested tool-policy replacement (1), memory frontmatter compatibility (7), and
  the core artifact gate checks (8). Counts overlap the unified suite.
- All seven fixed-artifact source oracle checks passed. The final production
  assembler replay passed **6/6 complete bodies**; all **3 child captures**,
  compiled background/Skill helpers and production tool-template byte checks
  passed. The final platform substitution changes each main body by exactly
  28 bytes and is checked as one exact whole-line match.
- Desktop and Mobile feature composition compiled successfully on the macOS
  host. This is a compile check, not device or paid-provider acceptance.
- Tests use the repository's existing `.cargo/config.toml` 16 MiB test-thread
  stack setting. Direct binary runs carry the same setting; it was not raised
  or changed for this work. Runtime entry/scope futures additionally use the
  existing factory-based heap boundary to limit large async temporaries.

The workspace evidence directory is
`artifacts/prompt-routing-20261109/work/` in the parent LingXi workspace.
`final-validation.json`, `final-cargo-prompts.log`,
`final-orchestrator-lifecycle.json`, `final-extra-lifecycle.json`,
`final-file-tool-integration.log` and `final-runtime-check.log` retain command
results. `assembler-host-availability-final/{result,source-manifest}.json`
records the final body replay and exact normalization.

## Teammate and fallback regressions

Generic teammates render through the host's target-specific system renderer on
both initial construction and a model change. Output style, portable memory and
Mod-provided sections survive alongside the teammate role and exact custom
UTF-16 instructions. Portable child consent and notes are appended once; the
native teammate renderer keeps its synchronized bytes. Regression tests cover
GPT and Kimi construction and a Claude-to-generic transition through the saved
child prompt state.

Each prepared step also retains the original engine-reminder source before
bundle and Mod rendering. A fallback to a different policy renders those
sources again with the new tool and model gates, removes superseded current-step
copies from the request, and preserves message identity. It does not drain
durable notifications or advance reminder cadence again. Same-policy retries
reuse the rendered suffix. Regression tests cover unavailable Task tools,
durable-notification delivery and complete reminder-template replacement.

Delivery verification uses an isolated worktree based on `968fd178`, retaining
the existing `f16f3d5d` SDK pin. The unified 651-test suite, 72 distinct targeted
teammate/fallback/reminder tests, seven fixed-artifact source oracles, six full
native bodies, three child captures and Desktop/Mobile feature compilation all
passed in that worktree. Five integration byte-lock tests retain ten model cases,
four output styles and source-vector projection checks, and verify environment,
model identity and project instructions in an actual mock request. Their body
locks were updated for the synchronized text and typed-ingress placement;
explicit styles retain proactive guidance as required by 2.1.319.
The publication evidence is in the parent workspace's
`artifacts/prompts-319-publication-20261109/work/` directory.
