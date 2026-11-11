# Session contracts: Claude Code 2.1.321

The reference is the installed macOS arm64 executable at
`~/.local/share/claude/versions/2.1.321`, SHA-256
`abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c`.
Anthropic's [npm latest metadata](https://registry.npmjs.org/@anthropic-ai/claude-code/latest)
reported 2.1.321 when checked on November 9, 2026, America/Los_Angeles.
REA verified the executable identity. Behavioral evidence below comes from
bounded embedded-JavaScript reads and Node VM replay, with every source range
checked against its SHA-256. Extracted proprietary source stays out of the
repository. The replay runners use Node built-ins and perform no model requests.
The additional live runners use actual executables with owned profiles and a
scripted loopback provider; they do not use real provider credentials.

## Replay coverage

| Runner | Coverage | Fixture assertions | Pinned ranges |
| --- | --- | ---: | ---: |
| [Session rows](../../scripts/tests/session_rows_321_oracle.mjs) | Unchanged chain writer; ordinary user, assistant, attachment and compact envelopes; team/agent identity; source-tool parents; source version presence | 18 cases | 9 |
| [Session discovery](../../scripts/tests/session_discovery_321_oracle.mjs) | Project slug bytes, alias append/read bytes, deduplication, current/repository catalogs, message presence, raw UUID job lookup | 148 cases | 31 |
| [Session forks](../../scripts/tests/session_fork_321_oracle.mjs) | Native fork names and stored titles, UTF-16 truncation, fork command/storage rows, tools/REPL restore handlers | 44 names + 44 stored titles + 6 display truncations + 31 forks | 20 |

Each runner's `--check` recomputes expected values from the pinned executable
and compares the entire generated fixture file. The discovery runner reuses
older fixture inputs and computes fresh 2.1.321 results. Its overlapping
path, alias, catalog and raw UUID outcomes agree with the 2.1.307 baseline.
The six display truncation cases wrap the native UTF-16 slicing helper with
the declared newline, trim and ellipsis operations.

## Implementation changes

[JSONL serialization](../../crates/session/src/jsonl/schema.rs) places
`teamName`, `agentName`, `promptId`, and `agentId` in the native chain header.
New compact boundaries follow the native factory's `timestamp, uuid, level,
compactMetadata` order. Restored row order is retained, and a source row with
an absent version keeps that absence. The current writer has a separate
[compact golden fixture](../../crates/session/tests/fixtures/golden_sessions/compacted_session_2_1_321.jsonl);
the earlier compact fixture continues to exercise legacy reading.

[Title extraction](../../crates/session/src/jsonl/title.rs) shares an exact
UTF-16 first-prompt selector between stored titles and fork names. It follows
ECMAScript whitespace, command/bash markers, pasted-content wrappers, plugin
display text and strict boolean metadata flags. Stored titles use the native
200-unit safe slice; fork names use the native 100-unit slice, including a
possible unpaired surrogate. Persisted names keep those exact code units;
public Rust `String` display values use the replacement-character rendering.
The [reader's title maps](../../crates/session/src/jsonl/reader.rs) use the flat
UTF-16 decoder too, so a legal sliced-surrogate name remains catalogued and
participates in exact fork-collision lookup.

[Fork rendering](../../crates/session/src/branch.rs) selects native-admitted
captured source rows and mutates exact JSON projections. This preserves source
property order, missing/null scalar fields and exact strings. Native filtering
of sidechain rows occurs before last-UUID-wins insertion. Forks clear
`sessionKind`, neutralize copied refusal-fallback notices, rechain parents,
stamp `forkedFrom`, carry source memory-mode entries, and save both custom
title and agent name. Explicit titles, including normalized empty strings,
are used directly; automatically derived names receive the native suffix.

Collision lookup uses the repository catalog, including linked worktrees.
The ordinary exact-title lookup uses catalog values. Potential surrogate
ambiguities and numeric custom-title occupancy reread matching candidates:
the normal metadata tail is bounded to 64 KiB, with a streamed fallback for
older candidates lacking metadata restamps. Unrelated transcripts are not
reread by this additional lookup.

Fork tools/REPL snapshots derive from the last valid persisted state. Later
invalid values are ignored, tools retain nonempty strings, and REPL mode is a
boolean. Native restore handlers provide the independent expected state;
the native fork saver omits `typedEvents` from the copied REPL stamp.

## Product boundaries

LingXi adds its fixed session-mode/instruction-epoch header and publishes the
fork atomically. Copied skill-activation epochs are read from exact selected
source rows so a malformed display envelope cannot weaken the new session's
instruction fence. Cross-workspace forks reset cwd to the target; valid string
cwd values also retain LingXi's existing target-root policy. A same-workspace
source's missing/null cwd is preserved. Internal process-only JSON carriers
are removed from fork output.

The captured-fork API supplies persisted snapshots. Native live global tools,
REPL, memory and ownership state can differ from that snapshot. Tools evidence
covers persisted record validation; the native registry's
additional tool-use inference from message history is outside this replay.
Catalog search, Git inventories, filesystem classification and remote backend
transport are controlled dependencies; Rust tests separately exercise real
disk aliases/catalogs/raw UUID selection and a real linked Git worktree.
The fork oracle records outputs from an in-memory backend rather than a native
filesystem transaction. The live process checks below supplement this replay;
they do not establish complete request or terminal byte equivalence, deployed
cloud acceptance, or new long-slug header/tail acceptance.

## Live process validation

[CLI runner](../../scripts/tests/session_cli_321_live.py) launches the pinned
native process and the current LingXi CLI with separate owned configuration,
workspace and temporary directories. It retains raw JSONL, invocation and
provider request snapshots at process boundaries. No runner overrides `HOME`.
LingXi uses the supported provider-account CLI with a fixed fake key, an exact
synthetic account and its nondefault configuration directory's hashed Broker
namespace. Cleanup removes only that account. The CLI artifact is signed with
the configured Flare development identity; its bundled Broker matches the
already installed development Broker, avoiding replacement or restart.

Both the native reference and the signed LingXi CLI passed 15 cases: initial
persistence, UUID resume,
continue, source-preserving fork, file resume, exact-title resume, ambiguous
title, missing UUID, resume-at, real Read tool execution, tool-result cold
resume, streaming manual compact, compacted cold resume, explicit one-text-array
SDK input, and its cold resume. Resume-at asserts
the exact retained prefix and excludes later turns; compacted cold resume
retains the summary and the native retained second turn.

Live comparison identified fixes in LingXi's print-mode session selection:
file arguments now pass the existing exact-source authority checks; absent
projects produce the native missing-session diagnostic; empty `--continue`
starts fresh; public fresh-session IDs require the native UUID form. Invalid
or broken project-store roots still fail. Combined continue/resume flags retain
the established continue precedence without applying resume truncation flags
to its selected source.

Twelve paired argument/lookup failures also passed exact stderr comparison.
Eight paired output-format cases cover text, JSON and verbose streaming errors.
An early streaming lookup failure now emits the native zero-turn result frame
without an init frame. SDK request markers use valid JSON pointers when adding
their result fields, so successful and failed requests publish their result
instead of ending after the assistant frame.

Native scalar user content remains scalar through cold resume, persisted forks
and later turns. Its exact source provenance travels with
[request-history metadata](../../crates/core/src/host/request_history.rs) to the
[Anthropic encoder](../../../llm-client/src/providers/anthropic/request_policy.rs),
including retry and fallback paths. A one-text-block array
remains an array; merged sources, tool blocks, citations and cache annotations
retain their array contracts. Exact UTF-16 source strings use the existing
projection mechanism rather than a lossy Rust string conversion. Model-list
operations now share one catalog snapshot per operation, avoiding repeated
catalog rebuilding during resumed CLI startup while refreshing on the next
operation.

[PTY runner](../../scripts/tests/session_ui_321_live.py) records actual terminal
bytes and individual keyboard events. Installed xterm reconstructs readable
buffer frames from those bytes; these frames are not OS screenshots. Native
search, selected-history hydration and a continued turn passed at 100 and 50
columns. Search Enter and either arrow move focus to the matching result
without opening it; the next Enter resumes. Clearing a query retains search
focus and the selected source; a subsequent Escape returns to list focus and
another Escape cancels. Escape after committed search cancels immediately.
Startup cancellation is silent status 1. Interactive empty `--continue`
reports `No conversation found to continue` and exits 1; its behavior differs
from print mode. An empty resume picker reports
`No conversations found in this project.`; Enter keeps it open and Escape
cancels. The LingXi picker was updated to these focus and cancellation
contracts. LingXi fixtures additionally carry its mandatory Code-mode and
instruction-epoch header; a native-only fixture correctly fails that product
authority boundary. Full visual identity is outside these checks.

All nine paired LingXi PTY scenarios passed on the final signed CLI: selected
history and a resumed turn at both widths, both search arrows, ordinary cancel,
query clear with three-stage Escape, committed-search cancel, empty continue,
and empty resume. Positive runs persist the turn into the original UUID,
exclude the other session's history, preserve its transcript hash and exit
normally. Every temporary provider account was removed.

The live native writer also emits diagnostic records such as `atis-latch`,
`api-shape`, `api-request` and `cost-state` which the LingXi transcript does not
emit. The entire JSONL files are therefore not byte-identical. Replay byte
locks cover the specified shared row contracts; live checks separately verify
source identity, restored history, persistence and request content shape.

The PTY runner drains terminal output between individually delivered keys.
This prevents repaint backpressure from batching otherwise spaced input into
a paste burst. Read-only termios snapshots confirm raw input throughout the
picker and resumed composer, with normal terminal modes restored on exit.

The existing real Electron renderer fixtures passed picker IME Escape,
ordinary Escape/focus, Tab trapping, navigation fencing, pending resume
approval, and storage-recovery retry checks (2 tests). Their host APIs are
synthetic; they do not prove packaged Desktop transcript hydration.

[Remote runner](../../scripts/tests/session_remote_321_live.py) passed 5 real
HTTP/SSE transport tests against Firebase RTDB emulator 4.11.2, including
reconnect, encrypted 300 KB traffic, cross-user authorization and rule
revocation. Three additional production service tests passed initial pairing,
second connection/cleanup, encrypted session flow and cross-user rules under
the full repository rules. That existing service suite uses a fake engine;
real transcript-backed gateway coverage is recorded separately by
[the gateway runner](../../scripts/tests/session_remote_321_gateway_live.py).
That runner passed all 13 checks using the current Bridge, production settings
and conversation-space stores, the exact Desktop saved-owner/path resolver,
the catalog subprocess, real engine hydration and authenticated gateway.
Cold reload reads owners, paths and sharing from disk without reseeding. Checks
cover duplicate Unicode titles with distinct UUIDs, selected history, cursor
deltas and reset, unshared and cross-account denial, and the host-only remote
fork restriction. All three source transcripts remain byte-identical after
final shutdown. Eighteen nonempty encrypted RTDB frames were captured before
acknowledgement cleanup; they expose none of the fixture's plaintext markers.
Both actual Bridge process groups and the emulator exited, with owned-process
identity and cleanup verified.
No cloud deployment, WebRTC path or real native V5 service is implied.

The actual native executable writes local JSONL even with
`CLAUDE_CODE_HOVER_REST=1`. Its exported default V5 factory returns no backend
in this public build: range `[277043460, 277043497)`, SHA-256
`1ad043a764e7d9c0c51370e297cc472182523f80a0beb132c5f8bea9b7d6394f`.
Credentials cannot enable this no-op factory. LingXi's Remote Control gateway
reaches Desktop-owned local transcripts, a separate storage architecture.

Evidence is under `artifacts/session-live-321` in the parent LingXi workspace.
Native CLI acceptance is `cli/native-source-shapes-final/report.json`;
the nine accepted native PTY scenarios are indexed by
`ui/native-acceptance.json`, and the paired product scenarios by
`ui/paired-acceptance.json`. Real-emulator reports are under `remote/`.
Exploratory failed captures remain distinguishable from accepted reports.
LingXi CLI acceptance is `cli/product-marker-final/report.json` (15/15);
paired failures are `cli/paired-errors-marker-final/report.json` (12/12) and
`cli/paired-error-formats-marker-final/report.json` (8/8). The actual gateway
report is `remote/gateway-live-final/gateway/report.json` (13/13).
The CLI was built from the current local runtime and SDK sources. Its unsigned
SHA-256 is `969030d97764a937480a8713e0dbc6fbd78b31b59a52f7aea7f8d237aaf6d42c`;
the signed QA copy is `77705538ff1aa4fd193115eb12630ea133ce3720329865a436443c801df85c0a`.
The Bridge SHA-256 is
`e4c656b9e2a29b4b74518d05f1df6244e2ebf4d0cc8781ebf3fde9aab2d42f2e`.

The global SDK init version remains the existing `2.1.295`, alongside LingXi's
routing/capability extensions. This session patch does not migrate the entire
SDK/harness wire contract. Native V5, deployed-cloud, WebRTC and full packaged
Desktop hydration remain outside the accepted coverage. Memory-only Side Chat
forks are outside this native file-session contract.

## Reproduction

From the harness-runtime root:

```sh
node scripts/tests/session_rows_321_oracle.mjs --check
node scripts/tests/session_discovery_321_oracle.mjs --check
node scripts/tests/session_fork_321_oracle.mjs --check
cargo test -p session --offline --no-fail-fast
cargo clippy -p session --offline --lib --no-deps -- -D warnings
```

Live runners require fresh output directories, installed Python `pexpect`,
the sibling Electron xterm dependency, and an executable signed CLI on macOS.
The workspace's QA signing receipt is under
`../artifacts/session-live-321/signed-cli/signature.json`. After preparing that
artifact and the current Bridge, representative live commands are:

```sh
python3 -B scripts/tests/session_cli_321_live.py --engine both --lingxi ../artifacts/session-live-321/signed-cli/bin/lingxi-cli --setup-fake-account --output ../artifacts/session-live-321/recheck-cli
python3 -B scripts/tests/session_ui_321_live.py --engine lingxi --binary ../artifacts/session-live-321/signed-cli/bin/lingxi-cli --case search --cols 50 --setup-fake-account --output ../artifacts/session-live-321/recheck-ui
python3 -B scripts/tests/session_remote_321_gateway_live.py --bridge-server ../lingxi-app/target/debug/bridge-server --output ../artifacts/session-live-321/recheck-gateway
```

Session-crate validation on November 9, 2026: all three oracle byte checks passed (291
reference checks across 60 pinned range receipts), all 685 session tests passed
with no failures or ignored tests, and the scoped Clippy command passed.
Unscoped Clippy stopped on `empty_line_after_outer_attr` in
`crates/core/src/host/orchestrator.rs`, outside the session patch; no
workspace-wide lint pass is claimed.

Final focused regressions passed 24 tests: CLI host 6, TUI picker 12, core
metadata 1, request pipeline 3, SDK encoder 1, and SDK result publication 1.
Every executed filter matched at least one test; none failed or were ignored.
Exact commands, counts, executable hashes and lockfile receipts are indexed by
`cli/focused-unit-summary.json`. The SDK test build completed offline without
fetching dependencies. All three repository lockfiles retained their hashes.

The focused orchestrator library-test target cannot currently compile:
`memory_update_reminder_tests.rs` still calls
`project_dynamic_memory_context` and
`assemble_subagent_prompt_context_for_target_preview`, which the existing
dirty prompt changes remove along with their production call sites. Those
concurrent changes were preserved and no tests were disabled. Consequently the
new orchestrator hydration, background-fork and catalog-snapshot unit cases
have no passing execution claim. Actual CLI cold resume, fork, request shape
and startup behavior are covered by the accepted process runs above.

The session production sources match the accepted build; the sole subsequent
source correction converts a catalog fixture into `ClientConfig` inside `cfg(test)`.
The exact delta and unchanged production files are recorded by
`verification/source-freeze-final.json`. Focused test commands and canonical
lockfile hashes are retained under `cli/build/`; copied lockfiles preserve all
registry versions/checksums and leave repository lockfiles untouched.

The native fixture tests are
[row bytes](../../crates/session/tests/session_rows_321_test.rs),
[discovery](../../crates/session/tests/session_discovery_321_test.rs), and
[fork bytes](../../crates/session/tests/session_fork_321_test.rs).
Title helper fixture assertions run in the session library tests. The fork
integration suite also covers linked-worktree collisions and instruction epochs
on malformed-but-native-admitted duplicates. Disk discovery adapts fixture paths
to `branding::DOT_DIR` and probes the host volume's filename case sensitivity.
The equal-mtime duplicate test uses actual directory enumeration order to
exercise native first-encounter selection on the host filesystem.
