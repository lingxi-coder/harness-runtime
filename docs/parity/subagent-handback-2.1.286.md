# Subagent Handback in Claude Code 2.1.286

`SubagentHandback` is implemented in the production child runtime, task registry,
permission path, main-session admission and desktop/mobile composition. The
previous read-only design and “absent from production” status are superseded.
The implementation has executed regression coverage; complete byte-level parity
with the native application is not claimed.

The target is the latest pinned Claude Code contract, currently `2.1.286`.
The user explicitly requires no older-version compatibility: do not retain old
Claude Code behavior, SDK aliases, private JSONL layouts or parked-row migration
adapters just to accept earlier versions. Branding, multiple LLM providers,
Fusion and Local Apps remain the explicit product exclusions. Ordinary Agent
reporting is in scope. `core::host::CLAUDE_CODE_VERSION` remains `2.1.267` until
the broader runtime audit has passed; implementing this capability does not
justify a version bump.

## Pinned native evidence

The darwin-arm64 reference binary is 225,167,728 bytes, SHA-256
`75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
All three generators reject a different hash and record actual binary byte offsets:

```sh
node scripts/tests/subagent_handback_286_oracle.mjs /path/to/2.1.286 --check
node scripts/tests/handback_wire_286_oracle.mjs /path/to/2.1.286 --check
node scripts/tests/handback_sanitize_286_oracle.mjs /path/to/2.1.286 --check
```

These generators execute hash-pinned native JavaScript source in Node VM.
That is native-source evidence, not execution by the binary's embedded Bun
engine. Engine-dependent Unicode semantics need separate actual-binary captures;
the corrected boundary gap below demonstrates why this distinction matters.
The sanitizer generator pins Node `25.2.1` and Unicode `17.0`; run it with that
Node version. Its recorded V8/ICU metadata also participates in fixture equality.

The first fixture,
[`subagent_handback_2_1_286.json`](../../crates/agent/tests/fixtures/subagent_handback_2_1_286.json),
contains 56 scenarios: 16 spawn, 5 recipient, 18 tool-call, 12 schema and 5 framing
cases. It executes native schema constructors, the lazy schema factory, tool and
queue helpers, recipient readability, aliases, warnings and spawn conditions.
It substitutes the effective-mode fold, feature flags, tool-factory adaptation,
registry, admission outcome, classifier, sanitizer and persistence. In
particular, its `mR` identity mock does not establish report-security sanitizer
parity, and its controlled `kwo` outcome does not execute a classifier model.
Its character threshold and in-memory pointer do not execute native filesystem
persistence. These are discovery expectations, not production test results.

The second fixture,
[`handback_wire_2_1_286.json`](../../crates/core/tests/fixtures/handback_wire_2_1_286.json),
contains 36 ordinary-string and 9 raw UTF-16 cases. It executes the actual native
`Aue` peer-envelope renderer, `wXe` neutralizer and their `pVr`/`Qo` dependencies,
with the imported tag constants checked against their native module. It covers
XML sender escaping, JSON-aware peer-frame neutralization, Unicode confusables,
unpaired high/low surrogates and sender newlines. It does not execute report
classification, the separate report-security sanitizer or filesystem writes.
Fixture scenario counts must not be reported as Rust test counts.

The third fixture,
[`handback_sanitize_2_1_286.json`](../../crates/core/tests/fixtures/handback_sanitize_2_1_286.json),
executes the native `mR` source, all 14 recognition rules, Unicode/control-tag and
artifact pattern builders, memoization, findings deduplication, marker defaults
and the empty native feature-environment provider. Its expanded 748 exact
scenarios are 680 scalar-string and 68 raw UTF-16 inputs across marker on/off and provenance
on/off. They cover escalation flagging, control/provenance neutralization,
silent turn markers, JavaScript whitespace/word boundaries, warnings, confusable
characters and unmatched surrogate units. Raw provenance environment values are
verified to have no getter in this artifact. The fixture substitutes only an
opaque cache-host identity and synthetic true/false `tengu_melodic_wolf`
feature-service state; other flags use the native caller defaults. It does not
execute peer-wire sanitization, persistence, classification or the binary CLI.
The independent wire fixture covers the peer-wire phase described above.

A separate actual-binary harness,
`scripts/tests/test_handback_native_sanitize_286.py`, passed all eight scenarios:
one Unicode scalar report and seven UTF-16 reports. It uses fresh config/workspace
directories and scripted loopback Anthropic replies with a fake key. Native
Agent finalization, report sanitization, Task notifications and the caller
request execute in the embedded Bun engine. This is distinct from executing the
full 748-case source corpus or a successful native SubagentHandback/classifier
session. Reproduce the isolated native check with:

```sh
python3 scripts/tests/test_handback_native_sanitize_286.py /path/to/2.1.286 \
  --output /tmp/handback-native-sanitize-286
```

## Native availability and tool contract

`tengu_lively_waffle` defaults true. Availability also requires the live parent
mode and effective child mode to be `auto`, explicit ordinary-Agent opt-in,
neither an exact-tool surface nor structured output, a supplied implementation,
and no conflicting canonical name or alias. Injection follows ordinary tool
filtering; an already supplied instance is reused. Ordinary launch opts in with
`!fork_conversation`; resumed asynchronous launch also excludes observers.
Generic teammate/resolver callers are not opted in merely by using auto mode.

The syntactic `CLAUDE_CODE_SENDMESSAGE_HANDBACK` override is unreachable in this
public artifact: the native provider schema is empty and creates no getter for
that key. The implementation does not invent a raw environment override.

The tool is always loaded, not read-only, and cannot run in the background. Its
input is `{message: string}`, with description `Your full report for your caller`;
native validation strips unknown fields. There is no recipient input. A valid
blank message separately returns `message must not be empty`, error code 9.
The model-facing result preserves native field order:
`{"success":boolean,"message":string}`. Successful admission returns
`Report delivered to your caller.` and a trusted turn-end marker under the
independent default-true `tengu_purring_pine` gate. Failed admission, an inactive
contract, duplicate delivery or a missing foreground caller does not end the run.

At natural end, an active contract permits at most three
`[handback-send-enforce]` reminders. Auxiliary work and a caller waiting on owned
background work remain exempt. Finalization produces `send`, `flagged` or
`withheld`: delivered output becomes the native inbox pointer, while unsent final
model text is withheld. Waiting owners retain the interim waiting message and
their existing keepalive lifecycle. A delivered report is not classified again
by the completed-work handoff review.

## Production implementation

| Boundary | Implemented contract | Source |
| --- | --- | --- |
| Authority and lifecycle | Session/activation scope, per-agent run epoch, private registration token, immutable receipt and typed Peer origin; new runs reset current delivery while preserving archived reports | `core/src/host/handback.rs`, `tasks/src/handback.rs`, `tasks/src/state.rs` |
| Trusted launch and injection | Live effective parent mode, explicit host opt-in, fork/observer/exact/schema gates and supplied child-only tool dispatch | `agent/src/handle/spawn_context.rs`, `agent/src/tool_resolver.rs`, `tool-api/src/tool_invoker_impl.rs` |
| Child reporting and termination | Native schema/result bytes, classifier-only action, enforcement, full-report state, pointer/withheld disposition and trusted batch termination | `agent/src/handback.rs`, `agent/src/runner.rs`, `tools/agent/src/agent.rs` |
| Permission composition | Forced review of the actual dispatching child's transcript; typed passed/blocked/refused/unavailable outcomes and protected deny sources | `permission/src/policy_gate.rs`, `permission/src/loop_llm.rs`, `orchestrator/src/loop_permission_classifier.rs` |
| Nested admission | Recipient-owned durable inbox, exact consumption acknowledgment, detached owned settlement and nonblocking wake hints | `tasks/src/handback.rs`, `tasks/src/handlers/local_agent.rs`, `core/src/host/task_registry.rs` |
| Main admission | Session-pinned durable inbox independent of the main turn gate; native meta row, stable message ID, activation-safe consumption and recovery | `orchestrator/src/conversation/main_reports.rs`, `orchestrator/src/conversation/drivers/prepare.rs`, `msgqueue/src/queue.rs` |
| Wire and durable transcript | Native peer renderer/neutralizer, exact UTF-16 overrides, durable append/retry and preservation through branch/retry/compact/rewind rewrites | `core/src/host/handback_wire.rs`, `core/src/types/exact_json.rs`, `agent/src/transcript.rs`, `session/src/jsonl/` |
| Report sanitizer | The pinned native 14-rule recognizer, ordered findings/warnings, marker/provenance choices and exact UTF-16 neutralization | `core/src/host/subagent_output_guard.rs`, `core/src/host/subagent_output_guard_286.json` |
| Current engine Unicode classes | Explicit pinned Unicode 17 property classes for L/N/Pd/Pc/Pe/Pf, independent of Rust regex-syntax's older tables | `core/src/host/subagent_output_guard_unicode17.json` |
| Large reports and restored hosts | Exclusive rooted output files; content-addressed parked payloads; provider/session origin pinning and shared desktop/mobile cold reconstruction | `agent/src/handback_output.rs`, `session/src/agent_rows.rs`, `runtime/src/parked_agent_restore.rs`, `runtime/src/main_report_waker.rs` |
| Mobile transcript authority | Canonical pinned session-state writer, initial activation and prepared cost/transcript switching through the owned orchestrator commit | `runtime/src/mobile/transcript.rs`, `runtime/src/mobile/host/assembly.rs` |

Paths in this table are relative to `crates/`. They identify production code;
the verification layers below state which paths have actually executed.

Latest-only cleanup removes the unused `mark_rested` forwarding API and keeps
one run-fenced `notify_rest`/`mark_task_rested` path. Native transcript append
and Fusion append retain distinct current identity formats: native rows are
UUID-only, while Fusion rows carry `deliveryId`. The native API rejects even a
null `deliveryId` with `NativeDeliveryIdField`; the Fusion API does not upgrade
an old UUID-only row into a Fusion duplicate. This strict separation preserves
the current Fusion product surface without an older-format compatibility path.
The reader also removes its unused `ReaderError::Parse` compatibility variant.
The old 2.1.212 report handmatcher is retired in favor of the current native
rule table; four obsolete brand-baseline entries for that removed code were
deleted with it. This cleanup is not itself a passing brand/repository gate.

### Admission, recipient identity and recovery

Creator ancestry is immutable and separate from the current reporting recipient.
Each logical child run begins after its task row and receiver are published,
before the first model request. Only that registration's live token can admit a
report. Sender identity is derived from the registered lifecycle: `from` uses
the registered name or agent ID, display/pointer name uses the registered name
or resolved agent type, and sender task identity is the canonical agent ID.
Model input, XML text and a display name cannot mint Peer authority.

A recipient must be a running local agent, or a completed local agent with
keepalive reasons other than the idle-window flag. A foreground child whose
caller disappeared fails admission; a background child may retarget the scoped
main recipient. Current-run state and immutable archived receipts are distinct,
so an earlier report does not ban the retained agent from reporting in a later
run or leak an earlier warning into the new run.

Nested admission writes the recipient's journal before returning success.
Snapshots retain pending entries until exact-receipt acknowledgment, and the
receiving transcript is durably written before that acknowledgment. Wake events
are hints; a full event queue does not hold report admission, synchronous parent
completion or user stop. Production settlement owns its transaction even if
the invoking future is canceled. Completion from an old run cannot park a newer
run while an earlier I/O operation is suspended.

Main admission likewise commits a private durable inbox before acknowledgment.
It does not wait for the parent's model consumption or reacquire the turn gate
held by a synchronous Agent call. Consumption creates one native model-visible
meta user row with a stable UUID and Peer origin. Private deduplication keys,
queue priority and storage carriers are not emitted as native transcript fields.
Clear and hot activation fence old reports by session and activation; deliberate
cold recovery may consume already committed reports for that same session while
retaining their original immutable receipt and origin. It cannot silently move
a report into another session or elevate malformed Peer data to Human authority.

Cold restoration publishes all retained task rows and uses a counted startup
barrier so a later caller is available before its reporting child resolves the
recipient. Startup failure, cancellation and an exhausted asynchronous pool
release participants without blocking the surviving restored actors. Parked
metadata remains small; large current/history/request data lives in a validated
content-addressed sibling payload. Origin directory registration happens before
the worker starts, so later park/unpark does not follow a changed current session
or working directory.

Full mobile execution uncovered a real composition gap: its JsonlWriter had no
durable target installed, so main-report scope was unavailable. Mobile now binds
`MobileTranscriptSessionSwitcher` to the canonical
`<lingxi_home>/session-state/<bare-session-UUID>` directory and activates the
initial transcript. For New, Resume and Clear, its CostSessionSwitcher prepares
the cost token and pinned writer without changing the active destination; the
owned orchestrator commit publishes the prepared target. A cached writer's
original inode authority is checked before publication. Session transitions do
not bypass that transaction by retargeting a bare writer.

### Bytes, output files and classifier review

The report-security phase now uses the exact native 2.1.286 rule table in
`subagent_output_guard_286.json`. `sanitize_text_with_options` is the scalar
projection; `sanitize_utf16` inserts neutralizers into the original code-unit
buffer so unmatched surrogates remain distinct. It preserves native rule order,
reportable/silent findings and warning deduplication. This phase does not
normalize newlines, decode JSON strings or remove invisible characters; the
subsequent report indentation and JSON-aware peer wire are separate operations.
Default marker/provenance behavior and explicit presentation options match the
the captured native-source scenarios rather than the retired handmatcher.

Actual binary testing subsequently found a Unicode engine mismatch outside that
corpus. The embedded Bun 1.4.3 recognizes Unicode 17 letters U+323B0, U+33479,
U+16EA0 and U+11DB0, leaving marker shapes such as `[subagent<new-letter>:` and
`[harness<new-letter>:` unchanged. The pinned Rust regex-syntax 0.8.10 Unicode 16
tables incorrectly permit neutralization there. This observation comes from a
real native Agent-child Task-notification request capture, not an inference from
Node execution. Production now uses the pinned 16,942-byte Unicode 17 class table:
L contains 145,672 codepoints, N 1,924, Pd 27, Pc 10, Pe 77 and Pf 10. The generator
exhaustively compares each emitted property class against its pinned engine over
all Unicode codepoints. The expanded 748-case Rust driver first failed on the
scalar/UTF-16 boundary tests (3 passed, 2 failed), then passed all 5 after the
production fix. The earlier 580 cases were insufficient coverage. The actual
binary additionally retains unmatched units in notification JSON but projects
them to U+FFFD in the subsequent caller API text. Storage/unit preservation and
provider-wire UTF-8 are separate boundaries; Rust String/UTF-16 tests do not
establish complete native provider-protocol acceptance.

The inbox copy uses native report-marker behavior; the stored full report uses
the marker-disabled sanitizer result and retains its optional warning. Warning
and report framing preserves the native newline boundary. The subsequent peer
envelope is `<agent-message from="...">\n...\n</agent-message>`; its body uses the
JSON-aware neutralization of agent, teammate and cross-session frames. Exact
UTF-16 data is carried separately from its lossy display string when necessary,
including a preview that ends on an unpaired surrogate. JSONL encoding retains
the corresponding escaped code units without publishing the internal override
carrier. Parsing, stable-UUID append, cold loading and transcript rewrites retain
those units rather than substituting U+FFFD.

The large-output threshold is 50,000 UTF-16 code units, with a 2,000-unit preview
and the native persistence limit. Output uses the trusted transcript session's
`tool-results` directory and a safe tool-use leaf. Creation opens and writes the
same exclusive rooted file handle; a collision never overwrites an existing
output. Local persistence failure falls back to inline report text. The collision
check uses metadata under a pinned parent without reading the previous output
payload. Unix uses no-follow `statat`; Windows requests `FILE_READ_ATTRIBUTES`.
A regular single-link file is acceptable even when its mode denies payload
reads; symlinks, directories and multiply linked files are rejected. The report
state retains the full sanitized text even when the inbox uses a saved-output
pointer. Durable receiving transcripts use the rooted append/sync/retry path:
an exact committed row is resynced once, and only an intent-verified partial tail
may be truncated and replayed.

Classifier-only composition runs even when saved rules, hooks or safe-tool logic
would allow the tool. It requires auto mode and has no human prompt fallback.
The flagging tool excludes user/project/local/session deny sources while keeping
managed, command and other protected sources. Blocked/refused reports are
delivered with a security warning and flagged; unavailable review adds its note
without flagging. A blocked result increments the native auto-denial counter;
passed review resets it. Typed metadata preserves refusal, HTTP/model failure
and parse/availability distinctions without interpreting reason strings. The
bound production classifier routes through ProviderTransport/ApiService and the
SDK using the child's transcript and native tool action. A fake response at that
transport boundary is still not a live classifier service acceptance result.

## Executed verification

The following logs were inspected for nonzero executions of this implementation
wave. Each row identifies its checkpoint. The Unicode 17 focused driver,
affected full libraries, five-package all-targets lint and actual-binary harness
have fresh successful executions. Earlier wider checks remain labeled by their
checkpoint; new cache/MCP work is outside this Handback validation scope.

| Execution | Result | Evidence |
| --- | --- | --- |
| Core full library after Unicode 17 correction | 738 passed, zero failed/ignored; includes native wire, expanded sanitizer and socket tests | `/tmp/harness-286-handback-unicode17-full-v1.log` |
| Agent full library after Unicode 17 correction | 587 passed, zero failed/ignored | `/tmp/harness-286-handback-unicode17-full-v1.log` |
| Permission full library | 1,528 passed, zero failed/ignored | `/tmp/harness-286-handback-engine-full-v4.log` |
| Session full library | 322 passed, zero failed/ignored | `/tmp/harness-286-handback-engine-full-v4.log` |
| Tasks full library after latest-only API cleanup | 532 passed, zero failed/ignored | `/tmp/harness-286-handback-final-core-tasks-v2.log` |
| Orchestrator full library | 1,254 passed, zero failed/ignored | `/tmp/harness-286-handback-orchestrator-full-v2.log` |
| Mobile runtime full library | 854 passed, zero failed, 1 catalog-regeneration helper ignored | `/tmp/harness-286-handback-mobile-full-v2.log` |
| Desktop all-targets compilation | Passed; not a desktop test execution | `/tmp/harness-286-handback-desktop-alltargets-v6.log` |
| Mobile/UniFFI all-targets compilation | Passed; superseded for runtime execution by mobile full tests above | `/tmp/harness-286-handback-mobile-alltargets-v2.log` |
| Core and platform-windows GNU all-targets compilation | Passed with existing missing-documentation warnings; no Windows execution | `/tmp/harness-286-handback-windows-alltargets-v1.log` |
| Agent tool full library after Unicode 17 correction | 186 passed, zero failed/ignored | `/tmp/harness-286-handback-unicode17-full-v1.log` |
| Task tool full library after Unicode 17 correction | 189 passed, zero failed/ignored | `/tmp/harness-286-handback-unicode17-full-v1.log` |
| Fusion full library after Unicode 17 correction | 264 passed, zero failed/ignored | `/tmp/harness-286-handback-unicode17-full-v1.log` |
| Fusion pool-full sibling cancellation after fixture repair | 1 passed; preserves the one-second abort assertions | `/tmp/harness-286-handback-fusion-cancel-v2.log` |
| Tool API full library | 250 passed, zero failed/ignored | `/tmp/harness-286-handback-engine-full-v4.log` |
| Native report-sanitizer focused Rust driver | 5 passed; consumes all 580 exact native scenarios | `/tmp/harness-286-handback-sanitize-core-v1.log` |
| Expanded Unicode 17 sanitizer regression, before production correction | 3 passed, 2 failed on scalar/UTF-16 boundaries | `/tmp/harness-286-handback-sanitize-unicode17-red-v1.log` |
| Expanded Unicode 17 sanitizer regression, after production correction | 5 passed; consumes 680 scalar and 68 raw UTF-16 scenarios | `/tmp/harness-286-handback-sanitize-unicode17-green-v2.log` |
| Actual native Agent report / Task-notification / caller serialization | 8 passed, 32 loopback requests; fake model replies | `/tmp/harness-286-handback-native-cli-unicode17-root-v1/validation.json` |
| Five affected packages all-targets Clippy after Unicode 17 correction | Passed with `-D warnings` | `/tmp/harness-286-handback-unicode17-clippy-v1.log` |
| Desktop shared restore tests, after initial native sanitizer | 14 passed | `/tmp/harness-286-handback-desktop-restore-final-v1.log` |
| Desktop assembled builder, before latest sanitizer | 1 passed | `/tmp/harness-286-handback-desktop-builder-v1.log` |
| Desktop settings-drain cancellation/lifetime, before latest sanitizer | 1 passed | `/tmp/harness-286-handback-desktop-settings-lifetime-v1.log` |
| 14-package all-targets Clippy after native sanitizer replacement | Passed with `-D warnings` | `/tmp/harness-286-handback-clippy-final-v1.log` |
| Desktop/mobile Clippy after initial native sanitizer | Passed with `-D warnings` | `/tmp/harness-286-handback-clippy-desktop-final-v2.log`, `/tmp/harness-286-handback-clippy-mobile-final-v1.log` |
| Mobile assembled main-report wake after initial native sanitizer | 1 passed | `/tmp/harness-286-handback-mobile-handback-final-v1.log` |
| Mobile New/Resume transcript authority after initial native sanitizer | 1 passed | `/tmp/harness-286-handback-mobile-authority-final-v1.log` |
| Mobile restore after initial native sanitizer | 16 passed: 14 shared units plus 2 actual cold TaskRegistry batches | `/tmp/harness-286-handback-mobile-restore-final-v1.log` |
| Repository gate execution-trigger regression | Passed: all 10 discovered gates have execution triggers | `/tmp/harness-286-handback-gate-triggers-final-v1.log` |
| Repository check-all after initial native sanitizer | Passed: all 10 gates exited zero | `/tmp/harness-286-handback-gates-final-v1.log` |
| 69-crate workspace all-features/all-targets compilation, before latest sanitizer | Passed; compilation only | `/tmp/harness-286-handback-workspace-alltargets-v2.log` |
| Actual task-registry Handback filters | 15 passed | `/tmp/harness-286-handback-tasks-v3.log` |
| Main-session report admission (`main_reports_impl::tests`) | 12 passed | `/tmp/harness-286-handback-admission-v3.log` |
| Core Handback filter | 11 passed | `/tmp/harness-286-handback-core-v5.log` |
| Classifier-only permission integration | 6 passed | `/tmp/harness-286-handback-permission-v3.log` |
| Permission classifier transport/typed failures (`loop_llm`) | 12 passed | `/tmp/harness-286-handback-permission-loop-v1.log` |
| Bound orchestrator classifier (`loop_permission_classifier`) | 7 passed, including real SDK request capture with scripted responses | `/tmp/harness-286-handback-classifier-real-v1.log` |
| Typed message-queue wake | 1 passed | `/tmp/harness-286-handback-msgqueue-v1.log` |
| Parked-row persistence filter | 10 passed; included in the later Session full run | `/tmp/harness-286-handback-session-rows-v1.log` |

Agent execution covers actual supplied-tool dispatch, request schema/result
bytes, reject/retry, the independent turn-end gate, three-bounce withholding,
live-mode countermand, per-run resets, native frame security, real POSIX output
writes, UTF-16 preview input and durable partial-write/sync-failure recovery.
Its scripted permission/registry components are disclosed by the fixture.
Task tests use the actual TaskRegistry with controlled journal failures and
queue saturation; they exercise ownership, cancellation, receipt durability,
readability, hot-scope rejection and cold recipient recovery. Main tests exercise
the real admission/consumption path, stable native rows, exact disk bytes,
activation and restart behavior with controlled dependencies.

The latest Core full execution includes all 45 native wire cases, all 748
native-source report-sanitizer scenarios and rooted filesystem regressions. Its 738
test count belongs to the suite after the old report matcher was retired; the
earlier 747-test checkpoint predates that replacement. Session full execution
includes the exact JSONL writer/parser, strict current identity formats and
rewrite regressions.
The fresh five-library Unicode 17 command executed 1,964 tests: Agent 587,
Core 738, Fusion 264, Agent tool 186 and Task tool 189. Its complete run exited
zero with no failed or ignored tests.
The earlier six-library engine command executed 3,405 tests in total. The fresh
Core/Tasks command supersedes the prior Tasks source-sampling reservation after
its obsolete API removals. Counts across filtered and full runs overlap and
must not be summed as unique tests.

The mobile full run executes the real assembled runtime's main-report admission
and model-turn wake, real New/Resume transcript activation, both actual cold
TaskRegistry batch tests and all 14 shared restoration unit tests. They cover
later callers and inactive archives, pool exhaustion, origin directory retention
and strict Peer/UTF-16 reconstruction. Its one ignored test,
`print_current_profile_contracts`, only prints reviewed Local App catalog
contracts for regeneration; it is not an unexecuted Handback or device test.
The earlier mobile run's three failures were corrected in tests: two MCP
fixtures lacked required protocolVersion/serverInfo, and a cold main-recipient
assertion used the old activation epoch. Production protocol and scope checks
remain strict. The mobile runtime test dependencies still use fake platform and
provider services, so real host assembly is not physical-device acceptance.

All three native-source generators passed fresh `--check` after the Unicode 17
correction: 56 helper, 45 peer-wire and 748 report-sanitizer scenarios. The
expanded Rust driver is green and the five affected full libraries pass.
The actual-binary harness
passed all eight scenarios with 32 loopback requests, retaining each notification's
exact UTF-16 units and independently checking the caller text's U+FFFD projection.
Other corpus modes still execute extracted source, not the complete binary.
Agent's trace-capture fixture repair
and metadata collision checks have a successful fresh full run. The combined
run's Fusion failure was isolated to initial native regex compilation occurring
inside the pool-full cancellation test's timing window. The fixture now
initializes the shared guard before measuring its unchanged one-second sibling
abort deadline; assertions and production code remain unchanged. Its focused
rerun and the full 264-test Fusion suite pass, superseding the 263/1 result.
The LLM boundary and diff checks also pass after the sanitizer replacement.

The Handback Unicode 17 scope has successful focused/full runs, native CLI
captures and affected all-targets lint. The guard's owned files pass rustfmt;
wider formatting and whole-tree validation are still in progress because new
instruction-cache/MCP production work is being edited. The earlier broader
host/lint/gate checkpoints do not blanket-validate those later changes.
Add only executed results to this section; a compiled runtime library or a
zero-match test filter is not a passing host test.

## Remaining acceptance boundaries

- None of the three generators runs a complete native Handback session. A full
  native child/caller differential trace, including complete phase ordering,
  real persisted outputs and classifier service behavior, remains distinct from
  the 56 helper, 45 peer-wire and 748 report-sanitizer scenarios and scripted Rust
  coverage.
- Node native-source fixtures do not substitute for the embedded Bun engine's
  Unicode semantics. The confirmed Unicode 17 marker boundary correction has
  meaningful red/green, full-suite and eight-scenario actual-binary coverage;
  the rest of the corpus remains native-source execution.
- Native notification JSON preserves lone surrogates while caller API text uses
  U+FFFD. Do not apply a single UTF-16-preservation claim across both boundaries.
- Mobile cold restoration, main-report wake and session transcript authority
  now execute through the assembled runtime. Desktop shared restoration and
  assembled builder execution also pass for the Handback implementation;
  post-sanitizer final verification is tracked separately above.
- Remote policy bodies, account entitlements, live provider behavior, packaged
  app operation, physical mobile devices and native Windows execution are not
  established by the local tests above.
- The Windows cross-check covers core and platform-windows all targets; it does
  not establish a Windows runtime composition build or native Windows behavior.
- The sibling app retains its canonical earlier runtime Git pin. Uncommitted
  SDK/source changes and `cli-bg-session-snapshot.patch` do not mean that the
  runtime has been pinned, built or deployed in that app.
- The broader latest-version backlog in
  [`claude-code-2.1.286.md`](claude-code-2.1.286.md) remains active. This production
  port removes the old “not implemented” status, not the remaining byte-parity
  audit or the overall goal.
