# Goal contracts: Claude Code 2.1.322

The reference is the installed macOS arm64 executable at
`~/.local/share/claude/versions/2.1.322`, SHA-256
`a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450`.
Anthropic's [npm latest metadata](https://registry.npmjs.org/@anthropic-ai/claude-code/latest)
was checked again during final validation and still reports 2.1.322.
The shipped executable, rather than an older parity document, defines the
behavior below.

## Evidence and byte coverage

| Replay | Cases | Pinned source ranges |
| --- | ---: | ---: |
| [Goal oracle](../../scripts/tests/goal_322_oracle.mjs) | 38 slash commands, 23 proposal canonicalizations, 24 proposals, 45 interruptions, 20 interval settings | 26 |
| [Heading oracle](../../scripts/tests/goal_heading_322_oracle.mjs) | 67 inputs, each comparing heading, check-in and retry payloads | 12 |

These 217 cases replay bounded JavaScript helpers read directly from the
reference executable. Each runner checks the executable digest and each
source range digest before evaluating the helper with explicit host adapters.
`--check` compares the entire generated fixture, including exact UTF-16 source
strings and JSON spelling. Two helper ranges are shared between the runners;
there are 36 distinct pinned ranges. Extracted native source is kept outside
the repositories.

REA independently parsed the four selected embedded JavaScript chunks. Its
parent evidence is
`ev_30145b41a2218892da5a965a6490069c523e3e658ac44dcc5f8717a498fdab1b`;
the summary evidence is
`ev_c67dbfe23b2b070c8af4e339604f6ef5a04bb176962814c696bb7d05ba66ac98`.
That graph is static evidence and has 239 semantic unknowns; it is not a native
application execution trace. The executable helper replay supplies the
behavioral expectations used by the Rust tests.

The fixture receipts and reproducible commands are in
[goal_2_1_322.json](../../crates/command-api/tests/fixtures/goal_2_1_322.json) and
[goal_heading_2_1_322.json](../../crates/orchestrator/tests/fixtures/goal_heading_2_1_322.json).
Run both from the runtime checkout:

```sh
node scripts/tests/goal_322_oracle.mjs --check
node scripts/tests/goal_heading_322_oracle.mjs --check
```

The proposal display helper uses native `Bun.stringWidth`; its replay delegates
that dependency to the installed Bun runtime. The receipt records that boundary.
The finite Unicode/tab cases also run against the Rust implementation. Schema
receipts establish native field metadata, defaults and guards; they do not
claim byte equivalence with native Zod's JSON Schema serializer or aggregated
validation diagnostics.

## Slash command and evaluation

[The command handler](../../crates/command-api/src/builtins/goal.rs) preserves
the native status, clear, error, success-notice and kickoff payloads. Typed
conditions retain their exact UTF-16 units, including lone surrogates. Trimming
uses ECMAScript whitespace and the length cap is 4,000 UTF-16 units. Set gates
run in the native hooks-restriction, organization-hold and trust order;
noninteractive invocation skips the trust gate only. Status and clear retain
their separate gate behavior. A verified relayed human command is rejected
before argument handling when the native relay guard is enabled.

Ingress facts are task scoped and supplied by the owning transport. Transcript
text, tool arguments and persisted metadata cannot grant relay or human-turn
authority. This work does not add a Slack or Teams account integration.

Goal evaluators participate in Stop hooks only while current policy permits
them. Restriction removes that evaluator from the dispatch without removing
independent Stop blockers, discarding the goal, or advancing its iteration.
Stale goal results cannot reappear as ordinary Stop blockers. Goal replacement
and clearing reset retry, deferral and check-in timing state.

[Check-ins](../../crates/orchestrator/src/prompt/goal_checkin.rs) and
[interruption retries](../../crates/orchestrator/src/prompt/goal_interruption.rs)
share the native heading pipeline: filter ill-formed and invisible source,
collapse control/whitespace runs, map all 51 native angle/slash aliases, then
escape markup. A typed goal's stored source remains exact while its reminder
heading is sanitized. The replay includes astral aliases and lone surrogates.
Idle reminders preserve the native pause suffix and live feature gate.

`CLAUDE_CODE_GOAL_CHECKIN_MINUTES` follows the native typed integer parser:
ECMAScript trim, optional sign, digits only, and a 0–10,080 minute bound.
Invalid values fall back to 30 minutes. Zero disables the periodic check-in.
Retry classification preserves native rate/quota, transient, authentication
and stop ordering, including the daily-cap pause variant.

## Model proposals and host ownership

[ProposeGoal](../../crates/tools/ui/src/propose_goal.rs) is deferred, read-only
and nonconcurrent. Its native `tengu_propose_goal` feature flag defaults to
false. It requires an interactive local main session, a real dialog/queue
transport, current evaluator permission, and a non-plan permission mode.
The condition is capped at 500 UTF-16 units before canonicalization and after
display normalization. `ask_user` defaults to true.

The host reads `modelProposedGoals` only from trusted managed, command-line and
user layers. `auto`, `alwaysAsk` and `disabled` retain their native meanings.
An incomplete settings snapshot requires consent. A proposal returns promptly;
approval happens through the real AskUserQuestion transport and activation
occurs later through the ordinary `/goal` queue. There is at most one pending
proposal per owning session. Ordinary goal set/clear operations do not cancel
that pending proposal. Session replacement invalidates its token and queued
origin. A matching failed set gate consumes its staged proposal origin.

Desktop and local mobile sessions share that state machine. Mobile supplies
its actual user settings source and excludes scheduler and AppAgent launches.
Its existing queue watcher reserves the owning ActiveTurn before a dedicated
dispatcher delivers `/goal` as a model-turn kickoff. Explicit nonhuman
authority is installed inside the spawned task, alongside native source
provenance. Host retirement permanently closes proposal admission, cancels
dialogs and revokes queued IDs, including markers already removed from the
queue. Resetting a retired owner cannot reopen that admission.

Direct and approved kickoffs keep their separate task-notification and
auto-continuation provenance. They remain visible in the transcript and do
not receive human-turn authority or reset human activity timers. Internal
queue IDs are claimed once under the actual host's turn/session ownership.

A Goal approval belongs to its session and can outlive the model turn that
proposed it. The broker, bridge server, TUI and Electron preserve it after
turn completion and allow an answer while idle. Ordinary turn questions still
expire at turn cleanup. The optional output marker `sessionOwned` informs
clients of that lifetime; only the broker's internal cancellation token grants
server authority. Hard session teardown retires the proposal. Native Android
and iOS question queues already retain questions across TurnEnded and clear
them at SessionEnded; generated native DTOs and their test constructors include
the added lifetime field.

## Validation results

| Check | Result |
| --- | --- |
| Both native helper replays | 217 cases passed; 36 distinct pinned source ranges |
| Goal-filtered Rust matrix: command-api, orchestrator, tool-ui, desktop runtime, msgqueue | 140 passed |
| Shared proposal host and teardown after mobile extraction | 12 passed, including 11 repeated checks from the matrix |
| Local mobile Goal host, production dispatcher and real broker | 5 passed |
| Queued-turn task-local authority regression | 1 passed |
| Client broker and protocol, with `uniffi,adapter` | 19 passed |
| Bridge server Goal lifetime and queued authority | 3 passed |
| TUI Goal command and session-question cleanup | 2 passed |
| Electron Goal lifetime, bridge and renderer bridge tests | 124 passed |
| Bridge-client build; Electron main and renderer TypeScript checks | Passed |
| CLI compile against the local runtime source | Passed |
| Generated Kotlin file compile and false/true request codec round trips | Passed |
| Generated Swift file typecheck and affected iOS test parse | Passed |

The native binding check uses fresh UniFFI metadata, preserves unrelated
generated sections, and confirms that the three API checksums and C header are
unchanged. The client snapshot suite reports eight passes and one unrelated
failure: the concurrent Decision commands `decision_configuration_changed` and
`decision_response` have no goldens yet. Ordinary AskUserQuestion event goldens
pass. No unrelated snapshot was blessed.

## Validation boundaries

The focused Rust validation uses an isolated snapshot of the dirty workspaces,
leaving canonical dependency locks and unrelated changes in place. The pinned
llm-client snapshot has the existing local request-policy field required by
parallel runtime work. One unrelated Memory test module is excluded only in
the validation snapshot: its five references to removed prompt methods do not
compile during the concurrent Memory refactor. These adjustments are recorded
in the task's validation manifest. Results are focused Goal checks, not a full
workspace test pass. Two additional unrelated Memory tests with an obsolete
prompt method or service signature are also excluded in that snapshot; their
exact names and reasons are retained in the validation manifest.

The final snapshot refresh includes the concurrent Memory and filesystem
source dependencies referenced by the shared host/context files. The overlay
manifest records that coherent source set; it does not modify those canonical
files or establish acceptance of the unrelated work.

Final shared-host and mobile checks use a separate task-owned Cargo target.
Its cache was seeded with an APFS copy-on-write clone and local Rust sources
were refreshed before compilation. Other sessions' shared target and source
files are preserved.

The initial product Rust checks used recorded local path patches for 49
runtime packages so they exercised these Goal sources together. Publication
advances the canonical product Git dependency pins and separately verifies
those immutable identities; the delivery receipt records that integration.

The native organization-hold trait is available, but LingXi has no equivalent
organization-consent flow to supply a nondefault hold. Provider-owned quota,
daily-cap and OAuth-disk-full facts have classification coverage; no new
provider source was invented for facts the current runtime does not expose.
Those integrations are outside the end-to-end evidence.

The retained task report records test commands, counts and source hashes.
Terminal pixel identity, a signed desktop package, live model-service behavior
and physical Android/iOS acceptance are outside this source-validation run.
