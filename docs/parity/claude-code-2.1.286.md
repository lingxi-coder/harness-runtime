# Claude Code 2.1.286 byte-level alignment

The latest target advanced to [2.1.287](claude-code-2.1.287.md) during the
2026-10-01 validation run. This document retains the preceding scoped evidence;
it no longer identifies the latest release.

Target rechecked on 2026-10-01. This is an active work register, not a declaration
of complete parity. `core::host::CLAUDE_CODE_VERSION` remains `2.1.267` until the
shared runtime contracts have been audited and verified.

## Reference and scope

- Official release: https://github.com/anthropics/claude-code/releases/tag/v2.1.286
- Published: `2026-09-30T19:10:13Z` (12:10:13 America/Los_Angeles).
- Version-pinned changelog:
  https://raw.githubusercontent.com/anthropics/claude-code/v2.1.286/CHANGELOG.md
- Changelog SHA-256:
  `da05018aedf08212a86cb8c26aa88fa9676fbc3bda86ece9687c4c222bdcf6cc`.
- Installed darwin-arm64 native binary: 225,167,728 bytes, SHA-256
  `75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
- Extracted reference: 2,140 plain JavaScript chunks. Local working evidence is
  `/tmp/claude-code-oracle-2.1.286`, with capture metadata and a complete
  270-to-286 release-note inventory. These temporary files are not CI inputs.

The user's explicit exclusions are LingXi branding, multiple LLM providers,
Fusion and Local Apps. The latest contract is the sole compatibility target:
the user explicitly requires no older-version compatibility. Do not retain old
Claude Code behavior, SDK aliases, private transcript layouts or migration
adapters solely to keep earlier versions working. Historical version-pinned
fixtures are evidence of an earlier audit, not an additional runtime target.

Historical product divergences remain registered in
`crates/test-harness/src/parity/fixtures/accepted_divergences.json`; an oracle
gate changing upstream can invalidate an oracle-derived exemption. That is why
the SubagentHandback exemption was retired: the latest default is enabled.
The production capability is now implemented and has separate native-helper,
wire and Rust execution evidence below. Retiring the exemption alone was not
evidence of that implementation.

Release notes contain 1,249 bullets across 15 published versions after 2.1.270.
They include UI, hosted products and duplicate/overlapping fixes. They are a
discovery inventory, not a count of runtime defects. Their classification and
the older backlog remain unfinished. Terminal rendering, remote service
entitlements and provider model behavior cannot be established by runtime unit
tests.

## Evidence standards

1. Compare observable UTF-8 bytes, JSON field presence/types, schema constraints,
   prompt assembly and state transitions using pinned reference outputs.
2. Normalize only explicitly identified branding and nondeterministic fields.
   Do not hide semantic differences by sorting messages or deleting fields.
3. Exercise the production runtime with differential or behavioral regression
   tests. Local self-hashes and source-string anchors remain useful regression
   checks, but do not independently prove upstream compatibility.
4. Record implementation, execution coverage and unresolved work separately.
   No version bump follows merely from updating fixtures.

## Active implementation lanes

| Lane | Confirmed issue / contract | Verification location |
| --- | --- | --- |
| Session recovery | Recover parallel tool-call tails by call identity and persisted order, including crash-truncated sessions | `crates/session/tests/load_session_test.rs` |
| Resume content | Coerce object/number/bool tool-result content at the model conversion boundary without changing raw JSONL | `crates/orchestrator/tests/resume_test.rs` |
| Diagnostic redaction | Invisible key characters, URL credentials and error-text token boundaries | `crates/llm-runtime/tests/redaction_test.rs` |
| npm plugins | Refuse folder/git source specs; fetch registry tarballs and sanitize dependency lockfiles | `crates/configuration-admin/src/plugin_npm.rs` |
| Monitor and Bash | Bounded Monitor default and background command deadlines including handoff | tool, task and process-runner regression tests |
| Prompt cache | Connect attribution and expected rebuilds to actual requests, streams and compaction; use actual cache TTL | cost and orchestrator regression tests |
| Model retries | Share retry state across streaming, non-streaming and fallback paths | llm-runtime, orchestrator and agent regression tests |
| Manual compact | One complete exchange can take the current summarize-all fallback | compaction tests and isolated live oracle |
| Subagent Handback | Child-only auto-mode tool, classifier-only review, durable nested/main admission, turn-end enforcement and scoped hot/cold recovery | `subagent-handback-2.1.286.md` and production Agent/Tasks/orchestrator/Session tests |
| Gv instruction cache, scoped regressions passed | Shared root/session context and independent eager-file caches, sticky rejection, explicit reasons, atomic compaction/clear and child Full/ManagedOnly projections; host refresh/acquisition gaps remain | [instruction-cache-2.1.286.md](instruction-cache-2.1.286.md), native oracle 20 scenarios/300 operations; Core cache 5 in Core full 743; Orchestrator cache 16 in full 1,276/1,276; Agent 595/595 |
| MCP ordinary input_required | Default-on auto-fulfillment through the actual local handler registry, retry state, progress/cancellation and concurrent HTTP SSE readers; remote task gate remains default false | `mcp-input-required-2.1.286.md` and actual MCP/HTTP/stdio regressions |

The lane table tracks scope, not test success. Validation results are recorded
below after execution.

## Known outstanding audit areas

- Complete SubagentHandback native-session differential acceptance, latest-only
  host regression validation and real classifier/persisted-output evidence;
  the production tool, enforcement and delivery dispositions are implemented.
- Extend the bounded native Handback evidence beyond the accepted Unicode 17
  sanitizer scope: 748 native-source cases, fresh affected full suites and eight
  actual native Agent-report captures do not prove a full classified Handback
  session or the entire provider protocol.
- Per-command `allowed_domains` and permission-scoped sandbox network grants.
- Finish the instruction announcement lifecycle and newer skill/prompt audit;
  implemented AGENTS.md/omission contracts do not establish the entire surface.
- Extend the validated Gv cache and Agent initial Full-load failure scope to
  native host acceptance. Wire PolicyRefresh, SettingsSync, trusted
  HooksInvalidate and memory controls; finish native health/acquisition error
  boundaries. Fresh conditional-only discovery is implemented and its focused
  regressions passed within the latest full run; the remaining host acquisition
  capabilities are not completed by account pT wiring.
- Remaining MCP schema-error/number/URL byte acceptance, agent-scoped
  instructions and client-side Chrome instruction augmentation. Modern remote
  task handling remains behind the native default-false task gate; do not treat
  task payloads as unconditionally enabled.
- Bare-mode reminder/background/MCP boundaries.
- Latest output/token limits and alias/fallback context-window behavior.
- AWS credential masking and SigV4 re-signing proxy (`sandbox/credentials.rs`).
- Revalidate the earlier audit's unavailable remote auto-mode policy bodies
  against this reference. The existing bundled policy is not yet independently
  byte-verified against 2.1.286.
- Remaining 270-to-286 release-note inventory and all prior unresolved items.

### MCP negotiation and instruction wave

The pinned native transport uses `initialize` at `2025-11-25` for its fallback
handshake. This is a branch of the latest contract, not an older Claude Code
compatibility adapter. The `2026-07-28` mechanism is
`server/discover`: select from `supportedVersions`, adopt capabilities and
metadata directly, and do not send `initialize` / `initialized` afterward.
The common/POSIX transports now select `supportedVersions`, adopt discovery
capabilities and metadata, and retain the HTTP session across probe/fallback.
HTTP negotiation now uses the latest default-on gate. Named probe IDs do not
consume ordinary request IDs; corrective probes are bounded by the original
connection deadline. Fallback replies validate their selected version before
`initialized` is sent. The live client adopts the transport's handshake state
instead of repeating initialize. Windows delegates remote metadata as well.
Upstream stdio negotiation still defaults off and uses a disposable sibling.
Main evidence: `src_209701831.js` Wc/So/al, `_connectNegotiated`, and
`_legacyHandshake`; host code is `platforms/common/src/mcp_remote.rs`,
`platforms/posix/src/mcp.rs`, and `mcp/src/protocol_negotiation.rs`.

Fresh discovery-cache connection failures now strike the persisted entry;
stale-background failures also retire cached catalog rows. At the strike
threshold (default one), the persisted entry is removed. Generation, config and
grant checks guard settlement so shared failure owners strike only once.
Authentication failures adopt `NeedsAuth` and purge rejected credential catalogs.
Current paths are `mcp/src/registry.rs` failure settlement and
`mcp/src/registry/discovery_cache.rs`. Latest evidence: `src_210493918.js`
Ma/wo, `src_202066919.js` cached-failure handler and `src_193120212.js` WRt.
Already-connected enable idempotence is present and should be retained.

Server-provided MCP instructions now use the reference's durable delta
attachments, with exact renderer bytes and transcript-derived announcement
baselines. Live/cold/hot resume retains original payloads by message identity;
disconnection adds a correction instead of deleting older model context. The
current-step reminder suffix is reused in the same order on streaming fallback
and context-overflow retries. UTF-16 truncation and the positive configured
description limit match the reference. Remaining MCP gaps include the default-on
ordinary `input_required` acceptance beyond the implemented driver, and
client-side Chrome instruction augmentation. Modern remote `task` handling is conditional on its native
default-false feature gate. Cached `serverInfo` is now carried through real write-through and
cache-hit paths, retaining the native name/version projection while omitting
server instructions and discovery metadata. The builtin computer-use
instruction block is implemented and covered by a pinned native text fixture
and a production prepared-request capture with a server that supplies no
instructions of its own.

Instruction selection implements the builtin's four AGENTS.md modes and its
trusted `pluginConfigs` option tiers. The product field `omitInstructions` and its `instructions` context block use
captured native `Fl`/`Lv` cases. Native fixtures retain their original
`omitClaudeMd` / `claudeMd` names; replay tests project those keys into the
current product namespace without changing the captured fixture bytes. The latest root-provider migration makes ordinary,
omit and cold-resumed children project the same current root Gv instead of
performing separate raw eager loads; cold resume still starts a fresh lazy Read
cursor, and explicit fork user-context overrides retain their bypass.
Production hosts bind the shared root provider, including mobile path mapping;
an empty injected memory provider stays empty. Current cache and Agent failure
regressions passed the full runs recorded below. The required current SDK
capabilities are documented in
[`current-only-sdk-contracts.md`](current-only-sdk-contracts.md).
Successful Read context is a durable `tool.call` additional-context attachment
for AGENTS.md. Explicit offset/limit inputs control self-read cursor behavior.
Full history and system context are forwarded into explicit nested forks.

A later source audit identified the 2.1.286 default announced-context lane:
`Or`/`bl` in `src_191662485.js` yield structured `instructions`, `date`,
`session_context` and `context_sections` attachments; only the inline lane uses
the raw additional-context prefix. The synthetic `instruction_context` System
row was removed because that private state is absent from native transcripts.
The corresponding normal-path implementation and actual request/cold-resume
regressions now pass. Existing agent JSONL headers and host message
projections are not independently byte-verified by these attachment checks.

The total-token reminder now persists its non-ephemeral attachment. An
independent reminder usage slot preserves cost accounting while reflecting
zero-usage compacted model history. Padded-countdown rollover is limited to the
reference's auto/PTL compaction and clear paths; the root-scoped ledger survives
hot resume. These integration repairs are under final validation below.

### Other concrete follow-ups

- PowerShell currently ignores `run_in_background`. Native Windows deadline
  support alone does not establish PowerShell end-to-end behavior.
- MCP-hosted shell deadline exemption (`backgroundDeadlineDisabled`) needs a
  composition-path audit; the ordinary-session timer default is not enough.
- A stalled output sink can delay process termination because drain awaits an
  accepted output write before polling its deadline again. Preserve each chunk
  exactly once while separating signaling from output I/O before claiming a
  hard wall-clock lifetime.
- Cache Usage currently lacks a typed presence flag for TTL breakdown. Distinct
  absent-breakdown and explicit all-5m responses require SDK-level evidence.
- Finish request, fork and restoration checks for AGENTS.md and `omitClaudeMd`,
  including child hook attachment fidelity and background fork serialization.
- Latest custom verify-skill commit guidance is in `src_183727495.js` `_It`
  (not just the lazy verify skill body); inspect Lqe/Rft custom-skill selection
  and the Bash prompt injection point before replacing text.
- Repeat cache and lifecycle checks after the durable `total_tokens` repair.
  Earlier wire captures proved the old outgoing-only reminder rewrote prior
  messages; the updated regression requires those prior bytes to be retained.

Two previous blockers are already implemented: Bash edit diffs are wired into
desktop composition, and live rewind summarization exists in the orchestrator.
Their old absence comments were corrected; product acceptance is separate.

## Reproducing targeted checks

```sh
python3 scripts/tests/extract_oracle_286.py /path/to/2.1.286/claude /tmp/new-oracle-chunks
cargo test --locked -p llm-runtime --test redaction_test
cargo test --locked -p configuration-admin --lib plugin_npm::tests
cargo test --locked -p session --test load_session_test
cargo test --locked -p orchestrator --test resume_test
cargo test --locked -p orchestrator --test force_compact_real
cargo test --locked -p test-harness --test 'parity_*'
python3 scripts/tests/test_prompt_drift_scan.py
python3 scripts/checks/prompt_drift_scan.py /path/to/2.1.286/chunks
python3 scripts/tests/compact_live_oracle.py /path/to/2.1.286/claude \
  --expected-version 2.1.286 --output /tmp/new-compact-capture --scenario all
```

The live oracle uses an isolated configuration, fake API key and loopback mock.
It needs local socket permission, runs no paid model request, and records raw
request bytes plus SDK events. `--scenario all` now actually includes all six
scenarios. The older 2.1.261 expectations are historical oracle fixtures; they
do not authorize an older-version runtime compatibility path.
The drift scanner fails when its source patterns match no files or no
comparable literals; a MISS is still only a candidate requiring investigation.
The extraction tool verifies the native SHA before writing a new directory;
`--check` compares all 2,140 existing chunks without changing them.

## Executed validation

- Prompt drift scanner regressions: 3 passed. Its repaired default path checks
  33 literals against the latest reference and reports 3 candidates. This is
  deliberately not a claim of complete prompt coverage.
- Latest live compact capture: all 6 scenarios passed against the pinned binary
  and loopback mock (`/tmp/harness-286-compact-live-v5/report.json`). The short
  exchange produces a summarize-all request using string content; both the
  request classifier and version-specific assertions now cover that form.
- Request classifier regressions: 2 passed. Both Python suites are included in
  the automatically discovered `check-parity-oracle-tools.sh` repository gate.
- Redaction integration tests: 11 passed, including 22 exact upstream error-text
  fixture cases. Stronger masking for known upstream leaks is explicitly
  distinguished in the security-strengthening regression.
- Plugin registry/archive/lockfile regressions: 11 passed, including fixes for
  valid partial hyphen ranges and `./` bin paths caught by independent review.
- Session: 44 tests passed across loader, recovery and gap suites (the recovery
  driver executes 20 pinned upstream scenarios). Resume: 35 passed. Live
  non-text tool-result tests: 5 passed.
- Existing parity drivers: 54 executables, 208 tests passed, zero ignored. These
  retain the evidence boundaries described above.
- Repository gates: all 10 passed, including mobile/UniFFI contract digests and
  Local App profile/workflow checks.
- Gate-trigger verification passed: all 10 discovered gates execute through
  CI -> check-all -> gate, including the newly added oracle-tooling checks.
- Final retry checks: 5 scope tests, 29 watchdog tests and 17 hosted execution
  tests passed. Cache attribution: all 8 orchestrator tests passed, plus 4
  request-snapshot and 13 ledger tests from the focused run.
- Final compaction: 275 unit tests and 2 existing prompt-oracle tests passed;
  the new differential driver covers 20 independently generated fallback
  traces. All 17 live-orchestrator compaction tests passed, including cancel
  after overflow. The hash-pinned fallback generator passed `--check`.
- Native POSIX background tests: all 5 passed after repairing process-group
  ownership on foreground handoff. They cover explicit and automatic handoff,
  leader-before-descendant exit, Stop attribution and complete UTF-8 output.
  Core/task state and receipt checks: 3 passed. Bash timeout/schema fixture:
  28 oracle cases passed its Rust driver; Monitor tests: 16 passed.
- `platform-windows --all-targets` cross-check passed for
  `x86_64-pc-windows-gnu`. Windows native tests compiled but were not executed
  on this macOS host; pre-existing warnings remain in untouched Windows code.
- Formatting and diff checks passed. Clippy passed with `-D warnings` for the
  changed libraries in configuration-admin, llm-runtime, compaction, session,
  orchestrator, tool-shell, tool-task, tasks and platform-posix. This is scoped
  validation, not the entire workspace/platform acceptance matrix.

The results above belong to the first implementation wave. They are retained
as historical evidence, not blanket validation of subsequent edits.

### Subsequent wave execution

- MCP registry/client/cache unit suite: 660 passed. Local OAuth callback socket
  tests were rerun with task-scoped socket access; the earlier sandbox-denied
  run is superseded. POSIX negotiation integration suite: 13 passed.
- Main AGENTS Read integration: 7 passed; MCP instruction request/fixture
  checks: 5 passed; durable model reminders: 3 passed; total-token lifecycle:
  5 passed; background snapshot/fork paths: 8 passed; streaming retry: 3 passed;
  resume: 35 passed; cancellation: 5 passed. These precede the final announced
  instruction change and must be rechecked where affected.
- Precommit guidance executed five focused tests, including a 29-case native
  prompt/latch fixture and the six-case Skill allowlist fixture. The desktop
  runtime compiled; runtime desktop/mobile integration tests are pending.
- A subsequent cache command exited successfully but ran zero tests. It is
  excluded from validation counts and scheduled for a corrected filter.
- Eight non-Cargo static checks passed. The Phase 2 contract/workflow gate
  timed out after its static prefix; it remains unverified for this wave.
- The required CLI background snapshot adapter is saved as a reviewable
  migration patch (`cli-bg-session-snapshot.patch`). The active app retains its
  canonical runtime Git pin; paired CLI compilation remains pending a new
  runtime commit and pin migration.

### Final normal-context wave execution

- Full libraries: core 733, Agent 559, MCP 661 and orchestrator 1,238 tests
  passed. Zero tests were ignored. The full orchestrator run executes the cache
  tests that the earlier zero-match command failed to select.
- Recovery integration: 38 passed; live orchestrator compaction: 17 passed;
  the sidequery zero-projection regression: 1 passed.
- Actual runtime wiring: desktop precommit 4, parked-agent restore 9, desktop
  Skill grants 1 and mobile/UniFFI precommit 2 passed. Cached MCP tools: 20
  passed; plugin materialization: 17 passed. Mobile uses fake host/platform
  services; these results are not physical-device acceptance.
- Native instruction helper evidence now contains 112 samples: 21 routing,
  33 renderer, 15 sanitizer, 3 JavaScript trim, 26 snapshot and 14 git-snapshot
  cases. The main request regression exercises three of the 14 git cases;
  extraction success alone does not establish runtime coverage of the others.
- A full-suite run found a real second-turn deadlock: a prompt-snapshot mutex
  temporary survived an awaited routing read that acquired the same mutex.
  The clone now drops its guard before the await, and a bounded two-turn
  request regression passes. Older tests now assert the exact native durable
  attachments and still check hook/terminal control behavior.
- Prepared-request cache tests keep prior text bytes exact while asserting the
  SDK's checkpoint relocation explicitly. Only the request-local
  `cache_control` marker moves; no broad normalization hides message rewrites.
- This wave saved the 56-scenario SubagentHandback discovery fixture with
  explicit queue, classifier and persistence mocks. Production implementation
  and its additional verification are recorded in the Handback wave below;
  these historical counts do not include that later implementation.
- Final checks for this tree are recorded in the next section; the earlier
  wave's results do not substitute for these executions.

### Child termination and cold snapshot writer wave

- The core invoker now carries trusted successful tool termination and its
  source. Registry dispatch preserves it, and the child settles the whole
  issued batch before ending. Failed/untrusted signals do not end the query;
  the last accepted marker wins. Schema final failure and existing owned-work
  parking/persistent wakeup remain intact. Full Tool API: 250 passed; full
  Agent: 566 passed, including seven new runtime drivers.
- Hash-pinned native termination evidence contains 20 MCP and 58 raw-frame
  cases. The Rust helper consumes 48 observations; 30 raw-frame cases remain
  outside the single-result boolean contract. The native terminal-hook and
  analytics attribution limits are explicit in `tool-turn-end-2.1.286.md`.
- Diagnostic whitespace fixes cover NEXT LINE, Basic/FEFF and URL token
  delimiters. Redaction integration: 13 passed, consuming 301 whitespace
  expectations (290 exact native outputs and 11 explicitly stronger masks).
  Native generator checks passed. Those 11 differences are not byte equality.
- A real writer omission dropped `contextRendering` despite the in-memory
  snapshot retaining it. The writer now emits the lowercase field only when
  present. Two new regressions exercise real writes, inline tool updates,
  disk cold resume and actual subsequent requests; absent legacy hints stay
  absent. Full orchestrator after the fix: 1,240 passed.
- All 54 compatibility drivers were rerun after the writer fix: 208 passed,
  zero ignored. Old request/JSONL assertions now verify the native announcement
  payloads, order, no inner attachment projection, durable IDs and complete
  parent chains. These are composed native-helper expectations, not a captured
  whole native JSONL session. Runtime session ID namespaces remain retained.
- The repository's ten gates passed, including the Phase 2 mobile/UniFFI
  contract and workflow executables. The independent trigger regression also
  passed. The initial brand-gate failure was repaired in a fixture and its
  reruns supersede the failed result.
- Windows GNU all-targets compilation passed after the invoker/whitespace
  changes, with existing warnings. This is compilation, not Windows execution.
  Desktop and mobile/UniFFI library Clippy passed with `-D warnings`; the final
  14-library run passed after the snapshot writer fix. Formatting and diff
  checks passed. Raw results are summarized in
  `/tmp/harness-286-final-validation.json`.
- Six production invokers/wrappers were checked for detailed result propagation.
  No additional wrapper drops the control. The app still uses its canonical
  older Git pin, so sibling source edits do not imply a deployed app update.

### Subagent Handback production wave

The runtime now injects the supplied child-only `SubagentHandback` tool under
the latest explicit auto-mode gates. Its native schema, result field order,
independent turn-end gate, three-bounce enforcement, report pointers and
withholding are implemented. The actual permission path reviews the dispatching
child's transcript even after saved/hook allows, retains protected deny sources,
and distinguishes blocked, refused and unavailable outcomes without a human
prompt fallback.

TaskRegistry now separates creator ancestry from the reporting recipient,
fences run tokens by session activation and registration, and commits a durable
recipient inbox before reporting success. Consumption persists the exact Peer
row before acknowledging it. Main admission commits independently of the
synchronous parent's turn gate and carries stable native message IDs through
clear, hot activation and cold recovery. Full queues, cancellation and old-run
completion cannot silently discard or retarget an admitted report.

Desktop and mobile share the restored-agent implementation and model-turn wake
path. Content-addressed parked payloads retain full current/archived reports,
and startup barriers account for later callers, pool exhaustion and cancellation.
Large reports use exclusive rooted output files with the native UTF-16 threshold
and preview. Existing output acceptance uses pinned regular single-link metadata,
rejecting symlink, directory and hardlink collisions without reading an old file's
payload. Unix queries the leaf with no-follow `statat` under its pinned parent;
Windows requests only attributes, so an unreadable regular file can still pass
the native metadata check. Exact escaped UTF-16 units survive receiving JSONL writes, retries,
branching, compaction, rewind and cold reconstruction.

Mobile's full execution found that its JsonlWriter lacked durable target
authority, making main-report scope unavailable. The portable
`MobileTranscriptSessionSwitcher` now pins the canonical
`<lingxi_home>/session-state/<bare-session-UUID>` directory and activates the
initial transcript. New/Resume/Clear prepare their cost token and target lock;
the owned orchestrator commit publishes the destination. A bare writer is not
retargeted outside that transaction. Real assembled-runtime tests cover main
report wake, New/Resume activation, both cold TaskRegistry batches and all 14
shared restore unit tests.

The report-security phase now uses the latest native `mR` implementation's 14
rules in `core/src/host/subagent_output_guard_286.json`. The old 2.1.212
handmatcher and its four obsolete brand-baseline entries are retired.
`sanitize_text_with_options` and exact `sanitize_utf16` preserve native findings,
marker/provenance behavior and unpaired code units. The sanitizer leaves
newline normalization, JSON decoding and invisible characters unchanged; later
indentation and JSON-aware peer-wire handling remain separate phases.

A subsequent actual native Agent-child capture found a current-engine mismatch:
Bun 1.4.3 leaves report-marker forms with the new Unicode 17 letters U+323B0,
U+33479, U+16EA0 and U+11DB0 unchanged; the pinned Rust regex-syntax 0.8.10
Unicode 16 tables neutralize them incorrectly. A real native Task-notification
request established this behavior. Production now pins all six relevant Unicode
17 classes in a 16,942-byte table instead of using the dependency's Unicode 16
tables. The expanded scalar/UTF-16 driver went from 3 passed/2 failed to 5 passed
after that fix. Fresh affected full suites and the reusable actual-binary
harness now pass.

The native evidence consists of the original 56 helper scenarios with disclosed
mocks and 45 additional actual native peer-wire cases: 36 strings and 9 raw
UTF-16 inputs. The latter executes the native renderer and JSON-aware
neutralizer. A third native-source sanitizer fixture now contains 748 exact
scenarios: 680 strings and 68 raw UTF-16 inputs. It executes native `mR`, pattern builders,
memoization, deduplication, default-marker behavior and the empty native
environment provider. Only cache-host identity and synthetic provenance
feature-service state are substituted; the fixture documents those boundaries.
None of these fixtures executes a complete native Handback session, classifier
service or native persisted-output write. The JavaScript generators run native
source in Node VM; the sanitizer extraction now pins Node 25.2.1/Unicode 17.0
and exhaustively checks its emitted classes over all Unicode codepoints. Node
execution remains distinct from the actual embedded-Bun path.

The actual native notification JSON preserves unmatched UTF-16 units, while the
subsequent caller API text projects them to U+FFFD. These are separate byte
boundaries. The reusable isolated native CLI harness passed one
scalar report plus seven UTF-16 reports through Agent finalization, task
notifications and caller request serialization. Scripted responses and a fake
loopback API key do not establish live classifier/provider/account acceptance;
Rust String or storage-unit tests alone do not establish the entire protocol.
The actual reusable harness passed all eight scenarios and 32 loopback requests;
its reference notification and caller-serialization assertions are separate.

The fresh post-Unicode-17 five-library run passed Agent 587, Core 738, Fusion 264,
Agent tool 186 and Task tool 189: 1,964 tests, zero failed or ignored. The focused
expanded sanitizer driver passed 5 Rust tests consuming all 748 native-source
scenarios, and the five affected packages passed all-targets Clippy with
`-D warnings`. Core's earlier
747-test checkpoint predates the old matcher replacement. Fusion's combined run
initially passed 263 tests and failed one pool-full sibling-cancellation timing
test. Native regex initialization occurred inside the timing window; the fixture
now initializes the guard before measuring its unchanged one-second abort bound.
The focused rerun passed 1 and full Fusion passed 264, superseding that failure
without a production change or weaker assertions.

Other completed checkpoints passed Permission 1,528, Session 322, Tasks 532,
Tool API 250 and orchestrator 1,254 with zero failed or ignored tests. Core/Tasks
were rerun after latest-only API cleanup, superseding the earlier Tasks
source-sampling reservation. The earlier six-library engine command executed
3,405 tests; counts from overlapping full and focused runs are not unique totals.
Focused runs passed actual TaskRegistry Handback 15, main admission 12,
classifier-only permission integration 6, typed permission
classifier 12, message-queue wake 1 and parked-row persistence 10. These focused
counts overlap the full suites. Bound orchestrator classifier tests passed 7,
including an actual ProviderTransport/ApiService/SDK request capture with
scripted transport responses. Core full coverage includes all 45 native wire
cases, all 748 native-source sanitizer cases and the socket regressions.
Raw logs and dependency boundaries are listed in
[`subagent-handback-2.1.286.md`](subagent-handback-2.1.286.md).

Mobile full execution passed 854 tests with zero failures and one ignored
Local App catalog-regeneration print helper. The earlier three mobile failures
were test-fixture/expectation repairs: strict MCP protocol/version metadata and
cold-recipient activation checks were retained. Desktop and mobile/UniFFI
all-targets checks passed. Core/platform-windows GNU all-targets compilation
passed with existing documentation warnings; this is neither Windows runtime
composition nor native Windows execution. All three native generators passed
fresh `--check` runs after the Unicode 17 correction for the 56 helper, 45
peer-wire and 748 sanitizer cases.

Fresh checks after the initial sanitizer replacement passed desktop/mobile
Clippy and 14-package all-targets Clippy with `-D warnings`, mobile main-report
wake 1, mobile New/Resume transcript authority 1, mobile restore 16 and desktop
restore 14. The desktop assembled builder and settings-drain lifetime each
passed 1 at the earlier checkpoint; the 69-crate workspace all-features/all-targets
compile also passed earlier. The LLM boundary and diff checks pass. Gate-trigger
verification and the explicit check-all run passed for all 10 discovered gates.
Formatting remains to be signed off.

The bounded Unicode 17 Handback scope has fresh full-suite, native CLI and
affected-lint results. The guard's owned files pass rustfmt; wider formatting and
whole-tree acceptance remain in progress as the new instruction-cache production
port is being edited. Earlier broad checks do not validate those later edits.
The saved Gv oracle,
[`instruction_cache_286_oracle.mjs`](../../scripts/tests/instruction_cache_286_oracle.mjs),
passed `--check` with 20 scenarios and 300 deterministic operations. It executes
native cache helpers with controlled file/context loaders, subscription
callbacks and recorded effects; rendering, acquisition, provider calls and
filesystem traversal remain substituted. Production sources are frozen.
Core's 5 cache tests passed within its 743-test full run. Orchestrator's 16 cache
tests and fresh conditional C7's four filesystem/reminder plus two helper cases
passed within the current **1,277/1,277** full run. The Agent full run passed
**595/595**, including authoritative Full-load failure and current SDK contract
coverage. The port includes root-bound child Full/ManagedOnly projections,
query ingress/prefix freezing, sticky failure and atomic compaction/clear
lifecycles. The Runtime account observer uses real first-party identity and pT
without WC. The
[`current-only-sdk-contracts.md`](current-only-sdk-contracts.md) records the
single required subagent stream, required fresh conditional and weak auth
observer capabilities, and explicit missing-backend failures; no older API
forwarding chain or synthetic backend success remains in that contract.

The memory library passed **196/196**. The persisted
[`memory_utf8_286_oracle.mjs`](../../scripts/tests/memory_utf8_286_oracle.mjs)
passed `--check` with **27 cases: 20 byte, 3 stat and 4 backend**; its fixture is
[`memory_utf8_2_1_286.json`](../../crates/memory/tests/fixtures/memory_utf8_2_1_286.json).
It executes actual l0/QDn/Hot with controlled Node fs/Buffer, backend and parser
seams. This is native-source helper evidence, not native CLI/Bun acceptance.
The earlier 12 filesystem soft-skip probes remain ad hoc evidence rather than
additional persisted coverage. Related suites passed **sidequery 60/60** and
**compaction 275/275**. Current integration filters passed
**force_compact_real 17**, **memory_seed 2** and **nested_discovery 8**.
The strict `compact_persistence` rerun passed **4/4**, including exact
context/date/token-budget payload bytes, physical sequence, hot UUIDs,
preserved tail and every cold-resume parent link. Runtime pool starvation
passed **3/3** with actual model entry, pending calls and cancellation-drop
receipts. Current Runtime, LLM Runtime, Orchestrator and Tasks full libraries
passed **1,386 (one ignored), 1,030, 1,277 and 534** respectively. Required Weak
reporting admission broke the actual registry/orchestrator ownership cycle;
the real desktop writer-release and same-session resume failures now pass.
Independent no-default-feature all-target compilation passes after marking
the SDK surface integration test with its actual required engine feature.

PolicyRefresh, SettingsSync, trusted HooksInvalidate, live memory controls,
native health/acquisition error boundaries, attached-project/Perforce
acquisition and real classifier/plugin publication still require production
acceptance. Details and exact evidence boundaries are in
[`instruction-cache-2.1.286.md`](instruction-cache-2.1.286.md).

The MCP `input_required` native-source oracle passed 84 primary cases: 26 codec,
18 schema and 40 flow cases, plus three JavaScript numeric serialization cases.
Production MCP, common HTTP and POSIX stdio now share the actual result driver
and local handler registry. The three new MCP targets passed 13 tests and the
actual HTTP target passed six, including open-SSE concurrent retries and correct
normal-success versus timeout cancellation. The broad transport run found two
old modern fixtures; after fixture-only corrections, the complete MCP/POSIX
library rerun passed 661/163 tests. Combined with the unchanged all-target
results, this scope has 1,099 passes and one explicitly ignored tmux test.
Native helper execution uses controlled transport, callbacks, time and URL
constructors. It does not establish native CLI/Bun IDNA, every error string or
number formatting case, live provider acceptance, or modern remote Tasks.
Details and the current execution boundary are in
[`mcp-input-required-2.1.286.md`](mcp-input-required-2.1.286.md).
The old zero-match filters and historical 2,277-test
validation are not substituted for this work. Account/live provider,
physical-device, packaged-app and complete native-session acceptance remain
unverified. The active sibling app's runtime Git pin has not been migrated.

The implemented main announcement scope and incomplete-thinking/bare-mode gaps
are detailed in
[`main-context-announcements-2.1.286.md`](main-context-announcements-2.1.286.md).
Root cache lifecycle, initial-failure boundaries and remaining host/file
acquisition gaps are tracked in
[`instruction-cache-2.1.286.md`](instruction-cache-2.1.286.md).

The overall latest-version goal remains incomplete; next-wave and evidence gaps
listed here must not be hidden by the passing historical parity suite.
